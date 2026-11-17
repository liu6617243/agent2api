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

/// 记忆事实源的**进程内写锁**。
///
/// ── 为什么必须有 ────────────────────────────────────────────
/// [`save_section_locked`] 与 [`remove_section_locked`] 都是「读整份容器 → 摘/改一节 → 整份
/// 写回」。两个写者落在**同一天、同一 agent** 的容器上时，后写的整份覆盖先写的，
/// 先写那一节静默消失 —— 文件层不会报任何错。并发来源至少三路：
///   · 多个客户端各自的 `tools/call`（`mcp.rs` 每调用一次丢一个 spawn_blocking）；
///   · 面板 `/api/memory/write`（直接在 tokio worker 上跑，连 spawn_blocking 都没包）；
///   · 调度器十二个任务（distill / tag / consolidate / cleanup …）。
/// 这三路都写同一个 `l1/<agent>/<YYYY-MM-DD>.md`。
///
/// ── 为什么是进程内 Mutex 而不是文件锁 ───────────────────────
/// 跨进程才需要 flock；事实源只由网关这一个进程写（用户手编 `.md` 属于「外力
/// 改动」，模块头已写明「下次启动 / 下次重建生效」，不在本锁的承诺范围内）。
/// 一把 `Mutex` 足够，也没有 Windows 上 flock 语义不一致的坑。
///
/// ── 附带修掉的一个更硬的坑：临时文件名只带 pid ──────────────
/// [`write_atomic`] 的临时文件叫 `<目标>.tmp.<pid>`。同一进程里两个线程同时
/// 写**同一份文件**时，它们拿到的是**同一个临时路径**：一个刚 create 完，另一
/// 个就把它截断；随后先完成的一方把 tmp rename 走，后完成的一方 rename 时
/// 文件已经不在了 —— 表现成随机的「落盘失败」或「内容串了」。锁把这条路径也
/// 一并串行化，所以不必把 pid 换成「pid + 线程 id + 计数器」。
///
/// ── 为什么锁在外层入口，而不是里面的每一处 `write_atomic` ────
/// `save` 内部会调 `detach_live` → `remove_section`，是天然嵌套的。std 的
/// `Mutex` 不可重入，若让 `save_section` / `remove_section` 各自加锁就是自锁死。
/// 所以这里采取「`*_locked` 内部版本不带锁 + 公开入口统一取锁」：
///            `save` ──► `save_locked` ──► `detach_live_locked` ──► `remove_section_locked`
///            `detach_live` ──► `detach_live_locked`
///            `remove_section` / `remove_file` ──► 各自的 `_locked`
///            `move_save` / `retire` / `revive` ──► 一次临界区里做两件事
/// 一次事务（比如「摘旧节 + 写回收站」）在**同一把锁内**完成，中途不会被插队。
static WRITE_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// 取写锁。panic 中毒只说明「当时那一节没写完」，文件因为原子写仍是一致的，
/// 不该让全库永久写不进 —— 直接取回内部值。
pub(crate) fn lock_writes() -> std::sync::MutexGuard<'static, ()> {
    WRITE_LOCK.lock().unwrap_or_else(|poison| poison.into_inner())
}

/// `projects.json` 的读改写锁（与 [`WRITE_LOCK`] 分开）。
///
/// 分类登记（`registry_upsert`）在**每一次写入**之前都会跑一遍：读整份登记表 →
/// 补一条 → 整份写回。两个并发写者各补一个不同的项目时，后写的整份覆盖先写的，
/// 先登记的那张项目卡凭空消失 —— 与容器丢节是同一类 bug，且触发的频率更高
/// （不是「同一天同 agent」，而是「同一时刻的任意两次写入」）。
///
/// ── 为什么不用同一把锁 ──────────────────────────────────────
/// `merge_project` 会「一边持锁遍历、一边逐条 `store::save`」，而 `store::save`
/// 内部要 [`lock_writes`]。std 的 `Mutex` 不可重入，用同一把锁就是自锁死。
/// 两把锁的**获取顺序固定为：先 PROJECTS 后 WRITE**（`merge_project` 之外的
/// 路径都是先登记、后落盘，天然同序），不存在反向持有，因此不会死锁。
static PROJECTS_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

pub(crate) fn lock_projects() -> std::sync::MutexGuard<'static, ()> {
    PROJECTS_LOCK.lock().unwrap_or_else(|poison| poison.into_inner())
}

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

/// 配置文件本身的路径：**固定**在默认目录下，绝不调用 [`root()`]。
///
/// 为什么不能写成 `root().join("config.json")`：`root()` 要读 `storage.root`，
/// 读配置要先进 `config::stored()`，而 `config::stored()` 又要 `config_file()`
/// 才能知道去哪读 —— 三者首尾相接，形成无穷递归。在 test 线程（默认 2MB 栈）
/// 上表现为 `stack overflow, aborting`，在主线程上是启动即崩。
///
/// 「决定根目录在哪」这份配置只能放在一个不依赖根目录的地方，这是唯一解。
pub fn config_file() -> PathBuf {
    crate::server::config::config_dir()
        .join("memory")
        .join("config.json")
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
///
/// **调用方必须已持有 [`lock_writes`]** —— 整段是「读容器 → 摘同 id 旧节 →
/// 追加新节 → 整份写回」，中途被另一个写者插进来，先写那一节会被这一份整份
/// 覆盖（静默丢数据）。加锁责任在公开入口，是为了让「摘 + 写」这类组合动作
/// 能在一个临界区里做完（见 [`save`]）。
fn save_section_locked(entry: &MemoryEntry) -> Result<(), String> {
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
/// 上一版对象，猜不出来。要「摘旧 + 写新」在同一个临界区完成，用 [`move_save`]。
///
/// 本函数仍会把 `deleted_at` 非空的条目送进回收站（同步、导入这类手里只有
/// 「最终形态」的路径要用），但**删除业务走 [`retire`]**：只有 `retire` 手里
/// 才有改动前的对象，能按正确的活路径把旧那份摘干净。
pub fn save(entry: &MemoryEntry) -> Result<(), String> {
    ensure_layout()?;
    let _guard = lock_writes();
    save_locked(entry)
}

fn save_locked(entry: &MemoryEntry) -> Result<(), String> {
    if entry.deleted_at.is_some() {
        // 进回收站前必须从原位置拿走，否则「同一个 id 两份文件」重建时会打架
        detach_live_locked(entry)?;
        return write_atomic(&trash_root().join(format!("{}.md", entry.id)), &entry.to_markdown());
    }
    if live_is_container(entry) {
        return save_section_locked(entry);
    }
    write_atomic(&entry_path(entry, false), &entry.to_markdown())
}

/// 把某个 id 从「活着的形态」里摘掉（容器摘一节 / 单条删文件），不动回收站。
///
/// 调用方负责之后的落盘。抽出来是因为三条路径（delete / 改类型换目录 / purge）
/// 都要做同一件事，而少做一次的后果都是「同一条记忆出现两份」。
pub fn detach_live(entry: &MemoryEntry) -> Result<(), String> {
    let _guard = lock_writes();
    detach_live_locked(entry)
}

fn detach_live_locked(entry: &MemoryEntry) -> Result<(), String> {
    let live = entry_path(&MemoryEntry { deleted_at: None, ..entry.clone() }, false);
    // ── 判据只看「这个落点是什么形状的文件」
    //
    // 不能写成 `live_is_container(entry)`：它把 `layer`/`kind` 也纳入了判断，
    // 于是结论取决于**调用方传进来的是哪一版对象**。当前所有调用方都传了与
    // `live` 匹配的那一版（`move_save` 传 `previous`、`retire` 传改动前的
    // 对象），所以没出问题；但这是个只靠调用纪律维持的隐式契约 —— 哪天有人
    // 顺手把改完的 `entry` 传进来，daily → knowledge 会让判据返回 false，落到
    // 下面「删单文件」分支，而 `live` 解析出来正是当天的容器：一次
    // `remove_file` 就把同一天其它所有条目一起删了，且不报错。
    // 收紧成「只看路径形状」之后，这个坑从根上没了。
    //
    // 这里用**绝对路径**判，不绕 `rel()`/`abs()` 一趟：那趟往返要求
    // `storage.root` 与 `path` 的字面完全一致（`strip_prefix` 按组件逐字比），
    // 用户把根目录写成不同大小写时 `rel()` 会回落成路径串、`abs()` 再拼一次
    // 就得到一个不存在的路径，摘节被静默跳过 —— 旧容器里留下删不掉的孤儿节。
    if looks_like_daily_container_path(&live) {
        return strip_section_at(&live, &entry.id);
    }
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
    let _guard = lock_writes();
    remove_section_locked(entry)
}

/// 与 [`save_section_locked`] 同一种读改写：摘节也必须独占整份容器，
/// 否则并发的 `save_section_locked` 会把刚摘掉的那一节又整份写回来（或者反过来）。
fn remove_section_locked(entry: &MemoryEntry) -> Result<(), String> {
    let target = entry_path(&MemoryEntry { deleted_at: None, ..entry.clone() }, false);
    strip_section_at(&target, &entry.id)
}

/// 按**绝对路径**从容器里摘掉某一节。不经过 `rel()`/`abs()` 往返。
///
/// 拆出来是因为两条路径算出来的「落点」可能不一样：`detach_live_locked` 要摘的是
/// **改动之前**那一份容器，而它手里的 `entry.path` 已经指向改完之后的新位置。
/// 让它复用 `remove_section_locked` 就只能靠临时改 `path` 再算一遍，既绕又容易
/// 在 `storage.root` 大小写不一致时算出不存在的路径。这里直接收绝对路径。
fn strip_section_at(target: &Path, id: &str) -> Result<(), String> {
    if !target.exists() || !looks_like_daily_container_path(target) {
        return Ok(());
    }
    let text = fs::read_to_string(target).map_err(|error| error.to_string())?;
    let (cleaned, removed) = strip_section(&text, id);
    if !removed {
        return Ok(());
    }
    // 摘完只剩容器头（没有任何 section）就把空容器删掉，免得 l1 目录里
    // 攒一堆「只有 frontmatter 的日期文件」
    let has_section = cleaned.lines().any(|line| parse_marker(line).is_some());
    if !has_section {
        let _ = fs::remove_file(target);
        return Ok(());
    }
    write_atomic(target, &cleaned)
}

/// 绝对路径版的「是不是一天一文件的 l1 容器」。
///
/// 与 [`is_daily_container`] 同一套判据，只是不要求先算成相对路径 —— 这样
/// 「按形状判容器」在 `storage.root` 大小写/分隔符与配置不一致时也不会失配。
fn looks_like_daily_container_path(path: &Path) -> bool {
    if path.extension().and_then(|ext| ext.to_str()) != Some("md") {
        return false;
    }
    let Some(stem) = path.file_stem().and_then(|name| name.to_str()) else {
        return false;
    };
    if !looks_like_date(stem) {
        return false;
    }
    // 形状固定是 `<base>/l1/<agent>/<YYYY-MM-DD>.md`：祖目录必须正好是 `l1`。
    // 不能写成「路径里任意一段叫 l1」—— 记忆根恰好放在名叫 `l1` 的盘目录下时
    // 会把根下所有日期命名的文件都当成容器。
    path.parent()
        .and_then(Path::parent)
        .and_then(Path::file_name)
        .and_then(|name| name.to_str())
        .map(|name| name == "l1")
        .unwrap_or(false)
}

/// 从事实源彻底移除一条记忆（purge 用；日常删除走回收站）。
///
/// 活形态与回收站两份都要清掉。活形态一律走 `detach_live_locked`，因为「活形态」
/// 可能是容器里的一节，也可能是一个独立的 `.md` —— 只按 layer/kind 猜会漏掉
/// 老 v1 单文件，或者在容器里留下孤儿节。
///
/// 整段一个临界区：分成「先摘活形态、再删回收站」两次加锁的话，中间插进来的
/// 并发 `save` 会让刚 purge 的条目又被写出来。
pub fn remove_file(entry: &MemoryEntry) -> Result<(), String> {
    let _guard = lock_writes();
    detach_live_locked(entry)?;
    let trashed = entry_path(entry, true);
    if trashed.exists() {
        fs::remove_file(&trashed)
            .map_err(|error| format!("删除失败 {}：{error}", trashed.display()))?;
    }
    Ok(())
}

/// 换位保存：先把**旧位置**的那一份摘掉，再写到新位置，整段一个临界区。
///
/// ── 为什么两个动作必须在一起 ────────────────────────────────
/// 改层 / 改类型 / 改日期 / 改项目都会换目录。调用方拿到的是旧 `MemoryEntry`，
/// 先 `detach_live`、再 `save` 是两次加锁：中间被并发的 `save`（同一个 id，
/// 同一个新路径）插进来，就会写出两份同 id 的文件 —— 下次重建索引时后者覆盖
/// 前者、删除时只删掉一份，另一份变成删不掉的幽灵。
///
/// `previous` 是**改动前**的对象（按它算旧路径），`current` 是改完的。
/// 两条路径算出来一样时只写不摘 —— 容器里的节原地更新由 `save` 自己处理。
pub fn move_save(previous: &MemoryEntry, current: &MemoryEntry) -> Result<(), String> {
    ensure_layout()?;
    let _guard = lock_writes();
    let old = entry_path(&MemoryEntry { deleted_at: None, ..previous.clone() }, false);
    let new = entry_path(&MemoryEntry { deleted_at: None, ..current.clone() }, false);
    if old != new {
        detach_live_locked(previous)?;
    }
    save_locked(current)
}

/// 退休：把一条活着的记忆「摘离活形态 + 落进回收站」，整段一个临界区。
///
/// ── 为什么不能直接用 `save(current)` ────────────────────────
/// [`save_locked`] 见到 `deleted_at` 非空时确实会走「摘活形态 + 写 trash/」，
/// 但它摘活形态用的判据是**这一份对象自己**的路径。删除路径里 `entry.path` 已经
/// 被改写成 `trash/<id>.md` 了：`entry_path` 见到 `trash/` 开头的相对路径会跳过
/// 「按 `entry.path` 定落点」，转而按 layer/kind 重算 —— 对 v1 那种「独立单文件」
/// 的 l1 记忆，重算出来是容器路径，原文件既没删掉、又写了一份进回收站。
///
/// 所以必须把**改动前**的对象（`previous`）一起传进来：摘活形态按它算，写回收站
/// 按 `current` 算。两件事在同一个临界区，中间插不进来并发 `save`，否则会出现
/// 「活的 + 回收站」两份同 id。
pub fn retire(previous: &MemoryEntry, current: &MemoryEntry) -> Result<(), String> {
    ensure_layout()?;
    let _guard = lock_writes();
    detach_live_locked(previous)?;
    write_atomic(&trash_root().join(format!("{}.md", current.id)), &current.to_markdown())
}

/// 复活：把回收站里的一条写回活位置，再把回收站那份删掉，整段一个临界区。
///
/// 与 [`retire`] 对称。分成「save 一次 + remove_file 一次」两步的话，中间那一瞬
/// 库里同时存在活的与回收站两份同 id：并发的 `restore` 会看到「已经活了」直接
/// 返回、并发 `purge` 会把刚写出来的活文件一起删掉。收进一个临界区就没这问题。
pub fn revive(trashed: &Path, current: &MemoryEntry) -> Result<(), String> {
    ensure_layout()?;
    let _guard = lock_writes();
    save_locked(current)?;
    if trashed.exists() {
        fs::remove_file(trashed)
            .map_err(|error| format!("清理回收站失败 {}：{error}", trashed.display()))?;
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
    // 读整份登记表 → 补一条 → 整份写回。并发写不同项目时后者会让前者凭空消失，
    // 所以整段持锁。注意这里**不能**再调 `store::save`（不会，只写 projects.json），
    // 因此与 [`WRITE_LOCK`] 没有嵌套。
    let _guard = lock_projects();
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
    // 与 `save` 里「进回收站」那一步互斥：定时清理正删这一份时，
    // 并发的 `save(deleted_at)` 可能刚把它写出来，两边都以为处理完了。
    let _guard = lock_writes();
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

    /// 回归：`config_file()` 曾经写成 `root().join("config.json")`，而 `root()`
    /// 又要读 `storage.root` 配置，读配置又要先知道配置文件在哪 —— 三个函数首尾
    /// 相扣，`config::stored()` 一调就是无穷递归。测试线程栈只有 2MB，表现成
    /// `stack overflow, aborting`；主线程上是启动即崩。
    ///
    /// 这个测试的意义不在断言值，而在**它能在不爆栈的前提下跑完**：一旦有人
    /// 把 `config_file()` 改回依赖 `root()`，这里立刻复现那个环。
    #[test]
    fn config_path_does_not_depend_on_itself() {
        let path = config_file();
        assert!(path.ends_with("config.json"), "实际 {}", path.display());
        // 读一次配置（`stored()` 内部会去 stat 这个路径）必须能返回
        let verbose = crate::server::core::memory::config::bool_of("search.synonymsEnabled");
        let _ = verbose;
    }

    /// 并发写同一个当天容器时，每一节都必须留下来。
    ///
    /// 这是 [`lock_writes`] 的回归测试：没有锁时 `save_section_locked` 的
    /// 「读整份 → 摘 → 追加 → 整份写回」会互相覆盖，最后容器里只剩一节的
    /// 内容（而且**不会报任何错**）。二十个线程各写一条，断言条数正好二十。
    ///
    /// ── 为什么必须包在 `isolated` 里 ────────────────────────────
    /// `cfg(test)` 下 `paths::config_dir()` 取的是 **thread_local** 临时目录
    /// （`test_support::HOME`）。直接 `spawn` 二十个线程，每个线程拿到的是
    /// **各自的**目录，二十条落到二十个文件里，测试永远「通过」——它测不到
    /// 任何并发。`isolated` 让测试跑在一个子进程里、由 `AGENT2API_PROXY_HOME`
    /// 指定目录，`config_dir()` 于是走进程级路径，二十个线程才真的抢同一份文件。
    ///
    /// 为什么用 20 而不是 2：两个线程撞上的概率不高，20 个足以让覆盖稳定复现
    /// （无锁版本跑十次失败十次）。
    #[test]
    fn concurrent_writes_keep_every_section() {
        crate::test_support::isolated(
            concat!(
                module_path!(),
                "::concurrent_writes_keep_every_section"
            ),
            || {
                // 固定 agent + 固定日期 → 所有条目落到同一个 `l1/<agent>/<date>.md`
                let agent = "conc".to_string();
                let stamp = 1_700_000_000_000_i64;
                let mut handles = Vec::new();
                for index in 0..20 {
                    let agent = agent.clone();
                    handles.push(std::thread::spawn(move || {
                        let mut entry = MemoryEntry::draft(
                            format!("{agent}-{index}"),
                            format!("第 {index} 条"),
                            MemoryKind::Daily,
                            format!("并发正文 {index}"),
                        );
                        entry.agent = Some(agent);
                        entry.created_at = stamp;
                        entry.updated_at = stamp;
                        save(&entry).expect("写入应当成功");
                    }));
                }
                for handle in handles {
                    handle.join().expect("线程不应 panic");
                }

                let mut probe = MemoryEntry::draft("probe", "探针", MemoryKind::Daily, "x");
                probe.agent = Some(agent.clone());
                probe.created_at = stamp;
                // `updated_at` 必须一起钉住：`entry_path` 用的是
                // `created_at.max(updated_at)`，而 `draft` 会把 updated_at 设成
                // 「现在」。只钉 created_at 的话，20 条写入落在 stamp 那天（2023），
                // 探针却指向今天 —— 断言「容器文件存在」永远失败，且看上去像
                // 并发写把文件写没了。
                probe.updated_at = stamp;
                let path = entry_path(&probe, false);
                let text = fs::read_to_string(&path).expect("容器文件应当存在");
                let sections = text.lines().filter(|line| parse_marker(line).is_some()).count();
                assert_eq!(sections, 20, "并发写丢了节：实际 {sections} 节，文件 {}", path.display());
            },
        );
    }

    /// 把一条 daily 改成别的类型时，**同一天的其它条目必须还在**。
    ///
    /// 这是 [`detach_live_locked`] 判据的回归测试。旧实现用
    /// `live_is_container(entry)` 判「要不要按容器摘节」，而它看的是**这一份对象
    /// 自己的** `layer`/`kind`。改类型时调用方已经把 `kind` 换成新值（daily →
    /// knowledge），判据返回 false，于是落到「删单文件」分支：`entry_path` 算出来
    /// 正是当天的容器，`remove_file` 把整份容器删掉 —— 同一天其他条目一起没了，
    /// 而且不报任何错。改成按**路径形状**判之后不会认错。
    #[test]
    fn type_change_keeps_siblings_in_the_container() {
        crate::test_support::isolated(
            concat!(module_path!(), "::type_change_keeps_siblings_in_the_container"),
            || {
                let agent = "mover".to_string();
                let stamp = 1_700_000_000_000_i64;
                let seed = |id: &str, title: &str| {
                    let mut entry = MemoryEntry::draft(id, title, MemoryKind::Daily, "正文");
                    entry.agent = Some(agent.clone());
                    entry.created_at = stamp;
                    entry.updated_at = stamp;
                    save(&entry).expect("写入应当成功");
                    entry
                };
                let staying = seed("stays", "留下的一条");
                let mut moving = seed("moves", "要搬走的一条");

                let container = entry_path(&staying, false);
                let before = fs::read_to_string(&container).expect("容器应当存在");
                assert_eq!(
                    before.lines().filter(|line| parse_marker(line).is_some()).count(),
                    2,
                    "前置条件：容器里应当有两节"
                );

                // 模拟「改类型」：kind 换成 knowledge（会落到 L2 的独立文件），
                // 但磁盘上那一份仍然是当天的容器
                let previous = moving.clone();
                moving.kind = MemoryKind::Knowledge;
                moving.layer = Layer::L2;
                moving.path.clear();
                moving.path = rel(&entry_path(&moving, false));
                move_save(&previous, &moving).expect("换位保存应当成功");

                assert!(
                    fs::read_to_string(&container).is_ok(),
                    "容器被整份删掉了：{} —— 同一天的其它条目一起丢了",
                    container.display()
                );
                let after = fs::read_to_string(&container).expect("容器应当还在");
                let sections: Vec<String> = after
                    .lines()
                    .filter_map(parse_marker)
                    .filter_map(|meta| meta.get("id").and_then(Value::as_str).map(str::to_string))
                    .collect();
                assert_eq!(sections, vec!["stays".to_string()], "留下的应当只有 stays：{sections:?}");
                assert!(entry_path(&moving, false).is_file(), "新位置应当写出来了");
            },
        );
    }
}
