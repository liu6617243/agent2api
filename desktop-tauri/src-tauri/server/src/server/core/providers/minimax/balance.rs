//! MiniMax 余额查询：`GET /minimax-cloud/api/v1/credit/details`。
//!
//! ── 口径（实测 + DSH minimax-credits.js 注记）─────────────────
//! 响应 `{ details: [ { remaining_amount, … }, … ], base_resp }`：
//! 余额 = **Σ details[].remaining_amount**。要点：
//!   - `points` 是总数，**不得**再加 `bonus_points`（重复计数的坑）；
//!   - HTTP 200 也可能是业务错误（判 `base_resp.status_code == 0`）；
//!   - 令牌临期时先刷一次再查（刷新链在 `oauth.rs`）。
//!
//! ── 积分是批次制：每笔签到积分独立计 30 天有效期 ──────────────────
//! **实测（`GET /credit/details`）**：`details[]` 每一条就是一笔独立批次，
//! 带两个关键时间戳：
//!
//! ```text
//!   granted_at_ms   领取时刻
//!   expire_at_ms    到期时刻  ← 就是「最先过期积分」的答案
//!   granted_amount / consumed_amount / remaining_amount   这一批的额度与消耗
//!   credit_type     2 = 签到积分
//! ```
//!
//! 实测一批：`granted_at=2026-10-14 06:13` → `expire_at=2026-11-13 00:00`，
//! 即**按自然日对齐**（到期落在当天 00:00，不是领取时刻 + 30×24h，所以实差
//! 约 29.1~29.7 天）。多笔各算各的：新签到**不延长**旧批次（官方规则：
//! 「每一笔签到积分，从领取当天开始算，有效期 30 天，到期自动作废，
//! 各笔独立计时」）。
//!
//! 因此 `nearest_expiring` 取的是**所有 `remaining_amount > 0` 的批次里
//! `expire_at_ms` 最小的那一个** —— 那是真正会最先作废的钱（也就是账号列表
//! 「有效期」列要显示为「最先过期积分的剩余时间」的那个值，不是 access token
//! 那约 1 小时的倒计时）。已过期（`expire_at_ms <= now`）的批次不参与取最小值：
//! 它们的余额按上游口径已被清零，不该再提示用户「即将过期」。
//!
//! 归一化成账号页的统一形状 `{available, unit, wallets, credits, raw}`（见
//! `ProviderAdapter::query_usage` 的契约）：每条明细一个钱包，另出 `credits[]`
//! 供余额格的「积分明细」弹层逐批展开（`creditDetail` 能力位）。
//!
//! ── 硬约束 ──────────────────────────────────────────────────
//! release 是 `panic=abort`：本文件零 unwrap/expect/panic。

use serde_json::{json, Value};

use crate::server::core::account_store::AccountStore;
use crate::server::errors::GatewayError;
use crate::server::logging;

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

/// 取毫秒时间戳字段（兼容数字与数字字符串两种形态）
fn millis_of(container: &Value, key: &str) -> Option<i64> {
    match container.get(key) {
        Some(Value::Number(number)) => number.as_i64(),
        Some(Value::String(text)) => text.trim().parse::<i64>().ok(),
        _ => None,
    }
}

/// `credit_type` 的展示名。上游实测是**数字**（`2` = 签到积分），不是字符串 ——
/// 用 `as_str()` 取会永远落空并回落到通用名。
fn credit_type_label(item: &Value) -> String {
    match item.get("credit_type") {
        Some(Value::Number(number)) => match number.as_i64() {
            Some(2) => "签到积分".to_string(),
            Some(other) => format!("积分类型 {other}"),
            None => "积分".to_string(),
        },
        Some(Value::String(text)) => match text.trim() {
            "2" => "签到积分".to_string(),
            "" => "积分".to_string(),
            other => other.to_string(),
        },
        _ => "积分".to_string(),
    }
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
    let now = logging::now_ms();
    if let Some(items) = details {
        for item in items {
            let remaining = number_of(item, "remaining_amount");
            total += remaining;
            // 批次信息：把这笔积分的领取 / 到期时刻带出来，界面才能说清
            // 「哪一笔会在什么时候作废」（`credits[].grantAt / expireAt` 同源）。
            let mut wallet = json!({
                "type": credit_type_label(item),
                "displayName": item.get("name").and_then(Value::as_str).unwrap_or("积分"),
                "balance": remaining,
                "grantedAmount": number_of(item, "granted_amount"),
                "consumedAmount": number_of(item, "consumed_amount"),
            });
            if let Some(granted) = millis_of(item, "granted_at_ms") {
                wallet["grantedAt"] = Value::from(granted);
            }
            if let Some(expire) = millis_of(item, "expire_at_ms") {
                wallet["expiresAt"] = Value::from(expire);
            }
            wallets.push(wallet);
        }
        // 最近会作废的那批（选取规则见 `nearest_expiring`）。
        // ① 回写账号记录的 `creditExpiresAt` —— 账号列表「有效期」列显示的
        //    「最先过期积分的剩余时间」就是从这儿来的（`set_minimax_credit_expiry`
        //    的文档说明了为什么不能写 `expiresAt`：那是刷新链的临期判据，
        //    被覆盖会让网关不再续期）。值没变时 setter 内部跳过写库
        //    （sweep 每分钟一轮，不能白挨写）。
        // ② `credits[]` 给余额格的「积分明细」弹层（`creditDetail` 能力位 +
        //    `accounts-credit-detail.tsx` 的 `creditRowsOf` 契约：面额 / 已用 /
        //    剩余 + 领取 → 到期）。**按到期升序**（最近作废在最前，与 Trae 的
        //    排序口径一致；前端不再重排，见 creditRowsOf 的注释）。
        if let Some(batch) = nearest_expiring(items, now) {
            let _ = store.set_minimax_credit_expiry(&resolved_id, batch.expire_at);
            let mut credits: Vec<Value> = items
                .iter()
                .map(|item| {
                    let mut pack = json!({
                        "name": credit_type_label(item),
                        "unit": "积分",
                        "total": number_of(item, "granted_amount"),
                        "used": number_of(item, "consumed_amount"),
                        "remain": number_of(item, "remaining_amount"),
                    });
                    if let Some(granted) = millis_of(item, "granted_at_ms") {
                        pack["grantAt"] = Value::from(granted);
                    }
                    if let Some(expire) = millis_of(item, "expire_at_ms") {
                        pack["expireAt"] = Value::from(expire);
                    }
                    pack
                })
                .collect();
            credits.sort_by_key(|pack| pack.get("expireAt").and_then(Value::as_i64).unwrap_or(i64::MAX));
            return Ok(json!({
                "available": total,
                "unit": "积分",
                "wallets": wallets,
                "credits": credits,
                "nearestExpiryMs": batch.expire_at,
                "nearestExpiryBalance": batch.remaining,
                "raw": payload,
            }));
        }
        // 没有会作废的批次：清掉记录里的旧值（传 0 → 有效期列回落令牌到期），
        // 并照常返回读数（不给 credits 键，弹层也就不会有明细行）。
        let _ = store.set_minimax_credit_expiry(&resolved_id, 0);
    }
    Ok(json!({
        "available": total,
        "unit": "积分",
        "wallets": wallets,
        "raw": payload,
    }))
}

/// 一笔积分批次的到期信息（`nearest_expiring` 的产物）。
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct BatchExpiry {
    /// 到期时刻（毫秒）
    pub expire_at: i64,
    /// 该笔剩余额度
    pub remaining: f64,
}

/// 从 `details[]` 里挑出「最近会作废的那笔余额」。
///
/// 规则（见模块头，实测依据官方口径）：
///   - 只看 `remaining_amount > 0` 的批次 —— 用完的不提示；
///   - 已过期（`expire_at <= now`）的不参与 —— 上游已清零，再提示是假消息；
///   - 取 `expire_at` **最小**的那笔（最先作废 = 真正会丢的钱）；
///   - 全部都不满足时返回 None，让界面回落到「无到期批次」。
fn nearest_expiring(details: &[Value], now: i64) -> Option<BatchExpiry> {
    details
        .iter()
        .filter_map(|item| {
            let remaining = number_of(item, "remaining_amount");
            if remaining <= 0.0 {
                return None;
            }
            let expire = millis_of(item, "expire_at_ms")?;
            if expire <= now {
                return None;
            }
            Some(BatchExpiry { expire_at: expire, remaining })
        })
        .min_by_key(|batch| batch.expire_at)
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::{credit_type_label, nearest_expiring, BatchExpiry};

    /// 上游实测的形状：credit_type 是**数字** 2，
    /// 时间戳在 granted_at_ms / expire_at_ms。
    fn batch(expire: i64, remaining: &str) -> serde_json::Value {
        json!({
            "credit_type": 2,
            "granted_amount": remaining,
            "remaining_amount": remaining,
            "consumed_amount": "0.00",
            "granted_at_ms": 1_791_929_630_448i64,
            "expire_at_ms": expire,
        })
    }

    #[test]
    fn picks_the_soonest_expiring_batch() {
        let now = 1_789_000_000_000i64;
        let details = vec![batch(now + 30 * 86_400_000, "1000"), batch(now + 5 * 86_400_000, "794.27")];
        assert_eq!(
            nearest_expiring(&details, now),
            Some(BatchExpiry { expire_at: now + 5 * 86_400_000, remaining: 794.27 })
        );
    }

    /// 各笔独立计时：新签到不会延长旧批次，所以「最近过期」永远是**最早**
    /// 那一笔，而不是最后签的那笔 —— 这是最容易写反的地方。
    #[test]
    fn new_batch_does_not_extend_the_old_one() {
        let now = 1_789_000_000_000i64;
        let details = vec![
            batch(now + 1 * 86_400_000, "400"),    // 昨天签的，最早到期
            batch(now + 29 * 86_400_000, "1000"), // 今天签的，最晚到期
        ];
        assert_eq!(nearest_expiring(&details, now).map(|b| b.remaining), Some(400.0));
    }

    #[test]
    fn ignores_spent_and_expired_batches() {
        let now = 1_789_000_000_000i64;
        let details = vec![
            batch(now - 86_400_000, "500"), // 已过期
            batch(now + 86_400_000, "0"),   // 已用完
            batch(now + 9 * 86_400_000, "800"),
        ];
        assert_eq!(nearest_expiring(&details, now).map(|b| b.remaining), Some(800.0));
    }

    #[test]
    fn none_when_no_batch_has_balance() {
        let now = 1_789_000_000_000i64;
        assert_eq!(nearest_expiring(&[], now), None);
        assert_eq!(nearest_expiring(&[batch(now + 86_400_000, "0")], now), None);
    }

    /// credit_type 是数字，取字符串会永远落空 —— 这条锁住那个坑
    #[test]
    fn credit_type_is_numeric_upstream() {
        assert_eq!(credit_type_label(&json!({ "credit_type": 2 })), "签到积分");
        assert_eq!(credit_type_label(&json!({ "credit_type": "2" })), "签到积分");
        assert_eq!(credit_type_label(&json!({})), "积分");
    }
}