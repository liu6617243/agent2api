"""静态自检：核对记忆模块内部的跨文件调用（`store::` / `index::` / `config::` …）是否存在。

本机无 Rust 工具链，cargo check 不可用；这类拼错在编译期是 E0425，
CI 一轮要 6~10 分钟，先用脚本扫掉。
"""

from __future__ import annotations

import re
import sys
from pathlib import Path

MEM = Path(r"D:/Personal/Desktop/agent2api-main/desktop-tauri/src-tauri/server/src/server/core/memory")
SERVER = MEM.parent.parent

FILES = {
    "config": MEM / "config.rs",
    "enrich": MEM / "enrich.rs",
    "index": MEM / "index.rs",
    "layout": MEM / "layout.rs",
    "llm": MEM / "llm.rs",
    "model": MEM / "model.rs",
    "schedule": MEM / "schedule.rs",
    "search": MEM / "search.rs",
    "store": MEM / "store.rs",
    "memory": MEM / "mod.rs",
}

SCAN = list(FILES.values()) + [
    SERVER / "api" / "memory.rs",
    SERVER / "core" / "tool_services" / "memory_mcp.rs",
]

ITEM_RE = re.compile(
    r"^\s*pub(?:\([^)]*\))?\s+(?:async\s+)?(?:fn|struct|enum|trait|const|static|type|mod)\s+([A-Za-z0-9_]+)",
    re.M,
)
# 模块内可见的项（含非 pub 的 fn，私有项同文件可见）
ANY_ITEM_RE = re.compile(
    r"^\s*(?:pub(?:\([^)]*\))?\s+)?(?:async\s+)?(?:fn|struct|enum|trait|const|static|type)\s+([A-Za-z0-9_]+)",
    re.M,
)

items: dict[str, set[str]] = {}
for name, path in FILES.items():
    text = path.read_text(encoding="utf-8")
    found = set(ITEM_RE.findall(text))
    found |= set(ANY_ITEM_RE.findall(text))
    items[name] = found

CALL_RE = re.compile(r"\b(" + "|".join(FILES) + r")::([A-Za-z0-9_]+)")

missing: dict[str, set[str]] = {}
for path in SCAN:
    text = path.read_text(encoding="utf-8")
    for mod, item in CALL_RE.findall(text):
        if item not in items[mod]:
            missing.setdefault(f"{mod}::{item}", set()).add(path.name)

if missing:
    print("--- MISSING ITEMS ---")
    for raw, files in sorted(missing.items()):
        print(f"  {raw:40s} <- {sorted(files)}")
    sys.exit(1)
print("OK: 记忆模块内部跨文件调用全部有定义")
