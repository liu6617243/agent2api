//! 识图（图像识别 / Vision）：把客户端发来的图交给一个**固定的**视觉模型
//! 描述成文字，再让不支持图片的模型「读到」这段文字。
//!
//! ── 这个模块解决什么（用户实测的现象）────────────────────────
//! 客户端在**发送之前**就按 `/v1/models` 声明的能力决定要不要把图交出来：
//! 模型被标成纯文本，它根本不会请求网关，而是自己在本地绕 —— 装 OCR 工具、
//! 把图丢掉、或直接提示「不支持图像输入」。所以「让纯文本模型也能接图」必须
//! 做两件事，缺一不可：
//!
//!   ① **对外声明所有模型都收图**（[`listed_item`]，由 [`declares_images`] 驱动）
//!      —— 客户端才会把图交过来；
//!   ② **网关内部把图换成文字**（[`rewrite`] + 上游 `describe`）—— 纯文本模型
//!      才接得住。
//!
//! 只做 ② 等于白做（图根本不会来）；只做 ① 更糟（图来了却描述不了，纯文本
//! 模型直接撞上游报错，比客户端自己 OCR 还差）。
//!
//! ── 与 magpie 的对应关系 ────────────────────────────────────
//! 参考实现是 yetone/magpie（Go）。本模块的两半与它的两半一一对应：
//!
//!   magpie                                 本模块
//!   ─────────────────────────────────────  ──────────────────────────────
//!   `provider/visible.go` 的 `Described`    [`declares_images`]
//!   `route.go` 里 `Images: e.Images || seen` [`listed_item`] 的覆盖
//!   `gateway/vision.go` 的 `seenBody()`     [`rewrite`]
//!   `gateway/vision.go` 的 `AutoVision()`   [`resolve`] 的 auto 分支
//!   `gateway/vision.go` 的描述缓存           [`CACHE`]
//!
//! ── 为什么是「钉住一个模型」而不是「在视觉模型之间轮询」──────
//! 与 magpie 同取舍：用户在设置里点名就用点名的那个；auto 就在目录里
//! **确定性地挑第一个**声明收图的模型，挑中即钉住这一次请求。不在多个视觉
//! 模型之间做故障转移 —— 那会让「描述一张图」变成一串串行重试（每换一家都
//! 要重发整张图），首字延迟成倍恶化，而识图失败的正确处置是**把图原样留给
//! 主请求**（见 [`rewrite`] 的失败分支），不是换一家继续试。
//!
//! ── 本模块与 `core::upstream` 的分工 ─────────────────────────
//! 本模块**不认识 HTTP、不发请求**：它只回答「该用哪个模型」「请求里有哪些图」
//! 「拿到描述后怎么替换」。真正的识图请求由 `core::upstream::UpstreamService`
//! 发出（走完整的选路 / 重试 / 限流链路，见 `forward` 的识图段），于是识图
//! 请求与正常请求共享同一套容错，本模块也不会反向依赖转发层。

use std::collections::{HashMap, VecDeque};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use serde_json::{json, Value};
use sha2::{Digest, Sha256};

use crate::server::config;
use crate::server::core::account_store::AccountStore;
use crate::server::core::models::{js_truthy, list_item, model_id};

/// 认作「图片块」的 `type` 值（Chat Completions 的 `image_url`、Responses 的
/// `input_image`）。Anthropic 的 `{"type":"image"}` 在进转发层之前已被
/// `protocol::anthropic` 的 `image_to_chat` 转成 `image_url`，所以不在这里。
const IMAGE_BLOCK_TYPES: [&str; 2] = ["image_url", "input_image"];

/// 描述缓存容量（条）。与 magpie 的 2048 同量级：一张图一段描述，几百字节
/// 到几 KB，2048 条最多十几 MB；超出后按插入顺序淘汰最老的一条。
const CACHE_CAPACITY: usize = 2048;

/// 识图模型解析结果的缓存时长。
///
/// 解析要遍历所有家的清单（`active_manifests` 会调各家的 `list_models`），
/// 而含图请求常常连着来（同一会话反复贴图），逐请求重算纯属浪费。30 秒是
/// 「用户刚加了账号 / 刚刷新目录，等一下就能用上」与「不重复扫目录」之间的
/// 折中；配置本身（点名 / auto / off）一改就会换 key，立即失效。
const PLAN_TTL: Duration = Duration::from_secs(30);

/// 识图请求的系统提示词。
///
/// 语义照 magpie 的 `visionSystem`：**只转录与描述，不猜测**。刻意用中文而不
/// 是英文 —— 描述会被塞进用户自己的对话里，中文对话里混一段英文描述会让
/// 主模型转述得别扭；而主流视觉模型对中文指令的遵从度足够。
const VISION_SYSTEM: &str = "\
你是一个图像转录器。用户会把图片发给你，你要把图里的信息完整、准确地转成文字，\
供一个看不见图片的模型继续处理。规则：\n\
1. 图里有文字就逐字转录，保留原文语言、标点、大小写与数字；不要翻译、不要概括、不要改写。\n\
2. 界面截图说清是什么应用或网页、当前在哪个页面、可见的按钮与输入框、任何报错或提示文字。\n\
3. 图表读出标题、坐标轴、图例与具体数值，能读到的都读。\n\
4. 流程图 / 架构图说清节点名称与连线方向。\n\
5. 照片描述可见的物体、场景与相对位置。\n\
6. 只写看得见的内容；看不清就写「（不清晰）」，绝不猜测、不补全、不解释。\n\
直接输出转录与描述正文，不要加「以下是图片内容」之类的前后缀。";

/// 替换进请求体的文本前缀（描述为空时不替换，见 [`rewrite`]）。
const PLACEHOLDER_PREFIX: &str = "[图片内容]\n";

/// 一次识图要用的模型。
#[derive(Clone, Debug)]
pub struct Target {
    /// 识图模型名（走正常路由，因此必须在聚合目录里）
    pub model: String,
    /// 用户点名时钉住的家（`None` = auto，交给正常路由）
    pub provider: Option<String>,
    /// 思考等级覆盖（`None` = 用模型自己的默认档）
    pub effort: Option<String>,
}

/// 解析当前配置下该用哪个识图模型（`None` = 这个功能当前不生效）。
///
/// 三种取值的语义见 `config::KEY_IMAGE_RECOGNITION`：
///   - `"off"` → 恒 `None`（不声明、不描述）；
///   - `"<provider>/<model>"` → 点名的那个（**必须**声明收图，否则当没配）；
///   - 空串（auto）→ 目录里第一个声明收图的模型。
///
/// 结果带 TTL 缓存（见 [`PLAN_TTL`]）。缓存的 key 含配置值，所以用户改设置
/// 后下一个请求就换新的解析，不必等 TTL 过期。
pub fn resolve(store: &AccountStore) -> Option<Target> {
    // 轻量读取（不克隆整份 raw）：这条路径在**每个含图请求**上跑一次
    let (setting, effort) = config::image_recognition_of();
    // `\u{1}` 是不可能出现在这两个值里的分隔符（两者都是人写的短串）
    let key = format!("{setting}\u{1}{effort}");

    if let Some(hit) = cached_plan(&key) {
        return hit;
    }
    let target = compute(store, &setting, &effort);
    store_plan(key, target.clone());
    target
}

/// 「对外要不要声明所有模型都收图」。
///
/// 与 [`resolve`] **同源**：有可用的识图模型才敢声明收到图 —— 声明了却描述
/// 不了，等于把客户端哄进来撞一个必然失败的请求（见模块头「只做 ① 更糟」）。
pub fn declares_images(store: &AccountStore) -> bool {
    resolve(store).is_some()
}

/// 实际解析（缓存未命中时才跑）。
fn compute(store: &AccountStore, setting: &str, effort: &str) -> Option<Target> {
    if setting.eq_ignore_ascii_case("off") {
        return None;
    }
    let effort = (!effort.is_empty()).then(|| effort.to_string());
    // ── 点名 `provider/model` ──────────────────────────────────
    // 校验它真在某家的清单里、且声明收图：写错一个字母就当没配（回到「不声明、
    // 不描述」），而不是发出去撞 404 —— 后者会让每条含图请求都多打一次上游。
    if let Some((provider, model)) = setting.split_once('/') {
        let (provider, model) = (provider.trim(), model.trim());
        if provider.is_empty() || model.is_empty() {
            return None;
        }
        if !crate::server::core::providers::catalog::provider_supports_images(provider, model) {
            return None;
        }
        return Some(Target {
            model: model.to_string(),
            provider: Some(provider.to_string()),
            effort,
        });
    }
    // ── auto：目录里第一个声明收图的 ───────────────────────────
    // **确定性**（按 manifest 顺序，不随机、不按用量）：同一个目录永远挑中同
    // 一个模型，用户问「为什么总是它」时答案可复现。内置家优先于自定义家
    // （内置家的清单与账号在本进程内更稳定）。
    //
    // 这里读的是**未应用 [`listed_item`] 覆盖**的原始清单（`active_manifests`
    // 的条目是驼峰 `supportsImages`），否则覆盖一生效就会「挑中一个其实不收图
    // 的模型」—— 覆盖是给下游看的声明，不是事实。
    for (provider, manifest) in crate::server::core::providers::catalog::active_manifests(store) {
        for item in &manifest {
            if js_truthy(item.get("supportsImages").unwrap_or(&Value::Null)) {
                let model = model_id(item);
                if !model.is_empty() {
                    return Some(Target {
                        model,
                        provider: Some(provider),
                        effort,
                    });
                }
            }
        }
    }
    for (provider, models) in crate::server::core::providers::custom::catalog_providers(store) {
        for item in &models {
            if js_truthy(item.get("supportsImages").unwrap_or(&Value::Null)) {
                let model = model_id(item);
                if !model.is_empty() {
                    return Some(Target { model, provider: Some(provider), effort });
                }
            }
        }
    }
    None
}

/// `/v1/models` 条目的统一出口：先按 [`list_item`] 映射，再按需覆盖能力位。
///
/// ── 为什么覆盖写在这里而不是改 `list_item` ────────────────────
/// `list_item` 是「来源无关的纯函数」（见 `models::shape` 的模块头），往里塞
/// 全局配置读取会破坏那个契约、也让它的单测要改。包一层之后，`list_item` 的
/// 输出对不想开这个功能的人**逐字节不变**，而覆盖只发生在三个出口的调用点上。
///
/// ── 覆盖什么、为什么两套都要改 ──────────────────────────────
/// `supports_images` 与 `input_modalities` 是同一件事的两套说法（一批客户端
/// 只认前者、另一批只认后者，见 `list_item` 里那段说明），所以必须同源覆盖：
/// 只改一个等于对一半客户端无效。`supports_video` / `supports_reasoning` 等
/// 其余位**不动** —— 这个功能只碰图片。
pub fn listed_item(model: &Value, provider_id: &str, declare: bool) -> Value {
    let mut item = list_item(model, provider_id);
    if !declare {
        return item;
    }
    let Some(object) = item.as_object_mut() else {
        return item;
    };
    object.insert("supports_images".to_string(), Value::Bool(true));
    // 顺序与 `list_item` 一致（text → image → video），下游若有按顺序取的逻辑
    // 不会因为这个覆盖而看到另一种排列
    let mut modalities = vec![Value::String("text".to_string())];
    modalities.push(Value::String("image".to_string()));
    if js_truthy(object.get("supports_video").unwrap_or(&Value::Null)) {
        modalities.push(Value::String("video".to_string()));
    }
    object.insert("input_modalities".to_string(), Value::Array(modalities));
    item
}

/// 请求体里所有图片块的「地址」（用于缓存键与日志；不改动 body）。
///
/// 认两种形态：`image_url` 块是 `image_url: {url}`（字符串形态也认 —— 少数
/// 客户端直接给 `image_url` 一个字符串）、`input_image` 块是顶层 `image_url`。
/// 地址可能是 `https://…` 或 `data:image/png;base64,…`，两种都原样取回。
pub fn image_sources(body: &Value) -> Vec<String> {
    image_blocks(body)
        .iter()
        .filter_map(|block| block.as_object().and_then(block_source))
        .collect()
}

/// 请求体里所有图片块的**原样副本**（按出现顺序）。
///
/// 发给识图模型时直接搬过去（见 [`describe_request`]）：网关在中间把 base64
/// 解出来再编回去只会丢掉媒体类型、放大体积，而上游本来就能读 `data:` URI。
pub fn image_blocks(body: &Value) -> Vec<Value> {
    let mut out = Vec::new();
    collect_blocks(body, &mut out);
    out
}

fn collect_blocks(value: &Value, out: &mut Vec<Value>) {
    match value {
        Value::Object(map) => {
            if is_image_block(map) {
                out.push(Value::Object(map.clone()));
                return;
            }
            for child in map.values() {
                collect_blocks(child, out);
            }
        }
        Value::Array(items) => {
            for child in items {
                collect_blocks(child, out);
            }
        }
        _ => {}
    }
}

/// 这个对象是不是图片块
fn is_image_block(map: &serde_json::Map<String, Value>) -> bool {
    map.get("type")
        .and_then(Value::as_str)
        .is_some_and(|ty| IMAGE_BLOCK_TYPES.contains(&ty))
}

/// 图片块的地址（取不到就给 `None`，调用方按「无图」处理）。
///
/// 两种形态都落在同一个键上，区别只在值的类型：
///   - Chat Completions：`{"type":"image_url","image_url":{"url":"..."}}`
///   - Responses：`{"type":"input_image","image_url":"..."}`
fn block_source(map: &serde_json::Map<String, Value>) -> Option<String> {
    match map.get("image_url")? {
        Value::String(url) => Some(url.clone()),
        Value::Object(object) => object.get("url").and_then(Value::as_str).map(str::to_string),
        _ => None,
    }
}

/// 描述缓存键：`sha256(模型 + 等级 + 全部图片地址)`。
///
/// 把模型与等级一起编进去是必须的：换一个识图模型（或抬一档思考）得到的是
/// **另一段**描述，用同一份缓存会让用户以为设置没生效。
pub fn cache_key(target: &Target, sources: &[String]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(target.model.as_bytes());
    hasher.update(b"\u{1}");
    hasher.update(target.effort.as_deref().unwrap_or("").as_bytes());
    for source in sources {
        hasher.update(b"\u{2}");
        hasher.update(source.as_bytes());
    }
    hex(&hasher.finalize())
}

/// 取缓存里的描述（未命中给 `None`）。
pub fn cached(key: &str) -> Option<String> {
    let mut guard = cache().lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    guard.entries.get(key).cloned()
}

/// 写入一条描述。**失败不要调它** —— 失败的描述一旦进缓存，用户重试时拿到的
/// 还是那句失败文案（与 magpie「fails not cached」同取舍）。
pub fn store(key: String, description: String) {
    if description.trim().is_empty() {
        return;
    }
    let mut guard = cache().lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    if guard.entries.contains_key(&key) {
        // 已有同键（并发两个请求各描述了一次）：后到者覆盖值，但不动插入顺序
        // —— FIFO 顺序只用于淘汰，重复插入不该把它顶到队尾
        guard.entries.insert(key, description);
        return;
    }
    guard.entries.insert(key.clone(), description);
    guard.order.push_back(key);
    while guard.order.len() > CACHE_CAPACITY {
        if let Some(old) = guard.order.pop_front() {
            guard.entries.remove(&old);
        }
    }
}

/// 构造发给识图模型的请求体（Chat 形态）。
///
/// `images` 是原图块数组（**原样**搬进 user 内容，不做任何转码）：识图模型
/// 自己知道怎么读 `data:` URI 与 `https://` 两种地址，网关在中间转一手只会
/// 丢掉媒体类型。
///
/// `stream` 写 `false`：调方（`UpstreamService::forward`）会按它自己那条
/// 规则把上游请求强制成流式再聚合回来，这里的值只表达「我要一个完整结果」。
pub fn describe_request(target: &Target, images: &[Value]) -> Value {
    let mut content: Vec<Value> = images.to_vec();
    content.push(json!({
        "type": "text",
        "text": "请按系统提示的规则，把上面这些图片的内容完整转成文字。",
    }));
    let mut body = json!({
        "model": target.model,
        "stream": false,
        "messages": [
            { "role": "system", "content": VISION_SYSTEM },
            { "role": "user", "content": content },
        ],
    });
    // 思考等级：只在用户显式设置时注入。用 OpenAI 通用的 `reasoning_effort`
    // 字段名 —— 认它的上游会抬高思考档，不认的会忽略（各家适配器另有自己的
    // 翻译逻辑，但识图请求走的是「模型名直发」，没有映射绑定可依，所以这里
    // 用最通用的那个字段）。
    if let Some(effort) = target.effort.as_deref() {
        if !effort.is_empty() {
            if let Some(object) = body.as_object_mut() {
                object.insert("reasoning_effort".to_string(), Value::String(effort.to_string()));
            }
        }
    }
    body
}

/// 从识图响应里取描述正文（取不到给 `None`）。
///
/// 只认 Chat 形态的 `choices[0].message.content`：识图请求恒走
/// `/v1/chat/completions` 那条链路（入口是 `UpstreamService::forward`），
/// 上游给的是 Chat 聚合体。`content` 是数组时（少数上游给分段）把 `text`
/// 段拼起来。
pub fn description_of(body: &Value) -> Option<String> {
    let content = body
        .get("choices")?
        .as_array()?
        .first()?
        .get("message")?
        .get("content")?;
    let text = match content {
        Value::String(text) => text.clone(),
        Value::Array(parts) => parts
            .iter()
            .filter_map(|part| {
                part.get("text")
                    .and_then(Value::as_str)
                    .or_else(|| part.as_str())
            })
            .collect::<Vec<_>>()
            .join("\n"),
        _ => return None,
    };
    let trimmed = text.trim();
    (!trimmed.is_empty()).then(|| trimmed.to_string())
}

/// 把请求体里的图片块全部替换成一段文字描述（原地改写 `body`）。
///
/// 返回被替换的块数（0 = 一个都没动，调用方据此决定是否记日志）。
///
/// ── 为什么所有图块替换成同一段描述 ──────────────────────────
/// 与 magpie 的 `seenBody()` 同做法：描述是**一次请求**拿回来的，它描述的是
/// 「这批图整体」而不是逐张。逐张描述要发 N 次上游（同一批图里常常是同一张
/// 截图的多页），成本与延迟都不划算，而主模型拿到的信息量相差无几。
///
/// ── 递归而不是只扫 `messages[].content` ─────────────────────
/// 图块可能嵌在更深处（工具结果里带的图、某些客户端的嵌套 content），只扫
/// 顶层会漏掉它们 —— 漏掉的图会原样发给纯文本模型，正是这个功能要避免的。
pub fn rewrite(body: &mut Value, description: &str) -> usize {
    let text = format!("{PLACEHOLDER_PREFIX}{description}");
    let mut replaced = 0usize;
    replace_in(body, &text, &mut replaced);
    replaced
}

fn replace_in(value: &mut Value, text: &str, replaced: &mut usize) {
    match value {
        Value::Object(map) => {
            if is_image_block(map) {
                // 整个块原地换成文本块：`image_url` / `input_image` 的其它键
                // （`detail` 之类）在文本块上没有意义，一并丢掉
                map.clear();
                map.insert("type".to_string(), Value::String("text".to_string()));
                map.insert("text".to_string(), Value::String(text.to_string()));
                *replaced += 1;
                return;
            }
            for child in map.values_mut() {
                replace_in(child, text, replaced);
            }
        }
        Value::Array(items) => {
            for child in items.iter_mut() {
                replace_in(child, text, replaced);
            }
        }
        _ => {}
    }
}

// ─── 进程内缓存 ─────────────────────────────────────────────

/// 描述缓存（FIFO 淘汰）。
///
/// 不引 `lru` 依赖（与本仓「零外部小依赖」的取向一致，见
/// `core::protocol::antigravity_signature_cache` 的同类说明）。严格 LRU 与
/// FIFO 的差别只在「最该淘汰哪一条」，而这个缓存的目的是**限制内存上限**、
/// 不是提高命中率，FIFO 用两张 std 容器就够。
struct DescriptionCache {
    entries: HashMap<String, String>,
    order: VecDeque<String>,
}

fn cache() -> &'static Mutex<DescriptionCache> {
    static CACHE: OnceLock<Mutex<DescriptionCache>> = OnceLock::new();
    CACHE.get_or_init(|| {
        Mutex::new(DescriptionCache {
            entries: HashMap::new(),
            order: VecDeque::new(),
        })
    })
}

/// 识图模型解析结果的缓存（单条 + TTL）。
struct PlanCache {
    key: String,
    at: Instant,
    target: Option<Target>,
}

fn plan_cache() -> &'static Mutex<Option<PlanCache>> {
    static PLAN: OnceLock<Mutex<Option<PlanCache>>> = OnceLock::new();
    PLAN.get_or_init(|| Mutex::new(None))
}

fn cached_plan(key: &str) -> Option<Option<Target>> {
    let guard = plan_cache()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let entry = guard.as_ref()?;
    (entry.key == key && entry.at.elapsed() < PLAN_TTL).then(|| entry.target.clone())
}

fn store_plan(key: String, target: Option<Target>) {
    let mut guard = plan_cache()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    *guard = Some(PlanCache { key, at: Instant::now(), target });
}

/// 作废解析缓存（配置刚改完时调，让紧随其后的回读看到新值）。
///
/// 严格来说**不必**：缓存 key 含配置值，配置一变 key 就变、自然不命中。但
/// `PUT /api/vision` 的响应体要回「当前生效的是谁」，那一刻配置已新、缓存
/// 还是旧的（key 不同所以会重算，本来也对）—— 这里清一次是给「同名 key 但
/// 目录变了」（用户刚加了账号、刚刷新了清单）留一条显式路径，也让语义不依赖
/// 「key 里编了配置」这个实现细节。
pub fn invalidate() {
    let mut guard = plan_cache()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    *guard = None;
}

/// 缓存键与日志用的短哈希（前 16 个十六进制字符够区分，日志里不必贴全长）
pub fn short_hash(value: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(value.as_bytes());
    hex(&hasher.finalize())[..16].to_string()
}

/// 字节 → 小写十六进制
fn hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push(DIGITS[(byte >> 4) as usize] as char);
        out.push(DIGITS[(byte & 0x0f) as usize] as char);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn chat_body(blocks: Value) -> Value {
        json!({
            "model": "deepseek-v4.1-flash",
            "messages": [{ "role": "user", "content": blocks }],
        })
    }

    #[test]
    fn sources_reads_both_block_shapes() {
        let body = chat_body(json!([
            { "type": "text", "text": "看图" },
            { "type": "image_url", "image_url": { "url": "https://a/1.png" } },
            { "type": "input_image", "image_url": "data:image/png;base64,AAA" },
        ]));
        assert_eq!(
            image_sources(&body),
            vec!["https://a/1.png".to_string(), "data:image/png;base64,AAA".to_string()]
        );
    }

    #[test]
    fn sources_ignores_text_only_body() {
        assert!(image_sources(&chat_body(json!([{ "type": "text", "text": "hi" }]))).is_empty());
    }

    #[test]
    fn rewrite_replaces_nested_image_blocks() {
        let mut body = json!({
            "model": "m",
            "messages": [{
                "role": "user",
                "content": [
                    { "type": "image_url", "image_url": { "url": "https://a/1.png" } },
                    { "type": "text", "text": "这是什么" }
                ]
            }],
        });
        assert_eq!(rewrite(&mut body, "一张截屏"), 1);
        let content = &body["messages"][0]["content"];
        assert_eq!(content[0]["type"], "text");
        assert_eq!(content[0]["text"], "[图片内容]\n一张截屏");
        // 原有的文本块不动
        assert_eq!(content[1]["text"], "这是什么");
    }

    #[test]
    fn rewrite_leaves_text_only_body_untouched() {
        let mut body = chat_body(json!([{ "type": "text", "text": "hi" }]));
        let before = body.clone();
        assert_eq!(rewrite(&mut body, "描述"), 0);
        assert_eq!(body, before);
    }

    #[test]
    fn description_prefers_string_content() {
        let body = json!({ "choices": [{ "message": { "content": "  描述  " } }] });
        assert_eq!(description_of(&body).as_deref(), Some("描述"));
    }

    #[test]
    fn description_joins_content_parts() {
        let body = json!({
            "choices": [{ "message": { "content": [
                { "type": "text", "text": "第一段" },
                { "type": "text", "text": "第二段" },
            ] } }],
        });
        assert_eq!(description_of(&body).as_deref(), Some("第一段\n第二段"));
    }

    #[test]
    fn description_of_empty_is_none() {
        let body = json!({ "choices": [{ "message": { "content": "   " } }] });
        assert_eq!(description_of(&body), None);
        assert_eq!(description_of(&json!({})), None);
    }

    #[test]
    fn cache_key_changes_with_model_and_effort() {
        let base = Target { model: "a".into(), provider: None, effort: None };
        let other = Target { model: "b".into(), provider: None, effort: None };
        let high = Target { model: "a".into(), provider: None, effort: Some("high".into()) };
        let images = vec!["data:x".to_string()];
        assert_ne!(cache_key(&base, &images), cache_key(&other, &images));
        assert_ne!(cache_key(&base, &images), cache_key(&high, &images));
        // 同一个模型同一批图 → 同一键（缓存能命中）
        assert_eq!(cache_key(&base, &images), cache_key(&base, &images));
    }

    #[test]
    fn listed_item_declares_images_only_when_asked() {
        let model = json!({ "id": "deepseek-v4.1-flash", "supportsImages": false });
        let plain = listed_item(&model, "workbuddy", false);
        assert_eq!(plain["supports_images"], Value::Bool(false));
        let declared = listed_item(&model, "workbuddy", true);
        assert_eq!(declared["supports_images"], Value::Bool(true));
        assert_eq!(
            declared["input_modalities"],
            json!(["text", "image"])
        );
    }

    #[test]
    fn listed_item_keeps_video_position() {
        let model = json!({ "id": "m", "supportsImages": true, "supportsVideo": true });
        let declared = listed_item(&model, "workbuddy", true);
        assert_eq!(declared["input_modalities"], json!(["text", "image", "video"]));
    }

    #[test]
    fn describe_request_carries_images_and_effort() {
        let target = Target {
            model: "glm-5.3v".into(),
            provider: None,
            effort: Some("high".into()),
        };
        let images = vec![json!({ "type": "image_url", "image_url": { "url": "https://a/1.png" } })];
        let body = describe_request(&target, &images);
        assert_eq!(body["model"], "glm-5.3v");
        assert_eq!(body["reasoning_effort"], "high");
        let content = body["messages"][1]["content"].as_array().unwrap();
        // 原图块原样在前，指令文本在后
        assert_eq!(content[0]["image_url"]["url"], "https://a/1.png");
        assert_eq!(content[1]["type"], "text");
    }
}
