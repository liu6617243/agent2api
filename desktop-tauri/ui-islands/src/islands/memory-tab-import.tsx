import * as React from 'react'
import { Badge, Button, Input, Switch } from '@ui'
import { t } from '../i18n'
import * as m from './memory-api'
import { Empty, Panel, Section, StateBadge, num, timeText, useLoad } from './memory-shared'

/**
 * 导入页：来源清单 + 干跑预览 + 真导入。
 *
 * ── 为什么「干跑」是强制的一步 ──────────────────────────────
 * 导入是这套系统里唯一的**批量写盘**：一次几千条，写错了会污染检索结果，
 * 更糟的是把隐私内容搬进一个会被 MCP 工具暴露出去的地方。所以流程固定两段，
 * 而且后端在 `import.dryRunFirst` 为真时会拒绝「没预览就直接导」的请求 ——
 * 防的不是新手，是「用户在另一个标签页里点了按钮」。
 *
 * ── 为什么预览要把「跳过的原因」列出来 ──────────────────────
 * 只报「会导入 800 条」等于什么都没说。用户需要知道「为什么另外 200 条没进来」——
 * 太短？重复？含密钥？这三种情况的处理方式完全不同。
 *
 * ── 为什么按「文件」而不是「消息」切条 ──────────────────────
 * 一次会话的价值在于上下文连着读。拆成一条条消息之后，「为什么这么改」与
 * 「改了哪里」分家，检索命中一半反而误导。所以一个文件一条。
 */

function Import(): React.ReactElement {
  const { data, loading, error, reload } = useLoad(() => m.loadImport(), [])
  const [preview, setPreview] = React.useState<m.ImportPreview | null>(null)
  const [summary, setSummary] = React.useState<m.ImportSummary | null>(null)
  const [busy, setBusy] = React.useState(false)
  const [actionError, setActionError] = React.useState('')
  const [adding, setAdding] = React.useState(false)
  const [newPath, setNewPath] = React.useState('')
  const [newFormat, setNewFormat] = React.useState('md')
  const [newPriority, setNewPriority] = React.useState('0')

  const act = async (task: () => Promise<unknown>) => {
    setBusy(true)
    setActionError('')
    try {
      await task()
      reload()
    } catch (caught) {
      setActionError(caught instanceof Error ? caught.message : String(caught))
    } finally {
      setBusy(false)
    }
  }

  if (loading) return <Panel loading error=''>{null}</Panel>
  if (error) return <Panel loading={false} error={error} onRetry={reload}>{null}</Panel>
  if (!data) return <Empty>{t('读不到导入配置。')}</Empty>

  return (
    <div className='mm-import'>
      {actionError ? <div className='mm-error'>{actionError}</div> : null}

      <Section
        title={t('来源')}
        hint={data.note}
        action={(
          <div className='mm-inline-actions'>
            <Button type='button' size='sm' variant='ghost' onClick={() => setAdding(value => !value)}>
              {adding ? t('收起') : t('添加来源')}
            </Button>
            <Button
              type='button'
              size='sm'
              disabled={busy || data.sources.length === 0}
              onClick={() => void act(async () => {
                setSummary(null)
                setPreview(await m.previewImport(undefined, 80))
              })}
            >
              {busy ? t('扫描中…') : t('干跑预览')}
            </Button>
          </div>
        )}
      >
        {adding ? (
          <div className='mm-add-source'>
            <Input
              value={newPath}
              placeholder={t('目录或文件路径，例如 D:/notes 或 ~/.claude/projects')}
              onChange={event => setNewPath(event.target.value)}
            />
            <select className='mm-select' value={newFormat} onChange={event => setNewFormat(event.target.value)}>
              <option value='md'>{t('Markdown 笔记')}</option>
              <option value='jsonl'>{t('会话记录（jsonl）')}</option>
            </select>
            <Input
              className='mm-priority-input'
              type='number'
              value={newPriority}
              placeholder={t('优先级')}
              onChange={event => setNewPriority(event.target.value)}
            />
            <Button
              type='button'
              size='sm'
              disabled={!newPath.trim()}
              onClick={() => void act(async () => {
                await m.saveImportSources([
                  ...data.sources.map(item => ({
                    path: item.path,
                    format: item.format,
                    enabled: item.enabled,
                    priority: item.priority,
                  })),
                  {
                    path: newPath.trim(),
                    format: newFormat,
                    enabled: true,
                    priority: Number(newPriority) || 0,
                  },
                ])
                setNewPath('')
                setAdding(false)
              })}
            >
              {t('添加')}
            </Button>
          </div>
        ) : null}

        {data.sources.length === 0 ? (
          <Empty>{t('还没有来源。点「添加来源」指一个 Markdown 笔记目录，或一个会话记录目录。')}</Empty>
        ) : (
          <div className='mm-source-list'>
            {data.sources.map(source => (
              <div key={source.path} className='mm-source-row'>
                <div className='mm-source-main'>
                  <div className='mm-source-title'>
                    <strong>{source.label}</strong>
                    <Badge variant='outline' shape='tag'>{source.format === 'jsonl' ? t('会话记录') : t('Markdown')}</Badge>
                    <StateBadge state={source.exists ? 'ok' : 'bad'} label={source.exists ? t('可读') : t('路径不存在')} />
                    <Badge variant='secondary' shape='tag'>{t('{n} 个文件', { n: source.files })}</Badge>
                    {typeof source.take === 'number' && source.take < source.files ? (
                      <Badge variant='warning' shape='tag'>{t('本次读 {n}', { n: source.take })}</Badge>
                    ) : null}
                  </div>
                  <code className='mm-source-path'>{source.path}</code>
                </div>
                <div className='mm-source-actions'>
                  <Switch
                    checked={source.enabled}
                    size='sm'
                    onCheckedChange={value => void act(() => m.saveImportSources(
                      data.sources.map(item => item.path === source.path
                        ? { path: item.path, format: item.format, enabled: value, priority: item.priority }
                        : { path: item.path, format: item.format, enabled: item.enabled, priority: item.priority }),
                    ))}
                  />
                  <Button
                    type='button'
                    size='sm'
                    variant='ghost'
                    onClick={() => void act(() => m.saveImportSources(
                      data.sources
                        .filter(item => item.path !== source.path)
                        .map(item => ({ path: item.path, format: item.format, enabled: item.enabled, priority: item.priority })),
                    ))}
                  >
                    {t('移除')}
                  </Button>
                </div>
              </div>
            ))}
          </div>
        )}
      </Section>

      {preview ? (
        <Section
          title={t('干跑结果')}
          hint={t('这一屏只读文件，一个字都没写。确认之后再点「开始导入」。')}
          action={(
            <Button
              type='button'
              size='sm'
              disabled={busy || preview.ready === 0}
              onClick={() => void act(async () => {
                const result = await m.runImport()
                setSummary(result.summary)
                setPreview(null)
              })}
            >
              {busy ? t('导入中…') : t('开始导入 {n} 条', { n: preview.ready })}
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
              {preview.skipped.map(row => (
                <div key={row.path} className='mm-skipped-row'>
                  <span className='mm-preview-title' title={row.path}>{row.title || row.path}</span>
                  <span className='mm-warn'>{row.reason}</span>
                </div>
              ))}
            </div>
          ) : null}
        </Section>
      ) : null}

      {summary ? (
        <Section title={t('导入完成')}>
          <div className='mm-inline-meta'>
            <Badge variant='success' shape='tag'>{t('写入 {n}', { n: summary.written })}</Badge>
            {summary.duplicated ? <Badge variant='outline' shape='tag'>{t('重复 {n}', { n: summary.duplicated })}</Badge> : null}
            {summary.skipped ? <Badge variant='outline' shape='tag'>{t('跳过 {n}', { n: summary.skipped })}</Badge> : null}
            {summary.failed ? <Badge variant='destructive' shape='tag'>{t('失败 {n}', { n: summary.failed })}</Badge> : null}
            <span className='mm-muted'>{t('扫描 {n} 个文件', { n: summary.scanned })}</span>
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

      {data.lastRun ? (
        <Section title={t('上次导入')} hint={t('时间：{when}', { when: data.lastRun.atText || timeText(data.lastRun.at) })}>
          <div className='mm-inline-meta'>
            <Badge variant='brand' shape='tag'>{t('写入 {n}', { n: num(data.lastRun.written) })}</Badge>
            <Badge variant='outline' shape='tag'>{t('重复 {n}', { n: data.lastRun.duplicated })}</Badge>
            <Badge variant='outline' shape='tag'>{t('跳过 {n}', { n: data.lastRun.skipped })}</Badge>
            {data.lastRun.failed ? <Badge variant='destructive' shape='tag'>{t('失败 {n}', { n: data.lastRun.failed })}</Badge> : null}
          </div>
        </Section>
      ) : null}
    </div>
  )
}

export { Import }
