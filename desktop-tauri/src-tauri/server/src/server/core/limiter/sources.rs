use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

use serde_json::{json, Value};

use super::types::{json_of, QuotaSource};

#[derive(Clone, Debug, Default)]
pub struct SourceFacts {
    pub sources: Vec<QuotaSource>,
    pub remaining: Option<f64>,
}

pub type Facts = HashMap<String, SourceFacts>;

fn slot() -> &'static Mutex<Facts> {
    static FACTS: OnceLock<Mutex<Facts>> = OnceLock::new();
    FACTS.get_or_init(|| Mutex::new(HashMap::new()))
}

pub fn facts() -> Facts { slot().lock().map(|facts| facts.clone()).unwrap_or_default() }

pub fn parse(usage: &Value) -> SourceFacts {
    let sources = usage.get("quotaSources").and_then(Value::as_array).map(|rows| {
        rows.iter().filter_map(|row| serde_json::from_value::<QuotaSource>(row.clone()).ok())
            .filter(|source| !source.id.is_empty() && source.id.len() <= 512).collect()
    }).unwrap_or_default();
    let (remaining, _) = crate::server::core::usage_records::extract_remaining(usage);
    SourceFacts { sources, remaining }
}

pub fn note(id: &str, usage: &Value) {
    if let Ok(mut facts) = slot().lock() { facts.insert(id.to_string(), parse(usage)); }
}

pub fn reload() {
    let fresh = crate::server::core::usage_records::load_all().into_iter()
        .filter_map(|(id, row)| row.usage.as_ref().map(|usage| (id, parse(usage)))).collect();
    if let Ok(mut facts) = slot().lock() { *facts = fresh; }
}

pub fn rows_for(account: &Value, facts: &Facts) -> Value {
    let id = account.get("id").and_then(Value::as_str).unwrap_or("");
    let provider = crate::server::core::routing::provider_of(account);
    let sources = facts.get(id).map(|facts| facts.sources.as_slice()).unwrap_or_default();
    let mut rows = json_of(&sources).as_array().cloned().unwrap_or_default();
    let has_balance = crate::server::core::usage_query::supports_usage_provider(provider);
    if has_balance && !sources.iter().any(|source| source.id == "account") {
        rows.insert(0, json!({ "id": "account", "label": "账号主额度", "provider": provider,
            "unit": "", "remaining": facts.get(id).and_then(|facts| facts.remaining) }));
    }
    rows.push(json!({ "id": "local", "label": "本地用量保护", "provider": provider,
        "localOnly": true, "localEstimate": true }));
    Value::Array(rows)
}

pub fn account_models(provider: &str) -> Vec<Value> {
    let models = if crate::server::core::custom_providers::is_custom_provider_id(provider) {
        crate::server::core::custom_providers::models_of(provider)
    } else {
        crate::server::core::providers::catalog::manifest_for_provider(provider)
    };
    models.iter().filter_map(|model| {
        let id = model.get("id").and_then(Value::as_str)?;
        Some(json!({ "id": id, "name": model.get("name").and_then(Value::as_str).unwrap_or(id) }))
    }).collect()
}
