/**
 * 记忆中枢 · 数据层（接口调用与类型）。
 *
 * ── 为什么单独一层 ──────────────────────────────────────────
 * 页面里有十个视图共用同一批数据（仪表盘 / 浏览 / 项目 / 画像 / 接入 / 自动化 /
 * 导入 / 同步 / 索引 / 配置），把 `invoke` 与字段名散在组件里，改一个字段要翻
 * 十处。这里收口：**接口形状只在这个文件里出现一次**，组件只认类型。
 *
 * ── 类型为什么必须与 Rust 那边逐字对齐 ──────────────────────
 * 这些类型是**手写的**（没有 ts-rs 之类的生成器），所以它们是「一份需要人工
 * 维护的契约」。注释里标了每个形状来自哪个函数，改后端时按注释去对一眼，
 * 比在浏览器里猜「为什么这个字段是 undefined」快得多。
 *
 * ── 请求走哪个通道 ──────────────────────────────────────────
 * 与插件页、账号页一致：桌面壳的 `api_request` command（`api()` 封装见
 * `plugins-shared.ts`）。网页端由壳自己带面板会话，所以这里不处理鉴权 ——
 * 浏览器直连 `http://127.0.0.1:3065` 时由网关的 protected 中间件挡。
 *
 * ── 为什么配置是「schema 驱动」而不是一组固定字段 ────────────
 * 后端有一百多个旋钮，前端要是自己抄一份枚举，每次加旋钮都要改两处、还必然
 * 出现「后端有前端没有」的漂移。所以 `loadConfig` 把 schema 一起取回来，配置页
 * 按 `kind` 渲染控件、按 `tier` 折叠 —— **前端永远不需要知道有哪些旋钮**。
 */

import { api } from './plugins-shared'

/* ─── 记忆条目（model.rs `MemoryEntry::to_json`）─────────────── */

export type MemoryEntry = {
  id: string
  title: string
  type: string
  typeLabel: string
  layer: string
  project: string | null
  agent: string | null
  device?: string | null
  session?: string | null
  tags: string[]
  importance: number
  files: string[]
  supersedes: string[]
  supersededBy: string | null
  pinned: boolean
  starred: boolean
  createdAt: number
  updatedAt: number
  validFrom: number
  validTo: number | null
  deletedAt: number | null
  dedupStatus?: string
  evidence: string[]
  hash?: string
  accessCount?: number
  lastHitAt?: number | null
  summary: string
  summaryText?: string
  chars: number
  path: string
  body?: string
  truncated?: boolean
  /**
   * 这条是自动抽取的草稿（`layer==l1 && agent=='auto-extract'`），尚未被确认成结论。
   * 只在**显式** `memoryGet(id)` 时出现 —— 检索默认不返回草稿，相关记忆也不返回。
   */
  draft?: boolean
  /** 与 `draft` 配套的人话解释，给模型读的：草稿可以当线索，不能当既定结论引用。 */
  layerCaveat?: string
  /** 被人判定为「这条错了」的时间。有值 = 不能当事实用（原文仍保留，可撤销）。 */
  refutedAt?: number | null
  /** 为什么错（人写的一句话，可能为空） */
  refutedReason?: string
  /** 纠错后接替它的条目 id */
  refutedBy?: string | null
  related?: { id: string; title: string; type: string; layer?: string; line: string }[]
}

export type MemoryList = {
  items: MemoryEntry[]
  lines: string[]
  /** 命中总数（**截断前**）。页数按它算，不要用 items.length */
  total: number
  /** 本次请求的跳过条数，回显用 */
  offset?: number
  fts: boolean
}

/* ─── 项目（memory::projects）──────────────────────────────── */

export type MemoryProject = {
  slug: string
  name: string
  remote: string | null
  remotes?: string[]
  path: string | null
  mergedInto?: string | null
  pinned?: boolean
  archived?: boolean
  count: number
  l1: number
  l2: number
  updatedAt: number
  updatedText: string
  /** 写入过本项目记忆的 Agent（去重，按最近写入优先） */
  agents: string[]
  latest: { id: string; title: string; type: string; layer: string }[]
}

/** 通用项目汇总（没有归属项目的记忆单独统计，不进项目列表） */
export type MemoryGeneral = {
  slug: string
  count: number
  l1: number
  l2: number
  latest: number
  agents: string[]
}

export type MemoryKindOption = { value: string; label: string; prefersL2?: boolean }

/* ─── 模块设置（memory::MemoryConfig）────────────────────────── */

export type MemoryConfig = {
  /** 记忆服务总开关。关掉后读写类工具一律拒绝（自检仍可用）。 */
  enabled: boolean
  /** `memory_core` 的默认 token 预算 */
  coreMaxTokens: number
}

/* ─── 自检（memory::status）────────────────────────────────── */

export type MemoryStatus = {
  root: string
  indexFile: string
  device: string
  total: number
  live: number
  l1: number
  l2: number
  /**
   * `l1` 的子集：自动抽取写下、还没攒够强化次数的候选（草稿）。
   *
   * 后端把 `l1` 与它分开数，是因为两者语义不同：`l1` 里剩的是用户自己的流水，
   * 候选不进检索、不进注入、不进核心记忆。仪表盘上若混在一起显示，用户会以为
   * 那些草稿已经是系统认下的记忆。
   */
  candidates: number
  /** 被判定为错的条数，不计入有效记忆 */
  refuted: number
  /**
   * 真正能当事实用的条数：活着、没被反驳、也不是自动抽取草稿。
   *
   * 后端直接给出这个数而不是让前端做 `live - candidates - refuted` —— 那些桶之间
   * 有交集（一条被反驳的草稿同时算草稿和被反驳），在前端相减会重复扣减。
   */
  trusted: number
  deleted: number
  superseded: number
  /** 本地自然日（零点起）新增的条数，只算 live。 */
  today: number
  /** 昨天的同口径条数，用来给「今日新增」一个参照。 */
  yesterday: number
  /**
   * 「已连通 Agent」的分子分母。
   *
   * 分母 `injected` = 配置文件里真有 `agent2api-memory` 这一条的客户端数；
   * 分子 `called` = 其中心跳 `calls > 0`（真的调过记忆工具）的数量。
   * 口径与接入页卡片同一套，见后端 `agents::connectivity`。
   */
  agents: { called: number; injected: number }
  projects: number
  review: { pending: number | null; warnThreshold: number }
  /**
   * `count` / `inSync` 可能是 `null`：索引被重建或打开时压缩独占着那条全局
   * 连接，后端**不会排队**（否则整页会卡到网关 60 秒超时），而是给 `null`。
   * 前端必须把它渲染成「整理中」而不是 0 —— 0 会被读成「索引空了」。
   */
  index: { count: number | null; fts: boolean; inSync: boolean | null }
  /**
   * 请求级注入的「真的在跑吗」证据：本次启动以来的注入次数、累计 token、
   * 平均每次 token，以及开关状态。注入是用户看不见的动作，没有这三个数
   * 就只能靠模型行为去猜有没有生效。
   */
  inject: {
    count: number
    tokens: number
    avgTokens: number
    enabled: boolean
    /** 走过注入逻辑的请求数（`count` 只是真正注入了的那部分） */
    attempts?: number
    /** 走了注入但没命中任何记忆的次数 */
    missed?: number
  }
  /**
   * 请求后留存的「写回来没有」证据：本次启动以来写回 l1 的轮数、跳过的轮数。
   *
   * 与 `inject` 成对：一个证明「记忆读进去了」，一个证明「新记忆写回来了」。
   * 只有前者时用户的感觉是「它记得老东西，但新东西永远不进来」。
   *
   * `skipped` 的含义是「走了留存逻辑但这一轮没有落库」，不是「出错」——
   * 逐轮写会把库灌成流水账，跳过才是常态。
   *
   * `skippedBy` 把跳过拆成原因。真机事故：仪表盘显示 `count=0 / skipped=4`，
   * 而同期 89 个请求里 82 个带工具调用 —— 只报总数会让「工具轮被正常跳过」
   * 读成「留存坏了」。分项之和恒等于 `skipped`。
   */
  retain?: {
    enabled: boolean
    count: number
    skipped: number
    skippedBy?: {
      toolTurn?: number
      tooShort?: number
      gate?: number
      duplicate?: number
      noUser?: number
      disabled?: number
    }
  }
  tables?: Record<string, number>
  config: MemoryConfig
  revision: number
}

export type MemoryTimelineItem = {
  id: string
  title: string
  type: string
  typeLabel: string
  layer: string
  /** 这条链上节点是自动抽取草稿（还没被确认成结论）。链里会显示「草稿」标记。 */
  draft?: boolean
  validFrom: number
  validTo: number | null
  updatedAt: number
  validFromText: string
  relative: string
  supersededBy: string | null
  current: boolean
}

export type ListParams = {
  query?: string
  project?: string
  agent?: string
  type?: string
  layer?: string
  tag?: string
  starred?: boolean
  pinned?: boolean
  includeSuperseded?: boolean
  includeDeleted?: boolean
  includeExpired?: boolean
  /**
   * 是否包含「未升级的抽取候选」。
   *
   * auto-extract 的产物先落 l1 当草稿，攒够强化次数才升 l2。默认（不传）
   * 后端会把草稿过滤掉——检索、注入、核心记忆都只认沉淀后的结论。
   * 只有浏览页 / 待确认队列这种「要看草稿本身」的界面才显式传 true。
   */
  includeCandidates?: boolean
  /**
   * 是否包含被人判定为「错了」的条目。默认（不传）后端不返回。
   * 只有浏览页这种「要看得见、才能撤销」的界面才显式传 true。
   */
  includeRefuted?: boolean
  limit?: number
  /** 跳过前 N 条。分页用：`offset = page * pageSize` */
  offset?: number
  /** 写入时间下界（毫秒，含）。热力图点某天 → 拉那一天的清单 */
  from?: number
  /** 写入时间上界（毫秒，不含） */
  to?: number
}

/** 热力图：每日写入量（`memory::heat`） */
export type MemoryHeat = {
  items: { day: string; count: number }[]
  days: number
  total: number
  activeDays: number
  /** 同期自动抽取写下的草稿条数。只画正式记忆，草稿不进格子。 */
  candidates?: number
  /** 今天（`YYYY-MM-DD`），前端据此对齐网格右端 */
  end: string
}

/* ─── 配置 schema（config::schema_json）────────────────────── */

export type KnobKind = 'number' | 'boolean' | 'string' | 'path' | 'secret' | 'enum' | 'multiselect' | 'list' | 'map'
export type KnobTier = 'basic' | 'advanced' | 'internal'

export type Knob = {
  key: string
  kind: KnobKind
  tier: KnobTier
  group: string
  label: string
  desc: string
  default: unknown
  min?: number
  max?: number
  options?: string[]
  hot?: boolean
  /**
   * 这个旋钮在**本仓**有没有真实读取点。
   *
   * `false` = 值照样落盘、照样能导入导出（跨实现共用同一份 `config.json`），
   * 但本仓没有任何运行时代码读它 —— 改了不会有任何行为变化。设置页因此把它
   * 置灰并标「本仓未接入」，而不是让人调完发现没用、再回来怀疑是自己没保存。
   *
   * 后端每条 `wired=false` 的说明里都必须写清楚「为什么没有」和「等价操作是
   * 什么」，由单测 `unwired_knobs_explain_themselves` 钉住。
   */
  wired?: boolean
  /**
   * 只有 `secret` 类有：库里**有没有值**。
   *
   * 秘密字段下发的 `value` 恒为空串（后端 `flat()` 脱敏），所以「空输入框」
   * 有两种含义 —— 没配过、配过但没回显。这个字段是唯一的区分依据。
   */
  set?: boolean
}

export type ConfigPayload = {
  config: MemoryConfig
  values: Record<string, unknown>
  /** 整份 schema：**数组**，按 `group` 分组由前端自己做 */
  schema: Knob[]
  status: MemoryStatus
  kinds: MemoryKindOption[]
  layers: { value: string; label: string }[]
}

/* ─── 待确认队列（index::ReviewRow::to_json）────────────────── */

export type ReviewItem = {
  id: number
  type: string
  target: string
  payload: Record<string, unknown>
  confidence: number
  status: string
  createdAt: number
  resolvedAt: number | null
  resolution: string | null
  createdText?: string
}

/* ─── 自动化（schedule::status）────────────────────────────── */

/** 任务当前生效的节奏。三种模式互斥，字段由后端按配置填。 */
export type AutoCadence =
  | { mode: 'interval'; intervalMin: number; text: string }
  | { mode: 'daily'; daily: string; text: string }
  | { mode: 'weekly'; weekly: number; weeklyTime: string; text: string }

export type AutoTask = {
  task: string
  enabled: boolean
  intervalMin: number
  lastRun: number | null
  lastRunText: string | null
  nextRun: number
  /** 「下次」的人话：按配置的 daily / weeklyTime 推，与 `lastRun + interval` 不同。 */
  nextRunText: string
  /** 累计执行次数（含跳过 / 失败）。 */
  runs: number
  /** 成功次数（status = done）。 */
  success: number
  /**
   * 跳过次数（status = skipped）：没素材 / 待确认积压 / 记忆中枢关着。
   *
   * 成功率的分母要把它减掉 —— 跳过不是失败。早先分母直接用 `runs`，
   * 于是一个每小时都正常跳过的任务会长期显示「成功率 0%」，看起来像坏了。
   */
  skipped: number
  /** 累计消耗的 token（零成本任务恒为 0）。 */
  tokens: number
  /** 这个任务会不会调模型 —— 界面据此标「零成本 / 调用模型」。 */
  usesModel: boolean
  /** 一句「它在干什么」，静态文案。空串 = 没有可说的（界面画 —）。 */
  summary: string
  /** 节奏：间隔 / 每天 / 每周。界面那句「调度节奏：…」直接用它。 */
  cadence: AutoCadence
}

/**
 * 执行记录表的两个档位：默认折叠 10 条、展开 50 条。
 *
 * ── 为什么这两个数要写在这里 ────────────────────────────────
 * 折叠档由后端 `schedule::PREVIEW_RUNS` 决定（`/auto` 的 `recentRuns` 就只有
 * 10 条），前端不需要知道；但**展开档**是前端主动请求的 `limit`，写死在调用处
 * 容易和后端 `EXPANDED_RUNS` 漂移 —— 于是这里留一份，改后端时两处一起改。
 */
export const RUNS_EXPANDED = 50

/** 一次任务执行（`task_run` 一行）。 */
export type TaskRun = {
  at: number
  atText: string
  task: string
  status: 'done' | 'skipped' | 'failed'
  detail: string
  tokens: number
  latencyMs: number
  manual: boolean
  /** 这一行代表「连续 N 次同样的结果」。1 = 普通一行。 */
  repeat: number
}

export type AutoStatus = {
  running: boolean
  lastTick: number
  globalEnabled: boolean
  privacyPaused: boolean
  dailyTokenLimit: number
  tokensToday: number
  budgetExhausted: boolean
  /** `pause` = 超预算挂起，`ignore` = 照跑。 */
  overBudgetAction: string
  /** 待裁决建议条数（`review_queue` 里 pending 的行）。 */
  pendingReview: number
  labels: { task: string; label: string }[]
  tasks: AutoTask[]
  /** 最近 10 条执行记录（预览；展开走 loadAutoRuns）。 */
  recentRuns: TaskRun[]
  /** 记录**行数**（展开按钮上的 N 与脚注里的「共 N 行」）。 */
  runRows: number
  /** 执行**次数**（合并后一行可代表多次，与任务行「累计」同口径）。 */
  runCount: number
}

export type LlmCall = {
  at: number
  atText: string
  task: string
  model: string
  provider: string
  promptTokens: number
  completionTokens: number
  latencyMs: number
  ok: boolean
  error: string | null
}

export type Outcome = {
  status: 'done' | 'skipped' | 'failed'
  result?: unknown
  reason?: string
  error?: string
}

/* ─── 画像（memory::profile_view）──────────────────────────── */

/**
 * 画像条目：`MemoryEntry` + 后端算好的两个展示字段。
 *
 * `sub` 是**归一之后**的四类 key（`identity` / `communication` / `technical` /
 * `workstyle`）。后端会把旧 key（`preference` / `habit` / `expertise` /
 * `context`）映射到新 key 再回填，所以页面永远只见到这四个 —— 「单独生成」
 * 直接把 `sub` 发回去即可，不需要前端再认一套历史拼写。
 */
export type ProfileItem = MemoryEntry & { evidenceCount?: number; sub?: string }

export type ProfileGroup = {
  sub: string
  label: string
  /** 这一类在讲什么（分组卡与面板标题下的一行小字） */
  hint: string
  items: ProfileItem[]
}

export type ProfileView = {
  groups: ProfileGroup[]
  /** 画像条目数（四类之和） */
  count: number
  /** 库里活着的记忆条数（门槛判据） */
  total: number
  /** 真正会喂给模型的素材条数（排序后截到 deep.batchSize） */
  material: number
  /** 手动置顶（重生成时保留）的条数 */
  pinned: number
  minMemories: number
  ready: boolean
  freshSinceProfile: number
  generatedAt: number | null
  generatedText: string | null
  enabled: boolean
  evidenceRequired: boolean
  /** 事实源位置：画像落在本地 `.md`，重启 / 断网都不丢 */
  storage?: { root: string; persistent: boolean }
  /** WebDAV 同步状态：顶部「持久缓存与 WebDAV 同步保护中」那枚徽标的判据 */
  sync?: {
    configured: boolean
    enabled: boolean
    provider: string
    lastSyncAt: number | null
  }
}

/* ─── 接入（agents::list）──────────────────────────────────── */

export type AgentCard = {
  id: string
  name: string
  vendor?: string
  hue?: number
  letter?: string
  icon?: string
  iconFull?: boolean
  detected: boolean
  version?: string | null
  enabled: boolean
  configPath?: string | null
  configExists: boolean
  /** 该客户端配置文件的形态：`json` / `jsonc` / `json+env` / `toml` / `yaml`。
   *  「手动接入片段」按它决定给 JSON 还是 TOML。 */
  format?: string
  supported: boolean
  note?: string | null
  /** 这一条配置里的 `X-Agent`（决定 `l1/<agent>/` 与心跳归属）。
   *  `null` = 老版本安装器写的条目，能连但记忆会落进共用的 `mcp` 目录。 */
  mcp: { installed: boolean; agentHeader?: string | null; serverName: string }
  instruction: { path?: string | null; exists: boolean; injected: boolean; broken: boolean } | null
  heartbeat: {
    agent: string
    lastSeen: number
    lastSeenText: string
    calls: number
    lastTool?: string | null
    version?: string | null
    ok: boolean
    alive: boolean
    /** 写类工具次数（升级后才开始分类，可能小于 `calls`） */
    writes?: number
    /** 读 / 检索类工具次数 */
    reads?: number
    /** 失败次数（失败优先计数，不重复计入 reads） */
    errors?: number
  } | null
}

export type AgentList = {
  cards: AgentCard[]
  custom: AgentCustom[]
  serverName: string
  endpoint: string
  injectAgentsMd: boolean
  tools: number
}

/**
 * 自定义 Agent（用户手填的扩展位）。
 *
 * 没有资源写入器 —— 我们不知道它家配置的合并语义，所以它不参与「一键注入」，
 * 只提供片段让用户自己贴。`installed` 是「配置文件里出现过我们的服务名」，
 * 文本级判断，够回答「我到底贴进去了没有」。
 */
export type AgentCustom = {
  id: string
  name: string
  custom: true
  configPath: string
  configExists: boolean
  /** `json` | `toml`：条目写在 `mcpServers` 还是 `[mcp_servers]` */
  format: string
  instructionPath: string
  instructionExists: boolean
  installed: boolean
  detected: boolean
}

export type InstallResult = {
  id: string
  label: string
  ok: boolean
  mcp: string | null
  instruction: string | null
  error: string | null
}

export type AgentSnippet = {
  serverName: string
  url: string
  json: unknown
  toml: string
  /** Claude Code 的 `claude mcp add` 命令。其它客户端 CLI 语法各家不同，不猜。 */
  commandLine?: string
  commandLineFor?: string
  instruction: string
}

/* ─── 同步（sync::status / devices / conflicts）─────────────── */

export type SyncStatus = {
  /**
   * 「配好了没」——判据是**账号非空**，不是地址非空。
   *
   * 地址有默认值（坚果云）之后恒有值，从没动过配置的机器也有地址；如果还按
   * 「地址非空」判断，第一次打开同步页就会显示「已配置」，而实际连一次都跑不成。
   */
  configured: boolean
  /** 生效地址（用户填的，或默认值；保留用户写的尾斜杠形态） */
  url: string
  /**
   * 这串地址是不是我们预填的默认值（`sync.url` 没填过）。
   *
   * 没有它，界面分不出「用户填的坚果云」和「从没动过、我们预填的坚果云」——
   * 两种情况下输入框里是同一串字符。有它就能给地址标一句「默认值」。
   */
  urlDefault?: boolean
  remoteDir: string
  /**
   * 存储类型。本仓只实现了 `webdav`；`s3` 是**存得下、跑不起来**的配置
   * （`sync.provider` 可以选，但打包与列目录都还是 WebDAV 那套）。界面据此
   * 把 S3 画成「未实现」，不给它一个假的「立即同步」。
   */
  provider?: string
  /** 远端完整地址（地址 + 目录）；未配置时为空串 */
  remoteUrl?: string
  /** WebDAV 账号（不是秘密，照实回显） */
  username?: string
  /** 有没有存过 WebDAV 密码（密码本身永不下发） */
  hasPassword?: boolean
  /** 有没有单独配过号池包口令（留空 = 沿用 WebDAV 密码） */
  hasConfirmPhrase?: boolean
  /** 存储预设 id（只用来填默认值，不改变同步行为） */
  preset?: string
  device: string
  deviceName: string
  /**
   * 系统电脑名（Windows 上是 `COMPUTERNAME`）。
   *
   * 「这台机器叫什么」系统本来就知道，让用户手打一遍只会多出一个打错的机器名。
   * `deviceName` 没填过时用它当默认值；取不到时是空串（不是 `"unknown"` ——
   * 那个词会被当成真实的机器名写进配置、再传到别的设备的列表里）。
   */
  hostName?: string
  enabled: boolean
  auto: boolean
  intervalMin: number
  packSizeLimitMB: number
  excludeIndex: boolean
  conflicts: number
  skipped: number
  baseFiles: number
  localFiles: number
  lastSyncAt: number
  lastResult: unknown
  package: string
  /** 本机记忆根目录（同步的就是这个目录下的 general / projects / profile） */
  localDir?: string
  /** 五个模块各自的远端目录，以及本仓有没有实现它的同步 */
  modules?: SyncModuleDir[]
  devices?: {
    devices: {
      package: string
      /** 设备 id（包名去掉 `.tar.gz`）—— 身份，不随改名而变 */
      device: string
      /** 人看得懂的名字（别的设备报上来的 `sync.deviceName`）；空串 = 还没读到过 */
      name?: string
      self: boolean
      lastSyncAt?: number
    }[]
    configured: boolean
  }
  skippedPaths?: { paths: string[] }
}

/**
 * 模块目录（`sync::MODULES`）。
 *
 * `id` 是稳定的机器名，`dir` 是生效值（可能为空串 = 该模块不参与同步），
 * `implemented` 是本仓**真的实现了同步**的那一个 —— 五个里只有 `memory`。
 */
export type SyncModuleDir = {
  id: string
  label: string
  key: string
  dir: string
  /** 默认目录名（如 `/agent-skills`）：界面用它写「（默认 …）」并在改过后提示怎么还原 */
  def: string
  implemented: boolean
}

export type SyncConflict = {
  id: number
  path: string
  device: string
  /**
   * 本地 / 远端 / 基线三侧的哈希。
   *
   * `null` 不是「算不出来」，而是**这一侧没有这个文件**（删了）。界面上必须能把
   * 它显示成「已删除」—— 「本地删了、远端改了」和「两边都改了」需要的判断完全
   * 不同，拿一个空哈希糊过去等于让用户在不知道发生了什么的情况下选边。
   */
  localHash: string | null
  remoteHash: string | null
  baseHash: string | null
  /** 远端那一侧的体积（字节）。裁决「保留本地」时基线要记远端那一版，用它。 */
  remoteSize?: number
  detail: string | null
  status: string
  createdAt: number
}

export type SyncLogRow = {
  id: number
  at: number
  device: string | null
  action: string
  detail: string | null
  ok: boolean
}

/* ─── 体检（memory::health）────────────────────────────────── */

export type HealthStale = {
  id: string
  title: string
  type: string
  layer: string
  updatedAt: number
  updatedText: string
  files: { file: string; changedAt: number; changedText: string }[]
}

export type HealthView = {
  stale: HealthStale[]
  staleCount: number
  review: { pending: number; warnThreshold: number; backlogged: boolean }
  index: { count: number; inSync: boolean; fts: boolean }
  note: string
}

/* ─── 导入（import::view / preview / run）──────────────────── */

export type ImportSource = {
  path: string
  label: string
  format: string
  enabled: boolean
  priority: number
  exists: boolean
  /** 匹配到的文件总数 */
  files: number
  /** 本次实际会读的条数（单个来源有上限） */
  take?: number
  config: string
}

export type ImportPreviewRow = {
  path: string
  title: string
  project: string | null
  /** 这条来自哪个来源目录（多来源时用来分辨） */
  source?: string
  chars: number
  preview: string
  reason?: string
}

export type ImportView = {
  sources: ImportSource[]
  lastRun: ImportSummary | null
  /** 自动增量入库总闸（`import.autoIngest`）。关掉后只剩手动导入。 */
  autoIngest: boolean
  /** 首次自动入库回溯天数（没有水位时用） */
  autoSinceDays: number
  /** 增量水位：下次从哪个时刻之后的文件读起 */
  watermark: ImportWatermark
  /** 上一次自动增量入库的结果（没跑过是 null） */
  lastAuto: ImportSummary | null
  scanned: number
  /** 本次会读的文件数（单个来源有上限，`scanned` 是总数） */
  take?: number
  dryRunFirst: boolean
  sensitiveSkip: boolean
  batchSize: number
  note: string
}

export type ImportWatermark = {
  at: number
  text: string
}

export type ImportPreview = {
  sources: ImportSource[]
  importable: ImportPreviewRow[]
  skipped: ImportPreviewRow[]
  /** 真正会写入的条数（不受列表截断影响） */
  ready: number
  /** `importable` 里实际列出的条数；小于 `ready` 说明还有没显示出来的 */
  listed?: number
  skippedCount: number
  duplicates: number
  scanned: number
  dryRunFirst: boolean
  sensitiveSkip: boolean
  batchSize: number
}

export type ImportSummary = {
  written: number
  duplicated: number
  skipped: number
  /** 增量模式下载「文件上次已读过、这次没变」的条数；手动导入恒为 0 */
  unchanged?: number
  /**
   * 有变动但超出单来源上限、这轮没轮到的文件数。
   *
   * 大于 0 时后端**故意不推进水位** —— 否则这批文件会被永久标成「已读」，
   * 表现为「某些项目的记忆从来没出现过」，且没有任何报错。
   */
  deferred?: number
  failed: number
  scanned: number
  reasons: string[]
  at: number
  atText: string
}

/* ─── 导入来源清册（import_registry::view）─────────────────── */

/**
 * 一个来源的探测结果。
 *
 * `kind` 决定「怎么读」，页面据此决定「格式」那一行显示什么：
 *   · `jsonl`       —— 会话日志，准实时
 *   · `md`          —— Markdown 笔记目录
 *   · `sqlite`      —— 明文会话库，权威源
 *   · `trae`        —— SQLCipher 加密库（需要运行时才能读）
 *   · `antigravity` —— 加密 bin 日志
 *   · `dsh`         —— zstd 压缩的 JSONL
 */
export type ImportSourceKind = 'jsonl' | 'md' | 'sqlite' | 'trae' | 'antigravity' | 'dsh'

export type ImportRegistryRow = {
  id: string
  name: string
  kind: ImportSourceKind | string
  /** 已脱敏（主目录换成 `~`）、正斜杠形态；空串表示「未指定」 */
  path: string
  /** 有没有指定过路径。`notes-md` 这类来源天生没有候选路径，状态要画成「未指定」 */
  hasPath: boolean
  exists: boolean
  /** 用户主动关掉的（`import.ignored`） */
  ignored: boolean
  /**
   * 找到、没被忽略、且已写进 `import.sources` —— 也就是「点了导入真的会读」。
   *
   * 这三个条件是叠加的，不是同义的：路径在盘上但没进清单时 `exists` 为真、
   * `enabled` 为假，此时卡片要显示「未启用」而不是「已找到」，否则用户点
   * 「导入该来源」只会看到「扫描 0 个文件」。
   */
  enabled: boolean
  /** 路径是用户手填的（`import.overrides`） */
  custom: boolean
  priority: number
  /** 条目数（文件数 / sqlite 会话数 / dsh 的 session 目录数）；加密源恒为 0 */
  items: number
  bytes: number
  bytesText: string
  /** 「已读到 id=2890，本次按增量续读」/「首次导入，将全量扫描」 */
  estimate: string
  /** 读法补充（加密库、zstd 这类要提前说） */
  note: string
  about: string
  candidates: string[]
}

export type ImportRegistry = {
  sources: ImportRegistryRow[]
  /** 已找到的来源数 */
  found: number
  total: number
  bytes: number
  bytesText: string
}

/** 全库去重巡检的结果。 */
export type DedupScan = {
  skipped: boolean
  reason?: string
  scanned: number
  exact: number
  near: number
  queued: number
  pending?: number
}

/* ─── 请求 ────────────────────────────────────────────────── */

function query(params: Record<string, string | number | boolean | undefined>): string {
  const search = new URLSearchParams()
  for (const [key, value] of Object.entries(params)) {
    if (value === undefined || value === '' || value === false) continue
    search.set(key, String(value))
  }
  const text = search.toString()
  return text ? `?${text}` : ''
}

/* ─── 读 ──────────────────────────────────────────────────── */

export function loadConfig(): Promise<ConfigPayload> {
  return api('GET', '/api/memory/config')
}

export function saveConfig(patch: Record<string, unknown>): Promise<{ config: MemoryConfig; values: Record<string, unknown>; status: MemoryStatus }> {
  return api('PUT', '/api/memory/config', { values: patch })
}

export function resetConfig(keys?: string[]): Promise<{ config: MemoryConfig; values: Record<string, unknown> }> {
  return api('POST', `/api/memory/config/reset${query({ keys: keys?.join(',') })}`)
}

export function loadList(params: ListParams): Promise<MemoryList> {
  return api('GET', `/api/memory/list${query({
    ...params,
    // 后端只认 `includeRefuted=1`。照 `includeCandidates: true` 的写法由调用方
    // 传布尔值，这里统一转成后端约定的 '1'（query 会跳过 false / undefined）。
    includeRefuted: params.includeRefuted ? '1' : undefined,
  } as Record<string, string | number | boolean | undefined>)}`)
}

/** 每日写入量：浏览页热力图视图的数据源。
 *
 * `tz` 必须是「本地时间比 UTC 快多少分钟」：`-new Date().getTimezoneOffset()`。
 * 后端按它切天，热力图的「今天」才跟用户日历一致（东八区 = 480）。
 */
export function loadHeat(days = 365): Promise<MemoryHeat> {
  const tz = -new Date().getTimezoneOffset()
  return api('GET', `/api/memory/heat${query({ days, tz })}`)
}

export function loadEntry(id: string): Promise<{ items: MemoryEntry[] }> {
  return api('GET', `/api/memory/get${query({ ids: id })}`)
}

export function loadCore(project?: string, maxTokens?: number): Promise<{ text: string; tokens: number; truncated: boolean }> {
  return api('GET', `/api/memory/core${query({ project, maxTokens })}`)
}

export function loadDigest(maxLines?: number): Promise<{ text: string; lines: number }> {
  return api('GET', `/api/memory/digest${query({ maxLines })}`)
}

/**
 * 请求级注入预览：给一段「像用户会说的话」，回「网关会往里塞什么」。
 *
 * 后端 `GET /api/memory/inject/preview` 走的是和真实转发**完全同一条**函数
 * （`inject::compose`），所以这里看到的就是线上会发生的。不带 `q` 时后端只算
 * 「必须遵守」那一段 —— 用来确认「我钉的那几条到底进没进去」。
 */
export function loadInjectPreview(q?: string): Promise<InjectPreview> {
  return api('GET', `/api/memory/inject/preview${query({ q })}`)
}

export type InjectHit = {
  id: string
  title: string
  type: string
  layer: 'l1' | 'l2'
  /** 相关度得分（路径命中 3.0 / 线索命中 2.0 + 类型与重要度加成） */
  score: number
  /** 这条要过多少分才能进注入块（l2 与结论类 2.0，普通流水 3.3） */
  floor: number
  /** `score >= floor` —— 过线了；过线却不在 `text` 里说明被预算挤掉 */
  pass: boolean
  /**
   * 和前面某条是「同一件事」（同会话碎片 / 同一句摘要）。
   * `pass === true && duplicate === true` 表示：够格，但被去重折叠了 ——
   * 界面上要和「没命中」分开说，否则用户会以为门槛调错了。
   */
  duplicate: boolean
  /** 去重分组键（`junk:<会话id>` 或 `text:<摘要开头>`） */
  group: string
  /** 实际会写进注入块的那一行 —— 和 `text` 里的内容逐字一致 */
  line: string
  files: string[]
}

export type InjectPreview = {
  enabled: boolean
  settings: {
    maxTokens: number
    rulesTokens: number
    experienceTokens: number
    maxItems: number
    pathMatch: boolean
  }
  /** 从请求文本里抽出来的线索（路径 / 反引号片段 / 标识符）—— 只有 ASCII */
  probes: string[]
  /**
   * 中文线索（四字组集合的大小）。
   *
   * 中文没有词边界，塞不进 `probes`（那边靠「整串出现」判定），所以走另一套：
   * 请求按 4 字切组，记忆里有多少个**连续**命中就折多少分。
   * 中文提问时 `probes.length === 0` 但这里 `> 0` —— 两个数一起看，
   * 才知道「是真没线索」还是「有线索只是没命中」。
   */
  cjkGrams: number
  /**
   * 候选记忆及其得分。
   *
   * 这一项是「为什么没注入」的唯一解释：`pass=false` 是没命中（线索不够具体），
   * `pass=true` 但不在 `text` 里是被预算挤掉（该调大 experienceTokens）。
   */
  hits: InjectHit[]
  /** 拼出来的注入块；`null` = 这次不会注入（没有命中） */
  text: string | null
  tokens: number
}

export function loadProjects(): Promise<{ items: MemoryProject[]; general: MemoryGeneral }> {
  return api('GET', '/api/memory/projects')
}

export function loadStatus(): Promise<MemoryStatus> {
  return api('GET', '/api/memory/status')
}

export function loadTimeline(id: string): Promise<{ items: MemoryTimelineItem[] }> {
  return api('GET', `/api/memory/timeline${query({ id })}`)
}

export function loadReview(status = 'pending', type?: string, limit = 50): Promise<{ items: ReviewItem[]; total: number; pending: number }> {
  return api('GET', `/api/memory/review${query({ status, type, limit })}`)
}

export function loadAuto(): Promise<AutoStatus> {
  return api('GET', '/api/memory/auto')
}

/**
 * 任务执行审计的「展开」数据。
 *
 * `/auto` 里只带 10 条预览；展开时按 50 条取（与目标界面同口径）。这个上限
 * 不是分页，是「展开态一次显示多少行」—— 再往前翻的需求属于导出报告。
 */
export function loadAutoRuns(limit = RUNS_EXPANDED): Promise<{ items: TaskRun[]; total: number; runs: number }> {
  return api('GET', `/api/memory/auto/runs${query({ limit })}`)
}

export function loadCalls(limit = 100): Promise<{ items: LlmCall[]; tokensToday: number; dailyLimit: number }> {
  return api('GET', `/api/memory/calls${query({ limit })}`)
}

export function loadProfile(): Promise<ProfileView> {
  return api('GET', '/api/memory/profile')
}

export function loadAgents(): Promise<AgentList> {
  return api('GET', '/api/memory/agents')
}

export function loadAgentSnippet(): Promise<AgentSnippet> {
  return api('GET', '/api/memory/agents/snippet')
}

export function loadHealth(limit = 50): Promise<HealthView> {
  return api('GET', `/api/memory/health${query({ limit })}`)
}

export function loadSync(): Promise<SyncStatus> {
  return api('GET', '/api/memory/sync')
}

export function loadSyncConflicts(): Promise<{ conflicts: SyncConflict[]; count: number }> {
  return api('GET', '/api/memory/sync/conflicts')
}

export function loadSyncLog(limit = 100): Promise<{ entries: SyncLogRow[]; total: number }> {
  return api('GET', `/api/memory/sync/log${query({ limit })}`)
}

export function loadSyncPreview(): Promise<{ device: string; createdAt: number; files: { path: string; hash: string; size: number }[] }> {
  return api('GET', '/api/memory/sync/preview')
}

export function loadImport(): Promise<ImportView> {
  return api('GET', '/api/memory/import')
}

/* ─── 写 ──────────────────────────────────────────────────── */

export type WritePayload = {
  content: string
  title?: string
  type?: string
  layer?: string
  project?: string
  tags?: string[]
  importance?: number
  files?: string[]
  supersedes?: string[]
  validTo?: number
  evidence?: string[]
}

export function writeEntry(payload: WritePayload): Promise<{ id: string; duplicate?: boolean }> {
  return api('POST', '/api/memory/write', payload)
}

export function updateEntry(id: string, patch: Record<string, unknown>): Promise<{ entry: MemoryEntry }> {
  return api('POST', '/api/memory/update', { id, ...patch })
}

export function deleteEntry(id: string): Promise<{ id: string }> {
  return api('POST', '/api/memory/delete', { id })
}

export function restoreEntry(id: string): Promise<{ id: string }> {
  return api('POST', '/api/memory/restore', { id })
}

export function purgeEntry(id: string): Promise<{ id: string }> {
  return api('POST', '/api/memory/purge', { id })
}

/**
 * 人工采纳一条草稿：直接升成 l2 沉淀结论。
 *
 * 与 `updateEntry(id, { layer: 'l2' })` 不是一回事 —— 那条是通用字段改写，
 * 不清容器路径、也不带「我认了」的语义；这条是专门的正向裁决，后端会追加一个
 * `manual:<id>` 证据锚点并记审计日志。
 */
export function promoteDraft(id: string): Promise<{ id: string; promoted: boolean; layer: string }> {
  return api('POST', '/api/memory/promote', { id })
}

/**
 * 把一条记忆判为「这条错了」。
 *
 * 不是删除：条目与演化链都留着，只是从检索 / 注入 / 核心记忆 / 画像素材里退出，
 * 界面上打红标并显示理由。`reason` 是人写的一句话，`replacementId` 是纠错后
 * 接替它的那条 —— 两者都可空。撤销走 `unrefuteMemory`。
 */
export function refuteMemory(
  id: string,
  reason?: string,
  replacementId?: string,
): Promise<{ id: string; refuted: boolean }> {
  return api('POST', '/api/memory/refute', { id, reason, replacementId })
}

/** 撤销「这条错了」标记，让它重新当事实用。 */
export function unrefuteMemory(id: string): Promise<{ id: string; refuted: boolean }> {
  return api('POST', '/api/memory/unrefute', { id })
}

export function purgeTrash(days?: number): Promise<{ removed: number; keepDays: number }> {
  return api('POST', `/api/memory/trash/purge${query({ days })}`)
}

/* ─── 回收站文件清单（memory::trash_list）─────────────────────── */

/**
 * 回收站里的**一个文件**。
 *
 * 与 `MemoryEntry` 不是同一口径：这份按「文件」组织（体积、剩余天数），
 * 那份按「记忆条目」组织（项目、层级、标记）。回收站看的是「删了什么、
 * 还能恢复几天」，所以字段跟着文件走。
 */
export type TrashItem = {
  id: string
  /** 回收站里的文件名（`<id>.md`） */
  name: string
  title: string
  /** 删除前它待在哪。老版本删的条目没有这一项，后端给的是占位文案 */
  originPath: string
  /** 字节数 */
  size: number
  /** 进回收站的时刻（文件 mtime），与真正会被清掉的时刻同源 */
  trashedAt: number
  /** 到期时刻。0 = 算不出来（mtime 读不到） */
  expiresAt: number
  /** 还剩几天（向上取整）。1 表示「今天就会被清」 */
  expiresIn: number
  keepDays: number
  layer: string
  project: string | null
  agent: string | null
}

export type TrashPayload = {
  items: TrashItem[]
  count: number
  totalBytes: number
  keepDays: number
  /** 今天就会到期的条数，用于卡片上的提醒 */
  expiring: number
  root?: string
}

export function loadTrash(): Promise<TrashPayload> {
  return api('GET', '/api/memory/trash')
}

export function rebuildIndex(): Promise<{ rebuilt: number; status: MemoryStatus }> {
  return api('POST', '/api/memory/rebuild')
}

export function resolveReview(id: number, approve: boolean): Promise<{ ok: boolean }> {
  return api('POST', '/api/memory/review', { id, approve })
}

export function rejectAllReview(): Promise<{ rejected: number }> {
  return api('POST', '/api/memory/review', { rejectAll: true })
}

export function runAutoTask(task: string, project?: string): Promise<{ task: string; outcome: Outcome }> {
  return api('POST', '/api/memory/auto/run', { task, project })
}

/** 改节奏 / 开关要发的东西。`cadence` 三种形状与 `AutoCadence` 的 mode 对齐。 */
export type AutoTaskPatch = {
  /** 开关。不传 = 不改。 */
  enabled?: boolean
  /** 节奏。不传 = 不改；传了就整份替换（旧模式的字段由后端清掉）。 */
  cadence?:
    | { mode: 'interval'; intervalMin: number }
    | { mode: 'daily'; daily: string }
    | { mode: 'weekly'; weekly: number; weeklyTime: string }
}

/**
 * 改一个任务的开关 / 节奏，返回刷新后的**整份**自动化状态。
 *
 * 为什么不走 `saveConfig`：那样页面得自己拼 `auto.tasks` 的三层嵌套。
 * 拼成扁平子键（`auto.tasks.extract`）会被后端当成未知键静默丢弃 ——
 * 开关点了、刷新又弹回去，且没有任何报错；拼成深合并形状则删不掉旧节奏
 * 字段（改成「每隔 30 分钟」后 `daily` 残留，调度器仍然每天只跑一次）。
 * 两类坏状态都不会在页面上报警，所以语义交给后端收口。
 */
export function setAutoTask(task: string, patch: AutoTaskPatch): Promise<AutoStatus> {
  return api('POST', '/api/memory/auto/task', { task, ...patch })
}

/**
 * 重生成画像。`sub` 传四类 key 之一 = 只重算这一类，不传 = 四类全量。
 *
 * 为什么给「只算一类」留一个参数而不是让前端循环调四次：四次调用会把四份
 * 结论分别写进库，中间任何一次模型失败都会留下「三类是新的、一类是旧的」
 * 这种界面上看不出来的混合态 —— 而这类混合态正是用户最不能判断可信度的
 * 状态。一次调用、后端决定范围，失败就整体不变。
 */
export function buildProfile(sub?: string): Promise<{ outcome: Outcome; profile: ProfileView }> {
  return api('POST', '/api/memory/profile/build', sub ? { sub } : {})
}

/**
 * 置顶 / 取消置顶一条画像结论。
 *
 * 置顶的语义只有一条：**重新生成时保留**。它不是「排到最前面」那种一次性的
 * 视觉偏好 —— 那条用 `starred` 就够了（生成器给每条都打星）。所以这个动作
 * 走独立端点：它要顺带在条目上留一个可判定标记，后端才知道下一次重算该跳过谁。
 */
export function pinProfileItem(id: string, pinned: boolean): Promise<{ entry: MemoryEntry; profile: ProfileView }> {
  return api('POST', '/api/memory/profile/pin', { id, pinned })
}

/** 写进客户端的 key 身份（不含明文）。`picked=true` 表示前端没选、由后端挑的第一把。 */
export type AgentInstalledKey = { id: string; name: string; picked: boolean }

export function installAgents(
  clients: string[],
  apiKeyId: string,
): Promise<{ results: InstallResult[]; cards: AgentList; apiKey?: AgentInstalledKey }> {
  return api('POST', '/api/memory/agents/install', { clients, api_key_id: apiKeyId })
}

/** 网关里可用的 API Key（id / 名字 / 掩码），复用工具服务页那个端点。
 *
 *  ── 为什么复用而不是再开一个 ──────────────────────────────────
 * 「写进客户端哪把 key」这件事没有记忆中枢特有的语义 —— 工具服务页早就有一份
 * 完全相同的清单（连过滤条件都一样：只要 `is_usable` 的）。再开一个端点，两边
 * 的可用性判定迟早会分叉（比如一边算上了降级态的 key，另一边没算）。
 */
export type GatewayKey = { id: string; name: string; masked: string }

export function loadGatewayKeys(): Promise<{ keys: GatewayKey[] }> {
  return api('GET', '/api/tools/api-keys')
}

export function uninstallAgents(clients: string[]): Promise<{ results: InstallResult[]; cards: AgentList }> {
  return api('POST', '/api/memory/agents/uninstall', { clients })
}

/** 一次探活的结果。`ok` = 配置在**且**握手成功，不再是「读盘看见条目」。 */
export type AgentVerifyRow = {
  id: string
  name?: string
  ok: boolean
  mcp: boolean
  reachable?: boolean
  detail?: string
  agentHeader?: string | null
  mcpError?: string | null
}

export function verifyAgents(clients: string[]): Promise<{ rows: AgentVerifyRow[]; ok: number; total: number }> {
  return api('POST', '/api/memory/agents/verify', { clients })
}

/** 加一条自定义 Agent（同名覆盖）。返回整份 custom 列表。 */
export function addCustomAgent(
  name: string,
  configPath: string,
  format: string,
  instructionPath = '',
): Promise<AgentCustom[]> {
  return api('POST', '/api/memory/agents/custom/add', { name, configPath, format, instructionPath })
}

export function removeCustomAgent(name: string): Promise<AgentCustom[]> {
  return api('POST', '/api/memory/agents/custom/remove', { name })
}

/**
 * 测连接。
 *
 * 带 `probe` 就用它去测（配置页那三个输入框里的当前值），不带就测已保存的
 * 那份配置（同步状态卡里那个按钮）。`probe` 里缺的字段沿用已保存值；密码
 * 只在给了**非空**字符串时才覆盖 —— 空串是「没动这一栏」。
 */
export function testSync(probe?: { url?: string; username?: string; password?: string; remoteDir?: string }): Promise<{ ok: boolean; message: string; url?: string; packages?: number }> {
  return api('POST', '/api/memory/sync/test', probe ?? {})
}

/** 保存同步设置（只接受 `sync.` 开头的键；秘密字段空串 = 沿用已保存的那份）。 */
export function saveSyncSetup(patch: Record<string, unknown>): Promise<{ values: Record<string, unknown>; status: SyncStatus }> {
  return api('POST', '/api/memory/sync/setup', patch)
}

/** 清除同步配置：`sync.*` 全部恢复默认（已同步的数据与远端包不动）。 */
export function resetSyncSetup(): Promise<{ values: Record<string, unknown>; status: SyncStatus }> {
  return api('POST', '/api/memory/sync/reset')
}

export function runSync(): Promise<{ outcome: Outcome; status: SyncStatus }> {
  return api('POST', '/api/memory/sync/run')
}

export function resolveSyncConflict(id: number, resolution: 'local' | 'remote' | 'ignore'): Promise<{ ok: boolean; conflicts: { conflicts: SyncConflict[]; count: number } }> {
  return api('POST', '/api/memory/sync/conflicts', { id, resolution })
}

export function previewImport(path?: string, limit = 60): Promise<ImportPreview> {
  return api('POST', '/api/memory/import/preview', { path, limit })
}

/**
 * 上一次干跑的结果缓存。
 *
 * ── 为什么要有它 ─────────────────────────────────────────────
 * 页签是条件渲染，切走就把导入组件连同 `preview` 一起卸载，回来只剩空表，
 * 用户只能再点一次「导入全部来源」重扫全盘。干跑结果本来就有 30 分钟有效期
 * （真导入的门槛按这个 TTL 放行），所以结果留在后端，页面重挂载时接回来。
 */
export type ImportPreviewSnapshot = {
  /** 上次干跑的时间戳；0 = 本次会话还没跑过 */
  at: number
  ageMs: number | null
  /** 那次预览针对哪个来源；null = 全部来源 */
  only: string | null
  /** 正在扫（重挂载时该等这一次，而不是再扫一遍） */
  running: boolean
  /** 是否仍在有效期内；false 时 `preview` 一定是 null */
  fresh: boolean
  ttlMs: number
  preview: ImportPreview | null
}

export function lastPreview(): Promise<ImportPreviewSnapshot> {
  return api('GET', '/api/memory/import/preview/last')
}

export function runImport(path?: string): Promise<{ summary: ImportSummary; view: ImportView }> {
  return api('POST', '/api/memory/import/run', { path })
}

/**
 * 后台导入的运行状态（`import::status`）。
 *
 * ── 为什么导入要拆成 start + status ─────────────────────────
 * 页签是条件渲染，切走就把导入组件卸载了；而一次几千条的导入必然超过桌面壳
 * 60 秒的请求超时。同步端点在这两件事上都会让用户看到「导入被中断」。
 * 换成后台任务之后，界面切回来只要再查一次状态就能接上进度与结果。
 */
export type ImportJob = {
  state: 'idle' | 'running' | 'done' | 'failed'
  running: boolean
  startedAt: number
  finishedAt: number
  only: string | null
  processed: number
  total: number
  summary: ImportSummary | null
  error: string
}

/** 启动后台导入，立刻返回（不等写完）。 */
export function startImport(path?: string): Promise<ImportJob> {
  return api('POST', '/api/memory/import/start', { path })
}

/** 查后台导入状态。不读盘，可以高频轮询。 */
export function importStatus(): Promise<ImportJob> {
  return api('GET', '/api/memory/import/status')
}

export function saveImportSources(sources: { path: string; format: string; enabled: boolean; priority: number }[]): Promise<{ view: ImportView }> {
  return api('POST', '/api/memory/import/sources', { sources })
}

/** 来源清册：会真的读盘（开 sqlite、递归数文件），比 `/import` 贵。 */
export function loadImportRegistry(): Promise<ImportRegistry> {
  return api('GET', '/api/memory/import/registry')
}

export function enableImportSource(id: string): Promise<ImportRegistry> {
  return api('POST', '/api/memory/import/registry/enable', { id })
}

export function disableImportSource(id: string): Promise<ImportRegistry> {
  return api('POST', '/api/memory/import/registry/disable', { id })
}

export function setImportSourcePath(id: string, path: string): Promise<ImportRegistry> {
  return api('POST', '/api/memory/import/registry/path', { id, path })
}

/**
 * 加一条自定义来源。走「追加」接口而不是整体替换 `import.sources` ——
 * 整体替换要求页面知道全部行，而内置来源启用后也住在同一份清单里，
 * 页面某次少算一行就会把它静默删掉。
 */
export function addImportSource(path: string, format = '', priority = 0): Promise<ImportRegistry> {
  return api('POST', '/api/memory/import/registry/add', { path, format, priority })
}

/** 全库去重巡检：只往待确认队列塞建议，不合并任何东西。 */
export function dedupScan(): Promise<DedupScan> {
  return api('POST', '/api/memory/dedup/scan')
}

/* ─── 项目卡操作 ──────────────────────────────────────────── */

export function updateProject(slug: string, patch: { name?: string; pinned?: boolean; archived?: boolean }): Promise<{ slug: string; items: MemoryProject[]; general: MemoryGeneral }> {
  return api('POST', '/api/memory/projects/update', { slug, ...patch })
}

/** 合并会逐条搬文件（改 frontmatter + 移动 .md），同步等待结果。 */
export function mergeProject(from: string, into: string): Promise<{ moved: number; items: MemoryProject[]; general: MemoryGeneral }> {
  return api('POST', '/api/memory/projects/merge', { from, into })
}

/* ─── 「MCP & Skills」页里那个接入目标的形状（记忆只展示、不在这里改）─── */

export type InstallTarget = {
  key: string
  label: string
  path?: string
  detected?: boolean
  installed: boolean
  supported: boolean
  note?: string | null
}

/**
 * 查「记忆已接入哪些客户端」。
 *
 * 复用工具服务那套 `/api/tools/install-targets`（记忆是一个**合法的服务 id**，
 * 只是不出现在「MCP & Skills」页的表格里 —— 见后端 `LISTED_SERVICE_IDS`）；
 * 安装 / 卸载的完整流程在本页的「接入」页签，这里只读状态 —— 否则记忆页会在
 * 没有任何人操作时静默变成「装了但不知道有没有接上」。
 */
export function loadInstallTargets(): Promise<{ targets: InstallTarget[] }> {
  return api('GET', '/api/tools/install-targets?service=memory&protocol=mcp')
}
