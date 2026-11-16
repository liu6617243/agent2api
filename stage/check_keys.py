"""静态自检：核对记忆模块引用的配置键是否都在 config.rs 的 schema 里定义。

为什么需要它：本机没有 Rust 工具链，cargo check 跑不了；配置键写错在编译期
是 `Option` 解引用，运行期才炸成「设置不生效」。这类错误 CI 也测不出来。
"""

from __future__ import annotations

import re
import sys
from pathlib import Path

ROOT = Path(r"D:/Personal/Desktop/agent2api-main/desktop-tauri/src-tauri/server/src/server")
MEM = ROOT / "core" / "memory"
CONFIG = MEM / "config.rs"

SCAN = [
    *sorted(MEM.glob("*.rs")),
    ROOT / "api" / "memory.rs",
    ROOT / "core" / "tool_services" / "memory_mcp.rs",
]

# 定义点：knob_d("key", ...) / knob("key", ...)
DEF_RE = re.compile(r'knob(?:_d)?\(\s*"([a-zA-Z0-9._]+)"')
# 使用点：config::get("key") / config::number("key") / ...
USE_RE = re.compile(
    r'config::(?:get|number|usize_of|bool_of|text_of|strings_of|object_of|default_of|find)\(\s*"([a-zA-Z0-9._]+)"'
)
# 常量式使用：config::ENABLE_KEY 之类
CONST_RE = re.compile(r'config::([A-Z][A-Z0-9_]+)')

defined = set(DEF_RE.findall(CONFIG.read_text(encoding="utf-8")))
used: dict[str, list[str]] = {}
consts: dict[str, list[str]] = {}

for path in SCAN:
    text = path.read_text(encoding="utf-8")
    for key in USE_RE.findall(text):
        used.setdefault(key, []).append(path.name)
    for name in CONST_RE.findall(text):
        consts.setdefault(name, []).append(path.name)

print(f"defined={len(defined)} used={len(used)}")
missing = sorted(k for k in used if k not in defined)
if missing:
    print("--- USED BUT NOT DEFINED ---")
    for key in missing:
        print(f"  {key:38s} <- {sorted(set(used[key]))}")
else:
    print("OK: 所有被引用的配置键都在 schema 中")

# 常量是否在 config.rs 中存在
missing_const = sorted(c for c in consts if f"pub const {c}" not in CONFIG.read_text(encoding="utf-8"))
if missing_const:
    print("--- MISSING CONST ---")
    for name in missing_const:
        print(f"  {name} <- {sorted(set(consts[name]))}")
else:
    print("OK: 常量引用都存在")

# 反向：schema 里定义了但从没用到的键（提示冗余）
unused = sorted(k for k in defined if k not in used)
if unused:
    print("--- DEFINED BUT NEVER READ ---")
    for key in unused:
        print(f"  {key}")

sys.exit(1 if (missing or missing_const) else 0)
