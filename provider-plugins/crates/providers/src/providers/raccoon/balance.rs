//! 小浣熊的余额 / 积分与订阅查询（移植来源 `account-balance.mjs` 的
//! `queryPoints` / `querySubscription`）。
//!
//! ── 上游长什么样（逐条对照源实现）─────────────────────────────
//!   - 余额：`GET {mainOrigin}/api/web/points/v1/balance`
//!     头 `Authorization: Bearer <token>`，响应 `{code, message?, data}`
//!     （`code` 非 0 视为失败，与源实现 `requestJson` 的判定一致）；
//!     `data.available_points` 是可用总量，另有四个钱包字段
//!     （`daily_points` / `monthly_points` / `reward_points` / `topup_points`）。
//!   - 订阅：`GET {mainOrigin}/api/web/auth/v1/entitlement_info`
//!     `data.office` / `data.code` 两套权益，各带 `pro_enable` / `active_plan`
//!     / `pro_expired_time`。
//!
//! ── 域名与 `RACCOON_MAIN_SITE_URL`（别和 `RACCOON_AUTH_API_BASE` 搞混）──
//! 源实现读的是 **`RACCOON_MAIN_SITE_URL`**（默认 `https://xiaohuanxiong.com`），
//! 而本项目既有的 `RACCOON_AUTH_API_BASE`（见 `raccoon/mod.rs`）是**鉴权接口**
//! 的完整前缀（`{origin}/api/web/auth/v1`）。两者默认指向同一个站点，但覆盖语义
//! 不同：本文件只复用主站 origin（`RACCOON_MAIN_SITE_URL`），**不从
//! `RACCOON_AUTH_API_BASE` 反推 origin** —— 那样会在用户只改了鉴权域（指向
//! 内网镜像）时把余额请求也一起改道，而这两条链路在源实现里是两个独立配置点。
//!
//! ── 出网代理（核对结论）──────────────────────────────────────
//! 源实现的 `queryPoints` 收到 `client` 参数但**从不使用**它（函数体内直接
//! `fetch`），余额与订阅都是**裸 fetch**，不走它自己的上游客户端。本网关里
//! 「裸 fetch」对应的是直连 —— 即 `send_raw(..., proxy = None, ...)`。
//! 这与刷新鉴权接口的既有取舍同一理由（见 `credentials::call_refresh_api` 的
//! 注释）：账号级代理是给转发（流式长请求）准备的出口，而主站域与 LLM 域是
//! 两个不同的站点。**不做**「顺手挂上账号代理」的改动，那会与源实现的行为分叉
//! （源实现里这条链路是直连的）。
//!
//! ── 超时 ────────────────────────────────────────────────────
//! 20 秒（源实现 `REQUEST_TIMEOUT_MS = 20_000`）。必须显式设：`egress` 的默认
//! read_timeout 是 600 秒（给 SSE 长连接留的），不设总超时的话一个挂住的余额
//! 请求能让前端的批量查询转圈十分钟。
//!
//! ── 硬约束 ──────────────────────────────────────────────────
//! release 是 `panic=abort`：零 unwrap/expect/panic，取值一律走 Option 链。

use serde_json::{json, Map, Value};

use agent2api_provider_sdk::PluginCtx;

use crate::beijing;
use crate::errors::GatewayError;
use crate::http::send_raw;

use super::credentials;

/// 余额 / 订阅接口的请求超时（源实现 `REQUEST_TIMEOUT_MS`）。
/// 新手任务的首次登录奖励探测（`onboarding.rs`）沿用同一超时。
pub(super) const REQUEST_TIMEOUT_MS: u64 = 20_000;

/// 默认主站 origin（源实现 `apiOrigin()` 的兜底值）
const DEFAULT_MAIN_SITE_URL: &str = "https://xiaohuanxiong.com";

/// 钱包字段 → 展示名（逐字照抄源实现 `WALLET_LABELS`）。
///
/// 顺序即展示顺序：源实现用 `Object.entries(WALLET_LABELS)` 遍历，JS 对象的
/// 字符串键按插入序枚举，所以这里的数组顺序必须与源实现的对象字面量一致。
const WALLET_LABELS: &[(&str, &str)] = &[
    ("daily_points", "每日积分"),
    ("monthly_points", "每月会员积分"),
    ("reward_points", "奖励积分"),
    ("topup_points", "充值积分"),
    ("enterprise_topup_points", "企业加油包积分"),
];

/// 主站 origin（`RACCOON_MAIN_SITE_URL` 可覆盖；去空白与末尾斜杠）。
pub(super) fn main_site_url() -> String {
    std::env::var("RACCOON_MAIN_SITE_URL")
        .ok()
        .map(|value| value.trim().trim_end_matches('/').to_string())
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| DEFAULT_MAIN_SITE_URL.to_string())
}

/// 查询某账号的余额 / 积分（归一化形状见 `ProviderAdapter::query_usage` 的文档）。
///
/// ── 订阅是「尽力而为」而不是必需 ──────────────────────────────
/// 源实现把余额与订阅拆成两个接口（`/api/accounts/balances` 与
/// `/api/accounts/subscriptions`），余额那条**只查积分**。这里合成一次调用，
/// 但订阅的失败**不影响余额结果**（`subscription` 给 null 并往 `raw` 里记一条
/// 原因）：订阅是附加信息，为了它把整个面板变成红色的「查询失败」得不偿失 ——
/// 与源实现给 AutoClaw 的 `points/expiring` 兜底同一取向。
pub(super) async fn query_usage(ctx: &PluginCtx, account_id: &str) -> Result<Value, GatewayError> {
    let credentials = credentials::snapshot(ctx, account_id).await?;
    if credentials.token.trim().is_empty() {
        return Err(GatewayError::with_status(
            401,
            "该账号没有可用凭证，无法查询积分",
        ));
    }
    let origin = main_site_url();
    let mut org_headers = Vec::new();
    super::headers::append_org(&mut org_headers, &credentials.org_code)
        .map_err(|message| GatewayError::with_status(400, message))?;
    let points = request_json(
        &origin,
        "/api/web/points/v1/balance",
        &credentials.token,
        &org_headers,
    )
    .await?;
    if !points.is_object() {
        return Err(GatewayError::with_status(
            502,
            "积分余额响应结构异常（期望一个 JSON 对象）",
        ));
    }

    let available = to_int(points.get("total_chargeable_points"))
        .or_else(|| to_int(points.get("available_points")));
    let wallets: Vec<Value> = WALLET_LABELS
        .iter()
        .filter_map(|(key, display_name)| {
            let balance = to_int(points.get(*key))?;
            Some(json!({
                "type": key,
                "displayName": display_name,
                "balance": balance,
            }))
        })
        .collect();

    // 逐份明细（账号页「积分明细」弹层的输入）：与上面 `wallets` 走**同一张
    // `WALLET_LABELS`、同一口径**（`to_int` 读不到数值就跳过这一行）—— 字段表只有
    // 一份，两处各抄一份迟早会漂。余额接口只给**池子级**的数（没有面额 / 已用 /
    // 到期时刻），所以每行只有 `name` 与 `remain`：缺的键**不写**（写 0 会让界面
    // 把「没读到」画成「只剩 0」）。到期时刻在下方的批次行里补（来自收入账单）。
    // 「每日积分」**不标 `daily`**：本文件与同目录其它文件里都没有「这份余额按天
    // 重置」的依据（每日积分走的是签到链路按天发放的那笔，不是余额清零）；缺一个
    // 标记只是少一句提示，编一个会让人以为余额明天回满。
    let credits: Vec<Value> = WALLET_LABELS
        .iter()
        .filter_map(|(key, display_name)| {
            let balance = to_int(points.get(*key))?;
            Some(json!({
                "name": display_name,
                "remain": balance,
            }))
        })
        .collect();

    // 批次明细（带到期时刻）：收入账单里每一笔「发放」都带 `expire_at` ——
    // 客户端「积分明细」页显示的「有效期至 11-11 24:00」就是它。追加进
    // `credits[]` 之后，账号页「有效期」列才能算出「最先到期的那笔还有多久」；
    // 没有它时这一列对小浣熊恒为空（余额只有池子级的数，没有时间）。
    //
    // 读失败静默跳过：余额本身已经在手，明细不该把它一起拖垮。
    let mut credits = credits;
    if let Ok(items) = income_bills(&origin, &credentials.token, &org_headers).await {
        // `daily_points` 是「每日积分」池的余额 —— `grant_packs` 拿它按到期日
        // 升序摊出每批还剩多少（见那个函数的说明）
        credits.extend(grant_packs(&items, to_int(points.get("daily_points"))));
    }

    // 订阅失败只记原因，不阻断余额展示（见上）
    let (subscription, subscription_error) = match request_json(
        &origin,
        "/api/web/auth/v1/entitlement_info",
        &credentials.token,
        &org_headers,
    )
    .await
    {
        Ok(data) => (normalize_subscription(&data), Value::Null),
        Err(error) => (Value::Null, Value::String(error.message)),
    };

    let mut raw = Map::new();
    raw.insert("points".to_string(), points);
    if !subscription_error.is_null() {
        raw.insert("subscriptionError".to_string(), subscription_error);
    }
    let mut document = json!({
        "available": available,
        // 小浣熊的积分口径就是「积分」（源实现的中文标签与文案都用它）
        "unit": "积分",
        "wallets": wallets,
        "subscription": subscription,
        "raw": Value::Object(raw),
    });
    // `credits[]` 只在非空时挂：空数组**整个不写这个键** —— 界面按「键在不在 +
    // 长度」决定要不要给这一格挂「可点开」的交互，给一个空数组会点出一个空框
    if !credits.is_empty() {
        if let Some(object) = document.as_object_mut() {
            object.insert("credits".to_string(), Value::Array(credits));
        }
    }
    Ok(document)
}

/// 发一次 GET 并解包 `{code, message, data}`（源实现 `requestJson` 的判定链）。
///
/// 判定顺序逐条照抄：
///   ① 传输失败 → 超时给 504「积分查询超时」，其余给 502（源实现把两者压成
///      同一个 `BalanceError` 的 502，这里沿用本项目计费接口的分档口径）；
///   ② HTTP 401 → 401「登录态已过期」（调用方据此走刷新重试）；
///   ③ 非 2xx → 502 带上游状态码；
///   ④ `code` 存在且非 0 → 502 取 `message`；
///   ⑤ 取 `data ?? payload`（源实现 `payload?.data ?? payload`）。
async fn request_json(
    origin: &str,
    path: &str,
    token: &str,
    headers: &[(String, String)],
) -> Result<Value, GatewayError> {
    request_json_ex(origin, "GET", path, token, headers, REQUEST_TIMEOUT_MS).await
}

/// `request_json` 的通用形态：方法与附加头可指定（每日签到与新手任务的
/// 桌面端链路需要 `X-Client-Platform` / `X-Client-Version` 头，
/// 账单核对需要更长的超时 —— 见 `claim_daily_grant` 的说明）。
///
/// 其余判定链与 `request_json` 完全一致；`method` 目前只有 GET / POST 两种用法
/// （上游的积分域没有其它方法）。
pub(super) async fn request_json_ex(
    origin: &str,
    method: &str,
    path: &str,
    token: &str,
    extra_headers: &[(String, String)],
    timeout_ms: u64,
) -> Result<Value, GatewayError> {
    let url = format!("{origin}{path}");
    let mut headers: Vec<(String, String)> = vec![
        ("Accept".to_string(), "application/json".to_string()),
        // 源实现 `token.replace(/^Bearer\s+/i, '')` 后再拼 Bearer：
        // 用户粘贴的 token 可能自带前缀，重复拼会得到 `Bearer Bearer xxx`。
        // 复用 `jwt::strip_bearer`（同目录、同一套「按字节前缀比较」的安全实现）——
        // 自己再写一份 `trimmed[..7]` 会在 token 以多字节字符开头时 panic
        (
            "Authorization".to_string(),
            format!("Bearer {}", super::jwt::strip_bearer(token)),
        ),
    ];
    headers.extend(extra_headers.iter().cloned());
    // 直连（源实现这条链路是裸 fetch，见模块头）
    let response = send_raw(method, &url, None, &headers, None, Some(timeout_ms))
        .await
        .map_err(|error| {
            if error.is_timeout() {
                GatewayError::with_status(504, "积分查询超时")
            } else {
                GatewayError::with_status(502, format!("积分查询失败: {error}"))
            }
        })?;
    let payload = response.payload.unwrap_or(Value::Null);
    if response.status == 401 || super::upstream_errors::token_expired(&payload) {
        return Err(GatewayError::with_status(401, "登录态已过期，无法查询积分"));
    }
    if !response.ok {
        return Err(GatewayError::with_status(
            502,
            format!("积分查询返回 HTTP {}", response.status),
        ));
    }
    if let Some(code) = super::upstream_errors::business_code(&payload) {
        if code != 0 {
            let message = payload
                .get("message")
                .and_then(Value::as_str)
                .filter(|text| !text.is_empty())
                .map(str::to_string)
                .unwrap_or_else(|| format!("积分查询返回异常 code={code}"));
            return Err(GatewayError::with_status(502, message));
        }
    }
    // `payload?.data ?? payload`：data 为 null（显式给了 null）时源实现的 `??`
    // 会落到 payload 本身，所以这里只在 data **缺失或为 null** 时回落
    let data = payload.get("data").cloned().unwrap_or(Value::Null);
    Ok(if data.is_null() { payload } else { data })
}

/// 订阅权益 → 统一形状的 `subscription`（源实现 `querySubscription` 的字段）。
///
/// 小浣熊有两套权益（办公端 `office` 与代码端 `code`），源实现两个都返回。
/// 统一形状里只留一组「套餐 / 状态 / 到期时间」，因此**优先取办公端的**
/// （那是小浣熊桌面端的主入口），办公端没有时回落代码端；两套都保留在
/// `raw` 里供排障。
///
/// `expireAt` 是上游给的字符串形态时间（源实现直接透出，不做解析），
/// 这里同样原样保留 —— 前端按字符串展示，转换成时间戳会让「上游给的是
/// 本地时间串还是 ISO」这个不确定性冒到界面上。
fn normalize_subscription(data: &Value) -> Value {
    let office = data.get("office").filter(|value| value.is_object());
    let code = data.get("code").filter(|value| value.is_object());
    let pick = |key: &str| -> Option<Value> {
        office
            .and_then(|value| value.get(key))
            .or_else(|| code.and_then(|value| value.get(key)))
            .filter(|value| !value.is_null())
            .cloned()
    };
    let pro_enabled = office
        .and_then(|value| value.get("pro_enable"))
        .and_then(Value::as_bool)
        .or_else(|| {
            code.and_then(|value| value.get("pro_enable"))
                .and_then(Value::as_bool)
        })
        .unwrap_or(false);
    if office.is_none() && code.is_none() {
        return Value::Null;
    }
    let plan_type = pick("active_plan")
        .and_then(|plan| plan.get("type").cloned())
        .or_else(|| pick("plan_type"))
        .unwrap_or(Value::Null);
    let expired_at = pick("pro_expired_time")
        .or_else(|| {
            pick("active_plan").and_then(|plan| plan.get("subscription_expired_at").cloned())
        })
        .unwrap_or(Value::Null);
    json!({
        "planName": plan_type.as_str().unwrap_or(""),
        // 状态沿用上游语义：开了 pro 是 active，没开是 none
        // （源实现只给布尔 pro_enable，没有更细的状态机）
        "status": if pro_enabled { "active" } else { "none" },
        "expireAt": expired_at,
        "remainQuota": Value::Null,
        "totalQuota": Value::Null,
    })
}

/// 拉取**收入类**积分账单（`type=income`）—— 签到判据的唯一数据源。
///
/// ── 为什么必须是收入视图，而不是「翻全量流水」──────────────────
/// 判据要回答的是「**今天这笔每日发放有没有入账**」，而这笔账在全部流水里
/// 占比极小：每次对话消耗都记一条 `desktop` 流水，活跃账号一天几百条，
/// 每日发放那条几分钟内就被挤出窗口。实测（2026-11-05 本机账号）：
///
/// | 查询 | 请求数 | 耗时 | 今日 daily_grant |
/// |---|---|---|---|
/// | `type=all` 每页 20 翻 6 页（旧实现） | 最多 6 | 最坏 45 s | **翻不到**（在第 8 页） |
/// | `type=income` 单页 20（本函数） | **1** | **0.22 s** | 第一页第 2 条 |
///
/// 上游 `type` 只认 `all` / `income`（其余取值 HTTP 400），`income` 精确过滤出
/// 「发放 / 奖励」类。每日发放按自然日一条，20 条覆盖最近约 19 天，因此
/// **不翻页**：翻页是给全量视图的补救手段，换成收入视图后就不该再存在 ——
/// 那等于用「多翻几页」去兜一个「查错了视图」的问题。
async fn income_bills(
    origin: &str,
    token: &str,
    headers: &[(String, String)],
) -> Result<Vec<Value>, GatewayError> {
    let path = format!("/api/web/points/v2/bills?paging.limit={INCOME_BILLS_LIMIT}&type=income");
    let data =
        request_json_ex(origin, "GET", &path, token, headers, GRANT_CHECK_TIMEOUT_MS).await?;
    let items = data
        .get("items")
        .and_then(Value::as_array)
        .ok_or_else(|| GatewayError::with_status(502, "积分账单响应缺少 items"))?;
    Ok(items.to_vec())
}

fn bill_date(item: &Value) -> Option<String> {
    let at = item
        .get("grant_at")
        .or_else(|| item.get("created_at"))?
        .as_str()?;
    if let Ok(date) = chrono::DateTime::parse_from_rfc3339(at) {
        return Some(beijing::date_key(date.with_timezone(&beijing::offset())));
    }
    at.get(..10).map(str::to_string)
}

fn daily_grant(item: &Value, today: &str) -> Option<Value> {
    let kind = item.get("biz_type").and_then(Value::as_str);
    let name = item.get("event_name").and_then(Value::as_str).unwrap_or("");
    let daily = kind == Some("daily_grant") || kind.is_none() && name == "每日积分发放";
    let points = to_int(item.get("points"))?;
    if !daily || points <= 0 || bill_date(item).as_deref() != Some(today) {
        return None;
    }
    let at = item.get("created_at").and_then(Value::as_str).unwrap_or("");
    Some(json!({
        "name": name,
        "points": points,
        "at": at.get(11..19).unwrap_or(""),
        "expireAt": item.get("expire_at"),
    }))
}

/// 收入账单里的「发放」记录 → `credits[]` 的批次行（带毫秒 `expireAt` 与 `remain`）。
///
/// ── 剩余是怎么来的 ────────────────────────────────────────
/// 上游**不给**「这一批还剩多少」，但能算出来：
///
/// 1. 客户端的消耗规则原文是「**同类内按到期日/入账时间排序**」—— 即先到期先扣；
/// 2. 余额接口的 `daily_points` 就是这些未过期批次的池子余额；
/// 3. 于是 `已消耗 = Σ未过期批次发放额 − daily_points`，再按到期日升序逐批扣。
///
/// 实测校验（2026-11-05 23:46）：7 批 × 300 = 2100，`daily_points = 1885`，
/// 已消耗 215 → 最早那批（10-30 发放）应剩 85；同一时刻账单里正躺着一条
/// `daily_expire`：「每日积分到期清零 -85，grant_at 2026-10-30，expire_at 11-05 23:59」
/// —— **逐字对上**，所以这个摊法不是猜的。
///
/// 拿不到 `daily_points` 时**不写 `remain`**（只给时间）：宁可少一个数，
/// 也不编一个「还剩满额」。
///
/// 只保留**未过期**且 `points > 0` 的：已作废的批次对「还有多久到期」没有
/// 意义，留在明细里只是噪音（实测 20 条收入里前 13 条早已过期）。
fn grant_packs(items: &[Value], daily_balance: Option<i64>) -> Vec<Value> {
    let now = crate::http::now_ms();
    // (到期毫秒, 发放额, 展示名)。先收集再排序：账单是**倒序**（最新在前），
    // 直接按账单顺序摊消耗会把余量算到错误的那一批上。
    let mut batches: Vec<(i64, i64, String)> = Vec::new();
    for item in items {
        let Some(expire) = expire_at_millis(item) else {
            continue;
        };
        if expire <= now {
            continue;
        }
        let points = to_int(item.get("points")).unwrap_or(0);
        if points <= 0 {
            continue;
        }
        let name = item
            .get("event_name")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|text| !text.is_empty())
            .unwrap_or("积分发放");
        // 附上发放日：否则明细里会排出一串同名行（全是「每日积分发放」）
        let label = match bill_date(item) {
            Some(date) if date.len() >= 10 => format!("{name} · {}", &date[5..10]),
            _ => name.to_string(),
        };
        batches.push((expire, points, label));
    }
    batches.sort_by_key(|(expire, _, _)| *expire);

    let mut rows = Vec::new();
    match daily_balance {
        Some(balance) => {
            let total: i64 = batches.iter().map(|(_, points, _)| *points).sum();
            // 仅当池子余额**小于**发放总额时才有消耗；大于等于时按 0 处理
            // （多出来的是别的池子串味或上游延迟，不该反推出负消耗）
            let mut used = (total - balance).max(0);
            for (expire, points, label) in batches {
                let take = used.min(points);
                used -= take;
                rows.push(json!({
                    "name": label,
                    "total": points,
                    "remain": points - take,
                    "expireAt": expire,
                }));
            }
        }
        None => {
            // 拿不到池子余额就不摊剩余：宁可只显示时间，也不编「还剩满额」
            for (expire, points, label) in batches {
                rows.push(json!({
                    "name": label,
                    "total": points,
                    "expireAt": expire,
                }));
            }
        }
    }
    rows
}

/// `expire_at`（RFC3339 字符串，如 `2026-11-11T23:59:59+08:00`）→ 毫秒时间戳。
///
/// 必须转成数字：前端 `creditExpiryOfUsage` 按**毫秒数字**读 `expireAt`，
/// 直接塞字符串进去会被 `Number(...)` 判成 `NaN`，等于这一行没有到期时间。
fn expire_at_millis(item: &Value) -> Option<i64> {
    chrono::DateTime::parse_from_rfc3339(item.get("expire_at").and_then(Value::as_str)?)
        .ok()
        .map(|value| value.timestamp_millis())
}

/// 数值取整（源实现 `toInt`：非有限值给 None，有限值取 floor）。
///
/// 返回 `Option<i64>` 而不是 f64：积分是整数口径，源实现也做 `Math.floor`。
fn to_int(value: Option<&Value>) -> Option<i64> {
    let number = match value? {
        Value::Number(number) => number.as_f64()?,
        // 上游偶尔把数字写成字符串（源实现的 `Number(value)` 两种都认）
        Value::String(text) => text.trim().parse::<f64>().ok()?,
        _ => return None,
    };
    if !number.is_finite() {
        return None;
    }
    Some(number.floor() as i64)
}

// ─── 每日「签到」（桌面登录积分发放）─────────────────────────
//
// 小浣熊没有 WorkBuddy 那种「每日签到」接口；它的每日奖励走的是**桌面端
// 登录积分**链路：官方桌面客户端每次启动会打 setting_info，服务端据此按天
// 发放每日积分。这里复刻同一条链路作为「签到」：
//   ① GET /api/web/points/v2/bills?type=income —— 查**今天的发放有没有入账**
//      （判据，见 `income_bills` 的说明：一页即命中，0.2 s）
//   ② GET /api/web/office/v3/setting_info —— 每日积分的**发放触发器**
//      （按天幂等；仅在 ① 显示今天还没入账时才打）
//
// 判据放在「收入视图」而不是「全量流水」上，是 2026-11-05 的修正：全量视图
// 里每次对话消耗都占一条流水，活跃账号一天几百条，每日发放那条几分钟就被挤
// 出窗口 —— 当时的实现只翻 6 页（120 条）就放弃，于是**明明已领到却报「尚未
// 确认领取」**（实测 11-05 当天那条在第 8 页）。收入视图只含发放/奖励，一页
// 20 条覆盖最近约 19 天，今天的必在第一页。
//
// 同一条 grant 接口里的**首次**桌面登录奖励（一次性 3000，新手福利）原先是
// 本函数的第①步，已拆到新手任务分组（`onboarding.rs`，与 Loomy 同一入口），
// 不再随每日签到执行 —— 一次性动作混进每日调度，之后每一轮都会白打一次。

/// 收入视图的拉取条数。每日发放按自然日一条 → 20 条覆盖最近约 19 天，
/// 足够覆盖「今天」且不必翻页（翻页是给全量视图的补救手段，收入视图不需要）。
const INCOME_BILLS_LIMIT: usize = 20;
/// 收入视图的超时。全量视图实测约 9 s/页，收入视图实测 **0.22 s**（2026-11-05），
/// 10 s 已是宽松上限；失败只影响「能不能读到分值」，不影响发放本身。
const GRANT_CHECK_TIMEOUT_MS: u64 = 10_000;

/// 客户端身份头：官方桌面端启动时带的两个头（源实现 grantDesktopLoginPoints）。
/// 每日签到（setting_info）与新手任务的首次登录奖励（`onboarding.rs`）共用。
pub(super) fn desktop_client_headers() -> Vec<(String, String)> {
    super::headers::desktop_client_headers()
}

/// 客户端身份头：官方手机端（Capacitor App）请求登录积分接口带的两个头。
///
/// 平台值是 App 壳的 `getPlatform()` 加 `app-` 前缀（`app-android` / `app-ios`，
/// 依据安卓包确认），版本头带 `v` 前缀（App 把自身版本号剥掉 `v` 再补回去）。
/// 这里固定报安卓 `v1.0.3`（实测样本）；上游只按账号结算一次性奖励，报哪端
/// 只影响走哪条端点，不影响凭证 —— 手机端接口认的也是桌面端登录签发的
/// 同一把 Bearer token（实测 + JWT claims 无平台字段双重确认）。
/// 只服务新手任务的手机端首登奖励（`onboarding.rs`）。
pub(super) fn mobile_client_headers() -> Vec<(String, String)> {
    vec![
        ("X-Client-Platform".to_string(), "app-android".to_string()),
        ("X-Client-Version".to_string(), "v1.0.3".to_string()),
    ]
}

/// 小浣熊账号的每日签到（claim 形状与 WorkBuddy 的 claim_daily_checkin 对齐：
/// `{success, msg}` —— billing::checkin 的汇总只认这两个字段）。
///
/// `success` 的口径是「**今日积分确有入账**」（账单核对），而不是某一步的
/// HTTP 成败：发放是服务端在 setting_info 上按天幂等完成的，重复执行
/// 不会重复入账，账单里看得到就是领到了。
///
/// 首次桌面登录奖励（一次性新手福利）**不在**本函数里 —— 它已拆到
/// `onboarding.rs` 挂新手任务分组，见那里的模块头说明。
pub async fn claim_daily_grant(ctx: &PluginCtx, account_id: &str) -> Result<Value, GatewayError> {
    let credentials = credentials::snapshot(ctx, account_id).await?;
    if credentials.token.trim().is_empty() {
        return Err(GatewayError::with_status(
            401,
            "该账号没有可用凭证，无法签到",
        ));
    }
    let origin = main_site_url();

    let today = beijing::today_key();
    let mut org_headers = Vec::new();
    super::headers::append_org(&mut org_headers, &credentials.org_code)
        .map_err(|message| GatewayError::with_status(400, message))?;
    // ① 先查今天的发放有没有入账（收入视图，一页即命中，见 `income_bills`）。
    //    已入账 = 今天已经领过了 —— 这是**状态**，不是「在流水里翻到了什么」。
    let previous = match income_bills(&origin, &credentials.token, &org_headers).await {
        Ok(items) => Some(
            items
                .iter()
                .filter_map(|item| daily_grant(item, &today))
                .collect::<Vec<_>>(),
        ),
        Err(error) if error.status_code == 401 => return Err(error),
        // 查不到就当作「未知」：不阻断流程，交给下面的触发 + 复核
        Err(_) => None,
    };
    let already_completed = previous.as_ref().is_some_and(|items| !items.is_empty());

    // ② 今天还没入账才触发发放（setting_info 按天幂等）。
    //    已入账时整段跳过：触发本来也不会重复发放，但少一次上游请求，
    //    也不必为一次「早就领过」的签到去讨要官方客户端的发放通知。
    let settings = if already_completed {
        json!({ "popups": [], "toast": Value::Null })
    } else {
        let mut settings_headers = desktop_client_headers();
        super::headers::append_org(&mut settings_headers, &credentials.org_code)
            .map_err(|message| GatewayError::with_status(400, message))?;
        settings_headers.push(("X-Raccoon-Language".to_string(), "zh".to_string()));
        let settings_path = if credentials.org_code.is_empty() {
            "/api/web/office/v3/setting_info"
        } else {
            "/api/web/org/user/office/v1/setting_info"
        };
        match request_json_ex(
            &origin,
            "GET",
            settings_path,
            &credentials.token,
            &settings_headers,
            REQUEST_TIMEOUT_MS,
        )
        .await
        {
            Ok(data) => {
                // 发放通知原样透出（结构与上游一致，界面不深入解析）
                json!({
                    "popups": data.get("point_grant_popups").cloned().unwrap_or(Value::Null),
                    "toast": data.get("point_grant_toast").cloned().unwrap_or(Value::Null),
                })
            }
            Err(error) => return Err(error),
        }
    };

    // ③ 复核今日入账（同一个收入视图）。已入账时直接用 ① 的结果，省一次请求。
    // 「今天」按**北京时间**（上游自然日即 UTC+8 零点，见 `core::beijing`），
    // 不跟机器时区：海外 / 容器部署下跟本地时区走会把 16 小时的账单认成昨天，
    // 让「今日积分 +N」与「今天已领过」的判定整体错位（issue #138）。
    let (grants, grants_error) = if already_completed {
        (previous.unwrap_or_default(), None)
    } else {
        match income_bills(&origin, &credentials.token, &org_headers).await {
            Ok(items) => (
                items
                    .iter()
                    .filter_map(|item| daily_grant(item, &today))
                    .collect::<Vec<_>>(),
                None,
            ),
            Err(error) if error.status_code == 401 => return Err(error),
            Err(error) => (Vec::new(), Some(error.message)),
        }
    };

    let granted_today = grants
        .iter()
        .map(|item| to_int(item.get("points")).unwrap_or(0))
        .sum::<i64>();
    // 「查过、今天确实没有入账」与「没查到」是两回事：前者是上游给出的确定
    // 状态（可能今天额度已用完），后者是核对失败（网络/接口），下次还能再试。
    // 上游把这两者混在一个 `success=false` 里时，调用方只能靠猜文案 —— 这里
    // 显式分出来，`checkin_report_of` 就不必再 `contains("未见")`。
    let unsettled = !already_completed && granted_today == 0 && grants_error.is_none();
    let mut msg_parts: Vec<String> = Vec::new();
    if granted_today > 0 {
        msg_parts.push(if already_completed {
            format!("今日已领取 +{granted_today}")
        } else {
            format!("今日积分 +{granted_today}")
        });
    } else if grants_error.is_some() {
        // 核对失败拿不到权威结果，不谎报成功
        msg_parts.push("未能核对今日入账".to_string());
    } else {
        msg_parts.push("今日发放未入账".to_string());
    }
    if let Some(error) = grants_error.as_deref() {
        msg_parts.push(format!("入账核对失败：{error}"));
    }

    Ok(json!({
        "success": granted_today > 0,
        "alreadyCompleted": already_completed,
        "unsettled": unsettled,
        "msg": msg_parts.join("；"),
        "grantsToday": grants,
        "settings": settings,
        "grantsError": grants_error.map(Value::String).unwrap_or(Value::Null),
    }))
}
