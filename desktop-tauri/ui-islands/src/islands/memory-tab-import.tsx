import * as React from 'react'
import { Badge, Button, Input, Popover, PopoverContent, PopoverTrigger } from '@ui'
import { t } from '../i18n'
import * as m from './memory-api'
import { Empty, Panel, Section, num, stamp, timeText, useLoad } from './memory-shared'

/**
 * 导入页：来源清册（一屏卡片）+ 干跑预览 + 真导入 + 全库去重巡检。
 *
 * ── 为什么主角从「一张表」变成「一屏卡片」 ──────────────────
 * 原来的来源区是一行一条：名字 / 路径 / 开关 / 移除。它能回答「我配了几条」，
 * 回答不了用户真正在问的三个问题：
 *
 *   1. 这台机器上**有哪些 Agent 的历史**？（→ 12 个内置来源，找到了几个）
 *   2. 每个来源里**到底有多少东西、上次读到哪**？（→ 体量 + 水位）
 *   3. 我现在**该做哪个动作**？（→ 导入该来源 / 改路径 / 忽略）
 *
 * 卡片把这四件事固定在同一位置：标题是名字，右上角一枚状态徽标，
 * 中间四行恒为「路径 / 格式 / 体量 / 已导入」，底部主按钮 + ⋯ 菜单。
 * 用户扫一眼就知道哪个能导、哪个要改路径、哪个已经导过。
 *
 * ── 为什么状态徽标是四种而不是两种 ──────────────────────────
 * 三种「不能用」的原因必须分开说，因为用户的下一步动作完全不同：
 *   · 未指定   —— 这个来源天生没有默认路径（Markdown 笔记目录）→ 去填路径；
 *   · 路径不存在 —— 探测不到（没装这个工具，或装到了别处）→ 去改路径；
 *   · 已忽略   —— 用户自己关的（装了但不想导）→ 去启用；
 *   · 未启用   —— 盘上有、但没写进生效清单 → 去启用。
 * 混成一个灰色「不可用」，用户会反复去改一个本来就不想导的来源的路径。
 *
 * ── 为什么「干跑」是强制的一步 ──────────────────────────────
 * 导入是这套系统里唯一的**批量写盘**：一次几千条，写错了会污染检索结果，
 * 更糟的是把隐私内容搬进一个会被 MCP 工具暴露出去的地方。所以流程固定两段，
 * 且后端在 `import.dryRunFirst` 为真时拒绝「没预览就直接导」的请求 ——
 * 防的不是新手，是「用户在另一个标签页里点了按钮」。
 */

/** 读法 → 「格式」那一行显示什么。 */
const KIND_TEXT: Record<string, string> = {
  sqlite: 'SQLite 会话库 · 权威源',
  jsonl: 'JSONL 会话日志 · 准实时',
  md: 'Markdown 笔记目录',
  trae: 'Trae 加密会话库（内置 SQLCipher 解密）',
  antigravity: 'Antigravity 会话日志',
  dsh: 'JSONL 会话日志（zstd 压缩）',
}

function kindText(kind: string): string {
  return t(KIND_TEXT[kind] ?? kind)
}

/**
 * 状态徽标。判定顺序就是用户该做的事的顺序：先问「有没有路径」，
 * 再问「路径在不在」，再问「用户有没有关它」，最后才问「有没有进清单」。
 */
function SourceState({ row }: { row: m.ImportRegistryRow }): React.ReactElement {
  if (!row.hasPath) {
    return <span className='mm-src-state idle'>{t('未指定')}</span>
  }
  if (!row.exists) {
    return <span className='mm-src-state bad'>{t('✗ 路径不存在')}</span>
  }
  if (row.ignored) {
    return <span className='mm-src-state idle'>{t('已忽略')}</span>
  }
  if (!row.enabled) {
    return <span className='mm-src-state off'>{t('未启用')}</span>
  }
  return <span className='mm-src-state ok'>{t('✓ 已找到')}</span>
}

/**
 * 一张来源卡片。
 *
 * ── 为什么「导入该来源」在路径不存在时禁用而不是隐藏 ────────
 * 隐藏会让卡片变成「一张只有信息的僵尸卡」，用户会以为这个来源不支持导入。
 * 禁用 + 菜单里摆着「改路径」，才说得清「不是不支持，是现在读不到」。
 */
function SourceCard({
  row,
  busy,
  onImport,
  onEnable,
  onDisable,
  onEditPath,
  onReset,
  onRemove,
}: {
  row: m.ImportRegistryRow
  busy: boolean
  onImport: () => void
  onEnable: () => void
  onDisable: () => void
  onEditPath: (path: string) => void
  onReset: () => void
  onRemove: () => void
}): React.ReactElement {
  const [open, setOpen] = React.useState(false)
  const [editing, setEditing] = React.useState(false)
  const [draft, setDraft] = React.useState('')

  // 自定义来源与内置来源的区别只有一个：它没有「默认路径」可恢复，
  // 也没有「忽略」这一层状态（它离开清单就不存在了）。
  const builtin = !row.id.startsWith('custom:')

  const startEdit = () => {
    setOpen(false)
    // 编辑框里给**已保存的自定义路径**（没改过就留空），不预先填脱敏后的 `~` 形态 ——
    // 那样用户点保存会把 `~/.claude/projects` 原样写回 override，看着像改了其实没改。
    setDraft(row.custom || !builtin ? row.path : '')
    setEditing(true)
  }

  const commit = () => {
    onEditPath(draft.trim())
    setEditing(false)
  }

  const place = row.path || row.candidates[0] || ''
  const readable = row.exists && row.enabled

  return (
    <div className='mm-src-card'>
      <div className='mm-src-head'>
        <span className='mm-src-name' title={row.about}>{row.name}</span>
        <SourceState row={row} />
      </div>

      <div className='mm-src-rows'>
        <span>{t('路径')}</span>
        <span className='mm-src-path' title={place}>
          {row.path || (place ? t('{path}（未指定）', { path: place }) : t('（未指定）'))}
        </span>

        <span>{t('格式')}</span>
        <span className='mm-src-value'>{kindText(row.kind)}</span>

        <span>{t('体量')}</span>
        <span className='mm-src-value'>
          {row.exists
            ? (row.items > 0
              ? t('{n} 项 · {size}', { n: num(row.items), size: row.bytesText })
              : t('— · {size}', { size: row.bytesText }))
            : '—'}
        </span>

        <span>{t('已导入')}</span>
        <span className='mm-src-value mm-src-estimate'>
          {row.estimate || (row.exists ? t('还没导过') : '—')}
        </span>
      </div>

      {row.note ? <p className='mm-src-note'>{row.note}</p> : null}

      {editing ? (
        <div className='mm-src-edit'>
          <Input
            value={draft}
            spellCheck={false}
            placeholder={row.candidates[0] ?? t('目录或文件路径')}
            onChange={event => setDraft(event.target.value)}
            onKeyDown={event => {
              if (event.key === 'Enter') commit()
              if (event.key === 'Escape') setEditing(false)
            }}
          />
          <Button type='button' size='sm' disabled={busy} onClick={commit}>{t('保存')}</Button>
          <Button type='button' size='sm' variant='ghost' onClick={() => setEditing(false)}>{t('取消')}</Button>
        </div>
      ) : null}

      <div className='mm-src-foot'>
        <Button
          type='button'
          size='sm'
          disabled={busy || !readable}
          onClick={onImport}
        >
          {t('导入该来源')}
        </Button>
        <Popover open={open} onOpenChange={setOpen}>
          <PopoverTrigger
            render={<Button type='button' size='sm' variant='ghost' className='mm-src-more' aria-label={t('更多操作')} />}
          >
            <span aria-hidden>⋯</span>
          </PopoverTrigger>
          <PopoverContent align='start' sideOffset={4} className='w-[190px]'>
            <div className='mm-src-menu'>
              <Button type='button' size='sm' variant='ghost' onClick={startEdit}>
                {t('改路径')}
              </Button>
              {builtin ? (
                <>
                  <Button
                    type='button'
                    size='sm'
                    variant='ghost'
                    disabled={!row.exists}
                    onClick={() => {
                      setOpen(false)
                      if (row.ignored || !row.enabled) onEnable()
                      else onDisable()
                    }}
                  >
                    {row.ignored || !row.enabled ? t('启用该来源') : t('忽略该来源')}
                  </Button>
                  <Button
                    type='button'
                    size='sm'
                    variant='ghost'
                    disabled={!row.custom}
                    onClick={() => {
                      setOpen(false)
                      onReset()
                    }}
                  >
                    {t('恢复默认路径')}
                  </Button>
                </>
              ) : (
                <Button
                  type='button'
                  size='sm'
                  variant='ghost'
                  onClick={() => {
                    setOpen(false)
                    onRemove()
                  }}
                >
                  {t('移除该来源')}
                </Button>
              )}
            </div>
          </PopoverContent>
        </Popover>
      </div>
    </div>
  )
}

/**
 * 已经「认领」过的那次后台导入结果。
 *
 * ── 为什么必须是模块级而不是组件 state ──────────────────────
 * 页签是条件渲染，切走就把这个组件卸载了。后端的状态还在，回来时会重新查到
 * 「已完成」—— 如果只看状态就弹结果、就 reload 清册，那每切一次页签都会
 * 重放一次「导入完成」，像刚导完一样。记一下 `finishedAt` 就够了。
 */
let appliedResult = 0

/**
 * 订阅后台导入状态。
 *
 * ── 为什么要有这一层 ────────────────────────────────────────
 * 导入不是「一次请求」而是「一台在后台跑着的活」：用户可能点完就切去别的页签，
 * 几分钟后回来。状态必须能从后端重新取，而不是存在组件里 —— 组件会被卸载。
 * 所以：挂载即查一次，跑着就每 1.2 秒续一次。
 *
 * ── 为什么「显示结果」和「只处理一次」要分开 ────────────────
 * 切走再回来，用户应该**还看得见上次导了多少条**（否则又是一片空白，等于没做）；
 * 但清册刷新、弹一次结果这类副作用只能发生一次，否则每切一次页签都像刚导完。
 * 所以回调带 `fresh`：false 表示「同一批结果，已经认领过了」。
 */
function useImportJob(handlers: {
  onDone: (summary: m.ImportSummary, fresh: boolean) => void
  onFail: (error: string, fresh: boolean) => void
}): { job: m.ImportJob | null; refresh: () => void } {
  const [job, setJob] = React.useState<m.ImportJob | null>(null)
  // 回调放进 ref：effect 只跑一次（不重建轮询），但每次 tick 都要用最新的回调
  // —— 否则 `registry.reload` 这类每次渲染都会换引用的函数会一直用第一版。
  const latest = React.useRef(handlers)
  latest.current = handlers
  /**
   * 手动催一次。
   *
   * ── 为什么必须有它 ──────────────────────────────────────────
   * 状态机是「跑着才续查」：idle / done 之后轮询就停了（不空转是本意）。
   * 那用户刚点完「开始导入」时，轮询已经停了 —— 不催一下，进度条永远不出现。
   */
  const kick = React.useRef<() => void>(() => {})

  React.useEffect(() => {
    let alive = true
    let timer = 0
    let busy = false

    const tick = async () => {
      if (!alive || busy) return
      busy = true
      let wait = 0
      try {
        const next = await m.importStatus()
        if (!alive) return
        setJob(next)
        if (next.state === 'running') {
          wait = 1200
        } else if (next.finishedAt) {
          const fresh = next.finishedAt !== appliedResult
          if (fresh) appliedResult = next.finishedAt
          if (next.state === 'done' && next.summary) latest.current.onDone(next.summary, fresh)
          if (next.state === 'failed') latest.current.onFail(next.error || t('导入失败'), fresh)
        }
      } catch {
        // 后端重启 / 暂时不可用时退避重试，不要把一次抖动变成「状态丢了」
        wait = 3000
      } finally {
        busy = false
        if (alive && wait) timer = window.setTimeout(() => void tick(), wait)
      }
    }

    kick.current = () => {
      if (timer) window.clearTimeout(timer)
      void tick()
    }
    void tick()

    return () => {
      alive = false
      if (timer) window.clearTimeout(timer)
    }
  }, [])

  return { job, refresh: () => kick.current() }
}

function Import(): React.ReactElement {
  const registry = useLoad(() => m.loadImportRegistry(), [])
  const [preview, setPreview] = React.useState<m.ImportPreview | null>(null)
  const [summary, setSummary] = React.useState<m.ImportSummary | null>(null)
  const [scan, setScan] = React.useState<m.DedupScan | null>(null)
  const [busy, setBusy] = React.useState('')
  const [actionError, setActionError] = React.useState('')
  /**
   * 本次干跑/导入针对哪个路径（空串 = 全部来源）。
   *
   * 必须记住它：干跑单家之后点「开始导入」，如果只传 `undefined` 就会把
   * **全部来源**都写进去 —— 用户以为在确认「这一家」，实际导了十二家。
   */
  const [target, setTarget] = React.useState('')
  /**
   * 这份干跑结果是什么时候扫的（0 = 刚在本地扫的）。
   *
   * 从后端接回来的清单必须标出处：扫描是一次快照，文件随时会变。用户切回来
   * 看到十几分钟前的清单，至少要能判断「要不要再扫一遍」。
   */
  const [restoredAt, setRestoredAt] = React.useState(0)
  /** 「添加来源」表单是否展开。低频动作，默认折叠着。 */
  const [addOpen, setAddOpen] = React.useState(false)
  /**
   * 本地发起了几次干跑。用来让「挂载时恢复缓存」让位给用户自己点的那一次 ——
   * 否则两者会互相覆盖，界面上看到的是哪一次全凭网络快慢。
   */
  const scanSeq = React.useRef(0)

  const { job, refresh } = useImportJob({
    onDone: (result, fresh) => {
      // 结果一律显示（切回来也要看得见「上次导了多少」）；
      // 清册只刷一次（刷清册很贵：要开 sqlite、递归数文件）。
      setSummary(result)
      if (fresh) {
        setPreview(null)
        registry.reload()
      }
    },
    // 失败提示**故意不看 `fresh`**：上一次导入失败了、之后什么也没发生，
    // 那切回来还应该看见这条错。丢掉的报错比重复的报错更贵 —— 用户会以为写进去了。
    onFail: message => setActionError(message),
  })
  const running = job?.running ?? false

  /**
   * 把上一次干跑的结果接回来。
   *
   * ── 为什么要做这件事 ────────────────────────────────────────
   * 页签是条件渲染，切走就把这个组件连同 `preview` 一起卸载。回来只剩一张空表，
   * 用户只能再点一次「导入全部来源」重扫全盘 —— 可干跑结果本来就有 30 分钟
   * 有效期（真导入的门槛就按这个 TTL 放行），在有效期内它依然是这份预览。
   *
   * ── 为什么要处理 `running` ──────────────────────────────────
   * 切走时那次扫描可能还在跑（POST 请求不会因为组件卸载而中断）。回来立刻再扫
   * 一遍等于全盘读两次；查到「正在扫」就原地等，扫完直接取结果。
   */
  React.useEffect(() => {
    let alive = true
    let timer = 0
    let tries = 0
    // 用户可能在这个 effect 落地前就自己点了「导入全部来源」—— 以本地那次为准。
    const seq = scanSeq.current

    const load = async () => {
      try {
        const snapshot = await m.lastPreview()
        if (!alive || scanSeq.current !== seq) return
        if (snapshot.running && tries < 60) {
          tries += 1
          timer = window.setTimeout(() => void load(), 800)
          return
        }
        if (!snapshot.fresh || !snapshot.preview) return
        setPreview(current => current ?? snapshot.preview)
        if (snapshot.at) setRestoredAt(snapshot.at)
        // 目标来源必须一起恢复：只恢复清单不恢复目标，「开始导入」会把全部来源
        // 都写进去，而用户以为自己只确认了那一家。
        setTarget(current => current || (snapshot.only ?? ''))
      } catch {
        // 后端还没起来 / 暂时不可用：保持空白，用户点按钮就行
      }
    }
    void load()

    return () => {
      alive = false
      if (timer) window.clearTimeout(timer)
    }
  }, [])

  const act = async (key: string, task: () => Promise<unknown>) => {
    setBusy(key)
    setActionError('')
    try {
      return await task()
    } catch (caught) {
      setActionError(caught instanceof Error ? caught.message : String(caught))
      return undefined
    } finally {
      setBusy('')
    }
  }

  /** 干跑。`path` 为空 = 全部来源；否则只预览这一条（后端按路径规范化后匹配）。 */
  const dryRun = async (path: string) => {
    scanSeq.current += 1
    setTarget(path)
    setRestoredAt(0)
    await act('preview', async () => {
      setSummary(null)
      try {
        setPreview(await m.previewImport(path || undefined, 80))
      } catch (caught) {
        // 桌面壳 60 秒超时 / 连接被 reset 时，后端可能已经把这次扫完了 ——
        // 先看缓存，别让用户白等一轮再手动点一次。缓存也没有才把错抛给用户。
        const snapshot = await m.lastPreview().catch(() => null)
        if (snapshot?.fresh && snapshot.preview) {
          setPreview(snapshot.preview)
          if (snapshot.at) setRestoredAt(snapshot.at)
          return
        }
        throw caught
      }
    })
  }

  /**
   * 真导入。**只发一个「开始」**，不等写完。
   *
   * 后面的事都交给 `useImportJob` 轮询：写了几千条要几分钟，期间用户会切页签，
   * 组件会被卸载 —— 结果只能从后端取，不能挂在这条请求上。
   */
  const commit = async () => {
    await act('import', async () => {
      await m.startImport(target || undefined)
      setPreview(null)
      setRestoredAt(0)
      // 起点就催一次：轮询在 idle 时是停着的，不催就一直不动。
      refresh()
    })
  }

  const rows = registry.data?.sources ?? []
  const found = registry.data?.found ?? 0
  const total = registry.data?.total ?? 0
  const importable = rows.filter(row => row.exists && row.enabled)

  /** 启停/改路径都返回新清册，直接灌进 useLoad 的缓存省一次读盘探测。 */
  const apply = (task: Promise<m.ImportRegistry>) => task.then(() => registry.reload())

  return (
    <div className='mm-import'>
      {actionError ? <div className='mm-error'>{actionError}</div> : null}

      {/* ── 后台导入进行中 ──────────────────────────────────────
          这一条是「切页签不丢」的可视证据：它读的是**后端状态**，不是本地
          的一次性 state，所以切走再回来还在，进度也接着走。 */}
      {running ? (
        <div className='mm-import-progress'>
          <span className='mm-import-pulse' aria-hidden />
          <span>
            {job && job.total > 0
              ? t('导入进行中：{done} / {total} 条（切到别的页签也不会中断，回来再看这里）', {
                done: num(job.processed),
                total: num(job.total),
              })
              : t('导入进行中（正在扫描来源，切到别的页签也不会中断）')}
          </span>
        </div>
      ) : null}

      {/* ── 顶栏：说明 + 两个「整批」动作 ────────────────────────
          与卡片上的「单家」动作分开摆，否则用户分不清
          「导入该来源」和「导入全部来源」哪个是哪个。 */}
      <div className='mm-import-head'>
        <p className='mm-import-sub'>
          {t('把各 Agent 的历史会话导入为记忆；先预览（不写入）看清楚变化，确认后才真正写入')}
        </p>
        <div className='mm-actions'>
          <Button
            type='button'
            size='sm'
            variant='outline'
            disabled={busy !== '' || running}
            onClick={() => void act('scan', async () => {
              setScan(await m.dedupScan())
            })}
          >
            {busy === 'scan' ? t('巡检中…') : t('全库去重巡检')}
          </Button>
          <Button
            type='button'
            size='sm'
            disabled={busy !== '' || running || importable.length === 0}
            onClick={() => void dryRun('')}
          >
            {busy === 'preview' && !target ? t('预览中…') : t('导入全部来源')}
          </Button>
        </div>
      </div>

      {/* 巡检结果：只产建议不合并，所以要明确说「去哪儿裁决」 */}
      {scan ? (
        <div className='mm-scan-note'>
          {scan.skipped
            ? scan.reason
            : scan.queued > 0
              ? t('巡检扫了 {scanned} 条：精确重复 {exact}、近似 {near}，已把 {queued} 条建议放进待确认队列（去「浏览」页裁决）。', {
                scanned: num(scan.scanned),
                exact: num(scan.exact),
                near: num(scan.near),
                queued: num(scan.queued),
              })
              : t('巡检扫了 {scanned} 条：没有发现重复。', { scanned: num(scan.scanned) })}
        </div>
      ) : null}

      {/* ── 来源清册 ────────────────────────────────────────────
          「路径写在本机配置」这句灰色说明按截图摆在标题行右侧，和「添加来源」
          并排：它是这页的**前提条件**（用户会以为路径跟着 WebDAV 走），
          放在标题下方会变成又一行正文，放右侧才像注脚。 */}
      <Section
        title={t('来源探测（{found}/{total} 个已找到）', { found, total })}
        action={(
          <div className='mm-inline-actions'>
            <span className='mm-muted'>{t('路径写在本机配置，不随 WebDAV 同步')}</span>
            <Button type='button' size='sm' variant='ghost' onClick={() => setAddOpen(value => !value)}>
              {addOpen ? t('收起') : t('添加来源')}
            </Button>
          </div>
        )}
      >
        {registry.loading ? <Panel loading error=''>{null}</Panel> : null}
        {registry.error ? <Panel loading={false} error={registry.error} onRetry={registry.reload}>{null}</Panel> : null}
        {!registry.loading && !registry.error ? (
          rows.length === 0 ? (
            <Empty>{t('没有可探测的来源。')}</Empty>
          ) : (
            <div className='mm-src-grid'>
              {rows.map(row => (
                <SourceCard
                  key={row.id}
                  row={row}
                  busy={busy !== '' || running}
                  onImport={() => void dryRun(row.path)}
                  onEnable={() => void act('toggle', () => apply(m.enableImportSource(row.id)))}
                  onDisable={() => void act('toggle', () => apply(m.disableImportSource(row.id)))}
                  onEditPath={path => void act('path', () => apply(m.setImportSourcePath(row.id, path)))}
                  onReset={() => void act('path', () => apply(m.setImportSourcePath(row.id, '')))}
                  onRemove={() => void act('path', () => apply(m.setImportSourcePath(row.id, '')))}
                />
              ))}
            </div>
          )
        ) : null}
      </Section>

      {/* ── 添加自定义来源 ────────────────────────────────────────
          内置 12 家覆盖不到的路径（一个笔记目录、某个自己写的导出目录）。
          折叠在标题行右侧的按钮里：它是低频动作，常驻一行会把来源网格往下推。
          添加完成后卡片**直接出现在上面的网格里**（后端返回整份清册），
          所以这里不需要另起一份「自定义来源列表」—— 两份列表会让用户
          以为自定义来源不参与「导入全部来源」。 */}
      {addOpen ? (
        <Section
          title={t('添加自定义来源')}
          hint={t('内置 12 家之外的历史数据，可以自己指一个目录或文件；添加后出现在上面的来源网格里。')}
        >
          <CustomSources onAdded={() => { setAddOpen(false); registry.reload() }} />
        </Section>
      ) : null}

      {/* ── 干跑结果 ──────────────────────────────────────────── */}
      {preview ? (
        <Section
          title={t('干跑结果')}
          hint={restoredAt
            ? t('这一屏只读文件，一个字都没写。这份结果是 {time} 扫的，30 分钟内有效（切页签回来还在）；要重扫就再点一次「导入全部来源」。', {
              time: stamp(restoredAt),
            })
            : t('这一屏只读文件，一个字都没写。确认之后再点「开始导入」。')}
          action={(
            <Button
              type='button'
              size='sm'
              disabled={busy !== '' || running || preview.ready === 0}
              onClick={() => void commit()}
            >
              {running ? t('导入中…') : t('开始导入 {n} 条', { n: preview.ready })}
            </Button>
          )}
        >
          <div className='mm-inline-meta'>
            <Badge variant='brand' shape='tag'>{t('会导入 {n}', { n: preview.ready })}</Badge>
            <Badge variant='outline' shape='tag'>{t('跳过 {n}', { n: preview.skippedCount })}</Badge>
            {preview.duplicates > 0 ? <Badge variant='warning' shape='tag'>{t('重复 {n}', { n: preview.duplicates })}</Badge> : null}
            <span className='mm-muted'>{t('扫描 {n} 个文件 · 每批 {size} 条', { n: preview.scanned, size: preview.batchSize })}</span>
          </div>
          {typeof preview.listed === 'number' && preview.listed < preview.ready ? (
            <p className='mm-hint'>
              {t('列表只显示前 {shown} 条，实际会导入 {total} 条。', { shown: preview.listed, total: preview.ready })}
            </p>
          ) : null}

          {preview.importable.length ? (
            <div className='mm-preview-table'>
              <div className='mm-preview-row mm-preview-head'>
                <span>{t('文件')}</span>
                <span>{t('项目')}</span>
                <span>{t('字数')}</span>
                <span>{t('开头')}</span>
              </div>
              {preview.importable.map(row => (
                <div key={row.path} className='mm-preview-row'>
                  <span className='mm-preview-title' title={row.path}>{row.title || row.path}</span>
                  <span className='mm-muted'>{row.project ?? t('全局')}</span>
                  <span className='mm-mono'>{row.chars}</span>
                  <span className='mm-preview-snippet'>{row.preview}</span>
                </div>
              ))}
            </div>
          ) : <Empty>{t('没有可导入的内容。检查路径对不对、里面有没有 .md / .jsonl。')}</Empty>}

          {preview.skipped.length ? (
            <div className='mm-skipped'>
              <div className='mm-section-title'>{t('被跳过的')}</div>
              <div className='mm-muted'>
                {t('跳过不是错误：太短的是噪声，重复的不必再写，含密钥的不该进记忆库。')}
              </div>
              {preview.skipped.map((row, index) => (
                <div key={`${row.path}#${index}`} className='mm-skipped-row'>
                  <span className='mm-preview-title' title={row.path}>{row.title || row.path}</span>
                  <span className='mm-warn'>{row.reason}</span>
                </div>
              ))}
            </div>
          ) : null}
        </Section>
      ) : null}

      {/* ── 导入完成 ──────────────────────────────────────────── */}
      {summary ? (
        <Section title={t('导入完成')}>
          <div className='mm-inline-meta'>
            <Badge variant='success' shape='tag'>{t('写入 {n}', { n: summary.written })}</Badge>
            {summary.duplicated ? <Badge variant='outline' shape='tag'>{t('重复 {n}', { n: summary.duplicated })}</Badge> : null}
            {summary.skipped ? <Badge variant='outline' shape='tag'>{t('跳过 {n}', { n: summary.skipped })}</Badge> : null}
            {summary.failed ? <Badge variant='destructive' shape='tag'>{t('失败 {n}', { n: summary.failed })}</Badge> : null}
            <span className='mm-muted'>{t('扫描 {n} 条内容', { n: summary.scanned })}</span>
          </div>
          {summary.reasons.length ? (
            <div className='mm-skipped'>
              <div className='mm-section-title'>{t('失败明细')}</div>
              {summary.reasons.map((reason, index) => (
                <div key={index} className='mm-skipped-row'><span className='mm-warn'>{reason}</span></div>
              ))}
            </div>
          ) : null}
          <p className='mm-hint'>
            {t('导入的条目都在 l1 流水里，类别是「日常」。要让它们变成沉淀结论，去「自动化」页跑一次「蒸馏结论」。')}
          </p>
        </Section>
      ) : null}

      <LastRun />
    </div>
  )
}

/**
 * 添加自定义来源的表单。
 *
 * ── 为什么单独一个接口而不是整体替换 `import.sources` ──────
 * 整体替换要求页面知道**全部**行，而内置来源启用后也住在同一份清单里 ——
 * 页面某次少算一行（筛掉了一个隐藏行、某个来源刚好在探测中）就会把它静默
 * 删掉。用「加一条」表达「加一条」，误删在语义上就不可能发生。
 */
function CustomSources({ onAdded }: { onAdded: () => void }): React.ReactElement {
  const [path, setPath] = React.useState('')
  const [format, setFormat] = React.useState('')
  const [priority, setPriority] = React.useState('0')
  const [busy, setBusy] = React.useState(false)
  const [error, setError] = React.useState('')

  const save = async () => {
    setError('')
    setBusy(true)
    try {
      await m.addImportSource(path.trim(), format, Number(priority) || 0)
      setPath('')
      onAdded()
    } catch (caught) {
      setError(caught instanceof Error ? caught.message : String(caught))
    } finally {
      setBusy(false)
    }
  }

  return (
    <div className='mm-add-source'>
      <Input
        value={path}
        spellCheck={false}
        placeholder={t('目录或文件路径，例如 D:/notes 或 ~/.claude/projects')}
        onChange={event => setPath(event.target.value)}
        onKeyDown={event => {
          if (event.key === 'Enter' && path.trim() && !busy) void save()
        }}
      />
      <select className='mm-select' value={format} onChange={event => setFormat(event.target.value)}>
        <option value=''>{t('自动识别')}</option>
        <option value='md'>{t('Markdown 笔记')}</option>
        <option value='jsonl'>{t('会话记录（jsonl）')}</option>
      </select>
      <Input
        className='mm-priority-input'
        type='number'
        value={priority}
        placeholder={t('优先级')}
        onChange={event => setPriority(event.target.value)}
      />
      <Button type='button' size='sm' disabled={!path.trim() || busy} onClick={() => void save()}>
        {busy ? t('添加中…') : t('添加')}
      </Button>
      {error ? <div className='mm-error mm-add-source-error'>{error}</div> : null}
    </div>
  )
}

/** 上次导入的完整结果。水位在卡片上，整次统计在这儿。 */
function LastRun(): React.ReactElement | null {
  const view = useLoad(() => m.loadImport(), [])
  const last = view.data?.lastRun ?? null
  if (!last) return null
  return (
    <Section title={t('上次导入')} hint={t('时间：{when}', { when: last.atText || timeText(last.at) })}>
      <div className='mm-inline-meta'>
        <Badge variant='brand' shape='tag'>{t('写入 {n}', { n: num(last.written) })}</Badge>
        <Badge variant='outline' shape='tag'>{t('重复 {n}', { n: last.duplicated })}</Badge>
        <Badge variant='outline' shape='tag'>{t('跳过 {n}', { n: last.skipped })}</Badge>
        {last.failed ? <Badge variant='destructive' shape='tag'>{t('失败 {n}', { n: last.failed })}</Badge> : null}
      </div>
    </Section>
  )
}

export { Import }