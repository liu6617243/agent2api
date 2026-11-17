//! 导入来源清册：内置的「这台机器上可能有哪些历史会话」清单 + 逐条探测。
//!
//! ── 为什么要有这一层，而不是继续用 `import.sources` ──────────
//! `import.sources` 是**用户手填的路径列表**：加一条 `D:/notes|md|true|0`，
//! 它只知道「有这么个目录」，不知道这个目录属于谁、里面是什么形态的数据。
//! 于是页面只能说「3 个来源，共 800 个文件」—— 用户看不出「Claude Code 的会话
//! 找到了、Codex 的没有」，也没法只导入一家。
//!
//! 这一层把「有哪些 Agent、它们的会话落在哪、能不能读、上次读到哪」变成**结构**
//! 化的东西：每个来源有 id（`claude` / `codex` / `trae`）、形态（`jsonl` /
//! `sqlite` / `sqlcipher`）、候选路径、以及读盘探测出来的真实体量。
//!
//! ── 和 `import.sources` 的关系 ──────────────────────────────
//! 清册是**候选**，`import.sources` 是**生效面**。两者的交汇点是「这个来源被
//! 用户启用了」：启用即把它的路径写进 `import.sources`（保持老逻辑不变），
//! 关掉即从中移除。这样现有 `scan()` / `collect()` / `run()` 一行都不用改，
//! 而页面拿到了它需要的结构。
//!
//! ── 为什么探测要读体量而不是只数文件 ────────────────────────
//! 会话目录的「1 个文件」和「800 个文件」差别巨大，但更关键的是**字节数**：
//! 一个 231 MB 的加密会话库和一堆 10 KB 的空会话，读进来的成本差四个数量级。
//! 用户要能在点「导入」之前看出这个差别，否则「导入全部来源」就是个赌博。

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde_json::{json, Value};

use super::config;

/// 一个内置来源的定义（静态部分）。
///
/// `kind` 决定「怎么读」，`paths` 是候选路径（第一个存在的胜出）。
/// 候选路径只写**社区常见**的位置：同一个工具在不同版本/地区安装位置不同，
/// 全列出来比猜一个准。命中即用，全不命中就报「路径不存在」让用户手填。
pub struct Builtin {
    /// 稳定 id。写进配置、记水位、页面上做 key —— 一旦发布不能改。
    pub id: &'static str,
    pub name: &'static str,
    /// 读法：`jsonl` / `md` / `sqlite` / `sqlcipher` / `trae` / `antigravity`
    pub kind: &'static str,
    /// 候选路径（支持 `~/` 与 `~/` 之后的部分；第一个存在的胜出）
    pub paths: &'static [&'static str],
    /// 一句话说明「这个来源为什么值得导」
    pub about: &'static str,
}

/// 内置来源表。
///
/// ── 排序不是随意的 ──────────────────────────────────────────
/// 前四个（ZCode / Claude / Codex / WorkBuddy）是「结构化会话日志」，读起来
/// 最省事、价值最高；后面几个是加密库与笔记，读法特殊。页面直接按这个顺序
/// 铺卡片，用户从上往下看就是「从易到难」。
pub const BUILTINS: &[Builtin] = &[
    Builtin {
        id: "zcode-db",
        name: "ZCode 会话库",
        kind: "sqlite",
        paths: &["~/.zcode/cli/db/db.sqlite"],
        about: "ZCode 自己维护的会话主库，权威源；按 id 水位增量读。",
    },
    Builtin {
        id: "zcode-tx",
        name: "ZCode 实时日志",
        kind: "jsonl",
        paths: &["~/.zcode/cli/agents"],
        about: "ZCode 边跑边写的 JSONL 日志，准实时；与会话库内容重复。",
    },
    Builtin {
        id: "claude",
        name: "Claude Code 会话",
        kind: "jsonl",
        paths: &["~/.claude/projects"],
        about: "Claude Code 的会话记录，一个项目一个目录。",
    },
    Builtin {
        id: "codex",
        name: "Codex 会话",
        kind: "jsonl",
        paths: &["~/.codex/sessions"],
        about: "Codex CLI 的会话记录，按天分目录。",
    },
    Builtin {
        id: "workbuddy",
        name: "WorkBuddy 会话（中国版）",
        kind: "jsonl",
        paths: &["~/.workbuddy/projects"],
        about: "WorkBuddy 中国版的工程会话记录。",
    },
    Builtin {
        id: "workbuddy-ai",
        name: "WorkBuddy AI 会话（国际版）",
        kind: "jsonl",
        paths: &["~/.workbuddy-ai/projects"],
        about: "WorkBuddy 国际版的工程会话记录，与中国版互不重叠。",
    },
    Builtin {
        id: "dsh",
        name: "DeepSeek Harness 会话",
        kind: "dsh",
        paths: &["~/.dsh/sessions"],
        about: "DeepSeek Harness 的会话，按 session 分目录、正文 zstd 压缩。",
    },
    Builtin {
        id: "qoder-cn",
        name: "Qoder CN 会话",
        kind: "jsonl",
        paths: &["~/.qoder-cn/projects"],
        about: "Qoder 中国版的工程会话记录。",
    },
    Builtin {
        id: "qoder",
        name: "Qoder 国际版会话",
        kind: "jsonl",
        paths: &["~/.qoder/projects"],
        about: "Qoder 国际版的工程会话记录。",
    },
    Builtin {
        id: "trae",
        name: "Trae 系会话（Trae / CN / SOLO）",
        kind: "trae",
        paths: &[
            "~/AppData/Roaming/Trae",
            "~/AppData/Roaming/Trae CN",
            "~/AppData/Roaming/Trae Solo",
        ],
        about: "Trae 家族的加密会话库，内置 SQLCipher 解密后按 id 增量读。",
    },
    Builtin {
        id: "antigravity",
        name: "Antigravity 会话（Antigravity / IDE）",
        kind: "antigravity",
        paths: &["~/.gemini", "~/.gemini/antigravity"],
        about: "Antigravity 与 Antigravity IDE 的会话日志。",
    },
    Builtin {
        id: "notes-md",
        name: "Markdown 笔记目录",
        kind: "md",
        paths: &[],
        about: "自选的笔记目录（Obsidian / 本地 markdown），一个文件一条。",
    },
];

/// 水位：某个来源上次读到哪。
///
/// ── 为什么单独存一张表而不是塞进 `import.sources` 的竖线串 ──
/// `import.sources` 一项是 `路径|格式|开关|优先级`，四个字段已经挤满；把水位
/// （sqlite 的 lastId、或每个文件的字节偏移）塞进去会把那个格式变成一门小语言。
/// 水位属于**运行时状态**，与「用户配置」生命周期不同：重装、换机、重置来源都
/// 该清掉水位，而路径要留着。分开放才对。
pub type Watermarks = BTreeMap<String, Value>;

const WATERMARK_KEY: &str = "import.watermarks";

pub fn watermarks() -> Watermarks {
    super::index::global()
        .sync_get(WATERMARK_KEY)
        .and_then(|(raw, _)| serde_json::from_str::<Watermarks>(&raw).ok())
        .unwrap_or_default()
}

pub fn save_watermarks(marks: &Watermarks) {
    let text = serde_json::to_string(marks).unwrap_or_else(|_| "{}".to_string());
    super::index::global().sync_set(WATERMARK_KEY, &text);
}

/// 展开 `~/` 前缀。
///
/// 复用 `import.rs` 的那一份，不在这里重写 —— 清册探测的路径和导入链路实际
/// 读的路径必须是同一个字符串处理，否则「卡片上说找到了，导入却说一条都没有」
/// 这种不一致会反复出现。
fn expand(path: &str) -> PathBuf {
    super::import::home_expand(path)
}

/// 用户改过的路径（`import.overrides` 里 `id -> 路径`）。
fn overrides() -> serde_json::Map<String, Value> {
    config::object_of("import.overrides")
}

/// 一个来源被用户关掉了吗。
fn ignored(id: &str) -> bool {
    config::strings_of("import.ignored").iter().any(|item| item == id)
}

/// 探测结果：一个来源在这台机器上的**真实**情况。
struct Probed {
    /// 选中的路径（overrides 优先，其次候选里第一个存在的）
    path: PathBuf,
    /// 这个路径是不是 overrides 给的
    custom: bool,
    exists: bool,
    /// 匹配到的条目数（文件数，或 sqlite 里的会话数）
    items: usize,
    bytes: u64,
    /// 一句话说明「上次读到哪」，没有水位时给「首次导入，将全量扫描」
    estimate: String,
    /// 读法补充说明（加密库、zstd、按 id 增量这类要提前说）
    note: String,
}

/// 递归数文件与字节数。深度上限 6，与 `walk` 一致。
fn measure(dir: &Path, filter: fn(&Path) -> bool) -> (usize, u64) {
    fn recurse(dir: &Path, depth: usize, filter: fn(&Path) -> bool, items: &mut usize, bytes: &mut u64) {
        if depth > 6 || *items >= 4000 {
            return;
        }
        let Ok(read) = std::fs::read_dir(dir) else { return };
        for entry in read.flatten() {
            let path = entry.path();
            let Ok(meta) = entry.metadata() else { continue };
            if meta.is_dir() {
                recurse(&path, depth + 1, filter, items, bytes);
            } else if filter(&path) {
                *items += 1;
                *bytes += meta.len();
            }
        }
    }
    let mut items = 0usize;
    let mut bytes = 0u64;
    if dir.is_file() {
        if let Ok(meta) = dir.metadata() {
            items = 1;
            bytes = meta.len();
        }
    } else if dir.is_dir() {
        recurse(dir, 0, filter, &mut items, &mut bytes);
    }
    (items, bytes)
}

/// 只求体量：目录里所有文件的总字节数（不筛扩展名）。
///
/// 加密库（Trae 系）与 Antigravity 这类来源在不解密的前提下数不出「条」，
/// 但**体量**仍然是有意义的信息 —— 用户能据此判断「这个源值不值得为它去配
/// SQLCipher 运行时」。所以这里单独一个函数，而不是给 `measure` 传个空过滤器
/// （那样字节数也会被过滤掉，量出来恒为 0）。
fn sum_bytes(dir: &Path) -> u64 {
    fn recurse(dir: &Path, depth: usize, bytes: &mut u64) {
        if depth > 6 {
            return;
        }
        let Ok(read) = std::fs::read_dir(dir) else { return };
        for entry in read.flatten() {
            let path = entry.path();
            let Ok(meta) = entry.metadata() else { continue };
            if meta.is_dir() {
                recurse(&path, depth + 1, bytes);
            } else {
                *bytes += meta.len();
            }
        }
    }
    if dir.is_file() {
        return dir.metadata().map(|meta| meta.len()).unwrap_or(0);
    }
    let mut bytes = 0u64;
    recurse(dir, 0, &mut bytes);
    bytes
}

fn is_jsonl(path: &Path) -> bool {
    matches!(
        path.extension().and_then(|ext| ext.to_str()).map(|ext| ext.to_ascii_lowercase()),
        Some(ext) if ext == "jsonl" || ext == "json"
    )
}

fn is_md(path: &Path) -> bool {
    matches!(
        path.extension().and_then(|ext| ext.to_str()).map(|ext| ext.to_ascii_lowercase()),
        Some(ext) if ext == "md" || ext == "markdown"
    )
}

/// DSH 的会话正文是 `session.jsonl.zstd`，按扩展名数不出来，只能按目录数。
fn count_dsh_sessions(dir: &Path) -> (usize, u64) {
    fn recurse(dir: &Path, depth: usize, items: &mut usize, bytes: &mut u64) {
        if depth > 4 || *items >= 4000 {
            return;
        }
        let Ok(read) = std::fs::read_dir(dir) else { return };
        for entry in read.flatten() {
            let path = entry.path();
            let Ok(meta) = entry.metadata() else { continue };
            if meta.is_dir() {
                // session-<uuid> 目录算一次会话
                if entry.file_name().to_string_lossy().starts_with("session-") {
                    *items += 1;
                }
                recurse(&path, depth + 1, items, bytes);
            } else {
                *bytes += meta.len();
            }
        }
    }
    let mut items = 0usize;
    let mut bytes = 0u64;
    if dir.is_dir() {
        recurse(dir, 0, &mut items, &mut bytes);
    }
    (items, bytes)
}

/// SQLite 类来源：数「会话」而不是数文件。
///
/// 表名与列名各家不同，所以这里只做**只读探测**：打开库、列出所有表、找一张
/// 名字里带 session/message/conversation 的表，数它的行数。找不到就退回数文件
/// 大小 —— 至少能让用户看出这个库有多大。
fn probe_sqlite(path: &Path) -> (usize, u64, String) {
    let bytes = path.metadata().map(|meta| meta.len()).unwrap_or(0);
    let Ok(conn) = rusqlite::Connection::open_with_flags(
        path,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
    ) else {
        return (0, bytes, "库打不开（可能被占用或加密）".to_string());
    };
    let Ok(mut statement) = conn.prepare("SELECT name FROM sqlite_master WHERE type='table'") else {
        return (0, bytes, "读不到表清单".to_string());
    };
    let names: Vec<String> = statement
        .query_map([], |row| row.get::<_, String>(0))
        .map(|rows| rows.flatten().collect())
        .unwrap_or_default();
    // 优先找「消息/会话」这种表；找不到再退回任意一张有 id 列的表
    let wanted = ["message", "messages", "session", "sessions", "conversation", "conversations", "chat"];
    // 先绑定成 Option<String>：`hit` 若是 &String 就会一直借着 `names`，
    // 后面 `statement`/`conn` 的借用窗口里还要用它，容易互相卡住。
    let table: Option<String> = names
        .iter()
        .find(|name| {
            let lower = name.to_ascii_lowercase();
            wanted.iter().any(|word| lower.contains(word))
        })
        .or_else(|| names.first())
        .cloned();
    let Some(table) = table else {
        return (0, bytes, "库里没有表".to_string());
    };
    let count: i64 = conn
        .query_row(&format!("SELECT COUNT(*) FROM \"{table}\""), [], |row| row.get(0))
        .unwrap_or(0);
    (count.max(0) as usize, bytes, format!("表 {table}"))
}

/// 探测单个内置来源。
fn probe(entry: &Builtin) -> Probed {
    let marks = watermarks();
    // 先绑成局部变量：`overrides()` 返回的 Map 是语句级临时值，
    // 直接在它上面取引用会借到马上要销毁的东西（E0716 的味道）。
    let overrides_map = overrides();
    let override_path = overrides_map
        .get(entry.id)
        .and_then(Value::as_str)
        .map(str::to_string)
        .filter(|text| !text.trim().is_empty());

    let (path, custom) = match override_path {
        Some(text) => (expand(&text), true),
        None => {
            let mut chosen = PathBuf::new();
            for candidate in entry.paths {
                let expanded = expand(candidate);
                if expanded.exists() {
                    chosen = expanded;
                    break;
                }
            }
            if chosen.as_os_str().is_empty() {
                // 一个都不存在：用第一个候选展示，路径栏会显示它、状态报不存在
                chosen = entry.paths.first().map(|item| expand(item)).unwrap_or_default();
            }
            (chosen, false)
        }
    };

    let exists = !path.as_os_str().is_empty() && path.exists();
    if !exists {
        return Probed {
            path,
            custom,
            exists: false,
            items: 0,
            bytes: 0,
            estimate: String::new(),
            note: String::new(),
        };
    }

    let (items, bytes, note) = match entry.kind {
        "sqlite" => {
            let (count, size, table) = probe_sqlite(&path);
            (count, size, table)
        }
        "trae" => {
            // 加密库本轮只报体量，不尝试解密 —— 解密要先有密钥与 SQLCipher 运行时，
            // 那是独立一件事。页面上要说清「已找到但还不能读」，不能假装能导。
            let size = sum_bytes(&path);
            (0, size, "加密会话库，需要 SQLCipher 运行时".to_string())
        }
        "antigravity" => {
            // Antigravity 的会话是加密的 bin 文件，同样数不出「条」；只报体量。
            let size = sum_bytes(&path);
            (0, size, "会话日志（加密 bin）".to_string())
        }
        "dsh" => {
            let (count, size) = count_dsh_sessions(&path);
            (count, size, "正文为 zstd 压缩的 JSONL".to_string())
        }
        "md" => {
            let (count, size) = measure(&path, is_md);
            (count, size, String::new())
        }
        _ => {
            let (count, size) = measure(&path, is_jsonl);
            (count, size, String::new())
        }
    };

    // 水位文案。有水位时说清「读到哪」，没有时说「首次全量」——
    // 这句话直接决定用户敢不敢点导入。
    let estimate = marks
        .get(entry.id)
        .map(|mark| describe_watermark(mark))
        .filter(|text| !text.is_empty())
        .unwrap_or_else(|| {
            if items > 0 {
                "首次导入，将全量扫描".to_string()
            } else {
                String::new()
            }
        });

    Probed { path, custom, exists, items, bytes, estimate, note }
}

/// 水位 → 一句话。
fn describe_watermark(mark: &Value) -> String {
    if let Some(last_id) = mark.get("lastId").and_then(Value::as_i64) {
        return format!("已读到 id={last_id}，本次按增量续读");
    }
    if let Some(files) = mark.get("files").and_then(Value::as_object) {
        let total: i64 = files.values().filter_map(Value::as_i64).sum();
        if total > 0 {
            return format!("已读字节水位合计 {}，本次按增量续读", bytes_text(total.max(0) as u64));
        }
    }
    String::new()
}

/// 字节 → `12.3 MB` / `820 KB`。
fn bytes_text(bytes: u64) -> String {
    const KB: f64 = 1024.0;
    let value = bytes as f64;
    if value >= KB * KB {
        format!("{:.1} MB", value / (KB * KB))
    } else if value >= KB {
        format!("{:.0} KB", value / KB)
    } else {
        format!("{bytes} B")
    }
}

/// 页面用的完整清册：12 个内置来源 + 自定义来源。
///
/// ── 为什么自定义来源也要并进来 ──────────────────────────────
/// 用户手填的路径（`D:/notes`）在清册里没有 id，但它确实是一个来源。分成两张
/// 卡片会让页面出现「内置 4/12 已找到」+「自定义 1 个」两套口径，用户得自己
/// 拼。并进来统一编号，标题才能诚实地说「4/13 个已找到」。
pub fn view() -> Value {
    let mut rows: Vec<Value> = Vec::new();
    let mut found = 0usize;
    let mut total_bytes = 0u64;
    // 内置来源实际指向的路径（规范化后），用来判断「这条自定义来源其实是一个内置来源」
    let mut taken: Vec<String> = Vec::new();
    // 生效面（`import.sources`）。只读一次：下面判「启用」和铺自定义来源卡片
    // 都要用同一份快照，读两次可能读到两次不同的配置（另一次保存正好插在中间），
    // 于是同一张卡上「状态」和「是否在清单里」互相矛盾。
    let active = super::import::sources();
    let configured: Vec<String> = active.iter().map(|source| canon(&source.path)).collect();

    for entry in BUILTINS {
        let probed = probe(entry);
        if probed.exists {
            found += 1;
        }
        let path_text = if probed.path.as_os_str().is_empty() {
            String::new()
        } else {
            crate::server::core::system_info::mask_path(&probed.path).replace('\\', "/")
        };
        if !path_text.is_empty() {
            taken.push(canon(&path_text));
        }
        total_bytes += probed.bytes;
        let off = ignored(entry.id);
        // 「能找到」与「会被读」是两件事，页面必须分开画：路径在盘上、
        // 但没写进 `import.sources` 的来源，点导入只会得到「扫描 0 个文件」。
        let enabled = probed.exists && !off && configured.iter().any(|item| item == &canon(&path_text));
        rows.push(json!({
            "id": entry.id,
            "name": entry.name,
            "kind": entry.kind,
            // 反斜杠统一成正斜杠：这一栏的字符串会被原样回传给
            // `/api/memory/import/preview` 做 `only` 过滤，而 `import.sources`
            // 里存的是正斜杠形态。不统一就会「卡片上看得见路径、点导入却一条
            // 都不读」—— 页面与配置对不上，用户只会觉得导入坏了。
            "path": path_text,
            // 「有没有指定过路径」：`notes-md` 这类内置来源天生没有候选路径，
            // 它既不是「找不到」也不是「被忽略」，页面要能把它画成「未指定」。
            "hasPath": !probed.path.as_os_str().is_empty(),
            "exists": probed.exists,
            "ignored": off,
            "enabled": enabled,
            "custom": probed.custom,
            "priority": 0,
            "items": probed.items,
            "bytes": probed.bytes,
            "bytesText": bytes_text(probed.bytes),
            "estimate": probed.estimate,
            "note": probed.note,
            "about": entry.about,
            "candidates": entry.paths.iter().map(|item| item.to_string()).collect::<Vec<_>>(),
        }));
    }

    // 自定义来源：`import.sources` 里有、但内置来源已经覆盖了同一路径的那些
    for source in &active {
        // 内置来源启用后会把路径写进 `import.sources`，所以这里一定要按**路径**
        // 去重，不能按 id —— 否则同一张卡片会出现两次（一次叫「Claude Code 会话」，
        // 一次叫「projects」），用户会以为配重了。
        let key = canon(&source.path);
        if taken.iter().any(|item| item == &key) {
            continue;
        }
        taken.push(key);
        let root = expand(&source.path);
        let exists = root.exists();
        if exists {
            found += 1;
        }
        let (items, bytes) = if exists {
            measure(&root, if source.format == "jsonl" { is_jsonl } else { is_md })
        } else {
            (0, 0)
        };
        total_bytes += bytes;
        rows.push(json!({
            "id": format!("custom:{}", source.path),
            "name": source.label(),
            "kind": source.format,
            "path": crate::server::core::system_info::mask_path(&root).replace('\\', "/"),
            "hasPath": true,
            "exists": exists,
            "ignored": false,
            "enabled": source.enabled,
            "custom": true,
            // 自定义来源的优先级要带出去：页面「整体替换」保存时若丢掉它，
            // 用户排好的导入顺序会被静默清零。
            "priority": source.priority,
            "items": items,
            "bytes": bytes,
            "bytesText": bytes_text(bytes),
            "estimate": "",
            "note": "",
            "about": "用户自填的来源",
            "candidates": Vec::<String>::new(),
        }));
    }

    json!({
        "sources": rows,
        "found": found,
        "total": rows.len(),
        "bytes": total_bytes,
        "bytesText": bytes_text(total_bytes),
    })
}

/// 两条路径是不是同一个目录 —— 直接用 `import.rs` 的那一份比较判据。
///
/// ── 为什么容不得第二份实现 ──────────────────────────────────
/// 「这个来源在不在生效清单里」这句话，由清册（画状态）和导入链路（决定读
/// 哪些目录）各判一次。两处判据不一致时，页面显示「已找到」而 `scan()` 认为
/// 不在清单里 —— 用户点「导入该来源」得到「扫描 0 个文件」，且没有任何报错。
/// 所以这里不留副本。
fn same_path(left: &str, right: &str) -> bool {
    super::import::same_path(left, right)
}

/// 路径的规范形态（比较用的键），同理复用。
fn canon(path: &str) -> String {
    super::import::canon(path)
}

/// `import.sources` 里某一项的路径部分是否就是 `path`。
fn entry_is_path(item: &str, path: &str) -> bool {
    let head = item.split('|').next().unwrap_or_default();
    canon(head) == canon(path)
}

/// 把 `import.sources` 里指向 `paths` 中任意一条的条目全摘掉。
fn drop_paths(paths: &[String], candidates: &[String]) -> Vec<String> {
    paths
        .iter()
        .filter(|item| {
            let head = item.split('|').next().unwrap_or_default();
            !candidates.iter().any(|candidate| canon(candidate) == canon(head))
        })
        .cloned()
        .collect()
}

/// 自定义来源的 id 前缀。`custom:<path>` —— 页面拿到的 row.id 原样回传即可。
///
/// ── 为什么自定义来源也走同一套 enable/disable/set_path ────────
/// 否则页面得为「内置」和「自填」写两套动作，而两套动作的差别只是「改清册」
/// 还是「改 `import.sources`」—— 这个差别应该由后端吸收，不该漏到界面上
/// （漏出去的表现是：同一张卡片，一种要「保存」，一种要「启用」，用户看不懂）。
const CUSTOM: &str = "custom:";

fn custom_path(id: &str) -> Option<&str> {
    id.strip_prefix(CUSTOM).map(str::trim).filter(|text| !text.is_empty())
}

/// 一条自定义来源「启用 / 停用 / 改路径」：改 `import.sources` 里那一条。
///
/// 停用即从清单移除 —— 自定义来源没有别的身份，离开清单就不存在了。
/// （内置来源不能这么干，它还要留在清册里显示成「已忽略」。）
///
/// ── 为什么不再「`sources()` → 改 → `save_sources()`」 ─────────
/// 那是两次独立读配置：中间插进来一次 `enable` 内置来源，这里写回的整份清单
/// 会把它刚加的那条抹掉（页面显示已启用、扫描 0 个文件）。而且 `save_sources`
/// 会把**清单里每一条**重新序列化，顺手把用户手写的 `1` 改成 `true`。
/// 现在整段走 `update_sources` 事务，只重写真正被改的那一条，其余原样保留。
fn set_custom(id: &str, path: Option<&str>, enabled: Option<bool>) -> Result<Value, String> {
    let target = custom_path(id).ok_or_else(|| format!("自定义来源 id 不合法：{id}"))?.to_string();
    let key = canon(&target);
    // 空串 = 删掉这条自定义来源。用 `Option<String>` 区分「没传路径」与「传了空」
    let editing: Option<String> = path.map(|text| text.trim().to_string());
    super::import::update_sources(move |sources, ignored| {
        let mut next: Vec<String> = Vec::with_capacity(sources.len().max(1));
        let mut hit = false;
        for raw in sources {
            let Some(mut item) = super::import::Source::parse(raw) else {
                // 解析不了的条目原样留着 —— 不是我们要改的这一条，也不该被
                // 一次「启用某来源」顺手清掉
                next.push(raw.clone());
                continue;
            };
            if canon(&item.path) != key {
                next.push(raw.clone());
                continue;
            }
            hit = true;
            if let Some(flag) = enabled {
                item.enabled = flag;
            } else if let Some(text) = editing.as_deref() {
                if text.is_empty() {
                    // 不进 `next` 就是删掉
                    continue;
                }
                // 格式按新路径重猜：用户把「笔记目录」改成「会话目录」时，
                // 保留旧格式会让 `walk` 用错扩展名过滤，一条都读不到。
                let guessed = super::import::guess_format(text);
                if item.format != guessed {
                    item.format = guessed;
                }
                item.path = text.to_string();
            }
            next.push(item.to_config());
        }
        if !hit {
            return Err(format!("这条来源已经不在清单里了：{target}"));
        }
        Ok((next, ignored.to_vec()))
    })?;
    Ok(view())
}

/// 加一条自定义来源。
///
/// ── 为什么单独一个接口，而不是让页面整体替换 `import.sources` ──
/// 整体替换是「页面必须知道全部行」的接口：内置来源启用后也在那份清单里，
/// 页面一旦某次少算了一条（比如筛掉了隐藏行），保存就会把它静默删掉。
/// 「加一条」这个动作用「加一条」的接口表达，语义上就不可能误删别的东西。
pub fn add(path: &str, format: &str, priority: i64) -> Result<Value, String> {
    let text = path.trim();
    if text.is_empty() {
        return Err("路径不能为空".into());
    }
    // 格式先看用户选的，选「自动」就按扩展名猜 —— 猜错的代价是一条都读不到，
    // 所以下拉里给了明确的「自动」。
    let format = match format.trim().to_ascii_lowercase().as_str() {
        "jsonl" => "jsonl".to_string(),
        "md" => "md".to_string(),
        _ => super::import::guess_format(text),
    };
    let line = super::import::Source {
        path: text.to_string(),
        format,
        enabled: true,
        priority,
    }
    .to_config();
    // 「查重 + 追加」必须在一个事务里：分开做的话，两次并发「加一条」都能通过
    // 查重，最后两条同路径都在清单里，同一批文件被导入两遍。
    super::import::update_sources(move |sources, ignored| {
        // 查重用 `Source::parse` 而不是裸切第一个 `|`：路径里合法地可以带 `|`
        // （Windows 文件名不允许，但用户手写的配置什么都有），只按分隔符切会把
        // 路径截断成前缀，两条不同目录被判成同一条。
        let clash = sources.iter().any(|item| {
            super::import::Source::parse(item)
                .map(|parsed| same_path(&parsed.path, text))
                .unwrap_or(false)
        });
        if clash {
            return Err(format!("这个路径已经在清单里了：{text}"));
        }
        let mut next: Vec<String> = sources.to_vec();
        next.push(line.clone());
        Ok((next, ignored.to_vec()))
    })?;
    Ok(view())
}

/// 把清册里一个来源「启用」—— 即写进 `import.sources`，让现有导入链路读它。
///
/// 这是清册与老配置的唯一交汇点。返回写回后的清册视图，调用方直接拿它刷新页面。
pub fn enable(id: &str) -> Result<Value, String> {
    if custom_path(id).is_some() {
        return set_custom(id, None, Some(true));
    }
    let Some(entry) = BUILTINS.iter().find(|item| item.id == id) else {
        return Err(format!("没有这个内置来源：{id}"));
    };
    let probed = probe(entry);
    if !probed.exists {
        return Err(format!("「{}」的路径不存在，先改路径再启用", entry.name));
    }
    // 存 `~` 形态：配置要能跟着仓库/同步走，绝对路径换台机器就废了
    let text = crate::server::core::system_info::mask_path(&probed.path).replace('\\', "/");
    let format = if entry.kind == "md" { "md" } else { "jsonl" };
    // 「读两个键 → 判断 → 写回」整段放在一个事务里：判断与写入之间不允许别人
    // 插进来（否则那一瞬间的清单和我们要写的这一份不是同一个版本）。
    let id = id.to_string();
    let line = format!("{text}|{format}|true|0");
    super::import::update_sources(move |sources, ignored| {
        let mut next_sources: Vec<String> = sources.to_vec();
        if !next_sources.iter().any(|item| entry_is_path(item, &text)) {
            next_sources.push(line.clone());
        }
        let mut next_ignored: Vec<String> = ignored.to_vec();
        next_ignored.retain(|item| item != &id);
        Ok((next_sources, next_ignored))
    })?;
    Ok(view())
}

/// 停用：从 `import.sources` 移除该路径，并记进忽略名单。
///
/// ── 为什么要记忽略名单 ──────────────────────────────────────
/// 只从 `import.sources` 移除是不够的：清册里这个来源仍然「存在」，页面下次
/// 刷新会重新显示成可启用，用户会以为自己的关闭没生效。忽略名单让「关掉」
/// 这件事有地方待着。
pub fn disable(id: &str) -> Result<Value, String> {
    if custom_path(id).is_some() {
        return set_custom(id, None, Some(false));
    }
    let Some(entry) = BUILTINS.iter().find(|item| item.id == id) else {
        return Err(format!("没有这个内置来源：{id}"));
    };
    let probed = probe(entry);
    // 候选路径与当前生效路径都要摘：用户改过路径时，配置里那条是改后的
    let mut candidates: Vec<String> = entry.paths.iter().map(|item| item.to_string()).collect();
    if !probed.path.as_os_str().is_empty() {
        candidates.push(probed.path.to_string_lossy().to_string());
    }
    // 与 `enable` 同理：读两个键 → 改 → 写回整段在一个事务里，
    // 不留「已从清单移除但还没进忽略名单」的中间态。
    let id = id.to_string();
    super::import::update_sources(move |sources, ignored| {
        let after = drop_paths(sources, &candidates);
        let mut next_ignored: Vec<String> = ignored.to_vec();
        if !next_ignored.iter().any(|item| item == &id) {
            next_ignored.push(id.clone());
        }
        Ok((after, next_ignored))
    })?;
    Ok(view())
}

/// 改路径：写进 `import.overrides`，并把新路径同步进 `import.sources`。
pub fn set_path(id: &str, path: &str) -> Result<Value, String> {
    if custom_path(id).is_some() {
        // 自定义来源「改路径」= 改清单里那一条；空串 = 删掉它
        return set_custom(id, Some(path), None);
    }
    let Some(entry) = BUILTINS.iter().find(|item| item.id == id) else {
        return Err(format!("没有这个内置来源：{id}"));
    };
    let text = path.trim().to_string();
    // 覆盖表与清单必须一起改：只写覆盖表的话，来源已经是启用状态时清单里那条
    // 旧路径还在，「页面显示新路径、导入仍然读旧路径」会静默地导错东西；
    // 只改清单不写覆盖表的话，下次探测又会回到候选路径上。一次事务写完。
    let old: Vec<String> = entry.paths.iter().map(|item| item.to_string()).collect();
    let format = if entry.kind == "md" { "md" } else { "jsonl" };
    let id = id.to_string();
    config::update(|next| {
        // 「是否处于启用状态」也要在事务里读：外面读一次、里面写一次，
        // 中间别人改了忽略名单，这里就会按过期的判断改清单。
        let enabled_now = !next.strings("import.ignored").iter().any(|item| item == &id) && !text.is_empty();
        // 覆盖表用 `set` 整份替换而不是 `apply_in`：Map 类的 `patch` 走深合并，
        // **删不掉键** ——「路径清空 = 删掉那条覆盖」用它写不进去（旧值还在）。
        let mut overrides = next.map_of("import.overrides");
        if text.is_empty() {
            overrides.remove(&id);
        } else {
            overrides.insert(id.clone(), json!(text));
        }
        next.set("import.overrides", Value::Object(overrides))?;
        if enabled_now {
            let mut paths = drop_paths(&next.strings("import.sources"), &old);
            if !paths.iter().any(|item| entry_is_path(item, &text)) {
                paths.push(format!("{text}|{format}|true|0"));
            }
            next.set("import.sources", json!(paths))?;
        }
        Ok(())
    })?;
    Ok(view())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builtin_ids_are_unique_and_stable() {
        let mut seen = std::collections::HashSet::new();
        for entry in BUILTINS {
            assert!(seen.insert(entry.id), "id 重复：{}", entry.id);
            assert!(!entry.name.is_empty());
            assert!(matches!(
                entry.kind,
                "jsonl" | "md" | "sqlite" | "sqlcipher" | "trae" | "antigravity" | "dsh"
            ));
        }
        assert_eq!(BUILTINS.len(), 12, "页面按 12 个内置来源设计");
    }

    #[test]
    fn bytes_text_switches_units_at_the_right_place() {
        assert_eq!(bytes_text(512), "512 B");
        assert_eq!(bytes_text(2048), "2 KB");
        assert_eq!(bytes_text(1024 * 1024 * 12 + 300 * 1024), "12.3 MB");
    }

    #[test]
    fn watermark_text_prefers_id_then_files() {
        assert_eq!(
            describe_watermark(&json!({ "lastId": 2890 })),
            "已读到 id=2890，本次按增量续读"
        );
        assert!(describe_watermark(&json!({ "files": { "a": 1000, "b": 2000 } })).contains("3 KB"));
        assert_eq!(describe_watermark(&json!({})), "");
    }

    #[test]
    fn expand_handles_tilde_and_plain_paths() {
        let home = crate::server::core::system_info::home_dir();
        if home.as_os_str().is_empty() {
            return;
        }
        assert_eq!(expand("~/.claude/projects"), home.join(".claude").join("projects"));
        // 中间的 `~` 不是前缀
        assert_eq!(expand("D:/a~b"), PathBuf::from("D:/a~b"));
    }

    /// 改一条自定义来源时，**别人的写法必须原样留着**。
    ///
    /// 旧实现是「`sources()` 读整份 → `save_sources()` 整份写回」，而
    /// `save_sources` 会把每一条都 `to_config()` 重新序列化：用户手写的
    /// `D:/notes|md|1|5`（`1` 而不是 `true`）、`D:/x|MD|on|5` 这类等价写法，
    /// 会在「改另一条来源的路径」这个动作里被顺手改写。功能上无害，但它让
    /// 配置文件出现大片与用户操作无关的 diff，也让「我只改了一条」这句话变成假的。
    /// 现在 `update_sources` 只重写真正命中的那一条。
    #[test]
    fn set_custom_keeps_other_rows_verbatim() {
        crate::test_support::isolated(
            concat!(module_path!(), "::set_custom_keeps_other_rows_verbatim"),
            || {
                // 两条「写法不标准」的旁证 + 一条待改的自定义来源
                let untouched = [
                    "D:/notes|md|1|5".to_string(),
                    "D:/chats|jsonl|on|9".to_string(),
                ];
                let editing = "D:/old|md|true|0".to_string();
                let mut rows = untouched.to_vec();
                rows.push(editing.clone());
                // 测试模块里的 `super` 是 `import_registry`，要再上一层才是 `memory`
                super::super::import::update_sources(move |_, ignored| Ok((rows.clone(), ignored.to_vec())))
                    .expect("写入初始清单应当成功");

                set_custom("custom:D:/old", Some("D:/new"), None).expect("改路径应当成功");

                let saved = super::config::strings_of("import.sources");
                for row in &untouched {
                    assert!(
                        saved.iter().any(|item| item == row),
                        "旁证行被改写了：`{row}` 不见了，实际 {saved:?}"
                    );
                }
                assert!(
                    saved.iter().any(|item| item.starts_with("D:/new|")),
                    "新路径没有写进清单：{saved:?}"
                );
                assert!(
                    !saved.iter().any(|item| item == &editing),
                    "旧路径应当被替换而不是并存：{saved:?}"
                );
            },
        );
    }

    /// 加一条已经被停用的自定义来源：应当**重新启用**，而不是加出第二条。
    ///
    /// `set_custom` 停用是「从清单移除」；用户再点一次「加」时走的却是
    /// `add()`。两条路径处理的是同一份清单，`add` 的查重必须把「已停用的那条」
    /// 也认成重复 —— 否则清单里会出现两条同路径（一条 `false` 一条 `true`），
    /// 同一批文件被扫两遍。
    #[test]
    fn add_does_not_duplicate_an_existing_path() {
        crate::test_support::isolated(
            concat!(module_path!(), "::add_does_not_duplicate_an_existing_path"),
            || {
                let rows = vec!["D:/notes|md|false|0".to_string()];
                super::super::import::update_sources(move |_, ignored| Ok((rows.clone(), ignored.to_vec())))
                    .expect("写入初始清单应当成功");

                let outcome = add("D:/notes", "md", 0);
                assert!(outcome.is_err(), "同路径重复添加应当报错，实际：{outcome:?}");

                let saved = super::config::strings_of("import.sources");
                assert_eq!(saved.len(), 1, "清单里出现了重复条目：{saved:?}");
            },
        );
    }
}
