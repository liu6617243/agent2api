//! 记忆中枢 · 派生索引（可直接删掉重建）。
//!
//! ── 为什么索引进单独的库文件，而不进 `agent2api.db` ──────────
//! 索引是**纯派生数据**：删掉这个文件、调一次 [`rebuild`]，内容与删之前逐字
//! 相同。放进主库意味着「给所有存量用户推一次 schema 版本」，而这里任何一次
//! 索引逻辑改动都要再推一次 —— 派生数据没有资格占用事实库的版本号。
//! 独立文件同时给了用户一个最干脆的修复动作：删了它。
//!
//! ── FTS5 是「有就用、没有就退化」──────────────────────────────
//! `rusqlite` 的 `bundled` 构建通常带 FTS5，但这属于「依赖的编译选项」而不是
//! 我们的代码能保证的事。所以建虚拟表时探一次：成功走 FTS5 检索，失败就退回
//! 「把行读进内存做子串计分」。两条路共用同一份排序逻辑，退化只会慢一点，
//! 不会搜不到。
//!
//! ── 中文为什么要先「拆字加空格」再进索引 ────────────────────
//! FTS5 的 `unicode61` 分词器把连续的 CJK 当成**一个** token：「记忆中枢」整串
//! 是一个词，搜「记忆」命中不了。把每个 CJK 字之间插一个空格之后，「记忆中枢」
//! 变成三个 token，查询侧也做同样处理并用短语（`"记 忆"`）匹配，就得到了
//! 「按字相邻」的语义 —— 效果接近二元分词，且不需要任何词典。

use std::sync::{Mutex, OnceLock};

use rusqlite::{params, Connection};
use serde_json::{json, Value};

use super::model::{is_cjk, MemoryEntry};

/// 检索用的行（只在库内流转，不直接出接口）
#[derive(Debug, Clone)]
pub struct IndexRow {
    pub id: String,
    pub project: Option<String>,
    pub agent: Option<String>,
    pub kind: String,
    pub title: String,
    pub summary: String,
    pub tags: Vec<String>,
    pub importance: f64,
    pub pinned: bool,
    pub starred: bool,
    pub created_at: i64,
    pub updated_at: i64,
    pub superseded_by: Option<String>,
    pub deleted_at: Option<i64>,
    pub path: String,
    /// FTS5 给出的负分（越小越相关）。退化模式下恒 0，由调用方按命中数补分。
    pub rank: f64,
    /// 命中的关键词个数（退化模式下的相关度依据）
    pub hits: usize,
}

impl IndexRow {
    pub fn to_json(&self) -> Value {
        json!({
            "id": self.id,
            "title": self.title,
            "type": self.kind,
            "project": self.project,
            "agent": self.agent,
            "tags": self.tags,
            "importance": self.importance,
            "pinned": self.pinned,
            "starred": self.starred,
            "createdAt": self.created_at,
            "updatedAt": self.updated_at,
            "supersededBy": self.superseded_by,
            "summary": self.summary,
            "path": self.path,
        })
    }
}

pub struct MemoryIndex {
    conn: Mutex<Connection>,
    /// FTS5 虚拟表是否建得起来
    fts: bool,
}

impl MemoryIndex {
    fn open() -> Self {
        let path = super::store::index_file();
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let conn = match Connection::open(&path) {
            Ok(conn) => conn,
            Err(error) => {
                // 打不开就退到内存库：检索仍然可用（每次重建），只是慢
                crate::server::logging::log(
                    "[Memory]",
                    &format!("⚠️ 索引库打开失败（改用内存索引）{}：{error}", path.display()),
                );
                Connection::open_in_memory().expect("内存 SQLite 不可用")
            }
        };
        let _ = conn.execute_batch(
            "PRAGMA journal_mode=WAL;
             CREATE TABLE IF NOT EXISTS entries (
               id TEXT PRIMARY KEY,
               project TEXT, agent TEXT, kind TEXT,
               title TEXT, summary TEXT, tags TEXT,
               importance REAL DEFAULT 0.5,
               pinned INTEGER DEFAULT 0, starred INTEGER DEFAULT 0,
               created_at INTEGER, updated_at INTEGER,
               superseded_by TEXT, deleted_at INTEGER,
               body TEXT, path TEXT
             );
             CREATE INDEX IF NOT EXISTS entries_project ON entries(project);
             CREATE INDEX IF NOT EXISTS entries_updated ON entries(updated_at DESC);",
        );
        // FTS5 探测：建不起来就退化（编译期选项，不是运行期错误）
        let fts = conn
            .execute_batch(
                "CREATE VIRTUAL TABLE IF NOT EXISTS entries_fts
                   USING fts5(id UNINDEXED, text, tokenize='unicode61');",
            )
            .is_ok();
        if !fts {
            crate::server::logging::log(
                "[Memory]",
                "ℹ️ 未启用 FTS5（SQLite 未编译该扩展），记忆检索退化为内存子串匹配",
            );
        }
        MemoryIndex {
            conn: Mutex::new(conn),
            fts,
        }
    }

    pub fn fts_enabled(&self) -> bool {
        self.fts
    }

    /// 全量重建：清表 → 按事实源逐条写入。索引损坏时这是唯一修复动作。
    pub fn rebuild(&self, entries: &[MemoryEntry]) -> Result<usize, String> {
        let Ok(mut conn) = self.conn.lock() else {
            return Err("索引锁不可用".into());
        };
        let tx = conn.transaction().map_err(|error| error.to_string())?;
        tx.execute("DELETE FROM entries", []).map_err(|error| error.to_string())?;
        if self.fts {
            let _ = tx.execute("DELETE FROM entries_fts", []);
        }
        let mut count = 0usize;
        for entry in entries {
            write_row(&tx, entry, self.fts).map_err(|error| error.to_string())?;
            count += 1;
        }
        tx.commit().map_err(|error| error.to_string())?;
        Ok(count)
    }

    /// 单条写入（写记忆后立即生效，不必等整轮重建）
    pub fn upsert(&self, entry: &MemoryEntry) -> Result<(), String> {
        let Ok(mut conn) = self.conn.lock() else {
            return Err("索引锁不可用".into());
        };
        let tx = conn.transaction().map_err(|error| error.to_string())?;
        write_row(&tx, entry, self.fts).map_err(|error| error.to_string())?;
        tx.commit().map_err(|error| error.to_string())
    }

    pub fn drop_one(&self, id: &str) {
        let Ok(conn) = self.conn.lock() else { return };
        let _ = conn.execute("DELETE FROM entries WHERE id = ?1", params![id]);
        if self.fts {
            let _ = conn.execute("DELETE FROM entries_fts WHERE id = ?1", params![id]);
        }
    }

    /// 库里现有条目数（状态自检用）
    pub fn count(&self) -> usize {
        let Ok(conn) = self.conn.lock() else { return 0 };
        conn.query_row("SELECT COUNT(*) FROM entries", [], |row| row.get::<_, i64>(0))
            .map(|value| value as usize)
            .unwrap_or(0)
    }

    /// 无条件列一批行（列表页 / 概览用）。条件过滤在 Rust 侧做 —— 记忆规模是
    /// 「几百到几千」，SQL 动态拼接换来的收益不值得那份注入面。
    pub fn rows(&self, include_deleted: bool) -> Vec<IndexRow> {
        let Ok(conn) = self.conn.lock() else { return Vec::new() };
        let sql = if include_deleted {
            "SELECT id, project, agent, kind, title, summary, tags, importance, pinned, starred,
                    created_at, updated_at, superseded_by, deleted_at, path
               FROM entries ORDER BY updated_at DESC"
        } else {
            "SELECT id, project, agent, kind, title, summary, tags, importance, pinned, starred,
                    created_at, updated_at, superseded_by, deleted_at, path
               FROM entries WHERE deleted_at IS NULL ORDER BY updated_at DESC"
        };
        let mut statement = match conn.prepare(sql) {
            Ok(statement) => statement,
            Err(_) => return Vec::new(),
        };
        let mapped = statement.query_map([], row_to_index);
        match mapped {
            Ok(rows) => rows.flatten().collect(),
            Err(_) => Vec::new(),
        }
    }

    /// 关键词检索。`query` 为空时等价于「按时间列一批」。
    pub fn search(&self, query: &str) -> Vec<IndexRow> {
        let tokens = tokenize_query(query);
        if tokens.is_empty() {
            return self.rows(false);
        }
        if self.fts {
            if let Some(rows) = self.search_fts(&tokens) {
                return rows;
            }
        }
        self.search_scan(&tokens)
    }

    fn search_fts(&self, tokens: &[String]) -> Option<Vec<IndexRow>> {
        let expression = fts_expression(tokens)?;
        let conn = self.conn.lock().ok()?;
        let mut statement = conn
            .prepare(
                "SELECT e.id, e.project, e.agent, e.kind, e.title, e.summary, e.tags, e.importance,
                        e.pinned, e.starred, e.created_at, e.updated_at, e.superseded_by,
                        e.deleted_at, e.path, bm25(entries_fts) AS rank
                   FROM entries_fts
                   JOIN entries e ON e.id = entries_fts.id
                  WHERE entries_fts MATCH ?1 AND e.deleted_at IS NULL
                  ORDER BY rank
                  LIMIT 200",
            )
            .ok()?;
        let mapped = statement.query_map(params![expression], |row| {
            let mut item = row_to_index(row)?;
            item.rank = row.get::<_, f64>(15)?;
            Ok(item)
        });
        let rows: Vec<IndexRow> = mapped.ok()?.flatten().collect();
        Some(rows)
    }

    fn search_scan(&self, tokens: &[String]) -> Vec<IndexRow> {
        let Ok(conn) = self.conn.lock() else { return Vec::new() };
        let mut statement = match conn.prepare(
            "SELECT id, project, agent, kind, title, summary, tags, importance, pinned, starred,
                    created_at, updated_at, superseded_by, deleted_at, path, body
               FROM entries WHERE deleted_at IS NULL",
        ) {
            Ok(statement) => statement,
            Err(_) => return Vec::new(),
        };
        let mapped = statement.query_map([], |row| {
            let mut item = row_to_index(row)?;
            let body: String = row.get::<_, Option<String>>(15)?.unwrap_or_default();
            let haystack = normalize(&format!(
                "{} {} {} {}",
                item.title, item.summary, item.tags.join(" "), body
            ));
            item.hits = tokens.iter().filter(|token| haystack.contains(*token)).count();
            Ok(item)
        });
        let mut rows: Vec<IndexRow> = mapped.map(|rows| rows.flatten().collect()).unwrap_or_default();
        rows.retain(|row| row.hits > 0);
        rows
    }
}

fn row_to_index(row: &rusqlite::Row<'_>) -> rusqlite::Result<IndexRow> {
    let tags: String = row.get::<_, Option<String>>(6)?.unwrap_or_default();
    Ok(IndexRow {
        id: row.get(0)?,
        project: row.get(1)?,
        agent: row.get(2)?,
        kind: row.get::<_, Option<String>>(3)?.unwrap_or_else(|| "other".into()),
        title: row.get::<_, Option<String>>(4)?.unwrap_or_default(),
        summary: row.get::<_, Option<String>>(5)?.unwrap_or_default(),
        tags: tags.split_whitespace().map(|tag| tag.to_string()).collect(),
        importance: row.get::<_, Option<f64>>(7)?.unwrap_or(0.5),
        pinned: row.get::<_, Option<i64>>(8)?.unwrap_or(0) != 0,
        starred: row.get::<_, Option<i64>>(9)?.unwrap_or(0) != 0,
        created_at: row.get::<_, Option<i64>>(10)?.unwrap_or(0),
        updated_at: row.get::<_, Option<i64>>(11)?.unwrap_or(0),
        superseded_by: row.get(12)?,
        deleted_at: row.get(13)?,
        path: row.get::<_, Option<String>>(14)?.unwrap_or_default(),
        rank: 0.0,
        hits: 0,
    })
}

fn write_row(conn: &Connection, entry: &MemoryEntry, fts: bool) -> rusqlite::Result<()> {
    conn.execute(
        "INSERT INTO entries
           (id, project, agent, kind, title, summary, tags, importance, pinned, starred,
            created_at, updated_at, superseded_by, deleted_at, body, path)
         VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16)
         ON CONFLICT(id) DO UPDATE SET
           project=excluded.project, agent=excluded.agent, kind=excluded.kind,
           title=excluded.title, summary=excluded.summary, tags=excluded.tags,
           importance=excluded.importance, pinned=excluded.pinned, starred=excluded.starred,
           created_at=excluded.created_at, updated_at=excluded.updated_at,
           superseded_by=excluded.superseded_by, deleted_at=excluded.deleted_at,
           body=excluded.body, path=excluded.path",
        params![
            &entry.id,
            &entry.project,
            &entry.agent,
            entry.kind.as_str(),
            &entry.title,
            entry.summary(240),
            entry.tags_text(),
            entry.importance,
            if entry.pinned { 1_i64 } else { 0 },
            if entry.starred { 1_i64 } else { 0 },
            entry.created_at,
            entry.updated_at,
            &entry.superseded_by,
            entry.deleted_at,
            &entry.body,
            &entry.path,
        ],
    )?;
    if fts {
        let _ = conn.execute("DELETE FROM entries_fts WHERE id = ?1", params![entry.id]);
        conn.execute(
            "INSERT INTO entries_fts (id, text) VALUES (?1, ?2)",
            params![entry.id, normalize(&entry.search_text())],
        )?;
    }
    Ok(())
}

/// 全局单例。首次访问时打开索引；事实源与索引不一致由 `rebuild` 修正。
pub fn global() -> &'static MemoryIndex {
    static INDEX: OnceLock<MemoryIndex> = OnceLock::new();
    INDEX.get_or_init(MemoryIndex::open)
}

// ── 文本归一化与查询构造 ─────────────────────────────────────

/// 小写 + CJK 逐字加空格。索引与查询两侧必须用同一份，否则永远搜不到。
pub fn normalize(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + text.len() / 2);
    for ch in text.chars() {
        if is_cjk(ch) {
            out.push(' ');
            out.push(ch);
            out.push(' ');
        } else if ch.is_alphanumeric() || ch == '_' || ch == '-' {
            for lower in ch.to_lowercase() {
                out.push(lower);
            }
        } else {
            out.push(' ');
        }
    }
    out.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// 把用户输入切成检索单元：ASCII 词（保留原词）与 CJK 连续段（拆成单字）。
pub fn tokenize_query(query: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut ascii = String::new();
    let mut cjk = String::new();
    let flush_ascii = |buffer: &mut String, out: &mut Vec<String>| {
        if !buffer.is_empty() {
            out.push(buffer.to_lowercase());
            buffer.clear();
        }
    };
    let flush_cjk = |buffer: &mut String, out: &mut Vec<String>| {
        if !buffer.is_empty() {
            for ch in buffer.chars() {
                out.push(ch.to_string());
            }
            buffer.clear();
        }
    };
    for ch in query.chars() {
        if is_cjk(ch) {
            flush_ascii(&mut ascii, &mut tokens);
            cjk.push(ch);
        } else if ch.is_alphanumeric() || ch == '_' || ch == '-' {
            flush_cjk(&mut cjk, &mut tokens);
            ascii.push(ch);
        } else {
            flush_ascii(&mut ascii, &mut tokens);
            flush_cjk(&mut cjk, &mut tokens);
        }
    }
    flush_ascii(&mut ascii, &mut tokens);
    flush_cjk(&mut cjk, &mut tokens);
    tokens
}

/// 由检索单元拼 FTS5 表达式：ASCII 词做前缀匹配，CJK 相邻字做短语匹配。
///
/// 为什么 CJK 用短语而不是「AND 单字」：单字 AND 会让「记忆」命中一切同时含
/// 「记」和「忆」的正文（顺序与相邻都不管），噪音大到没法用。
fn fts_expression(tokens: &[String]) -> Option<String> {
    if tokens.is_empty() {
        return None;
    }
    let mut parts: Vec<String> = Vec::new();
    let mut run: Vec<String> = Vec::new();
    let flush_run = |run: &mut Vec<String>, parts: &mut Vec<String>| {
        if run.is_empty() {
            return;
        }
        if run.len() == 1 {
            parts.push(format!("\"{}\"", run[0]));
        } else {
            parts.push(format!("\"{}\"", run.join(" ")));
        }
        run.clear();
    };
    for token in tokens {
        let is_single_cjk = token.chars().count() == 1 && token.chars().all(is_cjk);
        if is_single_cjk {
            run.push(token.clone());
        } else {
            flush_run(&mut run, &mut parts);
            // 前缀匹配：搜「rout」也能命中「routing」
            parts.push(format!("\"{}\"*", token.replace('"', "")));
        }
    }
    flush_run(&mut run, &mut parts);
    if parts.is_empty() {
        None
    } else {
        Some(parts.join(" AND "))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_separates_cjk() {
        assert_eq!(normalize("记忆中枢 FTS5"), "记 忆 中 枢 fts5");
    }

    #[test]
    fn tokenize_splits_cjk_and_ascii() {
        let tokens = tokenize_query("记忆 FTS5 索引");
        assert!(tokens.contains(&"记".to_string()));
        assert!(tokens.contains(&"忆".to_string()));
        assert!(tokens.contains(&"fts5".to_string()));
    }

    #[test]
    fn fts_expression_groups_cjk_as_phrase() {
        let expression = fts_expression(&tokenize_query("记忆")).unwrap();
        assert_eq!(expression, "\"记 忆\"");
    }

    #[test]
    fn normalize_lowercases_ascii() {
        assert_eq!(normalize("Routing-Table"), "routing-table");
    }
}
