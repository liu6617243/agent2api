//! Qoder 账号以 provider（地区身份）+ userId 识别；凭证与其它提供商共享存储键名。
//!
//! ── 拆家（2026-10）──────────────────────────────────────────
//! 两个地区是**两家 provider**（`qoder` 中国版 / `qoder-intl` 国际版），地区由
//! provider id 推导（`Region::from_provider_id`），不再从账号的 `mode` 字段猜。
//! 记录里仍写 `mode`（凭证落盘的既有字段，公开形态的 `edition` 也由它派生），
//! 但**归属判定只认 provider id** —— 与 AutoClaw / Accio / ZCode 同一口径。
//!
//! ── 实现搬走之后（2026-10 插件化）───────────────────────────
//! 设备授权登录、PAT 换证、续期、目录、额度这些**上游协议知识**已随实现搬到
//! 提供商插件，本文件保留账号层的网关职责：
//!
//! 1. **归属判定**（`qoder_account_record`）：续期分派靠它认出「这条记录属于
//!    本家」，否则会落到 workbuddy 兜底链路、给出与本家无关的错误。
//! 2. **存量记录的公开形态**（`to_qoder_public_account`）：升级上来的老账号没有
//!    `pluginSecret`，走不到插件投影；不在这里补形状就会被渲染成 WorkBuddy 的
//!    空壳（uid 空、tokenTail 空），账号明明能用界面却像坏了。
//! 3. **地区迁移与身份匹配**：缺少地区字段不能作为国际版的证据；同一 userId
//!    在两个地区是两条账号，重新登录复用原主键以保留请求历史。
//!
//! 新落的账号由插件写入（凭证在 `pluginSecret` 里），公开形态直接走插件投影。

use std::sync::MutexGuard;

use serde_json::{json, Map, Value};
use sha2::{Digest, Sha256};

use crate::server::core::providers::qoder::endpoints::Region;

use super::sql;
use super::state::StoredAccount;
use super::store::{AccountStore, AccountStoreError};

/// 网页会话 cookie 的落盘键（与插件侧 `qoder/credentials.rs` 的 `sessionCookie` 同值，
/// 两边各自声明同一个字面量 —— 它是**落盘契约**，插件与网关没有代码依赖）。
pub(crate) const SESSION_COOKIE_FIELD: &str = "sessionCookie";

/// cookie 长度上限（与插件侧 `credentials::MAX_COOKIE_LENGTH` 同值）。
const MAX_COOKIE_LENGTH: usize = 8192;

/// 把一段原始 cookie 文本压成**可单行进 HTTP 头**的形态（`a=1; b=2`）。
///
/// 与插件侧 `credentials::normalize_cookie` **逐条同规则**：用户最常见的来源是
/// 浏览器的「Copy as cURL」或整段请求头，那里 `Cookie:` 后面常跟若干行
/// `name=value;`；换行原样塞进 `Cookie` 头会让请求在第一个 `\r\n` 处断掉，
/// 而上游失败时插件是**静默降级**（明细返回空、界面只少几行），用户完全看不出
/// 是粘贴格式的锅。网关侧也过这一遍，是因为写入路径在网关（PATCH 账号），
/// 不能让「界面粘贴多行」与「插件读取」之间留一个只压一半的缺口。
fn normalize_session_cookie(raw: &str) -> String {
    let raw = raw.trim();
    if raw.is_empty() {
        return String::new();
    }
    let body = raw
        .lines()
        .map(str::trim)
        .map(|line| {
            line.strip_prefix("Cookie:")
                .or_else(|| line.strip_prefix("cookie:"))
                .unwrap_or(line)
                .trim()
        })
        .filter(|line| !line.is_empty())
        .map(|line| if line.ends_with(';') { line.to_string() } else { format!("{line};") })
        .collect::<Vec<_>>()
        .join(" ");
    body.trim_end_matches(';')
        .trim()
        .chars()
        .take(MAX_COOKIE_LENGTH)
        .collect()
}

fn explicit_region(fields: &Map<String, Value>) -> Option<Region> {
    let value = ["mode", "edition"].into_iter().find_map(|key| {
        fields.get(key).filter(|value| {
            !value.is_null() && !matches!(value, Value::String(text) if text.trim().is_empty())
        })
    })?;
    let text = value.as_str()?.trim();
    Region::from_provider_id(text).or_else(|| Region::parse(text).ok())
}

fn plugin_region(record: &StoredAccount) -> Option<Region> {
    // 公开快照可能在错分后的续期中被重写，恢复时优先取凭证原有的地区。
    record.get("pluginSecret").and_then(Value::as_object).and_then(explicit_region)
        .or_else(|| record.get("pluginPublic").and_then(Value::as_object).and_then(explicit_region))
}

pub(crate) fn migrated_region(record: &StoredAccount) -> Option<Region> {
    let region = Region::from_provider_id(&record.provider())?;
    if !record.has_plugin_secret() {
        return explicit_region(record.fields()).or(Some(region));
    }
    // 插件账号的 provider 是权威；只对有明确国内版证据的错分国际版记录归位。
    if region == Region::Global && plugin_region(record) == Some(Region::Cn) {
        Some(Region::Cn)
    } else {
        Some(region)
    }
}

pub(super) fn migrate_accounts(accounts: &mut [StoredAccount]) -> usize {
    let mut changed = 0;
    for record in accounts {
        let Some(region) = migrated_region(record) else { continue; };
        if record.provider() != region.provider_id() {
            record.set_provider(region.provider_id());
            changed += 1;
        }
    }
    changed
}

fn user_id_of(fields: &Map<String, Value>) -> Option<String> {
    ["userId", "user_id", "uid"].into_iter().find_map(|key| match fields.get(key) {
        Some(Value::String(text)) if !text.trim().is_empty() => Some(text.trim().to_string()),
        Some(Value::Number(number)) => Some(number.to_string()),
        _ => None,
    })
}

fn record_user_id(record: &StoredAccount) -> Option<String> {
    record.get("pluginSecret").and_then(Value::as_object).and_then(user_id_of)
        .or_else(|| user_id_of(record.fields()))
}

impl AccountStore {
    pub(super) fn qoder_account_for_upsert(
        &self,
        guard: &MutexGuard<'_, ()>,
        region: Region,
        identity: &str,
        secret: &Value,
    ) -> Result<(String, Option<StoredAccount>), String> {
        let mut records = self.with_conn(guard, |conn| {
            let ids: Vec<String> = sql::queue_rows(conn)?.into_iter()
                .filter(|row| Region::from_provider_id(&row.provider).is_some())
                .map(|row| row.id).collect();
            ids.into_iter().map(|id| sql::load_by_id(conn, &id))
                .collect::<rusqlite::Result<Vec<_>>>()
        }).map_err(|error| error.message)?;
        let user_id = secret.as_object().and_then(user_id_of);
        let existing = records.drain(..).flatten()
            .filter(|record| migrated_region(record) == Some(region))
            .filter(|record| match user_id.as_ref() {
                Some(user_id) => record_user_id(record).as_ref() == Some(user_id),
                None => record.id() == identity,
            })
            .min_by_key(|record| (record.added_at(), record.order_key(), record.id().to_string()));
        if let Some(record) = existing {
            return Ok((record.id().to_string(), Some(record)));
        }
        let id = format!("qoder-{}-{:x}", region.id(),
            Sha256::digest(user_id.as_deref().unwrap_or(identity).as_bytes()));
        if self.with_conn(guard, |conn| sql::load_by_id(conn, &id))
            .map_err(|error| error.message)?.is_some() {
            return Err("Qoder 账号 ID 已被其它账号占用，请先核对账号记录".to_string());
        }
        Ok((id, None))
    }

    /// 某一地区的账号记录：非空 id 按 id 直查（核对归属），空 id 取该地区
    /// **队首可用**账号（启用 + 有令牌，按全局优先级序）—— 与
    /// `autoclaw_account_record` 同一款形状（拆家后「队首」必须限定在本地区）。
    pub fn qoder_account_record(&self, region: Region, account_id: &str) -> Option<Value> {
        let guard = self.guard();
        let provider = region.provider_id();
        if !account_id.is_empty() {
            // 按 id 直查一行后确认归属（同 id 只会有一条，provider 判定是保险）
            let record = self.record_by_id(&guard, account_id)?;
            return (record.provider() == provider).then(|| record.to_value());
        }
        self.records_for_provider(&guard, provider)
            .into_iter()
            .filter(|record| record.enabled() && record.has_token())
            .min_by_key(|record| record.order_key())
            .map(|record| record.to_value())
    }

    /// 设置 / 清除某条账号的**网页会话 cookie**（`sessionCookie`）。
    ///
    /// ── 为什么需要这个入口（而不是复用添加账号那条链）─────────────
    /// cookie 不是刷新产物，也没有任何上游接口能把它换出来（Qoder 的
    /// `POST /api/v1/sessions` 报 `CSRFInvalid`；CodeArts 的 AK/SK 打 portal 路径
    /// 回 404），它**只能由用户从浏览器复制**。而它又是「逐批积分到期时间」的唯一
    /// 凭证（Qoder 读 `big_model_credits`，CodeArts 读 `portal/snap-manager/v1/
    /// package/credit/page` + `ops/credit/campaign/{id}`）。没有这个入口时，用户补
    /// 一条 cookie 只能删掉账号重加一遍 —— 那会丢掉请求历史、优先级、限制器与代理
    /// 配置，代价与「贴一行 cookie」完全不成比例。
    ///
    /// ── 为什么函数名还叫 `qoder_*`（2026-11-08 扩到 CodeArts 之后）──
    /// 它现在服务两家（判据见 `super::supports_session_cookie`），但**落盘键与
    /// 压平规则两家逐字相同**，改名只会让调用点与文档一起漂。保持原名、把「谁认这个
    /// 键」交给那一个判据函数，比造一个 `update_session_cookie` 再保留一个转发壳干净。
    ///
    /// ── 两种记录形态（与 `update_catpaw_balance_token` 同一套）───────
    ///   - **插件记录**（有 `pluginSecret`）：写进 secret 里的同名键 —— 插件
    ///     `Credentials::from_payload` 读的就是它，写进去立刻生效；
    ///   - **存量内置记录**：写记录顶层（插件摊平读取时同样认这个键）。
    ///
    /// ── 为什么字段名校验 / 压平都在这里做 ─────────────────────────
    /// 这个值最终原样拼进 `Cookie` 头。多行粘贴（Copy as cURL）不压平会让请求在
    /// 第一个 `\r\n` 处断掉，而插件对网页通道失败是静默降级 —— 用户会以为「贴了
    /// 没生效」却找不到原因。所以写入侧就压成单行，读取侧插件那边还有一道
    /// （兼容早期已经落了多行的记录）。
    ///
    /// `value` 语义：字符串（压平后非空）= 设置；`null` / 空串 = **清除**。
    /// 与余额凭证同一口径，清除是一个显式动作而不是「留空即清除」。
    pub fn update_qoder_session_cookie(
        &self,
        id: &str,
        value: &Value,
    ) -> Result<Vec<String>, AccountStoreError> {
        let next: Option<String> = match value {
            Value::Null => None,
            other => {
                let text = match other {
                    Value::String(text) => text.clone(),
                    _ => other.to_string(),
                };
                let cookie = normalize_session_cookie(&text);
                if cookie.is_empty() {
                    None
                } else {
                    Some(cookie)
                }
            }
        };
        // ── 插件记录：写进 `pluginSecret` ────────────────────────
        // 判据是记录上**有没有** `pluginSecret`（与 CatPaw 那一段同一理由：这个
        // 函数对存量内置记录也会摊平出一份，拿它当判据会把插件记录的写回也走成
        // 「改写顶层」）。锁在这里先放开：`plugin_update_secret` 内部还要取同一把锁。
        let plugin_secret = {
            let guard = self.guard();
            self.record_by_id(&guard, id)
                .filter(|record| super::supports_session_cookie(&record.provider()))
                .filter(StoredAccount::has_plugin_secret)
                .map(|record| record.get("pluginSecret").cloned().unwrap_or(Value::Null))
        };
        if let Some(secret) = plugin_secret {
            let mut next_secret = secret.clone();
            if let Some(object) = next_secret.as_object_mut() {
                match &next {
                    Some(cookie) => {
                        object.insert(SESSION_COOKIE_FIELD.to_string(), Value::String(cookie.clone()));
                    }
                    None => {
                        object.remove(SESSION_COOKIE_FIELD);
                    }
                }
            }
            match self.plugin_update_secret(id, &secret, &next_secret) {
                Ok(true) => {
                    return Ok(vec![match next {
                        Some(_) => "网页会话 cookie 已更新".to_string(),
                        None => "网页会话 cookie 已清除".to_string(),
                    }])
                }
                // 期望值已变（插件在这一瞬间刷新并写回了新令牌）：让用户重试一次，
                // 不要盲目覆盖 —— 覆盖掉的可能正是刚刷新出来的有效令牌
                Ok(false) => {
                    return Err(AccountStoreError::new("账号凭证已变化，请刷新后重试", 409))
                }
                Err(message) => return Err(AccountStoreError::new(message, 500)),
            }
        }
        // ── 存量内置记录：写顶层字段 ────────────────────────────
        let _guard = self.guard();
        let mut record = self
            .record_by_id(&_guard, id)
            .filter(|record| super::supports_session_cookie(&record.provider()))
            .ok_or_else(|| AccountStoreError::not_found("账号不存在或不属于认网页会话 cookie 的提供商"))?;
        let current = record
            .get(SESSION_COOKIE_FIELD)
            .and_then(Value::as_str)
            .map(str::to_string);
        if current == next {
            return Ok(Vec::new());
        }
        let mut changes = Vec::new();
        match &next {
            Some(cookie) => {
                record.set(SESSION_COOKIE_FIELD, Value::String(cookie.clone()));
                // 日志只说「已更新」，**不打印 cookie 本身**（与全仓的凭证处理一致）
                changes.push("网页会话 cookie 已更新".to_string());
            }
            None => {
                record.remove(SESSION_COOKIE_FIELD);
                changes.push("网页会话 cookie 已清除".to_string());
            }
        }
        record.set_updated_at(crate::server::logging::now_ms());
        let name = record.name();
        self.with_conn(&_guard, |conn| sql::update_in_place(conn, &record))?;
        crate::server::logging::log(
            "[Accounts]",
            &format!("✏️  {} 账号已更新: {name}（{}）", record.provider(), changes.join("，")),
        );
        Ok(changes)
    }

    pub fn to_qoder_public_account(&self, record: &StoredAccount) -> Value {
        // 地区先看 provider 身份（拆家后的权威来源），payload 里的 `mode` 只是
        // 旧数据的兼容读取 —— 记录缺 `mode` 时 `from_payload` 会默认 Global，
        // 那个默认对中国版记录是错的，不能当首选。
        let region = Region::from_provider_id(&record.provider())
            .or_else(|| Region::from_payload(&record.to_value()).ok());
        let available = record.has_token() && region.is_some();
        // 能不能自动续期只看记录里有没有 refreshToken —— 网关不再解析这家的
        // 凭据形状（`Credentials` 随实现搬到插件了）。判据与插件侧
        // `credentials.can_refresh()` 等价：非空即算。
        let can_refresh = !record.refresh_token().trim().is_empty();
        let mut public = Map::new();
        for key in ["id", "provider", "name", "userId", "email", "nickname", "mode", "source", "tokenTail", "expiresAt"] {
            public.insert(key.to_string(), record.get(key).cloned().unwrap_or(Value::Null));
        }
        public.insert("edition".to_string(), region.map(|value| Value::String(value.edition().to_string())).unwrap_or(Value::Null));
        public.insert("editionLabel".to_string(), region.map(|value| Value::String(value.label().to_string())).unwrap_or(Value::Null));
        public.insert("hasRefreshToken".to_string(), Value::Bool(can_refresh));
        public.insert("priority".to_string(), Value::from(record.priority()));
        public.insert("enabled".to_string(), Value::Bool(record.enabled()));
        public.insert("addedAt".to_string(), Value::from(record.added_at()));
        public.insert("updatedAt".to_string(), Value::from(record.updated_at()));
        public.insert("proxy".to_string(), crate::server::core::proxies::describe_account_proxy(Some(&record.proxy())));
        public.insert("rateLimits".to_string(), record.get("rateLimits").cloned().unwrap_or_else(|| json!({})));
        public.insert("desktop".to_string(), Value::Bool(false));
        public.insert("available".to_string(), Value::Bool(available));
        // `chatSupported` / `maxConcurrent` **刻意不在这里写**：它们是跨家的统一
        // 事实，由 `store_view::public_account` 在分派点注入（前者按适配器的
        // `supports_chat()`，后者按 `store_util::max_concurrent_public_for`）。
        Value::Object(public)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 多行粘贴（Copy as cURL / 整段请求头）必须压成**单行**的 `Cookie` 头形态：
    /// 换行留在值里会让上游请求在第一个 `\r\n` 处断掉，而插件对网页通道失败是
    /// 静默降级 —— 用户只会看到「有效期」列还是空的。
    #[test]
    fn a_multi_line_paste_becomes_one_cookie_header() {
        let raw = "Cookie: arms_uid=abc;\nqoder_session_cookie=xyz;\n  other=1  \n";
        assert_eq!(
            "arms_uid=abc; qoder_session_cookie=xyz; other=1",
            normalize_session_cookie(raw)
        );
        // 单行、只有一个键（页面引导用户复制的那一行）同样原样可用
        assert_eq!(
            "qoder_session_cookie=xyz",
            normalize_session_cookie("qoder_session_cookie=xyz\n")
        );
        // 空 / 全空白 = 清除（调用方据此落 `None`）
        assert_eq!("", normalize_session_cookie("  \n\t\n"));
    }

    /// 超长粘贴要截断到与插件侧同值的上限：这个值最终进 HTTP 头，头部没有长度保护。
    #[test]
    fn an_over_long_cookie_is_truncated() {
        let raw = format!("qoder_session_cookie={}", "a".repeat(MAX_COOKIE_LENGTH + 100));
        assert_eq!(MAX_COOKIE_LENGTH, normalize_session_cookie(&raw).chars().count());
    }
}
