import * as React from 'react'
import { Badge, Button, Checkbox, Input, Switch, Textarea } from '@ui'
import { t } from '../i18n'
import * as m from './memory-api'
import { agentText, Empty, isDraft, isRefuted, num, Panel, Section, stamp, StateBadge, timeText, fullTime, useLoad } from './memory-shared'

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

/** 每页条数兜底值。真实值由配置项 `ui.pageSize` 下发（见 `BrowseProps.pageSize`）。 */
const PAGE_SIZE_FALLBACK = 50
/** 回收站一次最多画多少行。回收站没有分页（文件是按删除时间排的一串），
 *  但也不能把上千行全铺进 DOM。 */
const TRASH_ROWS = 200
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
  /**
   * 外部要求直接打开某个视图（仪表盘「去裁决」跳「待确认」）。
   *
   * 为什么不能只跳页签：浏览页默认落在「列表」，而待确认是四段控件里的第三段。
   * 只切页签的结果是用户到了一张「共 199 条」的普通列表，看不见任何一条待裁决
   * 建议 —— 那正是「点了去裁决却不知道去哪处理」的来源。跳转必须带视图。
   */
  focusView?: View | null
  onFocusViewConsumed?: () => void
  /** 每页条数（配置项 `ui.pageSize`）。缺省按 50 —— 老后端不下发这个值时照样能用。 */
  pageSize?: number
}

function Browse({
  kinds,
  projects,
  onChanged,
  focusId,
  onFocusConsumed,
  focusProject,
  onFocusProjectConsumed,
  focusView,
  onFocusViewConsumed,
  pageSize = PAGE_SIZE_FALLBACK,
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
  /**
   * 「编辑」正在改的那条记忆 id。null = 对话框是「新建」。
   *
   * 为什么要单独记一个 id：对话框只有一份（`draft` 是 `WritePayload`，形状
   * 属于写入接口），早先「编辑」把原条目填进这份表单再保存，走的是
   * `writeEntry` —— 也就是**又写了一条新记忆**，原文不动。用户点一次「编辑」
   * 改个错字，库里就多一条近乎重复的条目，正是这一轮在消灭的东西。
   */
  const [editing, setEditing] = React.useState<string | null>(null)
  const [error, setError] = React.useState('')
  const [busy, setBusy] = React.useState('')
  /**
   * 批量裁决：勾中的待确认项 id。
   *
   * ── 为什么是「勾选 + 批量」而不是「全部批准」────────────────
   * 批准是写操作，四类建议没有一类是纯记录：
   * - 归类会把记忆文件搬进 `projects/<slug>/`，slug 不存在就**新建目录**；
   * - 去重 / 事实失效会把一条记忆标记为「被取代」，默认检索从此看不到它；
   * - 蒸馏 / 合并会新落一条 l2 结论，之后每个请求都会注入它。
   *
   * 全部批准 = 把「哪条会被搬错、哪条会被埋掉、哪条错知识会长期在线」这三个
   * 判断一次性交出去。后端也因此只提供批量拒绝（`rejectAll`），没有批量批准。
   * 所以这里的批量是「你逐条勾出来的那几条」，一次点确认，但每一条都是你选的。
   */
  const [picked, setPicked] = React.useState<Set<number>>(new Set())
  /** 批量执行进度；null = 没在跑。逐条串行，后端一次只认一个 id。 */
  const [batch, setBatch] = React.useState<{ done: number; total: number } | null>(null)
  /** 批准前的一次确认（写操作，不能一键到底）。 */
  const [confirmApprove, setConfirmApprove] = React.useState(false)
  /** 「这条错了」的理由输入是否展开。点按钮只展开、不立即提交（写操作要让人先说话）。 */
  const [refuting, setRefuting] = React.useState(false)
  /** 「这条错了」的理由。空串提交时按 undefined 处理（后端理由可空）。 */
  const [refuteReason, setRefuteReason] = React.useState('')

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
      includeSuperseded: includeSuperseded || undefined,
      // 浏览页是「看全部」的地方：未升级的抽取草稿也要出现在列表里，
      // 否则用户看不到它们、也就无从裁决。检索/注入等出口仍然默认排除草稿。
      includeCandidates: true,
      // 浏览页必须看得见被否掉的条目，否则用户没法撤销。检索/注入等出口
      // 仍然默认排除被反驳的条目 —— 这里只是「能看见并处理」。
      includeRefuted: true,
      limit: pageSize,
      offset: page * pageSize,
    }),
    [submitted, kind, project, agent, level, tag, starred, pinned, includeSuperseded, page, pageSize],
    // 重新进入页面就重拉：列表是最容易「刚在别处改过、回来还是旧的」的地方
    // （批准一条、导入一批、跑完一个任务，回到列表还停在改动前）。
    { refreshOnEnter: true },
  )

  const total = list.data?.total ?? 0
  const pages = Math.max(1, Math.ceil(total / pageSize))

  // 待确认队列同上：它是唯一「AI 一直在往里加」的列表，切回来必须是最新条数。
  const review = useLoad(() => m.loadReview('pending', undefined, 50), [], { refreshOnEnter: true })

  /** 库里真实存在的项目 slug —— 归类建议要跟它对照，指向不存在的项目必须点出来。 */
  const projectSlugs = React.useMemo(() => new Set(projects.map(item => item.slug)), [projects])

  // 外部指定视图（仪表盘「去裁决」）：切到那一段并拉一次最新队列。
  // 消费后立刻清掉，否则用户手动切回「列表」又会被下一次渲染顶回来。
  React.useEffect(() => {
    if (!focusView) return
    setView(focusView)
    setPage(0)
    if (focusView === 'review') review.reload()
    onFocusViewConsumed?.()
    // review.reload 每次渲染都是新函数，但它只读同一个 useLoad 状态
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [focusView, onFocusViewConsumed])

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
    // 换一条就收起上一条的「这条错了」输入，理由不跟着串到别条上。
    setRefuting(false)
    setRefuteReason('')
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
      // 两个分支语义不同，不能合并：编辑是「改这一条」，写入是「新建一条」。
      // 编辑走 `updateEntry` 才不会凭空多出一条（详见 `editing` 的说明）。
      if (editing) {
        await m.updateEntry(editing, {
          body: draft.content,
          title: draft.title,
          type: draft.type,
          layer: draft.layer,
          project: draft.project ?? '',
          tags: draft.tags,
          importance: draft.importance,
        })
      } else {
        await m.writeEntry(draft)
      }
      setDraft(null)
      setEditing(null)
      list.reload()
      review.reload()
      onChanged()
      // 编辑的目标就是正在看的那条：重新拉一次，抽屉里的正文不能停在旧版本
      if (editing) await openEntry(editing)
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

  /* ─── 批量裁决 ───────────────────────────────────────────── */

  /** 勾选 / 取消一条待确认项。 */
  const togglePicked = (id: number, next: boolean) => {
    setPicked(prev => {
      const holder = new Set(prev)
      if (next) holder.add(id)
      else holder.delete(id)
      return holder
    })
  }

  /** 当前页里真正被勾中的那些（上一轮裁决后已不在队列里的 id 自动不算）。 */
  const pickedItems = (review.data?.items ?? []).filter(item => picked.has(item.id))
  const pickable = review.data?.items.length ?? 0
  const allPicked = pickable > 0 && pickedItems.length === pickable
  const somePicked = pickedItems.length > 0 && !allPicked
  /** 勾中项的构成：批准前必须让人看清「将执行哪几类写操作」。 */
  const pickedTally = reviewTally(pickedItems)

  /**
   * 批量执行：逐条串行调 `resolveReview`。
   *
   * 后端一次只认一个 id（没有批量批准接口，见 `review_reject_all` 的注释），
   * 所以这里是前端串行循环，不是「一个请求改一批」。
   *
   * 不中断：一条失败（比如目标记忆已被别人删掉）不该把剩下 20 条全卡住 ——
   * 失败的逐条记下来，跑完一起报。
   */
  const runBatch = async (approve: boolean) => {
    const items = pickedItems
    if (items.length === 0 || batch) return
    setBatch({ done: 0, total: items.length })
    setError('')
    const failed: string[] = []
    for (const [index, item] of items.entries()) {
      try {
        await m.resolveReview(item.id, approve)
      } catch (caught) {
        failed.push(`${item.target}：${caught instanceof Error ? caught.message : String(caught)}`)
      }
      setBatch({ done: index + 1, total: items.length })
    }
    setBatch(null)
    setConfirmApprove(false)
    setPicked(new Set())
    review.reload()
    list.reload()
    onChanged()
    if (failed.length > 0) {
      setError(t('{n} 条没成功：{detail}', { n: failed.length, detail: failed.slice(0, 3).join('；') }))
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
          onClick={() => { setEditing(null); setDraft({ content: '', type: 'daily', project: project || undefined }) }}
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

      {/* ── 列表：等级 tab + 常显筛选行 ───────────────── */}
      {view === 'list' ? (
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
                            {/* 草稿优先于层级显示：一条 l1 候选写「普通」会被读成
                                「已经入库的低层记忆」，写「草稿」才是它真实的身份。 */}
                            {isDraft(item) ? (
                              <span className='mm-pill draft' title={t('自动抽取的草稿，还没被反复确认，不会进入检索与注入')}>
                                {t('草稿')}
                              </span>
                            ) : (
                              <span className={`mm-pill${item.layer === 'l2' ? ' l2' : ' muted'}`}>
                                {item.layer === 'l2' ? t('深层') : t('普通')}
                              </span>
                            )}
                            {isRefuted(item) ? <span className='mm-pill refuted'>{t('错误')}</span> : null}
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
                              busy={busy === item.id}
                              onOpen={() => void openEntry(item.id)}
                              onStar={() => void act(item.id, () => m.updateEntry(item.id, { starred: !item.starred }))}
                              onPin={() => void act(item.id, () => m.updateEntry(item.id, { pinned: !item.pinned }))}
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

      {/* ── 回收站：自动清理卡 + 文件表 ─────────────────────── */}
      {view === 'trash' ? (
        <TrashView
          onOpen={id => void openEntry(id)}
          onChanged={() => {
            list.reload()
            onChanged()
          }}
        />
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
            {/* 队列构成先摆出来：41 条里是「22 条归类 + 10 条失效 + 8 条蒸馏 + 1 条重复」
                还是「41 条全是归类」，处置完全不一样 —— 前者要逐类看，后者可以整批忽略。
                以前只有 41 这个总数，用户打开这一页也不知道该从哪条下手。 */}
            {review.data && review.data.items.length > 0 ? (
              <div className='mm-muted' style={{ marginBottom: 8 }}>
                {reviewTally(review.data.items)}
              </div>
            ) : null}

            {/* 批量条：勾 → 看清楚构成 → 一次确认。
                不提供「全部批准」，只提供「批准所选」—— 理由见上面 `picked` 的注释。 */}
            {pickable > 0 ? (
              <div className='mm-review-batch'>
                <Checkbox
                  checked={allPicked}
                  indeterminate={somePicked}
                  disabled={!!batch}
                  aria-label={t('全选当前队列')}
                  onCheckedChange={next => setPicked(next === true
                    ? new Set((review.data?.items ?? []).map(item => item.id))
                    : new Set())}
                />
                {batch ? (
                  <span className='mm-muted'>
                    {t('正在裁决 {done}/{total}…', { done: batch.done, total: batch.total })}
                  </span>
                ) : pickedItems.length > 0 ? (
                  <span>{t('已选 {n} 条：{tally}', { n: pickedItems.length, tally: pickedTally })}</span>
                ) : (
                  <span className='mm-muted'>{t('逐条勾选，可一次批准或忽略；批准是写操作，会先让你确认一次')}</span>
                )}
                <div className='mm-review-batch-actions'>
                  <Button
                    type='button'
                    size='sm'
                    disabled={pickedItems.length === 0 || !!batch}
                    onClick={() => setConfirmApprove(true)}
                  >
                    {t('批准所选')}
                  </Button>
                  <Button
                    type='button'
                    size='sm'
                    variant='ghost'
                    disabled={pickedItems.length === 0 || !!batch}
                    onClick={() => void runBatch(false)}
                  >
                    {t('忽略所选')}
                  </Button>
                  {pickedItems.length > 0 && !batch ? (
                    <Button type='button' size='sm' variant='link' onClick={() => setPicked(new Set())}>
                      {t('清空选择')}
                    </Button>
                  ) : null}
                </div>
              </div>
            ) : null}

            {/* 批准前的确认：把「即将发生什么」写全。四类建议的后果不同，
                只说「确认批准 22 条」等于让人闭眼签字。 */}
            {confirmApprove && pickedItems.length > 0 ? (
              <div className='mm-review-confirm'>
                <div>
                  <strong>{t('确认批准这 {n} 条？', { n: pickedItems.length })}</strong>
                  <div className='mm-muted' style={{ marginTop: 4 }}>
                    {t('构成：{tally}。归类会移动记忆文件（目标项目不存在时会新建目录）；事实失效 / 去重会让旧条目从默认检索里消失；蒸馏会新落一条之后会被注入的结论。忽略只改队列状态，可以随时反悔。', { tally: pickedTally })}
                  </div>
                </div>
                <div className='mm-review-batch-actions'>
                  <Button type='button' size='sm' variant='destructive' onClick={() => void runBatch(true)}>
                    {t('确认批准 {n} 条', { n: pickedItems.length })}
                  </Button>
                  <Button type='button' size='sm' variant='ghost' onClick={() => setConfirmApprove(false)}>
                    {t('取消')}
                  </Button>
                </div>
              </div>
            ) : null}
            {review.data && review.data.items.length === 0 ? (
              <Empty>{t('队列空了。没有待裁决的建议。')}</Empty>
            ) : null}
            <div className='mm-review-list'>
              {review.data?.items.map(item => (
                <div key={item.id} className='mm-review'>
                  <div className='mm-review-head'>
                    <Checkbox
                      checked={picked.has(item.id)}
                      disabled={!!batch}
                      aria-label={t('选择这一条')}
                      onCheckedChange={next => togglePicked(item.id, next === true)}
                    />
                    <StateBadge state='warn' label={reviewLabel(item.type)} />
                    <span className='mm-review-target'>{item.target}</span>
                    <span className='mm-muted'>{t('置信度 {n}%', { n: Math.round(item.confidence * 100) })}</span>
                    <span className='mm-muted'>{item.createdText ?? timeText(item.createdAt)}</span>
                  </div>
                  <div className='mm-review-detail'>{reviewDetail(item, projectSlugs)}</div>
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
              {isRefuted(selected) ? (
                <Badge variant='destructive' shape='tag' title={t('人已判定这条是错的，原文与演化链仍保留，可撤销')}>
                  {t('已被判定为错')}
                </Badge>
              ) : null}
              {isDraft(selected) ? (
                <Badge variant='warning' shape='tag' title={t('自动抽取的草稿，还没被反复确认，不会进入检索与注入')}>
                  {t('草稿')}
                </Badge>
              ) : (
                <Badge variant={selected.layer === 'l2' ? 'brand' : 'outline'} shape='tag'>
                  {selected.layer === 'l2' ? t('深层') : t('普通')}
                </Badge>
              )}
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
            {/* 后端在显式 `memoryGet(id)` 时会给草稿附一句 `layerCaveat`。
                读到的模型有可能直接采信正文，所以这句提示要在人能看到的界面上
                也出现一次 —— 界面上「草稿」只是个小标签，容易被当成装饰。 */}
            {selected.layerCaveat ? (
              <div className='mm-muted mm-draft-caveat'>{selected.layerCaveat}</div>
            ) : null}
            {isRefuted(selected) ? (
              <div className='mm-refuted-note'>
                <div>{t('这条已被判定为错误，不会进入检索、注入或画像。')}</div>
                {selected.refutedReason?.trim() ? (
                  <div className='mm-refuted-reason'>{t('理由')}：{selected.refutedReason}</div>
                ) : null}
                {selected.refutedBy ? (
                  <div className='mm-refuted-by'>
                    {t('已由')}{' '}
                    <button type='button' className='mm-link' onClick={() => void openEntry(selected.refutedBy!)}>
                      {selected.refutedBy}
                    </button>{' '}
                    {t('接替')}
                  </div>
                ) : null}
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
              ) : isRefuted(selected) ? (
                <>
                  {/* 已被人判错：只留「撤销」这一条路。撤销后条目恢复常态，
                      那时候再谈采纳 / 星标 / 编辑 / 删除 —— 特别是草稿，采纳必然
                      被后端拒绝，不该让用户点到一个注定失败的按钮。 */}
                  <Button type='button' size='sm' onClick={() => void act(selected.id, async () => {
                    await m.unrefuteMemory(selected.id)
                    await openEntry(selected.id)
                  })}>
                    {t('撤销错误标记')}
                  </Button>
                </>
              ) : (
                <>
                  {/* 「这条错了」是写操作，不一键到底：先展开理由输入框，让人写完
                      （理由可留空）再点确认。确认走 act() 的统一错误提示与刷新。 */}
                  {refuting ? (
                    <>
                      <Input
                        className='mm-refute-reason'
                        value={refuteReason}
                        placeholder={t('为什么错？（可留空）')}
                        onChange={event => setRefuteReason(event.target.value)}
                      />
                      <Button type='button' size='sm' variant='destructive' onClick={() => void act(selected.id, async () => {
                        await m.refuteMemory(selected.id, refuteReason.trim() || undefined)
                        await openEntry(selected.id)
                        setRefuting(false)
                        setRefuteReason('')
                      })}>
                        {t('确认')}
                      </Button>
                      <Button type='button' size='sm' variant='ghost' onClick={() => {
                        setRefuting(false)
                        setRefuteReason('')
                      }}>
                        {t('取消')}
                      </Button>
                    </>
                  ) : (
                    <Button type='button' size='sm' variant='destructive' onClick={() => {
                      setRefuteReason('')
                      setRefuting(true)
                    }}>
                      {t('这条错了')}
                    </Button>
                  )}
                  {/* 草稿只有「认了」这一个正向动作：模型抽出来、还没被反复确认的
                      东西，用户看一眼就知道对不对。缺了这个按钮，草稿就只能等模型
                      下次再抽到一次才升层 —— 人的判断排在机器后面。 */}
                  {isDraft(selected) ? (
                    <Button type='button' size='sm' onClick={() => void act(selected.id, async () => {
                      await m.promoteDraft(selected.id)
                      await openEntry(selected.id)
                    })}>
                      {t('采纳为结论')}
                    </Button>
                  ) : null}
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
                  <Button type='button' size='sm' variant='ghost' onClick={() => {
                    // 「取代」是**新建一条**并把旧的标失效，不能落进 editing 分支
                    setEditing(null)
                    setDraft({
                      content: '',
                      title: `${t('更新')}：${selected.title}`,
                      type: selected.type,
                      project: selected.project ?? undefined,
                      supersedes: [selected.id],
                    })
                  }}>
                    {t('写一条取代它')}
                  </Button>
                  <Button type='button' size='sm' variant='ghost' onClick={() => {
                    setEditing(selected.id)
                    setDraft({
                      content: selected.body ?? '',
                      title: selected.title,
                      type: selected.type,
                      layer: selected.layer,
                      project: selected.project ?? undefined,
                      tags: selected.tags,
                      importance: selected.importance,
                    })
                  }}>
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
                    {/* 草稿不进演化链（后端已过滤），唯一会出现的草稿是「当前这条自己」。
                        标出来是为了不让用户把「模型猜的」当成「当年确认过的」。 */}
                    {item.draft ? <span className='mm-pill draft'>{t('草稿')}</span> : null}
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
                    // 证据锚点分两种：真实记忆 id（点得开），以及强化计数用的
                    // 合成指纹 `distill:<id>` / `derived:<id>`。后者不是记忆，
                    // 做成链接只会点出一片空。渲染成纯文本标签，并说明来源。
                    evidenceLinkable(id) ? (
                      <button key={id} type='button' className='mm-link' onClick={() => void openEntry(id)}>{id}</button>
                    ) : (
                      <span key={id} className='mm-pill muted' title={t('这是强化计数用的锚点，不是一条独立记忆')}>
                        {evidenceLabel(id)}
                      </span>
                    )
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
              <strong>
                {draft.supersedes?.length
                  ? t('写一条取代旧记忆')
                  : editing ? t('编辑这条记忆') : t('新建记忆')}
              </strong>
              <Button type='button' size='sm' variant='ghost' onClick={() => { setDraft(null); setEditing(null) }}>{t('取消')}</Button>
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
              <Button type='button' variant='ghost' onClick={() => { setDraft(null); setEditing(null) }}>{t('取消')}</Button>
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

/** 行尾 `⋯` 菜单：收藏 / 置顶 / 打开 / 删除。回收站有自己的表，不再复用这套。 */
function RowMenu({
  item,
  busy,
  onOpen,
  onStar,
  onPin,
  onDelete,
}: {
  item: m.MemoryEntry
  busy: boolean
  onOpen: () => void
  onStar: () => void
  onPin: () => void
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
            <button type='button' className='mm-pt-menu-item' onClick={() => { close(); onStar() }}>
              {item.starred ? t('取消收藏') : t('收藏')}
            </button>
            <button type='button' className='mm-pt-menu-item' onClick={() => { close(); onPin() }}>
              {item.pinned ? t('取消置顶') : t('置顶')}
            </button>
            <button type='button' className='mm-pt-menu-item is-danger' onClick={() => { close(); onDelete() }}>
              {t('删除（移入回收站）')}
            </button>
          </div>
        </>
      ) : null}
    </span>
  )
}

/* ══════════════════════════════════════════════════════════════
 * 回收站
 * ══════════════════════════════════════════════════════════════ */

/**
 * 五个自动清理开关。key 直接就是配置项（`cleanup.autoDelete*`），
 * 所以「写配置」这一步不需要任何映射表，少一层会漂移的翻译。
 *
 * 默认全关：这五个动作都会把记忆挪走或把行抹掉，误开的代价是静默丢数据。
 */
const CLEANUP_FLAGS: { key: string; label: string; tip: string }[] = [
  {
    key: 'cleanup.autoDeleteSuperseded',
    label: '失效记忆',
    tip: '被新记忆取代、或有效期已过的记忆：移入回收站（保留期内可恢复）',
  },
  {
    key: 'cleanup.autoDeleteMissing',
    label: '不存在记忆',
    tip: '索引里还有、磁盘上文件已丢失的记录：只清索引行（没有内容可入回收站）',
  },
  {
    key: 'cleanup.autoDeleteError',
    label: '报错记忆',
    tip: '文件存在但读取失败（权限 / 损坏）的记忆：整份移入回收站',
  },
  {
    key: 'cleanup.autoDeleteConflict',
    label: '冲突记忆',
    tip: '同步冲突未裁决的记忆：整份移入回收站并清掉冲突记录（每日记录类多节文件保守跳过）',
  },
  {
    key: 'cleanup.autoDeleteInvalid',
    label: '无效记忆',
    tip: '内容为空壳、进不了索引的文件（迁移 / 删除后的残骸）：整份移入回收站',
  },
]

/** `2026-11-19 13:11` —— 回收站表用绝对时间，不用「N 天前」：这列要回答的是
 *  「它什么时候被删的」，相对时间在这里反而要心算。 */
function trashTime(timestamp: number): string {
  if (!timestamp) return '—'
  const date = new Date(timestamp)
  const pad = (value: number): string => String(value).padStart(2, '0')
  return `${date.getFullYear()}-${pad(date.getMonth() + 1)}-${pad(date.getDate())} ${pad(date.getHours())}:${pad(date.getMinutes())}`
}

/** 体积。回收站里大多数文件只有几 KB，但整段会话摘要是几百 KB ——
 *  一律 KB 会出现「2048 KB」这种读不出量级的数字。 */
function sizeText(bytes: number): string {
  const kb = Math.round(bytes / 1024)
  if (kb < 1024) return `${num(kb)} KB`
  return `${(kb / 1024).toFixed(1)} MB`
}

/**
 * 回收站视图。
 *
 * ── 为什么表头是「删除时间 / 原路径 / 体积 / 操作」而不是记忆表那八列 ──
 * 回收站里要回答的问题只有一个：「我删错了哪个、还能不能捞回来」。项目、
 * Agent、标签这些是**检索**维度，在已经删掉的一小堆文件里没有区分价值；
 * 体积和剩余天数才是这里真正需要看的（决定要不要马上恢复 / 要不要清）。
 *
 * ── 为什么原路径由后端在删除那一刻记下 ──────────────────────
 * 恢复时按 `layer` / `kind` 重算路径，对「独立单文件」的历史 l1 条会算到
 * 容器路径上去，捞回来的文件落在错的地方。所以删除时把当时的活路径写进
 * frontmatter（`origin_path`），恢复就只是「搬回去」。
 */
function TrashView({
  onOpen,
  onChanged,
}: {
  onOpen: (id: string) => void
  onChanged: () => void
}): React.ReactElement {
  const cfg = useLoad(() => m.loadConfig(), [])
  const trash = useLoad(() => m.loadTrash(), [])
  const [days, setDays] = React.useState(30)
  const [busy, setBusy] = React.useState('')
  const [note, setNote] = React.useState('')
  const [error, setError] = React.useState('')

  // 配置到手后把保留天数拉进输入框。只在第一次填 —— 之后用户可能正改着，
  // 每次 reload 都覆盖会把输入框里的数字打回去。
  const filled = React.useRef(false)
  React.useEffect(() => {
    if (filled.current || !cfg.data) return
    const raw = cfg.data.values['storage.trashKeepDays']
    const value = typeof raw === 'number' ? raw : Number(raw)
    if (Number.isFinite(value) && value > 0) setDays(value)
    filled.current = true
  }, [cfg.data])

  const flagOn = (key: string): boolean => {
    const raw = cfg.data?.values[key]
    return raw === true || raw === 'true'
  }

  // 提示条 6 秒后自己收：「已恢复到原路径」是一次性反馈，留在界面上会一直占一行，
  // 用户下次进来还会以为刚刚又发生了什么。
  React.useEffect(() => {
    if (!note) return
    const timer = window.setTimeout(() => setNote(''), 6000)
    return () => window.clearTimeout(timer)
  }, [note])

  const saveDays = async (): Promise<void> => {
    const value = Math.min(365, Math.max(7, Math.round(days || 30)))
    setDays(value)
    setBusy('__days__')
    setError('')
    try {
      await m.saveConfig({ 'storage.trashKeepDays': value })
      setNote(t('回收站保留期已设为 {n} 天', { n: value }))
      trash.reload()
    } catch (caught) {
      setError(caught instanceof Error ? caught.message : String(caught))
    } finally {
      setBusy('')
    }
  }

  const toggleFlag = async (key: string, next: boolean): Promise<void> => {
    setBusy(key)
    setError('')
    try {
      await m.saveConfig({ [key]: next })
      setNote(next
        ? t('已开启：每天 04:00 自动清理，也可「立即清理一次」')
        : t('已关闭自动清理'))
      cfg.reload()
    } catch (caught) {
      setError(caught instanceof Error ? caught.message : String(caught))
    } finally {
      setBusy('')
    }
  }

  /** 立即跑一次清理任务。走调度器而不是直接调 `cleanup()`：忙的时候任务会排队，
   *  两个清理同时跑会互相抢同一批文件。 */
  const cleanNow = async (): Promise<void> => {
    setBusy('__clean__')
    setError('')
    try {
      const reply = await m.runAutoTask('cleanup')
      const outcome = reply.outcome
      const result = (outcome.result ?? {}) as Record<string, unknown>
      const trashed = Number(result.supersededTrashed ?? 0)
      const purged = Number(result.trashPurged ?? 0)
      setNote(outcome.status === 'done'
        ? t('清理完成：移入回收站 {a} 条，彻底删除 {b} 个过期文件', { a: trashed, b: purged })
        : outcome.reason || outcome.error || t('清理任务已提交'))
      trash.reload()
      cfg.reload()
      onChanged()
    } catch (caught) {
      setError(caught instanceof Error ? caught.message : String(caught))
    } finally {
      setBusy('')
    }
  }

  const purgeExpired = async (): Promise<void> => {
    setBusy('__purge__')
    setError('')
    try {
      const reply = await m.purgeTrash()
      setNote(t('已清理 {n} 个超期文件', { n: reply.removed ?? 0 }))
      trash.reload()
      onChanged()
    } catch (caught) {
      setError(caught instanceof Error ? caught.message : String(caught))
    } finally {
      setBusy('')
    }
  }

  const restore = async (item: m.TrashItem): Promise<void> => {
    setBusy(item.id)
    setError('')
    try {
      await m.restoreEntry(item.id)
      setNote(t('已恢复到原路径：{path}', { path: item.originPath }))
      trash.reload()
      onChanged()
    } catch (caught) {
      setError(caught instanceof Error ? caught.message : String(caught))
    } finally {
      setBusy('')
    }
  }

  const count = trash.data?.count ?? 0
  const rows = trash.data?.items ?? []
  /** 一次最多画 200 行（与目标界面同口径）。回收站可能有上千个文件，
   *  全量铺进 DOM 会让这张表卡住；剩下的靠「清理超期文件」收掉，
   *  真的要看更早的，路径在磁盘上，列表只是索引。 */
  const shown = rows.slice(0, TRASH_ROWS)

  return (
    <div className='mm-trash'>
      {error ? <div className='mm-error'>{error}</div> : null}

      {/* ── 自动清理卡 ───────────────────────────────────── */}
      <div className='mm-trash-card'>
        <div className='mm-trash-card-head'>
          <strong className='mm-trash-card-title'>{t('自动清理')}</strong>
          <span className='mm-browse-spacer' />
          <span className='mm-trash-keep'>
            <span className='mm-muted'>{t('保留')}</span>
            <Input
              className='mm-trash-days'
              type='number'
              min='7'
              max='365'
              value={String(days)}
              onChange={event => setDays(Number(event.target.value))}
              onKeyDown={event => {
                if (event.key === 'Enter') void saveDays()
              }}
            />
            <span className='mm-muted'>{t('天')}</span>
            <Button
              type='button'
              size='sm'
              variant='ghost'
              disabled={busy === '__days__'}
              onClick={() => void saveDays()}
            >
              {t('保存')}
            </Button>
          </span>
          <Button
            type='button'
            size='sm'
            variant='outline'
            disabled={busy === '__clean__'}
            onClick={() => void cleanNow()}
          >
            {busy === '__clean__' ? t('已提交…') : t('立即清理一次')}
          </Button>
        </div>

        <div className='mm-trash-flags'>
          {CLEANUP_FLAGS.map(flag => (
            <span key={flag.key} className='mm-trash-flag' title={t(flag.tip)}>
              <Switch
                size='sm'
                variant='success'
                checked={flagOn(flag.key)}
                disabled={busy === flag.key || cfg.loading}
                aria-label={t(flag.label)}
                onCheckedChange={next => void toggleFlag(flag.key, next)}
              />
              {/* 标签本身也能点：这个开关只有 24px 宽，要求用户精确点中那颗小圆点
                  是没必要的。用 button 而不是 label —— label 包着 Switch 时点一下
                  会既触发 label 的 click 又触发 Switch 的切换，开关反而「点了没反应」。 */}
              <button
                type='button'
                className='mm-trash-flag-text'
                onClick={() => void toggleFlag(flag.key, !flagOn(flag.key))}
              >
                {t(flag.label)}
              </button>
            </span>
          ))}
        </div>
      </div>

      {/* ── 回收站文件表 ─────────────────────────────────── */}
      <div className='mm-trash-card mm-trash-list'>
        <div className='mm-trash-card-head'>
          <strong className='mm-trash-card-title'>{t('回收站')}</strong>
          <span className='mm-trash-summary'>
            {t('{n} 个文件 · 保留 {days} 天，到期自动彻底删除', { n: num(count), days })}
          </span>
          <span className='mm-browse-spacer' />
          <Button
            type='button'
            size='sm'
            variant='outline'
            className='mm-trash-purge'
            disabled={busy === '__purge__'}
            onClick={() => void purgeExpired()}
          >
            {t('清理超期文件')}
          </Button>
        </div>

        {note ? <div className='mm-trash-note'>{note}</div> : null}

        {trash.error ? (
          <Panel loading={false} error={trash.error} onRetry={trash.reload}>{null}</Panel>
        ) : trash.loading && !trash.data ? (
          <Panel loading error=''>{null}</Panel>
        ) : rows.length === 0 ? (
          <div className='mm-trash-empty'>{t('回收站为空')}</div>
        ) : (
          <div className='mm-trash-scroll'>
            <table className='mm-trash-table'>
              <thead>
                <tr>
                  <th className='mm-tt-time'>{t('删除时间')}</th>
                  <th className='mm-tt-path'>{t('原路径')}</th>
                  <th className='mm-tt-size'>{t('体积')}</th>
                  <th className='mm-tt-actions'>{t('操作')}</th>
                </tr>
              </thead>
              <tbody>
                {shown.map(item => (
                  <tr key={item.id}>
                    <td className='mm-tt-time' title={item.title}>
                      <span className='mm-mono'>{trashTime(item.trashedAt)}</span>
                    </td>
                    <td className='mm-tt-path'>
                      <button
                        type='button'
                        className='mm-tt-path-text'
                        title={`${item.originPath}\n${item.title}`}
                        onClick={() => onOpen(item.id)}
                      >
                        {item.originPath}
                      </button>
                    </td>
                    <td className='mm-tt-size'>{sizeText(item.size)}</td>
                    <td className='mm-tt-actions'>
                      <Button
                        type='button'
                        size='sm'
                        variant='ghost'
                        disabled={busy === item.id}
                        onClick={() => void restore(item)}
                      >
                        {t('恢复')}
                      </Button>
                    </td>
                  </tr>
                ))}
              </tbody>
            </table>
            {count > TRASH_ROWS ? (
              <div className='mm-trash-more'>
                {t('只显示最新的 {shown} 个文件（共 {n} 个）。更早的请先「清理超期文件」。', {
                  shown: TRASH_ROWS,
                  n: num(count),
                })}
              </div>
            ) : null}
          </div>
        )}
      </div>
    </div>
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
          {/* 草稿不进格子：这张图回答的是「我哪天写了东西」。同期由自动抽取
              写下的条数单独说一句，否则用户会发现「库里比图上多」。 */}
          {(data?.candidates ?? 0) > 0
            ? t('（另有 {n} 条自动抽取草稿未计入）', { n: num(data?.candidates ?? 0) })
            : ''}
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

/**
 * 证据锚点里哪些是**真实记忆 id**。
 *
 * `evidence` 现在的语义是「支撑这条结论的原始记忆 / 某次确认」。真实 id 形如
 * `<项目>-<类型>-<短哈希>`，点得开；而蒸馏与近似合并自己写进去的是
 * `distill:<id>` / `derived:<哈希前缀>` —— 那是**强化计数用的指纹**，库里
 * 没有这条记忆。把它们也渲染成链接，用户点一下只会拿到空结果，还以为数据坏了。
 */
function evidenceLinkable(id: string): boolean {
  return !id.startsWith('distill:') && !id.startsWith('derived:') && !id.startsWith('manual:')
}

/** 伪锚点的可读文案：把前缀翻译成人话，后面的 id 保留，便于对账。 */
function evidenceLabel(id: string): string {
  if (id.startsWith('distill:')) return t('蒸馏自 {id}', { id: id.slice('distill:'.length) })
  if (id.startsWith('derived:')) return t('同源确认 {id}', { id: id.slice('derived:'.length) })
  // 人工采纳自己也是「确认了一次」，锚点指回自己 —— 渲染成链接会变成自指。
  if (id.startsWith('manual:')) return t('人工采纳')
  return id
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

/**
 * 队列构成的一句话：`22 条项目归类 · 10 条事实失效 · 8 条蒸馏结论 · 1 条疑似重复`。
 *
 * 为什么要有：41 这个总数不说明任何处置方式。「全是归类」可以整批忽略；
 * 「8 条蒸馏结论」则值得逐条读正文 —— 这两件事在界面上必须一眼能分开。
 */
function reviewTally(items: m.ReviewItem[]): string {
  const counts = new Map<string, number>()
  for (const item of items) counts.set(item.type, (counts.get(item.type) ?? 0) + 1)
  return [...counts]
    .map(([type, n]) => t('{n} 条{label}', { n, label: reviewLabel(type) }))
    .join(' · ')
}

/**
 * 把队列载荷翻译成一句人话。载荷是各任务自己写的，形状不统一，所以逐类处理。
 *
 * `projectSlugs` 是库里真实存在的项目：归类建议如果指向一个不存在的 slug，
 * 批准后 `apply_classification` 会**照着这个 slug 新建目录**（假项目就是这么
 * 复活的）。这种建议必须当场标出来，不能等批完了才发现。
 */
function reviewDetail(item: m.ReviewItem, projectSlugs?: Set<string>): string {
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
      if (!suggestion) return t('建议归类，但载荷里没有目标项目')
      const missing = projectSlugs ? !projectSlugs.has(suggestion) && suggestion !== '_global' : false
      return missing
        ? t('建议归入项目 {slug} —— 这个项目现在不存在，批准会新建它。', { slug: suggestion })
        : t('建议归入项目 {slug}。批准后记忆会被搬到那个项目目录。', { slug: suggestion })
    }
    case 'supersede': {
      const oldId = pick('oldId') || pick('superseded')
      const reason = pick('reason')
      if (!oldId) return t('建议标记旧事实失效，但载荷里没有目标 id')
      return reason
        ? t('新记忆让 {id} 不再成立：{reason} 批准后旧条目被标记为已取代。', { id: oldId, reason })
        : t('新记忆让 {id} 不再成立。批准后旧条目被标记为已取代。', { id: oldId })
    }
    case 'distill':
    case 'consolidate': {
      // 这两类的载荷带的是**成品正文**（`body`）与证据 id 列表。以前落到
      // `JSON.stringify`，界面上是一串转义 JSON —— 用户没法读，也就没法裁决。
      const body = pick('body')
      const evidence = Array.isArray(payload.evidence) ? payload.evidence.length : 0
      if (!body) return t('建议沉成一条结论，但载荷里没有正文')
      const clipped = body.length > 160 ? `${body.slice(0, 160)}…` : body
      return evidence
        ? t('依据 {n} 条记忆写成：{body}', { n: evidence, body: clipped })
        : t('写成：{body}', { body: clipped })
    }
    default:
      return JSON.stringify(payload).slice(0, 200)
  }
}

export { Browse }
