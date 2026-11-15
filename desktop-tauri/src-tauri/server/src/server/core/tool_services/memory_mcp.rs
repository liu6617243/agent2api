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

use serde_json::{json, Value};

use super::mcp::McpSpec;
use crate::server::core::memory::{self, MemoryKind, Query as MemoryQuery};

/// 给模型的固定行为约定（`initialize.instructions`）。
///
/// 三条：开局读一次核心记忆、提到「之前/上次」时检索、做完决策写回去。
/// 另加一条硬约束「磁盘代码是唯一现实」—— 记忆是历史陈述，不是当前状态；
/// 没有这句，模型会拿一周前的记忆去覆盖刚改过的代码。
pub const INSTRUCTIONS: &str = "Local project memory (markdown on this machine, one file per entry). \
Call memory_core once at the start of a session to load the long-term picture; \
call memory_search when the user refers to earlier work or an earlier decision, and memory_get to read an entry in full. \
After finishing a task or making a decision, call memory_write with the reasoning, the trade-offs and the pitfalls — not the volatile code details. \
Use `supersedes` to retire an entry that is no longer true, and `files` to anchor entries to relative file paths. \
The code on disk is the only current truth: memory is history, intent and warnings, never a snapshot of the present. \
Read the actual files before editing them, and never revert working code back to an older memory.";

/// 单条正文上限：MCP 结果会被整段塞进上下文，超长正文留给 `memory_get`。
const MAX_BODY_CHARS: usize = 6000;

fn spec_tools() -> Vec<Value> {
    vec![
        json!({
            "name": "memory_core",
            "title": "核心记忆",
            "description": "Load the long-term picture: user profile plus per-project highlights (tier 1 of the disclosure ladder, default ~800 tokens). Call once at session start. Response carries `truncated`; when truncated, use memory_search or memory_digest for more.",
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
            "description": "Search memory and return compact index lines (titles and metadata, not full text). Superseded and deleted entries are excluded unless asked for. Ranking blends relevance, importance, pinning and recency.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "query": { "type": "string", "description": "Keywords. CJK is matched character by character, so short Chinese queries work." },
                    "project": { "type": "string" },
                    "agent": { "type": "string" },
                    "type": { "type": "string", "description": "Exact category: daily / decision / knowledge / preference / pitfall / profile / other." },
                    "tag": { "type": "string", "description": "Single tag, substring match." },
                    "starred": { "type": "boolean" },
                    "pinned": { "type": "boolean" },
                    "limit": { "type": "number", "description": "Default 30, max 500." },
                    "includeSuperseded": { "type": "boolean", "description": "Also return entries replaced by a newer one." }
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
            "description": "Write one memory. Record intent, architecture decisions and pitfalls rather than volatile code details. `files` anchors the entry to relative paths so stale-memory warnings can fire when those files change; `supersedes` retires older entries this one replaces.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "content": { "type": "string", "description": "Markdown body." },
                    "title": { "type": "string", "description": "Omit to use the first line of the body." },
                    "type": { "type": "string", "description": "daily / decision / knowledge / preference / pitfall / profile / other." },
                    "project": { "type": "string", "description": "Project slug. Omit for a global entry." },
                    "tags": { "type": "array", "items": { "type": "string" } },
                    "importance": { "type": "number", "description": "0.0-1.0, default 0.5." },
                    "files": { "type": "array", "items": { "type": "string" }, "description": "Related relative file paths." },
                    "supersedes": { "type": "array", "items": { "type": "string" }, "description": "Ids this entry replaces." },
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
            "description": "Update an existing entry's title, body, tags, category or importance. Prefer writing a new entry with `supersedes` when the fact itself changed.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "id": { "type": "string" },
                    "title": { "type": "string" },
                    "body": { "type": "string" },
                    "type": { "type": "string" },
                    "tags": { "type": "array", "items": { "type": "string" } },
                    "importance": { "type": "number" },
                    "pinned": { "type": "boolean" },
                    "starred": { "type": "boolean" }
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
            "description": "List projects with entry counts, latest update and git remote.",
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
            "description": "Store self-check: file root, entry counts, index state and whether full-text search is available.",
            "inputSchema": { "type": "object", "properties": {}, "additionalProperties": false },
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
pub fn call(name: &str, args: &Value) -> Result<Value, CallError> {
    // 总开关只管"业务工具"：`memory_status` 必须始终可用 —— 否则关掉之后
    // 用户连"为什么 Agent 一点记忆都没有"都查不了（这正是最需要自检的时刻）。
    if name != "memory_status" && !memory::config().enabled {
        return Err(CallError::Failed(
            "记忆中枢已停用：在「记忆中枢 → 设置」里重新启用后再试".into(),
        ));
    }
    match name {
        "memory_core" => {
            let project = text_arg(args, "project");
            let budget = number_arg(args, "maxTokens").unwrap_or_else(|| memory::config().core_max_tokens);
            Ok(memory::core_text(project.as_deref(), budget))
        }
        "memory_digest" => Ok(memory::digest_text(number_arg(args, "maxLines").unwrap_or(200))),
        "memory_search" => {
            let query = text_arg(args, "query").unwrap_or_default();
            if query.is_empty() {
                return Err(CallError::Failed("query 不能为空".into()));
            }
            let request = MemoryQuery {
                text: query,
                project: text_arg(args, "project"),
                agent: text_arg(args, "agent"),
                kind: text_arg(args, "type"),
                tag: text_arg(args, "tag"),
                starred: bool_arg(args, "starred"),
                pinned: bool_arg(args, "pinned"),
                include_superseded: bool_arg(args, "includeSuperseded").unwrap_or(false),
                include_deleted: false,
                limit: number_arg(args, "limit").unwrap_or(30),
            };
            let page = memory::list(&request);
            let lines = page.get("lines").cloned().unwrap_or_else(|| json!([]));
            let count = page.get("total").and_then(Value::as_u64).unwrap_or(0);
            Ok(json!({
                "count": count,
                "lines": lines,
                // 空结果是正常状态，不是错误：告诉模型「没有记忆」，
                // 比让它以为自己调错参数再重试三次要好
                "note": if count == 0 { "没有匹配的记忆；可用 memory_digest 看看有哪些项目。" } else { "" },
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
            Ok(memory::get(&ids, MAX_BODY_CHARS))
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
            let input = memory::WriteInput {
                content,
                title: text_arg(args, "title"),
                kind: Some(kind),
                project: text_arg(args, "project"),
                agent: Some("mcp".to_string()),
                tags: list_arg(args, "tags"),
                importance: args.get("importance").and_then(Value::as_f64),
                files: list_arg(args, "files"),
                supersedes: list_arg(args, "supersedes"),
                pinned: bool_arg(args, "pinned"),
                starred: bool_arg(args, "starred"),
                // MCP 客户端不传 cwd：项目归属以显式 `project` 为准 ——
                // Agent 知道自己在哪个目录，让它自己给 slug 比网关猜更准。
                cwd: None,
            };
            memory::write(input).map_err(CallError::Failed)
        }
        "memory_update" => {
            let id = text_arg(args, "id").ok_or_else(|| CallError::Failed("缺少 id".into()))?;
            memory::update(&id, args).map_err(CallError::Failed)
        }
        "memory_recent" => Ok(memory::recent(
            text_arg(args, "project").as_deref(),
            text_arg(args, "agent").as_deref(),
            number_arg(args, "days").unwrap_or(7),
        )),
        "memory_projects" => Ok(memory::projects()),
        "memory_forget" => {
            let id = text_arg(args, "id").ok_or_else(|| CallError::Failed("缺少 id".into()))?;
            memory::remove(&id).map_err(CallError::Failed)
        }
        "memory_status" => Ok(memory::status()),
        _ => Err(CallError::UnknownTool),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spec_exposes_eleven_tools() {
        // 必须与 tool_services::CAPABILITY_COUNTS 里登记的 11 一致，
        // 否则界面上「11 个工具」与实际差一个
        assert_eq!(spec().tools.len(), 11);
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
        assert!(matches!(call("memory_nope", &json!({})), Err(CallError::UnknownTool)));
    }

    #[test]
    fn search_requires_query() {
        assert!(matches!(call("memory_search", &json!({})), Err(CallError::Failed(_))));
    }

    #[test]
    fn write_rejects_unknown_type() {
        let args = json!({ "content": "x", "type": "nonsense" });
        assert!(matches!(call("memory_write", &args), Err(CallError::Failed(_))));
    }

    #[test]
    fn instructions_state_that_disk_wins() {
        // 这条是「记忆不得覆盖当前代码」的唯一护栏，措辞被改掉护栏就没了
        let text = spec().instructions.to_lowercase();
        assert!(text.contains("code on disk"));
        assert!(text.contains("never revert"));
    }
}
