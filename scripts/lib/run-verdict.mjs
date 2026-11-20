// ─────────────────────────────────────────────────────────────────────
// run-verdict.mjs —— 判一次 CI run 到底是「代码红了」还是「平台没跑起来」
//
// ── 为什么需要它 ──────────────────────────────────────────────────
// 这个仓库的 CI 在 GitHub 上反复出现同一种假红：某个平台的 job 分到 runner
// 之后一步都没执行（`steps: []`），整整 15 分钟（runner 分配超时）后被平台
// 判 cancelled，整个 run 的结论于是变成 failure。此时：
//
//   Windows / Ubuntu / 覆盖率 —— success
//   macOS                      —— cancelled，0 个步骤，日志 BlobNotFound
//
// 它不是代码问题：真的有断言失败时必然留下一个结论为 failure 的 step。
// 但旧脚本只看 run.conclusion，于是把这种假红当成真红，拦住出包 ——
// 人得手动判断、手动点 rerun、手动再跑一次交付。这个判断逻辑本身就是
// 需要被固化下来的经验，所以单独抽成一个模块，三处共用。
//
// ── 判据（顺序即优先级）───────────────────────────────────────────
//   code   任一 job 存在结论为 failure 的 step        → 真失败，不许重跑绕过
//   infra  非成功的 job 里，有 0 个步骤的             → 平台没启动任务
//   soft   非成功的 job 跑过步骤但被 cancelled        → 并发顶掉 / 手工取消
//   ok     全部成功（或只剩 skipped / neutral）
//
// code 永远不自动重跑；infra / soft 可以自动重跑 —— 它们没有代码证据。
// ─────────────────────────────────────────────────────────────────────

const OK_CONCLUSIONS = new Set(['success', 'skipped', 'neutral'])

export function jobVerdict(jobs) {
  const real = [] // 有失败步骤 —— 代码问题
  const infra = [] // 一个步骤都没跑
  const soft = [] // 跑过步骤，但被取消

  for (const job of jobs || []) {
    const steps = job.steps || []
    const badStep = steps.find((s) => s.conclusion === 'failure')
    if (badStep) {
      real.push({ job: job.name, step: badStep.name, conclusion: job.conclusion })
      continue
    }
    if (OK_CONCLUSIONS.has(job.conclusion)) continue
    if (!steps.length) infra.push({ job: job.name, conclusion: job.conclusion })
    else soft.push({ job: job.name, conclusion: job.conclusion })
  }

  const state = real.length ? 'code' : infra.length ? 'infra' : soft.length ? 'soft' : 'ok'
  return { state, real, infra, soft }
}

// 给人看的一句话说明：哪个 job、什么结论、几个步骤。
export function describeVerdict(verdict) {
  const one = (x) => `${x.job}（${x.conclusion}）`
  if (verdict.state === 'code') return `失败步骤：${verdict.real.map((x) => `${x.job} / ${x.step}`).join('；')}`
  if (verdict.state === 'infra') return `平台没启动：${verdict.infra.map(one).join('、')}，0 个步骤`
  if (verdict.state === 'soft') return `被取消：${verdict.soft.map(one).join('、')}`
  return '全部 job 成功'
}
