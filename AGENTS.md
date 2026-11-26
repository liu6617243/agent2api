# AGENTS.md

> **涉及分支、发版、版本号的操作，一律按 Skill `release-flow` 执行：**
> **`.agents/skills/release-flow/SKILL.md`** —— 动手前先读它，细则一律以该文件为准，本文件只作入口指路。
>
> 覆盖范围：切分支 / 给 PR 选 base / 合并 PR / 回复 Issue、PR / 改版本号 / 写更新日志 /
> 打 tag / 挂 GitHub Release / 发 beta 预发布版 / 发版验收 / 排查发版失败。
>
> **涉及推代码后的 CI / 出包 / 交付自检，按 Skill `delivery-pipeline` 执行：**
> **`.agents/skills/delivery-pipeline/SKILL.md`** —— 交付三件套、闸门、状态查询、排错都在那里。
> 覆盖范围：推完代码 CI 跑没跑 / 安装包出没出 / artifact id 怎么拿 / workflow 触发面对不对。

## 两条红线（先问人，不得自行推进）

1. **发版**：改版本号、推 `main` / `beta`、打 tag、挂 Release 之前，先把版本号与「这次要发什么」讲清楚，等用户确认；**未经确认不得推任何分支或 tag** —— tag 一旦推出去就会触发构建、产出公开的安装包与镜像，收不回来。
2. **更新日志**：写好的日志（= 发布提交信息 = Release 正文）必须先给用户逐条过目；按反馈改完**再确认一次**才能提交。
3. **版本号固定跟随基线（当前 `3.5.0-beta.1`），不得自行 bump。** 这个 fork 跟上游用同一个版本号，用户手动装包；「不 bump 更新器就认不出新版」是上游发布链路的语义，不是这里的目标。除非用户明确说「发一个新版本」，否则 `package.json`（根与 `desktop-tauri`）、`tauri.conf.json`、`Cargo.toml`（`src-tauri` 与 `server`）、`Cargo.lock` 里的版本号一个字都不许动。2026-11-21 已因擅自 bump 到 beta.2 / beta.3 被明确制止。

其余细则（分支落脚点、署名与 AI 落款、六处版本号、正式版 / beta 完整流程、验收清单、已知坑与排查）见上述 Skill。

## 提交与交付（硬性操作纪律）

这一节写在这里而不是 `memory/` 里，是因为 **`AGENTS.md` 每轮都会进上下文，`memory/` 不会** —— 放在
`memory/` 的规则等于没写（已因此重复踩坑三次）。凡「必须每次都生效」的规矩一律写进本文件。

1. **「提交」= 本地 commit + 推送 + **出 Windows 安装包**，一步做完。**
   不要在 commit 后反问「要推吗 / 推哪个分支 / 要出包吗」，
   也不要在只推完代码就收工 —— **推完没有包 = 没交付完**。
   入口只有一条：`node scripts/deliver.mjs`（推代码 → 跑测试 → 等绿 → 出安装包）。
   仍然受上面两条红线约束（版本号 / tag / Release 前必须确认）。
2. **推送后不下载构建产物。** CI 绿了就是交付完成：**不要** `curl` exe / zip / 安装包到 `.workbuddy/dist/`
   或任何临时目录，也不要为了「校验」「验证一下」去下载。报「CI 绿 + run / artifact id」即可。
   只有用户**明确要产物文件**时才下。
3. **验证靠代码与数据库，不靠下载产物。** 要确认改动生效，读源码 / 读 `~/.agent2api/agent2api.db`
   里的实际数据，不要去拉构建出来的二进制。
4. **编译与构建的唯一权威是 GitHub Actions，本机不装 Rust 工具链。** 2026-11-21 起本机的
   Rust 工具链（`D:/rust-toolchain`、`~/.cargo`）与 cargo 缓存已全部卸载 —— 装它要下 700MB+、
   在沙箱里连 crate 都解不开（os error 5），装完照样跑不通 check，纯属浪费。
   **不要**再在本机装 cargo/rustc 来"提前验证编译"；借用检查（E0507 / E0382 / 生命周期）
   由 CI 的 `rust-tests` 兜底。`node scripts/deliver.mjs` 默认**不跑**本机编译闸；
   确实需要时显式加 `--local-rust-check`（前提是本机已有工具链）。

### 交付一条命令：`node scripts/deliver.mjs`

推 `main-patched` **不要裸 `git push`**（`.githooks/pre-push` 会拦下来并报错）。交付入口只有一条：

```bash
node scripts/deliver.mjs          # 推代码 + 跑测试 + 等绿 + 出安装包（= npm run deliver）
node scripts/deliver.mjs --no-push    # 已推过，只补跑链路
node scripts/deliver.mjs --no-build   # 只跑测试与产物同源，不出包
```

它做的整条链：

```
推 HEAD:main-patched（git push 走不通时自动改走 GitHub API 直推）
   ├─ 派发 rust-tests          ┐ 并行
   └─ 派发 frontend-artifacts  ┘
          ↓ 两个都绿才继续（任一失败 → 不派发出包，退出码 1）
     派发 build-windows → 打印安装包 artifact 文件名 / 大小 / id
```

- **两条推送通道，闸门都挂着**：`git push` 走 `.githooks/pre-push` 钩子；
  GitHub API 直推走 `scripts/push-local-commit.mjs` 自带的同名闸门。
  放行词表统一：`A2A_DELIVER=1`（交付链路自己调）/ `A2A_SKIP_BUILD=1`（人明确说这次不出包）。
  **不要绕过这两条通道自己写推送脚本** —— 2026-11-26 就是靠一条躺在
  `.workbuddy/tmp/`（被 gitignore）里的脚本推的代码，绕过了全部闸门，
  结果 CI 没跑、安装包没出。凡「推完代码忘了出包」，先查这条推送路径有没有闸门。
- **为什么用派发而不是 push 触发**：三个工作流的 `push.branches` 写的是上游分支名
  `[main, beta]` / `[v3.3.0-beta.3]`，在本 fork（干活分支 `main-patched`）**从来不会因 push 自动跑**。
  治本是补触发面，但本机凭据是 classic PAT、scope 只有 `public_repo`，写 `.github/workflows/**`
  被 GitHub 拒（`without workflow scope`）。`public_repo` 够调 `workflow_dispatch`，所以走派发这条路 ——
  零新权限、当场可用。
- **触发面补丁已备好**：`.github/workflows/{rust-tests,frontend-artifacts,build-windows}.yml` 的改动在
  本地，等凭据补上 `workflow` scope 后跑 `node scripts/push-workflows.mjs --all` 写上去；
  那之后连派发这一步都不需要，本脚本仍可当闸门用。
- **闸门**：`build-windows` 的 `gate` job 跑 `scripts/ci-gate.mjs`，等前两道闸门对**同一个提交**都变绿
  才放行；任一失败则本条直接失败、**不产出 artifact**（该 job 随触发面补丁一起上线，派发路径的闸门由
  `deliver.mjs` 自己等）。
- **只看状态**：`node scripts/ci-watch.mjs`（`--wait` / `--artifact` / `--gate`）。

⚠️ **不要再出现「代码改完了但没出安装包」**。2026-11-20 之前，本 fork 的工作流 push 触发面写的是
`[main, beta]`（上游分支名），而实际干活的分支是 `main-patched` —— 于是三个流水线**从来不会因为 push 自动跑**，
历史上每个 run 都是手动 `workflow_dispatch`，每次都要用户提醒才想起出包。现在这条路已经堵死：
裸推会被钩子拦下，唯一入口会顺手把包出完。

## 排障纪律：先看真机，再读代码（2026-11-21 因浪费用户 5 小时立此条）

**用户报「页面打不开 / 按钮没反应 / 界面不对」时，第一动作是看真机，不是读代码。**

本机软件就在跑，进程和窗口都能直接拿到：

```bash
# 1) 找到正在运行的软件主窗口（不要猜进程名）
powershell -NoProfile -Command "Get-Process | Where-Object { \$_.MainWindowHandle -ne 0 } | Select-Object Id, ProcessName, MainWindowTitle"
# 2) 截这个窗口的真实画面（Python + PIL，本机已验证可用）
python - <<'PY'
import ctypes, ctypes.wintypes as wt, time
from PIL import ImageGrab
u = ctypes.windll.user32
CB = ctypes.WINFUNCTYPE(ctypes.c_bool, wt.HWND, wt.LPARAM)
found = []
def cb(h, _):
    n = u.GetWindowTextLengthW(h)
    if n:
        b = ctypes.create_unicode_buffer(n + 1); u.GetWindowTextW(h, b, n + 1)
        if "Agent2API" in b.value: found.append(h)
    return True
u.EnumWindows(CB(cb), 0)
h = found[0]; u.ShowWindow(h, 9); u.SetForegroundWindow(h); time.sleep(0.8)
r = wt.RECT(); u.GetWindowRect(h, ctypes.byref(r))
ImageGrab.grab(bbox=(r.left, r.top, r.right, r.bottom), all_screens=True).save(r".workbuddy/tmp/live.png")
PY
```

**禁止的做法（已犯过的错）**：自己 `python -m http.server` 起静态页、用浏览器直连
`127.0.0.1:3065` 测接口 —— 那既不是用户看到的界面（没有面板会话、没有桌面壳的 60 秒超时），
还会因为 401/404 把排查方向带偏。**用户说的「打不开」发生在桌面壳里，就必须在桌面壳里复现。**

**顺序固定为**：真机截图 / 真机日志 → 定位到具体文案或请求 → 再回代码找那一条字符串的出处。
反过来的顺序（先读代码猜、猜完编个场景验证）已经浪费过用户一整个上午，代价是 5 小时。

## 反代提供商问题排查

收到反代提供商相关的问题反馈时，**优先使用网络搜索查找已有解决方案**，重点参考同类项目的 Issue、PR、修复记录和适配实现，了解其他项目是否遇到过相同问题、如何解决。**不要默认从零自行排查**：先核对已有方案与本项目的适用性，优先借鉴可靠的解决思路；未找到可靠方案、现有方案不适用或仍有未解决的问题时，再开展针对性的本地排查。

## 文档落点

安装 / 部署 / 升级 / 排错说明只在 `docs/install.md`（另有 5 份译文 `docs/install.{en,zh-Hant,ja,ko,pt-BR}.md`，六份一一对应）里维护；各语言 README 只留「快速开始」速查表 + 一个指向安装文档的链接，**不要再把安装细节写回 README**。产物名、端口、环境变量、安装脚本参数有改动时，六份一起改。

**README 语言落点：根 `README.md` 是英文**（GitHub 默认展示），简体中文在 `docs/README.zh-CN.md`，其余四份为 `docs/README.{zh-Hant,ja,ko,pt-BR}.md`。中文仍是文案源语言，改支持矩阵等内容时六份同步，别把中文写回根目录。
