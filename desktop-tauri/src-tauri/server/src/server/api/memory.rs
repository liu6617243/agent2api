//! 记忆中枢（`/api/memory/*`）。
//!
//! ```text
//! GET  /api/memory/config      模块设置 + 一句话状态
//! PUT  /api/memory/config      部分更新（只改传进来的键）
//! GET  /api/memory/list        检索 / 列表（query 为空 = 按时间列）
//! GET  /api/memory/get         按 id 取全文（?ids=a,b 或 ?id=a）
//! GET  /api/memory/core        核心记忆文本（注入内容预览）
//! GET  /api/memory/digest      索引概览（≤200 行）
//! GET  /api/memory/timeline    事实演化链（?id=）
//! GET  /api/memory/projects    项目列表
//! GET  /api/memory/status      自检
//! POST /api/memory/write       写入一条记忆
//! POST /api/memory/update      更新标题/正文/标签/重要度
//! POST /api/memory/delete      进回收站
//! POST /api/memory/restore     从回收站恢复
//! POST /api/memory/purge       彻底删除（回收站里再删一次）
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

use std::collections::HashMap;

use axum::body::Bytes;
use axum::extract::{Query, State};
use axum::response::Response;
use serde_json::{json, Value};

use crate::server::core::memory::{self, model::MemoryKind, Query as MemoryQuery};
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
        "status": status,
        // 界面上「能选哪些类别」由后端给，避免前端再抄一份枚举
        "kinds": MemoryKind::ALL.iter().map(|kind| json!({
            "value": kind.as_str(),
            "label": kind.label(),
        })).collect::<Vec<_>>(),
    }))
}

pub async fn put_config(State(_state): State<ServerState>, body: Bytes) -> Response {
    let payload = match parse_body(&body) {
        Ok(value) => value,
        Err(error) => return errors::management_error(400, error.message),
    };
    // 允许 `{config: {...}}` 与直接传补丁对象两种形状（前端两种都发过）
    let patch = payload.get("config").cloned().unwrap_or(payload);
    match memory::save_config(&patch) {
        Ok(next) => ok_json(json!({ "config": next })),
        Err(message) => errors::management_error(400, message),
    }
}

pub async fn get_list(State(_state): State<ServerState>, Query(params): Query<HashMap<String, String>>) -> Response {
    let query = MemoryQuery {
        text: text(&params, "query").or_else(|| text(&params, "q")).unwrap_or_default(),
        project: text(&params, "project"),
        agent: text(&params, "agent"),
        kind: text(&params, "type").or_else(|| text(&params, "kind")),
        tag: text(&params, "tag"),
        starred: flag(&params, "starred"),
        pinned: flag(&params, "pinned"),
        include_superseded: flag(&params, "includeSuperseded").unwrap_or(false),
        include_deleted: flag(&params, "includeDeleted").unwrap_or(false),
        limit: number(&params, "limit").unwrap_or(30),
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
    ok_json(memory::get(&ids, 6000))
}

pub async fn get_core(State(_state): State<ServerState>, Query(params): Query<HashMap<String, String>>) -> Response {
    let project = text(&params, "project");
    let tokens = number(&params, "maxTokens").unwrap_or_else(|| memory::config().inject_tokens);
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

pub async fn post_write(State(_state): State<ServerState>, body: Bytes) -> Response {
    let payload = match parse_body(&body) {
        Ok(value) => value,
        Err(error) => return errors::management_error(400, error.message),
    };
    let input = memory::WriteInput {
        content: payload.get("content").and_then(Value::as_str).unwrap_or("").to_string(),
        title: payload.get("title").and_then(Value::as_str).map(str::to_string),
        kind: payload.get("type").and_then(Value::as_str).map(str::to_string),
        project: payload.get("project").and_then(Value::as_str).map(str::to_string),
        agent: Some("manual".to_string()),
        tags: strings(payload.get("tags")),
        importance: payload.get("importance").and_then(Value::as_f64),
        files: strings(payload.get("files")),
        supersedes: strings(payload.get("supersedes")),
        pinned: payload.get("pinned").and_then(Value::as_bool),
        starred: payload.get("starred").and_then(Value::as_bool),
        cwd: payload.get("cwd").and_then(Value::as_str).map(str::to_string),
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

pub async fn post_rebuild(State(_state): State<ServerState>) -> Response {
    match memory::rebuild() {
        Ok(count) => ok_json(json!({ "rebuilt": count, "status": memory::status() })),
        Err(message) => errors::management_error(500, message),
    }
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
