//! CodeArts 账号的**存量投影**（实现已整体搬到提供商插件）。
//!
//! ── 存量展示与配置迁移 ──────────────────────────────────────
//! `CODEARTS_PROVIDER_ID` 保持落盘身份；公开投影服务旧账号，限制器迁移清理已退役
//! 活动的 Token 条件，不改凭证、账号身份和启用状态。
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

use crate::server::core::limiter::{self, LimiterKind, LimiterRule};
use crate::server::core::limiter::types::Metric;
use crate::server::core::providers::{kind_id, ProviderKind};

use super::sql;
use super::state::StoredAccount;
use crate::server::core::account_store::store::AccountStoreError;

/// 本家的 provider id（`store_view` 的形状分派与别处比对都要用，
/// 与其它几家同一命名：`<X>_PROVIDER_ID`）。
pub const CODEARTS_PROVIDER_ID: &str = kind_id(ProviderKind::CodeArts);

/// **华为云 SSO 票根**（`SSOTGC`）的落盘键。
///
/// ── 为什么网关口也要有这一份字面量 ──────────────────────────
/// 插件的 `credentials::SSO_TGC_FIELD` 是同一个字符串，但插件与网关是**两个编译
/// 单元**，网关读不到插件常量。这一份存在的用途只有两个：
///   1. 公开视图给 `hasSsoTgc` 布尔（`plugin_public::credential_fields`）；
///   2. PATCH 入口写/清这个键（`update_codearts_sso_tgc`）。
///
/// ⚠️ 两边**必须同值**：改名只改一处会让「贴了票根却显示未配置」或「清了却还在」。
pub const SSO_TGC_FIELD: &str = "ssoTgc";

/// 票根的长度上限（字符），与插件侧 `MAX_TGC_LENGTH` 同值。
pub const MAX_SSO_TGC_LENGTH: usize = 4096;

impl super::AccountStore {
    /// 写/清 SSO 票根（`ssoTgc`）。语义与 `update_qoder_session_cookie` 一致：
    /// 字符串（归一后非空）= 设置；`null` / 空串 = **清除**。
    ///
    /// ── 这个键为什么也走「独立 PATCH」而不是通用 `apply_patch` ──────
    /// 它是密字段（`plugin_public::is_private_field` 里点名列了），值永不下发；
    /// 走通用补丁会让它出现在公开快照的 diff 里。与余额凭证 / 网页会话 cookie 同一形制。
    ///
    /// ⚠️ 必须在本文件**已有的 `impl` 之外单独开一块**（不能写成顶层自由函数）：
    /// `&self` 只存在于 `impl` 里 —— 顶层 `pub fn f(&self, …)` 的报错是
    /// `E0424: expected value, found module`，看起来像「模块没导入」这种无关方向。
    pub fn update_codearts_sso_tgc(
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
                let tgc = normalize_tgc(&text);
                if tgc.is_empty() {
                    // ── 「全空白输入」与「形状不对」必须走两条路 ─────────────
                    // 空 / 纯空白 = 用户想清除（与 `sessionCookie` 同语义）；
                    // 非空但认不出 = 贴错了东西，必须 400 拒掉。
                    //
                    // ⚠️ 早先这两种情况都落到 `None`（= 清除），后果很坏：
                    // 用户贴了一条门户 cookie（里面没有 `SSOTGC`）想「保存」，
                    // 结果把之前配好的票根**静默删掉**了，界面还回一句
                    // 「SSO 票根已清除」—— 他根本不知道自己刚毁掉了什么。
                    if text.trim().is_empty() {
                        None
                    } else {
                        return Err(AccountStoreError::new(
                            "SSO 票根形状不对：应形如 TGTX--…-sso（约 100 字符，来自 auth.huaweicloud.com 域的 SSOTGC）。\
                             你贴的这串里认不出票根 —— 多半是 codearts.huaweicloud.com 的会话 cookie，那个域没有 SSOTGC。\
                             要清除票根请点「清除」，或提交空值。",
                            400,
                        ));
                    }
                } else {
                    Some(tgc)
                }
            }
        };
        // ── 插件记录：写进 `pluginSecret` ──────────────────────────
        let plugin_secret = {
            let guard = self.guard();
            self.record_by_id(&guard, id)
                .filter(|record| record.provider() == CODEARTS_PROVIDER_ID)
                .filter(StoredAccount::has_plugin_secret)
                .map(|record| record.get("pluginSecret").cloned().unwrap_or(Value::Null))
        };
        if let Some(secret) = plugin_secret {
            let mut next_secret = secret.clone();
            if let Some(object) = next_secret.as_object_mut() {
                match &next {
                    Some(tgc) => {
                        object.insert(SSO_TGC_FIELD.to_string(), Value::String(tgc.clone()));
                    }
                    None => {
                        object.remove(SSO_TGC_FIELD);
                    }
                }
            }
            match self.plugin_update_secret(id, &secret, &next_secret) {
                Ok(true) => {
                    return Ok(vec![match next {
                        Some(_) => "SSO 票根已更新".to_string(),
                        None => "SSO 票根已清除".to_string(),
                    }])
                }
                Ok(false) => return Err(AccountStoreError::new("账号凭证已变化，请刷新后重试", 409)),
                Err(message) => return Err(AccountStoreError::new(message, 500)),
            }
        }
        // ── 存量内置记录：写顶层字段 ────────────────────────────────
        let _guard = self.guard();
        let mut record = self
            .record_by_id(&_guard, id)
            .filter(|record| record.provider() == CODEARTS_PROVIDER_ID)
            .ok_or_else(|| AccountStoreError::not_found("账号不存在或不是 CodeArts 账号"))?;
        let current = record.get(SSO_TGC_FIELD).and_then(Value::as_str).map(str::to_string);
        if current == next {
            return Ok(Vec::new());
        }
        let mut changes = Vec::new();
        match &next {
            Some(tgc) => {
                record.set(SSO_TGC_FIELD, Value::String(tgc.clone()));
                // 日志只说「已更新」，**不打印票根本身**（与全仓的凭证处理一致）
                changes.push("SSO 票根已更新".to_string());
            }
            None => {
                record.remove(SSO_TGC_FIELD);
                changes.push("SSO 票根已清除".to_string());
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
}

/// 票根文本 → 纯值。
///
/// 用户会从三种地方拿（DevTools 的 Request Headers / Application 表格 / Copy as
/// cURL），三种形态都得认。规则与插件侧 `credentials::normalize_tgc` **逐条同口径**
/// —— 两边各归一一次是刻意的：写入侧在网关上，读取侧在插件里，任何一侧单独
/// 退化都会让「贴了却没生效」变得难查。
///
/// 返回空串 = **认不出是票根**（不是「清除」：清除走 `null` / 空输入那条路）。
/// 调用方据此把 `Some("")` 变成 400，绝不能静默落盘成「已配置」。
fn normalize_tgc(raw: &str) -> String {
    let raw = raw.trim();
    if raw.is_empty() {
        return String::new();
    }
    // 多行粘贴先压成一行（与 cookie 同一个理由：换行会让 Cookie 头断在第一处）。
    // `Cookie:` 前缀也剥掉 —— Copy as cURL 给的就是那一行。
    let flat = raw
        .lines()
        .map(str::trim)
        .map(|line| {
            line.strip_prefix("Cookie:")
                .or_else(|| line.strip_prefix("cookie:"))
                .unwrap_or(line)
                .trim()
        })
        .filter(|line| !line.is_empty())
        .collect::<Vec<_>>()
        .join(" ");
    // ① 出现过 `SSOTGC=` → 取它后面到 `;`/空白为止的那一段。
    let candidate = if let Some(index) = flat.find("SSOTGC=") {
        flat[index + "SSOTGC=".len()..]
            .split(|character: char| character == ';' || character.is_whitespace())
            .next()
            .unwrap_or("")
            .trim()
    } else {
        // ② 纯值（可能带粘进来的属性尾巴）。
        flat.split(|character: char| character == ';' || character.is_whitespace())
            .next()
            .unwrap_or("")
            .trim()
    };
    // ③ **形状判据**：认不出就返回空串（见 `looks_like_tgc`）。
    if !looks_like_tgc(candidate) {
        return String::new();
    }
    candidate.chars().take(MAX_SSO_TGC_LENGTH).collect()
}

/// 这一段文本**像不像**一条 `SSOTGC` 票根。
///
/// ── 为什么必须有这条判据（2026-11-09 用户实测踩到）──────────
/// 早先的提示语写着「整条 Cookie 串或只粘这一项都行」，于是用户从
/// `codearts.huaweicloud.com` 复制**整条 cookie 串**贴进票根框 —— 但票根在
/// **`auth.huaweicloud.com`** 域（`Path=/authui/`），是两个不同的域，那条串里
/// 根本没有 `SSOTGC`。旧逻辑把第一个 `k=v` 对
/// （`devclouddevuibjJ_SESSION_ID=…`）当成票根落盘：界面显示「已配置」，
/// 插件拿它换票永远失败，日志里只有一句「换不到会话」—— **用户完全无从判断**。
///
/// 判据取上游的实际形状：以 `TGTX` 开头、以 `-sso` 结尾、长度 ≥ 16
/// （实测 `TGTX--F1840439038-8498999-<64 hex>-sso`，约 100 字符）。
fn looks_like_tgc(value: &str) -> bool {
    let value = value.trim();
    value.len() >= 16 && value.starts_with("TGTX") && value.ends_with("-sso")
}

/// 过期活动的 Token 规则不再迁入；已显式清空的限制器保持不限制。
pub(crate) fn migrate_limiters(accounts: &mut [StoredAccount]) -> usize {
    let mut changed = 0;
    for record in accounts {
        if record.provider() != CODEARTS_PROVIDER_ID { continue; }
        let mut groups = limiter::effective_groups_in(record.fields());
        let mut removed = limiter::effective_rules_in(record.fields()).iter().any(|rule| rule.kind == LimiterKind::Token);
        for group in &mut groups {
            let before = group.conditions.len();
            group.conditions.retain(|condition| !matches!(condition.metric, Metric::Tokens | Metric::Requests));
            removed |= group.conditions.len() != before;
        }
        if !removed { continue; }
        groups.retain(|group| !group.conditions.is_empty());
        let configured_empty = if let Some(groups) = record.get("limiterGroups").and_then(Value::as_array) {
            groups.is_empty()
        } else {
            record.get("limiters").and_then(Value::as_array).is_some_and(Vec::is_empty)
        };
        if groups.is_empty() && !configured_empty {
            let rule = LimiterRule { kind: LimiterKind::Balance, action: limiter::LimiterAction::Skip,
                threshold: crate::server::core::usage_records::DEFAULT_LOW_BALANCE_THRESHOLD,
                period: 0, reset: limiter::LimiterReset::Fixed, enabled: true };
            groups = limiter::groups::groups_from_rules(&[rule]);
        }
        for group in &mut groups {
            if let Some(condition) = group.conditions.first_mut() {
                condition.join = limiter::types::Join::Or;
            }
        }
        let rules = limiter::legacy_rules(&groups);
        record.set("limiterGroups", limiter::types::json_of(&groups));
        record.set("limiters", limiter::rules_to_json(&rules));
        let balance = rules.iter().find(|rule| rule.kind == LimiterKind::Balance);
        record.set("lowBalance", balance.map(|rule| json!({
            "mode": if rule.action == limiter::LimiterAction::Disable { "disable" } else { "skip" },
            "threshold": rule.threshold,
        })).unwrap_or_else(|| json!({ "mode": "off", "threshold": 0 })));
        changed += 1;
    }
    changed
}

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
