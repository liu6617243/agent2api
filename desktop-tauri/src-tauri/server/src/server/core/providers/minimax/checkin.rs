//! MiniMax 每日签到：`POST /minimax-cloud/api/v1/signin/claim`。
//!
//! ── 链路（实测 + DSH minimax-credits.js 复刻）──────────────────
//! ```text
//! ① GET  /minimax-cloud/api/v1/signin/status?timezone_id=Asia/Shanghai
//!      → data.days[]：{ day_no, status, points, is_today }
//!        status: 1=未解锁 2=可领 3=已领
//! ② POST /minimax-cloud/api/v1/signin/claim?timezone_id=Asia/Shanghai  body {}
//!      → data.claim_result: 1=本次领取 2=今日已领过
//! ```
//! 判「今日已完成」的口径（对齐 `billing::checkin::checkin_completed_today`）：
//!   - `claim_result == 1` → `{success: true, msg}`（本次领到）；
//!   - `claim_result == 2` → `{success: false, alreadyCompleted: true, msg:"今日已签到"}`；
//!   - status 面板今天 `status == 3` 且没有任何可领项 → 同上 alreadyCompleted；
//!   - 其余（无可领 / 业务错误）→ success:false（不落当日台账，定时链稍后重试）。
//! claim 接口本身幂等（重复领回 claim_result=2，不报错不重复加分）。
//!
//! ── 硬约束 ──────────────────────────────────────────────────
//! release 是 `panic=abort`：本文件零 unwrap/expect/panic。

use serde_json::{json, Value};

use crate::server::core::account_store::AccountStore;
use crate::server::errors::GatewayError;
use crate::server::logging;

use super::client;
use super::{endpoints, oauth};

/// MiniMax 账号的每日签到领取（自动签到框架的 claim）。
///
/// 返回 `{success, msg, alreadyCompleted?}` —— 与 WorkBuddy / 小浣熊 / Loomy 的
/// claim 同一形状（`billing::checkin` 的汇总只认这几个字段）。
pub async fn claim_daily_checkin(
    store: &AccountStore,
    account_id: &str,
) -> Result<Value, GatewayError> {
    let record = oauth::read_record(store, account_id)?;
    let resolved_id = record.get("id").and_then(Value::as_str).unwrap_or(account_id);
    let credentials = oauth::ensure_fresh(store, resolved_id, false).await?;
    let proxy = oauth::account_proxy(&record)?;
    let token = credentials.access_token.as_str();

    // ① 面板状态：先看今天/有没有可领项，纯为文案与「已领过」判定服务
    let status_payload = client::bearer_request(
        "GET",
        &endpoints::signin_status_url(),
        token,
        None,
        proxy.as_ref(),
        "签到状态查询",
    )
    .await?;
    client::ensure_business_ok(&status_payload, "签到状态查询")?;
    let data = status_payload
        .get("data")
        .filter(|value| value.is_object())
        .unwrap_or(&status_payload);
    let days = data.get("days").and_then(Value::as_array).cloned().unwrap_or_default();
    let today = days.iter().find(|day| day.get("is_today").and_then(Value::as_bool).unwrap_or(false));
    let has_claimable = days.iter().any(|day| day.get("status").and_then(Value::as_i64) == Some(2));
    let today_claimed = today.and_then(|day| day.get("status").and_then(Value::as_i64)) == Some(3);

    // 无可领项：今天已领 → alreadyCompleted；否则中性未领取（断签/未开始）
    if !has_claimable {
        if today_claimed {
            return Ok(json!({
                "success": false,
                "alreadyCompleted": true,
                "msg": "今日已签到",
            }));
        }
        return Ok(json!({
            "success": false,
            "msg": "无可领项（可能活动断签或未开始）",
        }));
    }

    // ② 领取（幂等；body 为空对象）
    let claim_payload = client::bearer_request(
        "POST",
        &endpoints::signin_claim_url(),
        token,
        Some(&json!({})),
        proxy.as_ref(),
        "每日签到领取",
    )
    .await?;
    client::ensure_business_ok(&claim_payload, "每日签到领取")?;
    let claim_data = claim_payload
        .get("data")
        .filter(|value| value.is_object())
        .unwrap_or(&claim_payload);
    match claim_data.get("claim_result").and_then(Value::as_i64) {
        Some(1) => {
            let points = claim_data
                .get("points")
                .and_then(Value::as_i64)
                .or_else(|| claim_data.get("points").and_then(Value::as_f64).map(|value| value as i64));
            let msg = match points {
                Some(points) => format!("签到成功（MiniMax 官方 +{points} 积分）"),
                None => "签到成功".to_string(),
            };
            logging::log("[Checkin]", &format!("✅ MiniMax {msg}"));
            Ok(json!({ "success": true, "msg": msg }))
        }
        Some(2) => Ok(json!({
            "success": false,
            "alreadyCompleted": true,
            "msg": "今日已签到",
        })),
        _ => {
            let raw = claim_data.to_string();
            let head: String = raw.chars().take(160).collect();
            Ok(json!({
                "success": false,
                "msg": format!("签到结果未知：{head}"),
            }))
        }
    }
}
