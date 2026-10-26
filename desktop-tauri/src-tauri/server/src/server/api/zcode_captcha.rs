//! 活动套餐通道的**人机验证令牌池**端点：`GET/POST /api/zcode/captcha`。
//!
//! ── 谁在用这条端点（两个方向，都要看）────────────────────────
//!   · **界面 → 网关（POST）**：桌面端 WebView 里的铸造器
//!     （`ui/zcode-captcha-pool.js`）静默铸一个阿里云无痕验证令牌，就推一个
//!     过来。令牌一次性、有寿命，池子因此只是个「已铸待用」的队列。
//!   · **界面 ← 网关（GET）**：铸造器轮询这里的库存与计数，决定要不要补货
//!     （`ready < target` 且账号表里有走活动套餐的账号时才铸，避免白烧风控配额）。
//!
//! ── 为什么令牌走管理 API 而不是 Tauri 事件 ──────────────────
//! 同一份界面代码在两种部署下都要能用：桌面端（Tauri 桥 → `api_request`）与
//! headless（浏览器直接打 `/api/*`）。管理 API 是这两条路唯一的公共面，而
//! 「池子」这个概念本身在服务端（转发层要按请求取），界面只负责生产 ——
//! 用一条普通端点表达，双方都不必知道对方的存在方式。
//!
//! ── 池子本体在**插件进程**里 ─────────────────────────────────
//! 「上游此刻要不要令牌」「令牌怎么取、放哪儿、过期怎么丢」都是上游协议知识，
//! 随实现搬到 `agent2api-providers`。网关这边只剩「把界面的推入转成一次 RPC」
//! 与「把插件回的读数补上网关侧的节奏设置与账号计数」—— 前者是 `admin.action`
//! 的 `captcha.push` / `captcha.stats`，后者是本文件自己的活。
//!
//! ── 与 `api::captcha`（ALTCHA 登录校验）的区别 ───────────────
//! 那条是**网关自己**的登录门槛开关；这条是**上游 ZCode 要求**的验证码令牌池。
//! 名字像、职责完全不同，别混。
//!
//! ── 硬约束 ──────────────────────────────────────────────────
//! release 是 `panic=abort`：本文件零 unwrap/expect/panic。

use axum::body::Bytes;
use axum::extract::State;
use axum::response::Response;
use serde_json::{json, Value};

use crate::server::config;
use crate::server::core::providers::zcode;
use crate::server::errors::management_error;
use crate::server::http::{ok_json, parse_body};
use crate::server::logging;
use crate::server::ServerState;

/// 池子由已加载的任一 ZCode 实例访问，不要求该地区有启用账号。
async fn call_action(state: &ServerState, action: &str, payload: &Value) -> Result<Value, (i32, String)> {
    let adapter = zcode::region::Region::ALL.into_iter()
        .find_map(|region| crate::server::core::providers::adapter::adapter_by_id(region.provider_id()));
    let Some(adapter) = adapter else {
        return Err((
            400,
            "ZCode 的提供商插件没有安装或未启用，无法访问验证码令牌池".to_string(),
        ));
    };
    adapter
        .admin_action(state.store(), "", action, payload)
        .await
        .map_err(|error| (error.status_code, error.message))
}

/// 池子概况 + 「界面该不该铸造」的判据。
///
/// 三项网关侧的补充（插件的 `captcha.stats` 只知道池子本身）：
///   · `target` / `pollMs` / `maxPerRound` —— 用户配的补货节奏（`config`）；
///   · `startPlanAccounts` / `captchaAccountId` —— 账号表里走活动套餐的家；
///   · `needsTokens` —— 三个条件合起来才算「该铸」。
async fn stats_json(state: &ServerState) -> Value {
    let mut body = match call_action(state, "captcha.stats", &Value::Null).await {
        Ok(body) => body,
        // 插件不在时给一张**如实**的空表（而不是 500）：界面据此显示
        // 「不可用」，铸造器也就不会白烧阿里云配额。
        Err((_status, message)) => json!({ "ready": 0, "required": false, "error": message }),
    };
    let (start_plan_accounts, account_id) = start_plan_accounts(state);
    let ready = body.get("ready").and_then(Value::as_u64).unwrap_or(0);
    let required = body.get("required").and_then(Value::as_bool).unwrap_or(false);
    let pool = config::captcha_pool_settings();
    if let Some(object) = body.as_object_mut() {
        object.insert("target".to_string(), Value::from(pool.target as u64));
        object.insert(
            "startPlanAccounts".to_string(),
            Value::from(start_plan_accounts as u64),
        );
        // 铸造器唯一需要的那个布尔：有账号要走这条路、库存不足目标、
        // **且上游此刻确实要令牌**。第三个条件很要紧：免码期还让界面每隔
        // 几秒去阿里云铸一枚，等于白烧风控配额 —— 那东西撞上限之后是整段
        // 铸不出来，比多等一会儿严重。
        object.insert(
            "needsTokens".to_string(),
            Value::Bool(required && start_plan_accounts > 0 && ready < pool.target as u64),
        );
        // 顺手把铸造器的另外两个节奏参数一起下发：轮询间隔与单轮上限。
        // 铸造器每轮都拉这份响应，改设置后下一轮就按新值跑（热生效）。
        object.insert("pollMs".to_string(), Value::from(pool.poll_ms as u64));
        object.insert(
            "maxPerRound".to_string(),
            Value::from(pool.max_per_round as u64),
        );
        // 顺手给一个**可用的账号 id**：铸造器还要拿它去问上游那份风控配置
        // （sceneId / prefix / region）。让界面自己去读账号列表会把「哪些账号
        // 能用」的判据抄第二遍 —— 那正是账号页最容易漂移的地方。
        object.insert(
            "captchaAccountId".to_string(),
            account_id.map(Value::String).unwrap_or(Value::Null),
        );
    }
    body
}

/// 账号表里「走活动套餐」的**启用**账号数，外加其中一个的 id（给铸造器取风控配置用）。
///
/// 优先挑走活动套餐的：那个账号的配置一定取得到（领取流程用过同一条接口）。
/// 一个都没有时退回任意一个启用的 ZCode 账号 —— 风控配置是共享的，从哪个账号
/// 取都一样；连 ZCode 账号都没有才回 `None`（界面据此不铸造）。
fn start_plan_accounts(state: &ServerState) -> (usize, Option<String>) {
    let list = state.store().list_accounts();
    let Some(accounts) = list.get("accounts").and_then(Value::as_array) else {
        return (0, None);
    };
    let mut start_plan = 0usize;
    let mut start_plan_id: Option<String> = None;
    let mut fallback_id: Option<String> = None;
    for account in accounts {
        let provider = account.get("provider").and_then(Value::as_str).unwrap_or("");
        if zcode::region::Region::from_provider_id(provider).is_none() {
            continue;
        }
        let id = account.get("id").and_then(Value::as_str).unwrap_or("");
        if account.get("enabled").and_then(Value::as_bool) == Some(false) || id.is_empty() {
            continue;
        }
        if fallback_id.is_none() {
            fallback_id = Some(id.to_string());
        }
        if zcode::plan_of(account) == zcode::PLAN_START {
            start_plan += 1;
            if start_plan_id.is_none() {
                start_plan_id = Some(id.to_string());
            }
        }
    }
    (start_plan, start_plan_id.or(fallback_id))
}

/// GET /api/zcode/captcha —— 池子概况（库存 / 寿命 / 计数 / 是否该补货）
pub async fn get_captcha(State(state): State<ServerState>) -> Response {
    ok_json(stats_json(&state).await)
}

/// POST /api/zcode/captcha —— body `{tokens: [{param, region}]}`
///
/// 批量形态是为了「一次铸好两个就一起推」：铸造本身要一两秒，逐条往返会让
/// 库存长期贴着 0。单条（`{param, region}`）也认 —— 手工排障时那是更自然的写法。
pub async fn push_captcha(State(state): State<ServerState>, body: Bytes) -> Response {
    let payload = match parse_body(&body) {
        Ok(value) => value,
        Err(error) => return management_error(400, error.message),
    };
    let Some(object) = payload.as_object() else {
        return management_error(400, "请求体必须是 JSON 对象");
    };
    // 两种形态归一成一个列表（见上面那条说明）
    let items: Vec<Value> = match object.get("tokens") {
        Some(Value::Array(items)) => items.clone(),
        Some(_) => return management_error(400, "tokens 必须是数组"),
        None => vec![payload.clone()],
    };
    // 入池的判据（param 非空）也在插件里：网关不解析令牌形态，它只知道
    // 「有一批条目要推过去」。插件回 `accepted`，网关据此决定要不要报 400。
    let accepted = match call_action(&state, "captcha.push", &json!({ "tokens": items })).await {
        Ok(outcome) => outcome
            .get("accepted")
            .and_then(Value::as_u64)
            .unwrap_or(0),
        Err((status_code, message)) => return management_error(status_code, message),
    };
    if accepted == 0 {
        return management_error(400, "没有可入池的令牌（param 不能为空）");
    }
    // 只写在**终端**：铸造是「有账号走活动套餐时每隔几秒一次」的常态动作，
    // 进运行日志页会把它刷成噪声（判断口径见 provider_loop 的模块头）
    logging::console_line(
        "[ZCode]",
        &format!(
            "🔐 人机验证令牌入池 {} 个（目标 {}）",
            accepted,
            config::captcha_pool_settings().target,
        ),
    );
    let mut response = stats_json(&state).await;
    if let Some(object) = response.as_object_mut() {
        object.insert("accepted".to_string(), Value::from(accepted));
    }
    // 铸造器不读 changes 之类的字段，但它会把整个响应打给控制台排障 ——
    // 这里明确带一个 accepted，让「推了几个」有据可查
    ok_json(response)
}
