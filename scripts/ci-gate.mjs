#!/usr/bin/env node
// ─────────────────────────────────────────────────────────────────────
// ci-gate.mjs —— 交付闸门：等上游两道 CI 跑完再放行
//
// ── 它解决什么 ─────────────────────────────────────────────────────
// 本 fork 的交付链条有三段：服务端测试（rust-tests）→ 前端产物同源
// （frontend-artifacts）→ Windows 安装包（build-windows）。
//
// 过去这段链条是**断的**：上游工作流的 push 触发面写的是 `[main, beta]`，
// 而本 fork 的默认分支是 `main-patched` —— 于是 push 从来不会自动触发
// 上游闸门，每一段都靠人手动 `workflow_dispatch`。结论就是：
// 「测试忘了跑」「安装包忘了出」这类事故反复发生，而且每次都要靠用户提醒。
//
// 现在触发面补齐了，链条自动串起来；这个脚本负责链条中间那一段 ——
// **在下游出包之前，确认上游两道闸门对这个提交都已经变绿**。
// 它不是装饰：没有它，build-windows 会在测试还在跑的时候就开始编译，
// 装出一个「测试没过但包已经出了」的安装包。
//
// ── 判定规则 ───────────────────────────────────────────────────────
// 每道闸门先判「是否适用」，再判「结论」：
//
//   是否适用 —— 取该提交实际改动的文件，与该闸门工作流的 paths 过滤比对。
//              一条都不匹配 = 这道闸门对这个提交不适用（上游不会跑它），
//              立刻判 not-applicable，不必干等。
//              有匹配 = 上游一定会跑它，必须等到结论。
//
//   结论     passed         run 结论 success
//            failed         run 结论 failure / timed_out
//            superseded     run 结论 cancelled —— 被更新的提交顶掉，不算失败
//            not-applicable 无匹配路径，或 run skipped
//            pending        run 还没出现，或还在跑
//
// pending 会一直等（有上限）；failed 让脚本非零退出；not-applicable 与
// superseded 都放行 —— 它们表示「这道闸对这个提交不适用」，而不是
// 「这道闸没把住」。
//
// ── 用法 ───────────────────────────────────────────────────────────
//   node scripts/ci-gate.mjs
//
// 环境变量（CI 里由 GitHub 提供）：
//   GITHUB_TOKEN       必需 —— 读 Actions API
//   GITHUB_REPOSITORY  必需 —— owner/repo
//   GITHUB_SHA         必需 —— 要判定的提交
// 可选：
//   CI_GATE_TIMEOUT    等待上限秒数，默认 3600
//   CI_GATE_POLL       轮询间隔毫秒，默认 20000
//
// ── 它不做什么 ─────────────────────────────────────────────────────
// 不下载任何产物，不改任何文件，只读 API 并输出判定结果。
// 产物一律不落盘（仓库纪律）。

const TOKEN = process.env.GITHUB_TOKEN || process.env.GH_TOKEN || ''
const REPO = process.env.GITHUB_REPOSITORY || ''
const SHA = process.env.GITHUB_SHA || ''

if (!TOKEN) { console.error('缺少 GITHUB_TOKEN'); process.exit(2) }
if (!REPO) { console.error('缺少 GITHUB_REPOSITORY'); process.exit(2) }
if (!SHA) { console.error('缺少 GITHUB_SHA'); process.exit(2) }

const TIMEOUT = Number(process.env.CI_GATE_TIMEOUT || 3600)
const POLL = Number(process.env.CI_GATE_POLL || 20000)

const API = `https://api.github.com/repos/${REPO}`
const H = {
  Authorization: `Bearer ${TOKEN}`,
  Accept: 'application/vnd.github+json',
  'X-GitHub-Api-Version': '2022-11-28',
  'User-Agent': 'agent2api-ci-gate',
}

// ── 上游闸门 ────────────────────────────────────────────────────────
// file      —— workflow 文件名。API 端点认的是**带扩展名的文件名**，
//               写 `rust-tests` 会 404。
// paths     —— 该工作流 push 触发时的 paths 过滤。空数组 = 不做过滤，
//               对任何提交都适用。
const GATES = [
  {
    file: 'rust-tests.yml',
    label: '服务端测试',
    paths: [
      'desktop-tauri/src-tauri/**',
      'scripts/cargo-test.mjs',
      '.github/workflows/rust-tests.yml',
      'package.json',
    ],
  },
  {
    file: 'frontend-artifacts.yml',
    label: '前端产物同源',
    paths: [], // 该工作流对任何提交都跑
  },
]

const sleep = (ms) => new Promise((r) => setTimeout(r, ms))

async function json(url) {
  const r = await fetch(url, { headers: H })
  if (!r.ok) {
    const body = await r.text().catch(() => '')
    throw new Error(`${r.status} ${url}\n${body.slice(0, 300)}`)
  }
  return r.json()
}

// ── 这个提交改了哪些文件 ────────────────────────────────────────────
let changedFiles = null
async function loadChangedFiles() {
  if (changedFiles) return changedFiles
  const data = await json(`${API}/commits/${SHA}`)
  changedFiles = (data.files ?? []).map((f) => f.filename)
  return changedFiles
}

// ── paths 过滤匹配（对齐 GitHub 的 glob 语义）─────────────────────
// 只支持工作流里实际用到的三种形态：
//   目录通配 `a/**`   —— a/ 下的所有路径
//   精确名   `a/b.rs` —— 根相对精确匹配
//   前缀通配 `a/**/b` —— 目录层数不限
function matchPath(pattern, file) {
  if (pattern.endsWith('/**')) {
    const prefix = pattern.slice(0, -3)
    return file === prefix || file.startsWith(prefix + '/')
  }
  if (pattern.includes('/**/')) {
    const [head, tail] = pattern.split('/**/')
    return file.startsWith(head + '/') && file.endsWith('/' + tail)
  }
  return file === pattern
}

function gateApplies(gate, files) {
  if (!gate.paths.length) return true
  return files.some((f) => gate.paths.some((p) => matchPath(p, f)))
}

// ── 取该工作流在这个提交上的 run ───────────────────────────────────
async function listRuns(file) {
  const data = await json(`${API}/actions/workflows/${file}/runs?head_sha=${SHA}&per_page=20`)
  const list = data.workflow_runs ?? []
  // 同一个提交可能有多次 run（重跑 / 并发取消后的残留），取最新一次为准。
  list.sort((a, b) =>
    String(a.created_at).localeCompare(String(b.created_at)) ||
    (a.run_attempt ?? 0) - (b.run_attempt ?? 0))
  return list
}

function classify(runs) {
  if (!runs.length) return { state: 'pending', why: 'run 尚未出现' }
  const last = runs[runs.length - 1]
  if (last.status !== 'completed') return { state: 'pending', why: `run 进行中（${last.status}）` }
  switch (last.conclusion) {
    case 'success': return { state: 'passed', why: `run ${last.id} success` }
    case 'cancelled': return { state: 'superseded', why: `run ${last.id} 被更新的提交顶掉` }
    case 'skipped': return { state: 'not-applicable', why: `run ${last.id} skipped` }
    case 'failure':
    case 'timed_out':
      return { state: 'failed', why: `run ${last.id} ${last.conclusion}` }
    default:
      return { state: 'pending', why: `run ${last.id} 结论 ${last.conclusion}` }
  }
}

// ── 一道闸门「被顶掉」时，整条链路的结论 ──────────────────────────
// 上游有并发取消（rust-tests 的 concurrency 是 cancel-in-progress: true），
// 提交 A 被提交 B 顶掉时，A 的 run 结论是 cancelled，而 A 的其它闸门
// 可能压根没跑过 —— 继续等会等到超时。
// 关键判断：**被更新的提交顶掉 = 这个提交已经作废**。
// 作废的提交不该再出包，所以整条链路直接判 superseded，放行并说明。
// 它不是「闸门没把住」，而是「这个提交本来就不该有产物」。
function chainVerdict(results) {
  const superseded = results.find((r) => r.state === 'superseded')
  if (superseded) {
    return {
      verdict: 'superseded',
      detail: `${superseded.label} 的 run 被更新的提交顶掉，本提交已作废，不该再产出安装包`,
    }
  }
  return null
}

// ── 主循环 ──────────────────────────────────────────────────────────
const started = Date.now()
const deadline = started + TIMEOUT * 1000
let lastLine = ''

for (;;) {
  const files = await loadChangedFiles()
  const results = []
  for (const g of GATES) {
    if (!gateApplies(g, files)) {
      results.push({ ...g, state: 'not-applicable', why: '该提交未触及本闸门的 paths' })
      continue
    }
    const runs = await listRuns(g.file)
    results.push({ ...g, ...classify(runs) })
  }

  const line = results.map((r) => `${r.file.replace('.yml', '')}=${r.state}`).join(' ')
  if (line !== lastLine) {
    console.log(`[ci-gate] ${Math.floor((Date.now() - started) / 1000)}s  ${line}`)
    lastLine = line
  }

  const bad = results.filter((r) => r.state === 'failed')
  if (bad.length) {
    console.error('\n[ci-gate] 闸门未通过：')
    for (const b of bad) console.error(`  - ${b.label}（${b.file}）：${b.why}`)
    console.error('\n[ci-gate] 不放行 —— 下游不会拿到这一版的产物。')
    process.exit(1)
  }

  // 提交已被更新的提交顶掉 → 本提交作废 → 不该再出包，直接收工。
  const superseded = chainVerdict(results)
  if (superseded) {
    console.log('\n[ci-gate] 全部闸门已决：')
    for (const r of results) console.log(`  ${r.state.padEnd(16)} ${r.label}（${r.file}）：${r.why}`)
    console.log(`\n[ci-gate] 放行 —— ${superseded.detail}。`)
    process.exit(0)
  }

  const pending = results.filter((r) => r.state === 'pending')
  if (!pending.length) {
    console.log('\n[ci-gate] 全部闸门已决：')
    for (const r of results) console.log(`  ${r.state.padEnd(16)} ${r.label}（${r.file}）：${r.why}`)
    console.log('\n[ci-gate] 放行。')
    process.exit(0)
  }

  if (Date.now() > deadline) {
    console.error(`\n[ci-gate] 等待超时（${TIMEOUT}s），仍未决：`)
    for (const p of pending) console.error(`  - ${p.label}：${p.why}`)
    process.exit(1)
  }

  await sleep(POLL)
}
