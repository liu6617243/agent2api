import * as React from 'react'
import { Badge, Button, MetricCard, MetricGrid, MetricLabel, MetricValue } from '@ui'
import { t } from '../i18n'
import * as m from './memory-api'
import { Empty, Panel, Section, StateBadge, num, timeText, useLoad } from './memory-shared'

/**
 * 仪表盘：一眼看全「记忆库现在什么状态」。
 *
 * ── 这一页要回答的四个问题 ──────────────────────────────────
 *   1. 有多少条？分层比例对不对（l2 太少说明从没蒸馏过）；
 *   2. 今天有没有新东西进来？（数字不动 = 没人在用，或 Agent 没接上）
 *   3. 有没有积压要人裁决的？（待确认队列是这套系统唯一需要人工的地方）
 *   4. 同步与体检有没有异常？
 *
 * ── 为什么把「待确认」放这么显眼 ────────────────────────────
 * 记忆的自动化默认全关，唯一会「卡住」的就是待确认队列：AI 提了建议没人批，
 * 队列会一直涨，而用户完全不知道。所以它必须在第一屏。
 *
 * ── 为什么没有「最近 7 天热力图」───────────────────────────
 * 后端目前没有按天聚合的接口（`llm_call` / `recall_log` 有原始行但没有日聚合）。
 * 画一个假的热力图不如不画 —— 这里改成「今日 token 消耗」这一个真实数字。
 * 要热力图就得先加一个按天 GROUP BY 的端点，那是另一件事。
 */

type DashboardProps = {
  status: m.MemoryStatus | null
  onGo: (tab: 'browse' | 'profile' | 'auto' | 'sync' | 'index' | 'agents') => void
}

function Dashboard({ status, onGo }: DashboardProps): React.ReactElement {
  const auto = useLoad(() => m.loadAuto(), [])
  const health = useLoad(() => m.loadHealth(20), [])
  const sync = useLoad(() => m.loadSync(), [])

  if (!status) {
    return <Panel loading error=''>{null}</Panel>
  }

  const l2Ratio = status.live > 0 ? Math.round((status.l2 / status.live) * 100) : 0
  const pending = status.review?.pending ?? 0
  const backlogged = pending >= (status.review?.warnThreshold ?? 20)
  const conflicts = sync.data?.conflicts ?? 0
  const stale = health.data?.staleCount ?? 0
  const drift = health.data ? !health.data.index.inSync : false

  return (
    <div className='mm-dashboard'>
      <MetricGrid>
        <MetricCard>
          <MetricLabel>{t('有效记忆')}</MetricLabel>
          <MetricValue>{num(status.live)}</MetricValue>
          <div className='mm-metric-sub'>
            {t('沉淀 {l2} · 流水 {l1}', { l2: num(status.l2), l1: num(status.l1) })}
          </div>
        </MetricCard>
        <MetricCard>
          <MetricLabel>{t('沉淀占比')}</MetricLabel>
          <MetricValue>{l2Ratio}%</MetricValue>
          <div className='mm-metric-sub'>
            {l2Ratio < 15
              ? t('偏低：打开「蒸馏」任务让结论沉下来')
              : t('比例健康')}
          </div>
        </MetricCard>
        <MetricCard>
          <MetricLabel>{t('待确认')}</MetricLabel>
          <MetricValue className={pending ? 'mm-warn-text' : undefined}>{num(pending)}</MetricValue>
          <div className='mm-metric-sub'>
            {pending ? t('需要你点头才会落盘') : t('队列是空的')}
          </div>
        </MetricCard>
        <MetricCard>
          <MetricLabel>{t('今日模型消耗')}</MetricLabel>
          <MetricValue>{num(auto.data?.tokensToday ?? 0)}</MetricValue>
          <div className='mm-metric-sub'>
            {t('上限 {limit} token', { limit: num(auto.data?.dailyTokenLimit ?? 0) })}
          </div>
        </MetricCard>
        <MetricCard>
          <MetricLabel>{t('项目')}</MetricLabel>
          <MetricValue>{num(status.projects)}</MetricValue>
          <div className='mm-metric-sub'>{t('按 git 远程自动归档')}</div>
        </MetricCard>
        <MetricCard>
          <MetricLabel>{t('回收站')}</MetricLabel>
          <MetricValue>{num(status.deleted)}</MetricValue>
          <div className='mm-metric-sub'>{t('已取代 {n} 条', { n: num(status.superseded) })}</div>
        </MetricCard>
      </MetricGrid>

      <Section
        title={t('需要处理')}
        hint={t('这四类问题都不会自己消失，也不会阻塞 Agent —— 但放着会越来越难查。')}
      >
        <div className='mm-todo-list'>
          <TodoRow
            state={backlogged ? 'warn' : 'ok'}
            title={t('待确认队列')}
            detail={pending ? t('{n} 条建议等待裁决（去重 / 归类 / 失效判定）', { n: pending }) : t('没有积压')}
            action={pending ? { label: t('去裁决'), run: () => onGo('browse') } : undefined}
          />
          <TodoRow
            state={stale > 0 ? 'warn' : 'ok'}
            title={t('可能过时的记忆')}
            detail={stale
              ? t('{n} 条记忆锚定的文件在写下之后被改过', { n: stale })
              : t('没有发现')}
            action={stale > 0 ? { label: t('看清单'), run: () => onGo('index') } : undefined}
          />
          <TodoRow
            state={conflicts > 0 ? 'bad' : 'ok'}
            title={t('同步冲突')}
            detail={conflicts ? t('{n} 个文件两边都改过，需要选一边', { n: conflicts }) : t('没有冲突')}
            action={conflicts > 0 ? { label: t('去裁决'), run: () => onGo('sync') } : undefined}
          />
          <TodoRow
            state={drift ? 'warn' : 'ok'}
            title={t('索引一致性')}
            detail={drift
              ? t('索引条数与事实源不一致（可重建，不影响数据）')
              : t('索引与事实源一致')}
            action={drift ? { label: t('去重建'), run: () => onGo('index') } : undefined}
          />
        </div>
      </Section>

      <div className='mm-two-col'>
        <Section title={t('自动化')} hint={t('后台任务默认只开零 token 的那几个。')}>
          {auto.loading ? <Panel loading error=''>{null}</Panel> : auto.data ? (
            <>
              <div className='mm-inline-meta'>
                <StateBadge
                  state={auto.data.globalEnabled ? (auto.data.running ? 'ok' : 'warn') : 'idle'}
                  label={auto.data.globalEnabled
                    ? (auto.data.running ? t('调度器运行中') : t('已开启但未在跑'))
                    : t('已关闭')}
                />
                {auto.data.privacyPaused ? <Badge variant='warning' shape='tag'>{t('隐私暂停')}</Badge> : null}
                {auto.data.budgetExhausted ? <Badge variant='destructive' shape='tag'>{t('今日预算已用完')}</Badge> : null}
              </div>
              <ul className='mm-plain-list'>
                {auto.data.tasks.slice(0, 6).map(task => {
                  const label = auto.data?.labels.find(item => item.task === task.task)?.label ?? task.task
                  return (
                    <li key={task.task}>
                      <span>{label}</span>
                      <span className='mm-muted'>
                        {task.enabled
                          ? (task.lastRunText ? t('上次 {when}', { when: task.lastRunText }) : t('还没跑过'))
                          : t('已关闭')}
                      </span>
                    </li>
                  )
                })}
              </ul>
              <Button type='button' size='sm' variant='ghost' onClick={() => onGo('auto')}>
                {t('查看全部 12 个任务')}
              </Button>
            </>
          ) : <Empty>{auto.error || t('读不到调度器状态')}</Empty>}
        </Section>

        <Section title={t('同步与接入')} hint={t('记忆跟着 WebDAV 走，Agent 通过 MCP 读它。')}>
          <div className='mm-status-grid mm-status-compact'>
            <span>{t('同步')}</span>
            <code>
              {sync.data
                ? sync.data.configured
                  ? (sync.data.lastSyncAt ? t('上次 {when}', { when: timeText(sync.data.lastSyncAt) }) : t('已配置，还没同步过'))
                  : t('未配置 WebDAV')
                : '—'}
            </code>
            <span>{t('设备')}</span>
            <code>{sync.data?.deviceName || sync.data?.device || '—'}</code>
            <span>{t('记忆根')}</span>
            <code>{status.root}</code>
            <span>{t('索引')}</span>
            <code>{status.index.count} {t('条')} · {status.index.fts ? 'FTS5' : t('内存匹配')}</code>
          </div>
          <div className='mm-inline-actions'>
            <Button type='button' size='sm' variant='ghost' onClick={() => onGo('sync')}>{t('同步设置')}</Button>
            <Button type='button' size='sm' variant='ghost' onClick={() => onGo('agents')}>{t('Agent 接入')}</Button>
            <Button type='button' size='sm' variant='ghost' onClick={() => onGo('profile')}>{t('画像')}</Button>
          </div>
        </Section>
      </div>
    </div>
  )
}

function TodoRow({
  state,
  title,
  detail,
  action,
}: {
  state: 'ok' | 'warn' | 'bad' | 'idle'
  title: string
  detail: string
  action?: { label: string; run: () => void }
}): React.ReactElement {
  const variant = state === 'ok' ? 'success' : state === 'warn' ? 'warning' : state === 'bad' ? 'destructive' : 'outline'
  return (
    <div className='mm-todo'>
      <span className={`mm-todo-dot mm-todo-${state}`} aria-hidden='true' />
      <div className='mm-todo-body'>
        <div className='mm-todo-title'>
          {title}
          <Badge variant={variant} shape='tag'>{state === 'ok' ? t('正常') : t('待处理')}</Badge>
        </div>
        <div className='mm-todo-detail'>{detail}</div>
      </div>
      {action ? (
        <Button type='button' size='sm' variant='ghost' onClick={action.run}>{action.label}</Button>
      ) : null}
    </div>
  )
}

export { Dashboard }
