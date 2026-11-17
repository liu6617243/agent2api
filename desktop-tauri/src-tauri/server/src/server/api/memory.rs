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
//! GET  /api/memory/auto        自动化：调度器状态 + 12 个任务的节奏与累计统计
//! POST /api/memory/auto/run    立即执行一个任务（同步等结果）
//! GET  /api/memory/auto/runs   任务执行审计（?limit=100）
//! GET  /api/memory/agents      Agent 心跳（接入页的「最近活跃」）
//! GET  /api/memory/health      体检（孤儿索引、断链、过期条目）
//! GET  /api/memory/sync        WebDAV 同步状态（含设备、跳过名单、上次结果）
//! POST /api/memory/sync/test   测连接（只读，不写任何文件）
//! POST /api/memory/sync/run    立即同步一次（同步等待结果）
//! GET  /api/memory/sync/conflicts  冲突队列
//! POST /api/memory/sync/conflicts  裁决冲突（{id, resolution: local|remote|ignore}）
//! GET  /api/memory/sync/log    同步流水
//! GET  /api/memory/sync/preview 本地同步包清单（不下发内容）
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

/// 任务执行审计（自动化页「查看全部」）。`status()` 已带 30 条预览，
/// 这个接口只为翻页。
pub async fn get_auto_runs(State(_state): State<ServerState>, Query(params): Query<HashMap<String, String>>) -> Response {
    let limit = number(&params, "limit").unwrap_or(100);
    ok_json(memory::schedule::runs(limit))
}

pub async fn get_agents(State(_state): State<ServerState>) -> Response {
    // 接入页要的是「卡片 + 心跳」一整套，不再是单纯的心跳列表：
    // 每张卡都要能回答「装了吗 / 配置在哪 / 写进去了吗 / 提示词块还在吗」。
    ok_json(memory::agents::list())
}

/// 一键接入：往选中的客户端写 MCP 条目 + 提示词受控块。
pub async fn post_agents_install(State(state): State<ServerState>, headers: axum::http::HeaderMap, body: Bytes) -> Response {
    let payload = match parse_body(&body) {
        Ok(value) => value,
        Err(error) => return errors::management_error(400, error.message),
    };
    let Some(object) = payload.as_object() else {
        return errors::management_error(400, "请求体必须是 JSON 对象");
    };
    let clients: Vec<String> = object
        .get("clients")
        .and_then(Value::as_array)
        .map(|items| items.iter().filter_map(Value::as_str).map(str::to_string).collect())
        .unwrap_or_default();
    if clients.is_empty() {
        return errors::management_error(400, "请至少选择一个客户端");
    }
    // 写进客户端的是「客户端访问网关」的地址，不是本进程视角的地址。
    let base = gateway_base(&headers, &state);
    let key = match plain_key(object.get("api_key_id").and_then(Value::as_str).unwrap_or("")) {
        Ok(key) => key,
        Err(response) => return response,
    };
    let outcome = crate::server::core::blocking::run(move || {
        memory::agents::install(&clients, &base, &key)
    })
    .await;
    match outcome {
        Ok(value) => ok_json(value),
        Err(error) => errors::management_error(500, error.message),
    }
}

pub async fn post_agents_uninstall(State(_state): State<ServerState>, body: Bytes) -> Response {
    let payload = match parse_body(&body) {
        Ok(value) => value,
        Err(error) => return errors::management_error(400, error.message),
    };
    let clients: Vec<String> = payload
        .get("clients")
        .and_then(Value::as_array)
        .map(|items| items.iter().filter_map(Value::as_str).map(str::to_string).collect())
        .unwrap_or_default();
    if clients.is_empty() {
        return errors::management_error(400, "请至少选择一个客户端");
    }
    let outcome = crate::server::core::blocking::run(move || memory::agents::uninstall(&clients)).await;
    match outcome {
        Ok(value) => ok_json(value),
        Err(error) => errors::management_error(500, error.message),
    }
}

/// 校验：逐个客户端读一遍配置，确认条目还在、提示词块还完整。
pub async fn post_agents_verify(State(_state): State<ServerState>, body: Bytes) -> Response {
    let payload = parse_body(&body).unwrap_or(Value::Null);
    let clients: Vec<String> = payload
        .get("clients")
        .and_then(Value::as_array)
        .map(|items| items.iter().filter_map(Value::as_str).map(str::to_string).collect())
        .unwrap_or_default();
    let outcome = crate::server::core::blocking::run(move || memory::agents::verify(&clients)).await;
    match outcome {
        Ok(value) => ok_json(value),
        Err(error) => errors::management_error(500, error.message),
    }
}

/// 配置片段：给自定义客户端 / 手动粘贴用。
pub async fn get_agents_snippet(State(state): State<ServerState>, headers: axum::http::HeaderMap) -> Response {
    ok_json(memory::agents::snippet(&gateway_base(&headers, &state)))
}

/// 加一条自定义 Agent（扩展位）。同名覆盖。
pub async fn post_agents_custom_add(State(_state): State<ServerState>, body: Bytes) -> Response {
    let payload = match parse_body(&body) {
        Ok(value) => value,
        Err(error) => return errors::management_error(400, error.message),
    };
    let text = |key: &str| payload.get(key).and_then(Value::as_str).unwrap_or("").to_string();
    let (name, config_path, format, instruction_path) = (
        text("name"),
        text("configPath"),
        text("format"),
        text("instructionPath"),
    );
    let outcome = crate::server::core::blocking::run(move || {
        memory::agents::add_custom(&name, &config_path, &format, &instruction_path)
    })
    .await;
    match outcome {
        Ok(Ok(value)) => ok_json(value),
        Ok(Err(message)) => errors::management_error(400, message),
        Err(error) => errors::management_error(500, error.message),
    }
}

/// 删一条自定义 Agent。
pub async fn post_agents_custom_remove(State(_state): State<ServerState>, body: Bytes) -> Response {
    let payload = parse_body(&body).unwrap_or(Value::Null);
    let name = payload.get("name").and_then(Value::as_str).unwrap_or("").to_string();
    let outcome = crate::server::core::blocking::run(move || memory::agents::remove_custom(&name)).await;
    match outcome {
        Ok(Ok(value)) => ok_json(value),
        Ok(Err(message)) => errors::management_error(400, message),
        Err(error) => errors::management_error(500, error.message),
    }
}

/// 「客户端要访问的网关地址」。
///
/// 面板端口与网关端口可能不同：客户端连的是网关端口，所以有面板端口时要把
/// Host 里的端口换回网关端口，否则写进客户端的是一个连不上的地址。
fn gateway_base(headers: &axum::http::HeaderMap, state: &ServerState) -> String {
    let host = headers
        .get(axum::http::header::HOST)
        .and_then(|value| value.to_str().ok())
        .filter(|value| !value.is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| format!("127.0.0.1:{}", state.port));
    let host = if state.panel_port.is_some() {
        replace_port(&host, state.port)
    } else {
        host
    };
    let scheme = headers
        .get("x-forwarded-proto")
        .and_then(|value| value.to_str().ok())
        .filter(|value| *value == "https")
        .unwrap_or("http");
    format!("{scheme}://{host}")
}

fn replace_port(host: &str, port: u16) -> String {
    if let Some(stripped) = host.strip_prefix('[') {
        if let Some(end) = stripped.find(']') {
            return format!("[{}]:{port}", &stripped[..end]);
        }
    }
    match host.rsplit_once(':') {
        Some((name, tail)) if !name.is_empty() && tail.chars().all(|ch| ch.is_ascii_digit()) => {
            format!("{name}:{port}")
        }
        _ => format!("{host}:{port}"),
    }
}

fn plain_key(key_id: &str) -> Result<String, Response> {
    crate::server::core::api_keys::list()
        .into_iter()
        .find(|entry| entry.id == key_id && entry.is_usable())
        .map(|entry| entry.key)
        .ok_or_else(|| errors::management_error(400, "所选 API Key 不存在、已停用或不可用"))
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

/// 自动化页顶部：调度线程状态 + 12 个任务的节奏与上次执行。
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

// ── 项目卡操作 ───────────────────────────────────────────────

/// 改一张项目卡（重命名 / 置顶 / 归档）。只动登记表，不动记忆文件。
pub async fn post_project_update(State(_state): State<ServerState>, body: Bytes) -> Response {
    let payload = match parse_body(&body) {
        Ok(value) => value,
        Err(error) => return errors::management_error(400, error.message),
    };
    let Some(slug) = payload.get("slug").and_then(Value::as_str) else {
        return errors::management_error(400, "缺少 slug");
    };
    match memory::update_project(slug, &payload) {
        Ok(value) => ok_json(value),
        Err(message) => errors::management_error(400, message),
    }
}

/// 把 `from` 合并进 `into`。会逐条改写记忆的 frontmatter 与落盘路径，
/// 所以同步等待 —— 用户点完就想看到「迁移了多少条」。
pub async fn post_project_merge(State(_state): State<ServerState>, body: Bytes) -> Response {
    let payload = match parse_body(&body) {
        Ok(value) => value,
        Err(error) => return errors::management_error(400, error.message),
    };
    let from = payload.get("from").and_then(Value::as_str).unwrap_or("").to_string();
    let into = payload.get("into").and_then(Value::as_str).unwrap_or("").to_string();
    if from.is_empty() || into.is_empty() {
        return errors::management_error(400, "缺少 from / into");
    }
    let outcome = crate::server::core::blocking::run(move || memory::merge_project(&from, &into)).await;
    match outcome {
        Ok(Ok(value)) => ok_json(value),
        Ok(Err(message)) => errors::management_error(400, message),
        Err(error) => errors::management_error(500, error.message),
    }
}

// ── 体检 ─────────────────────────────────────────────────────

pub async fn get_health(State(_state): State<ServerState>, Query(params): Query<HashMap<String, String>>) -> Response {
    ok_json(memory::health(number(&params, "limit").unwrap_or(50)))
}

// ── 导入 ─────────────────────────────────────────────────────

/// 导入页：来源清单 + 上次结果 + 扫描统计。
pub async fn get_import(State(_state): State<ServerState>) -> Response {
    let result = crate::server::core::blocking::run(memory::import::view).await;
    match result {
        Ok(value) => ok_json(value),
        Err(error) => errors::management_error(500, error.message),
    }
}

/// 干跑预览：**只读文件，不写任何东西**（除了「刚预览过」这个时间戳）。
///
/// 单独一个端点而不是给 `run` 加参数，是因为它必须在权限与审计上明显不同 ——
/// 「这个动作不会改数据」应该是端点级的承诺，不是请求体里的一个布尔。
pub async fn post_import_preview(State(_state): State<ServerState>, body: Bytes) -> Response {
    let payload = parse_body(&body).unwrap_or(Value::Null);
    let only = payload.get("path").and_then(Value::as_str).map(str::to_string);
    let limit = payload.get("limit").and_then(Value::as_u64).unwrap_or(60) as usize;
    let result = crate::server::core::blocking::run(move || memory::import::preview(only.as_deref(), limit)).await;
    match result {
        Ok(value) => ok_json(value),
        Err(error) => errors::management_error(500, error.message),
    }
}

/// 真导入。强制先预览（`import.dryRunFirst`），同步等待结果 ——
/// 用户点了就想知道「写进去多少条、跳过多少条」。
pub async fn post_import_run(State(_state): State<ServerState>, body: Bytes) -> Response {
    let payload = parse_body(&body).unwrap_or(Value::Null);
    let only = payload.get("path").and_then(Value::as_str).map(str::to_string);
    let result = crate::server::core::blocking::run(move || memory::import::run(only.as_deref())).await;
    match result {
        Ok(Ok(summary)) => ok_json(json!({ "summary": summary, "view": memory::import::view() })),
        Ok(Err(message)) => errors::management_error(400, message),
        Err(error) => errors::management_error(500, error.message),
    }
}

/// 增删改导入来源。整体替换而不是逐条增删：来源只有几条，整体替换让
/// 「前端删一条、后端留一条」这类不一致无从发生。
pub async fn post_import_sources(State(_state): State<ServerState>, body: Bytes) -> Response {
    let payload = match parse_body(&body) {
        Ok(value) => value,
        Err(error) => return errors::management_error(400, error.message),
    };
    let rows = payload
        .get("sources")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let mut items: Vec<memory::import::Source> = Vec::new();
    for row in &rows {
        // 接受两种形状：裸字符串（配置里的原始形态）与对象（页面表单）
        let raw = match row {
            Value::String(text) => text.clone(),
            Value::Object(map) => {
                let path = map.get("path").and_then(Value::as_str).unwrap_or("").trim();
                if path.is_empty() {
                    continue;
                }
                let format = map.get("format").and_then(Value::as_str).unwrap_or("md");
                let enabled = map.get("enabled").and_then(Value::as_bool).unwrap_or(true);
                let priority = map.get("priority").and_then(Value::as_i64).unwrap_or(0);
                format!("{path}|{format}|{enabled}|{priority}")
            }
            _ => continue,
        };
        if let Some(source) = memory::import::Source::parse(&raw) {
            items.push(source);
        }
    }
    match memory::import::save_sources(&items) {
        Ok(()) => ok_json(json!({ "view": memory::import::view() })),
        Err(message) => errors::management_error(400, message),
    }
}

// ── 导入来源清册 ─────────────────────────────────────────────

/// 12 个内置来源的探测结果。
///
/// 这个端点会**真的去读盘**（开 sqlite、递归数文件），所以它比 `/import` 贵。
/// 页面在两个时机拉它：进页签时、以及任何一个来源被启停/改路径之后。
pub async fn get_import_registry(State(_state): State<ServerState>) -> Response {
    let result = crate::server::core::blocking::run(memory::import_registry::view).await;
    match result {
        Ok(value) => ok_json(value),
        Err(error) => errors::management_error(500, error.message),
    }
}

/// 启用一个内置来源（写进 `import.sources`，现有导入链路就会读它）。
pub async fn post_import_registry_enable(State(_state): State<ServerState>, body: Bytes) -> Response {
    let payload = parse_body(&body).unwrap_or(Value::Null);
    let Some(id) = payload.get("id").and_then(Value::as_str).map(str::to_string) else {
        return errors::management_error(400, "缺少 id".to_string());
    };
    let result = crate::server::core::blocking::run(move || memory::import_registry::enable(&id)).await;
    match result {
        Ok(Ok(value)) => ok_json(value),
        Ok(Err(message)) => errors::management_error(400, message),
        Err(error) => errors::management_error(500, error.message),
    }
}

/// 停用一个内置来源（从 `import.sources` 移除 + 记进忽略名单）。
pub async fn post_import_registry_disable(State(_state): State<ServerState>, body: Bytes) -> Response {
    let payload = parse_body(&body).unwrap_or(Value::Null);
    let Some(id) = payload.get("id").and_then(Value::as_str).map(str::to_string) else {
        return errors::management_error(400, "缺少 id".to_string());
    };
    let result = crate::server::core::blocking::run(move || memory::import_registry::disable(&id)).await;
    match result {
        Ok(Ok(value)) => ok_json(value),
        Ok(Err(message)) => errors::management_error(400, message),
        Err(error) => errors::management_error(500, error.message),
    }
}

/// 改一个内置来源的路径（写 `import.overrides`，并把新路径同步进 `import.sources`）。
pub async fn post_import_registry_path(State(_state): State<ServerState>, body: Bytes) -> Response {
    let payload = parse_body(&body).unwrap_or(Value::Null);
    let Some(id) = payload.get("id").and_then(Value::as_str).map(str::to_string) else {
        return errors::management_error(400, "缺少 id".to_string());
    };
    let path = payload.get("path").and_then(Value::as_str).unwrap_or("").to_string();
    let result = crate::server::core::blocking::run(move || memory::import_registry::set_path(&id, &path)).await;
    match result {
        Ok(Ok(value)) => ok_json(value),
        Ok(Err(message)) => errors::management_error(400, message),
        Err(error) => errors::management_error(500, error.message),
    }
}

/// 加一条自定义来源。走「追加」而不是整体替换，页面就不必知道全部行。
pub async fn post_import_registry_add(State(_state): State<ServerState>, body: Bytes) -> Response {
    let payload = parse_body(&body).unwrap_or(Value::Null);
    let path = payload.get("path").and_then(Value::as_str).unwrap_or("").to_string();
    let format = payload.get("format").and_then(Value::as_str).unwrap_or("").to_string();
    let priority = payload.get("priority").and_then(Value::as_i64).unwrap_or(0);
    let result = crate::server::core::blocking::run(move || {
        memory::import_registry::add(&path, &format, priority)
    })
    .await;
    match result {
        Ok(Ok(value)) => ok_json(value),
        Ok(Err(message)) => errors::management_error(400, message),
        Err(error) => errors::management_error(500, error.message),
    }
}

// ── 去重巡检 ─────────────────────────────────────────────────

/// 全库去重巡检。同步等结果 —— 它只产建议不写记忆，但会读全库。
pub async fn post_dedup_scan(State(_state): State<ServerState>) -> Response {
    let result = crate::server::core::blocking::run(memory::dedup_scan).await;
    match result {
        Ok(value) => ok_json(value),
        Err(error) => errors::management_error(500, error.message),
    }
}

// ── WebDAV 同步 ──────────────────────────────────────────────

/// 同步页顶部：配置摘要 + 设备 + 冲突计数 + 上次结果。
pub async fn get_sync(State(_state): State<ServerState>) -> Response {
    let mut payload = memory::sync::status();
    if let Some(map) = payload.as_object_mut() {
        map.insert("devices".into(), memory::sync::devices());
        map.insert("skippedPaths".into(), memory::sync::skip_list());
    }
    ok_json(payload)
}

/// 测连接。**只读**：不做 MKCOL、不传包，避免「点一下测试就在远端留下目录」。
///
/// ── 为什么带一个可选 body ──────────────────────────────────
/// 配置页的「测连接」坐在三个输入框旁边，用户想「用刚填的值先验一下再保存」。
/// 带 body 就用 body 里的值（空的字段沿用已保存的），不带就测已保存的那份 ——
/// 「同步状态」卡里那个「测连接」发的正是空 body。
pub async fn post_sync_test(State(_state): State<ServerState>, body: Bytes) -> Response {
    let probe = if body.is_empty() {
        None
    } else {
        match parse_body(&body) {
            Ok(value) if value.is_null() || value.as_object().map(|map| map.is_empty()).unwrap_or(false) => None,
            Ok(value) => match serde_json::from_value::<memory::sync::Probe>(value) {
                Ok(probe) => Some(probe),
                Err(error) => return errors::management_error(400, format!("测试请求格式不对：{error}")),
            },
            Err(error) => return errors::management_error(400, error.message),
        }
    };
    let result = match probe {
        Some(probe) => {
            crate::server::core::blocking::run(move || memory::sync::test_connection_with(probe)).await
        }
        None => crate::server::core::blocking::run(memory::sync::test_connection).await,
    };
    match result {
        Ok(value) => ok_json(value),
        Err(error) => errors::management_error(500, error.message),
    }
}

/// 「保存设置」：同步页那张凭据卡 + 模块目录卡的统一保存入口。
///
/// ── 为什么单独一个端点，而不是让它走 `PUT /api/memory/config` ────
/// 那张卡片的语义是「这一组字段一起保存」，不参与配置页的草稿层（配置页是
/// 「改一堆、最后统一保存」）。若走同一个端点，用户点完这里配置页的草稿会被
/// 顶上来的新值覆盖，而配置页还以为自己没保存 —— 两个正交的界面互相打架。
///
/// ── 接受哪些键 ────────────────────────────────────────────
/// **只接受 `sync.` 开头的旋钮**（清单从 schema 里现取，不在这里抄一份）。
/// 于是加一个同步旋钮时这个端点自动跟上，而它永远不可能变成「任意写配置」
/// 的后门。秘密字段（密码 / 口令）沿用 `config::apply_one` 的空串语义：
/// 空串 = 保留库里那份，界面从不下发原文。
pub async fn post_sync_setup(State(_state): State<ServerState>, body: Bytes) -> Response {
    let payload = match parse_body(&body) {
        Ok(value) => value,
        Err(error) => return errors::management_error(400, error.message),
    };
    let Some(object) = payload.as_object() else {
        return errors::management_error(400, "请求体必须是 JSON 对象");
    };
    let mut patch = serde_json::Map::new();
    for (key, value) in object {
        if !key.starts_with("sync.") {
            return errors::management_error(400, format!("{key} 不属于同步设置"));
        }
        if memory_config::find(key).is_none() {
            return errors::management_error(400, format!("未知的同步配置项：{key}"));
        }
        // 远端目录名会被直接拼进 URL 路径，`../` 能跑出远端目录 —— 在入口
        // 拦住比在拼 URL 处拦更早，且错误信息能指到具体字段。
        if key.ends_with("remoteDir") || key.starts_with("sync.modules.") {
            if let Some(text) = value.as_str() {
                if text.contains("..") || text.contains('\\') {
                    return errors::management_error(400, format!("{key} 不能包含 .. 或反斜杠"));
                }
            }
        }
        patch.insert(key.clone(), value.clone());
    }
    if patch.is_empty() {
        return errors::management_error(400, "没有要保存的字段");
    }
    // 走 `memory::save_config` 而不是裸 `config::patch`：它是配置写入的唯一
    // 入口，除了落盘还会把内存版本号 +1 —— 页面据此判断「手里这份是不是过期了」。
    // 在这里绕过去，配置改了但版本号不动，页面就不会重取 schema / status。
    match memory::save_config(&Value::Object(patch)) {
        Ok(_) => ok_json(json!({
            "values": memory_config::flat(),
            "status": memory::sync::status(),
        })),
        Err(message) => errors::management_error(400, message),
    }
}

/// 「清除配置」：把 `sync.` 下的自定义值全部恢复默认（凭据一起清掉）。
///
/// 只动 `sync.*`：清除同步配置不该顺手把记忆的检索权重、自动化节奏一起重置。
/// 已同步的数据与远端包**不动** —— 这个按钮清的是「怎么连」，不是「同步过什么」。
pub async fn post_sync_reset(State(_state): State<ServerState>) -> Response {
    let keys: Vec<String> = memory_config::schema()
        .iter()
        .filter(|knob| knob.key.starts_with("sync."))
        .map(|knob| knob.key.to_string())
        .collect();
    match memory_config::reset(&keys) {
        Ok(_) => ok_json(json!({
            "values": memory_config::flat(),
            "status": memory::sync::status(),
        })),
        Err(message) => errors::management_error(400, message),
    }
}

/// 立即同步。同步等待：用户点「立即同步」就想看到下行/上行/冲突的条数。
pub async fn post_sync_run(State(_state): State<ServerState>) -> Response {
    let outcome = crate::server::core::blocking::run(|| memory::sync::run(true)).await;
    match outcome {
        Ok(outcome) => ok_json(json!({
            "outcome": outcome.to_json(),
            "status": memory::sync::status(),
        })),
        Err(error) => errors::management_error(500, error.message),
    }
}

pub async fn get_sync_conflicts(State(_state): State<ServerState>) -> Response {
    ok_json(memory::sync::conflicts())
}

/// 裁决一条冲突。三种裁决的语义见 `sync::resolve_conflict` 的注释。
pub async fn post_sync_conflict(State(_state): State<ServerState>, body: Bytes) -> Response {
    let payload = match parse_body(&body) {
        Ok(value) => value,
        Err(error) => return errors::management_error(400, error.message),
    };
    let Some(id) = payload.get("id").and_then(Value::as_i64) else {
        return errors::management_error(400, "缺少 id");
    };
    let resolution = payload
        .get("resolution")
        .and_then(Value::as_str)
        .unwrap_or("local");
    if !matches!(resolution, "local" | "remote" | "ignore") {
        return errors::management_error(
            400,
            format!("未知的裁决方式：{resolution}（可选 local / remote / ignore）"),
        );
    }
    let resolution = resolution.to_string();
    let outcome = crate::server::core::blocking::run(move || {
        memory::sync::resolve_conflict(id, &resolution)
    })
    .await;
    match outcome {
        Ok(Ok(())) => ok_json(json!({ "ok": true, "conflicts": memory::sync::conflicts() })),
        Ok(Err(message)) => errors::management_error(400, message),
        Err(error) => errors::management_error(500, error.message),
    }
}

pub async fn get_sync_log(State(_state): State<ServerState>, Query(params): Query<HashMap<String, String>>) -> Response {
    ok_json(memory::sync::log_entries(number(&params, "limit").unwrap_or(100)))
}

/// 同步预览：本地会打进包的文件清单（不含内容）。
pub async fn get_sync_preview(State(_state): State<ServerState>) -> Response {
    ok_json(memory::sync::local_snapshot())
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