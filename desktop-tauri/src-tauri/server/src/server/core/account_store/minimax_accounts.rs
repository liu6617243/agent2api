//! MiniMax 账号：粘贴 refreshToken 落账号 + 一次性轮换的刷新回写 + 公开形态。
//!
//! ── 账号字段（落盘形态）────────────────────────────────────────
//! ```text
//!   id            派生（见下）
//!   provider      "minimax"
//!   name          备注名（可空；界面兜底用「MiniMax …尾号」）
//!   refreshToken  主凭证（**一次性轮换**：每刷一次换一枚新的、旧值当场作废）
//!   accessToken   短期令牌（约 3600 秒；刷新链路就地更新，只是缓存）
//!   tokenTail     refreshToken 尾 4 位（展示用；**token 本体不进公开形态**）
//!   expiresAt     访问令牌过期时刻（毫秒；口径 = now + expires_in）
//!   userId        用户随粘贴提供的身份串（可选；没有就不写）
//!   source/priority/enabled/addedAt/updatedAt  与别家同形
//! ```
//!
//! ── 账号 id 怎么派生（为什么不用 email/userId）─────────────────
//! MiniMax 的 OAuth 响应里没有可用的稳定身份（实测刷新响应只有令牌本身），
//! 网关也不解 JWT claim 猜身份。因此身份键顺序是 **用户备注名 → refreshToken
//! 摘要**：
//!   - 粘贴时带了备注名（或记录里已有）→ `sha256("minimax:account-id:v1\0" +
//!     名字小写)[..12]`：同一个名字再粘一次命中同一条记录（换令牌 = 更新），
//!     不同账号要用户起不同名字 —— 与界面「一个名字一个号」的直觉一致；
//!   - 没名字 → 退到令牌摘要：**同一枚**令牌再粘一次仍命中同一条（幂等更新），
//!     重新授权拿到的**新**令牌会新增一条记录（一次性轮换下旧 rt 已作废，
//!     那条旧记录天然不可用，用户按尾号手工删掉即可）。
//! 摘要确定性、不可反推令牌、撞 id 概率可忽略（与 `antigravity_accounts` /
//! `commandcode_accounts` 同一手法）。
//!
//! ── 为什么「同 rt 查找」是必须的 ───────────────────────────────
//! 一次性轮换意味着刷新链会把记录里的 rt 换成新值；用户如果又粘了一次
//! **旧** rt，那条记录已换新值、按旧值派生的新 id 会撞出一条注定 401 的死号。
//! 因此先按「库里已有同 rt」归并到那条记录（与 Antigravity 的 email 归并同
//! 一层用途，这里身份缺位，用 rt 顶上）。
//!
//! 本文件全是「读-改-写」存储操作，**没有任何网络请求**（持锁不做网络）。
//! 绝不 unwrap/expect（release 是 panic=abort）。

use serde_json::{json, Map, Value};
use sha2::{Digest, Sha256};

use crate::server::core::providers::minimax::credentials;

use super::priority::next_free_priority;
use super::sql;
use super::state::{mark_name_custom, StoredAccount};
use super::store::{AccountStore, AccountStoreError};
use super::store_util::{token_tail_of, truncate_chars};
use super::{CredentialWrite, MINIMAX_PROVIDER_ID};
use crate::server::logging;

/// 备注名长度上限（与别家同一口径）
const MAX_NAME_LENGTH: usize = 100;

/// 身份 / 令牌字段长度上限
const MAX_IDENTITY_LENGTH: usize = 256;
const MAX_TOKEN_LENGTH: usize = 8192;

/// 访问令牌过期时刻缺失时的保守估计（实测 3600 秒；宁短勿长——
/// 判早了顶多多刷一次，判晚了会拿过期令牌打推理）
const FALLBACK_TOKEN_TTL_MS: i64 = 3600 * 1000;

/// 从 payload 里取第一个非空字符串（候选键覆盖本项目落盘名与粘贴形态）
fn pick(payload: &Value, keys: &[&str]) -> String {
    for key in keys {
        let value = payload
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

/// 名字或令牌 → 稳定的账号 id（见模块头）
fn account_id_for(name: &str, refresh_token: &str) -> String {
    let seed = if name.is_empty() { refresh_token } else { name };
    let mut hasher = Sha256::new();
    hasher.update(b"minimax:account-id:v1\0");
    hasher.update(seed.as_bytes());
    let digest = format!("{:x}", hasher.finalize());
    let head = digest.get(..12).unwrap_or(digest.as_str());
    format!("minimax-{head}")
}

impl AccountStore {
    /// 取一条 MiniMax 账号记录（公开形态的原始 JSON）。
    ///
    /// `account_id` 非空 → 按 id 直查（不属于本家返回 None）；为空 → 本家
    /// **启用中**账号里优先级最靠前的一条（转发与余额刷新默认用队首账号）。
    pub fn minimax_account_record(&self, account_id: &str) -> Option<Value> {
        let guard = self.guard();
        if !account_id.is_empty() {
            let record = self.record_by_id(&guard, account_id)?;
            return (record.provider() == MINIMAX_PROVIDER_ID).then(|| record.to_value());
        }
        let mut candidates: Vec<StoredAccount> = self
            .records_for_provider(&guard, MINIMAX_PROVIDER_ID)
            .into_iter()
            .filter(StoredAccount::enabled)
            .collect();
        candidates.sort_by_key(StoredAccount::order_key);
        candidates.into_iter().next().map(|item| item.to_value())
    }

    /// 添加/更新一个 MiniMax 账号（粘贴 refreshToken 的唯一落账号入口）。
    ///
    /// payload（界面直接传来的形状，兼容 snake_case）：
    ///   - `refreshToken` / `refresh_token`：主凭证（**必填**）；
    ///   - `accessToken` / `access_token`：短期令牌（可选；带上能立即转发，
    ///     省一次刷新）；
    ///   - `expiresAt`：访问令牌过期时刻（毫秒，可选）；
    ///   - `userId` / `user_id`：用户自备的身份串（可选；仅展示与导认身份）；
    ///   - `name`：备注名（身份键，见模块头）。
    ///
    /// 归并顺序：同 id → 同 refreshToken → 同名 → 新建。撞到**别的 provider**
    /// 的 id 时报错，绝不覆写。
    pub fn add_minimax_account(
        &self,
        payload: &Value,
        name: Option<&str>,
        source: &str,
    ) -> Result<Value, AccountStoreError> {
        let refresh_token = pick(payload, &["refreshToken", "refresh_token"]);
        if refresh_token.is_empty() {
            return Err(AccountStoreError::new(
                "缺少 refresh token（refreshToken / refresh_token）",
                400,
            ));
        }
        if refresh_token.chars().count() > MAX_TOKEN_LENGTH {
            return Err(AccountStoreError::new("refresh token 过长", 400));
        }
        let access_token = truncate_chars(&pick(payload, &["accessToken", "access_token"]), MAX_TOKEN_LENGTH);
        let user_id = truncate_chars(&pick(payload, &["userId", "user_id"]), MAX_IDENTITY_LENGTH);
        let explicit_name = name
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(|value| truncate_chars(value, MAX_NAME_LENGTH));
        let identity_name = explicit_name
            .clone()
            .unwrap_or_default()
            .to_ascii_lowercase();
        let provider_id = MINIMAX_PROVIDER_ID;

        let guard = self.guard();
        // 归并查找：同 rt（防粘旧令牌开死号）→ 同名（重新授权的新令牌归并进
        // 原名字那条记录）；id 直接命中在最后兜底（同名不同 rt 的新建路径）
        let existing = self
            .records_for_provider(&guard, provider_id)
            .into_iter()
            .find(|record| {
                record.refresh_token() == refresh_token
                    || (!identity_name.is_empty()
                        && record.name().trim().to_ascii_lowercase() == identity_name)
            })
            .or_else(|| {
                // 名字派生的 id 命中既有记录（可能是同名异 rt 之外的手工形态）
                let candidate_id = account_id_for(&identity_name, &refresh_token);
                let by_id = self.record_by_id(&guard, &candidate_id)?;
                (by_id.provider() == provider_id).then_some(by_id)
            });
        let id = match existing.as_ref() {
            Some(record) => record.id().to_string(),
            None => account_id_for(&identity_name, &refresh_token),
        };
        if existing.is_none() {
            if let Some(occupied) = self.record_by_id(&guard, &id) {
                return Err(AccountStoreError::new(
                    format!(
                        "账号 id「{id}」已被{}账号占用，无法添加 MiniMax 账号（请换一个备注名）",
                        occupied.provider()
                    ),
                    400,
                ));
            }
        }
        let record_name = explicit_name
            .or_else(|| {
                existing
                    .as_ref()
                    .map(StoredAccount::name)
                    .filter(|value| !value.is_empty())
            })
            .unwrap_or_else(|| format!("MiniMax {}", token_tail_of(&refresh_token)));
        let priority = match existing.as_ref() {
            Some(record) => record.priority(),
            None => {
                let used = self.with_conn(&guard, |conn| sql::priorities_all(conn))?;
                next_free_priority(&used)
            }
        };
        let now = logging::now_ms();
        let expires_at = payload
            .get("expiresAt")
            .and_then(|value| {
                value
                    .as_i64()
                    .or_else(|| value.as_str().and_then(|text| text.trim().parse::<i64>().ok()))
            })
            .filter(|value| *value > 0)
            .or_else(|| {
                existing
                    .as_ref()
                    .and_then(StoredAccount::expires_at)
                    .filter(|value| *value > 0.0)
                    .map(|value| value as i64)
            })
            .unwrap_or(now + FALLBACK_TOKEN_TTL_MS);

        let mut record = Map::new();
        record.insert("id".to_string(), Value::String(id.clone()));
        record.insert("provider".to_string(), Value::String(provider_id.to_string()));
        record.insert("name".to_string(), Value::String(record_name.clone()));
        mark_name_custom(
            &mut record,
            name.is_some_and(|value| !value.trim().is_empty()),
            existing.as_ref(),
        );
        record.insert("refreshToken".to_string(), Value::String(refresh_token.clone()));
        record.insert("tokenTail".to_string(), Value::String(token_tail_of(&refresh_token)));
        // 空值**不写**：重新粘贴不该把上次刷新到的 accessToken / expiresAt 洗掉
        // （与 Antigravity / ZCode 同一条规则）
        let previous = existing.as_ref().map(StoredAccount::fields);
        let prefer_previous = |key: &str, value: &str| -> Option<String> {
            if !value.is_empty() {
                return Some(value.to_string());
            }
            previous
                .and_then(|fields| fields.get(key))
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|text| !text.is_empty())
                .map(str::to_string)
        };
        if let Some(token) = prefer_previous("accessToken", &access_token) {
            record.insert("accessToken".to_string(), Value::String(token));
        }
        if !user_id.is_empty() {
            record.insert("userId".to_string(), Value::String(user_id));
        } else if let Some(value) = previous.and_then(|fields| fields.get("userId")).cloned() {
            record.insert("userId".to_string(), value);
        }
        record.insert("expiresAt".to_string(), Value::from(expires_at));
        record.insert("source".to_string(), Value::String(source.to_string()));
        record.insert("priority".to_string(), Value::from(priority));
        record.insert(
            "enabled".to_string(),
            Value::Bool(existing.as_ref().map(StoredAccount::enabled).unwrap_or(true)),
        );
        record.insert(
            "addedAt".to_string(),
            Value::from(
                existing
                    .as_ref()
                    .map(StoredAccount::added_at)
                    .filter(|value| *value != 0)
                    .unwrap_or(now),
            ),
        );
        record.insert("updatedAt".to_string(), Value::from(now));
        // 未知字段全量保留（用户手工加过的字段不能因为一次「更新账号」丢掉）
        let mut merged = record;
        if let Some(existing) = existing.as_ref() {
            for (key, value) in existing.fields() {
                merged.entry(key.clone()).or_insert_with(|| value.clone());
            }
        }
        let saved = StoredAccount::from_map(merged);
        self.with_conn(&guard, |conn| sql::put(conn, &saved))?;
        logging::log(
            "[Accounts]",
            &format!(
                "✅ MiniMax 账号已保存: {record_name}（优先级 {priority}，refresh token {}）",
                credentials::tail_chars(&refresh_token, 4)
            ),
        );
        Ok(self.public_account(&saved))
    }

    /// 刷新成功后回写新令牌（`oauth::refresh_and_save` 调用）。
    ///
    /// ── 比较-再写（与 Antigravity 同一纪律）───────────────────
    /// 只当记录里**此刻的** refresh token 仍等于刷新前那份快照时才写入；
    /// 不一致（用户重新粘贴 / 换号 / 另一轮刷新先落地）返回
    /// [`CredentialWrite::Stale`]。
    ///
    /// 与 Antigravity 的反差：MiniMax 的 refreshToken **每次刷新必轮换**，
    /// `refresh_token` 参数给到新值时**覆盖**落盘（None = 异常形态，调用方
    /// 已在日志里警告过，这里保留旧值）。`access_token` 为空 /
    /// `expires_at <= 0` 一律保留旧值。
    pub fn update_minimax_credentials_if_current(
        &self,
        id: &str,
        expected_refresh_token: &str,
        access_token: &str,
        refresh_token: Option<&str>,
        expires_at: i64,
    ) -> Result<CredentialWrite, String> {
        let guard = self.guard();
        let Some(mut record) = self.record_by_id(&guard, id) else {
            // 账号已被删除：结果无处可写，也不该新建记录
            return Ok(CredentialWrite::Stale);
        };
        if record.provider() != MINIMAX_PROVIDER_ID {
            return Err(format!("账号 {id} 不是 MiniMax 账号"));
        }
        if record.refresh_token() != expected_refresh_token {
            return Ok(CredentialWrite::Stale);
        }
        let access_token = access_token.trim();
        if !access_token.is_empty() {
            record.set("accessToken", Value::String(access_token.to_string()));
        }
        if let Some(refresh) = refresh_token.map(str::trim).filter(|text| !text.is_empty()) {
            record.set("refreshToken", Value::String(refresh.to_string()));
            record.set("tokenTail", Value::String(token_tail_of(refresh)));
        }
        if expires_at > 0 {
            record.set("expiresAt", Value::from(expires_at));
        }
        record.set_updated_at(logging::now_ms());
        self.with_conn(&guard, |conn| sql::update_in_place(conn, &record))
            .map_err(|error| error.message)?;
        Ok(CredentialWrite::Written)
    }

    /// MiniMax 账号的**公开形态**（进 HTTP 响应）。
    ///
    /// 与别家同一条硬约束：**绝不透出令牌本体** —— 只给 `tokenTail`
    /// （refreshToken 尾 4 位）与 `hasRefreshToken` / `hasAccessToken` 两个
    /// 存在性标记；`expiresAt` 是展示/排障需要的事实，可以给。
    pub fn to_minimax_public_account(&self, record: &StoredAccount) -> Value {
        let refresh_token = record.refresh_token();
        let mut public = Map::new();
        for key in ["id", "provider", "name", "userId", "source", "tokenTail"] {
            public.insert(
                key.to_string(),
                record.get(key).cloned().unwrap_or(Value::Null),
            );
        }
        public.insert(
            "hasRefreshToken".to_string(),
            Value::Bool(!refresh_token.trim().is_empty()),
        );
        public.insert(
            "hasAccessToken".to_string(),
            Value::Bool(
                record
                    .get("accessToken")
                    .and_then(Value::as_str)
                    .is_some_and(|value| !value.trim().is_empty()),
            ),
        );
        public.insert(
            "expiresAt".to_string(),
            Value::from(record.expires_at().unwrap_or(0.0) as i64),
        );
        public.insert("desktop".to_string(), Value::Bool(false));
        public.insert("priority".to_string(), Value::from(record.priority()));
        public.insert("enabled".to_string(), Value::Bool(record.enabled()));
        public.insert("addedAt".to_string(), Value::from(record.added_at()));
        public.insert("updatedAt".to_string(), Value::from(record.updated_at()));
        public.insert(
            "proxy".to_string(),
            crate::server::core::proxies::describe_account_proxy(Some(&record.proxy())),
        );
        public.insert(
            "rateLimits".to_string(),
            record.get("rateLimits").cloned().unwrap_or_else(|| json!({})),
        );
        // 可用 = 有主凭证（refreshToken 是这家唯一的续期手段）
        public.insert(
            "available".to_string(),
            Value::Bool(!refresh_token.trim().is_empty()),
        );
        Value::Object(public)
    }
}
