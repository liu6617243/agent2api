#!/usr/bin/env node
// ─────────────────────────────────────────────────────────────────────
// precheck-actions.mjs —— 在动手之前回答「这个仓库的 CI 现在能不能跑」
//
// ── 为什么单独一条命令 ─────────────────────────────────────────────
// 「CI 没跑」有四种互不相通的原因，处置完全不同：
//
//   1. 触发面没写这个分支   → 改 workflow（本机缺 workflow scope，走 Contents API）
//   2. 凭据不够             → 换 token
//   3. 工作流被单独禁用     → PUT /actions/workflows/<id>/enable
//   4. **平台把 fork 熔断** → 只能维护者在网页点一次，API 全无解
//
// 2026-11-21 卡了整轮，只因为第 4 种，而它不在任何 REST 端点里能直接读到。
// 单独一条命令，十几秒给结论，避免在 1/2/3 上白折腾。
//
// ── 用法 ───────────────────────────────────────────────────────────
//   node scripts/precheck-actions.mjs          # 人看的结论
//   node scripts/precheck-actions.mjs --json   # 机器可读
//
// 退出码：0 可以跑；1 明确不可用（熔断 / 策略关闭 / 凭据）；2 判不出来。
// ─────────────────────────────────────────────────────────────────────

import fs from 'node:fs'
import { execFileSync } from 'node:child_process'
import { healthVerdict } from './lib/actions-health.mjs'

const REPO = process.env.A2A_REPO || 'liu6617243/agent2api'
const API = `https://api.github.com/repos/${REPO}`
const JSON_OUT = process.argv.includes('--json')

function token() {
  const env = process.env.GH_TOKEN || process.env.GITHUB_TOKEN
  if (env) return env.trim()
  const local = '.workbuddy/tmp/gh_token_value.txt'
  if (fs.existsSync(local)) {
    const t = fs.readFileSync(local, 'utf8').trim()
    if (t) return t
  }
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

async function api(path) {
  for (let attempt = 0; attempt < 3; attempt += 1) {
    if (attempt) await new Promise((r) => setTimeout(r, 700 * 2 ** (attempt - 1)))
    try {
      const r = await fetch(`${API}${path}`, {
        headers: {
          Authorization: `Bearer ${TOKEN}`,
          Accept: 'application/vnd.github+json',
          'X-GitHub-Api-Version': '2022-11-28',
          'User-Agent': 'agent2api-precheck-actions',
        },
        signal: AbortSignal.timeout(15000),
      })
      const text = await r.text()
      let json = null
      try {
        json = text ? JSON.parse(text) : null
      } catch {
        /* 无体 */
      }
      if (r.status === 429 || r.status >= 500) continue
      return { ok: r.ok, status: r.status, json, text }
    } catch {
      /* 网络抖动，重试 */
    }
  }
  return { ok: false, status: 0, json: null, text: '' }
}

async function main() {
  const { verdict, evidence, hint } = await healthVerdict({ api, repo: REPO, token: TOKEN })

  if (JSON_OUT) {
    console.log(JSON.stringify({ repo: REPO, verdict, ...evidence }, null, 2))
  } else {
    console.log(`仓库 ${REPO}`)
    console.log(`  策略层   enabled=${evidence.policyEnabled} allowed=${evidence.allowedActions}`)
    console.log(
      `  实时调度 ${evidence.liveScheduling === 'scheduling' ? '正常' : evidence.liveScheduling === 'not-scheduling' ? '平台没分 job' : '读不到'}` +
        (evidence.liveRunId ? `（最新 run ${evidence.liveRunId}，jobs=${evidence.liveJobs}）` : ''),
    )
    console.log(
      `  排队 run 检查 ${evidence.queuedChecked} 个，僵死 ${evidence.stalledRuns.length} 个` +
        (evidence.stalledRuns.length ? `（${evidence.stalledRuns.join(',')}）` : '') +
        (evidence.staleStalledRuns?.length
          ? `，陈旧僵尸 ${evidence.staleStalledRuns.length} 个（${evidence.staleStalledRuns.join(',')}，熔断期遗留，不作判据）`
          : ''),
    )
    console.log(`  页面横幅 ${evidence.banner}${evidence.bannerError ? `（读不到：${evidence.bannerError}）` : ''}`)
    console.log('')
    console.log(`结论：${verdict}`)
    if (hint) {
      console.log('')
      console.log(hint)
    }
  }

  if (verdict === 'ok') process.exit(0)
  if (verdict === 'unknown') process.exit(2)
  process.exit(1)
}

main().catch((e) => {
  console.error(e?.message || e)
  process.exit(2)
})
