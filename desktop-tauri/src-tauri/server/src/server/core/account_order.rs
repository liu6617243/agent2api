use std::cmp::Ordering;
use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex, OnceLock};

use serde::Serialize;
use serde_json::{json, Value};

use crate::server::config::{self, AccountSortRule, AccountSortSettings, AddedSortDirection, CreditSortDirection};
use super::{routing, usage_records::UsageRecord};

type CreditFacts = HashMap<String, Arc<Vec<i64>>>;

fn credit_slot() -> &'static Mutex<CreditFacts> {
    static FACTS: OnceLock<Mutex<CreditFacts>> = OnceLock::new();
    FACTS.get_or_init(|| Mutex::new(HashMap::new()))
}

fn number(value: &Value) -> Option<f64> {
    value.as_f64().or_else(|| value.as_str()?.trim().parse().ok())
        .filter(|value| value.is_finite())
}

fn credit_expiries(usage: &Value) -> Vec<i64> {
    let mut expiries: Vec<i64> = usage.get("credits").and_then(Value::as_array)
        .into_iter().flatten().filter_map(|pack| {
            if ["snapshot", "record", "daily", "unlimited"].iter().any(|key| pack.get(key).and_then(Value::as_bool) == Some(true)) {
                return None;
            }
            if pack.get("remain").and_then(number).is_some_and(|remain| remain <= 0.0) { return None; }
            let expiry = pack.get("expireAt").and_then(number)?;
            (expiry > 0.0).then_some(expiry as i64)
        }).collect();
    expiries.sort_unstable();
    expiries.dedup();
    expiries
}

pub fn note_usage(id: &str, usage: &Value) {
    if let Ok(mut facts) = credit_slot().lock() { facts.insert(id.to_string(), Arc::new(credit_expiries(usage))); }
}

pub fn reload_usage(records: &HashMap<String, UsageRecord>) {
    let fresh = records.iter().filter_map(|(id, record)| {
        record.usage.as_ref().map(|usage| (id.clone(), Arc::new(credit_expiries(usage))))
    }).collect();
    if let Ok(mut facts) = credit_slot().lock() { *facts = fresh; }
}

pub fn supports_credit_expiry(provider: &str) -> bool {
    super::plugin_host::account_features_of(provider).map(|features| features.credit_expiry)
        .unwrap_or_else(|| matches!(provider, "workbuddy" | "workbuddy-intl" | "trae" | "officeace"))
}

/// 一次性取「账号 id → 最早**未过期**积分到期时刻」（毫秒）的快照。
///
/// 返回表里 `None` 与「表里没有这个 id」是同一件事：**测不出有效期**。
/// 三种情况都归到 `None`，语义上也确实都测不出：
///   1. 该家不支持积分到期（`supports_credit_expiry` 为假，从未写过事实）；
///   2. 事实是空表（没有可用的积分包，或积分包都是 unlimited / 快照 / 按日额度，
///      见 [`credit_expiries`] 过滤掉的那几类）；
///   3. 事实里的到期时刻全部已过（积分已作废，不再为它排队）。
///
/// ── 为什么选路也读这一份，而不是自己解析 `usage.credits` ─────────
/// 账号页「积分到期」列显示的到期时刻（`sorting` 里的 `creditExpiresAt`）与
/// 这里出自同一个函数、同一份内存事实。若选路另写一套解析，界面说「3 天后
/// 到期」而转发把该账号当「测不出」排在后面 —— 用户看到的就是自相矛盾。
///
/// 锁只在这一层持有：调用方拿到快照后随意遍历，不在持锁期间做排序。账号
/// 规模是几十量级，整表克隆（值都是 `Option<i64>`）比逐账号取锁更划算。
pub fn credit_expiries_at(now: i64) -> HashMap<String, Option<i64>> {
    let Ok(facts) = credit_slot().lock() else {
        return HashMap::new();
    };
    facts
        .iter()
        .map(|(id, expiries)| {
            let soonest = expiries.iter().copied().find(|expiry| *expiry > now);
            (id.clone(), soonest)
        })
        .collect()
}

pub fn rules_for(provider: &str, settings: &AccountSortSettings) -> Vec<AccountSortRule> {
    settings.get(provider).cloned().unwrap_or_else(|| vec![
        AccountSortRule::Credit { on: supports_credit_expiry(provider), dir: CreditSortDirection::Soon },
        AccountSortRule::Added { on: false, dir: AddedSortDirection::New },
    ])
}

pub fn manual_groups(accounts: &[Value]) -> Vec<(String, Vec<Value>)> {
    let mut accounts = accounts.to_vec();
    accounts.sort_by_key(|account| (
        account.get("priority").and_then(Value::as_i64).unwrap_or(i64::MAX),
        account.get("addedAt").and_then(Value::as_i64).unwrap_or(0),
    ));
    let mut groups: Vec<(String, Vec<Value>)> = Vec::new();
    for account in accounts {
        let provider = routing::provider_of(&account).to_string();
        match groups.iter_mut().find(|(id, _)| *id == provider) {
            Some((_, members)) => members.push(account),
            None => groups.push((provider, vec![account])),
        }
    }
    groups
}

fn compare_expiry(left: Option<i64>, right: Option<i64>, direction: CreditSortDirection) -> Ordering {
    match (left, right) {
        (Some(left), Some(right)) => match direction {
            CreditSortDirection::Soon => left.cmp(&right),
            CreditSortDirection::Late => right.cmp(&left),
        },
        (Some(_), None) => Ordering::Less,
        (None, Some(_)) => Ordering::Greater,
        (None, None) => Ordering::Equal,
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AccountSorting {
    pub updated_at: i64,
    pub configured: AccountSortSettings,
    pub rules: AccountSortSettings,
    pub provider_order: Vec<String>,
    pub account_order: BTreeMap<String, Vec<String>>,
    pub credit_expires_at: BTreeMap<String, Option<i64>>,
}

pub fn sorting(accounts: &[Value], now: i64) -> AccountSorting {
    let configured = config::account_sort_settings();
    let mut result = AccountSorting {
        updated_at: now,
        configured: (*configured).clone(), rules: BTreeMap::new(), provider_order: Vec::new(),
        account_order: BTreeMap::new(), credit_expires_at: BTreeMap::new(),
    };
    let facts = credit_slot().lock().map(|facts| facts.clone()).unwrap_or_default();
    for (provider, mut members) in manual_groups(accounts) {
        let credit_supported = supports_credit_expiry(&provider);
        let rules = rules_for(&provider, &configured);
        for account in &members {
            if let Some(id) = routing::account_id(account) {
                let expiry = facts.get(id).and_then(|expiries| expiries.iter().find(|expiry| **expiry > now)).copied();
                result.credit_expires_at.insert(id.to_string(), expiry);
            }
        }
        members.sort_by(|left, right| {
            for rule in &rules {
                let order = match rule {
                    AccountSortRule::Credit { on: true, dir } if credit_supported => {
                        let expiry = |account: &Value| routing::account_id(account)
                            .and_then(|id| result.credit_expires_at.get(id)).copied().flatten();
                        compare_expiry(expiry(left), expiry(right), *dir)
                    }
                    AccountSortRule::Added { on: true, dir } => {
                        let added = |account: &Value| account.get("addedAt").and_then(number).unwrap_or(0.0);
                        let order = added(left).total_cmp(&added(right));
                        match dir { AddedSortDirection::Old => order, AddedSortDirection::New => order.reverse() }
                    }
                    _ => Ordering::Equal,
                };
                if order != Ordering::Equal { return order; }
            }
            Ordering::Equal
        });
        result.provider_order.push(provider.clone());
        result.account_order.insert(provider.clone(), members.iter().filter_map(|account| routing::account_id(account).map(str::to_string)).collect());
        result.rules.insert(provider, rules);
    }
    result
}

pub fn ordered_accounts(accounts: &[Value], sorting: &AccountSorting) -> Vec<Value> {
    let by_id: HashMap<&str, &Value> = accounts.iter().filter_map(|account| routing::account_id(account).map(|id| (id, account))).collect();
    sorting.provider_order.iter().flat_map(|provider| sorting.account_order.get(provider).into_iter().flatten())
        .filter_map(|id| by_id.get(id.as_str()).map(|account| (*account).clone())).collect()
}

pub fn enrich_snapshot(snapshot: &mut Value) {
    let accounts = routing::accounts_of(snapshot);
    let sorting = sorting(&accounts, crate::server::logging::now_ms());
    snapshot["sorting"] = json!(sorting);
}
