//! MiniMax 的模型清单（**静态表**）。
//!
//! ── 为什么是静态表而不是远程目录 ────────────────────────────
//! `agent.minimax.cn` 的 mavis 推理端点没有公开的「列模型」接口（实测：
//! 凭据可用的探测里只有 messages / credit / signin 三类路径）。参考实现
//! （本地 shim）同样把这四支模型硬编码成常量表。因此本模块不提供 `refresh`：
//! `supports_model_refresh()` 为 false，适配器如实回答「这家没有远程目录」，
//! 而不是假装刷了一次（与 `zcode::models` 同一处置、同一理由）。
//!
//! ── 表里的名字是什么 ────────────────────────────────────────
//! 写的是**上游 Anthropic messages 端点认的 model 字面值**（实测可用）：
//! `MiniMax-M3.1-Flash-Preview` / `MiniMax-M3` / `MiniMax-M2.7-highspeed` /
//! `MiniMax-M2.7`。转发时**原样透传**、不做改名 —— 对外 id 就是上游 id
//! （与 Loomy 同口径）。
//!
//! 能力位（上下文 / 输出上限 / 思考 / 视觉）一律不在这里声明，由
//! `core::model_meta` 从 models.dev 补齐（与 `zcode::models` 的 2026-10
//! 收窄同一理由：没有依据的声明比空着更糟）。只有两条协议级事实声明：
//! `supportsToolCall`（M 系列在官方客户端里就是靠工具调用跑 agent 循环，
//! Anthropic tool_use 块实测可用）。
//!
//! ── 硬约束 ──────────────────────────────────────────────────
//! release 是 `panic=abort`：本文件零 unwrap/expect/panic。

use serde_json::{json, Value};

/// 一条静态模型。
struct Spec {
    /// 上游模型 id（**原样转发**，不做任何改名）
    id: &'static str,
    /// 展示名
    name: &'static str,
}

/// 模型表。顺序 = 展示顺序（新代在前，同代相邻）。
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
                "supportsToolCall": true,
            })
        })
        .collect()
}

/// 这个模型名是不是本家认识的（大小写不敏感，与客户端可能带原始大小写兼容）。
pub fn is_known(model: &str) -> bool {
    let needle = model.trim().to_ascii_lowercase();
    MODELS.iter().any(|spec| spec.id.to_ascii_lowercase() == needle)
}

/// 上游模型 id：本家对外 id 与上游字面值**逐字相同**，原样透传。
/// （表外名字也原样透传 —— 上游若上新模型，客户端点名就能用，等下一次
/// 更新本表再进目录，与 zcode 的透传口径一致。）
pub fn upstream_model_id(requested: &str) -> String {
    requested.trim().to_string()
}

#[cfg(test)]
mod tests {
    use super::{is_known, list};

    #[test]
    fn list_has_four_models() {
        assert_eq!(list().len(), 4);
    }

    #[test]
    fn known_checks_case_insensitive() {
        assert!(is_known("minimax-m3"));
        assert!(is_known(" MiniMax-M2.7 "));
        assert!(!is_known("gpt-4"));
    }
}
