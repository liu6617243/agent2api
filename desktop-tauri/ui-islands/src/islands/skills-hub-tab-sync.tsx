import * as React from 'react'
import { Badge, Button, MetricCard, MetricGrid, MetricLabel, MetricValue, Tooltip, TooltipArrow, TooltipContent, TooltipProvider, TooltipTrigger, confirmDialog } from '@ui'
import { t } from '../i18n'
import * as s from './skills-hub-api'
import { Empty, Panel, Section, ago, num, sizeText, toast, useLoad } from './skills-hub-shared'

/**
 * 技能仓库 · 同步。
 *
 * ── 这一页的形态是「预览 → 确认 → 结果」而不是一个「同步」按钮 ──
 * 同步会改用户的磁盘：把渠道目录里的副本收进中央、把目录原位换成指向中央的
 * 链接、把旧副本备份进回收站。一个按钮点下去就动几十个目录，用户连它要做什么
 * 都不知道 —— 这种「点一下看看」的代价太高。所以拆成两步：先给出**这一轮会做
 * 什么**的完整清单（一行不动），用户看清楚了再确认。
 *
 * ── 为什么执行后要显示 `manifestDiff` ───────────────────────
 * 三张统计卡说的是「几件事」，`diff` 说的是「哪些文件」。用户核对的是后者：
 * 「`+ mount taste @ ~/.codex/skills → skills/taste`」这种一行，能直接被拿去
 * 和自己的目录结构对一遍。统计卡只用来判断「要不要往下看」。
 *
 * ── 为什么报告要能翻历史 ────────────────────────────────────
 * 同步是**可回看**的操作：出错时第一个问题是「上一次同步到底动了什么」。
 * 每次同步都落一份 Markdown（`reports/sync-<ts>.md`），这一页给最近几条入口。
 */

type Props = { onChanged: () => void; onGo: (tab: 'dedup') => void }

/** 同步中心表格的一行（动作行或冲突行）。 */
type SyncRow = {
  key: string
  skill: string
  detail: string
  kind: string
  conflictId: string
}

function SyncTab({ onChanged, onGo }: Props): React.ReactElement {
  const preview = useLoad(() => s.loadPreview(), [], { refreshOnEnter: true })
  const reports = useLoad(() => s.loadReports(12), [])
  const [running, setRunning] = React.useState(false)
  const [result, setResult] = React.useState<s.SyncResult | null>(null)
  const [reportFile, setReportFile] = React.useState('')
  const [reportBody, setReportBody] = React.useState('')
  const [reportError, setReportError] = React.useState('')

  const run = async () => {
    // 执行前必须先说清影响面。AgentHub 用 confirm 列出「收进中央 N 个 ·
    // 挂载变更 M 处 · K 条冲突进入人工裁决」并声明「所有覆盖/删除先进入
    // 回收站，可还原」—— 同步会动几十个目录，用户不该在完全不知情的
    // 情况下点一下就交出磁盘。
    const planCounts = preview.data?.counts ?? {}
    const confirmed = await confirmDialog(
      t('确认执行同步吗？\n收进中央 {i} 个 · 挂载变更 {m} 处 · {c} 条冲突进入人工裁决。\n所有覆盖/删除先进入回收站，可还原。', {
        i: planCounts.import ?? 0,
        m: (planCounts.replace ?? 0) + (planCounts.publish ?? 0),
        c: planCounts.conflict ?? 0,
      }),
      { title: t('确认执行同步'), confirmText: t('确认执行') },
    )
    if (!confirmed) return
    setRunning(true)
    try {
      const outcome = await s.runSync()
      setResult(outcome)
      const summary = outcome.summary
      toast(
        t('收进中央 {imported} · 挂载 {mounted} · 合并 {merged} · 冲突 {conflicts}', {
          imported: summary.imported,
          mounted: summary.mounted,
          merged: summary.merged,
          conflicts: summary.conflicts,
        }),
        summary.conflicts > 0 ? 'err' : 'ok',
      )
      preview.reload()
      reports.reload()
      onChanged()
    } catch (error) {
      toast(t('同步失败：{error}', { error: String(error instanceof Error ? error.message : error) }), 'err')
    } finally {
      setRunning(false)
    }
  }

  const openReport = async (file: string) => {
    setReportFile(file)
    setReportBody('')
    setReportError('')
    try {
      const payload = await s.loadReport(file)
      setReportBody(payload.body)
    } catch (error) {
      setReportError(String(error instanceof Error ? error.message : error))
    }
  }

  const plan = preview.data
  const counts = plan?.counts ?? {}
  // 「两边已经一致」的技能数。AgentHub 同步中心会在统计卡下面显式说
  // 「另有 N 个技能两边已经一致，无需处理。」—— 这一句的作用是把「没有
  // 动作」和「页面坏了」区分开：214 个技能一致是正常的稳态，不是没东西可做。
  // 后端 `counts.skip` 就是这个数（plan 里 skip 动作 = 挂载已指向中央真身）。
  const skippedCount = counts.skip ?? 0

  // ── 表格行（对齐 AgentHub `syncer` 页的行构造）─────────────
  //
  // AgentHub 的同步中心把**动作行和冲突行放进同一张表**：
  //   - 动作行：技能名 = `mountName @ 工具名`，副标题 = note，徽章 = 动作类型
  //   - 冲突行：技能名 = 冲突标题，副标题 = detail，徽章 = 「需要你裁决」，
  //     右侧带一个「去裁决」按钮跳到去重页
  // 分两张表的话，用户得自己在两个地方数「一共几件事要办」。
  //
  // `skip` 动作不列表：它表示「两边已经一致」，上面那句提示条已经概括了，
  // 逐条列出来只会把真正要做的动作淹掉。
  const rows = React.useMemo(() => {
    const list: SyncRow[] = []
    for (const action of plan?.actions ?? []) {
      if (action.kind === 'skip') continue
      // 挂载行的技能名要带上工具：`skills/taste @ codex`，
      // 否则同名技能在多个工具下的挂载分不清是哪一条。
      const isMount = action.kind === 'mount'
      const label = isMount && action.tool
        ? `${action.mountName || action.skill} @ ${action.tool}`
        : action.skill
      // 后端把「原位转挂载」和「发布到工具」都记成 kind=mount，
      // 靠 replaceReal 区分（口径同 AgentHub 的 replaceReal ? mount : publish）。
      const kind = isMount && action.replaceReal === false ? 'publish' : action.kind
      list.push({
        key: `${action.kind}-${action.skill}-${action.tool}-${action.mountName}`,
        skill: label || '—',
        detail: action.note,
        kind,
        conflictId: '',
      })
    }
    for (const conflict of plan?.conflicts ?? []) {
      list.push({
        key: `conflict-${conflict.id}`,
        skill: conflict.skill || conflict.title,
        detail: conflict.detail,
        kind: 'conflict',
        conflictId: conflict.id,
      })
    }
    return list
  }, [plan])

  return (
    <Panel loading={preview.loading} error={preview.error} onRetry={preview.reload}>
      <div className='sk-sync'>
        <MetricGrid>
          <MetricCard>
            <MetricLabel>
              <MetricHint text={t('工具目录里有、中央仓库还没有的技能')}>{t('收进中央')}</MetricHint>
            </MetricLabel>
            <MetricValue>{num(counts.import ?? 0)}</MetricValue>
          </MetricCard>
          <MetricCard>
            <MetricLabel>{t('挂载变更')}</MetricLabel>
            <MetricValue>{num((counts.replace ?? 0) + (counts.publish ?? 0))}</MetricValue>
            <div className='sk-metric-sub'>
              {t('改为共用 {r} 处 · 分发到工具 {p} 处', { r: num(counts.replace ?? 0), p: num(counts.publish ?? 0) })}
            </div>
          </MetricCard>
          <MetricCard>
            <MetricLabel>
              <MetricHint text={t('同名技能但内容不一样，选保留哪个')}>{t('需要你裁决')}</MetricHint>
            </MetricLabel>
            <MetricValue className={counts.conflict ? 'is-warn' : undefined}>{num(counts.conflict ?? 0)}</MetricValue>
          </MetricCard>
        </MetricGrid>

        {/* ── 「两边已经一致」的显式说明 ──
            没有这一句时，「这次同步会做什么」下面是空的，用户分不清是稳态
            还是页面没加载出来。AgentHub 也有这一句（`另有 N 个技能两边已经
            一致，无需处理。`），数字来自同一份 plan 的 skip 计数。 */}
        {skippedCount > 0 ? (
          <p className='sk-sync-skip'>{t('另有 {n} 个技能两边已经一致，无需处理。', { n: num(skippedCount) })}</p>
        ) : null}

        <Section
          title={t('这次同步会做什么')}
          hint={t('（{mode}）确认执行前不会改任何文件。', {
            mode: plan?.mode === 'copy' ? t('复制模式') : t('Junction 共用模式'),
          })}
          action={
            <span className='sk-row-actions'>
              <Button type='button' size='sm' variant='ghost' disabled={running} onClick={preview.reload}>
                {t('重新扫描')}
              </Button>
              <Button type='button' size='sm' variant='default' disabled={running} onClick={() => void run()}>
                {running ? t('同步中…') : t('确认执行')}
              </Button>
            </span>
          }
        >
          {rows.length === 0 ? (
            // 空状态用圆形勾（AgentHub 的 `sk-empty-state`）：「一切就绪」和
            // 「没有内容」是两种不同的视觉，纯文字分不出来。
            <div className='sk-empty-state'>
              <span className='sk-empty-state-glyph' aria-hidden='true'>
                <svg viewBox='0 0 24 24' fill='none' stroke='currentColor' strokeWidth='2' strokeLinecap='round' strokeLinejoin='round'>
                  <circle cx='12' cy='12' r='9' />
                  <path d='M8 12.5l2.6 2.5L16 9.5' />
                </svg>
              </span>
              <span className='sk-empty-state-title'>{t('一切就绪，没有需要执行的动作')}</span>
            </div>
          ) : (
            <div className='sk-table sk-table-sync'>
              <div className='sk-table-head'>
                <span>{t('技能')}</span>
                <span>{t('将做什么')}</span>
                <span />
              </div>
              {rows.map(row => (
                <div key={row.key} className='sk-table-row'>
                  <span className='sk-cell-skill'>
                    <span className='sk-cell-name' title={row.skill}>{row.skill}</span>
                    {row.detail ? <span className='sk-cell-detail'>{row.detail}</span> : null}
                  </span>
                  <span><ActionBadge kind={row.kind} /></span>
                  <span>
                    {row.conflictId ? (
                      <Button type='button' size='sm' variant='ghost' onClick={() => onGo('dedup')}>
                        {t('去裁决')}
                      </Button>
                    ) : null}
                  </span>
                </div>
              ))}
            </div>
          )}
        </Section>

        {plan && plan.orphans.length > 0 ? (
          <Section
            title={t('待确认的孤儿目录（{n}）', { n: num(plan.orphans.length) })}
            hint={t('待收纳的陌生目录；程序只标记、不会删除它们。点一次「确认执行」就会被收进中央库并在原位转为挂载。')}
          >
            <div className='sk-table'>
              <div className='sk-table-head'>
                <span>{t('目录名')}</span>
                <span>{t('所在工具')}</span>
                <span>{t('最后修改')}</span>
              </div>
              {plan.orphans.map(item => (
                <div key={`${item.tool}-${item.name}`} className='sk-table-row'>
                  <span className='sk-cell-name' title={item.dir}>{item.name}</span>
                  <span className='sk-cell-tool'>{item.tool}</span>
                  <span>{ago(item.mtimeMs)}</span>
                </div>
              ))}
            </div>
          </Section>
        ) : null}

        {result ? (
          <Section
            title={t('本次同步结果')}
            hint={resolvedReportHint(result)}
            action={
              result.reportFile ? (
                <Button type='button' size='sm' variant='ghost' onClick={() => void openReport(result.reportFile)}>
                  {t('查看报告')}
                </Button>
              ) : null
            }
          >
            {result.manifestDiff.length === 0 ? (
              <Empty>{t('这一次没有改动任何文件')}</Empty>
            ) : (
              <pre className='sk-diff'>{result.manifestDiff.join('\n')}</pre>
            )}
          </Section>
        ) : null}

        {/* ── 同步报告（对齐 AgentHub：折叠在页面底部，标题带份数）────────
            AgentHub 的同步中心把报告收成一行「同步报告（N 份）」的折叠项，
            默认收起 —— 报告是回看用的，不该在「要做什么」旁边占一屏。 */}
        <details className='sk-report-fold' open={Boolean(reportBody)}>
          <summary>
            <span>{t('同步报告（{n} 份）', { n: num(reports.data?.items?.length ?? 0) })}</span>
          </summary>
          {reports.loading && !reports.data ? (
            <Empty>{t('正在读取…')}</Empty>
          ) : (reports.data?.items ?? []).length === 0 ? (
            <Empty>{t('还没同步过')}</Empty>
          ) : (
            <ul className='sk-report-list'>
              {(reports.data?.items ?? []).map(row => (
                <li key={row.file}>
                  <button
                    type='button'
                    className={`sk-report-link${reportFile === row.file ? ' is-active' : ''}`}
                    onClick={() => void openReport(row.file)}
                  >
                    {row.file}
                  </button>
                  <span className='sk-muted'>{ago(row.mtimeMs)} · {sizeText(row.size)}</span>
                </li>
              ))}
            </ul>
          )}

          {reportError ? <div className='sk-inline-error'>{reportError}</div> : null}
          {reportBody ? <pre className='sk-report-body'>{reportBody}</pre> : null}
        </details>
      </div>
    </Panel>
  )
}

/**
 * 动作徽标。文案在模块加载期取译文 —— 若在这里写 `t(map[kind].label)`，
 * i18n 扫描器只看到变量查表，提取不到「收纳 / 挂载 / 跳过 / 异常」四条键。
 */
/**
 * 动作徽章（对齐 AgentHub `f` 映射表）。
 *
 * AgentHub 的 kind → 文案：
 *   import  → 收进中央
 *   mount   → 改为共用      （原位转挂载）
 *   publish → 分发到工具    （工具目录里没有，发布一份）
 *   conflict→ 需要你裁决
 *   error   → 异常
 *
 * Agent2API 的后端只有 import / mount / skip / error 四种 kind，
 * `mount` 靠 `replace_real` 区分「原位转挂载」和「发布到工具」，
 * 前端拿不到这个字段，所以由后端在 note 里带上、这里按 note 判定。
 */
const ACTION_LABEL: Record<string, { label: string; variant: 'brand' | 'success' | 'info' | 'warning' | 'outline' | 'destructive' }> = {
  import: { label: t('收进中央'), variant: 'brand' },
  mount: { label: t('改为共用'), variant: 'success' },
  publish: { label: t('分发到工具'), variant: 'info' },
  conflict: { label: t('需要你裁决'), variant: 'warning' },
  error: { label: t('异常'), variant: 'destructive' },
}

function ActionBadge({ kind }: { kind: string }): React.ReactElement {
  const hit = ACTION_LABEL[kind] ?? { label: kind, variant: 'outline' as const }
  return <Badge variant={hit.variant} shape='tag'>{hit.label}</Badge>
}

/**
 * 指标卡标题上的 ⓘ 说明。
 *
 * AgentHub 的「收进中央」和「需要你裁决」两张卡，解释文字是挂在标题旁的
 * 问号气泡里，不是常驻副标题 —— 副标题位留给了真正需要一直可见的
 * 「改为共用 X 处 · 分发到工具 Y 处」（挂载变更卡）。
 */
function MetricHint({ text, children }: { text: string; children: React.ReactNode }): React.ReactElement {
  return (
    <span className='sk-metric-label-with-hint'>
      <span>{children}</span>
      <TooltipProvider delay={0}>
        <Tooltip>
          <TooltipTrigger render={<span className='sk-metric-hint' tabIndex={0} aria-label={text} />}>?</TooltipTrigger>
          <TooltipContent>
            <TooltipArrow />
            <div>{text}</div>
          </TooltipContent>
        </Tooltip>
      </TooltipProvider>
    </span>
  )
}

function resolvedReportHint(result: s.SyncResult): string {
  const s2 = result.summary
  return t('收进中央 {i} · 挂载 {m} · 合并 {g} · 跳过 {k} · 冲突 {c} · 清回收站 {d}', {
    i: s2.imported,
    m: s2.mounted,
    g: s2.merged,
    k: s2.skipped,
    c: s2.conflicts,
    d: s2.cleaned,
  })
}

export { SyncTab }