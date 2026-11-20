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
import { execFileSync } from 'node:child_process'
import { jobVerdict, describeVerdict } from './lib/run-verdict.mjs'

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

// ── 1. 推送 ─────────────────────────────────────────────────────────
function sha() {
  const given = val('--sha', '')
  if (given) return execFileSync('git', ['rev-parse', given], { encoding: 'utf8' }).trim()
  return execFileSync('git', ['rev-parse', 'HEAD'], { encoding: 'utf8' }).trim()
}

const SHA = sha()
const SHORT = SHA.slice(0, 8)

function push() {
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
  return false
}

// ── 2. 派发 ─────────────────────────────────────────────────────────
async function dispatch(file) {
  const r = await api(`/actions/workflows/${file}/dispatches`, {
    method: 'POST',
    body: JSON.stringify({ ref: REF }),
  })
  if (r.status === 204 || r.ok) return null
  return `派发 ${file} 失败 ${r.status}: ${r.text.slice(0, 300)}`
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
  while (Date.now() < deadline) {
    const run = await findRun(file, after)
    // attempt 变了（有人在 GitHub 上点了 rerun）要当成新的一次重新打印 ——
    // 否则日志里只有一行「run xxx」，用户看不出它已经被重跑过。
    if (run && (run.id !== seen || run.run_attempt !== lastAttempt)) {
      seen = run.id
      lastAttempt = run.run_attempt
      console.log(`  ${label}: run ${run.id}（${run.status}，第 ${run.run_attempt || 1} 次）`)
    }
    // 刚点完 rerun 时 API 还会短暂返回上一轮的 completed 结论；必须等
    // attempt 涨到目标值，否则会立刻拿旧结论下判断。
    if (run && run.status === 'completed' && (run.run_attempt || 1) >= minAttempt) return run
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
  const err = await dispatch(file)
  if (err) {
    console.error(err)
    process.exit(1)
  }
  return waitRun(file, label, deadline, base)
}

const MARK = (s) => {
  if (!s) return 'TIMEOUT'
  if (s === 'success') return 'OK  '
  if (s === 'skipped') return 'SKIP'
  return 'FAIL'
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
  for (;;) {
    if (!run) return { run: null, note: '超时未出结果' }
    if (run.conclusion === 'success') return { run }

    const verdict = jobVerdict(await jobsOf(run.id))

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
  console.log('─'.repeat(78))

  if (!NO_PUSH) {
    if (!push()) {
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
      console.log(`${MARK(out.run?.conclusion)} ${g.label.padEnd(12)} ${tail}${out.note ? `  ← ${out.note}` : ''}`)
      return { ...g, ...out }
    }),
  )

  const bad = settled.filter((s) => s.run?.conclusion !== 'success')
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
  console.log(`${MARK(finalRun?.conclusion)} Windows 安装包 ${finalRun ? `run ${finalRun.id}` : '超时未出结果'}${built?.note ? `  ← ${built.note}` : ''}`)

  if (!finalRun || finalRun.conclusion !== 'success') {
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
