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
//            failed         run 结论 failure / timed_out，且 job 里有失败步骤
//            infra          run 结论 failure / timed_out，但没有任何失败步骤
//                           （某个平台 job 一步都没执行就被平台取消）——
//                           放行，但输出里必须写明，不能悄悄吞掉
//            superseded     run 结论 cancelled —— 被更新的提交顶掉，不算失败
//            not-applicable 无匹配路径，或 run skipped
//            pending        run 还没出现，或还在跑
//
// pending 会一直等（有上限）；failed 让脚本非零退出；infra / not-applicable /
// superseded 都放行 —— 它们表示「这道闸对这个提交不适用 / 红不是代码的红」，
// 而不是「这道闸没把住」。
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

import { jobVerdict, describeVerdict, ignoredJob } from './lib/run-verdict.mjs'

const TOKEN = process.env.GITHUB_TOKEN || process.env.GH_TOKEN || ''
const REPO = process.env.GITHUB_REPOSITORY || ''
const SHA = process.env.GITHUB_SHA || ''

if (!TOKEN) { console.error('缺少 GITHUB_TOKEN'); process.exit(2) }
if (!REPO) { console.error('缺少 GITHUB_REPOSITORY'); process.exit(2) }
if (!SHA) { console.error('缺少 GITHUB_SHA'); process.exit(2) }

const TIMEOUT = Number(process.env.CI_GATE_TIMEOUT || 3600)
const POLL = Number(process.env.CI_GATE_POLL || 20000)

// ── 要跳过哪些平台 ──────────────────────────────────────────────────
// 本 fork 只交付 Windows（§62），macOS job 没有产物去处，却常在排队半小时
// 以上、或分到 runner 后 0 个步骤被判 cancelled —— 于是 run 永远不了结，
// 出包闸门永远不派发。
//
// `deliver.mjs` 早就认这个约定（`A2A_SKIP_OS`，默认 `macos`），但本脚本
// 原先没读，于是同一件事在两个地方有两种口径：交付脚本放行了，
// 闸门脚本还在按「macOS 也要绿」判。两边必须一致，否则就是「谁先跑谁说了算」。
//
// 用法：`A2A_SKIP_OS= node scripts/ci-gate.mjs`（空串 = 不跳过，三平台都判）。
const SKIP_OS = (process.env.A2A_SKIP_OS ?? 'macos')
  .split(',')
  .map((s) => s.trim())
  .filter(Boolean)
// job 名形如「服务端与插件联调 (macos-latest)」，用 `(macos` 当匹配串就够，
// 不必猜 runner 的具体版本号（macos-14 / macos-15 都命中）。
const SKIP_PATTERNS = SKIP_OS.map((os) => `(${os}`)

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
  // head_sha 必须完整。GitHub 对短 SHA 不报错，而是返回空列表；CI 里 GITHUB_SHA
  // 本来就是 40 位，本地手工复用时也要传完整对象名。
  if (!/^[0-9a-f]{40}$/i.test(SHA)) {
    console.error('GITHUB_SHA 必须是完整 40 位提交 SHA（短 SHA 会被 Actions API 当成无结果）')
    process.exit(2)
  }
  const data = await json(`${API}/actions/workflows/${file}/runs?head_sha=${SHA}&per_page=20`)
  const list = data.workflow_runs ?? []
  // 同一个提交可能有多次 run（重跑 / 并发取消后的残留），取最新一次为准。
  // rerun-failed-jobs 复用 run id 与 created_at，只增加 run_attempt；
  // 先比创建时间会让闸门在重跑期间仍读上一轮的 failure / cancelled。
  list.sort((a, b) =>
    (a.run_attempt ?? 1) - (b.run_attempt ?? 1) ||
    String(a.created_at).localeCompare(String(b.created_at)))
  return list
}

// ── run 结论 → 闸门状态 ────────────────────────────────────────────
// 光看 run.conclusion 会把「平台没把 job 跑起来」当成代码失败：
// 这个仓库反复出现过某个平台 job 分到 runner 后一步都没执行（steps 为空），
// 15 分钟被平台判 cancelled，run 结论于是 failure。真有断言失败时一定会留下
// 一个 conclusion=failure 的 step。所以 failure / timed_out 还要再下钻到 job
// 级别，没有失败步骤的一律按 infra 处理（放行，但必须大声说明）。
async function classify(runs) {
  if (!runs.length) return { state: 'pending', why: 'run 尚未出现' }
  const last = runs[runs.length - 1]
  if (last.status !== 'completed') return { state: 'pending', why: `run 进行中（${last.status}）` }
  switch (last.conclusion) {
    case 'success': return { state: 'passed', why: `run ${last.id} success` }
    case 'cancelled': {
      // 「cancelled」有两种含义，结论完全相反：
      //   ① 这个提交被更新的提交顶掉（concurrency cancel-in-progress）→ 提交作废，不该出包；
      //   ② run 里的某个平台 job 被人/被脚本取消，其余 job 全绿 → 提交没问题，只是那个平台没结论。
      // 本 fork 只交付 Windows，`deliver.mjs` 会主动取消还在排队的 macOS job（§62），
      // 那种情况下整个 run 的结论就是 cancelled —— 若一律判 superseded，
      // 闸门会拦住一个完全健康的提交。
      // 判据：下钻到 job 级，看「需要判定的 job」（排除跳过平台）是否已经全绿。
      const jobsData = await json(`${API}/actions/runs/${last.id}/jobs?per_page=100`)
      const verdict = jobVerdict(jobsData.jobs ?? [], { ignore: SKIP_PATTERNS })
      if (verdict.state === 'ok') {
        const skipped = (jobsData.jobs ?? []).filter((job) => ignoredJob(job, SKIP_PATTERNS))
        return {
          state: 'passed',
          why: `run ${last.id} cancelled，但需要判定的 job 全绿（被取消的是 ${
            skipped.map((j) => j.name).join('、') || '某个平台'
          }）`,
        }
      }
      if (verdict.state === 'code') {
        return { state: 'failed', why: `run ${last.id} cancelled —— ${describeVerdict(verdict)}` }
      }
      return { state: 'superseded', why: `run ${last.id} 被更新的提交顶掉` }
    }
    case 'skipped': return { state: 'not-applicable', why: `run ${last.id} skipped` }
    case 'failure':
    case 'timed_out': {
      const jobsData = await json(`${API}/actions/runs/${last.id}/jobs?per_page=100`)
      const verdict = jobVerdict(jobsData.jobs ?? [], { ignore: SKIP_PATTERNS })
      if (verdict.state === 'code') {
        return { state: 'failed', why: `run ${last.id} ${last.conclusion} —— ${describeVerdict(verdict)}` }
      }
      return { state: 'infra', why: `run ${last.id} ${last.conclusion} 但无失败步骤（${describeVerdict(verdict)}）` }
    }
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
    const infra = results.filter((r) => r.state === 'infra')
    if (infra.length) {
      console.log('\n[ci-gate] 注意：以下闸门是「平台没把 job 跑起来」，不是代码失败 ——')
      for (const r of infra) console.log(`  - ${r.label}：${r.why}`)
      console.log('  想拿到干净的测试结论，请重跑这几个 job（rerun-failed-jobs），不要当成已通过。')
    }
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
