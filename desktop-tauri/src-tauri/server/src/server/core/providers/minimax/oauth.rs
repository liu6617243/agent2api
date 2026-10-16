//! MiniMax 的 OAuth：`grant_type=refresh_token` 刷新（form 表单）+ 单飞 + 比较-再写。
//!
//! ── 协议（实测定案，参考实现 minimax_shim.py 逐字一致）──────────
//! ```text
//! POST https://account.minimax.cn/oauth2/token
//! Content-Type: application/x-www-form-urlencoded
//! Accept: application/json
//!
//! grant_type    = refresh_token
//! refresh_token = <账号记录里的主凭证>
//! client_id     = mcode-public
//! scope         = agent.default
//! audience      = agent-backend
//!
//! 200 → {access_token, expires_in(秒≈3600), refresh_token(新！), token_type?, ...}
//! ```
//! 表单不能走 `auth_http::send_raw`（它只发 JSON），因此这里直接
//! `egress::client_for(proxy).post(...).form(...)`，与 `antigravity::oauth` /
//! `codearts::oauth` 同一手法。
//!
//! ── refresh_token 一次性轮换（与 Google 那家相反的处置）────────
//! MiniMax **每次刷新都下发新的 refresh_token，旧的当场作废**。因此
//! [`refresh_and_save`] 把新 refreshToken 当**必写项**传给
//! `update_minimax_credentials_if_current`；上游万一没回传新值（异常形态），
//! 保留旧值并打一条 warning —— 那种情况下下一次刷新会失败，但至少不会把
//! 可用凭证洗成空。
//!
//! ── 单飞 + 比较再写 ─────────────────────────────────────────
//! 与 Antigravity 同一条纪律：并发请求只打一次上游；回写前比对刷新前的
//! refresh_token 指纹，期间用户重新粘贴则报 Stale、绝不覆盖新凭证。
//! 一次性轮换让「单飞」更关键：两个并发刷新各拿旧 rt 去打，第二个必
//! invalid_grant，还会把第一个刚写回的新 rt 判定成 Stale 的语境搅乱。
//!
//! ── 失败处置 ────────────────────────────────────────────────
//!   - 上游回 error / invalid_grant 类失败或响应缺 access_token → 401 +
//!     「refresh token 已失效或被轮换，请重新授权后粘贴新的 refreshToken」
//!     （MiniMax 的 rt 一旦被并发刷新消耗，症状就是这条路走不通）；
//!   - 连接类失败 → 502（可重试），带网络错误原文（截断）。
//!
//! ── 硬约束 ──────────────────────────────────────────────────
//! release 是 `panic=abort`：本文件零 unwrap/expect/panic；日志绝不打印令牌本体。

use std::sync::OnceLock;
use std::time::Duration;

use serde_json::Value;

use crate::server::core::account_store::{AccountStore, CredentialWrite};
use crate::server::core::egress;
use crate::server::core::proxies::{ProxyResolution, ResolvedProxy};
use crate::server::core::providers::refresh_flight::{self, Join, Table};
use crate::server::errors::GatewayError;
use crate::server::logging;

use super::credentials::MinimaxCredentials;
use super::endpoints;

/// token 端点请求超时（与别家 30 秒同档）
const REQUEST_TIMEOUT_MS: u64 = 30_000;

/// 上游没给 `expires_in` 时的保守默认（MiniMax 实测 3600）
const DEFAULT_EXPIRES_IN_SECONDS: i64 = 3600;

/// 供同 provider 的登录链路复用的默认有效期（秒）。
///
/// 与 [`DEFAULT_EXPIRES_IN_SECONDS`] 同值，但公开出来给 `login.rs` 用 ——
/// 两处各写一份常量的话，改了登录那处就与刷新链不一致，而这种偏差要等到
/// 令牌提前失效时才暴露出来。
pub fn oauth_default_expires_in_seconds() -> i64 {
    DEFAULT_EXPIRES_IN_SECONDS
}

/// 上游错误文在报错里的截断长度
const MAX_ERROR_CHARS: usize = 300;

/// 单飞表（键 = 账号文件 + 账号 id + refresh_token 指纹，见 [`ensure_fresh`]）
static FLIGHTS: OnceLock<Table<MinimaxCredentials>> = OnceLock::new();

/// 一次 token 端点调用的结果
#[derive(Clone, Debug)]
pub struct TokenResponse {
    /// 新的访问令牌
    pub access_token: String,
    /// 新的 refresh_token（MiniMax **每次都会下发**；None 是异常形态）
    pub refresh_token: Option<String>,
    /// 有效期（秒）
    pub expires_in: i64,
    /// `token_type`（`Bearer`）
    pub token_type: String,
}

/// 账号级出口代理（解析失败按 400 报，不静默直连）—— 与 `antigravity::oauth`
/// 同名函数同一语义。
pub fn account_proxy(record: &Value) -> Result<Option<ResolvedProxy>, GatewayError> {
    match crate::server::core::proxies::resolve_account_proxy(record.get("proxy")) {
        Some(ProxyResolution::Resolved(proxy)) => Ok(Some(proxy)),
        Some(ProxyResolution::Failed(reason)) => Err(GatewayError::with_status(400, reason)),
        None => Ok(None),
    }
}

/// 截断上游错误文（按字符，UTF-8 安全）
fn truncate(text: &str) -> String {
    let collapsed: String = text.chars().filter(|character| !character.is_control()).collect();
    match collapsed.char_indices().nth(MAX_ERROR_CHARS) {
        Some((index, _)) => format!("{}…", &collapsed[..index]),
        None => collapsed,
    }
}

/// 解析 token 端点读数（`expires_in` 接受数字与数字字符串两种形态）
fn expires_in_of(payload: &Value) -> i64 {
    match payload.get("expires_in") {
        Some(Value::Number(number)) => number.as_i64().unwrap_or(0),
        Some(Value::String(text)) => text.trim().parse::<i64>().unwrap_or(0),
        _ => 0,
    }
}

/// 解析成功响应（200 且带 `access_token`）
fn parse_token_response(payload: &Value) -> Result<TokenResponse, GatewayError> {
    let access_token = payload
        .get("access_token")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|text| !text.is_empty())
        .map(str::to_string)
        .ok_or_else(|| {
            GatewayError::with_status(
                401,
                "MiniMax 刷新响应缺少 access_token（refresh token 可能已被轮换或撤销）：\
                 请重新完成设备码授权后粘贴新的 refreshToken",
            )
        })?;
    let refresh_token = payload
        .get("refresh_token")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|text| !text.is_empty())
        .map(str::to_string);
    let expires_in = expires_in_of(payload);
    let token_type = payload
        .get("token_type")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|text| !text.is_empty())
        .unwrap_or("Bearer")
        .to_string();
    Ok(TokenResponse {
        access_token,
        refresh_token,
        expires_in,
        token_type,
    })
}

/// 打一次 token 端点（**纯网络 + 解析，不落盘**）。
///
/// 日志只打「成功/失败 + 状态码」，**绝不打印 token 本体或表单内容**。
pub async fn refresh_access_token(
    refresh_token: &str,
    proxy: Option<&ResolvedProxy>,
) -> Result<TokenResponse, GatewayError> {
    let refresh_token = refresh_token.trim();
    if refresh_token.is_empty() {
        return Err(GatewayError::with_status(
            401,
            "MiniMax 账号缺少 refresh token，无法续期，请重新授权后粘贴新的 refreshToken",
        ));
    }
    let client = super::client_for(proxy);
    let response = client
        .post(endpoints::token_url())
        .timeout(Duration::from_millis(REQUEST_TIMEOUT_MS))
        .header("Content-Type", "application/x-www-form-urlencoded")
        .header("Accept", "application/json")
        .form(&[
            ("grant_type", endpoints::GRANT_TYPE_REFRESH),
            ("refresh_token", refresh_token),
            ("client_id", endpoints::CLIENT_ID),
            ("scope", endpoints::SCOPE),
            ("audience", endpoints::AUDIENCE),
        ])
        .send()
        .await
        .map_err(|error| {
            if error.is_timeout() {
                GatewayError::with_status(504, "MiniMax 刷新 token 超时，请稍后重试")
            } else {
                GatewayError::with_status(
                    502,
                    format!(
                        "MiniMax 刷新请求失败：{}",
                        egress::describe_error_detail(&error)
                    ),
                )
            }
        })?;
    let status = response.status().as_u16();
    let text = response.text().await.unwrap_or_default();
    let payload: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
    if !(200..300).contains(&status) {
        // 把上游文案带上但不透出令牌：错误体里若有 base_resp / message / error
        let detail = payload
            .get("error_description")
            .and_then(Value::as_str)
            .or_else(|| payload.get("error").and_then(Value::as_str))
            .or_else(|| payload.get("base_resp").and_then(|br| br.get("status_msg")).and_then(Value::as_str))
            .or_else(|| payload.get("message").and_then(Value::as_str))
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(truncate)
            .unwrap_or_else(|| truncate(&text));
        let hint = if detail.is_empty() {
            String::new()
        } else {
            format!("：{detail}")
        };
        // 4xx 语义上都是「这份 rt 不被接受」：过期/被轮换/被撤销 —— 给用户
        // 一条能照做的出路（重新设备码授权）；5xx 才是上游抖动（可重试）。
        let error = if (400..500).contains(&status) {
            GatewayError::with_status(
                401,
                format!(
                    "MiniMax 的 refresh token 未被接受（HTTP {status}）：它的续期串是一次性的，\
                     被消耗或撤销后就再也用不了{hint}；请重新完成设备码授权后粘贴新的 refreshToken"
                ),
            )
        } else {
            GatewayError::with_status(502, format!("MiniMax 刷新 token 失败（HTTP {status}）{hint}"))
        };
        logging::log(
            "[MiniMax]",
            &format!("❌ 刷新 token 失败（HTTP {status}）：{}", error.message),
        );
        return Err(error);
    }
    parse_token_response(&payload)
}

/// 取可用凭证（`force = true` 时不看临期窗口，401 之后强制刷一次）。
pub async fn ensure_fresh(
    store: &AccountStore,
    account_id: &str,
    force: bool,
) -> Result<MinimaxCredentials, GatewayError> {
    let record = read_record(store, account_id)?;
    let credentials = super::credentials::from_record(Some(&record))?;
    if !force && !credentials.expiring() {
        return Ok(credentials);
    }
    if !credentials.can_refresh() {
        return Err(GatewayError::with_status(
            401,
            "MiniMax 账号缺少 refresh token，无法续期：请重新授权后粘贴完整的令牌（refreshToken）",
        ));
    }
    let key = format!(
        "{}:{}:{}",
        store.file_string(),
        credentials.id,
        refresh_flight::fingerprint(&credentials.refresh_token)
    );
    match FLIGHTS.get_or_init(Table::new).join(&key) {
        Join::Waiter(waiter) => waiter.wait().await,
        Join::Leader(leader) => {
            let result = refresh_and_save(store, &record, &credentials).await;
            leader.finish(result.clone());
            result
        }
    }
}

/// 打一次刷新 + 回写（单飞的 leader 走这一段）。
///
/// 与 Antigravity 的差别：MiniMax 的 refreshToken 每次都会轮换，所以
/// `update_minimax_credentials_if_current` 收到新 rt 时**覆盖**（None 才保留旧值）。
async fn refresh_and_save(
    store: &AccountStore,
    record: &Value,
    credentials: &MinimaxCredentials,
) -> Result<MinimaxCredentials, GatewayError> {
    let proxy = account_proxy(record)?;
    let response = refresh_access_token(&credentials.refresh_token, proxy.as_ref()).await?;
    let mut fresh = credentials.clone();
    fresh.access_token = response.access_token.trim().to_string();
    if let Some(refresh) = response.refresh_token.as_ref() {
        fresh.refresh_token = refresh.trim().to_string();
    } else {
        // 异常形态：上游没回新 rt（一次性轮换的契约被破坏）。保留旧值继续用，
        // 并如实记一句 —— 下一次刷新大概率 401，那时文案会指向重新授权。
        logging::log(
            "[MiniMax]",
            "⚠️ 刷新响应未携带新的 refresh_token（该家正常情况下必发），保留旧值继续",
        );
    }
    let expires_in = if (1..=7 * 24 * 3600).contains(&response.expires_in) {
        response.expires_in
    } else {
        DEFAULT_EXPIRES_IN_SECONDS
    };
    fresh.expires_at = logging::now_ms() + expires_in * 1000;
    logging::verbose(
        "[MiniMax]",
        &format!(
            "token 刷新成功（token_type {}，有效期 {expires_in} 秒）",
            response.token_type
        ),
    );
    match store.update_minimax_credentials_if_current(
        &fresh.id,
        &credentials.refresh_token,
        &fresh.access_token,
        response.refresh_token.as_deref(),
        fresh.expires_at,
    ) {
        Ok(CredentialWrite::Written) => Ok(fresh),
        Ok(CredentialWrite::Stale) => {
            // 记录在刷新期间被换过（重新粘贴 / 手工编辑）：本次请求用刷出的新令牌，
            // 但不覆盖记录（与 Antigravity / Trae 同一处置）。
            logging::log(
                "[MiniMax]",
                "⚠️ 刷新结果未能写回账号记录（期间被改动过），本次请求用新令牌，下次以记录里的为准",
            );
            Ok(fresh)
        }
        Err(reason) => Err(GatewayError::with_status(500, reason)),
    }
}

/// 读账号记录（点名 vs 队首）。点名的账号取不到直接报错，不回落队首。
pub fn read_record(store: &AccountStore, account_id: &str) -> Result<Value, GatewayError> {
    if account_id.trim().is_empty() {
        return store
            .minimax_account_record("")
            .ok_or_else(|| GatewayError::with_status(401, "没有可用的 MiniMax 账号，请先在「账号」页添加"));
    }
    store
        .minimax_account_record(account_id)
        .ok_or_else(|| GatewayError::with_status(401, "找不到该 MiniMax 账号"))
}
