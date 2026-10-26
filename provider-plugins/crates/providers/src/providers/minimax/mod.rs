//! MiniMax（MiniMax Code / agent.minimax.cn）：**只接账号 + 签到**的 provider。
//!
//! ── 为什么只接这两块 ─────────────────────────────────────────
//! 这家最初由 fork 在网关内核里接入（转发 + 余额 + 签到全家）。上游 3.3.0
//! 把 provider 实现整体移出主仓之后，本仓只补回「账号 + 每日签到」——
//! 转发/余额的端点常量留在 `endpoints.rs` 备用，但能力位一律 false，
//! 网关不会把模型请求派过来（派过来也没有实现，只会拿到一条明确错误）。
//!
//! ── 能力位（申报错方向的后果见 `Capabilities` 各字段注释）──
//! ```text
//!   chat            false  没有转发实现（模型请求请走别的家 / 自定义提供商）
//!   stateful        false
//!   refresh         true   refresh_token 一次性轮换，后台维护可续
//!   model_refresh   false 没有远程模型目录
//!   usage           false 本插件不查余额
//!   checkin         true  每日签到（signin/status + signin/claim）
//!   claims          false
//!   web_login       true  PKCE 设备码授权（oauth.rs）
//!   env_credentials false
//! ```
//!
//! ── 账号形态 ────────────────────────────────────────────────
//! `secret` = `{refreshToken, accessToken, expiresAt, identity}`：
//!   - `refreshToken` 是主凭证（一次性轮换，丢了只能重新登录）；
//!   - `identity` 在**首次落账号**时生成、之后只读 —— 不能从令牌派生，
//!     因为 refresh_token 每刷一次就换一枚（派生会让同一账号每次续期后变身份）。
//!
//! ── 登录方式（两条并存）─────────────────────────────────────
//!   1. `Browser`  设备码授权（新用户；扫码/打开链接确认一次，之后自动续期）；
//!   2. `Token`    粘贴 refreshToken（老用户沿用旧版网关里存的凭证，免重新授权）。
//!
//! ── 硬约束 ──────────────────────────────────────────────────
//! release 是 `panic=abort`：本文件零 unwrap/expect/panic。

pub mod checkin;
pub mod endpoints;
pub mod oauth;

use agent2api_provider_protocol as proto;
use agent2api_provider_sdk::{PluginCtx, Provider, ProviderMeta, SendFuture};
use serde_json::Value;

/// provider id（`pack.json` 里必须写同一个）。
///
/// 用 `minimax-cn` 而不是 `minimax`：与主仓预置卡里「国内站」那张卡的
/// key 同名，用户在账号页 / 签到中心看到的名字和预置卡是同一套叫法。
pub const ID: &str = "minimax-cn";

pub struct MiniMaxProvider;

impl Provider for MiniMaxProvider {
    fn meta(&self) -> ProviderMeta {
        ProviderMeta::new(ID, "minimax-cn")
            .with_capabilities(proto::Capabilities {
                // 本插件没有转发实现 —— 必须显式 false（这一位的 serde 缺省是 true）
                chat: false,
                stateful: false,
                // 设备码登录拿到的 refresh_token 可续期，后台凭证维护可以管它
                refresh: true,
                model_refresh: false,
                // 不查余额（端点常量留着备用）
                usage: false,
                checkin: true,
                claims: false,
                // PKCE 设备码授权（oauth.rs）+ 粘贴 refreshToken
                web_login: true,
                sse_model_rewrite: false,
                sse_strip_newline_chunks: false,
                anonymous_default_session: false,
                system_proxy_when_unset: false,
                refresh_uses_account: true,
                supports_default_model: false,
                env_credentials: false,
            })
            .with_login(vec![
                proto::LoginMethod::Token {
                    label: "refreshToken".to_string(),
                    placeholder: "粘贴 MiniMax 的 refreshToken（旧版网关里存的那枚，可免重新授权）"
                        .to_string(),
                    extra_fields: Vec::new(),
                },
                proto::LoginMethod::Browser {
                    button: "网页登录".to_string(),
                    hint: Some(
                        "在弹出的窗口里完成 MiniMax 登录（设备码），完成后自动收下账号并支持自动续期"
                            .to_string(),
                    ),
                },
            ])
            // 没有模型目录：清单交空，界面不会给「获取模型」入口
            .with_models(Vec::new())
            .with_checkin(proto::CheckinInfo {
                description: Some(
                    "MiniMax 每日签到（signin/status → signin/claim，Bearer 登录态）".to_string(),
                ),
                modes: vec![proto::CheckinMode::Checkin],
                // 显式写 true，不靠 `Default`：protocol 0.2.1 的 `derive(Default)`
                // 把 `daily` 建成 false（与 serde 缺省相反），照 Default 走的话
                // 这家签到了也不会出现在签到中心里。0.2.2 手写了 Default 修掉，
                // 这里再显式一遍，两个版本的协议都能工作。
                daily: true,
                ..Default::default()
            })
    }

    // ── 签到 ──────────────────────────────────────────────────

    fn checkin<'a>(
        &'a self,
        ctx: &'a PluginCtx,
        account: &'a str,
    ) -> SendFuture<'a, proto::CheckinReport> {
        Box::pin(async move {
            match checkin::run(ctx, account).await {
                Ok(report) => Ok(report),
                // 登录态被服务端提前作废时（本地 expiresAt 看不出来）强制刷一次
                // 再打一发 —— 与网关内核版「401 → 强制续期 → 重放」同口径。
                Err(error) if error.status_code == 401 => {
                    oauth::ensure_fresh(ctx, account, true)
                        .await
                        .map_err(|error| error.message)?;
                    checkin::run(ctx, account).await.map_err(|error| error.message)
                }
                Err(error) => Err(error.message),
            }
        })
    }

    // ── 凭证 ──────────────────────────────────────────────────

    fn ensure_token<'a>(&'a self, ctx: &'a PluginCtx, account: &'a str) -> SendFuture<'a, String> {
        Box::pin(async move {
            let credentials = oauth::ensure_fresh(ctx, account, false)
                .await
                .map_err(|error| error.message)?;
            Ok(credentials.access_token)
        })
    }

    fn refresh_token<'a>(
        &'a self,
        ctx: &'a PluginCtx,
        account: &'a str,
    ) -> SendFuture<'a, String> {
        Box::pin(async move {
            let credentials = oauth::ensure_fresh(ctx, account, true)
                .await
                .map_err(|error| error.message)?;
            Ok(credentials.access_token)
        })
    }

    fn token_expiring<'a>(
        &'a self,
        ctx: &'a PluginCtx,
        account: &'a str,
    ) -> SendFuture<'a, bool> {
        Box::pin(async move {
            // 判不出来就 false：宁可漏刷，也不要凭猜测打上游
            let Ok((_, credentials)) = oauth::snapshot(ctx, account).await else {
                return Ok(false);
            };
            Ok(credentials.expiring())
        })
    }

    // ── 账号 ──────────────────────────────────────────────────

    fn normalize_account<'a>(
        &'a self,
        _ctx: &'a PluginCtx,
        input: proto::NormalizeInput,
    ) -> SendFuture<'a, proto::NormalizedAccount> {
        Box::pin(async move {
            let (token, name) = match &input.source {
                proto::NormalizeSource::Manual { payload } => {
                    // 三条键名都认：`refreshToken`（本项目落盘名）/ `refresh_token` / `token`
                    let token = ["refreshToken", "refresh_token", "token"]
                        .iter()
                        .find_map(|key| payload.get(*key).and_then(Value::as_str))
                        .unwrap_or("")
                        .trim()
                        .to_string();
                    let name = payload
                        .get("name")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_string();
                    (token, name)
                }
                proto::NormalizeSource::FileText { content, .. } => {
                    let parsed: Value = serde_json::from_str(content)
                        .map_err(|error| format!("文件不是合法 JSON：{error}"))?;
                    let token = ["refreshToken", "refresh_token", "token"]
                        .iter()
                        .find_map(|key| parsed.get(*key).and_then(Value::as_str))
                        .unwrap_or("")
                        .trim()
                        .to_string();
                    let name = parsed
                        .get("name")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_string();
                    (token, name)
                }
                proto::NormalizeSource::FileBase64 { .. } => {
                    return Err("MiniMax 不支持二进制文件导入".to_string())
                }
                proto::NormalizeSource::Desktop => {
                    return Err("MiniMax 不支持从本机客户端导入，请用网页登录或粘贴 refreshToken"
                        .to_string())
                }
            };
            if token.is_empty() {
                return Err("refreshToken 不能为空（网页登录会自动获得；旧版网关的账号里也有）"
                    .to_string());
            }
            let expires_at = crate::http::now_ms();
            let credentials = oauth::Credentials {
                id: String::new(),
                refresh_token: token,
                // 粘贴路径只拿到主凭证：第一次取用会顺着 refresh 流程换 access_token
                access_token: String::new(),
                expires_at,
                identity: format!("minimax-{}", crate::ids::uuid_v4()),
            };
            Ok(proto::NormalizedAccount {
                secret: credentials.to_secret(),
                public: credentials.to_public(&name),
                identity: credentials.identity.clone(),
                desktop: false,
            })
        })
    }

    fn account_identity(&self, secret: &Value) -> Result<String, String> {
        // 正常路径：首次落账号时写进去的那枚（只读）
        let stored = secret
            .get("identity")
            .and_then(Value::as_str)
            .map(str::trim)
            .unwrap_or("");
        if !stored.is_empty() {
            return Ok(stored.to_string());
        }
        // 兜底（旧记录 / 手工构造）：用 refresh_token 指纹 —— 注意它在每次
        // 续期后都会变，所以这条只保证「同一条记录在续期前稳定」。
        let refresh = secret
            .get("refreshToken")
            .or_else(|| secret.get("refresh_token"))
            .and_then(Value::as_str)
            .map(str::trim)
            .unwrap_or("");
        if refresh.is_empty() {
            return Err("MiniMax 账号缺少 refreshToken".to_string());
        }
        Ok(format!(
            "minimax-{}",
            crate::flight::fingerprint(refresh)
        ))
    }

    // ── 登录 ──────────────────────────────────────────────────

    fn login_start<'a>(
        &'a self,
        _ctx: &'a PluginCtx,
        method: &'a str,
        _payload: &'a Value,
    ) -> SendFuture<'a, proto::LoginStart> {
        Box::pin(async move {
            match method {
                // 设备码授权：网关打开授权页，按 poll_interval_ms 轮询 login.poll
                "browser" | "" => oauth::login_start().await.map_err(|error| error.message),
                other => Err(format!(
                    "MiniMax 不支持这种登录方式（{other}）：粘贴令牌请用「添加账号」"
                )),
            }
        })
    }

    fn login_poll<'a>(
        &'a self,
        _ctx: &'a PluginCtx,
        flow: &'a str,
    ) -> SendFuture<'a, proto::LoginPoll> {
        Box::pin(async move { oauth::login_poll(flow).await.map_err(|error| error.message) })
    }
}
