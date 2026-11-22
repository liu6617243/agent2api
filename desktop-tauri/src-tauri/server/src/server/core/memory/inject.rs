//! 记忆中枢 · 请求级注入（把「硬规则 + 相关经验」送进每一次对话）。
//!
//! ── 这个文件解决什么问题 ────────────────────────────────────
//! 记忆存了几千条、检索也能用，但**模型不会主动去查**。原来的设计把「什么时候
//! 读记忆」完全交给客户端的提示词块（`agents::INSTRUCTION_BLOCK` 里那句「会话
//! 开始时先调 `memory_core`」），而那句话只是建议：客户端可以不读、模型可以忽略、
//! 换了客户端就没了。实测结果是 `memory_core` 调用 1 次、真检索 0 次 —— 记忆
//! 写了等于没写。
//!
//! 所以这里换一个位置：**网关在转发前替模型把该看的那几条塞进去**。模型不需要
//! 自觉，因为它一睁眼就已经看到了。
//!
//! ── 为什么不是「把记忆全塞进去」────────────────────────────
//! 几千条全塞是灾难：烧 token、稀释注意力、还会让模型拿三周前的结论去改刚重构
//! 的代码。所以注入分两段，各有硬预算：
//!
//! | 段 | 内容 | 依据 |
//! |---|---|---|
//! | 必须遵守 | `pinned` + `preference` + `profile` | 用户亲手钉的、或者明确是「长期要求」的 |
//! | 相关经验 | 命中本次请求里的**具体线索**的 pitfall / decision / knowledge | 路径、标识符、命令名 |
//!
//! ── 为什么用「线索命中」而不是语义检索 ──────────────────────
//! 用户的原话是「做一件事时会自然回忆以前是否做过」。人回忆的触发点很具体：
//! 又是这个文件、又是这条命令、又是这个报错。而向量/泛语义检索给的是「读起来
//! 差不多」的东西 —— 在记忆这种长尾语料上，噪音远大于信号（一次 `memory_search`
//! 返回 8 条里能有 2 条相关就算不错）。
//!
//! 所以这里只认**字符串级**命中：请求里出现了 `scripts/deliver.mjs`，就去找
//! `files` 里写着这个路径、或标题里写着这个名字的记忆。命不中就什么都不注入
//!（宁可不给，也不要给错的 —— 错的记忆比没有记忆更贵）。
//!
//! ── 为什么注入点在网关，而不是在 MCP ────────────────────────
//! MCP 是「模型来问」，本模块是「网关主动给」。前者依赖模型自觉，后者不依赖。
//! 两条路并存：注入负责「一定会看到的硬约束」，MCP 负责「想深挖时的检索」。
//!
//! ── 幂等 ────────────────────────────────────────────────────
//! 注入块自带 [`MARKER`] 标记。同一份 body 被注入两次（重试、原生转发与 Chat
//! 转换两条路都走到）时第二次直接跳过 —— 否则一次请求里会出现两段记忆，白烧
//! token 还让模型以为是两条不同的结论。
//!
//! ── 绝不因为注入失败而让请求失败 ────────────────────────────
//! 这是转发主链路。读快照失败、预算算错、正则写坏 —— 任何一种都只能让「这次
//! 没有记忆」而不是「这次请求 500」。所以所有函数返回 `Option` / `bool`，
//! 没有 `Result`，也没有 `unwrap`。

use std::collections::HashSet;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use regex::Regex;
use serde_json::Value;

use super::config;
use super::model::{
    estimate_tokens, truncate_chars, MemoryEntry, MemoryKind, Layer, AUTO_EXTRACT_AGENT,
};
use super::store;

/// 注入块的起止标记。
///
/// 用 HTML 注释风格而不是纯文本：万一用户把注入内容贴回聊天里，它也不会被
/// 当成正常内容渲染出来。同时它也是幂等判据（见模块头）。
pub const MARKER: &str = "<agent2api-memory>";
const MARKER_END: &str = "</agent2api-memory>";

/// 一次注入的全部旋钮。
#[derive(Debug, Clone, Copy)]
pub struct Settings {
    pub enabled: bool,
    /// 整块的总预算（硬上限）
    pub max_tokens: usize,
    /// 「必须遵守」那一段的预算
    pub rules_tokens: usize,
    /// 「相关经验」那一段的预算
    pub experience_tokens: usize,
    /// 经验最多几条
    pub max_items: usize,
    /// 是否用 `files` 字段做路径精确命中
    pub path_match: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            enabled: true,
            max_tokens: 600,
            rules_tokens: 260,
            experience_tokens: 340,
            max_items: 6,
            path_match: true,
        }
    }
}

/// 读当前生效的注入设置。
pub fn settings() -> Settings {
    let max_tokens = config::usize_of("inject.maxTokens").min(4000);
    Settings {
        enabled: config::bool_of("inject.enabled"),
        max_tokens,
        // 两段预算各自夹到总预算内：用户把总预算调到 200 时，两段加起来也不能
        // 超过它 —— 否则「总预算」就成了一个说了不算的数字。
        rules_tokens: config::usize_of("inject.rulesTokens").min(max_tokens),
        experience_tokens: config::usize_of("inject.experienceTokens").min(max_tokens),
        max_items: config::usize_of("inject.maxItems").clamp(1, 20),
        path_match: config::bool_of("inject.pathMatch"),
    }
}

/// 从请求文本里抽「线索」：路径、反引号片段、标识符。
///
/// ── 为什么要抽而不是直接拿整段请求去匹配 ────────────────────
/// 整段请求里 90% 是自然语言，拿它去 `contains` 只会命中一堆「的」「了」「这个」。
/// 抽出来的这几类东西有个共同点：**它们几乎只会在讲同一件事时重复出现**。
/// 一个文件路径不会碰巧出现在无关对话里。
fn regexes() -> &'static [Regex] {
    static CACHE: OnceLock<Vec<Regex>> = OnceLock::new();
    CACHE.get_or_init(|| {
        // 写坏一条只丢那一条，不让整个注入挂掉（这里没有 unwrap：release 是
        // panic=abort，一次 panic 会带走整个网关进程）
        [
            // 反引号里的内容：用户和模型都习惯用它包命令、路径、标识符
            r"`([^`\n]{2,120})`",
            // 带扩展名的路径 / 文件名
            r"(?i)[a-z0-9_.\-/\\]{3,120}\.(?:rs|ts|tsx|js|jsx|mjs|cjs|json|toml|ya?ml|md|py|go|sql|css|html|sh|ps1)",
            // snake_case 标识符（至少两段）
            r"\b[a-z][a-z0-9]*(?:_[a-z0-9]+)+\b",
            // camelCase **不在**这里：它必须在原文上匹配，见 `camel()` 与 `probes`
        ]
        .iter()
        .filter_map(|pattern| Regex::new(pattern).ok())
        .collect()
    })
}

/// 从一段请求文本里抽线索（已小写、已去重）。
///
/// ── camelCase 为什么要单独一遍 ──────────────────────────────
/// 其余三条正则都在小写化之后的文本上跑（大小写不该影响路径与 snake_case 的
/// 命中）。但 camelCase 的定义就是「小写开头 + 大写转折」—— 小写化之后
/// `injectChat` 变成 `injectchat`，那条正则永远匹配不到任何东西，等于白写。
/// 所以它单独在原文上跑一遍，命中的结果再统一小写化（下游匹配一律小写）。
///
/// ── 为什么要分桶而不是「先来先占」───────────────────────────
/// 24 个名额是硬上限。第一版按正则顺序 push，于是命中的先后就等于优先级 ——
/// 真机上一次 `ls` 回显的 14 个光杆文件名（`agents.rs`、`config.rs`…）先把名额
/// 占掉，紧接着才是真正被改的那个 `desktop-tauri/.../inject.rs`。而这两档的
/// 信息量完全不同：带目录的路径在仓库里唯一，光杆文件名在仓库里有十几个同名。
///
/// 所以按**信息量**分三桶，路径桶先满额再轮到标识符桶，光杆文件名最后一档、
/// 且名额最少（[`NAME_BUCKET`]）。这样一次 `ls` 无论列多少文件，都挤不掉那条
/// 唯一的路径。
pub fn probes(text: &str) -> Vec<String> {
    /// 桶容量：路径最多 10 条（一次任务改不了十个文件），标识符最多 10 条，
    /// 光杆文件名只留 4 条 —— 它是最弱的一档，多了就是纯占位。
    const PATH_BUCKET: usize = 10;
    const IDENT_BUCKET: usize = 10;
    const NAME_BUCKET: usize = 4;

    let mut paths: Vec<String> = Vec::new();
    let mut idents: Vec<String> = Vec::new();
    let mut names: Vec<String> = Vec::new();

    let lowered = text.to_lowercase();
    for (index, pattern) in regexes().iter().enumerate() {
        for capture in pattern.captures_iter(&lowered) {
            let raw = capture
                .get(1)
                .or_else(|| capture.get(0))
                .map(|matched| matched.as_str())
                .unwrap_or("");
            // index 0 = 反引号 / 1 = 带扩展名的路径 / 2 = snake_case
            let bucket = match index {
                0 => Bucket::Ident,
                1 => Bucket::PathOrName,
                _ => Bucket::Ident,
            };
            push_probe(&mut paths, &mut idents, &mut names, bucket, raw);
        }
    }
    if let Some(pattern) = camel() {
        for capture in pattern.captures_iter(text) {
            let raw = capture
                .get(0)
                .map(|matched| matched.as_str())
                .unwrap_or("");
            push_probe(
                &mut paths,
                &mut idents,
                &mut names,
                Bucket::Ident,
                &raw.to_lowercase(),
            );
        }
    }

    let mut out: Vec<String> = Vec::new();
    for (bucket, limit) in [
        (paths, PATH_BUCKET),
        (idents, IDENT_BUCKET),
        (names, NAME_BUCKET),
    ] {
        for value in bucket.into_iter().take(limit) {
            if !out.iter().any(|known| known == &value) {
                out.push(value);
            }
        }
    }
    out
}

/// 线索的三档。档位只影响排序，不影响能不能命中。
#[derive(Debug, Clone, Copy)]
enum Bucket {
    /// 反引号里的片段、snake_case、camelCase —— 用户/模型明说「就是这个东西」
    Ident,
    /// 带扩展名的串：含目录分隔的进路径桶，光杆文件名的进名字桶
    PathOrName,
}

/// camelCase 标识符（在**原文**上匹配，见 [`probes`] 的说明）。
///
/// 返回 `Option` 而不是 `Regex`：这条正则和上面那三条一样可能被改坏，而
/// 「改坏一条正则」绝不能让整条转发链路 panic（release 是 panic=abort，
/// 一次 panic 带走的是整个网关进程，不是这一次注入）。
fn camel() -> Option<&'static Regex> {
    static CACHE: OnceLock<Option<Regex>> = OnceLock::new();
    CACHE
        .get_or_init(|| Regex::new(r"\b[a-z][a-z0-9]*(?:[A-Z][a-z0-9]+)+\b").ok())
        .as_ref()
}

/// 从**记忆正文**里抽出「像仓库文件路径」的字符串（给 `apply_derived` 继承用）。
///
/// ── 为什么需要从正文里抽 ────────────────────────────────────
/// 注入的路径命中是权重最高的一档，但它读的是记忆的 `files` 字段。而绝大多数
/// 记忆的 `files` 是空的：
///
///   · 导入进来的会话流水（l1）根本没有 files 概念，路径全在正文里；
///   · 蒸馏 / 合并 / 抽取产出的 l2 原来是 `files: Vec::new()`。
///
/// 结果是「按文件命中」这一档对库里九成以上的条目永久失效。这里做一遍确定性
/// 抽取，把正文里出现过的路径补进新条目的 files。
///
/// ── 判据为什么这么严 ────────────────────────────────────────
/// 只认「带已知扩展名、且至少含一段目录分隔」的串：`chat.rs` 这种光杆文件名
/// 到处都是（同名的文件可能有十几个），收进来就是「命中一切」；而
/// `src/server/api/chat.rs` 这种带目录的串在整个仓库里唯一。
///
/// 反斜杠路径统一成正斜杠：Windows 上写出来的 `src\server\chat.rs` 与
/// `src/server/chat.rs` 是同一条路径，不归一化就等于两条都不命中。
pub fn harvest_paths(text: &str) -> Vec<String> {
    static CACHE: OnceLock<Option<Regex>> = OnceLock::new();
    let Some(pattern) = CACHE
        .get_or_init(|| {
            Regex::new(
                r"(?i)[a-z0-9_.\-]+(?:[/\\][a-z0-9_.\-]+)+\.[a-z0-9]{1,6}",
            )
            .ok()
        })
        .as_ref()
    else {
        return Vec::new();
    };
    let mut out: Vec<String> = Vec::new();
    for matched in pattern.find_iter(text) {
        let normalized = matched.as_str().replace('\\', "/");
        let normalized = normalized.trim_matches('/').to_string();
        // 太短的（`a/b.rs`）与太长的（一整行命令被当成路径）都丢掉
        let length = normalized.chars().count();
        if !(6..=200).contains(&length) {
            continue;
        }
        // 必须真的像个代码文件：最后一段要带扩展名且扩展名在名单里
        let Some(ext) = normalized.rsplit('.').next() else { continue };
        let ext = ext.to_lowercase();
        if !matches!(
            ext.as_str(),
            "rs" | "ts" | "tsx" | "js" | "jsx" | "mjs" | "cjs" | "json" | "toml"
                | "yaml" | "yml" | "md" | "py" | "go" | "sql" | "css" | "html" | "sh" | "ps1"
        ) {
            continue;
        }
        if !out.iter().any(|known| known == &normalized) {
            out.push(normalized);
        }
        if out.len() >= 8 {
            break;
        }
    }
    out
}

/// 临时目录 / 调试产物前缀：这些路径里的「文件名」是本机运行痕迹，不是仓库资产。
///
/// ── 为什么必须挡掉 ──────────────────────────────────────────
/// 本机实测抽出来的线索长这样：
///
/// ```text
/// /.agent2api/debug/last-request.js
/// .workbuddy/tmp/entries.js
/// .workbuddy/tmp/entries-full.js
/// ```
///
/// 它们全是**我自己排障时**写出的临时文件，`files` 里永远不会有一条记忆锚定到
/// 它们 —— 也就是说这几个名额是纯浪费，而名额总共只有 24 个。更糟的是它们排在
/// 前面（来自工具回显的最开头），会把真任务的路径挤出去。
const NOISE_PREFIXES: &[&str] = &[
    ".workbuddy/tmp/",
    "/.agent2api/debug/",
    ".agent2api/debug/",
    "/tmp/",
    "/temp/",
    "appdata/local/temp/",
    "/node_modules/",
    "/target/debug/",
    "/target/release/",
];

/// 语言 / 标准库 / SQL 的内部标识符，出现频率高但**不指向任何一条记忆**。
///
/// ── 判据为什么是「白名单式列举」而不是「猜」───────────────
/// snake_case 正则本身很宽（`\b[a-z][a-z0-9]*(?:_[a-z0-9]+)+\b`），凡是有下划线的
/// 都进。而工具回显里塞满了这类东西：`row_factory`、`ensure_ascii`、`deleted_at`、
/// `n_sum`。它们对「以前做过这件事吗」毫无指示性 —— 一条记忆不会因为提到
/// `deleted_at` 就和本次任务相关（那是每张表都有的列）。
///
/// 列举而不做启发式：这类词是有限的、稳定的，列举可读、可审计、可以随时加；
/// 启发式（比如「带 _at / _id 后缀就丢」）会误伤 `last_hit_at` 这种真字段名。
const NOISE_IDENTS: &[&str] = &[
    // Python 常用
    "row_factory", "ensure_ascii", "isinstance", "getattr", "setattr", "hasattr",
    "strftime", "strptime", "readlines", "writelines", "startswith", "endswith",
    "listdir", "walk_dir", "realpath", "basename", "dirname",
    // SQL / 表结构
    "deleted_at", "created_at", "updated_at", "valid_from", "valid_to",
    "group_concat", "insert_into", "order_by", "left_join", "inner_join",
    "not_null", "auto_increment", "primary_key",
    // 本仓常见但不指向具体结论的字段
    "n_sum", "max_tokens", "min_tokens", "content_type", "user_agent",
    "status_code", "request_id", "trace_id", "session_id",
];

/// 这个线索值不值得占一个名额。
///
/// 两道闸：临时/派生产物路径直接丢；纯内部标识符也丢。其余照旧交给打分环节 ——
/// 这里**不**做「像不像任务词」的判断，那种判断交给门槛（`floor_for`）。
fn noise_probe(value: &str) -> bool {
    let lowered = value.to_lowercase();
    if NOISE_PREFIXES.iter().any(|prefix| lowered.contains(prefix)) {
        return true;
    }
    // 文件名比较：`foo/entries.json` 与 `entries.json` 都算同一条
    if NOISE_IDENTS.iter().any(|name| lowered == *name) {
        return true;
    }
    // 无下划线的纯文件名不该被当成路径线索之外的噪音 —— 交给后续判据
    false
}

/// 把一条候选线索放进对应的桶。
///
/// ── 反引号片段为什么要额外体检 ──────────────────────────────
/// 反引号正则假设「成对出现」，但真实文本里经常是奇数个（模型把一句话里的
/// 三个反引号写进正文、或者代码围栏被截断）。真机实测抽出来过这么一条：
///
/// ```text
/// 这类 workbuddy 结构时需要谨慎，因为当前 system 里的
/// ```
///
/// —— 一整句中文，因为配对错位而落在两个反引号之间。它占一个名额、永远命中
/// 不了任何记忆。所以反引号这一档要求「不含空白且不含中文」：命令、路径、
/// 标识符都不含空格，句子必然含。
fn push_probe(
    paths: &mut Vec<String>,
    idents: &mut Vec<String>,
    names: &mut Vec<String>,
    bucket: Bucket,
    raw: &str,
) {
    let trimmed = raw.trim().trim_matches(|ch: char| {
        matches!(ch, '`' | '"' | '\'' | ',' | ';' | ')' | '(' | '[' | ']')
    });
    // ── 反斜杠必须归一化成正斜杠 ────────────────────────────────
    // Windows 上写 `D:\Personal\...\inject.rs`，正则的字符集不含冒号，匹配会从
    // `\personal\...` 开始 —— 于是同一条路径抽出两条探针（一条正斜杠、一条反
    // 斜杠），而记忆里存的是正斜杠形态，反斜杠那条永远命不中，白占名额。
    let value = trimmed.replace('\\', "/");
    let length = value.chars().count();
    // 太短的（`a` / `ok`）到处都是，太长的多半是一整行命令 —— 两种都不是
    // 「能定位到某条记忆」的线索
    if length < 3 || length > 120 {
        return;
    }
    if value.split_whitespace().count() > 1 {
        return;
    }
    if !value.is_ascii() {
        return;
    }
    if noise_probe(&value) {
        return;
    }
    let target = match bucket {
        Bucket::Ident => idents,
        // 归一化之后只剩正斜杠，判据也只看正斜杠
        Bucket::PathOrName => {
            if value.contains('/') {
                paths
            } else {
                names
            }
        }
    };
    if target.iter().any(|known| known == &value) {
        return;
    }
    target.push(value);
}

/// 这条记忆锚定的文件里有没有一条能和线索对上。
///
/// ── 为什么要双向比 + 单独比文件名 ────────────────────────────
/// 记忆里的路径和请求里的路径**很少逐字相同**，而三种写法都很常见：
///
/// | 记忆里 | 请求里 | 单向 `contains` 的结果 |
/// |---|---|---|
/// | `desktop-tauri/.../chat.rs` | `D:/Personal/.../chat.rs` | 漏（请求更长） |
/// | `D:/Personal/.../chat.rs` | `desktop-tauri/.../chat.rs` | 漏（记忆更长） |
/// | `desktop-tauri/.../chat.rs` | `chat.rs` | 漏（线索只有文件名） |
///
/// 三种都漏就等于「路径匹配」这个最强信号基本失效 —— 而它是唯一几乎不误报
/// 的信号（用户亲手锚定的文件）。所以这里三种都认：互相包含、以及文件名
/// 逐字相等。
///
/// ── 长度闸 ──────────────────────────────────────────────────
/// 线索短于 4 个字符直接不认；「互相包含」那一档要求两侧都 ≥ 8 个字符。
/// 少这两道闸，线索 `api`（3 个字符，抽得出来）会命中 `src/server/api/chat.rs`，
/// 拿满 3.0 分直接进注入块 —— 而「请求里出现了 api 这个词」根本不代表它在讲
/// 那个文件。唯一放宽的是「线索就是完整文件名」（`chat.rs`）：那本身就是精确匹配。
fn file_hit<'a>(mut files: impl Iterator<Item = &'a str>, probe: &str) -> bool {
    let probe_len = probe.chars().count();
    if probe_len < 4 {
        return false;
    }
    files.any(|file| {
        let file = file.trim().to_lowercase();
        if file.is_empty() {
            return false;
        }
        let file_len = file.chars().count();
        // 取最后一段当文件名：`desktop-tauri/.../chat.rs` 与 `D:\...\chat.rs`
        // 都要能落成 `chat.rs`（两种分隔符都得认，记忆里两种写法都有）。
        let name = file.rsplit(|ch| ch == '/' || ch == '\\').next().unwrap_or(&file);
        // 线索就是这条记忆锚定的文件名（`chat.rs`）—— 精确相等，最可靠的一档
        if name == probe {
            return true;
        }
        // 互相包含：一侧是绝对路径、另一侧是仓库相对路径，是最常见的形状。
        // 两侧都要 ≥ 8 个字符 —— 短串的包含关系几乎必然是巧合。
        probe_len >= 8 && file_len >= 8 && (file.contains(probe) || probe.contains(&file))
    })
}

/// 从正文里抽出来的路径，按条目 id 索引（给 `files` 为空的存量条目用）。
///
/// ── 为什么不能只改导入侧 ────────────────────────────────────
/// 导入侧已经改成「写入时从正文抽路径填 `files`」，但那只对**新导入**生效。
/// 用户库里已有的两千多条导入流水，`files` 是空的，重新导入会被 hash 去重挡掉
/// —— 于是「按文件命中」这一档对存量数据永久失效，而存量数据正是绝大多数。
///
/// 又不能改写用户的 `.md`：`files` 在磁盘格式里是「记忆根相对路径」，往里塞
/// 仓库相对路径会污染语义（体检会按记忆根 stat 它们）。所以这份索引只活在
/// 内存里，纯派生物，随时可丢。
///
/// ── 代价与触发条件 ──────────────────────────────────────────
/// 建一次要把全部正文过一遍正则（本机 55 MB，约 0.3-0.5 秒），所以：
///   · 只在**请求里真的出现路径**时才建（`path_like` 判断），纯文字聊天不付这份钱；
///   · 结果按 `store::generation()` + 条目数缓存，写一次记忆才重建一次；
///   · 调用方是 `blocking::in_place`，重建期间占的是阻塞池线程，不是 tokio worker。
fn derived_paths() -> std::sync::Arc<std::collections::HashMap<String, Vec<String>>> {
    use std::collections::HashMap;
    static SLOT: OnceLock<Mutex<Option<(u64, usize, Arc<HashMap<String, Vec<String>>>)>>> =
        OnceLock::new();
    let slot = SLOT.get_or_init(|| Mutex::new(None));
    let generation = store::generation();
    let entries = store::snapshot();
    if let Ok(guard) = slot.lock() {
        if let Some((stamp, count, index)) = guard.as_ref() {
            if *stamp == generation && *count == entries.len() {
                return Arc::clone(index);
            }
        }
    }
    // 只给 `files` 为空的条目抽：已经有 files 的条目用自己那份更准
    let mut index: HashMap<String, Vec<String>> = HashMap::new();
    for entry in entries.iter() {
        if !entry.files.is_empty() {
            continue;
        }
        let paths = harvest_paths(&entry.body);
        if !paths.is_empty() {
            index.insert(entry.id.clone(), paths);
        }
    }
    let index = Arc::new(index);
    if let Ok(mut guard) = slot.lock() {
        *guard = Some((generation, entries.len(), Arc::clone(&index)));
    }
    index
}

/// 请求文本里是否出现了「像路径」的东西。
///
/// 这是建 [`derived_paths`] 的闸：没有路径的请求根本用不上那份索引，不值得为它
/// 扫一遍全库正文。
fn path_like(probes: &[String]) -> bool {
    probes.iter().any(|probe| {
        (probe.contains('/') || probe.contains('\\')) && probe.chars().count() >= 6
    })
}

// ── 中文线索 ─────────────────────────────────────────────────────────

/// 中文线索的最小长度：4 个字。
///
/// 与 [`head_hit`] 的 4 字符门槛同义：「记忆中枢」四个字是一个完整的词，
/// 比英文的 `api` 这种碎片可靠得多。
const CJK_MIN: usize = 4;

/// 中文线索算「强命中」的长度：8 个字。
///
/// 8 个字以上的**连续**片段被原样复述，几乎必然是同一件事（用户当年的原话，
/// 或者同一个结论被写了两遍）。这一档的权重对齐路径档。
const CJK_STRONG: usize = 8;

/// 中文线索最多折算几个单位。
///
/// 2 个单位 = 4.0 分 =「两条独立线索互相印证」，与 ASCII 的两条探针同档。
/// 再多就是灌分 —— 一条记忆只该被自己的证据推进，不能被十个子串推进。
const CJK_UNITS_MAX: f64 = 2.0;

/// 中文线索最多取请求里的几段。
///
/// 请求文本不只有用户的话：Chat 路径会在用户侧抽不出结构线索时补上工具回显
/// （`chat_request_text_from`），原生路径更是取整个 body 的尾部。那些回显里
/// 也有中文 —— 源码注释、日志、报错 —— 那是「正在处理的文件」，不是「用户想
/// 做的事」。拿它们当线索，命中的会是碰巧含同一句注释的旧记忆。
///
/// ── 16 这个数是量出来的，不是拍的 ────────────────────────────
/// 真机一次真实请求（WorkBuddy CLI，814 KB，`<user_query>` 里是 2.5 万字符的
/// 会话摘要，截到 12k 后 453 个中文段）拿去跑打分：
///
/// | 取段数 | 四字组 | 命中 | 过线 | 前两名 |
/// |---|---|---|---|---|
/// | 8 | 99 | 29 | 29 | 全是同一条 l1 流水的碎片 |
/// | 12 | 130 | 71 | 33 | 「记忆留存边界」「留存边界：通知是控制消息」 |
/// | 16 | 167 | 71 | 33 | 同上（已饱和） |
/// | 24 | 235 | 71 | 33 | 同上 |
/// | 32 | 299 | 73 | 35 | 同上 |
/// | 64 | 478 | 102 | 66 | 开始混进相邻主题 |
///
/// 8 段太少（召回不到 l2 结论），64 段太多（噪音翻倍）。12 段已饱和，取 16
/// 留一点余量，同时避免抓到工具回显。
const CJK_RUNS_MAX: usize = 16;

/// 请求里的中文线索。
///
/// ── 为什么用「四字组集合」而不是「短语列表」 ───────────────────
/// 中文没有词边界，用户的话也没有标点分词：「记忆中枢的检索为什么命中不了」
/// 到底该切成哪几段？切错了就永远命中不了。
///
/// 四字组（4-gram）绕开分词问题：只要**同一段连续文字**在两边都出现，它
/// 内部的四字组就一定重合。于是「这两边是不是同一件事」变成一个纯集合问题：
/// 记忆那一侧的 4-gram 序列里，有多少个**连续**落在请求的 4-gram 集合里。
///
/// 代价是集合里混得进「差一点也算」的碎片（四字组重合 ≠ 整段重合）。所以
/// 下面按**连续**命中计数：k 个连续命中的 4-gram 拼起来就是 k+3 个字的
/// 连续片段，中间隔一个就断。
#[derive(Debug, Clone, Default)]
pub struct CjkClues {
    grams: HashSet<u64>,
}

impl CjkClues {
    /// 这份请求里有没有中文线索。
    pub fn is_empty(&self) -> bool {
        self.grams.is_empty()
    }

    /// 抽到了多少个四字组（诊断用：界面上能看出「这次到底有没有中文线索」）。
    pub fn len(&self) -> usize {
        self.grams.len()
    }

    /// 指纹 —— 缓存键要用。
    ///
    /// 排序后再算：`HashSet` 的遍历顺序随进程种子变，同一份请求在不同进程里
    /// 必须算出同一个指纹，否则缓存命中率会随重启塌掉。
    pub fn fingerprint(&self) -> u64 {
        let mut grams: Vec<u64> = self.grams.iter().copied().collect();
        grams.sort_unstable();
        let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
        for gram in grams {
            hash ^= gram;
            hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
        }
        hash
    }

    /// 这段文字的四字组是不是**全都在**线索里（诊断与单测用）。
    ///
    /// 按中文连续段逐段判：标点两侧的字本来就不该算连续。
    ///
    /// 只在测试里用：它回答的是「这份线索能不能覆盖这句话」，产品代码走的是
    /// [`cjk_units`] 那种按连续命中计权的路。放在 `cfg(test)` 里，免得二进制
    /// 构建背上一条 dead_code 警告。
    #[cfg(test)]
    pub fn covers(&self, text: &str) -> bool {
        let runs = cjk_runs(text);
        if runs.is_empty() {
            return false;
        }
        runs.iter().all(|run| {
            let chars: Vec<char> = run.chars().collect();
            chars.len() >= CJK_MIN
                && chars
                    .windows(CJK_MIN)
                    .all(|window| self.grams.contains(&gram_hash(window)))
        })
    }
}

/// 从一段请求文本里抽中文线索。
///
/// ── 为什么先剥客户端包裹块 ────────────────────────────────────
/// `<system-reminder>`、`<task-notification>`、`<previous_user_message>` 这些
/// 里全是中文，但它们不是用户说的话 —— 是客户端的说明文字与上一轮的回灌。
/// 拿它们当线索，命中的会是「客户端在解释自己」那条记忆，与本轮任务无关。
///
/// 剥的名单与导入侧共用一份（[`super::import::strip_client_blocks`]）。
///
/// ── 为什么这里只做「抽」，不做「切分」 ────────────────────────
/// 切分是错的那一步：中文没有词边界，`记忆中枢的检索` 切成 `记忆中枢`+`的检索`
/// 还是 `记忆中`+枢的检索，全凭猜。4-gram 把这个问题推给集合运算，答案稳定。
pub fn cjk_clues(text: &str) -> CjkClues {
    let mut grams = HashSet::new();
    if text.trim().is_empty() {
        return CjkClues { grams };
    }
    let cleaned = super::import::strip_client_blocks(text);
    let mut runs = cjk_runs(&cleaned);
    // 按长度取前几段（见 [`CJK_RUNS_MAX`]）：用户的话通常最长，工具回显里的
    // 中文该被挡在外面。稳定排序 → 长度相同时保持原顺序，指纹才不会飘。
    runs.sort_by(|left, right| right.chars().count().cmp(&left.chars().count()));
    runs.truncate(CJK_RUNS_MAX);
    for run in runs {
        let chars: Vec<char> = run.chars().collect();
        for window in chars.windows(CJK_MIN) {
            grams.insert(gram_hash(window));
        }
    }
    CjkClues { grams }
}

/// 把文本切成一段段连续的中文（标点、ASCII、空白都是分隔符）。
fn cjk_runs(text: &str) -> Vec<String> {
    let mut runs: Vec<String> = Vec::new();
    let mut current = String::new();
    for ch in text.chars() {
        if super::model::is_cjk(ch) {
            current.push(ch);
        } else if !current.is_empty() {
            runs.push(std::mem::take(&mut current));
        }
    }
    if !current.is_empty() {
        runs.push(current);
    }
    runs
}

/// 四字组的哈希（FNV-1a，64 位）。
fn gram_hash(window: &[char]) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for ch in window {
        for byte in (*ch as u32).to_le_bytes() {
            hash ^= byte as u64;
            hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
        }
    }
    hash
}

/// 记忆这一侧命中的中文线索有几个「单位」。
///
/// 一个独立命中区域算 1.0 个单位；连续 [`CJK_STRONG`] 个字以上的区域算 1.5 个
/// 单位。上限 [`CJK_UNITS_MAX`]。
fn cjk_units(head: &str, cjk: Option<&CjkClues>) -> f64 {
    let Some(clues) = cjk else { return 0.0 };
    if clues.is_empty() {
        return 0.0;
    }
    let mut units = 0.0f64;
    for run in cjk_runs(head) {
        let chars: Vec<char> = run.chars().collect();
        // 沿这一段扫四字组：连续命中的 k 个 = k+3 个字的连续片段
        let mut streak = 0usize;
        let mut region = 0usize;
        for window in chars.windows(CJK_MIN) {
            if clues.grams.contains(&gram_hash(window)) {
                streak += 1;
                let length = streak + CJK_MIN - 1;
                if length > region {
                    region = length;
                }
            } else if streak > 0 {
                // 断了：这个区域结算，独立算一条线索
                units += region_weight(region);
                streak = 0;
                region = 0;
            }
        }
        if streak > 0 {
            units += region_weight(region);
        }
        if units >= CJK_UNITS_MAX {
            return CJK_UNITS_MAX;
        }
    }
    units.min(CJK_UNITS_MAX)
}

/// 一个命中区域的权重：连续 [`CJK_STRONG`] 个字以上算强线索（1.5），否则 1.0。
fn region_weight(length: usize) -> f64 {
    if length >= CJK_STRONG {
        1.5
    } else {
        1.0
    }
}

/// 标题 / 摘要 / 标签里有没有这条线索。
///
/// 线索短于 4 个字符时**不参与**这一档：`api`、`md`、`v1` 这类碎片在几千条
/// 记忆里几乎必然出现在某条的标题上，命中一次就是 2.0 分（正好越过门槛），
/// 结果是一条不相干的记忆被塞进注入块。路径档不受这个限制（路径本身就长）。
fn head_hit(head: &str, probe: &str) -> bool {
    probe.chars().count() >= 4 && head.contains(probe)
}

/// 一条记忆与本次请求的相关度。0 = 无关。
///
/// ── 为什么只匹配 title / summary / tags / files，不匹配整段 body ──
/// body 动辄几百字，几千条 × 24 个线索逐条 `contains` 是几十毫秒起步，而注入
/// 在**每一条请求**的热路径上。`summary(240)` 是「这条记的是什么、结论是什么」
/// 的一句话（有模型摘要就用它，没有就取正文第一段），拿它做线索匹配，精度比
/// 整段正文更高 —— 正文里那些凑巧出现的词反而是噪音。
///
/// `extra` 是从正文抽出来的路径（见 [`derived_paths`]），只对 `files` 为空的
/// 存量条目有值 —— 否则那批数据在「按文件命中」这一档上永久隐形。
///
/// `cjk` 是请求里的中文线索（见 [`cjk_clues`]）。它与 `probes` 是**两套**机制：
/// `probes` 认「整串出现」（路径、标识符），中文没有词边界，切出来的短语
/// 几乎不可能整串出现在标题里 —— 所以中文走 4-gram 的连续命中，走另一套权重。
fn relevance(
    entry: &MemoryEntry,
    probes: &[String],
    path_match: bool,
    extra: Option<&Vec<String>>,
    cjk: Option<&CjkClues>,
) -> f64 {
    // 没有结构线索、也没有中文线索 → 这一次请求跟记忆库没关系，整库扫过去也是白扫
    if probes.is_empty() && cjk.map_or(true, |clues| clues.is_empty()) {
        return 0.0;
    }
    // ── 为什么用 `summary(240)` 而不是 `summary_text` ────────────
    // `summary_text` 是模型写过的那一句，绝大多数条目上是**空的**（摘要任务默认
    // 关着）。只拿它做匹配，等于对九成条目只剩标题可比 —— 而标题往往很短
    // （「注入点」），几乎不含能定位的字符串。
    //
    // `summary(240)` 在 `summary_text` 为空时会回退到正文第一段非空文本，那正是
    // 「这条记了什么」的原话。用它做匹配，命中率与精度都明显更高。
    let head = format!(
        "{} {} {}",
        entry.title,
        entry.summary(240),
        entry.tags.join(" ")
    )
    .to_lowercase();
    let mut score = 0.0f64;
    for probe in probes {
        // 路径命中权重最高：`files` 是写入时用户亲手锚定的，几乎不会误报
        if path_match
            && (file_hit(entry.files.iter().map(String::as_str), probe)
                || extra
                    .map(|paths| file_hit(paths.iter().map(String::as_str), probe))
                    .unwrap_or(false))
        {
            score += 3.0;
            continue;
        }
        if head_hit(&head, probe) {
            score += 2.0;
        }
    }
    // ── 中文线索 ────────────────────────────────────────────────
    // 一个单位 = 一条独立线索 = 2.0 分，与 ASCII 的标题命中同档；
    // 连续 8 字以上算 1.5 个单位 = 3.0 分，与路径命中同档。
    //
    // 档位刻意对齐 ASCII 那两档：门槛（[`floor_for`]）是按「一个泛词命中进不来、
    // 一条路径或两条独立线索才能进」定的，中文走同一套刻度，精度口径不变。
    score += cjk_units(&head, cjk) * 2.0;
    if score <= 0.0 {
        return 0.0;
    }
    // 类型加成：踩坑与决策是「下次做同样的事会踩到」的那一类，比日常流水值钱。
    //
    // ── 自动抽取的候选不享受这一档 ─────────────────────────────
    // 类型是模型自己填的：它把一条流水写成 `pitfall`，这里就白送 1.5 分，于是
    // 「一个标题命中 2.0 + 踩坑 1.5 + importance 0.6 = 4.1」直接越过 [`floor_for`]
    // 给候选定的 3.4 —— 那一档的意图是「必须路径命中或两条独立线索」，实际被
    // 类型加成淹没了（真机复现：单标题命中即可放行）。类型加成的前提是「有人
    // 认定这是一条踩坑」，而 auto-extract 的产物还没有人认定过；等它被 reinforce
    // 升到 l2 之后，这条限制自然失效。
    if entry.agent.as_deref() != Some(AUTO_EXTRACT_AGENT) {
        score += match entry.kind {
            MemoryKind::Pitfall => 1.5,
            MemoryKind::Decision => 1.0,
            MemoryKind::Preference | MemoryKind::Profile => 1.0,
            MemoryKind::Knowledge | MemoryKind::Insight => 0.5,
            _ => 0.0,
        };
    }
    score += entry.importance;
    if entry.pinned {
        score += 0.5;
    }
    score
}

/// 这条记忆进候选池的最低分。`None` = 根本不进池。
///
/// ── 为什么是「按层给不同门槛」而不是「只认 l2」───────────────
/// 只认 l2 曾经是对的：几千条流水里翻出来的「相关」几乎必然是巧合，而它们会把
/// 预算占满、把真正该看的那两条挤掉（`search::core_text` 早就有同一条取舍）。
///
/// 但本机真实数据把这个取舍推翻了：库里绝大多数条目是**导入进来的 l1 会话流水**，
/// 而 l2 要靠蒸馏任务产出（默认关着，开了也要先攒够素材）。只认 l2 的结果是
/// 用户导了两千多条、注入一条都命中不了 —— 而「以前做过这件事、踩过这个坑」
/// 恰恰就写在那批 l1 里。那不是精度高，那是没在用。
///
/// 所以改成**分层门槛**：l2 与「天然就是结论」的类别 2.0，普通 l1 流水 3.3。
/// 3.3 这个数不是拍的，它对应两条真实路径：
///
/// | 命中形态 | 得分（普通 l1，importance 0.4） | 结果 |
/// |---|---|---|
/// | 单个标题/摘要线索 | 2.0 + 0.4 = 2.4 | 不进（噪音） |
/// | 一条路径命中 | 3.0 + 0.4 = 3.4 | 进（路径几乎不误报） |
/// | 两条独立线索 | 4.0 + 0.4 = 4.4 | 进（同一件事被说了两遍） |
///
/// 也就是说 l1 流水要靠「路径」或「多个线索互相印证」才能挤进来，一个泛词命中
/// 进不来 —— 召回上去了，精度还在。
fn floor_for(entry: &MemoryEntry) -> Option<f64> {
    if !entry.is_live() {
        return None;
    }
    // 沉淀层本身就是结论，标题/摘要里出现线索就够
    if entry.layer == Layer::L2 {
        return Some(2.0);
    }
    // 自动抽取产出、还没被反复确认的 l1：它是一次模型调用的产物，不是人写下
    // 的结论。按下面「l1 里的要求/踩坑/决策天然是结论」那条 2.0 放行，结果是
    // 同一件事被抽出几份不同措辞，同时挤进注入预算（真机取证：132 条 l2 里
    // 五组同义结论各占 3-6 条）。这里要求「路径命中」或「两条独立线索」
    // （3.4 档）；被 reinforce 到升 l2 之后自然回到上面那条 2.0。
    //
    // 光给门槛不够：候选的类型是模型自己填的，`pitfall` 会白拿 1.5 分类型加成，
    // 2.0 + 1.5 + importance 就轻松越过 3.4。所以 [`relevance`] 里对候选关掉了
    // 类型加成 —— 两处要一起看，改一处等于没改。
    if entry.agent.as_deref() == Some(AUTO_EXTRACT_AGENT) {
        return Some(3.4);
    }
    // l1 里「要求 / 踩坑 / 决策 / 画像」这几类天然是「下次要用」的；加星与高
    // importance 是有人主动抬过 —— 两种都按结论对待
    if entry.starred
        || entry.importance >= 0.8
        || matches!(
            entry.kind,
            MemoryKind::Pitfall
                | MemoryKind::Preference
                | MemoryKind::Profile
                | MemoryKind::Decision
        )
    {
        return Some(2.0);
    }
    // 普通 l1 流水（含导入的会话记录）：高置信才进
    Some(3.3)
}

/// 标题是不是「导入流水留下的裸标识符」。
///
/// ── 真机数据长这样 ──────────────────────────────────────────
/// ```text
/// 538e6914-d5c6-486e-8635-a90ef475b1c9（866）
/// ```
///
/// 那是导入器拿「会话 id + 序号」拼出来的占位标题，同一个会话有 2200 多条这种
/// 条目。把它渲染成 `- [daily] 538e6914-…（866） —— 结论` 只会白烧 token：
/// 模型读那一长串十六进制得不到任何信息，真正有用的只有破折号后面的结论。
///
/// 判据要求「几乎全是十六进制与连字符、可选的结尾序号」—— 正常的标题（「注入点」、
/// 「beta.1 出包」）里不可能有中文和空格之外的这种形状，误判风险接近零。
fn junk_title(title: &str) -> bool {
    let trimmed = title.trim();
    if trimmed.is_empty() {
        return true;
    }
    // 去掉结尾的中文/英文括号序号：`（866）` / `(866)`
    let base = trimmed
        .rsplit_once('（')
        .map(|(head, _)| head)
        .or_else(|| trimmed.rsplit_once('(').map(|(head, _)| head))
        .unwrap_or(trimmed)
        .trim();
    if base.chars().count() < 8 {
        return false;
    }
    base.chars()
        .all(|ch| ch.is_ascii_hexdigit() || ch == '-')
}

/// 注入面去重的分组键。
///
/// ── 为什么必须去重 ──────────────────────────────────────────
/// 真机跑分得到 14 条过线候选，**全部**来自同一个导入会话的碎片（同一个
/// `538e6914-…` 前缀、只是序号不同）。不去重的话 6 个名额会被同一件事塞满，
/// 而这件事本身还只是一段流水，信息量远低于另一件完全不同的事。
///
/// 分组键取「去掉序号后的标题」—— 那正好是「同一次会话 / 同一件事」的天然边界。
/// 标题正常的条目不按标题分组（不同标题本来就是不同的事），改按摘要开头分组，
/// 那能挡住「同一条结论被蒸馏了两遍」。
fn group_key(entry: &MemoryEntry) -> String {
    if junk_title(&entry.title) {
        let base = entry
            .title
            .trim()
            .rsplit_once('（')
            .map(|(head, _)| head)
            .or_else(|| entry.title.trim().rsplit_once('(').map(|(head, _)| head))
            .unwrap_or(entry.title.trim());
        return format!("junk:{}", base.to_lowercase());
    }
    let head: String = entry
        .summary(60)
        .chars()
        .filter(|ch| !ch.is_whitespace())
        .collect();
    format!("text:{}", head.to_lowercase())
}

fn rule_candidate(entry: &MemoryEntry) -> bool {
    entry.is_live()
        && (entry.pinned
            || matches!(entry.kind, MemoryKind::Preference | MemoryKind::Profile))
}

/// 渲染一行。
///
/// 标题是裸标识符时只渲染摘要 —— 那一长串十六进制对模型是纯噪音（见
/// [`junk_title`]）。
fn line(entry: &MemoryEntry, width: usize) -> String {
    let summary = entry.summary(width);
    if junk_title(&entry.title) {
        if !summary.trim().is_empty() {
            return format!("- [{}] {}", entry.kind.label(), summary);
        }
        // 标题没用、摘要也空：这条拿不出任何可读内容，标题就是全部
        return format!(
            "- [{}] {}",
            entry.kind.label(),
            truncate_chars(&entry.title, width)
        );
    }
    if summary.trim().is_empty() {
        format!("- [{}] {}", entry.kind.label(), truncate_chars(&entry.title, width))
    } else {
        format!(
            "- [{}] {} —— {}",
            entry.kind.label(),
            truncate_chars(&entry.title, width),
            summary
        )
    }
}

/// 这个 User-Agent 是不是本进程自己的内部调用。
///
/// ── 为什么必须排除它们 ──────────────────────────────────────
/// 记忆中枢自己的 `distill` / `profile` / `summarize` 是**打自己的网关**跑的
/// （见 `llm::complete`，UA 固定 `Agent2API-MemoryBrain/1.0`；视觉分析是
/// `Agent2API-Vision`）。如果不排除，蒸馏那一次调用会被注入一段记忆块：
///
///   · 白烧 token —— 蒸馏的输入本来就长，再加 600 token 是纯浪费；
///   · 更糟的是**污染**：让模型「参考既有结论」去蒸馏新结论，等于让旧记忆
///     给自己背书，蒸馏会越来越自我重复，新信息进不来。
///
/// 判据用 UA 而不是「请求里有没有 x-agent2api-source 头」：UA 在客户端那一侧
/// 无法伪造出一个「看起来像内部调用」的值来骗取跳过（跳过注入对调用方是减配，
/// 不是特权，所以这里也不担心被利用）。
pub fn is_internal_origin(user_agent: &str) -> bool {
    // 本仓自己发出的 UA 都以 `Agent2API-` 开头：`Agent2API-MemoryBrain/1.0`
    // （记忆蒸馏）、`Agent2API-VisionTool/1.0`（视觉分析）、
    // `Agent2API-VisionCompatibility/1.0`（图片兼容层）。按前缀判一次即可 ——
    // 逐个列名会在下次新增内部工具时漏掉一个，而漏掉的后果是「那个工具莫名
    // 变贵、而且结论被旧记忆污染」，很难从现象反推到原因。
    user_agent.to_ascii_lowercase().starts_with("agent2api-")
}

/// 注入次数、尝试次数与累计 token。
///
/// ── 为什么需要这三个计数器 ──────────────────────────────────
/// 「注入到底有没有发生」是这套机制唯一说不清的地方：用户看到的是模型行为，
/// 而模型行为受太多因素影响。计数是唯一客观的证据 —— 显示「本次启动以来注入
/// 137 次 / 平均 420 token」，用户就能判断「它真的在跑」，而不是又去怀疑配置。
///
/// ── 为什么还要记「尝试次数」 ────────────────────────────────
/// 只有一个 `count` 时，「0 次注入」有两种完全不同的含义：
///
///   · 开关关着 / 一条都没命中 → 机制正常，只是没东西可注；
///   · 注入压根没被调用 → 代码没接上 / 请求走了别的入口。
///
/// 两种在界面上长得一模一样，而处理方式完全相反。所以再记一个 `attempts`：
/// `attempts > 0 && count == 0` 才是「没命中」，`attempts == 0` 才是「没跑」。
///
/// 用原子计数而不是往 `recall_log` 写一行：注入挂在每一条转发请求前面，
/// 一次 SQLite 写入（还要抢索引连接锁）在这个位置太贵。计数够用，细节看
/// 注入预览。
static INJECT_COUNT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
static INJECT_TOKENS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
static INJECT_ATTEMPTS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

fn note_attempt() {
    INJECT_ATTEMPTS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
}

fn note_injected(tokens: usize) {
    use std::sync::atomic::Ordering;
    INJECT_COUNT.fetch_add(1, Ordering::Relaxed);
    INJECT_TOKENS.fetch_add(tokens as u64, Ordering::Relaxed);
}

/// 本次启动以来的注入统计（仪表盘用）。
pub fn stats() -> Value {
    use std::sync::atomic::Ordering;
    let count = INJECT_COUNT.load(Ordering::Relaxed);
    let tokens = INJECT_TOKENS.load(Ordering::Relaxed);
    let attempts = INJECT_ATTEMPTS.load(Ordering::Relaxed);
    serde_json::json!({
        "count": count,
        "tokens": tokens,
        // 平均每次注入多少 token —— 判断「预算是不是被吃满」最直接的一个数
        "avgTokens": if count == 0 { 0 } else { tokens / count },
        "enabled": settings().enabled,
        // 走过注入逻辑的请求数。`attempts - count` = 没命中的次数
        "attempts": attempts,
        "missed": attempts.saturating_sub(count),
    })
}

/// 给一段请求文本产出注入块。没有任何相关内容时返回 `None`。
///
/// 纯函数（除了读快照），可以直接单测。
pub fn compose(request_text: &str) -> Option<String> {
    let settings = settings();
    if !settings.enabled || settings.max_tokens == 0 {
        return None;
    }
    // ── 空请求文本不等于不注入 ──────────────────────────────────
    // 第一版在这里直接 `return None`，理由是「没文本就没线索」。但「必须遵守」
    // 那一段（pinned / preference / profile）**不依赖线索** —— 用户钉的规矩对
    // 每一条请求都成立。真机后果：凡是取不到任务文本的请求（客户端只发 system、
    // 或 `<user_query>` 是空的），连「版本号保持 3.5.0-beta.1」这种硬规则都进不去。
    //
    // 所以空文本照常往下走：线索为空 → 「相关经验」自然空，但「必须遵守」还在。
    let text = request_text.trim();
    let probes = if text.is_empty() { Vec::new() } else { probes(text) };
    // 中文线索与结构线索是**两套**独立机制：结构线索认「整串出现」，中文没有
    // 词边界，只能按 4-gram 的连续命中算。
    let cjk = if text.is_empty() {
        None
    } else {
        Some(cjk_clues(text))
    };
    let key = cache_key(&settings, &probes, cjk.as_ref());
    if let Some(hit) = cache_get(key) {
        return hit;
    }
    let block = build(&settings, &probes, cjk.as_ref());
    cache_put(key, block.as_deref());
    block
}

/// 注入结果缓存。
///
/// ── 为什么热路径上必须有一层缓存 ────────────────────────────
/// `compose` 要对着**全部**记忆条目逐条跑 `contains`（本机实测 5800 条 × 最多
/// 24 个线索 ≈ 14 万次子串比较），还要走一遍目录指纹。单次十几毫秒，而它挂在
/// **每一条转发请求**前面 —— 聊天时用户感知不到，但并发一上来这条链路就成了
/// 串行瓶颈（`spawn_blocking` 线程池被记忆匹配占满，转发反而排队）。
///
/// 缓存的键不是「整段请求文本」而是**抽出来的线索集合**：同一件事换个说法
/// （「改一下 chat.rs」/「chat.rs 那个注入」）抽出来的线索一样，能直接命中。
/// 这正是我们想要的粒度 —— 注入结果本来就只由线索决定。
///
/// TTL 60 秒：用户钉一条记忆、写一条避坑之后，最多一分钟就能在下一轮对话里
/// 看到效果。比这更长的 TTL 会让「我刚写的规则模型怎么不用」变成一个反复
/// 出现的问题；比这更短就等于没缓存。
const CACHE_TTL: Duration = Duration::from_secs(60);
/// 最多记这么多条。一轮对话里线索组合也就十几种，64 足够且不会无限涨。
const CACHE_MAX: usize = 64;

fn cache() -> &'static Mutex<Vec<(u64, Instant, Option<Arc<String>>)>> {
    static CACHE: OnceLock<Mutex<Vec<(u64, Instant, Option<Arc<String>>)>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(Vec::new()))
}

fn cache_key(settings: &Settings, probes: &[String], cjk: Option<&CjkClues>) -> u64 {
    // FNV-1a：只需要「撞了也无所谓」的散列，撞了最多是拿错一次注入结果 ——
    // 而键里带着全部线索，实际碰撞概率可以忽略。
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    let mut feed = |bytes: &[u8]| {
        for byte in bytes {
            hash ^= *byte as u64;
            hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
        }
    };
    feed(&settings.max_tokens.to_le_bytes());
    feed(&settings.rules_tokens.to_le_bytes());
    feed(&settings.experience_tokens.to_le_bytes());
    feed(&settings.max_items.to_le_bytes());
    feed(&[settings.path_match as u8]);
    // 记忆被写过（`store::invalidate` 代际 +1）就换一个键：注入结果是快照的
    // 派生值，只清快照不清这份缓存，用户钉的规则要多等一个 TTL 才生效。
    feed(&store::generation().to_le_bytes());
    for probe in probes {
        feed(probe.as_bytes());
        feed(b"\x1f");
    }
    // ── 中文线索也要进键 ────────────────────────────────────────
    // 两条请求的 ASCII 线索可能一模一样（都只提到一个 `inject.rs`），但中文
    // 不同 —— 不进键就会拿上一条请求的注入块来用，中文那部分整个串台。
    if let Some(clues) = cjk {
        feed(&clues.fingerprint().to_le_bytes());
    }
    hash
}

fn cache_get(key: u64) -> Option<Option<String>> {
    let mut guard = cache().lock().ok()?;
    let now = Instant::now();
    guard.retain(|(_, stamp, _)| now.duration_since(*stamp) < CACHE_TTL);
    guard
        .iter()
        .find(|(stored, _, _)| *stored == key)
        .map(|(_, _, value)| value.as_deref().map(|text| text.to_string()))
}

fn cache_put(key: u64, value: Option<&str>) {
    let Ok(mut guard) = cache().lock() else { return };
    if guard.len() >= CACHE_MAX {
        // 简单淘汰：丢掉最早的那条。不用 LRU —— 缓存的是「同一批线索」，
        // 命中率对淘汰策略不敏感。
        guard.remove(0);
    }
    guard.push((key, Instant::now(), value.map(|text| Arc::new(text.to_string()))));
}

fn build(settings: &Settings, probes: &[String], cjk: Option<&CjkClues>) -> Option<String> {
    let entries = store::snapshot();

    // ── 第一段：必须遵守 ────────────────────────────────────────
    let mut rules: Vec<&MemoryEntry> = entries.iter().filter(|entry| rule_candidate(entry)).collect();
    rules.sort_by(|left, right| {
        right
            .pinned
            .cmp(&left.pinned)
            .then(
                right
                    .importance
                    .partial_cmp(&left.importance)
                    .unwrap_or(std::cmp::Ordering::Equal),
            )
            .then(right.updated_at.cmp(&left.updated_at))
    });

    let mut body = String::new();
    let mut used = estimate_tokens(&header());
    let mut rule_ids: Vec<&str> = Vec::new();
    let mut rules_text = String::new();
    let mut rules_used = 0usize;
    for entry in &rules {
        let rendered = line(entry, 56);
        let cost = estimate_tokens(&rendered) + 1;
        if rules_used + cost > settings.rules_tokens {
            break;
        }
        rules_text.push_str(&rendered);
        rules_text.push('\n');
        rules_used += cost;
        rule_ids.push(entry.id.as_str());
    }
    if !rules_text.is_empty() {
        body.push_str("## 必须遵守\n");
        body.push_str(&rules_text);
        used += rules_used + 2;
    }

    // ── 第二段：相关经验 ────────────────────────────────────────
    //
    // 门槛按条目分层（`floor_for`）：l2 与「天然是结论」的类别 2.0，普通 l1 流水
    // 3.3。不设统一门槛的话，两种坏结果必居其一：门槛 2.0 会让导入的流水把预算
    // 占满，门槛 3.3 会让沉淀结论全部落选。
    //
    // `extra` 只在这条请求里真的出现路径时才建（`path_like`）：它是全库正文扫
    // 一遍的派生索引，纯文字聊天不该付这份钱。
    let derived = if settings.path_match && path_like(probes) {
        Some(derived_paths())
    } else {
        None
    };
    let mut scored: Vec<(f64, &MemoryEntry)> = entries
        .iter()
        .filter(|entry| !rule_ids.contains(&entry.id.as_str()))
        .filter_map(|entry| {
            let floor = floor_for(entry)?;
            let extra = derived.as_ref().and_then(|index| index.get(&entry.id));
            let score = relevance(entry, probes, settings.path_match, extra, cjk);
            (score >= floor).then_some((score, entry))
        })
        .collect();
    scored.sort_by(|left, right| {
        right
            .0
            .partial_cmp(&left.0)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then(right.1.updated_at.cmp(&left.1.updated_at))
    });

    let mut experience_text = String::new();
    let mut experience_used = 0usize;
    let mut picked = 0usize;
    // 已经渲染过的正文（去重第二道闸）：同一条结论可能以两条 l1 碎片存在，
    // 分组键挡不住（两条的摘要尾部措辞略有差异），但渲染出来的行完全一样。
    let mut seen_lines: Vec<String> = Vec::new();
    // 去重第一道闸：同一件事只让**得分最高**的那条进来。
    let mut seen_groups: Vec<String> = Vec::new();
    for (_, entry) in &scored {
        // `max_items` 在**选中之后**才计数，不在排序之后先截断：先截断的话，
        // 前 6 条若都是长摘要，预算一满就直接 break，后面那些短而准的条目
        // 连被看一眼的机会都没有 —— 结果是一条经验都注不进去。
        if picked >= settings.max_items {
            break;
        }
        // ── 注入面去重 ──────────────────────────────────────────
        // 真机跑分得到 14 条过线，全部是同一个导入会话的碎片（同前缀、只差序号）。
        // `scored` 已按得分降序，所以「每组只取第一条」等于「每组取最好的那条」，
        // 剩下的名额留给别的组 —— 用户需要的是「三件不同的事」，不是「一件事的
        // 六种流水写法」。
        let group = group_key(entry);
        if seen_groups.iter().any(|known| known == &group) {
            continue;
        }
        let rendered = line(entry, 56);
        if seen_lines.iter().any(|known| known == &rendered) {
            continue;
        }
        let cost = estimate_tokens(&rendered) + 1;
        if experience_used + cost > settings.experience_tokens {
            continue;
        }
        if used + experience_used + cost > settings.max_tokens {
            break;
        }
        experience_text.push_str(&rendered);
        experience_text.push('\n');
        experience_used += cost;
        picked += 1;
        seen_groups.push(group);
        seen_lines.push(rendered);
    }
    if !experience_text.is_empty() {
        body.push_str("\n## 相关经验\n");
        body.push_str(&experience_text);
    }

    if body.trim().is_empty() {
        return None;
    }
    Some(format!("{MARKER}\n{}\n{body}{MARKER_END}", header().trim_end()))
}

/// 注入块的开头说明。
///
/// 最后那句「磁盘代码才是唯一现实」是硬约束，不能省：没有它，模型会拿三周前的
/// 记忆去「修复」刚改对的代码 —— 那比没有记忆糟得多（`agents::INSTRUCTION_BLOCK`
/// 里同一条，两处必须一致）。
fn header() -> String {
    "以下是本机记忆中枢里与本次任务相关的既有结论。\n\
     它们是历史陈述，不代表当前代码状态；与本次对话冲突时以本次为准，改文件前先读文件。\n\n"
        .to_string()
}

// ── 注入到请求体 ─────────────────────────────────────────────

/// 这个 body 里**本进程**已经注入过了吗。
///
/// ── 为什么不能扫全部消息找 MARKER ────────────────────────────
/// 第一版是 `messages.iter().any(|m| m.content.to_string().contains(MARKER))`。
/// 那在真机上等于永久关闭注入，而且症状极难反推：
///
///   · 用户会话里**有** `<agent2api-memory>` 字面量 —— 历史摘要里写了它、
///     模型把注入块贴回聊天里、或者工具读了一遍 `inject.rs` 把它回显出来；
///   · 于是从那一刻起，这条会话的每一个请求都被判成「已注入」直接 `return None`；
///   · 计数器上表现为 `attempts` 一路涨、`count` 永远 0、`missed` 一动不动地
///     累加 —— 看上去像「没命中」，实际是「根本没跑」。
///
/// 真机上就是这么炸的：`attempts = 109`、`count = 0`，而请求体里 MARKER 出现 9
/// 次，7 次在历史摘要里、2 次在 tool 回显里。
///
/// 正确的判据只有一个：**这条消息是不是我们插进去的那条**。注入块的形状是
/// 确定的（`system` 角色 + 内容以 [`MARKER`] 开头），任何带 MARKER 的 user /
/// tool 文本都不是我们插的。所以这里只认「system / developer 角色 + 开头就是
/// MARKER」，其余一律当作没注入过。
fn already_chat(body: &Value) -> bool {
    body.get("messages")
        .and_then(Value::as_array)
        .map(|messages| {
            messages.iter().any(|message| {
                if !matches!(
                    message.get("role").and_then(Value::as_str),
                    Some("system" | "developer")
                ) {
                    return false;
                }
                let text = match message.get("content") {
                    Some(Value::String(text)) => text.clone(),
                    Some(content) => crate::server::core::protocol::content_text(content),
                    None => return false,
                };
                text.trim_start().starts_with(MARKER)
            })
        })
        .unwrap_or(false)
}

/// 原生体（Responses 的 `instructions` / Anthropic 的 `system`）里本进程注入过了吗。
///
/// ── 为什么不能 `to_string().contains(MARKER)` ────────────────
/// 与 [`already_chat`] 是同一个病，只是更容易中招：这两个字段装的是**整个系统
/// 提示词**，而 Claude Code / Codex 的系统提示词里常年带着项目说明、CLAUDE.md
/// 全文、甚至一段讨论本仓源码的内容 —— 只要其中出现过 `<agent2api-memory>`
/// 这个字面量（本仓的 `inject.rs` 自己就写着它），这条路从此永久跳过注入，
/// 而计数器上表现为「attempts 涨、count 恒 0」，看上去像没命中。
///
/// ── 判据为什么是「末尾」而不是「开头」 ──────────────────────
/// 与 chat 侧不同：原生体不做插入，[`append_native`] 一律把记忆块**追加到
/// 字段末尾**。所以我们插进去的那一块一定是最后一个内容块 / 字符串尾部，
/// 且它的结尾恒为 [`MARKER_END`]。判「末尾是否以 MARKER_END 收束」既精确
/// （不看正文里出现过什么）又与写入方式同构（写在哪就认在哪）。
fn already_native(
    body: &Value,
    key: &str,
    protocol: crate::server::core::protocol::native::WireProtocol,
) -> bool {
    use crate::server::core::protocol::native::WireProtocol;
    let ends_with_block = |text: &str| text.trim_end().ends_with(MARKER_END);
    match body.get(key) {
        // Anthropic 的 `system` 可以是内容块数组：只看**最后一块**的 `text`。
        // 不能 `filter_map` 回溯前一块 —— 那样「最后一块没有 text」时会拿上一块
        // 来判，而那正是「没注入」的形状，会被读成「注入过」。
        Some(Value::Array(parts)) if protocol == WireProtocol::Anthropic => parts
            .last()
            .and_then(|part| part.get("text").and_then(Value::as_str))
            .map(ends_with_block)
            .unwrap_or(false),
        Some(Value::String(current)) => ends_with_block(current),
        _ => false,
    }
}

/// 尾部窗口：最后一条 user 消息里「真正的这一句」通常压在末尾。
///
/// WorkBuddy / CodeBuddy 会把整段会话（含上一轮摘要，本机实测 43,658 字符）压进
/// 同一条 user 消息，真正的输入只有最后百来个字符。取尾部窗口比取头部窗口命中
/// 率高得多：任务的落点是「继续做 X」，而历史摘要的开头永远是上一件事。
const TAIL_WINDOW: usize = 4_000;
/// 工具回显只取开头这么多个字符。
///
/// 开头是 `Command: cd ... && ...`，路径与命令名都在那里 —— 那正是最可靠的线索。
/// 再往后就是文件正文（一次 `Read` 能回显 5 万字符），抽出来全是代码里的标识符，
/// 24 个线索名额会被它们占满，把真任务的关键词挤出去。
const TOOL_WINDOW: usize = 600;
/// 整段取词文本的上限。probes 只认前 24 个，这里给足余量即可。
const REQUEST_TEXT_CAP: usize = 12_000;

/// 取出文本里所有 `<user_query>…</user_query>` 的内容。
///
/// ── 为什么优先认这个标签 ────────────────────────────────────
/// 它是**客户端自己写的**，不是模型编的，所以按它取词比按长度猜可靠。本机实测
/// WorkBuddy 的请求里：一条 user 消息 45,932 字符，其中 43,658 是上一轮摘要、
/// 2,094 是 system-reminder，真正的任务只有 `<user_query>` 里的 180 字符。
/// 不按标签取，就等于拿上一件事的关键词去检索这一件事的经验。
///
/// ── 为什么必须在**单个内容块**内配对 ────────────────────────
/// 这是第一版的真 bug。同一条 user 消息有三个内容块，历史摘要那个块里出现了
/// 3 次 `<user_query>`（正文在讨论这个标签），而**一个闭合标签都没有**；
/// 真正的任务在第三个块里，配对完整。把三块拼成一串再抽，第一个 `<user_query>`
/// 就会一路吃到第三个块的 `</user_query>` —— 抽出来的是 4 万多字符的整段历史
/// 摘要，真任务反而被包在里面当成正文。所以这里一定逐块调用。
fn user_queries(text: &str) -> Vec<String> {
    const OPEN: &str = "<user_query>";
    const CLOSE: &str = "</user_query>";
    let mut out: Vec<String> = Vec::new();
    let mut rest = text;
    while let Some(start) = rest.find(OPEN) {
        let body = &rest[start + OPEN.len()..];
        let Some(end) = body.find(CLOSE) else { break };
        let value = body[..end].trim();
        if !value.is_empty() && !out.iter().any(|known| known == value) {
            out.push(value.to_string());
        }
        rest = &body[end + CLOSE.len()..];
    }
    out
}

/// 最近一条**非空** user 消息的各个内容块（已过滤空块）。
///
/// 返回块列表而不是拼接后的整串：`user_queries` 需要逐块配对（见其说明），
/// 而「这块是不是空」比「整串是不是空」严格 —— 一条只有图片占位的 user 消息
/// 拼起来是非空的，逐块看才知道它没有文字。
fn last_user_blocks(messages: &[Value]) -> Vec<String> {
    messages
        .iter()
        .rev()
        .find_map(|message| {
            if message.get("role").and_then(Value::as_str) != Some("user") {
                return None;
            }
            let content = message.get("content").cloned().unwrap_or(Value::Null);
            let blocks: Vec<String> = match &content {
                Value::String(text) => vec![text.clone()],
                Value::Array(parts) => parts
                    .iter()
                    .filter_map(|part| {
                        if let Some(text) = part.as_str() {
                            return Some(text.to_string());
                        }
                        let kind = part
                            .get("type")
                            .and_then(Value::as_str)
                            .unwrap_or("")
                            .to_lowercase();
                        matches!(kind.as_str(), "text" | "input_text" | "output_text")
                            .then(|| part.get("text").and_then(Value::as_str).unwrap_or("").to_string())
                    })
                    .collect(),
                _ => Vec::new(),
            };
            let blocks: Vec<String> = blocks
                .into_iter()
                .filter(|text| !text.trim().is_empty())
                .collect();
            (!blocks.is_empty()).then_some(blocks)
        })
        .unwrap_or_default()
}

/// 最后 `count` 条 tool 消息的开头 `window` 个字符。
fn recent_tool_heads(messages: &[Value], count: usize, window: usize) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for message in messages.iter().rev() {
        if message.get("role").and_then(Value::as_str) != Some("tool") {
            continue;
        }
        let content = message.get("content").cloned().unwrap_or(Value::Null);
        let text = crate::server::core::protocol::content_text(&content);
        let head = truncate_chars(text.trim(), window);
        if !head.trim().is_empty() {
            out.push(head);
        }
        if out.len() >= count {
            break;
        }
    }
    out
}

/// 从 Chat 形态的请求体里取「要拿去做线索的文本」。
///
/// ── 取词顺序就是优先级 ──────────────────────────────────────
/// [`probes`] 的名额按桶分配，所以**谁先进来谁占名额**：
///
/// | 顺序 | 来源 | 为什么 |
/// |---|---|---|
/// | 1 | 最近一条 user 里各内容块的 `<user_query>` | 客户端标注的真实输入，最干净 |
/// | 2 | 最近一条 user 最后一个块的尾部窗口 | 没打标签的客户端（Claude Code 等）靠这一档 |
/// | 3 | 最近 2 条 tool 的开头 600 字符 | 命令行里的路径、报错文件位置 |
///
/// ── 第一版为什么一条都命中不了 ──────────────────────────────
/// 第一版取「最后 4 条 user / tool」，且按**倒序**往里拼，20,000 字符预算先被
/// 最近三条 tool 吃掉 —— 真机上那三条是 631 + 1,195 + 49,630 字符的文件回显和
/// 日志，拼到第二条就超预算 `break`，**真正含任务的 user 消息一个字都没进**。
/// 抽出来的线索全是 `.workbuddy/tmp/entries.json`、`row_factory`、`deleted_at`
/// 这类临时文件与脚本内部名，对着 5,997 条真实记忆跑分，`score >= floor` 的
/// 候选数是 0。
///
/// ── 为什么标签取不到时还要回退 ──────────────────────────────
/// `<user_query>` 里放的可能是一句「请继续」（本机实测就是这样，180 字符，
/// 抽不出任何路径或标识符）。只信这一档就等于「用户说请继续 → 零线索 → 不注入」，
/// 而上下文的真实任务恰恰就在最近那条 tool 回显的命令行里。所以标签档拿到的
/// 文本里**一个线索都没有**时，继续往下补 tool 开头 —— 有线索才停。
fn chat_request_text(body: &Value) -> String {
    let Some(messages) = body.get("messages").and_then(Value::as_array) else {
        return String::new();
    };
    chat_request_text_from(messages)
}

/// 同上，但直接吃消息数组（[`super::retain`] 拿到的是已经解析好的消息列表）。
///
/// ── 这一份是「给探针用的」还是「给留存用的」 ────────────────
/// 这里返回的是**含工具回显**的宽口径文本：注入抽线索时，工具回显里的路径
/// 恰恰是唯一可用的定位依据（见下方回退逻辑）。但那双鞋子留存不能穿 ——
/// 把 5 万字符的工具回显写进「用户说了什么」是错的。留存只要用户的话，
/// 那条需求走 [`user_turn_text`]。
pub fn chat_request_text_from(messages: &[Value]) -> String {
    let mut out = user_turn_text(messages);
    // 用户侧抽不出线索时（「请继续」这类），把最近工具回显的开头补进来：
    // 命令行里的路径与文件名是那一轮唯一可用的定位线索。
    if probes(&out).is_empty() {
        for head in recent_tool_heads(messages, 2, TOOL_WINDOW) {
            out.push_str(&head);
            out.push('\n');
            if out.chars().count() >= REQUEST_TEXT_CAP || !probes(&out).is_empty() {
                break;
            }
        }
    }
    truncate_chars(&out, REQUEST_TEXT_CAP)
}

/// 只取「用户这一轮真正说的话」：`<user_query>` 优先，否则最后一条 user
/// 内容块的尾部窗口。**不含工具回显**。
///
/// ── 为什么留存必须用这一份 ──────────────────────────────────
/// 留存写的是 `**我**：…`。用 [`chat_request_text_from`] 那份的话，当用户
/// 只说了「继续」时，写进记忆的「用户」行会变成一整屏命令行输出 —— 读起来
/// 像「用户发了一堆 shell 日志」，而且会把工具回显永久固化进记忆库。
pub fn user_turn_text(messages: &[Value]) -> String {
    let mut out = String::new();

    let blocks = last_user_blocks(messages);
    for block in &blocks {
        for query in user_queries(block) {
            out.push_str(&query);
            out.push('\n');
        }
    }
    if out.trim().is_empty() {
        // 没有标签（或标签里是空的）：退到最后一个内容块的尾部窗口。
        // 取尾部而不是头部 —— 任务的落点是结尾那一句，历史摘要的开头永远是上一件事。
        if let Some(last) = blocks.last() {
            let total = last.chars().count();
            if total > TAIL_WINDOW {
                out.extend(last.chars().skip(total - TAIL_WINDOW));
            } else {
                out.push_str(last);
            }
            out.push('\n');
        }
    }
    truncate_chars(&out, REQUEST_TEXT_CAP)
}

/// 把注入块插进 Chat 形态的 `messages`。
///
/// 位置选「开头连续 system 块之后」而不是最前面：客户端的 system 里常有
/// 「你是 Claude Code」这类身份声明，把记忆插到它前面会让那段声明变成对记忆的
/// 补充说明（模型读起来像「记忆里说我是 Claude Code」）。插在后面，记忆是
/// 附加信息，不是身份。
fn insert_chat(body: &mut Value, block: &str) -> bool {
    let Some(object) = body.as_object_mut() else {
        return false;
    };
    let Some(messages) = object.get_mut("messages").and_then(Value::as_array_mut) else {
        return false;
    };
    let at = messages
        .iter()
        .position(|message| {
            !matches!(
                message.get("role").and_then(Value::as_str),
                Some("system" | "developer")
            )
        })
        .unwrap_or(messages.len());
    messages.insert(
        at,
        serde_json::json!({ "role": "system", "content": block }),
    );
    true
}

/// 给 Chat 形态的请求体注入记忆。返回注入的 token 数（`None` = 没注入）。
///
/// ── 为什么返回 token 数而不是 bool ──────────────────────────
/// 「注入有没有发生」和「注入了多少」是两个问题。只回 bool 时，日志里那行
/// 「已附加记忆块」既不能证明它带了内容（可能是空块），也不能解释「这次请求
/// 怎么贵了 600 token」。把数字带回去，日志一行就说清了。
pub fn inject_chat(body: &mut Value) -> Option<usize> {
    note_attempt();
    if already_chat(body) {
        return None;
    }
    let text = chat_request_text(body);
    let block = compose(&text)?;
    if !insert_chat(body, &block) {
        return None;
    }
    let tokens = estimate_tokens(&block);
    note_injected(tokens);
    Some(tokens)
}

/// 给原生协议体注入记忆（Responses 的 `instructions`、Anthropic 的 `system`）。
///
/// ── 为什么原生体也要注 ──────────────────────────────────────
/// `/v1/responses` 与 `/v1/messages` 在上游协议相同时走**原生转发**，那条路
/// 发送的是 `native.routed(projection)` —— 它只从 Chat 投影里取 `model`，其余
/// 字段全部来自原生体。所以只在 `chat_body` 上注入，原生转发那一支收不到。
/// 两处都注，靠 [`MARKER`] 保证只生效一次。
pub fn inject_native(body: &mut Value, protocol: crate::server::core::protocol::native::WireProtocol) -> Option<usize> {
    use crate::server::core::protocol::native::WireProtocol;
    let key = match protocol {
        WireProtocol::Anthropic => "system",
        WireProtocol::Responses => "instructions",
        WireProtocol::Chat => return inject_chat(body),
    };
    note_attempt();
    if already_native(body, key, protocol) {
        return None;
    }
    // 原生体几 KB 到几百 KB，序列化一次是可接受的（上游请求本来就要序列化它）。
    // 线索只看后半段：`input` 数组里最新的消息在尾部。
    let serialized = body.to_string();
    let tail: String = {
        let total = serialized.chars().count();
        if total > 20_000 {
            serialized
                .chars()
                .skip(total.saturating_sub(20_000))
                .collect()
        } else {
            serialized
        }
    };
    let block = compose(&tail)?;
    if !append_native(body, protocol, key, &block) {
        return None;
    }
    let tokens = estimate_tokens(&block);
    note_injected(tokens);
    Some(tokens)
}

fn append_native(
    body: &mut Value,
    protocol: crate::server::core::protocol::native::WireProtocol,
    key: &str,
    block: &str,
) -> bool {
    use crate::server::core::protocol::native::WireProtocol;
    let Some(object) = body.as_object_mut() else {
        return false;
    };
    match object.get_mut(key) {
        // Anthropic 的 `system` 既可以是字符串，也可以是内容块数组
        Some(Value::Array(parts)) if protocol == WireProtocol::Anthropic => {
            parts.push(serde_json::json!({ "type": "text", "text": block }));
            true
        }
        Some(Value::String(current)) => {
            current.push_str("\n\n");
            current.push_str(block);
            true
        }
        Some(_) => false,
        None => {
            object.insert(key.to_string(), Value::String(block.to_string()));
            true
        }
    }
}

/// 排障用：给定一段请求文本，回一份「这次会注入什么」。
///
/// 用户抱怨过「改了配置看不出效果」。这个函数让界面能直接把注入结果摊开给他看，
/// 而不是让他去翻日志猜。
///
/// 传空文本时**不**返回空结果，而是只算「必须遵守」那一段（线索为空，所以
/// 相关经验一定没有）。这是常见用法：用户就想看看「我钉的那几条到底进没进去」。
///
/// ── 为什么要额外回一份 `hits` ──────────────────────────────
/// 只回拼好的 `text` 时，「没命中」和「命中了但被预算挤掉」在界面上长得一样。
/// `hits` 把每条候选的**得分与门槛**一起摊开：`score >= floor` 却不在 `text`
/// 里，就是被预算挤掉了（该调 `inject.experienceTokens`）；`score < floor` 才是
/// 真的没命中（该补记忆或换线索）。这两种情况的处置完全相反。
pub fn preview(request_text: &str) -> Value {
    let settings = settings();
    // 注入块与候选命中**必须**用同一份请求文本。以前这里塞的是一句占位中文
    // （「未提供请求文本，仅展示『必须遵守』」），而 `probes` 用的是真文本 ——
    // 两边口径不一致。现在中文线索也参与匹配，占位句不能再来冒充请求。
    let block = compose(request_text);
    // 命中的那几条单独扫一遍：`build` 只回拼好的文本，拿不到「差多少分」。
    // 这条接口是用户手点的，多扫一遍全库是几十毫秒，可以接受。
    let probes = probes(request_text);
    let cjk = cjk_clues(request_text);
    let hits: Vec<Value> = if settings.enabled {
        let entries = store::snapshot();
        let derived = if settings.path_match && path_like(&probes) {
            Some(derived_paths())
        } else {
            None
        };
        let mut rows: Vec<(f64, f64, &MemoryEntry)> = entries
            .iter()
            .filter_map(|entry| {
                let floor = floor_for(entry)?;
                let extra = derived.as_ref().and_then(|index| index.get(&entry.id));
                let score = relevance(entry, &probes, settings.path_match, extra, Some(&cjk));
                (score > 0.0).then_some((score, floor, entry))
            })
            .collect();
        rows.sort_by(|left, right| {
            right
                .0
                .partial_cmp(&left.0)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        // ── 去重必须在截断**之前** ──────────────────────────────
        // 先截 12 条再按组去重，得到的是「12 条里剩 1 条」，而不是「12 个不同的
        // 组」。真机就是这样：前 12 条全是同一个导入会话的碎片（`538e6914-…`，
        // 只是序号不同），界面上看着像 12 条候选，其实是同一件事说了 12 遍。
        //
        // 这里逐条走完，把「过线且是某个组的第一条」的数到 12 为止；重复项仍然
        // 收进来（但标 `duplicate: true`），用户才能看见「14 条候选里 13 条被折叠」
        // —— 那是门槛和去重都在工作的证据。
        let mut seen_groups: Vec<String> = Vec::new();
        let mut seen_lines: Vec<String> = Vec::new();
        let mut kept: Vec<(f64, f64, &MemoryEntry, bool, bool, String, String)> = Vec::new();
        let mut fresh_passed = 0usize;
        for (score, floor, entry) in rows {
            let group = group_key(entry);
            let text = line(entry, 56);
            let duplicate = seen_groups.iter().any(|known| known == &group)
                || seen_lines.iter().any(|known| known == &text);
            if !duplicate {
                seen_groups.push(group.clone());
                seen_lines.push(text.clone());
            }
            let pass = score >= floor;
            if pass && !duplicate {
                fresh_passed += 1;
            }
            kept.push((score, floor, entry, pass, duplicate, group, text));
            if fresh_passed >= 12 || kept.len() >= 24 {
                break;
            }
        }
        kept.into_iter()
            .map(|(score, floor, entry, pass, duplicate, group, text)| {
                serde_json::json!({
                    "id": entry.id,
                    "title": entry.title,
                    "type": entry.kind.as_str(),
                    "layer": if entry.layer == Layer::L2 { "l2" } else { "l1" },
                    "score": (score * 100.0).round() / 100.0,
                    "floor": floor,
                    "pass": pass,
                    // 同一件事的重复项：`pass` 仍为真，但不会进注入块
                    "duplicate": duplicate,
                    "group": group,
                    "line": text,
                    "files": entry.files.clone(),
                })
            })
            .collect()
    } else {
        Vec::new()
    };
    serde_json::json!({
        "enabled": settings.enabled,
        "settings": {
            "maxTokens": settings.max_tokens,
            "rulesTokens": settings.rules_tokens,
            "experienceTokens": settings.experience_tokens,
            "maxItems": settings.max_items,
            "pathMatch": settings.path_match,
        },
        "probes": probes,
        // 中文线索抽到了多少个四字组：界面上能一眼看出「这次到底有没有中文线索」，
        // 中文检索为 0 条时先看这个数，别再去猜。
        "cjkGrams": cjk.len(),
        "hits": hits,
        "text": block,
        "tokens": block.as_deref().map(estimate_tokens).unwrap_or(0),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn internal_origins_skip_injection() {
        // 记忆自己的蒸馏 / 画像任务是打本进程网关跑的，给它们注入记忆会让旧结论
        // 给新结论背书 —— 蒸馏会自我复制，新信息再也进不来。
        assert!(is_internal_origin("Agent2API-MemoryBrain/1.0"));
        assert!(is_internal_origin("agent2api-vision/2.0"));
        // 真实客户端不能被误判：误判的后果是「用户装了记忆但永远不注入」，
        // 而且没有任何报错 —— 那种 bug 只能靠用户报「怎么没生效」才发现。
        assert!(!is_internal_origin("claude-cli/2.0.1 (external, cli)"));
        assert!(!is_internal_origin("codex_cli_rs/0.51.0"));
        assert!(!is_internal_origin(""));
    }

    // ── 真机事故回归（attempts=109 / count=0）────────────────────

    #[test]
    fn marker_in_user_text_does_not_block_injection() {
        // 真机事故：历史摘要正文里写了 `<agent2api-memory>` 这个标签名，模型也把
        // 注入块贴回过聊天里。第一版 `already_chat` 扫**全部**消息找 MARKER，
        // 于是从那一刻起整条会话被永久判成「已注入」，注入一次都没发生
        //（计数器 attempts=109 / count=0 / missed=109）。
        let body = serde_json::json!({
            "messages": [
                { "role": "system", "content": "你是助手" },
                // 历史摘要里提到这个标签名 —— 必须不算「已注入」
                { "role": "user", "content": "上次说过 ` <agent2api-memory> ` 这个标签" },
                // tool 回显把 inject.rs 读了一遍 —— 也必须不算
                { "role": "tool", "content": "120: pub const MARKER: &str = \"<agent2api-memory>\";" },
                { "role": "user", "content": "继续" }
            ]
        });
        assert!(
            !already_chat(&body),
            "user / tool 文本里出现 MARKER 不能判成已注入"
        );

        // 真正被注入过的形状：system 角色 + 开头就是 MARKER
        let injected = serde_json::json!({
            "messages": [
                { "role": "system", "content": "你是助手" },
                { "role": "system", "content": format!("{MARKER}\n以下是本机记忆中枢…{MARKER_END}") },
                { "role": "user", "content": "继续" }
            ]
        });
        assert!(already_chat(&injected));
        // 前面有空白的也算（渲染时可能有缩进）
        let padded = serde_json::json!({
            "messages": [{ "role": "system", "content": format!("  \n{MARKER}x{MARKER_END}") }]
        });
        assert!(already_chat(&padded));
    }

    #[test]
    fn chat_text_prefers_user_query_over_tool_dump() {
        // 真机请求的形状：一条 user 消息里三个块，第二块是 4 万字符的历史摘要
        // （正文里出现了 3 次 `<user_query>` 但没有闭合标签），第三块才是真任务。
        // 第一版把三块拼成一串再抽标签，第一个 `<user_query>` 一路吃到第三块的
        // 闭合标签 —— 抽出来是整段历史摘要，真任务反而被包在里面。
        let body = serde_json::json!({
            "messages": [
                { "role": "system", "content": "系统说明：用户输入用 <user_query> 标签标注" },
                { "role": "user", "content": [
                    { "type": "text", "text": "<system-reminder>\n当前时间 10:49\n</system-reminder>" },
                    // 历史摘要：含 3 次未闭合的 <user_query> —— 拼接后会造成错配
                    { "type": "text", "text": "<conversation_history_summary>\n1. 修改 `<user_query>` / `<user_query> tag` 这类结构\n2. 之前提到 `<user_query>` 写错的坑\n</conversation_history_summary>" },
                    { "type": "text", "text": "<user_query>改一下 inject.rs 里的 chat_request_text</user_query>" }
                ]},
                // 最近一条 tool：一次 Read 回显了 5 万字符源码
                { "role": "tool", "content": "   1→//! 记忆中枢 请求级注入\n  120→pub const MARKER..." }
            ]
        });
        let text = chat_request_text(&body);
        assert!(
            text.contains("chat_request_text"),
            "真任务必须被取到：{text}"
        );
        assert!(
            !text.contains("conversation_history_summary"),
            "历史摘要不能顶替真任务：{text}"
        );
        // 抽出来的线索必须是路径/标识符，不该是历史摘要里的泛词
        let found = probes(&text);
        assert!(
            found.iter().any(|item| item.contains("inject.rs")),
            "真任务的路径必须进线索池：{found:?}"
        );
    }

    #[test]
    fn chat_text_falls_back_to_tool_when_query_has_no_clue() {
        // 「请继续」这类输入抽不出任何线索。这时必须退到最近工具回显的命令行 ——
        // 那是这一轮唯一可用的定位线索，不退的话注入永远是空的。
        let body = serde_json::json!({
            "messages": [
                { "role": "user", "content": "<user_query>please continue</user_query>" },
                { "role": "tool", "content": "Command: cd repo && wc -l desktop-tauri/src-tauri/server/src/server/core/memory/inject.rs" }
            ]
        });
        let found = probes(&chat_request_text(&body));
        assert!(
            found.iter().any(|item| item.contains("inject.rs")),
            "用户输入无线索时必须回退到工具回显：{found:?}"
        );
    }

    #[test]
    fn probes_skip_temp_files_and_library_idents() {
        // 真机抽出来的第一批线索全是噪音：它们来自「我自己排障时」写出的临时
        // 文件和脚本内部名，`files` 里永远不会有记忆锚定到它们，纯占名额。
        let text = "python -c \"json.dump(..., open('.workbuddy/tmp/entries-full.json'))\" \
                    conn.row_factory = sqlite3.Row; ensure_ascii=False; deleted_at is null; \
                    改 desktop-tauri/src-tauri/server/src/server/core/memory/inject.rs";
        let found = probes(text);
        assert!(
            !found.iter().any(|item| item.contains("workbuddy/tmp")),
            "临时目录不该进线索池：{found:?}"
        );
        assert!(
            !found.iter().any(|item| item == "row_factory" || item == "deleted_at" || item == "ensure_ascii"),
            "库内部标识符不该进线索池：{found:?}"
        );
        assert!(
            found.iter().any(|item| item.contains("memory/inject.rs")),
            "真路径必须留住：{found:?}"
        );
    }

    #[test]
    fn probes_rank_paths_above_bare_filenames() {
        // 一次 `ls` 会回显十几个光杆文件名（`agents.rs`、`config.rs`…）。它们和
        // 真正被改的那条路径混在一起时，必须让带目录的路径排在前面并占满额度 ——
        // 带目录的路径在仓库里唯一，光杆文件名有十几个同名。
        let text = "ls memory/\nagents.rs\nconfig.rs\nenrich.rs\nimport.rs\nimport_registry.rs\n\
                    index.rs\ninject.rs\nlayout.rs\nllm.rs\nmod.rs\nmodel.rs\nredact.rs\n\
                    schedule.rs\nsearch.rs\nstore.rs\nsync.rs\n\
                    改的是 desktop-tauri/src-tauri/server/src/server/core/memory/inject.rs";
        let found = probes(text);
        let path_at = found
            .iter()
            .position(|item| item.contains("memory/inject.rs"))
            .expect("带目录的路径必须在：{found:?}");
        let first_bare = found.iter().position(|item| item == "inject.rs");
        assert!(
            first_bare.map(|index| path_at < index).unwrap_or(true),
            "带目录的路径必须排在光杆文件名之前：{found:?}"
        );
        // 「光杆文件名」的判据必须与实现一致：**带扩展名**且不含目录分隔。
        // 第一版这里写成 `!item.contains('/')`，于是 `import_registry`（snake_case
        // 标识符，来自标识符桶）也被算成一个光杆文件名 —— 5 > 4 直接红。
        // 那不是逻辑错，是这条断言把两档混在一起数了（CI 上真红过一次）。
        let bare = found
            .iter()
            .filter(|item| !item.contains('/') && item.contains('.'))
            .count();
        assert!(bare <= 4, "光杆文件名最多占 4 个名额：{found:?}");
    }

    #[test]
    fn probes_normalise_backslashes_and_drop_odd_backtick_runs() {
        // Windows 路径的反斜杠形态必须归一化：不归一化就会抽出两条（\personal\…
        // 与 /personal/…），而记忆里存的是正斜杠，反斜杠那条永远命不中。
        let found = probes("读 D:\\Personal\\Desktop\\agent2api-main\\desktop-tauri\\src-tauri\\server\\src\\server\\core\\memory\\inject.rs");
        assert!(
            found.iter().all(|item| !item.contains('\\')),
            "线索里不该残留反斜杠：{found:?}"
        );
        assert!(
            found.iter().any(|item| item.contains("memory/inject.rs")),
            "归一化后必须还能命中：{found:?}"
        );

        // 奇数个反引号会导致配对错位，抽出一整句中文 —— 那一档要求「不含空白」，
        // 句子必然含空格，所以自然被挡掉。
        let noisy = probes("当前 `system` 里的 ` <user_query> tag` 是说明文字，`后面没了");
        assert!(
            noisy.iter().all(|item| !item.contains(' ') && item.is_ascii()),
            "错位抽出来的句子必须被挡掉：{noisy:?}"
        );
    }

    #[test]
    fn junk_titles_are_detected_and_not_rendered() {
        // 导入器拿「会话 id + 序号」拼标题，同一个会话有 2200 多条这种条目。
        // 把裸 UUID 渲染进注入块是纯浪费：模型从那串十六进制里得不到任何信息。
        assert!(junk_title("538e6914-d5c6-486e-8635-a90ef475b1c9（866）"));
        assert!(junk_title("538e6914-d5c6-486e-8635-a90ef475b1c9"));
        assert!(junk_title(""));
        // 正常标题不能被误判
        assert!(!junk_title("注入点"));
        assert!(!junk_title("beta.1 出包与索引锁"));
        assert!(!junk_title("deliver.mjs 是唯一交付入口"));

        let mut entry = MemoryEntry::draft(
            "j1",
            "538e6914-d5c6-486e-8635-a90ef475b1c9（866）",
            MemoryKind::Daily,
            "正文",
        );
        entry.summary_text = "beta.1 安装包 19.1MB 出包，三闸全绿".to_string();
        let rendered = line(&entry, 56);
        assert!(
            !rendered.contains("538e6914"),
            "裸 UUID 不该出现在注入行里：{rendered}"
        );
        assert!(
            rendered.contains("19.1MB"),
            "摘要必须留下 —— 那是这条唯一有用的内容：{rendered}"
        );
    }

    #[test]
    fn same_session_shards_share_a_group() {
        // 真机跑分得到 14 条过线候选，全部来自同一个导入会话（同前缀、只差序号）。
        // 不去重的话 6 个名额会被「一件事的六种流水写法」占满。
        let mut first = MemoryEntry::draft("a", "538e6914-d5c6-486e-8635-a90ef475b1c9（866）", MemoryKind::Daily, "正文");
        first.summary_text = "出包成功".to_string();
        let mut second = MemoryEntry::draft("b", "538e6914-d5c6-486e-8635-a90ef475b1c9（15）", MemoryKind::Daily, "正文");
        second.summary_text = "另一段流水".to_string();
        assert_eq!(
            group_key(&first),
            group_key(&second),
            "同一会话的碎片必须落在同一组"
        );
        // 不同会话不能被并到一组
        let other = MemoryEntry::draft("c", "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee（1）", MemoryKind::Daily, "正文");
        assert_ne!(group_key(&first), group_key(&other));
    }

    #[test]
    fn rules_survive_an_empty_request_even_with_zero_clues() {
        // 「必须遵守」不依赖线索：用户钉的规矩对每一条请求都成立。第一版在
        // `compose` 里对空文本直接 `return None`，于是「版本号保持 beta.1」
        // 这类硬规则在取不到任务文本的请求里全都被丢掉。
        // 这里只钉判据本身（不碰真实快照）：空线索 + 有规则候选时，`build` 的
        // 规则段必须还能产出一条规则行 —— 用一个 draft 条目直接验证渲染路径。
        let mut rule = MemoryEntry::draft(
            "r1",
            "版本号保持 3.5.0-beta.1",
            MemoryKind::Preference,
            "用户明确要求：不要自行 bump 版本号。",
        );
        rule.pinned = true;
        assert!(rule_candidate(&rule), "钉住的条目必须进「必须遵守」");
        let rendered = line(&rule, 56);
        assert!(rendered.contains("3.5.0-beta.1"), "{rendered}");

        // 空线索时 `probes("")` 为空 —— 「相关经验」段自然空，但规则段不受影响
        assert!(probes("").is_empty());
    }

    #[test]
    fn probes_pick_paths_and_identifiers() {
        let text = "改一下 desktop-tauri/src-tauri/server/src/server/api/chat.rs 里的 \
                    `inject_chat`，然后跑 node scripts/deliver.mjs";
        let found = probes(text);
        assert!(
            found.iter().any(|item| item.contains("chat.rs")),
            "路径必须被抽出来：{found:?}"
        );
        assert!(
            found.iter().any(|item| item == "inject_chat"),
            "snake_case 标识符必须被抽出来：{found:?}"
        );
        assert!(
            found.iter().any(|item| item.contains("deliver.mjs")),
            "命令里的脚本名必须被抽出来：{found:?}"
        );
    }

    #[test]
    fn probes_ignore_noise() {
        // 纯自然语言不该产出线索 —— 否则「这个」「问题」会命中一切
        assert!(probes("这个功能还有问题，再看看").is_empty());
    }

    #[test]
    fn probes_pick_camel_case() {
        // camelCase 必须单独在**原文**上匹配：小写化之后 `injectChat` 变成
        // `injectchat`，那条正则就永远匹配不到 —— 这是第一版真实的 bug。
        let found = probes("看看 injectChat 和 memoryPage 这两个函数");
        assert!(
            found.iter().any(|item| item == "injectchat"),
            "camelCase 必须被抽出来（小写化后入库）：{found:?}"
        );
        assert!(
            found.iter().any(|item| item == "memorypage"),
            "第二个 camelCase 也要抽出来：{found:?}"
        );
    }

    #[test]
    fn probe_budget_is_capped() {
        // 名额按桶分配（路径 10 + 标识符 10 + 光杆文件名 4 = 24）。一整段长日志
        // 塞进 60 个标识符时，最多只能占满标识符那一桶 —— 路径桶永远留得下位置，
        // 这是「一次日志把真任务的路径挤出去」那个真 bug 的回归钉。
        let text = (0..60)
            .map(|index| format!("foo_bar_{index}"))
            .collect::<Vec<_>>()
            .join(" ");
        assert_eq!(probes(&text).len(), 10, "标识符桶上限 10");

        // 三档加起来不超过 24
        let mixed = format!(
            "{}\n{}",
            (0..30)
                .map(|index| format!("dir{index}/nested/file{index}.rs"))
                .collect::<Vec<_>>()
                .join("\n"),
            (0..30)
                .map(|index| format!("ident_{index}"))
                .collect::<Vec<_>>()
                .join(" ")
        );
        assert!(probes(&mixed).len() <= 24, "总名额 24 是硬上限");
    }

    #[test]
    fn header_forbids_trusting_stale_memory() {
        // 这条文案是硬约束：少了它，模型会拿旧记忆去改刚改对的代码
        let text = header();
        assert!(text.contains("不代表当前代码状态"), "{text}");
        assert!(text.contains("改文件前先读文件"), "{text}");
    }

    #[test]
    fn insert_chat_lands_after_leading_system_blocks() {
        let mut body = serde_json::json!({
            "model": "x",
            "messages": [
                { "role": "system", "content": "你是 Claude Code" },
                { "role": "system", "content": "项目规范" },
                { "role": "user", "content": "hi" }
            ]
        });
        assert!(insert_chat(&mut body, "<agent2api-memory>x</agent2api-memory>"));
        let messages = body.get("messages").and_then(Value::as_array).unwrap();
        assert_eq!(messages.len(), 4);
        // 身份声明仍在最前，记忆是第三条
        assert_eq!(messages[0].get("role").and_then(Value::as_str), Some("system"));
        assert!(messages[2]
            .get("content")
            .map(|value| value.to_string().contains(MARKER))
            .unwrap_or(false));
    }

    #[test]
    fn already_chat_detects_marker() {
        let injected = serde_json::json!({
            "messages": [{ "role": "system", "content": format!("{MARKER}…{MARKER_END}") }]
        });
        assert!(already_chat(&injected));
        let plain = serde_json::json!({ "messages": [{ "role": "user", "content": "hi" }] });
        assert!(!already_chat(&plain));
    }

    #[test]
    fn already_native_ignores_marker_mentioned_in_system_prompt() {
        use crate::server::core::protocol::native::WireProtocol;
        // 真机事故形态：系统提示词里**提到过**这个标签（讨论本仓源码、贴
        // CLAUDE.md、Codex 的 AGENTS.md 里写了注入块长什么样），但并不是本进程
        // 追加的那一块。旧判据 `to_string().contains(MARKER)` 会把它当成「已注入」，
        // 从此这条路永久不注入，而计数器显示 count 恒 0。
        let poisoned = serde_json::json!({
            "instructions": format!("你在改 agent2api。注入块长这样：{MARKER}…{MARKER_END}，注意别重复。"),
            "input": "把注入修好"
        });
        assert!(
            !already_native(&poisoned, "instructions", WireProtocol::Responses),
            "系统提示词里提到字面量 ≠ 已注入"
        );

        // 真正追加过的那一块：结尾恒为 MARKER_END
        let injected = serde_json::json!({
            "instructions": format!("你在改 agent2api。\n\n{MARKER}\n规则…\n{MARKER_END}"),
            "input": "把注入修好"
        });
        assert!(already_native(&injected, "instructions", WireProtocol::Responses));

        // 没这个字段 = 没注入
        assert!(!already_native(
            &serde_json::json!({ "input": "hi" }),
            "instructions",
            WireProtocol::Responses
        ));
    }

    #[test]
    fn already_native_checks_last_system_block_for_anthropic() {
        use crate::server::core::protocol::native::WireProtocol;
        // Anthropic 的 `system` 是内容块数组：只在**最后一块**是记忆块时才算注入过。
        // 前面某块正文里提到标签不算 —— 那正是旧判据会误判的形状。
        let mentioned = serde_json::json!({
            "system": [
                { "type": "text", "text": format!("以前见过 {MARKER} 这种块，别照抄。") },
                { "type": "text", "text": "你是资深工程师。" }
            ]
        });
        assert!(!already_native(
            &mentioned,
            "system",
            WireProtocol::Anthropic
        ));

        let injected = serde_json::json!({
            "system": [
                { "type": "text", "text": "你是资深工程师。" },
                { "type": "text", "text": format!("{MARKER}\n规则…\n{MARKER_END}") }
            ]
        });
        assert!(already_native(&injected, "system", WireProtocol::Anthropic));
    }

    #[test]
    fn append_native_handles_string_and_blocks() {
        use crate::server::core::protocol::native::WireProtocol;
        let mut responses = serde_json::json!({ "instructions": "原有说明" });
        assert!(append_native(&mut responses, WireProtocol::Responses, "instructions", "MEM"));
        let text = responses.get("instructions").and_then(Value::as_str).unwrap();
        assert!(text.starts_with("原有说明"), "原有内容不能被覆盖：{text}");
        assert!(text.ends_with("MEM"));

        // Anthropic 的 system 是块数组时，必须 push 一个 text 块而不是拼字符串
        let mut anthropic = serde_json::json!({ "system": [{ "type": "text", "text": "原有" }] });
        assert!(append_native(&mut anthropic, WireProtocol::Anthropic, "system", "MEM"));
        let parts = anthropic.get("system").and_then(Value::as_array).unwrap();
        assert_eq!(parts.len(), 2);
        assert_eq!(parts[1].get("text").and_then(Value::as_str), Some("MEM"));

        // 没有 system 字段时直接插一个字符串
        let mut empty = serde_json::json!({});
        assert!(append_native(&mut empty, WireProtocol::Anthropic, "system", "MEM"));
        assert_eq!(empty.get("system").and_then(Value::as_str), Some("MEM"));
    }

    #[test]
    fn harvest_paths_requires_a_directory_segment() {
        // 带目录的路径要抽出来，并且反斜杠统一成正斜杠（Windows 写法与 Unix
        // 写法是同一条路径，不归一化就等于两条都不命中）
        let found = harvest_paths("改了 src\\server\\api\\chat.rs 和 desktop-tauri/ui-islands/src/islands/memory-api.ts");
        assert!(found.iter().any(|item| item == "src/server/api/chat.rs"), "{found:?}");
        assert!(found.iter().any(|item| item == "desktop-tauri/ui-islands/src/islands/memory-api.ts"), "{found:?}");
        // 光杆文件名不收：`chat.rs` 在仓库里可能有十几个，收进来就是「命中一切」
        assert!(
            !harvest_paths("再看一下 chat.rs 这个文件").iter().any(|item| item == "chat.rs"),
            "光杆文件名不该被收进 files"
        );
        // 非代码扩展名不收
        assert!(harvest_paths("读一下 C:/Users/me/Desktop/报表.xlsx").is_empty());
    }

    #[test]
    fn file_hit_matches_three_shapes() {
        let mut entry = MemoryEntry::draft("t1", "注入点", MemoryKind::Pitfall, "正文");
        entry.files = vec!["desktop-tauri/src-tauri/server/src/server/api/chat.rs".to_string()];
        let anchored = || entry.files.iter().map(String::as_str);
        // 记忆里的路径比请求里的短：请求带了绝对路径前缀
        assert!(file_hit(anchored(), "/d/personal/desktop/agent2api-main/desktop-tauri/src-tauri/server/src/server/api/chat.rs"));
        // 记忆里的路径比请求里的长：请求只给了仓库相对路径的一部分
        assert!(file_hit(anchored(), "src/server/api/chat.rs"));
        // 请求里只有文件名
        assert!(file_hit(anchored(), "chat.rs"));
        // 完全不相干
        assert!(!file_hit(anchored(), "deliver.mjs"));
        // 短线索（3 字符）不该命中路径 —— `api` 会命中 `.../api/chat.rs`
        assert!(!file_hit(anchored(), "api"));
    }

    #[test]
    fn short_probes_do_not_match_head() {
        // 短线索（`api` / `md`）在几千条记忆里几乎必然出现在某条标题上，
        // 放行就等于每次注入都掺一条不相干的记忆
        assert!(!head_hit("api gateway 的说明", "api"));
        assert!(head_hit("api gateway 的说明", "gateway"));
    }

    #[test]
    fn floors_let_imported_l1_in_but_keep_noise_out() {
        // 导入的会话流水是 l1 / daily / importance 0.4 —— 它们占了本机库的绝大多数。
        // 门槛必须是「路径命中或两条线索」才进，否则注入对真实数据一条都命中不了。
        let mut imported = MemoryEntry::draft("i1", "改注入点", MemoryKind::Daily, "正文");
        imported.layer = Layer::L1;
        imported.importance = 0.4;
        imported.files = vec!["desktop-tauri/src-tauri/server/src/server/api/chat.rs".to_string()];
        let floor = floor_for(&imported).expect("普通 l1 也要有门槛（不是不进池）");
        assert!(floor > 3.0, "普通流水的门槛必须高于「单条路径命中」：{floor}");
        // 一条路径命中 = 3.0 + 0.4，刚好过线
        let one_path = vec!["chat.rs".to_string()];
        assert!(
            relevance(&imported, &one_path, true, None, None) >= floor,
            "路径命中必须能进：{} vs {floor}",
            relevance(&imported, &one_path, true, None, None)
        );
        // 一个泛词命中 = 2.0 + 0.4，不该进
        let mut titled = MemoryEntry::draft("i2", "注入点说明", MemoryKind::Daily, "正文");
        titled.layer = Layer::L1;
        titled.importance = 0.4;
        let noise = vec!["注入点说明".to_string()];
        assert!(relevance(&titled, &noise, true, None, None) < floor_for(&titled).unwrap());

        // 沉淀层与「天然是结论」的类别门槛低：它们本来就是结论
        let mut l2 = MemoryEntry::draft("l2", "注入点", MemoryKind::Knowledge, "正文");
        l2.layer = Layer::L2;
        assert_eq!(floor_for(&l2), Some(2.0));
        let mut pitfall = MemoryEntry::draft("p1", "注入点", MemoryKind::Pitfall, "正文");
        pitfall.layer = Layer::L1;
        assert_eq!(floor_for(&pitfall), Some(2.0));

        // 死条目一律不进
        let mut dead = MemoryEntry::draft("d1", "x", MemoryKind::Knowledge, "正文");
        dead.deleted_at = Some(1);
        assert_eq!(floor_for(&dead), None);
    }

    #[test]
    fn derived_paths_let_imported_entries_match_by_file() {
        // 存量导入条目的 `files` 是空的（那时还没抽路径），而它们占了库里绝大多数。
        // `derived_paths` 从正文补一份内存索引，让这批数据在「按文件命中」这一档
        // 上不再是隐形的 —— 否则路径命中对存量数据永久失效。
        let mut legacy = MemoryEntry::draft("legacy", "改网关", MemoryKind::Daily, "正文");
        legacy.layer = Layer::L1;
        legacy.importance = 0.4;
        legacy.files = Vec::new();
        let body = "今天改了 desktop-tauri/src-tauri/server/src/server/api/chat.rs 里的注入";
        let extra = harvest_paths(body);
        assert!(!extra.is_empty(), "正文里的路径必须被抽出来：{extra:?}");

        let probe = vec!["chat.rs".to_string()];
        let floor = floor_for(&legacy).unwrap();
        // 没有派生路径：只剩标题可比，进不了
        assert!(relevance(&legacy, &probe, true, None, None) < floor);
        // 有派生路径：3.0 + 0.4 过线
        assert!(
            relevance(&legacy, &probe, true, Some(&extra), None) >= floor,
            "派生路径命中必须能进：{} vs {floor}",
            relevance(&legacy, &probe, true, Some(&extra), None)
        );
        // `files` 非空时不该再走派生路径（自己那份更准）—— 由调用侧保证，
        // 这里钉住判据：两处都命中不会重复加分，因为 `file_hit` 是短路 `any`
    }

    #[test]
    fn path_like_only_fires_on_real_paths() {
        // 建派生索引要扫全库正文，纯文字请求不该付这份钱
        assert!(!path_like(&["inject_chat".to_string()]));
        assert!(!path_like(&["a/b".to_string()]));
        assert!(path_like(&["src/server/api/chat.rs".to_string()]));
        assert!(path_like(&["src\\server\\chat.rs".to_string()]));
    }

    // ── 中文线索（真机事故：中文提问永远注不进「相关经验」）────────

    #[test]
    fn cjk_clues_pick_chinese_and_ignore_ascii() {
        assert!(!cjk_clues("帮我看看记忆中枢的检索").is_empty());
        assert!(cjk_clues("just plain english text").is_empty());
        assert!(cjk_clues("   ").is_empty());
        // 三个字不算线索：中文三个字太碎，比英文的 4 字符门槛还宽一格
        assert!(cjk_clues("中文的").is_empty());
    }

    #[test]
    fn cjk_clues_skip_client_envelopes() {
        // `<system-reminder>` 里全是中文，但那是客户端的说明文字，不是用户说的话。
        // 拿它当线索，命中的会是「客户端在解释自己」那条记忆，与本轮任务无关。
        let clues = cjk_clues(
            "<system-reminder>客户端注入的说明文字</system-reminder>用户真正说的话在这里",
        );
        assert!(!clues.is_empty());
        assert!(
            !clues.covers("客户端注入的说明文字"),
            "客户端包裹块里的中文不能进线索"
        );
        assert!(
            clues.covers("用户真正说的话在这里"),
            "用户原话必须进线索"
        );
    }

    #[test]
    fn chinese_request_can_hit_imported_l1() {
        // 真机 2026-11-22：`/api/memory/inject/preview?q=复用` 命中 0 条，
        // 同一条中文在 FTS 列表检索里命中 10 条 —— 差别就在这里：注入走
        // `probes()`，而它只抽 ASCII。中文提问 = 永远失忆。
        let mut imported =
            MemoryEntry::draft("i3", "记忆中枢按复用价值沉淀", MemoryKind::Daily, "正文");
        imported.layer = Layer::L1;
        imported.importance = 0.4;
        let floor = floor_for(&imported).unwrap();
        let clues = cjk_clues("记忆中枢按复用价值沉淀");
        let score = relevance(&imported, &[], true, None, Some(&clues));
        assert!(
            score >= floor,
            "整段中文被复述必须能进：{score} vs {floor}"
        );
    }

    #[test]
    fn weak_chinese_match_keeps_l1_precision() {
        // 一个四字词组 = 一条独立线索 = 2.0 分，与 ASCII 的单条标题命中同档：
        // 普通 l1 的门槛是 3.3，单条进不来 —— 精度不因中文而放松。
        let mut titled = MemoryEntry::draft("i4", "注入点说明", MemoryKind::Daily, "正文");
        titled.layer = Layer::L1;
        titled.importance = 0.4;
        let floor = floor_for(&titled).unwrap();
        let one = cjk_clues("注入点说明");
        let score = relevance(&titled, &[], true, None, Some(&one));
        assert!(score < floor, "单个词组不该进普通 l1：{score} vs {floor}");

        // 两条独立线索 = 4.0 分 → 进得来（与 ASCII 的两条探针同一口径）
        let mut two =
            MemoryEntry::draft("i5", "注入点说明", MemoryKind::Daily, "还有一条别的结论");
        two.layer = Layer::L1;
        two.importance = 0.4;
        let clues = cjk_clues("注入点说明 还有一条别的结论");
        let score = relevance(&two, &[], true, None, Some(&clues));
        assert!(
            score >= floor_for(&two).unwrap(),
            "两条独立中文线索必须能进：{score}"
        );
    }

    #[test]
    fn probes_stay_ascii_only() {
        // 中文**不能**进 `probes()`：那条链路靠「`probes()` 为空就补工具回显」
        // 来拿路径线索（`chat_request_text_from`）。中文一进去，纯中文提问就
        // 再也拿不到工具回显里的路径 —— 那是拿旧的毛病换新的毛病。
        assert!(probes("帮我看看记忆中枢的检索").is_empty());
        assert!(
            probes("改一下 inject.rs 里的 probes")
                .contains(&"inject.rs".to_string())
        );
    }

    #[test]
    fn cache_key_separates_different_chinese_requests() {
        // 中文线索必须进缓存键：两条请求的 ASCII 线索可以完全一样，
        // 不进键就会拿上一条请求的注入块来用，中文那部分整个串台。
        let settings = Settings::default();
        let probes = vec!["inject.rs".to_string()];
        let one = cjk_clues("记忆中枢按复用价值沉淀");
        let two = cjk_clues("完全不同的另一段中文内容");
        assert_ne!(
            cache_key(&settings, &probes, Some(&one)),
            cache_key(&settings, &probes, Some(&two))
        );
        assert_eq!(
            cache_key(&settings, &probes, Some(&one)),
            cache_key(&settings, &probes, Some(&one)),
            "同一份中文线索必须算出同一个键，否则缓存永远不命中"
        );
    }
}
