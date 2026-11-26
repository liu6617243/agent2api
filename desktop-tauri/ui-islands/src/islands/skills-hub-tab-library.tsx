import * as React from 'react'
import { Badge, Button } from '@ui'
import { t } from '../i18n'
import * as s from './skills-hub-api'
import { Empty, Panel, toast, useLoad } from './skills-hub-shared'

/**
 * 技能仓库 · 技能库。
 *
 * ── 这一页的列表为什么把「扫描结果」与「台账」并起来 ────────
 * 收纳之后，工具目录里只剩链接，扫描层不再把那个技能当「技能行」产出 —— 只读
 * 扫描结果的话，收纳一次技能它就从列表里消失了。所以后端 `library()` 两源合并：
 * 扫描到的（待收纳 / 冲突中）+ 台账里的（已收纳，含没挂载的）。
 *
 * ── 为什么筛选是六个而不是「搜索 + 排序」──────────────────
 * 这一页的用途不是「找某个技能」（那是搜索框的事），而是「看哪些没在受管状态」。
 * 六个档位正好对应六种处置状态，且**互斥**（数字相加 = 全部）：
 *   全部    —— 有多少东西
 *   已就绪  —— 真的在所有客户端里都能用了（含外部链接挂进来的）
 *   未就绪  —— 收进中央了，但没有任何客户端指向它（白收）
 *   错误    —— 缺 SKILL.md / 缺 description（客户端不认）
 *   警告    —— 能跑但可能触发不出（缺 frontmatter / 描述超长）
 *   待收纳  —— 还在渠道目录里，没进中央
 *
 * ── 版式为什么要「彩色圆点 + 单一状态徽标」──────────────────
 * 早先把「已收纳 / 挂载 N / 失效 N / 指纹前 8 位 / 来源」五六枚标签全塞在卡片
 * 底部，扫一眼读不出「这个技能现在到底能不能用」。现在把状态收到一处：左边一
 * 枚圆点（绿=已挂载、蓝=待收纳、灰=未挂载、红=有硬伤），右上角一枚徽标写明，
 * 其余细节（指纹、来源、时间）留给详情抽屉 —— 卡片只回答「受管到哪一步」。
 */

type Filter = 'all' | 'mounted' | 'unmounted' | 'error' | 'warn' | 'pending'

/** 六个筛选。label 在模块加载期取译文 —— 渲染时 `t(item.label)` 扫描器认不出键。 */
const FILTERS: { key: Filter; label: string }[] = [
  { key: 'all', label: t('全部') },
  { key: 'mounted', label: t('已就绪') },
  { key: 'unmounted', label: t('未就绪') },
  { key: 'error', label: t('错误') },
  { key: 'warn', label: t('警告') },
  { key: 'pending', label: t('待收纳') },
]

/** 每个筛选胶囊前面那枚小圆点用哪种颜色；`全部` 不画点。 */
const FILTER_DOT: Record<Exclude<Filter, 'all'>, string> = {
  mounted: 'sk-dot-ok',
  unmounted: 'sk-dot-muted',
  pending: 'sk-dot-pending',
  error: 'sk-dot-bad',
  warn: 'sk-dot-warn',
}

/**
 * 一行只属于一个状态档 —— 六个胶囊是**分区**，不是可重叠的筛选条件。
 *
 * 早先每个胶囊各算一套条件（「已挂载」看 mounts、「警告」看 health），同一个技能
 * 会同时落在好几档里，各档数字相加比「全部」还大。参考页的计数是相加等于全部的，
 * 所以这里定死优先级：硬伤 > 警告 > 已挂载 > 待收纳 > 未就绪。
 */
function statusKey(row: s.LibraryRow): Exclude<Filter, 'all'> {
  const issues = row.health ?? []
  if (issues.some(item => item.level === 'bad')) return 'error'
  if (issues.length > 0) return 'warn'
  // 外部链接技能也是「客户端里真的能用」：链接就挂在渠道目录里
  if (row.origin === 'link' || (row.mounts ?? []).some(item => item.valid)) return 'mounted'
  if (!row.inManifest && row.origin !== 'hub-extra' && row.origin !== 'system') return 'pending'
  return 'unmounted'
}

function match(row: s.LibraryRow, filter: Filter): boolean {
  return filter === 'all' || statusKey(row) === filter
}

/**
 * 一张卡片的状态：圆点配色 + 右上角徽标。
 * 优先级：已挂载（有硬伤也标红点）> 未登记/自带 > 待收纳 > 未挂载。
 */
function cardStatus(row: s.LibraryRow): { dot: string; label: string; variant: 'success' | 'warning' | 'brand' | 'outline' } {
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
function sourceTools(row: s.LibraryRow): string[] {
  const seen = new Set<string>()
  const out: string[] = []
  for (const source of row.sources ?? []) {
    if (source.tool && !seen.has(source.tool)) { seen.add(source.tool); out.push(source.tool) }
  }
  if (out.length === 0 && row.tool && !seen.has(row.tool)) out.push(row.tool)
  return out
}

/** 「上次扫描」用的短时间：月-日 时:分，和参考图的 11-26 08:17 同形。 */
function scanTime(timestamp: number | null | undefined): string {
  if (!timestamp || timestamp <= 0) return ''
  const date = new Date(timestamp)
  const pad = (value: number): string => String(value).padStart(2, '0')
  return `${pad(date.getMonth() + 1)}-${pad(date.getDate())} ${pad(date.getHours())}:${pad(date.getMinutes())}`
}

type Props = { onOpenSkill: (name: string) => void; onOpenSettings: () => void }

function Library({ onOpenSkill, onOpenSettings }: Props): React.ReactElement {
  const library = useLoad(() => s.loadLibrary(), [], { refreshOnEnter: true })
  const check = useLoad(() => s.loadDailyCheck(), [], { refreshOnEnter: true })
  const [filter, setFilter] = React.useState<Filter>('all')
  const [query, setQuery] = React.useState('')
  const [refreshing, setRefreshing] = React.useState(false)
  const [repairing, setRepairing] = React.useState(false)

  const items = library.data?.items ?? []
  const last = check.data?.last ?? null
  const counts = {
    all: items.length,
    mounted: items.filter(row => match(row, 'mounted')).length,
    unmounted: items.filter(row => match(row, 'unmounted')).length,
    pending: items.filter(row => match(row, 'pending')).length,
    error: items.filter(row => match(row, 'error')).length,
    warn: items.filter(row => match(row, 'warn')).length,
  }

  const visible = items
    .filter(row => match(row, filter))
    .filter(row => {
      const needle = query.trim().toLowerCase()
      if (!needle) return true
      return (
        row.name.toLowerCase().includes(needle)
        || (row.skillName ?? '').toLowerCase().includes(needle)
        || (row.description ?? '').toLowerCase().includes(needle)
      )
    })

  // 「立即刷新」不只是重读缓存，而是真触发一次扫描（`runDailyCheck` 就是给
  // 「上次扫描」时间戳供数的那条链路）；扫完把列表与状态一起重拉。
  const refreshNow = (): void => {
    setRefreshing(true)
    void s.runDailyCheck(false)
      .then(result => {
        toast(result.summary, result.ok ? 'ok' : 'err')
      })
      .catch(error => toast(t('刷新失败：{error}', { error: String(error) }), 'err'))
      .finally(() => {
        setRefreshing(false)
        library.reload()
        check.reload()
      })
  }

  const repair = (): void => {
    setRepairing(true)
    void s.repairMounts()
      .then(result => {
        toast(t('修复 {n} 处挂载', { n: result.repaired }), result.repaired > 0 ? 'ok' : undefined)
        library.reload()
        check.reload()
      })
      .catch(error => toast(t('修复失败：{error}', { error: String(error) }), 'err'))
      .finally(() => setRepairing(false))
  }

  return (
    <Panel loading={library.loading} error={library.error} onRetry={library.reload}>
      <div className='sk-lib'>
        {/* ── 扫描状态行：上一轮扫描时间 + 两个动作 ── */}
        <div className='sk-lib-status'>
          <span className='sk-lib-status-text'>
            <svg className='sk-lib-status-icon' viewBox='0 0 24 24' fill='none' stroke='currentColor' strokeWidth='2' strokeLinecap='round' strokeLinejoin='round' aria-hidden='true'>
              <circle cx='12' cy='12' r='9' />
              <path d='M12 7v5l3 2' />
            </svg>
            {last
              ? t('上次扫描 {time}', { time: scanTime(last.checkedAt) })
              : t('尚未扫描')}
          </span>
          <span className='sk-lib-status-actions'>
            <Button type='button' size='sm' variant='ghost' onClick={onOpenSettings}>
              {t('配置渠道')}
            </Button>
            <Button type='button' size='sm' variant='outline' disabled={repairing} onClick={repair}>
              {repairing ? t('修复中…') : t('修复挂载')}
            </Button>
            <Button type='button' size='sm' variant='default' disabled={refreshing} onClick={refreshNow}>
              {refreshing ? t('扫描中…') : t('立即刷新')}
            </Button>
          </span>
        </div>

        {/* ── 工具栏：搜索在左、带色点计数的筛选胶囊在右 ── */}
        <div className='sk-lib-bar'>
          <label className='sk-search-wrap'>
            <svg className='sk-search-icon' viewBox='0 0 24 24' fill='none' stroke='currentColor' strokeWidth='2' strokeLinecap='round' strokeLinejoin='round' aria-hidden='true'>
              <circle cx='11' cy='11' r='7' />
              <path d='m20 20-3.2-3.2' />
            </svg>
            <input
              className='sk-search'
              type='search'
              value={query}
              placeholder={t('搜索技能名称或描述，例如 gsap、设计、部署')}
              onChange={event => setQuery(event.target.value)}
            />
          </label>
          <div className='sk-filter-pills' role='tablist' aria-label={t('技能状态筛选')}>
            {FILTERS.map(item => (
              <button
                key={item.key}
                type='button'
                role='tab'
                aria-selected={filter === item.key}
                className={`sk-filter-pill${filter === item.key ? ' is-active' : ''}`}
                onClick={() => setFilter(item.key)}
              >
                {item.key === 'all' ? null : <span className={`sk-dot ${FILTER_DOT[item.key]}`} />}
                <span className='sk-filter-label'>{item.label}</span>
                <span className='sk-filter-count'>{counts[item.key]}</span>
              </button>
            ))}
          </div>
        </div>

        {visible.length === 0 ? (
          <Empty>
            {items.length === 0
              ? t('技能库里还没有任何技能。去配置页接上客户端的技能目录，然后同步一次。')
              : t('没有符合筛选的技能')}
          </Empty>
        ) : (
          <div className='sk-skill-grid'>
            {visible.map(row => {
              const status = cardStatus(row)
              const chips = sourceTools(row)
              return (
                <button
                  key={`${row.origin}-${row.tool ?? ''}-${row.name}`}
                  type='button'
                  className='sk-skill-card'
                  onClick={() => onOpenSkill(row.name)}
                >
                  <div className='sk-skill-head'>
                    <span className={`sk-dot sk-skill-dot ${status.dot}`} aria-hidden='true' />
                    <span className='sk-skill-name' title={row.skillName || row.name}>{row.skillName || row.name}</span>
                    <Badge className='sk-skill-badge' variant={status.variant} shape='tag'>{status.label}</Badge>
                  </div>
                  <div className='sk-skill-desc'>{row.description || t('（没有 description，Agent 触发不出这个技能）')}</div>
                  {chips.length > 0 ? (
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
        )}
      </div>
    </Panel>
  )
}

export { Library }
