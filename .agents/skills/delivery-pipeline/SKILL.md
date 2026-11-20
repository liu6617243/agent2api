---
name: delivery-pipeline
description: Agent2API（workbuddy）本 fork 的**交付流水线**规范 —— push 触发面、交付三件套（rust-tests / frontend-artifacts / build-windows）、交付闸门 scripts/ci-gate.mjs、状态查询 scripts/ci-watch.mjs、出包纪律与排错。凡是涉及推代码后 CI 跑没跑、安装包出没出、交付前自检、CI 红了怎么查、artifact id 怎么拿、workflow 触发面对不对时，都按本 Skill 执行 —— 即使对方只说「出包」「装包」「CI 绿了吗」。
---

# 交付流水线规范（本 fork）

> 与 `release-flow` 的分工：`release-flow` 管**发版**（版本号 / tag / Release / 发布到三个仓）；
> 本 Skill 管**日常交付**（推代码 → 测试 → 产物同源 → Windows 安装包）。
> 发正式版 / beta 版时两条一起看。

## 0. 一句话

**推 `main-patched` = 自动跑完交付三件套。** 不要手动 dispatch，不要等用户提醒出包。

## 1. 交付三件套

```
push main-patched
   ├─ rust-tests          服务端与插件联调（ubuntu / windows / macos 三平台）
   ├─ frontend-artifacts  入库前端产物必须与源码逐字节同源
   └─ build-windows       先过闸门 → 出 Windows NSIS 安装包
```

三段的关系是**串行依赖**，不是并行：

- `frontend-artifacts` 是**前端那道闸**：仓库把前端构建产物**入库**（`desktop-tauri/ui/islands/ui.js` 等），
  Tauri 打包吃的正是 `ui/` 静态目录。只改源码不重建、或手改产物，都能一路合进去还打包成安装包。
  判据是「产物能被源码复现出逐字节相同的结果」。
- `build-windows` 是**出包**：Tauri `nsis` target，exe 直传（`archive:false`，下载下来就是 exe 本体）。
- `gate` job 是**闸门**：在出包之前确认前两道都绿。

## 2. 闸门（scripts/ci-gate.mjs）

`build-windows` 的第一个 job 跑 `scripts/ci-gate.mjs`，它按提交的实际改动文件判定每道闸门是否适用，
再等结论：

| 结论 | 含义 | 是否放行 |
|---|---|---|
| `passed` | run 结论 success | 放行 |
| `failed` | run 结论 failure / timed_out | **拦下**，不产出 artifact |
| `superseded` | run 结论 cancelled —— 被更新的提交顶掉，本提交已作废 | 放行（作废的提交不该再出包） |
| `not-applicable` | 该提交未触及这道闸的 paths，或 run skipped | 放行（这道闸对它不适用） |
| `pending` | run 没出现或还在跑 | 继续等（有上限） |

**为什么需要它**：三段是同一批 push 触发的，天然并行。不设闸门的话，`build-windows` 会在测试还没跑完时
就开始编译，装出一个「测试没过但包已经出了」的安装包 —— 那是最难查的一类问题。

**paths 适用性判定**：闸门读该提交实际改动的文件，与该工作流 `push.paths` 比对。一条都不匹配 = 这道闸
对这个提交不适用（上游不会跑它），立刻放行，不必干等。这是「不误判」的关键：不精确就会白等一小时。

## 3. 状态查询（scripts/ci-watch.mjs）

```bash
node scripts/ci-watch.mjs                      # 查当前 HEAD 的三件套
node scripts/ci-watch.mjs --sha <sha>          # 查指定提交
node scripts/ci-watch.mjs --wait               # 等到全部跑完
node scripts/ci-watch.mjs --wait --timeout 40  # 最长等 40 分钟
node scripts/ci-watch.mjs --artifact           # 打印 artifact 名 / 大小 / id
node scripts/ci-watch.mjs --gate               # 交付自检：任一未绿则 exit 1
node scripts/ci-watch.mjs --dispatch build-windows   # 手动补一次出包
```

令牌来源按优先级：`GH_TOKEN` / `GITHUB_TOKEN` 环境变量 → `gh auth token` → `.workbuddy/tmp/gh_token_value.txt`
（该目录被 `.gitignore` 忽略，只在本机）。

**只读 API，不下载产物。** 产物一律不落盘（`AGENTS.md` 第 2 条）。

## 4. 出包纪律

1. **推完代码就等三件套跑完**，不要问用户「要出包吗」。
2. **不下载安装包**。报「run id + artifact id + 文件名 + 大小」就算交付完成。
3. **包与提交必须对应**：报告时带上 artifact 所属 run 的 `head_sha`，确认它就是你要的那一版。
4. **测试没过就不该有包**。`build-windows` 失败先看是不是 `gate` 拦的 —— 那是设计如此，不是故障。

## 5. 排错

### CI 没自动跑

先看触发面。上游工作流写的是 `branches: [main, beta]`，那是**主仓**的分支名；本 fork 的默认分支与
实际交付分支是 `main-patched`。2026-11-20 之前，本 fork 的三个工作流都只认 `[main, beta]`，
于是 **push 从来不会自动触发任何一条**，历史上每个 run 都是手动 `workflow_dispatch` ——
「测试忘了跑」「安装包忘了出」反复发生的根因就在这里。触发面已补齐 `main-patched`。

### 闸门卡住不动

看 `[ci-gate]` 输出的 `pending` 项：多半是上游 run 还没出现。确认那个提交是否真的触发了对应工作流
（看 Actions 页有没有 run），而不是闸门判错了。

### `build-windows` 红了但没有 exe

这是**预期行为**：闸门拦下时 `windows` job 根本不会跑。去看 `gate` job 的日志，那里写明是哪道闸、哪个 run。

### 只改了 `.md` 会不会触发

`build-windows` 的 `paths-ignore` 排掉 `docs/**` 与 `**.md`；`rust-tests` 有 `paths` 白名单，只在
`desktop-tauri/src-tauri/**` 等命中时才跑。闸门读实际改动文件判定适用性，所以纯文档提交会很快放行。

## 6. 沉淀要求

改这三段流水线时：

- 改 `paths` 白名单 → **同步改** `scripts/ci-gate.mjs` 里的 `GATES[].paths`，否则闸门会误判适用性。
- 新增一段流水线 → 加进 `scripts/ci-watch.mjs` 的 `TRACKED`。
- 静态检查 / 闸门逻辑改完 → **用真实事故版本反验**（见下）。

### 反验纪律（2026-11-20 立）

本 fork 已经吃过三次「看起来对但其实是错的检查」的亏：`unwired_knobs_explain_themselves` 一次只报一条、
Windows 时钟让测试跨分钟、`super::model` 在 `mod tests` 里层级变了。

所以**任何新增的检查逻辑，必须拿真实出过错的历史提交反验**，确认它对该拦的拦、对该放的放：

```bash
# 用真实失败过的提交验闸门
GITHUB_TOKEN=... GITHUB_REPOSITORY=liu6617243/agent2api \
GITHUB_SHA=$(git rev-parse 8b58c262) node scripts/ci-gate.mjs
# 期望：exit 1，闸门未通过
```

反验至少覆盖三类：**真实失败**（必须拦）、**真实成功**（必须放）、**不适用**（必须快速放行）。
只跑一遍「当前代码是绿的」不算反验。
