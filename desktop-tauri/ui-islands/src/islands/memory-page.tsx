import * as React from 'react'
import { mountOnPage } from '../island-lazy'
import { createRoot } from 'react-dom/client'
import { Badge, Button, Input, Spinner, Switch, Textarea } from '@ui'
import { t } from '../i18n'
import * as m from './memory-api'

/**
 * Agent2API · 记忆中枢（页面岛）。
 *
 * ── 为什么在网关里，而不是插件里 ────────────────────────────
 * 与签到中心同一决定、同一理由（见 `checkin-page.tsx` 的模块头）：插件只能出
 * 静态 HTML、跑在 iframe 里，读不到宿主的 CSS 变量、拿不到组件库实例；
 * 而「记忆」的后端（存储 + 检索 + 注入）本来就只能由网关提供 —— 插件连端点
 * 都加不了。所以页面归网关，插件不参与本功能。
 *
 * ── 五个视图为什么是「同级平铺」而不是设置里的子页 ──────────
 * 记忆的日常动作有三种：找一条（记忆库）、看有哪些项目（项目）、看模型会收到
 * 什么（核心记忆）。三者地位相同、都要一眼可达，藏进设置里的二级页会让人
 * 忘了它的存在。回收站与设置是低频项，排在后面。
 *
 * ── 为什么「核心记忆」要给用户看原文 ────────────────────────
 * 注入开关打开之后会改写发给上游的请求体。用户必须能直接看到「模型到底收到
 * 了哪几行」——否则开关等于盲开，出了问题也没法判断是记忆写坏了还是注入拼
 * 坏了。这一页就是那个「所见即所发」的窗口。
 */

type Tab = 'library' | 'projects' | 'core' | 'trash' | 'settings'

const TABS: { key: Tab; label: string }[] = [
  { key: 'library', label: '记忆库' },
  { key: 'projects', label: '项目' },
  { key: 'core', label: '核心记忆' },
  { key: 'trash', label: '回收站' },
  { key: 'settings', label: '设置' },
]

function timeText(timestamp: number): string {
  if (!timestamp) return '—'
  const delta = Date.now() - timestamp
  if (delta < 60_000) return '刚刚'
  if (delta < 3_600_000) return `${Math.floor(delta / 60_000)} 分钟前`
  if (delta < 86_400_000) return `${Math.floor(delta / 3_600_000)} 小时前`
  if (delta < 30 * 86_400_000) return `${Math.floor(delta / 86_400_000)} 天前`
  return new Date(timestamp).toLocaleDateString()
}

function MemoryPage(): React.ReactElement {
  const [tab, setTab] = React.useState<Tab>('library')
  const [kinds, setKinds] = React.useState<m.MemoryKindOption[]>([])
  const [status, setStatus] = React.useState<m.MemoryStatus | null>(null)
  const [config, setConfig] = React.useState<m.MemoryConfig | null>(null)
  const [error, setError] = React.useState('')
  const [busy, setBusy] = React.useState(false)

  const [query, setQuery] = React.useState('')
  const [kind, setKind] = React.useState('')
  const [project, setProject] = React.useState('')
  const [list, setList] = React.useState<m.MemoryEntry[]>([])
  const [selected, setSelected] = React.useState<m.MemoryEntry | null>(null)
  const [timeline, setTimeline] = React.useState<m.MemoryTimelineItem[]>([])
  const [projects, setProjects] = React.useState<m.MemoryProject[]>([])
  const [core, setCore] = React.useState<{ text: string; tokens: number; truncated: boolean } | null>(null)
  const [draft, setDraft] = React.useState<m.WritePayload | null>(null)

  const run = React.useCallback(async (task: () => Promise<void>) => {
    setBusy(true)
    setError('')
    try {
      await task()
    } catch (caught) {
      setError(caught instanceof Error ? caught.message : String(caught))
    } finally {
      setBusy(false)
    }
  }, [])

  const refreshStatus = React.useCallback(async () => {
    const next = await m.loadStatus()
    setStatus(next)
    setConfig(next.config)
  }, [])

  const refreshList = React.useCallback(async (deleted: boolean) => {
    const page = await m.loadList({
      query: query.trim(),
      type: kind,
      project: project.trim(),
      includeDeleted: deleted,
      includeSuperseded: deleted,
      limit: 200,
    })
    setList(page.items)
    setSelected(current => (current ? page.items.find(item => item.id === current.id) ?? null : null))
  }, [query, kind, project])

  React.useEffect(() => {
    void run(async () => {
      const payload = await m.loadConfig()
      setKinds(payload.kinds)
      setStatus(payload.status)
      setConfig(payload.config)
      const page = await m.loadList({ limit: 200 })
      setList(page.items)
      const items = await m.loadProjects()
      setProjects(items.items)
    })
    // 只在首次挂载时拉一次；筛选变化由 refreshList 显式触发
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [])

  const openEntry = React.useCallback((id: string) => {
    void run(async () => {
      const payload = await m.loadEntry(id)
      const entry = payload.items[0] ?? null
      setSelected(entry)
      setTimeline([])
      if (entry) {
        const chain = await m.loadTimeline(entry.id)
        setTimeline(chain.items)
      }
    })
  }, [run])

  const switchTab = (next: Tab) => {
    setTab(next)
    void run(async () => {
      if (next === 'library') await refreshList(false)
      if (next === 'trash') await refreshList(true)
      if (next === 'projects') setProjects((await m.loadProjects()).items)
      if (next === 'core') setCore(await m.loadCore(undefined, config?.injectTokens))
      if (next === 'settings') await refreshStatus()
    })
  }

  const saveDraft = () => {
    if (!draft) return
    if (!draft.content.trim()) {
      setError('正文不能为空')
      return
    }
    void run(async () => {
      await m.writeEntry(draft)
      setDraft(null)
      await refreshList(tab === 'trash')
      await refreshStatus()
    })
  }

  return (
    <div className='mm-page'>
      <div className='mm-head'>
        <div className='mm-tabs'>
          {TABS.map(item => (
            <Button
              key={item.key}
              type='button'
              size='sm'
              variant={tab === item.key ? 'default' : 'ghost'}
              onClick={() => switchTab(item.key)}
            >
              {t(item.label)}
            </Button>
          ))}
        </div>
        <div className='mm-actions'>
          {busy ? <Spinner /> : null}
          <Button
            type='button'
            size='sm'
            variant='ghost'
            onClick={() => void run(async () => {
              if (tab === 'core') setCore(await m.loadCore(undefined, config?.injectTokens))
              else if (tab === 'projects') setProjects((await m.loadProjects()).items)
              else await refreshList(tab === 'trash')
            })}
          >
            {t('刷新')}
          </Button>
          <Button type='button' size='sm' onClick={() => setDraft({ content: '', type: 'daily', project: project || undefined })}>
            {t('新建记忆')}
          </Button>
        </div>
      </div>

      {error ? <div className='mm-error'>{error}</div> : null}

      {tab === 'library' || tab === 'trash' ? (
        <div className='mm-library'>
          <div className='mm-filters'>
            <Input
              value={query}
              placeholder={t('检索记忆（支持中文按字匹配）')}
              onChange={event => setQuery(event.target.value)}
              onKeyDown={event => { if (event.key === 'Enter') void run(async () => refreshList(tab === 'trash')) }}
            />
            <select className='mm-select' value={kind} onChange={event => setKind(event.target.value)}>
              <option value=''>{t('全部类别')}</option>
              {kinds.map(option => <option key={option.value} value={option.value}>{t(option.label)}</option>)}
            </select>
            <Input
              value={project}
              placeholder={t('项目 slug')}
              onChange={event => setProject(event.target.value)}
              className='mm-project-input'
            />
            <Button type='button' size='sm' variant='ghost' onClick={() => void run(async () => refreshList(tab === 'trash'))}>
              {t('搜索')}
            </Button>
          </div>

          <div className='mm-split'>
            <div className='mm-list'>
              {list.length === 0 && !busy ? (
                <div className='mm-empty'>
                  {tab === 'trash' ? t('回收站是空的。') : t('还没有记忆。点右上角「新建记忆」，或在别处用接口写入。')}
                </div>
              ) : null}
              {list.map(item => (
                <button
                  key={item.id}
                  type='button'
                  className={`mm-row${selected?.id === item.id ? ' active' : ''}`}
                  onClick={() => openEntry(item.id)}
                >
                  <div className='mm-row-top'>
                    <span className='mm-row-title'>{item.title}</span>
                    <Badge variant='secondary' shape='tag'>{t(item.typeLabel)}</Badge>
                    {item.pinned ? <Badge variant='destructive' shape='tag'>{t('置顶')}</Badge> : null}
                    {item.starred ? <Badge variant='outline' shape='tag'>{t('星标')}</Badge> : null}
                  </div>
                  <div className='mm-row-summary'>{item.summary || t('（无摘要）')}</div>
                  <div className='mm-row-meta'>
                    {item.project ? <span>{item.project}</span> : <span>{t('全局')}</span>}
                    <span>{timeText(item.updatedAt)}</span>
                    {item.supersededBy ? <span className='mm-warn'>{t('已被取代')}</span> : null}
                  </div>
                </button>
              ))}
            </div>

            <div className='mm-detail'>
              {!selected ? <div className='mm-empty'>{t('从左侧选一条记忆查看全文。')}</div> : (
                <>
                  <div className='mm-detail-head'>
                    <h3>{selected.title}</h3>
                    <div className='mm-detail-meta'>
                      <Badge variant='secondary' shape='tag'>{t(selected.typeLabel)}</Badge>
                      {selected.project ? <span>{selected.project}</span> : <span>{t('全局')}</span>}
                      <span>{t('重要度')} {selected.importance.toFixed(2)}</span>
                      <span>{timeText(selected.updatedAt)}</span>
                      <span>{selected.chars} {t('字')}</span>
                    </div>
                    {selected.tags.length ? (
                      <div className='mm-tags'>{selected.tags.map(tag => <Badge key={tag} variant='outline' shape='tag'>{tag}</Badge>)}</div>
                    ) : null}
                  </div>

                  <pre className='mm-body'>{selected.body ?? selected.summary}</pre>

                  <div className='mm-detail-actions'>
                    {selected.deletedAt ? (
                      <>
                        <Button type='button' size='sm' onClick={() => void run(async () => { await m.restoreEntry(selected.id); await refreshList(tab === 'trash'); await refreshStatus() })}>
                          {t('恢复')}
                        </Button>
                        <Button type='button' size='sm' variant='destructive' onClick={() => void run(async () => { await m.purgeEntry(selected.id); setSelected(null); await refreshList(tab === 'trash') })}>
                          {t('彻底删除')}
                        </Button>
                      </>
                    ) : (
                      <>
                        <Button type='button' size='sm' variant='ghost' onClick={() => void run(async () => {
                          await m.updateEntry(selected.id, { starred: !selected.starred })
                          await refreshList(false)
                        })}>
                          {selected.starred ? t('取消星标') : t('星标')}
                        </Button>
                        <Button type='button' size='sm' variant='ghost' onClick={() => void run(async () => {
                          await m.updateEntry(selected.id, { pinned: !selected.pinned })
                          await refreshList(false)
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
                        <Button type='button' size='sm' variant='destructive' onClick={() => void run(async () => {
                          await m.deleteEntry(selected.id)
                          setSelected(null)
                          await refreshList(false)
                          await refreshStatus()
                        })}>
                          {t('删除')}
                        </Button>
                      </>
                    )}
                  </div>

                  {timeline.length > 1 ? (
                    <div className='mm-timeline'>
                      <div className='mm-section-title'>{t('事实演化链')}</div>
                      {timeline.map(item => (
                        <div key={item.id} className={`mm-timeline-row${item.current ? ' current' : ''}`}>
                          <span className='mm-timeline-date'>{item.validFromText}</span>
                          <button type='button' className='mm-link' onClick={() => openEntry(item.id)}>{item.title}</button>
                          <span className='mm-timeline-type'>{t(item.typeLabel)}</span>
                        </div>
                      ))}
                    </div>
                  ) : null}

                  {selected.related?.length ? (
                    <div className='mm-related'>
                      <div className='mm-section-title'>{t('相关记忆')}</div>
                      {selected.related.map(item => (
                        <button key={item.id} type='button' className='mm-link' onClick={() => openEntry(item.id)}>{item.line}</button>
                      ))}
                    </div>
                  ) : null}
                </>
              )}
            </div>
          </div>
        </div>
      ) : null}

      {tab === 'projects' ? (
        <div className='mm-projects'>
          {projects.length === 0 ? <div className='mm-empty'>{t('还没有项目。写入记忆时带上 project 就会自动登记。')}</div> : null}
          {projects.map(item => (
            <div key={item.slug} className='mm-project-card'>
              <div className='mm-project-head'>
                <strong>{item.name}</strong>
                <Badge variant='secondary' shape='tag'>{item.count} {t('条')}</Badge>
              </div>
              <div className='mm-project-meta'>
                <span>slug: {item.slug}</span>
                <span>{item.updatedText}</span>
              </div>
              {item.remote ? <div className='mm-project-remote'>{item.remote}</div> : <div className='mm-project-remote mm-muted'>{t('未记录远程地址')}</div>}
              {item.latest.length ? (
                <ul className='mm-project-latest'>
                  {item.latest.map(entry => (
                    <li key={entry.id}>
                      <button type='button' className='mm-link' onClick={() => { setTab('library'); openEntry(entry.id) }}>{entry.title}</button>
                    </li>
                  ))}
                </ul>
              ) : null}
            </div>
          ))}
        </div>
      ) : null}

      {tab === 'core' ? (
        <div className='mm-core'>
          <div className='mm-core-head'>
            <div>
              <div className='mm-section-title'>{t('注入内容的实际文本')}</div>
              <div className='mm-muted'>
                {core ? `${core.tokens} token（估算）${core.truncated ? ' · 已按预算截断' : ''}` : ''}
              </div>
            </div>
            <Button type='button' size='sm' variant='ghost' onClick={() => void run(async () => setCore(await m.loadCore(undefined, config?.injectTokens)))}>
              {t('按当前预算重算')}
            </Button>
          </div>
          <pre className='mm-body'>{core?.text || t('（没有可注入的内容：先写几条记忆）')}</pre>
        </div>
      ) : null}

      {tab === 'settings' && config ? (
        <div className='mm-settings'>
          <div className='mm-field'>
            <label>{t('转发时注入记忆')}</label>
            <Switch checked={config.injectEnabled} onCheckedChange={value => void run(async () => {
              const saved = await m.saveConfig({ injectEnabled: value })
              setConfig(saved.config)
            })} />
            <p className='mm-hint'>{t('关闭时完全不碰请求体。开启后会改写发给上游的 system 消息，请先用「核心记忆」页确认内容。')}</p>
          </div>

          <div className='mm-field'>
            <label>{t('注入方式')}</label>
            <select className='mm-select' value={config.injectMode} onChange={event => void run(async () => {
              const saved = await m.saveConfig({ injectMode: event.target.value })
              setConfig(saved.config)
            })}>
              <option value='core'>{t('核心记忆（画像 + 项目要点）')}</option>
              <option value='search'>{t('按本轮输入检索后注入')}</option>
            </select>
          </div>

          <div className='mm-field'>
            <label>{t('注入预算（token）')}</label>
            <Input
              type='number'
              value={String(config.injectTokens)}
              onChange={event => {
                const value = Number(event.target.value)
                if (!Number.isFinite(value)) return
                void run(async () => {
                  const saved = await m.saveConfig({ injectTokens: Math.round(value) })
                  setConfig(saved.config)
                })
              }}
            />
            <p className='mm-hint'>{t('200–4000。超出预算的条目会被整条丢掉，不会截半句。')}</p>
          </div>

          <div className='mm-field'>
            <label>{t('只注入这个项目')}</label>
            <Input
              value={config.injectProject ?? ''}
              placeholder={t('留空 = 全部项目')}
              onChange={event => setProject(event.target.value)}
              onBlur={() => void run(async () => {
                const saved = await m.saveConfig({ injectProject: project.trim() || null })
                setConfig(saved.config)
              })}
            />
          </div>

          <div className='mm-status'>
            <div className='mm-section-title'>{t('状态')}</div>
            <div className='mm-status-grid'>
              <span>{t('记忆文件根')}</span><code>{status?.root ?? '—'}</code>
              <span>{t('派生索引')}</span><code>{status?.indexFile ?? '—'}</code>
              <span>{t('条数')}</span><code>{status ? `${status.live} 有效 / ${status.deleted} 回收站 / ${status.superseded} 已取代` : '—'}</code>
              <span>{t('索引')}</span><code>{status ? `${status.index.count} 条 · ${status.index.fts ? 'FTS5' : '内存匹配'} · ${status.index.inSync ? '与事实源一致' : '待重建'}` : '—'}</code>
            </div>
            <div className='mm-detail-actions'>
              <Button type='button' size='sm' variant='ghost' onClick={() => void run(async () => {
                await m.rebuildIndex()
                await refreshStatus()
              })}>
                {t('重建索引')}
              </Button>
            </div>
            <p className='mm-hint'>{t('索引是派生数据：删掉 index.db 后重建即可，记忆正文在 .md 文件里。')}</p>
          </div>
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
              <Input value={draft.title ?? ''} placeholder={t('留空则取正文第一行')} onChange={event => setDraft({ ...draft, title: event.target.value })} />

              <div className='mm-dialog-row'>
                <div>
                  <label className='mm-label'>{t('类别')}</label>
                  <select className='mm-select' value={draft.type ?? 'daily'} onChange={event => setDraft({ ...draft, type: event.target.value })}>
                    {kinds.map(option => <option key={option.value} value={option.value}>{t(option.label)}</option>)}
                  </select>
                </div>
                <div>
                  <label className='mm-label'>{t('项目')}</label>
                  <Input value={draft.project ?? ''} placeholder={t('可留空 = 全局')} onChange={event => setDraft({ ...draft, project: event.target.value })} />
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

              <label className='mm-label'>{t('标签（逗号分隔）')}</label>
              <Input
                value={(draft.tags ?? []).join(', ')}
                onChange={event => setDraft({ ...draft, tags: event.target.value.split(',').map(item => item.trim()).filter(Boolean) })}
              />

              <label className='mm-label'>{t('正文（Markdown）')}</label>
              <Textarea
                rows={12}
                value={draft.content}
                placeholder={t('写「为什么」而不是「是什么」：决策取舍、避坑经验、接口规矩。')}
                onChange={event => setDraft({ ...draft, content: event.target.value })}
              />
              {draft.supersedes?.length ? (
                <p className='mm-hint'>{t('保存后旧记忆会被标记为「已被取代」，仍可查看，不进回收站。')}</p>
              ) : null}
            </div>
            <div className='mm-dialog-foot'>
              <Button type='button' variant='ghost' onClick={() => setDraft(null)}>{t('取消')}</Button>
              <Button type='button' onClick={saveDraft}>{t('保存')}</Button>
            </div>
          </div>
        </div>
      ) : null}
    </div>
  )
}

/* ─── 挂载 ─────────────────────────────────── */

const PAGE_SELECTOR = '.page[data-page="memory"]'

let mounted = false

function mount(): void {
  if (mounted) return
  const section = document.querySelector<HTMLElement>(PAGE_SELECTOR)
  if (!section) return
  mounted = true
  section.replaceChildren()
  createRoot(section).render(<MemoryPage />)
}

mountOnPage('memory', PAGE_SELECTOR, mount)
