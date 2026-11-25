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
pub mod dedup;
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
    ("dedup", "语义判重（L4）"),
    ("distill", "L2 蒸馏"),
    ("consolidate", "去重合并"),
    ("profile", "人格 / 偏好画像"),
    ("ingest", "会话增量入库"),
    ("index-scan", "索引自愈扫描"),
    ("cleanup", "异常记忆清理"),
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
                        entries.iter().filter(|entry| entry.is_trusted()).count()
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
    // `is_trusted` 而不是 `is_live`：关联图是检索时「顺便提示一下相关记忆」用的，
    // 把一条被人否掉的结论连进去，等于用它把用户引回错误。
    let live: Vec<&MemoryEntry> = entries.iter().filter(|entry| entry.is_trusted()).collect();
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
    // 这里刻意用 `is_live()` 而不是 `is_trusted()`：热力图回答的是「我哪天写了
    // 东西」，是**写作节律**的自我认知，不是「哪些结论还能用」。一条记忆被人
    // 判错之后，它仍然是「我那天写过的东西」，把它从格子里抹掉等于篡改历史 ——
    // 用户会以为那天什么都没干，实际那天他写了一条错的。
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
        // ── 显式读一条草稿时要自报身份 ──────────────────────────
        //
        // 调用方点名 `memory_get(id)` 是有意要看这一条，挡掉它反而会让模型以为
        // 「记忆丢了」，转而凭印象编。所以这里放行，但把 `draft: true` +
        // `layerCaveat` 一起塞进返回体：读到的模型/前端必须知道这不是既定事实。
        //
        // 相关记忆（下面的 `related`）不享受这个待遇 —— 那是顺手带出来的上下文，
        // 调用方根本没点名，多给一条草稿只会被当成事实引用。
        if is_auto_draft(entry) {
            value["draft"] = Value::Bool(true);
            value["layerCaveat"] = Value::String(
                "这条是自动抽取的候选，尚未被后续流水确认，也没有人采纳过。\
                 可以当成线索，不要当成已经成立的结论引用。"
                    .to_string(),
            );
        }
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
            .filter(|other| other.id != entry.id && other.is_trusted())
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
        // 草稿不进演化链：这是「结论怎么一步步变成现在这样」的账本，链上的每一
        // 节都会被当成「曾经成立过的结论」。而草稿是模型的一时猜测，从未成立过 ——
        // 把它挂进链里，用户会以为这件事当年被正式确认过。
        //
        // 注意这里过滤的是**链上其他节点**，不是 `target` 本身：显式点开一条草稿
        // 时，它自己应该出现在链里（否则页面全空，用户以为坏了），只是别的草稿
        // 不该混进来。
        "items": chain.into_iter()
            .filter(|entry| entry.id == target.id || !is_auto_draft(entry))
            .map(|entry| json!({
            "id": entry.id,
            "title": entry.title,
            "type": entry.kind.as_str(),
            "typeLabel": entry.kind.label(),
            "layer": entry.layer.as_str(),
            "draft": is_auto_draft(entry),
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
                .filter(|entry| entry.is_trusted())
                .filter(|entry| !is_auto_draft(entry))
                .filter(|entry| entry_project_slug(entry) == project.slug)
                .collect();
            entries.sort_by(|a, b| b.updated_at.cmp(&a.updated_at));
            // 与 `status()` 同一口径：画像（`kind = Profile`）是派生品，不混进
            // 「流水 / 沉淀」两个数字。项目卡上那个 `L2: N` 若含画像，用户每点一次
            // 「重生成画像」，通用项目那张卡的 L2 就涨十几条，而沉淀一条没多 ——
            // 与仪表盘「深层记忆」是同一种误导，只是换了个位置。
            // 恒等式：`l1 + l2 + profile == count`（三者都从 `entries` 里数，
            // 而 `entries` 已排掉草稿，所以这里不需要再扣）。
            let profile = entries.iter().filter(|entry| entry.kind == MemoryKind::Profile).count();
            let l1 = entries
                .iter()
                .filter(|entry| entry.layer == Layer::L1 && entry.kind != MemoryKind::Profile)
                .count();
            let l2 = entries
                .iter()
                .filter(|entry| entry.layer == Layer::L2 && entry.kind != MemoryKind::Profile)
                .count();
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
                // 画像单列（见上面 `l1` / `l2` 的注释）。老前端没有这个字段的
                // 消费方，多一个键不影响任何现有渲染。
                "profile": profile,
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
        .filter(|entry| entry.is_trusted())
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
        // 与列表里那张卡同一口径（画像单列，见上面 `profile` 的注释）。
        "l1": general_entries
            .iter()
            .filter(|entry| entry.layer == Layer::L1 && entry.kind != MemoryKind::Profile)
            .count(),
        "l2": general_entries
            .iter()
            .filter(|entry| entry.layer == Layer::L2 && entry.kind != MemoryKind::Profile)
            .count(),
        "profile": general_entries.iter().filter(|entry| entry.kind == MemoryKind::Profile).count(),
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

/// 这条记忆是否属于调用方指定的项目。
///
/// ── 为什么不能直接比字符串 ──────────────────────────────────
/// `entry.project` 写盘前经过 `classify_explicit → sanitize_slug`，永远是 slug
/// 形式（`aimod-cc-agent2api`）。而 `project` 参数来自三处**原始名字**：
/// 接口 `{"project":"aimod-cc/Agent2API"}`、项目页 `item.slug`（恰好是 slug）、
/// 自动 tick 的 `None`。直接 `entry.project.as_deref() == Some(slug)` 时，凡是
/// 非 slug 形式的名字**永远匹配不上任何条目**。
///
/// 症状不是报错，而是「过滤条件静默失效」：素材池空 → `extract` 以为流水太少
/// 直接跳过；已有结论清单空 → 模型看不到老话题，同一件事每轮都当新的抽一遍；
/// 候选互判重失效 → 重复 l2 又回来了。日志里一切正常，是最难查的一类 bug。
///
/// ── `None` 的语义 ──────────────────────────────────────────
/// `None` = 「自动 tick 没指定项目」，只比**通用**条目（`project` 为 `None`，
/// 磁盘上落在 `general/`），**不是**「所有项目」。跨项目合并会把两个项目的结论
/// 搓成一条 —— 那种错比重复难修得多。所以不做 `None => true` 的放宽。
///
/// 注意 `_global` 与 `general` 不是同一个项目：`slugify` 对空串兜底成 `_global`
/// （→ `general/` 目录），而真的有个项目叫 `general` 时 slug 就是 `general`
/// （→ `projects/general/`）。两者必须分开，不能拿「目录名」当判据。
pub(super) fn project_matches(entry_project: Option<&str>, wanted: Option<&str>) -> bool {
    let actual = entry_project
        .map(store::slugify)
        .unwrap_or_else(|| store::GLOBAL_SLUG.to_string());
    let target = wanted
        .map(store::slugify)
        .unwrap_or_else(|| store::GLOBAL_SLUG.to_string());
    actual == target
}

/// 素材池口径：`None` = 「不限定项目」= 全部项目都收。
///
/// 与 `project_matches` 的差别是 `None` 的含义，这一点必须分开写，不能合并：
/// 素材池（`extract` / `distill` 的输入、`supersede_recent` 的扫描范围）传
/// `None` 是「自动 tick 不知道项目，就全看」；而强化目标 / 候选判重传 `None`
/// 是「这次只处理通用项目」，**绝不能**因此跨到别的项目里去比正文 —— 那会把
/// 两个项目的结论搓成一条。
///
/// `Some` 的分支两处一致：都先把两边归一到 slug 再比。不归一化的话
/// `{"project":"aimod-cc/Agent2API"}` 这类原始名字会永远匹配不上任何条目，
/// 过滤条件静默失效（素材池空 → 任务以为没东西可做；清单空 → 重复重新长出来）。
pub(super) fn in_scope(entry_project: Option<&str>, scope: Option<&str>) -> bool {
    match scope {
        None => true,
        Some(_) => project_matches(entry_project, scope),
    }
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
        .filter(|entry| entry.is_trusted() && !is_auto_draft(entry))
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

/// 一次性解除**全部**画像条目的「手动置顶」（`pinned` 与 `PROFILE_PIN_TAG` 一起摘）。
///
/// ── 为什么需要一个批量出口 ──────────────────────────────────
/// 置顶是画像页上唯一「人的判断压过模型」的动作，语义是「重生成时保留这一条」。
/// 但它同时也是批量替换里唯一会让结果**定型**的因素：某一整批被钉住之后，
/// 每次重生成只会换掉剩下的非钉条目，四类的条数就锁死在同一个数字上 ——
/// 用户看到的是「每类都 8 条 / 置顶 16 条，点几次重新生成都不变」。
///
/// 逐条取消要点十几次（还得先切进「编辑条目」模式），所以给一个批量出口。
/// 它与「本批素材被模型完整覆盖」无关，纯粹是用户对存量数据的一次清理。
///
/// ── 只动画像 ────────────────────────────────────────────────
/// 普通记忆的 `pinned` 是浏览页的排序偏好，跟画像的重生成没有关系；对它们动手
/// 等于替用户改了一个他没提到的地方。判据因此收窄成「`kind == Profile` 且
/// 真的带钉（`pinned` 或标记任一）」，只在钉没了之后才跳过。
///
/// ── 为什么不动 `updated_at` ─────────────────────────────────
/// `profile_view()` 用「画像里最新的 `updated_at`」当作「上一版生成时刻」，
/// 据此算 `freshSinceProfile`（界面上那句「生成之后又新增了 N 条」）。解除置顶
/// 不是重新生成，把时间戳推到现在的后果是那句话直接变成 0 —— 一次清理顺手
/// 抹掉了「你的画像已经落后了」的唯一提示。
pub fn unpin_all_profile() -> Result<Value, String> {
    let mut cleared = 0usize;
    for mut entry in store::load_all() {
        if entry.kind != MemoryKind::Profile || !entry.is_trusted() {
            continue;
        }
        let tagged = entry.tags.iter().any(|tag| tag == PROFILE_PIN_TAG);
        if !entry.pinned && !tagged {
            continue;
        }
        entry.pinned = false;
        entry.tags.retain(|tag| tag != PROFILE_PIN_TAG);
        // 路径按「类型 / 层 / 项目 / 日期」算，这几项一个都没动，所以落点不变；
        // 仍然让 `store::save` 自己算路径，避免手写一份会漂的第二事实源。
        store::save(&entry)?;
        // 索引里存着 `pinned` 这一列（浏览页的筛选读它），不跟着改就会「文件说没钉、
        // 索引说钉着」。
        let _ = index::global().upsert(&entry);
        cleared += 1;
    }
    if cleared > 0 {
        bump_revision();
        crate::server::logging::log(
            "[Memory]",
            &format!("画像置顶：批量解除 {cleared} 条（下一次重生成会一并替换）"),
        );
    }
    Ok(json!({ "cleared": cleared, "profile": profile_view() }))
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
    // ── 画像不算「深层记忆」─────────────────────────────────────
    //
    // 画像（`kind = Profile`）是模型从深层记忆里归纳出的**派生品**。它自己也
    // 落在 l2 层（复用演化链、回收站、置顶那一套机制），但语义上和「流水反复
    // 确认后沉淀的结论」完全不是一回事：后者要攒够 `PROMOTE_AFTER_REINFORCE`
    // 条证据才升层，前者一次生成就有十几条。
    //
    // 混在一个计数器里的后果是真机实测到的：仪表盘报「深层记忆 68」，用户问
    // 「怎么一轮对话涨了 16 条」—— 而那 16 条全是刚生成的画像。数字本身没错，
    // 但它回答的不是用户问的那个问题，而且会让「反复出现才固化」这条承诺看起来
    // 是假的。
    //
    // 所以拆成两个数：`l2` 只数真沉淀，`profile` 单独给画像。
    //
    // 恒等式：`l1 + l2 + profile == trusted + 未被反驳的草稿`。
    //
    // 为什么右边不是直接写 `candidates`：`candidates` 的口径是 `is_live() && 草稿`，
    // **含被判错的草稿**；而左边三个桶都走 `is_trusted()`，把被判错的排掉了。
    // 一条草稿被标成「这条错了」之后，`candidates` 仍数它、左边不数它 ——
    // 右边会凭空多 1。正常库里没有「被判错的草稿」（草稿不进检索，用户看不到、
    // 也就无从反驳），但恒等式不能靠这个巧合成立，所以写成精确口径。
    // 真机核对（全部草稿可信时两者重合）：164 + 68 + 0 = 232，228 + 4 = 232。
    let profile = entries
        .iter()
        .filter(|entry| entry.is_trusted() && entry.kind == MemoryKind::Profile)
        .count();
    let l1 = entries
        .iter()
        .filter(|entry| entry.is_trusted() && entry.layer == Layer::L1)
        .filter(|entry| entry.kind != MemoryKind::Profile)
        .count();
    let l2 = entries
        .iter()
        .filter(|entry| entry.is_trusted() && entry.layer == Layer::L2)
        .filter(|entry| entry.kind != MemoryKind::Profile)
        .count();
    // 草稿（auto-extract 的 l1 候选）单独数出来：它在 `l1` 里，但语义上和
    // 用户自己写的流水完全不同 —— 它不参与检索、不进注入、不进核心记忆。
    // 仪表盘若把它算进「普通记忆」，用户看到的是一个虚高的数字，而那个数字
    // 对应的东西点不进任何地方（检索搜不到、注入里也没有）。
    let candidates = entries.iter().filter(|entry| entry.is_live() && is_auto_draft(entry)).count();
    // ── 被判定为错的条数：**包含在 `live` 里**，必须单列 ──────────
    //
    // 它没被取代、没进回收站、没过期，所以`is_live()` 为真 —— 但它不配当事实。
    // 仪表盘若要报「有效记忆」，直接拿 `live` 就是一个虚高的数（真机上会出现
    // 「132 条有效」其中 40 条已经被人否掉）。与 `candidates` 同一处理：单列
    // 出来，前端做 `live - candidates - refuted` 才是真正能当事实用的条数。
    //
    // 用 `is_refuted()` 而不是 `!is_trusted()`：后者把回收站与已取代也算进来，
    // 会和上面 `deleted` / `superseded` 两个数字重复计数。
    let refuted = entries.iter().filter(|entry| entry.is_live() && entry.is_refuted()).count();
    // ── 可信条数：真正能当事实用的那批 ──────────────────────────
    //
    // 「有效记忆」这个数字以前是前端拿 `live - candidates` 算的。加了 `refuted`
    // 之后那条算术会**重复扣减**：一条被反驳的草稿同时落在 `candidates` 和
    // `refuted` 两个桶里，前端减两次就少算一条。任何「先给三个桶、再让展示层
    // 相减」的设计都有这个毛病 —— 桶之间有交集，而展示层看不到交集。
    //
    // 所以由这里直接给出答案：活着、没被反驳、且不是自动抽取草稿。前端只显示，
    // 不再做任何加减。
    let trusted = entries
        .iter()
        .filter(|entry| entry.is_trusted() && !is_auto_draft(entry))
        .count();
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
        // 画像单列（见上面 `l2` 的说明），不混进「深层记忆」。
        "profile": profile,
        // 「草稿」是 l1 的子集（见上面的注释），单列给仪表盘用：
        // 界面要能说清「这 N 条是模型草稿，还没被确认」，而不是混进普通记忆。
        "candidates": candidates,
        // 被判定为错的条数（`live` 的子集，见上面的注释）。前端用它把
        // 「有效记忆」修正成 `live - candidates - refuted`。
        "refuted": refuted,
        // 真正能当事实用的条数：`live` 扣掉草稿与被反驳。恒等式是
        // `l1 + l2 + profile == trusted + 未被反驳的草稿` —— `l1` 含草稿、
        // `trusted` 排掉草稿，差的正好是草稿（精确口径见上面 `profile` 那段）。
        "trusted": trusted,
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
            // 高置信建议会不会自己落地。界面上「待确认 N 条」的措辞要按它变：
            // 自动裁决开着时，这个 N 是「需要你判断的」，关着时是「没人处理的」。
            "autoResolve": config::bool_of("review.autoResolve"),
            // 这里早先还有一个 `floor`（全局最低置信）。它已经删掉了 —— 那个数字
            // 与相似分是同一个量，叠在 classify / dedup 各自的判据上只会让高的
            // 那个悄悄吃掉低的那个。现在两类的风险分别由「路径证据」和
            // 「覆盖率」判定，不再下发一个既不准确也没人读的数字。
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
    for entry in entries.iter().filter(|entry| entry.is_trusted()) {
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
                    // 用 `is_trusted`：被人否掉的那条不该继续占着这个 hash。
                    // 否则「同一段话重新写一遍」会被判成重复、条目标是那条错记忆。
                    .filter(|entry| entry.is_trusted() && entry.layer == Layer::L1)
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
    //
    // 这里**一律入队**，不再看「要不要自动确认」的开关：早先 `review.autoConfirmClassify`
    // 开着时 `categorized_pending` 返回 false —— 建议既不进队列、也没被应用，
    // 记忆永远卡在 general，而界面上写着「已开启自动归类」。判据只留一份，
    // 在 `decide` 里；入队只是留档，落地由 `auto_resolve_ids` 决定。
    let pending_classify = if categorized_pending(&classified) {
        index::global().enqueue(
            "classify",
            &id,
            &json!({
                "suggestion": classified.suggestion,
                "confidence": classified.confidence,
                "cwd": input.cwd,
            }),
            classified.confidence,
        )
    } else {
        None
    };
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
        // 新写的条目不可能已经被判错：反驳是对既有条目的判断，只能走 refute_entry。
        refuted_at: None,
        refuted_reason: String::new(),
        refuted_by: None,
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

    // ── 归类建议：入队只是留档，够格的当场落地 ────────────────────
    // 必须在 `save` 之后：`decide` 的 classify 分支要在快照里查得到
    // 这条目标记忆，而它在写盘之前根本不存在。
    if let Some(queued) = pending_classify {
        if auto_resolve_ids(&[queued]).applied > 0 {
            // 自动归入会搬文件、换 project。返回的 entry 必须是搬完之后的形态，
            // 否则调用方拿到 `project: null`，而记忆已经在 `projects/<slug>/` 里了
            // —— 同一个响应里两处说法打架，任何拿它做后续判断的调用方都会错。
            if let Some(fresh) = store::snapshot().iter().find(|item| item.id == entry.id).cloned() {
                entry = fresh;
            }
        }
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

    // ── L2 近似去重：入队留档，落不落地由 `decide` 一处判 ────────────
    //
    // 早先这里自带一段 `review.autoConfirmDedup && score >= autoMergeThreshold`
    // 的判定，与队列裁决各走一条路。两条路对「什么算够格」的口径只要有一点
    // 不同，就会出现「巡检明明 0.93 却没自动合并」这种谁也解释不清的场面。
    // 现在：**一律入队**（留档 + 可审计），是否当场落地问 `decide`，
    // 落地跑的是和人工批准同一个 `apply_verdict`。
    if config::bool_of("dedup.enabled") && config::bool_of("dedup.l2.enabled") && layer == Layer::L2 {
        if let Some((candidate, score)) = near_duplicate(&entry) {
            // 谁留下不由「谁更新」决定，由信息量决定 —— 见 `information_score`。
            // 原来的规则是「新的必赢」，真实场面是隔天的「同上，已修复」把一份
            // 带根因带路径的完整结论顶进回收站，越整理越空。
            let keep = dedup_keeper(&entry, &candidate);
            let queued = index::global().enqueue(
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
            // 只处理**刚入队的这一条**，不顺手扫全队列：批量导入是几千次写，
            // 每次都扫队列会把 O(n) 摊成 O(n²)。返回 `Some(id)` 只在这一条真的
            // 被并进既有条目时才给 —— 那正是调用方需要换掉响应 id 的唯一情形。
            if let Some(merged_into) = queued.and_then(auto_resolve_one) {
                // 这一轮写进去的条目当场就被判成旧结论的旁支。返回既有条目的 id，
                // 否则调用方拿到的是一个「写了但搜不到」的 id（它已被 superseded
                // 过滤掉），会以为记忆丢了。
                crate::server::logging::log(
                    "[Memory]",
                    &format!("写入并入 {merged_into}（旧条目信息量更大，本次内容标为被取代）"),
                );
                return Ok(json!({
                    "id": merged_into,
                    "entry": Value::Null,
                    "duplicate": true,
                    "superseded": 0,
                    "mergedInto": merged_into,
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

/// 模糊归类的建议要不要进队列。
///
/// 只看「这条建议有没有内容可批」，不看「要不要自动确认」：`suggest` 才进 ——
/// 其余 origin 都是确定结论（显式指名 / git 身份 / 已允许自动建项目）。
///
/// ── 为什么把开关从这里拿掉 ──────────────────────────────────
/// 早先这里还串了 `!config::bool_of("review.autoConfirmClassify")`，于是那个
/// 开关开着时：建议不入队（`enqueue` 不执行），同时也没有任何地方去应用它 ——
/// 记忆永远留在 general，而设置页写着「已开启自动归类」。开关是真的，动作是
/// 不存在的。现在开关只作用于 [`decide`]，入队一律发生（留档）。
fn categorized_pending(classified: &Classified) -> bool {
    classified.origin == "suggest" && classified.suggestion.is_some()
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
        .filter(|entry| entry.is_trusted() && entry.hash == hash)
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

/// 一条记忆与另一条的「整体近似度」：正文 Dice、正文编辑距离、标题相似度三项加权。
///
/// ── 为什么必须只有这一份实现 ────────────────────────────────
/// 判重有两个入口，判的都是同一件事「这两条是不是在讲同一件事」：
///   ① `write_inner` 落地新条目时（`near_duplicate`）
///   ② `apply_derived` 落蒸馏结论前（先看能不能改成强化既有结论）
/// 早先这里各写了一套：① 用 `dice/wEdit/wTitle` 加权，② 用 `dice+edit` 的均值
/// 且不读标题。两边同阈值（0.72）却不同算法，于是同一条结论可以「②判无关 →
/// 新建」，紧接着「①判近似 → 入队 → 当场合并」（`review.autoConfirmDedup` 开着，
/// 覆盖率也过线时），把刚落地的那条埋掉）。症状是「明明合并过，怎么又出来两条」，
/// 且两处日志分别看都自洽 —— 只有把两份算法并排放才能发现。
///
/// 抽成一个纯函数之后，两处口径永远一致；想改判据只改这里。
fn pair_similarity(entry: &MemoryEntry, other: &MemoryEntry) -> f64 {
    let w_dice = config::number("dedup.l2.wDice");
    let w_edit = config::number("dedup.l2.wEdit");
    let w_title = config::number("dedup.l2.wTitle");
    let dice = dice_coefficient(&entry.body, &other.body);
    let chars = entry.body.chars().count().max(other.body.chars().count()).max(1);
    let edit = 1.0 - (levenshtein(&entry.body, &other.body) as f64) / chars as f64;
    let title_score = similarity(&entry.title, &other.title);
    // 三项各算各的再加权：早先这里把 Dice 与编辑距离都算成了 `similarity()`
    // 的均值，等于同一个数用了两遍，权重配置（wDice / wEdit）实际没生效
    let score = (dice * w_dice + edit * w_edit + title_score * w_title)
        / (w_dice + w_edit + w_title).max(0.001);
    score.clamp(0.0, 1.0)
}

/// L2 近似重复的候选：词面相似度（Dice + 编辑距离）超过 `dedup.l2.candidateThreshold`。
///
/// 为什么不用向量：这里不想为了「找相似标题」引入 embedding 模型与一次额外
/// 的模型调用；而沉淀结论的标题通常很短，词面相似度已经能抓住绝大多数重复。
/// 阈值之上只产建议（`review_queue`），裁决由 [`decide`] 按覆盖率做 —— 够了才
/// 合并，各有独有内容就连边，这条不成立就作废。
fn near_duplicate(entry: &MemoryEntry) -> Option<(String, f64)> {
    let threshold = config::number("dedup.l2.candidateThreshold");
    let mut best: Option<(String, f64)> = None;
    // 快照先绑到局部变量：`for other in store::snapshot().iter()` 虽然临时值会
    // 活到循环结束（只调一次），但读代码的人得先知道这条临时值生命周期规则才能
    // 确信它没在每轮重读 55 MB。绑出来是零成本的，且意图直接可读。
    let all = store::snapshot();
    for other in all.iter() {
        if other.id == entry.id || !other.is_trusted() || other.layer != Layer::L2 {
            continue;
        }
        if other.kind != entry.kind {
            continue;
        }
        let score = pair_similarity(entry, other);
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

/// 短边被长边覆盖的比例：较短那条的 token 有多少比例出现在另一条里。
///
/// ── 为什么需要它，`pair_similarity` 不够吗 ─────────────────────
/// 不够，而且差得很远。`pair_similarity` 是「两段文本像不像」，它对**长度差**
/// 不敏感：一段 40 字的结论和一段 400 字的完整复盘，只要共用的词够多，Dice 与
/// 编辑距离都能给出 0.75 左右。可真机抽样里这种配对是这两类：
///
///   同一件事的两次表述   「修改范围锁定为定点字节补丁」vs「坚持最小字节改动」
///   只是相关             「事故复盘清单化」vs「前端选型单文件离线 HTML」
///
/// 前者该合、后者不该合，而相似分给它们的分数几乎一样（0.73 / 0.75）。区别在
/// **谁能被谁解释掉**：前者短的那条整段落在长的那条里，后者一半的词是对方根本
/// 没提的另一件事。所以判据不是「像不像」，是「短的那条是不是长的那条的子集」。
///
/// ── 为什么取短边 ────────────────────────────────────────────
/// 合并的损失在**被取代方**：它的正文会退出检索（见 `apply_dedup`）。所以要看
/// 它有多少内容在幸存者里已经有 —— 这个比例高，它消失才不丢东西。反过来若按
/// 长边算，「400 字里覆盖了 40 字」会给出 0.1，把该合的全挡在门外。
///
/// ── 为什么用二元组而不是 `index::normalize` 的单字 ────────────
/// 检索侧把 CJK 拆成单字是对的（FTS5 的短语匹配靠它），但拿单字算覆盖率会
/// **虚高到失去区分力**：任意两段中文技术笔记都共用「的/是/在/个/数/据」这些
/// 高频字，真机实测 44 对里最不像的一对（「ATS/PSCADA 无显示结论」vs
/// 「PSCADA 供电显示语义」，两者几乎无关）单字覆盖率都有 0.41，中位数 0.75 ——
/// 这个数值区间里「同一件事」和「同领域」完全混在一起，拿它当闸门等于没闸门。
/// 二元组是「词组级」的重合：换个说法（「最小字节补丁」→「最小字节改动」）
/// 仍然有大量共同二元组，而换了主语（「事故复盘」→「前端选型」）几乎没有。
/// 实测同一批数据：最不像的 0.27，最像的 0.93，中位数 0.62，区分度够用。
///
/// 中英混排时分法不同：ASCII 词整词保留（`content-length` 不会被切成两半），
/// CJK 连续段切二元组（单字段落补一个单字 token，否则一个字永远算不出覆盖率）。
fn coverage_ratio(left: &str, right: &str) -> f64 {
    let a = token_set(left);
    let b = token_set(right);
    if a.is_empty() || b.is_empty() {
        return 0.0;
    }
    // 谁短算谁：无方向的「这两条是不是在讲同一件事」。
    let (short, long) = if a.len() <= b.len() { (&a, &b) } else { (&b, &a) };
    let hit = short.iter().filter(|token| long.contains(*token)).count();
    hit as f64 / short.len() as f64
}

/// 有向覆盖率：`subject` 的内容有多少比例被 `explainer` 讲到。
///
/// ── 为什么「谁短算谁」在有方向的判据上是错的（2026-11-24）──────────
/// `coverage_ratio` 取短边做分母，它回答的是「这两条是不是在讲同一件事」——
/// 那是个无方向的问题，取短边是对的。
///
/// 但 `supersede` 问的是有方向的问题：**旧条的内容有没有被新条解释掉**。
/// 新条写一句「插件要重写 host。」（5 个 token），旧条有 45 个 token 讲完整
/// 因果链 —— 取短边算出来 0.600（过闸 → 判失效 → 旧条正文被并进这条短句后
/// 退出检索，因果链就此消失）；取旧条做分母算出来 0.067（不过闸 → 旧事实保持
/// 有效，正确）。
///
/// 一句话结论：`supersede` 的风险是「短的新结论埋掉长的旧结论」，所以分母必须
/// 是**被取代的那条**，不能是「两条里较短的那条」。
///
/// `dedup` 仍然用对称的那个：两条地位对等，谁更全只有覆盖率知道。
fn coverage_of(subject: &str, explainer: &str) -> f64 {
    let a = token_set(subject);
    let b = token_set(explainer);
    if a.is_empty() || b.is_empty() {
        return 0.0;
    }
    let hit = a.iter().filter(|token| b.contains(*token)).count();
    hit as f64 / a.len() as f64
}

/// 覆盖率只算**正文主体**，不算「证据：」那一段。
///
/// ── 为什么必须剔掉证据段 ────────────────────────────────────────
/// 证据段是一串 `mem_20261011_...` 形式的 id。同一批流水压出来的记忆，证据列表
/// 高度重叠 —— 真机 44 对里有 6 对的证据列表几乎逐字相同，包括「事故复盘清单化」
/// vs「前端选型单文件离线 HTML」这种**毫不相干**的配对：它们的证据段一样，于是
/// 整段覆盖率被抬到 0.378，比真正同一件事的 #65（0.371）还高。
///
/// 也就是说，把证据段算进去，覆盖率就不再回答「这两条说的是不是同一件事」，
/// 而是回答「它们是不是同一批来的」—— 而后者恰恰恒为真。实测每一对都被抬高
/// 0.03~0.34，最离谱的 #70 抬了 0.337。这个数值一旦当真，就是把不相干的两条
/// 合在一起，理由还是「它们出处相同」。
///
/// 剔掉之后，同一批数据的分布才有断层：两条真正无关的落在 0.026 / 0.042，
/// 其余同事实的两次表述集中在 0.20~0.81。这才是一把能用的尺子。
fn content_coverage(left: &str, right: &str) -> f64 {
    coverage_ratio(split_evidence(left).0, split_evidence(right).0)
}

/// 有向版 `content_coverage`：`subject` 的正文有多少被 `explainer` 讲到。
///
/// 用于 `supersede`（「旧条被新条解释掉多少」）。见 [`coverage_of`] 里那段
/// 「为什么短边当分母是错的」。
fn content_coverage_of(subject: &str, explainer: &str) -> f64 {
    coverage_of(split_evidence(subject).0, split_evidence(explainer).0)
}

/// 文本 → token 集合（去重后）。ASCII 整词 + CJK 二元组。
fn token_set(text: &str) -> std::collections::HashSet<String> {
    let lowered: String = text.chars().flat_map(|ch| ch.to_lowercase()).collect();
    let mut tokens: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut ascii = String::new();
    let mut cjk: Vec<char> = Vec::new();
    let flush_ascii = |buffer: &mut String, out: &mut std::collections::HashSet<String>| {
        if buffer.chars().count() >= 2 {
            out.insert(buffer.clone());
        }
        buffer.clear();
    };
    let flush_cjk = |buffer: &mut Vec<char>, out: &mut std::collections::HashSet<String>| {
        if buffer.len() == 1 {
            out.insert(buffer[0].to_string());
        }
        for pair in buffer.windows(2) {
            let mut token = String::new();
            token.push(pair[0]);
            token.push(pair[1]);
            out.insert(token);
        }
        buffer.clear();
    };
    for ch in lowered.chars() {
        if model::is_cjk(ch) {
            flush_ascii(&mut ascii, &mut tokens);
            cjk.push(ch);
        } else if ch.is_alphanumeric() || ch == '_' || ch == '-' || ch == '.' {
            flush_cjk(&mut cjk, &mut tokens);
            ascii.push(ch);
        } else {
            flush_ascii(&mut ascii, &mut tokens);
            flush_cjk(&mut cjk, &mut tokens);
        }
    }
    flush_ascii(&mut ascii, &mut tokens);
    flush_cjk(&mut cjk, &mut tokens);
    tokens
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
            // ── 正文改了，「这一对判过了」就不再成立 ────────────────────
            // 巡检（`dedup_scan`）跳过已判配对的判据是「任一端 `dedup_status`
            // 是 `related` 且两点之间有边」。那个痕迹说的是「我问过这两条，结论
            // 是它们各讲各的」—— 而这句话只在**当时的正文**上成立。正文被改写
            // 之后它们可能真的变成同一件事，痕迹却还挂着，于是这对永远不再被
            // 提问。注释里写着「写正文的条目一律是 `Unique`，痕迹自然失效」，
            // 而这条路径根本没人重置它 —— 承诺没兑现。
            //
            // 只清 `Related`：`Merged` 是真实演化关系（`superseded_by` 在），
            // 清掉会让它看起来像一条从没被处理过的重复；`Pending` 是队列里的
            // 状态，与正文无关。
            if entry.dedup_status == DedupStatus::Related {
                entry.dedup_status = DedupStatus::Unique;
            }
        }
    }
    if let Some(kind) = patch.get("type").and_then(Value::as_str) {
        entry.kind = MemoryKind::parse(kind);
    }
    // 这一轮是不是「人工把一条 l1 抬成了 l2」。必须在换层字段的处理里就地记下，
    // 不能事后拿 `previous.layer` 比 —— 那个克隆发生在改动之后，永远等于新层。
    let mut promoted_by_hand = false;
    // ── 换层：这是 `update()` 里唯一能凭一个 HTTP 请求造出 l2 的字段 ──────
    //
    // 三处收紧，缺一不可：
    //
    // ① 严格解析。`Layer::parse` 把任何非 "l2" 的输入吞成 l1 —— 前端多传一个
    //    空格之外的东西（"L2 " 能过、拼成 "l3" 不能）就把一条沉淀结论静默
    //    降成流水、塞回 l1 容器。读磁盘时兜底成 l1 是止损，读意图时必须报错。
    //
    // ② l1 → l2 追加 `manual:` 锚点。`promote_draft` 与 `reinforce_entry_with`
    //    都留这个锚点，`update` 不留的话，界面上「被确认 N 次」会少算这一次，
    //    同一件事走两条路得到两种证据记录。
    //
    // ③ 升层也过一遍近似判重。`write_inner` 的 `near_duplicate` 只在
    //    「新条目落地」时跑；`update` 直接把一条 l1 抬成 l2，等于绕过整个
    //    准入。这里不改判重结论（人工改层就是要改），只把命中记进日志并
    //    入队 —— 让下一次裁决能看到「你刚手升的这条其实和 X 是同一件事」。
    if let Some(layer) = patch.get("layer") {
        let Some(raw) = layer.as_str() else {
            return Err("layer 必须是字符串 \"l1\" 或 \"l2\"".into());
        };
        let Some(target) = Layer::try_parse(raw) else {
            return Err(format!("未知层级 {raw:?}（只接受 l1 / l2）"));
        };
        // ── 被反驳的条目不许靠改层「复活」────────────────────────
        //
        // 这条与 `promote_draft` 里那道拒绝是同一条理由：改层说的是「它该在
        // 哪一层」，而「它是不是错的」是另一条轴，改层回答不了。一条被判错的
        // 结论若被抬成 l2，它仍然是错的，但界面上会显示「深层记忆」——
        // 用户以为它已经洗白了，实际检索仍然跳过它（`is_trusted()` 拦着），
        // 于是出现「我升了层，为什么还是搜不到」这种查不出原因的症状。
        //
        // 唯一的复活入口是 `unrefute_entry`：那是一个明确的、会被记日志的动作。
        if target == Layer::L2 && entry.is_refuted() {
            return Err(
                "这条已被判定为错，不能靠改层复活；先撤销「这条错了」的判定".into(),
            );
        }
        // 原层必须在**改之前**记下来：下面的 `previous` 是改完才克隆的，
        // 拿它比「是不是刚升上来的」永远成立不了（那是个恒假的判断）。
        if target == Layer::L2 && entry.layer != Layer::L2 {
            promoted_by_hand = true;
            let cue = format!("manual:{id}");
            if !entry.evidence.iter().any(|known| known.as_str() == cue) {
                entry.evidence.push(cue);
            }
        }
        entry.layer = target;
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
    // ── 手动升 l2 的旁路：只提示，不阻止 ────────────────────────────
    //
    // 人工改层是明确意图，不该被自动判重拦住（拦住的话用户点两次都不生效）。
    // 但把「刚手升的这条其实和 X 是同一件事」留在裁决队列里，下一次整理才
    // 看得见 —— 否则这条旁路就成了重复条目的合法入口。
    //
    // 只在「升到 l2」时做，且不入队 l1：l1 之间重复是常态（流水本来就该重复）。
    if entry.layer == Layer::L2
        && promoted_by_hand
        && config::bool_of("dedup.enabled")
        && config::bool_of("dedup.l2.enabled")
    {
        if let Some((candidate, score)) = near_duplicate(&entry) {
            // 谁活下来仍按信息量裁决，而不是「刚手升的那条必赢」—— 用户把一条
            // 半截流水抬成 l2，库里那条完整结论不该因此被埋。
            //
            // `keep` 的取值空间与 `write_inner` 保持一致：`"candidate"` 表示
            // 载荷里的 `candidate` 活、队列条目（`row.target`，这里是刚手升的
            // 这条）被取代；其余（含 `"new"`）表示反过来。`review_resolve` 只认
            // `"candidate"`，所以这里必须显式写，不能靠默认值猜。
            let keep = if dedup_keeper(&entry, &candidate) == DedupKeeper::Candidate {
                "candidate"
            } else {
                "new"
            };
            let queued = index::global().enqueue(
                "dedup",
                &entry.id,
                &json!({
                    "candidate": candidate,
                    "score": score,
                    "keep": keep,
                    "origin": "manual-promote",
                }),
                score,
            );
            // 只入队不裁决 = 留一行等人点。判据对这条同样有答案（合并 / 连边 /
            // 作废），所以当场问一次，和写入路径同一个 `decide`。人把一条半截
            // 流水手升成 l2 是明确意图，但它和既有结论的关系不该变成下一件
            // 「等你来点」的事。
            if let Some(id) = queued {
                auto_resolve_ids(&[id]);
            }
            crate::server::logging::log(
                "[Memory]",
                &format!("手动升层 {} → l2，与 {candidate} 近似（{score:.2}），已按判据裁决", entry.id),
            );
        }
    }
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
/// ── 为什么先按 hash 分组再进近似判定 ────────────────────────
/// 精确重复（同一段话导了两遍）占绝大多数，且 O(n) 就能抓干净。先分组把这一
/// 大批一次性解决掉，剩下的才进 O(n²) 的词面比较 —— 否则每次巡检都要对所有
/// 活条目两两算 Dice + Levenshtein，库上千条就会卡住整个进程。
///
/// ── 巡检产出的建议里，哪些会当场落地 ────────────────────────
/// 全部先入队（留档），然后跑一次 [`auto_resolve`]：每一对都由 [`pair_outcome`]
/// 给出终态 —— 短边被对方解释掉（覆盖率过线）就合，够像但各有独有内容就只连边、
/// 两条都留，够不上连边门槛的才按「这条建议不成立」作废。
///
/// 队列不承担「等人拍板」的职责：判据从条目状态重算得出确定答案，把确定答案
/// 留在队列里等于让「待确认 N 条」永远降不到 0（用户的实际用法是全选→确认）。
///
/// 早先这里写的是「近似项要过 `autoMergeThreshold`（默认 0.9），0.72~0.89 那段
/// 留给人」。真机上那段正好是全部 44 对 —— 队列清不完，而它们恰恰是最该合的。
pub fn dedup_scan() -> Value {
    if !config::bool_of("dedup.enabled") {
        return json!({
            "skipped": true,
            "reason": "去重总开关关着（dedup.enabled），巡检不跑。",
            "scanned": 0, "exact": 0, "near": 0, "queued": 0,
        });
    }
    // 巡检只扫「可以当事实用」的条目。被反驳的条目进来会有两个坏后果：① 它可能
    // 被选成「留哪条」的正主，等于把一条被否掉的结论重新立为标准；② 它与真正
    // 正确的结论之间的相似度会被反复入队，用户每次裁决都在处理同一桩旧账。
    let live: Vec<MemoryEntry> = store::snapshot()
        .iter()
        .filter(|entry| entry.is_trusted())
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
        // 阈值走配置，打分走 `pair_similarity` —— 巡检、写入、派生三个入口必须
        // 用**同一条判据**。早先这里自己又写了一遍加权（与 `near_duplicate`
        // 数值相同但独立维护），任何一方改权重就会让「写入时没判成重复、巡检
        // 却判成」这种不一致出现，用户会怀疑两边都在乱来。
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
                // ── 判过的配对别再问第二遍 ────────────────────────────────
                // `relate_pair` 留下的痕迹是「连了一条 `same_topic` 边 + 被取代方
                // 标 `related`」。两个条件一起看，是因为单看任何一个都会误伤：
                //
                //   * 只看边 —— `relink` 按共享标签就给同一项目里最近写入的几条
                //     连线，新配对被它提前连上的概率很高，只看边等于把新配对
                //     永久挡掉；
                //   * 只看 `related` —— 那条状态只标在被取代方身上，而下一轮扫描
                //     里这对的左右顺序可能反过来（由 `created_at` 决定），于是又
                //     进来一遍。
                //
                // 两个都成立 = 「这一对确实判过、且痕迹还在」，跳过它。判过之后
                // 真的重复了怎么办：写正文的条目一律是 `Unique`，痕迹自然失效，
                // 下一轮照常提问。
                let judged = (l2[i].dedup_status == DedupStatus::Related
                    || l2[j].dedup_status == DedupStatus::Related)
                    && index::global().linked(&l2[i].id, &l2[j].id);
                if judged {
                    continue;
                }
                let score = pair_similarity(l2[i], l2[j]);
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
    // ── 巡检结束后把队列裁干净 ──────────────────────────────────
    // 不这么做的话，「巡检」这个动作只是把库里已有的重复从「看不见」变成
    // 「队列里躺着」，用户点一次巡检得到的仍然是一个更长的队列。`decide` 对每一
    // 对都有答案（合并 / 连边 / 作废），所以这一轮之后队列里不该再留下本次巡检
    // 产生的东西。
    let auto = auto_resolve(500);
    let applied = auto.get("applied").and_then(Value::as_u64).unwrap_or(0);
    let retired = auto.get("retired").and_then(Value::as_u64).unwrap_or(0);
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
        "autoApplied": applied,
        "autoRetired": retired,
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

/// 裁决一条建议（人工入口）。`approve` 为真时**按判据**落盘 —— 不是「无视判据
/// 执行」：
///
/// * `classify` —— 把记忆搬进建议的项目
/// * `dedup` —— 覆盖率够就并正文后标取代，够像但各有独有内容就只连边，不够就作废
/// * `supersede` —— 同上，方向由载荷固定
///
/// 拒绝只改队列状态，不动数据。**永不物理删除**：任何裁决都可以反悔。
///
/// ── 为什么「批准」不等于「照做」──────────────────────────────
/// 这条队列的正常状态是空的（判据对每一行都有结论，见 [`decide`]）。所以出现在
/// 这里的一行，要么是某一类自动裁决被用户关掉了，要么是落地失败了 —— 两种情况下
/// 把「批准」做成「无条件执行」都会重新打开批量埋记忆的口子：真机上的用法是
/// 全选 → 确定，人不逐条看。批准的意思是「我同意按判据处置」，判据说这条建议
/// 不成立（一端没了 / 覆盖不到）时就什么都不动，只留下那个理由。
pub fn review_resolve(id: i64, approve: bool) -> Result<Value, String> {
    // 按 id 精确取：`review_list` 会截断到 500 条，队列积压时点「批准」会报
    // 「找不到待确认项」—— 而那条明明就在页面上。
    let row = index::global()
        .review_get(id)
        .ok_or_else(|| format!("找不到待确认项 {id}"))?;
    if row.status != "pending" {
        return Ok(json!({ "id": id, "status": row.status, "already": true }));
    }
    // ── 人点了「批准」，但判据说这条不成立 ─────────────────────
    // 这种情况要如实记成「作废 + 理由」，而不是「approved + manual」。
    // 后者会让审计链说谎：一条什么都没做的建议在库里显示「人工已批准」，
    // 事后谁也查不出它其实是被判据否掉的。
    let voided = if approve { apply_verdict(&row)? } else { None };
    if let Some(reason) = voided {
        index::global().review_resolve(&[id], "rejected", Some(reason));
        bump_revision();
        return Ok(json!({ "id": id, "approved": false, "type": row.kind, "reason": reason }));
    }
    index::global().review_resolve(
        &[id],
        if approve { "approved" } else { "rejected" },
        Some(if approve { "manual" } else { "manual-reject" }),
    );
    bump_revision();
    Ok(json!({ "id": id, "approved": approve, "type": row.kind }))
}

/// 把一条建议落盘，**动作由传进来的结论决定**。
///
/// 这是「执行」那一半：[`decide`] / [`pair_outcome`] 负责判，这里只负责照做。
/// 拆开的理由是判据只能有一处 —— 如果执行时再算一遍，两次计算之间快照可能已经
/// 变了（一轮批扫里前一行刚合并掉这一行的一端），于是「判据说合并」和「执行时
/// 说要作废」同时成立，而错的那次会静默生效。
///
/// 人工批准与自动批准共用这一份：两条路各写一遍的话，「自动合并的方向」与
/// 「人工批准的方向」迟早会漂开，而漂开的表现是「我点了批准，结果要留的那条
/// 被埋了」。
///
/// `outcome` 只对配对型（`dedup` / `supersede`）有意义：那两类的动作由裁决的
/// 终态决定（并正文 / 只连边 / 什么都不做）。其余三类（归类搬家、蒸馏新增、
/// 合并新增）不需要终态，传 `None`。参数做成 `Option` 而不是「随便传个 Merge
/// 占位」，是因为后者会让读代码的人以为「归类也走合并那条路」。
fn execute(row: &index::ReviewRow, outcome: Option<PairOutcome>) -> Result<(), String> {
    if matches!(row.kind.as_str(), "dedup" | "supersede") {
        let Some(outcome) = outcome else {
            // 配对型却没给终态：调用方写错了，不是数据问题，直接报出来。
            return Err(format!("配对型建议 {} 缺少裁决结果", row.kind));
        };
        return match outcome {
            PairOutcome::Merge => {
                if row.kind == "supersede" {
                    let new_id = pair_this_id(row).unwrap_or_default();
                    apply_supersede(new_id, pair_other_id(row))
                } else {
                    let candidate = pair_other_id(row).to_string();
                    // 方向由载荷里的 `keep` 决定：`candidate` = 库里那条更全，
                    // 把 `target`（新写的）并进去；`new` = 反过来。
                    if row.payload.get("keep").and_then(Value::as_str) == Some("candidate") {
                        apply_dedup(&candidate, &row.target)
                    } else {
                        apply_dedup(&row.target, &candidate)
                    }
                }
            }
            // 两条各有独有内容：连一条边，两份正文都留着。纯新增、可反悔。
            PairOutcome::Relate => {
                relate_pair(pair_this_id(row).unwrap_or_default(), pair_other_id(row))
            }
            // 判据说这条建议不成立（一端没了 / 已处理过 / 覆盖不到 / 分数不够）：
            // 人点了「批准」也不该凭空执行一个会丢内容的动作。
            PairOutcome::Retire(_) => Ok(()),
        };
    }
    match row.kind.as_str() {
        "classify" => {
            let slug = row
                .payload
                .get("suggestion")
                .and_then(Value::as_str)
                .unwrap_or_default();
            if slug.is_empty() {
                return Ok(());
            }
            apply_classification(&row.target, slug)
        }
        "distill" | "consolidate" => apply_derived(&row.payload),
        _ => Ok(()),
    }
}

/// 人工批准：先问判据，再照做。返回 `Some(理由)` 表示**判据说这条不成立**，
/// 调用方要把队列行记成「作废 + 理由」，而不是「人工已批准」。
///
/// ── 为什么人工批准也要过判据（2026-11-24）─────────────────────
/// 用户真实的操作是「全选 → 确定」，所以这条路上有多少人会逐条看，答案就是零。
/// 原来它对 dedup 一律合并、对 supersede 一律标失效 —— 于是那些「相似但各有
/// 独有内容」的配对被人手批量埋掉，正是自动裁决特意避开的事。
///
/// 现在人点「批准」= 「我同意按判据处置」，而不是「无视判据直接执行」：判据说
/// 该合并就合并，说只该连边就连边，说这条不成立就什么都不动、只留理由。
/// 人想强行合并的场合仍然有 —— 逐条编辑正文，那才是亲手表达。
fn apply_verdict(row: &index::ReviewRow) -> Result<Option<&'static str>, String> {
    // 没有 `execute` 分支能落这类建议：批准它只会把队列行记成「人工已批准」，
    // 而记忆一个字节都不会变。如实记成作废 + 理由，审计链才不撒谎。
    if !is_managed_kind(&row.kind) {
        return Ok(Some("auto-unknown-kind"));
    }
    let outcome = if matches!(row.kind.as_str(), "dedup" | "supersede") {
        Some(pair_outcome(row, &store::snapshot()))
    } else {
        None
    };
    if let Some(PairOutcome::Retire(reason)) = outcome {
        return Ok(Some(reason));
    }
    execute(row, outcome)?;
    Ok(None)
}

/// 这个 kind 有明确的入队方与落地动作。
///
/// 不在这张表里的建议（老版本留下的、别的模块塞进来的）既没有开关管它、也没有
/// `execute` 分支能落它 —— 无论自动还是人工都只会让它「看起来处理过，实际什么
/// 都没发生」。所以它的终局只能是作废，理由写清楚，事后能查、能反向批准。
///
/// `conflict` 也不在这张表里，但它走的是自己那张 `sync_conflict` 表与独立的
/// 冲突面板，本来就不该出现在 `review_queue`。
fn is_managed_kind(kind: &str) -> bool {
    matches!(kind, "classify" | "dedup" | "supersede" | "distill" | "consolidate")
}

// ── 自动裁决 ─────────────────────────────────────────────────

/// 一条建议现在该怎么办 —— 全仓唯一的裁决点。
///
/// ── 为什么把所有判据合成一个函数 ──────────────────────────────
/// 原来这件事散在四处：`auto_eligible` 判「能不能自动落」、`retire_reason_of`
/// 判「该不该作废」、`dedup_auto_verdict` / `supersede_auto_verdict` 各判各的
/// 动作。四个函数读同一份数据、回答同一件事，只要有一处判据漂了，表现就是
/// 「判据说能落的落不了、该清的清不掉」—— 真机上 2 条 supersede 挂了 7 小时、
/// 44 对 dedup 一条都没自动，都是这么来的。
///
/// 现在一次裁决给出唯一答案，调用方只负责执行：全队列扫描、写入路径、人工
/// 批准三条路都读这一份，不存在「两边判据不一致」这种状态。
///
/// ── 为什么是「按类型分档」而不是一个总阈值 ────────────────────
/// 四类建议的写操作后果不是一个量级：
///
/// * `classify` 移动文件、`dedup` / `supersede` 并正文后标取代 —— 都不删内容，
///   且都能倒着走回去（改回 project / 演化链），错了顶多是一条记忆放错抽屉；
/// * `supersede` 的落地动作就是 `dedup` 那个函数（见 [`apply_supersede`]），
///   所以判据也共用覆盖率，见 [`pair_outcome`]；
/// * `distill` / `consolidate` 只**新增** l2，靠内容哈希幂等，删掉即回原样，
///   默认就该落地（否则 l2 永远长不出来，见 `review.autoConfirmDistill`）。
///
/// ── 为什么 classify 还要额外要求「slug 已登记」────────────────
/// `classify` 的置信度来自 `name_similarity` 的词面比，它对「两个项目名只差一个
/// 词」几乎没有分辨力（真机上 `agent2api` 与 `agent2api-main` 能到 0.9+）。
/// 所以自动搬的判据不是分数，而是**证据类型**（路径 vs 提及）；此外目标项目必须
/// **已经在注册表里且未归档** —— 用户的意图已经表过一次态。否则一次模糊命中就会
/// 凭空建出一个项目目录，把人越分越乱。
///
/// ── 为什么 dedup 的 0.72~0.89 不靠分数一票否决 ────────────────
/// 真机抽样里 0.72 那一对是「最小字节补丁」与「坚持最小字节改动」，同一件事的
/// 两次表述里带着不同的具体字节值 —— 合并会丢掉其中一份。但「丢不丢」取决于
/// **短边覆盖率**，不取决于相似分：同一个分数区间里同时住着「该合」与「绝不能
/// 合」。所以分数只回答「值得比一比吗」，覆盖率回答「合了会不会丢东西」。
///
/// ── 全局最低置信（已删除，别再往回加）────────────────────────
/// 这里原来写着「classify / dedup 判错的代价大，所以除了各自阈值还要过一道全局
/// 下限 `review.autoResolveFloor`」。真机证明这道下限是**假严格**：它和相似分
/// 是同一个数字的两种说法，两把门槛叠在一件事上，高的那个生效。结果是用户在
/// 界面上把合并阈值调到 0.72、行为一点不变，44 对建议一条都没自动落地。
///
/// 现在的分工是「一道风险配一把尺子」：
///   * classify —— 风险是「搬错抽屉」，尺子是**证据类型**（路径 vs 提及）；
///   * dedup —— 风险是「把一条真结论从检索里抹掉」，尺子是**短边覆盖率**；
///   * 分数只用于「够不够格进候选」与「连边会不会连出噪音」，不再兼任安全闸。
///
/// ── 开关关着返回 `Hold`，不返回 `Retire` ────────────────────────
/// 用户主动关掉某类自动裁决时，他要的是「这类别自己动」，不是「这类给我清空」。
/// 所以开关关着 → 留在队列，等人。
///
/// `projects` 由调用方给而不是在这里 `load_projects()`：批扫 500 行时每次都读
/// 一遍 `projects.json` 就是 500 次磁盘读，而这个文件在一轮扫描里不会变。
fn decide(row: &index::ReviewRow, all: &[MemoryEntry], projects: &[store::Project]) -> Decision {
    if !config::bool_of("review.autoResolve") {
        return Decision::Hold;
    }
    let enabled = match row.kind.as_str() {
        "classify" => config::bool_of("review.autoConfirmClassify"),
        "dedup" => config::bool_of("review.autoConfirmDedup"),
        "supersede" => config::bool_of("review.autoConfirmSupersede"),
        "distill" | "consolidate" => config::bool_of("review.autoConfirmDistill"),
        // 没有开关、也没有落地动作能管它 —— 见 `is_managed_kind`。留着就是一条
        // 永远清不掉的「待确认」，这正是「这台机器永远要我去点一下」的来源之一。
        _ => return Decision::Retire("auto-unknown-kind"),
    };
    if !enabled {
        return Decision::Hold;
    }
    match row.kind.as_str() {
        "dedup" | "supersede" => match pair_outcome(row, all) {
            PairOutcome::Merge => Decision::Merge,
            PairOutcome::Relate => Decision::Relate,
            PairOutcome::Retire(reason) => Decision::Retire(reason),
        },
        "classify" => {
            // ── 先把「凭什么这么判」查清楚，再谈阈值 ──────────────
            // 阈值原来是 0.95，理由「词面相似度分辨不了同名前缀」。真机数据说明
            // 问题不在阈值：6 条待归类全是「标签/标题提到 agenthub」，讲的是
            // 「我们在讨论 agenthub」，不是「这些活是给 agenthub 干的」。这类
            // 建议无论置信多高都不该自动 —— 分数是 `name_similarity` 给的，它只
            // 知道两个字符串有多像，不知道「像」和「属于」的区别。
            //
            // 所以判据按证据类型分：只有路径证据（`files` / `cwd`）命中才算。
            // `row.confidence` 是**入队那一刻**按名字相似度算的，拿一个与证据
            // 无关的旧分数去否决一份硬证据，等于让「写记忆时工作目录在哪儿」
            // 这条最硬的事实永远落不了地。
            let slug = row
                .payload
                .get("suggestion")
                .and_then(Value::as_str)
                .unwrap_or_default();
            if slug.is_empty() {
                return Decision::Retire("auto-stale-pair");
            }
            let Some(target) = all.iter().find(|entry| entry.id == row.target) else {
                // 目标已被删 / 已被并。指向空气的建议永远无法被批准。
                return Decision::Retire("auto-target-missing");
            };
            // 已经被归到别处了（另一次归类成功、或用户手动改过）：这条建议过期。
            if target.project.as_deref() == Some(slug) {
                return Decision::Retire("auto-already-classified");
            }
            if !target.is_trusted() {
                return Decision::Retire("auto-untrusted");
            }
            match classify_evidence_of(&row.payload, target, projects) {
                // 路径证据指向建议要去的项目，且项目已登记、未归档 —— 用户的意图
                // 已经表过一次态；归档项目的语义是「不再接新记忆」，往里搬是反向操作。
                ClassifyEvidenceOf::OwnProject
                    if projects
                        .iter()
                        .any(|project| project.slug == slug && !project.archived) =>
                {
                    Decision::Apply
                }
                ClassifyEvidenceOf::OwnProject => Decision::Hold,
                // 有路径证据，但它指向另一个项目：入队时按名字把目标猜错了。
                // 留着等人点头只会在「全选确认」时被批量搬错。
                ClassifyEvidenceOf::OtherProject(_) => Decision::Retire("auto-wrong-project"),
                // 只有标签/标题提到项目名：那是「在讲它」，不是「属于它」。这种
                // 建议**永远**不可能被自动裁决（真机 562 条记忆里带 `files` 的
                // 只有 81 条，路径段命中已登记项目的 0 条），留着只会一直占着
                // 「待确认」的位置冒充一件需要人决定的事。
                ClassifyEvidenceOf::Mention => Decision::Retire("auto-mention-only"),
            }
        }
        // 蒸馏 / 合并结论只新增 l2，靠内容哈希幂等，删掉即回原样 —— 直接落。
        "distill" | "consolidate" => Decision::Apply,
        _ => Decision::Hold,
    }
}

/// 一次裁决的全部可能结果。`Hold` 是唯一会让建议继续占着「待确认」的一种。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Decision {
    /// 按 `apply_verdict` 落盘（归类搬家 / 蒸馏新增 / 合并新增）
    Apply,
    /// 配对型：并正文 + 标取代
    Merge,
    /// 配对型：只连一条边，两条都留
    Relate,
    /// 不该继续挂在队列，附作废理由（仍可反向批准）
    Retire(&'static str),
    /// 开关关着，或判据说「还没到时候」—— 留给人
    Hold,
}

/// 配对型建议（`dedup` / `supersede`）的终态。
///
/// ── 为什么没有「留给人」这一档 ────────────────────────────────
/// 队列曾经是闸门：判据没把握的就挂在那儿等人点。真机上人的用法是「全选 →
/// 确定」，所以「留给人」既没有换来安全（一段没有判据支撑的批量批准），又让
/// 「待确认 N 条」永远降不到 0 —— 用户看到的是「这台机器永远要我去点一下」。
///
/// 一条建议只有三种命运，都能由判据自己回答：
///   * `Merge`  —— 有内容要并，且并了不丢东西；
///   * `Relate` —— 不该并，但确实相关：连一条边，两条正文都留着（纯新增、可反悔）；
///   * `Retire` —— 这条建议已经不该存在（目标没了 / 早就处理过 / 够不上门槛），
///                留一个具体理由作废，仍然可查、可反向批准。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PairOutcome {
    /// 有两份内容要并到一起
    Merge,
    /// 不动正文，只连一条边
    Relate,
    /// 不该继续挂在队列里，附作废理由
    Retire(&'static str),
}

/// 一条配对型建议现在该怎么落。`all` 由调用方给（一轮扫描用同一份快照）。
///
/// 判据从**条目当前状态**重算，不读载荷里的分数当结论 —— `enqueue` 传的
/// `confidence` 是入队那一刻的相似分，而条目可能已经被别的建议合走、被人改过。
///
/// ── `dedup` 与 `supersede` 共用这一个函数 ──────────────────────
/// 两类建议的动作落到同一个函数（`apply_supersede` 直接调 `apply_dedup`），差别
/// 只在「方向是否已知」：
///   * `dedup` 的两个 id 地位对等，谁更全只有覆盖率知道，所以合并前必须由载荷的
///     `keep` 指明方向；方向缺失时**仍可连边**（连边不动正文，不需要方向）；
///   * `supersede` 方向由载荷固定（新的取代旧的），不需要 `keep`，判据只剩
///     「旧的还有没有独有内容」。
///
/// ── 为什么「覆盖率不够」的去重建议是连边而不是留给人 ────────────
/// 覆盖率低 = 两条各有独有内容 = 它们不是同一件事。这不是一个需要人拍板的
/// 疑难，而是一个已经从数据里读出来的事实：正确动作是「两条都留、建立关联」，
/// 也就是 `Relate`。留给人只会让人批一条「合并两个不同结论」的错操作。
fn pair_outcome(row: &index::ReviewRow, all: &[MemoryEntry]) -> PairOutcome {
    let Some(this) = pair_this_id(row) else {
        return PairOutcome::Retire("auto-stale-pair");
    };
    let other = pair_other_id(row);
    if other.is_empty() || other == this {
        return PairOutcome::Retire("auto-stale-pair");
    }
    let (Some(this_end), Some(other_end)) = (live_end_of(all, this), live_end_of(all, other)) else {
        // 有一端彻底没了（文件被删 / 链终点进回收站 / 被反驳 / 成环）：
        // 这对比不出来，也合不了。指向空气的建议永远无法被批准。
        return PairOutcome::Retire("auto-target-missing");
    };
    // 两端解到同一活条目 = 正文已经在一处了。真机那 2 条 supersede 就是这样：
    // 数据早就落过地，队列行没人清，用户看到「还有 2 条等你批」，点批准却
    // 什么都不变（`apply_dedup` 见两端相等直接返回）。
    if this_end == other_end {
        return PairOutcome::Retire("auto-already-resolved");
    }
    let (Some(this_entry), Some(other_entry)) = (
        all.iter().find(|entry| entry.id == this_end),
        all.iter().find(|entry| entry.id == other_end),
    ) else {
        return PairOutcome::Retire("auto-target-missing");
    };

    if row.kind == "supersede" {
        // 被取代的那条必须是可信的活结论：拿一条已被反驳的旧条去「被取代」，
        // 只是给一条错结论补一次葬礼，没有意义。
        if !this_entry.is_trusted() || !other_entry.is_trusted() {
            return PairOutcome::Retire("auto-untrusted");
        }
        // 判据是「旧条还有没有独有内容」：合并不删正文，旧条的内容会先并进新条
        // 再退出检索，所以「覆盖不到」就是「旧条还有独有内容」。
        //
        // 这里必须用**有向**覆盖率（分母是旧条）：对称版取两边较短的那条当分母，
        // 于是「新条只有一句话」时短边是新条、覆盖率虚高成 0.6 —— 一条只说
        // 「插件要重写 host」的短句就能把一份完整因果链判成失效并埋掉。
        return supersede_gate(
            content_coverage_of(&other_entry.body, &this_entry.body),
            this_entry.layer == other_entry.layer,
            config::number("dedup.l2.mergeMinCoverage"),
        );
    }

    // ── dedup ───────────────────────────────────────────────────
    if !this_entry.is_trusted() || !other_entry.is_trusted() {
        return PairOutcome::Retire("auto-untrusted");
    }
    // ── L4：模型裁决优先于词面判据 ────────────────────────────────
    // 载荷里有 `modelOp` 就是 `dedup::judge` 已经判过这一对。它在词面判据之前
    // 生效，因为词面分根本回答不了这个问题：`dedup_gate` 只能问「短边覆盖率够
    // 不够」，而「这两条是不是在说同一件事」是语义问题 —— 相似分 0.72 那一档
    // 里同时住着「同一件事的两次表述」和「同领域的两个结论」。
    //
    // 没写过线的模型意见不会进这里（`model_verdict` 自己看置信度），所以这条
    // 分支不会绕过 `dedup.l4.autoUpdateThreshold` 的保护。
    if let Some(outcome) = model_verdict(row, this_entry, other_entry) {
        return outcome;
    }
    let score = row.payload.get("score").and_then(Value::as_f64).unwrap_or(row.confidence);
    // 方向必须明确才合并：载荷缺 `keep` 时 `apply_verdict` 会退回「新条取代
    // 候选」的老语义，而巡检给的 `candidate` 恰恰是要留的那条 —— 那种合并会
    // 把正主标成「已被取代」。但方向不明**不妨碍连边**。
    let directed = row.payload.get("keep").and_then(Value::as_str).is_some();
    dedup_gate(
        score,
        content_coverage(&this_entry.body, &other_entry.body),
        this_entry.layer == other_entry.layer,
        directed,
        config::number("dedup.l2.mergeMinCoverage"),
        config::number("dedup.l2.autoMergeThreshold"),
        config::number("dedup.l2.relateMinScore"),
    )
}

/// `supersede` 的判据本体（纯函数：三个入参，没有配置、没有磁盘）。
///
/// 拆出来是为了可测：判据挂在 `config::number()` 上时，单测只能验证「读到的
/// 是不是默认值」，验不了「分档对不对」—— 而分档错了就是静默地丢内容。
///
/// ── 跨层为什么不合并 ──────────────────────────────────────────
/// l1 是流水（带发生顺序、带当时的上下文），l2 是沉淀。把一条 l1 并进 l2 会
/// 丢掉「什么时候知道的」这一层信息，而合并的结果是旧条退出检索 —— 丢掉的那
/// 部分就再也读不到了。两条有关联这件事仍然成立，那正是 `Relate`。
///
/// ── 覆盖不到为什么不连边 ──────────────────────────────────────
/// `supersede` 断言的是「旧的不再成立」。连边只会让两条都继续被当成事实，
/// 而事实方向没法用一条边表达。所以覆盖不到时唯一安全的动作是什么都不动、
/// 把这条无法成立的建议作废 —— 旧事实保持有效。
fn supersede_gate(coverage: f64, same_layer: bool, min_coverage: f64) -> PairOutcome {
    if !same_layer {
        return PairOutcome::Relate;
    }
    if coverage >= min_coverage {
        return PairOutcome::Merge;
    }
    PairOutcome::Retire("auto-not-covered")
}

/// `dedup` 的判据本体（纯函数）。三档的分工：
///
///   * `Retire` —— 连「值得比一比」都够不上（分数低于 `relate_min`）。连边是
///     纯新增，但门槛低到「同项目任意两条」会把关联图撑成完全图，那等于没有
///     关联：检索时每条都带出十条，人就不看了。
///   * `Merge`  —— 有两份内容确实要并到一起，且并了不丢东西：方向明确
///     （载荷有 `keep`）、同层、短边被长边解释掉（`coverage >= min_coverage`），
///     且相似分过合并阈值。
///   * `Relate` —— 其余全部：够像但各有独有内容、方向不明的老载荷、跨层。
///     连一条边，两条正文都留 —— 纯新增，可反悔。
///
/// ── 为什么同 hash 绕过覆盖率 ──────────────────────────────────
/// `score >= 0.999` 意味着两段字节相同，不存在「各有独有内容」；跨层也照合，
/// 因为「层不同所以不是同一件事」在这句话面前不成立。
///
/// ── 为什么覆盖率够、分数不够也不能合 ──────────────────────────
/// 典型来源是「两条短结论都引用了同一段长文本」：短边确实被长边含住，覆盖率
/// 虚高，但两条在讲不同的事。合并它们等于把两个结论压成一条。
fn dedup_gate(
    score: f64,
    coverage: f64,
    same_layer: bool,
    directed: bool,
    min_coverage: f64,
    merge_threshold: f64,
    relate_min: f64,
) -> PairOutcome {
    if score < relate_min {
        return PairOutcome::Retire("auto-below-threshold");
    }
    if directed && score >= 0.999 {
        return PairOutcome::Merge;
    }
    if directed
        && same_layer
        && coverage >= min_coverage
        && dedup_score_ok(score, merge_threshold)
    {
        return PairOutcome::Merge;
    }
    PairOutcome::Relate
}

/// L4 裁决 → 动作。载荷里没有模型结论、或置信度没过线时返回 `None`，
/// 交回词面判据（[`dedup_gate`]）处置。
///
/// ── 为什么是「覆盖」而不是「替换」 ────────────────────────────
/// 词面判据并不是错的，它只是不够：`find_hash_duplicate` 挡字节重复、`dedup_gate`
/// 挡「短边被长边解释掉」——这两件事模型也做得好，但做这两件事不需要模型。
/// 模型补的是中间那一档（像但说不好是不是同一件事）。所以 L4 只在**有结论且
/// 够确信**时接手，其余一律落回原判据，行为与加 L4 之前完全一致。
///
/// ── 四个操作怎么映射到三种动作 ────────────────────────────────
///   · `ADD`    —— 候选里没有同一件事 → [`PairOutcome::Relate`]：两条都留，
///     连一条边。连边不动正文，可反悔。
///   · `NONE`   —— 连「相关」都不是 → 作废这条建议，什么都不动。注意这与
///     「低于连边阈值」是同一类结论，所以复用 `auto-*` 作废理由的写法。
///   · `UPDATE` —— 同一件事，信息更全的那条留下 → [`PairOutcome::Merge`]。
///     方向由载荷里的 `keep` 决定（`dedup::judge` 写），不是在这里猜。
///   · `DELETE` —— 同一件事且新条没有新信息 → 同样是 `Merge`，方向固定为
///     「留候选」（`dedup::judge` 已把 `keep` 写成 `candidate`）。
///
/// ── 跨层为什么降级成连边 ─────────────────────────────────────
/// 与 [`dedup_gate`] 同一条理由：l1 是流水（带发生顺序与当时的上下文），l2 是
/// 沉淀。模型说「这两条是同一件事」很可能对，但把 l1 并进 l2 会丢掉「什么时候
/// 知道的」这一层信息，而合并的结果是被取代那条退出检索 —— 丢掉的部分再也读
/// 不到了。同一件事这个结论仍然成立，那正是连边。
fn model_verdict(
    row: &index::ReviewRow,
    this_entry: &MemoryEntry,
    other_entry: &MemoryEntry,
) -> Option<PairOutcome> {
    let op = row
        .payload
        .get("modelOp")
        .and_then(Value::as_str)?
        .trim()
        .to_ascii_uppercase();
    let confidence = row
        .payload
        .get("modelConfidence")
        .and_then(Value::as_f64)
        .unwrap_or(0.0);
    // 没过线 = 「模型觉得像，但没那么确定」。这一档交给词面判据：它的结论虽然
    // 粗，但至少是同一条数据算出来的，可复核；半信半疑的模型意见执行下去，
    // 出错时既没有数据依据也没有人能解释。
    if confidence < config::number("dedup.l4.autoUpdateThreshold") {
        return None;
    }
    let same_layer = this_entry.layer == other_entry.layer;
    Some(match op.as_str() {
        "ADD" => PairOutcome::Relate,
        "NONE" => PairOutcome::Retire("auto-model-none"),
        "UPDATE" | "DELETE" if same_layer => PairOutcome::Merge,
        "UPDATE" | "DELETE" => PairOutcome::Relate,
        _ => return None,
    })
}

/// 这条归类建议有没有「归属级」证据，以及证据指向谁。
///
/// 返回三态（见 [`ClassifyEvidenceOf`]）：`OwnProject` 才可自动搬；`OtherProject`
/// 说明入队时按名字把目标猜错了，该作废但原因要如实写；`Mention` 表示只有标签/
/// 标题提到项目名 —— 那是「在讲它」，不是「属于它」。
///
/// ── 证据有两个来源：`files` 与 `cwd` ──────────────────────────
/// 两者都是「这条记忆**落在**哪个目录」，而不是「它在**讲**什么」：
///
///   * `files` —— 条目挂的文件路径，路径段里出现项目目录名；
///   * `cwd` —— 写入时调用方报的工作目录。这条只在**写入路径**产生的建议里
///     才有（`enqueue` 的载荷写了 `"cwd": input.cwd`），而它恰恰是最强的证据：
///     用户在哪个仓库里干活，就在给哪个项目写记忆。巡检产出的建议没有这个键
///     （`MemoryEntry` 不存 cwd），所以老队列里那 6 条「提到 agenthub」的仍然
///     过不了这道闸 —— 那正是它们该被拦下的原因。
///
/// 少了 cwd 这一半会误伤：写入时 `layout::classify` 给出的 `suggest` 说明
/// **目录名**与某个已登记项目相似度落在模糊带（0.72~0.95），它不知道该怎么
/// 自动建项目，所以入队问人。那不是「在讲它」，是真的在那个目录里干活。
/// 只认 `files` 的话，这类建议会被当作「仅提及」直接作废 —— 把最该自动的一批
/// 连人看的机会都掐掉。
///
/// ── 为什么还要读 `Project.path` ────────────────────────────
/// 目录名匹配有个天然盲区：`git clone url 我的目录` 检出的目录名与 slug 毫无
/// 关系，路径段怎么比都不命中，可用户确实在那个项目里干活。`Project.path` 是
/// 登记时写下的真实目录，按「段前缀」比一下就是确定的答案 —— 但真机上这个字段
/// 现在几乎全是 `null`（登记表是旧版本写的，只存了 slug/name），所以它只能当
/// 加强项，不能当唯一判据。
///
/// ── 为什么不直接读载荷里的 `evidence` ────────────────────────
/// 载荷是入队那一刻写下的，而条目本身可能已经被改过（补了 `files`、换了标签）。
/// 更重要的是：老队列里那批建议**根本没有** `evidence` 键（这个字段是这一轮
/// 新加的），而它们恰恰是最需要拦住的一批。所以判据从**条目当前状态**重算，
/// 载荷只当辅助记录 —— 判据跟着事实走，不跟着当时的快照走。
fn classify_evidence_of(
    payload: &Value,
    target: &MemoryEntry,
    projects: &[store::Project],
) -> ClassifyEvidenceOf {
    let Some(slug) = payload.get("suggestion").and_then(Value::as_str).filter(|slug| !slug.is_empty()) else {
        return ClassifyEvidenceOf::Mention;
    };
    // 路径段 → 项目。两条判据按强度排序：
    //
    //   ① `Project.path` 登记的真实目录 —— 最硬。`git clone url 我的目录` 检出成
    //      什么样它就记什么样，与 slug 可以毫无关系；
    //   ② 路径段与 slug 结构相等 —— 覆盖「没记 path 的老项目」与「仓库内相对
    //      路径里带项目名」。
    //
    // 只判「有没有」，不在这里比相似度：相似度已经在上游分过档，重复算一遍只会
    // 让两处阈值有机会不一致。
    let mut segments: Vec<String> = Vec::new();
    let mut paths: Vec<&str> = Vec::new();
    for file in &target.files {
        push_path_segments(file, &mut segments);
        paths.push(file.as_str());
    }
    // `cwd` 也可能是文件路径（客户端偶尔把文件当 cwd 报），同样按路径段拆。
    if let Some(cwd) = payload.get("cwd").and_then(Value::as_str) {
        push_path_segments(cwd, &mut segments);
        paths.push(cwd);
    }

    let mut owner: Option<String> = segments_owner(&segments, projects);
    for project in projects {
        if project.archived || project.slug == store::GLOBAL_SLUG {
            continue;
        }
        let Some(root) = project.path.as_deref() else {
            continue;
        };
        if !paths.iter().any(|path| layout::path_is_inside_project(path, root)) {
            continue;
        }
        // `path` 是登记下来的真实目录，比目录名硬；两者冲突说明登记表本身矛盾
        // （同一个项目登记了两棵树），此时不做裁决。
        if owner.is_some() && owner.as_deref() != Some(project.slug.as_str()) {
            return ClassifyEvidenceOf::OtherProject(String::new());
        }
        owner = Some(project.slug.clone());
    }

    match owner {
        Some(found) if found == slug => ClassifyEvidenceOf::OwnProject,
        // 有路径证据，但指向另一个项目：入队时按名字猜错了目标。这种建议
        // 批准下去就是错搬，必须能自己消失，且原因要如实写。
        Some(found) => ClassifyEvidenceOf::OtherProject(found),
        None => ClassifyEvidenceOf::Mention,
    }
}

/// 一条归类建议的证据落点。三态而不是 `Option`：`Option` 把「没有路径证据」
/// 和「路径证据指向别处」压成同一个 `None`，于是入队时猜错项目的建议会被记成
/// 「只是提及」—— 那既不解释它为什么消失，也掩盖了一个真实的目标判错。
#[derive(Debug, Clone, PartialEq, Eq)]
enum ClassifyEvidenceOf {
    /// 路径证据指向建议要去的那个项目 —— 可自动搬
    OwnProject,
    /// 路径证据指向另一个项目（空串 = 横跨多个项目，没有唯一答案）
    OtherProject(String),
    /// 只有标签 / 标题提到项目名：不自动，也只是「在讲它」
    Mention,
}

/// 把一条路径拆成目录段（`C:\a\b\c.rs` → ["C:", "a", "b", "c.rs"]）。
///
/// ── 为什么手写 split，而不用 `Path::components()` ─────────────
/// 记忆是**跨机器**的：一份在 Windows 上写下、`files` 里带反斜杠的条目，同步到
/// 另一台机器后用 `Path::components()` 拆只会得到一个整段
/// `D:\work\agent2api\src`，于是「路径里有项目目录」这条判据在非 Windows 上
/// 永远不成立 —— 自动裁决会静默退化成一概不做，且没有任何报错。所以两种分隔符
/// 都认，与当前平台无关。
///
/// 空串直接跳过：调用方有一半的场合（`cwd` 没报、`files` 里有空元素）都会送
/// 空串过来，而空段拿去比相似度只会得 0。
fn push_path_segments(path: &str, out: &mut Vec<String>) {
    if path.trim().is_empty() {
        return;
    }
    for segment in path.split(['/', '\\']) {
        let segment = segment.trim();
        // `.` / `..` 不是名字，混进去只会白跑比较
        if segment.is_empty() || segment == "." || segment == ".." {
            continue;
        }
        out.push(segment.to_string());
    }
}

/// 「这个项目名下」的路径段里有几个。`classify_evidence_of` 用它区分
/// 「证据指向建议的目标」/「证据指向别处」/「没有路径证据」。
///
/// 判据是 [`layout::name_is_same_project`]：结构性相等，不是相似度。刻意不引入
/// 任何数字阈值 —— 用模糊区（0.72）会把 `agent2api` 判成 `agent2api-main` 的
/// 路径；而 `name_similarity` 在剥离检出后缀那条分支上给这两者打的分数**恰好
/// 就是** 0.98，所以「0.98 足够严」是个错觉，它挡不住这条。归属错了的代价是
/// 记忆静默流进另一个项目，用户在自己项目里再也搜不到它 —— 这个场合不猜。
///
/// 返回 `Some("")` 表示路径横跨两个已登记项目：多仓库工程没有唯一答案，
/// 调用方据此判「有证据但指向不明」，而不是「没有证据」。
fn segments_owner(segments: &[String], projects: &[store::Project]) -> Option<String> {
    let mut owner: Option<String> = None;
    for project in projects {
        // `_global` 是兜底桶，不是「某个目录里的项目」。而且它归一化后是
        // `global`，一个叫 `global` 的普通目录就能命中 —— 那条建议毫无意义。
        // 归档项目同理排除：它的语义是「不再接新记忆」。
        if project.archived || project.slug == store::GLOBAL_SLUG {
            continue;
        }
        if !segments
            .iter()
            .any(|segment| layout::name_is_same_project(segment, &project.slug))
        {
            continue;
        }
        if owner.is_some() {
            return Some(String::new());
        }
        owner = Some(project.slug.clone());
    }
    owner
}

/// 分数够不够格留在「值得比一比」的区间里。
///
/// ── 这个阈值下调到 0.72 的理由 ───────────────────────────────
/// 它曾经是 0.9，理由是「0.72 那种带着不同具体字节值，合并会丢一份」。那个理由
/// 本身没错，但它**压错了地方**：丢不丢取决于覆盖度，不取决于相似分。真机 44 对
/// 里相似分 0.72 与 0.89 的配对，覆盖度可以从 0.27 横跨到 0.93 —— 同一个分数区间
/// 里同时住着「该合」和「绝不能合」。用高阈值去挡后者，代价是前者一条也合不了
/// （实测 0 条自动裁决），而后者依旧可能被相似分碰巧抬到 0.89 之上。
///
/// 现在两把尺子分开：这个阈值只回答「值得比一比吗」，`mergeMinCoverage` 才回答
/// 「合了会不会丢东西」。它的读取点在 [`pair_outcome`] 的合并分支 —— 覆盖率够但
/// 分数不够的配对仍然不该合（引用了同一段长文本的两个不同结论，覆盖率会虚高），
/// 这一道闸留在那里。
fn dedup_score_ok(score: f64, merge_threshold: f64) -> bool {
    score >= 0.999 || score >= merge_threshold
}


/// 自动裁决一条（已入队）建议。返回 `Some(留下的 id)` 表示**真的改了数据**，
/// 调用方（写入路径）靠它决定是否要把响应里的 id 换成幸存者。
///
/// 队列行无论结果如何都会被裁决掉（`approved` + `auto-*` 痕迹，或 `rejected` +
/// 具体作废理由）：留档是「这条为什么会变」的唯一线索，删掉它就等于让一次自动
/// 写操作无声无息 —— 事后要反悔时，人得先找得到它。
fn auto_resolve_one(id: i64) -> Option<String> {
    // 按 id 精确取，不用 `review_list` 代取：后者按置信度排序并截断，积压到
    // 几百条时刚入队的低分建议排在 500 名之外，自动裁决会静默失效（见
    // `index::review_get` 的注释）。
    let row = index::global().review_get(id)?;
    if row.status != "pending" {
        return None;
    }
    let projects = store::load_projects();
    auto_apply(&row, &store::snapshot(), &projects).ok().flatten()
}

/// [`auto_resolve_one`] 的本体：`row` 由调用方给（批扫时已有一份 pending 快照，
/// 不必按 id 再查一遍），`all` 是当轮快照。
///
/// 返回 `Ok(Some(id))` 表示落盘成功且幸存者是 `id`（只有 `keep = candidate` 的
/// 合并有这种情况）；`Ok(None)` 表示「这一行已经有结论了」—— 包括真的落了盘、
/// 以及按判据作废；`Err` 表示判据说「先别动」，队列行保持 pending。
///
/// ── 为什么作废也返回 `Ok` ────────────────────────────────────
/// 对调用方（写入路径 / 巡检）来说，「这条建议已经有确定结论」是一件事，落盘
/// 与作废只是两种结论。返回 `Err` 会让调用方以为「判据不成立、下次再试」，
/// 而实际上它永远不会再成立 —— 那一行就会挂在队列里直到 30 天后过期。
fn auto_apply(
    row: &index::ReviewRow,
    all: &[MemoryEntry],
    projects: &[store::Project],
) -> Result<Option<String>, String> {
    match decide(row, all, projects) {
        Decision::Retire(reason) => {
            index::global().review_resolve(&[row.id], "rejected", Some(reason));
            crate::server::logging::verbose(
                "[Memory]",
                &format!("队列建议自行作废 {} {}（{reason}）", row.kind, row.target),
            );
            Ok(None)
        }
        Decision::Hold => Err("这一条现在不该自动裁决".to_string()),
        Decision::Merge => {
            // 「谁活下来」要被解到**链尽头**再交给调用方：载荷里的 `candidate` 可能
            // 本身已经被别的建议合走了，调用方拿到旧 id 就会把一个「写了但搜不到」
            // 的 id 交给上游。
            let survivor = if row.kind == "dedup" {
                row.payload
                    .get("candidate")
                    .and_then(Value::as_str)
                    .filter(|_| row.payload.get("keep").and_then(Value::as_str) == Some("candidate"))
                    .and_then(|id| live_end_of(all, id))
            } else {
                None
            };
            // 结论已经由 `decide` 给过了，这里只执行 —— 不再重算一遍判据：
            // 重算会在两次计算之间读到新快照（一轮批扫里前一行刚合掉这一行的
            // 一端），于是「判据说合并」变成「执行时判据说不行」，静默地什么都不做。
            execute(row, Some(PairOutcome::Merge))?;
            let note = format!("auto-{}", row.kind);
            index::global().review_resolve(&[row.id], "approved", Some(note.as_str()));
            // supersede 的日志要把「谁埋了谁」写出来：事后翻日志时，复合 target
            // `新->旧` 读不出方向，得拆开看。
            if row.kind == "supersede" {
                crate::server::logging::log(
                    "[Memory]",
                    &format!(
                        "自动判失效 {} → {}（旧条正文已被新条覆盖）",
                        pair_this_id(row).unwrap_or("?"),
                        pair_other_id(row),
                    ),
                );
            } else {
                crate::server::logging::log(
                    "[Memory]",
                    &format!(
                        "自动合并 {} → {}（置信 {:.2}）",
                        row.target,
                        pair_other_id(row),
                        row.confidence
                    ),
                );
            }
            Ok(survivor)
        }
        Decision::Relate => {
            execute(row, Some(PairOutcome::Relate))?;
            index::global().review_resolve(&[row.id], "approved", Some("auto-relate"));
            crate::server::logging::log(
                "[Memory]",
                &format!(
                    "判为相关而非重复 {} ↔ {}（置信 {:.2}），两条都留",
                    pair_this_id(row).unwrap_or("?"),
                    pair_other_id(row),
                    row.confidence
                ),
            );
            Ok(None)
        }
        // 归类 / 蒸馏 / 合并：动作由 `execute` 按 kind 分派，不需要配对终态。
        Decision::Apply => {
            execute(row, None)?;
            let note = format!("auto-{}", row.kind);
            index::global().review_resolve(&[row.id], "approved", Some(note.as_str()));
            crate::server::logging::log(
                "[Memory]",
                &format!("自动裁决 {} {}（置信 {:.2}）", row.kind, row.target, row.confidence),
            );
            Ok(None)
        }
    }
}


/// 把两条「相似但各有独有内容」的记忆连起来，两边都留。
///
/// ── 为什么不是「什么都不做」─────────────────────────────────
/// 什么都不做的话，下次巡检会把同一对重新发现、重新入队 —— 队列清空一轮就长
/// 回来，用户看到的是「整理过但没变化」。所以必须留一个**持久痕迹**：
///
///   1. `mem_link` 连一条 `same_topic` 边 —— 检索时互相带出来（这是连边的收益）；
///   2. 被取代方（`target`）的 `dedup_status` 标成 `related` —— 下一轮巡检靠它
///      认出「这一对判过了」，不再重复提问。
///
/// 不碰 `superseded_by`：那是「这条以后别读了」的语义，标上就是真删。
///
/// ── 为什么只标一边 ──────────────────────────────────────────
/// 标两边等于把幸存者也标成「疑似重复」，而它可能是另一个配对的候选 —— 一条记忆
/// 同时属于多个相关组是正常的，一条记忆同时属于多个「疑似重复」组才可疑。
fn relate_pair(left: &str, right: &str) -> Result<(), String> {
    if left.is_empty() || right.is_empty() || left == right {
        return Err("参数不成立".to_string());
    }
    // 先把两端解到活条目：载荷里的 id 可能已经被别的建议合走了，往一条死条目上
    // 连边，检索时带不出来（`is_trusted()` 过滤掉它），等于白连。
    let all = store::snapshot();
    let left = live_end_of(&all, left).ok_or_else(|| format!("找不到记忆 {left}"))?;
    let right = live_end_of(&all, right).ok_or_else(|| format!("找不到记忆 {right}"))?;
    if left == right {
        return Ok(());
    }
    index::global().link(&left, &right, "same_topic");
    let Some(mut entry) = store::snapshot().iter().find(|item| item.id == left).cloned() else {
        return Err(format!("找不到记忆 {left}"));
    };
    // 已经是 `merged` 的别降级：那是有真实演化关系的，不是「相关」
    if entry.dedup_status == DedupStatus::Unique {
        entry.dedup_status = DedupStatus::Related;
        entry.updated_at = crate::server::logging::now_ms();
        store::save(&entry)?;
        index::global().upsert(&entry)?;
    }
    Ok(())
}

/// 配对型建议的「本端」id。
///
/// 两类建议把 id 存在不同位置，这不是历史遗留而是语义差别：
///
/// * `dedup` —— `target` 就是条目 id（这条建议针对谁），另一端在载荷的
///   `candidate`；
/// * `supersede` —— `target` 是 `新id->旧id` 这个**复合串**（`enqueue` 用它
///   保证同一对只入队一次），真 id 在载荷的 `newId` / `oldId` 里。
///
/// 混用后果不是报错而是**判据全错**：拿 `新->旧` 去 `live_end_of`，永远查不到
/// 这个 id，于是每一条 supersede 都会被判成 `auto-target-missing` 当场作废 ——
/// 包括那些真正还需要处理的。
fn pair_this_id(row: &index::ReviewRow) -> Option<&str> {
    match row.kind.as_str() {
        "supersede" => row
            .payload
            .get("newId")
            .and_then(Value::as_str)
            .filter(|id| !id.is_empty())
            .or(Some(row.target.as_str())),
        _ => Some(row.target.as_str()),
    }
}

/// 配对型建议的「另一端」id（`dedup` 是 `candidate`，`supersede` 是 `oldId`）。
fn pair_other_id(row: &index::ReviewRow) -> &str {
    let key = if row.kind == "supersede" { "oldId" } else { "candidate" };
    row.payload.get(key).and_then(Value::as_str).unwrap_or_default()
}

/// 扫一遍队列，把每一行都裁掉 —— 落地、连边，或者按判据作废。
///
/// 供巡检末尾、调度器每轮 tick 与「立即处理」按钮调用。写入路径不走这里 ——
/// 它只处理自己刚入队的那一条（见 `auto_resolve_one`），否则批量导入几千次写
/// 就会变成几千次全队列扫描。
///
/// `limit` 上限 500：积压是每天几百条的稳态，扫不完的部分下一轮继续；一次
/// 处理太多会让自动任务的可观测时长失控。
///
/// ── 「扫一遍」的含义是「清空」，不是「挑几条处理」─────────────
/// 判据（[`decide`]）对每一行都有答案，包括「这条建议不成立了」—— 那是作废，
/// 也是答案。所以一次扫描之后队列里不该再有 pending：留下的一律是 `Hold`
/// （用户把那一类的自动裁决关掉了）。真机上「待确认 53 条」的来源正是这里：
/// 一次扫描只挑够格的落地，其余原样留着，而「其余」里绝大部分是不会自己变好的
/// 僵尸行（目标被删、早就合并过、只有名字提及）。
pub fn auto_resolve(limit: usize) -> Value {
    let rows = index::global().review_list(Some("pending"), None, limit.clamp(1, 500));
    // `projects.json` 一轮扫一遍就够：`decide` 里每次读盘会在 500 行上变成
    // 500 次文件读，而这份注册表在一轮扫描里不会变。
    let projects = store::load_projects();
    // ── 快照要跟着写操作一起更新，不能一轮只取一次 ────────────────
    // 这个循环**会改库**（合并正文并标取代）。拿循环开始前那张快照判后面每一行，
    // 判的就是「这一行还成不成立」的过期答案：真机 44 对里有 8 个 id 横跨两对
    // 以上，第一对合完之后，第三对的两端在**旧快照里**看着都还活着、而且不是同
    // 一条 —— 判据会把它当成「还有得合」，而实际两端已经解到同一条。
    //
    // 刷新按「上一次是否真的写过」条件触发，不是每行一次：`snapshot()` 要先走
    // 一遍目录指纹（550 个 .md 的 stat），满队列 500 行就是 500 次白走。
    let mut snapshot = store::snapshot();
    let mut stale = false;
    let mut scanned = 0usize;
    let mut applied: Vec<Value> = Vec::new();
    let mut retired: Vec<(i64, &'static str)> = Vec::new();
    let mut kept = 0usize;
    for row in &rows {
        scanned += 1;
        if stale {
            snapshot = store::snapshot();
            stale = false;
        }
        let entry = json!({ "id": row.id, "type": row.kind, "confidence": row.confidence, "target": row.target });
        match decide(row, &snapshot, &projects) {
            Decision::Hold => kept += 1,
            Decision::Retire(reason) => {
                retired.push((row.id, reason));
                // 作废只改队列行，不动记忆 —— 但下一行的判据仍可能因为它消失而
                // 变化（同一对的两个 id 出现在两条建议里时，`enqueue` 只会留一条，
                // 所以这里不必刷新快照）。
            }
            _ => match auto_apply(row, &snapshot, &projects) {
                Ok(_) => {
                    applied.push(entry);
                    // 真的动过数据（合并 / 搬家 / 新增）→ 下一行必须看新快照。
                    stale = true;
                }
                // 落盘失败：行还是 pending，人在队列里点一次就能看到真实报错。
                Err(_) => kept += 1,
            },
        }
    }
    // 一条一条标记而不是走批量：批量的语义是「用户点了全部拒绝」，而这里每种
    // 情况要留各自的 reason，事后才能回答「这条为什么自己消失了」。
    for (id, reason) in &retired {
        index::global().review_resolve(&[*id], "rejected", Some(reason));
    }
    if !applied.is_empty() || !retired.is_empty() {
        bump_revision();
        // 作废有几种来源，日志里要能分开看：合成一个「作废 N 条」会让人以为
        // 记忆被删 —— 而多数作废连记忆都没碰过，改的只是队列行。
        let mut by_reason: std::collections::BTreeMap<&str, usize> = std::collections::BTreeMap::new();
        for (_, reason) in &retired {
            *by_reason.entry(reason).or_default() += 1;
        }
        let detail = by_reason
            .iter()
            .map(|(reason, count)| format!("{reason}×{count}"))
            .collect::<Vec<_>>()
            .join("、");
        crate::server::logging::log(
            "[Memory]",
            &format!(
                "自动裁决 {} 条，作废 {} 条（{}），留待人工 {} 条",
                applied.len(),
                retired.len(),
                if detail.is_empty() { "—" } else { detail.as_str() },
                kept
            ),
        );
    }
    json!({
        "scanned": scanned,
        "applied": applied.len(),
        "appliedItems": applied,
        "retired": retired.len(),
        "kept": kept,
        "pending": index::global().review_count(),
    })
}

/// 对一批**刚入队**的建议做自动裁决。
///
/// 与 [`auto_resolve`] 的分工：这个只处理调用方点名的 id（写入路径、巡检的
/// 新建议），`auto_resolve` 扫全队列。分开是因为写入路径是热路径 —— 批量导入
/// 几千次写，每次扫全队列会把 O(n) 摊成 O(n²)；而它只需要管自己刚产生的那条。
///
/// ── 返回值换了含义（2026-11-24）────────────────────────────────
/// 旧签名返回「落地了几条」，调用方拿它当「要不要刷新快照」的信号。现在判据对
/// 每一行都有结论（落地 / 连边 / 作废），调用方关心的是「有没有动过数据」——
/// 作废只改队列行、不需要刷新，落地与连边需要。所以返回 `Resolved`，把三件事
/// 分开报：`落地的条数`、`连边的条数`、`作废的条数`。
pub struct Resolved {
    pub applied: usize,
    pub related: usize,
    pub retired: usize,
}

impl Resolved {
    /// 有没有真的动过记忆（作废只改队列行，不算动过）。
    pub fn touched(&self) -> bool {
        self.applied > 0 || self.related > 0
    }
}

pub fn auto_resolve_ids(ids: &[i64]) -> Resolved {
    let mut out = Resolved { applied: 0, related: 0, retired: 0 };
    if ids.is_empty() || !config::bool_of("review.autoResolve") {
        return out;
    }
    // 一次取齐：`decide` 的 classify 分支要查注册表，逐条读盘会在「一次巡检
    // 20 条新建议」上变成 20 次文件读。
    let projects = store::load_projects();
    // 快照只在真有行要判时才需要 —— 它是目录指纹 + 可能的整盘读，而这条路径
    // 是写入热路径。判据自己会决定要不要用，所以这里懒取。
    let mut snapshot: Option<std::sync::Arc<Vec<MemoryEntry>>> = None;
    for id in ids {
        // 逐条按 id 取（`review_get`），不走 `review_list`：刚入队的建议在排序里
        // 可能排到 500 名之外，见 `index::review_get` 的注释。这里是 O(1) 索引查询。
        let Some(row) = index::global().review_get(*id) else { continue };
        if row.status != "pending" {
            continue;
        }
        if snapshot.is_none() {
            snapshot = Some(store::snapshot());
        }
        let view = snapshot.clone().unwrap_or_else(store::snapshot);
        match decide(&row, &view, &projects) {
            // 开关关着 / 判据说先别动：留在队列，等人。
            Decision::Hold => {}
            Decision::Retire(reason) => {
                index::global().review_resolve(&[*id], "rejected", Some(reason));
                out.retired += 1;
            }
            Decision::Relate => {
                if auto_apply(&row, &view, &projects).is_ok() {
                    out.related += 1;
                    // 下一行的判据必须看新快照：连边会改 `dedup_status`，
                    // 而 `dedup_scan` 用「有边 + related」判「这一对判过」。
                    snapshot = Some(store::snapshot());
                }
            }
            Decision::Apply | Decision::Merge => {
                if auto_apply(&row, &view, &projects).is_ok() {
                    out.applied += 1;
                    // 这一步可能刚合并掉另一对的一端，后面几行必须看新快照。
                    snapshot = Some(store::snapshot());
                }
            }
        }
    }
    if out.touched() {
        bump_revision();
    }
    out
}

/// 批量拒绝（「全部忽略」）。不批量**批准**：一批建议里几乎必然有一条是错的，
/// 而错的那条会把记忆搬进错误项目，代价远高于逐条点多几下。
///
/// 「逐条点」这件事对单条高频场合是负担，所以真正的答案是 [`auto_resolve`]：
/// 够格的自动落地（留 `auto-*` 痕迹、可反悔）。判据对每一行都有答案，所以
/// 正常情况下不存在「不够格」这一档 —— 只剩「这一类开关被关掉了」。
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

/// 把「被取代的那条」并入「留下的那条」，再标取代。
///
/// ── 为什么必须先并正文，不能只标 superseded_by ─────────────────
/// `is_live()` 一看到 `superseded_by` 就变 false，这条立刻退出检索、注入、
/// 核心记忆、蒸馏素材 —— **不是「隐藏起来待查」，是真的从出口消失**。
/// 所以「合并」的真实语义是「这两条以后只剩一条能被读到」。
///
/// 真机 44 对 dedup 里，按 token 差分有 **32 对**的被取代方带着对方没有的
/// 具体值；抽两对看：
///
///   review 83  A 有 `solo_chat_mode=code` / `chat_mode=1` / `agent_type`
///              B 有「官方主对话走 create_agent_task（body 144~149KB）」
///                  「llm_utils_chat 仅辅助且 main.js 里根本没有」
///   review 86  A 有「插件是原生可执行文件，JSON-RPC over stdio」
///              B 有「requests 表 sensitive_hits 记录被清洗项」
///                  「access 已 11-01 过期，refresh 有效至 12-01」
///
/// 这两对**无论留谁都会丢东西**，而那恰恰是 dedup 想合的对象（相似度
/// 0.72~0.89 = 同一件事的两次表述，不是同一句话抄两遍）。早先的实现
/// 只标取代、正文一个字不动，于是「整理记忆」变成了「删记忆」：整理越勤，
/// 库越小。这也是为什么它的自动阈值一直不敢往下调 —— 阈值挡住的那 44 条
/// 正是最该合的那批。
///
/// ── 并入的判据：不是「谁的正文更长」，是「合起来是否更全」──────
/// 先试整段替换（约定 `should_absorb` 同一把尺子：`information_score` 更高才
/// 覆盖），能过就直接用被取代方更全的那版；过不了说明两段各有独有内容，
/// 这时**追加**成「另一说法」小节，原正文原样保留。
///
/// `score` 只决定「要不要合并」，不决定「留哪一段」—— 相似度算的是措辞，
/// 措辞相近不代表信息量相等。把它当保留依据，就是拿文本距离冒充内容判断。
///
/// ── `survivor` 已经被别人取代过时，跟着链走到底 ───────────────
/// 真机 44 对里有 7 个 id 出现在两对以上（「D 盘空间」那条同时在三对里）。
/// 串行执行时会出现 A←B 已合完、再来一条「B ← C」的情况：此时 B 已经
/// `superseded_by = Some(A)`，把 C 的正文并进 B 等于并进一条谁都不读的条目
/// —— C 的内容当场消失，而账面上这次合并「成功」了。所以这里先顺着
/// `superseded_by` 走到真正的活条目，再往它身上并。
fn apply_dedup(new_id: &str, old_id: &str) -> Result<(), String> {
    let all = store::snapshot();
    // 顺着取代链找真正的落点。两端都要走到头：同一批里三条互相相似的记忆，
    // 前两条已经合过时 `old_id` 可能已经被吃掉，`new_id` 也可能。
    //
    // 走完还相等 = 这两份内容已经在一处，重复执行不该再动任何东西（同一条建议
    // 被人工点一次、又被自动裁决扫一次就是这条路径）。
    let survivor_id = live_end_of(&all, new_id)
        .ok_or_else(|| format!("记忆 {new_id} 解不到活条目（已被删 / 被反驳 / 取代链成环）"))?;
    let old_end = live_end_of(&all, old_id)
        .ok_or_else(|| format!("记忆 {old_id} 解不到活条目（已被删 / 被反驳 / 取代链成环）"))?;
    if old_end == survivor_id {
        return Ok(());
    }
    // 要并的是 `old_end` 的正文，不是 `old_id` 的：`old_id` 若已被合走，它的正文
    // 早就落在 `old_end` 身上了，再并一次只会把同一段话往两个地方塞。
    let mut absorbed = all
        .iter()
        .find(|entry| entry.id == old_end)
        .cloned()
        .ok_or_else(|| format!("找不到记忆 {old_end}"))?;
    // 幸存者要先并正文再被标记：`store::save` 之后 `is_trusted()` 立刻为 false，
    // 到那时再想改它就只是改一条「已失效」的条目，容易被后续清理扫掉。
    merge_body_into_survivor(&survivor_id, &absorbed.body, &absorbed.title)?;
    absorbed.superseded_by = Some(survivor_id.clone());
    absorbed.dedup_status = DedupStatus::Merged;
    absorbed.updated_at = crate::server::logging::now_ms();
    store::save(&absorbed)?;
    index::global().upsert(&absorbed)?;
    index::global().link(&survivor_id, &absorbed.id, "relates");
    Ok(())
}

/// 顺着 `superseded_by` 一直走到「没有被取代」的那一条，返回它的 id。
///
/// 只走有限步（32）：真出现环（人为改坏 frontmatter）时不能挂死在巡检里，
/// 走满就返回当前节点 —— 后续合并会落在一个环上，但不会死循环。
///
/// ── 只给测试与「只想知道往哪走」的场合用，不要拿去当合并落点 ──────
/// 它走满就返回当前节点，那是「别死循环」的答案，不是「内容该落到哪」的答案。
/// 合并落点必须用 [`live_end_of`]：那个函数对环当场判负。真机 7 条成环的取代
/// 关系（删进回收站时互指留下的）就是这条区别的来处。
#[cfg(test)]
fn resolve_supersede_chain(all: &[MemoryEntry], start: &str) -> String {
    let mut current = start.to_string();
    for _ in 0..32 {
        let Some(entry) = all.iter().find(|item| item.id == current) else {
            return current;
        };
        match entry.superseded_by.as_deref() {
            Some(next) if !next.is_empty() && next != current => current = next.to_string(),
            _ => return current,
        }
    }
    current
}

/// 把一个 id 解到「现在还能当事实用的那一条」。
///
/// `None` 四种情况，都是「这一端已经没有可合并的内容」：
///   * 文件没了（链走到一个不存在的 id）；
///   * 链的终点已被删（在回收站里）；
///   * 链的终点被人反驳过 —— 往一条被否掉的结论里并内容，等于用一段没被否掉
///     的结论去给它背书。
///   * **链成环**（真机 93 条 `superseded_by` 里有 7 条互指）。环里没有「终点」
///     这个概念，随便挑一个节点并正文等于把内容塞进一条没人读的记录 —— 这正是
///     这一轮要修的那个 bug 的另一种形态。所以环一律当成「解不出来」。
///
/// ── 为什么不复用 `resolve_supersede_chain` ──────────────────────
/// 那个函数为了「永不挂死」在走满 32 步后返回当前节点 —— 对**合并落点**来说这是
/// 错的答案：它会让 `apply_dedup` 把正文并进环里某条随机记录，账面成功、内容蒸发。
/// 这里另走一遍带 `seen` 的游标，环当场判负。两处语义不同，就别硬塞进一个函数。
///
/// ── 为什么判据要从「两端都活着」改成「两端解到谁」───────────────
/// 真机 44 对 dedup 里有 7 个 id 出现在两对以上（「D 盘空间」那条同时在三对里）。
/// 原来的判据要求两端都 `is_trusted()`，于是第一对合并完，另两对的两端立刻有一端
/// 失效 —— 判据返回 `None`，被当成「还没到条件」永远留在队列里。而它们其实**还有
/// 内容要合**：`B` 已被 `A` 吃掉，但 `C` 的正文一个字都没进 `A`。
///
/// 解到活条目之后，这两种情况才分得开：
///   * `target` 解到 `A`、`candidate` 解到 `C`（两个不同的活条目）→ 还能合；
///   * 两端都解到 `A` → 真的已经把两份内容并到一处了，这条建议可以作废。
fn live_end_of(all: &[MemoryEntry], id: &str) -> Option<String> {
    if id.is_empty() {
        return None;
    }
    let mut current = id.to_string();
    let mut seen: Vec<&str> = vec![id];
    for _ in 0..32 {
        let entry = all.iter().find(|item| item.id == current)?;
        match entry.superseded_by.as_deref() {
            Some(next) if !next.is_empty() && next != current => {
                if seen.iter().any(|known| *known == next) {
                    return None;
                }
                seen.push(next);
                current = next.to_string();
            }
            // 落到一个「没有被取代」的节点，这才是真正的终点。
            _ => {
                return all
                    .iter()
                    .find(|item| item.id == current)
                    .filter(|item| item.deleted_at.is_none() && !item.is_refuted())
                    .map(|item| item.id.clone())
            }
        }
    }
    None
}

/// 把 `incoming`（被取代方的正文）并进幸存者，返回是否真的改了正文。
///
/// 两种情况分开处理，因为它们的**正确做法相反**：
/// * 对方的信息量更高 → 整段用它（同一件事写得更全，不需要两份）；
/// * 各有独有内容 → 追加成「另一说法」小节，谁都不丢。
///
/// 追加而不是拼接，是为了让「这条结论为什么有两个版本」在正文里自解释 ——
/// 直接 `old + "\n\n" + new` 会读成两句话，半年后看不出它们本来是同一件事。
fn merge_body_into_survivor(survivor_id: &str, incoming: &str, incoming_title: &str) -> Result<bool, String> {
    let incoming = incoming.trim();
    if incoming.is_empty() {
        return Ok(false);
    }
    let Some(mut survivor) = store::snapshot()
        .iter()
        .find(|entry| entry.id == survivor_id)
        .cloned()
    else {
        return Ok(false);
    };
    // 已经含这段就别再加：同一条建议可能被人工点一次、又被自动裁决扫一次。
    if survivor.body.contains(incoming) {
        return Ok(false);
    }
    let richer = should_absorb(&survivor, incoming);
    if richer {
        survivor.body = incoming.to_string();
        if !incoming_title.trim().is_empty() {
            survivor.title = truncate_chars(incoming_title.trim(), 80);
        }
    } else {
        // 证据段是「这条结论凭什么成立」，不参与正文比较，也别重复贴一遍。
        let (core, tail) = split_evidence(incoming);
        let core = core.trim();
        if core.is_empty() {
            return Ok(false);
        }
        // 已经出现过的措辞就不追加：否则「全选确认」跑两遍会把同一段贴两次。
        if survivor.body.contains(core) {
            return Ok(false);
        }
        survivor.body = format!(
            "{}\n\n另一说法（已合并）：{}\n{}",
            survivor.body.trim_end(),
            if incoming_title.trim().is_empty() { "（无标题）" } else { incoming_title.trim() },
            core
        );
        if !tail.is_empty() && !survivor.body.contains(tail.trim()) {
            survivor.body = format!("{}\n\n{}", survivor.body, tail.trim());
        }
    }
    // 哈希必须跟着正文走：精确去重靠它认「同一条内容」，不更新的话下次
    // 同内容派生会再新建一条 —— 这层判重就白设了。
    let (redacted, _) = redact::apply(&survivor.body);
    survivor.hash = model::content_hash(redacted.trim());
    survivor.updated_at = crate::server::logging::now_ms();
    store::save(&survivor)?;
    index::global().upsert(&survivor)?;
    crate::server::logging::verbose(
        "[Memory]",
        &format!(
            "合并 {} ← {} 时{}正文",
            survivor_id,
            survivor.title,
            if richer { "换成更全的" } else { "追加了" }
        ),
    );
    Ok(true)
}

/// 把正文按「证据：」那一段切开。`(正文主体, 证据段)`。
///
/// 合并时正文要拼、证据不该跟着拼：证据是出处列表，两边的出处往往高度重叠，
/// 拼起来只会得到一长串重复 id。
fn split_evidence(body: &str) -> (&str, &str) {
    const MARKERS: [&str; 2] = ["\n证据：", "\n证据:"];
    for marker in MARKERS {
        if let Some(at) = body.rfind(marker) {
            return (&body[..at], &body[at..]);
        }
    }
    (body, "")
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
pub(super) fn reinforce_entry(target: &str, cue: &str) -> Result<Reinforce, String> {
    reinforce_entry_with(target, cue, None, None)
}

/// 一次强化的结果。
///
/// ── 为什么不是 `bool`（是否升级）──────────────────────────────
/// 调用方要分别回答两个问题：「这次真的写进去了吗」（决定计数与日志）和
/// 「有没有升层」（决定 `promoted` 计数）。挤成一个 `bool` 的话，`false` 既
/// 可能是「确认了但还没攒够」也可能是「什么都没变、没写盘」—— 前者要计一次
/// 强化，后者不能。真机上表现为「本轮强化 3 条」而库里一条都没动。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Reinforce {
    /// 这次是否真的改了东西（正文被吸收 / 锚点被追加）。false = 完全没写盘。
    pub changed: bool,
    /// 这次是否把它从 l1 升到了 l2。
    pub promoted: bool,
}

impl Reinforce {
    /// 什么都没变。
    const NOOP: Self = Reinforce { changed: false, promoted: false };
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
) -> Result<Reinforce, String> {
    let Some(before) = store::load_all().into_iter().find(|entry| entry.id == target) else {
        return Err(format!("强化目标不存在：{target}"));
    };
    if !before.is_trusted() {
        // 被人判定为错的条目不能再被「确认」。`reinforce` 会给 l1 追加一条
        // evidence 锚点，攒够 `PROMOTE_AFTER_REINFORCE` 就升 l2 —— 允许强化一条
        // 被否掉的候选，等于让流水自动把它顶回来：用户明确说过「这是错的」，
        // 而模型又抽到同一批素材时，系统会替他把错误升成结论。这里必须挡住。
        //
        // 错误文案分开报，是因为两种失效的下一步动作完全不同：已失效（被取代 /
        // 在回收站）要找回或恢复；被反驳要撤销反驳 —— 而撤销是人的判断，
        // 不该由一次自动抽取隐式完成。
        return Err(if before.is_refuted() {
            format!("强化目标已被判定为错，先撤销反驳：{target}")
        } else {
            format!("强化目标已失效：{target}")
        });
    }
    let mut entry = before.clone();
    let mut changed = false;
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
            changed = true;
            crate::server::logging::verbose(
                "[Memory]",
                &format!("强化 {target} 时吸收了信息量更大的新正文"),
            );
        }
    }
    if !cue.is_empty() && !entry.evidence.iter().any(|known| known.as_str() == cue) {
        // ── 满员时不能从头部丢 ──────────────────────────────────
        //
        // 原来的写法是 `push` 之后 `drain(0..overflow)` —— 丢掉最早的那几条。
        // 但 `evidence` 对 l1 候选是**计数器**（攒够 2 条升层），对 l2 是**出处**
        // （这条结论凭什么成立）。后者越早的那条往往就是根因所在的那次排查，
        // 把它挤掉等于：一条结论被确认得越多，越说不清自己从哪来。
        //
        // 现在的取舍：草稿阶段满了先丢最旧的（它的语义只是计数，同一件事被
        // 反复抽到，旧的确认没有额外价值）；已经是 l2 了就不再追加 —— 20 条
        // 独立确认足够说明问题，再多的锚点只是把出处列表变成流水账。
        if entry.layer == Layer::L1 {
            entry.evidence.push(cue.to_string());
            if entry.evidence.len() > 20 {
                let overflow = entry.evidence.len() - 20;
                entry.evidence.drain(0..overflow);
            }
            changed = true;
        }
    }
    // ── 什么都没变就别写盘 ──────────────────────────────────────
    //
    // 蒸馏每轮都会把「清单里已有的结论」重新 reinforce 一遍，命中的多半正是
    // 已经攒够锚点的 l2。原实现无条件 `save()` + `upsert()`：每轮给同一批
    // 结论各写一次文件、推一次 `updated_at`。后果不只是白费 IO —— `updated_at`
    // 被推到此刻，`profile` 的「距上次画像有没有新素材」、热力图的「最近更新」
    // 全被这些空写搅乱，看起来像「一直在动」。
    if !changed {
        return Ok(Reinforce::NOOP);
    }
    entry.updated_at = model::now_ms();
    let promoted = entry.layer == Layer::L1
        && entry.evidence.len() >= model::PROMOTE_AFTER_REINFORCE;
    if promoted {
        entry.layer = Layer::L2;
        entry.path.clear();
        store::move_save(&before, &entry)?;
    } else {
        store::save(&entry)?;
    }
    index::global().upsert(&entry)?;
    Ok(Reinforce { changed: true, promoted })
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
///
/// ── 为什么只认草稿 ────────────────────────────────────────
/// 前端按 `isDraft` 决定要不要显示按钮，但接口本身不限来源的话，人写的 l1
/// 流水也能被这条路径抬成 l2 —— 而「人写的 l1 是流水，不是待确认的结论」。
/// 语义上「采纳」这个词只对草稿成立；想升人写的流水，走 `update` 的换层
/// （那里有意留了这条路，因为它同时会过判重与 `manual:` 锚点）。
pub fn promote_draft(id: &str) -> Result<Value, String> {
    let Some(before) = store::load_all().into_iter().find(|entry| entry.id == id) else {
        return Err(format!("找不到记忆 {id}"));
    };
    if !before.is_live() {
        return Err("这条记忆已失效，先恢复再采纳".into());
    }
    // 被人判定为错的条目不能靠「采纳」翻身：采纳的语义是「这条我认了」，而它
    // 已经被认过一次「这是错的」。要复活必须先撤销反驳（`unrefute`）—— 那是
    // 一个明确的、会被记进演化链的动作。
    if before.is_refuted() {
        return Err("这条已被判定为不成立，先撤销反驳再采纳".into());
    }
    if before.layer == Layer::L2 {
        // 幂等：重复点不该报错，也不该再写一次盘
        return Ok(json!({ "id": id, "promoted": false, "layer": "l2" }));
    }
    if !is_auto_draft(&before) {
        return Err(
            "只有自动抽取的草稿能被「采纳」；人写的流水要提升层级请用编辑里的层级字段"
                .into(),
        );
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

/// 人判定「这条记忆是错的」。
///
/// ── 为什么它是独立的一条轴，不是删除 / 降层 / supersede ─────────
/// 这三条近路都不行，各有一个致命后果：
///
/// ① **删除**（进回收站）：原文和演化链一起被埋掉。用户想回顾「我当年为什么
///    会信这条」时没有东西可看 —— 而这条记忆存在的价值之一恰恰是「我错过一次，
///    别再错第二次」。回收站还有保留期，到期清理会把它彻底抹掉。
///
/// ② **降层**（l2 → l1）：l1 是 `extract` / `distill` 的**素材池**。降下去的错误
///    结论下一轮就被重新抽出来，换个说法再攒证据升回 l2 —— 错误复活，而且这次
///    「有据可依」，比原来更难推翻。
///
/// ③ **`superseded_by`**：那条轴讲的是「**事实变了**」——旧结论当年成立，只是被
///    新结论顶掉。这里的语义是「**当年就错了**」，它从来没有成立过。混进同一条
///    轴，演化链会讲出「v1 被 v2 取代」的假故事，而真相是「v1 是个错误，v2 是
///    第一次说对」。两个轴可以同时成立，所以必须是两个字段（见 `MemoryEntry`
///    上 `refuted_at` 的注释）。
///
/// ── 为什么落盘要清 pinned / starred ─────────────────────────
/// `inject.rs` 的 `rule_candidate()` 用 `pinned` 直接判定「进必须遵守块」。一条
/// 被否掉的结论若还留着置顶，它会绕过 `is_trusted()` 之外的所有排序，成为 system
/// prompt 里的硬指令。`starred` 同理影响注入与排序权重。所以在反驳这一刻顺手
/// 清掉 —— 撤销反驳时**不自动恢复**：那时候的置顶意图已经过期了（用户今天的
/// 意图是「这条错了」），让他自己在撤销后重新置顶更诚实。
///
/// ── 为什么 `replacement` 可选 ──────────────────────────────
/// 真实纠错顺序通常是「先发现这条错了」「过一会儿才写对的那条」。强制要求先有
/// 接替者，用户就只能先造一条新记忆才能否掉旧的 —— 于是他会去用「删除」。
pub fn refute_entry(id: &str, reason: &str, replacement: Option<&str>) -> Result<Value, String> {
    let all = store::load_all();
    let Some(before) = all.iter().find(|entry| entry.id == id).cloned() else {
        return Err(format!("找不到记忆 {id}"));
    };
    // 已进回收站的条目不该被「反驳」：它在回收站里，界面上的动作是「恢复」或
    // 「彻底删除」。「这条错了」这个判断说的是「内容不成立」，而回收站说的是
    // 「这条我不要了」，两条不同的决定不能互相顶替。
    if before.deleted_at.is_some() {
        return Err("这条记忆在回收站里，先恢复再判定".into());
    }
    if before.is_refuted() {
        // 幂等：重复点（或两个界面同时判）不该报错，也不该覆盖第一次的理由 ——
        // 第一次那条通常是用户最清楚的时候写的。
        return Ok(json!({
            "id": id,
            "refuted": true,
            "already": true,
            "entry": before.to_json(true),
        }));
    }
    // 接替者必须真的存在，否则演化链上会留一条指向空气的边，`timeline()` 走到
    // 那里会静默断掉（它找不到就 break），用户以为链断了。
    if let Some(next) = replacement {
        if !all.iter().any(|entry| entry.id == next) {
            return Err(format!("接替条目 {next} 不存在"));
        }
        if next == id {
            return Err("接替条目不能是它自己".into());
        }
    }
    let mut entry = before.clone();
    entry.refuted_at = Some(model::now_ms());
    entry.refuted_reason = reason.trim().to_string();
    entry.refuted_by = replacement.map(str::to_string);
    // 见函数注释「为什么落盘要清 pinned / starred」。
    entry.pinned = false;
    if entry.kind == MemoryKind::Profile {
        // 画像的置顶是靠 tag 记的（`PROFILE_PIN_TAG`），光清 `pinned` 不清 tag，
        // 下一次 `build_profile` 会把这条被否决的画像当成「用户钉住的」再捡回来。
        entry.tags.retain(|tag| tag != PROFILE_PIN_TAG);
    }
    entry.starred = false;
    // `updated_at` 不动：它记的是「这条记忆最后一次被改动」，而反驳是**对它的
    // 判断**变了，不是内容变了。推它会让热力图与画像新鲜度误以为「今天写了这条」。
    // 反驳自己的时间在 `refuted_at` 里，界面要显示「多久前被判错」读那个。
    store::save(&entry)?;
    index::global().upsert(&entry)?;
    // 演化链补一条边：接替者 → 被反驳者。kind 用 `refute` 与 `relates` /
    // `derived` 区分开 —— 图上要能看出这条边是「纠错」还是「演进」。
    if let Some(next) = &entry.refuted_by {
        index::global().link(next, &entry.id, "refute");
    }
    bump_revision();
    let detail = if entry.refuted_reason.is_empty() {
        String::new()
    } else {
        format!("：{}", entry.refuted_reason)
    };
    crate::server::logging::log("[Memory]", &format!("判定记忆错误 {id}{detail}"));
    Ok(json!({
        "id": id,
        "refuted": true,
        "already": false,
        "entry": entry.to_json(true),
    }))
}

/// 撤销「这条错了」的判定：清掉三个字段，这条重新可以当事实用。
///
/// ── 为什么不保留历史 ───────────────────────────────────────
/// 保留一列 `unrefuted_at` 会诱出「到底信哪一次」的歧义，而人已经明确说了
/// 「撤销」。真正的审计痕迹在别处：`refute_entry` / 本函数各记一行 `[Memory]`
/// 日志，演化链上那条 `refute` 边还在（不会被删），条目自己的 `refuted_at` 清空
/// 之后，界面上一眼能看出「这条现在是正常的」。
///
/// ── 为什么不恢复 pinned / starred ──────────────────────────
/// 反驳时清掉了它们（见 `refute_entry` 的注释）。撤销时「恢复」等于替用户做
/// 一个他今天没做的决定 —— 他当初置顶是因为「这条重要」，后来判它错时那个
/// 判断已经被推翻了。所以撤销只把「能不能当事实」还回来，权重让他自己重设。
pub fn unrefute_entry(id: &str) -> Result<Value, String> {
    let all = store::load_all();
    let Some(before) = all.iter().find(|entry| entry.id == id).cloned() else {
        return Err(format!("找不到记忆 {id}"));
    };
    if !before.is_refuted() {
        return Ok(json!({
            "id": id,
            "refuted": false,
            "already": true,
            "entry": before.to_json(true),
        }));
    }
    let mut entry = before.clone();
    entry.refuted_at = None;
    entry.refuted_reason.clear();
    entry.refuted_by = None;
    store::save(&entry)?;
    index::global().upsert(&entry)?;
    bump_revision();
    crate::server::logging::log("[Memory]", &format!("撤销「判定为错」{id}"));
    Ok(json!({
        "id": id,
        "refuted": false,
        "already": false,
        "entry": entry.to_json(true),
    }))
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
    // 项目口径交给 `project_matches`：三处（这里 / extract / distill）必须用
    // 同一个判据，否则「候选互判重」和「已有结论清单」会各自失效。
    store::snapshot()
        .iter()
        // 被人否掉的候选不再参与判重：它不是「同一件事的另一种说法」，而是
        // 「同样错过的那个说法」。再强化它只会把它攒够证据升上去。
        .filter(|entry| entry.is_trusted() && entry.layer == Layer::L1)
        .filter(|entry| entry.agent.as_deref() == Some(model::AUTO_EXTRACT_AGENT))
        .filter(|entry| project_matches(entry.project.as_deref(), project))
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
    // 蒸馏 / 合并的结论由判据落盘（`review.autoConfirmDistill` + 总闸），而
    // 队列里那条仍然留着等审计。于是有两条路会走到这里：自动裁决落一次、
    // 用户之后在待确认列表里又点一次「批准」。`write()` 的精确去重只管 l1
    // （l2 允许两条一字不差但语境不同），所以第二次会老老实实再写一条 ——
    // 库里出现两条一模一样的结论，注入时还会占两个名额。
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
        // 已被反驳的同内容 l2 不算「已存在」：那样的结论当年就是错的，今天再
        // 蒸馏出一份同样的内容，应该落成一条新条目（而不是被静默跳过）。
        .any(|entry| entry.is_trusted() && entry.layer == Layer::L2 && entry.hash == digest)
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
    // 判据统一走 `pair_similarity`（与 `near_duplicate` 同一份实现）：两处同阈值
    // 却不同算法时，同一条结论会「这里判无关就新建 → 那边判近似就入队」，重复
    // 换个马甲又长回来。
    let target_project = payload.get("project").and_then(Value::as_str);
    let target_kind = payload
        .get("type")
        .and_then(Value::as_str)
        .map(MemoryKind::parse);
    let near = if is_merge {
        None
    } else {
        snapshot
            .iter()
            .filter(|other| other.is_trusted() && other.layer == Layer::L2)
            .filter(|other| project_matches(other.project.as_deref(), target_project))
            .filter(|other| target_kind.map(|kind| other.kind == kind).unwrap_or(true))
            // 长度先粗筛：`pair_similarity` 是 O(n·m) 的字符二元组比对，一次蒸馏
            // 最多 20 条结论 × 上百条 l2，不做粗筛就是几亿次比较。长度差一倍以上
            // 不可能是同一件事 —— 这是免费且不会误杀的前置条件。
            .filter(|other| {
                let (left, right) = (body.chars().count(), other.body.chars().count());
                let (short, long) = if left < right { (left, right) } else { (right, left) };
                short * 2 >= long
            })
            // `pair_similarity` 收的是两条完整条目，而这里手上只有新结论的
            // 正文与标题 —— 拼一个只用于打分的临时条目，不碰盘也不进索引。
            .map(|other| {
                let mut probe = other.clone();
                probe.body = body.to_string();
                if let Some(title) = payload.get("title").and_then(Value::as_str) {
                    probe.title = title.to_string();
                }
                (other.id.clone(), pair_similarity(&probe, other))
            })
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

/// 把一条记忆标记为「被另一条取代」——**已删除**，理由记在这里（2026-11-24）。
///
/// 它原本是给 `enrich::detect_supersede` 的「当场自动应用」路径用的包装。那条
/// 路已经删掉了：写入路径只入队，落地统一交给 `auto_resolve_ids` /
/// `auto_resolve`，判据只有 [`decide`] 一处。
///
/// 这个包装本身在删掉调用点之后就成了死代码。它的注释当时写着「留着一个没人
/// 调的 `pub fn` 比删掉更坏」—— 话是对的，那就该照做：留着它，下一个人会以为
/// 「自动应用」还有第二条路，照着它再写一套判据，于是队列里那条判据和它又漂开
/// 一次。要真标记失效，走 [`apply_supersede`]（它做的是「先并正文再标取代」，
/// 与合并同一个动作）。

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

// ── 一致性自检：队列积压与索引（`status()` 的 `review` / `index`）────
//
// 这里早先有一份 `health()`：按「锚定文件 mtime > 记忆 updated_at」找「可能过时」
// 的记忆，外加队列与索引的一致性。2026-11-24 整块删掉，理由记在这里，免得
// 将来有人觉得「很简单的检查为什么不加回去」：
//
// ① **它一次都没报出过东西，而且结构上不可能报出。** 真机上 73 条带 `files`
//    的条目、482 个 files 项，`stat` 成功 **0 次**：
//    · 13 项命中 `internal_file` 被跳过（记忆库自身容器，正确）；
//    · 469 项在 `root.join(file)` 与 `root.join("projects").join(file)` 两条基准下
//      全 ENOENT —— 因为 `files` 存的是**仓库相对路径**（`scripts/deliver.mjs`），
//      而两条基准都指向记忆根，差着整个仓库。
//    · `registry_upsert` 的 `path` 字段本是修正基准的唯一来源，但真机
//      `projects.json` 里 18 个项目 `path` 全是 `null` —— 于是没有任何一条能解析。
//
// ② **就算基准修对，判据本身也没有区分度。** 按真实仓库根复跑：66 条能解析，
//    66 条全部报「过时」—— 命中率 100%。`files` 里最热的 `scripts/deliver.mjs`
//    出现 67 次，而这文件几乎每次交付都改，等于把「所有锚过它的记忆」永久钉成
//    可疑。误报 100% 的提醒不是提醒，是把清单变成噪音。
//
// ③ **真正有价值的两个指标已经在 `status()` 里了**，而且口径经过事故修正：
//    `review.pending`（待确认队列）与 `index.inSync`（索引 vs 事实源）。
//    `health()` 把它们又算了一遍，多一次 `review_count()` 与 `count()` 的全局
//    连接等待 —— 同一屏两个面板读同一份数，还可能因为取数时刻不同而打架。
//
// 留下的是「体检」这个名字原本想解决的问题的正确位置：`files` 作为检索/注入的
// 路由此继续存在（`inject::harvest_paths` 用它），只是不再假装能判断过时。

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

    #[test]
    fn project_scope_normalises_raw_names_to_slugs() {
        // 真机症状：`{"project":"aimod-cc/Agent2API"}` 这类原始名字永远匹配不上
        // 已 slug 化的 `entry.project`，于是素材池空、清单空、候选互判重失效
        // —— 全都静默发生，日志里一切正常。判据必须两边都先归一。
        assert!(project_matches(Some("aimod-cc-agent2api"), Some("aimod-cc/Agent2API")));
        assert!(project_matches(Some("aimod-cc-agent2api"), Some("aimod-cc-agent2api")));
        assert!(!project_matches(Some("aimod-cc-agent2api"), Some("other-project")));

        // `None`（自动 tick）与 `_global`（项目页的「通用」卡）指同一批条目，必须
        // 互相等价；而一个真名叫 `general` 的项目落在 `projects/general/`，
        // 与通用目录不是一回事，不能被当成同一个。
        assert!(project_matches(None, None));
        assert!(project_matches(None, Some("_global")));
        assert!(!project_matches(None, Some("general")));
        assert!(!project_matches(Some("aimod-cc-agent2api"), None));

        // `in_scope` 的 `None` 是「不限定项目」= 全部收（素材池语义），
        // 与 `project_matches` 的「只比通用」不是一回事，不能合并。
        assert!(in_scope(Some("aimod-cc-agent2api"), None));
        assert!(in_scope(Some("aimod-cc-agent2api"), Some("aimod-cc/Agent2API")));
        assert!(!in_scope(Some("aimod-cc-agent2api"), Some("other")));
    }

    #[test]
    fn layer_try_parse_refuses_typos_instead_of_silently_demoting() {
        // 读磁盘用宽松解析（脏 frontmatter 不能把整条记忆炸掉），读意图必须严格：
        // 拼错 "l3" 静默降成 l1 会把一条沉淀结论塞回流水容器，用户完全看不出来。
        assert_eq!(Layer::try_parse("l2"), Some(Layer::L2));
        assert_eq!(Layer::try_parse(" L2 "), Some(Layer::L2));
        assert_eq!(Layer::try_parse("l1"), Some(Layer::L1));
        assert_eq!(Layer::try_parse("l3"), None);
        assert_eq!(Layer::try_parse(""), None);
        assert_eq!(Layer::try_parse("deep"), None);

        // 宽松版对同一个输入仍兜底成 l1（磁盘兜底语义）
        assert_eq!(Layer::parse("l3"), Layer::L1);
    }

    #[test]
    fn pair_similarity_is_one_implementation_for_all_three_call_sites() {
        // 三处判重（写入 / 派生 / 巡检）现在共用这一个函数。最低要求是它真的
        // 把「同义改写」判高、把「不同事」判低 —— 三处同阈值同算法，才不会再出现
        // 「这边判无关新建、那边判近似入队」。
        let a = MemoryEntry::draft(
            "a",
            "发布脚本读不到版本号",
            MemoryKind::Pitfall,
            "根因是 deliver.mjs 读 process.env.VERSION，而 CI 只写了 GITHUB_REF_NAME。",
        );
        let b = MemoryEntry::draft(
            "b",
            "发布脚本读不到版本号",
            MemoryKind::Pitfall,
            "根因是 deliver.mjs 读 process.env.VERSION，而 CI 只写了 GITHUB_REF_NAME，两边对齐即可。",
        );
        let c = MemoryEntry::draft(
            "c",
            "签到按钮点了没反应",
            MemoryKind::Pitfall,
            "供应商目录里没有 qoder，统一签到却仍然会去签它；要按目录判断而不是按按钮判断。",
        );
        assert!(pair_similarity(&a, &b) > pair_similarity(&a, &c));
        assert!(pair_similarity(&a, &a) > 0.99);
    }

    /// 「被反驳」与「被取代」是两条独立的轴，不能互相顶替。
    ///
    /// 这条测的是**语义边界**：如果哪天有人把 `refuted` 并进 `superseded_by`
    /// 省两个字段，下面的断言会立刻红 —— 那样做的后果是演化链讲出
    /// 「v1 被 v2 取代」的假故事（真相是「v1 当年就是个错误」），而且撤销
    /// 会退化成「从回收站恢复」。
    #[test]
    fn refuted_is_orthogonal_to_superseded_and_deleted() {
        let mut entry = MemoryEntry::draft("a", "t", MemoryKind::Decision, "body");
        // 被反驳：仍然活着、仍然在库里、没被取代
        entry.refuted_at = Some(1);
        assert!(entry.is_live(), "被反驳不等于失效");
        assert!(entry.superseded_by.is_none(), "被反驳不等于被取代");
        assert!(entry.deleted_at.is_none(), "被反驳不等于删除");
        assert!(!entry.is_trusted(), "但它不能当事实用");

        // 撤销反驳就恢复成正常事实 —— 不需要走「恢复」那条路
        entry.refuted_at = None;
        assert!(entry.is_trusted());

        // 两条轴可以同时成立：既被改写（superseded）又被指认当年就错（refuted）
        entry.superseded_by = Some("b".into());
        entry.refuted_at = Some(1);
        assert!(entry.is_refuted() && entry.superseded_by.is_some());
        assert!(!entry.is_live() && !entry.is_trusted());
    }

    /// 「可信条数」的分母口径：被反驳的、草稿的都不算，而且**不重复扣减**。
    ///
    /// 这是仪表盘那个数字的契约。前端以前自己相减（`live - candidates - refuted`），
    /// 一条被判错的草稿同时落在两个桶里会被扣两次 —— 数字少一条，页面上看不出
    /// 任何异常，只是永远对不上浏览页的条数。所以口径挪到后端，这条测试钉住它。
    #[test]
    fn trusted_count_excludes_drafts_and_refuted_without_double_subtracting() {
        let mut plain = MemoryEntry::draft("a", "t", MemoryKind::Decision, "b");
        plain.layer = Layer::L2;
        let mut draft = MemoryEntry::draft("b", "t", MemoryKind::Knowledge, "b");
        draft.agent = Some(model::AUTO_EXTRACT_AGENT.to_string());
        draft.layer = Layer::L1;
        // 落在两个桶的交集里：既是草稿（l1 + auto-extract）又被判错
        let mut draft_refuted = draft.clone();
        draft_refuted.id = "c".into();
        draft_refuted.refuted_at = Some(1);
        let mut refuted = MemoryEntry::draft("d", "t", MemoryKind::Decision, "b");
        refuted.layer = Layer::L2;
        refuted.refuted_at = Some(1);

        let all = [plain.clone(), draft.clone(), draft_refuted, refuted];
        let trusted = all.iter().filter(|entry| entry.is_trusted() && !is_auto_draft(entry)).count();
        assert_eq!(trusted, 1, "只有那条干净的 l2 能当事实用");

        // 反证：旧的相减算式在含交集数据时会算错（少算一条）
        let live = all.iter().filter(|entry| entry.is_live()).count();
        let candidates = all.iter().filter(|entry| entry.is_live() && is_auto_draft(entry)).count();
        let refuted_n = all.iter().filter(|entry| entry.is_live() && entry.is_refuted()).count();
        assert_eq!((live, candidates, refuted_n), (4, 2, 2));
        assert_eq!(live - candidates - refuted_n, 0, "相减会少算一条：交集被扣两次");
        assert_ne!(live - candidates - refuted_n, trusted, "所以分母不能由前端算");
    }

    // ── 自动裁决的判据（纯函数部分）──────────────────────────────
    //
    // `decide` 自己要读 `config::bool_of` 与真实记忆根，单测环境不保证有；
    // 所以把「分档」拆成纯函数逐条钉住（`dedup_gate` / `supersede_gate` /
    // `pair_this_id` / `live_end_of`）—— 判据本身漏一条，自动裁决就会去动它
    // 不该动的东西，而那种错误是静默的。

    #[test]
    fn path_segments_only_match_near_exact_names() {
        // 路径段是**字面事实**，不是猜测。用模糊阈值（0.72）会让 `agent2api`
        // 命中 `agent2api-main` 的路径 —— 那正是两个不同项目被并成一个的开始。
        let project = store::Project {
            slug: "agent2api-main".into(),
            name: "Agent2API".into(),
            remote: None,
            path: None,
            created_at: 0,
            updated_at: 0,
            remotes: Vec::new(),
            merged_into: None,
            pinned: false,
            archived: false,
        };
        let projects = vec![project.clone()];

        let mut segments = Vec::new();
        push_path_segments("D:\\Personal\\Desktop\\agent2api-main\\src\\server", &mut segments);
        assert!(segments.iter().any(|s| s == "agent2api-main"), "路径段该被拆开: {segments:?}");
        assert_eq!(
            segments_owner(&segments, &projects).as_deref(),
            Some("agent2api-main"),
            "路径段逐字等于 slug 就该认领"
        );
        // 目录名 `agent2api`（不带 -main）不该命中 `agent2api-main`
        let mut other = Vec::new();
        push_path_segments("D:\\work\\agent2api\\src", &mut other);
        assert_eq!(
            segments_owner(&other, &projects),
            None,
            "前缀相同的另一个仓库不该被当成同一个项目"
        );

        // ── 显示名不是归属证据 ───────────────────────────────────
        // 真机登记表里有 `name: "Agent2API"` 配 `slug: "agent2api-main"`。只要判据
        // 里带上 `project.name`，`agent2api` 这个目录段就会与 name 归一化后逐字
        // 相等，于是「不带 -main 的目录」照样算命中。这里把它钉死：
        // 反着摆一个 name 恰好等于目录段、slug 完全无关的项目。
        let name_only = vec![store::Project {
            slug: "app-x".into(),
            name: "agent2api".into(),
            ..project.clone()
        }];
        assert_eq!(
            segments_owner(&other, &name_only),
            None,
            "name 逐字相等也不能算命中：name 是显示名，不是目录名"
        );
        // 而 slug 逐字相等就必须命中（判据是 slug，不是别的）
        let mut app_x = Vec::new();
        push_path_segments("D:\\work\\app-x\\src", &mut app_x);
        assert_eq!(segments_owner(&app_x, &name_only).as_deref(), Some("app-x"), "slug 逐字相等就该命中");

        // ── 归档项目不接新记忆 ───────────────────────────────────
        let archived = vec![store::Project { archived: true, ..project.clone() }];
        assert_eq!(
            segments_owner(&segments, &archived),
            None,
            "归档项目不该再收记忆"
        );

        // ── 横跨两个项目 = 有证据但没有唯一答案 ───────────────────
        // 这不是「没有证据」：作废原因要区分两者，否则一次真实的目标判错会被
        // 记成「只是提及」。
        let two = vec![project.clone(), store::Project { slug: "agenthub".into(), ..project.clone() }];
        let mut cross = Vec::new();
        push_path_segments("D:\\work\\agent2api-main\\agenthub", &mut cross);
        assert_eq!(
            segments_owner(&cross, &two),
            Some(String::new()),
            "横跨两个项目要返回空串，不能随便挑一个"
        );

        // 空串与空元素不该产出段（`Path::new("")` 会给出一个空 Normal 段）
        let mut empty = Vec::new();
        push_path_segments("", &mut empty);
        push_path_segments("   ", &mut empty);
        assert!(empty.is_empty(), "空路径不该产出任何段: {empty:?}");
    }

    #[test]
    fn evidence_of_separates_mention_from_wrong_project() {
        // 三态判据的用处：`Option` 会把「有路径证据但指向别处」压成「没有证据」。
        let target_project = store::Project {
            slug: "agent2api-main".into(),
            name: "Agent2API".into(),
            remote: None,
            path: None,
            created_at: 0,
            updated_at: 0,
            remotes: Vec::new(),
            merged_into: None,
            pinned: false,
            archived: false,
        };
        let projects = vec![target_project.clone(), store::Project { slug: "agenthub".into(), ..target_project }];

        // ① 目标就是路径里的那个项目 → 可自动搬
        let mut entry = MemoryEntry::draft("x", "t", MemoryKind::Knowledge, "b");
        entry.files = vec!["D:\\work\\agent2api-main\\src\\main.rs".into()];
        let payload = json!({ "suggestion": "agent2api-main" });
        assert_eq!(
            classify_evidence_of(&payload, &entry, &projects),
            ClassifyEvidenceOf::OwnProject
        );

        // ② 路径里的项目不是建议的目标 → 目标判错，不是「只是提及」
        let wrong = json!({ "suggestion": "agenthub", "cwd": "D:\\work\\agent2api-main" });
        assert_eq!(
            classify_evidence_of(&wrong, &entry, &projects),
            ClassifyEvidenceOf::OtherProject("agent2api-main".into())
        );

        // ③ 只有标签/标题，没有路径 → 只是提及
        entry.files.clear();
        let mention = json!({ "suggestion": "agenthub" });
        assert_eq!(
            classify_evidence_of(&mention, &entry, &projects),
            ClassifyEvidenceOf::Mention
        );

        // ④ `cwd` 单独就够：写入时用户就在那个仓库里干活
        let by_cwd = json!({ "suggestion": "agent2api-main", "cwd": "D:\\work\\agent2api-main" });
        assert_eq!(
            classify_evidence_of(&by_cwd, &entry, &projects),
            ClassifyEvidenceOf::OwnProject
        );

        // ⑤ 归档项目不接新记忆，哪怕路径完全对得上
        let archived = vec![store::Project { archived: true, ..projects[0].clone() }];
        assert_eq!(
            classify_evidence_of(&by_cwd, &entry, &archived),
            ClassifyEvidenceOf::Mention,
            "归档项目里的路径不算归属证据"
        );
    }

    #[test]
    fn path_segments_are_split_the_same_on_every_os() {
        // 记忆跨机器：Windows 上写下的 `files` 会被 Mac / Linux 读回来判。用
        // `Path::components()` 的话，反斜杠路径在非 Windows 上只是一个整段，
        // 自动裁决就静默退化成一概不做 —— 而没有任何地方会报错。
        let mut win = Vec::new();
        push_path_segments("D:\\work\\agent2api-main\\src\\server\\x.rs", &mut win);
        assert_eq!(
            win,
            vec!["D:", "work", "agent2api-main", "src", "server", "x.rs"],
            "Windows 路径必须按反斜杠拆开，与当前平台无关"
        );

        let mut unix = Vec::new();
        push_path_segments("/home/me/agent2api-main/src", &mut unix);
        assert_eq!(unix, vec!["home", "me", "agent2api-main", "src"]);

        // 混着写的（WSL / MSYS 里 `cd /d/work` 这种）也别漏
        let mut mixed = Vec::new();
        push_path_segments("D:/work\\agent2api-main/src", &mut mixed);
        assert!(mixed.iter().any(|s| s == "agent2api-main"), "{mixed:?}");

        // `.` / `..` 不是名字
        let mut dotted = Vec::new();
        push_path_segments("./a/../b//c", &mut dotted);
        assert_eq!(dotted, vec!["a", "b", "c"], "相对段与空段都该被丢掉: {dotted:?}");
    }

    #[test]
    fn coverage_tells_rewording_from_merely_related() {
        // 这是这一轮的核心判据。三对真实数据：
        //
        //   a/b  同一件事换个说法 —— 短的那条整段落在长的那条里
        //   c/d  只是同领域 —— 「事故复盘清单化」vs「前端选型单文件离线 HTML」
        //   e/f  几乎无关 —— 只有「PSCADA」「显示」这两个词碰上了
        //
        // `pair_similarity` 给这三对的分数都在 0.72~0.78 之间（它只算「像不像」），
        // 覆盖度能把它们分开。合并的判据必须用后者：前者会把 c 并进 d，
        // 于是「事故复盘该做成清单」这条结论从检索里消失。
        let a = "修改范围锁定为定点字节补丁，不改其它指令。先坐实字节值再动手。";
        let b = "坚持最小字节改动：只改目标那一个字节，其余指令一个都不动，先坐实字节值。";
        let c = "事故复盘要清单化：每次出事按固定项走，不靠回忆。";
        let d = "前端选型走单文件离线 HTML：不引框架、不打包、双击就能开。";
        let e = "ATS/PSCADA 无显示问题结论：校验通过但不绘制。";
        let f = "PSCADA 供电显示语义与方向：送电/停电驱动前台仿真状态。";

        assert!(coverage_ratio(a, b) > 0.2, "换了个说法，覆盖率该过闸，实际 {}", coverage_ratio(a, b));
        assert!(coverage_ratio(c, d) < 0.15, "只是同领域，覆盖率该低，实际 {}", coverage_ratio(c, d));
        assert!(coverage_ratio(e, f) < 0.2, "同主题不同事，覆盖率该低，实际 {}", coverage_ratio(e, f));
        assert!(coverage_ratio(a, b) > coverage_ratio(e, f));
        assert!(coverage_ratio(e, f) > coverage_ratio(c, d));
    }

    #[test]
    fn coverage_ignores_the_evidence_block() {
        // ── 证据段必须从覆盖率里剔掉，这条钉住它 ────────────────────
        // 真机形状：「事故复盘清单化」和「前端选型单文件离线 HTML」两条毫不相干
        // 的记忆，因为同属一批流水，证据 id 列表几乎逐字相同。整段算覆盖率时
        // 它们拿到 0.378 —— 比真正同一件事的某对（0.371）还高。判据于是回答了
        // 「它们是不是同一批来的」（恒为真），而不是「它们说的是不是同一件事」。
        let same_evidence = "\n\n证据：mem_20261011_2ff1ab7d045d、mem_20261010_f9b96c102b4a、\
                             mem_20261010_e929b42087c3、mem_20261008_8c7e2aa5a998";
        let left = format!("事故复盘要清单化：每次出事按固定项走，不靠回忆。{same_evidence}");
        let right = format!("前端选型走单文件离线 HTML：不引框架、不打包、双击就能开。{same_evidence}");

        let whole = coverage_ratio(&left, &right);
        let content = content_coverage(&left, &right);
        assert!(whole > 0.15, "整段算会被共同证据抬起来，实际 {whole}");
        assert!(content < 0.05, "正文主体几乎不重叠，实际 {content}");
        assert!(content < whole, "剔除证据段必须让覆盖率下降");
    }

    #[test]
    fn coverage_does_not_need_an_evidence_block() {
        // 没有证据段的记忆（手写的）不能因此掉到 0：`split_evidence` 找不到
        // 标记时整段都是正文。这条防的是「加了剔除逻辑之后手写记忆再也合不了」。
        let a = "网关改写 body 后必须按新 body 重算 content-length，否则上游按旧长度截断。";
        let b = "网关改写 body（注入系统提示词）后仍发旧 content-length，上游按旧长度截断，JSON 断裂。";
        assert!(content_coverage(a, b) > 0.3, "无证据段也该正常算，实际 {}", content_coverage(a, b));
    }

    #[test]
    fn coverage_ignores_cjk_stopword_noise() {
        // 单字覆盖率对中文技术文本几乎没有区分力：任意两段笔记都共用
        // 「的/是/在/个/数/据」。真机最不像的一对（0.27 二元组覆盖）单字覆盖
        // 都有 0.41，中位数 0.75 —— 拿它当闸门等于没闸门。这里钉住「二元组不会
        // 被虚高」：两段完全不同主题的中文，覆盖率必须落在低位。
        let left = "记忆索引重建失败时要先看文件指纹，别急着删库。";
        let right = "赛车轮胎温度窗口在八十到一百一十度之间，超出就滑。";
        assert!(coverage_ratio(left, right) < 0.35, "无关主题覆盖率虚高：{}", coverage_ratio(left, right));
    }

    #[test]
    fn dedup_merges_only_when_the_short_side_is_explained() {
        // 三档各自的门槛。合并要同时过三道：覆盖度（0.2）、相似分（0.72）、
        // 方向明确（`directed`）；连边只看相似分。
        //
        // 为什么合并的覆盖率门槛能低到 0.2：覆盖率量的已经是**正文主体**（证据段
        // 剔除），而合并不删正文 —— 内容会被并入幸存者，退出检索的只是「另一条
        // 同样的结论」。真机 44 对在 0.2 处的分档是 42 合并 / 2 连边，那两条连边
        // 正是唯一真正无关的两对（覆盖 0.026 与 0.042）。
        let gate = |score: f64, coverage: f64| dedup_gate(score, coverage, true, true, 0.2, 0.72, 0.72);
        assert_eq!(gate(0.723, 0.62), PairOutcome::Merge);
        assert_eq!(gate(0.892, 0.55), PairOutcome::Merge);
        assert_eq!(gate(0.72, 0.204), PairOutcome::Merge);
        // 覆盖不够但确实像：连边，两条都留
        assert_eq!(gate(0.745, 0.042), PairOutcome::Relate);
        // 连相似分都不够：作废（连边会让关联图重新变成完全图）
        assert_eq!(gate(0.60, 0.30), PairOutcome::Retire("auto-below-threshold"));
        // 同 hash 无条件合 —— 那是同一段字节，不存在「各有独有内容」
        assert_eq!(gate(1.0, 0.0), PairOutcome::Merge);
        // ── 覆盖率高但相似分低：降级成连边，不能合 ──────────────────
        // 典型来源是「两条短结论都引用了同一段长文本」：短边确实被长边含住，
        // 覆盖率虚高，但两条在讲不同的事。合并它们等于把两个结论压成一条。
        assert_eq!(
            dedup_gate(0.66, 0.95, true, true, 0.2, 0.72, 0.72),
            PairOutcome::Retire("auto-below-threshold"),
            "连边门槛也要过：0.66 < relateMinScore"
        );
        assert_eq!(
            dedup_gate(0.66, 0.95, true, true, 0.2, 0.72, 0.60),
            PairOutcome::Relate,
            "连边门槛降到 0.60 就能连边了，但合并门槛 0.72 仍然过不了"
        );
    }

    /// 跨层与方向不明这两条闸：都不许合并，但都还能连边。
    ///
    /// `directed = false` 是那些老载荷（没有 `keep`）—— 判据不能猜方向，
    /// 猜错就是把该留的那条标成「已被取代」，而它正是巡检认为「更全」的候选。
    #[test]
    fn dedup_never_merges_across_layers_or_without_direction() {
        assert_eq!(
            dedup_gate(0.95, 0.9, false, true, 0.2, 0.72, 0.72),
            PairOutcome::Relate,
            "跨层不合并：l1 并入 l2 会丢掉发生顺序"
        );
        assert_eq!(
            dedup_gate(0.95, 0.9, true, false, 0.2, 0.72, 0.72),
            PairOutcome::Relate,
            "方向不明不合并：猜错方向会把正主埋掉"
        );
        // 唯独同 hash 例外：两段字节相同，方向猜错也还是同一段字
        assert_eq!(
            dedup_gate(1.0, 0.0, false, true, 0.2, 0.72, 0.72),
            PairOutcome::Merge,
            "同 hash 跨层也合"
        );
    }

    /// `supersede` 的三态：方向固定，判据只剩「旧条还有没有独有内容」。
    #[test]
    fn supersede_gate_merges_only_when_the_old_body_is_explained() {
        assert_eq!(supersede_gate(0.30, true, 0.2), PairOutcome::Merge);
        assert_eq!(supersede_gate(0.199, true, 0.2), PairOutcome::Retire("auto-not-covered"));
        assert_eq!(
            supersede_gate(0.95, false, 0.2),
            PairOutcome::Relate,
            "跨层有关联，但不能合并"
        );
        assert_eq!(
            supersede_gate(0.0, false, 0.2),
            PairOutcome::Relate,
            "跨层优先：连边不涉及正文，覆盖不到也不该被作废"
        );
    }

    /// 配对型建议的两端 id 取法：`dedup` 的 `target` 就是 id，`supersede` 的
    /// `target` 是 `新->旧` 复合串。
    ///
    /// 这条判据错了不报错，只会让 supersede 全被当成 `auto-target-missing`：
    /// 拿复合串去 `live_end_of` 永远查不到，于是每一条都被当场作废 ——
    /// 包括真正还需要处理的那批。
    #[test]
    fn supersede_ids_come_from_the_payload_not_the_target() {
        let row = index::ReviewRow {
            id: 1,
            kind: "supersede".to_string(),
            target: "new-id->old-id".to_string(),
            payload: json!({ "newId": "new-id", "oldId": "old-id" }),
            confidence: 0.9,
            status: "pending".to_string(),
            created_at: 0,
            resolved_at: None,
            resolution: None,
        };
        assert_eq!(pair_this_id(&row), Some("new-id"));
        assert_eq!(pair_other_id(&row), "old-id");

        let row = index::ReviewRow {
            kind: "dedup".to_string(),
            target: "target-id".to_string(),
            payload: json!({ "candidate": "candidate-id", "score": 0.8, "keep": "candidate" }),
            ..row
        };
        assert_eq!(pair_this_id(&row), Some("target-id"));
        assert_eq!(pair_other_id(&row), "candidate-id");
    }

    /// 老载荷没有 `newId` 时要退回 `target`，不能直接判 None 让整条建议消失。
    #[test]
    fn supersede_falls_back_to_target_when_payload_is_old() {
        let row = index::ReviewRow {
            id: 2,
            kind: "supersede".to_string(),
            target: "legacy-target".to_string(),
            payload: json!({ "oldId": "old-id" }),
            confidence: 0.9,
            status: "pending".to_string(),
            created_at: 0,
            resolved_at: None,
            resolution: None,
        };
        assert_eq!(pair_this_id(&row), Some("legacy-target"));
    }

    /// supersede 的自动判据：旧条正文有没有被新条解释掉。
    ///
    /// 用真机两条 supersede 的实际正文当样本，另外补一条「新条只讲了一半」的反例
    /// —— 那种情况旧条还有独有内容，判失效就是丢东西。
    ///
    /// ── 这条测试曾经红过，红的正是判据本身（2026-11-24 第三次 CI）────────
    /// 当时判据调的是对称版 `content_coverage`，它取**两边较短的那条**当分母。
    /// 反例 `partial` 只有 5 个 token，于是短边成了它、旧条覆盖它 3 个 ——
    /// 算出来 0.600「过闸」，与「旧条还有 41 个 token 的独有内容」这个事实无关。
    /// 症状是「一条短句就把完整因果链判成失效并埋掉」，而且不报错、只在事后翻
    /// 演化链时才发现。现在判据用有向版 `content_coverage_of(old, new)`：
    /// 同一个反例算出来 0.067。
    ///
    /// 保留对称版那一行断言（`sym 0.600`）是故意的：它把这个坑钉在原地，
    /// 谁把判据改回对称版，这里就会立刻指着说明为什么不行。
    #[test]
    fn supersede_coverage_is_directional_on_real_bodies() {
        let old = "网关改写 body（注入系统提示词）后仍把客户端旧 content-length 发给上游，\
                   上游按旧长度截断请求体，导致 JSON 断裂。改为由插件按实际 body 重算长度并重写 host。";
        let new = "网关改写 body 后，由插件按新 body 重新生成 content-length 并覆写 host，\
                   禁止沿用客户端原值。透传旧长度使上游按错误长度读取请求体，读到一半即停、JSON 断裂。";
        // 有向：旧条被新条解释掉多少（分母是旧条）。
        let coverage = content_coverage_of(old, new);
        assert!(
            coverage >= 0.20,
            "同一件事的两次表述覆盖率过低（{coverage:.3}），判失效会被永久挡住"
        );

        // ── 反例：新条只讲了「要重写 host」这一个点 ────────────────
        let partial = "插件要重写 host。";
        let partial_directed = content_coverage_of(old, partial);
        assert!(
            partial_directed < 0.20,
            "只讲了一个点的短句不该过闸（有向覆盖率 {partial_directed:.3}）"
        );
        // 对称版会算出 0.6 —— 这正是当初误判的来源，写在这里防回退。
        let partial_symmetric = content_coverage(old, partial);
        assert!(
            partial_symmetric > 0.5,
            "对称版本就该虚高（{partial_symmetric:.3}），它是无方向问题的尺子"
        );
        assert!(
            partial_directed < partial_symmetric,
            "有向覆盖率必须严格低于对称版：短的新条不该抬高它"
        );

        // 完全无关的两条：两种算法都必须远低于门槛。
        let unrelated = "赛车轮胎温度窗口在八十到一百一十度之间，超出就滑。";
        assert!(content_coverage_of(old, unrelated) < 0.20);
        assert!(content_coverage(old, unrelated) < 0.20);
    }

    #[test]
    fn dedup_without_keep_is_never_merged_but_can_still_relate() {
        // 方向没写清楚的建议一律不合并：老载荷没有 `keep`，`apply_verdict` 会按
        // 「新条取代候选」处理，而巡检给的 `candidate` 恰恰是该留的那条 ——
        // 自动合并一条方向不明的建议，等于把正主标成「已被取代」。
        //
        // 但「方向不明」不等于「这条建议没用」：两条确实相关这件事仍然成立，
        // 连边是纯新增、不需要方向，所以它落在 `Relate` 而不是留队列。
        assert_eq!(
            dedup_gate(1.0, 0.0, true, false, 0.2, 0.72, 0.72),
            PairOutcome::Relate,
            "同 hash 但不带方向：连边（两条内容一样，连边也无害）"
        );
        assert_eq!(
            dedup_gate(0.72, 0.62, true, false, 0.2, 0.72, 0.72),
            PairOutcome::Relate,
            "覆盖率够但不带方向：只能连边"
        );
        // 带上方向就恢复成能合
        assert_eq!(
            dedup_gate(0.72, 0.62, true, true, 0.2, 0.72, 0.72),
            PairOutcome::Merge
        );
    }

    #[test]
    fn dedup_auto_merge_lets_exact_duplicates_through() {
        // 同一段话导两遍（score 1.0）没有信息损失，阈值再高也该放行。
        // 近似项现在由 `dedup_gate` 的两把尺子决定（见上一个用例），
        // 这里的阈值只回答「值得比一比吗」。
        assert!(dedup_score_ok(1.0, 1.0));
        assert!(dedup_score_ok(0.932, 0.9));
        assert!(dedup_score_ok(0.9, 0.9));
        assert!(!dedup_score_ok(0.892, 0.9));
        // 降到 0.72 之后，0.723 这种「同一件事的两次表述」进入可比范围 ——
        // 能不能合由覆盖率说了算，不再由这个数字一票否决。
        assert!(dedup_score_ok(0.723, 0.72));
    }

    /// 造一条带模型裁决的 `dedup` 建议。
    fn dedup_row(op: &str, confidence: f64) -> index::ReviewRow {
        index::ReviewRow {
            id: 1,
            kind: "dedup".to_string(),
            target: "new".to_string(),
            payload: json!({
                "candidate": "old",
                "score": 0.80,
                "modelOp": op,
                "modelConfidence": confidence,
            }),
            confidence: 0.80,
            status: "pending".to_string(),
            created_at: 0,
            resolved_at: None,
            resolution: None,
        }
    }

    /// L4 的四个操作到三种动作的映射，以及两条保护线。
    ///
    /// 这条映射错了不会报错，只会静默地丢内容：`NONE` 被当成 `Merge` 就是把
    /// 两条无关结论压成一条；`ADD` 被当成 `Merge` 同理。所以逐格钉住。
    #[test]
    fn model_verdict_maps_four_operations_to_three_actions() {
        let mut new = MemoryEntry::draft("new", "新条", MemoryKind::Insight, "新条的正文");
        new.layer = Layer::L2;
        let mut old = MemoryEntry::draft("old", "旧条", MemoryKind::Insight, "旧条的正文");
        old.layer = Layer::L2;

        assert_eq!(
            model_verdict(&dedup_row("ADD", 0.95), &new, &old),
            Some(PairOutcome::Relate),
            "两条都留，只连边"
        );
        assert_eq!(
            model_verdict(&dedup_row("NONE", 0.95), &new, &old),
            Some(PairOutcome::Retire("auto-model-none")),
            "连相关都不是：这条建议该被作废，不是该被合并"
        );
        assert_eq!(
            model_verdict(&dedup_row("UPDATE", 0.95), &new, &old),
            Some(PairOutcome::Merge)
        );
        assert_eq!(
            model_verdict(&dedup_row("DELETE", 0.95), &new, &old),
            Some(PairOutcome::Merge),
            "DELETE 也是合并，方向由载荷 keep=candidate 表达"
        );
    }

    /// 两条保护线：置信度不够就交回词面判据；跨层只连边不合并。
    ///
    /// ── 为什么跨层要降级 ──────────────────────────────────────
    /// 与 `dedup_gate` 同一条理由：把一条 l1 流水并进 l2 沉淀，会丢掉「什么时候
    /// 知道的」这一层信息，而合并后被取代那条退出检索，丢的部分再也读不到。
    #[test]
    fn model_verdict_falls_back_below_threshold_and_never_merges_across_layers() {
        let mut new = MemoryEntry::draft("new", "新条", MemoryKind::Insight, "新条的正文");
        new.layer = Layer::L2;
        let mut old = MemoryEntry::draft("old", "旧条", MemoryKind::Insight, "旧条的正文");
        old.layer = Layer::L2;

        // 默认阈值 0.85：0.60 的结论只当参考，动作仍归 `dedup_gate`。
        assert_eq!(model_verdict(&dedup_row("UPDATE", 0.60), &new, &old), None);
        // 没有模型结论的老载荷：完全走原判据，行为与加 L4 之前一致。
        let mut row = dedup_row("UPDATE", 0.95);
        row.payload = json!({ "candidate": "old", "score": 0.80 });
        assert_eq!(model_verdict(&row, &new, &old), None);
        // 自创操作名不能被执行。
        assert_eq!(model_verdict(&dedup_row("MERGE", 0.99), &new, &old), None);

        let mut l1 = new.clone();
        l1.layer = Layer::L1;
        assert_eq!(
            model_verdict(&dedup_row("UPDATE", 0.95), &l1, &old),
            Some(PairOutcome::Relate),
            "跨层同一件事：连边，不合并"
        );
    }

    /// 造一条「已被 `superseded_by` 指向 `to`」的记忆，用来模拟串行合并的中途状态。
    fn superseded(id: &str, to: &str) -> MemoryEntry {
        let mut entry = MemoryEntry::draft(id, id, MemoryKind::Knowledge, format!("{id} 的正文"));
        entry.superseded_by = Some(to.to_string());
        entry.dedup_status = DedupStatus::Merged;
        entry
    }

    #[test]
    fn live_end_follows_the_whole_supersede_chain() {
        // 真机形状：A ← B ← C，三条都是「D 盘空间」的不同说法，串行合并后
        // 会串成一条链。`live_end_of` 必须一次走到底 —— 只走一步的话，
        // C 会解到 B（已是死条目），下一次合并就把正文并进一条谁都不读的记录。
        let all = vec![
            MemoryEntry::draft("A", "A", MemoryKind::Knowledge, "活的那条"),
            superseded("B", "A"),
            superseded("C", "B"),
        ];
        assert_eq!(live_end_of(&all, "A").as_deref(), Some("A"));
        assert_eq!(live_end_of(&all, "B").as_deref(), Some("A"), "B 已被 A 吃掉，落点必须是 A");
        assert_eq!(live_end_of(&all, "C").as_deref(), Some("A"), "C 要跨两步才走到活条目");
    }

    #[test]
    fn live_end_rejects_what_cannot_be_a_fact() {
        // 三种「这一端没内容可合」的情况。往它们身上并正文都是错的：
        //   * 链终点不存在（文件被人手删了）；
        //   * 链终点在回收站里；
        //   * 链终点被人反驳过 —— 并进去等于用一段没被否掉的结论给它背书。
        let all = vec![
            MemoryEntry::draft("A", "A", MemoryKind::Knowledge, "正文"),
            superseded("B", "ghost"),
        ];
        assert_eq!(live_end_of(&all, "B"), None, "链走到不存在的 id，不能算活条目");

        let mut trashed = MemoryEntry::draft("C", "C", MemoryKind::Knowledge, "正文");
        trashed.deleted_at = Some(crate::server::logging::now_ms());
        let mut refuted = MemoryEntry::draft("D", "D", MemoryKind::Knowledge, "正文");
        refuted.refuted_at = Some(crate::server::logging::now_ms());
        let all = vec![trashed, refuted, superseded("E", "C"), superseded("F", "D")];
        assert_eq!(live_end_of(&all, "E"), None, "终点在回收站");
        assert_eq!(live_end_of(&all, "F"), None, "终点被反驳");
        assert_eq!(live_end_of(&all, ""), None, "空 id");
    }

    #[test]
    fn live_end_treats_a_cycle_as_unresolvable() {
        // ── 真机有 7 条取代关系成环 ──────────────────────────────────
        // 它们全是「用户在界面上把这批记忆删进回收站」留下的：A→B、B→A 互指，
        // 两条都 `deleted_at` 非空。`resolve_supersede_chain` 走满 32 步返回当前
        // 节点，于是 `live_end_of` 会拿到环里某条 —— 如果环里混进一条活条目
        // （A→B、B→A 但只有 A 被删），合并就会把正文并进随机那一条。
        //
        // 环里没有「终点」这个概念，判据只能是「解不出来」。
        let mut a = MemoryEntry::draft("A", "A", MemoryKind::Knowledge, "x");
        a.superseded_by = Some("B".into());
        let mut b = MemoryEntry::draft("B", "B", MemoryKind::Knowledge, "y");
        b.superseded_by = Some("A".into());
        b.deleted_at = Some(crate::server::logging::now_ms());
        let all = vec![a, b];
        assert_eq!(live_end_of(&all, "A"), None, "环里挑不出一条活终点");
        assert_eq!(live_end_of(&all, "B"), None);
    }

    #[test]
    fn supersede_chain_survives_a_cycle() {
        // 人为把 frontmatter 改坏成 A→B→A 时不能挂死：走满上限就返回当前节点。
        // 返回哪一条不重要，重要的是有限步返回 —— 巡检查询里死循环会让整个
        // 记忆中枢看起来「卡住了」。
        let mut a = MemoryEntry::draft("A", "A", MemoryKind::Knowledge, "x");
        a.superseded_by = Some("B".into());
        let mut b = MemoryEntry::draft("B", "B", MemoryKind::Knowledge, "y");
        b.superseded_by = Some("A".into());
        let all = vec![a, b];
        let end = resolve_supersede_chain(&all, "A");
        assert!(end == "A" || end == "B");
    }

    #[test]
    fn one_ended_pair_is_still_mergeable_and_same_end_is_not() {
        // ── 这条钉住 `pair_outcome` 与链式合并的一致性 ──────────────
        // 早先 `retire_reason_of` 写的是「任一端 `superseded_by` 非空就作废」，
        // 而 `dedup_auto_verdict` 要求「两端都 is_trusted()」。两边叠加的结果：
        //   * 真机 44 对里 7 个 id 横跨多对，第一对合完之后，另两对既进不了
        //     合并（一端失效）、又被提前作废（一端失效）—— 而 C 的正文
        //     一个字都没进 A。
        // 现在两端各自解链，判据变成「解完是不是同一条」，两种情况才分得开。
        let all = vec![
            MemoryEntry::draft("A", "A", MemoryKind::Knowledge, "A 的正文"),
            superseded("B", "A"),
            MemoryEntry::draft("C", "C", MemoryKind::Knowledge, "C 的正文"),
        ];
        // A↔C：两端解出不同 id → 还能合（`pair_outcome` 会继续看覆盖率）
        assert_ne!(live_end_of(&all, "A"), live_end_of(&all, "C"));
        // B↔A：两端解到同一条 → 无事可做（`pair_outcome` 给 auto-already-resolved）
        assert_eq!(live_end_of(&all, "B"), live_end_of(&all, "A"));
    }

    /// `pair_outcome` 对 supersede 必须用**旧条**当覆盖率分母。
    ///
    /// 这是 `supersede_merges_only_when_the_old_body_is_explained` 的端到端版本：
    /// 那条测纯函数 `content_coverage_of`，这条测「判据在真实行上取的是哪个方向」。
    /// 只测纯函数挡不住「函数写对了但 `pair_outcome` 调错顺序」—— 而那个错法的
    /// 后果是短的新条把长的旧条埋掉，且没有任何报错。
    #[test]
    fn supersede_pairs_use_the_old_body_as_the_denominator() {
        let long_old = "网关改写 body 后仍把客户端旧 content-length 发给上游，\
                        上游按旧长度截断请求体，导致 JSON 断裂。改为由插件按实际 body \
                        重算长度并重写 host，同时要处理分块传输的边界情况。";
        let short_new = "插件要重写 host。";
        let all = vec![
            MemoryEntry::draft("new", "new", MemoryKind::Knowledge, short_new),
            MemoryEntry::draft("old", "old", MemoryKind::Knowledge, long_old),
        ];
        let row = index::ReviewRow {
            id: 1,
            kind: "supersede".to_string(),
            // 真机上 target 就是 `新->旧` 这个复合串。
            target: "new->old".to_string(),
            payload: json!({ "newId": "new", "oldId": "old" }),
            confidence: 0.9,
            status: "pending".to_string(),
            created_at: 0,
            resolved_at: None,
            resolution: None,
        };
        assert_eq!(
            pair_outcome(&row, &all),
            PairOutcome::Retire("auto-not-covered"),
            "短的新条不该把长的旧条判失效 —— 旧条还有大量独有内容"
        );

        // 反过来：新条确实把旧条讲全了 → 该合并。
        let full_new = "网关改写 body 后仍把客户端旧 content-length 发给上游，\
                        上游按旧长度截断请求体，导致 JSON 断裂。改为由插件按实际 body \
                        重算长度并重写 host。";
        let all = vec![
            MemoryEntry::draft("new", "new", MemoryKind::Knowledge, full_new),
            MemoryEntry::draft("old", "old", MemoryKind::Knowledge, long_old),
        ];
        assert_eq!(
            pair_outcome(&row, &all),
            PairOutcome::Merge,
            "旧条的内容已被新条讲全，该判失效"
        );
    }

    /// 批量解除置顶只动**画像**，而且 `pinned` 与标记一起摘。
    ///
    /// 真机撞到的情况：某一轮生成的 16 条画像被打上 `pin`，于是每次重生成都
    /// 「保留」它们，四类条数从此锁死在 8 条、置顶数锁死在 16 —— 用户点多少次
    /// 「重新生成全量画像」都不变。出口是这个批量动作；这里钉住它两条边界：
    /// ① 普通记忆的 `pinned` 是浏览页的排序偏好，不许连坐；
    /// ② 只摘 `pinned` 不摘 `pin` 等于没解（`profile_view` 与 `build_profile`
    ///    判的是「两者同时成立」），所以两者必须一起变。
    #[test]
    fn unpin_all_profile_clears_only_profile_pins() {
        crate::test_support::isolated(
            concat!(module_path!(), "::unpin_all_profile_clears_only_profile_pins"),
            || {
                let mut pinned_profile = MemoryEntry::draft(
                    "p-pin",
                    "被钉住的画像",
                    MemoryKind::Profile,
                    "这条结论在重生成时会被保留，于是四类条数永远不变。",
                );
                pinned_profile.layer = Layer::L2;
                pinned_profile.tags = vec!["identity".to_string(), PROFILE_PIN_TAG.to_string()];
                pinned_profile.pinned = true;
                store::save(&pinned_profile).expect("画像应当能落盘");

                let mut pinned_ordinary = MemoryEntry::draft(
                    "m-pin",
                    "被钉住的普通结论",
                    MemoryKind::Insight,
                    "普通记忆的置顶只是浏览页的排序偏好，与画像重生成无关。",
                );
                pinned_ordinary.layer = Layer::L2;
                pinned_ordinary.pinned = true;
                store::save(&pinned_ordinary).expect("普通记忆应当能落盘");

                let view = unpin_all_profile().expect("批量解除应当成功");
                assert_eq!(view["cleared"], 1, "只该解画像那一条：{view}");

                let reloaded = store::load_all();
                let profile = reloaded.iter().find(|entry| entry.id == "p-pin").expect("画像还在");
                assert!(!profile.pinned, "画像的 pinned 应当被摘下");
                assert!(
                    !profile.tags.iter().any(|tag| tag == PROFILE_PIN_TAG),
                    "标记没摘掉等于没解 —— 重生成照样跳过它：{:?}",
                    profile.tags
                );
                let ordinary = reloaded.iter().find(|entry| entry.id == "m-pin").expect("普通记忆还在");
                assert!(ordinary.pinned, "普通记忆的置顶不该被连坐");

                // 再跑一次：已经没有钉子可摘，`cleared` 必须是 0（幂等）。
                let again = unpin_all_profile().expect("第二次也应当成功");
                assert_eq!(again["cleared"], 0, "没有钉子时不该报解除了东西：{again}");
            },
        );
    }
}