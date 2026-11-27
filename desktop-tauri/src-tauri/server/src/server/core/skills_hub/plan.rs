//! 技能仓库 · 同步状态机（扫描 → 去重 → 预览 → 执行）。
//!
//! ```text
//!   扫各渠道目录 ──┐
//!                  ├─→ 去重（L1 合并 / L2 疑似 / L3 相似度提示）
//!   读台账      ──┘        │
//!                          ├─→ 预览（这次会做什么，一行不动）
//!                          └─→ 执行（收纳 → 挂载 → 冲突入库 → 清回收站 → 出报告）
//! ```
//!
//! ── 为什么「预览」与「执行」共用同一份规划 ──────────────────
//! 预览是给人看的，执行是真的动文件。两者若各算一遍，界面上说的和实际做的
//! 迟早不一致 —— 而这里的每个动作都会改用户磁盘。所以 `plan_actions()` 是
//! 唯一规划实现：`plan()` 把它压成界面要的 JSON，`execute()` 拿同一份动作
//! 逐条重放。
//!
//! ── 执行前为什么要重新规划 ──────────────────────────────────
//! 预览与执行之间可能过了几分钟（用户看完才点确认），磁盘状态早就变了（别处
//! 删了个技能、装了新客户端）。所以 `execute()` 拿**重新算的**动作执行，
//! 界面留下的预览只当「用户批准过那一类动作」的授权凭据 —— 从不按过期快照动手。
//!
//! ── 去重的三级，为什么只有一级是自动的 ──────────────────────
//! * **L1 内容树哈希一致** → 自动合并。同一份内容换个名字出现，「它们是一个
//!   技能」没有歧义，不需要人点。
//! * **L2 名称归一相同但内容不同** → 只进冲突队列。`gsap` 与 `gsap-v2` 归一后
//!   同名但内容不同，哪一个才是用户要的只有用户知道。
//! * **L3 文本相似度** → 默认关闭，开了也只提示。余弦 0.85 在「把 X 转成 Y」
//!   这类描述上误报率很高，自动合并会造成技能莫名消失。
//!
//! ── 孤儿与「中央未登记」是两种不同的东西 ────────────────────
//! * **孤儿**：工具目录里有、台账里没有任何记录。可能是用户刚拷进来的新技能，
//!   也可能是别的软件放的 —— 只标记，不动。
//! * **中央未登记**：`<hub>/skills/` 里有目录但台账没记（用户手动放进去的）。
//!   「纳管」只补台账、不动文件；悬空链接不能纳管。

use std::collections::{BTreeMap, VecDeque};
use std::fs;
use std::path::{Path, PathBuf};

use serde_json::{json, Map, Value};

use super::mount;
use super::scan;
use super::store;
use super::tools;

// ─── 去重 ────────────────────────────────────────────────────

/// 名称归一：`taste-skill` → `taste`，`gpt_tasteskill` → `gpt-taste`。
///
/// 只抹平「写法差异」而不是「语义差异」：空格与下划线统一成连字符、去掉结尾
/// 的 `skill(s)`、去掉 `-v2` 这类版本后缀。这三条覆盖社区里绝大多数「同一个
/// 技能换个名字出现」的写法。
pub fn normalize_name(name: &str) -> String {
    let lowered = name.trim().to_lowercase().replace([' ', '_'], "-");
    let mut text = lowered.as_str();
    if let Some(stripped) = text
        .strip_suffix("-skills")
        .or_else(|| text.strip_suffix("-skill"))
    {
        text = stripped;
    } else if let Some(stripped) = text
        .strip_suffix("skills")
        .or_else(|| text.strip_suffix("skill"))
    {
        if !stripped.is_empty() && !stripped.ends_with('-') {
            text = stripped;
        }
    }
    if let Some(at) = text.rfind("-v") {
        let tail = &text[at + 2..];
        if !tail.is_empty() && tail.chars().all(|ch| ch.is_ascii_digit()) {
            text = &text[..at];
        }
    }
    text.trim_matches('-').to_string()
}

/// 去重后的一个代表条目（`sources` 里是全部来源位置）。
struct Unique {
    name: String,
    dir: PathBuf,
    tool: String,
    skill_name: String,
    description: String,
    version: String,
    tree_hash: String,
    health: Vec<Value>,
    mtime_ms: i64,
    sources: Vec<Value>,
}

impl Unique {
    fn to_json(&self) -> Value {
        json!({
            "name": self.name,
            "dir": self.dir.to_string_lossy(),
            "skillName": self.skill_name,
            "description": self.description,
            "version": self.version,
            "treeHash": self.tree_hash,
            "health": self.health,
            "mtimeMs": self.mtime_ms,
            "sources": self.sources,
        })
    }
}

struct DedupeResult {
    unique: Vec<Unique>,
    duplicates: Vec<Value>,
    conflicts: Vec<Value>,
    hints: Vec<Value>,
}

/// 三级去重。
///
/// ── 空哈希为什么必须排除在 L1 之外 ──────────────────────────
/// 空目录（或缺 SKILL.md 的目录）`treeHash` 是空串。以空串为键，所有互不相干
/// 的空目录会被判成「同一个技能」，自动合并会把无辜目录备份进回收站再改成
/// 指向别人技能的链接 —— 一次同步就能毁掉一批目录。空哈希一律单独成条。
fn dedupe(skills: &[Value], l3_enabled: bool, l3_threshold: f64) -> DedupeResult {
    let mut unique: Vec<Unique> = Vec::new();
    let mut duplicates: Vec<Value> = Vec::new();
    let mut conflicts: Vec<Value> = Vec::new();
    let mut hints: Vec<Value> = Vec::new();
    let mut by_hash: BTreeMap<String, usize> = BTreeMap::new();

    for skill in skills {
        let name = text_of(skill, "name");
        let tool = text_of(skill, "tool");
        let dir_text = text_of(skill, "dir");
        let tree_hash = text_of(skill, "treeHash");
        let entry = Unique {
            dir: PathBuf::from(&dir_text),
            skill_name: {
                let declared = text_of(skill, "skillName");
                if declared.trim().is_empty() { name.clone() } else { declared }
            },
            description: text_of(skill, "description"),
            version: text_of(skill, "version"),
            tree_hash: tree_hash.clone(),
            health: skill.get("health").and_then(Value::as_array).cloned().unwrap_or_default(),
            mtime_ms: skill.get("mtimeMs").and_then(Value::as_i64).unwrap_or(0),
            sources: vec![json!({ "tool": tool, "name": name, "dir": dir_text })],
            name: name.clone(),
            tool: tool.clone(),
        };

        if tree_hash.is_empty() {
            unique.push(entry);
            continue;
        }
        match by_hash.get(&tree_hash).copied() {
            Some(at) => {
                let kept_name = unique[at].name.clone();
                let kept_tool = unique[at].tool.clone();
                unique[at]
                    .sources
                    .push(json!({ "tool": tool, "name": name, "dir": dir_text }));
                duplicates.push(json!({
                    "kept": { "name": kept_name, "tool": kept_tool },
                    "removed": { "name": name, "tool": tool },
                    "rule": "L1",
                    "basis": "内容树哈希一致",
                }));
            }
            None => {
                by_hash.insert(tree_hash, unique.len());
                unique.push(entry);
            }
        }
    }

    // L2：归一后同名、内容不同 → 冲突（不自动合并）
    let mut by_norm: BTreeMap<String, usize> = BTreeMap::new();
    for at in 0..unique.len() {
        let key = normalize_name(if unique[at].skill_name.is_empty() {
            &unique[at].name
        } else {
            &unique[at].skill_name
        });
        if key.is_empty() {
            continue;
        }
        match by_norm.get(&key).copied() {
            Some(first) => conflicts.push(json!({
                "id": format!("norm:{key}"),
                "kind": "norm",
                "key": key,
                "title": format!("{} ≈ {}", unique[first].name, unique[at].name),
                "detail": "名称归一后疑似同一技能，当前按两个技能分别收纳；如确认同一可合并",
                "a": unique[first].name,
                "b": unique[at].name,
            })),
            None => {
                by_norm.insert(key, at);
            }
        }
    }

    if l3_enabled {
        for left in 0..unique.len() {
            for right in (left + 1)..unique.len() {
                let sim = cosine(
                    &similar_text(&unique[left].skill_name, &unique[left].description),
                    &similar_text(&unique[right].skill_name, &unique[right].description),
                );
                if sim >= l3_threshold {
                    hints.push(json!({
                        "a": unique[left].name,
                        "b": unique[right].name,
                        "sim": (sim * 1000.0).round() / 1000.0,
                    }));
                }
            }
        }
    }

    DedupeResult { unique, duplicates, conflicts, hints }
}

fn similar_text(name: &str, description: &str) -> String {
    let raw = format!("{name} {description}").to_lowercase();
    let mut out = String::with_capacity(raw.len());
    for ch in raw.chars() {
        if ch.is_alphanumeric() || ('\u{4e00}'..='\u{9fa5}').contains(&ch) {
            out.push(ch);
        } else {
            out.push(' ');
        }
    }
    out
}

/// 词袋余弦相似度（去重 L3 用）。
fn cosine(left: &str, right: &str) -> f64 {
    let mut a: BTreeMap<&str, f64> = BTreeMap::new();
    let mut b: BTreeMap<&str, f64> = BTreeMap::new();
    for word in left.split_whitespace() {
        *a.entry(word).or_insert(0.0) += 1.0;
    }
    for word in right.split_whitespace() {
        *b.entry(word).or_insert(0.0) += 1.0;
    }
    let mut dot = 0.0;
    let mut na = 0.0;
    let mut nb = 0.0;
    for (word, x) in &a {
        na += x * x;
        if let Some(y) = b.get(word) {
            dot += x * y;
        }
    }
    for x in b.values() {
        nb += x * x;
    }
    if na == 0.0 || nb == 0.0 {
        return 0.0;
    }
    dot / (na * nb).sqrt()
}

// ─── 小工具 ──────────────────────────────────────────────────

fn text_of(value: &Value, key: &str) -> String {
    value.get(key).and_then(Value::as_str).unwrap_or("").to_string()
}

fn source_label(sources: &[Value]) -> String {
    sources
        .iter()
        .map(|source| {
            format!(
                "{}:{}",
                source.get("tool").and_then(Value::as_str).unwrap_or("?"),
                source.get("name").and_then(Value::as_str).unwrap_or("?")
            )
        })
        .collect::<Vec<String>>()
        .join(", ")
}

// ─── 调查（survey）───────────────────────────────────────────

struct Survey {
    targets: Vec<Value>,
    skills: Vec<Value>,
    dedupe: DedupeResult,
    manifest: Value,
    orphans: Vec<Value>,
    mount_health: Vec<Value>,
    hub_extra: Vec<Value>,
}

/// 全量调查：只读。仪表盘 / 技能库 / 去重页 / 预览都调它。
fn survey(config: &Value) -> Survey {
    let mut targets: Vec<Value> = Vec::new();
    let mut skills: Vec<Value> = Vec::new();
    for target in tools::resolve_targets(config) {
        let id = text_of(&target, "id");
        let dir_text = text_of(&target, "dir");
        if dir_text.is_empty() {
            continue;
        }
        let (mut found, _links) = scan::scan_channel(Path::new(&dir_text), &id);
        skills.append(&mut found);
        targets.push(json!({
            "id": id,
            "name": target.get("name").cloned().unwrap_or(Value::Null),
            "dir": dir_text,
            "custom": target.get("custom").cloned().unwrap_or(json!(false)),
        }));
    }

    let manifest = store::load_manifest();

    // 系统自带技能不参与去重收纳（不进中央、不挂载），墓碑里的技能也不再回来；
    // 指向别处的链接技能同样排除 —— 那是别人中枢挂进来的（见 `scan::scan_channel`），
    // 把它们纳入去重会把上百个技能整批复制进本机中央库，等于劫持别人的真身。
    let dedupe_input: Vec<Value> = skills
        .iter()
        .filter(|skill| !matches!(text_of(skill, "origin").as_str(), "system" | "link"))
        .filter(|skill| !store::is_deleted(&manifest, &text_of(skill, "name")))
        .cloned()
        .collect();

    let dedupe = dedupe(
        &dedupe_input,
        config.get("l3Enabled").and_then(Value::as_bool).unwrap_or(false),
        config.get("l3Threshold").and_then(Value::as_f64).unwrap_or(0.85),
    );

    // 孤儿：既不是台账里的技能，也不是任何已记录的来源名 / 挂载名。
    //
    // 链接技能不算孤儿：它们本来就不归本机台账管（真身在别的技能中枢），
    // 算成孤儿会让巡检永远红着，而「孤儿」这个词要提示的是「刚拷进来、还没
    // 纳管的新技能」，两者不是一件事。
    let mut known: Vec<String> = Vec::new();
    if let Some(map) = manifest.get("skills").and_then(Value::as_object) {
        for (name, entry) in map {
            known.push(name.clone());
            for source in entry.get("sources").and_then(Value::as_array).into_iter().flatten() {
                if let Some(text) = source.get("originalName").and_then(Value::as_str) {
                    known.push(text.to_string());
                }
            }
            for item in entry.get("mounts").and_then(Value::as_array).into_iter().flatten() {
                if let Some(text) = item.get("name").and_then(Value::as_str) {
                    known.push(text.to_string());
                }
            }
        }
    }
    let orphans: Vec<Value> = skills
        .iter()
        .filter(|skill| !matches!(text_of(skill, "origin").as_str(), "system" | "link"))
        .filter(|skill| !known.iter().any(|item| item == &text_of(skill, "name")))
        .map(|skill| {
            json!({
                "name": skill.get("name").cloned().unwrap_or(Value::Null),
                "tool": skill.get("tool").cloned().unwrap_or(Value::Null),
                "dir": skill.get("dir").cloned().unwrap_or(Value::Null),
                "mtimeMs": skill.get("mtimeMs").cloned().unwrap_or(json!(0)),
            })
        })
        .collect();

    let mount_health = mount::verify_all(&manifest);
    let hub_extra = hub_extra(&manifest);

    Survey { targets, skills, dedupe, manifest, orphans, mount_health, hub_extra }
}

/// 台账对不上磁盘的另一半：中央目录里有、台账没记的。
///
/// 两种：用户手动放进去的真身（可纳管），以及指向已不存在内容的悬空链接
/// （不可纳管 —— 纳管等于给一个空壳记账）。
fn hub_extra(manifest: &Value) -> Vec<Value> {
    let recorded = store::manifest_names(manifest);
    let mut rows: Vec<Value> = Vec::new();
    for name in store::skill_names() {
        if recorded.contains_key(&name) {
            continue;
        }
        let dir = store::skills_dir().join(&name);
        rows.push(json!({
            "name": name,
            "isLink": mount::is_link(&dir),
            "skillCount": scan::count_skill_dirs(&dir),
        }));
    }
    rows
}

/// 台账 ↔ 磁盘对账：磁盘上有链接但台账没记的补进去（只补不删）。
///
/// ── 为什么要这一步 ──────────────────────────────────────────
/// 台账丢过一次（旧实现合并写盘时抹掉 mounts），磁盘上的链接都在、账本却是空
/// 的 —— 表现是「所有技能都显示未挂载」，而同步把现存链接判成 already 不再补账，
/// 于是台账永远回不来。这里以磁盘为准补录，只补「指向中央真身」的链接；悬空
/// 链接不补，否则给已删技能复活记账。
fn reconcile_mounts(config: &Value, manifest: &mut Value) -> usize {
    let skills_root = store::skills_dir();
    let known = store::manifest_names(manifest);
    if known.is_empty() {
        return 0;
    }
    let mut added = 0usize;
    for target in tools::resolve_targets(config) {
        let id = text_of(&target, "id");
        let dir_text = text_of(&target, "dir");
        let entries = match fs::read_dir(&dir_text) {
            Ok(value) => value,
            Err(_) => continue,
        };
        for entry in entries.flatten() {
            let link_path = entry.path();
            if !mount::is_link(&link_path) {
                continue;
            }
            let target_dir = PathBuf::from(mount::link_target(&link_path));
            if target_dir.as_os_str().is_empty() || !target_dir.exists() {
                continue;
            }
            let parent_matches = target_dir
                .parent()
                .map(|parent| scan::path_key(parent) == scan::path_key(&skills_root))
                .unwrap_or(false);
            if !parent_matches {
                continue;
            }
            let skill = target_dir
                .file_name()
                .map(|value| value.to_string_lossy().to_string())
                .unwrap_or_default();
            if skill.is_empty() || !known.contains_key(&skill) {
                continue;
            }
            let link_key = scan::path_key(&link_path);
            let name = entry.file_name().to_string_lossy().to_string();
            let Some(slot) = manifest
                .get_mut("skills")
                .and_then(Value::as_object_mut)
                .and_then(|map| map.get_mut(&skill))
            else {
                continue;
            };
            let items = slot
                .as_object_mut()
                .map(|map| map.entry("mounts".to_string()).or_insert_with(|| json!([])))
                .and_then(Value::as_array_mut);
            let Some(items) = items else { continue };
            if items.iter().any(|item| {
                item.get("path")
                    .and_then(Value::as_str)
                    .map(|text| scan::path_key(Path::new(text)) == link_key)
                    .unwrap_or(false)
            }) {
                continue;
            }
            // 同一工具上同名的旧记录表示「用户明确停用过」，不自动恢复成启用
            if items.iter().any(|item| {
                item.get("tool").and_then(Value::as_str) == Some(&id)
                    && item.get("name").and_then(Value::as_str) == Some(&name)
            }) {
                continue;
            }
            items.push(json!({
                "tool": id,
                "name": name,
                "path": link_path.to_string_lossy(),
                "type": mount::link_type(),
                "enabled": true,
            }));
            added += 1;
        }
    }
    added
}

// ─── 规划 ────────────────────────────────────────────────────

/// 一个待执行动作（内部形态：`entry` 只在收纳时带，界面只看 `to_json`）。
struct Action {
    kind: &'static str,
    skill: String,
    tool: String,
    dir: PathBuf,
    mount_name: String,
    replace_real: bool,
    note: String,
    sources: Vec<Value>,
    entry: Option<Value>,
}

impl Action {
    fn to_json(&self) -> Value {
        json!({
            "kind": self.kind,
            "skill": self.skill,
            "tool": self.tool,
            "note": self.note,
            "mountName": self.mount_name,
            "dir": self.dir.to_string_lossy(),
            "sources": self.sources.iter()
                .map(|source| format!(
                    "{}:{}",
                    source.get("tool").and_then(Value::as_str).unwrap_or("?"),
                    source.get("name").and_then(Value::as_str).unwrap_or("?")
                ))
                .collect::<Vec<String>>(),
        })
    }
}

struct Planned {
    actions: Vec<Action>,
    conflicts: Vec<Value>,
    orphans: Vec<Value>,
    duplicates: Vec<Value>,
    hints: Vec<Value>,
    targets: Vec<Value>,
    mount_health: Vec<Value>,
    hub_extra: Vec<Value>,
    /// 去重前的原始扫描行，供 `library()` 复用（见 `plan_actions` 末尾的说明）。
    scanned: Vec<Value>,
}

/// 规划：**不改任何文件**（除了台账对账这一件幂等的自愈）。
fn plan_actions(config: &Value) -> Planned {
    let mut found = survey(config);
    if reconcile_mounts(config, &mut found.manifest) > 0 {
        let _ = store::save_manifest(&found.manifest);
    }
    let manifest = &found.manifest;
    let targets = &found.targets;
    let mut actions: Vec<Action> = Vec::new();
    let mut conflicts: Vec<Value> = Vec::new();
    let mut simulated: Vec<String> = Vec::new();
    // 「工具目录\0名字」已被来源原位占用：B 段不再重复排一次 mount
    let mut taken: Vec<String> = Vec::new();

    // ── A 段：中央没有的 → 收纳 + 来源原位转挂载 ─────────────
    for item in &found.dedupe.unique {
        let recorded = manifest
            .get("skills")
            .and_then(Value::as_object)
            .map(|map| map.contains_key(&item.name))
            .unwrap_or(false);
        if recorded {
            continue;
        }
        // 同名只能有一个进中央：L2 命中后归一相同的后来者在这里被判收纳冲突
        let can_import = !simulated.iter().any(|name| name == &item.name);
        if can_import {
            simulated.push(item.name.clone());
        }
        actions.push(Action {
            kind: "import",
            skill: item.name.clone(),
            tool: String::new(),
            dir: item.dir.clone(),
            mount_name: String::new(),
            replace_real: false,
            note: source_label(&item.sources),
            sources: item.sources.clone(),
            entry: Some(item.to_json()),
        });
        for source in &item.sources {
            let tool = text_of(source, "tool");
            let name = text_of(source, "name");
            let source_dir = PathBuf::from(text_of(source, "dir"));
            let parent = source_dir
                .parent()
                .map(|path| path.to_path_buf())
                .unwrap_or_default();
            actions.push(Action {
                kind: "mount",
                skill: item.name.clone(),
                tool: tool.clone(),
                dir: parent.clone(),
                mount_name: name.clone(),
                replace_real: true,
                note: format!("{tool}:{name} 原位转挂载"),
                sources: Vec::new(),
                entry: None,
            });
            if can_import {
                taken.push(format!("{}\u{0}{name}", parent.to_string_lossy()));
            }
        }
    }

    // ── B 段：中央已有（含本轮新收纳）→ 逐个渠道判定 ────────
    let recorded_names: Vec<String> = manifest
        .get("skills")
        .and_then(Value::as_object)
        .map(|map| map.keys().cloned().collect())
        .unwrap_or_default();
    let new_names: Vec<String> = found
        .dedupe
        .unique
        .iter()
        .filter(|item| !recorded_names.iter().any(|name| name == &item.name))
        .map(|item| item.name.clone())
        .collect();
    let mut all_names = recorded_names.clone();
    all_names.extend(new_names.iter().cloned());
    all_names.sort();
    all_names.dedup();

    for name in &all_names {
        let central = store::skills_dir().join(name);
        let is_new = new_names.iter().any(|item| item == name);
        if !is_new && !central.is_dir() {
            actions.push(Action {
                kind: "error",
                skill: name.clone(),
                tool: String::new(),
                dir: PathBuf::new(),
                mount_name: String::new(),
                replace_real: false,
                note: "中央真身丢失（被手动删除？），请从回收站还原或删除该条目".to_string(),
                sources: Vec::new(),
                entry: None,
            });
            continue;
        }
        let recorded_mounts: Vec<Value> = manifest
            .get("skills")
            .and_then(Value::as_object)
            .and_then(|map| map.get(name))
            .and_then(|entry| entry.get("mounts"))
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();

        for target in targets {
            let tool = text_of(target, "id");
            let tool_dir_text = text_of(target, "dir");
            let tool_dir = PathBuf::from(&tool_dir_text);
            if is_new && taken.iter().any(|key| *key == format!("{tool_dir_text}\u{0}{name}")) {
                continue;
            }
            // 用户在详情页手动停用的挂载不重新发布（否则开关形同虚设）
            if recorded_mounts.iter().any(|item| {
                item.get("tool").and_then(Value::as_str) == Some(&tool)
                    && item.get("enabled").and_then(Value::as_bool) == Some(false)
            }) {
                continue;
            }
            let link_path = tool_dir.join(name);
            if mount::is_link(&link_path) {
                if mount::points_to(&link_path, &central) && link_path.exists() {
                    // `tool` 要同时进结构体与 note，先备好文本再 move
                    let note = format!("{tool} 挂载有效");
                    actions.push(Action {
                        kind: "skip",
                        skill: name.clone(),
                        tool,
                        dir: tool_dir.clone(),
                        mount_name: name.clone(),
                        replace_real: false,
                        note,
                        sources: Vec::new(),
                        entry: None,
                    });
                } else if mount::is_link(&link_path)
                    && Path::new(&mount::link_target(&link_path)).join("SKILL.md").is_file()
                {
                    // 别的中枢在管的健康外链：不生成 diff-link 冲突。否则用户
                    // 点同步，处理页又冒出「链接指向异常」，两个软件抢同一条路径。
                    actions.push(Action {
                        kind: "skip",
                        skill: name.clone(),
                        tool,
                        dir: tool_dir.clone(),
                        mount_name: name.clone(),
                        replace_real: false,
                        note: format!("{tool} 是外部技能链接，不接管"),
                        sources: Vec::new(),
                        entry: None,
                    });
                } else {
                    let got = mount::link_target(&link_path);
                    let shown = if got.is_empty() { "（读不出）".to_string() } else { got };
                    conflicts.push(json!({
                        "id": format!("diff-link:{name}@{tool}"),
                        "kind": "diff-link",
                        "skill": name,
                        "toolId": tool,
                        "dir": tool_dir.to_string_lossy(),
                        "title": format!("{tool}:{name} 链接指向异常"),
                        "detail": format!("现有链接指向 {shown}，不是中央真身，需人工确认"),
                    }));
                }
                continue;
            }
            if link_path.exists() {
                let local_hash = scan::tree_hash(&link_path);
                let central_hash = scan::tree_hash(&central);
                if !local_hash.is_empty() && local_hash == central_hash {
                    actions.push(Action {
                        kind: "mount",
                        skill: name.clone(),
                        tool: tool.clone(),
                        dir: tool_dir.clone(),
                        mount_name: name.clone(),
                        replace_real: true,
                        note: format!("{tool} 版与中央一致，原位转挂载（原目录备份进回收站）"),
                        sources: Vec::new(),
                        entry: None,
                    });
                } else {
                    conflicts.push(json!({
                        "id": format!("content:{name}@{tool}"),
                        "kind": "content",
                        "skill": name,
                        "toolId": tool,
                        "dir": tool_dir.to_string_lossy(),
                        "title": format!("{tool}:{name} 内容冲突"),
                        "detail": "工具版与中央版内容不同，需人工裁决",
                    }));
                }
                continue;
            }
            actions.push(Action {
                kind: "mount",
                skill: name.clone(),
                tool: tool.clone(),
                dir: tool_dir.clone(),
                mount_name: name.clone(),
                replace_real: false,
                note: format!("{tool} 无此技能 → 发布挂载"),
                sources: Vec::new(),
                entry: None,
            });
        }
    }

    // L2 疑似项也要进队列（它们没有对应的磁盘动作，只是提示）
    for conflict in &found.dedupe.conflicts {
        if !conflicts.iter().any(|item| item.get("id") == conflict.get("id")) {
            conflicts.push(conflict.clone());
        }
    }

    Planned {
        actions,
        conflicts,
        orphans: found.orphans,
        duplicates: found.dedupe.duplicates,
        hints: found.dedupe.hints,
        targets: found.targets,
        mount_health: found.mount_health,
        hub_extra: found.hub_extra,
        // 原始扫描行（去重前、含同名变体与系统技能）带出去，让 `library()` 复用
        // 这一轮的结果 —— 不然它要再扫一遍目录、再算一遍内容树哈希，而两次扫描
        // 之间磁盘可能已经变了，最后拼出的列表与计数对不上。
        scanned: found.skills,
    }
}

/// 预览：把规划压成界面要的形状。不改任何文件。
pub fn plan(config: &Value) -> Value {
    plan_with(config, false)
}

/// 规划结果缓存：一个页签同时发多个请求时共用同一份规划。
///
/// ── 为什么按「请求批」缓存而不是按数据版本 ──────────────────
/// 打开一个页签会并发发 2~4 个请求（`overview` + `daily-check`，或
/// `library` + `daily-check`），每个都在内部跑一遍完整规划。规划本身要扫全部
/// 渠道目录、算内容树哈希、做 L1/L2/L3 去重 —— 本机一次 1s 上下。四个请求
/// 就是四遍，而它们要的其实是同一份结论。
///
/// 所以这里按 `PLAN_TTL` 短窗口复用：窗口内同一份配置直接返回上次结果。
/// 写操作（收纳 / 挂载 / 同步 / 改配置）都会 `clear_plan_cache()`，所以
/// 「改完立刻看」不会拿到旧结论；用户点「立即刷新」时传 `fresh = true`
/// 绕过缓存，那是他明确要求重扫的信号。
const PLAN_TTL: std::time::Duration = std::time::Duration::from_millis(2000);

struct CachedPlan {
    key: String,
    at: std::time::Instant,
    value: Value,
}

fn plan_cache() -> &'static std::sync::Mutex<Option<CachedPlan>> {
    static CACHE: std::sync::OnceLock<std::sync::Mutex<Option<CachedPlan>>> = std::sync::OnceLock::new();
    CACHE.get_or_init(|| std::sync::Mutex::new(None))
}

/// 丢掉规划缓存（写操作后调用）。
pub fn clear_plan_cache() {
    if let Ok(mut slot) = plan_cache().lock() {
        *slot = None;
    }
}

fn plan_with(config: &Value, fresh: bool) -> Value {
    let key = config.to_string();
    // `fresh` 的语义是「别用我之前算的」，不是「别用别人正在算的」：先作废旧结果，
    // 再走下面那条串行路径 —— 并发进来的第二个 fresh 请求就能复用第一个刚算出的
    // 结果，而不是陪着再扫一遍磁盘。
    if fresh {
        clear_plan_cache();
    }
    // 规划期间持锁：`fresh` 请求（巡检、立即刷新）在页签切换时可能同时进来
    // 两三个，不串行的话它们会各自跑一遍完整扫描 —— 那正是要消掉的浪费。
    let mut slot = match plan_cache().lock() {
        Ok(value) => value,
        // 锁中毒（别的线程 panic 过）时不阻塞功能，退化成不缓存。
        Err(_) => return plan_uncached(config),
    };
    if let Some(hit) = slot.as_ref() {
        if hit.key == key && hit.at.elapsed() < PLAN_TTL {
            return hit.value.clone();
        }
    }
    let value = plan_uncached(config);
    *slot = Some(CachedPlan { key, at: std::time::Instant::now(), value: value.clone() });
    value
}

/// 绕过缓存重新规划（用户点「立即刷新」/ 巡检要真实现场时用）。
pub fn plan_fresh(config: &Value) -> Value {
    plan_with(config, true)
}

fn plan_uncached(config: &Value) -> Value {
    let planned = plan_actions(config);
    let mode = mount_mode(config);
    let count = |kind: &str, replace: Option<bool>| -> usize {
        planned
            .actions
            .iter()
            .filter(|action| action.kind == kind && replace.map(|want| action.replace_real == want).unwrap_or(true))
            .count()
    };

    json!({
        "mode": mode,
        "actions": planned.actions.iter().map(Action::to_json).collect::<Vec<Value>>(),
        "counts": {
            "import": count("import", None),
            "replace": count("mount", Some(true)),
            "publish": count("mount", Some(false)),
            "skip": count("skip", None),
            "conflict": planned.conflicts.len(),
            "error": count("error", None),
        },
        "conflicts": planned.conflicts,
        "conflictsStored": store::load_conflicts(),
        "orphans": planned.orphans,
        "duplicates": planned.duplicates,
        "hints": planned.hints,
        "targets": planned.targets,
        "mountHealth": planned.mount_health,
        "hubExtra": planned.hub_extra,
        // 这一轮实际扫到的原始行。前端不用它（行数可能比列表多），是给
        // `library()` 复用同一轮扫描用的；空数组时 `library()` 会自动退回自己扫。
        "scanned": planned.scanned,
    })
}

fn mount_mode(config: &Value) -> String {
    config
        .get("mountMode")
        .and_then(Value::as_str)
        .filter(|text| *text == "copy")
        .unwrap_or("junction")
        .to_string()
}

// ─── 执行 ────────────────────────────────────────────────────

/// 执行同步：**重新规划**后逐条重放（不信任调用方传回的预览）。
pub fn execute(config: &Value) -> Value {
    let planned = plan_actions(config);
    let mode = mount_mode(config);
    let trash_days = config.get("trashDays").and_then(Value::as_i64).unwrap_or(7);
    let mut summary: Map<String, Value> = Map::new();
    for key in ["imported", "mounted", "merged", "skipped", "conflicts", "cleaned"] {
        summary.insert(key.to_string(), json!(0));
    }
    let mut imports: Vec<Value> = Vec::new();
    let mut merges: Vec<Value> = Vec::new();
    let mut mounts: Vec<Value> = Vec::new();
    let mut conflicts: Vec<Value> = planned.conflicts.clone();
    let mut diff: Vec<String> = Vec::new();

    let mut queue: VecDeque<Action> = planned.actions.into_iter().collect();
    while let Some(action) = queue.pop_front() {
        match action.kind {
            "import" => import_action(&action, &mut summary, &mut imports, &mut conflicts, &mut diff),
            "mount" => do_mount(
                &mode,
                &action,
                &mut summary,
                &mut mounts,
                &mut merges,
                &mut conflicts,
                &mut diff,
            ),
            "skip" => bump(&mut summary, "skipped"),
            "error" => {
                bump(&mut summary, "conflicts");
                conflicts.push(json!({
                    "kind": "missing",
                    "skill": action.skill,
                    "title": action.skill,
                    "detail": action.note,
                }));
            }
            _ => {}
        }
    }

    // 冲突落库 + 本轮不再成立的自动消失（norm 类保留到人裁决）
    let active_ids: Vec<String> = conflicts
        .iter()
        .filter_map(|item| item.get("id").and_then(Value::as_str).map(str::to_string))
        .collect();
    for item in &conflicts {
        if item.get("id").is_some() {
            let _ = store::upsert_conflict(item.clone());
        }
    }
    let _ = store::prune_conflicts(&active_ids);

    let cleaned = store::purge_trash(trash_days);
    summary.insert("cleaned".to_string(), json!(cleaned));

    // 收尾再对账一次：本轮新建 / 替换的链接若因中断没来得及记账，这里补齐
    let mut manifest = store::load_manifest();
    if reconcile_mounts(config, &mut manifest) > 0 {
        let _ = store::save_manifest(&manifest);
    }

    let report = render_report(&summary, &imports, &merges, &mounts, &conflicts, &diff);
    let report_file = store::write_report(&report).unwrap_or_default();

    json!({
        "summary": Value::Object(summary),
        "imports": imports,
        "merges": merges,
        "mounts": mounts,
        "conflicts": conflicts,
        "manifestDiff": diff,
        "reportFile": report_file,
        "mode": mode,
    })
}

fn bump(summary: &mut Map<String, Value>, key: &str) {
    let next = summary.get(key).and_then(Value::as_i64).unwrap_or(0) + 1;
    summary.insert(key.to_string(), json!(next));
}

fn import_action(
    action: &Action,
    summary: &mut Map<String, Value>,
    imports: &mut Vec<Value>,
    conflicts: &mut Vec<Value>,
    diff: &mut Vec<String>,
) {
    let entry = action.entry.clone().unwrap_or_else(|| json!({}));
    match store::import_skill(&entry) {
        Ok(result) => {
            let outcome = text_of(&result, "action");
            match outcome.as_str() {
                "imported" => {
                    bump(summary, "imported");
                    imports.push(json!({
                        "name": action.skill,
                        "sources": action.note,
                        "action": "收纳",
                        "path": store::skills_dir().join(&action.skill).to_string_lossy(),
                    }));
                    let hash = text_of(&result, "treeHash");
                    let short = &hash[..hash.len().min(12)];
                    diff.push(format!("+ skills/{} ← {} ({short})", action.skill, action.note));
                }
                "source-added" => imports.push(json!({
                    "name": action.skill,
                    "sources": action.note,
                    "action": "补充来源",
                    "path": store::skills_dir().join(&action.skill).to_string_lossy(),
                })),
                other => {
                    bump(summary, "conflicts");
                    conflicts.push(json!({
                        "kind": "content",
                        "skill": action.skill,
                        "title": format!("{} 收纳冲突", action.skill),
                        "detail": result.get("message").and_then(Value::as_str).unwrap_or(other),
                    }));
                }
            }
        }
        Err(message) => {
            bump(summary, "conflicts");
            conflicts.push(json!({
                "kind": "content",
                "skill": action.skill,
                "title": format!("{} 收纳失败", action.skill),
                "detail": message,
            }));
        }
    }
}

fn do_mount(
    mode: &str,
    action: &Action,
    summary: &mut Map<String, Value>,
    mounts: &mut Vec<Value>,
    merges: &mut Vec<Value>,
    conflicts: &mut Vec<Value>,
    diff: &mut Vec<String>,
) {
    if action.dir.as_os_str().is_empty() || !action.dir.is_dir() {
        mounts.push(json!({
            "skill": action.skill,
            "dir": action.dir.to_string_lossy(),
            "action": "mount",
            "outcome": "目录不存在，跳过",
        }));
        return;
    }
    let mount_name = if action.mount_name.is_empty() {
        action.skill.clone()
    } else {
        action.mount_name.clone()
    };
    let link_path = action.dir.join(&mount_name);
    let central = store::skills_dir().join(&action.skill);
    let real_exists = link_path.exists() && !mount::is_link(&link_path);

    if real_exists {
        if !action.replace_real {
            bump(summary, "conflicts");
            conflicts.push(json!({
                "id": format!("content:{}@{}", action.skill, action.tool),
                "kind": "content",
                "skill": action.skill,
                "toolId": action.tool,
                "dir": action.dir.to_string_lossy(),
                "title": format!("{mount_name} 挂载冲突"),
                "detail": "工具目录已有同名真实目录，需人工裁决",
            }));
            return;
        }
        // 备份前先确认中央真身在：先 toTrash 再发现挂不了，技能就从工具目录消失了
        if !central.is_dir() {
            bump(summary, "conflicts");
            conflicts.push(json!({
                "kind": "missing",
                "skill": action.skill,
                "title": format!("{} 无法原位转挂载", action.skill),
                "detail": format!("中央真身不存在：{}", central.display()),
            }));
            return;
        }
        // 覆盖前再算一次哈希，内容不一致绝不静默替换
        if scan::tree_hash(&link_path) != scan::tree_hash(&central) {
            bump(summary, "conflicts");
            let item = json!({
                "id": format!("content:{}@{}", action.skill, action.tool),
                "kind": "content",
                "skill": action.skill,
                "toolId": action.tool,
                "dir": action.dir.to_string_lossy(),
                "title": format!("{}:{} 内容冲突", action.tool, action.skill),
                "detail": "工具版与中央版内容不同，需人工裁决",
            });
            conflicts.push(item.clone());
            let _ = store::upsert_conflict(item);
            return;
        }
        match store::to_trash(&link_path, &mount_name) {
            Ok(backed) => {
                diff.push(format!(
                    "~ {mount_name} @ {} 原目录备份 → {}",
                    action.dir.display(),
                    backed.display()
                ));
                bump(summary, "merged");
                merges.push(json!({
                    "kept": action.skill,
                    "removed": format!("{}:{mount_name}", action.tool),
                    "basis": "内容一致，原位转挂载",
                }));
            }
            Err(message) => {
                bump(summary, "conflicts");
                conflicts.push(json!({
                    "skill": action.skill,
                    "title": format!("{mount_name} 备份失败"),
                    "detail": message,
                }));
                return;
            }
        }
    }

    let result = mount::mount(&action.skill, &action.dir, mode, &mount_name);
    let outcome = text_of(&result, "action");
    match outcome.as_str() {
        "mounted" | "copied" => {
            bump(summary, "mounted");
            mounts.push(json!({
                "skill": action.skill,
                "dir": action.dir.to_string_lossy(),
                "action": if action.replace_real && real_exists { "replace-mount" } else { "mount" },
                "outcome": if outcome == "copied" { "已复制（副本模式）" } else { "已挂载" },
                "message": result.get("message").cloned().unwrap_or(Value::Null),
            }));
            let kind = {
                let got = text_of(&result, "type");
                if got.is_empty() { mount::link_type().to_string() } else { got }
            };
            let _ = store::set_mount(&action.skill, &action.tool, &link_path, &kind, true);
            diff.push(format!(
                "+ mount {mount_name} @ {} → skills/{}",
                action.dir.display(),
                action.skill
            ));
        }
        "already" => {
            bump(summary, "skipped");
            mounts.push(json!({
                "skill": action.skill,
                "dir": action.dir.to_string_lossy(),
                "action": "mount",
                "outcome": "已是指向中央的挂载",
            }));
            let _ = store::set_mount(&action.skill, &action.tool, &link_path, mount::link_type(), true);
        }
        "conflict-real-dir" => {
            // 已在上面计过数，这里只补一行结果
            mounts.push(json!({
                "skill": action.skill,
                "dir": action.dir.to_string_lossy(),
                "action": "mount",
                "outcome": result
                    .get("message")
                    .and_then(Value::as_str)
                    .unwrap_or("目标位置已有真实目录"),
            }));
        }
        other => {
            bump(summary, "conflicts");
            conflicts.push(json!({
                "kind": if other == "conflict-diff-link" { "diff-link" } else { "content" },
                "id": format!("{other}:{}@{}", action.skill, action.tool),
                "skill": action.skill,
                "toolId": action.tool,
                "dir": action.dir.to_string_lossy(),
                "title": format!("{mount_name} 挂载异常"),
                "detail": result.get("message").and_then(Value::as_str).unwrap_or(other),
            }));
            mounts.push(json!({
                "skill": action.skill,
                "dir": action.dir.to_string_lossy(),
                "action": "mount",
                "outcome": result.get("message").and_then(Value::as_str).unwrap_or(other),
            }));
        }
    }
}

// ─── 裁决 ────────────────────────────────────────────────────

/// 裁决一条冲突。`choice` 的语义见各分支注释。
///
/// 支持的取值：`keepCentral`（= `keepHub`，保留中央版）/ `keepTool` / `keepBoth` /
/// `same` / `different` / `rebuildLink` / `adoptLink` / `ignore`。
pub fn resolve(id: &str, choice: &str, config: &Value) -> Result<Value, String> {
    let items = store::load_conflicts();
    let item = items
        .iter()
        .find(|item| item.get("id").and_then(Value::as_str) == Some(id))
        .cloned()
        .ok_or_else(|| "冲突不存在（可能已被处理）".to_string())?;
    let kind = text_of(&item, "kind");
    let mode = mount_mode(config);

    // L2 归一疑似 / 任意类的「忽略」：没有文件要动
    if kind == "norm" || choice == "ignore" {
        store::drop_conflict(id)?;
        let message = if kind == "norm" {
            if choice == "same" {
                "已确认这两个名字指同一技能（内容不同，两者仍各留一份真身）"
            } else {
                "已标记为不同技能"
            }
        } else {
            "已忽略这条冲突（没有动任何文件）"
        };
        return Ok(json!({ "resolved": true, "message": message }));
    }

    let skill = text_of(&item, "skill");
    let tool = {
        let got = text_of(&item, "toolId");
        if got.is_empty() { "custom".to_string() } else { got }
    };
    let dir = PathBuf::from(text_of(&item, "dir"));
    if skill.is_empty() || dir.as_os_str().is_empty() {
        return Err("冲突记录缺少技能名或目录".to_string());
    }
    let central = store::skills_dir().join(&skill);
    let tool_copy = dir.join(&skill);

    // ── diff-link：位置上是链接，但指错了地方 ────────────────
    if kind == "diff-link" {
        let existing = mount::link_target(&tool_copy);
        if choice == "rebuildLink" {
            if !central.is_dir() {
                return Err("中央真身不存在，先同步一次再重建".to_string());
            }
            if mount::is_link(&tool_copy) {
                mount::unmount(&tool_copy)?;
            }
            let result = mount::mount(&skill, &dir, &mode, &skill);
            if is_mounted(&result) {
                store::set_mount(&skill, &tool, &tool_copy, mount::link_type(), true)?;
                store::drop_conflict(id)?;
                return Ok(json!({ "resolved": true, "message": "已重建为指向中央真身的链接" }));
            }
            return Err(error_of(&result));
        }
        if choice == "adoptLink" {
            let source = PathBuf::from(&existing);
            if !source.is_dir() {
                return Err("这条链接指向的目标已不存在，只能重建或忽略".to_string());
            }
            let entry = scan_entry(&source, &skill)?;
            store::import_skill(&entry)?;
            if mount::is_link(&tool_copy) {
                mount::unmount(&tool_copy)?;
            }
            let result = mount::mount(&skill, &dir, &mode, &skill);
            if is_mounted(&result) {
                store::set_mount(&skill, &tool, &tool_copy, mount::link_type(), true)?;
                store::drop_conflict(id)?;
                return Ok(json!({
                    "resolved": true,
                    "message": "已把链接指向的那份收进中央，并把挂载改指向中央",
                }));
            }
            return Err(error_of(&result));
        }
        return Err(format!("链接指向异常只支持 rebuildLink / adoptLink，收到 {choice}"));
    }

    if !tool_copy.exists() {
        return Err("工具目录已不存在该技能".to_string());
    }

    match choice {
        // 保留中央版：工具版进回收站，原位改挂载
        "keepCentral" | "keepHub" => {
            if !central.is_dir() {
                return Err("中央版已不存在，无法保留中央版".to_string());
            }
            let backed = store::to_trash(&tool_copy, &skill)?;
            let result = mount::mount(&skill, &dir, &mode, &skill);
            if is_mounted(&result) {
                store::set_mount(&skill, &tool, &dir.join(&skill), mount::link_type(), true)?;
                store::drop_conflict(id)?;
                return Ok(json!({
                    "resolved": true,
                    "message": format!(
                        "已保留中央版并挂载；工具版在回收站 {}",
                        backed.file_name().map(|value| value.to_string_lossy().to_string()).unwrap_or_default()
                    ),
                }));
            }
            Err(error_of(&result))
        }
        // 用工具版覆盖中央：旧中央进回收站，工具版搬进中央，原位改挂载
        "keepTool" => {
            if central.exists() {
                store::to_trash(&central, &format!("{skill}-central"))?;
            }
            store::copy_dir_all(&tool_copy, &central)?;
            // 覆盖完立刻读：必须绕过缓存，否则拿到的是被覆盖掉的旧中央哈希。
            let hash = {
                let real = scan::tree_hash_fresh(&central);
                if real.is_empty() { scan::tree_hash_fresh(&tool_copy) } else { real }
            };
            let parsed = scan::parse_skill_md(&central).ok();
            let mut manifest = store::load_manifest();
            if let Some(map) = manifest.get_mut("skills").and_then(Value::as_object_mut) {
                map.insert(
                    skill.clone(),
                    json!({
                        "name": skill,
                        "skillName": parsed.as_ref().map(|item| item.name.clone()).filter(|text| !text.trim().is_empty()).unwrap_or_else(|| skill.clone()),
                        "description": parsed.as_ref().map(|item| item.description.clone()).unwrap_or_default(),
                        "version": parsed.as_ref().map(|item| item.version.clone()).unwrap_or_default(),
                        "treeHash": hash,
                        "sources": [{ "tool": tool, "originalName": skill, "firstSeen": store::now_iso() }],
                        "mounts": [],
                        "mergeHistory": [{ "at": store::now_iso(), "action": "conflict", "detail": format!("采纳 {tool} 版覆盖中央") }],
                    }),
                );
            }
            store::save_manifest(&manifest)?;
            store::to_trash(&tool_copy, &skill)?;
            let result = mount::mount(&skill, &dir, &mode, &skill);
            if is_mounted(&result) {
                store::set_mount(&skill, &tool, &dir.join(&skill), mount::link_type(), true)?;
                store::drop_conflict(id)?;
                return Ok(json!({ "resolved": true, "message": "已用工具版覆盖中央并挂载" }));
            }
            Err(error_of(&result))
        }
        // 双保留：工具版改名 `-<tool>` 收进中央，原位挂回中央版
        "keepBoth" => {
            let renamed = format!("{skill}-{tool}");
            let dest = store::skills_dir().join(&renamed);
            if dest.exists() {
                return Err(format!("中央已有「{renamed}」，先处理它再双保留"));
            }
            store::copy_dir_all(&tool_copy, &dest)?;
            let parsed = scan::parse_skill_md(&dest).ok();
            let mut manifest = store::load_manifest();
            if let Some(map) = manifest.get_mut("skills").and_then(Value::as_object_mut) {
                map.insert(
                    renamed.clone(),
                    json!({
                        "name": renamed,
                        "skillName": skill,
                        "description": parsed.as_ref().map(|item| item.description.clone()).unwrap_or_default(),
                        "version": parsed.as_ref().map(|item| item.version.clone()).unwrap_or_default(),
                        "treeHash": scan::tree_hash_fresh(&dest),
                        "sources": [{ "tool": tool, "originalName": skill, "firstSeen": store::now_iso() }],
                        "mounts": [],
                        "mergeHistory": [{ "at": store::now_iso(), "action": "conflict", "detail": format!("双保留改名自 {tool}:{skill}") }],
                    }),
                );
            }
            store::save_manifest(&manifest)?;
            store::to_trash(&tool_copy, &skill)?;
            if central.is_dir() {
                let result = mount::mount(&skill, &dir, &mode, &skill);
                if is_mounted(&result) {
                    store::set_mount(&skill, &tool, &dir.join(&skill), mount::link_type(), true)?;
                }
            }
            store::drop_conflict(id)?;
            Ok(json!({
                "resolved": true,
                "message": format!("已双保留：中央版 + 中央目录里的「{renamed}」"),
            }))
        }
        other => Err(format!("未知的裁决方式：{other}")),
    }
}

fn is_mounted(result: &Value) -> bool {
    matches!(text_of(result, "action").as_str(), "mounted" | "already" | "copied")
}

fn error_of(result: &Value) -> String {
    let message = text_of(result, "message");
    if message.is_empty() {
        format!("挂载失败（{}）", text_of(result, "action"))
    } else {
        format!("挂载失败：{message}")
    }
}

/// 把一个目录当「待收纳技能」打包（`adoptLink` 与纳管共用）。
fn scan_entry(dir: &Path, name: &str) -> Result<Value, String> {
    if !dir.is_dir() {
        return Err(format!("目录不存在：{}", dir.display()));
    }
    let parsed = scan::parse_skill_md(dir).ok();
    Ok(json!({
        "name": name,
        "dir": dir.to_string_lossy(),
        "skillName": parsed.as_ref().map(|item| item.name.clone()).filter(|text| !text.trim().is_empty()).unwrap_or_else(|| name.to_string()),
        "description": parsed.as_ref().map(|item| item.description.clone()).unwrap_or_default(),
        "version": parsed.as_ref().map(|item| item.version.clone()).unwrap_or_default(),
        "treeHash": scan::tree_hash(dir),
        "health": scan::health_check(dir),
        "sources": [{ "tool": "hub", "name": name, "dir": dir.to_string_lossy() }],
    }))
}

/// 纳管：中央目录里有、台账没记的，只补台账不动文件。
pub fn adopt(name: &str) -> Result<Value, String> {
    let name = name.trim();
    if name.is_empty() || name.contains('/') || name.contains('\\') || name.contains("..") {
        return Err("非法的技能名".to_string());
    }
    let dir = store::skills_dir().join(name);
    if !dir.is_dir() {
        return Err("中央仓库里没有这个目录".to_string());
    }
    if mount::is_link(&dir) {
        return Err("这是链接不是真身，不能纳管".to_string());
    }
    let entry = scan_entry(&dir, name)?;
    let result = store::import_skill(&entry)?;
    Ok(json!({
        "ok": true,
        "action": text_of(&result, "action"),
        "name": name,
    }))
}

/// 删除一个技能：真身进回收站、所有挂载摘掉、台账删记录 + 立墓碑。
///
/// 名字校验与 `adopt` 同一口径：这个函数会 **按名字拼出中央目录并搬走它**，
/// 漏一次校验就等于接受 `../` 之类的名字去动中央仓库外面的东西。
pub fn remove(name: &str) -> Result<Value, String> {
    let name = name.trim();
    if name.is_empty() || name.contains('/') || name.contains('\\') || name.contains("..") {
        return Err("非法的技能名".to_string());
    }
    let manifest = store::load_manifest();
    let entry = store::manifest_skill(&manifest, name).clone();
    let mut removed_mounts = 0usize;
    if let Some(items) = entry.get("mounts").and_then(Value::as_array) {
        for item in items {
            let path = PathBuf::from(item.get("path").and_then(Value::as_str).unwrap_or(""));
            if mount::is_link(&path) && mount::unmount(&path).is_ok() {
                removed_mounts += 1;
            }
        }
    }
    let central = store::skills_dir().join(name);
    let mut trashed = String::new();
    if central.exists() {
        let backed = store::to_trash(&central, name)?;
        trashed = backed.file_name().map(|value| value.to_string_lossy().to_string()).unwrap_or_default();
    }
    let mut next = store::load_manifest();
    if let Some(map) = next.get_mut("skills").and_then(Value::as_object_mut) {
        map.remove(name);
    }
    store::save_manifest(&next)?;
    // 墓碑：下一轮扫描看见工具目录里的残留副本也不会把它收回来
    store::tombstone(name, "用户删除")?;
    Ok(json!({ "name": name, "trash": trashed, "mounts": removed_mounts }))
}

/// 切换一处挂载的启用状态（技能详情页的开关）。
pub fn toggle_mount(name: &str, tool: &str, enabled: bool, config: &Value) -> Result<Value, String> {
    let mode = mount_mode(config);
    let manifest = store::load_manifest();
    let entry = store::manifest_skill(&manifest, name);
    let mounts = entry.get("mounts").and_then(Value::as_array).cloned().unwrap_or_default();
    let record = mounts
        .iter()
        .find(|item| item.get("tool").and_then(Value::as_str) == Some(tool))
        .cloned()
        .ok_or_else(|| format!("「{name}」没有登记到 {tool} 的挂载记录"))?;
    let path = PathBuf::from(record.get("path").and_then(Value::as_str).unwrap_or(""));
    if enabled {
        let parent = path
            .parent()
            .ok_or_else(|| "挂载路径没有父目录".to_string())?
            .to_path_buf();
        let mount_name = path
            .file_name()
            .map(|value| value.to_string_lossy().to_string())
            .unwrap_or_else(|| name.to_string());
        let result = mount::mount(name, &parent, &mode, &mount_name);
        if !is_mounted(&result) {
            return Err(error_of(&result));
        }
        store::set_mount(name, tool, &path, mount::link_type(), true)?;
        Ok(json!({ "ok": true, "enabled": true }))
    } else {
        if mount::is_link(&path) {
            mount::unmount(&path)?;
        }
        store::set_mount(name, tool, &path, mount::link_type(), false)?;
        Ok(json!({ "ok": true, "enabled": false }))
    }
}

/// 一键修复失效挂载。
///
/// 「失效」只算本程序该管的：链接丢了 / 指错且目标不是活技能目录。指向别的
/// 中枢的健康外链不在这里抢（见 `mount::repair` 里的 external 分支）。
pub fn repair(config: &Value) -> Value {
    let manifest = store::load_manifest();
    mount::repair(&manifest, &mount_mode(config))
}

/// 解除登记：只删台账记录，不动磁盘上的链接。
///
/// 用于「这个位置的链接是别的中枢在管的」—— 摘掉重挂会被抢回去，两个软件
/// 抢同一条路径。解除后 Agent2API 不再把它算作自己的挂载，也不再报失效。
pub fn unregister(name: &str, tool: &str) -> Result<Value, String> {
    let name = name.trim();
    let tool = tool.trim();
    if name.is_empty() || tool.is_empty() {
        return Err("缺少技能名或渠道".to_string());
    }
    let manifest = store::load_manifest();
    let mounts = manifest
        .get("skills")
        .and_then(Value::as_object)
        .and_then(|map| map.get(name))
        .and_then(|entry| entry.get("mounts"))
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let record = mounts
        .iter()
        .find(|item| item.get("tool").and_then(Value::as_str) == Some(tool))
        .cloned()
        .ok_or_else(|| format!("「{name}」没有登记到 {tool} 的挂载记录"))?;
    let path = PathBuf::from(record.get("path").and_then(Value::as_str).unwrap_or(""));
    // 只删台账；链接本身是别的中枢的资产，不动。
    store::drop_mount(name, tool, &path)?;
    Ok(json!({
        "ok": true,
        "name": name,
        "tool": tool,
        "path": path.to_string_lossy(),
        "message": format!("已解除 {name} 在 {tool} 的挂载登记（磁盘链接未动）"),
    }))
}

// ─── 日报 ────────────────────────────────────────────────────

fn render_report(
    summary: &Map<String, Value>,
    imports: &[Value],
    merges: &[Value],
    mounts: &[Value],
    conflicts: &[Value],
    diff: &[String],
) -> String {
    let num = |key: &str| summary.get(key).and_then(Value::as_i64).unwrap_or(0);
    let mut out = String::new();
    out.push_str("# 技能仓库 · 同步报告\n\n");
    out.push_str(&format!("- 时间：{}\n", store::now_iso()));
    out.push_str(&format!("- 中央仓库：{}\n", store::hub_dir().display()));
    out.push_str(&format!(
        "- 收进中央 {} · 挂载变更 {} · 合并重复 {} · 跳过 {} · 冲突 {} · 清理回收站 {}\n\n",
        num("imported"),
        num("mounted"),
        num("merged"),
        num("skipped"),
        num("conflicts"),
        num("cleaned")
    ));

    let section = |title: &str, rows: Vec<String>| -> String {
        if rows.is_empty() {
            return String::new();
        }
        let mut block = format!("## {title}\n\n");
        for row in rows {
            block.push_str(&format!("- {row}\n"));
        }
        block.push('\n');
        block
    };

    out.push_str(&section(
        "收进中央",
        imports
            .iter()
            .map(|item| {
                format!(
                    "**{}** ← {}（{}）",
                    text_of(item, "name"),
                    text_of(item, "sources"),
                    text_of(item, "action")
                )
            })
            .collect(),
    ));
    out.push_str(&section(
        "合并",
        merges
            .iter()
            .map(|item| {
                format!(
                    "保留 `{}`，移除 {}（{}）",
                    text_of(item, "kept"),
                    text_of(item, "removed"),
                    text_of(item, "basis")
                )
            })
            .collect(),
    ));
    out.push_str(&section(
        "挂载",
        mounts
            .iter()
            .map(|item| {
                format!(
                    "`{}` @ `{}` → {}",
                    text_of(item, "skill"),
                    text_of(item, "dir"),
                    text_of(item, "outcome")
                )
            })
            .collect(),
    ));
    out.push_str(&section(
        "待裁决",
        conflicts
            .iter()
            .map(|item| format!("{}：{}", text_of(item, "title"), text_of(item, "detail")))
            .collect(),
    ));
    if !diff.is_empty() {
        out.push_str("## 台账变更\n\n```text\n");
        for line in diff {
            out.push_str(line);
            out.push('\n');
        }
        out.push_str("```\n");
    }
    out
}