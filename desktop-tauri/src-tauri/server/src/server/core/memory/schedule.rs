//! 记忆自动化调度器。
//!
//! ── 这里调度的是什么 ────────────────────────────────────────
//! 十件事，按「要不要花 token」分两档：
//!
//! 零 token（纯本地，默认开）：
//!   · `index-scan` 增量重建索引 —— 索引是派生物，磁盘才是事实源；
//!   · `classify`  项目归档建议 —— 只算 slug 相似度，不调模型；
//!   · `cleanup`   回收站过期清理 + 队列过期。
//!
//! 花 token（默认全关，用户显式开才跑）：
//!   · `extract` / `summarize` / `tag` / `distill` / `profile` / `supersede` /
//!     `consolidate`。
//!
//! 默认关的理由：这东西在后台悄悄消耗用户的 API 额度。零 token 的活儿开了
//! 没风险（反正磁盘扫描本来也要做），有 token 的必须用户自己点头。
//!
//! ── 为什么是「扫描式」而不是「精确定时」 ─────────────────────
//! 一个每 60 秒醒一次的循环，检查每个任务「距上次跑够不够间隔」，够就跑。
//! 不用 cron 表达式、不用每个任务一条 tokio 定时器：
//!   · 桌面应用会睡眠/唤醒、会被关掉再打开，精确定时本来就会漂；
//!   · 十一个独立定时器在调试时无法一眼看出「现在到底有几个在跑」；
//!   · 扫描式天然幂等 —— 错过一次不会漏，只是晚一点。
//!
//! ── `health` 为什么被删（2026-11-24）──────────────────────────
//! 它跑的「锚定文件 mtime > 记忆 updated_at」检查在真机上 `stat` 成功 **0 次**
//! / 482 个 files 项：`files` 存的是**仓库相对路径**，检查用的是**记忆根**基准，
//! 差着整个仓库。按正确基准复跑，66 条能解析、66 条全部报「过时」——100% 误报。
//! 完整理由与取证数字见 `mod.rs` 里 `TASK_LABELS` 上方那段注释。
//!
//! ── 单写者 ──────────────────────────────────────────────────
//! 所有落盘都发生在调度线程里（`std::thread`，不是 tokio 任务）。记忆的事实源
//! 是文件，两个线程同时读改写同一个 `.md` 就是丢更新。AgentHub 用 stdio 桥把
//! 所有写操作串到一个进程里；本仓用一条专用线程 + 频道做同一件事。

use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::sync::mpsc::{self, Sender};
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

use serde_json::{json, Map, Value};

use super::enrich::{self, Outcome};
use super::{config, index, store};

/// 一次调度周期的间隔。60 秒足够细（最短任务间隔是 30 分钟），
/// 又不会让空闲时的 CPU 占用可见。
const TICK: Duration = Duration::from_secs(60);

/// 任务名清单（顺序即界面上的展示顺序）。
///
/// ── 为什么 `sync` 不在里面 ──────────────────────────────────
/// WebDAV 同步有自己的页面（`GET /api/memory/sync` 那一族），配置、账号、
/// 冲突裁决、预览、手动上传/下载都在那边，语义是「我点一下同步」而不是
/// 「后台每隔 N 分钟悄悄跑」。把它同时挂成第 12 个自动任务，等于让同一个
/// 动作有两个入口、两套开关、两份统计 —— 用户关掉卡片上的开关以为停了，
/// 同步页点一下还是照跑。所以调度器只认这 11 个，`sync` 模块本身照旧
/// 供同步页使用。
pub const TASKS: [&str; 11] = [
    "extract",
    "summarize",
    "tag",
    "classify",
    "supersede",
    "distill",
    "consolidate",
    "profile",
    "ingest",
    "index-scan",
    "cleanup",
];

/// 默认折叠时展示的执行记录行数。
///
/// ── 为什么是 10 而不是 30 ────────────────────────────────────
/// 这张表是「刚才发生了什么」的速览，不是列表页。30 行会把整页撑到两三屏：
/// 一轮 tick 会同时写 3~6 行（抽取 / 归类 / 画像各一条），30 行只覆盖最近
/// 五六轮，信息密度并不比 10 行高，但滚动距离翻了倍，而且把下面真正要动手
/// 的东西（裁决积压、任务卡）推到首屏之外。
///
/// 想翻全量走「展开」（`GET /api/memory/auto/runs?limit=50`）。
pub const PREVIEW_RUNS: usize = 10;

/// 「展开」一次最多给多少行。
///
/// ── 为什么是 50 ─────────────────────────────────────────────
/// 与目标界面对齐：展开态给「最新的 50 条」，够看清最近一两天的节奏，又不至于
/// 把 DOM 撑爆（每行 6 个格子，500 行会让滚动明显发涩）。再往前翻的需求属于
/// 「导出报告」，不该在页面上实现一遍分页。
pub const EXPANDED_RUNS: usize = 50;

/// 每个任务的默认节奏（分钟），与 `config.rs` 里 `auto.tasks` 的默认值保持一致。
fn default_interval(task: &str) -> i64 {
    match task {
        "classify" => 60,
        // 日常会话入库：半小时一轮，只读 mtime 变过的文件，没新会话时开销是
        // 「列一遍目录 + stat」，比它跑一次模型便宜五个数量级。
        "ingest" => 30,
        "index-scan" => 360,
        // 抽取是这批任务里唯一「跑得越勤、库越脏」的一个：每轮最多 3 条，
        // 半小时一轮就是每天上限 144 条，而真机一天的流水只有 85 条 ——
        // 产 l2 的速度比产 l1 还快。改成 2 小时一轮（每天上限 36 条），
        // 让它先有足够的流水可压缩，而不是每次都在同一批素材上重复抽取。
        "extract" => 120,
        // 按天跑的任务用 24h 表达；具体时刻由 `auto.tasks` 里的 `daily` 字段给，
        // 这里只是「两次之间至少隔多久」的兜底
        "supersede" | "distill" | "cleanup" => 1_440,
        "consolidate" | "profile" => 10_080,
        _ => 30,
    }
}

/// 任务是否启用。`auto.tasks[task].enabled`，缺省按「花不花 token」定。
fn task_enabled(task: &str) -> bool {
    let tasks = config::object_of("auto.tasks");
    let entry = tasks.get(task);
    if let Some(flag) = entry.and_then(|item| item.get("enabled")).and_then(Value::as_bool) {
        return flag;
    }
    // 表里没有这个任务：零 token 的默认开，花 token 的默认关。
    // 这里仍然显式写白名单、而不是跟 `task_uses_model` 取补集：万一将来加
    // 一个「不烧 token 但很吵」的任务（早先 `sync` 就是 —— 没配 WebDAV 时
    // 每分钟往日志里写一条「没配远端」），它不该因为免费就自动默认开启。
    //
    // `ingest` 在名单里：它不调模型（解析 + 写盘而已），而它的**存在意义**就是
    // 「不用人管」—— 默认关掉等于这个功能没发布。
    matches!(task, "classify" | "ingest" | "index-scan" | "cleanup")
}

/// 这个任务会不会调模型（即：开它会不会烧 token）。
///
/// ── 为什么单独一个函数，而不是在界面里写死一张表 ──────────────
/// 「哪个任务花 token」是实现事实，不是界面事实：`classify` 早先调过模型，
/// 后来改成纯 slug 相似度。这类变化只在后端可见，
/// 前端抄一份必然漂移 —— 漂移的后果是界面写着「零成本」而它在扣额度。
///
/// 口径是**当前实现**：`extract` / `summarize` / `tag` / `distill` /
/// `consolidate` / `profile` / `supersede` 会调 `llm`；其余四个纯本地
/// （`classify` / `ingest` / `index-scan` / `cleanup`）。
/// `task_enabled` 的「没配就默认关」用的也是这条线，两边同一份事实。
fn task_uses_model(task: &str) -> bool {
    matches!(
        task,
        "extract" | "summarize" | "tag" | "distill" | "consolidate" | "profile" | "supersede"
    )
}

fn task_interval(task: &str) -> i64 {
    let tasks = config::object_of("auto.tasks");
    let configured = tasks
        .get(task)
        .and_then(|item| item.get("intervalMin"))
        .and_then(Value::as_i64)
        .unwrap_or_else(|| default_interval(task));
    // `extract` 的下限单列一档：它每轮最多写 3 条记忆，跑太勤等于把同一批
    // 流水反复抽成多条结论（真机取证：配置写的 30 分钟，三天抽了 132 条）。
    // 这条下限是「数据质量」不是「性能」，配置里写 30 也按 120 执行。
    let floor = if task == "extract" { 120 } else { 1 };
    configured.max(floor)
}

fn task_batch(task: &str) -> usize {
    let tasks = config::object_of("auto.tasks");
    tasks
        .get(task)
        .and_then(|item| item.get("batchSize"))
        .and_then(Value::as_i64)
        .map(|value| value.clamp(1, 200) as usize)
        .unwrap_or(20)
}

/// 一个任务**当前生效的**节奏，给界面画「调度节奏：每隔 30 分钟」那一行。
///
/// ── 为什么要把「模式」发下去，而不是只发一个 `intervalMin` ─────
/// 三种节奏（每隔 N 分钟 / 每天 HH:MM / 每周第 N 天 HH:MM）在配置里是三组
/// 不同的字段，但界面上是同一句话。只发 `intervalMin` 的话，前端只能拿到
/// 「1440 分钟」这种等价但读起来不像人话的数字，于是页面会写「每 1 天」——
/// 而用户配的是「每天 11:00」。把模式和原始字段一起发下去，前端拼的句子
/// 与 `next_due` 用的是同一份事实，不需要自己从分钟数反推。
///
/// `weekly` 按 1 = 周一 … 7 = 周日（ISO），与 `first_weekly_after` 同口径。
fn task_cadence(task: &str) -> Value {
    let entry = config::object_of("auto.tasks");
    let item = entry.get(task);
    let daily = item.and_then(|value| value.get("daily")).and_then(Value::as_str).map(str::trim);
    let weekly_time = item
        .and_then(|value| value.get("weeklyTime"))
        .and_then(Value::as_str)
        .map(str::trim);
    let weekly = item.and_then(|value| value.get("weekly")).and_then(Value::as_i64);
    // 与 `next_due` 同一个优先级：按周优先，其次按天，最后按间隔。
    // 顺序反过来会出现「页面说每天 11:00、调度器其实按周跑」这种谎。
    if let (Some(day), Some(time)) = (weekly, weekly_time) {
        if parse_clock(time).is_some() {
            return json!({
                "mode": "weekly",
                "weekly": day.clamp(1, 7),
                "weeklyTime": time,
                "text": weekly_cadence_text(day.clamp(1, 7), time),
            });
        }
    }
    if let Some(time) = daily {
        if parse_clock(time).is_some() {
            return json!({
                "mode": "daily",
                "daily": time,
                "text": daily_cadence_text(time),
            });
        }
    }
    let minutes = task_interval(task);
    json!({
        "mode": "interval",
        "intervalMin": minutes,
        "text": interval_cadence_text(minutes),
    })
}

/// 「每隔 30 分钟 / 每隔 6 小时」。到天就退化成「每天」——「每隔 1 天」不像人话。
fn interval_cadence_text(minutes: i64) -> String {
    if minutes >= 1440 {
        let days = minutes / 1440;
        return if days == 1 { "每天".to_string() } else { format!("每隔 {days} 天") };
    }
    if minutes >= 60 && minutes % 60 == 0 {
        return format!("每隔 {} 小时", minutes / 60);
    }
    format!("每隔 {minutes} 分钟")
}

fn daily_cadence_text(time: &str) -> String {
    format!("每天 {time}")
}

fn weekly_cadence_text(day: i64, time: &str) -> String {
    let name = match day {
        1 => "周一",
        2 => "周二",
        3 => "周三",
        4 => "周四",
        5 => "周五",
        6 => "周六",
        _ => "周日",
    };
    format!("每{name} {time}")
}

/// 一次「任务改设置」请求：开关 + 节奏，两项都可缺省。
///
/// ── 为什么要有这个入口，而不是让页面自己拼 `auto.tasks` ──────
/// `auto.tasks` 是个三层嵌套的 Map（任务名 → 字段）。页面直接发
/// `{"auto.tasks": {"extract": {"enabled": true}}}` 能work（`patch` 对 Map
/// 做深合并），但发 `{"auto.tasks.extract": true}` 会被 `find()` 判成未知键
/// **静默丢掉** —— 开关看着点了、刷新又弹回去，且没有任何错误可看。
///
/// 节奏则更难拼对：改「每天 11:00」必须**同时删掉**旧的 `intervalMin`，
/// 否则 `next_due` 会因为按周/按天分支先命中而行为正确、但页面上的
/// 「每隔 N 分钟」残留下来变成谎言。这类「互斥字段要一起改」的规则属于
/// 调度器的语义，不该让前端抄一份。所以写入口收在这里：**页面只说
/// 「改成每天 11:00」，怎么落到那几个字段由调度器决定**。
struct TaskPatch {
    enabled: Option<bool>,
    cadence: Option<Cadence>,
}

enum Cadence {
    Interval(i64),
    Daily(String),
    Weekly(i64, String),
}

/// 解析一次任务的改动请求。形状：
/// `{"task":"extract","enabled":true,"cadence":{"mode":"daily","daily":"11:00"}}`
fn parse_task_patch(payload: &Value) -> Result<(String, TaskPatch), String> {
    let task = payload
        .get("task")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|text| !text.is_empty())
        .ok_or("缺少 task")?
        .to_string();
    if !TASKS.contains(&task.as_str()) {
        return Err(format!("未知任务：{task}（可选 {}）", TASKS.join(" / ")));
    }
    let enabled = match payload.get("enabled") {
        None | Some(Value::Null) => None,
        Some(Value::Bool(flag)) => Some(*flag),
        Some(_) => return Err("enabled 必须是布尔值".into()),
    };
    let cadence = match payload.get("cadence") {
        None | Some(Value::Null) => None,
        Some(value) => Some(parse_cadence(value)?),
    };
    if enabled.is_none() && cadence.is_none() {
        return Err("没有要改的字段".into());
    }
    Ok((task, TaskPatch { enabled, cadence }))
}

fn parse_cadence(value: &Value) -> Result<Cadence, String> {
    let mode = value
        .get("mode")
        .and_then(Value::as_str)
        .map(str::trim)
        .ok_or("cadence.mode 必须是 interval / daily / weekly")?;
    match mode {
        "interval" => {
            let minutes = value
                .get("intervalMin")
                .and_then(Value::as_i64)
                .ok_or("cadence.intervalMin 必须是整数分钟")?;
            if !(1..=43_200).contains(&minutes) {
                return Err("间隔必须在 1 分钟到 30 天之间".into());
            }
            Ok(Cadence::Interval(minutes))
        }
        "daily" => {
            let time = value.get("daily").and_then(Value::as_str).map(str::trim).ok_or("缺少 cadence.daily")?;
            let (hour, minute) = parse_clock(time).ok_or("cadence.daily 必须是 HH:MM（24 小时制）")?;
            Ok(Cadence::Daily(format!("{hour:02}:{minute:02}")))
        }
        "weekly" => {
            let day = value
                .get("weekly")
                .and_then(Value::as_i64)
                .ok_or("缺少 cadence.weekly（1 = 周一 … 7 = 周日）")?;
            if !(1..=7).contains(&day) {
                return Err("cadence.weekly 必须在 1（周一）到 7（周日）之间".into());
            }
            let time = value
                .get("weeklyTime")
                .and_then(Value::as_str)
                .map(str::trim)
                .ok_or("缺少 cadence.weeklyTime")?;
            let (hour, minute) = parse_clock(time).ok_or("cadence.weeklyTime 必须是 HH:MM（24 小时制）")?;
            Ok(Cadence::Weekly(day, format!("{hour:02}:{minute:02}")))
        }
        other => Err(format!("未知节奏模式：{other}（可选 interval / daily / weekly）")),
    }
}

/// 把一次任务改动写进配置，返回改动后的任务状态。
///
/// ── 为什么节奏用「整份替换这个任务条」而不是深合并 ──────────
/// 深合并删不掉键。从「每天 11:00」改成「每隔 30 分钟」时，旧的 `daily`
/// 会一直留在那一层里；`next_due` 先看 `daily`，于是用户明明改成了每 30
/// 分钟，调度器仍然每天只跑一次。`Stored::set` 的语义正是「整份替换」——
/// 它和 `patch` 走同一个 `coerce`，所以非法的值照样进不来。
///
/// ── 基准必须取「含默认值的完整表」，不能只取显式值 ────────────
/// `next.map_of("auto.tasks")` 只读**显式设置过**的那一份。首次写入时它是
/// 空对象，若拿它当基准，写完之后 `auto.tasks` 就变成「只有这一次改的那个
/// 任务」——其余十一个任务的默认字段（`supersede.daily = 11:00`、
/// `consolidate.weekly = 1` …）全部从生效值里消失，`next_due` 退回
/// 「上次 + 间隔」。表现是「我改了一下抽取的开关，其余按天的任务时间全乱了」，
/// 而且没有任何报错。所以基准按 `apply_one` 的同一口径取：
/// 显式值 → 否则 schema 默认值。
pub fn apply_task(payload: &Value) -> Result<Value, String> {
    let (task, patch) = parse_task_patch(payload)?;
    config::update(|next| {
        // 基准只在内存里取（显式值 → 默认值），不用 `config::object_of`：
        // 那个按 mtime 走缓存，在事务闭包内读到的是「改之前」的副本。
        let base = next
            .values
            .get("auto.tasks")
            .cloned()
            .or_else(|| config::default_of("auto.tasks"))
            .unwrap_or_else(|| Value::Object(Map::new()));
        let mut tasks = base.as_object().cloned().unwrap_or_default();
        let mut entry = tasks
            .get(&task)
            .and_then(Value::as_object)
            .cloned()
            .unwrap_or_default();
        if let Some(flag) = patch.enabled {
            entry.insert("enabled".into(), json!(flag));
        }
        if let Some(cadence) = &patch.cadence {
            // 三种节奏的字段互斥：先全部清掉，再按新模式写回。
            // 留着旧的等于让 `next_due` 的优先级分支替用户做决定。
            entry.remove("intervalMin");
            entry.remove("daily");
            entry.remove("weekly");
            entry.remove("weeklyTime");
            match cadence {
                Cadence::Interval(minutes) => {
                    entry.insert("intervalMin".into(), json!(minutes));
                }
                Cadence::Daily(time) => {
                    entry.insert("daily".into(), json!(time));
                }
                Cadence::Weekly(day, time) => {
                    entry.insert("weekly".into(), json!(day));
                    entry.insert("weeklyTime".into(), json!(time));
                }
            }
        }
        tasks.insert(task.clone(), Value::Object(entry));
        next.set("auto.tasks", Value::Object(tasks))?;
        Ok(())
    })
    .map(|_| status())
}

/// 上次执行时间（epoch ms）。
///
/// ── 内存 + 库里的一层 ───────────────────────────────────────
/// 进程内的 `LAST_RUN` 是热路径（每轮 tick 读 11 次），不能每次都查库；
/// 启动时用 `task_run` 的 `MAX(at)` 预热一次（[`hydrate_last_run`]），
/// 于是重启后页面不会说「还没跑过」，按天任务也不会在同一天跑两遍。
///
/// 两者为什么不合并成一张状态表：`task_run` 的 `at` 已经是事实，再存一份
/// 「上次」就是第二份真相（写失败时两边不一致）。这里只是它的内存缓存。
static LAST_RUN: OnceLock<Mutex<std::collections::HashMap<&'static str, i64>>> = OnceLock::new();

fn last_run() -> &'static Mutex<std::collections::HashMap<&'static str, i64>> {
    LAST_RUN.get_or_init(|| Mutex::new(std::collections::HashMap::new()))
}

/// 用库里的执行记录预热「上次」。调度线程启动时调一次。
///
/// 只在内存为空时填（内存里已有的值更新，不要被库里更旧的行覆盖 ——
/// 本进程刚跑过的那次可能还没写进库，或者写失败）。
fn hydrate_last_run() {
    let stored = index::global().task_last_runs();
    if stored.is_empty() {
        return;
    }
    if let Ok(mut slot) = last_run().lock() {
        for (task, at) in stored {
            // 库里可能留着已经删掉的任务名（配置改过、任务被移除），
            // 只认 `TASKS` 里还在的那些 —— 否则 `&'static str` 的键就会
            // 出现一个指向临时 `String` 的假键。
            let key = match TASKS.iter().copied().find(|name| *name == task.as_str()) {
                Some(key) => key,
                None => continue,
            };
            let current = slot.get(key).copied().unwrap_or(0);
            if at > current {
                slot.insert(key, at);
            }
        }
    }
}

fn stamp(task: &str, at: i64) {
    // `if let` 而不是 `contains` + `unwrap_or`：后者得给一个「不可能用到」的
    // 兜底任务名，而那个名字会悄悄变成对某个具体任务（早先是 `health`）的
    // 硬引用 —— 任务一删，兜底就成了指着一个不存在的键。
    let Some(key) = TASKS.iter().find(|name| **name == task).copied() else {
        return;
    };
    if let Ok(mut slot) = last_run().lock() {
        slot.insert(key, at);
    }
}

fn last_of(task: &str) -> i64 {
    last_run()
        .lock()
        .ok()
        .and_then(|slot| slot.get(task).copied())
        .unwrap_or(0)
}

/// 调度器是否在跑（重复启动只允许一份）。
static RUNNING: AtomicBool = AtomicBool::new(false);
/// 上一轮开始时间（状态页展示「刚刚检查过」）。
static LAST_TICK: AtomicI64 = AtomicI64::new(0);

/// 手动执行请求（管理 API 的「立即执行」）。
///
/// 走频道而不是直接调 [`run_once`]：执行必须发生在调度线程上，否则和自动轮次
/// 撞在一起，两条线程同时写同一个 `.md` 就会丢更新。
fn channel() -> &'static Mutex<Option<Sender<(String, Option<String>, Sender<Outcome>)>>> {
    static CHANNEL: OnceLock<Mutex<Option<Sender<(String, Option<String>, Sender<Outcome>)>>>> = OnceLock::new();
    CHANNEL.get_or_init(|| Mutex::new(None))
}

/// 启动调度线程。重复调用无副作用（桌面壳重启后端时会被再次调用）。
///
/// 为什么用 `std::thread` 而不是 tokio 任务：这条线程里做的全是阻塞的文件读写，
/// 放在 tokio worker 上会占住一个 worker 直到整轮跑完。独立的 OS 线程还能在
/// 网关正在关停时被单独丢弃。
pub fn spawn() {
    if RUNNING.swap(true, Ordering::SeqCst) {
        return;
    }
    let (tx, rx) = mpsc::channel::<(String, Option<String>, Sender<Outcome>)>();
    if let Ok(mut slot) = channel().lock() {
        *slot = Some(tx);
    }
    let spawned = std::thread::Builder::new()
        .name("memory-scheduler".to_string())
        .spawn(move || {
            crate::server::logging::log("[Memory]", "自动化调度线程已启动");
            // 先用库里已有的执行记录预热「上次」：重启后页面不该说「还没跑过」，
            // 按天任务也不该在同一天跑两遍（启动补跑 + 到点）。
            hydrate_last_run();
            loop {
                // 阻塞等「手动请求」或「到点」。等请求优先：用户点了「立即执行」
                // 之后最多几十毫秒就开始跑，而不是排到下一分钟。
                match rx.recv_timeout(TICK) {
                    Ok((task, project, reply)) => {
                        let outcome = run_once(&task, project.as_deref(), true);
                        let _ = reply.send(outcome);
                        // 手动请求跑完直接回循环：这次 tick 让给用户，
                        // 否则「点了个任务，结果它先跑了另外五个」很奇怪
                        continue;
                    }
                    Err(mpsc::RecvTimeoutError::Timeout) => {}
                    // 发送端被丢弃 = 进程在收尾，正常退出这条线程
                    Err(mpsc::RecvTimeoutError::Disconnected) => break,
                }
                LAST_TICK.store(crate::server::logging::now_ms(), Ordering::SeqCst);
                tick();
            }
            crate::server::logging::log("[Memory]", "自动化调度线程已退出");
        });
    if spawned.is_err() {
        RUNNING.store(false, Ordering::SeqCst);
        crate::server::logging::log("[Memory]", "⚠️ 自动化调度线程启动失败");
    }
}

/// 手动执行一个任务并等结果（管理 API 用）。
///
/// 调度线程没起来（或已经退出）时就地执行 —— 用户的「立即执行」不该因为
/// 后台线程状态而失败，代价只是这一次可能与自动轮次重叠（概率极低）。
pub fn run_now(task: &str, project: Option<&str>) -> Outcome {
    let sender = channel().lock().ok().and_then(|slot| slot.clone());
    if let Some(sender) = sender {
        let (reply_tx, reply_rx) = mpsc::channel();
        if sender.send((task.to_string(), project.map(str::to_string), reply_tx)).is_ok() {
            // 单个任务最长（蒸馏 200 条素材 + 模型 60s 超时）可能到两分钟
            if let Ok(outcome) = reply_rx.recv_timeout(Duration::from_secs(300)) {
                return outcome;
            }
        }
    }
    run_once(task, project, true)
}

/// 一轮扫描：把「到期且启用」的任务各跑一次。
fn tick() {
    if !config::bool_of(config::ENABLE_KEY) || !config::bool_of("auto.enabled") {
        return;
    }
    let now = crate::server::logging::now_ms();
    for task in TASKS {
        if !task_enabled(task) {
            continue;
        }
        // 与「下次」那一列同一个判据：配了 daily / weeklyTime 的任务按时刻，
        // 其余按「上次 + intervalMin」。这里不另写一套 —— 那会让「配置里写的
        // 11:00」变成一句摆设，而页面还照着 11:00 显示。
        // `next_due` 返回的时刻可能在过去（昨天 11:00 该跑、当时关机了），
        // `now < at` 不成立 → 立刻补跑，这正是重启后该发生的事。
        let at = next_due(task, last_of(task)).unwrap_or(now);
        if now < at {
            continue;
        }
        // `run_once` 内部会把「上次」戳成本轮开始时刻（失败也不在下一分钟重试，
        // 免得把日志刷满、网络故障期纯浪费）。用户可以在界面点「立即执行」重试。
        let started = crate::server::logging::now_ms();
        let outcome = run_once(task, None, false);
        let elapsed = crate::server::logging::now_ms() - started;
        match &outcome {
            Outcome::Done(payload) => crate::server::logging::log(
                "[Memory]",
                &format!("自动化 {task} 完成（{elapsed}ms）：{payload}"),
            ),
            Outcome::Skipped(reason) => {
                // 跳过是常态（没素材、预算用完），只在调试级别记
                crate::server::logging::log("[Memory]", &format!("自动化 {task} 跳过：{reason}"));
            }
            Outcome::Failed(error) => {
                crate::server::logging::log("[Memory]", &format!("⚠️ 自动化 {task} 失败：{error}"));
            }
        }
    }
    // 队列过期与回收站清理跟着 cleanup 一起做，但即使 cleanup 关着也要清队列：
    // 一条 pending 建议躺了 30 天还在，只会让「待确认 128 条」永远降不下来
    // ── 队列收尾：先自动裁决够格的，再让超期的归档 ─────────────────
    //
    // 顺序不能反：超期归档（`review_expire`）会把 pending 直接标成 expired ——
    // 一条 0.99 的精确重复如果恰好在第 31 天被人想起来，它已经「过期」了，
    // 而它原本可以被自动处理掉。先扫一遍能自动的，剩下的才谈过期。
    //
    // 每轮 tick（60 秒）都扫是故意的：旧建议是用户在队列里翻到的，不是
    // 等下一轮巡检才出现。`TICK` 的间隔远大于一次 500 行的 `review_list`。
    let swept = super::auto_resolve(200);
    let applied = swept.get("applied").and_then(Value::as_u64).unwrap_or(0);
    if applied > 0 {
        crate::server::logging::log(
            "[Memory]",
            &format!(
                "队列自动裁决 {applied} 条（扫 {}，仍待人工 {}）",
                swept.get("scanned").and_then(Value::as_u64).unwrap_or(0),
                swept.get("kept").and_then(Value::as_u64).unwrap_or(0),
            ),
        );
    }
    let expired = index::global().review_expire(30);
    if expired > 0 {
        crate::server::logging::log("[Memory]", &format!("过期待确认建议已归档 {expired} 条"));
    }
}

/// 执行单个任务。`manual` = 用户点按钮触发，会绕过预算与开关的部分限制。
///
/// ── 为什么统计与审计包在这一层而不是每个任务内部 ──────────────
/// 这条函数是**唯一**知道「这一轮从哪开始、到哪结束」的地方，也是唯一同时
/// 拿到 `manual` 的地方（自动轮次与手动点击的审计行要能区分）。各任务自己
/// 的返回值只有 `Outcome`，没有耗时与 token 合计；在这里埋一圈计时即可，
/// 不用去改十一个任务的签名。
///
/// ── 三个入口都走它 ────────────────────────────────────────
/// 自动 tick、频道里的手动请求、调度线程没起来时的就地执行，最终都落到
/// `run_once`。所以审计写在**这里**，三处自动都有了，不会漏掉某条路径
/// （漏一条就会出现「任务明明跑过，审计里没有」这种最难查的账）。
///
/// ── 为什么 token 用 `llm_call` 的增量算，而不是读 `Outcome` 载荷 ──
/// 只有少数任务的载荷里带 `tokens`（蒸馏带、摘要不带），字段名还各不相同。
/// 跑前读一次「今日累计」、跑后再读一次，差就是这一轮烧掉的 —— 与载荷无关，
/// 十一个任务一套逻辑。
pub fn run_once(task: &str, project: Option<&str>, manual: bool) -> Outcome {
    let before = index::global().tokens_today();
    let started = crate::server::logging::now_ms();
    let outcome = run_inner(task, project, manual);
    record(task, started, before, outcome, manual)
}

/// 立即重生成画像（可只重算一类）。`sub` 为空 = 四类全量。
///
/// ── 为什么不复用 `run_now("profile")` ────────────────────────
/// `run_now` 的任务名是 `TASKS` 里的枚举，审计表按它归类；「只重算技术偏好」
/// 与「全量重算」在审计里都该记成 `profile` 一行（否则任务列表会多出四个
/// 只在手动时才有的假任务）。所以这里走 `run_once` 的同一套记账，只是把
/// `sub` 透给 `build_profile`。
///
/// ── 为什么不等调度线程 ──────────────────────────────────────
/// 用户的「单独生成」点完就要看到结果（这一页的结论会整块换掉）。走频道会让
/// 它排在下一轮 tick 后面，用户以为按钮没反应。代价是可能与自动轮次重叠 ——
/// 但 `store` 的写锁 + 「替换 → 写入」的顺序保证了最坏情况是白算一次，不会
/// 产生两份同 id 的活画像。
pub fn run_profile_now(sub: Option<&str>) -> Outcome {
    let before = index::global().tokens_today();
    let started = crate::server::logging::now_ms();
    let outcome = if !config::bool_of(config::ENABLE_KEY) {
        Outcome::Skipped("记忆中枢已关闭".into())
    } else if config::bool_of("privacy.pause") {
        Outcome::Skipped("隐私模式已开启（privacy.pause），自动化暂停".into())
    } else {
        enrich::build_profile(sub, true)
    };
    record("profile", started, before, outcome, true)
}

/// 执行 + 记账（手动与自动共用）。抽成函数是为了让「只重算一类」也走同一套
/// token 口径与审计行 —— 抄两份记账代码的下一步必然是两份口径。
fn record(task: &str, started: i64, tokens_before: i64, outcome: Outcome, manual: bool) -> Outcome {
    let latency = crate::server::logging::now_ms() - started;
    let tokens = (index::global().tokens_today() - tokens_before).max(0);
    // 手动执行同样算「跑过一次」：不然点了「立即执行」之后，下一轮 tick
    // 会因为「距上次（自动那次）够久了」立刻再跑一遍 —— 用户看到的是
    // 「我手动跑了一次，它自己又跑了一次」。自动轮次在 `tick` 里已先
    // 戳过时间（为了失败不重试），这里戳的是同一件事，覆盖无害。
    stamp(task, started);
    index::global().log_task_run(
        task,
        outcome.label(),
        Some(&detail_of(task, &outcome)),
        tokens,
        latency,
        manual,
    );
    outcome
}

/// 任务本体（不含审计）。
fn run_inner(task: &str, project: Option<&str>, manual: bool) -> Outcome {
    if !config::bool_of(config::ENABLE_KEY) {
        return Outcome::Skipped("记忆中枢已关闭".into());
    }
    // 隐私模式：暂停一切采集与生成。手动执行也拦 —— 用户按了暂停就是暂停
    if config::bool_of("privacy.pause") {
        return Outcome::Skipped("隐私模式已开启（privacy.pause），自动化暂停".into());
    }
    if !manual && !config::bool_of("auto.enabled") {
        return Outcome::Skipped("自动化总开关已关闭".into());
    }
    match task {
        "index-scan" => index_scan(),
        "ingest" => ingest(),
        "classify" => classify_pending(project),
        "cleanup" => cleanup(manual),
        "extract" => extract(project, manual),
        "summarize" => enrich::summarize(&[], task_batch("summarize")),
        "tag" => enrich::tag(&[], false, task_batch("tag")),
        "distill" => enrich::distill(project, 0),
        "profile" => enrich::build_profile(None, manual),
        "supersede" => supersede_recent(project),
        "consolidate" => consolidate(project, manual),
        other => Outcome::Skipped(format!("未知任务：{other}")),
    }
}

/// 索引对账：读全部文件 → 重建派生表。
///
/// 全量重建而不是增量：记忆规模是几百到几千，一次重建在几十毫秒到一两百毫秒；
/// 而增量需要「哪些文件改过」的可靠判据（mtime 会被同步工具改、会被编辑器保留），
/// 判错的后果是索引与事实源长期不一致。
fn index_scan() -> Outcome {
    let entries = store::load_all();
    match index::global().rebuild(&entries) {
        Ok(count) => Outcome::Done(json!({ "indexed": count })),
        Err(error) => Outcome::Failed(format!("重建索引失败：{error}")),
    }
}

/// 日常会话增量入库：把客户端新写的会话搬进记忆。
///
/// ── 这个任务补的是什么洞 ────────────────────────────────────
/// 记忆中枢原来只有两条写入路径：模型主动调 `memory_write`、用户手动点「导入」。
/// 两条都要求**有人主动做**，于是「用了一周，库里还是几十条」成了默认结局 ——
/// 用户看到的是「已接入、没有调用记录、也没有记忆」，而事实是日常对话全躺在
/// `~/.workbuddy/projects/*.jsonl` 里没人搬。
///
/// 这项任务把它变成后台常态：按文件 mtime 增量读，走与手动导入完全相同的解析
/// 与写入链路（窗口切条、原始时间透传、hash 去重），所以「手动导过的」不会被
/// 再写一遍。
///
/// ── 为什么开关放在 `import.autoIngest` 而不是只靠任务开关 ────
/// 任务卡上那个开关是「这个任务跑不跑」，而 `import.autoIngest` 是「自动入库
/// 这件事允不允许」。两个开关的意义不同：前者用于「最近先别跑」，后者是隐私
/// 层面的总闸 —— 用户说「别自动收集我的对话」时要关的是后者，而且关了之后
/// 即使有人在任务卡上把 `ingest` 打开也不该开始搬。
fn ingest() -> Outcome {
    if !config::bool_of("import.autoIngest") {
        return Outcome::Skipped("自动入库已关闭（import.autoIngest）".into());
    }
    // 与手动导入互斥：两条路同时写同一个 l1 容器会丢更新（`store` 的写锁只保证
    // 单次 save 原子，不保证「读-改-写」整段互斥）。
    if super::import::is_running() {
        return Outcome::Skipped("手动导入正在进行，本轮跳过".into());
    }
    match super::import::run_auto(&|_, _| {}) {
        Ok(summary) => {
            let written = summary.get("written").and_then(Value::as_i64).unwrap_or(0);
            let deferred = summary.get("deferred").and_then(Value::as_i64).unwrap_or(0);
            // ── 有积压时不能说「没有新会话」 ────────────────────────
            // 积压意味着「确实有新文件，但这轮没轮到」，而且水位被故意冻住了。
            // 若这里仍报 `Skipped("没有新会话")`，审计里就只剩「一切正常」，
            // 而积压会一轮一轮地重复 —— 用户永远看不到「还有东西没进来」。
            if written == 0 && deferred == 0 {
                // 「一条没写」是常态（没新会话），不该在审计里刷成一次成功 ——
                // 半小时一条「写入 0 条」会把真正有内容的轮次埋掉。
                return Outcome::Skipped("没有新会话".into());
            }
            Outcome::Done(json!({
                "written": written,
                "duplicated": summary.get("duplicated").and_then(Value::as_i64).unwrap_or(0),
                "unchanged": summary.get("unchanged").and_then(Value::as_i64).unwrap_or(0),
                "scanned": summary.get("scanned").and_then(Value::as_i64).unwrap_or(0),
                "deferred": deferred,
            }))
        }
        Err(error) => Outcome::Failed(format!("会话入库失败：{error}")),
    }
}

/// 一条人能读的「这一轮干了什么」。
///
/// ── 为什么审计里存一句人话而不是原始 JSON ────────────────────
/// 「任务执行审计」那张表的用途是回答「昨天它到底动了什么」，不是在页面里
/// 再解析一遍结构。载荷里的键名各任务都不同（`written` / `tagged` /
/// `summarized` / `queued` / `indexed`…），把 JSON 原样塞进去，用户看到的是
/// `{"queued":4,"checked":100}` —— 得自己翻译。
///
/// ── 为什么按任务名分支而不是按载荷键猜 ──────────────────────
/// 同一句话在 `extract` 里是「新建 N 条候选」、在 `consolidate` 里是「合并
/// N 条」，语义不同却都叫 `written` / `queued`。按任务名写死措辞比按键名猜更准。
/// 每个分支都做了「键可能不存在」的兜底（`unwrap_or(0)`），因为任务的载荷
/// 形状会随实现调整，而这个函数只该影响展示，绝不该 panic。
fn detail_of(task: &str, outcome: &Outcome) -> String {
    match outcome {
        Outcome::Skipped(reason) => reason.clone(),
        Outcome::Failed(error) => error.clone(),
        Outcome::Done(payload) => {
            let n = |key: &str| payload.get(key).and_then(Value::as_i64).unwrap_or(0);
            match task {
                // 「处理 N 条，更新 M 条」—— 目标界面里最有信息量的一栏：
                // 读到几条、写了几个是两件事（有些挡在去重上）。
                // 「处理 N 条，新建 C 条候选，确认 R 条」—— 三个数是三种结局：
                // 读到多少流水、其中多少是清单里没有的新话题、多少只是又一次
                // 确认。以前只报「写入 N 条」，读起来像「抽了 3 条就沉淀了 3 条」，
                // 恰好掩盖了 l2 泛滥这件事。
                "extract" => {
                    let (created, reinforced, promoted) = (n("created"), n("reinforced"), n("promoted"));
                    if promoted > 0 {
                        format!(
                            "处理 {} 条，新建 {created} 条候选，确认 {reinforced} 条（{promoted} 条升沉淀）",
                            n("material")
                        )
                    } else {
                        format!("处理 {} 条，新建 {created} 条候选，确认 {reinforced} 条", n("material"))
                    }
                }
                "summarize" => format!("生成摘要 {} 条", n("summarized")),
                "tag" => format!("打标 {} 条", n("tagged")),
                "distill" => {
                    let (queued, applied, reinforced) = (n("queued"), n("applied"), n("reinforced"));
                    format!(
                        "读 {} 条素材，新建 {queued} 条（落地 {applied} 条），强化 {reinforced} 条",
                        n("material")
                    )
                }
                // 归类巡检报两个数：扫了多少条没项目的、其中新提了几条建议。
                // 只报「扫描 0 条」读起来像它没干活；只报建议数又在全部 skipped
                // 的轮次里没有内容 —— 两件事分开写，用户看得出「扫过了、没命中」
                // 与「扫都没扫」的区别。
                "classify" => format!("扫描 {} 条未归类，新增 {} 条建议", n("unclassified"), n("suggested")),
                "supersede" => format!("检查 {} 条，产出 {} 条失效建议", n("checked"), n("suggestions")),
                "consolidate" => format!("检查 {} 条，产出 {} 条合并建议", n("checked"), n("queued")),
                // 「保留 N 条置顶」只在真的有钉子时出现：大多数轮次没有用户钉的
                // 结论，恒显示「保留 0 条」会让审计行看起来像是漏了什么。
                "profile" => {
                    let kept = n("kept");
                    if kept > 0 {
                        format!("写入 {} 段画像，覆盖 {} 段，保留 {kept} 条置顶", n("written"), n("replaced"))
                    } else {
                        format!("写入 {} 段画像，覆盖 {} 段", n("written"), n("replaced"))
                    }
                }
                "index-scan" => format!("重建索引 {} 条", n("indexed")),
                "ingest" => {
                    // 有积压时把「还有多少没进来」写进审计，否则那一轮看起来
                    // 与「真的没有新会话」完全一样。
                    let backlog = n("deferred");
                    if backlog > 0 {
                        format!(
                            "写入 {} 条新记忆（扫描 {} 条候选，重复 {} 条，积压 {backlog} 个文件下轮继续）",
                            n("written"), n("scanned"), n("duplicated")
                        )
                    } else {
                        format!(
                            "写入 {} 条新记忆（扫描 {} 条候选，重复 {} 条）",
                            n("written"), n("scanned"), n("duplicated")
                        )
                    }
                }
                "cleanup" => format!("清回收站 {} 条，失效入回收站 {} 条", n("trashPurged"), n("supersededTrashed")),
                // 将来加了任务但这里没跟上：不编措辞，把载荷压成一行即可，
                // 至少能看出「它确实干了点什么」。
                //
                // 截断按**字符**数而不是字节数：`&text[..120]` 在中文载荷上
                // 会切在 UTF-8 码点中间直接 panic（一个汉字三字节，120 常落在
                // 汉字内部）—— 一句展示文案不值得让整条调度线程崩掉。
                _ => short(payload.to_string(), 120),
            }
        }
    }
}

/// 压成一行并截到 `max` 个字符（不是字节）。
fn short(text: String, max: usize) -> String {
    if text.chars().count() <= max {
        return text;
    }
    let cut: String = text.chars().take(max).collect();
    format!("{cut}…")
}

/// 归类巡检：给没有项目的活条目补「项目归属建议」。
///
/// ── 这条链路的两半 ──────────────────────────────────────────
/// 写入时那一半：写方带着 cwd，`layout::classify` 读 `git remote` / 目录名，
/// 模糊命中已登记项目就顺手入队一条建议（`write_inner` 里的 `enqueue`）。
///
/// 但那个前提是「写方报了 cwd」。Agent 不报 cwd、批量导入、旧版本写下的条目，
/// 都走不到那一步 —— 它们会永远停在 `general/` 里，没人再回头看。这项巡检补的
/// 就是这一半：**只用条目自己带得动的线索**（`files` 的路径段、标签、标题），
/// 与已登记项目名做 `name_similarity`，过阈值就入队一条建议。
///
/// ── 为什么只发建议、不直接归档 ──────────────────────────────
/// 没有 cwd 就没有事实依据，只有名字像。归错项目的代价高于「先留在全局」，
/// 所以巡检**无论 `classify.autoCreateProject` 开没开都只入待确认** —— 那个
/// 开关是给写入侧的（它手里有 git remote，证据强度不一样）。人点头后
/// `apply_classification` 才真的搬文件，且可反悔。
///
/// ── 为什么不比正文 ──────────────────────────────────────────
/// `name_similarity` 是给**短名字**算的（归一化 + 编辑距离 + 包含关系）。
/// 整段正文与项目名比，分数恒接近 0，除了给自己制造噪音没有别的作用。
fn classify_pending(project: Option<&str>) -> Outcome {
    // 项目范围这项巡检用不上：它处理的恰恰是「没有项目」的条目，再按
    // `Some(slug)` 过滤就是自相矛盾的条件集合。早先这里正是这么写的 ——
    // 带范围调用时两个过滤器互斥，结果恒为空，永远报「没有未归类的记忆」。
    // 参数留着只为与其它任务同形。
    let _ = project;
    let pending = index::global().review_count();
    if pending >= config::usize_of("dedup.pendingWarnThreshold").max(10) {
        return Outcome::Skipped(format!("待确认已积压 {pending} 条，先处理队列再说"));
    }
    let entries = store::load_all();
    let unclassified: Vec<_> = entries
        .iter()
        // 画像不是「待归类」的东西：它按设计就不属于任何项目。
        // 被反驳的结论也不必归类：给它打项目标签只会让错误看起来像正式归档。
        .filter(|entry| entry.is_trusted() && entry.project.is_none())
        .filter(|entry| entry.kind != super::MemoryKind::Profile)
        .collect();
    if unclassified.is_empty() {
        return Outcome::Skipped("没有未归类的记忆".into());
    }
    let projects = store::load_projects();
    if projects.is_empty() {
        return Outcome::Skipped("还没登记过任何项目（没有可比对的归档）".into());
    }
    let threshold = config::number("classify.fuzzyThreshold");
    // 已有待确认建议的条目不再重复插：`enqueue` 自身幂等，但它命中的是更新
    // 旧行，不告诉我们「这次真的新增了吗」—— 统计会每小时报同一个数字。
    let known: std::collections::HashSet<String> = index::global()
        .review_list(Some("pending"), Some("classify"), 1000)
        .into_iter()
        .map(|row| row.target)
        .collect();
    let mut suggested = 0usize;
    let mut queued_ids: Vec<i64> = Vec::new();
    for entry in unclassified.iter().copied() {
        if known.contains(&entry.id) {
            continue;
        }
        let Some((slug, confidence, evidence)) = best_project_match(entry, &projects, threshold) else {
            continue;
        };
        // ── 只有路径证据才入队 ──────────────────────────────────
        // 标签/标题命中（`Mention`）不进队列，理由见 `ClassifyEvidence`：真机上
        // 这批全是「在讨论某个项目」，归进去就是把别人的架构笔记记成自己的活。
        // 不入队不等于丢掉线索：`suggested` 照样计数，日志里能看出「这轮有多少
        // 条提到了某个项目」，只是不产出需要人点头的条目。
        if evidence == ClassifyEvidence::Mention {
            suggested += 1;
            continue;
        }
        if let Some(id) = index::global().enqueue(
            "classify",
            &entry.id,
            &json!({ "suggestion": slug, "confidence": confidence, "foundBy": "scan", "evidence": evidence.as_str() }),
            confidence,
        ) {
            queued_ids.push(id);
        }
        suggested += 1;
    }
    // ── 入队后立刻问一次「够不够格自动归」────────────────────────
    // 这条路以前只入队，于是每小时把同一批建议重新确认一遍、队列只增不减
    // （真机 17 条 classify 躺了好几天没人点）。判据在 `auto_eligible`，这里
    // 不另写一份 —— 两处独立维护必然漂开，「巡检说该自动处理的，队列里却一直
    // 留着」就是这么来的。
    //
    // 批末统一扫一次，不是每条扫一次：`auto_resolve_ids` 每次要读一遍 pending
    // 快照，放进循环就是每轮 20 次 `review_list`。
    let auto = super::auto_resolve_ids(&queued_ids);
    Outcome::Done(json!({
        "unclassified": unclassified.len(),
        "suggested": suggested,
        "autoApplied": auto,
    }))
}

/// 归类线索的强度。决定「能不能自动搬」，也决定「值不值得进队列」。
///
/// ── 为什么必须分等级，而不是一个分数 ──────────────────────────
/// 真机实测把这件事钉死了：562 个记忆文件里带 `files` 的有 81 条，而路径段命中
/// 已登记项目目录的是 **0 条** —— `files` 存的是 `src/server/...` 这类仓库内相对
/// 路径，从来不含项目目录名。于是巡检只剩标签和标题可用。而 279 条未归类条目里，
/// 靠标签/标题能凑出 13 条「像某项目」的建议 —— 逐条看下去全是这种：
///
///   `agenthub几万条对比` / `AgentHub画像设计` / `AgentHub记忆分层` / `AgentHub`
///   标题「画像不是深层记忆：它是 l2 的下游产物，不进 mem 表」
///
/// 讲的全是**这个项目是怎么设计的**，也就是「我们在讨论 agenthub」，而不是
/// 「这些是我们给 agenthub 干的活」。把它们归档进 `projects/agenthub/`，等于
/// 把「读别人的代码写的心得」记进「给别人干的活」。
///
/// 区别不在分数高低，在**证据类型**：路径是「这条记忆落在这个仓库的哪个文件」——
/// 唯一能证明归属的东西；标题/标签里的词只说明「这条在讲什么」。前者能自动，
/// 后者只能当一句提示，而且提示量大了就是纯噪音。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ClassifyEvidence {
    /// `files` 里的路径段命中项目目录：可自动归档
    Path,
    /// 只有标签 / 标题提到项目名：只说明在讲它，不该自动也不该入队
    Mention,
}

impl ClassifyEvidence {
    fn as_str(self) -> &'static str {
        match self {
            ClassifyEvidence::Path => "path",
            ClassifyEvidence::Mention => "mention",
        }
    }
}

/// 一条未归类记忆最像哪个已登记项目，以及凭什么。
///
/// 取「最像的那一项线索」而不是全部平均：一条记忆挂十个文件时，只有路径里
/// 带项目名的那一个是有证据的，其余都是背景噪音 —— 平均会把证据摊薄到阈值
/// 以下，于是明明有线索也发不出建议。
///
/// ── 路径优先，且路径一旦命中就不再退回标签 ──────────────────
/// 两级证据不是「加权平均」而是「有高等级就不用低等级」：路径命中是事实，
/// 标签命中是措辞。两者混在一起打分的话，一个写得很长的复合标签会靠字符串
/// 包含关系压过一条真实的路径证据。
fn best_project_match(
    entry: &super::model::MemoryEntry,
    projects: &[store::Project],
    threshold: f64,
) -> Option<(String, f64, ClassifyEvidence)> {
    // ── 第一级：`files` 的路径段 ────────────────────────────────
    // 只认路径里出现的项目目录名，不比文件名：`lib.rs` / `index.ts` 到处都是，
    // 拿它当线索等于给每条记忆都发一张「像某个项目」的通行证。
    let mut path_hits: Vec<String> = Vec::new();
    for file in &entry.files {
        for part in std::path::Path::new(file).components() {
            if let std::path::Component::Normal(segment) = part {
                path_hits.push(segment.to_string_lossy().to_string());
            }
        }
    }
    if let Some((slug, score)) = best_of(&path_hits, projects, threshold) {
        return Some((slug, score, ClassifyEvidence::Path));
    }

    // ── 第二级：标签与标题 ──────────────────────────────────────
    // 这一级只在「有路径但不命中」时才跑到。它的用途是把建议摆到人眼前，
    // 不是替人做决定 —— 见 `ClassifyEvidence` 的说明。
    let mut hints: Vec<String> = entry.tags.clone();
    if !entry.title.is_empty() {
        hints.push(entry.title.clone());
    }
    best_of(&hints, projects, threshold).map(|(slug, score)| (slug, score, ClassifyEvidence::Mention))
}

/// 在一组线索里找最像的项目。`threshold` 以下是「都不像」。
fn best_of(hints: &[String], projects: &[store::Project], threshold: f64) -> Option<(String, f64)> {
    if hints.is_empty() {
        return None;
    }
    let mut best: Option<(String, f64)> = None;
    for project in projects {
        let score = hints.iter().fold(0.0f64, |acc, hint| {
            let score = super::layout::name_similarity(hint, &project.name)
                .max(super::layout::name_similarity(hint, &project.slug));
            acc.max(score)
        });
        if score < threshold {
            continue;
        }
        if best.as_ref().map(|current| score > current.1).unwrap_or(true) {
            best = Some((project.slug.clone(), score));
        }
    }
    best
}

/// 清理：回收站过期文件 + 五类异常记忆入回收站（五类默认全关）。
///
/// ── 五个开关各自对应什么 ───────────────────────────────────
/// 它们的区别是**判据**，不是「删什么」：
///
/// | 开关 | 判据 | 处理 |
/// |---|---|---|
/// | `autoDeleteSuperseded` | `superseded_by` 非空 / 过了 `valid_to` | 整份进回收站 |
/// | `autoDeleteMissing` | 索引里有、磁盘上没这个文件 | 只清索引行（没内容可捡） |
/// | `autoDeleteError` | 文件在、读出来解析异常（空正文 + 无 frontmatter） | 整份进回收站 |
/// | `autoDeleteConflict` | `sync_conflict` 里有 pending 记录 | 整份进回收站 |
/// | `autoDeleteInvalid` | 正文空壳、进不了索引的残骸 | 整份进回收站 |
///
/// 为什么要分开而不是合成一个「清理垃圾」：这五类里只有「过期」是客观判据，
/// 其余四类都可能误判（权限故障读不出来、同步正在传、用户手写的空模板）。
/// 合成一个开关意味着开启「过期」就顺手清掉四类可疑文件 —— 那是拿数据赌。
///
/// 共同的排除项：画像（`Profile`）与钉住（`pinned`）的条目一律不动。它们是
/// 「要一直看见」的东西，任何自动清理都不该碰。
fn cleanup(manual: bool) -> Outcome {
    let days = config::usize_of("storage.trashKeepDays").max(1);
    let removed = store::purge_expired_trash(days);
    // 同步删除留档共用同一个保留窗口：它是「删错了能捞回来」的保险，
    // 不是永久归档。不清的话一批误判删除会在磁盘上留一辈子。
    let archived = store::purge_expired_sync_trash(days);
    // 按天清理调用台账（`auto.logKeepDays`）。放在回收站清理旁边：
    // 两者都是「到期就真删」的动作，同一个定时任务里做，用户只需要理解
    // 「清理任务会按保留窗口把过期东西删掉」这一条规则。
    //
    // 顺序上必须在 `load_all()` 之前 —— 它只动 SQLite 里的日志表，不动记忆
    // 文件；放前面也就不可能影响下面那五类清理读到的内容。
    let logs_purged = index::global().purge_logs_by_age(config::usize_of("auto.logKeepDays").max(1));
    let now = crate::server::logging::now_ms();
    let entries = store::load_all();
    let mut trashed = 0usize;
    let mut purged_rows = 0usize;
    let mut touched: Vec<&str> = Vec::new();

    // 「自动清理失效记忆」默认关：失效记忆可能是演化链的一环，
    // 用户看着历史的时候需要它还在
    if config::bool_of("cleanup.autoDeleteSuperseded") {
        // 每块单独记数：用全局 `trashed` 判「这一类有没有清到东西」，
        // 上一块清过就会让下一块误报（开关开着但一条没匹配，却报「已清理」）。
        let mut hit = 0usize;
        for entry in entries.iter() {
            if entry.deleted_at.is_some() {
                continue;
            }
            let stale = entry.superseded_by.is_some() || entry.is_expired(now);
            if !stale || entry.pinned || entry.kind == super::MemoryKind::Profile {
                continue;
            }
            if retire_entry(entry, now) {
                trashed += 1;
                hit += 1;
            }
            if hit >= 200 {
                break;
            }
        }
        if hit > 0 {
            touched.push("失效");
        }
    }

    // 「不存在记忆」：索引行在、文件没了。这里只能清索引 —— 文件不在就没有
    // 内容可进回收站，把索引行留着只会让界面显示一条点不开的幽灵。
    if config::bool_of("cleanup.autoDeleteMissing") {
        let root = store::root();
        for row in index::global().rows(true) {
            if row.deleted_at.is_some() {
                continue;
            }
            // 容器（`l1/<agent>/YYYY-MM-DD.md`）是「一天一文件」，磁盘上确实存在
            // 那个容器，只是不叫 `<id>.md` —— 直接按 id 拼路径查会把它们全判成
            // 「文件不存在」，一次清理就清掉整库的 daily 记忆。
            if store::is_daily_container(&row.path) {
                continue;
            }
            if root.join(&row.path).exists() {
                continue;
            }
            index::global().drop_one(&row.id);
            purged_rows += 1;
            if purged_rows >= 500 {
                break;
            }
        }
        if purged_rows > 0 {
            touched.push("不存在");
        }
    }

    // 「报错记忆」：文件在、读出来是空壳。真正的读取失败（权限/损坏）在
    // `read_dir_entries` 里只记日志、根本不产出条目，所以那条分支看到的是
    // **零条** —— 判据改成「文件存在但 frontmatter 解析后既无标题也无正文」，
    // 那才是能捡出来的一类。
    if config::bool_of("cleanup.autoDeleteError") {
        let mut hit = 0usize;
        for entry in entries.iter() {
            if entry.deleted_at.is_some() || entry.pinned || entry.kind == super::MemoryKind::Profile {
                continue;
            }
            if !entry.body.trim().is_empty() {
                continue;
            }
            // 「空正文」本身不足以判「报错」：还没生成的占位条目也长这样。
            // 加一道「文件存在」的确认，缺文件的那类归 `autoDeleteMissing`。
            let abs = store::root().join(&entry.path);
            if !abs.exists() {
                continue;
            }
            if retire_entry(entry, now) {
                trashed += 1;
                hit += 1;
            }
            if hit >= 200 {
                break;
            }
        }
        if hit > 0 {
            touched.push("报错");
        }
    }

    // 「冲突记忆」：同步冲突里还 pending 的那些。只处理**能定位到条目**的，
    // 冲突记录本身的清理由同步页裁决（`resolve_conflict`），这里不越权。
    if config::bool_of("cleanup.autoDeleteConflict") {
        let mut conflicted: Vec<String> = Vec::new();
        for row in index::global().conflict_list(200) {
            let pending = row.get("status").and_then(Value::as_str) == Some("pending");
            let path = row.get("path").and_then(Value::as_str).unwrap_or_default();
            // 每日容器保守跳过：一天一文件里可能有十几条正常记忆，为一条冲突
            // 把整份搬进回收站会连坐（这条与目标界面的说明一致）。
            if !pending || path.is_empty() || store::is_daily_container(path) {
                continue;
            }
            conflicted.push(path.to_string());
        }
        let mut hit = 0usize;
        for entry in entries.iter() {
            if entry.deleted_at.is_some() || entry.pinned || entry.kind == super::MemoryKind::Profile {
                continue;
            }
            if !conflicted.contains(&entry.path) {
                continue;
            }
            if retire_entry(entry, now) {
                trashed += 1;
                hit += 1;
            }
            if hit >= 200 {
                break;
            }
        }
        if hit > 0 {
            touched.push("冲突");
        }
    }

    // 「无效记忆」：正文是空壳、连标题都退成了兜底值的残骸。与「报错」的区别
    // 在于这条**进不了索引**（hash 为空或正文全是分隔符），是迁移/删除留下的壳。
    if config::bool_of("cleanup.autoDeleteInvalid") {
        let mut hit = 0usize;
        for entry in entries.iter() {
            if entry.deleted_at.is_some() || entry.pinned || entry.kind == super::MemoryKind::Profile {
                continue;
            }
            let shell = entry.body.trim().is_empty() && entry.title.trim() == "未命名记忆";
            if !shell {
                continue;
            }
            if retire_entry(entry, now) {
                trashed += 1;
                hit += 1;
            }
            if hit >= 200 {
                break;
            }
        }
        if hit > 0 {
            touched.push("无效");
        }
    }

    if removed == 0 && trashed == 0 && purged_rows == 0 && archived == 0 && logs_purged == 0 && !manual {
        return Outcome::Skipped("没有需要清理的内容".into());
    }
    Outcome::Done(json!({
        "trashPurged": removed,
        // 同步删除留档清掉的批次数。单独报而不并进 `trashPurged`：那一栏在界面上
        // 说的是「回收站」，而留档根本不在回收站里，混在一起用户会对不上数。
        "syncTrashPurged": archived,
        // 按 `auto.logKeepDays` 删掉的日志行数（llm_call + task_run 合计）
        "logsPurged": logs_purged,
        "supersededTrashed": trashed,
        "indexPurged": purged_rows,
        "kinds": touched,
        "keepDays": days,
    }))
}

/// 把一条活记忆搬进回收站（连同 `origin_path`）。
///
/// 抽成函数是因为五类清理里四条都要做同一件事，而它必须精确按顺序做：
/// **先按活形态算原路径**，再改 `deleted_at`，最后才 `retire`。顺序错一步
/// 就会得到「恢复后立刻又进回收站」或「同 id 两份」。
fn retire_entry(entry: &super::MemoryEntry, now: i64) -> bool {
    let mut trashed_entry = entry.clone();
    if trashed_entry.origin_path.is_empty() {
        trashed_entry.origin_path = store::live_rel(entry);
    }
    trashed_entry.deleted_at = Some(now);
    trashed_entry.updated_at = now;
    trashed_entry.path = store::rel(&store::entry_path(&trashed_entry, true));
    if store::retire(entry, &trashed_entry).is_err() {
        return false;
    }
    let _ = index::global().upsert(&trashed_entry);
    true
}

/// 抽取：把最近的 l1 流水压成结论候选（`extract` 与 `distill` 的分工）。
///
/// `extract` 只看**最近一批**（默认 20 条），产出比 distill 更轻：它不要求
/// 跨项目规律，只回答「这批流水里有什么值得留下来」。
///
/// ── 产物为什么落 l1，而不是 l2 ───────────────────────────────
/// 早先这里写死 `layer: Some("l2")`，等于「模型抽一次就直接算沉淀」。真机
/// 取证：三天产出 132 条 l2，`agent` 全是 `auto-extract`，真蒸馏产物 0 条；
/// 同一件事被抽出 3-6 份不同措辞（「版本号」3 条、「交付流程」6 条），l2
/// 数量反超 l1。抽取是**压缩流水**，不是**确认结论** —— 它该产出候选，
/// 候选要被后续流水反复确认，才配升 l2（`PROMOTE_AFTER_REINFORCE`）。
///
/// ── 为什么要喂已有结论 ───────────────────────────────────────
/// 以前 prompt 里只有流水素材，模型每轮从零生成「新」结论，同一件事换个说法
/// 再写一条。现在把已有 l2 与抽取产物的标题 + id 一起给它，并区分
/// `new` / `reinforce`：能复述成清单里某条的就是 reinforce，追加证据、
/// 计数 +1，不再新建。这一条比任何排序算法都重要 —— 重复在生成端就被拦住。
///
/// ── 为什么还要「攒够 N 条才跑」───────────────────────────────
/// 时间间隔只管「别太勤」，管不了「素材够不够」。2 小时一轮、每轮 3 条，
/// 每天仍有 36 条产出，而一天的流水只有几十条 —— 每轮都拿同一批素材重抽一遍，
/// 结论自然换着措辞反复出现。`deep.extractThresholdCount` 加的是内容闸门：
/// 距上次抽取之后新增的流水不足 N 条，本轮直接跳过。手动「立即执行」不受限，
/// 用户点了按钮就该有反应。
fn extract(project: Option<&str>, manual: bool) -> Outcome {
    if !config::bool_of("deep.enabled") {
        return Outcome::Skipped("深层记忆已关闭".into());
    }
    let batch = task_batch("extract");
    // 素材只收「真流水」：自动抽取自己的产物不能当下一轮的输入。剔掉的话
    // 第一轮写下的 l1 会在第二轮变成素材，被再抽一遍，越抽越多且每条都
    // 「有据可依」—— 自我循环比重复更难看懂。
    let mut recent: Vec<super::model::MemoryEntry> = store::load_all()
        .into_iter()
        // 被反驳的结论不是「流水素材」：它若混进来，下一轮会被重新抽成结论。
        .filter(|entry| entry.is_trusted() && entry.layer == super::Layer::L1)
        .filter(|entry| entry.agent.as_deref() != Some(super::model::AUTO_EXTRACT_AGENT))
        // 项目口径统一走 `super::in_scope`（见那里的说明）：`entry.project`
        // 是 slug，调用方传的是原始名字，直接比字符串会让过滤静默失效。
        .filter(|entry| super::in_scope(entry.project.as_deref(), project))
        .collect();
    recent.sort_by_key(|entry| std::cmp::Reverse(entry.updated_at));
    recent.truncate(batch);
    if recent.len() < 3 {
        return Outcome::Skipped("最近流水太少，不值得抽取".into());
    }
    // ── 内容闸门：上次抽取之后攒下来的流水够不够 ──────────────
    //
    // 时间到点不等于「有东西可抽」。上一轮刚把最近 20 条压成 3 条候选，隔两
    // 小时再看，那 20 条还是最新的 20 条 —— 模型会在同一批素材上再抽一遍，
    // 输出必然是同义改写。所以按 `updated_at > 上次抽取时刻` 数一遍新增流水，
    // 不足 `deep.extractThresholdCount` 就跳过。首轮（上次 = 0）不设闸门。
    //
    // 手动执行同样放行：用户点「立即执行」是明确要看结果，用阈值挡回去只会
    // 让它看起来像坏了。
    let since = last_of("extract");
    if !manual && since > 0 {
        let floor = config::number("deep.extractThresholdCount").max(3.0) as usize;
        let fresh = recent.iter().filter(|entry| entry.updated_at > since).count();
        if fresh < floor {
            return Outcome::Skipped(format!(
                "距上次抽取后新增流水 {fresh} 条，不足 {floor} 条，本轮跳过"
            ));
        }
    }
    // 已有结论清单：l2（真沉淀）+ 抽取产物（还没升上去的候选）。两类都要喂，
    // 否则「候选 vs 候选」的重复识别不出来。
    //
    // 上限 60 条是 token 预算的取舍：清单只是让模型「认出这是老话题」，不是
    // 让它逐条比对。按 updated_at 倒序取最近的，冷门老结论认不出来时会走
    // new，由下一轮的 near_duplicate 兜底。
    let mut known: Vec<super::model::MemoryEntry> = store::load_all()
        .into_iter()
        // 被否掉的结论不进「已有结论清单」，否则模型会去 reinforce 让它复活。
        .filter(|entry| entry.is_trusted())
        .filter(|entry| {
            entry.layer == super::Layer::L2
                || entry.agent.as_deref() == Some(super::model::AUTO_EXTRACT_AGENT)
        })
        .filter(|entry| super::in_scope(entry.project.as_deref(), project))
        .collect();
    known.sort_by_key(|entry| std::cmp::Reverse(entry.updated_at));
    known.truncate(60);
    let known_ids: std::collections::HashSet<String> =
        known.iter().map(|entry| entry.id.clone()).collect();
    let catalog = known
        .iter()
        .map(|entry| {
            format!(
                "[{}] ({}) {}",
                entry.id,
                entry.kind.as_str(),
                super::model::truncate_chars(&entry.title, 40)
            )
        })
        .collect::<Vec<_>>()
        .join("\n");
    let catalog = if catalog.is_empty() {
        "（还没有任何结论）".to_string()
    } else {
        catalog
    };
    // ── 明确负向证据 ─────────────────────────────────────────
    // 只把「对的」喂给模型不够：同一条错误结论会在下一轮被重新抽出来，因为
    // 模型看不到「这个说法上次被否过」。真机上 l2 泛滥的一半成因就是这个 ——
    // 同一件事反复换个说法冒出来。所以把被反驳的标题 + 理由一起喂进去，
    // 让它在生成阶段就避让。
    let mut refuted: Vec<super::model::MemoryEntry> = store::load_all()
        .into_iter()
        .filter(|entry| entry.is_live() && entry.is_refuted())
        .filter(|entry| super::in_scope(entry.project.as_deref(), project))
        .collect();
    refuted.sort_by_key(|entry| std::cmp::Reverse(entry.refuted_at.unwrap_or(0)));
    refuted.truncate(20);
    let rejected = refuted
        .iter()
        .map(|entry| {
            let reason = entry.refuted_reason.trim();
            let hint = if reason.is_empty() {
                "（未写理由）".to_string()
            } else {
                super::model::truncate_chars(reason, 60)
            };
            format!("- {} ← 理由：{}", super::model::truncate_chars(&entry.title, 40), hint)
        })
        .collect::<Vec<_>>()
        .join("\n");
    let rejected = if rejected.is_empty() { "（无）".to_string() } else { rejected };
    let material = recent
        .iter()
        .map(|entry| {
            format!(
                "[{}] {} · {}\n{}",
                entry.id,
                entry.title,
                super::search::format_date(entry.updated_at),
                super::model::truncate_chars(&entry.body, 500)
            )
        })
        .collect::<Vec<_>>()
        .join("\n\n---\n\n");
    let ask = format!(
        "下面是最近的若干条工作流水。请挑出**仍然值得长期记住**的内容，最多 3 条。\n\
         只挑有明确结论或明确教训的；进度汇报、寒暄、临时状态全部丢掉。\n\
         一条都没有就输出 {{\"items\":[]}}。\n\n\
         ── 先比对已有结论，再决定动作 ─────────────────────────\n\
         已经有这些结论（id 只能从这里选，不要自己编）：\n{catalog}\n\n\
         ── 明确负向证据 ─────────────────────────────────────\n\
         被判定为错误的旧说法（不要再用同样的措辞或同样的结论重新生成；如果新素材确实推翻了这些判断，明确说明为什么）\n\
         {rejected}\n\n\
         reinforce = 这批流水讲的事，上面清单里已经有了。target 填那条的 id，\n\
                     note 写这次补充或确认了什么。\n\
                     **哪怕措辞完全不同，只要是同一件事，就是 reinforce。**\n\
         new       = 清单里确实没有，才新建。\n\
                     同一件事被记成两条是最严重的错误 —— 宁可 reinforce，不要 new。\n\n\
         ── type 怎么选（只在 new 时填）─────────────────────────\n\
         decision  = 做了选择且说得出理由\n\
         knowledge = 外部事实、接口形态、平台规矩\n\
         pitfall   = 踩过的坑、下次要绕开的东西\n\
         preference= 用户明确提过的长期要求、禁止事项、做法偏好\n\
         用户说「不要这样」「以后都这样」「保持 X 不变」时一定是 preference，\n\
         而且**要写成命令句**（「版本号保持 3.5.0-beta.1，不要自行 bump」），\n\
         不要写成汇报句（「用户不想升版本号」）—— 这条会被原样注入到之后的\n\
         每一次对话里，汇报句读起来像历史，命令句读起来像要求。\n\n\
         输出 JSON：{{\"items\":[\
         {{\"action\":\"reinforce\",\"target\":\"已有 id\",\"note\":\"补充了什么\"}},\
         {{\"action\":\"new\",\"title\":\"不超过 40 字\",\"type\":\"decision|knowledge|pitfall|preference\",\"body\":\"100-300 字\",\"importance\":0.0-1.0}}\
         ]}}\n\n\
         流水：\n{material}"
    );
    let reply = match super::llm::complete(super::llm::Request::new("extract", SYSTEM_EXTRACT, ask)) {
        Ok(reply) => reply,
        Err(error) => {
            return if error.is_unavailable() {
                Outcome::Skipped(error.message().to_string())
            } else {
                Outcome::Failed(error.message().to_string())
            }
        }
    };
    let Some(parsed) = super::llm::parse_json(&reply.text) else {
        return Outcome::Failed("抽取结果不是合法 JSON，已丢弃".into());
    };
    let items = parsed.get("items").and_then(Value::as_array).cloned().unwrap_or_default();
    let mut created = 0usize;
    let mut reinforced = 0usize;
    let mut promoted = 0usize;
    // 一次强化的证据锚点：这批流水里最新的那条。它代表「这次是哪批流水确认
    // 的」，比塞时间戳干净，也比「把 20 条素材全挂上」克制。同一批流水重复
    // 跑时这个 id 已经在 evidence 里，不会重复计数。
    let cue = recent
        .first()
        .map(|entry| entry.id.clone())
        .unwrap_or_default();
    for item in items.iter().take(3) {
        if item.get("action").and_then(Value::as_str) == Some("reinforce") {
            let target = item.get("target").and_then(Value::as_str).unwrap_or("").trim();
            // 模型编了 id / 指向清单外：直接丢，不降级成 new。降级会把它变成
            // 一条新的重复结论，正是这一轮要消灭的东西。
            if target.is_empty() || !known_ids.contains(target) {
                continue;
            }
            // 只数「真的写进去了」的：NOOP（锚点已有、正文没变大）不该计入。
            if let Ok(out) = super::reinforce_entry(target, &cue) {
                if out.changed {
                    reinforced += 1;
                }
                if out.promoted {
                    promoted += 1;
                }
            }
            continue;
        }
        let body = item.get("body").and_then(Value::as_str).unwrap_or("").trim();
        if body.chars().count() < 20 {
            continue;
        }
        let kind = item.get("type").and_then(Value::as_str).unwrap_or("knowledge");
        let title = item.get("title").and_then(Value::as_str).unwrap_or("").to_string();
        let importance = item.get("importance").and_then(Value::as_f64).unwrap_or(0.6);
        // ── 候选之间也要判重 ───────────────────────────────────
        //
        // 模型这次说的是 `new`，但它只看得到「清单里的标题」，看不到清单外那些
        // 措辞相近的候选。同一件事被抽成两份候选，两份各自攒证据、各自升 l2，
        // 重复只是被推迟到升级那一刻。所以落盘前再比一次正文：已经有近似候选
        // 时，这次算「又确认了一次」，不算新建。
        //
        // 判据只看 auto-extract 的 l1 候选（见 `similar_l1_candidate`），
        // 不碰人写的流水 —— 那条界线不能越。
        let candidate_kind = super::MemoryKind::parse(kind);
        if let Some(existing) = super::similar_l1_candidate(project, candidate_kind, body) {
            // 正文一起比：同一件事的两份候选，留信息量更大的那份。只加锚点会让
            // 先落盘的半截版本一直代表这件事，直到它凑够次数升 l2 为止。
            if let Ok(out) = super::reinforce_entry_with(&existing, &cue, Some(body), Some(title.as_str())) {
                if out.changed {
                    reinforced += 1;
                }
                if out.promoted {
                    promoted += 1;
                }
            }
            continue;
        }
        // 继承素材条目的 `files`：抽取出来的结论建立在同一批文件之上，继承之后
        // 请求级注入的路径命中（权重最高的一档）才能打到这条 l2 上。早先这里
        // 写死空数组，等于让所有自动产出的结论对「按文件命中」永久隐形。
        //
        // 素材是 l1 会话流水，`files` 通常为空，路径全在正文里 —— 所以还要从
        // 正文抽一遍（`inject::harvest_paths`，确定性字符串抽取）。
        let mut files: Vec<String> = Vec::new();
        for entry in &recent {
            for file in &entry.files {
                if !files.iter().any(|known| known == file) {
                    files.push(file.clone());
                }
            }
            for file in super::inject::harvest_paths(&entry.body) {
                if !files.iter().any(|known| known == &file) {
                    files.push(file);
                }
            }
            if files.len() >= 8 {
                break;
            }
        }
        files.truncate(8);
        let input = super::WriteInput {
            content: body.to_string(),
            title: Some(title),
            kind: Some(kind.to_string()),
            // 落 l1：抽取是候选，不是沉淀。升 l2 由 reinforce 触发。
            layer: Some("l1".to_string()),
            project: project.map(str::to_string),
            agent: Some(super::model::AUTO_EXTRACT_AGENT.to_string()),
            tags: Vec::new(),
            importance: Some(importance),
            files,
            supersedes: Vec::new(),
            pinned: Some(false),
            starred: Some(false),
            cwd: None,
            session: None,
            created_at: None,
            valid_to: None,
            // 只挂「这次是哪批流水产出的」一条：`evidence` 现在兼职强化计数，
            // 一上来塞 5 条会让新建的候选在第一次 reinforce 时就跨过升级门槛，
            // 等于「新建 + 确认一次 = 沉淀」，跟以前一样松。
            evidence: if cue.is_empty() { Vec::new() } else { vec![cue.clone()] },
        };
        // 走 super::write 而不是直接落盘：抽取产物同样要经过去重、hash、索引
        if super::write(input).is_ok() {
            created += 1;
        }
    }
    if created == 0 && reinforced == 0 {
        return Outcome::Skipped("这批流水里没有值得留下来的结论".into());
    }
    Outcome::Done(json!({
        "created": created,
        "reinforced": reinforced,
        "promoted": promoted,
        "material": recent.len(),
        "model": reply.model
    }))
}

const SYSTEM_EXTRACT: &str = "You curate a long-term project memory. \
Keep only conclusions and lessons that will still matter in a month. \
Drop progress reports, greetings and transient state. \
Answer in the same language as the input. Output strict JSON, no prose around it.";

/// 失效判定：对最近写入的 l2 条目逐条跑 `detect_supersede`。
fn supersede_recent(project: Option<&str>) -> Outcome {
    let mut recent: Vec<super::model::MemoryEntry> = store::load_all()
        .into_iter()
        // 已被反驳的 l2 不参与失效判定：错误结论没有资格去取代别人。
        .filter(|entry| entry.is_trusted() && entry.layer == super::Layer::L2)
        .filter(|entry| entry.kind != super::MemoryKind::Profile)
        .filter(|entry| super::in_scope(entry.project.as_deref(), project))
        .collect();
    recent.sort_by_key(|entry| std::cmp::Reverse(entry.updated_at));
    // 只查最近 5 条：老条目的失效关系早就被查过了（每次写入也会查一次）
    recent.truncate(5);
    if recent.is_empty() {
        return Outcome::Skipped("没有可判定的 l2 条目".into());
    }
    let mut suggestions = 0usize;
    let mut skipped = 0usize;
    for entry in &recent {
        match enrich::detect_supersede(&entry.id) {
            Outcome::Done(payload) => {
                suggestions += payload.get("suggestions").and_then(Value::as_u64).unwrap_or(0) as usize
            }
            Outcome::Skipped(_) => skipped += 1,
            Outcome::Failed(error) => {
                crate::server::logging::log("[Memory]", &format!("⚠️ 失效判定失败：{error}"))
            }
        }
    }
    if suggestions == 0 {
        return Outcome::Skipped(format!("{skipped} 条都没有被取代"));
    }
    Outcome::Done(json!({ "suggestions": suggestions, "checked": recent.len() }))
}

/// 合并（consolidate）：把一个项目里同类的 l2 结论合并成一条更完整的。
///
/// 与 distill 的区别：distill 是「l1 → l2」，consolidate 是「l2 → 更短的 l2」。
/// 它产出的是**建议**，因为合并会删掉原条目（进回收站），这是不可逆的语义变化。
fn consolidate(project: Option<&str>, manual: bool) -> Outcome {
    let Some(slug) = project else {
        return Outcome::Skipped("合并需要指定项目（一次只合并一个项目，避免跨项目串味）".into());
    };
    let mut items: Vec<super::model::MemoryEntry> = store::load_all()
        .into_iter()
        // 被反驳的结论不进合并池：合并会删掉原条目，等于把错误洗成一条新结论。
        .filter(|entry| entry.is_trusted() && entry.layer == super::Layer::L2)
        .filter(|entry| super::project_matches(entry.project.as_deref(), Some(slug)))
        .filter(|entry| entry.kind != super::MemoryKind::Profile)
        .collect();
    if items.len() < 4 {
        return Outcome::Skipped(format!("{slug} 的 l2 只有 {} 条，不够合并", items.len()));
    }
    items.sort_by_key(|entry| std::cmp::Reverse(entry.updated_at));
    let batch: Vec<_> = items.into_iter().take(if manual { 24 } else { 12 }).collect();
    let ask = format!(
        "以下是项目 {slug} 的若干条长期结论。找出**主题重复**的组，把它们各合并成一条更完整的结论。\n\
         只合并真正讲同一件事的；拿不准就不要合并。\n\
         合并后的 body 要覆盖原来所有重要信息，不许丢细节。\n\n\
         输出 JSON：{{\"groups\":[{{\"title\":\"合并后标题\",\"type\":\"decision|knowledge|insight|pitfall\",\"body\":\"合并后正文\",\"sources\":[\"被合并的条目 id\"]}}]}}\n\
         没有可合并的组就输出 {{\"groups\":[]}}。\n\n\
         结论：\n{}",
        batch
            .iter()
            .map(|entry| format!(
                "[{}] {}（{}）\n{}",
                entry.id,
                entry.title,
                entry.kind.label(),
                super::model::truncate_chars(&entry.body, 500)
            ))
            .collect::<Vec<_>>()
            .join("\n\n")
    );
    let reply = match super::llm::complete(super::llm::Request::new("consolidate", SYSTEM_EXTRACT, ask)) {
        Ok(reply) => reply,
        Err(error) => {
            return if error.is_unavailable() {
                Outcome::Skipped(error.message().to_string())
            } else {
                Outcome::Failed(error.message().to_string())
            }
        }
    };
    let Some(parsed) = super::llm::parse_json(&reply.text) else {
        return Outcome::Failed("合并结果不是合法 JSON，已丢弃".into());
    };
    let groups = parsed.get("groups").and_then(Value::as_array).cloned().unwrap_or_default();
    let mut queued = 0usize;
    for group in groups.iter().take(5) {
        let body = group.get("body").and_then(Value::as_str).unwrap_or("").trim();
        let sources: Vec<String> = group
            .get("sources")
            .and_then(Value::as_array)
            .map(|items| items.iter().filter_map(|id| id.as_str().map(str::to_string)).collect())
            .unwrap_or_default();
        // 少于两条来源的「合并」没有意义，而且多半是模型把自己抄了一遍
        if body.chars().count() < 40 || sources.len() < 2 {
            continue;
        }
        let target = format!("{slug}:{}", &super::model::content_hash(body)[..12]);
        let payload = json!({
            "title": group.get("title").and_then(Value::as_str).unwrap_or(""),
            "type": group.get("type").and_then(Value::as_str).unwrap_or("knowledge"),
            "body": body,
            "sources": sources,
            "project": slug,
            "model": reply.model,
        });
        let queued_id = index::global().enqueue(
            "consolidate",
            &target,
            &payload,
            group.get("confidence").and_then(Value::as_f64).unwrap_or(0.7),
        );
        // 与蒸馏同一条理由（见 `enrich::distill`）：合并只**新增**一条 l2，
        // 被合并的原条目进回收站、可恢复，所以默认直接落地。
        //
        // 但合并比蒸馏多一道闸：来源少于两条的组已经在上面被跳过了，这里再要求
        // `review.autoConfirmDistill` 打开 —— 合并会动到已有 l2（把原件移进
        // 回收站），比「纯新增」重一档，用同一个开关是为了不让用户去理解两个
        // 名字不同的开关其实是一件事。
        if config::bool_of("review.autoConfirmDistill") {
            match super::apply_derived(&payload) {
                Ok(()) => {
                    if let Some(id) = queued_id {
                        index::global().review_resolve(&[id], "approved", Some("auto-consolidate"));
                    }
                }
                Err(error) => {
                    crate::server::logging::verbose(
                        "[Memory]",
                        &format!("合并结论自动落地失败（已留在待确认队列）：{error}"),
                    );
                }
            }
        }
        queued += 1;
    }
    if queued == 0 {
        return Outcome::Skipped("模型认为没有可合并的组".into());
    }
    Outcome::Done(json!({ "queued": queued, "checked": batch.len(), "model": reply.model }))
}

/// 调度器状态（自动化页面顶部）。
pub fn status() -> Value {
    let now = crate::server::logging::now_ms();
    // 每个任务的累计统计（跑过几次 / 成了几次 / 烧了多少 token）。
    // 一次查完再分发，不在下面每个任务里各查一遍 —— 11 个任务 =
    // 11 次 `GROUP BY`，而它们读的是同一张表的同一批行。
    let stats = index::global().task_stats();
    let tasks: Vec<Value> = TASKS
        .iter()
        .map(|task| {
            let last = last_of(task);
            let (runs, ok, skipped, tokens) = stats.get(*task).copied().unwrap_or((0, 0, 0, 0));
            // `nextRun` 与 `nextRunText` 必须来自同一个数：一个是机器用的闹钟，
            // 一个是给人看的话。先前这里按「上次 + 间隔」算，文字按配置时刻算，
            // 于是同一个任务同时存在两个「下次」—— 页面写 11:00、闹钟响在 15:20。
            let due = next_due(task, last);
            // 已到期（`due` 落在过去，含「从未跑过」的 0）就报 `now`：
            // 下一轮 tick 就会跑它。报过去的时间戳没有意义，报 1970 更糟。
            let next = due.map_or(now, |at| at.max(now));
            json!({
                "task": task,
                "enabled": task_enabled(task),
                "intervalMin": task_interval(task),
                "lastRun": if last > 0 { json!(last) } else { Value::Null },
                "lastRunText": if last > 0 { json!(super::search::relative_time(last, now)) } else { Value::Null },
                "nextRun": next,
                // 「下次」的人话：按 `daily` / `weeklyTime`（按天/按周）或
                // `intervalMin`（其余）推，与 `nextRun` 同一个来源。
                "nextRunText": next_run_text(task, last, now),
                // 累计统计。`runs` 含跳过（跳过也是跑过的），`ok` 只数成功，
                // `skipped` 单列 —— 成功率的分母是 `runs - skipped`，跳过不是失败。
                "runs": runs,
                "success": ok,
                "skipped": skipped,
                "tokens": tokens,
                // 这个任务要不要花 token。零 token 的标「零成本」，
                // 花 token 的标「调用模型」——用户扫一眼就知道开它会烧钱。
                "usesModel": task_uses_model(task),
                // 一句「它在干什么」，进卡片副标题。文案与 `detail_of`
                // 的分支同名同义，但这里是**静态说明**（还没跑时也要显示），
                // 那边是**运行结果**（跑完才有）。空串 = 这个任务没有可说的，
                // 前端画一个 `—`，不编一句废话。
                "summary": task_summary(task),
                // 节奏（三选一：间隔 / 每天 / 每周）。界面那句「调度节奏：…」
                // 直接用它，不自己从 `intervalMin` 反推 —— 反推会把
                // 「每天 11:00」显示成「每 1 天」。
                "cadence": task_cadence(task),
            })
        })
        .collect();
    let labels: Vec<Value> = super::TASK_LABELS
        .iter()
        .map(|(task, label)| json!({ "task": task, "label": label }))
        .collect();
    // 超预算时的行为：`pause` = 挂起，`ignore` = 照跑。
    // 目标界面把它做成一个下拉放在页面上，所以这里连**实际生效的值**一起给，
    // 前端不必自己抄一份枚举（抄了必然漂移）。空配置按 `pause` 报 —— 那是
    // `llm::budget_exhausted` 的默认行为，页面显示的必须是真正在跑的那条。
    //
    // 先在 `json!` 外面算好：`json!` 的 `{}` 只会被当成嵌套对象，写
    // `"k": { let ..; }` 是语法错误，而不是「先算再放」。
    let over_budget = {
        let action = config::text_of("auto.overBudgetAction");
        if action.trim().is_empty() { "pause".to_string() } else { action.trim().to_string() }
    };
    json!({
        "running": RUNNING.load(Ordering::SeqCst),
        "lastTick": LAST_TICK.load(Ordering::SeqCst),
        "globalEnabled": config::bool_of(config::ENABLE_KEY) && config::bool_of("auto.enabled"),
        "privacyPaused": config::bool_of("privacy.pause"),
        "dailyTokenLimit": config::number("auto.dailyTokenLimit") as i64,
        "tokensToday": index::global().tokens_today(),
        "budgetExhausted": super::llm::budget_exhausted(),
        // 超预算时的行为：pause = 挂起，ignore = 照跑。
        // 可选值见 `config` 的 `auto.overBudgetAction` 枚举（pause / ignore）。
        "overBudgetAction": over_budget,
        // 待裁决条数：目标界面那张「待确认裁决建议」卡的数字来源。
        // 与状态页 `review.pending` 同一个计数（`review_queue` 里 pending 的行），
        // 这里再给一次是因为自动化页不该为了一个数字去拉整个 status。
        "pendingReview": index::global().review_count(),
        // 最近几次执行（「任务执行记录」那张表）。**预览只取 10 条** ——
        // 与目标界面同口径：这张表是「刚才发生了什么」的速览，不是全量列表。
        // 早先取 30 条，结果页面被一张表撑到两三屏，下面「待确认裁决」之类
        // 真正要动手的东西全被挤到看不见。想看更多走「展开」。
        // `runRows` 是可翻的**行数**，`runCount` 是**执行次数**（合并后的
        // 一行代表多次）—— 前者给展开按钮上的数字，后者给页面顶部概览。
        "recentRuns": index::global().recent_task_runs(PREVIEW_RUNS),
        "runRows": index::global().task_run_count(),
        "runCount": index::global().task_run_executions(),
        "labels": labels,
        "tasks": tasks,
    })
}

/// 任务执行审计的「展开」数据。
///
/// `status()` 里已带 `PREVIEW_RUNS`（10 条）预览，这个接口只服务「展开」：
/// 默认给 `EXPANDED_RUNS`（50 条）。`limit` 仍可由调用方覆盖（`?limit=N`），
/// 上限 500 由 `recent_task_runs` 兜底 —— 一次拉太多没有意义，
/// `auto.logKeepCount` 默认只留几百行，翻到尽头也就是那个数。
///
/// 返回两个数：`total` 是**行数**（库里一共多少行），`runs` 是**执行次数**
/// （`repeat` 求和，与任务行「累计 N 次」同一口径）。合并过以后两者不再相等，
/// 页面上的「展开」按钮旁边的数字要用行数，任务行的「累计」要用次数。
pub fn runs(limit: usize) -> Value {
    json!({
        "items": index::global().recent_task_runs(limit.clamp(1, 500)),
        "total": index::global().task_run_count(),
        "runs": index::global().task_run_executions(),
    })
}

/// 任务的一句静态说明（「它在干什么」）。
/// 与 `TASK_LABELS` 分开：那个是**名字**（「抽取结构化信息」，用于标题与下拉），
/// 这里是**解释**（「给新记忆补一句语义摘要与重要度评级」）。卡片式界面里
/// 每张卡标题下面都是这一句，没有它用户只能从名字猜这个任务会动什么数据。
fn task_summary(task: &str) -> &'static str {
    match task {
        "extract" => "给新记忆补一句语义摘要与重要度评级",
        "summarize" => "为长时间会话或日常工作流提炼高密度摘要",
        "tag" => "按内容更新 2-5 个标签，优先与已有概念复用",
        "classify" => "未归属项目的碎片自动匹配相似归档（零成本）",
        "supersede" => "识别「新事实推翻旧事实」，输出失效建议",
        "distill" => "将项目原始碎片深度蒸馏为架构决策与取舍手册",
        "consolidate" => "全库记忆查重：自动合并雷同、存储换入队列",
        "profile" => "跨项目深度归纳人格、沟通、技术栈与工作习惯",
        "ingest" => "把客户端新产生的会话增量搬进记忆（零成本）",
        "index-scan" => "全库文件与索引一致性自检及修复（零成本）",
        "cleanup" => "清理回收站与异常无用记录（零成本）",
        // `sync` 不在这里：WebDAV 同步已从自动化任务里摘出去，它有自己的
        // 页面（`GET /api/memory/sync` 一族）。留一句文案在这里只会让
        // 「为什么有说明却没有卡」变成下一个要解释的问题。
        _ => "",
    }
}

/// 「下次」那一列的文字。
///
/// ── 与 `tick` 用同一个判据 ──────────────────────────────────
/// 显示说「18 分钟后」而调度器其实按「上次 + 间隔」在跑，是这一页最容易被
/// 抓到的谎。所以这里不自己算一套，直接问 [`next_due`] —— 页面上的「下次」
/// 就是下一轮 tick 真正会跑它的时刻。
fn next_run_text(task: &str, last: i64, now: i64) -> String {
    // 还没进过任何轮次：下一次就是启动后的第一个到期 tick。
    // 说「即将首次执行」而不是「现在」—— 后者会被读成「已经该跑了」。
    if last <= 0 {
        return "即将首次执行".to_string();
    }
    match next_due(task, last) {
        Some(at) if at <= now => "已到期".to_string(),
        Some(at) => until_text(at, now),
        // 本地时区拿不到（夏令时空档等）：不编一个时刻
        None => "—".to_string(),
    }
}

/// 下一次**该跑**它的时刻（epoch ms）。`tick` 与「下次」列共用这一份。
///
/// ── 三种节奏 ────────────────────────────────────────────────
/// · 配了 `daily: "HH:MM"`：每天的那一分钟。
/// · 配了 `weekly: N` + `weeklyTime: "HH:MM"`：每周第 N 天（1 = 周一）的那个点。
/// · 其余：`上次 + intervalMin`。这是兜底，也是「每 N 分钟」类任务的唯一规则。
///
/// ── 为什么不能与「上次 + 间隔」取 max ────────────────────────
/// 按天任务的间隔就是 24h，取 max 等于「明天同一时刻」：上次 09:59 跑过、
/// 配置写 11:00，下一次会变成明天 09:59，于是 11:00 这个配置永远落不到实处
/// （每天往后滑）。所以配了时刻就以时刻为准 —— `first_*_after` 已保证严格
/// 晚于上次，天然不会同一天跑两遍。只有**没配时刻**的任务才看间隔。
fn next_due(task: &str, last: i64) -> Option<i64> {
    // 从未跑过：`0` 让「到期」立刻成立，下一轮 tick 就跑（与 `tick` 里
    // `now - 0 >= interval` 同义）。
    if last <= 0 {
        return Some(0);
    }
    let entry = config::object_of("auto.tasks");
    let item = entry.get(task);
    let clock = item.and_then(|value| value.get("daily")).and_then(Value::as_str);
    let weekly_time = item.and_then(|value| value.get("weeklyTime")).and_then(Value::as_str);
    let weekly_day = item.and_then(|value| value.get("weekly")).and_then(Value::as_i64);

    // 按周优先：`weekly` 与 `weeklyTime` 都配了才算按周，否则退到 `daily`。
    if let (Some(day), Some((hour, minute))) = (weekly_day, weekly_time.and_then(parse_clock)) {
        if let Some(at) = first_weekly_after(last, day.clamp(1, 7), hour, minute) {
            return Some(at);
        }
    }
    if let Some((hour, minute)) = clock.and_then(parse_clock) {
        if let Some(at) = first_daily_after(last, hour, minute) {
            return Some(at);
        }
    }
    // 没配时刻（或时刻格式非法 / 时区取不到）：退回「上次 + 间隔」。
    Some(last + task_interval(task) * 60_000)
}

/// 未来某刻 → 「18 分钟后 / 4 小时后 / 6 天后」。
///
/// ── 为什么不能复用 `search::relative_time` ─────────────────
/// 那个函数是给**过去**的时间用的（「3 小时前」），遇到未来的时间戳它返回
/// 「刚刚」—— 于是「下次执行」这一列会整列显示「刚刚」，比空着更误导。
/// 这里方向相反，阈值也相反（`nextRun` 最远到一周后，所以要有「天」这一档）。
fn until_text(at: i64, now: i64) -> String {
    let delta = at - now;
    if delta <= 0 {
        return "已到期".to_string();
    }
    let minutes = delta / 60_000;
    if minutes < 1 {
        return "随即执行".to_string();
    }
    if minutes < 60 {
        return format!("{minutes} 分钟后");
    }
    let hours = minutes / 60;
    if hours < 24 {
        return format!("{hours} 小时后");
    }
    format!("{} 天后", hours / 24)
}

fn parse_clock(text: &str) -> Option<(u32, u32)> {
    let (hour, minute) = text.trim().split_once(':')?;
    let hour: u32 = hour.trim().parse().ok()?;
    let minute: u32 = minute.trim().parse().ok()?;
    if hour > 23 || minute > 59 {
        return None;
    }
    Some((hour, minute))
}

/// `base` 之后（严格大于）的第一个「每天 HH:MM」时刻，epoch 毫秒。
///
/// ── 为什么从 `base` 推而不是从 `now` 推 ───────────────────
/// 见 [`next_due`]：从 `last` 推出来的时刻可能已经过去，`tick` 立刻补跑；
/// 从 `now` 推则会静默跳过「该跑但没跑」的那一次。
///
/// 用 `chrono::Local` 与 `cloud_sync` 的时基同一套：用户配的是「本地时间
/// 11:00」，用 UTC 推会差出时区。
fn first_daily_after(base: i64, hour: u32, minute: u32) -> Option<i64> {
    use chrono::{Local, NaiveTime, TimeZone};
    let moment = Local.timestamp_millis_opt(base).single()?;
    let today = moment.date_naive();
    let at = NaiveTime::from_hms_opt(hour, minute, 0)?;
    // `and_local_timezone` 在夏令时切换的空档会回 None，那种情况下退回相对时间
    let today_at = today.and_time(at).and_local_timezone(Local).single()?;
    if today_at.timestamp_millis() > base {
        return Some(today_at.timestamp_millis());
    }
    let tomorrow = today.succ_opt()?.and_time(at);
    tomorrow.and_local_timezone(Local).single().map(|value| value.timestamp_millis())
}

/// `base` 之后（严格大于）的第一个「每周第 day 天 HH:MM」时刻。
///
/// `day` 按 1 = 周一 … 7 = 周日（与 `auto.tasks.*.weekly` 的配置口径一致，
/// 也是 ISO 的星期编号）。最远推到 7 天后，绝不会死循环。
fn first_weekly_after(base: i64, day: i64, hour: u32, minute: u32) -> Option<i64> {
    use chrono::{Datelike, Local, NaiveTime, TimeZone, Weekday};
    let moment = Local.timestamp_millis_opt(base).single()?;
    let at = NaiveTime::from_hms_opt(hour, minute, 0)?;
    let target = match day {
        1 => Weekday::Mon,
        2 => Weekday::Tue,
        3 => Weekday::Wed,
        4 => Weekday::Thu,
        5 => Weekday::Fri,
        6 => Weekday::Sat,
        _ => Weekday::Sun,
    };
    let mut date = moment.date_naive();
    for _ in 0..8 {
        if date.weekday() == target {
            if let Some(stamp) = date.and_time(at).and_local_timezone(Local).single() {
                if stamp.timestamp_millis() > base {
                    return Some(stamp.timestamp_millis());
                }
            }
        }
        date = date.succ_opt()?;
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_task_has_an_interval() {
        for task in TASKS {
            assert!(default_interval(task) > 0, "{task} 没有默认间隔");
        }
    }

    #[test]
    fn zero_token_tasks_default_on() {
        assert!(task_enabled("index-scan"));
        assert!(task_enabled("cleanup"));
        // 会话增量入库不调模型，而它的价值全在「不用人管」：默认关掉等于没发布
        assert!(task_enabled("ingest"));
    }

    #[test]
    fn last_run_stamp_round_trips() {
        // 别用 `health` 当样例任务：它 2026-11-24 被删掉了，测试会跟着一起失效。
        // 用仍在册的 `cleanup` —— 这条测的是 stamp/last_of 本身，与具体任务无关。
        stamp("cleanup", 123);
        assert_eq!(last_of("cleanup"), 123);
        // 不在册的任务名必须被静默丢弃（否则会往 last_run 里塞野键）
        stamp("not-a-task", 456);
        assert_eq!(last_of("not-a-task"), 0);
    }

    #[test]
    fn clock_parsing_rejects_impossible_times() {
        assert_eq!(parse_clock("11:00"), Some((11, 0)));
        assert_eq!(parse_clock(" 23:59 "), Some((23, 59)));
        assert_eq!(parse_clock("24:00"), None);
        assert_eq!(parse_clock("11:60"), None);
        assert_eq!(parse_clock("11"), None);
        assert_eq!(parse_clock(""), None);
    }

    #[test]
    fn until_text_counts_forward_not_backward() {
        // 回归：早先用 `relative_time` 算「下次」，未来时间会被它判成
        // 「刚刚」—— 整列都显示「刚刚」，比空着更误导用户。
        let now = 1_000_000_000_000_i64;
        assert_eq!(until_text(now + 30_000, now), "随即执行");
        assert_eq!(until_text(now + 18 * 60_000, now), "18 分钟后");
        assert_eq!(until_text(now + 4 * 3_600_000, now), "4 小时后");
        assert_eq!(until_text(now + 6 * 86_400_000, now), "6 天后");
        assert_eq!(until_text(now - 1, now), "已到期");
    }

    #[test]
    fn first_daily_after_rolls_to_tomorrow_once_the_clock_passes() {
        use chrono::Local;
        // 取「今天 00:00 本地时刻」，那样「今天 23:59」一定还没到、
        // 「今天 00:00」一定已过 —— 不依赖跑测试的钟点。
        let today = Local::now().date_naive();
        let midnight = today
            .and_hms_opt(0, 0, 0)
            .and_then(|value| value.and_local_timezone(Local).single())
            .expect("本地时区可用")
            .timestamp_millis();
        let later = first_daily_after(midnight, 23, 59).expect("应能算出今天的 23:59");
        assert!(later - midnight < 86_400_000, "今天 23:59 不该跨到后天");
        // 基准是 midnight 本身时，「今天 00:00」不算「之后」（严格大于），
        // 应滚到明天 00:00。
        let rolled = first_daily_after(midnight, 0, 0).expect("应能算出明天的 00:00");
        assert!(rolled > midnight && rolled - midnight > 86_300_000);
    }

    #[test]
    fn next_due_follows_the_configured_clock_not_a_sliding_interval() {
        // 回归：早先取「时刻表」与「上次 + 间隔」的 max，按天任务的间隔就是
        // 24h，于是 11:00 每天往后滑一分钟（上次 09:59 → 今天 11:00 跑完 →
        // 下次 11:00 与 明天 09:59+ 相撞）。配了时刻就必须按时刻。
        let last = crate::server::logging::now_ms();
        if let Some(at) = next_due("extract", last) {
            // `extract` 没配 `daily`，走间隔兜底。
            assert_eq!(at, last + task_interval("extract") * 60_000);
        }
        // 配了 daily 的任务：下次严格晚于上次，且不超过 24h。
        if let Some(at) = next_due("cleanup", last) {
            assert!(at > last, "cleanup 的下次应严格晚于上次");
            assert!(at - last <= 86_400_000, "cleanup 的下次应在 24h 内");
        }
        // 从未跑过 → 立刻到期（`0` 早于任何 now）。
        assert_eq!(next_due("extract", 0), Some(0));
    }

    #[test]
    fn first_weekly_after_lands_on_the_requested_weekday() {
        use chrono::{Datelike, Local, TimeZone, Weekday};
        let base = Local::now().timestamp_millis();
        // 数组元素类型要显式标注成 `(i64, Weekday)`：`first_weekly_after` 收
        // `i64`，而裸字面量默认是 `i32`，不标注会得到「expected i64, found i32」。
        // `day` 与 `auto.tasks.*.weekly` 同口径：1 = 周一 … 7 = 周日。
        let cases: [(i64, Weekday); 7] = [
            (1, Weekday::Mon),
            (2, Weekday::Tue),
            (3, Weekday::Wed),
            (4, Weekday::Thu),
            (5, Weekday::Fri),
            (6, Weekday::Sat),
            (7, Weekday::Sun),
        ];
        for (day, weekday) in cases {
            let at = first_weekly_after(base, day, 11, 0).expect("应能算出下一个该星期");
            assert!(at > base, "第 {day} 天应严格晚于基准");
            assert!(at - base <= 8 * 86_400_000, "第 {day} 天最远 7 天内");
            let got = Local.timestamp_millis_opt(at).single().expect("时间有效").weekday();
            assert_eq!(got, weekday, "第 {day} 天算错了星期");
        }
    }

    #[test]
    fn every_task_has_a_summary_and_zero_cost_tasks_are_marked() {
        for task in TASKS {
            assert!(!task_summary(task).is_empty(), "{task} 没有说明文案");
        }
        // 零 token 的四个必须标成零成本：界面据此写「零成本」，写错就等于
        // 让用户以为一个后台任务在烧钱。
        for task in ["classify", "ingest", "index-scan", "cleanup"] {
            assert!(!task_uses_model(task), "{task} 不该被标成调用模型");
        }
        for task in ["extract", "summarize", "tag", "distill", "consolidate", "profile", "supersede"] {
            assert!(task_uses_model(task), "{task} 会调模型，必须标出来");
        }
    }

    #[test]
    fn task_labels_cover_exactly_the_scheduler_tasks() {
        // `status()` 按 `TASKS` 遍历、用 `TASK_LABELS` 取名字：两张表错位就会
        // 出现「有卡没名」或「有名没卡」。改任何一张都得同步改另一张，
        // 所以把顺序和内容一起钉住 —— 也让「加任务忘了加标签」在 CI 就炸。
        assert_eq!(TASKS.len(), super::super::TASK_LABELS.len());
        for (index, task) in TASKS.iter().enumerate() {
            assert_eq!(*task, super::super::TASK_LABELS[index].0, "第 {index} 个任务名不一致");
        }
        // WebDAV 同步已经摘出自动化：它有自己的页面，不该再出现在这里。
        assert!(!TASKS.contains(&"sync"), "sync 不该是自动化任务");
    }

    #[test]
    fn interval_cadence_text_degrades_to_human_units() {
        assert_eq!(interval_cadence_text(30), "每隔 30 分钟");
        assert_eq!(interval_cadence_text(360), "每隔 6 小时");
        // 1440 分钟写「每隔 1 天」读起来像机器；说「每天」才是人的说法。
        assert_eq!(interval_cadence_text(1440), "每天");
        assert_eq!(interval_cadence_text(10_080), "每隔 7 天");
        // 90 分钟不能被 60 整除，退回分钟：写「每隔 1.5 小时」比分钟更难读。
        assert_eq!(interval_cadence_text(90), "每隔 90 分钟");
    }

    #[test]
    fn weekly_cadence_text_names_the_weekday() {
        assert_eq!(weekly_cadence_text(1, "11:00"), "每周一 11:00");
        assert_eq!(weekly_cadence_text(7, "09:30"), "每周日 09:30");
        // 越界的星期号被 `task_cadence` clamp 过；这里断言兜底落在周日，
        // 而不是 panic 或产出「每周 0」。
        assert_eq!(weekly_cadence_text(0, "11:00"), "每周日 11:00");
    }

    #[test]
    fn task_patch_rejects_unknown_task_and_bad_fields() {
        assert!(parse_task_patch(&json!({ "task": "nope", "enabled": true })).is_err());
        assert!(parse_task_patch(&json!({ "task": "extract" })).is_err(), "一个字段都没有应当报错");
        assert!(parse_task_patch(&json!({ "task": "extract", "enabled": "yes" })).is_err());
        // 节奏必须落在允许区间：0 分钟会让 tick 每一轮都跑，30 天以外没有意义。
        assert!(parse_task_patch(&json!({ "task": "extract", "cadence": { "mode": "interval", "intervalMin": 0 } })).is_err());
        assert!(parse_task_patch(&json!({ "task": "extract", "cadence": { "mode": "interval", "intervalMin": 60 } })).is_ok());
        // 时刻必须能被 `parse_clock` 认（24:00 / 25:00 都不行）—— 认不了的值
        // 写进去会让 `next_due` 静默退回「上次 + 间隔」，页面却还显示那个时刻。
        assert!(parse_task_patch(&json!({ "task": "extract", "cadence": { "mode": "daily", "daily": "24:00" } })).is_err());
        assert!(parse_task_patch(&json!({ "task": "extract", "cadence": { "mode": "daily", "daily": "9:05" } })).is_ok());
        assert!(parse_task_patch(&json!({ "task": "extract", "cadence": { "mode": "weekly", "weekly": 8, "weeklyTime": "11:00" } })).is_err());
    }

    #[test]
    fn task_patch_normalises_clock_text() {
        // 用户可能填 `9:5`：写进配置前统一补零，否则页面与 `next_due` 的
        // 显示会一个写 9:5、一个写 09:05。
        let (_, patch) = parse_task_patch(&json!({
            "task": "extract",
            "cadence": { "mode": "daily", "daily": "9:5" }
        }))
        .expect("9:5 应当被接受");
        match patch.cadence {
            Some(Cadence::Daily(time)) => assert_eq!(time, "09:05"),
            _ => panic!("应当解析成 daily"),
        }
    }
}
