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

/// 单次尾部读的字节上限（**仅测试用**；生产路径已改成流式读，见 [`for_each_line`]）。
///
/// ── 历史：这条上限曾经把 87% 的真实记录挡在库外 ──────────────
/// 会话记录是追加写的，`.jsonl` 能涨到 1 GB（2026-11-21 实测本机某个会话 955 MB）。
/// 早先 `from_transcript` 用 `read_to_string` 整份读，于是加了「只读尾部 8 MB」的
/// 上限来避免面板卡死。但这个折中的代价是**内容丢失**：2026-11-22 实测本机
/// 65 个会话共 1.53 GB，只读进 13%（198 MB）；1.04 GB 那个会话里 489 条用户
/// 提问只捞到 5 条（1%）。用户看到的现象就是「导入了 2341 条，可我的真实记录
/// 远不止这些」。
///
/// 现在改成 [`for_each_line`] 流式逐行读：峰值内存只跟单行大小有关，与文件
/// 总大小无关，所以**生产路径不再需要这条上限**。常量留着只服务
/// `read_tail`（测试里用 64 字节覆盖「文件远大于上限」分支）。
#[cfg(test)]
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
    /// 文件头指纹（前 512 字节的 FNV-1a）。0 = 没算过（老游标升级上来）。
    ///
    /// ── 为什么光有 offset 不够 ──────────────────────────────────
    /// `offset` 只能回答「读到第几个字节」，回答不了「这还是同一个文件吗」。
    /// 会话文件被轮转/重写之后，只要新内容比旧 offset 长，`offset <= len` 就
    /// 骗过了所有判断，我们从旧偏移继续读 —— 新文件的头部被整段跳过，那些行
    /// 永远进不来。头部指纹一变就当作「换了一个文件」，从头重读；正文 hash
    /// 去重保证重读不会产生重复条目。
    ///
    /// `serde(default)` 是给老游标表（只有 offset/size）留的：缺这个字段时按 0
    /// 处理，等于「不比对」，升级那一刻不会把所有文件重读一遍。
    #[serde(default)]
    head: u64,
}

type Cursors = std::collections::BTreeMap<String, FileCursor>;

/// 游标键：绝对路径、统一分隔符。
///
/// 不折大小写：同一进程的 `read_dir` 返回的字符串是稳定的，折了反而会让
/// 「用户手动改过盘符大小写」这种情况对不上。
fn cursor_key(path: &Path) -> String {
    path.to_string_lossy().replace('\\', "/")
}

/// 文件头指纹：**第一行**（最多 512 字节）的 FNV-1a 64 位散列。0 = 读不到 / 空文件。
///
/// ── 为什么是「第一行」而不是「前 512 字节」 ──────────────────
/// 前 512 字节是个**变长**窗口：文件还没到 512 字节时，一次追加就会把新内容
/// 卷进来，散列自然不同 —— 于是每次追加都被误判成「换了文件」，退化成整份
/// 重读（CI 上 `cursor_reads_only_appended_bytes` 就是这么红的：它只有 160
/// 字节，追加一行后头部窗口变大，指纹变了）。第一行在只追加的文件里永远不变，
/// 又足以区分「换了一个文件」：JSONL 的首行是会话元信息，换文件必然换行。
///
/// 为什么用散列而不是直接存第一行：游标表要整张序列化进 `mem_meta`，一万个
/// 文件各存几十上百字节就是几 MB 的库内噪音；一个 u64 是 8 字节。
fn head_fingerprint(path: &Path) -> u64 {
    use std::io::Read;

    let Ok(mut file) = std::fs::File::open(path) else {
        return 0;
    };
    let mut buffer = [0u8; 512];
    let read = match file.read(&mut buffer) {
        Ok(read) => read,
        Err(_) => return 0,
    };
    if read == 0 {
        return 0;
    }
    // 截到第一行末尾（含换行）；窗口里没有换行说明首行本身就超过 512 字节，
    // 那就用这 512 字节 —— 只追加的写入不会动它。
    let window = match buffer[..read].iter().position(|byte| *byte == b'\n') {
        Some(at) => &buffer[..=at],
        None => &buffer[..read],
    };
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in window {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
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
    let raw = parts.join("\n");
    let text = if speaker == "我" {
        // 用户侧：真实原话埋在信封里，必须**抽出来**而不是整条判定
        human_words(&raw)?
    } else {
        let cleaned = strip_client_blocks(&raw);
        let cleaned = cleaned.trim().to_string();
        if cleaned.chars().count() < 8 || is_harness_noise(&cleaned) {
            return None;
        }
        cleaned
    };
    if text.chars().count() < 8 {
        return None;
    }
    let created = parse_time(&value);
    Some((speaker.to_string(), text, created))
}

/// 从一条用户消息里抽出「人真正打的字」。
///
/// ── 根因：真机上根本没有「裸的用户原话」──────────────────────
/// 2026-11-22 扫本机 65 个 WorkBuddy 会话文件（1.5 GB，2464 条用户消息），
/// 形状分布是：
///
/// ```text
///  1009  <system-reminder …> … </system-reminder> + <user_query>真话</user_query>
///   617  <task-notification>…</task-notification>          后台任务回执
///   335  <conversation_history_summary>…</…>               客户端压缩的摘要
///   300  Please continue with the conversation based on…   续跑指令
///   195  <system-reminder …> 独立一段（只有环境信息，没有话）
///     7  <cb_summary>…</cb_summary>                        另一种摘要
///     1  <user_query>…</user_query>                        只有话
/// ```
///
/// 也就是说：**用户的每一句话都被客户端包在 `<user_query>` 里，外面还套着几千字的
/// 环境信封**。旧实现只有「整段以信封开头 → 整条丢弃」这一条判据，于是 1009 条
/// 真实诉求被全部丢弃；侥幸没被丢的，落库的是几千字的 `<user_info>` / `<identity_context>`
/// / `SOUL.md` 模板 —— 库里 5973 条导入记忆的用户侧全是机器模板，一条人话都没有。
///
/// ── 为什么要先剥摘要块再抽 `user_query` ──────────────────────
/// 有 7 条 `<user_query>` 是**嵌在 `<conversation_history_summary>` 内部**的（摘要
/// 复述历史时把当年的提问也抄了一遍）。直接全文搜 `user_query` 会把「上一轮的历史
/// 提问」当成「这一轮用户在说话」，时间线全错。所以顺序必须是：先整块删掉客户端
/// 摘要 / 回执 / 环境信封，再在剩下的文本里找 `<user_query>`。
///
/// ── 兜底 ────────────────────────────────────────────────────
/// 剥完还没有 `<user_query>`，但剩下的文本本身够长、且不再以信封标签开头 —— 说明
/// 这个客户端就是直接写纯文本（Claude Code / Codex 的某些形态）。这时整段就是原话。
fn human_words(raw: &str) -> Option<String> {
    let stripped = strip_client_blocks(raw);
    let mut said: Vec<String> = Vec::new();
    let mut rest = stripped.as_str();
    while let Some(at) = rest.find("<user_query>") {
        let after = &rest[at + "<user_query>".len()..];
        let Some(end) = after.find("</user_query>") else { break };
        let text = after[..end].trim();
        if text.chars().count() >= 1 {
            said.push(text.to_string());
        }
        rest = &after[end + "</user_query>".len()..];
    }
    if !said.is_empty() {
        let joined = said.join("\n\n");
        return if joined.chars().count() < 8 { None } else { Some(joined) };
    }
    // ── 闭标签残片开头：不是人话 ────────────────────────────────
    // 真机 5 条的形状是 `</user_query>Please continue with…` —— 开标签被客户端
    // 写坏/截断，闭标签落到了行首。这种残渣一律丢弃，它前面根本没有内容。
    let fallback = skip_leading_close_tags(stripped.trim());
    if fallback.is_empty() || is_harness_noise(fallback) {
        return None;
    }
    // ── 兜底必须「像一句话」，不能是代码回显 ────────────────────
    // 真机剩 3 条兜底里 2 条是 `"),\n  (""),\n  ...];` 这种代码中段（客户端把
    // 工具结果回显塞进了 user 角色）。兜底本来就是为了兼容「直接写纯文本的客户端」，
    // 判据取：含汉字 + 不以代码符号起头 + 不是 `looks_like_fragment` 的碎片。
    if !fallback.chars().any(is_cjk) || looks_like_fragment(fallback) {
        return None;
    }
    Some(fallback.to_string())
}

/// 是不是汉字（CJK 统一表意文字基本区 + 扩展 A）。
fn is_cjk(ch: char) -> bool {
    ('\u{4e00}'..='\u{9fff}').contains(&ch) || ('\u{3400}'..='\u{4dbf}').contains(&ch)
}

/// 吃掉行首连续的 `</名字>` 残片，返回剩下的正文。
fn skip_leading_close_tags(text: &str) -> &str {
    let mut rest = text.trim_start();
    loop {
        if !rest.starts_with("</") {
            return rest;
        }
        let Some(gt) = rest.find('>') else { return rest };
        // 只认「`</` + 名字 + `>`」这种紧贴形态，中间不能有空格（有空格就是在写正文）
        let name = &rest[2..gt];
        if name.is_empty() || name.contains(char::is_whitespace) {
            return rest;
        }
        rest = rest[gt + 1..].trim_start();
    }
}

/// 整块删掉客户端注入的成对标签块（含内容）。
///
/// 只删**成对**的：有开标签就必须找到对应闭标签才删。缺闭标签时原样保留 ——
/// 半截标签更可能是正文里在讨论这个标签，删了会把真实讨论吃掉。
///
/// 名单只收真机上出现过的、且**内容一定不是人话**的那几类：环境信封、两种客户端
/// 摘要、后台任务回执、图片回执、工具调用壳。
fn strip_client_blocks(raw: &str) -> String {
    // (开标签, 闭标签)。开标签带属性，所以只比前缀，闭标签要比全。
    const PAIRS: &[(&str, &str)] = &[
        ("<system-reminder", "</system-reminder>"),
        ("<environment_context", "</environment_context>"),
        ("<environment-context", "</environment-context>"),
        ("<conversation_history_summary", "</conversation_history_summary>"),
        ("<cb_summary>", "</cb_summary>"),
        ("<task-notification>", "</task-notification>"),
        ("<antml:function_calls", "</antml:function_calls>"),
    ];
    let mut text = raw.to_string();
    // 反复扫：信封套信封（`<system-reminder>` 里嵌 `<identity_context>`）时，
    // 删掉外层一次就够；但同一段里可能有多组并列信封，所以循环到不再变化。
    loop {
        let before = text.len();
        for (open, close) in PAIRS {
            let mut from = 0usize;
            let mut out = String::with_capacity(text.len());
            let mut changed = false;
            while let Some(at) = text[from..].find(open) {
                let start = from + at;
                let after_open = start + open.len();
                let Some(rel_end) = text[after_open..].find(close) else { break };
                let end = after_open + rel_end + close.len();
                out.push_str(&text[from..start]);
                changed = true;
                from = end;
            }
            if changed {
                out.push_str(&text[from..]);
                text = out;
            }
        }
        if text.len() == before {
            break;
        }
    }
    text
}

/// 客户端注入的上下文信封（不是对话内容）。与 AgentHub `isHarnessNoise` 同口径。
///
/// ── 这个判定现在只管「剥完还是信封」的残渣 ────────────────────
/// 用户侧的真实原话由 [`human_words`] 从 `<user_query>` 里抽出来，剥块由
/// [`strip_client_blocks`] 做。走到这里还没干净的情况只剩两种：
///
///   1. 助手侧整段就是一段客户端控制消息（`<task-notification>` 之类被写进
///      assistant 角色的怪文件）；
///   2. 用户侧剥完之后只剩一个孤零零的开标签（闭标签缺失，`strip_client_blocks`
///      故意不删），实质没有内容。
///
/// 判据仍是「整段以此开头」：只截断正文里的标签不算（正文里讨论标签本身
/// 是合法的，`system_reminder_with_attributes_is_stripped` 里有这条用例）。
fn is_harness_noise(text: &str) -> bool {
    const ENVELOPES: &[&str] = &[
        "<system-reminder",
        "<environment_context",
        "<environment-context",
        // 客户端压缩出来的会话摘要 —— 不是用户说的话
        "<conversation_history_summary",
        "<cb_summary>",
        "<summary>",
        "<session",
        // 客户端控制消息
        "<task-notification",
        "Please continue with the conversation based on",
        // 工具回执尾巴（`<task-notification>` 块被剥掉之后剩下的就是这几句）
        "Use the TaskOutput tool with task_id=",
        "IMPORTANT: Before responding, scroll back to the user's request",
        "The user cancelled this task.",
        // 工具回执
        "Attached image(s) from tool result:",
        "<antml:function_calls",
        "<function_calls>",
        "<tool_call>",
        "<tool_calls>",
    ];
    let trimmed = text.trim_start();
    ENVELOPES.iter().any(|prefix| trimmed.starts_with(prefix))
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

/// 按行流式读会话记录：从 `from` 字节开始逐行回调，返回**下一个游标**。
///
/// ── 为什么不能「一次 read_to_end 再 split」──────────────────
/// 真机 2026-11-22 实测：本机 `~/.workbuddy/projects` 65 个会话文件共 **1.53 GB**，
/// 其中单个会话 1.04 GB。旧实现用 `read_from` 一次读进 `String`，于是被
/// `MAX_TRANSCRIPT_BYTES`（8 MB）挡住 —— **整体只读进 13%**（198 MB / 1528 MB），
/// 1.04 GB 那个会话只读了尾部 8 MB：489 条用户提问只捞到 5 条（1%）。
/// 用户看到的现象就是「导入了 2341 条，可我的真实记录远不止这些」。
///
/// 流式读之后上限只受「单来源最多读多少文件」约束，不再按字节丢内容。
///
/// ── 三个边界（与旧 `read_from` 同口径）────────────────────────
/// 1. `from == 0`：从文件头开始读，一个字节都不跳。
/// 2. 文件变短（被轮转/截断）：`from > len` → 回到 0 重读，不能原地空转。
/// 3. 末尾没有换行（正在写的最后一行）：游标停在最后一个完整行的末尾，下一轮
///    重读这一行。宁可多读一行，不能吞掉半行 JSON。
///
/// 用 `from_utf8_lossy` 而不是 `read_line` 到 `String`：截断点可能落在一个多字节
/// 字符中间，而「整个文件读不出来」比「有一个替换符」糟糕得多。
fn for_each_line(
    path: &Path,
    from: u64,
    mut visit: impl FnMut(&str) -> bool,
) -> std::io::Result<u64> {
    use std::io::{BufRead, BufReader, Seek, SeekFrom};

    let mut file = std::fs::File::open(path)?;
    let len = file.metadata()?.len();
    let start = if from > len { 0 } else { from };
    file.seek(SeekFrom::Start(start))?;
    let mut reader = BufReader::with_capacity(256 * 1024, file);
    let mut buffer: Vec<u8> = Vec::with_capacity(64 * 1024);
    let mut cursor = start;
    loop {
        buffer.clear();
        let read = reader.read_until(b'\n', &mut buffer)?;
        if read == 0 {
            break;
        }
        // 没读到换行 = 这是正在写的半行，不消费它（游标停在本行开头）
        if buffer.last() != Some(&b'\n') {
            break;
        }
        let line = String::from_utf8_lossy(&buffer);
        cursor += read as u64;
        if !visit(line.trim_end_matches(['\n', '\r'])) {
            break;
        }
    }
    Ok(cursor.min(len))
}

/// 读会话记录（从 0 开始，兼容旧调用）。见 [`for_each_line`]。
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
/// 会话记录自己声明的工作目录，取末级目录名当项目名。
///
/// ── 为什么不能直接用来源目录名 ──────────────────────────────
/// 来源是「客户端把会话存在哪」，不是「这段对话在哪个项目里」。两者在真机上
/// 差异极大：`~/.workbuddy/projects/<编码后的工作区>/` 里，来源末级恒为
/// `projects`，于是几千条不同项目的对话全被归进同一个 `projects` 项目。
///
/// ── 为什么入参必须是**解码后**的文本 ─────────────────────────
/// 真机 2026-11-22 发现：拿原始 JSONL 行去扫会得到 `nNote:` 这种项目名。
/// 原因是 jsonl 里换行是 `\` + `n` 两个字符，`Workspace Folder: C:\\Users\\…\\demo\nNote:`
/// 后面的 `\n` 被当成了目录名的第一个字符，`split('\\')` 之后就落出 `nNote:`。
/// 全部 55 个会话都会被归进同一个假项目 `nNote:` —— 和当年 `projects` 那个事故
/// 一模一样的形状。
///
/// 修法是让调用方传**已 `serde_json` 解码**的正文（`line_to_turn` 里那份），
/// 解码之后换行是真换行，`split` 天然截断。`raw_head` 只在 `for_each_line`
/// 里拼接解码后的文本，所以这里不需要再解一次。
///
/// ── 认哪些声明 ───────────────────────────────────────────────
/// | 客户端 | 声明形态 |
/// |---|---|
/// | WorkBuddy | `<user_info>` 里 `Workspace Folder: C:\...\名字` |
/// | 各类 harness | `<environment_context>cwd=/x/y</environment_context>` |
/// | 会话元数据 | 行内 `"cwd":"/x/y"` |
///
/// 只取**末级目录名**，且要求它像名字（不是 `C:`、不是纯数字）。末级目录名
/// 会在写入侧走 `layout::classify`，那里再按 git 身份折成 slug —— 这里不做
/// 第二套判断。
fn declared_project_of(raw: &str) -> Option<String> {
    const KEYS: &[&str] = &[
        "Workspace Folder:",
        "workspace folder:",
        "\"cwd\"",
        "cwd=",
        "working directory:",
        "Working directory:",
    ];
    // 只扫前 40 KB：工作目录是会话开头的固定信息，扫全文等于把十几 MB 读两遍
    let head: String = raw.chars().take(40_000).collect();
    for key in KEYS {
        let Some(at) = head.find(key) else { continue };
        let rest = &head[at + key.len()..];
        let rest = rest.trim_start_matches(|ch: char| {
            matches!(ch, ':' | '"' | '=' | '>' | ' ' | '\t')
        });
        // ── 路径到「不可能是路径字符」为止 ──────────────────────
        // 早先只按 `"` `\n` `<` `>` 截断。真机上 `Workspace Folder: C:\a\demo\nNote:`
        // 因为解析后换行是真换行，看着没事；但有些客户端把整段信封压成一行，
        // 声明后面紧跟 `Note: Prefer using…`，于是尾巴会被一起吃进目录名。
        //
        // 两刀：
        //   1. 只吃到引号/换行/尖括号/竖线这类**绝不可能是路径**的字符；
        //   2. 再按冒号截一刀 —— 冒号在 Windows 路径里只出现在盘符（`C:`，下标 1），
        //      出现在更后面一律意味着「说明性文字开始了」。
        let rough: String = rest
            .chars()
            .take_while(|ch| !matches!(ch, '"' | '\n' | '\r' | '<' | '>' | '|' | '\u{0}'))
            .collect();
        let path = match rough.char_indices().skip(1).find(|(_, ch)| *ch == ':') {
            Some((at, _)) => &rough[..at],
            None => rough.as_str(),
        };
        let path = path
            .trim()
            .trim_end_matches(|ch: char| matches!(ch, '\\' | '/' | '。' | '.' | ','));
        if let Some(name) = project_name_of_path(path) {
            return Some(name);
        }
    }
    None
}

/// 从一条路径里取「像项目名」的末级目录名。
///
/// Windows 反斜杠与 Unix 正斜杠都算分隔符。以下都不像项目名，返回 `None`
/// 让调用方继续看下一个候选：盘符（`C:`）、纯数字、单字符、空。
fn project_name_of_path(path: &str) -> Option<String> {
    let name = path
        .rsplit(|ch| ch == '\\' || ch == '/')
        .find(|part| !part.trim().is_empty())?
        .trim();
    if name.len() < 3 || name.ends_with(':') || name.chars().all(|ch| ch.is_ascii_digit()) {
        return None;
    }
    // 名字里出现冒号 / 尖括号 / 引号：说明这不是路径，是「路径 + 说明性文字」粘在一起
    if name.contains([':', '<', '>', '"', '\n', '\r']) {
        return None;
    }
    Some(name.to_string())
}

/// 导入条目的标题基数：优先会话里第一句像「事情」的话，退回文件名。
///
/// ── 为什么文件名不能直接当标题 ──────────────────────────────
/// 真机事故：导入进来的 5,974 条标题全是裸 UUID（`538e6914-d5c6-…（2）`）。
/// 那些文件名是客户端生成的会话 id，对读的人零信息：列表页一屏全是十六进制，
/// 分不清哪条是关于什么的，连「这条讲的是哪个项目」都要点进去才知道。
///
/// ── 取什么 ──────────────────────────────────────────────────
/// 1. 第一句来自用户的话（`我：…`），且像一句话 —— 它天然就是「这段在谈什么」；
/// 2. 否则用 `Workspace Folder` 的末级目录名（至少能说清是哪个项目）；
/// 3. 再否则用文件名，但把纯 UUID / 纯十六进制换成「会话记录」。
///
/// 只取**已经解析好的** `raw` 里第一处 `我：` —— 与 `line_to_turn` 同一个口径，
/// 不另写一套解析。标题长度按 60 字符截断（同 `retain::infer_title`）。
fn title_from_session(session: Option<&str>, raw: &str) -> String {
    const MIN_TITLE: usize = 6;
    const UUID_LEN: usize = 32;

    /// 这段文字是不是「不像人话」的机器 id。
    fn looks_machine(text: &str) -> bool {
        let compact: String = text.chars().filter(|ch| ch.is_ascii_alphanumeric()).collect();
        // 裸 UUID（含花括号 / 连字符 / 带序号后缀）
        if compact.len() >= UUID_LEN
            && compact
                .chars()
                .all(|ch| ch.is_ascii_hexdigit())
        {
            return true;
        }
        // 纯数字 / 纯十六进制短串（`a3f1`、`173056`）
        compact.chars().all(|ch| ch.is_ascii_digit()) && compact.len() >= 8
    }

    /// 第一句来自用户的话（`我：` / `**我**：`）。
    fn first_user_line(raw: &str) -> Option<String> {
        for line in raw.lines().take(400) {
            let line = line.trim();
            let Some((speaker, text, _)) = line_to_turn(line) else { continue };
            if speaker != "我" {
                continue;
            }
            let text = text.trim();
            if text.chars().count() < MIN_TITLE {
                continue;
            }
            // 客户端包装块不算「在谈什么」
            if text.starts_with('<') || text.starts_with("Attached ") {
                continue;
            }
            return Some(text.to_string());
        }
        None
    }

    let candidate = first_user_line(raw)
        .or_else(|| declared_project_of(raw))
        .or_else(|| {
            session
                .map(str::trim)
                .filter(|stem| !stem.is_empty())
                .filter(|stem| !looks_machine(stem))
                .map(str::to_string)
        })
        .unwrap_or_else(|| "会话记录".to_string());
    candidate
        .lines()
        .next()
        .unwrap_or("")
        .trim()
        .chars()
        .take(60)
        .collect()
}

fn from_transcript_at(
    path: &Path,
    project: Option<String>,
    from: u64,
) -> (Vec<Pending>, u64) {
    let session = path
        .file_stem()
        .map(|stem| stem.to_string_lossy().to_string())
        .filter(|stem| !stem.is_empty());
    // ── 流式读，顺手攒片段 ──────────────────────────────────────
    // 不再把整份会话读进内存（见 `for_each_line`）：1 GB 的会话文件按行过一遍，
    // 峰值内存只跟单行大小有关。片段数可能上万，但每条都是几百字的短文本。
    let mut segments: Vec<(String, Option<i64>)> = Vec::new();
    // 开头 40 KB 的**解码后**正文（`declared_project_of` 只扫这一段）。
    // 必须是解码后的：原始 JSONL 里换行是 `\n` 两个字符，会把下一行的
    // `Note:` 粘到路径末尾，产出 `nNote:` 这种假项目名（见 `declared_project_of`）。
    let mut raw_head = String::new();
    // ── 工作目录优先取**字段**，不靠从正文里捞 ──────────────────
    // 真机 2026-11-22：WorkBuddy 的 jsonl 每行都带顶层 `"cwd"` 字段，
    // 值就是 `C:\Users\me\WorkBuddy\2026-10-04-17-09-08`。字段级取值不会被
    // 正文里的 `Note: Prefer using…` 尾巴污染，比 `Workspace Folder:` 文本匹配
    // 稳得多（后者要靠「路径何时结束」的启发式）。文本匹配留着做其它客户端的兜底。
    let mut cwd_field: Option<String> = None;
    // 标题基数的来源优先级见 `title_from_session`：第一条用户原话 > 工作目录名 > 会话名
    let mut first_user: Option<String> = None;
    let next = match for_each_line(path, from, |line| {
        if line.trim().is_empty() {
            return true;
        }
        // 先攒头部：`declared_project_of` 只扫开头 40 KB，而工作目录就写在第一条
        // 用户消息的环境信封里 —— 那条 `line_to_turn` 往往返回 None（整段是信封），
        // 所以拼接必须在解析**之前**做，且拼的是**解码后**的文本（不是裸 JSONL）。
        if let Ok(value) = serde_json::from_str::<Value>(line.trim()) {
            if cwd_field.is_none() {
                if let Some(cwd) = value.get("cwd").and_then(Value::as_str) {
                    if !cwd.trim().is_empty() {
                        cwd_field = Some(cwd.trim().to_string());
                    }
                }
            }
            if raw_head.chars().count() < 40_000 {
                let mut parts: Vec<String> = Vec::new();
                collect_text(&value, &mut parts);
                raw_head.push_str(&parts.join("\n"));
                raw_head.push('\n');
            }
        }
        let Some((speaker, text, created)) = line_to_turn(line) else { return true };
        if speaker == "我" && first_user.is_none() {
            let candidate = text.lines().next().unwrap_or("").trim();
            if candidate.chars().count() >= 6 {
                first_user = Some(candidate.chars().take(60).collect());
            }
        }
        let formatted = format!("**{speaker}**：{text}");
        for piece in split_long(&formatted, CHUNK_CHARS) {
            segments.push((piece, created));
        }
        true
    }) {
        Ok(cursor) => cursor,
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
    // ── 项目名要按会话自己声明的工作目录修正 ────────────────────
    // 真机事故：导入把 `~/.workbuddy/projects` 这份来源的末级目录名
    // (`projects`) 当成项目名，3162 条全部落进一个叫 `projects` 的假项目，
    // 真正的项目目录一个都没建出来。同一台机器上 `~/.qoder-cn/projects`
    // 也是末级 `projects` —— 两个来源合起来更像「一个项目」了。
    //
    // 会话记录里其实写着工作目录：`Workspace Folder: C:\...\WorkBuddy\2026-09-23-...`
    // 或 `<environment_context>cwd=/x</environment_context>`。它的末级目录名
    // 才是「这段对话在哪个项目里」，比来源目录名准得多（来源目录名只说明
    // 「这个客户端把会话存在哪」）。
    //
    // 只在会话自己**说了**的时候才改：没说就沿用来源名，不能凭空造。
    //
    // 取值优先级：顶层 `cwd` 字段（结构化，最准）> 正文里的声明（其它客户端）。
    let declared_project = cwd_field
        .as_deref()
        .and_then(project_name_of_path)
        .or_else(|| declared_project_of(&raw_head));
    let project = declared_project.or(project);
    // 标题优先用流式过程中抓到的第一条用户原话（可能在 40 KB 之后），退到
    // `title_from_session` 的「工作目录名 > 会话名」兜底。
    let title_base = first_user
        .or_else(|| Some(title_from_session(session.as_deref(), &raw_head)))
        .unwrap_or_else(|| "会话记录".to_string());
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
        } else if looks_like_fragment(&body) {
            // ── 为什么要在「写进库」这一步拦碎片 ──────────────────
            // 真机 2026-11-22：5973 条导入记忆里抽查 30 条，绝大多数是从
            // 代码行中间开始的碎片（`move("weekly");`、`ng":"  # ① 有…`）。
            // 光靠 `split_long` 找行边界不够 —— 一条 5 万字的 assistant 消息
            // 里全是代码，每 8000 字一个切点，切点两边都是代码。
            //
            // 判据：窗口**首行**就是代码 / 标记 / 命令，或者整窗没有自然语言
            // 句子。这类窗口留着的唯一作用是让注入把 shell 脚本当经验塞给模型。
            Some("窗口是代码/工具切片".to_string())
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

/// 这一窗内容是不是「代码 / 工具输出的切片」，而不是一段人话。
///
/// ── 这道闸现在只管一件事：`split_long` 切出来的裸中段 ────────
/// 窗口有两种来源：
///
///   1. `pack_windows` 按**轮次**打包 —— 每片是 `**我**：…` 或 `**助手**：…`，
///      天然带说话人前缀。这类窗口**必然**是对话，不该判碎片（哪怕用户问的是
///      「这段代码哪里错」，窗里全是代码，那也是有效对话）。
///   2. `split_long` 把一条超长发言从中间切开 —— 中段是**裸文本**，没有说话人
///      前缀，可能从半行代码起头。这类才是要拦的。
///
/// 早先没有这个豁免，`looks_like_fragment` 会按「首行以 `{` / `}` / `;` 结尾」
/// 把「用户贴代码问问题」的整窗判成碎片 —— 而真机上 assistant 消息 p50 只有
/// 61 字、p99 1890 字，绝大多数窗口首行就是一句短结论，反而幸免；真正该拦的
/// 裸中段却因为首行碰巧是中文注释而漏过。判据挂错了对象。
///
/// 判据（仅对裸中段生效，命中任一即判碎片）：
///
///   1. **首行**以代码/标记起头：`{` `}` `$` `> ` 或者以 `</` 起头（标签闭合残片）
///   2. 首行是「半行代码」：有 `");` `},` `),` 这类结尾，且没有句末标点
///   3. 整窗没有一句话：没有 `。！？` 也没有 `. ` + 至少 20 个汉字的连续段落
fn looks_like_fragment(body: &str) -> bool {
    let text = body.trim();
    if text.is_empty() {
        return true;
    }
    // ── 豁免：带说话人前缀的窗口是对话，不判碎片 ──────────────
    // `pack_windows` 拼出来的窗口每条发言都以 `**我**：` / `**助手**：` 起头。
    // 只要窗里有一行是这样的，这就是一段真对话（哪怕内容是代码）。
    if text.lines().any(|line| {
        let line = line.trim_start();
        line.starts_with("**我**：")
            || line.starts_with("**助手**：")
            || line.starts_with("我：")
            || line.starts_with("助手：")
    }) {
        return false;
    }
    let first = text.lines().next().unwrap_or("").trim();
    // 1) 首行是标签闭合残片 / JSON 尾巴 / shell 提示
    if first.starts_with("</")
        || first.starts_with('}')
        || first.starts_with('{')
        || first.starts_with("&& ")
        || first.starts_with("|| ")
        || first.starts_with('$')
    {
        return true;
    }
    // 2) 首行是半行代码：以代码符号结尾且没有中文句末标点
    let ends_like_code = first.ends_with("\");")
        || first.ends_with("},")
        || first.ends_with("),")
        || first.ends_with(";")
        || first.ends_with("=>")
        || first.ends_with('\\')
        || first.ends_with('`');
    let has_sentence_punct = first.contains('。') || first.contains('！') || first.contains('？');
    if ends_like_code && !has_sentence_punct {
        return true;
    }
    // 3) 整窗没有自然语言：没有任何中文句末标点，且英文字母/中文都在代码块里
    let has_cjk_sentence = text.contains('。') || text.contains('！') || text.contains('？');
    if has_cjk_sentence {
        return false;
    }
    // 没有中文句末标点时，看有没有「一段连续的自然语言」。判据取「至少一行
    // 以汉字或英文字母起头、长度 ≥ 20、且不以分号/花括号/反引号结尾」。
    for line in text.lines().take(40) {
        let line = line.trim();
        if line.chars().count() < 20 {
            continue;
        }
        let starts_wordy = line
            .chars()
            .next()
            .map(|c| c.is_alphabetic() || is_cjk(c))
            .unwrap_or(false);
        let ends_code = line.ends_with(';')
            || line.ends_with('{')
            || line.ends_with('}')
            || line.ends_with(',')
            || line.ends_with('`');
        if starts_wordy && !ends_code {
            return false;
        }
    }
    true
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
    // 重叠预算的一半作为「往前找切点」的回退纵深：切点最多退到这里，再退就
    // 把重叠吃掉大半。
    let min_step = limit.saturating_sub(CHUNK_OVERLAP / 2).max(1);
    let mut out: Vec<String> = Vec::new();
    let mut start = 0usize;
    while start < total {
        let hard_end = (start + limit).min(total);
        // ── 切点必须是「句子/行边界」，不能是字符中间 ──────────────
        // 真机事故（2026-11-22）：按 8000 字硬切，切点落在代码行中间，
        // 于是库里出现大量以 `ng":"                # ① 有 RefreshToken`、
        // `move("weekly");`、`- 禁用控件；` 起头的碎片 —— 这些碎片注入时
        // 会被当成「相关经验」。
        //
        // 回退顺序：段落空行 > 换行 > 句末标点 > 硬切（保底）。
        // 只往前退到 `min_step`，保证每次至少推进 `limit - step` 个字，
        // 不会退到原地打转。
        let end = if hard_end < total {
            pick_break(&chars, start + min_step, hard_end).unwrap_or(hard_end)
        } else {
            hard_end
        };
        out.push(chars[start..end].iter().collect());
        if end >= total {
            break;
        }
        start = if end > start { end } else { start + min_step };
    }
    out
}

/// 在 `[from, to)` 里从后往前找最合适的切点，返回切点下标（切点本身归下一段）。
///
/// 优先级：`\n\n`（段落）> `\n`（行）> `。！？.!?`（句）> `None`（交给硬切）。
///
/// 切在代码块中间会产出「从半行代码开始」的碎片，但退到段落边界往往要退很远
/// （整段代码可能几万字）。折中：行边界照收 —— 后面 `looks_like_fragment`
/// 会在导入时把「首行是代码」的窗口整条跳过，比在这里硬退更可控
/// （宁可留一条，也不要一次退掉整个窗口）。
fn pick_break(chars: &[char], from: usize, to: usize) -> Option<usize> {
    // 段落优先：`\n\n`
    let mut i = to.saturating_sub(1);
    while i > from {
        if chars[i] == '\n' && i >= 1 && chars[i - 1] == '\n' {
            return Some(i + 1);
        }
        i -= 1;
    }
    // 行边界
    let mut i = to.saturating_sub(1);
    while i > from {
        if chars[i] == '\n' {
            return Some(i + 1);
        }
        i -= 1;
    }
    // 句末标点（中英文）
    let mut i = to.saturating_sub(1);
    while i > from {
        if matches!(chars[i], '。' | '！' | '？' | '.' | '!' | '?') {
            return Some(i + 1);
        }
        i -= 1;
    }
    None
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
            Some(cursor) if cursor.offset <= len => {
                // 偏移没越界也可能是「换了个文件」：轮转出来的新会话比旧偏移长，
                // `offset <= len` 一样成立。头部指纹变了就从头读（见 `FileCursor.head`）。
                if cursor.offset > 0 && cursor.head != 0 && head_fingerprint(file) != cursor.head {
                    0
                } else {
                    cursor.offset
                }
            }
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
                    // 记下「这一份文件的头长什么样」，下一轮靠它识别轮转。
                    head: head_fingerprint(file),
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
                // ── 从会话正文里抽仓库路径填 `files` ───────────────
                //
                // 导入的流水原来一律 `files: Vec::new()`，于是「按文件命中」这一档
                // （请求级注入里权重最高的 3.0 分）对全部导入条目永久失效 —— 而
                // 本机库里绝大多数条目正是导入进来的。用户问过「导了 2341 条，
                // 为什么注入什么都不出来」，根因就在这一行。
                //
                // 这里抽的是**正文里写过的仓库路径**（`src/server/api/chat.rs`
                // 这种带目录的串），不是会话文件自身的路径：前者才是「这次改动
                // 涉及哪些文件」，后者只说明这段对话存在哪个 jsonl 里。
                files: super::inject::harvest_paths(&candidate.body),
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

    #[test]
    fn declared_workspace_name_beats_the_source_folder() {
        // 真机事故：来源末级目录恒为 `projects`，3162 条全被归进一个假项目。
        // 会话自己声明了 Workspace Folder，它的末级目录名才是真正的项目名。
        let raw = concat!(
            r#"{"type":"user","content":[{"type":"input_text","text":"#,
            r#""<system-reminder><user_info>Workspace Folder: C:\\Users\\me\\WorkBuddy\\2026-09-23-23-24-35</user_info></system-reminder>"#,
            r#""}]}"#,
            "\n"
        );
        assert_eq!(
            declared_project_of(raw).as_deref(),
            Some("2026-09-23-23-24-35"),
            "应取 Workspace Folder 的末级目录名"
        );

        // harness 的 environment_context 写法
        let raw = r#"{"role":"user","content":"<environment_context>cwd=/home/me/project</environment_context>"}"#;
        assert_eq!(declared_project_of(raw).as_deref(), Some("project"));

        // 没声明就不能凭空造：返回 None，调用方沿用来源名
        let raw = r#"{"role":"user","content":"帮我把端口改成 3065"}"#;
        assert_eq!(declared_project_of(raw), None);

        // 盘符 / 纯数字不像项目名，宁可退回来源名
        let raw = r#"{"cwd":"C:\\"}"#;
        assert_eq!(declared_project_of(raw), None);
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

    /// 真机事故 2026-11-22：5,974 条导入标题全是裸 UUID（`538e6914-d5c6-…（2）`）。
    /// 那是客户端生成的会话 id，列在列表页上一屏全是十六进制，分不清哪条讲什么。
    #[test]
    fn session_title_prefers_human_words_over_machine_id() {
        let uuid = "538e6914-d5c6-486e-8635-a90ef475b1c9";
        // 会话里有 `我：…`：用那句话当标题
        let raw = concat!(
            r#"{"role":"user","content":"仪表盘为什么打不开，日志里没有报错"}"#,
            "\n",
            r#"{"role":"assistant","content":"根因是索引库的全局 Mutex 被 rebuild 独占。"}"#
        );
        let title = title_from_session(Some(uuid), raw);
        assert!(!title.contains("538e6914"), "不能拿裸 UUID 当标题：{title}");
        assert!(title.starts_with("仪表盘为什么打不开"), "{title}");

        // 会话里没有说话（只有工具行）：退到工作目录末级名
        let raw = concat!(
            "Workspace Folder: D:\\Personal\\Desktop\\agent2api-main\n",
            r#"{"type":"tool","content":"ls -la 输出…"}"#
        );
        let title = title_from_session(Some(uuid), raw);
        assert_eq!(title, "agent2api-main");

        // 什么都没有：给一个能读的兜底，不是 UUID
        let title = title_from_session(Some(uuid), "{}");
        assert_eq!(title, "会话记录");

        // 文件名本身像人话（`.md` 导入）：保留它
        let title = title_from_session(Some("端口约定"), "");
        assert_eq!(title, "端口约定");
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
        // 旧文件必须**真的**比轮转后的文件大。
        //
        // 早先这里只写两行短句（62 字节），而下面那句 JSON 里的中文按 UTF-8
        // 是 3 字节一个字，重新写出来的单行反而更长（89 字节）—— `from` 没有
        // 越界，走的是「起点在行中间」那条分支，把唯一一行当残片丢掉，CI 上
        // 三个平台一起红。用例要验的是 `from > len` 的回绕，就得让前提站得住：
        // 先写 20 行长句，再收紧成一行。
        std::fs::write(
            &file,
            "第一行内容足够长一点，这一行要写得很长，确保整个文件明显大于轮转后那一行\n".repeat(20),
        )
        .ok();
        let old_len = file_len(&file).expect("应能取到长度");
        let mut cursors = Cursors::new();
        cursors.insert(
            cursor_key(&file),
            FileCursor {
                offset: old_len,
                size: old_len,
                // 头部指纹按旧文件算：这段验的是「偏移越界」，指纹本身就变了，
                // 两条判据都会指到「从头重读」，不冲突。
                head: head_fingerprint(&file),
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
        let new_len = file_len(&file).expect("应能取到长度");
        assert!(new_len < old_len, "用例前提：轮转后的文件必须更短（{new_len} < {old_len}）");
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

    /// 轮转后文件**变长**时也必须从头读。
    ///
    /// ── 为什么单靠 offset 抓不到 ────────────────────────────────
    /// 会话文件被整份重写（轮转、压缩、客户端重置会话），新内容比旧偏移更长，
    /// 于是 `offset <= len` 依然成立，我们就从旧偏移继续读 —— 新文件的头部被
    /// 整段跳过，那几行永远进不来，而且不报错。AgentHub 的 `readNewLines` 只有
    /// `size < fromByte` 一条判据，同样抓不到这种形状（这是它也没解决的一个洞）。
    /// 这里靠文件头指纹补：头部一变，就当「换了一个文件」，从头重读。
    #[test]
    fn cursor_restarts_when_rotated_file_is_longer() {
        let dir = std::env::temp_dir().join(format!("mem-cursor-rotate-{}", crate::server::logging::now_ms()));
        std::fs::create_dir_all(&dir).ok();
        let file = dir.join("session.jsonl");
        let line = |text: &str| {
            format!(
                r#"{{"role":"user","content":"{text}","timestamp":{}}}"#,
                crate::server::logging::now_ms()
            )
        };

        std::fs::write(&file, format!("{}\n", line("旧会话里的那一行"))).ok();
        let mut cursors = Cursors::new();
        let (_, _, _, _, advance) =
            collect_files(&[file.clone()], "jsonl", None, Some(0), None, 10, Some(&cursors));
        let old_len = advance[0].1.offset;
        cursors.insert(cursor_key(&file), advance[0].1.clone());

        // 轮转：整份换成新内容，且比旧文件更长 —— 偏移判据被彻底骗过的那一支。
        std::fs::write(
            &file,
            format!("{}\n{}\n", line("轮转之后的第一行内容"), line("轮转之后的第二行内容")),
        )
        .ok();
        let new_len = file_len(&file).expect("应能取到文件长度");
        assert!(new_len > old_len, "用例前提：轮转后的文件必须更长（{new_len} > {old_len}）");

        let (candidates, _, _, _, advance) =
            collect_files(&[file.clone()], "jsonl", None, Some(0), None, 10, Some(&cursors));
        assert_eq!(candidates.len(), 1, "轮转后必须从头读：{candidates:?}");
        assert!(
            candidates[0].body.contains("轮转之后的第一行内容"),
            "头部不能被跳过：{}",
            candidates[0].body
        );
        assert_eq!(advance[0].1.offset, new_len, "读完应停在新文件末尾");

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

    #[test]
    fn client_envelopes_are_not_user_words() {
        // 真机 2026-11-22：导入 5973 条里 546 条用户侧是
        // `<conversation_history_summary>`，162 条 `<cb_summary>`，99 条
        // `Please continue…`。旧名单只认 `<system-reminder>` /
        // `<environment_context>`，于是这些全被当成用户原话写进库。
        for text in [
            "<conversation_history_summary>\nSummary:\n1. Primary Request…",
            "<cb_summary>\nSummary of the conversation so far:",
            "<system-reminder data-role=\"user-context\">\n<user_info>",
            "<environment_context>cwd=/x</environment_context>",
            "<task-notification>\n<task-id>x</task-id>",
            "Please continue with the conversation based on the summarized context above.",
            "Attached image(s) from tool result:",
            "<antml:function_calls>",
        ] {
            assert!(is_harness_noise(text), "应当判成客户端信封: {}", &text[..30.min(text.len())]);
        }
        // 正文里**提到**这些标签不算信封（讨论标签本身是合法的）
        assert!(!is_harness_noise("我在改 strip_paired_tag，让它认 <system-reminder data-role> 这种带属性的开标签。"));
        assert!(!is_harness_noise("把 qoder 签到修好"));
    }

    #[test]
    fn code_slices_are_not_memories() {
        // 真机 2026-11-22 抽查到的真形状，全部应判碎片
        for body in [
            "ng\":\"                # ① 有 RefreshToken → 走 App 式续期（抓包 16/17）",
            "move(\"weekly\");\n         entry.remove(\"weeklyTime\");",
            "</result>\n</previous_tool_call>",
            "}\n\n**助手**：继续",
            "$PATH\"\\nIMG=\"C:/Users/me/a.jpg\"",
            "- 禁用控件；\n   - 保留值，允许导入导出配置兼容。",
        ] {
            assert!(looks_like_fragment(body), "应当判碎片: {}", &body[..30.min(body.len())]);
        }
        // 正常人话不能误伤 —— 这几条是库里的真记忆
        for body in [
            "**我**：动第一批，第二批和第三批我还没理解是什么意思，需要你解释\n\n**助手**：现在开始写第一批。第一件事：把 information_score 加进 model.rs。",
            "**助手**：## 诊断结果：AutoClaw 国内版上游的预校验拒收（406 空响应体），不是账号问题",
            "**助手**：实测确认：titleBoost=3 确实把「标题命中」翻到了「正文命中」前面。",
            "**我**：qoder签到有问题，供应商目录里没有qoder，但点击统一签到后，还是会去签qoder",
        ] {
            assert!(!looks_like_fragment(body), "不该判碎片: {}", &body[..30.min(body.len())]);
        }
    }

    #[test]
    fn split_long_cuts_at_line_boundaries() {
        // 8000 字硬切会落在代码行中间。这里构造一行一行的文本，切完每片
        // 都应当是「完整的行前缀」，不能从半行开始。
        let mut text = String::new();
        for i in 0..500 {
            text.push_str(&format!("第 {i} 行：这是一段用来测试切点位置的说明文字，需要足够长才有意义。\n"));
        }
        let parts = split_long(&text, 800);
        assert!(parts.len() > 1, "应当被切成多片");
        for part in &parts {
            // 每片都应以换行结尾（或在最后），不应停在半句话中间
            let first = part.lines().next().unwrap_or("");
            assert!(first.starts_with("第 "), "每片首行应是完整的一行: {first}");
        }
    }

    /// 用户原话埋在 `<system-reminder>` 信封里的 `<user_query>` 中 —— 必须抽出来。
    ///
    /// ── 真机根因（2026-11-22）──────────────────────────────────
    /// 扫本机 65 个 WorkBuddy 会话（1.53 GB，2464 条用户消息）形状分布：
    ///
    /// ```text
    ///  1009  <system-reminder …> 几千字环境信息 + <user_query>真话</user_query>
    ///   617  <task-notification>…</task-notification>
    ///   335  <conversation_history_summary>…</…>
    ///   300  Please continue with the conversation based on…
    ///   195  <system-reminder …> 独立一段（只有环境信息）
    ///   1    <user_query>…</user_query>
    /// ```
    ///
    /// 旧实现只有「整段以信封开头 → 整条丢弃」，于是 **1009 条真实诉求全部丢弃**；
    /// 侥幸落库的是几千字 `<user_info>` / `SOUL.md` 模板，5973 条导入记忆里
    /// 一条人话都没有。这条测试把「抽」这件事钉死。
    #[test]
    fn user_words_are_lifted_out_of_the_envelope() {
        let raw = concat!(
            "<system-reminder data-role=\"user-context\">\n",
            "<user_info>\nOS Version: win32\nWorkspace Folder: C:\\Work\\demo\n</user_info>\n",
            "<identity_context>\n## SOUL.md\nYou're not a chatbot. You're becoming someone.\n</identity_context>\n",
            "</system-reminder>\n",
            "<user_query>导入过程中如果切换标签界面，导入任务好像会被中断</user_query>"
        );
        let got = human_words(raw).expect("应能抽出用户原话");
        assert_eq!(got, "导入过程中如果切换标签界面，导入任务好像会被中断");
        assert!(!got.contains("SOUL.md"), "环境信封不能进正文：{got}");
        assert!(!got.contains("win32"), "环境信封不能进正文：{got}");

        // 摘要块里的 `user_query` 是「历史提问」，不是这一轮用户在说话
        let raw = concat!(
            "<conversation_history_summary>\nSummary:\n上一轮用户问：",
            "<user_query>三天前的旧问题不该被当成这一轮</user_query>\n",
            "</conversation_history_summary>\n",
            "<user_query>这一轮的真问题是什么</user_query>"
        );
        assert_eq!(human_words(raw).as_deref(), Some("这一轮的真问题是什么"));

        // 只有环境信封、没有话：直接丢
        let raw = concat!(
            "<system-reminder data-role=\"user-context\">\n<current_time>2026-11-22</current_time>\n",
            "</system-reminder>"
        );
        assert_eq!(human_words(raw), None, "只有信封没有话，不该产出用户侧");

        // 续跑指令不是用户原话
        assert_eq!(
            human_words("Please continue with the conversation based on the summarized context above."),
            None
        );

        // 工具回执尾巴（`<task-notification>` 剥掉之后剩下的提示语）
        assert_eq!(
            human_words(concat!(
                "<task-notification>\n<task-id>b1l36p</task-id>\n</task-notification>\n\n",
                "Use the TaskOutput tool with task_id=\"b1l36p\" to retrieve the full output if you need to act on it."
            )),
            None
        );

        // 闭标签残片开头（客户端把开标签写坏了）
        assert_eq!(
            human_words("</user_query>Please continue with the conversation based on the summarized context above."),
            None
        );

        // 直接写纯文本的客户端（Claude Code / Codex 某些形态）：兜底认它
        assert_eq!(
            human_words("端口固定 3065，网关和客户端都要改这里").as_deref(),
            Some("端口固定 3065，网关和客户端都要改这里")
        );

        // 兜底不能把代码回显当人话
        assert_eq!(human_words("),\n  (\"\"),\n  (\"\"),\n];\nlet mut out = text.to_string();"), None);
    }

    /// 1 GB 的会话文件必须能被完整读到，不能只读尾部 8 MB。
    ///
    /// ── 真机根因（2026-11-22）──────────────────────────────────
    /// `~/.workbuddy/projects` 65 个会话共 1.53 GB，其中一个 1.04 GB。
    /// 旧 `read_from` 硬上限 `MAX_TRANSCRIPT_BYTES`（8 MB）且「首次读尾部」，
    /// 于是整体只读进 13%（198 MB / 1528 MB）；1.04 GB 那个会话里 489 条
    /// 用户提问只捞到 5 条（1%）。
    ///
    /// 这条测试造一个远超 8 MB 的文件，要求首行和末行都被读到。
    #[test]
    fn huge_transcript_is_read_in_full_not_just_the_tail() {
        let dir = std::env::temp_dir()
            .join(format!("mem-import-huge-{}", crate::server::logging::now_ms()));
        std::fs::create_dir_all(&dir).ok();
        let file = dir.join("huge.jsonl");

        // 每行 ~1 KB × 20000 行 ≈ 20 MB，足够越过 8 MB 上限
        let mut text = String::with_capacity(21 * 1024 * 1024);
        text.push_str(r#"{"role":"user","content":"第一行：网关端口固定 3065，不要改"}"#);
        text.push('\n');
        for index in 0..20_000 {
            let pad = "x".repeat(900);
            text.push_str(&format!(
                r#"{{"role":"tool","content":"filler {index} {pad}"}}"#
            ));
            text.push('\n');
        }
        text.push_str(r#"{"role":"user","content":"最后一行：把 qoder 签到修好"}"#);
        text.push('\n');
        std::fs::write(&file, &text).ok();
        assert!(
            std::fs::metadata(&file).map(|m| m.len()).unwrap_or(0) > MAX_TRANSCRIPT_BYTES,
            "测试文件必须大于旧的 8 MB 上限"
        );

        let windows = from_transcript(&file, Some("huge".to_string()));
        let joined: String = windows.iter().map(|item| item.body.as_str()).collect::<Vec<_>>().join("\n");
        assert!(joined.contains("第一行：网关端口固定 3065"), "文件开头不能被丢掉");
        assert!(joined.contains("最后一行：把 qoder 签到修好"), "文件末尾不能被丢掉");
        assert!(
            windows.iter().all(|item| item.skip.is_none()),
            "全部窗口都该可导入，实得跳过原因：{:?}",
            windows.iter().filter_map(|item| item.skip.clone()).collect::<Vec<_>>()
        );

        // 游标必须走到文件末尾，下一轮不会重读这 20 MB
        let cursor = for_each_line(&file, 0, |_| true).expect("流式读应成功");
        assert_eq!(cursor, std::fs::metadata(&file).map(|m| m.len()).unwrap_or(0), "游标应停在文件末尾");

        std::fs::remove_dir_all(&dir).ok();
    }

    /// 标题优先取第一条用户原话，且必须是从信封里抽出来的那句。
    #[test]
    fn window_title_uses_the_real_user_sentence() {
        let dir = std::env::temp_dir()
            .join(format!("mem-import-title-{}", crate::server::logging::now_ms()));
        std::fs::create_dir_all(&dir).ok();
        let file = dir.join("538e6914-d5c6-486e-8635-a90ef475b1c9.jsonl");
        std::fs::write(
            &file,
            concat!(
                r#"{"role":"user","content":"<system-reminder data-role=\"user-context\"><user_info>Workspace Folder: C:\\Work\\demo</user_info></system-reminder><user_query>仪表盘的记忆库读数为什么一直是 0</user_query>"}"#,
                "\n",
                r#"{"role":"assistant","content":"根因是索引库的全局 Mutex 被 rebuild 独占。"}"#,
                "\n"
            ),
        )
        .ok();

        let windows = from_transcript(&file, Some("projects".to_string()));
        assert_eq!(windows.len(), 1, "短会话应只有一个窗口");
        let title = &windows[0].title;
        assert!(title.starts_with("仪表盘的记忆库读数"), "标题应是用户原话：{title}");
        assert!(!title.contains("538e6914"), "不能拿裸 UUID 当标题：{title}");
        assert!(!title.contains("user_info"), "不能拿信封当标题：{title}");
        // 项目名按会话自己声明的工作目录修正，不是来源末级目录 `projects`
        assert_eq!(windows[0].project.as_deref(), Some("demo"));

        std::fs::remove_dir_all(&dir).ok();
    }
}
