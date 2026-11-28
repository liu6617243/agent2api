import * as React from 'react'
import { Badge, Button, Spinner } from '@ui'
import { t } from '../i18n'
import type * as s from './skills-hub-api'

/**
 * 技能仓库各页签共用的小件。
 *
 * 它被 `import.meta.glob('./islands/*.tsx')` 一并引入（与 `memory-shared.tsx`
 * 同一情形），但**没有 `mountOnPage` 登记**，所以不会自己挂到任何 DOM 上 ——
 * 只是一份被各页签 import 的模块。
 *
 * ── 为什么自己再写一份 `useLoad` ────────────────────────────
 * 记忆中枢的 `useLoad`（`memory-shared.tsx`）把「重新进入页面」的信号绑在
 * **记忆页**的 `wb:page` / `visibilitychange` 上（模块级 `enterStamp`，由
 * `notifyMemoryEnter` 打戳）。技能仓库是另一个页面，套用那份信号会得到
 * 「切到记忆页、技能仓库跟着重扫磁盘」这种莫名其妙的联动。所以这里写一份
 * 同口径但信号独立的实现：`loading` 只表示「还没拿到过数据」，重新进入页面
 * 时静默重拉、不闪骨架。
 */

let enterStamp = 0
let forceStamp = 0
const watchers = new Set<() => void>()

function bump(kind: 'enter' | 'force'): void {
  if (kind === 'enter') enterStamp += 1
  else forceStamp += 1
  for (const watcher of watchers) watcher()
}

/** 技能仓库这一页重新成为当前页。 */
export function notifyEnter(): void {
  bump('enter')
}

/** 用户点了页面上的「刷新」：所有页签都重拉。 */
export function notifyRefresh(): void {
  bump('force')
}

function watch(watcher: () => void): () => void {
  watchers.add(watcher)
  return () => { watchers.delete(watcher) }
}

export function useLoad<T>(
  loader: () => Promise<T>,
  deps: React.DependencyList,
  options?: { refreshOnEnter?: boolean },
): { data: T | null; loading: boolean; error: string; reload: () => void } {
  const [data, setData] = React.useState<T | null>(null)
  const [loading, setLoading] = React.useState(true)
  const [error, setError] = React.useState('')
  const [tick, setTick] = React.useState(0)
  const [, rerender] = React.useState(0)
  const loaderRef = React.useRef(loader)
  loaderRef.current = loader
  const settled = React.useRef(false)

  React.useEffect(() => watch(() => rerender(value => value + 1)), [])

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

  const reload = React.useCallback(() => setTick(value => value + 1), [])
  return { data, loading, error, reload }
}

/* ─── 播放 / 格式化 ─────────────────────────────────────────── */

type ToastApi = { wbApp?: { toast?: (message: string, kind?: 'err' | 'ok') => void } }

export function toast(message: string, kind?: 'err' | 'ok'): void {
  const host = window as unknown as ToastApi
  host.wbApp?.toast?.(message, kind)
}

export function errorText(error: unknown): string {
  return error instanceof Error ? error.message : String(error)
}

export function num(value: number | null | undefined): string {
  if (value === null || value === undefined) return '—'
  return value.toLocaleString('en-US')
}

export function timeText(timestamp: number | null | undefined): string {
  if (!timestamp || timestamp <= 0) return t('从未')
  return new Date(timestamp).toLocaleString()
}

/** 相对时间（「3 分钟前」/「2 天前」）：比绝对时间更能回答「这是新的吗」。 */
export function ago(timestamp: number | null | undefined): string {
  if (!timestamp || timestamp <= 0) return t('从未')
  const diff = Date.now() - timestamp
  if (diff < 60_000) return t('刚刚')
  const minutes = Math.floor(diff / 60_000)
  if (minutes < 60) return t('{n} 分钟前', { n: minutes })
  const hours = Math.floor(minutes / 60)
  if (hours < 24) return t('{n} 小时前', { n: hours })
  const days = Math.floor(hours / 24)
  return t('{n} 天前', { n: days })
}

export function sizeText(bytes: number | null | undefined): string {
  const value = Number(bytes ?? 0)
  if (!Number.isFinite(value) || value <= 0) return '—'
  if (value >= 1024 * 1024 * 1024) return `${(value / (1024 * 1024 * 1024)).toFixed(1)} GB`
  if (value >= 1024 * 1024) return `${(value / (1024 * 1024)).toFixed(1)} MB`
  if (value >= 1024) return `${(value / 1024).toFixed(1)} KB`
  return `${value} B`
}

/* ─── 布局件 ───────────────────────────────────────────────── */

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
    <section className='sk-section'>
      <div className='sk-section-head'>
        <div className='sk-section-copy'>
          <div className='sk-section-title'>{title}</div>
          {hint ? <div className='sk-section-hint'>{hint}</div> : null}
        </div>
        {action ? <div className='sk-section-action'>{action}</div> : null}
      </div>
      {children}
    </section>
  )
}

export function Empty({ children }: { children: React.ReactNode }): React.ReactElement {
  return <div className='sk-empty'>{children}</div>
}

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
      <div className='sk-panel-error'>
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
      <div className='sk-panel-loading'>
        <Spinner />
        <span>{t('正在读取…')}</span>
      </div>
    )
  }
  return <div className='sk-panel'>{children}</div>
}

/** 体检徽标：`bad` 红（客户端不认这个技能）、`warn` 黄（能跑但触发不出）。 */
export function HealthBadge({ issues }: { issues?: { level: string; text: string }[] }): React.ReactElement | null {
  const list = issues ?? []
  if (list.length === 0) return null
  const worst = list.some(item => item.level === 'bad') ? 'bad' : 'warn'
  return (
    <span
      className={`sk-health sk-health-${worst}`}
      title={list.map(item => item.text).join('\n')}
    >
      {worst === 'bad' ? t('{n} 项要修', { n: list.length }) : t('{n} 项提醒', { n: list.length })}
    </span>
  )
}

/* ─── 技能卡片（技能库与仪表盘共用）─────────────────────────────
 *
 * ── 为什么状态判定放在这里而不是各自页面里 ────────────────────
 * 仪表盘现在也内嵌技能卡片网格。如果两个页面各写一套「什么算已挂载」，
 * 同一个技能在仪表盘标「已挂载」、在技能库里标「未挂载」几乎是必然的事 ——
 * 判定逻辑只允许存在一份，两处都从这里取。
 */

/** 六档筛选键；`all` 不参与状态判定。 */
export type FilterKey = 'all' | 'mounted' | 'unmounted' | 'error' | 'warn' | 'pending'

/** 每个筛选档前面那枚小圆点用哪种颜色；`all` 不画点。 */
export const FILTER_DOT: Record<Exclude<FilterKey, 'all'>, string> = {
  mounted: 'sk-dot-ok',
  unmounted: 'sk-dot-muted',
  pending: 'sk-dot-pending',
  error: 'sk-dot-bad',
  warn: 'sk-dot-warn',
}

/**
 * 一行只属于一个状态档 —— 六个档是**分区**，不是可重叠的筛选条件。
 *
 * 早先每个胶囊各算一套条件（「已挂载」看 mounts、「警告」看 health），同一个
 * 技能会同时落在好几档里，各档数字相加比「全部」还大。所以这里定死优先级：
 * 硬伤 > 警告 > 已挂载 > 待收纳 > 未就绪。
 */
export function statusKey(row: s.LibraryRow): Exclude<FilterKey, 'all'> {
  const issues = row.health ?? []
  if (issues.some(item => item.level === 'bad')) return 'error'
  if (issues.length > 0) return 'warn'
  // 外部链接技能也是「客户端里真的能用」：链接就挂在渠道目录里
  if (row.origin === 'link' || (row.mounts ?? []).some(item => item.valid)) return 'mounted'
  if (!row.inManifest && row.origin !== 'hub-extra' && row.origin !== 'system') return 'pending'
  return 'unmounted'
}

export function matchFilter(row: s.LibraryRow, filter: FilterKey): boolean {
  return filter === 'all' || statusKey(row) === filter
}

/**
 * 一张卡片的状态：圆点配色 + 右上角徽标。
 * 优先级：已挂载（有硬伤也标红点）> 未登记/自带 > 待收纳 > 未挂载。
 */
export function cardStatus(row: s.LibraryRow): {
  dot: string
  label: string
  variant: 'success' | 'warning' | 'brand' | 'outline'
} {
  const mounts = row.mounts ?? []
  const valid = mounts.filter(item => item.valid)
  const hasBad = (row.health ?? []).some(item => item.level === 'bad')

  if (valid.length > 0) {
    return { dot: hasBad ? 'sk-dot-bad' : 'sk-dot-ok', label: t('已挂载'), variant: 'success' }
  }
  if (!row.inManifest) {
    if (row.origin === 'hub-extra') return { dot: 'sk-dot-warn', label: t('中央未登记'), variant: 'warning' }
    if (row.origin === 'system') return { dot: 'sk-dot-muted', label: t('客户端自带'), variant: 'outline' }
    // 别人中枢挂进来的链接：客户端能用（绿点），但真身不归本机台账管，故标注来路
    if (row.origin === 'link') return { dot: 'sk-dot-ok', label: t('外部链接'), variant: 'outline' }
    return { dot: hasBad ? 'sk-dot-bad' : 'sk-dot-pending', label: t('待收纳'), variant: 'brand' }
  }
  return { dot: hasBad ? 'sk-dot-bad' : 'sk-dot-muted', label: t('未挂载'), variant: 'outline' }
}

/** 卡片底部那排来源胶囊：优先用扫描到的来源工具，其次回落到台账号上的 tool。 */
export function sourceTools(row: s.LibraryRow): string[] {
  const seen = new Set<string>()
  const out: string[] = []
  for (const source of row.sources ?? []) {
    if (source.tool && !seen.has(source.tool)) { seen.add(source.tool); out.push(source.tool) }
  }
  if (out.length === 0 && row.tool && !seen.has(row.tool)) out.push(row.tool)
  return out
}

/** 「上次扫描」用的短时间：月-日 时:分，和参考图的 11-26 08:17 同形。 */
export function scanTime(timestamp: number | null | undefined): string {
  if (!timestamp || timestamp <= 0) return ''
  const date = new Date(timestamp)
  const pad = (value: number): string => String(value).padStart(2, '0')
  return `${pad(date.getMonth() + 1)}-${pad(date.getDate())} ${pad(date.getHours())}:${pad(date.getMinutes())}`
}

/**
 * 技能卡片网格。
 *
 * ── 为什么是按钮而不是 div + onClick ──────────────────────────
 * 整张卡片是可点区域，语义上就是一个按钮：键盘能 Tab 到、回车能开、读屏会
 * 报「按钮」。用 div 挂 onClick 这三样全丢，只能靠补 tabIndex + role + onKeyDown
 * 手工模仿。
 */
export function SkillCardGrid({
  rows,
  onOpen,
  footer = 'sources',
  badge = 'state',
  className,
}: {
  rows: s.LibraryRow[]
  onOpen: (name: string) => void
  /**
   * 卡片底部画什么。
   *
   * `sources` 是技能库的样式：一排来源工具胶囊，回答「这个技能是从哪些渠道
   * 收进来的」。仪表盘的速览网格用 `mounts`：只留「来源工具 + 挂载数」，
   * 因为那一格只有 180px 宽，排不下四个胶囊，而且用户在那儿更关心「挂到几
   * 个客户端了」。同一个技能在两处的状态判定仍是同一份 `cardStatus`。
   */
  footer?: 'sources' | 'mounts'
  /**
   * 右上角徽标的口径。
   *
   * `state`（默认）是技能库那六档：已挂载 / 待收纳 / 外部链接 / 客户端自带…
   * `ledger` 是 AgentHub 仪表盘速览卡的口径 —— 只问「台账里登记了没有」，
   * 对应 `inManifest ? 已收纳 : 待收纳`。仪表盘一屏 18 张卡，六档标签里的
   * 「外部链接 / 客户端自带」在这里没有决策价值：用户在那儿只想知道
   * 「这个技能到底收进中央没有」。
   */
  badge?: 'state' | 'ledger'
  className?: string
}): React.ReactElement {
  return (
    <div className={className ? `sk-skill-grid ${className}` : 'sk-skill-grid'}>
      {rows.map(row => {
        const status = cardStatus(row)
        const chips = sourceTools(row)
        const mounts = (row.mounts ?? []).filter(item => item.valid).length
        const key = `${row.origin}-${row.tool ?? ''}-${row.name}`
        const led = badge === 'ledger'
        const label = led ? (row.inManifest ? t('已收纳') : t('待收纳')) : status.label
        const variant = led ? (row.inManifest ? 'success' : 'brand') : status.variant
        const dot = led ? (row.inManifest ? 'sk-dot-ok' : 'sk-dot-pending') : status.dot
        return (
          <button
            key={key}
            type='button'
            className={`sk-skill-card${footer === 'mounts' ? ' is-compact' : ''}`}
            onClick={() => onOpen(row.name)}
          >
            <div className='sk-skill-head'>
              {footer === 'mounts' ? null : (
                <span className={`sk-dot sk-skill-dot ${dot}`} aria-hidden='true' />
              )}
              <span className='sk-skill-name' title={row.skillName || row.name}>{row.skillName || row.name}</span>
              <Badge className='sk-skill-badge' variant={variant} shape='tag'>{label}</Badge>
            </div>
            <div className='sk-skill-desc'>{row.description || t('（没有 description，Agent 触发不出这个技能）')}</div>
            {footer === 'mounts' ? (
              <div className='sk-skill-foot'>
                <span className={`sk-dot ${dot}`} aria-hidden='true' />
                <span className='sk-skill-tool'>{row.tool || chips[0] || t('中央')}</span>
                <span className='sk-skill-mounts'>{t('{n} 挂载', { n: mounts })} <span aria-hidden='true'>›</span></span>
              </div>
            ) : chips.length > 0 ? (
              <div className='sk-skill-sources'>
                {chips.slice(0, 4).map(tool => (
                  <span key={tool} className='sk-skill-source'>{tool}</span>
                ))}
                {chips.length > 4 ? <span className='sk-skill-source'>+{chips.length - 4}</span> : null}
              </div>
            ) : null}
          </button>
        )
      })}
    </div>
  )
}