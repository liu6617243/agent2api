//! 记忆中枢 · 数据模型与 frontmatter 序列化。
//!
//! ── 为什么正文用 Markdown + frontmatter，而不是直接进库 ──────
//! 记忆是「跨会话、跨客户端」的长期资产，用户会想直接打开看、想跟着仓库走、
//! 想用 git diff 看出「这条什么时候改的」。塞进 SQLite 的 BLOB 之后，这三件
//! 事一件都做不了。所以事实源是 `.md` 文件，库只是**可随时重建的派生索引**
//! （见 `index`）。
//!
//! 本文件只负责「一条记忆 ⇄ 一段带 frontmatter 的 Markdown」这个映射，
//! 不碰文件系统（那是 `store` 的事），也不碰索引（那是 `index` 的事）。
//!
//! ── frontmatter 为什么自己写解析，不用 yaml crate ──────────────
//! 我们写的键是固定的一小撮，值的类型也只有「字符串 / 数字 / 布尔 / 字符串
//! 数组」。引一个完整 YAML 实现换来的是「用户手写 YAML 也能解析」这种我们用
//! 不到的能力，代价是多一个依赖 + 一份「它怎么理解空值/锚点/多行折叠」的
//! 未知。这里的解析器只认自己写出去的那几种形态，遇到读不懂的行就跳过 ——
//! 手改过的文件最坏丢掉一个字段，而不是整条记忆读不出来。

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

/// 记忆层。`l1` = 原始日常（一天一文件多节），`l2` = 蒸馏/沉淀（一文件一条）。
///
/// ── 为什么要有这一层 ────────────────────────────────────────
/// 「原始流水」与「沉淀后的结论」是两种东西：前者量大、时效短、单条价值低；后者
/// 量小、长期有效、值得进核心记忆。混在一个池子里排序，结果是流水把结论挤下去。
/// 分层之后检索可以给 l2 加权（`search.weightLayer`），核心记忆只喂 l2。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum Layer {
    /// 原始日常：这次聊了什么、做到哪一步
    #[default]
    L1,
    /// 蒸馏/沉淀：决策、知识、画像结论、洞见
    L2,
}

impl Layer {
    pub fn parse(raw: &str) -> Self {
        if raw.trim().eq_ignore_ascii_case("l2") { Layer::L2 } else { Layer::L1 }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Layer::L1 => "l1",
            Layer::L2 => "l2",
        }
    }
}

/// l2 的子目录（按类别分）。白名单外的一律进 `summary`。
///
/// `pitfall` 与 `insight` 合并到 `insight/`：两者都是「从经验里提炼出的判断」，
/// 分开只是多一个目录，检索上没有任何区别。
pub fn l2_dir_for(kind: &str) -> &'static str {
    match kind {
        "decision" => "decisions",
        "knowledge" => "knowledge",
        "insight" | "pitfall" => "insight",
        _ => "summary",
    }
}

/// 去重状态。只用于展示与统计 —— 「是否被取代」看 `superseded_by`。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum DedupStatus {
    /// 独一无二
    #[default]
    Unique,
    /// 已被判为重复并合并（旧条目仍在，标 superseded_by）
    Merged,
    /// 疑似重复，进了待确认队列
    Pending,
}

impl DedupStatus {
    pub fn parse(raw: &str) -> Self {
        match raw.trim().to_ascii_lowercase().as_str() {
            "merged" => DedupStatus::Merged,
            "pending" => DedupStatus::Pending,
            _ => DedupStatus::Unique,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            DedupStatus::Unique => "unique",
            DedupStatus::Merged => "merged",
            DedupStatus::Pending => "pending",
        }
    }
}

/// 记忆类别。`other` 是兜底：手写 frontmatter 写了不认识的 type 时不让整条
/// 记忆失效（类别只影响筛选与展示，不是权限）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MemoryKind {
    /// 日常流水：这次聊了什么、做到哪一步
    Daily,
    /// 决策：为什么这么选、排除了什么
    Decision,
    /// 知识：外部事实、接口形态、平台规矩
    Knowledge,
    /// 偏好：用户/团队长期口味与习惯
    Preference,
    /// 避坑：踩过什么、下次怎么绕
    Pitfall,
    /// 画像：关于用户本人的长期事实
    Profile,
    /// 洞见：跨项目可复用的规律（蒸馏产物）
    Insight,
    Other,
}

impl MemoryKind {
    pub fn parse(text: &str) -> Self {
        match text.trim().to_ascii_lowercase().as_str() {
            "daily" => MemoryKind::Daily,
            "decision" => MemoryKind::Decision,
            "knowledge" => MemoryKind::Knowledge,
            "preference" => MemoryKind::Preference,
            "pitfall" => MemoryKind::Pitfall,
            "profile" => MemoryKind::Profile,
            "insight" => MemoryKind::Insight,
            _ => MemoryKind::Other,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            MemoryKind::Daily => "daily",
            MemoryKind::Decision => "decision",
            MemoryKind::Knowledge => "knowledge",
            MemoryKind::Preference => "preference",
            MemoryKind::Pitfall => "pitfall",
            MemoryKind::Profile => "profile",
            MemoryKind::Insight => "insight",
            MemoryKind::Other => "other",
        }
    }

    /// 界面上给人看的中文名（与前端 `MEMORY_KIND_LABELS` 同口径）
    pub fn label(self) -> &'static str {
        match self {
            MemoryKind::Daily => "日常",
            MemoryKind::Decision => "决策",
            MemoryKind::Knowledge => "知识",
            MemoryKind::Preference => "偏好",
            MemoryKind::Pitfall => "避坑",
            MemoryKind::Profile => "画像",
            MemoryKind::Insight => "洞见",
            MemoryKind::Other => "其他",
        }
    }

    /// 这个类别是否天然属于沉淀层（写入时可以据此默认 layer）。
    pub fn prefers_l2(self) -> bool {
        matches!(
            self,
            MemoryKind::Decision | MemoryKind::Knowledge | MemoryKind::Insight | MemoryKind::Profile
        )
    }

    pub const ALL: [MemoryKind; 8] = [
        MemoryKind::Daily,
        MemoryKind::Decision,
        MemoryKind::Knowledge,
        MemoryKind::Preference,
        MemoryKind::Pitfall,
        MemoryKind::Profile,
        MemoryKind::Insight,
        MemoryKind::Other,
    ];
}

/// 一条记忆。字段与 frontmatter 一一对应（`type` 因为与 Rust 关键字冲突，
/// 在结构体里叫 `kind`，落到文件上仍是 `type`）。
///
/// ── 为什么后面那批字段都带 `#[serde(default)]` ────────────────
/// 这些是「对齐 AgentHub 全量能力」时新增的：`layer` / `dedup_status` / `device` /
/// `session` / `valid_to` / `evidence` / `hash` / `access_count` / `last_hit_at`。
/// 存量用户的 `.md` 里没有它们，而**事实源是文件**——读不出来就等于把用户已有
/// 的记忆全部作废。全部给默认值之后，老文件照常读、照常进索引，下次保存时自然
/// 补上。给默认值不是偷懒，是「磁盘格式只能加不能减」这条约束的实现。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemoryEntry {
    pub id: String,
    pub title: String,
    #[serde(rename = "type")]
    pub kind: MemoryKind,
    /// 层：`l1` 原始日常 / `l2` 沉淀结论。缺省 `l1`（老文件全是「一条一个文件」
    /// 的形态，按原始记录处理最保守）。
    #[serde(default)]
    pub layer: Layer,
    /// 项目 slug。`None` = 不属于任何项目（全局记忆）
    pub project: Option<String>,
    /// 写入方（agent 名 / `manual` / `import`）
    pub agent: Option<String>,
    /// 写入设备 id（`device.json` 里那个）。跨机同步后用来分辨「这条是谁写的」，
    /// 也是冲突三方合并的「base」定位依据之一。
    #[serde(default)]
    pub device: Option<String>,
    /// 会话标识（同一次对话的多条 l1 共享）。缺省 `None`。
    #[serde(default)]
    pub session: Option<String>,
    pub tags: Vec<String>,
    /// 0.0–1.0，检索排序的加权项；缺省 0.5
    pub importance: f64,
    /// 关联的本地文件相对路径。文件时间戳在写入时快照，之后文件被改动
    /// 即可提示「这条记忆可能过时」（`doc_status`）
    pub files: Vec<String>,
    /// 本条推翻/取代了哪些旧记忆 id。旧记忆标 `superseded_by`，不删除
    pub supersedes: Vec<String>,
    pub pinned: bool,
    pub starred: bool,
    pub created_at: i64,
    pub updated_at: i64,
    /// 事实生效时间（与 created_at 不同的场景：补录历史记忆）
    pub valid_from: i64,
    /// 事实失效时间。`Some` = 这条只在 `valid_to` 之前成立（如「旧版 API 的字段名」）。
    /// 与 `superseded_by` 是两条独立的时间轴：前者是「客观过期」，后者是「被新结论顶掉」。
    #[serde(default)]
    pub valid_to: Option<i64>,
    /// 被谁取代（由 `supersedes` 的反向维护写入）
    pub superseded_by: Option<String>,
    /// 回收站时间。`Some` = 已删除但可恢复
    pub deleted_at: Option<i64>,
    /// 去重结论（只用于展示与统计；「是否被取代」看 `superseded_by`）
    #[serde(default)]
    pub dedup_status: DedupStatus,
    /// 证据链锚点：指向支撑这条结论的原始记忆 id 或 `files` 里的路径 + `#hash`。
    /// 深层画像用它在界面上「点结论看依据」。
    #[serde(default)]
    pub evidence: Vec<String>,
    /// 正文归一化后的 SHA-256 前 32 位十六进制。L1 精确去重靠它：
    /// 同一段话被两个 Agent 各写一遍时，第二次能直接命中而不必做全文比较。
    #[serde(default)]
    pub hash: String,
    /// 被 `memory_search` / `memory_get` 命中的累计次数（索引侧维护，回写 frontmatter）。
    /// 检索的 `weightHeat` 项用它 —— 「反复被查到的记忆」比「写完就没人看」的更值得排前。
    #[serde(default)]
    pub access_count: u64,
    /// 最近一次被命中的时间。热度的衰减基准。
    #[serde(default)]
    pub last_hit_at: Option<i64>,
    /// 模型生成的一句话摘要。空 = 还没生成（此时 `summary()` 回退到正文首段）。
    ///
    /// 为什么不直接覆盖 `body`：摘要会丢证据。原文是这条记忆存在的理由，
    /// 模型产出的东西只能作为**附加物**存在 —— 用户不信时可以删掉摘要、
    /// 原文仍然完整。
    #[serde(default)]
    pub summary_text: String,
    /// 正文（frontmatter 之后的部分）。索引里存一份用于检索，源仍在 .md
    pub body: String,
    /// 文件相对路径（相对 memory 根）。恢复/重写时按它定位
    pub path: String,
    /// 进回收站前的原相对路径（`general/l1/.../2026-11-19.md` 这种）。
    ///
    /// 为什么必须存：`path` 在删除那一刻会被改写成 `trash/<id>.md`，原路径就
    /// 丢了；而恢复要「回到它原来待的地方」—— 只按 layer/kind 重算会算错：
    /// 容器里的一节（`l1/<agent>/YYYY-MM-DD.md`）和 v1 那种独立单文件是两种
    /// 形态，重算的结果与删除前不一致，恢复会把记忆写到错误的位置。
    /// 空串 = 不是从回收站里读出来的（活条目）。
    #[serde(default)]
    pub origin_path: String,
}

impl MemoryEntry {
    /// 骨架构造：字段多起来之后（20+ 个），测试与「只关心少数几项」的调用点
    /// 逐个列全字段既啰嗦又容易漏掉新字段。这个函数给出全部默认值，调用方按需
    /// 改三五个字段即可。
    pub fn draft(id: impl Into<String>, title: impl Into<String>, kind: MemoryKind, body: impl Into<String>) -> Self {
        let now = now_ms();
        let body = body.into();
        MemoryEntry {
            id: id.into(),
            title: title.into(),
            kind,
            layer: if kind.prefers_l2() { Layer::L2 } else { Layer::L1 },
            project: None,
            agent: None,
            device: None,
            session: None,
            tags: Vec::new(),
            importance: 0.5,
            files: Vec::new(),
            supersedes: Vec::new(),
            pinned: false,
            starred: false,
            created_at: now,
            updated_at: now,
            valid_from: now,
            valid_to: None,
            superseded_by: None,
            deleted_at: None,
            dedup_status: DedupStatus::Unique,
            evidence: Vec::new(),
            hash: content_hash(&body),
            access_count: 0,
            last_hit_at: None,
            summary_text: String::new(),
            body,
            path: String::new(),
            origin_path: String::new(),
        }
    }

    /// 摘要：优先用模型写过的一句摘要，其次正文第一段非空文本，压到 `limit` 字符。
    /// 索引与「紧凑索引行」用它。
    pub fn summary(&self, limit: usize) -> String {
        if !self.summary_text.trim().is_empty() {
            return truncate_chars(self.summary_text.trim(), limit);
        }
        let first = self
            .body
            .lines()
            .map(str::trim)
            .find(|line| !line.is_empty() && !line.starts_with('#'))
            .unwrap_or("");
        let cleaned = first.trim_start_matches(['-', '*', '>', ' ']).trim();
        truncate_chars(cleaned, limit)
    }

    /// 检索用的标签拼接串（索引里作为一列，FTS 能直接命中标签）
    pub fn tags_text(&self) -> String {
        self.tags.join(" ")
    }

    /// 检索用全文：标题 + 摘要 + 标签 + 正文。索引侧归一化后写进 FTS。
    ///
    /// 标题与摘要**重复**出现在这里（它们本身也是正文的一部分）是刻意的：
    /// FTS5 的 bm25 按词频与文档长度打分，重复一次让「标题命中」比「正文深处
    /// 命中」更容易排到前面，等价于一个廉价的位置加权。
    pub fn search_text(&self) -> String {
        // 标题重复 `index.titleBoost` 次（默认 3）。
        //
        // ── 为什么要靠「重复」而不是加权字段 ─────────────────────
        // FTS5 的 `bm25()` 支持按列权重，但那张表是一列 `text` 大杂烩（标题 /
        // 摘要 / 标签 / 正文拼在一起）。拆成多列要迁移整张表 + 改所有查询；
        // 而「重复标题」是 FTS5 里代价相同、效果等价的手工加权 —— 标题命中
        // 一次等于命中 N 次，bm25 自然把它往前提。
        //
        // 上限 10 由 schema 的 range 兜住；配置读坏时回落 3，不让检索退化成
        // 「标题完全不重要」。改动这个值需要重建索引才会反映到已有条目上。
        let boost = super::config::usize_of("index.titleBoost").clamp(1, 10);
        let title = self.title.repeat(boost);
        format!(
            "{}\n{}\n{}\n{}",
            title,
            self.summary(240),
            self.tags_text(),
            self.body
        )
    }

    /// 该记忆是否仍然有效（未被取代、未进回收站、未过有效期）
    pub fn is_live(&self) -> bool {
        self.superseded_by.is_none() && self.deleted_at.is_none() && !self.is_expired(now_ms())
    }

    /// 是否已超过声明的有效期（`valid_to` 为 `None` 时永不过期）。
    pub fn is_expired(&self, now: i64) -> bool {
        matches!(self.valid_to, Some(until) if until > 0 && now > until)
    }

    /// 接口响应形状。正文按需附带（列表不带全文，`get` 才带）
    pub fn to_json(&self, with_body: bool) -> Value {
        let mut value = json!({
            "id": self.id,
            "title": self.title,
            "type": self.kind.as_str(),
            "typeLabel": self.kind.label(),
            "layer": self.layer.as_str(),
            "project": self.project,
            "agent": self.agent,
            "device": self.device,
            "session": self.session,
            "tags": self.tags,
            "importance": self.importance,
            "files": self.files,
            "supersedes": self.supersedes,
            "supersededBy": self.superseded_by,
            "pinned": self.pinned,
            "starred": self.starred,
            "createdAt": self.created_at,
            "updatedAt": self.updated_at,
            "validFrom": self.valid_from,
            "validTo": self.valid_to,
            "deletedAt": self.deleted_at,
            "dedupStatus": self.dedup_status.as_str(),
            "evidence": self.evidence,
            "hash": self.hash,
            "accessCount": self.access_count,
            "lastHitAt": self.last_hit_at,
            "summary": self.summary(180),
            "summaryText": self.summary_text,
            "chars": self.body.chars().count(),
            "path": self.path,
            "originPath": self.origin_path,
        });
        if with_body {
            value["body"] = Value::String(self.body.clone());
        }
        value
    }
}

/// 正文归一化哈希：SHA-256 前 32 位十六进制。
///
/// 归一化做三件事：CRLF → LF、每行去尾部空白、整体去首尾空白。**不做**大小写
/// 折叠 —— 「不要用 rm」与「不要用 RM」在中文语境里是两条记忆，折叠会误合并。
pub fn content_hash(text: &str) -> String {
    let normalized = normalize_for_hash(text);
    let mut hasher = Sha256::new();
    hasher.update(normalized.as_bytes());
    let digest = hasher.finalize();
    digest.iter().take(16).map(|byte| format!("{byte:02x}")).collect()
}

/// 算哈希前的归一化强度（`dedup.l1.normalizeLevel`）。
///
/// ── 三档的差别是什么 ────────────────────────────────────────
/// `none`：原样算。同一段话换个换行符（`\r\n` vs `\n`）就是两条不同记忆 ——
///         只有在「我要精确到字节」时才该选它。
/// `trim`：只去整体首尾空白。中间的行尾空格、换行风格差异仍然算不同。
/// `full`（默认）：统一换行风格 + 去掉每行行尾空白 + 整体 trim。三档里最接近
///         「这两条讲的是同一件事」的判据 —— 编辑器/剪贴板带来的不可见差异
///         不该制造重复条目。
///
/// ── 为什么读配置而不是写死 ──────────────────────────────────
/// 这张表里这个键一直存在（与 AgentHub 共用 config.json），早先没接线等于
/// 「界面能调、行为不变」。接上之后必须提醒：**改这一档会换掉整库哈希**，
/// 存量条目的 `hash` 与新算法算出的值不再相等，精确去重会失效到重建索引为止。
pub fn normalize_for_hash(text: &str) -> String {
    let level = super::config::text_of("dedup.l1.normalizeLevel");
    match level.trim() {
        "none" => text.to_string(),
        "trim" => text.trim().to_string(),
        // 未知值也走 full：坏配置不该让去重退化成「按字节比」
        _ => text
            .replace("\r\n", "\n")
            .replace('\r', "\n")
            .lines()
            .map(str::trim_end)
            .collect::<Vec<_>>()
            .join("\n")
            .trim()
            .to_string(),
    }
}

fn now_ms() -> i64 {
    crate::server::logging::now_ms()
}

/// 画像条目的稳定 id：`profile-<sub>-<内容哈希前 10 位>`。
///
/// 为什么按内容算而不是按时间：画像每次重生成都会得到一批「说法相同、时间不同」
/// 的条目。按内容算 id 让同一句结论落在同一个文件上（覆盖而不是新增），
/// 用户在回收站/演化链里看到的是「这句结论换过几次说法」，不是「同一句话被
/// 重复生成了 20 次」。
pub fn make_profile_id(sub: &str, body: &str) -> String {
    let safe: String = sub
        .chars()
        .filter(|ch| ch.is_ascii_alphanumeric() || *ch == '-')
        .collect();
    let sub = if safe.is_empty() { "profile".to_string() } else { safe };
    format!("profile-{sub}-{}", &content_hash(body)[..10])
}

/// 按**字符**截断（不按字节）：中文正文按字节切会切出半个字。
pub fn truncate_chars(text: &str, limit: usize) -> String {
    if limit == 0 {
        return String::new();
    }
    let mut out = String::new();
    for (index, ch) in text.chars().enumerate() {
        if index >= limit {
            break;
        }
        out.push(ch);
    }
    out
}

/// 粗估 token 数：CJK 约 1 token/字，ASCII 约 1 token/4 字符。
///
/// 不引 tokenizer：三级披露的预算只需要「别把 800 token 撑成 5000」这个量级的
/// 判断，误差 ±20% 不影响结论；而一个真 tokenizer 会带来 BPE 词表与版本漂移。
pub fn estimate_tokens(text: &str) -> usize {
    let mut cjk = 0usize;
    let mut other = 0usize;
    for ch in text.chars() {
        if is_cjk(ch) {
            cjk += 1;
        } else {
            other += 1;
        }
    }
    cjk + other.div_ceil(4)
}

pub fn is_cjk(ch: char) -> bool {
    matches!(ch as u32,
        0x3400..=0x4DBF | 0x4E00..=0x9FFF | 0xF900..=0xFAFF | 0x3040..=0x30FF | 0xAC00..=0xD7AF)
}

// ── frontmatter 写出 ─────────────────────────────────────────

fn yaml_scalar(text: &str) -> String {
    if text.is_empty() {
        return String::new();
    }
    // 含这些字符就加引号：不加引号时 YAML 会把它当类型/注释/流式集合解释，
    // 我们自己读回来也会被那套规则带偏。
    let needs_quote = text.chars().any(|ch| matches!(ch, ':' | '#' | '[' | ']' | '{' | '}' | '"' | '\'' | ',' | '\n'))
        || text.starts_with(' ')
        || text.ends_with(' ');
    if needs_quote {
        format!("\"{}\"", text.replace('\\', "\\\\").replace('"', "\\\""))
    } else {
        text.to_string()
    }
}

fn yaml_list(items: &[String]) -> String {
    let parts: Vec<String> = items.iter().map(|item| yaml_scalar(item)).collect();
    format!("[{}]", parts.join(", "))
}

impl MemoryEntry {
    /// 整条记忆 → 带 frontmatter 的 Markdown 文本
    pub fn to_markdown(&self) -> String {
        let mut out = String::with_capacity(self.body.len() + 640);
        out.push_str("---\n");
        out.push_str(&format!("id: {}\n", yaml_scalar(&self.id)));
        out.push_str(&format!("title: {}\n", yaml_scalar(&self.title)));
        out.push_str(&format!("type: {}\n", self.kind.as_str()));
        // `l1` 是缺省值：不写出来，让存量文件的 diff 保持最小
        if self.layer != Layer::L1 {
            out.push_str(&format!("layer: {}\n", self.layer.as_str()));
        }
        if let Some(project) = &self.project {
            out.push_str(&format!("project: {}\n", yaml_scalar(project)));
        }
        if let Some(agent) = &self.agent {
            out.push_str(&format!("agent: {}\n", yaml_scalar(agent)));
        }
        if let Some(device) = &self.device {
            out.push_str(&format!("device: {}\n", yaml_scalar(device)));
        }
        if let Some(session) = &self.session {
            out.push_str(&format!("session: {}\n", yaml_scalar(session)));
        }
        out.push_str(&format!("tags: {}\n", yaml_list(&self.tags)));
        out.push_str(&format!("importance: {}\n", self.importance));
        if !self.summary_text.trim().is_empty() {
            out.push_str(&format!("summary: {}\n", yaml_scalar(self.summary_text.trim())));
        }
        if !self.files.is_empty() {
            out.push_str(&format!("files: {}\n", yaml_list(&self.files)));
        }
        if !self.supersedes.is_empty() {
            out.push_str(&format!("supersedes: {}\n", yaml_list(&self.supersedes)));
        }
        if let Some(by) = &self.superseded_by {
            out.push_str(&format!("superseded_by: {}\n", yaml_scalar(by)));
        }
        if self.pinned {
            out.push_str("pinned: true\n");
        }
        if self.starred {
            out.push_str("starred: true\n");
        }
        out.push_str(&format!("created_at: {}\n", self.created_at));
        out.push_str(&format!("updated_at: {}\n", self.updated_at));
        out.push_str(&format!("valid_from: {}\n", self.valid_from));
        if let Some(until) = self.valid_to {
            out.push_str(&format!("valid_to: {until}\n"));
        }
        if let Some(deleted) = self.deleted_at {
            out.push_str(&format!("deleted_at: {deleted}\n"));
            // 只在回收站文件里写原路径：活文件的 path 就是它自己，写出去是噪音，
            // 而且会在下次保存时与事实不一致（文件被搬走后 key 还留在原地）。
            if !self.origin_path.is_empty() {
                out.push_str(&format!("origin_path: {}\n", yaml_scalar(&self.origin_path)));
            }
        }
        if self.dedup_status != DedupStatus::Unique {
            out.push_str(&format!("dedup_status: {}\n", self.dedup_status.as_str()));
        }
        if !self.evidence.is_empty() {
            out.push_str(&format!("evidence: {}\n", yaml_list(&self.evidence)));
        }
        if !self.hash.is_empty() {
            out.push_str(&format!("hash: {}\n", yaml_scalar(&self.hash)));
        }
        // 热度两项只在真被读过时写：绝大多数记忆永远停在 0，写出去只是噪音
        if self.access_count > 0 {
            out.push_str(&format!("access_count: {}\n", self.access_count));
        }
        if let Some(hit) = self.last_hit_at {
            out.push_str(&format!("last_hit_at: {hit}\n"));
        }
        out.push_str("---\n\n");
        out.push_str(&self.body);
        if !out.ends_with('\n') {
            out.push('\n');
        }
        out
    }

    /// Markdown 文本 → 记忆。`fallback_id` 用于文件里没有 id 的情况
    /// （手写的 .md 也要能进库 —— 事实源是文件，不是接口）。
    pub fn from_markdown(text: &str, fallback_id: &str, path: &str) -> Self {
        let (meta, body) = split_frontmatter(text);
        let get = |key: &str| meta_get(&meta, key);
        let now = now_ms();
        let kind = MemoryKind::parse(&get("type").unwrap_or_default());
        let mut entry = MemoryEntry {
            id: get("id").filter(|v| !v.is_empty()).unwrap_or_else(|| fallback_id.to_string()),
            title: get("title").filter(|v| !v.is_empty()).unwrap_or_else(|| {
                // 没有 frontmatter 的手写 .md：第一行往往是 `# 标题`，那它就是标题。
                // 早先这里跳过了以 `#` 开头的行（照抄 `summary` 的口径），结果
                // 「# 标题 / 空行 / 正文」的文件标题变成正文第一段 —— 标题与正文
                // 是两件事，摘要跳过标题是对的，标题不能跳过自己。
                body.lines()
                    .map(str::trim)
                    .find(|line| !line.is_empty())
                    .map(|line| truncate_chars(line.trim_start_matches('#').trim(), 60))
                    .filter(|line| !line.is_empty())
                    .unwrap_or_else(|| "未命名记忆".to_string())
            }),
            kind,
            // 没写 `layer` 时按类别推断：决策/知识/洞见/画像天然是沉淀层。
            // 这比一律当 l1 更准 —— 存量文件里的 `type: decision` 本就该进核心记忆。
            layer: match get("layer") {
                Some(raw) => Layer::parse(&raw),
                None if kind.prefers_l2() => Layer::L2,
                None => Layer::L1,
            },
            project: get("project").filter(|v| !v.is_empty()),
            agent: get("agent").filter(|v| !v.is_empty()),
            device: get("device").filter(|v| !v.is_empty()),
            session: get("session").filter(|v| !v.is_empty()),
            tags: parse_list(get("tags").as_deref()),
            importance: get("importance").and_then(|v| v.parse::<f64>().ok()).unwrap_or(0.5).clamp(0.0, 1.0),
            files: parse_list(get("files").as_deref()),
            supersedes: parse_list(get("supersedes").as_deref()),
            pinned: get("pinned").as_deref() == Some("true"),
            starred: get("starred").as_deref() == Some("true"),
            created_at: get("created_at").and_then(|v| v.parse::<i64>().ok()).unwrap_or(now),
            updated_at: get("updated_at").and_then(|v| v.parse::<i64>().ok()).unwrap_or(now),
            valid_from: get("valid_from").and_then(|v| v.parse::<i64>().ok()).unwrap_or(now),
            valid_to: get("valid_to").and_then(|v| v.parse::<i64>().ok()).filter(|value| *value > 0),
            superseded_by: get("superseded_by").filter(|v| !v.is_empty()),
            deleted_at: get("deleted_at").and_then(|v| v.parse::<i64>().ok()),
            dedup_status: DedupStatus::parse(&get("dedup_status").unwrap_or_default()),
            evidence: parse_list(get("evidence").as_deref()),
            hash: get("hash").filter(|v| !v.is_empty()).unwrap_or_else(|| content_hash(&body)),
            access_count: get("access_count").and_then(|v| v.parse::<u64>().ok()).unwrap_or(0),
            last_hit_at: get("last_hit_at").and_then(|v| v.parse::<i64>().ok()).filter(|value| *value > 0),
            summary_text: get("summary").unwrap_or_default(),
            body,
            path: path.to_string(),
            // 回收站文件里写着它原来待在哪（见 `to_markdown` 的 `origin_path`）；
            // 活条目读不到这一项，就是空串。
            origin_path: get("origin_path").filter(|v| !v.is_empty()).unwrap_or_default(),
        };
        if entry.valid_from == 0 {
            entry.valid_from = entry.created_at;
        }
        entry
    }
}

/// 切出 `---` frontmatter 与正文。没有 frontmatter 时整段都是正文。
fn split_frontmatter(text: &str) -> (Vec<(String, String)>, String) {
    let normalized = text.replace("\r\n", "\n");
    let mut lines = normalized.lines();
    if lines.next().map(str::trim) != Some("---") {
        return (Vec::new(), normalized.trim_start_matches('\n').to_string());
    }
    let mut meta: Vec<(String, String)> = Vec::new();
    let mut closed = false;
    let mut body = String::new();
    for line in lines.by_ref() {
        if line.trim() == "---" {
            closed = true;
            break;
        }
        if let Some((key, value)) = line.split_once(':') {
            let key = key.trim();
            if !key.is_empty() && !key.starts_with('#') {
                meta.push((key.to_string(), value.trim().to_string()));
            }
        }
    }
    if !closed {
        // 只有开头 `---`、没有闭合：当作普通正文，别把整篇吃成 frontmatter
        return (Vec::new(), normalized);
    }
    for line in lines {
        body.push_str(line);
        body.push('\n');
    }
    (meta, body.trim_start_matches('\n').to_string())
}

fn meta_get(meta: &[(String, String)], key: &str) -> Option<String> {
    meta.iter()
        .find(|(name, _)| name == key)
        .map(|(_, value)| unquote(value))
        .filter(|value| !value.is_empty())
}

/// 供 `store` 读「容器头」用的只读切分：返回 `(头部键值, 正文)`。
///
/// 容器（`l1/<agent>/<date>.md`）的头部只有 date/agent/project 三个键，而正文
/// 里每一节又有自己的标记行 —— 逐节解析归 store，这里只负责把「头部」和
/// 「内容」切开，免得 store 再抄一份 frontmatter 解析。
pub fn split_frontmatter_public(text: &str) -> (std::collections::BTreeMap<String, String>, String) {
    let (meta, body) = split_frontmatter(text);
    (meta.into_iter().collect(), body)
}

/// 去掉写出时加的引号、还原转义。空引号串 `""` 视为空值。
fn unquote(raw: &str) -> String {
    let text = raw.trim();
    if text.len() >= 2 && text.starts_with('"') && text.ends_with('"') {
        return text[1..text.len() - 1].replace("\\\"", "\"").replace("\\\\", "\\");
    }
    text.to_string()
}

/// 解析 `[a, "b, c", d]`。逗号分隔、但引号内的逗号不切分
/// （标签里带逗号的写法必须能原样取回，否则「写进去的是 2 个、读出来是 3 个」）。
fn parse_list(raw: Option<&str>) -> Vec<String> {
    let Some(text) = raw else { return Vec::new() };
    let text = text.trim();
    if text.is_empty() {
        return Vec::new();
    }
    let inner = text.strip_prefix('[').and_then(|rest| rest.strip_suffix(']')).unwrap_or(text);
    let mut out = Vec::new();
    let mut current = String::new();
    let mut in_quotes = false;
    let mut escaped = false;
    for ch in inner.chars() {
        match ch {
            '\\' if in_quotes && !escaped => {
                escaped = true;
                current.push(ch);
            }
            '"' if !escaped => {
                in_quotes = !in_quotes;
                current.push(ch);
            }
            ',' if !in_quotes => {
                let item = unquote(current.trim());
                if !item.is_empty() {
                    out.push(item);
                }
                current.clear();
            }
            _ => {
                escaped = false;
                current.push(ch);
            }
        }
    }
    let item = unquote(current.trim());
    if !item.is_empty() {
        out.push(item);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> MemoryEntry {
        let mut entry = MemoryEntry::draft(
            "m1",
            "别用 rm -rf 清缓存",
            MemoryKind::Pitfall,
            "清了 node_modules 之后整机半小时不能工作。\n\n用 npm ci 重装即可。",
        );
        entry.project = Some("agent2api".into());
        entry.agent = Some("manual".into());
        entry.tags = vec!["磁盘".into(), "误删".into()];
        entry.importance = 0.9;
        entry.files = vec!["desktop-tauri/ui/app.js".into()];
        entry.pinned = true;
        entry.created_at = 1_700_000_000_000;
        entry.updated_at = 1_700_000_000_000;
        entry.valid_from = 1_700_000_000_000;
        entry.path = "projects/agent2api/m1.md".into();
        entry
    }

    #[test]
    fn markdown_round_trip_keeps_every_field() {
        let entry = sample();
        let text = entry.to_markdown();
        let back = MemoryEntry::from_markdown(&text, "fallback", &entry.path);
        assert_eq!(back.id, entry.id);
        assert_eq!(back.title, entry.title);
        assert_eq!(back.kind, entry.kind);
        assert_eq!(back.project, entry.project);
        assert_eq!(back.tags, entry.tags);
        assert_eq!(back.files, entry.files);
        assert!(back.pinned);
        assert!((back.importance - 0.9).abs() < 1e-9);
        assert_eq!(back.body.trim(), entry.body.trim());
    }

    /// 新增字段（layer / valid_to / dedup / evidence / 热度）必须能往返。
    #[test]
    fn markdown_round_trip_keeps_new_fields() {
        let mut entry = sample();
        entry.layer = Layer::L2;
        entry.device = Some("dev-01".into());
        entry.session = Some("sess-9".into());
        entry.valid_to = Some(1_800_000_000_000);
        entry.dedup_status = DedupStatus::Merged;
        entry.evidence = vec!["m0".into(), "docs/api.md#a1b2".into()];
        entry.access_count = 7;
        entry.last_hit_at = Some(1_700_000_500_000);
        let text = entry.to_markdown();
        let back = MemoryEntry::from_markdown(&text, "fallback", &entry.path);
        assert_eq!(back.layer, Layer::L2);
        assert_eq!(back.device.as_deref(), Some("dev-01"));
        assert_eq!(back.session.as_deref(), Some("sess-9"));
        assert_eq!(back.valid_to, Some(1_800_000_000_000));
        assert_eq!(back.dedup_status, DedupStatus::Merged);
        assert_eq!(back.evidence, entry.evidence);
        assert_eq!(back.access_count, 7);
        assert_eq!(back.last_hit_at, Some(1_700_000_500_000));
        assert_eq!(back.hash, entry.hash);
    }

    /// `origin_path` 只在回收站文件里出现，且必须能往返 —— 恢复靠它回原处。
    ///
    /// 两条都要测：
    /// ① 进了回收站（`deleted_at` 非空）→ 写出去、读回来都还在；
    /// ② 活着（`deleted_at` 空）→ **不写**这一项。活文件的 `path` 就是它自己，
    ///    写出去是噪音，而且下次保存时它已经不成立了（文件搬走了 key 还在原地）。
    #[test]
    fn origin_path_round_trips_only_when_trashed() {
        let mut entry = sample();
        entry.deleted_at = Some(1_700_000_100_000);
        entry.path = "trash/m1.md".into();
        entry.origin_path = "projects/agent2api/l1/manual/2026-11-19.md".into();
        let text = entry.to_markdown();
        assert!(text.contains("origin_path:"), "回收站文件必须写出 origin_path");
        let back = MemoryEntry::from_markdown(&text, "fallback", &entry.path);
        assert_eq!(back.origin_path, entry.origin_path);

        // 活条目：写了也不该落盘
        let mut live = sample();
        live.origin_path = "projects/agent2api/l1/manual/2026-11-19.md".into();
        let text = live.to_markdown();
        assert!(!text.contains("origin_path:"), "活文件不该写 origin_path");
        let back = MemoryEntry::from_markdown(&text, "fallback", &live.path);
        assert!(back.origin_path.is_empty());
    }

    /// 存量文件（没有 layer / dedup_status / hash）必须照常读得出来，
    /// 而且 `decision` 类要自动落到 l2 —— 这是「磁盘格式只加不减」的底线。
    #[test]
    fn legacy_file_without_new_keys_still_loads() {
        let legacy = "---\nid: m9\ntitle: 老决策\ntype: decision\ntags: []\nimportance: 0.5\ncreated_at: 1\nupdated_at: 1\nvalid_from: 1\n---\n\n正文\n";
        let back = MemoryEntry::from_markdown(legacy, "fallback", "projects/o--r/m9.md");
        assert_eq!(back.id, "m9");
        assert_eq!(back.layer, Layer::L2, "决策类无 layer 时按类别推断");
        assert_eq!(back.dedup_status, DedupStatus::Unique);
        assert!(!back.hash.is_empty(), "hash 缺失时按正文补算");
        assert_eq!(back.access_count, 0);
        assert_eq!(back.valid_to, None);
    }

    #[test]
    fn content_hash_ignores_line_endings_and_trailing_spaces() {
        assert_eq!(content_hash("a\nb"), content_hash("a\r\nb"));
        assert_eq!(content_hash("a\nb"), content_hash("a   \nb\t\n"));
        assert_ne!(content_hash("用 rm"), content_hash("用 RM"));
    }

    #[test]
    fn expired_entry_is_not_live() {
        let mut entry = sample();
        entry.deleted_at = None;
        entry.superseded_by = None;
        assert!(entry.is_live());
        entry.valid_to = Some(now_ms() - 1_000);
        assert!(!entry.is_live());
        entry.valid_to = Some(now_ms() + 1_000_000);
        assert!(entry.is_live());
    }

    #[test]
    fn markdown_without_frontmatter_is_all_body() {
        let back = MemoryEntry::from_markdown("# 标题\n\n正文一段", "auto", "a.md");
        assert_eq!(back.id, "auto");
        assert_eq!(back.title, "标题");
        assert!(back.body.contains("正文一段"));
    }

    #[test]
    fn list_with_comma_inside_quotes_is_one_item() {
        let items = parse_list(Some("[a, \"b, c\", d]"));
        assert_eq!(items, vec!["a".to_string(), "b, c".to_string(), "d".to_string()]);
    }

    #[test]
    fn summary_skips_heading_and_marks() {
        let mut entry = sample();
        entry.body = "# 小标题\n\n- 第一句要点\n第二行".into();
        assert_eq!(entry.summary(50), "第一句要点");
    }

    #[test]
    fn token_estimate_counts_cjk_per_char() {
        assert_eq!(estimate_tokens("记忆中枢"), 4);
        assert!(estimate_tokens("abcdefgh") <= 3);
    }
}
