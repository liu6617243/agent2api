//! 记忆中枢的 MCP 服务规格与工具分发（`/mcp/memory`）。
//!
//! ── 为什么记忆只走 MCP，而不是网关改请求体 ──────────────────
//! 早期版本把"注入"做成"转发前改写 system"，那是**层次错位**：客户端本来就能
//! 自己拉（它知道自己 cwd、知道当前在哪个项目），网关不知道。由网关塞就必然
//! 跨项目串味、对全机流量生效、破坏上游 prefix cache、把本地私有内容被动外发。
//! 现在改成 MCP：Agent 自己决定什么时候想起来，网关只提供工具。
//!
//! 这条路径与 AgentHub 的记忆中枢同构（MCP + 工具），差别只在它用 stdio 子进程
//! 桥、本仓用 Streamable HTTP 单端点 —— 客户端两种都认。
//!
//! ── 行为约定写在哪 ──────────────────────────────────────────
//! 写在 MCP `initialize.instructions` 里（[`INSTRUCTIONS`]），而不是往用户的
//! `AGENTS.md` 塞受控块。`instructions` 是协议内的正规通道，由客户端决定要不要
//! 转给模型；改用户文件是副作用，还得额外承担"备份 / 只动自己的块 / 幂等卸载"。
//! 协议能表达的事就不去动用户的文件。
//!
//! ── 工具面为什么分「给模型的」与「给人/给运维的」────────────
//! `memory_core / digest / search / get / timeline / write / update / recent` 是
//! 模型真会调的；`projects / forget / status` 是模型也可能用的管理与自检。
//! `review_*` 与 `health` 属于「人做裁决、Agent 报问题」那类：模型只该**提**
//! 建议与**报**状态，不该自己批准「这条记忆作废」。所以裁决工具在 schema 上
//! 明确写了「需要用户确认」——它不是模型的自助通道。

use serde_json::{json, Value};

use super::mcp::McpSpec;
use crate::server::core::memory::{self, MemoryKind, MemoryQuery};

/// 给模型的固定行为约定（`initialize.instructions`）。
///
/// 三条：开局读一次核心记忆、提到「之前/上次」时检索、做完决策写回去。
/// 另加一条硬约束「磁盘代码是唯一现实」—— 记忆是历史陈述，不是当前状态；
/// 没有这句，模型会拿一周前的记忆去覆盖刚改过的代码。
pub const INSTRUCTIONS: &str = "Local project memory (markdown on this machine). Two layers: l1 is raw daily notes, l2 is distilled conclusions (decisions, knowledge, pitfalls). \
Call memory_core once at the start of a session to load the long-term picture; \
call memory_search when the user refers to earlier work or an earlier decision, and memory_get to read an entry in full. \
After finishing a task or making a decision, call memory_write with the reasoning, the trade-offs and the pitfalls — not the volatile code details. Prefer type=decision/knowledge/pitfall so the entry lands in l2; use type=daily only for raw session notes. \
Use `supersedes` to retire an entry that is no longer true, `validTo` to state that a fact only holds until a date, and `files` to anchor entries to relative file paths so stale-memory warnings can fire. \
Never approve review items yourself: memory_review_list shows suggestions (dedup, project classification, stale facts) and memory_review_resolve needs the user's explicit go-ahead. \
The code on disk is the only current truth: memory is history, intent and warnings, never a snapshot of the present. \
Read the actual files before editing them, and never revert working code back to an older memory.";

/// 单条正文上限：MCP 结果会被整段塞进上下文，超长正文留给 `memory_get`。
const MAX_BODY_CHARS: usize = 6000;

fn spec_tools() -> Vec<Value> {
    vec![
        json!({
            "name": "memory_core",
            "title": "核心记忆",
            "description": "Load the long-term picture: user profile plus per-project conclusions (tier 1 of the disclosure ladder, default ~800 tokens). Only distilled (l2) entries, profiles and pinned items are included — raw daily notes are not. Call once at session start. Response carries `truncated`; when truncated, use memory_search or memory_digest for more.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "project": { "type": "string", "description": "Project slug to focus on. Omit for profile plus all projects." },
                    "maxTokens": { "type": "number", "description": "Budget for this call (200-4000). Omit to use the configured default." }
                },
                "additionalProperties": false
            },
            "annotations": { "readOnlyHint": true, "destructiveHint": false, "idempotentHint": true, "openWorldHint": false }
        }),
        json!({
            "name": "memory_digest",
            "title": "索引概览",
            "description": "Compact overview of the whole memory store (max ~200 lines): projects, their latest entries and frequent tags. Learn what exists without searching.",
            "inputSchema": {
                "type": "object",
                "properties": { "maxLines": { "type": "number", "description": "Line budget, default 200." } },
                "additionalProperties": false
            },
            "annotations": { "readOnlyHint": true, "destructiveHint": false, "idempotentHint": true, "openWorldHint": false }
        }),
        json!({
            "name": "memory_search",
            "title": "检索记忆",
            "description": "Search memory and return compact index lines (titles and metadata, not full text). Superseded, expired and deleted entries are excluded unless asked for. Ranking blends relevance, recency, importance, project/agent affinity, graph expansion, past hit heat and layer (l2 over l1).",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "query": { "type": "string", "description": "Keywords. CJK is matched character by character, so short Chinese queries work." },
                    "project": { "type": "string" },
                    "agent": { "type": "string" },
                    "type": { "type": "string", "description": "Exact category: daily / decision / knowledge / preference / pitfall / profile / insight / other." },
                    "layer": { "type": "string", "description": "l1 (raw notes) or l2 (distilled). Omit for both." },
                    "tag": { "type": "string", "description": "Single tag, substring match." },
                    "starred": { "type": "boolean" },
                    "pinned": { "type": "boolean" },
                    "limit": { "type": "number", "description": "Default 30, max 500." },
                    "includeSuperseded": { "type": "boolean", "description": "Also return entries replaced by a newer one." },
                    "includeExpired": { "type": "boolean", "description": "Also return entries whose validTo has passed." }
                },
                "required": ["query"],
                "additionalProperties": false
            },
            "annotations": { "readOnlyHint": true, "destructiveHint": false, "idempotentHint": true, "openWorldHint": false }
        }),
        json!({
            "name": "memory_get",
            "title": "取全文",
            "description": "Read full text of one or more entries by id (per-entry truncation). Each result lists up to three related entries.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "ids": { "type": "array", "items": { "type": "string" }, "description": "Entry ids." },
                    "id": { "type": "string", "description": "Single id (shorthand for ids)." }
                },
                "additionalProperties": false
            },
            "annotations": { "readOnlyHint": true, "destructiveHint": false, "idempotentHint": true, "openWorldHint": false }
        }),
        json!({
            "name": "memory_timeline",
            "title": "演化链",
            "description": "Show how a fact evolved (A → B → C) by following supersede links, with each entry's valid-from date. Accepts an entry id, or a topic that is looked up first.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "id": { "type": "string" },
                    "topic": { "type": "string", "description": "Used when id is absent: the best-matching entry is located first." }
                },
                "additionalProperties": false
            },
            "annotations": { "readOnlyHint": true, "destructiveHint": false, "idempotentHint": true, "openWorldHint": false }
        }),
        json!({
            "name": "memory_write",
            "title": "写入记忆",
            "description": "Write one memory. Record intent, architecture decisions and pitfalls rather than volatile code details. Identical content in l1 is deduplicated by hash. `files` anchors the entry to relative paths so stale-memory warnings can fire when those files change; `supersedes` retires older entries this one replaces; `validTo` marks a fact that only holds until a date.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "content": { "type": "string", "description": "Markdown body." },
                    "title": { "type": "string", "description": "Omit to use the first line of the body." },
                    "type": { "type": "string", "description": "daily / decision / knowledge / preference / pitfall / profile / insight / other." },
                    "layer": { "type": "string", "description": "l1 or l2. Omit to infer from type: decision/knowledge/insight/profile -> l2, others -> l1." },
                    "project": { "type": "string", "description": "Project slug. Omit for a global entry." },
                    "tags": { "type": "array", "items": { "type": "string" } },
                    "importance": { "type": "number", "description": "0.0-1.0, default 0.5." },
                    "files": { "type": "array", "items": { "type": "string" }, "description": "Related relative file paths." },
                    "supersedes": { "type": "array", "items": { "type": "string" }, "description": "Ids this entry replaces." },
                    "validTo": { "type": "number", "description": "Epoch milliseconds after which this entry no longer holds." },
                    "evidence": { "type": "array", "items": { "type": "string" }, "description": "Evidence anchors: entry ids or `path#hash` references that support this conclusion." },
                    "pinned": { "type": "boolean" },
                    "starred": { "type": "boolean" }
                },
                "required": ["content"],
                "additionalProperties": false
            },
            "annotations": { "readOnlyHint": false, "destructiveHint": false, "idempotentHint": false, "openWorldHint": false }
        }),
        json!({
            "name": "memory_update",
            "title": "更新记忆",
            "description": "Update an existing entry's title, body, tags, category, layer, importance, pin or validity. Prefer writing a new entry with `supersedes` when the fact itself changed.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "id": { "type": "string" },
                    "title": { "type": "string" },
                    "body": { "type": "string" },
                    "type": { "type": "string" },
                    "layer": { "type": "string" },
                    "tags": { "type": "array", "items": { "type": "string" } },
                    "importance": { "type": "number" },
                    "pinned": { "type": "boolean" },
                    "starred": { "type": "boolean" },
                    "validTo": { "type": "number", "description": "0 or null clears the expiry." },
                    "evidence": { "type": "array", "items": { "type": "string" } }
                },
                "required": ["id"],
                "additionalProperties": false
            },
            "annotations": { "readOnlyHint": false, "destructiveHint": false, "idempotentHint": true, "openWorldHint": false }
        }),
        json!({
            "name": "memory_recent",
            "title": "最近记忆",
            "description": "Entries updated within the last N days (default 7), newest first.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "project": { "type": "string" },
                    "agent": { "type": "string" },
                    "days": { "type": "number", "description": "Default 7." }
                },
                "additionalProperties": false
            },
            "annotations": { "readOnlyHint": true, "destructiveHint": false, "idempotentHint": true, "openWorldHint": false }
        }),
        json!({
            "name": "memory_projects",
            "title": "项目列表",
            "description": "List projects with entry counts split by layer (l1 raw / l2 distilled), latest update and git remote.",
            "inputSchema": { "type": "object", "properties": {}, "additionalProperties": false },
            "annotations": { "readOnlyHint": true, "destructiveHint": false, "idempotentHint": true, "openWorldHint": false }
        }),
        json!({
            "name": "memory_forget",
            "title": "删除记忆",
            "description": "Move an entry to the trash (recoverable). Inspect the store with memory_status first.",
            "inputSchema": {
                "type": "object",
                "properties": { "id": { "type": "string" } },
                "required": ["id"],
                "additionalProperties": false
            },
            "annotations": { "readOnlyHint": false, "destructiveHint": true, "idempotentHint": false, "openWorldHint": false }
        }),
        json!({
            "name": "memory_status",
            "title": "状态自检",
            "description": "Store self-check: file root, entry counts by layer, index state, review queue backlog and full-text availability.",
            "inputSchema": { "type": "object", "properties": {}, "additionalProperties": false },
            "annotations": { "readOnlyHint": true, "destructiveHint": false, "idempotentHint": true, "openWorldHint": false }
        }),
        json!({
            "name": "memory_review_list",
            "title": "待确认建议",
            "description": "List pending suggestions the store wants a human decision on: duplicate candidates, project classification guesses, facts that look superseded. Read-only; nothing has been applied yet.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "type": { "type": "string", "description": "dedup / classify / supersede. Omit for all." },
                    "status": { "type": "string", "description": "pending (default) / approved / rejected / expired." },
                    "limit": { "type": "number", "description": "Default 50." }
                },
                "additionalProperties": false
            },
            "annotations": { "readOnlyHint": true, "destructiveHint": false, "idempotentHint": true, "openWorldHint": false }
        }),
        json!({
            "name": "memory_review_resolve",
            "title": "裁决建议",
            "description": "Approve or reject one pending suggestion. Requires the user's explicit consent — do not call this on your own initiative. Approving a dedup marks the older entry superseded (recoverable from trash); approving a classify moves the entry to the suggested project.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "id": { "type": "number", "description": "Review item id from memory_review_list." },
                    "approve": { "type": "boolean", "description": "true applies the suggestion, false discards it." }
                },
                "required": ["id", "approve"],
                "additionalProperties": false
            },
            "annotations": { "readOnlyHint": false, "destructiveHint": false, "idempotentHint": true, "openWorldHint": false }
        }),
        json!({
            "name": "memory_health",
            "title": "记忆体检",
            "description": "Report entries whose anchored files changed since the entry was written (memory may be stale), plus queue backlog and index drift. Call when the user asks whether the memory is still accurate.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "limit": { "type": "number", "description": "Max stale entries to list, default 20." }
                },
                "additionalProperties": false
            },
            "annotations": { "readOnlyHint": true, "destructiveHint": false, "idempotentHint": true, "openWorldHint": false }
        }),
        json!({
            "name": "memory_profile",
            "title": "长期画像",
            "description": "Read the distilled long-term profile (working style, recurring preferences, standing constraints) with the evidence each statement rests on. Read-only: the profile is generated by the automation layer, not by tool calls. Use it to tailor answers instead of re-deriving preferences from individual entries.",
            "inputSchema": {
                "type": "object",
                "properties": {},
                "additionalProperties": false
            },
            "annotations": { "readOnlyHint": true, "destructiveHint": false, "idempotentHint": true, "openWorldHint": false }
        }),
        json!({
            "name": "memory_sync_status",
            "title": "同步状态",
            "description": "Report the multi-device sync state: whether WebDAV is configured, this device id, file counts, pending conflicts and the last sync result. Read-only — it never triggers a sync. Call when the user asks whether their memory is synced or why another machine is missing entries.",
            "inputSchema": {
                "type": "object",
                "properties": {},
                "additionalProperties": false
            },
            "annotations": { "readOnlyHint": true, "destructiveHint": false, "idempotentHint": true, "openWorldHint": false }
        }),
    ]
}

pub fn spec() -> McpSpec {
    McpSpec {
        server_info: json!({
            "name": super::package_name(super::MEMORY_SERVICE_ID),
            "title": "Project Memory",
            "version": super::MEMORY_VERSION,
        }),
        instructions: INSTRUCTIONS,
        tools: spec_tools(),
    }
}

/// 工具调用的两类失败。业务失败交给模型自我纠正（`isError` 结果）；
/// 只有"工具名不认识"才是协议层的 InvalidParams。
pub enum CallError {
    UnknownTool,
    Failed(String),
}

fn text_arg(args: &Value, key: &str) -> Option<String> {
    args.get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
}

fn number_arg(args: &Value, key: &str) -> Option<usize> {
    args.get(key).and_then(Value::as_u64).map(|value| value as usize)
}

fn bool_arg(args: &Value, key: &str) -> Option<bool> {
    args.get(key).and_then(Value::as_bool)
}

/// 按 token 预算从尾部丢行（配置项 `agents.searchMaxTokens`）。
///
/// ── 为什么按「整行」而不是按字符截断 ────────────────────────
/// `memory_search` 返回的是索引行（`[决策] 标题 · 项目 · 3 天前 · \`id\``）。
/// 把某一行砍一半，模型拿到的 id 就成了残串，`memory_get` 直接找不到 ——
/// 那比不给它更糟。所以宁可少给几行，每行都必须是完整可用的。
///
/// ── 为什么至少留一行 ──────────────────────────────────────
/// 预算配得过小（例如 200）时，第一行就可能超。返回空数组等于「没搜到」，
/// 模型会去重试别的查询；留最相关的那一行，它至少知道该 `memory_get` 哪条。
fn trim_to_budget(lines: Vec<Value>, budget: usize) -> (Vec<Value>, usize) {
    let total = lines.len();
    if budget == 0 || total == 0 {
        return (lines, 0);
    }
    let mut used = 0usize;
    let mut kept = 0usize;
    for line in &lines {
        let text = line.as_str().unwrap_or("");
        // +1 是行尾换行：拼成整块文本时每行都带一个分隔符，不记进去会低估
        let cost = memory::model::estimate_tokens(text) + 1;
        if kept > 0 && used + cost > budget {
            break;
        }
        used += cost;
        kept += 1;
    }
    if kept == total {
        return (lines, 0);
    }
    (lines.into_iter().take(kept).collect(), total - kept)
}

fn list_arg(args: &Value, key: &str) -> Vec<String> {
    args.get(key)
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(|item| item.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default()
}

/// 执行一个记忆工具，返回给模型的 payload。
///
/// `agent` 是「这次调用来自哪个客户端」（网关从安装时写入的 `X-Agent` 头读出来，
/// 见 `McpRequestContext::agent`）。它做两件事：
///   1. 决定 `memory_write` 落到 `l1/<agent>/…` 的哪个目录 —— 没有它时所有
///      客户端共用 `l1/mcp/`，事后分不清哪条是哪个客户端写的；
///   2. 落一条 `agent_beat` 心跳，接入页的「最近活跃」才有内容。
pub fn call(name: &str, args: &Value, agent: Option<&str>) -> Result<Value, CallError> {
    let outcome = dispatch(name, args, agent);
    // 心跳只在**协议层认识这个工具**时记：`UnknownTool` 说明客户端在用旧工具名，
    // 把它记成「活跃」会掩盖版本不匹配。
    //
    // 只读工具不心跳吗？—— 也记。接入页要回答的是「这个客户端最近有没有在
    // 用记忆」，只读调用（开局 `memory_core`）正是最可靠的活跃证据。
    //
    // 两个 `matches!` 都按**引用**匹配：`matches!(x, …)` 展开成 `match x`，
    // 按值匹配会把 `outcome` move 走，后面那次判断与最后 `outcome` 的返回
    // 就都拿不到它了（E0382）。
    if !matches!(&outcome, Err(CallError::UnknownTool)) {
        let identity = agent_slug(agent);
        crate::server::core::memory::index::global().beat(
            &identity,
            name,
            matches!(&outcome, Ok(_)),
        );
    }
    outcome
}

/// 客户端身份的归一化：只留 ASCII 字母数字与 `-` `_`，压到 64 字节以内。
///
/// 这个词会直接成为目录名（`l1/<agent>/`），所以必须是路径安全的。两条硬规则：
///   · **`/` `\` 必须挡掉** —— 否则一次写入能在记忆根下造出两级目录；
///   · **`.` 也挡掉** —— 唯一危险值是 `".."`：`l1/../<date>.md` 会跳出记忆根，
///     而 `".."` 恰好是「只由允许字符组成」的合法串，光靠白名单拦不住。
///     各家客户端 id（`claude-code` / `gemini-cli` / `kimi-code` …）都不带点，
///     去掉它不损失任何真实身份。
///
/// 清洗后若一个字母数字都不剩（全空白 / 全符号），回落到历史值 `mcp` ——
/// 宁可不分类，也不要造出一个 `_` 或 `-` 的目录。
fn agent_slug(agent: Option<&str>) -> String {
    let raw = agent.map(str::trim).unwrap_or("");
    let cleaned: String = raw
        .chars()
        .map(|ch| if ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_') { ch } else { '_' })
        .take(64)
        .collect();
    if !cleaned.chars().any(|ch| ch.is_ascii_alphanumeric()) {
        "mcp".to_string()
    } else {
        cleaned
    }
}

fn dispatch(name: &str, args: &Value, agent: Option<&str>) -> Result<Value, CallError> {
    // 总开关只管"业务工具"：`memory_status` / `memory_health` / `memory_sync_status`
    // 必须始终可用 —— 否则关掉之后用户连"为什么 Agent 一点记忆都没有"都查不了
    // （这正是最需要自检的时刻）。
    let always_available = matches!(name, "memory_status" | "memory_health" | "memory_sync_status");
    if !always_available && !memory::config().enabled {
        return Err(CallError::Failed(
            "记忆中枢已停用：在「记忆中枢 → 设置」里重新启用后再试".into(),
        ));
    }
    match name {
        "memory_core" => {
            let project = text_arg(args, "project");
            let budget = number_arg(args, "maxTokens").unwrap_or_else(|| memory::config().core_max_tokens);
            // 带上客户端身份：`recall_log` 里能按 agent 归因，接入页的
            // 「最近有没有在读记忆」才有第二份独立证据（第一份是 agent_beat）。
            Ok(memory::core_text_for(project.as_deref(), budget, Some(&agent_slug(agent))))
        }
        "memory_digest" => {
            // 不传 `maxLines` 时用配置项 `agents.digestMaxLines`（默认 200）。
            // 以前这里写死 200，设置页那条旋钮改了没有任何效果 —— 界面承诺与
            // 行为必须同源，否则用户会以为自己调过、其实没生效。
            let lines = number_arg(args, "maxLines")
                .unwrap_or_else(|| memory::config().digest_max_lines);
            Ok(memory::digest_text(lines))
        }
        "memory_search" => {
            let query = text_arg(args, "query").unwrap_or_default();
            if query.is_empty() {
                return Err(CallError::Failed("query 不能为空".into()));
            }
            let settings = memory::config();
            let request = MemoryQuery {
                text: query,
                project: text_arg(args, "project"),
                // `agent` 是**过滤条件**（只看某个客户端写过的记忆），不是调用方身份。
                // 归因走 `list_for()` 的第二个参数 —— 早先想在这里兜底成 `X-Agent`，
                // 那会把「搜全部」悄悄改成「只搜我自己写的」，导入来的记忆直接搜不到。
                agent: text_arg(args, "agent"),
                kind: text_arg(args, "type"),
                layer: text_arg(args, "layer"),
                tag: text_arg(args, "tag"),
                starred: bool_arg(args, "starred"),
                pinned: bool_arg(args, "pinned"),
                include_superseded: bool_arg(args, "includeSuperseded").unwrap_or(false),
                include_deleted: false,
                include_expired: bool_arg(args, "includeExpired").unwrap_or(false),
                // 不传 limit 时用 `search.finalTopK`（默认 8）。以前写死 30，
                // 那条「最终返回条数」旋钮同样是死键。
                limit: number_arg(args, "limit").unwrap_or(settings.final_top_k),
                // MCP 侧不分页：`memory_search` 一次给模型一屏最相关的就够，
                // 让它翻页只会多烧几轮上下文。
                offset: 0,
                // 时间窗是界面热力图用的（点某天看当天清单），MCP 侧不暴露。
                created_from: None,
                created_to: None,
                // 候选召回上限 `search.recallTopK`：只在这里设，界面列表传 None
                // （界面要用真实总数算页数）。
                recall: Some(settings.recall_top_k),
            };
            let page = memory::list_for(&request, Some(&agent_slug(agent)));
            let all = page
                .get("lines")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();
            let count = page.get("total").and_then(Value::as_u64).unwrap_or(0);
            let (lines, dropped) = trim_to_budget(all, settings.search_max_tokens);
            Ok(json!({
                "count": count,
                "lines": lines,
                // 因为 token 预算被砍掉的条数。模型看到这个数就知道
                // 「还有东西没给你」，该收窄条件或改调 memory_get，
                // 而不是以为这就是全部。
                "dropped": dropped,
                "budget": settings.search_max_tokens,
                // 空结果是正常状态，不是错误：告诉模型「没有记忆」，
                // 比让它以为自己调错参数再重试三次要好
                "note": if count == 0 { "没有匹配的记忆；可用 memory_digest 看看有哪些项目。" } else if dropped > 0 { "结果超出 token 预算，已从尾部丢弃；收窄条件或对具体 id 调 memory_get。" } else { "" },
            }))
        }
        "memory_get" => {
            let mut ids = list_arg(args, "ids");
            if ids.is_empty() {
                if let Some(id) = text_arg(args, "id") {
                    ids.push(id);
                }
            }
            if ids.is_empty() {
                return Err(CallError::Failed("缺少 id".into()));
            }
            Ok(memory::get_for(&ids, MAX_BODY_CHARS, Some(&agent_slug(agent))))
        }
        "memory_timeline" => {
            let mut id = text_arg(args, "id");
            if id.is_none() {
                if let Some(topic) = text_arg(args, "topic") {
                    let request = MemoryQuery { text: topic, limit: 1, ..Default::default() };
                    id = memory::list(&request)
                        .get("items")
                        .and_then(Value::as_array)
                        .and_then(|items| items.first())
                        .and_then(|item| item.get("id"))
                        .and_then(Value::as_str)
                        .map(str::to_string);
                }
            }
            let Some(id) = id else {
                return Err(CallError::Failed("缺少 id 或 topic".into()));
            };
            Ok(memory::timeline(&id))
        }
        "memory_write" => {
            let content = text_arg(args, "content")
                .ok_or_else(|| CallError::Failed("content 不能为空".into()))?;
            let kind = text_arg(args, "type").unwrap_or_else(|| "daily".to_string());
            if !MemoryKind::ALL.iter().any(|item| item.as_str() == kind) {
                return Err(CallError::Failed(format!(
                    "type 无效：{kind}（可选 {}）",
                    MemoryKind::ALL.iter().map(|item| item.as_str()).collect::<Vec<_>>().join(" / ")
                )));
            }
            if let Some(layer) = text_arg(args, "layer") {
                if !matches!(layer.as_str(), "l1" | "l2") {
                    return Err(CallError::Failed(format!("layer 无效：{layer}（可选 l1 / l2）")));
                }
            }
            let input = memory::WriteInput {
                content,
                title: text_arg(args, "title"),
                kind: Some(kind),
                layer: text_arg(args, "layer"),
                project: text_arg(args, "project"),
                // 身份优先取调用方带来的客户端 slug（决定 `l1/<agent>/` 目录与
                // 心跳归因）；取不到时才退回 `mcp` 这个历史值。
                agent: Some(agent_slug(agent)),
                tags: list_arg(args, "tags"),
                importance: args.get("importance").and_then(Value::as_f64),
                files: list_arg(args, "files"),
                supersedes: list_arg(args, "supersedes"),
                pinned: bool_arg(args, "pinned"),
                starred: bool_arg(args, "starred"),
                // MCP 客户端不传 cwd：项目归属以显式 `project` 为准 ——
                // Agent 知道自己在哪个目录，让它自己给 slug 比网关猜更准。
                cwd: None,
                session: text_arg(args, "session"),
                // 历史导入/补录场景：允许 MCP 调用方带原始发生时刻
                created_at: args.get("createdAt").and_then(Value::as_i64).filter(|stamp| *stamp > 0),
                valid_to: args.get("validTo").and_then(Value::as_i64).filter(|stamp| *stamp > 0),
                evidence: list_arg(args, "evidence"),
            };
            memory::write(input).map_err(CallError::Failed)
        }
        "memory_update" => {
            let id = text_arg(args, "id").ok_or_else(|| CallError::Failed("缺少 id".into()))?;
            memory::update(&id, args).map_err(CallError::Failed)
        }
        "memory_recent" => Ok(memory::recent_for(
            text_arg(args, "project").as_deref(),
            text_arg(args, "agent").as_deref(),
            number_arg(args, "days").unwrap_or(7),
            Some(&agent_slug(agent)),
        )),
        "memory_projects" => Ok(memory::projects()),
        "memory_forget" => {
            let id = text_arg(args, "id").ok_or_else(|| CallError::Failed("缺少 id".into()))?;
            memory::remove(&id).map_err(CallError::Failed)
        }
        "memory_status" => Ok(memory::status()),
        "memory_review_list" => {
            let status = text_arg(args, "status").unwrap_or_else(|| "pending".to_string());
            let kind = text_arg(args, "type");
            Ok(memory::review_list(
                Some(status.as_str()),
                kind.as_deref(),
                number_arg(args, "limit").unwrap_or(50),
            ))
        }
        "memory_review_resolve" => {
            let id = args
                .get("id")
                .and_then(Value::as_i64)
                .ok_or_else(|| CallError::Failed("缺少 id".into()))?;
            let approve = bool_arg(args, "approve")
                .ok_or_else(|| CallError::Failed("缺少 approve".into()))?;
            memory::review_resolve(id, approve).map_err(CallError::Failed)
        }
        "memory_health" => Ok(memory::health(number_arg(args, "limit").unwrap_or(20))),
        "memory_profile" => Ok(memory::profile_view()),
        "memory_sync_status" => Ok(memory::sync::status()),
        _ => Err(CallError::UnknownTool),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spec_exposes_sixteen_tools() {
        // 必须与 tool_services::CAPABILITY_COUNTS 里登记的一致，
        // 否则界面上「N 个工具」与实际差一个
        assert_eq!(spec().tools.len(), 16);
    }

    #[test]
    fn every_tool_has_a_name_and_object_schema() {
        for tool in spec().tools {
            let name = tool.get("name").and_then(Value::as_str).unwrap_or("");
            assert!(!name.is_empty(), "工具缺少 name");
            assert!(name.starts_with("memory_"), "{name} 命名不符");
            assert_eq!(
                tool.get("inputSchema").and_then(|schema| schema.get("type")).and_then(Value::as_str),
                Some("object")
            );
        }
    }

    #[test]
    fn unknown_tool_is_protocol_error() {
        assert!(matches!(call("memory_nope", &json!({}), None), Err(CallError::UnknownTool)));
    }

    #[test]
    fn search_requires_query() {
        assert!(matches!(call("memory_search", &json!({}), None), Err(CallError::Failed(_))));
    }

    #[test]
    fn write_rejects_unknown_type() {
        let args = json!({ "content": "x", "type": "nonsense" });
        assert!(matches!(call("memory_write", &args, None), Err(CallError::Failed(_))));
    }

    #[test]
    fn write_rejects_unknown_layer() {
        let args = json!({ "content": "x", "layer": "l3" });
        assert!(matches!(call("memory_write", &args, None), Err(CallError::Failed(_))));
    }

    #[test]
    fn instructions_state_that_disk_wins() {
        // 这条是「记忆不得覆盖当前代码」的唯一护栏，措辞被改掉护栏就没了
        let text = spec().instructions.to_lowercase();
        assert!(text.contains("code on disk"));
        assert!(text.contains("never revert"));
    }

    #[test]
    fn instructions_forbid_self_approving_reviews() {
        // 模型不该自己批准「这条记忆作废」
        let text = spec().instructions.to_lowercase();
        assert!(text.contains("never approve review items yourself"));
    }

    #[test]
    fn agent_slug_is_path_safe_and_falls_back_to_mcp() {
        // 这个词会直接成为 `l1/<agent>/` 的目录名：斜杠、空格、中文必须被挡掉，
        // 否则一次写入能在记忆根下造出两级目录
        assert_eq!(agent_slug(Some("claude-code")), "claude-code");
        assert_eq!(agent_slug(Some("a/b c")), "a_b_c");
        assert_eq!(agent_slug(Some("trae/ide")), "trae_ide");
        assert_eq!(agent_slug(Some("   ")), "mcp");
        assert_eq!(agent_slug(None), "mcp");
        // 上限 64：与 AgentHub 桥的 `slice(0, 64)` 同口径
        let long = "a".repeat(200);
        assert_eq!(agent_slug(Some(long.as_str())).len(), 64);
    }

    /// `..` 是唯一能跳出 `l1/` 的目录名，而且它只由「合法字符」组成 ——
    /// 白名单里留一个 `.` 就等于把 `l1/../<date>.md` 这个入口留开着。
    #[test]
    fn agent_slug_cannot_escape_the_l1_directory() {
        assert_eq!(agent_slug(Some("..")), "mcp");
        assert_eq!(agent_slug(Some(".")), "mcp");
        assert_eq!(agent_slug(Some("../..")), "mcp");
        // 中间带点的真实 id 也要落成安全名（不允许出现 `.` 段）
        assert_eq!(agent_slug(Some("a.b")), "a_b");
        // 纯符号同样不给造目录的机会
        assert_eq!(agent_slug(Some("---")), "mcp");
        assert_eq!(agent_slug(Some("___")), "mcp");
    }

    /// 超预算时按**整行**丢尾部，且至少留一行。
    ///
    /// ── 为什么「至少留一行」要单独钉住 ──────────────────────────
    /// 它是这条逻辑里唯一一个会让返回值看起来「不守预算」的分支：预算 200、
    /// 首行就要 300 token 时，我们仍然返回那一行。看起来像 bug，其实是
    /// 刻意的 —— 返回空数组等于告诉模型「没搜到」，它会换个词重试，白烧一轮
    /// 上下文；留一行，它至少知道该对哪条 id 调 `memory_get`。
    #[test]
    fn trim_to_budget_drops_whole_lines_and_keeps_at_least_one() {
        let long = "长".repeat(400); // 400 个 CJK 字符 ≈ 400 token
        let lines = vec![json!(long.clone()), json!(long.clone()), json!(long)];
        // 预算足够：原样返回，不报丢弃
        let (kept, dropped) = trim_to_budget(lines.clone(), 100_000);
        assert_eq!(kept.len(), 3);
        assert_eq!(dropped, 0);
        // 预算只够一行：留第一行，其余算丢弃（而不是全部丢掉）
        let (kept, dropped) = trim_to_budget(lines, 500);
        assert_eq!(kept.len(), 1, "至少保留最相关的一行");
        assert_eq!(dropped, 2);
        // 预算极小（连一行都放不下）：仍然留一行
        let (kept, dropped) = trim_to_budget(vec![json!(long)], 10);
        assert_eq!(kept.len(), 1);
        assert_eq!(dropped, 0);
    }

    /// 预算为 0 表示「不限制」，不是「全部丢掉」。
    ///
    /// 配置项被写成 0 时 `usize_of` 返回 0；若把它当成「上限 0」，
    /// 一次误配置就会让 `memory_search` 永远返回空 —— 而模型无法区分
    /// 「库里没有」和「配置坏了」。
    #[test]
    fn trim_to_budget_treats_zero_as_unlimited() {
        let lines = vec![json!("a"), json!("b")];
        let (kept, dropped) = trim_to_budget(lines, 0);
        assert_eq!(kept.len(), 2);
        assert_eq!(dropped, 0);
    }
}
