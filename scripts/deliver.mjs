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
// ── 用法 ─────────────────────────────────────────────────────────
//   node scripts/deliver.mjs                # 推 HEAD 并跑完整链路
//   node scripts/deliver.mjs --no-push      # 已推过，只跑闸门与出包
//   node scripts/deliver.mjs --no-build     # 只跑测试与产物同源，不出包
//   node scripts/deliver.mjs --sha <sha>    # 指定提交
//   node scripts/deliver.mjs --timeout 5400 # 单段等待上限（秒，默认 5400）

import fs from 'node:fs'
import { execFileSync } from 'node:child_process'

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

async function api(path, init) {
  const r = await fetch(`${API}${path}`, { ...init, headers: H })
  const text = await r.text()
  let json = null
  try {
    json = text ? JSON.parse(text) : null
  } catch {
    /* 204 无体 */
  }
  return { status: r.status, ok: r.ok, json, text }
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
const REUSE_WINDOW = 6 * 3600 * 1000

async function findRun(file, after = 0) {
  const r = await api(`/actions/workflows/${file}/runs?branch=${REF}&per_page=30`)
  if (!r.ok) return null
  const runs = (r.json?.workflow_runs || []).filter(
    (x) => x.head_sha === SHA && new Date(x.created_at).getTime() >= after,
  )
  runs.sort((a, b) => new Date(b.created_at) - new Date(a.created_at))
  return runs[0] || null
}

async function waitRun(file, label, deadline, after = 0) {
  let seen = null
  while (Date.now() < deadline) {
    const run = await findRun(file, after)
    if (run && run.id !== seen) {
      seen = run.id
      console.log(`  ${label}: run ${run.id}（${run.status}）`)
    }
    if (run && run.status === 'completed') return run
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
      console.log(`  ${label}: 已有 run ${existing.id}（${existing.conclusion}），重新派发`)
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
      const run = await ensureRun(g.file, g.label, deadline)
      console.log(`${MARK(run?.conclusion)} ${g.label.padEnd(12)} ${run ? `run ${run.id}` : '超时未出结果'}`)
      return { ...g, run }
    }),
  )

  const bad = settled.filter((s) => s.run?.conclusion !== 'success')
  if (bad.length) {
    console.error('')
    for (const b of bad) {
      console.error(`闸门未通过：${b.label}（${b.run ? b.run.conclusion : '未出结果'}）`)
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

  const run = await ensureRun('build-windows.yml', 'Windows 安装包', Date.now() + TIMEOUT)
  console.log(`${MARK(run?.conclusion)} Windows 安装包 ${run ? `run ${run.id}` : '超时未出结果'}`)

  if (!run || run.conclusion !== 'success') {
    if (run) console.error(`  ${run.html_url}`)
    process.exit(1)
  }

  const arts = await api(`/actions/runs/${run.id}/artifacts`)
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
  console.log(`  https://github.com/${REPO}/actions/runs/${run.id}`)
}

main().catch((e) => {
  console.error(e?.message || e)
  process.exit(1)
})
