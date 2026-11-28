//! 技能仓库 · 台账与回收站（manifest / 冲突队列 / 日报 / .trash）。
//!
//! ── 中央仓库的目录布局 ──────────────────────────────────────
//! ```text
//! <hub>/
//!   manifest.json     台账：每个技能一份真身记录（来源 / 挂载 / 变更历史）
//!   conflicts.json    待裁决队列（同名异容、链接指向异常、跨设备残留）
//!   skills/<name>/    真身目录 —— 全机唯一一份，其余位置都是指向它的链接
//!   .trash/<name>.<ts> 回收站（删技能 / 原位转挂载前的旧目录）
//!   reports/sync-<ts>.md 每次同步的 Markdown 报告
//! ```
//!
//! ── 为什么真身放在这里而不是各客户端目录里 ──────────────────
//! 四个客户端各有一份同名技能，改一处另外三处就旧了 —— 于是「你改的是哪个」
//! 变成每天都要回答的问题。收成一份真身、其余位置改链接之后，改任意一处就是
//! 改全部。这条也正是「份消除」这个指标的含义。
//!
//! ── 为什么 manifest 里要有 deleted 墓碑 ─────────────────────
//! 删掉的技能会在下一次扫描里「重新出现」—— 因为别的工具目录里还留着副本。
//! 墓碑记住「这个技能是用户主动删的」，扫描时跳过它，否则删除成了永远删不掉。
//!
//! ── 写盘为什么一律 tmp + rename ─────────────────────────────
//! 台账写一半断电留下半个 JSON，整个仓库就「没了」（解析失败 → 当成空台账 →
//! 下一次同步把所有链接判成野链接）。tempfile 名带 pid：两个进程共享同一个
//! 仓库时（桌面端 + 命令行），固定名字会让彼此把对方的半成品改名走。

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::{json, Map, Value};

use super::scan;
use super::tools;

/// 台账版本号。改了结构（不是新增可选字段）才 +1，供将来迁移判断。
const MANIFEST_VERSION: i64 = 1;

/// 中央仓库根目录。默认落在网关数据目录下，可被配置的 `hubDir` 改走
/// （换盘 / 放进同步盘的人不少）。
pub fn hub_dir() -> PathBuf {
    let config = tools::load_config();
    let custom = config.get("hubDir").and_then(Value::as_str).unwrap_or("").trim();
    if custom.is_empty() {
        crate::server::config::config_dir().join("skills-hub")
    } else {
        tools::expand_path(custom)
    }
}

pub fn skills_dir() -> PathBuf {
    hub_dir().join("skills")
}

pub fn trash_dir() -> PathBuf {
    hub_dir().join(".trash")
}

pub fn reports_dir() -> PathBuf {
    hub_dir().join("reports")
}

pub fn ensure_hub() -> Result<PathBuf, String> {
    let root = hub_dir();
    for dir in [root.clone(), skills_dir(), trash_dir(), reports_dir()] {
        fs::create_dir_all(&dir).map_err(|error| format!("创建目录失败 {}：{error}", dir.display()))?;
    }
    Ok(root)
}

fn manifest_file() -> PathBuf {
    hub_dir().join("manifest.json")
}

fn conflicts_file() -> PathBuf {
    hub_dir().join("conflicts.json")
}

/// 当前时间（毫秒）。避开 chrono，时区无关的计算都用它。
pub fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|value| value.as_millis() as i64)
        .unwrap_or(0)
}

/// 台账里的时间戳形态（ISO-8601 本地时间，人能读、也能排序）。
pub fn now_iso() -> String {
    chrono::Local::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}

fn stamp_for_name() -> String {
    chrono::Local::now().format("%Y%m%d-%H%M%S").to_string()
}

/// 原子写：同目录 tmp（带 pid）→ 写满 → fsync → 删旧 → rename。
pub fn write_atomic(path: &Path, contents: &str) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|error| format!("创建目录失败 {}：{error}", parent.display()))?;
    }
    let tmp = path.with_extension(format!("tmp.{}", std::process::id()));
    {
        let mut file = fs::File::create(&tmp).map_err(|error| format!("写入失败 {}：{error}", tmp.display()))?;
        file.write_all(contents.as_bytes())
            .map_err(|error| format!("写入失败 {}：{error}", tmp.display()))?;
        file.flush().map_err(|error| format!("刷盘失败 {}：{error}", tmp.display()))?;
        file.sync_all().map_err(|error| format!("刷盘失败 {}：{error}", tmp.display()))?;
    }
    // Windows 上 rename 到已存在文件会失败，先删目标；失败时顺手清 tmp，
    // 否则目录里会留下一堆 `.tmp.1234` 残骸。
    if path.exists() {
        if let Err(error) = fs::remove_file(path) {
            let _ = fs::remove_file(&tmp);
            return Err(format!("替换失败 {}：{error}", path.display()));
        }
    }
    fs::rename(&tmp, path).map_err(|error| {
        let _ = fs::remove_file(&tmp);
        format!("落盘失败 {}：{error}", path.display())
    })
}

// ─── manifest ────────────────────────────────────────────────

/// 读台账。文件不存在 / 解析失败一律给空台账 —— 报错会让整个页面打不开，
/// 而空台账至少能让用户从「重新扫描」开始自救。
pub fn load_manifest() -> Value {
    let empty = || {
        json!({
            "version": MANIFEST_VERSION,
            "updatedAt": Value::Null,
            "skills": {},
            "deleted": {},
        })
    };
    let text = match fs::read_to_string(manifest_file()) {
        Ok(value) => value,
        Err(_) => return empty(),
    };
    let mut parsed = match serde_json::from_str::<Value>(&text) {
        Ok(value) => value,
        Err(_) => return empty(),
    };
    if !parsed.is_object() {
        return empty();
    }
    if !parsed.get("skills").map(Value::is_object).unwrap_or(false) {
        parsed["skills"] = json!({});
    }
    if !parsed.get("deleted").map(Value::is_object).unwrap_or(false) {
        parsed["deleted"] = json!({});
    }
    parsed
}

pub fn save_manifest(manifest: &Value) -> Result<(), String> {
    let mut next = manifest.clone();
    if !next.is_object() {
        return Err("台账必须是 JSON 对象".to_string());
    }
    next["version"] = json!(MANIFEST_VERSION);
    next["updatedAt"] = json!(now_iso());
    let text = serde_json::to_string_pretty(&next).map_err(|error| format!("台账序列化失败：{error}"))?;
    write_atomic(&manifest_file(), &text)
}

/// 台账里的某个技能（不可变借用，找不到给 `Null`）。
pub fn manifest_skill<'a>(manifest: &'a Value, name: &str) -> &'a Value {
    manifest
        .get("skills")
        .and_then(Value::as_object)
        .and_then(|map| map.get(name))
        .unwrap_or(&Value::Null)
}

fn push_history(entry: &mut Value, action: &str, detail: &str) {
    if !entry.is_object() {
        return;
    }
    let list = entry
        .as_object_mut()
        .map(|map| {
            map.entry("mergeHistory".to_string())
                .or_insert_with(|| json!([]))
        })
        .and_then(Value::as_array_mut);
    if let Some(items) = list {
        items.push(json!({ "at": now_iso(), "action": action, "detail": detail }));
        // 留最近 50 条：技能详情页只展示历史，无限增长会让台账变成日志
        if items.len() > 50 {
            let overflow = items.len() - 50;
            items.drain(0..overflow);
        }
    }
}

/// 把来源并进已存在的记录（L1 合并时）。
pub fn merge_sources(entry: &mut Value, sources: &[Value]) {
    if !entry.is_object() {
        return;
    }
    let existing = entry
        .as_object_mut()
        .map(|map| map.entry("sources".to_string()).or_insert_with(|| json!([])))
        .and_then(Value::as_array_mut);
    let Some(items) = existing else { return };
    for source in sources {
        let tool = source.get("tool").and_then(Value::as_str).unwrap_or("");
        let name = source.get("name").and_then(Value::as_str).unwrap_or("");
        let dup = items.iter().any(|item| {
            item.get("tool").and_then(Value::as_str) == Some(tool)
                && item.get("originalName").and_then(Value::as_str) == Some(name)
        });
        if dup {
            continue;
        }
        items.push(json!({
            "tool": tool,
            "originalName": name,
            "firstSeen": now_iso(),
        }));
    }
}

/// 收纳一个技能：整目录复制进中央，按**中央副本实算**哈希落账。
///
/// ── 为什么哈希要重算 ────────────────────────────────────────
/// 扫描时算的是源侧哈希。源目录里可能有链接（扫描层不跟链接进哈希），复制
/// 之后链接变成实目录，两份哈希就不同了 —— 台账记旧值的话，下一轮同步会把
/// 这份内容永远判成冲突。落账用实算值，比较才收敛。
pub fn import_skill(entry: &Value) -> Result<Value, String> {
    ensure_hub()?;
    let name = entry.get("name").and_then(Value::as_str).unwrap_or("").trim().to_string();
    if name.is_empty() {
        return Err("技能名为空".to_string());
    }
    let source_dir = PathBuf::from(entry.get("dir").and_then(Value::as_str).unwrap_or(""));
    if !source_dir.is_dir() {
        return Err(format!("来源目录不存在：{}", source_dir.display()));
    }
    let dest = skills_dir().join(&name);
    let incoming_hash = entry.get("treeHash").and_then(Value::as_str).unwrap_or("");
    let mut manifest = load_manifest();

    if dest.exists() {
        let old = manifest_skill(&manifest, &name).clone();
        let old_hash = old.get("treeHash").and_then(Value::as_str).unwrap_or("");
        if !incoming_hash.is_empty() && old_hash == incoming_hash {
            // 同内容：只补来源记录
            if let Some(slot) = manifest
                .get_mut("skills")
                .and_then(Value::as_object_mut)
                .and_then(|map| map.get_mut(&name))
            {
                let sources = entry.get("sources").and_then(Value::as_array).cloned().unwrap_or_default();
                merge_sources(slot, &sources);
            }
            save_manifest(&manifest)?;
            return Ok(json!({ "action": "source-added", "name": name }));
        }
        return Ok(json!({
            "action": "conflict",
            "name": name,
            "message": "中央已存在同名不同内容的技能",
        }));
    }

    copy_dir_all(&source_dir, &dest)?;
    // 写入后立刻读：必须绕过缓存（写入前刚比对过两边哈希）。
    let real_hash = scan::tree_hash_fresh(&dest);
    let sources: Vec<Value> = entry
        .get("sources")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default()
        .into_iter()
        .map(|source| {
            json!({
                "tool": source.get("tool").and_then(Value::as_str).unwrap_or(""),
                "originalName": source.get("name").and_then(Value::as_str).unwrap_or(""),
                "firstSeen": now_iso(),
            })
        })
        .collect();
    let source_text = sources
        .iter()
        .map(|source| {
            format!(
                "{}:{}",
                source.get("tool").and_then(Value::as_str).unwrap_or("?"),
                source.get("originalName").and_then(Value::as_str).unwrap_or("?")
            )
        })
        .collect::<Vec<String>>()
        .join(", ");
    let record = json!({
        "name": name,
        "skillName": entry.get("skillName").and_then(Value::as_str).unwrap_or(&name),
        "description": entry.get("description").and_then(Value::as_str).unwrap_or(""),
        "version": entry.get("version").and_then(Value::as_str).unwrap_or(""),
        "treeHash": real_hash,
        "sources": sources,
        "mounts": [],
        "mergeHistory": [{
            "at": now_iso(),
            "action": "import",
            "detail": format!("收纳自 {source_text}"),
        }],
    });
    if let Some(map) = manifest.get_mut("skills").and_then(Value::as_object_mut) {
        map.insert(name.clone(), record);
    }
    // 重新收纳 = 用户明确要它回来，墓碑必须清掉，否则下一轮扫描又跳过它
    if let Some(map) = manifest.get_mut("deleted").and_then(Value::as_object_mut) {
        map.remove(&name);
    }
    save_manifest(&manifest)?;
    Ok(json!({ "action": "imported", "name": name, "treeHash": real_hash }))
}

/// 登记一处挂载（幂等：同一路径只留一条）。
pub fn set_mount(skill: &str, tool_id: &str, link_path: &Path, kind: &str, enabled: bool) -> Result<(), String> {
    let mut manifest = load_manifest();
    let Some(entry) = manifest
        .get_mut("skills")
        .and_then(Value::as_object_mut)
        .and_then(|map| map.get_mut(skill))
    else {
        return Ok(());
    };
    let path_key = scan::path_key(link_path);
    let mounts = entry
        .as_object_mut()
        .map(|map| map.entry("mounts".to_string()).or_insert_with(|| json!([])))
        .and_then(Value::as_array_mut);
    let Some(items) = mounts else { return Ok(()) };
    let name = link_path
        .file_name()
        .map(|value| value.to_string_lossy().to_string())
        .unwrap_or_else(|| skill.to_string());
    if let Some(slot) = items.iter_mut().find(|item| {
        item.get("path")
            .and_then(Value::as_str)
            .map(|text| scan::path_key(Path::new(text)) == path_key)
            .unwrap_or(false)
    }) {
        slot["tool"] = json!(tool_id);
        slot["name"] = json!(name);
        slot["path"] = json!(link_path.to_string_lossy());
        slot["type"] = json!(kind);
        slot["enabled"] = json!(enabled);
    } else {
        items.push(json!({
            "tool": tool_id,
            "name": name,
            "path": link_path.to_string_lossy(),
            "type": kind,
            "enabled": enabled,
        }));
    }
    push_history(
        entry,
        if enabled { "mount" } else { "unmount" },
        &format!("{tool_id}:{name} → {}", link_path.display()),
    );
    save_manifest(&manifest)
}

/// 摘掉一条挂载记录（中央真身还在，只是不再发布到那个位置）。
pub fn drop_mount(skill: &str, tool_id: &str, link_path: &Path) -> Result<(), String> {
    let mut manifest = load_manifest();
    let Some(entry) = manifest
        .get_mut("skills")
        .and_then(Value::as_object_mut)
        .and_then(|map| map.get_mut(skill))
    else {
        return Ok(());
    };
    let path_key = scan::path_key(link_path);
    if let Some(items) = entry.get_mut("mounts").and_then(Value::as_array_mut) {
        items.retain(|item| {
            let same_path = item
                .get("path")
                .and_then(Value::as_str)
                .map(|text| scan::path_key(Path::new(text)) == path_key)
                .unwrap_or(false);
            !same_path
        });
    }
    push_history(entry, "unmount", &format!("{tool_id} 摘除 {}", link_path.display()));
    save_manifest(&manifest)
}

// ─── 回收站 ──────────────────────────────────────────────────

/// 把目录移进回收站，返回落地路径。
///
/// ── 为什么不用「直接删」─────────────────────────────────────
/// 原位转挂载、冲突裁决的落选版本、用户手动删除 —— 这三个动作都会移除一份
/// 真实技能内容。删错了没有第二次机会（内容只有用户自己写得出来），所以
/// 一律先进回收站，保留 `trashDays` 天。
pub fn to_trash(path: &Path, name: &str) -> Result<PathBuf, String> {
    ensure_hub()?;
    let ts = stamp_for_name();
    let mut dest = trash_dir().join(format!("{name}.{ts}"));
    let mut index = 1;
    while dest.exists() {
        dest = trash_dir().join(format!("{name}.{ts}-{index}"));
        index += 1;
    }
    match fs::rename(path, &dest) {
        Ok(()) => Ok(dest),
        Err(_) => {
            // 跨盘（EXDEV）：rename 不行就复制后删源
            copy_dir_all(path, &dest)?;
            remove_dir_all(path)?;
            Ok(dest)
        }
    }
}

fn remove_dir_all(path: &Path) -> Result<(), String> {
    if path.is_dir() {
        fs::remove_dir_all(path).map_err(|error| format!("删除目录失败 {}：{error}", path.display()))
    } else if path.exists() {
        fs::remove_file(path).map_err(|error| format!("删除文件失败 {}：{error}", path.display()))
    } else {
        Ok(())
    }
}

/// 回收站条目：原名 / 落地路径 / 时间 / 体积 / 剩余天数。
pub fn list_trash(trash_days: i64) -> Vec<Value> {
    let mut rows: Vec<Value> = Vec::new();
    let entries = match fs::read_dir(trash_dir()) {
        Ok(value) => value,
        Err(_) => return rows,
    };
    let now = now_ms();
    for entry in entries.flatten() {
        let path = entry.path();
        let meta = match fs::symlink_metadata(&path) {
            Ok(value) => value,
            Err(_) => continue,
        };
        let file_name = entry.file_name().to_string_lossy().to_string();
        let original = name_from_trash(&file_name);
        let mtime_ms = meta
            .modified()
            .ok()
            .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
            .map(|duration| duration.as_millis() as i64)
            .unwrap_or(now);
        let age_days = ((now - mtime_ms) as f64 / 86_400_000.0).floor() as i64;
        rows.push(json!({
            "file": file_name,
            "name": original,
            "path": path.to_string_lossy(),
            "mtimeMs": mtime_ms,
            "size": if meta.is_dir() { scan::dir_size(&path) } else { meta.len() },
            "ageDays": age_days,
            "daysLeft": (trash_days - age_days).max(0),
        }));
    }
    rows.sort_by(|a, b| {
        b.get("mtimeMs").and_then(Value::as_i64).unwrap_or(0)
            .cmp(&a.get("mtimeMs").and_then(Value::as_i64).unwrap_or(0))
    });
    rows
}

/// 清理过期回收站，返回清掉的条数。
pub fn purge_trash(trash_days: i64) -> usize {
    let mut removed = 0usize;
    for row in list_trash(trash_days) {
        let days_left = row.get("daysLeft").and_then(Value::as_i64).unwrap_or(1);
        if days_left > 0 {
            continue;
        }
        let path = PathBuf::from(row.get("path").and_then(Value::as_str).unwrap_or(""));
        if path.exists() && remove_dir_all(&path).is_ok() {
            removed += 1;
        }
    }
    removed
}

/// 清空回收站（不管是否过期），返回清掉的条数。
///
/// ── 为什么和 `purge_trash` 分成两个函数 ──────────────────────
/// `purge_trash` 是「按保留天数扫过期」的定时动作：同步流程每轮顺手调用一次
/// （见 `plan.rs`），早一天都不该删。而设置页「危险区 · 清空」的语义是
/// **用户此刻就要这里全空**，跟保留天数无关。把两者合并成一个函数，要么定时
/// 清理会越权删掉未过期内容，要么「清空」点了半天什么都不消失 —— 后者正是
/// 之前「点了没反应」那类反馈的来源。
pub fn empty_trash() -> usize {
    let mut removed = 0usize;
    let entries = match fs::read_dir(trash_dir()) {
        Ok(value) => value,
        Err(_) => return removed,
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if remove_dir_all(&path).is_ok() {
            removed += 1;
        }
    }
    removed
}

/// 从回收站还原：回到中央真身目录并补回台账记录。
/// 从回收站条目名还原技能名（`<name>.<ts>` 或 `<name>.<ts>-<n>`）。
///
/// ── 为什么不用 `file.split('.').next()` ────────────────────
/// 技能名允许含点号（`pdf.tools`、`v1.2.helper` 都见过）。按第一个点切会把
/// `pdf.tools.20261125-120000` 还原成 `pdf` —— 技能名被截断，还原出来的目录
/// 与台账对不上。时间戳永远是**最后**一段且形态固定（`YYYYMMDD-HHMMSS`，
/// 冲突时再跟 `-<序号>`），所以从末尾切、并校验形态。
fn name_from_trash(file: &str) -> String {
    let is_stamp = |part: &str| -> bool {
        // `20261125-120000` 或 `20261125-120000-2`
        let mut segments = part.split('-');
        let (Some(date), Some(clock)) = (segments.next(), segments.next()) else {
            return false;
        };
        if date.len() != 8 || clock.len() != 6 {
            return false;
        }
        if !date.bytes().all(|b| b.is_ascii_digit()) || !clock.bytes().all(|b| b.is_ascii_digit()) {
            return false;
        }
        // 余下只允许一个纯数字序号
        segments.all(|extra| !extra.is_empty() && extra.bytes().all(|b| b.is_ascii_digit()))
    };
    match file.rsplit_once('.') {
        Some((head, tail)) if is_stamp(tail) => head.to_string(),
        // 形态对不上（用户手放进去的、旧版本留下的）：原样当名字，不做猜测
        _ => file.to_string(),
    }
}

pub fn restore_trash(file: &str) -> Result<Value, String> {
    ensure_hub()?;
    let file = file.trim();
    if file.is_empty() || file.contains('/') || file.contains('\\') || file.contains("..") {
        return Err("非法的回收站条目".to_string());
    }
    let source = trash_dir().join(file);
    if !source.exists() {
        return Err("回收站里没有这一项".to_string());
    }
    let name = name_from_trash(file);
    let dest = skills_dir().join(&name);
    if dest.exists() {
        return Err(format!("中央已有同名技能「{name}」，先删掉它再还原"));
    }
    match fs::rename(&source, &dest) {
        Ok(()) => {}
        Err(_) => {
            copy_dir_all(&source, &dest)?;
            remove_dir_all(&source)?;
        }
    }
    // 刚从回收站搬回来：绕过缓存，现场读一遍。
    let hash = scan::tree_hash_fresh(&dest);
    let parsed = scan::parse_skill_md(&dest).ok();
    let mut manifest = load_manifest();
    let record = json!({
        "name": name,
        "skillName": parsed.as_ref().map(|item| item.name.clone()).filter(|text| !text.trim().is_empty()).unwrap_or_else(|| name.clone()),
        "description": parsed.as_ref().map(|item| item.description.clone()).unwrap_or_default(),
        "version": parsed.as_ref().map(|item| item.version.clone()).unwrap_or_default(),
        "treeHash": hash,
        "sources": [],
        "mounts": [],
        "mergeHistory": [{ "at": now_iso(), "action": "restore", "detail": "从回收站还原" }],
    });
    if let Some(map) = manifest.get_mut("skills").and_then(Value::as_object_mut) {
        map.insert(name.clone(), record);
    }
    if let Some(map) = manifest.get_mut("deleted").and_then(Value::as_object_mut) {
        map.remove(&name);
    }
    save_manifest(&manifest)?;
    Ok(json!({ "name": name }))
}

// ─── 墓碑 ────────────────────────────────────────────────────

pub fn tombstone(name: &str, reason: &str) -> Result<(), String> {
    let mut manifest = load_manifest();
    if let Some(map) = manifest.get_mut("deleted").and_then(Value::as_object_mut) {
        map.insert(name.to_string(), json!({ "at": now_iso(), "reason": reason }));
    }
    save_manifest(&manifest)
}

pub fn is_deleted(manifest: &Value, name: &str) -> bool {
    manifest
        .get("deleted")
        .and_then(Value::as_object)
        .map(|map| map.contains_key(name))
        .unwrap_or(false)
}

// ─── 冲突队列 ────────────────────────────────────────────────

pub fn load_conflicts() -> Vec<Value> {
    let text = match fs::read_to_string(conflicts_file()) {
        Ok(value) => value,
        Err(_) => return Vec::new(),
    };
    let parsed = match serde_json::from_str::<Value>(&text) {
        Ok(value) => value,
        Err(_) => return Vec::new(),
    };
    parsed
        .get("items")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default()
        .into_iter()
        .filter(|item| !item.get("resolved").and_then(Value::as_bool).unwrap_or(false))
        .collect()
}

pub fn save_conflicts(items: &[Value]) -> Result<(), String> {
    ensure_hub()?;
    let payload = json!({ "items": items });
    let text = serde_json::to_string_pretty(&payload).map_err(|error| format!("冲突序列化失败：{error}"))?;
    write_atomic(&conflicts_file(), &text)
}

/// 插入一条待裁决（同 id 已存在就不重复插）。
pub fn upsert_conflict(item: Value) -> Result<(), String> {
    let id = item.get("id").and_then(Value::as_str).unwrap_or("").to_string();
    if id.is_empty() {
        return Ok(());
    }
    let mut items = load_conflicts();
    if items.iter().any(|existing| existing.get("id").and_then(Value::as_str) == Some(&id)) {
        return Ok(());
    }
    let mut record = item;
    record["at"] = json!(now_iso());
    items.push(record);
    save_conflicts(&items)
}

pub fn drop_conflict(id: &str) -> Result<(), String> {
    let items: Vec<Value> = load_conflicts()
        .into_iter()
        .filter(|item| item.get("id").and_then(Value::as_str) != Some(id))
        .collect();
    save_conflicts(&items)
}

/// 用本轮的扫描结果刷新「哪些冲突还成立」：不再成立的自动消失，
/// `norm` 类保留到人裁决为止（名称归一的关系不会因为一轮同步而消失）。
pub fn prune_conflicts(active_ids: &[String]) -> Result<Vec<Value>, String> {
    let kept: Vec<Value> = load_conflicts()
        .into_iter()
        .filter(|item| {
            let kind = item.get("kind").and_then(Value::as_str).unwrap_or("");
            if kind == "norm" {
                return true;
            }
            let id = item.get("id").and_then(Value::as_str).unwrap_or("");
            active_ids.iter().any(|active| active == id)
        })
        .collect();
    save_conflicts(&kept)?;
    Ok(kept)
}

// ─── 日报 ────────────────────────────────────────────────────

pub fn write_report(body: &str) -> Result<String, String> {
    ensure_hub()?;
    let file = format!("sync-{}.md", stamp_for_name());
    write_atomic(&reports_dir().join(&file), body)?;
    Ok(file)
}

pub fn list_reports(limit: usize) -> Vec<Value> {
    let mut rows: Vec<Value> = Vec::new();
    let entries = match fs::read_dir(reports_dir()) {
        Ok(value) => value,
        Err(_) => return rows,
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let meta = match fs::metadata(&path) {
            Ok(value) => value,
            Err(_) => continue,
        };
        let mtime_ms = meta
            .modified()
            .ok()
            .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
            .map(|duration| duration.as_millis() as i64)
            .unwrap_or(0);
        rows.push(json!({
            "file": entry.file_name().to_string_lossy(),
            "mtimeMs": mtime_ms,
            "size": meta.len(),
        }));
    }
    rows.sort_by(|a, b| {
        b.get("mtimeMs").and_then(Value::as_i64).unwrap_or(0)
            .cmp(&a.get("mtimeMs").and_then(Value::as_i64).unwrap_or(0))
    });
    rows.truncate(limit);
    rows
}

pub fn read_report(file: &str) -> Result<String, String> {
    let file = file.trim();
    if file.is_empty() || file.contains('/') || file.contains('\\') || file.contains("..") {
        return Err("非法的报告名".to_string());
    }
    fs::read_to_string(reports_dir().join(file)).map_err(|error| format!("读取报告失败：{error}"))
}

// ─── 目录工具 ────────────────────────────────────────────────

/// 递归复制目录（不跟符号链接 —— 挂载产物不能连带复制，那会把真身复制成副本）。
pub fn copy_dir_all(source: &Path, dest: &Path) -> Result<(), String> {
    fs::create_dir_all(dest).map_err(|error| format!("创建目录失败 {}：{error}", dest.display()))?;
    let entries = fs::read_dir(source).map_err(|error| format!("读取目录失败 {}：{error}", source.display()))?;
    for entry in entries.flatten() {
        let from = entry.path();
        let to = dest.join(entry.file_name());
        let meta = match fs::symlink_metadata(&from) {
            Ok(value) => value,
            Err(_) => continue,
        };
        if meta.file_type().is_symlink() {
            // 链接不复制：目标内容由中央真身负责，抄一份链接只会指回原处
            continue;
        }
        if meta.is_dir() {
            copy_dir_all(&from, &to)?;
        } else {
            fs::copy(&from, &to)
                .map_err(|error| format!("复制失败 {}：{error}", from.display()))?;
        }
    }
    Ok(())
}

/// 目录第一层的技能目录名（迁移判断 / 命名冲突时用）。
pub fn skill_names() -> Vec<String> {
    let entries = match fs::read_dir(skills_dir()) {
        Ok(value) => value,
        Err(_) => return Vec::new(),
    };
    let mut names: Vec<String> = entries
        .flatten()
        .filter(|entry| {
            fs::symlink_metadata(entry.path())
                .map(|meta| meta.is_dir() && !meta.file_type().is_symlink())
                .unwrap_or(false)
        })
        .map(|entry| entry.file_name().to_string_lossy().to_string())
        .collect();
    names.sort();
    names
}

/// 台账键集合（`Map` 便于按需查）。
pub fn manifest_names(manifest: &Value) -> Map<String, Value> {
    manifest
        .get("skills")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default()
}