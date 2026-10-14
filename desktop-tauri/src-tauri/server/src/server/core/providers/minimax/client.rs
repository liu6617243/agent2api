//! MiniMax agent 网关（`agent.minimax.cn`）的 Bearer 请求薄封装。
//!
//! 三个消费者（balance / checkin / 未来的目录）共用同一套超时、鉴权头与
//! 「base_resp 业务码 → 人话」口径。推理转发不走这里（它由编排层按
//! `ChatRequestPlan` 直接发 SSE）。
//!
//! ── 业务码口径（实测）────────────────────────────────────────
//! MiniMax 的管理接口用 HTTP 200 + `{ base_resp: { status_code, status_msg } }`
//! 表达业务结果：`status_code = 0` 为成功，非 0 是业务错误（HTTP 码看不出来）。
//! 鉴权失败可能直接回 401/403，也可能在 200 里给非 0 业务码，因此两处都要判。
//!
//! ── 硬约束 ──────────────────────────────────────────────────
//! release 是 `panic=abort`：本文件零 unwrap/expect/panic。

use serde_json::Value;

use crate::server::core::auth_http::send_raw;
use crate::server::core::proxies::ResolvedProxy;
use crate::server::errors::GatewayError;

/// 管理接口超时（与 Loomy 同档）
const REQUEST_TIMEOUT_MS: u64 = 20_000;

/// 向 agent 网关发一次带 `Authorization: Bearer` 的请求。
///
/// `method` 只支持 GET / POST（`send_raw` 的形态）；`body` 为 None 时不发体，
/// 为 Some 时按 JSON 发送（Content-Type 由 send_raw 补）。
/// 返回响应体；HTTP 失败 / 超时翻译成网关错误。**业务码由调用方判定**。
pub async fn bearer_request(
    method: &str,
    url: &str,
    access_token: &str,
    body: Option<&Value>,
    proxy: Option<&ResolvedProxy>,
    what: &str,
) -> Result<Value, GatewayError> {
    let token = access_token.trim();
    if token.is_empty() {
        return Err(GatewayError::with_status(
            401,
            format!("MiniMax 账号缺少 access token，无法{what}（请先刷新或重新粘贴凭证）"),
        ));
    }
    let headers = vec![("Authorization".to_string(), format!("Bearer {token}"))];
    let response = send_raw(method, url, body, &headers, proxy, Some(REQUEST_TIMEOUT_MS))
        .await
        .map_err(|error| {
            if error.is_timeout() {
                GatewayError::with_status(504, format!("MiniMax {what}超时，请稍后重试"))
            } else {
                GatewayError::with_status(502, format!("MiniMax {what}失败：{error}"))
            }
        })?;
    if !response.ok {
        if response.status == 401 || response.status == 403 {
            return Err(GatewayError::with_status(
                401,
                format!(
                    "MiniMax {what}：登录态已失效（HTTP {}），请重新授权或粘贴新的 refreshToken",
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

/// `base_resp.status_code`（缺失视为 0：有些接口不带 base_resp 直接给数据）
pub fn business_code(payload: &Value) -> i64 {
    payload
        .pointer("/base_resp/status_code")
        .and_then(Value::as_i64)
        // 兼容数字字符串形态
        .or_else(|| {
            payload
                .pointer("/base_resp/status_code")
                .and_then(Value::as_str)
                .and_then(|text| text.trim().parse::<i64>().ok())
        })
        .unwrap_or(0)
}

/// `base_resp.status_msg`
pub fn upstream_message(payload: &Value) -> String {
    payload
        .pointer("/base_resp/status_msg")
        .and_then(Value::as_str)
        .or_else(|| payload.get("message").and_then(Value::as_str))
        .map(str::trim)
        .unwrap_or("")
        .to_string()
}

/// 业务码非 0 → 网关错误（502：上游业务失败；文案带上游原文）。
pub fn ensure_business_ok(payload: &Value, what: &str) -> Result<(), GatewayError> {
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
