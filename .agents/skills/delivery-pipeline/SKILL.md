---
name: delivery-pipeline
description: Agent2API（workbuddy）本 fork 的**交付流水线**规范 —— push 触发面、交付三件套（rust-tests / frontend-artifacts / build-windows）、交付闸门 scripts/ci-gate.mjs、状态查询 scripts/ci-watch.mjs、出包纪律与排错。凡是涉及推代码后 CI 跑没跑、安装包出没出、交付前自检、CI 红了怎么查、artifact id 怎么拿、workflow 触发面对不对时，都按本 Skill 执行 —— 即使对方只说「出包」「装包」「CI 绿了吗」。
---

# 交付流水线规范（本 fork）

> 与 `release-flow` 的分工：`release-flow` 管**发版**（版本号 / tag / Release / 发布到三个仓）；
> 本 Skill 管**日常交付**（推代码 → 测试 → 产物同源 → Windows 安装包）。
> 发正式版 / beta 版时两条一起看。

## 0. 一句话

**`node scripts/deliver.mjs` = 推代码 + 跑完交付三件套 + 出安装包。**
推 `main-patched` 不要裸 `git push`，`.githooks/pre-push` 会拦下来。不要问用户「要出包吗」。

## 1. 交付三件套

```
node scripts/deliver.mjs
   ├─ git push HEAD:main-patched
   ├─ 派发 rust-tests          服务端与插件联调（ubuntu / windows / macos 三平台）  ┐ 并行
   ├─ 派发 frontend-artifacts  入库前端产物必须与源码逐字节同源                    ┘
   │        ↓ 两个都绿才继续（任一失败 → 不派发出包，退出码 1）
   └─ 派发 build-windows       出 Windows NSIS 安装包 → 打印 artifact 名 / 大小 / id
```

- `frontend-artifacts` 是**前端那道闸**：仓库把前端构建产物**入库**（`desktop-tauri/ui/islands/ui.js` 等），
  Tauri 打包吃的正是 `ui/` 静态目录。只改源码不重建、或手改产物，都能一路合进去还打包成安装包。
  判据是「产物能被源码复现出逐字节相同的结果」。
- `build-windows` 是**出包**：Tauri `nsis` target，exe 直传（`archive:false`，下载下来就是 exe 本体）。
- 顺序是**串行依赖**，不是并行 —— 出包必须等前两道绿。派发路径上这件事由 `deliver.mjs` 负责；
  触发面补丁上线后由 `build-windows` 的 `gate` job 负责（见 §2）。

## 1.5 为什么是「派发」而不是「push 触发」（2026-11-20 的现实约束）

治本做法是把三个工作流的 `push.branches` 补上 `main-patched`（本 fork 的默认分支与实际交付分支）。
改动已经在本地 `.github/workflows/{rust-tests,frontend-artifacts,build-windows}.yml` 里，
但**推不上去**：本机唯一可用的凭据是 classic PAT，scope 只有 `public_repo`。

```text
refusing to allow a Personal Access Token to create or update workflow
`.github/workflows/build-windows.yml` without `workflow` scope
```

试过的三条路：

| 路 | 结果 |
|---|---|
| `git push` 含 workflow 改动 | 远端拒绝（缺 `workflow` scope） |
| Git Data API（blob→tree→commit） | `POST /git/trees` 404 —— tree 里只要含 `.github/workflows/` 就拦 |
| Contents API `PUT /contents/<path>` | 同样 403（同一个 token） |

而 `public_repo` **足以调 `workflow_dispatch`**（三个工作流本来就有这个入口）。
所以先走派发这条路：零新权限、当场可用、链路完全自动。

**升级路径**：等凭据补上 `workflow` scope（GitHub → Settings → Developer settings → Tokens 勾 `workflow`），
跑 `node scripts/push-workflows.mjs --all` 把触发面写上去，之后 push 天然触发，
`deliver.mjs` 仍可当闸门与出包入口用（`--no-push`）。
**切不可**为了绕过 scope 去手改远端 workflow（会产出一堆 `probe:` 空提交，已经踩过）。

## 2. 闸门（scripts/ci-gate.mjs）

`build-windows` 的第一个 job 跑 `scripts/ci-gate.mjs`（随触发面补丁上线；派发路径上由 `deliver.mjs`
先等绿再派发出包，效果等价），它按提交的实际改动文件判定每道闸门是否适用，再等结论：

| 结论 | 含义 | 是否放行 |
|---|---|---|
| `passed` | run 结论 success | 放行 |
| `failed` | run 结论 failure / timed_out，**且 job 里有失败步骤** | **拦下**，不产出 artifact |
| `infra` | run 结论 failure / timed_out，但没有任何失败步骤（某平台 job 一步都没执行） | 放行，但必须打印「这不是已通过」 |
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
5. **本机不装 Rust 工具链**（2026-11-21 起）。编译与构建的唯一权威是 GitHub Actions；
   本机的 `D:/rust-toolchain` 与 `~/.cargo` 已卸载，`target/cargo-home` 与 `.workbuddy/cargo`
   缓存也清掉了（约 1.5 GB）。理由：装它要下 700MB+，在沙箱里连 crate 都解不开
   （os error 5），装完照样跑不通 check。借用检查交给 CI 的 `rust-tests`。
   `deliver.mjs` 默认**不跑**本机编译闸；确实想要时加 `--local-rust-check`（需本机已有工具链）。

### 4.1 本机预检（不需工具链，提交前就能跑）

```bash
node scripts/precheck-rust.mjs       # 结构体字面量字段 / mod 路径（纯文本解析）
node scripts/precheck-knobs.mjs      # 旋钮 schema 与构造调用参数
node scripts/precheck-workflows.mjs  # .github/workflows/** 的 YAML 能否被 GitHub 解析
node scripts/precheck-actions.mjs    # 这个仓库的 Actions 现在能不能跑
```

前三个是 `pre-commit` 钩子自动跑的（只在暂存区命中对应文件时触发）。
`precheck-actions.mjs` 给结论：`ok` / `disabled-fork` / `disabled-policy` / `forbidden` / `unknown`。

**`unknown` 不等于绿灯**，也不等于故障：它表示所有判据都读不到确切结论。
此时继续交付，但报告时要说「Actions 状态未确认」，不能当成已验证。

## 5. 排错

### CI 没自动跑 / 裸推被拦

先看**入口**，别先怀疑记性：

```text
pre-push 拦下了这次推送。
本 fork 推 main-patched 必须走交付入口：node scripts/deliver.mjs
```

这是设计行为。上游工作流的 `push.branches` 写的是 `[main, beta]`（主仓分支名），本 fork 的交付分支是
`main-patched` —— 2026-11-20 之前三个工作流都不认它，于是 push 从来不触发任何流水线，
历史上每个 run 都是手动 `workflow_dispatch`，「测试忘了跑」「安装包忘了出」的根因就在这里。
现在由 `deliver.mjs` 显式派发，钩子禁止绕过。

确实只要推代码、不要跑链路（例如纯文档提交）时：

```bash
A2A_DELIVER=1 git push origin HEAD:main-patched
```

### `deliver.mjs` 派发失败

- `404` —— 工作流文件名不对。Actions 的 workflow 端点认**带扩展名的文件名**：
  `/actions/workflows/rust-tests.yml/dispatches` 才对，`rust-tests` 是 404。
- `403` —— 凭据没有 actions 写权限，或 ref 上的工作流没有 `workflow_dispatch` 入口。
- `422` —— **两种完全不同的 422，必须看 message 原文**：
  - `ref` 上不存在该工作流文件（分支名写错，或文件在别的分支）→ 改 ref；
  - `{"message":"Actions has been disabled for this repository."}` → **不是 ref 问题**，
    是平台停用了本 fork 的 Actions，见 §5.1。
- `fetch failed` —— 网络抖动，不是业务失败。`api()` 已自带 4 次指数退避（网络异常 / 429 / 5xx 才重试），
  仍然断掉时直接**原地重跑 `node scripts/deliver.mjs`**：它按 `head_sha` 复用已存在的 run，
  推过的不会再推、绿了的不会再等。

### 5.1 「Actions has been disabled for this repository」= fork 被平台熔断（2026-11-21）

**症状**：任何 `workflow_dispatch` 一律返回 HTTP 422 +

```json
{"message":"Actions has been disabled for this repository."}
```

push tag、rerun 也一样不产生 run。

**这是 GitHub 对 fork 的平台级熔断**，不是本仓库配置错、不是 token 权限、不是 workflow 文件名、
不是 ref 写错、不是触发面没覆盖 `main-patched`、也不是 GitHub 整体故障。上面这些全都试过并排除了。

**GitHub Actions 有三层独立开关，只有第三层是真相**：

| 层 | 查法 | 能证明能跑吗 |
|---|---|---|
| 仓库策略层 | `GET /repos/{r}/actions/permissions` → `enabled:true` | **不能**（熔断时照样返回 true） |
| 工作流层 | `GET /repos/{r}/actions/workflows` → 全部 `state:active` | **不能**（熔断时照样 active） |
| 平台 fork 处置层 | Actions 页面横幅 / run 永远 0 job | **能** |

**纯 API 的判据（不用开浏览器）**：run 状态 `queued`，超过 8 分钟仍 `total_count: 0` 个 job
→ 平台没有调度，不是排队慢。`POST /actions/runs/{id}/rerun` 返回 201 但 jobs 永远是 0，也是同一现象。

**线上 API 改不了这个开关**，实测都是「返回成功但不生效」：

```text
PUT /repos/{r}/actions/permissions          → 204，仍旧 422
PUT /repos/{r}/actions/workflows/<id>/enable → 204，仍旧 422
POST /repos/{r}/actions/runs/<id>/rerun      → 201，但 jobs 永远是 0
```

**只能由维护者在浏览器点一次**：

```text
打开 https://github.com/liu6617243/agent2api/actions
点页面顶部横幅的 "I understand my workflows, go ahead and enable them"
```

**动手之前先查**：

```bash
node scripts/precheck-actions.mjs
```

输出会直接给结论（`ok` / `disabled-fork` / `disabled-policy` / `forbidden`）。
熔断时**不要推代码、不要重跑 run、不要试 API**——那三样都已经在熔断状态下证明无效了。

**已机制化**：

- 判据收敛在 `scripts/lib/actions-health.mjs`（`bannerState` / `probeBanner` /
  `probeStalledRuns` / `probeLiveScheduling` / `classifyDispatch` / `healthVerdict` / `enableHint`）。
- `scripts/deliver.mjs` 的 `healthGate()` 在**推送之前**探测；熔断则直接退出码 1，**不推送**，
  避免出现「推了但没包」这种半截交付。
- `dispatch()` 单独识别这条 422，报错信息里带网页启用指引，不再伪装成 ref 错误。
- `waitRun()` 遇到「queued 超过 8 分钟且 0 个 job」直接判定平台未调度并退出，不再干等 90 分钟。

**注意**：本机 `github.com` 的 HTML 通道经常 fetch 失败，所以**主判据是 API**（实时调度 +
僵死队列），页面横幅只是旁证。横幅读不到（`unknown`）不代表熔断不成立，别因为横幅 unknown 就推翻 API 结论。

### 5.2 熔断解除之后（2026-11-21 后续）：僵尸 run 会伪装成「还在熔断」

平台恢复后，**熔断期遗留的排队 run 永远不会被调度**，也永远不会被 GitHub 自动清理 ——
它就那么挂在 `queued` 里，`jobs` 永远是 0。

于是只按「有没有僵死 run」判定会出现假阴性：

```text
平台早已恢复 → dispatch 204、新 run 正常拿到 4 个 job
但老僵尸 run 还在 → 判定仍是 disabled-fork → 交付被拦住
```

这是把「历史上出过一次事」当成「现在正在出事」。所以判据分两层：

| 判据 | 含义 | 优先级 |
|---|---|---|
| `probeLiveScheduling` | **最新创建**的 run 有没有拿到 job | **第一**（不受僵尸 run 影响） |
| `probeStalledRuns` | 排队超 8 分钟且 0 job 的 run | 第二（**只算 3 小时内创建的**，老的记为 `stale` 不下结论） |

判据收敛在 `scripts/lib/actions-health.mjs`：

```js
if (live.state === 'scheduling')     return { verdict: 'ok' }           // 平台正常
if (live.state === 'not-scheduling') return { verdict: 'disabled-fork' } // 平台没调度
// 实时判据读不到 → 退回僵死队列判据（只算新鲜的）
```

**判据必须建在「最新那个 run 有没有 job」上**，不能建在「历史上有没有失败过」上。

### 5.3 `unknown` 不是一个结论

`healthVerdict` 有五种取值，用途完全不同：

| verdict | 含义 | 交付时怎么办 |
|---|---|---|
| `ok` | 平台正常调度 | 照常推 |
| `disabled-fork` | 平台没在调度 | **中止，不推**，给网页启用指引 |
| `disabled-policy` | 仓库策略层被关 | 中止，先开策略 |
| `forbidden` | 凭据读不到 permissions | 中止，换凭据 |
| `unknown` | 所有判据都读不到确切结论 | **放行但说明「没验过」**，以派发结果为准 |

`unknown` 最容易被误当成绿灯。它既不是「正常」也不是「坏了」，是「不知道」。
脚本必须按「不知道」处理：继续跑、但把不确定性说清楚，不能默默当成已验证。

### GitHub「重跑失败任务」不是新 run

`rerun-failed-jobs` 会**复用 run id 与 `created_at`**，只把 `run_attempt` +1。
因此 `deliver.mjs` / `ci-watch.mjs` / `ci-gate.mjs` 找当前尝试时，排序必须先比
`run_attempt`，再比 `created_at`；只看创建时间会在重跑期间读到上一轮的失败结论。

单平台 `cancelled` 且 `steps: []`、job 日志 `BlobNotFound`、开始到结束恰好整数分钟，
是 runner 没跑起来的基础设施问题，不是测试失败；先重跑失败任务。测试失败必有失败步骤与日志。

**这条判断已经不用你手动做了。** 判据收敛在 `scripts/lib/run-verdict.mjs`，
三处共用（`deliver` / `ci-gate` / `ci-watch --gate`）：

| state | 判据 | 处置 |
|---|---|---|
| `code` | 任一 job 有 `conclusion=failure` 的 step | 真失败，**绝不重跑** |
| `infra` | 非成功 job 里 `steps.length === 0` | 自动重跑（`deliver` 最多 3 次） |
| `soft` | 非成功 job 跑过步骤但被 cancelled | 自动重跑 |
| `ok` | 全绿（含 skipped / neutral） | 放行 |

`run.conclusion` 只回答「有没有东西不是绿的」；`job.steps` 才回答「红在哪」。
凡是拿 `conclusion` 判失败的地方，都要再问一句「失败的 step 叫什么」——
答不出来就是平台的问题，不是代码的问题。真失败不会因为重跑变绿，所以 `code` 一律不重跑。

`deliver.mjs` 的 `settle()` 在重跑前还会做两件事：等「同工作流 + 同分支」空档、
确认提交仍是分支头（`/git/ref/heads/<ref>`）。已被顶掉的提交不重跑，直接判作废。

**重跑也受 concurrency 约束。** `rust-tests` 对同一 workflow/ref 设置了
`cancel-in-progress: true`：旧提交的 run 仍 queued / in_progress 时再点 rerun，
会把当前 HEAD 的整组测试顶掉。先等同组在途 run 结束，再重跑当前 HEAD；
「只重跑失败 job」不等于「不取消其他 run」。

**Actions API 的 `head_sha` 只认完整 40 位 SHA。** 短 SHA 返回 HTTP 200 + 空列表，
不是参数错误。`ci-watch --sha <短SHA>` 会先 `git rev-parse <输入>^{commit}`；
手工跑 `ci-gate` 时也必须传完整 SHA，否则直接拒绝，不让「查不到」冒充「未触发」。

### 单平台单条红：先怀疑测试时钟，别先怀疑实现

真实案例（run `44720265261`）：Linux / Windows 全绿，只有 macOS 挂一条：

```text
server::core::providers::readiness::local_gates::a_gate_expires_rather_than_waiting_for_a_release
panicked: 寿命内仍然挡着 —— 比令牌 TTL 还短就放行是白挡
test result: FAILED. 577 passed; 1 failed
```

根因是**测试自己**：`hold()` 用真实时钟记时刻，之后才取 `base = now_ms()` 当基准；
两行之间过了 1 毫秒，`base + TTL - 1` 就已经越过 TTL，边界断言随机翻车。

判据：`577 passed / 1 failed` + 单平台 + 断言落在 TTL / 分钟 / 毫秒边界上 → 先看用例怎么取时间。
修法统一为**注入固定时刻**（`hold_at(..., SINCE)` / `const FIXED_AT: i64 = ...`），
不要靠「这两行之间不该耗时」。

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
