import * as React from 'react'
import { Badge, Button, MetricCard, MetricGrid, MetricLabel, MetricValue } from '@ui'
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

type Props = { onChanged: () => void }

function SyncTab({ onChanged }: Props): React.ReactElement {
  const preview = useLoad(() => s.loadPreview(), [], { refreshOnEnter: true })
  const reports = useLoad(() => s.loadReports(12), [])
  const [running, setRunning] = React.useState(false)
  const [result, setResult] = React.useState<s.SyncResult | null>(null)
  const [reportFile, setReportFile] = React.useState('')
  const [reportBody, setReportBody] = React.useState('')
  const [reportError, setReportError] = React.useState('')

  const run = async () => {
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

  return (
    <Panel loading={preview.loading} error={preview.error} onRetry={preview.reload}>
      <div className='sk-sync'>
        <MetricGrid>
          <MetricCard>
            <MetricLabel>{t('收进中央')}</MetricLabel>
            <MetricValue>{t('{n} 项', { n: num(counts.import ?? 0) })}</MetricValue>
            <div className='sk-metric-sub'>{t('渠道目录里的真身副本搬进中央')}</div>
          </MetricCard>
          <MetricCard>
            <MetricLabel>{t('挂载变更')}</MetricLabel>
            <MetricValue>{t('{n} 处', { n: num((counts.replace ?? 0) + (counts.publish ?? 0)) })}</MetricValue>
            <div className='sk-metric-sub'>
              {t('原位转挂载 {r} · 新发布 {p}', { r: num(counts.replace ?? 0), p: num(counts.publish ?? 0) })}
            </div>
          </MetricCard>
          <MetricCard>
            <MetricLabel>{t('需要裁决')}</MetricLabel>
            <MetricValue>{t('{n} 项', { n: num(counts.conflict ?? 0) })}</MetricValue>
            <div className='sk-metric-sub'>{t('同名不同内容、链接指向异常 —— 同步不会替你选')}</div>
          </MetricCard>
        </MetricGrid>

        <Section
          title={t('这次同步会做什么')}
          hint={t('预览不写任何文件；下面每一行都是执行时真的会做的事')}
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
          {!plan || plan.actions.length === 0 ? (
            <Empty>{t('没有要做的事：所有渠道与中央已经对齐')}</Empty>
          ) : (
            <div className='sk-table'>
              <div className='sk-table-head'>
                <span>{t('动作')}</span>
                <span>{t('技能')}</span>
                <span>{t('渠道')}</span>
                <span>{t('说明')}</span>
              </div>
              {plan.actions.map((action, at) => (
                <div key={`${action.kind}-${action.skill}-${action.tool}-${at}`} className='sk-table-row'>
                  <span><ActionBadge kind={action.kind} /></span>
                  <span className='sk-cell-name'>{action.skill || '—'}</span>
                  <span className='sk-cell-tool'>{action.tool || '—'}</span>
                  <span className='sk-cell-note' title={action.dir}>{action.note}</span>
                </div>
              ))}
            </div>
          )}
        </Section>

        {plan && plan.orphans.length > 0 ? (
          <Section
            title={t('孤儿目录')}
            hint={t('工具目录里有、台账里没有任何记录。可能是你刚拷进来的新技能，也可能是别的软件放的 —— 只标记，不动')}
          >
            <div className='sk-table'>
              <div className='sk-table-head'>
                <span>{t('目录名')}</span>
                <span>{t('渠道')}</span>
                <span>{t('最后修改')}</span>
                <span>{t('路径')}</span>
              </div>
              {plan.orphans.map(item => (
                <div key={`${item.tool}-${item.name}`} className='sk-table-row'>
                  <span className='sk-cell-name'>{item.name}</span>
                  <span className='sk-cell-tool'>{item.tool}</span>
                  <span>{ago(item.mtimeMs)}</span>
                  <span className='sk-cell-note' title={item.dir}>{item.dir}</span>
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

        <Section title={t('同步记录')} hint={t('每次同步都会落一份 Markdown 报告，含改了哪些文件')}>
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
        </Section>
      </div>
    </Panel>
  )
}

/**
 * 动作徽标。文案在模块加载期取译文 —— 若在这里写 `t(map[kind].label)`，
 * i18n 扫描器只看到变量查表，提取不到「收纳 / 挂载 / 跳过 / 异常」四条键。
 */
const ACTION_LABEL: Record<string, { label: string; variant: 'brand' | 'success' | 'outline' | 'destructive' | 'warning' }> = {
  import: { label: t('收纳'), variant: 'brand' },
  mount: { label: t('挂载'), variant: 'success' },
  skip: { label: t('跳过'), variant: 'outline' },
  error: { label: t('异常'), variant: 'destructive' },
}

function ActionBadge({ kind }: { kind: string }): React.ReactElement {
  const hit = ACTION_LABEL[kind] ?? { label: kind, variant: 'outline' as const }
  return <Badge variant={hit.variant} shape='tag'>{hit.label}</Badge>
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