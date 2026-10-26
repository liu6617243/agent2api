//! 插件更新策略独立保存；GitHub 凭证与出网代理沿用软件更新配置。

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::server::config;

const KEY: &str = "pluginUpdates";
pub const MIN_INTERVAL: i64 = 1;
pub const MAX_INTERVAL: i64 = 1440;

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(default, rename_all = "camelCase")]
pub struct PluginSettings {
    pub auto_check: bool,
    pub interval: i64,
    pub beta: bool,
    pub auto_update: bool,
    pub auto_install_official: bool,
}

impl Default for PluginSettings {
    fn default() -> Self {
        Self {
            auto_check: true,
            interval: 20,
            beta: false,
            auto_update: false,
            auto_install_official: false,
        }
    }
}

pub fn current() -> PluginSettings {
    let mut settings: PluginSettings = config::with_raw(|raw| {
        raw.get(KEY)
            .cloned()
            .and_then(|value| serde_json::from_value(value).ok())
    })
    .flatten()
    .unwrap_or_default();
    if !(MIN_INTERVAL..=MAX_INTERVAL).contains(&settings.interval) {
        settings.interval = PluginSettings::default().interval;
    }
    settings
}

pub fn configure(patch: &Value) -> Result<Value, String> {
    let object = patch
        .as_object()
        .ok_or_else(|| "插件设置必须是 JSON 对象".to_string())?;
    if object.is_empty() {
        return Err("没有需要更新的插件设置".to_string());
    }
    let before = current();
    let mut next = before;
    for (key, value) in object {
        if key == "interval" {
            let interval = value
                .as_i64()
                .filter(|value| (MIN_INTERVAL..=MAX_INTERVAL).contains(value))
                .ok_or_else(|| format!("检查间隔必须是 {MIN_INTERVAL} ~ {MAX_INTERVAL} 分钟"))?;
            next.interval = interval;
            continue;
        }
        let field = match key.as_str() {
            "autoCheck" => &mut next.auto_check,
            "beta" => &mut next.beta,
            "autoUpdate" => &mut next.auto_update,
            "autoInstallOfficial" => &mut next.auto_install_official,
            _ => return Err(format!("未知的插件设置：{key}")),
        };
        *field = value
            .as_bool()
            .ok_or_else(|| format!("{key} 必须为 true / false"))?;
    }
    let value = serde_json::to_value(next).map_err(|error| error.to_string())?;
    let saved = config::update_raw_field(KEY, value.clone());
    if next.interval != before.interval {
        super::catalog::reschedule(next.interval * 60_000);
    }
    if next.beta != before.beta {
        super::catalog::invalidate_channel();
    }
    if next != before {
        if next.beta != before.beta
            || (!before.auto_check && next.auto_check)
            || (!before.auto_update && next.auto_update)
            || (!before.auto_install_official && next.auto_install_official)
        {
            super::catalog::schedule_now();
        } else {
            super::catalog::wake();
        }
    }
    let mut result = value;
    result["saved"] = json!(saved);
    Ok(result)
}
