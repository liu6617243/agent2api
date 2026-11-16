"""静态自检：核对记忆模块里所有 `crate::server::...` 路径在仓库里真实存在。

为什么需要：本机没有 Rust 工具链（cargo/rustc/MSVC/Docker/WSL 全不可用），
cargo check 跑不了；路径写错在编译期就是 E0433/E0425，CI 一轮要 6~10 分钟。
先把这层用脚本扫掉，把 CI 留给真正需要编译器的语义问题。
"""

from __future__ import annotations

import re
import sys
from pathlib import Path

ROOT = Path(r"D:/Personal/Desktop/agent2api-main/desktop-tauri/src-tauri/server/src")
SERVER = ROOT / "server"

SCAN = [
    *sorted((SERVER / "core" / "memory").glob("*.rs")),
    SERVER / "api" / "memory.rs",
    SERVER / "core" / "tool_services" / "memory_mcp.rs",
]

PATH_RE = re.compile(r"crate::server::([A-Za-z0-9_:]+)")

# 收集仓库里所有可解析的模块/函数路径
available: set[str] = set()
for path in SERVER.rglob("*.rs"):
    rel = path.relative_to(ROOT).with_suffix("")
    parts = list(rel.parts)
    if parts[-1] == "mod":
        parts = parts[:-1]
    elif parts[-1] == "lib":
        parts = parts[:-1]
    if parts and parts[-1] == "server":
        # server/mod.rs 的内容属于 `server` 本身，模块路径要保留这一层
        pass
    stem = parts[-1] if parts else ""
    parent = parts[:-1]
    if stem:
        available.add("::".join(parent + [stem]))
        available.add(stem)
    # 模块内的 pub 项
    try:
        text = path.read_text(encoding="utf-8")
    except UnicodeDecodeError:
        continue
    for m in re.finditer(r"^\s*pub(?:\([^)]*\))?\s+(?:async\s+)?(?:fn|struct|enum|trait|const|static|type|mod|use)\s+([A-Za-z0-9_]+)", text, re.M):
        name = m.group(1)
        if parts:
            available.add("::".join(parts + [name]))
    for m in re.finditer(r"^\s*pub\s+mod\s+([A-Za-z0-9_]+)\s*;", text, re.M):
        if parts:
            available.add("::".join(parts + [m.group(1)]))

missing: dict[str, set[str]] = {}
for path in SCAN:
    text = path.read_text(encoding="utf-8")
    for raw in PATH_RE.findall(text):
        parts = ("server::" + raw).split("::")
        # 逐级最长匹配：找到最长可解析前缀，剩下的是项名
        ok = False
        for i in range(len(parts), 0, -1):
            prefix = "::".join(parts[:i])
            if prefix in available:
                ok = True
                break
        if not ok:
            missing.setdefault(raw, set()).add(path.name)

if missing:
    print("--- UNRESOLVED crate::server:: PATHS ---")
    for raw, files in sorted(missing.items()):
        print(f"  crate::server::{raw}   <- {sorted(files)}")
    sys.exit(1)
print(f"OK: {len(SCAN)} 个文件里的 crate::server:: 路径全部可解析")
