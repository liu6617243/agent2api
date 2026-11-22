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

/**
 * 紧凑时间戳 `MM-DD HH:mm`（列表表格用）。
 *
 * 为什么不用 `timeText`：表格一列几十行，「3 分钟前」读不出是哪天写的，
 * 而「11-19 10:41」既能横向比较又保留了精确度。悬停另有全量时间。
 */
export function stamp(timestamp: number | null | undefined): string {
  if (!timestamp) return '—'
  const date = new Date(timestamp)
  const pad = (value: number): string => String(value).padStart(2, '0')
  return `${pad(date.getMonth() + 1)}-${pad(date.getDate())} ${pad(date.getHours())}:${pad(date.getMinutes())}`
}

/**
 * 写入方显示名。
 *
 * 只映射 `manual` / `import` 两个固定来源：其余都是用户自己配的 Agent 名
 * （`WorkBuddy` / `distill` 之类），照原样显示才对 —— 用户认的是自己写的那个名字。
 */
const AGENT_LABELS: Record<string, string> = {
  manual: '手动',
  import: '导入',
  'auto-extract': '自动抽取',
}

export function agentText(agent: string | null | undefined): string {
  if (!agent) return '—'
  return AGENT_LABELS[agent] ?? agent
}

/**
 * 这条是不是「还没敲定的草稿」。
 *
 * auto-extract 的产物先落 l1，累够强化次数才升 l2。在 l1 这一段时间里它就是
 * 草稿：能看、能批、能被后续素材强化，但**不能**被当成既定事实引用。后端已经
 * 在检索 / 注入 / 核心记忆 / digest 里把这些出口堵住了，界面同样要标出来，
 * 否则用户在列表里看到一条漂亮的结论，会以为系统已经认了它。
 */
export function isDraft(entry: { layer: string; agent: string | null }): boolean {
  return entry.layer === 'l1' && entry.agent === 'auto-extract'
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

/* ─── 「这一页又被看见了」信号 ───────────────── */

/**
 * 「重新进入这一页」与「用户点了刷新」两个计数器。
 *
 * ── 为什么需要跨组件的信号 ──────────────────────────────────
 * 九个页签的数据各自用 `useLoad` 自己拉，壳那层够不着它们的 `reload()`。
 * 而「离开记忆页再回来」时组件树并不重建（`mountOnPage` 只挂一次 React root，
 * 之后外壳只切 `active` 类），`useLoad(..., [])` 不会重跑 —— 用户看到的是
 * 离开时的快照，只能手动点刷新。真机反馈就是这一条。
 *
 * 于是把「这一页又被看见了」做成一个模块级信号：页面监听 `wb:page` /
 * `visibilitychange`，在这里打一次戳；每个 `useLoad` 自己订阅，把戳编进依赖。
 * 不需要给九个组件各传一个 prop。
 *
 * ── 为什么要两个计数器 ──────────────────────────────────────
 * 两者的代价与语义不同：
 * - `enterStamp`：进页面就重拉。只有**显式开启**的端点跟（目前是仪表盘、
 *   浏览页、项目页的列表）。
 * - `forceStamp`：用户**主动**点了「刷新」，连没开启的那些也一起重拉。
 *
 * 分开是必须的：进页面重拉是「顺手」，可以只覆盖常用页；用户点刷新是「我现在
 * 就要最新的」，把导入页那种递归扫十二个来源目录的贵端点也排除在外就是耍赖。
 */
let enterStamp = 0
let forceStamp = 0

const watchers = new Set<() => void>()

/** 这一页重新成为当前页（切回来、窗口重新可见）。 */
export function notifyMemoryEnter(): void {
  enterStamp += 1
  for (const watcher of watchers) watcher()
}

/** 用户点了「刷新」：所有 `useLoad` 都重拉，包括豁免了「进页面重拉」的那些。 */
export function notifyMemoryRefresh(): void {
  forceStamp += 1
  for (const watcher of watchers) watcher()
}

function watchEnter(watcher: () => void): () => void {
  watchers.add(watcher)
  return () => { watchers.delete(watcher) }
}

/**
 * 一个页签自己的加载器：面板挂载、重新进入页面、以及「刷新」都走它。
 *
 * ── 为什么 `loading` 只表示「还没拿到过数据」────────────────
 * 重新进入页面时会静默重拉。如果 `loading` 在重拉期间也是 true，页面会整块闪
 * 成骨架 —— 每切一次页面闪一次，比不刷新还烦。所以：
 *
 * - `loading === true` ⇔ 请求还没回来 **且** 手上一条数据都没有 → 该显示骨架；
 * - 有旧数据时的重拉 → `loading` 保持 false，界面继续显示旧值，新值到了就地替换。
 *
 * 这个口径让各页签里现有的 `if (loading) return <Panel loading/>` 自动变成
 * 「只在首屏闪」，不必逐页改判断。
 */
export function useLoad<T>(
  loader: () => Promise<T>,
  deps: React.DependencyList,
  options?: {
    /**
     * 重新进入页面时要不要重拉。**默认 `false`（不跟）**。
     *
     * 刻意做成「按页签显式开启」而不是默认全开：重拉是要花钱的（有的端点要扫
     * 目录、有的会让正在编辑的表单换一份依据），哪些页该跟是产品决定，不该由
     * 公共 hook 替所有页签决定。目前开启的只有仪表盘、浏览页、项目页。
     *
     * 用户点「刷新」时**不看这个开关**，一律重拉（见 `notifyMemoryRefresh`）。
     */
    refreshOnEnter?: boolean
  },
): { data: T | null; loading: boolean; error: string; reload: () => void } {
  const [data, setData] = React.useState<T | null>(null)
  const [loading, setLoading] = React.useState(true)
  const [error, setError] = React.useState('')
  const [tick, setTick] = React.useState(0)
  /** 只为「信号变了要重渲染一次」而存在；真正的判据是下面的 `signal`。 */
  const [, rerender] = React.useState(0)
  const loaderRef = React.useRef(loader)
  loaderRef.current = loader
  /** 是否已经有过一次请求结束（成功或失败都算）。决定 `loading` 还该不该为真。 */
  const settled = React.useRef(false)

  React.useEffect(() => watchEnter(() => rerender(value => value + 1)), [])

  // 进入页面 / 手动刷新合成的依赖项。没开启 `refreshOnEnter` 的端点只看
  // `forceStamp`，所以 `enterStamp` 变化时它的这一项不变，effect 不会重跑。
  const signal = `${options?.refreshOnEnter === true ? String(enterStamp) : 'stay'}:${forceStamp}`

  React.useEffect(() => {
    let alive = true
    if (!settled.current) setLoading(true)
    setError('')
    loaderRef.current()
      .then(value => { if (alive) setData(value) })
      .catch((caught: unknown) => {
        if (alive) setError(caught instanceof Error ? caught.message : String(caught))
      })
      .finally(() => {
        if (!alive) return
        settled.current = true
        setLoading(false)
      })
    return () => { alive = false }
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [...deps, tick, signal])

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
