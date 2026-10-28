//! 识图（图像识别 / Vision）配置：`GET/PUT /api/vision`、`GET /api/vision/models`。
//!
//! ── 这个功能做什么 ──────────────────────────────────────────
//! 客户端在**发送之前**就按 `/v1/models` 声明的能力决定要不要把图交出来：模型
//! 被标成纯文本，它根本不会请求网关，而是自己在本地装 OCR、把图丢掉、或提示
//! 「不支持图像输入」。开启本功能后网关做两件事：
//!
//!   ① 对**所有**模型声明「收图」（`core::vision::listed_item`）；
//!   ② 内部把图交给一个固定的视觉模型描述成文字，再让纯文本模型读到它
//!      （`core::upstream::vision_step`）。
//!
//! 机制、与 magpie 的对应关系、以及「为什么是钉住一个模型而不是轮询」见
//! `core::vision` 的模块头。
//!
//! ── 三个端点 ────────────────────────────────────────────────
//!   - `GET  /api/vision`        当前配置（设置页读）
//!   - `PUT  /api/vision`        写入配置（`{imageRecognition, imageRecognitionEffort}`）
//!   - `GET  /api/vision/models` 目录里**声明收图**的模型（设置页的候选下拉）
//!
//! 第三个端点是必要的：`<provider>/<model>` 这个形态必须能填对，而用户没有
//! 别的地方能看到「哪些模型声明收图」—— 让他手敲模型 id，写错一个字母功能就
//! 静默失效（`vision::compute` 校验不过就当没配）。候选表把这件事变成选择。
//!
//! ── 写盘位置 ────────────────────────────────────────────────
//! `config` 的 `imageRecognition` / `imageRecognitionEffort` 键（统一库 `kv`
//! 表里的配置行）。转发层与 `/v1/models` 出口逐请求读快照，改完下一个请求
//! 立即生效，不重启进程。

use axum::body::Bytes;
use axum::extract::State;
use axum::response::Response;
use serde_json::{json, Value};

use crate::server::config;
use crate::server::core::providers::catalog;
use crate::server::core::vision;
use crate::server::errors;
use crate::server::http::{ok_json, parse_body};
use crate::server::logging;
use crate::server::ServerState;

/// 点名的长度上限（`provider/model` 整串）。模型 id 再长也就几十字符，
/// 256 是「明显不是笔误就是恶意输入」的界。
const MAX_SELECTION_LEN: usize = 256;

/// GET /api/vision —— 当前配置
///
/// 除了两个键的原值，还回 `declaring` 与 `active`：
///   - `declaring`：对外是否正在声明「所有模型都收图」（= 这一步真的生效了）；
///   - `active`：当前**生效**的识图模型（`model` / `provider` / `effort`），
///     或 `null`。
///
/// 设置页据此决定是「可以开」还是「先加一个视觉账号」，也让用户一眼看到
/// auto 档最后挑中了谁 —— 不然「自动」是个黑盒，用户没法判断它选得对不对。
pub async fn get_vision(State(state): State<ServerState>) -> Response {
    ok_json(vision_json(state.store()))
}

/// PUT /api/vision —— body `{imageRecognition: "...", imageRecognitionEffort: "..."}`
///
/// 两个键都可选（只改一个就只传一个）。`imageRecognition` 的合法取值：
///   - `"off"`：关闭（不声明、不描述）；
///   - `""`：自动（网关挑一个声明收图的模型）；
///   - `"<provider>/<model>"`：点名。
///
/// 校验**不拒绝**「点名的模型不声明收图」之外的东西 —— 那种情况直接 400 并
/// 说明原因：它一定是个错误（要么家写错、要么模型名写错），而静默当没配会让
/// 用户以为功能坏了却不知道坏在哪。
pub async fn put_vision(State(state): State<ServerState>, body: Bytes) -> Response {
    let payload = match parse_body(&body) {
        Ok(value) => value,
        Err(error) => return errors::management_error(400, error.message),
    };
    let Some(object) = payload.as_object() else {
        return errors::management_error(400, "请求体必须是 JSON 对象");
    };
    let key = config::KEY_IMAGE_RECOGNITION;
    let effort_key = config::KEY_IMAGE_RECOGNITION_EFFORT;
    if object.get(key).is_none() && object.get(effort_key).is_none() {
        return errors::management_error(
            400,
            format!("缺少 {key}（或 {effort_key}）"),
        );
    }
    // ── 识图模型的选择 ─────────────────────────────────────────
    if let Some(value) = object.get(key) {
        let Some(text) = value.as_str() else {
            return errors::management_error(400, format!("{key} 必须是字符串"));
        };
        let text = text.trim();
        if text.len() > MAX_SELECTION_LEN {
            return errors::management_error(
                400,
                format!("{key} 过长（上限 {MAX_SELECTION_LEN} 字符）"),
            );
        }
        if let Err(message) = validate_selection(text) {
            return errors::management_error(400, message);
        }
        if !config::set_image_recognition(text) {
            logging::log("[Config]", "⚠️  识图设置写入失败，本次运行内仍生效");
        }
    }
    // ── 思考等级覆盖 ──────────────────────────────────────────
    if let Some(value) = object.get(effort_key) {
        let Some(text) = value.as_str() else {
            return errors::management_error(400, format!("{effort_key} 必须是字符串"));
        };
        if text.trim().len() > MAX_SELECTION_LEN {
            return errors::management_error(
                400,
                format!("{effort_key} 过长（上限 {MAX_SELECTION_LEN} 字符）"),
            );
        }
        if !config::set_image_recognition_effort(text.trim()) {
            logging::log("[Config]", "⚠️  识图思考等级写入失败，本次运行内仍生效");
        }
    }
    // 配置变了，先把解析缓存作废，让紧随其后的回读（下面那句 `vision_json`）
    // 反映的是新值 —— 否则用户保存后立刻看到的是 30 秒前的旧解析
    vision::invalidate();
    logging::log("[Config]", &describe_change(&vision_json(state.store())));
    ok_json(vision_json(state.store()))
}

/// GET /api/vision/models —— 目录里声明收图的模型（候选下拉）
///
/// 条目形态：`{"provider": "...", "providerLabel": "...", "model": "...", "name": "..."}`。
/// 只列**真的声明收图**的（`supportsImages` 为真）：这是「能用来识图」的定义，
/// 与 `vision::compute` 的校验同一判据，于是下拉里选得到的必然填得进配置。
///
/// `selected` 是当前配置值，前端据此高亮 —— 让「现在用的是哪个」一眼可见。
pub async fn list_vision_models(State(state): State<ServerState>) -> Response {
    let mut items: Vec<Value> = Vec::new();
    // beta.4 起内置家全部走插件宿主，`active_manifests` 的键已经是字符串 id
    // （不再是 `ProviderKind`），直接当提供商 id 用
    for (provider, manifest) in catalog::active_manifests(state.store()) {
        push_image_models(&mut items, &provider, &manifest);
    }
    for (provider, models) in crate::server::core::providers::custom::catalog_providers(state.store()) {
        push_image_models(&mut items, &provider, &models);
    }
    ok_json(json!({
        config::KEY_IMAGE_RECOGNITION: config::current().image_recognition(),
        "models": items,
    }))
}

/// 把一份清单里声明收图的条目追加进候选表
fn push_image_models(items: &mut Vec<Value>, provider: &str, manifest: &[Value]) {
    for item in manifest {
        if !crate::server::core::models::js_truthy(
            item.get("supportsImages").unwrap_or(&Value::Null),
        ) {
            continue;
        }
        let model = crate::server::core::models::model_id(item);
        if model.is_empty() {
            continue;
        }
        items.push(json!({
            "provider": provider,
            "providerLabel": crate::server::core::providers::label_of(provider),
            "model": model,
            "name": item.get("name").cloned().unwrap_or(Value::Null),
            // 设置页直接把这个值填进输入框，省掉前端再拼一次（拼错就是功能失效）
            "value": format!("{provider}/{model}"),
        }));
    }
}

/// 校验 `imageRecognition` 的取值（`Ok` = 可写入）
fn validate_selection(value: &str) -> Result<(), String> {
    // 关 / 自动：两个都是合法形态，不需要更多校验
    if value.is_empty() || value.eq_ignore_ascii_case("off") {
        return Ok(());
    }
    let Some((provider, model)) = value.split_once('/') else {
        return Err(format!(
            "识图模型要写成 provider/model（例如 workbuddy/glm-5.3），或用 \"off\" 关闭、留空自动选择；收到的是 {value}"
        ));
    };
    let (provider, model) = (provider.trim(), model.trim());
    if provider.is_empty() || model.is_empty() {
        return Err("provider 与 model 都不能为空".to_string());
    }
    // 家必须存在（内置或自定义）：不认识的家一定填错了，而静默当没配会让用户
    // 以为「保存成功了但功能没生效」—— 那是最难排查的一种状态
    let known = crate::server::core::providers::kind_from_id(provider).is_some()
        || crate::server::core::custom_providers::list()
            .iter()
            .any(|item| item.get("id").and_then(Value::as_str) == Some(provider));
    if !known {
        return Err(format!("没有这个提供商：{provider}"));
    }
    if !catalog::provider_supports_images(provider, model) {
        return Err(format!(
            "模型 {model} 在 {provider} 的清单里不存在，或未声明支持图片输入；候选见 GET /api/vision/models"
        ));
    }
    Ok(())
}

/// 配置的回读形态（GET 与 PUT 共用，前端直接用响应刷新界面）
fn vision_json(store: &crate::server::core::account_store::AccountStore) -> Value {
    // 名字刻意叫 `snapshot` 而不是 `config`：这个作用域里还要用模块路径
    // `config::KEY_...` 当 JSON 键，同名局部变量虽然不冲突（模块在类型命名
    // 空间、局部量在值命名空间），但读起来容易误以为遮蔽了模块
    let snapshot = config::current();
    // 当前**生效**的解析结果：auto 档下用户看不到「挑了谁」就只能靠猜，
    // 而这一行正好回答「现在描述图的是哪个模型」
    let active = vision::resolve(store).map(|target| {
        json!({
            "model": target.model,
            "provider": target.provider,
            "effort": target.effort,
        })
    });
    json!({
        config::KEY_IMAGE_RECOGNITION: snapshot.image_recognition(),
        config::KEY_IMAGE_RECOGNITION_EFFORT: snapshot.image_recognition_effort(),
        // 声明是否生效 —— 与 `/v1/models` 出口读的是同一个判定
        "declaring": active.is_some(),
        "active": active,
    })
}

/// 改完之后往运行日志写一行（设置类改动的统一做法）
fn describe_change(state: &Value) -> String {
    let selection = state
        .get(config::KEY_IMAGE_RECOGNITION)
        .and_then(Value::as_str)
        .unwrap_or("");
    // `active` 为 null 时说明这一档配置当前**不生效**（关着、点名的模型不可用、
    // 或 auto 但目录里没有收图的模型），三种情况的措辞不同
    let active_model = state
        .get("active")
        .and_then(|active| active.get("model"))
        .and_then(Value::as_str);
    if selection.eq_ignore_ascii_case("off") {
        return "识图已关闭：不再对下游声明收图，请求里的图片原样透传".to_string();
    }
    match (selection.is_empty(), active_model) {
        (true, Some(model)) => format!("识图已开启（自动）：由 {model} 描述图片"),
        (true, None) => {
            "识图已开启（自动），但目录里没有声明收图的模型 —— 当前不生效".to_string()
        }
        (false, Some(model)) => format!("识图已开启：固定由 {selection} 描述图片（{model}）"),
        (false, None) => {
            format!("识图配置已保存为 {selection}，但该模型不可用 —— 当前不生效")
        }
    }
}
