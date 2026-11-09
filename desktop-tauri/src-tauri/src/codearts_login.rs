//! CodeArts 网页登录时**顺手收下 SSO 票根**（`SSOTGC`），登录成功后自动写进账号。
//!
//! ── 为什么这条能自动、而「读系统浏览器 Cookie 库」不能 ──────────
//! 票根在 `auth.huaweicloud.com`（`Path=/authui/`），门户会话 cookie 在
//! `codearts.huaweicloud.com` —— **两个域**。登录窗口（`login.rs` 的
//! `run_embedded_with_proxy`）每次都用**全新临时数据目录**（`LoginProfile`），
//! 因此每次网页登录都必然完整走一遍华为云 SSO：用户在窗口里登录 → 上游把
//! `SSOTGC` 写进**这个窗口自己的** Cookie 存储 → 壳侧用
//! `WebviewWindow::cookies()` 就能读到（含 HttpOnly）。
//!
//! 与之相对，「去读系统浏览器的 Cookie 库」这条路在本仓走不通，两条硬理由：
//!   1. `LoginProfile` 是临时目录，系统浏览器的库根本不在它管辖内 —— 要去读
//!      别的进程的库，得自己定位 `%LOCALAPPDATA%\…\User Data\…\Network\Cookies`，
//!      再把 DPAPI 包裹的 os_crypt 主密钥解出来（壳 crate 连 `crypto` 模块都
//!      没有，那套代码只在插件里）；
//!   2. 更关键的是 Chromium M127+ 已切到 **App-Bound Encryption**（密文前缀
//!      `v20`），而仓库现有的 `decrypt_os_crypt_blob` 只认 `v10`（旧 AES-256-GCM）。
//!      2026-11-09 在本机实测：Chrome 的 `Cookies` 库里 `huaweicloud` 行数为 0，
//!      且所有密文都是 `v20` 前缀 —— 即便定位到库也解不开。
//!
//! ── 为什么不改成「用登录态换票根」的接口调用 ──────────────────
//! 华为云没有「给我一张 TGC」的端点：TGC 是 SSO 认证过程**当场下发**的
//! Set-Cookie，不是可换取的令牌。能换的只有 ST（一次性，几秒寿命）与
//! AK/SK/security token（那是另一条 OAuth 链的产物，与票根无关）。所以
//! 「从登录态拿票根」唯一可能的实现就是**从登录窗口的 Cookie 存储里读**。
//!
//! ── 谁调用它 ────────────────────────────────────────────────
//! `login.rs` 的等待循环在 `/wait` 首次回 `done` 那一拍、**窗口销毁之前**调
//! [`capture`]；拿到票根再调 [`save`] 打到 `PATCH /api/accounts/{id}` 的
//! `ssoTgc` 键（与手工粘贴走**同一条**写入实现，见
//! `account_store::codearts_accounts::update_codearts_sso_tgc`）。
//!
//! ── 硬约束 ──────────────────────────────────────────────────
//! 失败一律**静默降级为 `None` / `Err`**，绝不阻断登录：票根是可选增强，
//! 抓不到时账号照样落库（用户还能手工贴，见账号设置弹窗的说明）。

use std::time::Duration;

use serde_json::json;
use tauri::WebviewWindow;

/// 票根所在的域（**不是**门户域 `codearts.huaweicloud.com`）。
const TGC_HOST: &str = "auth.huaweicloud.com";
/// 带点的后缀形态：Cookie 的 Domain 属性常写成 `.auth.huaweicloud.com`。
const TGC_HOST_SUFFIX: &str = ".auth.huaweicloud.com";
/// Cookie 名。
const TGC_COOKIE: &str = "SSOTGC";
/// 票根的路径前缀（`auth.huaweicloud.com` 上就这一条是门户认的那张）。
const TGC_PATH_PREFIX: &str = "/authui";
/// 形状判据的长度下限（与网关 / 插件两侧 `looks_like_tgc` 同口径）。
const MIN_TGC_LEN: usize = 16;
/// 形状判据的长度上限（与网关 `MAX_SSO_TGC_LENGTH` 同值）。
const MAX_TGC_LEN: usize = 4096;
/// PATCH 冲突（插件刚落库、正在刷新凭证）后的重试间隔。
const SAVE_RETRY_DELAY: Duration = Duration::from_millis(300);

/// 这一段文本**像不像**一条 `SSOTGC` 票根。
///
/// 判据取上游的实际形状：`TGTX` 前缀 + `-sso` 后缀 + 长度 ≥ 16（实测
/// `TGTX--F1840439038-8498999-<64 hex>-sso`，约 100 字符）。与
/// `account_store::codearts_accounts::looks_like_tgc`、插件
/// `codearts::credentials::looks_like_tgc` **三处同口径** —— 任一处放宽都会让
/// 「存进去了但换不到会话」重新变成无法排查的故障。
pub fn looks_like_tgc(value: &str) -> bool {
    let value = value.trim();
    value.len() >= MIN_TGC_LEN
        && value.len() <= MAX_TGC_LEN
        && value.starts_with("TGTX")
        && value.ends_with("-sso")
}

/// 从一批窗口 Cookie 里挑出票根（纯函数，便于离线对账）。
///
/// `auth.huaweicloud.com` 上同名 Cookie 可能不止一条（不同 Path / 历史遗留），
/// **优先返回 `Path=/authui/` 那一条** —— 那是门户 `/authui/login?service=…`
/// 真正会认的那张；其余路径的只当备选（实测更常见的是它压根不存在）。
pub fn tgc_of(cookies: &[tauri::webview::Cookie<'static>]) -> Option<String> {
    let mut fallback: Option<String> = None;
    for cookie in cookies {
        if cookie.name() != TGC_COOKIE {
            continue;
        }
        // Domain 属性可能带前导点（`.auth.huaweicloud.com`），也可能不带
        // （host-only cookie）—— 两种都要认。
        let host = cookie.domain().unwrap_or("").trim_start_matches('.');
        if host != TGC_HOST && !host.ends_with(TGC_HOST_SUFFIX) {
            continue;
        }
        let value = cookie.value().trim();
        if !looks_like_tgc(value) {
            // 形状不对的多半是某个同名的实验性 cookie：跳过，不要把登录搞失败
            continue;
        }
        if cookie
            .path()
            .unwrap_or("")
            .starts_with(TGC_PATH_PREFIX)
        {
            return Some(value.to_string());
        }
        fallback.get_or_insert_with(|| value.to_string());
    }
    fallback
}

/// 读登录窗口的 Cookie 存储并挑出票根（`None` = 没读到 / 没有 / 形状不对）。
///
/// 读取本身复用 `crate::login::read_window_cookies` —— **不要在这里另写一份**
/// `spawn_blocking + timeout`：Windows 上直接同步调 `cookies()` 会与 WebView2
/// 的消息循环互相等待而**死锁**，那道防线只该有一处实现（漂移一次就是一次
/// 「点登录后窗口卡死，日志什么都没有」的故障）。这里只负责挑值。
pub async fn capture(window: &WebviewWindow) -> Option<String> {
    match crate::login::read_window_cookies(window).await {
        Ok(cookies) => tgc_of(&cookies),
        Err(error) => {
            eprintln!("[login] CodeArts 读取登录窗口 Cookie 失败（本次不抓票根）：{error}");
            None
        }
    }
}

/// 把票根写进刚落库的账号（`PATCH /api/accounts/{id}` 的 `ssoTgc` 键）。
///
/// ── 为什么走 PATCH 而不是「登录时就带上」 ──────────────────────
/// 票根是**可选**字段（`Credential` 从登录链路的产出里恒为空，见
/// `codearts::oauth::parse_ticket_credential` 的注释），而账号是在插件侧落库的。
/// 手工粘贴走的也正是这条 PATCH（账号设置弹窗 → `updateAccount`），让自动抓取
/// 与手工粘贴**落到同一个写入实现**，两边就不可能在归一口径上分叉。
///
/// ── 409 为什么要重试一次 ──────────────────────────────────
/// CodeArts 账号刚落库的那一瞬间，插件可能正在刷新凭证并写回 ——
/// `plugin_update_secret` 因此会以「账号凭证已变化，请刷新后重试」（409）拒绝。
/// 这不是真的冲突，隔 300ms 再试一次就过（实测）。只重试一次：第三次仍失败
/// 说明是真问题（账号被删 / 记录形态不认识），继续重试只是刷日志。
pub async fn save(account_id: &str, tgc: &str) -> Result<(), String> {
    if !safe_segment(account_id) {
        return Err(format!("账号 id 含需要转义的字符（{account_id}），已跳过自动保存"));
    }
    let path = format!("/api/accounts/{account_id}");
    let body = json!({ "ssoTgc": tgc.trim() });
    let mut last = String::new();
    for attempt in 0..2 {
        match crate::gateway::call("PATCH", &path, Some(&body)).await {
            Ok(_) => return Ok(()),
            Err(error) => {
                last = error;
                if attempt == 0 && last.contains("已变化") {
                    tokio::time::sleep(SAVE_RETRY_DELAY).await;
                    continue;
                }
                break;
            }
        }
    }
    Err(last)
}

/// 账号 id 能不能直接拼进路径段。
///
/// 票根这条链只在登录成功那一拍跑，为它引入一个百分号编码依赖不划算；
/// 而账号 id 是网关自己生成的（字母数字 + `-`/`_`/`.`），实测不会撞上需要
/// 转义的字符。真撞上了就**跳过自动保存**并让上层给一条提示 —— 那比把
/// 一个编码错误的路径打到网关（换回 404「账号不存在」）要好解释得多。
fn safe_segment(id: &str) -> bool {
    !id.is_empty()
        && id
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.'))
}