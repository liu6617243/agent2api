//! msToken：官方客户端每个请求的 URL 参数都带（Cookie 里另有一颗）。
//!
//! ── 关于 a_bogus（2026-11-12 结论：不可伪造，只能绕）───────────
//! 抓包证实官方 `/alice/ug/*` 每条请求都带 `a_bogus`（`landing/checkin` 上是
//! 184 字符 / 解码 136 字节的纯二进制）。曾以为「照抖音版结构自己签一个」就能
//! 补上，实现完（见 [`super::abogus`]）拿真机签名反推验证后**否掉了**：豆包
//! 这一档与 `douyin_abogus_python` 系列不同构，三处标志位一个都对不上。
//!
//! 2026-11-12 又在真实客户端页面内直接探测其签名 SDK，把「能不能由 web 侧
//! 复现」也一并否掉：
//!
//! | 调用 | 返回 |
//! |------|------|
//! | `byted_acrawler.frontierSign({url,method,body})` | `{"X-Bogus":"RDgZdDWD1vSJNQ0Y"}` |
//! | 同上 + `signType:'a_bogus'` | 仍 `{"X-Bogus":…}` |
//! | `window.bdms.frontierSign` | 仍 `{"X-Bogus":…}` |
//!
//! 也就是说 `webmssdk.es5.js` / `sdk-glue.js` 这两份 web 侧 SDK **只能产出
//! 16 字符的 `X-Bogus`**；`landing/checkin` 上那颗 184 字符 `a_bogus` 是客户端
//! **native mssdk** 出的（客户端日志里的 `other.mssdk_native`）。
//!
//! 所以 `a_bogus` 的正确处理是**不发这颗请求**，而不是伪造它 —— 见
//! [`super::rewards::checkin_with_retry`]：签到改为只读 `v3/detail`，未签就
//! 引导用户去客户端点。伪造/裸奔的写请求每发一次就把该 uid 当天封死一次
//! （连官方客户端一起废），这是 11-05 以来所有「豆包签到废了」的来源。
//!
//! ── 为什么 msToken「没有就不带」，而不是自己造一颗 ────────────
//! msToken 是服务端加密下发的载荷，不是普通随机串。自造一颗 168 位
//! base64 塞进 query，服务端要么解不开直接判异常、要么（更糟）把它记成
//! 「这个会话的令牌是坏的」—— 那正是本改造要消除的东西。而「不带」是
//! 改造前一直在用的形态，已被实测证明能过。所以这里的策略是**只回放
//! 服务端真下发过的值**：还没有就返回 `None`，[`super::api::endpoint`]
//! 就不带这个参数，等第一个响应把令牌发下来，从下一个请求起才带。
//!
//! ── 缓存的作用域 ────────────────────────────────────────────
//! 按会话（`sessionid`）隔离：多账号共用一颗等于自报「这些账号是一台机器
//! 上的同一个人」。会话键取不到时（快照只存了 Cookie 数组、或响应没下发
//! 会话）落全局份，读取顺序「会话份 → 全局份」—— 官方本来就是一台设备
//! 一颗令牌，全局份对应这个形态。

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

/// 缓存时长。官方轮换得很勤，过旧的值反而显眼，所以不无限期复用。
const TOKEN_TTL: Duration = Duration::from_secs(30 * 60);

/// msToken 的进程内缓存：按会话（sessionid）隔离。
fn tokens() -> &'static Mutex<HashMap<String, (String, Instant)>> {
    static TOKENS: OnceLock<Mutex<HashMap<String, (String, Instant)>>> = OnceLock::new();
    TOKENS.get_or_init(Mutex::default)
}
/// 上游响应头 `x-ms-token` 下发的新值落盘（`api::json_response` 调用）。
///
/// `session` 传空表示「响应带了新令牌但取不到会话键」（官方这批响应的
/// `set-cookie` 不含 sessionid）：记为全局份。
pub fn store_ms_token(session: &str, token: &str) {
    let token = token.trim();
    if token.is_empty() {
        return;
    }
    let key = session.trim().to_string();
    if let Ok(mut tokens) = tokens().lock() {
        if tokens.len() >= 64 {
            // 上限保护：Map 无时序，整体清空最简单 —— 清空后各会话重新等
            // 服务端下发，行为与首次请求一致，不算异常。
            tokens.clear();
        }
        tokens.insert(key, (token.to_string(), Instant::now()));
    }
}

/// 当前会话可用的 msToken：服务端下发过且未过期才返回。
///
/// 读取顺序：会话份 → 全局份。**没有就返回 `None`，不自造**（见模块头
/// 「为什么没有就不带」）。
pub fn ms_token(session: &str) -> Option<String> {
    let session = session.trim();
    let tokens = tokens().lock().ok()?;
    for key in [session, ""] {
        if let Some((token, at)) = tokens.get(key) {
            if at.elapsed() < TOKEN_TTL {
                return Some(token.clone());
            }
        }
    }
    None
}

// ── web_tab_id ───────────────────────────────────────────────

/// `web_tab_id`：官方每开一个 webview 生成一颗 UUID，并在**该 webview 的所有
/// 请求**上复用。抓包里同一个 `conn` 内的 `landing/report` / `landing/get` /
/// `landing/checkin` 用的是同一颗。
///
/// 这里按会话（`sessionid`）缓存：同一账号同一时刻只应有一颗 tab id，反复换
/// 等于自报「这个账号同时开了十几个窗口」。缺失时按 UUIDv4 形态现场生成 ——
/// 它不是服务端下发的载荷（不像 msToken），自己生成是安全的。
pub fn web_tab_id(credentials: &serde_json::Value) -> Option<String> {
    let session = super::accounts::cookie_header(credentials)
        .ok()
        .map(|header| super::accounts::session_of_header(&header))
        .unwrap_or_default();
    let key = session.trim().to_string();
    type Tabs = Mutex<HashMap<String, (String, Instant)>>;
    static TABS: OnceLock<Tabs> = OnceLock::new();
    /// tab id 跟随 webview 生命周期，给足一天；过期即换一颗新的。
    const TAB_TTL: Duration = Duration::from_secs(24 * 60 * 60);
    let tabs = TABS.get_or_init(Mutex::default);
    let mut guard = tabs.lock().ok()?;
    if let Some((id, at)) = guard.get(&key) {
        if at.elapsed() < TAB_TTL {
            return Some(id.clone());
        }
    }
    let id = uuid_v4();
    if guard.len() >= 256 {
        guard.clear();
    }
    guard.insert(key, (id.clone(), Instant::now()));
    Some(id)
}

/// UUIDv4 形态的随机串（`8-4-4-4-12`，第三段首字符固定 `4`，第四段首字符
/// 取 `8|9|a|b`）—— 与 Chromium 的 `crypto.randomUUID()` 输出同形态。
fn uuid_v4() -> String {
    let mut bytes = [0u8; 16];
    if getrandom::getrandom(&mut bytes).is_err() {
        // 熵不可用时退回时间派生，绝不能因此让签到失败。
        let now = crate::http::now_ms() as u128;
        bytes.copy_from_slice(&now.to_le_bytes()[..16]);
    }
    bytes[6] = (bytes[6] & 0x0F) | 0x40;
    bytes[8] = (bytes[8] & 0x3F) | 0x80;
    let hex: String = bytes.iter().map(|byte| format!("{byte:02x}")).collect();
    format!(
        "{}-{}-{}-{}-{}",
        &hex[0..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..32]
    )
}

// ── aid ─────────────────────────────────────────────────────

/// 签到类请求该用的 `aid`。
///
/// 2026-11-12 用 CDP 直连真机客户端（`--remote-debugging-port=9342`）抓任务
/// 中心的 `landing/checkin`，`aid` / `real_aid` 是 **497858**。
///
/// 早先记的 `582478` 是错的：那次样本来自**网页聊天页**（`doubao.com` 文档
/// 里前端自己发的 fetch，用于 onboarding 引导签到），与客户端任务中心
/// 不是同一条入口。`landing/report` / `landing/get` / `v3/detail` 在任务中心
/// 语境下同样都是 `497858`。
///
/// `1044603` 是**豆包工作**的 app id，只有当账号快照**明确**声明自己是豆包
/// 工作端（`clientKind == "work"`）且用户真在工作端里签时才用。
pub fn checkin_aid(credentials: &serde_json::Value) -> u64 {
    match credentials["clientKind"]
        .as_str()
        .map(str::trim)
        .unwrap_or_default()
    {
        "work" => 1_044_603,
        _ => 497_858,
    }
}

// ── doubao_pc_version ────────────────────────────────────────

/// 谁是「豆包客户端版本 5.x」那一个数。
///
/// 上一版把 `pc_version` 和 `doubao_pc_version` 写成同一个值（都是
/// `clientVersion`），真实客户端不是：抓包里
/// `pc_version=2.32.6`（壳版本）、`doubao_pc_version=5.74.71`（业务包版本），
/// 而且 `runtime_version` 也跟着 `doubao_pc_version`（= 5.74.71）。
///
/// 三个数各是一套：
/// · `pc_version`       = 客户端壳版本 → `credentials.clientVersion`，缺省 2.32.6；
/// · `doubao_pc_version` = 业务包版本 → `credentials.doubaoPcVersion`，缺省 5.74.71；
/// · `runtime_version`   = 与业务包版本同值（真实抓包如此）。
///
/// 分开取值是因为它们在客户端是两份独立配置，硬绑成一个数会在壳/包版本
/// 不同步的机器上露出马脚。
pub fn doubao_pc_version(credentials: &serde_json::Value) -> &str {
    credentials["doubaoPcVersion"]
        .as_str()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or(DEFAULT_DOUBAO_PC_VERSION)
}

/// 真实客户端（2.32.6 壳 / 5.74.71 业务包）的业务包版本，抓包里每个请求都带。
pub const DEFAULT_DOUBAO_PC_VERSION: &str = "5.74.71";

// ── web_id ──────────────────────────────────────────────────

/// `web_id`：客户端每个请求都带，但**账号 Cookie 里没有这个键**。
///
/// 来源是 `POST /alice/user/launch/core` 的响应 `data.web_id`
/// （2026-11-12 抓包：`{"...","ttwid":"...","web_id":"7699132147475203619"}`）。
/// 客户端拿到后回填到之后所有请求的 query 上。
///
/// 所以这里跟 `msToken` 同一个套路：**只回放服务端真下发过的值**，拿不到就
/// 整条不带（`api::endpoint` 那边是 `if let Some`）。自造一颗会被当成
/// 「这个 uid 换了一台设备」—— 比不带更糟。
///
/// 缓存按会话（`sessionid`）隔离，与 `msToken` 同口径：多账号共用一颗等于
/// 自报「这些账号是同一台机器」。
pub fn store_web_id(session: &str, web_id: &str) {
    let web_id = web_id.trim();
    if web_id.is_empty() {
        return;
    }
    let key = session.trim().to_string();
    if let Ok(mut ids) = web_ids().lock() {
        if ids.len() >= 64 {
            ids.clear();
        }
        ids.insert(key, (web_id.to_string(), Instant::now()));
    }
}

fn web_ids() -> &'static Mutex<HashMap<String, (String, Instant)>> {
    static IDS: OnceLock<Mutex<HashMap<String, (String, Instant)>>> = OnceLock::new();
    IDS.get_or_init(Mutex::default)
}

/// 当前会话可用的 `web_id`（服务端下发过且未过期）。
///
/// 读取顺序：会话份 → 全局份。**没有就返回 `None`，不自造**。
pub fn web_id(credentials: &serde_json::Value) -> Option<String> {
    // 快照里显式存过就用它（用户重导入时可能已经把 webId 存进来了）。
    if let Some(value) = credentials["webId"]
        .as_str()
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        return Some(value.to_string());
    }
    let session = super::accounts::cookie_header(credentials)
        .ok()
        .map(|header| super::accounts::session_of_header(&header))
        .unwrap_or_default();
    let session = session.trim();
    let ids = web_ids().lock().ok()?;
    for key in [session, ""] {
        if let Some((value, at)) = ids.get(key) {
            if at.elapsed() < TOKEN_TTL {
                return Some(value.clone());
            }
        }
    }
    None
}

// ── a_bogus ─────────────────────────────────────────────────

/// 给一条**已拼好的 query 串**（不含 `a_bogus`）出签名。
///
/// 目前固定返回 `None` —— [`super::abogus::A_BOGUS_ENABLED`] 默认关闭，
/// 因为豆包这一档的签名结构与公开的抖音版**不同构**（反推证据见
/// [`super::abogus`] 模块头）。留着这个入口是为了将来换掉实现时上层不用动。
///
/// 读接口（`/chat/completion` 等）本来也不签 —— 抓包里它们在 `/samantha/*`
/// 下不带 `a_bogus`，且插件这条链路一直是通的，不该在这里冒险动它。
pub fn a_bogus(query: &str, user_agent: &str, method: &str) -> Option<String> {
    super::abogus::sign(query, user_agent, method)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 测试串行锁 + 清空缓存。
    ///
    /// 缓存是**进程级**的，`cargo test` 默认并行跑 —— 不串行的话，
    /// `global_fallback` 往全局份写一颗之后，任何会话都会退到全局份，
    /// 断言「没有就是 `None`」的用例就会随机失败（本文件第一版正是这样）。
    fn fresh() -> std::sync::MutexGuard<'static, ()> {
        static LOCK: Mutex<()> = Mutex::new(());
        let guard = match LOCK.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        if let Ok(mut tokens) = tokens().lock() {
            tokens.clear();
        }
        guard
    }

    #[test]
    fn no_token_without_server_value() {
        let _guard = fresh();
        // 关键行为：服务端没下发过就**不带**，绝不自己造一颗。
        assert_eq!(ms_token("never-seen-session"), None);
        assert_eq!(ms_token(""), None);
    }

    #[test]
    fn store_then_reuse() {
        let _guard = fresh();
        store_ms_token("sess-A", "tok_tok_tok");
        assert_eq!(ms_token("sess-A").as_deref(), Some("tok_tok_tok"));
        // 会话隔离：A 的缓存不会回给 B（此时全局份是空的，B 只能拿到 None）。
        assert_eq!(ms_token("sess-B"), None);
    }

    #[test]
    fn global_fallback() {
        let _guard = fresh();
        // 空会话键 = 全局份：还没有自己缓存的会话先用它。
        store_ms_token("", "global-token");
        assert_eq!(ms_token("unknown-session").as_deref(), Some("global-token"));
        // 会话份优先于全局份。
        store_ms_token("known-session", "own-token");
        assert_eq!(ms_token("known-session").as_deref(), Some("own-token"));
    }

    #[test]
    fn blank_token_ignored() {
        let _guard = fresh();
        store_ms_token("blank-session", "   ");
        store_ms_token("", "\t");
        assert_eq!(ms_token("blank-session"), None);
    }
}
