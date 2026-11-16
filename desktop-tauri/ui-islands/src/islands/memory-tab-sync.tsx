import * as React from 'react'
import { Badge, Button } from '@ui'
import { t } from '../i18n'
import * as m from './memory-api'
import { Empty, Grid, Panel, Section, StateBadge, timeText, useLoad } from './memory-shared'

/**
 * 同步页：WebDAV 配置摘要 + 立即同步 + 冲突裁决 + 设备 + 流水 + 包清单。
 *
 * ── 为什么同步是「整包 tar.gz + manifest」而不是逐文件同步 ────
 * 逐文件同步要为每个文件算增量、处理远端不支持 MOVE/COPY 的服务器、还要在
 * 弱网下反复往返。记忆目录总共几 MB、几百个文件，整包上传一次更快也更省心；
 * `manifest.json` 里带每个文件的 hash，接收端据此做三方合并（base / local /
 * remote）—— 这跟 git 的思路一样：包是传输单位，hash 是合并依据。
 *
 * ── 为什么冲突要人裁决而不是自动「保留最新」──────���───────────
 * 「最新」在两端各自成立：这台机器上刚改的，和另一台上刚改的，mtime 都新。
 * 自动挑一个必然有一边的工作丢失，而记忆是「只有用户自己能重建」的数据。
 * 所以冲突进队列，等人选。这也意味着**冲突会阻塞**：不裁决，那一条就一直是
 * 两边的旧版本 —— 这比静默丢数据好。
 *
 * ── 为什么不显示 WebDAV 密码 ────────────────────────────────
 * 这个响应会出现在浏览器里、会被贴进 issue。凭据只显示「已配置」这一个事实，
 * 要改就去配置页改。
 */

function Sync(): React.ReactElement {
  const { data, loading, error, reload } = useLoad(() => m.loadSync(), [])
  const conflicts = useLoad(() => m.loadSyncConflicts(), [])
  const log = useLoad(() => m.loadSyncLog(60), [])
  const [busy, setBusy] = React.useState('')
  const [actionError, setActionError] = React.useState('')
  const [test, setTest] = React.useState<{ ok: boolean; message: string } | null>(null)
  const [lastRun, setLastRun] = React.useState<m.Outcome | null>(null)
  const [showPreview, setShowPreview] = React.useState(false)
  const preview = useLoad(() => showPreview ? m.loadSyncPreview() : Promise.resolve(null), [showPreview])

  const act = async (key: string, task: () => Promise<unknown>) => {
    setBusy(key)
    setActionError('')
    try {
      await task()
      reload()
      conflicts.reload()
      log.reload()
    } catch (caught) {
      setActionError(caught instanceof Error ? caught.message : String(caught))
    } finally {
      setBusy('')
    }
  }

  if (loading) return <Panel loading error=''>{null}</Panel>
  if (error) return <Panel loading={false} error={error} onRetry={reload}>{null}</Panel>
  if (!data) return <Empty>{t('读不到同步状态。')}</Empty>

  const devices = data.devices?.devices ?? []

  return (
    <div className='mm-sync'>
      {actionError ? <div className='mm-error'>{actionError}</div> : null}

      <Section
        title={t('WebDAV')}
        hint={t('同步的是整个记忆目录（tar.gz + manifest）。索引库与设备身份不入包 —— 它们各自可重建。')}
        action={(
          <div className='mm-inline-actions'>
            <Button
              type='button'
              size='sm'
              variant='ghost'
              disabled={busy === 'test'}
              onClick={() => void act('test', async () => {
                setTest(await m.testSync())
              })}
            >
              {busy === 'test' ? t('测试中…') : t('测连接')}
            </Button>
            <Button
              type='button'
              size='sm'
              disabled={busy === 'run' || !data.configured}
              onClick={() => void act('run', async () => {
                const result = await m.runSync()
                setLastRun(result.outcome)
              })}
            >
              {busy === 'run' ? t('同步中…') : t('立即同步')}
            </Button>
          </div>
        )}
      >
        <div className='mm-inline-meta'>
          <StateBadge state={data.configured ? 'ok' : 'warn'} label={data.configured ? t('已配置') : t('未配置 WebDAV')} />
          <StateBadge state={data.enabled ? 'ok' : 'idle'} label={data.enabled ? t('同步已启用') : t('同步已关闭')} />
          {data.auto ? <Badge variant='brand' shape='tag'>{t('自动每 {n} 分钟', { n: data.intervalMin })}</Badge> : null}
          {data.conflicts > 0 ? <Badge variant='destructive' shape='tag'>{t('{n} 个冲突', { n: data.conflicts })}</Badge> : null}
          <span className='mm-muted'>
            {data.lastSyncAt ? t('上次同步：{when}', { when: timeText(data.lastSyncAt) }) : t('还没同步过')}
          </span>
        </div>

        {!data.configured ? (
          <p className='mm-hint'>
            {t('去「配置 → 同步」填 WebDAV 地址、账号与密码。坚果云 / Nextcloud / 群晖都行 —— 只要支持 PROPFIND 与 PUT。')}
          </p>
        ) : null}

        <Grid
          rows={[
            [t('地址'), data.url || '—'],
            [t('远端目录'), data.remoteDir || '—'],
            [t('本机设备'), `${data.deviceName || '—'}（${data.device}）`],
            [t('本机包名'), data.package],
            [t('文件数'), `${data.localFiles} 个本地 · ${data.baseFiles} 个有基线`],
            [t('索引入包'), data.excludeIndex ? t('否（推荐）') : t('是')],
            [t('单包上限'), `${data.packSizeLimitMB} MB`],
          ]}
        />

        {test ? (
          <p className={`mm-outcome ${test.ok ? 'mm-outcome-done' : 'mm-outcome-fail'}`}>
            {test.ok ? t('连接正常：') : t('连接失败：')}{test.message}
          </p>
        ) : null}
        {lastRun ? (
          <p className='mm-outcome mm-outcome-done'>
            {lastRun.status === 'done'
              ? t('同步完成：{detail}', { detail: JSON.stringify(lastRun.result ?? {}).slice(0, 200) })
              : lastRun.status === 'skipped'
                ? t('跳过：{reason}', { reason: lastRun.reason ?? '—' })
                : t('失败：{reason}', { reason: lastRun.error ?? '—' })}
          </p>
        ) : null}
      </Section>

      <Section
        title={t('冲突（{n}）', { n: conflicts.data?.count ?? 0 })}
        hint={t('两边都改过同一个文件。选一边保留 —— 另一边会进回收站，仍可恢复。')}
      >
        {conflicts.loading ? <Panel loading error=''>{null}</Panel> : null}
        {conflicts.data && conflicts.data.conflicts.length === 0 ? (
          <div className='mm-muted'>{t('没有冲突。')}</div>
        ) : null}
        {conflicts.data?.conflicts.map(row => (
          <div key={row.id} className='mm-conflict'>
            <div className='mm-conflict-head'>
              <StateBadge state='warn' label={t('冲突')} />
              <code>{row.path}</code>
              <span className='mm-muted'>{t('来自设备 {device}', { device: row.device })}</span>
              <span className='mm-muted'>{timeText(row.createdAt)}</span>
            </div>
            {row.detail ? <div className='mm-muted'>{row.detail}</div> : null}
            <div className='mm-conflict-hashes'>
              <span>{t('本地')} <code>{row.localHash?.slice(0, 12) ?? '—'}</code></span>
              <span>{t('远端')} <code>{row.remoteHash?.slice(0, 12) ?? '—'}</code></span>
              <span>{t('基线')} <code>{row.baseHash?.slice(0, 12) ?? '—'}</code></span>
            </div>
            <div className='mm-detail-actions'>
              <Button type='button' size='sm' onClick={() => void act(`c${row.id}`, () => m.resolveSyncConflict(row.id, 'local'))}>
                {t('保留本地')}
              </Button>
              <Button type='button' size='sm' variant='ghost' onClick={() => void act(`c${row.id}`, () => m.resolveSyncConflict(row.id, 'remote'))}>
                {t('保留远端')}
              </Button>
              <Button type='button' size='sm' variant='ghost' onClick={() => void act(`c${row.id}`, () => m.resolveSyncConflict(row.id, 'ignore'))}>
                {t('忽略（两个都留着）')}
              </Button>
            </div>
          </div>
        ))}
      </Section>

      <Section
        title={t('设备')}
        hint={t('同一个 WebDAV 目录下的所有设备。')}
        action={<Button type='button' size='sm' variant='ghost' onClick={reload}>{t('刷新')}</Button>}
      >
        {devices.length === 0 ? (
          <div className='mm-muted'>{data.configured ? t('远端还没有任何包。') : t('未配置。')}</div>
        ) : (
          <div className='mm-device-list'>
            {devices.map(item => (
              <div key={item.package} className='mm-device-row'>
                <code>{item.device}</code>
                {item.self ? <Badge variant='brand' shape='tag'>{t('本机')}</Badge> : <Badge variant='outline' shape='tag'>{t('其他设备')}</Badge>}
              </div>
            ))}
          </div>
        )}
      </Section>

      <Section
        title={t('同步包清单')}
        hint={t('「立即同步」之前想确认会传什么，看这里。只列路径与 hash，不下发正文。')}
        action={(
          <Button type='button' size='sm' variant='ghost' onClick={() => setShowPreview(value => !value)}>
            {showPreview ? t('收起') : t('展开清单')}
          </Button>
        )}
      >
        {showPreview ? (
          preview.loading ? <Panel loading error=''>{null}</Panel> : preview.data ? (
            <>
              <div className='mm-muted'>
                {t('{n} 个文件，生成于 {when}', { n: preview.data.files.length, when: timeText(preview.data.createdAt) })}
              </div>
              <div className='mm-file-list'>
                {preview.data.files.slice(0, 200).map(file => (
                  <div key={file.path} className='mm-file-row'>
                    <code>{file.path}</code>
                    <span className='mm-muted'>{file.size} B</span>
                  </div>
                ))}
                {preview.data.files.length > 200 ? (
                  <div className='mm-muted'>{t('还有 {n} 个未列出', { n: preview.data.files.length - 200 })}</div>
                ) : null}
              </div>
            </>
          ) : null
        ) : <div className='mm-muted'>{t('点「展开清单」看会打进包的文件。')}</div>}
      </Section>

      <Section title={t('同步流水')} action={<Button type='button' size='sm' variant='ghost' onClick={log.reload}>{t('刷新')}</Button>}>
        {log.loading ? <Panel loading error=''>{null}</Panel> : null}
        {log.data && log.data.entries.length === 0 ? <div className='mm-muted'>{t('还没有同步记录。')}</div> : null}
        {log.data?.entries.map(row => (
          <div key={row.id} className='mm-log-row'>
            <span className='mm-muted'>{timeText(row.at)}</span>
            <StateBadge state={row.ok ? 'ok' : 'bad'} label={row.ok ? t('成功') : t('失败')} />
            <span>{row.action}</span>
            <span className='mm-muted'>{row.detail}</span>
          </div>
        ))}
      </Section>
    </div>
  )
}

export { Sync }
