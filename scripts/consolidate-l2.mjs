#!/usr/bin/env node
// 存量 L2 治理执行器：按全局仲裁后的最终映射执行 保留 / 合并 / 降级
//
// ── 输入 ─────────────────────────────────────────────────────
//   .workbuddy/tmp/_final_map.json   全局仲裁后的逐条判定
//     { id, verdict: keep|merge|demote|noise, topic, keeper, points[], fixup }
//   $A2A_MEMORY_ROOT 下的 .md 事实源 + index.db 派生索引
//
// ── 四种处置与它们的可逆性 ───────────────────────────────────
//   keep   —— 原文不动；只洗 evidence、追加合并要点、按需订正错误事实
//   merge  —— 正文要点并入代表 → 自己标 superseded_by（退出检索/注入/画像/蒸馏，
//             原文与演化链完整保留，随时可撤）
//   demote —— 退回 l1（改 layer、清 path、搬进 l1 容器）。不删，只是不再是结论
//   noise  —— 进 trash/（deleted_at 置位，可恢复）
// 没有任何一条是不可逆删除。
//
// ── 「内容一条不丢」怎么保证 ─────────────────────────────────
// 只标 superseded_by 会丢掉被并条目的独有事实。所以 merge 时把被并条目的
// points 逐条追加到代表正文的「## 合并要点」小节，带来源 id。同一要点重复
// 出现只留一次（去重按内容，不按来源）。
//
// ── 为什么 evidence 要重洗 ───────────────────────────────────
// 旧版 extract 塞的 evidence 是「当轮素材 id」：640 个里 475 个指向不存在的
// 条目（抽样 60 个，磁盘上一个都找不到）。合并后洗成「真实存在的旧锚点 +
// 被并条目 id」——后者是真实条目，可核查，也正好表达「这几条是同一结论的
// 不同表述」。这是让 evidence 第一次真正名副其实。
//
// 用法：
//   node scripts/consolidate-l2.mjs              # dry-run
//   node scripts/consolidate-l2.mjs --apply
//   node scripts/consolidate-l2.mjs --apply --limit 3    # 只做前 3 个合并族
//                                                       # （订正/降级/清噪音一并跳过）

import fs from 'node:fs'
import path from 'node:path'
import { DatabaseSync } from 'node:sqlite'

const APPLY = process.argv.includes('--apply')
const ROOT = process.env.A2A_MEMORY_ROOT || 'C:/Users/yiyiyoyo/.agent2api/memory'
const DB = path.join(ROOT, 'index.db')
const TMP = '.workbuddy/tmp'

const li = process.argv.indexOf('--limit')
const LIMIT = li > 0 ? Number(process.argv[li + 1]) : Infinity

// ── frontmatter ──────────────────────────────────────────────
function split(raw) {
  const m = raw.match(/^---\r?\n([\s\S]*?)\r?\n---\r?\n?/)
  if (!m) return { front: '', body: raw, eol: '\n' }
  const eol = m[0].includes('\r\n') ? '\r\n' : '\n'
  return { front: m[1], body: raw.slice(m[0].length), eol }
}
function get(front, key) {
  const line = front.split(/\r?\n/).find((l) => l.startsWith(key + ':'))
  return line ? line.slice(key.length + 1).trim() : ''
}
function asList(raw) {
  return (raw || '').replace(/^\[|\]$/g, '').split(/[,\s]+/).map((s) => s.trim()).filter(Boolean)
}
function setField(front, key, value) {
  const lines = front.split(/\r?\n/)
  const i = lines.findIndex((l) => l.startsWith(key + ':'))
  if (i >= 0) lines[i] = `${key}: ${value}`
  else lines.push(`${key}: ${value}`)
  return lines.join('\n')
}
function build(front, body, eol) {
  const b = body.startsWith(eol) ? body.slice(eol.length) : body
  return `---${eol}${front}${eol}---${eol}${b}`
}

// ── 订正：这两条存量记忆说 consolidate 有「信息量不得低于最强来源」的校验，
//    但 schedule.rs 的 consolidate() 实际只有「正文≥40字 + 来源≥2条」两道闸。
//    而另一条记忆明确记着「别加这个校验，会废掉合并」。所以是存量写错，
//    要在正文里改成事实，而不是当结论传下去。
const FIXUPS = {
  '20261122-1306-4b77b6': {
    drop: /并加一道校验：合并结果的信息量不得低于它要取代的最强来源，否则拒绝合并，防止「合并」退化成批量删细节。/,
    append:
      '\n\n订正（2026-11-23 复核代码）：`consolidate` 实际只有两道闸——正文 ≥40 字、来源 ≥2 条；没有「合并结果信息量不得低于最强来源」的校验。曾设想过这道校验，但它会拦掉几乎所有合法合并，因为合并的语义本来就是 l2 → 更短的 l2（见 20261122-0701-2ce14d）。',
  },
  '20261122-0701-53ab14': {
    drop: /`consolidate` 合并组时也按信息量挑保留者。/,
    append:
      '\n\n订正（2026-11-23 复核代码）：`consolidate` 不按信息量挑保留者，也没有信息量校验；它只要求正文 ≥40 字、来源 ≥2 条。按信息量裁决的是写入路径上的 `near_duplicate` / `apply_dedup`。',
  },
}

// ── 读最终映射 ───────────────────────────────────────────────
const mapPath = path.join(TMP, '_final_map.json')
if (!fs.existsSync(mapPath)) {
  console.error(`缺少 ${mapPath}，先跑 .workbuddy/tmp/_arbitrate.mjs`)
  process.exit(1)
}
const finalMap = JSON.parse(fs.readFileSync(mapPath, 'utf8'))
const byMap = new Map(finalMap.map((x) => [x.id, x]))

const counts = {}
for (const x of finalMap) counts[x.verdict] = (counts[x.verdict] || 0) + 1
console.log(`映射条目 = ${finalMap.length}  判定分布 = ${JSON.stringify(counts)}`)

// ── 读库 ─────────────────────────────────────────────────────
const db = new DatabaseSync(DB, { readOnly: !APPLY })
const rows = db.prepare(`
  SELECT id, path, title, kind, agent, layer, project, created_at
    FROM entries WHERE deleted_at IS NULL
`).all()
const byId = new Map(rows.map((r) => [r.id, r]))

function loadEntry(id) {
  const r = byId.get(id)
  if (!r || !r.path) return null
  const abs = path.join(ROOT, r.path)
  if (!fs.existsSync(abs)) return null
  const raw = fs.readFileSync(abs, 'utf8')
  const { front, body, eol } = split(raw)
  return { row: r, abs, front, body, eol, ev: asList(get(front, 'evidence')) }
}
function validEvidence(ev) {
  return ev.filter((id) => {
    if (id.startsWith('manual:')) return true
    const e = db.prepare('SELECT deleted_at FROM entries WHERE id=?').get(id)
    return !!e && e.deleted_at == null
  })
}

// ── 按代表聚拢 ───────────────────────────────────────────────
const keeperMap = new Map()   // keeperId -> [mergeId]
for (const x of finalMap) {
  if (x.verdict !== 'merge' || !x.keeper) continue
  if (!keeperMap.has(x.keeper)) keeperMap.set(x.keeper, [])
  keeperMap.get(x.keeper).push(x.id)
}

const keepIds = finalMap.filter((x) => x.verdict === 'keep').map((x) => x.id)
const demotes = finalMap.filter((x) => x.verdict === 'demote')
const noises = finalMap.filter((x) => x.verdict === 'noise')

const l2now = rows.filter((r) => r.layer === 'l2').length
const mergeTotal = [...keeperMap.values()].reduce((s, v) => s + v.length, 0)
const demoteL2 = demotes.filter((d) => byId.get(d.id)?.layer === 'l2').length
const noiseL2 = noises.filter((d) => byId.get(d.id)?.layer === 'l2').length

console.log(`\n模式: ${APPLY ? 'APPLY（真写）' : 'DRY-RUN（只读）'}`)
console.log(`当前 l2 = ${l2now}`)
console.log(`代表 = ${keepIds.length}，并入 = ${mergeTotal}，降级 = ${demotes.length}，清噪音 = ${noises.length}`)
console.log(`预计治理后 l2 = ${l2now - mergeTotal - demoteL2 - noiseL2}`)

// ── 打印计划 ─────────────────────────────────────────────────
const dump = fs.readFileSync(path.join(TMP, '_l2_dump.txt'), 'utf8').split(/\n(?=### )/).filter((b) => b.trim())
const titleOf = (id) => {
  const b = dump.find((x) => x.includes(id))
  return b ? (b.match(/TITLE: (.*)/) || [, '?'])[1] : '?'
}

console.log('\n================ 合并计划（按族）================')
const byTopic = new Map()
for (const x of finalMap) {
  if (!byTopic.has(x.topic)) byTopic.set(x.topic, [])
  byTopic.get(x.topic).push(x)
}
for (const [topic, list] of [...byTopic.entries()].sort((a, b) => b[1].length - a[1].length)) {
  const keeps = list.filter((x) => x.verdict === 'keep')
  const merges = list.filter((x) => x.verdict === 'merge')
  if (!merges.length) continue
  console.log(`\n【${topic}】并入 ${merges.length} 条 → ${keeps.map((k) => k.id).join(', ')}`)
  console.log(`   代表：${titleOf(keeps[0]?.id)}`)
  for (const m of merges.slice(0, 6)) console.log(`     ← ${m.id}  ${titleOf(m.id).slice(0, 46)}`)
  if (merges.length > 6) console.log(`     … 其余 ${merges.length - 6} 条`)
}

if (demotes.length) {
  console.log('\n================ 降级回 l1（不是结论）================')
  for (const d of demotes) console.log(`  ${d.id}  ${titleOf(d.id).slice(0, 56)}`)
}
if (noises.length) {
  console.log('\n================ 清噪音 ================')
  for (const d of noises) console.log(`  ${d.id}  ${titleOf(d.id).slice(0, 56)}`)
}
const fixups = finalMap.filter((x) => x.fixup)
if (fixups.length) {
  console.log('\n================ 订正与代码不符的结论 ================')
  for (const f of fixups) console.log(`  ${f.id}  ${titleOf(f.id).slice(0, 56)}`)
}

if (!APPLY) {
  console.log('\n未写任何文件。')
  process.exit(0)
}

// ── 执行 ─────────────────────────────────────────────────────
const now = Date.now()
let nMerge = 0, nDemote = 0, nNoise = 0, nFix = 0
const doneKeepers = new Set()

// 1) 合并族
for (const [keeperId, mergeIds] of [...keeperMap.entries()].slice(0, LIMIT)) {
  const keeper = loadEntry(keeperId)
  if (!keeper) { console.log(`⚠ 代表 ${keeperId} 读不到，跳过`); continue }
  doneKeepers.add(keeperId)

  // 1a) 正文：追加合并要点（去重按内容）
  const seen = new Set()
  const lines = []
  for (const mid of mergeIds) {
    const meta = byMap.get(mid)
    const pts = (meta?.points || []).map((s) => String(s).trim()).filter(Boolean)
    if (!pts.length) continue
    const fresh = pts.filter((p) => !seen.has(p))
    if (!fresh.length) continue
    fresh.forEach((p) => seen.add(p))
    lines.push(`- ${titleOf(mid)}（来源 ${mid}）`)
    for (const p of fresh) lines.push(`  - ${p}`)
  }
  let body = keeper.body
  if (lines.length) {
    if (body.includes('## 合并要点')) body = body.trimEnd() + '\n' + lines.join('\n') + '\n'
    else body = body.trimEnd() + '\n\n## 合并要点\n\n' + lines.join('\n') + '\n'
  }

  // 1b) 订正（若有）
  let fixNote = ''
  const fx = FIXUPS[keeperId]
  if (fx) {
    body = body.replace(fx.drop, '')
    fixNote = fx.append
    body = body.trimEnd() + fixNote + '\n'
    nFix++
  }

  // 1c) 证据：有效旧锚点 + 被并条目 id
  const nextEv = [...new Set([...validEvidence(keeper.ev), ...mergeIds])].slice(0, 20)
  let front = setField(keeper.front, 'evidence', `[${nextEv.join(', ')}]`)
  front = setField(front, 'updated_at', String(now))
  fs.writeFileSync(keeper.abs, build(front, body, keeper.eol))
  db.prepare('UPDATE entries SET updated_at=? WHERE id=?').run(now, keeperId)

  // 1d) 被并条目：标 superseded_by
  for (const mid of mergeIds) {
    const e = loadEntry(mid)
    if (!e) continue
    let f = setField(e.front, 'superseded_by', keeperId)
    f = setField(f, 'dedup_status', 'merged')
    f = setField(f, 'updated_at', String(now))
    fs.writeFileSync(e.abs, build(f, e.body, e.eol))
    db.prepare('UPDATE entries SET superseded_by=?, dedup_status=?, updated_at=? WHERE id=?')
      .run(keeperId, 'merged', now, mid)
    nMerge++
  }
}

// 2) 独立 keep 的订正（没有合并族的那些）
// --limit 时必须一起收敛：否则「只做 1 个族」的试跑会顺手把全部订正、降级、
// 清噪音都写进真库，试跑就不再是试跑。
if (LIMIT !== Infinity && (fixups.some((f) => !doneKeepers.has(f.id)) || demotes.length || noises.length)) {
  console.log('⚠ --limit 生效：本轮跳过独立订正 / 降级 / 清噪音，只处理前 N 个合并族。')
}
for (const f of LIMIT === Infinity ? fixups : []) {
  if (doneKeepers.has(f.id)) continue
  const e = loadEntry(f.id)
  if (!e) continue
  const fx = FIXUPS[f.id]
  let body = e.body.replace(fx.drop, '')
  body = body.trimEnd() + fx.append + '\n'
  const front = setField(e.front, 'updated_at', String(now))
  fs.writeFileSync(e.abs, build(front, body, e.eol))
  db.prepare('UPDATE entries SET updated_at=? WHERE id=?').run(now, f.id)
  nFix++
}

// 3) 降级：layer l1 + 清 path（下次启动按 l1 容器规则重算落点）
for (const d of LIMIT === Infinity ? demotes : []) {
  const e = loadEntry(d.id)
  if (!e) continue
  let f = setField(e.front, 'layer', 'l1')
  f = setField(f, 'path', '')
  f = setField(f, 'updated_at', String(now))
  fs.writeFileSync(e.abs, build(f, e.body, e.eol))
  db.prepare('UPDATE entries SET layer=?, path=?, updated_at=? WHERE id=?').run('l1', '', now, d.id)
  nDemote++
}

// 4) 噪音：搬进 trash
const trashDir = path.join(ROOT, 'trash')
if (!fs.existsSync(trashDir)) fs.mkdirSync(trashDir, { recursive: true })
for (const d of LIMIT === Infinity ? noises : []) {
  const e = loadEntry(d.id)
  if (!e) continue
  let f = setField(e.front, 'deleted_at', String(now))
  f = setField(f, 'updated_at', String(now))
  fs.writeFileSync(path.join(trashDir, `${d.id}.md`), build(f, e.body, e.eol))
  fs.unlinkSync(e.abs)
  db.prepare('UPDATE entries SET deleted_at=?, path=?, updated_at=? WHERE id=?')
    .run(now, `trash/${d.id}.md`, now, d.id)
  nNoise++
}

console.log(`\n已执行：合并 ${nMerge} 条，降级 ${nDemote} 条，清噪音 ${nNoise} 条，订正 ${nFix} 条。`)
console.log('下一步：POST /api/memory/rebuild 或重启软件，让 .md 事实源重建索引。')
