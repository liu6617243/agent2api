//! 更新源索引：从这个仓库的 `app.json` 里读「构建产物放在哪个仓库」。
//!
//! ── 为什么要有这一层 ────────────────────────────────────────
//! 安装包由独立账号（`Agent2Builder`）下的构建仓库产出，而不是本项目主仓库。
//! 如果把这个仓库名硬编码进二进制，那么构建仓库一旦不可用（封号、误删、
//! 迁移），**所有已发布的版本都会失去更新能力**，而唯一的补救是发一个新版
//! 本 —— 前提是用户还能拿到新版本。这是个死循环。
//!
//! 所以把「构建产物在哪」做成**远程可改的一行配置**：索引文件放在一个与
//! 构建完全无关的仓库里，换构建仓库时只改那一个文件，用户下次检查更新就
//! 自动跟上，不必发版。
//!
//! ── 索引仓库为什么与插件索引是同一个 ────────────────────────
//! `Agent2plugin/Agent2plugin` 已经是插件商店的**唯一信任根**（见
//! `plugin_host::update::INDEX_REPO`）。再加一个「应用更新的信任根」等于让
//! 部署方维护两处，而它们回答的是同一类问题（「官方产物在哪」）。同一个
//! 仓库、两个文件（`index.json` 给插件，`app.json` 给本应用）。
//!
//! ── 读不到索引时的行为：静默回落到内置仓库 ──────────────────
//! 索引读失败（网络不通、仓库还没建、JSON 坏了、字段缺失）**不报错**，
//! 一律用 [`DEFAULT_REPO`]。理由有两条：
//!   · 更新检查失败不该影响用户用软件 —— 这是本模块与插件商店共同的取向；
//!   · 索引存在的作用是「换地址」，不是「必须读到才能更新」。正常情况下
//!     它与内置值指向同一个仓库，读不到等于没变化。
//!
//! 将来若接入更多更新源，这一层就是接入点：换成「按顺序试多个索引源」即可，
//! 调用方（`check::fetch_release`）一行都不用改。

use serde_json::Value;

use crate::server::core::plugin_host;
use crate::server::core::update::client;
use crate::server::core::update::version::DEFAULT_REPO;
use crate::server::logging;

/// 索引文件的原始内容地址（raw.githubusercontent.com，不吃 API 限额）。
///
/// 与插件索引共用仓库但**不同文件**：两个文件各自的 `schema` 独立演进，
/// 插件的索引改格式时不必连带改应用更新。
fn index_url() -> String {
    format!(
        "https://raw.githubusercontent.com/{}/main/app.json",
        plugin_host::update::INDEX_REPO
    )
}

/// 读索引的超时。它只是一个几百字节的 JSON，与插件索引同一档。
const INDEX_TIMEOUT_MS: u64 = 20_000;

/// 索引里声明的「应用构建产物所在仓库」。
///
/// 返回 `owner/repo` 形式；任何一步不成立都回 [`DEFAULT_REPO`]（静默，见模块头）。
pub async fn resolve_app_repo() -> String {
    match fetch_repo().await {
        Some(repo) => {
            logging::verbose(
                "[Update]",
                &format!("更新源索引指向 {repo}"),
            );
            repo
        }
        None => DEFAULT_REPO.to_string(),
    }
}

/// 拉一次索引并取出 `app.repo`。
///
/// 校验口径：非空、形如 `owner/repo`（恰好一个斜杠、两侧都有字符）。
/// 不校验仓库是否**存在** —— 那要多打一次 API，而「地址写错」的后果与
/// 「索引读不到」相同（检查更新时 404 按「无更新」处理，不是错误）。
async fn fetch_repo() -> Option<String> {
    let response = client::fetch_public(
        &index_url(),
        // raw.githubusercontent.com 不需要 Accept: application/vnd.github+json，
        // 但也不需要别的头；空切片即可（与插件索引同）
        &[],
        Some(INDEX_TIMEOUT_MS),
    )
    .await
    .ok()?;
    if !response.status().is_success() {
        return None;
    }
    let text = response.text().await.ok()?;
    let parsed: Value = serde_json::from_str(&text).ok()?;
    let repo = parsed
        .get("app")
        .and_then(|app| app.get("repo"))
        .and_then(Value::as_str)?
        .trim();
    if !is_repo_slug(repo) {
        logging::verbose(
            "[Update]",
            &format!("更新源索引里的 app.repo 不是 owner/repo 形式：{repo}"),
        );
        return None;
    }
    Some(repo.to_string())
}

/// 是否形如 `owner/repo`：恰好一个斜杠，两侧都非空，且不含空白。
///
/// 不追求完整的 GitHub 命名规则（那会拒绝一些历史仓库名）；这里只拦
/// 「写成了一个 URL」「扫进了一段说明文字」这类明显的错值。
fn is_repo_slug(value: &str) -> bool {
    if value.is_empty() || value.chars().any(char::is_whitespace) {
        return false;
    }
    let mut parts = value.split('/');
    let (Some(owner), Some(repo), None) = (parts.next(), parts.next(), parts.next()) else {
        return false;
    };
    !owner.is_empty() && !repo.is_empty()
}
