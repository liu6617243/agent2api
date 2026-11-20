// 校验 .github/workflows/*.yml 的 YAML 结构与关键字段
// 本机没有 yq / actionlint，用最小实现做基本解析 + 规则检查。
import fs from 'node:fs'
import path from 'node:path'

const DIR = '.github/workflows'
const files = fs.readdirSync(DIR).filter((f) => f.endsWith('.yml') || f.endsWith('.yaml'))

let problems = 0
const say = (m) => { console.log('  ' + m) }

for (const f of files) {
  const text = fs.readFileSync(path.join(DIR, f), 'utf8')
  const lines = text.split('\n')

  console.log(`${f}`)

  // 1. 制表符：YAML 缩进里 tab 是硬错误
  const tabLines = lines.map((l, i) => [i + 1, l]).filter(([, l]) => /^\s*\t/.test(l))
  if (tabLines.length) {
    console.log('  FAIL 含制表符缩进：', tabLines.map(([n]) => n).join(', '))
    problems++
  }

  // 2. 顶层键与 jobs 存在性
  const hasOn = /^on:\s*$/m.test(text) || /^on:\s*\S/m.test(text)
  const hasJobs = /^jobs:\s*$/m.test(text)
  if (!hasOn) { say('FAIL 缺 on:'); problems++ }
  if (!hasJobs) { say('FAIL 缺 jobs:'); problems++ }

  // 3. jobs 下每个 job 都要有 runs-on 与 steps
  const jobBlocks = []
  let cur = null
  let inJobs = false
  for (const line of lines) {
    if (/^jobs:\s*$/.test(line)) { inJobs = true; continue }
    if (inJobs && /^[A-Za-z]/.test(line)) { inJobs = false }
    if (!inJobs) continue
    const m = line.match(/^ {2}([A-Za-z0-9_-]+):\s*$/)
    if (m) { cur = { name: m[1], body: [] }; jobBlocks.push(cur); continue }
    if (cur) cur.body.push(line)
  }
  if (!jobBlocks.length) { say('FAIL jobs 下没解析出 job'); problems++ }
  for (const j of jobBlocks) {
    const b = j.body.join('\n')
    if (!/^\s*runs-on:/m.test(b)) { say(`FAIL job ${j.name} 缺 runs-on`); problems++ }
    if (!/^\s*steps:/m.test(b)) { say(`FAIL job ${j.name} 缺 steps`); problems++ }
    if (/needs:/m.test(b) && !/^\s*needs:/m.test(b)) { say(`FAIL job ${j.name} 的 needs 缩进不对`); problems++ }
  }

  // 4. needs 指向的 job 必须存在
  const jobNames = new Set(jobBlocks.map((j) => j.name))
  for (const j of jobBlocks) {
    const needsMatch = j.body.join('\n').match(/^\s*needs:\s*(.+)$/m)
    if (!needsMatch) continue
    for (const n of needsMatch[1].split(',').map((s) => s.trim()).filter(Boolean)) {
      if (!jobNames.has(n)) { say(`FAIL job ${j.name} 的 needs 指向不存在的 job: ${n}`); problems++ }
    }
  }

  // 5. ${{ }} 表达式括号配平
  const opens = (text.match(/\$\{\{/g) || []).length
  const closes = (text.match(/\}\}/g) || []).length
  if (opens !== closes) { say(`FAIL ${{ }} 不配平：${opens} 开 / ${closes} 闭`); problems++ }

  // 6. 每个 uses: 的 action 引用格式
  for (const [i, l] of lines.entries()) {
    const m = l.match(/^\s*uses:\s*(\S+)/)
    if (!m) continue
    const ref = m[1]
    if (!/^[^\s@]+@[^\s@]+$/.test(ref) && !ref.startsWith('./')) {
      say(`FAIL 第 ${i + 1} 行 uses 引用格式异常: ${ref}`)
      problems++
    }
  }

  console.log(`  jobs: ${jobNames.size} 个 — ${[...jobNames].join(', ')}`)
}

console.log(problems ? `\n发现 ${problems} 个问题。` : '\n工作流 YAML 检查通过。')
process.exit(problems ? 1 : 0)
