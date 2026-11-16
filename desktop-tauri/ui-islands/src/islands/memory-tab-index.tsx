import * as React from 'react'
import { Badge, Button, Input } from '@ui'
import { t } from '../i18n'
import * as m from './memory-api'
import { Empty, Grid, Panel, Section, StateBadge, num, timeText, useLoad } from './memory-shared'

/**
 * 检索与索引页：自检 / 重建 / 体检 / 检索调试。
 *
 * ── 为什么这一页要存在 ──────────────────────────────────────
 * 「记忆库」是派生数据：事实源是 `.md`，索引是 SQLite。派生数据会坏 —— 手改
 * 文件、同步过来一半、进程被杀在事务中间。这一页是那个「坏了怎么办」的入口。
 * 重建永远安全（因为事实源没动过），所以它是这一页的主按钮。
 *
 * ── 为什么「检索调试」默认折叠 ──────────────────────────────
 * 它是给排障用的：看一次查询命中了哪几条、七项打分各是多少。日常用户不需要，
 * 而摆在外面会让人以为「检索需要调」。折叠起来，需要时展开。
 *
 * ── 体检里的「可能过时」是怎么判的 ──────────────────────────
 * 记忆条目可以带 `files`（相对路径锚点）。如果那个文件在记忆写下之后被改过，
 * 这条记忆的结论可能已经不对了 —— 不是错误，是「值得复核」。这条规则的用意
 * 是让「代码改了但记忆没更新」这件事变得可见。
 */

function Index(): React.ReactElement {
  const status = useLoad(() => m.loadStatus(), [])
  const health = useLoad(() => m.loadHealth(50), [])
  const [busy, setBusy] = React.useState(false)
  const [actionError, setActionError] = React.useState('')
  const [rebuilt, setRebuilt] = React.useState<number | null>(null)
  const [probe, setProbe] = React.useState('')
  const [probeResult, setProbeResult] = React.useState<m.MemoryList | null>(null)
  const [showProbe, setShowProbe] = React.useState(false)

  const rebuild = async () => {
    setBusy(true)
    setActionError('')
    try {
      const result = await m.rebuildIndex()
      setRebuilt(result.rebuilt)
      status.reload()
      health.reload()
    } catch (caught) {
      setActionError(caught instanceof Error ? caught.message : String(caught))
    } finally {
      setBusy(false)
    }
  }

  const runProbe = async () => {
    setBusy(true)
    setActionError('')
    try {
      setProbeResult(await m.loadList({ query: probe.trim(), limit: 20 }))
    } catch (caught) {
      setActionError(caught instanceof Error ? caught.message : String(caught))
    } finally {
      setBusy(false)
    }
  }

  const s = status.data

  return (
    <div className='mm-index'>
      {actionError ? <div className='mm-error'>{actionError}</div> : null}

      <Section
        title={t('索引')}
        hint={t('索引是派生数据：删掉 index.db 重建即可，记忆正文在 .md 文件里。')}
        action={(
          <Button type='button' size='sm' disabled={busy} onClick={() => void rebuild()}>
            {busy ? t('重建中…') : t('重建索引')}
          </Button>
        )}
      >
        {status.loading ? <Panel loading error=''>{null}</Panel> : null}
        {s ? (
          <>
            <div className='mm-inline-meta'>
              <StateBadge
                state={s.index.inSync ? 'ok' : 'warn'}
                label={s.index.inSync ? t('与事实源一致') : t('待重建')}
              />
              <Badge variant='outline' shape='tag'>{s.index.fts ? 'FTS5' : t('内存匹配（FTS5 不可用）')}</Badge>
              <Badge variant='outline' shape='tag'>{t('内容版本 r{n}', { n: s.revision })}</Badge>
              {rebuilt !== null ? <Badge variant='success' shape='tag'>{t('刚重建 {n} 条', { n: rebuilt })}</Badge> : null}
            </div>
            <Grid
              rows={[
                [t('索引条数'), `${num(s.index.count)} / 有效 ${num(s.live)}`],
                [t('记忆根'), s.root],
                [t('索引文件'), s.indexFile],
                [t('设备 id'), s.device],
                [t('条数'), `${num(s.live)} 有效 · ${num(s.l2)} 沉淀 · ${num(s.l1)} 流水 · ${num(s.deleted)} 回收站 · ${num(s.superseded)} 已取代`],
                [t('待确认'), `${num(s.review.pending)} 条（告警阈值 ${s.review.warnThreshold}）`],
              ]}
            />
            {s.tables ? (
              <>
                <div className='mm-section-title'>{t('表行数')}</div>
                <div className='mm-table-stats'>
                  {Object.entries(s.tables).map(([name, rows]) => (
                    <div key={name} className='mm-table-stat'>
                      <code>{name}</code>
                      <span>{num(rows)}</span>
                    </div>
                  ))}
                </div>
              </>
            ) : null}
          </>
        ) : null}
      </Section>

      <Section
        title={t('体检')}
        hint={t('这三类问题都不会自己消失，但也不影响读写 —— 它们只影响「记忆还准不准」。')}
        action={<Button type='button' size='sm' variant='ghost' onClick={health.reload}>{t('重新体检')}</Button>}
      >
        {health.loading ? <Panel loading error=''>{null}</Panel> : null}
        {health.data ? (
          <>
            <div className='mm-inline-meta'>
              <StateBadge
                state={health.data.staleCount ? 'warn' : 'ok'}
                label={t('{n} 条可能过时', { n: health.data.staleCount })}
              />
              <StateBadge
                state={health.data.review.backlogged ? 'warn' : 'ok'}
                label={t('待确认 {n} 条', { n: health.data.review.pending })}
              />
              <StateBadge
                state={health.data.index.inSync ? 'ok' : 'warn'}
                label={health.data.index.inSync ? t('索引一致') : t('索引漂移')}
              />
            </div>
            <p className='mm-hint'>{health.data.note}</p>

            {health.data.stale.length ? (
              <div className='mm-stale-list'>
                {health.data.stale.map(row => (
                  <div key={row.id} className='mm-stale'>
                    <div className='mm-stale-head'>
                      <strong>{row.title}</strong>
                      <Badge variant='secondary' shape='tag'>{row.type}</Badge>
                      <Badge variant='outline' shape='tag'>{row.layer}</Badge>
                      <span className='mm-muted'>{t('写下于 {when}', { when: row.updatedText })}</span>
                    </div>
                    <div className='mm-stale-files'>
                      {row.files.map(file => (
                        <span key={file.file} className='mm-warn'>
                          <code>{file.file}</code> {t('改于 {when}', { when: file.changedText })}
                        </span>
                      ))}
                    </div>
                  </div>
                ))}
              </div>
            ) : (
              <Empty>{t('没有发现可能过时的记忆 —— 也就是说，带文件锚点的记忆所指向的文件都没再变过。')}</Empty>
            )}
          </>
        ) : null}
      </Section>

      <Section
        title={t('检索调试')}
        hint={t('看一次查询到底命中了什么、排在第几。七项混排：bm25 / 新鲜度 / 重要度 / 亲密度 / 图扩散 / 热度 / 层级。')}
        action={(
          <Button type='button' size='sm' variant='ghost' onClick={() => setShowProbe(value => !value)}>
            {showProbe ? t('收起') : t('展开')}
          </Button>
        )}
      >
        {showProbe ? (
          <>
            <div className='mm-filters'>
              <Input
                value={probe}
                placeholder={t('输入查询，例如「端口 3065」')}
                onChange={event => setProbe(event.target.value)}
                onKeyDown={event => { if (event.key === 'Enter') void runProbe() }}
              />
              <Button type='button' size='sm' disabled={busy} onClick={() => void runProbe()}>
                {busy ? t('检索中…') : t('检索')}
              </Button>
            </div>
            {probeResult ? (
              <>
                <div className='mm-muted'>
                  {t('命中 {n} 条 · {engine}', {
                    n: probeResult.total,
                    engine: probeResult.fts ? 'FTS5' : t('内存匹配'),
                  })}
                </div>
                <div className='mm-probe-list'>
                  {probeResult.lines.map((line, index) => (
                    <div key={index} className='mm-probe-row'>
                      <span className='mm-mono'>{index + 1}</span>
                      <span>{line}</span>
                    </div>
                  ))}
                  {probeResult.lines.length === 0 ? <div className='mm-muted'>{t('没有命中。')}</div> : null}
                </div>
              </>
            ) : null}
          </>
        ) : <div className='mm-muted'>{t('展开后可以输入查询看命中结果。')}</div>}
      </Section>

      <Section title={t('维护')} hint={t('回收站里的条目保留一段时间后自动清理；也可以手动提前清。')}>
        <div className='mm-inline-actions'>
          <Button
            type='button'
            size='sm'
            variant='ghost'
            disabled={busy}
            onClick={() => void (async () => {
              setBusy(true)
              setActionError('')
              try {
                await m.purgeTrash()
                status.reload()
                health.reload()
              } catch (caught) {
                setActionError(caught instanceof Error ? caught.message : String(caught))
              } finally {
                setBusy(false)
              }
            })()}
          >
            {t('清理过期回收站')}
          </Button>
          <span className='mm-muted'>
            {s ? t('当前回收站 {n} 条', { n: s.deleted }) : ''}
          </span>
        </div>
      </Section>
    </div>
  )
}

export { Index }
