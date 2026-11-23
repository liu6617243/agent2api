// ─────────────────────────────────────────────────────────────────────
// migrate-agenthub.mjs —— 把 AgentHub 记忆中枢的条目搬进 agent2api
//
// ── 为什么不是「指个目录让导入页自己读」 ────────────────────────────
// agent2api 的 Markdown 导入是**一个文件一条**（`import.rs` 的
// `from_markdown`），单文件正文上限 `MAX_FILE_CHARS = 24_000`。而 AgentHub 的
// 存储形态是「一天一个大文件、每个 section 才是一条记忆」：
//
//   general/l1/workbuddy/2026-10-18.md   ← 1,361 条 section 挤在一个文件里
//
// 直接指目录的后果：26188 条 section 被压成 264 条，94 个文件超长被截断，
// 一天之内的条目全部糊成一条。所以必须**按 section 拆**，走 `POST
// /api/memory/write` 逐条写 —— 顺便还能带上原始时间、tags、importance，
// 这些都是导入器拿不到的。
//
// ── 为什么默认只搬非 daily ──────────────────────────────────────────
// 库里 26,188 行里 25,821 行是 `type=daily`，其中 16,849 行正文不足 200 字节
// （一句话、一条命令、一次工具调用）。这些是「会话流水」，agent2api 自己也在
// 记；搬过去只会把 l1 撑到两万多条，而请求级注入每次只有 600 token 预算，
// 命中率被稀释、有用的条目反而更难浮上来。所以默认只搬蒸馏品：
// l2 的 knowledge / decision / insight（331 条）。
//
//   --scope l2     默认。非 daily 且 layer=l2      （331 条）
//   --scope typed  再加上有明确类型的 l1 笔记       （363 条）
//   --scope all    全搬，含 25,821 条 daily        （26,188 条）
//
// ── 为什么必须能回滚 ────────────────────────────────────────────────
// 写入是批量且不可逆的（进了库就参与检索与注入）。所以每写成功一条就把
// 「源 id → 新 id」记进 `--manifest`（默认 `.workbuddy/tmp/agenthub-migrate-*.json`），
// 想撤销就 `--undo`：按台账里的新 id 逐条 `POST /api/memory/delete`。
// 走的是**回收站**而不是物理删除 —— 撤错了还能 `POST /api/memory/restore` 捞回来。
//
// 用法：
//   node scripts/migrate-agenthub.mjs                    # 干跑，只报数
//   node scripts/migrate-agenthub.mjs --scope typed      # 干跑，363 条的口径
//   node scripts/migrate-agenthub.mjs --apply            # 真写
//   node scripts/migrate-agenthub.mjs --apply --only=l2:knowledge   # 先试一类
//   node scripts/migrate-agenthub.mjs --undo             # 干跑：按最近台账算能撤多少
//   node scripts/migrate-agenthub.mjs --undo --apply     # 真撤（进回收站）
//   node scripts/migrate-agenthub.mjs --undo --manifest=.workbuddy/tmp/xxx.json --apply
// ─────────────────────────────────────────────────────────────────────

import fs from 'node:fs'
import path from 'node:path'
import os from 'node:os'
import crypto from 'node:crypto'
import { createRequire } from 'node:module'

// ESM 里拿 `node:sqlite` 只能走 createRequire：它是内置模块，没有 ESM 导出面
const require = createRequire(import.meta.url)

// ── 参数 ────────────────────────────────────────────────────────────
const argv = process.argv.slice(2)
const has = (flag) => argv.includes(flag)
const val = (name, fallback) => {
  // 两种写法都认：`--scope=typed` 与 `--scope typed`。只认等号那种的话，
  // 「--scope all」会被静默忽略、干跑照旧报 l2 的 331 条，很容易看错口径。
  const eq = argv.find((a) => a.startsWith(`--${name}=`))
  if (eq) return eq.slice(name.length + 3)
  const at = argv.indexOf(`--${name}`)
  if (at >= 0 && argv[at + 1] && !argv[at + 1].startsWith('--')) return argv[at + 1]
  return fallback
}

const SOURCE = val('source', path.join(os.homedir(), 'AgentHub', 'memory')).replace(/^~/, os.homedir())
const DB = val('db', path.join(SOURCE, 'index', 'memory.sqlite'))
const API = val('api', 'http://127.0.0.1:3065').replace(/\/$/, '')
const SCOPE = val('scope', 'l2')
const ONLY = val('only', '')
const APPLY = has('--apply')
const LIMIT = Number(val('limit', '0'))
const CONCURRENCY = Number(val('concurrency', '4'))
const AGENT = val('agent', 'agenthub-migrate')
// `--manifest` 既用来「这轮写到哪」，也用来 `--undo`「撤哪一轮」。
// 不显式给的时候两边行为不同：写的是新建一个带时间戳的文件，撤的是取最新那个。
const MANIFEST_GIVEN = val('manifest', '')
const MANIFEST =
  MANIFEST_GIVEN ||
  path.join('.workbuddy', 'tmp', `agenthub-migrate-${new Date().toISOString().slice(0, 19).replace(/[:T]/g, '')}.json`)

/// 找最近一轮的台账。文件名是 `YYYYMMDDHHMMSS` 前缀，字典序即时间序。
function latestManifest() {
  const dir = path.join('.workbuddy', 'tmp')
  if (!fs.existsSync(dir)) return null
  const files = fs
    .readdirSync(dir)
    .filter((name) => /^agenthub-migrate-.*\.json$/.test(name))
    .sort()
  return files.length ? path.join(dir, files[files.length - 1]) : null
}

// ── 凭据：先看环境变量，再看 workbuddy 的 mcp.json ──────────────────
function token() {
  const env = process.env.A2A_MEMORY_TOKEN
  if (env) return env.trim()
  const candidates = [
    path.join(os.homedir(), '.workbuddy', 'mcp.json'),
    path.join(os.homedir(), '.codebuddy', 'mcp.json'),
  ]
  for (const file of candidates) {
    try {
      const raw = JSON.parse(fs.readFileSync(file, 'utf8'))
      const entry = (raw.mcpServers || raw.servers || {})['agent2api-memory']
      const auth = entry?.headers?.Authorization || entry?.headers?.authorization
      if (auth) return String(auth).replace(/^Bearer\s+/i, '').trim()
    } catch {
      /* 换下一个 */
    }
  }
  throw new Error('找不到 agent2api 的 Bearer token：设 A2A_MEMORY_TOKEN，或在 ~/.workbuddy/mcp.json 里配 agent2api-memory')
}

const TOKEN = token()
const HDR = {
  Authorization: `Bearer ${TOKEN}`,
  'Content-Type': 'application/json',
  Accept: 'application/json',
}

async function api(method, route, body) {
  const res = await fetch(`${API}${route}`, {
    method,
    headers: HDR,
    body: body === undefined ? undefined : JSON.stringify(body),
  })
  const text = await res.text()
  let json = null
  try {
    json = text ? JSON.parse(text) : null
  } catch {
    /* 非 JSON */
  }
  if (!res.ok) throw new Error(`${method} ${route} → ${res.status} ${text.slice(0, 200)}`)
  return json && Object.prototype.hasOwnProperty.call(json, 'data') ? json.data : json
}

// ── 类型映射 ────────────────────────────────────────────────────────
//
// AgentHub 的 type 是**自由文本**（用户在 UI 里能随手写「技术结论」「pattern」），
// 所以映射表必须有兜底：认不出来的归 knowledge，不归 other —— 这些条目能活到
// 现在，说明正文是有信息量的，`other` 在注入里只值一档权重，等于白搬。
const KIND_MAP = new Map([
  ['knowledge', 'knowledge'],
  ['decision', 'decision'],
  ['insight', 'insight'],
  ['preference', 'preference'],
  ['profile', 'profile'],
  ['daily', 'daily'],
  // 「踩过什么、下次怎么绕」这一类：AgentHub 分得很细，agent2api 只有一个 pitfall
  ['lesson', 'pitfall'],
  ['bugfix', 'pitfall'],
  ['troubleshooting', 'pitfall'],
  ['debug', 'pitfall'],
  // 事实/结论/模式/配置：都是「外部世界的知识」
  ['fact', 'knowledge'],
  ['conclusion', 'knowledge'],
  ['technical', 'knowledge'],
  ['技术结论', 'knowledge'],
  ['config', 'knowledge'],
  ['pattern', 'knowledge'],
  ['methodology', 'knowledge'],
  ['note', 'knowledge'],
  ['finding', 'knowledge'],
])

// l2 默认层：蒸馏品。pitfall/preference 来自 l1 笔记时保持 l1 的原始语义。
const L2_KINDS = new Set(['decision', 'knowledge', 'insight', 'profile'])

// ── 项目 slug 映射 ──────────────────────────────────────────────────
//
// 同一项目在两边叫法不同（AgentHub 用 `liu6617243--agent2api`，agent2api 用
// `agent2api-main`），而且 AgentHub 里同一个项目有好几个历史名。不归并的话
// 迁完会多出五六个几乎空的「项目」，项目卡刷一屏全是碎片。
//
// 还有一个坑：后端的 `sanitize_slug` 只留 `[a-z0-9._-]`，中文全变成分隔符再被
// trim 掉 —— `南昌地铁cbtc-ats模拟软件` 会落成 `cbtc-ats`（正好对上了已有项目），
// `json格式化工具` 会落成 `json`（和 `json-formatter` 分裂），纯中文的项目名
// （`磁盘清理`）会整个塌成 `_global`。所以中文名必须在这里显式给出英文 slug。
const PROJECT_MAP = new Map([
  ['liu6617243--agent2api', 'agent2api-main'],
  ['agenthub-agent2api-号池运维', 'agent2api-main'],
  ['agenthub-integration', 'agenthub'],
  ['南昌地铁cbtc-ats模拟软件', 'cbtc-ats'],
  ['南昌地铁cbtc新版ats模拟软件', 'cbtc-ats'],
  ['南昌地铁cbtc-ats新版ats模拟软件', 'cbtc-ats'],
  ['南昌地铁cbtc-ats', 'cbtc-ats'],
  ['json格式化工具', 'json-formatter'],
  ['json-format-tool', 'json-formatter'],
  ['磁盘清理', 'disk-cleanup'],
  ['pe32补丁方法论', 'pe32-patching'],
])

function projectSlug(raw) {
  const key = String(raw || '').trim()
  if (!key) return undefined // 不传 = 全局（_global）
  return PROJECT_MAP.get(key) || key
}

// ── importance：AgentHub 是 1..5 的整数，agent2api 是 0..1 ──────────
// 直接除以 5 会把 3（最常见，占 78%）压到 0.6，跟 agent2api 自己蒸馏出来的
// 0.7~0.9 相比永远排不上号。所以给中高档位一点偏置：4→0.82、5→0.95。
const IMPORTANCE = new Map([
  [1, 0.15],
  [2, 0.4],
  [3, 0.62],
  [4, 0.82],
  [5, 0.95],
])

function parseTags(raw) {
  const text = String(raw || '').trim()
  if (!text || text === '[]') return []
  const inner = text.startsWith('[') ? text.slice(1, -1) : text
  return inner
    .split(',')
    .map((t) => t.trim().replace(/^["']|["']$/g, ''))
    .filter(Boolean)
    .slice(0, 12)
}

function parseStamp(text) {
  const raw = String(text || '').trim()
  if (!raw) return undefined
  const ms = Date.parse(raw)
  return Number.isFinite(ms) ? ms : undefined
}

// ── 正文提取 ────────────────────────────────────────────────────────
const fileCache = new Map()

function readFile(file) {
  if (fileCache.has(file)) return fileCache.get(file)
  let text = ''
  try {
    text = fs.readFileSync(path.join(SOURCE, file), 'utf8').replace(/^\uFEFF/, '')
  } catch {
    text = ''
  }
  fileCache.set(file, text)
  return text
}

/// 去掉 frontmatter，返回正文与头部字段。
function splitFront(text) {
  if (!text.startsWith('---')) return { head: {}, body: text }
  const end = text.indexOf('\n---', 3)
  if (end < 0) return { head: {}, body: text }
  const head = {}
  for (const line of text.slice(3, end).split(/\r?\n/)) {
    const m = line.match(/^([A-Za-z_][A-Za-z0-9_]*):\s*(.*)$/)
    if (m) head[m[1]] = m[2].trim()
  }
  return { head, body: text.slice(end + 4) }
}

/// 从「一天一个大文件」里切出某条 section。
///
/// 判据是 AgentHub 自己写的锚点注释 `<!-- mem:<id> -->`：它就在 section 标题下面
/// 一行，比「按时间戳猜边界」可靠。切法是「从锚点往上找到最近的 `## ` 标题，
/// 往下到下一个 `## ` 标题之前」—— 中间的空行与 `> importance:` 引用行都归这条。
function sectionBody(text, id) {
  const marker = `<!-- mem:${id} -->`
  const at = text.indexOf(marker)
  if (at < 0) return ''
  const before = text.slice(0, at)
  const start = Math.max(before.lastIndexOf('\n## '), before.lastIndexOf('\n# '))
  const from = start >= 0 ? start + 1 : 0
  const after = text.slice(at + marker.length)
  const nextAt = after.search(/\n#{1,3} /)
  const to = nextAt >= 0 ? at + marker.length + nextAt : text.length
  return text.slice(from, to)
}

/// 拿掉标题行、锚点注释、`> importance:` 元信息行，只留正文。
/// 不拿掉的话，迁过去的每一条都会以「## 00:18 · xxx」开头，标题与正文重复。
function cleanBody(raw) {
  const lines = raw.split(/\r?\n/)
  const kept = []
  for (const line of lines) {
    if (/^#{1,3}\s/.test(line)) continue
    if (/^<!--\s*mem:/.test(line)) continue
    if (/^>\s*importance:/.test(line)) continue
    kept.push(line)
  }
  return kept.join('\n').replace(/\n{3,}/g, '\n\n').trim()
}

// ── 读源库 ──────────────────────────────────────────────────────────
function loadRows() {
  let DatabaseSync
  try {
    ;({ DatabaseSync } = require('node:sqlite'))
  } catch {
    throw new Error('需要 Node 22+（node:sqlite）：本机 node 版本过低')
  }
  const db = new DatabaseSync(DB, { readOnly: true })
  const sql = `
    select id, path, anchor, type, layer, title, tags, project, agent,
           importance, created, pinned, starred, valid_to, superseded_by
      from mem
     where superseded_by is null
       and (valid_to is null or valid_to = 0)
     order by created asc`
  return db.prepare(sql).all()
}

function inScope(row) {
  const type = String(row.type || '').toLowerCase()
  if (ONLY) {
    const [onlyLayer, onlyType] = ONLY.split(':')
    if (onlyLayer && row.layer !== onlyLayer) return false
    if (onlyType && type !== onlyType.toLowerCase()) return false
    return true
  }
  if (SCOPE === 'all') return true
  if (type === 'daily') return false
  if (SCOPE === 'typed') return true
  // l2：只认蒸馏层，且必须带明确类型
  return row.layer === 'l2' && KIND_MAP.has(type)
}

// ── 组装写入体 ──────────────────────────────────────────────────────
function toWrite(row) {
  const text = readFile(row.path)
  if (!text) return { skip: `正文文件读不到：${row.path}` }
  const raw = row.anchor ? sectionBody(text, row.id) : splitFront(text).body
  const body = cleanBody(raw)
  if (body.length < 20) return { skip: `正文太短（${body.length} 字）` }

  const type = KIND_MAP.get(String(row.type || '').toLowerCase()) || 'knowledge'
  const layer = row.layer === 'l2' && L2_KINDS.has(type) ? 'l2' : 'l1'
  const title = String(row.title || '').trim().slice(0, 120) || body.split('\n')[0].slice(0, 120)
  const created = Number(row.created) || parseStamp(row.created) || undefined

  return {
    payload: {
      content: body,
      title,
      type,
      layer,
      project: projectSlug(row.project),
      agent: AGENT,
      tags: parseTags(row.tags),
      importance: IMPORTANCE.get(Number(row.importance)) ?? 0.6,
      createdAt: created,
      pinned: Number(row.pinned) === 1 ? true : undefined,
      starred: Number(row.starred) === 1 ? true : undefined,
      // sourceId 不是后端认识的字键，但保留在 manifest 里做回溯
    },
    sourceId: row.id,
    sourceType: row.type,
    sourceLayer: row.layer,
    hash: crypto.createHash('sha256').update(body).digest('hex'),
  }
}

// ── 现有条目：只按「标题 + 类型」挡一遍 ─────────────────────────────
//
// 为什么不比正文 hash：`list` 端点不回正文，要比就得逐条 `get`，331 条还行、
// 两万条就是两万次请求。而真正会撞车的场景只有一种 —— 同一个项目在两边都被
// 蒸馏过同一条结论，那种条目的**标题几乎逐字相同**，标题挡就够了；正文级的
// 精确重复后端自己会用 hash 挡（`write_inner` 的 dedup）。
async function existingKeys() {
  const keys = new Set()
  let offset = 0
  for (;;) {
    const page = await api('GET', `/api/memory/list?limit=500&offset=${offset}&includeSuperseded=1`)
    const items = page?.items || []
    for (const item of items) keys.add(`${item.type}|${String(item.title || '').trim()}`)
    offset += items.length
    if (items.length < 500 || offset >= (page?.total || 0)) break
  }
  return keys
}

// ── 迁移前给现有画像加锁（`--protect-profiles`）─────────────────────
//
// 为什么需要：写入时后端把 `updated_at` 设成**现在**（`createdAt` 只回填
// `created_at` / `valid_from`，见 `write_inner` 那段注释），所以搬完 331 条，
// 「画像之后有多少条新记忆」会从 60 条直接涨到 391 条 —— 下一次画像生成必然
// 触发。而画像素材池是按 `importance*0.5 + recency*0.3 + pinned*0.2` 取前
// `deep.batchSize`（默认 50）条，agenthub 那批 importance 5（映射 0.95）刚更新过，
// 分值 0.775，直接盖过现有 0.9 分条目的 0.747 —— 50 条素材里会有一大半换成
// 它的内容，**现有 16 条画像（没锁的那些）会被送进回收站重写**。
//
// 加锁 = `pinned: true` + tags 里补 `pin`（`mod.rs` 的 `PROFILE_PIN_TAG`）。
// 重建时会跳过带这个标记的条目（`enrich.rs` 的 `kept` 那段），等于「这条我认，
// 别动它」。这是这个页面上唯一让人判断压过模型的地方。
async function protectProfiles(apply) {
  const page = await api('GET', '/api/memory/list?type=profile&limit=500')
  const items = page?.items || []
  const todo = items.filter((item) => !(item.pinned && (item.tags || []).includes('pin')))
  console.log(`画像现有 ${items.length} 条，需加锁 ${todo.length} 条`)
  if (!apply) return todo.length
  let ok = 0
  for (const item of todo) {
    try {
      const tags = [...new Set([...(item.tags || []), 'pin'])]
      await api('POST', '/api/memory/update', { id: item.id, pinned: true, tags })
      ok++
    } catch (error) {
      console.log(`  加锁失败 ${item.id}：${String(error.message || error).slice(0, 120)}`)
    }
  }
  console.log(`已加锁 ${ok} 条`)
  return ok
}

// ── 撤销：按台账把这一轮写进去的条目退回回收站 ────────────────────
//
// 为什么是回收站而不是物理删除：迁移的口径随时可能改（今天搬 331 条 l2，
// 明天想改成 363 条 typed），撤错一轮的代价是「库里多了一批要手工清的东西」。
// 回收站保留了 restore 的回头路，而删除本身已经把条目从检索与注入里摘掉了，
// 目的已经达到。
async function undo() {
  const file = MANIFEST_GIVEN || latestManifest()
  if (!file) throw new Error('找不到回滚台账：.workbuddy/tmp/agenthub-migrate-*.json 不存在')
  if (!fs.existsSync(file)) throw new Error(`台账不存在：${file}`)

  const ledger = JSON.parse(fs.readFileSync(file, 'utf8'))
  const written = (ledger.written || []).filter((item) => item.id)
  const noId = (ledger.written || []).length - written.length
  console.log(`台账 ${file}`)
  console.log(`  写入于 ${ledger.startedAt || '?'}，scope=${ledger.scope || '?'}，条目 ${written.length} 条${noId ? `（另有 ${noId} 条没拿到 id，跳过）` : ''}`)

  if (!APPLY) {
    console.log('\n前 5 条将退回回收站：')
    for (const item of written.slice(0, 5)) {
      console.log(' ──', item.layer, item.type, '|', item.title, `(${item.id})`)
    }
    console.log('\n干跑结束。真撤加 --apply（进回收站，可 /api/memory/restore 捞回）')
    return
  }

  let ok = 0
  let failed = 0
  const errors = []
  const queue = [...written]
  const worker = async () => {
    for (;;) {
      const item = queue.shift()
      if (!item) return
      try {
        await api('POST', '/api/memory/delete', { id: item.id })
        ok++
      } catch (error) {
        failed++
        if (errors.length < 5) errors.push(`${item.title} → ${String(error.message || error).slice(0, 160)}`)
      }
      if ((ok + failed) % 50 === 0) console.log(`  进度 ${ok + failed}/${written.length}（撤销 ${ok} / 失败 ${failed}）`)
    }
  }
  await Promise.all(Array.from({ length: Math.max(1, CONCURRENCY) }, worker))

  console.log(`\n撤销完成：${ok} 条进回收站，失败 ${failed} 条`)
  if (errors.length) {
    console.log('失败样例：')
    for (const line of errors) console.log('  ', line)
  }
  if (ok) console.log('要全部还原：对上面这些 id 逐条 POST /api/memory/restore')
}

// ── 主流程 ──────────────────────────────────────────────────────────
async function main() {
  if (has('--undo')) return undo()

  console.log(`源库 ${DB}`)
  console.log(`接口 ${API}   口径 ${ONLY ? 'only=' + ONLY : SCOPE}   ${APPLY ? '【真写】' : '【干跑】'}`)

  if (has('--protect-profiles')) await protectProfiles(APPLY)

  const rows = loadRows()
  const scopeRows = rows.filter(inScope)
  console.log(`库内有效 ${rows.length} 条，选中 ${scopeRows.length} 条`)

  const planned = []
  const skipped = []
  for (const row of scopeRows) {
    const out = toWrite(row)
    if (out.skip) skipped.push({ id: row.id, reason: out.skip })
    else planned.push(out)
  }
  console.log(`可写入 ${planned.length} 条，源侧跳过 ${skipped.length} 条`)

  const kinds = {}
  const layers = {}
  const projects = {}
  let chars = 0
  for (const item of planned) {
    kinds[item.payload.type] = (kinds[item.payload.type] || 0) + 1
    layers[item.payload.layer] = (layers[item.payload.layer] || 0) + 1
    const p = item.payload.project || '(全局)'
    projects[p] = (projects[p] || 0) + 1
    chars += item.payload.content.length
  }
  console.log('类型', JSON.stringify(kinds))
  console.log('层级', JSON.stringify(layers))
  console.log('项目', JSON.stringify(projects))
  console.log('正文合计', (chars / 1024).toFixed(0) + 'KB ≈', Math.round(chars / 2.2), 'tokens')

  const existing = await existingKeys()
  let dup = 0
  const todo = []
  for (const item of planned) {
    if (existing.has(`${item.payload.type}|${item.payload.title.trim()}`)) dup++
    else todo.push(item)
  }
  console.log(`与现有条目同类型同标题 ${dup} 条，待写 ${todo.length} 条`)

  if (!APPLY) {
    console.log('\n前 5 条预览：')
    for (const item of todo.slice(0, 5)) {
      console.log(' ──', item.payload.layer, item.payload.type, '|', item.payload.title)
      console.log('   ', item.payload.content.slice(0, 160).replace(/\n/g, ' '))
    }
    console.log('\n干跑结束。真写加 --apply')
    return
  }

  const target = LIMIT > 0 ? todo.slice(0, LIMIT) : todo
  const manifest = { startedAt: new Date().toISOString(), api: API, scope: SCOPE, only: ONLY, written: [], failed: [] }
  let done = 0
  let failed = 0

  const queue = [...target]
  const worker = async () => {
    for (;;) {
      const item = queue.shift()
      if (!item) return
      try {
        const res = await api('POST', '/api/memory/write', item.payload)
        const id = res?.id || res?.entry?.id || res?.items?.[0]?.id
        manifest.written.push({ source: item.sourceId, id, type: item.payload.type, layer: item.payload.layer, title: item.payload.title })
        done++
      } catch (error) {
        failed++
        manifest.failed.push({ source: item.sourceId, title: item.payload.title, error: String(error.message || error) })
      }
      if ((done + failed) % 25 === 0) console.log(`  进度 ${done + failed}/${target.length}（成功 ${done} / 失败 ${failed}）`)
    }
  }
  await Promise.all(Array.from({ length: Math.max(1, CONCURRENCY) }, worker))

  manifest.finishedAt = new Date().toISOString()
  manifest.summary = { planned: planned.length, dup, done, failed }
  fs.mkdirSync(path.dirname(MANIFEST), { recursive: true })
  fs.writeFileSync(MANIFEST, JSON.stringify(manifest, null, 2), 'utf8')

  console.log(`\n写入完成：成功 ${done} / 失败 ${failed}`)
  console.log(`回滚台账：${MANIFEST}`)
  if (failed) {
    console.log('失败样例：')
    for (const item of manifest.failed.slice(0, 5)) console.log('  ', item.title, '→', item.error.slice(0, 160))
  }
}

main().catch((error) => {
  console.error('失败：', error.message)
  process.exit(1)
})
