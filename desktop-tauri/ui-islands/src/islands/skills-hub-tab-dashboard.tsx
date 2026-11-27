import * as React from 'react'
import { Badge, Button } from '@ui'
import { t } from '../i18n'
import * as s from './skills-hub-api'
import { Empty, Panel, SkillCardGrid, ago, num, toast, useLoad } from './skills-hub-shared'

/**
 * 技能仓库 · 仪表盘。
 *
 * ── 这一页要回答的五个问题 ──────────────────────────────────
 *   1. 收敛到什么程度？（来源条数 / 真身条数 —— 两者的差就是「份消除」）
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

function Dashboard({ onGo, onOpenSkill, onChanged }: Props): React.ReactElement {
  // 仪表盘是全套里唯一「一眼看全现在」的一页，切回来还显示几分钟前的数字等于
  // 这一页白设 —— 显式开启「进页面重拉」。
  const overview = useLoad(() => s.loadOverview(), [], { refreshOnEnter: true })
  const library = useLoad(() => s.loadLibrary(), [], { refreshOnEnter: true })
  const check = useLoad(() => s.loadDailyCheck(), [], { refreshOnEnter: true })
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
  const brokenMounts = data.mountHealth.filter(item => !item.valid).length
  const todoTotal = data.pendingConflicts.length + data.orphans.length + data.hubExtra.length + brokenMounts
  // 「份消除」= 累计来源条数 − 中央真身条数（`Overview::sourceCount` 的注释就是这么
  // 定义的）。不用 `l1Merged`：那是「本轮扫描」的数字，一天没扫就归零，和左边
  // 「105 个技能 / 来自 121 个来源副本」对不上，读起来像去重没起作用。
  const deduped = Math.max(0, data.sourceCount - data.skillCount)

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

  const todo: TodoRow[] = [
    ...data.pendingConflicts.slice(0, 2).map(item => ({
      key: `c-${item.id}`,
      title: t('待裁决 · {title}', { title: item.title }),
      sub: item.detail,
      skill: item.skill ?? '',
    })),
    ...data.orphans.slice(0, 2).map(item => ({
      key: `o-${item.tool}-${item.name}`,
      title: t('孤儿目录 · {name}', { name: item.name }),
      sub: t('存在于 {tool}，待收纳', { tool: item.tool }),
      skill: item.name,
    })),
    ...data.hubExtra.slice(0, 2).map(item => ({
      key: `h-${item.name}`,
      title: t('中央未登记 · {name}', { name: item.name }),
      sub: item.isLink ? t('悬空链接，指向的真身已不在') : t('中央有目录，台账里没有记录'),
      skill: item.name,
    })),
  ].slice(0, 3)

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
              <b>{num(data.skillCount)}</b>
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
              {t('冲突 {c} · 孤儿 {o} · 未登记 {h}', {
                c: num(data.pendingConflicts.length),
                o: num(data.orphans.length),
                h: num(data.hubExtra.length),
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
      </div>
    </Panel>
  )
}

export { Dashboard }
