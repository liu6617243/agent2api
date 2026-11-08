//! msToken：官方客户端每个请求的 URL 参数都带（Cookie 里另有一颗）。
//!
//! ── 为什么只做 msToken、不做 a_bogus（2026-11-08 的实测决策）──
//! 抓包证实官方请求带 `msToken` 与 `a_bogus` 两个签名件，但两者的可补性
//! 完全不同：
//!
//! · `msToken` 是**服务端下发、前端回放**的轮换令牌：每次响应都经
//!   `x-ms-token` 响应头发给客户端（本插件不带 msToken 的响应里也有这个
//!   头），官方 JS 接住后放进下一个请求的 URL 参数。因此插件只要
//!   「接住响应头 → 带回下一个请求」就是在走官方同一条轮换链路。
//!
//! · `a_bogus` 是纯前端计算签名，但豆包的变体与公开逆向实现**对不上**：
//!   官方解码后 196 字符（含 `==` 补位，146 码位）；douyin 公开旧版实现
//!   （xiaolifeidao-fly/douyin_abogus_python，44 字节结构 + 浏览器信息）
//!   输出约 164~170 字符；mafqla/douyin-api 文档里的 douyin 新版是 192
//!   字符。三个数字互不相等 —— 豆包用的既不是旧版也不是新版 douyin 结构。
//!   塞一个结构错误的签名比不带更危险（服务端可直接判「签名无效」），而
//!   2026-11-08 的实数据表明插件不带 a_bogus 的请求全部成功（同日官方
//!   客户端签到成功、Agent2API 批量签到 10/10 成功），即这些端点目前
//!   **不硬校验** a_bogus。结论：不签，留此记录；若上游将来开始拒签，
//!   参考实现在 `.workbuddy/tmp/abogus/abogus.py`，先抓豆包自己的样本
//!   反推结构再移植，不要拿 douyin 版本硬凑。
//!
//! ── 为什么「没有就不带」，而不是自己造一颗 ────────────────────
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
