//! 记忆中枢 · 检索、排序与「三级披露」的文本产出。
//!
//! ── 三级披露是什么，为什么必须有 ────────────────────────────
//! 记忆是天生长尾的：几百条之后，「全都塞给模型」既烧 token 又稀释注意力。
//! 所以按「先看骨架、再要细节」分三级：
//!
//! | 级别 | 内容 | 体量 | 什么时候取 |
//! |---|---|---|---|
//! | `core` | 画像 + 各项目要点 | ≤ 预算（默认约 800 token） | 会话开局一次 |
//! | `digest` | 项目清单 + 最近标题 + 标签 | ≤ 200 行 | 「我得先知道有什么」 |
//! | `search` | 紧凑索引行（标题/类型/项目/时间） | 每行一行 | 拿到线索后定位 |
//! | `get` | 全文 | 按需 | 真要引用原文时 |
//!
//! 界面上的记忆页用同一套函数取数，于是「界面上看到的」与「注入给模型的」
//! 出自同一份代码 —— 不会出现「界面说有 3 条、模型只看到 1 条」的分叉。
//!
//! ── 排序为什么把 importance 与 recency 混进去 ───────────────
//! 纯 bm25 会给「长正文里的偶然命中」高分，而记忆的实际价值在「这条是决策、
//! 当时标了高重要度、而且是最近写的」。三项混排之后，同一批命中里先出的是
//! 「你当时认为重要、且还新鲜」的那几条。

use super::index::{self, IndexRow};
use super::model::{estimate_tokens, truncate_chars, MemoryEntry};

/// 检索条件。字段全空 = 「按时间列一批」。
#[derive(Debug, Default, Clone)]
pub struct Query {
    pub text: String,
    pub project: Option<String>,
    pub agent: Option<String>,
    pub kind: Option<String>,
    pub tag: Option<String>,
    pub starred: Option<bool>,
    pub pinned: Option<bool>,
    /// 包含已被取代的记忆（默认不含：旧结论留着只会误导）
    pub include_superseded: bool,
    pub include_deleted: bool,
    pub limit: usize,
}

fn now_ms() -> i64 {
    crate::server::logging::now_ms()
}

/// 一次检索：取候选 → 过滤 → 打分排序 → 截断。
pub fn search(query: &Query) -> Vec<IndexRow> {
    let mut rows = if query.text.trim().is_empty() {
        index::global().rows(query.include_deleted)
    } else {
        index::global().search(&query.text)
    };
    rows.retain(|row| keep(row, query));
    score_and_sort(&mut rows, &query.text);
    let limit = if query.limit == 0 { 30 } else { query.limit.min(500) };
    rows.truncate(limit);
    rows
}

fn keep(row: &IndexRow, query: &Query) -> bool {
    if row.deleted_at.is_some() && !query.include_deleted {
        return false;
    }
    if row.superseded_by.is_some() && !query.include_superseded {
        return false;
    }
    if let Some(project) = &query.project {
        let want = super::store::slugify(project);
        let hit = row.project.as_deref().map(super::store::slugify).as_deref() == Some(want.as_str());
        // `_global` 也接受「按项目筛」时显式指名它
        if !hit {
            return false;
        }
    }
    if let Some(agent) = &query.agent {
        if row.agent.as_deref() != Some(agent.as_str()) {
            return false;
        }
    }
    if let Some(kind) = &query.kind {
        if !kind.is_empty() && row.kind != *kind {
            return false;
        }
    }
    if let Some(tag) = &query.tag {
        if !tag.is_empty() && !row.tags.iter().any(|item| item.contains(tag.as_str())) {
            return false;
        }
    }
    if let Some(starred) = query.starred {
        if row.starred != starred {
            return false;
        }
    }
    if let Some(pinned) = query.pinned {
        if row.pinned != pinned {
            return false;
        }
    }
    true
}

/// 综合打分：相关度 + 重要度 + 置顶/星标 + 新鲜度。
fn score_and_sort(rows: &mut [IndexRow], text: &str) {
    let now = now_ms();
    let seven_days = 7 * 24 * 3600 * 1000;
    for row in rows.iter_mut() {
        // FTS5 的 bm25 是负分（越小越相关），取反后与 hits 同向
        let relevance = if row.rank != 0.0 { -row.rank } else { row.hits as f64 };
        let recency = if now - row.updated_at < seven_days && row.updated_at > 0 {
            1.0
        } else {
            0.0
        };
        row.rank = relevance
            + row.importance * 2.0
            + if row.pinned { 3.0 } else { 0.0 }
            + if row.starred { 1.0 } else { 0.0 }
            + recency;
    }
    if text.trim().is_empty() {
        // 无关键词：置顶优先、然后按更新时间（`rows()` 已按 updated_at 排好，
        // 这里只把置顶的提上来，稳定排序保住同分区内的顺序）
        rows.sort_by(|a, b| b.pinned.cmp(&a.pinned));
    } else {
        rows.sort_by(|a, b| {
            b.rank
                .partial_cmp(&a.rank)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then(b.updated_at.cmp(&a.updated_at))
        });
    }
}

/// 一行紧凑索引：「[决策] 标题 · 项目 · 3 天前」。
///
/// 为什么要「紧凑行」而不是直接给全文：一次检索返回 10 条全文可能就是 8000
/// token，而模型通常只需要「知道有这几条、叫什么」，真正要引用的再 `get`。
pub fn compact_line(row: &IndexRow, now: i64) -> String {
    let mut parts = Vec::new();
    parts.push(format!("[{}] {}", row.kind, row.title));
    if let Some(project) = &row.project {
        parts.push(project.clone());
    }
    if row.importance >= 0.8 {
        parts.push("重要".to_string());
    }
    if row.pinned {
        parts.push("置顶".to_string());
    }
    if row.superseded_by.is_some() {
        parts.push("已过时".to_string());
    }
    parts.push(relative_time(row.updated_at, now));
    format!("{} · `{}`", parts.join(" · "), row.id)
}

/// 相对时间：今天 / 3 天前 / 2026-11-03。
pub fn relative_time(timestamp: i64, now: i64) -> String {
    if timestamp <= 0 {
        return "未知时间".to_string();
    }
    let delta = now - timestamp;
    if delta < 0 {
        return "刚刚".to_string();
    }
    let minutes = delta / 60_000;
    if minutes < 1 {
        return "刚刚".to_string();
    }
    if minutes < 60 {
        return format!("{minutes} 分钟前");
    }
    let hours = minutes / 60;
    if hours < 24 {
        return format!("{hours} 小时前");
    }
    let days = hours / 24;
    if days < 30 {
        return format!("{days} 天前");
    }
    format_date(timestamp)
}

/// 毫秒时间戳 → `YYYY-MM-DD`（本地时区由 `core::beijing` 统一口径）。
pub fn format_date(timestamp: i64) -> String {
    let seconds = timestamp / 1000;
    let days = seconds.div_euclid(86_400);
    // 从 1970-01-01 起算的民用历换算（Howard Hinnant 的 days_from_civil 逆运算）
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = if m <= 2 { y + 1 } else { y };
    format!("{year:04}-{m:02}-{d:02}")
}

/// 核心记忆（三级披露第一级）。
///
/// 结构：画像条目 → 按项目的「最重要 + 最近」几条要点。预算按 token 估算逐步
/// 收缩，超出就丢最不重要的行（不是硬截断半句话 —— 半句话对模型是噪音）。
pub fn core_text(entries: &[MemoryEntry], project: Option<&str>, max_tokens: usize) -> (String, usize, bool) {
    let now = now_ms();
    let budget = max_tokens.clamp(200, 4000);
    let mut lines: Vec<(f64, String)> = Vec::new();

    for entry in entries.iter().filter(|entry| entry.is_live()) {
        let is_profile = matches!(
            entry.kind,
            super::model::MemoryKind::Profile | super::model::MemoryKind::Preference
        );
        if let Some(want) = project {
            let want = super::store::slugify(want);
            let same = entry.project.as_deref().map(super::store::slugify).as_deref() == Some(want.as_str());
            if !same && !is_profile {
                continue;
            }
        }
        let text = format!(
            "- [{}] {} — {}（{}）",
            entry.kind.label(),
            truncate_chars(&entry.title, 60),
            truncate_chars(&entry.summary(120), 120),
            relative_time(entry.updated_at, now),
        );
        let weight = entry.importance
            + if entry.pinned { 0.6 } else { 0.0 }
            + if is_profile { 0.3 } else { 0.0 };
        lines.push((weight, text));
    }

    lines.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));
    let mut out = String::new();
    out.push_str("# 长期记忆（会话开局可读）\n");
    out.push_str("以下是此前沉淀的事实与决策，供参考；与本轮对话冲突时以本轮为准。\n");
    let mut used = estimate_tokens(&out);
    let mut omitted = 0usize;
    for (_, line) in lines {
        let cost = estimate_tokens(&line) + 1;
        if used + cost > budget {
            omitted += 1;
            continue;
        }
        out.push_str(&line);
        out.push('\n');
        used += cost;
    }
    if omitted > 0 {
        out.push_str(&format!(
            "\n（另有 {omitted} 条未展开：需要时用按需检索）\n"
        ));
    }
    (out, used, omitted > 0)
}

/// 索引概览（三级披露第二级）：有哪些项目、各多少条、最近写了什么、常见标签。
pub fn digest_text(entries: &[MemoryEntry], max_lines: usize) -> String {
    let now = now_ms();
    let limit = if max_lines == 0 { 200 } else { max_lines.min(1000) };
    let live: Vec<&MemoryEntry> = entries.iter().filter(|entry| entry.is_live()).collect();
    let mut out = String::new();
    out.push_str(&format!("# 记忆索引（{} 条有效）\n", live.len()));

    // 项目分组
    let mut by_project: std::collections::BTreeMap<String, Vec<&MemoryEntry>> = Default::default();
    for entry in &live {
        let key = entry
            .project
            .clone()
            .unwrap_or_else(|| super::store::GLOBAL_SLUG.to_string());
        by_project.entry(key).or_default().push(entry);
    }
    let mut lines = 0usize;
    for (project, items) in &by_project {
        if lines >= limit {
            out.push_str("…（已截断）\n");
            break;
        }
        out.push_str(&format!("\n## {}（{} 条）\n", project, items.len()));
        lines += 2;
        let mut sorted: Vec<&&MemoryEntry> = items.iter().collect();
        sorted.sort_by(|a, b| b.updated_at.cmp(&a.updated_at));
        for entry in sorted.into_iter().take(5) {
            if lines >= limit {
                break;
            }
            out.push_str(&format!(
                "- [{}] {}（{}）\n",
                entry.kind.label(),
                truncate_chars(&entry.title, 50),
                relative_time(entry.updated_at, now)
            ));
            lines += 1;
        }
    }

    // 标签云：出现 2 次以上的前 20 个
    let mut tag_counts: std::collections::BTreeMap<&str, usize> = Default::default();
    for entry in &live {
        for tag in &entry.tags {
            *tag_counts.entry(tag.as_str()).or_default() += 1;
        }
    }
    let mut tags: Vec<(&str, usize)> = tag_counts.into_iter().filter(|(_, count)| *count >= 2).collect();
    tags.sort_by(|a, b| b.1.cmp(&a.1));
    if !tags.is_empty() {
        out.push_str("\n## 常见标签\n");
        let joined: Vec<String> = tags
            .into_iter()
            .take(20)
            .map(|(tag, count)| format!("{tag}({count})"))
            .collect();
        out.push_str(&joined.join("、"));
        out.push('\n');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn relative_time_buckets() {
        let now = 1_700_000_000_000;
        assert_eq!(relative_time(now - 30_000, now), "刚刚");
        assert_eq!(relative_time(now - 5 * 60_000, now), "5 分钟前");
        assert_eq!(relative_time(now - 3 * 3_600_000, now), "3 小时前");
        assert_eq!(relative_time(now - 2 * 86_400_000, now), "2 天前");
    }

    #[test]
    fn format_date_is_civil() {
        // 纪元零点必须是 1970-01-01（换算公式的边界；写死一个随便挑的时间戳
        // 只会变成「测试和实现一起错」）
        assert_eq!(format_date(0), "1970-01-01");
        assert_eq!(format_date(86_400_000), "1970-01-02");
    }

    #[test]
    fn core_text_respects_budget() {
        let mut entries = Vec::new();
        for index in 0..50 {
            entries.push(super::super::model::MemoryEntry {
                id: format!("m{index}"),
                title: format!("第 {index} 条很长的标题用于测试预算收缩"),
                kind: super::super::model::MemoryKind::Knowledge,
                project: Some("demo".into()),
                agent: None,
                tags: vec![],
                importance: 0.5,
                files: vec![],
                supersedes: vec![],
                pinned: false,
                starred: false,
                created_at: 1,
                updated_at: 1,
                valid_from: 1,
                superseded_by: None,
                deleted_at: None,
                body: "正文".into(),
                path: String::new(),
            });
        }
        let (text, used, truncated) = core_text(&entries, None, 200);
        assert!(used <= 210, "used={used}");
        assert!(truncated);
        assert!(text.contains("未展开"));
    }
}
