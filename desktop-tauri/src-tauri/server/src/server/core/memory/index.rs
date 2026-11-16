//! 记忆中枢 · 派生索引（可直接删掉重建）。
//!
//! ── 为什么索引进单独的库文件，而不进 `agent2api.db` ──────────
//! 索引是**纯派生数据**：删掉这个文件、调一次 [`rebuild`]，内容与删之前逐字
//! 相同。放进主库意味着「给所有存量用户推一次 schema 版本」，而这里任何一次
//! 索引逻辑改动都要再推一次 —— 派生数据没有资格占用事实库的版本号。
//! 独立文件同时给了用户一个最干脆的修复动作：删了它。
//!
//! ── 哪些表是「派生的」，哪些不是 ─────────────────────────────
//! `entries` / `entries_fts` 是派生的（事实源是 `.md`）。
//! `review_queue` / `recall_log` / `llm_call` / `agent_beat` / `sync_state`
//! 里那几张**不是**：待确认队列的裁决状态、调用台账、同步游标，删了就是丢数据。
//! 它们跟索引同库只是为了「一个文件搞定记忆模块的全部机读状态」；`rebuild`
//! 只清派生表，其余原样保留。
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
//!
//! ── 列自愈为什么必须写 ──────────────────────────────────────
//! 索引库是**有状态的磁盘文件**：用户升级到新版本时，老 `index.db` 里少几列。
//! `CREATE TABLE IF NOT EXISTS` 对已存在的表什么都不做，于是新代码一
//! `SELECT` 新列就整表读取失败 —— 表现是「升级后记忆页全空」。
//! 所以打开时拿 `PRAGMA table_info` 对一遍，缺哪列补哪列（`ALTER TABLE ADD
//! COLUMN` 只加不删，代价是几毫秒）。

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
    pub layer: String,
    pub title: String,
    pub summary: String,
    pub tags: Vec<String>,
    pub importance: f64,
    pub pinned: bool,
    pub starred: bool,
    pub created_at: i64,
    pub updated_at: i64,
    pub valid_to: Option<i64>,
    pub superseded_by: Option<String>,
    pub deleted_at: Option<i64>,
    pub dedup_status: String,
    pub access_count: i64,
    pub last_hit_at: Option<i64>,
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
            "layer": self.layer,
            "project": self.project,
            "agent": self.agent,
            "tags": self.tags,
            "importance": self.importance,
            "pinned": self.pinned,
            "starred": self.starred,
            "createdAt": self.created_at,
            "updatedAt": self.updated_at,
            "validTo": self.valid_to,
            "supersededBy": self.superseded_by,
            "dedupStatus": self.dedup_status,
            "accessCount": self.access_count,
            "lastHitAt": self.last_hit_at,
            "summary": self.summary,
            "path": self.path,
        })
    }
}

/// 待确认队列的一条建议（`review_queue` 表）。
#[derive(Debug, Clone)]
pub struct ReviewRow {
    pub id: i64,
    pub kind: String,
    pub target: String,
    pub payload: Value,
    pub confidence: f64,
    pub status: String,
    pub created_at: i64,
    pub resolved_at: Option<i64>,
    pub resolution: Option<String>,
}

impl ReviewRow {
    pub fn to_json(&self) -> Value {
        json!({
            "id": self.id,
            "type": self.kind,
            "target": self.target,
            "payload": self.payload,
            "confidence": self.confidence,
            "status": self.status,
            "createdAt": self.created_at,
            "resolvedAt": self.resolved_at,
            "resolution": self.resolution,
        })
    }
}

// ── 表结构 ───────────────────────────────────────────────────

/// `entries` 的列定义（顺序即写入顺序，`row_to_index` 按位置取值）。
///
/// 「索引表 vs 列自愈」两处都靠这张清单：新建库时按它建表，老库时按它补列。
const ENTRY_COLUMNS: &[(&str, &str)] = &[
    ("id", "TEXT PRIMARY KEY"),
    ("project", "TEXT"),
    ("agent", "TEXT"),
    ("kind", "TEXT"),
    ("layer", "TEXT"),
    ("title", "TEXT"),
    ("summary", "TEXT"),
    ("tags", "TEXT"),
    ("importance", "REAL DEFAULT 0.5"),
    ("pinned", "INTEGER DEFAULT 0"),
    ("starred", "INTEGER DEFAULT 0"),
    ("created_at", "INTEGER"),
    ("updated_at", "INTEGER"),
    ("valid_to", "INTEGER"),
    ("superseded_by", "TEXT"),
    ("deleted_at", "INTEGER"),
    ("dedup_status", "TEXT DEFAULT 'unique'"),
    ("access_count", "INTEGER DEFAULT 0"),
    ("last_hit_at", "INTEGER"),
    ("body", "TEXT"),
    ("path", "TEXT"),
];

/// `entries` 的 `SELECT` 列序（与 [`ENTRY_COLUMNS`] 前 20 项一致，去掉 body）。
const ENTRY_SELECT: &str = "id, project, agent, kind, layer, title, summary, tags, importance,
     pinned, starred, created_at, updated_at, valid_to, superseded_by, deleted_at,
     dedup_status, access_count, last_hit_at, path";

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
             PRAGMA synchronous=NORMAL;
             PRAGMA busy_timeout=5000;",
        );
        // ── 派生表 ──
        let mut create = String::from("CREATE TABLE IF NOT EXISTS entries (\n");
        for (index, (name, decl)) in ENTRY_COLUMNS.iter().enumerate() {
            create.push_str(&format!("  {name} {decl}"));
            if index + 1 < ENTRY_COLUMNS.len() {
                create.push(',');
            }
            create.push('\n');
        }
        create.push_str(");");
        if let Err(error) = conn.execute_batch(&create) {
            crate::server::logging::log("[Memory]", &format!("⚠️ 索引表创建失败：{error}"));
        }
        let _ = conn.execute_batch(
            "CREATE INDEX IF NOT EXISTS entries_project ON entries(project);
             CREATE INDEX IF NOT EXISTS entries_updated ON entries(updated_at DESC);
             CREATE INDEX IF NOT EXISTS entries_layer ON entries(layer);
             CREATE INDEX IF NOT EXISTS entries_hash ON entries(path);",
        );
        heal_columns(&conn);

        // ── 非派生表（rebuild 不清）──
        let _ = conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS review_queue (
               id INTEGER PRIMARY KEY AUTOINCREMENT,
               kind TEXT NOT NULL,            -- supersede / classify / dedup / conflict
               target TEXT NOT NULL,          -- 目标记忆 id 或项目 slug
               payload TEXT NOT NULL,         -- 建议内容（JSON）
               confidence REAL DEFAULT 0,
               status TEXT NOT NULL DEFAULT 'pending',  -- pending / approved / rejected / expired
               created_at INTEGER NOT NULL,
               resolved_at INTEGER,
               resolution TEXT
             );
             CREATE INDEX IF NOT EXISTS review_status ON review_queue(status, created_at DESC);

             CREATE TABLE IF NOT EXISTS recall_log (
               id INTEGER PRIMARY KEY AUTOINCREMENT,
               at INTEGER NOT NULL,
               tool TEXT NOT NULL,            -- search / get / core / digest / recent
               query TEXT,
               hits INTEGER DEFAULT 0,
               tokens INTEGER DEFAULT 0,
               agent TEXT,
               project TEXT
             );
             CREATE INDEX IF NOT EXISTS recall_at ON recall_log(at DESC);

             CREATE TABLE IF NOT EXISTS llm_call (
               id INTEGER PRIMARY KEY AUTOINCREMENT,
               at INTEGER NOT NULL,
               task TEXT NOT NULL,            -- extract / summarize / tag / classify / distill / profile …
               model TEXT,
               provider TEXT,
               prompt_tokens INTEGER DEFAULT 0,
               completion_tokens INTEGER DEFAULT 0,
               latency_ms INTEGER DEFAULT 0,
               ok INTEGER DEFAULT 1,
               error TEXT,
               note TEXT
             );
             CREATE INDEX IF NOT EXISTS llm_at ON llm_call(at DESC);

             CREATE TABLE IF NOT EXISTS agent_beat (
               agent TEXT PRIMARY KEY,
               last_seen INTEGER NOT NULL,
               calls INTEGER DEFAULT 0,
               last_tool TEXT,
               version TEXT,
               ok INTEGER DEFAULT 1
             );

             CREATE TABLE IF NOT EXISTS mem_link (
               from_id TEXT NOT NULL,
               to_id TEXT NOT NULL,
               kind TEXT NOT NULL DEFAULT 'related',  -- relates / cites / same_topic
               weight REAL DEFAULT 1.0,
               PRIMARY KEY (from_id, to_id, kind)
             );

             CREATE TABLE IF NOT EXISTS sync_state (
               key TEXT PRIMARY KEY,
               value TEXT NOT NULL,
               updated_at INTEGER NOT NULL
             );",
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

    /// 全量重建派生表：清表 → 按事实源逐条写入。
    ///
    /// 只清 `entries` / `entries_fts`：待确认队列、调用台账、同步游标不是派生
    /// 数据，重建索引不该把它们一起抹掉。
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
        let _ = conn.execute(
            "DELETE FROM mem_link WHERE from_id = ?1 OR to_id = ?1",
            params![id],
        );
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
            format!("SELECT {ENTRY_SELECT} FROM entries ORDER BY updated_at DESC")
        } else {
            format!("SELECT {ENTRY_SELECT} FROM entries WHERE deleted_at IS NULL ORDER BY updated_at DESC")
        };
        let mut statement = match conn.prepare(&sql) {
            Ok(statement) => statement,
            Err(error) => {
                crate::server::logging::log("[Memory]", &format!("⚠️ 索引查询失败：{error}"));
                return Vec::new();
            }
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
        let sql = format!(
            "SELECT {select}, bm25(entries_fts) AS rank
               FROM entries_fts
               JOIN entries e ON e.id = entries_fts.id
              WHERE entries_fts MATCH ?1 AND e.deleted_at IS NULL
              ORDER BY rank
              LIMIT 200",
            select = ENTRY_SELECT
                .split(',')
                .map(|column| format!("e.{}", column.trim()))
                .collect::<Vec<_>>()
                .join(", ")
        );
        let mut statement = conn.prepare(&sql).ok()?;
        let mapped = statement.query_map(params![expression], |row| {
            let mut item = row_to_index(row)?;
            item.rank = row.get::<_, f64>(20)?;
            Ok(item)
        });
        let rows: Vec<IndexRow> = mapped.ok()?.flatten().collect();
        Some(rows)
    }

    fn search_scan(&self, tokens: &[String]) -> Vec<IndexRow> {
        let Ok(conn) = self.conn.lock() else { return Vec::new() };
        let sql = format!("SELECT {ENTRY_SELECT}, body FROM entries WHERE deleted_at IS NULL");
        let mut statement = match conn.prepare(&sql) {
            Ok(statement) => statement,
            Err(_) => return Vec::new(),
        };
        let mapped = statement.query_map([], |row| {
            let mut item = row_to_index(row)?;
            let body: String = row.get::<_, Option<String>>(20)?.unwrap_or_default();
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

    // ── 关联图 ───────────────────────────────────────────────

    /// 与本批种子相连的一跳邻居（不含种子自己）。
    ///
    /// `search.weightGraph` 用它做一次性扩散：命中了「用 rustls 做 TLS」时，
    /// 和它相连的「证书链要 webpki-roots」也该出现在候选里 —— 这是纯词面检索
    /// 永远做不到的（两条没有任何共同词）。
    pub fn neighbors(&self, seeds: &[String]) -> Vec<String> {
        if seeds.is_empty() {
            return Vec::new();
        }
        let Ok(conn) = self.conn.lock() else { return Vec::new() };
        let mut out: Vec<String> = Vec::new();
        for seed in seeds {
            let mut statement = match conn.prepare(
                "SELECT to_id FROM mem_link WHERE from_id = ?1
                 UNION
                 SELECT from_id FROM mem_link WHERE to_id = ?1",
            ) {
                Ok(statement) => statement,
                Err(_) => continue,
            };
            // 先收成 Vec 再用：`rows` 借用 `statement`，如果借用活到 if-let 的
            // 块尾，`statement` 会先被析构（borrow checker 报错 E0597）。
            // 收集一次反而更便宜：这段本来就要遍历全部邻居。
            let ids: Vec<String> = match statement.query_map(params![seed], |row| row.get::<_, String>(0)) {
                Ok(rows) => rows.flatten().collect(),
                Err(_) => Vec::new(),
            };
            for id in ids {
                if !out.iter().any(|seen| seen == &id) {
                    out.push(id);
                }
            }
        }
        let depth = super::config::usize_of("search.graphExpansionDepth");
        let cap = super::config::usize_of("search.graphExpansionMax");
        // 深度 > 1 时再扩一轮；配置上限 3，这里按层迭代，但每层都受 `cap` 约束
        let mut frontier = out.clone();
        for _ in 1..depth {
            if frontier.is_empty() || out.len() >= cap {
                break;
            }
            let mut next: Vec<String> = Vec::new();
            for seed in &frontier {
                let mut statement = match conn.prepare(
                    "SELECT to_id FROM mem_link WHERE from_id = ?1
                     UNION
                     SELECT from_id FROM mem_link WHERE to_id = ?1",
                ) {
                    Ok(statement) => statement,
                    Err(_) => continue,
                };
                // 同上：先收集再遍历，避免借用活得比 statement 久
                let ids: Vec<String> =
                    match statement.query_map(params![seed], |row| row.get::<_, String>(0)) {
                        Ok(rows) => rows.flatten().collect(),
                        Err(_) => Vec::new(),
                    };
                for id in ids {
                    if !out.iter().any(|seen| seen == &id) && !next.iter().any(|seen| seen == &id) {
                        next.push(id);
                    }
                }
            }
            for id in &next {
                if out.len() >= cap {
                    break;
                }
                out.push(id.clone());
            }
            frontier = next;
        }
        out.truncate(cap.max(1));
        out
    }

    /// 写一条边（`mem_link`）。同一条边重复写只提升权重。
    pub fn link(&self, from: &str, to: &str, kind: &str) {
        if from == to || from.is_empty() || to.is_empty() {
            return;
        }
        let Ok(conn) = self.conn.lock() else { return };
        let _ = conn.execute(
            "INSERT INTO mem_link (from_id, to_id, kind, weight) VALUES (?1, ?2, ?3, 1.0)
             ON CONFLICT(from_id, to_id, kind) DO UPDATE SET weight = weight + 0.5",
            params![from, to, kind],
        );
    }

    // ── 去重 ─────────────────────────────────────────────────

    /// 按内容哈希找已存在的活条目（L1 精确去重）。
    ///
    /// 用 `entries` 里的 `id`+路径定位之后还要比对 `.md` 里的 `hash`，所以这里
    /// 只做「候选筛选」：真正的相等判定在 `dedup` 层用哈希比，避免索引过期时误判。
    pub fn by_hash(&self, hash: &str, exclude: &str) -> Vec<String> {
        if hash.len() < 8 {
            return Vec::new();
        }
        let Ok(conn) = self.conn.lock() else { return Vec::new() };
        // 索引里不存 hash 列（它是派生的派生），改用「摘要相同 + 同类型」粗筛，
        // 精确哈希比对交给调用方读文件。粗筛失败也不影响正确性。
        let mut statement = match conn.prepare(
            "SELECT id FROM entries
              WHERE deleted_at IS NULL AND superseded_by IS NULL AND id != ?1
                AND summary = (SELECT summary FROM entries WHERE id = ?1)
              LIMIT 20",
        ) {
            Ok(statement) => statement,
            Err(_) => return Vec::new(),
        };
        statement
            .query_map(params![exclude], |row| row.get::<_, String>(0))
            .map(|rows| rows.flatten().collect())
            .unwrap_or_default()
    }

    // ── 使用热度 ─────────────────────────────────────────────

    /// 记一次命中：累加 `entries.access_count` / `last_hit_at`。
    ///
    /// 只写索引不写 `.md`：热度是「使用痕迹」，不是记忆内容。写进事实源会让
    /// 每次检索都产生一次磁盘写 + 一次 git diff，而它的价值远不够那个代价。
    /// 索引重建时热度归零 —— 这是个有意的取舍，`search.weightHeat` 默认 0.1。
    pub fn hit(&self, ids: &[String]) {
        if ids.is_empty() {
            return;
        }
        let Ok(conn) = self.conn.lock() else { return };
        let now = crate::server::logging::now_ms();
        let _ = conn.execute_batch("BEGIN");
        for id in ids {
            let _ = conn.execute(
                "UPDATE entries SET access_count = access_count + 1, last_hit_at = ?2 WHERE id = ?1",
                params![id, now],
            );
        }
        let _ = conn.execute_batch("COMMIT");
    }

    /// 记一次检索调用（`recall_log`）。按条数上限滚动删除，不无限增长。
    pub fn log_recall(&self, tool: &str, query: &str, hits: usize, tokens: usize, agent: Option<&str>, project: Option<&str>) {
        let Ok(conn) = self.conn.lock() else { return };
        let now = crate::server::logging::now_ms();
        let _ = conn.execute(
            "INSERT INTO recall_log (at, tool, query, hits, tokens, agent, project)
             VALUES (?1,?2,?3,?4,?5,?6,?7)",
            params![now, tool, query, hits as i64, tokens as i64, agent, project],
        );
        let keep = super::config::usize_of("auto.logKeepCount").max(20) as i64;
        let _ = conn.execute(
            "DELETE FROM recall_log WHERE id NOT IN (SELECT id FROM recall_log ORDER BY id DESC LIMIT ?1)",
            params![keep],
        );
    }

    /// 记一次内部 LLM 调用（`llm_call`）。
    pub fn log_llm(&self, task: &str, model: &str, provider: &str, prompt: i64, completion: i64, latency: i64, ok: bool, error: Option<&str>, note: Option<&str>) {
        let Ok(conn) = self.conn.lock() else { return };
        let now = crate::server::logging::now_ms();
        let _ = conn.execute(
            "INSERT INTO llm_call (at, task, model, provider, prompt_tokens, completion_tokens, latency_ms, ok, error, note)
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10)",
            params![now, task, model, provider, prompt, completion, latency, if ok { 1_i64 } else { 0 }, error, note],
        );
    }

    /// 当日已消耗的 token（`auto.dailyTokenLimit` 的判据）。
    pub fn tokens_today(&self) -> i64 {
        let Ok(conn) = self.conn.lock() else { return 0 };
        let now = crate::server::logging::now_ms();
        let day_start = now - (now.rem_euclid(86_400_000));
        conn.query_row(
            "SELECT COALESCE(SUM(prompt_tokens + completion_tokens), 0) FROM llm_call WHERE at >= ?1",
            params![day_start],
            |row| row.get::<_, i64>(0),
        )
        .unwrap_or(0)
    }

    /// 最近若干条调用台账（自动化页面的「运行统计」）。
    pub fn recent_llm(&self, limit: usize) -> Vec<Value> {
        let Ok(conn) = self.conn.lock() else { return Vec::new() };
        let mut statement = match conn.prepare(
            "SELECT at, task, model, provider, prompt_tokens, completion_tokens, latency_ms, ok, error
               FROM llm_call ORDER BY id DESC LIMIT ?1",
        ) {
            Ok(statement) => statement,
            Err(_) => return Vec::new(),
        };
        let now = crate::server::logging::now_ms();
        statement
            .query_map(params![limit.min(500) as i64], |row| {
                let at: i64 = row.get(0)?;
                Ok(json!({
                    "at": at,
                    "atText": super::search::relative_time(at, now),
                    "task": row.get::<_, String>(1)?,
                    "model": row.get::<_, Option<String>>(2)?.unwrap_or_default(),
                    "provider": row.get::<_, Option<String>>(3)?.unwrap_or_default(),
                    "promptTokens": row.get::<_, i64>(4).unwrap_or(0),
                    "completionTokens": row.get::<_, i64>(5).unwrap_or(0),
                    "latencyMs": row.get::<_, i64>(6).unwrap_or(0),
                    "ok": row.get::<_, i64>(7).unwrap_or(1) != 0,
                    "error": row.get::<_, Option<String>>(8)?,
                }))
            })
            .map(|rows| rows.flatten().collect())
            .unwrap_or_default()
    }

    // ── 待确认队列 ───────────────────────────────────────────

    /// 入队。同一个 `(kind, target)` 已经有 pending 条目时不重复插入 ——
    /// 每次抽取都产生一条同样的建议会让队列迅速变成噪音。
    pub fn enqueue(&self, kind: &str, target: &str, payload: &Value, confidence: f64) -> Option<i64> {
        let Ok(conn) = self.conn.lock() else { return None };
        let existing: Option<i64> = conn
            .query_row(
                "SELECT id FROM review_queue WHERE kind = ?1 AND target = ?2 AND status = 'pending' LIMIT 1",
                params![kind, target],
                |row| row.get(0),
            )
            .ok();
        if let Some(id) = existing {
            // 已有待确认：用更高的置信度覆盖建议内容（同一目标的建议会更准）
            let _ = conn.execute(
                "UPDATE review_queue SET payload = ?2, confidence = MAX(confidence, ?3) WHERE id = ?1",
                params![id, payload.to_string(), confidence],
            );
            return Some(id);
        }
        let now = crate::server::logging::now_ms();
        conn.execute(
            "INSERT INTO review_queue (kind, target, payload, confidence, status, created_at)
             VALUES (?1,?2,?3,?4,'pending',?5)",
            params![kind, target, payload.to_string(), confidence, now],
        )
        .ok()?;
        Some(conn.last_insert_rowid())
    }

    pub fn review_list(&self, status: Option<&str>, kind: Option<&str>, limit: usize) -> Vec<ReviewRow> {
        let Ok(conn) = self.conn.lock() else { return Vec::new() };
        let sql = "SELECT id, kind, target, payload, confidence, status, created_at, resolved_at, resolution
                     FROM review_queue
                    WHERE (?1 IS NULL OR status = ?1) AND (?2 IS NULL OR kind = ?2)
                    ORDER BY CASE status WHEN 'pending' THEN 0 ELSE 1 END, confidence DESC, created_at DESC
                    LIMIT ?3";
        let mut statement = match conn.prepare(sql) {
            Ok(statement) => statement,
            Err(_) => return Vec::new(),
        };
        let mapped = statement.query_map(
            params![status, kind, limit.clamp(1, 500) as i64],
            |row| {
                let payload: String = row.get(3)?;
                Ok(ReviewRow {
                    id: row.get(0)?,
                    kind: row.get(1)?,
                    target: row.get(2)?,
                    payload: serde_json::from_str(&payload).unwrap_or(Value::Null),
                    confidence: row.get::<_, f64>(4).unwrap_or(0.0),
                    status: row.get(5)?,
                    created_at: row.get(6)?,
                    resolved_at: row.get(7)?,
                    resolution: row.get(8)?,
                })
            },
        );
        mapped.map(|rows| rows.flatten().collect()).unwrap_or_default()
    }

    pub fn review_count(&self) -> usize {
        let Ok(conn) = self.conn.lock() else { return 0 };
        conn.query_row(
            "SELECT COUNT(*) FROM review_queue WHERE status = 'pending'",
            [],
            |row| row.get::<_, i64>(0),
        )
        .map(|value| value as usize)
        .unwrap_or(0)
    }

    /// 裁决：`approved` / `rejected` / `expired`。返回受影响行数。
    pub fn review_resolve(&self, ids: &[i64], status: &str, resolution: Option<&str>) -> usize {
        if ids.is_empty() {
            return 0;
        }
        let Ok(conn) = self.conn.lock() else { return 0 };
        let now = crate::server::logging::now_ms();
        let mut changed = 0usize;
        for id in ids {
            changed += conn
                .execute(
                    "UPDATE review_queue SET status = ?2, resolved_at = ?3, resolution = ?4
                      WHERE id = ?1 AND status = 'pending'",
                    params![id, status, now, resolution],
                )
                .unwrap_or(0);
        }
        changed
    }

    /// 一键清空 pending（「全部拒绝」）。
    pub fn review_reject_all(&self, kind: Option<&str>) -> usize {
        let Ok(conn) = self.conn.lock() else { return 0 };
        let now = crate::server::logging::now_ms();
        match kind {
            Some(kind) => conn
                .execute(
                    "UPDATE review_queue SET status = 'rejected', resolved_at = ?1, resolution = 'batch'
                      WHERE status = 'pending' AND kind = ?2",
                    params![now, kind],
                )
                .unwrap_or(0),
            None => conn
                .execute(
                    "UPDATE review_queue SET status = 'rejected', resolved_at = ?1, resolution = 'batch'
                      WHERE status = 'pending'",
                    params![now],
                )
                .unwrap_or(0),
        }
    }

    /// 过期清理：pending 超过 N 天的建议自动作废（用户长期不看的建议没有价值）。
    pub fn review_expire(&self, days: usize) -> usize {
        let Ok(conn) = self.conn.lock() else { return 0 };
        let cutoff = crate::server::logging::now_ms() - days.max(1) as i64 * 24 * 3600 * 1000;
        conn.execute(
            "UPDATE review_queue SET status = 'expired', resolved_at = ?1, resolution = 'timeout'
              WHERE status = 'pending' AND created_at < ?2",
            params![crate::server::logging::now_ms(), cutoff],
        )
        .unwrap_or(0)
    }

    // ── Agent 心跳 ───────────────────────────────────────────

    pub fn beat(&self, agent: &str, tool: &str, ok: bool) {
        let Ok(conn) = self.conn.lock() else { return };
        let now = crate::server::logging::now_ms();
        let _ = conn.execute(
            "INSERT INTO agent_beat (agent, last_seen, calls, last_tool, ok) VALUES (?1, ?2, 1, ?3, ?4)
             ON CONFLICT(agent) DO UPDATE SET
               last_seen = excluded.last_seen,
               calls = calls + 1,
               last_tool = excluded.last_tool,
               ok = excluded.ok",
            params![agent, now, tool, if ok { 1_i64 } else { 0 }],
        );
    }

    pub fn beats(&self) -> Vec<Value> {
        let Ok(conn) = self.conn.lock() else { return Vec::new() };
        let mut statement = match conn.prepare(
            "SELECT agent, last_seen, calls, last_tool, version, ok FROM agent_beat ORDER BY last_seen DESC",
        ) {
            Ok(statement) => statement,
            Err(_) => return Vec::new(),
        };
        let now = crate::server::logging::now_ms();
        statement
            .query_map([], |row| {
                let last_seen: i64 = row.get(1)?;
                Ok(json!({
                    "agent": row.get::<_, String>(0)?,
                    "lastSeen": last_seen,
                    "lastSeenText": super::search::relative_time(last_seen, now),
                    "calls": row.get::<_, i64>(2).unwrap_or(0),
                    "lastTool": row.get::<_, Option<String>>(3)?,
                    "version": row.get::<_, Option<String>>(4)?,
                    "ok": row.get::<_, i64>(5).unwrap_or(1) != 0,
                    "alive": now - last_seen < 3_600_000,
                }))
            })
            // 这里返回 Value 数组而不是切片：调用方直接塞进 JSON 响应
            .map(|rows| rows.flatten().collect())
            .unwrap_or_default()
    }

    // ── 同步游标 ─────────────────────────────────────────────

    pub fn sync_set(&self, key: &str, value: &str) {
        let Ok(conn) = self.conn.lock() else { return };
        let now = crate::server::logging::now_ms();
        let _ = conn.execute(
            "INSERT INTO sync_state (key, value, updated_at) VALUES (?1, ?2, ?3)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value, updated_at = excluded.updated_at",
            params![key, value, now],
        );
    }

    pub fn sync_get(&self, key: &str) -> Option<(String, i64)> {
        let Ok(conn) = self.conn.lock() else { return None };
        conn.query_row(
            "SELECT value, updated_at FROM sync_state WHERE key = ?1",
            params![key],
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?)),
        )
        .ok()
    }

    /// 全部表与行数（状态页 / 自检）
    pub fn table_stats(&self) -> Value {
        let Ok(conn) = self.conn.lock() else { return json!({}) };
        let mut out = serde_json::Map::new();
        for table in ["entries", "review_queue", "recall_log", "llm_call", "agent_beat", "mem_link", "sync_state"] {
            let count: i64 = conn
                .query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |row| row.get(0))
                .unwrap_or(0);
            out.insert(table.to_string(), json!(count));
        }
        Value::Object(out)
    }
}

/// 老库补列：`PRAGMA table_info` 对一遍 [`ENTRY_COLUMNS`]，缺的 `ALTER TABLE ADD COLUMN`。
fn heal_columns(conn: &Connection) {
    let Ok(mut statement) = conn.prepare("PRAGMA table_info(entries)") else { return };
    let existing: Vec<String> = statement
        .query_map([], |row| row.get::<_, String>(1))
        .map(|rows| rows.flatten().collect())
        .unwrap_or_default();
    if existing.is_empty() {
        return;
    }
    for (name, decl) in ENTRY_COLUMNS {
        if existing.iter().any(|item| item == name) {
            continue;
        }
        // `PRIMARY KEY` 不能 ADD COLUMN；只有 id 会带它，而 id 必然已存在
        if decl.contains("PRIMARY KEY") {
            continue;
        }
        let sql = format!("ALTER TABLE entries ADD COLUMN {name} {decl}");
        if let Err(error) = conn.execute(&sql, []) {
            crate::server::logging::log(
                "[Memory]",
                &format!("⚠️ 索引补列失败 {name}：{error}"),
            );
        }
    }
}

fn row_to_index(row: &rusqlite::Row<'_>) -> rusqlite::Result<IndexRow> {
    let tags: String = row.get::<_, Option<String>>(7)?.unwrap_or_default();
    Ok(IndexRow {
        id: row.get(0)?,
        project: row.get(1)?,
        agent: row.get(2)?,
        kind: row.get::<_, Option<String>>(3)?.unwrap_or_else(|| "other".into()),
        layer: row.get::<_, Option<String>>(4)?.unwrap_or_else(|| "l1".into()),
        title: row.get::<_, Option<String>>(5)?.unwrap_or_default(),
        summary: row.get::<_, Option<String>>(6)?.unwrap_or_default(),
        tags: tags.split_whitespace().map(|tag| tag.to_string()).collect(),
        importance: row.get::<_, Option<f64>>(8)?.unwrap_or(0.5),
        pinned: row.get::<_, Option<i64>>(9)?.unwrap_or(0) != 0,
        starred: row.get::<_, Option<i64>>(10)?.unwrap_or(0) != 0,
        created_at: row.get::<_, Option<i64>>(11)?.unwrap_or(0),
        updated_at: row.get::<_, Option<i64>>(12)?.unwrap_or(0),
        valid_to: row.get(13)?,
        superseded_by: row.get(14)?,
        deleted_at: row.get(15)?,
        dedup_status: row
            .get::<_, Option<String>>(16)?
            .unwrap_or_else(|| "unique".into()),
        access_count: row.get::<_, Option<i64>>(17)?.unwrap_or(0),
        last_hit_at: row.get(18)?,
        path: row.get::<_, Option<String>>(19)?.unwrap_or_default(),
        rank: 0.0,
        hits: 0,
    })
}

fn write_row(conn: &Connection, entry: &MemoryEntry, fts: bool) -> rusqlite::Result<()> {
    conn.execute(
        "INSERT INTO entries
           (id, project, agent, kind, layer, title, summary, tags, importance, pinned, starred,
            created_at, updated_at, valid_to, superseded_by, deleted_at, dedup_status,
            access_count, last_hit_at, body, path)
         VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17,?18,?19,?20,?21)
         ON CONFLICT(id) DO UPDATE SET
           project=excluded.project, agent=excluded.agent, kind=excluded.kind,
           layer=excluded.layer,
           title=excluded.title, summary=excluded.summary, tags=excluded.tags,
           importance=excluded.importance, pinned=excluded.pinned, starred=excluded.starred,
           created_at=excluded.created_at, updated_at=excluded.updated_at,
           valid_to=excluded.valid_to,
           superseded_by=excluded.superseded_by, deleted_at=excluded.deleted_at,
           dedup_status=excluded.dedup_status,
           body=excluded.body, path=excluded.path",
        params![
            &entry.id,
            &entry.project,
            &entry.agent,
            entry.kind.as_str(),
            entry.layer.as_str(),
            &entry.title,
            entry.summary(240),
            entry.tags_text(),
            entry.importance,
            if entry.pinned { 1_i64 } else { 0 },
            if entry.starred { 1_i64 } else { 0 },
            entry.created_at,
            entry.updated_at,
            entry.valid_to,
            &entry.superseded_by,
            entry.deleted_at,
            entry.dedup_status.as_str(),
            entry.access_count as i64,
            entry.last_hit_at,
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
        parts.push(format!("\"{}\"", run.join(" ")));
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

    /// 列自愈：老库里缺的列必须能补上，否则升级后整页空白。
    #[test]
    fn missing_columns_are_added_back() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE entries (
               id TEXT PRIMARY KEY, project TEXT, agent TEXT, kind TEXT,
               title TEXT, summary TEXT, tags TEXT, importance REAL,
               pinned INTEGER, starred INTEGER, created_at INTEGER, updated_at INTEGER,
               superseded_by TEXT, deleted_at INTEGER, body TEXT, path TEXT
             );",
        )
        .unwrap();
        heal_columns(&conn);
        let mut statement = conn.prepare("PRAGMA table_info(entries)").unwrap();
        let names: Vec<String> = statement
            .query_map([], |row| row.get::<_, String>(1))
            .unwrap()
            .flatten()
            .collect();
        for required in ["layer", "valid_to", "dedup_status", "access_count", "last_hit_at"] {
            assert!(names.iter().any(|name| name == required), "缺列 {required}：{names:?}");
        }
    }

    #[test]
    fn review_queue_does_not_duplicate_pending() {
        // 用内存库直接验证「同 kind + 同 target 只留一条 pending」
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE review_queue (
               id INTEGER PRIMARY KEY AUTOINCREMENT, kind TEXT, target TEXT, payload TEXT,
               confidence REAL, status TEXT, created_at INTEGER, resolved_at INTEGER, resolution TEXT
             );",
        )
        .unwrap();
        let insert = |payload: &str, confidence: f64| {
            let existing: Option<i64> = conn
                .query_row(
                    "SELECT id FROM review_queue WHERE kind='dedup' AND target='m1' AND status='pending'",
                    [],
                    |row| row.get(0),
                )
                .ok();
            if let Some(id) = existing {
                conn.execute(
                    "UPDATE review_queue SET payload=?2, confidence=MAX(confidence,?3) WHERE id=?1",
                    params![id, payload, confidence],
                )
                .unwrap();
                return id;
            }
            conn.execute(
                "INSERT INTO review_queue (kind,target,payload,confidence,status,created_at) VALUES ('dedup','m1',?1,?2,'pending',1)",
                params![payload, confidence],
            )
            .unwrap();
            conn.last_insert_rowid()
        };
        let first = insert("a", 0.6);
        let second = insert("b", 0.9);
        assert_eq!(first, second);
        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM review_queue", [], |row| row.get(0))
            .unwrap();
        assert_eq!(count, 1);
        let (payload, confidence): (String, f64) = conn
            .query_row("SELECT payload, confidence FROM review_queue", [], |row| {
                Ok((row.get(0)?, row.get(1)?))
            })
            .unwrap();
        assert_eq!(payload, "b");
        assert!((confidence - 0.9).abs() < 1e-9);
    }
}