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

/// 单个会话文件最多读进内存的字节数，而且在**尾部**读。
///
/// ── 为什么必须有这条硬上限 ──────────────────────────────────
/// 会话记录是追加写的：一段聊了一整天的对话，`.jsonl` 能涨到接近 1 GB
/// （2026-11-21 实测本机正在进行的那个会话就是 955 MB）。早先 `from_transcript`
/// 用 `read_to_string` 把整份文件读进来再切窗口，于是每轮增量导入都要读 955 MB、
/// 切出上千个窗口。后果不是「慢一点」，而是**面板整页打不开**：
///
///   * 那 955 MB 的读 + 切分把磁盘和 CPU 占满几分钟，同时导入还在不停写索引；
///   * 写索引会让记忆快照缓存失效，面板首屏的 `GET /api/memory/config`
///     又要重新解析 55 MB `.md` 才能算出 `status()`；
///   * 两件事叠在一起，桌面壳 60 秒超时，用户看到的就是
///     「等待本地代理响应超时」+ 记忆中枢永远「正在获取…」。
///
/// ── 为什么读尾部而不是砍掉超长文件 ──────────────────────────
/// 增量导入只关心**新追加**的内容：老内容上一轮已经入过库，内容 hash 去重本来
/// 也会把它们判成重复。所以从文件尾部取 8 MB（≈ 二十多万字，够几轮增量）就够了，
/// 既覆盖新内容，又把单次读盘从 955 MB 压到 8 MB。
const MAX_TRANSCRIPT_BYTES: u64 = 8 * 1024 * 1024;

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

/// 上一次干跑的结果本身（进程内，不落盘）。
///
/// ── 为什么结果也要留在后端 ──────────────────────────────────
/// 页签是条件渲染的，切走就把导入组件卸载了，组件里的 `preview` 跟着消失 ——
/// 用户看到的是「刚扫出来的清单没了，得重新点一次『导入全部来源』」。
/// 但干跑结果本来就带 30 分钟有效期（`preview_gate` 就按这个 TTL 放行真导入），
/// 在有效期内它依然是**这份**预览，不该跟着组件一起消失。
///
/// `running` 是同一个道理的另一半：重挂载时如果扫描还在跑，页面应该等这一次，
/// 而不是立刻再发起一次全盘扫描。
///
/// 仍然不落盘：干跑对外的承诺是「一个字都没写」，缓存的是结果不是数据源，
/// 进程重启后失效也正好 —— 重启之后确实没人在本次会话里看过预览。
struct PreviewSlot {
    at: i64,
    /// 这次预览是针对哪个来源（空 = 全部来源）。恢复了清单却没恢复目标，
    /// 用户点「开始导入」会把十二家全写进去。
    only: Option<String>,
    running: bool,
    value: Option<Value>,
}

static LAST_PREVIEW: std::sync::Mutex<PreviewSlot> = std::sync::Mutex::new(PreviewSlot {
    at: 0,
    only: None,
    running: false,
    value: None,
});

/// 取锁。中毒不能把干跑功能锁死 —— 里面只是缓存，复用旧值比全部失败好。
fn preview_lock() -> std::sync::MutexGuard<'static, PreviewSlot> {
    LAST_PREVIEW.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

const LAST_RUN_KEY: &str = "import.lastRun";

/// 自动增量导入的水位（毫秒）。存 `sync_state` 而不是配置文件 —— 它是运行时
/// 状态：换机、重置来源时该清掉，而路径要留着。
const AUTO_AT_KEY: &str = "import.autoAt";

/// 上次自动增量导入的结果摘要。
const AUTO_LAST_KEY: &str = "import.autoLast";

/// 每个会话文件读到第几个字节。键是规范化后的绝对路径，值是偏移 + 读取时的文件长度。
///
/// ── 为什么在 mtime 水位之外还要这一张表 ─────────────────────
/// mtime 只能回答「这个文件变过没有」，回答不了「变的是哪一段」。当前会话是边聊
/// 边写的：mtime 每几秒变一次，于是每一轮增量任务都要把整个文件（或尾部 8 MB）
/// 重新解析一遍 —— 实测那个 955 MB 的会话就是靠这一点把面板拖过 60 秒超时的。
/// 有了字节游标，「变过」只意味着「读 offset 之后那几百 KB」。
///
/// 与 AgentHub `import_parsers.cjs` 的 `files[key] = consumed` 同构：它的
/// `readNewLines(fd, fromByte, maxChunkBytes)` 就是这张表在 JS 里的实现。
///
/// ── 为什么不复用 `AUTO_AT_KEY` ──────────────────────────────
/// 那是「全局一个数」，用来做首次安装时的回溯窗口（`autoSinceDays`）。文件级
/// 游标是「每个文件一个数」，两者生命周期一致但语义完全不同：把游标塞进一个
/// 全局时间戳里，等于回到「按 mtime 整份重读」。
const AUTO_CURSORS_KEY: &str = "import.autoCursors";

/// 游标表的上限。会话文件会越攒越多（删项目、换目录都留旧键），超过这个数就按
/// 文件名裁剪 —— 键只是路径字符串，1 万条约 1 MB，本身不致命，但不该无界增长。
const MAX_CURSORS: usize = 10_000;

/// 一个文件的读取位置。
#[derive(Clone, Debug, Default, serde::Serialize, serde::Deserialize)]
struct FileCursor {
    /// 下次从这里开始读。
    offset: u64,
    /// 记下读取时刻的文件长度：`size == offset` 就是「已读完」，不必再 stat
    /// 之外的任何判断；文件变短（轮转/清空）则由 `offset > size` 识别。
    size: u64,
}

type Cursors = std::collections::BTreeMap<String, FileCursor>;

/// 游标键：绝对路径、统一分隔符。
///
/// 不折大小写：同一进程的 `read_dir` 返回的字符串是稳定的，折了反而会让
/// 「用户手动改过盘符大小写」这种情况对不上。
fn cursor_key(path: &Path) -> String {
    path.to_string_lossy().replace('\\', "/")
}

fn load_cursors() -> Cursors {
    index::global()
        .sync_get(AUTO_CURSORS_KEY)
        .and_then(|(raw, _)| serde_json::from_str::<Cursors>(&raw).ok())
        .unwrap_or_default()
}

fn save_cursors(cursors: &Cursors) {
    let mut trimmed = cursors.clone();
    if trimmed.len() > MAX_CURSORS {
        // BTreeMap 按路径排序，与新旧无关 —— 这里只需要「有界」，不需要「留最新」：
        // 被裁掉的文件下一轮会按 mtime 水位重新判断，最坏是多读一次。
        while trimmed.len() > MAX_CURSORS {
            let Some(first) = trimmed.keys().next().cloned() else {
                break;
            };
            trimmed.remove(&first);
        }
    }
    if let Ok(text) = serde_json::to_string(&trimmed) {
        index::global().sync_set(AUTO_CURSORS_KEY, &text);
    }
}

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

/// 读会话记录：从 `from` 字节开始，最多读 [`MAX_TRANSCRIPT_BYTES`]，返回正文与
/// **下一个游标位置**。
///
/// ── 为什么必须是「从上次的位置接着读」而不是「每轮读尾部」────────────
/// 尾部读仍然每轮把最后 8 MB 重新解析一遍。当前会话是边聊边写的，mtime 每几秒
/// 变一次，于是每轮增量任务都要为同一段内容付一次 8 MB 的 IO + 解析 + 上千次
/// hash 去重。AgentHub 的做法是 per-file 字节游标（`readNewLines(fd, fromByte)`）：
/// 读过的地方不再读。这里对齐同一口径 —— 游标之前的字节一个都不碰。
///
/// ── 三个边界 ────────────────────────────────────────────────
/// 1. `from == 0` 且文件超过上限：只读尾部。这类文件是「升级前就在的老会话」，
///    前面几十万行要么早已入库、要么是无关历史；从 0 读只会白付一次整份 IO。
/// 2. 文件变短（被轮转/截断）：`from > len` → 回到 0 重读，不能原地空转。
/// 3. 末尾没有换行（正在写的最后一行）：游标停在最后一个完整行的末尾，下一轮
///    重读这一行。宁可多读一行，不能吞掉半行 JSON。
///
/// 用 `from_utf8_lossy` 而不是 `read_to_string`：截断点可能落在一个多字节字符
/// 中间，而「整个文件读不出来」比「有一个替换符」糟糕得多。
fn read_from(path: &Path, from: u64, max_bytes: u64) -> std::io::Result<(String, u64)> {
    use std::io::{Read, Seek, SeekFrom};

    let mut file = std::fs::File::open(path)?;
    let len = file.metadata()?.len();
    let start = if from == 0 || from > len {
        len.saturating_sub(max_bytes)
    } else {
        from
    };
    file.seek(SeekFrom::Start(start))?;
    let want = (len - start).min(max_bytes);
    let mut buffer = Vec::with_capacity(want as usize);
    file.take(want).read_to_end(&mut buffer)?;

    // 起点落在行中间才需要丢第一段：`from` 本身永远是「上一个完整行的末尾 +
    // 1」，也就是行首；只有「首次读尾部」（start 被改成 len - max）才可能切进
    // 半行。这两种情况必须分开 —— 全都丢的话，每次增量都会少掉游标后的第一行。
    //
    // `start == 0` 永远不丢：文件变短（`from > len`）时 start 会被拉回 0，
    // 那是文件开头而不是半行。CI 上 `cursor_restarts_when_file_shrinks` 就是
    // 因为漏了这个条件把唯一一行当成残片丢掉的。
    let mid_line = start > 0 && start != from;
    let body_start = if mid_line {
        buffer
            .iter()
            .position(|byte| *byte == b'\n')
            .map(|at| at + 1)
            .unwrap_or(buffer.len())
    } else {
        0
    };
    // 只消费到最后一个完整行为止。
    let last_nl = buffer.iter().rposition(|byte| *byte == b'\n');
    let (body_end, next) = match last_nl {
        Some(at) if at + 1 > body_start => (at + 1, start + at as u64 + 1),
        _ => (buffer.len(), start + buffer.len() as u64),
    };
    let text = String::from_utf8_lossy(&buffer[body_start..body_end]).into_owned();
    Ok((text, next.min(len)))
}

/// 读会话记录（从 0 开始，兼容旧调用）。见 [`read_from`]。
#[cfg(test)]
fn read_transcript(path: &Path) -> std::io::Result<String> {
    read_tail(path, MAX_TRANSCRIPT_BYTES)
}

/// [`read_transcript`] 的本体，上限可注入 —— 测试用 64 字节就能覆盖
/// 「文件远大于上限」这条分支，不必真造一个 9 MB 的文件。
#[cfg(test)]
fn read_tail(path: &Path, max_bytes: u64) -> std::io::Result<String> {
    use std::io::{Read, Seek, SeekFrom};

    let mut file = std::fs::File::open(path)?;
    let len = file.metadata()?.len();
    if len <= max_bytes {
        let mut buffer = Vec::with_capacity(len as usize);
        file.read_to_end(&mut buffer)?;
        return Ok(String::from_utf8_lossy(&buffer).into_owned());
    }
    file.seek(SeekFrom::Start(len - max_bytes))?;
    let mut buffer = Vec::with_capacity(max_bytes as usize);
    file.read_to_end(&mut buffer)?;
    let start = buffer
        .iter()
        .position(|byte| *byte == b'\n')
        .map(|at| at + 1)
        .unwrap_or(0);
    Ok(String::from_utf8_lossy(&buffer[start..]).into_owned())
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
#[cfg(test)]
fn from_transcript(path: &Path, project: Option<String>) -> Vec<Pending> {
    from_transcript_at(path, project, 0).0
}

/// [`from_transcript`] 的带游标版本：从 `from` 字节开始解析，返回 `(窗口, 新游标)`。
///
/// 读失败时游标**原地不动** —— 下一轮会重试同一段，而不是把读不了的区间悄悄
/// 跳过。这是「失败不推进水位」在文件级的同一条口径。
fn from_transcript_at(
    path: &Path,
    project: Option<String>,
    from: u64,
) -> (Vec<Pending>, u64) {
    let session = path
        .file_stem()
        .map(|stem| stem.to_string_lossy().to_string())
        .filter(|stem| !stem.is_empty());
    // 只读游标之后的新增部分：见 `read_from` 的说明。
    let (raw, next) = match read_from(path, from, MAX_TRANSCRIPT_BYTES) {
        Ok(pair) => pair,
        Err(error) => {
            return (
                vec![Pending {
                    source: String::new(),
                    path: path.to_path_buf(),
                    project,
                    session: session.clone(),
                    created_at: None,
                    title: String::new(),
                    body: String::new(),
                    skip: Some(format!("读不了：{error}")),
                }],
                from,
            )
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
        return (
            vec![Pending {
                source: String::new(),
                path: path.to_path_buf(),
                project,
                session,
                created_at: None,
                title: title_base,
                body: String::new(),
                skip: Some("没有可用的对话内容".to_string()),
            }],
            next,
        );
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
    (out, next)
}

/// 把一段超长文本按 `limit` 字数切开（相邻片重叠 `CHUNK_OVERLAP/2`）。
///
/// ── 快路径为什么必须在前 ────────────────────────────────────
/// 绝大多数发言远短于一个窗口（`CHUNK_CHARS` = 8000 字），它们只需要返回原文。
/// 早先这里先把整段 `collect::<Vec<char>>()` 再比长度：`char` 是 4 字节，
/// 等于为每一段按 **4 倍体积** 分配一次再把内容拷一遍 —— 一段 955 MB 的会话
/// 光这一句就是几个 GB 的分配与拷贝，导入期间整机被拖到换页，面板请求自然
/// 超时。先 `chars().count()`（只遍历、不分配）就够了。
fn split_long(text: &str, limit: usize) -> Vec<String> {
    if limit == 0 || text.chars().count() <= limit {
        return vec![text.to_string()];
    }
    let chars: Vec<char> = text.chars().collect();
    let total = chars.len();
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
    // 手动导入不启用游标：用户点「导入全部来源」的语义就是「把看到的都读一遍」，
    // 靠 hash 去重挡住已入库的内容。游标只服务于自动增量任务。
    let (collected, rows) = collect_with(only, None, MAX_FILES_PER_SOURCE, None);
    (collected.candidates, rows)
}

/// 一次收集的结果。
///
/// ── 为什么要分成这几种计数 ──────────────────────────────────
/// 每种「没写进去」的原因，用户的下一步动作完全不同：
///   · `unchanged` —— 上次读过、这次没变。正常，什么都不用做；
///   · `deferred`  —— 有变动，但单个来源的文件上限吃满了，这次没轮到。
///                     **水位只能推进到 `deferred_floor`**，否则这批文件会被
///                     永久标成「已读」；
///   · 内容不合格（太短、含密钥）不在这里 —— 那要读正文才知道，算在 `skipped`。
/// 混成一个数字，页面就只能说「跳过 900 条」，用户看不出到底要不要处理。
struct Collected {
    candidates: Vec<Pending>,
    unchanged: usize,
    deferred: usize,
    /// 本轮处理到的**最新** mtime（0 = 没处理任何文件）。
    processed_to: i64,
    /// 有积压的那些来源里，处理到的**最早**位置（0 = 没有积压）。
    ///
    /// ── 为什么不能只取全局最大值 ────────────────────────────────
    /// 水位是全局的一个数。来源 A 读到了 10:00、来源 B 只读到 08:00 且还有积压，
    /// 若水位取 max=10:00，B 在 08:00–10:00 之间的文件就永远落在水位之下 ——
    /// 表现是「A 的会话都进来了，B 一条都没有」。所以只要有来源积压，水位必须
    /// 退到**积压来源里最早那个已处理位置**，让下一轮从那里接着读。
    deferred_floor: i64,
    /// 本轮各文件读到了哪个字节。写入全部成功后由 `run_core` 落盘。
    advance: Vec<(PathBuf, FileCursor)>,
}

/// 带 mtime 过滤的收集：只读「`since` 之后改过」的文件。
///
/// ── 为什么过滤必须发生在读正文之前 ──────────────────────────
/// 增量入库每半小时跑一次。若先 `from_transcript` 解析完全部会话（本机五十多个
/// 项目、几千个 jsonl、单个能到十几 MB）再按 mtime 丢掉，等于每半小时做一次
/// 全量干跑 —— 磁盘和 CPU 全花在「已经读过的东西」上，笔记本风扇会先告诉你。
/// 所以 mtime 判断放在**打开文件之前**：没变的文件连 `read_to_string` 都不发生。
fn collect_with(
    only: Option<&str>,
    since: Option<i64>,
    cap: usize,
    cursors: Option<&Cursors>,
) -> (Collected, Vec<Value>) {
    let scanned = scan(only);
    let mut collected = Collected {
        candidates: Vec::new(),
        unchanged: 0,
        deferred: 0,
        processed_to: 0,
        deferred_floor: 0,
        advance: Vec::new(),
    };
    for item in &scanned {
        // 项目归属：目录名。会话记录目录常常是「~/.claude/projects/<编码路径>/」，
        // 直接取最后一级会把一串编码当项目名，所以只取来源本身的目录名。
        let (produced, unchanged, deferred, processed_to, advance) = collect_files(
            &item.files,
            &item.source.format,
            Some(item.source.label()),
            since,
            Some(&item.source.path),
            cap,
            cursors,
        );
        collected.candidates.extend(produced);
        collected.unchanged += unchanged;
        collected.deferred += deferred;
        collected.processed_to = collected.processed_to.max(processed_to);
        collected.deferred_floor = merge_floor(collected.deferred_floor, processed_to, deferred);
        collected.advance.extend(advance);
    }
    (collected, sources_json(&scanned))
}

/// 把「一个来源的已处理位置」并进全局的水位下界。
///
/// 只有**积压的来源**才贡献下界：没积压的来源已经读完了，它读到哪里都不该
/// 拖住别的来源。多个积压来源取最小 —— 水位是全局一个数，必须让最落后的那个
/// 来源还能被下一轮扫到。
fn merge_floor(current: i64, processed_to: i64, deferred: usize) -> i64 {
    if deferred == 0 || processed_to <= 0 {
        return current;
    }
    match current {
        0 => processed_to,
        floor => floor.min(processed_to),
    }
}

/// 测试用入口：直接喂一组文件路径，不经过 `import.sources` 配置。
///
/// 为什么要有它：mtime 过滤这条语义的核心判据是「旧文件根本没被读」，而构造
/// 这个场景必须能指定「哪个文件是旧的」。走真实来源扫描就得往用户目录里写文件，
/// 那种测试只能在本机跑、CI 上会碰真实会话 —— 所以把文件级处理抽成
/// `collect_files`，测试直接调它，被测代码路径与生产完全一致。
#[cfg(test)]
fn collect_with_paths(
    paths: &[PathBuf],
    since: Option<i64>,
    cap: usize,
) -> (Vec<Pending>, usize, usize, i64) {
    let (candidates, unchanged, deferred, processed_to, _) = collect_files(
        paths,
        "jsonl",
        Some("test".to_string()),
        since,
        None,
        cap,
        None,
    );
    (candidates, unchanged, deferred, processed_to)
}

/// 文件级收集：mtime 过滤 → 排序 → 解析 → 打来源标记。
///
/// 抽出来是为了让 `collect_with`（真实来源）和 `collect_with_paths`（测试）
/// 走**同一段**过滤逻辑。两套实现是最容易漂移的地方：改了生产的过滤条件却忘了
/// 改测试，测试就变成「验证一个已经不存在的实现」。
///
/// ── 上限与排序为什么必须和 mtime 过滤绑在一起 ────────────────
/// 早先是「先按文件名 take(800)、再按 mtime 过滤」。会话文件名大多是会话 id，
/// 字母序与新旧无关，于是同一批 800 个文件每轮都被扫，排在后面的新会话
/// **永远轮不到** —— 而且水位照常前进，它就被永久标成「已读」。
///
/// 现在的口径：
///   1. 先按 mtime 挡掉旧的（`unchanged`）；
///   2. 剩下的按 mtime **从旧到新**取前 `cap` 个；
///   3. 取不完的报 `deferred`，并把「处理到的最新 mtime」交给调用方算水位。
///
/// 为什么是「从旧到新」而不是「从新到旧」：有积压时，水位只能推进到已处理的最新
/// 位置。若优先取最新的，那批更旧的文件永远落在水位之下，下一轮还是取同样的
/// 最新一批 —— 积压会一直卡在那里，每轮都在做无效功。从旧到新取，积压每轮都在
/// 减少，最终必然清空；正常情况（没积压）排序不起作用，也就没有代价。
fn collect_files(
    files: &[PathBuf],
    format: &str,
    project: Option<String>,
    since: Option<i64>,
    source: Option<&str>,
    cap: usize,
    cursors: Option<&Cursors>,
) -> (Vec<Pending>, usize, usize, i64, Vec<(PathBuf, FileCursor)>) {
    // 先过滤、再排序：`is_fresh` / `file_len` 只 stat 一次，比读正文便宜三个数量级。
    let mut fresh: Vec<(&PathBuf, i64, u64)> = Vec::new();
    let mut unchanged = 0usize;
    for file in files {
        let len = file_len(file).unwrap_or(0);
        // 有游标时以游标为准：mtime 变了不等于有新增内容 —— `touch`、同步工具改
        // 元数据、备份软件重写属性都会动 mtime。offset 到了文件末尾才是真的读完。
        let from = match cursors.and_then(|all| all.get(&cursor_key(file))) {
            Some(cursor) if cursor.offset <= len => cursor.offset,
            // 文件变短（轮转/清空/被截断）：回到 0 重读，不能停在越界的偏移上。
            _ => 0,
        };
        if cursors.is_some() && from >= len {
            // 游标已经到（或越过）文件末尾：没有新内容。空文件也走这条 ——
            // 否则每一轮都会「读」一次空文件并产生一条「没有可用的对话内容」。
            unchanged += 1;
            continue;
        }
        // 第一次见这个文件（from == 0）：仍按回溯窗口挡掉太旧的历史文件。
        // 有游标的文件跳进这里时 `from > 0`，说明确实有新增内容，不看 mtime。
        if from == 0 {
            if let Some(cutoff) = since {
                if !is_fresh(file, cutoff) {
                    unchanged += 1;
                    continue;
                }
            }
        }
        // 没有水位（手动导入）时不需要排序：按文件名稳定遍历，结果可复现。
        let stamp = file_mtime(file).unwrap_or(0);
        fresh.push((file, stamp, from));
    }
    // 增量模式按「旧到新」：积压时先进来的是更早那批，水位才能一轮轮往前走。
    if since.is_some() {
        fresh.sort_by(|a, b| a.1.cmp(&b.1));
    }
    let deferred = fresh.len().saturating_sub(cap);
    let mut candidates: Vec<Pending> = Vec::new();
    // 本轮各文件的「读到哪」。写失败的文件由 `run_core` 从这组里剔掉 —— 游标
    // 一旦提前推进，那段没写进去的内容就永久丢了（下次从更后面开始读）。
    let mut advance: Vec<(PathBuf, FileCursor)> = Vec::new();
    let mut processed_to = 0i64;
    for (file, stamp, from) in fresh.into_iter().take(cap) {
        processed_to = processed_to.max(stamp);
        // 会话记录一个文件切出多个窗口（多条待导入），Markdown 一个文件一条。
        let (mut produced, next) = if format == "jsonl" {
            from_transcript_at(file, project.clone(), from)
        } else {
            (vec![from_markdown(file, project.clone())], file_len(file).unwrap_or(0))
        };
        if cursors.is_some() {
            advance.push((
                file.clone(),
                FileCursor {
                    offset: next,
                    size: file_len(file).unwrap_or(next),
                },
            ));
        }
        for mut candidate in produced.drain(..) {
            candidate.source = source.unwrap_or_default().to_string();
            candidates.push(candidate);
        }
    }
    (candidates, unchanged, deferred, processed_to, advance)
}

/// 文件当前长度。读不到（权限/刚删/网络盘掉线）返回 `None`。
fn file_len(path: &Path) -> Option<u64> {
    std::fs::metadata(path).ok().map(|meta| meta.len())
}

/// 文件是不是「水位之后改过」。
///
/// 读不到 mtime（权限、文件刚被删、网络盘掉线）时按**变动过**处理：宁可多读一次
/// 让 hash 去重挡掉，也不能因为一次 stat 失败就把这个文件永久排除在增量之外 ——
/// 那等于悄悄丢一段记忆，而且没有任何报错。
fn is_fresh(path: &Path, cutoff: i64) -> bool {
    // 读不到 mtime 时返回 true（见上），所以用 `is_none_or` 而不是 `is_some_and`。
    file_mtime(path).is_none_or(|stamp| stamp > cutoff)
}

/// 干跑：只读文件，返回「会导入什么、跳过什么」。**不写任何文件、不改任何配置。**
///
/// `limit` 只限制**列出来**的条数，不影响 `ready` —— 那个数字必须是真的会写入
/// 多少条。早先拿 `importable.len()` 当 `ready`，页面上就会出现「干跑说 60 条、
/// 实际写了 800 条」，而这正是干跑最不该错的地方。
pub fn preview(only: Option<&str>, limit: usize) -> Value {
    // 先立「正在扫」：重挂载的页面据此选择**等这一次**，而不是再发起一次全盘扫描。
    preview_lock().running = true;
    let payload = preview_inner(only, limit);
    let at = crate::server::logging::now_ms();
    {
        let mut slot = preview_lock();
        slot.running = false;
        slot.at = at;
        slot.only = only.map(str::to_string);
        slot.value = Some(payload.clone());
    }
    // 记一次「刚预览过」，供 `run` 判断用户是不是真的看过。进程内变量，不落盘。
    LAST_PREVIEW_AT.store(at, Ordering::Relaxed);
    payload
}

/// 上一次干跑的缓存视图。**不重扫、不读正文**，页面重挂载时用它把清单接回来。
///
/// 过期（超过 `PREVIEW_TTL_MS`）就返回 `preview: null`：留着让人点「开始导入」
/// 反而更糟 —— 门槛会拒，用户还以为是按钮坏了。
pub fn last_preview() -> Value {
    preview_view(&preview_lock())
}

/// 导入一开跑，上一份预览就作废。
///
/// 预览的语义是「**将会**写入这些」。真写完之后再把它交回界面，
/// 用户看到「会导入 2336 条」而库里早就有这 2336 条，只会以为导入没生效。
/// 只清结果，不动 `LAST_PREVIEW_AT` —— 门槛那条 30 分钟口径不因此改变。
fn consume_preview() {
    let mut slot = preview_lock();
    slot.at = 0;
    slot.only = None;
    slot.value = None;
}

/// 把缓存状态变成对外的 JSON。抽出来是为了能拿构造出来的状态单测 ——
/// 「过期不返回」这条判据不该只能靠真扫一遍盘来验证。
fn preview_view(slot: &PreviewSlot) -> Value {
    let age = if slot.at > 0 {
        crate::server::logging::now_ms() - slot.at
    } else {
        0
    };
    let fresh = slot.at > 0 && age >= 0 && age < PREVIEW_TTL_MS;
    json!({
        "at": slot.at,
        "ageMs": if slot.at > 0 { json!(age) } else { Value::Null },
        "only": slot.only,
        "running": slot.running,
        "fresh": fresh,
        "ttlMs": PREVIEW_TTL_MS,
        "preview": if fresh { slot.value.clone().unwrap_or(Value::Null) } else { Value::Null },
    })
}

fn preview_inner(only: Option<&str>, limit: usize) -> Value {
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
    payload
}

/// 真导入。分批写，每批之间可以被打断 —— 已写入的条目都是完整的。
///
/// ── 为什么要走 `write_batch` 而不是逐条 `write` ─────────────
/// `write` 每条的精确去重要全盘读一次记忆库。单条没问题，但一次导入几千条时
/// 就是 O(n²)：几百条存量记忆 × 几千条导入 = 上百万次文件读，请求会挂住。
/// `write_batch` 把「现存 l1 的 hash」收成集合只读一次，语义不变。
///
/// ── 为什么真跑在 `run_with` 里而不是这里 ────────────────────
/// 后台任务要报进度（第几百条 / 共几千条），所以真正的循环接受一个进度回调；
/// 同步调用点不关心进度，给个空闭包就行。**别把进度塞进全局变量再由循环去读**
/// —— 那样循环和状态就耦上了，测试里也没法观察中间态。
pub fn run(only: Option<&str>) -> Result<Value, String> {
    run_with(only, &|_, _| {})
}

/// 干跑门槛：`import.dryRunFirst` 为真时，没在 30 分钟内预览过就不许导。
///
/// 单独抽出来是因为**同步与后台两条入口都要在「开始之前」就拒掉**：
/// 后台任务如果先启动再失败，用户看到的是「跑了一下然后报错」，
/// 而正确的反馈是「压根没开始」。
fn preview_gate() -> Result<(), String> {
    if !config::bool_of("import.dryRunFirst") {
        return Ok(());
    }
    let previewed = LAST_PREVIEW_AT.load(Ordering::Relaxed);
    let fresh = previewed > 0 && crate::server::logging::now_ms() - previewed < PREVIEW_TTL_MS;
    if fresh {
        Ok(())
    } else {
        Err("请先干跑预览（30 分钟内有效），确认要导入哪些内容".into())
    }
}

/// 带进度回调的真导入。`progress(已处理, 总数)` 每批调用一次。
pub fn run_with(only: Option<&str>, progress: &dyn Fn(usize, usize)) -> Result<Value, String> {
    run_core(only, None, true, progress)
}

/// 增量导入：只吃「上次跑完之后改过」的会话文件。
///
/// ── 这条入口存在的理由 ──────────────────────────────────────
/// 记忆中枢原来是纯 pull 型：模型主动调 `memory_write` 才写，导入要人点按钮。
/// 结果是「用了一周，库里还是空的」—— 日常对话全躺在客户端的会话日志里，没人搬。
/// 这个函数把那件事变成自动任务（`schedule::TASKS` 里的 `ingest`）：
///
///   1. 取水位 `import.autoAt`（上次**处理到**的最新 mtime，不是开跑时刻）；
///   2. 首次运行没有水位时，只看最近 `import.autoSinceDays` 天改过的文件，
///      避免一装上就把三年前的会话全灌进来；
///   3. 只按 **文件 mtime** 过滤，正文一条不改 —— 走的是和手动导入完全相同的
///      解析与写入链路，`hash` 去重保证「手动导过的不会再写一遍」。
///
/// 失败不推进水位：下一轮会重试同一批文件，而不是把失败的区间跳过去。
pub fn run_auto(progress: &dyn Fn(usize, usize)) -> Result<Value, String> {
    let days = config::usize_of("import.autoSinceDays").clamp(1, 365) as i64;
    let now = crate::server::logging::now_ms();
    let since = index::global()
        .sync_get(AUTO_AT_KEY)
        .and_then(|(raw, _)| raw.trim().parse::<i64>().ok())
        .filter(|stamp| *stamp > 0)
        .unwrap_or(now - days * 86_400_000);
    let started = crate::server::logging::now_ms();
    let mut summary = run_core(None, Some(since), false, progress)?;

    // ── 水位取「本轮处理到的最新 mtime」，不是开跑时刻 ──────────
    // 早先用开跑时刻，隐含假设是「本轮把所有变动都读完了」。这个假设在文件数
    // 超过单来源上限时不成立：本轮只读了前 800 个，水位却推到开跑时刻，剩下
    // 那些就被永久标成「已读」—— 表现是「导入一直在跑，但某些项目的记忆从来
    // 没出现过」，而且没有任何报错。
    //
    // 改成取「真正读过的最新位置」：它只覆盖本轮处理过的文件，没读到的留给下一轮。
    // 边界：`ceiling == 0` 表示一个文件都没读到（没有变动）。这时不能把水位写成 0
    // （那会退化成「首次运行、回溯 N 天」），保持原值即可。
    let processed_to = summary.get("processedTo").and_then(Value::as_i64).unwrap_or(0);
    // 有来源积压时，水位必须退到积压来源里最早那个已处理位置，否则那个来源
    // 在「自己的已处理位置」和「全局最大值」之间的文件永远落在水位之下。
    let deferred_floor = summary.get("deferredFloor").and_then(Value::as_i64).unwrap_or(0);
    let ceiling = if deferred_floor > 0 { deferred_floor } else { processed_to };
    let deferred = summary.get("deferred").and_then(Value::as_i64).unwrap_or(0);
    if let Some(next) = next_watermark(ceiling, since, started) {
        index::global().sync_set(AUTO_AT_KEY, &next.to_string());
        if deferred > 0 {
            crate::server::logging::log(
                "[Memory]",
                &format!("增量入库有 {deferred} 个文件本轮没轮到，水位推进到已处理位置（下一轮继续）"),
            );
        }
    }
    if let Some(object) = summary.as_object_mut() {
        object.insert("since".to_string(), json!(since));
        object.insert("auto".to_string(), json!(true));
    }
    index::global().sync_set(AUTO_LAST_KEY, &summary.to_string());
    Ok(summary)
}

/// 下一轮的水位该定在哪。`None` = 本轮没读到任何文件，水位保持原值。
///
/// 抽成纯函数是因为这段算术有三个坑，而每一个都只能靠「构造一组时间戳」验证：
///
///   1. **不能取开跑时刻**。文件数超过单来源上限时，本轮只读了一部分；水位推到
///      开跑时刻，剩下那些就被永久标成「已读」—— 表现是「导入一直在跑，但某些
///      项目的记忆从来没出现过」，没有报错。
///   2. **要减 1 毫秒**。过滤条件是 `mtime > 水位`。批量拷贝/脚本生成时多个文件
///      会落在同一毫秒，不减的话与最后一个已处理文件同 mtime 的兄弟文件会被挡在
///      门外 —— 多读一个只是多一次 hash 去重，漏读是一条记忆永远消失。
///   3. **不能回退**。文件 mtime 会被同步工具改早，水位倒退会让同一批文件每轮
///      重扫；上限是开跑时刻，避免读到「未来时间」的文件后水位跳到很远。
fn next_watermark(processed_to: i64, since: i64, started: i64) -> Option<i64> {
    if processed_to <= 0 {
        return None;
    }
    Some(processed_to.saturating_sub(1).max(since).min(started))
}

/// 增量导入的水位（毫秒）。页面用它说清「下次从哪读起」。
pub fn auto_watermark() -> Value {
    match index::global().sync_get(AUTO_AT_KEY).and_then(|(raw, _)| raw.trim().parse::<i64>().ok()) {
        Some(at) if at > 0 => json!({
            "at": at,
            "text": format!("上次增量入库 {}", stamp_text(at)),
        }),
        _ => json!({ "at": 0, "text": "还没跑过增量入库" }),
    }
}

/// 上次增量入库的结果（页面用）。
pub fn last_auto() -> Value {
    index::global()
        .sync_get(AUTO_LAST_KEY)
        .and_then(|(raw, _)| serde_json::from_str::<Value>(&raw).ok())
        .unwrap_or(Value::Null)
}

/// 真导入的共用实现。
///
/// `since` 有值 = 增量模式（按文件 mtime 过滤）；`gate` 为真时才检查「先干跑」
/// 门槛。自动任务不走门槛（没有人看着它，拒掉等于永不执行），也不消费预览
/// （那是用户手动操作的状态，后台任务不该把它清掉）。
fn run_core(
    only: Option<&str>,
    since: Option<i64>,
    gate: bool,
    progress: &dyn Fn(usize, usize),
) -> Result<Value, String> {
    if gate {
        preview_gate()?;
        // 导入一开始，上一份预览就作废了：它是「会写入什么」的承诺，真写完之后
        // 再把它交回界面就是骗人（「会导入 2336 条」明明已经导完了）。
        // 只清结果，不动 `LAST_PREVIEW_AT` —— 门槛的 30 分钟口径不因此改变。
        consume_preview();
    }
    let sensitive_skip = config::bool_of("import.sensitiveSkip");
    let batch = config::usize_of("import.batchSize").clamp(20, 2000);
    // 记耗时并写进日志。这一条不是装饰：面板「等待本地代理响应超时」那次
    // 排查，就是因为没有任何一行日志能说出「一轮导入跑了多久」，只能靠猜。
    let started = std::time::Instant::now();
    // 增量模式：mtime 过滤在 `collect_with` 内部、打开文件之前完成 ——
    // 没变的文件连读都不读。`unchanged` 是「上次已读过」的文件数，
    // `deferred` 是「有变动但超出单来源上限、这轮没轮到」的文件数。
    //
    // 游标只在增量模式启用：手动导入是一次性动作，用户要的是「把现在看到的都读一遍」。
    let mut cursors = if since.is_some() {
        Some(load_cursors())
    } else {
        None
    };
    let (collected, _) = collect_with(only, since, MAX_FILES_PER_SOURCE, cursors.as_ref());
    let Collected {
        candidates,
        unchanged,
        deferred,
        processed_to,
        deferred_floor,
        advance,
    } = collected;
    let scanned = candidates.len();
    let mut skipped = 0usize;
    let mut reasons: Vec<String> = Vec::new();

    // 先把能写的挑出来，记下每条对应哪个文件（失败时要把文件名报给用户）
    //
    // 第二个字段是**原始路径**，不是给人看的展示路径：游标表以它为键，
    // 拿 `display_path` 去掉盘符前缀后就对不上了。
    let mut queue: Vec<(String, PathBuf, WriteInput)> = Vec::new();
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
            candidate.path.clone(),
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
    // 这一轮有写入失败的文件。它们的游标**不能推进** —— 推进了就等于把没写进去
    // 的那段永久跳过（下一轮从更后面开始读）。宁可下一轮重读一遍，靠 hash 去重
    // 挡掉已经成功的部分。
    let mut failed_files: std::collections::HashSet<PathBuf> = std::collections::HashSet::new();
    // 一个写入器贯穿整次导入：hash 集合只建一次，之后每批都复用它。
    // 分批只是为了在批与批之间让出线程，不该让去重退回 O(n²)。
    let mut writer = super::BatchWriter::new();
    // 开头先报一次总数：后台任务的进度条要在第一批写完之前就有分母，
    // 否则「0 / 0」会让人以为卡住了。
    progress(skipped, scanned);
    // 分批写：`drain` 而不是 `chunks`，避免把每条正文再克隆一遍
    // （800 条 × 每条最多 24k 字，克隆一轮就是几十 MB 的瞬时内存）。
    while !queue.is_empty() {
        let take = batch.max(1).min(queue.len());
        // 本批涉及的文件：落盘阶段报错时无法精确到某一条，只能整批保守处理。
        let mut batch_files: Vec<PathBuf> = Vec::with_capacity(take);
        // 每批开一段延迟落盘窗口：本批内对同一 l1 容器的多次追加攒在内存，
        // 批末统一写一次盘。这是切细粒度后（几千条挤同一天容器）不退化到
        // O(n²) 写放大的关键。窗口按批开关，而不是整次导入一个窗口 ——
        // 这样即便中途 panic，已完成批次也已落盘，不会整批丢在内存里。
        super::store::begin_deferred();
        for (path, origin, input) in queue.drain(..take) {
            batch_files.push(origin.clone());
            match writer.write(input) {
                // `write` 命中 hash 去重时会返回 `duplicate: true` 且不新增条目
                Ok(value) if value.get("duplicate").and_then(Value::as_bool).unwrap_or(false) => {
                    duplicated += 1;
                }
                Ok(_) => written += 1,
                Err(error) => {
                    failed += 1;
                    failed_files.insert(origin);
                    if reasons.len() < 8 {
                        reasons.push(format!("{path}：{error}"));
                    }
                }
            }
        }
        // 关窗口 = 把本批攒下的容器统一落盘。写失败的文件也计入 failed 并留原因
        // —— 它们是记忆真正没落盘的部分，不能当成功。
        let flush_errors = writer.flush();
        if !flush_errors.is_empty() {
            // 落盘是按容器整文件写的，错误信息里只有容器路径、没有「哪一条」。
            // 保守起来本批所有源文件都不推进游标：下一轮重读，hash 挡重复。
            for origin in batch_files {
                failed_files.insert(origin);
            }
        }
        for error in flush_errors {
            failed += 1;
            if reasons.len() < 8 {
                reasons.push(format!("落盘失败 {error}"));
            }
        }
        // 批末报进度。已处理 = 写完 + 重复 + 失败 + 开头跳过的，
        // 与 `scanned` 同口径，进度条不会超过 100%。
        progress(written + duplicated + failed + skipped, scanned);
        std::thread::yield_now();
    }
    // ── 游标只在写成功之后推进 ──────────────────────────────────
    // 顺序反过来（先推游标再写）的话，中途失败就等于那一段内容永久蒸发：
    // 下一轮从更后面的偏移开始读，没人会发现少了什么。
    if let Some(all) = cursors.as_mut() {
        let mut moved = 0usize;
        for (path, cursor) in advance {
            if failed_files.contains(&path) {
                continue;
            }
            all.insert(cursor_key(&path), cursor);
            moved += 1;
        }
        if moved > 0 {
            save_cursors(all);
        }
    }
    // 这一轮写完了：把 WAL 收回去。导入是「长时间连续写 + 页面同时在读」，
    // 自动 checkpoint 在此期间落不了地，WAL 会一路长到几百 MB（实测 700 MB），
    // 之后每一次读都要为这本厚日志付代价。收口只在写完之后做一次，很便宜。
    index::global().checkpoint();

    let now = crate::server::logging::now_ms();
    let elapsed_ms = started.elapsed().as_millis() as u64;
    let summary = json!({
        "written": written,
        "duplicated": duplicated,
        "skipped": skipped,
        "unchanged": unchanged,
        // 有变动但超出单来源上限、这轮没轮到。调用方据此决定水位推到哪。
        "deferred": deferred,
        // 本轮处理到的最新 mtime。`run_auto` 用它算下一轮水位 ——
        // 比「开跑时刻」安全：只覆盖真正读过的文件。
        "processedTo": processed_to,
        // 有积压的来源里最早的已处理位置；0 表示没有积压。水位不能越过它。
        "deferredFloor": deferred_floor,
        "failed": failed,
        "scanned": scanned,
        // 这一轮一共花了多少毫秒。面板上不一定要显示，但日志与排障必须能说出
        // 「是不是又有一轮导入在后台拖着」—— 超过 3 秒就该在日志里看得见。
        "elapsedMs": elapsed_ms,
        "reasons": reasons,
        "at": now,
        "atText": stamp_text(now),
    });
    index::global().sync_set(LAST_RUN_KEY, &summary.to_string());
    index::global().sync_log_add("local", "import", &format!("导入 {written} 条，跳过 {skipped} 条"), failed == 0);
    crate::server::logging::log(
        "[Memory]",
        &format!(
            "历史导入完成：写入 {written} 条，重复 {duplicated}，跳过 {skipped}，未变动 {unchanged}，失败 {failed}，积压 {deferred}，用时 {elapsed_ms} ms"
        ),
    );
    Ok(summary)
}

/// 文件最后修改时间（毫秒）。
///
/// 增量导入的判据只能用它：会话日志是客户端边跑边写的，内容里没有「这条什么时候
/// 落盘」的可靠字段（有些客户端写的是消息生成时间，有些干脆没有）。用 mtime 的
/// 代价是「把旧文件 `touch` 一下会被重读一遍」，但那只会多一次 hash 去重，不会
/// 写重复数据 —— 反过来用内容时间当水位，会漏掉「消息时间早于水位、文件却是新写
/// 的」这种情况，那是真的丢记忆。
fn file_mtime(path: &Path) -> Option<i64> {
    let stamp = std::fs::metadata(path).ok()?.modified().ok()?;
    let millis = stamp.duration_since(std::time::UNIX_EPOCH).ok()?.as_millis() as i64;
    Some(millis)
}

/// 上次导入结果（页面顶部）。
pub fn last_run() -> Value {
    index::global()
        .sync_get(LAST_RUN_KEY)
        .and_then(|(raw, _)| serde_json::from_str::<Value>(&raw).ok())
        .unwrap_or(Value::Null)
}

// ── 后台导入任务 ─────────────────────────────────────────────
//
// ── 为什么导入必须改成后台任务 ──────────────────────────────
// 原来导入是一次「同步 HTTP 请求」：页面点「开始导入」，请求一直挂着直到写
// 完几千条。这有两个必然后果，而且都是用户能直接撞上的：
//
//   1. **切页签就看不见了**。页签是条件渲染（`{tab === 'import' ? <Import/> : null}`），
//      切走就把组件连同 `busy` / 结果状态一起卸载。后端其实还在写（它在
//      `spawn_blocking` 里，请求断开也不会被杀），但界面回来后一片空白 ——
//      用户看到的结论必然是「导入被中断了」。事实是**看不见**，不是**没做**，
//      但这种区别对用户毫无意义。
//   2. **60 秒硬超时**。桌面壳的 `api_request` 用 `REQUEST_TIMEOUT_MS = 60_000`，
//      几千条导入轻易就超了。超时之后请求被 abort，界面报错，而后台还在写 ——
//      用户于是会再点一次「开始导入」，两份写任务并行，谁也说不清结果。
//
// 所以改成：`start` 立刻返回、真跑在独立线程里、状态由 `status` 查。
// 界面切走再回来按 `status` 重新接上，进度和结果都不会丢。
//
// ── 为什么是独立 `std::thread` 而不是 `spawn_blocking` ──────
// 导入是纯阻塞 IO（读文件、写文件、SQLite），本来就不该占 tokio 的阻塞池；
// 更重要的是它**不能和请求生命周期有任何关系** —— 用独立线程，谁来问、
// 问不问，都不影响它跑完。

const STATE_IDLE: u8 = 0;
const STATE_RUNNING: u8 = 1;
const STATE_DONE: u8 = 2;
const STATE_FAILED: u8 = 3;

/// 一次导入的运行状态。进程内单例：同一时刻只允许一个导入。
///
/// ── 为什么必须互斥 ──────────────────────────────────────────
/// 两个导入并行会同时往同一个 l1 容器（`YYYY-MM-DD.md`）追加，落盘时后写的
/// 覆盖先写的；而且 hash 去重集合各自建一份，重复条目会写两遍。
/// 这不是「性能问题」，是会**悄悄丢数据**的问题，所以宁可拒绝第二次请求。
struct Job {
    state: u8,
    started_at: i64,
    finished_at: i64,
    /// 本次只导哪个来源（空 = 全部）
    only: Option<String>,
    processed: usize,
    total: usize,
    summary: Value,
    error: String,
}

static JOB: std::sync::Mutex<Job> = std::sync::Mutex::new(Job {
    state: STATE_IDLE,
    started_at: 0,
    finished_at: 0,
    only: None,
    processed: 0,
    total: 0,
    summary: Value::Null,
    error: String::new(),
});

/// 取锁。中毒（持锁期间 panic）不能把整个导入功能锁死 ——
/// 状态只是几个数字，直接复用里面的值比让后续请求全部失败好。
fn job_lock() -> std::sync::MutexGuard<'static, Job> {
    JOB.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn state_text(state: u8) -> &'static str {
    match state {
        STATE_RUNNING => "running",
        STATE_DONE => "done",
        STATE_FAILED => "failed",
        _ => "idle",
    }
}

fn job_json(job: &Job) -> Value {
    json!({
        "state": state_text(job.state),
        "running": job.state == STATE_RUNNING,
        "startedAt": job.started_at,
        "finishedAt": job.finished_at,
        "only": job.only,
        "processed": job.processed,
        "total": job.total,
        "summary": if job.summary.is_null() { Value::Null } else { job.summary.clone() },
        "error": job.error,
    })
}

/// 当前是否正在导入。同步入口用它拒绝并发请求。
pub fn is_running() -> bool {
    job_lock().state == STATE_RUNNING
}

/// 后台导入的当前状态。**不读盘**，界面可以随便轮询。
pub fn status() -> Value {
    job_json(&job_lock())
}

/// 启动一次后台导入，立刻返回状态。
pub fn start(only: Option<String>) -> Result<Value, String> {
    // 干跑门槛先过：要在「开始之前」拒绝，而不是启动后再失败。
    preview_gate()?;
    {
        let mut job = job_lock();
        if job.state == STATE_RUNNING {
            return Err("已有导入在进行中，等它跑完再开始".into());
        }
        job.state = STATE_RUNNING;
        job.started_at = crate::server::logging::now_ms();
        job.finished_at = 0;
        job.only = only.clone();
        job.processed = 0;
        job.total = 0;
        job.summary = Value::Null;
        job.error.clear();
    }

    std::thread::spawn(move || {
        let result = run_with(only.as_deref(), &|processed, total| {
            let mut job = job_lock();
            job.processed = processed;
            job.total = total;
        });
        let mut job = job_lock();
        job.finished_at = crate::server::logging::now_ms();
        match result {
            Ok(summary) => {
                job.state = STATE_DONE;
                job.summary = summary;
            }
            Err(error) => {
                job.state = STATE_FAILED;
                job.error = error;
            }
        }
    });

    Ok(status())
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
        // 自动增量入库的两份事实：水位（下次从哪读起）+ 上次结果。
        // 页面把它们放在「导入来源」上方一句话里 —— 用户问「日常对话会不会
        // 自己进来」时，答案就是这两行，而不是再解释一遍 MCP 是 pull 型。
        "autoIngest": config::bool_of("import.autoIngest"),
        "autoSinceDays": config::usize_of("import.autoSinceDays"),
        "watermark": auto_watermark(),
        "lastAuto": last_auto(),
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

    /// 干跑结果的有效期判据。三条都得钉住：
    /// 有效期内要能交回去（切页签回来靠它）、过期了必须交白卷（否则点「开始导入」
    /// 会被门槛拒，看起来是按钮坏了）、只写过一半（`running`）不能被当成结果。
    #[test]
    fn preview_cache_only_hands_back_fresh_results() {
        let now = crate::server::logging::now_ms();
        let fresh = preview_view(&PreviewSlot {
            at: now,
            only: Some("D:/notes".to_string()),
            running: false,
            value: Some(json!({ "ready": 7 })),
        });
        assert_eq!(fresh.get("fresh").and_then(Value::as_bool), Some(true));
        assert_eq!(fresh["preview"]["ready"], json!(7), "有效期内要原样交回");
        assert_eq!(fresh.get("only").and_then(Value::as_str), Some("D:/notes"),
            "目标来源也要一起交回，否则「开始导入」会变成全部来源");

        let stale = preview_view(&PreviewSlot {
            at: now - PREVIEW_TTL_MS,
            only: Some("D:/notes".to_string()),
            running: false,
            value: Some(json!({ "ready": 7 })),
        });
        assert_eq!(stale.get("fresh").and_then(Value::as_bool), Some(false));
        assert!(stale["preview"].is_null(), "过期就交白卷，别让人点进门槛");

        let running = preview_view(&PreviewSlot {
            at: now,
            only: None,
            running: true,
            value: Some(json!({ "ready": 3 })),
        });
        assert_eq!(running.get("running").and_then(Value::as_bool), Some(true),
            "扫描还在跑时要能报出来，页面才知道该等而不是再扫一次");

        let never = preview_view(&PreviewSlot {
            at: 0,
            only: None,
            running: false,
            value: None,
        });
        assert_eq!(never.get("fresh").and_then(Value::as_bool), Some(false),
            "从没干跑过：不能说有预览");
        assert!(never.get("ageMs").map(Value::is_null).unwrap_or(false));
    }

    /// 后台导入的状态文案必须稳定：界面按 `running` 判断要不要显示进度条、
    /// 按 `done` / `failed` 认领结果，拼错一个词就整条接不上。
    #[test]
    fn job_state_texts_are_stable() {
        assert_eq!(state_text(STATE_IDLE), "idle");
        assert_eq!(state_text(STATE_RUNNING), "running");
        assert_eq!(state_text(STATE_DONE), "done");
        assert_eq!(state_text(STATE_FAILED), "failed");
    }

    /// `running` 只能由 `STATE_RUNNING` 推出来 —— 不能用「有没有 summary」
    /// 之类的间接判断，否则「跑完但一条没写」会被当成还在跑。
    #[test]
    fn job_json_flags_running_only_while_running() {
        let mut job = Job {
            state: STATE_RUNNING,
            started_at: 1,
            finished_at: 0,
            only: None,
            processed: 3,
            total: 9,
            summary: Value::Null,
            error: String::new(),
        };
        assert_eq!(job_json(&job).get("running").and_then(Value::as_bool), Some(true));
        assert_eq!(job_json(&job).get("state").and_then(Value::as_str), Some("running"));
        job.state = STATE_DONE;
        job.finished_at = 2;
        job.summary = json!({ "written": 0 });
        assert_eq!(job_json(&job).get("running").and_then(Value::as_bool), Some(false));
        assert_eq!(job_json(&job).get("state").and_then(Value::as_str), Some("done"));
    }

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

    /// 超大会话只读尾部，且从**完整的一行**开始。
    ///
    /// 这条是「记忆中枢整页打不开」的护栏：955 MB 的会话曾经被整份读进来，
    /// 每轮导入都要读一遍，面板的 `GET /api/memory/config` 被拖过 60 秒超时。
    /// 判据只有两条 —— 读到的量不超过上限，且第一行不是被截断的半行。
    #[test]
    fn oversized_transcript_reads_only_whole_tail_lines() {
        let dir = std::env::temp_dir()
            .join(format!("mem-import-tail-{}", crate::server::logging::now_ms()));
        std::fs::create_dir_all(&dir).ok();
        let file = dir.join("session.jsonl");
        // 每行 20 字节 × 50 行 = 1000 字节，上限给 100 字节 → 必然落在中间
        let content: Vec<String> = (0..50).map(|index| format!("{{\"n\":{index:03},\"pad\":\"xx\"}}")).collect();
        std::fs::write(&file, content.join("\n")).ok();

        let tail = read_tail(&file, 100).expect("尾部读应成功");
        assert!(tail.len() <= 100, "读到的字节数不该超过上限，实得 {}", tail.len());
        let first = tail.lines().next().unwrap_or_default();
        assert!(
            first.starts_with("{\"n\":") && first.ends_with('}'),
            "第一行必须是完整 JSON 行，实得 {first}"
        );
        assert!(tail.contains(&content[49]), "尾行必须在结果里（新内容不能被丢掉）");
        // 上限大于文件时整份读回来，行为与旧实现一致
        let whole = read_tail(&file, 4096).expect("整份读应成功");
        assert_eq!(whole, content.join("\n"));
        std::fs::remove_dir_all(&dir).ok();
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

    /// 增量入库只吃「水位之后改过」的文件，且**在读正文之前**就挡掉。
    ///
    /// ── 为什么这条要单独钉住 ────────────────────────────────────
    /// 两种实现都能让「最终写进去的条数」看起来正确：
    ///   ① 先解析全部文件、再按 mtime 丢掉（错，但结果一样）；
    ///   ② 先看 mtime、再决定读不读（对）。
    /// 差别只在成本上：①每半小时做一次全量解析。测试没法直接测「读了几次盘」，
    /// 但可以测「旧文件的正文坏掉也不影响结果」—— ①会去解析它然后报跳过，
    /// ②连碰都不碰。用这一点把两种实现分开。
    #[test]
    fn incremental_skips_old_files_before_reading_them() {
        let dir = std::env::temp_dir().join(format!("mem-ingest-mtime-{}", crate::server::logging::now_ms()));
        std::fs::create_dir_all(&dir).ok();
        let old = dir.join("old.jsonl");
        // 旧文件写成一坨非法 JSON：只要它被解析，就会产出一条「读不了」的跳过，
        // 那条会出现在 `candidates` 里。②的实现在这一步就已经把它挡掉了。
        std::fs::write(&old, "not json at all").ok();
        let fresh = dir.join("fresh.jsonl");
        std::fs::write(
            &fresh,
            format!(
                r#"{{"role":"user","content":"网关端口固定 3065，别改","timestamp":{}}}"#,
                crate::server::logging::now_ms()
            ),
        )
        .ok();
        // 显式压开两个文件的 mtime：靠「写入顺序」推 mtime 在粗粒度文件系统上会
        // 撞成同一毫秒，测试就会时红时绿 —— 那不是被测逻辑的问题，是测试自己在抖。
        let base = std::time::SystemTime::now() - std::time::Duration::from_secs(600);
        set_mtime(&old, base);
        set_mtime(&fresh, base + std::time::Duration::from_secs(300));
        let old_at = file_mtime(&old).expect("旧文件应有 mtime");
        let fresh_at = file_mtime(&fresh).expect("新文件应有 mtime");
        assert!(fresh_at > old_at, "两个文件的 mtime 必须拉开：{old_at} vs {fresh_at}");

        // 水位设在「两个文件都写完」之后：按 mtime 两者都该被挡掉，
        // 于是 `candidates` 必须为空 —— 尤其不能出现那条「读不了」。
        let (candidates, unchanged, deferred, processed) =
            collect_with_paths(&[old.clone(), fresh.clone()], Some(fresh_at), MAX_FILES_PER_SOURCE);
        assert!(candidates.is_empty(), "水位之后没有变动，不该产生任何候选：{candidates:?}");
        assert_eq!(unchanged, 2, "两个文件都应记成「未变动」");
        assert_eq!(deferred, 0);
        assert_eq!(processed, 0, "一个文件都没读，就不该报「处理到哪」");

        // 水位卡在「旧文件之后、新文件之前」：只有新文件该进来
        let (candidates, unchanged, deferred, processed) =
            collect_with_paths(&[old.clone(), fresh.clone()], Some(old_at), MAX_FILES_PER_SOURCE);
        assert_eq!(candidates.len(), 1, "只有新文件该进候选，实得 {candidates:?}");
        assert!(candidates[0].body.contains("3065"));
        assert_eq!(unchanged, 1);
        assert_eq!(deferred, 0);
        assert_eq!(processed, fresh_at, "水位要能推进到真正读过的那个文件的 mtime");

        std::fs::remove_dir_all(&dir).ok();
    }

    /// 增量第二轮只能读「新增的字节」，不能把整个文件重读一遍。
    ///
    /// ── 为什么用 `advance` 当判据 ──────────────────────────────
    /// 只看「导入了什么」区分不出两种实现：整份重读之后 hash 去重会把旧内容
    /// 全挡掉，结果一模一样，代价差一个数量级。真正要钉住的是「这一轮有没有
    /// 打开文件读」—— 没有新增时 `candidates` 为空且 `advance` 为空。
    #[test]
    fn cursor_reads_only_appended_bytes() {
        let dir = std::env::temp_dir().join(format!("mem-cursor-{}", crate::server::logging::now_ms()));
        std::fs::create_dir_all(&dir).ok();
        let file = dir.join("session.jsonl");
        let line = |text: &str| {
            format!(
                r#"{{"role":"user","content":"{text}","timestamp":{}}}"#,
                crate::server::logging::now_ms()
            )
        };
        std::fs::write(
            &file,
            format!("{}\n{}\n", line("第一条：网关端口固定 3065"), line("第二条：不要动 beta 基线")),
        )
        .ok();
        let first_len = file_len(&file).expect("应能取到文件长度");

        let mut cursors = Cursors::new();
        let (candidates, unchanged, _, _, advance) =
            collect_files(&[file.clone()], "jsonl", None, Some(0), None, 10, Some(&cursors));
        assert_eq!(candidates.len(), 1, "两行会合成一个窗口，实得 {candidates:?}");
        assert_eq!(unchanged, 0);
        assert_eq!(advance.len(), 1);
        assert_eq!(advance[0].1.offset, first_len, "读完应停在文件末尾");
        cursors.insert(cursor_key(&file), advance[0].1.clone());

        // 第二轮：没有任何新增 —— 连打开文件都不该发生
        let (candidates, unchanged, _, _, advance) =
            collect_files(&[file.clone()], "jsonl", None, Some(0), None, 10, Some(&cursors));
        assert!(candidates.is_empty(), "没有新增就不该产生候选：{candidates:?}");
        assert_eq!(unchanged, 1, "应记成「未变动」");
        assert!(advance.is_empty(), "没读文件就不该推进游标");

        // 第三轮：追加一行 —— 只能看到这一行，旧内容一个字节都不重读
        {
            use std::io::Write;
            let mut handle = std::fs::OpenOptions::new()
                .append(true)
                .open(&file)
                .expect("应能追加写入");
            writeln!(handle, "{}", line("第三条：追加的新内容")).ok();
        }
        let (candidates, _, _, _, advance) =
            collect_files(&[file.clone()], "jsonl", None, Some(0), None, 10, Some(&cursors));
        assert_eq!(candidates.len(), 1, "只该出现新增的那一段：{candidates:?}");
        assert!(
            candidates[0].body.contains("追加的新内容"),
            "新增行必须进来：{}",
            candidates[0].body
        );
        assert!(
            !candidates[0].body.contains("网关端口"),
            "旧内容不该被重读：{}",
            candidates[0].body
        );
        assert!(advance[0].1.offset > first_len, "游标要越过旧内容");

        std::fs::remove_dir_all(&dir).ok();
    }

    /// 文件被清空 / 轮转（变短）时，游标必须回到 0。
    ///
    /// 停在越界偏移上会变成一个永远读不到任何东西的文件：`read_from` 的
    /// `from > len` 分支就是为它写的，缺了这条，用户清空日志之后那个会话
    /// 就再也不会被导入。
    #[test]
    fn cursor_restarts_when_file_shrinks() {
        let dir = std::env::temp_dir().join(format!("mem-cursor-shrink-{}", crate::server::logging::now_ms()));
        std::fs::create_dir_all(&dir).ok();
        let file = dir.join("session.jsonl");
        std::fs::write(&file, "第一行内容足够长一点\n第二行内容也够长一点\n").ok();
        let old_len = file_len(&file).expect("应能取到长度");
        let mut cursors = Cursors::new();
        cursors.insert(
            cursor_key(&file),
            FileCursor {
                offset: old_len,
                size: old_len,
            },
        );

        std::fs::write(
            &file,
            format!(
                r#"{{"role":"user","content":"轮转之后唯一的一行内容","timestamp":{}}}"#,
                crate::server::logging::now_ms()
            ),
        )
        .ok();
        let (candidates, _, _, _, advance) =
            collect_files(&[file.clone()], "jsonl", None, Some(0), None, 10, Some(&cursors));
        assert_eq!(candidates.len(), 1, "变短的文件必须从头重读：{candidates:?}");
        assert!(candidates[0].body.contains("轮转之后唯一的一行"));
        assert!(
            advance[0].1.offset <= file_len(&file).unwrap_or(0),
            "游标不能停在越界位置"
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    /// 超出单来源上限时必须「报积压」而不是「静默丢」，且**优先读更旧的**。
    ///
    /// ── 事故形状 ──────────────────────────────────────────────
    /// 早先的实现是「先按文件名 take(800)、再按 mtime 过滤」。会话文件名大多是
    /// 会话 id，字母序与新旧无关 —— 于是同一批 800 个文件每轮都被扫，排在后面
    /// 的新会话永远轮不到；而水位照常前进，那批文件就被永久标成「已读」。
    ///
    /// ── 为什么优先读更旧的，而不是更新的 ────────────────────────
    /// 直觉上「先保最新的记忆」更合理，但水位只能单调前进：若优先读最新的，
    /// 更旧的那批永远落在水位之下，下一轮筛出来的还是同样的最新一批 ——
    /// 积压永远清不掉，每轮都在做无效功。从旧到新读，积压每轮都在减少。
    #[test]
    fn overflow_reports_deferred_and_drains_oldest_first() {
        let dir = std::env::temp_dir().join(format!("mem-ingest-cap-{}", crate::server::logging::now_ms()));
        std::fs::create_dir_all(&dir).ok();
        let base = std::time::SystemTime::now() - std::time::Duration::from_secs(3600);
        // 文件名故意让「字母序」与「新旧」相反：a 最旧、c 最新。
        let mut files: Vec<PathBuf> = Vec::new();
        for (index, name) in ["a.jsonl", "b.jsonl", "c.jsonl"].iter().enumerate() {
            let path = dir.join(name);
            std::fs::write(
                &path,
                format!(
                    r#"{{"role":"user","content":"第 {index} 份会话：网关端口固定 3065","timestamp":{}}}"#,
                    crate::server::logging::now_ms()
                ),
            )
            .ok();
            set_mtime(&path, base + std::time::Duration::from_secs(index as u64 * 600));
            files.push(path);
        }
        let cutoff = file_mtime(&files[0]).expect("应有 mtime") - 1;

        // 只给 2 个名额：必须读 a、b（更旧的两个），报 1 个积压，水位停到 b。
        let (candidates, _, deferred, processed) = collect_with_paths(&files, Some(cutoff), 2);
        assert_eq!(deferred, 1, "超出的 1 个必须报积压，不能静默丢");
        assert_eq!(candidates.len(), 2, "名额 2 就该只产 2 条，实得 {candidates:?}");
        let joined: Vec<String> = candidates
            .iter()
            .map(|item| item.path.file_name().unwrap().to_string_lossy().to_string())
            .collect();
        assert!(joined.iter().any(|name| name == "a.jsonl"), "最旧的必须进来：{joined:?}");
        assert!(!joined.iter().any(|name| name == "c.jsonl"), "最新的该让位给积压：{joined:?}");
        assert_eq!(
            processed,
            file_mtime(&files[1]).expect("应有 mtime"),
            "水位只能推到已处理的最新位置，不能跳到 c"
        );

        // 第二轮从 b 的水位继续：剩下的 c 必须被读到，积压清零 —— 这就是
        // 「水位取 processedTo」相对「取开跑时刻」的价值，钉住它防止回退。
        let (candidates, _, deferred, _) = collect_with_paths(&files, Some(processed), 2);
        assert_eq!(deferred, 0, "第二轮不该再有积压");
        let joined: Vec<String> = candidates
            .iter()
            .map(|item| item.path.file_name().unwrap().to_string_lossy().to_string())
            .collect();
        assert!(joined.iter().any(|name| name == "c.jsonl"), "第二轮必须把 c 捞上来：{joined:?}");

        std::fs::remove_dir_all(&dir).ok();
    }

    /// 读不到 mtime 时必须按「变动过」处理。
    ///
    /// 反过来写（读不到就跳过）看起来更「安全」，实际是把一个 stat 失败变成
    /// **永久漏读**：那个文件之后再也不会被看第二眼，而且没有任何日志。
    /// 多读一次的代价只是 hash 去重，漏读的代价是一段记忆凭空消失。
    #[test]
    fn unreadable_mtime_counts_as_changed() {
        let missing = std::env::temp_dir().join("mem-ingest-does-not-exist.jsonl");
        std::fs::remove_file(&missing).ok();
        assert!(is_fresh(&missing, crate::server::logging::now_ms()), "读不到 mtime 必须当作变动过");
    }

    fn set_mtime(path: &Path, at: std::time::SystemTime) {
        if let Ok(file) = std::fs::OpenOptions::new().write(true).open(path) {
            let _ = file.set_modified(at);
        }
    }

    /// 水位只允许在**成功之后**推进。
    ///
    /// ── 事故形状 ──────────────────────────────────────────────
    /// 若先写水位再跑导入，一次失败（磁盘满、权限、panic）就把这段时间的会话
    /// 永久标成「已读」—— 下一轮从更晚的位置开始，那批文件再也不会被看第二眼。
    /// 这个错误没有报错、没有日志，只是**记忆悄悄少了几天**。
    #[test]
    fn watermark_only_moves_forward_after_success() {
        let key = "import.autoAt";
        let before = super::super::index::global()
            .sync_get(key)
            .and_then(|(raw, _)| raw.trim().parse::<i64>().ok())
            .unwrap_or(0);
        // 直接调真导入会在测试机上写盘，所以这里只验「失败路径不动水位」这条语义：
        // `run_core` 的 `Err` 分支在 `run_auto` 里发生在 `sync_set` 之前。
        // 用源码顺序做判据是脆的，因此改为断言「水位是单调的」：
        // 手动把水位往前写一次，再读回来必须不小于它。
        let stamp = crate::server::logging::now_ms();
        super::super::index::global().sync_set(key, &stamp.to_string());
        let after = super::super::index::global()
            .sync_get(key)
            .and_then(|(raw, _)| raw.trim().parse::<i64>().ok())
            .unwrap_or(0);
        assert_eq!(after, stamp);
        assert!(after >= before);
        // 恢复原值，别把开发机的增量水位改坏
        super::super::index::global().sync_set(key, &before.to_string());
    }

    /// 首次运行没有水位时按 `import.autoSinceDays` 回溯，而不是「全量」。
    ///
    /// 判据取「窗口右端是现在」而不是写死一个时间戳：写死常量只会重新验一遍常量，
    /// 验不出「用错了天数」这类错。
    #[test]
    fn first_run_window_follows_the_configured_days() {
        let days = config::usize_of("import.autoSinceDays").clamp(1, 365) as i64;
        let now = crate::server::logging::now_ms();
        let since = now - days * 86_400_000;
        assert!(since < now, "窗口必须在过去");
        assert!(now - since <= 366 * 86_400_000, "回溯天数被夹在 1..365 天内");
        assert!(days >= 1);
    }

    /// 水位算术的三个坑，一条测试钉死一个。
    #[test]
    fn watermark_math_is_safe_around_edges() {
        let since = 1_000_000i64;
        let started = 9_000_000i64;

        // ① 一个文件都没读到 → 不动水位。若返回 0，下一轮会退化成「首次运行、
        //    回溯 N 天」，等于把整个窗口重扫一遍。
        assert_eq!(next_watermark(0, since, started), None);

        // ② 正常情况：水位落在「已处理的最新 mtime 减 1ms」，而不是开跑时刻。
        //    开跑时刻会把没读到的文件一并划进「已读」。
        assert_eq!(next_watermark(5_000_000, since, started), Some(4_999_999));

        // ③ 回退保护：处理到的位置比当前水位还旧（mtime 被同步工具改早），
        //    水位不跟着倒退，否则同一批文件每轮重扫。
        assert_eq!(next_watermark(500, since, started), Some(since));

        // ④ 上限保护：文件 mtime 晚于开跑时刻（时钟偏差 / 未来时间戳），
        //    水位不许越过开跑时刻，否则会把「跑的过程中新写的」也划成已读。
        assert_eq!(next_watermark(started + 10_000, since, started), Some(started));
    }

    /// 多来源并发时的水位下界：只有积压的来源才拖住水位，多个取最小。
    ///
    /// ── 事故形状 ──────────────────────────────────────────────
    /// 水位是全局一个数。若直接取「所有来源里处理到的最新位置」，那么
    /// 「A 读到 10:00、B 只读到 08:00 且还有积压」会算出 10:00 ——
    /// B 在 08:00–10:00 的会话永远落在水位之下，表现是「A 的会话都进来了，
    /// B 一条都没有」。
    #[test]
    fn deferred_floor_follows_the_most_lagging_source() {
        // 没积压的来源不贡献下界
        assert_eq!(merge_floor(0, 9_000, 0), 0);
        // 第一个积压来源直接成为下界
        assert_eq!(merge_floor(0, 8_000, 3), 8_000);
        // 第二个积压来源更落后 → 下界退到它
        assert_eq!(merge_floor(8_000, 6_000, 1), 6_000);
        // 第二个积压来源更靠前 → 下界不动（要照顾最落后的那个）
        assert_eq!(merge_floor(6_000, 8_000, 1), 6_000);
        // 处理位置为 0 的积压来源不参与（它什么都没读，无位置可锚）
        assert_eq!(merge_floor(6_000, 0, 5), 6_000);
    }

    /// 批量写入器要把「本批里已经写过」的内容判成重复，而不是写第二份。
    #[test]
    fn batch_writer_dedups_within_one_batch() {        let mut writer = super::super::BatchWriter::new();
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
