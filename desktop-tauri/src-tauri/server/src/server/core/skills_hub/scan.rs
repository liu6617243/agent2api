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
    let files = collect_files(root);
    if files.is_empty() {
        return String::new();
    }
    let mut hasher = Sha256::new();
    hasher.update(format!("files:{}\n", files.len()).as_bytes());
    for (rel, abs) in files {
        hasher.update(rel.as_bytes());
        hasher.update([0u8]);
        match fs::read(&abs) {
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

/// 递归数文件数（挂载链接不计）。
pub fn count_files(root: &Path) -> usize {
    collect_files(root).len()
}

/// 体检：只报「会让客户端不认这个技能」或「会明显影响触发」的问题。
///
/// 分级与界面对齐：`bad` 必须处理（缺 SKILL.md / 缺 description），`warn`
/// 可以不管（缺 frontmatter / 缺 name / 描述超长）。
pub fn health_check(dir: &Path) -> Vec<Value> {
    let mut issues: Vec<Value> = Vec::new();
    if !dir.join("SKILL.md").is_file() {
        issues.push(json!({ "level": "bad", "text": "缺少 SKILL.md" }));
        return issues;
    }
    let parsed = match parse_skill_md(dir) {
        Ok(value) => value,
        Err(_) => {
            issues.push(json!({ "level": "bad", "text": "SKILL.md 读取失败（编码或权限）" }));
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

/// 把一个技能目录压成界面要的那一行。
fn build_entry(dir: &Path, tool: &str, origin: &str) -> Value {
    let meta = fs::symlink_metadata(dir).ok();
    let mtime_ms = meta
        .as_ref()
        .and_then(|value| value.modified().ok())
        .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|duration| duration.as_millis() as i64)
        .unwrap_or(0);
    let name = dir
        .file_name()
        .map(|value| value.to_string_lossy().to_string())
        .unwrap_or_default();
    let parsed = parse_skill_md(dir);
    let (skill_name, description, version) = match parsed {
        Ok(value) => {
            let skill_name = if value.name.trim().is_empty() { name.clone() } else { value.name.trim().to_string() };
            (skill_name, value.description, value.version)
        }
        Err(_) => (name.clone(), String::new(), String::new()),
    };
    json!({
        "name": name,
        "dir": dir.to_string_lossy(),
        "tool": tool,
        "origin": origin,
        "skillName": skill_name,
        "description": description,
        "version": version,
        "treeHash": tree_hash(dir),
        "health": health_check(dir),
        "mtimeMs": mtime_ms,
        "fileCount": count_files(dir),
    })
}

/// 扫一个渠道目录。
///
/// 返回 `(skills, mounts, broken)`：
/// * `skills` 普通技能与系统技能（`origin` 区分）；
/// * `mounts` 目录里的符号链接 / junction（指向中央库 = 正常挂载，指向别处 =
///   用户自己的链接，都记下来 —— 不记就等于让它隐身，排障时找不到）；
/// * `broken` 链接目标不存在的条数（仪表盘「失效挂载」的一项来源）。
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
            let target = fs::read_link(&abs).map(|value| normalize_link_target(&value)).unwrap_or_default();
            let valid = !target.is_empty() && Path::new(&target).exists();
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
