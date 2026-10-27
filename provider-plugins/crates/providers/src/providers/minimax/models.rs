//! MiniMax 的模型清单（**静态表**，上游没有公开的列模型接口）。
//!
//! ── 为什么是静态表 ──────────────────────────────────────────
//! `agent.minimax.cn` 的 mavis 推理端实测只有 messages / credit / signin 三类
//! 路径，没有「列模型」接口；内核版（`feat/minimax-native`）同样把模型硬编码。
//! 因此 `capabilities.model_refresh = false`：界面显示「这家用固定模型清单」，
//! 不假装能刷新。
//!
//! ── 表里的名字是什么 ────────────────────────────────────────
//! 写的是**上游 Anthropic messages 端点认的 model 字面值**（实测可用）：
//! `MiniMax-M3.1-Flash-Preview` / `MiniMax-M3` / `MiniMax-M2.7-highspeed` /
//! `MiniMax-M2.7`。转发时原样透传，对外 id 就是上游 id。
//!
//! ── 硬约束 ──────────────────────────────────────────────────
//! release 是 `panic=abort`：本文件零 unwrap/expect/panic。

use serde_json::{json, Value};

/// 一条静态模型（顺序 = 展示顺序：新代在前，同代相邻）。
struct Spec {
    /// 上游模型 id（原样转发）
    id: &'static str,
    /// 展示名
    name: &'static str,
}

const MODELS: &[Spec] = &[
    Spec { id: "MiniMax-M3.1-Flash-Preview", name: "MiniMax M3.1 Flash Preview" },
    Spec { id: "MiniMax-M3", name: "MiniMax M3" },
    Spec { id: "MiniMax-M2.7-highspeed", name: "MiniMax M2.7 Highspeed" },
    Spec { id: "MiniMax-M2.7", name: "MiniMax M2.7" },
];

/// 本家的模型清单（聚合目录认的形态）。
pub fn list() -> Vec<Value> {
    MODELS
        .iter()
        .map(|spec| {
            json!({
                "id": spec.id,
                "name": spec.name,
                // M 系列在官方客户端里靠工具调用跑 agent 循环，Anthropic tool_use 实测可用
                "supportsToolCall": true,
            })
        })
        .collect()
}

/// 客户端可能带不同大小写：命中静态表时归一到上游字面值，未知名原样透传
/// （让上游自己报「模型不存在」，比我们改写成别的名字更诚实）。
pub fn upstream_model_id(requested: &str) -> String {
    let needle = requested.trim().to_ascii_lowercase();
    MODELS
        .iter()
        .find(|spec| spec.id.to_ascii_lowercase() == needle)
        .map(|spec| spec.id.to_string())
        .unwrap_or_else(|| requested.trim().to_string())
}
