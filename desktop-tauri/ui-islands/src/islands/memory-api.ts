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

export type KnobKind = 'number' | 'boolean' | 'string' | 'path' | 'enum' | 'multiselect' | 'list' | 'map'
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

export type AutoTask = {
  task: string
  enabled: boolean
  intervalMin: number
  lastRun: number | null
  lastRunText: string | null
  nextRun: number
}

export type AutoStatus = {
  running: boolean
  lastTick: number
  globalEnabled: boolean
  privacyPaused: boolean
  dailyTokenLimit: number
  tokensToday: number
  budgetExhausted: boolean
  labels: { task: string; label: string }[]
  tasks: AutoTask[]
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

export type ProfileItem = MemoryEntry & { evidenceCount?: number }

export type ProfileGroup = {
  sub: string
  label: string
  items: ProfileItem[]
}

export type ProfileView = {
  groups: ProfileGroup[]
  count: number
  total: number
  minMemories: number
  ready: boolean
  freshSinceProfile: number
  generatedAt: number | null
  generatedText: string | null
  enabled: boolean
  evidenceRequired: boolean
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
  supported: boolean
  note?: string | null
  mcp: { installed: boolean; serverName: string }
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
  } | null
}

export type AgentList = {
  cards: AgentCard[]
  custom: { id: string; name: string; custom: boolean }[]
  serverName: string
  endpoint: string
  injectAgentsMd: boolean
  tools: number
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
  instruction: string
}

/* ─── 同步（sync::status / devices / conflicts）─────────────── */

export type SyncStatus = {
  configured: boolean
  url: string
  remoteDir: string
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
  devices?: { devices: { package: string; device: string; self: boolean }[]; configured: boolean }
  skippedPaths?: { paths: string[] }
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

export function loadSyncLog(limit = 100): Promise<{ entries: SyncLogRow[] }> {
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

export function buildProfile(): Promise<{ outcome: Outcome; profile: ProfileView }> {
  return api('POST', '/api/memory/profile/build')
}

export function installAgents(clients: string[], apiKeyId: string): Promise<{ results: InstallResult[]; cards: AgentList }> {
  return api('POST', '/api/memory/agents/install', { clients, api_key_id: apiKeyId })
}

export function uninstallAgents(clients: string[]): Promise<{ results: InstallResult[]; cards: AgentList }> {
  return api('POST', '/api/memory/agents/uninstall', { clients })
}

export function verifyAgents(clients: string[]): Promise<{ rows: Record<string, unknown>[]; ok: number; total: number }> {
  return api('POST', '/api/memory/agents/verify', { clients })
}

export function testSync(): Promise<{ ok: boolean; message: string; url?: string; packages?: number }> {
  return api('POST', '/api/memory/sync/test')
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
