#!/usr/bin/env node
// ─────────────────────────────────────────────────────────────────────
// ci-watch.mjs —— 本 fork 的 CI / 出包状态一站式查询与触发
//
// ── 为什么需要它 ───────────────────────────────────────────────────
// 这个 fork 的默认分支是 `main-patched`。上游工作流的 push 触发面写的是
// `[main, beta]` —— 在这个 fork 里那两个分支不是干活的地方，所以历史上
// 每一个 run 都是手动 `workflow_dispatch`：没有人、也没有机器记得
// 「CI 绿了之后还要出 Windows 安装包」。
// 这个脚本把「查状态 / 触发 / 等结果 / 取 artifact id」收敛成一条命令，
// 让出包这件事不再依赖记忆。
//
// ── 用法 ───────────────────────────────────────────────────────────
//   node scripts/ci-watch.mjs                     # 查当前 HEAD 的三件套状态
//   node scripts/ci-watch.mjs --sha <sha>         # 查指定提交
//   node scripts/ci-watch.mjs --wait              # 等到全部跑完
//   node scripts/ci-watch.mjs --wait --timeout 40 # 最长等 40 分钟
//   node scripts/ci-watch.mjs --artifact          # 额外打印 artifact 明细
//   node scripts/ci-watch.mjs --gate              # 交付自检：任一未绿则 exit 1
//   node scripts/ci-watch.mjs --dispatch build-windows   # 手动补一次出包
//
// ── 令牌来源（按优先级）───────────────────────────────────────────
//   1. 环境变量 GH_TOKEN / GITHUB_TOKEN
//   2. 本机 gh 登录态（gh auth token）
//   3. 本机便利路径 .workbuddy/tmp/gh_token_value.txt（该目录被 .gitignore 忽略）
//
// 只读 GitHub API，不会下载任何产物（产物一律不落盘，仓库纪律如此）。

import fs from 'node:fs'
import path from 'node:path'
import { execFileSync } from 'node:child_process'
import { jobVerdict, describeVerdict } from './lib/run-verdict.mjs'

const REPO = process.env.A2A_REPO || 'liu6617243/agent2api'
const API = `https://api.github.com/repos/${REPO}`

// 交付三件套：服务端测试 / 前端产物同源 / Windows 安装包
// file 是 API 认工作流的凭据 —— 必须带 .yml 扩展名（写 `rust-tests` 会 404）。
const TRACKED = [
  { file: 'rust-tests.yml', label: '服务端测试' },
  { file: 'frontend-artifacts.yml', label: '前端产物同源' },
  { file: 'build-windows.yml', label: 'Windows 安装包' },
]

const argv = process.argv.slice(2)
const has = (f) => argv.includes(f)
const val = (f, d) => {
  const i = argv.indexOf(f)
  return i >= 0 && argv[i + 1] ? argv[i + 1] : d
}

// ── 令牌 ────────────────────────────────────────────────────────────
function token() {
  const env = process.env.GH_TOKEN || process.env.GITHUB_TOKEN
  if (env) return env.trim()
  try {
    const t = execFileSync('gh', ['auth', 'token'], { encoding: 'utf8', stdio: ['ignore', 'pipe', 'ignore'] })
    if (t.trim()) return t.trim()
  } catch { /* gh 不在 PATH 上是常态，继续走文件回退 */ }
  const local = path.resolve('.workbuddy/tmp/gh_token_value.txt')
  if (fs.existsSync(local)) return fs.readFileSync(local, 'utf8').trim()
  throw new Error('没有可用令牌：设 GH_TOKEN，或登录 gh，或放 .workbuddy/tmp/gh_token_value.txt')
}

const TOKEN = token()
const H = {
  Authorization: `Bearer ${TOKEN}`,
  Accept: 'application/vnd.github+json',
  'X-GitHub-Api-Version': '2022-11-28',
  'User-Agent': 'agent2api-ci-watch',
}

async function api(pathname) {
  const r = await fetch(`${API}${pathname}`, { headers: H })
  if (!r.ok) {
    const body = await r.text().catch(() => '')
    throw new Error(`${r.status} ${pathname}\n${body.slice(0, 400)}`)
  }
  return r.json()
}

const sleep = (ms) => new Promise((r) => setTimeout(r, ms))

// ── 当前提交 ────────────────────────────────────────────────────────
function headSha() {
  const s = val('--sha', '')
  // GitHub 的 head_sha 过滤器只认完整 40 位 SHA：短 SHA 会安静地返回
  // total_count=0，看起来像「这个提交没跑过」。无论用户传短 SHA 还是分支名，
  // 都先交给本地 git 解析成完整对象名，不能把 CLI 输入直接拼进 URL。
  if (s) return execFileSync('git', ['rev-parse', `${s}^{commit}`], { encoding: 'utf8' }).trim()
  return execFileSync('git', ['rev-parse', 'HEAD'], { encoding: 'utf8' }).trim()
}

// ── 取某个工作流在指定提交上的 run ─────────────────────────────────
async function runsFor(sha) {
  const out = {}
  for (const wf of TRACKED) {
    const data = await api(`/actions/workflows/${wf.file}/runs?head_sha=${sha}&per_page=20`)
    const list = data.workflow_runs ?? []
    // 同一工作流可能对同一提交跑多次（重跑 / workflow_run 链），
    // 取最新的一次作为当前态；全部记录留给 --artifact 汇总。
    // rerun-failed-jobs 复用 run id 与 created_at，只增加 run_attempt，
    // 所以「这次尝试」的优先级必须高于记录的创建时间。
    list.sort((a, b) =>
      (a.run_attempt ?? 1) - (b.run_attempt ?? 1) ||
      String(a.created_at).localeCompare(String(b.created_at)))
    out[wf.file] = list
  }
  return out
}

function statusOf(list) {
  if (!list.length) return { state: 'missing', conclusion: '-' }
  const last = list[list.length - 1]
  return { state: last.status, conclusion: last.conclusion ?? '-' }
}

const MARK = { success: 'OK  ', failure: 'FAIL', cancelled: 'CANC', skipped: 'SKIP', missing: 'MISS' }

function printState(sha, state) {
  console.log(`\n提交 ${sha.slice(0, 8)}  三件套状态`)
  console.log('─'.repeat(78))
  for (const wf of TRACKED) {
    const { state: st, conclusion } = statusOf(state[wf.file])
    const mark = MARK[conclusion] ?? (st === 'in_progress' ? '....' : '????')
    const tail = st === 'missing' ? '（未触发）' : st === 'in_progress' ? '（进行中）' : ''
    console.log(`${mark} ${wf.label.padEnd(12, '　')} ${wf.file.padEnd(24)} ${String(st).padEnd(12)} ${String(conclusion).padEnd(10)} ${tail}`)
  }
  console.log('─'.repeat(78))
}

// ── 主流程 ──────────────────────────────────────────────────────────
const sha = headSha()
let state = await runsFor(sha)

if (has('--dispatch')) {
  const want = val('--dispatch', 'build-windows.yml')
  const wf = TRACKED.find((w) => w.file === want || w.file === `${want}.yml`)
  if (!wf) throw new Error(`不认识的 workflow: ${want}`)
  const ref = execFileSync('git', ['rev-parse', '--abbrev-ref', 'HEAD'], { encoding: 'utf8' }).trim()
  const r = await fetch(`${API}/actions/workflows/${wf.file}/dispatches`, {
    method: 'POST',
    headers: { ...H, 'Content-Type': 'application/json' },
    body: JSON.stringify({ ref }),
  })
  if (r.status !== 204) throw new Error(`触发失败 ${r.status}: ${await r.text()}`)
  console.log(`已触发 ${wf.file}（ref=${ref}），等待 run 出现…`)
  for (let i = 0; i < 20; i++) {
    await sleep(3000)
    state = await runsFor(sha)
    if (state[wf.file].length) break
  }
}

if (has('--wait')) {
  const timeoutMin = Number(val('--timeout', '60'))
  const deadline = Date.now() + timeoutMin * 60_000
  let lastLine = ''
  while (Date.now() < deadline) {
    state = await runsFor(sha)
    const line = TRACKED.map((w) => {
      const s = statusOf(state[w.file])
      return `${w.file.replace('.yml', '')}=${s.conclusion === '-' ? s.state : s.conclusion}`
    }).join(' ')
    if (line !== lastLine) { console.log(`  … ${line}`); lastLine = line }
    const allDone = TRACKED.every((w) => {
      const s = statusOf(state[w.file])
      return s.state === 'completed' || s.state === 'missing'
    })
    if (allDone) break
    await sleep(20000)
  }
}

printState(sha, state)

// ── artifact 明细 ───────────────────────────────────────────────────
if (has('--artifact') || has('--gate')) {
  const wins = state['build-windows.yml']
  const good = wins.filter((r) => r.status === 'completed' && r.conclusion === 'success')
  if (!good.length) {
    console.log('artifact：无成功的 build-windows run')
  } else {
    for (const run of good) {
      const arts = await api(`/actions/runs/${run.id}/artifacts`)
      for (const a of arts.artifacts ?? []) {
        console.log(`artifact  ${a.name}  ${a.size_in_bytes} B  id=${a.id}  run=${run.id}  expired=${a.expired}`)
      }
    }
  }
}

// ── 交付自检 ────────────────────────────────────────────────────────
// 判定口径与 deliver.mjs 必须一致：run.conclusion 不是 success 时再下钻到
// job 级别。某个平台 job 一步都没跑就被平台取消，不是代码的红；把它当成
// 失败会让人反复手动重跑，把它当成通过则会丢测试覆盖 —— 所以这里判成
// 「假红」并明确要求重跑，而不是简单二分。
if (has('--gate')) {
  const bad = []
  const infra = []
  const lastOf = (file) => {
    const list = state[file]
    return list.length ? list[list.length - 1] : null
  }
  for (const wf of TRACKED) {
    const { state: st, conclusion } = statusOf(state[wf.file])
    if (st === 'missing') { bad.push(`${wf.file} 未触发`); continue }
    if (st !== 'completed') { bad.push(`${wf.file} 未完成（${st}）`); continue }
    if (conclusion === 'success') continue
    const run = lastOf(wf.file)
    const jobsData = run ? await api(`/actions/runs/${run.id}/jobs?per_page=100`) : null
    const verdict = jobVerdict(jobsData?.jobs ?? [])
    if (verdict.state === 'code') bad.push(`${wf.file} 结论为 ${conclusion} —— ${describeVerdict(verdict)}`)
    else infra.push(`${wf.file} run ${run?.id} 结论 ${conclusion}，但无失败步骤（${describeVerdict(verdict)}）`)
  }
  if (infra.length) {
    console.log('\n假红（平台没把 job 跑起来，不是代码失败）：')
    for (const i of infra) console.log(`  - ${i}`)
    console.log('  retry：node scripts/deliver.mjs --sha <完整 SHA> --no-push 会自动重跑')
  }
  if (bad.length) {
    console.error(`\n交付自检未通过：\n  - ${bad.join('\n  - ')}`)
    process.exit(1)
  }
  if (infra.length) {
    console.error('\n交付自检未通过：存在假红，测试覆盖不全，先重跑再出包。')
    process.exit(1)
  }
  console.log('\n交付自检通过：测试绿 + 产物同源 + 安装包已出。')
}
