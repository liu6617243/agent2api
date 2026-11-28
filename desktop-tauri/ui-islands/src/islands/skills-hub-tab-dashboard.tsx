import * as React from 'react'
import { Badge, Button } from '@ui'
import { t } from '../i18n'
import * as s from './skills-hub-api'
import { Empty, Panel, Section, SkillCardGrid, ago, errorText, num, toast, useLoad } from './skills-hub-shared'

/**
 * 技能仓库 · 仪表盘。
 *
 * ── 这一页要回答的五个问题 ──────────────────────────────────
 *   1. 收敛到什么程度？（真实目录份数 / 真身条数 —— 实际被合并的对数才是「份消除」）
 *   2. 各渠道通没通？目录在哪？
 *   3. 每个技能受管到哪一步？（下面那张速览网格，和技能库同一套状态判定）
 *   4. 上次同步是什么时候、成功没有？
 *   5. 有什么要人处理的？（待裁决 / 孤儿 / 中央未登记 / 失效挂载）
 *
 * ── 为什么速览网格直接嵌在仪表盘里 ──────────────────────────
 * 「有 105 个技能」是个抽象数字，用户下一步一定是「让我看看是哪些」。把网格嵌
 * 进来以后，首屏就能从数字走到具体条目，不用先去技能库再回来。网格与技能库
 * 共用 `SkillCardGrid`，所以两处对「已挂载 / 待收纳 / 有硬伤」的判定不可能打架。
 *
 * ── 为什么同步记录要逐份读报告正文 ──────────────────────────
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

/** 一份同步报告里读出来的结论。 */
type ReportMeta = {
  ok: boolean
  imported: number
  mounted: number
  merged: number
  skipped: number
  conflicts: number
}

/**
 * 解析同步报告正文。
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

/** 一条待办：裁决 / 孤儿 / 中央未登记，统一成「标题 + 一句说明 + 查看」。 */
type TodoRow = { key: string; title: string; sub: string; skill: string }

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
  // AgentHub 的仪表盘就是这么分工的：失效挂载那张卡上直接挂「一键修复」，
  // 「中央未登记」每行挂「纳管」，「陌生目录（孤儿）」行只给「查看」跳同步页。
  // 需要人对着两份内容做判断的冲突，才进「去重与冲突」页逐条裁决。
  //
  // 所以这里把 `resolve-all` 的入口从裁决页搬回仪表盘：它本来就是跨区块的机械
  // 动作，摆在裁决页会让用户以为「裁决」能顺手把这些办了，也会在无可裁决冲突
  // 时把整个按钮藏掉（旧实现：`clean` 分支下压根不渲染 toolbar）。
  const [busy, setBusy] = React.useState('')
  const [note, setNote] = React.useState('')

  const resolveAll = async () => {
    setBusy('resolve-all')
    try {
      const result = await s.resolveAll()
      const failed = result.repair.failed ?? []
      const skipped = result.adopt.skipped ?? []
      const lines: string[] = []
      if (failed.length > 0) {
        lines.push(t('重建失败 {n} 处：', { n: num(failed.length) }))
        lines.push(...failed.map(item => `${item.skill} @ ${item.tool}：${item.message}`))
      }
      if (skipped.length > 0) {
        lines.push(t('纳入跳过 {n} 个：', { n: num(skipped.length) }))
        lines.push(...skipped.map(item => `${item.name}：${item.reason}`))
      }
      setNote(lines.join('\n'))
      toast(result.message, failed.length + skipped.length > 0 ? 'err' : 'ok')
      refreshNow()
    } catch (error) {
      toast(t('处理失败：{error}', { error: errorText(error) }), 'err')
    } finally {
      setBusy('')
    }
  }

  const askResolveAll = async () => {
    const ask = (window as unknown as { wbConfirm?: { ask?: (options: Record<string, unknown>) => Promise<boolean> } }).wbConfirm?.ask
    const parts: string[] = []
    if (externals.length > 0) {
      parts.push(t('解除 {n} 条外部链接登记（磁盘链接不动）', { n: num(externals.length) }))
    }
    if (brokenMounts > 0) {
      parts.push(t('重建 {n} 处失效挂载', { n: num(brokenMounts) }))
    }
    const adoptables = extras.filter(item => !item.isLink)
    if (adoptables.length > 0) {
      parts.push(t('纳管 {n} 个中央未登记技能', { n: num(adoptables.length) }))
    }
    const nothing = parts.length === 0
    const body = nothing
      ? t('当前没有可自动处理的事项。冲突需要你选一边，孤儿目录由同步确认执行收纳。')
      : [t('将自动执行：'), ...parts.map(part => `• ${part}`), '', t('冲突需要你选一边，孤儿目录由同步确认执行收纳。')].join('\n')
    const ok = await Promise.resolve(ask?.({
      title: t('一键处理'),
      text: body,
      okText: t('立即执行'),
    }) ?? true)
    if (!ok) return
    await resolveAll()
  }

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

  if (!data) {
    return (
      <Panel loading={overview.loading} error={overview.error} onRetry={overview.reload}>
        <Empty>{t('还没读到技能仓库的状态')}</Empty>
      </Panel>
    )
  }

  // 渠道区只画「目录真的找到了」的渠道 —— 未找到目录 / 已停用的完整配置在配置页
  // 已经有（含候选路径、启用开关），仪表盘重复一遍只会把已连接的挤到下面。
  // 卡片里的状态徽标因此恒为「已连接」，不再需要三态分支。
  // 同一份过滤结果既喂卡片、也喂上面「已连接渠道」那格，别再写第二遍。
  const liveTools = data.tools.filter(row => row.dir)
  const channelSkills = data.tools.reduce((sum, row) => sum + row.skillCount, 0)
  // 失效挂载是「待处理」的第四类来源。之前它只进了 `todoTotal`、没进副标题，
  // 于是出现「4 项」配「冲突 0 · 孤儿 0 · 未登记 0」—— 四个数各自都对，加起来
  // 却对不上总数，用户只能猜第 4 项是什么。现在四类同进同出，总数 = 副标题之和。
  // external（别的中枢在管的健康链接）不算失效 —— 不排除的话，解除登记前
  // 仪表盘永远挂着「N 处挂载失效」，而处理页那边它根本不在失效列表里，
  // 两边对不上；§69 全部重建修完也压不掉这个数字，就是这个原因。
  const brokenList = data.mountHealth.filter(item => !item.valid && !item.external)
  const brokenMounts = brokenList.length
  // 另外三类同样在这里先算好：仪表盘现在是它们的唯一处理入口（裁决页只留冲突），
  // 所以「有没有东西要处理」「一键处理会做几件事」都靠这三个数组。
  const externals = data.mountHealth.filter(item => item.external)
  const extras = data.hubExtra ?? []
  const orphans = data.orphans ?? []
  // 两个数必须分开：`listed` 是表格实际会画出的行数（决定显不显示空状态），
  // `autoFixable` 是「一键处理」真能办掉的件数（悬空链接后端会拒绝纳管，
  // 所以不算）。混用会出现「表里有 1 行、却显示空状态」或反之。
  const listed = externals.length + brokenMounts + extras.length
  const autoFixable = externals.length + brokenMounts + extras.filter(item => !item.isLink).length
  const todoTotal = data.pendingConflicts.length + data.orphans.length + data.hubExtra.length + brokenMounts
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
  const quick = items
    .filter(row => {
      if (!needle) return true
      return (
        row.name.toLowerCase().includes(needle)
        || (row.skillName ?? '').toLowerCase().includes(needle)
        || (row.description ?? '').toLowerCase().includes(needle)
      )
    })
    .slice(0, 12)

  // 四类问题**轮流**取，先各取第一条再取第二条。
  //
  // 不能像原来那样先拼成一个大数组再 `slice(0, 3)`：挂载失效排在最后，只要前三类
  // 里有一条，它就被整类切掉。而用户遇到的情况正是「右上角写 4 项、副标题三个 0」——
  // 那 4 项全是挂载失效，列表里一条都看不见，点「去裁决」进去也是空的。
  const buckets: TodoRow[][] = [
    data.pendingConflicts.map(item => ({
      key: `c-${item.id}`,
      title: t('待裁决 · {title}', { title: item.title }),
      sub: item.detail,
      skill: item.skill ?? '',
    })),
    data.orphans.map(item => ({
      key: `o-${item.tool}-${item.name}`,
      title: t('孤儿目录 · {name}', { name: item.name }),
      sub: t('存在于 {tool}，待收纳', { tool: item.tool }),
      skill: item.name,
    })),
    data.hubExtra.map(item => ({
      key: `h-${item.name}`,
      title: t('中央未登记 · {name}', { name: item.name }),
      sub: item.isLink ? t('悬空链接，指向的真身已不在') : t('中央有目录，台账里没有记录'),
      skill: item.name,
    })),
    brokenList.map(item => ({
      key: `m-${item.tool}-${item.name}`,
      title: t('挂载失效 · {name}', { name: item.name }),
      sub: t('{tool} 里的链接指不到中央真身', { tool: item.tool }),
      skill: item.name,
    })),
  ]
  const todo: TodoRow[] = []
  for (let round = 0; round < 2 && todo.length < 3; round += 1) {
    for (const bucket of buckets) {
      if (todo.length >= 3) break
      const row = bucket[round]
      if (row) todo.push(row)
    }
  }

  const checkState = last
    ? last.ok
      ? t('{ago}检测通过', { ago: ago(last.checkedAt) })
      : t('{ago}检测有 {n} 项待处理', {
        ago: ago(last.checkedAt),
        n: last.brokenMounts + last.conflicts + last.orphans + last.hubExtra,
      })
    : t('尚未运行检测')

  return (
    <Panel loading={overview.loading} error={overview.error} onRetry={overview.reload}>
      <div className='sk-dash'>
        {/* ── 页头：标题 + 中央路径 + 三个动作 ── */}
        <div className='sk-dash-head'>
          <div className='sk-dash-head-copy'>
            <div className='sk-dash-title'>{t('技能仓库 · 仪表盘')}</div>
            <div className='sk-dash-sub' title={data.hubDir}>
              <span>{t('中央技能库：')}</span>
              <code>{data.hubDir}</code>
            </div>
          </div>
          <div className='sk-dash-head-actions'>
            <Button type='button' size='sm' variant='outline' onClick={() => onGo('sync')}>
              {t('去同步中心')}
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
              <svg viewBox='0 0 24 24' fill='none' stroke='currentColor' strokeWidth='2' strokeLinecap='round' strokeLinejoin='round'>
                <path d='M18 8A6 6 0 0 0 6 8c0 7-3 9-3 9h18s-3-2-3-9' />
                <path d='M13.7 21a2 2 0 0 1-3.4 0' />
              </svg>
            </span>
            <span className='sk-check-title'>{t('全自动每日检测')}</span>
            <span className='sk-check-state'>{checkState}</span>
          </div>
          <div className='sk-check-actions'>
            <Button type='button' size='sm' variant='outline' disabled={checking} onClick={() => void runCheck(false)}>
              {checking ? t('检测中…') : t('立即全面检测')}
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
            <div className='sk-stat-sub'>{t('来自 {n} 个来源副本', { n: num(data.sourceCount) })}</div>
            <div className='sk-stat-foot'>
              <button type='button' className='sk-stat-link' onClick={() => onGo('library')}>
                {t('去技能库')} <span aria-hidden='true'>→</span>
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
            <div className='sk-stat-sub'>{t('共 {n} 个技能目录', { n: num(channelSkills) })}</div>
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
                ? t('{n} 处挂载失效', { n: num(brokenMounts) })
                : t('挂载全部有效')}
            </div>
            <div className='sk-stat-foot'>
              <button type='button' className='sk-stat-link' onClick={() => onGo('dedup')}>
                {t('去冲突')} <span aria-hidden='true'>→</span>
              </button>
            </div>
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
              {t('冲突 {c} · 孤儿 {o} · 未登记 {h} · 挂载 {m}', {
                c: num(data.pendingConflicts.length),
                o: num(data.orphans.length),
                h: num(data.hubExtra.length),
                m: num(brokenMounts),
              })}
            </div>
            <div className='sk-stat-foot'>
              <button type='button' className='sk-stat-link' onClick={() => onGo('dedup')}>
                {t('去裁决')} <span aria-hidden='true'>→</span>
              </button>
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
              <Button type='button' size='sm' variant='ghost' onClick={() => onGo('settings')}>
                {t('+ 添加渠道')}
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
              {liveTools.map(row => (
                <div key={row.id} className='sk-channel'>
                  <span className='sk-channel-glyph is-ok' aria-hidden='true'>
                    {(row.name || '?').trim().charAt(0).toUpperCase()}
                  </span>
                  <div className='sk-channel-body'>
                    <div className='sk-channel-name'>
                      <span title={row.name}>{row.name}</span>
                      {row.builtin ? null : <Badge variant='outline' shape='tag'>{t('自定义')}</Badge>}
                      <Badge variant='success' shape='tag'>{t('已连接')}</Badge>
                    </div>
                    <code className='sk-channel-path' title={row.dir ?? ''}>
                      {row.dir}
                    </code>
                  </div>
                  <div className='sk-channel-meta'>
                    <span>{t('{n} 技能', { n: num(row.skillCount) })}</span>
                    <span>{t('{n} 挂载', { n: num(row.mountCount) })}</span>
                  </div>
                </div>
              ))}
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
              <span className='sk-block-count'>{num(items.length)}</span>
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
                {t('全部技能库')}
              </Button>
            </div>
          </div>
          {quick.length === 0 ? (
            <Empty>
              {items.length === 0
                ? t('技能库里还没有任何技能。去配置页接上客户端的技能目录，然后同步一次。')
                : t('没有符合搜索的技能')}
            </Empty>
          ) : (
            <SkillCardGrid rows={quick} onOpen={onOpenSkill} footer='mounts' className='sk-quick-grid' />
          )}
        </section>

        {/* ── 最近同步 + 待办 ── */}
        <div className='sk-dash-cols'>
          <section className='sk-block'>
            <div className='sk-block-head'>
              <span className='sk-block-title'>{t('最近同步记录')}</span>
              <div className='sk-block-action'>
                <Button type='button' size='sm' variant='ghost' onClick={() => onGo('sync')}>
                  {t('去同步')}
                </Button>
              </div>
            </div>
            {data.recentReports.length === 0 ? (
              <Empty>{t('还没同步过')}</Empty>
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
                              {meta.ok ? t('同步完成') : t('有 {n} 项冲突', { n: num(meta.conflicts) })}
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

          <section className='sk-block'>
            <div className='sk-block-head'>
              <span className='sk-block-title'>{t('待办决策与孤儿')}</span>
              <div className='sk-block-action'>
                <Button type='button' size='sm' variant='ghost' onClick={() => onGo('dedup')}>
                  {t('去裁决')}
                </Button>
              </div>
            </div>
            {todo.length === 0 ? (
              <Empty>{t('没有待处理项，仓库是干净的')}</Empty>
            ) : (
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
                    <Button
                      type='button'
                      size='sm'
                      variant='ghost'
                      disabled={!row.skill}
                      onClick={() => { if (row.skill) onOpenSkill(row.skill) }}
                    >
                      {t('查看')}
                    </Button>
                  </li>
                ))}
              </ul>
            )}
          </section>
        </div>

        {/* ── 需要处理（机械动作区）─────────────────────────────────
            对齐 AgentHub 的站位：仪表盘才是「把不用动脑的事办完」的地方。
            AgentHub 在「挂载与去重」那张卡上直接挂「一键修复」，未登记行挂
            「纳管」，陌生目录行跳同步 —— 裁决页只管冲突。

            这里把四类机械事项按「能不能自动做」分成两段：上半是自动段（外链
            登记 / 失效挂载 / 未登记），一个按钮全处理；下半是孤儿，只给「去同步」，
            因为收纳是同步的职责，造个按钮等于绕开同步直接改台账。 */}
        <section className='sk-block'>
          <div className='sk-block-head'>
            <span className='sk-block-title'>
              <span className='sk-block-glyph' aria-hidden='true'>
                <svg viewBox='0 0 24 24' fill='none' stroke='currentColor' strokeWidth='2' strokeLinecap='round' strokeLinejoin='round'>
                  <path d='M14.7 6.3a5 5 0 0 0 6.9 6.9l-8.3 8.3a2.5 2.5 0 0 1-3.5-3.5z' />
                  <path d='M3 21l3-3' />
                </svg>
              </span>
              {t('需要处理')}
              <span className='sk-block-count'>{num(listed)}</span>
            </span>
            <div className='sk-block-action'>
              <Button
                type='button'
                size='sm'
                variant='ghost'
                disabled={busy === 'resolve-all' || autoFixable === 0}
                onClick={() => void askResolveAll()}
              >
                {busy === 'resolve-all' ? t('处理中…') : t('一键处理')}
              </Button>
            </div>
          </div>

          {listed === 0 ? (
            <Empty>{t('挂载全部有效，中央目录也都登记过了')}</Empty>
          ) : (
            <div className='sk-table'>
              <div className='sk-table-head'>
                <span>{t('事项')}</span>
                <span>{t('技能')}</span>
                <span>{t('位置')}</span>
                <span>{t('操作')}</span>
              </div>

              {externals.map(item => (
                <div key={`x-${item.tool}-${item.skill}`} className='sk-table-row'>
                  <span className='sk-cell-note'>{t('外部链接登记（其他软件维护，磁盘链接不动）')}</span>
                  <span className='sk-cell-name'>{item.skill}</span>
                  <span className='sk-cell-note'><code>{item.path}</code></span>
                  <span className='sk-row-actions'>
                    <Button
                      type='button'
                      size='sm'
                      variant='ghost'
                      disabled={busy === `unregister:${item.tool}:${item.skill}`}
                      onClick={() => void unregister(item.skill, item.tool)}
                    >
                      {t('解除登记')}
                    </Button>
                  </span>
                </div>
              ))}

              {brokenList.map(item => (
                <div key={`m-${item.tool}-${item.skill}`} className='sk-table-row'>
                  <span className='sk-cell-note'>
                    {t('挂载失效（链接指不到中央真身）')}
                    {item.centralExists === false ? <span className='sk-muted'> · {t('中央真身也不在了')}</span> : null}
                  </span>
                  <span className='sk-cell-name'>{item.skill}</span>
                  <span className='sk-cell-note'><code>{item.path}</code></span>
                  <span className='sk-row-actions'>
                    <Button type='button' size='sm' variant='ghost' onClick={() => onOpenSkill(item.skill)}>
                      {t('看详情')}
                    </Button>
                  </span>
                </div>
              ))}

              {extras.map(item => (
                <div key={`h-${item.name}`} className='sk-table-row'>
                  <span className='sk-cell-note'>
                    {item.isLink ? t('中央未登记（悬空链接，指向的真身已不在）') : t('中央未登记（中央有目录，台账无记录）')}
                  </span>
                  <span className='sk-cell-name'>{item.name}</span>
                  <span className='sk-cell-note'><code>{item.path}</code></span>
                  <span className='sk-row-actions'>
                    {item.isLink ? (
                      <Button type='button' size='sm' variant='ghost' onClick={() => onOpenSkill(item.name)}>
                        {t('看详情')}
                      </Button>
                    ) : (
                      <Button
                        type='button'
                        size='sm'
                        variant='outline'
                        disabled={busy === `adopt:${item.name}`}
                        onClick={() => void adopt(item.name)}
                      >
                        {t('纳管')}
                      </Button>
                    )}
                  </span>
                </div>
              ))}
            </div>
          )}

          {note ? (
            <div className='sk-conflict-detail' style={{ whiteSpace: 'pre-line' }}>
              <b>{t('失败明细')}</b>
              {note}
            </div>
          ) : null}

          {orphans.length > 0 ? (
            <div className='sk-dedup-summary'>
              <span>
                {t('另有 {n} 个孤儿目录（渠道里有、中央还没有）。收纳由同步统一完成，去同步页确认执行即可。', {
                  n: num(orphans.length),
                })}
              </span>
              <Button type='button' size='sm' variant='ghost' onClick={() => onGo('sync')}>
                {t('去同步')}
              </Button>
            </div>
          ) : null}
        </section>
      </div>
    </Panel>
  )
}

export { Dashboard }
