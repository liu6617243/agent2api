#!/usr/bin/env node
// l2 存量语义聚类 + 合并方案 dry-run。
//
// 只读 C:/Users/yiyiyoyo/.agent2api/memory/index.db，绝不写库、绝不改任何 .md。
// 真实合并留给后续流程（pending review），本脚本只产出「谁保留、谁被合并」的方案。
//
// 判据：对筛选出的条目做 TF-IDF 向量化 + 平均链接凝聚聚类。
//   - 中文用字符 bigram（无需分词器），ASCII/数字（版本号、工具名）单独成词。
//   - 相似度用余弦，阈值默认 0.34，可用 --threshold 调。
//   - 每簇选「信息最完整」的一条作保留者，其余列为被合并者。
//   - 两轮归并：第一轮按全文（标题/summary/tags/body）聚类；
//     第二轮把第一轮保留者的「标题」再做一次同样的相似度计算并簇级归并，
//     反复到不再变化（最多 3 轮），消灭「同一件事换说法」被拆成多簇的问题。
//
// 用法：
//   node scripts/merge-duplicate-memory.mjs                 # dry-run，打印摘要 + 前 5 簇
//   node scripts/merge-duplicate-memory.mjs --top 10         # 多打印几簇
//   node scripts/merge-duplicate-memory.mjs --threshold 0.4  # 调相似度阈值
//   node scripts/merge-duplicate-memory.mjs --json           # 完整方案以 JSON 输出
//   node scripts/merge-duplicate-memory.mjs --db <path>      # 指定库路径
//   node scripts/merge-duplicate-memory.mjs --singletons 20  # 打印孤例标题条数（默认 20）

import { DatabaseSync } from 'node:sqlite'

const argv = process.argv.slice(2)
const has = (f) => argv.includes(f)
const val = (f, d) => {
  const i = argv.indexOf(f)
  return i >= 0 && argv[i + 1] ? argv[i + 1] : d
}
const num = (f, d) => {
  const v = val(f, '')
  const n = Number(v)
  return v !== '' && Number.isFinite(n) ? n : d
}

// Git Bash 下 node 拿到 /c/... 会解析成 D:\c\...，因此用 Windows 风格盘符路径。
const DB = val('--db', 'C:/Users/yiyiyoyo/.agent2api/memory/index.db')
const THRESHOLD = num('--threshold', 0.34)
const TOP = Math.max(1, Math.floor(num('--top', 5)))
const JSON_OUT = has('--json')
const SINGLETON_SHOW = Math.max(0, Math.floor(num('--singletons', 20)))

// ── 只读打开 ───────────────────────────────────────────────────
const db = new DatabaseSync(DB, { readOnly: true })
const rows = db.prepare(`
  SELECT id, project, agent, title, summary, tags, body,
         importance, pinned, starred, created_at, updated_at,
         access_count, last_hit_at
  FROM entries
  WHERE layer = 'l2'
    AND deleted_at IS NULL
    AND superseded_by IS NULL
    AND agent = 'auto-extract'
  ORDER BY updated_at DESC
`).all()
db.close()

// ── 文本向量化 ─────────────────────────────────────────────────
// 中文按字符 bigram 切（"固定构建" -> 固定/定构/构建），ASCII/数字按词切。
const tokenize = (text) => {
  const s = String(text ?? '').toLowerCase()
  const out = []
  const ascii = s.match(/[a-z0-9][a-z0-9._\-/]*/g) || []
  for (const t of ascii) out.push(t)
  const cjk = s.match(/[\u4e00-\u9fff]+/g) || []
  for (const run of cjk) {
    if (run.length === 1) { out.push(run); continue }
    for (let i = 0; i < run.length - 1; i++) out.push(run.slice(i, i + 2))
  }
  return out
}

function tfOf(tokens) {
  const m = new Map()
  for (const t of tokens) m.set(t, (m.get(t) || 0) + 1)
  const n = tokens.length || 1
  for (const [k, v] of m) m.set(k, v / n)
  return m
}

// 给定一组 token 数组，构造这组文档自己的 TF-IDF 向量（idf 在该组上计算）。
function vectorsFromTokens(tokenArrays) {
  const local = tokenArrays.map((t) => ({ tf: tfOf(t) }))
  const df = new Map()
  for (const d of local) for (const t of d.tf.keys()) df.set(t, (df.get(t) || 0) + 1)
  const n = local.length || 1
  const idf = new Map()
  for (const [t, c] of df) idf.set(t, Math.log((1 + n) / (1 + c)) + 1)
  return local.map((d) => {
    const v = new Map()
    let norm = 0
    for (const [t, tf] of d.tf) {
      const w = tf * idf.get(t)
      v.set(t, w)
      norm += w * w
    }
    norm = Math.sqrt(norm) || 1
    for (const [t, w] of v) v.set(t, w / norm)
    return v
  })
}

const cos = (a, b) => {
  let s = 0
  const [small, big] = a.size <= b.size ? [a, b] : [b, a]
  for (const [t, w] of small) {
    const w2 = big.get(t)
    if (w2 !== undefined) s += w * w2
  }
  return s
}

// ── 平均链接凝聚聚类 ───────────────────────────────────────────
// 输入向量数组与阈值，返回 { clusters: number[][], sim: number[][] }。
// clusters 内为向量下标；sim 为两两三两余弦相似度矩阵。
function agglomerate(vecs, threshold) {
  const n = vecs.length
  if (n === 0) return { clusters: [], sim: [] }
  const sim = Array.from({ length: n }, () => new Float64Array(n))
  for (let i = 0; i < n; i++) {
    for (let j = i + 1; j < n; j++) {
      const s = cos(vecs[i], vecs[j])
      sim[i][j] = s
      sim[j][i] = s
    }
  }
  let clusters = vecs.map((_, i) => [i])
  const avgSim = (ci, cj) => {
    let s = 0
    for (const i of ci) for (const j of cj) s += sim[i][j]
    return s / (ci.length * cj.length)
  }
  for (;;) {
    let best = null
    for (let a = 0; a < clusters.length; a++) {
      for (let b = a + 1; b < clusters.length; b++) {
        const s = avgSim(clusters[a], clusters[b])
        if (s >= threshold && (!best || s > best.s)) best = { a, b, s }
      }
    }
    if (!best) break
    const merged = [...clusters[best.a], ...clusters[best.b]]
    clusters = clusters.filter((_, i) => i !== best.a && i !== best.b)
    clusters.push(merged)
  }
  return { clusters, sim }
}

// ── 每簇选保留者（信息最完整者）───────────────────────────────
const bodyLen = (r) => String(r.body ?? '').length
const tagCount = (r) => String(r.tags ?? '').split(/\s+/).filter(Boolean).length
const keeperScore = (r) =>
  Number(r.importance || 0.5) * 3 +
  (r.starred ? 1.5 : 0) +
  (r.pinned ? 1 : 0) +
  Math.min(bodyLen(r), 800) / 800 * 2 +
  Math.min(tagCount(r), 8) / 8 +
  Math.log1p(Number(r.access_count || 0)) * 0.2

const pickKeeper = (memberRows) =>
  memberRows
    .map((r) => ({ r, score: keeperScore(r) }))
    .sort((a, b) => b.score - a.score || bodyLen(b.r) - bodyLen(a.r))[0].r

// ── 第一轮：全文聚类 ───────────────────────────────────────────
// 标题权重最高（重复 3 次），summary/tags 次之，body 覆盖细节。
const fullTokens = rows.map((r) => [
  ...tokenize(r.title), ...tokenize(r.title), ...tokenize(r.title),
  ...tokenize(r.summary), ...tokenize(r.summary),
  ...tokenize(r.tags), ...tokenize(r.tags),
  ...tokenize(r.body),
])
const fullVecs = vectorsFromTokens(fullTokens)
const N = rows.length
const round1 = agglomerate(fullVecs, THRESHOLD)
let clusters = round1.clusters
const sim = round1.sim // 全文相似度矩阵，供最终方案复用

// ── 第二轮：簇级归并（按保留者标题相似度）─────────────────────
// 每轮重新选各簇保留者，只用标题算 TF-IDF 余弦，再平均链接聚类；
// 归并后展开回原始文档下标。反复到没有合并或到 MAX_ROUNDS 轮。
const MAX_ROUNDS = 3
const roundLog = []
for (let round = 1; round <= MAX_ROUNDS && clusters.length > 1; round++) {
  const before = clusters.length
  const keepers = clusters.map((c) => pickKeeper(c.map((i) => rows[i])))
  const keeperVecs = vectorsFromTokens(keepers.map((k) => tokenize(k.title)))
  const merged = agglomerate(keeperVecs, THRESHOLD).clusters
  if (merged.length === before) {
    roundLog.push({ round, before, after: before, changed: false })
    break
  }
  clusters = merged.map((kc) => kc.flatMap((k) => clusters[k]))
  roundLog.push({ round, before, after: clusters.length, changed: true })
}

// ── 生成方案 ───────────────────────────────────────────────────
const plan = clusters
  .map((idx) => {
    const members = idx.map((i) => rows[i])
    const scored = members
      .map((r) => ({ r, score: keeperScore(r) }))
      .sort((a, b) => b.score - a.score || bodyLen(b.r) - bodyLen(a.r))
    const keeper = scored[0].r
    const merges = scored.slice(1).map(({ r }) => {
      const ki = idx.find((i) => rows[i].id === keeper.id)
      const mi = idx.find((i) => rows[i].id === r.id)
      return {
        id: r.id,
        title: r.title,
        project: r.project,
        similarity: Number(sim[ki][mi].toFixed(3)),
        reason: '与保留者语义重复，正文信息已被覆盖',
      }
    })
    let sum = 0
    let cnt = 0
    for (let a = 0; a < idx.length; a++) for (let b = a + 1; b < idx.length; b++) { sum += sim[idx[a]][idx[b]]; cnt++ }
    return {
      size: idx.length,
      avgSimilarity: cnt ? Number((sum / cnt).toFixed(3)) : 0,
      keeper: { id: keeper.id, title: keeper.title, project: keeper.project, bodyLen: bodyLen(keeper) },
      merges,
    }
  })
  .filter((c) => c.merges.length > 0)
  .sort((a, b) => b.size - a.size || b.avgSimilarity - a.avgSimilarity)

// ── 统计 ───────────────────────────────────────────────────────
const total = N
const clusterCount = plan.length
const inClusters = plan.reduce((s, c) => s + c.size, 0)
const removed = plan.reduce((s, c) => s + c.merges.length, 0)
const after = total - removed
const ratio = after ? total / after : 0
const singletons = total - inClusters
const avgPerCluster = clusterCount ? Number((inClusters / clusterCount).toFixed(2)) : 0
const mergedIds = new Set(plan.flatMap((c) => c.merges.map((m) => m.id)))
const singletonTitles = rows.filter((r) => !mergedIds.has(r.id) && !plan.some((c) => c.keeper.id === r.id)).map((r) => r.title)

if (JSON_OUT) {
  process.stdout.write(JSON.stringify({
    db: DB, threshold: THRESHOLD, total, clusterCount, inClusters,
    singletons, removed, after, avgPerCluster,
    compressionRatio: Number(ratio.toFixed(3)), roundLog,
    singletonTitles, plan,
  }, null, 2) + '\n')
} else {
  console.log('=== l2 存量语义聚类 dry-run ===')
  console.log(`筛选条件: layer=l2 AND deleted_at IS NULL AND superseded_by IS NULL AND agent='auto-extract'`)
  console.log(`相似度阈值: ${THRESHOLD}`)
  console.log('')
  console.log(`总条数        : ${total}`)
  console.log(`重复簇数      : ${clusterCount}`)
  console.log(`簇内条目      : ${inClusters} (单条未聚类 ${singletons})`)
  console.log(`每簇平均条数  : ${avgPerCluster}`)
  console.log(`将被合并      : ${removed}`)
  console.log(`合并后剩余    : ${after}`)
  console.log(`压缩比        : ${ratio.toFixed(2)}x`)
  console.log(`二级归并轮次  : ${roundLog.map((r) => `R${r.round} ${r.before}->${r.after}${r.changed ? '' : ' (不变)'}`).join(', ') || '未触发'}`)
  console.log('')
  console.log(`--- 前 ${TOP} 个簇（按簇大小 / 平均相似度排序）---`)
  for (const [i, c] of plan.slice(0, TOP).entries()) {
    console.log(`\n[${i + 1}] 簇大小 ${c.size} / 平均相似度 ${c.avgSimilarity}`)
    console.log(`  保留: ${c.keeper.title}  (${c.keeper.id}, ${c.keeper.bodyLen} 字)`)
    for (const m of c.merges) {
      console.log(`  合并: ${m.title}  (${m.id}, sim=${m.similarity})`)
    }
  }
  console.log('')
  console.log(`--- 孤例标题（单条成簇，最多 ${SINGLETON_SHOW} 条；共 ${singletonTitles.length} 条）---`)
  for (const [i, t] of singletonTitles.slice(0, SINGLETON_SHOW).entries()) {
    console.log(`  ${i + 1}. ${t}`)
  }
  console.log('\n(dry-run：未写库、未改任何 .md)')
}
