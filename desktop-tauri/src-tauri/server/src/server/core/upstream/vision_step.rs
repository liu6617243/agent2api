//! 识图的**执行**：把请求体里的图片块换成一段文字描述。
//!
//! ── 为什么这一段单独一个文件 ────────────────────────────────
//! 它是「转发入口里的一步」，但它自己要发一次完整的上游请求（走选路、换号、
//! 重试、限流全套），代码量与 `mod.rs` 里那些编排职责不同类。单文件行数约定
//! 之下拆出来，也让「识图做了什么」在这一处读完，不必在 800 行的转发入口里找。
//!
//! ── 为什么是「向自己再发一次」而不是「直接调 provider 适配器」────
//! 参考实现 magpie 的 `askVision()` 就是 POST 自己的 `/v1/chat/completions`。
//! 理由是复用：识图请求要享受与正常请求**完全一样**的选路（账号优先级、限额
//! 冷却、粘性、429 顺延）、代理、重试与协议适配。绕过转发层自己拼一个请求，
//! 等于把这套容错再实现一遍 —— 而两套实现迟早会分叉，分叉的表现是「正常请求
//! 能过、识图请求卡在某个只有识图才走的分支上」。
//!
//! 代价是一次进程内自调用（多一次路由计算与一次 body 序列化，都是纯内存），
//! 相对一次真实的上游往返可忽略。
//!
//! ── 递归怎么断（两处，缺一不可）─────────────────────────────
//!   1. **运行时**：子请求带 `skip_vision: true`（见
//!      [`super::ForwardRequest::skip_vision`]），进转发入口后不再走这一步。
//!      这是唯一的功能性出口 —— 丢了就是无限自调用（烧栈 + 烧额度）。
//!   2. **类型层**：[`UpstreamService::describe_images`] 刻意**不是** `async fn`，
//!      而是返回 `Pin<Box<dyn Future>>` 的普通函数。因为调用链是
//!      `forward` → `describe_images` → `forward`：若两个都是 `async fn`，它们
//!      的返回类型（`impl Future` 不透明类型）会互相引用、无限展开，编译器报
//!      E0733「recursion in an async fn requires boxing」。把 `describe_images`
//!      的返回类型**显式擦成 `dyn`** 之后，`forward` 的类型不再依赖自己，
//!      环就断在这里。
//!
//!      注意：光在调用点写 `Box::pin(self.forward(..))` **不够** —— `Box::pin`
//!      只是把「无限大的值」变成「指向它的指针」，而构造那个指针仍然要先知道
//!      无限大的类型。断点必须落在**函数签名**上。

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use serde_json::Value;

use crate::server::core::key_scope::KeyScope;
use crate::server::core::vision;
use crate::server::logging;

use super::usage::RequestTelemetry;
use super::{ForwardOutcome, ForwardRequest, UpstreamService};

/// 识图子请求的超时。
///
/// 描述一张密集截图在慢视觉模型上可能十几秒，但**不能让主请求无限期挂着**：
/// 超时就放弃这一步、把原图留着往下发（与「描述失败」同一处置）。60 秒是
/// 「最慢的视觉模型也够」与「用户还能忍受多等一段」之间的取值。
const VISION_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);

impl UpstreamService {
    /// 把 `body` 里的图片块替换成文字描述（原地改写）。
    ///
    /// 返回 `Some(块数)` 表示确实替换了；`None` 表示**这一步没生效**（无图、
    /// 功能关闭、没有可用识图模型、描述失败或超时）。四种 `None` 的处置完全
    /// 相同：body 保持原样，主请求照常往下走。
    ///
    /// ── 签名为什么不是 `async fn` ──────────────────────────────
    /// 见模块头第 2 条：这里是递归链的断点，返回类型必须显式擦成 `dyn`。
    /// 调用点照常 `.await`，用起来与 `async fn` 无异。
    ///
    /// ── 失败为什么保留原图而不是剥掉 ────────────────────────────
    /// 剥掉图会让主模型对着一段空占位凭空回答，用户拿到一个看起来正常、实则
    /// 毫无依据的回复 —— 比报错更坏。保留原图则至少有一条诚实的失败路径
    /// （纯文本上游会明确说它不收图）。
    pub(super) fn describe_images<'a>(
        &'a self,
        body: &'a mut Value,
    ) -> Pin<Box<dyn Future<Output = Option<usize>> + Send + 'a>> {
        Box::pin(async move {
            // ① 有没有图（绝大多数请求在这里就返回，一次递归扫描）
            let images = vision::image_blocks(body);
            if images.is_empty() {
                return None;
            }
            // ② 该用哪个识图模型（配置驱动的解析，带 TTL 缓存）
            let Some(target) = vision::resolve(&self.store) else {
                // 功能关着，或者一个声明收图的模型都没有 —— 静默跳过：这是用户
                // 明确选择的形态（`off`）或客观事实（没配视觉账号），不必每请求
                // 留一行日志
                return None;
            };
            // ③ 缓存（同一张图在同一会话里常常反复出现）
            let sources = vision::image_sources(body);
            let key = vision::cache_key(&target, &sources);
            if let Some(description) = vision::cached(&key) {
                logging::verbose(
                    "[Vision]",
                    &format!(
                        "命中描述缓存 {}（{} 张图）",
                        vision::short_hash(&key),
                        images.len(),
                    ),
                );
                return Some(vision::rewrite(body, &description));
            }
            // ④ 真发一次识图请求
            logging::log(
                "[Vision]",
                &format!(
                    "识图：{} 张图片交给 {}（{}）",
                    images.len(),
                    target.model,
                    target.provider.as_deref().unwrap_or("自动选路"),
                ),
            );
            let request = vision::describe_request(&target, &images);
            let Some(description) = self.ask_vision(&target, request).await else {
                logging::log(
                    "[Vision]",
                    &format!(
                        "⚠️  识图未取得描述（模型 {}），本次请求保留图片原样转发",
                        target.model,
                    ),
                );
                return None;
            };
            // ⑤ 先缓存再替换：**只缓存成功**的描述（失败文案进缓存会让用户
            // 重试时拿到同一句失败，见 `vision::store` 的说明）
            vision::store(key, description.clone());
            Some(vision::rewrite(body, &description))
        })
    }

    /// 发一次识图请求，拿回描述正文（失败 / 超时给 `None`）。
    ///
    /// 候选链刻意**钉住**解析出的那一家（`KeyScope::provider_only`）：识图要
    /// 可预测，不能让「自动选路」把描述交给另一家看着像但不收图的模型。这也
    /// 正是「轮询账号池找视觉模型」与「指定固定的供应商模型」之间的选择 ——
    /// 答案是后者（理由见 `core::vision` 模块头）。
    async fn ask_vision(&self, target: &vision::Target, body: Value) -> Option<String> {
        let scope = target.provider.as_deref().map(KeyScope::provider_only);
        let request = ForwardRequest {
            body,
            // 要一个聚合好的完整结果：描述是一段文本，逐帧拼 SSE 只是把聚合
            // 逻辑再写一遍（转发层会按自己的规则把上游强制成流式再聚合回来）
            stream: false,
            // 不去重：同名同图的并发请求由描述缓存收敛；让它参与「同 body 排队
            // 45 秒」只会把无关请求串起来
            dedupe_key: String::new(),
            // 不带客户端头：识图是网关自己发起的请求，透传客户端的会话类头
            // （`x-session-id` 之类）会让上游把它当成同一条会话的续轮
            client_headers: Default::default(),
            // 独立的遥测槽：**不能**用主请求那份 —— 子请求的承载家、上游模型、
            // 尝试明细会覆盖掉主请求的事实，请求日志里就再也看不出「这次到底
            // 是谁回答了用户」
            telemetry: Arc::new(RequestTelemetry::new()),
            allowed_providers: scope,
            // 钉账号与「跳过模型门禁」都不传：识图模型必须在目录里且已启用，
            // 解析时已校验过；这里按生产语义正常选路
            pinned_account: None,
            ignore_model_gate: false,
            // 代理跟随账号配置（与主请求一致：识图不该走另一条网络路径）
            proxy_override: None,
            // ★ 运行时递归出口：子请求不再走识图这一步
            skip_vision: true,
        };
        let outcome = match tokio::time::timeout(VISION_TIMEOUT, self.forward(request)).await {
            Ok(result) => result,
            Err(_) => {
                logging::log(
                    "[Vision]",
                    &format!(
                        "⚠️  识图请求超时（{} 秒），本次保留图片原样转发",
                        VISION_TIMEOUT.as_secs(),
                    ),
                );
                return None;
            }
        };
        let body = match outcome {
            Ok(ForwardOutcome::Completion { body }) => body,
            // 子请求恒为 `stream:false`，走到流式分支说明转发层的行为变了
            // —— 不 panic（release 是 abort），当失败处理
            Ok(ForwardOutcome::Stream { .. }) => {
                logging::log("[Vision]", "⚠️  识图请求意外返回流式响应，按失败处理");
                return None;
            }
            Err(error) => {
                logging::log("[Vision]", &format!("⚠️  识图请求失败：{error}"));
                return None;
            }
        };
        vision::description_of(&body)
    }
}
