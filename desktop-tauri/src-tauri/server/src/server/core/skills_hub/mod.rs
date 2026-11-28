//! 技能仓库（Resources → Skills Hub）。
//!
//! ── 这个模块解决什么问题 ────────────────────────────────────
//! 装了七八个 AI 编程客户端之后，同一个技能会在每个客户端的 `skills/` 里各存
//! 一份副本：改一处，另外七处就旧了；删一处，另外七处还在。用户每次都要回答
//! 「我现在改的是哪一份」。
//!
//! 技能仓库把这些副本收成**一份真身**放在中央目录，各客户端目录里只留指向它的
//! 链接。于是「改一处等于改全部」，而磁盘上只有一份内容。
//!
//! ```text
//! 渠道目录（各客户端的 skills/）     中央仓库（<数据目录>/skills-hub/）
//!   ~/.claude/skills/gsap   ──┐
//!   ~/.codex/skills/gsap    ──┼──►  skills/gsap/   ← 唯一真身
//!   ~/.trae/skills/gsap     ──┘      manifest.json ← 台账（来源/挂载/历史）
//! ```
//!
//! ── 分层与读写边界 ──────────────────────────────────────────
//! * `scan`  **只读**：解析 SKILL.md、算内容树哈希、体检。
//! * `tools` 渠道注册表 + 配置读写（配置只存用户改过的覆盖）。
//! * `store` 台账 / 回收站 / 冲突队列 / 日报（写盘一律 tmp + rename）。
//! * `mount` 链接的建立、校验、摘除（摘除只删链接本身）。
//! * `plan`  状态机：调查 → 去重 → 规划 → 执行 → 裁决。
//!
//! 这条边界让「扫描」可以被仪表盘、技能库、去重页、预览反复调用而不产生任何
//! 副作用 —— 用户点「扫描」不会悄悄改磁盘。
//!
//! ── 与「Agent 配置」页的区别 ────────────────────────────────
//! 「Agent 配置」管的是**客户端**（装没装、配置写没写），本模块管的是**技能**
//! （各客户端目录里的技能怎么收敛成一份）。两者共用渠道概念，但一个动配置
//! 文件、一个动技能目录，互不越界。
//!
//! ── 为什么第一屏要算「份消除」而不是「技能数」──────────────
//! 技能数只能说明「有东西」，说明不了「收敛做得怎么样」。`l1Merged` 是这一轮
//! 扫描里被合并掉的冗余副本数 —— 它才是这个功能创造的价值，也是用户判断
//! 「还需要做什么」的依据。

mod mount;
mod plan;
mod scan;
mod schedule;
mod store;
mod tools;

use serde_json::{json, Value};

pub use plan::{normalize_name, plan as sync_plan};
pub use schedule::spawn as spawn_daily_check;
pub use tools::{default_config, KV_KEY};

/// 磁盘或配置被改过之后，把两层缓存一起作废。
///
/// ── 为什么必须成对清 ────────────────────────────────────────
/// 这里有两层缓存，寿命不同：
/// * `scan` 的内容事实（哈希 / 描述 / 体检），按技能真实路径存；
/// * `plan` 的规划结论（待收纳 / 冲突 / 孤儿 / 挂载健康），2 秒窗口。
///
/// 写操作后只清前者的话，紧接着的列表请求会命中后者 —— 界面上就是「刚同步完，
/// 待收纳数还是老的」。这类不一致最难查（数据其实已经对了，只是被旧结论盖住），
/// 所以两层的失效一律走这一个入口，不在调用点各写一遍。
fn invalidate() {
    scan::clear_cache();
    plan::clear_plan_cache();
}

/// 配置读取（整份生效值：默认 + 覆盖 + 派生字段）。
pub fn config() -> Value {
    tools::load_config()
}

/// 保存配置（部分更新，返回整份生效值）。
pub fn save_config(patch: &Value) -> Result<Value, String> {
    let current = tools::load_config();
    let next = merge(&current, patch);
    let out = tools::save_config(&next);
    // `hubDir` / `customDirs` / `l3*` 都会改变「扫哪些目录、怎么去重」，
    // 缓存里的内容事实还是对的，但整份规划结论已经过期 —— 两层一起清。
    invalidate();
    out
}

/// 恢复默认（只重置传进来的键；`None` = 全量重置）。
pub fn reset_config(keys: Option<&[String]>) -> Result<Value, String> {
    let defaults = tools::default_config();
    let current = tools::load_config();
    let next = match keys {
        Some(list) if !list.is_empty() => {
            let mut next = current.clone();
            for key in list {
                let value = defaults.get(key).cloned().unwrap_or(Value::Null);
                if value.is_null() {
                    if let Some(map) = next.as_object_mut() {
                        map.remove(key);
                    }
                } else if let Some(map) = next.as_object_mut() {
                    map.insert(key.clone(), value);
                }
            }
            next
        }
        _ => defaults,
    };
    let out = tools::save_config(&next);
    invalidate();
    out
}

/// 一层浅合并（本模块的配置没有嵌套多层的键，`tools` / `customDirs` 都是整体替换）。
fn merge(base: &Value, patch: &Value) -> Value {
    let mut next = base.as_object().cloned().unwrap_or_default();
    if let Some(incoming) = patch.as_object() {
        for (key, value) in incoming {
            if value.is_null() {
                next.remove(key);
            } else {
                next.insert(key.clone(), value.clone());
            }
        }
    }
    Value::Object(next)
}

// ─── 总览（仪表盘）───────────────────────────────────────────

/// 仪表盘的数据源：一次扫描把页面要的计数、渠道矩阵、冲突、孤儿都算出来。
///
/// ── 为什么总览也调完整 `plan` ──────────────────────────────
/// 用户打开页面最想看的两个数字是「这次能收几个」和「有几个要裁决」，这两个
/// 只有规划算得出来。多走一遍规划换来「首屏就是准确的待办数」，比先给一个
/// 乐观数字再让他点开预览更省事。
pub fn overview() -> Value {
    let config = tools::load_config();
    let planned = sync_plan(&config);
    let manifest = store::load_manifest();
    let skills_root = store::skills_dir();

    let tools_rows: Vec<Value> = tools::list_tools(&config)
        .into_iter()
        .map(|row| {
            let id = row.get("id").and_then(Value::as_str).unwrap_or("");
            let dir = row.get("dir").and_then(Value::as_str).unwrap_or("");
            let skill_count =
                if dir.is_empty() { 0 } else { scan::count_channel_skills(std::path::Path::new(dir)) };
            let mounts = planned
                .get("mountHealth")
                .and_then(Value::as_array)
                .map(|list| {
                    list.iter()
                        .filter(|item| item.get("tool").and_then(Value::as_str) == Some(id))
                        .filter(|item| item.get("valid").and_then(Value::as_bool) == Some(true))
                        .count()
                })
                .unwrap_or(0);
            json!({
                "id": row.get("id").cloned().unwrap_or(Value::Null),
                "name": row.get("name").cloned().unwrap_or(Value::Null),
                "builtin": row.get("builtin").cloned().unwrap_or(json!(false)),
                "enabled": row.get("enabled").cloned().unwrap_or(json!(true)),
                "dir": row.get("dir").cloned().unwrap_or(Value::Null),
                "candidatePaths": row.get("candidatePaths").cloned().unwrap_or(json!([])),
                "skillCount": skill_count,
                "mountCount": mounts,
            })
        })
        .collect();

    let manifest_map = store::manifest_names(&manifest);
    // 页头「N 个技能」要回答的是「本机到底有几个技能」（= 技能库列表的「全部」）：
    // 本轮扫到的（含链接挂进来的）与台账里的取并集。中央真身目录里的数量仍按
    // `skillCount` 单独报，两者不是一回事。
    let scanned_rows: Vec<Value> = planned
        .get("scanned")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let mut visible: Vec<String> = Vec::new();
    for item in &scanned_rows {
        let name = text(item, "name");
        if !name.is_empty() && !visible.contains(&name) {
            visible.push(name);
        }
    }
    for name in manifest_map.keys() {
        if !visible.contains(name) {
            visible.push(name.clone());
        }
    }
    let visible_count = visible.len();
    // ── 来源副本数：只数「参与去重收纳的真实目录」──────────────────
    //
    // 教训（2026-11-28，用户报「挂载去重显示 107，和中央技能库一样」）：
    // 这里原来数的是 `scanned` 的**每一行**，而 `scan_channel` 为了让用户看见
    // 链路技能，会把 junction 也产出一行（`origin == "link"`）。于是 107 个
    // 链接在 `.workbuddy` 与 `.agents` 两个渠道各算一份 = 214 份「来源副本」，
    // `deduped = 214 − 107 = 107` 恰好等于技能数 —— 这个「份消除 107」纯属
    // 公式被 link 行污染，磁盘上根本没有任何副本被合并（真实目录 0 个、
    // 中央真身 2 个、duplicates 为空）。
    //
    // 对齐 AgentHub（`syncer.survey`）：它的 `scanDir` 遇到 junction 只往
    // `mounts` 塞、`continue` 掉，所以 `sourceCount = scanned.skills.length`
    // 天然不含链接；`l1Merged = dedup.duplicates.length` 是真实合并对数。
    // 两个数字必须同源同口径，否则「份消除」这个数永远不可信。
    //
    // 排除口径与 `plan()` 的 `dedupe_input` 完全一致（system / link 都不参与
    // 去重收纳），这样 `sourceCount − 真身数` 才是真正的「消除了几份」。
    let is_dedup_source = |item: &Value| {
        let name = text(item, "name");
        !name.is_empty() && !matches!(text(item, "origin").as_str(), "system" | "link")
    };
    let mut source_count: usize = 0;
    for item in &scanned_rows {
        if is_dedup_source(item) {
            source_count += 1;
        }
    }
    for (name, entry) in &manifest_map {
        // 本轮扫到真实目录的，已经在上面按行数过一遍了（一行 = 一份副本）
        if scanned_rows
            .iter()
            .any(|item| is_dedup_source(item) && text(item, "name") == *name)
        {
            continue;
        }
        let registered = entry
            .get("sources")
            .and_then(Value::as_array)
            .map(|list| list.len())
            .unwrap_or(0);
        source_count += registered.max(1);
    }

    let dedupe_merged = planned
        .get("duplicates")
        .and_then(Value::as_array)
        .map(|list| list.len())
        .unwrap_or(0);
    let conflicts = planned
        .get("conflictsStored")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let orphans = planned.get("orphans").and_then(Value::as_array).cloned().unwrap_or_default();
    let hub_extra = planned.get("hubExtra").and_then(Value::as_array).cloned().unwrap_or_default();
    let mount_health = planned.get("mountHealth").and_then(Value::as_array).cloned().unwrap_or_default();
    // 「失效」= 不是有效挂载，也**不是外部链接**。外链是别的中枢在管的健康
    // 链接（链接在、目标是活技能目录、只是不指本机中央），算进失效会让
    // 一键修复去抢那个中枢的路径，修完又被抢回来。
    let broken = mount_health
        .iter()
        .filter(|item| {
            item.get("valid").and_then(Value::as_bool) != Some(true)
                && item.get("external").and_then(Value::as_bool) != Some(true)
        })
        .count();

    json!({
        "hubDir": store::hub_dir().to_string_lossy(),
        "skillsDir": skills_root.to_string_lossy(),
        "manifestCount": manifest_map.len(),
        "skillCount": store::skill_names().len(),
        "visibleSkills": visible_count,
        "sourceCount": source_count,
        "l1Merged": dedupe_merged,
        "brokenMounts": broken,
        "mountHealth": mount_health,
        "pendingConflicts": conflicts,
        "orphans": orphans,
        "hubExtra": hub_extra,
        "tools": tools_rows,
        "recentReports": store::list_reports(8),
        "counts": planned.get("counts").cloned().unwrap_or(json!({})),
        "mode": planned.get("mode").cloned().unwrap_or(json!("junction")),
        "trash": store::list_trash(config.get("trashDays").and_then(Value::as_i64).unwrap_or(7))
            .len(),
    })
}

// ─── 技能库列表 ──────────────────────────────────────────────

/// 技能库的一行。
///
/// ── 为什么要把「台账里的」与「扫描到的」并起来 ───────────────
/// 收纳之后，工具目录里只剩链接，扫描层不再产出技能行 —— 只读扫描结果的话，
/// 收纳一次技能就从列表里消失了。所以两源合并：扫描到的（待收纳 / 冲突中）+
/// 台账里的（已收纳，含没挂载的）。
pub fn library() -> Value {
    let config = tools::load_config();
    let planned = sync_plan(&config);
    let manifest = store::load_manifest();

    // 「名称\0路径」→ (是否有效, 是否外部链接)。前端要分开
    // 「已启用但链接失效」「真有效」「别的中枢在管的外链」三种。
    let health: Vec<(String, bool, bool)> = planned
        .get("mountHealth")
        .and_then(Value::as_array)
        .map(|list| {
            list.iter()
                .map(|item| {
                    (
                        format!("{}\u{0}{}", text(item, "skill"), text(item, "path")),
                        item.get("valid").and_then(Value::as_bool).unwrap_or(false),
                        item.get("external").and_then(Value::as_bool).unwrap_or(false),
                    )
                })
                .collect()
        })
        .unwrap_or_default();
    let is_valid = |skill: &str, path: &str| -> bool {
        let key = format!("{skill}\u{0}{path}");
        health.iter().any(|(item, ok, _)| *item == key && *ok)
    };
    let is_external = |skill: &str, path: &str| -> bool {
        let key = format!("{skill}\u{0}{path}");
        health.iter().any(|(item, _, ext)| *item == key && *ext)
    };

    // ── 扫描行也要带挂载事实 ──────────────────────────────────
    //
    // 共享真身（`skillsDir` 指向现成仓库）之后，渠道目录里的链接目标就是本机
    // 中央目录，本该显示「已挂载」；但扫描层只产出 `origin == "link"` 的一行、
    // `mounts` 一律为空，而卡片判「已挂载」看的正是 `mounts[].valid` —— 于是
    // 用户看到 108 个技能全「未挂载」，AgentHub 里却全「已挂载」。
    //
    // 这里按链接的真实落点补一条挂载记录：落在本机中央 → valid；落在别处 →
    // external（别的中枢管的链接，不改不抢，只标来路）。
    let central_root = store::skills_dir();
    let central_key = match std::fs::canonicalize(&central_root) {
        Ok(real) => scan::path_key(&real),
        Err(_) => scan::path_key(&central_root),
    };
    let mount_prefix = format!("{}{}", central_key, std::path::MAIN_SEPARATOR);
    let link_mount = |row: &Value| -> Option<Value> {
        let target = text(row, "linkTarget");
        if target.is_empty() {
            return None;
        }
        let target_key = match std::fs::canonicalize(&target) {
            Ok(real) => scan::path_key(&real),
            Err(_) => scan::path_key(std::path::Path::new(&target)),
        };
        let inside = target_key == central_key || target_key.starts_with(mount_prefix.as_str());
        Some(json!({
            "tool": text(row, "tool"),
            "name": text(row, "name"),
            "path": text(row, "dir"),
            "target": target,
            "valid": inside,
            "external": !inside,
        }))
    };
    // 同一技能在多个渠道各有一条链接时，两行会合并成一行 —— 挂载也要合并，
    // 按链接路径去重，否则只看得到最后一个渠道。
    let push_mount = |list: &mut Vec<Value>, item: Value| {
        let path = text(&item, "path");
        if !list.iter().any(|row| text(row, "path") == path) {
            list.push(item);
        }
    };

    let mut rows: Vec<Value> = Vec::new();
    for action in planned.get("actions").and_then(Value::as_array).into_iter().flatten() {
        if text(action, "kind") != "import" {
            continue;
        }
        let name = text(action, "skill");
        rows.push(json!({
            "name": name,
            "inManifest": false,
            "origin": "channel",
            "mounts": [],
            "sources": action.get("sources").cloned().unwrap_or(json!([])),
        }));
    }

    // 扫描层里的独立技能（含同名不同容的多个变体，界面按行分开）
    //
    // 复用 `plan` 这一轮已经扫过、算过哈希的结果：再调一次 `scan_all()` 会把
    // 每个技能目录再走一遍并重算内容树哈希（技能多时这是这一页最贵的一段），
    // 更糟的是两次扫描之间磁盘可能被别的程序改过，拼出的行与 counts 对不上。
    // 只有 `plan` 没给（理论上不会）时才自己扫。
    let scanned = planned
        .get("scanned")
        .and_then(Value::as_array)
        .cloned()
        .filter(|list| !list.is_empty())
        .unwrap_or_else(|| scan_all(&config));
    for skill in &scanned {
        let name = text(skill, "name");
        let dir = text(skill, "dir");
        // 扫描层的 origin 只有三种：system（客户端自带）/ link（别的中枢挂进来
        // 的链接，真身不归本机管）/ 其余按 channel 显示。
        let origin = match text(skill, "origin").as_str() {
            "system" => "system",
            "link" => "link",
            _ => "channel",
        };
        let existing = rows.iter_mut().find(|row| text(row, "name") == name);
        if let Some(row) = existing {
            row["description"] = skill.get("description").cloned().unwrap_or(json!(""));
            row["version"] = skill.get("version").cloned().unwrap_or(json!(""));
            row["treeHash"] = skill.get("treeHash").cloned().unwrap_or(json!(""));
            row["health"] = skill.get("health").cloned().unwrap_or(json!([]));
            row["mtimeMs"] = skill.get("mtimeMs").cloned().unwrap_or(json!(0));
            row["origin"] = json!(origin);
            row["tool"] = skill.get("tool").cloned().unwrap_or(Value::Null);
            row["dir"] = json!(dir);
            row["linkTarget"] = skill.get("linkTarget").cloned().unwrap_or(json!(""));
            if origin == "link" {
                if let Some(item) = link_mount(skill) {
                    if !row["mounts"].is_array() {
                        row["mounts"] = json!([]);
                    }
                    if let Some(list) = row["mounts"].as_array_mut() {
                        push_mount(list, item);
                    }
                }
            }
            continue;
        }
        rows.push(json!({
            "name": name,
            "inManifest": manifest
                .get("skills")
                .and_then(Value::as_object)
                .map(|map| map.contains_key(&name))
                .unwrap_or(false),
            "origin": origin,
            "linkTarget": skill.get("linkTarget").cloned().unwrap_or(json!("")),
            "skillName": skill.get("skillName").cloned().unwrap_or(Value::Null),
            "description": skill.get("description").cloned().unwrap_or(json!("")),
            "version": skill.get("version").cloned().unwrap_or(json!("")),
            "treeHash": skill.get("treeHash").cloned().unwrap_or(json!("")),
            "health": skill.get("health").cloned().unwrap_or(json!([])),
            "mtimeMs": skill.get("mtimeMs").cloned().unwrap_or(json!(0)),
            "tool": skill.get("tool").cloned().unwrap_or(Value::Null),
            "dir": json!(dir),
            "sources": [{ "tool": text(skill, "tool"), "name": name }],
            "mounts": if origin == "link" {
                link_mount(skill).into_iter().collect::<Vec<Value>>()
            } else {
                Vec::new()
            },
        }));
    }

    // 台账里但扫描不到的（收纳后工具目录只剩链接）
    if let Some(map) = manifest.get("skills").and_then(Value::as_object) {
        for (name, entry) in map {
            if rows.iter().any(|row| text(row, "name") == *name) {
                continue;
            }
            let dir = store::skills_dir().join(name);
            let exists = dir.is_dir();
            let mounts: Vec<Value> = entry
                .get("mounts")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default()
                .into_iter()
                .map(|item| {
                    let path = text(&item, "path");
                    let mut next = item;
                    next["valid"] = json!(is_valid(name, &path));
                    next["external"] = json!(is_external(name, &path));
                    next
                })
                .collect();
            rows.push(json!({
                "name": name,
                "inManifest": true,
                "origin": "hub",
                "skillName": entry.get("skillName").cloned().unwrap_or(json!(name)),
                "description": entry.get("description").cloned().unwrap_or(json!("")),
                "version": entry.get("version").cloned().unwrap_or(json!("")),
                "treeHash": entry.get("treeHash").cloned().unwrap_or(json!("")),
                "health": if exists {
                    json!(scan::health_check(&dir))
                } else {
                    json!([{ "level": "bad", "text": "中央真身丢失，去回收站还原或删除该条目" }])
                },
                "mtimeMs": 0,
                "mounts": mounts,
                "sources": entry.get("sources").cloned().unwrap_or(json!([])),
            }));
        }
    }

    // 中央未登记（用户手动放进中央仓库的）
    for extra in planned.get("hubExtra").and_then(Value::as_array).into_iter().flatten() {
        let name = text(extra, "name");
        if rows.iter().any(|row| text(row, "name") == name) {
            continue;
        }
        rows.push(json!({
            "name": name,
            "inManifest": false,
            "origin": "hub-extra",
            "isLink": extra.get("isLink").cloned().unwrap_or(json!(false)),
            "mounts": [],
            "sources": [],
            "health": [],
        }));
    }

    json!({
        "items": rows,
        "total": rows.len(),
        "counts": planned.get("counts").cloned().unwrap_or(json!({})),
    })
}

fn scan_all(config: &Value) -> Vec<Value> {
    let mut out: Vec<Value> = Vec::new();
    for target in tools::resolve_targets(config) {
        let id = target.get("id").and_then(Value::as_str).unwrap_or("");
        let dir = target.get("dir").and_then(Value::as_str).unwrap_or("");
        if dir.is_empty() {
            continue;
        }
        let (mut skills, _links) = scan::scan_channel(std::path::Path::new(dir), id);
        out.append(&mut skills);
    }
    out
}

// ─── 技能详情 ────────────────────────────────────────────────

/// 单个技能的详情：台账记录 + 磁盘现状 + SKILL.md 原文 + 挂载实检。
pub fn skill(name: &str) -> Result<Value, String> {
    let name = name.trim();
    if name.is_empty() || name.contains('/') || name.contains('\\') || name.contains("..") {
        return Err("非法的技能名".to_string());
    }
    let manifest = store::load_manifest();
    let entry = store::manifest_skill(&manifest, name);
    let central = store::skills_dir().join(name);
    let has_central = central.is_dir();

    if entry.is_null() && !has_central {
        // 还不在中央：去扫描结果里找（待收纳的技能也要能看详情）
        let config = tools::load_config();
        let found = scan_all(&config)
            .into_iter()
            .find(|skill| text(skill, "name") == name);
        let Some(found) = found else {
            return Err(format!("找不到技能「{name}」"));
        };
        let dir = std::path::PathBuf::from(text(&found, "dir"));
        return Ok(json!({
            "name": name,
            "manifest": Value::Null,
            "inManifest": false,
            "dir": text(&found, "dir"),
            "tool": found.get("tool").cloned().unwrap_or(Value::Null),
            "description": found.get("description").cloned().unwrap_or(json!("")),
            "version": found.get("version").cloned().unwrap_or(json!("")),
            "treeHash": found.get("treeHash").cloned().unwrap_or(json!("")),
            "health": found.get("health").cloned().unwrap_or(json!([])),
            "skillMd": scan::skill_md_raw(&dir),
            "body": scan::skill_body(&dir),
            "children": scan::list_children(&dir),
            "mounts": [],
        }));
    }

    let mounts: Vec<Value> = if entry.is_null() {
        Vec::new()
    } else {
        entry
            .get("mounts")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default()
            .into_iter()
            .map(|item| mount::verify(&item))
            .collect()
    };

    Ok(json!({
        "name": name,
        "manifest": if entry.is_null() { Value::Null } else { entry.clone() },
        "inManifest": !entry.is_null(),
        "dir": central.to_string_lossy(),
        "centralExists": has_central,
        "description": entry.get("description").cloned().unwrap_or(json!("")),
        "version": entry.get("version").cloned().unwrap_or(json!("")),
        "treeHash": entry.get("treeHash").cloned().unwrap_or(json!("")),
        "health": if has_central { json!(scan::health_check(&central)) } else { json!([]) },
        "skillMd": if has_central { scan::skill_md_raw(&central) } else { String::new() },
        "body": if has_central { scan::skill_body(&central) } else { String::new() },
        // `list_children` 返回 `Vec<Value>`，与 `json!([])` 不同型 → 两边都过一层 `json!`
        "children": if has_central { json!(scan::list_children(&central)) } else { json!([]) },
        "fileCount": if has_central { scan::count_files(&central) } else { 0 },
        "mounts": mounts,
    }))
}

// ─── 去重裁决页 ──────────────────────────────────────────────

/// 冲突列表 + 每条冲突的现场（差异、链接指向）。
pub fn conflicts() -> Value {
    let config = tools::load_config();
    let planned = sync_plan(&config);
    let stored = store::load_conflicts();
    let mut rows: Vec<Value> = Vec::new();
    for item in &stored {
        let mut row = item.clone();
        let kind = text(item, "kind");
        if kind == "diff-link" || kind == "content" {
            let skill = text(item, "skill");
            let dir = text(item, "dir");
            let tool_path = std::path::PathBuf::from(&dir).join(&skill);
            let central = store::skills_dir().join(&skill);
            row["linkPath"] = json!(tool_path.to_string_lossy());
            row["linkTarget"] = json!(mount::link_target(&tool_path));
            row["centralPath"] = json!(central.to_string_lossy());
            row["centralExists"] = json!(central.is_dir());
            row["toolExists"] = json!(tool_path.exists());
            if tool_path.is_dir() {
                row["toolSkillMd"] = json!(scan::skill_md_raw(&tool_path));
            }
            if central.is_dir() {
                row["centralSkillMd"] = json!(scan::skill_md_raw(&central));
            }
        }
        rows.push(row);
    }
    json!({
        "items": rows,
        "duplicates": planned.get("duplicates").cloned().unwrap_or(json!([])),
        "hints": planned.get("hints").cloned().unwrap_or(json!([])),
        "conflictCount": planned.get("counts").and_then(|counts| counts.get("conflict")).cloned().unwrap_or(json!(0)),
    })
}

/// 裁决一条冲突（`plan::resolve` 的门面）。
pub fn resolve(id: &str, choice: &str) -> Result<Value, String> {
    let config = tools::load_config();
    let out = plan::resolve(id, choice, &config);
    invalidate();
    out
}

// ─── 同步 ────────────────────────────────────────────────────

/// 预览（不改任何文件）。
pub fn preview() -> Value {
    let config = tools::load_config();
    sync_plan(&config)
}

/// 执行一次同步（重新规划后重放）。
pub fn sync_now() -> Value {
    let config = tools::load_config();
    let out = plan::execute(&config);
    // 同步会建/摘链接、搬真身，磁盘已经变了 —— 缓存必须立刻失效，
    // 否则紧接着的列表请求会拿旧指纹比对（同一次请求内 mtime 精度不够）。
    invalidate();
    out
}

/// 同步报告内容。
pub fn report(file: &str) -> Result<String, String> {
    store::read_report(file)
}

/// 报告清单（同步页底部的「最近记录」）。
pub fn reports(limit: usize) -> Vec<Value> {
    store::list_reports(limit)
}

// ─── 单技能动作 ──────────────────────────────────────────────

/// 纳管：中央目录里有、台账没记的，只补台账不动文件。
pub fn adopt(name: &str) -> Result<Value, String> {
    let out = plan::adopt(name);
    invalidate();
    out
}

/// 删除一个技能（真身进回收站、挂载摘掉、立墓碑）。
pub fn remove(name: &str) -> Result<Value, String> {
    let out = plan::remove(name);
    invalidate();
    out
}

/// 切换一处挂载的启用状态。
pub fn toggle_mount(name: &str, tool: &str, enabled: bool) -> Result<Value, String> {
    let config = tools::load_config();
    let out = plan::toggle_mount(name, tool, enabled, &config);
    invalidate();
    out
}

/// 解除登记：只删台账记录，不动磁盘链接（用于其他中枢管理的链接位）。
pub fn unregister_mount(name: &str, tool: &str) -> Result<Value, String> {
    let out = plan::unregister(name, tool);
    invalidate();
    out
}

/// 批量解除登记：清掉所有外部链接的台账记录（不动磁盘链接）。
pub fn unregister_all_mounts() -> Value {
    let out = plan::unregister_all();
    invalidate();
    out
}

/// 批量纳管：中央里所有未登记的真实目录一次收进台账。
pub fn adopt_all_skills() -> Value {
    let out = plan::adopt_all();
    invalidate();
    out
}

/// 一键处理：外部登记 → 真失效挂载 → 中央未登记，一次做完（见 `plan::resolve_all`）。
/// 冲突与孤儿不在此列：前者要人选，后者归同步确认执行。
pub fn resolve_all() -> Value {
    let config = tools::load_config();
    let out = plan::resolve_all(&config);
    invalidate();
    out
}

/// 一键修复失效挂载。
pub fn repair() -> Value {
    let config = tools::load_config();
    let out = plan::repair(&config);
    invalidate();
    out
}

// ─── 每日检测 ────────────────────────────────────────────────

/// 每日检测的状态键（保留键，见 `db::schema::RESERVED_KV_KEYS`）。
const CHECK_KEY: &str = "skillsHubDailyCheck";

/// 上一次检测的结果（没跑过给 `null`）。
pub fn daily_check_status() -> Value {
    crate::server::access::kv_get(CHECK_KEY)
        .and_then(|text| serde_json::from_str::<Value>(&text).ok())
        .unwrap_or(Value::Null)
}

/// 跑一次巡检：连通性 / 技能数 / 失效挂载 / 孤儿 / 冲突 / 中央未登记。
///
/// ── 为什么它要落库 ────────────────────────────────────────
/// 「上次检测是什么时候、结论是什么」必须跨进程记住 —— 用户重启一次软件就
/// 看到「尚未运行检测」，等于这个功能不存在。所以结果整份存一个保留键。
pub fn run_daily_check(auto_mount: bool) -> Value {
    let config = tools::load_config();
    // 巡检是用户点「立即刷新」触发的，必须看现场 —— 绕开 2 秒规划缓存，
    // 否则刚在磁盘上改完（还没经过本模块写操作）的技能要等缓存过期才出现。
    // 自动修复要排在规划**之前**：`repair` 会建/摘链接，先规划再修复的话，
    // 下面统计用的 `planned` 是修复前的现场 —— 巡检结论会永远显示「还有 N 处
    // 失效挂载」，而实际上刚刚已经修好了。
    let mut repaired = 0usize;
    if auto_mount {
        let result = plan::repair(&config);
        repaired = result.get("repaired").and_then(Value::as_u64).unwrap_or(0) as usize;
        invalidate();
    }
    let planned = plan::plan_fresh(&config);
    // 只探一次：`list_tools` 每个渠道都要 stat 一遍候选目录，重复调用等于把
    // 同一批 stat 做两遍（14 个渠道 × 若干候选路径）。
    let tool_rows = tools::list_tools(&config);
    let tools_total = tool_rows.len();
    let tools_hit = tool_rows
        .iter()
        .filter(|row| row.get("dir").and_then(Value::as_str).map(|text| !text.is_empty()).unwrap_or(false))
        .count();
    let broken = planned
        .get("mountHealth")
        .and_then(Value::as_array)
        .map(|list| {
            list.iter()
                .filter(|item| {
                    item.get("valid").and_then(Value::as_bool) != Some(true)
                        && item.get("external").and_then(Value::as_bool) != Some(true)
                })
                .count()
        })
        .unwrap_or(0);
    let conflicts = planned.get("conflictsStored").and_then(Value::as_array).map(Vec::len).unwrap_or(0);
    let orphans = planned.get("orphans").and_then(Value::as_array).map(Vec::len).unwrap_or(0);
    let hub_extra = planned.get("hubExtra").and_then(Value::as_array).map(Vec::len).unwrap_or(0);

    // 「巡检失败」只认硬故障：失效挂载与待裁决冲突。
    //
    // 孤儿与中央未登记是「有东西可以纳管」的提示，不该把整轮结果染成失败 ——
    // 渠道里永远可能有别人放进去的技能，判失败的话用户每次点刷新都吃一条红字，
    // 几轮之后就学会无视这个信号。
    let ok = broken == 0 && conflicts == 0;
    let pending = if orphans == 0 && hub_extra == 0 {
        String::new()
    } else {
        format!("，另可纳管 孤儿 {orphans} / 中央未登记 {hub_extra}")
    };
    let summary = if ok {
        format!("{tools_hit}/{tools_total} 个渠道连通，没有失效挂载与待裁决项{pending}")
    } else {
        format!(
            "{tools_hit}/{tools_total} 个渠道连通 · 失效挂载 {broken} 处 · 待裁决 {conflicts}{pending}"
        )
    };
    let payload = json!({
        "ok": ok,
        "checkedAt": store::now_ms(),
        "summary": summary,
        "toolsHit": tools_hit,
        "toolsTotal": tools_total,
        "brokenMounts": broken,
        "repaired": repaired,
        "conflicts": conflicts,
        "orphans": orphans,
        "hubExtra": hub_extra,
    });
    if let Ok(text) = serde_json::to_string(&payload) {
        crate::server::access::kv_put(CHECK_KEY, &text);
    }
    payload
}

// ─── 回收站 ──────────────────────────────────────────────────

pub fn trash() -> Value {
    let config = tools::load_config();
    let days = config.get("trashDays").and_then(Value::as_i64).unwrap_or(7);
    json!({
        "items": store::list_trash(days),
        "trashDays": days,
        "dir": store::trash_dir().to_string_lossy(),
    })
}

pub fn restore(file: &str) -> Result<Value, String> {
    let out = store::restore_trash(file);
    invalidate();
    out
}

pub fn purge_trash() -> Value {
    let config = tools::load_config();
    let days = config.get("trashDays").and_then(Value::as_i64).unwrap_or(7);
    let removed = store::purge_trash(days);
    invalidate();
    json!({ "removed": removed })
}

/// 清空回收站：与「按天数清过期」分开，见 `store::empty_trash` 的注释。
pub fn empty_trash() -> Value {
    let removed = store::empty_trash();
    invalidate();
    json!({ "removed": removed })
}

// ─── 渠道探测 ────────────────────────────────────────────────

/// 渠道清单（含解析出的实际目录与是否存在）。
///
/// 名字避开 `tools` —— 子模块也叫 `tools`，同名函数会让 `tools::load_config()`
/// 与 `tools()` 在同一段代码里混着读，改起来容易看错。
pub fn tool_list() -> Vec<Value> {
    let config = tools::load_config();
    tools::list_tools(&config)
}

/// 未注册但社区常见的技能目录：只读探测，命中给建议（用户确认才加）。
pub fn probe() -> Vec<Value> {
    let config = tools::load_config();
    tools::probe_agents(&config)
}

/// 本机现成的技能仓库（带台账），供配置页一键填入 `skillsDir`。
pub fn shared_repos() -> Vec<Value> {
    tools::shared_repo_candidates()
}

/// 加一条自定义目录。
pub fn add_custom_dir(path: &str) -> Result<Value, String> {
    let spec = path.trim();
    if spec.is_empty() {
        return Err("目录不能为空".to_string());
    }
    let expanded = tools::expand_path(spec);
    if !expanded.is_dir() {
        return Err(format!("目录不存在：{}", expanded.display()));
    }
    let mut config = tools::load_config();
    let mut dirs: Vec<Value> = config
        .get("customDirs")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    if dirs.iter().any(|item| item.as_str().map(|text| text == spec).unwrap_or(false)) {
        return Err("这个目录已经加过了".to_string());
    }
    dirs.push(json!(spec));
    config["customDirs"] = Value::Array(dirs);
    let out = tools::save_config(&config);
    invalidate();
    out
}

/// 删一条自定义目录。
pub fn remove_custom_dir(path: &str) -> Result<Value, String> {
    let mut config = tools::load_config();
    let dirs: Vec<Value> = config
        .get("customDirs")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default()
        .into_iter()
        .filter(|item| item.as_str().map(|text| text != path).unwrap_or(false))
        .collect();
    config["customDirs"] = Value::Array(dirs);
    let out = tools::save_config(&config);
    invalidate();
    out
}

fn text(value: &Value, key: &str) -> String {
    value.get(key).and_then(Value::as_str).unwrap_or("").to_string()
}