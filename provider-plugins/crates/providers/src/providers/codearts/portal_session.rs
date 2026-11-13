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
//! ── 「保活」到底靠什么（2026-11-13 定稿，两次返工的结论）──────
//! 票根是滑动过期的，所以保活 = **在窗口内不断拿它换一次 ticket**。这里有两个
//! 相互独立的动作，接线位置**必须**分开：
//!
//! | 入口 | 什么时候跑 | 走几跳 | 写什么 |
//! |---|---|---|---|
//! | [`keepalive`] → [`touch`] → [`roll`] | **每次余额查询**（60 秒心跳） | 1 | 只写票根 + 推进时刻 |
//! | [`renew`] → [`rebuild`] → [`mint`] | cookie 读不出明细时 | 3 | cookie + 票根 + 推进时刻 |
//!
//! 两次返工都栽在**把 `touch` 接在 `renew`/cookie 成功那一侧**：
//!   · §10.21（11-11）：只接在 `renew` 成功之后 —— cookie 一直有效时它一次都不跑；
//!   · 第二次（11-12）：改挂在「cookie 读取成功」分支 —— 真实节奏下那条分支几乎
//!     不成立（cookie 会话级、机器休眠、用户按需点余额），票根从签发起一次都没被推。
//! 11-12 的实证：15:43 重建成功 → 23:02 票根被清，中间**零次 `touch`**、零条日志。
//!
//! 现在 `touch` 由 [`keepalive`] 无条件驱动（判据是账号里落盘的
//! [`credentials::SSO_TGC_PUSHED_FIELD`]），与 cookie 死活彻底解耦。
//!
//! ── 但「推得勤」只是一半：窗口本身要够宽（2026-11-13 补）──────
//! 上面那条只解决「网关在跑的时候不会闲置过期」。真实节奏是「白天零星用、
//! 晚上关机」，每天离线 9~14 小时 —— 而那段时间本模块**一次都跑不到**。
//! 所以还需要把服务端那个窗口拉长到能盖住关机时长：见
//! [`super::session_window`]（它把账号的会话超时策略从实测的 60 分钟调到
//! 官方上限 24 小时）。两件事合起来才是完整的「不死」：**窗口够长 + 一直在用**。
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

/// 换票链失败的三档 —— **必须分开**，因为它们的处置完全相反。
///
/// ── 为什么不能统一报「票根已失效」（2026-11-13 实测）───────────
/// 早先任何一步失败都写成「SSO 票根已失效…请重新登录」。真实日志里这类 warn
/// 有一半根本不是票根死了 —— 最硬的反证是 11-11 那两分钟：
/// ```text
///   11-11 14:46:12  warn  SSO 票根已失效（换票未拿到重定向…）
///   11-11 14:48:34  info  网页会话 cookie 已用 SSO 票根自动重建   ← 同一枚票根，成功
/// ```
/// 同一枚票根两分钟后就能换票，说明 14:46 那次是**短暂失败**（开机后网络未就绪 /
/// 上游限流），不是票根死亡。11-12 15:41→15:43 也是同一形状。把这些说成「去重新
/// 登录」，既误导用户，又淹没了真正的失效信号 —— 十条一样的 warn 里再没有一条
/// 能区分的。
///
/// 分档之后每档的处置：
///   · [`Failed::Transient`] —— 短退避（2 分钟）后重试，**不**碰 `claim_warn`、
///     **不**报「去重新登录」；
///   · [`Failed::Rejected`] —— 票根真被上游拒了：长退避（10 分钟）+ 每小时一句 warn；
///   · [`Failed::Shape`] —— 换到票了但后两跳形状不对（上游改版）：不劝用户重登，
///     因为重登救不了改版。
#[derive(Clone, Debug)]
pub enum Failed {
    /// 这次没成但**下次可能就成**：DNS / 连接 / 超时，或上游 5xx / 429 限流。
    Transient(String),
    /// 服务端明确回答「不给票」（200 登录页 / 清票指令）—— 这才是「已失效」。
    ///
    /// 载荷是**一句能直接看的证据**（如「HTTP 200，服务端当场清票
    /// （Set-Cookie: SSOTGC=; Max-Age=0）」），不是裸状态码：这一条 warn 会落到
    /// 用户面前，要能让人一眼看出「上游把票收了」而不是「某个数字不对」。
    Rejected(String),
    /// 票根收下了，但后两跳拿不到会话 / cftk（上游形状不符）。
    Shape(String),
}

impl Failed {
    /// 给人看的一句原因（日志 / `WebCreditsState` 的 `warnings` 共用）。
    pub fn reason(&self) -> String {
        match self {
            Failed::Transient(detail) => format!("暂时性失败（{detail}）"),
            Failed::Rejected(detail) => format!("服务端不给票（{detail}）：TGC 已被上游收回"),
            Failed::Shape(detail) => format!("换到票但会话没铸成（{detail}）：上游形状可能变了"),
        }
    }

    /// 是否**确实**需要用户去重新登录（只有这一档才算）。
    pub fn dead(&self) -> bool {
        matches!(self, Failed::Rejected(_))
    }

    /// 是不是「过一会儿自己会好」那一档（`credits` 据此给「稍后重试」的文案）。
    pub fn is_transient(&self) -> bool {
        matches!(self, Failed::Transient(_))
    }

    /// 这一档的退避时长。
    ///
    /// 暂时性那档短：多半是刚开机、Wi-Fi 还没连上，两分钟后自然就好；压 10 分钟
    /// 等于让一个刚恢复的网络白等八分钟。
    fn backoff(&self) -> std::time::Duration {
        match self {
            Failed::Transient(_) => TRANSIENT_BACKOFF,
            Failed::Rejected(_) | Failed::Shape(_) => FAIL_BACKOFF,
        }
    }
}

/// 把 [`Failed`] 压成一行可逆字符串 —— `flight::run` 的 `Err` 只收得动 `String`。
///
/// ⚠️ 为什么不用「解析中文文案」来还原档位：文案随时会改，改一个字档位就丢了，
/// 而这种丢法是**静默**的（失败还是失败，只是变成「票根已失效」）。用固定标记做
/// 前缀，文案怎么改都不影响分流。
fn tag(failed: &Failed) -> String {
    match failed {
        Failed::Transient(detail) => format!("{FAILED_TRANSIENT}\u{1}{detail}"),
        Failed::Rejected(detail) => format!("{FAILED_REJECTED}\u{1}{detail}"),
        Failed::Shape(detail) => format!("{FAILED_SHAPE}\u{1}{detail}"),
    }
}

/// [`tag`] 的逆：认不出标记就按 [`Failed::Shape`] 处理（不劝用户重登）。
fn untag(tagged: &str) -> Failed {
    if let Some(detail) = tagged.strip_prefix(FAILED_TRANSIENT) {
        return Failed::Transient(detail.trim_start_matches('\u{1}').to_string());
    }
    if let Some(detail) = tagged.strip_prefix(FAILED_REJECTED) {
        return Failed::Rejected(detail.trim_start_matches('\u{1}').to_string());
    }
    if let Some(detail) = tagged.strip_prefix(FAILED_SHAPE) {
        return Failed::Shape(detail.trim_start_matches('\u{1}').to_string());
    }
    // 没有标记：未知失败，按「形状不符」处置 —— 那种情形重登救不了，
    // 报「去重新登录」只会把人引到错的方向。
    Failed::Shape(tagged.to_string())
}

const FAILED_TRANSIENT: &str = "transient";
const FAILED_REJECTED: &str = "rejected";
const FAILED_SHAPE: &str = "shape";

/// 重放整条 SSO 链，产出一条能打 portal BFF 的 cookie 串（外加新票根）。
///
/// 失败时返回 [`Failed`]（分档见那里的说明），让调用方能分别处置。
pub async fn mint(tgc: &str, proxy: Option<&ResolvedProxy>) -> Result<Minted, Failed> {
    let tgc = tgc.trim();
    if tgc.is_empty() {
        return Err(Failed::Shape("账号里没有票根".to_string()));
    }
    let client = match strict_client(proxy) {
        Some(client) => client,
        None => return Err(Failed::Shape("HTTP 客户端构造失败".to_string())),
    };

    // ── 第 1 跳：TGC → 302 + ticket，**并顺走这枚滚动票根** ────────
    let (ticket, renewed_tgc) = exchange(&client, tgc).await?;

    // ── 第 2 跳：ticket → 门户会话 ─────────────────────────────
    let mut hop = match url::Url::parse(PORTAL_PAGE) {
        Ok(url) => url,
        Err(error) => return Err(Failed::Shape(format!("门户页地址非法：{error}"))),
    };
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
                    return Err(Failed::Shape(
                        "响应里没有 devclouddevuibjJ_SESSION_ID".to_string(),
                    ));
                }
            }
        }
        Err(error) => {
            return Err(Failed::Transient(crate::http::describe_chain(&error)));
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
                    return Err(Failed::Shape(
                        "门户页没有下发 cftk（换到的会话可能不是本人）".to_string(),
                    ));
                }
            }
        }
        Err(error) => {
            return Err(Failed::Transient(crate::http::describe_chain(&error)));
        }
    };

    // 两个值必须同一次铸出来（见模块头），拼成单行 cookie 串。
    Ok(Minted {
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
/// ⚠️ 成败判据只用「有没有 302 + `Location` 里有没有 `ticket`」，失败分两档：
/// 请求发不出去 = [`Failed::Transient`]（不是票根的错），拿到了响应但没有 302/票 =
/// [`Failed::Rejected`]（票根被服务端拒了，这才是真的死了）。
async fn exchange(
    client: &reqwest::Client,
    tgc: &str,
) -> Result<(String, Option<String>), Failed> {
    let mut login = match url::Url::parse(&format!("{AUTH_ORIGIN}{LOGIN_PATH}")) {
        Ok(url) => url,
        Err(error) => return Err(Failed::Shape(format!("换票入口地址非法：{error}"))),
    };
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
        Err(error) => return Err(Failed::Transient(crate::http::describe_chain(&error))),
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
        // ── 先排除「上游自己不舒服」那一类（2026-11-13 加）────────────
        // 429 / 5xx 是上游限流或抖动，票根**没有任何问题** —— 一律当暂时性失败，
        // 两分钟后重试。不分这一档的话，一次限流就会给用户留一句「请重新登录」，
        // 而 11-11 14:46→14:48 的证据正说明：同一枚票根两分钟后就好。
        if status == 429 || status >= 500 {
            return Err(Failed::Transient(format!("上游返回 HTTP {status}")));
        }
        // 走到这里说明**请求通了**、服务端明确回答「不给票」—— 票根真被拒。
        return Err(Failed::Rejected(status));
    }
    let ticket = match ticket_of(&location) {
        Some(ticket) => ticket,
        None => {
            return Err(Failed::Shape(
                "302 的 Location 里没有 ticket 参数".to_string(),
            ));
        }
    };
    Ok((ticket, renewed))
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
///
/// 失败带 [`Failed`] 档位回来：`renew` 要按档位决定是「劝用户重登」还是「等两分钟」。
async fn rebuild(
    ctx: &PluginCtx,
    account_id: &str,
    tgc: &str,
    proxy: Option<&ResolvedProxy>,
) -> Result<String, Failed> {
    let key = format!("codearts-portal:{account_id}");
    let tgc = tgc.trim().to_string();
    let account = account_id.to_string();
    let ctx = ctx.clone();
    let proxy = proxy.cloned();
    // ⚠️ 两处形态要求（`flight::run` 的签名所致，不是风格问题）：
    //   1. `async move` + 先把捕获值 clone 出来：闭包被 `run` 消费掉，它返回的
    //      future 不能再借闭包环境（`FnOnce() -> Fut` 不接受借用环境的 future）；
    //   2. 闭包返回 `Result<_, String>`：`flight` 只传得动 `String`，所以这里把
    //      [`Failed`] 压成一句话出去，**档位由 `renew` 从这句话里认回来**？
    //      —— 不，那太脆。于是这种「需要保档位」的场合改用 `String` 带上档位
    //      前缀（见 `FAILED_NETWORK` / `FAILED_SHAPE` 两个标记）。
    let outcome: Result<String, String> = crate::flight::run(&key, || async move {
        // ⚠️ **进锁后必须重读账号**（这是 `flight::run` 的约定，见该模块头注）：
        // 等锁的这段时间里，前一个跑完的请求可能已经落盘了一枚**更新**的票根。
        // 继续用调用方手里那枚旧票根去换票，会被服务端当成「上一代」而失败
        // （票根是滑动的：新的签发即意味着旧的该退了）。重读只是一次本地读，
        // 比一次 SSO 往返便宜得多。
        let newest = freshest_tgc(&ctx, &account, &tgc).await;
        match mint(&newest, proxy.as_ref()).await {
            Ok(minted) if !minted.cookie.is_empty() => {
                persist(&ctx, &account, Some(&minted.cookie), minted.tgc.as_deref()).await;
                Ok(minted.cookie)
            }
            Ok(_) => Err(tag(&Failed::Shape("铸出来的 cookie 是空的".to_string()))),
            Err(failed) => Err(tag(&failed)),
        }
    })
    .await;
    outcome.map_err(|tagged| untag(&tagged))
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
///
/// ── 返回 `Result` 而不是 `Option`（2026-11-13 改）─────────────
/// 调用方（`credits::fetch_renewing_with_state`）要把「为什么没拿到会话」写进
/// 账号页的 `warnings`，而那句话必须分档 —— 网络抖动与票根死亡要说完全不同的
/// 话。早先这里只有 `Option`，两档被压成同一个「已过期」，用户被误导去重登。
pub async fn renew(
    ctx: &PluginCtx,
    account_id: &str,
    credential: &Credential,
    proxy: Option<&ResolvedProxy>,
) -> Result<String, Failed> {
    let tgc = credential.sso_tgc.trim().to_string();
    if tgc.is_empty() {
        // 没票根不是「票根死了」：这个账号本来就走「cookie 死了等用户重贴」那条路。
        return Err(Failed::Shape("账号里没有 SSO 票根".to_string()));
    }
    match rebuild(ctx, account_id, &tgc, proxy).await {
        Ok(cookie) => {
            clear_failed(account_id);
            crate::logging::log("[CodeArts]", "网页会话 cookie 已用 SSO 票根自动重建");
            Ok(cookie)
        }
        Err(failed) => {
            mark_failed(account_id, failed.backoff());
            note_failure(account_id, &failed);
            Err(failed)
        }
    }
}

/// 把一次换票失败按**档位**落一条看得懂的日志。
///
/// ── 三档的说法必须不同（2026-11-13 实测的教训）───────────────
/// 早先不分档，一律 `warn` 说「票根已失效…请重新登录」。而 11-12 23:02、11-13 11:45
/// 那两次的真实原因是**机器刚从休眠恢复、网络还没通**（同一轮里其它 provider 全在
/// 报 dns error / timeout）。把网络问题说成票根问题，用户会去白重登一次，日志也
/// 被十条一样的 warn 淹掉，真正的失效反而看不出来。
///
/// 现在：
///   · 暂时性档 → `verbose`（不落库）：一天几十次开机抖动，落库只会刷屏，
///     但 `debugMode` 打开时仍能看到；
///   · 票根被拒 → 每小时一条 `warn`，明说「去重新登录」；
///   · 形状不符 → 每小时一条 `warn`，但说的是「上游改版」，**不**劝重登。
fn note_failure(account_id: &str, failed: &Failed) {
    let reason = failed.reason();
    if failed.is_transient() {
        verbose(&format!("CodeArts 换票暂时没成（{reason}）：两分钟后自动重试"));
        return;
    }
    if !failed.dead() {
        // 形状档不该催用户重登（重登救不了改版），但它是**新情况**，值得一小时说一次。
        if claim_warn(account_id) {
            crate::logging::warn(
                "[CodeArts]",
                &format!(
                    "CodeArts 网页会话换票异常（{reason}）：有效期列会缺读数，\
                     重登不一定能解决，先观察上游是否改版"
                ),
            );
        } else {
            verbose(&format!("CodeArts 换票仍未成（{reason}）：本小时已提示过"));
        }
        return;
    }
    if claim_warn(account_id) {
        crate::logging::warn(
            "[CodeArts]",
            &format!(
                "SSO 票根已失效（{reason}）：逐批明细与「有效期」列将不可用，\
                 请在设置里重新登录一次 CodeArts 以重取票根"
            ),
        );
    } else {
        verbose(&format!("SSO 票根仍未恢复（{reason}）：本小时已提示过，不再重复"));
    }
}

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
        Some((last, _)) if now.duration_since(*last) < WARN_INTERVAL => false,
        _ => {
            marks.insert(key, (now, WARN_INTERVAL));
            true
        }
    }
}

/// 「票根最近被推过」的节流窗口（进程内兜底）。
///
/// ── 这个表只解决「同一瞬间被并发调两次」────────────────────
/// 真正的节流判据是**账号里的** `ssoTgcPushedAt`（见
/// [`credentials::SSO_TGC_PUSHED_FIELD`]）：它跨进程存活，重启后仍然知道上次
/// 什么时候推的。内存表只用来挡住「同一秒内被两个调用点同时问」这一类重复，
/// 所以它的窗口只需要覆盖一次 SSO 往返（秒级），不必是保活间隔。
///
/// ── 2026-11-13 修的那一处（本次「保活又不生效」的根因之一）────
/// 早先这里是唯一的节流判据，窗口 10 分钟，而且 `Instant` 只活在本进程里。
/// 于是：网关一重启表就空 → 第一次调用必然真推（这一点反而是对的），但
/// **进程活着的那段时间里，只要 cookie 一直没被成功读过，`touch` 根本不会被
/// 调到** —— 节流窗口反而成了「不推」的借口。现在推票根与 cookie 解耦，
/// 这个窗口缩到 30 秒，只当防抖用。
const TOUCH_DEBOUNCE: std::time::Duration = std::time::Duration::from_secs(30);

/// 各类「上次做过」的时刻（进程级节流表）。
///
/// 只用来做节流判断，**不含任何凭据**：键是「用途 + 账号 id」，值是时刻。
/// 三种键：
///   - `{account_id}` —— 上次推票根的时刻（见 [`claim_touch`]）；
///   - `warn:{account_id}` —— 上次报「票根已失效」的时刻（见 [`claim_warn`]）；
///   - `fail:{account_id}` —— 上次换票失败的时刻**与该档的退避窗口**
///     （见 [`mark_failed`]；窗口随档位不同，所以值是一对）。
fn touch_marks() -> &'static std::sync::Mutex<
    std::collections::HashMap<String, (std::time::Instant, std::time::Duration)>,
> {
    static MARKS: std::sync::OnceLock<
        std::sync::Mutex<std::collections::HashMap<String, (std::time::Instant, std::time::Duration)>>,
    > = std::sync::OnceLock::new();
    MARKS.get_or_init(|| std::sync::Mutex::new(std::collections::HashMap::new()))
}

/// 现在是否该推一次票根（并顺手记下这次时刻）。
///
/// 判据是「距上次真推超过 [`TOUCH_DEBOUNCE`]」。锁中毒时不阻断链路：
/// 那就当作可以推（多推一次无害，少推才是问题）。
fn claim_touch(account_id: &str) -> bool {
    let now = std::time::Instant::now();
    let Ok(mut marks) = touch_marks().lock() else { return true };
    match marks.get(account_id) {
        Some((last, _)) if now.duration_since(*last) < TOUCH_DEBOUNCE => false,
        _ => {
            marks.insert(account_id.to_string(), (now, TOUCH_DEBOUNCE));
            true
        }
    }
}

/// 换票失败后的退避窗口（**票根被拒 / 形状不符**两档用这个）。
///
/// ── 为什么必须有它（2026-11-13 加）───────────────────────────
/// 票根一旦死了，`due_for_roll` 会因为「落盘时刻不再前进」而**永远判该推** ——
/// 于是每 [`TOUCH_DEBOUNCE`]（30 秒）就白打一次 SSO 往返，一天数千次，上游看到的
/// 是一台无休止重试的机器，本机也要多烧流量。
///
/// 这两档的失败**不会自己好**（要么等用户重登，要么等上游改回来），所以压得久：
/// 10 分钟 = 144 次/天，而 `claim_warn` 每小时一句提醒照发，用户不会被静默。
const FAIL_BACKOFF: std::time::Duration = std::time::Duration::from_secs(10 * 60);

/// 暂时性失败的退避窗口（**短**）。
///
/// ── 为什么与上面那个差这么多（2026-11-11 / 11-12 的实证）───────
/// ```text
///   11-11 14:46:12  warn  SSO 票根已失效（…）
///   11-11 14:48:34  info  网页会话 cookie 已用 SSO 票根自动重建   ← 同一枚，成功
///   11-12 15:41:53  warn  SSO 票根已失效（…）
///   11-12 15:43:50  info  网页会话 cookie 已用 SSO 票根自动重建   ← 又成功
/// ```
/// 这类失败**两分钟就自己好了**（开机后网络未就绪、上游限流）。压 10 分钟等于让
/// 刚恢复的网络白等八分钟；而它一天会发生好几次，所以也不该落 `warn`
/// （见 `note_failure`）。
const TRANSIENT_BACKOFF: std::time::Duration = std::time::Duration::from_secs(2 * 60);

/// 上次换票失败距今是否还在退避窗口内（窗口长度按**上次失败是什么档**算）。
///
/// 锁中毒时返回 `false`（= 不在退避，继续推）：宁可多一次往返，也不要让一个坏
/// 锁把保活停掉 —— 那正是本次要修的那类静默失效。
fn backing_off(account_id: &str) -> bool {
    let now = std::time::Instant::now();
    let Ok(marks) = touch_marks().lock() else { return false };
    marks
        .get(&format!("fail:{account_id}"))
        .is_some_and(|(last, window)| now.duration_since(*last) < *window)
}

/// 记下「这次换票失败了」，让接下来的退避窗口安静下来。
fn mark_failed(account_id: &str, window: std::time::Duration) {
    let Ok(mut marks) = touch_marks().lock() else { return };
    marks.insert(format!("fail:{account_id}"), (std::time::Instant::now(), window));
}

/// 换票成功了：退避立刻解除，不必等窗口自然过期。
///
/// 这一条不是可有可无的优化：票根死过一段时间、用户重新登录之后，如果不主动清，
/// 剩下的退避时间会把「刚恢复的账号」白白压住最多 10 分钟。
fn clear_failed(account_id: &str) {
    let Ok(mut marks) = touch_marks().lock() else { return };
    marks.remove(&format!("fail:{account_id}"));
}

/// 账号里落盘的「上次推票根」时刻，距今够久了吗（跨进程的**真**判据）。
///
/// ── 为什么不看内存表 ────────────────────────────────────────
/// 网关每天都会重启（用户关电脑）。内存表随之清空，于是「上次什么时候推的」
/// 这件事只能从账号里读 —— 这就是 `ssoTgcPushedAt` 存在的理由。
///
/// `pushed_at <= 0`（字段缺失 / 坏值）一律判**该推**：宁可多一次 SSO 往返，也不要
/// 因为一个坏字段把保活彻底停掉 —— 那正是本次要修的那类静默失效。
pub fn due_for_roll(secret: &Value, now_ms: i64) -> bool {
    let pushed_at = credentials::tgc_pushed_at(secret);
    if pushed_at <= 0 {
        return true;
    }
    // 时钟回拨（用户改了系统时间 / 换机器恢复备份）时 `now - pushed` 会是负数：
    // 那也判「该推」—— 拿不准就别省这一次。
    let elapsed = now_ms.saturating_sub(pushed_at);
    elapsed < 0 || elapsed >= credentials::SSO_TGC_ROLL_INTERVAL_MS
}

/// **保活入口**：余额查询每跑一轮就问一次「该推票根了吗」，该推就推。
///
/// ── 它凭什么能保活（与早先 `touch` 的根本区别）───────────────
/// 调用点在 `credits::keepalive`，而那里是**余额查询的固定心跳**：网关的
/// `usage_query::spawn_sweeper` 每 10 秒扫一轮到期账号，每账号默认 60 秒查一次。
/// 所以这个函数每账号每分钟都会被问一次，**与 cookie 死活无关**。
///
/// 早先的 `touch` 挂在「cookie 读取成功」之后（`credits.rs` 的旧第 302 行）：
/// cookie 一死或用户不点余额，它就永远不被调用 —— 11-12 那次 15:43→23:02
/// 七个小时零调用，票根就这么闲置死了。
///
/// ── 为什么先 `store::record` 再判 ────────────────────────────
/// 判据（`ssoTgcPushedAt`）在账号里，而调用方手里通常只有一份 `Credential`
/// 快照（`Credential` 结构里没有这个字段，也不该有：它是运行状态，不是凭据）。
/// 一次本地 SQLite 读（走 RPC，百微秒级）换掉一次不必要的 SSO 往返，值得。
///
/// 读不到账号就**按参数里的 `credential` 兜底判**：这种情形只可能是账号刚被删，
/// 那时推不推都无所谓，直接推一次让链路尽快自愈。
pub async fn keepalive(
    ctx: &PluginCtx,
    account_id: &str,
    credential: &Credential,
    proxy: Option<&ResolvedProxy>,
) {
    if credential.sso_tgc.trim().is_empty() {
        return;
    }
    let now = crate::logging::now_ms();
    let due = match crate::store::record(ctx, super::PROVIDER_ID, account_id).await {
        Ok(record) => due_for_roll(&crate::store::secret_view(&record), now),
        // 读不到账号：拿不到落盘时刻，就按「上一轮大概刚推过」处理是不对的 ——
        // 那会让一个新账号永远不推。这里判「该推」，让链路尽快自愈。
        Err(_) => true,
    };
    if !due {
        return;
    }
    // 刚失败过就先别试（见 [`FAIL_BACKOFF`]）：`due` 在票根死掉后会恒为真，
    // 没有这一条就是每 30 秒一次的无休止重试。
    if backing_off(account_id) {
        return;
    }
    touch(ctx, account_id, credential, proxy);
}

/// **滚动续期**：把票根往前推一次（后台跑，不阻塞调用方）。
///
/// ── 它现在与 cookie 完全无关（2026-11-13 改，这是关键）─────────
/// 早先这个函数只从 `credits::fetch_renewing_with_state` 的「cookie 读取成功」
/// 分支里被调用 —— 也就是**只有 cookie 还活着、并且这次真的读到了明细**才推票根。
/// 那条路径在真实使用节奏下几乎不成立：
///   · 门户 cookie 是会话级的，机器休眠/浏览器不动它就死；
///   · 用户是「白天零星点几下余额、晚上关机」，不点的时候一次读数都没有。
/// 实测（11-12）从 15:43 重建成功到 23:02 票根被清，中间**一次 `touch` 都没发生**，
/// 票根闲置 7.3 小时后被服务端杀掉 —— 这就是「明明写了滚动续期却还是保不住」。
///
/// 现在它由**余额查询这条固定心跳**无条件驱动（见 `credits::keepalive`）：
/// 每账号每 60 秒一轮查询，每轮都会问一次「该推了吗」，判据是账号里落盘的
/// `ssoTgcPushedAt` 与 [`credentials::SSO_TGC_ROLL_INTERVAL_MS`]（8 分钟）。
/// cookie 死不死、明细读没读到，都不影响推票根 —— 票根本来就只需要第 1 跳。
///
/// ── 为什么只走第 1 跳（不是完整三跳）─────────────────────────
/// 推票根只需要「用票根换一次 ticket」这一步 —— 服务端就是在这一步重置闲置计时器、
/// 并（通常）下发新票根。后两跳（换会话、取 cftk）是 `renew` 的正事：会话**还活着**
/// 的时候去换一条新的，既白打两次门户请求，又会把用户手贴的那条 cookie 悄悄换成
/// 机器铸的。所以 `touch` 走 [`roll`]（一跳 + 只写票根），`renew` 才走 [`rebuild`]
/// （三跳 + 写 cookie）。
///
/// ── 三条纪律 ────────────────────────────────────────────────
///   1. **不阻塞调用方**：推票根失败不该拖慢或破坏这次余额读数；
///   2. **失败要可见一次**：早先失败只落 `verbose`，而 `verbose` **不落库**
///      （见 `logging::verbose`），于是「保活到底跑没跑」在库里查不到任何痕迹 ——
///      排查时只能靠猜。现在失败按小时节流落一行 `warn`，成功也落一行 `info`
///      （`票根已往前推`），两类都在库里，能直接对时间线；
///   3. **不覆盖坏值**：新票根只在形状合法（[`credentials::looks_like_tgc`]）
///      时才写回，见 [`persist`]。
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
            Ok(outcome) => {
                clear_failed(&account);
                outcome.report(&account);
            }
            Err(failed) => {
                mark_failed(&account, failed.backoff());
                note_failure(&account, &failed);
            }
        }
    });
}

/// 一次滚动续期的结果。
///
/// ── 为什么要把「换到了但没给新票根」单独分出来 ────────────────
/// 它不是失败：服务端在换票这一步已经重置了闲置计时器（滑动过期的「使用即续」
/// 那一半生效了）。把它和失败混在一起会让日志说反话 —— 明明保活成功，却写着
/// 「续期失败」。所以这里分三档，各自说各自的话。
#[derive(Clone, Copy, Debug)]
enum Rolled {
    /// 上游下发了新票根，已写回账号。
    Renewed,
    /// 换票成功但这次响应没带新票根（闲置窗口已重置，本地值保留）。
    Kept,
}

impl Rolled {
    /// 落日志。
    ///
    /// ── 为什么成功是 `info`、`Kept` 却只是 `verbose`（2026-11-13）────
    /// `Renewed` 是「保活的证据」：账号里的票根值真的换代了（`ssoTgcPushedAt`
    /// 同时前进），用户与排查者都需要在库里看到「这台机器在什么时候保过活」。
    /// `Kept` 只证明「换了一次票」，它每 8 分钟就会来一条 —— 落 `info` 会把日志表
    /// 灌满（约 180 条/天/账号），真正的失败反而被冲走。保活的证据在两种情形下
    /// 都由 `ssoTgcPushedAt` 承担（见 `accounts.rs` 的公开视图），不靠日志。
    fn report(self, account_id: &str) {
        let tail = crate::store::token_tail(account_id);
        match self {
            Rolled::Renewed => crate::logging::log(
                "[CodeArts]",
                &format!("SSO 票根已滚动续期并落盘（账号 {tail}）：网页会话可继续自动重建"),
            ),
            Rolled::Kept => verbose(&format!(
                "票根换票成功但本次未下发新票根（闲置窗口已被重置，保留原值；账号 {tail}）"
            )),
        }
    }
}

/// 只推票根：第 1 跳 + 写回新票根 + 记下推进时刻，**不碰 cookie**。
///
/// 与 [`rebuild`] 共用同一把单飞钥匙：两条路都会用票根换 ticket，并发跑会各拿到
/// 一枚「代为最新」的票根、互相把对方写旧。
async fn roll(
    ctx: &PluginCtx,
    account_id: &str,
    tgc: &str,
    proxy: Option<&ResolvedProxy>,
) -> Result<Rolled, Failed> {
    let key = format!("codearts-portal:{account_id}");
    let tgc = tgc.trim().to_string();
    let account = account_id.to_string();
    let ctx = ctx.clone();
    let proxy = proxy.cloned();
    // 与 `rebuild` 同一理由：`flight::run` 的 `Err` 只收得动 `String`，
    // 档位用 `tag` / `untag` 带过去（见那两处的说明）。
    let outcome: Result<Rolled, String> = crate::flight::run(&key, || async move {
        // 进锁后重读（同 `rebuild`）：前一个可能刚把票根写新了。
        let newest = freshest_tgc(&ctx, &account, &tgc).await;
        let client = match strict_client(proxy.as_ref()) {
            Some(client) => client,
            // 客户端建不起来是本地问题，跟票根无关 —— 归到形状档（不劝重登）。
            None => return Err(tag(&Failed::Shape("HTTP 客户端构造失败".to_string()))),
        };
        let (_, renewed) = match exchange(&client, &newest).await {
            Ok(found) => found,
            Err(failed) => return Err(tag(&failed)),
        };
        match renewed {
            // 换票成功但上游这次没带新票根 —— **不是失败**：滑动过期里「使用即续」
            // 这一半已经生效了（服务端闲置计时器被重置）。没有新值就不该覆盖旧值，
            // 否则等于把一枚好票根洗成空串。推进时刻照样要写：这次确实「用过」它。
            None => {
                persist(&ctx, &account, None, None).await;
                Ok(Rolled::Kept)
            }
            Some(renewed) => {
                persist(&ctx, &account, None, Some(&renewed)).await;
                Ok(Rolled::Renewed)
            }
        }
    })
    .await;
    outcome.map_err(|tagged| untag(&tagged))
}

/// 把「新 cookie / 新票根 / 推进时刻」写回账号 secret（字段全部可选）。
///
/// 与续期那条链同一纪律：**读最新记录、比较再写**（`store::save` 的 `expect` 语义），
/// 写不进去只记日志 —— cookie 本身是短命物，丢了下次还能再铸。
///
/// ── 为什么合并成一个函数（2026-11-13）─────────────────────────
/// 早先有两条几乎相同的写回路径：`persist`（cookie + 票根，`renew` 走）与
/// `persist_tgc`（只有票根，`touch` 走）。两者都漏了同一件事 —— **没记「推的时刻」**，
/// 于是节流只能靠内存，重启即失效（见 [`credentials::SSO_TGC_PUSHED_FIELD`] 的说明）。
/// 合并之后只有一条写路径，`ssoTgcPushedAt` 不可能在某个分支被忘掉。
///
/// ── 票根为什么要一起写（这是「保活」的实质）───────────────
/// 票根是**会话级**的（`Set-Cookie` 无 `Max-Age` / `Expires`，见模块头），
/// 但它**滑动过期**：换票这个动作本身就把服务端的闲置计时器重置了（见 [`Minted`]）。
/// 上游多数时候还会再下发一枚新值 —— 把新值写回去，本地这枚就不会是签发当时那一枚；
/// 完全不写回去的后果是它最多活过一个窗口就死，有效期列随即无声消失
/// （2026-11-11 实测的正是这个）。
///
/// `tgc` 为 `None`（响应里没带新票根）时**不覆盖**旧值 —— 上游某个响应省略这一项
/// 不该被当成「票根没了」而把账号里的值洗掉。但**推进时刻照样写**：这次换票确实
/// 用过它，服务端的闲置计时器确实被重置了，这一点必须反映到节流判据上。
async fn persist(
    ctx: &PluginCtx,
    account_id: &str,
    cookie: Option<&str>,
    tgc: Option<&str>,
) {
    let record = match crate::store::record(ctx, super::PROVIDER_ID, account_id).await {
        Ok(record) => record,
        Err(error) => {
            verbose(&format!("票根未能落盘（读账号失败）：{}", error.message));
            return;
        }
    };
    let expect = crate::store::secret_view(&record);
    let mut fields: Vec<(&str, Value)> = Vec::new();
    if let Some(cookie) = cookie.map(str::trim).filter(|value| !value.is_empty()) {
        fields.push((
            credentials::SESSION_COOKIE_FIELD,
            Value::String(cookie.to_string()),
        ));
    }
    let renewed_tgc = tgc.map(str::trim).filter(|value| !value.is_empty());
    if let Some(tgc) = renewed_tgc {
        fields.push((credentials::SSO_TGC_FIELD, Value::String(tgc.to_string())));
    }
    // 推进时刻**总是**写：换票这一步已经发生过（调用方只在拿到 ticket 之后才来），
    // 服务端的「最后一次使用」已经被刷新到这个时刻。
    fields.push((
        credentials::SSO_TGC_PUSHED_FIELD,
        Value::from(crate::logging::now_ms()),
    ));
    let next = crate::store::with_fields(&expect, &fields);
    match crate::store::save(ctx, account_id, &expect, &next).await {
        // 成功不在这里吭声：调用方（`roll` / `rebuild`）已经有一条带账号尾号、
        // 落 `info` 的日志了。同一件事说两遍只会让日志更难扫。
        Ok(true) => {}
        Ok(false) => verbose("票根未能落盘：账号在换票期间被改动（下一轮再推）"),
        Err(error) => verbose(&format!("票根未能落盘：{}", error.message)),
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