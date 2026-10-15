//! MiniMax 余额查询：`GET /minimax-cloud/api/v1/credit/details`。
//!
//! ── 口径（实测 + DSH minimax-credits.js 注记）─────────────────
//! 响应 `{ data?, details: [ { remaining_amount, … }, … ], base_resp }`：
//! 余额 = **Σ details[].remaining_amount**。要点：
//!   - `points` 是总数，**不得**再加 `bonus_points`（重复计数的坑）；
//!   - HTTP 200 也可能是业务错误（判 `base_resp.status_code == 0`）；
//!   - 令牌临期时先刷一次再查（刷新链在 `oauth.rs`）。
//!
//! 归一化成账号页的统一形状 `{available, unit, wallets, raw}`（见
//! `ProviderAdapter::query_usage` 的契约）：每条明细一个钱包。
//!
//! ── 硬约束 ──────────────────────────────────────────────────
//! release 是 `panic=abort`：本文件零 unwrap/expect/panic。

use serde_json::{json, Value};

use crate::server::core::account_store::AccountStore;
use crate::server::errors::GatewayError;

use super::client;
use super::{endpoints, oauth};

/// 从对象里取一个非负数字（缺失 / 非法给 0；兼容数字字符串形态）
fn number_of(container: &Value, key: &str) -> f64 {
    container
        .get(key)
        .and_then(|value| {
            value
                .as_f64()
                .or_else(|| value.as_str().and_then(|text| text.trim().parse::<f64>().ok()))
        })
        .unwrap_or(0.0)
}

/// 查询某 MiniMax 账号的积分余额。
pub async fn query_usage(store: &AccountStore, account_id: &str) -> Result<Value, GatewayError> {
    let record = oauth::read_record(store, account_id)?;
    // 点名账号用点名的 id；空串时 read_record 已选出队首，用它的 id 续期，
    // 保证「查余额」与「刷令牌」落在同一条记录上。
    let resolved_id = record.get("id").and_then(Value::as_str).unwrap_or(account_id);
    // 临期先刷（一次性轮换的 rt 也在这里被回写）
    let credentials = oauth::ensure_fresh(store, resolved_id, false).await?;
    let proxy = oauth::account_proxy(&record)?;
    let payload = client::bearer_request(
        "GET",
        &endpoints::credit_url(),
        &credentials.access_token,
        None,
        proxy.as_ref(),
        "积分余额查询",
    )
    .await?;
    client::ensure_business_ok(&payload, "积分余额查询")?;

    // 明细数组可能在顶层 details 或 data.details 两种形态里
    let details = payload
        .get("details")
        .and_then(Value::as_array)
        .or_else(|| payload.pointer("/data/details").and_then(Value::as_array));
    let mut total = 0.0;
    let mut wallets: Vec<Value> = Vec::new();
    if let Some(items) = details {
        for item in items {
            let remaining = number_of(item, "remaining_amount");
            total += remaining;
            wallets.push(json!({
                "type": item.get("credit_type").and_then(Value::as_str).unwrap_or("credit"),
                "displayName": item.get("name").and_then(Value::as_str).unwrap_or("积分"),
                "balance": remaining,
            }));
        }
    }
    Ok(json!({
        "available": total,
        "unit": "积分",
        "wallets": wallets,
        "raw": payload,
    }))
}
