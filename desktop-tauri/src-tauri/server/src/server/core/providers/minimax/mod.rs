//! MiniMax（MiniMax Code / agent.minimax.cn）适配实现：账号 / token 刷新 /
//! 静态模型目录 / **聊天转发（Anthropic 通道）** / 积分余额 / 每日签到。
//!
//! ── 上游长什么样（实测定案；参考实现 minimax_shim.py + mm_checkin.py）──
//! MiniMax 是「一条账号体系 + 两个域」：
//!
//! ```text
//!   账号（OAuth）  POST https://account.minimax.cn/oauth2/token     （form 表单）
//!   推理 + 管理    https://agent.minimax.cn                          （Bearer access_token）
//!     推理         POST /mavis/api/v1/llm/v1/messages               （Anthropic Messages）
//!     余额         GET  /minimax-cloud/api/v1/credit/details        （Σ remaining_amount）
//!     签到         GET  .../signin/status + POST .../signin/claim   （幂等）
//! ```
//! 推理端点实测**只认 `Authorization: Bearer`**（不认 `x-api-key`），响应是
//! Anthropic Messages SSE —— 因此转发与 ZCode 活动套餐通道同一处置：
//! `ChatRequestPlan { response: UpstreamResponse::Anthropic }`，响应帧由编排层
//! `upstream::translate::AnthropicToChatStream` 折回 chat SSE（账号轮换 /
//! 限额冷却 / 退避重试 / usage 记账全部留在编排层，`is_stateful` 保持 false）。
//!
//! ── 为什么建模与别家不同（两处最容易踩的坑）────────────────────
//!   1. **refresh_token 一次性轮换**：每刷一次下发新值、旧值当场吊销（与
//!      Google 的长寿命 refresh_token 相反）。刷新成功必须**同时**回写新
//!      access_token 与新 refresh_token（`oauth::refresh_and_save` →
//!      `update_minimax_credentials_if_current`），漏掉任何一半，下一次刷新
//!      拿旧 rt 去打必然 invalid_grant，账号整条续期链报废。这条纪律也让
//!      「单飞」比别家更关键：两个并发刷新各拿旧 rt 去打，第二个必失败。
//!   2. **业务码在 200 里**：管理接口（余额/签到）用 HTTP 200 +
//!      `{base_resp:{status_code,status_msg}}` 表达业务结果，`status_code != 0`
//!      即业务错误——只看 HTTP 码会把失败当成功（见 `client::ensure_business_ok`）。
//!
//! ── 凭证形态 ──────────────────────────────────────────────────
//! `refreshToken` 是主凭证（丢了只能回 MiniMax Code 客户端重新走设备码授权），
//! `accessToken` 只是约 3600 秒的短期缓存。授权在客户端侧完成，网关只做
//! **粘贴令牌 + 续期**（没有网页登录链路；将来接设备码登录再开 `supports_web_login`）。
//!
//! ── 交付边界 ──────────────────────────────────────────────────
//! ```text
//!   ✅ 账号：粘贴 refreshToken 添加（account_store::minimax_accounts）
//!   ✅ token 刷新：临期懒刷 + 401 强刷（单飞 + 比较再写 + 新 rt 必回写）
//!   ✅ 模型目录：静态 4 项（上游无公开列模型接口；supports_model_refresh=false）
//!   ✅ 聊天转发：build_chat_request → Anthropic Messages（Bearer + anthropic-version）
//!   ✅ 积分余额：supports_usage=true（credit/details → {available, unit, wallets, raw}）
//!   ✅ 每日签到：billing/checkin 分派 + auto_checkin 台账（claim 幂等）
//!   ✅ 注册接线：ProviderKind / 注册表 / adapter_for / 目录缓存 / 账号存储 / 添加分支
//! ```
//!
//! ── 子模块分工 ──────────────────────────────────────────────
//! ```text
//!   endpoints.rs    两个基址 / 各端点路径 / OAuth 公开参数 / anthropic-version
//!   credentials.rs  凭证快照 + 临期判定（refreshToken 主、一次性轮换语义）
//!   oauth.rs        token 端点 form 刷新 + 单飞 + 比较再写（新 rt 必回写）
//!   models.rs       静态目录（4 项原样透传，is_known 判大小写不敏感）
//!   client.rs       agent 网关 Bearer 请求薄封装 + base_resp 业务码口径
//!   balance.rs      积分余额（Σ details[].remaining_amount，临期先刷）
//!   checkin.rs      每日签到（status 面板 + claim 领取 → {success, msg}）
//!   adapter.rs      ProviderAdapter 实现（账号 / 刷新 / 目录 / 转发 / 余额）
//! ```
//!
//! ── 硬约束 ──────────────────────────────────────────────────
//! release 是 `panic=abort`：本目录零 unwrap/expect/panic；日志绝不打印令牌本体。

pub mod adapter;
pub mod balance;
pub mod checkin;
pub mod client;
pub mod credentials;
pub mod endpoints;
pub mod models;
pub mod oauth;

pub use adapter::{MinimaxAdapter, MINIMAX_ADAPTER};

use std::sync::Arc;

use crate::server::core::egress;
use crate::server::core::proxies::ResolvedProxy;

/// 本家出网 Client：**显式配了代理才走代理，否则直连**（仓内默认口径）。
///
/// 与 Antigravity 那一格相反：本家的上游（`account.minimax.cn` /
/// `agent.minimax.cn`）全在国内，直连可达；跟随系统代理反而会把国内请求
/// 塞进用户为国外站点配的代理里（Loomy 等国内上游同一口径，见
/// `egress::client_for` 的说明）。
pub(crate) fn client_for(proxy: Option<&ResolvedProxy>) -> Arc<reqwest::Client> {
    egress::client_for(proxy)
}
