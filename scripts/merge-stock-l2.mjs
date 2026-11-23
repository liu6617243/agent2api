#!/usr/bin/env node
// 存量 L2 合并：同一主题的多条重复结论 → 保留 1 条代表，其余标 superseded_by
//
// ── 为什么不删文件 ───────────────────────────────────────────
// `is_live()` = `superseded_by.is_none() && deleted_at.is_none() && !expired`。
// 设了 `superseded_by` 之后，条目自动退出检索 / 注入 / 画像 / 蒸馏 / 去重正主
// 等所有事实出口，但原文与演化链完整保留、随时可撤。这正是「合并后保留」——
// 用户明确要求「提过的要求一条不丢」，删除做不到这一点。
//
// ── 代表条目怎么选 ───────────────────────────────────────────
// 不是「最新的赢」，也不是「最长的赢」，而是：
//   1. kind 是 preference/decision 的优先（那是要求与决策，不是零散观察）
//   2. 信息量高的优先（用库里已有的 information_score 口径：长度 + 标签 + 文件）
//   3. 再按 created_at 早的优先（同分时保留最早那条，它最接近事情发生的时间）
//
// ── 证据怎么补 ───────────────────────────────────────────────
// 这批条目的 evidence 是旧版 extract 塞的「当批素材 id」，640 个里 475 个指向
// 不存在的条目。合并后把 evidence 洗成：
//   保留代表条目**有效**的旧锚点 + 被合并条目的 id（它们是同一结论的不同表述，
//   互为佐证）。被合并条目本身是真实存在的条目，所以这样得到的锚点是可核查的。
//
// 用法：
//   node scripts/merge-stock-l2.mjs                # dry-run
//   node scripts/merge-stock-l2.mjs --apply
//   node scripts/merge-stock-l2.mjs --apply --limit 3

import fs from 'node:fs'
import path from 'node:path'
import { DatabaseSync } from 'node:sqlite'

const APPLY = process.argv.includes('--apply')
const ROOT = process.env.A2A_MEMORY_ROOT || 'C:/Users/yiyiyoyo/.agent2api/memory'
const DB = path.join(ROOT, 'index.db')

const li = process.argv.indexOf('--limit')
const LIMIT = li > 0 ? Number(process.argv[li + 1]) : Infinity

const MERGE_SIM = Number(process.env.MERGE_SIM || 0.34)
const VALID_MIN = 2

const db = new DatabaseSync(DB, { readOnly: !APPLY })

// ── frontmatter 读写 ─────────────────────────────────────────
function split(raw) {
  const m = raw.match(/^---\r?\n([\s\S]*?)\r?\n---\r?\n?/)
  if (!m) return { front: '', body: raw, eol: '\n' }
  const eol = m[0].includes('\r\n') ? '\r\n' : '\n'
  return { front: m[1], body: raw.slice(m[0].length), eol }
}
function get(front, key) {
  const line = front.split(/\r?\n/).find(l => l.startsWith(key + ':'))
  return line ? line.slice(key.length + 1).trim() : ''
}
function asList(raw) {
  return (raw || '').replace(/^\[|\]$/g, '').split(/[,\s]+/).map(s => s.trim()).filter(Boolean)
}
function setField(front, key, value) {
  const lines = front.split(/\r?\n/)
  const i = lines.findIndex(l => l.startsWith(key + ':'))
  if (i >= 0) lines[i] = `${key}: ${value}`
  else lines.push(`${key}: ${value}`)
  return lines.join('\n')
}

// ── 相似度：中文二元组 dice（与 merge-duplicate-memory.mjs 同口径）──
function grams(s, n = 2) {
  const clean = String(s).replace(/[\s，。、？！：；""''（）()【】\[\]@#*_\-—…/\\|]+/g, '').toLowerCase()
  const set = new Set()
  for (let i = 0; i + n <= clean.length; i++) set.add(clean.slice(i, i + n))
  return set
}
function dice(a, b) {
  if (!a.size || !b.size) return 0
  let hit = 0
  for (const g of a) if (b.has(g)) hit++
  return (2 * hit) / (a.size + b.size)
}

// ── 读条目 ───────────────────────────────────────────────────
const rows = db.prepare(`
  SELECT id, path, title, kind, project, importance, created_at
    FROM entries
   WHERE deleted_at IS NULL AND superseded_by IS NULL AND layer='l2' AND agent='auto-extract'
`).all()

const items = []
for (const r of rows) {
  const abs = path.join(ROOT, r.path)
  if (!fs.existsSync(abs)) continue
  const raw = fs.readFileSync(abs, 'utf8')
  const { front, body } = split(raw)
  const ev = asList(get(front, 'evidence'))
  const validEv = ev.filter(id => {
    const e = db.prepare('SELECT deleted_at FROM entries WHERE id=?').get(id)
    return !!e && e.deleted_at == null
  })
  items.push({
    id: r.id,
    abs,
    raw,
    front,
    body,
    title: String(r.title || ''),
    kind: r.kind || 'other',
    project: r.project || '',
    importance: Number(r.importance) || 0.5,
    created_at: Number(r.created_at) || 0,
    ev,
    validEv,
    tags: asList(get(front, 'tags')),
    files: asList(get(front, 'files')),
  })
}

console.log(`模式: ${APPLY ? 'APPLY（真写）' : 'DRY-RUN（只读）'}`)
console.log(`存活 auto-extract l2 = ${items.length}`)

// ── 聚类：贪心 + 相似度阈值 ───────────────────────────────────
const vecs = items.map(it => grams(it.title + ' ' + it.body))
const used = new Array(items.length).fill(false)
const clusters = []
for (let i = 0; i < items.length; i++) {
  if (used[i]) continue
  const group = [i]
  used[i] = true
  for (let j = i + 1; j < items.length; j++) {
    if (used[j]) continue
    if (dice(vecs[i], vecs[j]) >= MERGE_SIM) { group.push(j); used[j] = true }
  }
  if (group.length > 1) clusters.push(group.map(k => items[k]))
}

// ── 选代表 ───────────────────────────────────────────────────
function infoScore(it) {
  const kindBonus = (it.kind === 'preference' || it.kind === 'decision') ? 200 : 0
  return kindBonus + it.body.length + it.tags.length * 20 + it.files.length * 10 + it.validEv.length * 50
}
function pickKeeper(group) {
  return [...group].sort((a, b) => {
    const d = infoScore(b) - infoScore(a)
    if (d) return d
    return a.created_at - b.created_at   // 同分保留最早的
  })[0]
}

console.log(`阈值 ${MERGE_SIM}，重复簇 = ${clusters.length}，涉及条目 = ${clusters.reduce((s, c) => s + c.length, 0)}`)
console.log()

let mergeTotal = 0
const plans = []
for (const group of clusters) {
  const keeper = pickKeeper(group)
  const merged = group.filter(x => x.id !== keeper.id)
  // 合并后证据：代表的有效旧锚点 + 被合并的条目 id（真实存在的条目，可核查）
  const nextEv = [...new Set([...keeper.validEv, ...merged.map(m => m.id)])].slice(0, 20)
  plans.push({ keeper, merged, nextEv })
  mergeTotal += merged.length
}

console.log(`合并后剩余 l2 = ${items.length - mergeTotal}（并入 ${mergeTotal} 条）`)
console.log()

for (const p of plans.slice(0, 15)) {
  console.log(`── 保留 ${p.keeper.id} [${p.keeper.kind}] valid=${p.keeper.validEv.length}→${p.nextEv.length}`)
  console.log(`   「${p.keeper.title.slice(0, 60)}」`)
  for (const m of p.merged) {
    console.log(`     并入 ${m.id} [${m.kind}] ${m.title.slice(0, 52)}`)
  }
}
if (plans.length > 15) console.log(`… 其余 ${plans.length - 15} 簇`)

// ── 单条「不够格且不重复」的：证据洗成有效锚点 ───────────────
const clustered = new Set(clusters.flat().map(x => x.id))
const singles = items.filter(it => !clustered.has(it.id))
const weakSingles = singles.filter(it => it.validEv.length < VALID_MIN)
console.log(`\n未聚类的单条 = ${singles.length}，其中有效锚点 < ${VALID_MIN} 的 = ${weakSingles.length}`)
for (const it of weakSingles.slice(0, 10)) {
  console.log(`   ${it.id} [${it.kind}] ev=${it.ev.length}/valid=${it.validEv.length}  ${it.title.slice(0, 46)}`)
}

if (!APPLY) {
  fs.writeFileSync('.workbuddy/tmp/_merge_plan.json', JSON.stringify(
    plans.map(p => ({
      keeper: { id: p.keeper.id, title: p.keeper.title, kind: p.keeper.kind },
      merged: p.merged.map(m => ({ id: m.id, title: m.title, kind: m.kind })),
      nextEvidence: p.nextEv,
    })), null, 2))
  fs.writeFileSync('.workbuddy/tmp/_weak_singles.json', JSON.stringify(
    weakSingles.map(i => ({ id: i.id, title: i.title, kind: i.kind, ev: i.ev.length, valid: i.validEv.length })), null, 2))
  console.log('\n未写任何文件。方案已存 .workbuddy/tmp/_merge_plan.json')
  process.exit(0)
}

// ── 执行 ─────────────────────────────────────────────────────
let mergedCount = 0
for (const p of plans.slice(0, LIMIT)) {
  const now = Date.now()
  // 1) 代表条目：写入合并后的证据（其余字段不动）
  const { front, body, eol } = split(p.keeper.raw)
  let next = setField(front, 'evidence', `[${p.nextEv.join(', ')}]`)
  const text = `---${eol}${next}${eol}---${eol}${body.startsWith(eol) ? body.slice(eol.length) : body}`
  fs.writeFileSync(p.keeper.abs, text)

  // 2) 被合并的条目：标记被取代（原文保留，检索 / 注入自动跳过）
  for (const m of p.merged) {
    const s = split(m.raw)
    let f = setField(s.front, 'superseded_by', p.keeper.id)
    f = setField(f, 'dedup_status', 'merged')
    f = setField(f, 'updated_at', String(now))
    const t = `---${s.eol}${f}${s.eol}---${s.eol}${s.body.startsWith(s.eol) ? s.body.slice(s.eol.length) : s.body}`
    fs.writeFileSync(m.abs, t)
    mergedCount++
  }
}

console.log(`\n已合并 ${mergedCount} 条，保留 ${plans.slice(0, LIMIT).length} 条代表。`)
console.log('下一步：重启软件，索引会按 .md 事实源重建。')