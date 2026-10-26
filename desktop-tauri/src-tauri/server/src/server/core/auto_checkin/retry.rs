use serde_json::{Map, Value};

use crate::server::core::{account_store::AccountStore, beijing, billing::checkin};

const RETRY_GAP_MS: i64 = 10 * 60 * 1000;

pub(super) fn due_accounts(previous: Option<&Value>, now: i64) -> Option<Vec<String>> {
    let previous = previous.filter(|value| {
        value.get("date").and_then(Value::as_str) == Some(beijing::today_key().as_str())
    })?;
    let retry = previous.get("retryAccounts").and_then(Value::as_object)?;
    if retry.is_empty() {
        return None;
    }
    Some(
        retry
            .iter()
            .filter(|(_, at)| at.as_i64().is_some_and(|at| at <= now))
            .map(|(id, _)| id.clone())
            .collect(),
    )
}

fn retry_at(store: &AccountStore, row: &Value, now: i64) -> Option<i64> {
    if row
        .get("error")
        .and_then(Value::as_str)
        .is_some_and(|text| !text.trim().is_empty())
        || checkin::day_completed(row)
    {
        return None;
    }
    let claim = row.get("claim")?;
    if !checkin::claim_pending(claim) {
        return None;
    }
    if let Some(at) = claim.get("retryAt") {
        return at
            .as_i64()
            .filter(|at| *at > 0)
            .map(|at| at.max(now + 30_000));
    }
    let id = row.get("id").and_then(Value::as_str)?;
    let provider = store.account_provider_of(id)?;
    let qoder = crate::server::core::providers::qoder::endpoints::Region::Global.provider_id();
    if provider == qoder {
        let window = beijing::now()
            .date_naive()
            .and_hms_opt(10, 0, 0)?
            .and_local_timezone(beijing::offset())
            .single()?
            .timestamp_millis();
        return Some(window.max(now + RETRY_GAP_MS));
    }
    (provider == "codearts").then_some(now + RETRY_GAP_MS)
}

pub(super) fn merge(
    store: &AccountStore,
    mut summary: Value,
    result: &Value,
    previous: Option<&Value>,
    retried: Option<&[String]>,
) -> Value {
    let Some(fields) = summary.as_object_mut() else {
        return summary;
    };
    let now = crate::server::logging::now_ms();
    let mut retry = Map::new();
    if let (Some(previous), Some(retried)) = (previous, retried) {
        retry = previous
            .get("retryAccounts")
            .and_then(Value::as_object)
            .cloned()
            .unwrap_or_default();
        for id in retried {
            retry.remove(id);
        }
        for key in ["succeeded", "active", "failedCount"] {
            let before = previous.get(key).and_then(Value::as_u64).unwrap_or(0);
            let current = fields.get(key).and_then(Value::as_u64).unwrap_or(0);
            fields.insert(key.to_string(), Value::from(before.saturating_add(current)));
        }
        for key in ["total", "skipped"] {
            if let Some(value) = previous.get(key) {
                fields.insert(key.to_string(), value.clone());
            }
        }
        let mut failures = previous
            .get("failed")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        failures.extend(
            fields
                .get("failed")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default(),
        );
        failures.truncate(5);
        fields.insert("failed".to_string(), Value::Array(failures));
    }
    if let Some(rows) = result.get("results").and_then(Value::as_array) {
        for row in rows {
            if let (Some(id), Some(at)) = (
                row.get("id").and_then(Value::as_str),
                retry_at(store, row, now),
            ) {
                retry.insert(id.to_string(), Value::from(at));
            }
        }
    }
    fields.insert("retryAccounts".to_string(), Value::Object(retry));
    summary
}

pub(super) fn next_at(result: Option<&Value>, today: &str) -> Option<i64> {
    let result = result.filter(|value| value.get("date").and_then(Value::as_str) == Some(today))?;
    result
        .get("retryAccounts")
        .and_then(Value::as_object)?
        .values()
        .filter_map(Value::as_i64)
        .min()
}
