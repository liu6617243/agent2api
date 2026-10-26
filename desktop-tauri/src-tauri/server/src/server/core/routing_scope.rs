//! 将模型路由覆盖投影到候选快照；账号库的 priority 保持原样。

use serde_json::Value;

use crate::server::config::{self, RoutingProfile};
use super::routing;

pub fn providers_for(model: &str, available: &[String]) -> Vec<String> {
    let profiles = config::routing_profiles();
    match profiles.effective(model) {
        Some(profile) => profile.providers.iter()
            .filter(|id| available.contains(id) && !profile.disabled_providers.contains(id))
            .cloned().collect(),
        None => available.to_vec(),
    }
}

pub fn apply_accounts(accounts: &[Value], model: &str) -> Vec<Value> {
    let profiles = config::routing_profiles();
    match profiles.effective(model) {
        Some(profile) => project_accounts(accounts, profile),
        None => accounts.to_vec(),
    }
}

pub fn project_accounts(accounts: &[Value], profile: &RoutingProfile) -> Vec<Value> {
    let mut ordered = Vec::new();
    for provider in &profile.providers {
        if profile.disabled_providers.contains(provider) { continue; }
        let mut members: Vec<Value> = accounts.iter()
            .filter(|account| routing::provider_of(account) == provider)
            .filter(|account| routing::account_id(account)
                .is_some_and(|id| !profile.excluded_accounts.iter().any(|excluded| excluded == id)))
            .cloned().collect();
        members.sort_by_key(|account| (
            account.get("priority").and_then(Value::as_i64).unwrap_or(i64::MAX),
            account.get("addedAt").and_then(Value::as_i64).unwrap_or(0),
        ));
        if let Some(ids) = profile.account_order.get(provider) {
            members.sort_by_key(|account| ids.iter()
                .position(|id| Some(id.as_str()) == routing::account_id(account))
                .unwrap_or(usize::MAX));
        }
        ordered.extend(members);
    }
    // 仅供 plan_route 读取的投影位次；不会回写账号库。
    for (index, account) in ordered.iter_mut().enumerate() {
        if let Some(object) = account.as_object_mut() {
            object.insert("routingPriority".to_string(), Value::from(index));
        }
    }
    ordered
}

pub fn materialize_profile(accounts: &[Value], available: &[String], model: &str) -> RoutingProfile {
    let profiles = config::routing_profiles();
    let mut profile = profiles.effective(model).cloned().unwrap_or_else(|| RoutingProfile {
        providers: available.to_vec(),
        ..Default::default()
    });
    if profiles.model_profile(model).is_none() && !model.is_empty() {
        profile.providers.retain(|id| available.contains(id));
        profile.disabled_providers.retain(|id| profile.providers.contains(id));
        profile.account_order.retain(|provider, _| profile.providers.contains(provider));
    }
    for provider in &profile.providers {
        let mut members: Vec<&Value> = accounts.iter()
            .filter(|account| routing::provider_of(account) == provider).collect();
        members.sort_by_key(|account| (
            account.get("priority").and_then(Value::as_i64).unwrap_or(i64::MAX),
            account.get("addedAt").and_then(Value::as_i64).unwrap_or(0),
        ));
        let ids = profile.account_order.entry(provider.clone()).or_default();
        for account in members {
            if let Some(id) = routing::account_id(account) {
                if !ids.iter().any(|known| known == id) { ids.push(id.to_string()); }
            }
        }
    }
    profile
}
