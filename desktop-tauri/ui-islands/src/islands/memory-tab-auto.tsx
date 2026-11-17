import * as React from 'react'
import { Badge, Button, Switch } from '@ui'
import { t } from '../i18n'
import * as m from './memory-api'
import { Empty, Panel, Section, StateBadge, num, useLoad } from './memory-shared'
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

/**
 * 待确认裁决积压卡。
 *
 * ── 为什么把「待确认」从调度器状态里提出来单独一张卡 ─────────
 * 待确认积压**不是**调度器指标：调度器开得好好的、任务照常跑，积压也会涨。
 * 早先它只是状态行里一枚黄色小徽标，和「已开启 / 线程在跑」挤在同一排，
 * 读起来像又一次健康检查 —— 而它其实是这页唯一需要**人去干活**的东西。
 *
 * 提到独立卡片之后，卡片里只有两件事：有多少条等你裁决、去哪儿裁决。
 *
 * 超预算行为（`auto.overBudgetAction`）不在这张卡上：它属于「配置」，
 * 一年改一次，摆在日常页面上只会把这张卡的两个数字挤散。
 */
function VerdictCard({ pending, onGo }: { pending: number; onGo: () => void }): React.ReactElement {
  return (
    <div className='mm-verdict'>
      <div className='mm-verdict-head'>
        <span className='mm-verdict-icon' aria-hidden>
          {/* 方框里一个勾：与页面里「待确认」的语义一致（裁决 = 逐条过一遍）。
              用内联 SVG 而不是 wbIcons：图标字体在岛里要等 window.wbIcons 就位，
              这一枚是页面的主视觉，不能有「先空一下再出现」的时序。 */}
          <svg width='18' height='18' viewBox='0 0 24 24' fill='none' stroke='currentColor' strokeWidth='1.8' strokeLinecap='round' strokeLinejoin='round'>
            <rect x='3.5' y='3.5' width='17' height='17' rx='4' />
            <path d='M8 13.2l2.6 2.6L16 9.4' />
          </svg>
        </span>
        <div className='mm-verdict-title'>
          <div className='mm-verdict-name'>{t('待确认裁决建议')}</div>
          <div className='mm-verdict-sub'>{t('需人工判定的失效 / 归类 / 查重建议')}</div>
        </div>
      </div>

      <div className='mm-verdict-body'>
        <span className='mm-verdict-count'>
          <strong>{num(pending)}</strong>
          <span>{t('项待办')}</span>
        </span>
        <Button type='button' size='sm' variant='outline' onClick={onGo}>
          {t('进入裁决收件箱')}
          <span aria-hidden className='mm-verdict-arrow'>→</span>
        </Button>
      </div>
    </div>
  )
}

function Auto({ onGo }: { onGo: (tab: 'browse') => void }): React.ReactElement {
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
  // 累计 token 只在会调模型的任务上显示：零成本任务的 0 是恒定的，
  // 每行都写一遍「0 token」只会让人以为那台机器没记上账。
  const spend = (task: m.AutoTask) => (task.usesModel ? `${num(task.tokens)} token` : t('零成本'))
  const runs = showAllRuns ? allRuns.data?.items ?? data.recentRuns : data.recentRuns
  // 「查看全部 N 条」用**行数**（翻的是行）；执行次数在任务行的「累计」里。
  const runRows = data.runRows

  return (
    <div className='mm-auto'>
      {actionError ? <div className='mm-error'>{actionError}</div> : null}

      {/* 调度器状态 + 预算进度已按截图并入裁决卡；这里不再重复一遍「已开启 /
          线程在跑 / 上次 tick」—— 那三项在仪表盘有，两处都摆在页面最上面
          只会让真正要动手的事（裁决积压）被挤到第二眼。 */}
      <VerdictCard pending={data.pendingReview} onGo={() => onGo('browse')} />

      {data.privacyPaused ? (
        <div className='mm-scan-note'>{t('隐私暂停：全部自动化任务停跑。去「配置 → 自动化」解除。')}</div>
      ) : null}
      {data.budgetExhausted ? (
        <div className='mm-scan-note'>
          {t('今日预算已用完，任务按「{mode}」处理（auto.overBudgetAction，在「配置 → 自动化」里改）。', {
            mode: data.overBudgetAction === 'ignore' ? t('不限额继续') : t('暂停任务'),
          })}
        </div>
      ) : null}

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
