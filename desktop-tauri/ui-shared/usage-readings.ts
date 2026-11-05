type Translate = (key: string, params?: Record<string, string | number>) => string

type UsageObject = Record<string, unknown>

export type SnapshotReading = {
  name: string
  view: string
  tone: 'ok' | 'muted'
  daily: boolean
}

export type UsageAggregate = {
  label: string
  unit: string
  remaining: number | null
  unlimited: boolean
}

/** 汇总由提供商声明；积分包可能互相包含，不能用明细再次求和。 */
export function usageAggregateOf(value: unknown, t: Translate): UsageAggregate | null {
  const usage = object(value)
  if (!usage) return null
  const hasTotalLeft = Object.prototype.hasOwnProperty.call(usage, 'totalLeft')
  const sources = Array.isArray(usage.quotaSources) ? usage.quotaSources : []
  const account = sources.map(object).find(source => source?.id === 'account')
  const unit = String(usage.unit || account?.unit || (hasTotalLeft ? '积分' : '')).trim()
  const remaining = usageNumberOrNull(hasTotalLeft ? usage.totalLeft : usage.available)
  const unlimited = usage.unlimited === true
  if (remaining === null && !unlimited) return null
  return { label: isCreditUnit(unit) ? t('总积分') : t('可用合计'),
    unit: usageUnitLabel(unit, t), remaining, unlimited }
}

function object(value: unknown): UsageObject | null {
  return value && typeof value === 'object' && !Array.isArray(value) ? value as UsageObject : null
}

export function usageNumberOrNull(value: unknown): number | null {
  if (typeof value !== 'number' && typeof value !== 'string') return null
  if (typeof value === 'string' && !value.trim()) return null
  const number = Number(value)
  return Number.isFinite(number) ? number : null
}

/** 钱包当前余额不是套餐发放上限；旧版魔粒钱包没有 snapshot 标记。 */
export function isSnapshotWallet(value: unknown): boolean {
  const wallet = object(value)
  return wallet?.snapshot === true || wallet?.type === 'magicubes'
}

/** 旧版魔粒明细把钱包余额和已用次数都写成了 remain，兼容时只转换这一种钱包。 */
export function creditPacksOf(value: unknown): UsageObject[] {
  const usage = object(value)
  if (!usage) return []
  const packs = Array.isArray(usage.credits) ? usage.credits.filter(item => object(item)) as UsageObject[] : []
  const legacy = Array.isArray(usage.wallets) && usage.wallets.some(wallet => object(wallet)?.type === 'magicubes')
  if (!legacy) return packs
  return packs.map(pack => {
    if (pack.snapshot === true || pack.record === true) return pack
    const { remain, ...rest } = pack
    switch (pack.name) {
      case '可用魔粒':
        return { ...rest, key: 'available', name: '可用余额', balance: remain, unit: '魔粒', snapshot: true }
      case '冻结魔粒':
        return { ...rest, key: 'frozen', name: '冻结余额', balance: remain, unit: '魔粒', snapshot: true }
      case '今日已用（次）':
        return { ...rest, key: 'used-today', name: '今日已用', used: pack.used ?? remain, unit: '次', daily: true, snapshot: true }
      default:
        return pack
    }
  })
}

export function isCreditUnit(unit: string): boolean {
  return unit.trim() === '积分' || unit.trim().toLowerCase() === 'credits'
}

export function usageUnitLabel(unit: string, t: Translate): string {
  if (isCreditUnit(unit)) return t('积分')
  switch (unit) {
    case '额度': return t('额度')
    case '魔粒': return t('魔粒')
    case '次': return t('次')
    case 'copilot_quota_units': return t('Copilot 额度单位')
    default: return unit
  }
}

/** 上游 Copilot 的账单单位标签，不称为 Token 或货币。 */
export function usageWindowLabel(value: unknown, t: Translate): string {
  switch (value) {
    case 'Premium requests': return t('高级请求')
    case 'Chat requests': return t('对话请求')
    case 'Completions': return t('补全请求')
    default: return typeof value === 'string' ? t(value) : ''
  }
}

/** 网关生成的读数使用模板翻译；任意上游展示串只作文本，不反推余额。 */
export function usageWindowView(row: UsageObject, t: Translate, format: (value: number) => string): string {
  const supplied = typeof row.view === 'string' ? row.view.trim() : ''
  const remaining = usageNumberOrNull(row.remaining)
  const limit = usageNumberOrNull(row.limit)
  const ratio = usageNumberOrNull(row.remainingPercent)
  const unit = typeof row.unit === 'string' ? usageUnitLabel(row.unit, t) : ''
  if (row.unlimited === true && (!supplied || supplied === '不限')) return t('不限')
  if (remaining !== null && (!supplied || /^剩余 [-+\d.eE]+(?: \/ [-+\d.eE]+)?(?: .+)?$/.test(supplied))) {
    const text = limit === null ? t('剩 {remaining}', { remaining: format(remaining) })
      : t('剩 {remaining} / {total}', { remaining: format(remaining), total: format(limit) })
    return [text, unit].filter(Boolean).join(' ')
  }
  if (ratio !== null && (!supplied || /^剩余 [-+\d.]+%$/.test(supplied))) {
    return t('剩余 {percent}%', { percent: Math.round(Math.max(0, Math.min(100, ratio))) })
  }
  return supplied ? t(supplied) : '—'
}

/** snapshot 读数不表示一份积分包：不计算耗尽、到期或剩余比例。 */
export function snapshotReadingOf(pack: UsageObject, t: Translate, format: (value: number) => string): SnapshotReading | null {
  if (pack.snapshot !== true) return null
  const used = usageNumberOrNull(pack.used)
  const value = used ?? usageNumberOrNull(pack.balance) ?? usageNumberOrNull(pack.remain)
  const unit = String(pack.unit || '').trim()
  const rawName = String(pack.name || '').trim()
  const name = rawName === '可用余额' ? t('可用余额')
    : rawName === '冻结余额' ? t('冻结余额')
      : rawName === '今日已用' ? t('今日已用')
        : rawName === '今日发放' ? t('今日发放')
          : rawName || t('明细')
  const view = value === null ? '—'
    : unit === '次' ? t('{count} 次', { count: format(value) })
      : unit ? `${format(value)} ${usageUnitLabel(unit, t)}` : format(value)
  return {
    name,
    view,
    tone: used !== null || pack.key === 'frozen' || value === null || value <= 0 ? 'muted' : 'ok',
    daily: pack.daily === true,
  }
}

export function snapshotReadingsOf(value: unknown, t: Translate, format: (value: number) => string): SnapshotReading[] {
  return creditPacksOf(value)
    .map(pack => snapshotReadingOf(pack, t, format))
    .filter((reading): reading is SnapshotReading => reading !== null)
}

export function creditStatisticsWalletsOf(value: unknown): UsageObject[] | null {
  const usage = object(value)
  if (!Array.isArray(usage?.wallets)) return null
  if (!usage.wallets.some(wallet => String(object(wallet)?.type || '').startsWith('meter:'))) return null
  return usage.wallets.map(object).filter((wallet): wallet is UsageObject => wallet !== null
    && (wallet.creditRemaining != null || wallet.creditTotal != null || wallet.creditUsed != null))
}

export function usageSupplementReadingsOf(value: unknown, t: Translate, format: (value: number) => string): string[] {
  const data = object(value)
  if (!data) return []
  const readings: string[] = []
  const subscription = object(data.subscription)
  if (typeof subscription?.planName === 'string' && subscription.planName.trim()) {
    readings.push(t('套餐 {name}', { name: subscription.planName }))
  }
  if (typeof data.balanceView === 'string' && data.balanceView.trim()) readings.push(t(data.balanceView))
  for (const wallet of creditStatisticsWalletsOf(data) ?? (Array.isArray(data.wallets) ? data.wallets : [])) {
    const row = object(wallet)
    if (!row) continue
    const view = typeof row.balanceView === 'string' && row.balanceView.trim() ? t(row.balanceView)
      : usageNumberOrNull(row.balance) === null ? '' : format(usageNumberOrNull(row.balance)!)
    if (!view) continue
    const name = typeof row.displayName === 'string' ? t(row.displayName)
      : typeof row.type === 'string' ? row.type : t('明细')
    readings.push([name, view].filter(Boolean).join(' '))
  }
  return [...new Set(readings)]
}

export function usageWarningsOf(value: unknown): string[] {
  const usage = object(value)
  return Array.isArray(usage?.warnings)
    ? usage.warnings.filter((item): item is string => typeof item === 'string' && Boolean(item.trim()))
    : []
}

export function recordReadingsOf(
  value: unknown,
  t: Translate,
  format: (value: number) => string,
  time: (value: number) => string,
): string[] {
  return creditPacksOf(value).filter(pack => pack.record === true).map(pack => {
    const amount = usageNumberOrNull(pack.amount)
    const unit = String(pack.unit || '').trim()
    const view = amount === null ? '—' : `${amount > 0 ? '+' : ''}${format(amount)}${unit ? ` ${usageUnitLabel(unit, t)}` : ''}`
    const expireAt = usageNumberOrNull(pack.expireAt)
    const window = expireAt !== null && expireAt > 0
      ? expireAt <= Date.now() ? t('已过期') : t('到期 {time}', { time: time(expireAt) })
      : ''
    return [String(pack.name || '').trim(), view, window].filter(Boolean).join(' · ')
  })
}

/**
 * **最早一份「还能用」的积分包的到期时刻**（毫秒）；读不到就是 null。
 *
 * ── 谁在用 ──────────────────────────────────────────────────
 * 桌面端账号页与手机端的「有效期」列（申报了 `creditExpiry` 的家），以及桌面端
 * 「按积分到期排序」。三处必须算出同一个答案，所以实现只此一份：桌面端的
 * `creditExpiryOf` 与后端的 `account_order::credit_expiries` 都照着这份口径。
 *
 * ── 「还能用」的两个条件 ────────────────────────────────────
 *   · **剩余不为 0**：花完的包不该再提示；
 *   · **未过期**：已作废的包按上游口径已经清零，拿它算「距到期还有多久」是假消息。
 *
 * ── 哪些行不参与 ────────────────────────────────────────────
 * `snapshot`（钱包余额 / 计数器快照）、`record`（收支流水一笔）、`daily`（每日
 * 回满，它的 expireAt 是周期边界而非作废时刻）、`unlimited`（没有花完的概念）。
 * 判据走**后端给的标记**，与 `creditRowsOf` 的展示判据同源。
 *
 * 「剩余读不到」时不猜：只有明确读到 0 才排除（与 creditRowsOf 的 exhausted 同一取向）。
 */
export function creditExpiryOfUsage(value: unknown, now = Date.now()): number | null {
  return creditExpiryBatchOfUsage(value, now)?.expireAt ?? null
}

/**
 * 最先到期的「还能用」积分批次 —— 到期时刻 + **该批剩余多少** + 单位。
 *
 * ── 为什么把「这一批有多少分」一起带出来 ───────────────────────
 * 「有效期」列原本只回答「什么时候过期」。但用户真正要判断的是
 * 「**有多少钱要没了**」—— 一个 3 天后过期、只剩 20 分的批次，与一个
 * 3 天后过期、还剩 8,000 分的批次，处置方式完全不同。所以到期时刻与
 * 剩余积分必须成对出现，不能只给时刻。
 *
 * ── 剩余数从哪来 ──────────────────────────────────────────────
 * 批次行自带的 `remain`（与 `total` / `used` 同一组）。顶层 `available`
 * 是**全部批次之和**，不是这一批 —— 用它会把「总余额」当成「要过期的那批」，
 * 数字看着大、其实无关。
 *
 * ── 单位：批次自己的优先，顶层兜底 ─────────────────────────────
 * 批次行有的自带 `unit`（如 minimax 的「积分」），有的没有（workbuddy 的批次
 * 只有 name/remain/expireAt）→ 顶层 `unit` 兜底；两处都没有就留空，由调用方
 * 决定是省略单位还是写「分」。
 *
 * ── 与 {@link creditExpiryOfUsage} 的口径必须一致 ────────────────
 * 判据完全同一份（跳过流水/快照/每日重置/不限量、排除已过期与剩余为 0 的批次），
 * 只是多带两个字段。两者任何时候都要选出**同一批** —— 所以下面让
 * `creditExpiryOfUsage` 变成这个函数的薄封装，而不是各写一遍遍历。
 */
export type CreditExpiryBatch = {
  /** 该批次的作废时刻（毫秒） */
  expireAt: number
  /** 该批次的剩余积分；读不到就是 `null`（不猜） */
  remain: number | null
  /** 该批次的单位；批次与顶层都没给就是空串 */
  unit: string
  /** 该批次的名称（同一时刻可能有多个批次同名），用于 title */
  name: string
}

/**
 * 选出最先到期的「还能用」积分批次。
 *
 * 判据与 {@link creditExpiryOfUsage} 完全一致（那里有逐条口径说明），
 * 这里只多带出该批次的剩余积分与单位。
 */
export function creditExpiryBatchOfUsage(value: unknown, now = Date.now()): CreditExpiryBatch | null {
  let best: CreditExpiryBatch | null = null
  for (const pack of creditPacksOf(value)) {
    if (pack.snapshot === true || pack.record === true || pack.daily === true || pack.unlimited === true) continue
    const expire = usageNumberOrNull(pack.expireAt)
    if (expire === null || expire <= 0 || expire <= now) continue
    const remain = usageNumberOrNull(pack.remain)
    if (remain !== null && remain <= 0) continue
    if (best !== null && expire >= best.expireAt) continue
    best = {
      expireAt: expire,
      remain,
      // **只放批次自己的单位**：顶层单位的归一口径（`totalLeft → 积分`、
      // `credits → 积分`）在 `credit-expiry.ts` 的 `creditExpiryUnitOf` 里，
      // 由调用方做「批次没有就用顶层」的兜底 —— 两边各填一次必然走偏。
      unit: typeof pack.unit === 'string' ? pack.unit.trim() : '',
      name: typeof pack.name === 'string' ? pack.name : '',
    }
  }
  return best
}
