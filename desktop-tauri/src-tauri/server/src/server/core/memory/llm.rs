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
//!   2. `models.taskEffort` 里该任务配了思考强度 → 说明用户在意质量，仍走
//!      `models.routing` 的 `tags` 匹配（`light` / `heavy`），命中即用；
//!   3. 都没有 → `models.fallbackModel`；再没有 → 网关目录里第一个非图像模型。
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
    /// 覆盖思考强度（空 = 用 `models.taskEffort` 里该任务的配置）
    pub effort: String,
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
            effort: String::new(),
            model: String::new(),
            max_tokens: max_tokens_for(task),
            temperature: 0.2,
        }
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
/// 1200 对 `extract` / `tag` / `summarize` 是够的（一次十来条短句），所以不能
/// 简单地把全局值调大 —— 那会让每一条小任务都按大任务的额度去预留。
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
    // 2. 该任务配了思考强度 = 用户在意图质量，按标签在模型池里找
    let effort = config::object_of("models.taskEffort");
    let wants_quality = effort
        .get(task)
        .and_then(Value::as_str)
        .is_some_and(|level| matches!(level, "medium" | "high"));
    if wants_quality {
        let tag = if effort.get(task).and_then(Value::as_str) == Some("high") { "heavy" } else { "light" };
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

    let mut body = json!({
        "model": model,
        "messages": [
            { "role": "system", "content": request.system },
            { "role": "user", "content": request.user },
        ],
        "stream": false,
        "max_tokens": request.max_tokens,
        "temperature": request.temperature,
    });
    // 思考强度：任务级配置优先，其次调用方覆盖。空 = 不传（有些上游不认这个字段）
    let effort = if request.effort.trim().is_empty() {
        config::object_of("models.taskEffort")
            .get(&request.task)
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string()
    } else {
        request.effort.trim().to_string()
    };
    if !effort.is_empty() && effort != "minimal" {
        body["reasoning_effort"] = json!(effort);
    }

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
        // 上游把输出预算吃满时会回 `finish_reason: "length"` 且 `content` 为空
        //（思考型模型尤其常见：token 全花在 reasoning 上，正文一个字没留）。
        // 早先这里一律报「模型返回空内容」，于是用户调了半天 prompt，真正该调的
        // 是 `models.taskMaxTokens` 或 `models.taskEffort` —— 错误信息指错了方向。
        let truncated = data
            .pointer("/choices/0/finish_reason")
            .and_then(Value::as_str)
            .is_some_and(|reason| reason == "length");
        let message = if truncated {
            format!(
                "输出被 max_tokens={} 截断（finish_reason=length）：任务 {} 的预算不够，调大 models.taskMaxTokens.{}",
                request.max_tokens, request.task, request.task
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
}
