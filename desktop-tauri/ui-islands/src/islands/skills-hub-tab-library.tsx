import * as React from 'react'
import { Badge, Button, SegmentedControl } from '@ui'
import { t } from '../i18n'
import * as s from './skills-hub-api'
import { Empty, HealthBadge, Panel, ago, toast, useLoad } from './skills-hub-shared'

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
 * 六个筛选正好对应六种处置状态：
 *   全部    —— 有多少东西
 *   已挂载  —— 真的在所有客户端里都能用了
 *   未挂载  —— 收进中央了，但没有任何客户端指向它（白收）
 *   待收纳  —— 还在渠道目录里，没进中央
 *   错误    —— 缺 SKILL.md / 缺 description（客户端不认）
 *   警告    —— 能跑但可能触发不出（缺 frontmatter / 描述超长）
 *
 * ── 为什么卡片上要显示 treeHash 前 8 位 ─────────────────────
 * 「这两个是不是同一份内容」是这一页最常被问的问题，而内容树哈希是唯一判据。
 * 全串 64 位太吵，前 8 位足以在几十个技能里肉眼比对（L1 合并时也是整串比对）。
 */

type Filter = 'all' | 'mounted' | 'unmounted' | 'pending' | 'error' | 'warn'

/** 六个筛选。label 在模块加载期取译文 —— 渲染时 `t(item.label)` 扫描器认不出键。 */
const FILTERS: { key: Filter; label: string }[] = [
  { key: 'all', label: t('全部') },
  { key: 'mounted', label: t('已挂载') },
  { key: 'unmounted', label: t('未挂载') },
  { key: 'pending', label: t('待收纳') },
  { key: 'error', label: t('错误') },
  { key: 'warn', label: t('警告') },
]

function match(row: s.LibraryRow, filter: Filter): boolean {
  const issues = row.health ?? []
  const mounts = row.mounts ?? []
  switch (filter) {
    case 'mounted':
      return mounts.some(item => item.valid)
    case 'unmounted':
      // 「未挂载」只对已经收进中央的技能有意义：待收纳的天生没有挂载记录，
      // 把它们也算进来会让这一档变成「全部」的同义词。
      return row.inManifest && !mounts.some(item => item.valid)
    case 'pending':
      return !row.inManifest && row.origin !== 'hub-extra' && row.origin !== 'system'
    case 'error':
      return issues.some(item => item.level === 'bad')
    case 'warn':
      return issues.length > 0 && !issues.some(item => item.level === 'bad')
    default:
      return true
  }
}

type Props = { onOpenSkill: (name: string) => void; onOpenSettings: () => void }

function Library({ onOpenSkill, onOpenSettings }: Props): React.ReactElement {
  const library = useLoad(() => s.loadLibrary(), [], { refreshOnEnter: true })
  const [filter, setFilter] = React.useState<Filter>('all')
  const [query, setQuery] = React.useState('')

  const items = library.data?.items ?? []
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

  return (
    <Panel loading={library.loading} error={library.error} onRetry={library.reload}>
      <div className='sk-lib'>
        <div className='sk-lib-bar'>
          <SegmentedControl
            options={FILTERS.map(item => ({
              value: item.key,
              label: item.label,
              count: counts[item.key],
            }))}
            value={filter}
            onValueChange={value => setFilter(value as Filter)}
            aria-label={t('技能状态筛选')}
          />
          <input
            className='sk-search'
            type='search'
            value={query}
            placeholder={t('搜技能名或描述…')}
            onChange={event => setQuery(event.target.value)}
          />
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
              const mounts = row.mounts ?? []
              const valid = mounts.filter(item => item.valid)
              const bad = mounts.filter(item => !item.valid)
              return (
                <button
                  key={`${row.origin}-${row.tool ?? ''}-${row.name}`}
                  type='button'
                  className='sk-skill-card'
                  onClick={() => onOpenSkill(row.name)}
                >
                  <div className='sk-skill-head'>
                    <span className='sk-skill-name'>{row.skillName || row.name}</span>
                    <HealthBadge issues={row.health} />
                  </div>
                  <div className='sk-skill-desc'>{row.description || t('（没有 description，Agent 触发不出这个技能）')}</div>
                  <div className='sk-skill-foot'>
                    <span className='sk-skill-tags'>
                      {row.inManifest ? (
                        <Badge variant='success' shape='tag'>{t('已收纳')}</Badge>
                      ) : row.origin === 'hub-extra' ? (
                        <Badge variant='warning' shape='tag'>{t('中央未登记')}</Badge>
                      ) : row.origin === 'system' ? (
                        <Badge variant='outline' shape='tag'>{t('客户端自带')}</Badge>
                      ) : (
                        <Badge variant='outline' shape='tag'>{t('待收纳')}</Badge>
                      )}
                      {valid.length > 0 ? (
                        <Badge variant='brand' shape='tag'>{t('挂载 {n}', { n: valid.length })}</Badge>
                      ) : null}
                      {bad.length > 0 ? (
                        <Badge variant='destructive' shape='tag'>{t('失效 {n}', { n: bad.length })}</Badge>
                      ) : null}
                    </span>
                    <span className='sk-skill-hash' title={row.treeHash || ''}>
                      {(row.treeHash ?? '').slice(0, 8) || '—'}
                    </span>
                  </div>
                  {(row.sources ?? []).length > 0 ? (
                    <div className='sk-skill-sources'>
                      {(row.sources ?? []).slice(0, 4).map((source, at) => (
                        <span key={`${source.tool}-${at}`} className='sk-skill-source'>{source.tool}</span>
                      ))}
                      {(row.sources ?? []).length > 4 ? (
                        <span className='sk-skill-source'>+{(row.sources ?? []).length - 4}</span>
                      ) : null}
                    </div>
                  ) : null}
                  {row.mtimeMs ? <div className='sk-skill-time'>{ago(row.mtimeMs)}</div> : null}
                </button>
              )
            })}
          </div>
        )}

        <div className='sk-lib-foot'>
          <span className='sk-muted'>
            {t('共 {n} 个技能；收纳后在渠道目录里只留指向中央的链接', { n: visible.length })}
          </span>
          <span className='sk-lib-foot-actions'>
            <Button type='button' size='sm' variant='ghost' onClick={onOpenSettings}>
              {t('配置渠道')}
            </Button>
            <Button
              type='button'
              size='sm'
              variant='outline'
              onClick={() => {
                void s.repairMounts()
                  .then(result => {
                    toast(t('修复 {n} 处挂载', { n: result.repaired }), result.repaired > 0 ? 'ok' : undefined)
                    library.reload()
                  })
                  .catch(error => toast(t('修复失败：{error}', { error: String(error) }), 'err'))
              }}
            >
              {t('修复失效挂载')}
            </Button>
          </span>
        </div>
      </div>
    </Panel>
  )
}

export { Library }