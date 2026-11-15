//! 记忆注入：把「核心记忆」拼进即将出站的请求体。
//!
//! ── 为什么挂在提示词层之后、脱敏层之前 ──────────────────────
//! `payload.rs` 的处理链是有序的：① 提示词层 → ② 脱敏层 → ③ 图片压缩。
//! 记忆注入插在 ① 与 ② 之间，正好继承两头的好处：
//!   · 在 ① 之后 —— 提示词层可能整段替换 system（`custom` / `fujiang`），
//!     先注入会被替掉。注入必须在「system 最终形态」之上做。
//!   · 在 ② 之前 —— 注入的文本同样要过一遍指纹脱敏，与「网关自有提示词也要
//!     过脱敏」是同一条理由：刚写进去的东西没有人再过一遍，就会成为漏网的那段。
//!
//! ── 为什么默认关闭 ──────────────────────────────────────────
//! 它会改写发给上游的请求体。默认开等于「升级即改变既有行为」，与本项目
//! 「新能力默认不介入转发」的一贯取向冲突。开关在记忆中枢页面里。
//!
//! ── 幂等性 ──────────────────────────────────────────────────
//! 每次尝试都在 `ctx.body` 的**副本**上做，不写回 ctx，所以重试不会叠加。
//! 另外注入块带一个固定特征前缀，万一上游把上一轮的 system 回显进历史，
//! 也不会被再加一遍。

use serde_json::{json, Map, Value};

use super::model::{estimate_tokens, truncate_chars};
use super::search;
use super::{config, store};

/// 注入块的首行。既是给模型的说明，也是「这段是不是我加的」的判据。
const HEADER: &str = "# 长期记忆（会话开局可读）";

/// 给一个请求体算注入块；不需要注入时返回 `None`。
///
/// 返回 `None` 的三种情况都是「什么都不该做」：模块关、注入关、算出来是空。
pub fn block_for(body: &Value) -> Option<String> {
    let settings = config();
    if !settings.enabled || !settings.inject_enabled {
        return None;
    }
    let project = settings.inject_project.as_deref();
    match settings.inject_mode.as_str() {
        "search" => search_block(body, project, &settings),
        // 默认按 core：会话开局一次性给「画像 + 项目要点」，不做检索
        _ => core_block(project, settings.inject_tokens),
    }
}

fn core_block(project: Option<&str>, budget: usize) -> Option<String> {
    // 缓存键 = (内容版本号, 项目, 预算)。内容一变版本号就变，缓存立即失效；
    // 三个条件都相同才复用，所以「切了项目」「改了预算」都不会读到上一份。
    //
    // 为什么要缓存：core 模式要把全部记忆读出来拼文本，每个请求做一次目录扫描
    // 在网关这种 QPS 下是纯浪费 —— 而记忆内容在两次写入之间是静止的。
    use std::sync::Mutex;
    static CACHE: Mutex<Option<(u64, Option<String>, usize, Option<String>)>> = Mutex::new(None);
    let revision = super::revision();
    let key_project = project.map(str::to_string);
    if let Ok(slot) = CACHE.lock() {
        if let Some((cached_revision, cached_project, cached_budget, cached_block)) = slot.as_ref() {
            if *cached_revision == revision && *cached_project == key_project && *cached_budget == budget {
                return cached_block.clone();
            }
        }
    }
    let entries = store::load_all();
    let (text, _, _) = search::core_text(&entries, project, budget);
    // 只有标题 + 说明、没有任何条目时不要注入（否则每轮都在浪费 token）
    let block = if text.lines().filter(|line| line.starts_with("- ")).count() == 0 {
        None
    } else {
        Some(text)
    };
    if let Ok(mut slot) = CACHE.lock() {
        *slot = Some((revision, key_project, budget, block.clone()));
    }
    block
}

fn search_block(body: &Value, project: Option<&str>, settings: &super::MemoryConfig) -> Option<String> {
    let query = last_user_text(body);
    if query.trim().chars().count() < settings.inject_min_chars {
        return None;
    }
    let mut request = super::Query {
        text: query,
        project: project.map(str::to_string),
        include_superseded: false,
        include_deleted: false,
        limit: settings.inject_limit,
        ..Default::default()
    };
    // 预算小时连条数一起收
    if settings.inject_tokens < 400 {
        request.limit = request.limit.min(3);
    }
    let rows = search::search(&request);
    if rows.is_empty() {
        return None;
    }
    let now = crate::server::logging::now_ms();
    let mut out = String::new();
    out.push_str(HEADER);
    out.push('\n');
    out.push_str("与本轮问题相关的过往记忆（若与本轮事实冲突，以本轮为准）：\n");
    let mut used = estimate_tokens(&out);
    for row in rows {
        let line = search::compact_line(&row, now);
        let cost = estimate_tokens(&line) + 1;
        if used + cost > settings.inject_tokens {
            break;
        }
        out.push_str("- ");
        out.push_str(&line);
        out.push('\n');
        used += cost;
    }
    if used <= estimate_tokens(HEADER) + 8 {
        return None;
    }
    Some(out)
}

/// 取最后一条 user 消息的纯文本（多模态数组取第一段文字）。
pub fn last_user_text(body: &Value) -> String {
    let Some(messages) = body.get("messages").and_then(Value::as_array) else {
        return String::new();
    };
    for message in messages.iter().rev() {
        let role = message.get("role").and_then(Value::as_str).unwrap_or("");
        if role != "user" {
            continue;
        }
        let Some(content) = message.get("content") else { continue };
        if let Some(text) = content.as_str() {
            if !text.trim().is_empty() {
                return text.to_string();
            }
        }
        if let Some(parts) = content.as_array() {
            for part in parts {
                if let Some(text) = part.get("text").and_then(Value::as_str) {
                    if !text.trim().is_empty() {
                        return text.to_string();
                    }
                }
            }
        }
    }
    String::new()
}

/// 把记忆块拼进请求体。返回新体；不需要注入时返回 `None`。
///
/// 拼法：找**第一条** system / developer 消息，把块追加到它的正文末尾；
/// 没有 system 时在最前面插一条。为什么不新插一条独立 system：不少上游对
/// 「多条 system」的处理不一致（有的只认第一条、有的合并时改变顺序），
/// 复用它已有的那一条是所有家都能吃下的形态。
pub fn apply(body: &Value) -> Option<Value> {
    let block = block_for(body)?;
    let mut next = body.clone();
    let messages = next.get_mut("messages")?.as_array_mut()?;

    // 已有同样的块（历史回显 / 重复调用）就不再加
    if messages.iter().any(|message| {
        message
            .get("content")
            .and_then(Value::as_str)
            .map(|text| text.contains(HEADER))
            .unwrap_or(false)
    }) {
        return None;
    }

    for message in messages.iter_mut() {
        let role = message.get("role").and_then(Value::as_str).unwrap_or("");
        if role != "system" && role != "developer" {
            continue;
        }
        if append_to_content(message, &block) {
            crate::server::logging::verbose("[Memory]", &describe(&block));
            return Some(next);
        }
    }

    // 没有可用的 system：插到最前面
    let mut entry = Map::new();
    entry.insert("role".to_string(), Value::String("system".to_string()));
    entry.insert("content".to_string(), Value::String(block.clone()));
    messages.insert(0, Value::Object(entry));
    crate::server::logging::verbose("[Memory]", &describe(&block));
    Some(next)
}

/// 往一条消息的 content 上追加文本。支持字符串与「内容块数组」两种形态。
/// 形态不认识时返回 false（交给调用方继续找下一条）。
fn append_to_content(message: &mut Value, block: &str) -> bool {
    let Some(content) = message.get_mut("content") else {
        return false;
    };
    if let Some(text) = content.as_str() {
        let joined = if text.trim().is_empty() {
            block.to_string()
        } else {
            format!("{}\n\n{}", text.trim_end(), block)
        };
        *content = Value::String(joined);
        return true;
    }
    if let Some(parts) = content.as_array_mut() {
        parts.push(json!({ "type": "text", "text": block }));
        return true;
    }
    false
}

/// 调试用：把注入块压成一行摘要（日志里不打印全文）
pub fn describe(block: &str) -> String {
    let body: String = block.lines().skip(2).collect::<Vec<_>>().join(" ");
    format!(
        "记忆注入 {} 字 / 约 {} token：{}",
        block.chars().count(),
        estimate_tokens(block),
        truncate_chars(body.trim(), 60)
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn last_user_text_picks_latest_string() {
        let body = json!({
            "messages": [
                { "role": "user", "content": "第一条" },
                { "role": "assistant", "content": "回答" },
                { "role": "user", "content": "第二条" }
            ]
        });
        assert_eq!(last_user_text(&body), "第二条");
    }

    #[test]
    fn last_user_text_reads_content_parts() {
        let body = json!({
            "messages": [ { "role": "user", "content": [
                { "type": "text", "text": "看图说点什么" },
                { "type": "image_url", "image_url": { "url": "data:..." } }
            ]}]
        });
        assert_eq!(last_user_text(&body), "看图说点什么");
    }

    #[test]
    fn append_merges_into_existing_system() {
        let mut message = json!({ "role": "system", "content": "原有规则" });
        assert!(append_to_content(&mut message, "# 长期记忆"));
        let text = message["content"].as_str().unwrap();
        assert!(text.starts_with("原有规则"));
        assert!(text.contains("# 长期记忆"));
    }

    #[test]
    fn append_pushes_text_part_for_array_content() {
        let mut message = json!({ "role": "system", "content": [ { "type": "text", "text": "a" } ] });
        assert!(append_to_content(&mut message, "B"));
        let parts = message["content"].as_array().unwrap();
        assert_eq!(parts.len(), 2);
        assert_eq!(parts[1]["text"], "B");
    }

    #[test]
    fn append_rejects_unknown_shape() {
        let mut message = json!({ "role": "system", "content": 42 });
        assert!(!append_to_content(&mut message, "B"));
    }
}
