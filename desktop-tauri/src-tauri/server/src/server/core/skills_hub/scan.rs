//! 技能仓库 · 扫描层（SKILL.md 解析 / 内容树哈希 / 体检 / 渠道目录遍历）。
//!
//! ── 这一层只读 ──────────────────────────────────────────────
//! 文件系统在这里是**只读**的：解析、算哈希、给体检结论，一行都不写。所有写
//! 动作（收纳、挂载、进回收站）都在 `plan` 与 `store`。这条界线让「扫描」可以
//! 被仪表盘、去重页、同步预览反复调用而不产生任何副作用 —— 也正因为如此，
//! 用户点「扫描」不会悄悄改他的磁盘。
//!
//! ── SKILL.md 只认三个字段 ───────────────────────────────────
//! `name` / `description` / `metadata.version`。技能格式（Anthropic Agent
//! Skills 那一套）里还有 allowed-tools、license 等等，但**仓库不关心**：多认
//! 一个字段就多一处「某家客户端写法不同」的兼容负担，而这三个字段是各客户端
//! 都写、界面也都用得到的。其余字段原样留在文件里，扫描不碰。
//!
//! `description` 支持 YAML 块标量（`|` 保留换行、`>` 折成空格）：官方技能集里
//! 大量这么写，按「一行一个值」解析会把描述读成 `|` 三个字符，界面上就是错的。
//!
//! ── 内容树哈希为什么不用 mtime / 大小 ───────────────────────
//! 哈希是「是不是同一个技能」的唯一判据（L1 去重、挂载内容比对、台账校验都
//! 靠它）。mtime 在复制、解压、跨盘移动后全变，大小撞车更是常事。所以按**内容**
//! 算：文件按相对路径排序、文本统一成 LF（Windows 检出与 Linux 克隆应视作同一
//! 份），前 8K 含 0x00 视为二进制原样喂进去。目录里的**符号链接不进哈希** ——
//! 它们是挂载产物，跟进去会算成另一份不同内容（自己指向自己）。
//!
//! ── 系统自带技能 ────────────────────────────────────────────
//! 有些客户端在技能目录里放一层 `.system`，里面是它自己的技能，带一个
//! `.<名字>-system-skills.marker` 标记文件。那些不是用户的技能：不收纳、不挂载、
//! 不算孤儿，只在「扫到了什么」里标 `origin: "system"` 供界面区分。

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use serde_json::{json, Value};
use sha2::{Digest, Sha256};

/// 系统技能目录的标记文件后缀（`.<name>-system-skills.marker`）
const SYSTEM_MARKER_SUFFIX: &str = "-system-skills.marker";

/// SKILL.md 的解析结果。`ok = false` 表示没有 frontmatter（不是合法技能，
/// 但目录仍然会被列出来 —— 让用户看见「这里有个东西但客户端认不出」）。
pub struct SkillMd {
    pub ok: bool,
    pub name: String,
    pub description: String,
    pub version: String,
    pub raw: String,
}

/// 解析 `<dir>/SKILL.md`。读不到文件时返回 `Err`（调用方据此给「缺少 SKILL.md」）。
pub fn parse_skill_md(dir: &Path) -> Result<SkillMd, String> {
    let file = dir.join("SKILL.md");
    let raw = fs::read_to_string(&file).map_err(|error| format!("读取 SKILL.md 失败: {error}"))?;
    let mut parsed = SkillMd {
        ok: false,
        name: String::new(),
        description: String::new(),
        version: String::new(),
        raw: raw.clone(),
    };

    // frontmatter = 文件开头 `---` 到下一个独立 `---` 之间
    let body = match raw.strip_prefix("---") {
        Some(rest) => rest,
        None => return Ok(parsed),
    };
    let body = body.strip_prefix('\n').or_else(|| body.strip_prefix("\r\n")).unwrap_or(body);
    let mut lines: Vec<&str> = Vec::new();
    let mut closed = false;
    for line in body.split('\n') {
        let trimmed = line.trim_end_matches('\r');
        if trimmed.trim() == "---" {
            closed = true;
            break;
        }
        lines.push(trimmed);
    }
    if !closed {
        return Ok(parsed);
    }
    parsed.ok = true;

    let mut index = 0usize;
    let mut current_key = String::new();
    while index < lines.len() {
        let line = lines[index];
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            index += 1;
            continue;
        }
        // 缩进行：只有 metadata 段下的 version 有意义，其余忽略
        if line.starts_with(' ') || line.starts_with('\t') {
            if current_key == "metadata" {
                if let Some((key, value)) = split_key_value(trimmed) {
                    if key == "version" {
                        parsed.version = strip_quotes(&value);
                    }
                }
            }
            index += 1;
            continue;
        }
        let (key, value) = match split_key_value(trimmed) {
            Some(pair) => pair,
            None => {
                index += 1;
                continue;
            }
        };
        current_key = key.clone();
        if key == "metadata" {
            index += 1;
            continue;
        }
        // 块标量：`|` / `>` 可带 `-` / `+` 与缩进提示，后续更深的缩进行属于这个值
        let head = value.trim();
        let is_block = matches!(head.chars().next(), Some('|') | Some('>'))
            && head.chars().all(|c| matches!(c, '|' | '>' | '-' | '+' | '0'..='9'));
        let resolved = if is_block {
            let keep_newline = head.starts_with('|');
            let mut parts: Vec<String> = Vec::new();
            let mut next = index + 1;
            while next < lines.len() {
                let candidate = lines[next];
                if candidate.trim().is_empty() {
                    parts.push(String::new());
                    next += 1;
                    continue;
                }
                if !candidate.starts_with(' ') && !candidate.starts_with('\t') {
                    break;
                }
                parts.push(candidate.trim().to_string());
                next += 1;
            }
            index = next;
            let joined = if keep_newline {
                parts.join("\n")
            } else {
                parts.join(" ")
            };
            joined.trim().to_string()
        } else {
            index += 1;
            strip_quotes(&value)
        };
        match key.as_str() {
            "name" => parsed.name = resolved,
            "description" => parsed.description = resolved,
            _ => {}
        }
    }
    Ok(parsed)
}

/// `key: value` 拆分（键只认 ASCII 字母 / 下划线 / 连字符）
fn split_key_value(line: &str) -> Option<(String, String)> {
    let at = line.find(':')?;
    let key = line[..at].trim();
    if key.is_empty() || !key.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-') {
        return None;
    }
    Some((key.to_string(), line[at + 1..].trim().to_string()))
}

fn strip_quotes(value: &str) -> String {
    let text = value.trim();
    let bytes = text.as_bytes();
    if bytes.len() >= 2 {
        let first = bytes[0] as char;
        let last = bytes[bytes.len() - 1] as char;
        if (first == '"' || first == '\'') && first == last {
            return text[1..text.len() - 1].to_string();
        }
    }
    text.to_string()
}

/// 前 8K 有 0x00 视为二进制（与「文本统一 LF」共用同一份判定）
fn is_binary(bytes: &[u8]) -> bool {
    bytes.iter().take(8192).any(|byte| *byte == 0)
}

/// 收集目录下的普通文件（不跟随符号链接），按相对路径排序。
fn collect_files(root: &Path) -> Vec<(String, PathBuf)> {
    let mut out: Vec<(String, PathBuf)> = Vec::new();
    let mut stack: Vec<(PathBuf, String)> = vec![(root.to_path_buf(), String::new())];
    while let Some((dir, prefix)) = stack.pop() {
        let entries = match fs::read_dir(&dir) {
            Ok(value) => value,
            Err(_) => continue,
        };
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().to_string();
            let abs = entry.path();
            let rel = if prefix.is_empty() {
                name.clone()
            } else {
                format!("{prefix}/{name}")
            };
            let meta = match fs::symlink_metadata(&abs) {
                Ok(value) => value,
                Err(_) => continue,
            };
            let file_type = meta.file_type();
            if file_type.is_symlink() {
                // 挂载产物不进哈希：跟进去会把自己指向的那份内容算成「另一份」
                continue;
            }
            if file_type.is_dir() {
                stack.push((abs, rel));
            } else if file_type.is_file() {
                out.push((rel, abs));
            }
        }
    }
    out.sort_by(|a, b| a.0.cmp(&b.0));
    out
}

/// 目录内容树哈希（sha256，十六进制）。空目录返回空串 —— 见 `plan` 里
/// 「空哈希不参与 L1」的理由。
pub fn tree_hash(root: &Path) -> String {
    // 走内容缓存而不是直接读文件：挂载校验（`mount::verify_all` /
    // `plan` 的 L1 比对）对**同一个技能**要算两次 —— 链接路径一次、中央真身
    // 一次；两个路径 `canonicalize` 后是同一个键，第二次直接命中。
    // 而列表页 `build_entry` 里已经算过同一份事实，这里也一并复用。
    content_of(root).hash
}

/// 目录内容树哈希 —— **绕过缓存，现场读一遍**。
///
/// 写路径专用（复制 / 覆盖 / 还原进中央之后立刻校验）：同一个路径在写之前
/// 往往刚被读过（冲突检测要先比对两边哈希才决定动作），那份缓存还在
/// `BURST_WINDOW` 内。此时若走 `tree_hash`，拿到的会是**写入前**的旧哈希 ——
/// 记进台账就成了错的，而且看起来一切正常。
pub fn tree_hash_fresh(root: &Path) -> String {
    let files = collect_files(root);
    hash_of(&files)
}

/// 对已经收集好的文件清单算内容树哈希（不重复遍历目录）。
///
/// ── 为什么单独拆出来 ────────────────────────────────────────
/// 原来哈希与文件数各自走一遍 `collect_files`：列表页每行要 `treeHash` 与
/// `fileCount`，同一批目录就被 `read_dir` 两遍，第二遍的产出只有 `.len()`。
/// 拆出来后 `compute_content` 里一趟遍历同时喂给哈希、指纹、文件数。
///
/// 本机实测（105 个技能 / 3198 个文件 / 43MB）：一次全量扫描 1083ms，其中
/// 目录遍历约占 292ms —— 合并后这部分只付一次。
fn hash_of(files: &[(String, PathBuf)]) -> String {
    if files.is_empty() {
        return String::new();
    }
    let mut hasher = Sha256::new();
    hasher.update(format!("files:{}\n", files.len()).as_bytes());
    for (rel, abs) in files {
        hasher.update(rel.as_bytes());
        hasher.update([0u8]);
        match fs::read(abs) {
            Ok(bytes) => {
                if is_binary(&bytes) {
                    hasher.update(&bytes);
                } else {
                    let text = String::from_utf8_lossy(&bytes).replace("\r\n", "\n");
                    hasher.update(text.as_bytes());
                }
            }
            Err(_) => hasher.update(b"<unreadable>"),
        }
        hasher.update([0u8]);
    }
    let digest = hasher.finalize();
    digest.iter().map(|byte| format!("{byte:02x}")).collect()
}

// ─── 内容事实缓存 ────────────────────────────────────────────
//
// ── 为什么需要 ──────────────────────────────────────────────
// 列表页每一行都要 `treeHash` / `fileCount` / `description` / `health`，这些
// 全部来自「把技能目录里每个文件读出来算一遍」。本机实测（105 个技能 /
// 3198 个文件 / 43MB）：单渠道一次全量 1083ms。
//
// 更糟的是**同一个真身会被扫多次**：本机 `.workbuddy/skills` 与
// `.agents/skills` 两个渠道目录里各挂了 105 个链接，指向同一份
// `~/.agent_skills/skills/*`。按渠道逐个扫 = 同一批内容算两遍哈希。
//
// 每个页签切过去还要发 2~4 个请求（overview / library / daily-check …），
// 每个请求内部都各自重跑一遍全量扫描 —— 于是「点一下页签」实际是
// 「同一批 3198 个文件被读 4~8 遍」，几秒的等待就是这么来的。
//
// ── 两层收益 ────────────────────────────────────────────────
// 1. **按真实路径归并**（`fs::canonicalize`）：两个渠道里的同名链接落到同一个
//    缓存键，同一份内容一次扫描内只算一次。
// 2. **跨请求复用**：命中缓存时不重读文件内容，只用 stat 算一个指纹
//    （文件数 / 最大 mtime / 总字节）比对；指纹没变就直接返回上次的结果。
//    指纹变了才重算 —— 内容被改写必然改变 mtime 或大小，所以不会把旧结果
//    当成新的。
//
// 同一批请求（一个页签同时发 3 个）在 `BURST_WINDOW` 内连指纹都不算，直接
// 复用，避免「三个请求各走一遍 292ms 的目录遍历」。

/// 一次内容扫描的结果（与渠道无关：同一份真身挂到哪个渠道都是这些字段）。
#[derive(Clone)]
pub struct ContentFacts {
    /// SKILL.md 里的 name（可能为空，调用方自行回落到目录名）。
    pub skill_name: String,
    pub description: String,
    pub version: String,
    pub hash: String,
    pub file_count: usize,
    pub health: Vec<Value>,
    pub mtime_ms: i64,
}

/// 目录指纹：任一文件被改写都会让它变（mtime / 大小 / 文件数）。
#[derive(Clone, Copy, PartialEq, Eq)]
struct TreeSig {
    files: usize,
    max_mtime_ns: u128,
    total_bytes: u64,
}

struct CachedContent {
    sig: TreeSig,
    facts: ContentFacts,
    at: std::time::Instant,
}

/// 这个窗口内的重复请求连指纹都不重算（一个页签同时发多个请求时用）。
const BURST_WINDOW: std::time::Duration = std::time::Duration::from_millis(1500);

/// 缓存条目上限。正常情况下一台机器的技能数在几百量级，留 4 倍余量足够；
/// 超过就整体清空 —— 宁可多算一轮，也不让长期运行的进程无限涨内存
/// （用户可能反复改自定义目录，键是真实路径，会一直累积）。
const CACHE_LIMIT: usize = 2048;

fn content_cache() -> &'static std::sync::Mutex<std::collections::HashMap<String, CachedContent>> {
    static CACHE: std::sync::OnceLock<std::sync::Mutex<std::collections::HashMap<String, CachedContent>>> =
        std::sync::OnceLock::new();
    CACHE.get_or_init(|| std::sync::Mutex::new(std::collections::HashMap::new()))
}

/// 丢掉缓存。写操作（收纳 / 摘挂载 / 同步 / 改配置）之后调用 —— 那些动作会改
/// 磁盘，指纹虽然也能发现，但同一次请求里「写完立刻读」时 mtime 精度可能不够。
pub fn clear_cache() {
    if let Ok(mut cache) = content_cache().lock() {
        cache.clear();
    }
}

/// 只走目录、不读文件内容地算指纹（本机 105 个技能约 292ms）。
fn tree_sig(root: &Path) -> TreeSig {
    let files = collect_files(root);
    sig_of(&files)
}

fn sig_of(files: &[(String, PathBuf)]) -> TreeSig {
    let mut max_mtime_ns: u128 = 0;
    let mut total_bytes: u64 = 0;
    for (_, abs) in files {
        if let Ok(meta) = fs::metadata(abs) {
            total_bytes += meta.len();
            if let Ok(time) = meta.modified() {
                if let Ok(duration) = time.duration_since(std::time::UNIX_EPOCH) {
                    max_mtime_ns = max_mtime_ns.max(duration.as_nanos());
                }
            }
        }
    }
    TreeSig { files: files.len(), max_mtime_ns, total_bytes }
}

/// 真正读一遍内容，算出全部展示字段。
///
/// 目录只走一遍：`files` 同时喂给哈希与指纹，不再各自 `collect_files`。
fn compute_content(dir: &Path) -> (TreeSig, ContentFacts) {
    let files = collect_files(dir);
    let sig = sig_of(&files);
    let hash = hash_of(&files);
    let file_count = files.len();
    let mtime_ms = fs::metadata(dir)
        .ok()
        .and_then(|value| value.modified().ok())
        .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|duration| duration.as_millis() as i64)
        .unwrap_or(0);
    let parsed = parse_skill_md(dir);
    let (skill_name, description, version) = match &parsed {
        Ok(value) => (value.name.clone(), value.description.clone(), value.version.clone()),
        Err(_) => (String::new(), String::new(), String::new()),
    };
    let health = health_of(dir, parsed.as_ref().map_err(|_| ()));
    (
        sig,
        ContentFacts { skill_name, description, version, hash, file_count, health, mtime_ms },
    )
}

/// 取一份技能的内容事实（带缓存）。
///
/// 键是**真实路径**：链接与其目标共用一个键，所以两个渠道挂同一份真身时只算一次。
///
/// ── 锁只护「查表 / 写表」，不护磁盘 ────────────────────────
/// 指纹比对要遍历目录（单个技能几毫秒，全部加起来上百毫秒）。如果在持锁时做，
/// 一个页签并发发来的几个请求会在这把锁上排队 —— 本来是要省时间，反而变成串行。
/// 所以这里先取出快照就放锁，比对与重算都在锁外做。
pub fn content_of(dir: &Path) -> ContentFacts {
    let key = fs::canonicalize(dir).unwrap_or_else(|_| dir.to_path_buf());
    let key = path_key(&key);
    let now = std::time::Instant::now();

    // 快照：(上次的指纹, 上次的结果, 算出来的时刻)
    let hit = content_cache()
        .lock()
        .ok()
        .and_then(|cache| cache.get(&key).map(|item| (item.sig, item.facts.clone(), item.at)));

    if let Some((sig, facts, at)) = hit {
        // 一个页签同时发的几个请求落在窗口内：连指纹都不重算，直接复用。
        if now.duration_since(at) < BURST_WINDOW {
            return facts;
        }
        // 窗口外：stat 一遍比对指纹。没变说明内容没动过，旧结果仍然有效。
        if tree_sig(dir) == sig {
            return facts;
        }
    }

    let (sig, facts) = compute_content(dir);
    if let Ok(mut cache) = content_cache().lock() {
        if cache.len() >= CACHE_LIMIT && !cache.contains_key(&key) {
            // 到上限就整表清掉：这是纯性能缓存，清空的代价只是下一轮重算，
            // 而 LRU 记账本身要为每次命中改表，反而拖慢读路径。
            cache.clear();
        }
        cache.insert(key, CachedContent { sig, facts: facts.clone(), at: now });
    }
    facts
}

/// 递归数文件数（挂载链接不计）。
pub fn count_files(root: &Path) -> usize {
    content_of(root).file_count
}

/// 体检：只报「会让客户端不认这个技能」或「会明显影响触发」的问题。
///
/// 分级与界面对齐：`bad` 必须处理（缺 SKILL.md / 缺 description），`warn`
/// 可以不管（缺 frontmatter / 缺 name / 描述超长）。
pub fn health_check(dir: &Path) -> Vec<Value> {
    // 走内容缓存：列表页 `build_entry` 刚算过同一份事实，这里不该再把
    // SKILL.md 读一遍、frontmatter 再解析一遍。
    content_of(dir).health
}

/// `health_check` 的内核：SKILL.md 已经解析过就直接用，别再读一遍。
///
/// `compute_content` 每份技能本来就要 `parse_skill_md` 拿 name / description，
/// 紧接着体检又把同一个文件解析第二遍（含 frontmatter 逐行扫）。按目录缓存后
/// 这份结果跨请求复用，列表页每行省一次文件读 + 一次解析。
fn health_of(dir: &Path, parsed: Result<&SkillMd, ()>) -> Vec<Value> {
    let mut issues: Vec<Value> = Vec::new();
    let parsed = match parsed {
        Ok(value) => value,
        Err(_) => {
            // 读失败有两种：根本没有 SKILL.md（客户端不认这个技能，`bad`），
            // 还是文件在但读不出来（编码 / 权限）。用户看到的处置完全不同，
            // 所以这里区分一次 —— 一次 stat，不读内容。
            if !dir.join("SKILL.md").is_file() {
                issues.push(json!({ "level": "bad", "text": "缺少 SKILL.md" }));
            } else {
                issues.push(json!({ "level": "bad", "text": "SKILL.md 读取失败（编码或权限）" }));
            }
            return issues;
        }
    };
    if !parsed.ok {
        issues.push(json!({ "level": "warn", "text": "无 YAML frontmatter，客户端可能无法识别" }));
    }
    if parsed.ok && parsed.name.trim().is_empty() {
        issues.push(json!({ "level": "warn", "text": "frontmatter 缺 name 字段" }));
    }
    if parsed.ok {
        let chars = parsed.description.chars().count();
        if chars == 0 {
            issues.push(json!({ "level": "bad", "text": "缺 description，Agent 无法触发该技能" }));
        } else if chars > 1024 {
            issues.push(json!({ "level": "warn", "text": "description 超过 1024 字符，可能被截断" }));
        }
    }
    issues
}

/// 目录里的系统技能标记（`.system` 这类由客户端自己维护的子目录）
fn is_system_dir(dir: &Path) -> bool {
    let entries = match fs::read_dir(dir) {
        Ok(value) => value,
        Err(_) => return false,
    };
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        if name.starts_with('.') && name.ends_with(SYSTEM_MARKER_SUFFIX) {
            return true;
        }
    }
    false
}

/// 解析链接的真实落点：相对目标要按链接所在目录还原。
///
/// Windows 的 junction / `mklink` 一般写绝对路径，但 Git Bash 里手工
/// `ln -s ../x` 会留下相对目标 —— 那样 `Path::new(target).exists()` 会按
/// 当前工作目录解析，一律判成断链。
fn resolve_link_target(link: &Path) -> PathBuf {
    match fs::read_link(link) {
        Ok(value) if value.is_absolute() => value,
        Ok(value) => match link.parent() {
            Some(parent) => parent.join(value),
            None => value,
        },
        Err(_) => PathBuf::new(),
    }
}

/// 链接是否指向一个真技能（目标目录里有 SKILL.md）。
fn link_is_skill(link: &Path) -> bool {
    let resolved = resolve_link_target(link);
    !resolved.as_os_str().is_empty() && resolved.join("SKILL.md").is_file()
}

/// 把一个技能目录压成界面要的那一行。
///
/// 对链接行（`origin == "link"`）要跟到目标取时间：`symlink_metadata` 拿到的是
/// 链接自身的时间（创建那一刻），跟技能内容改没改无关。
fn build_entry(dir: &Path, tool: &str, origin: &str) -> Value {
    let name = dir
        .file_name()
        .map(|value| value.to_string_lossy().to_string())
        .unwrap_or_default();
    // 内容事实走缓存：同一份真身被多个渠道挂载时只读一遍文件。
    let facts = content_of(dir);
    let skill_name = if facts.skill_name.trim().is_empty() { name.clone() } else { facts.skill_name.trim().to_string() };
    let link_target = if origin == "link" {
        resolve_link_target(dir).to_string_lossy().to_string()
    } else {
        String::new()
    };
    json!({
        "name": name,
        "dir": dir.to_string_lossy(),
        "tool": tool,
        "origin": origin,
        "linkTarget": link_target,
        "skillName": skill_name,
        "description": facts.description,
        "version": facts.version,
        "treeHash": facts.hash,
        "health": facts.health,
        "mtimeMs": facts.mtime_ms,
        "fileCount": facts.file_count,
    })
}

/// 扫一个渠道目录。
///
/// 返回 `(skills, mounts)`：
/// * `skills` 普通技能、系统技能、以及**指向真技能的链接**（`origin` 区分：
///   `user` / `system` / `link`）；
/// * `mounts` 目录里所有符号链接 / junction（指向中央库 = 正常挂载，指向别处 =
///   用户自己的链接，都记下来 —— 不记就等于让它隐身，排障时找不到）；
///
/// ── 链接为什么也要产出技能行 ────────────────────────────────
/// 技能中枢（本机或别的客户端）是靠链接把真身挂进各渠道目录的。只把链接记成
/// 「挂载」而不产出技能行，界面就会「磁盘上有 106 个技能，列表里只有 1 个」——
/// 因为只有那个没被链接的真实目录才算技能。链接目标有 SKILL.md 就是客户端
/// 真能用的技能，必须让用户看见；这类链接**不参与**去重收纳（见 `plan` 里
/// `origin == "link"` 的排除），免得把别人中枢管的技能整批搬进本机中央库。
pub fn scan_channel(dir: &Path, tool: &str) -> (Vec<Value>, Vec<Value>) {
    let mut skills: Vec<Value> = Vec::new();
    let mut mounts: Vec<Value> = Vec::new();
    let entries = match fs::read_dir(dir) {
        Ok(value) => value,
        Err(_) => return (skills, mounts),
    };
    let mut children: Vec<PathBuf> = entries.flatten().map(|entry| entry.path()).collect();
    children.sort();
    for abs in children {
        let meta = match fs::symlink_metadata(&abs) {
            Ok(value) => value,
            Err(_) => continue,
        };
        if meta.file_type().is_symlink() {
            let resolved = resolve_link_target(&abs);
            let target = fs::read_link(&abs).map(|value| normalize_link_target(&value)).unwrap_or_default();
            let valid = !resolved.as_os_str().is_empty() && resolved.exists();
            if valid && resolved.join("SKILL.md").is_file() {
                skills.push(build_entry(&abs, tool, "link"));
            }
            mounts.push(json!({
                "tool": tool,
                "name": abs.file_name().map(|value| value.to_string_lossy().to_string()).unwrap_or_default(),
                "path": abs.to_string_lossy(),
                "target": target,
                "valid": valid,
            }));
            continue;
        }
        if !meta.file_type().is_dir() {
            continue;
        }
        if is_system_dir(&abs) {
            // 系统目录：只往下看一层子技能
            if let Ok(items) = fs::read_dir(&abs) {
                for item in items.flatten() {
                    let sub = item.path();
                    let sub_meta = match fs::symlink_metadata(&sub) {
                        Ok(value) => value,
                        Err(_) => continue,
                    };
                    if sub_meta.file_type().is_symlink() || !sub_meta.file_type().is_dir() {
                        continue;
                    }
                    skills.push(build_entry(&sub, tool, "system"));
                }
            }
            continue;
        }
        skills.push(build_entry(&abs, tool, "user"));
    }
    (skills, mounts)
}

/// Windows 的 junction `read_link` 会带 `\\?\` 前缀与尾部反斜杠，比较前抹平。
pub fn normalize_link_target(target: &Path) -> String {
    let text = target.to_string_lossy().to_string();
    let text = text.strip_prefix(r"\\?\").unwrap_or(&text).to_string();
    let text = text.trim_end_matches(['\\', '/']).to_string();
    if cfg!(windows) {
        text.to_lowercase()
    } else {
        text
    }
}

/// 路径比较键（Windows 不区分大小写）
pub fn path_key(path: &Path) -> String {
    let text = path.to_string_lossy().to_string();
    if cfg!(windows) {
        text.to_lowercase()
    } else {
        text
    }
}

/// 统计目录体积（回收站列表用）。只读，失败按 0 算。
pub fn dir_size(root: &Path) -> u64 {
    let mut total = 0u64;
    for (_, abs) in collect_files(root) {
        if let Ok(meta) = fs::metadata(&abs) {
            total += meta.len();
        }
    }
    total
}

/// 渠道目录里的技能目录数（设置页「探测到 N 个」用；挂载链接不计）。
pub fn count_skill_dirs(dir: &Path) -> usize {
    let entries = match fs::read_dir(dir) {
        Ok(value) => value,
        Err(_) => return 0,
    };
    entries
        .flatten()
        .filter(|entry| {
            fs::symlink_metadata(entry.path())
                .map(|meta| meta.file_type().is_dir() && !meta.file_type().is_symlink())
                .unwrap_or(false)
        })
        .count()
}

/// 渠道目录里「客户端看得见」的技能数：真实目录 + 指向真技能的链接。
///
/// 与 `count_skill_dirs` 的区别：后者只数真实目录（中央库 / 台账统计用，链接
/// 在那里算重复）；渠道矩阵要报的是「这个客户端到底能用几个技能」，链接挂进来
/// 的必须一起数，否则显示的数字和用户在客户端里看到的对不上。
pub fn count_channel_skills(dir: &Path) -> usize {
    let entries = match fs::read_dir(dir) {
        Ok(value) => value,
        Err(_) => return 0,
    };
    let mut total = 0usize;
    for entry in entries.flatten() {
        let path = entry.path();
        let meta = match fs::symlink_metadata(&path) {
            Ok(value) => value,
            Err(_) => continue,
        };
        let file_type = meta.file_type();
        if file_type.is_symlink() {
            if link_is_skill(&path) {
                total += 1;
            }
            continue;
        }
        if !file_type.is_dir() {
            continue;
        }
        if is_system_dir(&path) {
            if let Ok(items) = fs::read_dir(&path) {
                for item in items.flatten() {
                    let sub = item.path();
                    if fs::symlink_metadata(&sub)
                        .map(|meta| meta.file_type().is_dir() && !meta.file_type().is_symlink())
                        .unwrap_or(false)
                    {
                        total += 1;
                    }
                }
            }
            continue;
        }
        total += 1;
    }
    total
}

/// 目录第一层的子项名（技能详情页的文件树用；不进递归，避免把大目录读爆）。
pub fn list_children(dir: &Path) -> Vec<Value> {
    let mut rows: Vec<Value> = Vec::new();
    let entries = match fs::read_dir(dir) {
        Ok(value) => value,
        Err(_) => return rows,
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let meta = match fs::symlink_metadata(&path) {
            Ok(value) => value,
            Err(_) => continue,
        };
        let file_type = meta.file_type();
        let kind = if file_type.is_symlink() {
            "link"
        } else if file_type.is_dir() {
            "dir"
        } else {
            "file"
        };
        rows.push(json!({
            "name": entry.file_name().to_string_lossy(),
            "kind": kind,
            "size": if file_type.is_file() { meta.len() } else { 0 },
        }));
    }
    rows.sort_by(|a, b| {
        a.get("name").and_then(Value::as_str).unwrap_or("")
            .cmp(b.get("name").and_then(Value::as_str).unwrap_or(""))
    });
    rows
}

/// 说明文字（技能详情页头部用）：SKILL.md 的正文（去掉 frontmatter）。
pub fn skill_body(dir: &Path) -> String {
    match parse_skill_md(dir) {
        Ok(parsed) => {
            let raw = parsed.raw.replace("\r\n", "\n");
            match raw.find("\n---") {
                Some(at) => raw[at + 4..].trim_start_matches('\n').to_string(),
                None => raw,
            }
        }
        Err(_) => String::new(),
    }
}

/// 目录下的 SKILL.md 原文（详情页「看原文」用；读不到给空串）
pub fn skill_md_raw(dir: &Path) -> String {
    fs::read_to_string(dir.join("SKILL.md")).unwrap_or_default()
}

/// 汇总：`BTreeMap` 让「按渠道统计」的输出顺序稳定（前端不做二次排序）
pub fn empty_counts() -> BTreeMap<String, usize> {
    BTreeMap::new()
}
