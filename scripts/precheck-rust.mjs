#!/usr/bin/env node
// Rust 静态预检：结构体字面量字段 vs 结构体定义（不需要 cargo / 不需要工具链）。
//
// ── 为什么需要这道闸 ────────────────────────────────────────
// 2026-11-20 连着踩了两次同一种坑，每次都烧掉一整轮 CI（每轮 4 个 job × 十几分钟）：
//
//   1. `make_id()` 加了第二个参数，调用点漏改 → E0061；
//   2. `MemoryConfig` 的 `Default` 与 `config()` 都写好了四个新字段，
//      但 `pub struct MemoryConfig { ... }` 的定义里漏写 → E0560 ×4 + E0609。
//
// 第 2 次尤其典型：四段代码分开看都对，**只有放在一起编译才炸**。而本机没有
// cargo（Windows 上只装了 Tauri 的前端工具链），本地压根跑不出编译器错误 ——
// 唯一的反馈渠道就是「推上去、等十分钟、看红灯」。
//
// 所以这里用纯文本解析做一件很窄的事：把 `struct` 的**字段定义**与全仓的
// **结构体字面量**对一遍，抓「字面量写了定义里没有的字段」（E0560 那一类）。
// 这类错误的成因几乎都是「定义漏写」，而这恰好是最容易漏的一步。
//
// ── 宁可漏报，不要误报 ──────────────────────────────────────
// 一道会叫「狼来了」的闸门跟没有闸门一样 —— 报三次假警，下次就没人看它了。
// 所以判据刻意保守：
//
//   * 同名类型可能有多个定义（不同模块、函数内局部 struct）。合并**全部**定义
//     的字段，只有在该名字的所有定义里都不存在的字段才报。
//   * 认不出的类型（外部 crate、类型别名、宏生成）直接跳过。
//   * `->` / `=>` / `>=` 不当尖括号；只有真正成对的泛型尖括号参与逗号切分。
//   * 前面紧挨 `->` 或 `:` 的 `Type {` 是「返回类型 / 类型标注 + 函数体」，
//     不是字面量，跳过。
//   * 解构模式（`let Foo { a, b } = x`）没有冒号字段，天然不报。
import { readFileSync, readdirSync, statSync } from 'node:fs'
import path from 'node:path'
import process from 'node:process'
import { fileURLToPath } from 'node:url'

const root = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..')
const scanRoots = [
  path.join(root, 'desktop-tauri', 'src-tauri', 'server', 'src'),
  path.join(root, 'desktop-tauri', 'src-tauri', 'src'),
]

/// 把注释与字符串/字符字面量替换成等长空格，保留换行。
///
/// 目的：之后所有括号匹配、标识符切分都只看真代码。字符串里的 `{` / `//`
/// 会让朴素的括号计数整段错位，而大括号错位之后报出来的东西全是噪音。
function maskCode(src) {
  const out = src.split('')
  const n = src.length
  let i = 0
  const blank = (from, to) => {
    for (let k = from; k < to && k < n; k += 1) if (out[k] !== '\n') out[k] = ' '
  }
  while (i < n) {
    const c = src[i]
    const next = src[i + 1]
    if (c === '/' && next === '/') {
      let j = i
      while (j < n && src[j] !== '\n') j += 1
      blank(i, j)
      i = j
      continue
    }
    if (c === '/' && next === '*') {
      // Rust 的块注释允许嵌套
      let depth = 0
      let j = i
      while (j < n) {
        if (src[j] === '/' && src[j + 1] === '*') {
          depth += 1
          j += 2
          continue
        }
        if (src[j] === '*' && src[j + 1] === '/') {
          depth -= 1
          j += 2
          if (depth === 0) break
          continue
        }
        j += 1
      }
      blank(i, j)
      i = j
      continue
    }
    // r"..." / r#"..."# 原始字符串
    if (c === 'r' && (next === '"' || next === '#')) {
      let hashes = 0
      let j = i + 1
      while (src[j] === '#') {
        hashes += 1
        j += 1
      }
      if (src[j] === '"') {
        const terminator = '"' + '#'.repeat(hashes)
        const end = src.indexOf(terminator, j + 1)
        const stop = end === -1 ? n : end + terminator.length
        blank(i, stop)
        i = stop
        continue
      }
    }
    if (c === 'b' && next === '"') {
      let j = i + 2
      while (j < n) {
        if (src[j] === '\\') {
          j += 2
          continue
        }
        if (src[j] === '"') {
          j += 1
          break
        }
        j += 1
      }
      blank(i, j)
      i = j
      continue
    }
    if (c === '"') {
      let j = i + 1
      while (j < n) {
        if (src[j] === '\\') {
          j += 2
          continue
        }
        if (src[j] === '"') {
          j += 1
          break
        }
        j += 1
      }
      blank(i, j)
      i = j
      continue
    }
    // 字符字面量：'a' / '\n' / '\u{1F600}'。生命周期 'a 不能当字面量吃掉。
    if (c === "'") {
      let j = i + 1
      let isChar = false
      if (src[j] === '\\') {
        let k = j + 1
        if (src[k] === 'u') {
          while (k < n && src[k] !== "'") k += 1
          if (src[k] === "'") {
            isChar = true
            j = k + 1
          }
        } else {
          k += 1
          if (src[k] === "'") {
            isChar = true
            j = k + 1
          }
        }
      } else if (src[j + 1] === "'") {
        isChar = true
        j += 2
      }
      if (isChar) {
        blank(i, j)
        i = j
        continue
      }
    }
    i += 1
  }
  return out.join('')
}

function collectFiles(dir, acc) {
  let entries
  try {
    entries = readdirSync(dir)
  } catch {
    return acc
  }
  for (const name of entries) {
    const full = path.join(dir, name)
    let info
    try {
      info = statSync(full)
    } catch {
      continue
    }
    if (info.isDirectory()) {
      if (name === 'target' || name === 'node_modules') continue
      collectFiles(full, acc)
    } else if (name.endsWith('.rs')) {
      acc.push(full)
    }
  }
  return acc
}

/// 从 `open` 指向的 `{` 开始，返回匹配的 `}` 下标（只看 mask）。
function matchBrace(mask, open) {
  let depth = 0
  for (let i = open; i < mask.length; i += 1) {
    if (mask[i] === '{') depth += 1
    else if (mask[i] === '}') {
      depth -= 1
      if (depth === 0) return i
    }
  }
  return -1
}

/// 判断 `i` 处的 `<` 是否是一个泛型参数的开头。
///
/// 排除 `a < b`、`->`、`=>`、`<=` 这些非泛型用法。判据：`<` 后面紧跟的是
/// 类型名开头（大写、`&`、`'`、`_`、`[`、`(`），且前面不是运算符。
function isGenericOpen(mask, i) {
  const prev = mask[i - 1]
  if (prev === '-' || prev === '=' || prev === '<' || prev === '!') return false
  if (!/[A-Za-z0-9_\])>]/.test(prev || ' ')) return false
  let j = i + 1
  while (j < mask.length && /\s/.test(mask[j])) j += 1
  return /[A-Za-z_&'[(]/.test(mask[j] || '')
}

/// 把一个 `{ ... }` 区间按顶层逗号切开。
///
/// `<` / `>` 只在「成对泛型」时计入深度：`HashMap<String, Value>` 里的逗号
/// 不算顶层，但 `a < b` / `->` / `=>` 里的符号不参与计数。
function splitTopLevel(mask, from, to) {
  const parts = []
  let depth = 0
  let angle = 0
  let start = from
  for (let i = from; i < to; i += 1) {
    const c = mask[i]
    if (c === '{' || c === '(' || c === '[') depth += 1
    else if (c === '}' || c === ')' || c === ']') depth -= 1
    else if (c === '<' && depth === 0 && isGenericOpen(mask, i)) angle += 1
    else if (c === '>' && depth === 0 && angle > 0 && mask[i - 1] !== '-' && mask[i - 1] !== '=') angle -= 1
    else if (c === ',' && depth === 0 && angle === 0) {
      parts.push([start, i])
      start = i + 1
    }
  }
  if (start < to) parts.push([start, to])
  return parts
    .map(([a, b]) => ({ text: mask.slice(a, b), at: a, end: b }))
    .filter(part => part.text.trim())
}

/// 跳过段首的 `#[...]` / `///`（后者已在 mask 里被清空）与空白。
function skipAttributes(mask, at, end) {
  let i = at
  for (;;) {
    while (i < end && /\s/.test(mask[i])) i += 1
    if (mask[i] === '#' && mask[i + 1] === '[') {
      let depth = 0
      while (i < end) {
        if (mask[i] === '[') depth += 1
        else if (mask[i] === ']') {
          depth -= 1
          if (depth === 0) {
            i += 1
            break
          }
        }
        i += 1
      }
      continue
    }
    if (mask[i] === '#' && mask[i + 1] === '!') {
      i += 2
      continue
    }
    break
  }
  return i
}

/// 解析 `{ ... }` 里的字段名集合。`pub` / `pub(crate)` 前缀都吃掉。
function fieldNames(mask, open, close) {
  const fields = new Set()
  for (const part of splitTopLevel(mask, open + 1, close)) {
    const start = skipAttributes(mask, part.at, part.end)
    const chunk = mask.slice(start, part.end)
    const ident = /^(?:pub(?:\s*\([^)]*\))?\s+)?([A-Za-z_]\w*)\s*:/.exec(chunk)
    if (ident) fields.add(ident[1])
  }
  return fields
}

/// 登记一个类型。同名类型可能有多个定义：合并字段而不是覆盖。
function merge(types, key, file, open, fields) {
  let item = types.get(key)
  if (!item) {
    item = { fields: new Set(), opens: new Set(), files: new Set() }
    types.set(key, item)
  }
  for (const field of fields) item.fields.add(field)
  item.opens.add(open)
  item.files.add(file)
}

function scanDefinitions(files) {
  const types = new Map()
  for (const file of files) {
    const src = readFileSync(file, 'utf8')
    const mask = maskCode(src)
    const decl = /\b(struct|enum)\s+([A-Za-z_]\w*)/g
    let hit
    while ((hit = decl.exec(mask)) !== null) {
      const kind = hit[1]
      const name = hit[2]
      let i = decl.lastIndex
      // 跳过泛型参数与 where 子句，找到体内的 `{`
      let angle = 0
      while (i < mask.length) {
        const c = mask[i]
        if (c === '<' && isGenericOpen(mask, i)) angle += 1
        else if (c === '>' && angle > 0 && mask[i - 1] !== '-' && mask[i - 1] !== '=') angle -= 1
        else if ((c === '{' || c === ';' || c === '(') && angle === 0) break
        i += 1
      }
      if (mask[i] !== '{') continue
      const close = matchBrace(mask, i)
      if (close === -1) continue
      if (kind === 'struct') {
        merge(types, name, file, i, fieldNames(mask, i, close))
        continue
      }
      // 枚举：只登记「带命名字段的变体」，key 用 `Enum::Variant`
      for (const part of splitTopLevel(mask, i + 1, close)) {
        const start = skipAttributes(mask, part.at, part.end)
        const head = /^([A-Za-z_]\w*)\s*\{/.exec(mask.slice(start, part.end))
        if (!head) continue
        const braceOpen = start + head[0].length - 1
        const braceClose = matchBrace(mask, braceOpen)
        if (braceClose === -1) continue
        merge(types, `${name}::${head[1]}`, file, braceOpen, fieldNames(mask, braceOpen, braceClose))
      }
    }
  }
  return types
}

function scanLiterals(files, types) {
  const problems = []
  for (const file of files) {
    const src = readFileSync(file, 'utf8')
    const mask = maskCode(src)
    const lineOf = at => mask.slice(0, at).split('\n').length
    const lit = /\b([A-Za-z_]\w*)\s*\{/g
    let hit
    while ((hit = lit.exec(mask)) !== null) {
      const name = hit[1]
      const before = mask.slice(Math.max(0, hit.index - 60), hit.index)
      // 定义点本身、`impl X`、`for X` 之类都不是字面量
      if (/\b(struct|enum|impl|trait|union|type|dyn)\s*$/.test(before)) continue
      // 返回类型 / 类型标注后面紧跟的 `{` 是函数体，不是结构体字面量
      if (/(->|=|:)\s*$/.test(before)) continue
      let key = name
      const owner = /([A-Za-z_]\w*)\s*::\s*$/.exec(before)
      if (owner) key = `${owner[1]}::${name}`
      const decl = types.get(key)
      if (!decl) continue
      const open = hit.index + hit[0].length - 1
      if (decl.opens.has(open)) continue
      const close = matchBrace(mask, open)
      if (close === -1) continue
      for (const part of splitTopLevel(mask, open + 1, close)) {
        const text = part.text
        if (/^\s*\.\./.test(text)) break
        const ident = /^\s*([A-Za-z_]\w*)\s*:/.exec(text)
        if (!ident) continue
        if (!decl.fields.has(ident[1])) {
          problems.push({
            type: key,
            field: ident[1],
            definedIn: [...decl.files],
            usedAt: file,
            line: lineOf(part.at),
          })
        }
      }
    }
  }
  return problems
}

/// 检查「内联 mod 块（`mod tests { ... }`）里用 `super::X::`，但 X 是**文件模块的兄弟**」这类路径错误。
///
/// ── 为什么会犯这个错 ────────────────────────────────────────
/// `store.rs` 里的代码默认在模块 `memory::store` 里，`super::model` 指
/// `memory::model`（兄弟模块）—— 这是对的。但一旦这段代码被搬进文件内的
/// `mod tests { ... }`，它所在的模块变成 `memory::store::tests`，`super::` 就指回
/// `memory::store` 本身，`super::model` 变成 `memory::store::model`（不存在），
/// 于是 `E0433`。
///
/// 这个错尤其阴：`super::model::content_hash(...)` 写在文件主体里是对的，复制进
/// 测试块后**看起来一模一样**，只有编译器知道层级变了。2026-11-20 就这么红过
/// 一轮（`store.rs` 的 `container()` 测试辅助）。
///
/// ── 判据（严格限定，宁漏不误）──────────────────────────────
/// 只报这一种：
///   1. 文件 F 的**父模块文件** P（同目录 `mod.rs`，或上层 `<目录名>.rs`）里
///      声明了 `mod X;` —— 即 X 与 F 是兄弟；
///   2. F 的内联 `mod block { ... }` 里写了 `super::X::`；
///   3. F 的模块体里**没有** `use ... super::X ...`（这种 `use` 会在 F 下创建
///      名字 `X`，此时 `super::X::` 其实是合法的，跳过）。
///
/// 其余 `super::` 形态一律不碰 —— 宁可漏报。
function buildModuleTree(files) {
  // 文件 → 它声明的子模块名集合；以及 模块名 → 子文件的映射（用于找父文件）
  const declared = new Map() // file -> Set<string>
  const fileByPath = new Set(files)
  for (const file of files) {
    const mask = maskCode(readFileSync(file, 'utf8'))
    const names = new Set()
    const declRe = /^[ \t]*(?:pub(?:\([^)]*\))?\s+)?mod\s+([A-Za-z_]\w*)\s*;/gm
    let decl
    while ((decl = declRe.exec(mask)) !== null) names.add(decl[1])
    declared.set(file, names)
  }

  // 找每个文件的父文件：同目录 mod.rs 优先，其次上层 `<目录名>.rs`
  const parentOf = new Map()
  for (const file of files) {
    const dir = path.dirname(file)
    const base = path.basename(file, '.rs')
    const candidates = []
    if (base !== 'mod') {
      const modRs = path.join(dir, 'mod.rs')
      if (fileByPath.has(modRs)) candidates.push(modRs)
    }
    if (!candidates.length && (base === 'mod' || base === 'index')) {
      const parentDir = path.dirname(dir)
      const sibling = path.join(parentDir, `${path.basename(dir)}.rs`)
      if (fileByPath.has(sibling)) candidates.push(sibling)
      const upperMod = path.join(parentDir, 'mod.rs')
      if (!candidates.length && fileByPath.has(upperMod)) candidates.push(upperMod)
    }
    if (candidates.length) parentOf.set(file, candidates[0])
  }
  return { declared, parentOf }
}

/// 字节串字面量 `b"..."` 里不允许非 ASCII。
///
/// ── 为什么这条检查必须在本地跑 ──────────────────────────────
/// Rust 的 `b"..."` 只接受 ASCII；塞进中文的报错是
/// `error: non-ASCII character in byte string literal`，而且**一处中文会拆成
/// 每字节一条错误**（「修好了」三个字能刷出 9 条）。CI 上看到的是十来条
/// 同形错误，很容易误以为是别的地方炸了。
///
/// 这个坑踩过两次（0109f57f、c0488917），两次都是测试夹具里写了
/// `turn.push(b"data: {...中文...}")`。正确写法是普通字符串再 `.as_bytes()`。
/// 它是**纯文本**就能判的，不该等到 CI 编译才发现。
function scanByteStrings(files) {
  const problems = []
  for (const file of files) {
    const src = readFileSync(file, 'utf8')
    // ⚠️ 不能用 `maskCode`：它把字节串的**内容**也清成空格（那正是给结构体
    // 字段扫描用的），于是中文早就被抹平了，永远扫不到。这里只遮注释。
    const mask = blankComments(src)
    // 匹配 `b"..."`（含 `br"..."` 原始字节串），跳过转义字符
    const re = /\bbr?"(?:[^"\\]|\\.)*"/g
    let hit
    while ((hit = re.exec(mask)) !== null) {
      const literal = hit[0]
      if (!/^[\x00-\x7F]*$/.test(literal)) {
        const line = mask.slice(0, hit.index).split('\n').length
        problems.push({ file, line, literal: literal.slice(0, 60) })
      }
    }
  }
  return problems
}

/// 只把注释抹成空白，字符串原样保留（与 `maskCode` 分开）。
function blankComments(src) {
  const out = src.split('')
  const n = src.length
  let i = 0
  const blank = (from, to) => {
    for (let k = from; k < to && k < n; k += 1) if (out[k] !== '\n') out[k] = ' '
  }
  while (i < n) {
    const c = src[i]
    const next = src[i + 1]
    if (c === '/' && next === '/') {
      let j = i
      while (j < n && src[j] !== '\n') j += 1
      blank(i, j)
      i = j
      continue
    }
    if (c === '/' && next === '*') {
      let depth = 0
      let j = i
      while (j < n) {
        if (src[j] === '/' && src[j + 1] === '*') {
          depth += 1
          j += 2
          continue
        }
        if (src[j] === '*' && src[j + 1] === '/') {
          depth -= 1
          j += 2
          if (depth === 0) break
          continue
        }
        j += 1
      }
      blank(i, j)
      i = j
      continue
    }
    // 普通字符串 / 字节串 / 原始字符串整段跳过，免得里面的 `//` 被当成注释
    if (c === '"') {
      let j = i + 1
      while (j < n) {
        if (src[j] === '\\') {
          j += 2
          continue
        }
        if (src[j] === '"') {
          j += 1
          break
        }
        j += 1
      }
      i = j
      continue
    }
    if (c === 'b' && (next === '"' || next === 'r')) {
      let j = i + 1
      if (src[j] === 'r') {
        j += 1
        let hashes = 0
        while (src[j] === '#') {
          hashes += 1
          j += 1
        }
        if (src[j] === '"') {
          const terminator = '"' + '#'.repeat(hashes)
          const end = src.indexOf(terminator, j + 1)
          i = end === -1 ? n : end + terminator.length
          continue
        }
      }
      if (src[j] === '"') {
        j += 1
        while (j < n) {
          if (src[j] === '\\') {
            j += 2
            continue
          }
          if (src[j] === '"') {
            j += 1
            break
          }
          j += 1
        }
        i = j
        continue
      }
    }
    i += 1
  }
  return out.join('')
}

/// 同名函数重复定义（同一文件、模块顶层）。
///
/// ── 为什么需要这道闸 ────────────────────────────────────────
/// 2026-11-24：`memory/mod.rs` 里 `auto_apply` 被整段写了两遍，两份内容逐字
/// 相同。rustc 报 E0428（the name is defined multiple times），同样在第一个
/// job 就炸。成因是一次编辑把整块函数连同文档注释一起贴了两遍 —— 这种失误
/// 肉眼看 diff 时极难发现（两份一模一样，`git diff` 里只是多了一段）。
///
/// 只看**列 0** 的 `fn`：`impl` 块与内联 `mod` 里的函数有缩进，同名的 `new` /
/// `default` 在不同 `impl` 里完全合法，不能报。`#[cfg(test)]` 里的测试函数
/// 同样有缩进，天然被排除。
///
/// ── 平台分支要放过 ──────────────────────────────────────────
/// 上面这条规则一跑，`backend.rs` / `login.rs` 立刻报出三对「重名」，全都是
/// `#[cfg(windows)]` 与 `#[cfg(not(windows))]` 成对的实现（同一函数两份，
/// 各自只在一个平台上参与编译）。这是合法写法，rustc 不会报 E0428。
///
/// 所以判据是「**两边都**带 `#[cfg(...)]` 才放过」：
///   * 两边都有 cfg → 条件互斥的平台/特性分支，放过；
///   * 只有一边有 cfg → 仍是真错误，另一个配置下就是重名；
///   * 两边都没有 cfg → 真错误（这次 `auto_apply` 的形态）。
function scanDuplicateFns(files) {
  const problems = []
  for (const file of files) {
    const mask = maskCode(readFileSync(file, 'utf8'))
    const lines = mask.split('\n')
    // 某个顶层函数声明上方紧邻的属性集合（`#[cfg(...)]` 之类）。
    // 文档注释在 mask 里已被抹成空白，所以「往前跳过空白行」不会跳过属性。
    const cfgsAbove = (at) => {
      const found = []
      let k = at - 1
      let guard = 0
      while (k >= 0 && guard < 60) {
        const text = lines[k].trim()
        if (text === '') {
          k -= 1
          guard += 1
          continue
        }
        const attr = /^#\[[^\]]*\]/.exec(text)
        if (!attr) break
        if (/^#\[cfg(?:_attr)?\s*\(/.test(attr[0])) found.push(attr[0])
        k -= 1
        guard += 1
      }
      return found
    }
    const seen = new Map()
    for (let i = 0; i < lines.length; i += 1) {
      const hit =
        /^(?:pub(?:\([^)]*\))?\s+)?(?:const\s+)?(?:async\s+)?(?:unsafe\s+)?(?:extern\s+"[^"]*"\s+)?fn\s+([A-Za-z_]\w*)/.exec(
          lines[i],
        )
      if (!hit) continue
      const first = seen.get(hit[1])
      if (first === undefined) {
        seen.set(hit[1], i + 1)
        continue
      }
      const here = cfgsAbove(i)
      const there = cfgsAbove(first - 1)
      if (here.length && there.length) continue
      problems.push({ file, line: i + 1, name: hit[1], first })
    }
  }
  return problems
}

/// 括号平衡：`(` `[` `{` 三种是否成对闭合。
///
/// ── 为什么必须补这道闸 ──────────────────────────────────────
/// 2026-11-24：`config.rs` 的 SCHEMA 里多打了一个 `)`（`"false")),` 应为
/// `"false"),`）。这行改动只是加了一对括号里的一个字符，而上面三道检查
/// ——结构体字段、mod 路径、字节串——全都不看括号，于是 `npm run precheck`
/// 一路绿，只有 CI 的 rustc 会在第一个 job 就炸：
///
///   error: unexpected closing delimiter: `)`
///
/// 本机没有 cargo，CI 是唯一编译权威。一个字符的失误换来一整轮 4 个 job ×
/// 十几分钟的反馈延迟，而这恰好是纯文本就能查出来的东西。所以在这里补上。
///
/// 判据是 `maskCode()` 之后的文本（注释/字符串/字符字面量都已被抹成空格），
/// 所以中文字符串里的全角括号、注释里的 `//` 都不会干扰计数。
function scanDelimiters(files) {
  const pairs = { ')': '(', ']': '[', '}': '{' }
  const problems = []
  for (const file of files) {
    const src = readFileSync(file, 'utf8')
    const mask = maskCode(src)
    const stack = []
    for (let i = 0; i < mask.length; i += 1) {
      const c = mask[i]
      if (c === '(' || c === '[' || c === '{') {
        stack.push({ ch: c, at: i })
        continue
      }
      if (c !== ')' && c !== ']' && c !== '}') continue
      const top = stack.pop()
      const line = mask.slice(0, i).split('\n').length
      if (!top) {
        problems.push({ file, line, detail: `多出一个 \`${c}\`，此时没有任何未闭合的左括号` })
        continue
      }
      if (top.ch !== pairs[c]) {
        const openLine = mask.slice(0, top.at).split('\n').length
        problems.push({
          file,
          line,
          detail: `\`${c}\` 与第 ${openLine} 行的 \`${top.ch}\` 不配对`,
        })
      }
    }
    for (const left of stack) {
      const line = mask.slice(0, left.at).split('\n').length
      problems.push({ file, line, detail: `\`${left.ch}\` 一直没有闭合` })
    }
  }
  return problems
}

/// 找到 `from` 之后、同一深度上开启代码块的那个 `{`。
///
/// 只认深度 0：`if matches!(x, Foo { .. }) {` 里第一个 `{` 在括号内，属于
/// 模式而不是块，认错了整段后续匹配全错位。
///
/// ── 碰到 `=>` 就放弃 ────────────────────────────────────────
/// `match` 的**守卫**也长成 `if`：`Some(c) if c.is_null() => { ... }`。
/// 那不是 `if` 语句，它后面的 `{` 是 match arm 的块 —— 拿它当 if 体会把整个
/// arm 当成条件分支，于是「arm 块尾是 match」被误报成「if 块尾丢了返回值」。
/// 所以深度 0 上先撞到 `=>` 说明这是守卫，直接返回 -1。
///
/// 碰到深度 0 的 `;` / `,` 同样放弃：那说明后面没有块，继续往文件末尾找
/// 只会把不相干的 `{` 认成 if 体。
function findBlockOpen(mask, from) {
  let depth = 0
  for (let i = from; i < mask.length && i < from + 400; i += 1) {
    const c = mask[i]
    if (c === '(' || c === '[') depth += 1
    else if (c === ')' || c === ']') depth = Math.max(0, depth - 1)
    else if (depth === 0 && c === '=' && mask[i + 1] === '>') return -1
    else if (depth === 0 && (c === ';' || c === ',')) return -1
    else if (c === '{' && depth === 0) return i
  }
  return -1
}

/// `close` 处那个 `}` 对应的 `{`（`matchBrace` 的反向版）。
function matchBraceBack(mask, close) {
  let depth = 0
  for (let i = close; i >= 0; i -= 1) {
    const c = mask[i]
    if (c === '}') depth += 1
    else if (c === '{') {
      depth -= 1
      if (depth === 0) return i
    }
  }
  return -1
}

/// 「表达式被当成语句丢弃」：`if cond { <match 作为块尾表达式> }` 且没有 `else`。
///
/// ── 为什么必须补这道闸 ──────────────────────────────────────
/// 2026-11-24：`retire_reason_of` 的 dedup 分支写成了
///
///     if row.kind == "dedup" {
///         ...
///         match (target_end, candidate_end) {           // ← 少了 return
///             (Some(l), Some(r)) if l == r => Some("auto-already-resolved"),
///             _ => None,
///         }
///     }                                                  // ← 没有 else
///     if row.kind != "classify" { return None; }
///
/// 这个函数返回 `Option<&'static str>`，而「不带 else 的 if」要求块的值是 `()`。
/// rustc 报 E0308（expected `()`, found `Option<&str>`），三个平台同时编译失败 ——
/// 上一轮 4 个 job × 十几分钟全烧在这一行上。
///
/// 文字层面的特点非常固定：块内最后一条语句是 `match`，这些分支里至少有一个
/// 产出值（`Some(...)` / `Ok(...)` / `Err(...)` / `None` / 布尔 / 字面量），而且
/// `if` 后面没有 `else`。这三条同时成立就一定是把返回值丢了 —— 判据不需要知道
/// 类型，因为「不带 else 的 if」本来就不允许有值。
///
/// ── 哪些情况故意放过 ────────────────────────────────────────
///   * 块尾是 `match ... ;`（带分号 = 明确当语句用）；
///   * 块尾是 `let x = match ...;` 或 `return match ...;`（分号挡住）；
///   * 分支全是 `{}` / `()` 的 match（值就是 unit，合法）；
///   * 有 `else` 的 if（可以带值）；
///   * `loop { if c { match ... } }` 之类仍然会报 —— 那同样编译不过。
///
/// 宁可漏报：只认「块尾 match + 有值分支 + 无 else」这一种公认的错法。
function scanDiscardedMatch(files) {
  const problems = []
  for (const file of files) {
    const src = readFileSync(file, 'utf8')
    const mask = maskCode(src)
    const lineOf = at => mask.slice(0, at).split('\n').length
    const ifRe = /\bif\b/g
    let hit
    while ((hit = ifRe.exec(mask)) !== null) {
      const bodyOpen = findBlockOpen(mask, ifRe.lastIndex)
      if (bodyOpen === -1) continue
      const bodyClose = matchBrace(mask, bodyOpen)
      if (bodyClose === -1) continue
      // 有 else 就不是这条错法
      if (/^\s*else\b/.test(mask.slice(bodyClose + 1, bodyClose + 40))) continue
      const body = mask.slice(bodyOpen + 1, bodyClose)
      const lastMatch = [...body.matchAll(/\bmatch\b/g)].pop()
      if (!lastMatch) continue
      const matchAt = bodyOpen + 1 + lastMatch.index
      const armOpen = findBlockOpen(mask, matchAt + 5)
      if (armOpen === -1) continue
      const armClose = matchBrace(mask, armOpen)
      if (armClose === -1) continue
      // match 必须是块尾：后面只剩空白（分号会留一个 `;`，正好被这条挡掉）
      if (mask.slice(armClose + 1, bodyClose).trim() !== '') continue
      const arms = mask.slice(armOpen + 1, armClose)
      // 至少一个分支产出值。字符串字面量在 mask 里已被抹空，`Some("x")` 仍匹配。
      if (!/=>\s*(?:Some\s*\(|Ok\s*\(|Err\s*\(|None\b|true\b|false\b|&|\d)/.test(arms)) continue
      problems.push({ file, line: lineOf(matchAt), ifLine: lineOf(bodyOpen) })
    }
  }
  return problems
}

function scanModPaths(files) {
  const { declared, parentOf } = buildModuleTree(files)
  const problems = []
  for (const file of files) {
    const parent = parentOf.get(file)
    if (!parent) continue
    const siblings = declared.get(parent)
    if (!siblings || !siblings.size) continue

    const src = readFileSync(file, 'utf8')
    const mask = maskCode(src)

    // 文件模块体里若把兄弟**当成模块名本身**导进来，内联 mod 里的 `super::X::` 才可能合法：
    //   use super::model;            → 在 F 里创建名字 model
    //   use super::model as m;       → 创建别名 m（不是 X 本身，但保险起见一并放过）
    // 注意 `use super::model::{A, B};` **不算** —— 它只导入条目，不创建模块名 `model`，
    // 所以 `super::model::f()` 在内联 mod 里仍然解析不了（这正是 store.rs 那次事故的形态）。
    const reimported = new Set()
    const useRe = /\buse\s+[^;]*?\bsuper::([A-Za-z_]\w*)\s*(?:;|as\b)/g
    let use
    while ((use = useRe.exec(mask)) !== null) reimported.add(use[1])

    const blockRe = /^[ \t]*(?:pub(?:\([^)]*\))?\s+)?mod\s+([A-Za-z_]\w*)\s*\{/gm
    let block
    while ((block = blockRe.exec(mask)) !== null) {
      const open = block.index + block[0].length - 1
      const close = matchBrace(mask, open)
      if (close === -1) continue
      const inner = mask.slice(open + 1, close)
      const baseLine = mask.slice(0, open).split('\n').length
      const innerUseRe = /\bsuper::([A-Za-z_]\w*)::/g
      let hitInner
      while ((hitInner = innerUseRe.exec(inner)) !== null) {
        const target = hitInner[1]
        if (!siblings.has(target)) continue
        if (reimported.has(target)) continue
        const line = baseLine + inner.slice(0, hitInner.index).split('\n').length - 1
        problems.push({ file, line, module: block[1], target })
      }
    }
  }
  return problems
}

const files = scanRoots.flatMap(dir => collectFiles(dir, []))
if (!files.length) {
  console.error('precheck:rust 找不到 Rust 源码，检查脚本里的 scanRoots。')
  process.exit(1)
}
const types = scanDefinitions(files)
const problems = scanLiterals(files, types)
const pathProblems = scanModPaths(files)
const byteProblems = scanByteStrings(files)
const delimiterProblems = scanDelimiters(files)
const dupProblems = scanDuplicateFns(files)
const discardProblems = scanDiscardedMatch(files)
const rel = file => path.relative(root, file).split(path.sep).join('/')

if (
  !problems.length &&
  !pathProblems.length &&
  !byteProblems.length &&
  !delimiterProblems.length &&
  !dupProblems.length &&
  !discardProblems.length
) {
  console.log(
    `precheck:rust OK — ${files.length} 个文件 / ${types.size} 个类型定义，结构体字面量字段、mod 路径、括号平衡、重复函数、块尾 match 都没问题。`,
  )
  process.exit(0)
}

const seen = new Set()
for (const item of problems) {
  const stamp = `${item.type}|${item.field}|${item.usedAt}|${item.line}`
  if (seen.has(stamp)) continue
  seen.add(stamp)
  console.error(
    `✗ ${rel(item.usedAt)}:${item.line} — ${item.type} 用了未定义字段 \`${item.field}\`` +
      `（定义：${item.definedIn.map(rel).join(', ')}）`,
  )
}
for (const item of pathProblems) {
  const stamp = `path|${item.file}|${item.line}`
  if (seen.has(stamp)) continue
  seen.add(stamp)
  console.error(
    `✗ ${rel(item.file)}:${item.line} — 在 \`mod ${item.module}\` 里写 \`super::${item.target}::\`，但 \`${item.target}\` 与本文件模块是兄弟`,
  )
  console.error(
    `    内联 mod 里的 super:: 指回文件模块本身，碰不到兄弟模块；改成 \`super::super::${item.target}::\` 或 \`crate::...::${item.target}::\` 全路径`,
  )
}

for (const item of dupProblems) {
  console.error(
    `✗ ${rel(item.file)}:${item.line} — 函数 \`${item.name}\` 重复定义（首次在第 ${item.first} 行）`,
  )
}
if (dupProblems.length) {
  console.error(`\nprecheck:rust 失败：${dupProblems.length} 处同文件重名函数。`)
  console.error(
    '这会在 CI 里以 E0428（the name is defined multiple times）编译失败。' +
      '多半是整段函数被贴了两次 —— 删掉多余的那一份。',
  )
}
for (const item of delimiterProblems) {
  console.error(`✗ ${rel(item.file)}:${item.line} — ${item.detail}`)
}
if (delimiterProblems.length) {
  console.error(`\nprecheck:rust 失败：${delimiterProblems.length} 处括号不配对。`)
  console.error(
    '这会在 CI 里以「unexpected closing delimiter / mismatched closing delimiter」编译失败，' +
      '而且是 rustc 看到的第一类错误 —— 先把它修了，后面的真错误才轮得到显示。',
  )
}

for (const item of discardProblems) {
  console.error(
    `✗ ${rel(item.file)}:${item.line} — 第 ${item.ifLine} 行的 \`if\` 块尾是一个带返回值的 \`match\`，但这个 \`if\` 没有 \`else\``,
  )
}
if (discardProblems.length) {
  console.error(`\nprecheck:rust 失败：${discardProblems.length} 处表达式的值被当成语句丢弃。`)
  console.error(
    '这会在 CI 里以 E0308（expected `()`, found `Option<&str>` 之类）编译失败：' +
      '不带 `else` 的 `if` 块值必须是 `()`。',
  )
  console.error('要么给 match 前面加 `return`，要么在 match 后面补 `;` 把它明确当语句用。')
}

if (problems.length) {
  console.error(`\nprecheck:rust 失败：${problems.length} 处字面量字段与结构体定义不一致。`)
  console.error('这会在 CI 里以 E0560 编译失败 —— 先把结构体定义补上，或删掉字面量里的多余字段。')
}
if (pathProblems.length) {
  console.error(`\nprecheck:rust 失败：${pathProblems.length} 处 mod 路径解析不了。`)
  console.error('这会在 CI 里以 E0433 编译失败 —— 把代码搬进内联 mod 之后，super:: 的层级跟着变了。')
}
for (const item of byteProblems) {
  console.error(`✗ ${rel(item.file)}:${item.line} — 字节串字面量里有非 ASCII 字符`)
  console.error(`    ${item.literal}`)
}
if (byteProblems.length) {
  console.error(`\nprecheck:rust 失败：${byteProblems.length} 处字节串里塞了非 ASCII。`)
  console.error(
    '这会在 CI 里以「non-ASCII character in byte string literal」编译失败（而且一处中文会拆成每字节一条错误）。' +
      '改成普通字符串再 `.as_bytes()`：`\"...中文...\".as_bytes()`。',
  )
}
process.exit(1)
