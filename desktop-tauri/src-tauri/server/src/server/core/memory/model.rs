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
            MemoryKind::Other => "其他",
        }
    }

    pub const ALL: [MemoryKind; 7] = [
        MemoryKind::Daily,
        MemoryKind::Decision,
        MemoryKind::Knowledge,
        MemoryKind::Preference,
        MemoryKind::Pitfall,
        MemoryKind::Profile,
        MemoryKind::Other,
    ];
}

/// 一条记忆。字段与 frontmatter 一一对应（`type` 因为与 Rust 关键字冲突，
/// 在结构体里叫 `kind`，落到文件上仍是 `type`）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemoryEntry {
    pub id: String,
    pub title: String,
    #[serde(rename = "type")]
    pub kind: MemoryKind,
    /// 项目 slug。`None` = 不属于任何项目（全局记忆）
    pub project: Option<String>,
    /// 写入方（agent 名 / `manual` / `import`）
    pub agent: Option<String>,
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
    /// 被谁取代（由 `supersedes` 的反向维护写入）
    pub superseded_by: Option<String>,
    /// 回收站时间。`Some` = 已删除但可恢复
    pub deleted_at: Option<i64>,
    /// 正文（frontmatter 之后的部分）。索引里存一份用于检索，源仍在 .md
    pub body: String,
    /// 文件相对路径（相对 memory 根）。恢复/重写时按它定位
    pub path: String,
}

impl MemoryEntry {
    /// 摘要：正文第一段非空文本，压到 `limit` 字符。索引与「紧凑索引行」用它。
    pub fn summary(&self, limit: usize) -> String {
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
        format!(
            "{}\n{}\n{}\n{}",
            self.title,
            self.summary(240),
            self.tags_text(),
            self.body
        )
    }

    /// 该记忆是否仍然有效（未被取代、未进回收站）
    pub fn is_live(&self) -> bool {
        self.superseded_by.is_none() && self.deleted_at.is_none()
    }

    /// 接口响应形状。正文按需附带（列表不带全文，`get` 才带）
    pub fn to_json(&self, with_body: bool) -> Value {
        let mut value = json!({
            "id": self.id,
            "title": self.title,
            "type": self.kind.as_str(),
            "typeLabel": self.kind.label(),
            "project": self.project,
            "agent": self.agent,
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
            "deletedAt": self.deleted_at,
            "summary": self.summary(180),
            "chars": self.body.chars().count(),
            "path": self.path,
        });
        if with_body {
            value["body"] = Value::String(self.body.clone());
        }
        value
    }
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
        let mut out = String::with_capacity(self.body.len() + 512);
        out.push_str("---\n");
        out.push_str(&format!("id: {}\n", yaml_scalar(&self.id)));
        out.push_str(&format!("title: {}\n", yaml_scalar(&self.title)));
        out.push_str(&format!("type: {}\n", self.kind.as_str()));
        if let Some(project) = &self.project {
            out.push_str(&format!("project: {}\n", yaml_scalar(project)));
        }
        if let Some(agent) = &self.agent {
            out.push_str(&format!("agent: {}\n", yaml_scalar(agent)));
        }
        out.push_str(&format!("tags: {}\n", yaml_list(&self.tags)));
        out.push_str(&format!("importance: {}\n", self.importance));
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
        if let Some(deleted) = self.deleted_at {
            out.push_str(&format!("deleted_at: {deleted}\n"));
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
        let now = crate::server::logging::now_ms();
        let kind = MemoryKind::parse(&get("type").unwrap_or_default());
        let mut entry = MemoryEntry {
            id: get("id").filter(|v| !v.is_empty()).unwrap_or_else(|| fallback_id.to_string()),
            title: get("title").filter(|v| !v.is_empty()).unwrap_or_else(|| {
                body.lines()
                    .map(str::trim)
                    .find(|line| !line.is_empty() && !line.starts_with('#'))
                    .map(|line| truncate_chars(line, 60))
                    .unwrap_or_else(|| "未命名记忆".to_string())
            }),
            kind,
            project: get("project").filter(|v| !v.is_empty()),
            agent: get("agent").filter(|v| !v.is_empty()),
            tags: parse_list(get("tags").as_deref()),
            importance: get("importance").and_then(|v| v.parse::<f64>().ok()).unwrap_or(0.5).clamp(0.0, 1.0),
            files: parse_list(get("files").as_deref()),
            supersedes: parse_list(get("supersedes").as_deref()),
            pinned: get("pinned").as_deref() == Some("true"),
            starred: get("starred").as_deref() == Some("true"),
            created_at: get("created_at").and_then(|v| v.parse::<i64>().ok()).unwrap_or(now),
            updated_at: get("updated_at").and_then(|v| v.parse::<i64>().ok()).unwrap_or(now),
            valid_from: get("valid_from").and_then(|v| v.parse::<i64>().ok()).unwrap_or(now),
            superseded_by: get("superseded_by").filter(|v| !v.is_empty()),
            deleted_at: get("deleted_at").and_then(|v| v.parse::<i64>().ok()),
            body,
            path: path.to_string(),
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
        MemoryEntry {
            id: "m1".into(),
            title: "别用 rm -rf 清缓存".into(),
            kind: MemoryKind::Pitfall,
            project: Some("agent2api".into()),
            agent: Some("manual".into()),
            tags: vec!["磁盘".into(), "误删".into()],
            importance: 0.9,
            files: vec!["desktop-tauri/ui/app.js".into()],
            supersedes: vec![],
            pinned: true,
            starred: false,
            created_at: 1_700_000_000_000,
            updated_at: 1_700_000_000_000,
            valid_from: 1_700_000_000_000,
            superseded_by: None,
            deleted_at: None,
            body: "清了 node_modules 之后整机半小时不能工作。\n\n用 npm ci 重装即可。".into(),
            path: "projects/agent2api/m1.md".into(),
        }
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
