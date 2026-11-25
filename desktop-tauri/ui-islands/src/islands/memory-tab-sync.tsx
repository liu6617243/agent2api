import * as React from 'react'
import { Button, Input, SegmentedControl, Switch } from '@ui'
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
 * 这个响应会出现在浏览器里、会被贴进 issue。密码是 `Kind::Secret`，后端落盘
 * 加密、`flat()` 只回空串，所以这里永远拿不到原文；输入框只按 `hasPassword`
 * 画一串掩码点，空串的语义是「沿用已保存的那份」（`config::apply_one`）。
 *
 * ── 为什么这里没有「进度条」──────────────────────────────
 * 一次同步是同步阻塞跑完的，后端不落中间态，没有真实的百分比可显示。假装有
 * 进度条，要么是匀速动画（跟实际无关），要么卡在 90%（更糟）。所以只在按钮上
 * 写「同步中…」，跑完把 summary 说成一句人能读的话。
 *
 * ── 为什么凭据也能在这一页改 ──────────────────────────────
 * 原来「凭据」只有一个「修改凭据」的跳转，指向配置页的「同步」分组。真用过
 * 一次就知道那句话是废话：填地址 / 账号 / 密码、测连接、看远端有没有包，
 * 这三件事是**同一个动作的上下一步**，中间夹一次页签跳转（还把用户刚改的
 * 草稿留在配置页）纯属自找麻烦。所以这一页直接给一组输入框，保存走
 * `POST /api/memory/sync/setup`（只接受 `sync.*` 键，不碰配置页草稿）。
 *
 * ── 为什么这里只给记忆一个模块 ─────────────────────────────
 * 本仓只有记忆实现了同步：打包、三方合并、冲突裁决都是围着记忆目录转的。
 * 与其摆一排点了没反应的「立即同步」，不如只留真正会动的那一个 —— 同步
 * 入口的数量应当等于能跑的同步数量，多出来的都是噪音。
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
  /** 自动拼起来的 l1 容器条数（两台机器同一天都写过日志）。 */
  merged?: number
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
  // 「拼接」而不是「合并」：这条说的是**同一天的日志被拼成一份**，不是两条记忆
  // 被合成一条。用「合并」会和「合并项目」撞词，而那件事会改项目归属。
  if (summary.merged) parts.push(t('拼接 {n} 条', { n: summary.merged }))
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

/**
 * 「多久以前」：设备列表与快速同步里用。
 *
 * 与 `timeText` 的分工：那个给的是「11-16 19:18」这种绝对时刻 —— 适合「上次同步
 * 发生在哪一天」；这里是「58 分钟前」，适合「那台机器还活不活」。两者都在
 * `title` 里带上对方的形态，悬停就能互换。
 */
function agoText(timestamp: number | null | undefined): string {
  if (!timestamp) return t('从未同步')
  const delta = Date.now() - timestamp
  if (delta < 0) return t('刚刚')
  const minutes = Math.floor(delta / 60000)
  if (minutes < 1) return t('刚刚')
  if (minutes < 60) return t('{n} 分钟前', { n: minutes })
  const hours = Math.floor(minutes / 60)
  if (hours < 24) return t('{n} 小时前', { n: hours })
  const days = Math.floor(hours / 24)
  if (days < 30) return t('{n} 天前', { n: days })
  return clockText(timestamp)
}

/**
 * 「自动同步」的节奏文案。
 *
 * 后端默认 1440 分钟（24 小时）；直接按分钟写出来会变成「每 1440 分钟自动同步」，
 * 用户得自己除 60。整小时就换成小时，除不尽才退回分钟（5 分钟 / 90 分钟都能表达）。
 */
function intervalText(minutes: number): string {
  if (minutes >= 60 && minutes % 60 === 0) return t('每 {h} 小时自动同步', { h: minutes / 60 })
  return t('每 {n} 分钟自动同步', { n: minutes })
}

/**
 * 存储类型的可选档位。
 *
 * `s3` 存得下、跑不起来（打包与列目录还是 WebDAV 那套），所以它带 `disabled`：
 * 让用户看得见「这条路在这里没通」，而不是选完之后报一个看不懂的错。
 */
const PROVIDERS = [
  { value: 'webdav', label: t('WebDAV 网盘') },
  { value: 's3', label: t('S3 兼容对象存储'), disabled: true, title: t('本仓尚未接入 S3：打包、列目录、三方合并都还是 WebDAV 那一套') },
] as const

/**
 * 存储预设：只是「这台机器接的是谁家的网盘」的备忘。
 *
 * 为什么要有它而不是干脆去掉：同一份配置在多设备之间同步时，值会因为换机而
 * 变成另一家的地址，届时「上一次是哪家」这个信息只有用户自己记得。它不改变
 * 任何行为——地址、账号、目录都还是各自那一格里的值。
 *
 * 取值必须与后端 `sync.preset` 的 `options` 一致（`config.rs` 的
 * `["custom","jianguoyun","nextcloud","synology","fnos"]`）：后端是 Enum，
 * 传进去不认的值会被 `apply_one` 拒掉，界面上表现为「保存设置」报错。
 */
const PRESETS = [
  { value: 'custom', label: '自定义' },
  { value: 'jianguoyun', label: '坚果云' },
  { value: 'nextcloud', label: 'Nextcloud' },
  { value: 'synology', label: '群晖 DSM' },
  { value: 'fnos', label: '飞牛 fnOS' },
] as const

type SetupForm = {
  provider: string
  url: string
  username: string
  password: string
  remoteDir: string
  deviceName: string
  preset: string
}

/**
 * 从状态拼出表单初值。
 *
 * 密码恒为空串：后端从不下发原文，空串的语义是「沿用已保存的那份」
 * （`config::apply_one` 的 Secret 分支）。所以「用户没动密码框」与「密码本来是
 * 空的」在提交时是同一个值，区别只体现在占位符上（`hasPassword`）。
 */
function formOf(data: m.SyncStatus): SetupForm {
  return {
    provider: data.provider ?? 'webdav',
    // 地址后端已经兜了默认值（坚果云），所以这里拿到的一定是「该显示的那串」：
    // 用户填过就是他填的，没填过就是默认值。地址框因此不是空的 —— 用户只需补账号密码。
    url: data.url ?? '',
    username: data.username ?? '',
    password: '',
    remoteDir: data.remoteDir ?? '',
    // 电脑名没填过就用系统电脑名预填：系统本来就知道这台机器叫什么，让用户手打
    // 一遍只会多出一个打错的机器名（多设备列表里就多出一台不存在的机器）。
    // 预填之后它就是一个**看得见、可改、保存即落盘**的值，而不是一个空框。
    deviceName: data.deviceName || data.hostName || '',
    preset: String(data.preset ?? 'custom'),
  }
}

function Sync(): React.ReactElement {
  const { data, loading, error, reload } = useLoad(() => m.loadSync(), [])
  const conflicts = useLoad(() => m.loadSyncConflicts(), [])
  const log = useLoad(() => m.loadSyncLog(50), [])
  const [busy, setBusy] = React.useState('')
  const [actionError, setActionError] = React.useState('')
  // 测连接的结果带一个来源：两张卡各有一个「测连接 / 测试连接」按钮，结果只该
  // 出现在按下去的那张卡里。同一个结果在两处同时冒出来，用户会以为「我点了两次？」
  const [test, setTest] = React.useState<{ ok: boolean; message: string; from: 'setup' | 'status' } | null>(null)
  const [lastRun, setLastRun] = React.useState<m.Outcome | null>(null)
  const [showLog, setShowLog] = React.useState(false)
  const [showPreview, setShowPreview] = React.useState(false)
  /**
   * 同步间隔输入框的草稿（`null` = 没在编辑，直接显示已保存的值）。
   *
   * 为什么不 onChange 直接存：输入「60」时中间会先经过「6」，存下去立刻 reload，
   * `data.intervalMin` 变成 6、框里的值被回写成 6，用户再敲一下就得到「66」——
   * 一个他从没打算填的数。所以中间态只活在草稿里，失焦或回车才提交。
   */
  const [intervalDraft, setIntervalDraft] = React.useState<string | null>(null)
  const preview = useLoad(() => (showPreview ? m.loadSyncPreview() : Promise.resolve(null)), [showPreview])

  // ── 凭据与目录的草稿 ──
  //
  // 这一份草稿只属于同步页：保存走 `/sync/setup`，不经过配置页那套草稿层。
  // 初值来自 `data`，但在 `data` 因为别的原因重载（比如刚同步完）时**不覆盖**
  // 用户正在改的内容 —— 所以用 `drafted` 记「用户动过没有」，动过就不铺初值。
  const [form, setForm] = React.useState<SetupForm | null>(null)
  const drafted = React.useRef(false)
  const [confirmReset, setConfirmReset] = React.useState(false)

  // 凭据卡在本页最上面。状态卡那一行「修改凭据」不再跨页签跳转，而是把它滚到
  // 视野里并让地址框获得焦点 —— 用户点这个按钮的意图就是「我要改那个地址」，
  // 在这里直接满足，比跳去配置页再让他自己找回来快一步。
  const credentialCard = React.useRef<HTMLElement | null>(null)
  const gotoCredentials = () => {
    const node = credentialCard.current
    if (!node) return
    node.scrollIntoView({ behavior: 'smooth', block: 'start' })
    const input = node.querySelector<HTMLInputElement>('input')
    input?.focus({ preventScroll: true })
  }

  React.useEffect(() => {
    if (!data || drafted.current) return
    setForm(formOf(data))
  }, [data])

  const put = (patch: Partial<SetupForm>) => {
    drafted.current = true
    setForm(current => (current ? { ...current, ...patch } : current))
  }

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
  // 后端把「待裁决」排在前面，但**没有过滤掉已裁决的**（那张表是历史留痕：
  // 裁决过的行会保留 status=resolved，供流水追溯）。页面这一栏说的是「要你
  // 处理的」，所以这里必须按 status 过滤 —— 否则点完「保留本地」，那一条还
  // 挂在原位，用户会以为没生效，再点一次。
  const conflictRows = conflicts.data?.conflicts ?? []
  const pending = conflictRows.filter(row => row.status === 'pending')
  // 「N 条待裁决」用后端的**总数**，不是这一页的行数：`conflict_list(200)` 截断到
  // 200 条，用行数报会在第 201 条起永久少报，而且没有任何提示。
  const pendingTotal = conflicts.data?.count ?? pending.length
  const summary = (lastRun?.result ?? data.lastResult ?? null) as LastSummary | null
  const running = busy === 'run'
  const badge = runBadge(data.configured, running, lastRun, data.lastSyncAt)
  const current = form ?? formOf(data)
  // 「就绪」只判账号和密码：地址有默认值，恒有值。不满足时说清楚缺哪个，
  // 用户才知道该回到上面那张卡补什么。
  const ready = data.configured && data.hasPassword !== false
  const blocked = !data.configured ? t('还没填 WebDAV 账号') : t('还没填 WebDAV 密码')

  /**
   * 「间隔」的唯一写入点：失焦 / 回车。
   *
   * 越界值（<5 或 >1440）不提交，也不弹一句报错：输入框是受控的，草稿一清就
   * 自动回到已保存的值 —— 用户看到「填了 0 又变回去」就懂了。报错反而要解释
   * 「0 不代表关掉自动同步，关掉请用上面的开关」，那是一句话说不完的注解。
   */
  const commitInterval = () => {
    const raw = (intervalDraft ?? '').trim()
    setIntervalDraft(null)
    if (!raw) return
    const next = Math.round(Number(raw))
    if (!Number.isFinite(next) || next === data.intervalMin) return
    if (next < 5 || next > 1440) return
    void act('interval', () => m.saveSyncSetup({ 'sync.intervalMin': next }))
  }

  return (
    <div className='mm-sync'>
      {actionError ? <div className='mm-error'>{actionError}</div> : null}

      {/* ── 顶部：一句话说清这页在干什么 ──
          主操作不在这儿：截图里这张卡是纯说明条，真正的「立即同步」在下面的
          「快速同步」里 —— 用户要按的是「哪一份数据」，而不是笼统的「同步」。
          顶栏再放一个同名按钮会让人以为有两个入口。 */}
      <div className='mm-sync-bar'>
        <span className='mm-sync-bar-text'>
          {t('把记忆文件夹整体打包上传/下载，两边都改且不一样时进冲突队列等你裁决')}
        </span>
      </div>

      {/* ── WebDAV 服务器（凭据）──
          截图里这张卡自带账号 / 密码两个框和三个按钮。这里照做，但
          「测连接」测的是**当前输入框里的值**（后端 `post_sync_test` 接 body），
          这样可以「先验再存」；密码框从不下发原文，占位符说明「已保存」与否。 */}
      <section className='mm-cfg-card' ref={credentialCard}>
        <div className='mm-cfg-card-head'>
          <div className='mm-cfg-card-title'>{t('统一 WebDAV 服务器')}</div>
          <span className='mm-cfg-card-note'>
            {t('密码经系统密钥加密保存，界面只显示掩码')}
          </span>
        </div>

        <div className='mm-sync-field'>
          <span className='mm-sync-field-label'>{t('存储类型')}</span>
          <SegmentedControl
            className='mm-sync-seg'
            aria-label={t('存储类型')}
            options={PROVIDERS}
            value={current.provider === 's3' ? 's3' : 'webdav'}
            onValueChange={next => put({ provider: next })}
          />
          {current.provider === 's3' ? (
            <span className='mm-sync-field-hint mm-sync-pending'>
              {t('S3 只存得下配置，跑不起来：打包、列目录、三方合并都还是 WebDAV 那一套。选 WebDAV 才有可用的同步。')}
            </span>
          ) : null}
        </div>

        <div className='mm-sync-pair'>
          <label className='mm-sync-field'>
            <span className='mm-sync-field-label'>
              {t('服务器地址（WebDAV）')}
              {/* 「默认值」这枚标记只在 `sync.url` 没填过时出现：框里那串是后端
                  替我们填的坚果云地址，不是用户确认过的。不标出来，用户会以为
                  自己上次填过它。改任何一个字符（或保存一次）它就消失。 */}
              {data.urlDefault ? (
                <span className='mm-sync-field-aside'>{t('（预填默认值，可改）')}</span>
              ) : null}
            </span>
            <Input
              value={current.url}
              placeholder='https://dav.jianguoyun.com/dav/'
              spellCheck={false}
              onChange={event => put({ url: event.target.value })}
            />
          </label>
          <label className='mm-sync-field'>
            <span className='mm-sync-field-label'>{t('账号')}</span>
            <Input
              value={current.username}
              spellCheck={false}
              autoComplete='username'
              onChange={event => put({ username: event.target.value })}
            />
          </label>
        </div>

        {/* 远端目录必须在这一张卡里：它跟地址、账号、密码一样是「怎么连」的一部分
            —— URL 的最后一段就是它，缺了它这张卡的三个输入框拼不出一个可用地址。 */}
        <label className='mm-sync-field'>
          <span className='mm-sync-field-label'>
            {t('远端目录')}
            <span className='mm-sync-field-aside'>{t('（记忆中枢在网盘上的根目录）')}</span>
          </span>
          <Input
            value={current.remoteDir}
            spellCheck={false}
            placeholder='agent2api-memory'
            onChange={event => put({ remoteDir: event.target.value })}
          />
        </label>

        <label className='mm-sync-field'>
          <span className='mm-sync-field-label'>
            {t('密码')}
            <span className='mm-sync-field-aside'>{t('（WebDAV 密码，多设备必须一致；经系统密钥加密保存）')}</span>
          </span>
          <Input
            type='password'
            autoComplete='new-password'
            value={current.password}
            /* 已经存过就画一串掩码点（跟真密码的显示一模一样），而不是写一句
               「已保存」。理由：这里的位置语义就是「密码框」，用户扫一眼就知道
               「这格填过了」；写一句话反而让人怀疑这格是不是不用填。
               真正的事实放在 title 里，悬停能看到「留空 = 沿用」。 */
            placeholder={
              data.hasPassword
                ? '••••••••'
                : t('未设置（留空 = 沿用已保存的密码）')
            }
            title={t('留空 = 沿用已保存的密码；输入任意字符即覆盖')}
            onChange={event => put({ password: event.target.value })}
          />
        </label>

        <div className='mm-sync-actions'>
          <Button
            type='button'
            size='sm'
            variant='outline'
            disabled={busy === 'probe'}
            onClick={() =>
              void act('probe', async () => {
                const result = await m.testSync({
                  url: current.url,
                  username: current.username,
                  // 空串 = 没动这一栏，交给后端沿用已保存的密码
                  password: current.password,
                  remoteDir: current.remoteDir,
                })
                setTest({ ...result, from: 'setup' })
              })
            }
          >
            {busy === 'probe' ? t('测试中…') : t('测试连接')}
          </Button>
          <Button
            type='button'
            size='sm'
            variant='default'
            disabled={busy === 'setup'}
            onClick={() =>
              void act('setup', async () => {
                await m.saveSyncSetup({
                  'sync.provider': current.provider,
                  'sync.url': current.url,
                  'sync.username': current.username,
                  'sync.password': current.password,
                  'sync.remoteDir': current.remoteDir,
                  'sync.preset': current.preset,
                  'sync.deviceName': current.deviceName,
                })
                // 保存成功后草稿清空：下一次 `data` 回来就铺新值
                drafted.current = false
                setForm(null)
              })
            }
          >
            {busy === 'setup' ? t('保存中…') : t('保存设置')}
          </Button>
          <Button
            type='button'
            size='sm'
            variant='destructive'
            onClick={() => setConfirmReset(true)}
          >
            {t('清除配置')}
          </Button>
        </div>

        {confirmReset ? (
          <div className='mm-cfg-confirm'>
            <span>
              {t('会清掉地址 / 账号 / 密码 / 目录这些同步配置（远端包与本机已同步的数据不动）。确定？')}
            </span>
            <div className='mm-inline-actions'>
              <Button
                type='button'
                size='sm'
                variant='destructive'
                onClick={() =>
                  void act('reset', async () => {
                    await m.resetSyncSetup()
                    drafted.current = false
                    setForm(null)
                    setTest(null)
                    setConfirmReset(false)
                  })
                }
              >
                {t('确认清除')}
              </Button>
              <Button type='button' size='sm' variant='ghost' onClick={() => setConfirmReset(false)}>
                {t('取消')}
              </Button>
            </div>
          </div>
        ) : null}

        {test?.from === 'setup' ? (
          <p className={`mm-outcome ${test.ok ? 'mm-outcome-done' : 'mm-outcome-fail'}`}>
            {test.ok ? t('连接正常：') : t('连接失败：')}{test.message}
          </p>
        ) : null}
      </section>

      {/* ── 本机与存储预设 ──
          存储预设记着这台机器接的是谁家的网盘，电脑名是别的设备在「设备列表」
          里看到的称呼。两者都不改变同步行为，所以跟真正决定「怎么连」的凭据
          分开，单独存。 */}
      <section className='mm-cfg-card'>
        <div className='mm-cfg-card-head'>
          <div className='mm-cfg-card-title'>{t('本机与存储预设')}</div>
          <span className='mm-cfg-card-note'>
            {t('存储预设只是备忘，不改变任何同步行为；电脑名随同步包上传，别的设备在「设备列表」里看到的就是它')}
          </span>
        </div>

        <label className='mm-sync-field'>
          <span className='mm-sync-field-label'>
            {t('存储预设')}
            <span className='mm-sync-field-aside'>{t('（仅备忘，帮你记服务器是哪家的）')}</span>
          </span>
          <select
            className='mm-select mm-sync-select'
            value={current.preset}
            onChange={event => put({ preset: event.target.value })}
          >
            {PRESETS.map(item => (
              <option key={item.value} value={item.value}>{t(item.label)}</option>
            ))}
          </select>
        </label>

        {/* 「电脑名」这一格：标签独占一行，输入框和「保存预设与电脑名」按钮并排一行。
            为什么不是原来那种「label 里塞标签+输入框，按钮当 label 的兄弟」：
            那样按钮的参照系是「标签+输入框」整块（52px 高），而 align-items 又被
            .mm-sync-inline 的 center 盖成了居中 —— 按钮既没跟输入框对齐，
            中间隔着一整片空白，「这一格和这个按钮是同一件事」的关联也被切断了。
            现在输入框和按钮是同一行的兄弟节点（都是 30px 高），天然平齐，
            按钮也紧跟在输入框右边。 */}
        <div className='mm-sync-device'>
          <label className='mm-sync-field-label' htmlFor='mm-sync-device-name'>
            {/* 名字随包上传进 `manifest.name`，别的机器在下面「设备列表」里看到的就是它。 */}
            {t('电脑名（下方设备列表里显示）')}
          </label>
          <div className='mm-sync-inline'>
            <Input
              id='mm-sync-device-name'
              className='mm-sync-device-input'
              value={current.deviceName}
              spellCheck={false}
              /* 占位符给设备 id：电脑名已经预填成系统电脑名了，框里通常不是空的，
                 这行占位符只在用户手动清空后可见 —— 那时他需要知道「留空会退到什么」，
                 答案就是同步包名用的设备 id。 */
              placeholder={data.device}
              onChange={event => put({ deviceName: event.target.value })}
            />
            <Button
              type='button'
              /* default 档 = h-[30px]，跟 Input 的 h-[30px] 一致。
                 原来用 sm（26px），就算对齐了也会比输入框矮 4px。 */
              size='default'
              variant='outline'
              disabled={busy === 'dirs'}
              onClick={() =>
                void act('dirs', async () => {
                  await m.saveSyncSetup({
                    'sync.deviceName': current.deviceName,
                    'sync.preset': current.preset,
                  })
                  drafted.current = false
                  setForm(null)
                })
              }
            >
              {busy === 'dirs' ? t('保存中…') : t('保存预设与电脑名')}
            </Button>
          </div>
        </div>
      </section>

      {/* ── 快速同步 ──
          本仓只有记忆实现了同步，所以这里只给一行：按钮可用与否直接读同步状态
          （有没有账号、有没有密码），跑的就是 `runSync`。不再摆一排灰按钮 ——
          点一下没反应的「同步」比没有按钮更让人怀疑整个功能。

          页头不放「刷新状态」：每次动作（保存 / 测连接 / 同步）之后 `act()` 已经
          重载了状态，再给一个手动刷新按钮等于承认「显示的可能过期」—— 而它并不会。 */}
      <section className='mm-cfg-card'>
        <div className='mm-cfg-card-head'>
          <div className='mm-cfg-card-title'>{t('快速同步')}</div>
          <span className='mm-cfg-card-note'>
            {t('记忆中枢是本仓唯一实现了同步的模块')}
          </span>
        </div>

        <div className='mm-quick-row'>
          <div className='mm-quick-main'>
            <div className='mm-quick-title'>
              {t('记忆中枢同步')}
              <span
                className='mm-sync-info'
                title={t('把 general / projects / profile 打包上传；远端有别人的包时做三方合并')}
              >
                i
              </span>
            </div>
            <div className='mm-quick-sub'>
              {t('根目录')} <code>/{data.remoteDir}</code>
            </div>
            <div className={ready ? 'mm-quick-ok' : 'mm-quick-off'}>
              {ready ? t('已就绪') : blocked}
              {data.lastSyncAt ? (
                <>
                  <span className='mm-sync-sep'> · </span>
                  {t('上次同步 {when}', { when: agoText(data.lastSyncAt) })}
                </>
              ) : null}
            </div>
          </div>
          <Button
            type='button'
            size='sm'
            variant='outline'
            className={ready ? 'mm-quick-run' : undefined}
            disabled={!ready || running}
            onClick={() =>
              void act('run', async () => {
                const result = await m.runSync()
                setLastRun(result.outcome)
              })
            }
          >
            {running ? t('同步中…') : t('立即同步')}
          </Button>
        </div>
      </section>

      {/* ── 同步行为（总开关 / 自动定时 / 间隔）──
          这三个键原来只在配置页的「同步」分组里有控件，而配置页现在把它们标成
          owner=`sync`（那一页只留一句「去同步页改」）。所以编辑入口必须落在这一
          页 —— 否则「去同步页」指过去是一片只读状态，用户会以为功能坏了。

          为什么放在「快速同步」下面而不是塞进「同步状态」那张卡：状态卡是参考
          数据（地址 / 上次同步 / 冲突数），这三项是**能改的东西**。混在一起，
          整卡会在「读」和「写」之间反复横跳；分开之后，卡片标题就是语义分界。

          为什么即时保存而不是给一个「保存」按钮：开关没有中间态，改了就是改了；
          一键保存还会引出「我改了没保存就切页签」的旧问题。 */}
      <section className='mm-cfg-card'>
        <div className='mm-cfg-card-head'>
          <div className='mm-cfg-card-title'>{t('同步行为')}</div>
          <span className='mm-cfg-card-note'>
            {t('决定「什么时候自动跑」；手动「立即同步」不受这三项影响')}
          </span>
        </div>

        <div className='mm-sync-grid mm-sync-behave'>
          <span>{t('同步总开关')}</span>
          <div className='mm-sync-cell'>
            <Switch
              aria-label={t('同步总开关')}
              checked={data.enabled}
              disabled={busy === 'enabled'}
              onCheckedChange={next => {
                // 只在真的变了时发请求：Base UI 的开关在点按时总会回调，
                // 同值提交会让 `act()` 白跑一轮 reload（页面闪一下）。
                if (next === data.enabled) return
                void act('enabled', () => m.saveSyncSetup({ 'sync.enabled': next }))
              }}
            />
            <span className='mm-muted'>
              {data.enabled
                ? t('定时同步按下面的节奏触发')
                : t('定时同步已停；手动「立即同步」仍然会跑')}
            </span>
          </div>

          <span>{t('自动定时同步')}</span>
          <div className='mm-sync-cell'>
            <Switch
              aria-label={t('自动定时同步')}
              checked={data.auto}
              /* 总开关关着时它没有意义（后端 `auto_ready()` 要求两者都为真），
                 但值仍然照实显示 —— 关掉总开关不该顺手把用户的自动同步偏好抹掉。 */
              disabled={!data.enabled || busy === 'auto'}
              onCheckedChange={next => {
                if (next === data.auto) return
                void act('auto', () => m.saveSyncSetup({ 'sync.auto': next }))
              }}
            />
            <span className={data.enabled && data.auto ? 'mm-sync-on' : 'mm-muted'}>
              {!data.enabled
                ? t('先打开上面的总开关')
                : data.auto
                  ? intervalText(data.intervalMin)
                  : t('只剩手动同步')}
            </span>
          </div>

          <span>{t('同步间隔')}</span>
          <div className='mm-sync-cell'>
            <Input
              type='number'
              className='mm-cfg-number'
              aria-label={t('同步间隔（分钟）')}
              min={5}
              max={1440}
              step={5}
              disabled={!data.auto}
              value={intervalDraft ?? String(data.intervalMin)}
              onChange={event => setIntervalDraft(event.target.value)}
              onBlur={commitInterval}
              onKeyDown={event => {
                // 回车 = 失焦：不逼用户去点别处才能提交一个数字。
                if (event.key === 'Enter') event.currentTarget.blur()
              }}
            />
            <span className='mm-muted'>{t('分钟（5 – 1440，改完点开别处即生效）')}</span>
          </div>
        </div>
      </section>
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
              <span className='mm-cfg-card-note'>{t('总开关已关')}</span>
            ) : null}
          </div>
        </div>

        <div className='mm-sync-grid'>
          <span>{t('服务器')}</span>
          <div className='mm-sync-cell'>
            <code>{data.url}</code>
            {/* 「修改凭据」和「测连接」都只跟服务器地址有关，所以都放在这一行，
                而不是散在页头 —— 用户要改/验的就是眼前这一个地址。
                「修改凭据」不再跳页签：凭据输入框就在本页最上面，点它滚过去即可。 */}
            <Button type='button' size='sm' variant='outline' onClick={gotoCredentials}>
              {t('修改凭据')}
            </Button>
            <Button
              type='button'
              size='sm'
              variant='ghost'
              /* 不再按 `configured` 禁用：地址有默认值之后，「先点一下测连接」是
                 一个有用的第一动作 —— 没填账号时它会如实回「认证失败（HTTP 401）」，
                 而那恰好证明「地址是对的，只缺账号密码」。灰掉这个按钮等于把
                 「验证地址」这件事从首屏拿走了。 */
              disabled={busy === 'test'}
              onClick={() =>
                void act('test', async () => {
                  const result = await m.testSync()
                  setTest({ ...result, from: 'status' })
                })
              }
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
            同步范围 + 本地目录。回答的是同一个问题「这页说的同步，具体是哪份数据」，
            挤在一行里就够 —— 分成两行会让这张卡凭空长高一截，而它本该是
            「五行数据 + 一句脚注」的形状。

            运行节奏**不在这里**：它已经由上面「同步行为」那张卡的开关与间隔输入框
            表达，同一件事在两处各写一遍，迟早出现一处改了另一处没改。
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
        </div>

        {test?.from === 'status' ? (
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
            {/* 这句原来写「一律等人裁决」。加了 l1 容器按节拼接之后它不再成立
                （同一份「某天的日志」两台机器各写几条时会自动拼平），所以改成
                「不自动**挑边**」—— 拼接不是挑边，它是两边都留下；真正需要选
                一边的情形仍然一律进队列。 */}
            <span className='mm-sync-info' title={t('两边都改且内容不同时进队列，等人裁决 —— 自动挑一边必然丢掉另一边的工作。同一天的日志能按节拼起来的会先拼平，拼不平的才进这里。')}>i</span>
          </div>
          <div className='mm-cfg-card-actions'>
            {/* 一行小字而不是徽标：零条是好消息，给它一个绿色圆角框会显得
                页面到处都在「报警/OK」。这里只想让人扫到「有没有要处理的」，
                有的话按下面会列出条目，标题行不必抢戏。
                两段之间用 · 分隔而不是靠 gap：它们在读的时候是一句话
                （「0 条待裁决，且不自动挑边」），不该被当成两栏。 */}
            <span className={pendingTotal > 0 ? 'mm-sync-flag-warn' : 'mm-cfg-card-note'}>
              {t('{n} 条待裁决', { n: pendingTotal })}
            </span>
            <span className='mm-cfg-card-note'>·</span>
            {/* 「不自动挑边」不是「不自动合并」：同一天的日志会按节拼平（两边
                的内容都留下），那是合并、但没挑边。原句写「一律不自动合并」，
                与新行为直接矛盾 —— 用户看到一次「拼接 1 条」就会发现这行在骗人。 */}
            <span className='mm-cfg-card-note'>{t('不自动挑边')}</span>
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
            {/* `detail` 是后端写入的原因（「本地已删、远端改了」这类）。它本身
                就是一句中文，所以按「中文即键」的规矩过一遍 `t()`：词典里有就
                翻译，没有就原样显示 —— 不包这一层，非中文界面上这段原因永远是
                中文，而它恰恰是用户判断「该留哪边」的唯一依据。 */}
            {row.detail ? <div className='mm-muted'>{t(row.detail)}</div> : null}
            <div className='mm-conflict-hashes'>
              {/* 三侧各是什么。`null` 哈希 = 这一侧没有这个文件（删了），
                  必须显示成「已删除」而不是「—」：用户要选的正是「删除还是改动
                  说了算」，把删除显示成空白等于把判断依据藏起来。 */}
              <span>{t('本地')} <code>{row.localHash?.slice(0, 12) ?? t('已删除')}</code></span>
              <span>{t('远端')} <code>{row.remoteHash?.slice(0, 12) ?? t('已删除')}</code></span>
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
                {t('忽略（这条不再同步）')}
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
            {/* 两种空态：没账号 = 「还没开始配」，有账号 = 「配好了但远端还没有包」。
                这里不能再说「未配置 WebDAV」—— 地址栏里明明白白填着地址，用户会
                觉得页面在自相矛盾。 */}
            {data.configured ? t('远端还没有任何包，下一次「立即同步」会创建第一个。') : t('还没填 WebDAV 账号，远端没有设备。')}
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
                  {/* 主名用人看得懂的显示名（`sync.deviceName`），设备 id 进 title：
                      「我的笔记本」是给人认的，`3f7a1c0e` 是用来对包的。
                      名字没读到过（这台设备还没被同步过）就退回设备 id，
                      而不是显示一片空白。
                      不把 id 也排在名字后面：这一列只有 120px（见
                      `page-memory.css` 的注释），两段挤在一行会把名字截成省略号，
                      而这一列唯一的用途就是「哪台设备」。 */}
                  <code title={item.name ? `${item.name} · ${item.device}` : item.device}>
                    {item.name || item.device}
                  </code>
                </span>
                <span
                  className={item.self ? 'mm-device-when self' : 'mm-device-when'}
                  title={item.lastSyncAt ? fullTime(item.lastSyncAt) : undefined}
                >
                  {/* 「多久以前」是这一列真正要回答的问题：它在读的时候是
                      「另一台还活着吗」，而不是「那是哪一天」。绝对时刻放
                      title 里，要精确定位时悬停即可。 */}
                  {item.lastSyncAt ? agoText(item.lastSyncAt) : '—'}
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