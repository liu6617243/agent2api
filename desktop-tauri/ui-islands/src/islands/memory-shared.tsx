import * as React from 'react'
import { Badge, Button, Spinner } from '@ui'
import { t } from '../i18n'

/**
 * 记忆中枢 · 各页签共用的小件与工具函数。
 *
 * ── 为什么单独一个文件 ──────────────────────────────────────
 * 十个页签里有八处要显示「3 分钟前」、六处要显示「这里是空的，因为…」、
 * 五处要显示「这一段在讲什么」。抄五遍的结果是措辞与留白各走各的 —— 用户在
 * 页签之间切换时看到的是五种空态、三种时间格式。所以收在这里一份。
 *
 * ── 为什么没有做「通用卡片组件」────────────────────────────
 * 组件库已经有 Card / MetricCard 了。再包一层 `MemoryCard` 只会让每个调用点
 * 多一个不认识的中间层，而真正的差异（页签标题的排版）用 `Section` 一个就够。
 */

/** 相对时间。超过 30 天给日期 —— 「47 天前」不如「2026/9/30」有用。 */
export function timeText(timestamp: number | null | undefined): string {
  if (!timestamp) return '—'
  const delta = Date.now() - timestamp
  if (delta < 0) return '刚刚'
  if (delta < 60_000) return '刚刚'
  if (delta < 3_600_000) return `${Math.floor(delta / 60_000)} 分钟前`
  if (delta < 86_400_000) return `${Math.floor(delta / 3_600_000)} 小时前`
  if (delta < 30 * 86_400_000) return `${Math.floor(delta / 86_400_000)} 天前`
  return new Date(timestamp).toLocaleDateString()
}

/** 绝对时间（悬停提示用）。 */
export function fullTime(timestamp: number | null | undefined): string {
  if (!timestamp) return '—'
  return new Date(timestamp).toLocaleString()
}

/** 数字加千分位。token 数动辄六位，不加分隔读不出量级。 */
export function num(value: number | null | undefined): string {
  if (value === null || value === undefined) return '—'
  return value.toLocaleString('en-US')
}

/** 页签内的小标题 + 一行说明。 */
export function Section({
  title,
  hint,
  action,
  children,
}: {
  title: string
  hint?: React.ReactNode
  action?: React.ReactNode
  children?: React.ReactNode
}): React.ReactElement {
  return (
    <section className='mm-section'>
      <div className='mm-section-head'>
        <div>
          <div className='mm-section-title'>{title}</div>
          {hint ? <div className='mm-section-hint'>{hint}</div> : null}
        </div>
        {action ? <div className='mm-section-action'>{action}</div> : null}
      </div>
      {children}
    </section>
  )
}

/** 空态：说清「为什么空」以及「怎么让它不空」。 */
export function Empty({ children }: { children: React.ReactNode }): React.ReactElement {
  return <div className='mm-empty'>{children}</div>
}

/** 加载中 / 失败 / 空 三态的统一外壳。 */
export function Panel({
  loading,
  error,
  onRetry,
  children,
}: {
  loading: boolean
  error: string
  onRetry?: () => void
  children: React.ReactNode
}): React.ReactElement {
  if (error) {
    return (
      <div className='mm-panel-error'>
        <span>{error}</span>
        {onRetry ? (
          <Button type='button' size='sm' variant='ghost' onClick={onRetry}>
            {t('重试')}
          </Button>
        ) : null}
      </div>
    )
  }
  if (loading) {
    return (
      <div className='mm-panel-loading'>
        <Spinner />
        <span>{t('正在读取…')}</span>
      </div>
    )
  }
  return <div className='mm-panel'>{children}</div>
}

/** 键值网格（状态页、同步页、索引页共用）。 */
export function Grid({ rows }: { rows: [string, React.ReactNode][] }): React.ReactElement {
  return (
    <div className='mm-status-grid'>
      {rows.map(([label, value]) => (
        <React.Fragment key={label}>
          <span>{label}</span>
          <code>{value}</code>
        </React.Fragment>
      ))}
    </div>
  )
}

/** 一行「标签 + 值」，用于卡片内的紧凑信息。 */
export function Row({ label, children }: { label: string; children: React.ReactNode }): React.ReactElement {
  return (
    <div className='mm-row-line'>
      <span className='mm-row-label'>{label}</span>
      <span className='mm-row-value'>{children}</span>
    </div>
  )
}

/** 状态徽标：把布尔/枚举映射成统一的四种色。 */
export function StateBadge({
  state,
  label,
}: {
  state: 'ok' | 'warn' | 'bad' | 'idle'
  label: string
}): React.ReactElement {
  const variant = state === 'ok' ? 'success' : state === 'warn' ? 'warning' : state === 'bad' ? 'destructive' : 'outline'
  return <Badge variant={variant} shape='tag'>{label}</Badge>
}

/**
 * 一个页签自己的加载器：面板挂载与「刷新」都走它。
 *
 * 为什么不用全局 store：每个页签的数据来自不同端点、刷新节奏也不同（同步页要
 * 手动刷新，浏览页要跟着筛选走）。一个全局 store 会让「点刷新」变成「重拉十个
 * 端点」，而用户只想刷新眼前这一页。
 */
export function useLoad<T>(
  loader: () => Promise<T>,
  deps: React.DependencyList,
): { data: T | null; loading: boolean; error: string; reload: () => void } {
  const [data, setData] = React.useState<T | null>(null)
  const [loading, setLoading] = React.useState(true)
  const [error, setError] = React.useState('')
  const [tick, setTick] = React.useState(0)
  const loaderRef = React.useRef(loader)
  loaderRef.current = loader

  React.useEffect(() => {
    let alive = true
    setLoading(true)
    setError('')
    loaderRef.current()
      .then(value => { if (alive) setData(value) })
      .catch((caught: unknown) => {
        if (alive) setError(caught instanceof Error ? caught.message : String(caught))
      })
      .finally(() => { if (alive) setLoading(false) })
    return () => { alive = false }
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [...deps, tick])

  return { data, loading, error, reload: () => setTick(value => value + 1) }
}
