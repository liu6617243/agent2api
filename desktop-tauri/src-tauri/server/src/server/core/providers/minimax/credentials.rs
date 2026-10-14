//! MiniMax 账号凭证：**refresh_token 是主凭证**（一次性轮换），access_token 是短期缓存。
//!
//! ── 与 Google（Antigravity）那条链路的关键差别 ────────────────
//! Google 的 refresh_token 长寿命、刷新时通常**不**回传新值；MiniMax 的
//! refresh_token **每刷一次就换一枚新的、旧值当场吊销**。因此临期判定与回写
//! 策略都必须建立在「每次刷新都会带回新 refresh_token」这一事实上：
//!   - 刷新成功 → 新 access_token + 新 refresh_token **一起**回写（缺一必坏）；
//!   - 落盘形态里 refreshToken 是主凭证（丢了只能重新走设备码授权），
//!     accessToken 只是缓存。
//!
//! ── 过期判定：判不出来就说「不需要刷」─────────────────────────
//! 与 Antigravity / Trae 同一纪律（见 `ProviderAdapter::credentials_expiring`
//! 契约）：记录里没有 `expiresAt`（手工粘贴只给了 refreshToken）时返回 false，
//! 靠 401 之后的懒刷新链路救，不让维护任务每轮空打上游。没有 access_token
//! （从未刷过 / 只粘了 refreshToken）→ true：那种账号必须先刷一次才能用。
//! 提前量取 [`REFRESH_SKEW_MS`]（参考实现在 shim 里留 120s、checkin 留 60s，
//! 这里取仓内既有几家的整档 900s，方向安全——宁可多刷一次）。
//!
//! ── 硬约束 ──────────────────────────────────────────────────
//! release 是 `panic=abort`：本文件零 unwrap/expect/panic。

use serde_json::Value;

use crate::server::errors::GatewayError;

/// 刷新提前量（毫秒）：到期前 15 分钟即视为临期
pub const REFRESH_SKEW_MS: i64 = 900 * 1000;

/// 一次转发 / 刷新 / 余额 / 签到用的凭证快照。
#[derive(Clone, Debug)]
pub struct MinimaxCredentials {
    /// 账号记录 id
    pub id: String,
    /// 主凭证（**一次性**；刷新后必须回写新值，否则下次刷新必然 invalid_grant）
    pub refresh_token: String,
    /// 短期访问令牌（可能为空 —— 需要先刷新）
    pub access_token: String,
    /// 访问令牌过期时刻（毫秒；0 = 判不出来）
    pub expires_at: i64,
    /// 展示名（记录的备注名）
    pub name: String,
}

impl MinimaxCredentials {
    /// 有没有续期手段（refresh_token 非空）
    pub fn can_refresh(&self) -> bool {
        !self.refresh_token.trim().is_empty()
    }

    /// 有没有可用的 access_token（过期与否看 [`Self::expiring`]）
    pub fn has_access_token(&self) -> bool {
        !self.access_token.trim().is_empty()
    }

    /// 该账号此刻是否**已过期或进入提前量窗口**（[`REFRESH_SKEW_MS`]）。
    ///
    /// 判不出来（`expires_at <= 0`）→ false（见模块头）。没有 access_token
    /// → true（必须先刷一次）。
    pub fn expiring(&self) -> bool {
        if !self.has_access_token() {
            return true;
        }
        if self.expires_at <= 0 {
            return false;
        }
        self.expires_at - REFRESH_SKEW_MS <= crate::server::logging::now_ms()
    }

    /// 显示用的脱敏 refresh token 尾号（只给尾 4 位；**绝不打印完整串**）
    pub fn masked_refresh(&self) -> String {
        let trimmed = self.refresh_token.trim();
        if trimmed.is_empty() {
            return "(空)".to_string();
        }
        format!("…{}", tail_chars(trimmed, 4))
    }
}

/// 取字符串末尾 n 个字符（不足时给整串；UTF-8 安全）
pub fn tail_chars(text: &str, count: usize) -> String {
    let total = text.chars().count();
    let skip = total.saturating_sub(count);
    text.chars().skip(skip).collect()
}

/// 从一条取值链里挑第一个非空字符串（记录可能被手工编辑过，宽容读取）
fn pick(record: &Value, keys: &[&str]) -> String {
    for key in keys {
        let value = record
            .get(*key)
            .and_then(Value::as_str)
            .map(str::trim)
            .unwrap_or("");
        if !value.is_empty() {
            return value.to_string();
        }
    }
    String::new()
}

/// 记录里的过期时刻（毫秒；接受数字与数字字符串两种形态）。
fn expires_at_of(record: &Value) -> i64 {
    match record.get("expiresAt") {
        Some(Value::Number(number)) => number.as_i64().unwrap_or(0),
        Some(Value::String(text)) => text.trim().parse::<i64>().unwrap_or(0),
        _ => 0,
    }
}

/// 从账号记录解析凭证（`record` 为 None 时报「没有可用账号」）。
///
/// 字段兼容：`refreshToken`（本项目落盘名）/ `refresh_token`；`accessToken` /
/// `access_token`。
///
/// **主凭证优先 refreshToken**：它是这家唯一的续期手段。只有 accessToken 没有
/// refreshToken 的记录也能解析出来（转发能用直到令牌过期），但 `can_refresh()`
/// 为 false、临期后无法续期——账号页据此提示重新粘贴完整的令牌对。
pub fn from_record(record: Option<&Value>) -> Result<MinimaxCredentials, GatewayError> {
    let Some(record) = record else {
        return Err(GatewayError::with_status(
            401,
            "没有可用的 MiniMax 账号：请在账号页粘贴 MiniMax 令牌（refreshToken）添加",
        ));
    };
    let refresh_token = pick(record, &["refreshToken", "refresh_token"]);
    let access_token = pick(record, &["accessToken", "access_token"]);
    if refresh_token.is_empty() && access_token.is_empty() {
        return Err(GatewayError::with_status(
            401,
            "MiniMax 账号缺少令牌：请粘贴 refreshToken（建议连同 accessToken 与 expiresAt）",
        ));
    }
    Ok(MinimaxCredentials {
        id: pick(record, &["id"]),
        refresh_token,
        access_token,
        expires_at: expires_at_of(record),
        name: pick(record, &["name"]),
    })
}

/// 记录里的备注名 + 尾号的展示口径（账号页显示用）。
pub fn display_name(credentials: &MinimaxCredentials) -> String {
    if !credentials.name.trim().is_empty() {
        return credentials.name.trim().to_string();
    }
    format!("MiniMax {}", credentials.masked_refresh())
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::{from_record, tail_chars};

    #[test]
    fn prefers_refresh_token_camel_and_snake() {
        let snake = from_record(Some(&json!({ "refresh_token": "rt-1" })))
            .expect("snake_case 应能解析");
        assert_eq!(snake.refresh_token, "rt-1");
        assert!(snake.can_refresh());
    }

    #[test]
    fn empty_record_rejected() {
        assert!(from_record(Some(&json!({ "id": "minimax-x" }))).is_err());
        assert!(from_record(None).is_err());
    }

    #[test]
    fn tail_chars_survives_multibyte() {
        assert_eq!(tail_chars("token你好", 2), "你好");
        assert_eq!(tail_chars("abcd", 10), "abcd");
    }
}
