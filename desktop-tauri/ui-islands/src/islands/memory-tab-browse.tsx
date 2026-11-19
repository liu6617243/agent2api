import * as React from 'react'
import { Badge, Button, Input, Textarea } from '@ui'
import { t } from '../i18n'
import * as m from './memory-api'
import { agentText, Empty, num, Panel, Section, stamp, StateBadge, timeText, fullTime, useLoad } from './memory-shared'

/**
 * 记忆浏览：四个视图（列表 / 热力图 / 待确认 / 回收站）+ 常显筛选条 + 详情抽屉。
 *
 * ── 为什么是「表格」而不是「卡片流 + 右栏详情」──────────────
 * 记忆是**同一批字段的多条记录**：时间、层级、项目、Agent、标记、标签。卡片把
 * 这些竖着摞进每一格，横向比较（「这个项目最近写了几条」「哪个 Agent 写得最多」）
 * 就得靠眼睛来回扫；表格把同一维度排成一列，扫一列就完事。原来那条右栏详情
 * 也一起搬到抽屉里 —— 详情占半屏时，列表只剩一半宽度，列一多就挤成省略号。
 *
 * ── 为什么筛选行常显、不做「展开」按钮 ──────────────────────
 * 日常 90% 的操作都落在这一行的四个控件上。收起来省的那 30px，换来的是每次
 * 筛选都多点一次、且用户找不到入口。低频的三个开关（标签 / 收藏 / 置顶）
 * 才收进「更多筛选」。
 *
 * ── 为什么等级（L1/L2）单独一行而不并进「类型」下拉 ────────
 * 层级与类型是两套正交的分类：类型讲「这是决策还是流水」，层级讲「这是原始记录
 * 还是蒸馏结论」。混进同一个下拉，用户会以为它们互斥。
 *
 * ── 为什么「待确认」是独立视图 ─────────────────────────────
 * 裁决一条建议（「这条重复了」「这条该归到 X 项目」）必须能同时看到建议和
 * 被建议的那条记忆。放在列表上方折叠区时，一屏里既挤列表又挤队列，两边都不好用。
 */

/** 每页条数。50 是「一屏能滚完、翻页又不至于太频繁」的折中。 */
const PAGE_SIZE = 50
/** 热力图跨度（天）。一年是这类图的标准窗口（GitHub 同款）。 */
const HEAT_DAYS = 365

type View = 'list' | 'heatmap' | 'review' | 'trash'

const VIEWS: { key: View; label: string }[] = [
  { key: 'list', label: '列表' },
  { key: 'heatmap', label: '热力图' },
  { key: 'review', label: '待确认' },
  { key: 'trash', label: '回收站' },
]

const LEVELS: { key: '' | 'l1' | 'l2'; label: string; hint: string }[] = [
  { key: '', label: '全部', hint: '' },
  {
    key: 'l1',
    label: '普通',
    hint: 'L1：Agent 日常写入的流水与笔记（会话摘要、每日记录、手写笔记）。量大、粒度细，是记忆的主体。',
  },
  {
    key: 'l2',
    label: '深层',
    hint: 'L2：由自动化蒸馏出的知识、决策与术语表 —— 把多条原始记忆压缩成的长期结论。条数少但信息密度高。',
  },
]

type BrowseProps = {
  kinds: m.MemoryKindOption[]
  projects: m.MemoryProject[]
  onChanged: () => void
  /** 外部要求打开某条（项目页点标题跳过来） */
  focusId?: string | null
  onFocusConsumed?: () => void
  /** 外部要求按某个项目筛选（项目页「查看记忆」跳过来） */
  focusProject?: string | null
  onFocusProjectConsumed?: () => void
}

function Browse({
  kinds,
  projects,
  onChanged,
  focusId,
  onFocusConsumed,
  focusProject,
  onFocusProjectConsumed,
}: BrowseProps): React.ReactElement {
  const [view, setView] = React.useState<View>('list')
  const [level, setLevel] = React.useState<'' | 'l1' | 'l2'>('')
  const [query, setQuery] = React.useState('')
  /** 已提交的搜索词。回车/失焦才提交 —— 每敲一个字发一次请求会在输入中文时
   *  把半个拼音送去检索，结果闪来闪去。 */
  const [submitted, setSubmitted] = React.useState('')
  const [project, setProject] = React.useState('')
  const [agent, setAgent] = React.useState('')
  const [kind, setKind] = React.useState('')
  const [tag, setTag] = React.useState('')
  const [starred, setStarred] = React.useState(false)
  const [pinned, setPinned] = React.useState(false)
  const [includeSuperseded, setIncludeSuperseded] = React.useState(false)
  const [moreOpen, setMoreOpen] = React.useState(false)
  const [page, setPage] = React.useState(0)
  /** Agent 下拉的可选值：后端没有「列出所有写入方」的端点，所以随翻页累积。
   *  首次进入只有本页出现过的几个，滚两页就齐了 —— 比多开一个端点划算。 */
  const [agentOptions, setAgentOptions] = React.useState<string[]>([])
  const [selected, setSelected] = React.useState<m.MemoryEntry | null>(null)
  const [timeline, setTimeline] = React.useState<m.MemoryTimelineItem[]>([])
  const [draft, setDraft] = React.useState<m.WritePayload | null>(null)
  const [error, setError] = React.useState('')
  const [busy, setBusy] = React.useState('')

  const trashMode = view === 'trash'

  const list = useLoad(
    () => m.loadList({
      query: submitted.trim(),
      type: kind,
      project: project.trim(),
      agent: agent || undefined,
      layer: level,
      tag: tag || undefined,
      starred: starred || undefined,
      pinned: pinned || undefined,
      // 回收站要连「已失效」一起看：那些条目在真实数据里同样要能恢复
      includeSuperseded: includeSuperseded || trashMode,
      includeDeleted: trashMode,
      limit: PAGE_SIZE,
      offset: page * PAGE_SIZE,
    }),
    [submitted, kind, project, agent, level, tag, starred, pinned, includeSuperseded, trashMode, page],
  )

  const total = list.data?.total ?? 0
  const pages = Math.max(1, Math.ceil(total / PAGE_SIZE))

  const review = useLoad(() => m.loadReview('pending', undefined, 50), [])

  const heat = useLoad<m.MemoryHeat | null>(
    () => (view === 'heatmap' ? m.loadHeat(HEAT_DAYS) : Promise.resolve(null)),
    [view],
  )

  /** 热力图里点中的那一天（`YYYY-MM-DD`）。null = 没点，展开区收起。 */
  const [dayPick, setDayPick] = React.useState<string | null>(null)

  /** 那一天的写入时间窗（毫秒，左闭右开）。
   *
   * 为什么在前端算而不在后端切天：切天依赖**用户本地时区**，服务端不知道
   * 浏览器在哪个时区，跨零点那条会被切到错的一天。前端按本地日历算出
   * `[00:00, 次日 00:00)` 两个端点传过去，边界最准。 */
  const dayRange = React.useMemo(() => {
    if (!dayPick) return null
    const from = new Date(`${dayPick}T00:00:00`).getTime()
    return { from, to: from + 86_400_000 }
  }, [dayPick])

  const dayList = useLoad<m.MemoryList | null>(
    () => (dayRange
      ? m.loadList({ from: dayRange.from, to: dayRange.to, limit: 50 })
      : Promise.resolve(null)),
    [dayRange?.from, dayRange?.to],
  )

  // 把本页出现过的 Agent 累加进下拉候选（去重、保持出现顺序）
  React.useEffect(() => {
    const seen = list.data?.items.map(item => item.agent).filter((value): value is string => Boolean(value)) ?? []
    if (!seen.length) return
    setAgentOptions(current => {
      const merged = [...current]
      for (const value of seen) if (!merged.includes(value)) merged.push(value)
      return merged.length === current.length ? current : merged
    })
  }, [list.data])

  const openEntry = React.useCallback(async (id: string) => {
    setBusy(id)
    setError('')
    try {
      const payload = await m.loadEntry(id)
      const entry = payload.items[0] ?? null
      setSelected(entry)
      setTimeline([])
      if (entry) {
        const chain = await m.loadTimeline(entry.id)
        setTimeline(chain.items)
      }
    } catch (caught) {
      setError(caught instanceof Error ? caught.message : String(caught))
    } finally {
      setBusy('')
    }
  }, [])

  // 项目页点标题跳过来时，直接打开那条（并切回列表视图 —— 停在热力图上看不到它）
  React.useEffect(() => {
    if (!focusId) return
    setView('list')
    void openEntry(focusId)
    onFocusConsumed?.()
  }, [focusId, openEntry, onFocusConsumed])

  // 项目页「查看记忆」跳过来时，把项目筛选落到下拉框上
  //
  // 消费后立刻清掉外层的 `focusProject`，否则用户手动换一个项目，下一次
  // 任何重渲染都会把它顶回旧值 —— 筛选框看起来「不听话」。
  React.useEffect(() => {
    if (!focusProject) return
    setView('list')
    setProject(focusProject)
    setPage(0)
    onFocusProjectConsumed?.()
  }, [focusProject, onFocusProjectConsumed])

  const saveDraft = async () => {
    if (!draft) return
    if (!draft.content.trim()) {
      setError(t('正文不能为空'))
      return
    }
    setBusy('__save__')
    setError('')
    try {
      await m.writeEntry(draft)
      setDraft(null)
      list.reload()
      review.reload()
      onChanged()
    } catch (caught) {
      setError(caught instanceof Error ? caught.message : String(caught))
    } finally {
      setBusy('')
    }
  }

  const act = async (id: string, task: () => Promise<unknown>) => {
    setBusy(id)
    setError('')
    try {
      await task()
      list.reload()
      review.reload()
      onChanged()
    } catch (caught) {
      setError(caught instanceof Error ? caught.message : String(caught))
    } finally {
      setBusy('')
    }
  }

  /** 改筛选都要回到第一页：停在第 5 页换项目，多半会落在空页上。 */
  const patchFilter = (apply: () => void) => {
    apply()
    setPage(0)
  }

  const activeFilters = [project, agent, kind, tag].filter(Boolean).length
    + (starred ? 1 : 0) + (pinned ? 1 : 0) + (includeSuperseded ? 1 : 0) + (level ? 1 : 0)

  const resetFilters = () => {
    setProject('')
    setAgent('')
    setKind('')
    setTag('')
    setStarred(false)
    setPinned(false)
    setIncludeSuperseded(false)
    setLevel('')
    setQuery('')
    setSubmitted('')
    setPage(0)
  }

  const switchView = (next: View) => {
    setView(next)
    setPage(0)
  }

  const viewIndex = VIEWS.findIndex(item => item.key === view)
  const levelIndex = LEVELS.findIndex(item => item.key === level)

  return (
    <div className='mm-browse'>
      {error ? <div className='mm-error'>{error}</div> : null}

      {/* ── 视图切换行：新建按钮 + 四段分段控件 ───────────────── */}
      <div className='mm-browse-head'>
        <Button
          type='button'
          size='sm'
          className='mm-cta-solid'
          onClick={() => setDraft({ content: '', type: 'daily', project: project || undefined })}
        >
          ＋ {t('新增一条记忆')}
        </Button>
        <div className='mm-switch is-4' style={{ ['--sw-i' as string]: viewIndex }} role='tablist'>
          <span className='mm-switch-thumb' />
          {VIEWS.map(item => (
            <button
              key={item.key}
              type='button'
              role='tab'
              aria-selected={view === item.key}
              className={`mm-switch-item${view === item.key ? ' active' : ''}`}
              onClick={() => switchView(item.key)}
            >
              {t(item.label)}
              {item.key === 'review' && (review.data?.pending ?? 0) > 0 ? (
                <>
                  <span className='mm-switch-n'>{review.data?.pending}</span>
                  <span className='mm-switch-dot' />
                </>
              ) : null}
            </button>
          ))}
        </div>
      </div>

      {/* ── 列表 / 回收站：等级 tab + 常显筛选行 ───────────────── */}
      {view === 'list' || view === 'trash' ? (
        <>
          <div className='mm-browse-toolbar'>
            <div className='mm-switch is-3' style={{ ['--sw-i' as string]: levelIndex }} role='tablist'>
              <span className='mm-switch-thumb' />
              {LEVELS.map(item => (
                <button
                  key={item.key || 'all'}
                  type='button'
                  role='tab'
                  aria-selected={level === item.key}
                  title={item.hint || undefined}
                  className={`mm-switch-item${level === item.key ? ' active' : ''}`}
                  onClick={() => patchFilter(() => setLevel(item.key))}
                >
                  {t(item.label)}
                  {item.hint ? <span className='mm-info' aria-hidden='true'>i</span> : null}
                </button>
              ))}
            </div>
            <span className='mm-browse-count'>
              {t('共 {n} 条', { n: num(total) })}
            </span>
          </div>

          <div className='mm-browse-filters'>
            <Input
              className='mm-browse-search'
              value={query}
              placeholder={t('搜索记忆')}
              onChange={event => setQuery(event.target.value)}
              onKeyDown={event => {
                if (event.key === 'Enter') {
                  setSubmitted(query)
                  setPage(0)
                }
              }}
              onBlur={() => {
                if (query !== submitted) {
                  setSubmitted(query)
                  setPage(0)
                }
              }}
            />
            <select
              className='mm-select'
              value={project}
              onChange={event => patchFilter(() => setProject(event.target.value))}
            >
              <option value=''>{t('全部')}</option>
              {/* 通用项目不在项目列表里（它不是一张可维护的卡），但仍要能按它筛 */}
              <option value='_global'>{t('通用（general）')}</option>
              {projects.map(item => (
                <option key={item.slug} value={item.slug}>{item.name}</option>
              ))}
            </select>
            <select
              className='mm-select'
              value={agent}
              onChange={event => patchFilter(() => setAgent(event.target.value))}
            >
              <option value=''>{t('全部 Agent')}</option>
              {agentOptions.map(value => (
                <option key={value} value={value}>{agentText(value)}</option>
              ))}
            </select>
            <select
              className='mm-select'
              value={kind}
              onChange={event => patchFilter(() => setKind(event.target.value))}
            >
              <option value=''>{t('全部类型')}</option>
              {kinds.map(option => <option key={option.value} value={option.value}>{option.label}</option>)}
            </select>
            <span className='mm-browse-spacer' />
            <Button
              type='button'
              size='sm'
              variant='ghost'
              className={moreOpen ? 'mm-filter-more is-on' : 'mm-filter-more'}
              onClick={() => setMoreOpen(value => !value)}
            >
              {t('更多筛选')}{(tag || starred || pinned || includeSuperseded) ? ' ·' : ''}
            </Button>
            <Button
              type='button'
              size='sm'
              variant='ghost'
              className='mm-filter-reset'
              disabled={!activeFilters && !submitted}
              onClick={resetFilters}
            >
              {t('重置筛选')}{activeFilters ? ` · ${activeFilters}` : ''}
            </Button>
          </div>

          {moreOpen ? (
            <div className='mm-browse-filters'>
              <Input
                className='mm-browse-tag'
                value={tag}
                placeholder={t('标签包含')}
                onChange={event => patchFilter(() => setTag(event.target.value))}
              />
              <label className='mm-check'>
                <input
                  type='checkbox'
                  checked={includeSuperseded}
                  onChange={event => patchFilter(() => setIncludeSuperseded(event.target.checked))}
                />
                {t('显示已失效')}
              </label>
              <label className='mm-check'>
                <input
                  type='checkbox'
                  checked={starred}
                  onChange={event => patchFilter(() => setStarred(event.target.checked))}
                />
                {t('仅收藏')}
              </label>
              <label className='mm-check'>
                <input
                  type='checkbox'
                  checked={pinned}
                  onChange={event => patchFilter(() => setPinned(event.target.checked))}
                />
                {t('仅置顶')}
              </label>
            </div>
          ) : null}

          {/* ── 表格 ───────────────────────────────────────── */}
          <div className='mm-browse-card'>
            {list.error ? (
              <Panel loading={false} error={list.error} onRetry={list.reload}>{null}</Panel>
            ) : (
              <>
                <div className='mm-browse-scroll'>
                  <table className='mm-browse-table'>
                    <thead>
                      <tr>
                        <th className='mm-bt-time'>{t('时间')}</th>
                        <th className='mm-bt-title'>{t('标题')}</th>
                        <th className='mm-bt-layer'>{t('层级')}</th>
                        <th className='mm-bt-project'>{t('项目')}</th>
                        <th className='mm-bt-agent'>{t('Agent')}</th>
                        <th className='mm-bt-mark'>{t('标记')}</th>
                        <th className='mm-bt-tags'>{t('标签')}</th>
                        <th className='mm-bt-actions'>{t('操作')}</th>
                      </tr>
                    </thead>
                    <tbody>
                      {list.loading && !list.data ? (
                        Array.from({ length: 6 }).map((_, index) => (
                          <tr key={`sk-${index}`} className='mm-bt-skeleton'>
                            <td><span style={{ width: 92 }} /></td>
                            <td><span style={{ width: 240 }} /></td>
                            <td><span style={{ width: 40 }} /></td>
                            <td><span style={{ width: 70 }} /></td>
                            <td><span style={{ width: 56 }} /></td>
                            <td><span style={{ width: 44 }} /></td>
                            <td><span style={{ width: 88 }} /></td>
                            <td><span style={{ width: 20 }} /></td>
                          </tr>
                        ))
                      ) : null}

                      {!list.loading && (list.data?.items.length ?? 0) === 0 ? (
                        <tr>
                          <td colSpan={8} className='mm-bt-empty'>
                            {submitted
                              ? t('没有命中的记忆 —— 试试更短的关键词。')
                              : trashMode
                                ? t('回收站是空的。')
                                : t('当前条件下没有记忆。')}
                          </td>
                        </tr>
                      ) : null}

                      {list.data?.items.map(item => (
                        <tr
                          key={item.id}
                          className={`mm-bt-row${selected?.id === item.id ? ' is-active' : ''}${item.supersededBy ? ' is-superseded' : ''}`}
                          onClick={() => void openEntry(item.id)}
                        >
                          <td className='mm-bt-time' title={fullTime(item.createdAt)}>
                            <span className='mm-mono'>{stamp(item.createdAt)}</span>
                          </td>
                          <td className='mm-bt-title'>
                            <span className='mm-bt-title-text' title={item.title}>
                              {item.pinned ? '📌 ' : ''}{item.title}
                            </span>
                          </td>
                          <td className='mm-bt-layer'>
                            <span className={`mm-pill${item.layer === 'l2' ? ' l2' : ' muted'}`}>
                              {item.layer === 'l2' ? t('深层') : t('普通')}
                            </span>
                          </td>
                          <td className='mm-bt-link' onClick={event => {
                            event.stopPropagation()
                            patchFilter(() => setProject(item.project ?? '_global'))
                          }}>
                            <span className='mm-ellipsis' title={item.project ?? t('通用（general）')}>
                              {item.project ?? t('通用（general）')}
                            </span>
                          </td>
                          <td className='mm-bt-link' onClick={event => {
                            event.stopPropagation()
                            if (item.agent) patchFilter(() => setAgent(item.agent ?? ''))
                          }}>
                            <span className='mm-ellipsis'>{agentText(item.agent)}</span>
                          </td>
                          <td className='mm-bt-mark'>
                            <MarkCell item={item} />
                          </td>
                          <td className='mm-bt-tags'>
                            <span className='mm-ellipsis' title={item.tags.join(' · ')}>
                              {item.tags.slice(0, 3).join(' · ') || '—'}
                            </span>
                          </td>
                          <td className='mm-bt-actions' onClick={event => event.stopPropagation()}>
                            <RowMenu
                              item={item}
                              trash={trashMode}
                              busy={busy === item.id}
                              onOpen={() => void openEntry(item.id)}
                              onStar={() => void act(item.id, () => m.updateEntry(item.id, { starred: !item.starred }))}
                              onPin={() => void act(item.id, () => m.updateEntry(item.id, { pinned: !item.pinned }))}
                              onRestore={() => void act(item.id, () => m.restoreEntry(item.id))}
                              onPurge={() => void act(item.id, async () => {
                                await m.purgeEntry(item.id)
                                if (selected?.id === item.id) setSelected(null)
                              })}
                              onDelete={() => void act(item.id, async () => {
                                await m.deleteEntry(item.id)
                                if (selected?.id === item.id) setSelected(null)
                              })}
                            />
                          </td>
                        </tr>
                      ))}
                    </tbody>
                  </table>
                </div>

                <div className='mm-browse-pager'>
                  <span className='mm-pager-info'>
                    {t('共 {n} 条 · 第 {page} / {pages} 页', { n: num(total), page: page + 1, pages })}
                  </span>
                  <Button
                    type='button'
                    size='sm'
                    variant='ghost'
                    disabled={page === 0}
                    onClick={() => setPage(value => Math.max(0, value - 1))}
                  >
                    {t('上一页')}
                  </Button>
                  <Button
                    type='button'
                    size='sm'
                    variant='ghost'
                    disabled={page + 1 >= pages}
                    onClick={() => setPage(value => value + 1)}
                  >
                    {t('下一页')}
                  </Button>
                </div>
              </>
            )}
          </div>
        </>
      ) : null}

      {/* ── 热力图 ─────────────────────────────────────────── */}
      {view === 'heatmap' ? (
        <>
          <HeatView
            data={heat.data}
            loading={heat.loading}
            error={heat.error}
            onRetry={heat.reload}
            picked={dayPick}
            onPickDay={day => setDayPick(day)}
          />
          {dayPick ? (
            <DayPanel
              day={dayPick}
              data={dayList.data}
              loading={dayList.loading}
              onClose={() => setDayPick(null)}
              onOpen={id => void openEntry(id)}
            />
          ) : null}
        </>
      ) : null}

      {/* ── 待确认 ─────────────────────────────────────────── */}
      {view === 'review' ? (
        <div className='mm-review-page'>
          <Section
            title={t('待确认（{n}）', { n: review.data?.pending ?? 0 })}
            hint={t('AI 的建议不会自动落盘。批准才会改数据，拒绝只改队列状态 —— 任何裁决都能反悔。')}
            action={(
              <div className='mm-row' style={{ gap: 6 }}>
                <Button type='button' size='sm' variant='ghost' onClick={() => review.reload()}>
                  {t('刷新')}
                </Button>
                <Button
                  type='button'
                  size='sm'
                  variant='ghost'
                  onClick={() => void act('__review__', () => m.rejectAllReview())}
                >
                  {t('全部忽略')}
                </Button>
              </div>
            )}
          >
            {review.loading && !review.data ? <Panel loading error=''>{null}</Panel> : null}
            {review.error ? <Panel loading={false} error={review.error} onRetry={review.reload}>{null}</Panel> : null}
            {review.data && review.data.items.length === 0 ? (
              <Empty>{t('队列空了。没有待裁决的建议。')}</Empty>
            ) : null}
            <div className='mm-review-list'>
              {review.data?.items.map(item => (
                <div key={item.id} className='mm-review'>
                  <div className='mm-review-head'>
                    <StateBadge state='warn' label={reviewLabel(item.type)} />
                    <span className='mm-review-target'>{item.target}</span>
                    <span className='mm-muted'>{t('置信度 {n}%', { n: Math.round(item.confidence * 100) })}</span>
                    <span className='mm-muted'>{item.createdText ?? timeText(item.createdAt)}</span>
                  </div>
                  <div className='mm-review-detail'>{reviewDetail(item)}</div>
                  <div className='mm-review-actions'>
                    <Button type='button' size='sm' onClick={() => void act(`review-${item.id}`, () => m.resolveReview(item.id, true))}>
                      {t('批准')}
                    </Button>
                    <Button type='button' size='sm' variant='ghost' onClick={() => void act(`review-${item.id}`, () => m.resolveReview(item.id, false))}>
                      {t('忽略')}
                    </Button>
                    <Button type='button' size='sm' variant='link' onClick={() => void openEntry(item.target)}>
                      {t('看这条记忆')}
                    </Button>
                  </div>
                </div>
              ))}
            </div>
          </Section>
        </div>
      ) : null}

      {/* ── 详情抽屉 ───────────────────────────────────────── */}
      {selected ? (
        <div className='mm-drawer-mask' role='dialog' aria-modal='true' onClick={() => setSelected(null)}>
          <aside className='mm-drawer' onClick={event => event.stopPropagation()}>
            <div className='mm-drawer-head'>
              <div className='mm-drawer-title'>{selected.title}</div>
              <Button type='button' size='sm' variant='ghost' onClick={() => setSelected(null)}>✕</Button>
            </div>

            <div className='mm-drawer-meta'>
              <Badge variant='secondary' shape='tag'>{selected.typeLabel}</Badge>
              <Badge variant={selected.layer === 'l2' ? 'brand' : 'outline'} shape='tag'>
                {selected.layer === 'l2' ? t('深层') : t('普通')}
              </Badge>
              <span>{selected.project ?? t('通用（general）')}</span>
              <span>{agentText(selected.agent)}</span>
              <span>{stamp(selected.createdAt)}</span>
              <span>{selected.chars} {t('字')}</span>
            </div>

            {selected.tags.length ? (
              <div className='mm-tags'>
                {selected.tags.map(value => <Badge key={value} variant='outline' shape='tag'>{value}</Badge>)}
              </div>
            ) : null}
            {selected.files.length ? (
              <div className='mm-files'>
                {selected.files.map(file => <code key={file}>{file}</code>)}
              </div>
            ) : null}

            <div className='mm-drawer-actions'>
              {selected.deletedAt ? (
                <>
                  <Button type='button' size='sm' onClick={() => void act(selected.id, async () => {
                    await m.restoreEntry(selected.id)
                    await openEntry(selected.id)
                  })}>
                    {t('恢复')}
                  </Button>
                  <Button type='button' size='sm' variant='destructive' onClick={() => void act(selected.id, async () => {
                    await m.purgeEntry(selected.id)
                    setSelected(null)
                  })}>
                    {t('彻底删除')}
                  </Button>
                </>
              ) : (
                <>
                  <Button type='button' size='sm' variant='ghost' onClick={() => void act(selected.id, async () => {
                    await m.updateEntry(selected.id, { starred: !selected.starred })
                    await openEntry(selected.id)
                  })}>
                    {selected.starred ? t('取消星标') : t('星标')}
                  </Button>
                  <Button type='button' size='sm' variant='ghost' onClick={() => void act(selected.id, async () => {
                    await m.updateEntry(selected.id, { pinned: !selected.pinned })
                    await openEntry(selected.id)
                  })}>
                    {selected.pinned ? t('取消置顶') : t('置顶')}
                  </Button>
                  <Button type='button' size='sm' variant='ghost' onClick={() => setDraft({
                    content: '',
                    title: `${t('更新')}：${selected.title}`,
                    type: selected.type,
                    project: selected.project ?? undefined,
                    supersedes: [selected.id],
                  })}>
                    {t('写一条取代它')}
                  </Button>
                  <Button type='button' size='sm' variant='ghost' onClick={() => setDraft({
                    content: selected.body ?? '',
                    title: selected.title,
                    type: selected.type,
                    layer: selected.layer,
                    project: selected.project ?? undefined,
                    tags: selected.tags,
                    importance: selected.importance,
                  })}>
                    {t('编辑')}
                  </Button>
                  <Button type='button' size='sm' variant='destructive' onClick={() => void act(selected.id, async () => {
                    await m.deleteEntry(selected.id)
                    setSelected(null)
                  })}>
                    {t('删除')}
                  </Button>
                </>
              )}
            </div>

            <pre className='mm-body'>{selected.body ?? selected.summary}</pre>

            {timeline.length > 1 ? (
              <div className='mm-timeline'>
                <div className='mm-section-title'>{t('事实演化链')}</div>
                <div className='mm-muted mm-timeline-hint'>
                  {t('这条结论被改写过 {n} 次。旧版本留着是为了回答「为什么变了」。', { n: timeline.length - 1 })}
                </div>
                {timeline.map(item => (
                  <div key={item.id} className={`mm-timeline-row${item.current ? ' current' : ''}`}>
                    <span className='mm-timeline-date'>{item.validFromText}</span>
                    <button type='button' className='mm-link' onClick={() => void openEntry(item.id)}>{item.title}</button>
                    <span className='mm-timeline-type'>{item.typeLabel}</span>
                    {item.current ? <Badge variant='brand' shape='tag'>{t('当前')}</Badge> : null}
                  </div>
                ))}
              </div>
            ) : null}

            {selected.evidence.length ? (
              <div className='mm-related'>
                <div className='mm-section-title'>{t('证据链')}</div>
                <div className='mm-muted'>{t('这条结论是从下列记忆推出来的。')}</div>
                <div className='mm-tags'>
                  {selected.evidence.map(id => (
                    <button key={id} type='button' className='mm-link' onClick={() => void openEntry(id)}>{id}</button>
                  ))}
                </div>
              </div>
            ) : null}

            {selected.related?.length ? (
              <div className='mm-related'>
                <div className='mm-section-title'>{t('相关记忆')}</div>
                {selected.related.map(item => (
                  <button key={item.id} type='button' className='mm-link' onClick={() => void openEntry(item.id)}>
                    {item.line}
                  </button>
                ))}
              </div>
            ) : null}
          </aside>
        </div>
      ) : null}

      {draft ? (
        <div className='mm-dialog-mask' role='dialog' aria-modal='true'>
          <div className='mm-dialog'>
            <div className='mm-dialog-head'>
              <strong>{draft.supersedes?.length ? t('写一条取代旧记忆') : t('新建记忆')}</strong>
              <Button type='button' size='sm' variant='ghost' onClick={() => setDraft(null)}>{t('取消')}</Button>
            </div>
            <div className='mm-dialog-body'>
              <label className='mm-label'>{t('标题')}</label>
              <Input
                value={draft.title ?? ''}
                placeholder={t('留空则取正文第一段')}
                onChange={event => setDraft({ ...draft, title: event.target.value })}
              />

              <div className='mm-dialog-row'>
                <div>
                  <label className='mm-label'>{t('类别')}</label>
                  <select className='mm-select' value={draft.type ?? 'daily'} onChange={event => setDraft({ ...draft, type: event.target.value })}>
                    {kinds.map(option => <option key={option.value} value={option.value}>{option.label}</option>)}
                  </select>
                </div>
                <div>
                  <label className='mm-label'>{t('层级')}</label>
                  <select className='mm-select' value={draft.layer ?? ''} onChange={event => setDraft({ ...draft, layer: event.target.value || undefined })}>
                    <option value=''>{t('按类别自动')}</option>
                    <option value='l1'>{t('l1 原始流水')}</option>
                    <option value='l2'>{t('l2 沉淀结论')}</option>
                  </select>
                </div>
                <div>
                  <label className='mm-label'>{t('重要度')}</label>
                  <Input
                    type='number'
                    step='0.1'
                    min='0'
                    max='1'
                    value={String(draft.importance ?? 0.5)}
                    onChange={event => setDraft({ ...draft, importance: Number(event.target.value) })}
                  />
                </div>
              </div>

              <div className='mm-dialog-row'>
                <div>
                  <label className='mm-label'>{t('项目')}</label>
                  <Input
                    value={draft.project ?? ''}
                    placeholder={t('留空 = 全局')}
                    onChange={event => setDraft({ ...draft, project: event.target.value })}
                  />
                </div>
                <div className='mm-dialog-wide'>
                  <label className='mm-label'>{t('标签（逗号分隔）')}</label>
                  <Input
                    value={(draft.tags ?? []).join(', ')}
                    onChange={event => setDraft({
                      ...draft,
                      tags: event.target.value.split(',').map(item => item.trim()).filter(Boolean),
                    })}
                  />
                </div>
              </div>

              <label className='mm-label'>{t('正文（Markdown）')}</label>
              <Textarea
                rows={12}
                value={draft.content}
                placeholder={t('写「为什么」而不是「是什么」：决策取舍、避坑经验、接口规矩。')}
                onChange={event => setDraft({ ...draft, content: event.target.value })}
              />
              {draft.supersedes?.length ? (
                <p className='mm-hint'>
                  {t('保存后旧记忆会被标记为「已被取代」，仍可查看、不进回收站 —— 演化链要留着。')}
                </p>
              ) : null}
            </div>
            <div className='mm-dialog-foot'>
              <Button type='button' variant='ghost' onClick={() => setDraft(null)}>{t('取消')}</Button>
              <Button type='button' onClick={() => void saveDraft()}>{busy === '__save__' ? t('保存中…') : t('保存')}</Button>
            </div>
          </div>
        </div>
      ) : null}
    </div>
  )
}

/**
 * 标记列。
 *
 * 只显示**例外**：有效的记忆是默认状态，不用占地方。所以四类里最多显示三个，
 * 一个都没有时给一个 `—` —— 空单元格在表格里会被读成「加载失败」。
 */
function MarkCell({ item }: { item: m.MemoryEntry }): React.ReactElement {
  const marks: React.ReactElement[] = []
  if (item.supersededBy) {
    marks.push(<span key='sup' className='mm-pill warn' title={t('已被更新的记忆取代')}>{t('已失效')}</span>)
  }
  // 重要度：本仓存 0..1 的浮点，目标界面用的是 1..5 的整数档，
  // 所以先折算成档位再显示（0.8 → 4）。折算后 ≥4 才占位 —— 低档位是常态，
  // 每行都挂个数字反而把「真正需要注意的那几条」淹了。
  const score = Math.round(item.importance * 5)
  if (score >= 4) {
    marks.push(<span key='imp' className='mm-bt-score' title={t('重要度')}>{score}</span>)
  }
  if (item.starred) {
    marks.push(<span key='star' className='mm-pill ok'>{t('已收藏')}</span>)
  }
  if (!marks.length) return <span className='mm-muted'>—</span>
  return <span className='mm-bt-mark-cell'>{marks}</span>
}

/** 行尾 `⋯` 菜单：列表态给「收藏/置顶/删除」，回收站态给「恢复/彻底删除」。 */
function RowMenu({
  item,
  trash,
  busy,
  onOpen,
  onStar,
  onPin,
  onRestore,
  onPurge,
  onDelete,
}: {
  item: m.MemoryEntry
  trash: boolean
  busy: boolean
  onOpen: () => void
  onStar: () => void
  onPin: () => void
  onRestore: () => void
  onPurge: () => void
  onDelete: () => void
}): React.ReactElement {
  const [open, setOpen] = React.useState(false)
  const close = (): void => setOpen(false)
  return (
    <span className='mm-bt-menu-wrap'>
      <button
        type='button'
        className='mm-bt-more'
        title={t('更多操作')}
        disabled={busy}
        onClick={() => setOpen(value => !value)}
      >
        {busy ? '…' : '⋯'}
      </button>
      {open ? (
        <>
          <span className='mm-bt-menu-mask' onClick={close} />
          <div className='mm-bt-menu'>
            <button type='button' className='mm-pt-menu-item' onClick={() => { close(); onOpen() }}>
              {t('打开详情')}
            </button>
            {trash ? (
              <>
                <button type='button' className='mm-pt-menu-item' onClick={() => { close(); onRestore() }}>
                  {t('恢复')}
                </button>
                <button type='button' className='mm-pt-menu-item is-danger' onClick={() => { close(); onPurge() }}>
                  {t('彻底删除')}
                </button>
              </>
            ) : (
              <>
                <button type='button' className='mm-pt-menu-item' onClick={() => { close(); onStar() }}>
                  {item.starred ? t('取消收藏') : t('收藏')}
                </button>
                <button type='button' className='mm-pt-menu-item' onClick={() => { close(); onPin() }}>
                  {item.pinned ? t('取消置顶') : t('置顶')}
                </button>
                <button type='button' className='mm-pt-menu-item is-danger' onClick={() => { close(); onDelete() }}>
                  {t('删除（移入回收站）')}
                </button>
              </>
            )}
          </div>
        </>
      ) : null}
    </span>
  )
}

/**
 * 热力图：GitHub 同款日历网格（53 列 × 7 行）。
 *
 * 为什么自己算格子而不用现成组件：数据是「有记录的那些天」，稀疏且可能一天都没有。
 * 组件库的日历要么按完整月历铺（一年 12 屏），要么要求补零到连续序列。这里
 * 直接按「从今天往前 365 天」生成连续日期，缺的填 0，一周一列，最省事也最准。
 */
function HeatView({
  data,
  loading,
  error,
  onRetry,
  picked,
  onPickDay,
}: {
  data: m.MemoryHeat | null
  loading: boolean
  error: string
  onRetry: () => void
  picked: string | null
  onPickDay: (day: string) => void
}): React.ReactElement {
  const cells = React.useMemo(() => {
    const counts = new Map<string, number>()
    for (const item of data?.items ?? []) counts.set(item.day, item.count)
    const endText = data?.end ?? new Date().toISOString().slice(0, 10)
    const end = new Date(`${endText}T00:00:00Z`)
    const days = data?.days ?? HEAT_DAYS
    const list: { day: string; count: number }[] = []
    for (let offset = days - 1; offset >= 0; offset -= 1) {
      const cursor = new Date(end.getTime() - offset * 86_400_000)
      const key = cursor.toISOString().slice(0, 10)
      list.push({ day: key, count: counts.get(key) ?? 0 })
    }
    return list
  }, [data])

  if (loading && !data) return <Panel loading error=''>{null}</Panel>
  if (error) return <Panel loading={false} error={error} onRetry={onRetry}>{null}</Panel>

  // 分档：0 / 1-2 / 3-5 / 6-10 / >10。固定档位而不是按最大值归一 ——
  // 归一化会让「最近写了 200 条」把之前所有正常天全压成最浅色，看不出日常节奏。
  const bucket = (count: number): number => {
    if (count <= 0) return 0
    if (count <= 2) return 1
    if (count <= 5) return 2
    if (count <= 10) return 3
    return 4
  }

  const weeks: { day: string; count: number }[][] = []
  for (let index = 0; index < cells.length; index += 7) weeks.push(cells.slice(index, index + 7))

  return (
    <div className='mm-heat'>
      <div className='mm-heat-head'>
        <strong>{t('每日记录热力图')}</strong>
        <span className='mm-muted'>
          {t('近 {days} 天共 {total} 条 · 活跃 {active} 天', {
            days: data?.days ?? HEAT_DAYS,
            total: num(data?.total ?? 0),
            active: data?.activeDays ?? 0,
          })}
        </span>
        <span className='mm-heat-legend'>
          {t('少')}
          {[0, 1, 2, 3, 4].map(level => <i key={level} className={`mm-heat-cell is-${level}`} />)}
          {t('多')}
        </span>
      </div>
      <div className='mm-heat-grid'>
        {weeks.map((week, index) => (
          <div key={index} className='mm-heat-col'>
            {week.map(cell => (
              <button
                key={cell.day}
                type='button'
                className={`mm-heat-cell is-${bucket(cell.count)}${cell.day === picked ? ' is-picked' : ''}`}
                title={t('{day}：{n} 条', { day: cell.day, n: cell.count })}
                onClick={() => onPickDay(cell.day)}
              />
            ))}
          </div>
        ))}
      </div>
    </div>
  )
}

/**
 * 热力图点开某天之后的「当日清单」。
 *
 * 为什么单独一张卡而不是直接跳到列表视图：热力图的价值在于「哪几天密、哪几天疏」，
 * 跳走就等于把上下文丢了。这里在下方展开一小张表（时间 / 标题 / 层级 / 重要 / Agent），
 * 看完还能接着在图上点别的日子。
 */
function DayPanel({
  day,
  data,
  loading,
  onClose,
  onOpen,
}: {
  day: string
  data: m.MemoryList | null
  loading: boolean
  onClose: () => void
  onOpen: (id: string) => void
}): React.ReactElement {
  const rows = data?.items ?? []
  return (
    <div className='mm-heat-day'>
      <div className='mm-heat-day-head'>
        <strong>{t('{day} · 共 {n} 条', { day, n: num(data?.total ?? 0) })}</strong>
        <Button type='button' size='sm' variant='ghost' onClick={onClose}>
          {t('收起')}
        </Button>
      </div>
      {loading && !data ? <Panel loading error=''>{null}</Panel> : null}
      {rows.length === 0 && !loading ? (
        <div className='mm-heat-day-empty'>{t('这一天没有记录')}</div>
      ) : null}
      {rows.length ? (
        <table className='mm-heat-day-table'>
          <thead>
            <tr>
              <th className='mm-hd-time'>{t('时间')}</th>
              <th className='mm-hd-title'>{t('标题')}</th>
              <th className='mm-hd-layer'>{t('层级')}</th>
              <th className='mm-hd-score'>{t('重要')}</th>
              <th className='mm-hd-agent'>{t('Agent')}</th>
            </tr>
          </thead>
          <tbody>
            {rows.map(item => (
              <tr key={item.id} onClick={() => onOpen(item.id)}>
                <td className='mm-hd-time'>
                  <span className='mm-mono'>{stamp(item.createdAt).slice(6, 11)}</span>
                </td>
                <td className='mm-hd-title'>
                  <span className='mm-ellipsis' title={item.title}>{item.title}</span>
                </td>
                <td className='mm-hd-layer'>
                  <span className={`mm-pill${item.layer === 'l2' ? ' l2' : ' muted'}`}>
                    {item.layer === 'l2' ? t('深层') : t('普通')}
                  </span>
                </td>
                <td className='mm-hd-score'>{Math.round(item.importance * 5)}</td>
                <td className='mm-hd-agent'>{agentText(item.agent)}</td>
              </tr>
            ))}
          </tbody>
        </table>
      ) : null}
    </div>
  )
}

function reviewLabel(type: string): string {
  switch (type) {
    case 'dedup': return t('疑似重复')
    case 'classify': return t('项目归类')
    case 'supersede': return t('事实失效')
    case 'distill': return t('蒸馏结论')
    case 'consolidate': return t('合并结论')
    default: return type
  }
}

/** 把队列载荷翻译成一句人话。载荷是各任务自己写的，形状不统一，所以逐类处理。 */
function reviewDetail(item: m.ReviewItem): string {
  const payload = item.payload ?? {}
  const pick = (key: string) => {
    const value = payload[key]
    return typeof value === 'string' ? value : ''
  }
  switch (item.type) {
    case 'dedup': {
      const candidate = pick('candidate')
      const reason = pick('reason')
      return candidate
        ? t('与 {id} 高度相似，批准后本条会被它取代。{reason}', { id: candidate, reason: reason ? `（${reason}）` : '' })
        : t('疑似重复，但载荷里没有候选 id')
    }
    case 'classify': {
      const suggestion = pick('suggestion')
      return suggestion
        ? t('建议归入项目 {slug}。批准后记忆会被搬到那个项目目录。', { slug: suggestion })
        : t('建议归类，但载荷里没有目标项目')
    }
    case 'supersede': {
      const oldId = pick('oldId') || pick('superseded')
      return oldId
        ? t('新记忆让 {id} 不再成立。批准后旧条目被标记为已取代。', { id: oldId })
        : t('建议标记旧事实失效，但载荷里没有目标 id')
    }
    default:
      return JSON.stringify(payload).slice(0, 200)
  }
}

export { Browse }
