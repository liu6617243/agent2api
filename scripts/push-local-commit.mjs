// 用 GitHub Git Data API 推送**一个已存在的本地提交**，使远端 commit 的 sha
// 与本地逐字节相同。
//
// ── 为什么需要它 ────────────────────────────────────────────────
// 本机到 `github.com:443` 的链路经常整段不通（`Recv failure: Connection was
// reset` / `CONNECT tunnel 502`），而 `api.github.com` 是通的。于是 `git push`
// 不可用，但 GitHub REST API 可用 —— 这条路是本机推代码的**唯一稳定通道**。
//
// 为什么不是 `commit_multi.mjs`（新建 commit 的那个）：它不带 author /
// committer / date，GitHub 算出的 sha 与本地不同 → 本地与远端历史分叉，
// 后续 fetch / pull 会冲突。本脚本复刻本地 commit 的全部元数据
// （tree / parent / author / committer / message），两边 sha 一致。
//
// ── 闸门（重要）──────────────────────────────────────────────────
// 这个脚本**曾经住在 `.workbuddy/tmp/`**（被 `.gitignore` 忽略、不受版本管理），
// 于是形成了一条绕过交付的暗道：
//
//   push_local_commit.mjs  →  推上去了，但 CI 没跑、安装包没出
//
// 2026-11-26 就是这么漏掉一次安装包的。现在这份脚本进了仓库，并且**自己
// 也带闸门**：推 `main-patched` 必须显式声明「要么我在交付链路里，要么我
// 明确知道这次不出包」，否则拒绝执行。
//
//   A2A_DELIVER=1   由交付链路（deliver.mjs）调用 —— 允许
//   A2A_SKIP_BUILD=1 人明确说了这次不出包（如纯文档提交）—— 允许
//
// 与 `.githooks/pre-push` 用**同一个词表**，避免出现第二套放行语义。
//
// ── 用法 ────────────────────────────────────────────────────────
//   node scripts/push-local-commit.mjs --repo owner/repo --branch main-patched [--sha <sha>]
//
// 作为模块：
//   import { pushViaApi } from './push-local-commit.mjs'
//   await pushViaApi({ token, repo, branch, localSha, cwd })
import fs from 'node:fs'
import { execFileSync } from 'node:child_process'
import { pathToFileURL } from 'node:url'

// ── 闸门 ──────────────────────────────────────────────────────────
const DELIVER_BRANCH = 'main-patched'

export function assertPushAllowed({ branch, env = process.env }) {
  if (branch !== DELIVER_BRANCH) return
  if (env.A2A_DELIVER === '1') return
  if (env.A2A_SKIP_BUILD === '1') return
  throw new Error(
    [
      `拒绝推送 ${branch}：这条分支的交付是「推代码 + 出安装包」一步做完。`,
      '',
      '正确做法（推荐，会自动跑测试并出 Windows 安装包）：',
      '    node scripts/deliver.mjs',
      '',
      '确实只想推代码、这次明确不出包（例如纯文档提交）：',
      '    A2A_SKIP_BUILD=1 node scripts/push-local-commit.mjs --repo owner/repo --branch main-patched',
    ].join('\n'),
  )
}

// ── API 客户端 ────────────────────────────────────────────────────
const API = 'https://api.github.com'

function headersFor(token) {
  return {
    Authorization: `Bearer ${token}`,
    Accept: 'application/vnd.github+json',
    'Content-Type': 'application/json',
    'User-Agent': 'agent2api-deliver',
  }
}

async function call(token, method, path, body) {
  const res = await fetch(API + path, {
    method,
    headers: headersFor(token),
    body: body === undefined ? undefined : JSON.stringify(body),
  })
  const text = await res.text()
  let json = null
  try {
    json = text ? JSON.parse(text) : null
  } catch {
    json = { raw: text }
  }
  if (!res.ok) {
    throw new Error(`${method} ${path} -> ${res.status} ${text.slice(0, 600)}`)
  }
  return json
}

/**
 * 把一个**已存在的本地提交**推到远端分支。
 *
 * @returns {Promise<{ok: boolean, sha?: string, alreadyUpToDate?: boolean, reason?: string, message?: string}>}
 */
export async function pushViaApi({ token, repo, branch, localSha, cwd = process.cwd(), log = () => {} }) {
  // `-c core.quotepath=false` 不能少：默认 `quotepath=true` 会把非 ASCII 路径
  // 转义成 `".workbuddy/\344\270\212\346\270\272..."` 这种带引号的八进制串。
  // 拿去当 tree 条目发出去，远端建出来的 tree 必然和本地对不上 ——
  // 表现为「本地是 X、远端是 Y」然后中止。2026-11-27 改中文名的
  // `.workbuddy/修改清单.md` 时踩到，之前几次改动恰好都是纯 ASCII 路径。
  const git = (...args) =>
    execFileSync('git', ['-c', 'core.quotepath=false', ...args], {
      cwd,
      encoding: 'utf8',
      maxBuffer: 64 * 1024 * 1024,
    }).trim()

  // ── 1. 本地 commit 的元数据 ─────────────────────────────────────
  const raw = git('cat-file', '-p', localSha)
  const treeSha = /^tree ([0-9a-f]{40})$/m.exec(raw)?.[1]
  const parentSha = /^parent ([0-9a-f]{40})$/m.exec(raw)?.[1]
  const authorLine = /^author (.+) <(.+)> (\d+) ([+-]\d{4})$/m.exec(raw)
  const committerLine = /^committer (.+) <(.+)> (\d+) ([+-]\d{4})$/m.exec(raw)
  const message = raw.slice(raw.indexOf('\n\n') + 2)
  if (!treeSha || !parentSha || !authorLine || !committerLine) {
    return { ok: false, reason: 'parse', message: '无法解析本地 commit 元数据' }
  }

  // GitHub 的 commit API 会**剥掉** message 末尾的换行再入库：
  // 本地 `git commit -F` 保留的尾换行会让 sha 对不上。这里主动去掉，
  // 让 GitHub 算出的 sha 与本地一致（本地侧再按 GitHub 返回的 sha 对齐）。
  const messageForApi = message.replace(/\n+$/, '')

  // git 的时区格式是 ±HHMM（无冒号），要拆成小时/分钟。
  // 注意：不能转成 UTC 的 'Z' —— GitHub 会用传入 ISO 的偏移量重建 commit 里的
  // 时区字段，转成 Z 会把 "+0800" 变成 "+0000"，sha 必然对不上。
  const toIso = (ts, tz) => {
    const sign = tz.startsWith('-') ? -1 : 1
    const digits = tz.slice(1)
    const h = Number(digits.slice(0, 2))
    const m = Number(digits.slice(2, 4))
    if (!Number.isFinite(h) || !Number.isFinite(m)) {
      throw new Error(`无法解析时区: ${tz}`)
    }
    const offsetMin = sign * (h * 60 + m)
    const local = new Date((Number(ts) + offsetMin * 60) * 1000)
    if (Number.isNaN(local.getTime())) {
      throw new Error(`无法转换时间戳: ${ts} ${tz}`)
    }
    const pad = (n) => String(n).padStart(2, '0')
    const isoLocal = `${local.getUTCFullYear()}-${pad(local.getUTCMonth() + 1)}-${pad(local.getUTCDate())}` +
      `T${pad(local.getUTCHours())}:${pad(local.getUTCMinutes())}:${pad(local.getUTCSeconds())}`
    const off = `${sign < 0 ? '-' : '+'}${pad(h)}:${pad(m)}`
    return `${isoLocal}${off}`
  }
  const author = { name: authorLine[1], email: authorLine[2], date: toIso(authorLine[3], authorLine[4]) }
  const committer = {
    name: committerLine[1],
    email: committerLine[2],
    date: toIso(committerLine[3], committerLine[4]),
  }
  log(`local tree   : ${treeSha}`)
  log(`local parent : ${parentSha}`)

  // ── 2. 远端分支头必须是本地 parent（否则是别人的新提交，不能覆盖）──
  const ref = await call(token, 'GET', `/repos/${repo}/git/ref/heads/${branch}`)
  const remoteHead = ref.object.sha
  log(`remote head  : ${remoteHead}`)

  if (remoteHead === localSha) {
    return { ok: true, sha: localSha, alreadyUpToDate: true }
  }
  if (remoteHead !== parentSha) {
    return {
      ok: false,
      reason: 'diverged',
      message:
        `远端 head(${remoteHead.slice(0, 8)}) != 本地 parent(${parentSha.slice(0, 8)})，` +
        '拒绝覆盖 —— 远端有别人的新提交，先 fetch 对齐再推。',
    }
  }

  // ── 3. 逐个 blob 上传（只传变化的部分）────────────────────────
  // 未变动的 blob 远端已有，直接复用其 sha，不重复上传。
  const changedSet = new Set(git('diff', '--name-only', `${parentSha}..${localSha}`).split('\n').filter(Boolean))

  // 删除的文件：diff 里是 D。GitHub 建 tree 时用 `sha: null` 表示「删除该条目」，
  // 若只传变化的 blob，被删的文件会残留在 base_tree 里（tree sha 就对不上了）。
  const deleted = git('diff', '--name-status', `${parentSha}..${localSha}`)
    .split('\n')
    .filter(Boolean)
    .map((l) => /^D\t(.+)$/.exec(l)?.[1])
    .filter(Boolean)
  log(`changed files: ${changedSet.size} | deleted: ${deleted.length}`)

  // 3b. 取本地 tree 的完整条目列表（mode/path/sha/type），用 `ls-tree -r` 拿扁平结构
  function readTree(sha) {
    const out = []
    const txt = git('ls-tree', '-r', sha)
    for (const line of txt.split('\n')) {
      if (!line.trim()) continue
      const m = /^(\d{6}) (\w+) ([0-9a-f]{40})\t(.+)$/.exec(line)
      if (!m) continue
      out.push({ path: m[4], mode: m[1], type: m[2], sha: m[3] })
    }
    return out
  }

  const localEntries = readTree(treeSha)
  log(`local tree entries: ${localEntries.length}`)

  for (const e of localEntries) {
    if (!changedSet.has(e.path)) continue
    const rawBuf = execFileSync('git', ['cat-file', 'blob', e.sha], {
      cwd,
      maxBuffer: 64 * 1024 * 1024,
    })
    const blob = await call(token, 'POST', `/repos/${repo}/git/blobs`, {
      content: rawBuf.toString('base64'),
      encoding: 'base64',
    })
    if (blob.sha !== e.sha) {
      return {
        ok: false,
        reason: 'blob',
        message: `blob sha 不一致: ${e.path} local=${e.sha} remote=${blob.sha}`,
      }
    }
  }

  // ── 4. 建 tree（base = parent 的 tree）─────────────────────────
  // 只传**变化**的条目：base_tree 已含父提交的全部条目，GitHub 会继承其余部分。
  // 全量条目会触发 502（payload 过大）。
  const parentTree = (await call(token, 'GET', `/repos/${repo}/git/commits/${parentSha}`)).tree.sha
  const treePayload = localEntries
    .filter((e) => changedSet.has(e.path))
    .map((e) => ({ path: e.path, mode: e.mode, type: e.type === 'commit' ? 'commit' : 'blob', sha: e.sha }))

  // 被删除的路径在 localEntries 里已经不存在，必须显式补一条 sha=null 的条目，
  // 否则 base_tree 会把它带回来。mode 取自父提交里的原值（可执行文件是 100755，
  // 写死 100644 会让 tree sha 对不上）。
  for (const path of deleted) {
    const orig = git('ls-tree', parentSha, '--', path)
    const mode = /^(\d{6})\s/.exec(orig)?.[1] || '100644'
    treePayload.push({ path, mode, type: 'blob', sha: null })
  }
  log(`tree payload entries: ${treePayload.length}（含删除 ${deleted.length}）`)

  const newTree = await call(token, 'POST', `/repos/${repo}/git/trees`, {
    base_tree: parentTree,
    tree: treePayload,
  })
  log(`new tree     : ${newTree.sha}${newTree.sha === treeSha ? ' ✓ 与本地一致' : ` ✗ 本地是 ${treeSha}`}`)
  if (newTree.sha !== treeSha) {
    return {
      ok: false,
      reason: 'tree',
      message: `远端建出的 tree(${newTree.sha}) 与本地(${treeSha}) 不一致，已中止，未改分支。`,
    }
  }

  // ── 5. 建 commit（复刻完整元数据）────────────────────────────
  const commit = await call(token, 'POST', `/repos/${repo}/git/commits`, {
    message: messageForApi,
    tree: treeSha,
    parents: [parentSha],
    author,
    committer,
  })
  log(`new commit   : ${commit.sha}`)

  // 若 sha 不一致（本地 commit 尾部多了换行），把 GitHub 建出的 commit object
  // 写入本地对象库并把分支指过去 —— 两边 sha 就一致了，历史不分裂。
  if (commit.sha !== localSha) {
    const trimmed = Buffer.from(raw.replace(/\n$/, ''), 'utf8')
    const localSha2 = execFileSync('git', ['hash-object', '-t', 'commit', '-w', '--stdin'], {
      cwd,
      input: trimmed,
    })
      .toString()
      .trim()
    if (localSha2 !== commit.sha) {
      return {
        ok: false,
        reason: 'align',
        message: `本地对齐失败: 本地算出 ${localSha2}，GitHub 是 ${commit.sha}`,
      }
    }
    execFileSync('git', ['reset', '--soft', commit.sha], { cwd })
    log(`本地已对齐   : ${commit.sha}`)
  }

  // ── 6. 移动分支（非 force）─────────────────────────────────────
  await call(token, 'PATCH', `/repos/${repo}/git/refs/heads/${branch}`, {
    sha: commit.sha,
    force: false,
  })
  log(`branch moved : ${branch} -> ${commit.sha}`)
  return { ok: true, sha: commit.sha }
}

// ── CLI ───────────────────────────────────────────────────────────
if (process.argv[1] && import.meta.url === pathToFileURL(process.argv[1]).href) {
  const argv = process.argv.slice(2)
  const val = (flag, d = '') => {
    const i = argv.indexOf(flag)
    return i >= 0 && argv[i + 1] ? argv[i + 1] : d
  }
  const repo = val('--repo') || process.env.A2A_REPO || 'liu6617243/agent2api'
  const branch = val('--branch') || process.env.A2A_REF || 'main-patched'
  const localSha = val('--sha') || execFileSync('git', ['rev-parse', 'HEAD'], { encoding: 'utf8' }).trim()

  const token =
    process.env.GH_TOKEN ||
    process.env.GITHUB_TOKEN ||
    process.env.A2A_TOKEN ||
    (() => {
      for (const p of ['.workbuddy/tmp/gh_token_value.txt', '.workbuddy/tmp/github_pat.txt']) {
        if (fs.existsSync(p)) {
          const t = fs.readFileSync(p, 'utf8').trim()
          if (t) return t
        }
      }
      return ''
    })()

  if (!token) {
    console.error('找不到 GitHub 凭据（GH_TOKEN / GITHUB_TOKEN / .workbuddy/tmp/gh_token_value.txt）')
    process.exit(2)
  }

  try {
    assertPushAllowed({ branch })
  } catch (e) {
    console.error(e.message)
    process.exit(1)
  }

  const out = await pushViaApi({
    token,
    repo,
    branch,
    localSha,
    log: (m) => console.log(m),
  })
  if (!out.ok) {
    console.error(`推送失败（${out.reason}）：${out.message}`)
    process.exit(1)
  }
  console.log(out.alreadyUpToDate ? '远端已是最新，无需推送。' : `推送完成：${branch} -> ${out.sha}`)
  if (out.alreadyUpToDate) {
    console.error('')
    console.error('注意：远端已有这个提交，但本次没有触发任何流水线。')
    console.error('要跑测试并出 Windows 安装包：node scripts/deliver.mjs --no-push')
  }
}
