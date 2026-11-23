// 把 index.db 的 body 列对齐到 .md 事实源
//
// ── 为什么需要这一步 ─────────────────────────────────────────
// 主读路径（`store::snapshot()`）读 `.md` 事实源，靠目录指纹失效，所以界面内容
// 已是新的。但检索排序读 `index.db` 的 body / FTS —— 不同步会出现「点开是对的、
// 搜出来是旧的」。程序下次 rebuild 会自己对齐，但用户不一定立刻重启。
//
// ── 为什么只处理 l2 独立文件 ─────────────────────────────────
// l1 是「一天一容器」的共享文件：索引里一行的 body 是**那一个节**，而文件里是
// 整份容器（实测有个 105KB 的文件，索引里只有 2001 字节的那一节）。按文件写回
// 会把整份容器塞进一行的 body，索引立刻变成垃圾。l1 的 body 必须由程序自己
// 按节解析，这里不碰。
//
// ── 为什么只在实质不同时写 ───────────────────────────────────
// 40 条只是尾部换行/空白的差异，写回去没有信息增益，还会让全库 updated_at
// 一起跳。只有真正改了正文的（本次 22 条代表）才需要同步。
import fs from 'node:fs'
import path from 'node:path'
import { DatabaseSync } from 'node:sqlite'

const APPLY = process.argv.includes('--apply')
const ROOT = process.env.A2A_MEMORY_ROOT || 'C:/Users/yiyiyoyo/.agent2api/memory'
const db = new DatabaseSync(path.join(ROOT, 'index.db'), { readOnly: !APPLY })

function split(raw) {
  const m = raw.match(/^---\r?\n([\s\S]*?)\r?\n---\r?\n?/)
  if (!m) return { front: '', body: raw }
  return { front: m[1], body: raw.slice(m[0].length) }
}
function get(front, key) {
  const line = front.split(/\r?\n/).find((l) => l.startsWith(key + ':'))
  return line ? line.slice(key.length + 1).trim() : ''
}

const rows = db.prepare(`
  SELECT id, path, body, layer, superseded_by FROM entries WHERE layer='l2'
`).all()

let exact = 0, trimOnly = 0, noFile = 0
const real = []

for (const r of rows) {
  if (!r.path) { noFile++; continue }
  const abs = path.join(ROOT, r.path)
  if (!fs.existsSync(abs)) { noFile++; continue }
  const { front, body } = split(fs.readFileSync(abs, 'utf8'))
  if (body === r.body) { exact++; continue }
  if (body.trim() === String(r.body || '').trim()) { trimOnly++; continue }
  real.push({
    id: r.id,
    body,
    sup: get(front, 'superseded_by') || null,
    dedup: get(front, 'dedup_status') || null,
  })
}

console.log(`模式: ${APPLY ? 'APPLY' : 'DRY-RUN'}`)
console.log(`l2 行 = ${rows.length}：一致 ${exact} / 仅空白差 ${trimOnly} / 实质不同 ${real.length} / 无文件 ${noFile}`)

if (!APPLY) {
  for (const r of real.slice(0, 10)) console.log(`  ${r.id}`)
  console.log('\n未写。')
  process.exit(0)
}

const upd = db.prepare('UPDATE entries SET body=?, superseded_by=?, dedup_status=? WHERE id=?')
db.exec('BEGIN')
for (const r of real) upd.run(r.body, r.sup, r.dedup, r.id)
db.exec('COMMIT')
console.log(`\n已同步 ${real.length} 行 body。`)
console.log('注：FTS 索引由程序的 rebuild 重建；这里只对齐行数据，避免「点开对、搜出旧」。')
