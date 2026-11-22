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
//! ── 记忆怎么进到模型的上下文里（两条路，各管一段）────────────
//! **1. MCP 工具（`tool_services::memory_mcp`）** —— 模型主动来问。
//! 深挖、翻历史、按条件筛，都走这条。缺点是依赖模型自觉：实测装了 MCP 好几天，
//! `memory_core` 被调 1 次、`memory_search` 0 次。只靠这条，记忆写了等于没写。
//!
//! **2. 请求级注入（`inject`）** —— 网关在转发前主动塞。只塞两段有硬预算的
//! 内容：「必须遵守」（`pinned` + 偏好 + 画像）与「相关经验」（命中本次请求里
//! 的**具体线索**——路径 / 标识符 / 命令名——的避坑与决策）。开关是
//! `inject.enabled`，默认开。
//!
//! 早先本文件这里写的是「记忆不经由网关注入 system prompt」，理由是「网关不知道
//! 这个请求属于哪个项目」。那个理由只对了一半：网关确实不知道项目，但它知道
//! **这次请求提到了哪些文件、哪些函数、哪条命令** —— 那比项目名精确得多。所以
//! 现在的判据不是「猜项目」，而是「只认字符串级命中，命不中就不注入」。宁可不给，
//! 也不给错的：错的记忆比没有记忆更贵。
//!
//! 注入的内容永远是「历史陈述」，块头带一句硬约束（与
//! `agents::INSTRUCTION_BLOCK` 同一条）：「不代表当前代码状态，与本次对话冲突时
//! 以本次为准，改文件前先读文件」。没有这句，模型会拿三周前的结论去「修复」
//! 刚改对的代码。

pub mod agents;
pub mod config;
pub mod enrich;
pub mod import;
pub mod import_registry;
pub mod index;
pub mod inject;
pub mod retain;
pub mod layout;
pub mod llm;
pub mod model;
pub mod redact;
pub mod rt;
pub mod schedule;
pub mod search;
pub mod store;
pub mod sync;

use std::sync::OnceLock;

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use layout::Classified;
use model::{information_score, truncate_chars, DedupStatus, Layer, MemoryEntry};
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
pub const TASK_LABELS: [(&str, &str); 12] = [
    ("extract", "抽取结构化信息"),
    ("summarize", "生成摘要"),
    ("tag", "自动打标签"),
    ("classify", "项目归类建议"),
    ("supersede", "失效判定"),
    ("distill", "L2 蒸馏"),
    ("consolidate", "去重合并"),
    ("profile", "人格 / 偏好画像"),
    ("ingest", "会话增量入库"),
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
    /// `memory_digest` 的默认行数上限（工具参数可逐个覆盖）
    ///
    /// 对应 `agents.digestMaxLines`。以前 MCP 与 HTTP 两处都把 200 写死在代码里，
    /// 设置页那条旋钮改了没反应。
    pub digest_max_lines: usize,
    /// `memory_search` 返回行的 token 预算（超了从尾部丢整行）
    ///
    /// 对应 `agents.searchMaxTokens`。工具参数里不暴露 —— 模型不该自己决定
    /// 「这次能占多少上下文」，那是调用方（客户端）该管的事。
    pub search_max_tokens: usize,
    /// `memory_search` 不传 `limit` 时的默认返回条数（`search.finalTopK`）
    pub final_top_k: usize,
    /// `memory_search` 打分前的候选上限（`search.recallTopK`）。
    ///
    /// 只给 MCP 用：界面列表必须拿真实总数算页数，截断候选会让第 N 页之后
    /// 永远空着。
    pub recall_top_k: usize,
}

impl Default for MemoryConfig {
    fn default() -> Self {
        MemoryConfig {
            enabled: true,
            core_max_tokens: 800,
            digest_max_lines: 200,
            search_max_tokens: 1200,
            final_top_k: 8,
            recall_top_k: 20,
        }
    }
}

/// 读模块设置（每次调用都是缓存命中，成本是一次 `metadata()`）。
pub fn config() -> MemoryConfig {
    MemoryConfig {
        enabled: config::bool_of(config::ENABLE_KEY),
        core_max_tokens: config::usize_of("agents.coreMaxTokens").clamp(200, 4000),
        digest_max_lines: config::usize_of("agents.digestMaxLines").clamp(50, 1000),
        search_max_tokens: config::usize_of("agents.searchMaxTokens").clamp(200, 4000),
        final_top_k: config::usize_of("search.finalTopK").clamp(1, 50),
        recall_top_k: config::usize_of("search.recallTopK").clamp(5, 200),
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

/// 进程内只做一次「事实源 vs 索引」的一致性检查：条数对不上就整表重建。
/// 索引是派生的，重建永远安全。
///
/// 为什么不做文件监听：写入都经过本模块（写文件的同时更新索引），外力改动
/// （用户手编 .md）只需要「下次启动 / 下次点重建」生效，这比一个跨平台的
/// 文件监听器简单得多，也不会在编辑器保存的瞬间读到半个文件。
///
/// ── 启动时先跑一次「容器归位」迁移 ──────────────────────────
/// 老版本 `entry_path` 按 `created_at.max(updated_at)` 算 l1 容器日期，历史
/// 导入于是把几十个日期的几千条全写进「今天」那个文件；文件涨到几十 MB 后
/// 再被单文件上限整份跳过，表现成「导入了 2341 条，普通记忆 0 条」。
///
/// 计算逻辑已改成按 `created_at`，但**已经写错的文件不会自己变对**：光让
/// `read_dir_entries` 读得进超大容器，只能让「条数」恢复，那些记忆的日期
/// 仍然是错的（热力图把所有历史都记在今天、按日期检索也全落在今天）。
/// 所以启动自检里补一次拆分。`split_misplaced_once` 自带完成标记，正常机器
/// 只在升级后的第一次启动付一次全量解析的代价，之后直接返回。
fn ensure_index_once() {
    static DONE: OnceLock<()> = OnceLock::new();
    DONE.get_or_init(|| {
        // 拆分会改 `entry.path`（同一节从老容器搬到新容器）。只按条数判要不要
        // 重建会漏掉这种情况：拆完总数没变（1780 条还是 1780 条），索引里
        // 的 path 却全指向已经不存在的文件 —— 点开一条记忆会报「正文读不到」，
        // 而条数上看一切正常。所以拆分真的搬了东西就必须重建。
        let mut must_rebuild = false;
        if let Some((moved, files, failed)) = store::split_misplaced_once() {
            if moved > 0 || failed > 0 {
                must_rebuild = moved > 0;
                crate::server::logging::log(
                    "[Memory]",
                    &format!(
                        "l1 容器归位：{moved} 条搬回各自日期（{files} 个文件）{}",
                        if failed > 0 {
                            format!("，{failed} 处写失败待下次启动重试")
                        } else {
                            String::new()
                        }
                    ),
                );
            }
        }
        let entries = store::load_all();
        let index = index::global();
        // ── 关联图被老口径撑爆时，清一次重算 ────────────────────
        // 老 `relink()` 是「同项目 + 任意共同标签就连边」：5818 条都带 `导入`
        // 标签的记忆两两相连，`mem_link` 于是涨到 810 万行、索引库 275 MB。
        // 这张表是派生的，清空重算永远安全，而带着它跑的代价是每次 `status()`
        // 都要 `COUNT(*)` 扫一遍（几百毫秒）—— 用户看到的就是首屏转圈。
        //
        // 判据用「边数 > 条目数 × 20」：正常口径下每条最多 5 条边（双向即 10），
        // 20 已经是宽松的上界。超了就是老数据或老逻辑留下的，清掉。
        let links = index.link_count();
        let link_ceiling = entries.len().saturating_mul(20).max(2000);
        let links_bloated = links > link_ceiling;
        if links_bloated {
            crate::server::logging::log(
                "[Memory]",
                &format!(
                    "关联图异常膨胀（{links} 条边 / {} 条记忆），清空后按新口径重算",
                    entries.len()
                ),
            );
            index.clear_links();
        }
        // ── 比的是「索引行数 vs 事实源总条数」 ──────────────────
        // 索引留着回收站与已取代的行（浏览页的回收站视图就读这些行），拿 `live`
        // 去比，只要有回收站条目就永远对不上 —— 每次启动都全量重建一遍，
        // 而重建写入的仍是同一批行，下次启动继续对不上。这是一个自我维持的
        // 无效重建循环，与 `status()` 里 `inSync` 的口径必须完全一致。
        if must_rebuild || links_bloated || index.count() != entries.len() {
            match index.rebuild(&entries) {
                Ok(count) => crate::server::logging::log(
                    "[Memory]",
                    &format!(
                        "记忆索引已重建：{count} 条（事实源 {} 条，其中有效 {} 条）",
                        entries.len(),
                        entries.iter().filter(|entry| entry.is_live()).count()
                    ),
                ),
                Err(error) => crate::server::logging::log(
                    "[Memory]",
                    &format!("⚠️ 记忆索引重建失败：{error}"),
                ),
            }
            // 重建时顺手把 id 之间的关联补齐（同项目 + 主题标签，见 `relink`）。
            // 必须先清空：`rebuild()` 不清 `mem_link`，叠加写会把表撑到千万行。
            index.clear_links();
            relink(&entries);
        }
    });
}

/// 补 `mem_link`：同项目且共享**主题标签**的活条目之间连线。
///
/// 为什么在重建时做而不是每次写入做：连线的成本是 O(同项目条目数)，写入路径
/// 上做会把「写一条记忆」从毫秒级拖到几十毫秒；而连线不是实时需求 —— 检索时
/// 差几分钟的关联，用户感知不到。
///
/// ── 2026-11-21：这里曾经把索引库撑到 800 万行 ────────────────
/// 老实现是「同项目 + 任意一个共同标签」就连一条边。看起来合理，实际是 O(n²)
/// 的完全图：历史导入给每条记忆都打了 `导入` 标签，5818 条同项目记忆于是两两
/// 相连 —— 5818 × 5817 / 2 ≈ **1692 万条边**，实测表里躺着 810 万行、库文件
/// 涨到 275 MB。后果不只是占盘：
///
///   * `neighbors()` 对每个种子查一次 `from_id` / `to_id` 双向，单种子命中
///     5779 个邻居；`search.graphExpansionDepth` 默认 1、上限 3，深度一开就是
///     5779 个种子各查一次 —— 一次检索要几十秒；
///   * `table_stats()` 里 `SELECT COUNT(*) FROM mem_link` 要扫 810 万行，
///     而它在 `status()` 里，**每次打开记忆中枢都跑一遍**；
///   * 图扩散本来要「补充几条相关记忆」，全连通之后它给每条记忆都加分，
///     等于没有信号 —— 功能还在，效果归零。
///
/// 现在的口径：
///   1. **不连「泛标签」**。`导入` / `daily` 这类标签描述的是「怎么进来的」，
///      不是「讲什么」，拿它当主题会把不相干的记忆串成一片。判据是「标签出现在
///      超过 `MEM_LINK_TAG_SHARE` 比例的条目上就当泛标签忽略」。
///   2. **每条记忆最多连 `MEM_LINK_FANOUT` 条**。关联图是给检索做「一跳提示」
///      用的，一条记忆有 5 个邻居和 5000 个邻居，对检索的边际价值一样。
///   3. **只按时间邻近连**（同项目内最近更新的若干条）。真正常被一起想起的记忆
///      是同一段时间写的，这比「共享一个词」准得多，而且成本是排序不是笛卡尔积。
fn relink(entries: &[MemoryEntry]) {
    let live: Vec<&MemoryEntry> = entries.iter().filter(|entry| entry.is_live()).collect();
    if live.is_empty() {
        return;
    }
    // ── 泛标签：出现在超过阈值的条目上就不当主题 ──────────────
    let mut counts: std::collections::HashMap<&str, usize> = std::collections::HashMap::new();
    for entry in &live {
        for tag in &entry.tags {
            *counts.entry(tag.as_str()).or_insert(0) += 1;
        }
    }
    let generic_floor = (live.len() as f64 * MEM_LINK_TAG_SHARE).ceil() as usize;
    let is_topic = |tag: &str| counts.get(tag).copied().unwrap_or(0) <= generic_floor;

    let mut links = 0usize;
    // 按项目分组（`None` 与 `Some(slug)` 各自一组，不跨项目连线）
    let mut groups: std::collections::HashMap<Option<&str>, Vec<&MemoryEntry>> =
        std::collections::HashMap::new();
    for entry in &live {
        groups.entry(entry.project.as_deref()).or_default().push(*entry);
    }
    for (_, mut group) in groups {
        // 组内按更新时间倒序：新记忆优先与「最近写的那批」相连
        group.sort_by(|a, b| b.updated_at.cmp(&a.updated_at));
        for (index, left) in group.iter().enumerate() {
            let mut made = 0usize;
            for right in group.iter().skip(index + 1) {
                if made >= MEM_LINK_FANOUT {
                    break;
                }
                // 主题标签相交才算「讲同一件事」
                let shared = left
                    .tags
                    .iter()
                    .filter(|tag| is_topic(tag))
                    .any(|tag| right.tags.contains(tag) && is_topic(tag));
                // 取代关系是硬事实，不受标签与条数限制
                let supersedes = left.supersedes.contains(&right.id)
                    || right.supersedes.contains(&left.id);
                if supersedes {
                    index::global().link(&left.id, &right.id, "relates");
                    links += 1;
                    made += 1;
                } else if shared {
                    index::global().link(&left.id, &right.id, "same_topic");
                    links += 1;
                    made += 1;
                }
            }
        }
    }
    if links > 0 {
        crate::server::logging::log("[Memory]", &format!("记忆关联图已更新：{links} 条边"));
    }
}

/// 一条记忆在关联图里最多连几条边。
///
/// 5 是「够用且不会爆炸」的数：图扩散的用途是给检索补几个候选，补 5 个和补 50 个
/// 对最终 top-k 的影响没有区别，但 50 个邻居 × 20 个种子 = 1000 次 SQL。
const MEM_LINK_FANOUT: usize = 5;

/// 标签出现在超过这个比例的条目上，就当它是「泛标签」而不是主题。
///
/// 0.05 = 5%：一个真正有区分度的主题标签，在几千条记忆里通常只覆盖几十条。
/// 而 `导入` 这类标签会覆盖 100%。阈值放在 5% 既能放过「rust」这种热门主题，
/// 又能挡住「导入」「daily」这种全员标签。
const MEM_LINK_TAG_SHARE: f64 = 0.05;

/// 全量重建（「重建索引」按钮 / 索引损坏 / 同步落盘后）
///
/// ── 重建之前先修「容器落错日期」的存量数据 ────────────────────
/// `entry_path` 早期按 `created_at.max(updated_at)` 算落点（等于永远取
/// `updated_at`），于是历史导入把几十个不同日期的几千条全写进「今天」那一个
/// 容器，文件涨到几十 MB 后被单文件上限整份跳过 —— 表现是「导入了 2341 条，
/// 普通记忆 0 条」。
///
/// 计算逻辑已经改成按 `created_at`，但**已经写错的文件不会自己变对**：
/// `entry.path` 还指着老容器，重建索引只会从老容器里再读一遍错的日期。
/// 所以重建前先拆一次。
///
/// ── 这里为什么用无标记版（每次都拆）────────────────────────
/// 启动自检走 [`store::split_misplaced_once`]，跑过就记标记。但「重建索引」
/// 是用户/同步流程主动发起的显式动作，可能发生在别处把旧备份文件拷回来之后
/// （同步落盘、用户从另一台机器捞文件）。那些文件不会更新迁移标记，标记为
/// 真的机器就永远不修它们。所以显式重建一律重新扫 —— 拆分本身幂等，
/// 没有落错日期的节时只是遍历一遍目录，代价在几十毫秒量级。
pub fn rebuild() -> Result<usize, String> {
    store::ensure_layout()?;
    let (moved, files, failed) = store::split_misplaced_containers();
    if moved > 0 || failed > 0 {
        crate::server::logging::log(
            "[Memory]",
            &format!(
                "已拆分落错日期的 l1 容器：{moved} 条搬回各自日期（涉及 {files} 个文件）{}",
                if failed > 0 {
                    format!("，{failed} 处写失败（相关节仍留在原处，下次重建会重试）")
                } else {
                    String::new()
                }
            ),
        );
    }
    let entries = store::load_all();
    let count = index::global().rebuild(&entries)?;
    // ── 先清图再重算 ────────────────────────────────────────────
    // `rebuild()` 只清 `entries` / `entries_fts`，`mem_link` 是「派生但不在重建
    // 范围内」的那张表。不清就等于在旧图上叠新图：历史导入那 5818 条带 `导入`
    // 标签的记忆两两相连，重建一次写 810 万行、重建十次翻十倍。清空后
    // `relink()` 会按新口径（泛标签不连、每条最多 5 条边）重算一遍。
    index::global().clear_links();
    relink(&entries);
    bump_revision();
    Ok(count)
}

// ── 读 ───────────────────────────────────────────────────────

pub fn list(query: &Query) -> Value {
    list_for(query, None)
}

/// 带调用方身份的检索（MCP `memory_search` / `memory_recent` 走这条）。
///
/// ── 为什么身份要单独传，不能塞进 `query.agent` ──────────────
/// `query.agent` 是**过滤条件**（「只看某个客户端写过的记忆」），`caller` 是
/// **归因**（「这次是谁在查」）。两者混用会出现两种事故：
/// ① 客户端只在 `X-Agent` 头里带身份、参数里不写 `agent`，`recall_log` 的
///    `agent` 全成 NULL —— 检索日志明明有几十行，接入页却看不出是谁；
/// ② 反过来拿 `X-Agent` 兜底填过滤条件，等于把「搜全部」悄悄改成「只搜我自己
///    写的」，导入来的历史记忆一条都搜不到，而且失败得很安静。
pub fn list_for(query: &Query, caller: Option<&str>) -> Value {
    ensure_index_once();
    let now = crate::server::logging::now_ms();
    let (rows, total) = search::search_page(query);
    let items: Vec<Value> = rows.iter().map(|row| row.to_json()).collect();
    index::global().log_recall(
        "search",
        &query.text,
        items.len(),
        0,
        caller,
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
    let mut draft_buckets: std::collections::BTreeMap<String, usize> = std::collections::BTreeMap::new();
    for entry in store::snapshot().iter().filter(|item| item.is_live()) {
        if entry.created_at < since {
            continue;
        }
        let day = search::format_date(entry.created_at + offset);
        // 热力图是「我哪天写了东西」的自我认知，不是产出统计。抽取草稿是模型
        // 在后台写的，把它算进去会让这张图从「我的写作节律」变成「机器跑了多少轮」——
        // 用户点开某一天，会看到一堆他从没写过的条目。所以正式记忆画格子，
        // 草稿单独给一个计数（界面用一行小字说明），两者不混。
        if is_auto_draft(entry) {
            *draft_buckets.entry(day).or_insert(0) += 1;
        } else {
            *buckets.entry(day).or_insert(0) += 1;
        }
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
        // 同期由自动抽取写下的草稿条数（不进格子，只用来解释「为什么库里条数
        // 比图上的总数多」）。
        "candidates": draft_buckets.values().sum::<usize>(),
        "end": search::format_date(now + offset),
    })
}

/// 按 id 取全文（可一次取多条）。顺带给出 3 条相关记忆
/// ——「读一条时看到旁边那条」是最常见的发现路径。
pub fn get(ids: &[String], body_limit: usize) -> Value {
    get_for(ids, body_limit, None)
}

/// 带客户端身份的按 id 取全文（MCP `memory_get` 走这条）。
///
/// 早先这里写死 `None`，`recall_log` 里 `tool=get` 的行永远归不到客户端，
/// 接入页的「最近有没有在读记忆」就只剩 `agent_beat` 一份证据 —— 一旦
/// 心跳统计口径变了，整条链路看起来就像没被用过。归因和 `core_text_for()`
/// 保持同一套：谁调的，就记谁。
pub fn get_for(ids: &[String], body_limit: usize, agent: Option<&str>) -> Value {
    ensure_index_once();
    let all = store::snapshot();
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
        //
        // 草稿不进「相关」：`memory_get` 是模型读一条时顺手拿到的上下文，它
        // 没有「这是草稿」这个提示，看到一行 `[知识] xxx（3 天前）` 只会当成
        // 既定事实引用。要让它读草稿，得显式走检索并传 `includeCandidates`。
        let mut related: Vec<(&MemoryEntry, usize)> = all
            .iter()
            .filter(|other| other.id != entry.id && other.is_live())
            .filter(|other| !is_auto_draft(other))
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
        index::global().log_recall("get", &ids.join(","), items.len(), 0, agent, None);
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
    let all = store::snapshot();
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
    recent_for(project, agent, days, None)
}

/// 带调用方身份的「最近 N 天」（MCP `memory_recent` 走这条）。
pub fn recent_for(
    project: Option<&str>,
    agent: Option<&str>,
    days: usize,
    caller: Option<&str>,
) -> Value {
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
    let page = list_for(&query, caller);
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
///
/// ── 为什么用 `snapshot()` 而不是 `load_all()` ────────────────
/// `load_all()` 返回的是独立副本：5818 条记忆、55 MB 正文，每次调用都要深拷贝
/// 一遍。这个函数只读不写，拿 `Arc` 就够了 —— 省掉的那次拷贝在首屏是能感觉到的
/// 那几百毫秒里的一部分。
pub fn projects() -> Value {
    ensure_index_once();
    let all = store::snapshot();
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
                .filter(|entry| !is_auto_draft(entry))
                .filter(|entry| entry_project_slug(entry) == project.slug)
                .collect();
            entries.sort_by(|a, b| b.updated_at.cmp(&a.updated_at));
            let l1 = entries.iter().filter(|entry| entry.layer == Layer::L1).count();
            let l2 = entries.iter().filter(|entry| entry.layer == Layer::L2).count();
            // 草稿单独数：项目卡上的 `count` 是「这个项目有多少条真记忆」。
            // 把未确认的抽取草稿算进去，用户会以为这个项目已经积累了一堆结论，
            // 而检索里搜不到其中任何一条。
            let candidates = all
                .iter()
                .filter(|entry| entry.is_live() && is_auto_draft(entry))
                .filter(|entry| entry_project_slug(entry) == project.slug)
                .count();
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
                "candidates": candidates,
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
        .filter(|entry| !is_auto_draft(entry))
        .filter(|entry| entry_project_slug(entry) == store::GLOBAL_SLUG)
        .collect();
    let general_candidates = all
        .iter()
        .filter(|entry| entry.is_live() && is_auto_draft(entry))
        .filter(|entry| entry_project_slug(entry) == store::GLOBAL_SLUG)
        .count();
    let general = json!({
        "slug": store::GLOBAL_SLUG,
        "count": general_entries.len(),
        "l1": general_entries.iter().filter(|entry| entry.layer == Layer::L1).count(),
        "l2": general_entries.iter().filter(|entry| entry.layer == Layer::L2).count(),
        "candidates": general_candidates,
        "latest": general_entries
            .iter()
            .map(|entry| entry.updated_at)
            .max()
            .unwrap_or(0),
        "agents": project_agents(&general_entries),
    });
    json!({ "items": items, "general": general })
}

/// 这条是不是「自动抽取写下、还没攒够强化次数的草稿」。
///
/// ── 为什么收成一个判据 ───────────────────────────────────────
/// 「候选」这个概念现在要在七八处判断：检索过滤、核心记忆、digest、注入门槛、
/// 体检、项目统计、仪表盘计数、前端标识。各处各写一遍 `layer == L1 &&
/// agent == auto-extract`，改一处忘一处 —— 真机已经出现过「检索排除了、注入
/// 没排除」的漏网。收成一条之后，语义变了只改这里。
///
/// 「还在 l1」是关键：升到 l2 之后它就是正式结论了，`agent` 字段仍留着
/// `auto-extract`（那是来源标记，不是身份），这时候不该再被当草稿藏起来。
pub(crate) fn is_auto_draft(entry: &MemoryEntry) -> bool {
    entry.layer == Layer::L1 && entry.agent.as_deref() == Some(model::AUTO_EXTRACT_AGENT)
}

/// 项目列表 / 项目卡（含通用汇总）。
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
    let all = store::snapshot();
    // 与 `enrich::build_profile` 同一口径：草稿不是「这个人身上已确认的事实」，
    // 既不算门槛、也不算素材。两边口径不一致时，界面会显示「素材 20 条、可以
    // 生成」，点下去却被告知「记忆只有 3 条，未达门槛 5」。
    let live: Vec<&MemoryEntry> = all
        .iter()
        .filter(|entry| entry.is_live() && !is_auto_draft(entry))
        .collect();
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

/// 自检：文件根、条数、索引状态、回收站、队列积压
///
/// ── 为什么「项目数」不再走 `projects()` ──────────────────────
/// `projects()` 会把整库按项目分组、每组算 l1/l2、挑前三条最新 —— 那是「项目页」
/// 需要的数据。而 `status()` 只要一个数字：有几个项目。早先为了这一个数字调
/// `projects()`，等于每打一次 `GET /api/memory/config`（首屏必打）就多付一次
/// 全库遍历 + 一份完整的项目列表序列化。改成直接从条目里数 slug，成本一次
/// `HashSet` 插入，与 `projects()` 的口径仍然一致（都走 `entry_project_slug`）。
///
/// ── 2026-11-21：这里曾经还带一份 `table_stats()` ─────────────
/// 那段对 10 张表各跑一次 `SELECT COUNT(*)`，其中 `mem_link` 在老库上是
/// 810 万行 —— 单这一条就是几百毫秒。而**前端根本没有读它**（`memory-api.ts`
/// 里只有一个可选字段，全仓 `.tables` 零消费）。首屏为此付几百毫秒是纯亏。
///
/// 表行数仍然要能查（排障时有用），但它属于「自检详情」而不是「首屏快照」：
/// 挪到 `GET /api/memory/status?tables=1`，只有明确要的时候才算。
/// 默认路径不碰这几张表。
pub fn status() -> Value {
    status_with(false)
}

/// `status()` 的实现，`with_tables` 决定要不要多算一份表行数。
///
/// 分成两支而不是「算完再删字段」：`table_stats()` 的开销全在 SQL 上，
/// 算完再丢等于白算一遍。
pub fn status_with(with_tables: bool) -> Value {
    let entries = store::snapshot();
    let live = entries.iter().filter(|entry| entry.is_live()).count();
    let deleted = entries.iter().filter(|entry| entry.deleted_at.is_some()).count();
    let superseded = entries
        .iter()
        .filter(|entry| entry.superseded_by.is_some() && entry.deleted_at.is_none())
        .count();
    let l1 = entries.iter().filter(|entry| entry.is_live() && entry.layer == Layer::L1).count();
    let l2 = entries.iter().filter(|entry| entry.is_live() && entry.layer == Layer::L2).count();
    // 草稿（auto-extract 的 l1 候选）单独数出来：它在 `l1` 里，但语义上和
    // 用户自己写的流水完全不同 —— 它不参与检索、不进注入、不进核心记忆。
    // 仪表盘若把它算进「普通记忆」，用户看到的是一个虚高的数字，而那个数字
    // 对应的东西点不进任何地方（检索搜不到、注入里也没有）。
    let candidates = entries.iter().filter(|entry| entry.is_live() && is_auto_draft(entry)).count();
    let index_count = index::global().try_count();
    // 「今日新增 / 昨日」按**本地自然日**切：用户看的「今天」是他自己的今天，
    // 按 UTC 切会让东八区早上 8 点前的写入落到前一天（热力图那条注释踩过同一个坑）。
    // 只数 live 条目 —— 已删除的不算「新增」，否则删一条会让数字倒扣。
    let (today, yesterday) = daily_counts(&entries);
    let mut out = json!({
        "root": store::root().to_string_lossy(),
        "indexFile": store::index_file().to_string_lossy(),
        "device": store::device_id(),
        "total": entries.len(),
        "live": live,
        "l1": l1,
        "l2": l2,
        // 「草稿」是 l1 的子集（见上面的注释），单列给仪表盘用：
        // 界面要能说清「这 N 条是模型草稿，还没被确认」，而不是混进普通记忆。
        "candidates": candidates,
        "deleted": deleted,
        "superseded": superseded,
        "today": today,
        "yesterday": yesterday,
        // 「已连通 Agent」：`called / injected`。放在 status 里而不是让前端再打
        // 一次 `/agents` —— 那个端点会为每张卡探测路径、版本、读两份文件，
        // 而仪表盘只要两个数字。口径说明见 `agents::connectivity`。
        "agents": agents::connectivity(),
        "projects": project_count(&entries),
        "review": {
            // `try_` 版本：索引正忙时给 `null` 而不是排队。前端按「未知」渲染
            // （徽章不出现），页面不会因为这一格挂住。
            "pending": index::global().try_review_count(),
            "warnThreshold": config::usize_of("dedup.pendingWarnThreshold"),
        },
        // 「索引一致」比的是**索引行数 vs 事实源总条数**，不是 vs live。
        // 索引要留着回收站与已取代的行（浏览页的回收站视图就读这些行），
        // 拿 `live` 去比，等于只要有回收站条目就永远报「不一致」—— 而
        // `rebuild` 会把它们原样写回去，用户点十次重建也消不掉这条提示。
        //
        // `count` / `inSync` 可能是 `null`：索引被重建或打开时压缩独占着连接。
        // 那是「暂时读不到」，不是「不一致」—— 前端必须区分这两种，否则会拿
        // 一个假的 0 去和总条数比，凭空报一条一致性警告。
        "index": {
            "count": index_count,
            "fts": index::global().fts_enabled(),
            "inSync": index_count.map(|count| count == entries.len()),
        },
        "config": config(),
        // 请求级注入的「真的在跑吗」证据：本次启动以来的次数与平均 token。
        // 光给开关状态没用 —— 用户要的是「它到底注入了没有」，而这件事没有
        // 计数器就只能靠猜（注入是用户看不见的动作）。
        "inject": inject::stats(),
        // 请求后留存的「写回来没有」证据。与 inject 成对：一个证明「读进去了」，
        // 一个证明「写回来了」。只修前半边时用户的感觉是「它记得东西，但新东西
        // 不进来」—— 那正是本轮真机验证暴露的缺口（Hindsight 的
        // `wrap_openai` 是「调用前 recall、调用后 retain」，本仓原来只有前者）。
        "retain": retain::stats(),
        // 内容版本号：每次写入 +1。排障时用来判断「Agent 看到的是不是最新」。
        "revision": revision(),
    });
    // 表行数只在明确要的时候才算：10 张表各一次 `COUNT(*)`，`mem_link` 在老库
    // 上 810 万行。首屏不读它，就不该为首屏付这笔钱（见 `status()` 的说明）。
    if with_tables {
        out["tables"] = index::global().table_stats();
    }
    out
}

/// 项目数（`status()` 用）。
///
/// 口径与 [`projects`] 的 `items.len()` 完全一致：同样按 `entry_project_slug`
/// 归一、同样排除「通用」（`_global` 不是一张可维护的卡）。差别只在**不构造
/// 项目卡**：不分组、不算 l1/l2、不挑最新三条、不序列化。首屏只要一个数字。
fn project_count(entries: &[MemoryEntry]) -> usize {
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    for entry in entries.iter().filter(|entry| entry.is_live()) {
        let slug = entry_project_slug(entry);
        if slug != store::GLOBAL_SLUG {
            seen.insert(slug);
        }
    }
    // 登记表里可能有「还没写过任何记忆」的空项目：`projects()` 会把它列出来，
    // 这里也必须算进去，否则两处数字对不上（用户点进项目页会看到多一张卡）。
    for project in store::load_projects() {
        if project.slug != store::GLOBAL_SLUG {
            seen.insert(project.slug);
        }
    }
    seen.len()
}

/// 今天与昨天各新增了多少条（按本地自然日、只算 live）。
///
/// ── 为什么用 `chrono::Local` 而不是 `core::beijing` ──────────
/// `beijing` 服务的是**上游自然日**（额度重置、签到），必须恒为 UTC+8；
/// 这里服务的是**用户视角的今天**，跟着本机时区走才是对的（`schedule` 的
/// 「每天 11:00」也是这个口径）。两者不可互换。
fn daily_counts(entries: &[MemoryEntry]) -> (usize, usize) {
    use chrono::{Local, TimeZone};
    let now = crate::server::logging::now_ms();
    let Some(moment) = Local.timestamp_millis_opt(now).single() else {
        return (0, 0);
    };
    let today = moment.date_naive();
    // 取本地零点：夏令时空档会回 None（`and_local_timezone` 的已知行为），
    // 那种情况下退化成「按 24h 回退」，宁可差一小时也不要整块归零。
    let start_of = |day: chrono::NaiveDate| {
        day.and_hms_opt(0, 0, 0)
            .and_then(|at| at.and_local_timezone(Local).single())
            .map(|at| at.timestamp_millis())
            .unwrap_or_else(|| now - (today - day).num_days() * 86_400_000)
    };
    let today_start = start_of(today);
    let yesterday_start = today.pred_opt().map(start_of).unwrap_or(today_start - 86_400_000);
    let mut today_count = 0;
    let mut yesterday_count = 0;
    for entry in entries.iter().filter(|entry| entry.is_live()) {
        // 与 `status()` 的 `l1` / `candidates` 同一口径：草稿不算「今天写了」。
        // 仪表盘上「今日新增 12 / 有效记忆 30」如果一边含草稿一边不含，用户拿
        // 这两个数字相减会对不上，进而怀疑统计坏了。
        if is_auto_draft(entry) {
            continue;
        }
        if entry.created_at >= today_start {
            today_count += 1;
        } else if entry.created_at >= yesterday_start {
            yesterday_count += 1;
        }
    }
    (today_count, yesterday_count)
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
    /// 这条记忆**原始发生**的时间（毫秒）。不传时用写入时刻。
    ///
    /// ── 为什么必须能透传 ────────────────────────────────────
    /// 历史导入把「三年前那次会话」搬进来时，如果这里一律填写入时刻，所有导入
    /// 条目都会堆在导入当天：时间衰减排序失效、热力图缩成一个点、l1 容器全挤进
    /// 同一天。原始时间必须由导入链路带进来，写路径只负责照收不误。
    pub created_at: Option<i64>,
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
/// ── 时间前缀为什么用「原始发生时间」而不是「此刻」──────────────
/// 历史导入会把三年前的会话搬进来。若 id 前缀一律用导入时刻，那批条目 id 全挤在
/// 导入日、与「按原始日期分布」的容器对不上，扫 id 时也分不清先后。传进来的
/// `stamp` 是内容原始时间（缺省才用此刻），id 前缀跟着它走，与容器日期一致。
///
/// ── 为什么还要掺一个进程内的自增序号 ────────────────────────
/// 只用「毫秒时间 + 内容哈希」时，**同一毫秒内两次写入相同内容**会得到同一个
/// id，而 id 就是文件名 —— 第二次 `save` 会把第一条整个覆盖掉，是静默的数据
/// 丢失（CI 的 `id_is_time_prefixed_and_unique` 用两条相同内容把它逼出来过）。
/// 掺一个自增序数就把「同毫秒 + 同内容」拆开了，而时间与内容仍是主成分，
/// id 依旧人类可读。
fn make_id(content: &str, stamp: i64) -> String {
    use std::hash::{Hash, Hasher};
    use std::sync::atomic::{AtomicU64, Ordering};
    static SEQUENCE: AtomicU64 = AtomicU64::new(0);
    let now = stamp;
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
                store::snapshot()
                    .iter()
                    .filter(|entry| entry.is_live() && entry.layer == Layer::L1)
                    .map(|entry| entry.hash.clone())
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

    /// 关闭延迟落盘窗口并把攒下的容器统一写盘。返回写失败的文件说明。
    ///
    /// 调用方（批量导入）必须在所有 `write` 之后调一次；漏调会让这一批里对
    /// 同一 l1 容器的追加**全部留在内存**里丢掉。与 `store::begin_deferred`
    /// 成对使用。
    pub fn flush(&mut self) -> Vec<String> {
        store::end_deferred()
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
    // 写入前脱敏（`privacy.redact`）。放在 trim 之前还是之后？
    // 放在**之后**：规则里常有「行尾结束」这类锚点，先 trim 掉首尾空白能让
    // `^` / `$` 落在真正的第一行/最后一行的首尾，而不是撞上一个空行。
    // 开关关着时 `apply` 一次配置读取就返回，代价可以忽略。
    let (content, redacted) = redact::apply(input.content.trim());
    let content = content.trim().to_string();
    if content.is_empty() {
        return Err("正文不能为空".into());
    }
    if redacted > 0 {
        // 记一条：用户开了开关就要能看到它真的干了活，而不是只能凭信任
        crate::server::logging::log(
            "[Memory]",
            &format!("写入前脱敏：替换 {redacted} 处（privacy.redactRules）"),
        );
    }
    store::ensure_layout()?;
    let now = crate::server::logging::now_ms();
    // 原始发生时间：调用方能给就给（历史导入 / 补录），给不出才是「此刻」。
    // 只影响 created_at / valid_from 与按日期分的容器落点；updated_at 永远是
    // 真实写入时刻 —— 它记的是「这条什么时候被改过」，不是「事情什么时候发生」。
    let created_at = input.created_at.filter(|stamp| *stamp > 0).unwrap_or(now);

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

    // id 前缀跟**原始发生时间**走，与 l1 容器日期一致；缺省才是此刻
    let id = make_id(&content, created_at);
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
        created_at,
        updated_at: now,
        valid_from: created_at,
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
        origin_path: String::new(),
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
        let all = store::snapshot();
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
            // 谁留下不由「谁更新」决定，由信息量决定 —— 见 `information_score`。
            // 原来的规则是「新的必赢」，真实场面是隔天的「同上，已修复」把一份
            // 带根因带路径的完整结论顶进回收站，越整理越空。
            let keep = dedup_keeper(&entry, &candidate);
            let mut merged_into: Option<String> = None;
            if config::bool_of("review.autoConfirmDedup")
                && score >= config::number("dedup.l2.autoMergeThreshold")
            {
                let result = if keep == DedupKeeper::New {
                    apply_dedup(&entry.id, &candidate)
                } else {
                    // 反向：旧的那条更全，把**本次新写的**标成被它取代。
                    // 仍然不删新条目 —— 演化链要能倒着走回去。
                    apply_dedup(&candidate, &entry.id)
                };
                match result {
                    Ok(()) => {
                        if keep == DedupKeeper::Candidate {
                            merged_into = Some(candidate.clone());
                        }
                    }
                    Err(error) => {
                        crate::server::logging::log("[Memory]", &format!("⚠️ 自动采纳去重失败：{error}"));
                    }
                }
            } else {
                index::global().enqueue(
                    "dedup",
                    &entry.id,
                    &json!({
                        "candidate": candidate,
                        "score": score,
                        // 队列里要能看出「批准后会留哪一条」。只写 candidate 的
                        // 话，人工裁决时无法判断这次批准是把新条并进去还是反过来。
                        "keep": if keep == DedupKeeper::New { "new" } else { "candidate" },
                    }),
                    score,
                );
            }
            if let Some(into) = merged_into {
                // 这一轮写进去的条目当场就被判成旧结论的旁支。返回既有条目的 id，
                // 否则调用方拿到的是一个「写了但搜不到」的 id（它已被 superseded
                // 过滤掉），会以为记忆丢了。
                //
                // 只在**真的执行了**反向合并时才提前返回：`autoConfirmDedup` 关着
                // 的时候上面只入队、什么都不改，此时新条目是活的，必须照常返回。
                crate::server::logging::log(
                    "[Memory]",
                    &format!("写入并入 {into}（旧条目信息量更大，本次内容标为被取代）"),
                );
                return Ok(json!({
                    "id": into,
                    "entry": Value::Null,
                    "duplicate": true,
                    "superseded": 0,
                    "mergedInto": into,
                }));
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
///
/// 返回 owned 值（`cloned`）：调用方要拿它去标记 / 落盘，借一份引用反而要处理
/// 快照生命周期。命中时只克隆一条，代价可以忽略。
fn find_hash_duplicate(hash: &str) -> Option<MemoryEntry> {
    store::snapshot()
        .iter()
        .filter(|entry| entry.is_live() && entry.hash == hash)
        // 只有 l1 走精确去重：l2 的两条结论可能一字不差但语境不同
        // （例如同一个决定在两个项目里各下了一次），合并会丢掉项目归属
        .find(|entry| entry.layer == Layer::L1)
        .cloned()
}

/// 两条近似重复里，谁该留下。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DedupKeeper {
    /// 本次新写的这条信息量更大（或至少一样大）
    New,
    /// 库里已有的那条更全，本次内容并入它
    Candidate,
}

/// 由**信息量**决定留下谁，而不是由时间。
///
/// ── 为什么平手时留新的 ────────────────────────────────────
/// 分数相同意味着「两边信息量一样」，此时新条目通常包含用户刚补充的口径
/// （同一句结论的新版本），留它更符合直觉；而且留旧的会让「刚写的东西立刻
/// 被标成已取代」这种难以解释的结果变多。
///
/// ── 为什么不看 `updated_at` ────────────────────────────────
/// `updated_at` 会被「改了个标签」「点亮星标」推到现在，一次无关操作就能让
/// 一条短条目赢过一份完整结论。时间只以 `created_at` 的形态进入
/// `information_score` 的新鲜度项，权重 0.15，压不过「长度 + 标签」。
fn dedup_keeper(entry: &MemoryEntry, candidate_id: &str) -> DedupKeeper {
    // 关掉开关就等于退回旧规则：新条取代旧条。这是唯一能让用户「恢复原样」的
    // 出口 —— 判据本身改的是已有数据的去向。
    if !config::bool_of("dedup.l2.preferRicher") {
        return DedupKeeper::New;
    }
    let Some(other) = store::snapshot().iter().find(|item| item.id == candidate_id).cloned() else {
        // 快照里找不到（刚被删 / id 拼错）：结论只能是「保留刚写的这条」，
        // 因为没有可比的对手。宁可少合并一次，也不能把新条目挂到空气上。
        return DedupKeeper::New;
    };
    if information_score(&other) > information_score(entry) {
        DedupKeeper::Candidate
    } else {
        DedupKeeper::New
    }
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
    // 快照先绑到局部变量：`for other in store::snapshot().iter()` 虽然临时值会
    // 活到循环结束（只调一次），但读代码的人得先知道这条临时值生命周期规则才能
    // 确信它没在每轮重读 55 MB。绑出来是零成本的，且意图直接可读。
    let all = store::snapshot();
    for other in all.iter() {
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
            // 必须 `.clone()`：`all` 是 `Arc<Vec<MemoryEntry>>`，`other` 是共享
            // 借用，搬不走 `id`（E0507）。这条在上一轮把 `load_all()` 换成
            // `snapshot()` 时漏了 —— 当时 `load_all()` 返回 owned `Vec`，
            // `into_iter()` 能直接搬；换成 `iter()` 之后语义变了。
            best = Some((other.id.clone(), score));
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
            // 改写正文也过同一道脱敏：否则「先写安全内容、再 update 塞秘密」
            // 就能绕过写入侧的开关，那道闸门等于形同虚设。
            let (clean, redacted) = redact::apply(body);
            if redacted > 0 {
                crate::server::logging::log(
                    "[Memory]",
                    &format!("更新前脱敏：替换 {redacted} 处（privacy.redactRules）"),
                );
            }
            entry.body = clean;
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
    // 原路径必须在动 `deleted_at` **之前**算：`relative_path` 见到 `deleted_at`
    // 非空就返回 `trash/<id>.md`，顺序反了记下的「原路径」其实是回收站路径，
    // 恢复时会把记忆搬回 `trash/` 里（等于恢复后立刻又进回收站）。
    let origin = store::live_rel(&entry);
    entry.deleted_at = Some(crate::server::logging::now_ms());
    entry.updated_at = entry.deleted_at.unwrap_or(entry.updated_at);
    // 已经在回收站里的条目再次删除时保留第一次记下的原路径。
    if entry.origin_path.is_empty() {
        entry.origin_path = origin;
    }
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
    // 回原处，而不是按 layer/kind 重新算一个位置：原路径在删除那一刻记过
    // （`origin_path`，见 `remove`）。没有它说明这条是手改 frontmatter 或从
    // 别的设备同步过来、从没经过本机 `remove` —— 那时才退回重算，行为与旧版一致。
    if entry.origin_path.is_empty() {
        entry.path = relative_path(&entry);
    } else {
        entry.path = entry.origin_path.clone();
        entry.origin_path.clear();
    }
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
///
/// 顺手清一次同步删除留档（`sync-trash/`）：两者共用一个保留窗口，
/// 用户点「清空回收站」时脑子里想的就是「把删掉的东西真的删掉」，
/// 留档偏偏是另一份「删掉的东西」，不一起清就等于骗人。
pub fn purge_trash() -> Value {
    let days = config::usize_of("storage.trashKeepDays").max(1);
    let removed = store::purge_expired_trash(days);
    let archived = store::purge_expired_sync_trash(days);
    // 手动「清空回收站」也顺手按天清日志：用户此刻想清的就是过期数据，
    // 而 `auto.logKeepDays` 那一栏在设置页里没有单独按钮。
    let logs = index::global().purge_logs_by_age(config::usize_of("auto.logKeepDays").max(1));
    bump_revision();
    json!({
        "removed": removed,
        "syncTrashRemoved": archived,
        "logsRemoved": logs,
        "keepDays": days,
    })
}

/// 回收站文件清单（界面上「回收站」那张表的数据源）。
///
/// ── 为什么不直接用 `list(includeDeleted: true)` ──────────────────
/// 那张表是「记忆条目」，按项目/Agent/层级过滤、按相关度打分，返回的是记忆
/// 视图；回收站要看的是**文件**：删了什么、多大、还能恢复几天。两件事的排序
/// 与字段都不一样 —— 回收站按删除时间倒序，记忆表按更新时间倒序。
///
/// 排序与截断都在后端做：回收站可能有上千个文件，前端 `slice` 得先把全部
/// 传过去，一个删过几百条的用户每次打开回收站都要多传几百 KB。
pub fn trash_list() -> Value {
    let days = config::usize_of("storage.trashKeepDays").max(1);
    let now = crate::server::logging::now_ms();
    let root = store::root();
    let mut items: Vec<Value> = Vec::new();
    // 文件 mtime 是「进回收站的时刻」，与 `purge_expired_trash` 用同一判据 ——
    // 界面显示的「还剩几天」必须和真正会被清掉的时刻一致，否则用户会以为
    // 还有三天，结果今晚就被清了。
    let Ok(reader) = std::fs::read_dir(store::trash_root()) else {
        return json!({ "items": [], "count": 0, "totalBytes": 0, "keepDays": days, "expiring": 0 });
    };
    let mut total_bytes = 0u64;
    let mut expiring = 0usize;
    for item in reader.flatten() {
        let path = item.path();
        if path.extension().and_then(|ext| ext.to_str()) != Some("md") {
            continue;
        }
        let Ok(meta) = std::fs::metadata(&path) else { continue };
        let size = meta.len();
        total_bytes += size;
        let stamp = meta
            .modified()
            .ok()
            .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|duration| duration.as_millis() as i64)
            .unwrap_or(0);
        // 到期时刻 = 进回收站时刻 + 保留天数；`purge_expired_trash` 用的是同一
        // 条算式（`stamp < now - days`），所以这里算出来的 `expiresIn` 归零那天
        // 就是它真被删掉那天。
        let expire_at = if stamp > 0 { stamp + days as i64 * 86_400_000 } else { 0 };
        let left_ms = expire_at - now;
        let expires_in = if expire_at > 0 { (left_ms as f64 / 86_400_000.0).ceil() as i64 } else { days as i64 };
        if expires_in <= 1 {
            expiring += 1;
        }
        // 原路径与标题从文件本身读：回收站里的文件是自包含的（frontmatter 里
        // 有 `origin_path` / `title`），不必再查索引 —— 索引可能已经不含这条
        // （`purge` / 重建后索引里没有回收站条目）。
        let text = std::fs::read_to_string(&path).unwrap_or_default();
        let entry = MemoryEntry::from_markdown(&text, path.file_stem().and_then(|n| n.to_str()).unwrap_or(""), &store::rel(&path));
        let origin = if entry.origin_path.is_empty() {
            // 老版本删的条目没记原路径（那时还没这个字段）。给个能看懂的占位，
            // 别在界面上留一列空白 —— 用户会以为是坏了。
            String::from("（未记录原路径）")
        } else {
            entry.origin_path.clone()
        };
        items.push(json!({
            "id": entry.id,
            "name": path.file_name().and_then(|n| n.to_str()).unwrap_or(""),
            "title": entry.title,
            "originPath": origin,
            "size": size,
            "trashedAt": stamp,
            "expiresAt": expire_at,
            "expiresIn": expires_in,
            "keepDays": days,
            "layer": entry.layer.as_str(),
            "project": entry.project,
            "agent": entry.agent,
        }));
    }
    // 删除时间倒序：最近删的在最上面，与「先看刚删的」这个使用习惯一致。
    items.sort_by(|a, b| {
        let left = b.get("trashedAt").and_then(Value::as_i64).unwrap_or(0);
        let right = a.get("trashedAt").and_then(Value::as_i64).unwrap_or(0);
        left.cmp(&right)
    });
    let count = items.len();
    json!({
        "items": items,
        "count": count,
        "totalBytes": total_bytes,
        "keepDays": days,
        "expiring": expiring,
        "root": store::rel(&root.join("trash")),
    })
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
    let live: Vec<MemoryEntry> = store::snapshot()
        .iter()
        .filter(|entry| entry.is_live())
        .cloned()
        .collect();
    let scanned = live.len();

    // ── 第一轮：精确 hash 分组 ────────────────────────────────
    // 每组保留**最新**的那条（它是用户最可能还在引用的一条），其余作为候选。
    //
    // ── 草稿不能当「保留方」───────────────────────────────────
    // 排序只看 `updated_at`，而抽取草稿总是最新的（它是刚从流水里压出来的）。
    // 于是一条草稿抄了用户三天前写的一句话，分组里草稿排第一，用户原文被当成
    // 「多余的旧条」入队 —— 批准后，人写的记忆被一条没人确认过的草稿取代。
    // 所以排序键先按「是不是草稿」分层：真记忆永远排在草稿前面。
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
        sorted.sort_by_key(|entry| (is_auto_draft(entry), std::cmp::Reverse(entry.updated_at)));
        let keep = sorted[0];
        for other in sorted.iter().skip(1) {
            exact += 1;
            // 载荷形状与写入路径一致（`candidate` + `score`），
            // 这样 `review_resolve` 的 `dedup` 分支不用为巡检再加一条路径。
            //
            // ── `keep` 必须显式写 ────────────────────────────────
            // `review_resolve` 在载荷**没有** `keep` 键时按「新条取代候选」处理
            // （`apply_dedup(target, candidate)`，target 是被取代的一方）。而这里
            // 的 `candidate` 恰恰是**要保留**的那条，`target` 才是多余的 —— 不写
            // `keep` 就会把幸存者标成「已被取代」，批准一条巡检建议等于埋掉正主。
            index::global().enqueue(
                "dedup",
                &other.id,
                &json!({
                    "candidate": keep.id,
                    "score": 1.0,
                    "foundBy": "scan",
                    "keep": "candidate",
                }),
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
                    &json!({
                        "candidate": keep.id,
                        "score": score,
                        "foundBy": "scan",
                        // 与第一轮同一条理由：`keep` 缺失时 `review_resolve` 会把
                        // 关系倒过来，批准后埋掉的是要留的那条。
                        "keep": "candidate",
                    }),
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
                    // 方向由载荷里的 `keep` 决定：`candidate` = 库里那条更全，
                    // 把 `target`（新写的）并进去；其余（含老载荷没这个键）沿用
                    // 「新条取代候选」的旧语义。裁决错了会把一份完整结论埋掉，
                    // 所以这里不能靠猜。
                    //
                    // 生产端必须显式写 `keep`：`dedup_scan` 两轮入队原来都漏了这个
                    // 键，而它给的 `candidate` 恰恰是要保留的那条 —— 按旧语义处理
                    // 会把幸存者标成「已被取代」。
                    let keep_candidate = row
                        .payload
                        .get("keep")
                        .and_then(Value::as_str)
                        .map(|value| value == "candidate")
                        .unwrap_or(false);
                    if keep_candidate {
                        apply_dedup(candidate, &row.target)?;
                    } else {
                        apply_dedup(&row.target, candidate)?;
                    }
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
    let mut entry = store::snapshot()
        .iter()
        .find(|entry| entry.id == memory_id)
        .cloned()
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
    let all = store::snapshot();
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

/// 把一条已有结论「确认一次」：追加证据锚点，够了就升 l2。返回是否升级。
///
/// ── 为什么用 `evidence` 当计数器，而不是新加 `reinforced` 字段 ────
/// 强化次数 = 被多少批不同的流水独立确认过。这正好是 `evidence` 的语义
/// （「支撑这条结论的原始记忆 id」）。新加字段要动 `ENTRY_COLUMNS` /
/// `ENTRY_SELECT` / 行解析 / `write_row` 四处，老库还要迁移；而 `cue` 是
/// 流水 id，同一条流水重复跑时天然去重，计数不会虚高。
///
/// ── 升级为什么要清 path ─────────────────────────────────────
/// l1 落的是容器（`general/l1/<agent>/<date>.md`，一个文件多节），l2 是一条
/// 一文件。`entry_path` 见到非空 `path` 就直接沿用它，不清空的话「升级」只会
/// 改 layer 字段、文件还留在 l1 容器里，下一轮扫描又把它当流水。
pub(super) fn reinforce_entry(target: &str, cue: &str) -> Result<bool, String> {
    reinforce_entry_with(target, cue, None, None)
}

/// 强化一条既有结论；如果**这次产出的正文信息量更大**，顺手把正文吸收进来。
///
/// ── 为什么要吸收，不能只追加一个锚点 ────────────────────────
/// 近似判重命中时的原做法是「给旧条目加一个 evidence 锚点、正文一个字不动」。
/// 这在「同一件事换个说法」时没问题，但在「旧的那条是三天前的一句半截结论、
/// 今天蒸馏出带根因带路径的完整版」时，结果是**新写出来的内容被直接丢掉**，
/// 库里永远停在那句半截结论 —— 整理越勤，记忆越旧。AgentHub 那边同一处的解法
/// 是按 `informationScore` 决定留谁；这里换成「不换 id、只换更全的正文」，
/// 既补上内容，又不打断图边、注入统计、人工裁决记录里对旧 id 的引用。
///
/// ── 判据为什么在 `information_score` 上，而不是比长度 ────────
/// 长度只是其中一项。新正文更长但没有任何标签、没有代码块、把日志整段贴进来时，
/// `information_score` 不会跟着涨，于是不会被吸收 —— 挡住「灌水版替换结论版」。
/// 具体做法是克隆一条探针、只换正文，让两边的标签/证据/时间完全一致，
/// 于是分差只反映正文本身。
pub(super) fn reinforce_entry_with(
    target: &str,
    cue: &str,
    incoming_body: Option<&str>,
    incoming_title: Option<&str>,
) -> Result<bool, String> {
    let Some(before) = store::load_all().into_iter().find(|entry| entry.id == target) else {
        return Err(format!("强化目标不存在：{target}"));
    };
    if !before.is_live() {
        return Err(format!("强化目标已失效：{target}"));
    }
    let mut entry = before.clone();
    if let Some(body) = incoming_body.map(str::trim).filter(|body| !body.is_empty()) {
        if should_absorb(&entry, body) {
            entry.body = body.to_string();
            // 哈希要跟着正文走，否则精确去重会把这条当成「另一条内容」，
            // 下一次同内容派生又新建一条 —— 这层判重就白设了。
            let (redacted, _) = redact::apply(&entry.body);
            entry.hash = model::content_hash(redacted.trim());
            if let Some(title) = incoming_title.map(str::trim).filter(|title| !title.is_empty()) {
                entry.title = truncate_chars(title, 80);
            }
            crate::server::logging::verbose(
                "[Memory]",
                &format!("强化 {target} 时吸收了信息量更大的新正文"),
            );
        }
    }
    if !cue.is_empty() && !entry.evidence.iter().any(|known| known.as_str() == cue) {
        entry.evidence.push(cue.to_string());
        // 上限 20：证据链是给人看「凭什么」的，不是越长越好
        if entry.evidence.len() > 20 {
            let overflow = entry.evidence.len() - 20;
            entry.evidence.drain(0..overflow);
        }
    }
    entry.updated_at = model::now_ms();
    let ready = entry.layer == Layer::L1
        && entry.evidence.len() >= model::PROMOTE_AFTER_REINFORCE;
    if ready {
        entry.layer = Layer::L2;
        entry.path.clear();
        store::move_save(&before, &entry)?;
    } else {
        store::save(&entry)?;
    }
    index::global().upsert(&entry)?;
    Ok(ready)
}

/// 人工采纳一条草稿：把它直接升成 l2 结论。
///
/// ── 为什么必须有这条路径 ─────────────────────────────────────
/// 新的准入模型把「反复确认」交给了流水（`evidence` 攒够 2 条才升层），但那只
/// 覆盖「同一件事被不同批流水反复抽到」这一种情况。真实场面里更常见的是：
/// 用户打开浏览页，看见一条草稿，一眼就知道「对，就是这个，我早就想让它记住」。
/// 这时候让他等模型下次再抽到一次，等于把人的判断排在机器后面 —— 而人是唯一
/// 的负向真值入口，正向判断同样该算数。
///
/// 反过来，如果只给「拒绝」，草稿就只能靠时间自然堆积，用户会开始怀疑这套
/// 东西到底有没有在动。
///
/// ── 为什么不做成 `update(id, {layer:"l2"})` ─────────────────
/// `update()` 的换层是「按 patch 改字段」，它不会清 `path`、也不管这条是不是
/// 草稿；走那条路的调用方（含 MCP `memory_update`）还可能顺手改正文、改项目，
/// 一次调用做四件事。人工采纳的语义只有一个：**这条我认了**。单开一个函数，
/// 前端就能给一个明确的按钮，审计日志也能说清是谁认的。
pub fn promote_draft(id: &str) -> Result<Value, String> {
    let Some(before) = store::load_all().into_iter().find(|entry| entry.id == id) else {
        return Err(format!("找不到记忆 {id}"));
    };
    if !before.is_live() {
        return Err("这条记忆已失效，先恢复再采纳".into());
    }
    if before.layer == Layer::L2 {
        // 幂等：重复点不该报错，也不该再写一次盘
        return Ok(json!({ "id": id, "promoted": false, "layer": "l2" }));
    }
    let mut entry = before.clone();
    // 人工采纳本身算一次确认。锚点带 `manual:` 前缀，与流水 id 区分开 ——
    // 前端据此不把它渲染成可点链接（库里没有这条记忆）。
    let cue = format!("manual:{id}");
    if !entry.evidence.iter().any(|known| known.as_str() == cue) {
        entry.evidence.push(cue);
    }
    entry.layer = Layer::L2;
    // 与 `reinforce_entry_with` 同一条理由：不清 path 的话只会改 layer 字段，
    // 文件还留在 l1 容器里，下一轮扫描又把它当流水读回来。
    entry.path.clear();
    entry.updated_at = model::now_ms();
    store::move_save(&before, &entry)?;
    index::global().upsert(&entry)?;
    bump_revision();
    crate::server::logging::log(
        "[Memory]",
        &format!("人工采纳草稿 {id} → l2"),
    );
    Ok(json!({ "id": id, "promoted": true, "layer": "l2", "entry": entry.to_json(true) }))
}

/// 在**自动抽取的 l1 候选**里找一条讲同一件事的，返回它的 id。
///
/// ── 为什么候选之间也要判重 ─────────────────────────────────
/// 候选要攒够 `PROMOTE_AFTER_REINFORCE` 条证据才升 l2，看起来「多一条候选」只是
/// 多一行待确认。但升级是**每条候选各升各的**：同一件事被抽成两份措辞不同的
/// 候选，各自攒够证据，最后就是两条 l2 —— 重复只是被推迟了，没被消灭。真机那
/// 五组同义结论（版本号 / 交付方式 / 信息量裁决）就是这么来的。
///
/// 所以新建候选之前先看一眼：已经有近似候选时，这次不是「新建」而是「又确认了
/// 一次」，走 `reinforce_entry`。
///
/// ── 只比候选，不比人写的 l1 ────────────────────────────────
/// 人写的流水可能是同一件事的不同侧面（上午的排查、下午的结论），把它们合成
/// 一条等于改写历史。候选则相反：它本来就是模型对同一批素材的压缩产物，合并
/// 是它的本职。判据只认 `agent == auto-extract` 且 `layer == L1`。
pub(super) fn similar_l1_candidate(
    project: Option<&str>,
    kind: MemoryKind,
    body: &str,
) -> Option<String> {
    let threshold = config::number("dedup.l2.candidateThreshold");
    store::snapshot()
        .iter()
        .filter(|entry| entry.is_live() && entry.layer == Layer::L1)
        .filter(|entry| entry.agent.as_deref() == Some(model::AUTO_EXTRACT_AGENT))
        .filter(|entry| entry.project.as_deref() == project)
        .filter(|entry| entry.kind == kind)
        .filter(|entry| {
            // 与 `apply_derived` 同一道粗筛：长度差一倍以上不可能是同一件事，
            // 先挡掉能省下绝大部分 O(n·m) 的字符比对
            let (left, right) = (body.chars().count(), entry.body.chars().count());
            let (short, long) = if left < right { (left, right) } else { (right, left) };
            short * 2 >= long
        })
        .map(|entry| (entry.id.clone(), similarity(body, &entry.body)))
        .filter(|(_, score)| *score >= threshold)
        .max_by(|left, right| left.1.partial_cmp(&right.1).unwrap_or(std::cmp::Ordering::Equal))
        .map(|(id, _)| id)
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
    // 一次快照给下面三处用（继承 files / 判重 / 图边），不重复读盘
    let snapshot = store::snapshot();
    // ── 继承来源条目的 `files` ─────────────────────────────────
    //
    // 派生条目原来一律 `files: Vec::new()`，后果是**请求级注入的路径命中永远
    // 打不中 l2**：注入里权重最高的一档是「记忆的 files 与请求里的路径对上」
    // （+3.0），而 l2 全是蒸馏 / 合并 / 抽取出来的，files 空着就等于这一档对
    // 它们永久失效 —— 只剩标题 / 摘要里凑巧出现的字符串可比。
    //
    // 来源条目的 files 是写入时锚定的真实路径，派生结论本来就建立在这些文件
    // 之上，继承过来是语义正确的：下次再改同一个文件，这条结论能被命中。
    // 上限 8 条，避免一条合并结论把二十个文件全挂上、变成「命中一切」。
    //
    // 但只继承不够：导入进来的 l1 流水（会话记录）本身没有 `files`，它们的路径
    // 全在正文里。所以再从来源正文里抽一遍路径 —— 这是确定性字符串抽取，不依赖
    // 模型输出，也不会把「看起来像路径」的普通词收进来（判据见 `harvest_paths`）。
    //
    // ── 这里存的是**仓库相对路径**，与 `files` 原本的「记忆根相对」不同 ──
    // 体检（`health`）会按记忆根去 stat 这些路径，找不到就跳过那一条 —— 于是
    // 派生条目不会被误报成「过时」。这是有意的取舍：宁可体检对 l2 少报一次，
    // 也不要让请求级注入的路径命中对 l2 永久失效（那才是这套机制的主要出口）。
    let files: Vec<String> = {
        let mut out: Vec<String> = Vec::new();
        for source in &sources {
            let Some(entry) = snapshot.iter().find(|entry| &entry.id == source) else {
                continue;
            };
            for file in &entry.files {
                // 容器路径（`general/l1/**.md`）不是锚定文件：继承过来会让体检
                // 永久报「这条过时」。语义上它属于「来源」，由 `evidence` 记。
                if store::internal_file(file) {
                    continue;
                }
                if !out.iter().any(|known| known == file) {
                    out.push(file.clone());
                }
            }
            for file in inject::harvest_paths(&entry.body) {
                if store::internal_file(&file) {
                    continue;
                }
                if !out.iter().any(|known| known == &file) {
                    out.push(file);
                }
            }
            if out.len() >= 8 {
                break;
            }
        }
        out.truncate(8);
        out
    };
    // ── 同内容不写第二份 ───────────────────────────────────────
    //
    // 蒸馏 / 合并现在默认**直接落地**（`review.autoConfirmDistill`），而队列里
    // 那条仍然留着等审计。于是有两条路会走到这里：自动落地一次、用户之后在
    // 待确认列表里又点一次「批准」。`write()` 的精确去重只管 l1（l2 允许两条
    // 一字不差但语境不同），所以第二次会老老实实再写一条 —— 库里出现两条
    // 一模一样的结论，注入时还会占两个名额。
    //
    // 这里按内容哈希挡一次：l2 里已有同内容的活条目就直接返回成功（幂等），
    // 不写、不报错。判据放在派生入口而不是 `write()` 里，是为了不动 l1/l2
    // 各自的去重语义 —— 「同一个决定在两个项目里各下一次」仍然允许。
    //
    // 哈希要先过一遍脱敏（`write_inner` 里是 `redact::apply` 之后再算），否则
    // 开了 `privacy.redact` 时两条内容相同但含敏感词的结论会算出不同的哈希，
    // 这层判重就白设了。
    let (redacted_body, _) = redact::apply(body);
    let digest = model::content_hash(redacted_body.trim());
    if snapshot
        .iter()
        .any(|entry| entry.is_live() && entry.layer == Layer::L2 && entry.hash == digest)
    {
        crate::server::logging::verbose(
            "[Memory]",
            "派生结论已存在（同内容 l2），跳过重复落盘",
        );
        return Ok(());
    }
    // ── 同义改写也不写第二份 ───────────────────────────────────
    //
    // 上面那道哈希只挡「一字不差」。真机上 132 条 l2 里五组同义结论各占 3-6 条，
    // 每组措辞都不同 —— 哈希全放行。生成端（`extract` / `distill`）已经让模型
    // 先认清单，但还有一条绕过它的路：队列里的人工批准，`review_resolve` 直接
    // 调这里，模型当时看到的清单早就不作数了。
    //
    // 所以派生入口自己再判一次词面近似：同项目、同类型、相似度过
    // `dedup.l2.candidateThreshold`（默认 0.72）的活 l2 已经存在时，把这次的内容
    // 当成对那条的「再一次确认」—— 追加证据锚点、刷新时间，不再新建。
    //
    // 阈值用候选阈值而不是自动合并阈值（0.9）：0.9 只抓得住几乎一样的句子，
    // 而这里要抓的正是「换了个说法」。判错的代价可控 —— 被强化的那条只是多了
    // 一个证据锚点、正文没被改写，人随时能在浏览页看出来。
    //
    // ── 合并（consolidate）不走这一档 ───────────────────────────
    // 合并的产物本来就比任何单条来源更全，把它「强化」进一条既有结论等于把新
    // 写出来的内容丢掉、只留一个锚点。合并载荷带 `sources`（≥2 条），蒸馏载荷
    // 带 `evidence` —— 按**键名**区分，不按条数：蒸馏的一条结论也可能引用两三条
    // 流水，按条数会把它们误判成合并。
    let is_merge = payload.get("sources").map(|value| !value.is_null()).unwrap_or(false);
    let near = if is_merge {
        None
    } else {
        snapshot
            .iter()
            .filter(|other| other.is_live() && other.layer == Layer::L2)
            .filter(|other| other.project.as_deref() == payload.get("project").and_then(Value::as_str))
            .filter(|other| {
                payload
                    .get("type")
                    .and_then(Value::as_str)
                    .map(|kind| other.kind == MemoryKind::parse(kind))
                    .unwrap_or(true)
            })
            // 长度先粗筛：`similarity()` 是 O(n·m) 的字符二元组比对，一次蒸馏最多
            // 20 条结论 × 上百条 l2，不做粗筛就是几亿次比较。长度差一倍以上不可能
            // 是同一件事 —— 这是免费且不会误杀的前置条件。
            .filter(|other| {
                let (left, right) = (body.chars().count(), other.body.chars().count());
                let (short, long) = if left < right { (left, right) } else { (right, left) };
                short * 2 >= long
            })
            .map(|other| (other.id.clone(), similarity(body, &other.body)))
            .filter(|(_, score)| *score >= config::number("dedup.l2.candidateThreshold"))
            .max_by(|left, right| left.1.partial_cmp(&right.1).unwrap_or(std::cmp::Ordering::Equal))
    };
    if let Some((existing, score)) = near {
        // 锚点用「这次是从哪来的」：合并载荷带 sources，蒸馏载荷带 evidence，
        // 都没有就退回内容哈希前缀 —— 同一份内容重复批准时天然去重。
        let cue = sources
            .first()
            .cloned()
            .unwrap_or_else(|| format!("derived:{}", &digest[..12]));
        // ── 正文也要一起比 ──────────────────────────────────────
        // 只追加锚点会让「旧的那条是半截结论、今天蒸馏出完整版」这种情况把新
        // 内容直接丢掉。`reinforce_entry_with` 在信息量真的更大时把正文吸收进来，
        // 否则一个字都不动（同一件事换个说法时，旧措辞更稳）。
        let title = payload.get("title").and_then(Value::as_str);
        match reinforce_entry_with(&existing, &cue, Some(body), title) {
            Ok(_) => {
                crate::server::logging::verbose(
                    "[Memory]",
                    &format!(
                        "派生结论与已有 l2 近似（{score:.2}），改为强化 {existing} 而不新建"
                    ),
                );
                // 这里**不动** `sources`。走到这一步的一定是蒸馏载荷（合并载荷
                // 的 `is_merge` 为真、`near` 直接是 None），而蒸馏载荷里的
                // `sources` 来自 `evidence` —— 那是「这条结论引用过哪些流水」，
                // 不是「哪些条目被这条结论取代」。把它们标成 superseded 等于
                // 把证据链上的原始流水打成失效，比重复更糟。
                return Ok(());
            }
            Err(error) => {
                // 强化失败（目标刚被删 / 盘写不进去）不该让整条派生结论丢掉：
                // 落到下面正常新建那条路，最坏结果是多一条，比丢一条好。
                crate::server::logging::verbose(
                    "[Memory]",
                    &format!("近似结论改为强化失败，退回新建：{error}"),
                );
            }
        }
    }
    let input = WriteInput {
        content: body.to_string(),
        title: payload.get("title").and_then(Value::as_str).map(str::to_string),
        kind: payload.get("type").and_then(Value::as_str).map(str::to_string),
        layer: Some("l2".to_string()),
        project: payload.get("project").and_then(Value::as_str).map(str::to_string),
        agent: Some("review".to_string()),
        tags: Vec::new(),
        importance: payload.get("importance").and_then(Value::as_f64),
        files,
        supersedes: sources.clone(),
        pinned: Some(false),
        starred: Some(false),
        cwd: None,
        session: None,
        created_at: None,
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

/// 这次的正文该不该覆盖既有条目的正文。
///
/// 单独一个函数是为了可测：`reinforce_entry_with` 要读写真实记忆根，而这条
/// 判据本身是纯的（只比两条正文的信息量）。判错的两种代价不对称 ——
/// 该吸收没吸收 = 老结论一直半截；不该吸收却吸收 = 把一份完整结论换成灌水版，
/// 所以判据宁可保守：**必须严格更高**才换，平手保留原文（原措辞已被引用过）。
fn should_absorb(existing: &MemoryEntry, candidate: &str) -> bool {
    let mut probe = existing.clone();
    probe.body = candidate.to_string();
    model::information_score(&probe) > model::information_score(existing)
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

/// 核心记忆文本（MCP `memory_core` 的产出；也供页面预览）。
///
/// ── 为什么 `agent` 必须透传 ────────────────────────────────
/// 早先这里写死 `None`，于是 `recall_log` 里 `memory_core` / `memory_search`
/// 的调用全都归不到具体客户端：接入页按 `agent_beat` 显示「尚未观察到真实
/// 调用」，而检索日志里明明有几十条记录却看不出是谁 —— 两处口径互相矛盾，
/// 排查时只能靠猜。`agent_beat` 由 MCP 分发层统一记，这里的 `agent` 是给
/// 「按客户端看检索历史」用的第二份证据。
pub fn core_text(project: Option<&str>, max_tokens: usize) -> Value {
    core_text_for(project, max_tokens, None)
}

/// 带客户端身份的核心记忆读取（MCP `memory_core` 走这条）。
pub fn core_text_for(project: Option<&str>, max_tokens: usize, agent: Option<&str>) -> Value {
    ensure_index_once();
    let entries = store::snapshot();
    let (text, used, truncated) = search::core_text(&entries, project, max_tokens);
    index::global().log_recall("core", project.unwrap_or(""), entries.len(), used, agent, project);
    json!({
        "text": text,
        "tokens": used,
        "truncated": truncated,
        "totalTokens": model::estimate_tokens(&text),
    })
}

pub fn digest_text(max_lines: usize) -> Value {
    ensure_index_once();
    let entries = store::snapshot();
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
    let entries = store::snapshot();
    let root = store::root();
    let limit = limit.clamp(1, 200);
    let mut stale: Vec<Value> = Vec::new();
    for entry in entries.iter().filter(|entry| entry.is_live()) {
        // 候选不参与体检：它带着从流水继承的 `files`（`schedule.rs` 里抽取时
        // 抄了素材的文件锚点），于是每跑一轮就多几条「可能过时」——而这些条目
        // 本身还没被确认，让人去复核一堆草稿等于制造噪音。升到 l2 后再进体检。
        if is_auto_draft(entry) {
            continue;
        }
        if entry.files.is_empty() {
            continue;
        }
        let mut touched: Vec<Value> = Vec::new();
        for file in &entry.files {
            // 记忆库自己的容器不是「被引用的代码」：`general/l1/gateway/2026-11-22.md`
            // 每写一条新记忆 mtime 就变，拿它当锚定文件会**永久**报「可能过时」。
            // 真机 6 条全部是这个成因（l2 从 l1 派生时继承了来源容器路径）。
            if store::internal_file(file) {
                continue;
            }
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
    // 只取一次 `count()`：每次都要抢那条全局连接，取两遍等于把等待时间翻倍
    // （索引正在重建时它就是几十秒）。这一条走阻塞版是故意的 —— 健康检查问的
    // 就是「索引现在能不能读」，读不到时宁可让这张卡片报错，也不要给个假数字。
    let index_count = index::global().count();
    json!({
        "stale": stale,
        "staleCount": stale.len(),
        "review": { "pending": pending, "warnThreshold": warn, "backlogged": pending >= warn },
        "index": {
            "count": index_count,
            // 与 `status()` 同一口径：索引留回收站/已取代的行，所以比的是总条数。
            "inSync": index_count == entries.len(),
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
        let now = crate::server::logging::now_ms();
        let a = make_id("同样的内容", now);
        let b = make_id("同样的内容", now);
        assert!(a.starts_with(&search::format_date(now).replace('-', "")));
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

    #[test]
    fn longer_entry_keeps_its_place_over_a_short_follow_up() {
        // 真实场面：前一天写了一条带根因带路径的完整结论，第二天补一句
        // 「同上，已修复」。旧的规则是「新的必赢」，这句话会把完整结论顶进回收站。
        let mut full = MemoryEntry::draft(
            "a",
            "发布脚本为什么读不到版本号",
            MemoryKind::Pitfall,
            "根因：`scripts/deliver.mjs` 里读的是 `process.env.VERSION`，而 CI 只写了 \
             `GITHUB_REF_NAME`。修法是把两处对齐，并且不要在读不到时回落到 1.0.0 —— \
             那会让包名显示成错的版本。实测跑 `node scripts/deliver.mjs` 能看到正确的 \
             3.5.0-beta.1。",
        );
        full.tags = vec!["发布".into(), "版本号".into(), "ci".into()];
        full.evidence = vec!["run 44921295723".into()];

        let mut short = MemoryEntry::draft(
            "b",
            "发布脚本版本号",
            MemoryKind::Pitfall,
            "同上，已修复。",
        );
        short.tags = vec!["发布".into()];

        assert!(
            information_score(&full) > information_score(&short),
            "完整结论的信息量必须高于一句「已修复」"
        );
    }

    #[test]
    fn information_score_caps_runaway_bodies() {
        // 把整个文件倒进记忆不该换来更高的分：600 字封顶。
        let mut huge = MemoryEntry::draft("a", "标题", MemoryKind::Knowledge, "字".repeat(20_000));
        huge.tags = vec!["t".into()];
        let mut compact = MemoryEntry::draft("b", "标题", MemoryKind::Knowledge, "字".repeat(600));
        compact.tags = vec!["t".into()];
        let (h, c) = (information_score(&huge), information_score(&compact));
        assert!((h - c).abs() < 1e-9, "超长正文与 600 字封顶应当同分，实际 {h} vs {c}");
    }

    #[test]
    fn absorb_takes_the_richer_body_and_keeps_the_short_one_otherwise() {
        // 场景一：旧的是「同上，已修复」，新的是带根因带路径的完整版 —— 必须吸收。
        let mut thin = MemoryEntry::draft("a", "发布脚本版本号", MemoryKind::Pitfall, "同上，已修复。");
        thin.tags = vec!["发布".into()];
        let rich = "根因是 `scripts/deliver.mjs` 读 `process.env.VERSION`，而 CI 只写了 \
                    `GITHUB_REF_NAME`；两处对齐后 `node scripts/deliver.mjs` 能打出 \
                    3.5.0-beta.1。不要在读不到时回落到 1.0.0。";
        assert!(should_absorb(&thin, rich), "半截结论必须被完整版覆盖");

        // 场景二：新的只是把同一句话换个说法、还更短 —— 保留原文，别把措辞洗一遍。
        let mut full = MemoryEntry::draft("b", "t", MemoryKind::Decision, "x".repeat(600));
        full.tags = vec!["a".into(), "b".into()];
        let shorter = "x".repeat(80);
        assert!(!should_absorb(&full, &shorter), "更短的改写不该覆盖原文");
    }

    #[test]
    fn keeper_prefers_the_richer_existing_entry() {
        // `dedup_keeper` 要查真实记忆根（`snapshot()`），单测环境不保证有，
        // 所以这里验的是它的**判据**：信息量高的一方必须在比较里胜出。
        let mut rich = MemoryEntry::draft("old", "t", MemoryKind::Decision, "x".repeat(800));
        rich.tags = vec!["a".into(), "b".into(), "c".into()];
        rich.evidence = vec!["ev".into()];
        let thin = MemoryEntry::draft("new", "t", MemoryKind::Decision, "x".repeat(40));
        assert!(
            information_score(&rich) > information_score(&thin),
            "库里的完整结论应当赢过刚写的一句短话"
        );

        // 平手时 `dedup_keeper` 返回 New（留新的）：同一句话的修订版通常带
        // 用户刚补的口径。这里验「同长度同标签 = 同分」，即平手真的会发生。
        let same_a = MemoryEntry::draft("a", "t", MemoryKind::Decision, "x".repeat(300));
        let same_b = MemoryEntry::draft("b", "t", MemoryKind::Decision, "x".repeat(300));
        assert!(
            (information_score(&same_a) - information_score(&same_b)).abs() < 1e-9,
            "内容等量时必须真的打成平手，否则「平手留新的」这条规则永远走不到"
        );
    }

    #[test]
    fn draft_is_recognised_by_layer_and_agent_together() {
        // 判据是「还在 l1 **且** 来源是 auto-extract」两件事同时成立。
        // 少任何一条都会误伤：只看 agent 会把升上去的 l2 结论继续当草稿藏起来
        // （它已经是结论了），只看 layer 会把人写的 l1 流水也当草稿。
        let mut draft = MemoryEntry::draft("a", "t", MemoryKind::Knowledge, "body");
        draft.agent = Some(model::AUTO_EXTRACT_AGENT.to_string());
        draft.layer = Layer::L1;
        assert!(is_auto_draft(&draft));

        // 升到 l2 之后不再算草稿 —— `agent` 字段保留来源，但身份变了
        draft.layer = Layer::L2;
        assert!(!is_auto_draft(&draft));

        // 人写的 l1 流水不是草稿
        let mut manual = MemoryEntry::draft("b", "t", MemoryKind::Daily, "body");
        manual.agent = Some("manual".to_string());
        assert!(!is_auto_draft(&manual));
    }

    #[test]
    fn promoted_draft_clears_path_so_it_leaves_the_l1_container() {
        // 升级必须清 `path`：`entry_path` 见到非空 path 就直接沿用，不清的话
        // 只改了 layer 字段、文件还留在 `l1/<agent>/<date>.md` 那个多节容器里，
        // 下一轮扫描又把它当流水读回来 —— 升级等于没发生。
        let mut draft = MemoryEntry::draft("a", "t", MemoryKind::Knowledge, "body");
        draft.agent = Some(model::AUTO_EXTRACT_AGENT.to_string());
        draft.layer = Layer::L1;
        draft.project = Some("demo".into());
        let before = store::rel(&store::entry_path(&draft, false));
        assert!(before.contains("/l1/"), "草稿本该落在 l1 容器里，实际 {before}");

        // 模拟 `promote_draft` 的两步：改层 + 清 path
        let mut promoted = draft.clone();
        promoted.layer = Layer::L2;
        promoted.path.clear();
        let after = store::rel(&store::entry_path(&promoted, false));
        assert!(after.contains("/l2/"), "升层后必须落到 l2 目录，实际 {after}");
        assert_ne!(before, after);
    }
}