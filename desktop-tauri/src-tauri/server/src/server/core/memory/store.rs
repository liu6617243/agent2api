//! 记忆中枢 · 文件层（事实源的读写）。
//!
//! ── 目录布局 ────────────────────────────────────────────────
//! ```text
//! {config_dir}/memory/
//!   config.json          模块设置（扁平点号键，见 `config`）
//!   projects.json        项目登记表（slug → 名称 / 远程 / 本地路径）
//!   device.json          本机设备 id（同步与冲突裁决用）
//!   index.db             派生索引（可随时删除重建）
//!   general/l1/<agent>/<YYYY-MM-DD>.md    全局（不属于任何项目）的原始日常
//!   general/l2/<sub>/<id>.md              全局的沉淀条目
//!   projects/<slug>/l1/<agent>/<YYYY-MM-DD>.md
//!   projects/<slug>/l2/<sub>/<id>.md
//!   profile/<id>.md      深层画像条目
//!   trash/<id>.md        回收站（单条一个文件，可按 id 恢复）
//! ```
//!
//! ── 为什么 l1 是「一天一文件多节」，l2 是「一条一文件」──────────
//! 原始日常（l1）一天几十条，一条一个文件会把目录撑成几千个 inode，而它们
//! 天然属于「同一天的同一段工作」—— 合成一个文件既省目录，又能直接看出「那天
//! 干了什么」。沉淀结论（l2）每条都是独立结论，要能被单独取代、单独
//! 附证据、单独删除，共用文件会让「删一条」变成「改一个共享文件」。
//!
//! 早期形态曾把所有 summary 塞进共享的 `l2/summary.md`，结果后写覆盖先写、
//! 索引里两条指向同一路径 —— 这就是 l2 必须一条一文件的原因。
//!
//! ── 原子写为什么是四步 ──────────────────────────────────────
//! 「先写临时文件 → fsync → 删旧文件 → rename」：同目录 rename 在 NTFS 与
//! ext4 上都是原子的，读者要么看到旧版本、要么看到新版本，不会看到半个文件。
//! 少一步 fsync 的后果是断电后 rename 成功但内容为空（ext4 上真实发生过）。
//!
//! ── 为什么删除是「搬到 trash/」而不是删文件 ─────────────────
//! 记忆的价值在「当时的判断」，误删一条往往几天后才发现。回收站让删除可逆，
//! 而 `deleted_at` 同时写进 frontmatter，于是「索引丢了但文件还在」时
//! 重建出来的依然是「已删除」状态 —— 两个真相不会打架。

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use serde_json::{json, Value};

use super::layout;
use super::model::{MemoryEntry, MemoryKind};

/// 不属于任何项目的记忆，落到这个伪项目下（`general/` 在磁盘上的别名）
pub const GLOBAL_SLUG: &str = "_global";

fn now_ms() -> i64 {
    crate::server::logging::now_ms()
}

/// memory 根目录。默认与网关统一库同源（`config::config_dir()`），
/// `storage.root` 配了就按配置走（换盘、放同步盘）。
pub fn root() -> PathBuf {
    let custom = super::config::text_of("storage.root");
    if custom.trim().is_empty() {
        crate::server::config::config_dir().join("memory")
    } else {
        PathBuf::from(custom.trim())
    }
}

pub fn projects_root() -> PathBuf {
    root().join("projects")
}

pub fn general_root() -> PathBuf {
    root().join("general")
}

pub fn profile_root() -> PathBuf {
    root().join("profile")
}

pub fn trash_root() -> PathBuf {
    root().join("trash")
}

pub fn config_file() -> PathBuf {
    root().join("config.json")
}

pub fn projects_file() -> PathBuf {
    root().join("projects.json")
}

pub fn index_file() -> PathBuf {
    root().join("index.db")
}

pub fn device_file() -> PathBuf {
    root().join("device.json")
}

/// 建齐目录。幂等，每次访问前调用（允许用户中途手工删掉某个子目录）。
pub fn ensure_layout() -> Result<(), String> {
    for dir in [root(), projects_root(), general_root(), profile_root(), trash_root()] {
        fs::create_dir_all(&dir).map_err(|error| format!("创建记忆目录失败 {}：{error}", dir.display()))?;
    }
    Ok(())
}

/// 项目 slug：只保留小写字母、数字、`-`、`_`、`.`；连续非法字符压成一个 `-`。
///
/// 为什么不用项目名直接当目录名：项目名可能带空格、中文、斜杠（`a/b` 会造出
/// 两级目录），而 slug 同时是接口参数与前端路由片段，必须是路径安全的。
pub fn slugify(name: &str) -> String {
    let slug = layout::sanitize_slug(name);
    if slug == GLOBAL_SLUG {
        GLOBAL_SLUG.to_string()
    } else {
        slug
    }
}

// ── 路径 ─────────────────────────────────────────────────────

/// 相对路径 → 绝对路径。
pub fn abs(relative: &str) -> PathBuf {
    root().join(relative.replace('/', std::path::MAIN_SEPARATOR_STR))
}

/// 绝对路径 → 相对 memory 根的正斜杠路径。
pub fn rel(path: &Path) -> String {
    path.strip_prefix(root())
        .map(|rest| rest.to_string_lossy().replace('\\', "/"))
        .unwrap_or_else(|_| path.to_string_lossy().replace('\\', "/"))
}

/// 一条记忆相对 memory 根的路径。`deleted` 为真时落 `trash/`。
///
/// 已有 `entry.path` 时以它为准确认落点（记忆可能在 general 与 projects 之间
/// 搬过家，重算会把它搬回旧地方）；路径为空时按 layer 现算。
pub fn entry_path(entry: &MemoryEntry, deleted: bool) -> PathBuf {
    if deleted {
        return trash_root().join(format!("{}.md", entry.id));
    }
    if !entry.path.is_empty() && !entry.path.starts_with("trash/") {
        let candidate = abs(&entry.path);
        // 只认「确实在 memory 根下」的相对路径：同步过来的 path 可能来自
        // 另一台机器（盘符不同），不能直接当绝对路径用
        if candidate.starts_with(root()) {
            return candidate;
        }
    }
    let slug = entry.project.as_deref().map(slugify).filter(|slug| slug != GLOBAL_SLUG);
    let agent = entry.agent.as_deref().unwrap_or("manual");
    let date = super::search::format_date(entry.created_at.max(entry.updated_at));
    let relative = layout::memory_rel_path(
        slug.as_deref(),
        entry.layer,
        agent,
        entry.kind.as_str(),
        &date,
        &entry.id,
    );
    abs(&relative)
}

/// 是否「一天一文件」的 l1 容器（`.../l1/<agent>/YYYY-MM-DD.md`）。
///
/// 判据是路径形状而不是 frontmatter：存量文件、同步过来的文件、用户手建的
/// 文件都只有路径这一条共同信息。
pub fn is_daily_container(relative: &str) -> bool {
    let segments: Vec<&str> = relative.split('/').collect();
    if segments.len() < 3 || segments[segments.len() - 2].is_empty() {
        return false;
    }
    // `/l1/` 段必须在文件名之前
    if !segments[..segments.len() - 1].contains(&"l1") {
        return false;
    }
    let stem = segments[segments.len() - 1].trim_end_matches(".md");
    looks_like_date(stem)
}

fn looks_like_date(text: &str) -> bool {
    let bytes = text.as_bytes();
    bytes.len() == 10
        && bytes[4] == b'-'
        && bytes[7] == b'-'
        && bytes[..4].iter().all(u8::is_ascii_digit)
        && bytes[5..7].iter().all(u8::is_ascii_digit)
        && bytes[8..10].iter().all(u8::is_ascii_digit)
}

/// 原子写：同目录 tmp → 写满 → fsync → 删旧 → rename。
pub fn write_atomic(path: &Path, contents: &str) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|error| format!("创建目录失败 {}：{error}", parent.display()))?;
    }
    // 改写前留一份 .bak：记忆是「只有用户自己能重建」的数据，一次错误的
    // 覆盖没有第二次机会。备份按份数滚动，不占空间。
    if super::config::bool_of("storage.backupBeforeWrite") && path.exists() {
        backup(path);
    }
    let tmp = path.with_extension(format!("tmp.{}", std::process::id()));
    {
        let mut file = fs::File::create(&tmp).map_err(|error| format!("写入失败 {}：{error}", tmp.display()))?;
        file.write_all(contents.as_bytes())
            .map_err(|error| format!("写入失败 {}：{error}", tmp.display()))?;
        // flush + sync_all：rename 之前必须让内容真正落盘
        file.flush().map_err(|error| format!("刷盘失败 {}：{error}", tmp.display()))?;
        file.sync_all().map_err(|error| format!("刷盘失败 {}：{error}", tmp.display()))?;
    }
    // Windows 上 rename 到已存在文件会失败，先删目标
    if path.exists() {
        fs::remove_file(path).map_err(|error| format!("替换失败 {}：{error}", path.display()))?;
    }
    fs::rename(&tmp, path).map_err(|error| {
        let _ = fs::remove_file(&tmp);
        format!("落盘失败 {}：{error}", path.display())
    })
}

/// `.bak` / `.bak.1` … 滚动。失败不影响主流程（备份是保险，不是流程）。
fn backup(path: &Path) {
    let keep = super::config::usize_of("storage.backupKeep").max(1);
    let base = path.with_extension(format!(
        "{}.bak",
        path.extension().and_then(|ext| ext.to_str()).unwrap_or("")
    ));
    let base = if path.extension().is_some() {
        // `a.md` → `a.md.bak`
        PathBuf::from(format!("{}.bak", path.display()))
    } else {
        base
    };
    for index in (1..keep).rev() {
        let from = if index == 1 {
            base.clone()
        } else {
            PathBuf::from(format!("{}.{}", base.display(), index - 1))
        };
        let to = PathBuf::from(format!("{}.{}", base.display(), index));
        if from.exists() {
            let _ = fs::rename(&from, &to);
        }
    }
    let _ = fs::copy(path, &base);
}

// ── l1 容器 ──────────────────────────────────────────────────

/// 容器头：记「这段流水属于哪天、哪个 agent、哪个项目」。
///
/// 为什么不把每条的 frontmatter 都写进容器：一条记忆已经有一份完整
/// frontmatter 写在它自己的文件里；容器里再嵌一层 YAML 会造出「改哪个才算数」
/// 的问题。容器头只记「这段流水是谁的」，条目元数据一律走 JSON 标记行。
fn container_head(date: &str, agent: &str, slug: Option<&str>) -> String {
    let mut out = String::new();
    out.push_str("---\n");
    out.push_str("type: daily-log\n");
    out.push_str(&format!("date: {date}\n"));
    out.push_str(&format!("agent: {agent}\n"));
    if let Some(slug) = slug.filter(|slug| !slug.is_empty() && *slug != GLOBAL_SLUG) {
        out.push_str(&format!("project: {slug}\n"));
    }
    out.push_str("---\n\n");
    out
}

/// 条目标记行。用 JSON 而不是 `key=value`：标签、标题里的空格与逗号不会再造
/// 解析歧义 —— 手写文件最容易踩的坑就是「标签里带逗号」。
fn section_marker(entry: &MemoryEntry) -> String {
    let meta = json!({
        "id": entry.id,
        "title": entry.title,
        "kind": entry.kind.as_str(),
        "tags": entry.tags,
        "importance": entry.importance,
        "created": entry.created_at,
        "updated": entry.updated_at,
        "pinned": entry.pinned,
        "starred": entry.starred,
        "files": entry.files,
        "supersedes": entry.supersedes,
    });
    format!("<!-- mem {} -->\n", serde_json::to_string(&meta).unwrap_or_else(|_| "{}".into()))
}

fn section_heading(entry: &MemoryEntry) -> String {
    let seconds = (entry.created_at / 1000).rem_euclid(86_400);
    format!(
        "## {:02}:{:02} · {}\n\n",
        seconds / 3600,
        (seconds % 3600) / 60,
        entry.title
    )
}

/// 把容器文本切成 `(元数据 JSON, 正文)` 序列。读不懂的段落整段当正文，
/// 不丢内容 —— 记忆文件最怕「解析器看不懂就吃掉」。
fn split_sections(text: &str) -> Vec<(Value, String)> {
    let normalized = text.replace("\r\n", "\n").replace('\r', "\n");
    let mut out: Vec<(Value, String)> = Vec::new();
    let mut current: Option<(Value, String)> = None;
    for line in normalized.lines() {
        if let Some(meta) = parse_marker(line) {
            if let Some(done) = current.take() {
                out.push(done);
            }
            current = Some((meta, String::new()));
            continue;
        }
        if let Some((_, body)) = current.as_mut() {
            body.push_str(line);
            body.push('\n');
        }
    }
    if let Some(done) = current {
        out.push(done);
    }
    // 去掉每条开头那一行 `## HH:MM · 标题`（我们写出去的可读性行）
    out.into_iter()
        .map(|(meta, body)| {
            let trimmed = body.trim_start_matches('\n');
            let body = if trimmed.starts_with("## ") {
                trimmed.split_once('\n').map(|(_, rest)| rest.to_string()).unwrap_or_default()
            } else {
                trimmed.to_string()
            };
            (meta, body.trim_matches('\n').to_string())
        })
        .collect()
}

fn parse_marker(line: &str) -> Option<Value> {
    let trimmed = line.trim();
    let inner = trimmed.strip_prefix("<!-- mem ")?.strip_suffix(" -->")?;
    serde_json::from_str::<Value>(inner.trim()).ok()
}

/// 一条记忆 → 容器里的一个 section。
fn section_text(entry: &MemoryEntry) -> String {
    let mut out = String::new();
    out.push_str(&section_marker(entry));
    out.push_str(&section_heading(entry));
    out.push_str(entry.body.trim_end());
    out.push_str("\n\n");
    out
}

/// 从容器文本里移除某个 id 的 section（删一条 daily 用）。
fn strip_section(text: &str, id: &str) -> (String, bool) {
    let normalized = text.replace("\r\n", "\n");
    let mut out = String::new();
    let mut skipping = false;
    let mut removed = false;
    for line in normalized.lines() {
        if let Some(meta) = parse_marker(line) {
            let same = meta.get("id").and_then(Value::as_str) == Some(id);
            if same {
                skipping = true;
                removed = true;
                continue;
            }
            skipping = false;
        }
        if !skipping {
            out.push_str(line);
            out.push('\n');
        }
    }
    (out, removed)
}

/// 把一条 l1 记忆写进（或更新到）它所属的当天容器。
fn save_section(entry: &MemoryEntry) -> Result<(), String> {
    ensure_layout()?;
    let target = entry_path(entry, false);
    let relative = rel(&target);
    let date = relative
        .split('/')
        .next_back()
        .map(|name| name.trim_end_matches(".md").to_string())
        .unwrap_or_else(|| super::search::format_date(entry.created_at));
    let existing = fs::read_to_string(&target).unwrap_or_default();
    // 先摘掉同 id 的旧 section：这是「更新一条 l1」的唯一正确做法 ——
    // 追加而不摘会得到两份同 id 的条目，重建索引时后者覆盖前者、删除时只删掉一份
    let (cleaned, had_old) = strip_section(&existing, &entry.id);
    let mut text = if had_old {
        cleaned
    } else if cleaned.trim().is_empty() {
        container_head(&date, entry.agent.as_deref().unwrap_or("manual"), entry.project.as_deref())
    } else {
        // 保留原头部（可能有用户手写的说明），只在末尾追加
        cleaned
    };
    if !text.is_empty() && !text.ends_with('\n') {
        text.push('\n');
    }
    if !text.is_empty() && !text.ends_with("\n\n") {
        text.push('\n');
    }
    text.push_str(&section_text(entry));
    write_atomic(&target, &text)
}

// ── 读写 ─────────────────────────────────────────────────────

/// 这条记忆**活着的时候**该不该落在「一天一文件」的容器里。
///
/// 判据是**目标路径的形状**，不是 `layer == L1 && kind == Daily`：
/// 存量用户的 daily 记忆是一个独立的 `<id>.md`（v1 形态），它的 `path` 已经
/// 指向那个文件。只按 layer/kind 判会让一条老记忆在下次保存时被塞进容器格式，
/// 原文件既没被删、内容又被新格式覆盖 —— 那是数据损坏。
fn live_is_container(entry: &MemoryEntry) -> bool {
    if entry.layer != super::model::Layer::L1 || entry.kind != MemoryKind::Daily {
        return false;
    }
    let live = MemoryEntry { deleted_at: None, ..entry.clone() };
    is_daily_container(&rel(&entry_path(&live, false)))
}

/// 把一条记忆写回事实源。
///
/// 只负责「写到它现在该在的位置」。**老位置**的清理是 [`detach_live`] 的事，
/// 由调用方在改类型 / 改层 / 改日期这类会换路径的场景里先调一次 —— 这里没有
/// 上一版对象，猜不出来。
pub fn save(entry: &MemoryEntry) -> Result<(), String> {
    ensure_layout()?;
    if entry.deleted_at.is_some() {
        // 进回收站前必须从原位置拿走，否则「同一个 id 两份文件」重建时会打架
        detach_live(entry)?;
        return write_atomic(&trash_root().join(format!("{}.md", entry.id)), &entry.to_markdown());
    }
    if live_is_container(entry) {
        return save_section(entry);
    }
    write_atomic(&entry_path(entry, false), &entry.to_markdown())
}

/// 把某个 id 从「活着的形态」里摘掉（容器摘一节 / 单条删文件），不动回收站。
///
/// 调用方负责之后的落盘。抽出来是因为三条路径（delete / 改类型换目录 / purge）
/// 都要做同一件事，而少做一次的后果都是「同一条记忆出现两份」。
pub fn detach_live(entry: &MemoryEntry) -> Result<(), String> {
    if live_is_container(entry) {
        return remove_section(entry);
    }
    let live = entry_path(&MemoryEntry { deleted_at: None, ..entry.clone() }, false);
    if live.exists() {
        let _ = fs::remove_file(&live);
    }
    Ok(())
}

/// 从容器里摘掉某一节（删/搬走一条 l1 daily 时用）。
///
/// 只摘一节、不动同一天的其它条目。删一条 daily 记忆绝不能把同一天的其它
/// 条目一起带走 —— 那是数据丢失，不是删除。
pub fn remove_section(entry: &MemoryEntry) -> Result<(), String> {
    let target = entry_path(&MemoryEntry { deleted_at: None, ..entry.clone() }, false);
    if !target.exists() || !is_daily_container(&rel(&target)) {
        return Ok(());
    }
    let text = fs::read_to_string(&target).map_err(|error| error.to_string())?;
    let (cleaned, removed) = strip_section(&text, &entry.id);
    if !removed {
        return Ok(());
    }
    // 摘完只剩容器头（没有任何 section）就把空容器删掉，免得 l1 目录里
    // 攒一堆「只有 frontmatter 的日期文件」
    let has_section = cleaned.lines().any(|line| parse_marker(line).is_some());
    if !has_section {
        let _ = fs::remove_file(&target);
        return Ok(());
    }
    write_atomic(&target, &cleaned)
}

/// 从事实源彻底移除一条记忆（purge 用；日常删除走回收站）。
///
/// 活形态与回收站两份都要清掉。活形态一律走 [`detach_live`]，因为「活形态」
/// 可能是容器里的一节，也可能是一个独立的 `.md` —— 只按 layer/kind 猜会漏掉
/// 老 v1 单文件，或者在容器里留下孤儿节。
pub fn remove_file(entry: &MemoryEntry) -> Result<(), String> {
    detach_live(entry)?;
    let trashed = entry_path(entry, true);
    if trashed.exists() {
        fs::remove_file(&trashed)
            .map_err(|error| format!("删除失败 {}：{error}", trashed.display()))?;
    }
    Ok(())
}

/// 把一个目录下的 `.md` 全读出来。读不出来的单个文件只记日志，不中断整轮
/// （一条手写坏了的文件不该让整页空白）。
fn read_dir_entries(dir: &Path, out: &mut Vec<MemoryEntry>) {
    let Ok(reader) = fs::read_dir(dir) else { return };
    let mut items: Vec<PathBuf> = reader.flatten().map(|item| item.path()).collect();
    items.sort();
    for path in items {
        if path.is_dir() {
            read_dir_entries(&path, out);
            continue;
        }
        if path.extension().and_then(|ext| ext.to_str()) != Some("md") {
            continue;
        }
        let Ok(text) = fs::read_to_string(&path) else {
            crate::server::logging::log("[Memory]", &format!("⚠️ 记忆文件读取失败：{}", path.display()));
            continue;
        };
        let relative = rel(&path);
        if is_daily_container(&relative) {
            read_container(&text, &relative, &path, out);
            continue;
        }
        let stem = path
            .file_stem()
            .and_then(|name| name.to_str())
            .unwrap_or("unknown")
            .to_string();
        out.push(MemoryEntry::from_markdown(&text, &stem, &relative));
    }
}

/// 容器 → 多条 l1 记忆。容器头的 `project` / `agent` 作为条目缺省值。
fn read_container(text: &str, relative: &str, path: &Path, out: &mut Vec<MemoryEntry>) {
    let date = relative
        .split('/')
        .next_back()
        .unwrap_or("unknown")
        .trim_end_matches(".md")
        .to_string();
    let (head, _) = crate::server::core::memory::model::split_frontmatter_public(text);
    let head_project = head.get("project").cloned().filter(|value| !value.is_empty());
    let head_agent = head.get("agent").cloned().filter(|value| !value.is_empty());
    for (index, (meta, body)) in split_sections(text).into_iter().enumerate() {
        let id = meta
            .get("id")
            .and_then(Value::as_str)
            .map(str::to_string)
            .unwrap_or_else(|| format!("{date}-{index}"));
        let mut entry = MemoryEntry::from_markdown(
            &format!("---\nid: {id}\n---\n\n{body}"),
            &id,
            "trash/placeholder",
        );
        // 上面那次转换只为拿到标题/正文的默认推断；真正的字段全部来自标记行
        entry.id = id;
        entry.path = relative.to_string();
        entry.layer = super::model::Layer::L1;
        entry.kind = MemoryKind::parse(
            meta.get("kind").and_then(Value::as_str).unwrap_or("daily"),
        );
        entry.project = head_project.clone();
        entry.agent = head_agent.clone();
        if let Some(title) = meta.get("title").and_then(Value::as_str) {
            if !title.trim().is_empty() {
                entry.title = title.trim().to_string();
            }
        }
        entry.tags = meta
            .get("tags")
            .and_then(Value::as_array)
            .map(|items| items.iter().filter_map(|item| item.as_str().map(str::to_string)).collect())
            .unwrap_or_default();
        entry.files = meta
            .get("files")
            .and_then(Value::as_array)
            .map(|items| items.iter().filter_map(|item| item.as_str().map(str::to_string)).collect())
            .unwrap_or_default();
        entry.supersedes = meta
            .get("supersedes")
            .and_then(Value::as_array)
            .map(|items| items.iter().filter_map(|item| item.as_str().map(str::to_string)).collect())
            .unwrap_or_default();
        entry.importance = meta
            .get("importance")
            .and_then(Value::as_f64)
            .unwrap_or(0.5)
            .clamp(0.0, 1.0);
        entry.pinned = meta.get("pinned").and_then(Value::as_bool).unwrap_or(false);
        entry.starred = meta.get("starred").and_then(Value::as_bool).unwrap_or(false);
        entry.created_at = meta.get("created").and_then(Value::as_i64).unwrap_or_else(now_ms);
        entry.updated_at = meta.get("updated").and_then(Value::as_i64).unwrap_or(entry.created_at);
        entry.valid_from = entry.created_at;
        entry.body = body;
        entry.hash = super::model::content_hash(&entry.body);
        // 容器文件名就是日期；条目自己的时间只精确到分钟
        let _ = path;
        out.push(entry);
    }
}

/// 读全部记忆（含回收站）。重建索引与列表页共用这一份事实读取。
pub fn load_all() -> Vec<MemoryEntry> {
    let mut out = Vec::new();
    read_dir_entries(&general_root(), &mut out);
    read_dir_entries(&projects_root(), &mut out);
    read_dir_entries(&profile_root(), &mut out);
    read_dir_entries(&trash_root(), &mut out);
    // 回收站里的文件以 `trash/` 为准：手改过 frontmatter 忘了写 deleted_at 时，
    // 「文件在 trash 里」这件事本身就说明它被删了。
    for entry in &mut out {
        if entry.deleted_at.is_none() && entry.path.starts_with("trash/") {
            entry.deleted_at = Some(entry.updated_at.max(1));
        }
        if entry.hash.is_empty() {
            entry.hash = super::model::content_hash(&entry.body);
        }
    }
    out.sort_by(|a, b| b.updated_at.cmp(&a.updated_at));
    out
}

// ── 项目登记表 ───────────────────────────────────────────────

/// 项目卡（界面上「项目」列表的数据）
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Project {
    pub slug: String,
    pub name: String,
    #[serde(default)]
    pub remote: Option<String>,
    #[serde(default)]
    pub path: Option<String>,
    #[serde(default)]
    pub created_at: i64,
    #[serde(default)]
    pub updated_at: i64,
    /// 远程别名（origin / upstream / …）
    #[serde(default)]
    pub remotes: Vec<String>,
    /// 用户手动合并到另一个 slug（别名 → 主项目）
    #[serde(default)]
    pub merged_into: Option<String>,
    /// 固定显示在列表顶部
    #[serde(default)]
    pub pinned: bool,
    /// 归档（不再接受新记忆，但仍可检索）
    #[serde(default)]
    pub archived: bool,
}

pub fn load_projects() -> Vec<Project> {
    let Ok(text) = fs::read_to_string(projects_file()) else {
        return Vec::new();
    };
    let Ok(value) = serde_json::from_str::<Value>(&text) else {
        return Vec::new();
    };
    value
        .get("projects")
        .and_then(|list| serde_json::from_value::<Vec<Project>>(list.clone()).ok())
        .unwrap_or_default()
}

pub fn save_projects(projects: &[Project]) -> Result<(), String> {
    ensure_layout()?;
    let payload = json!({ "projects": projects });
    let text = serde_json::to_string_pretty(&payload).map_err(|error| error.to_string())?;
    write_atomic(&projects_file(), &text)
}

/// 登记（或更新）一个项目。已存在则只补空缺字段 —— 用户手填的本地路径、
/// 归档标记不该被一次自动归类覆盖。
pub fn registry_upsert(slug: &str, name: &str, cwd: &str, remote: &str) -> Project {
    let slug = slugify(slug);
    let mut projects = load_projects();
    let now = now_ms();
    let name = if name.trim().is_empty() { slug.clone() } else { name.trim().to_string() };
    if let Some(existing) = projects.iter_mut().find(|item| item.slug == slug) {
        existing.updated_at = now;
        if existing.name.trim().is_empty() || existing.name == existing.slug {
            existing.name = name;
        }
        if existing.path.is_none() && !cwd.trim().is_empty() {
            existing.path = Some(cwd.to_string());
        }
        if existing.remote.is_none() && !remote.trim().is_empty() {
            existing.remote = Some(remote.to_string());
        }
        let cloned = existing.clone();
        let _ = save_projects(&projects);
        return cloned;
    }
    let project = Project {
        slug: slug.clone(),
        name,
        remote: (!remote.trim().is_empty()).then(|| remote.to_string()),
        path: (!cwd.trim().is_empty()).then(|| cwd.to_string()),
        created_at: now,
        updated_at: now,
        remotes: Vec::new(),
        merged_into: None,
        pinned: false,
        archived: false,
    };
    projects.push(project.clone());
    let _ = save_projects(&projects);
    project
}

/// 旧接口（`write` 里调）：把「用户/AI 说的项目名 + cwd」登记成项目卡。
pub fn touch_project(name: &str, cwd: Option<&str>) -> Project {
    let classified = match cwd {
        Some(cwd) if !cwd.trim().is_empty() => layout::classify_explicit(name, Some(cwd)),
        _ => layout::classify_explicit(name, None),
    };
    // `classify_explicit` 已经把结果登记过，这里只把它读回来（避免二次写盘）
    let slug = classified.slug.unwrap_or_else(|| slugify(name));
    load_projects()
        .into_iter()
        .find(|item| item.slug == slug)
        .unwrap_or_else(|| registry_upsert(&slug, name, cwd.unwrap_or_default(), ""))
}

// ── 设备身份 ─────────────────────────────────────────────────

/// 本机设备 id。第一次调用时生成并落盘（`device.json`）。
///
/// 为什么要它：同步回来的一堆记忆分不清「这台机器写的」与「那台机器写的」，
/// 冲突裁决就无从下手；「谁在什么时候改过这条」也答不出来。
pub fn device_id() -> String {
    static CACHE: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    CACHE
        .get_or_init(|| {
            if let Ok(text) = fs::read_to_string(device_file()) {
                if let Ok(value) = serde_json::from_str::<Value>(&text) {
                    if let Some(id) = value.get("id").and_then(Value::as_str) {
                        if !id.trim().is_empty() {
                            return id.to_string();
                        }
                    }
                }
            }
            let id = format!("{:08x}{:08x}", random_u32(), random_u32());
            let payload = json!({
                "id": id,
                "name": hostname(),
                "createdAt": now_ms(),
            });
            if ensure_layout().is_ok() {
                let text = serde_json::to_string_pretty(&payload).unwrap_or_default();
                let _ = write_atomic(&device_file(), &text);
            }
            id
        })
        .clone()
}

fn hostname() -> String {
    std::env::var("COMPUTERNAME")
        .or_else(|_| std::env::var("HOSTNAME"))
        .unwrap_or_else(|_| "unknown".to_string())
}

/// 进程内伪随机（不引 rand：设备 id 只需要「本机唯一」，不需要密码学强度）。
fn random_u32() -> u32 {
    use std::hash::{Hash, Hasher};
    static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    now_ms().hash(&mut hasher);
    std::process::id().hash(&mut hasher);
    COUNTER
        .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        .hash(&mut hasher);
    hasher.finish() as u32
}

// ── 回收站维护 ───────────────────────────────────────────────

/// 清掉超过保留天数的回收站文件。返回清掉的条数。
pub fn purge_expired_trash(days: usize) -> usize {
    let days = days.max(1) as i64;
    let cutoff = now_ms() - days * 24 * 3600 * 1000;
    let Ok(reader) = fs::read_dir(trash_root()) else { return 0 };
    let mut removed = 0usize;
    for item in reader.flatten() {
        let path = item.path();
        if path.extension().and_then(|ext| ext.to_str()) != Some("md") {
            continue;
        }
        // 用文件 mtime 而不是 frontmatter：文件被搬进回收站会刷新 mtime，
        // 而 `deleted_at` 可能被手改过。清回收站不需要那么精确。
        let stamp = fs::metadata(&path)
            .ok()
            .and_then(|meta| meta.modified().ok())
            .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|duration| duration.as_millis() as i64)
            .unwrap_or(0);
        if stamp > 0 && stamp < cutoff && fs::remove_file(&path).is_ok() {
            removed += 1;
        }
    }
    removed
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::server::core::memory::model::Layer;

    #[test]
    fn slug_is_path_safe() {
        assert_eq!(slugify("Agent2API"), "agent2api");
        assert_eq!(slugify("a/b c"), "a-b-c");
        assert_eq!(slugify("中文项目"), GLOBAL_SLUG);
        assert_eq!(slugify("  "), GLOBAL_SLUG);
    }

    #[test]
    fn entry_path_follows_layer_layout() {
        let mut entry = MemoryEntry::draft("m1", "t", MemoryKind::Knowledge, "body");
        entry.project = Some("Agent2API".into());
        entry.layer = Layer::L2;
        let path = rel(&entry_path(&entry, false));
        assert_eq!(path, "projects/agent2api/l2/knowledge/m1.md");

        let mut daily = MemoryEntry::draft("d1", "t", MemoryKind::Daily, "body");
        daily.agent = Some("workbuddy".into());
        daily.created_at = 1_700_000_000_000;
        let path = rel(&entry_path(&daily, false));
        assert!(path.starts_with("general/l1/workbuddy/"), "实际 {path}");
        assert!(path.ends_with(".md"));

        let trashed = rel(&entry_path(&entry, true));
        assert_eq!(trashed, "trash/m1.md");
    }

    #[test]
    fn daily_container_is_recognised_by_shape() {
        assert!(is_daily_container("general/l1/workbuddy/2026-11-16.md"));
        assert!(is_daily_container("projects/o--r/l1/manual/2026-11-16.md"));
        // 同目录下的单条文件（非 daily 的 l1 条目）不是容器
        assert!(!is_daily_container("projects/o--r/l1/manual/2026-11-16-id.md"));
        assert!(!is_daily_container("projects/o--r/l2/decisions/2026-11-16.md"));
    }

    #[test]
    fn container_sections_round_trip() {
        let mut first = MemoryEntry::draft("a1", "第一条", MemoryKind::Daily, "正文一");
        first.agent = Some("workbuddy".into());
        first.tags = vec!["标签,带逗号".into()];
        let mut second = MemoryEntry::draft("a2", "第二条", MemoryKind::Daily, "正文二");
        second.agent = Some("workbuddy".into());

        let mut text = container_head("2026-11-16", "workbuddy", None);
        text.push_str(&section_text(&first));
        text.push_str(&section_text(&second));
        let sections = split_sections(&text);
        assert_eq!(sections.len(), 2);
        assert_eq!(sections[0].0.get("id").and_then(Value::as_str), Some("a1"));
        assert_eq!(sections[0].1.trim(), "正文一");
        assert_eq!(sections[1].1.trim(), "正文二");
    }

    #[test]
    fn strip_section_removes_only_target() {
        let first = MemoryEntry::draft("a1", "第一条", MemoryKind::Daily, "正文一");
        let second = MemoryEntry::draft("a2", "第二条", MemoryKind::Daily, "正文二");
        let mut text = container_head("2026-11-16", "manual", None);
        text.push_str(&section_text(&first));
        text.push_str(&section_text(&second));
        let (cleaned, removed) = strip_section(&text, "a1");
        assert!(removed);
        assert!(!cleaned.contains("正文一"));
        assert!(cleaned.contains("正文二"));
    }

    #[test]
    fn marker_with_awkward_tags_parses_back() {
        let mut entry = MemoryEntry::draft("x1", "标题 里 有 空格", MemoryKind::Daily, "b");
        entry.tags = vec!["含,逗号".into(), "含\"引号\"".into()];
        let line = section_marker(&entry);
        let meta = parse_marker(line.trim_end()).expect("能解析");
        assert_eq!(meta.get("title").and_then(Value::as_str), Some("标题 里 有 空格"));
        assert_eq!(meta.get("tags").and_then(Value::as_array).map(Vec::len), Some(2));
    }

    #[test]
    fn device_id_is_stable_within_process() {
        let first = device_id();
        let second = device_id();
        assert_eq!(first, second);
        assert_eq!(first.len(), 16);
    }
}