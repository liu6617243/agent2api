/**
 * 技能仓库 · 数据层（接口调用与类型）。
 *
 * ── 为什么单独一层 ──────────────────────────────────────────
 * 五个视图共用同一批数据（仪表盘 / 技能库 / 同步 / 去重 / 配置），把 `api()`
 * 与字段名散在组件里，改一个字段要翻五处。这里收口：**接口形状只在这个文件里
 * 出现一次**，组件只认类型。
 *
 * ── 类型为什么必须与 Rust 那边逐字对齐 ──────────────────────
 * 这些类型是手写的（没有生成器），所以它们是一份**需要人工维护的契约**。
 * 每个形状后面标了它来自哪个 Rust 函数（`core::skills_hub::*`），改后端时
 * 按注释去对一眼，比在浏览器里猜「为什么这个字段是 undefined」快得多。
 *
 * ── 请求走哪个通道 ──────────────────────────────────────────
 * 与记忆中枢一致：桌面壳的 `api_request` command（`api()` 封装见
 * `plugins-shared.ts`）。网页端由壳自己带面板会话，浏览器直连时由网关的
 * protected 中间件挡 —— 这个模块的端点全部要求鉴权，它会动用户的磁盘。
 */

import { api } from './plugins-shared'

/* ─── 通用 ──────────────────────────────────────────────────── */

/** 体检结论：`bad` 必须处理（缺 SKILL.md / 缺 description），`warn` 可以不管。 */
export type Health = { level: string; text: string }

/** 一条挂载记录（台账 `skills.<name>.mounts[]`，经 `mount::verify` 补上实检字段）。 */
export type MountRecord = {
  tool: string
  name: string
  path: string
  type: string
  enabled: boolean
  /** 链接存在 且 指向中央真身 且 中央真身还在 */
  valid?: boolean
  linkExists?: boolean
  pointsCentral?: boolean
  centralExists?: boolean
}

/** 一条来源：这个技能是从哪个渠道的哪个目录收进来的（`store::merge_sources`）。 */
export type SkillSource = {
  tool: string
  name?: string
  originalName?: string
  dir?: string
  firstSeen?: string
}

/* ─── 总览（`skills_hub::overview`）──────────────────────────── */

export type ToolRow = {
  id: string
  name: string
  builtin: boolean
  enabled: boolean
  dir: string | null
  candidatePaths: string[]
  skillCount: number
  mountCount: number
}

export type Overview = {
  hubDir: string
  skillsDir: string
  /** 台账里登记了几个技能 */
  manifestCount: number
  /** 中央真身目录里有几个（含未登记的） */
  skillCount: number
  /** 累计来源条数：这个数字与 `skillCount` 的差就是「份消除」 */
  sourceCount: number
  /** 本轮扫描里被 L1 合并掉的冗余副本数 */
  l1Merged: number
  brokenMounts: number
  mountHealth: (MountRecord & { skill: string })[]
  pendingConflicts: ConflictItem[]
  orphans: OrphanItem[]
  hubExtra: HubExtraItem[]
  tools: ToolRow[]
  recentReports: ReportRow[]
  counts: PlanCounts
  mode: string
  trash: number
}

export type PlanCounts = {
  import?: number
  replace?: number
  publish?: number
  skip?: number
  conflict?: number
  error?: number
}

/* ─── 技能库（`skills_hub::library`）────────────────────────── */

export type LibraryRow = {
  name: string
  /** 台账里有没有这一条 */
  inManifest: boolean
  /** `channel` 扫描到待收纳 / `hub` 已收纳 / `hub-extra` 中央未登记 / `system` 客户端自带 */
  origin: string
  skillName?: string | null
  description?: string
  version?: string
  treeHash?: string
  health?: Health[]
  mtimeMs?: number
  tool?: string | null
  dir?: string
  sources?: SkillSource[]
  mounts?: MountRecord[]
  isLink?: boolean
}

export type Library = {
  items: LibraryRow[]
  total: number
  counts: PlanCounts
}

/* ─── 技能详情（`skills_hub::skill`）────────────────────────── */

export type SkillDetail = {
  name: string
  manifest: Record<string, unknown> | null
  inManifest: boolean
  dir: string
  centralExists?: boolean
  tool?: string | null
  description: string
  version: string
  treeHash: string
  health: Health[]
  skillMd: string
  body: string
  children: { name: string; dir: boolean; size: number }[]
  fileCount?: number
  mounts: MountRecord[]
}

/* ─── 冲突（`skills_hub::conflicts`）────────────────────────── */

export type ConflictKind = 'content' | 'diff-link' | 'norm' | 'missing' | string

export type ConflictItem = {
  id: string
  kind: ConflictKind
  /** 技能目录名 */
  skill?: string
  /** `norm` 类是归一后的键 */
  key?: string
  toolId?: string
  dir?: string
  title: string
  detail: string
  /** `norm` 类的左右两个名字 */
  a?: string
  b?: string
  /** 现场（`conflicts()` 才补，`preview` 里没有） */
  linkPath?: string
  linkTarget?: string
  centralPath?: string
  centralExists?: boolean
  toolExists?: boolean
  toolSkillMd?: string
  centralSkillMd?: string
}

export type ConflictView = {
  items: ConflictItem[]
  duplicates: DuplicateItem[]
  hints: { a: string; b: string; sim: number }[]
  conflictCount: number
}

export type DuplicateItem = {
  kept: { name: string; tool: string }
  removed: { name: string; tool: string }
  rule: string
  basis: string
}

export type OrphanItem = { name: string; tool: string; dir: string; mtimeMs: number }
export type HubExtraItem = { name: string; path: string; isLink: boolean; size?: number }

/* ─── 预览 / 执行（`plan::plan` / `plan::execute`）──────────── */

export type PlanAction = {
  kind: string
  skill: string
  tool: string
  note: string
  mountName: string
  dir: string
  sources: string[]
}

export type Plan = {
  mode: string
  actions: PlanAction[]
  counts: PlanCounts
  conflicts: ConflictItem[]
  conflictsStored: ConflictItem[]
  orphans: OrphanItem[]
  duplicates: DuplicateItem[]
  hints: { a: string; b: string; sim: number }[]
  targets: { id: string; name: string; dir: string; custom: boolean }[]
  mountHealth: (MountRecord & { skill: string })[]
  hubExtra: HubExtraItem[]
}

export type SyncResult = {
  summary: { imported: number; mounted: number; merged: number; skipped: number; conflicts: number; cleaned: number }
  imports: { skill: string; tool: string; dir: string; outcome?: string }[]
  merges: { skill: string; action: string }[]
  mounts: { skill: string; dir: string; action: string; outcome: string; message?: string | null }[]
  conflicts: ConflictItem[]
  manifestDiff: string[]
  reportFile: string
  mode: string
}

/* ─── 报告（`store::list_reports` / `store::read_report`）───── */

export type ReportRow = { file: string; mtimeMs: number; size: number }

/* ─── 配置（`tools::load_config` / `save_config`）───────────── */

export type HubConfig = {
  /** `junction`：建目录链接；`copy`：复制一份（链接建不起来时的兜底，需手动选） */
  mountMode: 'junction' | 'copy'
  /** 回收站保留天数 */
  trashDays: number
  /** L3 文本相似度提示（默认关：0.85 在「把 X 转成 Y」这类描述上误报率高） */
  l3Enabled: boolean
  l3Threshold: number
  /** 只存用户改过的覆盖：`{ [toolId]: { enabled?, name?, paths?[] } }` */
  tools: Record<string, { enabled?: boolean; name?: string; paths?: string[] }>
  customDirs: string[]
}

/* ─── 回收站（`skills_hub::trash`）─────────────────────────── */

export type TrashRow = {
  file: string
  name: string
  path: string
  mtimeMs: number
  size: number
  ageDays: number
  daysLeft: number
}

export type TrashView = { items: TrashRow[]; trashDays: number; dir: string }

/* ─── 每日检测（`skills_hub::daily_check_status`）───────────── */

export type DailyCheck = {
  ok: boolean
  checkedAt: number
  summary: string
  toolsHit: number
  toolsTotal: number
  brokenMounts: number
  repaired: number
  conflicts: number
  orphans: number
  hubExtra: number
}

export type ProbeRow = {
  suggestId: string
  name: string
  hitDirs: string[]
  skillCount: number
  firstDir: string
}

/* ─── 调用 ──────────────────────────────────────────────────── */

const BASE = '/api/skills-hub'

export function loadOverview(): Promise<Overview> {
  return api('GET', `${BASE}/overview`)
}

export function loadLibrary(): Promise<Library> {
  return api('GET', `${BASE}/library`)
}

export function loadSkill(name: string): Promise<SkillDetail> {
  return api('GET', `${BASE}/skill?name=${encodeURIComponent(name)}`)
}

export function loadConflicts(): Promise<ConflictView> {
  return api('GET', `${BASE}/conflicts`)
}

export function loadPreview(): Promise<Plan> {
  return api('GET', `${BASE}/preview`)
}

export function runSync(): Promise<SyncResult> {
  return api('POST', `${BASE}/sync`, {})
}

export function resolveConflict(id: string, choice: string): Promise<{ resolved: boolean; message: string }> {
  return api('POST', `${BASE}/resolve`, { id, choice })
}

export function loadConfig(): Promise<{ values: HubConfig }> {
  return api('GET', `${BASE}/config`)
}

export function saveConfig(patch: Partial<HubConfig>): Promise<{ values: HubConfig }> {
  return api('PUT', `${BASE}/config`, { values: patch })
}

export function resetConfig(keys?: string[]): Promise<{ values: HubConfig }> {
  const query = keys && keys.length ? `?keys=${encodeURIComponent(keys.join(','))}` : ''
  return api('POST', `${BASE}/config/reset${query}`, {})
}

export function loadTools(): Promise<{ items: ToolRow[] }> {
  return api('GET', `${BASE}/tools`)
}

export function loadProbe(): Promise<{ items: ProbeRow[] }> {
  return api('GET', `${BASE}/probe`)
}

export function addCustomDir(path: string): Promise<{ values: HubConfig }> {
  return api('POST', `${BASE}/custom-dir`, { path })
}

export function removeCustomDir(path: string): Promise<{ values: HubConfig }> {
  return api('DELETE', `${BASE}/custom-dir`, { path })
}

export function loadDailyCheck(): Promise<{ last: DailyCheck | null }> {
  return api('GET', `${BASE}/daily-check`)
}

export function runDailyCheck(autoMount: boolean): Promise<DailyCheck> {
  return api('POST', `${BASE}/daily-check/run${autoMount ? '?autoMount=1' : ''}`, {})
}

export function repairMounts(): Promise<{ repaired: number; failed: unknown[] }> {
  return api('POST', `${BASE}/repair`, {})
}

export function loadTrash(): Promise<TrashView> {
  return api('GET', `${BASE}/trash`)
}

export function restoreTrash(file: string): Promise<{ name: string }> {
  return api('POST', `${BASE}/trash/restore`, { file })
}

export function purgeTrash(): Promise<{ removed: number }> {
  return api('POST', `${BASE}/trash/purge`, {})
}

export function adoptSkill(name: string): Promise<{ ok: boolean; action: string; name: string }> {
  return api('POST', `${BASE}/adopt`, { name })
}

export function removeSkill(name: string): Promise<{ name: string; trash: string; mounts: number }> {
  return api('POST', `${BASE}/remove`, { name })
}

export function toggleMount(name: string, tool: string, enabled: boolean): Promise<{ ok: boolean; enabled: boolean }> {
  return api('POST', `${BASE}/mount/toggle`, { name, tool, enabled })
}

export function loadReports(limit = 20): Promise<{ items: ReportRow[] }> {
  return api('GET', `${BASE}/reports?limit=${limit}`)
}

export function loadReport(file: string): Promise<{ file: string; body: string }> {
  return api('GET', `${BASE}/report?file=${encodeURIComponent(file)}`)
}