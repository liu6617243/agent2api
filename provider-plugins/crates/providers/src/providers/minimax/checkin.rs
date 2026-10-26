//! MiniMax 每日签到：`signin/status` → `signin/claim` → 结果判定。
//!
//! ── 协议（网关内核版实测定案）─────────────────────────────
//! ```text
//! GET  {api}/minimax-cloud/api/v1/signin/status?timezone_id=Asia/Shanghai
//!      → {base_resp:{status_code:0}, data:{days:[{is_today, status, ...}]}}
//!        status: 2 = 今天可领；3 = 今天已领
//! POST {api}/minimax-cloud/api/v1/signin/claim?timezone_id=Asia/Shanghai
//!      body {}（幂等）
//!      → {data:{claim_result: 1 成功 / 2 已签, points}}
//! ```
//! 鉴权一律 `Authorization: Bearer <access_token>`（登录态，不是 API Key）。
//!
//! ── 判定顺序（与网关内核版一致）───────────────────────────
//! 1. status 里没有可领项：今天已领 → `Done`；否则 → `Inactive`（断签/未开始）；
//! 2. 有可领项 → 打 claim；
//! 3. `claim_result == 1` → `Claimed`（带 points 文案）；`== 2` → `Done`；
//! 4. 其他值 / 业务码非 0 → `Failed`，把上游原文带出去（**不猜含义**）。
//!
//! ── 为什么不回 `Claimed` 就收工 ────────────────────────────
//! 上游的 `claim` 是幂等的，`claim_result` 才是真话；直接把 HTTP 200 当成功
//! 就是给用户一个假成功（界面亮绿、台账落库，实际什么也没到账）。
//!
//! ── 硬约束 ──────────────────────────────────────────────────
//! release 是 `panic=abort`：本文件零 unwrap/expect/panic。

use serde_json::{json, Value};

use agent2api_provider_protocol as proto;
use agent2api_provider_sdk::PluginCtx;

use crate::errors::GatewayError;

use super::endpoints;
use super::oauth;

/// 单次请求超时（签到这条族没有流式）
const REQUEST_TIMEOUT_MS: u64 = 20_000;

/// 错误体进消息的截断长度
const ERROR_BODY_HEAD: usize = 200;

/// `base_resp.status_code`（缺失视为 0：有些接口不带 base_resp 直接给数据）
fn business_code(payload: &Value) -> i64 {
    payload
        .pointer("/base_resp/status_code")
        .and_then(Value::as_i64)
        .or_else(|| {
            payload
                .pointer("/base_resp/status_code")
                .and_then(Value::as_str)
                .and_then(|text| text.trim().parse::<i64>().ok())
        })
        .unwrap_or(0)
}

/// `base_resp.status_msg`
fn upstream_message(payload: &Value) -> String {
    payload
        .pointer("/base_resp/status_msg")
        .and_then(Value::as_str)
        .or_else(|| payload.get("message").and_then(Value::as_str))
        .map(str::trim)
        .unwrap_or("")
        .to_string()
}

/// 业务码非 0 → 网关错误（502：上游业务失败；文案带上游原文）
fn ensure_business_ok(payload: &Value, what: &str) -> Result<(), GatewayError> {
    let code = business_code(payload);
    if code == 0 {
        return Ok(());
    }
    let message = upstream_message(payload);
    Err(GatewayError::with_status(
        502,
        if message.trim().is_empty() {
            format!("MiniMax {what}失败（上游 status_code={code}）")
        } else {
            format!("MiniMax {what}失败：{message}")
        },
    ))
}

/// 带 Bearer 打一次 JSON 接口。
async fn bearer_request(
    method: &str,
    url: &str,
    access_token: &str,
    body: Option<&Value>,
    proxy: Option<&proto::WireProxy>,
    what: &str,
) -> Result<Value, GatewayError> {
    let token = access_token.trim();
    if token.is_empty() {
        return Err(GatewayError::with_status(
            401,
            format!("MiniMax 账号缺少 access token，无法{what}（请重新登录或添加账号）"),
        ));
    }
    let headers = vec![("Authorization".to_string(), format!("Bearer {token}"))];
    let response = crate::http::send_raw(
        method,
        url,
        body,
        &headers,
        proxy,
        Some(REQUEST_TIMEOUT_MS),
    )
    .await
    .map_err(|error| {
        if error.is_timeout() {
            GatewayError::with_status(504, format!("MiniMax {what}超时，请稍后重试"))
        } else {
            GatewayError::with_status(
                502,
                format!("MiniMax {what}失败：{}", crate::http::describe(&error)),
            )
        }
    })?;
    if !response.ok {
        if response.status == 401 || response.status == 403 {
            return Err(GatewayError::with_status(
                401,
                format!(
                    "MiniMax {what}：登录态已失效（HTTP {}），请重新完成网页登录后添加账号",
                    response.status
                ),
            ));
        }
        return Err(GatewayError::with_status(
            response.status as i32,
            format!("MiniMax {what}返回 HTTP {}", response.status),
        ));
    }
    Ok(response.payload.unwrap_or(Value::Null))
}

/// 一次签到（状态 → 领取 → 判定），产出插件协议的 `CheckinReport`。
///
/// `force`（401 之后强制刷令牌）由 [`oauth::ensure_fresh`] 承接：这里先按
/// 常规路径取凭证，401 由调用方（trait 的 `checkin`）重试一次。
pub async fn run(ctx: &PluginCtx, account: &str) -> Result<proto::CheckinReport, GatewayError> {
    let (record, _) = oauth::snapshot(ctx, account).await?;
    let credentials = oauth::ensure_fresh(ctx, account, false).await?;
    let token = credentials.access_token.as_str();
    let proxy = record
        .get("proxy")
        .and_then(|value| serde_json::from_value::<proto::WireProxy>(value.clone()).ok());

    // ① 状态：先看今天有没有可领项（纯为文案与「已领过」判定服务）
    let status_payload = bearer_request(
        "GET",
        &endpoints::signin_status_url(),
        token,
        None,
        proxy.as_ref(),
        "签到状态查询",
    )
    .await?;
    ensure_business_ok(&status_payload, "签到状态查询")?;
    let data = status_payload
        .get("data")
        .filter(|value| value.is_object())
        .unwrap_or(&status_payload);
    let days = data
        .get("days")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let today = days
        .iter()
        .find(|day| day.get("is_today").and_then(Value::as_bool).unwrap_or(false));
    let has_claimable = days
        .iter()
        .any(|day| day.get("status").and_then(Value::as_i64) == Some(2));
    let today_claimed =
        today.and_then(|day| day.get("status").and_then(Value::as_i64)) == Some(3);

    // 无可领项：今天已领 → Done；否则中性未领取（断签 / 未开始）
    if !has_claimable {
        if today_claimed {
            return Ok(proto::CheckinReport {
                outcome: proto::CheckinOutcome::Done,
                credit: None,
                streak: None,
                message: Some("今日已签到".to_string()),
            });
        }
        return Ok(proto::CheckinReport {
            outcome: proto::CheckinOutcome::Inactive,
            credit: None,
            streak: None,
            message: Some("无可领项（可能活动断签或未开始）".to_string()),
        });
    }

    // ② 领取（幂等；body 为空对象）
    let claim_payload = bearer_request(
        "POST",
        &endpoints::signin_claim_url(),
        token,
        Some(&json!({})),
        proxy.as_ref(),
        "每日签到领取",
    )
    .await?;
    ensure_business_ok(&claim_payload, "每日签到领取")?;
    let claim_data = claim_payload
        .get("data")
        .filter(|value| value.is_object())
        .unwrap_or(&claim_payload);
    match claim_data
        .get("claim_result")
        .and_then(Value::as_i64)
    {
        Some(1) => {
            let points = claim_data
                .get("points")
                .and_then(Value::as_i64)
                .or_else(|| {
                    claim_data
                        .get("points")
                        .and_then(Value::as_f64)
                        .map(|value| value as i64)
                });
            crate::logging::log(
                "[Checkin]",
                &format!(
                    "✅ MiniMax 签到成功{}",
                    points.map(|p| format!("（+{p} 积分）")).unwrap_or_default()
                ),
            );
            Ok(proto::CheckinReport {
                outcome: proto::CheckinOutcome::Claimed,
                credit: points.map(|points| format!("+{points} 积分")),
                streak: None,
                message: None,
            })
        }
        Some(2) => Ok(proto::CheckinReport {
            outcome: proto::CheckinOutcome::Done,
            credit: None,
            streak: None,
            message: Some("今日已签到".to_string()),
        }),
        _ => {
            let raw = claim_data.to_string();
            let head: String = raw.chars().take(ERROR_BODY_HEAD).collect();
            Ok(proto::CheckinReport {
                outcome: proto::CheckinOutcome::Failed,
                credit: None,
                streak: None,
                message: Some(format!("签到结果未知：{head}")),
            })
        }
    }
}
