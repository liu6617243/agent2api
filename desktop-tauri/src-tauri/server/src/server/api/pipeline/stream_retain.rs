//! 流式响应的「这一轮」捕获：边下发边攒回答文本，流结束时写回 l1。
//!
//! ── 为什么流式要单独一个捕获器 ──────────────────────────────
//! 非流式那条路（`retain_answer`）手里直接有聚合好的 `body`，取
//! `choices[].message.content` 就完了。流式那条路手里只有一帧一帧的字节，
//! 而且这些字节在 `RecordingStream` 里是**已经转换过的下游协议帧**：
//!
//! ```text
//! 上游 SSE → 协议转换器 → RecordingStream → axum Body
//! ```
//!
//! 也就是说同一个 `RecordingStream` 可能看到 Chat 的 `choices[].delta`、
//! Responses 的 `response.output_text.delta`、Anthropic 的 `content_block_delta`
//! 三种形状。所以捕获器需要一个**方言**参数，只认「正文增量」这一个位置，
//! 不试图解析完整协议 —— 那是转换器的活。
//!
//! ── 为什么不用 `raw` 采集缓冲 ───────────────────────────────
//! `RawCapture` 只在调试模式开着时攒字节，而且攒的是**整条响应**（含
//! `usage`、`id`、保活换行）。用它做留存等于把「关掉调试模式就不记记忆」
//! 变成一条隐藏耦合。这里独立攒一份，只攒正文。
//!
//! ── 上限 ────────────────────────────────────────────────────
//! 一次流式响应可能几十万字符（长文、代码）。留存正文的硬上限是
//! `retain.maxChars`（默认 12,000），所以捕获也按字节封顶
//! （[`CAP_BYTES`]）：到顶后停止追加，但**不停止下发** —— 客户端该收的字节
//! 一个都不少，只是这一轮记忆变短。

use serde_json::Value;

use crate::server::core::memory::retain;
use crate::server::core::protocol;

/// 下游协议方言：决定从哪种增量帧里取回答正文。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Dialect {
    /// `/v1/chat/completions`：`choices[].delta.content`
    Chat,
    /// `/v1/responses`：`{"type":"response.output_text.delta","delta":"…"}`
    Responses,
    /// `/v1/messages`：`{"type":"content_block_delta","delta":{"type":"text_delta",…}}`
    Anthropic,
}

/// 捕获正文的字节上限（约 4 万汉字）。超过就停止追加。
const CAP_BYTES: usize = 120_000;

/// 一次流式响应的留存捕获器。
///
/// 生命周期：在 `RecordingStream` 里以 `Option` 存在，`settle()` 时被 `take()`
/// 走并调用 [`Self::finish`]。所以「恰好写一次」由 `Option::take` 保证，与
/// 记账那边同一个手法 —— 流的 `poll_next` 收尾和 `Drop` 兜底两条路都只会
/// 走到一次 `settle`。
pub struct TurnCapture {
    dialect: Dialect,
    /// **注入前**的消息快照（注入后写回会自我循环，见 `api::chat` 的说明）
    messages: Vec<Value>,
    session: Option<String>,
    /// 跨分片的半行缓冲：TCP 分片不按行对齐
    tail: Vec<u8>,
    /// 攒下来的回答正文
    text: String,
    /// 已经到上限（此后只下发，不攒）
    capped: bool,
    /// 这一段回答里出现过工具调用吗。
    ///
    /// ── 为什么这一位决定「写不写」─────────────────────────────
    /// Agent 类客户端的一次任务会走几十轮工具调用，每轮都是独立的 HTTP 请求。
    /// 若每轮都写一条，库里就是「先看文件。」「现在接进去。」这类执行旁白 ——
    /// 真机一天攒了 30 条，而且按信息量排序时它们排在前面，把真正的结论挤掉。
    ///
    /// 判据为什么在**响应**侧而不是请求侧：请求形状分不出中间轮与收尾轮
    /// （两者都以 `tool` 消息结尾），而「模型这一轮还调了工具」说明它没说完，
    /// 这件事只有在响应里能看到。
    used_tools: bool,
}

impl TurnCapture {
    /// 建一个捕获器。`messages` 为空时返回 `None` —— 没有请求就没有「这一轮」。
    pub fn new(dialect: Dialect, messages: Vec<Value>, session: Option<String>) -> Option<Self> {
        if messages.is_empty() {
            return None;
        }
        Some(Self {
            dialect,
            messages,
            session,
            tail: Vec::new(),
            text: String::new(),
            capped: false,
            used_tools: false,
        })
    }

    /// 吃一段刚下发的字节，把里面的正文增量攒起来。
    ///
    /// 只处理到最后一个 `\n` 为止，剩下的半行留到下一次 —— 与
    /// `upstream::sse::ReasoningCoalescer` 同一个手法。切行只在 ASCII 的
    /// `\n` 上发生，所以切出来的每一行都是完整的 UTF-8 字节序列
    /// （多字节字符可能被分片切开，但不会跨 `\n` 边界）。
    pub fn push(&mut self, chunk: &[u8]) {
        // 到上限后**仍然**要继续解析：`used_tools` 可能出现在正文之后
        // （先给一大段解释、再调工具去验证）。若这里直接 return，长回答型的
        // 中间轮就会被判成「收尾轮」写进库 —— 正是要挡掉的那类旁白。
        // 所以只停止攒正文（由 `absorb` 判断），不停止扫描。
        self.tail.extend_from_slice(chunk);
        let mut start = 0usize;
        while let Some(offset) = self.tail[start..].iter().position(|byte| *byte == b'\n') {
            let end = start + offset;
            let line = String::from_utf8_lossy(&self.tail[start..end]).into_owned();
            self.absorb(&line);
            start = end + 1;
        }
        if start > 0 {
            self.tail.drain(..start);
        }
        // ── 半行缓冲也要有边界 ──────────────────────────────────
        // 正常 SSE 单帧是几百字节到几十 KB。尾缓冲超过 CAP_BYTES 还没等到换行，
        // 说明这不是一条正常的数据行（上游发了巨型 base64 图片帧、或者协议被
        // 中间设备改坏了）。这里直接丢掉，只保留最后一小段 —— 不丢的话，一条
        // 坏流能让这个 Vec 长到内存爆掉，而它挂在每个活跃请求上。
        if self.tail.len() > CAP_BYTES {
            let keep = 8 * 1024;
            let drop_to = self.tail.len() - keep;
            self.tail.drain(..drop_to);
        }
    }

    /// 处理一行下游 SSE（含 `event:` 行与空行）。
    ///
    /// `event:` 行一律跳过：三种方言的 JSON 里都自带 `type` 字段，比行首的
    /// event 名更可靠（有的客户端/网关会省略 event 行）。
    fn absorb(&mut self, line: &str) {
        let line = line.trim_end_matches('\r');
        let Some(rest) = line.strip_prefix("data:") else {
            return;
        };
        let data = rest.trim();
        if data.is_empty() || data == "[DONE]" {
            return;
        }
        let Ok(value) = serde_json::from_str::<Value>(data) else {
            // 非法 JSON 不猜：这一行要么是心跳、要么是上游的私有帧。
            // 记错一条记忆的代价远大于少记一句。
            return;
        };
        let delta = match self.dialect {
            Dialect::Chat => chat_delta(&value),
            Dialect::Responses => responses_delta(&value),
            Dialect::Anthropic => anthropic_delta(&value),
        };
        // 工具调用标记与正文增量是**并列**的两件事：一帧可能只带工具调用
        // （`delta.tool_calls`），也可能只带正文。所以先独立判一次，再看正文。
        if !self.used_tools && has_tool_call(self.dialect, &value) {
            self.used_tools = true;
        }
        if let Some(delta) = delta {
            if self.capped {
                return;
            }
            self.text.push_str(&delta);
            if self.text.len() >= CAP_BYTES {
                self.capped = true;
            }
        }
    }

    /// 这一段回答里模型是否调用过工具（`true` = 不是收尾轮，不该写成记忆）。
    pub fn used_tools(&self) -> bool {
        self.used_tools
    }

    /// 已攒到的正文（测试与日志用）。
    pub fn text(&self) -> &str {
        &self.text
    }

    /// 流结束：把这一轮交给留存。`interrupted` = 流没完整跑完就被丢弃。
    ///
    /// ── 为什么要 `block_in_place` ───────────────────────────────
    /// 写入要读 hash 去重集合（`store::snapshot()` 冷启动要解析 55 MB 的
    /// `.md`），几百毫秒。这里的位置是流的收尾点（`poll_next` 返回 None 或
    /// `Drop`），已经在 async 上下文里了：直接在 worker 上阻塞会让同一线程上
    /// 的其它请求一起排队。`in_place` 在多线程运行时用 `block_in_place` 让出
    /// worker，在没有运行时 / current_thread 时退回直接调用（测试就是这种）。
    pub fn finish(mut self, interrupted: bool) {
        // ── 最后一帧可能没有换行 ────────────────────────────────
        // `push` 只处理到最后一个 `\n`。若上游最后一帧是 `data: {...}` 后面直接
        // EOF（少数实现不在末尾补换行），那一句会永远留在 `tail` 里丢掉 ——
        // 而「最后一句」恰恰常常是结论。这里补一次收尾解析。
        if !self.capped && !self.tail.is_empty() {
            let line = String::from_utf8_lossy(&self.tail).into_owned();
            self.absorb(&line);
            self.tail.clear();
        }
        if self.text.trim().is_empty() {
            // 流里一帧正文都没取到（工具调用型回答、纯推理回答）：
            // 写下去就是一条只有「用户说了什么」的空记忆，没有价值。
            return;
        }
        let messages = self.messages;
        let session = self.session;
        let text = self.text;
        let continued = self.used_tools;
        crate::server::core::blocking::in_place(|| {
            retain::remember_turn(&messages, &text, None, session.as_deref(), interrupted, continued);
        });
    }
}

/// 这一帧是不是「模型要调工具」的信号。
///
/// ── 三种方言各自的形状 ──────────────────────────────────────
/// | 协议 | 工具调用出现在哪 |
/// |---|---|
/// | Chat | `choices[].delta.tool_calls[]`（首个分片带 `index` / `id` / `name`） |
/// | Responses | `response.output_item.added` 且 `item.type == "function_call"` |
/// | Anthropic | `content_block_start` 且 `content_block.type == "tool_use"` |
///
/// 只认「开始」类事件就够：一次工具调用必然先出现这些帧。不做「参数是否收完」
/// 那种判定 —— 参数在 `input_json_delta` 里逐片拼，拼完才完整，而「有工具
/// 调用」这件事在第一个分片上就已经成立。
fn has_tool_call(dialect: &Dialect, value: &Value) -> bool {
    match dialect {
        Dialect::Chat => value
            .get("choices")
            .and_then(Value::as_array)
            .map(|choices| {
                choices.iter().any(|choice| {
                    choice
                        .get("delta")
                        .and_then(|delta| delta.get("tool_calls"))
                        .and_then(Value::as_array)
                        .map(|calls| !calls.is_empty())
                        .unwrap_or(false)
                })
            })
            .unwrap_or(false),
        Dialect::Responses => {
            let kind = value.get("type").and_then(Value::as_str).unwrap_or("");
            if kind == "response.output_item.added" || kind == "response.output_item.done" {
                return value
                    .get("item")
                    .and_then(|item| item.get("type"))
                    .and_then(Value::as_str)
                    .map(|kind| matches!(kind, "function_call" | "computer_call" | "custom_tool_call"))
                    .unwrap_or(false);
            }
            false
        }
        Dialect::Anthropic => {
            if value.get("type").and_then(Value::as_str) != Some("content_block_start") {
                return false;
            }
            value
                .get("content_block")
                .and_then(|block| block.get("type"))
                .and_then(Value::as_str)
                .map(|kind| kind == "tool_use" || kind == "server_tool_use")
                .unwrap_or(false)
        }
    }
}

/// Chat 方言：`choices[0].delta.content`。
///
/// 跳过 `reasoning_content` / `reasoning`（思维链不是回答）与 `tool_calls`
/// （工具调用是过程，不是结论）—— 与 `retain::worth_keeping` 的口径一致。
fn chat_delta(value: &Value) -> Option<String> {
    let delta = value.get("choices").and_then(Value::as_array)?.first()?.get("delta")?;
    let text = protocol::content_text(delta.get("content")?);
    if text.is_empty() {
        None
    } else {
        Some(text)
    }
}

/// Responses 方言：只认 `response.output_text.delta`。
///
/// `response.reasoning_summary_text.delta` 一类同样叫 `delta`，但那是摘要不是回答。
fn responses_delta(value: &Value) -> Option<String> {
    if value.get("type").and_then(Value::as_str)? != "response.output_text.delta" {
        return None;
    }
    let text = value.get("delta").and_then(Value::as_str)?;
    if text.is_empty() {
        None
    } else {
        Some(text.to_string())
    }
}

/// Anthropic 方言：只认 `content_block_delta` 里的 `text_delta`。
///
/// `thinking_delta` / `input_json_delta` 都不算回答。
fn anthropic_delta(value: &Value) -> Option<String> {
    if value.get("type").and_then(Value::as_str)? != "content_block_delta" {
        return None;
    }
    let delta = value.get("delta")?;
    if delta.get("type").and_then(Value::as_str)? != "text_delta" {
        return None;
    }
    let text = delta.get("text").and_then(Value::as_str)?;
    if text.is_empty() {
        None
    } else {
        Some(text.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn capture(dialect: Dialect) -> TurnCapture {
        TurnCapture::new(
            dialect,
            vec![serde_json::json!({ "role": "user", "content": "改注入" })],
            None,
        )
        .expect("非空消息应当能建捕获器")
    }

    #[test]
    fn chat_delta_skips_reasoning_and_tool_calls() {
        let mut turn = capture(Dialect::Chat);
        turn.push(b"data: {\"choices\":[{\"index\":0,\"delta\":{\"reasoning_content\":\"think\"}}]}\n\n");
        assert_eq!(turn.text(), "", "思维链不是回答");
        turn.push(b"data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"verdict\"}}]}\n\n");
        assert_eq!(turn.text(), "verdict");
        turn.push(b"data: {\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"id\":\"t1\"}]}}]}\n\n");
        assert_eq!(turn.text(), "verdict", "工具调用不是回答");
        turn.push(b"data: [DONE]\n\n");
        assert_eq!(turn.text(), "verdict", "收尾帧不该被当成正文");
    }

    #[test]
    fn frames_split_across_chunks_are_reassembled() {
        // TCP 分片不按行对齐：真实上游常把一个 data 行切成两半
        let mut turn = capture(Dialect::Chat);
        turn.push(b"data: {\"choices\":[{\"delta\":{\"content\":\"hel");
        assert_eq!(turn.text(), "", "半行不能解码");
        turn.push(b"lo\"}}]}\n\n");
        assert_eq!(turn.text(), "hello");
    }

    #[test]
    fn multi_byte_char_split_across_chunks_survives() {
        // 汉字被切成两半落在两个分片里：整行拼好之后再解码，不能出现 U+FFFD
        let full = "结论".as_bytes();
        let mut turn = capture(Dialect::Chat);
        let mut line = b"data: {\"choices\":[{\"delta\":{\"content\":\"".to_vec();
        line.extend_from_slice(&full[..4]);
        turn.push(&line);
        assert_eq!(turn.text(), "");
        let mut rest = full[4..].to_vec();
        rest.extend_from_slice(b"\"}}]}\n\n");
        turn.push(&rest);
        assert_eq!(turn.text(), "结论");
        assert!(!turn.text().contains('\u{fffd}'));
    }

    #[test]
    fn responses_only_takes_output_text() {
        let mut turn = capture(Dialect::Responses);
        turn.push(b"data: {\"type\":\"response.reasoning_summary_text.delta\",\"delta\":\"r\"}\n\n");
        assert_eq!(turn.text(), "", "推理摘要不是回答");
        turn.push(b"data: {\"type\":\"response.output_text.delta\",\"delta\":\"done\"}\n\n");
        assert_eq!(turn.text(), "done");
    }

    #[test]
    fn anthropic_only_takes_text_delta() {
        let mut turn = capture(Dialect::Anthropic);
        turn.push(b"data: {\"type\":\"content_block_delta\",\"delta\":{\"type\":\"thinking_delta\",\"thinking\":\"hmm\"}}\n\n");
        assert_eq!(turn.text(), "", "thinking 不是回答");
        turn.push(b"data: {\"type\":\"content_block_delta\",\"delta\":{\"type\":\"text_delta\",\"text\":\"verdict\"}}\n\n");
        assert_eq!(turn.text(), "verdict");
        turn.push(b"data: {\"type\":\"content_block_delta\",\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"{}\"}}\n\n");
        assert_eq!(turn.text(), "verdict", "工具参数不是回答");
    }

    #[test]
    fn event_lines_and_keepalives_are_ignored() {
        let mut turn = capture(Dialect::Chat);
        turn.push(b"event: ping\n\n");
        turn.push(b": keep-alive\n\n");
        turn.push(b"data: {not json}\n\n");
        assert_eq!(turn.text(), "");
    }

    #[test]
    fn last_frame_without_trailing_newline_is_absorbed() {
        // 少数上游不在最后一帧补换行。`push` 只处理到最后一个 `\n`，
        // 那一句会留在 tail 里 —— 而「最后一句」常常正是结论。
        // 这里只验收尾解析这一步（`finish` 会真的去写记忆，不适合在单测里跑）。
        let mut turn = TurnCapture::new(
            Dialect::Chat,
            vec![serde_json::json!({ "role": "user", "content": "改完了吗" })],
            None,
        )
        .expect("有请求就该有捕获");
        turn.push("data: {\"choices\":[{\"delta\":{\"content\":\"根因\"}}]}\n".as_bytes());
        turn.push(
            "data: {\"choices\":[{\"delta\":{\"content\":\"是标记误判\"}}]}".as_bytes(),
        );
        assert_eq!(turn.text(), "根因", "最后一句还在 tail 里等着");

        // finish 的收尾这一段：把 tail 当最后一行吃掉
        if !turn.capped && !turn.tail.is_empty() {
            let line = String::from_utf8_lossy(&turn.tail).into_owned();
            turn.absorb(&line);
            turn.tail.clear();
        }
        assert_eq!(turn.text(), "根因是标记误判");
        assert!(turn.tail.is_empty());
    }

    #[test]
    fn oversized_tail_does_not_grow_without_bound() {
        // 上游发巨型 base64 帧、或协议被改坏：尾缓冲不能无限长。
        let mut turn = TurnCapture::new(
            Dialect::Chat,
            vec![serde_json::json!({ "role": "user", "content": "看图" })],
            None,
        )
        .expect("有请求就该有捕获");
        // 喂 300 KB 无换行字节
        for _ in 0..30 {
            turn.push(&vec![b'x'; 10 * 1024]);
        }
        assert!(
            turn.tail.len() <= CAP_BYTES,
            "尾缓冲必须有上限，实际 {} 字节",
            turn.tail.len()
        );
    }

    #[test]
    fn tool_call_frames_mark_the_turn_as_continued() {
        // 真机事故：一次任务走二十多轮工具调用，每轮都写成一条 l1，一天攒
        // 30 条「先看文件。」这类执行旁白。判据在响应侧：这一轮还调了工具，
        // 说明模型没说完，不该写成记忆。
        let mut turn = capture(Dialect::Chat);
        turn.push(b"data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_1\",\"function\":{\"name\":\"Bash\"}}]}}]}\n\n");
        assert!(turn.used_tools(), "Chat 的 delta.tool_calls 必须被认出来");

        let mut turn = capture(Dialect::Responses);
        turn.push(b"data: {\"type\":\"response.output_item.added\",\"item\":{\"type\":\"function_call\"}}\n\n");
        assert!(turn.used_tools(), "Responses 的 function_call 必须被认出来");

        let mut turn = capture(Dialect::Anthropic);
        turn.push(b"data: {\"type\":\"content_block_start\",\"content_block\":{\"type\":\"tool_use\",\"id\":\"tu_1\"}}\n\n");
        assert!(turn.used_tools(), "Anthropic 的 tool_use 必须被认出来");
    }

    #[test]
    fn plain_text_answer_is_not_marked_as_continued() {
        // 收尾轮：模型只说了话、没再调工具 —— 这种才该写进记忆。
        let mut turn = capture(Dialect::Chat);
        turn.push(b"data: {\"choices\":[{\"delta\":{\"content\":\"**结论**：修好了。\"}}]}\n\n");
        assert!(!turn.used_tools());

        // Responses 的推理项不能误判成工具调用
        let mut turn = capture(Dialect::Responses);
        turn.push(b"data: {\"type\":\"response.output_item.added\",\"item\":{\"type\":\"reasoning\"}}\n\n");
        assert!(!turn.used_tools());

        // Anthropic 的 thinking 块同理
        let mut turn = capture(Dialect::Anthropic);
        turn.push(b"data: {\"type\":\"content_block_start\",\"content_block\":{\"type\":\"thinking\"}}\n\n");
        assert!(!turn.used_tools());
    }

    #[test]
    fn tool_call_after_a_long_answer_still_counts() {
        // 先给一大段解释、再调工具去验证：这是典型的中间轮。若「攒到上限就
        // 停止扫描」，这一帧的 tool_calls 会被漏掉，长回答型的中间轮就会被
        // 当成收尾轮写进库 —— 正是要挡掉的那类旁白。
        let mut turn = capture(Dialect::Chat);
        let big = "x".repeat(200_000);
        turn.push(format!("data: {{\"choices\":[{{\"delta\":{{\"content\":\"{big}\"}}}}]}}\n\n").as_bytes());
        assert!(!turn.used_tools());
        turn.push(b"data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0}]}}]}\n\n");
        assert!(turn.used_tools(), "正文封顶之后仍要看后面的工具调用帧");
    }

    #[test]
    fn empty_messages_give_no_capture() {
        assert!(TurnCapture::new(Dialect::Chat, Vec::new(), None).is_none());
    }
}