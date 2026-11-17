//! 记忆自动化调度器。
//!
//! ── 这里调度的是什么 ────────────────────────────────────────
//! 十一件事，按「要不要花 token」分两档：
//!
//! 零 token（纯本地，默认开）：
//!   · `index-scan` 增量重建索引 —— 索引是派生物，磁盘才是事实源；
//!   · `classify`  项目归档建议 —— 只算 slug 相似度，不调模型；
//!   · `cleanup`   回收站过期清理 + 队列过期；
//!   · `health`    体检并记一条日志（不写记忆）。
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
//! ── 单写者 ──────────────────────────────────────────────────
//! 所有落盘都发生在调度线程里（`std::thread`，不是 tokio 任务）。记忆的事实源
//! 是文件，两个线程同时读改写同一个 `.md` 就是丢更新。AgentHub 用 stdio 桥把
//! 所有写操作串到一个进程里；本仓用一条专用线程 + 频道做同一件事。

use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::sync::mpsc::{self, Sender};
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

use serde_json::{json, Value};

use super::enrich::{self, Outcome};
use super::{config, index, store};

/// 一次调度周期的间隔。60 秒足够细（最短任务间隔是 30 分钟），
/// 又不会让空闲时的 CPU 占用可见。
const TICK: Duration = Duration::from_secs(60);

/// 任务名清单（顺序即界面上的展示顺序）。
pub const TASKS: [&str; 12] = [
    "extract",
    "summarize",
    "tag",
    "classify",
    "supersede",
    "distill",
    "consolidate",
    "profile",
    "index-scan",
    "cleanup",
    "health",
    "sync",
];

/// 每个任务的默认节奏（分钟），与 `config.rs` 里 `auto.tasks` 的默认值保持一致。
fn default_interval(task: &str) -> i64 {
    match task {
        "classify" => 60,
        "index-scan" => 360,
        // 按天跑的任务用 24h 表达；具体时刻由 `auto.tasks` 里的 `daily` 字段给，
        // 这里只是「两次之间至少隔多久」的兜底
        "supersede" | "distill" | "cleanup" | "health" => 1_440,
        "consolidate" | "profile" => 10_080,
        "sync" => 60,
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
    // `sync` 虽然也不调模型，但它需要先配 WebDAV 才有意义 —— 默认开着只会
    // 每分钟往日志里写一条「没配远端」，所以这里仍按白名单，不跟
    // `task_uses_model` 走。
    matches!(task, "classify" | "index-scan" | "cleanup" | "health")
}

/// 这个任务会不会调模型（即：开它会不会烧 token）。
///
/// ── 为什么单独一个函数，而不是在界面里写死一张表 ──────────────
/// 「哪个任务花 token」是实现事实，不是界面事实：`classify` 早先调过模型，
/// 后来改成纯 slug 相似度、`sync` 走 WebDAV 不碰模型。这类变化只在后端可见，
/// 前端抄一份必然漂移 —— 漂移的后果是界面写着「零成本」而它在扣额度。
///
/// 口径是**当前实现**：`extract` / `summarize` / `tag` / `distill` /
/// `consolidate` / `profile` / `supersede` 会调 `llm`；其余五个纯本地。
/// `task_enabled` 的「没配就默认关」用的也是这条线，两边同一份事实。
fn task_uses_model(task: &str) -> bool {
    matches!(
        task,
        "extract" | "summarize" | "tag" | "distill" | "consolidate" | "profile" | "supersede"
    )
}

fn task_interval(task: &str) -> i64 {
    let tasks = config::object_of("auto.tasks");
    tasks
        .get(task)
        .and_then(|item| item.get("intervalMin"))
        .and_then(Value::as_i64)
        .unwrap_or_else(|| default_interval(task))
        .max(1)
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

/// 上次执行时间（epoch ms）。
///
/// ── 内存 + 库里的一层 ───────────────────────────────────────
/// 进程内的 `LAST_RUN` 是热路径（每轮 tick 读 12 次），不能每次都查库；
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
    if !TASKS.contains(&task) {
        return;
    }
    let key = TASKS.iter().find(|name| **name == task).copied().unwrap_or("health");
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
/// 不用去改十二个任务的签名。
///
/// ── 三个入口都走它 ────────────────────────────────────────
/// 自动 tick、频道里的手动请求、调度线程没起来时的就地执行，最终都落到
/// `run_once`。所以审计写在**这里**，三处自动都有了，不会漏掉某条路径
/// （漏一条就会出现「任务明明跑过，审计里没有」这种最难查的账）。
///
/// ── 为什么 token 用 `llm_call` 的增量算，而不是读 `Outcome` 载荷 ──
/// 只有少数任务的载荷里带 `tokens`（蒸馏带、摘要不带），字段名还各不相同。
/// 跑前读一次「今日累计」、跑后再读一次，差就是这一轮烧掉的 —— 与载荷无关，
/// 十二个任务一套逻辑。
pub fn run_once(task: &str, project: Option<&str>, manual: bool) -> Outcome {
    let before = index::global().tokens_today();
    let started = crate::server::logging::now_ms();
    let outcome = run_inner(task, project, manual);
    let latency = crate::server::logging::now_ms() - started;
    let tokens = (index::global().tokens_today() - before).max(0);
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
        "classify" => classify_pending(project),
        "cleanup" => cleanup(manual),
        "health" => health_check(),
        "extract" => extract(project),
        "summarize" => enrich::summarize(&[], task_batch("summarize")),
        "tag" => enrich::tag(&[], false, task_batch("tag")),
        "distill" => enrich::distill(project, 0),
        "profile" => enrich::build_profile(manual),
        "supersede" => supersede_recent(project),
        "consolidate" => consolidate(project, manual),
        "sync" => super::sync::run(manual),
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

/// 一条人能读的「这一轮干了什么」。
///
/// ── 为什么审计里存一句人话而不是原始 JSON ────────────────────
/// 「任务执行审计」那张表的用途是回答「昨天它到底动了什么」，不是在页面里
/// 再解析一遍结构。载荷里的键名各任务都不同（`written` / `tagged` /
/// `summarized` / `queued` / `indexed`…），把 JSON 原样塞进去，用户看到的是
/// `{"queued":4,"checked":100}` —— 得自己翻译。
///
/// ── 为什么按任务名分支而不是按载荷键猜 ──────────────────────
/// 同一句话在 `extract` 里是「写入 N 条」、在 `consolidate` 里是「合并 N 条」，
/// 语义不同却都叫 `written` / `queued`。按任务名写死措辞比按键名猜更准。
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
                "extract" => format!("处理 {} 条，写入 {} 条", n("material"), n("written")),
                "summarize" => format!("生成摘要 {} 条", n("summarized")),
                "tag" => format!("打标 {} 条", n("tagged")),
                "distill" => format!("读 {} 条素材，产出 {} 条结论", n("material"), n("queued")),
                // 分类不产建议（建议在写入时就产生了，见 `classify_pending` 的
                // 注释），所以这里只能报「扫了多少条还没归类」—— 写「产出 0 条
                // 建议」是把一件从未发生的事说成没发生，读起来像故障。
                "classify" => format!("扫描 {} 条未归类（建议在写入时产生）", n("unclassified")),
                "supersede" => format!("检查 {} 条，产出 {} 条失效建议", n("checked"), n("suggestions")),
                "consolidate" => format!("检查 {} 条，产出 {} 条合并建议", n("checked"), n("queued")),
                "profile" => format!("写入 {} 段画像，覆盖 {} 段", n("written"), n("replaced")),
                "index-scan" => format!("重建索引 {} 条", n("indexed")),
                "cleanup" => format!("清回收站 {} 条，失效入回收站 {} 条", n("trashPurged"), n("supersededTrashed")),
                "health" => format!("陈旧 {} 条，待确认 {} 条", n("stale"), n("pending")),
                "sync" => format!(
                    "上传 {} 个文件（{}），下行 {} 条，冲突 {} 条",
                    n("packed"),
                    bytes_text(payload.get("bytes").and_then(Value::as_i64).unwrap_or(0)),
                    n("pulled"),
                    n("conflicts"),
                ),
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

/// 字节数 → 「5.3MB」。与前端 `sizeText` 同口径（这里只给审计行的摘要用）。
fn bytes_text(bytes: i64) -> String {
    if bytes <= 0 {
        return "0B".to_string();
    }
    let value = bytes as f64;
    if value < 1024.0 {
        format!("{}B", bytes)
    } else if value < 1024.0 * 1024.0 {
        format!("{:.1}KB", value / 1024.0)
    } else if value < 1024.0 * 1024.0 * 1024.0 {
        format!("{:.1}MB", value / 1024.0 / 1024.0)
    } else {
        format!("{:.2}GB", value / 1024.0 / 1024.0 / 1024.0)
    }
}

/// 归类建议：把 `general/` 里未归类的 l1 记忆，按 cwd 的 git 身份补建议。
///
/// 只对**有 cwd 记录的条目**发建议 —— 没有 cwd 就没法判断它属于哪个项目，
/// 而按内容猜项目等于编造。
fn classify_pending(project: Option<&str>) -> Outcome {
    let pending = index::global().review_count();
    if pending >= config::usize_of("dedup.pendingWarnThreshold").max(10) {
        return Outcome::Skipped(format!("待确认已积压 {pending} 条，先处理队列再说"));
    }
    let entries = store::load_all();
    let unclassified: Vec<_> = entries
        .iter()
        .filter(|entry| entry.is_live() && entry.project.is_none())
        .filter(|entry| match project {
            Some(slug) => entry.project.as_deref() == Some(slug),
            None => true,
        })
        .collect();
    if unclassified.is_empty() {
        return Outcome::Skipped("没有未归类的记忆".into());
    }
    Outcome::Done(json!({
        "unclassified": unclassified.len(),
        "note": "归类建议在写入时就已产生（写入方知道 cwd）；这里只做计数与提示",
    }))
}

/// 清理：回收站过期文件 + 失效记忆入回收站（后者默认关）。
fn cleanup(manual: bool) -> Outcome {
    let days = config::usize_of("storage.trashKeepDays").max(1);
    let removed = store::purge_expired_trash(days);
    let mut trashed = 0usize;
    // 「自动清理失效记忆」默认关：失效记忆可能是演化链的一环，
    // 用户看着历史的时候需要它还在
    if config::bool_of("cleanup.autoDeleteSuperseded") {
        let now = crate::server::logging::now_ms();
        for entry in store::load_all() {
            if entry.deleted_at.is_some() {
                continue;
            }
            let stale = entry.superseded_by.is_some() || entry.is_expired(now);
            if !stale {
                continue;
            }
            // 画像与被钉住的条目不动：它们是被显式标成「要一直看见」的
            if entry.pinned || entry.kind == super::MemoryKind::Profile {
                continue;
            }
            let mut entry = entry;
            if store::detach_live(&entry).is_err() {
                continue;
            }
            entry.deleted_at = Some(now);
            entry.updated_at = now;
            entry.path = store::rel(&store::entry_path(&entry, true));
            if store::save(&entry).is_ok() {
                let _ = index::global().upsert(&entry);
                trashed += 1;
            }
            if trashed >= 200 {
                break;
            }
        }
    }
    if removed == 0 && trashed == 0 && !manual {
        return Outcome::Skipped("没有需要清理的内容".into());
    }
    Outcome::Done(json!({ "trashPurged": removed, "supersededTrashed": trashed, "keepDays": days }))
}

/// 体检：只统计、不改数据。结果进日志与状态页。
fn health_check() -> Outcome {
    let report = super::health(50);
    let stale = report.get("stale").and_then(Value::as_array).map(Vec::len).unwrap_or(0);
    let pending = report.get("pending").and_then(Value::as_u64).unwrap_or(0);
    if stale > 0 {
        crate::server::logging::log(
            "[Memory]",
            &format!("体检：{stale} 条记忆锚定的文件在写入后被改过，建议复核"),
        );
    }
    Outcome::Done(json!({ "stale": stale, "pending": pending }))
}

/// 抽取：把最近的 l1 流水压成结论候选（`extract` 与 `distill` 的分工）。
///
/// `extract` 只看**最近一批**（默认 20 条），产出比 distill 更轻：它不要求
/// 跨项目规律，只回答「这批流水里有什么值得留下来」。所以它的产物直接走
/// `memory_write`（写一条 l2），不进队列 —— 轻量、可撤销（删掉就行）。
fn extract(project: Option<&str>) -> Outcome {
    if !config::bool_of("deep.enabled") {
        return Outcome::Skipped("深层记忆已关闭".into());
    }
    let batch = task_batch("extract");
    let mut recent: Vec<super::model::MemoryEntry> = store::load_all()
        .into_iter()
        .filter(|entry| entry.is_live() && entry.layer == super::Layer::L1)
        .filter(|entry| match project {
            Some(slug) => entry.project.as_deref() == Some(slug),
            None => true,
        })
        .collect();
    recent.sort_by_key(|entry| std::cmp::Reverse(entry.updated_at));
    recent.truncate(batch);
    if recent.len() < 3 {
        return Outcome::Skipped("最近流水太少，不值得抽取".into());
    }
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
         输出 JSON：{{\"items\":[{{\"title\":\"不超过 40 字\",\"type\":\"decision|knowledge|pitfall\",\"body\":\"100-300 字\",\"importance\":0.0-1.0}}]}}\n\n\
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
    let mut written = 0usize;
    for item in items.iter().take(3) {
        let body = item.get("body").and_then(Value::as_str).unwrap_or("").trim();
        if body.chars().count() < 20 {
            continue;
        }
        let kind = item.get("type").and_then(Value::as_str).unwrap_or("knowledge");
        let title = item.get("title").and_then(Value::as_str).unwrap_or("").to_string();
        let importance = item.get("importance").and_then(Value::as_f64).unwrap_or(0.6);
        let input = super::WriteInput {
            content: body.to_string(),
            title: Some(title),
            kind: Some(kind.to_string()),
            layer: Some("l2".to_string()),
            project: project.map(str::to_string),
            agent: Some("auto-extract".to_string()),
            tags: Vec::new(),
            importance: Some(importance),
            files: Vec::new(),
            supersedes: Vec::new(),
            pinned: Some(false),
            starred: Some(false),
            cwd: None,
            session: None,
            valid_to: None,
            evidence: recent.iter().map(|entry| entry.id.clone()).take(5).collect(),
        };
        // 走 super::write 而不是直接落盘：抽取产物同样要经过去重、hash、索引
        if super::write(input).is_ok() {
            written += 1;
        }
    }
    if written == 0 {
        return Outcome::Skipped("这批流水里没有值得留下来的结论".into());
    }
    Outcome::Done(json!({ "written": written, "material": recent.len(), "model": reply.model }))
}

const SYSTEM_EXTRACT: &str = "You curate a long-term project memory. \
Keep only conclusions and lessons that will still matter in a month. \
Drop progress reports, greetings and transient state. \
Answer in the same language as the input. Output strict JSON, no prose around it.";

/// 失效判定：对最近写入的 l2 条目逐条跑 `detect_supersede`。
fn supersede_recent(project: Option<&str>) -> Outcome {
    let mut recent: Vec<super::model::MemoryEntry> = store::load_all()
        .into_iter()
        .filter(|entry| entry.is_live() && entry.layer == super::Layer::L2)
        .filter(|entry| entry.kind != super::MemoryKind::Profile)
        .filter(|entry| match project {
            Some(slug) => entry.project.as_deref() == Some(slug),
            None => true,
        })
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
        .filter(|entry| entry.is_live() && entry.layer == super::Layer::L2)
        .filter(|entry| entry.project.as_deref() == Some(slug))
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
        index::global().enqueue(
            "consolidate",
            &target,
            &json!({
                "title": group.get("title").and_then(Value::as_str).unwrap_or(""),
                "type": group.get("type").and_then(Value::as_str).unwrap_or("knowledge"),
                "body": body,
                "sources": sources,
                "project": slug,
                "model": reply.model,
            }),
            group.get("confidence").and_then(Value::as_f64).unwrap_or(0.7),
        );
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
    // 一次查完再分发，不在下面每个任务里各查一遍 —— 12 个任务 =
    // 12 次 `GROUP BY`，而它们读的是同一张表的同一批行。
    let stats = index::global().task_stats();
    let tasks: Vec<Value> = TASKS
        .iter()
        .map(|task| {
            let last = last_of(task);
            let (runs, ok, tokens) = stats.get(*task).copied().unwrap_or((0, 0, 0));
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
                // 累计统计。`runs` 含跳过（跳过也是跑过的），`ok` 只数成功。
                "runs": runs,
                "success": ok,
                "tokens": tokens,
                // 这个任务要不要花 token。零 token 的标「零成本」，
                // 花 token 的标「调用模型」——用户扫一眼就知道开它会烧钱。
                "usesModel": task_uses_model(task),
                // 一句「它在干什么」，进表格的副标题。文案与 `detail_of`
                // 的分支同名同义，但这里是**静态说明**（还没跑时也要显示），
                // 那边是**运行结果**（跑完才有）。
                "summary": task_summary(task),
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
        // 最近几次执行（「任务执行记录」那张表）。取 30 条：一页表格看得完，
        // 又不至于把一轮 12 个任务的整轮吞掉一半。
        // `runRows` 是可翻的**行数**，`runCount` 是**执行次数**（合并后的
        // 一行代表多次）—— 前者给「查看全部 N 条」，后者给页面顶部的概览。
        "recentRuns": index::global().recent_task_runs(30),
        "runRows": index::global().task_run_count(),
        "runCount": index::global().task_run_executions(),
        "labels": labels,
        "tasks": tasks,
    })
}

/// 任务执行审计（「查看全部」那一页）。
///
/// `status()` 里已带 30 条预览，这个接口只为翻页：`limit` 上限 500 由
/// `recent_task_runs` 兜底（一次拉太多没有意义 —— `auto.logKeepCount`
/// 默认只留几百行）。
///
/// 返回两个数：`total` 是**行数**（还能翻出多少行），`runs` 是**执行次数**
/// （`repeat` 求和，与任务行「累计 N 次」同一口径）。合并过以后两者不再相等，
/// 页面上的「查看全部 N 条」要用行数，任务行的「累计」要用次数。
pub fn runs(limit: usize) -> Value {
    json!({
        "items": index::global().recent_task_runs(limit.clamp(1, 500)),
        "total": index::global().task_run_count(),
        "runs": index::global().task_run_executions(),
    })
}

/// 任务的一句静态说明（「它在干什么」）。
/// 与 `TASK_LABELS` 分开：那个是**名字**（「抽取要点」，用于标题与下拉），
/// 这里是**解释**（「给新记忆抽一句摘要，写入摘要字段」）。目标界面的表格
/// 每行都带这么一句，没有它用户只能从名字猜这个任务会动什么数据。
fn task_summary(task: &str) -> &'static str {
    match task {
        "extract" => "把最近的流水压成结论候选，写进记忆",
        "summarize" => "为长文本记忆生成一句摘要，供检索行显示",
        "tag" => "按内容推断 2-5 个标签，优先复用已有标签",
        "classify" => "按 cwd 的 git 身份给未归类记忆补项目建议",
        "supersede" => "拿新记忆比同主题旧记忆，输出失效建议",
        "distill" => "把同项目的原始碎片蒸馏成 L2 结论与决策",
        "consolidate" => "把同类 L2 结论合并成一条更完整的",
        "profile" => "跨项目提炼用户画像：习惯、偏好、工作方式",
        "index-scan" => "全量重建索引与检索缓存（零成本）",
        "cleanup" => "清回收站与失效记忆（零成本）",
        "health" => "记忆体检：陈旧条目、待确认积压（零成本）",
        "sync" => "把记忆文件夹打包同步到 WebDAV 远端",
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
    }

    #[test]
    fn last_run_stamp_round_trips() {
        stamp("health", 123);
        assert_eq!(last_of("health"), 123);
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
        use chrono::{Local, TimeZone};
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
        // 零 token 的四个 + sync（它走 WebDAV，不调模型）必须标成零成本：
        // 界面据此写「零成本」，写错就等于让用户以为一个后台任务在烧钱。
        for task in ["classify", "index-scan", "cleanup", "health", "sync"] {
            assert!(!task_uses_model(task), "{task} 不该被标成调用模型");
        }
        for task in ["extract", "summarize", "tag", "distill", "consolidate", "profile", "supersede"] {
            assert!(task_uses_model(task), "{task} 会调模型，必须标出来");
        }
    }
}
