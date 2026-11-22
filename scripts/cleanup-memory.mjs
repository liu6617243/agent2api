#!/usr/bin/env node
// 旧库清理：删掉「不该进记忆」的存量条目，修掉「内容对但标题坏」的条目。
//
// ── 为什么必须改磁盘文件，不能只删索引 ─────────────────────────
// `index-scan`（默认开，每 6 小时）的做法是 `store::load_all()` → `index::rebuild()`：
// 索引是派生物，`memory/**/*.md` 才是事实源。只删 `index.db` 里的行，下一轮扫描
// 会把它们原样装回来 —— 用户会看到「清了，过一会儿又全回来了」。
//
// 所以这里的做法是：**只改 .md**，索引留给 `index-scan` 重建（或本脚本末尾手动触发）。
// 好处是幂等：同一个文件跑两遍结果一样。
//
// ── 判据分层（宁可少删，不要错删）─────────────────────────────
// 记忆库里有两类坏东西，处理方式完全不同：
//
//   A. **整条都该消失** —— 用户侧不是用户说的、助手侧也不是结论：
//      `<task-notification>`、`Attached image(s) from tool result:`、
//      `<antml:function_calls>`、`<system-reminder …>`、工具回执碎片。
//      这些条目正文里没有任何「用户要我记住的事」，留着只污染注入。
//
//   B. **正文有价值、只是用户侧被顶替** —— 典型是
//      `**我**：Attached image(s) from tool result:` 后面跟着 300 字的真实结论。
//      删了等于把结论一起扔掉。这类要么**只修标题**，要么**把用户侧替换成能
//      说明这轮在干什么的一行**（`_（本轮触发是工具回执，非用户原话）_`）。
//
// 判据只认**见过的真机形状**（`.workbuddy/tmp` 里的核对记录 + 2026-11-22 的库采样），
// 不猜。任何一条拿不准就不动，列进 `review` 让人看。
//
// ── 用法 ────────────────────────────────────────────────────────
//   node scripts/cleanup-memory.mjs                    # dry-run，只打印分类
//   node scripts/cleanup-memory.mjs --apply            # 真改（先自动备份）
//   node scripts/cleanup-memory.mjs --apply --project projects
//   node scripts/cleanup-memory.mjs --root <dir>       # 指定记忆库根（默认读 ~/.agent2api/memory）

import fs from 'node:fs'
import os from 'node:os'
import path from 'node:path'
import { fileURLToPath } from 'node:url'

const argv = process.argv.slice(2)
const has = (f) => argv.includes(f)
const val = (f, d) => {
  const i = argv.indexOf(f)
  return i >= 0 && argv[i + 1] ? argv[i + 1] : d
}

const APPLY = has('--apply')
const ONLY_PROJECT = val('--project', '')
const ROOT = val('--root', path.join(os.homedir(), '.agent2api', 'memory'))

// ── 用户侧不是用户原话的标记 ────────────────────────────────────
// 出现这些前缀，说明 `**我**：` 后面跟的不是用户打的字。整轮用户侧不可信。
//
// 顺序有意义：`<task-notification>` 要排在 `<task` 之前（更具体的先匹配），
// 但这里是 `startsWith` 逐个试，不存在前缀遮蔽问题。
const NOT_USER_PREFIXES = [
  '<task-notification>',
  '<tool-use-id>',
  '<status>',
  '<summary>',
  'Attached image(s) from tool result:',
  '<antml:function_calls>',
  '<function_calls>',
  '<tool_call>',
  '<tool_calls>',
  '<result>',
  '<previous_tool_call>',
  '<cb_summary>',
  // 客户端的续跑指令 —— 不是用户打的字
  'Please continue with the conversation based on',
  // 客户端自己的会话壳
  '<session>',
  '<system-reminder',
  '<environment_context',
]

// 整条都是「非用户原话 + 非结论」的形状：标题就长这样，说明当初 infer_title
// 直接截了第一行。
const NOT_USER_TITLE_PREFIXES = [
  '**我**：<task-notification>',
  '**我**：Attached image(s) from tool result:',
  '**我**：<antml:',
  '**我**：<tool_call',
  '**我**：<result>',
  '<system-reminder',
  '<antml:',
  '<function_calls',
  '<tool_call',
  '<task-notification',
  '**助手**：',
]

// 工具调用/回执的正文碎片：正文**整体**就是这些东西，没有人类可读结论。
const TOOL_NOISE_PATTERNS = [
  /^<cb_summary>/,
  /^<\/result>/,
  /^<result>/,
  /^<previous_tool_call>/,
  /^<previous_assistant_message>/,
  /^Tool: \w+/,
  /^us_tool_call>/,
  /^need to act on it\. IMPORTANT: Before responding/,
]

// ── 导入侧特有的垃圾形状 ────────────────────────────────────────
// 导入是把 `~/.workbuddy/projects/*.jsonl` 按窗口切片塞进库，切点不认语义，
// 于是产生三类「看起来像记忆、实际是机器输出」的段落：
//
//   1. `（前 10500 字已省略）` —— 窗口切片的省略标记，后面接着半句话
//   2. `$PATH\nIMG=...` / `export PATH=` —— shell 命令正文
//   3. `</result>\n</previous_tool_call>` —— 工具回执残片
//
// 这些留在库里只有一个后果：注入时把 shell 脚本当「相关经验」塞给模型。
const IMPORT_JUNK_PATTERNS = [
  /（前\s*\d+\s*字已省略）/, // 窗口切片省略标记
  /（后\s*\d+\s*字已省略）/,
  /^\s*\$[A-Z_]+=/, // shell 变量赋值
  /^\s*export\s+[A-Z_]+=/,
  /^\s*IM(G)?=/, // 真机样本：`IMG="C:/..."`
  /<\/result>\s*<\/previous_tool_call>/,
  /^lt><omitted \/>/,
]

// 「这一条整段都是导入切片的零件」—— 判据取「前三行里至少一行命中」+
// 「没有像样的中文/英文自然语言句子」。单看一行太容易误伤（正文里贴一段
// shell 是合法的），所以要求它是**整段的主体**。
function looksLikeImportFragment(body, title) {
  const text = (body || '').trim()
  if (!text) return false
  // 省略标记出现在开头 200 字内：这条就是被切碎的
  const head = text.slice(0, 200)
  if (/（前\s*\d+\s*字已省略）/.test(head)) return true
  // 整段没有「人名/主语 + 谓语」结构：按标点与中英文判断粗略
  const hasSentence = /[。！？!?]/.test(text) && /[\u4e00-\u9fa5]/.test(text)
  const junkHits = IMPORT_JUNK_PATTERNS.filter((re) => re.test(text)).length
  if (!hasSentence && junkHits >= 1) return true
  // 标题是裸会话 id 且正文像命令输出
  if (/^[0-9a-f]{8}-[0-9a-f]{4}/i.test(title) && junkHits >= 1) return true
  return false
}

// 「标题被写坏、但正文是真实轮次」—— 只改标题，不删条目
const BROKEN_TITLE_ONLY = [
  // 用户原话本身是好的（「动第一批…」），标题却带上了 `**我**：` 前缀
  { test: (t) => t.startsWith('**我**：') && !isNotUserTitle(t), fix: (t) => t.replace(/^\*\*我\*\*：/, '') },
  // 「Please continue with the conversation…」是客户端的续跑指令，不是用户诉求
  { test: (t) => /^Please continue with the conversation based on the summarize/i.test(t), fix: () => '续跑（客户端指令，非用户诉求）' },
]

const NOT_USER_TITLE_MATCH = (t) => NOT_USER_TITLE_PREFIXES.some((p) => t.startsWith(p))
function isNotUserTitle(t) {
  return NOT_USER_TITLE_MATCH(t)
}

// ── 读一个 .md：切成「头部 + 若干 section」 ──────────────────────
// 容器格式见 general/l1/gateway/2026-11-22.md：
//   --- frontmatter ---
//   <!-- mem {json} -->
//   ## HH:MM · title
//   正文…
// 一个文件里可以有几十个这样的 section，删某条就是删它那一段。
//
// ── `agent` / `project` 在 **frontmatter**，不在 mem 块里 ────────
// 真机 2026-11-22 踩过：第一版脚本读 `section.meta.agent` 判断「这条是导入
// 还是留存」，结果 5973 条导入条目的 `agent` 全是空 —— mem 块里只有
// `created/files/id/importance/kind/pinned/starred/supersedes/tags/title/updated`，
// `agent: import` 写在文件头的 frontmatter 里。判据读空 ⇒ 全部按 gateway
// 分支处理 ⇒ 把一个「全删后重新导入」的任务算成「改标题」。
//
// 所以 `parseContainer` 把 frontmatter 解析成对象一并返回，调用方用它，而不是
// 逐 section 去猜。
function parseContainer(text) {
  const lines = text.split('\n')
  // frontmatter：文件必须以 `---` 开头
  let i = 0
  let head = []
  if (lines[0] === '---') {
    head.push(lines[0])
    i = 1
    while (i < lines.length && lines[i] !== '---') {
      head.push(lines[i])
      i += 1
    }
    if (i < lines.length) {
      head.push(lines[i])
      i += 1
    }
  }
  const prefix = head.join('\n')
  // 把 `key: value` 折成对象。只认简单标量，够用且不会误判正文。
  const front = {}
  for (const line of head) {
    const m = line.match(/^([a-zA-Z_][\w-]*):\s*(.*)$/)
    if (m) front[m[1]] = m[2].trim()
  }
  const sections = []
  let current = null
  const memRe = /^<!-- mem (\{.*\}) -->\s*$/
  for (; i < lines.length; i += 1) {
    const line = lines[i]
    const m = line.match(memRe)
    if (m) {
      if (current) sections.push(current)
      let meta = null
      try {
        meta = JSON.parse(m[1])
      } catch {
        meta = null
      }
      current = { meta, metaLine: line, lines: [line], raw: m[1] }
      continue
    }
    if (current) current.lines.push(line)
  }
  if (current) sections.push(current)
  return { prefix, front, sections }
}

function rebuildContainer(prefix, sections, eol = '\n') {
  const body = sections.map((s) => s.lines.join(eol)).join(eol)
  return prefix ? `${prefix}${eol}${body}` : body
}

// 从 section 里取「用户侧第一行」和「助手正文」
function sectionParts(section) {
  const text = section.lines.join('\n')
  const titleMatch = text.match(/^##\s+[^·]*·\s*(.*)$/m)
  const title = titleMatch ? titleMatch[1].trim() : ''
  const bodyStart = text.search(/^##\s/m)
  const body = bodyStart >= 0 ? text.slice(bodyStart).replace(/^##[^\n]*\n/, '') : text
  const userMatch = body.match(/\*\*我\*\*：([\s\S]*?)(?=\n\n\*\*助手\*\*：|$)/)
  const user = userMatch ? userMatch[1].trim() : ''
  const assistantMatch = body.match(/\*\*助手\*\*：([\s\S]*)$/)
  const assistant = assistantMatch ? assistantMatch[1].trim() : ''
  return { title, user, assistant, body }
}

function isToolNoise(text) {
  const t = text.trim()
  if (!t) return false
  return TOOL_NOISE_PATTERNS.some((re) => re.test(t))
}

function isNotUser(text) {
  const t = text.trim()
  if (!t) return false
  return NOT_USER_PREFIXES.some((p) => t.startsWith(p))
}

// ── 自动定题：给「标题是标记语言、但正文有真结论」的条目起个能读的名字 ──
//
// 真机样本（gateway / 2026-11-22）：
//
//   标题：`<system-reminder data-role="error-recovery">Model error retr`
//   正文：**本轮完成：真机验证 → 发现三处新问题 → 修复 → 交付新包** + 详细报告
//
// 标题是客户端的控制标签，但助手侧有完整的排查结论 —— 这种条目**必须留**，
// 只是标题要换成「人话里的一句结论」。
//
// 取法：助手正文里第一行「像结论」的文字（去掉 markdown 标记、去掉列表符号），
// 截到 60 字。找不到就用用户侧第一行。
function autoTitle(section) {
  const { user, assistant } = sectionParts(section)
  const pick = (text) => {
    for (const raw of text.split('\n')) {
      const line = raw.trim()
      if (line.length < 6) continue
      // 跳过标题、代码围栏、表格、列表符号、纯标记行
      if (line.startsWith('#') || line.startsWith('```') || line.startsWith('|')) continue
      if (line.startsWith('**') && line.endsWith('**')) {
        // 整行加粗的，通常就是结论句
        return tidyTitle(line.replace(/^\*\*|\*\*$/g, ''))
      }
      if (/^[\*\-•\d\.\s]/.test(line)) {
        const plain = line.replace(/^[\*\-•\d\.\s]+/, '').trim()
        if (plain.length >= 6) return tidyTitle(plain)
        continue
      }
      if (line.length >= 6) return tidyTitle(line)
    }
    return ''
  }
  return pick(assistant) || pick(user) || '留存记录'
}

// 标题里的 markdown 残留要清干净：
//   `**第一批解决"两条相似记忆留谁"**：已经落地 information_score；…`
//    ↑ 开头两个 `**` 没有配对闭括号，读起来像乱码
function tidyTitle(text) {
  let out = String(text || '')
    .replace(/`+/g, '')
    .replace(/\s+/g, ' ')
    .trim()
  // 去掉没有配对的 `**`
  const marks = out.match(/\*\*/g)
  if (marks && marks.length % 2 === 1) out = out.replace(/\*\*/g, '')
  else out = out.replace(/\*\*/g, '')
  // 去掉首尾的标点
  out = out.replace(/^[\s，,。、：:；;！!？?\-—]+/, '').replace(/[\s，,。、：:；;！!？?]+$/, '')
  return out.slice(0, 60)
}

// ── 标题去壳 ────────────────────────────────────────────────────
//
// 真机 gateway 75 条里，**60 条标题一模一样**：
//
//   `**我**：动第一批，第二批和第三批我还没理解是什么意思，需要你解释`
//   `**我**：Please continue with the conversation based on the sum`
//
// 原因有两层：
//
//   1. 标题直接截了正文第一行，而正文第一行就是 `**我**：…` —— 说话人标记
//      被一起截进来了。标题应该是**内容**，不是**结构**。
//   2. 用户侧被客户端消息顶替时，标题也跟着变成客户端消息的原文。
//
// 处理：先剥掉 `**我**：` / `**助手**：` 前缀；剥完如果还是客户端消息
// （或太短），改用助手结论首句自动定题。
//
// 关键约束：**同一文件里标题必须能区分开**。否则检索时会一次性命中
// 十几条同名条目，用户看到的就是「搜出来全是同一句话」。
function normalizeTitle(section, currentTitle) {
  const { user, assistant } = sectionParts(section)
  let title = String(currentTitle || '').trim()
  // 1) 剥说话人标记
  title = title.replace(/^\*\*(我|助手|user|assistant)\*\*[：:]\s*/, '')
  // 2) 剥掉残留的正文残片（标题被截断在 JSON 里时会带上尾巴）
  title = title.replace(/","updated":\d+\} -->.*$/, '').trim()
  // 3) 剥掉行尾的 `\n## HH:MM · ...`（标题行被拼进了正文）
  title = title.replace(/\\n##\s+\d{2}:\d{2}.*$/s, '').trim()
  // 4) 剥掉重复的 `\n` 字面量
  title = title.replace(/\\n/g, ' ').trim()
  // 5) 还是客户端消息 / 空 / 太短 → 用助手结论自动定题
  const isClientNoise =
    !title ||
    isNotUserTitle(title) ||
    isNotUser(title) ||
    title.length < 6
  if (isClientNoise) {
    const auto = autoTitle(section)
    return auto
  }
  return title.slice(0, 80)
}

// ── 分类 ────────────────────────────────────────────────────────
// 返回 { action, reason }
//   delete      整条没价值 → 删 section
//   retitle     正文有价值、标题坏 → 只改标题
//   stub-user   正文有价值、用户侧是工具回执 → 用户侧换成占位说明
//   keep        正常
//   review      拿不准，列出不动
//
// ── 为什么 agent 维度是必需的 ────────────────────────────────
// 库里三种 agent 的成因完全不同，同一套判据套上去必错一边：
//
//   gateway      存留写入（re'm 注入 + 响应后留存）。一轮 = 用户原话 + 结论，
//                坏掉的方式是「用户侧被客户端控制消息顶替」。
//   import       会话文件切片。一轮 = 一个窗口 slice，坏掉的方式是
//                「窗口切在工具输出中间」——正文本身就是碎片，不修。
//   auto-extract l2 沉淀。都是蒸馏出来的结论（「排障必须看真实窗口」），
//                几乎不脏，动它就是毁东西。
//
// `agent` 来自 **frontmatter**（见 `parseContainer` 的说明），不是 mem 块。
//
// ── 用户已定的处置（2026-11-22）──────────────────────────────────
//   * `import`：**全删**。导入逻辑本身已经重写过（剥信封、抽 `<user_query>`、
//     发言级碎片判据、项目名取顶层 `cwd`），旧条目全是按老逻辑切出来的碎片，
//     逐条挑着删只会留下「老逻辑的漏网之鱼」。删干净再按新逻辑重导。
//   * `gateway`：**一条都不删**，只把坏标题改掉。用户原话和助手结论都在里面，
//     这是唯一真正的「学到的经验」所在。
//   * `auto-extract`：不动。
function classify(section, front) {
  const { title, user, assistant, body } = sectionParts(section)
  const agent = String(front?.agent ?? '')
  const metaTitle = String(section.meta?.title ?? title)

  // auto-extract 是蒸馏结论，一律不动 —— 误删它等于把学到的经验扔掉
  if (agent === 'auto-extract') return { action: 'keep', reason: '' }

  // ── 导入切片：整条作废，等新逻辑重导 ──────────────────────
  if (agent === 'import') {
    return { action: 'delete', reason: '旧导入条目（按老逻辑切出的碎片，整条重导）' }
  }

  // ── gateway 留存：一条都不删，只修标题 ──────────────────────
  // 助手侧没有人类结论的（整条都是工具调用 / 客户端标记）→ 真的没价值，删。
  const hasConclusion = assistant && assistant.length >= 20 && !isToolNoise(assistant)
  if (!hasConclusion) {
    return { action: 'delete', reason: '助手侧是机器产出，整条没有人类结论' }
  }

  // 用户侧被工具回执顶替的条目，**只换用户侧**，不删 —— 助手侧结论是真经验。
  const userIsTool = isNotUser(user)
  const newTitle = normalizeTitle(section, metaTitle)
  const titleChanged = newTitle !== metaTitle

  if (userIsTool && titleChanged) {
    return {
      action: 'stub-user',
      reason: `用户侧非原话（${shortPrefix(user)}），标题也要换`,
      newTitle,
    }
  }
  if (userIsTool) {
    return { action: 'stub-user', reason: `用户侧非原话（${shortPrefix(user)}），助手侧有结论` }
  }
  if (titleChanged) {
    return { action: 'retitle', reason: '标题带说话人标记/客户端指令，改成内容摘要', newTitle }
  }
  return { action: 'keep', reason: '' }
}

function shortPrefix(text) {
  return text.slice(0, 24).replace(/\s+/g, ' ')
}

// stub-user：把不可信的用户侧换成一行说明，并保留助手结论
const STUB_USER = '_（本轮触发是客户端控制消息 / 工具回执，不是用户原话；结论见下）_'
function applyStubUser(section, newTitle) {
  const text = section.lines.join('\n')
  const userMatch = text.match(/(\*\*我\*\*：)([\s\S]*?)(?=\n\n\*\*助手\*\*：|$)/)
  let replaced = userMatch ? text.replace(userMatch[0], `**我**：${STUB_USER}`) : text
  // 同时改标题（标题是元数据 + `## HH:MM · title` 两处，都要动）
  if (newTitle) {
    replaced = replaced.replace(/^(##\s+[^·]*·\s*).*$/m, `$1${newTitle}`)
    replaced = replaced.replace(/^<!-- mem (\{.*\}) -->\s*$/m, (whole, raw) => {
      try {
        const meta = JSON.parse(raw)
        meta.title = newTitle
        return `<!-- mem ${JSON.stringify(meta)} -->`
      } catch {
        return whole
      }
    })
  }
  return { ...section, lines: replaced.split('\n') }
}

function applyRetitle(section, newTitle) {
  const text = section.lines.join('\n')
  const replaced = text.replace(/^(##\s+[^·]*·\s*).*$/m, `$1${newTitle}`)
  // meta 里的 title 也要同步，否则下一次 rebuild 又把它写回来
  const fixedMeta = replaced.replace(/^<!-- mem (\{.*\}) -->\s*$/m, (whole, raw) => {
    try {
      const meta = JSON.parse(raw)
      meta.title = newTitle
      return `<!-- mem ${JSON.stringify(meta)} -->`
    } catch {
      return whole
    }
  })
  return { ...section, lines: fixedMeta.split('\n') }
}

// ── 遍历 ────────────────────────────────────────────────────────
function walkMd(dir, out = []) {
  if (!fs.existsSync(dir)) return out
  for (const item of fs.readdirSync(dir, { withFileTypes: true })) {
    const p = path.join(dir, item.name)
    if (item.isDirectory()) walkMd(p, out)
    else if (item.name.endsWith('.md') && !item.name.includes('.bak')) out.push(p)
  }
  return out
}

function main() {
  console.log(`记忆库根：${ROOT}`)
  console.log(APPLY ? '模式：APPLY（会改文件）' : '模式：dry-run（只打印）')
  if (ONLY_PROJECT) console.log(`限定项目：${ONLY_PROJECT}`)
  console.log('─'.repeat(78))

  const base = ONLY_PROJECT
    ? path.join(ROOT, 'projects', ONLY_PROJECT)
    : ROOT
  const files = walkMd(base)
  console.log(`扫描 ${files.length} 个 .md`)

  const stats = { delete: 0, retitle: 0, 'stub-user': 0, keep: 0, review: 0 }
  const plan = [] // { file, sectionIndex, action, reason, newTitle }
  const reviewList = []

  for (const file of files) {
    const text = fs.readFileSync(file, 'utf8')
    const eol = text.includes('\r\n') ? '\r\n' : '\n'
    const { prefix, front, sections } = parseContainer(text.replace(/\r\n/g, '\n'))
    sections.forEach((section, index) => {
      if (!section.meta) return
      stats.keep += 1
      const verdict = classify(section, front)
      if (verdict.action === 'keep') return
      stats.keep -= 1
      stats[verdict.action] += 1
      plan.push({ file, index, action: verdict.action, reason: verdict.reason, newTitle: verdict.newTitle, section })
      if (verdict.action === 'review') reviewList.push({ file, index, verdict, section })
    })
    plan.__eol ??= eol
  }

  console.log('')
  console.log('分类：')
  for (const [k, v] of Object.entries(stats)) console.log(`  ${k.padEnd(10)} ${v}`)
  console.log('')

  const byReason = new Map()
  for (const p of plan) {
    // 原因里的「X 字结论」是样例，不该进聚合 key（否则 474 条会散成 474 行）
    const key = `${p.action} · ${p.reason.replace(/有\s*\d+\s*字结论/, '有结论').replace(/（[^）]*）/, '')}`
    byReason.set(key, (byReason.get(key) ?? 0) + 1)
  }
  console.log('按原因：')
  for (const [k, v] of [...byReason.entries()].sort((a, b) => b[1] - a[1])) {
    console.log(`  ${String(v).padStart(5)}  ${k}`)
  }
  console.log('')

  if (reviewList.length) {
    console.log(`需要人工看（${reviewList.length} 条，本次不动）：`)
    for (const r of reviewList.slice(0, 20)) {
      console.log(`  ${path.basename(r.file)}#${r.index}  ${r.verdict.reason}`)
      console.log(`     title: ${sectionParts(r.section).title.slice(0, 70)}`)
    }
    if (reviewList.length > 20) console.log(`  …另有 ${reviewList.length - 20} 条`)
    console.log('')
  }

  // ── 预览改标题后的结果（不落盘）──────────────────────────────
  // 真机 gateway 文件 75 条里有 60 条同名标题 —— 光看计数看不出改得好不好，
  // 必须把新标题打出来看。这个预览在 apply 之前跑，改坏了还能改判据。
  //
  // 预览要跟 apply 用**同一套去重逻辑**，否则看到的标题不是最终结果。
  const preview = plan.filter((p) => p.action === 'retitle' || p.action === 'stub-user')
  if (preview.length) {
    console.log(`改标题预览（前 ${Math.min(preview.length, 25)} 条，共 ${preview.length} 条）：`)
    const previewTitles = new Set()
    const perFileSeen = new Map()
    for (const p of preview.slice(0, 25)) {
      const old = String(p.section?.meta?.title ?? '').slice(0, 46)
      let next = String(p.newTitle ?? '(不改)').slice(0, 60)
      // 与 apply 一致的去重：同文件内同名加序号
      const key = `${p.file}`
      if (!perFileSeen.has(key)) perFileSeen.set(key, new Set())
      const fileSet = perFileSeen.get(key)
      if (fileSet.has(next)) {
        let n = 2
        while (fileSet.has(`${next}（${n}）`)) n += 1
        next = `${next}（${n}）`
      }
      fileSet.add(next)
      const mark = previewTitles.has(next) ? '  ' : '* '
      previewTitles.add(next)
      console.log(`${mark}旧: ${old}`)
      console.log(`  新: ${next}`)
    }
    console.log('')
  }

  if (!APPLY) {
    console.log('dry-run 结束。加 --apply 才会真改（会先自动备份）。')
    return
  }

  // ── 真改：逐文件重写 ──────────────────────────────────────────
  const perFile = new Map()
  for (const p of plan) {
    if (p.action === 'review') continue
    if (!perFile.has(p.file)) perFile.set(p.file, [])
    perFile.get(p.file).push(p)
  }

  // ── 先备份整个记忆库 ─────────────────────────────────────────
  // 这次要删 6000+ 条。判据万一在某条上判错，备份就是唯一的回头路。
  // 备份放在 `.workbuddy/tmp/cleanup-backup/<时间戳>/`，跟记忆库**不同盘位**
  // 是做不到的（都在用户目录下），但至少不在记忆库目录里 ——
  // 放在里面会被 `index-scan` 当成记忆扫进去。
  const stamp = new Date()
    .toISOString()
    .replace(/[-:T]/g, '')
    .slice(0, 14)
  // `fileURLToPath` 而不是 `new URL(...).pathname` —— 后者在 Windows 上会留下
  // `/D:/...` 这种带前导斜杠的怪路径，拼出来的备份目录会跑到根目录去。
  const scriptDir = path.dirname(fileURLToPath(import.meta.url))
  const backupDir = path.resolve(
    scriptDir,
    '..',
    '.workbuddy',
    'tmp',
    'cleanup-backup',
    `memory-${stamp}`,
  )
  fs.mkdirSync(backupDir, { recursive: true })
  const allFiles = walkMd(ROOT)
  for (const file of allFiles) {
    const rel = path.relative(ROOT, file)
    const dest = path.join(backupDir, rel)
    fs.mkdirSync(path.dirname(dest), { recursive: true })
    fs.copyFileSync(file, dest)
  }
  console.log(`已备份 ${allFiles.length} 个文件 → ${backupDir}`)
  console.log('')

  let changedFiles = 0
  let removedSections = 0
  let retitled = 0
  let stubbed = 0
  for (const [file, items] of perFile) {
    const raw = fs.readFileSync(file, 'utf8')
    const eol = raw.includes('\r\n') ? '\r\n' : '\n'
    const { prefix, front, sections } = parseContainer(raw.replace(/\r\n/g, '\n'))
    const drop = new Set(items.filter((i) => i.action === 'delete').map((i) => i.index))
    const keep = []
    // ── 标题去重 ───────────────────────────────────────────────
    // 真机 gateway 文件里 75 条有 60 条同名（`**我**：动第一批…`）。
    // 同名标题在检索里会**一次性全命中**，用户看到的还是那句话。
    // 所以同一个文件里，标题重复就加上序号前缀，保证可区分。
    const usedTitles = new Set()
    const uniqueTitle = (base) => {
      if (!usedTitles.has(base)) {
        usedTitles.add(base)
        return base
      }
      let n = 2
      while (usedTitles.has(`${base}（${n}）`)) n += 1
      const next = `${base}（${n}）`
      usedTitles.add(next)
      return next
    }
    sections.forEach((section, index) => {
      if (drop.has(index)) {
        removedSections += 1
        return
      }
      const item = items.find((i) => i.index === index)
      if (!item) {
        keep.push(section)
        return
      }
      if (item.action === 'retitle') {
        keep.push(applyRetitle(section, uniqueTitle(item.newTitle)))
        retitled += 1
      } else if (item.action === 'stub-user') {
        keep.push(applyStubUser(section, item.newTitle ? uniqueTitle(item.newTitle) : ''))
        stubbed += 1
      } else {
        keep.push(section)
      }
    })
    // 一个 section 都不剩：整份文件删掉（空容器留着只会污染扫描）
    if (!keep.length) {
      fs.renameSync(file, `${file}.cleaned-empty`)
      changedFiles += 1
      continue
    }
    const next = rebuildContainer(prefix, keep, eol)
    if (next !== raw) {
      fs.writeFileSync(file, next, 'utf8')
      changedFiles += 1
    }
  }

  console.log('')
  console.log(`改了 ${changedFiles} 个文件`)
  console.log(`  删除 section ${removedSections}`)
  console.log(`  改标题 ${retitled}`)
  console.log(`  替换不可信用户侧 ${stubbed}`)
  console.log('')
  console.log('下一步：让 index-scan 重建索引，或重启应用（启动时也会 warm/rebuild）。')
}

main()
