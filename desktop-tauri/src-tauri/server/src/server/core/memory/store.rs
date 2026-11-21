//! 记忆中枢 · 文件层（事实源的读写）。
//!
//! ── 目录布局 ────────────────────────────────────────────────
//! ```text
//! {config_dir}/memory/
//!   config.json          模块设置（扁平点号键，见 `config`）
//!   projects.json        项目登记表（slug → 名称 / 远程 / 本地路径）
//!   device.json          本机设备 id（同步与冲突裁决用）
//!   index.db             派生索引（可随时删除重建）
//!   general/l1/<agent>/<YYYY-MM-DD>.md    全局（不属于任何项目）的原始日常
//!   general/l2/<sub>/<id>.md              全局的沉淀条目
//!   projects/<slug>/l1/<agent>/<YYYY-MM-DD>.md
//!   projects/<slug>/l2/<sub>/<id>.md
//!   profile/<id>.md      深层画像条目
//!   trash/<id>.md        回收站（单条一个文件，可按 id 恢复）
//! ```
//!
//! ── 为什么 l1 是「一天一文件多节」，l2 是「一条一文件」──────────
//! 原始日常（l1）一天几十条，一条一个文件会把目录撑成几千个 inode，而它们
//! 天然属于「同一天的同一段工作」—— 合成一个文件既省目录，又能直接看出「那天
//! 干了什么」。沉淀结论（l2）每条都是独立结论，要能被单独取代、单独
//! 附证据、单独删除，共用文件会让「删一条」变成「改一个共享文件」。
//!
//! 早期形态曾把所有 summary 塞进共享的 `l2/summary.md`，结果后写覆盖先写、
//! 索引里两条指向同一路径 —— 这就是 l2 必须一条一文件的原因。
//!
//! ── 原子写为什么是四步 ──────────────────────────────────────
//! 「先写临时文件 → fsync → 删旧文件 → rename」：同目录 rename 在 NTFS 与
//! ext4 上都是原子的，读者要么看到旧版本、要么看到新版本，不会看到半个文件。
//! 少一步 fsync 的后果是断电后 rename 成功但内容为空（ext4 上真实发生过）。
//!
//! ── 为什么删除是「搬到 trash/」而不是删文件 ─────────────────
//! 记忆的价值在「当时的判断」，误删一条往往几天后才发现。回收站让删除可逆，
//! 而 `deleted_at` 同时写进 frontmatter，于是「索引丢了但文件还在」时
//! 重建出来的依然是「已删除」状态 —— 两个真相不会打架。

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use serde_json::{json, Value};

use super::layout;
use super::model::{MemoryEntry, MemoryKind};

/// 不属于任何项目的记忆，落到这个伪项目下（`general/` 在磁盘上的别名）
pub const GLOBAL_SLUG: &str = "_global";

/// 记忆事实源的**进程内写锁**。
///
/// ── 为什么必须有 ────────────────────────────────────────────
/// [`save_section_locked`] 与 [`remove_section_locked`] 都是「读整份容器 → 摘/改一节 → 整份
/// 写回」。两个写者落在**同一天、同一 agent** 的容器上时，后写的整份覆盖先写的，
/// 先写那一节静默消失 —— 文件层不会报任何错。并发来源至少三路：
///   · 多个客户端各自的 `tools/call`（`mcp.rs` 每调用一次丢一个 spawn_blocking）；
///   · 面板 `/api/memory/write`（直接在 tokio worker 上跑，连 spawn_blocking 都没包）；
///   · 调度器十一个任务（distill / tag / consolidate / cleanup …）。
/// 这三路都写同一个 `l1/<agent>/<YYYY-MM-DD>.md`。
///
/// ── 为什么是进程内 Mutex 而不是文件锁 ───────────────────────
/// 跨进程才需要 flock；事实源只由网关这一个进程写（用户手编 `.md` 属于「外力
/// 改动」，模块头已写明「下次启动 / 下次重建生效」，不在本锁的承诺范围内）。
/// 一把 `Mutex` 足够，也没有 Windows 上 flock 语义不一致的坑。
///
/// ── 附带修掉的一个更硬的坑：临时文件名只带 pid ──────────────
/// [`write_atomic`] 的临时文件叫 `<目标>.tmp.<pid>`。同一进程里两个线程同时
/// 写**同一份文件**时，它们拿到的是**同一个临时路径**：一个刚 create 完，另一
/// 个就把它截断；随后先完成的一方把 tmp rename 走，后完成的一方 rename 时
/// 文件已经不在了 —— 表现成随机的「落盘失败」或「内容串了」。锁把这条路径也
/// 一并串行化，所以不必把 pid 换成「pid + 线程 id + 计数器」。
///
/// ── 为什么锁在外层入口，而不是里面的每一处 `write_atomic` ────
/// `save` 内部会调 `detach_live` → `remove_section`，是天然嵌套的。std 的
/// `Mutex` 不可重入，若让 `save_section` / `remove_section` 各自加锁就是自锁死。
/// 所以这里采取「`*_locked` 内部版本不带锁 + 公开入口统一取锁」：
///            `save` ──► `save_locked` ──► `detach_live_locked` ──► `remove_section_locked`
///            `detach_live` ──► `detach_live_locked`
///            `remove_section` / `remove_file` ──► 各自的 `_locked`
///            `move_save` / `retire` / `revive` ──► 一次临界区里做两件事
///            `end_deferred` ──► 批末一次性写回攒下的容器
/// 一次事务（比如「摘旧节 + 写回收站」）在**同一把锁内**完成，中途不会被插队。
static WRITE_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// 取写锁。panic 中毒只说明「当时那一节没写完」，文件因为原子写仍是一致的，
/// 不该让全库永久写不进 —— 直接取回内部值。
pub(crate) fn lock_writes() -> std::sync::MutexGuard<'static, ()> {
    WRITE_LOCK.lock().unwrap_or_else(|poison| poison.into_inner())
}

/// `projects.json` 的读改写锁（与 [`WRITE_LOCK`] 分开）。
///
/// 分类登记（`registry_upsert`）在**每一次写入**之前都会跑一遍：读整份登记表 →
/// 补一条 → 整份写回。两个并发写者各补一个不同的项目时，后写的整份覆盖先写的，
/// 先登记的那张项目卡凭空消失 —— 与容器丢节是同一类 bug，且触发的频率更高
/// （不是「同一天同 agent」，而是「同一时刻的任意两次写入」）。
///
/// ── 为什么不用同一把锁 ──────────────────────────────────────
/// `merge_project` 会「一边持锁遍历、一边逐条 `store::save`」，而 `store::save`
/// 内部要 [`lock_writes`]。std 的 `Mutex` 不可重入，用同一把锁就是自锁死。
/// 两把锁的**获取顺序固定为：先 PROJECTS 后 WRITE**（`merge_project` 之外的
/// 路径都是先登记、后落盘，天然同序），不存在反向持有，因此不会死锁。
static PROJECTS_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

pub(crate) fn lock_projects() -> std::sync::MutexGuard<'static, ()> {
    PROJECTS_LOCK.lock().unwrap_or_else(|poison| poison.into_inner())
}

fn now_ms() -> i64 {
    crate::server::logging::now_ms()
}

// ── 延迟落盘（deferred write）────────────────────────────────
//
// ── 为什么需要这一层 ────────────────────────────────────────
// l1 是「一天一文件、多节」的容器。`save_section_locked` 每写一条都要把整份
// 容器读进来、摘旧节、追加新节、再整份写回。单条没问题；但批量导入把成千上万
// 条塞进同一天（历史数据的 created 全落在导入日附近，或本就同一天几十条），
// 每条都整份读改写就是 O(n²) 写放大：文件涨到 MB 级后，60 秒只能写一两百条，
// 而且越写越慢。
//
// 解法抄 AgentHub 的 `beginDeferred` / `endDeferred`：批内对同一容器的多次追加
// 先在内存里攒成「该文件的最终文本」，批次结束统一写一次盘。对外语义不变
// （窗口内读者读到的是「已经加上这一条」的最新文本），只是把 N 次整份写盘压成
// 一次。
//
// ── 为什么是 thread_local 而不是全局状态 ────────────────────
// 写锁是全局的，但延迟窗口天然属于「某一个调用方的一次批量操作」。放进全局会
// 让别的并发写者也被卷进这个窗口、攒在别人的内存里最后被别人 flush 掉。绑定到
// 线程后，只有发起批量写入的那个线程看得到自己攒下的内容，互不干扰。
thread_local! {
    static OVERLAY: std::cell::RefCell<Overlay> = std::cell::RefCell::new(Overlay::default());
}

#[derive(Default)]
struct Overlay {
    /// 窗口是否打开。关着的时候所有读写都直透磁盘，行为与不引入本层完全一致。
    active: bool,
    /// 窗口内被改写文件的「待落盘文本」。key 是路径串；`None` 表示这个文件
    /// 在窗口内被删掉了（批末统一真删）。
    files: std::collections::HashMap<String, Option<String>>,
}

fn overlay_key(path: &Path) -> String {
    path.to_string_lossy().replace('\\', "/")
}

/// 窗口内读容器：有攒下的文本读攒下的，否则直读磁盘。窗口关着等价于直读。
fn overlay_read(path: &Path) -> String {
    OVERLAY.with(|cell| {
        let overlay = cell.borrow();
        match overlay.files.get(&overlay_key(path)) {
            Some(Some(text)) => text.clone(),
            Some(None) => String::new(),
            None => fs::read_to_string(path).unwrap_or_default(),
        }
    })
}

/// 窗口内写容器：窗口开着只记内存，关着直接落盘。
fn overlay_write(path: &Path, text: &str) -> Result<(), String> {
    let deferred = OVERLAY.with(|cell| {
        let mut overlay = cell.borrow_mut();
        if overlay.active {
            overlay.files.insert(overlay_key(path), Some(text.to_string()));
        }
        overlay.active
    });
    if deferred {
        Ok(())
    } else {
        write_atomic(path, text)
    }
}

/// 窗口内删容器：窗口开着只记「待删」，关着立即删。
fn overlay_remove(path: &Path) {
    let deferred = OVERLAY.with(|cell| {
        let mut overlay = cell.borrow_mut();
        if overlay.active {
            overlay.files.insert(overlay_key(path), None);
        }
        overlay.active
    });
    if !deferred {
        let _ = fs::remove_file(path);
    }
}

/// 当前是否有线程打开了延迟落盘窗口。
///
/// ── 为什么需要一个全局计数 ──────────────────────────────────
/// 窗口状态本身是 thread_local（每个线程管自己攒的那批），但「此刻有人正攒着
/// 某个容器的旧快照」这件事必须能被**别的线程**看到：批量导入正在攒
/// `2026-11-20.md` 的最终文本时，如果另一个线程去跑「容器归位」把几千节搬走并
/// 回写老容器，导入批末那一写会把**搬走之前的整份文本**盖回去 —— 结果是同一
/// 批节同时存在于两个容器（老容器复活一份、新容器一份），`load_all` 读出两份、
/// 索引按 id 只留一份，删一条时另一半还在。
///
/// 这个竞态无法靠 `lock_writes` 解：窗口内根本不持锁（就是为了避免批内每次写
/// 都抢锁）。只能在「拆容器」这一侧退让：有人在攒批，就先不拆。
static DEFERRED_ACTIVE: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

/// 是否有线程正在延迟落盘窗口内（见 [`begin_deferred`]）。
pub fn deferred_active() -> bool {
    DEFERRED_ACTIVE.load(std::sync::atomic::Ordering::SeqCst) > 0
}

/// 打开一段延迟落盘窗口。**不可嵌套**，且必须与 [`end_deferred`] 成对。
///
/// 只给批量导入这类「同线程连续写同一个容器」的场景用；普通单条写入不要碰它。
pub fn begin_deferred() {
    OVERLAY.with(|cell| {
        let mut overlay = cell.borrow_mut();
        overlay.active = true;
        overlay.files.clear();
    });
    DEFERRED_ACTIVE.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
}

/// 关闭延迟落盘窗口，把攒下的容器文本统一写盘。返回写失败的文件（路径 + 原因）。
///
/// 即便有文件写失败，也会把窗口状态清干净（`active = false`、清空攒下的表），
/// 绝不让一次失败的批量导入把线程永久卡在「所有写入只在内存」的状态 ——
/// 那会让后续所有写入都看不到已落盘的结果。
pub fn end_deferred() -> Vec<String> {
    let files = OVERLAY.with(|cell| {
        let mut overlay = cell.borrow_mut();
        overlay.active = false;
        std::mem::take(&mut overlay.files)
    });
    // 计数必须在取到 files 之后、写盘之前就减掉：写盘失败也要减，否则一次失败
    // 会让「容器归位」永久以为有人在攒批，迁移再也不会跑。
    DEFERRED_ACTIVE.fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
    if files.is_empty() {
        return Vec::new();
    }
    // 落盘这一段必须持写锁：本批攒下的是「基于首次读取时的快照」的最终文本，
    // 批末一次性写回。其它线程的单条写入（`store::save` → `save_locked`）也走
    // 这把锁，持锁落盘保证「批末这一写」不会和别人的一次整份写互相覆盖。
    // 注意这里直接调 `write_atomic`（它自己不加锁），不会与已持有的锁重入死锁。
    let _guard = lock_writes();
    let mut errors: Vec<String> = Vec::new();
    let mut removed_any = false;
    for (path, content) in files {
        match content {
            Some(text) => {
                if let Err(error) = write_atomic(Path::new(&path), &text) {
                    errors.push(format!("{path}：{error}"));
                }
            }
            // 窗口内被删掉的容器：批末真删（`overlay_remove` 在窗口里只记待删）
            None => {
                if fs::remove_file(&path).is_ok() {
                    removed_any = true;
                }
            }
        }
    }
    if removed_any {
        invalidate();
    }
    errors
}

/// memory 根目录。默认与网关统一库同源（`config::config_dir()`），
/// `storage.root` 配了就按配置走（换盘、放同步盘）。
pub fn root() -> PathBuf {
    let custom = super::config::text_of("storage.root");
    if custom.trim().is_empty() {
        crate::server::config::config_dir().join("memory")
    } else {
        PathBuf::from(custom.trim())
    }
}

pub fn projects_root() -> PathBuf {
    root().join("projects")
}

pub fn general_root() -> PathBuf {
    root().join("general")
}

pub fn profile_root() -> PathBuf {
    root().join("profile")
}

pub fn trash_root() -> PathBuf {
    root().join("trash")
}

/// 同步删除留档目录：`sync-trash/<时间戳>-<设备 id>/<原相对路径>`。
///
/// ── 为什么不直接进 `trash/` ─────────────────────────────────
/// `trash/` 里的文件是**记忆条目**：一条一个 `.md`、frontmatter 里有
/// `deleted_at` / `origin_path`，`load_all` 会把它读成一条「已删除的记忆」，
/// 于是能在界面上按 id 恢复。而同步删除的是一**个文件**，可能是 l1 容器
/// （一天一文件、里面几十节），把它整份塞进 `trash/` 会被当成一条垃圾记忆：
/// 标题取到第一行、正文混着所有节、恢复按单条写回 —— 数据形状就错了。
///
/// 所以同步删除走另一条路：原文件**原样留档**在这个目录里（保持相对路径），
/// 不进索引、不进同步包、不出现在回收站列表。它的用途只有一个 —— 真删错了
/// 的时候，人还能从磁盘上把那份字节捞回来。
///
/// 为什么要留档而不是物理删：远端「包里没有这个文件」这个信号本身可能是错的
/// （对面包被别的设备覆盖、对方手动清空过记忆目录、传输途中截断）。而删除是
/// 唯一不可逆的操作：一条记忆没了，两台机器就再也对不上「当时是怎么判断的」。
pub fn sync_trash_root() -> PathBuf {
    root().join("sync-trash")
}

/// 配置文件本身的路径：**固定**在默认目录下，绝不调用 [`root()`]。
///
/// 为什么不能写成 `root().join("config.json")`：`root()` 要读 `storage.root`，
/// 读配置要先进 `config::stored()`，而 `config::stored()` 又要 `config_file()`
/// 才能知道去哪读 —— 三者首尾相接，形成无穷递归。在 test 线程（默认 2MB 栈）
/// 上表现为 `stack overflow, aborting`，在主线程上是启动即崩。
///
/// 「决定根目录在哪」这份配置只能放在一个不依赖根目录的地方，这是唯一解。
pub fn config_file() -> PathBuf {
    crate::server::config::config_dir()
        .join("memory")
        .join("config.json")
}

pub fn projects_file() -> PathBuf {
    root().join("projects.json")
}

pub fn index_file() -> PathBuf {
    root().join("index.db")
}

pub fn device_file() -> PathBuf {
    root().join("device.json")
}

/// 建齐目录。幂等，每次访问前调用（允许用户中途手工删掉某个子目录）。
pub fn ensure_layout() -> Result<(), String> {
    for dir in [
        root(),
        projects_root(),
        general_root(),
        profile_root(),
        trash_root(),
        // 同步删除留档目录：建出来才能保证「先留档、再删」这一步在首次同步时
        // 不会因为目录不存在而失败（失败就整条不删，同步表现成「删不掉」）。
        sync_trash_root(),
    ] {
        fs::create_dir_all(&dir).map_err(|error| format!("创建记忆目录失败 {}：{error}", dir.display()))?;
    }
    Ok(())
}

/// 项目 slug：只保留小写字母、数字、`-`、`_`、`.`；连续非法字符压成一个 `-`。
///
/// 为什么不用项目名直接当目录名：项目名可能带空格、中文、斜杠（`a/b` 会造出
/// 两级目录），而 slug 同时是接口参数与前端路由片段，必须是路径安全的。
pub fn slugify(name: &str) -> String {
    let slug = layout::sanitize_slug(name);
    if slug == GLOBAL_SLUG {
        GLOBAL_SLUG.to_string()
    } else {
        slug
    }
}

// ── 路径 ─────────────────────────────────────────────────────

/// 相对路径 → 绝对路径。
pub fn abs(relative: &str) -> PathBuf {
    root().join(relative.replace('/', std::path::MAIN_SEPARATOR_STR))
}

/// 绝对路径 → 相对 memory 根的正斜杠路径。
pub fn rel(path: &Path) -> String {
    path.strip_prefix(root())
        .map(|rest| rest.to_string_lossy().replace('\\', "/"))
        .unwrap_or_else(|_| path.to_string_lossy().replace('\\', "/"))
}

/// 一条记忆相对 memory 根的路径。`deleted` 为真时落 `trash/`。
///
/// 已有 `entry.path` 时以它为准确认落点（记忆可能在 general 与 projects 之间
/// 搬过家，重算会把它搬回旧地方）；路径为空时按 layer 现算。
pub fn entry_path(entry: &MemoryEntry, deleted: bool) -> PathBuf {
    if deleted {
        return trash_root().join(format!("{}.md", entry.id));
    }
    if !entry.path.is_empty() && !entry.path.starts_with("trash/") {
        let candidate = abs(&entry.path);
        // 只认「确实在 memory 根下」的相对路径：同步过来的 path 可能来自
        // 另一台机器（盘符不同），不能直接当绝对路径用
        if candidate.starts_with(root()) {
            return candidate;
        }
    }
    let slug = entry.project.as_deref().map(slugify).filter(|slug| slug != GLOBAL_SLUG);
    let agent = entry.agent.as_deref().unwrap_or("manual");
    // ── 日期跟「事情发生时间」走，不跟「写入时刻」走 ──────────────
    //
    // 这里原来是 `created_at.max(updated_at)`，等于永远取 `updated_at`（它必然
    // ≥ created_at）。后果：**历史导入全部挤进「今天」那个容器**。补录九月
    // 二十号的会话，`created_at` 是九月二十号、`updated_at` 是此刻，
    // 取最大值就落进 `2026-11-20.md`。
    //
    // 一次导入 2341 条窗口切条 → 53 个不同日期的内容全写进同一个文件 →
    // 那个文件涨到 55 MB → 又被单文件上限整份跳过 → 「普通记忆 0 条」。
    // 两个 bug 串成一条链，这一处是源头。
    //
    // 只影响**新建条目**的落点；已有 `entry.path` 的条目在上面那个分支里
    // 直接沿用原路径，不会被搬走（搬动已有记忆另有 `split_misplaced_containers`）。
    let stamp = if entry.created_at > 0 { entry.created_at } else { entry.updated_at };
    let date = super::search::format_date(stamp);
    let relative = layout::memory_rel_path(
        slug.as_deref(),
        entry.layer,
        agent,
        entry.kind.as_str(),
        &date,
        &entry.id,
    );
    abs(&relative)
}

/// 是否「一天一文件」的 l1 容器（`.../l1/<agent>/YYYY-MM-DD.md`）。
///
/// 判据是路径形状而不是 frontmatter：存量文件、同步过来的文件、用户手建的
/// 文件都只有路径这一条共同信息。
pub fn is_daily_container(relative: &str) -> bool {
    let segments: Vec<&str> = relative.split('/').collect();
    if segments.len() < 3 || segments[segments.len() - 2].is_empty() {
        return false;
    }
    // `/l1/` 段必须在文件名之前
    if !segments[..segments.len() - 1].contains(&"l1") {
        return false;
    }
    let stem = segments[segments.len() - 1].trim_end_matches(".md");
    looks_like_date(stem)
}

fn looks_like_date(text: &str) -> bool {
    let bytes = text.as_bytes();
    bytes.len() == 10
        && bytes[4] == b'-'
        && bytes[7] == b'-'
        && bytes[..4].iter().all(u8::is_ascii_digit)
        && bytes[5..7].iter().all(u8::is_ascii_digit)
        && bytes[8..10].iter().all(u8::is_ascii_digit)
}

/// 原子写：同目录 tmp → 写满 → fsync → 删旧 → rename。
pub fn write_atomic(path: &Path, contents: &str) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|error| format!("创建目录失败 {}：{error}", parent.display()))?;
    }
    // 改写前留一份 .bak：记忆是「只有用户自己能重建」的数据，一次错误的
    // 覆盖没有第二次机会。备份按份数滚动，不占空间。
    if super::config::bool_of("storage.backupBeforeWrite") && path.exists() {
        backup(path);
    }
    let tmp = path.with_extension(format!("tmp.{}", std::process::id()));
    {
        let mut file = fs::File::create(&tmp).map_err(|error| format!("写入失败 {}：{error}", tmp.display()))?;
        file.write_all(contents.as_bytes())
            .map_err(|error| format!("写入失败 {}：{error}", tmp.display()))?;
        // flush + sync_all：rename 之前必须让内容真正落盘
        file.flush().map_err(|error| format!("刷盘失败 {}：{error}", tmp.display()))?;
        file.sync_all().map_err(|error| format!("刷盘失败 {}：{error}", tmp.display()))?;
    }
    // Windows 上 rename 到已存在文件会失败，先删目标。
    //
    // ── 失败必须顺手清掉 tmp ────────────────────────────────────
    // 早先只有 rename 失败那条分支会删 tmp，`remove_file` 失败就直接返回 ——
    // 于是记忆目录里留下 `2026-09-20.tmp.1234` 这类残骸。它的扩展名不是 `.md`，
    // `load_all` 读不到它（不会污染索引），但用户在自己目录里会看到一堆莫名
    // 文件，而且磁盘上真的要清也不是我们该让他们手动做的事。
    if path.exists() {
        if let Err(error) = fs::remove_file(path) {
            let _ = fs::remove_file(&tmp);
            return Err(format!("替换失败 {}：{error}", path.display()));
        }
    }
    let renamed = fs::rename(&tmp, path).map_err(|error| {
        let _ = fs::remove_file(&tmp);
        format!("落盘失败 {}：{error}", path.display())
    });
    // ── 写成功就丢快照 ────────────────────────────────────────
    // `write_atomic` 是**所有**记忆落盘的唯一出口（单条、容器、回收站、
    // `projects.json`、`migrations.json` 都走它），所以在这里丢缓存一处就够，
    // 不必给十几个调用点各加一行 —— 而漏加任何一处的后果都是「刚写完读不到」，
    // 那种 bug 只能等用户报「我明明存了」才会被发现。
    //
    // 放在 rename 之后而不是之前：指纹校验本身也会抓到变化，但那是「下次读
    // 才发现」；主动丢缓存让紧接着的那次读直接重算，省掉一次白读。
    if renamed.is_ok() {
        invalidate();
    }
    renamed
}

/// `.bak` / `.bak.1` … 滚动。失败不影响主流程（备份是保险，不是流程）。
fn backup(path: &Path) {
    let keep = super::config::usize_of("storage.backupKeep").max(1);
    let base = path.with_extension(format!(
        "{}.bak",
        path.extension().and_then(|ext| ext.to_str()).unwrap_or("")
    ));
    let base = if path.extension().is_some() {
        // `a.md` → `a.md.bak`
        PathBuf::from(format!("{}.bak", path.display()))
    } else {
        base
    };
    for index in (1..keep).rev() {
        let from = if index == 1 {
            base.clone()
        } else {
            PathBuf::from(format!("{}.{}", base.display(), index - 1))
        };
        let to = PathBuf::from(format!("{}.{}", base.display(), index));
        if from.exists() {
            let _ = fs::rename(&from, &to);
        }
    }
    let _ = fs::copy(path, &base);
}

// ── l1 容器 ──────────────────────────────────────────────────

/// 容器头：记「这段流水属于哪天、哪个 agent、哪个项目」。
///
/// 为什么不把每条的 frontmatter 都写进容器：一条记忆已经有一份完整
/// frontmatter 写在它自己的文件里；容器里再嵌一层 YAML 会造出「改哪个才算数」
/// 的问题。容器头只记「这段流水是谁的」，条目元数据一律走 JSON 标记行。
fn container_head(date: &str, agent: &str, slug: Option<&str>) -> String {
    let mut out = String::new();
    out.push_str("---\n");
    out.push_str("type: daily-log\n");
    out.push_str(&format!("date: {date}\n"));
    out.push_str(&format!("agent: {agent}\n"));
    if let Some(slug) = slug.filter(|slug| !slug.is_empty() && *slug != GLOBAL_SLUG) {
        out.push_str(&format!("project: {slug}\n"));
    }
    out.push_str("---\n\n");
    out
}

/// 条目标记行。用 JSON 而不是 `key=value`：标签、标题里的空格与逗号不会再造
/// 解析歧义 —— 手写文件最容易踩的坑就是「标签里带逗号」。
fn section_marker(entry: &MemoryEntry) -> String {
    let meta = json!({
        "id": entry.id,
        "title": entry.title,
        "kind": entry.kind.as_str(),
        "tags": entry.tags,
        "importance": entry.importance,
        "created": entry.created_at,
        "updated": entry.updated_at,
        "pinned": entry.pinned,
        "starred": entry.starred,
        "files": entry.files,
        "supersedes": entry.supersedes,
    });
    format!("<!-- mem {} -->\n", serde_json::to_string(&meta).unwrap_or_else(|_| "{}".into()))
}

fn section_heading(entry: &MemoryEntry) -> String {
    let seconds = (entry.created_at / 1000).rem_euclid(86_400);
    format!(
        "## {:02}:{:02} · {}\n\n",
        seconds / 3600,
        (seconds % 3600) / 60,
        entry.title
    )
}

/// 把容器文本切成 `(元数据 JSON, 正文)` 序列。读不懂的段落整段当正文，
/// 不丢内容 —— 记忆文件最怕「解析器看不懂就吃掉」。
fn split_sections(text: &str) -> Vec<(Value, String)> {
    let normalized = text.replace("\r\n", "\n").replace('\r', "\n");
    let mut out: Vec<(Value, String)> = Vec::new();
    let mut current: Option<(Value, String)> = None;
    for line in normalized.lines() {
        if let Some(meta) = parse_marker(line) {
            if let Some(done) = current.take() {
                out.push(done);
            }
            current = Some((meta, String::new()));
            continue;
        }
        if let Some((_, body)) = current.as_mut() {
            body.push_str(line);
            body.push('\n');
        }
    }
    if let Some(done) = current {
        out.push(done);
    }
    // 去掉每条开头那一行 `## HH:MM · 标题`（我们写出去的可读性行）
    out.into_iter()
        .map(|(meta, body)| {
            let trimmed = body.trim_start_matches('\n');
            let body = if trimmed.starts_with("## ") {
                trimmed.split_once('\n').map(|(_, rest)| rest.to_string()).unwrap_or_default()
            } else {
                trimmed.to_string()
            };
            (meta, body.trim_matches('\n').to_string())
        })
        .collect()
}

fn parse_marker(line: &str) -> Option<Value> {
    let trimmed = line.trim();
    let inner = trimmed.strip_prefix("<!-- mem ")?.strip_suffix(" -->")?;
    serde_json::from_str::<Value>(inner.trim()).ok()
}

/// 一条记忆 → 容器里的一个 section。
fn section_text(entry: &MemoryEntry) -> String {
    let mut out = String::new();
    out.push_str(&section_marker(entry));
    out.push_str(&section_heading(entry));
    out.push_str(entry.body.trim_end());
    out.push_str("\n\n");
    out
}

/// 从容器文本里移除某个 id 的 section（删一条 daily 用）。
fn strip_section(text: &str, id: &str) -> (String, bool) {
    let normalized = text.replace("\r\n", "\n");
    let mut out = String::new();
    let mut skipping = false;
    let mut removed = false;
    for line in normalized.lines() {
        if let Some(meta) = parse_marker(line) {
            let same = meta.get("id").and_then(Value::as_str) == Some(id);
            if same {
                skipping = true;
                removed = true;
                continue;
            }
            skipping = false;
        }
        if !skipping {
            out.push_str(line);
            out.push('\n');
        }
    }
    (out, removed)
}

/// 把容器文本按 id 分出「节 id → 这一节的正文（含 `## HH:MM · 标题` 行）」。
///
/// 同步合并要的是**原始切片**，而不是 [`split_sections`] 那种去掉标题行、
/// 只留正文的形状：合并结果要原样写回去，重新渲染一遍标题行会丢掉用户手改的
/// 文案。返回 `None` 表示文件根本不是容器（没有 `type: daily-log` 头）。
fn container_sections(text: &str) -> Option<(String, Vec<(String, String)>)> {
    let normalized = text.replace("\r\n", "\n").replace('\r', "\n");
    // 容器头 = 第一个 `<!-- mem ... -->` 之前的部分。不是容器就没有标记行，
    // 或者头里没有 `daily-log`，两种情况都返回 `None`。
    let first_marker = normalized.find("<!-- mem ")?;
    let head = &normalized[..first_marker];
    if !head.contains("type: daily-log") {
        return None;
    }
    let mut sections: Vec<(String, String)> = Vec::new();
    let mut current: Option<(String, String)> = None;
    for line in normalized[first_marker..].lines() {
        if let Some(meta) = parse_marker(line) {
            if let Some(done) = current.take() {
                sections.push(done);
            }
            let id = meta
                .get("id")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            current = Some((id, format!("{line}\n")));
            continue;
        }
        if let Some((_, body)) = current.as_mut() {
            body.push_str(line);
            body.push('\n');
        }
    }
    if let Some(done) = current {
        sections.push(done);
    }
    Some((head.to_string(), sections))
}

/// 把两份**同一个 l1 容器**（两台设备各写过同一天）拼成一份。拼不了就返回 `None`。
///
/// ── 判据为什么不是「按行 diff」────────────────────────────
/// l1 容器的形状是「头 + 一串 `<!-- mem {json} -->` 开头的节」，每节带稳定 id。
/// 同一天的容器在两个设备上几乎必然共有几节（同步过），各自又有对方没有的新节
/// （那天各写了几条）。所以判据是**按节 id 对齐**：
/// - 两边都有的 id：正文必须一模一样，否则真冲突（同一条被两边各改了一版，
///   拼不了，交给人）；
/// - 只有一边有的 id：直接并进来（对方那天多写的，本机没有）。
///
/// 这与 AgentHub `tryMergeDailyFiles` 同口径，只是它按 `time` 排序后再渲染，
/// 这里保留各自的原始切片、按「本地节在前、远端新增节在后」排列 —— 原始切片
/// 里带着对方写的可读性标题，重新渲染会改掉用户看得见的东西。
///
/// ── 为什么这条路径值得特别做 ────────────────────────────────
/// l1 是「一天一文件」，两台机器同一天都干活时这个文件**必然**两边都改过。
/// 不合并的话，每次同步都会往冲突队列里塞一条，用户每个工作日都要裁决一次
/// 「今天的日志听谁的」—— 而这个答案其实是「都要」。合并掉它，冲突队列才
/// 只剩真正需要人判断的那些。
pub(crate) fn merge_container(local: &str, remote: &str) -> Option<String> {
    let (local_head, local_sections) = container_sections(local)?;
    let (remote_head, remote_sections) = container_sections(remote)?;
    // 头不一致（agent / project / date 被改过）就不猜：头决定这个文件属于哪天、
    // 哪个 agent，猜错会把记忆挂到错的地方。
    if local_head.trim() != remote_head.trim() {
        return None;
    }
    let known: std::collections::HashSet<&str> =
        local_sections.iter().map(|(id, _)| id.as_str()).collect();
    let mut merged = local_sections.clone();
    for (id, body) in &remote_sections {
        // ── 没有 id 的节（用户手写的段落 / 早期格式）──
        //
        // 空 id 不能按「id 相同」处理：两份文件里各有几段没 id 的正文时，它们
        // 的 id 都是空串，按 id 对齐会把「不同的两段」判成「同一条改了两版」，
        // 明明能拼的内容被判成冲突。这里退化成按正文去重：本机已有完全相同
        // 的一段就跳过，否则并进来。
        if id.is_empty() {
            if !merged.iter().any(|(_, existing)| existing.trim_end() == body.trim_end()) {
                merged.push((id.clone(), body.clone()));
            }
            continue;
        }
        // 远端这一节是不是「本机已有」用哈希集合判：容器可能有几百节，
        // 每节都线性扫一遍是 O(n²)。
        if known.contains(id.as_str()) {
            let Some((_, existing)) = merged.iter().find(|(existing, _)| existing == id) else {
                return None;
            };
            // 同一条两边都在：正文必须一字不差，否则是真冲突
            if existing.trim_end() != body.trim_end() {
                return None;
            }
        } else {
            // 远端那天多写的节：并到后面
            merged.push((id.clone(), body.clone()));
        }
    }
    let mut out = local_head;
    for (_, body) in merged {
        out.push_str(body.trim_end());
        out.push_str("\n\n");
    }
    Some(out)
}

/// 把一条 l1 记忆写进（或更新到）它所属的当天容器。
///
/// **调用方必须已持有 [`lock_writes`]** —— 整段是「读容器 → 摘同 id 旧节 →
/// 追加新节 → 整份写回」，中途被另一个写者插进来，先写那一节会被这一份整份
/// 覆盖（静默丢数据）。加锁责任在公开入口，是为了让「摘 + 写」这类组合动作
/// 能在一个临界区里做完（见 [`save`]）。
fn save_section_locked(entry: &MemoryEntry) -> Result<(), String> {
    ensure_layout()?;
    let target = entry_path(entry, false);
    let relative = rel(&target);
    let date = relative
        .split('/')
        .next_back()
        .map(|name| name.trim_end_matches(".md").to_string())
        .unwrap_or_else(|| super::search::format_date(entry.created_at));
    let existing = overlay_read(&target);
    // 先摘掉同 id 的旧 section：这是「更新一条 l1」的唯一正确做法 ——
    // 追加而不摘会得到两份同 id 的条目，重建索引时后者覆盖前者、删除时只删掉一份
    let (cleaned, had_old) = strip_section(&existing, &entry.id);
    let mut text = if had_old {
        cleaned
    } else if cleaned.trim().is_empty() {
        container_head(&date, entry.agent.as_deref().unwrap_or("manual"), entry.project.as_deref())
    } else {
        // 保留原头部（可能有用户手写的说明），只在末尾追加
        cleaned
    };
    if !text.is_empty() && !text.ends_with('\n') {
        text.push('\n');
    }
    if !text.is_empty() && !text.ends_with("\n\n") {
        text.push('\n');
    }
    text.push_str(&section_text(entry));
    overlay_write(&target, &text)
}

// ── 读写 ─────────────────────────────────────────────────────

/// 这条记忆**活着的时候**该不该落在「一天一文件」的容器里。
///
/// 判据是**目标路径的形状**，不是 `layer == L1 && kind == Daily`：
/// 存量用户的 daily 记忆是一个独立的 `<id>.md`（v1 形态），它的 `path` 已经
/// 指向那个文件。只按 layer/kind 判会让一条老记忆在下次保存时被塞进容器格式，
/// 原文件既没被删、内容又被新格式覆盖 —— 那是数据损坏。
fn live_is_container(entry: &MemoryEntry) -> bool {
    if entry.layer != super::model::Layer::L1 || entry.kind != MemoryKind::Daily {
        return false;
    }
    let live = MemoryEntry { deleted_at: None, ..entry.clone() };
    is_daily_container(&rel(&entry_path(&live, false)))
}

/// 把一条记忆写回事实源。
///
/// 只负责「写到它现在该在的位置」。**老位置**的清理是 [`detach_live`] 的事，
/// 由调用方在改类型 / 改层 / 改日期这类会换路径的场景里先调一次 —— 这里没有
/// 上一版对象，猜不出来。要「摘旧 + 写新」在同一个临界区完成，用 [`move_save`]。
///
/// 本函数仍会把 `deleted_at` 非空的条目送进回收站（同步、导入这类手里只有
/// 「最终形态」的路径要用），但**删除业务走 [`retire`]**：只有 `retire` 手里
/// 才有改动前的对象，能按正确的活路径把旧那份摘干净。
pub fn save(entry: &MemoryEntry) -> Result<(), String> {
    ensure_layout()?;
    let _guard = lock_writes();
    save_locked(entry)
}

fn save_locked(entry: &MemoryEntry) -> Result<(), String> {
    if entry.deleted_at.is_some() {
        // 进回收站前必须从原位置拿走，否则「同一个 id 两份文件」重建时会打架
        detach_live_locked(entry)?;
        return write_atomic(&trash_root().join(format!("{}.md", entry.id)), &entry.to_markdown());
    }
    if live_is_container(entry) {
        return save_section_locked(entry);
    }
    write_atomic(&entry_path(entry, false), &entry.to_markdown())
}

/// 把某个 id 从「活着的形态」里摘掉（容器摘一节 / 单条删文件），不动回收站。
///
/// 调用方负责之后的落盘。抽出来是因为三条路径（delete / 改类型换目录 / purge）
/// 都要做同一件事，而少做一次的后果都是「同一条记忆出现两份」。
pub fn detach_live(entry: &MemoryEntry) -> Result<(), String> {
    let _guard = lock_writes();
    detach_live_locked(entry)
}

fn detach_live_locked(entry: &MemoryEntry) -> Result<(), String> {
    let live = entry_path(&MemoryEntry { deleted_at: None, ..entry.clone() }, false);
    // ── 判据只看「这个落点是什么形状的文件」
    //
    // 不能写成 `live_is_container(entry)`：它把 `layer`/`kind` 也纳入了判断，
    // 于是结论取决于**调用方传进来的是哪一版对象**。当前所有调用方都传了与
    // `live` 匹配的那一版（`move_save` 传 `previous`、`retire` 传改动前的
    // 对象），所以没出问题；但这是个只靠调用纪律维持的隐式契约 —— 哪天有人
    // 顺手把改完的 `entry` 传进来，daily → knowledge 会让判据返回 false，落到
    // 下面「删单文件」分支，而 `live` 解析出来正是当天的容器：一次
    // `remove_file` 就把同一天其它所有条目一起删了，且不报错。
    // 收紧成「只看路径形状」之后，这个坑从根上没了。
    //
    // 这里用**绝对路径**判，不绕 `rel()`/`abs()` 一趟：那趟往返要求
    // `storage.root` 与 `path` 的字面完全一致（`strip_prefix` 按组件逐字比），
    // 用户把根目录写成不同大小写时 `rel()` 会回落成路径串、`abs()` 再拼一次
    // 就得到一个不存在的路径，摘节被静默跳过 —— 旧容器里留下删不掉的孤儿节。
    if looks_like_daily_container_path(&live) {
        return strip_section_at(&live, &entry.id);
    }
    // 走 overlay：延迟窗口内「搬走一条单文件」也要能被批末统一处理，
    // 免得窗口里写的和窗口里删的互相看不见。窗口关着时等价于立即删。
    overlay_remove(&live);
    Ok(())
}

/// 从容器里摘掉某一节（删/搬走一条 l1 daily 时用）。
///
/// 只摘一节、不动同一天的其它条目。删一条 daily 记忆绝不能把同一天的其它
/// 条目一起带走 —— 那是数据丢失，不是删除。
pub fn remove_section(entry: &MemoryEntry) -> Result<(), String> {
    let _guard = lock_writes();
    remove_section_locked(entry)
}

/// 与 [`save_section_locked`] 同一种读改写：摘节也必须独占整份容器，
/// 否则并发的 `save_section_locked` 会把刚摘掉的那一节又整份写回来（或者反过来）。
fn remove_section_locked(entry: &MemoryEntry) -> Result<(), String> {
    let target = entry_path(&MemoryEntry { deleted_at: None, ..entry.clone() }, false);
    strip_section_at(&target, &entry.id)
}

/// 按**绝对路径**从容器里摘掉某一节。不经过 `rel()`/`abs()` 往返。
///
/// 拆出来是因为两条路径算出来的「落点」可能不一样：`detach_live_locked` 要摘的是
/// **改动之前**那一份容器，而它手里的 `entry.path` 已经指向改完之后的新位置。
/// 让它复用 `remove_section_locked` 就只能靠临时改 `path` 再算一遍，既绕又容易
/// 在 `storage.root` 大小写不一致时算出不存在的路径。这里直接收绝对路径。
fn strip_section_at(target: &Path, id: &str) -> Result<(), String> {
    if !looks_like_daily_container_path(target) {
        return Ok(());
    }
    let text = overlay_read(target);
    if text.is_empty() {
        return Ok(());
    }
    let (cleaned, removed) = strip_section(&text, id);
    if !removed {
        return Ok(());
    }
    // 摘完只剩容器头（没有任何 section）就把空容器删掉，免得 l1 目录里
    // 攒一堆「只有 frontmatter 的日期文件」
    let has_section = cleaned.lines().any(|line| parse_marker(line).is_some());
    if !has_section {
        overlay_remove(target);
        return Ok(());
    }
    overlay_write(target, &cleaned)
}

/// 绝对路径版的「是不是一天一文件的 l1 容器」。
///
/// 与 [`is_daily_container`] 同一套判据，只是不要求先算成相对路径 —— 这样
/// 「按形状判容器」在 `storage.root` 大小写/分隔符与配置不一致时也不会失配。
fn looks_like_daily_container_path(path: &Path) -> bool {
    if path.extension().and_then(|ext| ext.to_str()) != Some("md") {
        return false;
    }
    let Some(stem) = path.file_stem().and_then(|name| name.to_str()) else {
        return false;
    };
    if !looks_like_date(stem) {
        return false;
    }
    // 形状固定是 `<base>/l1/<agent>/<YYYY-MM-DD>.md`：祖目录必须正好是 `l1`。
    // 不能写成「路径里任意一段叫 l1」—— 记忆根恰好放在名叫 `l1` 的盘目录下时
    // 会把根下所有日期命名的文件都当成容器。
    path.parent()
        .and_then(Path::parent)
        .and_then(Path::file_name)
        .and_then(|name| name.to_str())
        .map(|name| name == "l1")
        .unwrap_or(false)
}

/// 从事实源彻底移除一条记忆（purge 用；日常删除走回收站）。
///
/// 活形态与回收站两份都要清掉。活形态一律走 `detach_live_locked`，因为「活形态」
/// 可能是容器里的一节，也可能是一个独立的 `.md` —— 只按 layer/kind 猜会漏掉
/// 老 v1 单文件，或者在容器里留下孤儿节。
///
/// 整段一个临界区：分成「先摘活形态、再删回收站」两次加锁的话，中间插进来的
/// 并发 `save` 会让刚 purge 的条目又被写出来。
pub fn remove_file(entry: &MemoryEntry) -> Result<(), String> {
    let _guard = lock_writes();
    detach_live_locked(entry)?;
    let trashed = entry_path(entry, true);
    if trashed.exists() {
        fs::remove_file(&trashed)
            .map_err(|error| format!("删除失败 {}：{error}", trashed.display()))?;
        invalidate();
    }
    Ok(())
}

/// 换位保存：先把**旧位置**的那一份摘掉，再写到新位置，整段一个临界区。
///
/// ── 为什么两个动作必须在一起 ────────────────────────────────
/// 改层 / 改类型 / 改日期 / 改项目都会换目录。调用方拿到的是旧 `MemoryEntry`，
/// 先 `detach_live`、再 `save` 是两次加锁：中间被并发的 `save`（同一个 id，
/// 同一个新路径）插进来，就会写出两份同 id 的文件 —— 下次重建索引时后者覆盖
/// 前者、删除时只删掉一份，另一份变成删不掉的幽灵。
///
/// `previous` 是**改动前**的对象（按它算旧路径），`current` 是改完的。
/// 两条路径算出来一样时只写不摘 —— 容器里的节原地更新由 `save` 自己处理。
pub fn move_save(previous: &MemoryEntry, current: &MemoryEntry) -> Result<(), String> {
    ensure_layout()?;
    let _guard = lock_writes();
    let old = entry_path(&MemoryEntry { deleted_at: None, ..previous.clone() }, false);
    let new = entry_path(&MemoryEntry { deleted_at: None, ..current.clone() }, false);
    if old != new {
        detach_live_locked(previous)?;
    }
    save_locked(current)
}

/// 退休：把一条活着的记忆「摘离活形态 + 落进回收站」，整段一个临界区。
///
/// ── 为什么不能直接用 `save(current)` ────────────────────────
/// [`save_locked`] 见到 `deleted_at` 非空时确实会走「摘活形态 + 写 trash/」，
/// 但它摘活形态用的判据是**这一份对象自己**的路径。删除路径里 `entry.path` 已经
/// 被改写成 `trash/<id>.md` 了：`entry_path` 见到 `trash/` 开头的相对路径会跳过
/// 「按 `entry.path` 定落点」，转而按 layer/kind 重算 —— 对 v1 那种「独立单文件」
/// 的 l1 记忆，重算出来是容器路径，原文件既没删掉、又写了一份进回收站。
///
/// 所以必须把**改动前**的对象（`previous`）一起传进来：摘活形态按它算，写回收站
/// 按 `current` 算。两件事在同一个临界区，中间插不进来并发 `save`，否则会出现
/// 「活的 + 回收站」两份同 id。
pub fn retire(previous: &MemoryEntry, current: &MemoryEntry) -> Result<(), String> {
    ensure_layout()?;
    let _guard = lock_writes();
    detach_live_locked(previous)?;
    write_atomic(&trash_root().join(format!("{}.md", current.id)), &current.to_markdown())
}

/// 这条记忆**活着时**的相对路径（忽略 `deleted_at`）。
///
/// 删除那一刻必须先把原路径按这个函数算出来，再改 `deleted_at`：`entry_path`
/// 见到 `deleted_at` 非空就返回 `trash/<id>.md`，顺序反了记下的「原路径」
/// 其实是回收站路径，恢复时会把记忆搬到 `trash/` 里。
pub fn live_rel(entry: &MemoryEntry) -> String {
    let live = MemoryEntry { deleted_at: None, ..entry.clone() };
    rel(&entry_path(&live, false))
}

/// 复活：把回收站里的一条写回活位置，再把回收站那份删掉，整段一个临界区。
///
/// 与 [`retire`] 对称。分成「save 一次 + remove_file 一次」两步的话，中间那一瞬
/// 库里同时存在活的与回收站两份同 id：并发的 `restore` 会看到「已经活了」直接
/// 返回、并发 `purge` 会把刚写出来的活文件一起删掉。收进一个临界区就没这问题。
pub fn revive(trashed: &Path, current: &MemoryEntry) -> Result<(), String> {
    ensure_layout()?;
    let _guard = lock_writes();
    save_locked(current)?;
    if trashed.exists() {
        fs::remove_file(trashed)
            .map_err(|error| format!("清理回收站失败 {}：{error}", trashed.display()))?;
    }
    Ok(())
}

/// 把一个目录下的 `.md` 全读出来。读不出来的单个文件只记日志，不中断整轮
/// （一条手写坏了的文件不该让整页空白）。
fn read_dir_entries(dir: &Path, out: &mut Vec<MemoryEntry>) {
    let Ok(reader) = fs::read_dir(dir) else { return };
    let mut items: Vec<PathBuf> = reader.flatten().map(|item| item.path()).collect();
    items.sort();
    // 单文件上限（`storage.maxFileSizeKB`，默认 512KB）。
    //
    // ── 为什么要在读取前用 metadata 挡，而不是读完再判长度 ───────
    // 这个上限保护的场景是「用户把手写的巨型日志 / 导出文件扔进记忆根」。
    // 读进内存再判断，那几 MB 已经花掉了；而且 `load_all()` 在每次检索前
    // 都会跑一遍，一次读 100MB 的代价是整页卡死。`metadata()` 一次 syscall
    // 就能挡掉，成本可以忽略。
    //
    // ── 为什么 0 当作「不限制」而不是「全跳过」───────────────
    // `usize_of` 对非正数返回 0。若把 0 解释成「上限 0」，配置写坏一次就会
    // 让记忆库读取变成空白 —— 那是灾难级副作用。0 代表「没限制」，
    // 与 `search.timeDecayHalfLife=0` / `auto.dailyTokenLimit=0` 的既有
    // 「0 = 关闭」语义一致。
    //
    // ── 为什么跳过时记日志 ──────────────────────────────────
    // 静默跳过最难排查：用户会看到「我明明写了这条，为什么搜不到」。
    let limit_kb = super::config::usize_of("storage.maxFileSizeKB");
    let limit_bytes = limit_kb.saturating_mul(1024);
    for path in items {
        if path.is_dir() {
            read_dir_entries(&path, out);
            continue;
        }
        if path.extension().and_then(|ext| ext.to_str()) != Some("md") {
            continue;
        }
        // ── 容器不吃体积上限，只吃「是容器」这件事 ────────────────
        //
        // 这个上限本意是挡「用户手扔进来的巨型文本」，但 l1 容器
        // （`.../l1/<agent>/YYYY-MM-DD.md`）的体积**不是单条记忆的体积** ——
        // 它是一天里所有记忆拼起来的。历史导入把几千条窗口切条全落到同一个
        // 「今天」的容器里，一天就能到几十 MB；一条都没超，整份文件超了。
        //
        // 后果是灾难级的：整个容器被跳过后，那几千条**全部消失**，
        // 索引里一条都不剩、页面上「普通记忆 0 条」，而磁盘上文件好好地躺着
        // —— 用户看到的是「导入了 2341 条，但库里没有」。
        //
        // 所以判据改成「形状」：容器一定读，读的时候由 `read_container`
        // 逐节解析；体积保护只对**非容器**的单条 `.md` 生效。
        let relative = rel(&path);
        if is_daily_container(&relative) {
            let Ok(text) = fs::read_to_string(&path) else {
                crate::server::logging::log("[Memory]", &format!("⚠️ 记忆文件读取失败：{}", path.display()));
                continue;
            };
            read_container(&text, &relative, &path, out);
            continue;
        }
        if limit_bytes > 0 {
            match fs::metadata(&path) {
                Ok(meta) if meta.len() > limit_bytes as u64 => {
                    crate::server::logging::log(
                        "[Memory]",
                        &format!(
                            "⏭️ 跳过超大记忆文件（{} KB > 上限 {} KB）：{}",
                            meta.len() / 1024,
                            limit_kb,
                            path.display()
                        ),
                    );
                    continue;
                }
                _ => {}
            }
        }
        let Ok(text) = fs::read_to_string(&path) else {
            crate::server::logging::log("[Memory]", &format!("⚠️ 记忆文件读取失败：{}", path.display()));
            continue;
        };
        let stem = path
            .file_stem()
            .and_then(|name| name.to_str())
            .unwrap_or("unknown")
            .to_string();
        out.push(MemoryEntry::from_markdown(&text, &stem, &relative));
    }
}

/// 容器 → 多条 l1 记忆。容器头的 `project` / `agent` 作为条目缺省值。
fn read_container(text: &str, relative: &str, path: &Path, out: &mut Vec<MemoryEntry>) {
    let date = relative
        .split('/')
        .next_back()
        .unwrap_or("unknown")
        .trim_end_matches(".md")
        .to_string();
    let (head, _) = crate::server::core::memory::model::split_frontmatter_public(text);
    let head_project = head.get("project").cloned().filter(|value| !value.is_empty());
    let head_agent = head.get("agent").cloned().filter(|value| !value.is_empty());
    for (index, (meta, body)) in split_sections(text).into_iter().enumerate() {
        let id = meta
            .get("id")
            .and_then(Value::as_str)
            .map(str::to_string)
            .unwrap_or_else(|| format!("{date}-{index}"));
        let mut entry = MemoryEntry::from_markdown(
            &format!("---\nid: {id}\n---\n\n{body}"),
            &id,
            "trash/placeholder",
        );
        // 上面那次转换只为拿到标题/正文的默认推断；真正的字段全部来自标记行
        entry.id = id;
        entry.path = relative.to_string();
        entry.layer = super::model::Layer::L1;
        entry.kind = MemoryKind::parse(
            meta.get("kind").and_then(Value::as_str).unwrap_or("daily"),
        );
        entry.project = head_project.clone();
        entry.agent = head_agent.clone();
        if let Some(title) = meta.get("title").and_then(Value::as_str) {
            if !title.trim().is_empty() {
                entry.title = title.trim().to_string();
            }
        }
        entry.tags = meta
            .get("tags")
            .and_then(Value::as_array)
            .map(|items| items.iter().filter_map(|item| item.as_str().map(str::to_string)).collect())
            .unwrap_or_default();
        entry.files = meta
            .get("files")
            .and_then(Value::as_array)
            .map(|items| items.iter().filter_map(|item| item.as_str().map(str::to_string)).collect())
            .unwrap_or_default();
        entry.supersedes = meta
            .get("supersedes")
            .and_then(Value::as_array)
            .map(|items| items.iter().filter_map(|item| item.as_str().map(str::to_string)).collect())
            .unwrap_or_default();
        entry.importance = meta
            .get("importance")
            .and_then(Value::as_f64)
            .unwrap_or(0.5)
            .clamp(0.0, 1.0);
        entry.pinned = meta.get("pinned").and_then(Value::as_bool).unwrap_or(false);
        entry.starred = meta.get("starred").and_then(Value::as_bool).unwrap_or(false);
        entry.created_at = meta.get("created").and_then(Value::as_i64).unwrap_or_else(now_ms);
        entry.updated_at = meta.get("updated").and_then(Value::as_i64).unwrap_or(entry.created_at);
        entry.valid_from = entry.created_at;
        entry.body = body;
        entry.hash = super::model::content_hash(&entry.body);
        // 容器文件名就是日期；条目自己的时间只精确到分钟
        let _ = path;
        out.push(entry);
    }
}

/// 读全部记忆（含回收站）。重建索引与列表页共用这一份事实读取。
///
/// ── 为什么这里有一层进程内快照 ──────────────────────────────
/// 记忆库是「一天一文件的容器」形态：5818 条记忆压在 65 个 `.md` 里，合计 55 MB。
/// 一次 `load_all()` 要把 55 MB 全部读进内存、按 `<!-- mem -->` 标记切成 5818 段、
/// 每段算一次 SHA-256 —— 实测 300 ms 上下。而**一个页面打开就要读三遍**：
///
///   `GET /api/memory/config`  → `status()` → `load_all()`
///                            → `projects()` → `load_all()`（只为数个项目数）
///   `GET /api/memory/projects` → `projects()` → `load_all()`
///
/// 三遍 900 ms 的纯 IO + 解析，全压在同一屏上，用户看到的就是「正在读取记忆库…」
/// 转半天。AgentHub 那边之所以「很快就出数据」，是因为它读的是 SQLite 索引，
/// 不碰原始文件。
///
/// 所以这里加一层「按目录指纹校验的快照」：
///   * 命中（指纹没变）→ 只花一次 stat 遍历（65 个文件约 8 ms），零解析；
///   * 未命中 → 重读一遍，并把新快照换上去；
///   * 写路径主动 [`invalidate`]（见 `save` / `move_save` / `end_deferred` 等）。
///
/// ── 为什么不干脆只读索引 ────────────────────────────────────
/// 索引是**派生**数据，事实源是 `.md`。用户手编一个记忆文件、同步落盘拷回来一批、
/// 或者索引库坏了要重建 —— 这些场景下「只信索引」会读出过期内容。快照校验的是
/// 文件系统本身（文件名 + 体积 + mtime），比任何一层缓存都更接近事实源。
pub fn snapshot() -> std::sync::Arc<Vec<MemoryEntry>> {
    let root = root();
    let stamp = fingerprint(&root);
    // 先查缓存（持锁时间只有一次 Arc 克隆，微秒级）
    if let Ok(guard) = snapshot_slot().lock() {
        if let Some(slot) = guard.as_ref() {
            if slot.stamp == stamp && slot.root == root {
                return std::sync::Arc::clone(&slot.entries);
            }
        }
    }
    // 读盘刻意在锁外：锁内读 55 MB 会让所有并发读者排队等同一份 IO，
    // 而「两个线程同时发现失效、各读一遍」的代价只是多一次读盘 —— 写路径会
    // 主动 invalidate，所以这种重复只出现在「刚写完的那一瞬间」。
    let entries = std::sync::Arc::new(load_all_raw());
    if let Ok(mut guard) = snapshot_slot().lock() {
        *guard = Some(Snapshot { stamp, root, entries: std::sync::Arc::clone(&entries) });
    }
    entries
}

/// 丢缓存。所有写路径都要调 —— 少调一处的后果是「写完看不到」，
/// 而那种 bug 只能靠用户报「我明明存了」才会被发现。
pub fn invalidate() {
    if let Ok(mut guard) = snapshot_slot().lock() {
        *guard = None;
    }
}

/// 预热快照：后台线程启动时调一次，把「第一次打开记忆页要解析 55 MB」的
/// 代价从用户身上挪到启动阶段。
///
/// ── 为什么必须预热而不是靠懒加载 ────────────────────────────
/// `snapshot()` 未命中时要读全部 `.md`（本机 65 个文件 / 55 MB）、切成几千节、
/// 每节算一次 SHA-256 —— 实测 300~500 ms，冷盘上更久。这段时间原来完全由
/// **第一个打开记忆中枢的用户**承担，叠加 `status()` 与 `projects()` 就是
/// 那句「正在读取记忆库…」。
///
/// 启动后立刻在后台线程里跑一遍，用户点进页面时命中率接近 100%：指纹没变就
/// 只花一次 stat 遍历（约 8 ms）。失败无所谓 —— 懒加载那条路还在，只是慢一点，
/// 所以这里刻意不返回错误、不记 warning，只在日志里留一行耗时。
pub fn warm() {
    let started = std::time::Instant::now();
    let entries = snapshot();
    crate::server::logging::log(
        "[Memory]",
        &format!(
            "记忆快照预热完成：{} 条 / {} ms（首屏将命中缓存）",
            entries.len(),
            started.elapsed().as_millis()
        ),
    );
}

struct Snapshot {
    /// 目录指纹（见 [`fingerprint`]）
    stamp: u64,
    /// 记忆根。`storage.root` 可以被改，换了根必须重读
    root: PathBuf,
    entries: std::sync::Arc<Vec<MemoryEntry>>,
}

fn snapshot_slot() -> &'static std::sync::Mutex<Option<Snapshot>> {
    static SLOT: std::sync::Mutex<Option<Snapshot>> = std::sync::Mutex::new(None);
    &SLOT
}

/// 目录指纹：把「哪些 `.md`、各多大、各什么时候改的」压成一个 u64。
///
/// ── 为什么不直接比 mtime 最大值 ────────────────────────────
/// 同步工具回滚、用户从备份拷回一批旧文件、删除一个容器 —— 这些都会让「最大值」
/// 变小或不变，但内容已经变了。把每个文件的名字 + 体积 + mtime 都喂进哈希里，
/// 三种情况都能抓到。
///
/// ── 为什么用纳秒级 mtime ───────────────────────────────────
/// 容器是整份重写的（不是追加），同一秒内连写两次的场景真实存在（导入批末
/// 落盘 + 紧接着一次手动写入）。秒级精度会把第二次判成「没变」。
fn fingerprint(root: &Path) -> u64 {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};

    fn walk<H: Hasher>(dir: &Path, hasher: &mut H) {
        let Ok(reader) = fs::read_dir(dir) else { return };
        let mut items: Vec<PathBuf> = reader.flatten().map(|item| item.path()).collect();
        // 排序：`read_dir` 的枚举顺序不保证稳定，不排序会让同一份目录算出两个指纹
        items.sort();
        for path in items {
            if path.is_dir() {
                path.file_name().hash(hasher);
                walk(&path, hasher);
                continue;
            }
            if path.extension().and_then(|ext| ext.to_str()) != Some("md") {
                continue;
            }
            let Ok(meta) = fs::metadata(&path) else { continue };
            path.file_name().hash(hasher);
            meta.len().hash(hasher);
            meta.modified()
                .ok()
                .and_then(|stamp| stamp.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|age| age.as_nanos() as u64)
                .hash(hasher);
        }
    }

    let mut hasher = DefaultHasher::new();
    root.to_string_lossy().hash(&mut hasher);
    walk(root, &mut hasher);
    hasher.finish()
}

/// 无缓存的全量读取。只有 [`snapshot`] 在缓存失效时调它。
fn load_all_raw() -> Vec<MemoryEntry> {
    let mut out = Vec::new();
    read_dir_entries(&general_root(), &mut out);
    read_dir_entries(&projects_root(), &mut out);
    read_dir_entries(&profile_root(), &mut out);
    read_dir_entries(&trash_root(), &mut out);
    // 回收站里的文件以 `trash/` 为准：手改过 frontmatter 忘了写 deleted_at 时，
    // 「文件在 trash 里」这件事本身就说明它被删了。
    for entry in &mut out {
        if entry.deleted_at.is_none() && entry.path.starts_with("trash/") {
            entry.deleted_at = Some(entry.updated_at.max(1));
        }
        if entry.hash.is_empty() {
            entry.hash = super::model::content_hash(&entry.body);
        }
    }
    out.sort_by(|a, b| b.updated_at.cmp(&a.updated_at));
    out
}

/// 读全部记忆（含回收站）。**返回独立副本** —— 需要 `Arc` 共享的调用方用
/// [`snapshot`]，省掉一次 55 MB 的深拷贝。
pub fn load_all() -> Vec<MemoryEntry> {
    (*snapshot()).clone()
}

/// 把「容器里节的日期」与「容器文件名上的日期」不一致的容器拆开，各自归位。
///
/// ── 这是给已经写错的存量数据做的一次性修复 ────────────────────
/// `entry_path` 曾经用 `created_at.max(updated_at)` 算落点，等于永远取
/// `updated_at`。历史导入补录九月的会话时，`created_at` 是九月、
/// `updated_at` 是此刻 —— 于是 53 个不同日期的几千条全挤进「今天」那个容器，
/// 文件涨到几十 MB，再被单文件上限整份跳过，索引里一条不剩。
///
/// 只改计算逻辑救不了这些数据：文件已经写在那儿了，`entry.path` 也指着它。
/// 所以这里主动拆一次：按每节的 `created` 重新分组，写回各自日期的容器。
///
/// ── 为什么不直接改 `entry.path` 了事 ────────────────────────
/// 改了也没用：事实源还是那个几十 MB 的文件，「同一天」的定义仍然是错的，
/// 下次重建索引依旧从同一个文件里读出错日期。必须真的把内容搬到新文件，
/// 并把老容器里搬走的那几节摘掉（摘空了就删文件）。
///
/// 返回 `(搬走的节数, 涉及的容器数, 写失败数)`。前两个为 0 表示没有需要修的
/// 历史数据。
///
/// ── 为什么失败数要单独返回 ──────────────────────────────────
/// 这个函数是**破坏性**的：它会把节从老容器里摘掉。摘的判据必须是「目标文件
/// 已经真的写成功了」，而不是「我已经打算写了」。写失败还照摘，等于把记忆从
/// 磁盘上抹掉一次 —— 而且因为源文件里本来就错，用户连「少了一条」都发现不了。
/// 所以流程固定为：**先写目标 → 确认成功 → 才从源里摘**。失败数 > 0 说明还有
/// 数据没归位，迁移标记不能落，下次启动会重试。
pub fn split_misplaced_containers() -> (usize, usize, usize) {
    let mut moved = 0usize;
    let mut files = 0usize;
    let mut failed = 0usize;
    // ── 有人在攒批就先不拆 ────────────────────────────────────
    // 批量导入用延迟窗口在内存里攒「某个容器的最终文本」，批末整份写回。这时把
    // 那几节搬走、老容器摘空，批末那一写会把搬走之前的整份文本盖回去 ——
    // 同一批节于是同时存在于两个容器。这个竞态不能靠 `lock_writes` 解（窗口内
    // 根本不持锁），只能在这一侧退让。跳过不算失败：下次启动 / 下次重建会再来。
    if deferred_active() {
        crate::server::logging::log(
            "[Memory]",
            "⏸️ 有批量写入正在攒批，跳过 l1 容器归位（下次启动或重建索引时再试）",
        );
        return (0, 0, 0);
    }
    let mut containers: Vec<PathBuf> = Vec::new();
    collect_containers(&general_root(), &mut containers);
    collect_containers(&projects_root(), &mut containers);
    for path in containers {
        // ── 读 + 算 + 写 全在一个临界区里 ──────────────────────
        // 「读老容器 → 摘节 → 回写」中间插进一次并发 `save` 的话，那一节会被
        // 这一份整份写回覆盖掉（正是 `lock_writes` 存在的原因）。逐文件加锁
        // 而不是整个函数一把：几千个文件逐个处理，锁的持有时间是一次的量级。
        let _guard = lock_writes();
        let Ok(text) = fs::read_to_string(&path) else { continue };
        let relative = rel(&path);
        let Some(file_date) = relative
            .split('/')
            .next_back()
            .map(|name| name.trim_end_matches(".md").to_string())
        else {
            continue;
        };
        let (head, sections) = match container_sections(&text) {
            Some(parts) => parts,
            None => continue,
        };
        // 按节的 `created` 分组；同一天的全留在原处，不同日期的搬走。
        //
        // ── 三种「不搬」的情况 ──────────────────────────────────
        // 1. 没有 marker / marker 里没有 `created`：不知道它属于哪天，猜就是
        //    制造第二份错数据；
        // 2. `id` 为空（用户手写段落 / 早期格式）：搬过去无法按 id 判重，
        //    搬一半失败会造出重复段落；
        // 3. 日期本来就等于文件名日期。
        // 这三种一律原地不动 —— 迁移的收益是「让能归位的归位」，不是「一条都
        // 不许留在原地」。
        let mut keep: Vec<(String, String)> = Vec::new();
        let mut relocate: std::collections::BTreeMap<String, Vec<(String, String)>> =
            std::collections::BTreeMap::new();
        for (id, body) in sections {
            let created = body
                .lines()
                .next()
                .and_then(parse_marker)
                .and_then(|meta| meta.get("created").and_then(Value::as_i64));
            match created {
                Some(stamp) if !id.is_empty() && super::search::format_date(stamp) != file_date => {
                    let day = super::search::format_date(stamp);
                    relocate.entry(day).or_default().push((id, body));
                }
                _ => keep.push((id, body)),
            }
        }
        if relocate.is_empty() {
            continue;
        }
        files += 1;
        // 目标容器 = 同目录下的 `<该日期>.md`（agent 目录不变，只换日期）。
        let dir = path.parent().map(Path::to_path_buf).unwrap_or_else(|| path.clone());
        for (day, items) in relocate {
            let target = dir.join(format!("{day}.md"));
            let existing = fs::read_to_string(&target).unwrap_or_default();
            let mut out = if existing.trim().is_empty() {
                // 目标还没有文件：沿用老容器的头（agent / project 一致），
                // 但**日期必须换成目标那天的** —— frontmatter 里的 `date:` 是
                // 人读这个文件时的第一行，写着 11-20 的文件摆在 09-21.md 里
                // 只会让下一次排查再错一遍。
                redate_head(&head, &day)
            } else {
                // 已经有文件：原样保留它的头与内容，只在末尾追加
                let mut text = existing;
                if !text.ends_with('\n') {
                    text.push('\n');
                }
                if !text.ends_with("\n\n") {
                    text.push('\n');
                }
                text
            };
            let known: std::collections::HashSet<String> = container_sections(&out)
                .map(|(_, sections)| sections.into_iter().map(|(id, _)| id).collect())
                .unwrap_or_default();
            let mut written: Vec<String> = Vec::new();
            let mut pending: Vec<(String, String)> = Vec::new();
            for (id, body) in items.iter() {
                // 目标里已有同 id（之前搬过一次 / 本来就在）就不再写第二份。
                // 它不算「搬走」（moved 不计），但源里那一节可以放心摘掉 ——
                // 内容在目标里确实存在。
                if known.contains(id) {
                    continue;
                }
                out.push_str(body.trim_end());
                out.push('\n');
                out.push('\n');
                written.push(id.clone());
                pending.push((id.clone(), body.clone()));
            }
            if written.is_empty() {
                // 全部已在目标里：目标不需要重写，源里那几节可以放心摘
                continue;
            }
            match write_atomic(&target, &out) {
                Ok(()) => moved += written.len(),
                Err(error) => {
                    // 写失败 → 这一天的节**留在老容器里**，下次再试。
                    // 绝不「先摘后写」：那等于把记忆从磁盘上抹掉一次。
                    //
                    // 只退回 `pending`（本次真要写的那些），不退回 `items` 全部：
                    // 已经存在于目标的那几节如果也留在源里，同一个 id 会同时存在
                    // 于两个容器，`load_all` 读出来是两条、索引按 id 只留一条 ——
                    // 检索能命中但列表会闪一次，属于自己造出来的脏数据。
                    failed += 1;
                    crate::server::logging::log(
                        "[Memory]",
                        &format!("⚠️ 拆分历史容器失败 {}：{error}", target.display()),
                    );
                    keep.extend(pending);
                }
            }
        }
        // 老容器只留 keep：上面的搬走节已确认落到目标，没落的已退回 keep。
        let mut out = head.clone();
        for (_, body) in &keep {
            out.push_str(body.trim_end());
            out.push('\n');
            out.push('\n');
        }
        if keep.is_empty() {
            // 一节点都没留下 → 删掉这个空壳容器（只剩头的文件没有意义，
            // 而且它会一直作为「一天」出现在浏览页里）。
            //
            // ── 删之前必须留一份原始字节 ──────────────────────────
            // 这是本模块唯一「没有任何东西可回退」的操作：源文件被删、内容是
            // 从它里面搬出去的。搬错一次（比如某节的 `created` 本身是脏的）
            // 就再也拼不回原样。所以先按 `storage.backupBeforeWrite` 的口径
            // 拷一份 `<文件名>.md.bak`：`.bak` 不是 `.md`，不会被 `load_all`
            // 读成记忆，也不会进同步包。
            if super::config::bool_of("storage.backupBeforeWrite") {
                backup(&path);
            }
            if let Err(error) = fs::remove_file(&path) {
                failed += 1;
                crate::server::logging::log(
                    "[Memory]",
                    &format!("⚠️ 删除已清空的历史容器失败 {}：{error}", path.display()),
                );
            }
        } else if out.trim_end() != text.trim_end() {
            // 没变化就不重写：避免给每个正常容器都刷一次 .bak、动一次 mtime
            if let Err(error) = write_atomic(&path, &out) {
                failed += 1;
                crate::server::logging::log(
                    "[Memory]",
                    &format!("⚠️ 回写历史容器失败 {}：{error}", path.display()),
                );
            }
        }
    }
    (moved, files, failed)
}

/// 一次修复只做一次：跑完把标记落盘，下次启动不再扫全量。
///
/// ── 为什么需要标记 ──────────────────────────────────────────
/// [`split_misplaced_containers`] 要遍历并解析全部 l1 容器。正常用户的容器
/// 数量是几十到几百，扫一遍是毫秒级；但「全部正常」也要走一遍目录树 —— 放在
/// 每次启动的索引自检里，等于每次启动都多付一份全量解析的成本，换来的收益
/// 是 0。
///
/// 标记落盘 = 「这套数据已经修过了」。写在 `migrations.json`（与记忆根同级），
/// 不放 `general/` 之类的同步目录：这是**本机数据状态**，不是记忆内容，
/// 跟着同步包走会让另一台机器误以为自己已经修过。
///
/// ── 什么时候会重跑 ──────────────────────────────────────────
/// - 修复过程中有写失败（`failed > 0`）：不落标记，下次启动重试；
/// - 用户手动点「重建索引」：`rebuild()` 无条件再拆一次（幂等，代价可接受），
///   用来兜「有人把旧备份文件又拷回来了」这种外力改动。
pub fn split_misplaced_once() -> Option<(usize, usize, usize)> {
    // 有批量写入正在攒批：什么都不做，**也不落标记** —— 这次根本没修，落了标记
    // 就等于把「还没修」记成「修过了」。返回 None 让调用方当成「没跑」。
    if deferred_active() {
        return None;
    }
    let marker = migrations_file();
    let done = |text: &str| {
        serde_json::from_str::<Value>(text)
            .ok()
            .and_then(|value| value.get(MIGRATION_SPLIT).and_then(Value::as_bool))
            .unwrap_or(false)
    };
    if let Ok(text) = fs::read_to_string(&marker) {
        if done(&text) {
            return None;
        }
    }
    let result = split_misplaced_containers();
    // 有写失败就不落标记：说明还有节没归位，下次启动要再试一遍。
    // 落标记的语义是「这套数据已经修完了」，不是「我试过了」。
    if result.2 == 0 {
        let mut payload = fs::read_to_string(&marker)
            .ok()
            .and_then(|text| serde_json::from_str::<Value>(&text).ok())
            .filter(Value::is_object)
            .unwrap_or_else(|| json!({}));
        if let Some(map) = payload.as_object_mut() {
            map.insert(MIGRATION_SPLIT.to_string(), json!(true));
        }
        let _ = write_atomic(&marker, &payload.to_string());
    }
    Some(result)
}

/// 迁移标记里「容器归位」这一项的键名。改语义就换名字，别改值 —— 换名字等于
/// 让所有存量机器重跑一次，改值等于悄悄跳过。
const MIGRATION_SPLIT: &str = "splitMisplacedContainers";

/// 本机一次性修复的完成标记（不同步、不进索引）。
pub fn migrations_file() -> PathBuf {
    root().join("migrations.json")
}

/// 递归收集「一天一文件」的容器路径（只认形状，不看内容）。
fn collect_containers(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(reader) = fs::read_dir(dir) else { return };
    let mut items: Vec<PathBuf> = reader.flatten().map(|item| item.path()).collect();
    items.sort();
    for path in items {
        if path.is_dir() {
            collect_containers(&path, out);
            continue;
        }
        if looks_like_daily_container_path(&path) {
            out.push(path);
        }
    }
}

/// 把容器头的 `date:` 换成指定日期，其余行原样保留。
///
/// 不重新生成整块头：老容器头里可能有用户手写的说明（`container_head` 只写
/// 五行的那种是新建时的样子，改过的不一定还长那样）。逐行替换一行是最小的
/// 改动面。
fn redate_head(head: &str, date: &str) -> String {
    let mut out = String::with_capacity(head.len());
    for line in head.lines() {
        if line.starts_with("date:") {
            out.push_str(&format!("date: {date}\n"));
        } else {
            out.push_str(line);
            out.push('\n');
        }
    }
    out
}

// ── 项目登记表 ───────────────────────────────────────────────

/// 项目卡（界面上「项目」列表的数据）
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Project {
    pub slug: String,
    pub name: String,
    #[serde(default)]
    pub remote: Option<String>,
    #[serde(default)]
    pub path: Option<String>,
    #[serde(default)]
    pub created_at: i64,
    #[serde(default)]
    pub updated_at: i64,
    /// 远程别名（origin / upstream / …）
    #[serde(default)]
    pub remotes: Vec<String>,
    /// 用户手动合并到另一个 slug（别名 → 主项目）
    #[serde(default)]
    pub merged_into: Option<String>,
    /// 固定显示在列表顶部
    #[serde(default)]
    pub pinned: bool,
    /// 归档（不再接受新记忆，但仍可检索）
    #[serde(default)]
    pub archived: bool,
}

pub fn load_projects() -> Vec<Project> {
    let Ok(text) = fs::read_to_string(projects_file()) else {
        return Vec::new();
    };
    let Ok(value) = serde_json::from_str::<Value>(&text) else {
        return Vec::new();
    };
    value
        .get("projects")
        .and_then(|list| serde_json::from_value::<Vec<Project>>(list.clone()).ok())
        .unwrap_or_default()
}

pub fn save_projects(projects: &[Project]) -> Result<(), String> {
    ensure_layout()?;
    let payload = json!({ "projects": projects });
    let text = serde_json::to_string_pretty(&payload).map_err(|error| error.to_string())?;
    write_atomic(&projects_file(), &text)
}

/// 登记（或更新）一个项目。已存在则只补空缺字段 —— 用户手填的本地路径、
/// 归档标记不该被一次自动归类覆盖。
pub fn registry_upsert(slug: &str, name: &str, cwd: &str, remote: &str) -> Project {
    let slug = slugify(slug);
    // 读整份登记表 → 补一条 → 整份写回。并发写不同项目时后者会让前者凭空消失，
    // 所以整段持锁。注意这里**不能**再调 `store::save`（不会，只写 projects.json），
    // 因此与 [`WRITE_LOCK`] 没有嵌套。
    let _guard = lock_projects();
    let mut projects = load_projects();
    let now = now_ms();
    let name = if name.trim().is_empty() { slug.clone() } else { name.trim().to_string() };
    if let Some(existing) = projects.iter_mut().find(|item| item.slug == slug) {
        existing.updated_at = now;
        if existing.name.trim().is_empty() || existing.name == existing.slug {
            existing.name = name;
        }
        if existing.path.is_none() && !cwd.trim().is_empty() {
            existing.path = Some(cwd.to_string());
        }
        if existing.remote.is_none() && !remote.trim().is_empty() {
            existing.remote = Some(remote.to_string());
        }
        let cloned = existing.clone();
        let _ = save_projects(&projects);
        return cloned;
    }
    let project = Project {
        slug: slug.clone(),
        name,
        remote: (!remote.trim().is_empty()).then(|| remote.to_string()),
        path: (!cwd.trim().is_empty()).then(|| cwd.to_string()),
        created_at: now,
        updated_at: now,
        remotes: Vec::new(),
        merged_into: None,
        pinned: false,
        archived: false,
    };
    projects.push(project.clone());
    let _ = save_projects(&projects);
    project
}

/// 旧接口（`write` 里调）：把「用户/AI 说的项目名 + cwd」登记成项目卡。
pub fn touch_project(name: &str, cwd: Option<&str>) -> Project {
    let classified = match cwd {
        Some(cwd) if !cwd.trim().is_empty() => layout::classify_explicit(name, Some(cwd)),
        _ => layout::classify_explicit(name, None),
    };
    // `classify_explicit` 已经把结果登记过，这里只把它读回来（避免二次写盘）
    let slug = classified.slug.unwrap_or_else(|| slugify(name));
    load_projects()
        .into_iter()
        .find(|item| item.slug == slug)
        .unwrap_or_else(|| registry_upsert(&slug, name, cwd.unwrap_or_default(), ""))
}

// ── 设备身份 ─────────────────────────────────────────────────

/// 本机设备 id。第一次调用时生成并落盘（`device.json`）。
///
/// 为什么要它：同步回来的一堆记忆分不清「这台机器写的」与「那台机器写的」，
/// 冲突裁决就无从下手；「谁在什么时候改过这条」也答不出来。
pub fn device_id() -> String {
    static CACHE: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    CACHE
        .get_or_init(|| {
            if let Ok(text) = fs::read_to_string(device_file()) {
                if let Ok(value) = serde_json::from_str::<Value>(&text) {
                    if let Some(id) = value.get("id").and_then(Value::as_str) {
                        if !id.trim().is_empty() {
                            return id.to_string();
                        }
                    }
                }
            }
            let id = format!("{:08x}{:08x}", random_u32(), random_u32());
            let payload = json!({
                "id": id,
                "name": hostname(),
                "createdAt": now_ms(),
            });
            if ensure_layout().is_ok() {
                let text = serde_json::to_string_pretty(&payload).unwrap_or_default();
                let _ = write_atomic(&device_file(), &text);
            }
            id
        })
        .clone()
}

/// 系统电脑名。
///
/// Windows 上 `COMPUTERNAME` 就是「此电脑 → 属性」里那个名字；Unix 上退到
/// `HOSTNAME`（`gethostname` 在多数发行版也塞进这个环境变量）。不引
/// `hostname` / `sysinfo` 这类 crate —— 这里要的只是「界面上默认填什么」，
/// 取不到时给空串让界面自己回落，不值得为它加一个依赖。
///
/// 为什么不给 `"unknown"`：那个词会被当成一个真实的机器名写进配置、传到
/// 仪表盘和设备列表里，用户看到会以为自己的机器叫什么 unknown。空串的语义
/// 是「没取到」，界面据此保留占位符。
pub(crate) fn hostname() -> String {
    std::env::var("COMPUTERNAME")
        .or_else(|_| std::env::var("HOSTNAME"))
        .unwrap_or_default()
}

/// 进程内伪随机（不引 rand：设备 id 只需要「本机唯一」，不需要密码学强度）。
fn random_u32() -> u32 {
    use std::hash::{Hash, Hasher};
    static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    now_ms().hash(&mut hasher);
    std::process::id().hash(&mut hasher);
    COUNTER
        .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        .hash(&mut hasher);
    hasher.finish() as u32
}

// ── 回收站维护 ───────────────────────────────────────────────

/// 清掉超过保留天数的回收站文件。返回清掉的条数。
pub fn purge_expired_trash(days: usize) -> usize {
    let days = days.max(1) as i64;
    let cutoff = now_ms() - days * 24 * 3600 * 1000;
    // 与 `save` 里「进回收站」那一步互斥：定时清理正删这一份时，
    // 并发的 `save(deleted_at)` 可能刚把它写出来，两边都以为处理完了。
    let _guard = lock_writes();
    let Ok(reader) = fs::read_dir(trash_root()) else { return 0 };
    let mut removed = 0usize;
    for item in reader.flatten() {
        let path = item.path();
        if path.extension().and_then(|ext| ext.to_str()) != Some("md") {
            continue;
        }
        // 用文件 mtime 而不是 frontmatter：文件被搬进回收站会刷新 mtime，
        // 而 `deleted_at` 可能被手改过。清回收站不需要那么精确。
        let stamp = fs::metadata(&path)
            .ok()
            .and_then(|meta| meta.modified().ok())
            .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|duration| duration.as_millis() as i64)
            .unwrap_or(0);
        if stamp > 0 && stamp < cutoff && fs::remove_file(&path).is_ok() {
            removed += 1;
        }
    }
    if removed > 0 {
        invalidate();
    }
    removed
}

/// 清掉超过保留天数的**同步删除留档**（见 [`sync_trash_root`]）。返回删掉的目录数。
///
/// ── 为什么要清 ──────────────────────────────────────────────
/// 留档是「删错了还能捞回来」的保险，不是永久归档。l1 容器一天一份、每份几十 KB，
/// 一次误判的批量删除会留下上百份副本；不清的话它会随同步次数单调增长，
/// 而用户根本不知道那个目录存在。所以给它和回收站同一个保留窗口。
///
/// ── 为什么按「批次目录」整份删 ────────────────────────────────
/// 一个批次目录（`<时刻>-<设备>/`）是同一轮同步删掉的所有文件，它们的价值是同
/// 一份：要捞就是整批一起捞，捞一条没有意义（其它同批的早被同样的判断删了）。
/// 按目录删也让「删了一半」不会发生 —— 保留窗口到了就整批走。
///
/// 按目录 mtime 判：目录 mtime 是「最后一次往里写文件」的时刻，也就是这一批
/// 删除真正完成的时刻，比 `created_at` 这类记账字段可信。
pub fn purge_expired_sync_trash(days: usize) -> usize {
    let days = days.max(1) as i64;
    let cutoff = now_ms() - days * 24 * 3600 * 1000;
    let Ok(reader) = fs::read_dir(sync_trash_root()) else { return 0 };
    let mut removed = 0usize;
    for item in reader.flatten() {
        let path = item.path();
        if !path.is_dir() {
            continue;
        }
        let stamp = fs::metadata(&path)
            .ok()
            .and_then(|meta| meta.modified().ok())
            .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|duration| duration.as_millis() as i64)
            .unwrap_or(0);
        // `stamp == 0` 说明读不到 mtime（权限 / 文件系统不支持）：这种**不删**。
        // 留档本身就是最后一道保险，读不准的时候留下比删掉安全。
        if stamp > 0 && stamp < cutoff && fs::remove_dir_all(&path).is_ok() {
            removed += 1;
        }
    }
    removed
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::server::core::memory::model::Layer;

    #[test]
    fn slug_is_path_safe() {
        assert_eq!(slugify("Agent2API"), "agent2api");
        assert_eq!(slugify("a/b c"), "a-b-c");
        assert_eq!(slugify("中文项目"), GLOBAL_SLUG);
        assert_eq!(slugify("  "), GLOBAL_SLUG);
    }

    #[test]
    fn entry_path_follows_layer_layout() {
        let mut entry = MemoryEntry::draft("m1", "t", MemoryKind::Knowledge, "body");
        entry.project = Some("Agent2API".into());
        entry.layer = Layer::L2;
        let path = rel(&entry_path(&entry, false));
        assert_eq!(path, "projects/agent2api/l2/knowledge/m1.md");

        let mut daily = MemoryEntry::draft("d1", "t", MemoryKind::Daily, "body");
        daily.agent = Some("workbuddy".into());
        daily.created_at = 1_700_000_000_000;
        let path = rel(&entry_path(&daily, false));
        assert!(path.starts_with("general/l1/workbuddy/"), "实际 {path}");
        assert!(path.ends_with(".md"));

        let trashed = rel(&entry_path(&entry, true));
        assert_eq!(trashed, "trash/m1.md");
    }

    #[test]
    fn daily_container_is_recognised_by_shape() {
        assert!(is_daily_container("general/l1/workbuddy/2026-11-16.md"));
        assert!(is_daily_container("projects/o--r/l1/manual/2026-11-16.md"));
        // 同目录下的单条文件（非 daily 的 l1 条目）不是容器
        assert!(!is_daily_container("projects/o--r/l1/manual/2026-11-16-id.md"));
        assert!(!is_daily_container("projects/o--r/l2/decisions/2026-11-16.md"));
    }

    #[test]
    fn container_sections_round_trip() {
        let mut first = MemoryEntry::draft("a1", "第一条", MemoryKind::Daily, "正文一");
        first.agent = Some("workbuddy".into());
        first.tags = vec!["标签,带逗号".into()];
        let mut second = MemoryEntry::draft("a2", "第二条", MemoryKind::Daily, "正文二");
        second.agent = Some("workbuddy".into());

        let mut text = container_head("2026-11-16", "workbuddy", None);
        text.push_str(&section_text(&first));
        text.push_str(&section_text(&second));
        let sections = split_sections(&text);
        assert_eq!(sections.len(), 2);
        assert_eq!(sections[0].0.get("id").and_then(Value::as_str), Some("a1"));
        assert_eq!(sections[0].1.trim(), "正文一");
        assert_eq!(sections[1].1.trim(), "正文二");
    }

    #[test]
    fn strip_section_removes_only_target() {
        let first = MemoryEntry::draft("a1", "第一条", MemoryKind::Daily, "正文一");
        let second = MemoryEntry::draft("a2", "第二条", MemoryKind::Daily, "正文二");
        let mut text = container_head("2026-11-16", "manual", None);
        text.push_str(&section_text(&first));
        text.push_str(&section_text(&second));
        let (cleaned, removed) = strip_section(&text, "a1");
        assert!(removed);
        assert!(!cleaned.contains("正文一"));
        assert!(cleaned.contains("正文二"));
    }

    #[test]
    fn marker_with_awkward_tags_parses_back() {
        let mut entry = MemoryEntry::draft("x1", "标题 里 有 空格", MemoryKind::Daily, "b");
        entry.tags = vec!["含,逗号".into(), "含\"引号\"".into()];
        let line = section_marker(&entry);
        let meta = parse_marker(line.trim_end()).expect("能解析");
        assert_eq!(meta.get("title").and_then(Value::as_str), Some("标题 里 有 空格"));
        assert_eq!(meta.get("tags").and_then(Value::as_array).map(Vec::len), Some(2));
    }

    /// 造一份「某一天的 l1 容器」。
    ///
    /// ── 为什么时间必须固定 ──────────────────────────────────
    /// 每节的原始切片里带着 `## HH:MM · 标题` 这一行（取自 `created_at`），
    /// 以及 marker JSON 里的 `created` / `updated`。`merge_container` 判「同 id
    /// 两节是否一致」用的就是这段原始切片，于是**测试里两次构造必须落在同一
    /// 分钟**，否则同一个 id 会因为标题行的时间戳不同被判成冲突。
    ///
    /// 2026-11-20 的 Windows CI 就这么红过一次：`container_merge_joins_each_sides_own_sections`
    /// 的 local / remote 两次 `draft()` 之间跨了分钟，`expect("能拼")` 直接炸。
    /// 那不是平台差异，是测试撞上了分钟边界 —— 换成固定时间就彻底确定。
    ///（真实文件里两边共有节的字节来自同一次写入，不存在这个差异。）
    ///
    /// 值取 2025-11-16T00:00:00Z。节标题只渲染 `HH:MM`（`created_at mod 一天`），
    /// 所以具体哪天不影响断言；要的是**每次构造都落在同一分钟**。
    const FIXED_AT: i64 = 1_763_251_200_000;

    fn container(day: &str, entries: &[(&str, &str)]) -> String {
        let mut text = container_head(day, "manual", None);
        for (id, body) in entries {
            let mut entry = MemoryEntry::draft(*id, "标题", MemoryKind::Daily, *body);
            entry.created_at = FIXED_AT;
            entry.updated_at = FIXED_AT;
            text.push_str(&section_text(&entry));
        }
        text
    }

    #[test]
    fn container_merge_joins_each_sides_own_sections() {
        // 两台机器同一天各写了一条：共用 a1，各有一条对方没有的
        let local = container("2026-11-16", &[("a1", "共用那条"), ("a2", "本机新写的")]);
        let remote = container("2026-11-16", &[("a1", "共用那条"), ("a3", "对方新写的")]);
        let merged = merge_container(&local, &remote).expect("能拼");
        assert!(merged.contains("共用那条"));
        assert!(merged.contains("本机新写的"));
        assert!(merged.contains("对方新写的"));
        // 共用那一条不能重复出现（按 id 对齐，不是简单拼接）
        assert_eq!(merged.matches("共用那条").count(), 1);
    }

    #[test]
    fn container_merge_refuses_when_the_same_id_differs() {
        // 同一个 id 两边各改了一版 → 真冲突，不能猜
        let local = container("2026-11-16", &[("a1", "我改的版本")]);
        let remote = container("2026-11-16", &[("a1", "对方改的版本")]);
        assert!(merge_container(&local, &remote).is_none());
    }

    #[test]
    fn container_merge_refuses_different_days() {
        // 头不一样（不同日期 / 不同 agent）→ 根本不是同一个容器，不猜
        let local = container("2026-11-16", &[("a1", "一")]);
        let remote = container("2026-11-17", &[("a1", "一")]);
        assert!(merge_container(&local, &remote).is_none());
    }

    #[test]
    fn container_merge_refuses_plain_notes() {
        // 普通 l2 单条不是容器 → 不参与这种合并（它没有节 id 可对齐）
        let plain = "---\nid: x\ntitle: 结论\n---\n\n正文\n";
        assert!(merge_container(plain, plain).is_none());
    }

    #[test]
    fn container_merge_keeps_the_head_once() {
        // 头只能出现一次：拼出来的必须是「一份容器」，不是两份文件首尾相接
        let local = container("2026-11-16", &[("a1", "一")]);
        let remote = container("2026-11-16", &[("a2", "二")]);
        let merged = merge_container(&local, &remote).expect("能拼");
        assert_eq!(merged.matches("type: daily-log").count(), 1);
        assert_eq!(merged.matches("date: 2026-11-16").count(), 1);
    }

    #[test]
    fn container_merge_treats_empty_id_sections_by_content() {
        // 没有 id 的节（手写段落 / 早期格式）不能按「id 都是空串」对齐 ——
        // 那会把两段不同的正文判成「同一条改了两版」，明明能拼的判成冲突。
        let local = container("2026-11-16", &[("", "本机手写的一段")]);
        let remote = container("2026-11-16", &[("", "对方手写的一段")]);
        let merged = merge_container(&local, &remote).expect("无 id 的段落应能并进来");
        assert!(merged.contains("本机手写的一段"));
        assert!(merged.contains("对方手写的一段"));

        // 同一段完全相同 → 只留一份，不重复
        let same = merge_container(&local, &local).expect("能拼");
        assert_eq!(same.matches("本机手写的一段").count(), 1);
    }

    #[test]
    fn device_id_is_stable_within_process() {
        let first = device_id();
        let second = device_id();
        assert_eq!(first, second);
        assert_eq!(first.len(), 16);
    }

    /// 回归：`config_file()` 曾经写成 `root().join("config.json")`，而 `root()`
    /// 又要读 `storage.root` 配置，读配置又要先知道配置文件在哪 —— 三个函数首尾
    /// 相扣，`config::stored()` 一调就是无穷递归。测试线程栈只有 2MB，表现成
    /// `stack overflow, aborting`；主线程上是启动即崩。
    ///
    /// 这个测试的意义不在断言值，而在**它能在不爆栈的前提下跑完**：一旦有人
    /// 把 `config_file()` 改回依赖 `root()`，这里立刻复现那个环。
    #[test]
    fn config_path_does_not_depend_on_itself() {
        let path = config_file();
        assert!(path.ends_with("config.json"), "实际 {}", path.display());
        // 读一次配置（`stored()` 内部会去 stat 这个路径）必须能返回
        let verbose = crate::server::core::memory::config::bool_of("search.synonymsEnabled");
        let _ = verbose;
    }

    /// 并发写同一个当天容器时，每一节都必须留下来。
    ///
    /// 这是 [`lock_writes`] 的回归测试：没有锁时 `save_section_locked` 的
    /// 「读整份 → 摘 → 追加 → 整份写回」会互相覆盖，最后容器里只剩一节的
    /// 内容（而且**不会报任何错**）。二十个线程各写一条，断言条数正好二十。
    ///
    /// ── 为什么必须包在 `isolated` 里 ────────────────────────────
    /// `cfg(test)` 下 `paths::config_dir()` 取的是 **thread_local** 临时目录
    /// （`test_support::HOME`）。直接 `spawn` 二十个线程，每个线程拿到的是
    /// **各自的**目录，二十条落到二十个文件里，测试永远「通过」——它测不到
    /// 任何并发。`isolated` 让测试跑在一个子进程里、由 `AGENT2API_PROXY_HOME`
    /// 指定目录，`config_dir()` 于是走进程级路径，二十个线程才真的抢同一份文件。
    ///
    /// 为什么用 20 而不是 2：两个线程撞上的概率不高，20 个足以让覆盖稳定复现
    /// （无锁版本跑十次失败十次）。
    #[test]
    fn concurrent_writes_keep_every_section() {
        crate::test_support::isolated(
            concat!(
                module_path!(),
                "::concurrent_writes_keep_every_section"
            ),
            || {
                // 固定 agent + 固定日期 → 所有条目落到同一个 `l1/<agent>/<date>.md`
                let agent = "conc".to_string();
                let stamp = 1_700_000_000_000_i64;
                let mut handles = Vec::new();
                for index in 0..20 {
                    let agent = agent.clone();
                    handles.push(std::thread::spawn(move || {
                        let mut entry = MemoryEntry::draft(
                            format!("{agent}-{index}"),
                            format!("第 {index} 条"),
                            MemoryKind::Daily,
                            format!("并发正文 {index}"),
                        );
                        entry.agent = Some(agent);
                        entry.created_at = stamp;
                        entry.updated_at = stamp;
                        save(&entry).expect("写入应当成功");
                    }));
                }
                for handle in handles {
                    handle.join().expect("线程不应 panic");
                }

                let mut probe = MemoryEntry::draft("probe", "探针", MemoryKind::Daily, "x");
                probe.agent = Some(agent.clone());
                probe.created_at = stamp;
                // `entry_path` 按 `created_at` 定 l1 容器日期（历史条目按原始日期
                // 分布），所以只钉 created_at 就够定位到 stamp 那天；updated_at 一并
                // 设上是为了让探针与真实写入的条目形态一致，不依赖顺序。
                probe.updated_at = stamp;
                let path = entry_path(&probe, false);
                let text = fs::read_to_string(&path).expect("容器文件应当存在");
                let sections = text.lines().filter(|line| parse_marker(line).is_some()).count();
                assert_eq!(sections, 20, "并发写丢了节：实际 {sections} 节，文件 {}", path.display());
            },
        );
    }

    /// 把一条 daily 改成别的类型时，**同一天的其它条目必须还在**。
    ///
    /// 这是 [`detach_live_locked`] 判据的回归测试。旧实现用
    /// `live_is_container(entry)` 判「要不要按容器摘节」，而它看的是**这一份对象
    /// 自己的** `layer`/`kind`。改类型时调用方已经把 `kind` 换成新值（daily →
    /// knowledge），判据返回 false，于是落到「删单文件」分支：`entry_path` 算出来
    /// 正是当天的容器，`remove_file` 把整份容器删掉 —— 同一天其他条目一起没了，
    /// 而且不报任何错。改成按**路径形状**判之后不会认错。
    #[test]
    fn type_change_keeps_siblings_in_the_container() {
        crate::test_support::isolated(
            concat!(module_path!(), "::type_change_keeps_siblings_in_the_container"),
            || {
                let agent = "mover".to_string();
                let stamp = 1_700_000_000_000_i64;
                let seed = |id: &str, title: &str| {
                    let mut entry = MemoryEntry::draft(id, title, MemoryKind::Daily, "正文");
                    entry.agent = Some(agent.clone());
                    entry.created_at = stamp;
                    entry.updated_at = stamp;
                    save(&entry).expect("写入应当成功");
                    entry
                };
                let staying = seed("stays", "留下的一条");
                let mut moving = seed("moves", "要搬走的一条");

                let container = entry_path(&staying, false);
                let before = fs::read_to_string(&container).expect("容器应当存在");
                assert_eq!(
                    before.lines().filter(|line| parse_marker(line).is_some()).count(),
                    2,
                    "前置条件：容器里应当有两节"
                );

                // 模拟「改类型」：kind 换成 knowledge（会落到 L2 的独立文件），
                // 但磁盘上那一份仍然是当天的容器
                let previous = moving.clone();
                moving.kind = MemoryKind::Knowledge;
                moving.layer = Layer::L2;
                moving.path.clear();
                moving.path = rel(&entry_path(&moving, false));
                move_save(&previous, &moving).expect("换位保存应当成功");

                assert!(
                    fs::read_to_string(&container).is_ok(),
                    "容器被整份删掉了：{} —— 同一天的其它条目一起丢了",
                    container.display()
                );
                let after = fs::read_to_string(&container).expect("容器应当还在");
                let sections: Vec<String> = after
                    .lines()
                    .filter_map(parse_marker)
                    .filter_map(|meta| meta.get("id").and_then(Value::as_str).map(str::to_string))
                    .collect();
                assert_eq!(sections, vec!["stays".to_string()], "留下的应当只有 stays：{sections:?}");
                assert!(entry_path(&moving, false).is_file(), "新位置应当写出来了");
            },
        );
    }

    // ── 以下四组：l1 容器「落错日期 + 被体积上限跳过」那条 bug 链的回归 ──
    //
    // 事故现场：用户历史导入 2341 条 → 页面「普通记忆 0 条 / 深层记忆 9 条」。
    // 根因是两个 bug 串成一条链，任修一个都不够：
    //   1. `entry_path` 用 `created_at.max(updated_at)` 算日期 → 全部落进「今天」；
    //   2. `read_dir_entries` 拿「单文件上限」判容器 → 几十 MB 的容器整份跳过。
    // 下面每条测试都对应链条上的一环，并且都**能反证旧实现**：
    // 把对应那一行改回去，这条测试就红。

    /// 时间常量：只用来定日期，不参与「现在几点」的判断。
    /// 2026-09-20T00:00Z / 2026-09-21T12:00Z / 2026-11-20T09:00Z（UTC）。
    const DAY_0920: i64 = 1_789_862_400_000;
    const DAY_0921: i64 = 1_789_992_000_000;
    const DAY_1120: i64 = 1_795_165_200_000;

    /// 回归 1：容器日期跟 `created_at` 走，不跟 `updated_at` 走。
    ///
    /// 旧实现 `created_at.max(updated_at)` 必然取 `updated_at`（它恒 ≥
    /// `created_at`），于是「补录九月」这件事永远落进「今天」。这条测试把
    /// `updated_at` 设成 11-20、`created_at` 设成 09-20，断言落点是 09-20。
    #[test]
    fn container_date_follows_created_at_not_updated_at() {
        let mut entry = MemoryEntry::draft("h1", "补录的一条", MemoryKind::Daily, "九月那天的正文");
        entry.agent = Some("import".into());
        entry.created_at = DAY_0920;
        entry.updated_at = DAY_1120; // 导入时刻
        let path = rel(&entry_path(&entry, false));
        assert_eq!(
            path, "general/l1/import/2026-09-20.md",
            "历史条目的容器必须按原始发生日期，实际 {path}"
        );
    }

    /// 回归 2：`created_at` 缺失（0）时才退回 `updated_at`。
    ///
    /// 迁移过来的老数据可能没有 `created`。这时用 `updated_at` 是「唯一能拿到的
    /// 时间」，而不是「优先取更新的那个」—— 两者差别在于：前者只在缺值兜底，
    /// 后者会把所有历史条目都改成今天。
    #[test]
    fn container_date_falls_back_to_updated_at_only_when_created_is_missing() {
        let mut entry = MemoryEntry::draft("h2", "没写 created", MemoryKind::Daily, "正文");
        entry.agent = Some("import".into());
        entry.created_at = 0;
        entry.updated_at = DAY_0921;
        let path = rel(&entry_path(&entry, false));
        assert_eq!(path, "general/l1/import/2026-09-21.md", "实际 {path}");
    }

    /// 回归 3：超大 l1 容器必须照样读出来。
    ///
    /// 旧实现先按 `storage.maxFileSizeKB`（默认 512KB）判体积、再判形状，于是
    /// 一个 55MB 的当天容器被整份跳过 —— 里面每一节都没超限，整份文件超了。
    /// 页面因此显示「普通记忆 0 条」，而磁盘上文件好好地躺着。
    ///
    /// 这条测试真的造一个超限容器（把上限设成 16KB，写 40KB 正文），断言
    /// `load_all()` 读得到。断言条数而不只是「不 panic」：被跳过时是 0 条。
    #[test]
    fn oversized_daily_container_is_still_read() {
        crate::test_support::isolated(
            concat!(module_path!(), "::oversized_daily_container_is_still_read"),
            || {
                // 把上限压到 16KB，用 40KB 的容器去撞它。不压这个值就要写 512KB+
                // 的测试数据，既慢又占磁盘；压配置验的是同一个判据。
                crate::server::core::memory::config::patch(&json!({ "storage.maxFileSizeKB": 16 }))
                    .expect("配置可写");

                let body = "长".repeat(20_000); // UTF-8 下约 60KB
                let mut entry = MemoryEntry::draft("big1", "大容器里的一条", MemoryKind::Daily, body);
                entry.agent = Some("bulk".into());
                entry.created_at = DAY_0920;
                entry.updated_at = DAY_0920;
                save(&entry).expect("写入应当成功");

                let path = entry_path(&entry, false);
                let size = fs::metadata(&path).expect("容器文件应当存在").len();
                assert!(
                    size > 16 * 1024,
                    "前置条件：容器应当超过上限（实际 {size} 字节）"
                );

                let loaded = load_all();
                let hit = loaded.iter().find(|item| item.id == "big1");
                assert!(
                    hit.is_some(),
                    "超过单文件上限的 l1 容器被整份跳过了 —— 容器体积是「一天所有记忆」\
                     的体积，不是单条记忆的体积，不能用单文件上限判"
                );
                assert_eq!(hit.map(|item| item.layer), Some(Layer::L1));
            },
        );
    }

    /// 回归 4：非容器的单条 `.md` 仍然受单文件上限保护。
    ///
    /// 修 bug 3 时最容易犯的错是「把上限整个删掉」—— 那会让用户手扔进来的
    /// 一百兆导出文件每次都进 `load_all()`，检索页直接卡死。上限本身是对的，
    /// 错的是把它套在容器上。
    #[test]
    fn oversized_plain_note_is_still_skipped() {
        crate::test_support::isolated(
            concat!(module_path!(), "::oversized_plain_note_is_still_skipped"),
            || {
                crate::server::core::memory::config::patch(&json!({ "storage.maxFileSizeKB": 16 }))
                    .expect("配置可写");
                ensure_layout().expect("目录可建");
                // l2 的单条文件：形状不是容器（文件名不是日期），应当被上限挡住
                let target = root().join("general/l2/knowledge/huge.md");
                let text = format!(
                    "---\nid: huge\ntitle: 巨型单条\nkind: knowledge\nlayer: l2\ncreated: {DAY_1120}\n---\n\n{}\n",
                    "长".repeat(20_000)
                );
                write_atomic(&target, &text).expect("写入应当成功");

                let loaded = load_all();
                assert!(
                    !loaded.iter().any(|item| item.id == "huge"),
                    "非容器的巨型单条应当仍然被单文件上限跳过（否则一次检索要读上百兆）"
                );
            },
        );
    }

    /// 回归 5：落错日期的节能被拆回各自日期，且**拆分是幂等的**。
    ///
    /// 造一个「09-20 的容器里塞了 09-20 / 09-21 / 11-20 三条」的现场 ——
    /// 正是历史导入造成的形状 —— 然后跑一次拆分：应当只有 1 条（09-20 那条）
    /// 留在原处，另两条各自归位。再跑一次必须什么都不动（幂等），否则每次
    /// 启动都会把同一批节搬来搬去。
    #[test]
    fn split_moves_sections_to_their_own_days_and_is_idempotent() {
        crate::test_support::isolated(
            concat!(module_path!(), "::split_moves_sections_to_their_own_days_and_is_idempotent"),
            || {
                ensure_layout().expect("目录可建");
                let dir = general_root().join("l1/import");
                fs::create_dir_all(&dir).expect("目录可建");
                let source = dir.join("2026-09-20.md");

                // 三条不同日期的节全写进 09-20 的容器（模拟落错日期的存量）
                let mut text = container_head("2026-09-20", "import", None);
                for (id, stamp, body) in [
                    ("k0920", DAY_0920, "九月二十日那条"),
                    ("k0921", DAY_0921, "九月二十一日那条"),
                    ("k1120", DAY_1120, "十一月二十日那条"),
                ] {
                    let mut entry = MemoryEntry::draft(id, "标题", MemoryKind::Daily, body);
                    entry.created_at = stamp;
                    entry.updated_at = DAY_1120; // 都是「导入时刻」
                    text.push_str(&section_text(&entry));
                }
                write_atomic(&source, &text).expect("写入应当成功");

                let (moved, files, failed) = split_misplaced_containers();
                assert_eq!(failed, 0, "拆分不应有写失败");
                assert_eq!(moved, 2, "应当搬走 2 条（09-21 与 11-20）");
                assert_eq!(files, 1, "涉及 1 个容器");

                let read = |name: &str| fs::read_to_string(dir.join(name)).unwrap_or_default();
                let day20 = read("2026-09-20.md");
                assert!(day20.contains("九月二十日那条"), "留在原处的那条不该丢");
                assert!(!day20.contains("九月二十一日那条"), "落错日期的节应当已摘走");
                assert!(read("2026-09-21.md").contains("九月二十一日那条"));
                assert!(read("2026-11-20.md").contains("十一月二十日那条"));

                // 目标容器的头必须是**它自己那天**：写着 09-20 的文件摆在
                // 11-20.md 里，下一次排查会再被误导一遍
                assert!(
                    read("2026-11-20.md").contains("date: 2026-11-20"),
                    "搬过去的容器头日期没改：{}",
                    read("2026-11-20.md")
                );

                // 幂等：再跑一次不能动任何东西
                let (again_moved, again_files, again_failed) = split_misplaced_containers();
                assert_eq!(
                    (again_moved, again_files, again_failed),
                    (0, 0, 0),
                    "拆分必须幂等：第二次不应搬动任何节"
                );
                assert_eq!(read("2026-09-21.md").matches("九月二十一日那条").count(), 1);
            },
        );
    }

    /// 回归 6：目标写不进去时，**源里的节必须留下**。
    ///
    /// 这是本函数唯一会造成数据丢失的路径。早期写法是「先打算搬 → 回写源摘掉
    /// 那几节 → 再写目标」，写失败就变成「记忆从磁盘上消失」，而且因为源文件
    /// 里本来就错，用户连「少了一条」都发现不了。
    ///
    /// 用「目标位置被一个目录占住」制造写失败：`write_atomic` 会失败，而
    /// `fs::read_to_string` 读目录也会失败 → 目标侧看起来「不存在」。
    #[test]
    fn split_keeps_sections_when_the_target_cannot_be_written() {
        crate::test_support::isolated(
            concat!(module_path!(), "::split_keeps_sections_when_the_target_cannot_be_written"),
            || {
                ensure_layout().expect("目录可建");
                let dir = general_root().join("l1/import");
                fs::create_dir_all(&dir).expect("目录可建");
                let source = dir.join("2026-09-20.md");
                let mut text = container_head("2026-09-20", "import", None);
                let mut entry = MemoryEntry::draft("stay", "必须留下", MemoryKind::Daily, "不能丢的正文");
                entry.created_at = DAY_0921;
                entry.updated_at = DAY_1120;
                text.push_str(&section_text(&entry));
                write_atomic(&source, &text).expect("写入应当成功");

                // 用目录占住目标路径 → rename 到它必然失败
                fs::create_dir_all(dir.join("2026-09-21.md")).expect("占位目录可建");

                let (_, _, failed) = split_misplaced_containers();
                assert!(failed > 0, "目标不可写时应当报失败");

                let kept = fs::read_to_string(&source).expect("源容器应当还在");
                assert!(
                    kept.contains("不能丢的正文"),
                    "目标写失败却把源里的节摘掉了 —— 这条记忆就此从磁盘上消失"
                );
            },
        );
    }

    /// 回归 7：一次修复只落一次标记，第二次启动不再扫全量。
    ///
    /// `split_misplaced_once` 的语义是「这套数据已经修完了」。没有标记的话，
    /// 每次启动都要遍历并解析全部 l1 容器 —— 正常用户没坏数据，收益是 0。
    /// 但写失败时**不能**落标记：那说明还有节没归位。
    #[test]
    fn split_once_marks_done_and_does_not_rescan() {
        crate::test_support::isolated(
            concat!(module_path!(), "::split_once_marks_done_and_does_not_rescan"),
            || {
                ensure_layout().expect("目录可建");
                let first = split_misplaced_once();
                assert!(first.is_some(), "第一次应当执行（并落标记）");
                let marker = migrations_file();
                assert!(marker.is_file(), "标记文件应当落盘");
                let text = fs::read_to_string(&marker).expect("标记可读");
                assert!(text.contains(MIGRATION_SPLIT), "标记内容不对：{text}");

                let second = split_misplaced_once();
                assert!(second.is_none(), "标记已落，第二次不应再扫全量");

                // 标记文件坏了 / 被删了 → 应当重新跑一次（宁可多扫，不可漏修）
                let _ = fs::remove_file(&marker);
                assert!(split_misplaced_once().is_some(), "标记丢失后应当重跑");
            },
        );
    }

    /// 回归 8：有人正在攒批时，拆分必须退让，且**不能落完成标记**。
    ///
    /// 批量导入用延迟窗口在内存里攒「某个容器的最终文本」、批末整份写回。这时
    /// 把节搬走并清空老容器，批末那一写会把搬走之前的整份文本盖回去 —— 同一批
    /// 节同时存在于两个容器（老容器复活一份、新容器一份）。这个竞态不能靠
    /// `lock_writes` 解，因为窗口内根本不持锁。
    ///
    /// 「不落标记」这一半同样重要：这次根本没修，落了标记等于把「还没修」记成
    /// 「修过了」，用户重启一百次也不会再试。
    #[test]
    fn split_backs_off_while_deferred_window_is_open() {
        crate::test_support::isolated(
            concat!(module_path!(), "::split_backs_off_while_deferred_window_is_open"),
            || {
                ensure_layout().expect("目录可建");
                let dir = general_root().join("l1/import");
                fs::create_dir_all(&dir).expect("目录可建");
                let source = dir.join("2026-09-20.md");
                let mut text = container_head("2026-09-20", "import", None);
                let mut entry = MemoryEntry::draft("later", "晚点再搬", MemoryKind::Daily, "正文");
                entry.created_at = DAY_0921;
                entry.updated_at = DAY_1120;
                text.push_str(&section_text(&entry));
                write_atomic(&source, &text).expect("写入应当成功");

                // 模拟「导入正在攒批」
                begin_deferred();
                assert!(deferred_active(), "前置条件：窗口应当开着");
                assert!(
                    split_misplaced_once().is_none(),
                    "有人在攒批时不该跑迁移，也不该返回「已执行」"
                );
                assert!(!migrations_file().is_file(), "没修就不能落完成标记");
                let untouched = fs::read_to_string(&source).expect("源容器应当还在");
                assert!(untouched.contains("晚点再搬"), "攒批期间不该动容器");
                assert!(!dir.join("2026-09-21.md").is_file(), "攒批期间不该写目标");
                end_deferred();

                // 窗口关了 → 正常搬
                let (moved, _, failed) = split_misplaced_containers();
                assert_eq!((moved, failed), (1, 0), "窗口关闭后应当正常归位");
                assert!(dir.join("2026-09-21.md").is_file());
            },
        );
    }
}
