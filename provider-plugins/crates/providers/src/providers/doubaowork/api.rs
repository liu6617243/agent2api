use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

use agent2api_provider_protocol as proto;
use agent2api_provider_sdk::{PluginCtx, StoreProxy};
use reqwest::header;
use serde_json::{json, Value};

use super::PROVIDER_ID;
use crate::errors::GatewayError;

pub const BASE_URL: &str = "https://www.doubao.com";
/// 官方客户端（豆包 PC，Chromium 内核）的 UA 形态。抓包（2026-11-08，
/// 客户端 2.32.6）证实官方请求 UA 恰是「Chrome UA + `SamanthaDoubao/<版本>`
/// 后缀」，无后缀的裸 Chrome UA 是「从未出现过的浏览器」—— 与签到 T+1
/// 软惩罚（见 `sign` 模块头）的嫌疑链一致，请求时按 [`samantha_ua`] 补齐。
pub const USER_AGENT: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/147.0.0.0 Safari/537.36";
// 上游按 pc_version 下发模型菜单；只有 runtime_version 时会成功返回空菜单。
const DEFAULT_CLIENT_VERSION: &str = "2.33.5";

/// UA 补 `SamanthaDoubao/<版本>` 后缀（幂等）。
///
/// ── 为什么在这里补而不是要求重导入 ──────────────────────────
/// 老快照（2026-11-03 那批「导入即快照」）存的是裸 Chrome UA；逐条让用户
/// 重导入不现实。后缀版本号用账号快照里的 `clientVersion`（与 query 里的
/// `pc_version` 同源 —— 官方两者就是同值），没有才退 [`DEFAULT_CLIENT_VERSION`]。
/// 判据是「带不带后缀」，所以幂等：官方形态的 UA 原样放行。
fn samantha_ua(credentials: &Value) -> String {
    let base = credentials["userAgent"]
        .as_str()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or(USER_AGENT)
        .to_string();
    if base.contains("SamanthaDoubao/") {
        return base;
    }
    let version = credentials["clientVersion"]
        .as_str()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or(DEFAULT_CLIENT_VERSION);
    format!("{base} SamanthaDoubao/{version}")
}

pub fn client(
    proxy: Option<&proto::WireProxy>,
    gateway_proxy: &str,
    stream: bool,
) -> Result<reqwest::Client, GatewayError> {
    type Key = (String, Option<crate::timeouts::Timeouts>);
    static CLIENTS: OnceLock<Mutex<HashMap<Key, reqwest::Client>>> = OnceLock::new();
    let proxy = proxy
        .map(proto::WireProxy::to_url)
        .unwrap_or_else(|| gateway_proxy.trim().to_string());
    let timeouts = stream.then(crate::timeouts::current);
    let key = (proxy.clone(), timeouts);
    let clients = CLIENTS.get_or_init(Mutex::default);
    if let Some(client) = clients
        .lock()
        .ok()
        .and_then(|clients| clients.get(&key).cloned())
    {
        return Ok(client);
    }
    let mut builder = reqwest::Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .user_agent(USER_AGENT)
        .connect_timeout(Duration::from_secs(15));
    builder = match timeouts {
        Some(timeouts) => builder
            .connect_timeout(Duration::from_millis(timeouts.connect_ms()))
            .read_timeout(Duration::from_millis(timeouts.read_timeout_backstop_ms())),
        None => builder.timeout(Duration::from_secs(30)),
    };
    if !proxy.is_empty() && proxy != "direct" {
        let proxy = reqwest::Proxy::all(&proxy)
            .map_err(|_| GatewayError::with_status(400, "豆包工作代理地址无效"))?;
        builder = builder.proxy(proxy);
    }
    let client = builder
        .build()
        .map_err(|_| GatewayError::with_status(502, "无法创建豆包工作出网连接"))?;
    if let Ok(mut clients) = clients.lock() {
        if clients.len() >= 128 {
            clients.clear();
        }
        clients.insert(key, client.clone());
    }
    Ok(client)
}

pub fn number(value: &Value) -> Option<u64> {
    value
        .as_u64()
        .or_else(|| value.as_str()?.trim().parse().ok())
}

pub fn identifier(value: &Value) -> Option<String> {
    value
        .as_str()
        .map(str::to_string)
        .or_else(|| value.as_u64().map(|id| id.to_string()))
        .filter(|id| !id.trim().is_empty())
}

pub fn encode(value: &str) -> String {
    value
        .bytes()
        .map(|byte| {
            if byte.is_ascii_alphanumeric() || b"-._~".contains(&byte) {
                char::from(byte).to_string()
            } else {
                format!("%{byte:02X}")
            }
        })
        .collect()
}

pub fn headers(credentials: &Value) -> Result<Vec<(String, String)>, GatewayError> {
    let cookie = super::accounts::cookie_header(credentials)?;
    let ua = samantha_ua(credentials);
    // `sec-ch-ua` 的版本要跟 UA 里的 Chrome 主版本一致（官方客户端两者同源，
    // 抓包里是 `Chrome/147` + `"Chromium";v="147"`）。UA 解不出主版本就退
    // [`USER_AGENT`] 的 147 —— 此时 UA 本身也是裸的，两边至少自洽。
    let chrome_major = ua
        .split("Chrome/")
        .nth(1)
        .and_then(|rest| rest.split('.').next())
        .map(str::to_string)
        .unwrap_or_else(|| "147".into());
    Ok(vec![
        (header::COOKIE.as_str().into(), cookie),
        (header::ORIGIN.as_str().into(), BASE_URL.into()),
        (header::REFERER.as_str().into(), format!("{BASE_URL}/chat/")),
        (header::USER_AGENT.as_str().into(), ua),
        (
            header::ACCEPT.as_str().into(),
            "application/json, text/plain, */*".into(),
        ),
        // 真实客户端（2026-11-12 CDP 抓包）在 `landing/*` 上带的浏览器指纹头：
        // `sec-ch-ua` / `sec-ch-ua-mobile` / `sec-ch-ua-platform` / `Agw-Js-Conv`。
        // 值是抓包原文，`chrome_major` 跟着 UA 走。
        (
            "sec-ch-ua".into(),
            format!("\"Chromium\";v=\"{chrome_major}\", \"Not.A/Brand\";v=\"8\""),
        ),
        ("sec-ch-ua-mobile".into(), "?0".into()),
        ("sec-ch-ua-platform".into(), "\"Windows\"".into()),
        ("sec-fetch-site".into(), "cross-site".into()),
        ("sec-fetch-mode".into(), "cors".into()),
        ("sec-fetch-dest".into(), "empty".into()),
        ("accept-language".into(), "zh-CN,zh;q=0.9".into()),
        ("Agw-Js-Conv".into(), "str".into()),
        ("priority".into(), "u=1, i".into()),
    ])
}

/// 按路径微调请求头 —— 目前只有一条差异，但它出现在**所有** `/alice/ug/*` 上。
///
/// ── 差异是什么（2026-11-12 CDP 抓真机客户端全量报文）────────────
/// 客户端这些 `landing/*` 请求**不带 `Origin`**，`Referer` 是**任务中心页**
/// `https://www.doubao.com/pc/agent-explore?scroll_to=tasks` —— 不是站点根。
///
/// 上一版（11-10 据 Reqable）把 Referer 写成站点根 `/`，理由是「同源请求」。
/// 那只对了一半：同源确实不带 `Origin`（这条已采纳并保留），但 Referer 是
/// 发起文档的真实地址 —— 客户端是从任务中心页发的，所以带的是那一页的 URL。
/// 插件凭空发起时没有「当前文档」，用任务中心页最贴合：签到任务就长在那里。
///
/// 带 `Origin` = 「这是一个跨源 fetch，我声明来自哪里」；不带 = 「同源请求」。
/// 插件此前两个头都带（`Origin` + `/chat/` Referer），等于在风控侧自报
/// 「我是从 `/chat/` 页面跨源打过来的第三方」—— 与它实际的身份矛盾。
///
/// ⚠️ 只对 `/alice/ug/*` 生效。`/chat/completion` 那条转发链路一直是通的，
/// 不动它；`/passport/*`（账号识别）抓包里确实带 `Origin`，也不动。
pub fn headers_for(credentials: &Value, path: &str) -> Result<Vec<(String, String)>, GatewayError> {
    let mut list = headers(credentials)?;
    if path.starts_with("/alice/ug/") {
        list.retain(|(key, _)| !key.eq_ignore_ascii_case("origin"));
        for (key, value) in list.iter_mut() {
            if key.eq_ignore_ascii_case("referer") {
                *value = format!("{BASE_URL}/pc/agent-explore?scroll_to=tasks");
            }
        }
    }
    Ok(list)
}

// ── aid：真实客户端是 497858，不是 582478 ───────────────────
// 2026-11-12 用 CDP 直连真机客户端（`--remote-debugging-port=9342`）抓
// 任务中心签到，`landing/checkin` 的 `aid` / `real_aid` 是 **497858**。
//
// 早先记的 582478 来自**网页聊天页**的 onboarding 签到（那是另一条入口，
// 前端在 `doubao.com` 文档里自己发的 fetch），不是客户端任务中心。两者不
// 通用：拿 582478 打任务中心的 checkin，服务端按「aid 与活动组不匹配」处置。
//
// 取值收敛在 `super::sign::checkin_aid`，这里只留「为什么」。
//
// ⚠️ 这与「读接口」无关：`/chat/completion` 等转发接口一直工作正常，不要
// 为了这条签到改动它们。

/// 真实客户端的查询串参数，**按线上顺序**返回（不含 `a_bogus`，它由签名层补）。
///
/// ── 为什么抽成独立函数 ──────────────────────────────────────
/// 同一份参数现在有两个消费者：
/// · [`endpoint`] —— 插件自己的 reqwest 通道（能签名就签，签不上就只能不发）；
/// · `super::webview` —— 交给 Chromium 渲染进程发（网络栈自动补 `a_bogus`）。
/// 两边一旦各写一遍，字段就会漂移 —— 2026-11-10 那版正是这么错的（11 处偏差，
/// 见 [`endpoint`] 的差异清单）。参数只有一个定义处，改一处两边同时生效。
///
/// 返回 `Vec<(String, String)>` 而非 `&'static str` 键：`web_id` / `tea_uuid`
/// 这类是运行时取的，键虽然静态但值不是，统一成 owned 少一层转换。
pub fn query_pairs(credentials: &Value) -> Result<Vec<(String, String)>, GatewayError> {
    let client_version = credentials["clientVersion"]
        .as_str()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or(DEFAULT_CLIENT_VERSION);
    let aid = super::sign::checkin_aid(credentials);
    let mut query: Vec<(String, String)> = vec![
        ("version_code".into(), "20800".into()),
        ("language".into(), "zh".into()),
        ("device_platform".into(), "web".into()),
        ("doubao_device_platform".into(), "web".into()),
        ("aid".into(), aid.to_string()),
        ("real_aid".into(), aid.to_string()),
        ("pkg_type".into(), "release_version".into()),
        // 客户端**一定**带 `device_id` 这个键，但值是空串。这里保留占位而不是
        // 整条省略：少一个键与「键在但空」在服务端日志里是两种形状。
        ("device_id".into(), String::new()),
    ];
    query.push(("pc_version".into(), client_version.to_string()));
    query.push((
        "doubao_pc_version".into(),
        super::sign::doubao_pc_version(credentials).to_string(),
    ));
    // `web_id`：客户端由 `POST /alice/user/launch/core` 的响应下发
    // （`data.web_id`），之后每个请求都带。账号 Cookie 里没有这个键，所以
    // 只能缓存服务端下发的那一颗（`sign::store_web_id` / `sign::web_id`）。
    if let Some(value) = super::sign::web_id(credentials) {
        query.push(("web_id".into(), value));
    }
    // `tea_uuid`：埋点 SDK 的设备标识 = 本机 device_id。客户端**不把设备号
    // 放进 `device_id`，而是放进这里** —— 与上一版正好相反。
    if let Some(device) = super::client::device_id() {
        query.push(("tea_uuid".into(), device));
    }
    query.extend([
        ("region".into(), String::new()),
        ("sys_region".into(), String::new()),
        ("samantha_web".into(), "1".into()),
        ("web_platform".into(), "desktop".into()),
        ("use-olympus-account".into(), "1".into()),
    ]);
    // `web_tab_id`：客户端每次新开 webview 生成一颗 UUID，官方每个请求都带。
    // 同一会话复用同一颗（只在新开会话时换），否则「同一账号同一时刻多个
    // 不同 tab」本身就是异常信号。
    //
    // ⚠️ 只出现**一次**。2026-11-12 之前的版本在这里和 `runtime` 之后各推了
    // 一次，导致线上 query 出现重复键（真实客户端只有一颗）—— 重复键在
    // percent-encode 之后的签名字符串里是两颗不同的值，签名必然对不上。
    if let Some(tab) = super::sign::web_tab_id(credentials) {
        query.push(("web_tab_id".into(), tab));
    }
    // ⚠️ 顺序：`runtime_version` 在 `runtime` **之前**（真实客户端如此）。
    query.push((
        "runtime_version".into(),
        super::sign::doubao_pc_version(credentials).to_string(),
    ));
    query.push(("runtime".into(), "web".into()));
    Ok(query)
}

/// 组装一次上游请求的完整地址（含查询串）。
///
/// ── 为什么单独拆出来 ────────────────────────────────────────
/// Images 扩展只交「地址 + 请求头 + 请求体」，不经 `reqwest` 的查询串编码，
/// 但它必须用**同一份**查询参数：豆包风控认的是「这个账号在这台受信设备上」
/// （见 [`super::client`] 模块头），少了 `device_id` / `fp` 一定会被人机验证
/// 拦下 —— 生图走的就是同一个 `/chat/completion`，没有例外。所以两边共用本
/// 函数，编码按 [`encode`] 的 RFC3986 未保留字符集自己来。
///
/// ── 参数顺序照抄真实客户端（2026-11-12 CDP 抓包逐字段对齐）──
/// 真实顺序（22 个）：
/// `version_code, language, device_platform, doubao_device_platform, aid,
/// real_aid, pkg_type, device_id, pc_version, doubao_pc_version, web_id,
/// tea_uuid, region, sys_region, samantha_web, web_platform,
/// use-olympus-account, web_tab_id, runtime_version, runtime, msToken,
/// a_bogus`。
///
/// 与 11-10 那版（据 Reqable 报文对齐）的差异 —— 那版错了 11 处，逐条：
/// · `aid`/`real_aid`：582478 → **497858**（入口不同，见上方 aid 段）；
/// · `doubao_device_platform`：desktop → **web**；
/// · `device_id`：有值 → **空串占位**（客户端确实带这个键但值为空）；
/// · `pc_version`：2.33.5 → **2.32.6**；`doubao_pc_version`：跟 pc_version
///   同值 → **5.74.71**（它俩不是同一个数，是两个独立字段）；
/// · `runtime_version`：3.42.0 → **5.74.71**；
/// · `region`/`sys_region`：CN → **空串**；
/// · `web_tab_id` 位置：在 `use-olympus-account` 之后、`runtime*` 之前；
/// · `runtime` 顺序：在 `runtime_version` **之后**；
/// · **删掉**客户端根本没有的 4 个：`client_platform`、`chromium_version`、
///   `channel`、`fp`。
///
/// 签名（`a_bogus`）对 query 串是**顺序敏感**的，顺序错了签名就废，所以这里
/// 不能随手插字段。
pub fn endpoint(credentials: &Value, path: &str) -> Result<String, GatewayError> {
    let mut query = query_pairs(credentials)?;
    // msToken：官方每个请求的 URL 参数都带（响应经 `x-ms-token` 轮换，见
    // `sign` 模块头）。放 query 末尾，与官方参数顺序一致。
    //
    // 会话键走 `accounts::cookie_header` 再抽：快照可能存成数组形态
    // （`credentials.cookies`），那时 `cookieHeader` 已被 `credentials()` 移除，
    // 直接读那个键会恒空 —— 而 `cookie_header` 会从数组拼回来。拼不出来
    // （Cookie 本身非法）就留空，退全局份。
    //
    // **拿不到就整条不带**（`sign::ms_token` 返回 `None`）：msToken 是服务端
    // 加密下发的载荷，自造一颗塞进去只会被当成坏令牌 —— 那正是本改造要消除
    // 的东西。「不带」是改造前的形态，已被实测证明能过。
    let session = super::accounts::cookie_header(credentials)
        .map(|header| super::accounts::session_of_header(&header))
        .unwrap_or_default();
    if let Some(token) = super::sign::ms_token(&session) {
        query.push(("msToken".into(), token));
    }

    // ── 先编码再签名 ────────────────────────────────────────────
    // `a_bogus` 的签名对象是**线上那一串字节**（即已 percent-encode 的
    // query），不是解码后的语义值 —— 服务端是从收到的原始 URL 上重算的。
    // 顺序同理：签名对参数顺序敏感，所以 `query` 的顺序就是签名的输入，
    // 中途插字段会让签名废掉（见 [`query_pairs`] 的顺序注释）。
    let mut wire = pairs_to_wire(&query);

    // ── 签名范围：只签 `/alice/ug/*` ─────────────────────────────
    // 抓包（2026-11-05 ~ 11-10，6 次）证实客户端在 `landing/report`、
    // `landing/get`、`v3/detail`、`landing/checkin` 上**全都**带 `a_bogus`；
    // 而 `/chat/completion`、`/samantha/*` 不带。插件这条转发链路一直是通的，
    // 不给它加签名 —— 签名对不上时服务端直接判无效，比不带更糟。
    if path.starts_with("/alice/ug/") {
        if let Some(bogus) = super::sign::a_bogus(&wire, &samantha_ua(credentials), "POST") {
            // 值本身也要编码（官方签名里会出现 `/` `+` `=`）。
            wire.push_str("&a_bogus=");
            wire.push_str(&encode(&bogus));
        }
    }
    Ok(format!("{BASE_URL}{path}?{wire}"))
}

/// 把参数对拼成线上 query 串（percent-encode 后按顺序用 `&` 连接）。
///
/// 抽出来是因为它同时是**签名的输入**与**线上字节**：`webview` 通道也要拼出
/// 一模一样的一串（差别只在最后不追加 `a_bogus`，由浏览器网络栈补）。两处
/// 拼法一旦不一致，签名对象就变了。
pub fn pairs_to_wire(pairs: &[(String, String)]) -> String {
    pairs
        .iter()
        .map(|(key, value)| format!("{}={}", encode(key), encode(value)))
        .collect::<Vec<_>>()
        .join("&")
}

pub fn request(
    client: &reqwest::Client,
    credentials: &Value,
    path: &str,
) -> Result<reqwest::RequestBuilder, GatewayError> {
    let mut builder = client.post(endpoint(credentials, path)?);
    for (key, value) in headers_for(credentials, path)? {
        builder = builder.header(key, value);
    }
    Ok(builder)
}

/// 上游文案的统一前缀。
const LABEL: &str = "豆包工作：";

/// 给上游文案加一次「豆包工作：」前缀（**幂等**）。
///
/// ── 为什么必须幂等 ──────────────────────────────────────────
/// 转发失败时插件先把错误体（[`error_body`]）推给宿主，宿主紧接着又拿
/// **同一个体**回头调 `classify`；`classify` 会再走一次 [`error`]。不幂等的话
/// 用户看到的是「豆包工作：豆包工作：rate limited」—— 同一件事说两遍，还把
/// 真正有用的那半句挤到了后面（2026-11-02 在真实账号上逐字抓到）。
///
/// 判据用「是否已带前缀」而不是「调用次数」：两侧都可能先跑（宿主读 head 的
/// 错误体 / 先到的正文），靠计数去重会在任何一条路径改动后失效。
fn label_once(message: &str) -> String {
    if message.starts_with(LABEL) {
        message.to_string()
    } else {
        format!("{LABEL}{message}")
    }
}

/// 上游风控的人机验证判据：命中时返回场景名（如 `doubao_message_web`）。
///
/// ── 为什么必须单独认这一档 ──────────────────────────────────
/// 豆包风控（`shark_admin`）把「要人工过滑块」伪装成
/// `error_code=710022004 / error_msg=rate limited`：照字面读就是一条账号限额，
/// 于是用户看到的文案既说不清原因、也给不出下一步。实测（2026-11-02，两个
/// 账号复现）：去掉 `pc_version`、换参数组合、用浏览器 TLS 指纹重发，全都被
/// 同样拦下 —— 它是**账号 / 出口 IP 级的风控**，重发不会通过。
///
/// 真类型只写在 `extra.decision` 里（一段 JSON **字符串**）：
/// `{"from":"shark_admin","type":"verify","subtype":"slide","verify_scene":"…"}`。
/// 只认 `type=verify`，解析不出来就当普通错误，不去猜。
fn risk_verify(value: &Value) -> Option<String> {
    /// `decision` 里还塞着一段加密的挑战载荷，给个上限免得为一条错误体做大解析。
    const MAX_DECISION: usize = 64 * 1024;
    let extra = value
        .get("extra")
        .filter(|value| value.is_object())
        .unwrap_or(value);
    let decision = extra.get("decision").or_else(|| value.get("decision"))?;
    let parsed = match decision {
        Value::String(text) if text.len() <= MAX_DECISION => {
            serde_json::from_str::<Value>(text).ok()?
        }
        Value::Object(_) => decision.clone(),
        _ => return None,
    };
    if parsed.get("type").and_then(Value::as_str) != Some("verify") {
        return None;
    }
    Some(
        parsed
            .get("verify_scene")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string(),
    )
}

/// 风控要求人工验证时的文案：说清「这不是额度问题」与「下一步做什么」。
///
/// 状态码仍取 **429** —— 编排层对此的处置正是我们要的（把这个账号从本模型上
/// 挪开、换下一个候选），而 `mod.rs` 的 `classify` 按状态码分档，改动它的语义
/// 会牵动「退避 / 熔断」那一套（那是给瞬时故障准备的，对一个需要人工介入的
/// 账号更不合适）。
///
/// 文案里点明「所有模型都会被拦」：实测症状正是「请求任何模型都是同一条
/// 429」，不写清的话用户会逐个模型试过去（2026-11-02 的原始报告就是这样）。
fn verify_message(scene: &str) -> String {
    let scene = scene.trim();
    let scene = if scene.is_empty() {
        String::new()
    } else {
        format!("（场景 {scene}）")
    };
    format!(
        "触发了豆包上游的人机验证{scene}，不是账号额度问题（该账号当前所有模型都会被拦下）：\
         请在该账号的豆包网页端发一条消息、完成滑块验证后重试；\
         也可以等待约 30 分钟或更换出口网络再试。"
    )
}

pub fn error(status: u16, value: &Value) -> GatewayError {
    let detail = value
        .get("error_detail")
        .filter(|value| value.is_object())
        .or_else(|| value.get("error").filter(|value| value.is_object()))
        .unwrap_or(value);
    let code = number(&detail["code"])
        .or_else(|| number(&detail["error_code"]))
        .or_else(|| number(&value["code"]))
        .or_else(|| number(&value["error_code"]));
    let message = ["message", "msg", "error_msg", "error_message", "detail"]
        .iter()
        .find_map(|key| detail[*key].as_str().filter(|text| !text.trim().is_empty()))
        .or_else(|| value.as_str())
        .unwrap_or("上游返回了无法解析的错误");
    // 风控挑战先于状态码映射：`710022004` 只是它的外壳码，将来上游换码时
    // 「是不是人机验证」仍由 `extra.decision` 说了算。
    if let Some(scene) = risk_verify(value) {
        crate::logging::verbose(
            "[DoubaoWork]",
            &format!("上游要求人机验证（场景 {scene}），原始文案：{message}"),
        );
        return GatewayError::with_status(429, label_once(&verify_message(&scene)))
            .with_optional_code(code.and_then(|code| i64::try_from(code).ok()));
    }
    let lower = message.to_ascii_lowercase();
    let status = if matches!(code, Some(710012001))
        || status == 401
        || lower.contains("not logged in")
        || lower.contains("login required")
    {
        401
    } else if status == 429 || matches!(code, Some(710022004)) {
        429
    } else if status >= 400 {
        status
    } else {
        502
    };
    GatewayError::with_status(status as i32, label_once(message))
        .with_optional_code(code.and_then(|code| i64::try_from(code).ok()))
}

pub fn error_body(error: &GatewayError) -> Value {
    json!({"message":error.message,"code":error.upstream_code,"error":{"message":error.message,"code":error.upstream_code}})
}

pub async fn json_response(
    builder: reqwest::RequestBuilder,
    ug: bool,
) -> Result<Value, GatewayError> {
    let mut response = builder
        .send()
        .await
        .map_err(|error| GatewayError::with_status(502, crate::http::describe(&error)))?;
    // 官方每个响应都经 `x-ms-token` 响应头下发下一颗令牌（抓包 2026-11-08
    // 证实），接住回放即官方同一条轮换链路（见 `sign` 模块头）。`set-cookie`
    // 里取不到会话键（官方这批响应只下发 `bd_sso_hi3jfd`），所以这里统一记
    // 为全局份：`sign::ms_token` 的读取顺序是「会话份 → 全局份」，**都没有
    // 就不带**（不自造，见 `sign` 模块头）。全局份兜所有账号的首轮请求；
    // 某账号一旦用过全局份，它下一个响应里的新令牌又会刷新全局份 —— 相当于
    // 全体账号共享一条官方轮换链，形态与官方客户端一致（官方本来就是一台
    // 设备一颗令牌）。
    if let Some(token) = response
        .headers()
        .get("x-ms-token")
        .and_then(|value| value.to_str().ok())
    {
        super::sign::store_ms_token("", token);
    }
    let status = response.status().as_u16();
    let mut bytes = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|error| GatewayError::with_status(502, crate::http::describe(&error)))?
    {
        if bytes.len().saturating_add(chunk.len()) > 8 * 1024 * 1024 {
            return Err(GatewayError::with_status(502, "豆包工作控制面响应过大"));
        }
        bytes.extend_from_slice(&chunk);
    }
    let value: Value = serde_json::from_slice(&bytes).map_err(|_| {
        GatewayError::with_status(502, "豆包工作未返回有效 JSON，请检查登录态或稍后重试")
    })?;
    // `web_id` 由 `launch/core` 的响应下发（`data.web_id`），客户端拿到后回填
    // 到之后所有请求的 query 上（见 `sign::store_web_id`）。这里接住存起来，
    // 与 `x-ms-token` 同一条「只回放服务端下发值」的路子。
    if let Some(web_id) = value
        .pointer("/data/web_id")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|web_id| !web_id.is_empty())
    {
        super::sign::store_web_id("", web_id);
    }
    if !(200..300).contains(&status)
        || number(&value["code"]).is_some_and(|code| code != 0 && !(ug && code == 200))
    {
        return Err(error(status, &value));
    }
    Ok(value)
}

pub async fn post(
    client: &reqwest::Client,
    credentials: &Value,
    path: &str,
    body: &Value,
) -> Result<Value, GatewayError> {
    json_response(
        request(client, credentials, path)?.json(body),
        path.starts_with("/alice/ug/"),
    )
    .await
}

pub async fn account(
    ctx: &PluginCtx,
    id: &str,
    stream: bool,
) -> Result<(String, Value, reqwest::Client), GatewayError> {
    let found = ctx.store_get(PROVIDER_ID, id).await.map_err(|error| {
        GatewayError::with_status(401, format!("读取豆包工作账号失败：{error}"))
    })?;
    let proxy = match found.proxy {
        Some(StoreProxy::Resolved { proxy }) => Some(proxy),
        Some(StoreProxy::Static { config }) => {
            match crate::proxy_util::resolve_account_proxy(Some(&config)) {
                Some(crate::proxy_util::ProxyResolution::Resolved(proxy)) => Some(proxy),
                Some(crate::proxy_util::ProxyResolution::Failed(message)) => {
                    return Err(GatewayError::with_status(502, message))
                }
                None => None,
            }
        }
        Some(StoreProxy::Failed { message }) => {
            return Err(GatewayError::with_status(502, message))
        }
        _ => None,
    };
    let credentials = super::accounts::resolve(&found.secret).await?;
    let client = client(proxy.as_ref(), &ctx.env.proxy, stream)?;
    if credentials["expectedUserId"].is_string() {
        let profile = super::accounts::identify(&client, &credentials).await?;
        if profile["userId"] != credentials["expectedUserId"]
            || profile["enterpriseType"] != credentials["expectedEnterpriseType"]
        {
            return Err(GatewayError::with_status(
                401,
                "豆包工作客户端已切换账号，请重新导入当前登录态",
            ));
        }
        // 旧版「只存身份」的记录：这次既然已经从客户端读到了登录态，顺手存一份
        // 快照（见 `accounts::upgrade_snapshot`），下次客户端不在也能用。
        // 失败不影响本次请求 —— 升级只是顺手，不是前置条件。
        if let Some(upgraded) = super::accounts::upgrade_snapshot(&found.secret, &credentials) {
            if let Err(error) = ctx.store_update(&found.id, &found.secret, &upgraded).await {
                crate::logging::verbose(
                    "[DoubaoWork]",
                    &format!("旧账号升级为登录态快照失败（本次仍可用）：{error}"),
                );
            }
        }
    }
    Ok((found.id, credentials, client))
}

pub fn classify(status: u16, body: &Value) -> proto::ErrorClass {
    let error = error(status, body);
    if error.status_code == 401 {
        return proto::ErrorClass::TokenExpired {
            message: error.message,
        };
    }
    if error.status_code == 429 {
        return proto::ErrorClass::QuotaLimited {
            reset_at: None,
            message: error.message,
            upstream_code: error.upstream_code,
            status: 429,
        };
    }
    proto::ErrorClass::Fatal {
        status: error.status_code as u16,
        message: error.message,
        upstream_code: error.upstream_code,
    }
}
