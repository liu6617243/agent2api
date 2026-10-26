//! 流式帧的网关侧消费者。
//!
//! ── 这个类型解决什么 ────────────────────────────────────────
//! `ForwardOutcome::Stream` 要求一个**活的** `Stream`，而插件的字节在另一个
//! 进程里。跨进程把它拉平之后，网关这侧拿到的是「一条无界通道 + 一个额度
//! 计数器」，本类型把那两样重新装成一条 `Stream` —— 上层（`chat.rs` 的
//! `RecordingStream`、协议翻译、续写）看到的形状与内置家完全一致。
//!
//! ── 背压在这里发生 ──────────────────────────────────────────
//! 插件侧的窗口是 [`proto::DEFAULT_WINDOW_BYTES`]，每发一个 chunk 就
//! `forget()` 掉等量额度（见 SDK 的 `StreamSink`）。额度只有网关发
//! `credit{n}` 才补回来。所以本类型**每消费一个 chunk 就归还等量额度**：
//! 客户端读得慢 → 上层不 poll 我们 → 我们不归还 → 插件停在 `send` 上。
//! 这条链断了的话症状是「插件把整段回答缓冲进自己的内存」，背压形同虚设。
//!
//! ── 为什么收尾要合成错误帧 ──────────────────────────────────
//! 客户端此时已经收到 HTTP 200 与部分内容，唯一能告诉它「这条流是失败的」
//! 的办法就是在流里写一条错误帧（内置家的 `ForwardStream` 是同一处置）。
//! 插件自己可能已经写过一条 —— 所以合成前先看尾巴：末尾已经有 `[DONE]`
//! 就不再补，避免客户端收到两个终止信号。

use std::collections::VecDeque;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use bytes::Bytes;
use futures::Stream;
use serde_json::json;

use agent2api_provider_protocol as proto;

use crate::server::core::upstream::usage::RequestTelemetry;
use crate::server::logging;

use super::{Host, StreamEvent};

/// 判定「插件已经自己收尾」的窗口大小。
///
/// 只留尾巴而不是整份响应：`[DONE]` 总是最后几帧之一，而留整份会让每条流
/// 多占一份与响应体等大的内存（那正是流式要避免的）。
const TAIL_BYTES: usize = 64;

/// 一条插件流在网关侧的形态。
pub(crate) struct RpcStream {
    /// 宿主（Drop 时要发 abort / 清登记）。
    host: Arc<Host>,
    /// 本次调用的 RPC id。
    id: i64,
    /// 插件的 chunk / 收尾事件。
    events: tokio::sync::mpsc::UnboundedReceiver<StreamEvent>,
    /// 归还额度的发送端（见模块头）。通道关闭 = 本流结束，泵任务自行退出。
    credit: tokio::sync::mpsc::UnboundedSender<usize>,
    /// 待下发的合成帧（错误帧 / 补的 `[DONE]`）。
    pending: VecDeque<Bytes>,
    /// 已下发字节的尾巴（判「插件是否已经自己收尾」）。
    tail: Vec<u8>,
    /// 已经排好收尾帧，下一次 poll 直接返回 `None`。
    ended: bool,
    /// usage / 首响旁路槽。
    telemetry: Arc<RequestTelemetry>,
}

impl RpcStream {
    /// 建一条流，并起一个**额度泵**。
    ///
    /// ── 为什么额度要单独一个任务 ────────────────────────────────
    /// `poll_next` 是同步的，而归还额度要写 stdin（异步）。在 `poll_next`
    /// 里 `tokio::spawn` 一个写任务会让每个 chunk 起一个任务（高频流上是
    /// 每秒几十个），而泵任务是一条流一个：它阻塞在 `recv` 上，来一个还一个。
    ///
    /// `pending` 是「已经从通道里取出来、但还没下发」的正文（见
    /// [`Self::with_pending`] 的说明）。
    pub(crate) fn with_pending(
        host: Arc<Host>,
        id: i64,
        events: tokio::sync::mpsc::UnboundedReceiver<StreamEvent>,
        pending: Option<Vec<u8>>,
        telemetry: Arc<RequestTelemetry>,
    ) -> Self {
        let (credit, mut credit_rx) = tokio::sync::mpsc::unbounded_channel::<usize>();
        let pump_host = host.clone();
        tokio::spawn(async move {
            while let Some(n) = credit_rx.recv().await {
                pump_host.credit(id, n).await;
            }
        });
        let mut queue = VecDeque::new();
        if let Some(bytes) = pending {
            queue.push_back(Bytes::from(bytes));
        }
        Self {
            host,
            id,
            events,
            credit,
            pending: queue,
            tail: Vec::new(),
            ended: false,
            telemetry,
        }
    }

    /// 记下已下发字节的尾巴。
    fn remember(&mut self, bytes: &[u8]) {
        self.tail.extend_from_slice(bytes);
        if self.tail.len() > TAIL_BYTES {
            let cut = self.tail.len() - TAIL_BYTES;
            self.tail.drain(..cut);
        }
    }

    /// 插件是否已经写过终止帧。
    fn tail_has_done(&self) -> bool {
        self.tail.windows(6).any(|window| window == b"[DONE]")
    }

    /// 排上「错误帧 + [DONE]」收尾。
    fn finish_with_error(&mut self, message: &str) {
        self.ended = true;
        logging::console_line("[Model]", &format!("❌ {message}"));
        self.telemetry.note_error(message);
        if self.tail_has_done() {
            // 插件自己已经收尾过了：再写一条会让客户端收到两个终止信号
            return;
        }
        self.pending.push_back(crate::server::core::upstream::sse::sse_frame(&json!({
            "error": {
                "message": message,
                "type": "proxy_error",
            }
        })));
        self.pending.push_back(Bytes::from_static(b"data: [DONE]\n\n"));
    }
}

impl Stream for RpcStream {
    type Item = Result<Bytes, std::io::Error>;

    fn poll_next(mut self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.as_mut().get_mut();
        loop {
            if let Some(frame) = this.pending.pop_front() {
                return Poll::Ready(Some(Ok(frame)));
            }
            if this.ended {
                return Poll::Ready(None);
            }
            match this.events.poll_recv(context) {
                Poll::Pending => return Poll::Pending,
                // 通道关了却没有 `Done`：宿主被清表（进程没了 / 主动关停）。
                // 不合成错误的话，客户端会把截断当成正常结束。
                Poll::Ready(None) => this.finish_with_error("插件流在收尾信号之前中断"),
                // 重复的 head（插件协议违规）：消费者只认第一条，后续丢弃。
                // 不报错是因为它不影响正文 —— 一条多余的控制帧不该毁掉整条流。
                Poll::Ready(Some(StreamEvent::Head(_))) => {}
                Poll::Ready(Some(StreamEvent::Chunk(bytes))) => {
                    // 归还等量额度：载荷 + 帧开销（与插件侧计费口径逐字一致，
                    // 差一个常量就会让窗口缓慢泄漏或溢出）
                    let _ = this.credit.send(bytes.len() + proto::FRAME_OVERHEAD_BYTES);
                    this.remember(&bytes);
                    this.telemetry.note_first_frame();
                    return Poll::Ready(Some(Ok(Bytes::from(bytes))));
                }
                Poll::Ready(Some(StreamEvent::Failed(message))) => {
                    this.finish_with_error(&message);
                }
                Poll::Ready(Some(StreamEvent::Done(Ok(_)))) => {
                    // 插件正常结束。它本该自己写 `[DONE]`；漏了就在这里补上
                    // —— 少一个终止帧会让客户端一直等（连接挂着不关）。
                    if !this.tail_has_done() {
                        this.pending.push_back(Bytes::from_static(b"data: [DONE]\n\n"));
                    }
                    this.ended = true;
                }
                Poll::Ready(Some(StreamEvent::Done(Err(error)))) => {
                    this.finish_with_error(&error.message);
                }
            }
        }
    }
}

impl Drop for RpcStream {
    fn drop(&mut self) {
        // 先清登记：插件随后到达的最终应答会落进「没有对应流」的分支被丢弃。
        self.host.close_stream(self.id);
        // 正常收尾（`ended`）不必取消 —— 插件那边已经结束了，多发一个 abort
        // 只会让「刚好在最后一帧之后到达」的取消被误判成客户端断开。
        if !self.ended {
            let host = self.host.clone();
            let id = self.id;
            tokio::spawn(async move { host.abort(id).await });
        }
    }
}
