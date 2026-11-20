//! 记忆中枢 · 写入前脱敏（`privacy.redact` / `privacy.redactRules`）。
//!
//! ── 为什么放在写入路径上，而不是导入路径上 ──────────────────
//! 敏感内容不止从导入进来：MCP 工具写入、界面上手写、自动化任务产出的正文
//! 都可能带着密钥。只在导入里过滤等于「换一条路就漏」。所以脱敏挂在
//! `write_inner`（所有写入的必经之路）与 `update`（改写正文）上 ——
//! 一处生效，全链路生效。
//!
//! ── 为什么是「替换成 ***」而不是「跳过整条」──────────────────
//! 导入侧的 `import.sensitiveSkip` 是**跳过整条**（保守：宁可少一条记忆），
//! 而这里是**局部替换**：一条决策记录里顺手贴了个 token，整条丢掉等于把
//! 决策本身也丢了。两者是不同场景下的不同取舍，都存在是有意的。
//!
//! ── 为什么规则编译要缓存 ────────────────────────────────────
//! 导入一次几千条，每条都编译六个正则就是几万次编译。规则集是配置项、变化
//! 极少，所以按「规则列表拼成的串」做键缓存：串一样就直接复用编译结果，
//! 串变了（用户改了规则）自然重建。
//!
//! ── 为什么某条规则写坏了只跳过它 ────────────────────────────
//! 正则写错是用户在「内部」层级里手改配置时会踩的坑。整条链路报错会让写入
//! 直接失败（记忆写不进去），而跳过那一条只会少脱敏一种形态 —— 后者的代价
//! 小得多，且有一条明确的日志可查。

use std::sync::{Mutex, OnceLock};

use regex::Regex;

/// 命中后的占位。用固定串而不是「保留前几位」：保留前几位等于把秘密的一部分
/// 写进了一个会被模型读到的文件里，遮蔽要彻底才有意义。
const MASK: &str = "***";

/// 编译缓存：`(规则串, 编译结果)`。
static CACHE: OnceLock<Mutex<Option<(String, Vec<Regex>)>>> = OnceLock::new();

fn cache() -> &'static Mutex<Option<(String, Vec<Regex>)>> {
    CACHE.get_or_init(|| Mutex::new(None))
}

/// 编译一组规则（带缓存）。返回 `(正则, 跳过原因)`：跳过原因用于日志。
fn compiled(rules: &[String]) -> (Vec<Regex>, Vec<String>) {
    let key = rules.join("\u{1}");
    if let Ok(guard) = cache().lock() {
        if let Some((cached_key, list)) = guard.as_ref() {
            if *cached_key == key {
                return (list.clone(), Vec::new());
            }
        }
    }
    let mut list: Vec<Regex> = Vec::new();
    let mut broken: Vec<String> = Vec::new();
    for rule in rules {
        let pattern = rule.trim();
        if pattern.is_empty() {
            continue;
        }
        match Regex::new(pattern) {
            Ok(regex) => list.push(regex),
            // 写坏的规则只跳过这一条：让写入继续，日志里留一句可查的线索
            Err(error) => broken.push(format!("{pattern}（{error}）")),
        }
    }
    if let Ok(mut guard) = cache().lock() {
        *guard = Some((key, list.clone()));
    }
    (list, broken)
}

/// 对一段正文脱敏。开关关着、或没有可用规则时**原样返回**（计数为 0）。
///
/// 返回值第二项是命中次数：调用方用它写一条「这次脱敏了几处」的日志 ——
/// 用户开了开关却看不到任何命中记录，会怀疑开关没生效。
pub fn apply(text: &str) -> (String, usize) {
    if !super::config::bool_of("privacy.redact") {
        return (text.to_string(), 0);
    }
    let rules = super::config::strings_of("privacy.redactRules");
    if rules.is_empty() {
        // 开了开关但一条规则都没有：这不是「没事」，是配置没配完。静默放过去
        // 等于给用户一个假的「已脱敏」，所以留一条日志。
        crate::server::logging::log(
            "[Memory]",
            "⚠️ privacy.redact 已开启但 privacy.redactRules 为空，本次不做脱敏",
        );
        return (text.to_string(), 0);
    }
    let (list, broken) = compiled(&rules);
    if !broken.is_empty() {
        crate::server::logging::log(
            "[Memory]",
            &format!("⚠️ 脱敏规则无法编译，已跳过：{}", broken.join("；")),
        );
    }
    if list.is_empty() {
        return (text.to_string(), 0);
    }
    let mut out = text.to_string();
    let mut hits = 0usize;
    for regex in &list {
        let count = regex.find_iter(&out).count();
        if count == 0 {
            continue;
        }
        out = regex.replace_all(&out, MASK).to_string();
        hits += count;
    }
    (out, hits)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 不带配置的纯内核：验证「逐条替换」这件事本身。
    fn redact_with(rules: &[&str], text: &str) -> (String, usize) {
        let owned: Vec<String> = rules.iter().map(|rule| rule.to_string()).collect();
        let (list, _) = compiled(&owned);
        let mut out = text.to_string();
        let mut hits = 0usize;
        for regex in &list {
            let count = regex.find_iter(&out).count();
            if count == 0 {
                continue;
            }
            out = regex.replace_all(&out, MASK).to_string();
            hits += count;
        }
        (out, hits)
    }

    #[test]
    fn replaces_every_match_with_a_fixed_mask() {
        let (text, hits) = redact_with(&[r"sk-[A-Za-z0-9_-]{8,}"], "key=sk-abcdefgh1234 tail");
        assert_eq!(hits, 1);
        assert_eq!(text, "key=*** tail");
    }

    #[test]
    fn replaces_all_occurrences_not_just_the_first() {
        let (text, hits) = redact_with(&[r"\d{11}"], "13800138000 与 13900139000");
        assert_eq!(hits, 2);
        assert_eq!(text, "*** 与 ***");
    }

    #[test]
    fn a_broken_pattern_does_not_take_down_the_others() {
        let rules = vec!["(".to_string(), r"sk-[a-z]{4}".to_string()];
        let (list, broken) = compiled(&rules);
        assert_eq!(list.len(), 1, "坏规则应当被跳过");
        assert_eq!(broken.len(), 1);
        assert!(list[0].is_match("sk-abcd"));
    }

    #[test]
    fn compiled_cache_is_keyed_by_the_rule_list() {
        let first = vec!["a+".to_string()];
        let (list_one, _) = compiled(&first);
        assert_eq!(list_one.len(), 1);
        // 换一组规则必须重建（缓存键不同），不能沿用上一组
        let second = vec!["b+".to_string(), "c+".to_string()];
        let (list_two, _) = compiled(&second);
        assert_eq!(list_two.len(), 2);
        assert!(list_two[0].is_match("bbb"));
    }
}