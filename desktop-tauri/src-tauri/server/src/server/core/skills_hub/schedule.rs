//! 技能仓库的「每日自动巡检」调度。
//!
//! ── 为什么必须有这个模块 ─────────────────────────────────
//! 参考实现（AgentHub）在配置页给了三个开关：每日自动检测、执行时刻、
//! 巡检自动修复与纳管。Agent2API 之前只有 HTTP 接口 `daily-check/run`，
//! **没有任何定时器** —— 也就是说那三个开关即使画出来也是装饰品，
//! 面板关掉就再也不会有第二次巡检。这里补的就是「没人点也要跑」那一半。
//!
//! ── 判据：从「上次该跑的时刻」推，不从 now 推 ──────────────
//! 用户设 09:00，如果他 10:00 才开机，那一次该跑的巡检不能静默丢掉：
//! 从 `lastRun`（或首次从「今天 09:00」）往后推，推出来的时刻一旦过去，
//! 下一个 tick 就立刻补跑。这与记忆调度 `memory::schedule` 的口径一致，
//! 那边已经踩过「从 now 推 → 静默跳过」的坑。
//!
//! ── 为什么用 60 秒 tick 而不是精确 sleep 到点 ───────────────
//! 睡到点要靠一次长 `sleep`，用户中途改时刻、关开关，那条 sleep 不会醒；
//! 60 秒轮询最坏晚一分钟，但每次都能看到最新配置。巡检本身是几十毫秒的
//! 磁盘 stat，一分钟一次的开销可以忽略。

use serde_json::Value;
#[cfg(test)]
use serde_json::json;

use super::{invalidate, run_daily_check, tools};

/// 上一次自动巡检的时刻（epoch 毫秒），与手动巡检分开记。
///
/// 为什么不复用 `daily_check_status().checkedAt`：那个值是「最后一次巡检
/// 是什么时候」，手动点一次也会刷新它。拿它当自动调度的基准，会让「用户
/// 上午手动点了一下」变成「今晚这次自动巡检已经跑过了」，白等一天。
const LAST_AUTO_KEY: &str = "skillsHubDailyCheckAuto";

/// 轮询间隔：60 秒。最坏晚一分钟，换来「改配置立刻生效」。
const TICK_SECS: u64 = 60;

/// 读上次自动巡检时刻（没跑过给 `None`）。
fn last_auto_at() -> Option<i64> {
    crate::server::access::kv_get(LAST_AUTO_KEY)
        .and_then(|text| text.trim().parse::<i64>().ok())
        .filter(|value| *value > 0)
}

fn mark_auto_at(at: i64) {
    crate::server::access::kv_put(LAST_AUTO_KEY, &at.to_string());
}

/// `HH:MM` → `(hour, minute)`。与 `memory::schedule::parse_clock` 同一套规则，
/// 但那边是私有函数，这里保持「越界即 None」的语义自己实现一份，
/// 免得为了复用把记忆模块的内部函数提升成 pub。
fn parse_clock(text: &str) -> Option<(u32, u32)> {
    let (hour, minute) = text.trim().split_once(':')?;
    let hour: u32 = hour.trim().parse().ok()?;
    let minute: u32 = minute.trim().parse().ok()?;
    if hour > 23 || minute > 59 {
        return None;
    }
    Some((hour, minute))
}

/// 给配置收敛用：`HH:MM` 是否合法。`tools::load_config` 拿它判断
/// 「存进去的时刻能不能用」，坏值退回 09:00，避免调度器每分钟解析失败。
pub fn is_valid_clock(text: &str) -> bool {
    parse_clock(text).is_some()
}

/// `base` 之后（严格大于）的第一个「每天 HH:MM」本地时刻，epoch 毫秒。
///
/// 仅测试用（`next_daily_is_strictly_after` 验它不会原地打转）。
#[cfg(test)]
fn next_daily_after(base: i64, hour: u32, minute: u32) -> i64 {
    use chrono::{Local, NaiveTime, TimeZone};
    let moment = match Local.timestamp_millis_opt(base).single() {
        Some(value) => value,
        None => return base + 24 * 3600 * 1000,
    };
    let at = match NaiveTime::from_hms_opt(hour, minute, 0) {
        Some(value) => value,
        None => return base + 24 * 3600 * 1000,
    };
    let today = moment.date_naive();
    if let Some(today_at) = today
        .and_time(at)
        .and_local_timezone(Local)
        .single()
        .map(|value| value.timestamp_millis())
    {
        if today_at > base {
            return today_at;
        }
    }
    today
        .succ_opt()
        .unwrap_or(today)
        .and_time(at)
        .and_local_timezone(Local)
        .single()
        .map(|value| value.timestamp_millis())
        .unwrap_or(base + 24 * 3600 * 1000)
}

/// 「`now` 当天的 HH:MM」本地时刻，epoch 毫秒。
///
/// ── 为什么判定要按「今天这个点」而不是「上次之后的下一个点」──────
/// 用「上次之后的下一个点」判会出现两种错：
/// 1. 从没跑过（`last` 为空）时只能拿 `now - 25h` 当基准，推出来的是
///    **今天** 的 HH:MM，再对它推一次就变成明天 —— 开机晚于设定时刻
///    **永远补不上**，这正好是用户最需要的场景（白天开机、夜里关机）。
/// 2. 上次跑在三天前时，那个推法只往后推一天，仍然落在过去，
///    连续几个 tick 里反复补跑。
///
/// 改成「今天这个点到了没」：`now >= 今天 HH:MM` 且「上次自动巡检早于今天
/// 这个点」→ 跑一次。跨几天没开机也只补一次（不做 N 次补跑 —— 补跑三次
/// 巡检的价值是零，只会把日志刷满）。
///
/// 夏令时空档（`and_local_timezone` 回 Ambiguous/None）时退回 `now`，
/// 效果是「这一轮不跑、下一分钟再看」，宁可晚一分钟也不要算错日期。
fn today_at(now: i64, hour: u32, minute: u32) -> i64 {
    use chrono::{Local, NaiveTime, TimeZone};
    let moment = match Local.timestamp_millis_opt(now).single() {
        Some(value) => value,
        None => return now,
    };
    let at = match NaiveTime::from_hms_opt(hour, minute, 0) {
        Some(value) => value,
        None => return now,
    };
    moment
        .date_naive()
        .and_time(at)
        .and_local_timezone(Local)
        .single()
        .map(|value| value.timestamp_millis())
        .unwrap_or(now)
}

/// 判定「现在该不该跑」，纯函数（不读 kv、不碰磁盘），便于单测。
///
/// 把 `last` 作为参数传入而不是内部读 kv：测试才能构造「从没跑过」与
/// 「今天已经跑过」两种现场。
pub fn decide(schedule: Option<&Value>, now: i64, last: Option<i64>) -> bool {
    let enabled = schedule
        .and_then(|value| value.get("dailyCheck"))
        .and_then(Value::as_bool)
        .unwrap_or(true);
    if !enabled {
        return false;
    }
    let time_text = schedule
        .and_then(|value| value.get("dailyCheckTime"))
        .and_then(Value::as_str)
        .unwrap_or("09:00");
    let (hour, minute) = parse_clock(time_text).unwrap_or((9, 0));

    // 判定：今天这个点到了没 + 上次自动巡检是否早于今天这个点。
    //
    // 这两个条件合起来天然幂等：跑过一次后 `last` 变成「今天这个点之后」，
    // 同一天内后续 tick 一律不再触发；跨几天没开机，只补跑今天这一次。
    let due = today_at(now, hour, minute);
    if now < due {
        return false;
    }
    if let Some(previous) = last {
        if previous >= due {
            return false;
        }
    }
    true
}

/// 一次 tick 的判定：该跑就跑，返回 `Some(结果)`；不该跑给 `None`。
pub fn tick_once(config: &Value, now: i64) -> Option<Value> {
    let schedule = config.get("schedule");
    if !decide(schedule, now, last_auto_at()) {
        return None;
    }

    let auto_mount = schedule
        .and_then(|value| value.get("dailyCheckAutoMount"))
        .and_then(Value::as_bool)
        .unwrap_or(true);
    let result = run_daily_check(auto_mount);
    mark_auto_at(now);
    Some(result)
}

/// 起调度线程。重复调用只跑一轮（桌面壳重启后端时 `start` 可能被再次调用）。
pub fn spawn() {
    use std::sync::atomic::{AtomicBool, Ordering};

    static STARTED: AtomicBool = AtomicBool::new(false);
    // 用 `Once` 语义：第二次调用直接返回，不叠加第二条轮询线程。
    // 这里刻意不用 `swap` 后提前 return 的写法 —— 下面 `spawn_task` 是异步的，
    // 必须在同步阶段就决定「要不要起」，否则两条线程同时进 loop。
    if STARTED.swap(true, Ordering::SeqCst) {
        return;
    }

    // 这条线程没有停止条件：后端进程活着它就该在跑，进程退出线程随之消失。
    // 不做「可以外部关掉」的开关 —— 那是配置里 `schedule.dailyCheck` 的职责，
    // 在循环里每轮重读即可，不需要再引一层生命周期管理。
    crate::spawn_task(async move {
        let mut ticker = tokio::time::interval(std::time::Duration::from_secs(TICK_SECS));
        loop {
            ticker.tick().await;
            // 每轮重读配置：用户在面板上改时刻 / 关开关，下一分钟就生效，
            // 不需要重启后端。
            let config = tools::load_config();
            let now = crate::server::logging::now_ms();
            if let Some(result) = tick_once(&config, now) {
                invalidate();
                // 不额外写一条 `task_run` 台账：巡检结论本身就落在
                // `skillsHubDailyCheck` 里，配置页会直接读它展示状态条。
                // 再造一份台账只会让「自动化」页出现一堆没人点的记录。
                let ok = result.get("ok").and_then(Value::as_bool).unwrap_or(false);
                let summary = result
                    .get("summary")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string();
                crate::server::logging::log(
                    "[SkillsHub]",
                    &format!(
                        "每日自动巡检完成（{}）：{summary}",
                        if ok { "健康" } else { "有待处理" }
                    ),
                );
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 开关关掉时，任何时刻都不该跑。
    #[test]
    fn disabled_never_runs() {
        let config = json!({ "schedule": { "dailyCheck": false, "dailyCheckTime": "00:00" } });
        assert!(!decide(config.get("schedule"), 1_700_000_000_000, None));
    }

    /// 今天这个点还没到 → 不跑。
    #[test]
    fn not_due_yet() {
        use chrono::{Local, TimeZone};
        let today = Local::now().date_naive();
        let at_0800 = Local
            .from_local_datetime(&today.and_hms_opt(8, 0, 0).unwrap())
            .single()
            .unwrap()
            .timestamp_millis();
        let config = json!({ "schedule": { "dailyCheck": true, "dailyCheckTime": "09:00" } });
        assert!(
            !decide(config.get("schedule"), at_0800, None),
            "08:00 时 09:00 那次还没到，不该跑"
        );
    }

    /// 今天这个点已过、从没跑过 → 跑（白天开机补跑昨夜/今早那次）。
    #[test]
    fn due_and_never_ran() {
        use chrono::{Local, TimeZone};
        let today = Local::now().date_naive();
        let at_1000 = Local
            .from_local_datetime(&today.and_hms_opt(10, 0, 0).unwrap())
            .single()
            .unwrap()
            .timestamp_millis();
        let config = json!({ "schedule": { "dailyCheck": true, "dailyCheckTime": "09:00" } });
        assert!(
            decide(config.get("schedule"), at_1000, None),
            "10:00 时 09:00 已过且从没跑过，必须补跑"
        );
    }

    /// 今天这个点已过，但今天已经跑过 → 不重复跑（幂等）。
    #[test]
    fn due_but_already_ran_today() {
        use chrono::{Local, TimeZone};
        let today = Local::now().date_naive();
        let at_0930 = Local
            .from_local_datetime(&today.and_hms_opt(9, 30, 0).unwrap())
            .single()
            .unwrap()
            .timestamp_millis();
        let at_1000 = at_0930 + 30 * 60 * 1000;
        let config = json!({ "schedule": { "dailyCheck": true, "dailyCheckTime": "09:00" } });
        assert!(
            !decide(config.get("schedule"), at_1000, Some(at_0930)),
            "09:30 已经跑过，10:00 不该再跑一次"
        );
    }

    /// `HH:MM` 解析：合法值正常，越界给 None。
    #[test]
    fn clock_parsing() {
        assert_eq!(parse_clock("09:00"), Some((9, 0)));
        assert_eq!(parse_clock(" 23:59 "), Some((23, 59)));
        assert_eq!(parse_clock("24:00"), None);
        assert_eq!(parse_clock("9:70"), None);
        assert_eq!(parse_clock("abc"), None);
    }

    /// 从基准往后推的时刻一定严格大于基准（不会原地打转）。
    #[test]
    fn next_daily_is_strictly_after() {
        let base = 1_700_000_000_000;
        let next = next_daily_after(base, 9, 0);
        assert!(next > base, "推出来的时刻必须晚于基准，否则会死循环");
        let again = next_daily_after(next, 9, 0);
        assert!(again > next, "连续两次推必须是不同天的时刻");
    }

    /// `today_at` 必须落在「与 now 同一天」的 HH:MM 上。
    #[test]
    fn today_at_is_same_day() {
        use chrono::{Local, TimeZone};
        let now = 1_700_000_000_000;
        let at = today_at(now, 9, 0);
        let now_dt = Local.timestamp_millis_opt(now).unwrap();
        let at_dt = Local.timestamp_millis_opt(at).unwrap();
        assert_eq!(
            now_dt.date_naive(),
            at_dt.date_naive(),
            "today_at 不能漂到别的日期"
        );
        assert_eq!(at_dt.format("%H:%M").to_string(), "09:00");
    }
}