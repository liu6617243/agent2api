//! 历史会话导入：把「别处已经说过的话」搬进记忆库。
//!
//! ── 为什么要有这一层 ────────────────────────────────────────
//! 记忆中枢的价值取决于「第一条记忆从哪来」。新装用户的记忆库是空的，而他要
//! 用的历史决策全躺在别处：Claude Code 的 `~/.claude/projects/*.jsonl`、
//! Codex 的会话记录、Obsidian 里的笔记目录。让用户手抄一遍等于这个功能不存在。
//!
//! ── 为什么强制干跑 ──────────────────────────────────────────
//! 导入是**批量写盘**：一次几千条，写错了要么污染检索结果、要么把隐私内容
//! 搬进一个会被 MCP 工具暴露出去的地方。所以流程固定两段：
//!
//!   1. `preview` 只读文件、只算统计，**一个字都不写**，返回「会导入多少条、
//!      跳过多少条、为什么跳过」；
//!   2. `run` 真写，且按 `import.batchSize` 分批，每批之间可以被中断 ——
//!      已写入的条目都是完整的，不会留下半条。
//!
//! `import.dryRunFirst` 默认 `true` 时，`run` 会在没有近期预览记录的情况下
//! 拒绝执行。这不是防呆，是防「用户在另一个标签页里点了按钮」。
//!
//! ── 为什么敏感内容默认跳过而不是脱敏 ────────────────────────
//! 脱敏要靠正则猜「哪个串是密钥」，猜漏一个就把真实凭据写进了会被模型读到的
//! 地方。跳过只损失一条记忆，猜漏是不可逆的泄漏。想导就自己关掉这个开关。
//!
//! ── 为什么按「文件」而不是按「消息」切条 ────────────────────
//! 一次会话的价值在于上下文连着读；拆成一条条消息之后，「为什么这么改」与
//! 「改了哪里」分家，检索命中一半反而误导。所以：Markdown 一个文件一条，
//! 会话记录一次会话一条，正文里保留原始轮次顺序。

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicI64, Ordering};

use serde_json::{json, Value};

use super::model::{self, content_hash, truncate_chars};
use super::{config, index, store, WriteInput};

/// 单个文件最多吃进来的字符数。超出部分在「窗口切条」路径里由窗口大小
/// （`CHUNK_CHARS`）接管；这条只作为 Markdown 单条记忆的硬上限 —— 一篇几万字的
/// 长文档全塞进一条记忆，检索时它会挤掉所有别的东西。
const MAX_FILE_CHARS: usize = 24_000;

/// 单条记忆窗口的目标字数。历史会话按这个宽度切成多条记忆，而不是「一个会话一条」。
///
/// ── 为什么是「按窗口切」而不是「一条消息一条」───────────────
/// 一条消息一条会产生大量碎片（本次实测本机同类数据约 3.1 万条），检索命中半句
/// 反而误导；一个会话一条又会把整段上下文压成一条巨物（实测丢掉 94% 内容）。
/// 定长窗口折中：既 100% 覆盖原始内容，又把碎片控制在一个会话十条上下，且窗口
/// 内上下文仍连着读。
const CHUNK_CHARS: usize = 8_000;

/// 相邻窗口的重叠字数。切点落在轮次边界时，靠这段重叠保证「跨边界的那句话」
/// 在两个窗口里都能被检索到 —— 否则一句话恰好被切在中间，两个窗口都不完整。
const CHUNK_OVERLAP: usize = 600;

/// 一条记忆正文的下限：太短的（「ok」「继续」）不是记忆，是噪声。
///
/// 按**字符**算而不是字节：中文一个字等于三个字节，用字节数当门槛会让
/// 「端口固定 3065」这种确实有信息量的短笔记被判成噪声（17 字 = 51 字节，
/// 刚好在 40 的边缘上反复）。
///
/// 12 是反复试出来的下限：能过的都是成句的结论（「网关端口固定 3065，
/// 客户端写死了」），过不了的都是「ok」「继续」「1」这类纯应答。
const MIN_BODY_CHARS: usize = 12;

/// 预览记录的有效期。超过这个时间的预览不算「刚看过」。
const PREVIEW_TTL_MS: i64 = 30 * 60 * 1000;

/// 上次干跑的时间戳。
///
/// ── 为什么放在内存而不是索引库 ──────────────────────────────
/// `preview` 对外的承诺是「一个字都不写」。往 `index.db` 里塞一个时间戳虽然
/// 没人看得见，但它让这句话变成假的 —— 而且下次有人照这个模式再存点别的，
/// 「干跑不写盘」这条边界就慢慢没了。进程重启后失效是**特性**：重启之后
/// 用户确实没有在本次会话里看过预览。
static LAST_PREVIEW_AT: AtomicI64 = AtomicI64::new(0);

const LAST_RUN_KEY: &str = "import.lastRun";

// ── 来源解析 ─────────────────────────────────────────────────

/// 一个导入来源。
///
/// 配置里是字符串数组（`import.sources`），每项形如
/// `路径|格式|开关|优先级`，例如：
///
/// ```text
/// C:/Users/me/.claude/projects|jsonl|true|10
/// D:/notes|md|false|5
/// ```
///
/// 为什么用竖线分隔的裸字符串而不是嵌套对象：`Kind::List` 只接受字符串数组，
/// 改成对象数组要动配置校验那一层；而这里要表达的字段就四个，多一层嵌套换来
/// 的只是「更好看」，代价是用户手改配置时更容易写错括号。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Source {
    pub path: String,
    /// `md` / `jsonl`
    pub format: String,
    pub enabled: bool,
    pub priority: i64,
}

impl Source {
    /// 解析一条 `路径|格式|开关|优先级`。
    ///
    /// 公开是因为保存来源的 API 也要用它 —— 页面提交的是表单对象，落盘前统一
    /// 折成同一种字符串形态，避免「前端拼的串」与「后端拼的串」两套格式。
    pub fn parse(raw: &str) -> Option<Self> {
        let text = raw.trim();
        if text.is_empty() {
            return None;
        }
        let mut parts = text.split('|').map(str::trim);
        let path = parts.next().unwrap_or_default().to_string();
        if path.is_empty() {
            return None;
        }
        let format = parts
            .next()
            .map(|value| value.to_ascii_lowercase())
            .filter(|value| value == "md" || value == "jsonl")
            .unwrap_or_else(|| guess_format(&path));
        let enabled = parts
            .next()
            .map(|value| !matches!(value.to_ascii_lowercase().as_str(), "false" | "0" | "off" | "no"))
            .unwrap_or(true);
        let priority = parts.next().and_then(|value| value.parse::<i64>().ok()).unwrap_or(0);
        Some(Source { path, format, enabled, priority })
    }

    pub fn to_config(&self) -> String {
        format!("{}|{}|{}|{}", self.path, self.format, self.enabled, self.priority)
    }

    /// 实际读盘的路径。
    ///
    /// 页面上的示例写的是 `~/.claude/projects`，而 `PathBuf::from` 不认 `~` ——
    /// 不展开的话用户照着提示填完只会看到「路径不存在」，还会以为是自己填错。
    /// 只认开头的 `~/` 与 `~\`：中间出现的 `~` 在真实文件名里是合法字符。
    pub fn root(&self) -> PathBuf {
        home_expand(&self.path)
    }

    pub fn label(&self) -> String {
        self.root()
            .file_name()
            .map(|name| name.to_string_lossy().to_string())
            .unwrap_or_else(|| self.path.clone())
    }
}

/// 没写格式时按路径猜：`.jsonl` / `.json` 是会话记录，其余按 Markdown 目录。
pub(super) fn guess_format(path: &str) -> String {
    let lowered = path.to_ascii_lowercase();
    if lowered.ends_with(".jsonl") || lowered.ends_with(".json") {
        "jsonl".to_string()
    } else {
        "md".to_string()
    }
}

/// 展开路径开头的 `~`。`Source::root` 与清册的 `expand` 共用这一份 ——
/// 两处各写一遍的话，「配置文件里写的 `~/x`」和「卡片上探测的 `~/x`」迟早
/// 有一边忘了改。
pub(super) fn home_expand(path: &str) -> PathBuf {
    let raw = path.trim();
    let rest = raw
        .strip_prefix("~/")
        .or_else(|| raw.strip_prefix("~\\"))
        .or_else(|| if raw == "~" { Some("") } else { None });
    match rest {
        Some(rest) => {
            let home = crate::server::core::system_info::home_dir();
            if home.as_os_str().is_empty() {
                PathBuf::from(raw)
            } else if rest.is_empty() {
                home
            } else {
                home.join(rest)
            }
        }
        None => PathBuf::from(raw),
    }
}

/// 路径的规范形态，只用于**比较**。
///
/// 四种写法指同一个位置：`~/.claude/projects`（用户照提示填的）、
/// `C:\Users\me\.claude\projects`（目录选择器给的）、
/// `C:/Users/me/.claude/projects`（清册把它转成了正斜杠），以及
/// Windows 上的大小写差异。不归一就没法判断「这个来源已经在清单里了」。
pub(super) fn canon(path: &str) -> String {
    let text = home_expand(path).to_string_lossy().replace('\\', "/");
    let text = text.trim_end_matches('/').to_string();
    if cfg!(windows) {
        text.to_ascii_lowercase()
    } else {
        text
    }
}

/// 两条路径是不是同一个目录（比较用）。
pub(super) fn same_path(left: &str, right: &str) -> bool {
    canon(left) == canon(right)
}

/// 生效的来源清单（按优先级降序，关掉的也返回 —— 页面要显示它们为什么没跑）。
pub fn sources() -> Vec<Source> {
    let mut items: Vec<Source> = config::strings_of("import.sources")
        .iter()
        .filter_map(|raw| Source::parse(raw))
        .collect();
    items.sort_by(|a, b| b.priority.cmp(&a.priority).then(a.path.cmp(&b.path)));
    items
}

/// 写回来源清单（页面增删改后调用）。
pub fn save_sources(items: &[Source]) -> Result<(), String> {
    let list: Vec<Value> = items.iter().map(|item| json!(item.to_config())).collect();
    config::patch(&json!({ "import.sources": list }))?;
    Ok(())
}

/// 「启用 / 停用 / 改路径」这类要同时动 `import.sources` 与 `import.ignored`
/// 的动作，统一走这一个事务。
///
/// ── 为什么必须是一个事务 ────────────────────────────────────
/// 两个键是一份**两元状态**：一个来源要么「在清单里」，要么「在忽略名单里」。
/// 拆成两次 `patch` 时，第二次失败（或另一路并发写入插进来）就会留下自相矛盾
/// 的中间态 —— 页面显示「已启用」，扫描却把它当忽略跳过。而且「先读两个键、
/// 判断、再写回」这段逻辑本身也不允许中途换版本，否则写回去的是过期快照。
///
/// 闭包拿到的是**刚从磁盘读上来的**两份原始清单（原串，不重新 parse 再拼，
/// 免得顺手把人家写的 `1` 改成 `true`），返回改后的两份，由本函数一次落盘。
pub fn update_sources(
    edit: impl FnOnce(&[String], &[String]) -> Result<(Vec<String>, Vec<String>), String>,
) -> Result<(), String> {
    config::update(|next| {
        let sources = next.strings("import.sources");
        let ignored = next.strings("import.ignored");
        let (sources, ignored) = edit(&sources, &ignored)?;
        let payload = json!({
            "import.sources": sources.iter().map(|item| json!(item)).collect::<Vec<_>>(),
            "import.ignored": ignored.iter().map(|item| json!(item)).collect::<Vec<_>>(),
        });
        // 走 `apply_in` 而不是直接改 `values`：类型校验、未知键丢弃等规则与
        // `patch` 完全同一条路，避免「事务版本」与「普通版本」两套校验口径。
        config::apply_in(next, &payload)
    })?;
    Ok(())
}

// ── 候选文件 ─────────────────────────────────────────────────

/// 一个待导入的文件（或一次会话切出的一段窗口）。
#[derive(Debug, Clone)]
struct Pending {
    source: String,
    path: PathBuf,
    /// 归档到哪个项目（来源目录名，slug 化由 `write` 再做一次）
    project: Option<String>,
    /// 会话标识：同一次会话切出的多个窗口共享，检索时能串回上下文
    session: Option<String>,
    /// 这段内容**原始发生**的时间（毫秒）。会话记录能读到就透传，读不到留 `None`
    /// 由写路径兜底为写入时刻。窗口切条时取窗口内第一段带时间的内容。
    created_at: Option<i64>,
    title: String,
    body: String,
    /// 为什么被跳过（`None` = 会导入）
    skip: Option<String>,
}

/// 递归列目录里的候选文件。深度上限 6 —— 笔记目录偶尔有很深的层级，但
/// 无上限的递归遇到符号链接环会把进程卡死。
fn walk(dir: &Path, depth: usize, out: &mut Vec<PathBuf>) {
    if depth > 6 || out.len() >= 4000 {
        return;
    }
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    let mut files: Vec<PathBuf> = Vec::new();
    let mut dirs: Vec<PathBuf> = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        let Ok(meta) = entry.metadata() else { continue };
        if meta.is_dir() {
            // 跳过明显的依赖/版本库目录：它们里面的 md 是文档，不是记忆
            let name = entry.file_name().to_string_lossy().to_ascii_lowercase();
            if matches!(
                name.as_str(),
                "node_modules" | ".git" | "target" | "dist" | "build" | ".venv" | "__pycache__"
            ) {
                continue;
            }
            dirs.push(path);
        } else if meta.is_file() {
            files.push(path);
        }
    }
    files.sort();
    dirs.sort();
    out.extend(files);
    for sub in dirs {
        walk(&sub, depth + 1, out);
    }
}

fn is_note(path: &Path) -> bool {
    matches!(
        path.extension().and_then(|ext| ext.to_str()).map(|ext| ext.to_ascii_lowercase()),
        Some(ext) if ext == "md" || ext == "markdown"
    )
}

fn is_transcript(path: &Path) -> bool {
    matches!(
        path.extension().and_then(|ext| ext.to_str()).map(|ext| ext.to_ascii_lowercase()),
        Some(ext) if ext == "jsonl" || ext == "json"
    )
}

/// 毫秒时间戳 → `YYYY-MM-DD HH:MM`（北京时间）。
///
/// 导入结果里的时间给绝对时刻而不是「3 分钟前」：这是一条**历史记录**，
/// 用户回头看的时候「刚刚」毫无信息量。
fn stamp_text(ms: i64) -> String {
    crate::server::core::beijing::date_time_of_ms(ms)
        .map(|at| at.format("%Y-%m-%d %H:%M").to_string())
        .unwrap_or_default()
}

/// 展示用的路径。
///
/// 候选文件都在 memory 根**之外**，`store::rel` 会原样返回绝对路径 —— 那会把
/// `C:/Users/<名字>/...` 直接摆到页面上。先试相对路径（同步进来的条目可能真在
/// 根下），再退回把主目录换成 `~` 的形态；反斜杠统一成正斜杠，与页面其它
/// 路径展示一致。
fn display_path(path: &Path) -> String {
    let relative = store::rel(path);
    let text = if relative.contains(':') || relative.starts_with('/') {
        crate::server::core::system_info::mask_path(path)
    } else {
        relative
    };
    text.replace('\\', "/")
}

// ── 敏感内容 ─────────────────────────────────────────────────

/// 一眼就是凭据的形态。命中即跳过整条，不做局部替换。
///
/// 这份清单只求「不漏掉明显的」：`sk-` 开头的密钥、GitHub token、AWS key id、
/// PEM 私钥头、`password = "..."`、`Bearer <长串>`。漏掉的靠用户自己关开关前
/// 先干跑看一眼 —— 所以预览必须把「为什么跳过」写清楚，而不是只报个数字。
fn sensitive_hit(text: &str) -> Option<&'static str> {
    use std::sync::OnceLock;
    static RULES: OnceLock<Vec<(regex::Regex, &'static str)>> = OnceLock::new();
    let rules = RULES.get_or_init(|| {
        [
            (r"sk-[A-Za-z0-9_\-]{20,}", "疑似 OpenAI/Anthropic 密钥"),
            (r"gh[pousr]_[A-Za-z0-9]{20,}", "疑似 GitHub token"),
            (r"AKIA[0-9A-Z]{16}", "疑似 AWS access key"),
            (r"-----BEGIN [A-Z ]*PRIVATE KEY-----", "私钥块"),
            (r"(?i)\b(?:password|passwd|pwd)\s*[:=]\s*\S{4,}", "疑似口令赋值"),
            (r"(?i)\bBearer\s+[A-Za-z0-9._\-]{20,}", "疑似 Bearer token"),
            (r"xox[baprs]-[A-Za-z0-9\-]{10,}", "疑似 Slack token"),
        ]
        .into_iter()
        .filter_map(|(pattern, label)| regex::Regex::new(pattern).ok().map(|regex| (regex, label)))
        .collect()
    });
    rules.iter().find(|(regex, _)| regex.is_match(text)).map(|(_, label)| *label)
}

// ── Markdown ─────────────────────────────────────────────────

/// 一篇 Markdown → 一条记忆。
///
/// 标题取第一个一级标题（没有就取文件名）；正文去掉最前面的 H1，避免标题在
/// 记忆里出现两遍。frontmatter 原样保留在正文里 —— 那是用户的元数据，删掉
/// 等于篡改原文。
fn from_markdown(path: &Path, project: Option<String>) -> Pending {
    let raw = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(error) => {
            return Pending {
                source: String::new(),
                path: path.to_path_buf(),
                project,
                session: None,
                created_at: None,
                title: String::new(),
                body: String::new(),
                skip: Some(format!("读不了：{error}")),
            }
        }
    };
    let trimmed = raw.trim_start_matches('\u{feff}');
    let mut title = path
        .file_stem()
        .map(|stem| stem.to_string_lossy().to_string())
        .unwrap_or_default();
    let mut body_lines: Vec<&str> = Vec::new();
    let mut heading_taken = false;
    for line in trimmed.lines() {
        if !heading_taken {
            if let Some(rest) = line.strip_prefix("# ") {
                let text = rest.trim();
                if !text.is_empty() {
                    title = text.to_string();
                    heading_taken = true;
                    continue;
                }
            }
        }
        body_lines.push(line);
    }
    let body = truncate_chars(body_lines.join("\n").trim(), MAX_FILE_CHARS);
    let skip = if body.chars().count() < MIN_BODY_CHARS {
        Some(format!("正文只有 {} 字，太短", body.chars().count()))
    } else {
        sensitive_hit(&body).map(|label| format!("含{label}"))
    };
    Pending {
        source: String::new(),
        path: path.to_path_buf(),
        project,
        session: None,
        created_at: None,
        title,
        body,
        skip,
    }
}

// ── 会话记录 ─────────────────────────────────────────────────

/// 从一行 JSON 里挖出「谁说了什么、什么时候说的」。
///
/// 各家的会话记录格式不统一（Claude Code 的 `{type,message:{role,content}}`、
/// Codex 的 `{type,payload}`、自制的 `{role,content}`），但有一个共同点：
/// 文本最终落在 `text` 或字符串形态的 `content` 里。所以不做逐格式适配，
/// 只做三件事 —— 判角色、递归收文本、尽力取时间。多收进来一点噪声，
/// 比漏掉半段对话好。
///
/// ── 为什么还要滤掉 `<system-reminder>` / `<environment_context>` ──
/// 这些是客户端注入的「上下文信封」，不是用户或助手说的话。原样保留会让每条
/// 记忆都顶着一大段模板噪声，检索命中它反而挤掉真正的结论。判据抄 AgentHub 的
/// `isHarnessNoise`：整段以这两个标签开头，就当这条不是对话内容。
fn line_to_turn(line: &str) -> Option<(String, String, Option<i64>)> {
    let value: Value = serde_json::from_str(line.trim()).ok()?;
    let role = value
        .get("role")
        .and_then(Value::as_str)
        .or_else(|| value.get("message").and_then(|msg| msg.get("role")).and_then(Value::as_str))
        .or_else(|| value.get("type").and_then(Value::as_str))
        .unwrap_or("")
        .to_ascii_lowercase();
    let speaker = match role.as_str() {
        "user" | "human" => "我",
        "assistant" | "ai" | "model" | "bot" => "助手",
        // 工具调用 / 系统提示 / 元数据行：不是对话内容
        _ => return None,
    };
    let mut parts: Vec<String> = Vec::new();
    collect_text(&value, &mut parts);
    let text = parts.join("\n").trim().to_string();
    if text.chars().count() < 8 {
        return None;
    }
    // 注入信封不是对话：判据是「整段以标签开头」，只截断正文里的标签不算
    if is_harness_noise(&text) {
        return None;
    }
    let created = parse_time(&value);
    Some((speaker.to_string(), text, created))
}

/// 客户端注入的上下文信封（不是对话内容）。与 AgentHub `isHarnessNoise` 同口径。
fn is_harness_noise(text: &str) -> bool {
    let trimmed = text.trim_start();
    trimmed.starts_with("<system-reminder")
        || trimmed.starts_with("<environment_context")
        || trimmed.starts_with("<environment-context")
}

/// 从一行 JSON 里尽力取「原始发生时间」的毫秒时间戳。
///
/// 各家字段名不一：`timestamp` / `createdAt` / `created_at` / `time` / `ts`，
/// 值可能是毫秒数、秒数、ISO 8601 字符串。全取不到就返回 `None`，由写路径
/// 兜底为写入时刻 —— 少一条的精确时间，好过瞎赋值把它挂到错误的日期上。
fn parse_time(value: &Value) -> Option<i64> {
    let candidates = ["timestamp", "createdAt", "created_at", "time", "ts", "created"];
    // `message` 信封里的时间也算（Claude Code 偶尔把 timestamp 放在 message 下）
    let nested = value.get("message");
    for key in candidates {
        if let Some(stamp) = value.get(key).and_then(scalar_to_ms) {
            return Some(stamp);
        }
        if let Some(stamp) = nested.and_then(|msg| msg.get(key)).and_then(scalar_to_ms) {
            return Some(stamp);
        }
    }
    None
}

/// 把一个 JSON 标量折成毫秒时间戳：数字（自动分辨秒/毫秒）、ISO 8601 字符串。
fn scalar_to_ms(value: &Value) -> Option<i64> {
    match value {
        Value::Number(number) => number.as_i64().and_then(normalize_epoch),
        Value::String(text) => parse_iso_ms(text.trim()).or_else(|| text.trim().parse::<i64>().ok().and_then(normalize_epoch)),
        _ => None,
    }
}

/// 秒/毫秒统一成毫秒：10 位是秒，13 位是毫秒。太小或太大的直接丢掉，
/// 免得把 0 或一个明显离谱的值当成真实时间。
fn normalize_epoch(stamp: i64) -> Option<i64> {
    let ms = if stamp.abs() < 10_000_000_000 {
        stamp * 1000 // 秒 → 毫秒
    } else {
        stamp
    };
    // 1980-01-01 ~ 2100-01-01 的合理区间，之外当成解析失败
    const MIN_MS: i64 = 315_532_800_000;
    const MAX_MS: i64 = 4_102_444_800_000;
    if (MIN_MS..MAX_MS).contains(&ms) {
        Some(ms)
    } else {
        None
    }
}

/// 解析 ISO 8601 / 常见日期串到毫秒（按 UTC 解释；带 `Z` 或偏移的按偏移算）。
fn parse_iso_ms(text: &str) -> Option<i64> {
    use chrono::{DateTime, NaiveDate, NaiveDateTime, TimeZone, Utc};
    if let Ok(dt) = DateTime::parse_from_rfc3339(text) {
        return Some(dt.timestamp_millis());
    }
    // `2024-01-02T03:04:05`（无时区，按 UTC 解释）
    for pattern in ["%Y-%m-%dT%H:%M:%S%.f", "%Y-%m-%d %H:%M:%S%.f", "%Y-%m-%dT%H:%M:%S", "%Y-%m-%d %H:%M:%S"] {
        if let Ok(naive) = NaiveDateTime::parse_from_str(text, pattern) {
            return Some(Utc.from_utc_datetime(&naive).timestamp_millis());
        }
    }
    if let Ok(date) = NaiveDate::parse_from_str(text, "%Y-%m-%d") {
        let naive = date.and_hms_opt(0, 0, 0)?;
        return Some(Utc.from_utc_datetime(&naive).timestamp_millis());
    }
    None
}

/// 递归收文本：只认 `text` / `content` / `message` 三个键，且不重复收同一段
/// （父节点已经把 `content` 收过时，子节点里的 `text` 是同一条）。
///
/// 为什么要专门挖 `message`：Claude Code 的会话记录是
/// `{"type":"user","message":{"role":"user","content":[{"type":"text","text":"…"}]}}`
/// —— 对话正文包在 `message` 里一层。不挖这一层，这种格式的行一条都读不出来，
/// 表现为「导入 0 条」，而不是报错，最难查。
fn collect_text(value: &Value, out: &mut Vec<String>) {
    match value {
        Value::Object(map) => {
            for (key, item) in map {
                match (key.as_str(), item) {
                    ("text", Value::String(text)) => push_unique(out, text),
                    ("content", Value::String(text)) => push_unique(out, text),
                    // `content` 是数组时逐项递归（Anthropic 的块结构）
                    ("content", Value::Array(items)) => {
                        for sub in items {
                            collect_text(sub, out);
                        }
                    }
                    // `message` 是会话信封，正文在它下面
                    ("message", Value::Object(_)) => collect_text(item, out),
                    // 其余键一律不深挖：`toolUseResult`、`usage` 这些里没有对话
                    _ => {}
                }
            }
        }
        Value::Array(items) => {
            for item in items {
                collect_text(item, out);
            }
        }
        _ => {}
    }
}

fn push_unique(out: &mut Vec<String>, text: &str) {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return;
    }
    if out.iter().any(|seen| seen == trimmed) {
        return;
    }
    out.push(trimmed.to_string());
}

/// 一个会话文件 → 多条记忆（按 `CHUNK_CHARS` 定长窗口切）。
///
/// ── 为什么从「一个文件一条」改成「窗口切条」─────────────────
/// 旧实现把一个会话压成一条、超长砍头留尾（`MAX_FILE_CHARS`），实测本机这批
/// 数据只落盘 5.6%，丢掉 94% 内容。改成按窗口切之后 100% 覆盖原文，每条又不会
/// 大到挤掉检索里的其它记忆。
///
/// ── 为什么切点落在轮次边界 ──────────────────────────────────
/// 以「一整段发言」为最小不可分割单位往窗口里塞，切点永远落在两条发言之间，
/// 不会把一句话拦腰截断。相邻窗口留 `CHUNK_OVERLAP` 字重叠，让跨边界的那句话
/// 两边的窗口都能检索到。单条发言本身超过一个窗口时，才在它内部再切。
///
/// 每个窗口都带同一次会话的 `session` 与「窗内最早的原始时间」，于是检索能
/// 串回上下文、时间轴上落在真实发生的那天，而不是导入当天。
fn from_transcript(path: &Path, project: Option<String>) -> Vec<Pending> {
    let session = path
        .file_stem()
        .map(|stem| stem.to_string_lossy().to_string())
        .filter(|stem| !stem.is_empty());
    let raw = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(error) => {
            return vec![Pending {
                source: String::new(),
                path: path.to_path_buf(),
                project,
                session: session.clone(),
                created_at: None,
                title: String::new(),
                body: String::new(),
                skip: Some(format!("读不了：{error}")),
            }]
        }
    };
    // 收成「一段发言一行」的片段：`(格式化正文, 该段原始时间)`。超长单段先单独切碎，
    // 保证后面按窗口打包时每片都不会撑爆窗口。
    let mut segments: Vec<(String, Option<i64>)> = Vec::new();
    for line in raw.lines() {
        if line.trim().is_empty() {
            continue;
        }
        if let Some((speaker, text, created)) = line_to_turn(line) {
            let formatted = format!("**{speaker}**：{text}");
            for piece in split_long(&formatted, CHUNK_CHARS) {
                segments.push((piece, created));
            }
        }
    }
    let title_base = session.clone().unwrap_or_else(|| "会话记录".to_string());
    let total_chars: usize = segments.iter().map(|(text, _)| text.chars().count()).sum();
    if segments.is_empty() || total_chars < MIN_BODY_CHARS {
        return vec![Pending {
            source: String::new(),
            path: path.to_path_buf(),
            project,
            session,
            created_at: None,
            title: title_base,
            body: String::new(),
            skip: Some("没有可用的对话内容".to_string()),
        }];
    }

    let windows = pack_windows(&segments);
    let count = windows.len();
    let mut out: Vec<Pending> = Vec::with_capacity(count);
    for (index, window) in windows.iter().enumerate() {
        let body = window
            .iter()
            .map(|&slot| segments[slot].0.as_str())
            .collect::<Vec<_>>()
            .join("\n\n");
        // 窗内最早出现的原始时间（找不到就交给写路径兜底为写入时刻）
        let created_at = window.iter().find_map(|&slot| segments[slot].1);
        let title = if count == 1 {
            title_base.clone()
        } else {
            format!("{title_base}（{}）", index + 1)
        };
        let skip = if body.chars().count() < MIN_BODY_CHARS {
            Some("窗口内容太短".to_string())
        } else {
            sensitive_hit(&body).map(|label| format!("含{label}"))
        };
        out.push(Pending {
            source: String::new(),
            path: path.to_path_buf(),
            project: project.clone(),
            session: session.clone(),
            created_at,
            title,
            body,
            skip,
        });
    }
    out
}

/// 把一段超长文本按 `limit` 字数切开（相邻片重叠 `CHUNK_OVERLAP/2`）。
fn split_long(text: &str, limit: usize) -> Vec<String> {
    let chars: Vec<char> = text.chars().collect();
    let total = chars.len();
    if total <= limit || limit == 0 {
        return vec![text.to_string()];
    }
    let step = limit.saturating_sub(CHUNK_OVERLAP / 2).max(1);
    let mut out: Vec<String> = Vec::new();
    let mut start = 0usize;
    while start < total {
        let end = (start + limit).min(total);
        out.push(chars[start..end].iter().collect());
        if end >= total {
            break;
        }
        start += step;
    }
    out
}

/// 按 `CHUNK_CHARS` 把片段序列打包成若干窗口（每个窗口是一串片段下标）。
///
/// 关一个窗口时，从末尾挑「总字数不超过重叠预算、且给下一段留足了空间」的若干片
/// 带进下一个窗口，形成重叠上下文。至少带一片（哪怕它本身超过重叠预算），保证
/// 相邻窗口不是硬切；但绝不会带进去之后放不下下一段 —— 那样会产出一个只含重复
/// 内容、没有任何新信息的空窗口。
fn pack_windows(segments: &[(String, Option<i64>)]) -> Vec<Vec<usize>> {
    let mut windows: Vec<Vec<usize>> = Vec::new();
    let mut current: Vec<usize> = Vec::new();
    let mut current_chars = 0usize;
    for (index, (text, _)) in segments.iter().enumerate() {
        let chars = text.chars().count();
        if !current.is_empty() && current_chars + chars > CHUNK_CHARS {
            let overlap = trailing_overlap(&current, segments, CHUNK_OVERLAP, chars);
            windows.push(std::mem::take(&mut current));
            current_chars = overlap.iter().map(|&slot| segments[slot].0.chars().count()).sum();
            current = overlap;
        }
        current.push(index);
        current_chars += chars;
    }
    if !current.is_empty() {
        windows.push(current);
    }
    windows
}

/// 取 `current` 末尾若干片做重叠：总字数既不超过 `budget`，也不超过「窗口上限
/// 减去即将进窗那一整段的字数」。至少一片（若这一片本身就是整个窗口大小，也不
/// 硬塞，宁可无重叠）。
fn trailing_overlap(
    current: &[usize],
    segments: &[(String, Option<i64>)],
    budget: usize,
    incoming: usize,
) -> Vec<usize> {
    // 留给重叠的空间 = min(重叠预算, 窗口上限 - 即将进窗那一段)。incoming 已受
    // split_long 约束不会超过 CHUNK_CHARS，所以这个差至少为 0。
    let allowance = budget.min(CHUNK_CHARS.saturating_sub(incoming));
    let mut out: Vec<usize> = Vec::new();
    let mut used = 0usize;
    for &slot in current.iter().rev() {
        let chars = segments[slot].0.chars().count();
        if !out.is_empty() && used + chars > allowance {
            break;
        }
        // 连一片都放不下（allowance 太小）时，宁可不重叠
        if out.is_empty() && chars > allowance {
            break;
        }
        out.push(slot);
        used += chars;
        if used >= allowance {
            break;
        }
    }
    out.reverse();
    out
}

// ── 扫描 ─────────────────────────────────────────────────────

/// 单个来源一次最多吃多少个文件。
///
/// 上限不是「性能优化」而是「别一次写爆」：`~/.claude/projects` 攒上一年能有
/// 上万个会话文件，全导进去既拖慢检索也让第一次导入无法收尾。超出的部分下次
/// 再导 —— hash 去重保证已经进来的不会重复。
const MAX_FILES_PER_SOURCE: usize = 800;

/// 一个来源扫出来的结果：来源摘要 + 匹配到的文件。
struct Scanned {
    source: Source,
    exists: bool,
    /// 全部匹配到的文件（未截断，页面要显示真实数量）
    files: Vec<PathBuf>,
}

/// 只列文件、不读正文。
///
/// ── 为什么要把「列文件」和「读正文」拆开 ────────────────────
/// 导入页每次打开都要显示「每个来源有几个文件」，而读正文是这条链路里最贵的
/// 一步（一次会话记录可能几 MB）。为了数个数把每个文件都读进来解析一遍，等于
/// 打开页面就做了一次全量干跑 —— 几百个会话记录能让页面卡上十几秒。
fn scan(only: Option<&str>) -> Vec<Scanned> {
    let mut out: Vec<Scanned> = Vec::new();
    // `only` 来自页面回传的路径，与配置里存的那条可能只差分隔符或大小写
    // （`C:\Users\me\.claude\projects` vs `C:/Users/me/.claude/projects`）。
    // 裸字符串比较会让「点某张卡片的导入」静默变成「一条都不读」——
    // 用户看到的是「扫描 0 个文件」而不是报错，最难查。
    for source in sources() {
        if let Some(filter) = only {
            if !same_path(&source.path, filter) {
                continue;
            }
        }
        let root = source.root();
        let exists = root.exists();
        let mut found: Vec<PathBuf> = Vec::new();
        if root.is_file() {
            found.push(root.clone());
        } else if root.is_dir() {
            walk(&root, 0, &mut found);
        }
        // 关掉的来源仍然出现在列表里（页面要显示它、并给出开关），
        // 但一条都不列 —— 「关」必须是真的不碰盘，不能只是页面上灰掉。
        let files = if source.enabled {
            found
                .into_iter()
                .filter(|file| match source.format.as_str() {
                    "jsonl" => is_transcript(file),
                    _ => is_note(file),
                })
                .collect()
        } else {
            Vec::new()
        };
        out.push(Scanned { source, exists, files });
    }
    out
}

/// 来源摘要（页面顶部那几行）。
///
/// `files` 是**真实**匹配数，`take` 是本次会读的上限 —— 两个都给，页面才能
/// 说清「有 3200 个，这次导前 800 个」，而不是让用户以为只有 800 个。
fn sources_json(scanned: &[Scanned]) -> Vec<Value> {
    scanned
        .iter()
        .map(|item| {
            json!({
                "path": item.source.path,
                "label": item.source.label(),
                "format": item.source.format,
                "enabled": item.source.enabled,
                "priority": item.source.priority,
                "exists": item.exists,
                "files": item.files.len(),
                "take": item.files.len().min(MAX_FILES_PER_SOURCE),
                "config": item.source.to_config(),
            })
        })
        .collect()
}

/// 收集全部候选（读正文，但不写盘）。
fn collect(only: Option<&str>) -> (Vec<Pending>, Vec<Value>) {
    let scanned = scan(only);
    let mut candidates: Vec<Pending> = Vec::new();
    for item in &scanned {
        // 项目归属：目录名。会话记录目录常常是「~/.claude/projects/<编码路径>/」，
        // 直接取最后一级会把一串编码当项目名，所以只取来源本身的目录名。
        let project = Some(item.source.label());
        for file in item.files.iter().take(MAX_FILES_PER_SOURCE) {
            // 会话记录一个文件切出多个窗口（多条待导入），Markdown 一个文件一条。
            let mut produced: Vec<Pending> = if item.source.format == "jsonl" {
                from_transcript(file, project.clone())
            } else {
                vec![from_markdown(file, project.clone())]
            };
            for mut candidate in produced.drain(..) {
                candidate.source = item.source.path.clone();
                candidates.push(candidate);
            }
        }
    }
    (candidates, sources_json(&scanned))
}

/// 干跑：只读文件，返回「会导入什么、跳过什么」。**不写任何文件、不改任何配置。**
///
/// `limit` 只限制**列出来**的条数，不影响 `ready` —— 那个数字必须是真的会写入
/// 多少条。早先拿 `importable.len()` 当 `ready`，页面上就会出现「干跑说 60 条、
/// 实际写了 800 条」，而这正是干跑最不该错的地方。
pub fn preview(only: Option<&str>, limit: usize) -> Value {
    let (candidates, rows) = collect(only);
    let limit = limit.clamp(1, 500);
    // 干跑必须和真导入用**同一条跳过规则**，否则「预览说跳过 50 条、导入却写了」
    // 这种不一致会让用户再也不信预览。敏感内容这一条取决于开关：关掉之后
    // 含密钥的条目照导，只有「太短」这类与隐私无关的仍然跳过。
    let sensitive_skip = config::bool_of("import.sensitiveSkip");
    let mut importable: Vec<Value> = Vec::new();
    let mut skipped: Vec<Value> = Vec::new();
    let mut duplicates = 0usize;
    let mut ready = 0usize;
    // 预览要回答「会写入多少条」，而这个数字取决于库里已有什么 —— 所以这里
    // 也要带上现存 l1 的 hash，否则干跑会把「早就导过、只是这次又被扫到」的
    // 文件也算进 ready，用户点导入之后发现数字对不上。
    let dedup_on = config::bool_of("dedup.enabled") && config::bool_of("dedup.l1.enabled");
    let mut known: std::collections::HashSet<String> = if dedup_on {
        store::load_all()
            .into_iter()
            .filter(|entry| entry.is_live() && entry.layer == model::Layer::L1)
            .map(|entry| entry.hash)
            .filter(|hash| !hash.is_empty())
            .collect()
    } else {
        std::collections::HashSet::new()
    };
    for candidate in &candidates {
        let row = json!({
            "path": display_path(&candidate.path),
            "title": truncate_chars(&candidate.title, 80),
            "project": candidate.project,
            // 多来源时用户要能看出这条是哪个目录来的（同一篇笔记可能在两处都有）
            "source": candidate.source,
            "chars": candidate.body.chars().count(),
            "preview": truncate_chars(&candidate.body, 160),
        });
        if let Some(reason) = &candidate.skip {
            let privacy = reason.starts_with("含");
            if !(privacy && !sensitive_skip) {
                if skipped.len() < limit {
                    let mut row = row;
                    row["reason"] = json!(reason);
                    skipped.push(row);
                }
                continue;
            }
        }
        // 内容相同的只算一条（跨来源重复很常见：同一个仓库被两处记过；
        // 库里已有的也要排除，否则「会导入 800 条」每次都会说 800）。
        // 去重关掉时不做这个判断 —— 预览必须复现真导入的行为。
        if dedup_on {
            let hash = content_hash(&candidate.body);
            if !known.insert(hash) {
                duplicates += 1;
                continue;
            }
        }
        ready += 1;
        if importable.len() < limit {
            importable.push(row);
        }
    }
    let skipped_count = candidates
        .iter()
        .filter(|item| match item.skip.as_deref() {
            Some(reason) => !(reason.starts_with("含") && !sensitive_skip),
            None => false,
        })
        .count();
    let payload = json!({
        "sources": rows,
        "importable": importable,
        "skipped": skipped,
        "ready": ready,
        // 列表被 limit 截断时告诉页面「还有多少没列出来」
        "listed": importable.len(),
        "skippedCount": skipped_count,
        "duplicates": duplicates,
        "scanned": candidates.len(),
        "dryRunFirst": config::bool_of("import.dryRunFirst"),
        "sensitiveSkip": sensitive_skip,
        "batchSize": config::usize_of("import.batchSize"),
    });
    // 记一次「刚预览过」，供 `run` 判断用户是不是真的看过。进程内变量，不落盘。
    LAST_PREVIEW_AT.store(crate::server::logging::now_ms(), Ordering::Relaxed);
    payload
}

/// 真导入。分批写，每批之间可以被打断 —— 已写入的条目都是完整的。
///
/// ── 为什么要走 `write_batch` 而不是逐条 `write` ─────────────
/// `write` 每条的精确去重要全盘读一次记忆库。单条没问题，但一次导入几千条时
/// 就是 O(n²)：几百条存量记忆 × 几千条导入 = 上百万次文件读，请求会挂住。
/// `write_batch` 把「现存 l1 的 hash」收成集合只读一次，语义不变。
pub fn run(only: Option<&str>) -> Result<Value, String> {
    if config::bool_of("import.dryRunFirst") {
        let previewed = LAST_PREVIEW_AT.load(Ordering::Relaxed);
        let fresh = previewed > 0 && crate::server::logging::now_ms() - previewed < PREVIEW_TTL_MS;
        if !fresh {
            return Err("请先干跑预览（30 分钟内有效），确认要导入哪些内容".into());
        }
    }
    let sensitive_skip = config::bool_of("import.sensitiveSkip");
    let batch = config::usize_of("import.batchSize").clamp(20, 2000);
    let (candidates, _) = collect(only);
    let scanned = candidates.len();
    let mut skipped = 0usize;
    let mut reasons: Vec<String> = Vec::new();

    // 先把能写的挑出来，记下每条对应哪个文件（失败时要把文件名报给用户）
    let mut queue: Vec<(String, WriteInput)> = Vec::new();
    for candidate in &candidates {
        if candidate.skip.is_some() {
            // 敏感内容开关关掉时，被标记为「含密钥」的条目也照导 —— 但只限这一类，
            // 「太短」这类与隐私无关的跳过仍然生效。
            let privacy = candidate
                .skip
                .as_deref()
                .is_some_and(|reason| reason.starts_with("含"));
            if !(privacy && !sensitive_skip) {
                skipped += 1;
                continue;
            }
        }
        queue.push((
            display_path(&candidate.path),
            WriteInput {
                content: candidate.body.clone(),
                title: Some(candidate.title.clone()),
                kind: Some("daily".to_string()),
                layer: Some("l1".to_string()),
                project: candidate.project.clone(),
                agent: Some("import".to_string()),
                tags: vec!["导入".to_string()],
                importance: Some(0.4),
                files: Vec::new(),
                supersedes: Vec::new(),
                pinned: Some(false),
                starred: Some(false),
                cwd: None,
                session: candidate.session.clone(),
                created_at: candidate.created_at,
                valid_to: None,
                evidence: Vec::new(),
            },
        ));
    }

    let mut written = 0usize;
    let mut duplicated = 0usize;
    let mut failed = 0usize;
    // 一个写入器贯穿整次导入：hash 集合只建一次，之后每批都复用它。
    // 分批只是为了在批与批之间让出线程，不该让去重退回 O(n²)。
    let mut writer = super::BatchWriter::new();
    // 分批写：`drain` 而不是 `chunks`，避免把每条正文再克隆一遍
    // （800 条 × 每条最多 24k 字，克隆一轮就是几十 MB 的瞬时内存）。
    while !queue.is_empty() {
        let take = batch.max(1).min(queue.len());
        // 每批开一段延迟落盘窗口：本批内对同一 l1 容器的多次追加攒在内存，
        // 批末统一写一次盘。这是切细粒度后（几千条挤同一天容器）不退化到
        // O(n²) 写放大的关键。窗口按批开关，而不是整次导入一个窗口 ——
        // 这样即便中途 panic，已完成批次也已落盘，不会整批丢在内存里。
        super::store::begin_deferred();
        for (path, input) in queue.drain(..take) {
            match writer.write(input) {
                // `write` 命中 hash 去重时会返回 `duplicate: true` 且不新增条目
                Ok(value) if value.get("duplicate").and_then(Value::as_bool).unwrap_or(false) => {
                    duplicated += 1;
                }
                Ok(_) => written += 1,
                Err(error) => {
                    failed += 1;
                    if reasons.len() < 8 {
                        reasons.push(format!("{path}：{error}"));
                    }
                }
            }
        }
        // 关窗口 = 把本批攒下的容器统一落盘。写失败的文件也计入 failed 并留原因
        // —— 它们是记忆真正没落盘的部分，不能当成功。
        for error in writer.flush() {
            failed += 1;
            if reasons.len() < 8 {
                reasons.push(format!("落盘失败 {error}"));
            }
        }
        std::thread::yield_now();
    }

    let now = crate::server::logging::now_ms();
    let summary = json!({
        "written": written,
        "duplicated": duplicated,
        "skipped": skipped,
        "failed": failed,
        "scanned": scanned,
        "reasons": reasons,
        "at": now,
        "atText": stamp_text(now),
    });
    index::global().sync_set(LAST_RUN_KEY, &summary.to_string());
    index::global().sync_log_add("local", "import", &format!("导入 {written} 条，跳过 {skipped} 条"), failed == 0);
    crate::server::logging::log(
        "[Memory]",
        &format!("历史导入完成：写入 {written} 条，重复 {duplicated}，跳过 {skipped}，失败 {failed}"),
    );
    Ok(summary)
}

/// 上次导入结果（页面顶部）。
pub fn last_run() -> Value {
    index::global()
        .sync_get(LAST_RUN_KEY)
        .and_then(|(raw, _)| serde_json::from_str::<Value>(&raw).ok())
        .unwrap_or(Value::Null)
}

/// 页面用的完整视图：来源清单 + 上次结果 + 关键开关。
///
/// 这里**不读正文**：页面上只需要「每个来源有几个文件」。要真数字就走干跑。
pub fn view() -> Value {
    let scanned = scan(None);
    let rows = sources_json(&scanned);
    // 页面上的「N 个文件」用真实匹配数；本次能读多少由 take 说明
    let files: usize = scanned.iter().map(|item| item.files.len()).sum();
    let take: usize = scanned.iter().map(|item| item.files.len().min(MAX_FILES_PER_SOURCE)).sum();
    json!({
        "sources": rows,
        "lastRun": last_run(),
        // 只数文件，不判断能不能导 —— 精确的条数在干跑里给。
        // 这里**故意不返回 skippedCount**：不读正文就不知道谁会被跳过，
        // 硬给一个 0 只会让页面显示「跳过 0 条」这种假消息。
        "scanned": files,
        "take": take,
        "dryRunFirst": config::bool_of("import.dryRunFirst"),
        "sensitiveSkip": config::bool_of("import.sensitiveSkip"),
        "batchSize": config::usize_of("import.batchSize"),
        "note": if rows.is_empty() {
            "还没有配置导入来源。加一个目录（Markdown 笔记）或会话记录目录（jsonl），先干跑看看会导入什么。"
        } else {
            "导入的内容按「Markdown 一文件一条 / 会话记录按窗口切条」进 l1 流水，项目按来源目录名归属，原始发生时间透传；重复内容由 hash 去重。"
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn source_parses_all_four_fields() {
        let source = Source::parse("D:/notes|md|false|7").expect("应能解析");
        assert_eq!(source.path, "D:/notes");
        assert_eq!(source.format, "md");
        assert!(!source.enabled);
        assert_eq!(source.priority, 7);
        assert_eq!(source.to_config(), "D:/notes|md|false|7");
    }

    #[test]
    fn source_guesses_format_from_extension() {
        assert_eq!(Source::parse("C:/a/b.jsonl").map(|item| item.format), Some("jsonl".to_string()));
        assert_eq!(Source::parse("C:/notes").map(|item| item.format), Some("md".to_string()));
    }

    #[test]
    fn blank_source_is_dropped() {
        assert!(Source::parse("   ").is_none());
        assert!(Source::parse("|md|true|0").is_none());
    }

    #[test]
    fn sensitive_rules_catch_obvious_secrets() {
        assert!(sensitive_hit("key = sk-abcdefghijklmnopqrstuvwxyz012345").is_some());
        assert!(sensitive_hit("-----BEGIN RSA PRIVATE KEY-----").is_some());
        assert!(sensitive_hit("password: hunter2hunter2").is_some());
        assert!(sensitive_hit("用 npm ci 重装依赖，不要 rm -rf node_modules").is_none());
    }

    #[test]
    fn transcript_line_keeps_roles_and_drops_metadata() {
        let user = r#"{"type":"user","message":{"role":"user","content":[{"type":"text","text":"帮我把端口改成 3065"}]}}"#;
        let (speaker, text, _) = line_to_turn(user).expect("用户行应被识别");
        assert_eq!(speaker, "我");
        assert!(text.contains("3065"));

        let assistant = r#"{"role":"assistant","content":"改好了，端口在 config.json 里"}"#;
        assert_eq!(line_to_turn(assistant).map(|item| item.0), Some("助手".to_string()));

        // 元数据行不该变成一条对话
        assert!(line_to_turn(r#"{"type":"summary","summary":"x"}"#).is_none());
        assert!(line_to_turn("not json").is_none());
    }

    /// 客户端注入的上下文信封不是对话内容，必须整段滤掉。
    #[test]
    fn transcript_drops_harness_noise() {
        let noise = r#"{"type":"user","message":{"role":"user","content":[{"type":"text","text":"<system-reminder>this is a reminder block</system-reminder>"}]}}"#;
        assert!(line_to_turn(noise).is_none(), "注入信封不该变成一条对话");
        let env = r#"{"role":"user","content":"<environment_context>cwd=/x</environment_context>"}"#;
        assert!(line_to_turn(env).is_none());
    }

    /// 原始时间要能透传：数字毫秒 / 数字秒 / ISO 字符串三种形态都认。
    #[test]
    fn transcript_carries_original_time() {
        let ms = 1_700_000_000_000i64;
        let a = format!(r#"{{"role":"user","content":"端口固定 3065，别改","timestamp":{ms}}}"#);
        assert_eq!(line_to_turn(&a).and_then(|item| item.2), Some(ms));

        let seconds = 1_700_000_000i64; // 秒 → 毫秒
        let b = format!(r#"{{"role":"user","content":"端口固定 3065，别改","created_at":{seconds}}}"#);
        assert_eq!(line_to_turn(&b).and_then(|item| item.2), Some(ms));

        let iso = r#"{"role":"user","content":"端口固定 3065，别改","timestamp":"2023-11-14T22:13:20Z"}"#;
        assert_eq!(line_to_turn(iso).and_then(|item| item.2), Some(ms));

        // 没有时间字段：返回 None（由写路径兜底）
        let none = r#"{"role":"user","content":"端口固定 3065，别改"}"#;
        assert_eq!(line_to_turn(none).and_then(|item| item.2), None);
    }

    /// 一个长会话要被切成多个窗口，而不是压成一条或砍头留尾。
    #[test]
    fn transcript_splits_into_windows() {
        let dir = std::env::temp_dir().join(format!("mem-import-chunk-{}", crate::server::logging::now_ms()));
        std::fs::create_dir_all(&dir).ok();
        let file = dir.join("session.jsonl");
        // 造 40 条每条约 500 字的用户发言 ≈ 2 万字，应切出多条窗口
        let mut lines: Vec<String> = Vec::new();
        for index in 0..40 {
            let body = format!("第{index}段：{}", "内容".repeat(250));
            lines.push(format!(
                r#"{{"role":"user","content":"{}","timestamp":{}}}"#,
                body,
                1_700_000_000_000i64 + index as i64 * 1000
            ));
        }
        std::fs::write(&file, lines.join("\n")).ok();
        let windows = from_transcript(&file, Some("notes".to_string()));
        assert!(windows.len() > 1, "两万字应切出多条窗口，实得 {}", windows.len());
        // 每个窗口都带同一次会话的 session，且都带得回原始时间
        assert!(windows.iter().all(|window| window.session.as_deref() == Some("session")));
        assert!(windows.iter().all(|window| window.created_at.is_some()));
        // 全部可导入（无跳过）
        assert!(windows.iter().all(|window| window.skip.is_none()));
        // 覆盖不丢内容：窗口正文总字数明显超过单个窗口上限（因为重叠+全覆盖）
        let total: usize = windows.iter().map(|window| window.body.chars().count()).sum();
        assert!(total >= 20_000 * 9 / 10, "窗口应基本覆盖全部原文，实得 {total} 字");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn markdown_title_falls_back_and_heading_is_stripped() {
        let dir = std::env::temp_dir().join(format!("mem-import-{}", crate::server::logging::now_ms()));
        std::fs::create_dir_all(&dir).ok();
        let file = dir.join("端口约定.md");
        std::fs::write(&file, "# 端口固定 3065\n\n网关端口不要改，客户端配置写死了。\n").ok();
        let candidate = from_markdown(&file, Some("notes".to_string()));
        assert_eq!(candidate.title, "端口固定 3065");
        assert!(!candidate.body.contains("# 端口固定 3065"), "H1 不该在正文里出现第二次");
        assert!(candidate.skip.is_none());
        std::fs::remove_dir_all(&dir).ok();
    }

    /// 短句（「ok」「继续」）该被跳过，但**成句的短中文**不该 —— 这条把两个方向
    /// 都钉住，防止有人把门槛又调回按字节算。
    #[test]
    fn short_filler_is_skipped_but_short_sentence_is_kept() {
        let dir = std::env::temp_dir().join(format!("mem-import-short-{}", crate::server::logging::now_ms()));
        std::fs::create_dir_all(&dir).ok();

        let filler = dir.join("ok.md");
        std::fs::write(&filler, "# ok\n\n好的\n").ok();
        let candidate = from_markdown(&filler, None);
        assert!(candidate.skip.is_some(), "纯应答不该被当成记忆");

        let sentence = dir.join("端口.md");
        std::fs::write(&sentence, "# 端口\n\n网关端口固定 3065，客户端写死了。\n").ok();
        let candidate = from_markdown(&sentence, None);
        assert!(candidate.skip.is_none(), "短但有信息的句子应当保留");

        std::fs::remove_dir_all(&dir).ok();
    }

    /// 页面示例写的是 `~/...`，`PathBuf::from` 不展开 `~`。不测这一条的话，
    /// 用户照着提示填完只会看到「路径不存在」。
    #[test]
    fn tilde_source_expands_to_home() {
        let home = crate::server::core::system_info::home_dir();
        if home.as_os_str().is_empty() {
            return; // 拿不到主目录的机器上这条无从验证
        }
        let source = Source::parse("~/.claude/projects|jsonl|true|10").expect("应能解析");
        assert_eq!(source.root(), home.join(".claude").join("projects"));
        // 中间的 `~` 不是前缀，不该被展开
        let literal = Source::parse("D:/a~b|md|true|0").expect("应能解析");
        assert_eq!(literal.root(), PathBuf::from("D:/a~b"));
    }

    /// 批量写入器要把「本批里已经写过」的内容判成重复，而不是写第二份。
    #[test]
    fn batch_writer_dedups_within_one_batch() {
        let mut writer = super::super::BatchWriter::new();
        let body = format!("批量去重测试正文 {}", crate::server::logging::now_ms());
        let make = || super::super::WriteInput {
            content: body.clone(),
            title: Some("批量去重".to_string()),
            kind: Some("daily".to_string()),
            layer: Some("l1".to_string()),
            agent: Some("import-test".to_string()),
            importance: Some(0.1),
            pinned: Some(false),
            starred: Some(false),
            ..Default::default()
        };
        let first = writer.write(make()).expect("第一次应写入");
        assert!(!first.get("duplicate").and_then(Value::as_bool).unwrap_or(false));
        let second = writer.write(make()).expect("第二次不该报错");
        assert!(second.get("duplicate").and_then(Value::as_bool).unwrap_or(false), "同批同内容应判重复");
    }
}
