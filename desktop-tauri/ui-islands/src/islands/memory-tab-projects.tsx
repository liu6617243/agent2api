import * as React from 'react'
import { Badge, Button, Input, Popover, PopoverContent, PopoverTrigger } from '@ui'
import { t } from '../i18n'
import * as m from './memory-api'
import { agentText, Empty, Panel, timeText } from './memory-shared'

/**
 * 项目页：一张表，一行一个项目。
 *
 * ── 为什么从卡片网格改成表格 ──────────────────────────────────
 * 项目之间要横向比较的东西很多：谁还活着、谁有远程、谁记忆多、谁在用。
 * 卡片把这些信息竖着摞起来，比较就得靠眼睛来回扫；表格把同一维度排成一列，
 * 扫一列就完事。项目数量一多（几十上百个仓库）差别尤其明显。
 *
 * ── 列的读法 ────────────────────────────────────────────────
 *   项目名称 / Slug —— 展示名 + 机器标识，slug 才是目录名，改名不影响它
 *   状态            —— 7 天内有更新算「活跃」，否则「静默」
 *   远程仓库        —— 有远程显示「查看 (N)」，没有显示「未记录远程」
 *   本地路径        —— 有本地目录显示「查看 (N)」，没有显示「未关联」
 *   记忆统计        —— 总条数 · L2 沉淀条数 · 最近更新
 *   关联 Agent      —— 写过这个项目记忆的写入方
 *   操作            —— 「查看记忆」是高频动作，其余收进 ⋯ 菜单
 *
 * ── 通用项目为什么不在表里 ────────────────────────────────────
 * 通用（`_global`）是**没有归属项目**的记忆的落脚点，条数通常远大于任何
 * 具体项目，名字固定、不可合并、不可归档。它不是一张可维护的卡，只在顶部
 * 统计条里出现 —— 后端 `memory::projects()` 已经把它单独汇总成 `general`。
 *
 * ── 合并为什么要二次确认 ────────────────────────────────────
 * 合并会**逐条搬文件**：记忆的 frontmatter 里 `project` 要改，`.md` 要移到新目录。
 * 这是这套系统里唯一会批量改文件路径的操作，点错一次要手工收拾。所以确认框
 * 里明写「会迁移 N 条」。
 */

/** 活跃窗口：7 天内有更新算活跃 */
const ACTIVE_WINDOW_MS = 7 * 24 * 60 * 60 * 1000

/** 写入方 → 界面显示名（`memory-shared` 里与浏览页共用一份，见 `agentText`）。 */
function agentNames(agents: string[] | undefined): string {
  return (agents ?? []).map(agentText).join(' · ')
}

type ProjectsProps = {
  projects: m.MemoryProject[]
  general: m.MemoryGeneral | null
  loading: boolean
  error: string
  onReload: () => void
  onOpenEntry: (id: string) => void
  onOpenProject: (slug: string) => void
  onChanged: () => void
}

function Projects({
  projects,
  general,
  loading,
  error,
  onReload,
  onOpenEntry,
  onOpenProject,
  onChanged,
}: ProjectsProps): React.ReactElement {
  const [query, setQuery] = React.useState('')
  const [renaming, setRenaming] = React.useState<string | null>(null)
  const [nameDraft, setNameDraft] = React.useState('')
  const [merging, setMerging] = React.useState<m.MemoryProject | null>(null)
  const [mergeInto, setMergeInto] = React.useState('')
  const [busy, setBusy] = React.useState('')
  const [localError, setLocalError] = React.useState('')
  const [showArchived, setShowArchived] = React.useState(false)

  const act = async (task: () => Promise<unknown>) => {
    setBusy('__all__')
    setLocalError('')
    try {
      await task()
      onReload()
      onChanged()
    } catch (caught) {
      setLocalError(caught instanceof Error ? caught.message : String(caught))
    } finally {
      setBusy('')
    }
  }

  const archived = projects.filter(item => item.archived).length

  /** 搜索：匹配名称、slug、远程与路径（都在项目名里出现过的字段） */
  const filtered = React.useMemo(() => {
    const keyword = query.trim().toLowerCase()
    return projects.filter(item => {
      if (!showArchived && item.archived) return false
      if (!keyword) return true
      const haystack = [
        item.name,
        item.slug,
        item.remote ?? '',
        item.path ?? '',
        ...(item.remotes ?? []),
      ]
        .join(' ')
        .toLowerCase()
      return haystack.includes(keyword)
    })
  }, [projects, query, showArchived])

  return (
    <div className='mm-projects-page'>
      {localError ? <div className='mm-error'>{localError}</div> : null}

      <div className='mm-projects-toolbar'>
        <Input
          className='mm-projects-search'
          value={query}
          placeholder={t('搜索项目')}
          onChange={event => setQuery(event.target.value)}
        />
        <span className='mm-chip'>{t('共 {n} 个项目', { n: projects.length })}</span>
        <span className='mm-chip'>{t('通用（general）{n} 条', { n: general?.count ?? 0 })}</span>
        <span className='mm-projects-help' title={t('一个 Git 项目一个文件夹，按远程地址跨机器归并')}>?</span>
        <span className='mm-projects-toolbar-right'>
          {archived > 0 ? (
            <Button type='button' size='sm' variant='ghost' onClick={() => setShowArchived(value => !value)}>
              {showArchived ? t('隐藏已归档（{n}）', { n: archived }) : t('显示已归档（{n}）', { n: archived })}
            </Button>
          ) : null}
          <Button type='button' size='sm' variant='ghost' onClick={onReload}>{t('刷新')}</Button>
        </span>
      </div>

      {loading ? <Panel loading error=''>{null}</Panel> : null}
      {error ? <Panel loading={false} error={error} onRetry={onReload}>{null}</Panel> : null}

      {!loading && !error && filtered.length === 0 ? (
        <Empty>
          {query
            ? t('没有匹配的项目')
            : t('还没有项目。让 Agent 带上项目路径写记忆，或手动记一条并选项目。')}
        </Empty>
      ) : null}

      {!loading && !error && filtered.length > 0 ? (
        <div className='mm-projects-table-wrap'>
          <table className='mm-projects-table'>
            <thead>
              <tr>
                <th className='mm-pt-name'>{t('项目名称 / Slug')}</th>
                <th className='mm-pt-status'>{t('状态')}</th>
                <th className='mm-pt-remote'>{t('远程仓库')}</th>
                <th className='mm-pt-path'>{t('本地路径')}</th>
                <th className='mm-pt-stats'>{t('记忆统计')}</th>
                <th className='mm-pt-agents'>{t('关联 Agent')}</th>
                <th className='mm-pt-actions'>{t('操作')}</th>
              </tr>
            </thead>
            <tbody>
              {filtered.map(item => {
                const active = item.updatedAt > Date.now() - ACTIVE_WINDOW_MS
                const agents = agentNames(item.agents)
                return (
                  <tr key={item.slug} className={item.archived ? 'archived' : ''}>
                    <td className='mm-pt-name'>
                      {renaming === item.slug ? (
                        <div className='mm-rename'>
                          <Input
                            value={nameDraft}
                            autoFocus
                            onChange={event => setNameDraft(event.target.value)}
                            onKeyDown={event => {
                              if (event.key === 'Enter') {
                                void act(async () => {
                                  await m.updateProject(item.slug, { name: nameDraft })
                                  setRenaming(null)
                                })
                              }
                              if (event.key === 'Escape') setRenaming(null)
                            }}
                          />
                          <Button type='button' size='sm' onClick={() => void act(async () => {
                            await m.updateProject(item.slug, { name: nameDraft })
                            setRenaming(null)
                          })}>{t('保存')}</Button>
                          <Button type='button' size='sm' variant='ghost' onClick={() => setRenaming(null)}>{t('取消')}</Button>
                        </div>
                      ) : (
                        <div className='mm-pt-name-cell'>
                          <span className='mm-pt-name-text' title={item.name}>{item.name}</span>
                          <span className='mm-pt-slug-text' title={item.slug}>{item.slug}</span>
                        </div>
                      )}
                      {item.pinned ? <span className='mm-pt-flag'>{t('置顶')}</span> : null}
                    </td>

                    <td className='mm-pt-status'>
                      <span className={`mm-pill${active ? ' ok' : ' muted'}`}>{active ? t('活跃') : t('静默')}</span>
                    </td>

                    <td className='mm-pt-remote'>
                      {item.remotes && item.remotes.length ? (
                        <span className='mm-pt-link' title={item.remotes.join('\n')}>
                          {t('查看 ({n})', { n: item.remotes.length })}
                        </span>
                      ) : item.remote ? (
                        <span className='mm-pt-link' title={item.remote}>{t('查看 (1)')}</span>
                      ) : (
                        <span className='mm-pill warn'>{t('未记录远程')}</span>
                      )}
                    </td>

                    <td className='mm-pt-path'>
                      {item.path ? (
                        <span className='mm-pt-link' title={item.path}>{t('查看 (1)')}</span>
                      ) : (
                        <span className='mm-pt-muted'>{t('未关联')}</span>
                      )}
                    </td>

                    <td className='mm-pt-stats'>
                      <div className='mm-pt-stats-cell'>
                        <span className='mm-mono'>{t('{n} 条', { n: item.count })}</span>
                        <span className='mm-pt-dot'>·</span>
                        {/* `item.l2` 已不含画像（后端与 `status()` 同口径）。
                            画像有就多一枚灰 pill：它确实属于这个项目，但不算沉淀，
                            混进 `L2:` 那个数字会让每次重生成画像都把这一格顶上去。 */}
                        <span className='mm-pill l2'>L2: {item.l2}</span>
                        {(item.profile ?? 0) > 0 ? (
                          <span
                            className='mm-pill muted'
                            title={t('画像：由深层记忆提炼的派生条目，不算沉淀')}
                          >
                            {t('画像 {n}', { n: item.profile ?? 0 })}
                          </span>
                        ) : null}
                        <span className='mm-pt-time'>{item.updatedText || timeText(item.updatedAt)}</span>
                      </div>
                    </td>

                    <td className='mm-pt-agents'>
                      <div className='mm-pt-ellipsis' title={agents || t('无关联 Agent')}>
                        {agents || '—'}
                      </div>
                    </td>

                    <td className='mm-pt-actions'>
                      <div className='mm-pt-actions-cell'>
                        <Button
                          type='button'
                          size='sm'
                          className='mm-cta'
                          onClick={() => onOpenProject(item.slug)}
                        >
                          {t('查看记忆')}
                        </Button>
                        <RowMenu
                          item={item}
                          busy={busy === item.slug}
                          onRename={() => {
                            setRenaming(item.slug)
                            setNameDraft(item.name)
                          }}
                          onTogglePin={() => void act(() => m.updateProject(item.slug, { pinned: !item.pinned }))}
                          onToggleArchive={() => void act(() => m.updateProject(item.slug, { archived: !item.archived }))}
                          onDistill={() => void act(async () => {
                            // 蒸馏是「花 token 的长任务」：后端同步跑完再返回，
                            // 期间整表置忙碌态。失败要把原因抛出来 —— 否则
                            // act() 只看到 resolve，界面会静默地什么都没发生。
                            const done = await m.runAutoTask('distill', item.slug)
                            if (done.outcome.status === 'failed') {
                              throw new Error(done.outcome.error || done.outcome.reason || t('蒸馏失败'))
                            }
                          })}
                          onMerge={() => {
                            setMerging(item)
                            setMergeInto(projects.find(other => other.slug !== item.slug)?.slug ?? '')
                          }}
                          onOpenLatest={() => {
                            const first = item.latest[0]
                            if (first) onOpenEntry(first.id)
                          }}
                          canMerge={projects.length > 1 && !item.archived}
                        />
                      </div>
                    </td>
                  </tr>
                )
              })}
            </tbody>
          </table>
        </div>
      ) : null}

      {merging ? (
        <div className='mm-dialog-mask' role='dialog' aria-modal='true'>
          <div className='mm-dialog mm-dialog-narrow'>
            <div className='mm-dialog-head'>
              <strong>{t('把「{name}」合并到另一个项目', { name: merging.name })}</strong>
              <Button type='button' size='sm' variant='ghost' onClick={() => setMerging(null)}>{t('取消')}</Button>
            </div>
            <div className='mm-dialog-body'>
              <p className='mm-hint'>
                {t('会把 {n} 条记忆的 project 字段与磁盘路径逐条改到目标项目下，然后旧项目卡标记为「已合并」。这个操作会移动文件。', { n: merging.count })}
              </p>
              <label className='mm-label'>{t('目标项目')}</label>
              <select className='mm-select' value={mergeInto} onChange={event => setMergeInto(event.target.value)}>
                {projects.filter(other => other.slug !== merging.slug).map(other => (
                  <option key={other.slug} value={other.slug}>{other.name}（{other.count}）</option>
                ))}
              </select>
            </div>
            <div className='mm-dialog-foot'>
              <Button type='button' variant='ghost' onClick={() => setMerging(null)}>{t('取消')}</Button>
              <Button
                type='button'
                variant='destructive'
                disabled={busy === '__all__' || !mergeInto}
                onClick={() => void act(async () => {
                  await m.mergeProject(merging.slug, mergeInto)
                  setMerging(null)
                })}
              >
                {busy === '__all__' ? t('迁移中…') : t('确认合并')}
              </Button>
            </div>
          </div>
        </div>
      ) : null}
    </div>
  )
}

/**
 * 行尾「⋯」菜单。
 *
 * 只有「查看记忆」是高频动作，其余都是低频维护项 —— 平铺五个按钮会把表格
 * 的操作列撑得很宽，也会让每行的注意力被分散。收进菜单是唯一能同时保住
 * 高频入口和表格密度的做法。
 */
function RowMenu({
  item,
  busy,
  onRename,
  onTogglePin,
  onToggleArchive,
  onDistill,
  onMerge,
  onOpenLatest,
  canMerge,
}: {
  item: m.MemoryProject
  busy: boolean
  onRename: () => void
  onTogglePin: () => void
  onToggleArchive: () => void
  onDistill: () => void
  onMerge: () => void
  onOpenLatest: () => void
  canMerge: boolean
}): React.ReactElement {
  const [open, setOpen] = React.useState(false)
  const close = () => setOpen(false)
  return (
    <Popover open={open} onOpenChange={setOpen}>
      <PopoverTrigger render={<Button type='button' size='sm' variant='ghost' className='mm-pt-more' title={t('更多操作')} />}>
        ⋯
      </PopoverTrigger>
      <PopoverContent align='end' sideOffset={4} className='mm-pt-menu'>
        <div className='flex flex-col gap-0.5'>
          <button type='button' className='mm-pt-menu-item' onClick={() => { close(); onDistill() }}>
            {t('蒸馏 L2')}
          </button>
          <button type='button' className='mm-pt-menu-item' onClick={() => { close(); onOpenLatest() }}>
            {t('打开最近一条')}
          </button>
          <button type='button' className='mm-pt-menu-item' onClick={() => { close(); onRename() }}>
            {t('重命名项目')}
          </button>
          <button type='button' className='mm-pt-menu-item' onClick={() => { close(); onTogglePin() }}>
            {item.pinned ? t('取消置顶') : t('置顶')}
          </button>
          <button type='button' className='mm-pt-menu-item' onClick={() => { close(); onToggleArchive() }}>
            {item.archived ? t('取消归档') : t('归档')}
          </button>
          {canMerge ? (
            <button type='button' className='mm-pt-menu-item' onClick={() => { close(); onMerge() }}>
              {t('合并到…')}
            </button>
          ) : null}
        </div>
      </PopoverContent>
    </Popover>
  )
}

export { Projects }
