//! 自定义提供商的目录、转发接线与余额查询。

use serde_json::Value;

use crate::server::core::account_store::AccountStore;
use crate::server::core::custom_providers;
use crate::server::core::key_scope::{self, KeyScope};
use crate::server::core::models::model_id;
use crate::server::core::vision;

/// 余额查询：七种方案（sub2api / NewAPI / DeepSeek / OpenCode Go / 商汤日日新 /
/// 智谱 / 智谱 z.ai 国际）的编排。方案由提供商记录的 `balanceScheme` 决定
/// （预置卡写入），查询凭证按账号存（`balanceQuery`）—— 详见各文件模块头。
pub mod balance;
/// 七种方案的上游响应解析（纯函数；与 `balance` 的请求链路分开，
/// 便于对照移植来源逐字段读）
pub mod balance_parse;
mod quota;
pub mod emulation;
pub mod forward;
/// 商汤日日新的账号密码登录（OAuth2 + PKCE 换控制台 JWT），只服务余额查询
pub mod sensenova_auth;

pub(crate) fn catalog_providers(store: &AccountStore) -> Vec<(String, Vec<Value>)> {
    custom_providers::list().into_iter().filter_map(|provider| {
        let id = provider.get("id")?.as_str()?.to_string();
        if store.accounts_for_provider(&id).is_empty() { return None; }
        let models = custom_providers::public_models(&provider);
        (!models.is_empty()).then_some((id, models))
    }).collect()
}

/// `declare_images` 与内置家同一个开关（由 `catalog::view::models_response`
/// 算一次后传进来）：**要声明就全套声明** —— 只给内置家声明、漏掉自定义家，
/// 用户在自定义家上贴图仍会被客户端在本地挡掉，等于功能对一半人无效。
pub fn append_models_response(
    store: &AccountStore,
    scope: Option<&KeyScope>,
    data: &mut Vec<Value>,
    declare_images: bool,
) {
    let mut claimed: std::collections::HashSet<_> = data.iter()
        .map(|item| model_id(item).to_lowercase()).collect();
    for (provider, models) in catalog_providers(store) {
        if !key_scope::allows_provider(scope, &provider) { continue; }
        for model in models {
            let id = model_id(&model);
            if key_scope::allows_model(scope, &id) && claimed.insert(id.to_lowercase()) {
                data.push(vision::listed_item(&model, &provider, declare_images));
            }
        }
    }
}
