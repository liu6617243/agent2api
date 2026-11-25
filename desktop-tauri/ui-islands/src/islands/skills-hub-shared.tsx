import * as React from 'react'
import { Button, Spinner } from '@ui'
import { t } from '../i18n'

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