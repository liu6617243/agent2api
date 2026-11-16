//! 记忆中枢（`/api/memory/*`）。
//!
//! ```text
//! GET  /api/memory/config      模块设置 + schema + 一句话状态
//! PUT  /api/memory/config      部分更新（只改传进来的键，扁平点号键）
//! POST /api/memory/config/reset 恢复默认（?keys=a.b,c.d 或全量）
//! GET  /api/memory/list        检索 / 列表（query 为空 = 按时间列）
//! GET  /api/memory/get         按 id 取全文（?ids=a,b 或 ?id=a）
//! GET  /api/memory/core        核心记忆文本（MCP `memory_core` 的同一份产出）
//! GET  /api/memory/digest      索引概览（≤200 行）
//! GET  /api/memory/timeline    事实演化链（?id=）
//! GET  /api/memory/projects    项目列表（含 l1/l2 计数与注册表字段）
//! GET  /api/memory/status      自检（含表行数、队列积压、设备 id）
//! GET  /api/memory/review      待确认队列（?status=pending&type=dedup）
//! POST /api/memory/review      裁决（{id, approve} 或 {rejectAll: true}）
//! GET  /api/memory/calls       内部 LLM 调用台账（?limit=100）
//! GET  /api/memory/agents      Agent 心跳（接入页的「最近活跃」）
//! POST /api/memory/write       写入一条记忆
//! POST /api/memory/update      更新标题/正文/标签/重要度/层/有效期
//! POST /api/memory/delete      进回收站
//! POST /api/memory/restore     从回收站恢复
//! POST /api/memory/purge       彻底删除（回收站里再删一次）
//! POST /api/memory/trash/purge 清空过期回收站（?days= 覆盖保留天数）
//! POST /api/memory/rebuild     重建派生索引
//! ```
//!
//! ── 为什么统一挂 protected ──────────────────────────────────
//! 记忆正文是「用户在别处说过的话、项目内部决策、踩坑记录」，敏感度与管理面
//! 同级（与 `/api/config`、`/api/accounts` 一致）。免鉴权会把它们暴露给任何
//! 能连到端口的人。全部端点都要求登录或网关 Key。
//!
//! ── 为什么读接口用 GET 而不是 POST ─────────────────────────
//! 列表/检索是可缓存的纯读，用 GET 才能被浏览器与排障工具按 URL 直接打开
//! （这类「看一眼」的接口最常被贴进 issue）。写入类一律 POST。
//!
//! ── 为什么 `GET /api/memory/config` 连 schema 一起发 ────────
//! 配置有一百多个旋钮，前端要是自己抄一份枚举，每次加旋钮都要改两处、还必然
//! 出现「后端有前端没有」的漂移。把 schema（类型/默认值/范围/分组/层级）
//! 直接下发，前端按 `kind` 渲染控件、按 `tier` 折叠，**永远不需要知道有哪些
//! 旋钮**。

use std::collections::HashMap;

use axum::body::Bytes;
use axum::extract::{Query, State};
use axum::response::Response;
use serde_json::{json, Value};

use crate::server::core::memory::{self, config as memory_config, MemoryKind, MemoryQuery};
use crate::server::errors;
use crate::server::http::{ok_json, parse_body};
use crate::server::ServerState;

fn flag(map: &HashMap<String, String>, key: &str) -> Option<bool> {
    map.get(key).map(|value| {
        let lowered = value.trim().to_ascii_lowercase();
        matches!(lowered.as_str(), "1" | "true" | "yes" | "on")
    })
}

fn number(map: &HashMap<String, String>, key: &str) -> Option<usize> {
    map.get(key).and_then(|value| value.trim().parse::<usize>().ok())
}

fn text(map: &HashMap<String, String>, key: &str) -> Option<String> {
    map.get(key)
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

pub async fn get_config(State(_state): State<ServerState>) -> Response {
    let settings = memory::config();
    let status = memory::status();
    ok_json(json!({
        "config": settings,
        // 扁平点号键 → 生效值（前端表单直接按它填）
        "values": memory_config::flat(),
        // 整份 schema：前端据此渲染控件、折叠层级
        "schema": memory_config::schema_json(),
        "status": status,
        // 界面上「能选哪些类别」由后端给，避免前端再抄一份枚举
        "kinds": MemoryKind::ALL.iter().map(|kind| json!({
            "value": kind.as_str(),
            "label": kind.label(),
            "prefersL2": kind.prefers_l2(),
        })).collect::<Vec<_>>(),
        // 层级枚举同理
        "layers": [
            { "value": "l1", "label": "原始日常" },
            { "value": "l2", "label": "沉淀结论" },
        ],
    }))
}

pub async fn put_config(State(_state): State<ServerState>, body: Bytes) -> Response {
    let payload = match parse_body(&body) {
        Ok(value) => value,
        Err(error) => return errors::management_error(400, error.message),
    };
    // 允许 `{config: {...}}` / `{values: {...}}` 与直接传补丁对象三种形状
    // （前端三种都发过：早期发 config，配置页发 values，脚本直接发扁平键）
    let patch = payload
        .get("config")
        .or_else(|| payload.get("values"))
        .cloned()
        .unwrap_or(payload);
    match memory::save_config(&patch) {
        Ok(next) => ok_json(json!({
            "config": next,
            "values": memory_config::flat(),
            "status": memory::status(),
        })),
        Err(message) => errors::management_error(400, message),
    }
}

pub async fn post_config_reset(State(_state): State<ServerState>, Query(params): Query<HashMap<String, String>>) -> Response {
    let keys: Vec<String> = text(&params, "keys")
        .map(|joined| joined.split(',').map(|item| item.trim().to_string()).filter(|item| !item.is_empty()).collect())
        .unwrap_or_default();
    match memory_config::reset(&keys) {
        Ok(_) => ok_json(json!({
            "values": memory_config::flat(),
            "config": memory::config(),
        })),
        Err(message) => errors::management_error(400, message),
    }
}

pub async fn get_list(State(_state): State<ServerState>, Query(params): Query<HashMap<String, String>>) -> Response {
    let query = MemoryQuery {
        text: text(&params, "query").or_else(|| text(&params, "q")).unwrap_or_default(),
        project: text(&params, "project"),
        agent: text(&params, "agent"),
        kind: text(&params, "type").or_else(|| text(&params, "kind")),
        layer: text(&params, "layer"),
        tag: text(&params, "tag"),
        starred: flag(&params, "starred"),
        pinned: flag(&params, "pinned"),
        include_superseded: flag(&params, "includeSuperseded").unwrap_or(false),
        include_deleted: flag(&params, "includeDeleted").unwrap_or(false),
        include_expired: flag(&params, "includeExpired").unwrap_or(false),
        limit: number(&params, "limit").unwrap_or_else(|| memory_config::usize_of("ui.pageSize").max(1)),
    };
    ok_json(memory::list(&query))
}

pub async fn get_one(State(_state): State<ServerState>, Query(params): Query<HashMap<String, String>>) -> Response {
    let ids: Vec<String> = text(&params, "ids")
        .map(|joined| {
            joined
                .split(',')
                .map(|item| item.trim().to_string())
                .filter(|item| !item.is_empty())
                .collect()
        })
        .or_else(|| text(&params, "id").map(|id| vec![id]))
        .unwrap_or_default();
    if ids.is_empty() {
        return errors::management_error(400, "缺少 id");
    }
    let limit = number(&params, "bodyLimit").unwrap_or(6000);
    ok_json(memory::get(&ids, limit))
}

pub async fn get_core(State(_state): State<ServerState>, Query(params): Query<HashMap<String, String>>) -> Response {
    let project = text(&params, "project");
    let tokens = number(&params, "maxTokens").unwrap_or_else(|| memory::config().core_max_tokens);
    ok_json(memory::core_text(project.as_deref(), tokens))
}

pub async fn get_digest(State(_state): State<ServerState>, Query(params): Query<HashMap<String, String>>) -> Response {
    ok_json(memory::digest_text(number(&params, "maxLines").unwrap_or(200)))
}

pub async fn get_timeline(State(_state): State<ServerState>, Query(params): Query<HashMap<String, String>>) -> Response {
    let Some(id) = text(&params, "id") else {
        return errors::management_error(400, "缺少 id");
    };
    ok_json(memory::timeline(&id))
}

pub async fn get_projects(State(_state): State<ServerState>) -> Response {
    ok_json(memory::projects())
}

pub async fn get_status(State(_state): State<ServerState>) -> Response {
    ok_json(memory::status())
}

pub async fn get_review(State(_state): State<ServerState>, Query(params): Query<HashMap<String, String>>) -> Response {
    let status = text(&params, "status");
    let kind = text(&params, "type").or_else(|| text(&params, "kind"));
    ok_json(memory::review_list(
        status.as_deref(),
        kind.as_deref(),
        number(&params, "limit").unwrap_or(200),
    ))
}

pub async fn post_review(State(_state): State<ServerState>, body: Bytes) -> Response {
    let payload = match parse_body(&body) {
        Ok(value) => value,
        Err(error) => return errors::management_error(400, error.message),
    };
    // 批量拒绝（「全部忽略」）：不提供批量批准 —— 一批建议里几乎必然有一条是错的
    if payload.get("rejectAll").and_then(Value::as_bool).unwrap_or(false) {
        let kind = payload.get("type").and_then(Value::as_str);
        return ok_json(memory::review_reject_all(kind));
    }
    let Some(id) = payload.get("id").and_then(Value::as_i64) else {
        return errors::management_error(400, "缺少 id");
    };
    let approve = payload.get("approve").and_then(Value::as_bool).unwrap_or(false);
    match memory::review_resolve(id, approve) {
        Ok(value) => ok_json(value),
        Err(message) => errors::management_error(400, message),
    }
}

pub async fn get_calls(State(_state): State<ServerState>, Query(params): Query<HashMap<String, String>>) -> Response {
    let limit = number(&params, "limit").unwrap_or(100);
    let rows = crate::server::core::memory::index::global().recent_llm(limit);
    ok_json(json!({
        "items": rows,
        "tokensToday": crate::server::core::memory::index::global().tokens_today(),
        "dailyLimit": memory_config::usize_of("auto.dailyTokenLimit"),
    }))
}

pub async fn get_agents(State(_state): State<ServerState>) -> Response {
    let beats = crate::server::core::memory::index::global().beats();
    ok_json(json!({
        "items": beats,
        "enabled": memory_config::strings_of("agents.enabled"),
    }))
}

pub async fn post_write(State(_state): State<ServerState>, body: Bytes) -> Response {
    let payload = match parse_body(&body) {
        Ok(value) => value,
        Err(error) => return errors::management_error(400, error.message),
    };
    let input = memory::WriteInput {
        content: payload.get("content").and_then(Value::as_str).unwrap_or("").to_string(),
        title: payload.get("title").and_then(Value::as_str).map(str::to_string),
        kind: payload.get("type").and_then(Value::as_str).map(str::to_string),
        layer: payload.get("layer").and_then(Value::as_str).map(str::to_string),
        project: payload.get("project").and_then(Value::as_str).map(str::to_string),
        agent: Some(
            payload
                .get("agent")
                .and_then(Value::as_str)
                .unwrap_or("manual")
                .to_string(),
        ),
        tags: strings(payload.get("tags")),
        importance: payload.get("importance").and_then(Value::as_f64),
        files: strings(payload.get("files")),
        supersedes: strings(payload.get("supersedes")),
        pinned: payload.get("pinned").and_then(Value::as_bool),
        starred: payload.get("starred").and_then(Value::as_bool),
        cwd: payload.get("cwd").and_then(Value::as_str).map(str::to_string),
        session: payload.get("session").and_then(Value::as_str).map(str::to_string),
        valid_to: payload.get("validTo").and_then(Value::as_i64).filter(|stamp| *stamp > 0),
        evidence: strings(payload.get("evidence")),
    };
    match memory::write(input) {
        Ok(value) => ok_json(value),
        Err(message) => errors::management_error(400, message),
    }
}

pub async fn post_update(State(_state): State<ServerState>, body: Bytes) -> Response {
    let payload = match parse_body(&body) {
        Ok(value) => value,
        Err(error) => return errors::management_error(400, error.message),
    };
    let Some(id) = payload.get("id").and_then(Value::as_str) else {
        return errors::management_error(400, "缺少 id");
    };
    match memory::update(id, &payload) {
        Ok(value) => ok_json(value),
        Err(message) => errors::management_error(400, message),
    }
}

pub async fn post_delete(State(_state): State<ServerState>, body: Bytes) -> Response {
    with_id(body, memory::remove)
}

pub async fn post_restore(State(_state): State<ServerState>, body: Bytes) -> Response {
    with_id(body, memory::restore)
}

pub async fn post_purge(State(_state): State<ServerState>, body: Bytes) -> Response {
    with_id(body, memory::purge)
}

pub async fn post_trash_purge(State(_state): State<ServerState>, Query(params): Query<HashMap<String, String>>) -> Response {
    // `?days=` 覆盖保留天数：接口不提供「无条件全清」，那是不可恢复的误操作入口
    if let Some(days) = number(&params, "days") {
        memory_config::patch(&json!({ "storage.trashKeepDays": days })).ok();
    }
    ok_json(memory::purge_trash())
}

pub async fn post_rebuild(State(_state): State<ServerState>) -> Response {
    match memory::rebuild() {
        Ok(count) => ok_json(json!({ "rebuilt": count, "status": memory::status() })),
        Err(message) => errors::management_error(500, message),
    }
}

// ── 自动化 ───────────────────────────────────────────────────

/// 自动化页顶部：调度线程状态 + 11 个任务的节奏与上次执行。
pub async fn get_auto(State(_state): State<ServerState>) -> Response {
    ok_json(memory::schedule::status())
}

/// 手动执行一个任务。**同步等待结果**：用户点了「立即执行」就想看它跑成什么样，
/// 返回一个 taskId 让他再刷新一次是多余的一步。
pub async fn post_auto_run(State(_state): State<ServerState>, body: Bytes) -> Response {
    let payload = match parse_body(&body) {
        Ok(value) => value,
        Err(error) => return errors::management_error(400, error.message),
    };
    let Some(task) = payload.get("task").and_then(Value::as_str) else {
        return errors::management_error(400, "缺少 task");
    };
    if !memory::schedule::TASKS.contains(&task) {
        return errors::management_error(
            400,
            format!("未知任务：{task}（可选 {}）", memory::schedule::TASKS.join(" / ")),
        );
    }
    let project = payload.get("project").and_then(Value::as_str).map(str::to_string);
    // 任务里可能同步等模型（最长 60s×N 次），不能在异步上下文里直接阻塞
    let task_owned = task.to_string();
    let outcome = crate::server::core::blocking::run(move || {
        memory::schedule::run_now(&task_owned, project.as_deref())
    })
    .await;
    match outcome {
        Ok(outcome) => ok_json(json!({ "task": task, "outcome": outcome.to_json() })),
        Err(error) => errors::management_error(500, error.message),
    }
}

// ── 画像 ─────────────────────────────────────────────────────

/// 画像页：现有画像条目 + 生成门槛状态。
pub async fn get_profile(State(_state): State<ServerState>) -> Response {
    ok_json(memory::profile_view())
}

/// 强制重生成画像（不等门槛）。同步等待模型。
pub async fn post_profile_build(State(_state): State<ServerState>) -> Response {
    let outcome = crate::server::core::blocking::run(|| {
        memory::schedule::run_now("profile", None)
    })
    .await;
    match outcome {
        Ok(outcome) => ok_json(json!({ "outcome": outcome.to_json(), "profile": memory::profile_view() })),
        Err(error) => errors::management_error(500, error.message),
    }
}

// ── 体检 ─────────────────────────────────────────────────────

pub async fn get_health(State(_state): State<ServerState>, Query(params): Query<HashMap<String, String>>) -> Response {
    ok_json(memory::health(number(&params, "limit").unwrap_or(50)))
}

fn with_id(body: Bytes, handler: fn(&str) -> Result<Value, String>) -> Response {
    let payload = match parse_body(&body) {
        Ok(value) => value,
        Err(error) => return errors::management_error(400, error.message),
    };
    let Some(id) = payload.get("id").and_then(Value::as_str) else {
        return errors::management_error(400, "缺少 id");
    };
    match handler(id) {
        Ok(value) => ok_json(value),
        Err(message) => errors::management_error(400, message),
    }
}

fn strings(value: Option<&Value>) -> Vec<String> {
    value
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(|item| item.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default()
}