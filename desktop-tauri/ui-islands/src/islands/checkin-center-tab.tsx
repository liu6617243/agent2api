import * as React from 'react'
import { Badge, Button, Progress } from '@ui'
import {
  bridgeApi, growthOf, loadCenter, patch, queryGrowth, runAllCheckin, runGrowth,
  runSchool, runTravelOnce, signSingle, store, toast,
  type OnboardingCache,
} from './checkin-store'
import {
  MODE_LABEL, MODE_TITLE, formatTime, modesOf,
  type CheckinAccount, type CheckinCenter, type GrowthEntry, type OnboardingEntry,
  type OnboardingTask, type PlanEntry, type ProviderGroup,
} from './checkin-types'

/**
 * 「签到中心」标签页：总览卡 + 每日签到 + 新手福利 + 活动套餐 + 成长任务。
 *
 * ── 数据全部来自快照 ────────────────────────────────────────
 * 分组、已签判定、范围外原因都在网关算好（`api::checkin_center`）。这里只画。
 * **不要**按 provider id 写死任何判断 —— 那是插件申报的（`description` /
 * `modes` / `uiHooks` / `onboarding` / `plans` / `growth`）。
 *
 * ── 惰性块 ──────────────────────────────────────────────────
 * 新手福利与成长任务的**明细**要打上游，快照刻意不带（快照必须「零上游请求」）。
 * 行上有「查询任务」按钮，点了才查。
 */

/* ─── 空态 ─────────────────────────────────── */

/**
 * 没装插件时的空态。
 *
 * 这是「签到中心常驻菜单」的代价，也是它的价值：用户看到一个明确的说明与去处，
 * 而不是一个凭空消失的菜单项。判定依据是快照里 `checkinFeatures` 为空 ——
 * 那是插件申报表，空 = 没有任何插件提供签到能力。
 */
function EmptyState(): React.ReactElement {
  const hasFeature = Object.keys(store.center?.checkinFeatures ?? {}).length > 0
  const hasAccounts = (store.center?.daily?.providers?.length ?? 0) > 0
  const missingPlugin = !hasFeature
  const message = missingPlugin
    ? '还没有可签到的提供商。签到能力由提供商插件提供，请先到「插件」页安装 Agent2API 提供商插件。'
    : !hasAccounts
      ? '插件已安装，但还没有可签到的账号。到「账号」页添加并启用账号后，这里会自动出现。'
      : ''
  if (!message) return <></>
  return (
    <section className="panel">
      <div className="panel-head">
        <h2>每日签到</h2>
      </div>
      <div className="ck-empty-state">
        <p>{message}</p>
        <div className="ck-empty-actions">
          {/* 用壳的 `showPage` 而不是改 location：宿主是单页应用，没有 hash 路由 */}
          <Button
            size="sm"
            variant="outline"
            onClick={() => window.wbApp?.showPage?.(missingPlugin ? 'plugins' : 'accounts')}
          >
            去「{missingPlugin ? '插件' : '账号'}」页
          </Button>
          <Button size="sm" variant="ghost" onClick={() => void loadCenter()}>
            重新检查
          </Button>
        </div>
      </div>
    </section>
  )
}

/* ─── 总览卡 ───────────────────────────────── */

function StatCard({ label, value, foot, extra, onClick }: {
  label: string
  value: React.ReactNode
  foot?: React.ReactNode
  extra?: React.ReactNode
  onClick?: () => void
}): React.ReactElement {
  const className = onClick ? 'ck-stat ck-stat-link' : 'ck-stat'
  return (
    <div
      className={className}
      role={onClick ? 'button' : undefined}
      tabIndex={onClick ? 0 : undefined}
      onClick={onClick}
      onKeyDown={onClick
        ? event => { if (event.key === 'Enter') onClick() }
        : undefined}
    >
      <div className="ck-stat-label">{label}</div>
      <div className="ck-stat-value">{value}</div>
      {foot ? <div className="ck-stat-foot">{foot}</div> : null}
      {extra}
    </div>
  )
}

/**
 * 总览卡（四张，与旧实现逐项一致）。
 *
 * 旧版是：今日签到（带进度条）/ 定时签到（可点进「定时签到」页）/
 * 待领新手福利 / 累计签到记录（可点进「签到记录」页）。
 * 点卡切页走自定义事件，理由见 `checkin-page.tsx` 的 `wb:checkin-tab`。
 */
function StatsGrid({ center }: { center: CheckinCenter }): React.ReactElement {
  const { daily, extras } = center
  const auto = center.auto
  const percent = daily.todayEligible > 0
    ? (daily.todayDone / daily.todayEligible) * 100
    : 0
  const outCount = daily.outOfScope.reduce((sum, item) => sum + item.count, 0)

  // 定时签到卡：启用的条目数与下一条的执行时刻
  const enabledEntries = (auto?.schedules ?? []).filter(entry => entry.enabled)
  const nextRun = enabledEntries
    .map(entry => Number(entry.nextRunAt))
    .filter(stamp => Number.isFinite(stamp) && stamp > 0)
    .sort((a, b) => a - b)[0]

  // 新手福利卡：已查到的账号里有几项待领
  let onboardingTotal = 0
  let onboardingChecked = 0
  let onboardingPending = 0
  for (const row of extras.onboarding) {
    const cache = store.onboarding.get(row.id)
    if (cache?.status !== 'loaded') continue
    onboardingChecked += 1
    onboardingTotal += cache.tasks.length
    onboardingPending += cache.tasks.filter(task => !task.done && !task.blocked).length
  }

  const goTab = (tab: string) => window.dispatchEvent(new CustomEvent('wb:checkin-tab', { detail: { tab } }))

  return (
    <div className="ck-stats">
      <StatCard
        label="今日签到"
        value={daily.todayEligible > 0
          ? <>{daily.todayDone}<small> / {daily.todayEligible}</small></>
          : <span className="ck-pending">—</span>}
        extra={daily.todayEligible > 0
          ? <Progress className="ck-stat-bar" value={percent} />
          : null}
        foot={outCount > 0 ? `另有 ${outCount} 个账号不参与每日签到` : '全部账号均可签到'}
      />
      <StatCard
        label="定时签到"
        value={enabledEntries.length > 0
          ? <>{enabledEntries.length}<small> / {auto?.schedules?.length ?? 0} 条启用</small></>
          : <span className="ck-pending">—</span>}
        foot={auto?.enabled !== true
          ? '总开关已关闭 · 在「定时签到」页打开'
          : nextRun
            ? `下次 ${formatTime(nextRun)} · 点此管理条目`
            : '暂无启用的条目 · 点此添加'}
        onClick={() => goTab('schedule')}
      />
      <StatCard
        label="待领新手福利"
        value={extras.onboarding.length > 0
          ? <>{onboardingPending}<small> 项</small></>
          : <span className="ck-pending">无</span>}
        foot={extras.onboarding.length > 0
          ? `已有结果 ${onboardingChecked} / 共 ${extras.onboarding.length} 个账号`
          : '没有支持新手福利的账号'}
      />
      <StatCard
        label="累计签到记录"
        value={store.recordsLoaded ? store.recordsTotal : <span className="ck-pending">—</span>}
        foot="每账号一条明细 · 点此查看「签到记录」"
        onClick={() => goTab('records')}
      />
    </div>
  )
}

/**
 * 范围外账号的说明（**卡内的一行**，不是独立卡片）。
 *
 * 旧实现就是把它挂在「每日签到」卡底部的 `ck-fold-note` 里：它是那张卡的
 * **脚注**（解释「为什么这些账号没出现在上面的列表里」），独立成卡会显得
 * 它们是另一类功能。这是我早先迁移时漏掉的一处。
 */
function OutOfScopeNote({ items }: { items: { label: string; reason: string; count: number }[] }): React.ReactElement {
  const list = items
    .map(item => `${item.label} ×${item.count}（${item.reason}）`)
    .join('；')
  return (
    <div className="ck-fold-note">
      不参与每日签到：{list}。CodeArts 的一次性奖励在下方「新手福利」，它的每日福利就在上方「每日签到」里；
      ZCode 的套餐领取见「活动福利」。
    </div>
  )
}

/* ─── 每日签到 ─────────────────────────────── */

/** 分组徽标：无账号 / 已签 / 部分 / 待签 */
function GroupBadge({ group }: { group: ProviderGroup }): React.ReactElement {
  if (!group.totalCount) return <Badge variant="outline" shape="tag">无账号</Badge>
  if (group.doneCount >= group.totalCount) return <Badge variant="success" shape="tag">已签</Badge>
  if (group.doneCount > 0) return <Badge variant="warning" shape="tag">部分</Badge>
  return <Badge variant="brand" shape="tag">待签</Badge>
}

/** 账号明细表（展开区） */
function AccountTable({ group }: { group: ProviderGroup }): React.ReactElement {
  const modes = modesOf(group)
  // 分档执行由插件申报：keepalive / claim / full 各一颗按钮 —— 领取才加积分、
  // 保活只维持活跃，分开是刻意的
  const dailyActivity = modes.some(mode => mode === 'keepalive' || mode === 'claim' || mode === 'full')
  return (
    <table className="ck-table">
      <thead>
        <tr>
          <th>账号</th>
          <th>今日签到</th>
          <th>上次签到时间</th>
          <th />
        </tr>
      </thead>
      <tbody>
        {group.accounts.map(account => (
          <AccountRow key={account.id} account={account} modes={modes} dailyActivity={dailyActivity} />
        ))}
      </tbody>
    </table>
  )
}

function AccountRow({ account, modes, dailyActivity }: {
  account: CheckinAccount
  modes: string[]
  dailyActivity: boolean
}): React.ReactElement {
  const busy = (mode: string) => store.signing.has(`${account.id}:${mode}`)
  return (
    <tr>
      <td>
        {account.name}
        {/* 连登天数：0 = 还没读到过，不显示成「连登 0 天」 */}
        {account.streakDays > 0
          ? <span className="ck-sub"> · 连登 {account.streakDays} 天</span>
          : null}
        {account.available ? null : <Badge variant="outline" shape="tag">凭据不可用</Badge>}
      </td>
      <td>
        {account.checkedInToday
          ? <Badge variant="success" shape="tag">已签</Badge>
          : <Badge variant="brand" shape="tag">待签</Badge>}
      </td>
      <td className="ck-sub">{formatTime(account.checkinAt)}</td>
      <td className="ck-right">
        <span className="ck-inline">
          {dailyActivity
            ? modes.map(mode => (
              <Button
                key={mode}
                size="sm"
                variant="ghost"
                disabled={busy(mode) || !account.available}
                title={MODE_TITLE[mode]}
                onClick={() => void signSingle(account.id, mode)}
              >
                {busy(mode) ? `${MODE_LABEL[mode] ?? mode}中…` : (MODE_LABEL[mode] ?? mode)}
              </Button>
            ))
            : (
              <Button
                size="sm"
                variant="ghost"
                disabled={busy('checkin') || !account.available}
                onClick={() => void signSingle(account.id, 'checkin')}
              >
                {busy('checkin') ? '签到中…' : account.checkedInToday ? '重签' : '签到'}
              </Button>
            )}
        </span>
      </td>
    </tr>
  )
}

function ProviderRow({ group }: { group: ProviderGroup }): React.ReactElement {
  const [expanded, setExpanded] = React.useState(false)
  const hasAccounts = group.totalCount > 0
  return (
    <div className={expanded ? 'ck-row open' : 'ck-row'}>
      <div
        className="ck-row-main"
        role={hasAccounts ? 'button' : undefined}
        tabIndex={hasAccounts ? 0 : undefined}
        onClick={hasAccounts ? () => setExpanded(v => !v) : undefined}
        onKeyDown={hasAccounts
          ? event => { if (event.key === 'Enter') setExpanded(v => !v) }
          : undefined}
      >
        <ProviderLogo id={group.id} label={group.label} />
        <div className="ck-row-info">
          <div className="ck-row-name">{group.label}</div>
          {group.description ? <div className="ck-row-desc">{group.description}</div> : null}
        </div>
        <div className="ck-row-right">
          <span className="ck-row-count">
            {hasAccounts ? `${group.totalCount} 账号 · ${group.doneCount} 已签` : '—'}
          </span>
          <GroupBadge group={group} />
          {hasAccounts ? <Chevron open={expanded} /> : null}
        </div>
      </div>
      {expanded ? (
        <div className="ck-row-detail">
          <AccountTable group={group} />
        </div>
      ) : null}
    </div>
  )
}

/* ─── 通用小件 ─────────────────────────────── */

/**
 * 提供商图标。
 *
 * 图标是**客户端资源**（`ui/assets/providers/<id>.png`），迁到宿主后与账号页
 * 同源同目录，直接用相对路径即可 —— iframe 形态下这条路径够不到，只能靠网关
 * 开一条 `/plugins/runtime/providers/*` 路由转发，那正是这次迁移要去掉的东西。
 */
function ProviderLogo({ id, label }: { id: string; label: string }): React.ReactElement {
  const [failed, setFailed] = React.useState(false)
  const slug = ICON_FALLBACK[id] ?? id
  const initial = (label || id || '·').trim().slice(0, 1).toUpperCase() || '·'
  return (
    <span className="ck-badge" title={label}>
      {failed
        ? initial
        : (
          <img
            src={`assets/providers/${slug}.png`}
            alt=""
            loading="lazy"
            onError={() => setFailed(true)}
          />
        )}
    </span>
  )
}

/** 国际版与国内版共用同一张图（与账号页 `PROVIDER_ICONS` 的回落口径一致） */
const ICON_FALLBACK: Record<string, string> = {
  'workbuddy-intl': 'workbuddy',
  'autoclaw-intl': 'autoclaw',
  'qoder-intl': 'qoder',
  'zcode-intl': 'zcode',
  'monkeycode-intl': 'monkeycode',
  'modelscope-intl': 'modelscope',
  'cline-pass': 'cline',
  'cline-free': 'cline',
  'accio-cn': 'accio',
}

function Chevron({ open }: { open: boolean }): React.ReactElement {
  return (
    <span className={open ? 'ck-chev open' : 'ck-chev'}>
      <svg viewBox="0 0 24 24" width="14" height="14" fill="none" stroke="currentColor"
        strokeWidth="1.8" strokeLinecap="round" strokeLinejoin="round" aria-hidden="true">
        <path d="M9.5 5.5 16 12l-6.5 6.5" />
      </svg>
    </span>
  )
}

/* ─── 新手福利 ─────────────────────────────── */

/**
 * 新手福利行。
 *
 * 明细要打上游，快照不带 —— 所以这里的按钮是「查询任务」，点了才查。
 * 状态缓存在 `store.onboarding`（跨标签页保留，切走再回来不用重查）。
 */
function OnboardingRow({ row }: { row: OnboardingEntry }): React.ReactElement {
  const cache = store.onboarding.get(row.id)
  const [expanded, setExpanded] = React.useState(false)
  const tasks = cache?.tasks ?? []
  const unclaimed = tasks.filter(task => !task.done && !task.blocked).length
  const status = cache?.status ?? 'idle'

  return (
    <div className={expanded && tasks.length ? 'ck-row open' : 'ck-row'}>
      <div className="ck-row-main">
        <ProviderLogo id={row.provider} label={row.name} />
        <div className="ck-row-info">
          <div className="ck-row-name">{row.name}</div>
          <div className="ck-row-desc">
            {status === 'loaded'
              ? `已领 ${tasks.length - unclaimed}/${tasks.length} · 累计 ${cache?.earned ?? 0} 积分`
              : status === 'loading' ? '正在查询…' : '尚未查询任务状态'}
            {cache?.checkedAt ? ` · 查询于 ${formatTime(cache.checkedAt)}` : ''}
          </div>
        </div>
        <div className="ck-row-right">
          {status === 'loaded' && unclaimed > 0
            ? <Badge variant="warning" shape="tag">{unclaimed} 项待领</Badge>
            : status === 'loaded'
              ? <Badge variant="success" shape="tag">全部领取</Badge>
              : status === 'error'
                ? <Badge variant="destructive" shape="tag">查询失败</Badge>
                : null}
          <Button
            size="sm"
            variant="outline"
            disabled={status === 'loading' || cache?.claiming === true}
            onClick={() => void queryOnboarding(row.id, true)}
          >
            {status === 'loading' ? '查询中…' : '查询任务'}
          </Button>
          <Button
            size="sm"
            variant="outline"
            disabled={status !== 'loaded' || cache?.claiming === true || (row.provider !== 'officeace' && unclaimed === 0)}
            onClick={() => void claimOnboarding(row.id)}
          >
            {cache?.claiming ? '领取中…' : unclaimed > 0 ? `一键领取（${unclaimed}）` : '领取'}
          </Button>
          {tasks.length > 0 ? (
            <span
              className="ck-chev-hit"
              onClick={event => { event.stopPropagation(); setExpanded(v => !v) }}
            >
              <Chevron open={expanded} />
            </span>
          ) : null}
        </div>
      </div>
      {status === 'error' && cache?.error ? (
        <div className="ck-fold-note">查询失败：{cache.error}</div>
      ) : null}
      {expanded && tasks.length > 0 ? (
        <div className="ck-row-detail">
          {tasks.map(task => (
            <div className="ck-task-line" key={task.key}>
              <span className={task.done ? 'ck-task-tick done' : 'ck-task-tick'}>{task.done ? '✓' : '·'}</span>
              <span className="ck-task-title">{task.title}</span>
              <span className="ck-task-pts">+{task.points}</span>
              <span className="ck-task-st">
                {task.claiming
                  ? <Badge variant="warning" shape="tag">领取中</Badge>
                  : task.done
                    ? <Badge variant="success" shape="tag">已领取</Badge>
                    : task.blocked
                      ? <Badge variant="outline" shape="tag">暂不可领</Badge>
                      : task.error
                        ? <Badge variant="destructive" shape="tag">{task.error}</Badge>
                        : <Badge variant="brand" shape="tag">可领取</Badge>}
              </span>
            </div>
          ))}
        </div>
      ) : null}
    </div>
  )
}

/** 查询新手任务（`refresh` = 强制实查上游，不吃本地结算记忆） */
async function queryOnboarding(id: string, refresh: boolean): Promise<void> {
  patchOnboarding(id, { status: 'loading', error: '' })
  try {
    const data = await bridgeApi().getOnboardingTasks?.(id, refresh)
    const tasks = normalizeTasks(data?.tasks)
    patchOnboarding(id, {
      status: 'loaded',
      tasks,
      earned: Number(data?.earned) || 0,
      checkedAt: Date.now(),
    })
  } catch (error) {
    patchOnboarding(id, { status: 'error', error: messageOf(error) })
  }
}

/** 一键领取 */
async function claimOnboarding(id: string): Promise<void> {
  const cache = store.onboarding.get(id)
  if (!cache || cache.claiming) return
  patchOnboarding(id, { claiming: true })
  try {
    const data = await bridgeApi().claimOnboardingTasks?.(id)
    const tasks = normalizeTasks(data?.tasks)
    patchOnboarding(id, {
      claiming: false,
      status: 'loaded',
      tasks: tasks.length ? tasks : cache.tasks,
      earned: Number(data?.earned) || cache.earned,
      checkedAt: Date.now(),
    })
    const message = typeof data?.message === 'string' ? data.message.trim() : ''
    toast(message || '✅ 新手福利领取完成')
  } catch (error) {
    patchOnboarding(id, { claiming: false })
    toast(`领取失败：${messageOf(error)}`, 'err')
  }
}

function patchOnboarding(id: string, next: Partial<OnboardingCache>): void {
  const map = new Map(store.onboarding)
  map.set(id, { ...(map.get(id) ?? emptyOnboarding()), ...next })
  patch({ onboarding: map })
}

function emptyOnboarding(): OnboardingCache {
  return { status: 'idle', tasks: [], earned: 0, claiming: false }
}

function normalizeTasks(raw: unknown): OnboardingTask[] {
  if (!Array.isArray(raw)) return []
  return raw
    .map(item => {
      const row = (item ?? {}) as Record<string, unknown>
      const key = typeof row.key === 'string' ? row.key : ''
      if (!key) return null
      return {
        key,
        title: typeof row.title === 'string' && row.title ? row.title : key,
        points: Number(row.points) || 0,
        done: row.done === true,
        blocked: row.blocked === true,
        claiming: row.claiming === true,
        error: typeof row.error === 'string' ? row.error : '',
      }
    })
    .filter((item): item is OnboardingTask => item !== null)
}

function messageOf(error: unknown): string {
  if (error instanceof Error) return error.message
  if (typeof error === 'string') return error
  return String(error)
}

/* ─── 活动福利（ZCode 套餐）────────────────── */

/**
 * 今天（北京时间）在本地台账里领过的套餐 id。
 *
 * 上游按北京判「今天」，所以日界也按北京算 —— 与宿主 `claimedPlanIdsToday`
 * 同一口径（跟本机时区走会在海外的部署上把「今天」判错）。
 */
function claimedPlansToday(claimPlans: Record<string, number> | null | undefined): string[] {
  if (!claimPlans || typeof claimPlans !== 'object') return []
  const today = new Date(Date.now() + 8 * 3600 * 1000).toISOString().slice(0, 10)
  return Object.entries(claimPlans)
    .filter(([, at]) => {
      const stamp = Number(at)
      if (!Number.isFinite(stamp) || stamp <= 0) return false
      return new Date(stamp + 8 * 3600 * 1000).toISOString().slice(0, 10) === today
    })
    .map(([planId]) => planId)
}

/**
 * 活动福利的一行（ZCode 限时体验套餐）。
 *
 * 这一行只提供**入口**：套餐明细与逐份领取在宿主那个弹窗里（`wbZcodePlans`）。
 * 迁到客户端之后可以直接调它 —— 早先页面在 iframe 里拿不到那个岛，只能靠
 * postMessage 请宿主代开，宿主没响应时就退化成一句「去账号页操作」。
 */
function PlanRow({ row }: { row: PlanEntry }): React.ReactElement {
  const claimed = claimedPlansToday(row.claimPlans)
  const hasClaimed = claimed.length > 0
  const desc = hasClaimed
    ? '点「套餐明细」看领到了哪几份、什么时候生效；活动期内每天可再领'
    : row.claimAt
      ? `上次领取 ${formatTime(row.claimAt)} · 领取需通过滑块验证码`
      : '限时体验套餐（start-plan），活动期内每天一份 · 领取需滑块验证码'
  return (
    <div className="ck-welfare-row">
      <ProviderLogo id={row.provider || 'zcode'} label="ZCode" />
      <div className="ck-prov-info">
        <div className="ck-prov-name">
          {row.name || row.id}
          {hasClaimed
            ? (
              <Badge
                variant="success"
                shape="tag"
                title={`本地台账里今天（北京时间）领过的套餐：${claimed.join('、')}`}
              >
                今天已领 {claimed.length} 份
              </Badge>
            )
            : <Badge variant="outline" shape="tag" title="本地台账里今天还没领过；活动期内每天发一份新套餐">今天未领</Badge>}
        </div>
        <div className="ck-prov-desc">{desc}</div>
      </div>
      <div className="ck-prov-right">
        <Button
          size="sm"
          variant="outline"
          onClick={() => window.wbZcodePlans?.open?.({ id: row.id, name: row.name })}
        >
          {hasClaimed ? '套餐明细' : '去领取'}
        </Button>
      </div>
    </div>
  )
}

/* ─── 成长任务（WorkBuddy 国内版）────────────── */

/**
 * 成长任务的一行。
 *
 * 三块内容：任务树（逐条点亮 / 领奖）、开学季（任务 + 抽奖）、猫猫旅行。
 * 明细要打一次上游，所以行上是「查询任务」按钮，点了才查。
 *
 * 「一键完成」逐条调用单任务端点（见 `runGrowth` 的说明）：全量一次请求会
 * 超过管理 API 的 60 秒转发超时。
 */
function GrowthRow({ row }: { row: GrowthEntry }): React.ReactElement {
  const cache = growthOf(row.id)
  const tasks = cache.tasks ?? []
  // 能自动执行的：受支持、非「需真实操作」
  const runnable = tasks.filter(task => task.supported && !task.unforgeable)
  const pending = runnable.filter(task => task.status !== 'claimed' && task.current < task.target)
  const loaded = cache.status === 'loaded' && tasks.length > 0
  const [expanded, setExpanded] = React.useState(false)
  const running = cache.running || cache.runningSchool

  return (
    <div className={expanded && loaded ? 'ck-prov-row open' : 'ck-prov-row'}>
      <div
        className="ck-prov-main"
        role={loaded ? 'button' : undefined}
        tabIndex={loaded ? 0 : -1}
        aria-expanded={loaded ? expanded : undefined}
        onClick={loaded ? () => setExpanded(v => !v) : undefined}
        onKeyDown={loaded
          ? event => {
            if (event.key === 'Enter' || event.key === ' ') {
              event.preventDefault()
              setExpanded(v => !v)
            }
          }
          : undefined}
      >
        <ProviderLogo id={row.provider || 'workbuddy'} label="WorkBuddy" />
        <div className="ck-prov-info">
          <div className="ck-prov-name">{row.name || row.id}</div>
          <div className="ck-prov-desc">
            {cache.status === 'loaded'
              ? `${runnable.length} 项任务 · ${pending.length} 项可完成`
              : '查询任务树并一键点亮 / 领奖（含开学季与猫猫旅行）'}
            {cache.checkedAt ? ` · 查询于 ${formatTime(cache.checkedAt)}` : ''}
          </div>
        </div>
        <div className="ck-prov-right" onClick={event => event.stopPropagation()}>
          {cache.status === 'loaded'
            ? pending.length > 0
              ? <Badge variant="warning" shape="tag">{pending.length} 项可完成</Badge>
              : <Badge variant="success" shape="tag">全部完成</Badge>
            : cache.status === 'error'
              ? <Badge variant="destructive" shape="tag">查询失败</Badge>
              : null}
          <Button
            size="sm"
            variant="outline"
            disabled={cache.status === 'loading' || running}
            onClick={() => void queryGrowth(row.id)}
          >
            {cache.status === 'loading' ? '查询中…' : '查询任务'}
          </Button>
          <Button
            size="sm"
            variant="outline"
            disabled={cache.status !== 'loaded' || running}
            title="接单 + 点亮 + 领奖（只处理已实测沉淀的任务）"
            onClick={() => void runGrowth(row.id, 'full')}
          >
            {cache.running ? '执行中…' : '一键完成'}
          </Button>
          <Button
            size="sm"
            variant="ghost"
            disabled={cache.status !== 'loaded' || running}
            title="只领取已完成任务的奖励（幂等，重复点不会重复入账）"
            onClick={() => void runGrowth(row.id, 'claim')}
          >
            领取
          </Button>
          {loaded ? <Chevron open={expanded} /> : null}
        </div>
      </div>

      {cache.status === 'error' ? (
        <div className="ck-fold-note">查询失败：{cache.error}</div>
      ) : null}

      {expanded && loaded ? (
        <div className="ck-prov-detail">
          <div className="ck-fold-note">
            {cache.energy !== null && cache.energy !== undefined ? `能量 ${cache.energy}` : ''}
            {cache.streak !== null && cache.streak !== undefined ? ` · 连登 ${cache.streak} 天` : ''}
            {cache.energy === null && cache.streak === null ? '（能量 / 连登读数本次未取到）' : ''}
          </div>
          {tasks.map(task => {
            const result = cache.results.find(item => item.code === task.code)
            return (
              <div className="ck-task-line" key={task.code}>
                <span className={task.status === 'claimed' ? 'ck-task-tick done' : 'ck-task-tick'}>
                  {task.status === 'claimed' ? '✓' : '·'}
                </span>
                <span title={task.description || task.source || task.code}>
                  {task.title || task.code}
                  {task.target > 1 ? ` (${task.current}/${task.target})` : ''}
                </span>
                {result ? <span className="ck-task-pts">{result.message}</span> : null}
                <span className="ck-task-st">
                  {task.unforgeable
                    ? <Badge variant="outline" shape="tag">需真实操作</Badge>
                    : task.supported
                      ? task.status === 'claimed'
                        ? <Badge variant="success" shape="tag">已领取</Badge>
                        : task.status === 'completed'
                          ? <Badge variant="brand" shape="tag">可领取</Badge>
                          : task.locked
                            ? <Badge variant="outline" shape="tag">未解锁</Badge>
                            : <Badge variant="warning" shape="tag">待完成</Badge>
                      : <Badge variant="outline" shape="tag">不支持</Badge>}
                </span>
              </div>
            )
          })}
          {/* 开学季与猫猫旅行：各自一个动作按钮（不走任务树的逐条执行） */}
          <div className="ck-grow-extra">
            <Button
              size="sm"
              variant="outline"
              disabled={running}
              onClick={() => void runSchool(row.id, false)}
            >
              {cache.runningSchool ? '执行中…' : '开学季任务'}
            </Button>
            <Button
              size="sm"
              variant="outline"
              disabled={running}
              onClick={() => void runSchool(row.id, true)}
            >
              开学季 + 抽奖
            </Button>
            <Button
              size="sm"
              variant="outline"
              disabled={running}
              onClick={() => void runTravelOnce(row.id)}
            >
              猫猫旅行一趟
            </Button>
          </div>
        </div>
      ) : null}
    </div>
  )
}

/* ─── 主体 ─────────────────────────────────── */

export function CenterTab({ center }: { center: CheckinCenter }): React.ReactElement {
  const { daily, extras } = center
  const hasAnything = daily.providers.length > 0 || daily.outOfScope.length > 0
  const runningAll = store.runningAll || center.auto?.running === true

  return (
    <div className="ck-main">
      {hasAnything ? <StatsGrid center={center} /> : null}
      {!hasAnything ? <EmptyState /> : null}

      {daily.providers.length > 0 ? (
        <section className="panel">
          <div className="panel-head">
            <h2>每日签到</h2>
            <span className="panel-sub">点击行展开账号明细 · 单账号签到不受范围限制</span>
            <Button
              size="sm"
              variant="default"
              className="ml-auto flex-none"
              disabled={runningAll}
              onClick={() => void runAllCheckin()}
            >
              {runningAll ? '签到中…' : '立即全部签到'}
            </Button>
          </div>
          <div>
            {daily.providers.map(group => <ProviderRow key={group.id} group={group} />)}
            {/* 范围外账号是这张卡的**脚注**（见 OutOfScopeNote 的说明） */}
            {daily.outOfScope.length > 0 ? <OutOfScopeNote items={daily.outOfScope} /> : null}
          </div>
        </section>
      ) : null}

      <section className="panel">
        <div className="panel-head">
          <h2>新手福利</h2>
          <span className="panel-sub">一次性福利 · 签到后自动查询并领取，领完记在本机不再查上游 · 点击行展开任务清单</span>
          {extras.onboarding.length > 0 ? (
            <Button
              size="sm"
              variant="outline"
              className="ml-auto flex-none"
              onClick={() => {
                for (const row of extras.onboarding) {
                  const cached = store.onboarding.get(row.id)
                  // 已结清的账号不再问上游（本地结算记忆就是终态）
                  if (cached?.status === 'loaded' && row.settled) continue
                  void queryOnboarding(row.id, false)
                }
              }}
            >
              全部查询
            </Button>
          ) : null}
        </div>
        {extras.onboarding.length > 0
          ? <div>{extras.onboarding.map(row => <OnboardingRow key={row.id} row={row} />)}</div>
          : <div className="ck-empty">没有支持新手福利的账号。</div>}
      </section>

      <section className="panel">
        <div className="panel-head">
          <h2>活动福利</h2>
          <span className="panel-sub">限时体验套餐 · 领取需滑块验证码 · 逐份查看与领取</span>
        </div>
        {extras.plans.length > 0
          ? <div>{extras.plans.map(row => <PlanRow key={row.id} row={row} />)}</div>
          : <div className="ck-empty">没有可领套餐的账号（ZCode 未添加）。</div>}
      </section>

      <section className="panel">
        <div className="panel-head">
          <h2>成长任务</h2>
          <span className="panel-sub">WorkBuddy 国内版 · 查询任务树后一键点亮 / 领奖 · 含开学季与猫猫旅行 · 点击行展开明细</span>
        </div>
        {extras.growth.length > 0
          ? <div>{extras.growth.map(row => <GrowthRow key={row.id} row={row} />)}</div>
          : <div className="ck-empty">没有支持成长任务的账号（需要 WorkBuddy 国内版）。</div>}
      </section>
    </div>
  )
}
