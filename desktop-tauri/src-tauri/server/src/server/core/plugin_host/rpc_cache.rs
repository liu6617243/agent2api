//! 插件模型目录的同步快照；成功刷新持久化，装配时恢复后再使用握手兜底。

use std::collections::{HashMap, HashSet};
use std::sync::{Mutex, OnceLock};

use agent2api_provider_protocol as proto;
use serde_json::Value;

use crate::server::core::providers::catalog_cache;
use crate::server::logging;

pub const SETTING_MODEL_CATALOG: &str = "modelCatalog";

#[derive(Clone, Default)]
struct Entry {
    remote: Option<proto::ModelList>,
    fallback: Vec<Value>,
}

fn table() -> &'static Mutex<HashMap<String, Entry>> {
    static TABLE: OnceLock<Mutex<HashMap<String, Entry>>> = OnceLock::new();
    TABLE.get_or_init(|| Mutex::new(HashMap::new()))
}

fn scope(provider: &str) -> String {
    format!("plugin:{provider}")
}

fn from_cached(cached: catalog_cache::CachedCatalog) -> Option<proto::ModelList> {
    let models = valid_models(cached.models);
    (!models.is_empty()).then_some(proto::ModelList {
        models,
        meta: proto::CatalogMeta { remote: true, refreshed_at: cached.fetched_at },
    })
}

fn restore(provider: &str) -> Option<proto::ModelList> {
    let key = scope(provider);
    if let Some(list) = catalog_cache::load(&key).and_then(from_cached) {
        return Some(list);
    }
    let list = catalog_cache::load_legacy(provider).and_then(from_cached)?;
    // 写事务里再次检查新槽，避免与在线刷新竞争时覆盖它；旧槽保留供回退。
    catalog_cache::save_migrated(&key, catalog_cache::CachedCatalog {
        models: list.models,
        fetched_at: list.meta.refreshed_at,
    }).and_then(from_cached)
}

fn valid_models(models: Vec<Value>) -> Vec<Value> {
    let mut claimed = HashSet::new();
    models.into_iter().filter(|model| {
        model.get("id").and_then(Value::as_str)
            .is_some_and(|id| !id.trim().is_empty() && claimed.insert(id.to_ascii_lowercase()))
    }).collect()
}

pub fn set_defaults(provider: &str, models: Vec<Value>) -> Option<(usize, i64)> {
    let restored = restore(provider);
    let mut map = table().lock().ok()?;
    let entry = map.entry(provider.to_string()).or_default();
    let recovered = if entry.remote.is_none() {
        let recovered = restored.as_ref().map(|list| (list.models.len(), list.meta.refreshed_at));
        entry.remote = restored;
        recovered
    } else {
        None
    };
    entry.fallback = valid_models(models);
    recovered
}

pub fn set_remote(provider: &str, mut list: proto::ModelList) -> usize {
    list.models = valid_models(list.models);
    if list.models.is_empty() {
        return 0;
    }
    list.meta.remote = true;
    if list.meta.refreshed_at <= 0 {
        list.meta.refreshed_at = logging::now_ms();
    }
    let count = list.models.len();
    catalog_cache::save(&scope(provider), &list.models, list.meta.refreshed_at);
    let Ok(mut map) = table().lock() else { return 0 };
    map.entry(provider.to_string()).or_default().remote = Some(list);
    count
}

pub fn snapshot(provider: &str) -> Option<proto::ModelList> {
    let map = table().lock().ok()?;
    let entry = map.get(provider)?;
    entry.remote.clone().or_else(|| {
        (!entry.fallback.is_empty()).then(|| proto::ModelList {
            models: entry.fallback.clone(),
            meta: proto::CatalogMeta::default(),
        })
    })
}

pub fn models_for(provider: &str) -> Option<Vec<Value>> {
    snapshot(provider).map(|list| list.models)
}

pub fn meta(provider: &str) -> proto::CatalogMeta {
    let Ok(map) = table().lock() else { return proto::CatalogMeta::default() };
    map.get(provider).and_then(|entry| entry.remote.as_ref())
        .map(|list| list.meta.clone()).unwrap_or_default()
}

pub fn forget(provider: &str) {
    if let Ok(mut map) = table().lock() {
        map.remove(provider);
    }
}

pub fn clear_all() {
    if let Ok(mut map) = table().lock() {
        map.clear();
    }
}
