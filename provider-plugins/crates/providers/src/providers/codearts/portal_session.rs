//! 网页会话 cookie 的**静默重建**：拿华为云 SSO 的 TGC 现场铸一条新的 portal 会话。
//!
//! ── 为什么需要这条通道（2026-11-09 抓包 + 命令行实测）──────────
//! `credits.rs` 读的 `devclouddevuibjJ_SESSION_ID` 与 `devclouddevuibjtcftk` 两个
//! cookie 都是**会话级**的：门户下发它们的 `Set-Cookie` 里既没有 `Max-Age` 也没有
//! `Expires`，浏览器一关就被丢掉。而它们还**严格配对** —— 实测拿 A 会话的 `cftk`
//! 配 B 会话的 session 打 `credit/page`，上游回 `400 {"error": "csrf validate fail."}`。
//!
//! 于是「留一份旧 cookie 备用」这条路不通：必须**同一时刻**铸出两个值。
//!
//! 但 SSO 那一层是持久的：`auth.huaweicloud.com` 的 `SSOTGC`（Ticket-Granting
//! Cookie，`Path=/authui/`）在重启后依然有效，浏览器就是靠它静默重登。整条链是：
//!
//! ```text
//!   GET auth.huaweicloud.com/authui/login?service=<门户页>   Cookie: SSOTGC=<TGC>
//!     → 302 Location: <门户页>?ticket=ST-…
//!   GET <门户页>?ticket=ST-…                                  （不带 cookie）
//!     → 302 Set-Cookie: devclouddevuibjJ_SESSION_ID=…        （HttpOnly，会话级）
//!   GET <门户页>                                             Cookie: SESSION_ID
//!     → 200 Set-Cookie: devclouddevuibjtcftk=…               （cftk 只在门户页下发）
//! ```
//!
//! 这条链在命令行上可以完整重放（实测同一 TGC 连跑 6 次都成功），所以不必让用户
//! 每次重启后手动去浏览器复制一遍。
//!
//! ── 几个只有试过才知道的细节 ────────────────────────────────
//!   1. **同一个 ticket 可以反复换 session**（`ST-8498043-…` 连用 6 次都 302），
//!      所以真正的一次性凭据只有 TGC 本身。重放失败只可能是 TGC 死了。
//!   2. **`Set-Cookie` 可能一次给两条 `SESSION_ID`**，浏览器按「后者覆盖前者」处理；
//!      实测只有**最后一条**能换到 cftk（取第一条会得到门户登录页）。
//!      所以这里一律取最后一条，与浏览器一致。
//!   3. **不能跟随重定向**：第 1 跳的 `Location` 就是 ticket 本身，跟随过去会把它
//!      消费掉而且看不到。客户端必须 `redirect::Policy::none()`。
//!
//! ── 硬约束 ──────────────────────────────────────────────────
//! release 是 `panic=abort`：本文件零 unwrap / expect / panic。任何一步失败都只返回
//! `None`，让余额查询退回「只有聚合值」的形态（与没贴 cookie 时完全一样）。

use agent2api_provider_protocol::WireProxy as ResolvedProxy;
use agent2api_provider_sdk::PluginCtx;
use serde_json::Value;

use super::credentials::{self, Credential};

/// SSO 主站（TGC 所在域）。
const AUTH_ORIGIN: &str = "https://auth.huaweicloud.com";
/// 换票入口。
const LOGIN_PATH: &str = "/authui/login";

/// 用来换票的 `service` 参数：**门户设置页**（与 `credits.rs` 的 Referer 同一页）。
///
/// 上游对 `service` 不校验具体路径（实测换成 `/chat/home` 也能拿到 ticket），
/// 但用同一页能让服务端把会话绑到 portal BFF 能认的身份上，少一类「换到了
/// 但 BFF 不认」的排查。
const PORTAL_PAGE: &str = "https://codearts.huaweicloud.com/portal/settings/personal-usage";

/// 与官方控制台同一个 UA。
const UA: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 \
                  (KHTML, like Gecko) Chrome/132.0.0.0 Safari/537.36";

/// 一次重放的超时（三跳，每跳都不该慢过一次控制面请求）。
const TIMEOUT_SECS: u64 = 20;

/// 重放整条 SSO 链，产出一条能打 portal BFF 的 cookie 串。
///
/// 返回 `None` = 任何一步没成（TGC 失效 / 上游改版 / 网络失败）。**不返回错误**：
/// 这条通道是可选增强，失败时余额查询照旧走聚合值那一路。
pub async fn mint(tgc: &str, proxy: Option<&ResolvedProxy>) -> Option<String> {
    let tgc = tgc.trim();
    if tgc.is_empty() {
        return None;
    }
    let client = match strict_client(proxy) {
        Some(client) => client,
        None => return None,
    };

    // ── 第 1 跳：TGC → 302 + ticket ────────────────────────────
    let mut login = url::Url::parse(&format!("{AUTH_ORIGIN}{LOGIN_PATH}")).ok()?;
    login.query_pairs_mut().append_pair("service", PORTAL_PAGE);
    let ticket = match client
        .get(login)
        .header("Cookie", format!("SSOTGC={tgc}"))
        .header("Accept", "text/html,application/xhtml+xml,application/xml;q=0.9,*/*;q=0.8")
        .header("User-Agent", UA)
        .send()
        .await
    {
        Ok(response) => {
            let status = response.status().as_u16();
            let location = response
                .headers()
                .get(reqwest::header::LOCATION)
                .and_then(|value| value.to_str().ok())
                .unwrap_or("")
                .to_string();
            if !(300..400).contains(&status) || location.is_empty() {
                verbose(&format!(
                    "SSO 换票未拿到重定向（HTTP {status}）：TGC 可能已失效，需要在浏览器里重新登录后重取"
                ));
                return None;
            }
            match ticket_of(&location) {
                Some(ticket) => ticket,
                None => {
                    verbose("SSO 换票的重定向里没有 ticket 参数");
                    return None;
                }
            }
        }
        Err(error) => {
            verbose(&format!("SSO 换票请求失败：{}", crate::http::describe_chain(&error)));
            return None;
        }
    };

    // ── 第 2 跳：ticket → 门户会话 ─────────────────────────────
    let mut hop = url::Url::parse(PORTAL_PAGE).ok()?;
    hop.query_pairs_mut().append_pair("ticket", &ticket);
    let session = match client
        .get(hop)
        .header("Accept", "text/html,application/xhtml+xml,application/xml;q=0.9,*/*;q=0.8")
        .header("User-Agent", UA)
        .send()
        .await
    {
        Ok(response) => {
            let found = last_cookie(response.headers(), credentials::SESSION_COOKIE_KEY);
            drop(response);
            match found {
                Some(session) => session,
                None => {
                    verbose("ticket 换会话失败：响应里没有 devclouddevuibjJ_SESSION_ID");
                    return None;
                }
            }
        }
        Err(error) => {
            verbose(&format!("ticket 换会话请求失败：{}", crate::http::describe_chain(&error)));
            return None;
        }
    };

    // ── 第 3 跳：拉门户页 → cftk ───────────────────────────────
    let cftk = match client
        .get(PORTAL_PAGE)
        .header(
            "Cookie",
            format!("{}={session}", credentials::SESSION_COOKIE_KEY),
        )
        .header("Accept", "text/html,application/xhtml+xml,application/xml;q=0.9,*/*;q=0.8")
        .header("User-Agent", UA)
        .send()
        .await
    {
        Ok(response) => {
            let found = last_cookie(response.headers(), credentials::CFTK_COOKIE);
            drop(response);
            match found {
                Some(cftk) => cftk,
                None => {
                    verbose("门户页没有下发 cftk：换到的会话可能不是本人（上游改版？）");
                    return None;
                }
            }
        }
        Err(error) => {
            verbose(&format!("门户页请求失败：{}", crate::http::describe_chain(&error)));
            return None;
        }
    };

    // 两个值必须同一次铸出来（见模块头），拼成单行 cookie 串。
    Some(
        format!(
            "{}={session}; {}={cftk}",
            credentials::SESSION_COOKIE_KEY,
            credentials::CFTK_COOKIE
        )
        .chars()
        .take(credentials::MAX_COOKIE_LENGTH)
        .collect(),
    )
}

/// 重建并**落盘**（同一账号的单飞，避免并发余额查询同时铸一串）。
///
/// 返回新 cookie（不管落盘成没成）：它已经能用了，本次请求不该因为写不回数据库
/// 而作废 —— 与 `refresh.rs` 处理「续期成功但写回失败」同一取向。
pub async fn renew(
    ctx: &PluginCtx,
    account_id: &str,
    credential: &Credential,
    proxy: Option<&ResolvedProxy>,
) -> Option<String> {
    let tgc = credential.sso_tgc.trim().to_string();
    if tgc.is_empty() {
        return None;
    }
    let key = format!("codearts-portal:{account_id}");
    // 闭包里做「铸 + 落盘」，等待者直接拿结果 —— 否则一次并发刷新会铸出 N 条
    // 服务端会话，白占上游资源。
    //
    // ⚠️ 两处形态要求（`flight::run` 的签名所致，不是风格问题）：
    //   1. `async move` + 先把捕获值 clone 出来：闭包被 `run` 消费掉，它返回的
    //      future 不能再借闭包环境（`FnOnce() -> Fut` 不接受借用环境的 future）；
    //   2. 闭包返回 `Result<_, String>`，而 `mint` 给的是 `Option` —— 必须显式
    //      match 转成 `Err`，不能 `?`（类型不兼容，且 `panic=abort` 下不许 unwrap）。
    let account = account_id.to_string();
    let ctx = ctx.clone();
    let minted = crate::flight::run(&key, || async move {
        let cookie = match mint(&tgc, proxy).await {
            Some(cookie) => cookie,
            None => return Err("SSO 票根换不到会话".to_string()),
        };
        persist(&ctx, &account, &cookie).await;
        Ok(cookie)
    })
    .await;
    match minted {
        Ok(cookie) if !cookie.is_empty() => {
            crate::logging::log("[CodeArts]", "网页会话 cookie 已用 SSO 票根自动重建");
            Some(cookie)
        }
        _ => None,
    }
}

/// 把新 cookie 写回账号 secret。
///
/// 与续期那条链同一纪律：**读最新记录、比较再写**（`store::save` 的 `expect` 语义），
/// 写不进去只记日志 —— cookie 本身是短命物，丢了下次还能再铸。
async fn persist(ctx: &PluginCtx, account_id: &str, cookie: &str) {
    let record = match crate::store::record(ctx, super::PROVIDER_ID, account_id).await {
        Ok(record) => record,
        Err(error) => {
            verbose(&format!("重建的 cookie 未能落盘（读账号失败）：{}", error.message));
            return;
        }
    };
    let expect = crate::store::secret_view(&record);
    let next = crate::store::with_fields(
        &expect,
        &[(
            credentials::SESSION_COOKIE_FIELD,
            Value::String(cookie.to_string()),
        )],
    );
    match crate::store::save(ctx, account_id, &expect, &next).await {
        Ok(true) => {}
        Ok(false) => verbose("重建的 cookie 未能落盘：账号在重建期间被改动"),
        Err(error) => verbose(&format!("重建的 cookie 未能落盘：{}", error.message)),
    }
}

/// 不跟随重定向的客户端（第 1 跳的 `Location` 就是票，跟随过去就看不见了）。
///
/// 代理口径与 `http::control_client` 一致：账号代理优先，没有就直连
/// （`no_proxy()` 显式关掉系统代理，见 `http::build` 的说明）。
fn strict_client(proxy: Option<&ResolvedProxy>) -> Option<reqwest::Client> {
    let mut builder = reqwest::Client::builder()
        .use_rustls_tls()
        .timeout(std::time::Duration::from_secs(TIMEOUT_SECS))
        .redirect(reqwest::redirect::Policy::none());
    builder = match proxy {
        Some(proxy) => match reqwest::Proxy::all(proxy.to_url()) {
            Ok(configured) => builder.proxy(configured),
            // 代理地址非法时降级直连（与 `http::build` 同一取向：不阻断链路）
            Err(_) => builder.no_proxy(),
        },
        None => builder.no_proxy(),
    };
    builder.build().ok()
}

/// `Set-Cookie` 里某个键的**最后一个**值（浏览器语义：后来者覆盖前者）。
fn last_cookie(headers: &reqwest::header::HeaderMap, name: &str) -> Option<String> {
    let mut found = None;
    for header in headers.get_all(reqwest::header::SET_COOKIE) {
        let Ok(text) = header.to_str() else { continue };
        let first = text.split(';').next().unwrap_or("").trim();
        let Some((key, value)) = first.split_once('=') else { continue };
        if key.trim() != name {
            continue;
        }
        let value = value.trim();
        if !value.is_empty() {
            found = Some(value.to_string());
        }
    }
    found
}

/// 从 `Location` 里取 `ticket` 查询参数。
fn ticket_of(location: &str) -> Option<String> {
    let url = url::Url::parse(location).ok()?;
    url.query_pairs()
        .find(|(key, _)| key == "ticket")
        .map(|(_, value)| value.into_owned())
        .filter(|value| !value.is_empty())
}

fn verbose(message: &str) {
    crate::logging::verbose("[CodeArts]", message);
}