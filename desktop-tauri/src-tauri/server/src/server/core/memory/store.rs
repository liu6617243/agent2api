//! 记忆中枢 · 文件层（事实源的读写）。
//!
//! ── 目录布局 ────────────────────────────────────────────────
//! ```text
//! {config_dir}/memory/
//!   config.json          注入开关 / 预算等项目设置
//!   projects.json        项目索引（slug → 名称 / 远程 / 本地路径）
//!   index.db             派生索引（可随时删除重建）
//!   projects/<slug>/<id>.md     一条记忆一个文件
//!   projects/_global/<id>.md    不属于任何项目的记忆
//!   trash/<id>.md        回收站（文件搬到这里，可按 id 恢复）
//! ```
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

use super::model::MemoryEntry;

/// 不属于任何项目的记忆，落到这个伪项目下（避免「没有 project 就没地方放」）
pub const GLOBAL_SLUG: &str = "_global";

fn now_ms() -> i64 {
    crate::server::logging::now_ms()
}

/// memory 根目录。与网关统一库同源（`config::config_dir()`），
/// 换 `AGENT2API_PROXY_HOME` 时一起搬。
pub fn root() -> PathBuf {
    crate::server::config::config_dir().join("memory")
}

pub fn projects_root() -> PathBuf {
    root().join("projects")
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

/// 建齐目录。幂等，每次访问前调用（允许用户中途手工删掉某个子目录）。
pub fn ensure_layout() -> Result<(), String> {
    for dir in [root(), projects_root(), trash_root()] {
        fs::create_dir_all(&dir).map_err(|error| format!("创建记忆目录失败 {}：{error}", dir.display()))?;
    }
    Ok(())
}

/// 项目 slug：只保留小写字母、数字、`-`、`_`、`.`；连续非法字符压成一个 `-`。
///
/// 为什么不用项目名直接当目录名：项目名可能带空格、中文、斜杠（`a/b` 会造出
/// 两级目录），而 slug 同时是接口参数与前端路由片段，必须是路径安全的。
pub fn slugify(name: &str) -> String {
    let mut out = String::new();
    let mut last_dash = false;
    for ch in name.trim().chars() {
        let lower = ch.to_ascii_lowercase();
        if lower.is_ascii_alphanumeric() || lower == '_' || lower == '.' {
            out.push(lower);
            last_dash = false;
        } else if !last_dash {
            out.push('-');
            last_dash = true;
        }
    }
    let trimmed = out.trim_matches('-').to_string();
    if trimmed.is_empty() {
        GLOBAL_SLUG.to_string()
    } else {
        super::model::truncate_chars(&trimmed, 48)
    }
}

/// 一条记忆相对 memory 根的路径。`deleted` 为真时落 `trash/`。
pub fn entry_path(entry: &MemoryEntry, deleted: bool) -> PathBuf {
    let name = format!("{}.md", entry.id);
    if deleted {
        return trash_root().join(name);
    }
    let slug = entry
        .project
        .as_deref()
        .map(slugify)
        .unwrap_or_else(|| GLOBAL_SLUG.to_string());
    projects_root().join(slug).join(name)
}

/// 原子写：同目录 tmp → 写满 → fsync → 删旧 → rename。
pub fn write_atomic(path: &Path, contents: &str) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|error| format!("创建目录失败 {}：{error}", parent.display()))?;
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

/// 把一条记忆写回事实源（自动落在 `projects/<slug>/` 或 `trash/`）。
pub fn save(entry: &MemoryEntry) -> Result<(), String> {
    ensure_layout()?;
    let target = entry_path(entry, entry.deleted_at.is_some());
    write_atomic(&target, &entry.to_markdown())
}

/// 从事实源移除文件（用于「彻底删除」；日常删除走回收站）。
pub fn remove_file(entry: &MemoryEntry) -> Result<(), String> {
    for path in [entry_path(entry, false), entry_path(entry, true)] {
        if path.exists() {
            fs::remove_file(&path).map_err(|error| format!("删除失败 {}：{error}", path.display()))?;
        }
    }
    Ok(())
}

/// 把一个目录下的 `.md` 全读出来。读不出来的单个文件只记日志，不中断整轮
/// （一条手写坏了的文件不该让整页空白）。
fn read_dir_entries(dir: &Path, out: &mut Vec<MemoryEntry>) {
    let Ok(reader) = fs::read_dir(dir) else { return };
    for item in reader.flatten() {
        let path = item.path();
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
        let stem = path
            .file_stem()
            .and_then(|name| name.to_str())
            .unwrap_or("unknown")
            .to_string();
        let relative = path
            .strip_prefix(root())
            .map(|rest| rest.to_string_lossy().replace('\\', "/"))
            .unwrap_or_else(|_| stem.clone());
        out.push(MemoryEntry::from_markdown(&text, &stem, &relative));
    }
}

/// 读全部记忆（含回收站）。重建索引与列表页共用这一份事实读取。
pub fn load_all() -> Vec<MemoryEntry> {
    let mut out = Vec::new();
    read_dir_entries(&projects_root(), &mut out);
    read_dir_entries(&trash_root(), &mut out);
    // 回收站里的文件以 `trash/` 为准：手改过 frontmatter 忘了写 deleted_at 时，
    // 「文件在 trash 里」这件事本身就说明它被删了。
    for entry in &mut out {
        if entry.deleted_at.is_none() && entry.path.starts_with("trash/") {
            entry.deleted_at = Some(entry.updated_at.max(1));
        }
    }
    out.sort_by(|a, b| b.updated_at.cmp(&a.updated_at));
    out
}

// ── 项目索引 ─────────────────────────────────────────────────

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
}

pub fn load_projects() -> Vec<Project> {
    let Ok(text) = fs::read_to_string(projects_file()) else {
        return Vec::new();
    };
    let Ok(value) = serde_json::from_str::<serde_json::Value>(&text) else {
        return Vec::new();
    };
    value
        .get("projects")
        .and_then(|list| serde_json::from_value::<Vec<Project>>(list.clone()).ok())
        .unwrap_or_default()
}

pub fn save_projects(projects: &[Project]) -> Result<(), String> {
    ensure_layout()?;
    let payload = serde_json::json!({ "projects": projects });
    let text = serde_json::to_string_pretty(&payload).map_err(|error| error.to_string())?;
    write_atomic(&projects_file(), &text)
}

/// 登记（或更新）一个项目。`cwd` 存在时顺手补上本地路径与 git 远程
/// ——「项目卡上只有名字、没有出处」在排障时几乎没用。
pub fn touch_project(name: &str, cwd: Option<&str>) -> Project {
    let slug = slugify(name);
    let mut projects = load_projects();
    let now = now_ms();
    if let Some(existing) = projects.iter_mut().find(|item| item.slug == slug) {
        existing.updated_at = now;
        if existing.path.is_none() {
            existing.path = cwd.map(|value| value.to_string());
        }
        let cloned = existing.clone();
        let _ = save_projects(&projects);
        return cloned;
    }
    let project = Project {
        slug: slug.clone(),
        name: name.trim().to_string(),
        remote: cwd.and_then(git_remote),
        path: cwd.map(|value| value.to_string()),
        created_at: now,
        updated_at: now,
    };
    projects.push(project.clone());
    let _ = save_projects(&projects);
    project
}

/// 从工作目录读 git 远程地址。读不到就返回 `None`（不是错误：
/// 项目完全可能还没初始化仓库）。
fn git_remote(cwd: &str) -> Option<String> {
    let output = std::process::Command::new("git")
        .args(["-C", cwd, "config", "--get", "remote.origin.url"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if text.is_empty() {
        None
    } else {
        Some(text)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slug_is_path_safe() {
        assert_eq!(slugify("Agent2API"), "agent2api");
        assert_eq!(slugify("a/b c"), "a-b-c");
        assert_eq!(slugify("中文项目"), GLOBAL_SLUG);
        assert_eq!(slugify("  "), GLOBAL_SLUG);
    }

    #[test]
    fn entry_path_uses_project_slug() {
        let entry = MemoryEntry {
            id: "m1".into(),
            title: "t".into(),
            kind: super::super::model::MemoryKind::Daily,
            project: Some("Agent2API".into()),
            agent: None,
            tags: vec![],
            importance: 0.5,
            files: vec![],
            supersedes: vec![],
            pinned: false,
            starred: false,
            created_at: 1,
            updated_at: 1,
            valid_from: 1,
            superseded_by: None,
            deleted_at: None,
            body: String::new(),
            path: String::new(),
        };
        let path = entry_path(&entry, false);
        assert!(path.ends_with("projects/agent2api/m1.md"), "实际 {}", path.display());
        let trashed = entry_path(&entry, true);
        assert!(trashed.ends_with("trash/m1.md"));
    }
}
