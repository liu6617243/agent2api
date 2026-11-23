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
//!   · 自动应用与否由 `review.autoConfirm*` 决定。改**已有**记忆的那几条
//!     （归类 / 失效 / 去重）默认全关，只入待确认队列；只**新增** l2 的
//!     蒸馏 / 合并默认直接落地（`review.autoConfirmDistill`，默认开）——
//!     因为队列留档 + 内容哈希幂等，落地是可撤销的，而「只入队」的实测结果
//!     是 l2 永远长不出来。
//!
//! ── 为什么不覆盖原文 ────────────────────────────────────────
//! 摘要写进「新增的一条 `l2 / insight`」，原文一个字都不动。记忆是历史陈述，
//! 改写历史等于伪造证据链。用户想看生成物就去看新条目，不想看就删掉它。

use std::collections::HashSet;

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
When the user stated a lasting requirement, preference or prohibition, record it as type=preference verbatim in spirit — \
those are re-injected into every future request, so they must be phrased as an instruction, not as a report. \
Answer in the same language as the input. Output strict JSON, no prose around it.";

const SYSTEM_TAG: &str = "You tag engineering notes for retrieval. \
Prefer the vocabulary the text already uses over synonyms you would invent. \
Output strict JSON, no prose around it.";

const SYSTEM_PROFILE: &str = "You build a long-term profile of one engineer from their own memory entries. \
Every statement must be supported by the entries you were given, and you must cite the entry ids. \
Never speculate about personality from a single event. \
The four sections are orthogonal: identity = who they are, communication = how they talk and want to be told, \
technical = what they build and with what, workstyle = how they investigate and verify. \
A statement belongs to exactly one section. Output strict JSON, no prose around it.";

const SYSTEM_SUPERSEDE: &str = "You compare a new memory entry against older entries on the same topic. \
Decide only whether the new entry makes an older one factually obsolete. \
A different angle or an extra detail is not obsolescence. Output strict JSON, no prose around it.";

/// 把一批 l1 流水蒸馏成 l2 结论。
///
/// `project` 为空表示「所有项目的流水一起看」——不推荐，但允许（第一次使用时
/// 用户往往还没分类）。
///
/// ── 产物默认直接落地，但仍然进队列留档 ──────────────────────
/// 早先这里只入队等人工点头。实测结果是死结：导入 2341 条之后深度记忆还是 9 条，
/// 而人不会每天点几百下「批准」—— l2 长不出来，请求级注入最值钱的那一段
/// （沉淀结论 + 踩坑/决策）就永远没东西可注。所以改成「先落地 + 队列留档」
/// （`review.autoConfirmDistill`，默认开）：批准时 `apply_derived` 靠内容哈希
/// 幂等，不会写成两条。
/// 现在还会先比对已有结论：能强化（追加证据锚点）的不再新建，确实没有才 new ——
/// 重复从生成端就被拦住，否则每跑一轮 distill 都净增几条同义 l2。
pub fn distill(project: Option<&str>, limit: usize) -> Outcome {
    if !config::bool_of("deep.enabled") {
        return Outcome::Skipped("深层记忆已关闭（deep.enabled）".into());
    }
    let cap = if limit == 0 { config::usize_of("deep.distillMaxPerProject").max(20) } else { limit };
    let all = store::load_all();
    let mut source: Vec<model::MemoryEntry> = all
        .iter()
        // 被反驳的结论不能当蒸馏素材：否则会被重新压成一条新结论，错误复活。
        .filter(|entry| entry.is_trusted() && entry.layer == Layer::L1)
        // 素材池不喂 auto-extract 自己的候选：那些条目是「待确认的结论」，不是
        // 原始流水。拿它们再蒸馏一遍 = 模型对自己上一轮的输出做二次压缩，同一件
        // 事被改写两遍，正是这一轮要消灭的重复。它们只作为「已有结论清单」出现，
        // 走 reinforce 那条路。
        .filter(|entry| entry.agent.as_deref() != Some(model::AUTO_EXTRACT_AGENT))
        // 项目口径统一走 `super::in_scope`：`entry.project` 是 slug，而调用方
        // 传的是原始名字（`aimod-cc/Agent2API`），直接比字符串会让过滤静默
        // 失效 —— 素材池看起来「一条都没有」，任务每轮都跳过。
        .filter(|entry| super::in_scope(entry.project.as_deref(), project))
        .cloned()
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

    // 已有结论清单：口径与 extract 一致 —— l2（真沉淀）+ auto-extract 候选都算
    // 「已经有人提过」。只喂 l2 的话，候选会先被 distill 原样造一遍，等于把上一轮
    // 在生成端拦住的重复又放回来。和 source 共用同一次 load_all，不重复读盘。
    //
    // 上限 60 条是 token 预算的取舍：清单只用于让模型「认出这是老话题」，不是
    // 让它逐条比对。按 updated_at 倒序取最近的，冷门老结论认不出来时会走 new，
    // 由后续的 near_duplicate 兜底。
    let mut known: Vec<model::MemoryEntry> = all
        .iter()
        // 被否掉的结论不该被当成「这个老话题已经讲过」，否则模型会去 reinforce 它。
        .filter(|entry| entry.is_trusted())
        .filter(|entry| {
            entry.layer == Layer::L2
                || entry.agent.as_deref() == Some(model::AUTO_EXTRACT_AGENT)
        })
        .filter(|entry| super::in_scope(entry.project.as_deref(), project))
        .cloned()
        .collect();
    known.sort_by_key(|entry| std::cmp::Reverse(entry.updated_at));
    known.truncate(60);
    let known_ids: HashSet<String> = known.iter().map(|entry| entry.id.clone()).collect();
    let catalog = known
        .iter()
        .map(|entry| format!("[{}] {}", entry.id, model::truncate_chars(&entry.title, 40)))
        .collect::<Vec<_>>()
        .join("\n");
    let catalog = if catalog.is_empty() {
        "（还没有任何结论）".to_string()
    } else {
        catalog
    };

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
         insight（可跨项目复用的规律）/ pitfall（踩过的坑）/ \
         preference（用户明确提过的长期要求、禁止事项、做法偏好）之一。\n\
         只写仍然成立的东西；被后来推翻的内容不要写。\n\n\
         ── preference 特别说明 ────────────────────────────────\n\
         用户说过「不要这样做」「以后都这样」「保持 X 不变」这类话，就是 preference。\n\
         写成命令句（「版本号保持 3.5.0-beta.1，不要自行 bump」），不要写成汇报\n\
         （「用户说他不想升版本号」）—— 这条会被原样注入到之后每一次对话里，\n\
         汇报句读起来像历史，命令句读起来像要求。\n\n\
         ── 先比对已有结论，再决定动作 ─────────────────────────\n\
         已经有这些结论（id 只能从这里选，不要自己编）：\n{catalog}\n\n\
         reinforce = 这批流水讲的事，上面清单里已经有了。target 填那条的 id，\n\
                     note 写这次补充或确认了什么。\n\
                     **哪怕措辞完全不同，只要是同一件事，就是 reinforce。**\n\
         new       = 清单里确实没有，才新建。\n\
                     同一件事被记成两条是最严重的错误 —— 宁可 reinforce，不要 new。\n\n\
         输出 JSON：{{\"items\":[\n\
         {{\"action\":\"reinforce\",\"target\":\"清单里的 id\",\"note\":\"这次补充或确认了什么\"}},\n\
         {{\"action\":\"new\",\"title\":\"不超过 40 字\",\"type\":\"decision|knowledge|insight|pitfall|preference\",\"body\":\"100-300 字，含理由与取舍\",\"importance\":0.0-1.0,\"evidence\":[\"引用到的条目 id\"]}}\n\
         ]}}\n\n\
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
    let mut applied_count = 0usize;
    let mut reinforced = 0usize;
    // 强化锚点：这批素材的指纹，不是模型写的 `note`。
    //
    // ── 为什么不能用 note ─────────────────────────────────────
    // `note` 是自然语言（「再次确认了版本号约束」），模型对不同批次写出同一句
    // 话是常态。而 `reinforce_entry` 按 cue 去重 —— 两句一模一样的 note 只会
    // 记一次证据，于是「被多批流水反复确认」这个升级条件永远攒不满，候选卡在
    // l1 出不来。这正是要修的毛病的反面：从「太容易升」变成「永远升不了」。
    //
    // 用素材指纹则两边都对：同一批流水重跑 → 同一串 → 去重，计数不虚高；
    // 不同批流水确认同一件事 → 不同串 → 真的加一。与 `extract` 拿
    // `recent.first().id` 当 cue 是同一思路（那边素材是「最近 N 条流水」，
    // 这里素材是「按重要度截断后的 N 条」，同样取最新一条的 id）。
    let cue = source
        .iter()
        .max_by_key(|entry| entry.updated_at)
        .map(|entry| format!("distill:{}", entry.id))
        .unwrap_or_default();
    for item in items.iter().take(20) {
        if item.get("action").and_then(Value::as_str) == Some("reinforce") {
            let target = item.get("target").and_then(Value::as_str).unwrap_or("").trim();
            // 模型编了 id 或指向清单外：直接丢，不降级成 new。
            // 降级会把它变成一条新的重复结论，正是这一轮要消灭的东西。
            if target.is_empty() || !known_ids.contains(target) {
                continue;
            }
            // 只数「真的写进去了」的那些：模型每轮都会把清单里的老结论
            // reinforce 一遍，其中多数锚点早就有了。把 NOOP 也计入会让审计
            // 文案变成「本轮确认 8 条」，而库里一条都没动。
            if super::reinforce_entry(target, &cue).map(|out| out.changed).unwrap_or(false) {
                reinforced += 1;
            }
            continue;
        }
        let body = item.get("body").and_then(Value::as_str).unwrap_or("").trim();
        if body.chars().count() < 12 {
            continue;
        }
        let title = item.get("title").and_then(Value::as_str).unwrap_or("").trim().to_string();
        let kind = MemoryKind::parse(item.get("type").and_then(Value::as_str).unwrap_or("insight"));
        // ── 候选之间也判一次重 ─────────────────────────────────
        //
        // 模型说 `new` 的依据只是「清单里的标题」，而清单只有 60 条、只有标题。
        // 清单外那些措辞相近的候选它看不到，于是同一件事被蒸馏成第二份候选，
        // 两份各自攒证据、各自升级 —— 重复被推迟，没被消灭。
        //
        // 落盘前按正文再比一次（判据见 `similar_l1_candidate`，只比 auto-extract
        // 的 l1 候选，不碰人写的流水）。命中就算「又确认了一次」。
        if let Some(existing) = super::similar_l1_candidate(project, kind, body) {
            // 带正文一起强化：候选被同一件事的另一个措辞覆盖时，留信息量更大的那份。
            // 只加锚点的话，先落盘的那份半截候选会一直代表这件事，直到它升 l2。
            // 同样只数真的写进去的（见上面 reinforce 分支的说明）。
            if super::reinforce_entry_with(&existing, &cue, Some(body), Some(title.as_str()))
                .map(|out| out.changed)
                .unwrap_or(false)
            {
                reinforced += 1;
            }
            continue;
        }
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
        let payload = json!({
            "title": title,
            "type": kind.as_str(),
            "body": body,
            "importance": importance,
            "evidence": evidence,
            "project": project,
            "model": reply.model,
        });
        let queued_id = index::global().enqueue("distill", &target, &payload, importance);
        // ── 默认直接落地（`review.autoConfirmDistill`）────────────
        //
        // 为什么蒸馏和「归类 / 失效 / 去重」三条自动确认的默认值相反：那三条改的
        // 是**已有**记忆，判错就破坏原数据；蒸馏只**新增**一条 l2，来源写在
        // `supersedes` / `evidence` 里，删掉就回到原样。
        //
        // 而默认「只入队」的实测结果是死结：导入 2341 条之后深度记忆还是 9 条 ——
        // 蒸馏产物全躺在待确认队列里等人点头，人不会每天点几百下。l2 长不出来，
        // 请求级注入最值钱的那一段（沉淀结论 + 踩坑/决策）就永远没东西可注。
        //
        // 队列**照样留着**（`enqueue` 已写入），所以这是「先落地 + 可审计」，
        // 不是「不留痕」。批准后 `apply_derived` 会再跑一遍，靠 `supersedes`
        // 与内容哈希去重，不会写成两条。
        let mut applied = false;
        if config::bool_of("review.autoConfirmDistill") {
            match super::apply_derived(&payload) {
                Ok(()) => {
                    applied = true;
                    if let Some(id) = queued_id {
                        index::global().review_resolve(&[id], "approved", Some("auto-distill"));
                    }
                }
                Err(error) => {
                    // 落地失败不算整体失败：队列里那条还在，人还能点头。
                    // 这里只记日志，不把已经成功的其它条目一起丢掉。
                    crate::server::logging::verbose(
                        "[Memory]",
                        &format!("蒸馏结论自动落地失败（已留在待确认队列）：{error}"),
                    );
                }
            }
        }
        if applied {
            applied_count += 1;
        }
        queued += 1;
    }
    if queued == 0 && reinforced == 0 {
        return Outcome::Skipped("模型既没给出新结论，也没确认任何已有结论".into());
    }
    Outcome::Done(json!({
        "queued": queued,
        "applied": applied_count,
        "reinforced": reinforced,
        "material": source.len(),
        "model": reply.model,
        "tokens": reply.prompt_tokens + reply.completion_tokens,
    }))
}

/// 给一条记忆补标签（只补空标签的条目，或强制全部）。
pub fn tag(ids: &[String], force: bool, limit: usize) -> Outcome {
    let mut targets: Vec<model::MemoryEntry> = store::load_all()
        .into_iter()
        // 不给一条被否掉的错误结论打标签、花 token：它不配进检索标签体系。
        .filter(|entry| entry.is_trusted())
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
        // 摘要是「这条值得被记住」的信号，被反驳的结论不该拿到它。
        .filter(|entry| entry.is_trusted() && entry.body.chars().count() >= 200)
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
    // 新记忆本身若已被反驳，就不再拿它去判别人的失效：错误结论没有资格推翻别人。
    let Some(entry) = all.iter().find(|entry| entry.id == new_id && entry.is_trusted()).cloned() else {
        return Outcome::Skipped(format!("找不到活着的记忆 {new_id}"));
    };
    if entry.layer != Layer::L2 {
        return Outcome::Skipped("只有沉淀层（l2）参与失效判定".into());
    }
    let mut candidates: Vec<model::MemoryEntry> = all
        .into_iter()
        // 旧结论也必须可信：不能让一条已被否掉的判断通过「被再次取代」而洗白。
        .filter(|other| other.id != entry.id && other.is_trusted() && other.layer == Layer::L2)
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
///
/// 传 `sub` 就是「只重生成这一类」：改了一类结论、或只想刷新技术偏好时，没必要
/// 把另外三类也重算一遍（既是 token 浪费，也会让**已经满意的三类结论被无谓地
/// 洗掉**——模型每次措辞都不一样）。
pub fn build_profile(sub: Option<&str>, force: bool) -> Outcome {
    if !config::bool_of("deep.enabled") || !config::bool_of("deep.personaEnabled") {
        return Outcome::Skipped("画像生成已关闭（deep.personaEnabled）".into());
    }
    let scope: Option<&'static str> = sub.map(super::profile_sub);
    let min = config::usize_of("deep.personaMinMemories").max(5);
    let all = store::load_all();
    // 画像素材必须是可信事实：被反驳的结论进画像等于把错误写进「这个人是谁」。
    let live: Vec<&model::MemoryEntry> = all.iter().filter(|entry| entry.is_trusted()).collect();
    // 门槛按**真记忆**算：草稿不算「这个人身上已经积累的事实」。否则一个刚装上
    // 的库跑了几轮抽取就凑够 5 条草稿，画像立刻开始生成 —— 素材全是模型自己
    // 编的，画出来的「画像」也是模型的自我复读。
    let real = live.iter().filter(|entry| !super::is_auto_draft(entry)).count();
    if real < min {
        return Outcome::Skipped(format!("记忆只有 {real} 条，未达画像门槛 {min}"));
    }
    // 本次要重写的那几类旧结论。别的类的条目一个字都不动。
    let existing: Vec<&model::MemoryEntry> = live
        .iter()
        .copied()
        .filter(|entry| entry.kind == MemoryKind::Profile)
        .filter(|entry| match scope {
            Some(want) => super::profile_sub(entry.tags.first().map(String::as_str).unwrap_or("")) == want,
            None => true,
        })
        .collect();
    if !force && !existing.is_empty() {
        let newest = existing.iter().map(|entry| entry.updated_at).max().unwrap_or(0);
        let fresh = live
            .iter()
            .filter(|entry| entry.updated_at > newest)
            .filter(|entry| !super::is_auto_draft(entry))
            .count();
        if fresh < min {
            return Outcome::Skipped(format!("画像之后只有 {fresh} 条新记忆，未达门槛 {min}"));
        }
    }
    // 素材：重要度高 + 类型上是「关于这个人/这件事」的
    //
    // ── 草稿不能当画像素材 ─────────────────────────────────────
    // 画像产物是 `layer = l2` + `kind = profile`，而 `inject::rule_candidate`
    // 对画像无条件放行（`kind` 命中就进「必须遵守」块）。也就是说：一条还没被
    // 确认的草稿，只要被喂进画像、再被模型写成一条 profile，它就绕过了整条
    // 准入模型，直接变成之后每一次请求里的硬约束。这条链路比「草稿进检索」
    // 严重一档 —— 检索是模型按需读，硬约束是每次都被塞进 system prompt。
    //
    // 所以素材池必须与检索/注入同一口径：还在 l1 的 auto-extract 产物不参与。
    //
    // ── 为什么再收窄成「l2 或高重要度」（2026-11-23，对齐 AgentHub）──
    // 原来是「全部可信非草稿条目按 importance 排，截 `deep.batchSize` 条」。
    // 本机实测：214 条素材池里前 50 条恰好全是 l2，所以没出过事 —— 但这是
    // 排序碰巧，不是判据。一旦 l1 里出现几条 importance 0.9 的导入流水，
    // 它们就会挤掉真结论进画像 prompt，画出来的「画像」是某次会话的转述。
    //
    // AgentHub 的原判据是 `layer = 'l2' OR importance >= 4`（importance 满分
    // 5，即 >= 0.8）。这里照抄：画像的输入必须是「已经沉淀过的结论」或
    // 「有人主动抬过高重要度的条目」，不是任意流水。
    let mut material: Vec<&model::MemoryEntry> = live
        .iter()
        .copied()
        .filter(|entry| entry.kind != MemoryKind::Profile)
        .filter(|entry| !super::is_auto_draft(entry))
        .filter(|entry| entry.layer == Layer::L2 || entry.importance >= 0.8)
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

    // 四类的定义直接取自 `PROFILE_SUBS`：这里再抄一遍的话，改分类就得改两处，
    // 而且必然出现「prompt 说三类、界面画四类」的漂移。
    // `copied()` 让元素从 `&(&str, &str, &str)` 变成元组本体：直接解构引用
    // 会得到 `&&str`，与 `scope` 的 `&str` 比较时还要额外解一层。
    let outline = super::PROFILE_SUBS
        .iter()
        .copied()
        .filter(|(key, _, _)| scope.is_none_or(|want| *key == want))
        .map(|(key, label, hint)| format!("{key}（{label}）：{hint}"))
        .collect::<Vec<_>>()
        .join("\n");
    let example = scope.unwrap_or(super::PROFILE_SUBS[0].0);
    // 证据要求在 prompt 与代码里必须是**同一句话**。
    //
    // 原来 prompt 无条件写「证据不足 2 条的结论不要输出」，而代码侧
    // `deep.evidenceRequired=false` 时会放行无证据条目 —— 两边不一致的后果是
    // 用户关掉这个旋钮之后，模型仍然只产出带 2 条证据的结论（它照 prompt 做），
    // 于是「关掉了」这件事在结果上看不出来，用户以为开关坏了。
    //
    // 代码侧的门槛只有两档：`PROMOTE_AFTER_REINFORCE`（2）或 0。prompt 照抄这两档。
    let need_evidence = config::bool_of("deep.evidenceRequired");
    let evidence_rule = if need_evidence {
        // 三个位置参数全用 `{}`：混用位置参数与命名参数也能编，但本机没有 cargo，
        // 少一个参数只会在 CI 里炸一次，不值得为省两个字符冒这个险。
        format!(
            "硬性要求：每条结论必须带 {}~{} 个下面真实存在的条目 id 作为证据；\n\
             证据不足 {} 条的结论不要输出；不要编造 id（编造的会被丢弃）。",
            model::PROMOTE_AFTER_REINFORCE,
            5,
            model::PROMOTE_AFTER_REINFORCE
        )
    } else {
        "（证据链要求已关闭：evidence 可留空，但若给出就必须是下面真实存在的 id。）".to_string()
    };

    let ask = format!(
        "以下是某位工程师自己的记忆条目。请归纳出下列每一类的画像结论，每类 1-4 条，每条一句话。\n\
         {outline}\n\n\
         每一类必须正交：一条结论只属于一类，不要换个说法把同一条写进两类。\n\
         {evidence_rule}\n\
         输出 JSON：{{\"items\":[{{\"sub\":\"{example}\",\"title\":\"不超过 30 字\",\"body\":\"一句话\",\"evidence\":[\"id\",\"id\"]}}]}}\n\n\
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

    // ── 手动置顶的条目要活过重新生成 ────────────────────────────
    // 用户钉住一条结论，表达的是「这句我认，别动它」。这是这个页面上唯一
    // **人的判断压过模型**的地方，所以它必须在替换时被排除，而不是照常进回收站
    // 再让用户从一堆新结论里找回来。
    let kept: Vec<&model::MemoryEntry> = existing
        .iter()
        .copied()
        .filter(|entry| entry.pinned && entry.tags.iter().any(|tag| tag == super::PROFILE_PIN_TAG))
        .collect();
    let kept_ids: Vec<&str> = kept.iter().map(|entry| entry.id.as_str()).collect();

    // 旧画像进回收站（不物理删除：用户可能想对比上一版）
    let mut replaced = 0usize;
    for stale in existing.iter().filter(|entry| !kept_ids.contains(&entry.id.as_str())) {
        let mut entry = (*stale).clone();
        entry.deleted_at = Some(crate::server::logging::now_ms());
        entry.updated_at = entry.deleted_at.unwrap_or(entry.updated_at);
        entry.path = store::rel(&store::entry_path(&entry, true));
        // 「摘活形态 + 写回收站」一个临界区：`stale` 是改动前的对象，摘活形态
        // 必须按它算路径；分成 detach + save 两次的话，中间那一段里同 id 两份都在。
        if store::retire(stale, &entry).is_ok() {
            let _ = index::global().upsert(&entry);
            replaced += 1;
        }
    }

    let mut written = 0usize;
    let mut superseded_pins = 0usize;
    // 本次素材的真实 id 集合：证据必须落在这个集合里。
    //
    // ── 为什么不能只判「非空」（2026-11-23）────────────────────
    // 原判据是 `evidence.is_empty()` —— 模型随手编一个 id 也算「有证据」。
    // 画像比普通条目更该校验：它的产物会被注入到每一次请求，一条「凭空的
    // 结论」比一条「凭空的流水」危害大得多。
    //
    // AgentHub 的做法是 `_renderProfileSection` 里 `.filter((id) => validIds.has(id))`，
    // prompt 里同时写死「每条结论必须带 2~5 个真实存在的记忆 id」。这里对齐：
    // 先按素材池过滤掉编造的 id，再要求至少 2 条 —— 与草稿升 l2 的
    // `PROMOTE_AFTER_REINFORCE` 同一口径，不搞两套标准。
    // `(*entry).id.as_str()` 而不是 `entry.id.as_str()`：`material` 的元素是
    // `&MemoryEntry`，`iter()` 给出的是 `&&MemoryEntry`。不解这一层的话，
    // `.as_str()` 借的是循环变量的临时引用，`HashSet<&str>` 存不下。
    let material_ids: HashSet<&str> = material.iter().map(|entry| (*entry).id.as_str()).collect();
    for item in items.iter().take(24) {
        let body = item.get("body").and_then(Value::as_str).unwrap_or("").trim();
        if body.chars().count() < 8 {
            continue;
        }
        // 只留真实存在于素材池里的 id：模型编出来的、或者从 prompt 别处抄来的
        // 都不算数。`HashSet<&str>` 借的是 `material`，两者生命周期一致。
        let evidence: Vec<String> = item
            .get("evidence")
            .and_then(Value::as_array)
            .map(|items| {
                items
                    .iter()
                    .filter_map(|id| id.as_str())
                    .filter(|id| material_ids.contains(*id))
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default();
        // 门槛：与 `PROMOTE_AFTER_REINFORCE` 一致。`deep.evidenceRequired` 关掉
        // 时退回**原行为**（完全不要求证据），不是「只要求 1 条」——
        // 那个旋钮的文案是「画像结论必须附证据链」，关掉它就是「别管证据」。
        // 顺手多要一条等于偷偷改用户的设置，而且只会在下一次画像时暴露成
        // 「怎么少了好几条」。
        let required = if config::bool_of("deep.evidenceRequired") {
            model::PROMOTE_AFTER_REINFORCE
        } else {
            0
        };
        if evidence.len() < required {
            continue;
        }
        let sub = super::profile_sub(item.get("sub").and_then(Value::as_str).unwrap_or(""));
        // 只重生成一类时，模型偶尔会顺带写别的类 —— 丢掉，不然「只刷新技术偏好」
        // 会悄悄改掉沟通类。
        if scope.is_some_and(|want| want != sub) {
            continue;
        }
        let title = item
            .get("title")
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim()
            .to_string();
        let id = model::make_profile_id(sub, body);
        // 与保留下来的置顶条目同 id（正文一字不差）→ 不重写：重写会把 pinned
        // 清成 false，等于用户钉住的那条被「重新生成」偷偷摘了钉。
        if kept_ids.contains(&id.as_str()) {
            superseded_pins += 1;
            continue;
        }
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
        // 星标但不钉住：星标表达「这是精选结论」，钉住表达「别动它」。生成的条目
        // 两者都不该占 —— 钉住留给用户，否则「保留手动置顶」就无从判定（上一版
        // 给每一条都设了 pinned，用户根本分不出哪条是自己钉的）。
        entry.pinned = false;
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
        &format!(
            "画像已重生成：{written} 条（替换 {replaced} 条旧画像，保留 {} 条置顶）",
            kept.len()
        ),
    );
    Outcome::Done(json!({
        "written": written,
        "replaced": replaced,
        "kept": kept.len(),
        "duplicatePins": superseded_pins,
        "material": material.len(),
        "model": reply.model,
        "scope": scope.unwrap_or("all"),
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
        "profile" => build_profile(None, false),
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
