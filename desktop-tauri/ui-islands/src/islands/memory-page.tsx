import * as React from 'react'
import { mountOnPage } from '../island-lazy'
import { createRoot } from 'react-dom/client'
import { Badge, Button, SegmentedControl, Spinner } from '@ui'
import { t } from '../i18n'
import * as m from './memory-api'
import { Dashboard } from './memory-tab-dashboard'
import { Browse } from './memory-tab-browse'
import { Projects } from './memory-tab-projects'
import { Profile } from './memory-tab-profile'
import { Agents } from './memory-tab-agents'
import { Auto } from './memory-tab-auto'
import { Import } from './memory-tab-import'
import { Sync } from './memory-tab-sync'
import { Settings } from './memory-tab-settings'
import { TabBoundary, notifyMemoryEnter, notifyMemoryRefresh } from './memory-shared'

/**
 * Agent2API · 记忆中枢（页面岛）。
 *
 * ── 为什么在网关里，而不是插件里 ────────────────────────────
 * 与签到中心同一决定、同一理由（见 `checkin-page.tsx` 的模块头）：插件只能出
 * 静态 HTML、跑在 iframe 里，读不到宿主的 CSS 变量、拿不到组件库实例；
 * 而「记忆」的后端（存储 + 检索 + MCP 工具）本来就只能由网关提供 —— 插件连
 * 端点都加不了。所以页面归网关，插件不参与本功能。
 *
 * ── 为什么是九个页签 ────────────────────────────────────────
 * 对齐 AgentHub 的记忆中枢之后，这一页承载的不再只是「看记忆」：
 *
 *   dashboard 仪表盘  —— 现在什么状态、有什么要处理
 *   browse    浏览    —— 找一条、改一条、裁决待确认
 *   projects  项目    —— 按仓库归档、改名、合并
 *   profile   画像    —— 关于「我」的长期事实，每条带证据
 *   agents    接入    —— 11 个客户端装没装、真的调过没
 *   auto      自动化  —— 11 个后台任务的节奏与消耗
 *   import    导入    —— 把别处的历史会话搬进来
 *   sync      同步    —— WebDAV 多设备，冲突裁决
 *   settings  配置    —— 一百多个旋钮，schema 驱动
 *
 * ── 原来的第十个「索引」页去哪了 ────────────────────────────
 * 那一页原本放四样排障工具：重建索引、体检清单、检索调试、清理回收站。
 * 前两样在仪表盘「需要处理」里本来就有对等入口（可能过时 / 索引一致性），
 * 检索与回收站浏览页都有，剩下「重建索引」一个动作撑不起一整页 ——
 * 于是把它挪到仪表盘「索引一致性」那一行的按钮上，页签删掉。
 *
 * 九项平铺在一条页签栏里而不是收进二级菜单：记忆的日常动作横跨这九项（看一条 →
 * 发现过时 → 去改配置 → 回来验证），藏进二级菜单会让人忘了它的存在。
 *
 * ── 为什么没有「核心记忆」这个独立页签了 ────────────────────
 * 原来有一页专门显示「注入给模型的原文」。改成 MCP 之后不存在「网关注入」这件事：
 * 是 Agent 自己调 `memory_core`。所以那页的语义变成「Agent 会看到的默认内容」，
 * 收进浏览页的筛选（层级 = l2）与仪表盘的指标里，不再单列 —— 一个不存在的
 * 注入动作不值得一个页签。
 *
 * ── 页签顺序按什么排 ────────────────────────────────────────
 * 按「日常使用频率」降序：看状态 → 找记忆 → 管项目 → 其余是配置性动作。
 * 配置排最后是惯例（也是最不容易误入的位置）。
 */

const TABS = [
  { key: 'dashboard', label: '仪表盘' },
  { key: 'browse', label: '浏览' },
  { key: 'projects', label: '项目' },
  { key: 'profile', label: '画像' },
  { key: 'agents', label: '接入' },
  { key: 'auto', label: '自动化' },
  { key: 'import', label: '导入' },
  { key: 'sync', label: '同步' },
  { key: 'settings', label: '配置' },
] as const

type Tab = typeof TABS[number]['key']

/**
 * 「配置」页签永远可见，不受 `ui.tabs` 约束。
 *
 * ── 为什么这条保护是必须的 ──────────────────────────────────
 * `ui.tabs` 是用户可以自己关掉页签的旋钮，而改这个旋钮的唯一入口就在「配置」
 * 页签里。允许把「配置」也关掉，就等于给用户一个**一次性按钮**：点一下，
 * 出口消失，只能去改 config.json 手工救回来。任何「关闭自己的开关」都要
 * 有这条硬保护。
 */
const PINNED_TABS: Tab[] = ['settings']

/** 当前页签（组件外也能读：壳切页时判断要不要刷新） */
let activeTab: Tab = 'dashboard'

/**
 * 把 `config.values` 里的界面旋钮解析成可用形状。
 *
 * ── 三条兜底都必须存在 ──────────────────────────────────────
 * 1. `ui.tabs` 里没有一个合法值（用户手改 config.json 写错了）→ 全显示；
 * 2. `ui.defaultTab` 指向一个已被 `ui.tabs` 关掉的页签 → 回落到第一个可见页；
 * 3. `ui.pageSize` 不是正整数 → 50。
 *
 * 任何一条不兜底都会出现「打开页面空白」或「首屏停在一个不存在的页签」，
 * 而这两种都不是用户能自己想明白的状态。
 */
function resolveUi(values: Record<string, unknown>): { tabs: Tab[]; defaultTab: Tab; pageSize: number } {
  // 键名写字面量而不是拼 `ui.${name}`：后端单测 `wired_knobs_have_a_reader`
  // 靠扫 `"ui.tabs"` 这样的完整串确认「这个旋钮真的有人读」。拼出来的键
  // 在源码里找不到，那条旋钮就会被判成死键 —— 扫描口径反过来约束了写法，
  // 这是有意的：宁可多写两遍字面量，也不要让「有没有接上」变成靠人记。
  const rawTabs = values['ui.tabs']
  const picked = Array.isArray(rawTabs)
    ? rawTabs.filter((value): value is Tab => TABS.some(item => item.key === value))
    : []
  // `ui.tabs` 少了什么都不能少「配置」（见 `PINNED_TABS`）
  const visible = TABS.map(item => item.key).filter(
    key => picked.includes(key) || PINNED_TABS.includes(key),
  )
  const tabs = picked.length ? visible : TABS.map(item => item.key)
  const wanted = values['ui.defaultTab']
  const defaultTab: Tab =
    typeof wanted === 'string' && tabs.includes(wanted as Tab) ? (wanted as Tab) : tabs[0]
  const rawSize = values['ui.pageSize']
  const pageSize = typeof rawSize === 'number' && rawSize >= 10 ? Math.floor(rawSize) : 50
  return { tabs, defaultTab, pageSize }
}

function MemoryPage(): React.ReactElement {
  const [tab, setTab] = React.useState<Tab>(activeTab)
  const [status, setStatus] = React.useState<m.MemoryStatus | null>(null)
  const [kinds, setKinds] = React.useState<m.MemoryKindOption[]>([])
  /**
   * 界面相关旋钮的生效值（`ui.tabs` / `ui.defaultTab` / `ui.pageSize`）。
   *
   * ── 为什么放在壳这一层 ────────────────────────────────────
   * 这三条同时被「顶栏」与「浏览页」读，放在任一子页里都会让另一个拿不到；
   * 而 `loadShell()` 本来就在首屏拉过一次 `loadConfig()`，顺手取出来零额外请求。
   *
   * `null` = 还没读到。此时按「全显示 / 默认仪表盘 / 50 条」的最保守组合渲染，
   * 不让首屏因为一个配置读取失败就空白。
   */
  const [ui, setUi] = React.useState<{ tabs: Tab[]; defaultTab: Tab; pageSize: number } | null>(null)
  /**
   * `ui` 的镜像 ref。
   *
   * `mm:tab` 的监听器刻意只挂一次（依赖为空，否则每帧重挂会漏事件），
   * 而它的回调里要读「哪些页签可见」。直接读 `ui` 会永远拿到首次渲染的
   * `null` —— 于是「关掉的页签不可外部跳转」这条保护形同虚设。用 ref 让
   * 回调拿到最新值，同时保持监听器只挂一次。
   */
  const uiRef = React.useRef<{ tabs: Tab[]; defaultTab: Tab; pageSize: number } | null>(null)
  const [projects, setProjects] = React.useState<m.MemoryProject[]>([])
  const [general, setGeneral] = React.useState<m.MemoryGeneral | null>(null)
  const [projectsLoading, setProjectsLoading] = React.useState(true)
  const [projectsError, setProjectsError] = React.useState('')
  const [error, setError] = React.useState('')
  const [booted, setBooted] = React.useState(false)
  /**
   * `booted` 的镜像 ref。
   *
   * `wb:page` 的监听器只挂一次（回调里读 state 会永远读到首次渲染的 `false`），
   * 而它要用 `booted` 区分两种 `wb:page`：首屏那一次（`loadShell()` 正在拉，
   * 再来一次就是重复请求）和「离开又回来」那一次（必须刷新）。用 ref 拿最新值。
   */
  const bootedRef = React.useRef(false)
  bootedRef.current = booted
  const [focusId, setFocusId] = React.useState<string | null>(null)
  const [focusProject, setFocusProject] = React.useState<string | null>(null)
  /**
   * 「跳浏览页并直接打开这一段」。目前只有「待确认」需要：仪表盘与顶栏的待确认
   * 徽标点进去必须落在待裁决清单上，而不是落在普通列表上再让人自己找。
   */
  const [focusView, setFocusView] = React.useState<'review' | null>(null)

  /** 项目列表是否已经落过一次盘（决定 `projectsLoading` 还该不该为真）。 */
  const projectsSettled = React.useRef(false)

  /**
   * 项目列表：**不参与首屏**，由 `loadShell()` 在后台触发。
   *
   * 定义在 `loadShell` 之前是为了让依赖关系顺着读：首屏只用它，不依赖它完成。
   */
  const refreshProjects = React.useCallback(async () => {
    // `setProjectsLoading(true)` 只在「还没拿到过列表」时为真：重新进入页面会
    // 静默重拉一次，那时项目卡上已经有内容，再亮骨架就是一整块闪。
    setProjectsLoading(!projectsSettled.current)
    try {
      const projectList = await m.loadProjects()
      setProjects(projectList.items)
      setGeneral(projectList.general ?? null)
      setProjectsError('')
      projectsSettled.current = true
    } catch (caught) {
      setProjectsError(caught instanceof Error ? caught.message : String(caught))
    } finally {
      setProjectsLoading(false)
    }
  }, [])

  /**
   * 首屏：先让页面能看，再补重数据。
   *
   * ── 为什么不能再 `Promise.all([loadConfig, loadProjects])` ──
   * `loadProjects()` 要把整库按项目分组、每组算 l1/l2、挑前三条最新 —— 记忆多
   * 的时候这一步比 `loadConfig()` 慢一个数量级。早先两个请求用 `Promise.all`
   * 一起等，`booted` 卡在慢的那个上，于是用户看到的是「正在读取记忆库…」转
   * 好几秒，而其实配置早就回来了、顶栏本可以立刻画出来。
   *
   * AgentHub 那边「很快就出现数据」，是因为它首屏只查索引里的几个计数。
   * 这里的口径对齐它：**首屏只等配置**（决定有哪些页签、默认落在哪一页、
   * 每页几条），项目列表改为后台加载 —— 只有「项目」页签真的用得上它，
   * 而切到那一页时 `switchTab` 本来就会再刷一次。
   *
   * ── 失败也要 booted ─────────────────────────────────────────
   * `loadConfig()` 挂了（后端刚重启、鉴权过期）时仍然要进页面：卡在 spinner 上
   * 用户既看不到错误也点不到「刷新」，那是比空白更糟的状态。错误写进 `error`
   * 条，页面用最保守的默认值渲染（全页签 / 仪表盘 / 50 条）。
   */
  const loadShell = React.useCallback(async () => {
    try {
      const config = await m.loadConfig()
      setStatus(config.status)
      setKinds(config.kinds)
      // 界面旋钮：`ui.tabs` 决定顶栏显示哪些页签，`ui.defaultTab` 决定首屏落在
      // 哪一页，`ui.pageSize` 决定浏览页每页条数。三条都在这里一次读出来。
      const resolved = resolveUi(config.values)
      uiRef.current = resolved
      setUi(resolved)
    } catch (caught) {
      setError(caught instanceof Error ? caught.message : String(caught))
    } finally {
      setBooted(true)
    }
    // 项目列表不参与首屏判定：`void` 是有意的，让它在后台跑完自己更新状态。
    void refreshProjects()
  }, [refreshProjects])

  React.useEffect(() => {
    void loadShell()
  }, [loadShell])

  /**
   * 配置读回来后把首屏落到 `ui.defaultTab`（只做一次）。
   *
   * ── 为什么要一个 `applied` 标记 ─────────────────────────────
   * `loadShell()` 会被「刷新」按钮再次调用。没有这个标记，用户手动切到「浏览」
   * 之后点一次刷新，页面就会被弹回默认页签 —— 那是一次纯粹的骚扰。
   * 所以只在**第一次**读到配置时对齐一次，之后完全交给用户。
   */
  const defaultTabApplied = React.useRef(false)
  React.useEffect(() => {
    if (!ui || defaultTabApplied.current) return
    defaultTabApplied.current = true
    if (ui.defaultTab !== activeTab) {
      activeTab = ui.defaultTab
      setTab(ui.defaultTab)
    }
  }, [ui])

  const refreshStatus = React.useCallback(async () => {
    try {
      setStatus(await m.loadStatus())
    } catch (caught) {
      setError(caught instanceof Error ? caught.message : String(caught))
    }
  }, [])

  /** 任何写操作之后：状态与项目都可能变，一起刷。 */
  const afterChange = React.useCallback(() => {
    void refreshStatus()
    void refreshProjects()
  }, [refreshStatus, refreshProjects])

  /**
   * 「这一页重新成为当前页」时要刷的全部数据。
   *
   * 三类分开表述，因为归属不同：
   * - `status`   → 壳这一层的 state（顶栏徽标 + 全部指标卡）；
   * - `projects` → 壳这一层的 state（项目页、浏览页的项目下拉）；
   * - `notifyMemoryEnter()` → 当前页签内部自己 `useLoad` 的那些数据，壳够不着。
   *
   * 第三类只发一次信号，九个页签各自决定要不要跟（详见 `memory-shared.tsx`
   * 里 `notifyMemoryEnter` / `useLoad` 的说明）。
   */
  const refreshOnEnter = React.useCallback(() => {
    void refreshStatus()
    void refreshProjects()
    notifyMemoryEnter()
  }, [refreshStatus, refreshProjects])

  /**
   * 监听页签切换请求（`mm:tab`）。
   *
   * 为什么用自定义事件而不是 props 回调：发起方是「同步页里的『修改凭据』」
   * 这类深层子组件，一层层把 `switchTab` 传下去会让每个中间层都多一个自己
   * 不用的 prop。事件是解耦的 —— 谁想切页签就发一个，开关在页面这一层。
   */
  React.useEffect(() => {
    const onTab = (event: Event) => {
      const detail = (event as CustomEvent<{ tab?: string; focusId?: string }>).detail
      const next = detail?.tab
      if (!next || !TABS.some(item => item.key === next)) return
      // `ui.tabs` 关掉的页签不接受外部跳转：否则「导入页里点某条 → 跳浏览」
      // 会切到一个顶栏上根本不存在的高亮项，用户看到的是一张没有选中态的页面。
      if (uiRef.current && !uiRef.current.tabs.includes(next as Tab)) return
      // `focusId`：「在浏览页打开这一条」。画像页的证据 chip 走的就是这条路 ——
      // 它比 `switchTab` 多带一个目标，否则切过去只会看到列表第一屏，
      // 用户还得自己再搜一遍刚才点的那条 id。
      if (detail?.focusId) setFocusId(detail.focusId)
      switchTab(next as Tab)
    }
    window.addEventListener('mm:tab', onTab)
    return () => window.removeEventListener('mm:tab', onTab)
    // switchTab 每个渲染周期都是新函数，但只依赖 setTab 与 refreshProjects（都稳定），
    // 挂一次就够；列进依赖会让监听器每帧重挂，反而漏事件。
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [])

  /**
   * 重新进入这一页就刷新 —— 不必再手动点「刷新」。
   *
   * ── 两条触发路径，各管一种「回来」──────────────────────────
   * 1. `wb:page`：外壳切页面（用户点左侧导航）。detail 就是页面名，本页是 `memory`。
   * 2. `visibilitychange`：页面没换、窗口被切走又切回来（最小化、切到别的应用、
   *    从别的窗口点回主窗口）。这种情况下 `wb:page` 不派发，只看第一路会漏。
   *
   * ── 为什么首屏那一次要跳过 ──────────────────────────────────
   * `index.tsx` 在模块求值末尾会为「上次停留的页面」先派发一次 `wb:page`。如果
   * 上次正好停在记忆页，这一发会在 `loadShell()` 刚发出请求时到达 —— 不拦就是
   * 首屏同一批接口各请求两次。`bootedRef` 为 `false` 时说明首屏正在加载，跳过；
   * 首屏的数据由 `loadShell()` 负责，语义上也不该由「切页」来拉。
   *
   * ── 为什么不做定时轮询 ──────────────────────────────────────
   * 记忆库的变化都是「用户刚才做了什么」引起的（批准、导入、跑任务），不是
   * 自己按秒变的。定时拉只会让静止的页面每 N 秒闪一次 loading。签到页要轮询是
   * 因为它真有一个后台任务在跑；这里没有。
   */
  React.useEffect(() => {
    const onPage = (event: Event) => {
      if ((event as CustomEvent<unknown>).detail !== 'memory') return
      if (!bootedRef.current) return
      refreshOnEnter()
    }
    const onVisible = () => {
      if (document.hidden) return
      const current = (window as unknown as { wbApp?: { currentPage?: string } }).wbApp?.currentPage
      // `currentPage` 缺失（老外壳没有这个字段）时按「就在这一页」处理：
      // 宁可多刷一次，也不要因为读不到字段就退化成「永不刷新」。
      if (current && current !== 'memory') return
      if (!bootedRef.current) return
      refreshOnEnter()
    }
    window.addEventListener('wb:page', onPage)
    document.addEventListener('visibilitychange', onVisible)
    return () => {
      window.removeEventListener('wb:page', onPage)
      document.removeEventListener('visibilitychange', onVisible)
    }
  }, [refreshOnEnter])

  const switchTab = (next: Tab) => {
    activeTab = next
    setTab(next)
    // 切到需要新鲜项目数据的页签时补一次（项目可能在别处被合并/归档）
    if (next === 'projects' || next === 'browse') void refreshProjects()
    // 切回仪表盘：`Dashboard` 是条件渲染，重建时会自己重拉体检/同步/自动化，
    // 但指标卡与顶栏读的是壳这一层的 `status` —— 它不会因为换页签而更新，
    // 于是「条数 / 待确认 / 今日新增」会停在离开时的数字。
    if (next === 'dashboard') void refreshStatus()
  }

  const openEntryFromProject = (id: string) => {
    setFocusId(id)
    switchTab('browse')
  }

  /**
   * 「查看记忆」→ 跳浏览页并带上项目筛选。
   *
   * 为什么带筛选而不是只跳过去：浏览页默认是「全部项目」的列表，用户点了
   * 「查看记忆」却看到一堆别的项目的条目，等于还得自己再筛一遍 —— 这一跳
   * 就白跳了。筛选条件由浏览页消费后清掉（`onFocusProjectConsumed`），
   * 否则用户手动改掉筛选后又被外力改回去。
   */
  const openProjectMemories = (slug: string) => {
    setFocusProject(slug)
    switchTab('browse')
  }

  /**
   * 「去裁决」/「N 条待确认」→ 浏览页的待确认清单。
   *
   * 只调 `switchTab('browse')` 是不够的：浏览页默认是「列表」，待确认在四段控件
   * 的第三段里。真机反馈正是「仪表盘说有 41 条待确认，点进去不知道去哪处理」。
   */
  const openReview = () => {
    setFocusView('review')
    switchTab('browse')
  }

  return (
    <div className='mm-page'>
      <div className='mm-head'>
        <div className='mm-tabs'>
          <SegmentedControl
            options={TABS.filter(item => !ui || ui.tabs.includes(item.key))
              .map(item => ({ value: item.key, label: t(item.label) }))}
            value={tab}
            onValueChange={value => switchTab(value as Tab)}
            aria-label={t('记忆中枢功能分类')}
          />
        </div>
        <div className='mm-actions'>
          {status ? (
            <Badge variant={status.config.enabled ? 'success' : 'outline'} shape='tag'>
              {status.config.enabled
                ? t('{n} 条有效记忆', { n: status.live })
                : t('记忆服务已关闭')}
            </Badge>
          ) : null}
          {status && (status.review.pending ?? 0) > 0 ? (
            // 徽标本身就是入口：看到「41 条待确认」的第一反应是「点它」，
            // 以前它只是个 Badge，点了没反应，也没有别处写着处理路径。
            <button
              type='button'
              onClick={openReview}
              title={t('去「浏览 → 待确认」逐条批准或忽略')}
              style={{ background: 'none', border: 0, padding: 0, cursor: 'pointer', lineHeight: 0 }}
            >
              <Badge variant='warning' shape='tag'>{t('{n} 条待确认', { n: status.review.pending ?? 0 })}</Badge>
            </button>
          ) : null}
          <Button
            type='button'
            size='sm'
            variant='ghost'
            onClick={() => {
              // 「刷新」要刷得比「重新进入」更全：`loadShell()` 管壳这一层
              // （配置 / 状态 / 项目），`notifyMemoryRefresh()` 让所有页签的
              // `useLoad` 都重拉 —— 包括那些豁免了「进页面重拉」的贵端点
              // （导入页要递归扫十二个来源）。
              // 只调 loadShell 的话，用户点完会发现条数变了、体检和同步没变。
              void loadShell()
              notifyMemoryRefresh()
            }}
          >
            {t('刷新')}
          </Button>
        </div>
      </div>

      {error ? <div className='mm-error'>{error}</div> : null}

      {!booted ? (
        <div className='mm-boot'>
          <Spinner />
          <span>{t('正在读取记忆库…')}</span>
        </div>
      ) : (
        <div className='mm-body-area'>
          {tab === 'dashboard' ? (
            <TabBoundary name='仪表盘'>
              <Dashboard
                status={status}
                onGo={next => switchTab(next as Tab)}
                onReview={openReview}
                onOpenEntry={openEntryFromProject}
                onChanged={afterChange}
              />
            </TabBoundary>
          ) : null}

          {tab === 'browse' ? (
            <TabBoundary name='浏览'>
              <Browse
                kinds={kinds}
                projects={projects}
                onChanged={afterChange}
                focusId={focusId}
                onFocusConsumed={() => setFocusId(null)}
                focusProject={focusProject}
                onFocusProjectConsumed={() => setFocusProject(null)}
                focusView={focusView}
                onFocusViewConsumed={() => setFocusView(null)}
                pageSize={ui?.pageSize}
              />
            </TabBoundary>
          ) : null}

          {tab === 'projects' ? (
            <TabBoundary name='项目'>
              <Projects
                projects={projects}
                general={general}
                loading={projectsLoading}
                error={projectsError}
                onReload={() => void refreshProjects()}
                onOpenEntry={openEntryFromProject}
                onOpenProject={openProjectMemories}
                onChanged={afterChange}
              />
            </TabBoundary>
          ) : null}

          {tab === 'profile' ? <TabBoundary name='画像'><Profile /></TabBoundary> : null}
          {tab === 'agents' ? <TabBoundary name='接入'><Agents /></TabBoundary> : null}
          {tab === 'auto' ? (
            <TabBoundary name='自动化'>
              <Auto onGo={next => switchTab(next as Tab)} />
            </TabBoundary>
          ) : null}
          {tab === 'import' ? <TabBoundary name='导入'><Import /></TabBoundary> : null}
          {tab === 'sync' ? <TabBoundary name='同步'><Sync /></TabBoundary> : null}
          {tab === 'settings' ? (
            <TabBoundary name='配置'>
              <Settings onChanged={afterChange} />
            </TabBoundary>
          ) : null}
        </div>
      )}
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

declare global {
  interface Window {
    /** 记忆中枢（壳侧接口，与 `wbCheckin` / `wbAccountsView` 同一套约定）。 */
    wbMemory?: {
      load(): Promise<void>
      open(tab?: string): void
    }
  }
}

window.wbMemory = {
  load: async () => { /* 页面自己首屏拉数据，这里只保留接口形状 */ },
  open: (tab?: string) => {
    if (!tab) return
    const next = TABS.find(item => item.key === tab)
    if (next) activeTab = next.key
  },
}
