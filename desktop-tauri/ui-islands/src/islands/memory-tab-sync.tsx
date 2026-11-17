import * as React from 'react'
import { Button, Input, SegmentedControl } from '@ui'
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
 * ── 为什么「模块根目录」里四个模块写着未实现 ──────────────
 * 这五个目录是「同一个网盘里各模块各占一个」，这件事本身要显示出来，否则
 * 用户看不懂记忆同步到底占了远端的哪一格。但本仓**只有记忆实现了同步**
 * （见 `sync::MODULES`）。所以另外四个：输入框照给（值会被保存，将来实现了
 * 就直接生效）、按钮不给 —— 画一个点了没反应的「立即同步」比不画更糟。
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

/**
 * 「快速同步」里的行标题。
 *
 * 为什么不用后端下发的 `label`：那张卡的动词是「同步」，标题写「技能仓库」等于
 * 让人自己把名词翻译成动作；而上面「模块根目录」那张卡的动词是「配目录」，标题
 * 就该是数据源的名字（「技能仓库」）。同一个模块在两张卡里读法不同，所以名字
 * 也不同 —— 一处叫 `label`（源），一处叫 `action`（动作）。
 *
 * 缺键时回落到 `label`：后端将来加第六个模块，这里没跟上也不会渲染出一个空行。
 */
const QUICK_ACTIONS: Record<string, string> = {
  skills: '技能同步',
  dosage: '用量同步',
  pool: '号池管家 · 号池同步',
  memory: '记忆中枢同步',
  prompts: '提示词同步',
}

/**
 * 「快速同步」里的行序。
 *
 * 与「模块根目录」那张卡不同序，照抄目标界面：那张卡按「记忆中枢排第二列」的
 * 两列网格成对排（技能 | 用量、号池 | 记忆、提示词 | 预设），而这张卡是一列
 * 竖排，顺序按「用户平常先同步哪一份」——先把白拿的数据（用量、技能）列出来，
 * 再把要打包上传的（号池、记忆）放后面。两张卡的顺序不一样不是笔误。
 *
 * 未列出的模块按后端顺序追加在后面，这样后端加模块时这里不用改也不会漏行。
 */
const QUICK_ORDER = ['dosage', 'skills', 'pool', 'memory', 'prompts']

type SetupForm = {
  provider: string
  url: string
  username: string
  password: string
  confirmPhrase: string
  remoteDir: string
  deviceName: string
  preset: string
  modules: Record<string, string>
}

/**
 * 从状态拼出表单初值。
 *
 * 密码 / 口令恒为空串：后端从不下发原文，空串的语义是「沿用已保存的那份」
 * （`config::apply_one` 的 Secret 分支）。所以「用户没动密码框」与「密码本来是
 * 空的」在提交时是同一个值，区别只体现在占位符上（`hasPassword`）。
 */
function formOf(data: m.SyncStatus): SetupForm {
  return {
    provider: data.provider ?? 'webdav',
    url: data.url ?? '',
    username: data.username ?? '',
    password: '',
    confirmPhrase: '',
    remoteDir: data.remoteDir ?? '',
    deviceName: data.deviceName ?? '',
    preset: String(data.preset ?? 'custom'),
    modules: Object.fromEntries((data.modules ?? []).map(item => [item.id, item.dir])),
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

  const putModule = (id: string, dir: string) => {
    drafted.current = true
    setForm(current => (current ? { ...current, modules: { ...current.modules, [id]: dir } } : current))
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

  // ── 快速同步的行序（必须在提前返回之前算）──
  //
  // `QUICK_ORDER` 里的五个先按那个顺序，后端将来新增的模块（不在表里）按原顺序
  // 接在后面。
  //
  // 为什么这个 `useMemo` 抬到三个 `if (...) return` 上面：hooks 的调用顺序必须
  // 逐帧一致。它原来在返回语句之后，首帧 `loading === true` 提前返回、这一句根本
  // 没跑；数据回来那帧才第一次执行 —— React 数出「这一帧比上一帧多了个 hook」，
  // 抛 `Rendered more hooks than during the previous render` 并把整棵树卸载。
  // 表现就是点「同步」页签后整页空白（连页签栏一起没了，因为崩的是 `MemoryPage`
  // 的子树而不是这张卡）。所以它只能待在所有条件返回之前。
  const modules = data?.modules ?? []
  const quick = React.useMemo(() => {
    const rank = (id: string) => {
      const at = QUICK_ORDER.indexOf(id)
      return at < 0 ? QUICK_ORDER.length : at
    }
    return [...modules].sort((a, b) => rank(a.id) - rank(b.id))
  }, [modules])

  if (loading) return <Panel loading error=''>{null}</Panel>
  if (error) return <Panel loading={false} error={error} onRetry={reload}>{null}</Panel>
  if (!data) return <Empty>{t('读不到同步状态。')}</Empty>

  const devices = data.devices?.devices ?? []
  const pending = conflicts.data?.conflicts ?? []
  const summary = (lastRun?.result ?? data.lastResult ?? null) as LastSummary | null
  const running = busy === 'run'
  const badge = runBadge(data.configured, running, lastRun, data.lastSyncAt)
  const current = form ?? formOf(data)

  return (
    <div className='mm-sync'>
      {actionError ? <div className='mm-error'>{actionError}</div> : null}

      {/* ── 顶部：一句话说清这页在干什么 ──
          主操作不在这儿：截图里这张卡是纯说明条，真正的「立即同步」在下面的
          「快速同步」里（那里每个模块一个按钮，用户要按的是「哪一份数据」，
          而不是笼统的「同步」）。顶栏再放一个同名按钮会让人以为有两个入口。 */}
      <div className='mm-sync-bar'>
        <span className='mm-sync-bar-text'>
          {t('把记忆文件夹整体打包上传/下载，两边都改且不一样时进冲突队列等你裁决')}
        </span>
      </div>

      {/* ── WebDAV 服务器（凭据）──
          截图里这张卡自带账号 / 密码 / 口令三个框和三个按钮。这里照做，但
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
            <span className='mm-sync-field-label'>{t('服务器地址（WebDAV）')}</span>
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
            —— URL 的最后一段就是它，缺了它这张卡的三个输入框拼不出一个可用地址。
            下面「模块根目录」里记忆那一行只是它的只读投影（同一个值，一个入口）。 */}
        <label className='mm-sync-field'>
          <span className='mm-sync-field-label'>
            {t('远端目录')}
            <span className='mm-sync-field-aside'>
              {t('（记忆中枢在网盘上的根目录；与「模块根目录」里那一行是同一个值）')}
            </span>
          </span>
          <Input
            value={current.remoteDir}
            spellCheck={false}
            placeholder='agent2api-memory'
            onChange={event => put({ remoteDir: event.target.value })}
          />
        </label>

        <label className='mm-sync-field'>
          {/* 标签只写「密码」：目标界面那句「同时作为号池同步压缩包的默认加密
              口令」描述的是号池同步，而本仓没有号池同步 —— 把一句实现不了的话
              写在字段名里，等于承诺了一个不存在的行为。*/}
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

        <label className='mm-sync-field'>
          <span className='mm-sync-field-label'>
            {t('号池加密口令')}
            <span className='mm-sync-field-aside'>{t('（可选，留空沿用上面的密码；本仓尚未实现号池同步）')}</span>
          </span>
          <Input
            type='password'
            autoComplete='new-password'
            value={current.confirmPhrase}
            placeholder={
              data.hasConfirmPhrase
                ? '••••••••'
                : t('留空 = 沿用上面的密码')
            }
            title={t('单独给号池包设口令时填；留空 = 沿用 WebDAV 密码')}
            onChange={event => put({ confirmPhrase: event.target.value })}
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
                  'sync.confirmPhrase': current.confirmPhrase,
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
              {t('会清掉地址 / 账号 / 密码 / 目录 / 口令这些同步配置（远端包与本机已同步的数据不动）。确定？')}
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

      {/* ── 模块根目录 ──
          五个模块在同一个网盘里各占一格。记忆那行才是本仓真正会读的
          （`sync.remoteDir`），其余四行是「先占个位、将来接上就用」——
          所以它们的「立即同步」按钮在下面那张卡里是灰的。 */}
      <section className='mm-cfg-card'>
        <div className='mm-cfg-card-head'>
          <div className='mm-cfg-card-title'>{t('模块根目录')}</div>
          <span className='mm-cfg-card-note'>
            {t('五个模块在同一网盘上的隔离目录 · 改根目录 = 换一个全新数据位置（旧数据不搬）')}
          </span>
        </div>

        <div className='mm-sync-pair'>
          {modules.map(item => (
            <label key={item.id} className='mm-sync-field'>
              <span className='mm-sync-field-label'>
                {t(item.label)}
                {/* 记忆这一格**不写「（默认 …）」**：它真正的默认值是 `agent2api-memory`
                    （见 `sync::remote_dir`），而 `MODULES` 里那个 `/agenthub-memory`
                    只是「对齐别的模块长什么样」的参考名。写出来会让用户以为默认是后者。 */}
                {item.id === 'memory' ? null : (
                  <span className='mm-sync-field-aside'>
                    {t('（默认 {dir}）', { dir: item.def })}
                  </span>
                )}
                {item.implemented ? null : <span className='mm-sync-tag'>{t('未实现')}</span>}
              </span>
              <Input
                value={current.modules[item.id] ?? ''}
                spellCheck={false}
                placeholder={item.dir}
                /* 记忆这一行是只读的：它真正生效的值是 `sync.remoteDir`（在上一张
                   卡的「远端目录」里改）。两处都能写会变成「改了这里没生效」，
                   所以这里只显示、不可编辑，并把它指回真正的那一格。 */
                disabled={item.id === 'memory'}
                title={item.id === 'memory' ? t('这一项由上一张卡的「远端目录」决定，改了会同步到那里') : undefined}
                onChange={event => putModule(item.id, event.target.value)}
              />
            </label>
          ))}

          {/* 存储预设排在最后一个「模块目录」位置：它跟这五个目录一样，是
              「这台机器接的是谁家的网盘」这类元信息，属于同一次保存。 */}
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
        </div>

        <div className='mm-sync-inline mm-sync-inline-end'>
          <label className='mm-sync-field mm-sync-field-grow'>
            <span className='mm-sync-field-label'>{t('电脑名（用量统计在多设备列表中显示）')}</span>
            <Input
              value={current.deviceName}
              spellCheck={false}
              placeholder={data.device}
              onChange={event => put({ deviceName: event.target.value })}
            />
          </label>
          <Button
            type='button'
            size='sm'
            variant='outline'
            disabled={busy === 'dirs'}
            onClick={() =>
              void act('dirs', async () => {
                await m.saveSyncSetup({
                  'sync.deviceName': current.deviceName,
                  'sync.preset': current.preset,
                  ...Object.fromEntries(
                    modules
                      .filter(item => item.id !== 'memory')
                      .map(item => [item.key, current.modules[item.id] ?? ''])
                  ),
                })
                drafted.current = false
                setForm(null)
              })
            }
          >
            {busy === 'dirs' ? t('保存中…') : t('保存预设与电脑名')}
          </Button>
        </div>

        <div className='mm-sync-note'>
          {t('记忆中枢用的是「远端目录」那一项（')}
          <code>/{current.remoteDir || 'agent2api-memory'}</code>
          {t('），它在上一张卡里改；此处这一行只是把它显示出来对齐。其余四个模块的目录名会被保存，但本仓还没有它们的同步实现。')}
        </div>
      </section>

      {/* ── 快速同步 ──
          截图里这张卡是「每个模块一行 + 立即同步」，并且每行带「根目录 + 就绪状态」。
          这里照做，但只有记忆那行是真的：
            · 记忆：按钮可用，就绪状态直接读同步状态（有没有地址 / 有没有密码）；
            · 其余四行：目录名照显（那是用户填的），按钮 **disabled** 并标「未实现」。
          不做假按钮是这条界面的底线：点一下没反应比没有按钮更让人怀疑整个功能。

          页头不放「刷新状态」：每次动作（保存 / 测连接 / 同步）之后 `act()` 已经
          重载了状态，再给一个手动刷新按钮等于承认「显示的可能过期」—— 而它并不会。 */}
      <section className='mm-cfg-card'>
        <div className='mm-cfg-card-head'>
          <div className='mm-cfg-card-title'>{t('快速同步')}</div>
          <span className='mm-cfg-card-note'>
            {t('每个模块一个按钮：按「哪一份数据」同步，而不是笼统的一个同步键')}
          </span>
        </div>

        {quick.map(item => {
          // 记忆那行才需要「就绪」判断：地址与密码是它跑起来的前提。
          const ready = data.configured && data.hasPassword !== false
          const blocked = !data.configured
            ? t('还没填 WebDAV 地址')
            : t('还没填 WebDAV 密码')
          const enabled = item.implemented && ready
          return (
            <div key={item.id} className='mm-quick-row'>
              <div className='mm-quick-main'>
                <div className='mm-quick-title'>
                  {t(QUICK_ACTIONS[item.id] ?? item.label)}
                  <span
                    className='mm-sync-info'
                    title={
                      item.implemented
                        ? t('把 general / projects / profile 打包上传；远端有别人的包时做三方合并')
                        : t('这个模块的同步本仓还没实现：目录名会被保存，但按钮点了不会传任何东西 —— 所以它是灰的。')
                    }
                  >
                    i
                  </span>
                </div>
                {/* 号池那行多一句「传的是什么、怎么合并」：它是五个模块里唯一
                    涉及账号凭据的，光看「根目录」两个字根本不知道包里有密码。
                    其余四行不加：它们传的就是那个目录里的文件，一句话说不说都一样，
                    加了只会让这张卡变成五行注释。 */}
                {item.id === 'pool' ? (
                  <div className='mm-quick-desc'>
                    {t('号池账号（含凭据）加密打包同步到他机，按账号去重合并；本机删号会同步删他机账号')}
                  </div>
                ) : null}
                <div className='mm-quick-sub'>
                  {t('根目录')} <code>{current.modules[item.id] || item.dir || '—'}</code>
                </div>
                <div className={enabled ? 'mm-quick-ok' : 'mm-quick-off'}>
                  {item.implemented ? (ready ? t('已就绪') : blocked) : t('未实现')}
                  {item.implemented && data.lastSyncAt ? (
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
                /* 三态共用一个形状：可用 = 主色描边（`.mm-quick-run`），未实现 /
                   未就绪 = 普通描边按钮的禁用态。不给未实现的模块画「幽灵按钮」
                   （无描边的灰字），那样它看起来像一句说明而不是一个控件 ——
                   用户会去点它，然后怀疑整个页面。 */
                variant='outline'
                className={enabled ? 'mm-quick-run' : undefined}
                disabled={!enabled || running}
                title={item.implemented ? undefined : t('本仓尚未实现该模块的同步')}
                onClick={
                  item.implemented
                    ? () =>
                        void act('run', async () => {
                          const result = await m.runSync()
                          setLastRun(result.outcome)
                        })
                    : undefined
                }
              >
                {running && item.implemented ? t('同步中…') : t('立即同步')}
              </Button>
            </div>
          )
        })}
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
              <span className='mm-cfg-card-note'>{t('同步功能已关闭')}</span>
            ) : null}
          </div>
        </div>

        <div className='mm-sync-grid'>
          <span>{t('服务器')}</span>
          <div className='mm-sync-cell'>
            <code>{data.url || t('未填写 WebDAV 地址')}</code>
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
              disabled={busy === 'test' || !data.configured}
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