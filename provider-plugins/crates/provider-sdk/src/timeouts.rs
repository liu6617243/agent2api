use std::future::Future;

use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq, Hash)]
#[serde(default, rename_all = "camelCase")]
pub struct Timeouts {
    pub connect_seconds: i64,
    pub headers_seconds: i64,
    pub stream_idle_seconds: i64,
    pub body_seconds: i64,
}

impl Default for Timeouts {
    fn default() -> Self {
        Self {
            connect_seconds: 30,
            headers_seconds: 300,
            stream_idle_seconds: 300,
            body_seconds: 300,
        }
    }
}

impl Timeouts {
    fn ms(seconds: i64) -> u64 {
        (seconds.max(1) as u64).saturating_mul(1000)
    }

    pub fn connect_ms(&self) -> u64 {
        Self::ms(self.connect_seconds)
    }

    pub fn headers_ms(&self) -> u64 {
        Self::ms(self.headers_seconds)
    }

    pub fn stream_idle_ms(&self) -> u64 {
        Self::ms(self.stream_idle_seconds)
    }

    pub fn body_ms(&self) -> u64 {
        Self::ms(self.body_seconds)
    }

    /// 传输层「读超时」后备值（reqwest `read_timeout`，**逐次读重置**）。
    ///
    /// ── 为什么必须比空闲守卫晚一截 ──────────────────────────────
    /// 这条超时和 [`crate::stall::idle_guard`] 的读空闲守卫**量同一件事**
    /// （都是「多久没读到新字节」），一旦两者相等就是赛跑：
    ///
    ///   * `idle_guard` 是**先 poll 上游、后 poll 定时器** —— 上游刚吐出
    ///     数据、但定时器先就绪的那一轮，仍会走「收到数据」分支（不误杀）；
    ///   * reqwest 的 `ReadTimeoutBody` 反过来**先 poll 定时器、后 poll 上游**，
    ///     `Poll::Ready(TimedOut)` 立即返回，把这一轮已经缓冲的帧一起丢掉。
    ///
    /// 两者同值时 reqwest 在临界点必然抢先，用户看到的是
    /// `error decoding response body → request or response body error →
    /// operation timed out` —— 既丢了数据（可能截断在半个 chunk 上），
    /// 又把守卫那条可读的「流式响应空闲超时(N秒)」盖成一句英文堆栈。
    ///
    /// ── 为什么是「加一截」而不是「乘 2」─────────────────────────
    /// 不是每个 `stream_client` 的调用方都装了守卫（catpaw / kiro / kuku /
    /// astudio 就直接读 `bytes_stream`），**对它们这条后备是唯一的僵死
    /// 探测**。翻倍会把它们的「挂住」从 5 分钟拖到 10 分钟，是实打实的退步。
    /// 加 [`BACKSTOP_MARGIN_MS`] 就够赢下赛跑了 —— 竞态只差一个调度粒度，
    /// 60 秒的余量对 tokio 定时器而言等同于无穷。
    pub fn read_timeout_backstop_ms(&self) -> u64 {
        let guard = self.headers_ms().max(self.stream_idle_ms());
        guard.saturating_add(BACKSTOP_MARGIN_MS)
    }
}

/// 传输层后备比空闲守卫晚多少（见 [`Timeouts::read_timeout_backstop_ms`]）。
pub const BACKSTOP_MARGIN_MS: u64 = 60_000;

tokio::task_local! {
    static CURRENT: Timeouts;
}

/// 每次 RPC 独立持有超时快照；旧网关不传时沿用默认值。
pub fn current() -> Timeouts {
    CURRENT.try_with(|timeouts| *timeouts).unwrap_or_default()
}

pub(crate) async fn scope<F: Future>(timeouts: Timeouts, future: F) -> F::Output {
    CURRENT.scope(timeouts, future).await
}

#[cfg(test)]
mod tests {
    use super::{Timeouts, BACKSTOP_MARGIN_MS};

    /// 传输层后备**必须**严格大于空闲守卫的预算。
    ///
    /// 相等就是赛跑，而 reqwest 的 `ReadTimeoutBody` 先 poll 定时器、后 poll
    /// 上游，临界点上必然抢先 —— 用户看到的是
    /// `error decoding response body → … → operation timed out`（英文堆栈、
    /// 且丢掉这一轮已缓冲的帧），而不是守卫那句可读的「流式响应空闲超时」。
    /// 这条断言就是那次线上问题的墓碑。
    #[test]
    fn backstop_outlives_idle_guard() {
        for (headers, idle) in [(300, 300), (300, 60), (60, 300), (1, 1), (900, 120), (5, 5)] {
            let timeouts = Timeouts {
                connect_seconds: 30,
                headers_seconds: headers,
                stream_idle_seconds: idle,
                body_seconds: 300,
            };
            let guard = timeouts.headers_ms().max(timeouts.stream_idle_ms());
            assert!(
                timeouts.read_timeout_backstop_ms() > guard,
                "后备读超时必须大于空闲守卫（headers={headers}s idle={idle}s）"
            );
        }
    }

    /// 后备只比守卫晚一个固定余量 —— 不能翻倍。
    ///
    /// 没装守卫的几家（catpaw / kiro / kuku / astudio）只能靠这条后备发现
    /// 「上游挂住了」，余量给太大等于让用户多干等一倍时间。
    #[test]
    fn backstop_margin_is_a_sixty_second_step() {
        let timeouts = Timeouts::default();
        let guard = timeouts.headers_ms().max(timeouts.stream_idle_ms());
        assert_eq!(timeouts.read_timeout_backstop_ms(), guard + BACKSTOP_MARGIN_MS);
        // 默认 300 秒守卫 → 360 秒后备（不是 600）
        assert_eq!(timeouts.read_timeout_backstop_ms(), 360_000);
    }
}
