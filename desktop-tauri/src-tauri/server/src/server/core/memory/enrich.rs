//! 记忆增强任务：摘要、标签、蒸馏、失效判定、画像。
//!
//! ── 这一层为什么单独存在 ────────────────────────────────────
//! `llm` 只管「把一段话发给模型、拿回一段话」；怎么问、问完怎么落到记忆里，
//! 是每个任务各不相同的业务。混在一起会让 `llm.rs` 变成一个知道所有 prompt 的
//! 大文件，而 prompt 是这一层里最常改的东西。
//!
//! ── 统一的失败策略：不制造坏数据 ────────────────────────────
//! 每条任务都遵守同一套规矩：
//!   · 模型不可用（没配 Key / 预算用完）→ 返回 [`Outcome::Skipped`]，什么都不写；
//!   · 模型给的 JSON 解析不了 → 同样跳过。**绝不用正则从自由文本里抠结论** ——
//!     抠错一条会被当成事实长期留在记忆里，比没生成糟得多；
//!   · 自动应用与否由 `review.autoConfirm*` 决定，默认全关，只入待确认队列。
//!
//! ── 为什么不覆盖原文 ────────────────────────────────────────
//! 摘要写进「新增的一条 `l2 / insight`」，原文一个字都不动。记忆是历史陈述，
//! 改写历史等于伪造证据链。用户想看生成物就去看新条目，不想看就删掉它。

use serde_json::{json, Value};

use super::llm::{self, LlmError, Request};
use super::model::Layer;
use super::{config, index, model, search, store, MemoryKind};

/// 任务结果。区分「跳过」与「失败」：调度器据此决定要不要重试。
pub enum Outcome {
    /// 真的做了（可能产出了建议或新条目）
    Done(Value),
    /// 条件不满足（没模型、没素材、预算用完），不是错误
    Skipped(String),
    Failed(String),
}

impl Outcome {
    pub fn to_json(&self) -> Value {
        match self {
            Self::Done(payload) => json!({ "status": "done", "result": payload }),
            Self::Skipped(reason) => json!({ "status": "skipped", "reason": reason }),
            Self::Failed(error) => json!({ "status": "failed", "error": error }),
        }
    }

    pub fn is_failed(&self) -> bool {
        matches!(self, Self::Failed(_))
    }

    pub fn label(&self) -> &'static str {
        match self {
            Self::Done(_) => "done",
            Self::Skipped(_) => "skipped",
            Self::Failed(_) => "failed",
        }
    }
}

fn from_llm(error: LlmError) -> Outcome {
    if error.is_unavailable() {
        Outcome::Skipped(error.message().to_string())
    } else {
        Outcome::Failed(error.message().to_string())
    }
}

const SYSTEM_EXTRACT: &str = "You maintain a local project memory store. \
Summarise raw work logs into durable conclusions. \
Only state things the log actually supports; never invent file names, APIs or numbers. \
Answer in the same language as the input. Output strict JSON, no prose around it.";

const SYSTEM_TAG: &str = "You tag engineering notes for retrieval. \
Prefer the vocabulary the text already uses over synonyms you would invent. \
Output strict JSON, no prose around it.";

const SYSTEM_PROFILE: &str = "You build a long-term profile of one engineer from their own memory entries. \
Every statement must be supported by the entries you were given, and you must cite the entry ids. \
Never speculate about personality from a single event. Output strict JSON, no prose around it.";

const SYSTEM_SUPERSEDE: &str = "You compare a new memory entry against older entries on the same topic. \
Decide only whether the new entry makes an older one factually obsolete. \
A different angle or an extra detail is not obsolescence. Output strict JSON, no prose around it.";

/// 把一批 l1 流水蒸馏成 l2 结论。
///
/// `project` 为空表示「所有项目的流水一起看」——不推荐，但允许（第一次使用时
/// 用户往往还没分类）。产物是**待确认队列里的建议**，不是直接写入的条目：
/// 蒸馏是这批任务里唯一会「从原文派生出新结论」的一步，它最需要人看一眼。
pub fn distill(project: Option<&str>, limit: usize) -> Outcome {
    if !config::bool_of("deep.enabled") {
        return Outcome::Skipped("深层记忆已关闭（deep.enabled）".into());
    }
    let cap = if limit == 0 { config::usize_of("deep.distillMaxPerProject").max(20) } else { limit };
    let mut source: Vec<model::MemoryEntry> = store::load_all()
        .into_iter()
        .filter(|entry| entry.is_live() && entry.layer == Layer::L1)
        .filter(|entry| match project {
            Some(slug) => entry.project.as_deref() == Some(slug),
            None => true,
        })
        .collect();
    if source.len() < 5 {
        return Outcome::Skipped(format!("L1 素材只有 {} 条，至少需要 5 条", source.len()));
    }
    // 重要度 + 新近度排序后截断：一次蒸馏喂给模型的条数是有限的，
    // 挑「重要且新」的比挑「最早的」有用
    source.sort_by(|left, right| {
        let score = |entry: &model::MemoryEntry| entry.importance * 0.6 + recency_score(entry.updated_at) * 0.4;
        score(right).partial_cmp(&score(left)).unwrap_or(std::cmp::Ordering::Equal)
    });
    source.truncate(cap.min(200));

    let material = source
        .iter()
        .map(|entry| {
            format!(
                "[{}] {} · {} · {}\n{}",
                entry.id,
                entry.title,
                entry.kind.label(),
                search::format_date(entry.updated_at),
                model::truncate_chars(entry.body.trim(), 700)
            )
        })
        .collect::<Vec<_>>()
        .join("\n\n---\n\n");

    let ask = format!(
        "以下是某个项目最近的工作流水，请你把它们蒸馏成 3-8 条长期结论。\n\
         每条结论落在 decision（做过什么选择及理由）/ knowledge（外部事实与约束）/ \
         insight（可跨项目复用的规律）/ pitfall（踩过的坑）之一。\n\
         只写仍然成立的东西；被后来推翻的内容不要写。\n\n\
         输出 JSON：{{\"items\":[{{\"title\":\"不超过 40 字\",\"type\":\"decision\",\"body\":\"100-300 字，含理由与取舍\",\"importance\":0.0-1.0,\"evidence\":[\"引用到的条目 id\"]}}]}}\n\n\
         素材：\n{material}"
    );
    let reply = match llm::complete(Request::new("distill", SYSTEM_EXTRACT, ask)) {
        Ok(reply) => reply,
        Err(error) => return from_llm(error),
    };
    let Some(parsed) = llm::parse_json(&reply.text) else {
        return Outcome::Failed("蒸馏结果不是合法 JSON，已丢弃（不猜结论）".into());
    };
    let items = parsed.get("items").and_then(Value::as_array).cloned().unwrap_or_default();
    let mut queued = 0usize;
    for item in items.iter().take(20) {
        let body = item.get("body").and_then(Value::as_str).unwrap_or("").trim();
        if body.chars().count() < 12 {
            continue;
        }
        let title = item.get("title").and_then(Value::as_str).unwrap_or("").trim().to_string();
        let kind = MemoryKind::parse(item.get("type").and_then(Value::as_str).unwrap_or("insight"));
        let evidence: Vec<String> = item
            .get("evidence")
            .and_then(Value::as_array)
            .map(|items| items.iter().filter_map(|id| id.as_str().map(str::to_string)).collect())
            .unwrap_or_default();
        // 证据链要求：`deep.evidenceRequired` 开时，无引用的结论不入队 ——
        // 一条说不出出处的「结论」正是最容易被模型编出来的东西
        if config::bool_of("deep.evidenceRequired") && evidence.is_empty() {
            continue;
        }
        let importance = item.get("importance").and_then(Value::as_f64).unwrap_or(0.6).clamp(0.0, 1.0);
        // target 用「项目 + 内容哈希」：同一份素材重复蒸馏不会把队列刷满
        let target = format!(
            "{}:{}",
            project.unwrap_or("general"),
            &model::content_hash(body)[..12]
        );
        index::global().enqueue(
            "distill",
            &target,
            &json!({
                "title": title,
                "type": kind.as_str(),
                "body": body,
                "importance": importance,
                "evidence": evidence,
                "project": project,
                "model": reply.model,
            }),
            importance,
        );
        queued += 1;
    }
    if queued == 0 {
        return Outcome::Skipped("模型没给出满足条件的结论（可能都缺证据链）".into());
    }
    Outcome::Done(json!({
        "queued": queued,
        "material": source.len(),
        "model": reply.model,
        "tokens": reply.prompt_tokens + reply.completion_tokens,
    }))
}

/// 给一条记忆补标签（只补空标签的条目，或强制全部）。
pub fn tag(ids: &[String], force: bool, limit: usize) -> Outcome {
    let mut targets: Vec<model::MemoryEntry> = store::load_all()
        .into_iter()
        .filter(|entry| entry.is_live())
        .filter(|entry| ids.is_empty() || ids.contains(&entry.id))
        .filter(|entry| force || entry.tags.len() < 3)
        .collect();
    if targets.is_empty() {
        return Outcome::Skipped("没有需要打标签的记忆".into());
    }
    targets.sort_by_key(|entry| std::cmp::Reverse(entry.updated_at));
    targets.truncate(limit.clamp(1, 20));

    let ask = format!(
        "为下面每条记忆给出 2-5 个检索标签（中文优先，专有名词保留原文）。\n\
         标签要能在「以后想找这条」时被想到，不要写「代码」「项目」这种无信息量的词。\n\n\
         输出 JSON：{{\"items\":[{{\"id\":\"条目 id\",\"tags\":[\"标签\"]}}]}}\n\n\
         条目：\n{}",
        targets
            .iter()
            .map(|entry| format!("[{}] {}\n{}", entry.id, entry.title, model::truncate_chars(&entry.body, 400)))
            .collect::<Vec<_>>()
            .join("\n\n")
    );
    let reply = match llm::complete(Request::new("tag", SYSTEM_TAG, ask)) {
        Ok(reply) => reply,
        Err(error) => return from_llm(error),
    };
    let Some(parsed) = llm::parse_json(&reply.text) else {
        return Outcome::Failed("打标签结果不是合法 JSON，已丢弃".into());
    };
    let mut touched = 0usize;
    for item in parsed.get("items").and_then(Value::as_array).into_iter().flatten() {
        let Some(id) = item.get("id").and_then(Value::as_str) else { continue };
        let tags: Vec<String> = item
            .get("tags")
            .and_then(Value::as_array)
            .map(|items| items.iter().filter_map(|tag| tag.as_str().map(|text| text.trim().to_string())).collect())
            .unwrap_or_default();
        if tags.is_empty() {
            continue;
        }
        let Some(mut entry) = targets.iter().find(|entry| entry.id == id).cloned() else { continue };
        let before = entry.tags.len();
        for tag in tags {
            if tag.is_empty() || entry.tags.contains(&tag) {
                continue;
            }
            entry.tags.push(tag);
        }
        entry.tags.truncate(8);
        if entry.tags.len() == before {
            continue;
        }
        entry.updated_at = crate::server::logging::now_ms();
        entry.path = store::rel(&store::entry_path(&entry, false));
        if store::save(&entry).is_err() {
            continue;
        }
        let _ = index::global().upsert(&entry);
        touched += 1;
    }
    if touched == 0 {
        return Outcome::Skipped("模型没给出可用标签".into());
    }
    crate::server::logging::log("[Memory]", &format!("自动打标签：{touched} 条"));
    Outcome::Done(json!({ "tagged": touched, "model": reply.model }))
}

/// 给一条 l1 记忆生成一句摘要，落到 `summary` 字段（供检索行显示）。
///
/// 不新建条目：摘要是「这条记忆自己的一句话」，属于原条目的一个字段。
pub fn summarize(ids: &[String], limit: usize) -> Outcome {
    let targets: Vec<model::MemoryEntry> = store::load_all()
        .into_iter()
        .filter(|entry| entry.is_live() && entry.body.chars().count() >= 200)
        .filter(|entry| ids.is_empty() || ids.contains(&entry.id))
        .filter(|entry| entry.summary_text.trim().is_empty() || ids.contains(&entry.id))
        .into_iter()
        .take(limit.clamp(1, 20))
        .collect();
    if targets.is_empty() {
        return Outcome::Skipped("没有需要摘要的记忆（正文都太短或已有摘要）".into());
    }
    let ask = format!(
        "为下面每条记忆写一句不超过 40 字的摘要，说明「这条记的是什么、结论是什么」。\n\
         不要以「这条记忆」开头。\n\n\
         输出 JSON：{{\"items\":[{{\"id\":\"条目 id\",\"summary\":\"摘要\"}}]}}\n\n\
         条目：\n{}",
        targets
            .iter()
            .map(|entry| format!("[{}] {}\n{}", entry.id, entry.title, model::truncate_chars(&entry.body, 900)))
            .collect::<Vec<_>>()
            .join("\n\n")
    );
    let reply = match llm::complete(Request::new("summarize", SYSTEM_EXTRACT, ask)) {
        Ok(reply) => reply,
        Err(error) => return from_llm(error),
    };
    let Some(parsed) = llm::parse_json(&reply.text) else {
        return Outcome::Failed("摘要结果不是合法 JSON，已丢弃".into());
    };
    let mut touched = 0usize;
    for item in parsed.get("items").and_then(Value::as_array).into_iter().flatten() {
        let Some(id) = item.get("id").and_then(Value::as_str) else { continue };
        let summary = item.get("summary").and_then(Value::as_str).unwrap_or("").trim();
        if summary.is_empty() {
            continue;
        }
        let Some(mut entry) = targets.iter().find(|entry| entry.id == id).cloned() else { continue };
        entry.summary_text = model::truncate_chars(summary, 120);
        entry.updated_at = crate::server::logging::now_ms();
        entry.path = store::rel(&store::entry_path(&entry, false));
        if store::save(&entry).is_err() {
            continue;
        }
        let _ = index::global().upsert(&entry);
        touched += 1;
    }
    if touched == 0 {
        return Outcome::Skipped("模型没给出可用摘要".into());
    }
    Outcome::Done(json!({ "summarized": touched, "model": reply.model }))
}

/// 事实失效判定：拿一条新记忆去比同主题的旧记忆，产 `supersede` 建议。
///
/// 默认只入队（`timeline.requireConfirm` = true）。关掉确认时才在
/// `review.autoConfirmSupersede` 也开着的情况下自动应用 —— 两个开关同时开，
/// 是因为这一步会**让旧事实失效**，误判的代价很高。
pub fn detect_supersede(new_id: &str) -> Outcome {
    if !config::bool_of("timeline.enabled") || !config::bool_of("timeline.autoDetect") {
        return Outcome::Skipped("双时间轴自动识别已关闭".into());
    }
    let all = store::load_all();
    let Some(entry) = all.iter().find(|entry| entry.id == new_id && entry.is_live()).cloned() else {
        return Outcome::Skipped(format!("找不到活着的记忆 {new_id}"));
    };
    if entry.layer != Layer::L2 {
        return Outcome::Skipped("只有沉淀层（l2）参与失效判定".into());
    }
    let mut candidates: Vec<model::MemoryEntry> = all
        .into_iter()
        .filter(|other| other.id != entry.id && other.is_live() && other.layer == Layer::L2)
        .filter(|other| other.kind == entry.kind)
        .filter(|other| other.project == entry.project)
        .collect();
    if candidates.is_empty() {
        return Outcome::Skipped("没有同主题的旧结论可比".into());
    }
    candidates.sort_by_key(|other| std::cmp::Reverse(other.updated_at));
    candidates.truncate(6);

    let ask = format!(
        "新记忆：\n[{}] {}\n{}\n\n\
         旧记忆：\n{}\n\n\
         判断哪条旧记忆被新记忆**在事实上**取代了（不是补充、不是不同角度）。\n\
         输出 JSON：{{\"supersedes\":[{{\"id\":\"旧记忆 id\",\"confidence\":0.0-1.0,\"reason\":\"为什么被取代\"}}]}}\n\
         没有取代任何一条时输出 {{\"supersedes\":[]}}。",
        entry.id,
        entry.title,
        model::truncate_chars(&entry.body, 900),
        candidates
            .iter()
            .map(|other| format!(
                "[{}] {}（{}）\n{}",
                other.id,
                other.title,
                search::format_date(other.updated_at),
                model::truncate_chars(&other.body, 500)
            ))
            .collect::<Vec<_>>()
            .join("\n\n")
    );
    let reply = match llm::complete(Request::new("supersede", SYSTEM_SUPERSEDE, ask)) {
        Ok(reply) => reply,
        Err(error) => return from_llm(error),
    };
    let Some(parsed) = llm::parse_json(&reply.text) else {
        return Outcome::Failed("失效判定结果不是合法 JSON，已丢弃".into());
    };
    let threshold = config::number("dedup.l4.autoUpdateThreshold").clamp(0.0, 1.0);
    let mut queued = 0usize;
    for item in parsed.get("supersedes").and_then(Value::as_array).into_iter().flatten() {
        let Some(old_id) = item.get("id").and_then(Value::as_str) else { continue };
        if !candidates.iter().any(|other| other.id == old_id) {
            continue;
        }
        let confidence = item.get("confidence").and_then(Value::as_f64).unwrap_or(0.0).clamp(0.0, 1.0);
        let reason = item.get("reason").and_then(Value::as_str).unwrap_or("").to_string();
        index::global().enqueue(
            "supersede",
            &format!("{new_id}->{old_id}"),
            &json!({
                "newId": new_id,
                "oldId": old_id,
                "reason": reason,
                "confidence": confidence,
                "model": reply.model,
            }),
            confidence,
        );
        // 两个开关同时开且置信度够高才自动应用
        let auto = !config::bool_of("timeline.requireConfirm")
            && config::bool_of("review.autoConfirmSupersede")
            && confidence >= threshold;
        if auto {
            if let Err(error) = super::mark_superseded(old_id, new_id) {
                crate::server::logging::log("[Memory]", &format!("⚠️ 自动标记失效失败：{error}"));
            }
        }
        queued += 1;
    }
    if queued == 0 {
        return Outcome::Skipped("模型认为没有旧结论被取代".into());
    }
    Outcome::Done(json!({ "suggestions": queued, "model": reply.model }))
}

/// 生成 / 刷新用户画像。
///
/// 画像与普通记忆的区别：它是**编译产物**，每次重生成都覆盖同一批
/// `type=profile` 条目（按 `sub` 区分四类），而不是无限追加。否则一年后
/// `memory_core` 里会塞进几十份互相矛盾的人格描述。
pub fn build_profile(force: bool) -> Outcome {
    if !config::bool_of("deep.enabled") || !config::bool_of("deep.personaEnabled") {
        return Outcome::Skipped("画像生成已关闭（deep.personaEnabled）".into());
    }
    let min = config::usize_of("deep.personaMinMemories").max(5);
    let all = store::load_all();
    let live: Vec<&model::MemoryEntry> = all.iter().filter(|entry| entry.is_live()).collect();
    if live.len() < min {
        return Outcome::Skipped(format!("记忆只有 {} 条，未达画像门槛 {min}", live.len()));
    }
    let existing: Vec<&model::MemoryEntry> = live
        .iter()
        .copied()
        .filter(|entry| entry.kind == MemoryKind::Profile)
        .collect();
    if !force && !existing.is_empty() {
        let newest = existing.iter().map(|entry| entry.updated_at).max().unwrap_or(0);
        let fresh = live.iter().filter(|entry| entry.updated_at > newest).count();
        if fresh < min {
            return Outcome::Skipped(format!("画像之后只有 {fresh} 条新记忆，未达门槛 {min}"));
        }
    }
    // 素材：重要度高 + 类型上是「关于这个人/这件事」的
    let mut material: Vec<&model::MemoryEntry> = live
        .iter()
        .copied()
        .filter(|entry| entry.kind != MemoryKind::Profile)
        .collect();
    material.sort_by(|left, right| {
        let score = |entry: &model::MemoryEntry| {
            entry.importance * 0.5
                + recency_score(entry.updated_at) * 0.3
                + if entry.pinned { 0.2 } else { 0.0 }
        };
        score(right).partial_cmp(&score(left)).unwrap_or(std::cmp::Ordering::Equal)
    });
    material.truncate(config::usize_of("deep.batchSize").max(20).min(200));

    let ask = format!(
        "以下是某位工程师自己的记忆条目。请归纳出四类画像，每类 1-4 条，每条一句话。\n\
         preference：长期偏好（工具、写法、沟通方式）\n\
         habit：工作习惯（怎么排查、怎么验证、什么时间做什么）\n\
         expertise：擅长领域（有反复出现的深度证据才算）\n\
         context：长期背景（在做什么项目、受什么约束）\n\n\
         每条必须给出支撑它的条目 id（evidence）。证据不足就少写几条，不要凑。\n\
         输出 JSON：{{\"items\":[{{\"sub\":\"preference\",\"title\":\"不超过 30 字\",\"body\":\"一句话\",\"evidence\":[\"id\"]}}]}}\n\n\
         条目：\n{}",
        material
            .iter()
            .map(|entry| format!(
                "[{}] {}（{}）\n{}",
                entry.id,
                entry.title,
                entry.kind.label(),
                model::truncate_chars(&entry.body, 400)
            ))
            .collect::<Vec<_>>()
            .join("\n\n")
    );
    let reply = match llm::complete(Request::new("profile", SYSTEM_PROFILE, ask)) {
        Ok(reply) => reply,
        Err(error) => return from_llm(error),
    };
    let Some(parsed) = llm::parse_json(&reply.text) else {
        return Outcome::Failed("画像结果不是合法 JSON，已丢弃（保留旧画像）".into());
    };
    let items = parsed.get("items").and_then(Value::as_array).cloned().unwrap_or_default();
    if items.is_empty() {
        return Outcome::Skipped("模型没给出画像条目".into());
    }

    // 旧画像进回收站（不物理删除：用户可能想对比上一版）
    let mut replaced = 0usize;
    for stale in &existing {
        let mut entry = (*stale).clone();
        if let Err(error) = store::detach_live(&entry) {
            crate::server::logging::log("[Memory]", &format!("⚠️ 旧画像移出失败：{error}"));
            continue;
        }
        entry.deleted_at = Some(crate::server::logging::now_ms());
        entry.updated_at = entry.deleted_at.unwrap_or(entry.updated_at);
        entry.path = store::rel(&store::entry_path(&entry, true));
        if store::save(&entry).is_ok() {
            let _ = index::global().upsert(&entry);
            replaced += 1;
        }
    }

    let mut written = 0usize;
    for item in items.iter().take(24) {
        let body = item.get("body").and_then(Value::as_str).unwrap_or("").trim();
        if body.chars().count() < 8 {
            continue;
        }
        let evidence: Vec<String> = item
            .get("evidence")
            .and_then(Value::as_array)
            .map(|items| items.iter().filter_map(|id| id.as_str().map(str::to_string)).collect())
            .unwrap_or_default();
        if config::bool_of("deep.evidenceRequired") && evidence.is_empty() {
            continue;
        }
        let sub = item.get("sub").and_then(Value::as_str).unwrap_or("preference");
        let title = item
            .get("title")
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim()
            .to_string();
        let id = model::make_profile_id(sub, body);
        let mut entry = model::MemoryEntry::draft(
            id,
            if title.is_empty() { model::truncate_chars(body, 40) } else { title },
            MemoryKind::Profile,
            body,
        );
        entry.layer = Layer::L2;
        entry.agent = Some("profile".to_string());
        entry.device = Some(store::device_id());
        entry.tags = vec![sub.to_string()];
        entry.importance = 0.9;
        // 钉住 + 星标：画像条目每一条都应该出现在 `memory_core` 里，而不是等到
        // 预算不够时被截掉
        entry.pinned = true;
        entry.starred = true;
        entry.evidence = evidence;
        entry.path = store::rel(&store::entry_path(&entry, false));
        if store::save(&entry).is_err() {
            continue;
        }
        let _ = index::global().upsert(&entry);
        written += 1;
    }
    if written == 0 {
        return Outcome::Skipped("模型给的画像条目都缺证据链，未写入".into());
    }
    crate::server::logging::log(
        "[Memory]",
        &format!("画像已重生成：{written} 条（替换 {replaced} 条旧画像）"),
    );
    Outcome::Done(json!({
        "written": written,
        "replaced": replaced,
        "material": material.len(),
        "model": reply.model,
    }))
}

/// 0..1 的新近度：半衰期取 `search.timeDecayHalfLife`。
fn recency_score(at: i64) -> f64 {
    let half = config::number("search.timeDecayHalfLife");
    if half <= 0.0 {
        return 1.0;
    }
    let now = crate::server::logging::now_ms();
    let days = ((now - at).max(0) as f64) / 86_400_000.0;
    0.5_f64.powf(days / half)
}

/// 供调度器与「立即执行」按钮共用的任务分发。
pub fn run_task(task: &str, project: Option<&str>) -> Outcome {
    match task {
        "distill" => distill(project, 0),
        "tag" => tag(&[], false, 10),
        "summarize" => summarize(&[], 10),
        "profile" => build_profile(false),
        other => Outcome::Skipped(format!("未知任务：{other}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recency_is_monotonic() {
        let now = crate::server::logging::now_ms();
        assert!(recency_score(now) > recency_score(now - 86_400_000 * 30));
    }

    #[test]
    fn outcome_labels() {
        assert_eq!(Outcome::Done(json!({})).label(), "done");
        assert_eq!(Outcome::Skipped("x".into()).label(), "skipped");
        assert!(Outcome::Failed("x".into()).is_failed());
    }
}
