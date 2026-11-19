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
        // 取到那一条的完整配置，而不只是「在不在」：卡片还要回答「它带没带身份头」。
        let entry = mcp_entry_of(mcp.as_ref().ok());
        let installed = entry.is_some();
        // `X-Agent` 是记忆分目录（`l1/<agent>/`）与心跳归属的唯一来源。老版本安装器
        // 不写它，条目照样「存在」，于是卡片显示「已接入」而记忆全部静默落进
        // `l1/mcp/` —— 事后分不清是谁写的。这里把事实摆到卡片上，让用户能看见。
        //
        // 变量不能叫 `agent_header`：本模块有同名函数，局部绑定会把它遮住，
        // 后面再想调用就变成「调用一个 Option」（与 E0618 同一个坑）。
        let identity = entry.as_ref().and_then(agent_header);
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
            // 配置文件形态（json / jsonc / json+env / toml / yaml）。页面「手动接入
            // 片段」要按它选 JSON 还是 TOML —— 给 Codex 贴一段 JSON 是纯浪费用户时间。
            "format": def.format.as_str(),
            "supported": supported,
            "note": mcp_error.clone().or(detection.note.clone()),
            "mcp": {
                "installed": installed,
                // 写进配置的 `X-Agent` 值（没有就是 null）。它既是「要不要提醒用户
                // 重写一次」的判据，也让用户能看出这条是不是别的客户端共用来的。
                "agentHeader": identity,
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

    // 自定义客户端：用户手填路径的兜底通道。不参与一键接入（没有资源写入器，
    // 不知道它家配置的合并语义），只把配置片段给他自己贴。
    let custom: Vec<Value> = custom_agents()
        .iter()
        .map(|item| {
            let config_exists = !item.config_path.is_empty() && std::path::Path::new(&item.config_path).is_file();
            let instruction_exists = !item.instruction_path.is_empty()
                && std::path::Path::new(&item.instruction_path).is_file();
            json!({
                "id": item.name,
                "name": item.name,
                "custom": true,
                "configPath": item.config_path,
                "configExists": config_exists,
                "format": item.format,
                "instructionPath": item.instruction_path,
                "instructionExists": instruction_exists,
                // 「写过没有」只看配置文件里有没有这个名字。自定义 Agent 的格式
                // 我们没解析，做不了 keys 级判断 —— 但「这一行有没有出现」足够
                // 回答用户此刻的问题（我到底贴进去了没有）。
                "installed": config_exists && item.mentions_server(),
                "detected": config_exists,
            })
        })
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

// ── 配置读取的小工具 ──────────────────────────────────────────

/// 仪表盘「已连通 Agent」那一个数字的专用口径。
///
/// ── 为什么不能直接拿 `list()` 数一下 ────────────────────────
/// `list()` 每张卡要做四件重活：探测客户端路径与版本、读 MCP 配置文件、
/// 读提示词文件、算受控块状态。仪表盘每次刷新都要这两个数字，把整套探测
/// 拉一遍是白花的 —— 用户看仪表盘时并不需要知道「Codex 装的是 0.44 还是 0.45」。
/// 这里只读心跳表 + 配置里「有没有我们这一条」，回答的就是那一个问题。
///
/// ── 分子分母为什么都只算「已配置」 ──────────────────────────
/// 分母若用「探测到的客户端总数」，会混进用户根本没接的客户端 —— 界面上
/// `0/7` 读起来像「全坏了」，实际是「6 个从来就没接过」。所以分母是
/// `injected`（配置文件里真有 `agent2api-memory` 这一条），分子是这些里
/// 「心跳 `calls > 0`」的数量。只配置不调用不算连通 —— 那是假绿灯。
pub fn connectivity() -> Value {
    let beats = index::global().beats();
    let mut injected = 0;
    let mut called = 0;
    for def in hub::CLIENTS {
        let mcp = resources::snapshot(def, "mcp");
        if mcp_entry_of(mcp.as_ref().ok()).is_none() {
            continue;
        }
        injected += 1;
        // 心跳行的 `agent` 是安装器写进配置的 `X-Agent`，与 `def.id` 同一套取值；
        // 老版本安装器没写这个头，那条记忆会落进 `l1/mcp/`，心跳也记在 `mcp` 上，
        // 于是这里数不到 —— 这与接入页卡片显示「未接入身份」是同一件事。
        let live = beats.iter().any(|beat| {
            beat.get("agent").and_then(Value::as_str) == Some(def.id)
                && beat.get("calls").and_then(Value::as_i64).unwrap_or(0) > 0
        });
        if live {
            called += 1;
        }
    }
    json!({ "called": called, "injected": injected })
}

/// 从 MCP 快照里取出本服务那一条的 `config`。
///
/// 快照的 `servers` 是 `[{name, endpoint, config}]`；不同客户端的 `config` 形态
/// 各不相同（json 用 `headers`、codex 用 `http_headers`、dsh 用 `serverName`），
/// 所以这里只负责「按名字找到那一条」，不解释内容。
fn mcp_entry_of(snapshot: Option<&Value>) -> Option<Value> {
    snapshot
        .and_then(|snapshot| snapshot.get("servers").and_then(Value::as_array))
        .and_then(|servers| {
            servers
                .iter()
                .find(|server| server.get("name").and_then(Value::as_str) == Some(server_name()))
        })
        .map(|server| server.get("config").cloned().unwrap_or(Value::Null))
}

/// 这一条配置里的 `X-Agent`（大小写不敏感地找头名）。
///
/// 三种头容器都要看：普通客户端写 `headers`、Codex 写 `http_headers`、DSH 的
/// 条目直接是 `config.headers`。头名按小写比对 —— JSON 里 `X-Agent` / `x-agent`
/// 都合法，客户端自己不区分大小写，我们也不该因为大小写把它判成「没写」。
fn agent_header(config: &Value) -> Option<String> {
    for key in ["headers", "http_headers", "httpHeaders"] {
        let Some(headers) = config.get(key).and_then(Value::as_object) else {
            continue;
        };
        for (name, value) in headers {
            if name.eq_ignore_ascii_case("x-agent") {
                if let Some(text) = value.as_str().map(str::trim).filter(|text| !text.is_empty()) {
                    return Some(text.to_string());
                }
            }
        }
    }
    None
}

// ── 自定义 Agent（用户手填的扩展位）────────────────────────────

/// 一条自定义 Agent。
///
/// ── 为什么存成 `名称|配置路径|格式|指令路径` 而不是 JSON 对象 ──
/// `agents.custom` 是 `Kind::List`（字符串数组），改成对象数组要动配置校验那一层；
/// 而这四个字段都是短字符串，竖线分隔与 `import.sources` 是同一套写法。
/// 兼容旧数据：老版本只存了名称（裸字符串），解析时按「只有名字」处理。
#[derive(Debug, Clone)]
pub struct CustomAgent {
    pub name: String,
    pub config_path: String,
    /// `json`（条目写在 `mcpServers`）或 `toml`（写在 `[mcp_servers]` 表）
    pub format: String,
    pub instruction_path: String,
}

impl CustomAgent {
    fn parse(raw: &str) -> Option<Self> {
        let text = raw.trim();
        if text.is_empty() {
            return None;
        }
        let mut parts = text.split('|').map(str::trim);
        let name = parts.next().unwrap_or_default().to_string();
        if name.is_empty() {
            return None;
        }
        let config_path = parts.next().unwrap_or_default().to_string();
        let format = parts
            .next()
            .map(|value| value.to_ascii_lowercase())
            .filter(|value| value == "json" || value == "toml")
            .unwrap_or_else(|| "json".to_string());
        let instruction_path = parts.next().unwrap_or_default().to_string();
        Some(Self { name, config_path, format, instruction_path })
    }

    fn to_config(&self) -> String {
        format!("{}|{}|{}|{}", self.name, self.config_path, self.format, self.instruction_path)
    }

    /// 配置文件里出现过我们的 server 名吗。
    ///
    /// 用文本包含而不是解析：JSON / TOML 两种格式各写一套解析只为回答一个是非题，
    /// 反而多两处会漂移的地方。代价是别人在注释里写到这个名字时会误报 ——
    /// 这个代价比「解析器跟客户端格式一起演进」小得多。
    fn mentions_server(&self) -> bool {
        if self.config_path.is_empty() {
            return false;
        }
        std::fs::read_to_string(&self.config_path)
            .map(|text| text.contains(server_name()))
            .unwrap_or(false)
    }
}

/// 当前配置里的自定义 Agent 列表。
pub fn custom_agents() -> Vec<CustomAgent> {
    config::strings_of("agents.custom")
        .iter()
        .filter_map(|raw| CustomAgent::parse(raw))
        .collect()
}

/// 加一条自定义 Agent（同名覆盖）。
///
/// ── 为什么要跟扩展名对一下格式 ──────────────────────────────
/// 格式选错的表现是「保存成功了，但那条 MCP 条目写在错误的键下，客户端永远读不到」
/// —— 用户会以为是客户端不支持。这里在保存时就拦：`.toml` 配 `json` 直接报错。
pub fn add_custom(name: &str, config_path: &str, format: &str, instruction_path: &str) -> Result<Value, String> {
    let name = name.trim();
    let config_path = config_path.trim();
    if name.is_empty() {
        return Err("名称不能为空".into());
    }
    if config_path.is_empty() {
        return Err("配置文件路径不能为空".into());
    }
    let format = format.trim().to_ascii_lowercase();
    let format = match format.as_str() {
        "json" | "toml" => format,
        _ => return Err(format!("不认识的格式：{format}（只支持 json / toml）")),
    };
    let lowered = config_path.to_ascii_lowercase();
    if lowered.ends_with(".toml") && format == "json" {
        return Err("配置文件是 .toml，格式要选 TOML —— 用 JSON 写进去客户端读不到".into());
    }
    if (lowered.ends_with(".json") || lowered.ends_with(".jsonc")) && format == "toml" {
        return Err("配置文件是 JSON，格式要选 JSON —— 用 TOML 写进去客户端读不到".into());
    }

    let mut items = custom_agents();
    let entry = CustomAgent {
        name: name.to_string(),
        config_path: config_path.to_string(),
        format,
        instruction_path: instruction_path.trim().to_string(),
    };
    match items.iter_mut().find(|item| item.name == entry.name) {
        Some(slot) => *slot = entry,
        None => items.push(entry),
    }
    // 变量不能叫 `list`：本模块有一个同名的 `fn list()`，局部绑定会把它遮住，
    // 于是 `Ok(list())` 变成「调用一个 Vec」（E0618）。改完顺手记住这个坑。
    let entries: Vec<Value> = items.iter().map(|item| json!(item.to_config())).collect();
    config::update(|next| next.set("agents.custom", json!(entries)))?;
    Ok(list())
}

/// 删一条自定义 Agent。
pub fn remove_custom(name: &str) -> Result<Value, String> {
    let name = name.trim();
    let mut items = custom_agents();
    let before = items.len();
    items.retain(|item| item.name != name);
    if items.len() == before {
        return Err(format!("没有这个自定义 Agent：{name}"));
    }
    let entries: Vec<Value> = items.iter().map(|item| json!(item.to_config())).collect();
    config::update(|next| next.set("agents.custom", json!(entries)))?;
    Ok(list())
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

/// 待探活的一个客户端：配置侧的事实 + 探活要用的原始条目。
///
/// 用具名结构体而不是六元组：`zip` 之后要同时解出 `installed` / `entry` /
/// `instruction` 三样，元组下标写错（`entry` 与 `mcp_error` 都是 `Option`）编译器
/// 也不会拦 —— 那种错误的表现是「卡片显示的身份来自另一个字段」，很难查。
struct Target {
    id: String,
    name: String,
    installed: bool,
    entry: Option<Value>,
    mcp_error: Option<String>,
    instruction: Option<BlockState>,
}

/// 校验：逐个客户端读一遍配置，确认条目还在，**再真的发一次 `initialize`**。
///
/// ── 为什么必须真发请求 ──────────────────────────────────────
/// 早先这里只读盘（条目在不在、标记块全不全），于是「测试连接」这个名字是假的：
/// 配置文件写对了但密钥失效、端口改了、服务没起，它照样报「检测通过」。用户拿
/// 这个绿色结论去排查「为什么 Agent 不写记忆」，方向会被彻底带偏。
///
/// ── 为什么用**配置里那把** url / headers ────────────────────
/// 要回答的是「这个客户端照它现在的配置，能不能连上」。用网关自己算一遍地址去
/// 探活，测的是「网关能不能连自己」—— 永远为真，一点信息量都没有。所以 url 和
/// 鉴权头都从快照里取，探的就是客户端将来会走的那条路。
///
/// ── 为什么探 `initialize` 而不是 `tools/list` ────────────────
/// `initialize` 是任何 MCP 客户端握手的第一步，也是最便宜的一次往返：它不碰
/// 记忆数据、不写心跳、不需要任何参数。`tools/list` 要多传一次协议版本，且
/// 万一服务端按会话记账，会把「探活」记成「客户端真的在用了」。
pub async fn verify(clients: &[String]) -> Value {
    let targets: Vec<String> = if clients.is_empty() {
        config::strings_of("agents.enabled")
    } else {
        clients.to_vec()
    };
    // 读配置是同步文件 IO：放到 `blocking::in_place` 里，免得在 async 上下文里
    // 阻塞 tokio 的 worker 线程（项目里读盘一律走这条，见 `core::blocking`）。
    let prepared = crate::server::core::blocking::in_place(|| read_targets(&targets));

    // 只探「配置里确实有我们这一条」的客户端；没有条目的不用发请求 —— 结论是
    // 「没接入」，不是「连不上」，两者给用户的下一步完全不同。
    let probes = futures::future::join_all(prepared.iter().map(|target| async move {
        if target.installed {
            probe(target.entry.as_ref()).await
        } else {
            Probe::skipped("配置里没有这条 MCP 条目")
        }
    }))
    .await;

    let mut rows = Vec::new();
    // 循环变量不叫 `probe`：本模块有同名 async fn，遮住之后若将来在循环体内再
    // 想探一次，会得到「调用一个 struct」（E0618 同款）。
    for (target, outcome) in prepared.into_iter().zip(probes) {
        // `ok` 的含义收窄成「配置在 **且** 探活成功」：徽标上的绿必须对应一个
        // 真实往返，否则又回到「读盘报通过」的老问题。
        let ok = target.installed && outcome.reachable;
        rows.push(json!({
            "id": target.id,
            "name": target.name,
            "ok": ok,
            "mcp": target.installed,
            "mcpError": target.mcp_error,
            "agentHeader": target.entry.as_ref().and_then(agent_header),
            "reachable": outcome.reachable,
            "detail": outcome.detail,
            "instruction": target.instruction.map(|state| json!({
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

/// 逐个客户端把配置侧的事实读出来（同步 IO，调用方负责切到 blocking 上下文）。
fn read_targets(ids: &[String]) -> Vec<Target> {
    let mut prepared: Vec<Target> = Vec::new();
    for id in ids {
        let Some(def) = hub::find(id) else {
            prepared.push(Target {
                id: id.clone(),
                name: String::new(),
                installed: false,
                entry: None,
                mcp_error: Some("未知客户端".into()),
                instruction: None,
            });
            continue;
        };
        let mcp = resources::snapshot(def, "mcp");
        let mcp_error = mcp.as_ref().err().cloned();
        let entry = mcp_entry_of(mcp.as_ref().ok());
        let instruction = resources::snapshot(def, "prompt")
            .ok()
            .and_then(|snapshot| snapshot.get("path").and_then(Value::as_str).map(str::to_string))
            .map(|path| block_state(&path));
        prepared.push(Target {
            id: id.clone(),
            name: def.name.to_string(),
            installed: entry.is_some(),
            entry,
            mcp_error,
            instruction,
        });
    }
    prepared
}

/// 一次探活的结果。
struct Probe {
    reachable: bool,
    detail: String,
}

impl Probe {
    fn skipped(detail: &str) -> Self {
        Self { reachable: false, detail: detail.to_string() }
    }
    fn failed(detail: String) -> Self {
        Self { reachable: false, detail }
    }
}

/// 按客户端配置里的 url / headers 发一次 MCP `initialize`。
///
/// 客户端配置里可能根本没有 url（本地桥接形态是 `command` + `args`），那种情况
/// 我们探不了 —— 桥接进程是客户端自己 spawn 的，网关这边看不到。明确说「探不了」
/// 比编一个「通过」诚实。
async fn probe(entry: Option<&Value>) -> Probe {
    let Some(config) = entry else {
        return Probe::skipped("配置里没有这条 MCP 条目");
    };
    let Some(url) = config
        .get("url")
        .or_else(|| config.get("httpUrl"))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|url| !url.is_empty())
    else {
        return Probe::skipped("这条配置走本地桥接（command），网关无法代探");
    };

    let client = match reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(5))
        // 探的是本机/局域网端点，绝不能被系统代理截走 —— 一个配了全局代理的
        // 机器上，请求会跑到代理那边去，得到的 502 会被误读成「服务挂了」。
        .no_proxy()
        .build()
    {
        Ok(client) => client,
        Err(error) => return Probe::failed(format!("构造探活请求失败：{error}")),
    };

    let mut request = client
        .post(url)
        .header("Content-Type", "application/json")
        .header("Accept", "application/json, text/event-stream")
        .json(&json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {
                "protocolVersion": "2024-11-05",
                "capabilities": {},
                "clientInfo": { "name": "agent2api-verify", "version": env!("CARGO_PKG_VERSION") },
            },
        }));
    for (name, value) in config_headers(config) {
        request = request.header(name, value);
    }

    let response = match request.send().await {
        Ok(response) => response,
        Err(error) => {
            // 连不上是接入页最需要区分的一类失败：服务没起 / 端口不对 / 被防火墙
            // 挡了。`reqwest` 的错误文本已经分得够细（connection refused / timeout），
            // 原样带上比我们自己归纳成一句「失败」有用得多。
            return Probe::failed(format!("连不上 {url}：{error}"));
        }
    };
    let status = response.status();
    if status == reqwest::StatusCode::UNAUTHORIZED || status == reqwest::StatusCode::FORBIDDEN {
        return Probe::failed(format!("{url} 返回 {status} —— 配置里的 API Key 无效或已停用"));
    }
    if !status.is_success() {
        return Probe::failed(format!("{url} 返回 {status}"));
    }

    // 握手成功的判据是「回了带 serverInfo 的 result」，不是「HTTP 200」：
    // 反代 / 登录页也可能给 200，那种 200 会让客户端拿不到任何工具。
    let body: Value = match response.json().await {
        Ok(body) => body,
        Err(error) => return Probe::failed(format!("响应不是 JSON：{error}")),
    };
    match handshake(&body) {
        Some(name) => Probe { reachable: true, detail: format!("{url} 握手成功（{name}）") },
        None => Probe::failed(format!("{url} 有响应但不是 MCP 握手：{}", brief(&body))),
    }
}

/// 响应体是不是一次成功的 MCP 握手？是则返回服务端自报的名字。
///
/// 只看 `result.serverInfo.name`：JSON-RPC 的错误响应长成 `{"error": {...}}`，
/// 没有 `result`，天然被排除；而「200 + 登录页 HTML」在解析 JSON 那一步就失败了。
/// 单独抽成函数是为了能测 —— `probe` 本身要发网络请求，测试里不该起服务。
fn handshake(body: &Value) -> Option<&str> {
    body.get("result")
        .and_then(|result| result.get("serverInfo"))
        .and_then(|info| info.get("name"))
        .and_then(Value::as_str)
        .filter(|name| !name.is_empty())
}

/// 配置里的请求头，转成可以塞进 reqwest 的键值对。
///
/// `Authorization` 与 `X-Agent` 都要带上：少了前者会被判 401（探活变成误报失败），
/// 少了后者虽然能握手，但探的不是「客户端真实会发的那一份」。
fn config_headers(config: &Value) -> Vec<(String, String)> {
    let mut pairs = Vec::new();
    for key in ["headers", "http_headers", "httpHeaders"] {
        let Some(headers) = config.get(key).and_then(Value::as_object) else {
            continue;
        };
        for (name, value) in headers {
            if let Some(text) = value.as_str() {
                pairs.push((name.clone(), text.to_string()));
            }
        }
    }
    pairs
}

/// 把响应体截短到可读长度。
///
/// 不叫 `snippet`：本模块有 `pub fn snippet(base_url)`（配置片段），同名会让
/// 编译直接失败 —— 而且这种失败发生在「加了个小工具函数」之后，第一眼很难联想到
/// 是重名。
fn brief(body: &Value) -> String {
    let text = body.to_string();
    if text.chars().count() > 160 {
        format!("{}…", text.chars().take(160).collect::<String>())
    } else {
        text
    }
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
        // 命令行形态只给 Claude Code：它的 CLI 语法是公开且稳定的。
        // 其它客户端的 CLI 参数各家不同、还会随版本改 —— 猜一条错的命令，
        // 用户会照着执行然后得到「未知参数」，比不给他更糟。页面上要写清这点。
        "commandLine": format!(
            "claude mcp add --transport http {name} {url} --header \"Authorization: Bearer <你的 Agent2API Key>\""
        ),
        "commandLineFor": "claude-code",
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

    /// 身份头三种容器都要认得。
    ///
    /// 漏掉 `http_headers`（Codex）或大小写写歪，卡片会误报「缺身份」，
    /// 用户于是去点一键注入，把本来正确的配置重写一遍 —— 白折腾还多一次备份。
    #[test]
    fn agent_header_reads_every_container() {
        let plain = json!({ "headers": { "X-Agent": "workbuddy" } });
        assert_eq!(agent_header(&plain).as_deref(), Some("workbuddy"));

        let codex = json!({ "http_headers": { "x-agent": "codex" } });
        assert_eq!(agent_header(&codex).as_deref(), Some("codex"));

        // 真实世界里 `X-AGENT` / `x-Agent` 都出现过，客户端自己不区分大小写。
        let shouted = json!({ "headers": { "X-AGENT": "claude-code" } });
        assert_eq!(agent_header(&shouted).as_deref(), Some("claude-code"));
    }

    /// 老版本写的那一条（只有 Authorization）必须判成「缺身份」。
    ///
    /// 这是本次修复的起因：条目存在 → 卡片显示「已接入」→ 用户以为一切正常，
    /// 而记忆其实全落进 `l1/mcp/`。判定必须为 None，界面才有机会提示重写。
    #[test]
    fn legacy_entry_without_identity_is_detected() {
        let legacy = json!({ "type": "http", "url": "http://127.0.0.1:3065/mcp/memory",
            "headers": { "Authorization": "Bearer sk-a2a-x" } });
        assert_eq!(agent_header(&legacy), None);
    }

    /// 空值不算写了：`"X-Agent": ""` 与没有这一条对下游等价（都会回落 `mcp`），
    /// 所以卡片也不该显示一个空的身份。
    #[test]
    fn blank_identity_counts_as_missing() {
        let blank = json!({ "headers": { "X-Agent": "   " } });
        assert_eq!(agent_header(&blank), None);
    }

    /// 探活要带的就是配置里那几个头，一个不能少。
    ///
    /// 少了 `Authorization` → 401，探活变成误报失败；少了 `X-Agent` → 探的不是
    /// 客户端真实会发的那一份。
    #[test]
    fn config_headers_collects_auth_and_identity() {
        let entry = json!({ "headers": { "Authorization": "Bearer sk-a2a-x", "X-Agent": "workbuddy" } });
        let pairs = config_headers(&entry);
        let get = |key: &str| pairs.iter().find(|(name, _)| name == key).map(|(_, value)| value.as_str());
        assert_eq!(get("Authorization"), Some("Bearer sk-a2a-x"));
        assert_eq!(get("X-Agent"), Some("workbuddy"));
    }

    /// 真握手成功的响应长什么样（取自本机网关的实际返回）。
    #[test]
    fn handshake_accepts_the_real_initialize_response() {
        let body = json!({
            "id": 1, "jsonrpc": "2.0",
            "result": {
                "capabilities": { "tools": {} },
                "instructions": "…",
                "protocolVersion": "2024-11-05",
                "serverInfo": { "name": "agent2api-memory", "version": "1.0.0" },
            },
        });
        assert_eq!(handshake(&body), Some("agent2api-memory"));
    }

    /// 401 / 反代页 / JSON-RPC 错误都不是握手成功。
    ///
    /// 这条断言守的是「测试连接」的可信度：只要有一类非握手的 200 被当成通过，
    /// 用户就会拿着假绿去排查真问题。
    #[test]
    fn handshake_rejects_non_mcp_payloads() {
        // JSON-RPC 错误：有 jsonrpc 有 id，但没有 result
        let error = json!({ "jsonrpc": "2.0", "id": 1, "error": { "code": -32600, "message": "bad" } });
        assert_eq!(handshake(&error), None);
        // 反代 / 登录页包装过的响应
        let proxy = json!({ "code": 200, "data": { "ok": true } });
        assert_eq!(handshake(&proxy), None);
        // serverInfo 里名字为空 —— 客户端会拿不到可用服务名
        let blank = json!({ "result": { "serverInfo": { "name": "" } } });
        assert_eq!(handshake(&blank), None);
    }
}
