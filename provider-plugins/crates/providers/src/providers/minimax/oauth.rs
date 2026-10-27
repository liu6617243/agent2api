//! MiniMax 凭证与 OAuth：设备码登录（PKCE S256）+ refresh_token 续期。
//!
//! ── 凭证形态 ────────────────────────────────────────────────
//! `refresh_token` 是**主凭证**（一次性轮换），`access_token` 是短期缓存：
//!   - 刷新成功 → 新 access_token + 新 refresh_token **一起**回写（缺一必坏）；
//!   - 丢了 refresh_token 只能重新走设备码授权。
//!
//! ── 设备码授权（RFC 8628 + PKCE S256）──────────────────────
//! ```text
//! ① POST {account}/oauth2/device/code   （form）
//!      client_id / scope / audience / code_challenge / code_challenge_method=S256
//!    → {device_code, user_code, verification_uri[_complete], expires_in, interval}
//! ② 用户打开 verification_uri_complete 完成授权
//! ③ POST {account}/oauth2/token         （form，每 interval 秒轮询一次）
//!      grant_type=urn:ietf:params:oauth:grant-type:device_code
//!      device_code / client_id / code_verifier
//!    → 200 {access_token, refresh_token, expires_in}
//!    → 4xx authorization_pending（继续）/ slow_down（拉长间隔）
//!        / expired_token / access_denied / invalid_grant（终止）
//! ```
//! `code_verifier` 只活这一轮、**绝不落盘**（它等价于密码）；日志绝不打印
//! device_code / verifier / 令牌本体。
//!
//! ── 登录在插件协议里的形态：进程内 flow 表 ─────────────────
//! `login.start` 与 `login.poll` 是两次独立 RPC，只靠一个 `flow` 字符串关联，
//! 而 verifier / device_code 必须留在后端。插件是单进程、且授权在几分钟内
//! 完成（网关侧超时同档），因此把待办流程放进程内表（与 qoder 的设备授权
//! 同一手法）。进程真重启了那次登录本来也就失败了，用户重点一次即可。
//!
//! ── 续期：单飞 + 比较再写 ──────────────────────────────────
//! 一次性轮换让「单飞」很关键：两个并发刷新各拿旧 rt 去打，第二个必
//! invalid_grant，还会把第一个刚写回的新 rt 判定搅乱。回写用通用
//! `store::save(expect, next)`——期间记录被改过（重新粘贴/重新登录）时
//! 不覆盖，重读一次以记录为准。
//!
//! ── 硬约束 ──────────────────────────────────────────────────
//! release 是 `panic=abort`：本文件零 unwrap/expect/panic。

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

use agent2api_provider_protocol as proto;
use agent2api_provider_sdk::PluginCtx;

use crate::errors::GatewayError;
use crate::flight::{self, Join, Table};
use crate::ids;
use crate::store;

use super::endpoints;
use super::ID;

/// token 端点请求超时（与别家 30 秒同档）
const REQUEST_TIMEOUT_MS: u64 = 30_000;

/// 上游没给 `expires_in` 时的保守默认（MiniMax 实测 3600）
const DEFAULT_EXPIRES_IN_SECONDS: i64 = 3600;

/// 刷新提前量（毫秒）：到期前 15 分钟即视为临期
const REFRESH_SKEW_MS: i64 = 900 * 1000;

/// 设备码授权的最长等待（秒）：上游给的 `expires_in` 被夹在 [60, 300] 内
const MAX_POLL_SECONDS: i64 = 300;

/// 默认轮询间隔（上游返回 `interval`，缺失时用它）
const DEFAULT_INTERVAL_SECONDS: u64 = 5;

/// 上游错误文在报错里的截断长度
const MAX_ERROR_CHARS: usize = 300;

/// 一次 token 端点调用的结果
#[derive(Clone, Debug)]
pub struct TokenResponse {
    pub access_token: String,
    /// 新的 refresh_token（本家**每次都会下发**；None 是异常形态）
    pub refresh_token: Option<String>,
    pub expires_in: i64,
    pub token_type: String,
}

/// 一次转发 / 签到 / 刷新用的凭证快照。
#[derive(Clone, Debug)]
pub struct Credentials {
    /// 账号记录 id
    pub id: String,
    /// 主凭证（**一次性**；刷新后必须回写新值）
    pub refresh_token: String,
    /// 短期访问令牌（可能为空 —— 需要先刷新）
    pub access_token: String,
    /// 访问令牌过期时刻（毫秒；0 = 判不出来）
    pub expires_at: i64,
    /// 稳定身份（首次落账号时生成，之后**只读**）
    ///
    /// 为什么不从令牌派生：本家的 refresh_token 每次刷新都会换一枚，
    /// 从它算哈希会让「同一个账号」在每次续期后变成另一个身份。
    pub identity: String,
}

impl Credentials {
    fn can_refresh(&self) -> bool {
        !self.refresh_token.trim().is_empty()
    }

    fn has_access_token(&self) -> bool {
        !self.access_token.trim().is_empty()
    }

    /// 该账号此刻是否**已过期或进入提前量窗口**。
    ///
    /// 判不出来（`expires_at <= 0`）→ false：宁可漏刷，也不要凭猜测打上游；
    /// 没有 access_token → true（必须先刷一次）。
    pub(super) fn expiring(&self) -> bool {
        if !self.has_access_token() {
            return true;
        }
        if self.expires_at <= 0 {
            return false;
        }
        self.expires_at - REFRESH_SKEW_MS <= crate::http::now_ms()
    }

    /// 落盘形态（`secret`）。
    pub(super) fn to_secret(&self) -> Value {
        json!({
            "refreshToken": self.refresh_token,
            "accessToken": self.access_token,
            "expiresAt": self.expires_at,
            "identity": self.identity,
        })
    }

    /// 公开形态（账号页展示；token 只给尾号）。
    pub(super) fn to_public(&self, name: &str) -> Value {
        json!({
            "name": if name.trim().is_empty() {
                format!("MiniMax {}", store::token_tail(&self.refresh_token))
            } else {
                name.to_string()
            },
            "tokenTail": store::token_tail(&self.refresh_token),
            "expiresAt": self.expires_at,
        })
    }
}

/// 从 secret 解析凭证（宽容读取：`refreshToken`/`refresh_token` 两种写法都认）。
fn credentials_of(secret: &Value) -> Result<Credentials, GatewayError> {
    let pick = |keys: &[&str]| store::pick(secret, keys);
    let refresh_token = pick(&["refreshToken", "refresh_token"]);
    let access_token = pick(&["accessToken", "access_token"]);
    let expires_at = secret
        .get("expiresAt")
        .and_then(Value::as_i64)
        .or_else(|| {
            secret
                .get("expiresAt")
                .and_then(Value::as_str)
                .and_then(|text| text.trim().parse::<i64>().ok())
        })
        .unwrap_or(0);
    if refresh_token.is_empty() && access_token.is_empty() {
        return Err(GatewayError::with_status(
            401,
            "MiniMax 账号没有可用凭证：请在账号页重新添加（网页登录或粘贴 refreshToken）",
        ));
    }
    let identity = pick(&["identity"]);
    Ok(Credentials {
        id: String::new(),
        refresh_token,
        access_token,
        expires_at,
        identity,
    })
}

/// 读账号记录 + 凭证。
pub async fn snapshot(ctx: &PluginCtx, account: &str) -> Result<(Value, Credentials), GatewayError> {
    let record = ctx
        .store_get(ID, account)
        .await
        .map_err(|error| match error {
            agent2api_provider_sdk::StoreError::NotFound => GatewayError::with_status(
                401,
                "MiniMax 没有这个账号：请在账号页添加（网页登录或粘贴 refreshToken）",
            ),
            other => GatewayError::with_status(500, format!("读取 MiniMax 账号失败：{other}")),
        })?;
    let mut credentials = credentials_of(&record.secret)?;
    credentials.id = record.id.clone();
    // `record_of` 已把 id / provider / name 提到顶层，并把 secret 铺平 ——
    // 下游（签到）按顶层键读 `proxy`（网关注入的账号代理，没有则直连）。
    let value = store::record_of(&record);
    Ok((value, credentials))
}

// ─── 续期（单飞 + 比较再写）──────────────────────────────────

/// 单飞表（键 = provider + 账号 + refresh_token 指纹）
static FLIGHTS: OnceLock<Table<Credentials>> = OnceLock::new();

/// 取可用凭证（`force = true` 时不看临期窗口，401 之后强制刷一次）。
pub async fn ensure_fresh(
    ctx: &PluginCtx,
    account: &str,
    force: bool,
) -> Result<Credentials, GatewayError> {
    let (record, credentials) = snapshot(ctx, account).await?;
    if !force && !credentials.expiring() {
        return Ok(credentials);
    }
    if !credentials.can_refresh() {
        return Err(GatewayError::with_status(
            401,
            "MiniMax 账号缺少 refresh token，无法续期：请重新完成网页登录后添加账号",
        ));
    }
    let key = format!(
        "{}:{}:{}",
        ID,
        account,
        flight::fingerprint(&credentials.refresh_token)
    );
    match FLIGHTS.get_or_init(Table::new).join(&key) {
        Join::Waiter(waiter) => waiter.wait().await,
        Join::Leader(leader) => {
            let result = refresh_and_save(ctx, &record, &credentials).await;
            leader.finish(result.clone());
            result
        }
    }
}

/// 打一次刷新 + 回写（单飞的 leader 走这一段）。
async fn refresh_and_save(
    ctx: &PluginCtx,
    record: &Value,
    credentials: &Credentials,
) -> Result<Credentials, GatewayError> {
    let proxy = record.get("proxy").cloned();
    let wire = proxy
        .as_ref()
        .and_then(|value| serde_json::from_value::<proto::WireProxy>(value.clone()).ok());
    let response = refresh_access_token(&credentials.refresh_token, wire.as_ref()).await?;
    let mut fresh = credentials.clone();
    fresh.access_token = response.access_token.trim().to_string();
    if let Some(refresh) = response.refresh_token.as_ref() {
        fresh.refresh_token = refresh.trim().to_string();
    } else {
        // 异常形态：上游没回新 rt（一次性轮换的契约被破坏）。保留旧值继续用，
        // 并如实记一句 —— 下一次刷新大概率 401，那时文案会指向重新授权。
        crate::logging::log(
            "[MiniMax]",
            "⚠️ 刷新响应未携带新的 refresh_token（该家正常情况下必发），保留旧值继续",
        );
    }
    let expires_in = if (1..=7 * 24 * 3600).contains(&response.expires_in) {
        response.expires_in
    } else {
        DEFAULT_EXPIRES_IN_SECONDS
    };
    fresh.expires_at = crate::http::now_ms() + expires_in * 1000;
    crate::logging::verbose(
        "[MiniMax]",
        &format!(
            "token 刷新成功（token_type {}，有效期 {expires_in} 秒）",
            response.token_type
        ),
    );
    let expect = store::secret_view(record);
    let next = store::with_fields(
        &expect,
        &[
            ("refreshToken", json!(fresh.refresh_token)),
            ("accessToken", json!(fresh.access_token)),
            ("expiresAt", json!(fresh.expires_at)),
        ],
    );
    match crate::store::save(ctx, &credentials.id, &expect, &next).await {
        Ok(true) => Ok(fresh),
        Ok(false) => {
            // 记录在刷新期间被换过（重新粘贴 / 重新登录）：本次用刷出的新令牌，
            // 但不覆盖记录（与网关内核版的 Stale 处置一致）。
            crate::logging::log(
                "[MiniMax]",
                "⚠️ 刷新结果未能写回账号记录（期间被改动过），本次请求用新令牌，下次以记录里的为准",
            );
            Ok(fresh)
        }
        Err(error) => Err(error),
    }
}

// ─── token 端点 ─────────────────────────────────────────────

/// 截断上游错误文（按字符，UTF-8 安全）
fn truncate(text: &str) -> String {
    let collapsed: String = text
        .chars()
        .filter(|character| !character.is_control())
        .collect();
    match collapsed.char_indices().nth(MAX_ERROR_CHARS) {
        Some((index, _)) => format!("{}…", &collapsed[..index]),
        None => collapsed,
    }
}

/// 解析 token 读数（`expires_in` 接受数字与数字字符串两种形态）
fn expires_in_of(payload: &Value) -> i64 {
    match payload.get("expires_in") {
        Some(Value::Number(number)) => number.as_i64().unwrap_or(0),
        Some(Value::String(text)) => text.trim().parse::<i64>().unwrap_or(0),
        _ => 0,
    }
}

/// 上游错误文案（`error_description` / `error` / `base_resp.status_msg` / `message`）
fn error_detail(payload: &Value, fallback: &str) -> String {
    payload
        .get("error_description")
        .and_then(Value::as_str)
        .or_else(|| payload.get("error").and_then(Value::as_str))
        .or_else(|| {
            payload
                .pointer("/base_resp/status_msg")
                .and_then(Value::as_str)
        })
        .or_else(|| payload.get("message").and_then(Value::as_str))
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(truncate)
        .unwrap_or_else(|| truncate(fallback))
}

/// 打一次 token 端点（form 表单；纯网络 + 解析，不落盘）。
pub async fn post_token_form(
    fields: &[(&str, &str)],
    proxy: Option<&proto::WireProxy>,
) -> Result<(u16, Value, String), GatewayError> {
    let client = crate::http::client(proxy, "direct");
    let response = client
        .post(endpoints::token_url())
        .timeout(Duration::from_millis(REQUEST_TIMEOUT_MS))
        .header("Content-Type", "application/x-www-form-urlencoded")
        .header("Accept", "application/json")
        .form(fields)
        .send()
        .await
        .map_err(|error| {
            if error.is_timeout() {
                GatewayError::with_status(504, "MiniMax 授权请求超时，请稍后重试")
            } else {
                GatewayError::with_status(
                    502,
                    format!("MiniMax 授权请求失败：{}", crate::http::describe(&error)),
                )
            }
        })?;
    let status = response.status().as_u16();
    let text = response.text().await.unwrap_or_default();
    let payload: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
    Ok((status, payload, text))
}

/// 刷新 access_token（`grant_type=refresh_token`）。
pub async fn refresh_access_token(
    refresh_token: &str,
    proxy: Option<&proto::WireProxy>,
) -> Result<TokenResponse, GatewayError> {
    let refresh_token = refresh_token.trim();
    if refresh_token.is_empty() {
        return Err(GatewayError::with_status(
            401,
            "MiniMax 账号缺少 refresh token，无法续期，请重新登录后添加账号",
        ));
    }
    let (status, payload, text) = post_token_form(
        &[
            ("grant_type", endpoints::GRANT_TYPE_REFRESH),
            ("refresh_token", refresh_token),
            ("client_id", endpoints::CLIENT_ID),
            ("scope", endpoints::SCOPE),
            ("audience", endpoints::AUDIENCE),
        ],
        proxy,
    )
    .await?;
    if !(200..300).contains(&status) {
        let detail = error_detail(&payload, &text);
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
                     被消耗或撤销后就再也用不了{hint}；请重新完成网页登录后添加账号"
                ),
            )
        } else {
            GatewayError::with_status(
                502,
                format!("MiniMax 刷新 token 失败（HTTP {status}）{hint}"),
            )
        };
        crate::logging::log(
            "[MiniMax]",
            &format!("❌ 刷新 token 失败（HTTP {status}）：{}", error.message),
        );
        return Err(error);
    }
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
                 请重新完成网页登录后添加账号",
            )
        })?;
    let refresh = payload
        .get("refresh_token")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|text| !text.is_empty())
        .map(str::to_string);
    let expires_in = expires_in_of(&payload);
    let token_type = payload
        .get("token_type")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|text| !text.is_empty())
        .unwrap_or("Bearer")
        .to_string();
    Ok(TokenResponse {
        access_token,
        refresh_token: refresh,
        expires_in,
        token_type,
    })
}

// ─── 设备码登录 ─────────────────────────────────────────────

/// 一次进行中的设备授权。
struct DeviceLogin {
    device_code: String,
    code_verifier: String,
    /// 轮询间隔（秒）；上游喊 slow_down 时加一
    interval: u64,
    /// 截止时刻（毫秒）
    deadline_ms: i64,
}

/// 进程内的设备授权表（`flow` → 授权上下文）。
fn flows() -> &'static Mutex<HashMap<String, DeviceLogin>> {
    static FLOWS: OnceLock<Mutex<HashMap<String, DeviceLogin>>> = OnceLock::new();
    FLOWS.get_or_init(|| Mutex::new(HashMap::new()))
}

/// 把锁中毒当成空表处理（授权表丢一条只影响那一次登录）。
fn with_flows<T>(handler: impl FnOnce(&mut HashMap<String, DeviceLogin>) -> T) -> T {
    match flows().lock() {
        Ok(mut table) => handler(&mut table),
        Err(poisoned) => {
            let mut table = poisoned.into_inner();
            handler(&mut table)
        }
    }
}

/// PKCE：`BASE64URL(SHA256(verifier))`，无 padding。
fn code_challenge(verifier: &str) -> String {
    URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()))
}

/// 发起设备授权，返回 flow 与授权页地址。
pub async fn login_start() -> Result<proto::LoginStart, GatewayError> {
    let verifier = format!("{}{}", ids::random_id("mm"), ids::uuid_v4().replace('-', ""));
    let challenge = code_challenge(&verifier);
    let client = crate::http::client(None, "direct");
    let response = client
        .post(endpoints::device_code_url())
        .timeout(Duration::from_millis(REQUEST_TIMEOUT_MS))
        .header("Content-Type", "application/x-www-form-urlencoded")
        .header("Accept", "application/json")
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
                    "MiniMax 设备授权请求失败：{}",
                    crate::http::describe(&error)
                ),
            )
        })?;
    let status = response.status().as_u16();
    let text = response.text().await.unwrap_or_default();
    let payload: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
    if !(200..300).contains(&status) {
        return Err(GatewayError::with_status(
            502,
            format!(
                "MiniMax 设备授权失败（HTTP {status}）：{}",
                error_detail(&payload, &text)
            ),
        ));
    }
    let device_code = payload
        .get("device_code")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|text| !text.is_empty())
        .ok_or_else(|| GatewayError::with_status(502, "MiniMax 设备授权响应缺少 device_code"))?
        .to_string();
    let user_code = payload
        .get("user_code")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .to_string();
    let verification_uri = payload
        .get("verification_uri")
        .or_else(|| payload.get("verification_url"))
        .and_then(Value::as_str)
        .map(str::to_string)
        .unwrap_or_else(endpoints::authorize_url);
    let verification_uri_complete = payload
        .get("verification_uri_complete")
        .or_else(|| payload.get("verification_url_complete"))
        .and_then(Value::as_str)
        .map(str::to_string);
    let expires_in = payload
        .get("expires_in")
        .and_then(Value::as_i64)
        .unwrap_or(MAX_POLL_SECONDS)
        .clamp(60, MAX_POLL_SECONDS);
    let interval = payload
        .get("interval")
        .and_then(Value::as_u64)
        .unwrap_or(DEFAULT_INTERVAL_SECONDS)
        .clamp(1, 30);
    let flow = format!("minimax-device-{}", ids::uuid_v4());
    with_flows(|table| {
        table.insert(
            flow.clone(),
            DeviceLogin {
                device_code,
                code_verifier: verifier,
                interval,
                deadline_ms: crate::http::now_ms() + expires_in * 1000,
            },
        )
    });
    crate::logging::log(
        "[Login]",
        &format!("发起 MiniMax 网页登录（设备码，user_code={user_code}）"),
    );
    Ok(proto::LoginStart {
        flow,
        url: verification_uri_complete.unwrap_or(verification_uri),
        instructions: if user_code.is_empty() {
            "在弹出的窗口里完成 MiniMax 登录，完成后会自动收下账号".to_string()
        } else {
            format!("在弹出的窗口里完成 MiniMax 登录（确认码 {user_code}），完成后会自动收下账号")
        },
        poll_interval_ms: interval * 1000,
        // 走**内嵌弹窗**（面板里的登录窗口），不是系统浏览器：
        //   ① 与旧网关的 webLogin 默认档（内嵌窗口）一致，用户点一下就在原地登录，
        //      不用切到外部浏览器再切回来；
        //   ② 设备码授权页本身就是登录页，弹窗里能完成全部步骤；
        //   ③ 系统浏览器那条路复用不到已登录的会话反而更慢（同一台机器上两者 cookie
        //      不共享，用户在外部浏览器里常常还得重新登录一次）。
        external_browser: false,
        // 登录页会经过的域：授权页在 account.*，登录成功后可能跳到官网/控制台。
        // 给足子域，漏一个的表现是「窗口一片空白」（导航被白名单拦掉）。
        allowed_hosts: vec![
            "minimax.cn".to_string(),
            "account.minimax.cn".to_string(),
            "agent.minimax.cn".to_string(),
            "minimaxi.com".to_string(),
            "platform.minimaxi.com".to_string(),
            "www.minimaxi.com".to_string(),
        ],
    })
}

/// 轮询登录进度（网关按 `poll_interval_ms` 反复调用）。
pub async fn login_poll(flow: &str) -> Result<proto::LoginPoll, GatewayError> {
    let Some((device_code, verifier, deadline_ms)) = with_flows(|table| {
        table.get(flow).map(|entry| {
            (
                entry.device_code.clone(),
                entry.code_verifier.clone(),
                entry.deadline_ms,
            )
        })
    }) else {
        return Ok(proto::LoginPoll::Failed {
            message: "这次登录的上下文已过期（插件重启或超时），请重新发起网页登录".to_string(),
        });
    };
    if crate::http::now_ms() > deadline_ms {
        with_flows(|table| table.remove(flow));
        return Ok(proto::LoginPoll::Failed {
            message: "MiniMax 登录超时（授权码已过期），请重新发起登录".to_string(),
        });
    }
    let (status, payload, text) = post_token_form(
        &[
            ("grant_type", endpoints::GRANT_TYPE_DEVICE_CODE),
            ("device_code", device_code.as_str()),
            ("client_id", endpoints::CLIENT_ID),
            ("code_verifier", verifier.as_str()),
        ],
        None,
    )
    .await?;
    if (200..300).contains(&status) {
        let access_token = payload
            .get("access_token")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|text| !text.is_empty());
        let refresh_token = payload
            .get("refresh_token")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|text| !text.is_empty());
        let (Some(access_token), Some(refresh_token)) = (access_token, refresh_token) else {
            return Ok(proto::LoginPoll::Failed {
                message: "MiniMax 登录响应缺少令牌（access_token / refresh_token）".to_string(),
            });
        };
        let expires_in = {
            let value = expires_in_of(&payload);
            if value > 0 {
                value
            } else {
                DEFAULT_EXPIRES_IN_SECONDS
            }
        };
        with_flows(|table| table.remove(flow));
        let credentials = Credentials {
            id: String::new(),
            refresh_token: refresh_token.to_string(),
            access_token: access_token.to_string(),
            expires_at: crate::http::now_ms() + expires_in * 1000,
            // 稳定身份在**首次落账号**时生成：之后 refresh_token 会轮换，
            // 从它派生会让同一账号每次续期换一个身份。
            identity: format!("minimax-{}", ids::uuid_v4()),
        };
        crate::logging::log("[Login]", "✅ MiniMax 登录成功，账号已加入列表");
        return Ok(proto::LoginPoll::Done {
            account: proto::NormalizedAccount {
                secret: credentials.to_secret(),
                public: credentials.to_public(""),
                identity: credentials.identity.clone(),
                desktop: false,
            },
        });
    }
    // 非 2xx：按 RFC 8628 的 error 码决定继续还是终止
    let error_code = payload
        .get("error")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_ascii_lowercase();
    match error_code.as_str() {
        "authorization_pending" => Ok(proto::LoginPoll::Pending {
            hint: Some("等待你在授权页确认…".to_string()),
        }),
        "slow_down" => {
            with_flows(|table| {
                if let Some(entry) = table.get_mut(flow) {
                    entry.interval = (entry.interval + 1).min(30);
                }
            });
            Ok(proto::LoginPoll::Pending {
                hint: Some("上游要求放慢轮询…".to_string()),
            })
        }
        "expired_token" => {
            with_flows(|table| table.remove(flow));
            Ok(proto::LoginPoll::Failed {
                message: "MiniMax 登录超时（授权码已过期），请重新发起登录".to_string(),
            })
        }
        "access_denied" => {
            with_flows(|table| table.remove(flow));
            Ok(proto::LoginPoll::Failed {
                message: "MiniMax 登录被拒绝（在授权页取消了）".to_string(),
            })
        }
        "invalid_grant" => {
            with_flows(|table| table.remove(flow));
            Ok(proto::LoginPoll::Failed {
                message: "MiniMax 授权码已失效或被使用，请重新发起登录".to_string(),
            })
        }
        _ => {
            with_flows(|table| table.remove(flow));
            Ok(proto::LoginPoll::Failed {
                message: format!(
                    "MiniMax 登录失败（HTTP {status}）：{}",
                    error_detail(&payload, &text)
                ),
            })
        }
    }
}
