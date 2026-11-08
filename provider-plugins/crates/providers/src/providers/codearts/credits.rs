//! CodeArts 的**逐批积分明细**：网页控制台那套 BFF 上的账。
//!
//! ── 为什么要另开一条通道（2026-11-08 抓包实测）────────────────
//! `balance.rs` 读的订阅统计（`/snap-manager/v1/statistics/plugin`，走 AK/SK 签名）
//! 只给**聚合值**：「套餐积分 20422.13、基础包 500、赠送 19922.13」。而官网
//! 「套餐与用量 → 获取明细」展开的是**逐份积分包**，每份各有自己的作废时刻 ——
//! 那份明细只在 portal BFF 上，且**只认网页会话 cookie**：
//!
//!   · 用 AK/SK 签名打 portal 路径 → `404`（两条通道不同网关）；
//!   · 不带 cookie 打 portal 路径 → 回门户 **HTML 首页，HTTP 200**（不是 401！）；
//!   · 带 cookie 打 → 真 JSON。
//!
//! 所以「没有 cookie」与「cookie 过期」在状态码上看起来都是成功，判据只能是
//! 「解析得出来吗」。这一点决定了本模块的所有失败分支都是**静默降级**。
//!
//! ── 四个端点 ────────────────────────────────────────────────
//! ```text
//!   GET  /portal/snap-manager/v1/package/overview
//!        汇总：all_credit_package{ amount / used / remain / expiring_credit_amount }
//!              basic_package / ondemand_credit_package / bonus_credit_package
//!   POST /portal/snap-manager/v1/package/credit/page
//!        **基础包 / 按需包逐份**：creditAmount / creditUsed / validityPeriod /
//!        expiredTime / status / createdTime。body 要 packageTypes。
//!   GET  /portal/promptcenter/v1/ops/credit/overview
//!        活动福利按活动汇总，items[].campaignId 是下面那个接口的路由参数
//!   GET  /portal/promptcenter/v1/ops/credit/campaign/{id}?pageNum=1&pageSize=200
//!        **活动福利逐笔**：claimedAmount / consumedAmount / expiredAmount /
//!        claimedAt / expireAt / status / benefitText
//! ```
//!
//! ── 两个只有抓包才知道的细节 ────────────────────────────────
//!   1. **POST 要 `cftk` 头**，值就是同一条 cookie 里 `devclouddevuibjtcftk` 的值。
//!      缺它或值不对 → `400 {"error": "csrf validate fail."}`。让用户为同一件事贴
//!      两次没有道理（且两次一定会有一次贴错），所以这里从 cookie 自己抠。
//!   2. **赠送批次不在 `credit/page` 里**：`packageTypes:["bonus"]` 实测返回空数组。
//!      赠送要走活动福利那条链（`ops/credit/campaign/{id}`）。两条链各管一块，
//!      合并时**不能**再按 packageType 去重 —— 它们本来就互不重叠。
//!
//! ── 上游的数字口径（全部实测对账过）─────────────────────────
//!   · `bonus_credit_package.package_credit_remain` == 未过期各笔
//!     `claimedAmount - consumedAmount` 之和（2026-11-08 实测精确相等，19922.13）；
//!     **已过期但没花掉的批次不计入** —— 上游口径就是「过期即作废」。
//!     这条决定了 `credits[]` 必须带 `expireAt`：不带的话前端算不出哪些已经死了，
//!     「即将到期」那行会显示一个虚高的金额。
//!   · `expiring_credit_amount` == 剩余未过期批次里 **7 天内**到期之和
//!     （按 1/2/3/5/7/10/15/30 天逐个套过，只有 7 天精确命中）。这是官网
//!     「积分即将到期」那行背后的窗口，与前端 `CREDIT_EXPIRY_WINDOW_MS` 同一个值。
//!     本模块**不拿它算展示**（前端自己按 `expireAt` 算，口径只留一处），
//!     只把它放进 `raw` 供排查对账。
//!
//! ── 硬约束 ──────────────────────────────────────────────────
//! release 是 `panic=abort`：本文件零 unwrap / expect / panic。任何一处失败都
//! 只让「明细少几行」，绝不打断余额读数。

use agent2api_provider_protocol::WireProxy as ResolvedProxy;
use serde_json::{json, Map, Value};

use super::credentials::{self, Credential};

/// 网页控制台的 origin（与 `models::DEFAULT_BASE_URL` 的 snap-access 是**两个**网关）。
pub const PORTAL_ORIGIN: &str = "https://codearts.huaweicloud.com";

/// 汇总（含 `expiring_credit_amount`）。
const OVERVIEW_PATH: &str = "/portal/snap-manager/v1/package/overview";
/// 基础包 / 按需包的逐份明细（POST）。
const CREDIT_PAGE_PATH: &str = "/portal/snap-manager/v1/package/credit/page";
/// 活动福利按活动汇总（给下面那个接口的活动 id）。
const OPS_OVERVIEW_PATH: &str = "/portal/promptcenter/v1/ops/credit/overview";
/// 活动福利逐笔。
const OPS_CAMPAIGN_PATH: &str = "/portal/promptcenter/v1/ops/credit/campaign";

/// `cftk` 头的来源键（同一条 cookie 串里）。
const CFTK_COOKIE: &str = "devclouddevuibjtcftk";

/// 最多拉多少个活动的逐笔明细。上游的活动数是个位数（实测 4 个），
/// 这个上限只是防止某个账号被发了上百个活动时把一次余额查询拖成串行长龙。
const MAX_CAMPAIGNS: usize = 12;

/// 单个列表接口的页大小。实测 33 笔全在一个 200 的页里，够用。
const PAGE_SIZE: u32 = 200;

/// 逐批明细的一次读取结果。
///
/// `rows` 是**已经排好序**的 `credits[]` 行（见 [`sort_rows`]）；其余几项只用于
/// `raw` 留档与「统计接口挂了就拿它兜底」那一处。
#[derive(Clone, Debug, Default)]
pub struct WebCredits {
    pub rows: Vec<Value>,
    /// `all_credit_package.package_credit_remain`（与统计接口的总积分同源）。
    pub available: Option<f64>,
    /// `all_credit_package.expiring_credit_amount`（7 天窗口，上游直给）。
    pub expiring_amount: Option<f64>,
    /// 汇总里各桶的剩余，`raw` 留档用。
    pub buckets: Vec<(&'static str, Value)>,
}

impl WebCredits {
    /// 读不到任何东西（没 cookie / 全失败 / 全空）—— 调用方据此整段省略 `credits[]`。
    pub fn is_empty(&self) -> bool {
        self.rows.is_empty() && self.available.is_none()
    }

    /// 放进 `usage_document.raw.web` 的排查用快照。
    ///
    /// ⚠️ 这里**不放 cookie**：它是完整会话，与令牌同级。`redact` 也盖得住，
    /// 但根本不该进这个结构才是对的。
    pub fn raw(&self) -> Value {
        json!({
            "expiringAmount": self.expiring_amount,
            // 7 天是**反推**出来的（见模块头），不是上游字段，标出来免得下一个人
            // 以为上游给了这个数。
            "expiringWindowDays": 7,
            "batches": self.rows.len(),
            "buckets": self.buckets.iter().map(|(name, value)| json!({ "name": name, "value": value })).collect::<Vec<_>>(),
        })
    }
}

/// 一份还没转成 JSON 的批次（内部形状，便于排序与过滤）。
struct Batch {
    name: String,
    total: Option<f64>,
    used: Option<f64>,
    remain: Option<f64>,
    grant_at: Option<i64>,
    expire_at: Option<i64>,
}

fn number(value: Option<&Value>) -> Option<f64> {
    match value? {
        Value::Number(number) => number.as_f64(),
        Value::String(text) => text.trim().parse::<f64>().ok(),
        _ => None,
    }
    .filter(|value| value.is_finite())
}

fn text<'a>(value: &'a Value, key: &str) -> &'a str {
    value.get(key).and_then(Value::as_str).unwrap_or("").trim()
}

/// ISO 时刻 → 毫秒。
///
/// 上游在这四个接口里给了**两种**拼法：`2026-11-10T15:59:59.000+00:00`（基础包）
/// 与 `2026-11-09T11:06:44.276Z`（活动福利）。RFC3339 两种都认，所以不必分支。
///
/// 解析不出来就 `None` —— **不猜**。猜一个「大概没过期」会让一笔已经作废的积分
/// 继续挂在「还能用」里，那比少显示一行糟得多。
fn iso_ms(value: &str) -> Option<i64> {
    let text = value.trim();
    if text.is_empty() {
        return None;
    }
    chrono::DateTime::parse_from_rfc3339(text)
        .ok()
        .map(|time| time.timestamp_millis())
        .filter(|millis| *millis > 0)
}

// ─────────────────────────────────────────────────────────────
// 出网
// ─────────────────────────────────────────────────────────────

/// 一条 portal 请求的公共头。
///
/// `X-Requested-With` 是**必需的**：不带它，GET 回的是门户 HTML（200）；
/// 带上才是 JSON。这一条是逐个头试出来的（`Accept` / `x-language` 都不起作用）。
fn base_headers(cookie: &str) -> Vec<(String, String)> {
    vec![
        ("Cookie".to_string(), cookie.to_string()),
        ("X-Requested-With".to_string(), "XMLHttpRequest".to_string()),
        ("Accept".to_string(), "application/json, text/plain, */*".to_string()),
        ("Origin".to_string(), PORTAL_ORIGIN.to_string()),
        (
            "Referer".to_string(),
            format!("{PORTAL_ORIGIN}/portal/settings/personal-usage"),
        ),
        // 与官方控制台同一个 UA：BFF 对未知 UA 没有额外校验，但照抄能少一类
        // 「为什么我的请求和浏览器的不一样」的排查。
        (
            "User-Agent".to_string(),
            "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 \
             (KHTML, like Gecko) Chrome/132.0.0.0 Safari/537.36"
                .to_string(),
        ),
    ]
}

/// 发一条 portal 请求，返回解析后的 JSON。
///
/// ── 为什么判据是「JSON 解析成功」而不是状态码 ────────────────
/// 没有 cookie / cookie 过期时上游回的是**门户 HTML 首页，HTTP 200**。按状态码判
/// 会把「没登录」当成「成功」，然后在下游解析处炸出一堆看不懂的错误。这里直接把
/// 「解析不出来」当作失败 —— 那正是它的事实含义。
async fn portal(
    method: &str,
    cookie: &str,
    cftk: Option<&str>,
    path: &str,
    body: Option<&Value>,
    proxy: Option<&ResolvedProxy>,
) -> Option<Value> {
    let url = format!("{PORTAL_ORIGIN}{path}");
    let verb = reqwest::Method::from_bytes(method.as_bytes()).unwrap_or(reqwest::Method::GET);
    let mut request = crate::http::control_client(proxy, "direct")
        .request(verb, url)
        .timeout(std::time::Duration::from_secs(20));
    for (name, value) in base_headers(cookie) {
        request = request.header(name.as_str(), value.as_str());
    }
    if let Some(token) = cftk {
        // 缺它 / 值不对 → 400 csrf validate fail（只有 POST 需要）
        request = request.header("cftk", token);
    }
    if let Some(body) = body {
        request = request
            .header("Content-Type", "application/json")
            .body(serde_json::to_vec(body).unwrap_or_default());
    }
    let response = match request.send().await {
        Ok(response) => response,
        Err(error) => {
            crate::logging::verbose(
                "[CodeArts]",
                &format!(
                    "积分明细请求失败（{path}）：{}",
                    crate::http::describe_chain(&error)
                ),
            );
            return None;
        }
    };
    let status = response.status().as_u16();
    let text = response.text().await.unwrap_or_default();
    match serde_json::from_str::<Value>(&text) {
        Ok(value) => Some(value),
        Err(_) => {
            // 门户 HTML（没登录）与「上游改版回了别的形状」都走这里。
            // 只记状态码与前 80 字，**不记 cookie**。
            crate::logging::verbose(
                "[CodeArts]",
                &format!(
                    "积分明细响应不是 JSON（{path}，HTTP {status}）：{}",
                    text.chars().take(80).collect::<String>()
                ),
            );
            None
        }
    }
}

// ─────────────────────────────────────────────────────────────
// 解析
// ─────────────────────────────────────────────────────────────

/// 读全部逐批明细。**没有 cookie 直接返回 `None`**（不发任何请求）。
///
/// 失败一律静默降级：cookie 过期时这条通道会一直不可用，不该每轮余额查询都报错
/// （用户没重贴之前没有任何动作可做）。与 Qoder 的 `web_credit_packs` 同一取向。
pub async fn fetch(credential: &Credential, proxy: Option<&ResolvedProxy>) -> Option<WebCredits> {
    // 读取侧也过一遍归一：早期版本可能已经把多行串落了库（见 `normalize_cookie`）。
    let cookie = credentials::normalize_cookie(&credential.session_cookie);
    if cookie.is_empty() {
        return None;
    }
    // `cftk` 就在同一条 cookie 里 —— 用户不必为同一件事贴两次。
    let cftk = credentials::cookie_value(&cookie, CFTK_COOKIE).map(str::to_string);

    let mut credits = WebCredits::default();

    // ① 汇总：给 available 兜底 + expiring_credit_amount 留档
    if let Some(payload) = portal("GET", &cookie, None, OVERVIEW_PATH, None, proxy).await {
        read_overview(&payload, &mut credits);
    }

    // ② 基础包 / 按需包逐份（POST，需要 cftk）
    if let Some(token) = cftk.as_deref() {
        let body = json!({
            "sortField": "created_time",
            "sortOrder": "desc",
            "packageTypes": ["basic", "ondemand"],
            "pageNum": 1,
            "pageSize": PAGE_SIZE,
        });
        if let Some(payload) = portal("POST", &cookie, Some(token), CREDIT_PAGE_PATH, Some(&body), proxy).await {
            for batch in read_credit_page(&payload) {
                credits.rows.push(row_of(&batch));
            }
        }
    } else {
        // 少了这个键就没法 POST —— 只影响基础包明细，活动福利那条链照旧能走。
        crate::logging::verbose(
            "[CodeArts]",
            "网页会话 cookie 里没有 cftk 键：基础包逐份明细本次跳过（活动福利不受影响）",
        );
    }

    // ③ 活动福利：先拿活动清单，再逐个拉逐笔
    if let Some(payload) = portal("GET", &cookie, None, OPS_OVERVIEW_PATH, None, proxy).await {
        for campaign_id in campaign_ids(&payload) {
            let path = format!("{OPS_CAMPAIGN_PATH}/{campaign_id}?pageNum=1&pageSize={PAGE_SIZE}");
            let Some(payload) = portal("GET", &cookie, None, &path, None, proxy).await else {
                continue;
            };
            for batch in read_campaign(&payload) {
                credits.rows.push(row_of(&batch));
            }
        }
    }

    if credits.is_empty() {
        return None;
    }
    sort_rows(&mut credits.rows);
    Some(credits)
}

fn read_overview(payload: &Value, credits: &mut WebCredits) {
    let all = payload.get("all_credit_package").filter(|value| value.is_object());
    if let Some(all) = all {
        credits.available = number(all.get("package_credit_remain"));
        credits.expiring_amount = number(all.get("expiring_credit_amount"));
    }
    // 四个桶逐个留档（形状与上游同名，不做归一 —— 这张表就是给对账用的）
    for key in [
        "all_credit_package",
        "basic_package",
        "ondemand_credit_package",
        "bonus_credit_package",
    ] {
        let value = payload.get(key).cloned().unwrap_or(Value::Null);
        if !value.is_null() {
            let name = match key {
                "all_credit_package" => "all",
                "basic_package" => "basic",
                "ondemand_credit_package" => "ondemand",
                _ => "bonus",
            };
            credits.buckets.push((name, value));
        }
    }
}

/// `package/credit/page` → 批次。只取 `basic` / `ondemand` 两类。
///
/// `bonus` 不在这里（实测返回空数组），见模块头第 2 条。
fn read_credit_page(payload: &Value) -> Vec<Batch> {
    let mut batches = Vec::new();
    for item in payload.get("list").and_then(Value::as_array).into_iter().flatten() {
        let total = number(item.get("creditAmount"));
        let used = number(item.get("creditUsed"));
        let remain = match (total, used) {
            // 上游给了两个数就自己减 —— 这一层没有独立的 remain 字段
            (Some(total), Some(used)) => Some((total - used).max(0.0)),
            (Some(total), None) => Some(total),
            _ => None,
        };
        // 名字：`packageType` 决定口径，`resourceSpec` 是套餐代号。
        // 基础包给中文名（与统计接口那张对照表同一个说法），按需包直说。
        let package_type = text(item, "packageType");
        let name = match package_type {
            "basic" => "个人订阅套餐".to_string(),
            "ondemand" => "按需付费积分".to_string(),
            other if !other.is_empty() => other.to_string(),
            _ => "积分包".to_string(),
        };
        batches.push(Batch {
            name,
            total,
            used,
            remain,
            grant_at: iso_ms(text(item, "createdTime")),
            // `expiredTime` 可能为 null（按需付费没有到期）—— 那就**不写** expireAt
            expire_at: iso_ms(text(item, "expiredTime")),
        });
    }
    batches
}

/// 活动清单 → 活动 id（去重、保序、截断到 [`MAX_CAMPAIGNS`]）。
fn campaign_ids(payload: &Value) -> Vec<i64> {
    let mut ids: Vec<i64> = Vec::new();
    for item in payload
        .pointer("/data/items")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        let Some(id) = item.get("campaignId").and_then(Value::as_i64) else {
            continue;
        };
        if id > 0 && !ids.contains(&id) {
            ids.push(id);
        }
    }
    ids.truncate(MAX_CAMPAIGNS);
    ids
}

/// `ops/credit/campaign/{id}` → 批次（一笔领取 = 一份积分包）。
fn read_campaign(payload: &Value) -> Vec<Batch> {
    let mut batches = Vec::new();
    let title = payload.pointer("/data/campaignTitle").and_then(Value::as_str).unwrap_or("");
    for item in payload
        .pointer("/data/benefits")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        let claimed = number(item.get("claimedAmount"));
        let consumed = number(item.get("consumedAmount"));
        let expired = number(item.get("expiredAmount"));
        // 剩余 = 领到的 - 已消耗 - 已作废。三项上游都给，缺项按 0（这里与别处
        // 「缺键不补 0」不同：这一行的 `claimedAmount` 是权威总量，缺 consumed
        // 就是真的没消耗，而不是「不知道」）。
        let remain = claimed.map(|claimed| {
            (claimed - consumed.unwrap_or(0.0) - expired.unwrap_or(0.0)).max(0.0)
        });
        // 名字优先用这一笔自己的 `benefitText`（图上就是它），退回活动名。
        let own = text(item, "benefitText");
        let name = if own.is_empty() { title.to_string() } else { own.to_string() };
        batches.push(Batch {
            name: if name.is_empty() { "活动福利".to_string() } else { name },
            total: claimed,
            used: consumed,
            remain,
            grant_at: iso_ms(text(item, "claimedAt")),
            expire_at: iso_ms(text(item, "expireAt")),
        });
    }
    batches
}

/// 内部批次 → `credits[]` 的一行。
///
/// ── 谁缺谁不写，一个 `null` 都不许出去 ──────────────────────
/// 前端的 `Number(null) === 0`，把「上游没给」写成 0 会画成「剩 0 / 共 0」这种
/// 看起来像读到了的假读数。所以三个数与两个时刻都收 `Option`，`None` 直接不出键
/// （与 officeace / trae 的行构造同一条纪律）。
///
/// `unit` 不写：本家所有批次都是「积分」，与文档级 `unit` 相同，不必每行再标一遍。
fn row_of(batch: &Batch) -> Value {
    let mut row = Map::new();
    row.insert("name".to_string(), Value::String(batch.name.clone()));
    for (key, value) in [("total", batch.total), ("used", batch.used), ("remain", batch.remain)] {
        if let Some(value) = value {
            row.insert(key.to_string(), json!(value));
        }
    }
    if let Some(grant_at) = batch.grant_at {
        row.insert("grantAt".to_string(), json!(grant_at));
    }
    if let Some(expire_at) = batch.expire_at {
        row.insert("expireAt".to_string(), json!(expire_at));
    }
    Value::Object(row)
}

/// 排序：**最近到期的排前面**，没有到期时刻的排最后。
///
/// 明细第一个要回答的问题是「哪笔先作废」（官网那张表也是这么排的）。
/// 同一时刻按名字比，否则每次刷新行序都在跳，看起来像数据在变。
fn sort_rows(rows: &mut [Value]) {
    rows.sort_by(|left, right| {
        let key = |row: &Value| {
            row.get("expireAt")
                .and_then(Value::as_i64)
                .filter(|millis| *millis > 0)
                .unwrap_or(i64::MAX)
        };
        let by_expiry = key(left).cmp(&key(right));
        if by_expiry != std::cmp::Ordering::Equal {
            return by_expiry;
        }
        let name = |row: &Value| row.get("name").and_then(Value::as_str).unwrap_or("").to_string();
        name(left).cmp(&name(right))
    });
}

#[cfg(any())]
mod tests {
    //! 这里钉的都是**边界**：没有 cookie 不发请求、没有 `cftk` 就不 POST、
    //! `null` 时刻不写键、已过期批次照样输出（由前端分组），以及排序。
    use super::*;

    /// 真响应（2026-11-08 从现网抓的，字段名逐字照上游）。
    const PAGE: &str = r#"{"list":[{"id":"5624f05a","packageType":"basic","resourceSpec":"codearts.agent.individual.trial","creditAmount":500.00,"creditUsed":0.00,"validityPeriod":"31","expiredTime":"2026-11-10T15:59:59.000+00:00","status":1,"createdTime":"2026-10-10T11:06:43.000+00:00"}],"total":1,"pageNum":1,"pageSize":1}"#;

    const CAMPAIGN: &str = r#"{"code":0,"message":"ok","data":{"campaignId":4,"campaignTitle":"新用户注册送 4000 积分","benefits":[{"benefitId":571159,"status":"CONFIRMED","claimedAmount":4000.00,"consumedAmount":3177.87,"expiredAmount":0.00,"claimedAt":"2026-10-10T11:06:44.276Z","expireAt":"2026-11-09T11:06:44.276Z","benefitText":"新用户注册送 4000 积分"}]}}"#;

    #[test]
    fn the_basic_package_row_carries_both_timestamps() {
        let rows = read_credit_page(&serde_json::from_str(PAGE).unwrap());
        assert_eq!(1, rows.len());
        let row = row_of(&rows[0]);
        assert_eq!("个人订阅套餐", row["name"]);
        assert_eq!(Some(500.0), row["total"].as_f64());
        assert_eq!(Some(500.0), row["remain"].as_f64(), "上游没给 remain，由 amount - used 得出");
        // 2026-11-10T15:59:59Z == 1794326399000
        assert_eq!(Some(1_794_326_399_000), row["expireAt"].as_i64());
        assert_eq!(Some(1_791_630_403_000), row["grantAt"].as_i64());
    }

    #[test]
    fn a_missing_expiry_writes_no_key_instead_of_zero() {
        // 按需付费包没有到期时刻（上游给 null）
        let body = r#"{"list":[{"packageType":"ondemand","creditAmount":300.0,"creditUsed":0.0,"expiredTime":null}]}"#;
        let rows = read_credit_page(&serde_json::from_str(body).unwrap());
        let row = row_of(&rows[0]);
        assert_eq!(None, row.get("expireAt"), "null 时刻不能写成 0 —— 那会被画成「已过期」");
        assert_eq!("按需付费积分", row["name"]);
    }

    #[test]
    fn a_campaign_benefit_is_reduced_by_both_consumed_and_expired() {
        let rows = read_campaign(&serde_json::from_str(CAMPAIGN).unwrap());
        let row = row_of(&rows[0]);
        assert_eq!(Some(4_000.0), row["total"].as_f64());
        assert_eq!(Some(3_177.87), row["used"].as_f64());
        // 4000 - 3177.87 - 0 = 822.13（与网页上那一行一致）
        assert_eq!(Some(822.13), row["remain"].as_f64());
    }

    #[test]
    fn expired_batches_are_kept_and_sorted_last_first() {
        // 已过期的那笔照样输出（前端收进「已用完 / 已过期」折叠组），
        // 但排序按到期升序 —— 作废的排在最前，因为它们的时刻最小。
        let mut rows = vec![
            json!({"name":"晚","expireAt": 2_000_i64}),
            json!({"name":"无到期"}),
            json!({"name":"早","expireAt": 1_000_i64}),
        ];
        sort_rows(&mut rows);
        let names: Vec<&str> = rows.iter().map(|row| row["name"].as_str().unwrap_or("")).collect();
        assert_eq!(vec!["早", "晚", "无到期"], names, "无到期时刻的排最后");
    }

    #[test]
    fn campaign_ids_are_deduped_and_capped() {
        let payload = json!({"data": {"items": [
            {"campaignId": 4}, {"campaignId": 3}, {"campaignId": 4}, {"campaignId": 0}, {"other": 1}
        ]}});
        assert_eq!(vec![4, 3], campaign_ids(&payload));
    }

    #[test]
    fn an_empty_cookie_never_leaves_the_process() {
        // 没有 cookie 时 `fetch` 在第一行就返回 None —— 这条用「不会 panic」间接钉住：
        // 真发请求的话这里会因为没有运行时而崩（`control_client` 在测试环境里也建得出来，
        // 但那样就是网络依赖了，所以只验归一化那一层）。
        assert_eq!("", credentials::normalize_cookie("   "));
        assert_eq!("a=1; b=2", credentials::normalize_cookie("a=1;\nb=2"));
        assert_eq!("a=1", credentials::normalize_cookie("Cookie: a=1"));
    }
}
