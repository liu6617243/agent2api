// ─────────────────────────────────────────────────────────────────────
// actions-health.mjs —— 判「这个仓库的 Actions 到底能不能跑」
//
// ── 为什么需要它 ──────────────────────────────────────────────────
// 2026-11-21 卡了一整轮：`workflow_dispatch` 全部返回 422
//
//   {"message":"Actions has been disabled for this repository."}
//
// 而所有**能查到的**信号都显示「正常」：
//
//   GET /repos/{r}/actions/permissions           → enabled: true, allowed_actions: all
//   GET /repos/{r}/actions/workflows/rust-tests.yml → state: active
//   PUT /repos/{r}/actions/permissions           → 204（写成功，仍然 422）
//   PUT /repos/{r}/actions/workflows/<id>/enable → 204（仍然 422）
//   POST /repos/{r}/actions/runs/<id>/rerun      → 201（排队了，但 jobs 永远是 0 条）
//
// 真因是**第四层开关**：GitHub 对 fork 的 Actions 使用量做了熔断，页面上挂一条
//
//   Workflows are not currently being run on this fork due to the scale of
//   GitHub Actions usage. A repository maintainer can choose to re-enable these.
//
// 它没有 REST 端点，关掉之后 `push` 也照样不触发（实测推了 `v*` tag，20 秒内
// 0 个新 run）。所以「CI 没跑」有四种互不相通的原因，处置方式各不一样：
//
//   ok              正常
//   disabled-fork   平台把 fork 熔断   → 维护者网页点一次，API 全无解
//   disabled-policy 仓库策略关掉了     → PUT /actions/permissions
//   forbidden       凭据不够           → 换 token
//   unknown         判不出来           → 不是「正常」，别当成绿灯
//
// ── 判据为什么要两条腿 ────────────────────────────────────────────
// 横幅只能从页面 HTML 里读，而**本机网络访问 github.com 的 HTML 页面基本不通**
// （api.github.com 正常，git 走镜像）—— 只靠横幅会一律 unknown。
// 所以主判据改成纯 API 的**僵死队列**：熔断生效时，已被启用的旧 run（rerun
// 出来的那个）会一直停在 queued，`/jobs` 永远 `total_count: 0`，因为平台
// 根本没在调度。这条信号只依赖 api.github.com，稳定可读。
//
// 横幅退为旁证：能读到就加分，读不到不影响结论。
// ─────────────────────────────────────────────────────────────────────

const BANNER = /Workflows are not currently being run on this fork/i

/** 从 Actions 页面 HTML 读 fork 熔断横幅。读不到就是 unknown，不是「正常」。 */
export function bannerState(html) {
  if (!html) return 'unknown'
  return BANNER.test(html) ? 'disabled' : 'enabled'
}

/**
 * 抓 Actions 页面（旁证，可能失败）。
 * 本机到 github.com 的 HTML 通道经常不通，所以只试 2 次、短超时。
 */
export async function probeBanner({ repo, token, tries = 2, timeoutMs = 8000 }) {
  const url = `https://github.com/${repo}/actions`
  let error = ''
  for (let i = 0; i < tries; i += 1) {
    if (i) await new Promise((r) => setTimeout(r, 1000))
    try {
      const r = await fetch(url, {
        headers: {
          Authorization: `Bearer ${token}`,
          // 页面是 HTML，Accept 不能写 application/vnd.github+json
          Accept: 'text/html',
          'User-Agent': 'Mozilla/5.0 (agent2api-actions-health)',
        },
        signal: AbortSignal.timeout(timeoutMs),
      })
      if (!r.ok) {
        error = `页面 HTTP ${r.status}`
        continue
      }
      const state = bannerState(await r.text())
      if (state === 'unknown') {
        error = '页面里没有可判读的内容'
        continue
      }
      return { state }
    } catch (e) {
      error = String((e && e.message) || e)
    }
  }
  return { state: 'unknown', error }
}

/**
 * 主判据：找「排队很久、却一个 job 都没分到」的 run。
 *
 * ── 为什么这条能证明熔断 ───────────────────────────────────────
 * 正常排队慢是「等 runner」，此时 job 列表**已经存在**（状态 queued，steps 为空）。
 * 熔断时是完全不同的一种状态：run 自己 queued，`/jobs` 直接 `total_count: 0`
 * —— 连 job 记录都没生成，平台没打算跑。实测本仓那个 run 卡了 20 分钟
 * `jobs.total_count = 0`，同时所有 dispatch 422。
 *
 * ── 为什么必须加「时效窗」（2026-11-21 修正）────────────────────
 * 熔断解除之后，熔断期那个排队的 run **永远不会被调度** —— 它就那么挂在
 * `queued` 里，`jobs` 永远是 0，直到有人手动取消它。GitHub 不会自动清理。
 *
 * 于是只按「有没有僵死 run」判定，会出现这种假阴性：
 *
 *   平台早已恢复 → dispatch 204、新 run 正常拿到 4 个 job
 *   但老僵尸 run 还在 → 判定仍是 disabled-fork → 交付被拦住
 *
 * 这是把「历史上出过一次事」当成「现在正在出事」。所以僵死证据必须加时效：
 * 只有**近期创建**的 run 才算数；老的僵尸 run 只作为旁证记录，不下结论。
 *
 * @returns {Promise<{stalled: number[], checked: number, stale: number[]}>}
 */
export async function probeStalledRuns({ api, minAgeMs = 8 * 60_000, freshWindowMs = 3 * 3600_000 }) {
  const r = await api('/actions/runs?per_page=20&status=queued')
  if (!r.ok) return { stalled: [], checked: 0, stale: [] }
  const runs = r.json?.workflow_runs || []
  const stalled = []
  const stale = []
  for (const run of runs) {
    const age = Date.now() - new Date(run.created_at).getTime()
    if (age < minAgeMs) continue
    const jobs = await api(`/actions/runs/${run.id}/jobs?per_page=100`)
    const total = jobs.ok ? jobs.json?.total_count ?? 0 : -1
    // -1 = 查不到，不下结论；0 = 确认没有 job
    if (total !== 0) continue
    // 僵尸 run 不会被平台清理，所以必须按创建时间分「新鲜」与「陈旧」。
    if (age <= freshWindowMs) stalled.push(run.id)
    else stale.push(run.id)
  }
  return { stalled, checked: runs.length, stale }
}

/**
 * 判「平台现在到底调度不调度」。
 *
 * ── 为什么要有这个判据 ───────────────────────────────────────
 * 单看僵死 run 会把熔断解除后的僵尸 run 当成现状（假阴性）。
 * 单看策略层又会漏（`permissions` 在熔断时也是 `enabled: true`，纯谎报）。
 * 真信号是：**最近创建的那个 run 有没有拿到 job**。
 *
 *   平台正常  → 新 run 立刻有 job（in_progress，total_count > 0）
 *   平台熔断  → 新 run 一直 queued，total_count = 0
 *
 * 这个判据只看「新 run 拿到 job 没有」，不看历史，因此不受僵尸 run 影响。
 */
export async function probeLiveScheduling({ api }) {
  const r = await api('/actions/runs?per_page=10')
  if (!r.ok) return { state: 'unknown', runId: null, jobs: -1 }
  const runs = (r.json?.workflow_runs || []).slice(0, 5)
  if (!runs.length) return { state: 'unknown', runId: null, jobs: -1 }

  // 按创建时间找最新的一个
  const latest = runs.reduce((a, b) =>
    new Date(a.created_at) >= new Date(b.created_at) ? a : b,
  )
  const jobs = await api(`/actions/runs/${latest.id}/jobs?per_page=100`)
  const total = jobs.ok ? jobs.json?.total_count ?? 0 : -1
  if (total === -1) return { state: 'unknown', runId: latest.id, jobs: -1 }
  return {
    state: total > 0 ? 'scheduling' : 'not-scheduling',
    runId: latest.id,
    jobs: total,
    status: latest.status,
  }
}

/** 熔断时唯一有效的动作：维护者在网页上点一次。API 无解。 */
export function enableHint(repo) {
  return [
    `打开 https://github.com/${repo}/actions`,
    '点页面顶部横幅的 "I understand my workflows, go ahead and enable them"，',
    '然后再跑一次 node scripts/deliver.mjs（已推的提交会复用，不会重复推）。',
    '',
    '线上 API 改不了这个开关 —— 实测都是「返回成功但不生效」：',
    '  PUT /repos/{r}/actions/permissions          → 204，仍旧 422',
    '  PUT /repos/{r}/actions/workflows/<id>/enable → 204，仍旧 422',
    '  POST /repos/{r}/actions/runs/<id>/rerun      → 201，但 jobs 永远是 0',
    '别在这上面耗时间。',
  ].join('\n')
}

/**
 * 把 dispatch 的失败响应归类。
 *
 * 关键区别：422 + "Actions has been disabled" 是**平台熔断**，不是参数错。
 * 老代码把原文打到屏幕上就退出，读起来像「派发参数写错了」，下一轮还在改 ref。
 */
export function classifyDispatch(status, text) {
  if (status === 0) return { kind: 'network', note: '网络失败（不是结论，重跑一次）' }
  if (status === 204 || (status >= 200 && status < 300)) return { kind: 'ok' }
  if (/Actions has been disabled for this repository/i.test(text || '')) {
    return { kind: 'disabled-fork', note: '平台已停用本 fork 的 Actions' }
  }
  if (status === 403) return { kind: 'forbidden', note: '凭据权限不足（403）' }
  if (status === 404) return { kind: 'not-found', note: '工作流文件名或 ref 不存在（404）' }
  if (status === 422) return { kind: 'unprocessable', note: '派发被拒（422，非熔断原因）' }
  return { kind: 'error', note: `派发失败 HTTP ${status}` }
}

/**
 * 综合判定。调用方给一个跑 API 的函数，其余全包在这里，保证三处判据一致。
 *
 * @param {{api: Function, repo: string, token: string}} opts
 * @returns {Promise<{verdict: string, evidence: object, hint: string|null}>}
 */
export async function healthVerdict({ api, repo, token }) {
  const perms = await api('/actions/permissions')
  const policy = perms.ok ? perms.json : null
  const stalled = await probeStalledRuns({ api })
  const live = await probeLiveScheduling({ api })
  const banner = await probeBanner({ repo, token })

  const evidence = {
    policyEnabled: policy?.enabled ?? null,
    allowedActions: policy?.allowed_actions ?? null,
    stalledRuns: stalled.stalled,
    staleStalledRuns: stalled.stale,
    queuedChecked: stalled.checked,
    liveScheduling: live.state,
    liveRunId: live.runId,
    liveJobs: live.jobs,
    banner: banner.state,
    bannerError: banner.error || null,
  }

  // ── 顺序即优先级 ─────────────────────────────────────────────
  // 1. 实时调度判据优先：它不受熔断期遗留的僵尸 run 影响。
  //    平台正常时新 run 一定拿得到 job —— 这是最硬的证据。
  if (live.state === 'scheduling') {
    return { verdict: 'ok', evidence, hint: null }
  }
  if (live.state === 'not-scheduling') {
    // 新 run 一个 job 都没分到，且它已经存在超过 8 分钟 → 平台没在调度。
    return {
      verdict: 'disabled-fork',
      evidence,
      hint: `最新 run ${live.runId} 没分到任何 job（jobs=0）—— 平台没有调度。\n\n${enableHint(repo)}`,
    }
  }

  // 2. 实时判据读不到（网络/权限），退回僵死队列判据。
  //    只看「新鲜」的僵死 run；陈旧的是熔断期遗留的僵尸，不能当现状。
  if (stalled.stalled.length) {
    return {
      verdict: 'disabled-fork',
      evidence,
      hint: `run ${stalled.stalled.join(',')} 排队超 8 分钟仍 0 个 job —— 平台没有调度。\n\n${enableHint(repo)}`,
    }
  }
  if (policy && policy.enabled === false) {
    return {
      verdict: 'disabled-policy',
      evidence,
      hint: `仓库策略层已关闭 Actions：PUT /repos/${repo}/actions/permissions {"enabled":true,"allowed_actions":"all"}`,
    }
  }
  if (banner.state === 'disabled') {
    return { verdict: 'disabled-fork', evidence, hint: enableHint(repo) }
  }
  if (!policy) {
    return {
      verdict: 'forbidden',
      evidence,
      hint: '读不到 actions/permissions —— 凭据权限不足或网络不通。',
    }
  }
  // 实时判据读不到、也没僵死 run、策略层正常 —— 不能当成绿灯，
  // 但也不能拦死交付：放行，同时把不确定性说清楚。
  return {
    verdict: 'unknown',
    evidence,
    hint: '所有判据都读不到确切结论（既没读到 job 分发，也没读到僵死队列）。\n推送后会以派发结果为准；若派发被拒会明确报 422 与原因。',
  }
}
