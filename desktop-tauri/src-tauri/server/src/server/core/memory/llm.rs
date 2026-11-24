//! 记忆中枢的内部 LLM 通道。
//!
//! ── 为什么复用网关自己的 `/v1/chat/completions` ──────────────
//! 记忆中枢要做的事（抽取要点、生成摘要、打标签、判定事实是否失效、写画像）
//! 本质都是「一次普通的对话补全」。这条链路上网关已经解决了协议转换、
//! 提供商轮换、429 退避、故障转移、用量记账、请求日志。另写一套供应商客户端
//! 等于把这些问题再解一遍，而且必然与网关的选择逻辑分叉 —— 用户在「提供商」
//! 页调好的顺序，记忆任务不该看到另一套。
//!
//! 同一取向的先例是视觉分析（`tool_services::vision::run_vision`）：用内部 Key
//! 打本进程网关，拿回来的就是已经算过账的那次调用。
//!
//! ── 模型怎么选 ──────────────────────────────────────────────
//! 三级回退，越靠前越具体：
//!   1. `models.routing` 里 `task` 命中本任务的条目，取它的 `model`；
//!   2. 按 [`task_tag`] 给的固定档位（`light` / `heavy`）在 `models.models`
//!      里找带同一标签的模型，命中即用；
//!   3. 都没有 → `models.fallbackModel`；再没有 → 网关目录里第一个非图像模型。
//!
//! ── 思考强度为什么不在配置里（2026-11-24）────────────────────
//! 早先这里有一个 `models.taskEffort` 旋钮，默认值里 `summarize`/`tag` 是
//! `low`/`minimal`，其余是 `medium`/`high`。它把两个不该混在一起的东西绑成了
//! 一个字段：**"这个任务值不值得用更强的模型"**（选模型）和 **"要让上游多想"**
//! （`reasoning_effort`）。
//!
//! 后一半是纯粹的破坏性旋钮：`max_tokens` 是 reason + content 的**共享预算**，
//! 机械任务（打标签、写摘要、抽条目）本来只需要几百 token 的结论，一旦被设成
//! `high`，上游会先把 1600 全花在思考上、`content` 返回空串、`finish_reason`
//! 报 `length`。真机台账（`llm_call`）里 `summarize` 78 次失败 / 707k token、
//! `tag` 25 次失败 / 134k token，全是这个形状。
//!
//! 而它错得**不可自查**：界面上写着"思考强度：高"，用户会理解成"更认真"，
//! 没有任何线索指向"这是失败的原因"。错误信息里那句"调大 `models.taskMaxTokens`"
//! 也只是绕道 —— 真正的因果在上游把预算吃完了。
//!
//! 所以这个字段**整体不再由记忆中枢发送**：记忆任务的输入是"给定素材、输出
//! 结构化 JSON"，要的是稳定而不是深度推理；需要推理的模型自己会推理，不需要
//! 我们替它加码。选模型那一半（`light` / `heavy`）继续保留，但改成代码里的
//! 固定档位 [`task_tag`]，不再是配置项。
//!
//! `models.taskEffort` 这个键仍然留在 schema 里（跨实现配置契约，删键会让老
//! 配置静默丢值），但标成 `wired=false`：改了没有任何行为影响。
//!
//! 为什么第 3 步不是「写死一个模型名」：这个仓库要能装在任何人的机器上，
//! 写死必然在别人的账号里不存在。目录里有什么就用什么，才叫「本地可用」。
//!
//! ── 没有可用模型时怎么办 ────────────────────────────────────
//! 返回 [`LlmError::Unavailable`]，调用方把任务挂起而不是失败退出：记忆的写入
//! 与检索**绝不能**因为「今天没配模型」而停摆 —— 那些是零 token 的本地操作。
//! 只有真正需要 LLM 的增强项（摘要、标签、画像、失效判定）才依赖它。

use std::sync::OnceLock;
use std::time::Duration;

use serde_json::{json, Value};

use super::config;
use super::index;
use super::rt;

/// 一次补全的入参。
pub struct Request {
    /// 任务名（`extract` / `summarize` / `tag` / `classify` / `distill` / `profile` /
    /// `supersede` / `dedup` / `consolidate`）。既是选模型的键，也是台账的维度。
    pub task: String,
    pub system: String,
    pub user: String,
    /// 覆盖模型（空 = 按任务选）
    pub model: String,
    pub max_tokens: u32,
    pub temperature: f32,
}

impl Request {
    pub fn new(task: &str, system: impl Into<String>, user: impl Into<String>) -> Self {
        Self {
            task: task.to_string(),
            system: system.into(),
            user: user.into(),
            model: String::new(),
            max_tokens: max_tokens_for(task),
            temperature: 0.2,
        }
    }

    /// 按"这一批有几条"抬高输出预算。
    ///
    /// ── 为什么预算要跟着条数走，而不是一个固定数字 ────────────
    /// 同一次调用可能只处理 3 条，也可能处理 20 条（`auto.tasks.*.batchSize`
    /// 上限就是 200）。固定 1600 在 20 条时必然截断 —— 而截断的后果是**整批
    /// 丢弃**，20 条的 token 全白花。
    ///
    /// 早先这里只有一张固定表，用户撞上截断之后唯一的出口是去调
    /// `models.taskMaxTokens`：一个"我该填多少"完全靠猜的内部旋钮。猜小了
    /// 继续截断，猜大了每条小任务都按大任务预留额度。让代码按条数算，
    /// 用户就不用参与这个算术。
    ///
    /// ── 为什么尊重 `models.taskMaxTokens` 里显式写了的值 ──────────
    /// 显式写一个更小的值通常是有意的（省额度、或上游对 `max_tokens` 有上限）。
    /// 按条数一抬，等于把用户的意图盖掉，而且他看不出来是被谁改的。
    /// 所以只在"这个任务没被显式配置过"时才自动抬高。
    ///
    /// ── 判"显式"必须走 [`config::explicit`]，不能走 `object_of` ────
    /// `object_of` 在没配时回落到 schema 默认值，而 `models.taskMaxTokens` 的
    /// 默认值里**就有** `tag` / `summarize` 这些键 —— 用它判断等于"永远都算
    /// 显式配过"，自动抬高成了死代码。这种"读到了但读错了层"的 bug 不会报错，
    /// 只会让功能静默失效。
    pub fn for_items(mut self, items: usize, per_item: u32) -> Self {
        let configured = config::explicit()
            .get("models.taskMaxTokens")
            .and_then(Value::as_object)
            .cloned()
            .unwrap_or_default();
        if configured.contains_key(&self.task) {
            return self;
        }
        let needed = per_item.saturating_mul(items.min(200) as u32);
        if needed > self.max_tokens {
            self.max_tokens = needed.min(32_000);
        }
        self
    }
}

/// 任务对应的模型池标签。**代码里固定，不是配置项。**
///
/// ── 为什么固定 ──────────────────────────────────────────────
/// 只有两个任务值得往"更强的模型"上靠：
///   · `profile` —— 跨条目归纳人格与技术栈，需要长上下文与抽象能力；
///   · `distill` —— 把一堆碎片压成架构决策，需要取舍判断。
///
/// 其余任务（打标签、写摘要、抽条目、失效判定、合并）都是**结构化抽取**：
/// 输入已经给全，输出是短 JSON，模型再强也不会让结论更好，只会让 `max_tokens`
/// 更快被吃掉。给它们开"高质量模型"是纯浪费。
///
/// 这个判断不需要用户参与 —— 用户既不知道每个任务的提示词长什么样，也无法从
/// 界面上判断"该不该给它 heavy"。早先它是个下拉框，结果真机被设成
/// `{"summarize":"high","tag":"high",…}`：机械任务全走 heavy，`reasoning_effort`
/// 把输出预算吃干，`content` 空、`finish_reason=length`。
pub fn task_tag(task: &str) -> Option<&'static str> {
    match task {
        "profile" => Some("heavy"),
        "distill" => Some("light"),
        _ => None,
    }
}

/// 每个任务默认要多少输出 token。
///
/// ── 为什么不能所有任务共用一个 1200 ──────────────────────────
/// 早先这里是写死的 `1200`。而 `distill` 要一次产出 3-8 条、每条正文 100-300 字
/// 的 JSON，`profile` 要四类各 1-4 条 —— 这些在 1200 token 上必然被截断，截断后
/// 的 JSON 解析失败，表现成一句「模型返回空内容」。用户看到的是「深度记忆 0 条」，
/// 而真正的原因是预算不够。
///
/// 1200 对 `tag` / `summarize` 是够的（一次十来条短句），所以不能简单地把全局值
/// 调大 —— 那会让每一条小任务都按大任务的额度去预留。
///
/// ── `extract` 为什么从 1200 提到 2400（2026-11-22）─────────────
/// 真机 `task_run` 里 `extract` 最近 12 次有 7 次失败，其中 4 次是
/// `max_tokens=1200` 截断、3 次是截断后 JSON 不合法。而它的输出还要再长一截：
/// 现在每条要么是 `action/target/note`，要么是 `title/type/body/importance`，
/// 结构比原来那版多两个字段。不抬预算的话，「减少重复」换来的是一半解析失败。
///
/// 可按 `models.taskMaxTokens` 覆盖（`{"distill": 4000}`），未列的走这张表。
fn max_tokens_for(task: &str) -> u32 {
    let configured = config::object_of("models.taskMaxTokens");
    if let Some(value) = configured.get(task).and_then(Value::as_u64) {
        if value >= 128 {
            return value.min(32_000) as u32;
        }
    }
    match task {
        // 最多 3 条，每条要么带 action/target/note，要么带 title/type/body/importance；
        // 正文 100-300 字，JSON 骨架本身也要占掉几百 token
        "extract" => 2400,
        // 一次产出 3-8 条结论，每条含标题 + 100-300 字正文 + 证据 id
        "distill" => 3000,
        // 四类画像各 1-4 条，每条含标题 + 一句话 + 证据 id
        "profile" => 3000,
        "consolidate" => 3000,
        // 一次给 10-20 条打标签 / 写摘要，输出比 distill 短但仍不止 1200
        "tag" => 1600,
        "summarize" => 1600,
        // 只回答「哪几条被取代」，输出很短
        "supersede" => 800,
        _ => 1200,
    }
}

/// 一次补全的出参。
pub struct Reply {
    pub text: String,
    pub model: String,
    pub prompt_tokens: i64,
    pub completion_tokens: i64,
}

#[derive(Debug)]
pub enum LlmError {
    /// 没配模型 / 没可用 Key / 记忆功能关着 —— 调用方应当挂起任务，不是报错
    Unavailable(String),
    /// 真跑了但失败（网络、上游 4xx/5xx、解析不了）
    Failed(String),
}

impl LlmError {
    pub fn message(&self) -> &str {
        match self {
            Self::Unavailable(message) | Self::Failed(message) => message,
        }
    }

    pub fn is_unavailable(&self) -> bool {
        matches!(self, Self::Unavailable(_))
    }
}

/// 网关基址。
///
/// 优先 `models.gatewayBase`（用户显式指定），其次环境变量推出来的端口，最后
/// 回退 3065。为什么不从 HTTP 请求头拿：需要调 LLM 的路径大部分不在请求上下文
/// 里（后台调度器、MCP 的单写者线程），拿不到 `Host`。这些路径与网关同进程，
/// 回环地址永远是对的。
pub fn gateway_base() -> String {
    static CACHE: OnceLock<String> = OnceLock::new();
    CACHE
        .get_or_init(|| {
            let configured = config::text_of("models.gatewayBase");
            if !configured.trim().is_empty() {
                return configured.trim().trim_end_matches('/').to_string();
            }
            let port = std::env::var("AGENT2API_PROXY_PORT")
                .or_else(|_| std::env::var("WORKBUDDY_PROXY_PORT"))
                .ok()
                .and_then(|raw| raw.trim().parse::<u16>().ok())
                .unwrap_or(3065);
            format!("http://127.0.0.1:{port}")
        })
        .clone()
}

/// 内部调用用的 Key：优先「可用且有模型白名单」的那把，否则第一把可用 Key。
///
/// 为什么偏好有白名单的：这类 Key 通常是用户专门给工具服务建的，权限被有意
/// 收窄；拿它调记忆任务不会意外用到全权限主 Key。没有就退回第一把可用的 ——
/// 单 Key 用户的机器上只有那一把。
fn internal_key() -> Option<String> {
    let keys = crate::server::core::api_keys::list();
    let usable = || keys.iter().filter(|entry| entry.is_usable());
    usable()
        .find(|entry| !entry.allowed_models.is_empty())
        .or_else(|| usable().next())
        .map(|entry| entry.key.clone())
        .or_else(|| crate::server::config::parse::env_api_key_value())
}

/// 网关目录里第一个可用的对话模型（跳过图像/向量类）。
fn first_catalog_model() -> Option<String> {
    let store = crate::server::core::account_store::plugin_accounts::shared()?;
    let catalog = crate::server::core::providers::catalog::models_response(&store, None);    catalog
        .get("data")
        .and_then(Value::as_array)?
        .iter()
        .find(|item| {
            let kind = item.get("kind").and_then(Value::as_str).unwrap_or("chat");
            let id = item.get("id").and_then(Value::as_str).unwrap_or("");
            !id.is_empty() && kind != "image" && kind != "embedding"
        })
        .and_then(|item| item.get("id").and_then(Value::as_str).map(str::to_string))
}

/// 按任务选模型。
pub fn model_for(task: &str) -> Option<String> {
    // 1. 显式路由：`[{task, tags, model}]`
    for entry in config::get("models.routing").and_then(|value| value.as_array().cloned()).unwrap_or_default() {
        if entry.get("task").and_then(Value::as_str) != Some(task) {
            continue;
        }
        if let Some(model) = entry.get("model").and_then(Value::as_str) {
            if !model.trim().is_empty() {
                return Some(model.trim().to_string());
            }
        }
    }
    // 2. 任务固定的模型池标签（不是配置项，见 `task_tag`）
    if let Some(tag) = task_tag(task) {
        let tagged = config::get("models.models").and_then(|value| value.as_array().cloned()).unwrap_or_default();
        for item in tagged {
            let has_tag = item
                .get("tags")
                .and_then(Value::as_array)
                .is_some_and(|tags| tags.iter().any(|value| value.as_str() == Some(tag)));
            if has_tag {
                if let Some(model) = item.get("model").and_then(Value::as_str) {
                    if !model.trim().is_empty() {
                        return Some(model.trim().to_string());
                    }
                }
            }
        }
    }
    // 3. 用户指定的兜底，再不行就问目录
    let fallback = config::text_of("models.fallbackModel");
    if !fallback.trim().is_empty() {
        return Some(fallback.trim().to_string());
    }
    first_catalog_model()
}

/// 当日预算是否已经用完。
///
/// `auto.dailyTokenLimit = 0` 表示不限。`auto.overBudgetAction = pause` 时超预算
/// 直接挂起；`ignore` 时照跑（只是台账会继续涨，供用户自己看）。
pub fn budget_exhausted() -> bool {
    let limit = config::number("auto.dailyTokenLimit") as i64;
    if limit <= 0 {
        return false;
    }
    if config::text_of("auto.overBudgetAction") == "ignore" {
        return false;
    }
    index::global().tokens_today() >= limit
}

/// 跑一次补全。
///
/// 同步函数（内部用 `blocking::in_place` 等异步结果）：调用方几乎都是
/// `spawn_blocking` 里的同步代码（写记忆、MCP 分发、调度器），在这层再开
/// 一个运行时没有意义，而且会在单线程运行时上 panic。
pub fn complete(request: Request) -> Result<Reply, LlmError> {
    if !config::bool_of(config::ENABLE_KEY) {
        return Err(LlmError::Unavailable("记忆中枢已关闭".into()));
    }
    if budget_exhausted() {
        return Err(LlmError::Unavailable("今日 token 预算已用完（auto.dailyTokenLimit）".into()));
    }
    let model = if request.model.trim().is_empty() {
        model_for(&request.task).ok_or_else(|| {
            LlmError::Unavailable("没有可用模型：模型目录为空，也没配 models.fallbackModel".into())
        })?
    } else {
        request.model.trim().to_string()
    };
    let key = internal_key().ok_or_else(|| LlmError::Unavailable("没有可用的网关 Key".into()))?;

    let body = json!({
        "model": model,
        "messages": [
            { "role": "system", "content": request.system },
            { "role": "user", "content": request.user },
        ],
        "stream": false,
        "max_tokens": request.max_tokens,
        "temperature": request.temperature,
    });
    // ── 不写 `reasoning_effort`（2026-11-24）──────────────────────
    // `max_tokens` 是 reason + content 的**共享预算**。上游一旦被要求多想，
    // 会先把预算花在思考上，然后回一个空的 `content` + `finish_reason=length`；
    // 记忆任务拿到空串只能丢弃重来，用户看到的是"摘要/打标老是失败"，
    // 而真实原因是这一行多写了一个字段。
    //
    // 记忆任务的输入已经给全（素材 + 输出格式），要求的是**稳定的结构化输出**，
    // 不是深度推理。需要推理的模型自己会推理，不用我们替它加码。所以这里
    // 只发 `max_tokens`，让它全归 content。
    //
    // 想确认这一点：真机 `llm_call` 里失败行全是 `finish_reason=length` 且
    // `completion_tokens` 打满上限（`summarize` 平均 2207 > 上限 1600），
    // 成功行的 completion 也常常高过上限 —— reasoning 计费在 usage 里，
    // 但 `content` 被挤没了。

    let timeout = config::number("models.timeout").max(5.0) as u64;
    let url = format!("{}/v1/chat/completions", gateway_base());
    let started = crate::server::logging::now_ms();
    let client = crate::server::core::egress::client_for(None);
    let body_for_call = body.clone();
    let outcome = rt::block_on(async move {
        let response = client
            .post(&url)
            .timeout(Duration::from_secs(timeout))
            .header("content-type", "application/json")
            .header("authorization", format!("Bearer {key}"))
            .header("x-agent2api-source", "memory")
            .header("user-agent", "Agent2API-MemoryBrain/1.0")
            .json(&body_for_call)
            .send()
            .await?;
        let status = response.status();
        let text = response.text().await.unwrap_or_default();
        Ok::<_, reqwest::Error>((status, text))
    });

    let latency = crate::server::logging::now_ms() - started;
    let (status, text) = match outcome {
        Ok(pair) => pair,
        Err(rt::Failure::Timeout) => {
            let message = format!("内部模型请求超时（{timeout}s）");
            index::global().log_llm(&request.task, &model, "gateway", 0, 0, latency, false, Some(&message), None);
            return Err(LlmError::Failed(message));
        }
        Err(rt::Failure::Transport(error)) => {
            let message = format!("无法连接本机网关：{error}");
            index::global().log_llm(&request.task, &model, "gateway", 0, 0, latency, false, Some(&message), None);
            return Err(LlmError::Failed(message));
        }
        Err(rt::Failure::NoRuntime) => {
            return Err(LlmError::Failed(
                "记忆任务需要多线程运行时（当前是 current_thread，无法安全阻塞等待）".into(),
            ));
        }
    };
    let data: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
    let prompt_tokens = data.pointer("/usage/prompt_tokens").and_then(Value::as_i64).unwrap_or(0);
    let completion_tokens = data.pointer("/usage/completion_tokens").and_then(Value::as_i64).unwrap_or(0);
    if !status.is_success() {
        let message = data
            .pointer("/error/message")
            .and_then(Value::as_str)
            .map(str::to_string)
            .unwrap_or_else(|| {
                if text.is_empty() {
                    format!("网关返回 HTTP {}", status.as_u16())
                } else {
                    text.chars().take(300).collect()
                }
            });
        index::global().log_llm(
            &request.task,
            &model,
            "gateway",
            prompt_tokens,
            completion_tokens,
            latency,
            false,
            Some(&message),
            None,
        );
        return Err(LlmError::Failed(message));
    }
    let reply = data
        .pointer("/choices/0/message/content")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .to_string();
    if reply.is_empty() {
        // ── 区分「截断」与「真空」────────────────────────────────
        // 上游把输出预算吃满时会回 `finish_reason: "length"` 且 `content` 为空。
        // 2026-11-24 之前这里还会提示「或调小 `models.taskEffort`」—— 那个旋钮
        // 已经整体不发送了（见文件头），现在只剩一个可调项，错误信息也只指一个
        // 方向，不再把用户往一个不存在的开关上引。
        let truncated = data
            .pointer("/choices/0/finish_reason")
            .and_then(Value::as_str)
            .is_some_and(|reason| reason == "length");
        let message = if truncated {
            format!(
                "输出被 max_tokens={} 截断（finish_reason=length）：任务 {} 一次要产出的条数或长度超过预算，调大 models.taskMaxTokens.{} 或减小 auto.tasks.{}.batchSize",
                request.max_tokens, request.task, request.task, request.task
            )
        } else {
            "模型返回空内容".to_string()
        };
        index::global().log_llm(
            &request.task,
            &model,
            "gateway",
            prompt_tokens,
            completion_tokens,
            latency,
            false,
            Some(&message),
            None,
        );
        return Err(LlmError::Failed(message));
    }
    index::global().log_llm(
        &request.task,
        &model,
        "gateway",
        prompt_tokens,
        completion_tokens,
        latency,
        true,
        None,
        None,
    );
    logging_note(&request.task, &model, latency, prompt_tokens + completion_tokens);
    Ok(Reply {
        text: reply,
        model,
        prompt_tokens,
        completion_tokens,
    })
}

fn logging_note(task: &str, model: &str, latency: i64, tokens: i64) {
    crate::server::logging::log(
        "[Memory]",
        &format!("内部 LLM {task} ok · {model} · {tokens} tokens · {latency}ms"),
    );
}

/// 让模型输出一段 JSON，并容忍它外面裹了 ``` 围栏。
///
/// 为什么不用严格 JSON 模式：不是所有上游都支持 `response_format`，而记忆任务
/// 的结果只用于本地增强，解析失败退化成「按纯文本处理」也能用。围栏剥离是必须
/// 的 —— 几乎所有模型在被要求输出 JSON 时都会顺手加 ```json。
pub fn parse_json(text: &str) -> Option<Value> {
    let trimmed = text.trim();
    let body = if trimmed.starts_with("```") {
        let without_open = trimmed.trim_start_matches("```");
        let without_lang = without_open.split_once('\n').map(|(_, rest)| rest).unwrap_or(without_open);
        without_lang.trim_end_matches("```").trim()
    } else {
        trimmed
    };
    serde_json::from_str::<Value>(body).ok().or_else(|| {
        // 前后有解释性文字时，取第一个 { 到最后一个 } 之间
        let start = body.find('{')?;
        let end = body.rfind('}')?;
        if end <= start {
            return None;
        }
        serde_json::from_str::<Value>(&body[start..=end]).ok()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_plain_json() {
        let value = parse_json(r#"{"a":1}"#).expect("应当解析成功");
        assert_eq!(value.get("a").and_then(Value::as_i64), Some(1));
    }

    #[test]
    fn strips_markdown_fence() {
        let value = parse_json("```json\n{\"tags\":[\"a\"]}\n```").expect("应当解析成功");
        assert_eq!(value.get("tags").and_then(Value::as_array).map(Vec::len), Some(1));
    }

    #[test]
    fn tolerates_chatter_around_object() {
        let value = parse_json("好的，结果如下：{\"ok\":true} 以上。").expect("应当解析成功");
        assert_eq!(value.get("ok").and_then(Value::as_bool), Some(true));
    }

    #[test]
    fn returns_none_for_garbage() {
        assert!(parse_json("完全不是 JSON").is_none());
    }

    /// 机械任务一律不往"更强的模型"上靠。
    ///
    /// ── 这条测试防的是什么 ────────────────────────────────────
    /// 早先 `models.taskEffort` 是配置项，真机把它设成了
    /// `{"summarize":"high","tag":"high","extract":"high",…}` ——
    /// 于是打标签、写摘要这些**只输出短 JSON** 的任务全走了 heavy 模型。
    /// 这不是"多花点钱"：`max_tokens` 是 reason + content 的共享预算，
    /// 上游被要求多想就会把预算花在思考上，回一个空的 `content` +
    /// `finish_reason=length`，整批丢弃重来。
    ///
    /// 真机台账（`llm_call`）：`summarize` 78 次失败烧 707k token、
    /// `tag` 25 次失败烧 134k token。成功行的 `completion_tokens` 也常常
    /// 高过 `max_tokens` —— 说明预算是被 reasoning 吃掉的，不是 content 太长。
    ///
    /// 判据：只有 `profile`（跨条目归纳）与 `distill`（取舍判断）配得上
    /// 更强的模型；其余任务返回 `None`，走兜底模型。
    #[test]
    fn mechanical_tasks_do_not_ask_for_a_stronger_model() {
        for task in [
            "extract",
            "summarize",
            "tag",
            "classify",
            "supersede",
            "dedup",
            "consolidate",
            "ingest",
            "index-scan",
            "cleanup",
        ] {
            assert_eq!(
                task_tag(task),
                None,
                "{task} 是结构化抽取任务，不该往更强/更贵的模型上路由 —— 它的失败模式是内容被挤空，不是结论不够好"
            );
        }
        assert_eq!(task_tag("profile"), Some("heavy"), "画像要跨条目归纳，值得 heavy");
        assert_eq!(task_tag("distill"), Some("light"), "蒸馏要取舍判断，light 足够");
    }

    /// 每个任务的输出预算至少要能装下"一批结论"。
    ///
    /// ── 为什么 800 是下限 ────────────────────────────────────
    /// 一次 `tag` 给 20 条各出 2-5 个标签，JSON 骨架（`{"items":[{"id":…,
    /// "tags":[…]}]}`）本身就要几百 token。早先通用兜底是 1200，
    /// 而 `supersede` 只要回答"哪几条被取代"，800 够用。
    ///
    /// 低于 800 的预算在真机上没出现过成功案例，只会稳定截断。
    #[test]
    fn every_task_budget_can_hold_a_batch() {
        for task in [
            "extract",
            "summarize",
            "tag",
            "classify",
            "supersede",
            "distill",
            "profile",
            "consolidate",
            "dedup",
        ] {
            let budget = max_tokens_for(task);
            assert!(
                budget >= 800,
                "{task} 的默认输出预算只有 {budget} token，装不下一批结构化结论"
            );
        }
    }

    /// 预算随条数抬高：20 条绝不能还是 1600。
    ///
    /// ── 这条测试防的是什么 ────────────────────────────────────
    /// 真机 `summarize` 一次处理 20 条、每条摘要 40 字，固定 1600 必然截断，
    /// 截断后**整批丢弃**（20 条的 prompt token 全白花）。而用户的唯一出口是
    /// 去调 `models.taskMaxTokens` —— 一个"该填多少"靠猜的内部旋钮。
    ///
    /// 判据：`for_items(20, …)` 之后必须显著高于 20 × 单条成本。
    ///
    /// 必须 `isolated`：`for_items` 会读 `config::explicit()` 判断"这个任务
    /// 有没有被显式配过"。真机上 `models.taskMaxTokens` 一旦写过值，测试
    /// 就会走"尊重显式配置"那条分支，断言全部落空 —— 而这种失败只在有配置
    /// 的机器上出现，CI 上是绿的。
    #[test]
    fn budget_grows_with_batch_size() {
        crate::test_support::isolated(
            concat!(module_path!(), "::budget_grows_with_batch_size"),
            || {
                let single = Request::new("summarize", "", "");
                let widened = Request::new("summarize", "", "").for_items(20, 100);
                assert!(
                    widened.max_tokens >= 2000,
                    "20 条摘要仍然只给 {} token —— 真机上这就是整批截断的形状",
                    widened.max_tokens
                );
                assert!(
                    widened.max_tokens > single.max_tokens,
                    "批数变多之后预算没有变：for_items 没生效"
                );
                // 上限别无限涨：200 条 × 1000 = 200k 会撞上游的硬上限
                assert!(
                    Request::new("tag", "", "").for_items(200, 1000).max_tokens <= 32_000,
                    "自动预算没有封顶，200 条会把请求打到上游拒绝"
                );
            },
        );
    }
}
