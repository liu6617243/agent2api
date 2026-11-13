//! **让 SSO 票根不死**：把华为云账号的「会话超时策略」拉到官方上限（24 小时）。
//!
//! ── 这是「保活」的最后一块拼图（2026-11-13 实测）───────────────
//! 前面几个模块（`portal_session`）解决的是「票根闲置会过期」——按 8 分钟一次的
//! 节奏拿它换票，服务端的闲置计时器就一直被重置。但那条机制有一个**前提**：
//! 网关得在跑。而真实使用节奏是「白天零星用、晚上关机」，每天离线 9~14 小时：
//!
//! ```text
//!   11-12 00:04 → 09:36   离线 9.5h
//!   11-12 23:52 → 11-13 11:44  离线 11.9h
//!   11-10 22:22 → 11-11 12:35  离线 14.2h
//! ```
//!
//! 而账号的会话超时策略实测是 **60 分钟**（`session_timeout: 60`，见下）：
//! ```text
//!   GET https://iam.cn-north-4.myhuaweicloud.com
//!        /v3.0/OS-SECURITYPOLICY/domains/{domain_id}/login-policy
//!   → {"login_policy":{..., "session_timeout":60, ...}}
//! ```
//!
//! **60 分钟的窗口装不下 14 小时的关机。** 所以不管保活写得多勤，只要用户睡一觉，
//! 票根必死 —— 这正是「写了滚动续期却还是保不住」的真正原因，也是这两次返工都
//! 没找对的地方：眼睛一直盯着「怎么推得更勤」，而瓶颈在**窗口本身太短**。
//!
//! ── 这一项为什么能改、改了有什么代价 ──────────────────────────
//! 官方文档（`PUT /v3.0/OS-SECURITYPOLICY/domains/{domain_id}/login-policy`）里
//! `session_timeout` 的合法区间是 **15 ~ 1440 分钟**。取上限 1440（24 小时）：
//!   · 关机 14 小时 < 24 小时 → 票根不会因为「一夜没动」被清；
//!   · 只要白天网关在跑，[`super::portal_session::keepalive`] 每 8 分钟把它往前推一次，
//!     窗口永远落在「最近一次查询 + 24 小时」上。
//! 两者合起来才是完整的「不死」：**窗口够长 + 一直在用**。
//!
//! ⚠️ 代价要说清楚：这是**账号级**设置（对账号下所有 IAM 用户生效），把网页控制台
//! 的闲置登出从 1 小时放宽到 24 小时。对「一台常开的个人开发机」是合理取舍，
//! 对高安全要求的账号则不是 —— 所以本模块**只往上调、不往下调**，且每次改动都
//! 落一行 `info` 日志说明改了什么。
//!
//! ── 权限 ────────────────────────────────────────────────────
//! 调这个接口需要 Security Administrator 权限。实测本账号的 STS 临时凭证**有**
//! （`PUT` 回 200）。没有权限时上游回 403：那不是故障，记一条「需要管理员手动调」
//! 的提示就够，不重试、不刷屏。
//!
//! ── 硬约束 ──────────────────────────────────────────────────
//! release 是 `panic=abort`：本文件零 unwrap / expect / panic。

use std::time::Duration;

use agent2api_provider_protocol::WireProxy as ResolvedProxy;
use agent2api_provider_sdk::PluginCtx;
use serde_json::{json, Value};

use super::credentials::{
    Credential, SESSION_WINDOW_FIELD, SESSION_WINDOW_SET_AT_FIELD,
};
use super::{oauth, redact, signer};

/// IAM 控制面（与 `models::DEFAULT_BASE_URL` 同区域：本家恒 cn-north-4）。
const IAM_ORIGIN: &str = "https://iam.cn-north-4.myhuaweicloud.com";

/// 登录策略的路径模板（`{domain}` 换成账号的 domain id）。
const LOGIN_POLICY_PATH: &str = "/v3.0/OS-SECURITYPOLICY/domains/{domain}/login-policy";

/// 我们要的会话窗口：**官方上限 24 小时**。
///
/// 取上限而不是「够用就好」：用户关机时长不固定（实测最长 14.2 小时），
/// 而 15~1440 这个区间里再往上没有别的收益 —— 24 小时是能拿到的最宽窗口，
/// 也是唯一能把「关机一夜」覆盖掉的值。
pub const WANTED_SESSION_TIMEOUT_MINUTES: i64 = 1440;

/// 进程内复查间隔。
///
/// 窗口是 24 小时量级的设置，没必要频繁确认：一天查一次足够（用户手动改回去
/// 也能在一天内被纠正）。进程重启会重查一次 —— 那正好覆盖「刚开机」这个时刻。
const RECHECK: Duration = Duration::from_secs(6 * 60 * 60);

/// 失败后多久再试。
///
/// ── 为什么不能沿用 [`RECHECK`]（2026-11-13 修的一处逻辑洞）──────
/// 节流表若「先记时刻、再干活」，那么一次因 STS 刚好过期而失败的尝试，会把
/// 这个账号锁住 6 小时 —— 而 STS 每小时就换一次，下一次心跳（60 秒后）本来是
/// 能成的。**失败必须比成功更快回来**，否则这条机制会以「看起来在跑、实际六个月
/// 没生效」的方式静默失效，正是本次要修的那一类。
///
/// 10 分钟：够短，能在一两次心跳内自愈；够长，不至于在上游持续 403 时刷接口。
const RETRY_AFTER_FAILURE: Duration = Duration::from_secs(10 * 60);

/// 进程内「下次允许检查的时刻」表（键 = 账号 id）。
///
/// 只做节流，不含凭据。锁中毒时返回 `true`（= 该查）：宁可多打一次 IAM，
/// 也不要让一个坏锁把这条机制停掉。
fn checked_marks() -> &'static std::sync::Mutex<std::collections::HashMap<String, std::time::Instant>> {
    static MARKS: std::sync::OnceLock<
        std::sync::Mutex<std::collections::HashMap<String, std::time::Instant>>,
    > = std::sync::OnceLock::new();
    MARKS.get_or_init(|| std::sync::Mutex::new(std::collections::HashMap::new()))
}

/// 现在能不能查（并在能查时**先**把下次时刻推后 [`RECHECK`]）。
fn claim_check(account_id: &str) -> bool {
    let now = std::time::Instant::now();
    let Ok(mut marks) = checked_marks().lock() else { return true };
    match marks.get(account_id) {
        Some(next) if now < *next => false,
        _ => {
            marks.insert(account_id.to_string(), now + RECHECK);
            true
        }
    }
}

/// 这一轮没成：把下次时刻拉近到 [`RETRY_AFTER_FAILURE`]（见那段说明）。
fn release_check(account_id: &str) {
    let Ok(mut marks) = checked_marks().lock() else { return };
    marks.insert(account_id.to_string(), std::time::Instant::now() + RETRY_AFTER_FAILURE);
}

/// **入口**：确保这个账号的会话窗口够宽（后台跑，不阻塞调用方）。
///
/// 调用点是余额查询心跳（`credits::fetch_renewing_with_state`），与推票根同一处 ——
/// 那条心跳是插件唯一稳定的「每隔一分钟就会被叫一次」的地方。
///
/// 没有 `domain_id` / AK / SK 时直接跳过：那三类凭据缺一就发不出可签名的请求，
/// 而「调不了策略」不该影响余额读数。
pub fn keepalive(
    ctx: &PluginCtx,
    account_id: &str,
    credential: &Credential,
    proxy: Option<&ResolvedProxy>,
) {
    if credential.domain_id.trim().is_empty()
        || credential.access_key_id.trim().is_empty()
        || credential.secret_access_key.trim().is_empty()
    {
        return;
    }
    if !claim_check(account_id) {
        return;
    }
    let ctx = ctx.clone();
    let account = account_id.to_string();
    let credential = credential.clone();
    let proxy = proxy.cloned();
    tokio::spawn(async move {
        ensure_wide_window(&ctx, &account, &credential, proxy.as_ref()).await;
    });
}

/// 读窗口 → 不够就调 → 落盘。失败只记日志（这条链路不该影响任何读数）。
async fn ensure_wide_window(
    ctx: &PluginCtx,
    account_id: &str,
    credential: &Credential,
    proxy: Option<&ResolvedProxy>,
) {
    let (current, policy) = match read_window(credential, proxy).await {
        Ok(found) => found,
        Err(reason) => {
            release_check(account_id);
            verbose(&format!("会话窗口未能读取（{reason}）：十分钟后再试"));
            return;
        }
    };
    if current >= WANTED_SESSION_TIMEOUT_MINUTES {
        // 已经够宽（多半是上一轮改的）：只把读数记下来，供账号页显示。
        persist_window(ctx, account_id, current).await;
        return;
    }
    match write_window(credential, &policy, proxy).await {
        Ok(minutes) => {
            crate::logging::log(
                "[CodeArts]",
                &format!(
                    "已把华为云账号的会话窗口从 {current} 分钟调到 {minutes} 分钟（官方上限）：\
                     这样关机一夜（实测最长 14.2 小时）也不会让 SSO 票根失效"
                ),
            );
            persist_window(ctx, account_id, minutes).await;
        }
        Err(reason) => {
            // 没权限 / 上游抖动：说清楚「谁需要做什么」，十分钟后再试一次。
            release_check(account_id);
            verbose(&format!(
                "会话窗口仍是 {current} 分钟，自动上调失败（{reason}）。\
                 若要彻底避免关机后票根失效，请在华为云 IAM 控制台 \
                 「安全设置 → 登录验证策略 → 会话超时策略」改成 24 小时"
            ));
        }
    }
}

/// 读当前策略：返回 `(session_timeout, 整份 login_policy)`。
///
/// 把整份带出来是为了 [`write_window`] 能**只改 `session_timeout` 这一项**，
/// 其余可写字段按原值回填 —— 这个接口是 PUT 语义，少带一个字段就等于把它重置成
/// 服务端默认值（比如用户特意放宽过的账号锁定策略会被悄悄改回去）。
async fn read_window(
    credential: &Credential,
    proxy: Option<&ResolvedProxy>,
) -> Result<(i64, Value), String> {
    let url = policy_url(credential);
    let headers = vec![("Accept".to_string(), "application/json".to_string())];
    let (status, body) = signed_call("GET", &url, headers, b"", credential, proxy).await?;
    if status != 200 {
        return Err(format!(
            "HTTP {status}：{}",
            redact::redact(&body, credential, false)
                .chars()
                .take(160)
                .collect::<String>()
        ));
    }
    let parsed: Value = serde_json::from_str(&body)
        .map_err(|error| format!("响应不是合法 JSON：{error}"))?;
    let policy = parsed
        .get("login_policy")
        .cloned()
        .filter(Value::is_object)
        .ok_or_else(|| "响应里没有 login_policy".to_string())?;
    let minutes = policy
        .get("session_timeout")
        .and_then(Value::as_i64)
        .ok_or_else(|| "login_policy 里没有 session_timeout".to_string())?;
    Ok((minutes, policy))
}

/// 把窗口调到 [`WANTED_SESSION_TIMEOUT_MINUTES`]，返回**上游确认后**的值。
///
/// ── 只动一个字段（2026-11-13 修）─────────────────────────────
/// 早先这里是「按官方文档的七个可写字段整份重写」，值全是硬编码默认值 —— 那会把
/// 用户自己放宽/收紧过的锁定策略悄悄改回默认。现在从当前策略里**逐个取原值**回填，
/// 只把 `session_timeout` 换掉；取不到的字段才落回默认（接口要求这几个字段存在）。
///
/// 上游回的是**部分更新**语义：GET 里有、但官方文档没列为可写的两项
/// （`allow_sms_login` / `certificate_timeout_duration`）不往前带，实测保持原样。
async fn write_window(
    credential: &Credential,
    current: &Value,
    proxy: Option<&ResolvedProxy>,
) -> Result<i64, String> {
    let url = policy_url(credential);
    let text_of = |key: &str| current.get(key).cloned().unwrap_or(Value::Null);
    let policy = json!({
        "login_policy": {
            "session_timeout": WANTED_SESSION_TIMEOUT_MINUTES,
            "lockout_duration": pick_number(&text_of("lockout_duration"), 15),
            "login_failed_times": pick_number(&text_of("login_failed_times"), 5),
            "period_with_login_failures": pick_number(&text_of("period_with_login_failures"), 15),
            "account_validity_period": pick_number(&text_of("account_validity_period"), 0),
            "custom_info_for_login": current
                .get("custom_info_for_login")
                .and_then(Value::as_str)
                .unwrap_or(""),
            "show_recent_login_info": current
                .get("show_recent_login_info")
                .and_then(Value::as_bool)
                .unwrap_or(false),
        }
    });
    let body = serde_json::to_vec(&policy).map_err(|error| format!("请求体编码失败：{error}"))?;
    let headers = vec![
        ("Accept".to_string(), "application/json".to_string()),
        (
            "Content-Type".to_string(),
            "application/json;charset=UTF-8".to_string(),
        ),
    ];
    let (status, text) = signed_call("PUT", &url, headers, &body, credential, proxy).await?;
    if status == 403 {
        return Err("HTTP 403：当前凭据没有修改登录策略的权限（需要 Security Administrator）".to_string());
    }
    if status != 200 {
        return Err(format!(
            "HTTP {status}：{}",
            redact::redact(&text, credential, false)
                .chars()
                .take(160)
                .collect::<String>()
        ));
    }
    // 以上游**回读**的值为准，不用我们自己写的那个常量：接口可能夹紧到区间内。
    let parsed: Value = serde_json::from_str(&text)
        .map_err(|error| format!("响应不是合法 JSON：{error}"))?;
    Ok(parsed
        .get("login_policy")
        .and_then(|policy| policy.get("session_timeout"))
        .and_then(Value::as_i64)
        .unwrap_or(WANTED_SESSION_TIMEOUT_MINUTES))
}

fn policy_url(credential: &Credential) -> String {
    format!(
        "{IAM_ORIGIN}{}",
        LOGIN_POLICY_PATH.replace("{domain}", credential.domain_id.trim())
    )
}

/// 取一个整数字段，缺失/类型不对就用默认值（PUT 要求这几个键都在）。
fn pick_number(value: &Value, fallback: i64) -> i64 {
    value.as_i64().unwrap_or(fallback)
}

/// 签名并发请求。返回 `(状态码, 响应文本)`；只有「发不出去」才是 `Err`。
///
/// 签名口径与 `balance::signed_request` 完全一致（同一套 `signer`），
/// 区别只在这里**不带** `plugin-name` / `X-Language` 那四件套 —— IAM 不认识它们，
/// 多带会进规范头集合、把签名串变成上游对不上的样子。
async fn signed_call(
    method: &str,
    url: &str,
    headers: Vec<(String, String)>,
    payload: &[u8],
    credential: &Credential,
    proxy: Option<&ResolvedProxy>,
) -> Result<(u16, String), String> {
    let signing = oauth::signer_credential(credential);
    let signed = signer::sign(method, url, &headers, payload, &signing, false)?;
    let verb = reqwest::Method::from_bytes(method.as_bytes())
        .map_err(|error| format!("方法名非法：{error}"))?;
    let mut request = crate::http::control_client(proxy, "direct")
        .request(verb, url)
        .timeout(Duration::from_secs(20));
    if !payload.is_empty() {
        request = request.body(payload.to_vec());
    }
    for (name, value) in signed {
        request = request.header(name.as_str(), value.as_str());
    }
    let response = request
        .send()
        .await
        .map_err(|error| crate::http::describe_chain(&error))?;
    let status = response.status().as_u16();
    Ok((status, response.text().await.unwrap_or_default()))
}

/// 把「当前生效的窗口」写回账号 secret（读最新记录、比较再写）。
///
/// 写不进去只记 `verbose`：它是一个**读数**，丢了下一轮还会再写；
/// 绝不能因为写不进就把整条链路当失败。
async fn persist_window(ctx: &PluginCtx, account_id: &str, minutes: i64) {
    let Ok(record) = crate::store::record(ctx, super::PROVIDER_ID, account_id).await else {
        return;
    };
    let expect = crate::store::secret_view(&record);
    // 已经一样就不写：这一轮是心跳驱动的，一天会跑很多次，
    // 每次都比对成功也要落一次盘，白白制造写放大。
    let stored = expect
        .get(SESSION_WINDOW_FIELD)
        .and_then(Value::as_i64)
        .unwrap_or(0);
    if stored == minutes {
        return;
    }
    let fields = vec![
        (SESSION_WINDOW_FIELD, Value::from(minutes)),
        (SESSION_WINDOW_SET_AT_FIELD, Value::from(crate::logging::now_ms())),
    ];
    let next = crate::store::with_fields(&expect, &fields);
    if let Err(error) = crate::store::save(ctx, account_id, &expect, &next).await {
        verbose(&format!("会话窗口读数未能落盘：{}", error.message));
    }
}

fn verbose(message: &str) {
    crate::logging::verbose("[CodeArts]", message);
}
