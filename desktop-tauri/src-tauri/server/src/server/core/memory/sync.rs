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
    // 用户手填时经常带前导斜杠（`/agent2api-memory`），而 `remote_url()` 自己会
    // 补那一个 —— 不剥掉就拼出 `https://host//agent2api-memory`。多数 WebDAV
    // 服务端能容忍双斜杠，但有些（含部分反代）会 404，而错误信息里那串双斜杠
    // 看起来像客户端 bug。同理，尾随斜杠会让空目录变成一个多余的段。
    let trimmed = configured.trim_matches('/').to_string();
    if trimmed.is_empty() {
        "agent2api-memory".to_string()
    } else {
        trimmed
    }
}

/// 远端目录地址。
fn remote_url() -> String {
    format!("{}/{}", base_url(), remote_dir())
}

/// 一个模块的同步目录：名字 + 本仓是否真的实现了它。
///
/// ── 为什么把「未实现」写进结构里而不是藏起来 ──────────────────
/// 界面上要有「模块根目录」那一组输入框（五个模块在同一个网盘里各占一个目录），
/// 否则用户没法理解「记忆同步只是这一套里的一支」。但本仓只有记忆实现了同步，
/// 另外四个的目录名只是**存下来备用**。把 `implemented` 一起下发，页面就能给
/// 未实现的模块画灰按钮 + 写「未实现」，而不是做一个点了没反应的「立即同步」。
pub struct ModuleDir {
    pub id: &'static str,
    pub label: &'static str,
    pub key: &'static str,
    pub def: &'static str,
    pub implemented: bool,
}

pub const MODULES: &[ModuleDir] = &[
    ModuleDir { id: "skills", label: "技能仓库", key: "sync.modules.skills", def: "/agent-skills", implemented: false },
    ModuleDir { id: "dosage", label: "用量统计", key: "sync.modules.dosage", def: "/dosage-sync", implemented: false },
    ModuleDir { id: "pool", label: "号池管家号池", key: "sync.modules.pool", def: "/agenthub-proxy", implemented: false },
    // 记忆自己的目录走 `sync.remoteDir`（历史键，已有用户在用）。这里只作为
    // 展示行出现，读写都落到 `sync.remoteDir`，避免同一件事两个键。
    ModuleDir { id: "memory", label: "记忆中枢", key: "sync.modules.memory", def: "/agenthub-memory", implemented: true },
    ModuleDir { id: "prompts", label: "提示词库", key: "sync.modules.prompts", def: "/agenthub-prompts", implemented: false },
];

/// 模块目录的生效值。
///
/// 记忆那行恒等于 `sync.remoteDir`（见 `MODULES` 注释）。其余四个：
///   · 没显式配过 → 用默认名（界面上显示 `/agent-skills` 这种）；
///   · 显式配成空串 → **空串**，语义是「这个模块不参与同步」，界面把这一行
///     画成「已关闭」而不是回落到默认名 —— 否则用户没法表达「我不要它」。
pub fn module_dir(item: &ModuleDir) -> String {
    if item.id == "memory" {
        return format!("/{}", remote_dir());
    }
    let explicit = config::stored().values.contains_key(item.key);
    resolve_module_path(item.def, &config::text_of(item.key), explicit)
}

/// 「默认 / 显式空 / 自定义」三态归一成最终目录（`module_dir` 的纯函数内核）。
///
/// 单独拆出来是为了**可测**：`module_dir` 要读磁盘上的 config.json，测试里没法
/// 断言「显式配成空串」这种只存在于内存的状态；这里三个入参把那种状态直接表达
/// 成 `explicit = true, raw = ""`。
fn resolve_module_path(def: &str, raw: &str, explicit: bool) -> String {
    let raw = raw.trim();
    if raw.is_empty() {
        // 显式配成空 = 用户主动关掉这个模块；没配过 = 用默认。
        // 这两者在界面上是「已关闭」与「/agent-skills」，不能混成一个。
        return if explicit { String::new() } else { def.to_string() };
    }
    if raw.starts_with('/') {
        raw.to_string()
    } else {
        format!("/{raw}")
    }
}

/// 模块目录清单（同步页「模块根目录」那一组）。
///
/// `def` 一起下发：界面要在标签后面写「（默认 /agent-skills）」—— 用户改过目录
/// 之后得知道「原来应该长什么样」，否则想还原只能去翻文档。
pub fn modules_json() -> Value {
    Value::Array(
        MODULES
            .iter()
            .map(|item| {
                json!({
                    "id": item.id,
                    "label": item.label,
                    "key": item.key,
                    "dir": module_dir(item),
                    "def": item.def,
                    "implemented": item.implemented,
                })
            })
            .collect(),
    )
}

fn auth_headers(builder: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
    let user = config::text_of("sync.username").trim().to_string();
    // `text_of` 对 Secret 类的键会解密，这里拿到的就是明文（只在本次请求的内存里）
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

/// 一个远端包：文件名 + 服务器上的修改时刻（epoch ms，取不到就是 0）。
///
/// 带修改时间是给「设备列表 → 最后同步」用的。PROPFIND 本来就在请求
/// `getlastmodified`，原来只是解析时把这段丢掉了 —— 于是界面只能说
/// 「远端有这个包」，说不出「它上次是什么时候传的」。
pub struct RemotePack {
    pub name: String,
    pub modified_at: i64,
}

/// 列出远端目录下的包（PROPFIND，Depth: 1）。
///
/// 不用 XML 解析器：只取 `<D:href>` 的值，够用了 —— 少一个依赖，也少一处
/// 「不同 WebDAV 实现的命名空间前缀不一样」的坑（我们按 `href` 里是否以
/// `.tar.gz` 结尾来筛，不看 displayname）。
fn list_remote_packs() -> Vec<RemotePack> {
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
    let mut out = parse_propfind(&text);
    out.sort_by(|a, b| a.name.cmp(&b.name));
    out.dedup_by(|a, b| a.name == b.name);
    out
}

/// 把一个 PROPFIND 响应文本里的包列表取出来（供测试与网络版共用）。
///
/// 单独成函数是为了能离线测（`list_remote_packs` 里有网络与鉴权）。测试覆盖
/// 的是这段最容易错的字符串游标逻辑 —— 它曾经用错偏移量，把每个包的时间都
/// 从文档开头取。
fn parse_propfind(text: &str) -> Vec<RemotePack> {
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
            // `<D:response>` 是一个响应块：从这次 href 结束的位置往后找同块里的
            // `getlastmodified`。找不到就往后看「够不够近」—— 块与块之间通常
            // 只隔几十字节的属性，取 1200 字节这个上限足够，又不会跨到
            // 下一个包的 response 块里。
            //
            // 注意 `end` 是相对 `text[start..]` 的偏移，绝对位置得加上 `start`。
            // 直接拿 `end` 切片会指回文档开头（撞巧还能切在 UTF-8 字符中间 panic）。
            let after = start + end;
            let window_end = char_boundary_at_or_before(text, (after + 1200).min(text.len()));
            let window = &text[after..window_end];
            out.push(RemotePack {
                name,
                modified_at: last_modified(window),
            });
        }
        cursor = start + end;
    }
    out
}

/// 把一个字节下标收到最近的「字符起点」上（向前收）。
///
/// 只用来给字符串切片算窗口终点：`after + 1200` 是字节算术，中文目录名会把
/// 它落到某个字符的中间，Rust 切片在这里会 panic。往回收一个字符不影响窗口
/// 用途 —— 我们只是想「往后看足够远」，少一两个字节没有意义。
fn char_boundary_at_or_before(text: &str, index: usize) -> usize {
    let mut at = index.min(text.len());
    while at > 0 && !text.is_char_boundary(at) {
        at -= 1;
    }
    at
}

/// 从一个 `<D:response>` 块里取 `getlastmodified`，转成 epoch ms。
///
/// WebDAV 的 `getlastmodified` 是 RFC 1123 格式（`Tue, 15 Nov 1994 08:12:31 GMT`），
/// 而 `chrono::DateTime::parse_from_rfc2822` 认的正是这个。有些实现会带
/// 毫秒或时区偏移，`parse_from_rfc3339` 兜一次。
fn last_modified(block: &str) -> i64 {
    let lowered = block.to_ascii_lowercase();
    let Some(found) = lowered.find("getlastmodified>") else {
        return 0;
    };
    let start = found + "getlastmodified>".len();
    let Some(end) = block[start..].find('<') else {
        return 0;
    };
    let raw = block[start..start + end].trim();
    chrono::DateTime::parse_from_rfc2822(raw)
        .or_else(|_| chrono::DateTime::parse_from_rfc3339(raw))
        .map(|time| time.timestamp_millis())
        .unwrap_or(0)
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
    // 本次上行的包体大小（字节）。界面上的「上传 5.3 MB」说的是这个，
    // 不是「记忆正文总量」—— 包里还有 frontmatter 与 manifest。
    let mut pushed_bytes = 0usize;

    ensure_remote_dir();

    // ── 下行：拉其他设备的包 ──
    let base = load_base();
    let skip = load_skip();
    for pack in list_remote_packs() {
        let name = pack.name;
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
    let packed_files = match pack(&manifest, &contents) {
        Ok(bytes) => {
            let packed = contents.len();
            match push_package(&own_package_name(&device), &bytes) {
                Ok(()) => {
                    pushed += 1;
                    pushed_bytes = bytes.len();
                    index::global().sync_log_add(
                        &device,
                        "push",
                        &format!("上行 {} 个文件 / {}", packed, size_mb(bytes.len())),
                        true,
                    );
                    packed
                }
                Err(error) => {
                    index::global().sync_log_add(&device, "error", &error, false);
                    return Outcome::Failed(error);
                }
            }
        }
        Err(error) => {
            index::global().sync_log_add(&device, "error", &error, false);
            return Outcome::Failed(error);
        }
    };

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
        // 真正进包的文件数。`files` 是「本地清单里有多少个」，两者可能差几个
        // —— 读不到内容的文件进不了包。界面上说「上传 N 个文件」时要用这个。
        "packed": packed_files,
        "bytes": pushed_bytes,
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

/// 字节 → 「5.3 MB」。同步日志里写体积是为了让人能判断「这次传了多少」，
/// 而不是拿一串八位数字去心算。
fn size_mb(bytes: usize) -> String {
    let mb = bytes as f64 / 1024.0 / 1024.0;
    if mb >= 1.0 {
        format!("{mb:.1} MB")
    } else {
        format!("{:.0} KB", bytes as f64 / 1024.0)
    }
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
    // 协议里写什么就是什么；界面据此把「S3 兼容对象存储」那一档画成未实现。
    //
    // 这两个值先在 `json!` **外面**算好：宏里的 `{}` 只会被解析成嵌套对象，
    // 写成 `"provider": { let x = ..; }` 会得到 `missing tokens in macro
    // arguments` 的编译错（宏把 `let` 当成了键名）。
    let provider = {
        let value = config::text_of("sync.provider");
        if value.trim().is_empty() { "webdav".to_string() } else { value.trim().to_string() }
    };
    // 预设只影响「填默认值」，不改变行为；这里照实回给界面好让它把下拉
    // 停在用户上次选的那一项（而不是每次都跳回「自定义」）。
    let preset = {
        let value = config::text_of("sync.preset");
        if value.trim().is_empty() { "custom".to_string() } else { value.trim().to_string() }
    };
    json!({
        "configured": configured,
        "url": base_url(),
        "remoteDir": remote_dir(),
        "provider": provider,
        // 远端完整地址（地址 + 目录）。地址为空时给空串而不是「/目录」——
        // 后者在界面上看起来像「配置了一半」，实际上就是没配。
        "remoteUrl": if configured { remote_url() } else { String::new() },
        // 凭据的三个事实：账号有没有、密码有没有。密码是 Secret 键，这里只问
        // 「配过没有」，不去解它 —— 解出来唯一需要的地方是发请求时的
        // `auth_headers`。
        "username": config::text_of("sync.username"),
        "hasPassword": config::secret_set("sync.password"),
        // 号池包口令：留空 = 沿用 WebDAV 密码，所以「有没有」要单独说清，
        // 不然界面分不出「没配」和「沿用」。
        "hasConfirmPhrase": config::secret_set("sync.confirmPhrase"),
        "preset": preset,
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
        // 五个模块的目录与「本仓实现了没有」。页面上的「模块根目录」与
        // 「快速同步」两张卡都读这一份，免得两处各拼一遍。
        "modules": modules_json(),
        // 本机记忆根目录：界面上要显示「同步的是哪个目录」，否则用户没法确认
        // 自己到底在同步哪一份数据（换过 storage.root 的人尤其需要这个）。
        "localDir": store::root().to_string_lossy().to_string(),
        "package": own_package_name(&device),
    })
}

/// 远端设备列表。
pub fn devices() -> Value {
    if base_url().is_empty() {
        return json!({ "devices": [], "configured": false });
    }
    let own = own_package_name(&store::device_id());
    let last_at = index::global()
        .sync_get(LAST_KEY)
        .and_then(|(raw, _)| raw.trim().parse::<i64>().ok())
        .unwrap_or(0);
    let devices: Vec<Value> = list_remote_packs()
        .into_iter()
        .map(|pack| {
            let is_self = pack.name == own;
            json!({
                // 本机的「最后同步」用本机记录的同步时刻；别的设备只能靠远端包的
                // 修改时间 —— 那个包是它上传的那一刻写的，语义上就是它的最后同步。
                "lastSyncAt": if is_self { last_at } else { pack.modified_at },
                "package": pack.name,
                "device": pack.name.trim_end_matches(".tar.gz").to_string(),
                "self": is_self,
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
    json!({
        "entries": index::global().sync_log_list(limit.clamp(1, 500)),
        "total": index::global().sync_log_count(),
    })
}

/// 测连接：PROPFIND 远端目录。
///
/// 单独做一个「测试」而不是让用户点「立即同步」看结果：同步会真的写文件，
/// 而用户在这个阶段只想确认地址、账号、密码对不对。这里只读不写。
// ── 连接探测（可带临时凭据）──────────────────────────────────

/// 一组「待测」的 WebDAV 凭据。
///
/// ── 为什么测试连接要能覆盖配置 ──────────────────────────────
/// 界面上「测连接」坐在地址 / 账号 / 密码三个输入框旁边，用户改完就想先验一下
/// 再保存。若只能测「已保存的那份」，那么「改错了密码」与「旧密码失效」在界面上
/// 是同一个结果，用户只能先保存再测、测完再把错的改回来。与
/// `cloud_sync::test_connection` 同一取向：用「库里已存 + 表单新值」拼一份临时
/// 配置来测。
///
/// 字段是 `Option` 而不是空串：`Some("")` 表示「用户把它清空了」，`None` 表示
/// 「这个框没动，沿用库里的」。密码框尤其需要这个区分 —— 它回显的是占位符而
/// 不是原文，一旦被当成空串就会把已保存的密码测成「未配置」。
///
/// `rename_all = camelCase` 不能省：前端 `testSync()` 发的键是 `remoteDir`
/// （与 `SyncStatus` 的字段名一致），不带这条 rename 的话 serde 只认
/// `remote_dir` —— 那个键永远收不到，`remoteDir` 被静默忽略，于是「测连接」
/// 永远在测已保存的目录，用户改了目录名再点测试会得到上一个目录的结果。
#[derive(Debug, Clone, Default, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Probe {
    #[serde(default)]
    pub url: Option<String>,
    #[serde(default)]
    pub username: Option<String>,
    #[serde(default)]
    pub password: Option<String>,
    #[serde(default)]
    pub remote_dir: Option<String>,
}

struct Credentials {
    url: String,
    username: String,
    password: String,
    remote_dir: String,
}

impl Credentials {
    fn saved() -> Self {
        Self {
            url: base_url(),
            username: config::text_of("sync.username").trim().to_string(),
            password: config::text_of("sync.password"),
            remote_dir: remote_dir(),
        }
    }

    fn with_probe(probe: &Probe) -> Self {
        let mut next = Self::saved();
        if let Some(url) = &probe.url {
            next.url = url.trim().trim_end_matches('/').to_string();
        }
        if let Some(user) = &probe.username {
            next.username = user.trim().to_string();
        }
        // 密码只在给了**非空**值时才覆盖：空串是「没动这一栏」，不是「清空」。
        // 要真正清掉已保存的密码，走 `PUT /api/memory/config` 传空串（那里
        // `apply_one` 同样是沿用语义），或直接 reset 那个键。
        if let Some(password) = &probe.password {
            if !password.is_empty() {
                next.password = password.clone();
            }
        }
        if let Some(dir) = &probe.remote_dir {
            let trimmed = dir.trim().trim_matches('/').to_string();
            next.remote_dir = if trimmed.is_empty() { "agent2api-memory".to_string() } else { trimmed };
        }
        next
    }

    fn remote_url(&self) -> String {
        if self.url.is_empty() {
            String::new()
        } else {
            format!("{}/{}", self.url, self.remote_dir)
        }
    }

    fn apply(&self, builder: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        if self.username.is_empty() {
            return builder;
        }
        builder.basic_auth(&self.username, Some(&self.password))
    }
}

/// 用给定凭据探一次 PROPFIND（Depth: 0）。
///
/// 返回 `(status, 列表里的包数)`。`packages` 只在这台服务器愿意回 Depth:1 时
/// 才有值 —— 有些实现（坚果云的根路径、部分 S3 网关）会拒绝或多回，那种情况下
/// 保持 `None`，界面上只报「连接正常」，不编一个数字出来。
fn probe_with(creds: &Credentials) -> (bool, String, Option<usize>) {
    if creds.url.is_empty() {
        return (false, "未填写 WebDAV 地址".to_string(), None);
    }
    let url = creds.remote_url();
    let client = crate::server::core::egress::client_for(None);
    let method = reqwest::Method::from_bytes(b"PROPFIND").unwrap_or(reqwest::Method::GET);
    let body = r#"<?xml version="1.0" encoding="utf-8" ?>
<D:propfind xmlns:D="DAV:"><D:prop><D:resourcetype/></D:prop></D:propfind>"#;
    let probe = url.clone();
    let outcome = rt::block_on(async move {
        let response = creds
            .apply(client.request(method, &probe))
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
        Ok((status, _)) if status.is_success() => {
            // 目录可读，再数一次包。列表失败不算连接失败 —— 连接本身已经通了。
            let count = if creds.url == base_url() && creds.remote_dir == remote_dir() {
                list_remote_packs().len()
            } else {
                list_packs_with(creds).len()
            };
            (true, format!("连接正常（HTTP {}）", status.as_u16()), Some(count))
        }
        // 404 说明服务器在、目录还没建 —— 这不算连接失败，第一次同步会自动建
        Ok((status, _)) if status.as_u16() == 404 => (
            true,
            "服务器可达，远端目录尚未创建（首次同步会自动创建）".to_string(),
            Some(0),
        ),
        Ok((status, _)) if status.as_u16() == 401 || status.as_u16() == 403 => (
            false,
            format!("认证失败（HTTP {}）：检查用户名与密码", status.as_u16()),
            None,
        ),
        Ok((status, text)) => (
            false,
            format!(
                "服务器返回 HTTP {}{}",
                status.as_u16(),
                if text.is_empty() {
                    String::new()
                } else {
                    format!(" · {}", text.chars().take(160).collect::<String>())
                }
            ),
            None,
        ),
        Err(error) => (false, error.message(), None),
    }
}

/// 列出给定凭据下远端目录里的包（`probe_with` 的辅助，不读已保存配置）。
fn list_packs_with(creds: &Credentials) -> Vec<RemotePack> {
    let url = creds.remote_url();
    if url.is_empty() {
        return Vec::new();
    }
    let client = crate::server::core::egress::client_for(None);
    let method = reqwest::Method::from_bytes(b"PROPFIND").unwrap_or(reqwest::Method::GET);
    let body = r#"<?xml version="1.0" encoding="utf-8" ?>
<D:propfind xmlns:D="DAV:"><D:prop><D:displayname/><D:getcontentlength/><D:getlastmodified/></D:prop></D:propfind>"#;
    let outcome = rt::block_on(async move {
        let response = creds
            .apply(client.request(method, &url))
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
        return Vec::new();
    }
    parse_propfind(&text)
}

/// 测试连接（同步页「测连接」按钮）。
pub fn test_connection() -> Value {
    let creds = Credentials::saved();
    let url = creds.remote_url();
    let (ok, message, packages) = probe_with(&creds);
    match packages {
        Some(count) => json!({ "ok": ok, "message": message, "url": url, "packages": count }),
        None => json!({ "ok": ok, "message": message, "url": url }),
    }
}

/// 用表单里当前填的值测连接（配置页「测连接」按钮）。
pub fn test_connection_with(probe: Probe) -> Value {
    let creds = Credentials::with_probe(&probe);
    let url = creds.remote_url();
    let (ok, message, packages) = probe_with(&creds);
    match packages {
        Some(count) => json!({ "ok": ok, "message": message, "url": url, "packages": count }),
        None => json!({ "ok": ok, "message": message, "url": url }),
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
        // 基线有、远端没有、本地还在且相对基线没改过 → 远端删了，本地跟着删
        let mut base = BTreeMap::new();
        base.insert("a.md".to_string(), entry("h1"));
        let mut local = BTreeMap::new();
        local.insert("a.md".to_string(), entry("h1"));
        let remote = Manifest::empty("dev");
        let result = merge(&base, &local, &remote, &BTreeMap::new());
        assert_eq!(result.delete.len(), 1);
        assert!(result.conflicts.is_empty());
    }

    #[test]
    fn local_deletion_alone_is_not_propagated() {
        // 本地已经删了、远端还在：不该反过来去删远端，也不该把文件又拉下来
        // （`(None, Some(_))` 分支只处理「远端新增」，这里远端相对基线没变）
        let mut base = BTreeMap::new();
        base.insert("a.md".to_string(), entry("h1"));
        let local = BTreeMap::new();
        let mut remote = Manifest::empty("dev");
        remote.files.insert("a.md".to_string(), entry("h1"));
        let result = merge(&base, &local, &remote, &BTreeMap::new());
        assert!(result.delete.is_empty());
        assert!(result.down.is_empty());
        assert!(result.conflicts.is_empty());
    }

    /// 把一个 PROPFIND 响应切成「包名 + 修改时间」的纯函数版（`list_remote_packs`
    /// 的网络部分抽出来），只为让下面的回归测试能离线跑。
    fn packs_from(text: &str) -> Vec<RemotePack> {
        let mut out = parse_propfind(text);
        out.sort_by(|a, b| a.name.cmp(&b.name));
        out
    }

    #[test]
    fn propfind_window_uses_absolute_offset() {
        // 回归：窗口起点曾经写成 `end`（相对 href 文本的偏移），于是每个包都从
        // 文档开头截 1200 字节 —— 只要第一个响应块里的时间不是最后一个包的，
        // 结果就会串位；多字节字符撞上切片边界还会 panic。
        // 星期几必须写对：chrono 的 RFC 2822 解析会校验它与日期是否一致。
        let response = concat!(
            "<D:multistatus xmlns:D=\"DAV:\">",
            "<D:response><D:href>/dav/memory/dev-a.tar.gz</D:href>",
            "<D:propstat><D:prop><D:getlastmodified>Sun, 15 Nov 2026 08:12:31 GMT</D:getlastmodified>",
            "</D:prop></D:propstat></D:response>",
            "<D:response><D:href>/dav/memory/dev-b.tar.gz</D:href>",
            "<D:propstat><D:prop><D:getlastmodified>Mon, 16 Nov 2026 09:30:00 GMT</D:getlastmodified>",
            "</D:prop></D:propstat></D:response>",
            "</D:multistatus>"
        );
        let packs = packs_from(response);
        assert_eq!(packs.len(), 2);
        assert_eq!(packs[0].name, "dev-a.tar.gz");
        assert_eq!(packs[1].name, "dev-b.tar.gz");
        // 两个包各自的时间必须不同 —— 串位时两者会取到同一个值
        assert!(packs[0].modified_at > 0, "第一个包的修改时间没解析出来");
        assert!(packs[1].modified_at > packs[0].modified_at, "两个包的时间串位了");
    }

    #[test]
    fn propfind_window_survives_multibyte_chars() {
        // 中文目录名会让字节偏移与字符边界错开；按字符安全切片不应 panic。
        let response = concat!(
            "<D:response><D:href>/dav/记忆中枢/dev-a.tar.gz</D:href>",
            "<D:propstat><D:prop><D:getlastmodified>Mon, 16 Nov 2026 09:30:00 GMT</D:getlastmodified>",
            "</D:prop></D:propstat></D:response>"
        );
        let packs = packs_from(response);
        assert_eq!(packs.len(), 1);
        assert_eq!(packs[0].name, "dev-a.tar.gz");
        assert!(packs[0].modified_at > 0);
    }

    #[test]
    fn module_path_keeps_three_states_apart() {
        // 三态是这套目录配置的全部难点：没配过 / 显式关掉 / 自定义。
        // 混掉任意两个都会出现「用户说不要了，界面又给回默认名」这种鬼故事。
        assert_eq!(resolve_module_path("/agent-skills", "", false), "/agent-skills");
        assert_eq!(resolve_module_path("/agent-skills", "   ", false), "/agent-skills");
        assert_eq!(resolve_module_path("/agent-skills", "", true), "");
        assert_eq!(resolve_module_path("/agent-skills", "/mine/skills", true), "/mine/skills");
        // 没写前导斜杠的按「相对根」补齐：用户填 `skills` 时不该得到一个
        // 拼出来是 `https://host/davskills` 的地址。
        assert_eq!(resolve_module_path("/agent-skills", "skills", true), "/skills");
    }

    #[test]
    fn probe_accepts_the_camel_case_keys_the_page_sends() {
        // 回归：`Probe` 少了 `rename_all = "camelCase"`，前端发的 `remoteDir`
        // 被 serde 静默忽略，于是「改了远端目录再点测试连接」测的永远是旧目录 ——
        // 一个只在界面上能看出来的错，后端测试不锁住就再也发现不了。
        let probe: Probe = serde_json::from_value(json!({
            "url": "https://dav.example.com/dav/",
            "username": "me",
            "password": "",
            "remoteDir": "my-memory"
        }))
        .expect("页面发的形状必须能被解析");
        assert_eq!(probe.remote_dir.as_deref(), Some("my-memory"));
        assert_eq!(probe.url.as_deref(), Some("https://dav.example.com/dav/"));
        // 空串密码保留 `Some("")`：它不是「沿用」，是「这一栏被清空」——
        // 由 `Credentials::with_probe` 决定要不要覆盖（它只在非空时覆盖）。
        assert_eq!(probe.password.as_deref(), Some(""));
        // 缺字段是 `None`（沿用已保存值），不是 `Some("")`（清空）
        let sparse: Probe = serde_json::from_value(json!({})).expect("空对象也合法");
        assert!(sparse.url.is_none() && sparse.remote_dir.is_none());
    }

    #[test]
    fn module_list_covers_every_module_and_only_memory_is_live() {
        // 界面上「模块根目录」与「快速同步」两张卡都按这份清单渲染：漏一个
        // 模块，用户就会觉得「少了一格」；多标一个 implemented，页面就会
        // 给一个点了没反应的「立即同步」。
        let ids: Vec<&str> = MODULES.iter().map(|item| item.id).collect();
        for expected in ["skills", "dosage", "pool", "memory", "prompts"] {
            assert!(ids.contains(&expected), "模块清单缺 {expected}");
        }
        assert_eq!(ids.len(), 5);
        let live: Vec<&str> = MODULES.iter().filter(|item| item.implemented).map(|item| item.id).collect();
        assert_eq!(live, vec!["memory"], "只有记忆中枢实现了同步");
        for item in MODULES {
            assert!(item.def.starts_with('/'), "{} 的默认目录要以 / 开头", item.id);
        }
    }
}
