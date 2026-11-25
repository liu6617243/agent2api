import * as React from 'react'
import { mountOnPage } from '../island-lazy'
import { createRoot } from 'react-dom/client'
import { Badge, Button, SegmentedControl, Spinner, Switch } from '@ui'
import { t } from '../i18n'
import * as s from './skills-hub-api'
import { Dashboard } from './skills-hub-tab-dashboard'
import { Library } from './skills-hub-tab-library'
import { SyncTab } from './skills-hub-tab-sync'
import { Dedup } from './skills-hub-tab-dedup'
import { Settings } from './skills-hub-tab-settings'
import { Empty, notifyEnter, notifyRefresh, sizeText, toast } from './skills-hub-shared'

/**
 * Agent2API · 技能仓库（页面岛）。
 *
 * ── 这个页面在解决什么 ──────────────────────────────────────
 * 装了七八个 AI 编程客户端之后，同一个技能会在每个客户端的 `skills/` 里各存一份
 * 副本：改一处，另外七处就旧了；删一处，另外七处还在。技能仓库把这些副本收成
 * **一份真身**放在中央目录，各客户端目录里只留指向它的链接 —— 于是「改一处等于
 * 改全部」，而磁盘上只有一份内容。
 *
 * ── 为什么是五个页签而不是照搬 AgentHub 的九个 ──────────────
 * AgentHub 那套把「技能库 / 同步 / 去重 / 仪表盘 / 详情 / 配置 / WebDAV / 迁移 /
 * 帮助」拆成九个入口。其中 WebDAV 那一是**跨设备同步技能仓库**，本项目已有
 * 记忆中枢的 cloud sync 体系（`core::cloud_sync` + `memory.sync`），技能仓库
 * 走另建一条 WebDAV 通道会在同一份配置里出现两套远端语义 —— 一期不做，需要时
 * 按 cloud sync 的口径接进来，而不是照抄一个并行实现。
 *
 * 剩下的五个是**这条链路的必经站点**，且顺序就是用户的心路：
 *
 *   dashboard 仪表盘 —— 现在什么状态、有什么要处理
 *   library   技能库 —— 每个技能受管到什么程度
 *   sync      同步    —— 这次会做什么（预览 → 确认 → 结果）
 *   dedup     去重    —— 同名异容、链接异常这些只有人能决定的事
 *   settings  配置    —— 扫哪些渠道、怎么挂、删了能救多久
 *
 * 详情不占一个页签：它是「点开某一个技能」的抽屉，抽屉比页签更符合它的用法
 * （看完就关，不会有人停在详情页上）。
 *
 * ── 为什么详情页的挂载开关是「一个技能 × 一个渠道」──────────
 * 挂载的粒度就是这一对：`taste` 在 Codex 里开着、在 Trae 里关着是常见需求
 * （某个客户端里有同名旧版本，或那个客户端根本不用这个技能）。做成整技能的
 * 一个总开关，用户就只能在「全开」和「全关」之间选。
 *
 * ── 顶栏徽标为什么只留一个 ──────────────────────────────────
 * 早先想同时显示「N 个技能」「N 项待处理」「N 处失效挂载」。三个徽标并排的
 * 结果是用户读不出哪个是当下该管的 —— 而待处理数是唯一**会随用户动作变化**
 * 的数字（技能数只在同步时变）。所以顶栏只留在处理数，其余数字都在仪表盘上。
 */

/**
 * 五个页签。
 *
 * ── 为什么 label 在这里就调 t()，而不是渲染时 `t(item.label)` ──────
 * i18n 扫描器只认**字面量** `t('…')`：渲染时套一层变量查表，扫描器提取不到键，
 * 五份词典永远缺这一页的页签名，英文界面会直接露出中文。所以这张表在模块加载
 * 期就把译文取好（语言切换是整页刷新，见 `ui/i18n.js`，不存在译文过期）。
 */
const TABS = [
  { key: 'dashboard', label: t('仪表盘') },
  { key: 'library', label: t('技能库') },
  { key: 'sync', label: t('同步') },
  { key: 'dedup', label: t('去重') },
  { key: 'settings', label: t('仓库配置') },
] as const

type Tab = typeof TABS[number]['key']

/**
 * 壳侧入口（`window.wbSkillsHub`）与组件之间的通道。
 *
 * ── 为什么不是直接改模块变量 ──────────────────────────────
 * 模块变量只在**首次渲染**被读一次（`useState(activeTab)`）。用户已经在这一页
 * 上时，`wbSkillsHub.open('dedup')` 只改变量、不触发重渲染 —— 点了没反应。所以
 * 入口统一派发一个事件，组件订阅它再 setState；模块变量只负责「页面还没挂载时
 * 记住该开哪个页签」。
 */
const HUB_EVENT = 'wb:skills-hub'

type HubIntent = { tab?: Tab; skill?: string }

function emit(intent: HubIntent): void {
  window.dispatchEvent(new CustomEvent<HubIntent>(HUB_EVENT, { detail: intent }))
}

let activeTab: Tab = 'dashboard'
let pendingSkill = ''

function SkillsHubPage(): React.ReactElement {
  const [tab, setTab] = React.useState<Tab>(activeTab)
  const [detail, setDetail] = React.useState<string>('')
  const [badge, setBadge] = React.useState<{ skills: number; todo: number } | null>(null)
  const [booted, setBooted] = React.useState(false)
  const [error, setError] = React.useState('')
  const [reloadAt, setReloadAt] = React.useState(0)

  /**
   * 首屏只拉一次 overview：它同时给出技能数、待处理数与渠道矩阵，是这一页
   * 唯一「一眼看全」的数据源。其余页签各自的 `useLoad` 会在被渲染时才请求 ——
   * 五个页签全在首屏拉一遍，等于用户打开这一页就要扫五次磁盘。
   */
  const loadShell = React.useCallback(async () => {
    try {
      const data = await s.loadOverview()
      setBadge({
        skills: data.skillCount,
        todo: data.pendingConflicts.length + data.orphans.length + data.hubExtra.length + data.brokenMounts,
      })
      setError('')
    } catch (caught) {
      setError(caught instanceof Error ? caught.message : String(caught))
    } finally {
      setBooted(true)
    }
  }, [])

  React.useEffect(() => { void loadShell() }, [loadShell, reloadAt])

  // 「从别处跳进来并要求打开某个技能」：壳侧 `wbSkillsHub.open(name)` 会设置
  // `pendingSkill` 再切页；这里消费掉它 —— 否则用户点第二次同一个技能时
  // `pendingSkill` 没变，抽屉不会重开。
  React.useEffect(() => {
    if (booted && pendingSkill) {
      setDetail(pendingSkill)
      pendingSkill = ''
    }
  }, [booted, tab])

  /**
   * 壳侧入口的运行时通道。
   *
   * 上一步的 effect 只在挂载/切页时跑一次；已经停在技能仓库页时（比如用户在
   * 另一个窗口点了某个技能），必须靠事件把 intent 推进来。挂载时也补一次消费：
   * `open()` 可能发生在页面还没渲染之前，那时是模块变量接的。
   */
  React.useEffect(() => {
    const onIntent = (event: Event): void => {
      const detail = (event as CustomEvent<HubIntent>).detail
      if (!detail) return
      if (detail.skill) setDetail(detail.skill)
      if (detail.tab) {
        activeTab = detail.tab
        setTab(detail.tab)
      }
    }
    window.addEventListener(HUB_EVENT, onIntent)
    return () => window.removeEventListener(HUB_EVENT, onIntent)
  }, [])

  const refreshAll = () => {
    notifyRefresh()
    setReloadAt(value => value + 1)
  }

  const goto = (next: Tab) => {
    activeTab = next
    setTab(next)
  }

  const openSkill = (name: string) => {
    setDetail(name)
  }

  return (
    <div className='sk-page'>
      <div className='sk-head'>
        <div className='sk-tabs'>
          <SegmentedControl
            options={TABS.map(item => ({ value: item.key, label: item.label }))}
            value={tab}
            onValueChange={value => goto(value as Tab)}
            aria-label={t('技能仓库功能分类')}
          />
        </div>
        <div className='sk-actions'>
          {badge ? (
            <Badge variant='outline' shape='tag'>{t('{n} 个技能', { n: badge.skills })}</Badge>
          ) : null}
          {badge && badge.todo > 0 ? (
            <button
              type='button'
              className='sk-badge-btn'
              title={t('去「去重」逐条裁决')}
              onClick={() => goto('dedup')}
            >
              <Badge variant='warning' shape='tag'>{t('{n} 项待处理', { n: badge.todo })}</Badge>
            </button>
          ) : null}
          <Button type='button' size='sm' variant='ghost' onClick={refreshAll}>
            {t('刷新')}
          </Button>
        </div>
      </div>

      {error ? <div className='sk-error'>{error}</div> : null}

      {!booted ? (
        <div className='sk-boot'>
          <Spinner />
          <span>{t('正在读取技能仓库…')}</span>
        </div>
      ) : (
        <div className='sk-body'>
          {tab === 'dashboard' ? (
            <Dashboard onGo={goto} onOpenSkill={openSkill} onChanged={() => setReloadAt(value => value + 1)} />
          ) : null}
          {tab === 'library' ? (
            <Library onOpenSkill={openSkill} onOpenSettings={() => goto('settings')} />
          ) : null}
          {tab === 'sync' ? (
            <SyncTab onChanged={() => setReloadAt(value => value + 1)} />
          ) : null}
          {tab === 'dedup' ? (
            <Dedup onChanged={() => setReloadAt(value => value + 1)} onOpenSkill={openSkill} />
          ) : null}
          {tab === 'settings' ? (
            <Settings onChanged={() => setReloadAt(value => value + 1)} />
          ) : null}
        </div>
      )}

      {detail ? <SkillDrawer name={detail} onClose={() => setDetail('')} onChanged={() => setReloadAt(value => value + 1)} /> : null}
    </div>
  )
}

/* ─── 技能详情抽屉 ─────────────────────────────────────────── */

function SkillDrawer({
  name,
  onClose,
  onChanged,
}: {
  name: string
  onClose: () => void
  onChanged: () => void
}): React.ReactElement {
  const [data, setData] = React.useState<s.SkillDetail | null>(null)
  const [error, setError] = React.useState('')
  const [busy, setBusy] = React.useState('')

  const load = React.useCallback(async () => {
    setError('')
    try {
      setData(await s.loadSkill(name))
    } catch (caught) {
      setError(caught instanceof Error ? caught.message : String(caught))
    }
  }, [name])

  React.useEffect(() => { void load() }, [load])

  // Esc 关闭：抽屉盖住了列表，键盘用户不该被逼着去点右上角的叉。
  React.useEffect(() => {
    const onKey = (event: KeyboardEvent) => { if (event.key === 'Escape') onClose() }
    window.addEventListener('keydown', onKey)
    return () => window.removeEventListener('keydown', onKey)
  }, [onClose])

  const act = async (kind: 'adopt' | 'remove', skillName: string) => {
    setBusy(kind)
    try {
      if (kind === 'adopt') {
        await s.adoptSkill(skillName)
        toast(t('已纳入台账（没有移动任何文件）'), 'ok')
      } else {
        const result = await s.removeSkill(skillName)
        toast(t('已删除，真身在回收站 {file}', { file: result.trash || '—' }), 'ok')
        onClose()
      }
      await load()
      onChanged()
    } catch (caught) {
      toast(t('操作失败：{error}', { error: String(caught instanceof Error ? caught.message : caught) }), 'err')
    } finally {
      setBusy('')
    }
  }

  const toggle = async (tool: string, enabled: boolean) => {
    setBusy(`mount-${tool}`)
    try {
      await s.toggleMount(name, tool, enabled)
      await load()
      onChanged()
    } catch (caught) {
      toast(t('切换失败：{error}', { error: String(caught instanceof Error ? caught.message : caught) }), 'err')
    } finally {
      setBusy('')
    }
  }

  return (
    <>
      <div className='sk-drawer-veil' onClick={onClose} />
      <aside className='sk-drawer' role='dialog' aria-label={name}>
        <div className='sk-drawer-head'>
          <div className='sk-drawer-title'>
            <span className='sk-drawer-name'>{data?.skillMd ? name : name}</span>
            {data ? (
              data.inManifest ? (
                <Badge variant='success' shape='tag'>{t('已收纳')}</Badge>
              ) : (
                <Badge variant='outline' shape='tag'>{t('待收纳')}</Badge>
              )
            ) : null}
          </div>
          <Button type='button' size='sm' variant='ghost' onClick={onClose}>{t('关闭')}</Button>
        </div>

        {error ? <div className='sk-inline-error'>{error}</div> : null}
        {!data && !error ? (
          <div className='sk-panel-loading'><Spinner /><span>{t('正在读取…')}</span></div>
        ) : null}

        {data ? (
          <div className='sk-drawer-body'>
            <div className='sk-drawer-desc'>{data.description || t('（没有 description，Agent 触发不出这个技能）')}</div>

            {(data.health ?? []).length > 0 ? (
              <ul className='sk-health-list'>
                {(data.health ?? []).map((item, at) => (
                  <li key={at} className={`sk-health-item sk-health-${item.level === 'bad' ? 'bad' : 'warn'}`}>
                    {item.text}
                  </li>
                ))}
              </ul>
            ) : null}

            <div className='sk-kv'>
              <span>{t('目录')}</span><code>{data.dir}</code>
              <span>{t('版本')}</span><code>{data.version || '—'}</code>
              <span>{t('指纹')}</span><code title={data.treeHash}>{(data.treeHash || '—').slice(0, 16)}</code>
              {data.fileCount !== undefined ? (
                <><span>{t('文件数')}</span><code>{data.fileCount}</code></>
              ) : null}
              {data.manifest ? (
                <><span>{t('最后同步')}</span><code>{lastHistory(data.manifest)}</code></>
              ) : null}
            </div>

            {/* ── 挂载矩阵：一个技能 × N 个渠道 ── */}
            <div className='sk-sub-title'>{t('挂载')}</div>
            {(data.mounts ?? []).length === 0 ? (
              <Empty>{t('还没有挂到任何客户端目录')}</Empty>
            ) : (
              <div className='sk-table'>
                <div className='sk-table-head'>
                  <span>{t('渠道')}</span>
                  <span>{t('位置')}</span>
                  <span>{t('状态')}</span>
                  <span>{t('挂载开关')}</span>
                </div>
                {(data.mounts ?? []).map(row => (
                  <div key={row.path} className='sk-table-row'>
                    <span className='sk-cell-tool'>{row.tool}</span>
                    <code className='sk-cell-note' title={row.path}>{row.path}</code>
                    <span>
                      {row.valid ? (
                        <Badge variant='success' shape='tag'>{t('有效')}</Badge>
                      ) : (
                        <Badge variant='destructive' shape='tag'>{t('失效')}</Badge>
                      )}
                    </span>
                    <span>
                      <Switch
                        checked={row.enabled}
                        disabled={busy === `mount-${row.tool}`}
                        onCheckedChange={next => void toggle(row.tool, next)}
                      />
                    </span>
                  </div>
                ))}
              </div>
            )}

            <div className='sk-sub-title'>{t('SKILL.md')}</div>
            <pre className='sk-md'>{data.skillMd || t('（读不到 SKILL.md）')}</pre>

            {(data.children ?? []).length > 0 ? (
              <>
                <div className='sk-sub-title'>{t('目录内容')}</div>
                <div className='sk-children'>
                  {(data.children ?? []).map(child => (
                    <span key={child.name} className='sk-child'>
                      {child.dir ? '▸' : '·'} {child.name}
                      {child.dir ? null : <span className='sk-child-size'>{sizeText(child.size)}</span>}
                    </span>
                  ))}
                </div>
              </>
            ) : null}

            <div className='sk-drawer-actions'>
              {!data.inManifest && data.centralExists ? (
                <Button
                  type='button'
                  size='sm'
                  variant='outline'
                  disabled={busy === 'adopt'}
                  onClick={() => void act('adopt', name)}
                >
                  {busy === 'adopt' ? t('纳管中…') : t('纳入台账')}
                </Button>
              ) : null}
              <Button
                type='button'
                size='sm'
                variant='destructive'
                disabled={busy === 'remove'}
                onClick={() => {
                  // 删除是不可逆的（虽然进回收站），必须让用户二次确认。
                  // 用原生 confirm 而不是自绘对话框：这个动作在抽屉里，多一层
                  // 组件状态不值得，而原生对话框无法被误关。
                  if (!window.confirm(t('删除「{name}」？真身会进回收站，所有挂载一起摘掉。', { name }))) return
                  void act('remove', name)
                }}
              >
                {busy === 'remove' ? t('删除中…') : t('移到回收站')}
              </Button>
            </div>
          </div>
        ) : null}
      </aside>
    </>
  )
}

/** 台账里最后一条变更历史的时间与动作（`mergeHistory` 是倒序追加的）。 */
function lastHistory(manifest: Record<string, unknown>): string {
  const list = (manifest as { mergeHistory?: { at?: string; action?: string; detail?: string }[] }).mergeHistory
  if (!Array.isArray(list) || list.length === 0) return '—'
  const last = list[list.length - 1]
  const at = last?.at ?? ''
  const detail = last?.detail ?? last?.action ?? ''
  const stamp = at ? new Date(at).toLocaleString() : ''
  return stamp ? `${stamp} · ${detail}` : detail || '—'
}

/* ─── 挂载 ─────────────────────────────────────────────────── */

const PAGE_SELECTOR = '.page[data-page="skills-hub"]'

let mounted = false

function mount(): void {
  if (mounted) return
  const section = document.querySelector<HTMLElement>(PAGE_SELECTOR)
  if (!section) return
  mounted = true
  section.replaceChildren()
  createRoot(section).render(<SkillsHubPage />)
}

// 页面重新成为当前页时，让各页签的 useLoad 静默重拉（磁盘是别的东西在改的，
// 用户切走再回来看到旧快照会以为同步没生效）。
window.addEventListener('wb:page', event => {
  const detail = (event as CustomEvent<{ page?: string }>).detail
  if (detail?.page === 'skills-hub') notifyEnter()
})

mountOnPage('skills-hub', PAGE_SELECTOR, mount)

declare global {
  interface Window {
    /** 技能仓库（壳侧接口，与 `wbMemory` / `wbCheckin` 同一套约定）。 */
    wbSkillsHub?: {
      open(tab?: string): void
      openSkill(name: string): void
    }
  }
}

window.wbSkillsHub = {
  open: (tab?: string) => {
    if (!tab) return
    const hit = TABS.find(item => item.key === tab)
    if (!hit) return
    // 先改变量：页面还没挂载时（首次进入）由它决定开哪个页签。
    activeTab = hit.key
    // 再派事件：已经在这一页上时，变量改不动已渲染的 state。
    emit({ tab: hit.key })
  },
  openSkill: (name: string) => {
    const trimmed = name.trim()
    if (!trimmed) return
    pendingSkill = trimmed
    activeTab = 'library'
    emit({ tab: 'library', skill: trimmed })
  },
}

