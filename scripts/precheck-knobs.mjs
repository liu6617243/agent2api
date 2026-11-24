#!/usr/bin/env node
// 旋钮 schema 预检：抓 `knob(...)` / `knob_d(...)` 的参数错位。
//
// ── 为什么需要这道闸 ────────────────────────────────────────
// `knob()` 收 6 个参数、`knob_d()` 收 7 个，两者的差别只在「有没有描述」。
// 于是把描述写在 `knob()` 的第 6 位（也就是默认值位置）时，**代码照样编译**，
// 只是 `default_json` 变成了 `"浏览页每页条数；后端在 limit 缺省时也用这个值"`。
//
// 这种错位不会在编译期炸，要等到跑单测 `every_default_parses` 才显形 ——
// 而那时已经推了一轮 CI，等十分钟换来一句 `serde_json` 解析失败。
// 2026-11-20 的 `ui.pageSize` 就是这么坏的。
//
// ── 判据 ────────────────────────────────────────────────────
//   1. 实参个数必须与构造器签名一致；
//   2. 最后一个实参是 JSON 默认值，不允许出现中文（默认值里出现散文 = 错位）。
//
// 只查这份 schema 文件，纯文本解析，几十毫秒。宁可漏报，不要误报。
import { readFileSync } from 'node:fs'
import path from 'node:path'
import process from 'node:process'
import { fileURLToPath } from 'node:url'

const root = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..')
const file = process.argv[2] || path.join(root, 'desktop-tauri/src-tauri/server/src/server/core/memory/config.rs')

let src
try {
  src = readFileSync(file, 'utf8')
} catch (error) {
  console.error(`precheck:knobs 读不到 ${file}（${error.message}）`)
  process.exit(1)
}

const found = /\b(knob|knob_d)\s*\(/g
let problems = 0
let checked = 0
let match
while ((match = found.exec(src)) !== null) {
  const start = found.lastIndex
  let depth = 1
  let j = start
  while (j < src.length && depth > 0) {
    if (src[j] === '(') depth += 1
    else if (src[j] === ')') depth -= 1
    j += 1
  }
  const body = src.slice(start, j - 1)

  // 顶层逗号切分（跳过字符串里的逗号与括号里的逗号）
  const args = []
  let inner = 0
  let current = ''
  let inString = false
  for (let k = 0; k < body.length; k += 1) {
    const c = body[k]
    if (inString) {
      current += c
      if (c === '\\') {
        current += body[k + 1] ?? ''
        k += 1
        continue
      }
      if (c === '"') inString = false
      continue
    }
    if (c === '"') {
      inString = true
      current += c
      continue
    }
    if (c === '(' || c === '[') inner += 1
    if (c === ')' || c === ']') inner -= 1
    if (c === ',' && inner === 0) {
      args.push(current.trim())
      current = ''
      continue
    }
    current += c
  }
  args.push(current.trim())

  const want = match[1] === 'knob' ? 6 : 7
  const last = args[args.length - 1] ?? ''
  const prose = /[\u4e00-\u9fa5]/.test(last)
  checked += 1
  if (args.length !== want || prose) {
    problems += 1
    const line = src.slice(0, match.index).split('\n').length
    const why = prose ? '默认值里出现中文 —— 描述文案串到了默认值位置' : `实参 ${args.length} 个，签名要 ${want} 个`
    console.error(`✗ ${path.relative(root, file)}:${line} — ${match[1]}() ${why}`)
    console.error(`    key  = ${args[0]}`)
    console.error(`    last = ${last.slice(0, 90)}`)
  }
  found.lastIndex = j
}

if (problems) {
  console.error(`\nprecheck:knobs 失败：${checked} 处构造调用里 ${problems} 处参数错位。`)
  console.error('这会在 CI 的 every_default_parses 单测里失败 —— knob() 少一个 desc 参数，用 knob_d()。')
  process.exit(1)
}

// ── 第二项：`unwired(...)` 的说明必须自带出口 ─────────────────
//
// 对应 Rust 单测 `unwired_knobs_explain_themselves`：标了 wired=false 的旋钮，
// desc 必须含「本仓未接入」且不少于 24 字。
//
// ── 为什么要在本地镜像这条 ───────────────────────────────────
// 那条 Rust 单测在 `for` 里 `assert!`，**第一条不合格就 panic**，CI 一次只报一条。
// 2026-11-20 就因此连烧三轮：改一条、推一次、等十分钟、发现下一条又不行。
// 这里一次列全。
function unwiredBlocks(text) {
  const out = []
  const re = /\bunwired\s*\(/g
  let m
  while ((m = re.exec(text)) !== null) {
    let depth = 1
    let i = re.lastIndex
    while (i < text.length && depth > 0) {
      if (text[i] === '(') depth += 1
      else if (text[i] === ')') depth -= 1
      i += 1
    }
    out.push({ text: text.slice(re.lastIndex, i - 1), at: m.index })
  }
  return out
}

// Rust 的字符串续行：`"...第一段\` + 换行 + 缩进 + `第二段..."`。
// 编译器会把反斜杠+换行+行首空白整个吃掉，所以这里也要先吃掉，
// 否则 JSON.parse 撞上真换行就判「取不到 desc」—— 那是脚本的错，不是代码的错。
function unquote(raw) {
  const trimmed = raw.trim().replace(/\\\r?\n[ \t]*/g, '')
  if (!trimmed.startsWith('"') || !trimmed.endsWith('"')) return null
  try {
    return JSON.parse(trimmed)
  } catch {
    return null
  }
}

let unwiredTotal = 0
let unwiredBad = 0
for (const block of unwiredBlocks(src)) {
  const body = block.text
  const k = /\bknob(?:_d)?\s*\(/.exec(body)
  if (!k) continue
  let depth = 1
  let i = k.index + k[0].length
  while (i < body.length && depth > 0) {
    if (body[i] === '(') depth += 1
    else if (body[i] === ')') depth -= 1
    i += 1
  }
  const inner = body.slice(k.index + k[0].length, i - 1)
  // 按顶层逗号切实参
  const args = []
  let d = 0
  let cur = ''
  let inString = false
  for (let x = 0; x < inner.length; x += 1) {
    const c = inner[x]
    if (inString) {
      cur += c
      if (c === '\\') {
        cur += inner[x + 1] ?? ''
        x += 1
        continue
      }
      if (c === '"') inString = false
      continue
    }
    if (c === '"') {
      inString = true
      cur += c
      continue
    }
    if (c === '(' || c === '[' || c === '{') d += 1
    if (c === ')' || c === ']' || c === '}') d -= 1
    if (c === ',' && d === 0) {
      args.push(cur.trim())
      cur = ''
      continue
    }
    cur += c
  }
  if (cur.trim()) args.push(cur.trim())
  const key = unquote(args[0]) ?? args[0] ?? '(?)'
  const desc = args.length >= 6 ? unquote(args[5]) : null
  unwiredTotal += 1
  const line = src.slice(0, block.at).split('\n').length
  if (desc === null) {
    unwiredBad += 1
    console.error(`✗ ${path.relative(root, file)}:${line} — unwired(${key}) 取不到 desc`)
    continue
  }
  const hasPhrase = desc.includes('本仓未接入')
  const long = [...desc].length >= 24
  if (!hasPhrase || !long) {
    unwiredBad += 1
    console.error(
      `✗ ${path.relative(root, file)}:${line} — ${key} 标了 wired=false 但说明不合格` +
        `（含「本仓未接入」=${hasPhrase}，长度=${[...desc].length}）`,
    )
    console.error(`    desc=${desc.slice(0, 100)}`)
  }
}

if (unwiredBad) {
  console.error(`\nprecheck:knobs 失败：${unwiredTotal} 条 unwired 里 ${unwiredBad} 条说明不合格。`)
  console.error('这会在 CI 的 unwired_knobs_explain_themselves 单测里失败 —— 用户需要的是原因和出口，不是一句「暂不支持」。')
  process.exit(1)
}

console.log(
  `precheck:knobs OK — ${checked} 处构造调用参数合法，${unwiredTotal} 条 unwired 说明齐全。`,
)
