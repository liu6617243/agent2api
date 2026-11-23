// L2 治理收尾：让「必须遵守」块真的装下用户的每一条硬要求
//
// ── 这一轮解决的两个真问题 ────────────────────────────────────
//
// 1) 降级条目留在 l2 目录里
//    `consolidate-l2.mjs` 把三条降级条目改成 `layer: l1` 并清空了 `path`，
//    但文件本身还在 `general/l2/summary/` 下。事实源按**文件路径形状**读，
//    layer 取自 frontmatter —— 计数是对的（l1），可磁盘上 l1 条目躺在 l2 目录，
//    下次谁按目录找「深层记忆」都会多数出三条。
//    这里按 `layout::memory_rel_path` 的同一套规则把它们搬到
//    `<base>/l1/<agent>/<created 日期>-<id>.md`。
//
// 2) 硬要求被 56 字截断吃掉
//    「必须遵守」块渲染的是 `标题 + 摘要`，两段各截 56 字。合并后的两个代表
//    标题/摘要太短，`本机不装 Rust`、`不下载安装包`、`长输出拆小调用` 这三条
//    用户明确说过的要求只存在于正文里 —— 而正文不进注入块。
//    记录一条要求却看不见它，等于没记。这里把这三条挪进标题/摘要。
//
// 用法：
//   node scripts/finalize-l2-governance.mjs            # dry-run
//   node scripts/finalize-l2-governance.mjs --apply
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

// ── 1) 降级条目的落点 ────────────────────────────────────────
const DEMOTED = ['20261122-0933-eb06d7', '20261121-0226-566c4e', '20261122-1034-b93c56']

function ymd(ms) {
  const d = new Date(ms)
  const p = (n) => String(n).padStart(2, '0')
  return `${d.getFullYear()}-${p(d.getMonth() + 1)}-${p(d.getDate())}`
}

const moves = []
for (const id of DEMOTED) {
  const row = db.prepare('SELECT id, path, layer, agent, kind, created_at FROM entries WHERE id=?').get(id)
  if (!row) { console.log(`⚠ ${id} 不在库里，跳过`); continue }
  if (row.layer !== 'l1') { console.log(`⚠ ${id} 仍是 ${row.layer}，不搬`); continue }
  if (row.path.includes('/l1/')) { console.log(`· ${id} 已在 l1 目录，无需搬`); continue }
  const base = row.path.startsWith('projects/') ? row.path.split('/').slice(0, 2).join('/') : 'general'
  const agent = (row.agent || 'manual').replace(/[^a-z0-9_-]+/gi, '-')
  const to = `${base}/l1/${agent}/${ymd(row.created_at)}-${id}.md`
  moves.push({ id, from: row.path, to })
}

console.log(`模式: ${APPLY ? 'APPLY' : 'DRY-RUN'}`)
console.log(`待搬 ${moves.length} 条：`)
for (const m of moves) console.log(`  ${m.id}\n    ${m.from}\n →  ${m.to}`)

// ── 2) 让硬要求出现在注入行里 ─────────────────────────────────
//
// 截断宽度是 56 字（`inject::line`）。标题与摘要都必须整句装得下，
// 否则最后那几条要求会被静默截掉 —— 那是「记得住但看不见」。
const REWRITE = {
  // 「CI 唯一权威」那条：补「本机不装 Rust」—— 它是用户反复强调的约束，
  // 之前只在正文里
  '20261121-1037-7938ba': {
    summary: 'CI 唯一权威，本机不装 Rust；交付只跑 deliver.mjs，版本锁 3.5.0-beta.1',
  },
  // 「长期协作与记忆约束」那条：标题改成要求本身。原标题「长期协作与记忆约束」
  // 是一句分类名，模型看了不知道要守什么；把三条具体禁令写进标题才拦得住人
  '20261122-1004-931b21': {
    title: '不下载安装包、长输出拆小调用、版本固定只走 CI',
    summary: '用户明确要求：版本号固定不 bump，构建测试只走 CI，交付只跑 deliver.mjs',
  },
}

const edits = []
for (const [id, patch] of Object.entries(REWRITE)) {
  const row = db.prepare('SELECT id, path, title, summary FROM entries WHERE id=?').get(id)
  if (!row) { console.log(`⚠ ${id} 不在库里`); continue }
  edits.push({ id, abs: path.join(ROOT, row.path), patch, before: { title: row.title, summary: row.summary } })
}
console.log(`\n待改标题/摘要 ${edits.length} 条：`)
for (const e of edits) {
  console.log(`  ${e.id}`)
  if (e.patch.title) console.log(`    标题  ${e.before.title}\n       →  ${e.patch.title}`)
  if (e.patch.summary) console.log(`    摘要  ${e.before.summary}\n       →  ${e.patch.summary}`)
}

if (!APPLY) {
  console.log('\n未写。')
  process.exit(0)
}

const now = Date.now()
const updPath = db.prepare('UPDATE entries SET path=?, updated_at=? WHERE id=?')
for (const m of moves) {
  const from = path.join(ROOT, m.from)
  const to = path.join(ROOT, m.to)
  if (!fs.existsSync(from)) { console.log(`⚠ ${m.id} 源文件缺失：${m.from}`); continue }
  fs.mkdirSync(path.dirname(to), { recursive: true })
  fs.renameSync(from, to)
  const { front, body, eol } = split(fs.readFileSync(to, 'utf8'))
  // frontmatter 里的 `path` 是空串（降级时清掉的）：清空后由程序按 layer 重算。
  // 这里保持空，让「文件在哪」始终是唯一事实。
  fs.writeFileSync(to, build(setField(front, 'updated_at', String(now)), body, eol))
  updPath.run(m.to, now, m.id)
  console.log(`✔ 搬走 ${m.id} → ${m.to}`)
}

for (const e of edits) {
  if (!fs.existsSync(e.abs)) { console.log(`⚠ ${e.id} 文件缺失：${e.abs}`); continue }
  const { front, body, eol } = split(fs.readFileSync(e.abs, 'utf8'))
  let f = front
  if (e.patch.title) f = setField(f, 'title', e.patch.title)
  if (e.patch.summary) f = setField(f, 'summary', e.patch.summary)
  f = setField(f, 'updated_at', String(now))
  fs.writeFileSync(e.abs, build(f, body, eol))
  console.log(`✔ 改好 ${e.id}`)
}
console.log('\n下一步：重建索引（POST /api/memory/rebuild 或重启软件）。')