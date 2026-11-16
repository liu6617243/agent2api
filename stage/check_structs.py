"""静态自检：核对记忆模块内部的结构体字面量有没有漏字段（E0063）。

只处理「全仓唯一名字」的结构体，避免 `Request` / `Query` 这类跨模块重名导致的
误报；重名的结构体人工核对。
"""

from __future__ import annotations

import re
import sys
from collections import defaultdict
from pathlib import Path

ROOT = Path(r"D:/Personal/Desktop/agent2api-main/desktop-tauri/src-tauri/server/src")
SERVER = ROOT / "server"
MEM = SERVER / "core" / "memory"

owners: dict[str, set[Path]] = defaultdict(set)
fields: dict[str, set[str]] = defaultdict(set)

STRUCT_RE = re.compile(r"pub\s+struct\s+([A-Za-z0-9_]+)\s*\{")
for path in SERVER.rglob("*.rs"):
    text = path.read_text(encoding="utf-8", errors="ignore")
    for m in STRUCT_RE.finditer(text):
        name = m.group(1)
        owners[name].add(path)
        start = text.index("{", m.end() - 1)
        depth = 0
        i = start
        while i < len(text):
            if text[i] == "{":
                depth += 1
            elif text[i] == "}":
                depth -= 1
                if depth == 0:
                    break
            i += 1
        body = text[start : i + 1]
        for fm in re.finditer(r"^\s*(?:pub(?:\([^)]*\))?\s+)?([A-Za-z0-9_]+)\s*:\s*[^,]+,", body, re.M):
            fields[name].add(fm.group(1))

unique = {name for name, files in owners.items() if len(files) == 1}

SCAN = [
    *sorted(MEM.glob("*.rs")),
    SERVER / "api" / "memory.rs",
    SERVER / "core" / "tool_services" / "memory_mcp.rs",
]

LIT_RE = re.compile(r"\b([A-Z][A-Za-z0-9_]*)\s*\{")
# 字段名一律小写下划线风格；类型名是驼峰，取小写标识符作为「疑似字段」候选
FIELD_RE = re.compile(r"\b([a-z_][a-z0-9_]*)\b")

problems: list[str] = []
for path in SCAN:
    text = path.read_text(encoding="utf-8", errors="ignore")
    for m in LIT_RE.finditer(text):
        name = m.group(1)
        if name not in unique or name not in fields:
            continue
        # `-> Foo {` 是函数返回类型，`impl Foo {` / `impl Trait for Foo {` 是实现块，
        # 都不是结构体字面量
        if re.search(r"(->|impl)\s*$", text[: m.start()]):
            continue
        if re.search(r"\bimpl\s+[A-Za-z0-9_:<>' ]+\s+for\s+$", text[: m.start()]):
            continue
        if re.search(r"&'?static\s+$", text[: m.start()]):
            continue
        # `Kind::Profile {` / `x == Foo {` 也不是字面量
        if re.search(r"(::|==|!=|\.|&&|\|\|)\s*$", text[: m.start()]):
            continue
        depth = 0
        i = m.end() - 1
        while i < len(text):
            if text[i] == "{":
                depth += 1
            elif text[i] == "}":
                depth -= 1
                if depth == 0:
                    break
            i += 1
        body = text[m.end() : i]
        if ".." in body:
            continue
        given = set(FIELD_RE.findall(body))
        missing = fields[name] - given
        if missing:
            line = text[: m.start()].count("\n") + 1
            problems.append(f"  {path.name}:{line}  {name}  缺: {sorted(missing)}")

if problems:
    print("--- STRUCT LITERAL PROBLEMS ---")
    for line in problems:
        print(line)
    sys.exit(1)
print("OK: 记忆模块结构体字面量字段完整")
