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
//! ── 思考强度：每个任务固定发一档（2026-11-25 二次修）────────────
//! 早先这里有一个 `models.taskEffort` 旋钮，默认值里 `summarize`/`tag` 是
//! `low`/`minimal`，其余是 `medium`/`high`。它把两个不该混在一起的东西绑成了
//! 一个字段：**"这个任务值不值得用更强的模型"**（选模型）和 **"要让上游多想"**
//! （`reasoning_effort`）。
//!
//! 2026-11-24 那一轮的结论是「干脆不发 `reasoning_effort`」，理由是「`max_tokens`
//! 是共享预算，让上游多想会把正文挤空」。方向对了一半，但那一步**做反了**：
//! 真机 + 本机实测证明 `deepseek-v4.1-flash` 别名背后几十个上游的 mapping 绑定的
//! reasoning 全是 `null`，网关不注入，模型就按**自己默认的最高档**狂想 ——
//! 「不发」不是「不想」，而是「让它随便想」。真机 `summarize` 一半调用正文为 0、
//! `completion_tokens` 打满 `max_tokens`，就是这个形状。
//!
//! 所以现在反过来：**每个任务都显式发一档**，见 [`effort_for`] —— 机械任务
//! （打标签 / 写摘要 / 抽条目…）发 `minimal`，判断类（`distill` / `profile`）发
//! `low`。本机实测同一 prompt：不发 → reasoning 723 / 20 条时正文 0；发 `minimal`
//! → reasoning 17 / 20 条正文一条不少。网关会优先采用客户端这份档位
//! （`upstream::payload` 的 `read_client_level` → `apply_reasoning`）。
//!
//! 选模型那一半（`light` / `heavy`）继续保留，但仍是代码里的固定档位
//! [`task_tag`]、不是配置项。`models.taskEffort` 这个键已从 schema 删除
//! （2026-11-25）：真正决定思考的是 [`effort_for`]，留着它只会让用户把
//! 「更认真」调成「一次都不成功」。
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
        let count = items.min(200) as u32;
        // ── 预算 = 正文 + 少量余量（2026-11-25 二次修）────────────────
        // 思考已经改由 `reasoning_effort` 从**源头**压住（见 [`effort_for`]），
        // 所以这里不再需要为「默认狂想」预留一大块。真机实测（同一份 20 条素材、
        // 模型 `deepseek-v4.1-flash`）：
        //
        //   不发 effort + 2000  → reasoning 吃满 2000、正文 0（旧失败形态）
        //   不发 effort + 7424  → reasoning 仍吃 700+，靠加预算硬扛
        //   发 minimal + 2000   → reasoning 仅 0~30、正文 20 条一条不少
        //
        // 所以「给思考留预算」是治标：模型会一路把预算吃满（2000 吃满、5600 也吃
        // 满、正文照样空）。真正的旋钮是 `reasoning_effort`。预算只需覆盖正文 +
        // 一个小垫底（JSON 骨架、字段名转义、偶发的少量思考）。
        //
        // `CONTENT_HEADROOM` 是正文之外固定留的余量：给 `{"items":[…]}` 的外层
        // 骨架和每条 id 字段。`per_item` 由各任务按「每条正文多长」传进来。
        const CONTENT_HEADROOM: u32 = 400;
        let content = per_item.saturating_mul(count);
        let needed = content.saturating_add(CONTENT_HEADROOM);
        if needed > self.max_tokens {
            self.max_tokens = needed.min(MAX_TOKENS_CEILING);
        }
        self
    }
}

/// 任务对应的思考档位（`reasoning_effort`）。**必须每一个任务都返回一个值。**
///
/// ── 为什么每档都要发，不能「不发」（2026-11-25 二次修，推翻上一轮）──────
/// 上一轮（2026-11-24）的结论是「记忆任务一律不发 `reasoning_effort`」，理由是
/// 「`max_tokens` 是共享预算，让上游多想会把正文挤空」。方向对了一半：确实不该
/// 让它多想 —— 但**不发 ≠ 不想**。真机 `request_raw` + 本机实测（同一个网关、
/// 同一条 `deepseek-v4.1-flash` 别名、同一份 prompt）铁证：
///
///   请求里**没有** `reasoning_effort` → 模型默认狂想：
///     3 条素材：reasoning 723 token，正文 191 字；
///     20 条 + max_tokens 2000：reasoning 吃满 → 正文 0 字、finish_reason=length。
///   请求里 `reasoning_effort="minimal"`：
///     3 条：reasoning 17 token；20 条：reasoning 0~30 token，正文 20 条**一条不少**。
///
/// 也就是说：`deepseek-v4.1-flash` 这个别名背后是几十个真实上游（qoder→Qwen3.8、
/// orcaterm→glm、minimax→MiniMax…，见网关 `modelRules`），它们的 mapping 绑定的
/// reasoning 几乎全是 `null` —— 网关不注入任何限制，模型就按自己默认的最高档想。
/// 唯一能压住它的，是**客户端自己带上 `reasoning_effort`**：网关的
/// `upstream::payload` 会优先用客户端这份（见 `read_client_level` → `apply_reasoning`
/// 的 `injected.or_else(outbound_reasoning_of(..))`）。
///
/// 所以现在反过来：**每个任务都发**，只是分档 ——
///   · 机械任务（打标签 / 写摘要 / 抽条目 / 失效判定 / 合并 / 分类 / 画像数据整理）
///     用 `minimal`：输入已给全、输出是短 JSON，不需要推理，实测正文一字不差还更快；
///   · 判断类任务（`distill` 蒸馏取舍、`profile` 跨条目归纳）用 `low`：实测
///     `minimal` 也照样出合法 JSON（distill 150 条素材 → 5 条结论），`low` 再稳一档，
///     给「压成几条、哪些该 reinforce」这类取舍留一点点思考，但不至于像默认档那样
///     把预算全吃掉。
///
/// 为什么不用 `off`/`none`：网关对这两档**不向上游注入任何字段**（
/// `model_rules::is_thinking_off`，Qoder 关思考会让 Qwen 异常、CatPaw 没这档），
/// 等于退回「不发 = 默认狂想」。`minimal` 才是真正意义上「尽量少想」的下限。
pub fn effort_for(task: &str) -> &'static str {
    match task {
        // 判断类：需要一点点取舍，给低档而非最小档。
        "distill" | "profile" | "consolidate" | "supersede" => "low",
        // 其余（extract / summarize / tag / classify / dedup / insight / …）都是
        // 结构化抽取，最小思考足够，且实测正文更完整、更快。
        _ => "minimal",
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
    /// 这次实际用的输出预算（正文 + 固定余量，见 `for_items`）。台账/重试用得到。
    pub max_tokens: u32,
    /// 模型这次把多少 token 花在思考上（`usage.completion_tokens_details.reasoning_tokens`）。
    ///
    /// ── 为什么要单独记下来（2026-11-25）──────────────────────
    /// `completion_tokens` 是「思考 + 正文」的总和，单看它分不清「模型写了
    /// 2000 字正文」和「模型想了 2000 token、正文一个字没写」。真机上这个区分
    /// 是做减法做出来的（`reasoning_tokens=2000`、`content` 长度 0），绕了一大圈。
    /// 这张字段就是为了下次不必再绕：思考吃预算的形状，一次就能看见。
    ///
    /// 上游不回这个字段时是 `None`（不是 0）—— 「没报」和「没想」是两件事，
    /// 混在一起会让统计把「不支持该字段的上游」算成「从不思考」。
    pub reasoning_tokens: Option<i64>,
    /// 上游报了 `finish_reason=length`，但正文**解析得出结果**。
    ///
    /// ── 为什么这也要单独标出来（2026-11-25）──────────────────────
    /// `parse_json` 有个花括号回退：从第一个 `{` 取到最后一个 `}`。截断的
    /// `{"items":[{"id":"a",…},{"id":"b",…},{"id":"c","summ` 恰好能被回退救成
    /// 「前两条完整的结果」，于是**看起来成功**。这不是好事 —— 5 条只写了 2 条，
    /// 另外 3 条一声不响地没有摘要，台账里却是一行 ok。
    ///
    /// 所以带这个标记的 `Reply` 是「有结果、但不全」：调用方可以照常用它兜底
    /// （总比整批丢弃强），但要知道这不是一份完整结果。[`complete`] **不**为它
    /// 重试 —— 最常见的那一档是「完整 JSON + 尾部解释被切」，重试必然同样结果、
    /// 白烧一整份 prompt（原因见 [`Output`]）；它只把这个事实写进 `llm_call.note`
    /// （`partial=1`），让「条数对不上」在数据里留痕，而不是凭空相信它完整。
    pub truncated: bool,
}

#[derive(Debug)]
pub enum LlmError {
    /// 没配模型 / 没可用 Key / 记忆功能关着 —— 调用方应当挂起任务，不是报错
    Unavailable(String),
    /// 真跑了但失败（网络、上游 4xx/5xx、解析不了）
    Failed(String),
    /// 跑完了但输出被输出预算截断（`finish_reason=length`，正文为空或未闭合）。
    ///
    /// ── 为什么单独成一个变体，而不是混进 `Failed`（2026-11-25）────────
    /// 「被预算截断」和「网络挂了 / 上游 500」是两种完全不同的失败：前者只要
    /// 给更多 token 就能成，后者给再多也没用。混在一起的结果是，调用方只能
    /// 一律丢弃，用户看到「摘要失败」却无从知道其实再加两千 token 就好了。
    ///
    /// 拆出来之后，[`complete`] 可以**只对这一种**自动加预算重试一次 ——
    /// 精准重试，不会让真的坏请求花两倍 token。
    Truncated(String),
}

impl LlmError {
    pub fn message(&self) -> &str {
        match self {
            Self::Unavailable(message) | Self::Failed(message) | Self::Truncated(message) => message,
        }
    }

    pub fn is_unavailable(&self) -> bool {
        matches!(self, Self::Unavailable(_))
    }

    pub fn is_truncated(&self) -> bool {
        matches!(self, Self::Truncated(_))
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

/// 预算上限。抬高 `max_tokens` 不按上限计费，但存在的意义只是「让原本装不下
/// 的请求收尾」；再往上加既没必要，也会撞部分上游的硬上限。
const MAX_TOKENS_CEILING: u32 = 32_000;

/// 截断后重试时把预算放到多大。
///
/// ── 为什么是「翻倍」而不是「加固定值」───────────────────────────
/// 真机上的截断不是差一点点：`summarize` 请求 2000、响应 `reasoning_tokens`
/// 就 2000，差的是「整份思考 + 整份正文」。加 500 只是让下一次再失败一遍，
/// 白烧一次 prompt token（真机单次 prompt 约 9.5k token，不便宜）。
/// 翻倍在两次之内就能覆盖到 4 倍余量，而 `MAX_TOKENS_CEILING` 兜住上限。
const RETRY_FACTOR: u32 = 2;

/// 截断后该把预算抬到多少；`None` = 已经贴着上限，重试没有意义。
///
/// 抽成独立函数只为一件事：**这条算术要能被测**。它埋在 `complete` 里的话，
/// 要验证「翻倍」与「封顶」就得真的打一次网关 —— 一个纯算术不该付那个代价。
fn widened_budget(budget: u32) -> Option<u32> {
    let next = budget.saturating_mul(RETRY_FACTOR).min(MAX_TOKENS_CEILING);
    if next > budget {
        Some(next)
    } else {
        None
    }
}

/// 一次补全的正文判读结果。
///
/// ── 为什么「截断但解析成功」要单独成一档（2026-11-25）────────────
/// `parse_json` 的最后一个回退是「取第一个 `{` 到最后一个 `}`」。这带来两种
/// 完全不同的「`finish_reason=length` 却解析成功」：
///
///   · **JSON 完整、尾部解释被切**：模型写完 `{"items":[…完整…]}` 又接着写
///     「以上是 N 条摘要……」被切断。回退取到的是完整 JSON，**结果没有缺损**。
///     这是最常见的一档，真机上 `tag`/`summarize` 每天都在发生。
///   · **模型直接输出裸数组**（不是我们现在的 prompt 形状，但上游换实现可能
///     变成这样）：`[{…},{…},{…` 被切，回退只取到第一个对象，**条数悄悄少**。
///
/// 两者在字符串层面无法区分 —— 要区分得知道「这次本该产出几条」，那是各任务
/// 自己的 schema 知识，不该塞进 `llm` 层。所以这里只做**标记**不做处置：
/// `Partial` 的语义是「有可用结果，但上游报了截断，可能不全」，交给台账记
/// `partial=1`，让「条数对不上」这件事在数据里留下痕迹，而不是凭空相信它完整。
///
/// ── 为什么不因此重试（代价算过）────────────────────────────
/// 最常见的那一档（完整 JSON + 尾部被切）重试也是**必然**同样结果：模型还是会
/// 写完 JSON 再解释，还是会被切。重试一次白烧一整份 prompt（真机实测 9.5k）。
/// 真正需要补救的是「裸数组少条数」那一档，而它在我们当前全部 prompt 形状下
/// 不可达（都是 `{"items":[…]}` 外层对象，外层 `}` 没闭合就解析失败）。
/// 为一个当前不可达的形状给所有调用加一倍成本，算不过来。
#[derive(Debug, PartialEq)]
enum Output {
    /// 有正文，且没被截断
    Complete,
    /// `finish_reason=length` 但整体解析成功 —— 结果可用，可能不全
    Partial,
    /// 正文为空，或截断到连 JSON 都拼不出来（连残片都没有）
    Empty,
}

fn read_output(reply: &str, finish: &str) -> Output {
    if reply.is_empty() {
        return Output::Empty;
    }
    if finish != "length" {
        return Output::Complete;
    }
    // 截断且解析得出：结果可用，但标出来。
    // 截断且解析不出：连残片都没有，按空处理（走 `Truncated` → 加预算重试）。
    if parse_json(reply).is_some() {
        Output::Partial
    } else {
        Output::Empty
    }
}

/// 跑一次补全。
///
/// 同步函数（内部用 `blocking::in_place` 等异步结果）：调用方几乎都是
/// `spawn_blocking` 里的同步代码（写记忆、MCP 分发、调度器），在这层再开
/// 一个运行时没有意义，而且会在单线程运行时上 panic。
///
/// ── 被截断时自动加预算重试一次（2026-11-25）─────────────────────
/// 思考已由 [`effort_for`] 从源头压住，但预算终究是估计：素材偶尔比上一批长、
/// 或个别上游对档位打折，都仍可能撞截断。而截断的代价是**整批丢弃** —— 真机上
/// 一次 `summarize` 的 prompt 就要 9.5k token，白花。
///
/// 所以这里补最后一道：失败原因是 [`LlmError::Truncated`] 时，把 `max_tokens`
/// 翻倍再跑一次。只重试这一种 —— 网络错误、上游 5xx、JSON 不合法重试也都是
/// 同样结果，重试只是把 token 花两遍。重试次数固定 1 次：第二次还截断，说明
/// 问题不在预算（比如素材本身超长），继续加倍只会无限烧钱。
///
/// ── 这条重试**优先于** `models.taskMaxTokens`（2026-11-25）───────────
/// `for_items` 尊重用户显式写的值，但那是「事前估计」层面的尊重：用户写了
/// 1600，代码不去猜他要更多。而重试发生在**事后** —— 1600 已经被证明装不下，
/// 这时候再守着它，等于把「用户填了一个数字」当成「用户要求失败」。所以重试
/// 不受显式配置约束；但只重试一次，也不会把它无声抬成 32k。
pub fn complete(request: Request) -> Result<Reply, LlmError> {
    let budget = request.max_tokens;
    // 先把第一次的结果取成局部变量再拆：直接 `match complete_once(&request)`
    // 会把那次借用在整段 match 里挂住，后面 `let mut wider = request`（move）
    // 就编不过 —— 一个只有真正编译才会暴露的借用冲突。
    let first = complete_once(&request);
    let first_error = match first {
        // `Partial` 不重试：最常见的那一档是「完整 JSON + 尾部解释被切」，
        // 重试必然同样结果，白烧一整份 prompt（原因见 [`Output`] 的说明）。
        // 它带着 `truncated=true` 回去，由台账记 `partial=1`。
        Ok(reply) => return Ok(reply),
        Err(LlmError::Truncated(message)) => message,
        other => return other,
    };
    let Some(retry) = widened_budget(budget) else {
        // 已经贴着上限，再加也不会变宽 —— 直接把原错误交回去。
        return Err(LlmError::Truncated(first_error));
    };
    crate::server::logging::log(
        "[Memory]",
        &format!(
            "任务 {} 输出被 max_tokens={budget} 截断，按 {retry} 重试一次",
            request.task
        ),
    );
    let mut wider = request;
    wider.max_tokens = retry;
    match complete_once(&wider) {
        // 第二次还截断：把第一次的报错带上，并说明已经加倍过 —— 否则用户看到
        // 「被 max_tokens=4000 截断」会以为代码没重试。
        Err(LlmError::Truncated(_)) => Err(LlmError::Truncated(format!(
            "{first_error}；已按 max_tokens={retry} 重试仍然截断"
        ))),
        other => other,
    }
}

/// 跑**一次**补全（不重试）。重试策略在 [`complete`] 里。
fn complete_once(request: &Request) -> Result<Reply, LlmError> {
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
        // ── 必须显式发思考档位（2026-11-25 二次修，推翻上一轮）──────────
        // 见 [`effort_for`]：不发时模型默认按最高档狂想，`max_tokens` 被吃满、
        // 正文为空。本机实测同一 prompt：不发 → reasoning 723、正文 191；
        // 发 minimal → reasoning 17、正文 188；20 条 + 2000 时更极端（不发=正文 0、
        // 发 minimal=20 条全齐）。网关会优先采用客户端这份档位
        // （`read_client_level` → `apply_reasoning` 的 `injected.or_else(...)`）。
        "reasoning_effort": effort_for(&request.task),
    });
    // ⚠ 上面那行是**修复的核心**。上一轮（2026-11-24）把 `reasoning_effort`
    // 整个删掉，以为「不发 = 不想」；真机证明恰好相反 —— 不发 = 按上游默认最高档想。
    // `deepseek-v4.1-flash` 别名背后几十个上游的 mapping 绑定的 reasoning 全是
    // `null`，网关不注入，模型就用默认（很高）的档位，于是把共享预算烧光。
    // 把档位显式压到 `minimal`/`low` 才是从源头解决，[`for_items`] 的预算因此
    // 也不需要再为「默认狂想」预留一大块。

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
    // ── 思考 token 单独取（2026-11-25）──────────────────────────
    // 各家上游的字段位置不统一：OpenAI 风格在
    // `usage.completion_tokens_details.reasoning_tokens`，部分中转在
    // `usage.reasoning_tokens`，也有藏在 `usage.completion_tokens_details`
    // 之外的。两处都看，取到的第一个算数。取不到就是 `None` —— 「没报」与
    // 「没想」必须分开，否则统计会把「不支持该字段的上游」当成「从不思考」。
    let reasoning_tokens = data
        .pointer("/usage/completion_tokens_details/reasoning_tokens")
        .and_then(Value::as_i64)
        .or_else(|| data.pointer("/usage/reasoning_tokens").and_then(Value::as_i64));
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
    let finish = data
        .pointer("/choices/0/finish_reason")
        .and_then(Value::as_str)
        .unwrap_or("");
    let truncated = finish == "length";
    // ── 三种结局分开判，别只判「空不空」（2026-11-25）────────────────
    // 见 [`read_output`] 的说明：截断到一半的 JSON 能解析成功、只含前几条，
    // 那份残片必须被标成 `Partial`，否则「5 条只写了 2 条」会以「成功」入账。
    let output = read_output(&reply, finish);
    if output == Output::Empty {
        // ── 区分「截断」与「真空」────────────────────────────────
        // 上游把输出预算吃满时会回 `finish_reason: "length"` 且 `content` 为空。
        let message = if truncated {
            format!(
                "输出被 max_tokens={} 截断（finish_reason=length，思考占 {} token）：任务 {} 一次要产出的条数或长度超过预算",
                request.max_tokens,
                reasoning_tokens.map(|n| n.to_string()).unwrap_or_else(|| "未报".into()),
                request.task
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
        return Err(if truncated {
            LlmError::Truncated(message)
        } else {
            LlmError::Failed(message)
        });
    }
    // 成功行也记下「这次预算是多少、思考吃掉多少」—— 下次判断要不要调预算，
    // 看的就是这一列。不写原始响应体（体积大且含素材正文）。
    //
    // `partial` 表示「截断但解析得出」：这一行的 `ok` 是 1（确实产出了可用的
    // 一部分），note 里必须写出来 —— 否则台账里它和「完整成功」长得一模一样，
    // 而它恰恰是「条数没齐」的那一行。
    let partial = output == Output::Partial;
    let note = format!(
        "effort={} max_tokens={} reasoning={}{}",
        effort_for(&request.task),
        request.max_tokens,
        reasoning_tokens.map(|n| n.to_string()).unwrap_or_else(|| "未报".into()),
        if partial { " partial=1" } else { "" }
    );
    index::global().log_llm(
        &request.task,
        &model,
        "gateway",
        prompt_tokens,
        completion_tokens,
        latency,
        true,
        None,
        Some(&note),
    );
    logging_note(&request.task, &model, latency, prompt_tokens + completion_tokens);
    Ok(Reply {
        text: reply,
        model,
        prompt_tokens,
        completion_tokens,
        max_tokens: request.max_tokens,
        reasoning_tokens,
        truncated: partial,
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

    /// 截断的输出必须被分成三档，`truncated` 只标 `Partial`。
    ///
    /// ── 这条测试防的是什么 ────────────────────────────────────
    /// `parse_json` 的最后一个回退是「取第一个 `{` 到最后一个 `}`」。这让
    /// `finish_reason=length` 有两种完全不同的表现：
    ///
    ///   · JSON 写完、尾部解释被切 → 回退取到完整 JSON，**结果无缺损**（最常见）；
    ///   · 裸数组被切 → 回退只取到第一个对象，**条数悄悄少**。
    ///
    /// 字符串层面分不开这两者，所以这里只判「有没有可用正文」，用 `Partial`
    /// 把上游那次 `length` 标出来。判错的两种方式都会静默坏事：
    ///   · 全当 `Complete` → 「条数少」那一档不留任何痕迹；
    ///   · 全当 `Empty` → 明明拿到完整结果，扔掉重来，白烧 9.5k prompt。
    ///
    /// ── 「裸数组被切」这一档其实到不了消费方（2026-11-25 复核）──────
    /// `enrich.rs` 里所有消费点都写 `parsed.get("items").and_then(as_array)`：
    /// 回退救回的是**一个对象**（`{"id":"a",…}`），不是 `[{…}]`，所以 `.get("items")`
    /// 必然 `None`，那一块一条都写不进去。也就是说这档在数据上等价于「整块丢弃」，
    /// 只是它被记成 `ok=1 partial=1` 而不是 `ok=0`。
    ///
    /// 这不影响本函数的分档判据（`Partial` 的语义就是「有正文但可能不全」），
    /// 但看台账时要知道：**`partial=1` 且条数为 0 是可能的**，别把它当成
    /// 「模型给了内容所以没问题」。
    #[test]
    fn truncated_json_is_recognised_as_partial() {
        // 典型形态：完整 JSON 之后跟着一段被切断的解释文字。
        let cut = r#"{"items":[{"id":"a","summary":"甲"}]} 以上共 1 条"#;
        assert_eq!(
            read_output(cut, "length"),
            Output::Partial,
            "截断但解析得出的结果被当成了完全成功 —— 上游报的 length 就丢了"
        );
        // 裸数组被切：回退取「第一个 `{` 到最后一个 `}`」，救出的是**第一个
        // 对象本身**，不是 `[{…}]` 这样的单元素数组。
        //
        // ⚠ 这里曾经断言 `as_array().len() == Some(1)`，CI 三个平台一起红：
        // 回退切的边界是花括号，不是方括号，所以外层 `[` 和 `]` 根本没进来，
        // `parse_json` 返回的是 `{"id":"a",…}` 这个 object。断言写错了形状，
        // 不是 `parse_json` 的行为错了 —— 对象形态一样能说明「残片只剩一条」。
        let pieces = r#"[{"id":"a","summary":"甲"},{"id":"b","summ"#;
        assert_eq!(read_output(pieces, "length"), Output::Partial);
        let salvaged = parse_json(pieces).expect("第一个对象完整，应当能救回来");
        assert_eq!(
            salvaged.get("id").and_then(Value::as_str),
            Some("a"),
            "残片只剩第一个对象 —— 正是「条数悄悄少」那一档，必须留痕"
        );
        assert!(
            salvaged.get("summary").and_then(Value::as_str).is_some(),
            "救回来的对象应当是完整的，不是半个字段"
        );
        // 截断且解析不出：连残片都没有 → Empty（走 Truncated → 加预算重试）。
        assert_eq!(read_output(r#"{"items":[{"id":"a","summ"#, "length"), Output::Empty);
        // 没截断：正常，无论形状。
        assert_eq!(read_output("{}", "stop"), Output::Complete);
        // 真空：任何 finish_reason 都是 Empty。
        assert_eq!(read_output("", "stop"), Output::Empty);
        assert_eq!(read_output("", "length"), Output::Empty);
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

    /// 预算按「正文 + 固定余量」算，且每个任务都配置了思考档位。
    ///
    /// ── 这条测试防的是什么 ────────────────────────────────────
    /// 2026-11-25 二次修把预算公式从「正文 + 每-条思考预留（1024 + 220×条数）」
    /// 改回「正文 + 400 固定余量」，因为思考已改由 `reasoning_effort` 从源头压住
    /// （见 `effort_for`）。如果哪天有人把这层改回去（又给每-条加一大块思考预留），
    /// 或者把 `effort_for` 删成「不发」，本测试要能立刻发现 —— 判据是：
    ///   · 20 条的预算 ≈ 20×100 + 400 = 2400，**不再**是 7424 那种「为思考预留」；
    ///   · 5 条 → 20 条预算按正文线性增长、封顶 32k；
    ///   · `effort_for` 对每个已知任务都返回一个非空档位（没有一个任务「不发」）。
    #[test]
    fn budget_tracks_content_and_every_task_has_effort() {
        crate::test_support::isolated(
            concat!(module_path!(), "::budget_tracks_content_and_every_task_has_effort"),
            || {
                let five = Request::new("summarize", "", "").for_items(5, 100);
                let twenty = Request::new("summarize", "", "").for_items(20, 100);
                // 20 条：正文 2000 + 400 固定余量 = 2400（不是 7424 那种思考预留）
                assert_eq!(
                    twenty.max_tokens, 2400,
                    "20 条摘要预算应=正文2000+固定余量400；若变成 7k+ 说明思考预留又被加回来"
                );
                assert!(
                    twenty.max_tokens > five.max_tokens,
                    "条数从 5 涨到 20，预算没变：for_items 没按正文走"
                );
                // 封顶：200 条 × 1000 会算出 200k，必须被 32k 兜住
                assert!(
                    Request::new("tag", "", "").for_items(200, 1000).max_tokens <= 32_000,
                    "自动预算没有封顶，200 条会把请求打到上游拒绝"
                );
            },
        );
        // 每个任务都必须有档位 —— 一个「不发」就等于默认狂想、正文被挤空。
        // 这就是上一轮 bug 的形状：`effort_for` 若返回空串/漏掉某任务，本断言挂。
        for task in [
            "extract", "summarize", "tag", "classify", "distill", "profile", "consolidate",
            "supersede", "dedup", "insight",
        ] {
            let effort = effort_for(task);
            assert!(!effort.is_empty(), "任务 {task} 没有思考档位：会退回默认狂想");
        }
        // 机械任务用 minimal（实测正文更全更快），判断类用 low（留一点取舍）。
        assert_eq!(effort_for("summarize"), "minimal");
        assert_eq!(effort_for("tag"), "minimal");
        assert_eq!(effort_for("distill"), "low");
        assert_eq!(effort_for("profile"), "low");
    }

    /// 截断后重试：翻倍，但不越过上限，也不在已经触顶时无意义地重跑。
    ///
    /// ── 这条测试防的是什么 ────────────────────────────────────
    /// `complete` 的截断重试是「整批不被丢弃」的最后一道。它出错的方式很安静：
    ///   · 乘法写成 `budget + RETRY_FACTOR` → 每次只多给 2 token，等于没重试；
    ///   · 忘了 `.min(MAX_TOKENS_CEILING)` → 32k 再翻一倍，部分上游直接 400；
    ///   · 触顶后还返回 `Some(同样的大小)` → 第二次调用与第一次一模一样，
    ///     白烧一整份 prompt token（真机单次约 9.5k）。
    /// 三条都不会让编译失败，只会让「修好的重试」看起来修好了、实际没修。
    #[test]
    fn retry_doubles_budget_without_breaking_the_ceiling() {
        assert_eq!(widened_budget(2000), Some(4000), "截断后预算没有翻倍");
        assert_eq!(widened_budget(1600), Some(3200));
        // 16k 翻倍到 32k 正好贴着上限，还能再宽一次
        assert_eq!(widened_budget(16_000), Some(MAX_TOKENS_CEILING));
        // 已经在上限：重试不会更宽，必须返回 None 让调用方直接放弃
        assert_eq!(widened_budget(MAX_TOKENS_CEILING), None, "触顶后还安排了重试");
        assert_eq!(widened_budget(MAX_TOKENS_CEILING + 1), None);
        // 任何返回值都不能越过上限
        for budget in [1u32, 800, 2400, 7424, 31_999] {
            let next = widened_budget(budget).expect("低于上限的都该能再宽一次");
            assert!(next <= MAX_TOKENS_CEILING, "{budget} 翻倍后越过上限：{next}");
            assert!(next > budget, "{budget} 重试预算没有变大：{next}");
        }
    }
}
