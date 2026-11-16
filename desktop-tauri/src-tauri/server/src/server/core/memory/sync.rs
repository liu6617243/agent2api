//! 记忆的 WebDAV 同步。
//!
//! ── 为什么是「整包 + 三方合并」而不是「逐文件双向同步」 ──────
//! 记忆文件是**长期事实**：一条结论被改了、删了、被新结论取代了，这些都要
//! 传到另一台机器上。逐文件同步要做「谁先改谁赢」的仲裁，而记忆恰恰是最不能
//! 仲裁的场景 —— 猜错一次，两台机器从此对同一条事实有两种说法，而且没人知道
//! 哪边对。
//!
//! 所以这里走**整包 + 三方合并**：
//!   · 每台设备把自己的记忆目录打成一个 tar.gz 包（清单 + 全部文件）；
//!   · 拉取远端包时，拿「本地当前状态」「上次同步后的基线」「远端状态」做三方
//!     比对：只有一边改过就自动采纳那一边，两边都改过且内容不同才进冲突队列；
//!   · 删除通过「包里没有这个文件」表达 —— 对方收到后会删掉本地对应文件。
//!
//! ── 包格式 ────────────────────────────────────────────────
//! tar.gz，内含：
//!   `manifest.json` —— `{device, createdAt, files: {相对路径: {hash, size}}}`
//!   其余条目        —— 记忆文件本身，路径与本地相对路径一致
//!
//! 索引库（`index.db`）不进包：它是可重建的派生数据，而且是二进制，
//! 三方合并不了。设备身份（`device.json`）也不进：每台机器一份。
//!
//! ── 为什么用 tar.gz 而不是 zip ─────────────────────────────
//! `tar` 与 `flate2` 已经在依赖树里（插件包解包用的就是它们），不引入新依赖。
//! zip 也行，但 tar 的流式读写更简单，且保留了 mtime 与权限位。

use std::collections::{BTreeMap, BTreeSet};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use flate2::read::GzDecoder;
use flate2::write::GzEncoder;
use flate2::Compression;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tar::{Archive, Builder, Header};

use super::config;
use super::index;
use super::rt;
use super::store;

/// 同步包清单里的单个文件。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileEntry {
    /// 内容哈希（sha256 前 16 位，与记忆条目的 hash 同口径）
    pub hash: String,
    pub size: u64,
}

/// 同步包清单。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Manifest {
    /// 打包设备 id
    pub device: String,
    /// 打包时刻（epoch ms）
    pub created_at: i64,
    /// 相对路径 → 文件信息
    pub files: BTreeMap<String, FileEntry>,
}

impl Manifest {
    fn empty(device: &str) -> Self {
        Self {
            device: device.to_string(),
            created_at: crate::server::logging::now_ms(),
            files: BTreeMap::new(),
        }
    }
}

/// 基线清单在 `sync_state` 里的键。
const BASE_KEY: &str = "sync.base.manifest";

/// 最近一次成功同步的时刻（`sync_state` 里的键）。
///
/// 不复用 `BASE_KEY` 的 `updated_at`：基线在「只有冲突、没有别的变化」时也会
/// 被重写，而用户想知道的是「上一次真正同步成功是什么时候」。
const LAST_KEY: &str = "sync.last.at";

/// 最近一次同步结果摘要。
const LAST_RESULT_KEY: &str = "sync.last.result";

/// 「不再同步这个文件」的路径集合（用户裁决为 ignore 的那些）。
///
/// 单独存一份而不是只把冲突记录标成 resolved：冲突记录是历史，跳过是长期
/// 生效的策略。下一轮同步仍然会看到两边内容不同，没有这份名单就会重新入队，
/// 用户会以为自己点了没用。
const SKIP_KEY: &str = "sync.skip.paths";

/// 参与同步的根目录（相对 memory 根）。
///
/// 只同步这三块：`general/`（日常与通用结论）、`projects/`（项目归档）、
/// `profile/`（画像）。`trash/` 不进包 —— 回收站是本机的临时状态，同步它
/// 会让另一台机器莫名其妙多出一堆「已删除」。删除的传播靠「包里没有这个文件」
/// 而不是靠回收站。
fn sync_roots() -> [&'static str; 3] {
    ["general", "projects", "profile"]
}

/// 不进同步包的文件名（后缀或整名）。
fn excluded(name: &str) -> bool {
    matches!(
        name,
        "index.db" | "index.db-wal" | "index.db-shm" | "device.json" | "manifest.json"
    )
}

// ── 本地清单 ─────────────────────────────────────────────────

/// 扫描本地记忆目录，得出清单。
pub fn local_manifest() -> Manifest {
    let root = store::root();
    let mut manifest = Manifest::empty(&store::device_id());
    for prefix in sync_roots() {
        let dir = root.join(prefix);
        collect(&dir, &root, &mut manifest);
    }
    manifest
}

fn collect(dir: &Path, root: &Path, manifest: &mut Manifest) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let name = entry.file_name().to_string_lossy().to_string();
        if excluded(&name) {
            continue;
        }
        if path.is_dir() {
            collect(&path, root, manifest);
            continue;
        }
        if path.extension().and_then(|ext| ext.to_str()) != Some("md") {
            continue;
        }
        let Ok(bytes) = std::fs::read(&path) else {
            continue;
        };
        let Ok(relative) = path.strip_prefix(root) else {
            continue;
        };
        let relative = relative.to_string_lossy().replace('\\', "/");
        manifest.files.insert(
            relative,
            FileEntry {
                hash: super::model::content_hash(&String::from_utf8_lossy(&bytes)),
                size: bytes.len() as u64,
            },
        );
    }
}

// ── 打包 / 解包 ──────────────────────────────────────────────

/// 把清单与文件内容打成 tar.gz。
fn pack(manifest: &Manifest, contents: &BTreeMap<String, String>) -> Result<Vec<u8>, String> {
    let encoder = GzEncoder::new(Vec::new(), Compression::default());
    let mut builder = Builder::new(encoder);
    let manifest_text = serde_json::to_vec_pretty(manifest).map_err(|error| error.to_string())?;
    append(&mut builder, "manifest.json", &manifest_text)?;
    for (path, text) in contents {
        append(&mut builder, path, text.as_bytes())?;
    }
    let encoder = builder
        .into_inner()
        .map_err(|error| format!("打包失败：{error}"))?;
    encoder.finish().map_err(|error| format!("压缩失败：{error}"))
}

fn append<W: Write>(builder: &mut Builder<W>, path: &str, bytes: &[u8]) -> Result<(), String> {
    let mut header = Header::new_gnu();
    header.set_size(bytes.len() as u64);
    header.set_mode(0o644);
    // `set_mtime` 只收 u64：now_ms 是 i64，负数（理论上不会出现）按 0 处理
    header.set_mtime(crate::server::logging::now_ms().max(0) as u64 / 1000);
    header.set_cksum();
    builder
        .append_data(&mut header, path, bytes)
        .map_err(|error| format!("写入 {path} 失败：{error}"))
}

/// 解包：返回清单与「相对路径 → 文本」。
fn unpack(bytes: &[u8]) -> Result<(Manifest, BTreeMap<String, String>), String> {
    let decoder = GzDecoder::new(bytes);
    let mut archive = Archive::new(decoder);
    let mut contents: BTreeMap<String, String> = BTreeMap::new();
    let mut manifest: Option<Manifest> = None;
    let entries = archive
        .entries()
        .map_err(|error| format!("读取同步包失败：{error}"))?;
    for entry in entries.flatten() {
        let path = entry
            .path()
            .map_err(|error| format!("同步包条目路径非法：{error}"))?
            .to_string_lossy()
            .replace('\\', "/");
        let mut text = String::new();
        let mut reader = entry;
        reader
            .read_to_string(&mut text)
            .map_err(|error| format!("读取同步包条目失败：{error}"))?;
        if path == "manifest.json" {
            manifest = serde_json::from_str(&text).ok();
            continue;
        }
        contents.insert(path, text);
    }
    let manifest = manifest.ok_or_else(|| "同步包缺少 manifest.json".to_string())?;
    Ok((manifest, contents))
}

// ── WebDAV 客户端 ────────────────────────────────────────────

fn base_url() -> String {
    config::text_of("sync.url").trim().trim_end_matches('/').to_string()
}

fn remote_dir() -> String {
    let configured = config::text_of("sync.remoteDir").trim().to_string();
    if configured.is_empty() {
        "agent2api-memory".to_string()
    } else {
        configured
    }
}

/// 远端目录地址。
fn remote_url() -> String {
    format!("{}/{}", base_url(), remote_dir())
}

fn auth_headers(builder: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
    let user = config::text_of("sync.username").trim().to_string();
    let password = config::text_of("sync.password").to_string();
    if user.is_empty() {
        return builder;
    }
    builder.basic_auth(user, Some(password))
}

/// 远端是否存在某个文件（HEAD）。
fn remote_exists(url: &str) -> bool {
    let client = crate::server::core::egress::client_for(None);
    let outcome = rt::block_on(async move {
        let response = auth_headers(client.head(url))
            .timeout(std::time::Duration::from_secs(20))
            .send()
            .await?;
        Ok::<_, reqwest::Error>(response.status())
    });
    matches!(outcome, Ok(status) if status.is_success())
}

/// 列出远端目录下的包（PROPFIND，Depth: 1）。
///
/// 不用 XML 解析器：只取 `<D:href>` 的值，够用了 —— 少一个依赖，也少一处
/// 「不同 WebDAV 实现的命名空间前缀不一样」的坑（我们按 `href` 里是否以
/// `.tar.gz` 结尾来筛，不看 displayname）。
fn list_remote() -> Vec<String> {
    let url = remote_url();
    let client = crate::server::core::egress::client_for(None);
    let method = reqwest::Method::from_bytes(b"PROPFIND").unwrap_or(reqwest::Method::GET);
    let body = r#"<?xml version="1.0" encoding="utf-8" ?>
<D:propfind xmlns:D="DAV:"><D:prop><D:displayname/><D:getcontentlength/><D:getlastmodified/></D:prop></D:propfind>"#;
    let outcome = rt::block_on(async move {
        let response = auth_headers(client.request(method, &url))
            .timeout(std::time::Duration::from_secs(30))
            .header("depth", "1")
            .header("content-type", "application/xml; charset=utf-8")
            .body(body.to_string())
            .send()
            .await?;
        let status = response.status();
        let text = response.text().await.unwrap_or_default();
        Ok::<_, reqwest::Error>((status, text))
    });
    let Ok((status, text)) = outcome else {
        return Vec::new();
    };
    if !status.is_success() {
        crate::server::logging::log(
            "[Memory]",
            &format!("⚠️ WebDAV 列目录失败：HTTP {}", status.as_u16()),
        );
        return Vec::new();
    }
    let mut out = Vec::new();
    // 不同 WebDAV 实现的命名空间前缀大小写不一（`<D:href>` / `<d:href>` /
    // 无前缀的 `<href>`）。不解析 XML，只按「`href>` 到下一个 `<`」取文本，
    // 三种写法都能命中。
    let lowered = text.to_ascii_lowercase();
    let mut cursor = 0usize;
    while let Some(found) = lowered[cursor..].find("href>") {
        let start = cursor + found + "href>".len();
        let Some(end) = text[start..].find('<') else {
            break;
        };
        let href = text[start..start + end].trim();
        let name = href.rsplit('/').next().unwrap_or("").to_string();
        if name.ends_with(".tar.gz") {
            out.push(name);
        }
        cursor = start + end;
    }
    out.sort();
    out.dedup();
    out
}

/// 下载一个远端包。
fn fetch_package(name: &str) -> Result<Vec<u8>, String> {
    let url = format!("{}/{}", remote_url(), name);
    let client = crate::server::core::egress::client_for(None);
    let outcome = rt::block_on(async move {
        let response = auth_headers(client.get(&url))
            .timeout(std::time::Duration::from_secs(120))
            .send()
            .await?;
        let status = response.status();
        let bytes = response.bytes().await.unwrap_or_default();
        Ok::<_, reqwest::Error>((status, bytes))
    });
    match outcome {
        Ok((status, bytes)) if status.is_success() => Ok(bytes.to_vec()),
        Ok((status, _)) => Err(format!("下载 {name} 失败：HTTP {}", status.as_u16())),
        Err(error) => Err(format!("下载 {name} 失败：{}", error.message())),
    }
}

/// 上传一个包。
fn push_package(name: &str, bytes: &[u8]) -> Result<(), String> {
    let url = format!("{}/{}", remote_url(), name);
    let client = crate::server::core::egress::client_for(None);
    let payload = bytes.to_vec();
    let outcome = rt::block_on(async move {
        let response = auth_headers(client.put(&url))
            .timeout(std::time::Duration::from_secs(180))
            .header("content-type", "application/gzip")
            .body(payload)
            .send()
            .await?;
        let status = response.status();
        let text = response.text().await.unwrap_or_default();
        Ok::<_, reqwest::Error>((status, text))
    });
    match outcome {
        Ok((status, _)) if status.is_success() => Ok(()),
        Ok((status, text)) => Err(format!(
            "上传 {name} 失败：HTTP {}{}",
            status.as_u16(),
            if text.is_empty() {
                String::new()
            } else {
                format!(" · {}", text.chars().take(200).collect::<String>())
            }
        )),
        Err(error) => Err(format!("上传 {name} 失败：{}", error.message())),
    }
}

/// 确保远端目录存在（MKCOL）。已存在时 WebDAV 返回 405，算成功。
fn ensure_remote_dir() {
    let url = remote_url();
    if remote_exists(&url) {
        return;
    }
    let client = crate::server::core::egress::client_for(None);
    let method = reqwest::Method::from_bytes(b"MKCOL").unwrap_or(reqwest::Method::GET);
    let outcome = rt::block_on(async move {
        let response = auth_headers(client.request(method, &url))
            .timeout(std::time::Duration::from_secs(20))
            .send()
            .await?;
        Ok::<_, reqwest::Error>(response.status())
    });
    match outcome {
        Ok(status) if status.is_success() || status.as_u16() == 405 => {}
        Ok(status) => crate::server::logging::log(
            "[Memory]",
            &format!("⚠️ WebDAV 建目录失败：HTTP {}", status.as_u16()),
        ),
        Err(error) => crate::server::logging::log(
            "[Memory]",
            &format!("⚠️ WebDAV 建目录失败：{}", error.message()),
        ),
    }
}

// ── 三方合并 ─────────────────────────────────────────────────

/// 合并结果。
struct Merge {
    /// 要写到本地的（路径 → 内容）
    down: Vec<(String, String)>,
    /// 要从本地删掉的（远端已删）
    delete: Vec<String>,
    /// 冲突（不进包，等人工裁决）
    conflicts: Vec<(String, String, String, String)>,
}

/// 三方合并：`base`（上次同步后的基线）/ `local` / `remote`。
///
/// 判定规则（每条路径独立）：
/// - 两边都一样 → 无事发生；
/// - 只有远端变了（本地 == 基线）→ 采纳远端；
/// - 只有本地变了（远端 == 基线）→ 保留本地（上行）；
/// - 两边都变了且结果不同 → 冲突。
/// - 基线里有、远端包里没有 → 远端删了 → 本地跟着删。
/// - 基线没有、远端有 → 远端新增 → 直接写下来（除非本地也有一份不同内容）。
fn merge(
    base: &BTreeMap<String, FileEntry>,
    local: &BTreeMap<String, FileEntry>,
    remote: &Manifest,
    remote_files: &BTreeMap<String, String>,
) -> Merge {
    let mut down = Vec::new();
    let mut delete = Vec::new();
    let mut conflicts = Vec::new();
    let mut paths: BTreeSet<&String> = BTreeSet::new();
    paths.extend(base.keys());
    paths.extend(local.keys());
    paths.extend(remote.files.keys());

    for path in paths {
        let base_entry = base.get(path);
        let local_entry = local.get(path);
        let remote_entry = remote.files.get(path);
        let local_hash = local_entry.map(|item| item.hash.as_str());
        let remote_hash = remote_entry.map(|item| item.hash.as_str());
        let base_hash = base_entry.map(|item| item.hash.as_str());

        match (local_hash, remote_hash) {
            (None, None) => {}
            (Some(l), None) => {
                // 远端包里没有这条：远端删了它。本地也刚删 → 一致；
                // 本地还在且相对基线没改过 → 跟着删。
                if base_hash == Some(l) {
                    delete.push(path.clone());
                }
            }
            (None, Some(_)) => {
                // 远端新增：直接写下来。
                if let Some(text) = remote_files.get(path) {
                    down.push((path.clone(), text.clone()));
                }
            }
            (Some(l), Some(r)) => {
                if l == r {
                    continue;
                }
                let local_changed = base_hash != Some(l);
                let remote_changed = base_hash != Some(r);
                match (local_changed, remote_changed) {
                    (false, true) => {
                        if let Some(text) = remote_files.get(path) {
                            down.push((path.clone(), text.clone()));
                        }
                    }
                    (true, false) => {}
                    (true, true) => {
                        conflicts.push((
                            path.clone(),
                            l.to_string(),
                            r.to_string(),
                            base_hash.unwrap_or_default().to_string(),
                        ));
                    }
                    (false, false) => {}
                }
            }
        }
    }
    Merge {
        down,
        delete,
        conflicts,
    }
}

// ── 一次同步 ─────────────────────────────────────────────────

/// 跑一次同步。`manual` 为真时忽略自动开关。
pub fn run(manual: bool) -> super::enrich::Outcome {
    use super::enrich::Outcome;

    if !config::bool_of(config::ENABLE_KEY) {
        return Outcome::Skipped("记忆中枢已关闭".into());
    }
    if base_url().is_empty() {
        return Outcome::Skipped("未配置 WebDAV 地址（sync.url）".into());
    }
    if !manual && !config::bool_of("sync.enabled") {
        return Outcome::Skipped("同步总开关已关闭（sync.enabled）".into());
    }
    if !manual && !config::bool_of("sync.auto") {
        return Outcome::Skipped("自动同步已关闭（sync.auto）".into());
    }

    let device = store::device_id();
    let mut pushed = 0usize;
    let mut pulled = 0usize;
    let mut deleted = 0usize;
    let mut conflicted = 0usize;
    let mut skipped = 0usize;

    ensure_remote_dir();

    // ── 下行：拉其他设备的包 ──
    let base = load_base();
    let skip = load_skip();
    for name in list_remote() {
        if name == own_package_name(&device) {
            continue;
        }
        let bytes = match fetch_package(&name) {
            Ok(bytes) => bytes,
            Err(error) => {
                index::global().sync_log_add(&name, "error", &error, false);
                crate::server::logging::log("[Memory]", &format!("⚠️ {error}"));
                continue;
            }
        };
        let (manifest, files) = match unpack(&bytes) {
            Ok(pair) => pair,
            Err(error) => {
                index::global().sync_log_add(&name, "error", &error, false);
                crate::server::logging::log("[Memory]", &format!("⚠️ {error}"));
                continue;
            }
        };
        // 每台设备都重新扫一次本地清单：上一台设备的包可能刚改过本地文件，
        // 用循环外那份旧清单做三方比对会把「刚写下来的内容」当成本地改动。
        let local_now = local_manifest();
        let result = merge(&base, &local_now.files, &manifest, &files);
        // 本台设备这一轮的跳过条数。不能用全局 `skipped` 算 —— 那是累加值，
        // 第二台设备的日志会把第一台的跳过数也扣掉。
        let mut skipped_here = 0usize;
        for (path, text) in &result.down {
            if skip.contains(path) {
                skipped += 1;
                skipped_here += 1;
                continue;
            }
            if write_file(path, text) {
                pulled += 1;
            }
        }
        for path in &result.delete {
            if remove_file(path) {
                deleted += 1;
            }
        }
        for (path, local_hash, remote_hash, base_hash) in &result.conflicts {
            if skip.contains(path) {
                skipped += 1;
                skipped_here += 1;
                continue;
            }
            conflicted += 1;
            index::global().conflict_enqueue(
                path,
                &manifest.device,
                local_hash,
                remote_hash,
                base_hash,
                "本地与远端都改过同一份记忆",
            );
        }
        index::global().sync_log_add(
            &manifest.device,
            "pull",
            &format!(
                "下行 {} 条 / 删除 {} 条 / 冲突 {} 条 / 跳过 {} 条",
                result.down.len().saturating_sub(skipped_here),
                result.delete.len(),
                result.conflicts.len(),
                skipped_here
            ),
            true,
        );
    }

    // ── 上行：把当前状态整包推上去 ──
    let manifest = local_manifest();
    let mut contents: BTreeMap<String, String> = BTreeMap::new();
    for path in manifest.files.keys() {
        if let Some(text) = read_file(path) {
            contents.insert(path.clone(), text);
        }
    }
    let limit_mb = config::number("sync.packSizeLimitMB").max(1.0) as usize;
    let total: usize = contents.values().map(|text| text.len()).sum();
    if total > limit_mb * 1024 * 1024 {
        let message = format!("记忆总量 {} MB 超过单包上限 {} MB，本次不同步", total / 1024 / 1024, limit_mb);
        index::global().sync_log_add(&device, "error", &message, false);
        return Outcome::Skipped(message);
    }
    match pack(&manifest, &contents) {
        Ok(bytes) => match push_package(&own_package_name(&device), &bytes) {
            Ok(()) => {
                pushed += 1;
                index::global().sync_log_add(&device, "push", &format!("上行 {} 个文件", contents.len()), true);
            }
            Err(error) => {
                index::global().sync_log_add(&device, "error", &error, false);
                return Outcome::Failed(error);
            }
        },
        Err(error) => {
            index::global().sync_log_add(&device, "error", &error, false);
            return Outcome::Failed(error);
        }
    }

    // ── 基线更新 ──
    // 注意：基线只记「同步完成后双方一致的状态」。冲突条目不进基线 ——
    // 否则下一轮就会把「还没裁决的冲突」当成已经一致，冲突就永远看不见了。
    let mut next_base = manifest.files.clone();
    for path in &conflicted_paths() {
        next_base.remove(path);
    }
    save_base(&next_base);

    if pulled > 0 || deleted > 0 {
        let _ = super::index::global().rebuild(&super::store::load_all());
    }

    let summary = json!({
        "device": device,
        "pulled": pulled,
        "deleted": deleted,
        "pushed": pushed,
        "conflicts": conflicted,
        "skipped": skipped,
        "files": manifest.files.len(),
    });
    let now = crate::server::logging::now_ms();
    index::global().sync_set(LAST_KEY, &now.to_string());
    index::global().sync_set(LAST_RESULT_KEY, &summary.to_string());

    Outcome::Done(summary)
}

fn conflicted_paths() -> Vec<String> {
    index::global()
        .conflict_list(200)
        .into_iter()
        .filter(|item| item.get("status").and_then(Value::as_str) == Some("pending"))
        .filter_map(|item| item.get("path").and_then(Value::as_str).map(str::to_string))
        .collect()
}

fn own_package_name(device: &str) -> String {
    format!("{device}.tar.gz")
}

// ── 文件读写 ─────────────────────────────────────────────────

fn read_file(relative: &str) -> Option<String> {
    std::fs::read(store::abs(relative))
        .ok()
        .map(|bytes| String::from_utf8_lossy(&bytes).to_string())
}

fn write_file(relative: &str, text: &str) -> bool {
    let path = store::abs(relative);
    if let Some(parent) = path.parent() {
        if std::fs::create_dir_all(parent).is_err() {
            return false;
        }
    }
    // 写入前先备份：同步进来的内容是远端写的，出了问题要能退回去
    if config::bool_of("storage.backupBeforeWrite") && path.exists() {
        let backup = path.with_extension("md.bak");
        let _ = std::fs::copy(&path, backup);
    }
    std::fs::write(&path, text).is_ok()
}

fn remove_file(relative: &str) -> bool {
    let path = store::abs(relative);
    if path.exists() {
        return std::fs::remove_file(&path).is_ok();
    }
    false
}

// ── 基线存取 ─────────────────────────────────────────────────

fn load_base() -> BTreeMap<String, FileEntry> {
    index::global()
        .sync_get(BASE_KEY)
        .and_then(|(raw, _)| serde_json::from_str::<BTreeMap<String, FileEntry>>(&raw).ok())
        .unwrap_or_default()
}

fn save_base(files: &BTreeMap<String, FileEntry>) {
    if let Ok(raw) = serde_json::to_string(files) {
        index::global().sync_set(BASE_KEY, &raw);
    }
}

// ── 跳过名单 ─────────────────────────────────────────────────

fn load_skip() -> BTreeSet<String> {
    index::global()
        .sync_get(SKIP_KEY)
        .and_then(|(raw, _)| serde_json::from_str::<Vec<String>>(&raw).ok())
        .map(|items| items.into_iter().collect())
        .unwrap_or_default()
}

fn save_skip(paths: &BTreeSet<String>) {
    let list: Vec<&String> = paths.iter().collect();
    if let Ok(raw) = serde_json::to_string(&list) {
        index::global().sync_set(SKIP_KEY, &raw);
    }
}

/// 把某条路径加入 / 移出跳过名单。
pub fn set_skip(path: &str, skip: bool) {
    let mut paths = load_skip();
    if skip {
        paths.insert(path.to_string());
    } else {
        paths.remove(path);
    }
    save_skip(&paths);
}

/// 跳过名单（同步页「已忽略的文件」）。
pub fn skip_list() -> Value {
    json!({ "paths": load_skip().into_iter().collect::<Vec<_>>() })
}

// ── 状态与查询 ───────────────────────────────────────────────

/// 同步页顶部状态。
pub fn status() -> Value {
    let configured = !base_url().is_empty();
    let device = store::device_id();
    let last_at = index::global()
        .sync_get(LAST_KEY)
        .and_then(|(raw, _)| raw.trim().parse::<i64>().ok())
        .unwrap_or(0);
    let last_result = index::global()
        .sync_get(LAST_RESULT_KEY)
        .and_then(|(raw, _)| serde_json::from_str::<Value>(&raw).ok())
        .unwrap_or(Value::Null);
    json!({
        "configured": configured,
        "url": base_url(),
        "remoteDir": remote_dir(),
        "device": device,
        "deviceName": config::text_of("sync.deviceName"),
        "enabled": config::bool_of("sync.enabled"),
        "auto": config::bool_of("sync.auto"),
        "intervalMin": config::number("sync.intervalMin") as i64,
        "packSizeLimitMB": config::number("sync.packSizeLimitMB") as i64,
        "excludeIndex": config::bool_of("sync.excludeIndex"),
        "conflicts": index::global().conflict_count(),
        "skipped": load_skip().len(),
        "baseFiles": load_base().len(),
        "lastSyncAt": last_at,
        "lastResult": last_result,
        "localFiles": local_manifest().files.len(),
        "package": own_package_name(&device),
    })
}

/// 远端设备列表。
pub fn devices() -> Value {
    if base_url().is_empty() {
        return json!({ "devices": [], "configured": false });
    }
    let own = own_package_name(&store::device_id());
    let devices: Vec<Value> = list_remote()
        .into_iter()
        .map(|name| {
            json!({
                "package": name,
                "device": name.trim_end_matches(".tar.gz").to_string(),
                "self": name == own,
            })
        })
        .collect();
    json!({ "devices": devices, "configured": true })
}

/// 冲突列表。
pub fn conflicts() -> Value {
    json!({
        "conflicts": index::global().conflict_list(200),
        "count": index::global().conflict_count(),
    })
}

/// 裁决一条冲突。`resolution` = `local` / `remote` / `ignore`。
pub fn resolve_conflict(id: i64, resolution: &str) -> Result<(), String> {
    let Some(row) = index::global().conflict_get(id) else {
        return Err("冲突记录不存在".into());
    };
    let path = row
        .get("path")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let device = row
        .get("device")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    match resolution {
        // 忽略：把这条路径拉进跳过名单。只把冲突记录标成 resolved 不够 ——
        // 下一轮同步两边内容还是不同，没有跳过名单就会重新入队。
        "ignore" => {
            set_skip(&path, true);
        }
        // 保留本地：基线改成本地当前哈希。下一轮「本地 == 基线、远端变了」，
        // 于是本地的内容会被推上去，对方再拉下来 —— 正是用户想要的结果。
        // 这里如果改成「从基线里删掉」，下一轮会变成「两边都改过」，又冲突一次。
        "local" => {
            let mut base = load_base();
            match read_file(&path) {
                Some(text) => {
                    base.insert(
                        path.clone(),
                        FileEntry {
                            hash: super::model::content_hash(&text),
                            size: text.len() as u64,
                        },
                    );
                }
                None => {
                    base.remove(&path);
                }
            }
            save_base(&base);
            set_skip(&path, false);
        }
        "remote" => {
            // 取远端内容：重新拉一次那个设备的包，把这一条写下来。
            // `device` 可能来自两种来源：`manifest.device`（裸 id）或同步流水里
            // 的包名（已带 `.tar.gz`）。统一按裸 id 拼包名，避免出现
            // `xxx.tar.gz.tar.gz`。
            let device_id = device.trim_end_matches(".tar.gz");
            let name = own_package_name(device_id);
            let bytes = fetch_package(&name)?;
            let (_manifest, files) = unpack(&bytes)?;
            if let Some(text) = files.get(&path) {
                write_file(&path, text);
            } else {
                remove_file(&path);
            }
            // 裁决完之后这条路径两边已经一致，补进基线，下一轮不再当冲突
            let mut base = load_base();
            match files.get(&path) {
                Some(text) => {
                    base.insert(
                        path.clone(),
                        FileEntry {
                            hash: super::model::content_hash(text),
                            size: text.len() as u64,
                        },
                    );
                }
                None => {
                    base.remove(&path);
                }
            }
            save_base(&base);
            set_skip(&path, false);
        }
        other => return Err(format!("未知的裁决方式：{other}")),
    }
    index::global().conflict_resolve(id, resolution);
    index::global().sync_log_add(&device, "conflict", &format!("{path} → {resolution}"), true);
    Ok(())
}

/// 同步流水。
pub fn log_entries(limit: usize) -> Value {
    json!({ "entries": index::global().sync_log_list(limit.clamp(1, 500)) })
}

/// 测连接：PROPFIND 远端目录。
///
/// 单独做一个「测试」而不是让用户点「立即同步」看结果：同步会真的写文件，
/// 而用户在这个阶段只想确认地址、账号、密码对不对。这里只读不写。
pub fn test_connection() -> Value {
    if base_url().is_empty() {
        return json!({ "ok": false, "message": "未填写 WebDAV 地址" });
    }
    let url = remote_url();
    let client = crate::server::core::egress::client_for(None);
    let method = reqwest::Method::from_bytes(b"PROPFIND").unwrap_or(reqwest::Method::GET);
    let body = r#"<?xml version="1.0" encoding="utf-8" ?>
<D:propfind xmlns:D="DAV:"><D:prop><D:resourcetype/></D:prop></D:propfind>"#;
    let outcome = rt::block_on(async move {
        let response = auth_headers(client.request(method, &url))
            .timeout(std::time::Duration::from_secs(20))
            .header("depth", "0")
            .header("content-type", "application/xml; charset=utf-8")
            .body(body.to_string())
            .send()
            .await?;
        let status = response.status();
        let text = response.text().await.unwrap_or_default();
        Ok::<_, reqwest::Error>((status, text))
    });
    match outcome {
        Ok((status, _)) if status.is_success() => json!({
            "ok": true,
            "message": format!("连接正常（HTTP {}）", status.as_u16()),
            "url": url,
            "packages": list_remote().len(),
        }),
        // 404 说明服务器在、目录还没建 —— 这不算连接失败，第一次同步会自动建
        Ok((status, _)) if status.as_u16() == 404 => json!({
            "ok": true,
            "message": "服务器可达，远端目录尚未创建（首次同步会自动创建）",
            "url": url,
            "packages": 0,
        }),
        Ok((status, _)) if status.as_u16() == 401 || status.as_u16() == 403 => json!({
            "ok": false,
            "message": format!("认证失败（HTTP {}）：检查用户名与密码", status.as_u16()),
            "url": url,
        }),
        Ok((status, text)) => json!({
            "ok": false,
            "message": format!(
                "服务器返回 HTTP {}{}",
                status.as_u16(),
                if text.is_empty() { String::new() } else { format!(" · {}", text.chars().take(160).collect::<String>()) }
            ),
            "url": url,
        }),
        Err(error) => json!({ "ok": false, "message": error.message(), "url": url }),
    }
}

/// 立刻同步一次（同步页的「立即同步」按钮）。
pub fn sync_now() -> super::enrich::Outcome {
    run(true)
}

/// 同步包在远端的名字（调试用）。
pub fn package_name() -> String {
    own_package_name(&store::device_id())
}

/// 本地清单快照（索引页的「同步预览」）。
pub fn local_snapshot() -> Value {
    let manifest = local_manifest();
    json!({
        "device": manifest.device,
        "createdAt": manifest.created_at,
        "files": manifest.files.iter().map(|(path, entry)| json!({
            "path": path,
            "hash": entry.hash,
            "size": entry.size,
        })).collect::<Vec<_>>(),
    })
}

/// 清理本机同步缓存（不动记忆本身）。
pub fn purge_cache() -> Result<usize, String> {
    let root = store::root();
    let mut removed = 0usize;
    if let Ok(entries) = std::fs::read_dir(&root) {
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().to_string();
            if name.ends_with(".md.bak") {
                if std::fs::remove_file(entry.path()).is_ok() {
                    removed += 1;
                }
            }
        }
    }
    Ok(removed)
}

/// 同步包所在目录（给「打开目录」用）。
pub fn package_dir() -> PathBuf {
    store::root()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(hash: &str) -> FileEntry {
        FileEntry {
            hash: hash.to_string(),
            size: 1,
        }
    }

    #[test]
    fn same_content_is_not_a_conflict() {
        let mut base = BTreeMap::new();
        base.insert("a.md".to_string(), entry("h1"));
        let mut local = BTreeMap::new();
        local.insert("a.md".to_string(), entry("h1"));
        let mut remote = Manifest::empty("dev");
        remote.files.insert("a.md".to_string(), entry("h1"));
        let result = merge(&base, &local, &remote, &BTreeMap::new());
        assert!(result.conflicts.is_empty());
        assert!(result.down.is_empty());
    }

    #[test]
    fn remote_only_change_is_taken_down() {
        let mut base = BTreeMap::new();
        base.insert("a.md".to_string(), entry("h1"));
        let mut local = BTreeMap::new();
        local.insert("a.md".to_string(), entry("h1"));
        let mut remote = Manifest::empty("dev");
        remote.files.insert("a.md".to_string(), entry("h2"));
        let mut files = BTreeMap::new();
        files.insert("a.md".to_string(), "new".to_string());
        let result = merge(&base, &local, &remote, &files);
        assert_eq!(result.down.len(), 1);
        assert!(result.conflicts.is_empty());
    }

    #[test]
    fn both_sides_changed_is_a_conflict() {
        let mut base = BTreeMap::new();
        base.insert("a.md".to_string(), entry("h1"));
        let mut local = BTreeMap::new();
        local.insert("a.md".to_string(), entry("h2"));
        let mut remote = Manifest::empty("dev");
        remote.files.insert("a.md".to_string(), entry("h3"));
        let result = merge(&base, &local, &remote, &BTreeMap::new());
        assert_eq!(result.conflicts.len(), 1);
        assert!(result.down.is_empty());
    }

    #[test]
    fn remote_deletion_propagates() {
        let mut base = BTreeMap::new();
        base.insert("a.md".to_string(), entry("h1"));
        let local = BTreeMap::new();
        let remote = Manifest::empty("dev");
        let result = merge(&base, &local, &remote, &BTreeMap::new());
        assert_eq!(result.delete.len(), 1);
    }
}
