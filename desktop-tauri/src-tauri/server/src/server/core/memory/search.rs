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
//! ── 排序为什么是七项混排，而不是纯 bm25 ─────────────────────
//! 纯 bm25 会给「长正文里的偶然命中」高分，而记忆的实际价值在「这条是决策、
//! 当时标了高重要度、属于我正在做的项目、而且是最近写的、还被反复查过」。
//! 所以七项各自归一化到 0..1 后加权求和（权重来自 `config`，默认值见
//! `config::SCHEMA` 的 `search.weight*`）：
//!
//! | 项 | 归一口径 |
//! |---|---|
//! | `bm25` | `-rank` 除以本批最大值；退化模式用命中词数 |
//! | `recency` | 按 `search.timeDecayHalfLife` 做半衰期衰减 |
//! | `importance` | 原值（0..1） |
//! | `affinity` | 项目匹配 + 同 agent 的加成 |
//! | `graph` | `mem_link` 里与本批命中相连的条目（一次性扩散） |
//! | `heat` | `access_count` 的对数尺度 + `last_hit_at` 衰减 |
//! | `layer` | `l2` 得满分，`l1` 得 0（结论压过流水） |
//!
//! 置顶与星标不占权重位：它们改成**硬排序前缀**（置顶 > 星标 > 其余），
//! 因为「用户手动钉住」是明确意图，混进加权里会被一堆小项稀释掉。

use std::collections::HashSet;

use super::config;
use super::index::{self, IndexRow};
use super::model::{estimate_tokens, truncate_chars, MemoryEntry, MemoryKind};

/// 检索条件。字段全空 = 「按时间列一批」。
#[derive(Debug, Default, Clone)]
pub struct Query {
    pub text: String,
    pub project: Option<String>,
    pub agent: Option<String>,
    pub kind: Option<String>,
    /// `l1` / `l2`。空 = 不限层
    pub layer: Option<String>,
    pub tag: Option<String>,
    pub starred: Option<bool>,
    pub pinned: Option<bool>,
    /// 包含已被取代的记忆（默认不含：旧结论留着只会误导）
    pub include_superseded: bool,
    pub include_deleted: bool,
    /// 包含已过有效期（`valid_to`）的记忆
    pub include_expired: bool,
    pub limit: usize,
}

fn now_ms() -> i64 {
    crate::server::logging::now_ms()
}

/// 一次检索：取候选 → 过滤 → 打分排序 → 截断 → 记热度。
pub fn search(query: &Query) -> Vec<IndexRow> {
    let mut rows = if query.text.trim().is_empty() {
        index::global().rows(query.include_deleted)
    } else {
        index::global().search(&query.text)
    };
    rows.retain(|row| keep(row, query));
    score_and_sort(&mut rows, query);
    let limit = if query.limit == 0 { 30 } else { query.limit.min(500) };
    rows.truncate(limit);
    // 热度只记「关键词检索」的命中：无关键词的列表浏览不算「用户要看这条」
    if !query.text.trim().is_empty() {
        let ids: Vec<String> = rows.iter().map(|row| row.id.clone()).collect();
        index::global().hit(&ids);
    }
    rows
}

fn keep(row: &IndexRow, query: &Query) -> bool {
    if row.deleted_at.is_some() && !query.include_deleted {
        return false;
    }
    if row.superseded_by.is_some() && !query.include_superseded {
        return false;
    }
    if !query.include_expired {
        if let Some(until) = row.valid_to {
            if until > 0 && now_ms() > until {
                return false;
            }
        }
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
    if let Some(layer) = &query.layer {
        if !layer.is_empty() && row.layer != *layer {
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

/// 七项混排。权重从配置读，缺省值保证「不配也合理」。
fn score_and_sort(rows: &mut [IndexRow], query: &Query) {
    let now = now_ms();
    let half_life_days = config::number("search.timeDecayHalfLife");
    let w_bm25 = config::number("search.weightBm25");
    let w_recency = config::number("search.weightRecency");
    let w_importance = config::number("search.weightImportance");
    let w_affinity = config::number("search.weightAffinity");
    let w_graph = config::number("search.weightGraph");
    let w_heat = config::number("search.weightHeat");
    let w_layer = config::number("search.weightLayer");

    // bm25 归一需要先拿到本批最大值：绝对分值随查询长度变化，不归一就不可比
    let max_relevance = rows
        .iter()
        .map(|row| if row.rank != 0.0 { -row.rank } else { row.hits as f64 })
        .fold(0.0f64, f64::max);

    // 图扩散：本批已命中的 id 出发，一跳邻居额外加分（只做一层，不做迭代传播）
    let seeds: HashSet<&str> = rows.iter().map(|row| row.id.as_str()).collect();
    let graph_boost: HashSet<String> = if w_graph > 0.0 {
        index::global()
            .neighbors(&seeds.iter().map(|id| id.to_string()).collect::<Vec<_>>())
            .into_iter()
            .filter(|id| !seeds.contains(id.as_str()))
            .collect()
    } else {
        HashSet::new()
    };

    let current_project = query.project.as_deref().map(super::store::slugify);
    let current_agent = query.agent.as_deref();

    for row in rows.iter_mut() {
        let relevance = if row.rank != 0.0 { -row.rank } else { row.hits as f64 };
        let bm25 = if max_relevance > 0.0 {
            (relevance / max_relevance).clamp(0.0, 1.0)
        } else {
            0.0
        };
        let recency = if half_life_days <= 0.0 {
            // 半衰期 0 = 不衰减，全部记满分（让其它项决定排序）
            1.0
        } else if row.updated_at <= 0 {
            0.0
        } else {
            let age_days = ((now - row.updated_at).max(0) as f64) / 86_400_000.0;
            // 0.5^(age/halfLife)：标准半衰期衰减
            (0.5f64).powf(age_days / half_life_days)
        };
        let affinity = {
            let mut score = 0.0;
            if let (Some(want), Some(project)) = (&current_project, &row.project) {
                if super::store::slugify(project) == *want {
                    score += 0.7;
                }
            }
            if let (Some(want), Some(agent)) = (current_agent, row.agent.as_deref()) {
                if agent == want {
                    score += 0.3;
                }
            }
            score.min(1.0)
        };
        let graph = if graph_boost.contains(&row.id) { 1.0 } else { 0.0 };
        let heat = {
            if row.access_count <= 0 {
                0.0
            } else {
                // log 尺度：1 次 ≈ 0.34，10 次 ≈ 0.67，100 次 ≈ 1.0。
                // 线性会让「被机器批量读过 100 次」的条目永久霸榜
                let frequency = (1.0 + row.access_count as f64).ln() / 11.0_f64.ln();
                let freshness = match row.last_hit_at {
                    Some(at) if half_life_days > 0.0 => {
                        let age_days = ((now - at).max(0) as f64) / 86_400_000.0;
                        (0.5f64).powf(age_days / half_life_days)
                    }
                    Some(_) => 1.0,
                    None => 0.5,
                };
                (frequency * freshness).clamp(0.0, 1.0)
            }
        };
        let layer = if row.layer == "l2" { 1.0 } else { 0.0 };

        row.rank = bm25 * w_bm25
            + recency * w_recency
            + row.importance * w_importance
            + affinity * w_affinity
            + graph * w_graph
            + heat * w_heat
            + layer * w_layer;
    }

    // 置顶 / 星标 / 过期 作为硬前缀：明确意图不该被加权稀释。
    // 有置顶时置顶排前面；同等条件下星标次之；已过期（用户显式要求包含时）沉底。
    if query.text.trim().is_empty() {
        // 无关键词：置顶优先，其余保持 `rows()` 给的时间序（稳定排序）
        rows.sort_by(|a, b| b.pinned.cmp(&a.pinned).then(b.starred.cmp(&a.starred)));
    } else {
        rows.sort_by(|a, b| {
            b.pinned
                .cmp(&a.pinned)
                .then(b.starred.cmp(&a.starred))
                .then(
                    b.rank
                        .partial_cmp(&a.rank)
                        .unwrap_or(std::cmp::Ordering::Equal),
                )
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
    parts.push(format!("[{}] {}", kind_label(&row.kind), row.title));
    if let Some(project) = &row.project {
        parts.push(project.clone());
    }
    if row.importance >= 0.8 {
        parts.push("重要".to_string());
    }
    if row.pinned {
        parts.push("置顶".to_string());
    }
    if row.starred {
        parts.push("星标".to_string());
    }
    if row.layer == "l2" {
        parts.push("沉淀".to_string());
    }
    if row.superseded_by.is_some() {
        parts.push("已过时".to_string());
    } else if matches!(row.valid_to, Some(until) if until > 0 && now > until) {
        parts.push("已失效".to_string());
    }
    parts.push(relative_time(row.updated_at, now));
    format!("{} · `{}`", parts.join(" · "), row.id)
}

/// 索引里存的是 `type` 字符串，界面上要中文。走 `MemoryKind` 保持单一来源。
fn kind_label(kind: &str) -> &'static str {
    MemoryKind::parse(kind).label()
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
///
/// 只喂 `l2`（沉淀结论）+ 画像 + 置顶：`l1` 是原始流水，把它塞进 800 token
/// 的核心预算里，等于用「今天聊了啥」把「长期结论」挤出去。
pub fn core_text(entries: &[MemoryEntry], project: Option<&str>, max_tokens: usize) -> (String, usize, bool) {
    let now = now_ms();
    let budget = max_tokens.clamp(200, 4000);
    let mut lines: Vec<(f64, String)> = Vec::new();

    for entry in entries.iter().filter(|entry| entry.is_live()) {
        let is_profile = matches!(
            entry.kind,
            MemoryKind::Profile | MemoryKind::Preference
        );
        // l1 且非画像、非置顶 → 不进核心记忆
        if entry.layer != super::model::Layer::L2 && !is_profile && !entry.pinned {
            continue;
        }
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
            + if is_profile { 0.3 } else { 0.0 }
            + if entry.layer == super::model::Layer::L2 { 0.2 } else { 0.0 };
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
        let name = if project == super::store::GLOBAL_SLUG { "通用" } else { project.as_str() };
        out.push_str(&format!("\n## {name}（{} 条）\n", items.len()));
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
            let mut entry = MemoryEntry::draft(
                format!("m{index}"),
                format!("第 {index} 条很长的标题用于测试预算收缩"),
                MemoryKind::Knowledge,
                "正文",
            );
            entry.project = Some("demo".into());
            entry.layer = super::super::model::Layer::L2;
            entry.created_at = 1;
            entry.updated_at = 1;
            entry.valid_from = 1;
            entries.push(entry);
        }
        let (text, used, truncated) = core_text(&entries, None, 200);
        assert!(used <= 210, "used={used}");
        assert!(truncated);
        assert!(text.contains("未展开"));
    }

    /// l1 流水不该进核心记忆（否则 800 token 会被「今天聊了啥」占满）。
    #[test]
    fn core_text_skips_l1_noise() {
        let mut daily = MemoryEntry::draft("d1", "今天调了半小时端口", MemoryKind::Daily, "流水");
        daily.layer = super::super::model::Layer::L1;
        let mut decision = MemoryEntry::draft("d2", "端口固定 3065", MemoryKind::Decision, "结论");
        decision.layer = super::super::model::Layer::L2;
        let (text, _, _) = core_text(&[daily, decision], None, 800);
        assert!(text.contains("端口固定"), "结论必须在核心记忆里");
        assert!(!text.contains("调了半小时端口"), "l1 流水不该进核心记忆");
    }

    #[test]
    fn compact_line_marks_layer_and_expiry() {
        let now = 1_700_000_000_000;
        let row = IndexRow {
            id: "m1".into(),
            project: Some("demo".into()),
            agent: None,
            kind: "decision".into(),
            layer: "l2".into(),
            title: "用 rustls".into(),
            summary: String::new(),
            tags: vec![],
            importance: 0.9,
            pinned: false,
            starred: false,
            created_at: now,
            updated_at: now,
            valid_to: Some(now - 1),
            superseded_by: None,
            deleted_at: None,
            dedup_status: "unique".into(),
            access_count: 0,
            last_hit_at: None,
            path: String::new(),
            rank: 0.0,
            hits: 0,
        };
        let line = compact_line(&row, now);
        assert!(line.contains("决策"), "类型要显示中文：{line}");
        assert!(line.contains("沉淀"), "l2 要标出来：{line}");
        assert!(line.contains("已失效"), "过期要标出来：{line}");
    }
}