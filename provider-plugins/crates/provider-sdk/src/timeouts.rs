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
    /// ── 必须严格大于空闲守卫的预算 ──────────────────────────────
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
    /// 因此这里给到守卫预算的**两倍**：正常僵死由守卫先判（文案可读、且
    /// 收尾时已到手的字节不丢），传输层只是「守卫自己也没能唤醒」时的
    /// 最后兜底。默认配置下 guard 300 秒 / 后备 600 秒，正是历史上这份
    /// 代码各处注释写的「`egress` 默认 read_timeout 是 600 秒」那个形态。
    pub fn read_timeout_backstop_ms(&self) -> u64 {
        let guard = self.headers_ms().max(self.stream_idle_ms());
        guard.saturating_mul(2)
    }
}

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
    use super::Timeouts;

    /// 传输层后备**必须**严格大于空闲守卫的预算。
    ///
    /// 相等就是赛跑，而 reqwest 的 `ReadTimeoutBody` 先 poll 定时器、后 poll
    /// 上游，临界点上必然抢先 —— 用户看到的是
    /// `error decoding response body → … → operation timed out`（英文堆栈、
    /// 且丢掉这一轮已缓冲的帧），而不是守卫那句可读的「流式响应空闲超时」。
    /// 这条断言就是那次线上问题的墓碑。
    #[test]
    fn backstop_outlives_idle_guard() {
        for (headers, idle) in [(300, 300), (300, 60), (60, 300), (1, 1), (900, 120)] {
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

    /// 默认配置下后备是 600 秒 —— 与「给 SSE 长连接留的」那句老注释一致。
    #[test]
    fn default_backstop_is_ten_minutes() {
        assert_eq!(Timeouts::default().read_timeout_backstop_ms(), 600_000);
    }
}
