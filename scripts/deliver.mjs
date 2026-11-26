// 交付一条命令：推代码 → 等闸门 → 出安装包
//
// ── 为什么需要它 ─────────────────────────────────────────────────
// 本 fork 的三个工作流，`push.branches` 写的是上游分支 `[main, beta]` /
// `[v3.3.0-beta.3]`，而本 fork 的默认分支与实际交付分支是 `main-patched`。
// 结果：**推了代码，CI 与出包都不会自己跑**，只能靠人记得去点。
//
// 治本的办法是把触发面补成 `main-patched`（改动已在本地 `.github/workflows/`），
// 但本机唯一可用的凭据是 classic PAT、scope 只有 `public_repo`，
// 写 `.github/workflows/**` 会被拒：
//
//   refusing to allow a Personal Access Token to create or update workflow
//   `.github/workflows/build-windows.yml` without `workflow` scope
//
// 而 `public_repo` **足以调用 workflow_dispatch**（三个工作流都有这个入口）。
// 所以先按“派发”这条路把链条闭合 —— 不依赖任何新权限、当场可用。
// 等凭据补上 `workflow` scope 后，`scripts/push-workflows.mjs` 把触发面
// 写上去，那时连这一步派发都可以省掉（本脚本仍能当闸门用）。
//
// ── 链路 ─────────────────────────────────────────────────────────
//   push HEAD:main-patched
//     ├─ 派发 rust-tests.yml          ┐ 并行
//     └─ 派发 frontend-artifacts.yml  ┘
//            ↓ 两个都绿
//        派发 build-windows.yml → 打印 artifact 文件名 / 大小 / id
//
// 任一闸门失败 → 不派发出包，退出码 1。
//
// ── 假红自动重跑（2026-11 补）──────────────────────────────────
// 只看 run.conclusion 会把「平台没把 job 跑起来」当成代码失败：本仓反复
// 出现某个平台 job 分到 runner 后一步都没执行（steps 为空），15 分钟被平台
// 判 cancelled，run 结论于是 failure。真有断言失败时一定留下失败步骤，
// 所以这里下钻到 job 级别（scripts/lib/run-verdict.mjs）：
//   code  有失败步骤        → 直接判失败，绝不重跑
//   infra 0 个步骤          → 自动重跑，最多 --infra-retries 次（默认 3）
//   soft  跑过步骤但被取消  → 同上
// 重跑前先等「同工作流 + 同分支」空档（concurrency 是 cancel-in-progress，
// 重跑会顶掉在跑的 run），并且要求提交仍是分支头 —— 已被顶掉的提交不重跑。
//
// ── 用法 ─────────────────────────────────────────────────────────
//   node scripts/deliver.mjs                # 推 HEAD 并跑完整链路
//   node scripts/deliver.mjs --no-push      # 已推过，只跑闸门与出包
//   node scripts/deliver.mjs --no-build     # 只跑测试与产物同源，不出包
//   node scripts/deliver.mjs --sha <sha>    # 指定提交
//   node scripts/deliver.mjs --timeout 5400 # 单段等待上限（秒，默认 5400）

import fs from 'node:fs'
import path from 'node:path'
import { execFileSync, spawnSync } from 'node:child_process'
import { jobVerdict, describeVerdict, ignoredJob } from './lib/run-verdict.mjs'
import { healthVerdict, enableHint, classifyDispatch } from './lib/actions-health.mjs'

const ROOT = path.resolve(import.meta.dirname, '..')
const REPO = process.env.A2A_REPO || 'liu6617243/agent2api'
const API = `https://api.github.com/repos/${REPO}`
const REF = process.env.A2A_REF || 'main-patched'

const argv = process.argv.slice(2)
const has = (f) => argv.includes(f)
const val = (f, d) => {
  const i = argv.indexOf(f)
  return i >= 0 && argv[i + 1] ? argv[i + 1] : d
}

const NO_PUSH = has('--no-push')
const NO_BUILD = has('--no-build')
const TIMEOUT = Number(val('--timeout', '5400')) * 1000
const POLL = 15_000
// ── 只交付 Windows：默认把 macOS 的 job 排除在判定之外（2026-11-24）────
// 本 fork 的产物只有 Windows 安装包（`build-windows.yml`），macOS 那个 job 在
// `rust-tests.yml` 的 matrix 里纯粹是拖累：public_repo 的 macOS runner 排队
// 30~60 分钟是常态，还经常「分到 runner 后 0 个步骤被判 cancelled」
// （`scripts/lib/run-verdict.mjs` 的模块头写的就是这个假红）。
//
// 顺手想把它从 matrix 里删掉，但**git push 推不上去**：本机 PAT 只有 `public_repo`，
// 缺 `workflow` scope，GitHub 直接拒：
//   refusing to allow a Personal Access Token to create or update workflow
//   `.github/workflows/rust-tests.yml` without `workflow` scope
// 后来试过两条路，都堵在同一个 scope 上：
//   · `node scripts/push-workflows.mjs`（Contents API PUT）→ 403，同一个 token；
//     GET 能读（200），PUT 才要 `workflow` scope。
//   · 删 matrix 的那个 commit 暂存在本地分支 `ci/rust-tests-no-macos`，
//     等有 workflow scope 的凭据再落（`git push origin ci/rust-tests-no-macos`
//     之后网页开 PR，或用带 scope 的 token 直接推）。
// 所以这里的跳过逻辑是**当前唯一生效**的手段，不要删。
//
// 想恢复三平台判定：`A2A_SKIP_OS= node scripts/deliver.mjs`（空串 = 不跳过）。
const SKIP_OS = (process.env.A2A_SKIP_OS ?? 'macos')
  .split(',')
  .map((s) => s.trim())
  .filter(Boolean)
// job 名形如「服务端与插件联调 (macos-latest)」，用 `(macos` 当匹配串就够，
// 不必猜 runner 的具体版本号（macos-14 / macos-15 都命中）。
const SKIP_PATTERNS = SKIP_OS.map((os) => `(${os}`)
// 假红（平台没启动 job）自动重跑的次数上限。真失败永远不重跑，见 settle()。
const MAX_INFRA_RERUN = Number(val('--infra-retries', '3'))
// 重跑前等「同工作流 + 同分支」空档的上限。rust-tests 的 concurrency 是
// cancel-in-progress，重跑一个旧 run 会顶掉正在跑的那个。
const QUIET_TIMEOUT = Number(val('--quiet-timeout', '1200')) * 1000

// ── 凭据 ────────────────────────────────────────────────────────────
function token() {
  const local = '.workbuddy/tmp/gh_token_value.txt'
  if (fs.existsSync(local)) {
    const t = fs.readFileSync(local, 'utf8').trim()
    if (t) return t
  }
  const env = process.env.GH_TOKEN || process.env.GITHUB_TOKEN
  if (env) return env.trim()
  try {
    const raw = execFileSync('git', ['credential', 'fill'], {
      input: 'protocol=https\nhost=github.com\n\n',
      encoding: 'utf8',
      stdio: ['pipe', 'pipe', 'ignore'],
    })
    const m = raw.match(/^password=(.+)$/m)
    if (m) return m[1].trim()
  } catch {
    /* 没配 helper */
  }
  throw new Error('找不到 GitHub 凭据')
}

const TOKEN = token()
const H = {
  Authorization: `Bearer ${TOKEN}`,
  Accept: 'application/vnd.github+json',
  'X-GitHub-Api-Version': '2022-11-28',
  'User-Agent': 'agent2api-deliver',
  'Content-Type': 'application/json',
}

/**
 * 一次 API 调用，带网络级重试。
 *
 * ── 为什么必须重试 ─────────────────────────────────────────
 * 整条链路要跑十几分钟、几十次请求。只要其中一次撞上 `fetch failed`
 * （DNS 抖动、连接被 reset —— 本机直连 GitHub 经常这样），链路就断在半路，
 * 人得手动再跑一遍。已经真踩过：推送成功后第一次轮询就 `fetch failed`，
 * 出包闸门还没等到。重试 + 指数退避，把「偶发网络」和「真失败」分开。
 *
 * 只对「可能自己好」的错重试：网络异常、429、5xx。4xx 是请求本身错，
 * 重试只是把同一个错误问四遍。
 */
async function api(path, init) {
  let last = { status: 0, ok: false, json: null, text: 'not attempted' }
  for (let attempt = 0; attempt < 4; attempt += 1) {
    if (attempt > 0) await sleep(800 * 2 ** (attempt - 1))
    try {
      const r = await fetch(`${API}${path}`, { ...init, headers: H })
      const text = await r.text()
      let json = null
      try {
        json = text ? JSON.parse(text) : null
      } catch {
        /* 204 无体 */
      }
      if (r.status === 429 || r.status >= 500) {
        last = { status: r.status, ok: false, json, text }
        continue
      }
      return { status: r.status, ok: r.ok, json, text }
    } catch (error) {
      last = { status: 0, ok: false, json: null, text: String((error && error.message) || error) }
    }
  }
  return last
}

const sleep = (ms) => new Promise((r) => setTimeout(r, ms))

// ── 1.5 本机编译闸（Rust 借用检查）—— 默认关闭，需显式开启 ─────────
//
// ── 为什么默认关闭 ────────────────────────────────────────────
// 2026-11-21 决定：**GitHub Actions 是唯一的构建与编译权威**。本机装 Rust
// 工具链（下载 700MB+、建 CARGO_HOME、绕沙箱解包限制）代价高、稳定性差，
// 而且在沙箱里 cargo 连 crate 都解不开 —— 装完照样跑不通 check。
//
// 既然 CI 一定能跑，编译检查就交给 CI。本机这一关降级为**可选加速**：
//
//   node scripts/deliver.mjs --local-rust-check
//
// 想在推之前先拿到借用检查结果的人可以开；默认不开，交付链路不被本机
// 环境绑死。
//
// ── 为什么这道闸值得保留（当 opt-in）────────────────────────────
// 2026-11-20 烧掉一整轮 CI 的根因：`precheck-rust.mjs` 是**纯文本解析**，
// 它抓得住「结构体字段没同步」，抓不住**借用检查**：
//
//   error[E0507]: cannot move out of `other.id` which is behind a shared reference
//
// 三个平台各跑 20 分钟，全部红在同一条，而本地从提交到推送一路畅通。
// 本机若恰好有 Rust 工具链，先 check 一遍能省下这一轮。
//
// ── 失败怎么处置 ────────────────────────────────────────────────
//   · 代码有错（cargo check 红）→ 中止，不推。这正是要拦的。
//   · 环境没准备好（没工具链 / 依赖没拉下来）→ 放行并明确说「这一关没验过」。
//     环境问题不该把整条链路卡死，但也不能当成通过了。
async function rustGate() {
  if (!has('--local-rust-check')) return
  if (has('--skip-rust-check')) {
    console.log('跳过本机 Rust 编译闸（--skip-rust-check）。')
    return
  }

  // 本次提交动了 Rust 源码才跑
  let changed = ''
  try {
    const base = execFileSync('git', ['rev-parse', `${SHA}^`], { encoding: 'utf8' }).trim()
    changed = execFileSync('git', ['diff', '--name-only', base, SHA], { encoding: 'utf8' })
  } catch {
    changed = execFileSync('git', ['show', '--name-only', '--format=', SHA], { encoding: 'utf8' })
  }
  const touchedRust = changed
    .split('\n')
    .some((f) => f.trim() && /^desktop-tauri\/src-tauri\/.*\.rs$/.test(f.trim()))

  if (!touchedRust) {
    console.log('本次提交没动 Rust 源码，跳过本机编译闸。')
    return
  }

  console.log('本次提交动了 Rust 源码，跑本机借用检查 …')
  const r = spawnSync(process.execPath, [path.join(ROOT, 'scripts', 'rust-check.mjs')], {
    cwd: ROOT,
    stdio: 'inherit',
  })
  if (r.status === 0) {
    console.log('本机编译闸通过。')
    return
  }
  // 退出码 2 = 本机没有工具链（环境问题，不是代码问题）
  if (r.status === 2) {
    console.log('')
    console.log('本机没有 Rust 工具链，这一关没验过 —— 借用检查交给 CI 兜底。')
    console.log('（默认交付链路本就不跑本机编译闸；这里是 --local-rust-check 主动要求的结果。）')
    return
  }
  console.error('')
  console.error('本机借用检查不通过，已中止（未推送）。')
  console.error('  这条错误推上去就会让三个平台的 CI 各红一次。')
  console.error('  确有特殊情况可 --skip-rust-check 跳过，但别常用。')
  process.exit(1)
}

// ── 1. 推送 ─────────────────────────────────────────────────────────
function sha() {
  const given = val('--sha', '')
  if (given) return execFileSync('git', ['rev-parse', given], { encoding: 'utf8' }).trim()
  return execFileSync('git', ['rev-parse', 'HEAD'], { encoding: 'utf8' }).trim()
}

// ── 提交身份 ──────────────────────────────────────────────────────
// SHA 必须是**可变的**：API 直推有一个副作用 —— GitHub 的 commit API 会剥掉
// message 末尾的换行，本地 commit 与远端 commit 因此可能算出不同 sha，
// 脚本会 `git reset --soft` 把本地对齐到远端那个 sha。此时「这次交付的提交」
// 已经变了，后续所有按 head_sha 找 run 的逻辑必须用新的 sha，否则会一直
// 找不到 run（判据：run 的 head_sha 必须等于本次真正推上远端的那个 commit）。
let SHA = sha()
let SHORT = SHA.slice(0, 8)

/** 推送后重新读取本地 HEAD（API 直推对齐后 sha 会变）。 */
function refreshSha() {
  // 用户用 --sha 钉死了要交付的提交 → 不动它。
  if (val('--sha', '')) return
  const now = execFileSync('git', ['rev-parse', 'HEAD'], { encoding: 'utf8' }).trim()
  if (now !== SHA) {
    console.log(`  本地提交已对齐到远端 sha：${SHORT} → ${now.slice(0, 8)}`)
    SHA = now
    SHORT = now.slice(0, 8)
  }
}

async function push() {
  const attempts = [
    ['origin'],
    ['https://ghfast.top/https://github.com/liu6617243/agent2api.git'],
  ]
  for (const [remote] of attempts) {
    const label = remote === 'origin' ? '直连' : '镜像 ghfast.top'
    console.log(`推送 ${SHORT} → ${REF}（${label}）…`)
    try {
      execFileSync('git', ['push', remote, `HEAD:${REF}`], {
        stdio: ['ignore', 'pipe', 'pipe'],
        encoding: 'utf8',
        // .githooks/pre-push 会拦裸推 main-patched；这里是官方入口，放行。
        env: { ...process.env, A2A_DELIVER: '1' },
      })
      console.log('  推送成功')
      return true
    } catch (e) {
      const msg = `${e.stdout || ''}${e.stderr || ''}`.trim()
      const upToDate = /Everything up-to-date/i.test(msg)
      if (upToDate) {
        console.log('  远端已是最新')
        return true
      }
      console.log(`  失败：${msg.split('\n').slice(-3).join(' / ').slice(0, 200)}`)
    }
  }

  // ── 回退：GitHub REST API 直推 ────────────────────────────────
  // 本机到 github.com:443 经常整段不通（Recv failure / CONNECT tunnel 502），
  // 而 api.github.com 是通的 —— 所以 `git push` 不可用时，GitHub API 才是
  // 本机唯一稳定的推代码通道。
  //
  // 关键：**这条回退必须留在交付链路内部**。历史上同一份脚本放在
  // `.workbuddy/tmp/`（被 gitignore），于是它成了一条绕过交付的暗道 ——
  // 推上去了，但 CI 没跑、安装包没出，2026-11-26 就这么漏过一次。
  // 现在它进了仓库（scripts/push-local-commit.mjs），并且由本脚本调用，
  // 推送成功后继续走闸门与出包，链路不会断。
  console.log(`推送 ${SHORT} → ${REF}（GitHub API 直推）…`)
  try {
    const { pushViaApi } = await import('./push-local-commit.mjs')
    // 推的是 HEAD（与上面 git push HEAD:REF 同一个东西）。
    // SHA 可能被 --sha 钉成别的提交，那只影响「按哪个 head_sha 找 run」，
    // 不影响「推哪个 commit」。
    const head = execFileSync('git', ['rev-parse', 'HEAD'], { encoding: 'utf8' }).trim()
    const out = await pushViaApi({
      token: TOKEN,
      repo: REPO,
      branch: REF,
      localSha: head,
      cwd: ROOT,
      log: (m) => console.log(`  ${m}`),
    })
    if (!out.ok) {
      console.log(`  失败（${out.reason}）：${out.message}`)
      return false
    }
    if (out.alreadyUpToDate) {
      console.log('  远端已是最新')
      return true
    }
    console.log('  推送成功')
    refreshSha()
    return true
  } catch (e) {
    console.log(`  失败：${String(e?.message || e).slice(0, 300)}`)
    return false
  }
}

// ── 2. 派发 ─────────────────────────────────────────────────────────
async function dispatch(file) {
  const r = await api(`/actions/workflows/${file}/dispatches`, {
    method: 'POST',
    body: JSON.stringify({ ref: REF }),
  })
  const verdict = classifyDispatch(r.status, r.text)
  if (verdict.kind === 'ok') return null
  // 熔断单独给出口：这不是「参数写错」，改 ref / 改文件名都没用，
  // 唯一解法是维护者在网页上点一次启用。混在通用错误里会让人白改一轮。
  if (verdict.kind === 'disabled-fork') {
    return {
      fatal: true,
      message: [
        `派发 ${file} 被拒：平台已停用本 fork 的 Actions（HTTP 422）。`,
        '',
        enableHint(REPO),
      ].join('\n'),
    }
  }
  return { fatal: true, message: `派发 ${file} 失败 ${r.status}: ${r.text.slice(0, 300)}` }
}

/**
 * 推送前先确认「这个仓库的 Actions 能用」。
 *
 * ── 为什么放在推送之前 ─────────────────────────────────────
 * 2026-11-21 的顺序是：推成功了 → 派发 422 → 才知道跑不了。代码已经上了远端，
 * 本地看起来一切正常，只剩一句 422 —— 用户看到的是「推了但没包」。
 * 先探测，就在**动手之前**给出结论，而不是事后解释。
 *
 * 探测不到（网络问题）不算失败：宁可放行让它继续推，也不要在网络抖动时
 * 把整条链路拦死。只有明确读到「fork 被熔断」才中止。
 */
async function healthGate() {
  const { verdict, evidence, hint } = await healthVerdict({ api, repo: REPO, token: TOKEN })
  if (verdict === 'disabled-fork' || verdict === 'disabled-policy') {
    console.error('')
    console.error('这个仓库的 GitHub Actions 现在跑不了，推代码不会触发任何流水线。')
    console.error(`  判定：${verdict}`)
    if (evidence?.stalledRuns?.length) {
      console.error(`  证据：run ${evidence.stalledRuns.join(',')} 排队很久但一个 job 都没分到`)
    }
    console.error('')
    console.error(hint)
    console.error('')
    console.error('已中止（未推送）。')
    process.exit(1)
  }
  if (verdict === 'forbidden') {
    console.error('')
    console.error('读不到仓库 Actions 策略 —— 凭据权限不足。')
    console.error('')
    console.error(hint)
    process.exit(1)
  }
  console.log('Actions 可用。')
}

// ── 3. 找 run / 等 run ──────────────────────────────────────────────
// 认「同一个 head_sha」的 run，不看它是不是刚刚派发的 —— 这样本脚本**幂等**：
// 重复跑不会重复出包，已经绿过的直接复用。
//
// ── 为什么复用要看 attempt 而不是 created_at ────────────────────
// GitHub 的「rerun failed jobs」**复用同一个 run id、保留 created_at**，只把
// `run_attempt` +1。按 created_at 找「最新的那次」会在重跑期间拿到上一轮的
// failure 结论，于是刚点完 rerun、再跑一次本脚本就立刻被判「闸门未通过」——
// 用户只能干等 CI 跑完再重跑，而 CI 往往十几分钟。
//
// 所以这里先按 attempt 降序、再按 created_at 降序：attempt 是 GitHub 给
// 「同一个 run 的第几次尝试」的编号，它才是真正的时间序。字段不存在的旧数据
// 视为 1（API 文档里 run_attempt 从 1 开始）。
const REUSE_WINDOW = 6 * 3600 * 1000

async function findRun(file, after = 0) {
  const r = await api(`/actions/workflows/${file}/runs?branch=${REF}&per_page=30`)
  if (!r.ok) return null
  const runs = (r.json?.workflow_runs || []).filter(
    (x) => x.head_sha === SHA && new Date(x.created_at).getTime() >= after,
  )
  runs.sort((a, b) => {
    const attempt = (b.run_attempt || 1) - (a.run_attempt || 1)
    if (attempt !== 0) return attempt
    return new Date(b.created_at) - new Date(a.created_at)
  })
  return runs[0] || null
}

async function waitRun(file, label, deadline, after = 0, minAttempt = 1) {
  let seen = null
  let lastAttempt = null
  let stuckSince = 0
  let warned = false
  // 每 4 次轮询（≈1 分钟）才查一次 job 明细：`/jobs` 比 `/runs` 重，
  // 而「被跳过的平台还在排队吗」不需要秒级新鲜度。
  let jobTick = 0
  while (Date.now() < deadline) {
    const run = await findRun(file, after)
    // attempt 变了（有人在 GitHub 上点了 rerun）要当成新的一次重新打印 ——
    // 否则日志里只有一行「run xxx」，用户看不出它已经被重跑过。
    if (run && (run.id !== seen || run.run_attempt !== lastAttempt)) {
      seen = run.id
      lastAttempt = run.run_attempt
      stuckSince = Date.now()
      console.log(`  ${label}: run ${run.id}（${run.status}，第 ${run.run_attempt || 1} 次）`)
    }
    // 刚点完 rerun 时 API 还会短暂返回上一轮的 completed 结论；必须等
    // attempt 涨到目标值，否则会立刻拿旧结论下判断。
    if (run && run.status === 'completed' && (run.run_attempt || 1) >= minAttempt) return run

    // ── 需要的平台跑完了 → 不再等被跳过的平台（2026-11-24）──────────
    // macOS 那个 job 可能排队一小时，也可能永远分不到 runner。只要「要看的
    // job」全部 completed，这一轮就没有更多信息可等：取消整轮（把还在排队的
    // 那个一起清掉，免得它继续占 concurrency group），把结论交给 settle() 里
    // 过滤后的 verdict。
    if (run && run.status !== 'completed' && SKIP_PATTERNS.length && ++jobTick % 4 === 0) {
      const jobs = await jobsOf(run.id)
      const skipped = jobs.filter((job) => ignoredJob(job, SKIP_PATTERNS))
      const wanted = jobs.filter((job) => !ignoredJob(job, SKIP_PATTERNS))
      const stillQueued = skipped.filter((job) => job.status !== 'completed')
      if (wanted.length && stillQueued.length && wanted.every((job) => job.status === 'completed')) {
        const res = await api(`/actions/runs/${run.id}/cancel`, { method: 'POST' })
        console.log(
          `  ${label}: 需要的 job 都结束了，取消还在排队的 ${stillQueued.map((job) => job.name).join('、')}（HTTP ${res.status}）`,
        )
        return { ...run, status: 'completed', conclusion: 'cancelled' }
      }
    }

    // ── 永远 queued、一个 job 都没有 = 平台没在调度 ───────────────
    // 这不是「跑得慢」。fork 熔断时不但 dispatch 会 422，已经被启用的旧
    // run（rerun 出来那个）也会一直 queued、`/jobs` 永远 `total_count: 0`。
    // 干等一整轮（默认 90 分钟）毫无意义，8 分钟没动静就给明确结论。
    if (run && run.status === 'queued' && !warned) {
      if (!stuckSince) stuckSince = Date.now()
      if (Date.now() - stuckSince > 8 * 60_000) {
        const jobs = await jobsOf(run.id)
        if (!jobs.length) {
          warned = true
          console.error(
            `  ${label}: run ${run.id} 排队 8 分钟仍未分到任何 job（jobs=0）—— 平台没有调度，不是测试慢。`,
          )
          console.error('  这通常意味着本 fork 的 Actions 被停用。')
          console.error(enableHint(REPO))
          return null
        }
      }
    }
    await sleep(POLL)
  }
  return null
}

// 已有成功 run → 复用；已有跑着的 → 等；否则派发。
async function ensureRun(file, label, deadline) {
  const existing = await findRun(file)
  if (existing) {
    const age = Date.now() - new Date(existing.created_at).getTime()
    if (age < REUSE_WINDOW) {
      if (existing.status === 'completed' && existing.conclusion === 'success') {
        console.log(`  ${label}: 复用 run ${existing.id}（已成功）`)
        return existing
      }
      if (existing.status !== 'completed') {
        console.log(`  ${label}: 已有 run ${existing.id} 在跑，等它`)
        return waitRun(file, label, deadline)
      }
      // 已完成但不是 success：原样交给 settle() 判真假红。
      // 这里**不再重新派发** —— 同一个提交重开一个 run 会跟旧 run 抢同一个
      // concurrency group，且真假红的分辨与重跑策略只应该有一处（settle）。
      console.log(`  ${label}: 已有 run ${existing.id}（${existing.conclusion}），校验真假红`)
      return existing
    }
  }
  // 记下派发时刻：重新派发时旧 run 还在列表里，不过滤就会立刻「拿到」那个旧的失败结果。
  const base = Date.now() - 60_000
  const failure = await dispatch(file)
  if (failure) {
    console.error(failure.message || failure)
    process.exit(1)
  }
  return waitRun(file, label, deadline, base)
}

// 打勾看的是 **settle 的判定**，不是 run.conclusion。
//
// ── 为什么不能直接读 run.conclusion（2026-11-24 撞到）──────────────
// 跳过平台是靠「需要的 job 都完了 → 主动取消还在排队的那个」实现的（见
// settle 里的 cancel 调用）。这一刀会让 run 级结论写成 `cancelled`，
// 而 job 级判定（jobVerdict + ignore）依然是 `ok`。
//
// 于是 `MARK(run.conclusion)` 打出 `FAIL 服务端测试`，紧接着下一行却打印
// 「闸门全绿，开始出包」—— 两行自相矛盾。流程是对的（走的是 `ok`），
// 错的只是这一行显示。人看到 FAIL 会以为测试挂了，去翻一个并不存在的代码失败。
//
// 正确形状：判定为通过就只可能是 OK / SKIP，绝不显示 FAIL。
//   ok    → run 全绿写 OK；结论是 cancelled/skipped 且靠跳过平台过关的写 SKIP
//   !ok   → FAIL（真失败 / 超时 / 平台没跑起来）
const MARK = (out) => {
  if (!out?.ok) return out?.run ? 'FAIL' : 'TIMEOUT'
  const conclusion = out.run?.conclusion
  if (conclusion === 'success') return 'OK  '
  // 这里只剩「job 全绿但 run 汇总不是 success」——被跳过的平台拖累了汇总。
  return 'SKIP'
}

// ── 4. 判真假红 / 自动重跑 ──────────────────────────────────────────
// ── 为什么必须到 job 级别去判 ────────────────────────────────────
// run.conclusion 只是「有没有 job 不是 success」的汇总。这个仓库反复出现的
// 假红是：某个平台的 job 分到 runner 后**一个步骤都没执行**（steps 为空），
// 15 分钟后被平台判 cancelled，run 结论于是 failure。真有断言失败时，一定
// 会留下一个 conclusion=failure 的 step —— 这条判据把两者分得干干净净。
//
// 判成假红就直接重跑，不再等人来点。重跑前必须等「同工作流 + 同分支」空档：
// rust-tests 的 concurrency group 是 workflow+ref 且 cancel-in-progress，
// 重跑旧 run 会把当前在跑的那个顶掉 —— 踩过一次，整轮四个 job 全 cancelled。
async function jobsOf(runId) {
  const r = await api(`/actions/runs/${runId}/jobs?per_page=100`)
  return r.ok ? r.json?.jobs || [] : []
}

async function branchHead() {
  const r = await api(`/git/ref/heads/${REF}`)
  return r.ok ? r.json?.object?.sha || null : null
}

async function quiet(file, runId, deadline) {
  for (;;) {
    const r = await api(`/actions/workflows/${file}/runs?branch=${REF}&per_page=30`)
    const busy = r.ok
      ? (r.json?.workflow_runs || []).filter((x) => x.status !== 'completed' && x.id !== runId)
      : []
    if (!busy.length) return true
    if (Date.now() > deadline) {
      console.log(`  ${file}: 同组还有 ${busy.map((b) => b.id).join(',')} 在跑，等不到空档`)
      return false
    }
    console.log(`  ${file}: 同组 run ${busy.map((b) => `${b.id}(${b.status})`).join(',')} 在跑，等它结束再重跑`)
    await sleep(POLL)
  }
}

async function rerunFailed(runId) {
  const r = await api(`/actions/runs/${runId}/rerun-failed-jobs`, { method: 'POST' })
  if (r.status === 201 || r.status === 204 || r.ok) return null
  return `${r.status} ${r.text.slice(0, 300)}`
}

/**
 * 把一个 run 等到「有可信结论」为止：真失败原样返回，假红自动重跑再等。
 * 返回 { run, note, detail }，run 为空表示超时。
 */
async function settle(file, label, deadline) {
  let run = await ensureRun(file, label, deadline)
  let retries = 0
  const skipNote = SKIP_OS.length ? `已跳过 ${SKIP_OS.join('/')}` : ''
  for (;;) {
    if (!run) return { run: null, note: '超时未出结果' }
    if (run.conclusion === 'success') return { run, ok: true, note: skipNote }

    const jobs = await jobsOf(run.id)
    // 「要看的 job」= 过滤掉被跳过平台之后的那些。数量为 0 时不能算通过：
    // 那说明 dispatch 根本没建出 job（平台没调度），是没跑而不是绿了 ——
    // 交给下面的重跑逻辑处理。
    const wanted = jobs.filter((job) => !ignoredJob(job, SKIP_PATTERNS))
    const verdict = jobVerdict(jobs, { ignore: SKIP_PATTERNS })

    // 过滤掉被跳过的平台之后全绿 —— 这就是「通过」。整轮结论可能因为那个
    // 被取消的 macOS job 写成 `cancelled`，但那不是本轮交付要看的东西。
    if (verdict.state === 'ok' && wanted.length) {
      return { run, ok: true, note: skipNote || '被跳过的平台不影响判定' }
    }

    // 有失败步骤 = 代码问题。绝不重跑，重跑只会把同一个红再跑一遍。
    if (verdict.state === 'code') {
      return { run, note: '代码失败', detail: describeVerdict(verdict) }
    }

    // 提交已经作废时不该再给它出产物，也不该浪费一轮重跑。
    const head = await branchHead()
    if (head && head !== SHA) {
      return { run, note: `提交已被 ${head.slice(0, 8)} 顶掉，不重跑` }
    }

    if (retries >= MAX_INFRA_RERUN) {
      return { run, note: `平台连续 ${retries} 次没跑起来，放弃自动重跑`, detail: describeVerdict(verdict) }
    }

    retries += 1
    console.log(
      `  ${label}: 第 ${run.run_attempt || 1} 次非代码失败（${describeVerdict(verdict)}），自动重跑 ${retries}/${MAX_INFRA_RERUN}`,
    )

    if (!(await quiet(file, run.id, Date.now() + QUIET_TIMEOUT))) {
      return { run, note: '同组一直不空，无法安全重跑' }
    }
    const err = await rerunFailed(run.id)
    if (err) return { run, note: `重跑请求失败 ${err}` }

    run = await waitRun(file, label, deadline, 0, (run.run_attempt || 1) + 1)
  }
}

// ── 主流程 ──────────────────────────────────────────────────────────
async function main() {
  console.log(`仓库 ${REPO}  分支 ${REF}  提交 ${SHORT}`)
  if (SKIP_PATTERNS.length) {
    console.log(`跳过平台：${SKIP_OS.join('、')}（A2A_SKIP_OS；本 fork 只出 Windows 包）`)
  }
  console.log('─'.repeat(78))

  // 先探平台，再动手 —— 否则「推成功但一条流水线也不会跑」这件事
  // 要等到派发那一步才暴露，代码已经上了远端。
  await healthGate()

  // 再过本机编译闸 —— 默认关闭（--local-rust-check 才启用）。
  // 编译与构建的唯一权威是 GitHub Actions，本机不被工具链绑死。
  if (!NO_PUSH) await rustGate()

  if (!NO_PUSH) {
    if (!(await push())) {
      console.error('推送失败，链路中止。')
      process.exit(1)
    }
  }

  const gates = [
    { file: 'rust-tests.yml', label: '服务端测试' },
    { file: 'frontend-artifacts.yml', label: '前端产物同源' },
  ]

  // 两道闸门并行跑（GitHub 侧本来就互不依赖），这里同时等。
  const deadline = Date.now() + TIMEOUT
  const settled = await Promise.all(
    gates.map(async (g) => {
      const out = await settle(g.file, g.label, deadline)
      const tail = out.run ? `run ${out.run.id}` : '超时未出结果'
      console.log(`${MARK(out)} ${g.label.padEnd(12)} ${tail}${out.note ? `  ← ${out.note}` : ''}`)
      return { ...g, ...out }
    }),
  )

  const bad = settled.filter((s) => !s.ok)
  if (bad.length) {
    console.error('')
    for (const b of bad) {
      console.error(`闸门未通过：${b.label}（${b.run ? b.run.conclusion : '未出结果'}）${b.note ? ` — ${b.note}` : ''}`)
      if (b.detail) console.error(`  ${b.detail}`)
      if (b.run) console.error(`  ${b.run.html_url}`)
    }
    console.error('不出安装包。')
    process.exit(1)
  }

  if (NO_BUILD) {
    console.log('')
    console.log('闸门全绿（--no-build，跳过出包）。')
    return
  }

  console.log('')
  console.log('闸门全绿，开始出包 …（首次含依赖下载，约 10~20 分钟）')

  const built = await settle('build-windows.yml', 'Windows 安装包', Date.now() + TIMEOUT)
  const finalRun = built?.run
  console.log(`${MARK(built)} Windows 安装包 ${finalRun ? `run ${finalRun.id}` : '超时未出结果'}${built?.note ? `  ← ${built.note}` : ''}`)

  // `ok` 而不是 `conclusion === success`：被跳过的平台（默认 macOS）可能让整轮
  // 结论写成 cancelled，但需要的 job 全绿就等于通过（见 settle()）。
  if (!built?.ok) {
    if (built?.note) console.error(`  ${built.note}`)
    if (built?.detail) console.error(`  ${built.detail}`)
    if (finalRun) console.error(`  ${finalRun.html_url}`)
    process.exit(1)
  }

  const arts = await api(`/actions/runs/${finalRun.id}/artifacts`)
  const list = arts.json?.artifacts || []
  if (!list.length) {
    console.log('  （无 artifact）')
    return
  }
  console.log('')
  for (const a of list) {
    console.log(`artifact  ${a.name}  ${a.size_in_bytes} B  id=${a.id}  expired=${a.expired}`)
  }
  console.log('')
  console.log('下载：')
  console.log(`  https://github.com/${REPO}/actions/runs/${finalRun.id}`)
}

main().catch((e) => {
  console.error(e?.message || e)
  process.exit(1)
})
