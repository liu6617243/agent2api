use std::collections::BTreeSet;
use std::sync::OnceLock;

fn claim_lock() -> &'static tokio::sync::Mutex<()> {
    static LOCK: OnceLock<tokio::sync::Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| tokio::sync::Mutex::new(()))
}

use agent2api_provider_protocol as proto;
use agent2api_provider_sdk::{CheckinResult, PluginCtx};
use serde_json::{json, Value};

use super::api;
use crate::errors::GatewayError;

const V3_GROUP: &str = "pc_incentive_task_mvp_v3";


/// 为什么插件**自己（reqwest）发不出**这条写接口（2026-11-12 定案，推翻
/// §10.20 的「对齐参数即可」）。
///
/// 真实客户端的 `landing/checkin` 上带 `a_bogus`：184 字符、解码 136 字节的
/// 纯二进制。2026-11-12 逐层探测，把「谁能生成它」问清楚了：
///
/// | 环境 | 能否产出 a_bogus | 证据 |
/// |------|-----------------|------|
/// | 纯 Rust（插件进程） | **不能** | 自研 `abogus.rs` 与豆包不同构，反推零命中 |
/// | 页面 JS 显式调用 | **不能** | `frontierSign` 只回 16 字符 `X-Bogus` |
/// | **任意 Chromium 渲染进程的 fetch** | **能** | 拦截器自动补 `msToken` + `a_bogus` |
/// | 豆包客户端 webview | **能** | 同上（它也是 Chromium） |
///
/// 关键差别在最后两行：`a_bogus` 不是在 JS 里算的，而是**渲染进程把请求交给
/// native 网络栈时补上的**（`Network.requestWillBeSent` 抓到的 wire URL 里
/// 已经带好了，但 152 个脚本源码里 `a_bogus` 出现 0 次）。所以：
///
/// · 插件用 reqwest 自己发 → 永远没有签名 → 必被 710010702，且**把该 uid
///   当天的签到口封死，连官方客户端一起废**（用户 2026-11-12 原话：
///   「风控惩罚会导致客户端也无法签到」）；
/// · 走任意 Chromium 渲染进程发 → 签名由网络栈自动补 → 这才是唯一能通的通道。
///
/// ⚠️ 2026-11-12 追加实测（推翻此前「必须用豆包客户端」的判断）：**普通 Chrome
/// 与 --headless=new Chrome 都能补出完整签名**，`v3/detail` 回 `code:0`。
/// 所以插件不需要用户先开豆包客户端，自己起一个 headless 就行 ——
/// 见 [`super::webview`]，它自带「先借已有调试端口、再自己拉起」两条路。
///
/// [`CheckinOutcome::Captcha`]（「需人工」）留给两种情形，都不发写请求：
/// 自动签到路径（本就不该替用户写），以及浏览器通道不可用（这台机器上一个
/// Chromium 都没有、或被 30 秒超时挡住）。
///
/// 这个常量只是文档锚点（正文在上），没有运行时用途。
#[allow(dead_code)]
const CHECKIN_BLOCKED: () = ();

/// 任务奖励领取接口。只用于**非签到**任务；签到走 [`super::webview`]。
///
/// 历史坑：daily_signin 打这个接口永远只回
/// `biz_error{code:3,message:"领取结果确认中，请稍后重试"}, task_status:0`，
/// 且 task_status 不会自行变 3 —— 于是每场签到都停在「未确认到账」，
/// 这就是「豆包签到总是有问题」的真正原因。
const CLAIM_PATH: &str = "/alice/ug/campaign/item/reward/claim/";



fn text<'a>(value: &'a Value, key: &str) -> Option<&'a str> {
    value[key]
        .as_str()
        .map(str::trim)
        .filter(|text| !text.is_empty())
}

async fn detail(
    client: &reqwest::Client,
    credentials: &Value,
) -> Result<Option<(String, Value)>, GatewayError> {
    let launch = api::post(client, credentials, "/alice/user/launch", &json!({})).await?;
    let Some(group) = text(&launch["data"]["ug_campaign_info"], "campaign_group_code") else {
        return Ok(None);
    };
    let path = if group == V3_GROUP {
        "/alice/ug/campaign/v3/detail/"
    } else {
        "/alice/ug/campaign/list/"
    };
    let value = api::post(
        client,
        credentials,
        path,
        &json!({"campaign_group_code":group}),
    )
    .await?;
    let data = value["data"]
        .as_object()
        .ok_or_else(|| GatewayError::with_status(502, "豆包工作活动详情格式无效"))?;
    if group == V3_GROUP
        && data
            .get("campaign_code")
            .and_then(Value::as_str)
            .is_some_and(|code| !code.is_empty() && code != group)
    {
        return Err(GatewayError::with_status(
            502,
            "豆包工作返回了不匹配的活动组",
        ));
    }
    Ok(Some((group.into(), value)))
}

#[derive(Clone, PartialEq)]
struct Task {
    key: String,
    title: String,
    group: String,
    state: u64,
    daily: bool,
    checked: bool,
    body: Option<Value>,
    points: Option<f64>,
    streak: Option<u32>,
}

fn tasks(value: &Value, group: &str) -> Vec<Task> {
    fn collect(value: &Value, group: &str, output: &mut Vec<Task>, depth: usize) {
        if depth > 8 {
            return;
        }
        let group = text(value, "campaign_group_code")
            .or_else(|| text(value, "campaign_code"))
            .unwrap_or(group);
        for key in ["tasks", "campaigns", "milestones"] {
            for row in value[key].as_array().into_iter().flatten() {
                let Some(item) = text(row, "campaign_code") else {
                    continue;
                };
                let group = text(row, "campaign_group_code").unwrap_or(group);
                let daily = row["daily_checkin"]["checked_in_today"].is_boolean();
                let checked = row["daily_checkin"]["checked_in_today"] == true;
                let state = api::number(&row["task_status"]).unwrap_or(0);
                let direct = api::number(&row["reward_mode"]) == Some(2) || key == "milestones";
                let body = if direct {
                    (!group.is_empty()).then(||json!({"task_id":"","campaign_group_code":group,"campaign_item_code":item}))
                } else {
                    text(row, "task_id").map(|id| json!({"task_id":id}))
                };
                let points =
                    crate::quota::number(row.pointer("/daily_checkin/reward/reward_amount"))
                        .or_else(|| {
                            crate::quota::number(row.pointer("/reward_summaries/0/reward_amount"))
                        });
                let task = Task {
                    key: item.into(),
                    title: text(row, "title")
                        .or_else(|| text(row, "name"))
                        .unwrap_or(item)
                        .into(),
                    group: group.into(),
                    state,
                    daily,
                    checked,
                    body: if state == 2 || (daily && direct && matches!(state, 1 | 4)) {
                        body
                    } else {
                        None
                    },
                    points,
                    streak: api::number(&row["daily_checkin"]["continuous_days"])
                        .and_then(|days| u32::try_from(days).ok()),
                };
                if !output
                    .iter()
                    .any(|old| old.key == task.key && old.group == task.group)
                {
                    output.push(task);
                }
            }
        }
        if let Some(child) = value.get("campaign_group") {
            collect(child, group, output, depth + 1);
        }
        for child in value["campaign_groups"].as_array().into_iter().flatten() {
            collect(child, group, output, depth + 1);
        }
    }
    let mut output = Vec::new();
    collect(value.get("data").unwrap_or(value), group, &mut output, 0);
    output
}

/// 领奖回执的判读结果。**仅服务于 [`claim_extras`]**（非签到任务领奖）。
///
/// 签到不再用这套判读：抓包证实 daily_signin 打领奖接口永远只回
/// `biz_error{code:3,"领取结果确认中，请稍后重试"}`，任务状态不会自行落地。
/// 签到现在走 [`super::webview`]（浏览器通道），回执自带 `checked_in` 权威字段。
///
/// 「上游说已经受理，但结果还没出」以前落在 `Err` 里被当成致命错误，于是整场
/// 判失败、`retryAt` 也带不出去 —— 那是旧签到路径的成因，留此备查。
#[derive(Clone, Copy, PartialEq)]
enum Receipt {
    /// 上游回执明确表示已领取/已完成
    Confirmed,
    /// 上游未确认（任务状态仍是未领取）
    Unconfirmed,
    /// 上游受理中，稍后才出结果
    Accepted,
}

/// 上游回执里的 `biz_error.msg` 是否表示「已受理、结果待定」。
///
/// 文案原样来自豆包服务端（`领取结果确认中，请稍后重试`），不是本插件写的。
/// 用宽松词表匹配，避免上游换措辞后又被当成致命错误打死整场签到。
fn settled_later(error: &Value) -> bool {
    let message = error
        .get("msg")
        .or_else(|| error.get("message"))
        .and_then(Value::as_str)
        .unwrap_or("");
    let normalized = message.to_lowercase();
    ["确认中", "稍后重试", "处理中", "审核中", "请稍候"]
        .iter()
        .any(|needle| normalized.contains(*needle))
        || normalized.contains("processing")
        || normalized.contains("pending")
}

fn receipt(value: &Value) -> Result<Receipt, GatewayError> {
    let data = value["data"]
        .as_object()
        .ok_or_else(|| GatewayError::with_status(502, "豆包工作未返回奖励领取回执"))?;
    // 先读 biz_error：受理态可能和 `success: false` 一起出现（「确认中」本来
    // 就不是成功），先判拒绝会把这种可恢复的异步态打死成致命错误。
    let mut accepted = false;
    if let Some(error) = data.get("biz_error").filter(|error| !error.is_null()) {
        if !matches!(api::number(&error["code"]), Some(0 | 4 | 6)) {
            if !settled_later(error) {
                return Err(api::error(200, error));
            }
            accepted = true;
        }
    }
    if data.get("success") == Some(&Value::Bool(false)) && !accepted {
        return Err(GatewayError::with_status(502, "豆包工作奖励领取被拒绝"));
    }
    if api::number(&value["data"]["task_status"]) == Some(3)
        || value["data"]["checked_in_today"] == true
        || value["data"]["daily_checkin"]["checked_in_today"] == true
    {
        return Ok(Receipt::Confirmed);
    }
    Ok(if accepted {
        Receipt::Accepted
    } else {
        Receipt::Unconfirmed
    })
}

/// 回查活动详情里那个每日签到任务，返回 (是否已完成, 连续天数)。
async fn refetch(
    client: &reqwest::Client,
    credentials: &Value,
    key: &str,
    group: &str,
) -> Result<Option<(bool, Option<u32>)>, GatewayError> {
    let fresh = detail(client, credentials).await?;
    Ok(fresh.as_ref().and_then(|(group_code, value)| {
        tasks(value, group_code)
            .into_iter()
            .find(|row| row.daily && row.key == key && row.group == group)
            .map(|row| (row.checked || row.state == 3, row.streak))
    }))
}

/// 预热回执（`v3/detail`）里那个每日签到任务是否已完成，以及连续天数。
///
/// ── 为什么不按名字匹配 ──────────────────────────────────────
/// 2026-11-12 把真实回执打出来看：任务行的标识是 `campaign_code`
/// （签到那条的值是 `daily_signin`），状态在 `daily_checkin.checked_in_today`；
/// 行里**根本没有** `name` / `task_name` / `title` 字段。按名字找「签到」会
/// 恒返回 `None`，白写一段永远走不到的分支。
///
/// 所以直接复用 [`tasks`] —— 它本来就按 `campaign_code` + `daily_checkin`
/// 判定 `daily` / `checked`，与签到主流程用的是同一套解析，不会两处判据漂移。
fn warmup_state(warmup: &Value) -> Option<(bool, Option<u32>)> {
    tasks(warmup, V3_GROUP)
        .into_iter()
        .find(|task| task.daily)
        .map(|task| (task.checked || task.state == 3, task.streak))
}

/// 每日签到。返回 SDK 的 [`CheckinResult`]：`report` 是协议化结果，
/// `retry_at` 是「稍后自动复核」的时刻 —— 必须一路带到网关
/// （`claim.retryAt` → `auto_checkin::retry`）。
/// 与 `Provider::checkin` 的契约对齐：只要报告本身。
/// `retry_at` 这类附加信息走 [`checkin_with_retry`]（由 `checkin_with_options` 转发）。
///
/// ⚠️ 这条旧入口没有 `manual` 信息，按**自动**处理 —— 于是它只会读、不会写。
/// 见 [`checkin_with_retry`] 的 `manual` 说明。
pub async fn checkin(
    ctx: &PluginCtx,
    account: &str,
) -> Result<proto::CheckinReport, GatewayError> {
    Ok(checkin_with_retry(ctx, account, false).await?.report)
}

/// 每日签到主流程。
///
/// ── `manual` 决定「读」还是「读+写」（2026-11-12 定案）──────────
/// 豆包这个写接口的失败代价不对称：**一次不合规的 `landing/checkin` 会把该 uid
/// 当天的签到口封死，连官方客户端一起废**（用户 2026-11-12 原话：「风控惩罚会
/// 导致客户端也无法签到」），而成功只是 +200 额度。所以无人值守不该碰它：
///
/// | `manual` | 触发方 | 行为 |
/// |----------|--------|------|
/// | `false` | 网关定时任务 / 自动重试 | 只 POST `v3/detail` 判状态；未签则报 `Captcha` 等人点，**不发任何写请求** |
/// | `true`  | 用户在界面上点「签到」 | 完整通道：headless Chromium 补签名 → `report` → `checkin` |
///
/// 自动路径报 `Captcha` 而不是 `Inactive`：网关的 `claim_pending()` 只对
/// `inactive` / `ineligible` 排重试（见 `core/billing/checkin.rs`），报
/// `Inactive` 等于让它隔一段时间再跑一遍；`Captcha` 不在那张表里，
/// 排不进重试循环。这样即便链路哪天又坏，自动侧也只是多读几次详情 ——
/// 读接口没有惩罚。
pub async fn checkin_with_retry(
    ctx: &PluginCtx,
    account: &str,
    manual: bool,
) -> Result<CheckinResult, GatewayError> {
    let _guard = claim_lock().lock().await;
    let (_, credentials, client) = api::account(ctx, account, false).await?;
    let Some((group, value)) = detail(&client, &credentials).await? else {
        return Ok(CheckinResult::from(report(
            proto::CheckinOutcome::Inactive,
            "当前没有豆包工作签到活动",
        )));
    };
    let daily: Vec<_> = tasks(&value, &group)
        .into_iter()
        .filter(|task| task.daily)
        .collect();
    if daily.len() != 1 {
        return Ok(CheckinResult::from(report(
            proto::CheckinOutcome::Inactive,
            "当前活动未提供可确认的每日签到任务",
        )));
    }
    let Some(task) = daily.first() else {
        return Ok(CheckinResult::from(report(
            proto::CheckinOutcome::Inactive,
            "当前没有每日签到任务",
        )));
    };
    if task.checked || task.state == 3 {
        return Ok(CheckinResult::from(report(
            proto::CheckinOutcome::Done,
            "今日已领取豆包工作签到奖励",
        )));
    }
    // ── 自动签到的终点：只读，到此为止 ──────────────────────────
    // 上面那次 `detail` 已经拿到了权威状态（那一刻未签，否则上面就 `Done` 了）。
    // 自动任务在这里收工，**不发任何写请求** —— 这是「不加重账号惩罚」的全部实现。
    //
    // 报 `Captcha`（语义是「需人工」，见 `CheckinOutcome` 的变体说明），不是
    // `Inactive`。两层原因：
    // · 语义上就是「这一步要人来点」，不是「今天没活动」——报 `Inactive` 会让
    //   界面显示成没什么可做；
    // · 机制上网关的 `claim_pending()` 只把 `inactive` / `ineligible` 放进
    //   重试名单（`core/billing/checkin.rs`），`captcha` 不在其中。
    //   不想让这台机器在你没看着的时候反复跑，就得避开那两个字面量。
    if !manual {
        return Ok(CheckinResult::from(report(
            proto::CheckinOutcome::Captcha,
            "豆包签到需要你手动点一次：它的签到是写入型接口，一次不合规的请求就会让该账号当天被封（连官方客户端一起签不上），所以插件不做自动签到。",
        )));
    }
    // ── 通道一：Chromium 渲染栈（唯一能签上的路）─────────────────
    // `a_bogus` 由 Chromium 网络栈在**渲染进程交出发送**那一刻补上，与「是不是
    // 豆包客户端」无关。插件自己起 headless Chrome 就行，用户不用先开客户端。
    // 见 [`super::webview`]。失败（没装浏览器/启动失败/超时）返回 `None`，
    // 落到通道二。
    let cookies = super::accounts::cookie_pairs(&credentials).unwrap_or_default();
    let pairs = api::query_pairs(&credentials)?;
    if let Some(attempt) = super::webview::checkin(&cookies, &pairs).await {
        let warmup = &attempt.warmup;
        // ── 先看预热回执：签到任务是否已完成 ──
        // `detail` 里那条每日签到行带 `daily_checkin.checked_in_today`。这比
        // 让写请求去撞 710010702 再猜要可靠：撞一次就是一次不合规写请求。
        let before = warmup_state(warmup).unwrap_or((false, None));
        if before.0 {
            return Ok(CheckinResult {
                report: proto::CheckinReport {
                    outcome: proto::CheckinOutcome::Claimed,
                    credit: task.points.map(|points| format!("{points} 额度")),
                    streak: before.1.or(task.streak),
                    message: Some("今日已领取豆包签到奖励".into()),
                },
                retry_at: None,
                details: None,
            });
        }
        // ── 没有写回执 = 浏览器侧已判定「今天签过」，没发写请求 ──
        // 两个判据（`webview::signed_in` 与 `warmup_state`）都读同一批字段，
        // 但遍历口径略有差别（前者走整棵树，后者走 `tasks`/`campaigns`/
        // `milestones`）。真出现「浏览器说签了、`tasks` 没认出来」的情形，
        // 宁可报「已领取」也不要补发写请求 —— 少签一次只是少一次展示，
        // 多写一次是把账号往封口里推。
        let Some(response) = attempt.checkin.as_ref() else {
            return Ok(CheckinResult::from(report(
                proto::CheckinOutcome::Claimed,
                "今日已领取豆包签到奖励",
            )));
        };
        let data = response.get("data").and_then(Value::as_object);
        let confirmed = data
            .map(|data| {
                // ⚠️ 用 `get` 而不是 `data["…"]`：`serde_json::Map` 没有实现
                // `Index<&str>`（只有 `Value` 有），直接下标编译不过。
                data.get("checked_in").and_then(Value::as_bool) == Some(true)
                    || data.get("reward_status").and_then(api::number) == Some(3)
                    || data.get("task_status").and_then(api::number) == Some(3)
            })
            .unwrap_or(false);
        // 回执是上游原文；code 0 + checked_in 才算成。
        if confirmed {
            // `quota::number` 收 `Option<&Value>`，所以走 `pointer` 而不是取址下标。
            let points = crate::quota::number(response.pointer("/data/reward_amount"))
                .or(task.points);
            return Ok(CheckinResult {
                report: proto::CheckinReport {
                    outcome: proto::CheckinOutcome::Claimed,
                    credit: points.map(|points| format!("{points} 额度")),
                    streak: api::number(&response["data"]["continuous_days"])
                        .and_then(|days| u32::try_from(days).ok())
                        .or(task.streak),
                    message: Some("豆包签到成功".into()),
                },
                retry_at: None,
                details: None,
            });
        }
        // 通道通了但上游没认（典型是当日已封口）：回查一次，仍未签就报
        // 「已封口，明天再来」，并把重试指到次日 —— 当天换任何通道都一样。
        let after = refetch(&client, &credentials, &task.key, &task.group)
            .await
            .ok()
            .flatten();
        if after.as_ref().is_some_and(|(done, _)| *done) {
            return Ok(CheckinResult {
                report: proto::CheckinReport {
                    outcome: proto::CheckinOutcome::Claimed,
                    credit: task.points.map(|points| format!("{points} 额度")),
                    streak: after.and_then(|(_, streak)| streak).or(task.streak),
                    message: Some("豆包签到成功".into()),
                },
                retry_at: None,
                details: None,
            });
        }
        let blocked = response
            .get("code")
            .and_then(Value::as_i64)
            .map(|code| code == 710010702 || code == 625002002)
            .unwrap_or(false);
        // ── 关键判据：预热（读接口）通不通 ──
        // 预热是 `v3/detail`，纯读、签名同一条链路。它能回 `code:0` 就说明
        // 「签名有效、Cookie 有效、通道成立」—— 那么随后的 `checkin` 报
        // 710010702 只能是**服务端对写接口的独立处置**（当日封口/风控降权），
        // 不是插件这边的参数或通道问题。这个区分决定了用户该等一天还是该改配置，
        // 所以文案必须分开写。
        let channel_ok = warmup.get("code").and_then(Value::as_i64) == Some(0);
        // ⚠️ **不带** `retry_at`，且报 `Captcha` 而不是 `Inactive`。
        // 两个都为了同一件事：不让网关**排进重试循环**。
        // · 封口按自然日重置，当天再跑必然是同一档，重试纯属浪费；
        // · 更要紧的是重试意味着「你没看着的时候机器又发了一次请求」—— 正是
        //   2026-11-12 用户明确否掉的行为（骂的是自动签到，不分读还是写）。
        // `Captcha`（需人工）不在 `claim_pending()` 的重试名单里（它只认
        // `inactive` / `ineligible`），语义也正好对：这一步该由人来点。
        return Ok(CheckinResult::from(report(
            proto::CheckinOutcome::Captcha,
            if blocked && channel_ok {
                "豆包签到被服务端拒绝（710010702）：读接口正常、写接口被单独挡下，属该账号当日风控封口。签到口按自然日重置，明天你再点一次即可"
            } else if blocked {
                "豆包返回「系统错误」（710010702）：该账号当日签到口已被风控封口，明天再点一次即可"
            } else {
                "豆包签到已提交但未确认到账，稍后回账号看一下状态"
            },
        )));
    }
    // ── 浏览器通道不可用：终止，不加任何后备写请求 ───────────────
    // 走到这里说明预热/签到两步都没跑起来（没装 Chrome/Edge、启动失败、30 秒内
    // 没拿到签名 SDK）。**完全不需要用户开豆包客户端** —— 插件会自己拉起
    // headless 浏览器；仅当这台机器上一个 Chromium 系浏览器都没有时才会到这里。
    //
    // ⚠️ 这里**不放**任何「退回 reqwest 直发」的分支。那条路发出的
    // `landing/checkin` 没有 `a_bogus`，一次就够把该 uid 当天封死，连官方客户端
    // 一起废（见 [`CHECKIN_BLOCKED`]）。历史上 `directCheckin` 开关就是干这个的，
    // 2026-11-12 按用户要求整个删掉：宁可签不上，也不能替用户按下那个按钮。
    //
    // 报 `Captcha` 且**不带** `retry_at`：这是需要人来处理的状态（装浏览器），
    // 自动重试不会让它变好，带上只会空耗网关的重试额度。
    //
    // 文案分两档：机器上确实没有 Chromium 内核浏览器（用户能自己解决），
    // 还是浏览器在但这条通道没跑通（需要我这边再看日志）。混成一句会让
    // 用户白装一遍浏览器。
    let message = if super::webview::chromium_available() {
        "豆包签到没跑通：浏览器起得来，但这条通道没拿到签名回执。可以先在豆包客户端「任务中心」手点一次签到，同时把这次的时间点告诉我，我去看实际回执。"
    } else {
        "豆包签到需要 Chromium 内核浏览器（a_bogus 签名由它补）。这台机器上没找到 Chrome/Edge，请装一个再点一次签到；也可以在豆包客户端「任务中心」手点「立即签到」。"
    };
    Ok(CheckinResult::from(report(proto::CheckinOutcome::Captcha, message)))
}

fn report(outcome: proto::CheckinOutcome, message: &str) -> proto::CheckinReport {
    proto::CheckinReport {
        outcome,
        credit: None,
        streak: None,
        message: Some(message.into()),
    }
}

fn status_of(group: &str, value: &Value) -> Value {
    let tasks: Vec<_> = tasks(value, group)
        .into_iter()
        .filter(|task| !task.daily)
        .collect();
    let earned: f64 = tasks
        .iter()
        .filter(|task| task.state == 3)
        .filter_map(|task| task.points)
        .sum();
    let total: f64 = tasks.iter().filter_map(|task| task.points).sum();
    let unclaimed = tasks
        .iter()
        .filter(|task| task.body.is_some() && task.state != 3)
        .count();
    let rows: Vec<_> = tasks
        .iter()
        .map(|task| {
            json!({"key":task.key,"title":task.title,"group":task.group,"points":task.points,
        "done":task.state == 3,"blocked":task.body.is_none() && task.state != 3})
        })
        .collect();
    json!({"tasks":rows,"earned":earned,"total":total,"unclaimed":unclaimed,"settled":!tasks.is_empty() && tasks.iter().all(|task|task.state == 3)})
}

pub async fn status(ctx: &PluginCtx, account: &str) -> Result<Value, GatewayError> {
    let (_, credentials, client) = api::account(ctx, account, false).await?;
    Ok(match detail(&client, &credentials).await? {
        Some((group, value)) => status_of(&group, &value),
        None => json!({"tasks":[],"earned":0,"total":0,"unclaimed":0,"settled":false}),
    })
}

pub async fn claim_extras(ctx: &PluginCtx, account: &str) -> Result<Value, GatewayError> {
    let _guard = claim_lock().lock().await;
    let (_, credentials, client) = api::account(ctx, account, false).await?;
    let Some((group, value)) = detail(&client, &credentials).await? else {
        return Ok(json!({"tasks":[],"claimed":0,"failed":0,"unclaimed":0,"settled":false}));
    };
    let pending: Vec<_> = tasks(&value, &group)
        .into_iter()
        .filter(|task| !task.daily && task.body.is_some() && task.state != 3)
        .collect();
    let mut attempted = BTreeSet::new();
    let mut results = Vec::new();
    for task in pending {
        if !attempted.insert((task.group.clone(), task.key.clone())) {
            continue;
        }
        let Some(body) = task.body.as_ref() else {
            continue;
        };
        let response = api::post(&client, &credentials, CLAIM_PATH, body).await;
        let result = match response {
            Ok(response) => receipt(&response).map(|_| ()),
            Err(error) => Err(error),
        };
        match result {
            Ok(()) => results.push(json!({"key":task.key,"ok":true})),
            Err(error) => results.push(json!({"key":task.key,"ok":false,"error":error.message})),
        }
    }
    let fresh = detail(&client, &credentials).await?;
    let mut state = fresh
        .as_ref()
        .map(|(group, value)| status_of(group, value))
        .unwrap_or_else(|| status_of(&group, &value));
    let verified: BTreeSet<_> = fresh
        .as_ref()
        .map(|(group, value)| {
            tasks(value, group)
                .into_iter()
                .filter(|task| task.state == 3)
                .map(|task| task.key)
                .collect()
        })
        .unwrap_or_default();
    for result in &mut results {
        if result["ok"] == true && !verified.contains(result["key"].as_str().unwrap_or("")) {
            result["ok"] = json!(false);
            result["error"] = json!("官方尚未确认奖励到账，请稍后查询");
        }
    }
    let claimed = results.iter().filter(|result| result["ok"] == true).count();
    state["claimed"] = json!(claimed);
    state["failed"] = json!(results.len() - claimed);
    state["results"] = json!(results);
    Ok(state)
}
