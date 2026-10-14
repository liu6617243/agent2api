//! MiniMax 的 `ProviderAdapter` 实现：账号 / token 刷新 / 静态目录 /
//! **聊天转发（Anthropic 通道）** / 余额 / 每日签到。
//!
//! ── 转发的落点 ───────────────────────────────────────────────
//! 推理端点 `POST agent.minimax.cn/mavis/api/v1/llm/v1/messages` 说的是
//! **Anthropic Messages**（实测：只认 `Authorization: Bearer <OAuth token>`，
//! 不认 `x-api-key`）。照 ZCode 活动套餐通道的先例：`ChatRequestPlan` 用
//! `protocol::anthropic_outbound::anthropic_request_from_chat` 构造 Anthropic
//! 请求体、`response: UpstreamResponse::Anthropic` —— 响应帧由编排层的
//! `AnthropicToChatStream` 翻译回 chat SSE，账号轮换 / 限额冷却 / 退避重试 /
//! usage 记账全部留在编排层（`is_stateful` 保持 false）。
//!
//! ── 续期（本家最需要小心的一格）────────────────────────────
//! access_token 约 1 小时过期，refresh_token **一次性轮换**（刷新即吊销旧值）。
//! 因此 `ensure_access_token`（临期懒刷）与 `refresh_access_token`（401 后
//! 强刷）都走 `oauth::ensure_fresh`（单飞 + 比较-再写 + **回写新 refreshToken**）。
//! 这条链不能省：漏了回写，下一次刷新拿旧 rt 必 invalid_grant，账号整条链报废。
//!
//! ── 本家有 / 没有的东西（如实声明）──────────────────────────
//!   - **有余额**（`supports_usage = true`，`GET /credit/details`，见 `balance.rs`）；
//!   - **有签到**（`POST /signin/claim`，幂等，见 `checkin.rs`）；
//!   - **没有远程目录**（`supports_model_refresh = false`：静态 4 项，见 `models.rs`）；
//!   - **没有网页登录**（授权在 MiniMax Code 客户端侧完成（设备码），网关只做
//!     粘贴令牌 + 续期；将来若接设备码登录再开 `supports_web_login`）；
//!   - **不改写模型名**（对外 id = 上游字面值，`sse_model_rewrite` 保持 false）。
//!
//! ── 硬约束 ──────────────────────────────────────────────────
//! release 是 `panic=abort`：本文件零 unwrap/expect/panic。

use axum::http::HeaderMap;
use serde_json::Value;

use crate::server::core::account_store::AccountStore;
use crate::server::core::protocol::anthropic_outbound;
use crate::server::core::providers::adapter::{
    ChatRequestPlan, ModelRefreshOutcome, ProviderAdapter, UpstreamErrorClass, UpstreamResponse,
};
use crate::server::core::providers::{content_block, ProviderKind};
use crate::server::errors::GatewayError;

use super::{credentials, endpoints, models, oauth};

/// MiniMax 适配器（无状态单例；身份全在账号记录里）
pub struct MinimaxAdapter;

/// 静态单例（`adapter_for(ProviderKind::Minimax)` 返回这一个）
pub static MINIMAX_ADAPTER: MinimaxAdapter = MinimaxAdapter;

impl ProviderAdapter for MinimaxAdapter {
    fn kind(&self) -> ProviderKind {
        ProviderKind::Minimax
    }

    /// 模型清单是静态表（上游没有公开的列模型接口，见 `models.rs` 的模块头）
    fn list_models(&self) -> Vec<Value> {
        models::list()
    }

    /// 构造 `POST /mavis/api/v1/llm/v1/messages`（Anthropic Messages）。
    ///
    /// `account` 是会话形态：Bearer 从 `auth.accessToken` 取（编排层在转发前
    /// 已调 `ensure_access_token`，临期令牌会被刷成最新并回写，这里读到的一定
    /// 是续期后的那条）。model 为空时报 400 —— Anthropic 协议必填，而本家
    /// 没有「上游默认模型」可言（与 ZCode 活动套餐通道同一口径）。
    fn build_chat_request(
        &self,
        account: &Value,
        body: &Value,
        _client_headers: &HeaderMap,
    ) -> Result<ChatRequestPlan, GatewayError> {
        let access_token = account
            .pointer("/auth/accessToken")
            .and_then(Value::as_str)
            .map(str::trim)
            .unwrap_or("");
        if access_token.is_empty() {
            return Err(GatewayError::with_status(
                401,
                "MiniMax 账号缺少 access token，无法转发（请刷新令牌或重新粘贴 refreshToken）",
            ));
        }
        let requested = body
            .get("model")
            .and_then(Value::as_str)
            .map(str::trim)
            .unwrap_or("");
        if requested.is_empty() {
            return Err(GatewayError::with_status(
                400,
                "MiniMax 请求缺少 model 字段：请在客户端指定模型（如 MiniMax-M3）",
            ));
        }
        let wire_model = models::upstream_model_id(requested);
        // chat 体 → Anthropic Messages 体（system 折顶、tool 消息拆块、
        // reasoning_effort 折 thinking 预算，全部复用共用转换层）
        let payload = anthropic_outbound::anthropic_request_from_chat(body, &wire_model)
            .map_err(|message| {
                GatewayError::with_status(400, format!("MiniMax 请求体转换失败：{message}"))
            })?;
        let headers: Vec<(String, String)> = vec![
            ("Content-Type".to_string(), "application/json".to_string()),
            ("Authorization".to_string(), format!("Bearer {access_token}")),
            ("anthropic-version".to_string(), endpoints::ANTHROPIC_VERSION.to_string()),
            ("Accept".to_string(), "text/event-stream".to_string()),
        ];
        Ok(ChatRequestPlan {
            url: endpoints::messages_url(),
            headers,
            body: payload,
            // 上游说 Anthropic：响应帧要先折成 chat SSE 再下发（upstream::translate）
            response: UpstreamResponse::Anthropic,
        })
    }

    /// 上游错误分类：
    ///   - 401 / 403 → `TokenExpired`（编排层先强刷一次再重试/换号）；
    ///   - 429 → `QuotaLimited`（不给结构化恢复时间，走兜底冷却）；
    ///   - 400 → 内容拦截判定或原样；其余 → `Fatal`。
    fn classify_error(&self, status: u16, error_body: &Value) -> UpstreamErrorClass {
        let raw = error_body
            .pointer("/error/message")
            .and_then(Value::as_str)
            .or_else(|| error_body.get("message").and_then(Value::as_str))
            .or_else(|| error_body.pointer("/base_resp/status_msg").and_then(Value::as_str))
            .map(str::trim)
            .filter(|text| !text.is_empty())
            .unwrap_or("上游错误");
        let message = format!("上游返回 {status}: {raw}");
        let code = error_body
            .get("code")
            .and_then(Value::as_i64)
            .or_else(|| error_body.pointer("/base_resp/status_code").and_then(Value::as_i64));
        if status == 401 || status == 403 {
            return UpstreamErrorClass::TokenExpired { message };
        }
        if status == 429 {
            return UpstreamErrorClass::QuotaLimited {
                reset_at: None,
                message,
                upstream_code: code,
                status,
            };
        }
        if status == 400 {
            return content_block::classify_or_fatal(status, error_body, message, code);
        }
        UpstreamErrorClass::Fatal {
            status,
            message,
            upstream_code: code,
        }
    }

    /// 取可用 access token：临期（提前 15 分钟）或没有 token 时刷新并回写
    /// （单飞 + 比较-再写链在 `oauth::ensure_fresh`，新 refreshToken 一起落盘）。
    fn ensure_access_token<'a>(
        &'a self,
        store: &'a AccountStore,
        account_id: &'a str,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<String, GatewayError>> + Send + 'a>>
    {
        Box::pin(async move {
            let credentials = oauth::ensure_fresh(store, account_id, false).await?;
            if credentials.access_token.trim().is_empty() {
                return Err(GatewayError::with_status(
                    401,
                    "MiniMax 账号没有可用的 access token（刷新未返回令牌），请重新粘贴 refreshToken",
                ));
            }
            Ok(credentials.access_token)
        })
    }

    /// 401 后的**强制**刷新：无视临期判定直接刷一次（一次性轮换语义下，
    /// 被拒的那枚 rt 已作废，必须拿记录里最新的重打）。
    fn refresh_access_token<'a>(
        &'a self,
        store: &'a AccountStore,
        account_id: &'a str,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<String, GatewayError>> + Send + 'a>>
    {
        Box::pin(async move {
            let credentials = oauth::ensure_fresh(store, account_id, true).await?;
            Ok(credentials.access_token)
        })
    }

    /// 本家有续期手段（refreshToken + OAuth token 端点）——维护任务要问本家。
    fn supports_refresh(&self) -> bool {
        true
    }

    /// 后台凭证维护问「这条账号是不是快到期了」（判据与刷新用同一把尺）。
    /// 没有 refreshToken 的账号返回 false：它连续期手段都没有。
    fn credentials_expiring(&self, store: &AccountStore, account_id: &str) -> bool {
        if account_id.trim().is_empty() {
            return false;
        }
        let Some(record) = store.minimax_account_record(account_id) else {
            return false;
        };
        match credentials::from_record(Some(&record)) {
            Ok(credentials) if credentials.can_refresh() => credentials.expiring(),
            _ => false,
        }
    }

    /// 静态清单，没有远程目录可刷（`supports_model_refresh` 保持默认 false，
    /// 刷新循环不会调度到本家）；trait 的 `refresh_models` 是**必选方法**
    /// （无默认实现），照 ZCode 静态目录的先例返回 `unchanged()` —— 用户真
    /// 点了「刷新模型清单」也不会假报「已刷新」。
    fn refresh_models<'a>(
        &'a self,
        _store: &'a AccountStore,
        _account_id: &'a str,
        _force: bool,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = ModelRefreshOutcome> + Send + 'a>,
    > {
        Box::pin(async move { ModelRefreshOutcome::unchanged() })
    }

    /// 有积分余额（`GET /minimax-cloud/api/v1/credit/details`，见 `balance.rs`）
    fn supports_usage(&self) -> bool {
        true
    }

    /// 查余额（临期令牌会先刷一次，见 `balance::query_usage`）
    fn query_usage<'a>(
        &'a self,
        store: &'a AccountStore,
        account_id: &'a str,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<Value, GatewayError>> + Send + 'a>>
    {
        Box::pin(async move { balance_query(store, account_id).await })
    }
}

/// 余额查询入口（单独成函数：上面闭包里的 future 生命周期更短更好检查）
async fn balance_query(store: &AccountStore, account_id: &str) -> Result<Value, GatewayError> {
    super::balance::query_usage(store, account_id).await
}
