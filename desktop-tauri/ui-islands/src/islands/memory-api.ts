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
  related?: { id: string; title: string; type: string; layer?: string; line: string }[]
}

export type MemoryList = {
  items: MemoryEntry[]
  lines: string[]
  total: number
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
  latest: { id: string; title: string; type: string; layer: string }[]
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
  deleted: number
  superseded: number
  projects: number
  review: { pending: number; warnThreshold: number }
  index: { count: number; fts: boolean; inSync: boolean }
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
  type?: string
  layer?: string
  tag?: string
  starred?: boolean
  pinned?: boolean
  includeSuperseded?: boolean
  includeDeleted?: boolean
  includeExpired?: boolean
  limit?: number
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
  configured: boolean
  url: string
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
    devices: { package: string; device: string; self: boolean; lastSyncAt?: number }[]
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
  localHash: string | null
  remoteHash: string | null
  baseHash: string | null
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
  scanned: number
  /** 本次会读的文件数（单个来源有上限，`scanned` 是总数） */
  take?: number
  dryRunFirst: boolean
  sensitiveSkip: boolean
  batchSize: number
  note: string
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
  return api('GET', `/api/memory/list${query(params as Record<string, string | number | boolean | undefined>)}`)
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

export function loadProjects(): Promise<{ items: MemoryProject[] }> {
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

export function purgeTrash(days?: number): Promise<{ purged: number }> {
  return api('POST', `/api/memory/trash/purge${query({ days })}`)
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

export function runImport(path?: string): Promise<{ summary: ImportSummary; view: ImportView }> {
  return api('POST', '/api/memory/import/run', { path })
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

export function updateProject(slug: string, patch: { name?: string; pinned?: boolean; archived?: boolean }): Promise<{ slug: string; items: MemoryProject[] }> {
  return api('POST', '/api/memory/projects/update', { slug, ...patch })
}

/** 合并会逐条搬文件（改 frontmatter + 移动 .md），同步等待结果。 */
export function mergeProject(from: string, into: string): Promise<{ moved: number; items: MemoryProject[] }> {
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
 * 复用工具服务那套 `/api/tools/install-targets`（记忆在 `SERVICE_IDS` 里是
 * 第四个服务）：安装/卸载的完整流程在「MCP & Skills」页，这里只读状态 ——
 * 否则记忆页会在没有任何人操作时静默变成「装了但不知道有没有接上」。
 */
export function loadInstallTargets(): Promise<{ targets: InstallTarget[] }> {
  return api('GET', '/api/tools/install-targets?service=memory&protocol=mcp')
}
