//! 聚合模型目录：提供商的原始清单、对外绑定及发送名解析。
//!
//! 原始模型 ID 是默认绑定，别名是额外绑定。各绑定独立开关；关闭默认绑定
//! 不会禁用它指向的上游模型，也不会影响仍开启的别名。
//! 清单来源在本文件，绑定解析在 routing.rs，对外与管理视图在 view.rs。

mod routing;
mod view;

use serde_json::Value;

use crate::server::core::account_store::AccountStore;
use crate::server::core::custom_providers;
use crate::server::core::model_meta;
use crate::server::core::model_rules;
use crate::server::core::models::{js_truthy, model_id};
use crate::server::core::providers::adapter::adapter_by_id;
use crate::server::core::providers::{kind_from_id, kind_id, ProviderKind, PROVIDERS};

pub use routing::{
    default_model_catalog, default_model_usable, forwarding_providers, model_blocked_everywhere,
    providers_for_model, wire_target_for_provider, WireTarget,
};
pub use view::{
    advertised_manifest_contains, advertised_model_ids, has_available_providers, manage_view,
    models_by_provider, models_response, session_models, suggest_advertised,
};

/// 原始能力清单不应用绑定开关；关闭原始 ID 后，别名仍需解析到这条上游记录。
/// 用户自建模型、models.dev 补齐与能力覆盖都在这一处应用，插件与内置身份共用。
pub(crate) fn manifest_for_provider(provider: &str) -> Vec<Value> {
    let Some(adapter) = adapter_by_id(provider) else {
        return Vec::new();
    };
    let mut models = adapter.list_models();
    for item in model_rules::custom_models_for(provider) {
        let id = model_id(&item);
        if !id.is_empty()
            && !models.iter().any(|existing| model_id(existing).eq_ignore_ascii_case(&id))
        {
            models.push(item);
        }
    }
    for item in models.iter_mut() {
        model_meta::apply(item);
    }
    model_rules::apply_capability_overrides(provider, &mut models);
    models
}

/// 编译期身份的调用方仍按 kind 查询，执行体优先取已安装的插件。
pub(crate) fn manifest_for(kind: ProviderKind) -> Vec<Value> {
    manifest_for_provider(kind_id(kind))
}

/// 旧内置目录已迁入插件，网关不再持有按 kind 划分的刷新元数据。
pub(crate) fn refresh_meta(_kind: ProviderKind) -> (bool, i64) {
    (false, 0)
}

pub(crate) fn refresh_meta_for_provider(provider: &str) -> (bool, i64) {
    if crate::server::core::plugin_host::serves_provider(provider) {
        let meta = crate::server::core::plugin_host::model_catalog_meta(provider);
        return (meta.remote, meta.refreshed_at);
    }
    kind_from_id(provider).map(refresh_meta).unwrap_or_default()
}

fn all_provider_ids() -> Vec<String> {
    let mut ids = crate::server::core::plugin_host::installed_ids();
    for provider in PROVIDERS {
        if !ids.iter().any(|id| id.eq_ignore_ascii_case(provider.id)) {
            ids.push(provider.id.to_string());
        }
    }
    ids
}

pub fn provider_available(store: &AccountStore, provider: &str) -> bool {
    !store.accounts_for_provider(provider).is_empty()
}

/// 管理视图需要完整清单；对外视图在此基础上再应用每条绑定的开关。
pub(crate) fn managed_manifests(store: &AccountStore) -> Vec<(String, Vec<Value>)> {
    all_provider_ids()
        .into_iter()
        .filter(|provider| provider_available(store, provider))
        .map(|provider| {
            let manifest = manifest_for_provider(&provider);
            (provider, manifest)
        })
        .collect()
}

pub fn active_manifests(store: &AccountStore) -> Vec<(String, Vec<Value>)> {
    managed_manifests(store).into_iter().filter_map(|(provider, manifest)| {
        let manifest = adapter_by_id(&provider)
            .map(|adapter| adapter.advertise_models(store, manifest))
            .unwrap_or_default();
        (!manifest.is_empty()).then_some((provider, manifest))
    }).collect()
}

/// 某家在本进程里承载 `entry_model` 时，是否声明支持图片输入。
///
/// ── 为什么要先解析「该家实际发送的 target」────────────────────
/// 一个对外的请求名（映射别名）在**承载它的那些家**的清单里通常不存在：
/// 别名 `deepseek-v4.1-flash` 由三条映射分别指向 `glm-5.3-flash`、
/// `qwen3.8-flash`、`deepseek-v4.1-flash`，而 glm / qwen 两家的目录里根本
/// 没有 `deepseek-v4.1-flash` 这个名字。直接拿入口名去查，这两家必然查不到
/// 条目、返回 false。
///
/// 因此先经 [`routing::wire_target_for_provider`] 解析出「**这一家实际会发送
/// 的那个模型名**」（与发送侧改写同源），再拿它去这家的清单里查
/// `supportsImages`。
///
/// 认不出的家 / 认不出的 target → `false`（保守：不声称有视觉）。
///
/// ── 与 3.1.0 那条线的差别 ───────────────────────────────────
/// 老实现按 `ProviderKind` 取清单（`manifest_for(kind)`）；beta.4 已把内置家
/// 全部迁到插件宿主，清单按**字符串 id** 取（`manifest_for_provider`），所以
/// 这里不再需要 kind 分支。
pub fn provider_supports_images(provider_id: &str, entry_model: &str) -> bool {
    if custom_providers::is_custom_provider_id(provider_id) {
        // 自定义家：存储条目里能力位嵌在 `capabilities` 对象里，平铺的
        // `supportsImages` 只在对外条目上 —— 走 `public_models`（与
        // `/v1/models` 同源：登记覆盖 > models.dev > 未声明）。自定义家没有
        // `wire_target_for_provider` 那套改写，别名条目的能力本来取自 target，
        // 直接按来电名比对即可。
        return custom_provider_supports_images(provider_id, entry_model);
    }
    // 认不出的家走 `manifest_for_provider` 的空清单 → `false`，不必单独判 ——
    // 那个函数里 `adapter_by_id` 查不到就返回空 Vec（beta.4 的家身份由适配器
    // 注册决定，`PROVIDERS` 只是编译期遗留的静态表）
    let target = routing::wire_target_for_provider(entry_model, provider_id, None);
    item_supports_images(&manifest_for_provider(provider_id), &target.model)
}

/// 在一条清单里按模型 id 定位条目，取它的 `supportsImages`（与 `/v1/models`
/// 出口同一真值口径，见 `models::js_truthy`）。
fn item_supports_images(manifest: &[Value], model: &str) -> bool {
    manifest
        .iter()
        .find(|item| model_id(item).eq_ignore_ascii_case(model))
        .and_then(|item| item.get("supportsImages"))
        .map(js_truthy)
        .unwrap_or(false)
}

/// 自定义家：能力位判读与内置家同源 —— 读 `custom_providers::public_models`
/// 产出的**对外条目**（`/v1/models` 就发这份），而不是原始存储。
fn custom_provider_supports_images(provider_id: &str, model: &str) -> bool {
    let Some(provider) = custom_providers::list().into_iter().find(|provider| {
        provider.get("id").and_then(Value::as_str) == Some(provider_id)
    }) else {
        return false;
    };
    item_supports_images(&custom_providers::public_models(&provider), model)
}

fn aggregate_source(active: &[(String, Vec<Value>)]) -> (&'static str, i64) {
    match active {
        [(provider, _)] => {
            let (remote, refreshed_at) = refresh_meta_for_provider(provider);
            (if remote { "remote" } else { "builtin" }, refreshed_at)
        }
        [] => ("none", 0),
        _ => ("aggregate", active.first().map(|(provider, _)| refresh_meta_for_provider(provider).1).unwrap_or(0)),
    }
}
