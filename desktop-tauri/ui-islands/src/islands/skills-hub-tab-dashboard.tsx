import * as React from 'react'
import { Badge, Button } from '@ui'
import { t } from '../i18n'
import * as s from './skills-hub-api'
import { Empty, Panel, SkillCardGrid, ago, errorText, num, scanTime, toast, useLoad } from './skills-hub-shared'

/**
 * 技能仓库 · 仪表盘。
 *
 * ── 这一页要回答的五个问题 ──────────────────────────────────
 *   1. 收敛到什么程度？（真实目录份数 / 真身条数 —— 实际被合并的对数才是「份消除」）
 *   2. 各渠道通没通？目录在哪？
 *   3. 每个技能受管到哪一步？（下面那张速览网格，和技能库同一套状态判定）
 *   4. 上次收纳是什么时候、成功没有？
 *   5. 有什么要人处理的？（待裁决 / 孤儿 / 中央未登记 —— 这个数字点进去必须
 *      真的能逐条办掉；外链只说明、失效挂载在「挂载与去重」卡上）
 *
 * ── 为什么速览网格直接嵌在仪表盘里 ──────────────────────────
 * 「有 105 个技能」是个抽象数字，用户下一步一定是「让我看看是哪些」。把网格嵌
 * 进来以后，首屏就能从数字走到具体条目，不用先去技能库再回来。网格与技能库
 * 共用 `SkillCardGrid`，所以两处对「已挂载 / 待收纳 / 有硬伤」的判定不可能打架。
 *
 * ── 为什么收纳记录要逐份读报告正文 ──────────────────────────
 * 文件名只带时间，判断不了这一轮成功没有。`overview` 里给的是「最近几份报告」
 * 的元信息，状态只能从正文首行的统计行解析（`render_report` 的固定格式）：
 * 冲突数为 0 且没有「失败」字样才算完成。只读最近三条 —— 每条都是一次小文件
 * 读取，读满一页没有收益。
 *
 * ── 为什么渠道是卡片而不是矩阵 ──────────────────────────────
 * 渠道数量是个位数，卡片能同时放下「名字 / 目录 / 技能数 / 挂载数 / 通没通」，
 * 而且每个卡片右下角就是它自己的「配置」入口 —— 矩阵要横向读三列才知道哪个
 * 没通，卡片一眼就能。
 */

type Props = {
  onGo: (tab: 'library' | 'sync' | 'dedup' | 'settings') => void
  onOpenSkill: (name: string) => void
  onChanged: () => void
  /** 每次切入仪表盘 +1。保活后切 tab 不会卸载组件，靠它当重拉信号。 */
  enterKey: number
}

/** 一份收纳报告里读出来的结论。 */
type ReportMeta = {
  ok: boolean
  imported: number
  mounted: number
  merged: number
  skipped: number
  conflicts: number
}

/**
 * 解析收纳报告正文。
 *
 * 概要行是唯一的统计来源，但**它的标签改过名**，两种都在磁盘上：
 *   现在：`- 收进中央 3 · 挂载变更 2 · 合并重复 1 · 跳过 0 · 冲突 0 · 清理回收站 0`
 *   旧版：`新增 1 · 合并重复 1 · 挂载变更 2 · 冲突 0 · 跳过 208 · 修复 0 · 清理 2`
 * （2026-11-17 那份报告就是旧格式 —— 只认「收进中央」会让最近三条全读成 0。）
 *
 * 只取第一条「带冲突 + 带导入数」的行：正文下面的分节标题里也有这些词，
 * 按行号取第一条才是概要。缺哪个标签就当 0，不抛错。
 */
function parseReport(body: string): ReportMeta {
  const line = body
    .split('\n')
    .find(row => row.includes('冲突') && (row.includes('收进中央') || row.includes('新增'))) ?? ''
  const pick = (...labels: string[]): number => {
    for (const label of labels) {
      const hit = new RegExp(`${label}\\s*(\\d+)`).exec(line)
      if (hit) return Number(hit[1])
    }
    return 0
  }
  const conflicts = pick('冲突')
  return {
    ok: conflicts === 0 && !body.includes('失败'),
    imported: pick('收进中央', '新增'),
    mounted: pick('挂载变更'),
    merged: pick('合并重复'),
    skipped: pick('跳过'),
    conflicts,
  }
}

/**
 * 报告文件名里的时间戳 → `11-17 13:19`。
 *
 * 磁盘上的名字长这样：`sync-2026-11-17-131920.md` —— 日期与时分之间是 `-`，
 * 不是空格也不是 `T`。早先只留了 `[ T]?` 一种分隔符，结果一条都匹配不上，
 * 整个列表退回「N 天前」，看不清是哪一轮。
 */
function reportStamp(file: string): string {
  const hit = /(\d{4})-(\d{2})-(\d{2})[ T-]?(\d{2})(\d{2})(\d{2})?/.exec(file)
  if (!hit) return ''
  return `${hit[2]}-${hit[3]} ${hit[4]}:${hit[5]}`
}

/**
 * 一条待办：裁决 / 孤儿 / 中央未登记 —— 只有这三类（= AgentHub 的 `x()`）。
 *
 * 统一成「图标 + 标题 + 一句说明 + 一个动作」。`action` 决定那颗按钮做什么 ——
 * AgentHub 就是这么分工的：冲突给「裁决」跳去重页，孤儿给「查看」跳收纳页，
 * 中央未登记给「纳管」直接改台账。把它摊平成一个无脑的「查看」会让用户多点
 * 两层才办得成事，也让仪表盘失去「把不用动脑的事办完」的站位。
 *
 * 外链与失效挂载**不是**这个类型：前者在下方「其他中枢在管的登记」小组里，
 * 后者在「挂载与去重」卡上（一键修复）。它们进不来，是因为它们不在这个数字里。
 */
type TodoRow = {
  key: string
  title: string
  sub: string
  skill: string
  action: 'dedup' | 'sync' | 'adopt' | 'open'
}

function Dashboard({ onGo, onOpenSkill, onChanged, enterKey }: Props): React.ReactElement {
  // 仪表盘是全套里唯一「一眼看全现在」的一页，切回来还显示几分钟前的数字等于
  // 这一页白设 —— 显式开启「进页面重拉」。`enterKey` 补上「Skills Hub 内部切
  // tab」这个信号：`refreshOnEnter` 只认整页重进 / 手动刷新，保活后切 tab
  // 不卸载组件，`enterKey` 变了才把 deps 里的 effect 再跑一遍。
  const overview = useLoad(() => s.loadOverview(), [enterKey], { refreshOnEnter: true })
  const library = useLoad(() => s.loadLibrary(), [enterKey], { refreshOnEnter: true })
  const check = useLoad(() => s.loadDailyCheck(), [enterKey], { refreshOnEnter: true })
  const [checking, setChecking] = React.useState(false)
  const [query, setQuery] = React.useState('')
  /** 渠道卡片点出来的筛选；空串 = 全部。同一时刻只选一个渠道（AgentHub 同款）。 */
  const [toolFilter, setToolFilter] = React.useState('')
  /** 待办区锚点：三类全是「中央未登记」时，入口只能滚到本页那几条行内纳管。 */
  const todoRef = React.useRef<HTMLElement | null>(null)
  const [reports, setReports] = React.useState<Record<string, ReportMeta>>({})

  const data = overview.data
  const last = check.data?.last ?? null
  const recent = React.useMemo(() => (data?.recentReports ?? []).slice(0, 3), [data])
  const recentKey = recent.map(row => row.file).join('|')

  /**
   * 只为「还没读过」的报告发请求。
   *
   * `reports` 不进依赖表（只在回调里读），改用 `recentKey` 触发 —— 否则每次
   * 写入状态都会重跑这个 effect，而它自己又写状态，成了死循环。
   */
  React.useEffect(() => {
    if (!recentKey) return
    let alive = true
    const files = recentKey.split('|')
    void Promise.all(
      files.map(async file => {
        try {
          const body = (await s.loadReport(file)).body
          return [file, parseReport(body)] as const
        } catch {
          return [file, null] as const
        }
      }),
    ).then(pairs => {
      if (!alive) return
      setReports(prev => {
        const next = { ...prev }
        let changed = false
        for (const [file, meta] of pairs) {
          if (meta && !next[file]) { next[file] = meta; changed = true }
        }
        return changed ? next : prev
      })
    })
    return () => { alive = false }
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [recentKey])

  const refreshNow = (): void => {
    setReports({})
    overview.reload()
    library.reload()
    check.reload()
    onChanged()
  }

  const runCheck = async (autoMount: boolean) => {
    setChecking(true)
    try {
      const result = await s.runDailyCheck(autoMount)
      toast(result.summary, result.ok ? 'ok' : 'err')
      check.reload()
      overview.reload()
      onChanged()
    } catch (error) {
      toast(t('检测失败：{error}', { error: String(error instanceof Error ? error.message : error) }), 'err')
    } finally {
      setChecking(false)
    }
  }

  // ─── 机械类事项的处理动作（参考 AgentHub：动作挂在仪表盘，不占裁决页）──
  //
  // AgentHub 的分工：失效挂载那张卡上挂「一键修复」，「中央未登记」行挂
  // 「纳管」，「陌生目录（孤儿）」行只给「查看」跳收纳页，需要人对着两份内容
  // 做判断的冲突才进去重页逐条裁决。这里照同一套分工。
  //
  // 特意不再保留跨类的「一键处理」总按钮：它会让用户以为冲突也能一键办掉，
  // 而冲突永远需要人做选择。一个区块一个入口，才是 AgentHub 的写法。
  const [busy, setBusy] = React.useState('')
  const [note, setNote] = React.useState('')

  const unregister = async (name: string, tool: string) => {
    setBusy(`unregister:${tool}:${name}`)
    try {
      await s.unregisterMount(name, tool)
      toast(t('已解除 {name} 在 {tool} 的挂载登记', { name, tool }), 'ok')
      refreshNow()
    } catch (error) {
      toast(t('解除失败：{error}', { error: errorText(error) }), 'err')
    } finally {
      setBusy('')
    }
  }

  // `adopt` 只对「中央已有真实目录、但台账里没登记」的条目有效（后端会拒绝链接与
  // 不存在的目录），正好就是 `hubExtra` 里非链接的那一类。
  const adopt = async (name: string) => {
    setBusy(`adopt:${name}`)
    try {
      await s.adoptSkill(name)
      toast(t('已纳入台账：{name}', { name }), 'ok')
      refreshNow()
    } catch (error) {
      toast(t('纳入失败：{error}', { error: errorText(error) }), 'err')
    } finally {
      setBusy('')
    }
  }

  // 「挂载与去重」卡片上的「一键修复」：只重建失效的 Junction，不动内容。
  // AgentHub 把入口放在这张卡上，本机跟着放 —— 失效挂载属于「机械活」，
  // 不该让用户先进去重页再找按钮。
  const repairMounts = async () => {
    setBusy('repair')
    try {
      const result = await s.repairMounts()
      const failed = result.failed ?? []
      setNote(failed.length > 0
        ? [t('重建失败 {n} 处：', { n: num(failed.length) }), ...failed.map(item => `${item.skill} @ ${item.tool}：${item.message}`)].join('\n')
        : '')
      toast(
        failed.length > 0
          ? t('已重建 {n} 处，{k} 处失败', { n: num(result.repaired), k: num(failed.length) })
          : t('已重建 {n} 处失效挂载', { n: num(result.repaired) }),
        failed.length > 0 ? 'err' : 'ok',
      )
      refreshNow()
    } catch (error) {
      toast(t('修复失败：{error}', { error: errorText(error) }), 'err')
    } finally {
      setBusy('')
    }
  }

  if (!data) {
    return (
      <Panel loading={overview.loading} error={overview.error} onRetry={overview.reload}>
        <Empty>{t('还没读到技能仓库的状态')}</Empty>
      </Panel>
    )
  }

  // 渠道区只画「目录真的找到了」的渠道 —— 未找到目录 / 已停用的完整配置在配置页
  // 已经有（含候选路径、启用开关），仪表盘重复一遍只会把已连接的挤到下面。
  // 同一份过滤结果既喂卡片、也喂上面「已连接渠道」那格，别再写第二遍。
  const liveTools = data.tools.filter(row => row.dir)

  // 失效挂载：`external`（别的中枢在管的健康链接）不算失效 —— 不排除的话，
  // 解除登记前仪表盘永远挂着「N 处挂载失效」，而处理页那边它根本不在失效
  // 列表里，两边对不上；§69 全部重建修完也压不掉这个数字，就是这个原因。
  const brokenList = data.mountHealth.filter(item => !item.valid && !item.external)
  const brokenMounts = brokenList.length
  // 另外两类先算好：外链只作「登记说明」展示，孤儿是待办。
  const externals = data.mountHealth.filter(item => item.external)
  const extras = data.hubExtra ?? []
  const orphans = data.orphans ?? []
  const conflicts = data.pendingConflicts

  /**
   * 「待处理优化」= 冲突 + 孤儿 + 中央未登记 —— 就是 AgentHub 的 `x()`。
   *
   * ── 为什么不能把外链 / 失效挂载也加进来（2026-11-28 第二次报同一个 bug）──
   * 上一版把五类合成一个数，于是用户看到「4 项：0 冲突 + 2 孤儿 + 0 未登记 +
   * 2 其他」，点「去裁决」进裁决页 —— 那里只有冲突，0 条，空的。数字没错，
   * 错在**这个数字承诺的事，入口那边办不了**：
   *   * 失效挂载的处理入口在「挂载与去重」那张卡上（`一键修复`），已经闭环；
   *   * 外部链接是「别的中枢在管」的**登记状态**，不是缺陷，处置动作只有
   *     「解除登记」这种可选项 —— AgentHub 的仪表盘压根不列它。
   * 三类之外的这两类留在本页展示（外链单独一小组、失效挂载在卡上），但不进
   * 这个数字。判据只有一条：**卡片上的数字，必须等于点进去能逐条办掉的事**。
   */
  const todoTotal = conflicts.length + orphans.length + extras.length

  // 「份消除」= 本轮扫描里**真的被 L1 合并掉**的冗余副本对数。
  //
  // 不要用 `sourceCount − visibleSkills`（2026-11-28 用户报的 bug）：
  // `sourceCount` 里没有 junction 了（后端已与 AgentHub 对齐），但
  // `visibleSkills` 是「扫描 ∪ 台账」的并集，两者相减还混进了「链接技能」
  // 这类本来就不参与收纳的条目 —— 107 个链路技能会算出「份消除 107」，
  // 和中央技能库一模一样，看着像有 107 份冗余其实一份都没有。
  //
  // AgentHub 的仪表盘卡片读的就是 `survey.dedup.duplicates.length`
  // （`l1Merged`），即真实合并对数。这里照做，数字才有意义。
  const uniqueSkills = data.visibleSkills
  const deduped = data.l1Merged

  const items = library.data?.items ?? []
  const needle = query.trim().toLowerCase()
  // 渠道筛选与关键词是「与」关系：点了某个渠道卡片，网格只留该渠道收进来或
  // 挂在该渠道里的技能 —— 和 AgentHub 的 `d.value` 过滤口径一致。
  const quick = items
    .filter(row => {
      if (toolFilter) {
        const hitSource = (row.sources ?? []).some(source => source.tool === toolFilter)
        const hitMount = (row.mounts ?? []).some(mount => mount.tool === toolFilter)
        if (!hitSource && !hitMount) return false
      }
      if (!needle) return true
      return (
        row.name.toLowerCase().includes(needle)
        || (row.skillName ?? '').toLowerCase().includes(needle)
        || (row.description ?? '').toLowerCase().includes(needle)
      )
    })
    .slice(0, 18)

  // 三类问题**轮流**取，先各取第一条再取第二条。
  //
  // 不能先拼成一个大数组再 `slice(0, 3)`：未登记排在最后，只要前面两类里有
  // 一条，它就被整类切掉 —— 而用户遇到的情况正是「右上角写 4 项、列表一条都
  // 看不见」。轮流取保证每一类都有机会露头。
  //
  // 只放「待处理优化」计数的三类（= AgentHub 的 `x()`）。外链不在这里 ——
  // 它不是缺陷而是登记状态，混进来会让「待办 N 项」点进去永远是空的。
  const buckets: TodoRow[][] = [
    conflicts.map(item => ({
      key: `c-${item.id}`,
      title: t('待裁决 · {title}', { title: item.title }),
      sub: item.detail,
      skill: item.skill ?? '',
      action: 'dedup',
    })),
    orphans.map(item => ({
      key: `o-${item.tool}-${item.name}`,
      title: t('孤儿目录 · {name}', { name: item.name }),
      sub: t('存在于 {tool}，待收纳', { tool: item.tool }),
      skill: item.name,
      action: 'sync',
    })),
    extras.map(item => ({
      key: `h-${item.name}`,
      title: t('中央未登记 · {name}', { name: item.name }),
      sub: item.isLink ? t('悬空链接，指向的真身已不在') : t('中央有目录，台账里没有记录'),
      skill: item.name,
      action: item.isLink ? 'open' : 'adopt',
    })),
  ]
  const todo: TodoRow[] = []
  for (let round = 0; round < 3 && todo.length < 6; round += 1) {
    for (const bucket of buckets) {
      if (todo.length >= 6) break
      const row = bucket[round]
      if (row) todo.push(row)
    }
  }

  /**
   * 「这个数字点进去会到哪」—— 按组成分派，不写死跳裁决页。
   *
   * AgentHub 自己也栽在这儿：它的卡片脚注是一颗写死的「去裁决」，只要
   * `orphans > 0` 而 `conflicts === 0`，点下去就是一张空页（它的 `x()` 把
   * 三类算在一起，入口却只认冲突）。照抄这个写法等于把这个洞一起继承，
   * 所以这里按「先冲突、再孤儿、最后未登记」分派到**真能办掉它**的地方。
   */
  const nextStep = todoTotal === 0
    ? null
    : conflicts.length > 0
      ? { label: t('去裁决'), arrow: '→', run: () => onGo('dedup') }
      : orphans.length > 0
        ? { label: t('去收纳'), arrow: '→', run: () => onGo('sync') }
        : { label: t('逐条纳管'), arrow: '↓', run: () => todoRef.current?.scrollIntoView({ behavior: 'smooth', block: 'start' }) }

  // AgentHub 的胶囊文案是「· {时间} 已自动完成」/「· 尚未运行检测」，两态都
  // 只陈述事实、不做评价。本机保留「有 N 项待处理」这一句，因为巡检失败时
  // 用户第一件想知道的就是「差在哪」。
  const checkState = last
    ? last.ok
      ? t('· {time} 已自动完成', { time: scanTime(last.checkedAt) })
      : t('· {time} 完成，{n} 项待处理', {
        time: scanTime(last.checkedAt),
        n: num(last.brokenMounts + last.conflicts + last.orphans + last.hubExtra),
      })
    : t('· 尚未运行检测')

  return (
    <Panel loading={overview.loading} error={overview.error} onRetry={overview.reload}>
      <div className='sk-dash'>
        {/* ── 页头：标题 + 中央路径芯片 + 三个动作 ──
            AgentHub 把中央仓库路径做成标题右侧的胶囊，而不是单独一行副标题 ——
            一行头只放「这是什么 + 在哪」，视线不用上下跳。这里照排。 */}
        <div className='sk-dash-head'>
          <div className='sk-dash-head-copy'>
            <div className='sk-dash-title-row'>
              <span className='sk-dash-title'>{t('技能仓库 · 仪表盘')}</span>
              <span className='sk-dash-pill' title={data.hubDir}>
                <svg viewBox='0 0 24 24' fill='none' stroke='currentColor' strokeWidth='1.8' strokeLinecap='round' strokeLinejoin='round' aria-hidden='true'>
                  <rect x='3' y='4' width='18' height='7' rx='2' />
                  <rect x='3' y='13' width='18' height='7' rx='2' />
                  <path d='M7 7.5h.01M7 16.5h.01' />
                </svg>
                <code>{data.hubDir}</code>
              </span>
            </div>
          </div>
          <div className='sk-dash-head-actions'>
            <Button type='button' size='sm' variant='outline' onClick={() => onGo('sync')}>
              {t('去收纳中心')}
            </Button>
            <Button type='button' size='sm' variant='ghost' onClick={() => onGo('settings')}>
              {t('配置渠道')}
            </Button>
            <Button type='button' size='sm' variant='default' onClick={refreshNow}>
              {t('刷新')}
            </Button>
          </div>
        </div>

        {/* ── 每日检测条 ── */}
        <div className='sk-check-bar'>
          <div className='sk-check-main'>
            <span className={`sk-check-glyph${last ? (last.ok ? ' is-ok' : ' is-warn') : ''}`} aria-hidden='true'>
              {last && !last.ok ? (
                <svg viewBox='0 0 24 24' fill='none' stroke='currentColor' strokeWidth='2' strokeLinecap='round' strokeLinejoin='round'>
                  <circle cx='12' cy='12' r='9' />
                  <path d='M12 7.5v5M12 16.2v.1' />
                </svg>
              ) : (
                <svg viewBox='0 0 24 24' fill='none' stroke='currentColor' strokeWidth='2' strokeLinecap='round' strokeLinejoin='round'>
                  <circle cx='12' cy='12' r='9' />
                  <path d='m8.4 12.2 2.4 2.4 4.8-5' />
                </svg>
              )}
            </span>
            <span className='sk-check-title'>{t('全自动每日检测')}</span>
            <span className='sk-check-state'>{checkState}</span>
          </div>
          <div className='sk-check-actions'>
            <Button type='button' size='sm' variant='outline' disabled={checking} onClick={() => void runCheck(false)}>
              {checking ? t('检测中…') : t('立即全盘检测')}
            </Button>
            <button type='button' className='sk-icon-btn' title={t('重新读取状态')} onClick={refreshNow} aria-label={t('重新读取状态')}>
              <svg viewBox='0 0 24 24' fill='none' stroke='currentColor' strokeWidth='2' strokeLinecap='round' strokeLinejoin='round' aria-hidden='true'>
                <path d='M21 12a9 9 0 1 1-3.2-6.9' />
                <path d='M21 3v6h-6' />
              </svg>
            </button>
          </div>
        </div>

        {/* ── 四宫格统计 ── */}
        <div className='sk-stats'>
          <div className='sk-stat'>
            <div className='sk-stat-top'>
              <span className='sk-stat-label'>{t('中央技能库')}</span>
              <span className='sk-stat-glyph' aria-hidden='true'>
                <svg viewBox='0 0 24 24' fill='none' stroke='currentColor' strokeWidth='1.8' strokeLinecap='round' strokeLinejoin='round'>
                  <path d='M4 5.5A2.5 2.5 0 0 1 6.5 3H20v15H6.5A2.5 2.5 0 0 0 4 20.5z' />
                  <path d='M8 7h8M8 11h6' />
                </svg>
              </span>
            </div>
            <div className='sk-stat-value'>
              <b>{num(uniqueSkills)}</b>
              <span>{t('个技能')}</span>
            </div>
            <div className='sk-stat-sub'>{t('来自 {n} 份原始副本', { n: num(data.sourceCount) })}</div>
            <div className='sk-stat-foot'>
              <button type='button' className='sk-stat-link' onClick={() => onGo('library')}>
                {t('去浏览')} <span aria-hidden='true'>→</span>
              </button>
            </div>
          </div>

          <div className='sk-stat'>
            <div className='sk-stat-top'>
              <span className='sk-stat-label'>{t('已连接渠道')}</span>
              <span className='sk-stat-glyph' aria-hidden='true'>
                <svg viewBox='0 0 24 24' fill='none' stroke='currentColor' strokeWidth='1.8' strokeLinecap='round' strokeLinejoin='round'>
                  <path d='M9 3v6M15 3v6' />
                  <path d='M6 9h12v4a6 6 0 0 1-12 0z' />
                  <path d='M12 19v3' />
                </svg>
              </span>
            </div>
            <div className='sk-stat-value'>
              <b>{num(liveTools.length)}</b>
              <span className='sk-stat-slash'>/</span>
              <b>{num(data.tools.length)}</b>
              <span>{t('个渠道')}</span>
            </div>
            {/* AgentHub 这张卡只有「N/M 个渠道」+「管理 →」，没有第二行统计。
                多加一行「共 N 个技能目录」会把用户引去数目录，而那一格真正要
                回答的是「接了几个客户端」。 */}
            <div className='sk-stat-foot'>
              <button type='button' className='sk-stat-link' onClick={() => onGo('settings')}>
                {t('管理')} <span aria-hidden='true'>→</span>
              </button>
            </div>
          </div>

          <div className='sk-stat'>
            <div className='sk-stat-top'>
              <span className='sk-stat-label'>{t('挂载与去重')}</span>
              <span className='sk-stat-glyph' aria-hidden='true'>
                <svg viewBox='0 0 24 24' fill='none' stroke='currentColor' strokeWidth='1.8' strokeLinecap='round' strokeLinejoin='round'>
                  <path d='M9 15 15 9' />
                  <path d='M11 5.5 12.6 4a3.7 3.7 0 0 1 5.2 5.2L16 11' />
                  <path d='M13 18.5 11.4 20a3.7 3.7 0 0 1-5.2-5.2L8 13' />
                </svg>
              </span>
            </div>
            <div className='sk-stat-value'>
              <b>{num(deduped)}</b>
              <span>{t('份消除')}</span>
            </div>
            <div className={`sk-stat-sub${brokenMounts > 0 ? ' is-warn' : ' is-ok'}`}>
              {brokenMounts > 0
                ? t('{n} 个失效挂载', { n: num(brokenMounts) })
                : t('✓ 健康')}
            </div>
            {brokenMounts > 0 ? (
              <div className='sk-stat-foot'>
                <button
                  type='button'
                  className='sk-stat-link'
                  disabled={busy === 'repair'}
                  onClick={() => void repairMounts()}
                >
                  {busy === 'repair' ? t('修复中…') : t('一键修复')}
                </button>
              </div>
            ) : null}
          </div>

          <div className='sk-stat'>
            <div className='sk-stat-top'>
              <span className='sk-stat-label'>{t('待处理优化')}</span>
              <span className='sk-stat-glyph' aria-hidden='true'>
                <svg viewBox='0 0 24 24' fill='none' stroke='currentColor' strokeWidth='1.8' strokeLinecap='round' strokeLinejoin='round'>
                  <path d='M12 3 2.5 20h19z' />
                  <path d='M12 9v5M12 17.2v.1' />
                </svg>
              </span>
            </div>
            <div className='sk-stat-value'>
              <b>{num(todoTotal)}</b>
              <span>{t('项')}</span>
            </div>
            <div className={`sk-stat-sub${todoTotal > 0 ? ' is-warn' : ' is-ok'}`}>
              {t('{c} 冲突 + {o} 孤儿 + {h} 未登记', {
                c: num(conflicts.length),
                o: num(orphans.length),
                h: num(extras.length),
              })}
            </div>
            <div className='sk-stat-foot'>
              {nextStep ? (
                <button type='button' className='sk-stat-link' onClick={nextStep.run}>
                  {nextStep.label} <span aria-hidden='true'>{nextStep.arrow}</span>
                </button>
              ) : (
                <span className='sk-stat-ok'>{t('✓ 已就绪')}</span>
              )}
            </div>
          </div>
        </div>

        {/* ── 渠道 ── */}
        <section className='sk-block'>
          <div className='sk-block-head'>
            <span className='sk-block-title'>
              <span className='sk-block-glyph' aria-hidden='true'>
                <svg viewBox='0 0 24 24' fill='none' stroke='currentColor' strokeWidth='2' strokeLinecap='round' strokeLinejoin='round'>
                  <rect x='3' y='4' width='18' height='6' rx='1.6' />
                  <rect x='3' y='14' width='18' height='6' rx='1.6' />
                  <path d='M7 7h.01M7 17h.01' />
                </svg>
              </span>
              {t('渠道工具连接矩阵')}
              <span className='sk-block-count' title={t('只显示已连接的渠道')}>{num(liveTools.length)}</span>
            </span>
            <div className='sk-block-action'>
              {toolFilter ? (
                <Button type='button' size='sm' variant='ghost' onClick={() => setToolFilter('')}>
                  {t('清除筛选（显示全部）')}
                </Button>
              ) : null}
              <Button type='button' size='sm' variant='ghost' onClick={() => onGo('settings')}>
                {t('+ 增减渠道')}
              </Button>
            </div>
          </div>
          {liveTools.length === 0 ? (
            <Empty>
              {data.tools.length === 0
                ? t('没有可用的渠道，先去配置页加一个目录')
                : t('还没有已连接的渠道，去配置页检查目录')}
            </Empty>
          ) : (
            <div className='sk-channels'>
              {liveTools.map(row => {
                const active = toolFilter === row.id
                return (
                  <button
                    key={row.id}
                    type='button'
                    className={`sk-channel${active ? ' is-active' : ''}`}
                    aria-pressed={active}
                    title={t('点击按 {name} 筛选下方技能列表', { name: row.name })}
                    onClick={() => setToolFilter(active ? '' : row.id)}
                  >
                    <span className='sk-channel-glyph is-ok' aria-hidden='true'>
                      {(row.name || '?').trim().charAt(0).toUpperCase()}
                    </span>
                    <span className='sk-channel-body'>
                      <span className='sk-channel-name'>
                        <span title={row.name}>{row.name}</span>
                        {row.builtin ? null : <Badge variant='outline' shape='tag'>{t('自定义')}</Badge>}
                      </span>
                    </span>
                    {/* AgentHub 这张卡右侧是「N 技能」+ 挂载状态标签，不再单列目录。
                        目录在配置页和 tooltip 里都有，占一行只会挤掉名字。 */}
                    <span className='sk-channel-meta'>
                      <span>{t('{n} 技能', { n: num(row.skillCount) })}</span>
                      {row.mountCount > 0 ? (
                        <Badge variant='success' shape='tag'>{t('已挂载 {n}', { n: num(row.mountCount) })}</Badge>
                      ) : (
                        <Badge variant='outline' shape='tag'>{t('未挂载')}</Badge>
                      )}
                    </span>
                  </button>
                )
              })}
            </div>
          )}
        </section>

        {/* ── 技能速览 ── */}
        <section className='sk-block'>
          <div className='sk-block-head'>
            <span className='sk-block-title'>
              <span className='sk-block-glyph' aria-hidden='true'>
                <svg viewBox='0 0 24 24' fill='none' stroke='currentColor' strokeWidth='2' strokeLinecap='round' strokeLinejoin='round'>
                  <rect x='3.5' y='3.5' width='7' height='7' rx='1.6' />
                  <rect x='13.5' y='3.5' width='7' height='7' rx='1.6' />
                  <rect x='3.5' y='13.5' width='7' height='7' rx='1.6' />
                  <rect x='13.5' y='13.5' width='7' height='7' rx='1.6' />
                </svg>
              </span>
              {t('技能快捷速览与控制')}
              <span className='sk-block-count'>{num(quick.length)}</span>
            </span>
            <div className='sk-block-action'>
              <label className='sk-search-wrap'>
                <svg className='sk-search-icon' viewBox='0 0 24 24' fill='none' stroke='currentColor' strokeWidth='2' strokeLinecap='round' strokeLinejoin='round' aria-hidden='true'>
                  <circle cx='11' cy='11' r='7' />
                  <path d='m20 20-3.2-3.2' />
                </svg>
                <input
                  className='sk-search'
                  type='search'
                  value={query}
                  placeholder={t('搜索技能名称或描述…')}
                  onChange={event => setQuery(event.target.value)}
                />
              </label>
              <Button type='button' size='sm' variant='outline' onClick={() => onGo('library')}>
                {t('全量技能库')}
              </Button>
            </div>
          </div>
          {quick.length === 0 ? (
            <Empty>
              {items.length === 0
                ? t('技能库里还没有任何技能。去配置页接上客户端的技能目录，然后收纳一次。')
                : t('没有找到符合条件的技能')}
            </Empty>
          ) : (
            <SkillCardGrid rows={quick} onOpen={onOpenSkill} footer='mounts' badge='ledger' className='sk-quick-grid' />
          )}
        </section>

        {/* ── 最近收纳 + 待办 ── */}
        <div className='sk-dash-cols'>
          <section className='sk-block'>
            <div className='sk-block-head'>
              <span className='sk-block-title'>{t('最近收纳记录')}</span>
              <div className='sk-block-action'>
                <Button type='button' size='sm' variant='ghost' onClick={() => onGo('sync')}>
                  {t('去收纳')}
                </Button>
              </div>
            </div>
            {data.recentReports.length === 0 ? (
              <Empty>{t('还没收纳过')}</Empty>
            ) : (
              <ul className='sk-sync-list'>
                {recent.map(row => {
                  const meta = reports[row.file]
                  return (
                    <li key={row.file}>
                      <span className='sk-sync-time'>{reportStamp(row.file) || ago(row.mtimeMs)}</span>
                      <span className='sk-sync-main'>
                        <span className='sk-sync-line'>
                          {meta ? (
                            <Badge variant={meta.ok ? 'success' : 'warning'} shape='tag'>
                              {meta.ok ? t('收纳完成') : t('有 {n} 项冲突', { n: num(meta.conflicts) })}
                            </Badge>
                          ) : (
                            <Badge variant='outline' shape='tag'>{t('读取中…')}</Badge>
                          )}
                          {meta ? (
                            <span className='sk-sync-note'>
                              {t('收进 {i} · 挂载 {m} · 合并 {g}', {
                                i: num(meta.imported),
                                m: num(meta.mounted),
                                g: num(meta.merged),
                              })}
                            </span>
                          ) : null}
                        </span>
                        <code className='sk-sync-file' title={row.file}>{row.file}</code>
                      </span>
                      <Button type='button' size='sm' variant='ghost' onClick={() => onGo('sync')}>
                        {t('报告')}
                      </Button>
                    </li>
                  )
                })}
              </ul>
            )}
          </section>

          <section className='sk-block' ref={todoRef}>
            <div className='sk-block-head'>
              <span className='sk-block-title'>{t('待办决策与孤儿')}</span>
              <div className='sk-block-action'>
                {nextStep ? (
                  <Button type='button' size='sm' variant='ghost' onClick={nextStep.run}>
                    {nextStep.label}
                  </Button>
                ) : null}
              </div>
            </div>
            {todo.length === 0 ? (
              // AgentHub 的空态是居中的大对勾 —— 「仓库是干净的」是个好消息，
              // 用一行灰字说掉太像报错。样式沿用本机既有的 `.sk-empty-state`。
              <div className='sk-empty-state'>
                <span className='sk-empty-state-glyph is-ok' aria-hidden='true'>
                  <svg viewBox='0 0 24 24' fill='none' stroke='currentColor' strokeWidth='1.6' strokeLinecap='round' strokeLinejoin='round'>
                    <circle cx='12' cy='12' r='9.2' />
                    <path d='m8.2 12.3 2.6 2.6 5-5.4' />
                  </svg>
                </span>
                <span className='sk-empty-state-title'>{t('暂无待办事项')}</span>
              </div>
            ) : (
              <>
                <ul className='sk-todo-list'>
                  {todo.map(row => (
                    <li key={row.key}>
                      <span className='sk-todo-glyph' aria-hidden='true'>
                        <svg viewBox='0 0 24 24' fill='none' stroke='currentColor' strokeWidth='2' strokeLinecap='round' strokeLinejoin='round'>
                          <circle cx='12' cy='12' r='8.5' />
                          <path d='M12 8v4.5M12 16v.1' />
                        </svg>
                      </span>
                      <span className='sk-todo-copy'>
                        <span className='sk-todo-title' title={row.title}>{row.title}</span>
                        <span className='sk-todo-sub'>{row.sub}</span>
                      </span>
                      {row.action === 'dedup' ? (
                        <Button type='button' size='sm' variant='ghost' onClick={() => onGo('dedup')}>
                          {t('裁决')}
                        </Button>
                      ) : row.action === 'sync' ? (
                        <Button type='button' size='sm' variant='ghost' onClick={() => onGo('sync')}>
                          {t('查看')}
                        </Button>
                      ) : row.action === 'adopt' ? (
                        <Button
                          type='button'
                          size='sm'
                          variant='ghost'
                          disabled={busy === `adopt:${row.skill}`}
                          onClick={() => void adopt(row.skill)}
                        >
                          {t('纳管')}
                        </Button>
                      ) : (
                        <Button
                          type='button'
                          size='sm'
                          variant='ghost'
                          disabled={!row.skill}
                          onClick={() => { if (row.skill) onOpenSkill(row.skill) }}
                        >
                          {t('看详情')}
                        </Button>
                      )}
                    </li>
                  ))}
                </ul>
                {todoTotal > todo.length ? (
                  <div className='sk-dedup-summary'>
                    <span>{t('还有 {n} 项待处理，去对应页面逐条看完。', { n: num(todoTotal - todo.length) })}</span>
                    {nextStep ? (
                      <Button type='button' size='sm' variant='ghost' onClick={nextStep.run}>
                        {nextStep.label}
                      </Button>
                    ) : null}
                  </div>
                ) : null}
                {note ? (
                  <div className='sk-conflict-detail' style={{ whiteSpace: 'pre-line' }}>
                    <b>{t('失败明细')}</b>
                    {note}
                  </div>
                ) : null}
              </>
            )}
            {/* ── 外链：只说明，不算待办 ──
                AgentHub 的仪表盘完全不列外链，本机留着是因为它是唯一能给「解除
                登记」的入口（外链指向别处时，那个链接不是缺陷）。但它**不进**
                上面那个数字：把它算成待办，用户点「去处理」永远落空 —— 这正是
                2026-11-28 第二次报同一个 bug 的直接原因。所以单独一小组，标题
                就写明「不计入待办」。 */}
            {externals.length > 0 ? (
              <div className='sk-todo-aside'>
                <div className='sk-todo-aside-head'>
                  <span className='sk-todo-aside-title'>{t('其他中枢在管的登记（{n}）', { n: num(externals.length) })}</span>
                  <span className='sk-todo-aside-hint'>{t('磁盘链接由别的软件维护，不计入待办')}</span>
                </div>
                <ul className='sk-todo-list'>
                  {externals.map(item => (
                    <li key={`x-${item.tool}-${item.skill}`}>
                      <span className='sk-todo-glyph' aria-hidden='true'>
                        <svg viewBox='0 0 24 24' fill='none' stroke='currentColor' strokeWidth='2' strokeLinecap='round' strokeLinejoin='round'>
                          <path d='M10 13a5 5 0 0 0 7.5.5l2-2a5 5 0 0 0-7-7l-1.2 1.2' />
                          <path d='M14 11a5 5 0 0 0-7.5-.5l-2 2a5 5 0 0 0 7 7l1.2-1.2' />
                        </svg>
                      </span>
                      <span className='sk-todo-copy'>
                        <span className='sk-todo-title' title={item.skill}>{t('外部链接登记 · {name}', { name: item.skill })}</span>
                        <span className='sk-todo-sub'>{t('{tool} 里由其他软件维护，磁盘链接不动', { tool: item.tool })}</span>
                      </span>
                      <Button
                        type='button'
                        size='sm'
                        variant='ghost'
                        disabled={busy === `unregister:${item.tool}:${item.skill}`}
                        onClick={() => void unregister(item.skill, item.tool)}
                      >
                        {t('解除登记')}
                      </Button>
                    </li>
                  ))}
                </ul>
              </div>
            ) : null}
          </section>
        </div>

      </div>
    </Panel>
  )
}

export { Dashboard }
