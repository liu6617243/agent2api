use axum::http::StatusCode;
use axum::response::Response;

use crate::server::core::protocol::native::WireProtocol;
use crate::server::core::upstream::native::{NativeBody, NativeResponse};
use crate::server::errors::GatewayError;

use super::super::disconnect_guard::DisconnectGuard;
use super::super::pipeline::{self, Dialect, RecordContext, RecordingStream, TurnSource};

/// 协议 → 留存方言。
///
/// ── 为什么原生转发也要留存 ──────────────────────────────────
/// 原生分支（`NativeRequest::routed()`）恰恰是 Codex / Claude Code 的常态路径：
/// 上下游协议相同就完全绕过 Chat 投影。只在投影那条路上做留存，等于「用 Codex
/// 的用户永远不产生记忆」—— 而那正是这台机器上的主力客户端。
fn dialect_of(protocol: WireProtocol) -> Dialect {
    match protocol {
        WireProtocol::Anthropic => Dialect::Anthropic,
        WireProtocol::Responses => Dialect::Responses,
        WireProtocol::Chat => Dialect::Chat,
    }
}

pub(in crate::server::api) fn respond(
    response: NativeResponse,
    context: RecordContext,
    turn_source: &TurnSource,
    guard: &mut DisconnectGuard,
) -> Response {
    let status = StatusCode::from_u16(response.status).unwrap_or(StatusCode::OK);
    let mut context = RecordContext {
        status: i64::from(status.as_u16()),
        ..context
    };
    let mut result = match response.body {
        NativeBody::Stream(source) => {
            let terminals = match response.protocol {
                WireProtocol::Anthropic => pipeline::terminal::ANTHROPIC,
                WireProtocol::Responses => pipeline::terminal::RESPONSES,
                WireProtocol::Chat => pipeline::terminal::CHAT,
            };
            // 原生分支的字节就是上游的原生协议帧（没有经过投影转换），
            // 所以方言直接按协议判 —— 用 Chat 口径去读 Responses 的
            // `output_text.delta` 会读到空串，而且不会报错。
            let turn = turn_source.capture(dialect_of(response.protocol));
            let stream =
                RecordingStream::with_terminals(source, context, terminals).with_turn(turn);
            guard.handoff();
            pipeline::sse_response(status, Box::new(stream))
        }
        NativeBody::Json(body) => {
            context.raw_response = pipeline::capture_raw_json(&body);
            pipeline::record_entry(&context, None);
            guard.complete();
            // 请求后留存：原生非流式体的回答位置按协议分三种（见
            // `pipeline::answer_text_*`），这里按同一个 `protocol` 选口径
            let answer = match response.protocol {
                WireProtocol::Anthropic => pipeline::answer_text_anthropic(&body),
                WireProtocol::Responses => pipeline::answer_text_responses(&body),
                WireProtocol::Chat => pipeline::answer_text_chat(&body),
            };
            pipeline::retain_answer(turn_source, &answer, false);
            let mut result = pipeline::json_response(body);
            *result.status_mut() = status;
            result
        }
    };
    result.headers_mut().extend(response.headers);
    result
}

pub(super) fn error_response(
    protocol: WireProtocol,
    status: u16,
    error: &GatewayError,
) -> Option<Response> {
    let (source, body, headers) = error.native_response.as_ref()?;
    if *source != protocol {
        return None;
    }
    let mut response = pipeline::json_response(body.clone());
    *response.status_mut() = StatusCode::from_u16(status).unwrap_or(StatusCode::BAD_GATEWAY);
    response.headers_mut().extend(headers.clone());
    Some(response)
}
