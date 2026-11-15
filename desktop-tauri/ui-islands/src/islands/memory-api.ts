/**
 * 记忆中枢 · 数据层（接口调用与类型）。
 *
 * ── 为什么单独一层 ──────────────────────────────────────────
 * 页面里有五个视图共用同一批数据（记忆库 / 项目 / 核心记忆 / 回收站 / 设置），
 * 把 `invoke` 与字段名散在组件里，改一个字段要翻五处。这里收口：**接口形状只
 * 在这个文件里出现一次**，组件只认类型。
 *
 * ── 请求走哪个通道 ──────────────────────────────────────────
 * 与插件页、账号页一致：桌面壳的 `api_request` command（`api()` 封装见
 * `plugins-shared.ts`）。网页端由壳自己带面板会话，所以这里不处理鉴权 ——
 * 浏览器直连 `http://127.0.0.1:3065` 时由网关的 protected 中间件挡。
 */

import { api } from './plugins-shared'

export type MemoryEntry = {
  id: string
  title: string
  type: string
  typeLabel: string
  project: string | null
  agent: string | null
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
  deletedAt: number | null
  summary: string
  chars: number
  path: string
  body?: string
  truncated?: boolean
  related?: { id: string; title: string; type: string; line: string }[]
}

export type MemoryList = {
  items: MemoryEntry[]
  lines: string[]
  total: number
  fts: boolean
}

export type MemoryProject = {
  slug: string
  name: string
  remote: string | null
  path: string | null
  count: number
  updatedAt: number
  updatedText: string
  latest: { id: string; title: string; type: string }[]
}

export type MemoryKindOption = { value: string; label: string }

export type MemoryConfig = {
  /** 记忆服务总开关。关掉后读写类工具一律拒绝（自检仍可用）。 */
  enabled: boolean
  /** `memory_core` 的默认 token 预算 */
  coreMaxTokens: number
}

export type MemoryStatus = {
  root: string
  indexFile: string
  total: number
  live: number
  deleted: number
  superseded: number
  projects: number
  index: { count: number; fts: boolean; inSync: boolean }
  config: MemoryConfig
}

export type MemoryTimelineItem = {
  id: string
  title: string
  type: string
  typeLabel: string
  validFrom: number
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
  tag?: string
  starred?: boolean
  pinned?: boolean
  includeSuperseded?: boolean
  includeDeleted?: boolean
  limit?: number
}

function query(params: Record<string, string | number | boolean | undefined>): string {
  const search = new URLSearchParams()
  for (const [key, value] of Object.entries(params)) {
    if (value === undefined || value === '' || value === false) continue
    search.set(key, String(value))
  }
  const text = search.toString()
  return text ? `?${text}` : ''
}

export function loadConfig(): Promise<{ config: MemoryConfig; status: MemoryStatus; kinds: MemoryKindOption[] }> {
  return api('GET', '/api/memory/config')
}

export function saveConfig(patch: Partial<MemoryConfig>): Promise<{ config: MemoryConfig }> {
  return api('PUT', '/api/memory/config', patch)
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

export type WritePayload = {
  content: string
  title?: string
  type?: string
  project?: string
  tags?: string[]
  importance?: number
  files?: string[]
  supersedes?: string[]
}

export function writeEntry(payload: WritePayload): Promise<{ id: string }> {
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

export function rebuildIndex(): Promise<{ rebuilt: number; status: MemoryStatus }> {
  return api('POST', '/api/memory/rebuild')
}

/** 「MCP & Skills」页里那个接入目标的形状（记忆只展示、不在这里改） */
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
