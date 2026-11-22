//! 记忆中枢 · 请求后留存（把这一轮真正发生的事写回 l1）。
//!
//! ── 这个文件解决什么问题 ────────────────────────────────────
//! [`super::inject`] 解决了「记忆写得进去、模型看得见」的前半边：网关在转发前
//! 主动把相关记忆塞进请求。但那只覆盖了**读**。真机上把注入修通之后，立刻暴露
//! 了另外半边：
//!
//! ```text
//! 用户提问 → 注入记忆 → 模型回答 → 会话结束 → 什么都没写回记忆库
//! ```
//!
//! 结果就是「同样的坑第二次还是要我讲一遍」。用户的原话是「新的记忆会自己记录
//! 进记忆系统」—— 靠客户端提示词让模型自觉调 `memory_core` 写，实测调用次数 1、
//! 真正写入 0。跟「模型不会主动读」是同一个病根：**把该做的事放在有选择权的位置**。
//!
//! 所以这里换位置：网关在**响应结束后**自己判断「这一轮有没有值得记的东西」，
//! 有就写一条 l1。
//!
//! ── 为什么不等模型调工具，而在网关里截 ──────────────────────
//! Hindsight（`vectorize-io/hindsight`）的 `wrap_openai()` 就是这个形状：
//! 调用前 recall、调用后 retain。它是所有参考项目里唯一与 agent2api 注入位置
//! **同构**的一个 —— 那恰好证明「网关位置」是对的，同时暴露本仓缺了 retain 这一半。
//!
//! ── 写什么，不写什么 ────────────────────────────────────────
//! 只写「用户说了什么 + 助手结论了什么」。**不写工具回显**：
//!
//!   · 一次 `Read` 能回显 5 万字符源码，写进去等于把记忆库变成代码备份；
//!   · 工具回显里没有「结论」，只有过程 —— 而记忆要存的是结论；
//!   · 同一个文件每次读都不一样（行号变了），写进去就是无数条近似重复。
//!
//! ── 为什么不写「成功 / 失败」字段 ────────────────────────────
//! 用户明确说过：「成败并不是一定要作为结论被记录……除非事件不是以正常结束，
//! 而是以失败或者放弃结束」。所以默认不标成败；只有**明确异常收尾**（上游报错、
//! 流被中断）才在正文里写一句，作为事实陈述而不是判定字段。
//!
//! ── 绝不因为留存失败而影响请求 ──────────────────────────────
//! 留存挂在响应已经下发**之后**。任何失败（写盘、索引、序列化）只能让「这一轮
//! 没记下来」，绝不能让用户看到一个已经拿到的回答变成错误。
//!
//! ── 为什么不记每一轮 ────────────────────────────────────────
//! 逐轮写会把库灌成流水账：`你好` / `继续` / `谢谢` 各一条，几个月后记忆库里
//! 九成是寒暄。本机 5,997 条里 5,931 条就是这种流水（导入进来的），而它们对
//! 「下次怎么做更快」几乎没有贡献。所以这里有一道**内容闸**（[`worth_keeping`]）：
//! 只留「有结论、有决策、有踩坑、有明确要求」的那几轮。

use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use serde_json::Value;

use super::super::protocol;
use super::config;

/// 一轮里「用户侧」最多保留多少字符。
///
/// 用户的输入常常很长（贴一整段日志、一整份需求）。留 4,000 足够表达「他要什么」，
/// 再多就是日志正文 —— 而那部分对「下次怎么做」没有指示性。
const USER_CAP: usize = 4_000;
/// 「助手侧」最多保留多少字符。
///
/// 助手的结论往往集中在开头（`**结论**` / 第一段），尾部多是收尾寒暄。8,000 够
/// 装下一次完整的技术结论，又不会让一条记忆变成整篇文档。
const ASSISTANT_CAP: usize = 8_000;
/// 整条记忆正文的硬上限。超过就截断 —— `.md` 是要人读的，不是数据库行。
const BODY_CAP: usize = 12_000;
/// 太短的回答不值得记（`好的` / `收到` / 空响应）。
const MIN_ANSWER: usize = 20;

/// 留存旋钮。
#[derive(Debug, Clone, Copy)]
pub struct Settings {
    pub enabled: bool,
    /// 一轮里至少要有这么多字符的「实质回答」才考虑留存
    pub min_chars: usize,
    /// 一轮里至多保留多少字符
    pub max_chars: usize,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            enabled: true,
            min_chars: MIN_ANSWER,
            max_chars: BODY_CAP,
        }
    }
}

pub fn settings() -> Settings {
    Settings {
        enabled: config::bool_of("retain.enabled"),
        min_chars: config::usize_of("retain.minChars").clamp(0, 2_000),
        max_chars: config::usize_of("retain.maxChars").clamp(500, 40_000),
    }
}

/// 这一轮的用户侧文本：最近一条 user 消息里「真正是他说的话」。
///
/// ── 为什么用 `user_turn_text` 而不是 `chat_request_text_from` ─
/// 后者是**给探针用的宽口径**：用户只说「继续」时它会退到最近工具回显的开头，
/// 好让注入还能靠命令行里的路径命中。留存不能这么干 —— 记忆里写的是
/// `**我**：…`，把 5 万字符的工具回显塞进去等于把记忆库变日志备份，而且
/// 会永久固化一批「用户好像发了一屏 shell 输出」的假记录。
///
/// 两条路仍然共用**同一个**「哪条 user 消息、哪个内容块」的判据（`user_queries`
/// 与尾部窗口都在 `inject` 里），只是留存不要那层工具回退。
pub fn user_text(messages: &[Value]) -> String {
    let text = super::inject::user_turn_text(messages);
    let text = strip_client_wrappers(&text);
    text.chars().take(USER_CAP).collect()
}

/// 把客户端的包装块从「用户侧」剔掉。
///
/// ── 为什么留存要单独剔一遍 ──────────────────────────────────
/// [`super::inject::user_turn_text`] 是**两边共用**的取词口径，它必须宽：注入侧
/// 在用户只说「继续」时要靠历史摘要里的路径 / 标识符去索引记忆，剔掉等于让
/// 注入永远命中不了。
///
/// 但留存写的是 `**我**：…`。同一段摘要被写进去，就变成「我上一轮总结过…」
/// 的伪记录：每轮都把上一轮的结论再存一遍，库开始自我增殖；更糟的是它会把
/// 真正的一句话挤到后面，读记忆时看到的是模型自己的旧总结。
///
/// 所以这里按客户端自己的标签剔：那些块是**客户端**写的，不是用户说的。
/// 只剔整块命中包装标签的段落，正常的一句话不受影响。
fn strip_client_wrappers(text: &str) -> String {
    const WRAPPERS: &[(&str, &str)] = &[
        ("<conversation_history_summary>", "</conversation_history_summary>"),
        ("<system-reminder>", "</system-reminder>"),
        ("<task-notification>", "</task-notification>"),
        ("<environment_context>", "</environment_context>"),
    ];
    let mut out = text.to_string();
    for (open, close) in WRAPPERS {
        while let Some(start) = out.find(open) {
            let rest = &out[start + open.len()..];
            // 有闭合标签：整段剔掉。没有闭合标签（真机出现过：摘要块只有开标签）
            // 就从开标签一直剔到结尾 —— 留半截摘要比整段丢掉更糟。
            let end = match rest.find(close) {
                Some(at) => start + open.len() + at + close.len(),
                None => out.len(),
            };
            out.replace_range(start..end, "");
        }
    }
    // 上一轮摘要的另一族写法：`<summary>…</summary>` 也按同样方式处理
    while let Some(start) = out.find("<summary>") {
        let rest = &out[start + "<summary>".len()..];
        let end = match rest.find("</summary>") {
            Some(at) => start + "<summary>".len() + at + "</summary>".len(),
            None => out.len(),
        };
        out.replace_range(start..end, "");
    }
    out.trim().to_string()
}

/// 助手侧文本：最近一条 assistant 消息的正文（不含工具调用块）。
pub fn assistant_text(messages: &[Value]) -> String {
    for message in messages.iter().rev() {
        if message.get("role").and_then(Value::as_str) != Some("assistant") {
            continue;
        }
        let content = message.get("content").cloned().unwrap_or(Value::Null);
        let text = protocol::content_text(&content);
        if !text.trim().is_empty() {
            return text.chars().take(ASSISTANT_CAP).collect();
        }
    }
    String::new()
}

/// 这段回答值不值得写成一条记忆。
///
/// ── 判据为什么这么保守 ──────────────────────────────────────
/// 记忆库的价值在**密度**，不在条数。本机 5,997 条里 5,931 条是导入的会话流水，
/// 而注入跑分显示它们一条都过不了门槛（`score >= floor` 全为 0）—— 也就是说
/// 「什么都记」等于「什么都没记」，只是把库撑大、把检索拖慢。
///
/// 所以这里只认三种形状（任一条命中即留）：
///
/// | 形状 | 例子 | 为什么留 |
/// |---|---|---|
/// | 有结论标记 | `**结论**` / `根因是` / `所以` / `结论：` | 助手的判断，最值钱 |
/// | 有踩坑描述 | `坑` / `注意` / `不能用` / `会失败` / `必须` | 下次会再踩到 |
/// | 有一组具体标识 | 路径 / 命令 / 报错码 | 能被下面的线索命中 |
///
/// 反过来，「好的」「收到」「要我继续吗」这类回答一个都不命中 —— 它们本来也
/// 不是结论。
fn worth_keeping(user: &str, answer: &str, min_chars: usize) -> bool {
    let answer = answer.trim();
    if answer.chars().count() < min_chars {
        return false;
    }
    let lowered = answer.to_lowercase();
    const VERDICT: &[&str] = &[
        "**结论**", "结论：", "结论:", "根因", "所以", "因此", "修好了", "已修复",
        "改完了", "出包", "artifact", "run =", "踩坑", "坑在", "注意", "不能用",
        "会失败", "必须", "不要", "别用", "建议", "区别是", "口径",
    ];
    if VERDICT.iter().any(|word| lowered.contains(&word.to_lowercase())) {
        return true;
    }
    // 没有结论词，但回答里有一组具体标识（路径 / 命令）—— 那多半在描述改了什么
    if !super::inject::probes(answer).is_empty() {
        return true;
    }
    // 用户说了一段很长的话，助手也回了一段不短的话：那多半是在讨论具体任务
    // （而不是寒暄）。两侧都够长时放行。
    user.chars().count() >= 200 && answer.chars().count() >= 400
}

/// 把一轮对话拼成一条 l1 正文。
///
/// 形状固定成 `**我**：… / **助手**：…` —— 与导入器写进来的形状**逐字**一致
/// （`import::line_to_turn` 把 user 折成 `我`、assistant 折成 `助手`），所以
/// 检索、摘要、蒸馏都不需要为「网关写的」和「导入的」分两套解析。
///
/// 早先这里写的是 `**用户**：`，读起来更正式，但那会让同一份库里出现两种用户
/// 标签 —— 任何按 `**我**：` 切块的逻辑（摘要、蒸馏、指纹去重）都会把网关写的
/// 那些条目整条跳过，而且不会报错。
fn compose_body(user: &str, answer: &str, interrupted: bool) -> String {
    let mut body = String::new();
    if !user.trim().is_empty() {
        body.push_str("**我**：");
        body.push_str(user.trim());
        body.push_str("\n\n");
    }
    body.push_str("**助手**：");
    body.push_str(answer.trim());
    if interrupted {
        // 「异常收尾」才标注，正常收尾一个字都不加 —— 用户的明确口径：
        // 默认就是成功的，只有失败或放弃才需要额外说一句。
        body.push_str("\n\n（本轮未正常结束：响应被中断或上游报错）");
    }
    body
}

/// 这一轮的标题。
///
/// ── 为什么网关要自己给标题，而不是留给 `infer_title` ────────
/// 写入侧的 `infer_title` 取「正文第一个非空行」。而这里正文的第一个非空行
/// 恒为 `**我**：…` —— 真机库里 30 条留存全部显示成
/// 「**我**：动第一批，第二批和第三批我还没理解是什么意思…」。
/// 那不是标题，那是标签被当成内容。列表页一屏全是同一个前缀，扫一眼分不出
/// 哪条是哪条，等于把标题这一列废掉。
///
/// 所以这里在网关侧自己拼：用户的话够长就用它（`**我**：` 之后的原话本来
/// 就是「这一轮在谈什么」），用户只说了「继续」才退到助手的第一句。
fn infer_title(user: &str, answer: &str) -> String {
    /// 标题太短说明不了「这一轮在谈什么」（「继续」「改一下」「看下」）
    const MIN_TITLE: usize = 12;
    fn first_line(text: &str) -> String {
        text.lines()
            .map(str::trim)
            .find(|line| !line.is_empty() && !line.starts_with('#'))
            .unwrap_or("")
            // 标题是纯文本展示，`**` / `-` / `>` 这些标记留着会把列表页渲染成
            // 一堆断开的重号
            .trim_start_matches(|ch: char| matches!(ch, '*' | '-' | '>' | '·' | ' '))
            .trim()
            .to_string()
    }
    let from_user = first_line(user);
    let picked = if from_user.chars().count() >= MIN_TITLE {
        from_user
    } else {
        let from_answer = first_line(answer);
        if from_answer.chars().count() >= MIN_TITLE {
            from_answer
        } else {
            "对话留存".to_string()
        }
    };
    picked.chars().take(60).collect()
}

/// 从请求里取工作目录。
///
/// ── 为什么必须取到它 ────────────────────────────────────────
/// 真机事故：修好注入之后，库里的留存全部落在 `general/`，一条都没进
/// `projects/`。原因是 `remember_turn` 的 `cwd` 一直传 `None`，而项目归类
/// 在写入侧是「显式项目名 > cwd 的 git 身份 > general」—— 三者都没有就进
/// `general`。结果就是「在 agent2api 这个仓库里干了三天活，记忆库里一条都
/// 不在 agent2api 项目下」，项目页永远是空的，注入也少了「按项目取」这一档。
///
/// ── 为什么不指望客户端给 `cwd` 字段 ─────────────────────────
/// OpenAI 协议没有这个字段，WorkBuddy / Claude Code 都不发。它们把工作目录
/// 写在**上下文块**里：`<system-reminder>今天是 …，工作目录 D:\…。</…>`、
/// `<environment_context><cwd>…</cwd></environment_context>`。所以只能从
/// 消息文本里抠。抠不到就不传（`None` 仍是合法的「不知道」）。
///
/// 只认「路径形状」的结果：`X:\…` / `/…` / `~/…`。抠到一句中文不算命中 ——
/// 把中文当目录去 `classify` 会登记出一个假项目，比不归类更糟。
pub fn cwd_of(messages: &[Value]) -> Option<String> {
    const PATTERNS: &[&str] = &[
        "工作目录",
        "working directory",
        "Primary working directory",
        "Current working directory",
        "<cwd>",
        "<workspace>",
        "workspace folder",
    ];
    let looks_like_path = |text: &str| {
        let text = text.trim().trim_matches(|ch| matches!(ch, '"' | '\'' | '`' | '。' | '.' | '，' | ','));
        text.starts_with("~/")
            || text.starts_with('/')
            || (text.len() >= 3
                && text.as_bytes()[1] == b':'
                && (text.as_bytes()[2] == b'\\' || text.as_bytes()[2] == b'/'))
    };
    // 只扫最近几条 user 消息的前半段：工作目录是上下文块里的固定信息，
    // 在整段历史摘要里翻等于把 4 万字符读两遍。
    for message in messages.iter().rev().take(3) {
        if message.get("role").and_then(Value::as_str) != Some("user") {
            continue;
        }
        let blocks = match message.get("content") {
            Some(Value::String(text)) => vec![text.clone()],
            Some(Value::Array(items)) => items
                .iter()
                .filter_map(|item| item.get("text").and_then(Value::as_str).map(str::to_string))
                .collect(),
            _ => continue,
        };
        for block in blocks {
            let head: String = block.chars().take(4_000).collect();
            for pattern in PATTERNS {
                let Some(at) = head.find(pattern) else { continue };
                let rest = &head[at + pattern.len()..];
                // 标记后面可能跟着 `:` / `：` / `>` / 空白，逐个跳过
                let rest = rest.trim_start_matches(|ch: char| {
                    matches!(ch, ':' | '：' | '=' | '>' | ' ' | '\t' | '\n' | '\r')
                });
                // 路径到空白或闭合标签为止（Windows 路径可能带空格，所以先试到
                // 行尾：取行内第一个「像完整路径」的片段）
                for candidate in rest.split(|ch: char| matches!(ch, '<' | '\n' | '\r')) {
                    let candidate = candidate.trim();
                    if !looks_like_path(candidate) {
                        continue;
                    }
                    // 带空格的中文句子里，路径通常到第一个空格就结束；
                    // 但 `D:\Program Files\...` 是合法的。取「最后一个不存在的
                    // 空格位置」太猜，这里保守取整段（`classify` 里
                    // `detect_identity` 找不到目录就退回目录名匹配，不会炸）。
                    let path = candidate
                        .trim_end_matches(|ch: char| {
                            matches!(ch, '。' | '.' | ',' | '，' | ';' | '；' | ')' | '）' | '"' | '\'')
                        })
                        .trim();
                    if path.chars().count() >= 3 {
                        return Some(path.to_string());
                    }
                }
            }
        }
    }
    None
}

/// 这一轮的去重键。
///
/// ── 为什么本节内要去重 ──────────────────────────────────────
/// 一条请求可能被客户端重试（网络抖动、用户点两次）。重试的请求体与第一轮几乎
/// 一样，若不去重就会写两条一模一样的记忆。库里有精确 hash 去重（`dedup`），
/// 但它读的是**归一化后的正文**；这里在更前面挡一道，代价只是一个哈希比较。
///
/// 取「用户侧 + 助手侧的开头」而不是全文：全文里常有易变的部分（时间戳、行号），
/// 取开头更稳定。
fn dedupe_key(user: &str, answer: &str) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    user.trim().hash(&mut hasher);
    answer.trim().chars().take(400).collect::<String>().hash(&mut hasher);
    hasher.finish()
}

/// 最近写过的键（带时间戳），用于挡住重试造成的重复写入。
///
/// TTL 10 分钟：超过这个窗口的「同样内容」多半是用户真的又问了一遍（比如
/// 「再说一遍刚才那个」），那时候写第二条是合理的。
const RECENT_TTL: Duration = Duration::from_secs(600);
const RECENT_MAX: usize = 32;

fn recent() -> &'static Mutex<Vec<(u64, Instant)>> {
    static SLOT: OnceLock<Mutex<Vec<(u64, Instant)>>> = OnceLock::new();
    SLOT.get_or_init(|| Mutex::new(Vec::new()))
}

/// 这个键刚写过吗（写过就返回 true，并顺手把过期项清掉）。
fn seen_recently(key: u64) -> bool {
    let Ok(mut guard) = recent().lock() else {
        // 锁中毒：宁可放行一次重复写入，也不要让留存永久失效
        return false;
    };
    let now = Instant::now();
    guard.retain(|(_, stamp)| now.duration_since(*stamp) < RECENT_TTL);
    if guard.iter().any(|(known, _)| *known == key) {
        return true;
    }
    if guard.len() >= RECENT_MAX {
        guard.remove(0);
    }
    guard.push((key, now));
    false
}

/// 留存计数（仪表盘用）。
static RETAIN_COUNT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
static RETAIN_SKIPPED: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

pub fn stats() -> Value {
    use std::sync::atomic::Ordering;
    serde_json::json!({
        "enabled": settings().enabled,
        "count": RETAIN_COUNT.load(Ordering::Relaxed),
        // 「跳过」= 走了留存逻辑但判定不值得记（寒暄 / 太短 / 重复）
        "skipped": RETAIN_SKIPPED.load(Ordering::Relaxed),
    })
}

/// 从一次请求的完整消息列表里提取「这一轮」。
///
/// 返回 `(用户侧, 助手侧)`。两个都可能为空（比如请求里没有 user 消息）。
pub fn turn(messages: &[Value]) -> (String, String) {
    (user_text(messages), assistant_text(messages))
}

/// 把这一轮写进 l1。返回写入的 id（`None` = 没写，原因见 [`stats`]）。
///
/// ── 为什么参数是「消息列表 + 回答文本」而不是整条响应 ────────
/// 响应可能是流式的（SSE），而且协议有三种。把「已聚合好的回答文本」传进来，
/// 这个函数就与协议解耦：Chat / Responses / Anthropic 三条路各自聚合完再调一次。
///
/// `cwd` 用于让写入侧按 git 身份推断项目（`layout::classify`）。这里**不**自己
/// 算项目 slug：那套判据（git remote / 仓库根 / 标题相似度）已经在写入路径里，
/// 在网关这边重实现一遍等于两处判据，迟早会不一致。
pub fn remember_turn(
    messages: &[Value],
    answer: &str,
    cwd: Option<&str>,
    session: Option<&str>,
    interrupted: bool,
    continued: bool,
) -> Option<String> {
    let settings = settings();
    if !settings.enabled {
        return None;
    }
    // ── 工具循环里的中间轮不算「一轮」────────────────────────────
    // 真机事故：一次「动第一批」的任务，Agent 走了二十多次工具调用。每次调用
    // 客户端都发一个新请求，网关每次都当「一轮对话」存下来 —— 一天攒出 30 条
    // 「先看留存写入路径。」「现在把它接进去。」这种旁白，全部落进 l1。
    // 那不是记忆，那是执行日志；而且它们把真正的结论挤到后面，注入时按分数
    // 取前几名，取到的全是旁白。
    //
    // 判据只能放在**响应**侧：请求形状分不出「中间轮」和「最后一轮」——
    // 两者都以 `tool` 消息结尾。响应里有工具调用 = 模型还要接着跑，这一轮
    // 还没结束，不写。真正的收尾那一轮响应里没有工具调用，才会落库。
    if continued {
        use std::sync::atomic::Ordering;
        RETAIN_SKIPPED.fetch_add(1, Ordering::Relaxed);
        return None;
    }
    let user = user_text(messages);
    // ── cwd 传下去才进得了项目目录 ────────────────────────────────
    // 调用方一律传 `None`（网关拿不到客户端的 cwd 字段），所以这里必须自己
    // 从上下文块抠。抠不到就进 `general`，那是正确的降级。
    let cwd: Option<String> = match cwd {
        Some(given) if !given.trim().is_empty() => Some(given.to_string()),
        _ => cwd_of(messages),
    };
    if !worth_keeping(&user, answer, settings.min_chars) {
        use std::sync::atomic::Ordering;
        RETAIN_SKIPPED.fetch_add(1, Ordering::Relaxed);
        return None;
    }
    let key = dedupe_key(&user, answer);
    if seen_recently(key) {
        use std::sync::atomic::Ordering;
        RETAIN_SKIPPED.fetch_add(1, Ordering::Relaxed);
        return None;
    }
    let body = compose_body(&user, answer, interrupted);
    let body = body.chars().take(settings.max_chars).collect::<String>();

    let input = super::WriteInput {
        content: body,
        // 标题由这里给，不能留给写入侧的 `infer_title`：正文第一行恒为
        // `**我**：…`，推断出来就是把标签当标题（真机库 30 条全中）。
        title: Some(infer_title(&user, answer)),
        // l1 流水：网关写的是「这一轮发生了什么」，不是提炼后的结论。
        // 提炼是 distill 的活，那条链已经在跑。
        kind: Some("daily".to_string()),
        layer: Some("l1".to_string()),
        project: None,
        agent: Some("gateway".to_string()),
        tags: Vec::new(),
        importance: None,
        // 文件锚点由写入侧从正文抽（`inject::harvest_paths`），这里不重复做
        files: Vec::new(),
        supersedes: Vec::new(),
        pinned: None,
        starred: None,
        cwd,
        session: session.map(str::to_string),
        created_at: None,
        valid_to: None,
        evidence: Vec::new(),
    };
    match super::write(input) {
        Ok(value) => {
            use std::sync::atomic::Ordering;
            let id = value
                .get("id")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            // 被 hash 去重挡掉（库里已有完全相同的一条）不算「新写入」，
            // 但也不算跳过 —— 它本来就是重复，计数上归到 skipped 更诚实。
            let duplicate = value
                .get("duplicate")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            if duplicate {
                RETAIN_SKIPPED.fetch_add(1, Ordering::Relaxed);
                return None;
            }
            RETAIN_COUNT.fetch_add(1, Ordering::Relaxed);
            crate::server::logging::verbose(
                "[Memory]",
                &format!("请求后留存：已写回 l1（{id}）"),
            );
            Some(id)
        }
        Err(error) => {
            // 留存失败绝不影响已经下发的响应：这里只记一行日志
            crate::server::logging::log(
                "[Memory]",
                &format!("请求后留存失败（不影响本次响应）：{error}"),
            );
            None
        }
    }
}

/// 从请求体里取 session 标识（客户端不一定发，缺省不传）。
///
/// ── 为什么要这个 ────────────────────────────────────────────
/// 同一次对话的若干轮共享 `session`，界面才能把「一次会话的几轮」串起来看。
/// 客户端没有统一约定：OpenAI 官方没有会话字段，兼容实现在 `session_id` /
/// `conversation_id` / `thread_id` 之间各写各的，还有些客户端图省事把会话 id
/// 塞进 `user`。三个都认，取不到就不传（`None` 的语义是「同一次会话未知」，
/// 不是「同一次会话」）。
pub fn session_of(body: &Value) -> Option<String> {
    for key in ["session_id", "conversation_id", "thread_id"] {
        if let Some(value) = body.get(key).and_then(Value::as_str) {
            if !value.trim().is_empty() {
                return Some(value.trim().to_string());
            }
        }
    }
    // OpenAI 兼容客户端常用 `user` 字段带会话标识
    body.get("user")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
}

/// 把这一轮写进 l1 的测试夹具说明见下方测试。
#[cfg(test)]
mod tests {
    use super::*;

    fn messages(user: &str, answer: &str) -> Vec<Value> {
        vec![
            serde_json::json!({ "role": "user", "content": user }),
            serde_json::json!({ "role": "assistant", "content": answer }),
        ]
    }

    #[test]
    fn small_talk_is_not_worth_keeping() {
        // 「好的」不记 —— 逐轮写会把库灌成寒暄流水。本机 5,997 条里 5,931 条
        // 就是这种流水，而注入跑分显示它们一条都过不了门槛。
        assert!(!worth_keeping("继续", "好的，已完成。", 20));
        assert!(!worth_keeping("谢谢", "不客气！", 20));
        assert!(!worth_keeping("", "ok", 20));
    }

    #[test]
    fn verdicts_and_pitfalls_are_worth_keeping() {
        // 有结论词的才留 —— 那是「下次怎么做」的依据
        assert!(worth_keeping(
            "仪表盘打不开",
            "**结论**：根因是索引库的全局 Mutex 被 rebuild 长时间独占。",
            20
        ));
        assert!(worth_keeping(
            "为什么注入不生效",
            "这里有个坑：`already_chat` 扫全部消息找标记，会话里提到过就永远跳过。",
            20
        ));
        // 没有结论词但有具体路径：多半在描述改了什么
        assert!(worth_keeping(
            "改一下注入",
            "已改 desktop-tauri/src-tauri/server/src/server/core/memory/inject.rs",
            20
        ));
    }

    #[test]
    fn long_exchange_counts_even_without_keywords() {
        // 两侧都够长：多半是在讨论具体任务，不是寒暄
        let user = "这".repeat(220);
        let answer = "那".repeat(420);
        assert!(worth_keeping(&user, &answer, 20));
        // 只有一侧长不算：用户贴了一大段日志、助手只回了「收到」
        assert!(!worth_keeping(&user, "收到", 20));
    }

    #[test]
    fn body_shape_matches_imported_entries() {
        // 形状必须与导入器一致（`**我**：… / **助手**：…`），否则检索 / 摘要 /
        // 蒸馏要为「网关写的」和「导入的」分两套解析。
        let body = compose_body("改一下注入", "改好了。", false);
        assert!(body.starts_with("**我**：改一下注入"), "{body}");
        assert!(body.contains("**助手**：改好了。"), "{body}");
        // 正常收尾不加任何判定字段 —— 默认就是成功的
        assert!(!body.contains("未正常结束"), "{body}");

        // 异常收尾才标一句，且是事实陈述而不是判定字段
        let broken = compose_body("改一下注入", "改到一半断了", true);
        assert!(broken.contains("未正常结束"), "{broken}");
    }

    #[test]
    fn assistant_text_skips_tool_call_blocks() {
        // 助手消息的内容块里常有 tool_use —— 那些不是「回答」，不能进记忆正文
        let messages = vec![serde_json::json!({
            "role": "assistant",
            "content": [
                { "type": "text", "text": "我先读一下文件。" },
                { "type": "tool_use", "id": "t1", "name": "Read", "input": { "path": "x" } }
            ]
        })];
        let text = assistant_text(&messages);
        assert_eq!(text, "我先读一下文件。");
    }

    #[test]
    fn user_text_prefers_user_query_over_history_summary() {
        // 与注入共用同一套「哪条 user 消息、哪个内容块」的判据：WorkBuddy 把
        // 整段会话 + 上一轮摘要压在一条 user 消息里，真正的输入只有
        // `<user_query>` 里的那句。
        let messages = vec![serde_json::json!({
            "role": "user",
            "content": [
                { "type": "text", "text": "<conversation_history_summary>上一轮的结论……</conversation_history_summary>" },
                { "type": "text", "text": "<user_query>把注入修好</user_query>" }
            ]
        })];
        let text = user_text(&messages);
        assert!(text.contains("把注入修好"), "{text}");
        assert!(!text.contains("上一轮的结论"), "{text}");
    }

    #[test]
    fn user_text_never_includes_tool_output() {
        // 用户只说了「继续」时，注入会退到工具回显去找线索（那是它该做的），
        // 但留存绝不能把那屏输出写成「用户说了什么」—— 库会变成日志备份。
        let messages = vec![
            serde_json::json!({ "role": "user", "content": "<user_query>继续</user_query>" }),
            serde_json::json!({
                "role": "tool",
                "content": "Command: ls\nagents.rs\nconfig.rs\ninject.rs\n".repeat(50)
            }),
        ];
        let text = user_text(&messages);
        assert_eq!(text.trim(), "继续", "工具回显不该进用户侧：{text}");
    }

    #[test]
    fn task_notifications_are_not_user_words() {
        // 真机库 2026-11-22：两条后台任务通知被写成「**我**：<task-notification>…」，
        // 标题也变成 `<task-notification>`。通知是客户端给模型的控制消息，
        // 不是用户原话；即使剥离标签后还剩 TaskOutput 指令，整轮也不该留存。
        let messages = vec![serde_json::json!({
            "role": "user",
            "content": "<task-notification>\n<task-id>PW8w98</task-id>\n<status>completed</status>\n</task-notification>\nUse the TaskOutput tool to retrieve the full output."
        })];
        assert_eq!(user_text(&messages), "");

        let messages = vec![serde_json::json!({
            "role": "user",
            "content": "<task-notification><status>completed</status></task-notification>\n继续处理旧库里的错误条目"
        })];
        assert_eq!(user_text(&messages), "继续处理旧库里的错误条目");
    }

    #[test]
    fn user_text_never_includes_history_summary() {
        // 真机形态：整条 user 消息里只有摘要块 + system-reminder，没有
        // `<user_query>`（用户这一轮只发了张图 / 只点了继续）。尾部窗口会落在
        // 摘要上 —— 直接写进 `**我**：` 就等于「我上一轮总结过…」的伪记录，
        // 每轮都把上一轮的结论再存一遍。
        let messages = vec![serde_json::json!({
            "role": "user",
            "content": [
                { "type": "text", "text": "<conversation_history_summary>上一轮我说：注入零命中的根因是 already_chat 扫全部消息。收尾：已提交 ca045bc9。</conversation_history_summary>" },
                { "type": "text", "text": "<system-reminder>今天是 2026-11-21，工作目录 D:\\Personal\\Desktop\\agent2api-main。</system-reminder>" }
            ]
        })];
        let text = user_text(&messages);
        assert!(text.is_empty(), "摘要不该被当成「我说的话」：{text}");

        // 真话与摘要混在一起：只留真话
        let mixed = vec![serde_json::json!({
            "role": "user",
            "content": [
                { "type": "text", "text": "<conversation_history_summary>旧结论……</conversation_history_summary>\n继续把留存接上" }
            ]
        })];
        let text = user_text(&mixed);
        assert_eq!(text.trim(), "继续把留存接上", "{text}");
    }

    #[test]
    fn title_is_the_user_words_not_the_label() {
        // 真机事故：库里 30 条留存的标题全是 `**我**：动第一批，第二批和第三批…`。
        // 写入侧 `infer_title` 取正文第一个非空行，而这里第一个非空行恒为
        // `**我**：…` 这个标签 —— 列表页一屏全是同一个前缀，标题列等于废掉。
        let title = infer_title(
            "动第一批，第二批和第三批我还没理解是什么意思，需要你解释",
            "**结论**：第一批只做了 information_score。",
        );
        assert!(!title.contains("**我**"), "标题不能带标签：{title}");
        assert!(title.starts_with("动第一批"), "{title}");

        // 用户只说了「继续」：太短说明不了在谈什么，退到助手第一句
        let title = infer_title("继续", "记忆中枢：两条近似重复改由信息量决定留谁");
        assert_eq!(title, "记忆中枢：两条近似重复改由信息量决定留谁");

        // 两边都短：给一个不丢人的默认值，不要写成空串
        assert_eq!(infer_title("继续", "好的"), "对话留存");
    }

    #[test]
    fn cwd_is_dug_out_of_the_context_block() {
        // 真机事故：留存全部落进 `general/`，一条都没进 `projects/`。原因是
        // `remember_turn` 的 cwd 一直传 None，而写入侧归类是「显式项目名 >
        // cwd 的 git 身份 > general」。OpenAI 协议没有 cwd 字段，客户端把
        // 工作目录写在上下文块里，只能从文本里抠。
        let messages = vec![serde_json::json!({
            "role": "user",
            "content": [
                { "type": "text", "text": "<system-reminder>今天是 2026-11-21，工作目录 D:\\Personal\\Desktop\\agent2api-main。</system-reminder>" },
                { "type": "text", "text": "<user_query>继续</user_query>" }
            ]
        })];
        assert_eq!(
            cwd_of(&messages),
            Some("D:\\Personal\\Desktop\\agent2api-main".to_string())
        );

        // 英文写法 + 正斜杠
        let messages = vec![serde_json::json!({
            "role": "user",
            "content": "<environment_context>working directory: /home/me/project</environment_context>"
        })];
        assert_eq!(cwd_of(&messages), Some("/home/me/project".to_string()));

        // 抠不到就不猜：返回 None 仍会进 general，那是正确的降级
        let messages = vec![serde_json::json!({ "role": "user", "content": "继续" })];
        assert_eq!(cwd_of(&messages), None);

        // 中文不算路径：把一句话当目录去 classify 会登记出假项目
        let messages = vec![serde_json::json!({
            "role": "user",
            "content": "工作目录这块还没配好"
        })];
        assert_eq!(cwd_of(&messages), None);
    }

    #[test]
    fn retried_turn_is_not_written_twice() {
        // 客户端重试会发几乎一样的请求体：不挡就会写两条一模一样的记忆。
        let key = dedupe_key("把注入修好", "**结论**：修好了。");
        assert!(!seen_recently(key), "第一次应该放行");
        assert!(seen_recently(key), "同样的内容第二次必须挡住");
        // 不同内容不受影响
        assert!(!seen_recently(dedupe_key("另一个问题", "另一个回答")));
    }

    #[test]
    fn session_of_picks_client_hints() {
        assert_eq!(
            session_of(&serde_json::json!({ "session_id": "abc" })),
            Some("abc".to_string())
        );
        assert_eq!(
            session_of(&serde_json::json!({ "conversation_id": "c1" })),
            Some("c1".to_string())
        );
        assert_eq!(
            session_of(&serde_json::json!({ "user": "u1" })),
            Some("u1".to_string())
        );
        assert_eq!(session_of(&serde_json::json!({ "session_id": "  " })), None);
        assert_eq!(session_of(&serde_json::json!({})), None);
    }
}