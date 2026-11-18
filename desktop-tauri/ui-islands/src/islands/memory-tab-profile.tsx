import * as React from 'react'
import { Badge, Button, Input, Textarea } from '@ui'
import { t } from '../i18n'
import * as m from './memory-api'
import { Panel, StateBadge, num, timeText, useLoad } from './memory-shared'

/**
 * 画像页：四类长期事实（人格特质 / 沟通偏好 / 技术偏好 / 工作习惯）。
 *
 * ── 画像为什么值得单独一页 ──────────────────────────────────
 * 普通记忆是「关于项目的事实」，画像是「关于这个人的事实」。它的消费方式不同：
 * 前者的读者是下一次会话的模型，后者的读者是**用户自己**——人想确认「系统对
 * 我的理解对不对」。所以这一页的排版重心是「每条结论都能点回证据」，而不是列表
 * 密度。
 *
 * ── 为什么是「四宫格 + 单面板」而不是四张纵向长卡 ────────────
 * 四类各自都有 0-6 条结论，纵向堆四张卡的结果是：用户永远只看得见第一类，
 * 另外三类要滚半屏才知道存在 —— 而「我到底被总结成了什么样的人」这个问题
 * 必须一眼看到全貌。四宫格先把四类的**规模**（各几条）摊开，点谁才展开谁，
 * 于是「哪一类是空的」也成了一个可见的事实，而不是要滚下去才发现。
 *
 * ── 为什么每条都要带证据链 ──────────────────────────────────
 * 模型总结人格最容易犯的错是「从一件事推整个人」。让每条结论都挂着它引用的
 * 记忆 id，用户一眼就能判断「这条是过度推断」并删掉。没有证据链的画像不可信，
 * 不可信就不该被注入。证据 chip 点一下就跳到浏览页的原文。
 *
 * ── 为什么「置顶」是一个独立动作，而不是排序偏好 ────────────
 * 重新生成是整批替换。用户钉住一条结论时表达的是「这句我认，别被模型洗掉」，
 * 所以置顶必须**活过重生成**（后端按 `pin` 标记跳过它）。这也意味着置顶不能
 * 随手就点 —— 它改变的是下一次重算的输入，不是眼睛看到的位置。
 *
 * ── 为什么门槛没到时不能强制生成 ────────────────────────────
 * 5 条记忆生成出来的「画像」必然是把偶然当必然。门槛（`deep.personaMinMemories`）
 * 是质量下限，不是性能优化 —— 但用户坚持要看也行，所以「重新生成」按钮保留，
 * 只是明确标注「样本不足，结论可能不准」。
 */

/**
 * 四类的展示元数据。
 *
 * `key` 必须与后端 `PROFILE_SUBS` 一字不差：它同时是「单独生成」的请求参数与
 * 条目上的分组键。`label` / `hint` 由后端随视图一起下发（同一份措辞两处维护
 * 必然漂移），这里只保留**后端没给时**的兜底 —— 老后端 + 新前端也画得出来。
 */
const SUB_FALLBACK: { key: string; label: string; hint: string }[] = [
  { key: 'identity', label: '人格特质', hint: '思维倾向、决策模式与协作个性' },
  { key: 'communication', label: '沟通偏好', hint: '说话方式、信息密度与反馈节奏' },
  { key: 'technical', label: '技术偏好', hint: '技术选型、工具链与代码审美' },
  { key: 'workstyle', label: '工作习惯', hint: '排查路径、验证方式与时间安排' },
]

/** 证据 id → 界面上那个短锚点（`#dc1b9c6b`）。 */
function shortId(id: string): string {
  const tail = id.split('-').pop() ?? id
  return `#${tail.slice(-8)}`
}

/**
 * 证据跳转：把浏览页拉到那条原文。
 *
 * 为什么用事件而不是 props：证据 chip 在第四层子组件里，把 `switchTab` 一层层
 * 传下去会让每个中间层多一个自己不用的 prop。事件是解耦的 —— 页面壳已经在听
 * `mm:tab`（同步页的「修改凭据」也走它），这里只是把 `focusId` 一起捎上。
 */
function openEntry(id: string): void {
  window.dispatchEvent(new CustomEvent('mm:tab', { detail: { tab: 'browse', focusId: id } }))
}

function SubIcon({ sub }: { sub: string }): React.ReactElement {
  const common = {
    width: 15,
    height: 15,
    viewBox: '0 0 24 24',
    fill: 'none',
    stroke: 'currentColor',
    strokeWidth: 1.7,
    strokeLinecap: 'round' as const,
    strokeLinejoin: 'round' as const,
    'aria-hidden': true,
  }
  if (sub === 'communication') {
    return (
      <svg {...common}>
        <path d='M20 14.5a2.5 2.5 0 0 1-2.5 2.5H9l-4 3v-3H6a2 2 0 0 1-2-2V6.5A2.5 2.5 0 0 1 6.5 4h11A2.5 2.5 0 0 1 20 6.5z' />
        <path d='M8 9h8M8 12.5h5' />
      </svg>
    )
  }
  if (sub === 'technical') {
    return (
      <svg {...common}>
        <circle cx='12' cy='12' r='3.2' />
        <path d='M12 2.8v2.6M12 18.6v2.6M4.5 12H2M22 12h-2.5M6.7 6.7 4.9 4.9M19.1 19.1l-1.8-1.8M17.3 6.7l1.8-1.8M4.9 19.1l1.8-1.8' />
      </svg>
    )
  }
  if (sub === 'workstyle') {
    return (
      <svg {...common}>
        <path d='M20 11.5a8 8 0 1 0-2.3 6.1' />
        <path d='M20 4.5v7h-7' />
      </svg>
    )
  }
  // identity：一枚「内在结构」的抽象符号（中心 + 三条分支），比人像更少歧义
  return (
    <svg {...common}>
      <path d='M12 20.5c0-4.2 0-6.3 0-8' />
      <circle cx='12' cy='5.6' r='2.6' />
      <path d='M12 12.5c-2.6 0-4.4-1.6-4.9-3.7M12 12.5c2.6 0 4.4-1.6 4.9-3.7' />
      <circle cx='6.2' cy='16.8' r='2.2' />
      <circle cx='17.8' cy='16.8' r='2.2' />
      <path d='M12 12.5v2.2M8.2 15.2 10 13.6M15.8 15.2 14 13.6' />
    </svg>
  )
}

function LinkIcon(): React.ReactElement {
  return (
    <svg width='11' height='11' viewBox='0 0 24 24' fill='none' stroke='currentColor' strokeWidth='2' strokeLinecap='round' strokeLinejoin='round' aria-hidden>
      <path d='M9.5 14.5 14.5 9.5' />
      <path d='M11 6.5 12.6 5a3.8 3.8 0 0 1 5.4 5.4L16.4 12' />
      <path d='M13 17.5 11.4 19a3.8 3.8 0 0 1-5.4-5.4L7.6 12' />
    </svg>
  )
}

function PencilIcon(): React.ReactElement {
  return (
    <svg width='11' height='11' viewBox='0 0 24 24' fill='none' stroke='currentColor' strokeWidth='2' strokeLinecap='round' strokeLinejoin='round' aria-hidden>
      <path d='M4 20h4L19.5 8.5a2.1 2.1 0 0 0-3-3L5 17v3z' />
    </svg>
  )
}

function PinIcon(): React.ReactElement {
  return (
    <svg width='11' height='11' viewBox='0 0 24 24' fill='none' stroke='currentColor' strokeWidth='2' strokeLinecap='round' strokeLinejoin='round' aria-hidden>
      <path d='M12 17v5' />
      <path d='M8.5 3h7l-1 5.2 3 3.1v2.2H6.5v-2.2l3-3.1z' />
    </svg>
  )
}

function ArchiveIcon(): React.ReactElement {
  return (
    <svg width='12' height='12' viewBox='0 0 24 24' fill='none' stroke='currentColor' strokeWidth='2' strokeLinecap='round' strokeLinejoin='round' aria-hidden>
      <rect x='3.5' y='4' width='17' height='4.5' rx='1.5' />
      <path d='M5 8.5V19a1.5 1.5 0 0 0 1.5 1.5h11A1.5 1.5 0 0 0 19 19V8.5' />
      <path d='M10 12.5h4' />
    </svg>
  )
}

function SparkIcon(): React.ReactElement {
  return (
    <svg width='12' height='12' viewBox='0 0 24 24' fill='none' stroke='currentColor' strokeWidth='2' strokeLinecap='round' strokeLinejoin='round' aria-hidden>
      <path d='M12 3.5l1.9 4.9 4.9 1.9-4.9 1.9L12 17.1l-1.9-4.9L5.2 10.3l4.9-1.9z' />
      <path d='M18.5 16.5l.8 2 2 .8-2 .8-.8 2-.8-2-2-.8 2-.8z' />
    </svg>
  )
}

function CopyIcon(): React.ReactElement {
  return (
    <svg width='12' height='12' viewBox='0 0 24 24' fill='none' stroke='currentColor' strokeWidth='2' strokeLinecap='round' strokeLinejoin='round' aria-hidden>
      <rect x='9' y='9' width='11.5' height='11.5' rx='2.5' />
      <path d='M15 9V6.5A2.5 2.5 0 0 0 12.5 4H6.5A2.5 2.5 0 0 0 4 6.5v6A2.5 2.5 0 0 0 6.5 15H9' />
    </svg>
  )
}

/** 一条画像的编辑草稿。证据用一行一个，避免「逗号还是顿号」的输入歧义。 */
type Draft = { id: string; title: string; body: string; evidence: string }

function Profile(): React.ReactElement {
  const { data, loading, error, reload } = useLoad(() => m.loadProfile(), [])
  const [active, setActive] = React.useState('identity')
  const [busy, setBusy] = React.useState('')
  const [actionError, setActionError] = React.useState('')
  const [outcome, setOutcome] = React.useState<m.Outcome | null>(null)
  const [note, setNote] = React.useState('')
  const [editing, setEditing] = React.useState(false)
  const [draft, setDraft] = React.useState<Draft | null>(null)
  const [history, setHistory] = React.useState<m.MemoryEntry[] | null>(null)

  /**
   * 默认展开哪一类：优先「有结论的」第一类。
   *
   * 为什么不是固定 identity：一类都没分类时（新库、第一次点生成）停在空类上，
   * 用户看到的是一句「这一类还没有结论」，会以为生成失败了 —— 而实际上
   * 另外三类里有内容。
   */
  React.useEffect(() => {
    if (!data) return
    const filled = data.groups.filter(group => group.items.length > 0)
    if (filled.length === 0) return
    if (data.groups.some(group => group.sub === active && group.items.length > 0)) return
    setActive(filled[0].sub)
    // active 故意不进依赖：它变化时不重新挑（否则用户点了空类会被强行弹回有内容的类）
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [data])

  const generate = async (sub?: string) => {
    setBusy(sub ?? 'all')
    setActionError('')
    setOutcome(null)
    setNote('')
    try {
      const result = await m.buildProfile(sub)
      setOutcome(result.outcome)
      reload()
    } catch (caught) {
      setActionError(caught instanceof Error ? caught.message : String(caught))
    } finally {
      setBusy('')
    }
  }

  const togglePin = async (item: m.ProfileItem) => {
    setBusy(item.id)
    setActionError('')
    try {
      await m.pinProfileItem(item.id, !item.pinned)
      reload()
    } catch (caught) {
      setActionError(caught instanceof Error ? caught.message : String(caught))
    } finally {
      setBusy('')
    }
  }

  const saveDraft = async () => {
    if (!draft) return
    if (!draft.title.trim()) {
      setActionError(t('标题不能为空'))
      return
    }
    setBusy(draft.id)
    setActionError('')
    try {
      await m.updateEntry(draft.id, {
        title: draft.title.trim(),
        body: draft.body,
        evidence: draft.evidence.split('\n').map(line => line.trim()).filter(Boolean),
      })
      setDraft(null)
      reload()
    } catch (caught) {
      setActionError(caught instanceof Error ? caught.message : String(caught))
    } finally {
      setBusy('')
    }
  }

  const openHistory = async () => {
    setBusy('history')
    setActionError('')
    try {
      const list = await m.loadList({ type: 'profile', includeDeleted: true, limit: 200 })
      setHistory(list.items.filter(item => item.deletedAt !== null))
    } catch (caught) {
      setActionError(caught instanceof Error ? caught.message : String(caught))
    } finally {
      setBusy('')
    }
  }

  const restore = async (id: string) => {
    setBusy(id)
    try {
      await m.restoreEntry(id)
      setHistory(current => (current ? current.filter(item => item.id !== id) : current))
      reload()
    } catch (caught) {
      setActionError(caught instanceof Error ? caught.message : String(caught))
    } finally {
      setBusy('')
    }
  }

  if (loading) return <Panel loading error=''>{null}</Panel>
  if (error) return <Panel loading={false} error={error} onRetry={reload}>{null}</Panel>
  if (!data) return <Panel loading={false} error={t('读不到画像。')} onRetry={reload}>{null}</Panel>

  // 后端给的分组是权威；老后端没有 `hint` 时才用本地兜底。
  const groups = data.groups.map(group => {
    const fallback = SUB_FALLBACK.find(item => item.key === group.sub)
    return {
      ...group,
      label: group.label || fallback?.label || group.sub,
      hint: group.hint || fallback?.hint || '',
    }
  })
  const current = groups.find(group => group.sub === active) ?? groups[0]
  const items = current?.items ?? []
  const total = groups.reduce((sum, group) => sum + group.items.length, 0)

  // 同步保护判据：持久（本地事实源）+ 已配 + 开关打开，三者都成立才算「保护中」。
  // 少任何一项都只算「仅本地缓存」—— 说成「保护中」会让人以为换机器也在。
  const syncOn = Boolean(data.sync?.configured && data.sync?.enabled)
  const persistOn = data.storage?.persistent !== false

  const copyMarkdown = async () => {
    const lines = [`## ${current?.label ?? ''}`, '']
    if (current?.hint) lines.push(`> ${current.hint}`, '')
    items.forEach((item, index) => {
      lines.push(`${index + 1}. **${item.title}**`, '', item.body || item.summary || '')
      if (item.evidence.length) {
        lines.push('', `证据：${item.evidence.map(id => `\`${id}\``).join('、')}`)
      }
      lines.push('')
    })
    try {
      await navigator.clipboard.writeText(lines.join('\n'))
      setNote(t('已复制「{name}」的 {n} 条结论（Markdown）', { name: current?.label ?? '', n: items.length }))
    } catch {
      setActionError(t('复制失败：浏览器拒绝了剪贴板权限'))
    }
  }

  return (
    <div className='mm-prof'>
      {actionError ? <div className='mm-error'>{actionError}</div> : null}

      {/* ── 状态带 ─────────────────────────────────────────── */}
      <section className='mm-prof-hero'>
        <div className='mm-prof-hero-top'>
          <p className='mm-prof-hero-lead'>
            {t('跨项目归纳的人格与偏好特征，每条带真实证据链；手动置顶的条目重新生成时保留')}
          </p>
          <div className='mm-prof-hero-actions'>
            <Button type='button' size='sm' variant='outline' disabled={busy !== ''} onClick={() => void openHistory()}>
              <ArchiveIcon />
              {t('历史留存')}
            </Button>
            <Button
              type='button'
              size='sm'
              disabled={busy !== ''}
              onClick={() => void generate()}
            >
              <SparkIcon />
              {busy === 'all' ? t('生成中…') : t('重新生成全量画像')}
            </Button>
          </div>
        </div>

        <div className='mm-prof-hero-meta'>
          <StateBadge
            state={persistOn && syncOn ? 'ok' : 'idle'}
            label={persistOn && syncOn
              ? t('持久缓存与 WebDAV 同步保护中')
              : persistOn
                ? t('持久缓存已开，云同步未开启')
                : t('仅本地临时缓存')}
          />
          <StateBadge
            state={data.generatedAt ? 'ok' : 'idle'}
            label={data.generatedAt
              ? t('上次处理 {when}', { when: data.generatedText ?? timeText(data.generatedAt) })
              : t('还没生成过')}
          />
          <StateBadge
            state='idle'
            label={t('记忆素材 {n} 条', { n: num(data.material ?? data.total) })}
          />
          {data.pinned > 0 ? (
            <Badge variant='brand' shape='tag'>{t('{n} 条已置顶', { n: data.pinned })}</Badge>
          ) : null}
          {!data.enabled ? (
            <Badge variant='warning' shape='tag'>{t('画像已关闭')}</Badge>
          ) : null}
        </div>

        {!data.enabled ? (
          <p className='mm-hint'>
            {t('去「配置」页打开「深层画像」（deep.enabled 与 deep.personaEnabled），否则画像任务会直接跳过。')}
          </p>
        ) : null}
        {data.enabled && !data.ready ? (
          <p className='mm-hint'>
            {t('样本少于门槛（{n} / {min}）时生成的结论多半是把偶然当必然。仍然可以强制生成，但结果仅供试验。',
              { n: data.total, min: data.minMemories })}
          </p>
        ) : null}
        {data.freshSinceProfile > 0 ? (
          <p className='mm-hint'>
            {t('画像生成之后又新增了 {n} 条记忆，这些还没被归纳进来。', { n: data.freshSinceProfile })}
          </p>
        ) : null}
        {note ? <p className='mm-hint'>{note}</p> : null}
        {outcome ? <OutcomeLine outcome={outcome} /> : null}
      </section>

      {/* ── 四宫格分类 ─────────────────────────────────────── */}
      <div className='mm-prof-tabs' role='tablist' aria-label={t('画像分类')}>
        {groups.map(group => {
          const selected = group.sub === current?.sub
          return (
            <button
              key={group.sub}
              type='button'
              role='tab'
              aria-selected={selected}
              className={`mm-prof-tab${selected ? ' active' : ''}${group.items.length === 0 ? ' empty' : ''}`}
              onClick={() => {
                setActive(group.sub)
                setEditing(false)
                setDraft(null)
              }}
            >
              <span className='mm-prof-tab-icon' aria-hidden><SubIcon sub={group.sub} /></span>
              <span className='mm-prof-tab-name'>{group.label}</span>
              <span className='mm-prof-tab-count'>{group.items.length}</span>
            </button>
          )
        })}
      </div>

      {/* ── 单面板 ─────────────────────────────────────────── */}
      <section className='mm-prof-panel' aria-label={current?.label}>
        <div className='mm-prof-panel-head'>
          {/* 面板头只写「这一类在讲什么 + 共几条」：类名已经在上面的选中格里，
              再重复一次会让每一屏都出现两个同样的标题。 */}
          <span className='mm-prof-panel-hint'>
            {current?.hint}
            <span className='mm-prof-panel-count'>{t('共 {n} 条归纳', { n: items.length })}</span>
          </span>
          <div className='mm-prof-panel-actions'>
            <Button type='button' size='sm' variant='outline' disabled={items.length === 0} onClick={() => void copyMarkdown()}>
              <CopyIcon />
              {t('复制 Markdown')}
            </Button>
            <Button
              type='button'
              size='sm'
              variant='outline'
              disabled={busy !== ''}
              onClick={() => void generate(current?.sub)}
            >
              <SparkIcon />
              {busy === current?.sub ? t('生成中…') : t('单独生成')}
            </Button>
            <Button
              type='button'
              size='sm'
              disabled={items.length === 0}
              onClick={() => {
                setEditing(!editing)
                setDraft(null)
                setNote('')
              }}
            >
              <PencilIcon />
              {editing ? t('完成编辑') : t('编辑条目')}
            </Button>
          </div>
        </div>

        {items.length === 0 ? (
          <div className='mm-prof-empty'>
            <div className='mm-prof-empty-title'>{t('这一类还没有结论')}</div>
            <p className='mm-prof-empty-body'>
              {total === 0
                ? t('点右上角「重新生成全量画像」，模型会从 {n} 条记忆素材里归纳出四类结论。',
                  { n: num(data.material ?? data.total) })
                : t('让模型单独补这一类，或从别的类里挪一条过来（在「浏览」页改标签）。')}
            </p>
            {total === 0 ? null : (
              <Button type='button' size='sm' variant='outline' disabled={busy !== ''} onClick={() => void generate(current?.sub)}>
                {t('单独生成这一类')}
              </Button>
            )}
          </div>
        ) : (
          <div className='mm-prof-list'>
            {items.map((item, index) => (
              <article key={item.id} className={`mm-prof-item${item.pinned ? ' pinned' : ''}`}>
                <div className='mm-prof-item-head'>
                  <span className='mm-prof-item-meta'>
                    <span className='mm-prof-item-no'>#{index + 1}</span>
                    {item.pinned ? (
                      <span className='mm-prof-item-flag' title={t('已置顶：下次重新生成会保留它')}>
                        <PinIcon />
                        {t('置顶')}
                      </span>
                    ) : null}
                  </span>
                  <span className='mm-prof-item-title'>{item.title}</span>
                  {editing ? (
                    <span className='mm-prof-item-tools'>
                      <button
                        type='button'
                        className={`mm-prof-icon-btn${item.pinned ? ' on' : ''}`}
                        title={item.pinned ? t('取消置顶（下次重生成会替换它）') : t('置顶（下次重生成保留它）')}
                        aria-label={item.pinned ? t('取消置顶') : t('置顶')}
                        disabled={busy !== ''}
                        onClick={() => void togglePin(item)}
                      >
                        <PinIcon />
                      </button>
                      <button
                        type='button'
                        className='mm-prof-icon-btn'
                        title={t('编辑标题、正文与证据')}
                        aria-label={t('编辑这一条')}
                        onClick={() => setDraft({
                          id: item.id,
                          title: item.title,
                          body: item.body || item.summary || '',
                          evidence: item.evidence.join('\n'),
                        })}
                      >
                        <PencilIcon />
                      </button>
                    </span>
                  ) : null}
                </div>

                {draft?.id === item.id ? (
                  <div className='mm-prof-editor'>
                    <label className='mm-label'>{t('标题')}</label>
                    <Input
                      value={draft.title}
                      onChange={event => setDraft({ ...draft, title: event.target.value })}
                    />
                    <label className='mm-label'>{t('正文（一句话结论）')}</label>
                    <Textarea
                      rows={3}
                      value={draft.body}
                      onChange={event => setDraft({ ...draft, body: event.target.value })}
                    />
                    <label className='mm-label'>{t('证据（一行一个记忆 id）')}</label>
                    <Textarea
                      rows={Math.max(2, draft.evidence.split('\n').length)}
                      value={draft.evidence}
                      onChange={event => setDraft({ ...draft, evidence: event.target.value })}
                    />
                    <div className='mm-prof-editor-foot'>
                      <Button type='button' size='sm' variant='ghost' disabled={busy !== ''} onClick={() => setDraft(null)}>
                        {t('取消')}
                      </Button>
                      <Button type='button' size='sm' disabled={busy !== ''} onClick={() => void saveDraft()}>
                        {busy === draft.id ? t('保存中…') : t('保存')}
                      </Button>
                    </div>
                  </div>
                ) : (
                  <div className='mm-prof-evidence'>
                    <span className='mm-prof-evidence-label'>
                      <LinkIcon />
                      {item.evidence.length
                        ? t('证据 ({n}):', { n: item.evidence.length })
                        : t('无证据')}
                    </span>
                    {item.evidence.length === 0 ? (
                      <Badge variant='warning' shape='tag'>{t('这条结论没有证据链')}</Badge>
                    ) : item.evidence.map(id => (
                      <button
                        key={id}
                        type='button'
                        className='mm-prof-chip'
                        title={t('在「浏览」页打开这条原始记忆')}
                        onClick={() => openEntry(id)}
                      >
                        {shortId(id)}
                      </button>
                    ))}
                  </div>
                )}
              </article>
            ))}
          </div>
        )}
      </section>

      {/* ── 历史留存 ───────────────────────────────────────── */}
      {history ? (
        <div className='mm-dialog-mask' role='dialog' aria-modal='true' aria-label={t('历史留存')}>
          <div className='mm-dialog mm-dialog-narrow'>
            <div className='mm-dialog-head'>
              <strong>{t('历史留存（{n} 条）', { n: history.length })}</strong>
              <Button type='button' size='sm' variant='ghost' onClick={() => setHistory(null)}>{t('关闭')}</Button>
            </div>
            <div className='mm-dialog-body'>
              <p className='mm-hint'>
                {t('每次重新生成，旧结论进回收站而不是被删除 —— 上一版长什么样、为什么变了，都还能查。')}
              </p>
              {history.length === 0 ? (
                <p className='mm-muted'>{t('还没有被替换下来的旧结论。')}</p>
              ) : (
                <div className='mm-prof-history'>
                  {history.map(item => (
                    <div key={item.id} className='mm-prof-history-row'>
                      <div className='mm-prof-history-main'>
                        <div className='mm-prof-history-title'>{item.title}</div>
                        <div className='mm-prof-history-meta'>
                          {timeText(item.deletedAt)} · {item.typeLabel}
                        </div>
                      </div>
                      <Button type='button' size='sm' variant='ghost' disabled={busy !== ''} onClick={() => void restore(item.id)}>
                        {t('恢复')}
                      </Button>
                    </div>
                  ))}
                </div>
              )}
            </div>
          </div>
        </div>
      ) : null}
    </div>
  )
}

/** 任务结果的统一呈现：done / skipped / failed 三态文案。 */
export function OutcomeLine({ outcome }: { outcome: m.Outcome }): React.ReactElement {
  if (outcome.status === 'done') {
    return (
      <p className='mm-outcome mm-outcome-done'>
        {t('完成')}
        {outcome.result ? <code>{JSON.stringify(outcome.result).slice(0, 240)}</code> : null}
      </p>
    )
  }
  if (outcome.status === 'skipped') {
    return <p className='mm-outcome mm-outcome-skip'>{t('跳过：{reason}', { reason: outcome.reason ?? '—' })}</p>
  }
  return <p className='mm-outcome mm-outcome-fail'>{t('失败：{reason}', { reason: outcome.error ?? '—' })}</p>
}

export { Profile }