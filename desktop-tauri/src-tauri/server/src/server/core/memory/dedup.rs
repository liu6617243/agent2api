//! 判重 · L3 候选召回 + L4 模型裁决。
//!
//! ── 为什么判重需要 L3 / L4 这两层 ──────────────────────────
//! 本仓的去重原来只有两级：L1 精确哈希（同一段字节不写第二份）与 L2 词面近似
//! （`pair_similarity` 取 Dice + 编辑距离 + 标题加权，过阈值就产一条建议）。
//! 这两级解决的是「字面重复」，而真机上最贵的重复不是字面重复：
//!
//!   「修改范围锁定为定点字节补丁」  vs  「坚持最小字节改动」
//!   「事故复盘清单化」              vs  「前端选型单文件离线 HTML」
//!
//! 前一对是同一件事的两次表述（该合），后一对只是同一个人在同一周写的两件事
//! （绝不能合），而词面分给它们的相似度几乎一样（0.73 / 0.75）。L2 只能把两对
//! 都塞进队列，剩下的判断没有工具可做 —— 人也不可能每天点几百条。
//!
//! 所以补两层，分工是**贵的判断只做一次，便宜的筛选做在前面**：
//!
//!   · **L3（本文件 `shortlist`）** —— 便宜的那一层。用索引里的 BM25 从全库
//!     召回 Top-K 条「词面相关」的记忆，把「两两比对全库」压成「这一条 vs 几
//!     条候选」。它不判重，只决定「模型该看哪几条」。
//!   · **L4（本文件 `judge`）** —— 贵的那一层。把这 K 条摆给模型，要求它在
//!     四种操作里选一个：`ADD` / `UPDATE` / `DELETE` / `NONE`。这是词面分
//!     永远做不到的事：只有读过两段话在说什么，才知道它们是同一件事的两次
//!     表述、还是同一领域的两个结论。
//!
//! ── 为什么不放在写入路径上 ────────────────────────────────
//! 写入是热路径：`memory_write` 与自动入库都是同步调用，一条记忆落地要让调用方
//! 等一次网络往返（实测 1~3 秒，模型慢时十几秒）。记忆写入**绝不能**因为模型
//! 慢而卡住 —— 它首先是本地文件操作。
//!
//! 所以 L4 挂在**队列巡检**上：写入仍然只做 L1 + L2（几百微秒，纯本地），产出的
//! 建议留在 `review_queue`；本任务的职责是给这些**已经排队**的建议补一份模型
//! 裁决，再交回 [`super::auto_resolve_ids`] 落地。代价是「判重比写入晚一轮」，
//! 换来的是「写入永远不等网络」。
//!
//! ── 裁决怎么写回去，为什么不直接执行 ──────────────────────
//! 判与执行必须分开（这是本仓的一条硬规矩，见 `mod.rs` 里 `execute` 的注释）：
//! 执行时如果重算一遍判据，两次计算之间快照可能已经变了，于是「判据说合并」
//! 与「执行时说作废」同时成立，而错的那次会静默生效。
//!
//! 所以 L4 **只写载荷**：把裁决写进那一行 `review_queue.payload` 的
//! `modelOp` / `modelConfidence` / `keep` / `candidate` 几个键，
//! 由 [`super::pair_outcome`] 统一读、[`super::execute`] 统一做。这样模型意见
//! 在队列页上完全可见（人能看见「模型为什么说该合」），而落地动作仍只有一条路。
//!
//! ── 置信度不够怎么办 ──────────────────────────────────────
//! `dedup.l4.autoUpdateThreshold` 之上才改写「怎么合」的那两个键（`keep` /
//! `candidate`）。之下时模型意见只作为参考留在载荷里，动作仍由词面判据定 ——
//! 一个 0.55 置信度的「这两条是一回事」不值得拿一条真结论去赌。

use std::collections::HashMap;

use serde_json::{json, Value};

use super::enrich::Outcome;
use super::llm::{self, Request};
use super::model::{truncate_chars, MemoryEntry};
use super::{config, index, search, store};

/// L4 的系统提示。
///
/// ── 为什么把「ADD 优先」写成硬规矩 ─────────────────────────
/// 判错的两个方向代价不对称：
///   · 该合没合 —— 库里多一条近义结论，检索时多占几行上下文，可逆、无损失；
///   · 不该合却合了 —— 被取代的那条**退出检索**，它独有的那半句从此读不到，
///     而「丢了什么」在合并之后无法从数据里看出来。
///
/// 所以默认动作是 ADD：只有模型能明确指出「同一件事、且没有独有信息」时才允许
/// UPDATE / DELETE。这与 `dedup_gate` 里「覆盖率不够就只连边」是同一条原则 ——
/// 拿不准的时候选不丢内容的那个动作。
const SYSTEM_DEDUP: &str = "\
你是记忆库的去重裁判。记忆是一条条关于用户、项目或技术的结论，会长期被检索使用。

你会收到若干组：每组一条「新记忆」和它的若干「候选」。逐组判断新记忆与候选的关系。

四个操作，只能选一个：
- ADD    —— 候选里没有同一件事。新记忆是新增内容，两条都留。
- UPDATE —— 同一件事，而其中一条信息更全（多出根因、路径、结论、边界条件）。应合并成一条。
- DELETE —— 同一件事且新记忆没有带来任何新信息（只是换个说法重复一遍）。新记忆该被丢弃。
- NONE   —— 候选都只是词面像（同领域、同项目、提到同一个文件名），讲的不是同一件事。什么都不做。

判断要点：
1. 「同一件事」= 两条在回答同一个问题。同一个系统的两个不同侧面、同一周的两件不同工作，都是 ADD。
2. 一条是另一条的具体例证 / 补充 / 后续进展，而自身没有独立结论 —— 那是 UPDATE。
3. 两条结论互相矛盾（一条说 A 对，一条说 A 错）不算重复，是演化 —— 那是 UPDATE，keep 选更晚的那条。
4. 拿不准时选 ADD：合并会丢掉被取代那条的独有内容，而多留一条只是多占一行。

只输出 JSON，不要解释、不要 Markdown 代码块：
{\"items\":[{\"id\":\"新记忆 id\",\"other\":\"最相关的候选 id（ADD / NONE 时给空串）\",\"op\":\"ADD|UPDATE|DELETE|NONE\",\"keep\":\"new|candidate\",\"confidence\":0.0-1.0,\"why\":\"不超过 20 字\"}]}

keep 只在 UPDATE 时有意义：留信息更全的那条（new = 留新记忆，candidate = 留候选）。DELETE 时 keep 固定填 candidate。";

/// 一条待判建议在内存里的形态。
///
/// 只存 id 不存 `MemoryEntry`：`store::snapshot()` 返回的是共享快照，判一整批的
/// 过程中可能被别的写入换掉（`snapshot` 每次按目录指纹判断是否重建），把引用
/// 跨调用握着会拿到过期数据 —— 每次用 id 现查，代价是 `HashMap` 的一次哈希。
struct Group {
    /// `review_queue` 行 id
    row_id: i64,
    /// 新记忆 id（`row.target`）
    new_id: String,
    /// L3 召回 + 入队候选，最多 `topK + 1` 个 id
    candidates: Vec<String>,
}

/// 一条模型裁决。
struct Verdict {
    op: String,
    /// 模型指认的候选 id；ADD / NONE 时为空串
    other: String,
    /// `new` / `candidate`
    keep: String,
    confidence: f64,
    why: String,
}

impl Verdict {
    /// 从一条模型输出里取裁决，校验不过就返回 `None`（那一条丢弃，不影响别的）。
    fn parse(item: &Value, group: &Group) -> Option<Self> {
        let op = item
            .get("op")
            .and_then(Value::as_str)?
            .trim()
            .to_ascii_uppercase();
        if !matches!(op.as_str(), "ADD" | "UPDATE" | "DELETE" | "NONE") {
            // 模型自创操作名（`MERGE` / `SKIP` / 中文）：当作这条没给结论。
            // 猜它的意思等于替模型做判重 —— 那正是这一层要避免的事。
            return None;
        }
        // 只认本次真的给出去的候选 id。模型编出来的 id 一旦写进载荷，
        // `pair_outcome` 会拿它去查条目、查不到就整对作废 —— 一句幻觉能清掉
        // 一条真建议，而队列上看不出是被谁清的。
        let raw = item.get("other").and_then(Value::as_str).unwrap_or_default().trim();
        let other = if group.candidates.iter().any(|id| id == raw) {
            raw.to_string()
        } else {
            String::new()
        };
        // UPDATE / DELETE 必须指名候选：没有对手的「该合并」是无法执行的动作。
        if other.is_empty() && matches!(op.as_str(), "UPDATE" | "DELETE") {
            return None;
        }
        let raw_keep = item
            .get("keep")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .trim()
            .to_ascii_lowercase();
        // DELETE 先判、且不看模型写的 `keep`：它的语义固定是「丢掉新写的这条」。
        // 这一支必须排在 `raw_keep` 判断之前 —— 模型写 `keep:"new"` 时如果先看
        // 模型的话，就会把「新条该丢」执行成「旧条该丢」，等于判据说合、执行说废。
        let keep = if op == "DELETE" {
            "candidate"
        } else if raw_keep == "new" {
            "new"
        } else if raw_keep == "candidate" || raw_keep == "other" || raw_keep == "old" {
            "candidate"
        } else {
            // UPDATE 没给方向时留新的那条：新记忆通常带着刚补上的口径，
            // 而留旧的会让「刚写的东西立刻被标成已取代」变多。
            "new"
        };
        Some(Verdict {
            op,
            other,
            keep: keep.to_string(),
            confidence: item
                .get("confidence")
                .and_then(Value::as_f64)
                .unwrap_or(0.0)
                .clamp(0.0, 1.0),
            why: truncate_chars(item.get("why").and_then(Value::as_str).unwrap_or_default().trim(), 40),
        })
    }
}

/// 给待确认队列里够格的 `dedup` 建议补一份模型裁决。
///
/// `limit` 是一轮最多看多少行（由调度器的 `auto.tasks.dedup.batchSize` 给），
/// 与「一次请求塞几组」是两件事：后者看 `dedup.l4.batchSize`。
pub fn judge(limit: usize) -> Outcome {
    if !config::bool_of("dedup.enabled") {
        return Outcome::Skipped("去重总开关已关闭（dedup.enabled）".into());
    }
    if !config::bool_of("dedup.l4.enabled") {
        return Outcome::Skipped("L4 语义判重已关闭（dedup.l4.enabled）".into());
    }
    let all = store::snapshot();
    let min_score = config::number("dedup.l4.minCandidateScore");
    let threshold = config::number("dedup.l4.autoUpdateThreshold");
    let rows = index::global().review_list(Some("pending"), Some("dedup"), limit.clamp(1, 200));

    let mut groups: Vec<Group> = Vec::new();
    for row in rows {
        // 已经判过的不再重复烧 token。`modelOp` 存在 = 这一对有模型结论了；
        // 它没落地只可能是「置信度没过线」或「这类自动裁决关着」，两种都不该
        // 靠重跑改变结论 —— 重跑只会花钱买一个可能不同的答案。
        if row.payload.get("modelOp").is_some() {
            continue;
        }
        let score = row
            .payload
            .get("score")
            .and_then(Value::as_f64)
            .unwrap_or(row.confidence);
        if score < min_score {
            // 词面分连「值得让模型看一眼」都够不上：这一档里绝大多数是同项目
            // 无关条目，过一次模型只是把「显然不像」翻译成「模型说不像」。
            continue;
        }
        let Some(entry) = all.iter().find(|item| item.id == row.target) else { continue };
        // 被反驳的结论不参与判重：拿一条已知错的记忆去合并别人，是把错误
        // 通过一次「合并」洗白成共识。
        if !entry.is_trusted() {
            continue;
        }
        let queued = row.payload.get("candidate").and_then(Value::as_str).unwrap_or_default();
        let candidates = shortlist(&all, entry, queued);
        if candidates.is_empty() {
            continue;
        }
        groups.push(Group { row_id: row.id, new_id: entry.id.clone(), candidates });
    }
    if groups.is_empty() {
        return Outcome::Skipped("没有待判的近似对".into());
    }

    let by_id: HashMap<&str, &MemoryEntry> = all.iter().map(|item| (item.id.as_str(), item)).collect();
    let batch = config::usize_of("dedup.l4.batchSize").clamp(1, 20);
    let mut judged = 0usize;
    let mut auto: Vec<i64> = Vec::new();
    let mut model = String::new();
    let mut first_error: Option<String> = None;

    for chunk in groups.chunks(batch) {
        let ask = build_ask(chunk, &by_id);
        let reply = match llm::complete(Request::new("dedup", SYSTEM_DEDUP, ask).for_items(chunk.len(), 220)) {
            Ok(reply) => reply,
            Err(error) => {
                if error.is_unavailable() {
                    // 没配模型不是失败：队列原样留着，下一轮再来。一条都没判成时
                    // 如实报跳过，让审计里看得出「是没模型，不是没活干」。
                    if judged == 0 {
                        return Outcome::Skipped(error.message().to_string());
                    }
                    break;
                }
                first_error.get_or_insert_with(|| error.message().to_string());
                continue;
            }
        };
        if model.is_empty() {
            model = reply.model.clone();
        }
        let Some(parsed) = llm::parse_json(&reply.text) else {
            first_error.get_or_insert_with(|| "判重结果不是合法 JSON".into());
            continue;
        };
        for item in parsed.get("items").and_then(Value::as_array).into_iter().flatten() {
            let id = item.get("id").and_then(Value::as_str).unwrap_or_default();
            // `chunk.iter()` 的 Item 是 `&Group`，`find` 直接给出 `Option<&Group>`，
            // 不需要再解一层引用。
            let Some(group) = chunk.iter().find(|group| group.new_id == id) else { continue };
            let Some(verdict) = Verdict::parse(item, group) else { continue };
            // 重新取一次：这一批模型调用可能花了几秒，其间队列行可能已经被
            // 别的路径裁决掉了（定时巡检、用户点批准）。
            let Some(row) = index::global().review_get(group.row_id) else { continue };
            if row.status != "pending" || row.payload.get("modelOp").is_some() {
                continue;
            }
            let mut payload = row.payload.clone();
            payload["modelOp"] = json!(verdict.op);
            payload["modelConfidence"] = json!(verdict.confidence);
            payload["modelWhy"] = json!(verdict.why);
            payload["modelName"] = json!(reply.model);
            payload["modelPick"] = json!(verdict.other);
            // 过线才改写落地方向；不过线时这对仍由词面判据处置，模型意见只
            // 留在队列页当参考 —— 否则一个 0.6 置信度的 `NONE` 会盖掉判据，
            // 而事后没人能解释「这一对为什么没合并」。
            if verdict.confidence >= threshold {
                if !verdict.other.is_empty() {
                    payload["candidate"] = json!(verdict.other);
                    payload["keep"] = json!(verdict.keep);
                }
            }
            // 无论过不过线都交回自动裁决：过了线由 `model_verdict` 接，没过线
            // 由 `dedup_gate` 接。留着不交，这一行就会因为「已经判过模型」而
            // 永久停在待确认 —— 队列清不掉，正是这套机制最该避免的结局。
            auto.push(group.row_id);
            if index::global().annotate(group.row_id, &payload) {
                judged += 1;
            }
        }
    }

    // 落地交给唯一的裁决点：这里只写载荷。
    let resolved = super::auto_resolve_ids(&auto);

    if judged == 0 {
        return match first_error {
            Some(error) => Outcome::Failed(format!("判重失败：{error}")),
            None => Outcome::Skipped("没有可判的近似对".into()),
        };
    }
    Outcome::Done(json!({
        "checked": groups.len(),
        "judged": judged,
        "applied": resolved.applied,
        "related": resolved.related,
        "retired": resolved.retired,
        "model": model,
    }))
}

/// L3 召回：给一条新记忆挑出模型该看的候选。
///
/// 顺序有讲究：入队那条永远排第一，再补 BM25 召回的邻居。
///   · 入队候选是词面判据给的「最佳匹配」，哪怕模型否掉它也必须看见它 ——
///     否则「模型说这几条都不像」与「模型根本没看过这条」在数据上无法区分；
///   · BM25 补的是词面分算不出来的那种相关：`pair_similarity` 是整段加权，
///     一条短结论与一条长复盘之间相似分会被长度差拉低，但它们讲的可能是同一
///     件事。BM25 只看词在不在，反而更容易把它们排在一起。
///
/// 过滤条件与 `near_duplicate` 保持一致（同层、同类、可信）：跨层合并会丢掉
/// 「这条是流水还是沉淀」的信息，跨类合并会把「偏好」与「避坑」压成一条。
fn shortlist(all: &[MemoryEntry], entry: &MemoryEntry, queued: &str) -> Vec<String> {
    let top_k = config::usize_of("dedup.l3.topK").clamp(1, 10);
    let usable = |other: &MemoryEntry| {
        other.id != entry.id
            && other.is_trusted()
            && other.layer == entry.layer
            && other.kind == entry.kind
    };
    let mut out: Vec<String> = Vec::new();
    if let Some(other) = all.iter().find(|other| other.id == queued) {
        if usable(other) {
            out.push(other.id.clone());
        }
    }
    if !config::bool_of("dedup.l3.enabled") {
        // 关掉 L3 = 退回「一条建议只看它自己配的那一对」。这仍然可用：
        // 模型判的就是那一对，只是看不到别的相近结论。
        return out;
    }
    let query = format!("{} {}", entry.title, entry.body);
    for (id, _rank) in index::global().bm25_candidates(&query, &entry.id, top_k + out.len()) {
        if out.iter().any(|seen| seen == &id) {
            continue;
        }
        let Some(other) = all.iter().find(|other| other.id == id) else { continue };
        if !usable(other) {
            continue;
        }
        out.push(id);
        if out.len() >= top_k + 1 {
            break;
        }
    }
    out
}

/// 把若干组摊成一段 prompt。
///
/// 正文截断到 400 / 300 字：判重看的是「在说什么」，不是「写了多少」。给全文
/// 只会让一次请求处理不了几组（`dedup.l4.batchSize` 默认 5 组 × 6 条候选），
/// 而 400 字足够看清一条结论的主语与判断。
fn build_ask(groups: &[Group], by_id: &HashMap<&str, &MemoryEntry>) -> String {
    let mut ask = String::from("判断下面每一组新记忆与该组候选的关系。\n");
    for (index, group) in groups.iter().enumerate() {
        let Some(entry) = by_id.get(group.new_id.as_str()) else { continue };
        ask.push_str(&format!(
            "\n## 组 {}\n新记忆 [{}] {}（{}）\n{}\n\n候选：\n",
            index + 1,
            entry.id,
            entry.title,
            search::format_date(entry.created_at),
            truncate_chars(&entry.body, 400)
        ));
        for id in &group.candidates {
            let Some(other) = by_id.get(id.as_str()) else { continue };
            ask.push_str(&format!(
                "[{}] {}（{}）\n{}\n",
                other.id,
                other.title,
                search::format_date(other.created_at),
                truncate_chars(&other.body, 300)
            ));
        }
    }
    ask
}

#[cfg(test)]
mod tests {
    use super::*;

    fn group(candidates: &[&str]) -> Group {
        Group {
            row_id: 1,
            new_id: "new-1".to_string(),
            candidates: candidates.iter().map(|id| id.to_string()).collect(),
        }
    }

    #[test]
    fn verdict_rejects_operations_the_pipeline_cannot_execute() {
        // 模型自创操作名（MERGE / SKIP / 中文）不能被猜成合并：猜错就是埋内容。
        let item = json!({ "id": "new-1", "other": "old-1", "op": "MERGE", "confidence": 0.9 });
        assert!(Verdict::parse(&item, &group(&["old-1"])).is_none());
        let item = json!({ "id": "new-1", "other": "old-1", "op": "合并", "confidence": 0.9 });
        assert!(Verdict::parse(&item, &group(&["old-1"])).is_none());
    }

    #[test]
    fn verdict_ignores_ids_that_were_never_offered() {
        // 编出来的 id 会让 `pair_outcome` 查不到对手、整对作废 —— 等于让模型
        // 的一句幻觉清掉一条真建议。所以不在候选表里的 id 一律当没给。
        let item = json!({ "id": "new-1", "other": "hallucinated", "op": "ADD", "confidence": 0.9 });
        let verdict = Verdict::parse(&item, &group(&["old-1"])).unwrap();
        assert_eq!(verdict.op, "ADD");
        assert!(verdict.other.is_empty());
    }

    #[test]
    fn update_without_a_named_candidate_is_dropped() {
        // 「该合并」却没说是跟谁合并：没有对手的动作无法执行，留着只会让
        // 队列页显示一条点不动的建议。
        let item = json!({ "id": "new-1", "other": "", "op": "UPDATE", "confidence": 0.95 });
        assert!(Verdict::parse(&item, &group(&["old-1"])).is_none());
    }

    #[test]
    fn delete_always_keeps_the_candidate() {
        // DELETE = 丢掉新写的这条。方向不能由模型随手写的 keep 决定，
        // 否则一个 `keep:"new"` 会让「新条该丢」执行成「旧条该丢」。
        let item = json!({ "id": "new-1", "other": "old-1", "op": "DELETE", "keep": "new" });
        assert_eq!(Verdict::parse(&item, &group(&["old-1"])).unwrap().keep, "candidate");
        let item = json!({ "id": "new-1", "other": "old-1", "op": "DELETE" });
        assert_eq!(Verdict::parse(&item, &group(&["old-1"])).unwrap().keep, "candidate");
    }

    #[test]
    fn update_keeps_the_new_entry_when_direction_is_missing() {
        // 方向缺失时留新的：新记忆通常带着刚补上的口径；留旧的会让「刚写的
        // 东西立刻被标成已取代」变多。
        let item = json!({ "id": "new-1", "other": "old-1", "op": "UPDATE", "confidence": 0.9 });
        assert_eq!(Verdict::parse(&item, &group(&["old-1"])).unwrap().keep, "new");
    }

    #[test]
    fn confidence_is_clamped_to_unit_interval() {
        let item = json!({ "id": "new-1", "other": "old-1", "op": "UPDATE", "confidence": 1.7 });
        assert_eq!(Verdict::parse(&item, &group(&["old-1"])).unwrap().confidence, 1.0);
        let item = json!({ "id": "new-1", "other": "old-1", "op": "UPDATE", "confidence": -2 });
        assert_eq!(Verdict::parse(&item, &group(&["old-1"])).unwrap().confidence, 0.0);
    }
}
