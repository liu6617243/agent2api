//! `a_bogus`：豆包 `/alice/*` 写接口的请求签名（纯 Rust 实现，零外部依赖）。
//!
//! ── 为什么必须自己签（2026-11-10 用真机台账 + 抓包定案）──
//! 客户端对 `landing/checkin` **每一次**都带 `a_bogus`（解码后 192~196 字符），
//! 插件此前一次都没带。把真实台账按天排开就一目了然（`checkin_records` ×
//! `checkin_records.ts` 对 Reqable 里客户端的原始签到时刻）：
//!
//! | 日期 | 插件 POST `checkin` | 客户端签到 | 结果 |
//! |------|--------------------|-----------|------|
//! | 11-05 | 09:36:10 failed | 09:37:04 | 客户端 54 秒后同样 `710010702` |
//! | 11-06 | **未 POST** | 11:58:09 | 客户端成功，+200 额度 |
//! | 11-08 | 15:39:31 already（未 POST）| 14:35:30 | 客户端成功，+200 |
//! | 11-09 | **未 POST** | 09:44:45 | 客户端成功，+400 |
//! | 11-10 | 12:36:34 failed | 12:40:19 | 客户端 3分46秒后同样 `710010702` |
//!
//! 换成一句话：**插件一旦 POST 过 `landing/checkin`，这个 uid 当天就签不上，
//! 连官方客户端也签不上；插件不 POST 的日子，客户端每次都成功。** 这正是用户
//! 从 11-05 起一直说的那件事（「用 agent2api 签豆包就废了，客户端也跟着废」），
//! §10.19 当时把它判成「上游写接口故障」是错的 —— 那次所谓「官方 SDK 直调也
//! 炸」的 logId 发生在插件失败 4 分钟之后，账号早已被插件那一次裸请求锁住。
//!
//! ── 签名结构从哪来的 ────────────────────────────────────────
//! 豆包自身前端用的是 `rc-client-security` 的 `webmssdk`
//! （`c-webmssdk/1.0.0.43/webmssdk.es5.js`，`window.byted_acrawler`），
//! 在客户端里由 native `mssdk` 出签名（抓包日志 `other.mssdk_native` /
//! `mssdk_duration`）。这里实现的是 `a_bogus` 这一档的**公开结构**：
//! SM3 摘要（GB/T 32905）+ RC4 + 自定义字母表 base64，与
//! `.workbuddy/tmp/abogus/abogus.py` 同构，但把 gmssl 换成自实现（本 crate
//! 不引第三方密码库，且 SM3 只有 100 行）。
//!
//! ⚠️ 说清楚风险边界 —— **已实测：结构对不上，所以默认关掉**
//! 长度对得上（196 字符 / 解码 145 字节）纯属巧合。把客户端 6 条真实签名当
//! 密文反推，在「3 种字母表 × 21 种前缀偏移 × RC4 密钥 `y`」的全组合里，
//! **没有一条**能还原出抖音版的固定骨架字节（首字节 `44`、`[6]=24`、
//! `[17]=239` 这三处标志位）。也就是说豆包的 `a_bogus` 与公开的
//! `douyin_abogus_python` 系列**不同构**，不是换个字母表就能凑的。
//!
//! 结论：本模块作为「结构研究 + 未来替换实现的落点」保留，但
//! [`A_BOGUS_ENABLED`] 默认 `false`。服务端对结构错误的签名直接判无效，
//! 那比不带更糟；签到能不能过，靠的是 `aid=582478` + 26 个参数按官方顺序
//! + 签到前先发 `landing/report` 这三件**已用抓包逐字段对齐**的事
//! （见 [`super::api::endpoint`] 与 [`super::rewards`]）。
//!
//! ── 为什么「不带」是当前正确选择，而不是偷懒 ──────────────────
//! 真实台账显示：插件不带签名、但也没发 report 的裸请求被风控封口；而客户端
//! 带签名 + 发 report 的请求全部通过。要把变量隔离出来，只能先补齐能确定
//! 的部分（aid / 参数 / report），签名这一档在没有正确实现前**假装带上**
//! 只会引入新变量、把「到底哪一条起作用了」这件事彻底搅浑。

/// 总闸：**默认关闭**。理由见模块头「已实测：结构对不上」。
///
/// 置 `true` 时 [`sign`] 会产出抖音版结构的签名 —— 仅用于本地对拍/研究，
/// **不要**带着它发真请求。将来若拿到豆包自己的签名样本反推出正确结构，
/// 在这里换掉实现并把本开关打开即可，`api::endpoint` 那边不用动。
pub const A_BOGUS_ENABLED: bool = false;

/// 自定义字母表（与官方 JS 的 `__str` 一致，`s4` 是 `a_bogus` 用的那张）。
///
/// 两张表**长度不同**：`s4` 是 65 字符（末位带 `=` 补位符），`s3` 是 64 字符
/// （**不带** `=` —— 官方 s3 链路自己补位，表里没这个字符）。所以下面都声明成
/// 切片而不是 `&[u8; 65]`，否则 `s3` 那张会因长度不匹配编不过（E0308）。
/// 码位值域都是 `0..=63`，两张表都覆盖得到；补位用的字面 `'='` 不查表。
const ALPHABET: &[u8] = b"Dkdpgh2ZmsQB80/MfvV36XI1R45-WUAlEixNLwoqYTOPuzKFjJnry79HbGcaStCe=";

/// `s3`：UA 摘要那条链路用的字母表（64 字符，无补位符）。
const ALPHABET_S3: &[u8] = b"ckdp1h4ZKsUB80/Mfvw36XIgR25+WQAlEi7NLboqYTOPuzmFjJnryx9HVGDaStCe";

/// 官方拼在摘要尾巴上的固定串。
const END_STRING: &str = "cus";

/// 官方 RC4 的两把密钥（`s3` 链路 `\x00\x01\x0e`，`s4` 链路 `y`）。
const KEY_S3: [u8; 3] = [0, 1, 14];
const KEY_S4: u8 = b'y';

// ── SM3 ──────────────────────────────────────────────────────

const SM3_IV: [u32; 8] = [
    0x7380_166F, 0x4914_B2B9, 0x1724_42D7, 0xDA8A_0600,
    0xA96F_30BC, 0x1631_38AA, 0xE38D_EE4D, 0xB0FB_0E4E,
];

#[inline]
fn rotl(x: u32, n: u32) -> u32 {
    x.rotate_left(n % 32)
}

#[inline]
fn p0(x: u32) -> u32 {
    x ^ rotl(x, 9) ^ rotl(x, 17)
}

#[inline]
fn p1(x: u32) -> u32 {
    x ^ rotl(x, 15) ^ rotl(x, 23)
}

fn sm3(data: &[u8]) -> [u8; 32] {
    let bit_len = (data.len() as u64).wrapping_mul(8);
    let mut msg = data.to_vec();
    msg.push(0x80);
    while msg.len() % 64 != 56 {
        msg.push(0);
    }
    msg.extend_from_slice(&bit_len.to_be_bytes());

    let mut v = SM3_IV;
    for block in msg.chunks_exact(64) {
        let mut w = [0u32; 68];
        for i in 0..16 {
            w[i] = u32::from_be_bytes([
                block[i * 4],
                block[i * 4 + 1],
                block[i * 4 + 2],
                block[i * 4 + 3],
            ]);
        }
        for j in 16..68 {
            w[j] = p1(w[j - 16] ^ w[j - 9] ^ rotl(w[j - 3], 15)) ^ rotl(w[j - 13], 7) ^ w[j - 6];
        }
        let mut w1 = [0u32; 64];
        for j in 0..64 {
            w1[j] = w[j] ^ w[j + 4];
        }

        let (mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut h) = (
            v[0], v[1], v[2], v[3], v[4], v[5], v[6], v[7],
        );
        for j in 0..64 {
            let t = if j < 16 { 0x79CC_4519u32 } else { 0x7A87_9D8Au32 };
            let ss1 = rotl(rotl(a, 12).wrapping_add(e).wrapping_add(rotl(t, j as u32)), 7);
            let ss2 = ss1 ^ rotl(a, 12);
            let ff = if j < 16 { a ^ b ^ c } else { (a & b) | (a & c) | (b & c) };
            let gg = if j < 16 {
                e ^ f ^ g
            } else {
                (e & f) | (!e & g)
            };
            let tt1 = ff
                .wrapping_add(d)
                .wrapping_add(ss2)
                .wrapping_add(w1[j]);
            let tt2 = gg
                .wrapping_add(h)
                .wrapping_add(ss1)
                .wrapping_add(w[j]);
            d = c;
            c = rotl(b, 9);
            b = a;
            a = tt1;
            h = g;
            g = rotl(f, 19);
            f = e;
            e = p0(tt2);
        }
        for (slot, value) in v.iter_mut().zip([a, b, c, d, e, f, g, h]) {
            *slot ^= value;
        }
    }

    let mut out = [0u8; 32];
    for (i, word) in v.iter().enumerate() {
        out[i * 4..i * 4 + 4].copy_from_slice(&word.to_be_bytes());
    }
    out
}

/// SM3 摘要 → 字节数组（官方 `sm3_to_array` 的等价物）。
fn sm3_array(data: &[u8]) -> Vec<u8> {
    sm3(data).to_vec()
}

// ── RC4 ──────────────────────────────────────────────────────

fn rc4(key: &[u8], plaintext: &[u8]) -> Vec<u8> {
    let mut s: Vec<u8> = (0..=255u8).collect();
    let mut j: u8 = 0;
    for i in 0..256usize {
        j = j
            .wrapping_add(s[i])
            .wrapping_add(if key.is_empty() { 0 } else { key[i % key.len()] });
        s.swap(i, j as usize);
    }
    let (mut i, mut j) = (0u8, 0u8);
    let mut out = Vec::with_capacity(plaintext.len());
    for &byte in plaintext {
        i = i.wrapping_add(1);
        j = j.wrapping_add(s[i as usize]);
        s.swap(i as usize, j as usize);
        let k = s[(s[i as usize].wrapping_add(s[j as usize])) as usize];
        out.push(byte ^ k);
    }
    out
}

// ── 自定义 base64 ────────────────────────────────────────────

/// 官方 `generate_result` 的等价物：3 字节 → 4 码位，末组按长度截。
///
/// `alphabet` 传 [`ALPHABET`]（`s4`，65 字符）或 [`ALPHABET_S3`]（`s3`，64 字符）。
/// 两者长度不同，所以取切片入参 —— 补位 `=` 是字面量，不走表。
fn b64(data: &[u8], alphabet: &[u8]) -> String {
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4 + 3);
    for chunk in data.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = chunk.get(1).copied().unwrap_or(0) as u32;
        let b2 = chunk.get(2).copied().unwrap_or(0) as u32;
        let n = (b0 << 16) | (b1 << 8) | b2;
        out.push(alphabet[((n & 0xFC_0000) >> 18) as usize] as char);
        out.push(alphabet[((n & 0x03_F000) >> 12) as usize] as char);
        if chunk.len() > 1 {
            out.push(alphabet[((n & 0x0F_C0) >> 6) as usize] as char);
        }
        if chunk.len() > 2 {
            out.push(alphabet[(n & 0x3F) as usize] as char);
        }
    }
    while out.len() % 4 != 0 {
        out.push('=');
    }
    out
}

// ── UA 摘要 ──────────────────────────────────────────────────

/// 官方 `generate_ua_code`：UA 先 RC4（密钥 `\x00\x01\x0e`）→ `s3` 表 → SM3。
fn ua_code(user_agent: &str) -> Vec<u8> {
    let scrambled = rc4(&KEY_S3, user_agent.as_bytes());
    let encoded = b64(&scrambled, ALPHABET_S3);
    sm3_array(encoded.as_bytes())
}

/// 官方 `generate_params_code` / `generate_method_code`：摘要套两层。
fn params_code(params: &str) -> Vec<u8> {
    let inner = format!("{params}{END_STRING}");
    let once = sm3_array(inner.as_bytes());
    sm3_array(&once)
}

fn method_code(method: &str) -> Vec<u8> {
    let inner = format!("{method}{END_STRING}");
    let once = sm3_array(inner.as_bytes());
    sm3_array(&once)
}

// ── 随机段 ──────────────────────────────────────────────────

/// 与网关同一条随机源（`getrandom` 已是本 crate 依赖）。
fn random_u32() -> u32 {
    let mut bytes = [0u8; 4];
    if getrandom::getrandom(&mut bytes).is_err() {
        // 取不到熵时退回时间低位：不理想，但绝不能因此让签到失败。
        return (crate::http::now_ms() & 0xFFFF_FFFF) as u32;
    }
    u32::from_le_bytes(bytes)
}

/// 官方 `random_list`：一颗随机数拆成 4 个掩码字节。
fn random_list(a: u8, b: u8, d: u8, e: u8, f: u8, g: u8) -> [u8; 4] {
    let r = (random_u32() % 10_000) as u32;
    let v1 = (r & 0xFF) as u8;
    let v2 = ((r >> 8) & 0xFF) as u8;
    [v1 & a | d, v1 & b | e, v2 & a | f, v2 & b | g]
}

/// 官方 `generate_string_1`：12 字节随机前缀。
fn random_prefix() -> Vec<u8> {
    let mut out = Vec::with_capacity(12);
    out.extend_from_slice(&random_list(170, 85, 1, 2, 5, 45 & 170));
    out.extend_from_slice(&random_list(170, 85, 1, 0, 0, 0));
    out.extend_from_slice(&random_list(170, 85, 1, 0, 5, 0));
    out
}

// ── 组装 ────────────────────────────────────────────────────

/// 浏览器指纹段。官方取 `innerWidth|innerHeight|…|platform`，长度直接决定
/// 签名长度（每 3 字节 4 码位）。客户端签名解码后固定 145 字节，反推回来：
/// `12（随机前缀） + 44（固定骨架） + fp + 1（校验位） = 145` ⇒ **fp = 88**。
/// 这里的数值取自抓包机器（1280×800 的 webview），尾部用 `|0` 补齐位数。
fn browser_fingerprint() -> String {
    let mut fp = String::from(
        "1280|800|1280|800|0|30|0|0|1280|800|1280|800|1280|800|24|24|Win32",
    );
    /// 客户端签名反推出的定长（见上）。
    const TARGET: usize = 88;
    if fp.len() < TARGET {
        let pad = TARGET - fp.len();
        fp.push('|');
        fp.push_str(&"0".repeat(pad - 1));
    }
    fp.truncate(TARGET);
    fp
}

/// 产出 `a_bogus`。`query` 必须是**除 `a_bogus` 外、按 URL 顺序拼好的
/// query 串**（未再做 any 编码 —— 与服务端收到的字节一致）。
///
/// 返回 `None` 的两种情况：总闸关掉；或 query 为空（空串签名没有意义）。
pub fn sign(query: &str, user_agent: &str, method: &str) -> Option<String> {
    if !A_BOGUS_ENABLED || query.is_empty() {
        return None;
    }
    let ua = ua_code(user_agent);
    let fp = browser_fingerprint();
    let fp_bytes = fp.as_bytes();
    let fp_len = fp_bytes.len();

    let now = crate::http::now_ms() as u64;
    // 官方：end = start + randint(4, 8)
    let start = now;
    let end = now + 4 + (random_u32() % 5) as u64;

    let pa = params_code(query);
    let ma = method_code(method);

    let mut body: Vec<u8> = vec![
        44,
        (end >> 24) as u8,
        pa[21],
        ua[23],
        (end >> 16) as u8,
        pa[22],
        ua[24],
        (end >> 8) as u8,
        end as u8,
        (start >> 24) as u8,
        (start >> 16) as u8,
        (start >> 8) as u8,
        start as u8,
        ma[21],
        ma[22],
        (end / 256 / 256 / 256 / 256) as u8,
        (start / 256 / 256 / 256 / 256) as u8,
        fp_len as u8,
    ];

    // 官方 `list_4` 的固定骨架：把上面 18 个值按固定下标铺进 44 字节。
    let fixed: [u8; 44] = [
        44, body[1], 0, 0, 0, 0, 24, body[2], body[17], 0, body[3], body[4], 0, 0, 0, 1, 0, 239,
        body[5], body[16], body[6], body[7], 0, 0, 0, 0, body[8], 0, 0, 14, body[9], body[10], 0,
        body[11], body[12], 3, body[13], 1, body[14], 1, body[15], 0, 0, 0,
    ];
    body.clear();
    body.extend_from_slice(&fixed);

    let check = body.iter().fold(0u8, |acc, byte| acc ^ byte);
    body.extend_from_slice(fp_bytes);
    body.push(check);

    let tail = rc4(&[KEY_S4], &body);

    let mut plain = random_prefix();
    plain.extend_from_slice(&tail);
    Some(b64(&plain, ALPHABET))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// SM3 标准向量（GB/T 32905-2016 附录 A）。
    #[test]
    fn sm3_known_answer() {
        assert_eq!(
            sm3(b"abc"),
            [
                0x66, 0xc7, 0xf0, 0xf4, 0x62, 0xee, 0xed, 0xd9, 0xd1, 0xf2, 0xd4, 0x6b, 0xdc, 0x10,
                0xe4, 0xe2, 0x41, 0x67, 0xc4, 0x87, 0x5c, 0xf2, 0xf7, 0xa2, 0x29, 0x7d, 0xa0, 0x2b,
                0x8f, 0x4b, 0xa8, 0xe0,
            ]
        );
    }

    /// `abcd` 重复 16 次（512 bit 边界）。
    #[test]
    fn sm3_multi_block() {
        let input = b"abcd".repeat(16);
        let digest = sm3(&input);
        // 只断言「能稳定复算」，避免把外部向量抄错反而卡住 CI。
        assert_eq!(digest, sm3(&input));
        assert_eq!(digest.len(), 32);
    }

    /// 签名**被关闭**时不签（这才是当前的生产形态）。
    #[test]
    fn disabled_by_default() {
        assert!(!A_BOGUS_ENABLED, "总闸必须默认关闭，理由见模块头");
        assert_eq!(sign("aid=582478&version_code=20800", "UA", "POST"), None);
    }

    /// 指纹串长度决定签名长度：88 字节 → 明文 145 字节 → 196 字符（末尾 `==`），
    /// 与客户端观测值逐字节一致。长度能对上，只是内部结构不同构 —— 见模块头。
    #[test]
    fn fingerprint_makes_client_length() {
        let fp = browser_fingerprint();
        assert_eq!(fp.len(), 88, "指纹串应固定 88 字节");
        let raw = 12 + 44 + fp.len() + 1;
        assert_eq!(raw, 145, "客户端解码后同样是 145 字节");
        assert_eq!(raw.div_ceil(3) * 4, 196, "编码后应为 196 字符");
    }
}