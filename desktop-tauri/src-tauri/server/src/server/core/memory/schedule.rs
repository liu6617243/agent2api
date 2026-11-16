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
pub const TASKS: [&str; 11] = [
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
    // 表里没有这个任务：零 token 的默认开，花 token 的默认关
    matches!(task, "classify" | "index-scan" | "cleanup" | "health")
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

/// 上次执行时间（epoch ms）。存内存：进程重启后「距上次」归零，
/// 启动后第一轮会各跑一次 —— 这正是重启后该发生的事（索引要重新对账）。
static LAST_RUN: OnceLock<Mutex<std::collections::HashMap<&'static str, i64>>> = OnceLock::new();

fn last_run() -> &'static Mutex<std::collections::HashMap<&'static str, i64>> {
    LAST_RUN.get_or_init(|| Mutex::new(std::collections::HashMap::new()))
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
        let interval = task_interval(task) * 60_000;
        if now - last_of(task) < interval {
            continue;
        }
        // 先记时间戳再跑：任务失败时不该在下一分钟立刻重试（那会把日志刷满，
        // 而且网络故障期间重试是纯浪费）。用户可以在界面点「立即执行」重试。
        stamp(task, now);
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
pub fn run_once(task: &str, project: Option<&str>, manual: bool) -> Outcome {
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
    let tasks: Vec<Value> = TASKS
        .iter()
        .map(|task| {
            let last = last_of(task);
            json!({
                "task": task,
                "enabled": task_enabled(task),
                "intervalMin": task_interval(task),
                "lastRun": if last > 0 { json!(last) } else { Value::Null },
                "lastRunText": if last > 0 { json!(super::search::relative_time(last, now)) } else { Value::Null },
                "nextRun": if last > 0 { json!(last + task_interval(task) * 60_000) } else { json!(now) },
            })
        })
        .collect();
    let labels: Vec<Value> = super::TASK_LABELS
        .iter()
        .map(|(task, label)| json!({ "task": task, "label": label }))
        .collect();
    json!({
        "running": RUNNING.load(Ordering::SeqCst),
        "lastTick": LAST_TICK.load(Ordering::SeqCst),
        "globalEnabled": config::bool_of(config::ENABLE_KEY) && config::bool_of("auto.enabled"),
        "privacyPaused": config::bool_of("privacy.pause"),
        "dailyTokenLimit": config::number("auto.dailyTokenLimit") as i64,
        "tokensToday": index::global().tokens_today(),
        "budgetExhausted": super::llm::budget_exhausted(),
        "labels": labels,
        "tasks": tasks,
    })
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
}
