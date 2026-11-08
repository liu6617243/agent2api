//! 读本机豆包（工作）客户端的设备号。
//!
//! ── 为什么转发必须带上它（2026-11-03 实测）──────────────────────
//! 同一份 Cookie、同一台机器、同一个出口，只差这一个参数：
//!   · 只带 Cookie（改造前的做法）→ 上游回 `STREAM_ERROR` /
//!     `error_code=710022004` / `shark_admin` 的**人机验证（滑块）**，
//!     任何模型、任何账号都一样；
//!   · 补上 `device_id` 与它派生出的 `fp=verify_<device_id>` → 正常回答；
//!   · 只补版本号与渠道号（`pc_version=2.32.14` / `channel=1001`）→ 照样被拦。
//! 也就是说风控认的是「这个账号在这台受信设备上」，而设备号只在客户端的配置
//! 文件里（`Local State` 的 `aha.device.device_id`）：Cookie 里没有这个键，
//! 协议也不下发 —— 不去读它就永远是"未受信设备"，转发必然被拦。
//!
//! ── 读不到怎么办 ────────────────────────────────────────────
//! 客户端没装、文件不在、格式变了、值不合规，一律当"读不到"，行为与改造前
//! 完全一致（没有设备号可带）。读这个文件绝不能把转发弄挂，所以全程失败即 None。
//!
//! ── 2026-11-08 补豆包客户端作为回退源 ─────────────────────────
//! 本插件是「豆包工作」（aid=1044603），但用户的账号就是普通豆包账号，官方
//! 客户端是豆包（`%LOCALAPPDATA%\Doubao`，aid=582478）—— 两客户端共用同一
//! 账号体系，风控也认同一批受信设备。实测（2026-11-08）：只装豆包不装豆包
//! 工作的机器上，DoubaoWork 目录根本不存在，插件一直裸奔（无设备号）；
//! 豆包客户端 `Local State` 里的 `aha.device.device_id` 与官方客户端请求
//! 抓包里的 `device_id` 完全一致。所以豆包工作读不到时回退读豆包客户端的 ——
//! 风控视角里这是把请求挂回「用户真实在用的那台受信设备」，而不是一台
//! 从没出现过的陌生设备（这正是 T+1 签到软惩罚的嫌疑来源，见 `sign.rs`）。

use std::path::PathBuf;
use std::sync::Mutex;
use std::time::Duration;

use serde_json::Value;

/// `Local State` 是 Chromium 的配置文件，正常只有几十到几百 KB。
/// 给个上限：一个被换过的巨大文件不该被整份读进内存。
const MAX_FILE_BYTES: u64 = 8 * 1024 * 1024;
/// 设备号长度上限（正常是 15 位左右的数字串）。
const MAX_DEVICE_BYTES: usize = 64;
/// 结果缓存时长：设备号几乎不变，但重装客户端后会变，所以不缓存整个进程期。
const TTL: Duration = Duration::from_secs(60);

fn cache() -> &'static Mutex<Option<(std::time::Instant, Option<String>)>> {
    static CACHE: std::sync::OnceLock<Mutex<Option<(std::time::Instant, Option<String>)>>> =
        std::sync::OnceLock::new();
    CACHE.get_or_init(Mutex::default)
}

/// 各候选客户端的 `Local State` 路径，按优先级排列：豆包工作（本插件的本体
/// 客户端）优先，读不到回退豆包主客户端 —— 两者共用账号体系与风控设备库
/// （见模块头的 2026-11-08 注）。
fn local_state_candidates() -> Vec<PathBuf> {
    // ⚠️ 每个 cfg 块都以 `return` 收尾而不是当尾表达式：`#[cfg]` 挂在
    // 「尾表达式块」上要 `stmt_expr_attributes`（实验特性），挂在语句块上
    // 才稳定。改变量类型时保持这个形状，别顺手改成 `vec![…]` 结尾。
    #[cfg(target_os = "windows")]
    {
        let Some(base) = std::env::var_os("LOCALAPPDATA") else {
            return Vec::new();
        };
        let base = PathBuf::from(base);
        return vec![
            base.join("DoubaoWork")
                .join("User Data")
                .join("Local State"),
            base.join("Doubao").join("User Data").join("Local State"),
        ];
    }
    #[cfg(target_os = "macos")]
    {
        let Some(base) = std::env::var_os("HOME") else {
            return Vec::new();
        };
        let base = PathBuf::from(base)
            .join("Library")
            .join("Application Support");
        return vec![
            base.join("DoubaoWork").join("Local State"),
            base.join("Doubao").join("Local State"),
        ];
    }
    #[cfg(not(any(target_os = "windows", target_os = "macos")))]
    {
        let base = match std::env::var_os("XDG_CONFIG_HOME") {
            Some(dir) => PathBuf::from(dir),
            None => match std::env::var_os("HOME") {
                Some(home) => PathBuf::from(home).join(".config"),
                None => return Vec::new(),
            },
        };
        return vec![
            base.join("DoubaoWork").join("Local State"),
            base.join("Doubao").join("Local State"),
        ];
    }
}

/// 值必须能安全地进 query：只收 ASCII 字母数字与 `-`/`_`，其余当读不到。
fn usable(value: &str) -> Option<String> {
    let value = value.trim();
    (!value.is_empty()
        && value.len() <= MAX_DEVICE_BYTES
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_')))
    .then(|| value.to_string())
}

fn read() -> Option<String> {
    local_state_candidates().into_iter().find_map(read_at)
}

/// 读单个 `Local State`：文件不存在 / 超限 / 解析失败都返回 `None`，交给
/// 下一个候选。绝不因为某个文件异常把整个读取弄挂。
fn read_at(path: PathBuf) -> Option<String> {
    let meta = std::fs::metadata(&path).ok()?;
    if !meta.is_file() || meta.len() > MAX_FILE_BYTES {
        return None;
    }
    let bytes = std::fs::read(&path).ok()?;
    let value: Value = serde_json::from_slice(&bytes).ok()?;
    let device = &value["aha"]["device"]["device_id"];
    let text = device
        .as_str()
        .map(str::to_string)
        .or_else(|| device.as_u64().map(|id| id.to_string()))?;
    usable(&text)
}

/// 本机客户端的设备号（读不到就是 `None`，缓存 [`TTL`]）。
pub fn device_id() -> Option<String> {
    if let Ok(cache) = cache().lock() {
        if let Some((at, value)) = cache.as_ref() {
            if at.elapsed() < TTL {
                return value.clone();
            }
        }
    }
    let found = read();
    if let Ok(mut cache) = cache().lock() {
        *cache = Some((std::time::Instant::now(), found.clone()));
    }
    if found.is_some() {
        crate::logging::verbose(
            "[DoubaoWork]",
            "已读到本机客户端的设备号（豆包工作或豆包客户端），转发时会带上（上游风控认这个设备）",
        );
    }
    found
}
