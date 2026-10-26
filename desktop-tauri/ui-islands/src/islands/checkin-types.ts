/**
 * 签到中心的数据形状（`/api/checkin-center` 与相邻几条接口）。
 *
 * ── 为什么单开一个类型文件 ──────────────────────────────────
 * 三个标签页（中心 / 定时 / 记录）共用同一批类型，而它们分散在三个 tsx 里。
 * 类型定义跟着页面走会在 tsx 之间形成 import 环（center → 定时 → center），
 * 集中在这里就没有这个问题。
 *
 * ── 字段口径以谁为准 ────────────────────────────────────────
 * 全部照 `server/src/server/api/checkin_center.rs` 的 `json!` 逐键抄，**不是**
 * 照旧插件页（`ui/checkin/state.js`）的 normalize 函数抄 —— 那边是 iframe
 * 形态的兜底层（要处理跨源、缺字段、旧插件），迁到宿主后同源直连，后端给什么
 * 就用什么。少一层 normalize 就少一处会漂移的口径。
 */

/** 每日签到里的一条账号 */
export type CheckinAccount = {
  id: string
  name: string
  /** 凭据可用（不可用的账号签到必然失败，界面据此禁用按钮） */
  available: boolean
  /** 上次签到时刻（毫秒；缺失为 null） */
  checkinAt: number | null
  checkedInToday: boolean
  /** 版本：`cn` / `intl` / null。WorkBuddy 国际版执行的是「领日活」 */
  edition: string | null
  /** 成长中心连登天数（恒为数字，0 = 还没读到过） */
  streakDays: number
}

/**
 * 一家提供商的签到分组。
 *
 * `description` / `modes` / `uiHooks` 只在**插件申报过**时出现（旧插件不带这几个
 * 键）—— 所以是可选字段，缺省时界面走「只有签到一档」的兜底。
 */
export type ProviderGroup = {
  id: string
  label: string
  accounts: CheckinAccount[]
  doneCount: number
  totalCount: number
  /** 这一家怎么签到的一句话说明（插件申报） */
  description?: string | null
  /** 执行档位：`keepalive` / `claim` / `full`（插件申报） */
  modes?: string[] | null
  /** 附加 UI 钩子（插件申报，界面目前不用，留着给将来） */
  uiHooks?: string[] | null
}

/** 范围外账号（不能签到的原因聚合） */
export type OutOfScope = { label: string; reason: string; count: number }

/** 新手福利的账号入口（任务详情要展开时另查） */
export type OnboardingEntry = {
  id: string
  name: string
  provider: string
  /** 一次性福利的结算记忆（非空 = 已结清） */
  settled: unknown
}

/** 新手福利里的一条任务（`/api/accounts/{id}/onboarding`） */
export type OnboardingTask = {
  key: string
  title: string
  points: number
  done: boolean
  blocked: boolean
  claiming: boolean
  error: string
}

/** ZCode 套餐的账号入口 */
export type PlanEntry = {
  id: string
  name: string
  provider: string
  claimAt: number | null
  claimPlans: Record<string, number> | null
}

/** 成长任务的账号入口 */
export type GrowthEntry = { id: string; name: string; provider: string }

/** 一条定时签到条目 */
export type ScheduleEntry = {
  id: string
  label: string
  displayLabel: string
  enabled: boolean
  time: string
  providers: string[]
  lastFiredDate: string | null
  lastFiredToday: boolean
  /** 下次执行时刻（毫秒；总开关关着或条目停用时为 null） */
  nextRunAt: number | null
  lastResult: CheckinResult | null
}

/** 一次执行的摘要（定时条目 / 猫猫旅行共用同一形状） */
export type CheckinResult = {
  at?: number
  reason?: string
  succeeded?: number
  active?: number
  skipped?: number
  failedCount?: number
  failed?: string[] | number
  /** 猫猫旅行专用 */
  outcome?: 'completed' | 'failed'
  error?: string
  accounts?: number
  credit?: number
  actions?: TravelAction[]
}

export type TravelAction = {
  accountId?: string
  name?: string
  outcome?: string
  message?: string
  credit?: number
}

/** 定时签到状态（`/api/checkin-schedules`，与 `autoCheckin` 同形） */
export type AutoState = {
  enabled: boolean
  lastFiredToday: boolean
  running: boolean
  providerOptions: { id: string; label: string }[]
  schedules: ScheduleEntry[]
}

/** 保活模型链（`/api/checkin-keepalive`） */
export type KeepaliveState = {
  models: string[]
  defaultModels: string[]
}

/** 猫猫旅行排程（`/api/travel-schedule`） */
export type TravelState = {
  enabled: boolean
  available?: boolean
  pausedReason?: string | null
  times: string[]
  firedToday: string[]
  lastFiredDate: string | null
  lastFiredTime: string | null
  lastResult: CheckinResult | null
  running: boolean
}

export type TravelRunResult = {
  outcome: 'completed' | 'skipped'
  message?: string
  summary: CheckinResult | null
  schedule: TravelState
}

/** 一条签到历史（`history`） */
export type HistoryRow = {
  at: number
  date?: string
  reason?: string
  succeeded?: number
  total?: number
  skipped?: number
  failed?: number
  failedCount?: number
}

/** 一条签到记录明细（`/api/checkin-records`） */
export type RecordRow = {
  ts: number
  date?: string
  accountId?: string
  accountName?: string
  provider?: string
  source?: string
  outcome?: string
  reason?: string
  message?: string
  error?: string
  keptActive?: boolean
}

/** `/api/checkin-center` 的完整响应 */
export type CheckinCenter = {
  daily: {
    providers: ProviderGroup[]
    outOfScope: OutOfScope[]
    todayDone: number
    todayEligible: number
  }
  extras: {
    onboarding: OnboardingEntry[]
    plans: PlanEntry[]
    growth: GrowthEntry[]
  }
  auto: AutoState
  keepalive: KeepaliveState
  travel: TravelState
  history: HistoryRow[]
  /**
   * 插件申报的管理面板：`daily_keepalive`（保活模型链）/ `growth_tasks`（猫猫旅行）。
   *
   * 这两个是**地区专属玩法**（国际版的日活保活、国内版的猫猫旅行），只有对应
   * 地区的插件版本才提供。它们跟着插件走是对的 —— 那是插件独有的领域知识，
   * 网关不该假装知道「国际版有保活这回事」。
   */
  adminPanels: string[]
  /** 各家申报的签到链路信息（按 provider id 索引），形状与 ProviderGroup 的几个可选键一致 */
  checkinFeatures: Record<string, {
    description?: string | null
    modes?: string[] | null
    uiHooks?: string[] | null
  }>
}

/** 签到记录的分页响应 */
export type RecordsPage = {
  records: RecordRow[]
  total: number
}

/**
 * 这一家有哪些执行档位。
 *
 * 插件没申报时按「只有签到」处理 —— 与申报前的行为一致（那时所有家都只有一个
 * 「签到」按钮）。申报了 `modes` 就照它渲染，不再猜。
 */
export function modesOf(group: ProviderGroup | undefined): string[] {
  const modes = group?.modes
  if (!Array.isArray(modes) || modes.length === 0) return ['checkin']
  return modes.filter(mode => typeof mode === 'string' && mode)
}

/** 档位 → 按钮文案（`checkin` 是兜底档，不是插件申报值） */
export const MODE_LABEL: Record<string, string> = {
  checkin: '签到',
  keepalive: '保活',
  claim: '领取',
  full: '保活 + 领取',
}

/** 档位 → 悬停说明 */
export const MODE_TITLE: Record<string, string> = {
  checkin: '走这一家的每日签到接口',
  keepalive: '用免费模型与网页会话维持账号活跃 · 不领取奖励',
  claim: '只领取当天的奖励 · 不做保活',
  full: '保活并领取奖励',
}

/** 签到来源 → 展示名 */
export const SOURCE_LABEL: Record<string, string> = {
  schedule: '定时',
  startup: '补签',
  manual: '手动',
  single: '单签',
  activity: '活动',
}

/** 结果 → 展示名与色调 */
export const OUTCOME_VIEW: Record<string, { label: string; variant: 'success' | 'brand' | 'warning' | 'destructive' | 'outline' }> = {
  success: { label: '成功领取', variant: 'success' },
  already: { label: '今日已领', variant: 'brand' },
  pending: { label: '等待窗口', variant: 'warning' },
  failed: { label: '失败', variant: 'destructive' },
}

/** 毫秒时间戳 → 本地可读时间；缺失 / 非法给 `—` */
export function formatTime(value: unknown): string {
  const stamp = Number(value)
  if (!Number.isFinite(stamp) || stamp <= 0) return '—'
  const date = new Date(stamp)
  if (Number.isNaN(date.getTime())) return '—'
  return date.toLocaleString('zh-CN', { hour12: false })
}

/** 毫秒 → `HH:MM`（只关心当天时刻的场合） */
export function formatClock(value: unknown): string {
  const stamp = Number(value)
  if (!Number.isFinite(stamp) || stamp <= 0) return ''
  const date = new Date(stamp)
  if (Number.isNaN(date.getTime())) return ''
  const pad = (n: number) => String(n).padStart(2, '0')
  return `${pad(date.getHours())}:${pad(date.getMinutes())}`
}

/** 毫秒 → `MM-DD HH:MM`（记录表的时间列） */
export function formatShort(value: unknown): string {
  const stamp = Number(value)
  if (!Number.isFinite(stamp) || stamp <= 0) return '—'
  const date = new Date(stamp)
  if (Number.isNaN(date.getTime())) return '—'
  const pad = (n: number) => String(n).padStart(2, '0')
  return `${pad(date.getMonth() + 1)}-${pad(date.getDate())} ${pad(date.getHours())}:${pad(date.getMinutes())}`
}

declare global {
  interface Window {
    /**
     * 宿主壳的公开接口（`ui/app.js` 的 `window.wbApp`）。
     *
     * 只声明本页用得到的那几个 —— 全量声明会把壳的内部结构变成契约。需要新
     * 能力时在这里加一行，顺带也就记录了「谁在用」。
     */
    wbApp?: {
      /** 切页（空态里的「去插件页」用它，宿主是单页应用、没有 hash 路由） */
      showPage?: (name: string) => void
      toast?: (message: string, tone?: string) => void
      readonly currentPage?: string
    }
    /**
     * ZCode 套餐明细弹窗（宿主已有的命令式岛，见 `zcode-plans-modal.tsx`）。
     *
     * 「活动福利」那一行只提供入口：套餐明细与逐份领取都在这个弹窗里。
     * 迁到客户端之后可以直接调 —— iframe 形态下拿不到它，只能靠 postMessage
     * 请宿主代开，宿主没响应时退化成一句「去账号页操作」。
     */
    wbZcodePlans?: { open(options: { id: string; name?: string }): void }
  }
}
