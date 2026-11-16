import * as React from 'react'
import { Badge, Button, Switch } from '@ui'
import { t } from '../i18n'
import * as m from './memory-api'
import { Empty, Panel, Section, StateBadge, num, timeText, useLoad } from './memory-shared'
import { OutcomeLine } from './memory-tab-profile'

/**
 * 自动化页：12 个后台任务的节奏、上次执行、立即执行、今日消耗与调用台账。
 *
 * ── 为什么默认只开零 token 的任务 ────────────────────────────
 * 花 token 的任务在后台悄悄烧钱，而用户装完这个功能时对「它会花多少」没有概念。
 * 所以有 token 的默认全关，需要用户自己点头。这一页的责任就是把「开它会花多少」
 * 说清楚 —— 每个任务标了「需模型」与估算。
 *
 * ── 为什么「立即执行」是同步等待 ────────────────────────────
 * 用户点了就想看到结果（「跑了什么、跳过没、为什么跳过」）。返回一个 taskId
 * 让他再刷新一次是多余的一步，而且刷新之后也未必看得出是刚跑的那次。
 *
 * ── 为什么跳过要说原因 ──────────────────────────────────────
 * 「跳过」有五种：没配模型、没素材、预算用完、隐私暂停、开关没开。混成一句
 * 「没执行」等于让用户去猜。所以 `OutcomeLine` 把 reason 原样显示。
 */

function Auto(): React.ReactElement {
  const { data, loading, error, reload } = useLoad(() => m.loadAuto(), [])
  const calls = useLoad(() => m.loadCalls(60), [])
  const [busy, setBusy] = React.useState('')
  const [actionError, setActionError] = React.useState('')
  const [outcome, setOutcome] = React.useState<{ task: string; outcome: m.Outcome } | null>(null)

  const run = async (task: string) => {
    setBusy(task)
    setActionError('')
    setOutcome(null)
    try {
      const result = await m.runAutoTask(task)
      setOutcome(result)
      reload()
      calls.reload()
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

  return (
    <div className='mm-auto'>
      {actionError ? <div className='mm-error'>{actionError}</div> : null}

      <Section title={t('调度器')} hint={t('扫描式：每 60 秒醒一次，检查每个任务「距上次跑够不够间隔」。错过一次不会漏，只是晚一点。')}>
        <div className='mm-inline-meta'>
          <StateBadge state={data.globalEnabled ? 'ok' : 'idle'} label={data.globalEnabled ? t('已开启') : t('已关闭')} />
          <StateBadge state={data.running ? 'ok' : 'idle'} label={data.running ? t('线程在跑') : t('线程未启动')} />
          {data.privacyPaused ? <Badge variant='warning' shape='tag'>{t('隐私暂停：全部任务停跑')}</Badge> : null}
          {data.budgetExhausted ? <Badge variant='destructive' shape='tag'>{t('今日预算已用完')}</Badge> : null}
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
            {t('预算在「配置 → 自动化」里改（auto.dailyTokenLimit）。超预算后按 auto.overBudget 处理：默认暂停，不是静默超支。')}
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
            <span />
          </div>
          {data.tasks.map(task => (
            <div key={task.task} className='mm-task-row'>
              <span className='mm-task-name'>
                {labelOf(task.task)}
                {task.task === 'sync' ? <Badge variant='outline' shape='tag'>{t('WebDAV')}</Badge> : null}
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
              <span className='mm-muted'>
                {task.intervalMin >= 1440
                  ? t('每 {n} 天', { n: Math.round(task.intervalMin / 1440) })
                  : t('每 {n} 分钟', { n: task.intervalMin })}
              </span>
              <span className='mm-muted'>{task.lastRunText ?? t('还没跑过')}</span>
              <span className='mm-muted'>{task.enabled ? timeText(task.nextRun) : '—'}</span>
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
