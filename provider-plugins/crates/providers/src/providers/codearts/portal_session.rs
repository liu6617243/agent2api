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
//! 但 SSO 那一层**只比它长一点**：`auth.huaweicloud.com` 的 `SSOTGC`
//! （Ticket-Granting Cookie，`Path=/authui/`）在**浏览器关掉后也还在**（华为云把它
//! 持久化在自己的登录态里），所以「昨天登录过、今天打开浏览器还在」这个体验是真的。
//! ⚠️ 但它的 `Set-Cookie` 同样**没有 `Max-Age` / `Expires`** —— 按 RFC 6265 它是
//! **会话级** cookie，再叠上账号级「会话超时策略」（IAM → 安全设置 → 登录验证策略，
//! 默认闲置 1 小时、上限 24 小时），所以**它会在最后一次使用之后的一个窗口内死掉**，
//! 而不是永远活着。2026-11-11 实测：11-10 12:33 登录捡到的票根，11-11 13:15 就被
//! 服务端当场清掉（`Set-Cookie: SSOTGC=; Max-Age=0`）。
//!
//! 这条链因此是**可重放的，但票根必须跟着往前推**：换票这个动作本身就会重置
//! 服务端的闲置计时器（滑动过期），多数响应里还会再下发一枚新 `SSOTGC`。见
//! [`Minted`]、[`roll`] 与 [`persist`]。整条链是：
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

/// 一次重放的产物。
///
/// ── 为什么要连**新票根**一起带出来（2026-11-11 实测）──────────
/// `SSOTGC` **不是**长期票据：华为云对它下发的 `Set-Cookie` 是
/// `SSOTGC=…; Path=/authui/; secure; SameSite=None; HttpOnly`，**没有 `Max-Age`、
/// 也没有 `Expires`** —— 按 RFC 6265 这就是**会话级 cookie**（浏览器关掉即丢），
/// 服务端那侧还挂着一道账号级「会话超时策略」（默认 1 小时闲置、上限 24 小时）。
///
/// 于是「贴一次票根，之后永久自动重铸」这个假设是**错的**：实测 11-10 12:33 登录
/// 捡到的票根，到 11-11 13:15 就已经换不到 ticket 了（服务端直接回
/// `Set-Cookie: SSOTGC=; Max-Age=0; Expires=1970`，即当场清掉）。票根一死，
/// 整条网页通道静默降级成聚合值，有效期列就空了。
///
/// 唯一的活路是**滚动续期**，而且它有两条各自独立生效的路：
///   1. **使用即续**：服务端对票根做滑动过期 —— 闲置窗口从「最后一次使用」重新
///      计时。所以哪怕上游这次不回新票根，只要定期拿它换一次 ticket，服务端那侧
///      的计时器就一直被重置（这是浏览器里「一直用着就不会掉」的真正机制）；
///   2. **票根换代**：多数换票响应里会**再下发一枚新 `SSOTGC`**（值变了）。把它捡
///      起来写回账号，本地这枚就不会是签发当时那一枚。
///
/// 两条都做，票根窗口才会一直落在「最后一次查询 + 一个超时窗口」上，只要网关在跑、
/// 余额在查，有效期列就不会再无声消失。
///
/// 所以 `mint` 必须把新票根交出来（`Option`：没带也算正常，见第 1 条）。以前它只
/// 返回 cookie 串，换代那一半根本做不到 —— 这是本次「有效期又没了」的直接原因。
#[derive(Clone, Debug)]
pub struct Minted {
    /// 能打 portal BFF 的 cookie 串（`SESSION_ID=…; cftk=…`）。
    pub cookie: String,
    /// 上游这次下发的新票根；响应里没带就是 `None`（此时不该覆盖旧值）。
    pub tgc: Option<String>,
}

/// 重放整条 SSO 链，产出一条能打 portal BFF 的 cookie 串（外加新票根）。
///
/// 返回 `None` = 任何一步没成（TGC 失效 / 上游改版 / 网络失败）。**不返回错误**：
/// 这条通道是可选增强，失败时余额查询照旧走聚合值那一路。
pub async fn mint(tgc: &str, proxy: Option<&ResolvedProxy>) -> Option<Minted> {
    let tgc = tgc.trim();
    if tgc.is_empty() {
        return None;
    }
    let client = match strict_client(proxy) {
        Some(client) => client,
        None => return None,
    };

    // ── 第 1 跳：TGC → 302 + ticket，**并顺走这枚滚动票根** ────────
    let (ticket, renewed_tgc) = exchange(&client, tgc).await?;

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
    Some(Minted {
        cookie: format!(
            "{}={session}; {}={cftk}",
            credentials::SESSION_COOKIE_KEY,
            credentials::CFTK_COOKIE
        )
        .chars()
        .take(credentials::MAX_COOKIE_LENGTH)
        .collect(),
        tgc: renewed_tgc,
    })
}

/// **第 1 跳**：拿票根换 ticket，并顺手捡回上游下发的新票根。
///
/// ── 为什么单独抽出来 ────────────────────────────────────────
/// 它有两个用途，只有第一个需要后两跳：
///   1. `mint`（cookie 已死 → 真铸一条会话）：`exchange` → 换会话 → 取 cftk；
///   2. `roll`（cookie 还活着 → **只推票根**）：只需要这一跳，不该为它多打两次
///      门户请求、更不该把用户手贴的那条 cookie 换成机器铸的。
///
/// 返回值是 `(ticket, 新票根)`。新票根为 `None` 只说明这次响应没带 —— 不是失败。
///
/// ⚠️ 成败判据只用「有没有 302 + `Location` 里有没有 `ticket`」：票根死了就是这么
/// 表现出来的（上游回 200 登录页，或回一枚 `SSOTGC=; Max-Age=0` 的清票指令）。
async fn exchange(client: &reqwest::Client, tgc: &str) -> Option<(String, Option<String>)> {
    let mut login = url::Url::parse(&format!("{AUTH_ORIGIN}{LOGIN_PATH}")).ok()?;
    login.query_pairs_mut().append_pair("service", PORTAL_PAGE);
    let response = match client
        .get(login)
        .header(
            "Cookie",
            format!("{}={tgc}", credentials::SSO_TGC_COOKIE),
        )
        .header("Accept", "text/html,application/xhtml+xml,application/xml;q=0.9,*/*;q=0.8")
        .header("User-Agent", UA)
        .send()
        .await
    {
        Ok(response) => response,
        Err(error) => {
            verbose(&format!("SSO 换票请求失败：{}", crate::http::describe_chain(&error)));
            return None;
        }
    };
    let status = response.status().as_u16();
    let location = response
        .headers()
        .get(reqwest::header::LOCATION)
        .and_then(|value| value.to_str().ok())
        .unwrap_or("")
        .to_string();
    // ⚠️ 顺序不能反：先捞票根**再**判成败。
    // 服务端清票时回的是 `SSOTGC=; Max-Age=0`，`last_cookie` 对空值返回 `None`
    // （见它的判据），所以这次「清空」不会被误当成「换到新票根」写回账号。
    let renewed = last_cookie(response.headers(), credentials::SSO_TGC_COOKIE)
        .filter(|value| credentials::looks_like_tgc(value));
    if !(300..400).contains(&status) || location.is_empty() {
        verbose(&format!(
            "SSO 换票未拿到重定向（HTTP {status}）：TGC 可能已失效，需要在浏览器里重新登录后重取"
        ));
        return None;
    }
    let ticket = ticket_of(&location)?;
    Some((ticket, renewed))
}

/// 单飞地「换票 + 换会话 + 落盘」。`renew` 走这条（cookie 已经死了，要新的一条）。
///
/// ── 为什么与 `roll` 共用同一把单飞钥匙 ──────────────────────
/// 两条路都会用**同一枚**票根去换 ticket。并发跑会同时铸出 N 条服务端会话，
/// 而且**后写的那一枚票根会覆盖先写的** —— 服务端对旧票根做滑动过期，如果本地
/// 存的是较旧的那一枚，下一次就可能换不到（票根被另一次登录顶掉）。
/// 共用 `codearts-portal:{account_id}` 这个 key，并发时只有第一个真跑，其余直接
/// 拿结果，问题从根上消失。
///
/// 返回新 cookie（**不管落盘成没成**）：它已经能用了，本次请求不该因为写不回
/// 数据库而作废 —— 与 `refresh.rs` 处理「续期成功但写回失败」同一取向。
async fn rebuild(
    ctx: &PluginCtx,
    account_id: &str,
    tgc: &str,
    proxy: Option<&ResolvedProxy>,
) -> Result<String, String> {
    let key = format!("codearts-portal:{account_id}");
    let tgc = tgc.trim().to_string();
    let account = account_id.to_string();
    let ctx = ctx.clone();
    let proxy = proxy.cloned();
    // ⚠️ 两处形态要求（`flight::run` 的签名所致，不是风格问题）：
    //   1. `async move` + 先把捕获值 clone 出来：闭包被 `run` 消费掉，它返回的
    //      future 不能再借闭包环境（`FnOnce() -> Fut` 不接受借用环境的 future）；
    //   2. 闭包返回 `Result<_, String>`，而 `mint` 给的是 `Option` —— 必须显式
    //      match 转成 `Err`，不能 `?`（类型不兼容，且 `panic=abort` 下不许 unwrap）。
    crate::flight::run(&key, || async move {
        // ⚠️ **进锁后必须重读账号**（这是 `flight::run` 的约定，见该模块头注）：
        // 等锁的这段时间里，前一个跑完的请求可能已经落盘了一枚**更新**的票根。
        // 继续用调用方手里那枚旧票根去换票，会被服务端当成「上一代」而失败
        // （票根是滑动的：新的签发即意味着旧的该退了）。重读只是一次本地读，
        // 比一次 SSO 往返便宜得多。
        let newest = freshest_tgc(&ctx, &account, &tgc).await;
        let Some(minted) = mint(&newest, proxy.as_ref()).await else {
            return Err(TGC_DEAD.to_string());
        };
        if minted.cookie.is_empty() {
            return Err(TGC_DEAD.to_string());
        }
        persist(&ctx, &account, &minted.cookie, minted.tgc.as_deref()).await;
        Ok(minted.cookie)
    })
    .await
}

/// 账号里**当前**存着的那枚票根；读不出来 / 是空的就退回调用方手里这枚。
///
/// ── 为什么不让调用方直接用自己那枚 ──────────────────────────
/// `renew` 与 `touch` 都会走到 [`rebuild`]，而它们手里的 `Credential` 是**进入本次
/// 查询时**读出来的快照。谁先跑完谁就把票根写新了，后跑的那位如果还用旧快照，
/// 换票就必然失败 —— 症状是「明明刚成功过一次，下一分钟又报票根失效」。
/// 重读一次账号是最便宜的纠正方式（本地 SQLite 读，不打上游）。
async fn freshest_tgc(ctx: &PluginCtx, account_id: &str, fallback: &str) -> String {
    let Ok(record) = crate::store::record(ctx, super::PROVIDER_ID, account_id).await else {
        return fallback.to_string();
    };
    let secret = crate::store::secret_view(&record);
    let stored = crate::store::pick(&secret, &[credentials::SSO_TGC_FIELD]);
    if stored.is_empty() {
        fallback.to_string()
    } else {
        stored
    }
}

/// 重建并**落盘**（同一账号的单飞，避免并发余额查询同时铸一串）。
///
/// ── 落盘写两样：cookie **和滚动票根** ────────────────────────
/// 只写 cookie 是不够的（那是本次「有效期又没了」的根因）：票根本身有超时窗口，
/// 不跟着往前推，最多活一个窗口就死，之后所有查询都静默降级。所以每次换票成功
/// 都把上游下发的新票根一起写回（见 [`Minted`] 的说明）。
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
    match rebuild(ctx, account_id, &tgc, proxy).await {
        Ok(cookie) => {
            crate::logging::log("[CodeArts]", "网页会话 cookie 已用 SSO 票根自动重建");
            Some(cookie)
        }
        Err(reason) => {
            // ⚠️ 必须**可见**（`warn`，不是 `verbose`）：
            // 这条通道失败是静默降级 —— 账号页只会少掉「有效期」那一列，余额数字
            // 照常显示，用户完全看不出发生了什么（本次就是这么被问上来的）。
            // 一行 warn 落进日志，至少排查时能立刻看到「票根死了，去重新登录」。
            //
            // 按小时节流（见 `claim_warn`）：票根死了之后每次查询都会失败，
            // 不节流会把日志表刷满，反而淹没别的线索。
            if claim_warn(account_id) {
                crate::logging::warn(
                    "[CodeArts]",
                    &format!(
                        "SSO 票根已失效（{reason}）：逐批明细与「有效期」列将不可用，\
                         请在设置里重新登录一次 CodeArts 以重取票根"
                    ),
                );
            } else {
                crate::logging::verbose(
                    "[CodeArts]",
                    &format!("SSO 票根仍未恢复（{reason}）：本小时已提示过，不再重复"),
                );
            }
            None
        }
    }
}

/// 换票失败时给日志的那句话（要能让用户照着动作）。
///
/// 与 `mint` 里那句同一说法，避免两处措辞不同让人以为是两个问题。
const TGC_DEAD: &str = "换票未拿到重定向，TGC 可能已过期";

/// 「票根已失效」这条 warn 的节流窗口。
///
/// ── 为什么不每次都 warn ──────────────────────────────────────
/// 票根一旦死了，**每一次**余额查询都会走到 `renew` 并失败（自动查询按每账号
/// 间隔跑，用户还可能连点）。每条都 `warn` 会把日志表刷满，反而看不到别的。
/// 一小时说一次足够：它要传达的是「去重新登录」，这件事不会在一分钟内变化。
const WARN_INTERVAL: std::time::Duration = std::time::Duration::from_secs(60 * 60);

/// 同一条 warn 是否该说（每个账号每小时一次）。
fn claim_warn(account_id: &str) -> bool {
    let now = std::time::Instant::now();
    let Ok(mut marks) = touch_marks().lock() else { return true };
    let key = format!("warn:{account_id}");
    match marks.get(&key) {
        Some(last) if now.duration_since(*last) < WARN_INTERVAL => false,
        _ => {
            marks.insert(key, now);
            true
        }
    }
}

/// 「票根最近被推过」的节流窗口。
///
/// ── 为什么需要节流 ──────────────────────────────────────────
/// `touch` 会在**每一次成功的明细读取之后**被调用（见 `credits::fetch_renewing_with_state`），
/// 而余额查询可以由用户连点、也可以被自动查询按「每账号间隔」高频触发。
/// 每次都真去换一枚票根既浪费一次 SSO 往返，也会在上游留下不必要的登录审计。
///
/// 10 分钟这个值的取法：票根窗口是「闲置 1 小时 ~ 24 小时」这个量级，只要在
/// 窗口内被推一次就不会倒；而 10 分钟远小于 1 小时，所以即便用户只在一天里
/// 零星点几次余额，票根也始终被推在窗口以内。反过来它也足够大 —— 连点十次
/// 余额只会产生一次 SSO 往返。
const TOUCH_INTERVAL: std::time::Duration = std::time::Duration::from_secs(10 * 60);

/// 各类「上次做过」的时刻（进程级节流表）。
///
/// 只用来做节流判断，**不含任何凭据**：键是「用途 + 账号 id」，值是时刻。
/// 两种键：
///   - `{account_id}` —— 上次推票根的时刻（见 [`claim_touch`]）；
///   - `warn:{account_id}` —— 上次报「票根已失效」的时刻（见 [`claim_warn`]）。
fn touch_marks() -> &'static std::sync::Mutex<std::collections::HashMap<String, std::time::Instant>> {
    static MARKS: std::sync::OnceLock<std::sync::Mutex<std::collections::HashMap<String, std::time::Instant>>> =
        std::sync::OnceLock::new();
    MARKS.get_or_init(|| std::sync::Mutex::new(std::collections::HashMap::new()))
}

/// 现在是否该推一次票根（并顺手记下这次时刻）。
///
/// 判据是「距上次推超过 [`TOUCH_INTERVAL`]」。锁中毒时不阻断链路：
/// 那就当作可以推（多推一次无害，少推才是问题）。
fn claim_touch(account_id: &str) -> bool {
    let now = std::time::Instant::now();
    let Ok(mut marks) = touch_marks().lock() else { return true };
    match marks.get(account_id) {
        Some(last) if now.duration_since(*last) < TOUCH_INTERVAL => false,
        _ => {
            marks.insert(account_id.to_string(), now);
            true
        }
    }
}

/// **滚动续期**：趁会话还活着，把票根往前推一次（后台跑，不阻塞读数）。
///
/// ── 为什么必须有这一步（而不是「cookie 挂了再 `renew`」就够）──────
/// `SSOTGC` 靠「每次使用都被服务端换新」而滑动过期。只在 cookie 失效时才去
/// 换票的话，cookie 活着的那整段时间里票根一次都不会被推 —— 于是两者**同时**
/// 到点，一起死（2026-11-11 实测：11-10 12:33 起的票根与 cookie，11-11 13:15
/// 同时失效，逐批明细整段消失）。
///
/// 这里在每次成功读到明细之后顺手推一次票根，票根的窗口就永远落在
/// 「最近一次成功查询 + 一个超时窗口」上。**只要网关在跑、余额在查，票根就不会倒。**
///
/// ── 为什么只走第 1 跳（不是完整三跳）─────────────────────────
/// 推票根只需要「用票根换一次 ticket」这一步 —— 服务端就是在这一步重置闲置计时器、
/// 并（通常）下发新票根。后两跳（换会话、取 cftk）是 `renew` 的正事：会话**还活着**
/// 的时候去换一条新的，既白打两次门户请求，又会把用户手贴的那条 cookie 悄悄换成
/// 机器铸的。所以 `touch` 走 [`roll`]（一跳 + 只写票根），`renew` 才走 [`rebuild`]
/// （三跳 + 写 cookie）。
///
/// ── 三条纪律 ────────────────────────────────────────────────
///   1. **不阻塞调用方**：明细已经拿到，推票根失败不该拖慢或破坏这次读数；
///   2. **不报错**：失败只落一行 `verbose`（下一步还有 `renew` 兜着），
///      真正到「换不到」时才由 `renew` 出 `warn`；
///   3. **不覆盖坏值**：新票根只在形状合法（[`credentials::looks_like_tgc`]）
///      时才写回，见 `persist_tgc`。
///
/// 没配票根的账号直接跳过（一次请求都不发）。
pub fn touch(
    ctx: &PluginCtx,
    account_id: &str,
    credential: &Credential,
    proxy: Option<&ResolvedProxy>,
) {
    let tgc = credential.sso_tgc.trim().to_string();
    if tgc.is_empty() {
        // 没票根就没有可续的东西 —— 这个账号本来就走「cookie 死了等用户重贴」那条路。
        return;
    }
    if !claim_touch(account_id) {
        return;
    }
    let ctx = ctx.clone();
    let account = account_id.to_string();
    let proxy = proxy.cloned();
    tokio::spawn(async move {
        match roll(&ctx, &account, &tgc, proxy.as_ref()).await {
            // ⚠️ 与 `renew` 的区别就在这里：`touch` 的成功**不需要**吭声
            // （`persist_tgc` 已在 `verbose` 里记了「票根已往前推」），
            // 失败也不必 `warn` —— cookie 还活着，用户无感；真到 cookie 也失效时，
            // `renew` 会给出那句可见的 warn。
            Ok(()) => {}
            Err(reason) => verbose(&format!(
                "票根滚动续期未成功（{reason}；会话仍可用，下次再试）"
            )),
        }
    });
}

/// 只推票根：第 1 跳 + 写回新票根，**不碰 cookie**。
///
/// 与 [`rebuild`] 共用同一把单飞钥匙：两条路都会用票根换 ticket，并发跑会各拿到
/// 一枚「代为最新」的票根、互相把对方写旧。
async fn roll(
    ctx: &PluginCtx,
    account_id: &str,
    tgc: &str,
    proxy: Option<&ResolvedProxy>,
) -> Result<(), String> {
    let key = format!("codearts-portal:{account_id}");
    let tgc = tgc.trim().to_string();
    let account = account_id.to_string();
    let ctx = ctx.clone();
    let proxy = proxy.cloned();
    crate::flight::run(&key, || async move {
        // 进锁后重读（同 `rebuild`）：前一个可能刚把票根写新了。
        let newest = freshest_tgc(&ctx, &account, &tgc).await;
        let client = strict_client(proxy.as_ref()).ok_or_else(|| TGC_DEAD.to_string())?;
        let (_, renewed) = exchange(&client, &newest).await.ok_or_else(|| TGC_DEAD.to_string())?;
        let Some(renewed) = renewed else {
            // 换票成功但上游这次没带新票根 —— **不是失败**：滑动过期里「使用即续」
            // 这一半已经生效了（服务端闲置计时器被重置）。没有新值就不该覆盖旧值，
            // 否则等于把一枚好票根洗成空串。
            verbose("票根换票成功但本次未下发新票根（闲置窗口已被重置，保留原值）");
            return Ok(());
        };
        persist_tgc(&ctx, &account, &renewed).await;
        Ok(())
    })
    .await
}

/// 把新 cookie（以及滚动票根）写回账号 secret。
///
/// 与续期那条链同一纪律：**读最新记录、比较再写**（`store::save` 的 `expect` 语义），
/// 写不进去只记日志 —— cookie 本身是短命物，丢了下次还能再铸。
///
/// ── 票根为什么要一起写（这是「保活」的实质）───────────────
/// 票根是**会话级**的（`Set-Cookie` 无 `Max-Age` / `Expires`，见模块头），
/// 但它**滑动过期**：换票这个动作本身就把服务端的闲置计时器重置了（见 [`Minted`]）。
/// 上游多数时候还会再下发一枚新值 —— 把新值写回去，本地这枚就不会是签发当时那一枚；
/// 完全不写回去的后果是它最多活过一个窗口就死，有效期列随即无声消失
/// （2026-11-11 实测的正是这个）。
///
/// `tgc` 为 `None`（响应里没带新票根）时**不覆盖**旧值 —— 上游某个响应省略这一项
/// 不该被当成「票根没了」而把账号里的值洗掉。
async fn persist(ctx: &PluginCtx, account_id: &str, cookie: &str, tgc: Option<&str>) {
    let record = match crate::store::record(ctx, super::PROVIDER_ID, account_id).await {
        Ok(record) => record,
        Err(error) => {
            verbose(&format!("重建的 cookie 未能落盘（读账号失败）：{}", error.message));
            return;
        }
    };
    let expect = crate::store::secret_view(&record);
    let mut fields: Vec<(&str, Value)> = vec![(
        credentials::SESSION_COOKIE_FIELD,
        Value::String(cookie.to_string()),
    )];
    let renewed_tgc = tgc.map(str::trim).filter(|value| !value.is_empty());
    if let Some(tgc) = renewed_tgc {
        fields.push((credentials::SSO_TGC_FIELD, Value::String(tgc.to_string())));
    }
    let next = crate::store::with_fields(&expect, &fields);
    match crate::store::save(ctx, account_id, &expect, &next).await {
        Ok(true) => {
            if renewed_tgc.is_some() {
                // 写成功才说这句：它是「票根确实被往前推了」的凭据，
                // 排查「有效期什么时候断的」时就看这条日志的最后一笔。
                verbose("票根已随新会话一起落盘（票根窗口已往后推）");
            }
        }
        Ok(false) => verbose("重建的 cookie 未能落盘：账号在重建期间被改动"),
        Err(error) => verbose(&format!("重建的 cookie 未能落盘：{}", error.message)),
    }
}

/// 只把新票根写回账号（`touch` 走的这条：**不动 cookie**）。
///
/// 与 [`persist`] 同一纪律（读最新、比较再写、写不进去只记日志），差别只在字段：
/// 这里只写 `ssoTgc`。会话 cookie 保持用户原来那一条 —— 它还活着，没有理由换。
async fn persist_tgc(ctx: &PluginCtx, account_id: &str, tgc: &str) {
    let record = match crate::store::record(ctx, super::PROVIDER_ID, account_id).await {
        Ok(record) => record,
        Err(error) => {
            verbose(&format!("新票根未能落盘（读账号失败）：{}", error.message));
            return;
        }
    };
    let expect = crate::store::secret_view(&record);
    let fields = [(
        credentials::SSO_TGC_FIELD,
        Value::String(tgc.trim().to_string()),
    )];
    let next = crate::store::with_fields(&expect, &fields);
    match crate::store::save(ctx, account_id, &expect, &next).await {
        // 这条 verbose 是排查「有效期什么时候断的」的唯一凭据：它每次成功滚动
        // 都会留一笔，最后一笔的时间就是票根被推到的位置。
        Ok(true) => verbose("票根已往前推并落盘（会话 cookie 未改动）"),
        Ok(false) => verbose("新票根未能落盘：账号在滚动期间被改动（下一轮再推）"),
        Err(error) => verbose(&format!("新票根未能落盘：{}", error.message)),
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