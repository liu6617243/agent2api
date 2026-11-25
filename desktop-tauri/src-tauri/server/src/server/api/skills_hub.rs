//! 技能仓库（`/api/skills-hub/*`）。
//!
//! ```text
//! GET  /api/skills-hub/overview       仪表盘：计数 / 渠道矩阵 / 待裁决 / 孤儿 / 最近报告
//! GET  /api/skills-hub/library        技能库列表（扫描结果 ∪ 台账）
//! GET  /api/skills-hub/skill          单个技能详情（?name=）
//! GET  /api/skills-hub/conflicts      冲突队列 + 现场（差异 / 链接指向）
//! POST /api/skills-hub/resolve        裁决一条（{id, choice}）
//! GET  /api/skills-hub/preview        干跑预览（不改任何文件）
//! POST /api/skills-hub/sync           执行一次同步（重新规划后重放）
//! GET  /api/skills-hub/reports        报告清单（?limit=）
//! GET  /api/skills-hub/report         报告正文（?file=）
//! GET  /api/skills-hub/config         配置（整份生效值）
//! PUT  /api/skills-hub/config         配置（部分更新）
//! POST /api/skills-hub/config/reset   恢复默认（?keys=a,b 或全量）
//! GET  /api/skills-hub/tools          渠道清单（含解析出的实际目录）
//! GET  /api/skills-hub/probe          未注册渠道的只读探测
//! POST /api/skills-hub/custom-dir     加一条自定义目录（{path}）
//! DELETE /api/skills-hub/custom-dir   删一条自定义目录（{path}）
//! GET  /api/skills-hub/daily-check    上次巡检结果
//! POST /api/skills-hub/daily-check/run 立即巡检（?autoMount=1 顺手修失效挂载）
//! POST /api/skills-hub/repair         一键修复失效挂载
//! GET  /api/skills-hub/trash          回收站
//! POST /api/skills-hub/trash/restore  从回收站还原（{file}）
//! POST /api/skills-hub/trash/purge    清空过期回收站
//! POST /api/skills-hub/adopt          纳管中央目录里未登记的技能（{name}）
//! POST /api/skills-hub/remove         删除技能（{name}）
//! POST /api/skills-hub/mount/toggle   切换一处挂载（{name, tool, enabled}）
//! ```
//!
//! ── 为什么全部挂 protected ──────────────────────────────────
//! 这个模块**会动磁盘**：`sync` 会把渠道目录里的副本收进中央、把目录换成交接
//! 链接；`remove` 会删真身。响应里还带用户的完整目录结构（各客户端的 skills
//! 路径、技能正文）。与 `/api/config`、`/api/accounts` 同一敏感级 —— 免鉴权
//! 等于任何能连到端口的人都能改用户的技能目录。
//!
//! ── 为什么「预览」与「执行」是两个端点 ─────────────────────
//! 和记忆中枢的历史导入同一口径：「这个动作不写数据」应该是**端点级承诺**，
//! 不是请求体里的布尔开关。预览是 GET（可缓存、可按 URL 直接打开），执行是
//! POST。执行侧**不信**前端传回的预览 —— 它会重新规划一遍再逐条重放，因为
//! 预览到执行之间磁盘可能已经变了（见 `plan::execute` 的注释）。
//!
//! ── 为什么读接口一次给整份现状 ─────────────────────────────
//! 每个读端点返回的都是「这一屏要画的全套数据」（计数 + 行 + 现场），前端不
//! 需要拼多请求。代价是每次读都重跑一遍规划 —— 但规划本身只是遍历目录与读
//! `SKILL.md`，几十个技能的量级远不到需要缓存的规模，而缓存会立刻引入
//! 「预览说不冲突、点开却说冲突」的不一致。

use std::collections::HashMap;

use axum::body::Bytes;
use axum::extract::{Query, State};
use axum::response::Response;
use serde_json::{json, Value};

use crate::server::core::skills_hub;
use crate::server::errors;
use crate::server::http::{ok_json, parse_body};
use crate::server::ServerState;

fn text(map: &HashMap<String, String>, key: &str) -> Option<String> {
    map.get(key)
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

fn flag(map: &HashMap<String, String>, key: &str) -> bool {
    map.get(key)
        .map(|value| {
            matches!(
                value.trim().to_ascii_lowercase().as_str(),
                "1" | "true" | "yes" | "on"
            )
        })
        .unwrap_or(false)
}

/// 从 body 取一个必填字符串字段。
fn field(payload: &Value, key: &str) -> Result<String, Response> {
    let Some(object) = payload.as_object() else {
        return Err(errors::management_error(400, "请求体必须是 JSON 对象"));
    };
    let value = object
        .get(key)
        .and_then(Value::as_str)
        .map(|text| text.trim().to_string())
        .filter(|text| !text.is_empty());
    match value {
        Some(text) => Ok(text),
        None => Err(errors::management_error(
            400,
            format!("缺少字段 {key}（非空字符串）"),
        )),
    }
}

fn body_or_bad_request(body: &Bytes) -> Result<Value, Response> {
    parse_body(body).map_err(|error| errors::management_error(400, error.message))
}

// ─── 读 ──────────────────────────────────────────────────────

pub async fn get_overview(State(_state): State<ServerState>) -> Response {
    ok_json(skills_hub::overview())
}

pub async fn get_library(State(_state): State<ServerState>) -> Response {
    ok_json(skills_hub::library())
}

pub async fn get_skill(State(_state): State<ServerState>, Query(params): Query<HashMap<String, String>>) -> Response {
    let Some(name) = text(&params, "name") else {
        return errors::management_error(400, "缺少 name 参数");
    };
    match skills_hub::skill(&name) {
        Ok(value) => ok_json(value),
        Err(message) => errors::management_error(404, message),
    }
}

pub async fn get_conflicts(State(_state): State<ServerState>) -> Response {
    ok_json(skills_hub::conflicts())
}

pub async fn get_preview(State(_state): State<ServerState>) -> Response {
    ok_json(skills_hub::preview())
}

pub async fn get_reports(State(_state): State<ServerState>, Query(params): Query<HashMap<String, String>>) -> Response {
    let limit = text(&params, "limit")
        .and_then(|value| value.parse::<usize>().ok())
        .unwrap_or(20)
        .clamp(1, 200);
    ok_json(json!({ "items": skills_hub::reports(limit) }))
}

pub async fn get_report(State(_state): State<ServerState>, Query(params): Query<HashMap<String, String>>) -> Response {
    let Some(file) = text(&params, "file") else {
        return errors::management_error(400, "缺少 file 参数");
    };
    match skills_hub::report(&file) {
        Ok(body) => ok_json(json!({ "file": file, "body": body })),
        Err(message) => errors::management_error(404, message),
    }
}

pub async fn get_config(State(_state): State<ServerState>) -> Response {
    ok_json(json!({ "values": skills_hub::config() }))
}

pub async fn get_tools(State(_state): State<ServerState>) -> Response {
    ok_json(json!({ "items": skills_hub::tool_list() }))
}

pub async fn get_probe(State(_state): State<ServerState>) -> Response {
    ok_json(json!({ "items": skills_hub::probe() }))
}

pub async fn get_daily_check(State(_state): State<ServerState>) -> Response {
    ok_json(json!({ "last": skills_hub::daily_check_status() }))
}

pub async fn get_trash(State(_state): State<ServerState>) -> Response {
    ok_json(skills_hub::trash())
}

// ─── 配置写 ──────────────────────────────────────────────────

pub async fn put_config(State(_state): State<ServerState>, body: Bytes) -> Response {
    let payload = match body_or_bad_request(&body) {
        Ok(value) => value,
        Err(response) => return response,
    };
    // 接受 `{values: {...}}` 与裸对象两种形状：前端有时整份提交、有时只发补丁。
    let patch = payload.get("values").cloned().unwrap_or(payload);
    if !patch.is_object() {
        return errors::management_error(400, "配置必须是 JSON 对象");
    }
    match skills_hub::save_config(&patch) {
        Ok(values) => ok_json(json!({ "values": values })),
        Err(message) => errors::management_error(400, message),
    }
}

pub async fn post_config_reset(State(_state): State<ServerState>, Query(params): Query<HashMap<String, String>>) -> Response {
    let keys: Vec<String> = text(&params, "keys")
        .map(|joined| {
            joined
                .split(',')
                .map(|item| item.trim().to_string())
                .filter(|item| !item.is_empty())
                .collect()
        })
        .unwrap_or_default();
    match skills_hub::reset_config(if keys.is_empty() { None } else { Some(&keys) }) {
        Ok(values) => ok_json(json!({ "values": values })),
        Err(message) => errors::management_error(400, message),
    }
}

pub async fn post_custom_dir(State(_state): State<ServerState>, body: Bytes) -> Response {
    let payload = match body_or_bad_request(&body) {
        Ok(value) => value,
        Err(response) => return response,
    };
    let path = match field(&payload, "path") {
        Ok(value) => value,
        Err(response) => return response,
    };
    match skills_hub::add_custom_dir(&path) {
        Ok(values) => ok_json(json!({ "values": values })),
        Err(message) => errors::management_error(400, message),
    }
}

pub async fn delete_custom_dir(State(_state): State<ServerState>, body: Bytes) -> Response {
    let payload = match body_or_bad_request(&body) {
        Ok(value) => value,
        Err(response) => return response,
    };
    let path = match field(&payload, "path") {
        Ok(value) => value,
        Err(response) => return response,
    };
    match skills_hub::remove_custom_dir(&path) {
        Ok(values) => ok_json(json!({ "values": values })),
        Err(message) => errors::management_error(400, message),
    }
}

// ─── 同步写 ──────────────────────────────────────────────────

pub async fn post_sync(State(_state): State<ServerState>) -> Response {
    ok_json(skills_hub::sync_now())
}

pub async fn post_resolve(State(_state): State<ServerState>, body: Bytes) -> Response {
    let payload = match body_or_bad_request(&body) {
        Ok(value) => value,
        Err(response) => return response,
    };
    let id = match field(&payload, "id") {
        Ok(value) => value,
        Err(response) => return response,
    };
    let choice = payload
        .get("choice")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .to_string();
    if choice.is_empty() {
        return errors::management_error(400, "缺少字段 choice");
    }
    match skills_hub::resolve(&id, &choice) {
        Ok(value) => ok_json(value),
        Err(message) => errors::management_error(400, message),
    }
}

pub async fn post_repair(State(_state): State<ServerState>) -> Response {
    ok_json(skills_hub::repair())
}

pub async fn post_daily_check_run(State(_state): State<ServerState>, Query(params): Query<HashMap<String, String>>) -> Response {
    ok_json(skills_hub::run_daily_check(flag(&params, "autoMount")))
}

pub async fn post_trash_restore(State(_state): State<ServerState>, body: Bytes) -> Response {
    let payload = match body_or_bad_request(&body) {
        Ok(value) => value,
        Err(response) => return response,
    };
    let file = match field(&payload, "file") {
        Ok(value) => value,
        Err(response) => return response,
    };
    match skills_hub::restore(&file) {
        Ok(value) => ok_json(value),
        Err(message) => errors::management_error(400, message),
    }
}

pub async fn post_trash_purge(State(_state): State<ServerState>) -> Response {
    ok_json(skills_hub::purge_trash())
}

pub async fn post_adopt(State(_state): State<ServerState>, body: Bytes) -> Response {
    let payload = match body_or_bad_request(&body) {
        Ok(value) => value,
        Err(response) => return response,
    };
    let name = match field(&payload, "name") {
        Ok(value) => value,
        Err(response) => return response,
    };
    match skills_hub::adopt(&name) {
        Ok(value) => ok_json(value),
        Err(message) => errors::management_error(400, message),
    }
}

pub async fn post_remove(State(_state): State<ServerState>, body: Bytes) -> Response {
    let payload = match body_or_bad_request(&body) {
        Ok(value) => value,
        Err(response) => return response,
    };
    let name = match field(&payload, "name") {
        Ok(value) => value,
        Err(response) => return response,
    };
    match skills_hub::remove(&name) {
        Ok(value) => ok_json(value),
        Err(message) => errors::management_error(400, message),
    }
}

pub async fn post_mount_toggle(State(_state): State<ServerState>, body: Bytes) -> Response {
    let payload = match body_or_bad_request(&body) {
        Ok(value) => value,
        Err(response) => return response,
    };
    let name = match field(&payload, "name") {
        Ok(value) => value,
        Err(response) => return response,
    };
    let tool = match field(&payload, "tool") {
        Ok(value) => value,
        Err(response) => return response,
    };
    let enabled = payload
        .get("enabled")
        .and_then(Value::as_bool)
        .unwrap_or(true);
    match skills_hub::toggle_mount(&name, &tool, enabled) {
        Ok(value) => ok_json(value),
        Err(message) => errors::management_error(400, message),
    }
}