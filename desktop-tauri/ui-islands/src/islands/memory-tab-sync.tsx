import * as React from 'react'
import { Button } from '@ui'
import { t } from '../i18n'
import * as m from './memory-api'
import { Empty, Panel, StateBadge, fullTime, timeText, useLoad } from './memory-shared'

/**
 * 同步页：WebDAV 状态 + 冲突裁决 + 设备列表 + 同步日志。
 *
 * ── 为什么同步是「整包 tar.gz + manifest」而不是逐文件同步 ────
 * 逐文件同步要为每个文件算增量、处理远端不支持 MOVE/COPY 的服务器、还要在
 * 弱网下反复往返。记忆目录总共几 MB、几百个文件，整包上传一次更快也更省心；
 * `manifest.json` 里带每个文件的 hash，接收端据此做三方合并（base / local /
 * remote）—— 这跟 git 的思路一样：包是传输单位，hash 是合并依据。
 *
 * ── 为什么冲突要人裁决而不是自动「保留最新」───────────────
 * 「最新」在两端各自成立：这台机器上刚改的，和另一台上刚改的，mtime 都新。
 * 自动挑一个必然有一边的工作丢失，而记忆是「只有用户自己能重建」的数据。
 * 所以冲突进队列，等人选。这也意味着**冲突会阻塞**：不裁决，那一条就一直是
 * 两边的旧版本 —— 这比静默丢数据好。
 *
 * ── 为什么不显示 WebDAV 密码 ────────────────────────────
 * 这个响应会出现在浏览器里、会被贴进 issue。凭据只显示「已配置」这一个事实，
 * 要改就去配置页改。
 *
 * ── 为什么这里没有「进度条」──────────────────────────────
 * 一次同步是同步阻塞跑完的，后端不落中间态，没有真实的百分比可显示。假装有
 * 进度条，要么是匀速动画（跟实际无关），要么卡在 90%（更糟）。所以只在按钮上
 * 写「同步中…」，跑完把 summary 说成一句人能读的话。
 */

/**
 * 字节数 → 「5.3MB」。
 *
 * 数字与单位之间不留空格：这一串会出现在「同步完成：上传 5.3MB · 冲突 0 条」
 * 这种一句话里，空格会把一个数值读成两个词。
 */
function sizeText(bytes: number | null | undefined): string {
  if (!bytes || bytes < 0) return '—'
  if (bytes < 1024) return `${bytes}B`
  if (bytes < 1024 * 1024) return `${(bytes / 1024).toFixed(1)}KB`
  if (bytes < 1024 * 1024 * 1024) return `${(bytes / 1024 / 1024).toFixed(1)}MB`
  return `${(bytes / 1024 / 1024 / 1024).toFixed(2)}GB`
}

/**
 * 「11-16 19:18」。
 *
 * 为什么不用 toLocaleString：它给出「2026/11/16 19:18:00」——年份和秒在这里
 * 都是噪声。同步是近几天的事，月日时分足够定位；完整时刻放 title 里悬停看。
 */
function clockText(timestamp: number | null | undefined): string {
  if (!timestamp) return '—'
  const at = new Date(timestamp)
  const pad = (value: number) => String(value).padStart(2, '0')
  return `${pad(at.getMonth() + 1)}-${pad(at.getDate())} ${pad(at.getHours())}:${pad(at.getMinutes())}`
}

/**
 * 上次同步结果里的字段。后端是 `json!` 出来的，字段可能缺，一律宽松读。
 *
 * 后端还会给 `files`（清点到的文件数）与 `packed`（真正进包的文件数）——
 * 这里不读：页面只用 `bytes` 说体积，两个文件数在正常同步里总是相等，
 * 只有「读不到内容」的边角情况才会不等，那时原因写在日志里。
 */
type LastSummary = {
  pulled?: number
  deleted?: number
  pushed?: number
  conflicts?: number
  skipped?: number
  bytes?: number
}

/**
 * 「当前阶段」这一行：把上次同步的结果说成一句人能读的话。
 *
 * 正在跑的时候由按钮上的「同步中…」表达，这里只描述**已经跑完的那一次**。
 *
 * 句式是「同步完成：上传 5.3MB · 下行 3 条 · 冲突 0 条」—— 三段并列、用 · 断句。
 * 「上传」而不是「上行」：这是用户视角的动作，不是抓包术语。
 */
function stageText(summary: LastSummary | null, running: boolean): string {
  if (running) return t('同步中：正在打包与上传，请稍候')
  if (!summary) return t('还没有同步过')
  const parts: string[] = []
  if (summary.pushed !== undefined) {
    parts.push(t('上传 {size}', { size: sizeText(summary.bytes) }))
  }
  if (summary.pulled) parts.push(t('下行 {n} 条', { n: summary.pulled }))
  if (summary.deleted) parts.push(t('删除 {n} 条', { n: summary.deleted }))
  if (summary.conflicts !== undefined) parts.push(t('冲突 {n} 条', { n: summary.conflicts }))
  return parts.length > 0 ? `${t('同步完成：')}${parts.join(' · ')}` : t('同步完成，没有变化')
}

/**
 * 右上角那个徽标：说的是「上一次跑成什么样」。
 *
 * 为什么不是「功能开没开」：开关状态由 switch 与 auto 两个配置决定，用户改完
 * 立刻生效，不需要一个「运行中」的假象。真正想知道的是「上次同步成功了没有」，
 * 所以这里按 lastRun / lastSyncAt 出四种结果。
 */
function runBadge(configured: boolean, running: boolean, lastRun: m.Outcome | null, lastSyncAt: number): {
  state: 'ok' | 'warn' | 'bad' | 'idle'
  label: string
} {
  if (!configured) return { state: 'warn', label: t('未配置') }
  if (running) return { state: 'warn', label: t('同步中') }
  const status = lastRun?.status ?? (lastSyncAt ? 'done' : '')
  if (!status) return { state: 'idle', label: t('从未同步') }
  if (status === 'done') return { state: 'ok', label: t('完成') }
  if (status === 'skipped') return { state: 'warn', label: t('已跳过') }
  return { state: 'bad', label: t('失败') }
}

function Sync(): React.ReactElement {
  const { data, loading, error, reload } = useLoad(() => m.loadSync(), [])
  const conflicts = useLoad(() => m.loadSyncConflicts(), [])
  const log = useLoad(() => m.loadSyncLog(50), [])
  const [busy, setBusy] = React.useState('')
  const [actionError, setActionError] = React.useState('')
  const [test, setTest] = React.useState<{ ok: boolean; message: string } | null>(null)
  const [lastRun, setLastRun] = React.useState<m.Outcome | null>(null)
  const [showLog, setShowLog] = React.useState(false)
  const [showPreview, setShowPreview] = React.useState(false)
  const preview = useLoad(() => (showPreview ? m.loadSyncPreview() : Promise.resolve(null)), [showPreview])

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
  const pending = conflicts.data?.conflicts ?? []
  const summary = (lastRun?.result ?? data.lastResult ?? null) as LastSummary | null
  const running = busy === 'run'
  const badge = runBadge(data.configured, running, lastRun, data.lastSyncAt)

  /**
   * 跳到配置页。
   *
   * 配置页的「同步」分组没法靠 URL 直达（页签与分组都是页面岛内部状态），
   * 所以两边约定一个自定义事件：`memory-page` 收到 `mm:tab` 就切页签，
   * 配置页收到 `mm:group` 就切分组。任一环节没挂上就只是切页签，不会报错。
   */
  const gotoConfig = () => {
    window.dispatchEvent(new CustomEvent('mm:tab', { detail: { tab: 'settings' } }))
    window.dispatchEvent(new CustomEvent('mm:group', { detail: { group: '同步' } }))
  }

  return (
    <div className='mm-sync'>
      {actionError ? <div className='mm-error'>{actionError}</div> : null}

      {/* ── 顶部：一句话说清这页在干什么 + 主操作 ── */}
      <div className='mm-sync-bar'>
        <span className='mm-sync-bar-text'>
          {t('把记忆文件夹整体打包上传/下载，两边都改且不一样时进冲突队列等你裁决')}
        </span>
        <div className='mm-inline-actions'>
          {/* 顶栏只放「立即同步」一个动作。刷新不在这儿：一次同步跑完 act()
              已经把状态 / 冲突 / 日志三个端点一起重拉，手动刷新只在看日志时
              才需要 —— 那个按钮在日志卡展开后。 */}
          <Button
            type='button'
            size='sm'
            disabled={running || !data.configured}
            onClick={() => void act('run', async () => {
              const result = await m.runSync()
              setLastRun(result.outcome)
            })}
          >
            {running ? t('同步中…') : t('立即同步')}
          </Button>
        </div>
      </div>

      {/* ── 同步状态 ── */}
      <section className='mm-cfg-card'>
        <div className='mm-sync-head'>
          <div className='mm-cfg-card-title'>{t('同步状态')}</div>
          {/* 地址独立成一个格子，落在下方「值列」的起始线上 —— 这样标题 / 地址 /
              状态徽标各占一列，眼睛顺着竖线往下读，不会因为标题字数变化而漂移。 */}
          <span className='mm-sync-head-url' title={data.url || undefined}>
            {data.url}
          </span>
          <div className='mm-cfg-card-actions'>
            <StateBadge state={badge.state} label={badge.label} />
            {data.configured && !data.enabled ? (
              <span className='mm-cfg-card-note'>{t('同步功能已关闭')}</span>
            ) : null}
          </div>
        </div>

        <div className='mm-sync-grid'>
          <span>{t('服务器')}</span>
          <div className='mm-sync-cell'>
            <code>{data.url || t('未填写 WebDAV 地址')}</code>
            {/* 「修改凭据」和「测连接」都只跟服务器地址有关，所以都放在这一行，
                而不是散在页头 —— 用户要改/验的就是眼前这一个地址。 */}
            <Button type='button' size='sm' variant='outline' onClick={gotoConfig}>
              {t('修改凭据')}
            </Button>
            <Button
              type='button'
              size='sm'
              variant='ghost'
              disabled={busy === 'test' || !data.configured}
              onClick={() => void act('test', async () => { setTest(await m.testSync()) })}
            >
              {busy === 'test' ? t('测试中…') : t('测连接')}
            </Button>
          </div>

          <span>{t('远端目录')}</span>
          <div className='mm-sync-cell'>
            <code>{data.remoteDir ? `/${data.remoteDir}` : '—'}</code>
          </div>

          <span>{t('上次同步')}</span>
          <div className='mm-sync-cell'>
            {data.lastSyncAt ? (
              <code title={fullTime(data.lastSyncAt)}>
                {clockText(data.lastSyncAt)}（{timeText(data.lastSyncAt)}）
              </code>
            ) : (
              <code>{t('还没同步过')}</code>
            )}
          </div>

          <span>{t('当前阶段')}</span>
          <div className='mm-sync-cell'>
            <code>{stageText(summary, running)}</code>
          </div>

          <span>
            {t('冲突')}
            <span className='mm-sync-info' title={t('两边都改且内容不同时进队列，不自动合并 —— 自动挑一个必然有一边的工作丢失。')}>
              i
            </span>
          </span>
          <div className='mm-sync-cell'>
            <code>{data.conflicts}</code>
            {data.skipped > 0 ? (
              <span className='mm-muted'>{t('另有 {n} 条已在跳过名单里', { n: data.skipped })}</span>
            ) : null}
          </div>
        </div>

        {/* 「凭什么这么同步」的边界说明。目标界面里那句话讲的是共用凭据与根目录
            隔离，我们的实现是「记忆目录独立，凭据在配置页单独填」，所以照实写：
            同步范围 + 本地目录 + 运行节奏。全挤在一行里 —— 它们回答的是同一个
            问题「这页说的同步，具体是哪份数据、按什么节奏」，分成两行会让这张卡
            凭空长高一截，而它本该是「五行数据 + 一句脚注」的形状。
            文件数与单包上限只在 `title` 里：几百个文件是常态、上限只有超限时才被
            想起（超限会直接把原因写进「当前阶段」），两者都不值得占脚注的宽度。 */}
        <div
          className='mm-sync-note'
          title={t('{n} 个本地文件，{m} 个有基线；单包上限 {c} MB', {
            n: data.localFiles,
            m: data.baseFiles,
            c: data.packSizeLimitMB,
          })}
        >
          {t('同步 general / projects / profile，index.db 等可重建文件不入包；本地目录：')}
          {data.localDir ? <code>{data.localDir}</code> : null}
          <span className='mm-sync-sep'> · </span>
          <span className={data.auto ? 'mm-sync-on' : ''}>
            {data.auto ? t('每 {n} 分钟自动同步', { n: data.intervalMin }) : t('自动同步已关')}
          </span>
        </div>

        {test ? (
          <p className={`mm-outcome ${test.ok ? 'mm-outcome-done' : 'mm-outcome-fail'}`}>
            {test.ok ? t('连接正常：') : t('连接失败：')}{test.message}
          </p>
        ) : null}
        {lastRun && lastRun.status !== 'done' ? (
          <p className='mm-outcome mm-outcome-fail'>
            {lastRun.status === 'skipped'
              ? t('跳过：{reason}', { reason: lastRun.reason ?? '—' })
              : t('失败：{reason}', { reason: lastRun.error ?? '—' })}
          </p>
        ) : null}
      </section>

      {/* ── 冲突裁决 ── */}
      <section className='mm-cfg-card'>
        <div className='mm-cfg-card-head'>
          <div className='mm-cfg-card-title'>
            {t('冲突裁决')}
            <span className='mm-sync-info' title={t('两边都改且内容不同时进队列，一律等人裁决 —— 自动挑一边必然丢掉另一边的工作。')}>i</span>
          </div>
          <div className='mm-cfg-card-actions'>
            {/* 一行小字而不是徽标：零条是好消息，给它一个绿色圆角框会显得
                页面到处都在「报警/OK」。这里只想让人扫到「有没有要处理的」，
                有的话按下面会列出条目，标题行不必抢戏。
                两段之间用 · 分隔而不是靠 gap：它们在读的时候是一句话
                （「0 条待裁决，且一律不合并」），不该被当成两栏。 */}
            <span className={pending.length > 0 ? 'mm-sync-flag-warn' : 'mm-cfg-card-note'}>
              {t('{n} 条待裁决', { n: pending.length })}
            </span>
            <span className='mm-cfg-card-note'>·</span>
            <span className='mm-cfg-card-note'>{t('一律不自动合并')}</span>
          </div>
        </div>

        {conflicts.loading ? <Panel loading error=''>{null}</Panel> : null}
        {!conflicts.loading && pending.length === 0 ? (
          <div className='mm-sync-empty'>{t('没有待裁决冲突')}</div>
        ) : null}

        {pending.map(row => (
          <div key={row.id} className='mm-conflict'>
            <div className='mm-conflict-head'>
              <StateBadge state='warn' label={t('冲突')} />
              <code>{row.path}</code>
              <span className='mm-muted'>{t('来自设备 {device}', { device: row.device })}</span>
              <span className='mm-muted' title={fullTime(row.createdAt)}>{timeText(row.createdAt)}</span>
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
      </section>

      {/* ── 设备列表 ── */}
      <section className='mm-cfg-card'>
        <div className='mm-cfg-card-head'>
          <div className='mm-cfg-card-title'>
            {t('设备列表')}
            {/* 这两条解释都进 title：卡片下面不再挂一行说明。
                目标界面这张卡就是「标题 + 表」，多一行小字会让它比上面两张卡
                显得更重，而设备列表恰恰是最不需要读的一张。 */}
            <span
              className='mm-sync-info'
              title={t('远端每台设备各自上传一个包，这里列的是包，不是在线状态。「最后同步」的本机行取本机记录的时刻，其他设备取远端包的修改时间 —— 那个包是它上传的那一刻写的。')}
            >
              i
            </span>
          </div>
        </div>

        {devices.length === 0 ? (
          <div className='mm-sync-empty'>
            {data.configured ? t('远端还没有任何包，下一次「立即同步」会创建第一个。') : t('未配置 WebDAV，没有设备。')}
          </div>
        ) : (
          <div className='mm-device-table'>
            <div className='mm-device-row mm-device-head'>
              <span>{t('设备')}</span>
              <span>{t('最后同步')}</span>
            </div>
            {devices.map(item => (
              <div key={item.package} className={item.self ? 'mm-device-row is-self' : 'mm-device-row'}>
                <span className='mm-device-name'>
                  <code title={item.device}>{item.device}</code>
                </span>
                <span
                  className={item.self ? 'mm-device-when self' : 'mm-device-when'}
                  title={item.lastSyncAt ? fullTime(item.lastSyncAt) : undefined}
                >
                  {item.lastSyncAt ? timeText(item.lastSyncAt) : '—'}
                </span>
              </div>
            ))}
          </div>
        )}
      </section>

      {/* ── 同步日志（折叠） ── */}
      <section className='mm-cfg-card'>
        <div className='mm-cfg-card-head'>
          <div className='mm-cfg-card-title'>{t('同步日志')}</div>
          <div className='mm-cfg-card-actions'>
            <span className='mm-cfg-card-note'>
              {t('{n} 条，出问题时先看这里（含失败原因）', { n: log.data?.total ?? 0 })}
            </span>
            <Button type='button' size='sm' variant='ghost' onClick={() => setShowLog(value => !value)}>
              {showLog ? t('收起') : t('展开')}
            </Button>
            <span
              className='mm-sync-info'
              title={t('每次同步都会写一条：打包、上传、合并、冲突、失败原因。默认折叠是因为正常情况下没人需要看它，出问题时才需要。')}
            >
              i
            </span>
          </div>
        </div>

        {showLog ? (
          <>
            <div className='mm-inline-actions'>
              <Button type='button' size='sm' variant='ghost' onClick={() => log.reload()}>{t('刷新')}</Button>
              <Button type='button' size='sm' variant='ghost' onClick={() => setShowPreview(value => !value)}>
                {showPreview ? t('收起包清单') : t('看本次会打包什么')}
              </Button>
            </div>

            {showPreview ? (
              preview.loading ? <Panel loading error=''>{null}</Panel> : preview.data ? (
                <div className='mm-file-list'>
                  <div className='mm-muted'>
                    {t('{n} 个文件，生成于 {when}', { n: preview.data.files.length, when: timeText(preview.data.createdAt) })}
                  </div>
                  {preview.data.files.slice(0, 200).map(file => (
                    <div key={file.path} className='mm-file-row'>
                      <code>{file.path}</code>
                      <span className='mm-muted'>{sizeText(file.size)}</span>
                    </div>
                  ))}
                  {preview.data.files.length > 200 ? (
                    <div className='mm-muted'>{t('还有 {n} 个未列出', { n: preview.data.files.length - 200 })}</div>
                  ) : null}
                </div>
              ) : null
            ) : null}

            {log.loading ? <Panel loading error=''>{null}</Panel> : null}
            {log.data && log.data.entries.length === 0 ? (
              <div className='mm-sync-empty'>{t('还没有同步记录。')}</div>
            ) : null}
            {log.data?.entries.map(row => (
              <div key={row.id} className='mm-log-row'>
                {/* 日志是历史记录：这里要的是「哪一天几点」，不是「多久以前」。
                    相对时间在日志里会让人反复心算，而且三天前的一行和三条前
                    的一行都显示「3 天前」时分不清先后。 */}
                <span className='mm-muted mm-log-when' title={fullTime(row.at)}>{clockText(row.at)}</span>
                <StateBadge state={row.ok ? 'ok' : 'bad'} label={row.ok ? t('成功') : t('失败')} />
                <span className='mm-log-action'>{row.action}</span>
                <span className='mm-muted'>{row.detail}</span>
              </div>
            ))}
          </>
        ) : null}
      </section>
    </div>
  )
}

export { Sync }