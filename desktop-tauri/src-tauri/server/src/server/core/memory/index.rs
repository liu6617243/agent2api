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
//! `review_queue` / `recall_log` / `llm_call` / `agent_beat` / `sync_state` /
//! `heat` 里那几张**不是**：待确认队列的裁决状态、调用台账、同步游标、使用
//! 热度，删了就是丢数据。它们跟索引同库只是为了「一个文件搞定记忆模块的全部
//! 机读状态」；`rebuild` 只清派生表，其余原样保留。
//!
//! `heat` 为什么必须单开一张表（而不是只写在 `entries` 里）：`rebuild` 是
//! 「删光 entries 再按 `.md` 重写」，而热度不写 `.md`（见 [`MemoryIndex::hit`]），
//! 于是每次重建都把「哪条被读过 30 次」抹成 0。这不是理论问题：注入与检索
//! 都会打热度，而索引重建是常规操作 —— 结果就是 `search.weightHeat` 这一项
//! 永远等于「最近一次重建之后被读过的次数」。`heat` 表重建后按 id 回填，
//! 热度才真的是累计值。
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
    /// 被人判定为错的时间。`Some` = 这条不能当事实用（见 `MemoryEntry::is_refuted`）。
    pub refuted_at: Option<i64>,
    pub refuted_reason: String,
    pub refuted_by: Option<String>,
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
            "refutedAt": self.refuted_at,
            "refutedReason": self.refuted_reason,
            "refutedBy": self.refuted_by,
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

/// `entries` 的列定义（顺序即写入顺序；读取按列名，见 [`row_to_index`]）。
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
    // 「被人判定为错」三列。`refuted_at` 要能进 WHERE（检索默认跳过），所以
    // 必须是独立列而不是塞进 body；`refuted_reason` 给列表行用（界面显示
    // 「为什么错」），`refuted_by` 给演化链用。
    ("refuted_at", "INTEGER"),
    ("refuted_reason", "TEXT"),
    ("refuted_by", "TEXT"),
    ("body", "TEXT"),
    ("path", "TEXT"),
];

/// `entries` 的 `SELECT` 列序（与 [`ENTRY_COLUMNS`] 一致，去掉 body）。
///
/// ── 为什么列序必须与 [`ENTRY_COLUMNS`] 严格对齐 ────────────────
/// 下面的 `row_to_entry` 是按**下标**取列的。两处一旦错位，取出来的值会串位
/// （`refuted_at` 拿到 `access_count` 的数字），而且不会报错 —— 症状是「分数
/// 怪怪的」「有人的记忆莫名被标成错了」。加列必须同时改这三处：
/// `ENTRY_COLUMNS` / `ENTRY_SELECT` / `upsert` 的 `INSERT` 与 `params![]`。
const ENTRY_SELECT: &str = "id, project, agent, kind, layer, title, summary, tags, importance,
     pinned, starred, created_at, updated_at, valid_to, superseded_by, deleted_at,
     dedup_status, access_count, last_hit_at, refuted_at, refuted_reason, refuted_by, path";

pub struct MemoryIndex {
    conn: Mutex<Connection>,
    /// FTS5 虚拟表是否建得起来
    fts: bool,
}

/// 两次执行是否可以合并成一行：任务、状态、明细三项全等。
///
/// ── 为什么是模块级自由函数，不是 `impl MemoryIndex` 的方法 ────────
/// 它不碰连接、不碰 `self`，只是三个字段的比较。放进 `impl` 里会带来两个
/// 后果：一是调用点必须写 `Self::`（写成 `same_run(..)` 直接 `E0425`），
/// 二是 `mod tests` 的 `use super::*` 看不到关联函数，必须再写一遍
/// `MemoryIndex::same_run` —— 一条纯判据没必要让两处都记着它挂在哪。
///
/// 抽成独立函数是为了能单测：这条判据写错的后果是「不同任务的记录互相覆盖」，
/// 而它埋在一条 SQL 里时只能靠读代码发现。
fn same_run(
    last_task: &str,
    last_status: &str,
    last_detail: Option<&str>,
    task: &str,
    status: &str,
    detail: Option<&str>,
) -> bool {
    last_task == task && last_status == status && last_detail == detail
}

/// 这个工具是不是「写」。
///
/// ── 为什么用白名单而不是按名字猜 ────────────────────────────
/// 「写 / 读」这两个字用户是当**后果**看的：说「写了 17 次」意味着记忆库被改了
/// 17 次。猜错的代价是让人误判自己的数据被动过。所以只有明确落盘的几个算写，
/// 其余（含 `memory_status` 这种纯查询、`memory_review_list` 这种读队列）都算读。
fn is_write_tool(tool: &str) -> bool {
    matches!(
        tool,
        "memory_write"
            | "memory_update"
            | "memory_forget"
            | "memory_digest"
            | "memory_core"
            | "memory_review_resolve"
    )
}

/// 按 `auto.logKeepCount` 把 `recall_log` 滚到上限之内。
///
/// 抽成独立函数是因为有两个写入点：[`MemoryIndex::log_recall`]（MCP 检索）
/// 与 [`MemoryIndex::trace_inject`]（请求级注入）。两处各写一遍删除语句，迟早
/// 出现「一处改了上限、另一处没改」—— 而症状是「注入写了一堆台账把检索记录
/// 挤出窗口」，从现象完全看不出是上限口径不一致。
///
/// ── 为什么不用 `id NOT IN (SELECT id ... LIMIT n)`（2026-11-24）──
/// 那个写法要把整张表读一遍、再逐行判断「在不在那 n 行里」。注入接入之后
/// 台账变成**每请求一行**，这个写法就是每请求一次全表扫。
///
/// `id <= (SELECT id ... LIMIT 1 OFFSET n)` 沿主键索引走 n+1 行就停：
/// 它取到第 n+1 新的一行，把比它老的全删掉，留下的正好是最新的 n 行。
/// 不足 n 行时子查询返回 `NULL`，什么都不删 —— 语义与原来一致。
fn prune_recall_log(conn: &Connection) {
    let keep = super::config::usize_of("auto.logKeepCount").max(20) as i64;
    let _ = conn.execute(
        "DELETE FROM recall_log WHERE id <= (SELECT id FROM recall_log ORDER BY id DESC LIMIT 1 OFFSET ?1)",
        params![keep],
    );
}

/// 本地自然日零点的毫秒时间戳。
///
/// ── 为什么不能用 `now - now % 86400000` ─────────────────────
/// 那是 **UTC** 零点。东八区用户在本地 0:00–8:00 之间看「今日消耗 / 今日预算」，
/// 读到的是**昨天**的数字：昨天已经烧完的预算，会一直显示「今日已用完」到早上
/// 8 点才恢复 —— 而他这一整天其实一次模型都没调。`mod.rs::daily_counts` 早就
/// 因为同一个坑改成按本地自然日切，这里漏了。
fn local_day_start(now: i64) -> i64 {
    use chrono::{Local, TimeZone};
    // 夏令时空档会拿到 None（`and_local_timezone` 的已知行为）；
    // 那种情况退化成「按 24h 回退」，宁可差一小时也不要整块算错。
    Local
        .timestamp_millis_opt(now)
        .single()
        .and_then(|moment| moment.date_naive().and_hms_opt(0, 0, 0))
        .and_then(|at| at.and_local_timezone(Local).single())
        .map(|at| at.timestamp_millis())
        .unwrap_or_else(|| now - now.rem_euclid(86_400_000))
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
             PRAGMA busy_timeout=5000;
             PRAGMA wal_autocheckpoint=512;",
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
               tool TEXT NOT NULL,            -- search / get / core / digest / recent / inject
               query TEXT,
               hits INTEGER DEFAULT 0,
               tokens INTEGER DEFAULT 0,
               agent TEXT,
               project TEXT
             );
             CREATE INDEX IF NOT EXISTS recall_at ON recall_log(at DESC);

             -- 使用热度：**不是**派生表，rebuild 不清它。
             --
             -- 只记「被读过几次 / 最后一次什么时候」，不记内容 —— 它是使用痕迹，
             -- 不是记忆。放在单独一张表里的唯一理由：`entries` 每次重建都会整表
             -- 重写，而热度不写 `.md`，混在里面就必然被抹成 0（详见文件头）。
             CREATE TABLE IF NOT EXISTS heat (
               id TEXT PRIMARY KEY,
               hits INTEGER NOT NULL DEFAULT 0,
               last_hit_at INTEGER
             );

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
               ok INTEGER DEFAULT 1,
               writes INTEGER DEFAULT 0,
               reads INTEGER DEFAULT 0,
               errors INTEGER DEFAULT 0
             );

             CREATE TABLE IF NOT EXISTS mem_link (
               from_id TEXT NOT NULL,
               to_id TEXT NOT NULL,
               kind TEXT NOT NULL DEFAULT 'related',  -- relates / cites / same_topic
               weight REAL DEFAULT 1.0,
               PRIMARY KEY (from_id, to_id, kind)
             );

             -- 反向查邻居必须走索引。`PRIMARY KEY (from_id, to_id, kind)` 只给
             -- `from_id` 前缀建了索引，`neighbors()` 里那一半
             -- `SELECT from_id FROM mem_link WHERE to_id = ?1` 是**全表扫**——
             -- 表小的时候看不出，810 万行时单种子要扫几秒。这条索引让双向查询
             -- 都走 B 树。
             CREATE INDEX IF NOT EXISTS mem_link_to ON mem_link(to_id);

             CREATE TABLE IF NOT EXISTS sync_state (
               key TEXT PRIMARY KEY,
               value TEXT NOT NULL,
               updated_at INTEGER NOT NULL
             );

             -- 同步冲突：本地与远端同时改了同一个文件，且都不同于上次同步的基线。
             -- 不自动裁决 —— 记忆是长期事实，猜错一次的代价远大于多问一句。
             --
             -- `local_hash` / `remote_hash` 可以为 NULL：NULL 表示**这一侧没有这个
             -- 文件**（删了），而不是「哈希算不出来」。裁决「保留本地」时基线要记
             -- 远端那一侧，所以远端体积也得存（只靠哈希算不出来）。
             CREATE TABLE IF NOT EXISTS sync_conflict (
               id INTEGER PRIMARY KEY AUTOINCREMENT,
               path TEXT NOT NULL,
               device TEXT NOT NULL,        -- 远端设备 id
               local_hash TEXT,
               remote_hash TEXT,
               remote_size INTEGER,
               base_hash TEXT,
               detail TEXT,
               status TEXT NOT NULL DEFAULT 'pending',  -- pending / resolved / ignored
               created_at INTEGER NOT NULL,
               resolved_at INTEGER,
               resolution TEXT
             );
             CREATE INDEX IF NOT EXISTS sync_conflict_status ON sync_conflict(status, created_at DESC);

             -- 同步流水：谁在什么时候推了什么、拉了什么
             CREATE TABLE IF NOT EXISTS sync_log (
               id INTEGER PRIMARY KEY AUTOINCREMENT,
               at INTEGER NOT NULL,
               device TEXT,
               action TEXT NOT NULL,        -- pull / push / conflict / skip / error
               detail TEXT,
               ok INTEGER DEFAULT 1
             );
             CREATE INDEX IF NOT EXISTS sync_log_at ON sync_log(at DESC);

             -- 任务执行审计：每次自动化任务跑完（含跳过 / 失败）落一行。
             --
             -- ── 为什么不复用 llm_call ────────────────────────────
             -- 两者是一对多的关系，不是同一件事：`llm_call` 记「这一批里调了几次
             -- 模型、各花了多少 token」，`task_run` 记「这一轮任务整体跑了多久、
             -- 处理了几条、结没成功」。零 token 的任务（索引对账、清理回收站、
             -- 记忆体检）根本不产生 llm_call，但用户同样要知道它们跑过没有 ——
             -- 只看 llm_call 会得到一张「这些任务从没运行过」的假报表。
             --
             -- `tokens` 是这一轮该任务所有模型调用的合计（零 token 任务为 0），
             -- 冗余存一份是为了让「任务执行记录」那张表不用 join 两张表、也不用
             -- 在时间窗口上做近似匹配（两次调用的时间戳与任务开始时间必然不同）。
             CREATE TABLE IF NOT EXISTS task_run (
               id INTEGER PRIMARY KEY AUTOINCREMENT,
               at INTEGER NOT NULL,
               task TEXT NOT NULL,
               status TEXT NOT NULL,          -- done / skipped / failed
               detail TEXT,                   -- 一句人能读的结果摘要
               tokens INTEGER DEFAULT 0,
               latency_ms INTEGER DEFAULT 0,
               manual INTEGER DEFAULT 0       -- 1 = 用户点「立即执行」
             );
             CREATE INDEX IF NOT EXISTS task_run_at ON task_run(at DESC);
             CREATE INDEX IF NOT EXISTS task_run_task ON task_run(task, at DESC);",
        );
        // 老库补列：`repeat` 是后加的（见 `log_task_run` 的「连续相同结果合并」）。
        // SQLite 的 `ALTER TABLE ADD COLUMN` 不支持 `IF NOT EXISTS`，所以先问
        // `PRAGMA table_info`。放在 `execute_batch` 外面：批次里有一条失败，
        // 后面全不执行。
        let has_repeat: bool = conn
            .prepare("PRAGMA table_info(task_run)")
            .and_then(|mut statement| {
                let names: Vec<String> = statement
                    .query_map([], |row| row.get::<_, String>(1))
                    .map(|rows| rows.flatten().collect())
                    .unwrap_or_default();
                Ok(names.iter().any(|name| name == "repeat"))
            })
            .unwrap_or(false);
        if !has_repeat {
            let _ = conn.execute_batch("ALTER TABLE task_run ADD COLUMN repeat INTEGER DEFAULT 1;");
        }

        // 老库补列：`agent_beat` 的 writes / reads / errors 是后加的。
        //
        // 「共 30 次」这一句在页面上不够用 —— 用户真正想知道的是「它在写、在读，
        // 还是在报错」。三列都从 0 起算：老库里历史调用的分类补不回来（当时没记
        // 工具名的那部分只能算总数），所以这三列是「从这次升级之后的分布」，
        // 页面文案要说清，不能让人以为它等于 calls。
        let beat_columns: Vec<String> = conn
            .prepare("PRAGMA table_info(agent_beat)")
            .and_then(|mut statement| {
                statement
                    .query_map([], |row| row.get::<_, String>(1))
                    .map(|rows| rows.flatten().collect())
            })
            .unwrap_or_default();
        for column in ["writes", "reads", "errors"] {
            if !beat_columns.iter().any(|name| name == column) {
                let _ = conn.execute_batch(&format!(
                    "ALTER TABLE agent_beat ADD COLUMN {column} INTEGER DEFAULT 0;"
                ));
            }
        }

        // 老库补列：`sync_conflict.remote_size` 是后加的（裁决「保留本地」时基线
        // 要记远端那一侧的体积）。老记录没有它，补成 NULL —— 那时裁决会退回
        // 「体积未知按 0」，只影响同步预览里显示的大小，不影响同步判定。
        let conflict_columns: Vec<String> = conn
            .prepare("PRAGMA table_info(sync_conflict)")
            .and_then(|mut statement| {
                statement
                    .query_map([], |row| row.get::<_, String>(1))
                    .map(|rows| rows.flatten().collect())
            })
            .unwrap_or_default();
        if !conflict_columns.iter().any(|name| name == "remote_size") {
            let _ = conn.execute_batch("ALTER TABLE sync_conflict ADD COLUMN remote_size INTEGER;");
        }

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
        // 建完表就地自查一次。此刻单例还没发布、除这条连接外没人能碰这个文件，
        // 所以哪怕 `VACUUM` 要几十秒也不会让任何请求排队 —— 这是唯一安全的时机。
        // 正常机器上 `freelist_count` 是 0（或很小），这里只是一次 PRAGMA。
        if let Some(note) = compact_connection(&conn) {
            crate::server::logging::log("[Memory]", &format!("🧹 {note}"));
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
    /// 只清 `entries` / `entries_fts`：待确认队列、调用台账、同步游标、使用
    /// 热度不是派生数据，重建索引不该把它们一起抹掉。
    ///
    /// ── 为什么写完要把 `heat` join 回去（2026-11-24）─────────────
    /// `access_count` / `last_hit_at` 在 `entries` 里，但它的真源是 `heat`
    /// （见 [`Self::hit`] 的说明：热度不写 `.md`，而重建是「清空再按 `.md`
    /// 重写」）。不回填的话，重建一次全库热度归零 —— 那正是修这个问题的原因。
    ///
    /// 回填在**同一个事务里**：重建中途失败时不能出现「行写好了但热度没跟上」
    /// 的半成品；一起回滚，读到的就是重建前的完整状态。
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
        // 热度回填：`heat` 是「不是派生」的那一侧，重建只读它、不动它。
        //
        // `MAX(...)` 而不是直接覆盖：老版本曾经把热度写进 `.md`（`access_count`
        // 是 `MemoryEntry` 的字段），那些行里的数字可能比 `heat` 还大。取两者
        // 较大的一个，升级不会被「回填」成更小的数 —— 热度只能涨，不该因为
        // 换了个存法而倒退。
        //
        // 失败不致命：最坏结果是这一轮热度退化为 `.md` 里记的值，而不是整次
        // 重建失败（索引是派生数据，可用性优先）。
        let _ = tx.execute(
            "UPDATE entries SET
               access_count = MAX(COALESCE(access_count, 0), COALESCE((SELECT hits FROM heat WHERE heat.id = entries.id), 0)),
               last_hit_at  = COALESCE((SELECT last_hit_at FROM heat WHERE heat.id = entries.id), last_hit_at)",
            [],
        );
        tx.commit().map_err(|error| error.to_string())?;
        // 重建是唯一「先整表清空、再整表写满」的操作，也是空闲页的主要来源。
        // 写完顺手压一遍：判据在 `compact_connection` 里，正常情况下它什么都不做。
        if let Some(note) = compact_connection(&conn) {
            crate::server::logging::log("[Memory]", &format!("🧹 {note}"));
        }
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

    /// 从索引里彻底移除一条（**真删除**，不是进回收站）。
    ///
    /// ── 为什么连 `heat` 一起删 ──────────────────────────────────
    /// `heat` 是「不随 rebuild 清空」的表（见 [`Self::hit`]），所以它的行不会
    /// 自己消失 —— 删了条目却留着热度的直接后果是「同 id 重写一条新记忆，
    /// 它一出生就带着 30 次阅读量」。两处调用方（`purge` 与清理任务的「文件
    /// 不存在」）都是「这条真的没了」，一起删才对。
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
        let _ = conn.execute("DELETE FROM heat WHERE id = ?1", params![id]);
    }

    /// 库里现有条目数（状态自检用）
    pub fn count(&self) -> usize {
        self.try_count().unwrap_or(0)
    }

    /// 同 [`count`]，但**不排队**：拿不到锁就返回 `None`。
    ///
    /// ── 为什么首屏必须用这一版 ──────────────────────────────────
    /// 全局只有一条连接（`Mutex<Connection>`）。`rebuild()` 与打开时的 `VACUUM`
    /// 都会独占它几十秒，`lock()` 会让所有读请求排在那条队列后面 —— 那正是
    /// 「记忆中枢整页打不开、一直等到 60 秒超时」的形状（2026-11-21 实测
    /// `index.db` 3.0 GB / 94.5% 是空闲页，打开就要压一遍）。
    /// 首屏宁可少一个数字（页面显示「整理中」），也不能让整页挂住。
    pub fn try_count(&self) -> Option<usize> {
        let conn = self.conn.try_lock().ok()?;
        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM entries", [], |row| row.get(0))
            .ok()?;
        Some(count as usize)
    }

    /// 把 WAL 收回去（只截断，不做 `VACUUM`）。
    ///
    /// ── 为什么需要它 ────────────────────────────────────────────
    /// 索引是 WAL 模式写的，而 WAL 只会在「没有别的读者」时被自动回收。
    /// 导入这种「几千条连续写 + 页面同时在读」的场景里，自动 checkpoint
    /// 经常落不了地，文件就一路长：2026-11-21 实测 `index.db-wal` 到了
    /// **700 MB**，每次读都要翻这本越来越厚的日志索引。
    ///
    /// 所以在一轮批量写**结束时**显式收一次 —— 那时读者刚好是空闲的，
    /// 截断几乎必然成功。这条很便宜（一次 `PRAGMA`），可以在热路径上调。
    pub fn checkpoint(&self) {
        let Ok(conn) = self.conn.lock() else { return };
        let _ = conn.execute_batch("PRAGMA wal_checkpoint(TRUNCATE);");
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
            // 按列名取：`bm25()` 的别名是 `rank`，不随 ENTRY_SELECT 变。
            // 原来写死 `row.get(20)`，加列后读到的是 `refuted_reason`（TEXT），
            // `get::<f64>` 会失败，整条 FTS 结果被 `flatten()` 静默丢掉 ——
            // 症状是「搜索突然什么都搜不到」，而且没有报错。
            item.rank = row.get::<_, f64>("rank")?;
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
            // 同上：按列名取 `body`。写死下标后加列会让这里读到 `refuted_reason`，
            // 扫描检索就会拿「为什么错」当正文匹配，命中结果全是假的。
            let body: String = row.get::<_, Option<String>>("body")?.unwrap_or_default();
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

    /// 这两个 id 之间是否已经有一条边（任意方向、任意类型）。
    ///
    /// ── 为什么需要「任意类型」而不是只查 `same_topic` ──────────────
    /// 调用方（`dedup_scan`）要回答的是「这一对判过没有」，而判过的痕迹有三种
    /// 落法：`relate_pair` 连 `same_topic`、`apply_dedup` 连 `relates`、
    /// `relink` 按共享标签连 `same_topic`。第三种是**自动生成的**，不代表判过
    /// —— 但它同样意味着这两条已经在互相带出，再入队一次也只是多问一遍。
    /// 所以判据取「有没有边」，由调用方叠加 `Related` 状态一起判：只靠边会把
    /// 新配对（relink 早就连上了、但没人判过）永久挡掉。
    pub fn linked(&self, left: &str, right: &str) -> bool {
        if left.is_empty() || right.is_empty() || left == right {
            return false;
        }
        let Ok(conn) = self.conn.lock() else { return false };
        conn.query_row(
            "SELECT 1 FROM mem_link
              WHERE (from_id = ?1 AND to_id = ?2) OR (from_id = ?2 AND to_id = ?1)
              LIMIT 1",
            params![left, right],
            |_| Ok(()),
        )
        .is_ok()
    }

    /// 清空整张关联图。
    ///
    /// ── 为什么需要它 ────────────────────────────────────────────
    /// `mem_link` 是**派生**数据（由 `relink()` 按当前条目重算），但 `rebuild()`
    /// 只清 `entries` / `entries_fts`，从不清它。于是每次重建都是「在旧图上叠新图」：
    /// 老实现按「任意共同标签」连线，5818 条带 `导入` 标签的记忆两两相连，重建一次
    /// 就把 810 万行写进去；再重建一次还要在那个基数上继续 `weight + 0.5`。
    ///
    /// 表涨到 810 万行之后，`table_stats()` 的 `SELECT COUNT(*)` 要扫几百毫秒，
    /// 而它挂在 `status()` 里 —— 每打开一次记忆中枢都要付一次。这就是「读取记忆库
    /// 有点长」的第二个来源。
    ///
    /// 重建前清空是安全的：`relink()` 紧接着会把该有的边全部重算一遍。清空之后
    /// 图比之前**更小也更准**，因为泛标签已经不再参与连线。
    pub fn clear_links(&self) {
        let Ok(conn) = self.conn.lock() else { return };
        let _ = conn.execute("DELETE FROM mem_link", []);
    }

    /// 关联图里有多少条边。启动自检用它判断「这张表是不是被撑爆了」。
    pub fn link_count(&self) -> usize {
        let Ok(conn) = self.conn.lock() else { return 0 };
        conn.query_row("SELECT COUNT(*) FROM mem_link", [], |row| row.get::<_, i64>(0))
            .map(|value| value as usize)
            .unwrap_or(0)
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

    /// 记一次命中：累加 `heat.hits` / 刷新 `heat.last_hit_at`。
    ///
    /// ── 为什么热度写 `heat` 表而不写 `entries`（2026-11-24）───────
    /// 原来这里 `UPDATE entries SET access_count = ...`。这看着很自然 —— 热度
    /// 就是条目的一个字段。但它和 [`Self::rebuild`] 直接冲突：`rebuild` 是
    /// 「`DELETE FROM entries` 再按 `.md` 逐条重写」，而热度**故意**不写
    /// `.md`（写进去等于每次检索都产生一次磁盘写 + 一次 git diff）。于是
    /// 「重建索引」这个常规操作会把全库热度抹成 0。
    ///
    /// 这不是理论问题。真机数据：`access_count > 0` 的条目 **0 条**，
    /// 而同期 `search` / `get` 调用几十次 —— 每次重建都归零，而重建是排障第
    /// 一步。`search.weightHeat`（默认 0.1）这一项于是恒等于「最近一次重建之后
    /// 被读过的次数」，等于没有这一项。
    ///
    /// 单开一张 `heat` 表，让「条目内容」与「使用痕迹」各有各的生命周期：
    /// `entries` 跟着 `.md` 走（可整表重建），`heat` 跟着索引库走（只在回收站
    /// 清空时按 id 删）。`rebuild()` 结尾把两份数据 join 回去，热度就真的是
    /// 累计值。
    ///
    /// ── 为什么不是 `INSERT OR REPLACE` ─────────────────────────
    /// 同一次请求里两条 id 可能重复（`scored` 与画像段各算一遍），用
    /// `hits = hits + 1` 才是「读了几次」；`REPLACE` 会把重复写成 1。
    pub fn hit(&self, ids: &[String]) {
        if ids.is_empty() {
            return;
        }
        let Ok(conn) = self.conn.lock() else { return };
        let now = crate::server::logging::now_ms();
        let _ = conn.execute_batch("BEGIN");
        for id in ids {
            // `ON CONFLICT` 而不是 `INSERT OR REPLACE`：后者会先 DELETE 再 INSERT，
            // `hits` 归 1，而且多一次写放大。
            let _ = conn.execute(
                "INSERT INTO heat (id, hits, last_hit_at) VALUES (?1, 1, ?2)
                 ON CONFLICT(id) DO UPDATE SET hits = hits + 1, last_hit_at = ?2",
                params![id, now],
            );
            // `entries` 是派生的一侧：`search` 直接读它，同步写一份就不必为了
            // 一个字段去 join。真源仍是 `heat`（重建时会从它回填）。
            let _ = conn.execute(
                "UPDATE entries SET access_count = access_count + 1, last_hit_at = ?2 WHERE id = ?1",
                params![id, now],
            );
        }
        let _ = conn.execute_batch("COMMIT");
    }

    /// 记一次注入的使用痕迹：热度 + 检索台账，**一次加锁、失败就放弃**。
    ///
    /// ── 为什么单独一个方法，而不是调 `hit()` + `log_recall()` ─────
    /// 两件事要落在同一个临界区里：只写了热度没写台账，`recall_log` 里仍然
    /// 看不到注入；只写了台账没记热度，`weightHeat` 那一项仍然是死的。
    ///
    /// ── 为什么用 `try_lock` 而不是 `lock`（关键）───────────────
    /// 这条路径挂在**每一条转发请求**前面。全局只有一条索引连接，
    /// [`Self::rebuild`] 会独占它几十秒（3 GB 的 `index.db` 实测过）。用
    /// `lock()` 的话，重建期间所有转发请求都排在这把锁后面 —— 那正是
    /// 「记忆中枢整页打不开、一直等到 60 秒超时」那个事故的形状。
    ///
    /// 丢一行使用痕迹的代价，和卡住一次用户请求的代价，不是一个量级。所以
    /// 索引忙就放弃这一条，返回 `false` 让调用方记一笔「这次没留痕」——
    /// 不静默吞掉，否则「注入 137 次 / 台账只有 3 行」又变成新的无解问题。
    pub fn trace_inject(
        &self,
        ids: &[String],
        query: &str,
        tokens: usize,
        agent: Option<&str>,
        project: Option<&str>,
    ) -> bool {
        let Ok(conn) = self.conn.try_lock() else {
            return false;
        };
        let now = crate::server::logging::now_ms();
        let _ = conn.execute_batch("BEGIN");
        for id in ids {
            let _ = conn.execute(
                "INSERT INTO heat (id, hits, last_hit_at) VALUES (?1, 1, ?2)
                 ON CONFLICT(id) DO UPDATE SET hits = hits + 1, last_hit_at = ?2",
                params![id, now],
            );
            let _ = conn.execute(
                "UPDATE entries SET access_count = access_count + 1, last_hit_at = ?2 WHERE id = ?1",
                params![id, now],
            );
        }
        let _ = conn.execute(
            "INSERT INTO recall_log (at, tool, query, hits, tokens, agent, project)
             VALUES (?1,'inject',?2,?3,?4,?5,?6)",
            params![now, query, ids.len() as i64, tokens as i64, agent, project],
        );
        // 裁剪放在同一个事务里：注入每请求一行，不裁剪的话台账会一直涨；
        // 放在事务外就是两次写、两次提交 —— 这条路径挂在每个转发请求前面。
        prune_recall_log(&conn);
        let _ = conn.execute_batch("COMMIT");
        true
    }

    /// 记一次检索调用（`recall_log`）。按条数上限滚动删除，不无限增长。
    ///
    /// `tool` 的取值到目前为止是 `search` / `get` / `core` / `digest` / `recent`
    /// / `inject`。最后那个是 2026-11-24 加的：请求级注入是全套机制里唯一
    /// **自动**读记忆的入口，不记它等于「记忆被用了」这条链上少一半证据。
    pub fn log_recall(&self, tool: &str, query: &str, hits: usize, tokens: usize, agent: Option<&str>, project: Option<&str>) {
        let Ok(conn) = self.conn.lock() else { return };
        let now = crate::server::logging::now_ms();
        let _ = conn.execute(
            "INSERT INTO recall_log (at, tool, query, hits, tokens, agent, project)
             VALUES (?1,?2,?3,?4,?5,?6,?7)",
            params![now, tool, query, hits as i64, tokens as i64, agent, project],
        );
        prune_recall_log(&conn);
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

    /// 记一次任务执行（`task_run`）。
    ///
    /// 与 [`Self::log_llm`] 分开落表：一次任务可能调 0 次或 N 次模型，而
    /// 「跑没跑、跑成什么样」是任务级的结论。表在 `index.rs` 里建，
    /// 写入点是 `schedule::run_once` 的出口 —— 那里是唯一知道「这一轮整体
    /// 耗时多少、产出了什么」的地方。
    ///
    /// ── 连续相同结果的合并 ──────────────────────────────────
    /// `classify` 每小时跑一次，没东西可归的时候每次都跳过、理由一字不差。
    /// 每跳过一次就落一行的话，一天 24 行全是「没有未归类的记忆」，把真正
    /// 有信息量的那几行挤出「最近 30 条」。所以：**当最后一行是同任务、
    /// 同状态、同明细**时，不新增行，只把最后一行的 `at` 推到本次、`repeat +1`。
    /// 只比最后一行（不是全表），这样「跳过 → 成功 → 跳过」不会被合并成
    /// 一条，顺序与次数都还在。
    pub fn log_task_run(&self, task: &str, status: &str, detail: Option<&str>, tokens: i64, latency: i64, manual: bool) {
        let Ok(conn) = self.conn.lock() else { return };
        let now = crate::server::logging::now_ms();
        // 最后一行（同任务、同状态、同明细）→ 合并到它上面。
        let previous: Option<(i64, String, String, Option<String>)> = conn
            .query_row(
                "SELECT id, task, status, detail FROM task_run ORDER BY id DESC LIMIT 1",
                [],
                |row| {
                    Ok((
                        row.get::<_, i64>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, Option<String>>(3)?,
                    ))
                },
            )
            .ok();
        if let Some((id, last_task, last_status, last_detail)) = previous {
            // 三项全等才合并。任务名也要比 —— 两个任务的「跳过」理由可能
            // 一字不差（都是「没有未归类的记忆」那种通用句），只比状态与
            // 明细会把甲任务的记录挂到乙任务名下。
            if same_run(&last_task, &last_status, last_detail.as_deref(), task, status, detail) {
                let _ = conn.execute(
                    // `at` 推到本次（这行的意思是「最近一次是什么时候」），
                    // `repeat` 记次数，`tokens` 累加（跳过恒 0，成功则每次都
                    // 真花了），耗时取本次。
                    "UPDATE task_run
                        SET at = ?1, repeat = repeat + 1, tokens = tokens + ?2, latency_ms = ?3, manual = ?4
                      WHERE id = ?5",
                    params![now, tokens.max(0), latency.max(0), if manual { 1_i64 } else { 0 }, id],
                );
                return;
            }
        }
        let _ = conn.execute(
            "INSERT INTO task_run (at, task, status, detail, tokens, latency_ms, manual, repeat)
             VALUES (?1,?2,?3,?4,?5,?6,?7,1)",
            params![now, task, status, detail, tokens.max(0), latency.max(0), if manual { 1_i64 } else { 0 }],
        );
        // 滚动删除：与 `recall_log` 同一口径，靠 `auto.logKeepCount` 一档配置
        // 收两条表的上限。任务日志比检索日志稀（一轮 tick 最多 12 行），
        // 用同一个上限不会把用户真正想看的历史挤掉。
        let keep = super::config::usize_of("auto.logKeepCount").max(20) as i64;
        let _ = conn.execute(
            "DELETE FROM task_run WHERE id NOT IN (SELECT id FROM task_run ORDER BY id DESC LIMIT ?1)",
            params![keep],
        );
    }

    /// 最近的执行记录（「任务执行审计」那张表）。
    pub fn recent_task_runs(&self, limit: usize) -> Vec<Value> {
        let Ok(conn) = self.conn.lock() else { return Vec::new() };
        let mut statement = match conn.prepare(
            "SELECT at, task, status, detail, tokens, latency_ms, manual, repeat
               FROM task_run ORDER BY id DESC LIMIT ?1",
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
                    "status": row.get::<_, String>(2)?,
                    "detail": row.get::<_, Option<String>>(3)?.unwrap_or_default(),
                    "tokens": row.get::<_, i64>(4).unwrap_or(0),
                    "latencyMs": row.get::<_, i64>(5).unwrap_or(0),
                    "manual": row.get::<_, i64>(6).unwrap_or(0) != 0,
                    // 这一行代表「连续 N 次同样的结果」。1 = 普通的一行。
                    "repeat": row.get::<_, i64>(7).unwrap_or(1),
                }))
            })
            .map(|rows| rows.flatten().collect())
            .unwrap_or_default()
    }

    /// 执行记录**行数**（「查看全部 N 条」里的 N —— 翻页翻的是行）。
    pub fn task_run_count(&self) -> i64 {
        let Ok(conn) = self.conn.lock() else { return 0 };
        conn.query_row("SELECT COUNT(*) FROM task_run", [], |row| row.get::<_, i64>(0))
            .unwrap_or(0)
    }

    /// 执行**次数**（`repeat` 求和）。合并过以后不等于行数：一行可能代表
    /// 24 次「同样的结果」。与任务行「累计 N 次」同一口径。
    pub fn task_run_executions(&self) -> i64 {
        let Ok(conn) = self.conn.lock() else { return 0 };
        conn.query_row("SELECT COALESCE(SUM(COALESCE(repeat, 1)), 0) FROM task_run", [], |row| row.get::<_, i64>(0))
            .unwrap_or(0)
    }

    /// 每个任务**上次执行**的时刻（epoch ms）。
    ///
    /// ── 为什么「上次」要从库里读，而不是只存内存 ────────────────
    /// 调度器原先只把 `last` 放在内存里，进程重启即归零。后果有两个：
    /// · 页面重启后整列显示「还没跑过」—— 明明昨天 11:00 跑过；
    /// · 按天配时刻的任务在「启动即补跑」之后，到点又跑一次（一天两遍）。
    /// `task_run` 本来就有 `at`，直接取 `MAX(at)` 即可，不必另建一张状态表。
    ///
    /// 一次 `GROUP BY` 取全量：调度线程启动时调一次，不在每轮 tick 里查。
    pub fn task_last_runs(&self) -> std::collections::HashMap<String, i64> {
        let mut out = std::collections::HashMap::new();
        let Ok(conn) = self.conn.lock() else { return out };
        let mut statement = match conn.prepare("SELECT task, MAX(at) FROM task_run GROUP BY task") {
            Ok(statement) => statement,
            Err(_) => return out,
        };
        if let Ok(rows) = statement.query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1).unwrap_or(0)))
        }) {
            for (task, at) in rows.flatten() {
                out.insert(task, at);
            }
        }
        out
    }

    /// 每个任务的累计统计：跑了几次、成功几次、跳过几次、一共烧了多少 token。
    ///
    /// ── 口径 ────────────────────────────────────────────────
    /// `runs` 数的是**所有状态**（done / skipped / failed）的行 —— 用户问
    /// 「这个任务跑过几次」时，跳过的那几次同样是跑过的；把它们剔掉会让
    /// 「从没跑过」与「跑了但一直跳过」看起来一样。
    ///
    /// `ok` 只数 `done`，`skipped` 单列。**成功率的分母是 `runs - skipped`**：
    /// 跳过不是失败（没素材 / 待确认积压 / 记忆中枢关着都是常态）。早先分母
    /// 用 `runs`，于是一个每小时都正常跳过 `classify` 报 0% —— 用户会以为它
    /// 坏了，实际是「扫过了，没什么可归的」。反过来说，`runs - skipped == 0`
    /// 时成功率没有意义，界面该说「全部跳过」而不是「0%」。
    ///
    /// ── 为什么要乘 `repeat` ─────────────────────────────────
    /// 连续相同结果会合并成一行（见 [`Self::log_task_run`]），表里一行可能
    /// 代表 24 次执行。按行数统计会把「每小时跑一次、整天都在跳过」报成
    /// 「跑了 1 次」，用户据此判断「这个任务是不是没在跑」就会判错。
    pub fn task_stats(&self) -> std::collections::HashMap<String, (i64, i64, i64, i64)> {
        let mut out = std::collections::HashMap::new();
        let Ok(conn) = self.conn.lock() else { return out };
        let mut statement = match conn.prepare(
            "SELECT task, COALESCE(SUM(COALESCE(repeat, 1)), 0),
                    COALESCE(SUM(CASE WHEN status = 'done' THEN COALESCE(repeat, 1) ELSE 0 END), 0),
                    COALESCE(SUM(CASE WHEN status = 'skipped' THEN COALESCE(repeat, 1) ELSE 0 END), 0),
                    COALESCE(SUM(tokens), 0)
               FROM task_run GROUP BY task",
        ) {
            Ok(statement) => statement,
            Err(_) => return out,
        };
        let rows = statement.query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, i64>(1).unwrap_or(0),
                row.get::<_, i64>(2).unwrap_or(0),
                row.get::<_, i64>(3).unwrap_or(0),
                row.get::<_, i64>(4).unwrap_or(0),
            ))
        });
        if let Ok(rows) = rows {
            for (task, runs, ok, skipped, tokens) in rows.flatten() {
                out.insert(task, (runs, ok, skipped, tokens));
            }
        }
        out
    }

    /// 当日已消耗的 token（`auto.dailyTokenLimit` 的判据）。
    pub fn tokens_today(&self) -> i64 {
        let Ok(conn) = self.conn.lock() else { return 0 };
        let day_start = local_day_start(crate::server::logging::now_ms());
        conn.query_row(
            "SELECT COALESCE(SUM(prompt_tokens + completion_tokens), 0) FROM llm_call WHERE at >= ?1",
            params![day_start],
            |row| row.get::<_, i64>(0),
        )
        .unwrap_or(0)
    }

    /// 按天清理 `llm_call` / `task_run`（`auto.logKeepDays`）。
    ///
    /// ── 为什么除了条数上限还要有天数上限 ──────────────────────
    /// `auto.logKeepCount` 是「滚动保留最近 N 条」：一周不看，N 条里可能全挤在
    /// 同一天 —— 半年前那次失败的调用还在，昨天之前的历史却已经被挤掉了。
    /// 这两个上限管的是两件不同的事：条数防「表无限长」，天数防「老记录赖着」。
    ///
    /// 返回删掉的行数（两张表合计），调用方拿它决定要不要在日志里提一句。
    pub fn purge_logs_by_age(&self, days: usize) -> usize {
        if days == 0 {
            return 0;
        }
        let Ok(conn) = self.conn.lock() else { return 0 };
        let now = crate::server::logging::now_ms();
        let cutoff = now - (days as i64) * 86_400_000;
        let mut removed = 0usize;
        for table in ["llm_call", "task_run"] {
            match conn.execute(&format!("DELETE FROM {table} WHERE at < ?1"), params![cutoff]) {
                Ok(count) => removed += count,
                Err(error) => crate::server::logging::log(
                    "[Memory]",
                    &format!("⚠️ 按天清理 {table} 失败：{error}"),
                ),
            }
        }
        removed
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

    /// 按 id 精确取一条（含已裁决的）。
    ///
    /// ── 为什么不能用 `review_list` 代取 ──────────────────────────
    /// `review_list` 按 `confidence DESC` 排序并截断：队列积压到几百条时，一条刚
    /// 入队的低分建议（比如 0.64 的归类）会排在 500 名之后，于是「入队后立刻问
    /// 一次能不能自动裁决」永远查不到自己那一行 —— 自动裁决在那批建议上静默失效，
    /// 而失效的表现只是「队列没清」，看起来像判据太严，不像 bug。
    pub fn review_get(&self, id: i64) -> Option<ReviewRow> {
        let Ok(conn) = self.conn.lock() else { return None };
        conn.query_row(
            "SELECT id, kind, target, payload, confidence, status, created_at, resolved_at, resolution
               FROM review_queue WHERE id = ?1",
            params![id],
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
        )
        .ok()
    }

    /// pending 条数。索引正忙（`try_lock` 失败）时给 0，不排队。
    pub fn review_count(&self) -> usize {
        self.try_review_count().unwrap_or(0)
    }

    /// 同 [`review_count`]，不排队（首屏自检用，理由见 [`try_count`]）。
    pub fn try_review_count(&self) -> Option<usize> {
        let conn = self.conn.try_lock().ok()?;
        let count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM review_queue WHERE status = 'pending'",
                [],
                |row| row.get(0),
            )
            .ok()?;
        Some(count as usize)
    }

    /// 某类建议的 pending 条数。
    ///
    /// ── 为什么必须能分类计数（2026-11-24）────────────────────────
    /// 归类巡检原来用 [`review_count`]（**全部** pending）和
    /// `dedup.pendingWarnThreshold` 比，本意是"队列积压成这样，先别再加新建议了"。
    /// 但队列是所有建议共用的一张表：真机 52 条 pending 里 44 条是 dedup、
    /// 6 条 classify、2 条 supersede —— 归类巡检每轮都看到"积压 52 条"，
    /// 于是永久跳过，而那些 dedup 积压跟它一点关系都没有。
    ///
    /// **一种建议的积压让另一种任务的巡检停止**，这是判据用错了对象。所以积压
    /// 闸必须按 kind 看：`classify` 只该被 `classify` 自己的积压拦住。
    pub fn review_count_of(&self, kind: &str) -> usize {
        let Ok(conn) = self.conn.lock() else { return 0 };
        conn.query_row(
            "SELECT COUNT(*) FROM review_queue WHERE status = 'pending' AND kind = ?1",
            params![kind],
            |row| row.get::<_, i64>(0),
        )
        .map(|count| count as usize)
        .unwrap_or(0)
    }

    /// 按 kind 统计全部 pending（排障与界面用）。
    ///
    /// 一次 `GROUP BY` 拿到全部，比每个任务各查一遍便宜 —— 调用方通常想同时
    /// 知道"我这类的积压"和"别处有没有堵着"。
    pub fn review_count_by_kind(&self) -> std::collections::BTreeMap<String, usize> {
        let mut out = std::collections::BTreeMap::new();
        let Ok(conn) = self.conn.lock() else { return out };
        let Ok(mut statement) = conn.prepare(
            "SELECT kind, COUNT(*) FROM review_queue WHERE status = 'pending' GROUP BY kind",
        ) else {
            return out;
        };
        let rows = statement.query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
        });
        let Ok(rows) = rows else { return out };
        for row in rows.flatten() {
            out.insert(row.0, row.1.max(0) as usize);
        }
        out
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
        // 归类在 Rust 侧做，不写进 SQL：`IN (...)` 拼字符串既难读又容易被工具名
        // 里的引号咬到。失败优先计入 errors —— 一次失败的写不该同时算「写成功」，
        // 否则「写 N / 错误 M」两个数字会互相矛盾。
        let write = ok && is_write_tool(tool);
        let read = ok && !write;
        let _ = conn.execute(
            "INSERT INTO agent_beat
               (agent, last_seen, calls, last_tool, ok, writes, reads, errors)
             VALUES (?1, ?2, 1, ?3, ?4, ?5, ?6, ?7)
             ON CONFLICT(agent) DO UPDATE SET
               last_seen = excluded.last_seen,
               calls = calls + 1,
               last_tool = excluded.last_tool,
               ok = excluded.ok,
               writes = writes + excluded.writes,
               reads = reads + excluded.reads,
               errors = errors + excluded.errors",
            params![
                agent,
                now,
                tool,
                if ok { 1_i64 } else { 0 },
                i64::from(write),
                i64::from(read),
                i64::from(!ok)
            ],
        );
    }

    pub fn beats(&self) -> Vec<Value> {
        self.try_beats().unwrap_or_default()
    }

    /// 同 [`beats`]，不排队（首屏自检用，理由见 [`try_count`]）。
    pub fn try_beats(&self) -> Option<Vec<Value>> {
        let conn = self.conn.try_lock().ok()?;
        let mut statement = conn
            .prepare(
                "SELECT agent, last_seen, calls, last_tool, version, ok, writes, reads, errors
               FROM agent_beat ORDER BY last_seen DESC",
            )
            .ok()?;
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
                    // 分类计数从升级之后起算，可能小于 calls（历史行没分类）
                    "writes": row.get::<_, i64>(6).unwrap_or(0),
                    "reads": row.get::<_, i64>(7).unwrap_or(0),
                    "errors": row.get::<_, i64>(8).unwrap_or(0),
                    "alive": now - last_seen < 3_600_000,
                }))
            })
            // 这里返回 Value 数组而不是切片：调用方直接塞进 JSON 响应
            .map(|rows| rows.flatten().collect())
            .ok()
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

    /// 记一条同步冲突。同一个路径重复冲突时更新旧记录而不是不断追加 ——
    /// 用户没处理之前，同一个文件的冲突记录应该只有一条。
    ///
    /// 哈希参数是 `Option<&str>`：`None` = 这一侧没有这个文件（删了）。表里这两列
    /// 本来就是 `TEXT`（可空），存 `NULL` 不用改 schema；而界面上「本地 / 远端」
    /// 那一行要能显示「已删除」，所以必须能区分「没有」和「哈希算不出来」。
    ///
    /// `remote_size` 与 `remote` 同生共死：`remote` 是 `None` 时它没有意义，传 0。
    /// 之所以要单独带体积：裁决「保留本地」时基线要记**远端**那一版，而基线里的
    /// 条目是 `{hash, size}` —— 只存哈希，那一版在同步预览里会显示成 0 字节。
    #[allow(clippy::too_many_arguments)]
    pub fn conflict_enqueue(
        &self,
        path: &str,
        device: &str,
        local: Option<&str>,
        remote: Option<&str>,
        remote_size: u64,
        base: Option<&str>,
        detail: &str,
    ) {
        let Ok(conn) = self.conn.lock() else { return };
        let now = crate::server::logging::now_ms();
        // 同一路径的旧 pending 记录先清掉：用户在「待确认」里看到的应该是一条，
        // 不是每次同步多一行几乎一样的
        let _ = conn.execute(
            "DELETE FROM sync_conflict WHERE path = ?1 AND device = ?2 AND status = 'pending'",
            params![path, device],
        );
        let _ = conn.execute(
            "INSERT INTO sync_conflict
               (path, device, local_hash, remote_hash, remote_size, base_hash, detail, status, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, 'pending', ?8)",
            params![path, device, local, remote, remote_size as i64, base, detail, now],
        );
    }

    pub fn conflict_list(&self, limit: usize) -> Vec<Value> {
        let Ok(conn) = self.conn.lock() else { return Vec::new() };
        // `query_map` 挂在 `Statement` 上，不挂在 `Connection` 上：先 prepare 再查
        let Ok(mut statement) = conn.prepare(
            "SELECT id, path, device, local_hash, remote_hash, base_hash, detail, status, created_at, remote_size
             FROM sync_conflict ORDER BY status = 'pending' DESC, created_at DESC LIMIT ?1",
        ) else {
            return Vec::new();
        };
        let mapped = statement.query_map(params![limit.max(1) as i64], |row| {
            Ok(json!({
                "id": row.get::<_, i64>(0)?,
                "path": row.get::<_, String>(1)?,
                "device": row.get::<_, String>(2)?,
                "localHash": row.get::<_, Option<String>>(3)?,
                "remoteHash": row.get::<_, Option<String>>(4)?,
                "baseHash": row.get::<_, Option<String>>(5)?,
                "detail": row.get::<_, Option<String>>(6)?,
                "status": row.get::<_, String>(7)?,
                "createdAt": row.get::<_, i64>(8)?,
                // 老记录没有这一列值时是 NULL，按 0 给 —— 界面只说「体积未知」，
                // 不用它做判断。
                "remoteSize": row.get::<_, Option<i64>>(9)?.unwrap_or(0),
            }))
        });
        match mapped {
            Ok(rows) => rows.flatten().collect(),
            Err(_) => Vec::new(),
        }
    }

    pub fn conflict_count(&self) -> usize {
        let Ok(conn) = self.conn.lock() else { return 0 };
        conn.query_row(
            "SELECT COUNT(*) FROM sync_conflict WHERE status = 'pending'",
            [],
            |row| row.get::<_, i64>(0),
        )
        .unwrap_or(0) as usize
    }

    /// **全部**待裁决冲突的路径（不截断）。
    ///
    /// 为什么不能拿 [`conflict_list`] 的结果当全集：那个接口带 `LIMIT`，是给
    /// 界面列表用的。而基线更新必须覆盖**每一条**待裁决的路径 —— 漏掉一条，
    /// 它的本地哈希就会留在基线里，下一轮三方比对得出「本地没变、远端变了」，
    /// 于是静默采纳远端、把还没裁决的冲突覆盖掉。冲突一多（>200 条）就会整片
    /// 出现这种静默覆盖，而界面上还挂着那些「待裁决」条目。
    ///
    /// 这里只取 `path`：调用方要的就是路径集合。
    pub fn conflict_pending_paths(&self) -> Vec<String> {
        let Ok(conn) = self.conn.lock() else { return Vec::new() };
        let Ok(mut statement) = conn
            .prepare("SELECT DISTINCT path FROM sync_conflict WHERE status = 'pending'")
        else {
            return Vec::new();
        };
        let mapped = statement.query_map([], |row| row.get::<_, String>(0));
        match mapped {
            Ok(rows) => rows.flatten().collect(),
            Err(_) => Vec::new(),
        }
    }

    /// 裁决一条冲突。`resolution` = `local` / `remote` / `ignore`。
    pub fn conflict_resolve(&self, id: i64, resolution: &str) -> usize {
        let Ok(conn) = self.conn.lock() else { return 0 };
        let now = crate::server::logging::now_ms();
        conn.execute(
            "UPDATE sync_conflict SET status = 'resolved', resolution = ?2, resolved_at = ?3
             WHERE id = ?1 AND status = 'pending'",
            params![id, resolution, now],
        )
        .unwrap_or(0)
    }

    /// 把某条路径上**还挂着**的待裁决冲突改成「已自动拼接」。
    ///
    /// 场景：上一轮同步这条进了冲突队列（两边都改、内容不同），这一轮恰好能按节
    /// 拼平 —— 冲突在逻辑上已经不存在了，但队列里那行还在。不主动关掉的话它会
    /// 一直显示「待裁决」，用户去点「保留本地」会把刚拼好的合并结果覆盖成半份。
    ///
    /// `resolution` 记成 `merged` 而不是 `resolved`：同步流水与冲突表里能区分
    /// 「人裁决的」与「自动拼掉的」，排查时不用猜。
    pub fn conflict_resolve_merged(&self, path: &str) -> usize {
        let Ok(conn) = self.conn.lock() else { return 0 };
        let now = crate::server::logging::now_ms();
        conn.execute(
            "UPDATE sync_conflict SET status = 'resolved', resolution = 'merged', resolved_at = ?2
             WHERE path = ?1 AND status = 'pending'",
            params![path, now],
        )
        .unwrap_or(0)
    }

    /// 冲突详情（裁决「用远端」时要把远端内容落盘，得先拿到它）。
    pub fn conflict_get(&self, id: i64) -> Option<Value> {
        let Ok(conn) = self.conn.lock() else { return None };
        conn.query_row(
            "SELECT id, path, device, local_hash, remote_hash, base_hash, detail, status, created_at, remote_size
             FROM sync_conflict WHERE id = ?1",
            params![id],
            |row| {
                Ok(json!({
                    "id": row.get::<_, i64>(0)?,
                    "path": row.get::<_, String>(1)?,
                    "device": row.get::<_, String>(2)?,
                    "localHash": row.get::<_, Option<String>>(3)?,
                    "remoteHash": row.get::<_, Option<String>>(4)?,
                    "baseHash": row.get::<_, Option<String>>(5)?,
                    "detail": row.get::<_, Option<String>>(6)?,
                    "status": row.get::<_, String>(7)?,
                    "createdAt": row.get::<_, i64>(8)?,
                    "remoteSize": row.get::<_, Option<i64>>(9)?.unwrap_or(0),
                }))
            },
        )
        .ok()
    }

    // ── 同步流水 ─────────────────────────────────────────────

    pub fn sync_log_add(&self, device: &str, action: &str, detail: &str, ok: bool) {
        let Ok(conn) = self.conn.lock() else { return };
        let now = crate::server::logging::now_ms();
        let _ = conn.execute(
            "INSERT INTO sync_log (at, device, action, detail, ok) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![now, device, action, detail, i64::from(ok)],
        );
    }

    /// 同步流水的总条数。
    ///
    /// 与 `sync_log_list` 分开：列表按 limit 取，界面上写「50 条」说的是**总量**，
    /// 不是「这一页有几条」。表会一直长，所以 count 走索引扫，不拉行。
    pub fn sync_log_count(&self) -> i64 {
        let Ok(conn) = self.conn.lock() else { return 0 };
        conn.query_row("SELECT COUNT(*) FROM sync_log", [], |row| row.get::<_, i64>(0))
            .unwrap_or(0)
    }

    pub fn sync_log_list(&self, limit: usize) -> Vec<Value> {
        let Ok(conn) = self.conn.lock() else { return Vec::new() };
        let Ok(mut statement) = conn.prepare(
            "SELECT id, at, device, action, detail, ok FROM sync_log ORDER BY at DESC LIMIT ?1",
        ) else {
            return Vec::new();
        };
        let mapped = statement.query_map(params![limit.max(1) as i64], |row| {
            Ok(json!({
                "id": row.get::<_, i64>(0)?,
                "at": row.get::<_, i64>(1)?,
                "device": row.get::<_, Option<String>>(2)?,
                "action": row.get::<_, String>(3)?,
                "detail": row.get::<_, Option<String>>(4)?,
                "ok": row.get::<_, i64>(5).unwrap_or(1) != 0,
            }))
        });
        match mapped {
            Ok(rows) => rows.flatten().collect(),
            Err(_) => Vec::new(),
        }
    }

    /// 全部表与行数（状态页 / 自检）。
    ///
    /// ── `mem_link` 为什么走 `dbstat` 估算 ───────────────────────
    /// 其余各表都是「条目量级」（几千行），`COUNT(*)` 是毫秒级。`mem_link`
    /// 不一样：老版本按「任意共同标签」建边，一度涨到 810 万行，`COUNT(*)` 要
    /// 扫几百毫秒。虽然新口径已经把边数压回条目量的几倍，但**老库在第一次
    /// 重建之前仍然是几百万行** —— 而这一栏是排障时看的，不该让排障本身变成
    /// 一次全表扫。所以这张表用 `dbstat` 的页数估算，误差几个百分点，够用来
    /// 判断「它是不是被撑爆了」。
    ///
    /// `dbstat` 是编译期可选虚表（`SQLITE_ENABLE_DBSTAT_VTAB`），没有就退回
    /// 精确计数 —— 宁可慢一点，也不要给一个「查不到」的坑。
    pub fn table_stats(&self) -> Value {
        let Ok(conn) = self.conn.lock() else { return json!({}) };
        let mut out = serde_json::Map::new();
        for table in [
            "entries",
            "review_queue",
            "recall_log",
            "heat",
            "llm_call",
            "agent_beat",
            "mem_link",
            "sync_state",
            "sync_conflict",
            "sync_log",
            "task_run",
        ] {
            let count: i64 = if table == "mem_link" {
                approximate_rows(&conn, table).unwrap_or_else(|| {
                    conn.query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |row| row.get(0))
                        .unwrap_or(0)
                })
            } else {
                conn.query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |row| row.get(0))
                    .unwrap_or(0)
            };
            out.insert(table.to_string(), json!(count));
        }
        Value::Object(out)
    }
}

/// 用 `dbstat` 的字节数估算一张表的行数（不扫表）。
///
/// 算法：`dbstat` 给出这张表（含它的索引）每个页的字节数，求和得到总字节；
/// 再除以「一行大概多少字节」。`mem_link` 一行是 2 个 id（各 36 字节）+ kind
/// 短串 + weight + B 树开销，实测约 150 字节 —— 估出来的数落在同一个数量级，
/// 而排障只需要数量级。
///
/// 任何一步失败都返回 `None`，由调用方退回精确 `COUNT(*)`。
fn approximate_rows(conn: &Connection, table: &str) -> Option<i64> {
    // `dbstat` 的 `pgsize` 是**每页字节数**，不是页数 —— `SUM` 出来直接就是字节。
    // 名字起成 `bytes` 是为了让下面那行除法不用再解释一次。
    let bytes: i64 = conn
        .query_row(
            "SELECT COALESCE(SUM(pgsize), 0) FROM dbstat
              WHERE name = ?1
                 OR name IN (SELECT name FROM sqlite_master WHERE tbl_name = ?1 AND type = 'index')",
            params![table],
            |row| row.get(0),
        )
        .ok()?;
    // 空表返回 0 而不是 `None`：`None` 会让调用方退回全表扫，而空表的
    // `COUNT(*)` 虽然也快，但没必要为它开一条分支。
    Some(bytes / 150)
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

/// ── 为什么这里按**列名**取值，不用下标 ────────────────────────────
/// 原来是 `row.get(19)?` 这种下标写法，加一列就要同步改 5 处，漏一处的症状
/// 是值串位（`refuted_at` 拿到 `access_count` 的数），而且编译期、运行期都
/// 不报错 —— 只会表现为「分数怪怪的」「有记忆莫名被判错」。
///
/// 本次加 `refuted_*` 三列时就真的漏了两处（`search_fts` 的 rank、`search_scan`
/// 的 body 还停在旧列号 20），说明靠人工纪律守不住。改成列名取值后，
/// `ENTRY_COLUMNS` 怎么增删都不会影响这里，`rusqlite` 找不到列名会直接报错，
/// 把静默串位变成显式失败。
fn row_to_index(row: &rusqlite::Row<'_>) -> rusqlite::Result<IndexRow> {
    let tags: String = row.get::<_, Option<String>>("tags")?.unwrap_or_default();
    Ok(IndexRow {
        id: row.get("id")?,
        project: row.get("project")?,
        agent: row.get("agent")?,
        kind: row
            .get::<_, Option<String>>("kind")?
            .unwrap_or_else(|| "other".into()),
        layer: row
            .get::<_, Option<String>>("layer")?
            .unwrap_or_else(|| "l1".into()),
        title: row.get::<_, Option<String>>("title")?.unwrap_or_default(),
        summary: row.get::<_, Option<String>>("summary")?.unwrap_or_default(),
        tags: tags.split_whitespace().map(|tag| tag.to_string()).collect(),
        importance: row.get::<_, Option<f64>>("importance")?.unwrap_or(0.5),
        pinned: row.get::<_, Option<i64>>("pinned")?.unwrap_or(0) != 0,
        starred: row.get::<_, Option<i64>>("starred")?.unwrap_or(0) != 0,
        created_at: row.get::<_, Option<i64>>("created_at")?.unwrap_or(0),
        updated_at: row.get::<_, Option<i64>>("updated_at")?.unwrap_or(0),
        valid_to: row.get("valid_to")?,
        superseded_by: row.get("superseded_by")?,
        deleted_at: row.get("deleted_at")?,
        dedup_status: row
            .get::<_, Option<String>>("dedup_status")?
            .unwrap_or_else(|| "unique".into()),
        access_count: row.get::<_, Option<i64>>("access_count")?.unwrap_or(0),
        last_hit_at: row.get("last_hit_at")?,
        refuted_at: row.get("refuted_at")?,
        refuted_reason: row
            .get::<_, Option<String>>("refuted_reason")?
            .unwrap_or_default(),
        refuted_by: row.get("refuted_by")?,
        path: row.get::<_, Option<String>>("path")?.unwrap_or_default(),
        rank: 0.0,
        hits: 0,
    })
}

fn write_row(conn: &Connection, entry: &MemoryEntry, fts: bool) -> rusqlite::Result<()> {
    conn.execute(
        "INSERT INTO entries
           (id, project, agent, kind, layer, title, summary, tags, importance, pinned, starred,
            created_at, updated_at, valid_to, superseded_by, deleted_at, dedup_status,
            access_count, last_hit_at, refuted_at, refuted_reason, refuted_by, body, path)
         VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17,?18,?19,?20,?21,?22,?23,?24)
         ON CONFLICT(id) DO UPDATE SET
           project=excluded.project, agent=excluded.agent, kind=excluded.kind,
           layer=excluded.layer,
           title=excluded.title, summary=excluded.summary, tags=excluded.tags,
           importance=excluded.importance, pinned=excluded.pinned, starred=excluded.starred,
           created_at=excluded.created_at, updated_at=excluded.updated_at,
           valid_to=excluded.valid_to,
           superseded_by=excluded.superseded_by, deleted_at=excluded.deleted_at,
           dedup_status=excluded.dedup_status,
           refuted_at=excluded.refuted_at, refuted_reason=excluded.refuted_reason,
           refuted_by=excluded.refuted_by,
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
            entry.refuted_at,
            &entry.refuted_reason,
            &entry.refuted_by,
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

/// WAL 截断 + 空闲页回收。
///
/// ── 为什么必须有这一步（2026-11-21 实测）──────────────────────
/// 这台机器的 `index.db` 长到了 **3.0 GB**，而 `freelist_count` 是 72 万页
/// （`page_count` 76 万）—— **94.5% 的文件是空的**：`mem_link` 那 754 万行
/// 删掉之后，页面只是还给了 SQLite，文件一个字节都不会自己缩回去。同目录的
/// `index.db-wal` 涨到 **700 MB** 且从不截断。于是每一次写都要在这坨空洞里
/// 翻页、每一轮读都要翻一本 700 MB 的 WAL 索引，配合导入任务反复使快照缓存
/// 失效，面板首屏的 `GET /api/memory/config` 被拖过桌面壳 60 秒的超时线 ——
/// 用户看到「等待本地代理响应超时」，记忆中枢整页永远「正在获取…」。
///
/// ── 为什么只在两个「没人用这条连接」的时机调 ────────────────
///   1. `MemoryIndex::open()` 刚建好表、单例还没发布时 —— 除自己外没人能碰
///      这个文件，`VACUUM` 慢几十秒也不会有请求排在锁后面；
///   2. `rebuild()` 里、事务提交之后 —— 那本来就是个独占的慢操作。
///
/// 反过来，**绝不能**在服务跑起来之后从后台线程随便 VACUUM：那条连接是全局
/// 唯一的 `Mutex<Connection>`，`VACUUM` 会把它独占几十秒，首屏的 `status()`
/// 就排在锁后面 —— 那正是要修的症状本身。
///
/// ── 为什么两条判据都要满足才 VACUUM ────────────────────────
/// 比例过半（一半以上是空洞）说明「值得压」；绝对量 ≥ 200 MB 说明「压得回本」。
/// 只看比例的话，一个小库删掉几条记录就会触发一次全量重写，纯亏。
/// 返回值是给人看的说明（没做事就是 `None`），由调用方写进日志。
fn compact_connection(conn: &Connection) -> Option<String> {
    // WAL 截断很便宜，而且它自己也会随写入再长出来，所以每次都做。
    let _ = conn.execute_batch("PRAGMA wal_checkpoint(TRUNCATE);");
    let page_count: i64 = conn
        .query_row("PRAGMA page_count", [], |row| row.get(0))
        .unwrap_or(0);
    let free_pages: i64 = conn
        .query_row("PRAGMA freelist_count", [], |row| row.get(0))
        .unwrap_or(0);
    let page_size: i64 = conn
        .query_row("PRAGMA page_size", [], |row| row.get(0))
        .unwrap_or(4096);
    if page_count <= 0 || free_pages <= 0 {
        return None;
    }
    let wasted = free_pages.saturating_mul(page_size);
    if free_pages * 2 < page_count || wasted < 200 * 1024 * 1024 {
        return None;
    }
    let before = page_count.saturating_mul(page_size);
    if let Err(error) = conn.execute_batch("VACUUM;") {
        return Some(format!("索引整理失败：{error}"));
    }
    let after: i64 = conn
        .query_row("PRAGMA page_count", [], |row| row.get(0))
        .unwrap_or(page_count);
    let after = after.saturating_mul(page_size);
    Some(format!(
        "索引库整理完成：{} MB → {} MB（回收 {} MB）",
        before / 1024 / 1024,
        after / 1024 / 1024,
        before.saturating_sub(after) / 1024 / 1024
    ))
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

    #[test]
    fn same_run_requires_all_three_fields_to_match() {
        // 合并成一行会让「最近 30 条」少掉一半噪声，但判错就会把两个任务
        // 的记录搅在一起。三个字段缺一不可。
        assert!(same_run("classify", "skipped", Some("没有未归类的记忆"), "classify", "skipped", Some("没有未归类的记忆")));
        // 同任务同状态、明细不同 → 不合并（「跳过」的理由不同就是不同的事）
        assert!(!same_run("classify", "skipped", Some("没有未归类的记忆"), "classify", "skipped", Some("待确认已积压 12 条")));
        // 同状态同明细、任务不同 → 不合并。两个任务的跳过理由可能一字不差。
        assert!(!same_run("classify", "skipped", Some("没有未归类的记忆"), "cleanup", "skipped", Some("没有未归类的记忆")));
        // 同任务同明细、状态不同 → 不合并（跳过变成功是转折，必须留痕）
        assert!(!same_run("extract", "skipped", Some("最近流水太少"), "extract", "done", Some("最近流水太少")));
        // 明细从 None 到 Some → 不合并
        assert!(!same_run("cleanup", "done", None, "cleanup", "done", Some("清回收站 0 条")));
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
        for required in [
            "layer",
            "valid_to",
            "dedup_status",
            "access_count",
            "last_hit_at",
            "refuted_at",
            "refuted_reason",
            "refuted_by",
        ] {
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

    /// 一个只活在内存里的索引：DDL 与 `open()` 同源（`heal_columns` 补齐新增列），
    /// 但不动磁盘 —— 「重建写了哪些行」这件事不该靠用户的真实索引库来验证。
    fn scratch_index() -> MemoryIndex {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE entries (
               id TEXT PRIMARY KEY, project TEXT, agent TEXT, kind TEXT,
               title TEXT, summary TEXT, tags TEXT, importance REAL,
               pinned INTEGER, starred INTEGER, created_at INTEGER, updated_at INTEGER,
               superseded_by TEXT, deleted_at INTEGER, body TEXT, path TEXT
             );
             -- `hit` / `trace_inject` 要写这两张，`rebuild` 的回填要读 `heat`。
             -- DDL 与 `open()` 同源：缺了任何一张，测试里的热度路径都走不到。
             CREATE TABLE heat (
               id TEXT PRIMARY KEY,
               hits INTEGER NOT NULL DEFAULT 0,
               last_hit_at INTEGER
             );
             CREATE TABLE recall_log (
               id INTEGER PRIMARY KEY AUTOINCREMENT,
               at INTEGER NOT NULL,
               tool TEXT NOT NULL,
               query TEXT,
               hits INTEGER DEFAULT 0,
               tokens INTEGER DEFAULT 0,
               agent TEXT,
               project TEXT
             );
             CREATE TABLE llm_call (
               at INTEGER NOT NULL,
               prompt_tokens INTEGER DEFAULT 0,
               completion_tokens INTEGER DEFAULT 0
             );",
        )
        .unwrap();
        heal_columns(&conn);
        MemoryIndex {
            conn: Mutex::new(conn),
            fts: false,
        }
    }

    /// 热度是**累计值**，不能被一次索引重建抹掉。
    ///
    /// 事故形状：`hit()` 原来只 `UPDATE entries`，而 `rebuild()` 是「清空
    /// `entries` 再按 `.md` 重写」—— 热度不写 `.md`，于是每次重建都归零。
    /// 真机上 `access_count > 0` 的条目 0 条，而同期 `search` / `get` 有几十次
    /// 调用：`search.weightHeat` 那一项恒等于「最近一次重建之后被读过的次数」。
    ///
    /// 修法：热度落在 `heat` 表（非派生，重建不动），重建结尾从它回填。
    #[test]
    fn heat_survives_a_rebuild() {
        use crate::server::core::memory::model::MemoryKind;
        let index = scratch_index();
        let entry = MemoryEntry::draft("m1", "被读过很多次的结论", MemoryKind::Knowledge, "正文");
        index.rebuild(std::slice::from_ref(&entry)).unwrap();

        index.hit(&["m1".to_string()]);
        index.hit(&["m1".to_string()]);
        // 重复 id 记两次就是两次：「读了几次」不能因为一次请求里出现两遍而打折
        index.hit(&["m1".to_string(), "m1".to_string()]);
        let before = index.rows(false);
        let before_hits = before.iter().find(|row| row.id == "m1").map(|row| row.access_count).unwrap_or(0);
        assert_eq!(before_hits, 4, "四次命中必须累加成 4");

        // 重建（= 用户点「重建索引」，也是排障第一步）之后热度必须还在
        index.rebuild(std::slice::from_ref(&entry)).unwrap();
        let after = index.rows(false);
        let after_hits = after.iter().find(|row| row.id == "m1").map(|row| row.access_count).unwrap_or(0);
        assert_eq!(after_hits, before_hits, "重建不能把热度清零");
    }

    /// 真删除要连热度一起删：否则同 id 重写一条新记忆，它一出生就带着旧阅读量。
    #[test]
    fn dropping_an_entry_also_drops_its_heat() {
        use crate::server::core::memory::model::MemoryKind;
        let index = scratch_index();
        let entry = MemoryEntry::draft("m1", "会被彻底删掉", MemoryKind::Knowledge, "正文");
        index.rebuild(std::slice::from_ref(&entry)).unwrap();
        index.hit(&["m1".to_string()]);
        index.drop_one("m1");
        {
            let conn = index.conn.lock().unwrap();
            let left: i64 = conn
                .query_row("SELECT COUNT(*) FROM heat WHERE id = 'm1'", [], |row| row.get(0))
                .unwrap();
            assert_eq!(left, 0, "彻底删掉的条目不该留着热度");
        }
        // 同 id 重写：新条目必须是干净的 0
        index.rebuild(std::slice::from_ref(&entry)).unwrap();
        let rows = index.rows(false);
        assert_eq!(rows[0].access_count, 0);
    }

    /// 注入也要留痕：一行 `recall_log` + 每条命中的热度。
    ///
    /// 这条是「记忆被用了」链路的闭环证据 —— 没有它，注入成功多少次都只能看
    /// 那三个原子计数器，而计数器不告诉你「用了哪几条」。
    #[test]
    fn trace_inject_records_both_heat_and_recall_log() {
        use crate::server::core::memory::model::MemoryKind;
        let index = scratch_index();
        let entry = MemoryEntry::draft("m1", "这次真被注入的结论", MemoryKind::Knowledge, "正文");
        index.rebuild(std::slice::from_ref(&entry)).unwrap();

        let traced = index.trace_inject(&["m1".to_string()], "改一下 chat.rs", 412, Some("claude-code"), None);
        assert!(traced, "索引空闲时必须写得进去");

        {
            let conn = index.conn.lock().unwrap();
            let (tool, hits, tokens, agent): (String, i64, i64, Option<String>) = conn
                .query_row(
                    "SELECT tool, hits, tokens, agent FROM recall_log ORDER BY id DESC LIMIT 1",
                    [],
                    |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
                )
                .unwrap();
            assert_eq!(tool, "inject");
            assert_eq!(hits, 1);
            assert_eq!(tokens, 412);
            // 归因丢了的话「哪个客户端在读记忆」这个问题又变得无法回答
            assert_eq!(agent.as_deref(), Some("claude-code"));
        }
        let rows = index.rows(false);
        assert_eq!(rows[0].access_count, 1, "注入命中的条目要加热度");
    }

    /// 台账必须真的封顶 —— 注入接入后台账是**每请求一行**。
    ///
    /// 不封顶的后果不是「库变大」这么轻：`recall_log` 是「记忆被用了」这件事
    /// 的唯一台账，它被撑爆之后，界面上的近期调用记录会被几十天前的注入刷掉，
    /// 于是「最近用了哪些记忆」这个问题又变成无解。
    ///
    /// 断言不写死上限：上限来自 `auto.logKeepCount`（默认 200），本机配置
    /// 可能改过。只断言「不超过上限」与「最新那一条还在」。
    #[test]
    fn recall_log_stays_capped_and_keeps_the_newest_row() {
        let index = scratch_index();
        for i in 0..120 {
            index.log_recall("search", &format!("q{i}"), 1, 10, None, None);
        }
        let keep = crate::server::core::memory::config::usize_of("auto.logKeepCount").max(20);
        let conn = index.conn.lock().unwrap();
        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM recall_log", [], |row| row.get(0))
            .unwrap();
        assert!(
            count as usize <= keep,
            "台账不能超过上限：{count} > {keep}"
        );
        let newest: String = conn
            .query_row(
                "SELECT query FROM recall_log ORDER BY id DESC LIMIT 1",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(newest, "q119", "最新的那条不能被剪掉");
    }

    /// 「今日消耗」的窗口跟着**本地自然日**走，不是 UTC 24 小时窗口。
    ///
    /// 事故形状（东八区）：本机 0:00–8:00 之间，`now - now % 86400000` 落在**昨天**
    /// 的 UTC 零点 —— 昨天的调用被算进今天。表现是「今天一次模型都没调，页面上却
    /// 写着今日预算已用完」，而且每天早上 8 点自己就好了（UTC 零点对齐那一刻）。
    ///
    /// 断言不写死时区，判据是「跨本地零点就换窗口」：`midnight` 换成 UTC 后未必是
    /// 整点，所以只有按本地日切才能让这三条同时成立。UTC 机器上两种实现等价 ——
    /// 这条测试的价值在 +8 的开发机与非 UTC 的 CI 上。
    #[test]
    fn token_window_follows_the_local_calendar_day() {
        use chrono::{Local, TimeZone, Timelike};
        let midnight = Local
            .with_ymd_and_hms(2026, 11, 20, 0, 0, 0)
            .single()
            .expect("本地零点")
            .timestamp_millis();
        // ① 零点本身
        assert_eq!(local_day_start(midnight), midnight);
        // ② 同一天里的任何时刻都在同一个窗口（1 分钟后 / 20 小时后）
        assert_eq!(local_day_start(midnight + 60_000), midnight);
        assert_eq!(local_day_start(midnight + 20 * 3_600_000), midnight);
        // ③ 窗口起点换成当地时间必须是 00:00:00 —— 这是「本地零点」的定义，
        //    也是 UTC 实现会当场露馅的那一条（它给出的是当地 08:00）。
        let start = Local.timestamp_millis_opt(local_day_start(midnight + 20 * 3_600_000)).single().unwrap();
        assert_eq!((start.hour(), start.minute(), start.second()), (0, 0, 0));
        // ④ 前一天的最后一毫秒属于前一个窗口，绝不能算进今天
        assert!(local_day_start(midnight - 1) < midnight);
    }

    /// 索引里要留着回收站条目与「已被取代」条目 —— 它们是浏览页回收站视图的数据源。
    ///
    /// 事故形状：`status()/health()` 的 `inSync` 曾经拿索引行数去比 **live 条数**。
    /// 只要用户删过一条记忆（或蒸馏过一条），索引就必然比 live 多 —— 于是仪表盘
    /// 永远显示「索引一致性需处理」，而点「重建索引」也没用：`rebuild` 本来就是
    /// 按事实源**全量**重写，删掉的行照样写回去。
    #[test]
    fn rebuild_keeps_trash_and_superseded_rows() {
        use crate::server::core::memory::model::MemoryKind;
        let index = scratch_index();
        let live = MemoryEntry::draft("m1", "今天做了什么", MemoryKind::Daily, "正文");
        let mut trashed = MemoryEntry::draft("m2", "删掉的", MemoryKind::Daily, "正文");
        trashed.deleted_at = Some(1);
        let mut replaced = MemoryEntry::draft("m3", "被取代的", MemoryKind::Daily, "正文");
        replaced.superseded_by = Some("m1".to_string());
        let entries = vec![live, trashed, replaced];

        assert_eq!(index.rebuild(&entries).unwrap(), 3);
        // 「一致」= 索引行数 == 事实源**总条数**（含回收站与已取代）
        assert_eq!(index.count(), entries.len());
        // 而 live 只有 1 条：拿 live 当判据就会永远「不一致」
        assert_eq!(entries.iter().filter(|entry| entry.is_live()).count(), 1);
    }

    /// `tokens_today` 只累加**今天**的调用，且「今天」与 `budget_exhausted` 同一口径。
    ///
    /// 窗口起点从 `local_day_start` 取而不是写死一个时间戳：测试要验的是
    /// 「台账查询用的是哪个窗口」，把起点抄成常量只会重新验一遍常量。
    #[test]
    fn tokens_today_counts_only_the_current_local_day() {
        let index = scratch_index();
        let start = local_day_start(crate::server::logging::now_ms());
        {
            let conn = index.conn.lock().unwrap();
            for (at, prompt, completion) in [
                (start - 1_000, 9_000_i64, 9_000_i64), // 昨天最后一秒：不算
                (start + 60_000, 120, 30),             // 今天：算
                (start + 3_600_000, 7, 3),             // 今天、后一小时：算
            ] {
                conn.execute(
                    "INSERT INTO llm_call (at, prompt_tokens, completion_tokens) VALUES (?1,?2,?3)",
                    params![at, prompt, completion],
                )
                .unwrap();
            }
        }
        assert_eq!(index.tokens_today(), 160);
    }
}