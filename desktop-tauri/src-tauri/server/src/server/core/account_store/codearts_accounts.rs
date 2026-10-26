//! CodeArts 账号的**存量投影**（实现已整体搬到提供商插件）。
//!
//! ── 这里只剩两件事 ──────────────────────────────────────────
//!   1. `CODEARTS_PROVIDER_ID`：落盘契约（`provider` 字段的值），别处比对都用它；
//!   2. `to_codearts_public_account`：**升级上来的老账号**的公开形态。
//!
//! ── 为什么投影必须留着 ──────────────────────────────────────
//! 存量记录的凭证散在顶层字段里（`access_key_id` / `security_token` /
//! `oauth_context`…），没有 `pluginSecret` —— `store_view::public_account`
//! 因此走不到「插件记录优先」那一支。不留投影，这些账号会退化成
//! `public_account` 的通用骨架（uid 空、tokenTail 空、`hasRefreshToken` false），
//! 账号页上看起来像坏了。
//!
//! ── 什么已经搬走了 ──────────────────────────────────────────
//! 凭据解析（`Credential::from_payload` / `to_record_value`）、身份判据
//! （`domain_id + user_id`）、续期写回、福利台账、登录落库 —— 全在插件里。
//! 新账号由插件经 `plugin_upsert_account` 落库（凭证在 `pluginSecret`），
//! 上面那条「插件记录优先」会先带走它们，本文件对这些记录是逐字空操作。
//!
//! ── 硬约束 ──────────────────────────────────────────────────
//! release 是 `panic=abort`：本文件零 unwrap/expect/panic。

use serde_json::{json, Value};

use crate::server::core::providers::{kind_id, ProviderKind};

use super::state::StoredAccount;

/// 本家的 provider id（`store_view` 的形状分派与别处比对都要用，
/// 与其它几家同一命名：`<X>_PROVIDER_ID`）。
pub const CODEARTS_PROVIDER_ID: &str = kind_id(ProviderKind::CodeArts);

impl super::AccountStore {
    /// 界面用的公开形态（**只服务存量内置记录**）。
    ///
    /// **绝不带任何凭证字段**（AK/SK/STS/refresh token/DPoP 私钥一个都不出现
    /// 在这里）。
    ///
    /// 判据不解析凭据结构 —— 网关不再认识这家的凭据形状（那部分在插件里）。
    /// 这里只读通用键：`hasRefreshToken` 看 `refreshToken` 在不在，
    /// `available` 看记录有没有 token。缺哪一项就如实报缺，不猜。
    pub(crate) fn to_codearts_public_account(&self, record: &StoredAccount) -> Value {
        let available = record.has_token();
        let can_refresh = !record.refresh_token().is_empty();
        let mut public = serde_json::Map::new();
        for key in ["id", "provider", "name", "userId", "source", "tokenTail", "expiresAt"] {
            public.insert(key.to_string(), record.get(key).cloned().unwrap_or(Value::Null));
        }
        public.insert("edition".to_string(), Value::Null);
        public.insert("editionLabel".to_string(), Value::Null);
        public.insert("hasRefreshToken".to_string(), Value::Bool(can_refresh));
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
        public.insert("desktop".to_string(), Value::Bool(false));
        public.insert("available".to_string(), Value::Bool(available));
        // `maxConcurrent` 不在这里写：本家**没有「不限」这个选项**，记录上的 0
        // （含从未设置过）要报成上游的默认并发 3 而不是 0，否则选路读到的 0 等于
        // 不限、不会在本家满员时跳过这个账号（详见 `store_util::
        // max_concurrent_public_for`，那个函数按 provider 套上这层口径）。
        // 进入本家的这个键由 `store_view::public_account` 统一注入。
        //
        // 每日福利领取的台账原样透出（存量记录的顶层键 `codeartsWelfare`；
        // 插件新落的是 `pluginLedger.codeartsWelfare`，那条路走插件投影）：
        // 界面要显示「今天已试过几次 / 是否已到账」。里面**只有活动 id、幂等键
        // 与布尔状态**，没有任何凭据材料，所以不需要像凭据那样过滤。
        public.insert(
            "welfare".to_string(),
            record.get("codeartsWelfare").cloned().unwrap_or(Value::Null),
        );
        // 账号能路由到哪些模型（转发与广告都按这个判），以及它是不是福利通道
        for (key, source) in [
            ("domainId", "domain_id"),
            ("userName", "user_name"),
            ("loginType", "login_type"),
        ] {
            if let Some(value) = record.get(source).filter(|value| !value.is_null()) {
                public.insert(key.to_string(), value.clone());
            }
        }
        Value::Object(public)
    }
}
