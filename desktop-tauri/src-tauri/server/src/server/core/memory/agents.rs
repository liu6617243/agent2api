//! 记忆中枢 · Agent 接入与迁移。
//!
//! ── 这个文件解决什么 ────────────────────────────────────────
//! 记忆存好了，但 Agent 不知道自己该调哪些工具、也不知道本机装了哪些客户端。
//! 这一层做三件事：
//!   1. **列出本机客户端**：哪些装了、配置文件在哪、记忆 MCP 有没有写进去；
//!   2. **一键接入**：往客户端的 MCP 配置里写一条 `agent2api-memory`，再往它的
//!      提示词文件里写一段受控块（告诉模型什么时候该读、该写记忆）；
//!   3. **一键卸载**：只删自己那一条 / 那一块，用户其它配置原样不动。
//!
//! ── 为什么复用 `agents::resources` 而不是自己写文件 ──────────
//! 「Agent 配置」页已经有一套成熟的客户端路径表（JSON / JSONC / TOML / YAML
//! 四种格式的读写器、原子写、写入前备份、版本号防并发覆盖）。记忆接入要做的事
//! 与「工具服务 → 本机接入」完全同构，另写一套只会让两边对同一个客户端的格式
//! 判断慢慢漂移。所以这里只负责「写什么」，不负责「怎么写」。
//!
//! ── 为什么提示词块要带标记 ──────────────────────────────────
//! 用户的 `AGENTS.md` / `CLAUDE.md` 是他自己写的东西。没有标记就既做不到「只改
//! 我这一块」，也做不到「干净卸载」—— 卸载时只能整份覆盖或整份不删，两种都
//! 会让用户丢掉自己的内容。标记块是唯一能同时满足幂等写入与无损卸载的形态。
//!
//! ── 为什么 instructions 已经有一份，还要写提示词块 ──────────
//! MCP `initialize.instructions` 是协议内的正规通道，但**客户端可以不理它**
//! （很多客户端只把它当展示文本）。提示词文件是客户端一定会读的。两处都写：
//! 支持 instructions 的客户端拿到结构化约定，不支持的靠文件兜底。

use serde_json::{json, Value};

// 别名叫 `hub`：本文件本身就在 `memory::agents` 里，再引入一个同名 `agents`
// 会让 `agents::CLIENTS` 到底指哪一层变得靠猜。
use crate::server::core::agents as hub;
use crate::server::core::agents::resources;
use crate::server::core::tool_services;

use super::config;
use super::index;

/// 写进客户端 MCP 配置的键名（与 `tool_services::package_name("memory")` 一致）。
pub fn server_name() -> &'static str {
    tool_services::package_name(tool_services::MEMORY_SERVICE_ID)
}

/// 受控块的起止标记。
const BLOCK_BEGIN: &str = "<!-- agent2api-memory:begin -->";
const BLOCK_END: &str = "<!-- agent2api-memory:end -->";

/// 写进提示词文件的内容。
///
/// 只讲「什么时候调哪个工具」，不讲工具参数（参数在 MCP schema 里，重复一遍
/// 就会两边漂移）。最后一条是硬约束：记忆是历史陈述，磁盘代码才是当前状态 ——
/// 少了这句，模型会拿一周前的记忆去「修复」刚改对的代码。
///
/// 用 raw string 而不是 `\` 续行：续行会把换行也吃掉，整块会挤成一行，
/// 写进 `AGENTS.md` 之后 markdown 结构全没了。
const INSTRUCTION_BLOCK: &str = r#"<!-- agent2api-memory:begin -->
## 记忆中枢（Agent2API · 本机项目记忆）

- 会话开始时先调 `memory_core` 读一次长期记忆；用户提到「之前 / 上次 / 这个项目怎么定的」时，先 `memory_search`，需要全文再用 `memory_get`。
- 完成任务或做出决策后，用 `memory_write` 记下**动机、取舍与踩坑**，不要堆砌易变的代码细节；能定性成 decision / knowledge / pitfall 的就别记成 daily。
- 旧结论被推翻时，用 `supersedes` 淘汰旧条目；只在一段时间内成立的事实，用 `validTo` 标注；用 `files` 锚定相关文件，便于触发过期提醒。
- **磁盘上的代码是唯一现实**：记忆只是历史、意图与提醒，绝不代表当前代码状态。改任何文件前先读它，永远不要依据旧记忆把代码改回去。
- `memory_review_*` 是给人做裁决的，不要自行批准「这条记忆作废」。
<!-- agent2api-memory:end -->"#;

// ── 客户端清单 ───────────────────────────────────────────────

/// 接入页的卡片列表。
///
/// 每张卡片回答四个问题：这个客户端装了吗、配置在哪、记忆 MCP 写进去了吗、
/// 提示词块写进去了吗。`supported=false` 的卡片（比如 DSH 还没选定运行方式）
/// 也要列出来 —— 用户需要知道「为什么这个不能点」，而不是从列表里凭空消失。
pub fn list() -> Value {
    let enabled = config::strings_of("agents.enabled");
    let beats = index::global().beats();
    let mut cards = Vec::new();

    for def in hub::CLIENTS {
        let detection = hub::paths::detect(def);
        let mcp_path = resources::mcp_config_path(def);
        let prompt_path = resources::snapshot(def, "prompt")
            .ok()
            .and_then(|snapshot| snapshot.get("path").and_then(Value::as_str).map(str::to_string));

        // 两个都没有的客户端（既写不了 MCP 也写不了提示词）不进卡片列表：
        // 卡片点了什么也做不了，只会让用户以为功能坏了。
        if mcp_path.is_none() && prompt_path.is_none() {
            continue;
        }

        let mcp = resources::snapshot(def, "mcp");
        let mcp_error = mcp.as_ref().err().cloned();
        let supported = mcp.is_ok();
        let installed = mcp
            .as_ref()
            .ok()
            .and_then(|snapshot| snapshot.get("servers").and_then(Value::as_array))
            .is_some_and(|servers| {
                servers
                    .iter()
                    .any(|server| server.get("name").and_then(Value::as_str) == Some(server_name()))
            });
        let prompt = prompt_path.as_deref().map(block_state);
        // `block_state` 里已有 path 之外的字段，卡片直接展开：
        // 前端不需要再自己拼路径，也就不用猜提示词文件叫什么。
        let instruction = match prompt {
            Some(state) => json!({
                "path": prompt_path,
                "exists": state.exists,
                "injected": state.injected,
                "broken": state.broken,
            }),
            None => Value::Null,
        };

        cards.push(json!({
            "id": def.id,
            "name": def.name,
            "vendor": def.vendor,
            "hue": def.hue,
            "letter": def.letter,
            "icon": def.icon,
            "iconFull": def.icon_full,
            "detected": detection.detected,
            "version": detection.version,
            "enabled": enabled.iter().any(|item| item.as_str() == def.id),
            "configPath": mcp_path.as_ref().map(|path| path.display().to_string()),
            "configExists": mcp_path.as_ref().is_some_and(|path| path.is_file()),
            "supported": supported,
            "note": mcp_error.clone().or(detection.note.clone()),
            "mcp": {
                "installed": installed,
                "serverName": server_name(),
            },
            "instruction": instruction,
            "heartbeat": beats
                .iter()
                .find(|beat| beat.get("agent").and_then(Value::as_str) == Some(def.id))
                .cloned()
                .unwrap_or(Value::Null),
        }));
    }

    // 自定义客户端：用户手填路径的兜底通道。不参与一键接入（格式未知），
    // 只把配置片段给他自己贴。
    let custom: Vec<Value> = config::strings_of("agents.custom")
        .into_iter()
        .map(|entry| json!({ "id": entry, "name": entry, "custom": true }))
        .collect();

    json!({
        "cards": cards,
        "custom": custom,
        "serverName": server_name(),
        "endpoint": tool_services::endpoint(tool_services::MEMORY_SERVICE_ID, "mcp"),
        "injectAgentsMd": config::bool_of("agents.injectAgentsMd"),
        "tools": tool_services::capability_count(tool_services::MEMORY_SERVICE_ID),
    })
}

struct BlockState {
    exists: bool,
    injected: bool,
    /// 起了标记但没收到结束标记 —— 文件被别的程序改坏了，不能再往里写
    broken: bool,
}

fn block_state(path: &str) -> BlockState {
    let text = std::fs::read_to_string(path).unwrap_or_default();
    let exists = std::path::Path::new(path).exists();
    let begin = text.find(BLOCK_BEGIN);
    let end = text.find(BLOCK_END);
    BlockState {
        exists,
        injected: begin.is_some() && end.is_some(),
        broken: begin.is_some() != end.is_some(),
    }
}

/// 把受控块拼进原文（已有则替换，没有则追加）。
///
/// 拼接接缝处只压「连续三个以上换行」到两个 —— 只动接缝，不动正文。整份
/// `replace("\n\n\n", "\n\n")` 会把用户代码块里刻意留的空行也吃掉。
fn inject_block(original: &str, block: &str) -> Result<String, String> {
    let begin = original.find(BLOCK_BEGIN);
    let end = original.find(BLOCK_END);
    match (begin, end) {
        (Some(start), Some(stop)) if stop > start => {
            let head = squeeze_tail(&original[..start]);
            let tail = squeeze_head(&original[stop + BLOCK_END.len()..]);
            Ok(format!("{head}{block}{tail}"))
        }
        // 只有一半标记：绝不追加。再写一个 begin 之后文件里就有两个起点，
        // 卸载时只能删到第一对，永远清不干净。
        (Some(_), _) | (_, Some(_)) => {
            Err("提示词文件里的记忆受控块标记不完整（只有一半），已拒绝写入以免产生重复块，请手动处理".into())
        }
        (None, None) => {
            if original.is_empty() {
                return Ok(format!("{block}\n"));
            }
            let sep = if original.ends_with("\n\n") {
                ""
            } else if original.ends_with('\n') {
                "\n"
            } else {
                "\n\n"
            };
            Ok(format!("{original}{sep}{block}\n"))
        }
    }
}

fn squeeze_tail(head: &str) -> String {
    let trimmed = head.trim_end_matches('\n');
    if trimmed.is_empty() {
        String::new()
    } else {
        format!("{trimmed}\n\n")
    }
}

fn squeeze_head(tail: &str) -> String {
    let trimmed = tail.trim_start_matches('\n');
    if trimmed.is_empty() {
        "\n".to_string()
    } else {
        format!("\n{trimmed}")
    }
}

fn remove_block(original: &str) -> Option<String> {
    let begin = original.find(BLOCK_BEGIN)?;
    let end = original.find(BLOCK_END)?;
    if end <= begin {
        return None;
    }
    let head = squeeze_tail(&original[..begin]);
    let tail = squeeze_head(&original[end + BLOCK_END.len()..]);
    let merged = format!("{head}{tail}");
    Some(merged.trim_end().to_string() + "\n")
}

// ── 接入 / 卸载 ──────────────────────────────────────────────

/// 一次接入的结果。
struct Outcome {
    id: String,
    label: String,
    mcp: Option<String>,
    instruction: Option<String>,
    error: Option<String>,
}

impl Outcome {
    fn to_json(&self) -> Value {
        json!({
            "id": self.id,
            "label": self.label,
            "ok": self.error.is_none(),
            "mcp": self.mcp,
            "instruction": self.instruction,
            "error": self.error,
        })
    }
}

/// 给一组客户端接上记忆 MCP，并（按配置）写提示词受控块。
///
/// `base_url` 是**客户端要访问的网关地址**，不是本进程视角的地址：面板端口与
/// 网关端口可能不同，这里必须用调用方算好的那个。
///
/// 返回值里带一份最新的 `cards`：前端点完「接入」就能直接重画，不用再发一次
/// `GET /api/memory/agents`（少一次往返，也避免两次请求之间状态不一致）。
pub fn install(clients: &[String], base_url: &str, api_key: &str) -> Value {
    let inject_prompt = config::bool_of("agents.injectAgentsMd");
    let mut outcomes: Vec<Outcome> = Vec::new();

    // MCP 那半边完全交给「工具服务 → 本机接入」：它对每个客户端的格式差异
    // （opencode 的 local/remote、gemini 的 httpUrl、codex 的 http_headers、
    // DSH 的 serverName/transport）都已经处理过，重复实现只会两边不一致。
    let mcp_results = tool_services::install_mcp(
        tool_services::MEMORY_SERVICE_ID,
        "http",
        base_url,
        api_key,
        clients,
        None,
    );
    for result in mcp_results {
        outcomes.push(Outcome {
            id: result.client_id.clone(),
            label: result.label.clone(),
            mcp: if result.ok { Some(result.path.clone()) } else { None },
            instruction: None,
            error: result.error.clone(),
        });
    }

    // 提示词那半边单独做：MCP 装失败不代表提示词也该失败（有的客户端 MCP
    // 配置只读，但提示词可写），两件事各自报结果。
    if inject_prompt {
        for outcome in outcomes.iter_mut() {
            let Some(def) = hub::find(&outcome.id) else {
                continue;
            };
            match write_prompt_block(def) {
                Ok(Some(action)) => outcome.instruction = Some(action),
                Ok(None) => outcome.instruction = Some("该客户端不支持提示词写入".into()),
                Err(error) => {
                    outcome.instruction = Some(format!("提示词写入失败：{error}"));
                }
            }
        }
    }

    json!({
        "results": outcomes.iter().map(Outcome::to_json).collect::<Vec<_>>(),
        "cards": list(),
    })
}

fn write_prompt_block(def: &hub::ClientDef) -> Result<Option<String>, String> {
    let snapshot = match resources::snapshot(def, "prompt") {
        Ok(snapshot) => snapshot,
        // 不支持提示词写入的客户端（DSH / Kimi 之类）不是错误
        Err(_) => return Ok(None),
    };
    let original = snapshot
        .get("content")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let next = inject_block(original, INSTRUCTION_BLOCK)?;
    if next == original {
        return Ok(Some("unchanged".into()));
    }
    let revision = snapshot
        .get("revision")
        .and_then(Value::as_str)
        .unwrap_or("absent")
        .to_string();
    let mut payload = json!({
        "kind": "prompt",
        "content": next,
        "revision": revision,
    });
    let preview = resources::create_preview(def, &payload)?;
    if preview.get("hasChanges").and_then(Value::as_bool) == Some(false) {
        return Ok(Some("unchanged".into()));
    }
    payload["token"] = preview.get("token").cloned().unwrap_or(Value::Null);
    resources::apply(def, &payload)?;
    Ok(Some(
        snapshot
            .get("path")
            .and_then(Value::as_str)
            .map(|path| format!("已写入 {path}"))
            .unwrap_or_else(|| "已写入".into()),
    ))
}

fn clear_prompt_block(def: &hub::ClientDef) -> Result<bool, String> {
    let snapshot = match resources::snapshot(def, "prompt") {
        Ok(snapshot) => snapshot,
        Err(_) => return Ok(false),
    };
    let original = snapshot
        .get("content")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let Some(next) = remove_block(original) else {
        return Ok(false);
    };
    let revision = snapshot
        .get("revision")
        .and_then(Value::as_str)
        .unwrap_or("absent")
        .to_string();
    let mut payload = json!({
        "kind": "prompt",
        "content": next,
        "revision": revision,
    });
    let preview = resources::create_preview(def, &payload)?;
    payload["token"] = preview.get("token").cloned().unwrap_or(Value::Null);
    resources::apply(def, &payload)?;
    Ok(true)
}

/// 卸载：删 MCP 条目 + 删提示词受控块。
pub fn uninstall(clients: &[String]) -> Value {
    let mut outcomes: Vec<Outcome> = tool_services::uninstall_mcp(tool_services::MEMORY_SERVICE_ID, clients)
        .into_iter()
        .map(|result| Outcome {
            id: result.client_id.clone(),
            label: result.label.clone(),
            mcp: if result.ok { Some(result.path.clone()) } else { None },
            instruction: None,
            error: result.error.clone(),
        })
        .collect();

    for outcome in outcomes.iter_mut() {
        let Some(def) = hub::find(&outcome.id) else {
            continue;
        };
        match clear_prompt_block(def) {
            Ok(true) => outcome.instruction = Some("已移除提示词块".into()),
            Ok(false) => outcome.instruction = Some("提示词里没有记忆块".into()),
            Err(error) => outcome.instruction = Some(format!("提示词块移除失败：{error}")),
        }
    }

    json!({
        "results": outcomes.iter().map(Outcome::to_json).collect::<Vec<_>>(),
        "cards": list(),
    })
}

/// 校验：逐个客户端读一遍配置，确认条目还在、提示词块还完整。
///
/// 单独做一个「校验」而不是只在接入时检查一次：用户可能事后手改配置文件，
/// 也可能换了客户端版本导致配置搬家。接入页要能回答「现在还通不通」。
pub fn verify(clients: &[String]) -> Value {
    let targets: Vec<String> = if clients.is_empty() {
        config::strings_of("agents.enabled")
    } else {
        clients.to_vec()
    };
    let mut rows = Vec::new();
    for id in targets {
        let Some(def) = hub::find(&id) else {
            rows.push(json!({ "id": id, "ok": false, "message": "未知客户端" }));
            continue;
        };
        let mcp = resources::snapshot(def, "mcp");
        let installed = mcp
            .as_ref()
            .ok()
            .and_then(|snapshot| snapshot.get("servers").and_then(Value::as_array))
            .is_some_and(|servers| {
                servers
                    .iter()
                    .any(|server| server.get("name").and_then(Value::as_str) == Some(server_name()))
            });
        let instruction = resources::snapshot(def, "prompt")
            .ok()
            .and_then(|snapshot| snapshot.get("path").and_then(Value::as_str).map(str::to_string))
            .map(|path| block_state(&path));
        rows.push(json!({
            "id": id,
            "name": def.name,
            "ok": installed,
            "mcp": installed,
            "mcpError": mcp.err(),
            "instruction": instruction.map(|state| json!({
                "injected": state.injected,
                "broken": state.broken,
            })),
        }));
    }
    let ok = rows
        .iter()
        .filter(|row| row.get("ok").and_then(Value::as_bool) == Some(true))
        .count();
    json!({ "rows": rows, "ok": ok, "total": rows.len() })
}

/// 配置片段：给自定义客户端 / 手贴用。
pub fn snippet(base_url: &str) -> Value {
    let url = format!(
        "{}{}",
        base_url.trim_end_matches('/'),
        tool_services::endpoint(tool_services::MEMORY_SERVICE_ID, "mcp")
    );
    let name = server_name();
    // 键名是运行时算出来的（跟着 `package_name` 走），不能写成字面量 ——
    // 否则改了服务名之后这里会静默地给用户一个错的片段。
    let entry = json!({
        "type": "http",
        "url": url,
        "headers": { "Authorization": "Bearer <你的 Agent2API Key>" },
    });
    let mut servers = serde_json::Map::new();
    servers.insert(name.to_string(), entry);
    json!({
        "serverName": name,
        "url": url,
        "json": { "mcpServers": servers },
        "toml": format!(
            "[mcp_servers.{name}]\nurl = \"{url}\"\nhttp_headers = {{ Authorization = \"Bearer <你的 Agent2API Key>\" }}\n"
        ),
        "instruction": INSTRUCTION_BLOCK,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inject_appends_once_and_replaces_afterwards() {
        let once = inject_block("# 我的规则\n\n原有内容\n", INSTRUCTION_BLOCK).unwrap();
        assert!(once.contains(BLOCK_BEGIN));
        assert!(once.starts_with("# 我的规则"));
        let twice = inject_block(&once, INSTRUCTION_BLOCK).unwrap();
        assert_eq!(twice.matches(BLOCK_BEGIN).count(), 1);
        assert_eq!(twice.matches(BLOCK_END).count(), 1);
    }

    #[test]
    fn remove_restores_user_text() {
        let once = inject_block("# 我的规则\n\n原有内容\n", INSTRUCTION_BLOCK).unwrap();
        let restored = remove_block(&once).unwrap();
        assert!(!restored.contains(BLOCK_BEGIN));
        assert!(restored.contains("原有内容"));
    }

    #[test]
    fn half_marker_is_refused() {
        let broken = format!("前文\n{BLOCK_BEGIN}\n没有结束标记\n");
        assert!(inject_block(&broken, INSTRUCTION_BLOCK).is_err());
    }

    #[test]
    fn empty_file_gets_block_only() {
        let result = inject_block("", INSTRUCTION_BLOCK).unwrap();
        assert!(result.starts_with(BLOCK_BEGIN));
        assert_eq!(result.matches(BLOCK_BEGIN).count(), 1);
    }
}
