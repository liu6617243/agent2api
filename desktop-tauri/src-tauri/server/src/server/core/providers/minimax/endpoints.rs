//! MiniMax（MiniMax Code / agent.minimax.cn）的上游地址与 OAuth 常量。
//!
//! ── 上游长什么样（实测定案，来源：本地 shim 逆向 + DSH dsh-codearts-auth）──
//! MiniMax 是「一条账号体系 + 两个域」：
//!
//! ```text
//!   账号（OAuth）  https://account.minimax.cn/oauth2/token   （form，refresh_token 换 access_token）
//!   推理 + 管理    https://agent.minimax.cn                  （Bearer access_token）
//!     推理         POST /mavis/api/v1/llm/v1/messages        （Anthropic Messages 协议）
//!     余额         GET  /minimax-cloud/api/v1/credit/details （Σ details[].remaining_amount）
//!     签到状态     GET  /minimax-cloud/api/v1/signin/status?timezone_id=Asia/Shanghai
//!     签到领取     POST /minimax-cloud/api/v1/signin/claim?timezone_id=Asia/Shanghai
//! ```
//!
//! OAuth 参数（客户端内置的公开客户端，不是用户凭证，与 Loomy 的签名对同性质）：
//!   - `client_id = mcode-public`
//!   - `scope = agent.default`
//!   - `audience = agent-backend`
//!   - `grant_type = refresh_token`
//!
//! ── 一次性轮换的 refresh_token（本家最关键的一条事实）──────────
//! MiniMax 的 `access_token` 约 3600 秒过期，`refresh_token` **每刷新一次就轮换**
//! （旧值当场吊销，响应里下发新值）。这与 Google（refresh_token 长寿命、通常不回传）
//! 相反：本家刷新成功**必须**把新 refresh_token 回写落盘，否则下一次刷新拿旧的
//! 去打必然 invalid_grant。因此回写走 `update_minimax_credentials_if_current`
//! （比较-再写）且 `refresh_token` 是**必写项**（见 `oauth.rs`）。
//!
//! ── 基址为什么可覆盖 ────────────────────────────────────────
//! 与 Loomy 同口径：域名以常量内置，环境变量可覆盖（`MINIMAX_ACCOUNT_BASE_URL` /
//! `MINIMAX_API_BASE_URL`），换端点不需要重新编译。
//!
//! ── 硬约束 ──────────────────────────────────────────────────
//! release 是 `panic=abort`：本文件零 unwrap/expect/panic。

/// 账号（OAuth token 端点）默认基址
pub const DEFAULT_ACCOUNT_BASE_URL: &str = "https://account.minimax.cn";

/// 推理 + 管理（agent 网关）默认基址
pub const DEFAULT_API_BASE_URL: &str = "https://agent.minimax.cn";

/// OAuth token 端点路径（相对账号基址）
pub const TOKEN_PATH: &str = "/oauth2/token";

/// 推理路径（Anthropic Messages，相对 API 基址）
pub const MESSAGES_PATH: &str = "/mavis/api/v1/llm/v1/messages";

/// 余额明细路径（相对 API 基址）
pub const CREDIT_PATH: &str = "/minimax-cloud/api/v1/credit/details";

/// 签到状态路径（相对 API 基址，含固定 query；与参考实现逐字一致）
pub const SIGNIN_STATUS_PATH: &str =
    "/minimax-cloud/api/v1/signin/status?timezone_id=Asia%2FShanghai";

/// 签到领取路径（相对 API 基址，含固定 query；POST，幂等）
pub const SIGNIN_CLAIM_PATH: &str =
    "/minimax-cloud/api/v1/signin/claim?timezone_id=Asia%2FShanghai";

/// 客户端内置的公开 OAuth client_id（随客户端分发，不是用户凭证）
pub const CLIENT_ID: &str = "mcode-public";

/// OAuth scope（固定 `agent.default`）
pub const SCOPE: &str = "agent.default";

/// OAuth audience（固定 `agent-backend`）
pub const AUDIENCE: &str = "agent-backend";

/// 刷新用的 grant_type（下划线形态，别写成 refreshToken）
pub const GRANT_TYPE_REFRESH: &str = "refresh_token";

/// Anthropic 协议版本头（与 ZCode 活动套餐通道同值）
pub const ANTHROPIC_VERSION: &str = "2023-06-01";

/// 读环境变量覆盖（空值视为未设置），去掉尾部斜杠
fn env_override(name: &str) -> Option<String> {
    std::env::var(name)
        .ok()
        .map(|value| value.trim().trim_end_matches('/').to_string())
        .filter(|value| !value.is_empty())
}

/// 账号（OAuth）基址：`MINIMAX_ACCOUNT_BASE_URL` 可覆盖
pub fn account_base_url() -> String {
    env_override("MINIMAX_ACCOUNT_BASE_URL")
        .unwrap_or_else(|| DEFAULT_ACCOUNT_BASE_URL.to_string())
}

/// 推理 + 管理（API）基址：`MINIMAX_API_BASE_URL` 可覆盖
pub fn api_base_url() -> String {
    env_override("MINIMAX_API_BASE_URL").unwrap_or_else(|| DEFAULT_API_BASE_URL.to_string())
}

/// OAuth token 端点完整地址
pub fn token_url() -> String {
    format!("{}{TOKEN_PATH}", account_base_url())
}

/// 推理端点完整地址
pub fn messages_url() -> String {
    format!("{}{MESSAGES_PATH}", api_base_url())
}

/// 余额端点完整地址
pub fn credit_url() -> String {
    format!("{}{CREDIT_PATH}", api_base_url())
}

/// 签到状态端点完整地址
pub fn signin_status_url() -> String {
    format!("{}{SIGNIN_STATUS_PATH}", api_base_url())
}

/// 签到领取端点完整地址
pub fn signin_claim_url() -> String {
    format!("{}{SIGNIN_CLAIM_PATH}", api_base_url())
}
