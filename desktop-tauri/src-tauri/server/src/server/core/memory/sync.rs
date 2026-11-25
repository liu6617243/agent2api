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
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

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
    /// 打包设备的**显示名**（`sync.deviceName`，没填过就是系统电脑名）。
    ///
    /// 加它是为了让「设备列表」能显示人看得懂的名字：包名是设备 id（那串
    /// 十六进制是**身份**，改名会换掉远端文件名、把「自己」认成别人），
    /// 而列表里该出现的是「我的笔记本」。
    ///
    /// `serde(default)` 不能省：旧版本打出的包里没有这个键，没有默认值会让
    /// 整个清单解析失败 —— 那不是「少个名字」，是「这个包读不出来」。
    #[serde(default)]
    pub name: String,
    /// 打包时刻（epoch ms）
    pub created_at: i64,
    /// 相对路径 → 文件信息
    pub files: BTreeMap<String, FileEntry>,
}

impl Manifest {
    fn empty(device: &str) -> Self {
        Self {
            device: device.to_string(),
            name: String::new(),
            created_at: crate::server::logging::now_ms(),
            files: BTreeMap::new(),
        }
    }
}

/// 本机显示名：`sync.deviceName`，没填过退到系统电脑名，再取不到就是空串。
///
/// 「空串」在这里是合法且常见的：用户可能既没填显示名、环境里也读不到
/// `COMPUTERNAME`。调用方据此决定「显示什么」—— 包名那边仍用设备 id，
/// 这里只负责「给人看的那一份」。
fn display_name() -> String {
    let configured = config::text_of("sync.deviceName").trim().to_string();
    if configured.is_empty() {
        store::hostname().trim().to_string()
    } else {
        configured
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

/// 远端设备 id → 它自己报的显示名（`Manifest::name`）。
///
/// 为什么要有这份缓存：设备列表是 PROPFIND 出来的，只能看到包名（设备 id），
/// 看不到包里那份 manifest。而「把那串十六进制显示成我的笔记本」唯一的来源
/// 就是包里的 `name` —— 它只在下行那一刻被读到过。存下来，列表才有名字可用。
///
/// 不为了拿名字去逐个下载远端包：列表页会因此等上几秒甚至几十秒（每个包都要
/// 完整下载一次），而名字只是显示层的锦上添花。缓存不到就退回设备 id，如实显示
/// 「这一台的名字我还不知道」。
const NAMES_KEY: &str = "sync.names";

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
    // 显示名随包走：别的设备解包时没有本机的 config.json，只能从清单里读。
    manifest.name = display_name();
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

/// 拼 URL 用的地址：**无尾斜杠**（`format!("{}/{}", base_url(), dir)` 靠这个）。
fn base_url() -> String {
    configured_url().trim_end_matches('/').to_string()
}

/// 默认 WebDAV 地址（坚果云）。
///
/// ── 为什么要有默认值而不是「留空 = 未配置」──────────────
/// 第一次打开同步页时，用户面对的是空地址 + 一句「未配置」，还得自己去查
/// 「坚果云的 WebDAV 地址是什么」。而绝大多数人就接坚果云 —— 填好默认值，
/// 界面一打开就是「填了地址、缺账号密码」，用户只需要补两格。
///
/// ── 为什么它必须和「用户主动清空」区分开 ─────────────────
/// `sync.url` 的存储值仍然是空串（默认值不写进配置），所以：
///   · 用户没填过 → 这里返回默认地址，界面显示它；
///   · 用户填了别的 → 用他填的；
///   · 用户想过「我不同步」→ 走「清除配置」或者关 `sync.enabled`，
///     而不是清空地址框（清空了下一帧又会回到默认值）。
/// 这个取舍是为了「打开就能用」，代价是「清空地址」不再等于「关闭同步」——
/// 后者本来就该由开关表达，而不是由一个输入框是否为空表达。
///
/// 尾斜杠留在这里：它就是界面输入框里显示的那串（截图里也是带斜杠的），
/// `base_url()` 拼路径前自己会剥掉。
const DEFAULT_URL: &str = "https://dav.jianguoyun.com/dav/";

/// 生效地址：用户填的，或者默认值（**保留用户写的那个形态**）。
///
/// 与 `base_url()` 的分工：这里不做任何修剪，就是「界面上该显示的那串」；
/// `base_url()` 在它上面只做一件事 —— 剥尾斜杠给拼 URL 用。
///
/// 为什么回显要保留尾斜杠：`base_url()` 剥完变成 `https://dav.jianguoyun.com/dav`，
/// 与用户（以及坚果云文档里）看到的写法差一个字符，看起来像「我填的那串被改过」。
fn configured_url() -> String {
    resolve_url(&config::text_of("sync.url"))
}

/// `configured_url` 的纯函数内核：`sync.url` 的存值 → 生效地址。
///
/// 拆出来是为了**可测**：`configured_url` 要读磁盘上的 config.json，而这里的
/// 关键约定恰好是「空串 → 默认值、非空 → 原样」这一条 —— 测不到它，就等于
/// 「默认地址」这个功能没有回归测试。
fn resolve_url(stored: &str) -> String {
    let trimmed = stored.trim();
    if trimmed.is_empty() {
        DEFAULT_URL.to_string()
    } else {
        trimmed.to_string()
    }
}

/// 凭据齐没齐 —— 也就是界面上那句「配好了没」。
///
/// 地址有默认值之后，「地址非空」不再能回答这个问题 —— 从没动过配置的机器也有
/// 地址。所以判据改成**账号非空**（密码允许为空：少数自建 WebDAV 只认用户名，
/// 或者走匿名写）。同步页的「未配置」徽标、设备列表的空态、快速同步的就绪判断、
/// 自动化任务的跳过条件都读这一个判据，避免「一次都没配过」被显示成「已配置」。
///
/// 不用 `has_password`：坚果云这类必须密码，但自建服务里「只填用户名」是合法
/// 配置，把它判成未配置会让用户永远停在「未配置」。密码缺失在真发请求时由
/// 401 如实报出来，不需要在这里猜。
fn has_credentials() -> bool {
    !config::text_of("sync.username").trim().is_empty()
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

// ── 这里原本有一张「各模块同步目录」表（`MODULES` / `modules_json`）────
//
// 它把技能仓库 / 用量统计 / 号池管家 / 提示词库的远端根目录也列进同步页，
// 而本仓从来没实现过那四个模块的同步 —— 界面上就是四个灰掉的按钮加一句
// 「未实现」。2026-11-25 整组删除（连同 `sync.modules.*` 四个配置键）。
//
// 现在的事实只有两条，不需要一张表来表达：
//   · 同步哪些数据 —— `sync_roots()`：general / projects / profile；
//   · 传到远端哪个目录 —— `remote_dir()`：`sync.remoteDir`，默认 agent2api-memory。

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
    conflicts: Vec<Conflict>,
}

/// 一条冲突：两侧各是什么版本。
///
/// `local` / `remote` 是 `Option` 而不是空串：`None` 的含义是**这一侧没有这个
/// 文件**（被删了）。原来用四个 `String` 表达不了「一侧已删」—— 空串既可能是
/// 「删了」也可能只是「哈希没算出来」，于是「本地删、远端改了」这种冲突根本
/// 进不了队列（见 `merge` 的判据）。数据库里这两列本来就是可空的，存 `NULL`
/// 不需要改表。
struct Conflict {
    path: String,
    local: Option<String>,
    remote: Option<String>,
    base: Option<String>,
    /// 远端那一侧的体积。裁决「保留本地」之后基线要记**远端版本**，那时得知道
    /// 它的体积；哈希在冲突行里本来就有，体积没有，所以一并带上。
    remote_size: Option<u64>,
}

/// 三方合并：`base`（上次同步后的基线）/ `local` / `remote`。
///
/// 判定规则（每条路径独立），与 AgentHub `sync.cjs` 的 `decide()` 同口径：
/// - 两边结果一致（含「两边都删了」「两边改成同一版」）→ 无事发生；
/// - 只有远端变了：远端删了 → 本地跟着删；远端改了 → 采纳远端；
/// - 只有本地变了（含本地删了）→ 保留本地，上行会把结果推上去；
/// - 两边都变了且结果不同 → 冲突。**一边是删除也算「变了」**。
///
/// ── 为什么「本地删、远端没改」不能把文件拉回来 ──────────────
/// 基线记的是「上次同步后两边一致的状态」。本地删掉某个文件后，基线里还有它，
/// 而别的设备的包里同样还有 —— 那一侧相对基线**没变**。只看「本地没有、远端有
/// → 远端新增，直接写下来」会把本地删除当成「远端新增」处理，于是用户删掉的
/// 记忆被别的设备的旧包复活（删三条、同步一次全回来）。判据必须两边都跟基线
/// 比，而不是拿本地跟远端比。
///
/// ── 为什么「本地改了、远端删了」要进队列 ────────────────────
/// 静默保留本地等于**撤销对方的删除**，而对方删之前可能刚把结论挪到了别处。
/// 记忆是长期事实，猜错一次两台机器就各说一套，所以交给人。
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
        let base_hash = base.get(path).map(|item| item.hash.as_str());
        let local_entry = local.get(path);
        let remote_entry = remote.files.get(path);
        let local_hash = local_entry.map(|item| item.hash.as_str());
        let remote_hash = remote_entry.map(|item| item.hash.as_str());

        // 两边结果一致（含「两边都没有这个文件」「两边改成同一版」）→ 无事发生。
        if local_hash == remote_hash {
            continue;
        }
        // 每一侧各自跟**基线**比，而不是拿本地跟远端比。这一条是删除语义的地基：
        // 「本地删了、远端没动」时 `local_changed` 为真而 `remote_changed` 为假，
        // 于是走「保留本地」——删除不会被对方的旧包复活。
        let local_changed = local_hash != base_hash;
        let remote_changed = remote_hash != base_hash;

        match (local_hash, remote_hash, local_changed, remote_changed) {
            // 只有远端变了，且远端那个文件不见了 → 远端删了 → 本地跟着删。
            (_, None, false, true) => delete.push(path.clone()),
            // 只有远端变了，远端有内容 → 采纳远端（远端新增也落这一支：
            // 那时本地与基线都是「没有」，本地相对基线没变）。
            (_, Some(_), false, true) => {
                if let Some(text) = remote_files.get(path) {
                    down.push((path.clone(), text.clone()));
                }
            }
            // 只有本地变了 → 保留本地。上行的那一包会把结果推上去，
            // 别的设备下一轮读到的就是它。
            (_, _, true, false) => {}
            // 两边都变了且结果不同 → 冲突。任一侧可能是 `None`（那一侧删了），
            // 交给裁决的人判断「这边还在、那边删了」到底谁对。
            (l, r, true, true) => conflicts.push(Conflict {
                path: path.clone(),
                local: l.map(str::to_string),
                remote: r.map(str::to_string),
                base: base_hash.map(str::to_string),
                remote_size: remote_entry.map(|item| item.size),
            }),
            // 走到这里说明两边的哈希不同、却都相对基线没变 —— 基线里两侧
            // 一定是同一个值，不可能同时等于两个不同的哈希。留着兜底而不是
            // `unreachable!()`：同步是后台任务，为一个理论上到不了的分支让
            // 整轮 panic 不值得。
            _ => {}
        }
    }
    Merge {
        down,
        delete,
        conflicts,
    }
}

/// 两边都改过、但内容是**同一个 l1 容器**（两台设备同一天各写了几条）→ 能不能拼。
///
/// 返回合并后的全文；拼不了（不是容器 / 节冲突 / 头不一致）返回 `None`，交给
/// 冲突队列。
///
/// ── 为什么要在 `merge` 之外做 ───────────────────────────────
/// `merge` 只拿得到哈希（清单是 {hash,size}），而容器合并必须看正文。把正文
/// 塞进 `merge` 意味着每次同步都要把全库读一遍；放在冲突循环里做，只有
/// **真正冲突的那几条**才读一次本地文件 —— 一次同步通常零到两条。
///
/// ── 为什么这条路径优先级高于冲突 ───────────────────────────
/// l1 一天一文件，两台机器同一天都干活时这个文件必然两边都改过。不合并的话
/// 用户每个工作日都要裁决一次「今天的日志听谁的」，答案永远是「都要」。
/// 能拼就拼掉，冲突队列才只剩真需要人判断的。
fn auto_merge_container(path: &str, remote_text: &str) -> Option<String> {
    let local_text = read_file(path)?;
    store::merge_container(&local_text, remote_text)
}

// ── 一次同步 ─────────────────────────────────────────────────

/// 跑一次同步。`manual` 为真时忽略自动开关。
pub fn run(manual: bool) -> super::enrich::Outcome {
    use super::enrich::Outcome;

    if !config::bool_of(config::ENABLE_KEY) {
        return Outcome::Skipped("记忆中枢已关闭".into());
    }
    if !manual && !config::bool_of("sync.enabled") {
        return Outcome::Skipped("同步总开关已关闭（sync.enabled）".into());
    }
    if !manual && !config::bool_of("sync.auto") {
        return Outcome::Skipped("自动同步已关闭（sync.auto）".into());
    }
    // 自动跑（定时）没账号就直接跳过。地址现在有默认值（坚果云），
    // `base_url()` 不再是「配没配」的判据 —— 不在这里拦，一台从没填过账号的
    // 机器会定期往坚果云发一次注定 401 的 PROPFIND，日志里堆满「失败」，
    // 而用户从没打算用同步。
    //
    // 手动跑不拦：用户点「立即同步」就是想看到真实结果（包括那句「认证失败」）。
    // 原来那句「未配置 WebDAV 地址（sync.url）」的跳过分支随之删掉 —— 地址恒有值，
    // 它已经是不可达代码；「没配好」由 `has_credentials()` 表达。
    if !manual && !has_credentials() {
        return Outcome::Skipped("还没填 WebDAV 账号（sync.username）".into());
    }

    let device = store::device_id();
    let mut pushed = 0usize;
    let mut pulled = 0usize;
    let mut deleted = 0usize;
    let mut conflicted = 0usize;
    let mut skipped = 0usize;
    // 自动合并掉的容器条数。与 `pulled` 分开报：合并是「两边的这一天拼起来」，
    // 不是「从别的设备拿了一条新的」—— 混在一起会让人以为多出一条记忆。
    let mut merged = 0usize;
    // 本轮自动合并过的路径。基线更新时要把它们**退回旧值**（见下面的注释）。
    let mut merged_paths: BTreeSet<String> = BTreeSet::new();

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
        // ── 空包守卫：远端包声称有 N 个文件、实际一个字节都没带
        //
        // 正常的空包（对方确实清空了记忆）不应该触发这里 —— 那种情况下
        // `manifest.files` 也是空的。这里挡的是**清单与内容不一致**：清单说
        // 有一百个文件、包里一个都没解出来。这时 `merge` 会得出「远端把这一百个
        // 全删了」，于是本机把这一百份记忆全部留档后删掉 —— 对端一旦是半截上传
        // 或打包中途崩，就变成一次跨设备的批量误删。
        if !manifest.files.is_empty() && files.is_empty() {
            let message = format!(
                "同步包 {name} 的清单列了 {} 个文件，但包里一个都读不到（可能上传中断）；本轮跳过该设备，未改动本地",
                manifest.files.len()
            );
            index::global().sync_log_add(&name, "error", &message, false);
            crate::server::logging::log("[Memory]", &format!("⚠️ {message}"));
            continue;
        }
        // ── 每台设备都重新扫一次本地清单：上一台设备的包可能刚改过本地文件，
        // 用循环外那份旧清单做三方比对会把「刚写下来的内容」当成本地改动。
        let local_now = local_manifest();
        let result = merge(&base, &local_now.files, &manifest, &files);
        // ── 批量删除守卫：一次下行要删掉一半以上（且至少 10 个）→ 停下来问人
        //
        // 与 AgentHub「冲突时暂停上传」同一种取向：宁可这一轮不同步，也不要
        // 在一轮里把本机大半个记忆库删掉。同步是无人值守的后台任务 —— 删掉
        // 之后用户可能几天才发现，而删除是本模块唯一不可逆的操作（虽然有
        // `sync-trash/` 留档，但那只是最后一道保险，不是常规路径）。
        //
        // 阈值「一半且至少 10 个」：小记忆库里删 3 个是正常操作，不该被拦；
        // 大记忆库里一轮删掉一半更像对面出了问题（包被别的设备覆盖、对方清了
        // 记忆目录、打包中途崩）。
        if let Some(message) = bulk_delete_guard(local_now.files.len(), result.delete.len()) {
            index::global().sync_log_add(&name, "error", &message, false);
            crate::server::logging::log("[Memory]", &format!("⚠️ {message}"));
            // 这一轮不上传：本地已经被别的设备改过一部分，此时把「半合并」的
            // 状态推上去会把自己的包覆盖成那个中间态。直接整轮失败，等人处理。
            index::global().sync_log_add(
                &device,
                "error",
                &format!("本轮未上传：远端包 {name} 触发了批量删除守卫"),
                false,
            );
            return Outcome::Failed(message);
        }
        // 本台设备这一轮的跳过条数。不能用全局 `skipped` 算 —— 那是累加值，
        // 第二台设备的日志会把第一台的跳过数也扣掉。
        let mut skipped_here = 0usize;
        // 本台设备这一轮拼起来的条数（与 `skipped_here` 同理：`merged` 是累加值）
        let mut merged_here = 0usize;
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
            // 跳过名单也要管删除。用户点「忽略」时表达的是「这条别动」——
            // 若只拦下行写入、不拦删除，对方哪天把这个文件从包里去掉，本机
            // 就会在用户明确说过「别动」之后把它删掉。
            if skip.contains(path) {
                skipped += 1;
                skipped_here += 1;
                continue;
            }
            if remove_file(path) {
                deleted += 1;
            }
        }
        for conflict in &result.conflicts {
            if skip.contains(&conflict.path) {
                skipped += 1;
                skipped_here += 1;
                continue;
            }
            // ── 先试容器自动合并：两边都是「同一天的 l1」，内容能拼
            //
            // 判据在 `store::merge_container` 里：按节 id 对齐，两边共有的节
            // 正文一致、各自独有的节并进来。拼成了就直接落盘、不占冲突队列 ——
            // 这条路径覆盖的是「两台机器同一天都干了活」，它每个工作日都会发生，
            // 而答案永远是「两边都要」。
            //
            // 落盘用的是 `write_file`（原子写 + 写锁），与下行同一条路径。
            // 成功后这一条算「已合并」而不是「冲突」：`conflicted` 不增长，
            // 但它也不进 `pulled` —— 界面上的「下行 N 条」说的是「从别的设备
            // 拿过来的条数」，合并是两边各出一半，混进去会让人以为多了一条新记忆。
            if let Some(remote_text) = files.get(&conflict.path) {
                if let Some(merged_text) = auto_merge_container(&conflict.path, remote_text) {
                    // 内容已经拼平（上一轮拼过、对方包还没更新）→ 不重复落盘，
                    // 只算一次「已合并」。否则每次同步都会重写一遍同一个文件，
                    // 白白刷 mtime 与备份。
                    if read_file(&conflict.path).as_deref() == Some(merged_text.as_str())
                        || write_file(&conflict.path, &merged_text)
                    {
                        merged += 1;
                        merged_here += 1;
                        // **不推进基线**：合并结果只在本机与上传包里，对方的包
                        // 还是它自己那半份。若把基线记成合并后的哈希，下一轮
                        // merge 会得到「本地 == 基线、远端变了」→ 把对方那半份
                        // 覆盖回来，本地独有的节静默消失（与「保留本地」的基线
                        // 口径是同一个坑，见 `resolve_conflict`）。
                        // 基线留在旧值上，下一轮两边都算「改过」→ 再拼一次；
                        // 等对方拉到合并后的包，两边哈希一致，自然收敛。
                        merged_paths.insert(conflict.path.clone());
                        // 上一轮进过队列的，现在拼平了 → 把那条待裁决关掉。
                        // 不关的话它会一直挂在「冲突裁决」里等人点，而点下去
                        // 是把刚拼好的结果覆盖成半份。
                        let closed = index::global().conflict_resolve_merged(&conflict.path);
                        if closed > 0 {
                            index::global().sync_log_add(
                                &manifest.device,
                                "conflict",
                                &format!("{} → merged（自动拼接，撤销待裁决）", conflict.path),
                                true,
                            );
                        }
                        continue;
                    }
                }
            }
            conflicted += 1;
            index::global().conflict_enqueue(
                &conflict.path,
                &manifest.device,
                conflict.local.as_deref(),
                conflict.remote.as_deref(),
                conflict.remote_size.unwrap_or(0),
                conflict.base.as_deref(),
                if conflict.local.is_none() {
                    "本地已删，远端改了同一份记忆"
                } else if conflict.remote.is_none() {
                    "本地改了，远端删了同一份记忆"
                } else {
                    "本地与远端都改过同一份记忆"
                },
            );
        }
        // 第一次见到这台设备（或它改过显示名）时记下名字，设备列表要用。
        remember_name(&manifest.device, &manifest.name);
        index::global().sync_log_add(
            &manifest.device,
            "pull",
            &format!(
                "下行 {} 条 / 拼接 {} 条 / 删除 {} 条 / 冲突 {} 条 / 跳过 {} 条",
                result.down.len().saturating_sub(skipped_here),
                merged_here,
                result.delete.len(),
                // 拼掉的不算冲突：`result.conflicts` 是「merge 判出的待处理条数」，
                // 里面有一部分这一轮已经自动拼平了，照原数报会让人以为还有那么多条要裁决。
                result.conflicts.len().saturating_sub(merged_here),
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
    // 上行的两个结果一起带出来：`packed_files` 是真正进包的文件数，
    // `pushed_bytes` 是包体大小（界面上的「上传 5.3 MB」说的就是这个，不是
    // 「记忆正文总量」—— 包里还有 frontmatter 与 manifest）。
    //
    // 用元组一次性接住，而不是先在循环外 `let mut pushed_bytes = 0` 再赋值：
    // 那个初值必然被这里覆盖，编译器会报 `unused_assignments`（且真的有一次
    // 读不到初值的赋值）。上行失败的两个分支都是 `return`，不会走到解构。
    let (packed_files, pushed_bytes) = match pack(&manifest, &contents) {
        Ok(bytes) => {
            let packed = contents.len();
            let size = bytes.len();
            match push_package(&own_package_name(&device), &bytes) {
                Ok(()) => {
                    pushed += 1;
                    index::global().sync_log_add(
                        &device,
                        "push",
                        &format!("上行 {} 个文件 / {}", packed, size_mb(size)),
                        true,
                    );
                    (packed, size)
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
    // 自动合并过的路径**不能**记进基线。
    //
    // 基线记的是「两边一致的状态」，而合并结果此刻只在本机：对方的包里还是它
    // 自己那半份。把合并后的哈希写进基线，下一轮就是
    //     localChanged  = 本地 == 基线 → false
    //     remoteChanged = 远端 != 基线 → true
    // 于是走「仅远端改」把本机独有的那几节覆盖掉 —— 与「保留本地」裁决的
    // 基线口径是同一个坑，只是这次受害者是刚拼好的那半天记忆。
    //
    // 把这几条从基线里摘掉（=「两边都算改过」），下一轮会再拼一次：拼出来
    // 的内容与现有文件一致时直接跳过写盘，等对方拉到合并后的包、两边哈希相同
    // 自然收敛。中间那几轮不会产生冲突条目，也不会重写文件。
    for path in &merged_paths {
        next_base.remove(path);
    }
    save_base(&next_base);

    if pulled > 0 || deleted > 0 || merged > 0 {
        let _ = super::index::global().rebuild(&super::store::load_all());
    }

    let summary = json!({
        "device": device,
        "pulled": pulled,
        "deleted": deleted,
        "pushed": pushed,
        "conflicts": conflicted,
        // 自动拼起来的 l1 容器条数。与 `pulled` 分开：合并是两边各出一半，
        // 界面上「下行 N 条」说的是「从别的设备拿来的」，混进去会多算。
        "merged": merged,
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

// ── 自动同步：按 sync.intervalMin 定期跑 ─────────────────────

/// 自动同步的默认 / 最大间隔（分钟）：24 小时。
const DEFAULT_INTERVAL_MIN: i64 = 1440;

/// 巡检节奏：每隔这么久看一次「到点没有」。
///
/// 60 秒而不是「睡到点再醒」：`sync.intervalMin` 是热配置，用户把它从 1440 改成
/// 5 之后，一条睡在 24 小时长睡里的线程不会知道 —— 改完毫无反应。一分钟空转的
/// 成本可以忽略（只读一次配置与一个 kv 时间戳，不碰网络）。
const AUTO_TICK: Duration = Duration::from_secs(60);

/// 巡检线程是否已经起来（桌面壳重启后端时会再次调用 [`spawn_auto`]）。
static AUTO_RUNNING: AtomicBool = AtomicBool::new(false);

/// 最近一次**成功**同步的时刻（epoch ms）；从没同步过是 0。
fn last_sync_at() -> i64 {
    index::global()
        .sync_get(LAST_KEY)
        .and_then(|(raw, _)| raw.trim().parse::<i64>().ok())
        .unwrap_or(0)
}

/// 生效的自动同步间隔（分钟）。
///
/// schema 里有 `min` / `max`，但 `config.json` 是手可改的文本 —— 用户手写一个
/// `0` 或 `100000` 时，`apply_one` 那边的校验不会回头再跑一遍。这里夹一次：
/// 下限 5 分钟（再密就只是把同一份整包反复上传），上限 24 小时（默认值即上限）。
fn interval_minutes() -> i64 {
    clamp_interval(config::number("sync.intervalMin"))
}

/// 间隔的边界归一（纯函数，见下面 `interval_is_clamped_to_sane_bounds`）。
fn clamp_interval(value: f64) -> i64 {
    if value.is_finite() {
        (value as i64).clamp(5, DEFAULT_INTERVAL_MIN)
    } else {
        DEFAULT_INTERVAL_MIN
    }
}

/// 现在该不该自动跑：记忆中枢开着 + 同步总开关 + 自动同步 + 填过账号。
///
/// 与 `run(false)` 里的判据同一口径，但先在这里问一遍：否则每 60 秒都会进一次
/// `run(false)` 拿回一条 `Skipped`，而那条跳过会把 `last_attempt` 推进 ——
/// 「配置不全导致的跳过」不该被算成一次尝试。
fn auto_ready() -> bool {
    config::bool_of(config::ENABLE_KEY)
        && config::bool_of("sync.enabled")
        && config::bool_of("sync.auto")
        && has_credentials()
}

/// 启动自动同步线程。重复调用无副作用。
///
/// ── 为什么它是一条独立线程，而不是 `schedule.rs` 里的一个任务 ──
/// `schedule.rs` 的任务表是「记忆中枢的 AI / 维护任务」：每个任务有 intervalMin
/// 或 daily 节奏、有 `auto.tasks` 里的开关、跑完写 `task_run` 台账。同步不属于
/// 那一族 —— 它的开关是 `sync.enabled` + `sync.auto`，节奏是 `sync.intervalMin`，
/// 做的是网络传输而不是模型调用。塞进任务表会让「同步」出现在自动化页的任务
/// 列表里，而那一页的「立即执行」按钮对它毫无意义（同步页自己有「立即同步」）。
///
/// ── 为什么失败也等满一个间隔再试 ────────────────────────────
/// 断网时每分钟重试一次只会把同步日志刷满，并且真的在打远端（每次 401 或超时）。
/// 失败后等满 `intervalMin` 再试，用户想立刻重试点「立即同步」即可。
///
/// ── 为什么第一轮的起算点是「上次成功同步」而不是开机时刻 ──────
/// 重启后端不该让一台十分钟前刚同步过的机器再传一次整包。从没同步过（0）时，
/// 开机一分钟后就会跑第一轮 —— 配好账号的机器不必等到第二天。
pub fn spawn_auto() {
    if AUTO_RUNNING.swap(true, Ordering::SeqCst) {
        return;
    }
    let spawned = std::thread::Builder::new()
        .name("memory-sync".to_string())
        .spawn(|| {
            crate::server::logging::log(
                "[Memory]",
                &format!("同步巡检线程已启动（间隔读 sync.intervalMin，默认 {DEFAULT_INTERVAL_MIN} 分钟）"),
            );
            let mut last_attempt = last_sync_at();
            loop {
                std::thread::sleep(AUTO_TICK);
                if !auto_ready() {
                    continue;
                }
                let wait = interval_minutes().saturating_mul(60_000);
                // 锚点取「上次尝试」与「上次成功」里更晚的那个：手动同步也会写
                // `LAST_KEY`，不比对的话，刚点过「立即同步」的机器一分钟后会被
                // 后台再传一遍整包。
                let anchor = last_attempt.max(last_sync_at());
                let now = crate::server::logging::now_ms();
                if anchor > 0 && now.saturating_sub(anchor) < wait {
                    continue;
                }
                // 先记「尝试过」再跑：这一轮无论成功失败，下一轮都在一个完整间隔
                // 之后 —— 失败重试由用户点「立即同步」触发，不由后台刷屏。
                last_attempt = now;
                match run(false) {
                    super::enrich::Outcome::Done(summary) => crate::server::logging::log(
                        "[Memory]",
                        &format!("自动同步完成：{summary}"),
                    ),
                    super::enrich::Outcome::Skipped(reason) => {
                        crate::server::logging::log("[Memory]", &format!("自动同步跳过：{reason}"))
                    }
                    super::enrich::Outcome::Failed(error) => {
                        crate::server::logging::log("[Memory]", &format!("⚠️ 自动同步失败：{error}"))
                    }
                }
            }
        });
    if spawned.is_err() {
        AUTO_RUNNING.store(false, Ordering::SeqCst);
        crate::server::logging::log("[Memory]", "⚠️ 同步巡检线程启动失败");
    }
}

fn conflicted_paths() -> Vec<String> {
    // 用**不截断**的查询。原来这里走 `conflict_list(200)`，冲突超过 200 条时
    // 第 201 条起的路径不会从基线里摘掉 → 下一轮三方比对把它读成「本地没变、
    // 远端改了」→ 静默采纳远端，还没裁决的冲突被覆盖，而界面上那条还挂着。
    index::global().conflict_pending_paths()
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
    // 与容器/单条的读改写互斥：同步落盘是「整份覆盖某个 .md」，若此刻另一个
    // 线程正按「读容器 → 追加一节 → 整份写回」写同一份文件，两边各写一半版本，
    // 最后活下来的是谁完全看时序 —— 拿锁把它变成确定的一件事。
    //
    // 顺带把 `fs::write` 换成 `store::write_atomic`：后者是「临时文件 → fsync →
    // rename」，读者要么看到旧版、要么看到新版。裸 `fs::write` 是就地截断再写，
    // 同步循环跑到一半时页面刷新恰好读到，就会看到半份文件（解析失败 → 那条
    // 记忆从索引里消失，直到下次重建）。它自带的滚动备份也替代了这里的
    // 手写 `.bak`，口径与其余写入路径一致。
    let _guard = super::store::lock_writes();
    store::write_atomic(&store::abs(relative), text).is_ok()
}

fn remove_file(relative: &str) -> bool {
    let _guard = super::store::lock_writes();
    let path = store::abs(relative);
    if !path.exists() {
        return false;
    }
    // ── 先留档，再删
    //
    // 远端「包里没有这个文件」不一定等于「对方真的删了它」：对面包可能被别的
    // 设备覆盖、对方可能手动清空过记忆目录、传输可能截断。而删除是本模块唯一
    // 不可逆的操作。所以这里把原字节原样搬进 `sync-trash/<时刻>-<设备>/<原路径>`
    // 再删活文件 —— 报告里「删除 N 条」之后如果发现删错了，人还能从磁盘上捞。
    //
    // 留档失败就不删：宁可这一轮少删一次（下一轮还会再判定一次），也不要
    // 在没有副本的情况下抹掉用户的记忆。
    if archive_before_remove(relative, &path).is_err() {
        crate::server::logging::log(
            "[Memory]",
            &format!("⚠️ 同步删除留档失败，本次保留 {relative}（下次同步会再判定）"),
        );
        return false;
    }
    std::fs::remove_file(&path).is_ok()
}

/// 把即将同步删除的文件原样搬进 `sync-trash/`。
///
/// 保持相对路径（`projects/<slug>/l1/<agent>/2026-11-19.md` 在留档里也是这个
/// 形状），这样「从留档里捞回来」就是一次复制，不用再查任何映射。
/// 留档的相对路径：`<时刻>-<设备>/<原相对路径>`。
///
/// 纯函数，能离线测 —— 这里最容易错的是「路径段校验」：同步包里的路径来自
/// 别的设备，`..` 段能让留档写到记忆根目录外面去。
fn archive_rel_path(relative: &str) -> Result<String, String> {
    archive_rel_path_at(relative, crate::server::logging::now_ms(), &store::device_id())
}

/// [`archive_rel_path`] 的可测内核：时刻与设备 id 由调用方给。
fn archive_rel_path_at(relative: &str, stamp: i64, device: &str) -> Result<String, String> {
    let mut parts: Vec<&str> = Vec::new();
    for segment in relative.split('/') {
        // 只取正常路径段：`..` 与空段一律不认。不认就整条拒绝（这一轮既不留档
        // 也不删），而不是拼出一个跳出根目录的路径。
        if segment.is_empty() || segment == "." || segment == ".." {
            return Err(format!("路径段非法：{relative}"));
        }
        parts.push(segment);
    }
    if parts.is_empty() {
        return Err(format!("路径为空：{relative}"));
    }
    let device = device.trim();
    Ok(format!(
        "{stamp}-{}/{}",
        if device.is_empty() { "unknown" } else { device },
        parts.join("/")
    ))
}

fn archive_before_remove(relative: &str, path: &Path) -> Result<(), String> {
    let target = store::sync_trash_root().join(archive_rel_path(relative)?);
    if let Some(parent) = target.parent() {
        std::fs::create_dir_all(parent).map_err(|error| format!("建留档目录失败：{error}"))?;
    }
    std::fs::copy(path, &target).map_err(|error| format!("写留档失败：{error}"))?;
    Ok(())
}

// ── 基线存取 ─────────────────────────────────────────────────

/// 读设备名缓存（`device id → 显示名`）。
fn load_names() -> BTreeMap<String, String> {
    index::global()
        .sync_get(NAMES_KEY)
        .and_then(|(raw, _)| serde_json::from_str::<BTreeMap<String, String>>(&raw).ok())
        .unwrap_or_default()
}

/// 记下某台设备的显示名。空名字不落盘 —— 那等于把「不知道」记成「叫空串」。
///
/// 只在名字变了才写库：每次同步都会走到这里，无条件写会让 `sync_state` 的
/// `updated_at` 每轮都跳一次，而它本来代表的是「名字这份数据什么时候变过」。
fn remember_name(device: &str, name: &str) {
    let name = name.trim();
    if device.is_empty() || name.is_empty() {
        return;
    }
    let mut names = load_names();
    if names.get(device).map(String::as_str) == Some(name) {
        return;
    }
    names.insert(device.to_string(), name.to_string());
    if let Ok(raw) = serde_json::to_string(&names) {
        index::global().sync_set(NAMES_KEY, &raw);
    }
}

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

/// 同步状态的**轻量摘要**：只回「配没配 / 开没开 / 上次什么时候」。
///
/// ── 为什么不能拿 `status()` 顶替 ────────────────────────────
/// `status()` 里有 `local_manifest()`，那是一次**全目录递归扫描**（记忆几千条时
/// 几百毫秒）。`status()` 的调用者是同步页 —— 用户主动打开、可以等；而画像页顶
/// 那枚「持久缓存与 WebDAV 同步保护中」的徽标会在每次 `profile_view()` 时重算，
/// 让一次「看一眼画像」的请求去扫全盘是明显的浪费。这里只读配置与一个 kv 时间戳。
pub fn summary() -> Value {
    let last_at = index::global()
        .sync_get(LAST_KEY)
        .and_then(|(raw, _)| raw.trim().parse::<i64>().ok())
        .unwrap_or(0);
    // 先在 `json!` **外面**算好：宏里的 `{}` 只会被解析成嵌套对象，写成
    // `"provider": { let x = ..; }` 会得到 `missing tokens in macro arguments`
    // 的编译错（宏把 `let` 当成了键名）。`status()` 里同一个值也是这么算的。
    let provider = {
        let value = config::text_of("sync.provider");
        if value.trim().is_empty() { "webdav".to_string() } else { value.trim().to_string() }
    };
    json!({
        // 这里不再看 `base_url()`：地址有默认值，恒非空。判据是「有没有账号」，
        // 与 `status()` / 快速同步 / 自动同步的跳过条件同一口径。
        "configured": has_credentials(),
        "enabled": config::bool_of("sync.enabled"),
        "provider": provider,
        "lastSyncAt": if last_at > 0 { json!(last_at) } else { Value::Null },
    })
}

/// 同步页顶部状态。
pub fn status() -> Value {
    let configured = has_credentials();
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
        // 回显用 `configured_url()`（保留用户写的形态，含尾斜杠）：这是输入框里
        // 该出现的那串。`base_url()` 剥过尾斜杠，只适合拼路径。
        "url": configured_url(),
        // 这串地址是不是「我们替他填的默认值」。
        //
        // 没有这个字段，界面分不出「用户填的坚果云地址」与「用户从没动过、
        // 我们预填的坚果云地址」—— 两种情况下输入框里都是同一个字符串。
        // 有了它，界面可以在地址旁边标一句「默认值」，用户就知道这一格还没确认过。
        "urlDefault": config::text_of("sync.url").trim().is_empty(),
        "remoteDir": remote_dir(),
        "provider": provider,
        // 远端完整地址（地址 + 目录）。地址恒有值（默认值兜底），所以这里不再
        // 按 `configured` 置空 —— 拼出来的地址是「真要去连的那个」，与地址框
        // 里显示的一致；「没配好」由 `configured` 单独表达。
        "remoteUrl": remote_url(),
        // 凭据的三个事实：账号有没有、密码有没有。密码是 Secret 键，这里只问
        // 「配过没有」，不去解它 —— 解出来唯一需要的地方是发请求时的
        // `auth_headers`。
        "username": config::text_of("sync.username"),
        "hasPassword": config::secret_set("sync.password"),
        "preset": preset,
        "device": device,
        "deviceName": config::text_of("sync.deviceName"),
        // 系统电脑名（Windows 上是 `COMPUTERNAME`）。界面在 `sync.deviceName`
        // 还没填过时拿它当默认值 —— 「这台机器叫什么」系统本来就知道，让用户
        // 自己手打一遍（打错了多设备列表里就多出一台不存在的机器）没有意义。
        //
        // 取不到时是空串，不是 `"unknown"`：那个词会被当成一个真实的机器名
        // 写进配置、再传到别的设备的列表里。
        "hostName": store::hostname(),
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
        // 本机记忆根目录：界面上要显示「同步的是哪个目录」，否则用户没法确认
        // 自己到底在同步哪一份数据（换过 storage.root 的人尤其需要这个）。
        "localDir": store::root().to_string_lossy().to_string(),
        "package": own_package_name(&device),
    })
}

/// 远端设备列表。
///
/// 没账号就**不发请求**：地址有默认值之后这里恒有 URL，照旧往下走会变成
/// 「每次打开同步页都向坚果云发一次匿名 PROPFIND」—— 结果是 401 + 空列表，
/// 而界面已经知道「未配置」，这次出网纯属浪费（且无人账号下请求日志会很难看）。
pub fn devices() -> Value {
    if !has_credentials() {
        return json!({ "devices": [], "configured": false });
    }
    let own = own_package_name(&store::device_id());
    let names = load_names();
    // 本机的名字就地算：其他设备的名字来自同步包里报的那份（`remember_name`
    // 缓存），本机不用等一次下行 —— 它现在就能读自己的配置。
    let own_name = display_name();
    let last_at = index::global()
        .sync_get(LAST_KEY)
        .and_then(|(raw, _)| raw.trim().parse::<i64>().ok())
        .unwrap_or(0);
    let devices: Vec<Value> = list_remote_packs()
        .into_iter()
        .map(|pack| {
            let is_self = pack.name == own;
            let device = pack.name.trim_end_matches(".tar.gz").to_string();
            // 名字的优先级：本机现算 → 下行时缓存下来的那份 → 空串（界面退回设备 id）。
            let name = if is_self {
                own_name.clone()
            } else {
                names.get(&device).cloned().unwrap_or_default()
            };
            json!({
                // 本机的「最后同步」用本机记录的同步时刻；别的设备只能靠远端包的
                // 修改时间 —— 那个包是它上传的那一刻写的，语义上就是它的最后同步。
                "lastSyncAt": if is_self { last_at } else { pack.modified_at },
                "package": pack.name,
                "device": device,
                // 人看得懂的那个名字。空串表示「还没从它的包里读到过」——
                // 界面据此显示设备 id，而不是编一个名字出来。
                "name": name,
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
    let remote_hash = row.get("remoteHash").and_then(Value::as_str).map(str::to_string);
    // 远端那一侧的体积。老记录里没有这一列（补列时是 NULL），按 0 走 ——
    // 体积只影响同步预览里显示的大小，不参与「哪边改过」的判定。
    let remote_size = row.get("remoteSize").and_then(Value::as_u64).unwrap_or(0);
    match resolution {
        // 忽略：把这条路径拉进跳过名单。只把冲突记录标成 resolved 不够 ——
        // 下一轮同步两边内容还是不同，没有跳过名单就会重新入队。
        "ignore" => {
            set_skip(&path, true);
        }
        // 保留本地：基线记**远端那一侧**的版本，而不是本地当前版本。
        //
        // ── 为什么必须是远端 ────────────────────────────────────
        // 裁决表达的是「我要本地这版」。此刻远端还没收到它，远端包还是旧版。
        // 若把基线推成本地版本，下一轮 merge 会得到：
        //     localChanged  = 本地 == 基线 → false
        //     remoteChanged = 远端 != 基线 → true
        // 于是走「仅远端改」把本地的裁决结果**用远端旧版覆盖回去**，裁决被静默
        // 撤销（用户点完「保留本地」，再同步一次改动就没了）。
        // 记远端版本则相反：本地相对基线算「改过」，远端算「没改」→ 保留本地，
        // 随后上行把本地这版推上去，语义正确。这与 AgentHub `sync.cjs` 的
        // `resolve()` 同一口径。
        //
        // 远端那一侧是 `None`（远端删了这个文件）时，基线里这一条要删掉：
        // 「远端没有它」本身就是远端当前状态。删掉之后本地算「新增」，上行会带上。
        "local" => {
            let mut base = load_base();
            match remote_hash {
                Some(hash) => {
                    base.insert(
                        path.clone(),
                        FileEntry {
                            hash,
                            size: remote_size,
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
            // ── 先判「远端已删」，再判「远端有内容」
            //
            // 顺序不能反：远端删了这个文件时 `remote_hash` 是 `None`，此时
            // `files.get(&path)` 也返回 `None` —— 两件事在类型上长得一样。
            // 若先判「`files` 里没有 → 删本地」，那么「包里该有这条、却读不出
            // 文本」（解包截断、内容读取失败）也会走到「删本地」，一次网络抖动
            // 就把用户的记忆删掉。所以「远端有内容」以冲突行里记的哈希为准，
            // 哈希在而文本不在 = 数据不完整，宁可让用户重同步。
            if remote_hash.is_some() {
                let Some(text) = files.get(&path) else {
                    return Err(format!(
                        "远端包里缺 {path} 的内容（清单里有、包里读不到），为防误删未改动本地；请重新同步后再裁决"
                    ));
                };
                write_file(&path, text);
            } else {
                remove_file(&path);
            }
            // 裁决完之后这条路径两边已经一致，补进基线，下一轮不再当冲突。
            // 「保留远端」这一支的基线就是远端当前版本 —— 与「保留本地」相反，
            // 因为此刻本地已经被写成远端那份了（或已按远端删除）。
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
            // 空串按「回到默认地址」处理，而不是「测一个空地址」。地址有默认值
            // 之后，界面上把地址框清空的语义就是「用默认的」，与保存时同口径
            // （`sync.url` 存空串 → `configured_url()` 给默认值）。照抄成空串会让
            // 「清空地址框 + 测连接」得到一句「未填写 WebDAV 地址」，而框里实际
            // 显示着一串默认地址 —— 界面与结论互相打架。
            let trimmed = url.trim();
            next.url = if trimmed.is_empty() {
                DEFAULT_URL.trim_end_matches('/').to_string()
            } else {
                trimmed.trim_end_matches('/').to_string()
            };
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
    // 正常路径下到不了这里：`saved()` 走 `base_url()`（默认值兜底），
    // `with_probe()` 把空串也折回默认值，两条来源都恒非空。留着这道闸是防
    // 「以后有人从别处拼一份 Credentials 进来」—— 那时该报的是「地址是空的」，
    // 而不是让 `format!("{}/{}")` 拼出一条 `/agent2api-memory` 这种相对路径去
    // 发给 reqwest（它会当成无 host 的非法 URL，错误信息里看不出是配置问题）。
    if creds.url.is_empty() {
        return (false, "WebDAV 地址是空的".to_string(), None);
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

/// 这一轮要删掉的文件数是不是「可疑的大批删除」。是则返回一句人能读的说明。
///
/// 阈值：`deleting >= 10 && deleting * 2 >= local_files`。
/// - 至少 10 个：小记忆库（十几个文件）里删掉一半是正常整理，拦下来只会烦人；
/// - 一半以上：大记忆库里一轮删掉一半，更像对面包出了问题（被别的设备覆盖、
///   对方清空了记忆目录、打包中途崩），而不是用户真的一次删了一半。
///
/// 纯函数，能离线测 —— 这条判据如果写错，要么永远拦（同步变成摆设），
/// 要么永远不拦（误删没人挡）。
fn bulk_delete_guard(local_files: usize, deleting: usize) -> Option<String> {
    if deleting < 10 || deleting * 2 < local_files {
        return None;
    }
    Some(format!(
        "远端包要求删除本机 {deleting} 个文件（本机共 {local_files} 个），一次删掉这么多更像对面出了问题；本轮已中止，本地未改动。\
         先确认对端设备的记忆目录是不是被清过 / 包是不是被覆盖；确实要删的话，把「同步总开关」暂时关掉，在同步页「打开目录」里备份远端包，再逐条手工删本机文件，最后重新打开同步"
    ))
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
    fn archive_path_keeps_the_relative_layout() {
        // 留档保持原相对路径：从留档里捞回来就是一次复制，不用查映射
        let path = archive_rel_path_at("projects/o--r/l1/manual/2026-11-19.md", 1_700_000_000_000, "dev1")
            .expect("正常路径应通过");
        assert_eq!(path, "1700000000000-dev1/projects/o--r/l1/manual/2026-11-19.md");
    }

    #[test]
    fn archive_path_rejects_traversal() {
        // 同步包里的路径来自别的设备：`..` 段能让留档写到记忆根目录外面去
        assert!(archive_rel_path_at("../escape.md", 1, "dev").is_err());
        assert!(archive_rel_path_at("a/../../b.md", 1, "dev").is_err());
        assert!(archive_rel_path_at("", 1, "dev").is_err());
        assert!(archive_rel_path_at("a//b.md", 1, "dev").is_err());
    }

    #[test]
    fn bulk_delete_guard_only_stops_big_deletions() {
        // 小记忆库删几条不拦：那是正常整理
        assert!(bulk_delete_guard(12, 5).is_none());
        // 大记忆库里删掉一半 → 拦
        assert!(bulk_delete_guard(200, 120).is_some());
        // 一半但不到 10 个 → 不拦
        assert!(bulk_delete_guard(12, 7).is_none());
        // 恰好一半且够 10 个 → 拦（`deleting * 2 >= local_files` 含等号）
        assert!(bulk_delete_guard(20, 10).is_some());
        // 什么都没删 → 不拦
        assert!(bulk_delete_guard(200, 0).is_none());
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
        // （远端相对基线没变，走「保留本地」那一支）
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

    #[test]
    fn remote_new_file_is_pulled() {
        // 基线没有、本地没有、远端有 → 远端新增，直接写下来。
        // （新判据里这一条落在「仅远端变了」：本地与基线都是「没有」。）
        let base = BTreeMap::new();
        let local = BTreeMap::new();
        let mut remote = Manifest::empty("dev");
        remote.files.insert("a.md".to_string(), entry("h9"));
        let mut files = BTreeMap::new();
        files.insert("a.md".to_string(), "brand new".to_string());
        let result = merge(&base, &local, &remote, &files);
        assert_eq!(result.down.len(), 1);
        assert!(result.conflicts.is_empty());
        assert!(result.delete.is_empty());
    }

    #[test]
    fn local_deletion_against_remote_change_is_a_conflict() {
        // 本地删了、远端改了 → 冲突。不能静默把文件拉回来（用户的删除被撤销），
        // 也不能静默保留删除（对方刚写的内容凭空消失）。
        let mut base = BTreeMap::new();
        base.insert("a.md".to_string(), entry("h1"));
        let local = BTreeMap::new();
        let mut remote = Manifest::empty("dev");
        remote.files.insert("a.md".to_string(), entry("h2"));
        let mut files = BTreeMap::new();
        files.insert("a.md".to_string(), "remote edit".to_string());
        let result = merge(&base, &local, &remote, &files);
        assert!(result.down.is_empty(), "不能把用户删掉的文件拉回来");
        assert_eq!(result.conflicts.len(), 1);
        let conflict = &result.conflicts[0];
        assert!(conflict.local.is_none(), "本地那一侧应记为「没有」");
        assert_eq!(conflict.remote.as_deref(), Some("h2"));
    }

    #[test]
    fn local_change_against_remote_deletion_is_a_conflict() {
        // 本地改了、远端删了 → 冲突。静默保留本地等于撤销对方的删除。
        let mut base = BTreeMap::new();
        base.insert("a.md".to_string(), entry("h1"));
        let mut local = BTreeMap::new();
        local.insert("a.md".to_string(), entry("h2"));
        let remote = Manifest::empty("dev");
        let result = merge(&base, &local, &remote, &BTreeMap::new());
        assert!(result.delete.is_empty(), "不能删掉用户刚改过的本地文件");
        assert_eq!(result.conflicts.len(), 1);
        let conflict = &result.conflicts[0];
        assert_eq!(conflict.local.as_deref(), Some("h2"));
        assert!(conflict.remote.is_none(), "远端那一侧应记为「没有」");
    }

    #[test]
    fn keep_local_baseline_follows_remote_so_the_decision_survives() {
        // 回归：裁决「保留本地」后基线必须记**远端**版本。
        //
        // 场景：本地 h2 / 远端 h1 / 基线 h1，用户裁决保留本地。
        // 基线记远端（h1）时，下一轮 merge 得到 localChanged=true、remoteChanged=false
        // → 保留本地，裁决成立。
        // 若基线记成本地（h2），下一轮得到 localChanged=false、remoteChanged=true
        // → 采纳远端，本地 h2 被 h1 覆盖 —— 这正是修掉的那个 bug。
        let mut base = BTreeMap::new();
        base.insert("a.md".to_string(), entry("h1"));
        let mut local = BTreeMap::new();
        local.insert("a.md".to_string(), entry("h2"));
        let mut remote = Manifest::empty("dev");
        remote.files.insert("a.md".to_string(), entry("h1"));

        let result = merge(&base, &local, &remote, &BTreeMap::new());
        assert!(result.down.is_empty(), "基线已是远端版本，本地改动不该被覆盖");
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

    /// 默认地址这条链路的三条约定：空串兜底、用户值原样、拼 URL 时剥尾斜杠。
    ///
    /// 三个断言各自锁住一个曾经/可能出错的点：
    ///   · `""` 不兜底 → 第一次打开同步页是个空框，用户得自己去查坚果云地址；
    ///   · 非空值被改写（比如被 `trim_end_matches` 处理过）→ 回显的地址与用户
    ///     填的那串差一个字符，看起来像「我的配置被系统改了」；
    ///   · `base_url()` 没剥尾斜杠 → `format!("{}/{}", ..)` 拼出 `.../dav//dir`，
    ///     部分反代会 404，而错误信息里那串双斜杠看起来像客户端 bug。
    #[test]
    fn url_falls_back_to_default_and_strips_trailing_slash_for_requests() {
        assert_eq!(resolve_url(""), DEFAULT_URL);
        assert_eq!(resolve_url("   "), DEFAULT_URL);
        assert_eq!(resolve_url(" https://dav.example.com/dav/ "), "https://dav.example.com/dav/");
        assert!(!DEFAULT_URL.trim_end_matches('/').ends_with('/'));
        // 回显形态带斜杠、拼 URL 用的形态不带 —— 两者必须都能从同一个值推出来。
        assert_eq!(resolve_url("").trim_end_matches('/'), "https://dav.jianguoyun.com/dav");
    }

    /// 旧版本打的包里没有 `name`，解析不能因此整包失败。
    ///
    /// `Manifest::name` 是后加的字段。缺了 `serde(default)`，`serde_json::from_str`
    /// 会报 `missing field 'name'`，`unpack` 里那句 `.ok()` 把它吞成 `None`，
    /// 于是整个包被当成「没有 manifest.json」—— 用户看到的是「同步包缺少
    /// manifest.json」，而包其实完好。这条断言就是拦这个的。
    #[test]
    fn manifest_without_name_field_still_parses() {
        let legacy = json!({
            "device": "abc123",
            "created_at": 1700000000000i64,
            "files": {},
        });
        let manifest: Manifest = serde_json::from_value(legacy).expect("旧包必须能解析");
        assert_eq!(manifest.device, "abc123");
        assert!(manifest.name.is_empty(), "缺字段时是空串，不是猜一个名字");
    }

    #[test]
    fn interval_is_clamped_to_sane_bounds() {
        // `config.json` 是手可改的文本：写个 0 会变成「每分钟传一次整包」，
        // 写个十万会变成「半年才同步一次」。两者都不会报错，只会让人觉得
        // 「同步坏了」—— 所以在读出口夹一道。
        assert_eq!(clamp_interval(1440.0), 1440, "默认值就是上限：24 小时");
        assert_eq!(clamp_interval(0.0), 5);
        assert_eq!(clamp_interval(-30.0), 5);
        assert_eq!(clamp_interval(100_000.0), 1440);
        assert_eq!(clamp_interval(f64::NAN), 1440, "读不出数字时退回默认，不是 0");
        assert_eq!(clamp_interval(90.0), 90, "整数分钟原样保留");
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
}
