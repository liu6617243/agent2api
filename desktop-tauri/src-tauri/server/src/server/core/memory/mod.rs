//! 记忆中枢 · 模块门面（配置 / 写入 / 注入 / 自检）。
//!
//! ── 这一层为什么存在 ────────────────────────────────────────
//! 上层（HTTP API 与转发链路）只该看到「一条记忆进来、一段文本出去」，不该
//! 关心 frontmatter 怎么写、索引怎么建。所以 `model`（形状）/ `store`（文件）/
//! `index`（检索）/ `search`（排序与披露）四块都在内部，本文件是唯一出口。
//!
//! ── 与 AgentHub「记忆中枢」的关系 ────────────────────────────
//! **功能对齐、代码重写。** 设计参考自 AgentHub（作者：沐晖 / HUIdada1，
//! https://github.com/HUIdada1/AgentHub）的记忆中枢：MD 事实源 + 派生索引、
//! 三级披露、`supersedes` 事实演化、`files` 时效预警、回收站。原项目该模块的
//! 文件头写明「禁止未经授权直接搬运代码」，因此本项目**未复制其实现**，只按
//! 上述功能规格用 Rust 重写；行为等价之处是设计对齐的结果，不是代码搬运。
//!
//! ── 与 AgentHub 的有意简化（v1）────────────────────────────
//! · 不做「daily 一文件多节」：所有记忆都是一个文件一条，`type=daily` 只表示
//!   类别。少一层分节解析，代价是当天的多条记忆不在同一个文件里。
//! · 不做 LLM 自动抽取与「待确认」队列：v1 由用户手工写、或由 API 写。
//! · 不做 WebDAV 同步与外部记忆源导入。

pub mod index;
pub mod model;
pub mod search;
pub mod store;

use std::sync::OnceLock;

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use model::{truncate_chars, MemoryEntry};

/// 对外暴露的两个核心类型。
///
/// `MemoryKind` 必须从这里出：`tool_services::memory_mcp` 要用它校验 `type`
/// 参数。`model` 本身是 `pub mod`，但让调用方写 `memory::model::MemoryKind`
/// 等于把内部文件布局变成对外契约 —— 改一次 model 的分文件就波及调用方。
/// re-export 一次之后，调用方只认 `memory::MemoryKind`。
pub use model::MemoryKind;
pub use search::Query;

/// 模块设置。落 `{config_dir}/memory/config.json`（与记忆文件同根，
/// 一起备份、一起搬走）。
///
/// ── 为什么只剩两项 ─────────────────────────────────────────
/// 早期版本还有 `injectEnabled / injectMode / injectTokens / injectProject /
/// injectLimit / injectMinChars` 六项 —— 那是「网关转发时改写 system」路线
/// 的配置。那条路线已删除（层次错位：网关不知道请求属于哪个项目，详见
/// `tool_services::memory_mcp` 的模块头）。现在记忆只通过 MCP 工具暴露，
/// **由客户端决定何时读取**，网关不再有「注不注入、注入多少」这种旋钮。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct MemoryConfig {
    /// 记忆服务总开关。关掉后读写类工具一律拒绝（`memory_status` 仍可用，
    /// 否则关了之后连「为什么没记忆」都查不了）。用途是临时让 Agent 忘记，
    /// 不必去每个客户端里逐个卸载。
    pub enabled: bool,
    /// `memory_core` 的默认 token 预算（工具参数可逐个覆盖）
    pub core_max_tokens: usize,
}

impl Default for MemoryConfig {
    fn default() -> Self {
        MemoryConfig {
            enabled: true,
            core_max_tokens: 800,
        }
    }
}

impl MemoryConfig {
    fn sanitized(mut self) -> Self {
        self.core_max_tokens = self.core_max_tokens.clamp(200, 4000);
        self
    }
}

/// 读模块设置。
///
/// ── 为什么带缓存，而且按 **mtime** 失效 ──────────────────────
/// 这个函数在转发链路上每请求调用一次（决定要不要注入）。每次真去
/// `read_to_string` + `serde_json::from_str` 等于给每个请求加一次文件 IO ——
/// 在 3065 端口那种 QPS 下是纯粹的浪费。按 mtime 判断「文件变了没有」：
/// 命中缓存时只多一次 `metadata()`，改配置（本机页面或手编文件）后下一次
/// 请求就能生效，不需要重启进程。
pub fn config() -> MemoryConfig {
    use std::sync::Mutex;
    static CACHE: OnceLock<Mutex<(Option<std::time::SystemTime>, MemoryConfig)>> = OnceLock::new();
    let path = store::config_file();
    let stamp = std::fs::metadata(&path).ok().and_then(|meta| meta.modified().ok());
    let cache = CACHE.get_or_init(|| Mutex::new((None, MemoryConfig::default())));
    let Ok(mut slot) = cache.lock() else {
        return MemoryConfig::default();
    };
    if slot.0 == stamp {
        return slot.1.clone();
    }
    let loaded = std::fs::read_to_string(&path)
        .ok()
        .and_then(|text| serde_json::from_str::<MemoryConfig>(&text).ok())
        .map(MemoryConfig::sanitized)
        .unwrap_or_default();
    *slot = (stamp, loaded.clone());
    loaded
}

/// 部分更新：只改传进来的键。返回更新后的完整配置。
pub fn save_config(patch: &Value) -> Result<MemoryConfig, String> {
    let mut current = serde_json::to_value(config()).map_err(|error| error.to_string())?;
    let incoming = patch.as_object().ok_or("配置必须是对象")?;
    for (key, value) in incoming {
        if current.get(key).is_some() {
            current[key] = value.clone();
        }
    }
    let next: MemoryConfig = serde_json::from_value(current).map_err(|error| format!("配置值无效：{error}"))?;
    let next = next.sanitized();
    store::ensure_layout()?;
    let text = serde_json::to_string_pretty(&next).map_err(|error| error.to_string())?;
    store::write_atomic(&store::config_file(), &text)?;
    Ok(next)
}

// ── 索引保鲜 ─────────────────────────────────────────────────

/// 记忆内容的「版本号」：每次写入 / 更新 / 删除 / 恢复 / 重建都 +1。
///
/// 注入链路拿它做缓存键 —— `core` 模式要把全部记忆读出来拼文本，每请求做一次
/// 目录扫描在 3065 那种 QPS 下是不可接受的。有版本号之后：内容没变时命中缓存
/// （零 IO），内容一变立即失效（没有 TTL 带来的「刚写的记忆 N 秒后才生效」）。
static REVISION: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

pub fn revision() -> u64 {
    REVISION.load(std::sync::atomic::Ordering::Relaxed)
}

fn bump_revision() {
    REVISION.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
}

/// 进程内只做一次「事实源 vs 索引」的一致性检查：条数或最新更新时间对不上
/// 就整表重建。索引是派生的，重建永远安全。
///
/// 为什么不做文件监听：v1 的写入都经过本模块（写文件的同时更新索引），外力
/// 改动（用户手编 .md）只需要「下次启动/下次点重建」生效，这比一个跨平台的
/// 文件监听器简单得多，也不会在编辑器保存的瞬间读到半个文件。
fn ensure_index_once() {
    static DONE: OnceLock<()> = OnceLock::new();
    DONE.get_or_init(|| {
        let entries = store::load_all();
        let index = index::global();
        let live = entries.iter().filter(|entry| entry.deleted_at.is_none()).count();
        if index.count() != live {
            match index.rebuild(&entries) {
                Ok(count) => crate::server::logging::log(
                    "[Memory]",
                    &format!("记忆索引已重建：{count} 条（事实源 {live} 条有效）"),
                ),
                Err(error) => crate::server::logging::log(
                    "[Memory]",
                    &format!("⚠️ 记忆索引重建失败：{error}"),
                ),
            }
        }
    });
}

/// 全量重建（「重建索引」按钮 / 索引损坏时）
pub fn rebuild() -> Result<usize, String> {
    store::ensure_layout()?;
    let entries = store::load_all();
    let count = index::global().rebuild(&entries)?;
    bump_revision();
    Ok(count)
}

// ── 读 ───────────────────────────────────────────────────────

pub fn list(query: &Query) -> Value {
    ensure_index_once();
    let now = crate::server::logging::now_ms();
    let rows = search::search(query);
    let items: Vec<Value> = rows.iter().map(|row| row.to_json()).collect();
    json!({
        "items": items,
        "lines": rows.iter().map(|row| search::compact_line(row, now)).collect::<Vec<_>>(),
        "total": items.len(),
        "fts": index::global().fts_enabled(),
    })
}

/// 按 id 取全文（可一次取多条）。顺带给出 3 条相关记忆
/// ——「读一条时看到旁边那条」是最常见的发现路径。
pub fn get(ids: &[String], body_limit: usize) -> Value {
    ensure_index_once();
    let all = store::load_all();
    let now = crate::server::logging::now_ms();
    let mut items = Vec::new();
    for id in ids {
        let Some(entry) = all.iter().find(|entry| &entry.id == id) else {
            continue;
        };
        let mut value = entry.to_json(true);
        if body_limit > 0 {
            let body = truncate_chars(&entry.body, body_limit);
            let truncated = body.chars().count() < entry.body.chars().count();
            value["body"] = Value::String(body);
            value["truncated"] = Value::Bool(truncated);
        }
        // 相关记忆：同项目、同类别或共享标签的前 3 条（排除自己）
        let mut related: Vec<(&MemoryEntry, usize)> = all
            .iter()
            .filter(|other| other.id != entry.id && other.is_live())
            .map(|other| {
                let mut score = 0usize;
                if other.project == entry.project && entry.project.is_some() {
                    score += 2;
                }
                if other.kind == entry.kind {
                    score += 1;
                }
                score += other
                    .tags
                    .iter()
                    .filter(|tag| entry.tags.contains(tag))
                    .count();
                (other, score)
            })
            .filter(|(_, score)| *score > 0)
            .collect();
        related.sort_by(|a, b| b.1.cmp(&a.1).then(b.0.updated_at.cmp(&a.0.updated_at)));
        value["related"] = Value::Array(
            related
                .into_iter()
                .take(3)
                .map(|(other, _)| {
                    json!({
                        "id": other.id,
                        "title": other.title,
                        "type": other.kind.as_str(),
                        "line": search_compact(other, now),
                    })
                })
                .collect(),
        );
        items.push(value);
    }
    json!({ "items": items })
}

fn search_compact(entry: &MemoryEntry, now: i64) -> String {
    format!(
        "[{}] {}（{}） · `{}`",
        entry.kind.label(),
        truncate_chars(&entry.title, 50),
        search::relative_time(entry.updated_at, now),
        entry.id
    )
}

/// 事实演化链：以 `supersedes` 为边，向上找根、向下找后继。
pub fn timeline(id: &str) -> Value {
    let all = store::load_all();
    let Some(target) = all.iter().find(|entry| entry.id == id) else {
        return json!({ "items": [] });
    };
    // 向上：我取代了谁（递归），向下：谁取代了我（递归）
    let mut chain: Vec<&MemoryEntry> = Vec::new();
    let mut cursor = Some(target);
    let mut guard = 0;
    while let Some(current) = cursor {
        guard += 1;
        if guard > 32 {
            break;
        }
        chain.push(current);
        cursor = current
            .supersedes
            .first()
            .and_then(|previous| all.iter().find(|entry| &entry.id == previous));
    }
    chain.reverse();
    let mut forward = target.superseded_by.clone();
    let mut guard = 0;
    while let Some(next_id) = forward {
        guard += 1;
        if guard > 32 {
            break;
        }
        let Some(next) = all.iter().find(|entry| entry.id == next_id) else {
            break;
        };
        if chain.iter().any(|item| item.id == next.id) {
            break;
        }
        chain.push(next);
        forward = next.superseded_by.clone();
    }
    let now = crate::server::logging::now_ms();
    json!({
        "items": chain.into_iter().map(|entry| json!({
            "id": entry.id,
            "title": entry.title,
            "type": entry.kind.as_str(),
            "typeLabel": entry.kind.label(),
            "validFrom": entry.valid_from,
            "updatedAt": entry.updated_at,
            "validFromText": search::format_date(entry.valid_from),
            "relative": search::relative_time(entry.updated_at, now),
            "supersededBy": entry.superseded_by,
            "current": entry.id == target.id,
        })).collect::<Vec<_>>(),
    })
}

/// 项目列表（带条数与最近更新）
/// 最近 N 天更新过的记忆（`memory_recent` 工具用）。
///
/// 走索引而不是扫文件：这里要的是「时间段 + 排序」，索引本来就按 `updated_at`
/// 建了索引列；扫文件要全量解析 frontmatter，只为丢掉大部分行。
pub fn recent(project: Option<&str>, agent: Option<&str>, days: usize) -> Value {
    ensure_index_once();
    let now = crate::server::logging::now_ms();
    let days = days.clamp(1, 3650) as i64;
    let since = now - days * 24 * 3600 * 1000;
    let query = Query {
        project: project.map(str::to_string),
        agent: agent.map(str::to_string),
        limit: 200,
        ..Default::default()
    };
    let page = list(&query);
    let items: Vec<Value> = page
        .get("items")
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter(|item| item.get("updatedAt").and_then(Value::as_i64).unwrap_or(0) >= since)
                .cloned()
                .collect()
        })
        .unwrap_or_default();
    let count = items.len();
    json!({ "days": days, "count": count, "items": items })
}

pub fn projects() -> Value {
    ensure_index_once();
    let all = store::load_all();
    let now = crate::server::logging::now_ms();
    let mut stored = store::load_projects();
    for entry in all.iter().filter(|entry| entry.is_live()) {
        let slug = entry
            .project
            .as_deref()
            .map(store::slugify)
            .unwrap_or_else(|| store::GLOBAL_SLUG.to_string());
        if !stored.iter().any(|project| project.slug == slug) {
            stored.push(store::Project {
                slug: slug.clone(),
                name: if slug == store::GLOBAL_SLUG { "全局".to_string() } else { slug },
                remote: None,
                path: None,
                created_at: entry.created_at,
                updated_at: entry.updated_at,
            });
        }
    }
    let items: Vec<Value> = stored
        .iter()
        .map(|project| {
            let mut entries: Vec<&MemoryEntry> = all
                .iter()
                .filter(|entry| entry.is_live())
                .filter(|entry| {
                    entry.project.as_deref().map(store::slugify).as_deref() == Some(project.slug.as_str())
                })
                .collect();
            entries.sort_by(|a, b| b.updated_at.cmp(&a.updated_at));
            json!({
                "slug": project.slug,
                "name": project.name,
                "remote": project.remote,
                "path": project.path,
                "count": entries.len(),
                "updatedAt": entries.first().map(|entry| entry.updated_at).unwrap_or(project.updated_at),
                "updatedText": search::relative_time(
                    entries.first().map(|entry| entry.updated_at).unwrap_or(project.updated_at), now),
                "latest": entries.iter().take(3).map(|entry| json!({
                    "id": entry.id,
                    "title": entry.title,
                    "type": entry.kind.as_str(),
                })).collect::<Vec<_>>(),
            })
        })
        .collect();
    json!({ "items": items })
}

/// 自检：文件根、条数、索引状态、回收站
pub fn status() -> Value {
    let entries = store::load_all();
    let live = entries.iter().filter(|entry| entry.is_live()).count();
    let deleted = entries.iter().filter(|entry| entry.deleted_at.is_some()).count();
    let superseded = entries
        .iter()
        .filter(|entry| entry.superseded_by.is_some() && entry.deleted_at.is_none())
        .count();
    let index_count = index::global().count();
    json!({
        "root": store::root().to_string_lossy(),
        "indexFile": store::index_file().to_string_lossy(),
        "total": entries.len(),
        "live": live,
        "deleted": deleted,
        "superseded": superseded,
        "projects": projects().get("items").map(|items| items.as_array().map(Vec::len).unwrap_or(0)).unwrap_or(0),
        "index": { "count": index_count, "fts": index::global().fts_enabled(), "inSync": index_count == live },
        "config": config(),
        // 内容版本号：每次写入 +1。排障时用来判断「Agent 看到的是不是最新」。
        "revision": revision(),
    })
}

// ── 写 ───────────────────────────────────────────────────────

/// 写入请求（HTTP 与内部调用共用）
#[derive(Debug, Default, Clone)]
pub struct WriteInput {
    pub content: String,
    pub title: Option<String>,
    pub kind: Option<String>,
    pub project: Option<String>,
    pub agent: Option<String>,
    pub tags: Vec<String>,
    pub importance: Option<f64>,
    pub files: Vec<String>,
    pub supersedes: Vec<String>,
    pub pinned: Option<bool>,
    pub starred: Option<bool>,
    pub cwd: Option<String>,
}

/// 生成 id：`20261115-2213-a1b2c3`（日期 + 时分 + 内容哈希前 6 位）。
///
/// 为什么不用递增序号：记忆要能跟着仓库走、能在两台机器之间合并，序号必然撞。
/// 时间 + 内容哈希既人类可读，又不需要协调。
///
/// ── 为什么还要掺一个进程内的自增序号 ────────────────────────
/// 只用「毫秒时间 + 内容哈希」时，**同一毫秒内两次写入相同内容**会得到同一个
/// id，而 id 就是文件名 —— 第二次 `save` 会把第一条整个覆盖掉，是静默的数据
/// 丢失（CI 的 `id_is_time_prefixed_and_unique` 用两条相同内容把它逼出来过）。
/// 掺一个自增序数就把「同毫秒 + 同内容」拆开了，而时间与内容仍是主成分，
/// id 依旧人类可读。
fn make_id(content: &str) -> String {
    use std::hash::{Hash, Hasher};
    use std::sync::atomic::{AtomicU64, Ordering};
    static SEQUENCE: AtomicU64 = AtomicU64::new(0);
    let now = crate::server::logging::now_ms();
    let date = search::format_date(now).replace('-', "");
    let minutes = (now / 60_000).rem_euclid(24 * 60);
    let sequence = SEQUENCE.fetch_add(1, Ordering::Relaxed);
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    content.hash(&mut hasher);
    now.hash(&mut hasher);
    sequence.hash(&mut hasher);
    let digest = format!("{:06x}", hasher.finish() & 0xff_ffff);
    format!("{date}-{:02}{:02}-{digest}", minutes / 60, minutes % 60)
}

pub fn write(input: WriteInput) -> Result<Value, String> {
    let content = input.content.trim().to_string();
    if content.is_empty() {
        return Err("正文不能为空".into());
    }
    store::ensure_layout()?;
    let now = crate::server::logging::now_ms();
    let project = input
        .project
        .as_deref()
        .map(store::slugify)
        .filter(|value| !value.is_empty() && value != store::GLOBAL_SLUG);
    if let (Some(name), Some(cwd)) = (input.project.as_deref(), input.cwd.as_deref()) {
        store::touch_project(name, Some(cwd));
    } else if let Some(name) = input.project.as_deref() {
        store::touch_project(name, None);
    }
    let id = make_id(&content);
    let title = input
        .title
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| {
            content
                .lines()
                .map(str::trim)
                .find(|line| !line.is_empty() && !line.starts_with('#'))
                .map(|line| truncate_chars(line, 60))
                .unwrap_or_else(|| "未命名记忆".to_string())
        });
    let mut entry = MemoryEntry {
        id: id.clone(),
        title,
        kind: MemoryKind::parse(input.kind.as_deref().unwrap_or("daily")),
        project,
        agent: input.agent.clone().or_else(|| Some("manual".to_string())),
        tags: dedup(input.tags),
        importance: input.importance.unwrap_or(0.5).clamp(0.0, 1.0),
        files: dedup(input.files),
        supersedes: dedup(input.supersedes),
        pinned: input.pinned.unwrap_or(false),
        starred: input.starred.unwrap_or(false),
        created_at: now,
        updated_at: now,
        valid_from: now,
        superseded_by: None,
        deleted_at: None,
        body: content,
        path: String::new(),
    };
    entry.path = relative_path(&entry);
    store::save(&entry)?;
    index::global().upsert(&entry)?;
    bump_revision();

    // 被取代的旧记忆：标 superseded_by（不删 —— 演化链要能倒着走回去）
    let mut marked = 0usize;
    for previous in &entry.supersedes {
        if let Some(mut old) = store::load_all().into_iter().find(|item| &item.id == previous) {
            if old.superseded_by.as_deref() == Some(id.as_str()) {
                continue;
            }
            old.superseded_by = Some(id.clone());
            old.updated_at = now;
            old.path = relative_path(&old);
            store::save(&old)?;
            index::global().upsert(&old)?;
            marked += 1;
        }
    }

    crate::server::logging::log(
        "[Memory]",
        &format!(
            "写入记忆 {id}（{}，{} 字，取代 {marked} 条）",
            entry.kind.label(),
            entry.body.chars().count()
        ),
    );
    Ok(json!({ "id": id, "entry": entry.to_json(true), "superseded": marked }))
}

fn relative_path(entry: &MemoryEntry) -> String {
    let deleted = entry.deleted_at.is_some();
    let absolute = store::entry_path(entry, deleted);
    absolute
        .strip_prefix(store::root())
        .map(|rest| rest.to_string_lossy().replace('\\', "/"))
        .unwrap_or_else(|_| format!("projects/{}/{}", store::GLOBAL_SLUG, entry.id))
}

fn dedup(items: Vec<String>) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for item in items {
        let trimmed = item.trim().to_string();
        if !trimmed.is_empty() && !out.contains(&trimmed) {
            out.push(trimmed);
        }
    }
    out
}

/// 更新（标题/正文/标签/重要度/星标/置顶）
pub fn update(id: &str, patch: &Value) -> Result<Value, String> {
    let all = store::load_all();
    let mut entry = all
        .into_iter()
        .find(|entry| entry.id == id)
        .ok_or_else(|| format!("找不到记忆 {id}"))?;
    if let Some(title) = patch.get("title").and_then(Value::as_str) {
        if !title.trim().is_empty() {
            entry.title = title.trim().to_string();
        }
    }
    if let Some(body) = patch.get("body").and_then(Value::as_str) {
        if !body.trim().is_empty() {
            entry.body = body.to_string();
        }
    }
    if let Some(kind) = patch.get("type").and_then(Value::as_str) {
        entry.kind = MemoryKind::parse(kind);
    }
    if let Some(tags) = patch.get("tags").and_then(Value::as_array) {
        entry.tags = dedup(tags.iter().filter_map(|tag| tag.as_str().map(str::to_string)).collect());
    }
    if let Some(importance) = patch.get("importance").and_then(Value::as_f64) {
        entry.importance = importance.clamp(0.0, 1.0);
    }
    if let Some(starred) = patch.get("starred").and_then(Value::as_bool) {
        entry.starred = starred;
    }
    if let Some(pinned) = patch.get("pinned").and_then(Value::as_bool) {
        entry.pinned = pinned;
    }
    entry.updated_at = crate::server::logging::now_ms();
    entry.path = relative_path(&entry);
    store::save(&entry)?;
    index::global().upsert(&entry)?;
    bump_revision();
    Ok(json!({ "entry": entry.to_json(true) }))
}

/// 删除（进回收站）。文件搬 `trash/`，frontmatter 写 `deleted_at`。
pub fn remove(id: &str) -> Result<Value, String> {
    let all = store::load_all();
    let mut entry = all
        .into_iter()
        .find(|entry| entry.id == id)
        .ok_or_else(|| format!("找不到记忆 {id}"))?;
    if entry.deleted_at.is_some() {
        return Ok(json!({ "id": id, "already": true }));
    }
    let previous_path = store::entry_path(&entry, false);
    if previous_path.exists() {
        let _ = std::fs::remove_file(&previous_path);
    }
    entry.deleted_at = Some(crate::server::logging::now_ms());
    entry.updated_at = entry.deleted_at.unwrap_or(entry.updated_at);
    entry.path = relative_path(&entry);
    store::save(&entry)?;
    index::global().upsert(&entry)?;
    bump_revision();
    Ok(json!({ "id": id, "trashed": true }))
}

/// 从回收站恢复
pub fn restore(id: &str) -> Result<Value, String> {
    let all = store::load_all();
    let mut entry = all
        .into_iter()
        .find(|entry| entry.id == id)
        .ok_or_else(|| format!("找不到记忆 {id}"))?;
    if entry.deleted_at.is_none() {
        return Ok(json!({ "id": id, "already": true }));
    }
    let previous = store::entry_path(&entry, true);
    if previous.exists() {
        let _ = std::fs::remove_file(&previous);
    }
    entry.deleted_at = None;
    entry.updated_at = crate::server::logging::now_ms();
    entry.path = relative_path(&entry);
    store::save(&entry)?;
    index::global().upsert(&entry)?;
    bump_revision();
    Ok(json!({ "id": id, "restored": true }))
}

/// 彻底删除（不进回收站）。仅供「回收站里再删一次」。
pub fn purge(id: &str) -> Result<Value, String> {
    let all = store::load_all();
    let entry = all
        .into_iter()
        .find(|entry| entry.id == id)
        .ok_or_else(|| format!("找不到记忆 {id}"))?;
    store::remove_file(&entry)?;
    index::global().drop_one(id);
    bump_revision();
    Ok(json!({ "id": id, "purged": true }))
}

// ── 三级披露的对外入口 ───────────────────────────────────────

/// 核心记忆文本（注入用；也供页面「核心记忆」视图预览）
pub fn core_text(project: Option<&str>, max_tokens: usize) -> Value {
    ensure_index_once();
    let entries = store::load_all();
    let (text, used, truncated) = search::core_text(&entries, project, max_tokens);
    json!({
        "text": text,
        "tokens": used,
        "truncated": truncated,
        "totalTokens": model::estimate_tokens(&text),
    })
}

pub fn digest_text(max_lines: usize) -> Value {
    ensure_index_once();
    let entries = store::load_all();
    let text = search::digest_text(&entries, max_lines);
    json!({ "text": text, "lines": text.lines().count() })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn id_is_time_prefixed_and_unique() {
        let a = make_id("同样的内容");
        let b = make_id("同样的内容");
        assert!(a.starts_with(&search::format_date(crate::server::logging::now_ms()).replace('-', "")));
        assert_ne!(a, b, "同一秒内两次写入也必须拿到不同 id");
    }

    #[test]
    fn config_patch_only_touches_given_keys() {
        let patch = json!({ "coreMaxTokens": 640, "无关键": 1 });
        let mut current = serde_json::to_value(MemoryConfig::default()).unwrap();
        for (key, value) in patch.as_object().unwrap() {
            if current.get(key).is_some() {
                current[key] = value.clone();
            }
        }
        let next: MemoryConfig = serde_json::from_value(current).unwrap();
        assert_eq!(next.core_max_tokens, 640);
        // 未传的键保持默认
        assert!(next.enabled);
    }

    /// 旧的 `inject*` 配置留在磁盘上时不能把整份配置读崩（升级路径）。
    #[test]
    fn legacy_inject_keys_are_ignored() {
        let legacy = json!({
            "enabled": true,
            "injectEnabled": true,
            "injectMode": "search",
            "injectTokens": 1200,
            "coreMaxTokens": 500
        });
        let config: MemoryConfig = serde_json::from_value(legacy).unwrap();
        assert!(config.enabled);
        assert_eq!(config.core_max_tokens, 500);
    }

    #[test]
    fn dedup_keeps_order_and_drops_blank() {
        let out = dedup(vec![" a ".into(), "a".into(), "".into(), "b".into()]);
        assert_eq!(out, vec!["a".to_string(), "b".to_string()]);
    }
}
