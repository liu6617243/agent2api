//! MiniMax 的推理转发（`build_chat` 的实现体）：Anthropic Messages 通道。
//!
//! ── 落点 ────────────────────────────────────────────────────
//! `POST {api_base}/mavis/api/v1/llm/v1/messages`，说的是 **Anthropic Messages**：
//! 实测只认 `Authorization: Bearer <OAuth access token>`，不认 `x-api-key`。
//! 请求体复用同 crate 里 `zcode::anthropic_outbound` 的共用转换（system 折顶、
//! tool 消息拆块、reasoning 折 thinking 预算）；响应由网关按
//! `UpstreamKind::Anthropic` 翻回 chat SSE —— 账号轮换 / 限额冷却 / 退避重试 /
//! usage 记账全部留在编排层，插件只负责「怎么发」。
//!
//! ── 不改写模型名 ────────────────────────────────────────────
//! 对外 id 就是上游字面值（见 `models.rs`），只在大小写不一致时归一，
//! `sse_model_rewrite` 保持 false。
//!
//! ── 硬约束 ──────────────────────────────────────────────────
//! release 是 `panic=abort`：本文件零 unwrap/expect/panic。

use agent2api_provider_protocol as proto;
use agent2api_provider_sdk::ChatPlan;
use serde_json::Value;

use crate::errors::GatewayError;
use crate::providers::zcode::anthropic_outbound;

use super::oauth::Credentials;
use super::{endpoints, models};

/// Anthropic 协议版本头（与 zcode 同值；本家 messages 端点同样认它）。
const ANTHROPIC_VERSION: &str = "2023-06-01";

/// 组装一次 Anthropic Messages 请求。
pub fn build(credential: &Credentials, body: &Value) -> Result<ChatPlan, GatewayError> {
    let token = credential.access_token.trim();
    if token.is_empty() {
        return Err(GatewayError::with_status(
            401,
            "MiniMax 账号缺少 access token，无法转发（请刷新令牌或重新登录）",
        ));
    }
    let requested = body
        .get("model")
        .and_then(Value::as_str)
        .map(str::trim)
        .unwrap_or("");
    if requested.is_empty() {
        return Err(GatewayError::with_status(
            400,
            "MiniMax 请求缺少 model 字段：请在客户端指定模型（如 MiniMax-M3）",
        ));
    }
    let wire_model = models::upstream_model_id(requested);
    let payload = anthropic_outbound::anthropic_request_from_chat(body, &wire_model)
        .map_err(|message| {
            GatewayError::with_status(400, format!("MiniMax 请求体转换失败：{message}"))
        })?;
    let headers = vec![
        ("Content-Type".to_string(), "application/json".to_string()),
        ("Authorization".to_string(), format!("Bearer {token}")),
        ("anthropic-version".to_string(), ANTHROPIC_VERSION.to_string()),
        ("Accept".to_string(), "text/event-stream".to_string()),
    ];
    Ok(ChatPlan {
        url: format!("{}{}", endpoints::api_base_url(), endpoints::MESSAGES_PATH),
        headers,
        body: payload,
        upstream: proto::UpstreamKind::Anthropic,
    })
}
