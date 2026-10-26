//! 插件控制面错误的 HTTP 状态与业务码兼容解码。

use agent2api_provider_protocol as proto;
use serde::Deserialize;

use crate::server::errors::GatewayError;

const HTTP_CODE_PREFIX: &str = "provider_http:";

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ErrorMetadata {
    status: i32,
    #[serde(default)]
    code: Option<String>,
    #[serde(default)]
    upstream_code: Option<i64>,
}

pub(super) fn control_error(error: &proto::WireError) -> Option<GatewayError> {
    let payload = error.code.as_deref()?.strip_prefix(HTTP_CODE_PREFIX)?;
    let metadata: ErrorMetadata = serde_json::from_str(payload).ok()?;
    if !(400..600).contains(&metadata.status) {
        return None;
    }
    let mut failure = GatewayError::with_status(metadata.status, error.message.clone())
        .with_optional_code(metadata.upstream_code);
    failure.code = metadata.code;
    Some(failure)
}
