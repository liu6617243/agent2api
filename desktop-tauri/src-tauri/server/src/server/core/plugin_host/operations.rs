//! 插件下载任务在服务端执行，进度按包 id 隔离；下载后自动验签、安装并热加载。

use std::collections::BTreeMap;
use std::sync::{Mutex, OnceLock};

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio::sync::{Mutex as AsyncMutex, MutexGuard};

use super::{catalog, packages, update};
use crate::server::core::account_store::plugin_accounts;
use crate::server::core::task_state::{self, Claim, ManualBackoff};
use crate::server::logging;

#[derive(Clone, Default, Deserialize, Serialize)]
#[serde(default, rename_all = "camelCase")]
pub struct Operation {
    pub id: String,
    pub action: String,
    pub stage: String,
    pub active: bool,
    pub done: bool,
    pub received: u64,
    pub total: u64,
    pub percent: Option<f64>,
    pub version: String,
    pub error: Option<String>,
    pub host_error: Option<String>,
    pub updated_at: i64,
    pub result: Option<Value>,
}

fn tasks() -> &'static Mutex<BTreeMap<String, Operation>> {
    static TASKS: OnceLock<Mutex<BTreeMap<String, Operation>>> = OnceLock::new();
    TASKS.get_or_init(|| Mutex::new(BTreeMap::new()))
}

fn mutation_lock() -> &'static AsyncMutex<()> {
    static LOCK: OnceLock<AsyncMutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| AsyncMutex::new(()))
}

pub async fn mutation_guard() -> MutexGuard<'static, ()> {
    mutation_lock().lock().await
}

fn record_key(id: &str) -> String {
    format!("pluginOperation:{id}")
}

fn mutate(id: &str, update: impl FnOnce(&mut Operation)) {
    if let Ok(mut tasks) = tasks().lock() {
        if let Some(task) = tasks.get_mut(id) {
            update(task);
            task.updated_at = logging::now_ms();
        }
    }
}

fn persist(id: &str) {
    let operation = tasks().lock().ok().and_then(|tasks| tasks.get(id).cloned());
    if let Some(operation) = operation {
        if let Ok(value) = serde_json::to_value(operation) {
            let _ = task_state::store_value(&record_key(id), value);
        }
    }
}

pub fn active(id: &str) -> bool {
    tasks()
        .lock()
        .ok()
        .and_then(|tasks| tasks.get(id).map(|task| task.active))
        .unwrap_or(false)
}

pub fn snapshot() -> Value {
    let mut operations = tasks()
        .lock()
        .map(|tasks| tasks.clone())
        .unwrap_or_default();
    if let Ok(states) = task_state::read_all() {
        for (key, state) in states {
            let Some(id) = key.strip_prefix("pluginOperation:") else {
                continue;
            };
            if operations.contains_key(id) {
                continue;
            }
            let Some(value) = state.value else {
                continue;
            };
            let Ok(mut operation) = serde_json::from_value::<Operation>(value) else {
                continue;
            };
            if operation.active {
                operation.active = false;
                operation.stage = "failed".to_string();
                operation.error = Some("上次插件安装中断，请重试".to_string());
            }
            operation.result = None;
            operations.insert(id.to_string(), operation);
        }
    }
    json!({ "operations": operations.into_values().collect::<Vec<_>>() })
}

pub async fn install_prepared(prepared: packages::PreparedPack) -> Result<Value, String> {
    let _guard = mutation_guard().await;
    let store = plugin_accounts::shared();
    super::shutdown_all().await;
    super::clear_providers();
    let outcome = tokio::task::spawn_blocking(move || prepared.install())
        .await
        .map_err(|error| format!("插件安装任务中断：{error}"))
        .and_then(|result| result);
    if let Some(store) = store {
        super::reload_host(store).await;
    }
    let installed = outcome?;
    let pack = super::list_packs().into_iter().find(|pack| pack.id == installed.id)
        .unwrap_or(installed);
    let reloaded = super::host_for_pack(&pack.id).is_some_and(|host| host.status().running);
    let mut result = super::management_snapshot();
    result["pack"] = serde_json::to_value(&pack).map_err(|error| error.to_string())?;
    result["hostError"] = if pack.enabled && !reloaded {
        json!("插件已安装，但本次没能启动；请检查日志或重启应用")
    } else {
        Value::Null
    };
    catalog::wake();
    Ok(result)
}

pub fn start_cached(
    id: &str,
    action: &str,
    target: &str,
    version: &str,
    platform: &Value,
    automatic: bool,
) -> Result<Operation, String> {
    if target != super::host_target() {
        return Err("只能安装当前网关平台的插件包".to_string());
    }
    let asset = update::PackAsset {
        version: version.to_string(),
        name: update::asset_name(id, version, target),
        url: platform["url"].as_str().unwrap_or("").to_string(),
        size: platform["size"].as_u64().unwrap_or(0),
        page: String::new(),
        require_signed: automatic || platform["requireSigned"] == true,
    };
    crate::server::core::update::assert_downloadable(&asset.url).map_err(|error| error.message)?;
    let guard = match task_state::claim(
        &record_key(id),
        60_000,
        true,
        if automatic {
            ManualBackoff::Respect
        } else {
            ManualBackoff::Bypass
        },
        1000,
    )? {
        Claim::Acquired(guard) => guard,
        Claim::Deferred(state) => return Err(state.waiting_message()),
    };
    let operation = Operation {
        id: id.to_string(),
        action: action.to_string(),
        stage: "downloading".to_string(),
        active: true,
        version: version.to_string(),
        updated_at: logging::now_ms(),
        ..Default::default()
    };
    tasks()
        .lock()
        .map_err(|_| "插件任务状态不可用".to_string())?
        .insert(id.to_string(), operation.clone());
    persist(id);
    let id = id.to_string();
    let channels = super::settings::current().channels();
    crate::spawn_task(async move {
        let result = run(&id, &asset, automatic, channels).await;
        let mut finish_value = Value::Null;
        let success = result.is_ok();
        mutate(&id, |operation| {
            operation.active = false;
            match result {
                Ok(result) => {
                    operation.stage = "done".to_string();
                    operation.done = true;
                    operation.percent = Some(100.0);
                    operation.host_error = result["hostError"].as_str().map(str::to_string);
                    operation.result = None;
                }
                Err(error) => {
                    operation.stage = "failed".to_string();
                    operation.error = Some(error);
                }
            }
            finish_value = serde_json::to_value(&*operation).unwrap_or(Value::Null);
        });
        let message = finish_value["error"]
            .as_str()
            .unwrap_or("插件安装完成")
            .to_string();
        if !success {
            logging::log("[Plugin]", &message);
        }
        let _ = guard.finish(success, message, Some(finish_value), 0, 60_000);
    });
    Ok(operation)
}

async fn run(
    id: &str,
    asset: &update::PackAsset,
    automatic: bool,
    channels: crate::server::config::UpdateChannels,
) -> Result<Value, String> {
    let bytes = update::download(asset, |received, total| {
        mutate(id, |task| {
            task.received = received;
            task.total = total;
            task.percent = if total > 0 {
                Some((received as f64 / total as f64 * 100.0).min(100.0))
            } else {
                None
            };
        });
    })
    .await?;
    mutate(id, |task| {
        task.stage = "verifying".to_string();
        task.percent = None;
    });
    persist(id);
    let expected_id = id.to_string();
    let version = asset.version.clone();
    let require_signed = automatic || asset.require_signed;
    let prepared = tokio::task::spawn_blocking(move || {
        packages::prepare(&bytes, Some(&expected_id), Some(&version), require_signed)
    })
    .await
    .map_err(|error| format!("插件校验任务中断：{error}"))??;
    if automatic {
        let settings = super::settings::current();
        let installed = super::list_packs().into_iter().find(|pack| pack.id == id);
        let allowed = match installed {
            Some(pack) => settings.auto_update && pack.enabled && pack.signed
                && crate::server::core::update::compare_versions(&asset.version, &pack.version) == Some(1),
            None => settings.auto_install_official,
        };
        if !allowed || !settings.auto_check || settings.channels() != channels {
            return Err("自动安装已关闭或通道已改变，本次下载未安装".to_string());
        }
    }
    mutate(id, |task| {
        task.stage = "installing".to_string();
        task.percent = None;
    });
    persist(id);
    install_prepared(prepared).await
}

pub async fn start(id: &str, action: &str, target: &str) -> Result<Operation, String> {
    let packs = super::list_packs();
    let pack = packs.iter().find(|pack| pack.id == id);
    if action == "update" && pack.is_none() {
        return Err("还没有安装这个插件包".to_string());
    }
    let cached = catalog::snapshot();
    let catalog = if cached["checked"] == true && cached["stale"] != true {
        cached
    } else {
        catalog::refresh(true).await?
    };
    let entry = catalog["plugins"]
        .as_array()
        .and_then(|items| items.iter().find(|item| item["id"] == id))
        .ok_or_else(|| format!("插件索引里没有这个包：{id}"))?;
    if entry["error"] == true {
        return Err(entry["reason"]
            .as_str()
            .unwrap_or("插件更新检查失败")
            .to_string());
    }
    let version = entry["version"]
        .as_str()
        .ok_or_else(|| "当前通道暂无可安装的版本".to_string())?;
    if action == "update"
        && pack.is_some_and(|pack| {
            crate::server::core::update::compare_versions(version, &pack.version) != Some(1)
        })
    {
        return Err("已经是最新版本".to_string());
    }
    let platform = entry["platforms"]
        .as_array()
        .and_then(|items| items.iter().find(|item| item["target"] == target))
        .ok_or_else(|| "当前平台暂无安装包".to_string())?;
    start_cached(id, action, target, version, platform, false)
}
