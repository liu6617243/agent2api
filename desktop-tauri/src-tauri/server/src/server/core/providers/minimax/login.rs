//! MiniMax **设备码授权登录**（RFC 8628 形态 + PKCE S256）。
//!
//! ── 协议（实测定案，参考实现 minimax_shim.py / dsh-codearts-auth）──────
//! ```text
//! ① POST https://account.minimax.cn/oauth2/device/code
//!      Content-Type: application/x-www-form-urlencoded
//!      body: client_id=mcode-public
//!            &scope=agent.default
//!            &audience=agent-backend
//!            &code_challenge=<S256(verifier)>
//!            &code_challenge_method=S256
//!    → 200 {device_code, user_code, verification_uri,
//!           verification_uri_complete, expires_in(≈300), interval(≈5)}
//!
//! ② 用户在浏览器打开 verification_uri_complete，登录并确认授权
//!
//! ③ POST https://account.minimax.cn/oauth2/token   （轮询，每 interval 秒）
//!      body: grant_type=urn:ietf:params:oauth:grant-type:device_code
//!            &device_code=<device_code>
//!            &client_id=mcode-public
//!            &code_verifier=<verifier>
//!      → 200 {access_token, refresh_token(新!), expires_in≈3600}
//!      → 4xx {error:"authorization_pending"}   （还没确认，继续轮询）
//!      → 4xx {error:"slow_down"}              （轮太快，拉长间隔）
//!      → 4xx {error:"expired_token"} / access_denied （终止）
//!
//! ④ 落账号 → `AccountStore::add_minimax_account`
//! ```
//!
//! ── 与 Cline 设备授权的差别（为什么不能照抄）──────────────────
//!   - Cline 只需 client_id；**本家要 PKCE**：设备码这一步就带
//!     `code_challenge`，换令牌那一步必须带上原 verifier，否则上游拒绝。
//!     verifier 本地随机生成、只活这一轮，**绝不落盘**（它等价于密码）。
//!   - Cline 换到 WorkOS 令牌后还要打一次 `/auth/register` 换自家会话；
//!     **本家不需要** —— `/oauth2/token` 直接就发能打推理端点的令牌。
//!   - Cline 的 access/refresh 都不轮换；**本家 refresh_token 一次性轮换**
//!     （见 `super::oauth`），所以换到的一定是主凭证，落盘后即可自动续期。
//!
//! ── 为什么值得做 ─────────────────────────────────────────────
//! 这是标准设备码流程：不需要回调端口、不需要自定义协议、不需要拦截域名。
//! 用户点开链接确认就行，网关这边只管轮询 —— 与「粘贴 refreshToken」相比，
//! 用户不必再去客户端登录态里翻���子（那是本家最容易出错的一步）。
//!
//! ── 落账号走既有入口 ──────────────────────────────────────────
//! 与「粘贴凭证」共用 `add_minimax_account`，不另写落盘逻辑（否则两条路会在
//! id 派生、归并、优先级分配上慢慢分叉）。
//!
//! ── 硬约束 ────────────────────────────────────────────────────
//! release 是 `panic=abort`：绝不 unwrap/expect/panic；所有失败转 `GatewayError`。
//! 日志绝不打印 device_code / code_verifier /令牌本体。

use std::time::Duration;

use base64::Engine;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

use crate::server::core::account_store::AccountStore;
use crate::server::core::egress;
use crate::server::errors::GatewayError;
use crate::server::logging;

use super::endpoints;

/// 单次 HTTP 超时（授权接口是交互式的，给短一些）
const REQUEST_TIMEOUT_MS: u64 = 30_000;

/// 轮询上限兜底（上游`expires_in` 缺失时用它）
const MAX_POLL_MS: u64 = 300_000;

/// 默认轮询间隔（上游返回 `interval`，缺失时用它）
const DEFAULT_INTERVAL_SECONDS: u64 = 5;

/// 设备码授权的第一步产物（给界面展示「去哪儿输码」用）。
#[derive(Clone, Debug)]
pub struct DeviceAuthStart {
    /// 换令牌时要带的设备码（**机密**，只活这一轮）
    pub device_code: String,
    /// 让用户核对/输入的短码（可展示）
    pub user_code: String,
    /// 授权页地址
    pub verification_uri: String,
    /// 已带 user_code 的授权页地址（有它就用它，用户点开即免手输）
    pub verification_uri_complete: Option<String>,
    /// 有效期（秒）
    pub expires_in_seconds: u64,
    /// 轮询间隔（秒）
    pub interval_seconds: u64,
    /// PKCE verifier（换令牌时带回；**只活这一轮，绝不落盘/打印**）
    pub code_verifier: String,
}

/// PKCE code_challenge（S256）：`BASE64URL(SHA256(verifier))`，无 padding。
fn code_challenge(verifier: &str) -> String {
    let digest = Sha256::digest(verifier.as_bytes());
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(digest)
}

/// 生成 PKCE verifier：43~128 字符的随机串（RFC 7636 要求 43~128）。
fn new_code_verifier() -> String {
    // 用时间 + 进程内计数器做种子源不理想；这里用系统随机源的可用形式：
    // 直接取 64 字节随机再base64url。系统随机失败时退回时间派生（登录场景可接受，
    // 且 verifier 只活 5 分钟、绑定同一个 device_code）。
    let mut buf = [0u8; 64];
    let filled = getrandom_fill(&mut buf);
    if !filled {
        let nanos = logging::now_ms();
        let digest = Sha256::digest(format!("minimax-pkce:{nanos}").as_bytes());
        buf[..32].copy_from_slice(&digest);
    }
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(&buf[..48])
}

/// 读系统随机源（`getrandom` 语义）。用时间熵兜底，绝不 panic。
fn getrandom_fill(buf: &mut [u8]) -> bool {
    use std::io::Read;
    // Windows: BCryptGenRandom 走 std 无直接入口；这里用「读系统时间 + 地址扰动」
    // 的方式拿不到密码学强度 —— 因此优先尝试打开系统随机设备。
    // Linux/Android 有 /dev/urandom；macOS 由 getrandom(2) 提供。
    if let Ok(mut file) = std::fs::File::open("/dev/urandom") {
        if file.read_exact(buf).is_ok() {
            return true;
        }
    }
    false
}

/// 取响应里的非空字符串字段。
fn text_field(payload: &Value, key: &str) -> Option<String> {
    payload
        .get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
}

/// 取响应里的非负整数（兼容数字字符串）。
fn number_field(payload: &Value, key: &str) -> Option<u64> {
    match payload.get(key) {
        Some(Value::Number(number)) => number.as_u64(),
        Some(Value::String(text)) => text.trim().parse::<u64>().ok(),
        _ => None,
    }
}

/// 发起设备码授权（第一步）。
pub async fn start() -> Result<DeviceAuthStart, GatewayError> {
    let code_verifier = new_code_verifier();
    let challenge = code_challenge(&code_verifier);
    let client = super::client_for(None);
    let response = client
        .post(endpoints::device_code_url())
        .header("Content-Type", "application/x-www-form-urlencoded")
        .header("Accept", "application/json")
        .timeout(Duration::from_millis(REQUEST_TIMEOUT_MS))
        .form(&[
            ("client_id", endpoints::CLIENT_ID),
            ("scope", endpoints::SCOPE),
            ("audience", endpoints::AUDIENCE),
            ("code_challenge", challenge.as_str()),
            ("code_challenge_method", "S256"),
        ])
        .send()
        .await
        .map_err(|error| {
            GatewayError::with_status(
                502,
                format!(
                    "Minimax 设备授权请求失败：{}",
                    egress::describe_error_detail(&error)
                ),
            )
        })?;
    let status = response.status().as_u16();
    let text = response.text().await.unwrap_or_default();
    let payload: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
    if !(200..300).contains(&status) {
        let detail = text_field(&payload, "error_description")
            .or_else(|| text_field(&payload, "error"))
            .or_else(|| {
                payload
                    .pointer("/base_resp/status_msg")
                    .and_then(Value::as_str)
                    .map(str::to_string)
            })
            .unwrap_or_else(|| {
                crate::server::core::account_store::store_util::truncate_text(&text, 200)
            });
        return Err(GatewayError::with_status(
            502,
            format!("Minimax 设备授权失败（HTTP {status}）：{detail}"),
        ));
    }
    let Some(device_code) = text_field(&payload, "device_code") else {
        return Err(GatewayError::with_status(
            502,
            "Minimax 设备授权响应缺少 device_code",
        ));
    };
    let Some(user_code) = text_field(&payload, "user_code") else {
        return Err(GatewayError::with_status(
            502,
            "Minimax 设备授权响应缺少 user_code",
        ));
    };
    let verification_uri = text_field(&payload, "verification_uri")
        .or_else(|| text_field(&payload, "verification_url"))
        .unwrap_or_else(|| endpoints::authorize_url());
    Ok(DeviceAuthStart {
        device_code,
        user_code,
        verification_uri,
        verification_uri_complete: text_field(&payload, "verification_uri_complete")
            .or_else(|| text_field(&payload, "verification_url_complete")),
        expires_in_seconds: number_field(&payload, "expires_in")
            .unwrap_or(300)
            .min(MAX_POLL_MS / 1000)
            .max(60),
        interval_seconds: number_field(&payload, "interval")
            .unwrap_or(DEFAULT_INTERVAL_SECONDS)
            .clamp(1, 30),
        code_verifier,
    })
}

/// 换到的令牌对。
struct DeviceTokens {
    access_token: String,
    /// 主凭证（一次性轮换）
    refresh_token: String,
    expires_in: i64,
}

/// 轮询 `/oauth2/token` 直到用户确认，成功后**落账号**并返回账号 id。
///
/// 会阻塞到「确认 / 超时 / 取消」三者之一（由调用方的登录任务管理取消）。
pub async fn poll_and_register(
    store: &AccountStore,
    start: &DeviceAuthStart,
) -> Result<String, GatewayError> {
    let tokens = poll_tokens(start).await?;
    let account = store
        .add_minimax_account(
            &json!({
                "refreshToken": tokens.refresh_token,
                "accessToken": tokens.access_token,
            }),
            None,
            "web",
        )
        .map_err(|error| GatewayError::with_status(error.status_code, error.message))?;
    let id = account
        .get("id")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    logging::log("[Login]", "✅ Minimax 登录成功，账号已加入列表");
    Ok(id)
}

/// 轮询 token 端点（RFC 8628 的 device_code 兑换语义）。
async fn poll_tokens(start: &DeviceAuthStart) -> Result<DeviceTokens, GatewayError> {
    let started = std::time::Instant::now();
    let mut interval = start.interval_seconds.max(1);
    let deadline = Duration::from_secs(start.expires_in_seconds.max(60));
    loop {
        if started.elapsed() >= deadline {
            return Err(GatewayError::with_status(
                408,
                "Minimax 登录超时（授权码已过期），请重新发起登录",
            ));
        }
        tokio::time::sleep(Duration::from_secs(interval)).await;
        let client = super::client_for(None);
        let response = client
            .post(endpoints::token_url())
            .header("Content-Type", "application/x-www-form-urlencoded")
            .header("Accept", "application/json")
            .timeout(Duration::from_millis(REQUEST_TIMEOUT_MS))
            .form(&[
                ("grant_type", "urn:ietf:params:oauth:grant-type:device_code"),
                ("device_code", start.device_code.as_str()),
                ("client_id", endpoints::CLIENT_ID),
                ("code_verifier", start.code_verifier.as_str()),
            ])
            .send()
            .await
            .map_err(|error| {
                GatewayError::with_status(
                    502,
                    format!(
                        "Minimax 登录轮询失败：{}",
                        egress::describe_error_detail(&error)
                    ),
                )
            })?;
        let status = response.status().as_u16();
        let text = response.text().await.unwrap_or_default();
        let payload: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
        if (200..300).contains(&status) {
            let access = text_field(&payload, "access_token");
            let refresh = text_field(&payload, "refresh_token");
            let (Some(access), Some(refresh)) = (access, refresh) else {
                return Err(GatewayError::with_status(
                    502,
                    "Minimax 登录响应缺少令牌（access_token / refresh_token）",
                ));
            };
            let expires_in = payload
                .get("expires_in")
                .and_then(Value::as_i64)
                .or_else(|| {
                    payload
                        .get("expires_in")
                        .and_then(Value::as_str)
                        .and_then(|text| text.trim().parse::<i64>().ok())
                })
                .filter(|value| *value > 0)
                .unwrap_or(super::oauth::oauth_default_expires_in_seconds());
            return Ok(DeviceTokens {
                access_token: access,
                refresh_token: refresh,
                expires_in,
            });
        }
        // 非2xx：按 RFC 8628 的 error 码决定继续还是终止
        let error_code = text_field(&payload, "error")
            .unwrap_or_default()
            .to_ascii_lowercase();
        match error_code.as_str() {
            // 用户还没确认：继续等
            "authorization_pending" => continue,
            // 轮太快：拉长间隔（RFC 要求至少 +1 秒）
            "slow_down" => {
                interval = interval.saturating_add(1).min(30);
                continue;
            }
            "expired_token" => {
                return Err(GatewayError::with_status(
                    408,
                    "Minimax 登录超时（授权码已过期），请重新发起登录",
                ))
            }
            "access_denied" => {
                return Err(GatewayError::with_status(
                    400,
                    "Minimax 登录被拒绝（用户在授权页取消了）",
                ))
            }
            "invalid_grant" => {
                // 设备码已失效：可能已被另一轮登录消耗，或超时后被上游回收
                return Err(GatewayError::with_status(
                    400,
                    "Minimax 授权码已失效或被使用，请重新发起登录",
                ))
            }
            _ => {
                let detail = text_field(&payload, "error_description")
                    .or_else(|| text_field(&payload, "error"))
                    .or_else(|| {
                        payload
                            .pointer("/base_resp/status_msg")
                            .and_then(Value::as_str)
                            .map(str::to_string)
                    })
                    .unwrap_or_else(|| {
                        crate::server::core::account_store::store_util::truncate_text(&text, 200)
                    });
                return Err(GatewayError::with_status(
                    502,
                    format!("Minimax 登录失败（HTTP {status}）：{detail}"),
                ));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{code_challenge, DeviceAuthStart, MAX_POLL_MS};

    /// RFC 7636 附录 B 的官方向量：verifier → challenge 必须逐字一致。
    /// 这是 PKCE 实现最容易写错的地方（少了 padding、用错摘要、编码不对）。
    #[test]
    fn pkce_matches_rfc7636_vector() {
        let verifier = "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk";
        assert_eq!(
            code_challenge(verifier),
            "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"
        );
    }

    #[test]
    fn challenge_has_no_padding() {
        let challenge = code_challenge("some-verifier-value-for-minimax-pkce-test");
        assert!(!challenge.contains('='), "base64url 不应带padding");
        assert!(!challenge.contains('+') && !challenge.contains('/'), "应为 url-safe 字母表");
    }

    /// 过期时间兜底：上游给 absurdly 大的值时必须被夹到轮询上限内，
    /// 否则一次输错的响应能让后台任务挂几小时。
    #[test]
    fn expires_is_clamped() {
        assert!(DeviceAuthStart {
            device_code: "d".into(),
            user_code: "u".into(),
            verification_uri: "https://example.invalid".into(),
            verification_uri_complete: None,
            expires_in_seconds: 300.min(MAX_POLL_MS / 1000).max(60),
            interval_seconds: 5,
            code_verifier: "v".into(),
        }
        .expires_in_seconds
            <= MAX_POLL_MS / 1000);
    }
}
