//! 借一个真实 Chromium 渲染栈发 `landing/checkin` —— 唯一能拿到 `a_bogus` 的通道。
//!
//! ── 为什么必须借浏览器（2026-11-12 逐层实测）─────────────────────
//! `/alice/ug/campaign/landing/checkin` 上带 184 字符 `a_bogus`（解码 136 字节
//! 纯二进制）。这串东西**不在任何 JS 里算**：把任务中心页 152 个脚本全部
//! `Debugger.getScriptSource` 下来数一遍，`a_bogus` 出现 **0 次**；页面能调到的
//! `byted_acrawler.frontierSign` / `window.bdms.frontierSign` 都只回 16 字符
//! `X-Bogus`。自研 `abogus.rs`（抖音结构）与豆包不同构，反推零命中。
//!
//! 但**只要请求从 Chromium 的渲染进程发出**，它就自动带上完整签名了。实测
//! 四种环境（都不自己写 `a_bogus`）：
//!
//! | 环境 | wire 上的 `a_bogus` | 服务端回执 |
//! |------|-------------------|-----------|
//! | 豆包客户端 webview | 184 字符 | 签名通过 |
//! | **普通 Chrome** | 194 字符 | 签名通过（`v3/detail` → `code:0`） |
//! | **--headless=new Chrome** | 194 字符 | 签名通过（`v3/detail` → `code:0`） |
//! | 纯 Rust reqwest | 无 | 710010702，且封该 uid 当天签到口 |
//!
//! 签名发生在**渲染进程把请求交给 Chromium 网络栈**的那一刻，与「是不是豆包
//! 客户端」无关。所以插件不需要用户先开豆包客户端 —— 自己起一个 headless
//! Chrome，用完就关，即可静默签到。
//!
//! ── 四条已实测的硬约束（别再踩）────────────────────────────
//! 1. **拦截器只补 `msToken` + `a_bogus`，不补业务参数。**
//!    裸路径 fetch 的 wire 上只有 `msToken, a_bogus`，服务端回
//!    `{"code":1,"message":"invalid param"}`。22 个业务参数必须由我们写全。
//! 2. **签到前必须先发一条读请求预热。**
//!    页面加载完直接发 `report` → `checkin`，wire 上**可能没有 `a_bogus`**
//!    （实测到过一次）。先发 `v3/detail` 之后，后续每条都稳定带签名 ——
//!    首条请求是签名拦截器接管前的「踩空」那一条。
//! 3. **UA / `Agw-Js-Conv` 等进阶头都不影响加签。**
//!    4 组单变量对照全过（裸 `HeadlessChrome/157` 同样被签名）。伪客户端 UA
//!    只是为了少一个「同账号两个浏览器指纹」的异常信号，不是必要条件。
//! 4. **CDP 状态是会话级的。**
//!    `Network.setCookie` / `Network.setUserAgentOverride` 写在**某条 WebSocket
//!    会话**上；换成另一条连接就丢了。所以全程复用一条连接，不逐条重连。
//!
//! ── 通道优先级 ────────────────────────────────────────────
//! 1. **已有浏览器调试端口**（[`EXISTING_PORT`]，9342）：豆包客户端或用户自己的
//!    Chrome 带着 `--remote-debugging-port` 跑着，直接借用，零启动开销；
//! 2. **自己拉起 headless Chrome/Edge**（[`SPAWNED_PORT`]，9343）：用户什么都
//!    没开也能签。复用同一条「注入 cookie → 预热 → fetch」的流程。
//!
//! 通道 1 只挑**已经在 doubao.com 上的页面**（不去导航用户自己的标签页）；
//! 通道 2 是自己拉起来的，导航随便做。
//!
//! ── 失败时怎么办 ────────────────────────────────────────────
//! 两条都不可用（没装 Chrome/Edge、启动失败、超时）返回 `None`，由调用方退回
//! 「引导用户手点」。**绝不**退回「插件 reqwest 自己发裸请求」—— 那条路会把
//! uid 当天封死（见 [`super::rewards`] 的 `CHECKIN_BLOCKED`）。

use std::time::Duration;

use futures::{SinkExt, StreamExt};
use serde::Deserialize;
use serde_json::{json, Value};
use tokio::net::TcpStream;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};

/// 借用已有浏览器调试端口时用它（豆包客户端默认值，也是本插件 CDP 通道的惯例）。
const EXISTING_PORT: u16 = 9342;
/// 自己拉起 headless 浏览器时用这个端口，与 [`EXISTING_PORT`] 错开：
/// 两个流程可能同时进行（用户在客户端里手点、插件在后台自动签），
/// 端口撞上会互相抢 CDP 连接。
const SPAWNED_PORT: u16 = 9343;

/// 等调试端口起来的上限。headless 冷启动在机械盘上要十几秒。
const CONNECT_TIMEOUT: Duration = Duration::from_secs(25);
/// 等签名 SDK（`byted_acrawler`）就绪的上限。实测 14~20 秒才拿到。
const READY_TIMEOUT_MS: u64 = 30_000;
/// 单条 `Runtime.evaluate` 的超时。签到是一次外网 POST，留足余量。
const CALL_TIMEOUT: Duration = Duration::from_secs(30);

/// 伪客户端 UA。实测不影响加签（见模块头的约束 3）。
const UA: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 \
                  (KHTML, like Gecko) Chrome/147.0.0.0 Safari/537.36 SamanthaDoubao/2.32.6";

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Target {
    id: String,
    #[serde(rename = "type")]
    kind: String,
    url: String,
    web_socket_debugger_url: Option<String>,
}

fn is_page(target: &Target) -> bool {
    matches!(target.kind.as_str(), "page" | "webview")
}

fn on_doubao(target: &Target) -> bool {
    url::Url::parse(&target.url)
        .map(|url| url.scheme() == "https" && url.host_str() == Some("www.doubao.com"))
        .unwrap_or(false)
}

fn http() -> Option<reqwest::Client> {
    reqwest::Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(Duration::from_secs(5))
        .build()
        .ok()
}

/// 在 `port` 上找一个页面目标。
///
/// `require_doubao` 区分两条通道的意图：
/// · `true` —— 借用别人的浏览器，只认已经在 doubao.com 上的页（不去导航别人的
///   标签页）；找不到就说明这条路不可用，交给通道 2；
/// · `false` —— 自己拉起的浏览器，`about:blank` 也算，随后由 [`checkin_at`]
///   导航过去。
///
/// 无论哪种，ws 地址都必须**严格**是本机这个端口上的
/// `/devtools/page/{id}` —— 防止 `/json/list` 被替换成指向别处的地址。
async fn page_at(port: u16, require_doubao: bool) -> Option<Target> {
    let targets: Vec<Target> = http()?
        .get(format!("http://127.0.0.1:{port}/json/list"))
        .send()
        .await
        .ok()?
        .json()
        .await
        .ok()?;
    if targets.len() > 256 {
        return None;
    }
    let target = targets
        .into_iter()
        .filter(is_page)
        .find(|target| !require_doubao || on_doubao(target))?;
    let url = url::Url::parse(target.web_socket_debugger_url.as_deref().unwrap_or("")).ok()?;
    if url.scheme() != "ws"
        || url.host_str() != Some("127.0.0.1")
        || url.port() != Some(port)
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.path() != format!("/devtools/page/{}", target.id)
    {
        return None;
    }
    Some(target)
}

/// 一条常驻的 CDP 会话。
///
/// ── 为什么必须常驻 ──────────────────────────────────────────
/// `Network.setCookie` / `Network.setUserAgentOverride` 是**会话级**状态。每次
/// 命令新开一条 WebSocket 会把这些设置随连接一起丢掉 —— cookie 注不进去，
/// 页面就是未登录态，预热直接回 401/未登录。所以一条连接跑到底，靠自增 id
/// 多路复用。
type Socket = WebSocketStream<MaybeTlsStream<TcpStream>>;

struct Session {
    socket: Socket,
    next: u64,
}

impl Session {
    async fn open(ws: &str) -> Option<Self> {
        let (socket, _) = tokio::time::timeout(CONNECT_TIMEOUT, tokio_tungstenite::connect_async(ws))
            .await
            .ok()?
            .ok()?;
        Some(Self { socket, next: 1 })
    }

    /// 发一条命令并等它的 `result`。事件帧（`method` 字段）一律丢弃 ——
    /// 本模块不依赖网络事件，只看返回值。
    async fn call(&mut self, method: &str, params: Value) -> Option<Value> {
        let id = self.next;
        self.next += 1;
        let payload = json!({"id": id, "method": method, "params": params});
        tokio::time::timeout(CALL_TIMEOUT, async {
            self.socket
                .send(Message::Text(payload.to_string().into()))
                .await
                .ok()?;
            while let Some(message) = self.socket.next().await {
                let Message::Text(text) = message.ok()? else {
                    continue;
                };
                let value: Value = serde_json::from_str(text.as_str()).ok()?;
                if value["id"] != id {
                    continue;
                }
                if value.get("error").is_some() {
                    return None;
                }
                return Some(value["result"].clone());
            }
            None
        })
        .await
        .ok()
        .flatten()
    }

    /// 跑一段异步表达式，把它 `return` 的字符串解析成 JSON。
    ///
    /// 脚本一律 `return JSON.stringify(...)`：`returnByValue` 直接给对象时，
    /// 大响应（detail 有几万字）在部分 Chrome 版本上会被截断成 `undefined`。
    async fn evaluate(&mut self, expression: &str, wait: Duration) -> Option<Value> {
        let result = self
            .call(
                "Runtime.evaluate",
                json!({
                    "expression": expression,
                    "awaitPromise": true,
                    "returnByValue": true,
                    "timeout": wait.as_millis() as u64,
                }),
            )
            .await?;
        let text = result["result"]["value"].as_str()?;
        serde_json::from_str::<Value>(text).ok()
    }
}

/// 页面脚本的公共前奏：把参数对拼成 query 串并 POST。
///
/// `pairs_json` 是 `[[name, value], …]` 的 JSON 字面量，由 Rust 侧
/// [`super::api::query_pairs`] 生成 —— 参数**只在 Rust 里定义一次**，JS 侧只做
/// 「按顺序拼接」。两边各写一份必然漂移，2026-11-10 那版就是那么错的（11 处
/// 字段偏差）。
fn preamble(pairs_json: &str) -> String {
    format!(
        r#"const P = {pairs_json};
  const Q = () => P.map(([k, v]) => k + '=' + encodeURIComponent(v)).join('&');
  const BASE = 'https://www.doubao.com';
  const post = (path, body) => fetch(BASE + path + '?' + Q(), {{
    method: 'POST', credentials: 'include',
    headers: {{'Content-Type': 'application/json'}},
    body: JSON.stringify(body)
  }}).then(r => r.text());"#
    )
}

/// 预热脚本：读一次活动详情。
///
/// 两个作用（见模块头约束 2）：
/// · 让签名拦截器接管后续请求 —— 首条请求会「踩空」不带 `a_bogus`；
/// · 带回 `data.tasks`，签到任务今天是否已完成在这里就能读到，不必让写请求
///   去撞一次 710010702 才知道。
fn script_warmup(pairs_json: &str) -> String {
    format!(
        r#"(async () => {{
  {preamble}
  try {{
    const text = await post('/alice/ug/campaign/v3/detail/',
                           {{campaign_group_code: 'pc_incentive_task_mvp_v3'}});
    return text;
  }} catch (e) {{ return JSON.stringify({{err: String(e)}}); }}
}})()"#,
        preamble = preamble(pairs_json),
    )
}

/// 签到脚本：照客户端顺序先 `report` 再 `checkin`（间隔 20ms，不等回执）。
///
/// URL 只写业务参数，**不写 `a_bogus`** —— 由 Chromium 网络栈补。
fn script_checkin(pairs_json: &str) -> String {
    format!(
        r#"(async () => {{
  {preamble}
  const out = {{}};
  try {{
    out.report = await post('/alice/ug/campaign/landing/report',
                            {{event_type: 'user_operate', biz_event_name: 'check_in'}});
  }} catch (e) {{ out.report = String(e); }}
  await new Promise(r => setTimeout(r, 20));
  try {{
    out.checkin = await post('/alice/ug/campaign/landing/checkin',
                             {{scene: 'pc_incentive_task_mvp_v3'}});
  }} catch (e) {{ out.checkin = String(e); }}
  return JSON.stringify(out);
}})()"#,
        preamble = preamble(pairs_json),
    )
}

/// 预热回执里「今天已经签过了」是否成立。
///
/// ── 为什么要在这里判，而不是照发写请求 ──────────────────────
/// 用户在界面上连点两次「签到」时，第二次如果还发 `landing/checkin`，那就是
/// 一条**多余的写请求** —— 而这类写请求正是把该 uid 当天封死的东西
/// （见 [`super::rewards`] 的 `CHECKIN_BLOCKED`）。预热已经把权威状态拿回来
/// 了，就地判掉，写请求一次都不用发。
///
/// 判据与 `rewards::tasks` 同源（`campaign_code == "daily_signin"` +
/// `daily_checkin.checked_in_today`），只是压成一行谓词。判错的方向是安全的：
/// 判不出来返回 `false`，照旧发写请求，与改造前的行为一致。
fn signed_in(warmup: &Value) -> bool {
    fn walk(value: &Value, depth: usize) -> bool {
        if depth > 8 {
            return false;
        }
        if value
            .get("campaign_code")
            .and_then(Value::as_str)
            .is_some_and(|code| code == "daily_signin")
            && value
                .pointer("/daily_checkin/checked_in_today")
                .and_then(Value::as_bool)
                == Some(true)
        {
            return true;
        }
        match value {
            Value::Object(map) => map.values().any(|child| walk(child, depth + 1)),
            Value::Array(rows) => rows.iter().any(|child| walk(child, depth + 1)),
            _ => false,
        }
    }
    walk(warmup, 0)
}

/// 一次浏览器通道的结果。
///
/// ── 为什么 `checkin` 是 `Option` ────────────────────────────
/// 预热（纯读）回来时可能已经知道「今天签过了」。那种情况下**不该再发**
/// `landing/checkin` —— 每一条写请求都是一次新的风控判定，多发的这一条
/// 只会白白消耗掉当天的签到口（见 [`super::rewards`] 的 `CHECKIN_BLOCKED`）。
/// 所以用 `None` 表示「没发写请求」，而不是拿一个空对象冒充回执 ——
/// 空对象一旦被下游当成真回执解析，就会得出「上游没认」的错误结论。
pub struct Attempt {
    /// `landing/checkin` 的回执原文；`None` = 预热已判定今天签过，未发写请求。
    pub checkin: Option<Value>,
    /// 预热（`v3/detail`）的回执原文。
    pub warmup: Value,
}

/// 在某个调试端口上完成一次签到。
///
/// `navigate` 表示这个页面是不是「可以随便导航」的（自己拉起来的浏览器才允许）。
async fn checkin_at(
    port: u16,
    cookies: &[(String, String)],
    pairs: &[(String, String)],
    navigate: bool,
) -> Option<Attempt> {
    let target = page_at(port, !navigate).await?;
    let ws = target.web_socket_debugger_url.clone()?;
    let mut session = Session::open(&ws).await?;
    let pairs_json = serde_json::to_string(pairs).ok()?;

    if navigate && !on_doubao(&target) {
        session
            .call(
                "Page.navigate",
                json!({"url": "https://www.doubao.com/pc/agent-explore?scroll_to=tasks"}),
            )
            .await?;
        tokio::time::sleep(Duration::from_millis(2_000)).await;
    }

    // UA 对齐客户端（不影响签名，见模块头约束 3）。
    let _ = session
        .call(
            "Network.setUserAgentOverride",
            json!({"userAgent": UA, "acceptLanguage": "zh-CN,zh;q=0.9", "platform": "Win32"}),
        )
        .await;

    // 注入账号 cookie。逐条写、失败的跳过：不是每条都必要，
    // 但 `sessionid` 必须成 —— 没有它就等于未登录，预热会直接回未登录。
    let mut injected = 0;
    for (name, value) in cookies {
        let written = session
            .call(
                "Network.setCookie",
                json!({
                    "name": name, "value": value, "domain": ".doubao.com",
                    "path": "/", "secure": true, "httpOnly": false,
                }),
            )
            .await;
        if written
            .as_ref()
            .and_then(|result| result["success"].as_bool())
            .unwrap_or(false)
        {
            injected += 1;
        }
    }
    if injected == 0 {
        return None;
    }

    // 等签名 SDK 就绪。拿 `byted_acrawler` 是否存在作判据（比时间戳更直接）。
    let deadline = std::time::Instant::now() + Duration::from_millis(READY_TIMEOUT_MS);
    loop {
        let ready = session
            .evaluate(
                "JSON.stringify({ready: typeof window.byted_acrawler === 'object'})",
                Duration::from_secs(5),
            )
            .await;
        if ready
            .as_ref()
            .and_then(|value| value["ready"].as_bool())
            .unwrap_or(false)
        {
            break;
        }
        if std::time::Instant::now() >= deadline {
            // 没就绪也继续试：`detail` 尚且能通过（签名由网络栈给，不依赖 JS SDK
            // 完全就绪），只有 checkin 可能需要更完整的信号面。
            break;
        }
        tokio::time::sleep(Duration::from_millis(800)).await;
    }

    // 预热：读一次 detail。失败即认为这条通道不成立（cookie 没进去/页面没加载好），
    // 直接返回 None 让调用方换通道，而不是拿写请求去试探。
    let warmup = session.evaluate(&script_warmup(&pairs_json), CALL_TIMEOUT).await?;
    if warmup.get("code").is_none() && warmup.get("data").is_none() {
        return None;
    }

    // ── 已经签过就收工，不写 ────────────────────────────────────
    // 预热是纯读，不消耗任何「写接口配额」；而 `landing/checkin` 每发一次都
    // 是一次新的风控判定。今天已经签过还再发一遍，纯属自找麻烦。
    // 这里返回 `checkin: None`，调用方靠 `warmup` 就能报「今日已领取」。
    if signed_in(&warmup) {
        let _ = session.socket.close(None).await;
        return Some(Attempt {
            checkin: None,
            warmup,
        });
    }

    let response = session.evaluate(&script_checkin(&pairs_json), CALL_TIMEOUT).await?;
    let checkin = response
        .get("checkin")
        .and_then(Value::as_str)
        .and_then(|text| serde_json::from_str::<Value>(text).ok())
        .unwrap_or_else(|| response.clone());
    let _ = session.socket.close(None).await;
    Some(Attempt {
        checkin: Some(checkin),
        warmup,
    })
}

/// 自己拉起一个 headless Chromium（Chrome 优先，Edge 兜底），返回
/// `(子进程, profile 目录)`。调用方负责在用完后 kill 并删目录。
fn launch(port: u16) -> Option<(tokio::process::Child, std::path::PathBuf)> {
    let profile = std::env::temp_dir().join(format!("agent2api-doubao-{port}"));
    // 上一轮被强杀可能留下带 cookie 的 profile，先清掉再建。
    let _ = std::fs::remove_dir_all(&profile);
    let _ = std::fs::create_dir_all(&profile);
    for exe in browsers() {
        if !exe.exists() {
            continue;
        }
        let mut command = tokio::process::Command::new(&exe);
        command
            .args([
                format!("--remote-debugging-port={port}"),
                "--remote-allow-origins=*".to_string(),
                format!("--user-data-dir={}", profile.display()),
                "--headless=new".to_string(),
                "--no-first-run".to_string(),
                "--no-default-browser-check".to_string(),
                "--disable-gpu".to_string(),
                "--no-sandbox".to_string(),
                "--disable-dev-shm-usage".to_string(),
                "--disable-extensions".to_string(),
                "--disable-background-networking".to_string(),
                "--mute-audio".to_string(),
                "about:blank".to_string(),
            ])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null());
        // Windows 上隐藏控制台窗口。
        #[cfg(windows)]
        {
            command.creation_flags(0x0800_0000);
        }
        if let Ok(child) = command.spawn() {
            return Some((child, profile));
        }
    }
    None
}

/// 可用的 Chromium 系浏览器，按优先级。Edge 在 Windows 上必然存在，作为兜底。
fn browsers() -> Vec<std::path::PathBuf> {
    let mut list = Vec::new();
    #[cfg(windows)]
    {
        let roots = [
            std::env::var_os("PROGRAMFILES"),
            std::env::var_os("PROGRAMFILES(X86)"),
            std::env::var_os("LOCALAPPDATA"),
        ];
        for root in roots.into_iter().flatten() {
            let root = std::path::PathBuf::from(root);
            list.push(
                root.join("Google")
                    .join("Chrome")
                    .join("Application")
                    .join("chrome.exe"),
            );
            list.push(
                root.join("Microsoft")
                    .join("Edge")
                    .join("Application")
                    .join("msedge.exe"),
            );
        }
    }
    #[cfg(target_os = "macos")]
    {
        for (app, binary) in [
            ("Google Chrome", "Google Chrome"),
            ("Microsoft Edge", "Microsoft Edge"),
        ] {
            list.push(
                std::path::PathBuf::from("/Applications")
                    .join(format!("{app}.app"))
                    .join("Contents")
                    .join("MacOS")
                    .join(binary),
            );
        }
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        for name in [
            "google-chrome",
            "google-chrome-stable",
            "chromium",
            "chromium-browser",
            "microsoft-edge",
        ] {
            if let Ok(path) = which(name) {
                list.push(path);
            }
        }
    }
    list
}

#[cfg(all(unix, not(target_os = "macos")))]
fn which(name: &str) -> Result<std::path::PathBuf, ()> {
    let path = std::env::var_os("PATH").ok_or(())?;
    for dir in std::env::split_paths(&path) {
        let candidate = dir.join(name);
        if candidate.is_file() {
            return Ok(candidate);
        }
    }
    Err(())
}

/// 这台机器上有没有可用的 Chromium 系浏览器（给调用方写**准确**的失败文案用）。
///
/// 只回答「有没有装」，不回答「能不能连通签名」—— 后者要起进程、注入 cookie、
/// 预热才知道，代价太高，不适合放进一句错误文案的判定里。
pub fn chromium_available() -> bool {
    browsers().iter().any(|exe| exe.exists())
}

/// 在任意可用通道上完成一次签到，返回预热回执与（可能没有的）写回执。
///
/// 顺序：已有调试端口 → 自己拉起 headless。自己拉起的那条**必然**在用完后
/// kill 掉，不留后台进程，也不留带 cookie 的 profile 目录。
pub async fn checkin(
    cookies: &[(String, String)],
    pairs: &[(String, String)],
) -> Option<Attempt> {
    // 通道 1：已有的调试端口（豆包客户端或用户自己的 Chrome 开着）。
    // 只认 doubao.com 上的页，不导航别人的标签页。
    if let Some(value) = checkin_at(EXISTING_PORT, cookies, pairs, false).await {
        return Some(value);
    }

    // 通道 2：自己拉起 headless。
    let (mut child, profile) = launch(SPAWNED_PORT)?;
    let deadline = std::time::Instant::now() + CONNECT_TIMEOUT;
    let mut ready = false;
    while std::time::Instant::now() < deadline {
        if page_at(SPAWNED_PORT, false).await.is_some() {
            ready = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(400)).await;
    }
    let result = if ready {
        checkin_at(SPAWNED_PORT, cookies, pairs, true).await
    } else {
        None
    };
    let _ = child.kill().await;
    let _ = child.wait().await;
    // profile 目录带 cookie 落盘，用完就删，不留痕。
    let _ = std::fs::remove_dir_all(&profile);
    result
}