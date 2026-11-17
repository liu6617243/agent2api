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

/* ─── 页签容错边界 ───────────────────────────── */

type BoundaryState = { error: Error | null }

/**
 * 单个页签的渲染错误边界。
 *
 * ── 为什么必须有它 ────────────────────────────────────────
 * React 18 里渲染期抛出的异常（hooks 顺序变了、读到了 undefined 的字段、
 * 某个 `.map` 的返回值不是节点……）如果没有边界接住，React 会**卸载整棵
 * 子树**。记忆中枢是十页签共用一个根，一页崩掉的表现是「整页空白，连页签栏
 * 都没了」—— 用户既看不出是哪一页的问题，也没法切走。这不是理论风险：同步页
 * 就因为 `useMemo` 写在条件返回之后真的白过一次。
 *
 * 边界放在**每个页签外面**、不在页面根上：崩掉的只有那一页，页签栏和其它九页
 * 照常可用，用户还能换个页签继续干活，也还留着一条「重试」的路。
 *
 * ── 为什么用 class ───────────────────────────────────────
 * `componentDidCatch` / `getDerivedStateFromError` 至今只有 class 组件能实现，
 * 没有函数式等价物。这是 React 官方唯一强制 class 的场景。
 */
export class TabBoundary extends React.Component<
  { name: string; children: React.ReactNode },
  BoundaryState
> {
  state: BoundaryState = { error: null }

  static getDerivedStateFromError(error: Error): BoundaryState {
    return { error }
  }

  componentDidCatch(error: Error, info: React.ErrorInfo): void {
    // 控制台留全量堆栈：页面上的那句话只够让用户知道「哪一页坏了」，
    // 定位还是要看 `componentStack`（它指出崩在哪个组件的哪一次渲染）。
    console.error(`[memory:${this.props.name}] 渲染失败`, error, info.componentStack)
  }

  render(): React.ReactNode {
    const { error } = this.state
    if (!error) return this.props.children
    return (
      <div className='mm-panel-error'>
        <span>{t('「{name}」这一页渲染失败了：{message}', { name: t(this.props.name), message: error.message })}</span>
        <Button type='button' size='sm' variant='ghost' onClick={() => this.setState({ error: null })}>
          {t('重试')}
        </Button>
      </div>
    )
  }
}
