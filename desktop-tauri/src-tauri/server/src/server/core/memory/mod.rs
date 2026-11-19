//! 记忆中枢 · 模块门面。
//!
//! ── 这一层为什么存在 ────────────────────────────────────────
//! 上层（HTTP API 与 MCP 工具）只该看到「一条记忆进来、一段文本出去」，不该
//! 关心 frontmatter 怎么写、索引怎么建、slug 怎么推。所以
//! `model`（形状）/ `store`（文件）/ `index`（检索与队列）/ `search`（排序与披露）/
//! `layout`（项目归类）/ `config`（可调项）六块都在内部，本文件是唯一出口。
//!
//! ── 与 AgentHub「记忆中枢」的关系 ────────────────────────────
//! **功能对齐、代码重写。** 设计参考自 AgentHub（作者：沐晖 / HUIdada1，
//! https://github.com/HUIdada1/AgentHub）的记忆中枢：MD 事实源 + 派生索引、
//! 分层（l1 流水 / l2 沉淀）、三级披露、`supersedes` 事实演化、待确认队列、
//! 深层画像、自动化调度、WebDAV 同步、回收站。原项目该模块的文件头写明
//! 「禁止未经授权直接搬运代码」，因此本项目**未复制其实现**，只按上述功能规格
//! 用 Rust 重写；行为等价之处是设计对齐的结果，不是代码搬运。
//!
//! ── 磁盘格式是跨实现的契约 ──────────────────────────────────
//! `config.json` / `projects.json` / `general/` / `projects/<slug>/` /
//! `profile/` / `trash/` 这套布局与 frontmatter 键名和 AgentHub 一致：两边的
//! 记忆目录可以互相直接读。所以这些名字改动要当成协议变更处理。
//!
//! ── 记忆为什么不经由网关注入 system prompt ───────────────────
//! 网关不知道「这个请求属于哪个项目」，只能在请求体里塞一段与上下文无关的
//! 文本；而记忆的价值恰恰在「什么时候该读哪一条」。所以记忆只通过 MCP 工具
//! 暴露，由客户端决定何时读取 —— 这也是 AgentHub 的做法。

pub mod agents;
pub mod config;
pub mod enrich;
pub mod import;
pub mod import_registry;
pub mod index;
pub mod layout;
pub mod llm;
pub mod model;
pub mod rt;
pub mod schedule;
pub mod search;
pub mod store;
pub mod sync;

use std::sync::OnceLock;

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use layout::Classified;
use model::{truncate_chars, DedupStatus, Layer, MemoryEntry};
use search::Query;

/// 对外暴露的核心类型。
///
/// `MemoryKind` 必须从这里出：`tool_services::memory_mcp` 与 `api::memory`
/// 要用它校验 `type` 参数、给界面列出可选类别。`model` 本身是 `pub mod`，但
/// 让调用方写 `memory::model::MemoryKind` 等于把内部文件布局变成对外契约 ——
/// 改一次 model 的分文件就波及调用方。re-export 一次之后，调用方只认
/// `memory::MemoryKind`。
pub use model::{DedupStatus as MemoryDedupStatus, Layer as MemoryLayer, MemoryKind};
pub use search::Query as MemoryQuery;

// ── 画像四类 ─────────────────────────────────────────────────
//
// 四类的 key 进了 frontmatter（`type=profile` 条目的 `tags[0]`），所以它同时是
// **磁盘格式的一部分**：改 key 等于让存量画像从 `profile_view` 里消失。因此这里
// 只增不减 —— 旧 key 留在 `legacy` 一侧，按新 key 归类时回落，不会丢条目。
//
// `identity` / `communication` / `technical` / `workstyle` 是「关于这个人」的四个
// 正交视角：他是谁（人格特质）、他怎么跟人说话（沟通偏好）、他擅长什么（技术偏好）、
// 他怎么干活（工作习惯）。原来的 `preference / habit / expertise / context` 混了
// 两个维度（「偏好」既可能是沟通也可能是技术），一个结论同时属于两类的后果是
// 分类全靠模型措辞，同一份画像重生成一次就可能整批换组。
pub const PROFILE_SUBS: [(&str, &str, &str); 4] = [
    // (key, 中文名, 一句话说明：界面上那行小字)
    ("identity", "人格特质", "思维倾向、决策模式与协作个性，帮助 Agent 理解你的深层思考习惯"),
    ("communication", "沟通偏好", "说话方式、信息密度与反馈节奏：Agent 该怎样向你汇报与求证"),
    ("technical", "技术偏好", "技术选型倾向、工具链习惯与代码审美：Agent 该按什么标准写代码"),
    ("workstyle", "工作习惯", "排查路径、验证方式与时间安排：Agent 该在什么时候做什么、怎么自证"),
];

/// 旧 key → 新 key。存量画像文件里写的是这四个，读的时候按它归位。
///
/// `preference` 归到 `communication` 而不是 `technical`：旧 prompt 对 preference 的
/// 定义是「工具、写法、沟通方式」，其中沟通方式与说话风格是最常被总结出来的一类，
/// 而技术细节在旧数据里本来就稀疏（它要求「反复出现的深度证据」）。归错一类的
/// 代价是用户看到一条「技术偏好：喜欢简短回复」，归到沟通类至少读得通。
pub const PROFILE_LEGACY_SUBS: [(&str, &str); 4] = [
    ("preference", "communication"),
    ("habit", "workstyle"),
    ("expertise", "technical"),
    ("context", "identity"),
];

/// 把任意一个 `sub`（新 key / 旧 key / 模型自由发挥的字符串）归到四类之一。
///
/// 为什么必须兜底而不是 `Option`：画像条目的分类只用于**分组展示**，漏一类的
/// 后果是一条结论在界面上彻底不可见（用户以为它被删了）。所以宁可归错也不能归丢。
pub fn profile_sub(raw: &str) -> &'static str {
    let key = raw.trim().to_ascii_lowercase();
    // 用 `for` + `copied()` 而不是 `iter().find(...)`：`iter()` 的元素是
    // `&(&str, &str, &str)`，闭包与解构里会多出一层引用（`&&str`），返回时
    // 还得再解一次 —— 那种写法编译能过但读起来全是 `*`。`copied()` 把元素变成
    // `(&str, &str, &str)`，解构出来就是 `&'static str`，直接可返回。
    for (name, _, _) in PROFILE_SUBS.iter().copied() {
        if name == key.as_str() {
            return name;
        }
    }
    for (old, mapped) in PROFILE_LEGACY_SUBS.iter().copied() {
        if old == key.as_str() {
            return mapped;
        }
    }
    // 模型自造的类别（"personality" / "style" …）：按关键词猜，最后落 identity。
    if key.contains("commun") || key.contains("talk") || key.contains("feedback") {
        return "communication";
    }
    if key.contains("tech") || key.contains("skill") || key.contains("code") || key.contains("tool") {
        return "technical";
    }
    if key.contains("work") || key.contains("habit") || key.contains("routine") {
        return "workstyle";
    }
    "identity"
}

/// 「这条是用户自己钉的」标记（写进 `tags`，与四类 key 并列）。
///
/// ── 为什么需要一个标记，不能只看 `pinned` ────────────────────
/// 上一版给每一条生成的画像都设了 `pinned = true`（当时的理由是「画像必须进
/// `memory_core`」）。于是升级之后，库里所有画像条目都有 `pinned`，光看这个布尔
/// 分不出「用户钉的」与「上一版批量钉的」—— 把前者当真会保护住全部旧结论、让
/// 重生成什么也换不掉；把后者当真会让用户钉的那条一起被冲掉。
///
/// 标记之后口径变成一句可执行的话：**`pinned && tags 含本标记` 才是用户的钉子**。
/// 生成器只写 `starred`，用户点置顶时由 `update()` 顺手打上这个标记。
///
/// key 用 ASCII：它同时是磁盘上 frontmatter 的一部分，非 ASCII 标签在别的
/// 记忆工具里过滤时容易出现编码差异。
pub const PROFILE_PIN_TAG: &str = "pin";

/// 自动化任务的中文名（状态页与服务清单共用一份，避免两处漂移）。
///
/// 条目数必须与 `schedule::TASKS` 一致 —— `status()` 是按 `TASKS` 遍历、
/// 用这张表取名字的，少一条就会出现「有卡没名」或「有名没卡」。
pub const TASK_LABELS: [(&str, &str); 11] = [
    ("extract", "抽取结构化信息"),
    ("summarize", "生成摘要"),
    ("tag", "自动打标签"),
    ("classify", "项目归类建议"),
    ("supersede", "失效判定"),
    ("distill", "L2 蒸馏"),
    ("consolidate", "去重合并"),
    ("profile", "人格 / 偏好画像"),
    ("index-scan", "索引自愈扫描"),
    ("cleanup", "异常记忆清理"),
    ("health", "记忆体检"),
];

// ── 设置 ─────────────────────────────────────────────────────

/// 模块设置的**兼容视图**。
///
/// 真实配置在 `config`（一百多个扁平点号键，schema 驱动）。这里只投影出两个
/// 老字段，因为它们在两处被高频读取：`enabled`（每个 MCP 工具调用都要判）
/// 与 `core_max_tokens`（`memory_core` 的默认预算）。
///
/// ── 为什么还要保留这个结构 ─────────────────────────────────
/// `api/memory.rs` 的 `GET /api/memory/config` 与 `memory_mcp` 的状态输出都
/// 以它为形状；直接换成整棵 schema 树会让这两个响应变成「一百个键的大对象」，
/// 而它们只需要「开没开、给多少预算」。完整配置走 `config::schema_json()`。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct MemoryConfig {
    /// 记忆服务总开关。关掉后读写类工具一律拒绝（`memory_status` 仍可用，
    /// 否则关了之后连「为什么没记忆」都查不了）。用途是临时让 Agent 忘记，
    /// 不必去每个客户端里逐个卸载。
    pub enabled: bool,
    /// `memory_core` 的默认 token 预算（工具参数可逐个覆盖）
    pub core_max_tokens: usize,
}

impl Default for MemoryConfig {
    fn default() -> Self {
        MemoryConfig {
            enabled: true,
            core_max_tokens: 800,
        }
    }
}

/// 读模块设置（每次调用都是缓存命中，成本是一次 `metadata()`）。
pub fn config() -> MemoryConfig {
    MemoryConfig {
        enabled: config::bool_of(config::ENABLE_KEY),
        core_max_tokens: config::usize_of("agents.coreMaxTokens").clamp(200, 4000),
    }
}

/// 部分更新：只改传进来的键。返回更新后的兼容视图。
///
/// 接受三种形状（页面与 MCP 都在用）：
/// * `{"agents.coreMaxTokens": 900}` —— 扁平点号键
/// * `{"coreMaxTokens": 900}` —— v1 旧键（自动映射）
/// * `{"enabled": false}`
pub fn save_config(patch: &Value) -> Result<MemoryConfig, String> {
    config::patch(patch)?;
    bump_revision();
    Ok(config())
}

// ── 索引保鲜 ─────────────────────────────────────────────────

/// 记忆内容的「版本号」：每次写入 / 更新 / 删除 / 恢复 / 重建都 +1。
///
/// 页面与 MCP 用它判断「手里这份是不是过期了」。有版本号之后：内容没变时命中
/// 缓存（零 IO），内容一变立即失效（没有 TTL 带来的「刚写的记忆 N 秒后才生效」）。
static REVISION: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

pub fn revision() -> u64 {
    REVISION.load(std::sync::atomic::Ordering::Relaxed)
}

/// 内容版本号 +1。页面与 MCP 据此判断「手里这份是不是过期了」。
///
/// 公开是因为自动化页的「改开关 / 改节奏」走 `schedule::apply_task`（它改的是
/// 配置，但入口不在 `save_config` 上）：那条路不改版本号的话，另一个页签
/// 打开着配置页时会一直显示旧值。
pub fn bump_revision() {
    REVISION.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
}

/// 进程内只做一次「事实源 vs 索引」的一致性检查：条数或最新更新时间对不上
/// 就整表重建。索引是派生的，重建永远安全。
///
/// 为什么不做文件监听：写入都经过本模块（写文件的同时更新索引），外力改动
/// （用户手编 .md）只需要「下次启动 / 下次点重建」生效，这比一个跨平台的
/// 文件监听器简单得多，也不会在编辑器保存的瞬间读到半个文件。
fn ensure_index_once() {
    static DONE: OnceLock<()> = OnceLock::new();
    DONE.get_or_init(|| {
        let entries = store::load_all();
        let index = index::global();
        let live = entries.iter().filter(|entry| entry.is_live()).count();
        if index.count() != live {
            match index.rebuild(&entries) {
                Ok(count) => crate::server::logging::log(
                    "[Memory]",
                    &format!("记忆索引已重建：{count} 条（事实源 {live} 条有效）"),
                ),
                Err(error) => crate::server::logging::log(
                    "[Memory]",
                    &format!("⚠️ 记忆索引重建失败：{error}"),
                ),
            }
            // 重建时顺手把 id 之间的关联补齐（同项目 + 共享标签）
            relink(&entries);
        }
    });
}

/// 补 `mem_link`：同项目且共享标签的活条目之间连线。
///
/// 为什么在重建时做而不是每次写入做：连线的成本是 O(同项目条目数)，写入路径
/// 上做会把「写一条记忆」从毫秒级拖到几十毫秒；而连线不是实时需求 —— 检索时
/// 差几分钟的关联，用户感知不到。
fn relink(entries: &[MemoryEntry]) {
    let live: Vec<&MemoryEntry> = entries.iter().filter(|entry| entry.is_live()).collect();
    let mut links = 0usize;
    for (index, left) in live.iter().enumerate() {
        for right in live.iter().skip(index + 1) {
            if left.project != right.project {
                continue;
            }
            if !left.tags.is_empty() && left.tags.iter().any(|tag| right.tags.contains(tag)) {
                index::global().link(&left.id, &right.id, "same_topic");
                links += 1;
            } else if !left.supersedes.is_empty() && left.supersedes.contains(&right.id) {
                index::global().link(&left.id, &right.id, "relates");
                links += 1;
            }
        }
    }
    if links > 0 {
        crate::server::logging::log("[Memory]", &format!("记忆关联图已更新：{links} 条边"));
    }
}

/// 全量重建（「重建索引」按钮 / 索引损坏时）
pub fn rebuild() -> Result<usize, String> {
    store::ensure_layout()?;
    let entries = store::load_all();
    let count = index::global().rebuild(&entries)?;
    relink(&entries);
    bump_revision();
    Ok(count)
}

// ── 读 ───────────────────────────────────────────────────────

pub fn list(query: &Query) -> Value {
    ensure_index_once();
    let now = crate::server::logging::now_ms();
    let (rows, total) = search::search_page(query);
    let items: Vec<Value> = rows.iter().map(|row| row.to_json()).collect();
    index::global().log_recall(
        "search",
        &query.text,
        items.len(),
        0,
        query.agent.as_deref(),
        query.project.as_deref(),
    );
    json!({
        "items": items,
        "lines": rows.iter().map(|row| search::compact_line(row, now)).collect::<Vec<_>>(),
        // 命中总数（截断前）。前端按它算页数 —— 用 items.len() 会永远显示第 1/1 页。
        "total": total,
        "offset": query.offset,
        "fts": index::global().fts_enabled(),
    })
}

/// 每日写入量的日历桶（浏览页「热力图」视图）。
///
/// ── 为什么按 `created_at` 而不是 `updated_at` ──────────────
/// 热力图回答的是「我哪天写了东西」。`updated_at` 会被星标、置顶、改标签这些
/// 不产生新内容的操作推到现在，用它画出来的图会把「改了个标签」记成一次写作 ——
/// 越用越失真。
///
/// ── 为什么把时区当参数传进来 ──────────────────────────────
/// 界面说「今天写了 12 条」，用户心里的「今天」是本地日历。后端跑在同一台机器上，
/// 但 Rust 标准库拿不到本地时区偏移（要引 chrono），所以由前端把
/// `-new Date().getTimezoneOffset()` 算好的分钟数送进来，这里只做加法。
/// 不传就按 UTC —— 与 `search::format_date` 的默认行为一致，不会跑偏成第三种结果。
pub fn heat(days: usize, tz_minutes: i64) -> Value {
    ensure_index_once();
    let span = days.clamp(7, 366);
    let now = crate::server::logging::now_ms();
    // 时区偏移夹在 ±14 小时内：这是现实世界的极限，越界值（例如前端传了毫秒）
    // 会把「今天」推到几百年后，热力图直接空掉，看不出是参数错。
    let offset = tz_minutes.clamp(-840, 840) * 60_000;
    let since = now - (span as i64) * 86_400_000;
    let mut buckets: std::collections::BTreeMap<String, usize> = std::collections::BTreeMap::new();
    for entry in store::load_all().iter().filter(|item| item.is_live()) {
        if entry.created_at < since {
            continue;
        }
        *buckets.entry(search::format_date(entry.created_at + offset)).or_insert(0) += 1;
    }
    let items: Vec<Value> = buckets
        .iter()
        .map(|(day, count)| json!({ "day": day, "count": count }))
        .collect();
    let total: usize = buckets.values().sum();
    json!({
        "items": items,
        "days": span,
        "total": total,
        // 「活跃天」= 至少写过一条的天数，不是跨度天数 —— 图例上的数字
        "activeDays": buckets.len(),
        "end": search::format_date(now + offset),
    })
}

/// 按 id 取全文（可一次取多条）。顺带给出 3 条相关记忆
/// ——「读一条时看到旁边那条」是最常见的发现路径。
pub fn get(ids: &[String], body_limit: usize) -> Value {
    ensure_index_once();
    let all = store::load_all();
    let now = crate::server::logging::now_ms();
    let mut items = Vec::new();
    for id in ids {
        let Some(entry) = all.iter().find(|entry| &entry.id == id) else {
            continue;
        };
        let mut value = entry.to_json(true);
        if body_limit > 0 {
            let body = truncate_chars(&entry.body, body_limit);
            let truncated = body.chars().count() < entry.body.chars().count();
            value["body"] = Value::String(body);
            value["truncated"] = Value::Bool(truncated);
        }
        // 相关记忆：同项目、同类别或共享标签的前 3 条（排除自己）
        let mut related: Vec<(&MemoryEntry, usize)> = all
            .iter()
            .filter(|other| other.id != entry.id && other.is_live())
            .map(|other| {
                let mut score = 0usize;
                if other.project == entry.project && entry.project.is_some() {
                    score += 2;
                }
                if other.kind == entry.kind {
                    score += 1;
                }
                if other.layer == entry.layer {
                    score += 1;
                }
                score += other
                    .tags
                    .iter()
                    .filter(|tag| entry.tags.contains(tag))
                    .count();
                (other, score)
            })
            .filter(|(_, score)| *score > 0)
            .collect();
        related.sort_by(|a, b| b.1.cmp(&a.1).then(b.0.updated_at.cmp(&a.0.updated_at)));
        value["related"] = Value::Array(
            related
                .into_iter()
                .take(3)
                .map(|(other, _)| {
                    json!({
                        "id": other.id,
                        "title": other.title,
                        "type": other.kind.as_str(),
                        "layer": other.layer.as_str(),
                        "line": search_compact(other, now),
                    })
                })
                .collect(),
        );
        items.push(value);
    }
    if !ids.is_empty() {
        index::global().hit(ids);
        index::global().log_recall("get", &ids.join(","), items.len(), 0, None, None);
    }
    json!({ "items": items })
}

fn search_compact(entry: &MemoryEntry, now: i64) -> String {
    format!(
        "[{}] {}（{}） · `{}`",
        entry.kind.label(),
        truncate_chars(&entry.title, 50),
        search::relative_time(entry.updated_at, now),
        entry.id
    )
}

/// 事实演化链：以 `supersedes` 为边，向上找根、向下找后继。
pub fn timeline(id: &str) -> Value {
    let all = store::load_all();
    let Some(target) = all.iter().find(|entry| entry.id == id) else {
        return json!({ "items": [] });
    };
    // 向上：我取代了谁（递归），向下：谁取代了我（递归）
    let mut chain: Vec<&MemoryEntry> = Vec::new();
    let mut cursor = Some(target);
    let mut guard = 0;
    while let Some(current) = cursor {
        guard += 1;
        if guard > 32 {
            break;
        }
        chain.push(current);
        cursor = current
            .supersedes
            .first()
            .and_then(|previous| all.iter().find(|entry| &entry.id == previous));
    }
    chain.reverse();
    let mut forward = target.superseded_by.clone();
    let mut guard = 0;
    while let Some(next_id) = forward {
        guard += 1;
        if guard > 32 {
            break;
        }
        let Some(next) = all.iter().find(|entry| entry.id == next_id) else {
            break;
        };
        if chain.iter().any(|item| item.id == next.id) {
            break;
        }
        chain.push(next);
        forward = next.superseded_by.clone();
    }
    let now = crate::server::logging::now_ms();
    json!({
        "items": chain.into_iter().map(|entry| json!({
            "id": entry.id,
            "title": entry.title,
            "type": entry.kind.as_str(),
            "typeLabel": entry.kind.label(),
            "layer": entry.layer.as_str(),
            "validFrom": entry.valid_from,
            "validTo": entry.valid_to,
            "updatedAt": entry.updated_at,
            "validFromText": search::format_date(entry.valid_from),
            "relative": search::relative_time(entry.updated_at, now),
            "supersededBy": entry.superseded_by,
            "current": entry.id == target.id,
        })).collect::<Vec<_>>(),
    })
}

/// 最近 N 天更新过的记忆（`memory_recent` 工具用）。
///
/// 走索引而不是扫文件：这里要的是「时间段 + 排序」，索引本来就按 `updated_at`
/// 建了索引列；扫文件要全量解析 frontmatter，只为丢掉大部分行。
pub fn recent(project: Option<&str>, agent: Option<&str>, days: usize) -> Value {
    ensure_index_once();
    let now = crate::server::logging::now_ms();
    let days = days.clamp(1, 3650) as i64;
    let since = now - days * 24 * 3600 * 1000;
    let query = Query {
        project: project.map(str::to_string),
        agent: agent.map(str::to_string),
        limit: 200,
        ..Default::default()
    };
    let page = list(&query);
    let items: Vec<Value> = page
        .get("items")
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter(|item| item.get("updatedAt").and_then(Value::as_i64).unwrap_or(0) >= since)
                .cloned()
                .collect()
        })
        .unwrap_or_default();
    let count = items.len();
    json!({ "days": days, "count": count, "items": items })
}

/// 项目列表（登记表 + 实际落盘条数 + 分层计数 + 关联 Agent）
///
/// ── 为什么要单独统计「通用」 ──────────────────────────────────
/// 通用项目（`_global`）不是一张普通项目卡：它是**没有归属项目**的记忆的落脚点，
/// 条数往往远大于任何一个具体项目。混在列表里既抢注意力，又让人误以为通用是个
/// 可以合并/归档的对象。所以单独算一个 `general` 汇总挂在返回体上。
pub fn projects() -> Value {
    ensure_index_once();
    let all = store::load_all();
    let now = crate::server::logging::now_ms();
    let mut stored = store::load_projects();
    for entry in all.iter().filter(|entry| entry.is_live()) {
        let slug = entry_project_slug(entry);
        if !stored.iter().any(|project| project.slug == slug) {
            stored.push(store::Project {
                slug: slug.clone(),
                name: if slug == store::GLOBAL_SLUG { "通用".to_string() } else { slug },
                remote: None,
                path: None,
                created_at: entry.created_at,
                updated_at: entry.updated_at,
                remotes: Vec::new(),
                merged_into: None,
                pinned: false,
                archived: false,
            });
        }
    }
    stored.sort_by(|a, b| b.pinned.cmp(&a.pinned).then(a.name.cmp(&b.name)));
    // 通用项目不进列表：它不是一张可维护的卡（名字固定、不可合并、不可归档），
    // 条数往往远大于任何具体项目，混在表里只会抢注意力。单独走 `general` 汇总。
    let items: Vec<Value> = stored
        .iter()
        .filter(|project| project.slug != store::GLOBAL_SLUG)
        .map(|project| {
            let mut entries: Vec<&MemoryEntry> = all
                .iter()
                .filter(|entry| entry.is_live())
                .filter(|entry| entry_project_slug(entry) == project.slug)
                .collect();
            entries.sort_by(|a, b| b.updated_at.cmp(&a.updated_at));
            let l1 = entries.iter().filter(|entry| entry.layer == Layer::L1).count();
            let l2 = entries.iter().filter(|entry| entry.layer == Layer::L2).count();
            let latest = entries.first().map(|entry| entry.updated_at).unwrap_or(project.updated_at);
            json!({
                "slug": project.slug,
                "name": project.name,
                "remote": project.remote,
                "remotes": project.remotes,
                "path": project.path,
                "mergedInto": project.merged_into,
                "pinned": project.pinned,
                "archived": project.archived,
                "count": entries.len(),
                "l1": l1,
                "l2": l2,
                "updatedAt": latest,
                "updatedText": search::relative_time(latest, now),
                "agents": project_agents(&entries),
                "latest": entries.iter().take(3).map(|entry| json!({
                    "id": entry.id,
                    "title": entry.title,
                    "type": entry.kind.as_str(),
                    "layer": entry.layer.as_str(),
                })).collect::<Vec<_>>(),
            })
        })
        .collect();
    // 通用项目的汇总：口径与列表里的 `count` 一致（同一个过滤条件），
    // 但单独抽出来给顶部统计条用 —— 通用不是一张可维护的卡，不该混进列表。
    let general_entries: Vec<&MemoryEntry> = all
        .iter()
        .filter(|entry| entry.is_live())
        .filter(|entry| entry_project_slug(entry) == store::GLOBAL_SLUG)
        .collect();
    let general = json!({
        "slug": store::GLOBAL_SLUG,
        "count": general_entries.len(),
        "l1": general_entries.iter().filter(|entry| entry.layer == Layer::L1).count(),
        "l2": general_entries.iter().filter(|entry| entry.layer == Layer::L2).count(),
        "latest": general_entries
            .iter()
            .map(|entry| entry.updated_at)
            .max()
            .unwrap_or(0),
        "agents": project_agents(&general_entries),
    });
    json!({ "items": items, "general": general })
}

/// 一条记忆归属的项目 slug。
///
/// 没有 `project` 字段的条目属于「通用」（`_global`）—— 早先这里直接拿
/// `Option::as_deref()` 比较，导致通用项目的计数恒为 0（`None != Some("_global")`）。
/// 归属判断只有一个口径，所有地方都必须走这个函数。
fn entry_project_slug(entry: &MemoryEntry) -> String {
    entry
        .project
        .as_deref()
        .map(store::slugify)
        .unwrap_or_else(|| store::GLOBAL_SLUG.to_string())
}

/// 项目下所有条目的写入方（去重、首次出现顺序）。
///
/// 顺序用**条目时间倒序下的首次出现序**：新写入的 Agent 排前面，比按字母排
/// 更符合「最近是谁在用这个项目」的直觉。
fn project_agents(entries: &[&MemoryEntry]) -> Vec<String> {
    let mut seen: Vec<String> = Vec::new();
    for entry in entries {
        let Some(agent) = entry.agent.as_deref().map(str::trim) else {
            continue;
        };
        if agent.is_empty() {
            continue;
        }
        if !seen.iter().any(|item| item == agent) {
            seen.push(agent.to_string());
        }
    }
    seen
}

/// 改一张项目卡：重命名 / 置顶 / 归档。
///
/// 只改登记表（`projects.json`），不动记忆文件 —— 项目名是展示名，slug 才是
/// 目录名。改 slug 等于搬家，那是 [`merge_project`] 的事，且必须逐条重写
/// frontmatter 与路径，风险高一个量级。
pub fn update_project(slug: &str, patch: &Value) -> Result<Value, String> {
    let slug = store::slugify(slug);
    // 读整份登记表 → 改一张卡 → 整份写回（见 `store::lock_projects` 的说明）
    let _guard = store::lock_projects();
    // 变量不能叫 `projects`：会和同模块的 `projects()` 函数互相遮蔽（E0618）
    let mut cards = store::load_projects();
    let Some(project) = cards.iter_mut().find(|item| item.slug == slug) else {
        return Err(format!("找不到项目 {slug}"));
    };
    if let Some(name) = patch.get("name").and_then(Value::as_str) {
        let name = name.trim();
        if !name.is_empty() {
            project.name = name.to_string();
        }
    }
    if let Some(pinned) = patch.get("pinned").and_then(Value::as_bool) {
        project.pinned = pinned;
    }
    if let Some(archived) = patch.get("archived").and_then(Value::as_bool) {
        project.archived = archived;
    }
    project.updated_at = crate::server::logging::now_ms();
    store::save_projects(&cards)?;
    bump_revision();
    let refreshed = projects();
    Ok(json!({ "slug": slug, "items": refreshed["items"], "general": refreshed["general"] }))
}

/// 把 `from` 合并进 `into`：改写所有记忆的 `project` 字段与落盘路径，
/// 旧卡标记 `merged_into` 后从列表里隐藏（但保留记录，合并可追溯）。
///
/// 为什么不是「删掉旧卡、改数据库字段」：记忆的事实源是 `.md` 文件，frontmatter
/// 里的 `project` 与文件所在的目录必须一致，否则下次全量扫描会把它们重新归到
/// 旧项目。所以这里逐条 `save`（会写新路径并删旧文件），不是批量 UPDATE。
pub fn merge_project(from: &str, into: &str) -> Result<Value, String> {
    let from_slug = store::slugify(from);
    let into_slug = store::slugify(into);
    if from_slug == into_slug {
        return Err("不能合并到自己".into());
    }
    if from_slug == store::GLOBAL_SLUG {
        return Err("通用项目不能合并".into());
    }
    // 登记表要「读 → 改写 → 整份写回」，与并发的 `registry_upsert` 互斥。
    // 顺序固定：先登记表锁，后（`store::save` 内部的）容器锁 —— 见 `store`
    // 里两把锁的说明，反向持有会自锁死。
    let _guard = store::lock_projects();
    let mut cards = store::load_projects();
    if !cards.iter().any(|item| item.slug == into_slug) {
        return Err(format!("目标项目 {into_slug} 不存在"));
    }
    let mut moved = 0usize;
    for mut entry in store::load_all() {
        let Some(project) = entry.project.clone() else { continue };
        if store::slugify(&project) != from_slug {
            continue;
        }
        // 旧对象留一份：`move_save` 要按它算「活形态的旧位置」。原来这里是
        // 「save 一次 + 手动 remove_file 一次」两个临界区，中间可能被并发写插队；
        // 而且手动删除那步特意跳过了容器路径，于是旧容器里留下一条孤儿节。
        let previous = entry.clone();
        entry.project = Some(into_slug.clone());
        // 清空 path 让 `entry_path` 按新项目重算 —— 否则它仍指向旧目录
        entry.path = String::new();
        entry.path = store::rel(&store::entry_path(&entry, false));
        store::move_save(&previous, &entry)?;
        let _ = index::global().upsert(&entry);
        moved += 1;
    }
    for project in cards.iter_mut() {
        if project.slug == from_slug {
            project.merged_into = Some(into_slug.clone());
            project.updated_at = crate::server::logging::now_ms();
        }
    }
    store::save_projects(&cards)?;
    bump_revision();
    crate::server::logging::log(
        "[Memory]",
        &format!("项目合并：{from_slug} → {into_slug}，迁移 {moved} 条"),
    );
    let refreshed = projects();
    Ok(json!({ "moved": moved, "items": refreshed["items"], "general": refreshed["general"] }))
}

/// 画像页视图：四类画像 + 生成门槛是否已达 + 上一版生成时间 + 存储与同步状态。
///
/// 为什么按 `sub`（`tags[0]`）分类而不是按标题关键词：模型写出来的标题措辞每次
/// 都不一样，而 `sub` 是我们在 prompt 里固定要求输出的四个枚举值之一，稳定。
///
/// ── 为什么这里连「持久缓存 / WebDAV 同步」一起回 ──────────────
/// 画像页顶部的三枚状态徽标里有两枚讲的是**这份画像存得靠不靠得住**：内容落在
/// 本地 `.md`（重启、断网、换模型都不丢）、而且开了同步（换机器还在）。它们与
/// 「上次处理 N 天前」「素材 M 条」同属「你现在看到的这四类结论是可不可信」的
/// 判据，放在同一份响应里才不会出现「徽标说同步保护中、同步页说没配」。
///
/// ── `material` 与 `total` 的区别 ────────────────────────────
/// `total` = 库里活着的记忆条数（门槛判据）；`material` = **真正会喂给模型**的
/// 素材条数（重要度排序后截到 `deep.batchSize`）。界面上写「记忆素材 N 条」时
/// 用户以为那是「模型看过的东西」，所以给 material —— 给 total 会在素材被截断时
/// 撒一个无害但会让人误判「为什么结论没覆盖到某条记忆」的谎。
pub fn profile_view() -> Value {
    ensure_index_once();
    let all = store::load_all();
    let live: Vec<&MemoryEntry> = all.iter().filter(|entry| entry.is_live()).collect();
    let profiles: Vec<&MemoryEntry> = live
        .iter()
        .copied()
        .filter(|entry| entry.kind == MemoryKind::Profile)
        .collect();
    let now = crate::server::logging::now_ms();
    let min = config::usize_of("deep.personaMinMemories").max(5);
    let newest = profiles.iter().map(|entry| entry.updated_at).max().unwrap_or(0);
    let fresh = live.iter().filter(|entry| entry.updated_at > newest).count();
    let material = live
        .iter()
        .filter(|entry| entry.kind != MemoryKind::Profile)
        .count()
        .min(config::usize_of("deep.batchSize").max(20).min(200));
    // 只数**用户钉的**：上一版给每条画像都设了 `pinned = true`，直接数 `pinned`
    // 会让升级后的老库显示「24 条已置顶」——那个数字既不是用户钉的，也保护不了
    // 任何东西（真正被保护的是带 `PROFILE_PIN_TAG` 的那些）。
    let pinned = profiles
        .iter()
        .filter(|entry| entry.pinned && entry.tags.iter().any(|tag| tag == PROFILE_PIN_TAG))
        .count();

    let groups: Vec<Value> = PROFILE_SUBS
        .iter()
        .copied()
        .map(|(sub, label, hint)| {
            // 旧 key 的条目也要出现在新分组里（见 `profile_sub` 的说明）。
            let mut items: Vec<Value> = profiles
                .iter()
                .filter(|entry| {
                    profile_sub(entry.tags.first().map(String::as_str).unwrap_or("")) == sub
                })
                .map(|entry| {
                    let mut value = entry.to_json(true);
                    value["evidenceCount"] = json!(entry.evidence.len());
                    // `sub` 回填成**归一之后的 key**：前端用它做「单独生成」的参数，
                    // 直接拿 `tags[0]` 会把旧 key 再发回来，后端又得猜一遍。
                    value["sub"] = json!(sub);
                    // `pinned` 也归一成「用户钉的」：老库里每条画像都带 `pinned=true`，
                    // 原样下发会让整页挂满「置顶」签，而那个签按下去又什么都不会发生
                    // （它本来就已经是 true 了）。归一后界面上的签 = 真正会被保护的条目。
                    value["pinned"] = json!(entry.pinned && entry.tags.iter().any(|tag| tag == PROFILE_PIN_TAG));
                    value
                })
                .collect();
            // 置顶的先排，其余按更新时间倒序 —— 与 `projects()` 的排序口径一致。
            items.sort_by(|left, right| {
                let pin = |value: &Value| value.get("pinned").and_then(Value::as_bool).unwrap_or(false);
                pin(right)
                    .cmp(&pin(left))
                    .then_with(|| {
                        let at = |value: &Value| value.get("updatedAt").and_then(Value::as_i64).unwrap_or(0);
                        at(right).cmp(&at(left))
                    })
            });
            json!({
                "sub": sub,
                "label": label,
                "hint": hint,
                "items": items,
            })
        })
        .collect();

    // 持久缓存：事实源是本地 `.md`，只要根目录可写就算「存得住」。这里不探写权限
    // （一次探针会在用户的记忆目录里留下垃圾文件），改为回根目录让界面显示位置。
    let storage_root = store::root().to_string_lossy().to_string();
    // 用 `summary()` 而不是 `status()`：后者会跑 `local_manifest()` 扫全目录，
    // 而这份视图每次刷新画像页都会被请求一次（见 `sync::summary` 的说明）。
    //
    // 变量不叫 `sync`：局部绑定会遮住同名的 `sync` 模块，之后任何 `sync::xxx`
    // 都会变成 `E0618` 之类的怪错（这条坑在 `agents.rs` 踩过两次）。
    let sync_state = sync::summary();
    json!({
        "groups": groups,
        "count": profiles.len(),
        "total": live.len(),
        "material": material,
        "pinned": pinned,
        "minMemories": min,
        "ready": live.len() >= min,
        "freshSinceProfile": fresh,
        "generatedAt": if newest > 0 { json!(newest) } else { Value::Null },
        "generatedText": if newest > 0 { json!(search::relative_time(newest, now)) } else { Value::Null },
        "enabled": config::bool_of("deep.enabled") && config::bool_of("deep.personaEnabled"),
        "evidenceRequired": config::bool_of("deep.evidenceRequired"),
        // 顶部两枚徽标的判据：存在哪、同步开没开。
        "storage": {
            "root": storage_root,
            "persistent": true,
        },
        // `summary()` 已经是这四个键了，直接嵌进去：再逐键 `get` 一遍只是把
        // 同一份结构抄第二次，且多一处「新加字段忘了转发」的机会。
        "sync": sync_state,
    })
}

/// 自检：文件根、条数、索引状态、回收站、队列积压、表行数
pub fn status() -> Value {
    let entries = store::load_all();
    let live = entries.iter().filter(|entry| entry.is_live()).count();
    let deleted = entries.iter().filter(|entry| entry.deleted_at.is_some()).count();
    let superseded = entries
        .iter()
        .filter(|entry| entry.superseded_by.is_some() && entry.deleted_at.is_none())
        .count();
    let l1 = entries.iter().filter(|entry| entry.is_live() && entry.layer == Layer::L1).count();
    let l2 = entries.iter().filter(|entry| entry.is_live() && entry.layer == Layer::L2).count();
    let index_count = index::global().count();
    json!({
        "root": store::root().to_string_lossy(),
        "indexFile": store::index_file().to_string_lossy(),
        "device": store::device_id(),
        "total": entries.len(),
        "live": live,
        "l1": l1,
        "l2": l2,
        "deleted": deleted,
        "superseded": superseded,
        "projects": projects().get("items").map(|items| items.as_array().map(Vec::len).unwrap_or(0)).unwrap_or(0),
        "review": {
            "pending": index::global().review_count(),
            "warnThreshold": config::usize_of("dedup.pendingWarnThreshold"),
        },
        "index": { "count": index_count, "fts": index::global().fts_enabled(), "inSync": index_count == live },
        "tables": index::global().table_stats(),
        "config": config(),
        // 内容版本号：每次写入 +1。排障时用来判断「Agent 看到的是不是最新」。
        "revision": revision(),
    })
}

// ── 写 ───────────────────────────────────────────────────────

/// 写入请求（HTTP 与 MCP 共用）
#[derive(Debug, Default, Clone)]
pub struct WriteInput {
    pub content: String,
    pub title: Option<String>,
    pub kind: Option<String>,
    /// `l1` / `l2`。不传时按类别推断（`decision` / `knowledge` / `insight` /
    /// `profile` 进 `l2`，其余进 `l1`）
    pub layer: Option<String>,
    pub project: Option<String>,
    pub agent: Option<String>,
    pub tags: Vec<String>,
    pub importance: Option<f64>,
    pub files: Vec<String>,
    pub supersedes: Vec<String>,
    pub pinned: Option<bool>,
    pub starred: Option<bool>,
    pub cwd: Option<String>,
    /// 会话标识（同一次对话的多条 l1 共享）
    pub session: Option<String>,
    /// 事实失效时间（「这条只在某时点前成立」）
    pub valid_to: Option<i64>,
    /// 证据链锚点（画像条目用）
    pub evidence: Vec<String>,
}

/// 生成 id：`20261115-2213-a1b2c3`（日期 + 时分 + 内容哈希前 6 位）。
///
/// 为什么不用递增序号：记忆要能跟着仓库走、能在两台机器之间合并，序号必然撞。
/// 时间 + 内容哈希既人类可读，又不需要协调。
///
/// ── 为什么还要掺一个进程内的自增序号 ────────────────────────
/// 只用「毫秒时间 + 内容哈希」时，**同一毫秒内两次写入相同内容**会得到同一个
/// id，而 id 就是文件名 —— 第二次 `save` 会把第一条整个覆盖掉，是静默的数据
/// 丢失（CI 的 `id_is_time_prefixed_and_unique` 用两条相同内容把它逼出来过）。
/// 掺一个自增序数就把「同毫秒 + 同内容」拆开了，而时间与内容仍是主成分，
/// id 依旧人类可读。
fn make_id(content: &str) -> String {
    use std::hash::{Hash, Hasher};
    use std::sync::atomic::{AtomicU64, Ordering};
    static SEQUENCE: AtomicU64 = AtomicU64::new(0);
    let now = crate::server::logging::now_ms();
    let date = search::format_date(now).replace('-', "");
    let minutes = (now / 60_000).rem_euclid(24 * 60);
    let sequence = SEQUENCE.fetch_add(1, Ordering::Relaxed);
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    content.hash(&mut hasher);
    now.hash(&mut hasher);
    sequence.hash(&mut hasher);
    let digest = format!("{:06x}", hasher.finish() & 0xff_ffff);
    format!("{date}-{:02}{:02}-{digest}", minutes / 60, minutes % 60)
}

pub fn write(input: WriteInput) -> Result<Value, String> {
    write_inner(input, None)
}

/// 批量写入器（导入用）。
///
/// ── 为什么需要这一层 ────────────────────────────────────────
/// `write` 的精确去重要 `store::load_all()` 把整个记忆库读一遍再逐条比对。单条
/// 写入时这没问题（几百条记忆、几十毫秒），但导入一次是几千条：每条都全盘扫，
/// 复杂度直接变成 O(n²)，800 条笔记能让请求挂上几分钟 —— 而且扫的是同一批文件。
///
/// 所以这里在**构造时**把「现存 l1 的 hash」收成一个集合，之后每条只查集合；
/// 每写成功一条就把它的 hash 补进去，同一次导入里的重复也一并挡住。语义与
/// `write` 一致，只是把「每条约一次全盘读」换成「整批一次」。
///
/// 为什么是持久的写入器而不是一个 `write_batch(Vec<..>)` 函数：调用方要分批
/// 写入（每批之间让出线程），函数形态会让每批都重新扫一遍库 —— 分批本身又把
/// 全盘读带回来了。
pub struct BatchWriter {
    /// `None` = 去重关掉了，不做任何 hash 判断
    seen: Option<std::collections::HashSet<String>>,
}

impl BatchWriter {
    pub fn new() -> Self {
        let seen = if dedup_on() {
            Some(
                store::load_all()
                    .into_iter()
                    .filter(|entry| entry.is_live() && entry.layer == Layer::L1)
                    .map(|entry| entry.hash)
                    .filter(|hash| !hash.is_empty())
                    .collect(),
            )
        } else {
            None
        };
        BatchWriter { seen }
    }

    pub fn write(&mut self, input: WriteInput) -> Result<Value, String> {
        write_inner(input, self.seen.as_mut())
    }
}

impl Default for BatchWriter {
    fn default() -> Self {
        Self::new()
    }
}

/// 精确去重是否生效（两把开关都在 `dedup.enabled` / `dedup.l1.enabled` 上）。
fn dedup_on() -> bool {
    config::bool_of("dedup.enabled") && config::bool_of("dedup.l1.enabled")
}

fn write_inner(
    input: WriteInput,
    mut seen: Option<&mut std::collections::HashSet<String>>,
) -> Result<Value, String> {
    let content = input.content.trim().to_string();
    if content.is_empty() {
        return Err("正文不能为空".into());
    }
    store::ensure_layout()?;
    let now = crate::server::logging::now_ms();

    // ── 归类：显式项目名 > cwd 的 git 身份 > 模糊建议（进 general + 待确认）──
    let classified: Classified = match (input.project.as_deref(), input.cwd.as_deref()) {
        (Some(name), cwd) => layout::classify_explicit(name, cwd),
        (None, Some(cwd)) => layout::classify(cwd, input.agent.as_deref()),
        (None, None) => layout::classify("", None),
    };
    let project = classified
        .slug
        .clone()
        .filter(|slug| !slug.is_empty() && slug != store::GLOBAL_SLUG);

    let kind = MemoryKind::parse(input.kind.as_deref().unwrap_or("daily"));
    let layer = match input.layer.as_deref() {
        Some(raw) => Layer::parse(raw),
        None if kind.prefers_l2() => Layer::L2,
        None => Layer::L1,
    };
    let hash = model::content_hash(&content);

    // ── 去重：L1 精确哈希（同内容不写第二份）──
    if dedup_on() {
        // 批量通道：集合里已经有这个 hash 就说明库里或本批前面已经写过，
        // 不必再全盘读一次。返回的形状与单条路径一致（`duplicate: true`），
        // 只是拿不到 existing —— 调用方只看这个布尔。
        //
        // 只在**写成功之后**才把 hash 记进集合：先记后写的话，一次落盘失败
        // 会让后面真正该写的那条被误判成重复，导入就会静默少一条。
        if let Some(set) = seen.as_deref() {
            if set.contains(&hash) {
                return Ok(json!({
                    "id": "",
                    "entry": Value::Null,
                    "duplicate": true,
                    "superseded": 0,
                }));
            }
        } else if let Some(existing) = find_hash_duplicate(&hash) {
            crate::server::logging::log(
                "[Memory]",
                &format!("写入跳过：与 {} 内容完全相同（hash 去重）", existing.id),
            );
            return Ok(json!({
                "id": existing.id,
                "entry": existing.to_json(true),
                "duplicate": true,
                "superseded": 0,
            }));
        }
    }

    let id = make_id(&content);
    // 归类建议要用**最终 id** 作 target：早先这里又调了一次 `make_id`，
    // 拿到的 id 与真正落盘的那条不是同一个，于是「批准归类」永远找不到目标
    if categorized_pending(&classified) {
        index::global().enqueue(
            "classify",
            &id,
            &json!({
                "suggestion": classified.suggestion,
                "confidence": classified.confidence,
                "cwd": input.cwd,
            }),
            classified.confidence,
        );
    }
    let title = input
        .title
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| infer_title(&content));
    let agent = input.agent.clone().or_else(|| Some("manual".to_string()));
    let mut entry = MemoryEntry {
        id: id.clone(),
        title,
        kind,
        layer,
        project: project.clone(),
        agent: agent.clone(),
        device: Some(store::device_id()),
        session: input.session.clone(),
        tags: dedup(input.tags),
        importance: input.importance.unwrap_or(0.5).clamp(0.0, 1.0),
        files: dedup(input.files),
        supersedes: dedup(input.supersedes),
        pinned: input.pinned.unwrap_or(false),
        starred: input.starred.unwrap_or(false),
        created_at: now,
        updated_at: now,
        valid_from: now,
        valid_to: input.valid_to,
        superseded_by: None,
        deleted_at: None,
        dedup_status: DedupStatus::Unique,
        evidence: dedup(input.evidence),
        hash,
        access_count: 0,
        last_hit_at: None,
        summary_text: String::new(),
        body: content,
        path: String::new(),
    };
    entry.path = relative_path(&entry);
    // 容器（l1 daily）里追加一节时不动容器头的 project/agent，所以这里先写文件
    store::save(&entry)?;
    index::global().upsert(&entry)?;
    bump_revision();
    // 写成功了才把 hash 记进批量集合（见上面去重分支的注释）
    if let Some(set) = seen.as_deref_mut() {
        set.insert(entry.hash.clone());
    }

    // ── 被取代的旧记忆：标 superseded_by（不删 —— 演化链要能倒着走回去）──
    let mut marked = 0usize;
    if !entry.supersedes.is_empty() {
        // 只在真要标记时才扫全量：绝大多数写入不带 supersedes
        let all = store::load_all();
        for previous in &entry.supersedes {
            if let Some(mut old) = all.iter().find(|item| &item.id == previous).cloned() {
                if old.superseded_by.as_deref() == Some(id.as_str()) {
                    continue;
                }
                old.superseded_by = Some(id.clone());
                old.updated_at = now;
                old.path = relative_path(&old);
                store::save(&old)?;
                index::global().upsert(&old)?;
                index::global().link(&entry.id, &old.id, "relates");
                marked += 1;
            }
        }
    }

    // ── L2 近似去重：只产建议，不动数据。归类错了比不归类更糟，同理，
    //    判重判错了会 silently 合并两条不同的结论。`review.autoConfirmDedup`
    //    是唯一的例外，且它**只标取代、不物理删除**（旧条目仍可恢复）──
    if config::bool_of("dedup.enabled") && config::bool_of("dedup.l2.enabled") && layer == Layer::L2 {
        if let Some((candidate, score)) = near_duplicate(&entry) {
            if config::bool_of("review.autoConfirmDedup")
                && score >= config::number("dedup.l2.autoMergeThreshold")
            {
                if let Err(error) = apply_dedup(&entry.id, &candidate) {
                    crate::server::logging::log("[Memory]", &format!("⚠️ 自动采纳去重失败：{error}"));
                }
            } else {
                index::global().enqueue(
                    "dedup",
                    &entry.id,
                    &json!({ "candidate": candidate, "score": score }),
                    score,
                );
            }
        }
    }

    crate::server::logging::log(
        "[Memory]",
        &format!(
            "写入记忆 {id}（{}/{}, {} 字，取代 {marked} 条）",
            entry.layer.as_str(),
            entry.kind.label(),
            entry.body.chars().count()
        ),
    );
    Ok(json!({
        "id": id,
        "entry": entry.to_json(true),
        "superseded": marked,
        "classified": classified.to_json(),
        "duplicate": false,
    }))
}

/// 模糊归类的建议要不要进队列。`suggest` 才进 —— 其余结论都是确定的。
fn categorized_pending(classified: &Classified) -> bool {
    classified.origin == "suggest" && classified.suggestion.is_some() && !config::bool_of("review.autoConfirmClassify")
}

/// 标题兜底：第一段非空、非标题行；全篇都是标题时退回第一个标题。
fn infer_title(content: &str) -> String {
    content
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty() && !line.starts_with('#'))
        .map(|line| truncate_chars(line, 60))
        .or_else(|| {
            content
                .lines()
                .map(str::trim)
                .find(|line| !line.is_empty())
                .map(|line| truncate_chars(line.trim_start_matches('#').trim(), 60))
        })
        .unwrap_or_else(|| "未命名记忆".to_string())
}

/// 按 hash 找已存在的活条目（读文件确认，不只信索引）。
fn find_hash_duplicate(hash: &str) -> Option<MemoryEntry> {
    store::load_all()
        .into_iter()
        .filter(|entry| entry.is_live() && entry.hash == hash)
        // 只有 l1 走精确去重：l2 的两条结论可能一字不差但语境不同
        // （例如同一个决定在两个项目里各下了一次），合并会丢掉项目归属
        .find(|entry| entry.layer == Layer::L1)
}

/// L2 近似重复的候选：词面相似度（Dice + 编辑距离）超过 `dedup.l2.candidateThreshold`。
///
/// 为什么不用向量：这里不想为了「找相似标题」引入 embedding 模型与一次额外
/// 的模型调用；而沉淀结论的标题通常很短，词面相似度已经能抓住绝大多数重复。
/// 阈值之上只产建议（`review_queue`），裁决由人做。
fn near_duplicate(entry: &MemoryEntry) -> Option<(String, f64)> {
    let threshold = config::number("dedup.l2.candidateThreshold");
    let w_dice = config::number("dedup.l2.wDice");
    let w_edit = config::number("dedup.l2.wEdit");
    let w_title = config::number("dedup.l2.wTitle");
    let mut best: Option<(String, f64)> = None;
    for other in store::load_all() {
        if other.id == entry.id || !other.is_live() || other.layer != Layer::L2 {
            continue;
        }
        if other.kind != entry.kind {
            continue;
        }
        let dice = dice_coefficient(&entry.body, &other.body);
        let chars = entry.body.chars().count().max(other.body.chars().count()).max(1);
        let edit = 1.0 - (levenshtein(&entry.body, &other.body) as f64) / chars as f64;
        let title_score = similarity(&entry.title, &other.title);
        // 三项各算各的再加权：早先这里把 Dice 与编辑距离都算成了 `similarity()`
        // 的均值，等于同一个数用了两遍，权重配置（wDice / wEdit）实际没生效
        let score = (dice * w_dice + edit * w_edit + title_score * w_title)
            / (w_dice + w_edit + w_title).max(0.001);
        if score < threshold {
            continue;
        }
        if best.as_ref().map(|(_, current)| score > *current).unwrap_or(true) {
            best = Some((other.id, score));
        }
    }
    best
}

/// 词面相似度：字符二元组 Dice 系数与归一化编辑距离的均值。
fn similarity(left: &str, right: &str) -> f64 {
    let a = index::normalize(left);
    let b = index::normalize(right);
    if a.is_empty() || b.is_empty() {
        return 0.0;
    }
    if a == b {
        return 1.0;
    }
    let dice = dice_coefficient(&a, &b);
    let edit = 1.0 - (levenshtein(&a, &b) as f64) / a.chars().count().max(b.chars().count()) as f64;
    ((dice + edit) / 2.0).clamp(0.0, 1.0)
}

fn dice_coefficient(left: &str, right: &str) -> f64 {
    let bigrams = |text: &str| -> Vec<String> {
        let chars: Vec<char> = text.chars().collect();
        if chars.len() < 2 {
            return vec![text.to_string()];
        }
        chars.windows(2).map(|pair| pair.iter().collect()).collect()
    };
    let a = bigrams(left);
    let b = bigrams(right);
    if a.is_empty() || b.is_empty() {
        return 0.0;
    }
    let mut used = vec![false; b.len()];
    let mut hits = 0usize;
    for item in &a {
        if let Some(index) = b.iter().enumerate().find(|(index, other)| !used[*index] && other == &item).map(|(index, _)| index) {
            used[index] = true;
            hits += 1;
        }
    }
    2.0 * hits as f64 / (a.len() + b.len()) as f64
}

fn levenshtein(left: &str, right: &str) -> usize {
    let a: Vec<char> = left.chars().collect();
    let b: Vec<char> = right.chars().collect();
    if a.is_empty() {
        return b.len();
    }
    if b.is_empty() {
        return a.len();
    }
    let mut previous: Vec<usize> = (0..=b.len()).collect();
    let mut current = vec![0usize; b.len() + 1];
    for i in 1..=a.len() {
        current[0] = i;
        for j in 1..=b.len() {
            let cost = if a[i - 1] == b[j - 1] { 0 } else { 1 };
            current[j] = (previous[j] + 1).min(current[j - 1] + 1).min(previous[j - 1] + cost);
        }
        std::mem::swap(&mut previous, &mut current);
    }
    previous[b.len()]
}

fn relative_path(entry: &MemoryEntry) -> String {
    let deleted = entry.deleted_at.is_some();
    let absolute = store::entry_path(entry, deleted);
    store::rel(&absolute)
}

fn dedup(items: Vec<String>) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for item in items {
        let trimmed = item.trim().to_string();
        if !trimmed.is_empty() && !out.contains(&trimmed) {
            out.push(trimmed);
        }
    }
    out
}

/// 更新（标题/正文/标签/重要度/星标/置顶/层/有效期/证据）
pub fn update(id: &str, patch: &Value) -> Result<Value, String> {
    let all = store::load_all();
    let mut entry = all
        .into_iter()
        .find(|entry| entry.id == id)
        .ok_or_else(|| format!("找不到记忆 {id}"))?;
    if let Some(title) = patch.get("title").and_then(Value::as_str) {
        if !title.trim().is_empty() {
            entry.title = title.trim().to_string();
        }
    }
    if let Some(body) = patch.get("body").and_then(Value::as_str) {
        if !body.trim().is_empty() {
            entry.body = body.to_string();
            entry.hash = model::content_hash(&entry.body);
        }
    }
    if let Some(kind) = patch.get("type").and_then(Value::as_str) {
        entry.kind = MemoryKind::parse(kind);
    }
    if let Some(layer) = patch.get("layer").and_then(Value::as_str) {
        entry.layer = Layer::parse(layer);
    }
    if let Some(tags) = patch.get("tags").and_then(Value::as_array) {
        entry.tags = dedup(tags.iter().filter_map(|tag| tag.as_str().map(str::to_string)).collect());
    }
    if let Some(importance) = patch.get("importance").and_then(Value::as_f64) {
        entry.importance = importance.clamp(0.0, 1.0);
    }
    if let Some(starred) = patch.get("starred").and_then(Value::as_bool) {
        entry.starred = starred;
    }
    if let Some(pinned) = patch.get("pinned").and_then(Value::as_bool) {
        entry.pinned = pinned;
        // 画像是「编译产物」，重生成会整批替换 —— 用户钉住一条时说的事实是
        // 「这句别被模型洗掉」。所以置顶要在 `tags` 里留一个可判定的标记，
        // 否则下一次 `build_profile` 分不出「用户钉的」与「上一版批量钉的」。
        // 见 `PROFILE_PIN_TAG` 的说明。
        if entry.kind == MemoryKind::Profile {
            if pinned {
                if !entry.tags.iter().any(|tag| tag == PROFILE_PIN_TAG) {
                    entry.tags.push(PROFILE_PIN_TAG.to_string());
                }
            } else {
                entry.tags.retain(|tag| tag != PROFILE_PIN_TAG);
            }
        }
    }
    if let Some(project) = patch.get("project").and_then(Value::as_str) {
        let slug = store::slugify(project);
        entry.project = if slug == store::GLOBAL_SLUG { None } else { Some(slug) };
    }
    // `validTo: 0` / `null` 都表示「取消有效期」
    if let Some(value) = patch.get("validTo") {
        entry.valid_to = value.as_i64().filter(|stamp| *stamp > 0);
    }
    if let Some(evidence) = patch.get("evidence").and_then(Value::as_array) {
        entry.evidence = dedup(
            evidence
                .iter()
                .filter_map(|item| item.as_str().map(str::to_string))
                .collect(),
        );
    }
    entry.updated_at = crate::server::logging::now_ms();
    // 换层 / 换类型 / 换项目 / 换日期都会换目录。旧形态可能是「容器里的一节」，
    // 也可能是独立的 `.md`，只有 move_save 两种都认。
    //
    // 为什么不是「先 detach_live、再 save」两次调用：那是两个临界区，中间插进来
    // 的并发写入会让同一条记忆出现两份（见 `store::move_save` 的说明）。
    let previous = entry.clone();
    entry.path.clear();
    entry.path = store::rel(&store::entry_path(&entry, false));
    store::move_save(&previous, &entry)?;
    index::global().upsert(&entry)?;
    bump_revision();
    Ok(json!({ "entry": entry.to_json(true) }))
}

/// 删除（进回收站）。文件搬 `trash/`，frontmatter 写 `deleted_at`。
pub fn remove(id: &str) -> Result<Value, String> {
    let all = store::load_all();
    let mut entry = all
        .into_iter()
        .find(|entry| entry.id == id)
        .ok_or_else(|| format!("找不到记忆 {id}"))?;
    if entry.deleted_at.is_some() {
        return Ok(json!({ "id": id, "already": true }));
    }
    // 「摘活形态」与「写回收站文件」必须是一个临界区：分两次做的话，中间插进来
    // 的并发 `save` 会把这条记忆又写到活位置，删除于是留下「活的 + 回收站」两份。
    //
    // 为什么把**改动前**的对象一起传下去：`detach_live` 按「活着的路径」找文件，
    // 而这里 `entry.path` 已经改成 `trash/<id>.md` 了；靠 layer/kind 重算会把
    // v1 那种「独立单文件」的 l1 记忆算成容器路径，原文件既没删掉、又写了一份
    // 进回收站 —— 那是数据损坏。
    let previous = entry.clone();
    entry.deleted_at = Some(crate::server::logging::now_ms());
    entry.updated_at = entry.deleted_at.unwrap_or(entry.updated_at);
    entry.path = relative_path(&entry);
    store::retire(&previous, &entry)?;
    index::global().upsert(&entry)?;
    bump_revision();
    Ok(json!({ "id": id, "trashed": true }))
}

/// 从回收站恢复
pub fn restore(id: &str) -> Result<Value, String> {
    let all = store::load_all();
    let mut entry = all
        .into_iter()
        .find(|entry| entry.id == id)
        .ok_or_else(|| format!("找不到记忆 {id}"))?;
    if entry.deleted_at.is_none() {
        return Ok(json!({ "id": id, "already": true }));
    }
    let previous = store::entry_path(&entry, true);
    entry.deleted_at = None;
    entry.updated_at = crate::server::logging::now_ms();
    entry.path = relative_path(&entry);
    // 「写回活位置」与「删掉回收站那份」必须是一个临界区：分两次做的话中间会有
    // 一瞬间同 id 两份都在，并发的 restore 会当成「已经活了」直接返回、并发的
    // purge 会把刚写出来的活文件一起删掉（见 `store::revive`）。
    store::revive(&previous, &entry)?;
    index::global().upsert(&entry)?;
    bump_revision();
    Ok(json!({ "id": id, "restored": true }))
}

/// 彻底删除（不进回收站）。仅供「回收站里再删一次」。
pub fn purge(id: &str) -> Result<Value, String> {
    let all = store::load_all();
    let entry = all
        .into_iter()
        .find(|entry| entry.id == id)
        .ok_or_else(|| format!("找不到记忆 {id}"))?;
    store::remove_file(&entry)?;
    index::global().drop_one(id);
    bump_revision();
    Ok(json!({ "id": id, "purged": true }))
}

/// 清空回收站（`storage.trashKeepDays` 到期的清理任务也走它）。
pub fn purge_trash() -> Value {
    let days = config::usize_of("storage.trashKeepDays").max(1);
    let removed = store::purge_expired_trash(days);
    bump_revision();
    json!({ "removed": removed, "keepDays": days })
}

/// 全库去重巡检：扫一遍存量记忆，找出**已经重复但没被处理**的那些。
///
/// ── 与写入时的去重差在哪 ────────────────────────────────────
/// 写入时 `find_hash_duplicate`（l1 精确哈希）与 `near_duplicate`（l2 词面近似）
/// 只在**新条目落盘那一刻**看一眼当下库里的东西。历史数据是分批导进来的、
/// 中间可能关过去重、也可能换过归一化强度 —— 所以「当时没判成重复」的那些
/// 会一直留在库里，越导越多。这个巡检就是补那一步。
///
/// ── 为什么只产建议不自动合并 ────────────────────────────────
/// 判重判错的代价是**静默丢掉一条不同的结论**，而且用户不会知道。所以这里只
/// 往 `review_queue` 塞 `dedup` 建议，由人在浏览页逐条点头。`dedup.l2.autoMergeThreshold`
/// 这条自动通道仍然保留给写入路径 —— 那里两边都是「刚写下来的」，语境更近。
///
/// ── 为什么先按 hash 分组再进近似判定 ────────────────────────
/// 精确重复（同一段话导了两遍）占绝大多数，且 O(n) 就能抓干净。先分组把这一
/// 大批一次性解决掉，剩下的才进 O(n²) 的词面比较 —— 否则每次巡检都要对所有
/// 活条目两两算 Dice + Levenshtein，库上千条就会卡住整个进程。
pub fn dedup_scan() -> Value {
    if !config::bool_of("dedup.enabled") {
        return json!({
            "skipped": true,
            "reason": "去重总开关关着（dedup.enabled），巡检不跑。",
            "scanned": 0, "exact": 0, "near": 0, "queued": 0,
        });
    }
    let live: Vec<MemoryEntry> = store::load_all()
        .into_iter()
        .filter(|entry| entry.is_live())
        .collect();
    let scanned = live.len();

    // ── 第一轮：精确 hash 分组 ────────────────────────────────
    // 每组保留**最新**的那条（它是用户最可能还在引用的一条），其余作为候选。
    let mut by_hash: std::collections::HashMap<String, Vec<&MemoryEntry>> =
        std::collections::HashMap::new();
    for entry in &live {
        if entry.hash.is_empty() || entry.layer != Layer::L1 {
            continue;
        }
        by_hash.entry(entry.hash.clone()).or_default().push(entry);
    }

    let mut queued = 0usize;
    let mut exact = 0usize;
    for (_hash, group) in &by_hash {
        if group.len() < 2 {
            continue;
        }
        let mut sorted = group.clone();
        sorted.sort_by_key(|entry| std::cmp::Reverse(entry.updated_at));
        let keep = sorted[0];
        for other in sorted.iter().skip(1) {
            exact += 1;
            // 载荷形状与写入路径一致（`candidate` + `score`），
            // 这样 `review_resolve` 的 `dedup` 分支不用为巡检再加一条路径。
            index::global().enqueue(
                "dedup",
                &other.id,
                &json!({ "candidate": keep.id, "score": 1.0, "foundBy": "scan" }),
                1.0,
            );
            queued += 1;
        }
    }

    // ── 第二轮：l2 词面近似 ───────────────────────────────────
    // 只对 l2 跑：l1 流水里「同一个决定说过两遍」很常见，合并它会丢掉时间线。
    let mut near = 0usize;
    if config::bool_of("dedup.l2.enabled") {
        // 权重与阈值都走配置：巡检与写入路径必须用**同一条判据**，否则
        // 「写入时没判成重复、巡检却判成」这种不一致会让用户怀疑两边都在乱来。
        let w_dice = config::number("dedup.l2.wDice");
        let w_edit = config::number("dedup.l2.wEdit");
        let w_title = config::number("dedup.l2.wTitle");
        let weight_sum = (w_dice + w_edit + w_title).max(0.001);
        let threshold = config::number("dedup.l2.candidateThreshold");
        let l2: Vec<&MemoryEntry> = live.iter().filter(|entry| entry.layer == Layer::L2).collect();
        // 上限 400 条：两两比较是 O(n²)，400 条 = 8 万次比较，几秒钟的量级；
        // 再多就该让用户分批跑，而不是让请求挂十分钟。
        let take = l2.len().min(400);
        for i in 0..take {
            for j in (i + 1)..take {
                if l2[i].kind != l2[j].kind {
                    continue;
                }
                let dice = dice_coefficient(&l2[i].body, &l2[j].body);
                let chars = l2[i].body.chars().count().max(l2[j].body.chars().count()).max(1);
                let edit = 1.0 - (levenshtein(&l2[i].body, &l2[j].body) as f64) / chars as f64;
                let title = similarity(&l2[i].title, &l2[j].title);
                let score = (dice * w_dice + edit * w_edit + title * w_title) / weight_sum;
                if score < threshold {
                    continue;
                }
                // 保留更早的那条（沉淀结论通常越早越是被引用的那次），新的去候选
                let (keep, other) = if l2[i].created_at <= l2[j].created_at {
                    (l2[i], l2[j])
                } else {
                    (l2[j], l2[i])
                };
                index::global().enqueue(
                    "dedup",
                    &other.id,
                    &json!({ "candidate": keep.id, "score": score, "foundBy": "scan" }),
                    score,
                );
                near += 1;
                queued += 1;
            }
        }
    }

    bump_revision();
    crate::server::logging::log(
        "[Memory]",
        &format!("全库去重巡检：扫 {scanned} 条，精确重复 {exact}，近似 {near}"),
    );
    json!({
        "skipped": false,
        "scanned": scanned,
        "exact": exact,
        "near": near,
        "queued": queued,
        "pending": index::global().review_count(),
    })
}

// ── 待确认队列 ───────────────────────────────────────────────

pub fn review_list(status: Option<&str>, kind: Option<&str>, limit: usize) -> Value {
    let rows = index::global().review_list(status, kind, limit);
    let now = crate::server::logging::now_ms();
    let items: Vec<Value> = rows
        .iter()
        .map(|row| {
            let mut value = row.to_json();
            value["createdText"] = Value::String(search::relative_time(row.created_at, now));
            value
        })
        .collect();
    json!({
        "items": items,
        "total": items.len(),
        "pending": index::global().review_count(),
    })
}

/// 裁决一条建议。`approve` 为真时按建议内容落盘：
///
/// * `classify` —— 把记忆搬进建议的项目
/// * `dedup` —— 把新条目取代旧条目（旧条目进回收站，可恢复）
/// * `supersede` —— 标记旧事实失效
/// * `conflict` —— 保留当前版本，清掉冲突记录
///
/// 拒绝只改队列状态，不动数据。**永不物理删除**：任何裁决都可以反悔。
pub fn review_resolve(id: i64, approve: bool) -> Result<Value, String> {
    let rows = index::global().review_list(None, None, 500);
    let row = rows
        .into_iter()
        .find(|row| row.id == id)
        .ok_or_else(|| format!("找不到待确认项 {id}"))?;
    if row.status != "pending" {
        return Ok(json!({ "id": id, "status": row.status, "already": true }));
    }
    if approve {
        match row.kind.as_str() {
            "classify" => {
                let slug = row
                    .payload
                    .get("suggestion")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                if !slug.is_empty() {
                    apply_classification(&row.target, slug)?;
                }
            }
            "dedup" => {
                let candidate = row
                    .payload
                    .get("candidate")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                if !candidate.is_empty() {
                    apply_dedup(&row.target, candidate)?;
                }
            }
            "supersede" => {
                // 载荷里两个 id 都带：`oldId` 是被取代的那条，`newId` 是取代者。
                // 早先这里只读一个叫 `superseded` 的键，而 `detect_supersede`
                // 写的是 `oldId` —— 于是「批准失效」永远什么都没做却照样标记成功。
                let old_id = row
                    .payload
                    .get("oldId")
                    .and_then(Value::as_str)
                    .or_else(|| row.payload.get("superseded").and_then(Value::as_str))
                    .unwrap_or_default();
                let new_id = row
                    .payload
                    .get("newId")
                    .and_then(Value::as_str)
                    .unwrap_or(&row.target);
                if !old_id.is_empty() {
                    apply_supersede(new_id, old_id)?;
                }
            }
            "distill" | "consolidate" => apply_derived(&row.payload)?,
            _ => {}
        }
    }
    index::global().review_resolve(&[id], if approve { "approved" } else { "rejected" }, Some(if approve { "manual" } else { "manual-reject" }));
    bump_revision();
    Ok(json!({ "id": id, "approved": approve, "type": row.kind }))
}

/// 批量拒绝（「全部忽略」）。不批量批准：一批建议里几乎必然有一条是错的，
/// 而错的那条会把记忆搬进错误项目，代价远高于逐条点多几下。
pub fn review_reject_all(kind: Option<&str>) -> Value {
    let changed = index::global().review_reject_all(kind);
    bump_revision();
    json!({ "rejected": changed })
}

fn apply_classification(memory_id: &str, slug: &str) -> Result<(), String> {
    let mut entry = store::load_all()
        .into_iter()
        .find(|entry| entry.id == memory_id)
        .ok_or_else(|| format!("找不到记忆 {memory_id}"))?;
    let before = entry.clone();
    let slug = store::slugify(slug);
    entry.project = if slug == store::GLOBAL_SLUG { None } else { Some(slug) };
    entry.updated_at = crate::server::logging::now_ms();
    entry.path.clear();
    let next_path = store::entry_path(&entry, false);
    entry.path = store::rel(&next_path);
    // 归档会把记忆从 `general/` 搬进 `projects/<slug>/`。「摘旧 + 写新」必须在
    // 一个临界区里（见 `store::move_save`）：分成两次调用的话，中间插进来一次
    // 并发写入就会留下同 id 两份文件。
    store::move_save(&before, &entry)?;
    index::global().upsert(&entry)?;
    Ok(())
}

fn apply_dedup(new_id: &str, old_id: &str) -> Result<(), String> {
    let all = store::load_all();
    let mut old = all
        .iter()
        .find(|entry| entry.id == old_id)
        .cloned()
        .ok_or_else(|| format!("找不到记忆 {old_id}"))?;
    if old.superseded_by.as_deref() == Some(new_id) {
        return Ok(());
    }
    old.superseded_by = Some(new_id.to_string());
    old.dedup_status = DedupStatus::Merged;
    old.updated_at = crate::server::logging::now_ms();
    store::save(&old)?;
    index::global().upsert(&old)?;
    index::global().link(new_id, old_id, "relates");
    Ok(())
}

fn apply_supersede(new_id: &str, old_id: &str) -> Result<(), String> {
    // 与 dedup 同一条路径：区别只在语义（「换了个说法」vs「事实变了」）
    apply_dedup(new_id, old_id)
}

/// 把一条「派生建议」（蒸馏 / 合并）落成真正的记忆条目。
///
/// 载荷形状由 `enrich::distill` / `schedule::consolidate` 产出。合并类载荷带
/// `sources`：落盘新条目后把来源逐条标记为「被这条取代」—— 不删除来源，
/// 演化链要能倒着走回去（这是全模块的一条硬约束）。
fn apply_derived(payload: &Value) -> Result<(), String> {
    let body = payload.get("body").and_then(Value::as_str).unwrap_or("").trim();
    if body.chars().count() < 20 {
        return Err("建议内容太短，无法落成记忆".into());
    }
    let sources: Vec<String> = payload
        .get("sources")
        .and_then(Value::as_array)
        .map(|items| items.iter().filter_map(|id| id.as_str().map(str::to_string)).collect())
        .unwrap_or_else(|| {
            payload
                .get("evidence")
                .and_then(Value::as_array)
                .map(|items| items.iter().filter_map(|id| id.as_str().map(str::to_string)).collect())
                .unwrap_or_default()
        });
    let input = WriteInput {
        content: body.to_string(),
        title: payload.get("title").and_then(Value::as_str).map(str::to_string),
        kind: payload.get("type").and_then(Value::as_str).map(str::to_string),
        layer: Some("l2".to_string()),
        project: payload.get("project").and_then(Value::as_str).map(str::to_string),
        agent: Some("review".to_string()),
        tags: Vec::new(),
        importance: payload.get("importance").and_then(Value::as_f64),
        files: Vec::new(),
        supersedes: sources.clone(),
        pinned: Some(false),
        starred: Some(false),
        cwd: None,
        session: None,
        valid_to: None,
        evidence: sources.clone(),
    };
    let written = write(input)?;
    let new_id = written.get("id").and_then(Value::as_str).unwrap_or_default().to_string();
    // `write()` 已经按 `supersedes` 标了旧条目的 `superseded_by`；这里只需补图边，
    // 让图扩散检索也能从新条目走到来源
    for source in &sources {
        index::global().link(&new_id, source, "derived");
    }
    Ok(())
}

/// 把一条记忆标记为「被另一条取代」。
///
/// 对外公开是给 `enrich::detect_supersede` 的自动应用路径用的（本地 LLM 判定
/// + 两个开关都打开时）。与 `apply_supersede` 的区别只在可见性，逻辑同一份 ——
/// 分两份实现迟早会出现「自动应用的历史链与人工批准的不一样」。
pub fn mark_superseded(old_id: &str, new_id: &str) -> Result<(), String> {
    apply_supersede(new_id, old_id)
}

// ── 三级披露的对外入口 ───────────────────────────────────────

/// 核心记忆文本（MCP `memory_core` 的产出；也供页面预览）
pub fn core_text(project: Option<&str>, max_tokens: usize) -> Value {
    ensure_index_once();
    let entries = store::load_all();
    let (text, used, truncated) = search::core_text(&entries, project, max_tokens);
    index::global().log_recall("core", project.unwrap_or(""), entries.len(), used, None, project);
    json!({
        "text": text,
        "tokens": used,
        "truncated": truncated,
        "totalTokens": model::estimate_tokens(&text),
    })
}

pub fn digest_text(max_lines: usize) -> Value {
    ensure_index_once();
    let entries = store::load_all();
    let text = search::digest_text(&entries, max_lines);
    json!({ "text": text, "lines": text.lines().count() })
}

// ── 体检：记忆可能过时 ───────────────────────────────────────

/// 记忆体检：找出「写这条时锚定的文件后来改过」的条目，外加队列与索引的一致性。
///
/// ── 为什么用「文件 mtime > 记忆 updated_at」这个判据 ─────────
/// 记忆里存的是**判断**（「这里要这么改，因为…」），而代码会继续变。当锚定文件
/// 在记忆写下之后又被修改，「这条判断基于的代码已经变了」—— 它不是错的，而是
/// **需要复核**。没有这个信号，模型会拿三个月前的判断去改刚重构过的文件。
///
/// 不做内容哈希比对：`files` 是用户手填的相对路径，路径在仓库里搬来搬去很常见，
/// 而 mtime 检查的目标只是「提个醒」，误报的代价（多看一眼）远低于漏报。
pub fn health(limit: usize) -> Value {
    ensure_index_once();
    let entries = store::load_all();
    let root = store::root();
    let limit = limit.clamp(1, 200);
    let mut stale: Vec<Value> = Vec::new();
    for entry in entries.iter().filter(|entry| entry.is_live()) {
        if entry.files.is_empty() {
            continue;
        }
        let mut touched: Vec<Value> = Vec::new();
        for file in &entry.files {
            // `files` 是相对路径，但相对的是**记忆根**还是**项目目录**？
            // 约定是相对记忆根（与 `path` 同口径），同时对「相对 cwd」的写法宽容：
            // 两种都试，任一命中就算改动过。
            let candidates = [root.join(file), root.join("projects").join(file)];
            for path in candidates {
                let Ok(meta) = std::fs::metadata(&path) else { continue };
                let Ok(modified) = meta.modified() else { continue };
                let Ok(age) = modified.duration_since(std::time::UNIX_EPOCH) else { continue };
                let stamp = age.as_millis() as i64;
                if stamp > entry.updated_at {
                    touched.push(json!({
                        "file": file,
                        "changedAt": stamp,
                        "changedText": search::relative_time(stamp, crate::server::logging::now_ms()),
                    }));
                    break;
                }
            }
        }
        if touched.is_empty() {
            continue;
        }
        stale.push(json!({
            "id": entry.id,
            "title": entry.title,
            "type": entry.kind.as_str(),
            "layer": entry.layer.as_str(),
            "updatedAt": entry.updated_at,
            "updatedText": search::relative_time(entry.updated_at, crate::server::logging::now_ms()),
            "files": touched,
        }));
        if stale.len() >= limit {
            break;
        }
    }
    let pending = index::global().review_count();
    let warn = config::usize_of("dedup.pendingWarnThreshold");
    json!({
        "stale": stale,
        "staleCount": stale.len(),
        "review": { "pending": pending, "warnThreshold": warn, "backlogged": pending >= warn },
        "index": {
            "count": index::global().count(),
            "inSync": index::global().count() == entries.iter().filter(|entry| entry.is_live()).count(),
            "fts": index::global().fts_enabled(),
        },
        "note": if stale.is_empty() {
            "没有发现可能过时的记忆。"
        } else {
            "这些记忆锚定的文件在记忆写下之后被改过：结论可能仍成立，但值得复核。"
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn id_is_time_prefixed_and_unique() {
        let a = make_id("同样的内容");
        let b = make_id("同样的内容");
        assert!(a.starts_with(&search::format_date(crate::server::logging::now_ms()).replace('-', "")));
        assert_ne!(a, b, "同一秒内两次写入也必须拿到不同 id");
    }

    #[test]
    fn config_patch_only_touches_given_keys() {
        // 走真实写盘路径会把测试机配置改掉，这里只验「未知键被丢弃」这条语义
        assert!(config::find("injectEnabled").is_none());
        assert!(config::find("agents.coreMaxTokens").is_some());
        assert_eq!(
            config::find("agents.coreMaxTokens").map(|knob| knob.kind),
            Some(config::Kind::Number)
        );
    }

    #[test]
    fn dedup_keeps_order_and_drops_blank() {
        let out = dedup(vec![" a ".into(), "a".into(), "".into(), "b".into()]);
        assert_eq!(out, vec!["a".to_string(), "b".to_string()]);
    }

    #[test]
    fn infer_title_skips_heading_but_falls_back_to_it() {
        assert_eq!(infer_title("# 标题\n\n正文第一段"), "正文第一段");
        assert_eq!(infer_title("# 只有标题"), "只有标题");
    }

    #[test]
    fn similarity_flags_same_sentence_rewrites() {
        let a = "用 npm ci 重装依赖，不要 rm -rf node_modules";
        let b = "用 npm ci 重装依赖，别 rm -rf node_modules";
        assert!(similarity(a, b) > 0.8, "近似改写应被判相似");
        assert!(similarity(a, "把端口固定到 3065") < 0.3);
    }

    #[test]
    fn dice_and_levenshtein_sanity() {
        assert!((dice_coefficient("abcd", "abcd") - 1.0).abs() < 1e-9);
        assert_eq!(levenshtein("kitten", "sitting"), 3);
    }
}