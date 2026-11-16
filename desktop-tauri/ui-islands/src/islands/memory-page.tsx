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
import { Index } from './memory-tab-index'
import { Settings } from './memory-tab-settings'

/**
 * Agent2API · 记忆中枢（页面岛）。
 *
 * ── 为什么在网关里，而不是插件里 ────────────────────────────
 * 与签到中心同一决定、同一理由（见 `checkin-page.tsx` 的模块头）：插件只能出
 * 静态 HTML、跑在 iframe 里，读不到宿主的 CSS 变量、拿不到组件库实例；
 * 而「记忆」的后端（存储 + 检索 + MCP 工具）本来就只能由网关提供 —— 插件连
 * 端点都加不了。所以页面归网关，插件不参与本功能。
 *
 * ── 为什么是十个页签 ────────────────────────────────────────
 * 对齐 AgentHub 的记忆中枢之后，这一页承载的不再只是「看记忆」：
 *
 *   dashboard 仪表盘  —— 现在什么状态、有什么要处理
 *   browse    浏览    —— 找一条、改一条、裁决待确认
 *   projects  项目    —— 按仓库归档、改名、合并
 *   profile   画像    —— 关于「我」的长期事实，每条带证据
 *   agents    接入    —— 11 个客户端装没装、真的调过没
 *   auto      自动化  —— 12 个后台任务的节奏与消耗
 *   import    导入    —— 把别处的历史会话搬进来
 *   sync      同步    —— WebDAV 多设备，冲突裁决
 *   index     索引    —— 派生数据的自检与重建
 *   settings  配置    —— 一百多个旋钮，schema 驱动
 *
 * 十项平铺在一条页签栏里而不是收进二级菜单：记忆的日常动作横跨这十项（看一条 →
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
  { key: 'index', label: '索引' },
  { key: 'settings', label: '配置' },
] as const

type Tab = typeof TABS[number]['key']

/** 当前页签（组件外也能读：壳切页时判断要不要刷新） */
let activeTab: Tab = 'dashboard'

function MemoryPage(): React.ReactElement {
  const [tab, setTab] = React.useState<Tab>(activeTab)
  const [status, setStatus] = React.useState<m.MemoryStatus | null>(null)
  const [kinds, setKinds] = React.useState<m.MemoryKindOption[]>([])
  const [projects, setProjects] = React.useState<m.MemoryProject[]>([])
  const [projectsLoading, setProjectsLoading] = React.useState(true)
  const [projectsError, setProjectsError] = React.useState('')
  const [error, setError] = React.useState('')
  const [booted, setBooted] = React.useState(false)
  const [focusId, setFocusId] = React.useState<string | null>(null)

  /**
   * 首屏一次拉全「壳数据」：状态、类别枚举、项目列表。
   *
   * 这三样被多个页签共用（状态在仪表盘与配置页、类别在浏览页、项目在浏览与项目页），
   * 每个页签各拉一次会得到三份可能不一致的快照 —— 用户刚写完一条记忆，切到仪表盘
   * 看到旧条数，会以为写入失败了。
   */
  const loadShell = React.useCallback(async () => {
    try {
      const [config, projectList] = await Promise.all([m.loadConfig(), m.loadProjects()])
      setStatus(config.status)
      setKinds(config.kinds)
      setProjects(projectList.items)
      setProjectsError('')
    } catch (caught) {
      setProjectsError(caught instanceof Error ? caught.message : String(caught))
    } finally {
      setProjectsLoading(false)
      setBooted(true)
    }
  }, [])

  React.useEffect(() => {
    void loadShell()
  }, [loadShell])

  const refreshProjects = React.useCallback(async () => {
    setProjectsLoading(true)
    try {
      const projectList = await m.loadProjects()
      setProjects(projectList.items)
      setProjectsError('')
    } catch (caught) {
      setProjectsError(caught instanceof Error ? caught.message : String(caught))
    } finally {
      setProjectsLoading(false)
    }
  }, [])

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

  const switchTab = (next: Tab) => {
    activeTab = next
    setTab(next)
    // 切到需要新鲜项目数据的页签时补一次（项目可能在别处被合并/归档）
    if (next === 'projects' || next === 'browse') void refreshProjects()
  }

  const openEntryFromProject = (id: string) => {
    setFocusId(id)
    switchTab('browse')
  }

  return (
    <div className='mm-page'>
      <div className='mm-head'>
        <div className='mm-tabs'>
          <SegmentedControl
            options={TABS.map(item => ({ value: item.key, label: t(item.label) }))}
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
          {status && status.review.pending > 0 ? (
            <Badge variant='warning' shape='tag'>{t('{n} 条待确认', { n: status.review.pending })}</Badge>
          ) : null}
          <Button
            type='button'
            size='sm'
            variant='ghost'
            onClick={() => { void loadShell() }}
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
            <Dashboard status={status} onGo={next => switchTab(next as Tab)} />
          ) : null}

          {tab === 'browse' ? (
            <Browse
              kinds={kinds}
              projects={projects}
              onChanged={afterChange}
              focusId={focusId}
              onFocusConsumed={() => setFocusId(null)}
            />
          ) : null}

          {tab === 'projects' ? (
            <Projects
              projects={projects}
              loading={projectsLoading}
              error={projectsError}
              onReload={() => void refreshProjects()}
              onOpenEntry={openEntryFromProject}
              onChanged={afterChange}
            />
          ) : null}

          {tab === 'profile' ? <Profile /> : null}
          {tab === 'agents' ? <Agents /> : null}
          {tab === 'auto' ? <Auto /> : null}
          {tab === 'import' ? <Import /> : null}
          {tab === 'sync' ? <Sync /> : null}
          {tab === 'index' ? <Index /> : null}
          {tab === 'settings' ? <Settings onChanged={afterChange} /> : null}
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
