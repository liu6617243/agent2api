// 合并后修补：吸收了 preference 的代表必须是「必须遵守」的候选
//
// ── 为什么需要这一步 ─────────────────────────────────────────
// `inject::rule_candidate()` 的准入是：
//     entry.pinned || matches!(entry.kind, Preference | Profile)
// 也就是说，一条结论要进每轮自动注入的「必须遵守」块，只有两条路：类型是
// 偏好/画像，或者被钉住。
//
// 本次合并把 9 条 preference（「只用 GitHub Actions 构建、唯一交付入口
// deliver.mjs」等）并进了一条 `decision` 类型的代表，把 4 条 preference
// （「界面问题必须看真实窗口」）并进了一条 `pitfall` 类型的代表。
// 它们的内容进了代表正文，但代表的 kind 不是 preference —— 那条用户硬要求
// 就从「必须遵守」块里消失了。
//
// 这正是用户最在意的那条：「我提过的要求不能忘」。所以这里给吸收了
// preference 的代表补 `pinned`：pinned 是 rule_candidate 的独立准入条件，
// 与 kind 并列，不改变条目的语义类型。
import fs from 'node:fs'
import path from 'node:path'
import { DatabaseSync } from 'node:sqlite'

const APPLY = process.argv.includes('--apply')
const ROOT = process.env.A2A_MEMORY_ROOT || 'C:/Users/yiyiyoyo/.agent2api/memory'
const db = new DatabaseSync(path.join(ROOT, 'index.db'), { readOnly: !APPLY })

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

// 吸收了多少条 preference
const absorbed = new Map(
  db.prepare(`
    SELECT superseded_by, count(*) c FROM entries
     WHERE superseded_by IS NOT NULL AND deleted_at IS NULL AND kind='preference'
     GROUP BY superseded_by
  `).all().map((x) => [x.superseded_by, x.c]),
)

const keepers = db.prepare(`
  SELECT id, path, kind, pinned, title FROM entries
   WHERE layer='l2' AND deleted_at IS NULL AND superseded_by IS NULL
`).all()

const todo = []
for (const k of keepers) {
  const c = absorbed.get(k.id)
  if (!c) continue
  const alreadyRule = k.pinned === 1 || k.kind === 'preference' || k.kind === 'profile'
  if (alreadyRule) continue
  todo.push({ ...k, absorbed: c })
}

console.log(`模式: ${APPLY ? 'APPLY' : 'DRY-RUN'}`)
console.log(`吸收 preference 但不能进规则块的代表 = ${todo.length}`)
for (const t of todo) {
  console.log(`  ${t.id} [${t.kind}] 吸收 ${t.absorbed} 条 preference`)
  console.log(`     ${String(t.title).slice(0, 56)}`)
}

if (!APPLY) {
  console.log('\n未写。')
  process.exit(0)
}

const now = Date.now()
const upd = db.prepare('UPDATE entries SET pinned=1, updated_at=? WHERE id=?')
for (const t of todo) {
  const abs = path.join(ROOT, t.path)
  if (!fs.existsSync(abs)) { console.log(`⚠ ${t.id} 文件缺失，跳过`); continue }
  const { front, body, eol } = split(fs.readFileSync(abs, 'utf8'))
  let f = setField(front, 'pinned', 'true')
  f = setField(f, 'updated_at', String(now))
  fs.writeFileSync(abs, build(f, body, eol))
  upd.run(now, t.id)
}
console.log(`\n已钉住 ${todo.length} 条代表。`)
