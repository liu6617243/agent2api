use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use super::{files, ClientDef};

#[derive(Default, Serialize, Deserialize)]
struct Preferences {
    paths: std::collections::BTreeMap<String, String>,
}

fn preferences_file() -> PathBuf {
    crate::paths::config_dir().join("agent-paths.json")
}

fn preferences() -> Result<Preferences, String> {
    let Some(bytes) = files::read(&preferences_file())? else {
        return Ok(Preferences::default());
    };
    serde_json::from_slice(&bytes)
        .map_err(|error| format!("配置路径偏好无法读取，拒绝覆盖: {error}"))
}

pub fn path_override(id: &str) -> Option<String> {
    preferences().ok()?.paths.remove(id)
}

pub fn save_override(def: &ClientDef, raw: &str) -> Result<(), String> {
    static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    let _guard = LOCK.lock().map_err(|_| "配置路径锁不可用")?;
    let mut preferences = preferences()?;
    let raw = raw.trim();
    if raw.is_empty() {
        preferences.paths.remove(def.id);
    } else {
        let path = expand(raw)?;
        if !path.is_absolute() || path.is_dir() {
            return Err("配置路径必须是绝对文件路径，不能是目录".into());
        }
        let valid_name = match def.id {
            "codex" | "kimi-code" => path.file_name().is_some_and(|name| name == "config.toml"),
            "workbuddy" => path.file_name().is_some_and(|name| name == "models.json"),
            "cline" => path
                .file_name()
                .is_some_and(|name| name == "providers.json"),
            "zcode" => path
                .file_name()
                .is_some_and(|name| name == "provider_config.json"),
            "crush" => path.file_name().is_some_and(|name| name == "crush.json"),
            "dsh" => super::dsh_paths::validate_path(&path),
            "cursor-cli" => path
                .file_name()
                .is_some_and(|name| name == "cli-config.json"),
            "opencode" => path.file_name().is_some_and(|name| {
                name == "opencode.json" || name == "opencode.jsonc" || name == "config.json"
            }),
            _ => path.file_name().is_some_and(|name| name == "settings.json"),
        };
        if !valid_name {
            return Err("文件名与这个客户端的配置文件不匹配".into());
        }
        preferences
            .paths
            .insert(def.id.into(), path.display().to_string());
    }
    let text =
        serde_json::to_vec_pretty(&preferences).map_err(|e| format!("保存配置路径失败: {e}"))?;
    let written = files::write_atomic(&preferences_file(), &text);
    // 路径偏好变了，缓存里的 `path` 就过期了 —— 立刻作废，别让用户改完路径
    // 回来还看到旧目录。
    forget_detection();
    written
}

pub fn home() -> PathBuf {
    std::env::var_os("USERPROFILE")
        .or_else(|| std::env::var_os("HOME"))
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
}

pub fn expand(raw: &str) -> Result<PathBuf, String> {
    if raw == "~" {
        return Ok(home());
    }
    if let Some(rest) = raw.strip_prefix("~/").or_else(|| raw.strip_prefix("~\\")) {
        return Ok(home().join(rest));
    }
    Ok(PathBuf::from(raw))
}

fn env_dir(key: &str) -> Option<PathBuf> {
    std::env::var_os(key)
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
}

pub fn candidates(def: &ClientDef) -> Vec<PathBuf> {
    let home = home();
    let xdg = env_dir("XDG_CONFIG_HOME").unwrap_or_else(|| home.join(".config"));
    let dir = |env: &str, fallback: &str| env_dir(env).unwrap_or_else(|| home.join(fallback));
    match def.id {
        "claude-code" => vec![dir("CLAUDE_CONFIG_DIR", ".claude").join("settings.json")],
        "codex" => vec![dir("CODEX_HOME", ".codex").join("config.toml")],
        "gemini-cli" => vec![env_dir("GEMINI_CLI_HOME")
            .unwrap_or_else(|| home.clone())
            .join(".gemini/settings.json")],
        "zcode" => vec![home.join(".zcode/v2/provider_config.json")],
        "dsh" => vec![dir("DSH_HOME", ".dsh").join("settings.yaml")],
        "workbuddy" => vec![
            dir("WORKBUDDY_CONFIG_DIR", ".workbuddy").join("models.json"),
            home.join(".codebuddy/models.json"),
        ],
        "qoder" => vec![
            dir("QODER_CONFIG_DIR", ".qoder").join("settings.json"),
            dir("QODERCN_CONFIG_DIR", ".qoder-cn").join("settings.json"),
        ],
        "cline" => vec![env_dir("CLINE_DATA_DIR")
            .unwrap_or_else(|| dir("CLINE_DIR", ".cline").join("data"))
            .join("settings/providers.json")],
        "opencode" => {
            if let Some(path) = env_dir("OPENCODE_CONFIG") {
                return vec![path];
            }
            let dir = env_dir("OPENCODE_CONFIG_DIR").unwrap_or_else(|| xdg.join("opencode"));
            vec![
                dir.join("opencode.jsonc"),
                dir.join("opencode.json"),
                dir.join("config.json"),
            ]
        }
        "kimi-code" => {
            if let Some(dir) = env_dir("KIMI_CODE_HOME") {
                return vec![dir.join("config.toml")];
            }
            if let Some(dir) = env_dir("KIMI_SHARE_DIR") {
                return vec![dir.join("config.toml")];
            }
            vec![
                home.join(".kimi-code/config.toml"),
                home.join(".kimi/config.toml"),
            ]
        }
        "cursor-cli" => vec![env_dir("CURSOR_CONFIG_DIR")
            .unwrap_or_else(|| {
                if cfg!(target_os = "linux") {
                    xdg.join("cursor")
                } else {
                    home.join(".cursor")
                }
            })
            .join("cli-config.json")],
        "zed" => vec![if cfg!(windows) {
            env_dir("APPDATA")
                .unwrap_or_else(|| home.join("AppData/Roaming"))
                .join("Zed/settings.json")
        } else {
            xdg.join("zed/settings.json")
        }],
        "crush" => vec![env_dir("CRUSH_GLOBAL_CONFIG")
            .unwrap_or_else(|| {
                if cfg!(windows) {
                    env_dir("LOCALAPPDATA")
                        .unwrap_or_else(|| home.join("AppData/Local"))
                        .join("crush")
                } else {
                    xdg.join("crush")
                }
            })
            .join("crush.json")],
        _ => Vec::new(),
    }
}

#[derive(Clone)]
pub struct Detection {
    pub path: PathBuf,
    pub detected: bool,
    pub evidence: Vec<String>,
    pub version: String,
    pub unsupported: bool,
    pub note: Option<String>,
    pub config_targets: Vec<super::dsh_paths::ConfigTarget>,
    pub resource_path: Option<PathBuf>,
}

/// 探测结果的短命缓存（**进程级**一份，不是每线程一份）。
///
/// ── 为什么必须有 ────────────────────────────────────────────
/// `detect()` 里最贵的是 `find_binary`：它把 PATH 上**每个目录 × 每个可执行名
/// × 每个 PATHEXT 后缀**都 stat 一遍。本机实测 PATH 58 个目录、11 个后缀，
/// 13 个客户端扫一遍是 **4.4 万次 stat / 约 2.9 秒**（Windows 上还要过一遍
/// 实时防护）。而接入页一次 `list()` 会对**同一个客户端**调四次 `detect` ——
/// 自己一次，`for_client` 里再来三次（`mcp_config_path`、提示词路径、MCP 快照
/// 各一次）。四次里三次纯属重复，页面就卡在那句「正在读取…」上。
///
/// ── 2026-11-21：这层缓存原来是 `thread_local`，等于没有 ────────
/// 记忆中枢首屏仍然要转好几秒，查下来根因就在这里：`thread_local!` 是**每线程
/// 一份**，而 axum 是多线程运行时 —— 同一个页面连发的 `/config`、`/projects`、
/// `/status` 大概率落在三个不同的 worker 线程上，每条线程都各自冷启动，
/// 缓存命中率接近 0。于是「5 秒 TTL」形同虚设：13 个客户端 × 4.4 万次 stat
/// 在**每个请求、每条线程**上重付一遍，一次 `connectivity()` 就是 2~3 秒。
///
/// 改成进程级 `Mutex<HashMap>`：一次探测的结果所有 worker 共享，命中率从
/// 「同线程连续调用」变成「所有请求共享 5 秒窗口」，这才是 TTL 本来想做的事。
/// 代价是跨线程的一把短锁（读一次 clone 一个 `Detection`，微秒级）。
///
/// ── 为什么 TTL 只给 5 秒 ────────────────────────────────────
/// 要盖住的是「同一次请求里的四次调用」，它们相隔微秒；顺手把「切走再切回来」
/// 也盖成瞬开。5 秒又短到用户去装个客户端、回来时不会看到过期的「未检出」。
/// 缓存只存**探测结果**（装没装、版本、路径），配置文件内容每次照读 ——
/// 「已接入」这类会当场变的事实完全不受影响。
const DETECT_TTL: std::time::Duration = std::time::Duration::from_secs(5);

type DetectCache = std::sync::Mutex<std::collections::HashMap<&'static str, (std::time::Instant, Detection)>>;

fn detect_cache() -> &'static DetectCache {
    static CACHE: std::sync::OnceLock<DetectCache> = std::sync::OnceLock::new();
    CACHE.get_or_init(|| std::sync::Mutex::new(std::collections::HashMap::new()))
}

/// 丢掉探测缓存。改了路径偏好 / 刚写完客户端配置之后要叫一声，
/// 否则界面会拿着 5 秒内的旧结论说「未检出」。
///
/// 浅路径缓存一起清：它比探测缓存更容易过期 —— 改的就是它缓存的那个值。
pub fn forget_detection() {
    if let Ok(mut cache) = detect_cache().lock() {
        cache.clear();
    }
    if let Ok(mut cache) = shallow_cache().lock() {
        cache.clear();
    }
}

pub fn detect(def: &ClientDef) -> Detection {
    if let Ok(cache) = detect_cache().lock() {
        if let Some((_, hit)) = cache.get(def.id).filter(|(at, _)| at.elapsed() < DETECT_TTL) {
            return hit.clone();
        }
    }
    let fresh = detect_uncached(def);
    if let Ok(mut cache) = detect_cache().lock() {
        // 客户端数量封顶 14 个，这条清理几乎不会触发；写在这里是为了让缓存
        // 在任何情况下都只占常数内存。
        if cache.len() > 64 {
            cache.clear();
        }
        cache.insert(def.id, (std::time::Instant::now(), fresh.clone()));
    }
    fresh
}

/// 只解出「配置文件路径」，不做任何探测。
///
/// ── 为什么需要一条浅通道 ────────────────────────────────────
/// `resources::for_client` 要的只是 `config` 这一个路径（再据此推 parent、
/// 拼 MCP / 提示词 / Skill 的位置）。它原来调 `detect(def)`，而 `detect` 里
/// 最贵的一段是 `find_binary`：PATH 上每个目录 × 每个可执行名 × 每个 PATHEXT
/// 后缀各 stat 一次（本机 58 × 11 ≈ 4.4 万次 / 约 2.9 秒）。为了拿一个路径
/// 去扫一遍全盘可执行文件，纯属白花 —— 而且这条路径在「仪表盘连通数」「MCP
/// 配置快照」「提示词路径」上每次首屏都要走 13 遍。
///
/// 语义与 `detect(def).path` **完全一致**（同样的 override 优先级、同样的
/// opencode 回退），差别只是不产出 evidence / version / unsupported。
/// 需要那三样（接入页卡片、版本徽标）的调用方仍然走 `detect`。
pub fn config_path(def: &ClientDef) -> PathBuf {
    // 浅通道也要缓存：`preferences()` 每次都会读一遍 `agent-paths.json`，而
    // `for_client` 一次请求就要走 13 个客户端、每张卡还调两次 —— 不缓存等于把
    // 「4.4 万次 stat」换成「几十次 JSON 解析」，只是换个贵法。
    //
    // 缓存键是客户端 id。`save_override` 会显式 `forget_detection()`（同时清
    // 这张表），所以「改完路径 5 秒内还是旧值」只可能发生在用户手编 json 时。
    if let Ok(cache) = shallow_cache().lock() {
        if let Some((_, path)) = cache.get(def.id).filter(|(at, _)| at.elapsed() < DETECT_TTL) {
            return path.clone();
        }
    }
    let path = config_path_uncached(def);
    if let Ok(mut cache) = shallow_cache().lock() {
        if cache.len() > 64 {
            cache.clear();
        }
        cache.insert(def.id, (std::time::Instant::now(), path.clone()));
    }
    path
}

/// 浅路径缓存：只存 `id → PathBuf`，与 `DETECT_CACHE` 分开。
///
/// 合成一个 `Detection` 塞进探测缓存看起来能省一张表，但那会让 `detect()`
/// 拿到的是一条「没有 evidence、没有 version」的假记录 —— 接入页卡片会因此
/// 显示空版本号。两张表各管各的，语义干净。
type ShallowCache = std::sync::Mutex<std::collections::HashMap<&'static str, (std::time::Instant, PathBuf)>>;

fn shallow_cache() -> &'static ShallowCache {
    static CACHE: std::sync::OnceLock<ShallowCache> = std::sync::OnceLock::new();
    CACHE.get_or_init(|| std::sync::Mutex::new(std::collections::HashMap::new()))
}

fn config_path_uncached(def: &ClientDef) -> PathBuf {
    let override_path = preferences().ok().and_then(|mut items| items.paths.remove(def.id));
    // DSH 的路径不来自 `candidates()`：它要扫 `~/.dsh/profiles/*` 才能定出
    // 当前 profile 的补丁文件。走 [`dsh_shallow`]，语义与 `detect(def).path`
    // 完全一致，省掉的是 `find_binary` 那 4.4 万次 stat。
    if def.id == "dsh" {
        // `resource_path` 就是「该写哪个补丁文件」；没有时退回 `path`
        // （旧版 DSH 只有 `settings.yaml`，`resource_path` 为空）。
        // 两者在 modern 布局下是同一个值，所以这里与老 `dsh::config_path()`
        // 的返回值逐字一致。
        let found = dsh_shallow();
        return found.resource_path.unwrap_or(found.path);
    }
    let paths = candidates(def);
    override_path
        .as_ref()
        .map(PathBuf::from)
        .or_else(|| paths.iter().find(|path| path.is_file()).cloned())
        .or_else(|| {
            if def.id == "opencode" {
                paths.get(1).cloned()
            } else {
                paths.first().cloned()
            }
        })
        .unwrap_or_default()
}

/// DSH 的浅探测：只定路径，不扫 PATH。
///
/// 传 `None` 当 binary —— `installation::cli(None)` 立刻返回默认值（版本空、
/// 来源空），正好是「只要路径」想要的语义。桌面端探测有 10 秒缓存，代价可控。
///
/// 调用方要区分「路径能定」与「用户还没选定运行方式」时看 `.error`：
/// `Some` 说明 DSH 自己都说不清该写哪个文件，此时不该把它当成可写目标。
pub fn dsh_shallow() -> super::dsh_paths::Detection {
    super::dsh_paths::detect(None, path_override("dsh").as_deref().map(Path::new))
}

fn detect_uncached(def: &ClientDef) -> Detection {
    let preferences = preferences();
    let preference_error = preferences.as_ref().err().cloned();
    let override_path = preferences
        .ok()
        .and_then(|mut preferences| preferences.paths.remove(def.id));
    if def.id == "dsh" {
        let binary = find_binary(def);
        let found = super::dsh_paths::detect(binary.as_deref(), override_path.as_deref().map(Path::new));
        let mut evidence = Vec::new();
        if found.path.is_file() { evidence.push(format!("发现配置文件: {}", found.path.display())); }
        if let Some(binary) = binary { evidence.push(format!("发现客户端程序: {}", binary.display())); }
        if !found.source.as_os_str().is_empty() { evidence.push(format!("版本信息来源: {}", found.source.display())); }
        if override_path.is_some() { evidence.push("使用手动指定的配置路径".into()); }
        let note = preference_error.or(found.error);
        return Detection { path: found.path, detected: found.detected, version: found.version,
            unsupported: note.is_some(), note, evidence, config_targets: found.targets, resource_path: found.resource_path };
    }
    let paths = candidates(def);
    let path = override_path
        .as_ref()
        .map(PathBuf::from)
        .or_else(|| paths.iter().find(|path| path.is_file()).cloned())
        .or_else(|| {
            if def.id == "opencode" {
                paths.get(1).cloned()
            } else {
                paths.first().cloned()
            }
        })
        .unwrap_or_default();
    let mut evidence = Vec::new();
    if path.is_file() {
        evidence.push(format!("发现配置文件: {}", path.display()));
    }
    if def.id == "gemini-cli" && path.with_file_name(".env").is_file() {
        evidence.push(format!(
            "发现环境配置文件: {}",
            path.with_file_name(".env").display()
        ));
    }
    if override_path.is_some() {
        evidence.push("使用手动指定的配置路径".into());
    }
    let binary = find_binary(def);
    if let Some(ref binary) = binary {
        evidence.push(format!("发现客户端程序: {}", binary.display()));
    }
    let app = app_path(def);
    if let Some(app) = app {
        evidence.push(format!("发现桌面客户端: {}", app.display()));
    }
    let mut unsupported = preference_error.is_some();
    let mut note = preference_error;
    if def.id == "crush" {
        let dir = path.parent().unwrap_or(Path::new("."));
        if dir.join("crushrc").is_file() || dir.join(".crushrc").is_file() {
            unsupported = true;
            note = Some(
                "发现优先级更高的 crushrc；本页只管理 JSON，请在 Crush 中编辑实际配置。".into(),
            );
        }
    }
    let version = package_version(binary.as_deref(), def).unwrap_or_default();
    if def.id == "opencode"
        && version
            .split('.')
            .next()
            .and_then(|major| major.parse::<u32>().ok())
            .is_some_and(|major| major >= 2)
    {
        unsupported = true;
        note = Some("检测到 OpenCode v2；当前仅适配 v1 provider 格式，暂不开放写入。".into());
    }
    Detection {
        detected: !evidence.is_empty(),
        path,
        evidence,
        version,
        unsupported,
        note,
        config_targets: Vec::new(),
        resource_path: None,
    }
}

pub(super) fn find_binary(def: &ClientDef) -> Option<PathBuf> {
    let path = std::env::var_os("PATH").unwrap_or_default();
    let mut dirs: Vec<PathBuf> = std::env::split_paths(&path).collect();
    let home = home();
    dirs.extend([
        home.join(".local/bin"),
        home.join(".npm-global/bin"),
        home.join(".bun/bin"),
    ]);
    if let Some(appdata) = env_dir("APPDATA") {
        dirs.push(appdata.join("npm"));
    }
    if let Some(local) = env_dir("LOCALAPPDATA") {
        dirs.push(local.join("Microsoft/WinGet/Links"));
    }
    // ── 为什么不用 PATHEXT 全表 ────────────────────────────────
    // PATHEXT 通常有 11 项（`.VBS` / `.JS` / `.JSE` / `.WSF` / `.WSH` / `.MSC`
    // 都在里面）。给「找 CLI 程序」这件事用全表，等于让每个目录多付 7 次注定
    // 落空的 stat —— 58 个目录 × 7 就是 400 次纯浪费。只留能当命令行程序启动
    // 的四种，命中的集合一个不少：Node 装的 shim 是 `.cmd`，原生程序是 `.exe`。
    let extensions: Vec<String> = if cfg!(windows) {
        vec![".exe".into(), ".cmd".into(), ".bat".into(), ".com".into()]
    } else {
        Vec::new()
    };
    // 重复的 PATH 项、已经删掉的目录都先摘掉：不存在的目录里每个名字都要白试
    // 一遍，58 次 `is_dir` 换掉的是几百次注定失败的 `is_file`。
    let mut seen = std::collections::HashSet::new();
    let dirs: Vec<PathBuf> = dirs
        .into_iter()
        .filter(|dir| dir.is_dir())
        .filter(|dir| seen.insert(dir.to_string_lossy().to_ascii_lowercase()))
        .collect();
    for name in def.binaries {
        for dir in &dirs {
            let plain = dir.join(name);
            if plain.is_file()
                && (*name != "agent"
                    || plain
                        .to_string_lossy()
                        .to_ascii_lowercase()
                        .contains("cursor"))
            {
                return Some(plain);
            }
            for ext in &extensions {
                let path = dir.join(format!("{name}{ext}"));
                if path.is_file()
                    && (*name != "agent"
                        || path
                            .to_string_lossy()
                            .to_ascii_lowercase()
                            .contains("cursor"))
                {
                    return Some(path);
                }
            }
        }
    }
    None
}

fn app_path(def: &ClientDef) -> Option<PathBuf> {
    let mut paths = Vec::new();
    if cfg!(target_os = "macos") {
        for name in match def.id {
            "zcode" => vec!["ZCode"],
            "workbuddy" => vec!["WorkBuddy", "CodeBuddy"],
            "codex" => vec!["Codex"],
            "zed" => vec!["Zed"],
            _ => vec![],
        } {
            paths.push(PathBuf::from(format!("/Applications/{name}.app")));
            paths.push(home().join(format!("Applications/{name}.app")));
        }
    }
    if cfg!(windows) {
        if let Some(local) = env_dir("LOCALAPPDATA") {
            for (folder, exe) in match def.id {
                "zcode" => vec![("ZCode", "ZCode.exe")],
                "workbuddy" => vec![
                    ("WorkBuddy", "WorkBuddy.exe"),
                    ("CodeBuddy", "CodeBuddy.exe"),
                ],
                "zed" => vec![("Zed", "Zed.exe")],
                _ => vec![],
            } {
                paths.push(local.join("Programs").join(folder).join(exe));
                paths.push(local.join(folder).join(exe));
            }
        }
    }
    paths.into_iter().find(|path| {
        path.is_file() || path.extension().is_some_and(|ext| ext == "app") && path.is_dir()
    })
}

fn package_version(binary: Option<&Path>, def: &ClientDef) -> Option<String> {
    let package = match def.id {
        "claude-code" => "@anthropic-ai/claude-code",
        "codex" => "@openai/codex",
        "gemini-cli" => "@google/gemini-cli",
        "qoder" => "@qoder-ai/qodercli",
        "cline" => "cline",
        "opencode" => "opencode-ai",
        "kimi-code" => "@moonshot-ai/kimi-code",
        "dsh" => "@deepseek-ai/dsh",
        _ => return None,
    };
    let parent = binary?.parent()?;
    for path in [
        parent
            .join("node_modules")
            .join(package)
            .join("package.json"),
        parent
            .join("../lib/node_modules")
            .join(package)
            .join("package.json"),
    ] {
        let Ok(bytes) = std::fs::read(path) else {
            continue;
        };
        let Ok(value) = serde_json::from_slice::<serde_json::Value>(&bytes) else {
            continue;
        };
        if let Some(version) = value.get("version").and_then(serde_json::Value::as_str) {
            return Some(version.into());
        }
    }
    None
}

pub fn kimi_legacy(path: &Path) -> bool {
    path.parent()
        .and_then(Path::file_name)
        .is_some_and(|name| name == ".kimi")
}
