//! 记忆模块里「在可能是异步上下文里等一个 future」的运行时适配。
//!
//! ── 为什么需要这一层 ────────────────────────────────────────
//! 记忆的写入、检索、调度全部跑在**同步**代码里（`spawn_blocking` 内部、
//! MCP 分发线程、调度线程）。而「调模型」「连 WebDAV」本质是异步 IO。
//! 直接在同步函数里 `.await` 编不过，硬等又会把 tokio worker 卡死。
//!
//! 三种运行时有三种等法：
//!   · 没有运行时（调度线程、单元测试）→ 自己起一个 current_thread 跑完；
//!   · 多线程运行时（网关主路径）→ `block_in_place` 让出 worker 再阻塞；
//!   · current_thread 运行时 → 阻塞会把唯一的 worker 卡死（future 永远没机会
//!     推进），这时候只能报错，不能硬等。
//!
//! 第三种是唯一会「硬等出死锁」的情形，所以明确报错而不是等下去。
//!
//! 为什么不是「所有地方都起一个 runtime」：在 tokio worker 里嵌套建 runtime
//! 是被官方明确劝阻的（会绕过任务调度、超时与取消语义），而记忆的调用方
//! 大多已经在 async 上下文里了。

use std::future::Future;

use tokio::runtime::{Builder, Handle, RuntimeFlavor};

/// 等待失败的三种原因。
pub(crate) enum Failure {
    /// 超过了自己设的时限
    Timeout,
    /// 连接层失败（DNS / TLS / 代理 / 连接被拒）
    Transport(reqwest::Error),
    /// 当前运行时不能安全地阻塞等待
    NoRuntime,
}

impl Failure {
    pub(crate) fn message(&self) -> String {
        match self {
            Self::Timeout => "请求超时".to_string(),
            Self::Transport(error) => format!("网络请求失败：{error}"),
            Self::NoRuntime => "当前运行时不能安全阻塞等待（current_thread 运行时）".to_string(),
        }
    }
}

/// 在一个**可能已处于异步上下文**的同步函数里等一个 future。
pub(crate) fn block_on<F, T>(future: F) -> Result<T, Failure>
where
    F: Future<Output = Result<T, reqwest::Error>>,
{
    let Ok(handle) = Handle::try_current() else {
        let runtime = Builder::new_current_thread().enable_all().build().map_err(|_| Failure::NoRuntime)?;
        return runtime.block_on(future).map_err(classify);
    };
    if handle.runtime_flavor() != RuntimeFlavor::MultiThread {
        return Err(Failure::NoRuntime);
    }
    tokio::task::block_in_place(|| handle.block_on(future)).map_err(classify)
}

fn classify(error: reqwest::Error) -> Failure {
    if error.is_timeout() {
        Failure::Timeout
    } else {
        Failure::Transport(error)
    }
}
