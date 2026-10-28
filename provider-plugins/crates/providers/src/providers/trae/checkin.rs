//! Trae 的每日签到（`checkin_credits/status` + `.../claim`）。
//!
//! ── 协议来源 ──────────────────────────────────────────────
//! 参考实现 `cpa-multi-plugins/plugins/trae`：协议层 `upstream/client.go`
//! 给出两个端点与设备号生成；节奏层 `scheduler/scheduler.go` 定「不自动重试」。
//! 发送序列与请求头则以 **AgentHub**（`electron/backend/proxy/adapters.cjs` 的
//! trae 适配器）为准 —— 它是同族实现里的长跑者，2026-10-28 起本模块按它逐条对齐。
//! 这一条链上每一步看着都可以"顺手简化"，而每一处简化的后果都是官方风控。
//!
//! ── 为什么现在做签到（usage.rs 旧注释里写过「不做」，那半句已收回）──
//! SOLO 已转积分制：模型调用消耗的就是签到钱包那份积分（参考实现
//! `panel.html:304`，调度器给池子打分也算它）。不做签到等于每天白丢一笔额度。
//! 那条链真正的前科不是"签到会被封"，而是**签到姿势**会被风控：
//!   · 设备号与官方客户端不一致 —— 2026-10 修订：改为**账号稳定派生**
//!     （见 `checkin_device_id`），旧实现「每轮新号」正是 9074 反复出现的根因；
//!   · 同一账号混用多套客户端身份 —— 这里只发一套极简浏览器特征头
//!     （origin / referer / x-app-type，见 `add_client_headers`）；
//!   · 9074 之后反复重试（参考实现有当日指数退避定时器）—— 这里不搬。
//! 因此态度是「按定稿姿势碰，且每天只发该发的几次请求」。
//!
//! ── 2026-10-26 对齐 AgentHub（同族实现的长跑者）──────────────
//! 同一分钟的同批账号对照：本仓 11:45 撞 1001，AgentHub 11:46 success。
//! 据此收敛三处：请求头改为极简浏览器形态（`checkin_headers`，不再叠 ug 的
//! VSCode 画像）；1001 换发重放以**落盘最新记录**为准（`renew_with_fresh_record`，
//! 旧写法拿自己第一发写进去的 secret 当 CAS 基线，必然误报「期间被改过」）；
//! 业务码 200 归一为成功（对齐 AgentHub 的 `code!==0 && code!==200`）。
//!
//! ── 一轮到底发几个请求（2026-10-28 起与 AgentHub 逐字对齐）────────
//! **先领取，不先查状态。** AgentHub 的 trae 签到（`adapters.cjs` 的
//! `checkin()`）就是直接打 claim，序列与发数如下：
//!   常态 **1 发**（claim）；领取成功后为了拿到数额与连签明细再查一次状态
//!   = **2 发**；被「今日已签到」文案挡回时 1 发就收。
//! 本模块此前是 `status → claim → 回查` = **3 发**，比 AgentHub 多发一发，
//! 前置那一发已删除。它原本的唯一作用「活动没开就早退」在 AgentHub 里没有，
//! 而 claim 自己的业务码就能说清楚 —— 何况前置 status 的 `enable` / `checked_in`
//! 在 1001 形态下全是缺字段兜出来的 false，据此早退会把活账号判成「没有活动」。
//! 请求体恒 `{}`（与 AgentHub 的 `body: "{}"` 同字面量）。
//!
//! ⚠️ 保留的**唯一**额外路径：claim 回 `1001`（服务端作废了这张 JWT）时换发
//! 新令牌重放一轮。AgentHub 把 1001 直接读成「服务未开放」，而本仓一手实测过
//! 「1001 = 令牌被吊销，换发后当天就能签成」——照抄它会把这个能力弄丢。
//! 这一条只在 1001 时触发，正常签到路径与 AgentHub 完全同形同数。
//! **不搬**参考实现的当日指数退避定时器（1m→2m→…→2h、每日 10 次）：那套是
//! 为「0 点整官方重置瞬间的洪峰」调的，而本仓的定时签到时刻由用户在设置页决定，
//! 再叠一层后台重试会把「一天三次」变成「一天几十次」——恰好是前科的成因。
//! 撞 9074 时如实回报、当日不再自动重试；想再试就点面板按钮，一次点击一轮。
//!
//! ── 判「已签到」只认 claim 自己的回答 ──────────────────────
//! 不再先查状态去猜。领取被业务码挡回、且文案命中「已签到 / 已经签到 / already」
//! 时判 `AlreadyCheckedIn`（与 AgentHub 的 `/已签到|已经签到|already/i` 同一条
//! 正则），并落 `alreadyCompleted:true` 让当日台账正常记账。领取 `code:0` 就是
//! 领取成功的权威答复（2026-10-27 实测：code 0 的同时积分 +100 落账；AgentHub
//! 也完全按这个码判成功）。随后的状态查询**只为拿数额与明细**，它失败不影响
//! 本次成功判定 —— 那是 AgentHub 的 `status: st.ok ? st : null` 同一口径。
//!
//! ── claim 形状与其他几家对齐 ───────────────────────────────
//! 返回 `{success, msg, ...}`，`billing::checkin` 的汇总与 `mark_checkin` 只认
//! 这两个字段，本家不另立口径：今日已签 → `success:false` + `alreadyCompleted:true`
//! + msg 带「今日已签到」；真领到 → `success:true` + msg 带奖励数额。
//!
//! ⚠️ 数额是上游**报出的当日奖励**，不是到账余额 —— 实测一个 SOLO 账号官方报
//!   200（100+100）、积分池只长 100。两个分量原样透出（`checkinCredits` /
//!   `checkinExtraCredits`），文案不替上游的名义数做到账背书。

use std::collections::BTreeMap;
use std::time::Duration;

use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use agent2api_provider_sdk::PluginCtx;
use crate::errors::GatewayError;

use super::adapter::{account_proxy, read_record, renew_forced, renew_if_due};
use super::credentials::Credential;
use super::errors::classify;
use super::http::post_json;
use super::usage::UG_HOST;

/// 签到状态查询（参考实现 `EpCheckinStatus`）。
const EP_CHECKIN_STATUS: &str = "/trae/api/v2/ug/checkin_credits/status";
/// 领取（参考实现 `EpCheckinClaim`）。
const EP_CHECKIN_CLAIM: &str = "/trae/api/v2/ug/checkin_credits/claim";

/// 业务码：成功。
const CODE_OK: i64 = 0;
/// 业务码：`当前参与用户太多，请稍后再试`。
///
/// ── 2026-10-28 实测：这一码是**设备维度**的拒绝，不是名额 ──────
/// 同账号间隔 1.5 秒的两次 claim：带凭据里的 web `deviceId` 回 9074，
/// 换 `sha256` 稳定派生号立刻 `code:0` 并真到账（两个账号各自复现）。
/// 同一时刻 status 是 `enable:true` / `checked_in:false`，活动明明是开的。
/// 所以「参与用户太多」是上游的通用风控话术，**不能读成"名额满了、等等再来"**；
/// 真正的处置是换设备号（见 `run()` 里设备号选择那段的完整实测）。
///
/// 10-27 之前的旧注释写过「设备是固定的，撞上它换号也无从换起」—— 那是基于
/// 10-26 的错误假设，已被两组对照推翻。
const CODE_REJECTED: i64 = 9074;

/// 业务码：**服务端已作废这张 JWT**，而本地 `expiredAt` 看不出来。
///
/// 出处是同一族端点（`ug/checkin_credits/*` + `pay/ide_user_ent_usage`）上的
/// 第三方判定：`code == 1001` 与文案 `not able to authenticate` 一并当作鉴权
/// 失效，动作是换发新 JWT 后重放一次。参考实现的码表里没有这一码；
/// 本仓一手验证过语义：连拒 1001 的账号刷新令牌后当天就签到成功 ——
/// 不是"名额"、不是"没资格"，就是令牌被服务端作废而 `expiredAt` 没反映。
///
/// ── 2026-10-27 补充：1001 有两种来源，处置不同 ────────────────
/// 只有 **claim 的 1001** 才是「令牌被吊销」，才走换发重放；**status 的
/// 1001 不能当判据** —— 已废弃的 GET 形态对任何令牌（含刚领取成功的）都回
/// 1001，那是网关层的形态拒绝。
///
/// ── 2026-10-28：前置 status 已删，这条区分自然落定 ──────────────
/// 现在每轮第一发就是 claim，所以「1001 来自哪里」不再需要判断：能触发重放的
/// 1001 只可能是 claim 回的。明细查询（`status`）的 1001 只让那一格为空，
/// 不参与任何判定（见 `run()` 里 `detail` 为 `None` 的分支）。
const CODE_TOKEN_DEAD: i64 = 1001;

/// 单次请求超时。与 `usage.rs` 同一个数：这条族没有流式，`egress` 默认的
/// 600 秒读超时（留给 SSE 的）会把一次批量签到拖成整页转圈。
const REQUEST_TIMEOUT: Duration = Duration::from_secs(20);

/// 错误体进消息的截断长度（与 `usage.rs` 同一口径）。
const ERROR_BODY_HEAD: usize = 200;

/// 鉴权方案。与 AgentHub 一致：**只用 `Cloud-IDE-JWT`**。
///
/// 这里曾经有一套「Jwt 失败换 Bearer 再打一发」的方案轮换（照参考实现搬的），
/// 2026-10-28 按 AgentHub 收敛掉：它的 trae 签到只有一发 POST + Cloud-IDE-JWT，
/// 没有 Bearer 分支，而且成功率高。多发那一发不只是浪费配额 —— 同一账号在
/// 9074 之后立刻再打一次，正好踩在「9074 后重试加重限流」那条实测上。
fn authorization(access_token: &str) -> String {
    format!("Cloud-IDE-JWT {access_token}")
}

/// 一轮签到的**设备号**：由账号稳定标识派生的 15 位数字串。
///
/// ── 为什么是稳定的（2026-10-26 修订，9074 的根因）─────────────
/// 旧实现每轮新生成（依据是参考实现 `cpa-multi-plugins` 的结论「复用是可疑
/// 项」）。但同族实现的实测反向：AgentHub 用 `sha256(account.id)` 派生、
/// **同账号多次请求保持一致**，签到长期正常；反观旧姿势 —— 每次点击都是
/// 「一台从没见过的设备」来领奖，设备画像上更可疑，与 9074 反复出现相符。
/// 官方客户端的 device id 本来就是装机后不变的，稳定派生才像真客户端。
///
/// ── 为什么用 sha256 而不是随机数 ────────────────────────────
/// 稳定值必须能**复算**：账号记录重建（重新登录 / 导入凭证）后要还原出同一
/// 个号，否则官方会把同一台设备看成两台。派生串带固定前缀，避免撞上真实
/// 设备号的分布。
///
/// ── 分布形状（与 AgentHub 的 `deviceIds()` 同构）──────────────
/// 首位 1-9（消除前导零假特征）、其余 14 位 0-9；连续出现 3 个 0 时扰动，
/// 根除「一串 0」这种静态特征。
///
/// `seed` 取账号的稳定标识（`uid` 优先、`machineId` 次之、令牌指纹兜底）。
pub fn checkin_device_id(seed: &str) -> String {
    let material = if seed.trim().is_empty() {
        // 没有稳定标识（极旧记录）：退回时间戳 —— 至少本轮每一发一致
        crate::logging::now_ms().to_string()
    } else {
        format!("agent2api:device:{}", seed.trim())
    };
    let digest = Sha256::digest(material.as_bytes());
    let mut digits = String::with_capacity(15);
    digits.push(char::from(b'0' + (digest[0] % 9) + 1));
    let mut zero_run = 0u8;
    for byte in digest.iter().skip(1) {
        let mut digit = byte % 10;
        if digit == 0 {
            zero_run += 1;
            if zero_run >= 3 {
                digit = (byte % 9) + 1;
                zero_run = 0;
            }
        } else {
            zero_run = 0;
        }
        digits.push(char::from(b'0' + digit));
        if digits.len() == 15 {
            break;
        }
    }
    digits
}

/// 账号稳定标识：`uid` 优先，`machineId` 次之，令牌指纹兜底。
///
/// 前两者跨轮、跨令牌刷新都不变；指纹兜底只保证「同一枚令牌期间稳定」，
/// 是没有 uid 的极旧记录的最后一条路。
fn device_seed(credential: &Credential) -> String {
    if !credential.uid.trim().is_empty() {
        return credential.uid.clone();
    }
    if !credential.machine_id.trim().is_empty() {
        return credential.machine_id.clone();
    }
    crate::flight::fingerprint(&credential.access_token)
}

/// `checkin_credits/status` 响应里本模块要用的字段。
///
/// `credits` / `extra_credits` 是**本次奖励数额**（不是钱包余额、签到后不增长），
/// 界面与日志都按"今日奖励"来写，别把它当余额显示。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Status {
    pub checked_in: bool,
    pub did_checked_in: bool,
    pub enable: bool,
    pub credits: i64,
    pub extra_credits: i64,
    pub code: i64,
    pub message: String,
}

impl Status {
    /// 「今天这个账号已经算签过了」——两个维度任一为真即真。
    ///
    /// `did_checked_in` 是**设备维度**（这个号今天签过），`checked_in` 是账号维度。
    /// 2026-10-28 起这条不再用作「跳过领取」的前置判据（前置 status 已删，见
    /// 模块头），只用来把明细挂到成功行上，让日志能区分「账号签过」与「这台
    /// 设备签过」——排查 9074 时这两个维度正是关键分水岭。
    pub fn done_today(&self) -> bool {
        self.checked_in || self.did_checked_in
    }

    /// 今日奖励的两个分量（合计见 [`Award::total`]）。
    pub fn award(&self) -> Award {
        Award { credits: self.credits, extra_credits: self.extra_credits }
    }
}

/// 响应体 → `Status`。缺字段一律按 `false` / `0` 收（**不猜**：`enable` 缺省
/// 按"没开"处理最保守 —— 但 2026-10-28 起它已不再是「跳过领取」的判据，
/// 前置 status 已删，只在明细里如实透出）。
pub fn status_of(payload: &Value) -> Status {
    let number = |key: &str| payload.get(key).and_then(Value::as_i64).unwrap_or(0);
    Status {
        checked_in: flag(payload, "checked_in"),
        did_checked_in: flag(payload, "did_checked_in"),
        enable: flag(payload, "enable"),
        credits: number("credits"),
        extra_credits: number("extra_credits"),
        code: {
            let raw = payload.get("code").and_then(Value::as_i64).unwrap_or(CODE_OK);
            // AgentHub 的码表把 200 也按成功收（`code!==0 && code!==200` 才算
            // 失败）。在这里归一到 CODE_OK，调用方就不用两处各认一遍。
            if raw == 200 { CODE_OK } else { raw }
        },
        message: payload
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim()
            .to_string(),
    }
}

fn flag(payload: &Value, key: &str) -> bool {
    payload.get(key).and_then(Value::as_bool).unwrap_or(false)
}

/// 状态 → 领取 的**结论**（与网络层解耦）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Outcome {
    /// 今天已经签过了（`already` 带的是当日奖励数额，可能为 0）
    AlreadyCheckedIn(Award),
    /// 本次真领到了（claim 回了 code 0），参数为奖励数额
    Claimed(Award),
    /// 领取被拒（`code` 是上游业务码，`message` 是上游原话）
    Rejected(i64, String),
}

/// 今日奖励的**两部分**（基础 + 加成）。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Award {
    pub credits: i64,
    pub extra_credits: i64,
}

impl Award {
    /// 参考实现给界面看的合计数（两者相加）。
    pub fn total(&self) -> i64 {
        self.credits + self.extra_credits
    }
}

/// 「今日已签到」的文案判据 —— 与 AgentHub **同一条正则的等价实现**。
///
/// AgentHub: `const already = /已签到|已经签到|already/i.test(msg);`
/// 上游在重复领取时不给专属业务码，只给这句话；所以文案就是唯一判据。
fn claims_already_checked_in(message: &str) -> bool {
    message.contains("已签到") || message.contains("已经签到") || message.to_lowercase().contains("already")
}

/// 领取那一发的答复 → 结论（顺序与 AgentHub 的 `checkin()` 逐条对应）。
///
/// AgentHub 的判定顺序，照抄不改：
/// ```text
/// code === 1001                     → unavailable（本仓走换发重放，见 claim_daily_checkin）
/// code !== 0 && code !== 200        → already = /已签到|already/i.test(msg)，ok = already
/// !r.ok || !r.data                  → ok:false（HTTP 层失败）
/// 其余（code 0 / 200）              → ok:true, already:false，再查一次 status 带明细
/// ```
/// 本仓在 `code 0` 之后同样查一次 status，但**只当明细来源**：它失败不影响
/// 成功判定（等价于 AgentHub 的 `status: st.ok ? st : null`）。
pub fn decide(claim: &Status, detail: Option<&Status>) -> Outcome {
    // 文案判「今日已签」。AgentHub 把这一档当 ok（幂等成功），本仓对应
    // `alreadyCompleted:true` + `success:false` —— 与自家其他提供商同一口径，
    // 且界面文案由 `complete()` 明确写出「今日已签到」。
    //
    // 数额：这一支**没有**明细（按 AgentHub 的序列，非 0 码不再发第二发），
    // 所以带 0 出去，界面就说「今日已签到」而不编数额。仅当调用方确实传了
    // 明细（例如将来改成已签也查一次）才用它报数。
    if claim.code != CODE_OK {
        if claims_already_checked_in(&claim.message) {
            return Outcome::AlreadyCheckedIn(detail.map(Status::award).unwrap_or_default());
        }
        return Outcome::Rejected(claim.code, claim.message.clone());
    }
    // code 0 就是领取成功。数额优先取明细（它带 credits / extra_credits 两个
    // 分量），明细拿不到就退回 claim 自己报的数，绝不编一个。
    Outcome::Claimed(detail.map(Status::award).unwrap_or_else(|| claim.award()))
}

/// 这一行结果是不是「服务端把这张 JWT 作废了」。
///
/// 判据两条并列（码、文案），因为 1001 有时**不带文案**：只认码会在文案到位时
/// 仍走一遍，只认文案会在文案缺失时整条失效。文案两种写法都要认 —— 上游与
/// 参考实现各自转述过 `not able to authenticate` 和 `unable to authenticate`，
/// 差一个词就漏判。
pub fn needs_reauth(row: &Value) -> bool {
    if row.get("code").and_then(Value::as_i64) == Some(CODE_TOKEN_DEAD) {
        return true;
    }
    let message = row.get("msg").and_then(Value::as_str).unwrap_or_default();
    message.contains("not able to authenticate") || message.contains("unable to authenticate")
}

/// Trae 账号的每日签到。
///
/// 与 `query_usage` 同一条凭证链：读记录 → 组凭据 → 解代理 → **临期先续期**
/// （参考实现的"签到前保鲜"：token 过期时签到必撞会话类错误，而那一发错误
/// 看起来像"账号不行了"，会把健康账号拖进冷却）。
///
/// 之上多一条 **1001 的换发重放**：`renew_if_due` 是按本地 `expiredAt` 判临期的，
/// 而 `CODE_TOKEN_DEAD` 的意义恰恰是"本地那张表看不出来"。所以这一码单独走
/// `renew_forced`（无视临期直接换发），再打一轮，总共两轮封顶 —— 一次点击不该
/// 变成五六发。换发也失败就叫用户重新登录，两种结果都比一句报错有用。
///
/// 📌 这是与 AgentHub **唯一**的序列差异：它把 1001 直接读成「服务未开放」。
/// 本仓保留重放，因为一手实测过「1001 = 令牌被吊销，换发后当天就能签成」。
/// 只在 1001 触发；正常路径（含 9074、已签到、code 0）与 AgentHub 完全同形同数。
pub async fn claim_daily_checkin(
    ctx: &PluginCtx,
    account_id: &str,
) -> Result<Value, GatewayError> {
    let record = read_record(ctx, account_id).await?;
    let credential = Credential::from_payload(&record).map_err(GatewayError::new)?;
    let proxy = account_proxy(&record)?;
    let credential = renew_if_due(ctx, &record, &credential, proxy.as_ref()).await?;
    let first = run(&credential, proxy.as_ref(), UG_HOST).await?;
    if !needs_reauth(&first) {
        return Ok(first);
    }
    // 没有 refreshToken 就没得换：如实回报，别拿一个必然 401 的换发放大失败感
    // （上游 beta.4 同款早退，2026-10-27 合入）。
    if !credential.can_refresh() {
        return Ok(first);
    }
    let renewed = match renew_with_fresh_record(ctx, account_id).await {
        Ok(renewed) => renewed,
        Err(error) => {
            // 连 refreshToken 都换不出新令牌：这才是真的"要重新登录"。
            // 原行留着（码、文案都在），只在后面接一句我们做了什么的实话说。
            // ⚠️ 追加的句子不许出现「已签到」「已领取」，理由见 `complete`。
            let mut row = first;
            if let Some(map) = row.as_object_mut() {
                map.insert("reauthAttempted".to_string(), json!(true));
                map.insert("reauthFailed".to_string(), json!(true));
                let head = map.get("msg").and_then(Value::as_str).unwrap_or_default().to_string();
                map.insert(
                    "msg".to_string(),
                    json!(format!("{head}；换发新令牌也失败（{}），请在「账号」页重新登录", error.message)),
                );
            }
            return Ok(row);
        }
    };
    let mut row = run(&renewed, proxy.as_ref(), UG_HOST).await.map_err(|error| {
        // 401 打上专属码：换发重放之后仍 401 = 「重试已耗尽」，与本轮开头的
        // 普通 401 不是同一件事，编排层据此决定要不要再进冷却（上游 beta.4 同款）。
        if error.status_code == 401 {
            error.with_code("checkin_auth_retry_exhausted")
        } else {
            error
        }
    })?;
    let replay_still_rejected = needs_reauth(&row);
    if let Some(map) = row.as_object_mut() {
        map.insert("reauthAttempted".to_string(), json!(true));
        // 重放**仍然**撞 1001：把语气从"计划"改成"结论"——已做了什么、
        // 还剩哪条路，一句话说完。追加句同样不许出现「已签到」。
        if replay_still_rejected {
            let head = map.get("msg").and_then(Value::as_str).unwrap_or_default().to_string();
            map.insert(
                "msg".to_string(),
                json!(format!(
                    "{head}；已换发新令牌并重放一轮仍被拒，请在「账号」页重新登录"
                )),
            );
        }
    }
    Ok(row)
}

/// 1001 之后的换发：**以落盘的最新记录为准**，不复用本轮开头的旧物。
///
/// ── 为什么必须重读（2026-10-26 实测踩坑）──────────────────────
/// 旧写法把本轮开头的 `record` / `credential` 原样传给 `renew_forced`，两个
/// 后果都是自伤：① 本轮 `renew_if_due` 若已换发落盘，store 里的 secret 就比
/// 手里的 record 新一代 —— 拿旧 record 当 CAS 的 `expect` 比较必然不等，报
/// 「续期结果与账号记录不匹配（期间被改过）」，其实没别人改，是自己上一发写
/// 的；② 旧 credential 的 refreshToken 也可能已被第一发轮换，拿它换证等于拿
/// 作废串去打。重读之后 expect 与 refreshToken 都取最新落盘值，CAS 只在真有
/// 并发改动时才失败，单飞（refresh_shared）也按新指纹去重。
async fn renew_with_fresh_record(
    ctx: &PluginCtx,
    account_id: &str,
) -> Result<Credential, GatewayError> {
    let record = read_record(ctx, account_id).await?;
    let credential = Credential::from_payload(&record).map_err(GatewayError::new)?;
    let proxy = account_proxy(&record)?;
    renew_forced(ctx, &record, &credential, proxy.as_ref()).await
}

/// 一轮签到（凭证 → 结果行）。`base` 让测试夹具能把这几发指向假上游；
/// 生产上它是 `UG_HOST` 常量，而**这条链最容易写错的就是 host**（打错 host 的
/// 失败形态是 401「unable to authenticate」而不是 404，看不出是 host 问题）。
async fn run(
    credential: &Credential,
    proxy: Option<&agent2api_provider_protocol::WireProxy>,
    base: &str,
) -> Result<Value, GatewayError> {
    let variant = credential.variant().to_string();
    if variant == "intl" || variant == "solo-intl" {
        // 参考实现里国际版根本没有签到（上游无此活动）。走到这里只可能是
        // 手工粘进来的谱系，如实说明而不是打一发注定失败的请求。
        return Ok(json!({
            "success": false,
            "msg": "国际版 Trae 没有签到通道（上游无此活动）",
        }));
    }
    if credential.access_token.trim().is_empty() {
        return Err(GatewayError::with_status(401, "该 Trae 账号没有可用凭证，无法签到"));
    }

    // 设备号**一律用账号稳定派生**（`checkin_device_id`），不再优先取凭据里的
    // `deviceId` —— 2026-10-28 实测推翻了 10-26 的假设。
    //
    // ── 实测（同账号、间隔 1.5s 的 claim 对照，两个账号各自复现）──────────
    // · 凭据 `deviceId`（web 授权登录注册的那把，如 `7115872261017347`）
    //   → `code:9074`「当前参与用户太多，请稍后再试」；
    // · 同一账号换 `sha256("agent2api:device:"+uid)` 派生的数字号
    //   → `code:0`，且随后回查 `checked_in:true`、`did_checked_in:true` 真到账；
    // · 不带 `X-Device-Id` → `code:9004`（参数错误，这一格必填）。
    // 并且 `did_checked_in` 只在用「真正签成功的那个号」回查时才为 true，
    // 说明签到记录**记在设备维度上**：凭据那把号对上游来说是"今天没签过"，
    // 却照样被 9074 挡掉 —— 它被拒不是"已签过"。
    //
    // 推断（未完全逆出，缺干净样本）：凭据里的 web 设备号是**已注册设备**，
    // 会话上还挂着它的 `devicePublicKey/devicePrivateKey`，上游对已注册设备的
    // claim 多半还要一份设备签名；我们只带号不带签名，于是落进风控话术 9074。
    // 派生号是"未注册的新号"，走的是新设备领奖那条路，反而不校验签名。
    // 要坐实需等一个当日未签的账号做「凭据号 + 设备签名」对照 —— 记在这里。
    //
    // 顺带纠正 10-26 那条反向假设：当时认为「带别的号 = 另一台设备来领奖，
    // 更可疑」，实测恰好相反 —— **带 web 登录那把号才是最可疑的**。
    // 稳定性不受影响：派生号同账号每天一致（uid 优先、machineId 次之），
    // 对上游就是「同一台设备每天来领一次」，正是官方客户端的形态。
    let device_id = checkin_device_id(&device_seed(credential));

    // ── 发数序列：与 AgentHub 的 `checkin()` 逐条对齐 ───────────────
    // AgentHub 的实际序列（`adapters.cjs` 883–911 行）：
    //   ① 直接打 claim（前面**不查状态**）；
    //   ② `code!==0 && code!==200` → 按文案判 already，**就此返回（共 1 发）**；
    //   ③ `code===0/200` → 再查一次 status 拿明细，`status: st.ok ? st : null`
    //      —— 也就是**只有真领到才发第二发（共 2 发）**。
    // 本模块照此，常态 1 发、成功时 2 发；不再有前置 status，也不再有第三发回查。
    let claim_outcome = claim(base, credential, &device_id, proxy).await?;
    if claim_outcome.code != CODE_OK {
        // 非 0 业务码：可能是「今日已签到」（文案判定，见 `decide`），也可能真被拒。
        // 两种都不再发第二发 —— 与 AgentHub 一致。
        return Ok(complete(decide(&claim_outcome, None), &device_id));
    }
    // 真领到了：查一次明细（数额 / 连签）供界面显示。这一发失败不影响成功判定，
    // 等价于 AgentHub 的 `status: st.ok ? st : null`。
    let detail = status(base, credential, &device_id, proxy).await.ok();
    let row = complete(decide(&claim_outcome, detail.as_ref()), &device_id);
    Ok(match detail.as_ref() {
        Some(detail) => with_detail(row, detail),
        None => row,
    })
}

/// 把明细查询读到的结果挂到成功行上（`status` 那几格）。
///
/// 成功行带这些不是为了文案 —— 是为了**排障**：`statusDidCheckedIn` 是设备维度、
/// `statusCheckedIn` 是账号维度，两者在 9074 排查里是分水岭；`statusEnable`
/// 说明活动当时到底开着没开。AgentHub 也把整份 status 挂在结果里（`status: st`）。
fn with_detail(row: Value, detail: &Status) -> Value {
    let mut row = row;
    if let Some(map) = row.as_object_mut() {
        map.insert("statusEnable".into(), json!(detail.enable));
        map.insert("statusCheckedIn".into(), json!(detail.checked_in));
        map.insert("statusDidCheckedIn".into(), json!(detail.did_checked_in));
        map.insert("statusCredits".into(), json!(detail.credits));
        map.insert("statusExtraCredits".into(), json!(detail.extra_credits));
        map.insert("statusDoneToday".into(), json!(detail.done_today()));
    }
    row
}

/// 判定 → 界面与日志消费的 `{success, msg, ...}`。
///
/// 设备号一并带回（`checkinDeviceId`）：这一轮用的哪个号，是把「上游回了 9074」
/// 与「官方客户端同日替用户签了」在后端日志里分开的唯一线索。
///
/// ⚠️ 两个雷区：
///   1. 数字口径：`credits`/`extra_credits` 是上游**报出的当日奖励数额**，不是
///      到账余额 —— 文案说"官方报出的当日奖励"，把"实际到账"留给界面上已有的
///      余额列，不用一句会撒谎的成功文案替它背书。
///   2. Rejected 的文案不许出现「已签到」「已领取」：批量层
///      `billing::checkin::checkin_completed_today` 是按 msg 子串判"当日已办"的，
///      写进去就把一次失败洗成已签、当日不再重试。（上游自己给的文案落在
///      `{reason}` 里，那一格若真说"已领"，被批量层当成当日已办是对上游原话
///      的忠实执行，不是我们的加工。）
fn complete(outcome: Outcome, device_id: &str) -> Value {
    let mut row = serde_json::Map::new();
    let awarded = match outcome {
        Outcome::AlreadyCheckedIn(award) => {
            row.insert("success".into(), json!(false));
            // `alreadyCompleted` 是给 `billing::checkin::checkin_completed_today`
            // 看的：今天已经签过 = 当日台账要落库，定时链明天才照常再试，
            // 而不是把"已签"当成失败反复重打。
            row.insert("alreadyCompleted".into(), json!(true));
            row.insert(
                "msg".into(),
                json!(if award.total() > 0 {
                    format!("今日已签到（官方报出当日奖励 {} 积分）", award.total())
                } else {
                    "今日已签到".to_string()
                }),
            );
            Some(award)
        }
        Outcome::Claimed(award) => {
            row.insert("success".into(), json!(true));
            row.insert(
                "msg".into(),
                json!(if award.total() > 0 {
                    format!(
                        "签到成功（官方报出当日奖励 {} 积分；到账看余额列）",
                        award.total()
                    )
                } else {
                    // claim 回了 code 0，但明细查询没给出数额 —— 不编一个数字出来
                    "签到成功（上游未给出奖励数额）".to_string()
                }),
            );
            Some(award)
        }
        Outcome::Rejected(code, message) => {
            row.insert("success".into(), json!(false));
            row.insert("code".into(), json!(code));
            // 上游给了文案就原样带出，没给才说"未给"。
            let reason = if message.is_empty() {
                "上游未给文案"
            } else {
                message.as_str()
            };
            let text = if code == CODE_REJECTED {
                // 文案别再写「名额满了、稍后再试」——10-28 实测它是设备维度的
                // 风控拒绝，同一秒换设备号就能签成。写错会把人引向"等一等"。
                format!(
                    "官方拒绝本次签到（{code}）：{reason}；这一码实测是**设备维度**的\
                     风控拒绝（不是名额）。本版设备号与发送序列已与 AgentHub 对齐\
                     （账号固定派生号、单发 claim）；若仍出现，说明这个号今天被临时\
                     拦了，稍后再试（当日不再自动重试）"
                )
            } else if code == CODE_TOKEN_DEAD {
                format!(
                    "官方拒绝本次签到（{code}）：{reason}；这一码判为「服务端已作废这张登录态」\
                     （本地到期时间看不出来），处置是换发新令牌后重放一轮"
                )
            } else {
                // 不猜含义 —— 跨家搬码表正是把别家结论当自家事实的那类错。
                format!(
                    "官方拒绝本次签到（{code}）：{reason}；这一码本家没有实测依据，只按失败上报，当日台账不落"
                )
            };
            row.insert("msg".into(), json!(text));
            None
        }
    };
    if let Some(award) = awarded {
        row.insert("awarded".into(), json!(award.total()));
        row.insert("checkinCredits".into(), json!(award.credits));
        row.insert("checkinExtraCredits".into(), json!(award.extra_credits));
    }
    row.insert("checkinDeviceId".into(), json!(device_id));
    Value::Object(row)
}

/// 官方客户端的浏览器特征头（origin / referer / x-app-type，与 AgentHub
/// 同值同形）。单独一个函数是因为它同时被 status 与 claim 两条路用。
fn add_client_headers(headers: &mut BTreeMap<String, String>) {
    headers.insert("Origin".to_string(), "https://www.trae.cn".to_string());
    headers.insert("Referer".to_string(), "https://www.trae.cn/".to_string());
    headers.insert("X-App-Type".to_string(), "trae".to_string());
}

/// 签到请求头 —— 与 AgentHub 的 `checkin()` 那组头**逐项对齐**。
///
/// AgentHub 那一发的头正好是这几项（`adapters.cjs` 883–899 行）：
/// `content-type` / `accept` 均 json、`authorization: Cloud-IDE-JWT …`、
/// `x-user-region: CN`、`x-device-id`、`origin` / `referer: https://www.trae.cn`、
/// `x-app-type: trae`。没有 UA、没有 ug 的 VSCode 画像、没有 trace 串 ——
/// 头越少越不像拼出来的脚本，这一条是它长期稳的直接原因。
fn checkin_headers(access_token: &str, device_id: &str) -> BTreeMap<String, String> {
    let mut headers = BTreeMap::new();
    headers.insert("Content-Type".to_string(), "application/json".to_string());
    headers.insert("Accept".to_string(), "application/json".to_string());
    headers.insert("Authorization".to_string(), authorization(access_token));
    headers.insert("X-User-Region".to_string(), "CN".to_string());
    if !device_id.is_empty() {
        headers.insert("X-Device-Id".to_string(), device_id.to_string());
    }
    add_client_headers(&mut headers);
    headers
}

/// 状态查询 —— **只在 claim 回了 code 0 之后发一次，用来取明细**。
///
/// ── 形态：POST + Cloud-IDE-JWT（2026-10-27 实测修订）──────────
/// 10-26 曾按「AgentHub 同形」把首发改成 GET `?did=` + Bearer；10-27 实测
/// 这条 GET 形态**已对所有人失效**：拿 AgentHub 自己那枚刚签到成功的令牌
/// 打 GET 也回 1001「not able to authenticate you」，而不带鉴权头 / 假令牌
/// 回的是一字不差的同一句 —— 说明这一形态在网关层就被整体拒了，与令牌无关。
/// 同一时刻同一枚令牌打 POST（Cloud-IDE-JWT、空体）能正常领取并到账 100 积分。
/// 故走 POST（与 claim 同一套鉴权），不发那发注定被拒的 GET。
///
/// ⚠️ **它不再是每轮的前置必发**：取消前置查询是 2026-10-28 对齐 AgentHub 的
/// 结果（它直接 claim、不先查状态）。这里只做明细来源，失败不影响成功判定。
async fn status(
    base: &str,
    credential: &Credential,
    device_id: &str,
    proxy: Option<&agent2api_provider_protocol::WireProxy>,
) -> Result<Status, GatewayError> {
    let payload = round_trip(base, EP_CHECKIN_STATUS, device_id, credential, proxy).await?;
    Ok(status_of(&payload))
}

/// 领取 —— **每轮的主发**，形状与顺序都照 AgentHub 的 `checkin()`。
///
/// 返回它的 code 与 message：`code:0` 即领取成功；被「已签到」文案挡回由
/// 调用方按文案判定；上游在 claim 里不保证给奖励数额，数额归随后那次明细查询。
async fn claim(
    base: &str,
    credential: &Credential,
    device_id: &str,
    proxy: Option<&agent2api_provider_protocol::WireProxy>,
) -> Result<Status, GatewayError> {
    let payload = round_trip(base, EP_CHECKIN_CLAIM, device_id, credential, proxy).await?;
    Ok(status_of(&payload))
}

/// 一发请求 + 响应体返回。**不再有方案轮换、不再有探测体序列**。
///
/// 此前这里有「Jwt→Bearer 换方案」「9074→换探测体」两套重试（照参考实现搬的）。
/// 2026-10-28 按 AgentHub 收敛成单发：它的 claim 就是一次 POST，没有任何重试，
/// 而成功率高。多发那一发不是"更稳"，是**多一次被风控看见的机会**，而且
/// 9074 本身就是设备维度的拒绝，换鉴权方案根本改不了它。
/// 业务码一律原样返回，由调用方判定 —— 签到失败不是网关错误，不该冒成 5xx。
/// 只有 HTTP 层的会话失效（401 一类）才往上抛 `GatewayError`。
async fn round_trip(
    base: &str,
    endpoint: &str,
    device_id: &str,
    credential: &Credential,
    proxy: Option<&agent2api_provider_protocol::WireProxy>,
) -> Result<Value, GatewayError> {
    let url = format!("{base}{endpoint}");
    post_checkin(url.as_str(), &json!({}), device_id, &credential.access_token, proxy).await
}

/// 真正发出去的那一发：AgentHub 同形头组 + 本轮固定设备号。
///
/// HTTP 层的状态码在这里就判成错误（与 `usage.rs` 的出站同一口径）：
/// ug 族带着 `Cloud-IDE-JWT` 在生产上是通的（余额读数实测），所以一个 401 的
/// 含义是"这条会话真的死了"。业务码不在这里分派，原样交给调用方。
async fn post_checkin(
    url: &str,
    body: &Value,
    device_id: &str,
    access_token: &str,
    proxy: Option<&agent2api_provider_protocol::WireProxy>,
) -> Result<Value, GatewayError> {
    let headers = checkin_headers(access_token, device_id);
    let prepared: Vec<(String, String)> = headers.into_iter().collect();
    let pairs: Vec<(&str, String)> = prepared
        .iter()
        .map(|(name, value)| (name.as_str(), value.clone()))
        .collect();
    let reply = post_json(url, body, &pairs, REQUEST_TIMEOUT, proxy).await?;
    if reply.status >= 400 {
        let head: String = reply.body.chars().take(ERROR_BODY_HEAD).collect();
        let kind = classify(reply.status, &head);
        return Err(GatewayError::with_status(
            i32::from(super::forward::status_for(kind)),
            format!("Trae 签到接口返回 {}: {}", reply.status, head.trim()),
        ));
    }
    Ok(reply.json().unwrap_or_else(|| json!({})))
}
