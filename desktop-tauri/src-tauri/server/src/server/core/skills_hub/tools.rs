//! 技能仓库 · 渠道（各 AI 客户端的技能目录）注册表与配置。
//!
//! ── 「渠道」是什么 ──────────────────────────────────────────
//! 每个 AI 编程客户端（Codex / Claude Code / WorkBuddy / Qoder / Trae …）都把
//! 技能放在自己家目录下的 `skills/` 里。技能仓库要做的第一件事就是知道**这些
//! 目录在哪**：扫描它们、把重复的收成一份真身、再把真身挂回每个目录。
//!
//! 所以一条渠道 = `{ id, name, paths[] }`。`id` 是引用键：manifest 的来源、
//! 挂载台账、冲突记录里写的都是它，**创建后不能改** —— 改了等于所有历史记录
//! 指向一个不存在的渠道。
//!
//! ── 为什么内置表写在代码里，配置只存覆盖 ────────────────────
//! 内置渠道的默认路径随客户端版本变（比如某家把 skills 从 `~/.x` 挪到
//! `%APPDATA%/x`）。若把整份表存进配置，升级后老配置里的旧路径会一直生效，
//! 用户看到的永远是「找不到目录」。所以：**默认值在代码里，配置只记用户改过
//! 的部分**（改名 / 改路径 / 停用）。新增内置渠道时老用户自动拿到。
//!
//! ── 路径展开 ────────────────────────────────────────────────
//! 支持三种写法，与各客户端的文档口径一致：
//! * `~/x/skills` 或 `x/skills`（相对路径按用户主目录算）
//! * `%APPDATA%/x/skills`、`%LOCALAPPDATA%/x/skills`（Windows 数据目录，
//!   大小写不敏感；非 Windows 上退回 `<home>/AppData/Roaming` 这类约定路径，
//!   找不到就是找不到 —— 这些客户端本来也不在那上面跑）
//! * 绝对路径
//!
//! ── 自定义目录为什么要派生 id ───────────────────────────────
//! 用户可能加好几条自定义目录。若都用 `custom` 当 id，挂载台账里
//! `(tool, name)` 相同的记录会互相顶掉，启停开关串台。所以自定义目录的 id 是
//! `custom-<路径哈希前 8 位>`：路径不变 id 就不变，台账能对上。

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use serde_json::{json, Map, Value};

use crate::server::access;

/// `kv` 表里的配置键（保留键，见 `db::schema::RESERVED_KV_KEYS`）
pub const KV_KEY: &str = "skillsHubConfig";

/// 一条内置渠道。`paths` 按顺序探测，第一个存在的生效。
pub struct ToolDef {
    pub id: &'static str,
    pub name: &'static str,
    pub paths: &'static [&'static str],
}

/// 内置渠道表（社区事实，不是猜测：这些路径来自各客户端自己的文档与实机）。
pub const TOOLS: &[ToolDef] = &[
    ToolDef { id: "codex", name: "Codex CLI", paths: &[".codex/skills"] },
    ToolDef { id: "claude", name: "Claude Code", paths: &[".claude/skills"] },
    ToolDef { id: "zcode", name: "ZCode", paths: &[".zcode/skills"] },
    ToolDef { id: "dsh", name: "DeepSeek Harness", paths: &[".dsh/skills"] },
    ToolDef { id: "workbuddy", name: "WorkBuddy", paths: &[".workbuddy/skills"] },
    ToolDef { id: "workbuddy-ai", name: "WorkBuddy AI", paths: &[".workbuddy-ai/skills"] },
    ToolDef {
        id: "antigravity",
        name: "Antigravity",
        paths: &[".gemini/antigravity/skills", ".gemini/config/skills"],
    },
    ToolDef { id: "agents", name: "通用 ~/.agents", paths: &[".agents/skills"] },
    ToolDef {
        id: "raccoon",
        name: "商汤小浣熊",
        paths: &[".box-agent/skills", ".raccoon/skills", "%APPDATA%/office-raccoon/skills"],
    },
    ToolDef {
        id: "autoclaw",
        name: "AutoClaw",
        paths: &[".autoclaw/skills", "%APPDATA%/AutoClaw/skills", "%LOCALAPPDATA%/Programs/AutoClaw/skills"],
    },
    ToolDef { id: "autoclaw-intl", name: "AutoClaw 国际版", paths: &[".autoclaw-intl/skills", "%APPDATA%/AutoClaw-intl/skills"] },
    ToolDef { id: "qoder", name: "Qoder CN", paths: &[".qoder/skills", "%APPDATA%/Qoder/skills"] },
    ToolDef {
        id: "qoder-intl",
        name: "Qoder 国际版",
        paths: &[".qoder-intl/skills", ".qoder_intl/skills", "%APPDATA%/Qoder-intl/skills"],
    },
    ToolDef { id: "trae", name: "Trae", paths: &[".trae/skills", "%APPDATA%/Trae/skills"] },
];

/// 未注册但社区常见的技能目录：只做只读探测，命中即建议（用户确认才加）。
pub struct KnownAgent {
    pub suggest_id: &'static str,
    pub name: &'static str,
    pub hit_paths: &'static [&'static str],
}

pub const KNOWN_AGENTS: &[KnownAgent] = &[
    KnownAgent { suggest_id: "cursor", name: "Cursor", hit_paths: &[".cursor/skills", "%APPDATA%/Cursor/User/skills"] },
    KnownAgent { suggest_id: "gemini", name: "Gemini CLI", hit_paths: &[".gemini/skills"] },
    KnownAgent { suggest_id: "opencode", name: "OpenCode", hit_paths: &[".opencode/skills"] },
    KnownAgent { suggest_id: "roo", name: "Roo Code", hit_paths: &[".roo/skills", ".roo-code/skills"] },
    KnownAgent { suggest_id: "kilocode", name: "Kilo Code", hit_paths: &[".kilocode/skills"] },
    KnownAgent { suggest_id: "windsurf", name: "Windsurf", hit_paths: &[".windsurf/skills"] },
    KnownAgent { suggest_id: "augment", name: "Augment Code", hit_paths: &[".augment/skills"] },
];

/// 用户主目录。`AGENT_SKILLS_FAKE_HOME` 让自测跑在临时目录里（与 AgentHub
/// 同一个环境变量名：两边的技能仓库指同一套磁盘布局时，测试也应当对称）。
pub fn home_dir() -> PathBuf {
    if let Ok(value) = std::env::var("AGENT_SKILLS_FAKE_HOME") {
        if !value.trim().is_empty() {
            return PathBuf::from(value.trim());
        }
    }
    std::env::var("USERPROFILE")
        .or_else(|_| std::env::var("HOME"))
        .map(|value| PathBuf::from(value.trim()))
        .unwrap_or_default()
}

fn appdata_dir(local: bool) -> PathBuf {
    let name = if local { "LOCALAPPDATA" } else { "APPDATA" };
    if let Ok(value) = std::env::var(name) {
        if !value.trim().is_empty() {
            return PathBuf::from(value.trim());
        }
    }
    let home = home_dir();
    if local {
        home.join("AppData").join("Local")
    } else {
        home.join("AppData").join("Roaming")
    }
}

/// 路径展开（见模块头「路径展开」）。
pub fn expand_path(spec: &str) -> PathBuf {
    let text = spec.trim();
    if text.is_empty() {
        return PathBuf::new();
    }
    let upper = text.to_uppercase();
    if upper.starts_with("%APPDATA%") {
        return appdata_dir(false).join(text["%APPDATA%".len()..].trim_start_matches(['/', '\\']));
    }
    if upper.starts_with("%LOCALAPPDATA%") {
        return appdata_dir(true).join(text["%LOCALAPPDATA%".len()..].trim_start_matches(['/', '\\']));
    }
    if let Some(rest) = text.strip_prefix('~') {
        return home_dir().join(rest.trim_start_matches(['/', '\\']));
    }
    let path = PathBuf::from(text);
    if path.is_absolute() {
        path
    } else {
        home_dir().join(path)
    }
}

/// 自定义目录的稳定 id：`custom-` + 路径 sha1 前 8 位（见模块头）
pub fn custom_dir_id(path: &Path) -> String {
    use sha1::{Digest, Sha1};
    let mut hasher = Sha1::new();
    let key = if cfg!(windows) {
        path.to_string_lossy().to_lowercase()
    } else {
        path.to_string_lossy().to_string()
    };
    hasher.update(key.as_bytes());
    let digest = hasher.finalize();
    let hex: String = digest.iter().map(|byte| format!("{byte:02x}")).collect();
    format!("custom-{}", &hex[..8])
}

/// 配置默认值。键名与前端 `skills-api.ts` 的 `HubConfig` 逐字对齐。
pub fn default_config() -> Value {
    json!({
        "mountMode": "junction",
        "trashDays": 7,
        "l3Enabled": false,
        "l3Threshold": 0.85,
        "tools": {},
        "customDirs": [],
        // 仓库位置覆盖：空串 = 用默认（`<数据目录>/skills-hub`）。
        // `skillsDir` 可指到现成仓库（如 AgentHub 的 `~/.agent_skills/skills`），
        // 真身两边共用、都能往里写，只有台账各管各的。
        "hubDir": "",
        "skillsDir": "",
        // 每日自动巡检三项（对齐参考实现的配置页）。
        // `dailyCheck` 默认**开**：这是保底健康检查，用户装完就该有；
        // `dailyCheckAutoMount` 也默认开，但它只重建**确信失效**的链接
        // （外链不算失效，见 `mount::verify` 的 `external`）。
        "schedule": {
            "dailyCheck": true,
            "dailyCheckTime": "09:00",
            "dailyCheckAutoMount": true,
        },
    })
}

fn clamp_int(value: Option<&Value>, fallback: i64, min: i64, max: i64) -> i64 {
    let raw = match value {
        Some(Value::Number(number)) => number.as_f64().unwrap_or(fallback as f64),
        Some(Value::String(text)) => text.trim().parse::<f64>().unwrap_or(fallback as f64),
        _ => fallback as f64,
    };
    if !raw.is_finite() {
        return fallback;
    }
    (raw as i64).clamp(min, max)
}

/// 读配置：默认值 + kv 里存的那份（逐字段收敛，坏值退回默认）。
pub fn load_config() -> Value {
    let stored = access::kv_get(KV_KEY)
        .and_then(|text| serde_json::from_str::<Value>(&text).ok())
        .unwrap_or_else(|| json!({}));
    let mut config = default_config();
    if let Some(raw) = stored.as_object() {
        let mode = raw.get("mountMode").and_then(Value::as_str).unwrap_or("junction");
        config["mountMode"] = json!(if mode == "copy" { "copy" } else { "junction" });
        config["trashDays"] = json!(clamp_int(raw.get("trashDays"), 7, 0, 3650));
        config["l3Enabled"] = json!(raw.get("l3Enabled").and_then(Value::as_bool).unwrap_or(false));
        let threshold = raw
            .get("l3Threshold")
            .and_then(Value::as_f64)
            .unwrap_or(0.85)
            .clamp(0.3, 0.999);
        config["l3Threshold"] = json!(threshold);
        // tools：只留认识的 id、只留合法字段（用户手改坏了不该整页 500）
        if let Some(tools) = raw.get("tools").and_then(Value::as_object) {
            let mut out = Map::new();
            for (id, item) in tools {
                if !is_known_tool_id(id) {
                    continue;
                }
                let mut entry = Map::new();
                if let Some(enabled) = item.get("enabled").and_then(Value::as_bool) {
                    entry.insert("enabled".to_string(), json!(enabled));
                }
                if let Some(name) = item.get("name").and_then(Value::as_str) {
                    let name = name.trim();
                    if !name.is_empty() {
                        entry.insert("name".to_string(), json!(name));
                    }
                }
                if let Some(paths) = item.get("paths").and_then(Value::as_array) {
                    let cleaned: Vec<Value> = paths
                        .iter()
                        .filter_map(Value::as_str)
                        .map(str::trim)
                        .filter(|text| !text.is_empty())
                        .map(|text| json!(text))
                        .collect();
                    if !cleaned.is_empty() {
                        entry.insert("paths".to_string(), Value::Array(cleaned));
                    }
                }
                if !entry.is_empty() {
                    out.insert(id.clone(), Value::Object(entry));
                }
            }
            config["tools"] = Value::Object(out);
        }
        if let Some(dirs) = raw.get("customDirs").and_then(Value::as_array) {
            let cleaned: Vec<Value> = dirs
                .iter()
                .filter_map(Value::as_str)
                .map(str::trim)
                .filter(|text| !text.is_empty())
                .map(|text| json!(text))
                .collect();
            config["customDirs"] = Value::Array(cleaned);
        }
        // 仓库位置：`hubDir`（账本 / 回收站 / 报告）与 `skillsDir`（真身）。
        //
        // 这两个字段以前只写在文档注释里、**从没在这张白名单里透传过**，于是填了
        // 也不生效，永远落回 `config_dir()/skills-hub`（2026-11-28 查出来的死开关）。
        // `skillsDir` 用来借现成仓库：指到 AgentHub 的 `.agent_skills/skills` 就能
        // 共用同一批真身，不复制、不改对方。
        for key in ["hubDir", "skillsDir"] {
            let value = raw.get(key).and_then(Value::as_str).map(str::trim).unwrap_or("");
            if !value.is_empty() {
                config[key] = json!(value);
            }
        }
        // schedule：三个字段逐个收敛。时刻必须是合法 HH:MM，坏了退回 09:00 ——
        // 留着坏值会让调度器每分钟解析失败一次，等于这个功能悄悄死掉。
        if let Some(schedule) = raw.get("schedule").and_then(Value::as_object) {
            let daily_check = schedule
                .get("dailyCheck")
                .and_then(Value::as_bool)
                .unwrap_or(true);
            let auto_mount = schedule
                .get("dailyCheckAutoMount")
                .and_then(Value::as_bool)
                .unwrap_or(true);
            let time_text = schedule
                .get("dailyCheckTime")
                .and_then(Value::as_str)
                .map(str::trim)
                .unwrap_or("");
            let time_text = if super::schedule::is_valid_clock(time_text) {
                time_text.to_string()
            } else {
                "09:00".to_string()
            };
            config["schedule"] = json!({
                "dailyCheck": daily_check,
                "dailyCheckTime": time_text,
                "dailyCheckAutoMount": auto_mount,
            });
        }
    }
    config
}

pub fn is_known_tool_id(id: &str) -> bool {
    TOOLS.iter().any(|tool| tool.id == id)
}

/// 本机现成的技能仓库候选（用户想「共用真身」时不用手打路径）。
///
/// ── 为什么只认这几处 ────────────────────────────────────────
/// 判据不是「哪个目录里有 skills」，而是「哪个目录里有**带台账的技能仓库**」：
/// 有 `manifest.json` 说明它被某个中枢（AgentHub / 别的客户端）真正管着，指过去
/// 才有意义 —— 只有一个空 `skills/` 目录指过去等于把中央库换成空的。所以候选
/// 必须同时满足「`<dir>/skills` 存在」+「`<dir>/manifest.json` 存在」。
///
/// 返回 `[{ path, skills, count }]`，按技能数降序，供配置页一键填入。
pub fn shared_repo_candidates() -> Vec<Value> {
    let home = home_dir();
    let mut roots: Vec<PathBuf> = vec![
        // AgentHub 的默认中枢（本程序要复刻/替代的那个）
        home.join(".agent_skills"),
        // 少数版本把它放在数据目录下
        home.join(".agenthub").join("skills-hub"),
        home.join(".agent2api").join("agenthub"),
    ];
    // 用户已配的 skillsDir 也当候选（方便切回去）
    let config = load_config();
    let configured = config.get("skillsDir").and_then(Value::as_str).unwrap_or("").trim().to_string();
    if !configured.is_empty() {
        roots.push(expand_path(&configured));
    }

    let mut out: Vec<Value> = Vec::new();
    for root in roots {
        // 允许用户填的是 skills 那一层
        let repo = if root.file_name().map(|n| n.to_string_lossy().eq_ignore_ascii_case("skills")).unwrap_or(false)
            && root.join("manifest.json").is_file()
        {
            root.parent().map(|p| p.to_path_buf()).unwrap_or(root.clone())
        } else {
            root
        };
        let skills = repo.join("skills");
        if !skills.is_dir() || !repo.join("manifest.json").is_file() {
            continue;
        }
        let key = scan_path_key(&repo);
        if out.iter().any(|row| row.get("key").and_then(Value::as_str) == Some(key.as_str())) {
            continue;
        }
        let count = fs::read_dir(&skills)
            .map(|entries| {
                entries
                    .flatten()
                    .filter(|entry| {
                        let path = entry.path();
                        fs::symlink_metadata(&path)
                            .map(|meta| meta.is_dir() && !super::scan::is_link_entry(&path, &meta))
                            .unwrap_or(false)
                    })
                    .count()
            })
            .unwrap_or(0);
        out.push(json!({
            "key": key,
            "path": repo.to_string_lossy(),
            "skillsDir": skills.to_string_lossy(),
            "count": count,
        }));
    }
    out.sort_by(|a, b| {
        b.get("count").and_then(Value::as_u64).unwrap_or(0)
            .cmp(&a.get("count").and_then(Value::as_u64).unwrap_or(0))
    });
    out
}

/// 路径比较键（与 `scan::path_key` 同口径，这里只为少一层依赖）。
fn scan_path_key(path: &Path) -> String {
    let text = fs::canonicalize(path)
        .map(|real| real.to_string_lossy().to_string())
        .unwrap_or_else(|_| path.to_string_lossy().to_string());
    if cfg!(windows) { text.to_lowercase() } else { text }
}

/// 写配置（整份替换：配置是整体读写单元，拆字段会让一次保存变成多次写）。
pub fn save_config(next: &Value) -> Result<Value, String> {
    if !next.is_object() {
        return Err("配置必须是 JSON 对象".to_string());
    }
    let text = serde_json::to_string(next).map_err(|error| format!("配置序列化失败: {error}"))?;
    access::kv_put(KV_KEY, &text);
    Ok(load_config())
}

fn tool_override<'a>(config: &'a Value, id: &str) -> Option<&'a Value> {
    config.get("tools").and_then(Value::as_object).and_then(|map| map.get(id))
}

/// 一条渠道的最终形态（内置默认 + 配置覆盖）。
fn tool_entry(config: &Value, id: &str) -> Value {
    let builtin = TOOLS.iter().find(|tool| tool.id == id);
    let over = tool_override(config, id);
    let enabled = over.and_then(|item| item.get("enabled")).and_then(Value::as_bool).unwrap_or(true);
    let name = over
        .and_then(|item| item.get("name"))
        .and_then(Value::as_str)
        .map(str::to_string)
        .or_else(|| builtin.map(|tool| tool.name.to_string()))
        .unwrap_or_else(|| id.to_string());
    let paths: Vec<String> = over
        .and_then(|item| item.get("paths"))
        .and_then(Value::as_array)
        .map(|list| list.iter().filter_map(Value::as_str).map(str::to_string).collect())
        .filter(|list: &Vec<String>| !list.is_empty())
        .or_else(|| builtin.map(|tool| tool.paths.iter().map(|path| path.to_string()).collect()))
        .unwrap_or_default();
    json!({
        "id": id,
        "name": name,
        "builtin": builtin.is_some(),
        "enabled": enabled,
        "paths": paths,
    })
}

/// 全部渠道 id：内置在前，配置里新增的（不该有，防御）在后。
pub fn tool_ids(config: &Value) -> Vec<String> {
    let mut ids: Vec<String> = TOOLS.iter().map(|tool| tool.id.to_string()).collect();
    if let Some(map) = config.get("tools").and_then(Value::as_object) {
        for id in map.keys() {
            if !ids.iter().any(|known| known == id) {
                ids.push(id.clone());
            }
        }
    }
    ids
}

/// 解析一条渠道实际要扫的目录（第一个存在的候选路径生效）。
pub fn resolve_tool_dir(config: &Value, id: &str) -> Value {
    let entry = tool_entry(config, id);
    let paths: Vec<String> = entry
        .get("paths")
        .and_then(Value::as_array)
        .map(|list| list.iter().filter_map(Value::as_str).map(str::to_string).collect())
        .unwrap_or_default();
    let mut found = String::new();
    if entry.get("enabled").and_then(Value::as_bool).unwrap_or(true) {
        for spec in &paths {
            let path = expand_path(spec);
            if !path.as_os_str().is_empty() && path.exists() {
                found = path.to_string_lossy().to_string();
                break;
            }
        }
    }
    json!({
        "id": entry["id"],
        "name": entry["name"],
        "builtin": entry["builtin"],
        "enabled": entry["enabled"],
        "dir": if found.is_empty() { Value::Null } else { json!(found) },
        "candidatePaths": paths,
    })
}

/// 设置页用：含「没启用 / 没找到」的渠道，让界面能显示为什么没连上。
pub fn list_tools(config: &Value) -> Vec<Value> {
    tool_ids(config)
        .iter()
        .map(|id| resolve_tool_dir(config, id))
        .collect()
}

/// 扫描目标：真正存在的目录（含启用的自定义目录）。
pub fn resolve_targets(config: &Value) -> Vec<Value> {
    let mut out: Vec<Value> = Vec::new();
    for row in list_tools(config) {
        if row.get("enabled").and_then(Value::as_bool) != Some(true) {
            continue;
        }
        let dir = row.get("dir").and_then(Value::as_str).unwrap_or("");
        if dir.is_empty() {
            continue;
        }
        out.push(json!({
            "id": row["id"],
            "name": row["name"],
            "dir": dir,
        }));
    }
    if let Some(dirs) = config.get("customDirs").and_then(Value::as_array) {
        for item in dirs {
            let spec = item.as_str().unwrap_or("").trim();
            if spec.is_empty() {
                continue;
            }
            let path = expand_path(spec);
            if !path.exists() {
                continue;
            }
            out.push(json!({
                "id": custom_dir_id(&path),
                "name": path
                    .file_name()
                    .map(|value| value.to_string_lossy().to_string())
                    .unwrap_or_else(|| "自定义目录".to_string()),
                "dir": path.to_string_lossy(),
                "custom": true,
            }));
        }
    }
    out
}

/// 只读探测：社区常见目录里，用户还没接进来的那些。
///
/// 命中即建议、绝不自动加：猜错一条目录就会把别人的技能扫进仓库。
pub fn probe_agents(config: &Value) -> Vec<Value> {
    let mut registered_ids: Vec<String> = tool_ids(config);
    let mut registered_dirs: BTreeMap<String, ()> = BTreeMap::new();
    for id in tool_ids(config) {
        let entry = tool_entry(config, &id);
        if let Some(paths) = entry.get("paths").and_then(Value::as_array) {
            for spec in paths.iter().filter_map(Value::as_str) {
                let path = expand_path(spec);
                if !path.as_os_str().is_empty() {
                    registered_dirs.insert(path_key(&path), ());
                }
            }
        }
    }
    for spec in config.get("customDirs").and_then(Value::as_array).into_iter().flatten() {
        let path = expand_path(spec.as_str().unwrap_or(""));
        if !path.as_os_str().is_empty() {
            registered_dirs.insert(path_key(&path), ());
        }
    }
    let mut detected: Vec<Value> = Vec::new();
    for agent in KNOWN_AGENTS {
        let hits: Vec<PathBuf> = agent
            .hit_paths
            .iter()
            .map(|spec| expand_path(spec))
            .filter(|path| !path.as_os_str().is_empty() && path.exists())
            .filter(|path| !registered_dirs.contains_key(&path_key(path)))
            .collect();
        if hits.is_empty() {
            continue;
        }
        if registered_ids.iter().any(|id| id == agent.suggest_id) {
            continue;
        }
        registered_ids.push(agent.suggest_id.to_string());
        let first = hits[0].to_string_lossy().to_string();
        detected.push(json!({
            "suggestId": agent.suggest_id,
            "name": agent.name,
            "hitDirs": hits.iter().map(|path| path.to_string_lossy().to_string()).collect::<Vec<String>>(),
            "skillCount": super::scan::count_channel_skills(&hits[0]),
            "firstDir": first,
        }));
    }
    detected
}

fn path_key(path: &Path) -> String {
    let text = path.to_string_lossy().to_string();
    if cfg!(windows) {
        text.to_lowercase()
    } else {
        text
    }
}

/// 渠道目录是否存在（设置页显示「已连接 / 未找到」时不抛错）
pub fn dir_exists(dir: &str) -> bool {
    !dir.trim().is_empty() && Path::new(dir).exists() && fs::metadata(dir).is_ok()
}
