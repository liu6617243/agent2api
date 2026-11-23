#!/usr/bin/env node
// 存量 L2 降级迁移 · 默认 dry-run
//
// 判据不是我自己发明的，而是直接用新准入规则本身：
//   「auto-extract 产出的条目，evidence 里**有效**（指向真实存在、未删除条目）
//    的锚点 < 2 条，就不够格当 l2」
//
// 为什么不看 evidence 的长度：旧版 extract 把当批 5 条素材 id 直接写进 evidence，
// 那是「生成时的原料」，不是「独立批次的确认」。更糟的是模型会编 id：真机
// 640 个锚点里 475 个指向的条目在磁盘上根本不存在。
//
// 所以只有「能解析到真实条目」的锚点才算数。
//
// 迁移动作（与代码里的 promote_draft 反向）：
//   1. .md 里 layer: l2 → l1
//   2. 把 evidence 洗成只剩有效锚点（不足 2 条就清空）
//   3. 文件从 general/l2/<kind>/<id>.md 搬到 general/l1/auto-extract/<日期>.md
//   4. 重建索引（用 .md 为事实源，索引是派生物）
//
// 用法：
//   node scripts/rollback-l2.mjs              # dry-run，只打印
//   node scripts/rollback-l2.mjs --apply      # 真跑
//   node scripts/rollback-l2.mjs --apply --limit 5

import fs from 'node:fs'
import path from 'node:path'
import { DatabaseSync } from 'node:sqlite'

const APPLY = process.argv.includes('--apply')
const ROOT = process.env.A2A_MEMORY_ROOT || 'C:/Users/yiyiyoyo/.agent2api/memory'
const DB = path.join(ROOT, 'index.db')
const VALID_MIN = 2

const limitIdx = process.argv.indexOf('--limit')
const LIMIT = limitIdx > 0 ? Number(process.argv[limitIdx + 1]) : Infinity

const db = new DatabaseSync(DB, { readOnly: !APPLY })

// ── 读 frontmatter ────────────────────────────────────────────
function split(raw) {
  const m = raw.match(/^---\r?\n([\s\S]*?)\r?\n---\r?\n?/)
  if (!m) return { front: '', body: raw, eol: '\n' }
  const head = m[0]
  const eol = head.includes('\r\n') ? '\r\n' : '\n'
  return { front: m[1], body: raw.slice(m[0].length), eol }
}
function get(front, key) {
  const line = front.split(/\r?\n/).find(l => l.startsWith(key + ':'))
  return line ? line.slice(key.length + 1).trim() : null
}
function asList(raw) {
  if (raw == null) return []
  return raw.replace(/^\[|\]$/g, '').split(/[,\s]+/).map(s => s.trim()).filter(Boolean)
}
function setField(front, key, value) {
  const lines = front.split(/\r?\n/)
  const i = lines.findIndex(l => l.startsWith(key + ':'))
  if (i >= 0) lines[i] = `${key}: ${value}`
  else lines.push(`${key}: ${value}`)
  return lines.join('\n')
}

// ── 判定 ──────────────────────────────────────────────────────
const exists = (id) => {
  const r = db.prepare('SELECT deleted_at FROM entries WHERE id=?').get(id)
  return !!r && r.deleted_at == null
}

const rows = db.prepare(`
  SELECT id, path, title, project, kind, created_at
    FROM entries
   WHERE deleted_at IS NULL AND layer='l2' AND agent='auto-extract'
`).all()

const plan = []
for (const r of rows) {
  const abs = path.join(ROOT, r.path)
  if (!fs.existsSync(abs)) continue
  const { front } = split(fs.readFileSync(abs, 'utf8'))
  const ev = asList(get(front, 'evidence'))
  const valid = ev.filter(exists)
  if (valid.length >= VALID_MIN) continue
  plan.push({
    id: r.id,
    title: String(r.title).slice(0, 52),
    path: r.path,
    abs,
    kind: r.kind || 'other',
    created_at: r.created_at || Date.now(),
    evAll: ev.length,
    evValid: valid.length,
    keepEvidence: valid,
  })
}

console.log(`模式: ${APPLY ? 'APPLY（真写）' : 'DRY-RUN（只读）'}`)
console.log(`记忆根: ${ROOT}`)
console.log(`候选（auto-extract 的 l2，有效锚点 < ${VALID_MIN}）: ${plan.length} 条\n`)

const byKind = {}
for (const p of plan) byKind[p.kind] = (byKind[p.kind] || 0) + 1
console.log('按 kind 分布:', JSON.stringify(byKind))
console.log()

for (const p of plan.slice(0, 12)) {
  console.log(`  ${p.id}  ev=${p.evAll}/valid=${p.evValid}  ${p.title}`)
}
if (plan.length > 12) console.log(`  … 其余 ${plan.length - 12} 条`)

if (!APPLY) {
  console.log('\n未写任何文件。确认后加 --apply 执行。')
  fs.writeFileSync('.workbuddy/tmp/_l2_rollback_dryrun.json', JSON.stringify(plan.map(({ abs, ...rest }) => rest), null, 2))
  console.log('清单已存 .workbuddy/tmp/_l2_rollback_dryrun.json')
  process.exit(0)
}

// ── 执行 ──────────────────────────────────────────────────────
// 日期目录：跟 created_at 走（与 entry_path 的规则一致）
function dateOf(stamp) {
  const d = new Date(stamp)
  const p = (n) => String(n).padStart(2, '0')
  return `${d.getFullYear()}-${p(d.getMonth() + 1)}-${p(d.getDate())}`
}
function l1Abs(p) {
  const proj = (p.project || '').replace(/^_global$/i, '')
  const base = proj ? path.join(ROOT, 'projects', proj, 'l1', 'auto-extract') : path.join(ROOT, 'general', 'l1', 'auto-extract')
  return path.join(base, dateOf(p.created_at) + '.md')
}

let done = 0, skipped = 0
const sections = new Map() // targetFile -> [{id, block}]

for (const p of plan.slice(0, LIMIT)) {
  try {
    const raw = fs.readFileSync(p.abs, 'utf8')
    const { front, body, eol } = split(raw)
    let next = setField(front, 'layer', 'l1')
    // evidence 洗成只剩真实存在的锚点；不足 2 条时保持为空（它回到「草稿」
    // 状态，靠以后被真实流水反复确认再升回来）
    next = setField(next, 'evidence', `[${p.keepEvidence.join(', ')}]`)
    const block = `---${eol}${next}${eol}---${eol}${body.startsWith(eol) ? body.slice(eol.length) : body}`

    // id 是唯一的，l1 容器里追加一节
    const target = l1Abs(p)
    if (!sections.has(target)) sections.set(target, { dir: path.dirname(target), blocks: [] })
    sections.get(target).blocks.push({ id: p.id, block })
    done++
  } catch (error) {
    console.log(`  ⚠️ ${p.id} 失败: ${error.message}`)
    skipped++
  }
}

// 写入 l1 容器
for (const [target, info] of sections) {
  fs.mkdirSync(info.dir, { recursive: true })
  const exists0 = fs.existsSync(target)
  let text = exists0 ? fs.readFileSync(target, 'utf8') : ''
  for (const b of info.blocks) {
    text += (text && !text.endsWith('\n') ? '\n' : '') + b.block + '\n\n'
  }
  fs.writeFileSync(target, text)
}

// 删除原 l2 文件
for (const p of plan.slice(0, LIMIT)) {
  try { fs.unlinkSync(p.abs) } catch {}
}

// 清理空目录
for (const p of plan.slice(0, LIMIT)) {
  let dir = path.dirname(p.abs)
  for (let i = 0; i < 4; i++) {
    try {
      if (fs.readdirSync(dir).length === 0) { fs.rmdirSync(dir); dir = path.dirname(dir) } else break
    } catch { break }
  }
}

console.log(`\n已迁移 ${done} 条到 l1 容器，跳过 ${skipped} 条。`)
console.log('下一步：重启软件（索引会按 .md 事实源重建）。')