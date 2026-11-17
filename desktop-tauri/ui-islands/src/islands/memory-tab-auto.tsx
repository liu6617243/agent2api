import * as React from 'react'
import { Badge, Button, Switch } from '@ui'
import { t } from '../i18n'
import * as m from './memory-api'
import { Empty, Panel, Section, StateBadge, num, timeText, useLoad } from './memory-shared'
import { OutcomeLine } from './memory-tab-profile'

/**
 * 自动化页：12 个后台任务的节奏、上次执行、立即执行、累计统计与执行审计。
 *
 * ── 为什么默认只开零 token 的任务 ────────────────────────────
 * 花 token 的任务在后台悄悄烧钱，而用户装完这个功能时对「它会花多少」没有概念。
 * 所以有 token 的默认全关，需要用户自己点头。这一页的责任就是把「开它会花多少」
 * 说清楚 —— 每个任务标了「调用模型 / 零成本」，累计列里还给出「已经烧了多少」。
 *
 * ── 为什么每行都要一句说明 ──────────────────────────────────
 * 任务名是抽象名词（「抽取要点」「事实失效判定」），不看代码猜不出它会动哪些
 * 数据、写到哪里。光有名字的表读起来像一份选项清单，加一句解释才像说明书。
 *
 * ── 为什么「累计」与「执行记录」是两张表 ────────────────────
 * 累计回答「这个任务可靠吗、贵不贵」（按任务聚合），执行记录回答「昨天 11:00
 * 它到底干了什么」（按时间下钻）。前者只有三个数字，塞进任务行不占地方；
 * 后者需要「结果 + 明细」两列，摊进任务行会把表撑爆。
 *
 * ── 为什么「立即执行」是同步等待 ────────────────────────────
 * 用户点了就想看到结果（「跑了什么、跳过没、为什么跳过」）。返回一个 taskId
 * 让他再刷新一次是多余的一步，而且刷新之后也未必看得出是刚跑的那次。
 *
 * ── 为什么跳过要说原因 ──────────────────────────────────────
 * 「跳过」有五种：没配模型、没素材、预算用完、隐私暂停、开关没开。混成一句
 * 「没执行」等于让用户去猜。所以 `OutcomeLine` 把 reason 原样显示，执行记录
 * 里的 `detail` 也存的是那句人话（后端 `detail_of`）。
 */

/** 执行结果 → 徽标。跳过着灰色而不是红色：它是常态，不是故障。 */
function StatusBadge({ status }: { status: TaskRunStatus }): React.ReactElement {
  const state = status === 'done' ? 'ok' : status === 'failed' ? 'bad' : 'idle'
  const label = status === 'done' ? t('完成') : status === 'failed' ? t('失败') : t('跳过')
  return <StateBadge state={state} label={label} />
}

type TaskRunStatus = m.TaskRun['status']

/** 「每 N 天 / 每 N 分钟」。与后端 `intervalMin` 同口径。 */
function cadenceText(minutes: number): string {
  return minutes >= 1440
    ? t('每 {n} 天', { n: Math.round(minutes / 1440) })
    : t('每 {n} 分钟', { n: minutes })
}

function Auto(): React.ReactElement {
  const { data, loading, error, reload } = useLoad(() => m.loadAuto(), [])
  const calls = useLoad(() => m.loadCalls(60), [])
  const [busy, setBusy] = React.useState('')
  const [actionError, setActionError] = React.useState('')
  const [outcome, setOutcome] = React.useState<{ task: string; outcome: m.Outcome } | null>(null)
  const [showAllRuns, setShowAllRuns] = React.useState(false)
  const allRuns = useLoad(() => (showAllRuns ? m.loadAutoRuns(200) : Promise.resolve(null)), [showAllRuns])

  const run = async (task: string) => {
    setBusy(task)
    setActionError('')
    setOutcome(null)
    try {
      const result = await m.runAutoTask(task)
      setOutcome(result)
      reload()
      calls.reload()
      if (showAllRuns) allRuns.reload()
    } catch (caught) {
      setActionError(caught instanceof Error ? caught.message : String(caught))
    } finally {
      setBusy('')
    }
  }

  if (loading) return <Panel loading error=''>{null}</Panel>
  if (error) return <Panel loading={false} error={error} onRetry={reload}>{null}</Panel>
  if (!data) return <Empty>{t('读不到调度器状态。')}</Empty>

  const labelOf = (task: string) => data.labels.find(item => item.task === task)?.label ?? task
  const used = data.dailyTokenLimit > 0 ? Math.min(100, Math.round((data.tokensToday / data.dailyTokenLimit) * 100)) : 0
  // 累计 token 只在会调模型的任务上显示：零成本任务的 0 是恒定的，
  // 每行都写一遍「0 token」只会让人以为那台机器没记上账。
  const spend = (task: m.AutoTask) => (task.usesModel ? `${num(task.tokens)} token` : t('零成本'))
  const runs = showAllRuns ? allRuns.data?.items ?? data.recentRuns : data.recentRuns
  // 「查看全部 N 条」用**行数**（翻的是行）；执行次数在任务行的「累计」里。
  const runRows = data.runRows

  return (
    <div className='mm-auto'>
      {actionError ? <div className='mm-error'>{actionError}</div> : null}

      <Section title={t('调度器')} hint={t('扫描式：每 60 秒醒一次，检查每个任务「距上次跑够不够间隔」。错过一次不会漏，只是晚一点。')}>
        <div className='mm-inline-meta'>
          <StateBadge state={data.globalEnabled ? 'ok' : 'idle'} label={data.globalEnabled ? t('已开启') : t('已关闭')} />
          <StateBadge state={data.running ? 'ok' : 'idle'} label={data.running ? t('线程在跑') : t('线程未启动')} />
          {data.privacyPaused ? <Badge variant='warning' shape='tag'>{t('隐私暂停：全部任务停跑')}</Badge> : null}
          {data.budgetExhausted ? <Badge variant='destructive' shape='tag'>{t('今日预算已用完')}</Badge> : null}
          {data.pendingReview > 0 ? (
            <Badge
              variant='warning'
              shape='tag'
              title={t('待确认的裁决建议（去重 / 失效 / 合并）。积压到 50 条会挡住新的归类任务。')}
            >
              {t('待确认 {n} 条', { n: num(data.pendingReview) })}
            </Badge>
          ) : null}
          <span className='mm-muted'>{t('上次 tick：{when}', { when: timeText(data.lastTick) })}</span>
        </div>

        <div className='mm-budget'>
          <div className='mm-budget-head'>
            <span>{t('今日模型消耗')}</span>
            <span className='mm-muted'>{num(data.tokensToday)} / {num(data.dailyTokenLimit)} token（{used}%）</span>
          </div>
          <div className='mm-budget-track'>
            <div className={`mm-budget-fill${used >= 100 ? ' full' : used >= 80 ? ' high' : ''}`} style={{ width: `${used}%` }} />
          </div>
          <p className='mm-hint'>
            {t('预算在「配置 → 自动化」里改（auto.dailyTokenLimit）。超预算后按 auto.overBudgetAction 处理：现在是「{mode}」。', {
              mode: data.overBudgetAction === 'ignore' ? t('照跑') : t('暂停'),
            })}
          </p>
        </div>
      </Section>

      <Section title={t('任务')} hint={t('零 token 的默认开，花 token 的默认关 —— 它们会在后台消耗你的额度。')}>
        <div className='mm-task-table'>
          <div className='mm-task-row mm-task-head'>
            <span>{t('任务')}</span>
            <span>{t('开关')}</span>
            <span>{t('节奏')}</span>
            <span>{t('上次')}</span>
            <span>{t('下次')}</span>
            <span>{t('累计')}</span>
            <span />
          </div>
          {data.tasks.map(task => (
            <div key={task.task} className='mm-task-row'>
              <span className='mm-task-cell'>
                <span className='mm-task-name'>
                  {labelOf(task.task)}
                  {task.task === 'sync' ? <Badge variant='outline' shape='tag'>{t('WebDAV')}</Badge> : null}
                  {task.usesModel
                    ? <Badge variant='outline' shape='tag' title={t('开它会在后台调模型、消耗额度')}>{t('调用模型')}</Badge>
                    : <Badge variant='ghost' shape='tag' title={t('纯本地计算，不调模型')}>{t('零成本')}</Badge>}
                </span>
                <span className='mm-task-desc'>{task.summary}</span>
              </span>
              <span>
                <Switch
                  checked={task.enabled}
                  size='sm'
                  onCheckedChange={value => {
                    void (async () => {
                      try {
                        await m.saveConfig({ [`auto.tasks.${task.task}`]: value })
                        reload()
                      } catch (caught) {
                        setActionError(caught instanceof Error ? caught.message : String(caught))
                      }
                    })()
                  }}
                />
              </span>
              <span className='mm-muted'>{cadenceText(task.intervalMin)}</span>
              <span className='mm-muted'>{task.lastRunText ?? t('还没跑过')}</span>
              <span className='mm-muted'>{task.enabled ? task.nextRunText : '—'}</span>
              <span
                className='mm-muted mm-task-stats'
                title={t('跑过 {runs} 次，成功 {ok} 次；累计消耗 {tokens}', {
                  runs: num(task.runs),
                  ok: num(task.success),
                  tokens: spend(task),
                })}
              >
                {task.runs > 0
                  ? <>{t('{runs} 次', { runs: num(task.runs) })}<span className='mm-muted'>{task.runs > task.success ? ` · ${t('成功 {n}', { n: num(task.success) })}` : ''}</span></>
                  : '—'}
                <span className='mm-task-spend'>{spend(task)}</span>
              </span>
              <span>
                <Button
                  type='button'
                  size='sm'
                  variant='ghost'
                  disabled={busy === task.task}
                  onClick={() => void run(task.task)}
                >
                  {busy === task.task ? t('执行中…') : t('立即执行')}
                </Button>
              </span>
            </div>
          ))}
        </div>
        {outcome ? (
          <div className='mm-outcome-box'>
            <div className='mm-section-title'>{t('「{task}」的结果', { task: labelOf(outcome.task) })}</div>
            <OutcomeLine outcome={outcome.outcome} />
          </div>
        ) : null}
      </Section>

      <Section
        title={t('执行记录')}
        hint={t('每次任务跑完（含跳过与失败）都落一行。想知道「昨天 11:00 它到底动了什么」看这里。')}
        action={
          <div className='mm-inline-actions'>
            {runRows > data.recentRuns.length ? (
              <Button type='button' size='sm' variant='ghost' onClick={() => setShowAllRuns(value => !value)}>
                {showAllRuns ? t('只看最近 {n} 条', { n: data.recentRuns.length }) : t('查看全部 {n} 条', { n: num(runRows) })}
              </Button>
            ) : null}
            <Button type='button' size='sm' variant='ghost' onClick={() => { reload(); if (showAllRuns) allRuns.reload() }}>
              {t('刷新')}
            </Button>
          </div>
        }
      >
        {runs.length === 0 ? (
          <Empty>{t('还没有执行记录。任务跑过一次之后这里就会有行 —— 自动轮次与「立即执行」都算。')}</Empty>
        ) : (
          <div className='mm-run-table'>
            <div className='mm-run-row mm-run-head'>
              <span>{t('时间')}</span>
              <span>{t('任务')}</span>
              <span>{t('结果')}</span>
              <span>{t('明细')}</span>
              <span>{t('token')}</span>
              <span>{t('耗时')}</span>
            </div>
            {runs.map((row, index) => (
              <div key={`${row.at}-${row.task}-${index}`} className='mm-run-row'>
                <span className='mm-muted' title={row.manual ? t('用户点「立即执行」触发') : t('自动轮次触发')}>
                  {row.atText}
                  {row.repeat > 1 ? <span className='mm-run-manual' title={t('连续 {n} 次相同结果，合并成一行', { n: row.repeat })}>{t('×{n}', { n: row.repeat })}</span> : null}
                  {row.manual ? <span className='mm-run-manual'>{t('手动')}</span> : null}
                </span>
                <span>{labelOf(row.task)}</span>
                <span><StatusBadge status={row.status} /></span>
                {/* 明细是后端 `detail_of` 生成的一句人话，原样显示：
                    失败/跳过的原因已经在里面，不再拼前缀。 */}
                <span className='mm-muted mm-run-detail' title={row.detail}>{row.detail || '—'}</span>
                <span className='mm-mono'>{row.tokens > 0 ? num(row.tokens) : '—'}</span>
                <span className='mm-mono'>{row.latencyMs} ms</span>
              </div>
            ))}
          </div>
        )}
      </Section>

      <Section
        title={t('调用台账')}
        hint={t('每次内部模型调用都落一行。排查「为什么今天烧了这么多」看这里。')}
        action={<Button type='button' size='sm' variant='ghost' onClick={calls.reload}>{t('刷新')}</Button>}
      >
        {calls.loading ? <Panel loading error=''>{null}</Panel> : null}
        {calls.data && calls.data.items.length === 0 ? (
          <Empty>{t('还没有调用记录。开了花 token 的任务之后这里才会有行。')}</Empty>
        ) : null}
        {calls.data && calls.data.items.length > 0 ? (
          <div className='mm-call-table'>
            <div className='mm-call-row mm-call-head'>
              <span>{t('时间')}</span>
              <span>{t('任务')}</span>
              <span>{t('模型')}</span>
              <span>{t('token')}</span>
              <span>{t('耗时')}</span>
              <span>{t('结果')}</span>
            </div>
            {calls.data.items.map((row, index) => (
              <div key={index} className='mm-call-row'>
                <span className='mm-muted'>{row.atText}</span>
                <span>{labelOf(row.task)}</span>
                <span className='mm-mono'>{row.model || '—'}</span>
                <span className='mm-mono'>{num(row.promptTokens)} + {num(row.completionTokens)}</span>
                <span className='mm-mono'>{row.latencyMs} ms</span>
                <span>
                  {row.ok
                    ? <Badge variant='success' shape='tag'>{t('成功')}</Badge>
                    : <Badge variant='destructive' shape='tag' title={row.error ?? ''}>{t('失败')}</Badge>}
                </span>
              </div>
            ))}
          </div>
        ) : null}
      </Section>
    </div>
  )
}

export { Auto }
