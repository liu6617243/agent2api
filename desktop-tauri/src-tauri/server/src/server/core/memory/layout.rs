//! 记忆中枢 · 目录布局与项目归类引擎。
//!
//! ── 为什么项目身份由 Git 远程决定，而不是目录名 ──────────────
//! 同一件事在两台机器上的目录名几乎一定不同（`~/src/agent2api` 与
//! `D:\work\Agent2API-main`），而**远程地址**是跨机唯一的稳定标识。所以 slug
//! 从 `<host>/<owner>/<repo>` 派生：换机器、换目录名、同步到另一台电脑，记忆都
//! 落到同一个项目下。没有 git 的项目退回目录名，再退回 `_global`。
//!
//! ── 为什么 slug 要把 host 也编进去 ───────────────────────────
//! `owner--repo` 在 github / gitee / gitlab 之间会撞（同名 fork 极常见）。
//! 已知的四家托管站（github / gitlab / gitee / bitbucket）省掉 host —— 它们的
//! 命名空间事实上不重叠，省下来的是可读性；其余站点一律带上 host。
//!
//! ── 模糊匹配为什么进「待确认」而不是直接归类 ─────────────────
//! 归类错了比不归类更糟：记忆会静默流进另一个项目，用户在一个项目里再也搜不到
//! 它，而界面上看起来「什么都没发生」。所以阈值之下只产出**建议**，人工点头
//! 才落盘（`classify.autoCreateProject` 默认关，同一理由）。

use std::collections::HashMap;
use std::sync::Mutex;
use std::sync::OnceLock;

use serde_json::{json, Value};

use super::config;
use super::model::{l2_dir_for, Layer};
use super::store::{self, GLOBAL_SLUG};

/// 已知托管站：slug 省掉 host（`owner--repo` 已足够唯一）。
const KNOWN_HOSTS: [&str; 4] = ["github.com", "gitlab.com", "gitee.com", "bitbucket.org"];

/// 家目录下这些子目录本身永远不是项目根（项目只会在它们的子级里）。
const GENERIC_HOME_DIRS: [&str; 8] = [
    "Desktop", "Documents", "Downloads", "AppData", "Pictures", "Videos", "Music", "OneDrive",
];

// ── Git 地址规范化 ───────────────────────────────────────────

/// 归一化后的远程仓库。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Remote {
    pub host: String,
    pub owner: String,
    pub repo: String,
    /// `owner/repo`（界面上「悬停显示」用）
    pub display: String,
    /// `host/owner/repo`
    pub url: String,
    /// 项目身份
    pub slug: String,
    /// 远程名（origin / upstream / …）
    pub name: String,
}

/// 把各种写法的 git 地址归一成 `Remote`。认不出返回 `None`。
///
/// 支持的写法（都是真实见过的）：
/// * `git@github.com:owner/repo.git`
/// * `ssh://git@github.com/owner/repo.git`
/// * `https://github.com/owner/repo`（可带 `.git` / 尾斜杠 / 用户名密码）
/// * `git://github.com/owner/repo`
/// * `github.com/owner/repo`（裸串，无协议）
/// * `https://gitlab.example.com/group/sub/repo`（多级 namespace，取最后两段）
pub fn normalize_remote(raw: &str) -> Option<Remote> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }
    // `git@host:path` → `ssh://git@host/path`；`ssh://` / `git://` → https
    let mut text = trimmed.to_string();
    if let Some(rest) = text.strip_prefix("git@") {
        if let Some((host, path)) = rest.split_once(':') {
            text = format!("ssh://git@{host}/{path}");
        }
    }
    text = text.replacen("ssh://git@", "https://", 1);
    if let Some(rest) = text.strip_prefix("ssh://") {
        text = format!("https://{rest}");
    }
    if let Some(rest) = text.strip_prefix("git://") {
        text = format!("https://{rest}");
    }
    if !text.starts_with("http://") && !text.starts_with("https://") {
        // 裸串 `github.com/owner/repo`：首段像域名才接受，否则（本地路径）放弃
        let looks_like_domain = text
            .split('/')
            .next()
            .map(|head| head.contains('.') && !head.starts_with('.') && !head.starts_with('/'))
            .unwrap_or(false);
        if !looks_like_domain {
            return None;
        }
        text = format!("https://{text}");
    }
    let url = url::Url::parse(&text).ok()?;
    let host = url.host_str()?.to_lowercase();
    let path = url.path().trim_start_matches('/').trim_end_matches('/');
    let path = path.strip_suffix(".git").unwrap_or(path);
    let segments: Vec<&str> = path.split('/').filter(|part| !part.is_empty()).collect();
    if segments.len() < 2 {
        return None;
    }
    let owner = segments[segments.len() - 2].to_string();
    let repo = segments[segments.len() - 1].to_string();
    if owner.is_empty() || repo.is_empty() {
        return None;
    }
    let slug_source = if KNOWN_HOSTS.contains(&host.as_str()) {
        format!("{owner}--{repo}")
    } else {
        format!("{host}--{owner}--{repo}")
    };
    Some(Remote {
        display: format!("{owner}/{repo}"),
        url: format!("{host}/{owner}/{repo}"),
        slug: sanitize_slug(&slug_source),
        host,
        owner,
        repo,
        name: String::new(),
    })
}

/// slug 合法化：小写 + 只留 `[a-z0-9._-]` + 压掉连续分隔符。
///
/// 为什么要小写：Windows 的 NTFS 不区分大小写，`MyApp` 与 `myapp` 会落到同一个
/// 目录，而 macOS 默认也不区分 —— 不折叠就会在两台机器上出现「同一项目两个 slug」。
pub fn sanitize_slug(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    let mut last_sep = false;
    for ch in raw.chars() {
        let lower = ch.to_ascii_lowercase();
        if lower.is_ascii_alphanumeric() || lower == '_' || lower == '.' {
            out.push(lower);
            last_sep = false;
        } else if !last_sep {
            out.push('-');
            last_sep = true;
        }
    }
    let trimmed = out.trim_matches('-').to_string();
    if trimmed.is_empty() {
        return GLOBAL_SLUG.to_string();
    }
    // 120：三个 40 字符的段 + 两个分隔符。够长，又不至于撞文件系统路径上限
    super::model::truncate_chars(&trimmed, 120)
}

// ── 读 git 远程（带缓存）────────────────────────────────────

fn cache() -> &'static Mutex<HashMap<String, Vec<Remote>>> {
    static CACHE: OnceLock<Mutex<HashMap<String, Vec<Remote>>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

/// 某个工作目录下的**全部** git 远程。origin 排第一，其余按 git 返回顺序。
///
/// 为什么全取：一个仓库常同时挂 `origin`（自己的 fork）与 `upstream`（上游）。
/// 项目卡上那一列叫「远程仓库」，只读 origin 会让 upstream 永远不显示。
///
/// 为什么缓存：写记忆的路径要 <50ms，每条记忆起一次 git 子进程（Windows 上
/// 单次 40~80ms）会把这个预算吃穿。进程内缓存，键是 cwd。
pub fn detect_remotes(cwd: &str) -> Vec<Remote> {
    if cwd.trim().is_empty() {
        return Vec::new();
    }
    if let Ok(guard) = cache().lock() {
        if let Some(cached) = guard.get(cwd) {
            return cached.clone();
        }
    }
    let list = read_remotes(cwd);
    if let Ok(mut guard) = cache().lock() {
        // 上限 1000：一台机器上的项目数远小于它，加个界只是防「每次会话都换 cwd」
        // 那种极端用法把内存撑起来
        if guard.len() >= 1000 {
            guard.clear();
        }
        guard.insert(cwd.to_string(), list.clone());
    }
    list
}

fn read_remotes(cwd: &str) -> Vec<Remote> {
    // 单次 `git remote -v` 拿全部：逐 remote 各起一次子进程在 N 个 remote 时
    // 是 N 倍耗时，而输出格式是稳定的（name<TAB>url<TAB>(fetch|push)）
    let output = std::process::Command::new("git")
        .args(["-C", cwd, "remote", "-v"])
        .output();
    let Ok(output) = output else { return Vec::new() };
    if !output.status.success() {
        return Vec::new();
    }
    let text = String::from_utf8_lossy(&output.stdout);
    let mut by_name: Vec<(String, Vec<String>)> = Vec::new();
    for line in text.lines() {
        let parts: Vec<&str> = line.split('\t').collect();
        if parts.len() < 3 || !parts[2].starts_with("(fetch") {
            continue;
        }
        let name = parts[0].trim();
        let url = parts[1].trim();
        if name.is_empty() || url.is_empty() {
            continue;
        }
        match by_name.iter_mut().find(|(existing, _)| existing == name) {
            Some((_, urls)) => {
                if !urls.iter().any(|seen| seen == url) {
                    urls.push(url.to_string());
                }
            }
            None => by_name.push((name.to_string(), vec![url.to_string()])),
        }
    }
    // origin 优先，其余保持 git 的字母序（`git remote -v` 输出本身按名字排序）
    by_name.sort_by(|a, b| match (a.0 == "origin", b.0 == "origin") {
        (true, false) => std::cmp::Ordering::Less,
        (false, true) => std::cmp::Ordering::Greater,
        _ => std::cmp::Ordering::Equal,
    });
    let mut seen = Vec::new();
    let mut out = Vec::new();
    for (name, urls) in by_name {
        for raw in urls {
            let Some(mut remote) = normalize_remote(&raw) else { continue };
            // 同一个仓库挂 ssh + https 两个地址只留一条
            let key = format!("{}/{}/{}", remote.host, remote.owner, remote.repo).to_lowercase();
            if seen.iter().any(|existing| existing == &key) {
                continue;
            }
            seen.push(key);
            remote.name = name.clone();
            out.push(remote);
        }
    }
    out
}

/// 项目身份只认 origin（没有 origin 返回 `None`）。
///
/// 为什么不取「第一个远程」：仓库常挂 upstream / 别人的 remote。把它们当身份，
/// 会让本项目被判成另一个仓库，记忆静默流到错误的项目目录。
pub fn detect_identity(cwd: &str) -> Option<Remote> {
    detect_remotes(cwd).into_iter().find(|remote| remote.name == "origin")
}

/// 从 cwd 往上找最近的 git 根。找不到返回 `None`。
pub fn find_git_root(cwd: &str) -> Option<String> {
    let output = std::process::Command::new("git")
        .args(["-C", cwd, "rev-parse", "--show-toplevel"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if text.is_empty() { None } else { Some(text) }
}

// ── 名称相似度 ───────────────────────────────────────────────

/// 名称相似度（0..1）：大小写归一后的归一化编辑距离 + 子串包含加成。
///
/// 不用 Levenshtein 原值的原因：`agent2api` 与 `Agent2API-main` 的编辑距离是 5，
/// 但语义上几乎是同一个项目；而「包含关系」正好捕捉这一类（目录名 = 仓库名 +
/// 后缀）。两条混起来比任何单一指标都稳。
pub fn name_similarity(a: &str, b: &str) -> f64 {
    let left = normalize_name(a);
    let right = normalize_name(b);
    if left.is_empty() || right.is_empty() {
        return 0.0;
    }
    if left == right {
        return 1.0;
    }
    // 去后缀再比一次：`xxx-main` / `xxx-master` / `xxx-dev` 这类检出目录
    let left_base = strip_checkout_suffix(&left);
    let right_base = strip_checkout_suffix(&right);
    if left_base == right_base && !left_base.is_empty() {
        return 0.98;
    }
    let edit = edit_ratio(&left, &right);
    let contained = if left.contains(&right) || right.contains(&left) {
        // 短串占长串的比例：`app` 在 `application` 里的可信度远低于
        // `agent2api` 在 `agent2api-main` 里
        let (short, long) = if left.len() < right.len() { (&left, &right) } else { (&right, &left) };
        short.chars().count() as f64 / long.chars().count().max(1) as f64
    } else {
        0.0
    };
    // 加权：编辑距离为主，包含关系做加成，上限 1.0
    (edit * 0.75 + contained * 0.45).min(1.0)
}

fn normalize_name(raw: &str) -> String {
    raw.chars()
        .filter(|ch| ch.is_alphanumeric())
        .flat_map(|ch| ch.to_lowercase())
        .collect()
}

fn strip_checkout_suffix(name: &str) -> String {
    for suffix in ["main", "master", "dev", "develop", "trunk", "work", "src"] {
        if let Some(head) = name.strip_suffix(suffix) {
            if !head.is_empty() && head.chars().count() >= 3 {
                return head.to_string();
            }
        }
    }
    name.to_string()
}

/// 归一化编辑相似度：`1 - distance / max(len)`，按字符（不是字节）算。
fn edit_ratio(a: &str, b: &str) -> f64 {
    let left: Vec<char> = a.chars().collect();
    let right: Vec<char> = b.chars().collect();
    let (n, m) = (left.len(), right.len());
    if n == 0 && m == 0 {
        return 1.0;
    }
    if n == 0 || m == 0 {
        return 0.0;
    }
    // 只保留两行：记忆名字最长也就几十字符，全矩阵是 40KB 的白白复制
    let mut previous: Vec<usize> = (0..=m).collect();
    let mut current: Vec<usize> = vec![0; m + 1];
    for i in 1..=n {
        current[0] = i;
        for j in 1..=m {
            let cost = if left[i - 1] == right[j - 1] { 0 } else { 1 };
            current[j] = (previous[j] + 1).min(current[j - 1] + 1).min(previous[j - 1] + cost);
        }
        std::mem::swap(&mut previous, &mut current);
    }
    let distance = previous[m];
    1.0 - distance as f64 / n.max(m) as f64
}

/// 是否像一个「项目目录」：存在、且不是家目录本身、不是家目录下的通用子目录。
pub fn is_project_dir_candidate(path: &str) -> bool {
    let trimmed = path.trim();
    if trimmed.is_empty() {
        return false;
    }
    let candidate = std::path::Path::new(trimmed);
    if !candidate.is_dir() {
        return false;
    }
    let name = candidate.file_name().and_then(|value| value.to_str()).unwrap_or("");
    if GENERIC_HOME_DIRS.iter().any(|generic| generic.eq_ignore_ascii_case(name)) {
        return false;
    }
    // 根目录与一级目录（`C:\`、`/home`、`/tmp`）不是项目
    if candidate.parent().map(|parent| parent.parent().is_none()).unwrap_or(true) {
        return false;
    }
    true
}

// ── 归类 ─────────────────────────────────────────────────────

/// 归类的结论。`origin` 说明「凭什么这么判」，页面据此显示不同提示。
#[derive(Debug, Clone, PartialEq)]
pub struct Classified {
    /// `None` = 归入 general（全局）
    pub slug: Option<String>,
    pub name: Option<String>,
    /// `explicit` = 调用方指名 / `git` = origin 远程 / `gitroot` = 从子目录找到根 /
    /// `fuzzy-auto` = 模糊命中且允许自动建 / `suggest` = 模糊命中待确认 / `general`
    pub origin: &'static str,
    pub confidence: f64,
    /// `suggest` 时的候选
    pub suggestion: Option<String>,
}

impl Classified {
    pub fn to_json(&self) -> Value {
        json!({
            "slug": self.slug,
            "name": self.name,
            "origin": self.origin,
            "confidence": self.confidence,
            "suggestion": self.suggestion,
            "pending": self.origin == "suggest",
        })
    }
}

/// 显式指名的项目：直接采信，但顺手把 git 远程与本地路径补进 registry。
pub fn classify_explicit(name: &str, cwd: Option<&str>) -> Classified {
    let slug = sanitize_slug(name);
    // 显式名如果有 git 身份，用 git 的 slug —— 否则同一个仓库会因为「用户在
    // 客户端里写的项目名」与「远程推导的 slug」分裂成两个项目
    if let Some(cwd) = cwd {
        if let Some(identity) = detect_identity(cwd) {
            store::registry_upsert(&identity.slug, &identity.display, cwd, identity.url.as_str());
            return Classified {
                slug: Some(identity.slug),
                name: Some(identity.display),
                origin: "git",
                confidence: 1.0,
                suggestion: None,
            };
        }
    }
    store::registry_upsert(&slug, name, cwd.unwrap_or_default(), "");
    Classified { slug: Some(slug), name: Some(name.trim().to_string()), origin: "explicit", confidence: 1.0, suggestion: None }
}

/// 从工作目录推断项目。
///
/// 顺序（每一步命中就返回）：
/// 1. `cwd` 自己的 origin 远程
/// 2. 往上找到 git 根，根的 origin 远程
/// 3. registry 里已登记项目名的模糊匹配 ≥ `classify.fuzzyThreshold`
/// 4. 都没有 → general（`suggest` 有待确认候选）
pub fn classify(cwd: &str, agent: Option<&str>) -> Classified {
    if cwd.trim().is_empty() {
        return Classified { slug: None, name: None, origin: "general", confidence: 0.0, suggestion: None };
    }
    // ① 直接的 origin
    if let Some(identity) = detect_identity(cwd) {
        store::registry_upsert(&identity.slug, &identity.display, cwd, identity.url.as_str());
        return Classified {
            slug: Some(identity.slug),
            name: Some(identity.display),
            origin: "git",
            confidence: 1.0,
            suggestion: None,
        };
    }
    // ② 从子目录找根
    if let Some(root) = find_git_root(cwd) {
        if root != cwd {
            if let Some(identity) = detect_identity(&root) {
                store::registry_upsert(&identity.slug, &identity.display, &root, identity.url.as_str());
                return Classified {
                    slug: Some(identity.slug),
                    name: Some(identity.display),
                    origin: "gitroot",
                    confidence: 0.95,
                    suggestion: None,
                };
            }
        }
    }
    // ③ 按目录名模糊匹配已登记项目
    let dir_name = std::path::Path::new(cwd)
        .file_name()
        .and_then(|value| value.to_str())
        .unwrap_or("")
        .to_string();
    if dir_name.is_empty() {
        return Classified { slug: None, name: None, origin: "general", confidence: 0.0, suggestion: None };
    }
    let threshold = config::number("classify.fuzzyThreshold");
    let mut best: Option<(String, String, f64)> = None;
    for project in store::load_projects() {
        let score = name_similarity(&dir_name, &project.name).max(name_similarity(&dir_name, &project.slug));
        if score < threshold {
            continue;
        }
        if best.as_ref().map(|current| score > current.2).unwrap_or(true) {
            best = Some((project.slug, project.name, score));
        }
    }
    if let Some((slug, name, score)) = best {
        if config::bool_of("classify.autoCreateProject") {
            // 明确允许「模糊即建」：直接落 registry，不等人工
            store::registry_upsert(&slug, &name, cwd, "");
            return Classified { slug: Some(slug), name: Some(name), origin: "fuzzy-auto", confidence: score, suggestion: None };
        }
        // 默认路径：产出建议，进待确认队列。写入侧拿到的 slug 是 None（进 general），
        // 人工点头后由 review 队列把它搬过去 —— 归错项目的代价比「先放全局」大得多
        let _ = agent;
        return Classified {
            slug: None,
            name: None,
            origin: "suggest",
            confidence: score,
            suggestion: Some(slug),
        };
    }
    Classified { slug: None, name: None, origin: "general", confidence: 0.0, suggestion: None }
}

// ── 路径拼装 ─────────────────────────────────────────────────

/// 一条记忆相对 memory 根的路径。
///
/// ```text
/// l1: [projects/<slug>|general]/l1/<agent>/<YYYY-MM-DD>.md
/// l2: [projects/<slug>|general]/l2/<sub>/<id>.md
/// ```
///
/// l2 **一律每条一个文件**：早期形态把所有 summary 塞进共享的 `l2/summary.md`，
/// 于是后写覆盖先写、索引里两条指向同一路径、删一条就把另一条变成空壳。
pub fn memory_rel_path(slug: Option<&str>, layer: Layer, agent: &str, kind: &str, date: &str, id: &str) -> String {
    let base = match slug {
        Some(slug) if !slug.is_empty() && slug != GLOBAL_SLUG => format!("projects/{slug}"),
        _ => "general".to_string(),
    };
    match layer {
        Layer::L2 => format!("{base}/l2/{}/{id}.md", l2_dir_for(kind)),
        Layer::L1 => {
            let agent = sanitize_slug(if agent.trim().is_empty() { "manual" } else { agent });
            if kind == "daily" || kind.is_empty() {
                format!("{base}/l1/{agent}/{date}.md")
            } else {
                // l1 里非 daily 的条目（很少见：客户端直接写了一条 knowledge）也
                // 落到同一目录，但文件名带上 id 以免与当天的 daily 文件撞名
                format!("{base}/l1/{agent}/{date}-{id}.md")
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ssh_and_https_forms_give_the_same_slug() {
        let a = normalize_remote("git@github.com:aimod-cc/Agent2API.git").expect("ssh 形式");
        let b = normalize_remote("https://github.com/aimod-cc/Agent2API").expect("https 形式");
        assert_eq!(a.slug, b.slug);
        assert_eq!(a.slug, "aimod-cc--agent2api");
        assert_eq!(a.display, "aimod-cc/Agent2API");
    }

    #[test]
    fn unknown_host_keeps_the_host_in_slug() {
        // 自建 gitlab 上极可能存在同名 fork：不带 host 就会串项目
        let remote = normalize_remote("https://git.corp.example.com/team/repo.git").expect("能解析");
        assert_eq!(remote.slug, "git.corp.example.com--team--repo");
    }

    #[test]
    fn bare_host_string_is_accepted() {
        assert!(normalize_remote("github.com/o/r").is_some());
    }

    #[test]
    fn local_path_is_rejected() {
        assert!(normalize_remote("/home/user/project").is_none());
        assert!(normalize_remote("C:\\work\\project").is_none());
        assert!(normalize_remote("").is_none());
    }

    #[test]
    fn slug_is_case_folded_and_path_safe() {
        // NTFS 不区分大小写：不折叠会让同一项目出现两个目录
        assert_eq!(sanitize_slug("My App/子"), "my-app");
        assert_eq!(sanitize_slug("a//b"), "a-b");
        assert_eq!(sanitize_slug("   "), GLOBAL_SLUG);
    }

    #[test]
    fn similarity_recognises_checkout_suffix() {
        assert!(name_similarity("agent2api", "Agent2API-main") > 0.9);
        assert!(name_similarity("agent2api", "agent2api") > 0.99);
        assert!(name_similarity("alpha", "omega") < 0.4);
    }

    #[test]
    fn short_substring_is_not_a_match() {
        // `app` ⊂ `application` 不该被判成同一项目
        assert!(name_similarity("app", "application") < 0.72);
    }

    #[test]
    fn l2_paths_are_one_file_each() {
        let a = memory_rel_path(Some("o--r"), Layer::L2, "manual", "decision", "2026-11-16", "id-a");
        let b = memory_rel_path(Some("o--r"), Layer::L2, "manual", "decision", "2026-11-16", "id-b");
        assert_ne!(a, b, "两条 decision 不能共用一个文件");
        assert!(a.starts_with("projects/o--r/l2/decisions/"));
    }

    #[test]
    fn l1_daily_is_one_file_per_day() {
        let a = memory_rel_path(Some("o--r"), Layer::L1, "workbuddy", "daily", "2026-11-16", "id-a");
        let b = memory_rel_path(Some("o--r"), Layer::L1, "workbuddy", "daily", "2026-11-16", "id-b");
        assert_eq!(a, b, "同一天同一 agent 的 daily 必须是同一个文件");
        assert_eq!(a, "projects/o--r/l1/workbuddy/2026-11-16.md");
    }

    #[test]
    fn global_project_uses_general_dir() {
        let path = memory_rel_path(None, Layer::L2, "manual", "knowledge", "2026-11-16", "x");
        assert_eq!(path, "general/l2/knowledge/x.md");
    }

    #[test]
    fn edit_ratio_is_char_based() {
        // 按字节算会让中文的相似度全部偏低（每个汉字 3 字节）
        assert!(edit_ratio("记忆", "记忆") > 0.99);
        assert!(edit_ratio("记忆中枢", "记忆中心") > 0.6);
    }
}