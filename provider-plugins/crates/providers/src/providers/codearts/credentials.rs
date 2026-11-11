//! CodeArts 凭证：一次登录换来的**临时** AK/SK/STS + 域号，外加续期所需的
//! 长期那一半（refresh token、PKCE verifier、DPoP 私钥）。
//!
//! ── 两半的分工（这是本模块存在的理由）────────────────────
//! 上游登录（`sts.cn-north-4.myhuaweicloud.com/v1/oauth2/tokens`）返回三段
//! 短期材料 —— `access_key_id` / `secret_access_key` / `security_token`，
//! 有效期由 `credentials.expiration` 给出（实测约 24 小时）。这三段用过就
//! 必须续，而**续期不能只靠 refresh token**：同一个令牌请求还要出示
//! **当初那次登录所用的 PKCE verifier 与 DPoP 私钥**（官方扩展之所以把它们
//! 一起落盘就是因为这个）。所以凭据是一整块、不能只存 AK/SK/STS；少存一半，
//! 到期后就再也续不回来，只能重新走浏览器登录。
//!
//! ── 字段名为什么是 snake_case ──────────────────────────────
//! 与上游 JSON 对齐（`credentials.access_key_id`、`expiration`），也与参考
//! 实现 `cpa-codearts-plugin` 的 `credential` 结构逐字一致 —— 迁移过来的
//! 凭据不用做一次字段改名，少一次出错的机会。

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// 上游契约里 access 材料的最小有效期；低于它就不该再发请求（提前续期见
/// `refresh.rs` 的 15 分钟提前量）。这条只是兜底，真正的判据是 `expires_at`。
pub const MAX_TOKEN_LENGTH: usize = 8192;

/// 网页会话 cookie 的长度上限（字符）。与 `MAX_TOKEN_LENGTH` 同量级 ——
/// 一条完整的华为云控制台 Cookie 串实测 700 字符上下，8 KiB 足够容纳
/// 「Copy as cURL」那种带几十个键的整段粘贴。
pub const MAX_COOKIE_LENGTH: usize = 8192;

/// 网页会话 cookie 的落盘键。
///
/// 与 Qoder 同值（`sessionCookie`）：后端的 PATCH 入口、公开视图的
/// `hasSessionCookie`、前端输入框的 `key` 全都按这个名字找，两家共用一个键名
/// 少一层翻译。⚠️ 这是**落盘契约**：改名会让存量账号的 cookie 读不到。
pub const SESSION_COOKIE_FIELD: &str = "sessionCookie";

/// 华为云 SSO 票根（`SSOTGC`）的落盘键。
///
/// ── 它是干什么的 ────────────────────────────────────────────
/// 网页会话 cookie 那两条（`devclouddevuibjJ_SESSION_ID` + `devclouddevuibjtcftk`）
/// 都是**会话级**的，浏览器一重启就没了，且两者严格配对、不能分开复用。而
/// `SSOTGC`（Ticket-Granting Cookie，落在 `auth.huaweicloud.com` 的
/// `Path=/authui/`）是它们的**种子** —— 拿它能现场重放整条 SSO 链铸出新会话。
///
/// ⚠️ **它不是「长期票据」**（这一点 2026-11-11 用实测纠正过）：
/// `Set-Cookie` 里既没有 `Max-Age` 也没有 `Expires`，按 RFC 6265 属会话级；
/// 服务端另有账号级「会话超时策略」（IAM → 安全设置 → 登录验证策略，默认闲置
/// 1 小时、上限 24 小时）。它之所以在浏览器里显得很耐用，是因为**每次使用都会
/// 被换成一枚新的**（滑动过期）—— 所以「一直用着」才是它活着的原因。
///
/// 存它就能在 cookie 失效时静默重铸一条（见 [`super::portal_session`]），
/// 并且每次重铸都把上游回的新票根写回来（见同模块的 `persist`）——
/// 这两件事合起来才是真正的「保活」。⚠️ 这是**落盘契约**：改名会让存量账号的
/// 票根读不到（症状是「配了票根却还是一次性」）。
pub const SSO_TGC_FIELD: &str = "ssoTgc";

/// 会话 cookie 的键名（门户下发会话时用的就是这个名）。
pub const SESSION_COOKIE_KEY: &str = "devclouddevuibjJ_SESSION_ID";

/// 票根在 Cookie 头里的**键名**（与落盘键 `ssoTgc` 不是一个东西）。
///
/// ⚠️ 这两个名字必须分清：`SSOTGC` 是**上游的 cookie 名**（出现在 `Cookie:` /
/// `Set-Cookie:` 里），`ssoTgc` 是**我们落盘的字段名**。混用会让「从响应里捡
/// 新票根」这一步永远捡不到 —— 2026-11-11 那次就是这么看着代码没问题的。
pub const SSO_TGC_COOKIE: &str = "SSOTGC";

/// CSRF 令牌的键名（POST 的 `cftk` 头取的就是同一条 cookie 里这一项的值）。
pub const CFTK_COOKIE: &str = "devclouddevuibjtcftk";

/// SSO 票根的长度上限（字符）。
///
/// 实测票根 100 字符上下（`TGTX--F1840439038-8498999-<64 hex>-sso`）；给 4 KiB
/// 余量是为了容忍上游换编码/加前缀，但仍在「不可能是一条 cookie 串」的量级上
/// —— 用户把整段 Cookie 串贴进票根框时要能拦住，否则会把只该有一项的字段塞满。
pub const MAX_TGC_LENGTH: usize = 4096;

/// P-256 私钥 JWK（`d` 是标量的 base64url，32 字节定长）。
#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
pub struct Jwk {
    #[serde(default)]
    pub kty: String,
    #[serde(default)]
    pub crv: String,
    #[serde(default)]
    pub x: String,
    #[serde(default)]
    pub y: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub d: String,
}

/// PKCE 的两半：verifier 是秘密（只在后端），challenge 是它的一次性公开形式。
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct PkcePair {
    #[serde(default)]
    pub code_verifier: String,
    #[serde(default)]
    pub code_challenge: String,
    #[serde(default)]
    pub code_challenge_method: String,
}

/// DPoP 密钥对。私钥用来签每一次令牌请求的 proof，公钥嵌在 proof 的 JWS 头里
/// 让上游验证签名；两者都由私钥推出，这里都存是为了**迁移时原样搬运**。
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct DpopKeyPair {
    #[serde(default)]
    pub private_key_jwk: Jwk,
    #[serde(default)]
    pub public_key_jwk: Jwk,
}

/// 长期那一半：续期要用、短期材料可以整块替换的东西。
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct OAuthContext {
    #[serde(default)]
    pub pkce_pair: PkcePair,
    #[serde(default)]
    pub dpop_key_pair: DpopKeyPair,
}

/// 一份 CodeArts 凭据。
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct Credential {
    #[serde(default)]
    pub access_key_id: String,
    #[serde(default)]
    pub secret_access_key: String,
    #[serde(default)]
    pub security_token: String,
    /// 上游给的到期时刻（RFC3339，如 `2026-09-27T16:17:00.327Z`）
    #[serde(default)]
    pub expires_at: String,
    #[serde(default)]
    pub domain_id: String,
    #[serde(default)]
    pub user_id: String,
    #[serde(default)]
    pub user_name: String,
    #[serde(default)]
    pub login_type: String,
    #[serde(default)]
    pub refresh_token: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub oauth_context: Option<OAuthContext>,
    /// **网页会话 cookie**（可选）：只有它能让 `credits.rs` 读到逐批积分的到期
    /// 时刻，AK/SK 那条签名链打不进 portal BFF（实测 404）。
    ///
    /// 它不是登录换来的、也没有任何接口能换出来 —— 只能由用户从浏览器复制。
    /// 缺省就整条通道不查（见 `credits::fetch_renewing_with_state` 的第一道判据），账号页只剩聚合
    /// 余额，与本次改动之前的行为一致。
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub session_cookie: String,
    /// **华为云 SSO 票根**（`SSOTGC`，可选）：网页会话 cookie 的「种子」。
    ///
    /// 上面那条 cookie 是会话级的（门户的 `Set-Cookie` 没有 `Max-Age` / `Expires`），
    /// 浏览器一重启就丢。这条票根**比它长，但也不是长期的** —— 它的
    /// `Set-Cookie` 同样没有 `Max-Age` / `Expires`（按 RFC 6265 是会话级 cookie），
    /// 服务端再叠一道账号级「会话超时策略」（默认闲置 1 小时、上限 24 小时）。
    ///
    /// ⚠️ 2026-11-11 实测推翻过旧说法「票根是长期票据」：11-10 12:33 登录捡到的票根
    /// 在 11-11 13:15 就被服务端清掉了。**它靠「一直被用」活着**：每次换票成功，
    /// 上游都回一枚新的（滑动过期），所以要把它一起写回去（见
    /// [`super::portal_session::persist`]），否则最多活过一个窗口。
    ///
    /// 有它就等于把「每次重启手工复制 cookie」换成「贴一次票根，之后自动重铸」。
    /// 缺省时行为与只有 `session_cookie` 时完全一样：cookie 过期就静默降级成
    /// 聚合值，等用户重贴。
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub sso_tgc: String,
}

impl Credential {
    /// 能拿它去签上游请求吗（签名的两个必要材料）。
    pub fn valid(&self) -> bool {
        !self.access_key_id.is_empty() && !self.secret_access_key.is_empty()
    }

    /// 能拿它去续期吗 —— 续期要 refresh token **和** 当初那次登录的上下文。
    pub fn can_refresh(&self) -> bool {
        self.refresh_token.trim() != ""
            && self
                .oauth_context
                .as_ref()
                .is_some_and(|context| !context.pkce_pair.code_verifier.trim().is_empty())
    }

    /// 账号身份（用于同账号判定与单飞 key，不要拿去展示）。
    pub fn identity(&self) -> String {
        let domain = self.domain_id.trim();
        let user = self.user_id.trim();
        if !domain.is_empty() || !user.is_empty() {
            return format!("{domain}\u{0}{user}");
        }
        if !self.user_name.trim().is_empty() {
            return self.user_name.trim().to_string();
        }
        self.access_key_id.trim().to_string()
    }

#[allow(dead_code)]
    /// 是否同一个账号（三项按优先级回退，与参考实现同口径）。
    pub fn same_account(&self, other: &Credential) -> bool {
        let both = |left: &str, right: &str| !left.trim().is_empty() && !right.trim().is_empty();
        if both(&self.domain_id, &other.domain_id) && both(&self.user_id, &other.user_id) {
            return self.domain_id.trim() == other.domain_id.trim()
                && self.user_id.trim() == other.user_id.trim();
        }
        if both(&self.user_name, &other.user_name) {
            return self.user_name.trim() == other.user_name.trim();
        }
        both(&self.access_key_id, &other.access_key_id)
            && self.access_key_id.trim() == other.access_key_id.trim()
    }

    /// 短期材料是否变化（决定要不要落盘）。
    #[allow(dead_code)]
    pub fn material_changed(&self, other: &Credential) -> bool {
        self.access_key_id != other.access_key_id
            || self.secret_access_key != other.secret_access_key
            || self.security_token != other.security_token
            || self.refresh_token != other.refresh_token
            || self.expires_at != other.expires_at
    }

    /// 到期时刻（毫秒 epoch）。只有能解析出 RFC3339 才有值 ——
    /// 解析不出来**不能**当成「还新鲜」：那会让一个坏时间戳永久躲在缓存里。
    pub fn expires_at_ms(&self) -> Option<i64> {
        let text = self.expires_at.trim();
        if text.is_empty() {
            return None;
        }
        chrono::DateTime::parse_from_rfc3339(text)
            .ok()
            .map(|time| time.timestamp_millis())
    }

    /// 是否需要续期（`lead_ms` 是提前量；`lead_ms = 0` 即「已经过期了吗」）。
    ///
    /// 三条与参考实现一致的行为：没有 security token 的凭据不刷（它本来就不完整）；
    /// 没有到期时间的不刷（无从判断，交给登录补全）；**解析不出来的当作要刷**
    /// （坏时间戳绝不能被当成新鲜）。
    pub fn needs_refresh(&self, lead_ms: i64, now_ms: i64) -> bool {
        if self.security_token.trim().is_empty() || !self.valid() {
            return false;
        }
        if self.expires_at.trim().is_empty() {
            return false;
        }
        match self.expires_at_ms() {
            Some(expiry) => expiry <= now_ms + lead_ms,
            None => true,
        }
    }

    /// 迁移/导入时用：上游 JSON 里字段名可能落在嵌套或平铺两种位置。
    ///
    /// 参考实现把凭据存在 `codearts_provider_credential` 这个键下（CPA 的 auth
    /// 文件就是这么写的），但手工粘贴时人是平铺给的，所以两种都认。
    pub fn from_payload(payload: &Value) -> Result<Self, String> {
        let body = payload
            .get("codearts_provider_credential")
            .or_else(|| payload.get("credential"))
            .unwrap_or(payload);
        let mut credential: Credential = serde_json::from_value(body.clone())
            .map_err(|error| format!("CodeArts 凭据解析失败：{error}"))?;
        // 到期时间可能写在 `expiration`（上游 OAuth 响应里的原生键名）
        if credential.expires_at.trim().is_empty() {
            credential.expires_at = text_of(body, &["expiration", "expires_at", "expiresAt"]);
        }
        credential.domain_id = first_non_empty(&[
            credential.domain_id.clone(),
            text_of(body, &["domain_id", "account_id"]),
        ]);
        credential.user_id = first_non_empty(&[
            credential.user_id.clone(),
            text_of(body, &["user_id", "principal_id"]),
        ]);
        credential.user_name = first_non_empty(&[
            credential.user_name.clone(),
            text_of(body, &["user_name", "principal_urn"]),
        ]);
        credential.login_type = first_non_empty(&[
            credential.login_type.clone(),
            text_of(body, &["login_type"]),
            "WEB".to_string(),
        ]);
        // 网页会话 cookie：显式三个键名（与 Qoder 同一份名单）。**读取侧也归一**
        // —— 早期/别的入口可能已经把多行串落了库，只在写入处压平的话那些记录
        // 要重新添加一次才能恢复。
        credential.session_cookie = session_cookie_of(body);
        // SSO 票根：只认一个键名（本家专有，Qoder 没有对应物）。读取侧同样过一遍
        // 「抽值」—— 用户常常整段粘 `SSOTGC=TGTX--…; Path=/authui/`（F12 的
        // Table 视图给的就是这种形状），那不是一条能直接拼进 Cookie 头的值。
        credential.sso_tgc = sso_tgc_of(body);
        if !credential.valid() {
            return Err("CodeArts 凭据缺少 access_key_id 或 secret_access_key".to_string());
        }
        if credential.access_key_id.len() > MAX_TOKEN_LENGTH
            || credential.secret_access_key.len() > MAX_TOKEN_LENGTH
            || credential.security_token.len() > MAX_TOKEN_LENGTH
        {
            return Err("CodeArts 凭据过长".to_string());
        }
        Ok(credential)
    }

    /// 落库形状：除本家的专有字段外，**同时写通用键名**（`accessToken` /
    /// `refreshToken` / `expiresAt` / `userId`）。
    ///
    /// 这不是冗余：账号存储的投影（`StoredAccount::has_token` → `access_token()`、
    /// `user_id()`、`token_expires_at()`）读的是这些通用键，全仓几十处"这个账号
    /// 能不能用/什么时候过期"的判定都走它们；不写就会让 codearts 账号在账号列表
    /// 里被当成"没有 token"而永远选不中。Qoder 也是同一套共享键名。
    pub fn to_record_value(&self) -> Value {
        let mut record = match serde_json::to_value(self) {
            Ok(value) => value.as_object().cloned().unwrap_or_default(),
            Err(_) => serde_json::Map::new(),
        };
        record.insert("accessToken".to_string(), Value::String(self.access_key_id.clone()));
        record.insert("refreshToken".to_string(), Value::String(self.refresh_token.clone()));
        record.insert("userId".to_string(), Value::String(self.user_id.clone()));
        record.insert(
            "expiresAt".to_string(),
            self.expires_at_ms().map(Value::from).unwrap_or(Value::Null),
        );
        // 网页会话 cookie 单独补一个通用键名：`from_payload` 认三个名字，
        // 而 serde 序列化出来的键是 `session_cookie`（snake_case）。写 `sessionCookie`
        // 是为了与 Qoder 的记录、后端 PATCH 入口、前端输入框**同名** —— 三家都按
        // 这一个键找，少一层翻译就少一次「贴了却没生效」的排查。
        //
        // ⚠️ 早期漏写这一项的症状是「用户粘贴时带了 cookie、落库时被丢掉」，
        // 于是逐批明细永远读不出来、有效期列永远空着，而记录里看起来什么都没少。
        if !self.session_cookie.trim().is_empty() {
            record.insert(
                SESSION_COOKIE_FIELD.to_string(),
                Value::String(self.session_cookie.clone()),
            );
        }
        // SSO 票根同理：serde 给的是 `sso_tgc`，而落盘键、后端 PATCH 入口、前端
        // 输入框都按 `ssoTgc` 找。**空值不写**（与上面那条同一纪律：用户重新贴一份
        // 只有 AK/SK 的凭据时不该把票根洗掉）。
        if !self.sso_tgc.trim().is_empty() {
            record.insert(SSO_TGC_FIELD.to_string(), Value::String(self.sso_tgc.clone()));
        }
        Value::Object(record)
    }

    /// 落盘形状：与上游 JSON 同名，方便与 CPA 的 auth 文件对拍。
    pub fn to_value(&self) -> Value {
        serde_json::to_value(self).unwrap_or(Value::Null)
    }
}

fn text_of(value: &Value, keys: &[&str]) -> String {
    keys.iter()
        .find_map(|key| value.get(*key).and_then(Value::as_str))
        .map(str::trim)
        .unwrap_or_default()
        .to_string()
}

/// 从 payload 里取网页会话 cookie 并规整成**可单行进 HTTP 头**的形态。
///
/// 字段名只认显式的三个（与 Qoder 同一份名单）：账号记录里可能出现无关的
/// `cookie` 键（别的家用它存别的东西），认错会拿它去打 portal BFF。
///
/// 名字带 `_of` 是为了不和 `Credential::session_cookie` 那个同名字段混淆。
pub fn session_cookie_of(payload: &Value) -> String {
    normalize_cookie(&text_of(payload, &[SESSION_COOKIE_FIELD, "session_cookie", "webCookie"]))
}

/// 从 payload 里取 SSO 票根并抽成**纯值**。
///
/// ── 为什么不能像 cookie 那样只 `trim` ────────────────────────
/// 票根只有一个键、一个值，而用户会从三种地方拿：
///   · DevTools 的 **Request Headers → Cookie**：`SSOTGC=TGTX--…-sso`（可能与其他
///     键混在同一行）→ 要按 `;` 切开挑出 `SSOTGC`；
///   · DevTools 的 **Application → Cookies** 表格里那一格：纯值 `TGTX--…-sso`；
///   · 「Copy as cURL」：整段 `Cookie:` 多行 → 先压平再挑。
///
/// 三种都只在「输入里出现过 `SSOTGC=`」时才走抽取；否则整段当纯值看（第二种）。
/// 抽不出来（有 `SSOTGC=` 但值为空）返回空串 —— 让上层判成「没配票根」，
/// 而不是把一段带属性尾巴（`; Path=/authui/`）的垃圾拼进 Cookie 头。
///
/// ⚠️ **不是票根形状的结果一律返回空串**（见 [`looks_like_tgc`]）：早先这里是
/// 「没看到 `SSOTGC=` 就当纯值用」，于是把整条 `devclouddevuibjJ_SESSION_ID=…;
/// devclouddevuibjtcftk=…` 存成了票根 —— 那串永远换不到会话，而界面显示
/// 「已配置」。这类**认不出**必须显形（空串 → 上层拒绝 / 判未配置），
/// 不能静默落盘。
pub fn sso_tgc_of(payload: &Value) -> String {
    let raw = text_of(payload, &[SSO_TGC_FIELD, "sso_tgc", "SSOTGC", "tgc"]);
    normalize_tgc(&raw)
}

/// 票根文本 → 纯值（见 [`sso_tgc_of`] 的说明）。
///
/// 返回空串 = **认不出是票根**（不是「清除」：清除走 `null` / 空输入那条路）。
pub fn normalize_tgc(raw: &str) -> String {
    let raw = raw.trim();
    if raw.is_empty() {
        return String::new();
    }
    // 多行粘贴（Copy as cURL）先压平；压平只按 `;` 与空格拼，不会破坏值本身。
    let flat = normalize_cookie(raw);
    // ① 输入里明确出现过 `SSOTGC=` → 取它后面那一段（到 `;` 或空白为止）。
    //    这条覆盖「整段请求头 / 整段 cookie 串」，是最常见的粘贴形态。
    let candidate = if let Some(index) = flat.find("SSOTGC=") {
        let tail = &flat[index + "SSOTGC=".len()..];
        tail.split(|character: char| character == ';' || character.is_whitespace())
            .next()
            .unwrap_or("")
            .trim()
    } else {
        // ② 没有键名 → 当纯值看，只切掉可能粘进来的属性尾巴（`TGTX--…-sso; Path=/authui/`）。
        flat.split(|character: char| character == ';' || character.is_whitespace())
            .next()
            .unwrap_or("")
            .trim()
    };
    // ③ **形状判据**：认不出就返回空串，绝不把别家的 cookie 存成票根。
    if !looks_like_tgc(candidate) {
        return String::new();
    }
    candidate.chars().take(MAX_TGC_LENGTH).collect()
}

/// 这一段文本**像不像**一条 `SSOTGC` 票根。
///
/// ── 为什么必须有这条判据（2026-11-09 踩过）──────────────────
/// 用户从 `codearts.huaweicloud.com` 复制**整条 cookie 串**贴进票根框（那正是
/// 早先提示语鼓励的形态！），而那条串里**根本没有 `SSOTGC`** —— 票根在
/// `auth.huaweicloud.com` 域、`Path=/authui/`，是两个不同的域。旧逻辑会把
/// 第一个 `k=v` 对（`devclouddevuibjJ_SESSION_ID=…`）当成票根落盘：
/// 界面显示「已配置」，插件拿它换票永远失败，而日志里只有一句「换不到会话」。
///
/// 判据取**上游的实际形状**：以 `TGTX--` 开头、以 `-sso` 结尾（实测
/// `TGTX--F1840439038-8498999-<64 hex>-sso`，约 100 字符）。放宽成只认
/// `TGTX` 前缀也够 —— 尾部再加一条长度下限，避免把 `TGTX` 本身当票根。
pub fn looks_like_tgc(value: &str) -> bool {
    let value = value.trim();
    value.len() >= 16 && value.starts_with("TGTX") && value.ends_with("-sso")
}

/// 把一段原始 cookie 文本压成**单行**（`a=1; b=2`）。
///
/// ── 为什么要压平换行 ────────────────────────────────────────
/// 用户最常见的来源是浏览器 DevTools 的「Copy as cURL」或整段请求头，那里
/// `Cookie:` 后面常跟若干行 `name=value;`。换行原样塞进 `Cookie` 头会让请求在
/// 第一个 `\r\n` 处断掉 —— 上游回的是门户 HTML 首页（**HTTP 200**，不是 401），
/// 而这条通道对失败是静默降级，用户完全看不出是粘贴格式的锅。
///
/// 读取侧也过这一遍（不只是写入侧）：早期版本可能已经把多行串落了库，
/// 只在写入处规整的话，那些老记录要重新添加一次才能恢复。
pub fn normalize_cookie(raw: &str) -> String {
    let raw = raw.trim();
    if raw.is_empty() {
        return String::new();
    }
    // 每行去空白 → 剥掉可能连头名一起复制的 `Cookie:` → 丢空行 →
    // 行尾没有 `;` 的补一个 → 空格连接。
    let body = raw
        .lines()
        .map(str::trim)
        .map(|line| {
            let line = line
                .strip_prefix("Cookie:")
                .or_else(|| line.strip_prefix("cookie:"))
                .unwrap_or(line);
            line.trim()
        })
        .filter(|line| !line.is_empty())
        .map(|line| {
            if line.ends_with(';') {
                line.to_string()
            } else {
                format!("{line};")
            }
        })
        .collect::<Vec<_>>()
        .join(" ");
    // 末尾那个 `;` 是合法的（`a=1; b=2;` 与 `a=1; b=2` 等价），去掉更整洁。
    body.trim_end_matches(';').trim().chars().take(MAX_COOKIE_LENGTH).collect()
}

/// 从一条已归一的 cookie 串里取某个键的值（`None` = 没这个键）。
///
/// `cftk` 头要用 `devclouddevuibjtcftk` 的值，而它就在同一条 cookie 里 ——
/// 让用户为同一件事贴两次没有道理（且两次一定会有一次贴错）。
pub fn cookie_value<'a>(cookie: &'a str, key: &str) -> Option<&'a str> {
    cookie.split(';').find_map(|pair| {
        let pair = pair.trim();
        let (name, value) = pair.split_once('=')?;
        (name.trim() == key).then(|| value.trim()).filter(|value| !value.is_empty())
    })
}

fn first_non_empty(values: &[String]) -> String {
    values
        .iter()
        .map(|value| value.trim())
        .find(|value| !value.is_empty())
        .unwrap_or_default()
        .to_string()
}

#[cfg(any())]
mod tests {
    use super::*;
    use serde_json::json;

    fn credential(expires_at: &str, token: &str) -> Credential {
        Credential {
            access_key_id: "AK".to_string(),
            secret_access_key: "SK".to_string(),
            security_token: token.to_string(),
            expires_at: expires_at.to_string(),
            ..Credential::default()
        }
    }

    /// 参考实现的口径：`expiry <= now + lead` 就算要刷，`lead = 0` 即「过期了吗」。
    /// `2026-09-26T16:17:00.327Z` 的毫秒值是 `1790439420327`（含那 327 毫秒 ——
    /// 解析出来必须带小数部分，丢了它就说明用了秒级解析）。
    #[test]
    fn expiry_decides_refresh_with_the_same_lead_semantics() {
        const EXPIRY: i64 = 1790439420327;
        const MINUTE: i64 = 60 * 1000;
        let credential = credential("2026-09-26T16:17:00.327Z", "sts");
        let at = |minutes_before: i64| EXPIRY - minutes_before * MINUTE;
        // 15 分钟提前量：离到期还有 10 分钟 → 要刷；还有 30 分钟 → 还不刷；正好 15 → 要刷
        assert!(credential.needs_refresh(15 * MINUTE, at(10)));
        assert!(credential.needs_refresh(15 * MINUTE, EXPIRY - 15 * MINUTE), "边界是闭区间");
        assert!(!credential.needs_refresh(15 * MINUTE, at(30)));
        // 45 分钟提前量：离到期 30 分钟就该刷了（提前量越大越早动手）
        assert!(credential.needs_refresh(45 * MINUTE, at(30)));
        // 0 提前量只回答「过期了吗」
        assert!(!credential.needs_refresh(0, at(1)));
        assert!(credential.needs_refresh(0, EXPIRY));
        assert_eq!(Some(EXPIRY), credential.expires_at_ms());
    }

    #[test]
    fn incomplete_or_unparseable_credentials_are_handled_deliberately() {
        // 没有 security token：本来就不完整，不作为「要刷」的信号
        assert!(!credential("2026-09-26T16:17:00.327Z", "").needs_refresh(0, i64::MAX));
        // 没有到期时间：无从判断
        assert!(!credential("", "sts").needs_refresh(0, i64::MAX));
        // 解析不出来：**当作要刷**（坏时间戳不能被当成新鲜）
        assert!(credential("not-a-timestamp", "sts").needs_refresh(0, 0));
    }

    #[test]
    fn identity_and_account_matching_fall_back_in_order() {
        let mut left = credential("", "sts");
        left.domain_id = "dom".to_string();
        left.user_id = "uid".to_string();
        let mut right = credential("", "sts");
        right.domain_id = "dom".to_string();
        right.user_id = "uid".to_string();
        assert!(left.same_account(&right));
        right.user_id = "other".to_string();
        assert!(!left.same_account(&right));
        // 只有 user_name 时按 user_name 兜
        let mut named = credential("", "sts");
        named.user_name = "n".to_string();
        let mut same = credential("", "sts");
        same.user_name = "n".to_string();
        assert!(named.same_account(&same));
    }

    #[test]
    fn payload_parsing_accepts_both_nested_and_flat_shapes() {
        let nested = json!({
            "codearts_provider_credential": {
                "access_key_id": "AK1",
                "secret_access_key": "SK1",
                "security_token": "sts",
                "expiration": "2026-09-27T00:00:00Z",
                "domain_id": "dom",
                "user_id": "uid",
                "refresh_token": "jwt",
                "oauth_context": {
                    "pkce_pair": {"code_verifier": "v", "code_challenge": "c", "code_challenge_method": "SHA-256"},
                    "dpop_key_pair": {"private_key_jwk": {"kty": "EC", "crv": "P-256", "x": "x", "y": "y", "d": "d"}}
                }
            }
        });
        let credential = Credential::from_payload(&nested).expect("嵌套形状应当能解析");
        assert_eq!("AK1", credential.access_key_id);
        assert_eq!("2026-09-27T00:00:00Z", credential.expires_at, "expiration 要归一到 expires_at");
        assert_eq!("WEB", credential.login_type, "缺 login_type 时按 WEB 兜底");
        assert!(credential.can_refresh(), "有 verifier 与 refresh token 就应当能续期");

        // 平铺 + accessKeyId 这类驼峰别名不该被认成有效（不给它乱猜的机会）
        let flat = json!({"accessKeyId": "AK2", "secretAccessKey": "SK2"});
        assert!(Credential::from_payload(&flat).is_err(), "只有驼峰名时应当明确报错而不是收下一份空凭据");

        let flat_ok = json!({"access_key_id": "AK3", "secret_access_key": "SK3"});
        assert_eq!("AK3", Credential::from_payload(&flat_ok).expect("平铺应当能解析").access_key_id);
    }
}
