import * as React from 'react'
import { Badge, Button, Input, Textarea } from '@ui'
import { t } from '../i18n'
import * as m from './memory-api'
import { Empty, Panel, Section, StateBadge, timeText, useLoad } from './memory-shared'

/**
 * 记忆浏览：列表 + 详情 + 待确认收件箱，同屏。
 *
 * ── 为什么把「待确认」塞进浏览页而不是单独一页 ──────────────
 * 裁决一条建议（「这条重复了」「这条该归到 X 项目」）必须能同时看到**建议**和
 * **被建议的那条记忆**。分成两页就得来回跳，跳两次之后用户就不裁了 —— 队列于是
 * 永久积压。同屏的代价只是左侧多一个折叠区。
 *
 * ── 为什么编辑用的是弹层而不是右侧就地编辑 ──────────────────
 * 记忆正文是 Markdown，就地编辑会让详情区高度随内容乱跳，而且「改一半切走了」
 * 没法收尾。弹层有明确的保存/取消边界，也复用了「新建记忆」的同一套表单。
 *
 * ── 为什么「写一条取代它」是一个显眼按钮 ────────────────────
 * 记忆系统最怕的不是写错，是**旧结论一直留着**。发现一条过时记忆时，最顺手的
 * 动作应该是「写下新的、把旧的标为已取代」，而不是「删掉旧的」——删掉就丢了
 * 演化链，将来没人知道这条结论为什么变了。
 */

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
  const [query, setQuery] = React.useState('')
  const [kind, setKind] = React.useState('')
  const [project, setProject] = React.useState('')
  const [layer, setLayer] = React.useState('')
  const [trash, setTrash] = React.useState(false)
  const [selected, setSelected] = React.useState<m.MemoryEntry | null>(null)
  const [timeline, setTimeline] = React.useState<m.MemoryTimelineItem[]>([])
  const [draft, setDraft] = React.useState<m.WritePayload | null>(null)
  const [error, setError] = React.useState('')
  const [busy, setBusy] = React.useState(false)

  const list = useLoad(
    () => m.loadList({
      query: query.trim(),
      type: kind,
      project: project.trim(),
      layer,
      includeDeleted: trash,
      includeSuperseded: trash,
      limit: 300,
    }),
    [query, kind, project, layer, trash],
  )

  const review = useLoad(() => m.loadReview('pending', undefined, 40), [])

  const openEntry = React.useCallback(async (id: string) => {
    setBusy(true)
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
      setBusy(false)
    }
  }, [])

  // 项目页点标题跳过来时，直接打开那条
  React.useEffect(() => {
    if (!focusId) return
    void openEntry(focusId)
    onFocusConsumed?.()
  }, [focusId, openEntry, onFocusConsumed])

  // 项目页「查看记忆」跳过来时，把项目筛选落到下拉框上
  //
  // 消费后立刻清掉外层的 `focusProject`，否则用户手动换一个项目，下一次
  // 任何重渲染都会把它顶回旧值 —— 筛选框看起来「不听话」。
  React.useEffect(() => {
    if (!focusProject) return
    setProject(focusProject)
    onFocusProjectConsumed?.()
  }, [focusProject, onFocusProjectConsumed])

  const saveDraft = async () => {
    if (!draft) return
    if (!draft.content.trim()) {
      setError(t('正文不能为空'))
      return
    }
    setBusy(true)
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
      setBusy(false)
    }
  }

  const act = async (task: () => Promise<unknown>) => {
    setBusy(true)
    setError('')
    try {
      await task()
      list.reload()
      review.reload()
      onChanged()
    } catch (caught) {
      setError(caught instanceof Error ? caught.message : String(caught))
    } finally {
      setBusy(false)
    }
  }

  return (
    <div className='mm-browse'>
      {error ? <div className='mm-error'>{error}</div> : null}

      <div className='mm-filters'>
        <Input
          value={query}
          placeholder={t('检索记忆（中文按字匹配）')}
          onChange={event => setQuery(event.target.value)}
        />
        <select className='mm-select' value={kind} onChange={event => setKind(event.target.value)}>
          <option value=''>{t('全部类别')}</option>
          {kinds.map(option => <option key={option.value} value={option.value}>{option.label}</option>)}
        </select>
        <select className='mm-select' value={layer} onChange={event => setLayer(event.target.value)}>
          <option value=''>{t('全部层级')}</option>
          <option value='l1'>{t('l1 原始流水')}</option>
          <option value='l2'>{t('l2 沉淀结论')}</option>
        </select>
        <select className='mm-select mm-project-select' value={project} onChange={event => setProject(event.target.value)}>
          <option value=''>{t('全部项目')}</option>
          {/* 通用项目不在项目列表里（它不是一张可维护的卡），但仍要能按它筛 */}
          <option value='_global'>{t('通用（general）')}</option>
          {projects.map(item => (
            <option key={item.slug} value={item.slug}>{item.name}（{item.count}）</option>
          ))}
        </select>
        <Button
          type='button'
          size='sm'
          variant={trash ? 'default' : 'ghost'}
          onClick={() => setTrash(value => !value)}
        >
          {trash ? t('正在看回收站') : t('回收站')}
        </Button>
        <Button
          type='button'
          size='sm'
          onClick={() => setDraft({ content: '', type: 'daily', project: project || undefined })}
        >
          {t('新建记忆')}
        </Button>
      </div>

      {review.data && review.data.items.length > 0 ? (
        <Section
          title={t('待确认（{n}）', { n: review.data.pending })}
          hint={t('AI 的建议不会自动落盘。批准才会改数据，拒绝只改队列状态 —— 任何裁决都能反悔。')}
          action={(
            <Button
              type='button'
              size='sm'
              variant='ghost'
              onClick={() => void act(() => m.rejectAllReview())}
            >
              {t('全部忽略')}
            </Button>
          )}
        >
          <div className='mm-review-list'>
            {review.data.items.map(item => (
              <div key={item.id} className='mm-review'>
                <div className='mm-review-head'>
                  <StateBadge state='warn' label={reviewLabel(item.type)} />
                  <span className='mm-review-target'>{item.target}</span>
                  <span className='mm-muted'>{t('置信度 {n}%', { n: Math.round(item.confidence * 100) })}</span>
                  <span className='mm-muted'>{item.createdText ?? timeText(item.createdAt)}</span>
                </div>
                <div className='mm-review-detail'>{reviewDetail(item)}</div>
                <div className='mm-review-actions'>
                  <Button type='button' size='sm' onClick={() => void act(() => m.resolveReview(item.id, true))}>
                    {t('批准')}
                  </Button>
                  <Button type='button' size='sm' variant='ghost' onClick={() => void act(() => m.resolveReview(item.id, false))}>
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
      ) : null}

      <div className='mm-split'>
        <div className='mm-list'>
          {list.loading ? <Panel loading error=''>{null}</Panel> : null}
          {list.error ? <Panel loading={false} error={list.error} onRetry={list.reload}>{null}</Panel> : null}
          {!list.loading && !list.error && (list.data?.items.length ?? 0) === 0 ? (
            <Empty>
              {trash
                ? t('回收站是空的。')
                : t('没有匹配的记忆。换个关键词，或者点「新建记忆」写第一条。')}
            </Empty>
          ) : null}
          {list.data?.items.map(item => (
            <button
              key={item.id}
              type='button'
              className={`mm-row${selected?.id === item.id ? ' active' : ''}`}
              onClick={() => void openEntry(item.id)}
            >
              <div className='mm-row-top'>
                <span className='mm-row-title'>{item.title}</span>
                <Badge variant={item.layer === 'l2' ? 'brand' : 'secondary'} shape='tag'>{item.typeLabel}</Badge>
                {item.pinned ? <Badge variant='destructive' shape='tag'>{t('置顶')}</Badge> : null}
                {item.starred ? <Badge variant='outline' shape='tag'>{t('星标')}</Badge> : null}
                {item.supersededBy ? <Badge variant='warning' shape='tag'>{t('已被取代')}</Badge> : null}
              </div>
              <div className='mm-row-summary'>{item.summary || t('（无摘要）')}</div>
              <div className='mm-row-meta'>
                <span>{item.project ?? t('全局')}</span>
                <span>{timeText(item.updatedAt)}</span>
                {item.validTo ? <span className='mm-warn'>{t('有效期至 {when}', { when: timeText(item.validTo) })}</span> : null}
              </div>
            </button>
          ))}
        </div>

        <div className='mm-detail'>
          {!selected ? <Empty>{t('从左侧选一条记忆看全文。')}</Empty> : (
            <>
              <div className='mm-detail-head'>
                <h3>{selected.title}</h3>
                <div className='mm-detail-meta'>
                  <Badge variant='secondary' shape='tag'>{selected.typeLabel}</Badge>
                  <Badge variant='outline' shape='tag'>{selected.layer}</Badge>
                  <span>{selected.project ?? t('全局')}</span>
                  <span>{t('重要度 {n}', { n: selected.importance.toFixed(2) })}</span>
                  <span>{timeText(selected.updatedAt)}</span>
                  <span>{selected.chars} {t('字')}</span>
                  {selected.agent ? <span>{selected.agent}</span> : null}
                </div>
                {selected.tags.length ? (
                  <div className='mm-tags'>
                    {selected.tags.map(tag => <Badge key={tag} variant='outline' shape='tag'>{tag}</Badge>)}
                  </div>
                ) : null}
                {selected.files.length ? (
                  <div className='mm-files'>
                    {selected.files.map(file => <code key={file}>{file}</code>)}
                  </div>
                ) : null}
              </div>

              <pre className='mm-body'>{selected.body ?? selected.summary}</pre>

              <div className='mm-detail-actions'>
                {selected.deletedAt ? (
                  <>
                    <Button type='button' size='sm' onClick={() => void act(() => m.restoreEntry(selected.id))}>
                      {t('恢复')}
                    </Button>
                    <Button type='button' size='sm' variant='destructive' onClick={() => void act(async () => {
                      await m.purgeEntry(selected.id)
                      setSelected(null)
                    })}>
                      {t('彻底删除')}
                    </Button>
                  </>
                ) : (
                  <>
                    <Button type='button' size='sm' variant='ghost' onClick={() => void act(async () => {
                      await m.updateEntry(selected.id, { starred: !selected.starred })
                      await openEntry(selected.id)
                    })}>
                      {selected.starred ? t('取消星标') : t('星标')}
                    </Button>
                    <Button type='button' size='sm' variant='ghost' onClick={() => void act(async () => {
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
                    <Button type='button' size='sm' variant='destructive' onClick={() => void act(async () => {
                      await m.deleteEntry(selected.id)
                      setSelected(null)
                    })}>
                      {t('删除')}
                    </Button>
                  </>
                )}
              </div>

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
            </>
          )}
        </div>
      </div>

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
              <Button type='button' onClick={() => void saveDraft()}>{busy ? t('保存中…') : t('保存')}</Button>
            </div>
          </div>
        </div>
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
