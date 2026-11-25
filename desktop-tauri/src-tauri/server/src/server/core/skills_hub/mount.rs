//! 技能仓库 · 挂载层（链接的建立 / 校验 / 摘除）。
//!
//! ── 挂载是什么 ──────────────────────────────────────────────
//! 在客户端技能目录里放一个**链接**，指向中央真身 `<hub>/skills/<name>`。
//! Windows 用 junction（目录联接，不需要管理员权限、能被资源管理器当目录用），
//! 其它平台用目录符号链接。客户端遍历技能目录时看到的是目录，读到的内容是
//! 中央那一份 —— 于是「改一处等于改全部」，而磁盘上只有一份内容。
//!
//! ── 三条底线，任何一条破都会伤到用户文件 ────────────────────
//! 1. **摘除只删链接本身**：绝不对链接目标递归删除。`remove_dir_all` 在链接上
//!    的行为各平台不一致，所以摘除只用 `remove_file`（符号链接）或
//!    `remove_dir`（junction —— Windows 上 junction 必须用这个）。
//! 2. **目标位置已有真实目录就报冲突**，绝不静默替换。那个目录里可能是用户
//!    改了一半的技能，替换掉等于把他的编辑扔了。
//! 3. **copy 模式只允许在内容一致时删除本地副本**。内容不同时删就是丢数据。
//!
//! ── 为什么校验要单独一个函数 ───────────────────────────────
//! 「台账说已挂载」与「磁盘上真的能进去」是两件事：真身被删、链接被系统清掉、
//! 用户手动改成了别的链接，都会让台账变得乐观。仪表盘的「失效挂载」和「一键
//! 修复」都读校验结果，不读台账。

use std::fs;
use std::path::{Path, PathBuf};

use serde_json::{json, Value};

use super::scan;
use super::store;

/// 当前平台使用的链接类型名（落台账用）。
pub fn link_type() -> &'static str {
    if cfg!(windows) {
        "junction"
    } else {
        "symlink"
    }
}

/// 这个路径是不是链接（符号链接 / junction 都算）。
///
/// `symlink_metadata` 对 junction 报的是 `is_symlink() == false` 但
/// `file_type().is_dir()` 也可能为假 —— 所以两者都要判，再用
/// `read_link` 兜一次（能读出目标就是链接）。
pub fn is_link(path: &Path) -> bool {
    let meta = match fs::symlink_metadata(path) {
        Ok(value) => value,
        Err(_) => return false,
    };
    if meta.file_type().is_symlink() {
        return true;
    }
    // Windows junction：FILE_ATTRIBUTE_REPARSE_POINT 在 std 里表现为
    // symlink_metadata + read_link 成功。读出目标即认定是链接。
    fs::read_link(path).is_ok()
}

/// 链接指向的目标（归一化后）。不是链接 / 读不出返回空串。
pub fn link_target(path: &Path) -> String {
    match fs::read_link(path) {
        Ok(target) => scan::normalize_link_target(&target),
        Err(_) => String::new(),
    }
}

/// 链接是否指向 `target`（忽略大小写与尾部斜杠差异）。
pub fn points_to(path: &Path, target: &Path) -> bool {
    let got = link_target(path);
    if got.is_empty() {
        return false;
    }
    scan::path_key(Path::new(&got)) == scan::path_key(target)
}

/// 建立挂载。返回 `{ action, linkPath, message? }`。
///
/// `action` 取值：`mounted` / `already` / `copied` / `conflict-real-dir` /
/// `conflict-diff-link` / `error`。调用方按它决定计数与提示。
pub fn mount(skill: &str, tool_dir: &Path, mode: &str, mount_name: &str) -> Value {
    let target = store::skills_dir().join(skill);
    let link_path = tool_dir.join(mount_name);
    if !target.is_dir() {
        return json!({
            "action": "error",
            "linkPath": link_path.to_string_lossy(),
            "message": format!("中央真身不存在：{}", target.display()),
        });
    }
    if !tool_dir.is_dir() {
        return json!({
            "action": "error",
            "linkPath": link_path.to_string_lossy(),
            "message": format!("工具目录不存在：{}", tool_dir.display()),
        });
    }

    if is_link(&link_path) {
        if points_to(&link_path, &target) && link_path.exists() {
            return json!({ "action": "already", "linkPath": link_path.to_string_lossy() });
        }
        return json!({
            "action": "conflict-diff-link",
            "linkPath": link_path.to_string_lossy(),
            "message": format!(
                "已有链接指向 {}，不是中央真身",
                link_target(&link_path)
            ),
        });
    }

    if link_path.exists() {
        if mode == "copy" {
            let local_hash = scan::tree_hash(&link_path);
            let central_hash = scan::tree_hash(&target);
            if !local_hash.is_empty() && local_hash == central_hash {
                // 内容一致：重建一份副本没有意义，直接算已挂载
                return json!({ "action": "already", "linkPath": link_path.to_string_lossy() });
            }
        }
        return json!({
            "action": "conflict-real-dir",
            "linkPath": link_path.to_string_lossy(),
            "message": "目标位置已有同名真实目录",
        });
    }

    if mode == "copy" {
        return match store::copy_dir_all(&target, &link_path) {
            Ok(()) => json!({
                "action": "copied",
                "type": "copy",
                "linkPath": link_path.to_string_lossy(),
            }),
            Err(message) => json!({
                "action": "error",
                "linkPath": link_path.to_string_lossy(),
                "message": message,
            }),
        };
    }

    match make_link(&target, &link_path) {
        Ok(()) => json!({
            "action": "mounted",
            "type": link_type(),
            "linkPath": link_path.to_string_lossy(),
        }),
        Err(message) => {
            // ── 链接建不了就退回复制 ────────────────────────────────
            // Windows 上建立目录符号链接需要「开发者模式」或管理员权限，
            // `std::os::windows::fs::symlink_dir` 走的是符号链接而不是 junction，
            // 权限不足时报 os error 1314。这时**不能直接失败**：用户的目录
            // 已经扫描过了，他只是想让技能可用。退回复制一份，并在结果里
            // 说清「为什么是副本」—— 否则下次同步看到真实目录又要冲突一轮。
            match store::copy_dir_all(&target, &link_path) {
                Ok(()) => json!({
                    "action": "copied",
                    "type": "copy",
                    "linkPath": link_path.to_string_lossy(),
                    "message": format!("{message}；已改为复制一份（副本模式）"),
                }),
                Err(copy_error) => json!({
                    "action": "error",
                    "linkPath": link_path.to_string_lossy(),
                    "message": format!("{message}；复制兜底也失败：{copy_error}"),
                }),
            }
        }
    }
}

/// 建立目录链接。
///
/// ── Windows 为什么不用 `symlink_dir` ────────────────────────
/// `std::os::windows::fs::symlink_dir` 建的是**符号链接**，需要「开发者模式」
/// 或管理员权限；未开这两样的机器上会报 os error 1314，于是每一个挂载都退回
/// 「复制一份」—— 那正好丢掉了本模块存在的理由（改一处等于改全部）。而
/// **junction 不需要任何特权**，所以改走 `mklink /J`：两者都是目录型重解析点，
/// 客户端看到的是目录、读到的内容是中央那一份，语义一致。
///
/// `mklink` 是 cmd 内建命令，路径会被 cmd 二次解析 —— 含 `& | < > ^ "` 时退回
/// 符号链接，绝不把用户路径拼进命令行。目标是 FAT/exFAT 卷（不支持重解析点）
/// 时 `mklink` 也会失败，同样退回符号链接，最后交给调用方的复制兜底。
#[cfg(unix)]
fn make_link(target: &Path, link_path: &Path) -> Result<(), String> {
    std::os::unix::fs::symlink(target, link_path)
        .map_err(|error| format!("建立符号链接失败 {}：{error}", link_path.display()))
}

#[cfg(windows)]
fn make_link(target: &Path, link_path: &Path) -> Result<(), String> {
    let link = link_path.to_string_lossy().to_string();
    let dest = target.to_string_lossy().to_string();
    // 用户名与客户端目录名不含 cmd 元字符，但「自定义目录」是用户手输的。
    let has_meta = |text: &str| {
        text.chars()
            .any(|c| matches!(c, '&' | '|' | '<' | '>' | '^' | '"' | '\n' | '\r'))
    };
    let fallback = |why: &str| {
        std::os::windows::fs::symlink_dir(target, link_path).map_err(|error| {
            format!("建立目录链接失败：{error}（{why}）")
        })
    };
    if has_meta(&link) || has_meta(&dest) {
        return fallback("路径含命令行元字符，未走 junction");
    }

    let output = match std::process::Command::new("cmd")
        .args(["/C", "mklink", "/J", &link, &dest])
        .output()
    {
        Ok(value) => value,
        Err(error) => return fallback(&format!("调用 mklink 失败：{error}")),
    };
    // mklink 成功时退出码为 0；再验一次链接真的建出来了（mklink 有「已存在」
    // 等情形会返回 0 却什么都没做）。
    if output.status.success() && is_link(link_path) {
        return Ok(());
    }
    let detail = String::from_utf8_lossy(&output.stderr)
        .replace('\r', " ")
        .replace('\n', " ");
    let detail = detail.trim();
    fallback(if detail.is_empty() { "mklink 未成功" } else { &detail })
}

/// 摘除挂载：**只删链接本身**，绝不触碰目标。
pub fn unmount(link_path: &Path) -> Result<(), String> {
    if !is_link(link_path) {
        return Err("这个位置不是链接，拒绝摘除（真实目录请手动处理）".to_string());
    }
    // Windows junction 是「目录型重解析点」，`remove_file` 会失败（它不是文件），
    // `remove_dir` 才是正确动作；符号链接两种都可能成，先试 dir 再试 file。
    let as_dir = fs::remove_dir(link_path);
    if as_dir.is_ok() {
        return Ok(());
    }
    match fs::remove_file(link_path) {
        Ok(()) => Ok(()),
        Err(_) => {
            let first = as_dir.err().map(|error| error.to_string()).unwrap_or_default();
            Err(format!("摘除链接失败 {}：{first}", link_path.display()))
        }
    }
}

/// 校验一条挂载记录：链接还在、指向中央真身、目标还能进去。
pub fn verify(mount_record: &Value) -> Value {
    let path = PathBuf::from(mount_record.get("path").and_then(Value::as_str).unwrap_or(""));
    let skill = mount_record.get("name").and_then(Value::as_str).unwrap_or("");
    let central = store::skills_dir().join(skill);
    let link_exists = is_link(&path);
    let target = link_target(&path);
    let points_ok = !target.is_empty() && scan::path_key(Path::new(&target)) == scan::path_key(&central);
    let mut row = mount_record.clone();
    row["valid"] = json!(link_exists && points_ok && central.is_dir());
    row["linkExists"] = json!(link_exists);
    row["pointsCentral"] = json!(points_ok);
    row["centralExists"] = json!(central.is_dir());
    row
}

/// 台账里所有挂载的实检结果（仪表盘 / 技能库的「失效挂载」都读它）。
pub fn verify_all(manifest: &Value) -> Vec<Value> {
    let mut rows: Vec<Value> = Vec::new();
    let Some(skills) = manifest.get("skills").and_then(Value::as_object) else {
        return rows;
    };
    for (name, entry) in skills {
        let mounts = match entry.get("mounts").and_then(Value::as_array) {
            Some(list) => list,
            None => continue,
        };
        for item in mounts {
            if !item.get("enabled").and_then(Value::as_bool).unwrap_or(true) {
                continue;
            }
            let mut row = verify(item);
            row["skill"] = json!(name);
            rows.push(row);
        }
    }
    rows
}

/// 一键修复：重建所有失效挂载，返回修好的条数。
///
/// 只处理「台账里有、链接丢了或指错」的情况。目标位置已有真实目录的不动 ——
/// 那是冲突，得人来裁决。
pub fn repair(manifest: &Value, mode: &str) -> Value {
    let mut repaired = 0usize;
    let mut failed: Vec<Value> = Vec::new();
    let Some(skills) = manifest.get("skills").and_then(Value::as_object) else {
        return json!({ "repaired": 0, "failed": failed });
    };
    for (name, entry) in skills {
        let central = store::skills_dir().join(name);
        if !central.is_dir() {
            continue;
        }
        let Some(mounts) = entry.get("mounts").and_then(Value::as_array) else {
            continue;
        };
        for item in mounts {
            if !item.get("enabled").and_then(Value::as_bool).unwrap_or(true) {
                continue;
            }
            let path = PathBuf::from(item.get("path").and_then(Value::as_str).unwrap_or(""));
            if path.as_os_str().is_empty() {
                continue;
            }
            if verify(item).get("valid").and_then(Value::as_bool) == Some(true) {
                continue;
            }
            let tool_dir = match path.parent() {
                Some(value) => value.to_path_buf(),
                None => continue,
            };
            if !tool_dir.is_dir() {
                failed.push(json!({
                    "skill": name,
                    "path": path.to_string_lossy(),
                    "message": "工具目录已不存在",
                }));
                continue;
            }
            // 指错的链接先摘掉（它不指向中央，留着也建不了新的）
            if is_link(&path) {
                let _ = unmount(&path);
            } else if path.exists() {
                failed.push(json!({
                    "skill": name,
                    "path": path.to_string_lossy(),
                    "message": "目标位置有真实目录，需人工裁决",
                }));
                continue;
            }
            let mount_name = path
                .file_name()
                .map(|value| value.to_string_lossy().to_string())
                .unwrap_or_else(|| name.clone());
            let result = mount(name, &tool_dir, mode, &mount_name);
            let action = result.get("action").and_then(Value::as_str).unwrap_or("");
            if action == "mounted" || action == "already" || action == "copied" {
                repaired += 1;
            } else {
                failed.push(json!({
                    "skill": name,
                    "path": path.to_string_lossy(),
                    "message": result.get("message").and_then(Value::as_str).unwrap_or("修复失败"),
                }));
            }
        }
    }
    json!({ "repaired": repaired, "failed": failed })
}