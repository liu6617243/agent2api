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

/// 签到接口。抓包（Reqable，豆包客户端 2.32.6 / doubao_pc_version 5.73.60）
/// 证实客户端点「签到」**只**调这一个接口：
/// `POST /alice/ug/campaign/landing/checkin` body `{"scene":"<活动组>"}`，
/// 回执 `data.checked_in / reward_status / continuous_days` 就是权威结果。
const CHECKIN_PATH: &str = "/alice/ug/campaign/landing/checkin";

/// 任务奖励领取接口。只用于**非签到**任务；签到走 [`CHECKIN_PATH`]。
///
/// 历史坑：daily_signin 打这个接口永远只回
/// `biz_error{code:3,message:"领取结果确认中，请稍后重试"}, task_status:0`，
/// 且 task_status 不会自行变 3 —— 于是每场签到都停在「未确认到账」，
/// 这就是「豆包签到总是有问题」的真正原因。
const CLAIM_PATH: &str = "/alice/ug/campaign/item/reward/claim/";

/// 领奖回执为「已受理但未出结果」时，等一拍再回查的时长。
///
/// 豆包的奖励发放是**异步**的：老领奖接口先回执「领取结果确认中，请稍后
/// 重试」，积分/任务状态随后才落地。触发完立刻回查大概率还是未完成。
const CLAIM_SETTLE_WAIT_MS: u64 = 8_000;

/// 「确实没确认到账」时的自动复核间隔：挂进 `retryAt`，由网关调度重跑本次
/// 签到。上游按天幂等，重跑不会重复发奖。
const CLAIM_RETRY_MS: i64 = 10 * 60_000;

/// 上游写接口整体报「系统错误」时的自动复核间隔。
///
/// 比 [`CLAIM_RETRY_MS`] 拉宽一倍：网关对每个账号每天只给 3 次自动重试
/// （`auto_checkin::retry` 的 `retryLimit` 缺省 3、`retryGapMinutes` 缺省 10），
/// 而「写接口整体不可用」这种故障通常以「几十分钟到几小时」计。10 分钟一档只覆盖
/// 半小时；20 分钟一档能覆盖一小时，且当天手动签到、启动补签仍会各自再打一次。
const SYSTEM_ERROR_RETRY_MS: i64 = 20 * 60_000;

/// 上游「系统错误」的业务码：`710010702`（`msg=系统错误 / message=system error`）
/// 与 `625002002`（`系统内部异常`）。
///
/// ── 为什么单认这一档（2026-11-10 实测）─────────────────────────
/// `landing/checkin` 对**任何通过鉴权的入参**都回 `710010702`：真实 scene、
/// 瞎编的 `scene`（`__no_such_campaign__`）、空 body、换 `aid`（1044603 /
/// 582478 / 497858）、只换活动组名 —— 全是同一条 710010702。而同一套
/// cookie/query 打 `v3/detail`（同一活动组）、`landing/get`（`main_page_info`
/// 此刻仍写着「今日签到领 200 额度」）、`quota/records`、`launch`、
/// `item/reward/claim/` **全部正常返回** —— 说明是**这条写接口自身**在上游侧
/// 不可用，不是请求构造问题。
///
/// 鉴权仍然先行：去掉 cookie 时同一接口回 `710012001 登录已过期`（不是
/// 710010702），所以这一档是「过了鉴权、死在业务处理里」的 5xx 语义，
/// 不是「你没登录」也不是「你没资格」。走 `item/reward/claim/` 领
/// `daily_signin` 仍只回 `biz_error code 3 领取结果确认中` 且状态永不落地
/// （见 [`CLAIM_PATH`] 的注），所以也没有旁路可绕。
///
/// 决定性证据：用官方 SDK（页面内 `incentiveTaskUgCampaignApiService.DailyCheckIn`，
/// 带官方 `a_bogus` + 真 `msToken`）在同一浏览器里直调，回执拿到的是**一模一样**的
/// 710010702（`logId 2026111014254054FE84F66D4B67EF94DB`）。也就是说官方自己此刻
/// 也签不上 —— 补 `a_bogus` 不可能解决这一档。
///
/// 这一档是**上游写接口的瞬时故障**（语义等同 5xx），不是「账号不可签」也不是
/// 「插件请求不对」，所以不能按 `Fatal` 落成一条 `failed`（那正是「用 agent2api
/// 签豆包就废了」的观感来源）。改为回查 + 挂 `retryAt`，等上游恢复后自动补领。
fn upstream_system_error(error: &GatewayError) -> bool {
    matches!(error.upstream_code, Some(710010702 | 625002002))
}

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
/// 签到现在走 [`CHECKIN_PATH`]，回执自带 `checked_in` 权威字段。
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

/// 每日签到。返回 SDK 的 [`CheckinResult`]：`report` 是协议化结果，
/// `retry_at` 是「稍后自动复核」的时刻 —— 必须一路带到网关
/// （`claim.retryAt` → `auto_checkin::retry`），否则「没确认到账」的那一场
/// 只能等用户手点，这正是此前的症状。
/// 与 `Provider::checkin` 的契约对齐：只要报告本身。
/// `retry_at` 这类附加信息走 [`checkin_with_retry`]（由 `checkin_with_options` 转发），
/// 否则「回查未确认」的那一场会丢掉自动复核时刻。
pub async fn checkin(
    ctx: &PluginCtx,
    account: &str,
) -> Result<proto::CheckinReport, GatewayError> {
    Ok(checkin_with_retry(ctx, account).await?.report)
}

pub async fn checkin_with_retry(
    ctx: &PluginCtx,
    account: &str,
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
    // 签到**不是**任务领奖：抓包证实 daily_signin 打 `item/reward/claim/` 只会
    // 回 biz_error code 3「领取结果确认中，请稍后重试」，状态永不落地——这正是
    // 「签到总是有问题」的根因。真正的签到是 landing/checkin，body 只带 scene
    // （= campaign_group_code），同步返回权威字段 checked_in。
    let response = match api::post(
        &client,
        &credentials,
        CHECKIN_PATH,
        &json!({"scene": task.group}),
    )
    .await
    {
        Ok(response) => response,
        Err(error) if upstream_system_error(&error) => {
            // 上游写接口当前报「系统错误」（见 [`upstream_system_error`]）：这一档
            // 连官方客户端都签不上，判 failed 只会让用户以为插件坏了。先回查一次
            // —— 上游偶尔在「已落库但响应组装失败」时也回这个码，真签上了就别报错；
            // 没签上就报中性 Inactive + retryAt，等上游恢复后由网关自动补领。
            crate::logging::verbose(
                "[DoubaoWork]",
                &format!(
                    "签到写接口报上游系统错误（code={:?}），已转回查 + 自动复核：{}",
                    error.upstream_code, error.message
                ),
            );
            return Ok(match refetch(&client, &credentials, &task.key, &task.group).await {
                Ok(Some((true, streak))) => CheckinResult {
                    report: proto::CheckinReport {
                        outcome: proto::CheckinOutcome::Claimed,
                        credit: task.points.map(|points| format!("{points} 额度")),
                        streak: streak.or(task.streak),
                        message: Some("豆包工作签到成功".into()),
                    },
                    retry_at: None,
                    details: None,
                },
                _ => CheckinResult {
                    report: report(
                        proto::CheckinOutcome::Inactive,
                        "豆包上游签到接口暂时报「系统错误」（官方客户端此刻同样签不上），已排自动复核，恢复后会自动补领",
                    ),
                    retry_at: Some(crate::http::now_ms() + SYSTEM_ERROR_RETRY_MS),
                    details: None,
                },
            });
        }
        Err(error) => return Err(error),
    };
    let data = response
        .get("data")
        .and_then(Value::as_object)
        .ok_or_else(|| GatewayError::with_status(502, "豆包工作未返回签到回执"))?;
    // biz_error 在这里只作「上游暂时不受理」的信号；成功判据只有 checked_in。
    if let Some(error) = data.get("biz_error").filter(|error| !error.is_null()) {
        if !settled_later(error) {
            return Err(api::error(200, error));
        }
        // 上游说「确认中」：等一拍再回查，多半就能看到状态翻转
        tokio::time::sleep(std::time::Duration::from_millis(CLAIM_SETTLE_WAIT_MS)).await;
    }
    // checked_in 是幂等的：当天已签再打一次仍回 true（并置 idempotent），所以
    // 重跑不会重复发奖，也不会误判失败。
    let confirmed = data.get("checked_in").and_then(Value::as_bool) == Some(true)
        || api::number(&response["data"]["reward_status"]) == Some(3)
        || api::number(&response["data"]["task_status"]) == Some(3);
    // 回查失败**不能**把整场判失败：签到请求已经发出去了，这里只是没读到权威
    // 状态。报中性 Inactive + retryAt，等网关复核即可。
    let after = if confirmed {
        None
    } else {
        match refetch(&client, &credentials, &task.key, &task.group).await {
            Ok(after) => after,
            Err(_) => {
                return Ok(CheckinResult {
                    report: report(
                        proto::CheckinOutcome::Inactive,
                        "豆包工作签到已提交，回查未成功，稍后自动复核",
                    ),
                    retry_at: Some(crate::http::now_ms() + CLAIM_RETRY_MS),
                    details: None,
                });
            }
        }
    };
    let done = confirmed || after.as_ref().is_some_and(|(done, _)| *done);
    if done {
        let points = crate::quota::number(response.pointer("/data/reward_amount")).or(task.points);
        return Ok(CheckinResult {
            report: proto::CheckinReport {
                outcome: proto::CheckinOutcome::Claimed,
                credit: points.map(|points| format!("{points} 额度")),
                streak: api::number(&response["data"]["continuous_days"])
                    .and_then(|days| u32::try_from(days).ok())
                    .or_else(|| after.and_then(|(_, streak)| streak))
                    .or(task.streak),
                message: Some("豆包工作签到成功".into()),
            },
            retry_at: None,
            details: None,
        });
    }

    // 走到这里是「本次没确认到账」：接口没报错，只是状态还没翻转。不判失败
    // （失败会让用户以为签到坏了），报中性 Inactive 挂 retryAt 让网关自动复核。
    Ok(CheckinResult {
        report: report(
            proto::CheckinOutcome::Inactive,
            "豆包工作签到未确认到账，稍后自动复核",
        ),
        retry_at: Some(crate::http::now_ms() + CLAIM_RETRY_MS),
        details: None,
    })
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
