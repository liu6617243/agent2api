import * as React from 'react'
import { Badge, Button, Input, SegmentedControl, Switch } from '@ui'
import { t } from '../i18n'
import * as m from './memory-api'
import { Empty, Panel, Section, StateBadge, num, useLoad } from './memory-shared'
import { OutcomeLine } from './memory-tab-profile'

/**
 * 自动化页：12 个后台任务的开关、节奏、上次/下次执行、累计统计与执行记录。
 *
 * ── 为什么是卡片网格而不是表格 ──────────────────────────────
 * 表格的每一行只放得下一个短语（「每 30 分钟」），而这一页要回答的问题比短语
 * 复杂：它什么时候跑（节奏 + 上次 + 下次）、跑得稳不稳（次数 + 成功率）、
 * 贵不贵（token）。挤在同一行的七列里，用户得横着扫过七个列头才能读完一个
 * 任务；换成卡片之后每张卡自带这四组信息，视线只在 340px 宽度内上下移动。
 *
 * 三列是宽度与信息密度的折中：卡片再窄（四列）节奏面板里的「上次运行 /
 * 下次计划」就要换行，再宽（两列）则在 1440 的屏上一行放两张、每张 660px，
 * 大片空白反而看不出对比。
 *
 * ── 为什么默认只开零 token 的任务 ────────────────────────────
 * 花 token 的任务在后台悄悄烧钱，而用户装完这个功能时对「它会花多少」没有
 * 概念。所以有 token 的默认全关，需要用户自己点头。这张卡上的「调用模型 /
 * 零成本」徽标与底部 token 累计就是为这件事服务的。
 *
 * ── 为什么「立即执行」是同步等待 ────────────────────────────
 * 用户点了就想看到结果（「跑了什么、跳过没、为什么跳过」）。返回一个 taskId
 * 让他再刷新一次是多余的一步，而且刷新之后也未必看得出是刚跑的那次。
 *
 * ── 为什么开关与节奏走同一个端点 ────────────────────────────
 * `auto.tasks` 是三层嵌套 Map（任务 → 字段）。页面自己拼形状有两条错法：
 * 发扁平子键会被后端静默丢弃（开关刷新就弹回去），发深合并则删不掉旧节奏
 * 字段（改成「每隔 30 分钟」后仍然每天只跑一次）。两条都不报错，所以改成
 * 后端收口：页面只说「这个任务开关变成 X」「这个任务改成每天 11:00」。
 */

/** 执行结果 → 徽标。跳过着灰色而不是红色：它是常态，不是故障。 */
function StatusBadge({ status }: { status: TaskRunStatus }): React.ReactElement {
  const state = status === 'done' ? 'ok' : status === 'failed' ? 'bad' : 'idle'
  const label = status === 'done' ? t('完成') : status === 'failed' ? t('失败') : t('跳过')
  return <StateBadge state={state} label={label} />
}

type TaskRunStatus = m.TaskRun['status']

/**
 * 待确认裁决积压卡。
 *
 * ── 为什么把「待确认」从调度器状态里提出来单独一张卡 ─────────
 * 待确认积压**不是**调度器指标：调度器开得好好的、任务照常跑，积压也会涨。
 * 早先它只是状态行里一枚黄色小徽标，和「已开启 / 线程在跑」挤在同一排，
 * 读起来像又一次健康检查 —— 而它其实是这页唯一需要**人去干活**的东西。
 *
 * 提到独立卡片之后，卡片里只有两件事：有多少条等你裁决、去哪儿裁决。
 *
 * 超预算行为（`auto.overBudgetAction`）不在这张卡上：它属于「配置」，
 * 一年改一次，摆在日常页面上只会把这张卡的两个数字挤散。
 */
function VerdictCard({ pending, onGo }: { pending: number; onGo: () => void }): React.ReactElement {
  return (
    <div className='mm-verdict'>
      <div className='mm-verdict-head'>
        <span className='mm-verdict-icon' aria-hidden>
          {/* 方框里一个勾：与页面里「待确认」的语义一致（裁决 = 逐条过一遍）。
              用内联 SVG 而不是 wbIcons：图标字体在岛里要等 window.wbIcons 就位，
              这一枚是页面的主视觉，不能有「先空一下再出现」的时序。 */}
          <svg width='18' height='18' viewBox='0 0 24 24' fill='none' stroke='currentColor' strokeWidth='1.8' strokeLinecap='round' strokeLinejoin='round'>
            <rect x='3.5' y='3.5' width='17' height='17' rx='4' />
            <path d='M8 13.2l2.6 2.6L16 9.4' />
          </svg>
        </span>
        <div className='mm-verdict-title'>
          <div className='mm-verdict-name'>{t('待确认裁决建议')}</div>
          <div className='mm-verdict-sub'>{t('需人工判定的失效 / 归类 / 查重建议')}</div>
        </div>
      </div>

      <div className='mm-verdict-body'>
        <span className='mm-verdict-count'>
          <strong>{num(pending)}</strong>
          <span>{t('项待办')}</span>
        </span>
        <Button type='button' size='sm' variant='outline' onClick={onGo}>
          {t('进入裁决收件箱')}
          <span aria-hidden className='mm-verdict-arrow'>→</span>
        </Button>
      </div>
    </div>
  )
}

/** 节奏面板上那个循环箭头：示意「周期性」，与「改节奏」的铅笔区分开。 */
function CadenceIcon(): React.ReactElement {
  return (
    <svg
      width='12'
      height='12'
      viewBox='0 0 24 24'
      fill='none'
      stroke='currentColor'
      strokeWidth='2'
      strokeLinecap='round'
      strokeLinejoin='round'
      aria-hidden
      className='mm-auto-card-cadence-icon'
    >
      <path d='M20 11.5a8 8 0 1 0-2.3 6.1' />
      <path d='M20 4.5v7h-7' />
    </svg>
  )
}

function PencilIcon(): React.ReactElement {
  return (
    <svg width='11' height='11' viewBox='0 0 24 24' fill='none' stroke='currentColor' strokeWidth='2' strokeLinecap='round' strokeLinejoin='round' aria-hidden>
      <path d='M4 20h4L19.5 8.5a2.1 2.1 0 0 0-3-3L5 17v3z' />
    </svg>
  )
}

function PlayIcon(): React.ReactElement {
  return (
    <svg width='10' height='10' viewBox='0 0 24 24' fill='currentColor' aria-hidden>
      <path d='M8 5.5v13l10-6.5z' />
    </svg>
  )
}

/**
 * 成功率。只数「跑过几次成功几次」—— 跳过不算失败（没素材 / 预算用完 / 未配
 * 模型都是常态），把它算进分母会让一个健康的任务显示 60%。
 *
 * 因此分母是 `success` 与 `runs` 的差加上 `success`……不对：`runs` 含跳过，
 * 直接 `success / runs` 会把跳过算成失败。这里用后端给的 `runs`（含跳过）与
 * `success`，两者相减得到「非成功次数」，其中跳过的部分无法从这两个数里区分。
 *
 * 取舍：既然区分不出，就不假装精确 —— 页面显示的成功率是
 * `success / runs`，并在 title 里写明「含跳过」。用户真正要看的是「有没有
 * 大面积失败」，差几个百分点的口径不影响这个判断；而另开一次查询只为了让
 * 标签好看，代价不划算。
 */
function successRate(task: m.AutoTask): number | null {
  if (task.runs <= 0) return null
  return (task.success / task.runs) * 100
}

/** 百分比保留一位小数，但整数不带 `.0`（「100%」而不是「100.0%」）。 */
function percentText(value: number): string {
  const rounded = Math.round(value * 10) / 10
  return Number.isInteger(rounded) ? `${rounded}%` : `${rounded.toFixed(1)}%`
}

/** 平均每次运行的 token。0 次运行或零成本任务返回 null（不显示这一行）。 */
function avgTokens(task: m.AutoTask): number | null {
  if (!task.usesModel || task.runs <= 0 || task.tokens <= 0) return null
  return Math.round(task.tokens / task.runs)
}

type Draft = {
  task: string
  label: string
  mode: m.AutoCadence['mode']
  intervalMin: number
  daily: string
  weekly: number
  weeklyTime: string
}

const WEEKDAYS = [1, 2, 3, 4, 5, 6, 7]

const WEEKDAY_NAMES: Record<number, string> = {
  1: '周一',
  2: '周二',
  3: '周三',
  4: '周四',
  5: '周五',
  6: '周六',
  7: '周日',
}

/** 从后端节奏开一份编辑草稿。缺字段的用合理默认，绝不留空。 */
function draftOf(task: m.AutoTask, label: string): Draft {
  const cadence = task.cadence
  return {
    task: task.task,
    label,
    mode: cadence.mode,
    // 切到「每隔」时用的预填值：优先用当前间隔，否则 30 分钟（最常用的档）。
    intervalMin: cadence.mode === 'interval' ? cadence.intervalMin : 30,
    // 切到「每天」时的预填：保留当前时刻，否则 11:00（与默认配置里的时刻一致）。
    daily: cadence.mode === 'daily' ? cadence.daily : '11:00',
    weekly: cadence.mode === 'weekly' ? cadence.weekly : 1,
    weeklyTime: cadence.mode === 'weekly' ? cadence.weeklyTime : '11:00',
  }
}

function draftPatch(draft: Draft): m.AutoTaskPatch {
  const mode = draft.mode
  if (mode === 'daily') {
    return { cadence: { mode: 'daily', daily: draft.daily } }
  }
  if (mode === 'weekly') {
    return { cadence: { mode: 'weekly', weekly: draft.weekly, weeklyTime: draft.weeklyTime } }
  }
  return { cadence: { mode: 'interval', intervalMin: draft.intervalMin } }
}

/**
 * 改节奏弹窗。
 *
 * ── 为什么三种模式是分段控件而不是三个下拉 ──────────────────
 * 「每隔 / 每天 / 每周」是三选一的档位，用户改的往往是从「每隔 30 分钟」变成
 * 「每天 11:00」—— 换档是主要动作。分段控件把换档做成一次点击，下面的输入
 * 随之换形状（分钟数 / 时刻 / 周几+时刻），比三个并排的字段（永远只填一个、
 * 另两个要自己忽略）少一次认知负担。
 *
 * 用页内 `.mm-dialog-mask` 而不是组件库的 `Dialog`：这一页的遮罩与宽度已经
 * 在 page-memory.css 里有一套（浏览页编辑、项目页合并都用它），同一个页面里
 * 两套弹窗的老化速度不一样，混用会出现「这个弹窗比那个弹窗圆角大一档」。
 */
function CadenceDialog({
  draft,
  busy,
  error,
  onChange,
  onClose,
  onSave,
}: {
  draft: Draft
  busy: boolean
  error: string
  onChange: (next: Draft) => void
  onClose: () => void
  onSave: () => void
}): React.ReactElement {
  const modeOptions = [
    { value: 'interval' as const, label: t('每隔') },
    { value: 'daily' as const, label: t('每天') },
    { value: 'weekly' as const, label: t('每周') },
  ]

  return (
    <div className='mm-dialog-mask' role='dialog' aria-modal='true' aria-label={t('改调度节奏')}>
      <div className='mm-dialog mm-dialog-narrow'>
        <div className='mm-dialog-head'>
          <strong>{t('「{name}」的调度节奏', { name: draft.label })}</strong>
          <Button type='button' size='sm' variant='ghost' disabled={busy} onClick={onClose}>
            {t('取消')}
          </Button>
        </div>

        <div className='mm-dialog-body'>
          <SegmentedControl
            aria-label={t('节奏模式')}
            options={modeOptions}
            value={draft.mode}
            onValueChange={value => onChange({ ...draft, mode: value })}
          />

          {draft.mode === 'interval' ? (
            <>
              <label className='mm-label'>{t('间隔（分钟）')}</label>
              <Input
                type='number'
                min={1}
                max={43200}
                value={String(draft.intervalMin)}
                onChange={event => {
                  // 空串保留为 1 而不是 NaN：用户全选删除的那一瞬间不该让
                  // 受控输入框显示 `NaN`。
                  const parsed = Number.parseInt(event.target.value, 10)
                  onChange({ ...draft, intervalMin: Number.isFinite(parsed) ? parsed : 1 })
                }}
              />
              <p className='mm-hint'>
                {t('调度器每 60 秒扫一次，到点才跑；间隔越大越省额度。30 分钟、6 小时、12 小时是最常用的三档。')}
              </p>
            </>
          ) : null}

          {draft.mode === 'daily' ? (
            <>
              <label className='mm-label'>{t('每天几点跑')}</label>
              <Input
                type='time'
                value={draft.daily}
                onChange={event => onChange({ ...draft, daily: event.target.value })}
              />
              <p className='mm-hint'>{t('按本地时区的这个时刻跑，错过（关机 / 睡眠）会在下次启动后补跑一次。')}</p>
            </>
          ) : null}

          {draft.mode === 'weekly' ? (
            <>
              <label className='mm-label'>{t('每周哪一天')}</label>
              <select
                className='mm-select'
                aria-label={t('星期')}
                value={String(draft.weekly)}
                onChange={event => onChange({ ...draft, weekly: Number.parseInt(event.target.value, 10) || 1 })}
              >
                {WEEKDAYS.map(day => (
                  <option key={day} value={String(day)}>{t(WEEKDAY_NAMES[day])}</option>
                ))}
              </select>
              <label className='mm-label'>{t('几点跑')}</label>
              <Input
                type='time'
                value={draft.weeklyTime}
                onChange={event => onChange({ ...draft, weeklyTime: event.target.value })}
              />
              <p className='mm-hint'>{t('按本地时区，每周固定一天。适合画像、去重合并这类「一周看一次就够」的任务。')}</p>
            </>
          ) : null}

          {error ? <div className='mm-error'>{error}</div> : null}
        </div>

        <div className='mm-dialog-foot'>
          <Button type='button' variant='ghost' disabled={busy} onClick={onClose}>{t('取消')}</Button>
          <Button type='button' variant='default' disabled={busy} onClick={onSave}>
            {busy ? t('保存中…') : t('保存节奏')}
          </Button>
        </div>
      </div>
    </div>
  )
}

/**
 * 一张任务卡。
 *
 * 版式（自上而下）：
 *   标题 + 成本徽标 + 开关
 *   一句说明
 *   节奏面板（调度节奏 / 上次运行 / 下次计划 / 改节奏）
 *   统计行（运行次数 / 成功率 / 平均 token）+ 立即执行
 *
 * ── 为什么节奏面板是内嵌的一块而不是四行普通文字 ──────────────
 * 这四行讲的是**同一件事的两面**：什么时候跑、上次什么时候跑的、据此什么时候
 * 再跑。用一块比卡片底色再暗一档的面板框起来，视觉上就是「这台机器的节拍器」；
 * 摊成四行普通文字则会和上面那句说明混成一段。
 *
 * ── 为什么左侧有一条绿竖条 ──────────────────────────────────
 * 12 张卡里哪几张在跑，是这一页最需要一眼看到的分布。开关本身很小（24px），
 * 扫一列 12 个开关要逐个数；一条 3px 的竖条在卡片外缘，余光就能看出「开了几个」。
 */
function TaskCard({
  task,
  label,
  busy,
  onChangeEnabled,
  onRun,
  onEditCadence,
}: {
  task: m.AutoTask
  label: string
  busy: boolean
  onChangeEnabled: (value: boolean) => void
  onRun: () => void
  onEditCadence: () => void
}): React.ReactElement {
  const rate = successRate(task)
  const avg = avgTokens(task)
  const summary = task.summary.trim()

  return (
    <div className={task.enabled ? 'mm-auto-card is-on' : 'mm-auto-card'}>
      <div className='mm-auto-card-head'>
        <div className='mm-auto-card-title'>
          <span className='mm-auto-card-name'>{label}</span>
          {task.task === 'sync' ? <Badge variant='outline' shape='tag'>{t('WebDAV')}</Badge> : null}
          {task.usesModel ? (
            <Badge variant='info' shape='tag' title={t('开它会在后台调模型、消耗额度')}>
              {t('调用模型')}
            </Badge>
          ) : (
            <Badge variant='success' shape='tag' title={t('纯本地计算，不调模型')}>
              {t('零成本')}
            </Badge>
          )}
        </div>
        <Switch
          checked={task.enabled}
          size='sm'
          variant='success'
          aria-label={t('启用「{name}」', { name: label })}
          onCheckedChange={onChangeEnabled}
        />
      </div>

      {/* 空说明画 `—` 而不是留白：留白会让同一排里这张卡的节奏面板比邻卡高
          一行，「调度节奏」三行就不在同一条水平线上。占住两行高度即可，不必
          为凑字数编一句同义反复。 */}
      <div className={summary ? 'mm-auto-card-desc' : 'mm-auto-card-desc is-empty'}>
        {summary || '—'}
      </div>

      <div className='mm-auto-card-cadence'>
        <div className='mm-auto-card-cadence-row'>
          <span className='mm-auto-card-cadence-main'>
            <CadenceIcon />
            <span className='mm-auto-card-cadence-label'>{t('调度节奏：')}</span>
            <strong className='mm-auto-card-cadence-value'>{task.cadence.text}</strong>
          </span>
          <button
            type='button'
            className='mm-auto-card-cadence-edit'
            onClick={onEditCadence}
            aria-label={t('改「{name}」的节奏', { name: label })}
          >
            <PencilIcon />
            {t('改节奏')}
          </button>
        </div>

        <div className='mm-auto-card-cadence-row mm-auto-card-cadence-times'>
          <span className='mm-auto-card-time'>
            <span className='mm-auto-card-time-label'>{t('上次运行：')}</span>
            <span className='mm-auto-card-time-value'>{task.lastRunText ?? t('还没跑过')}</span>
          </span>
          <span className='mm-auto-card-time'>
            <span className='mm-auto-card-time-label'>{t('下次计划：')}</span>
            <span className='mm-auto-card-time-value is-next'>
              {/* 关掉的任务没有「下次」：调度器不会跑它。写 `—` 而不是留着
                  上次算出来的时刻 —— 那会让人以为它还开着。 */}
              {task.enabled ? task.nextRunText : '—'}
            </span>
          </span>
        </div>
      </div>

      <div className='mm-auto-card-foot'>
        <div className='mm-auto-card-stats'>
          {task.runs > 0 ? (
            <>
              <span className='mm-auto-card-runs'>{t('{n} 次运行', { n: num(task.runs) })}</span>
              {rate === null ? null : (
                <span
                  className='mm-auto-card-rate'
                  title={t('成功 {ok} 次 / 共 {runs} 次（跳过也计入分母）', {
                    ok: num(task.success),
                    runs: num(task.runs),
                  })}
                >
                  {t('成功率 {percent}', { percent: percentText(rate) })}
                </span>
              )}
              {task.usesModel && task.tokens > 0 ? (
                <span
                  className='mm-auto-card-tokens'
                  title={avg === null ? undefined : t('平均每次约 {n} token', { n: num(avg) })}
                >
                  {t('{n} tokens', { n: num(task.tokens) })}
                </span>
              ) : null}
            </>
          ) : (
            /* 0 次运行只写这一句：成功率和 token 都是 0，摆出来是三个恒定的
               零，读起来像这台机器没在记账。 */
            <span className='mm-auto-card-runs'>{t('0 次运行')}</span>
          )}
        </div>

        <Button
          type='button'
          size='sm'
          variant='outline'
          className='mm-auto-card-run'
          disabled={busy}
          onClick={onRun}
        >
          {busy ? t('执行中…') : (
            <>
              <PlayIcon />
              {t('立即执行')}
            </>
          )}
        </Button>
      </div>
    </div>
  )
}

function Auto({ onGo }: { onGo: (tab: 'browse') => void }): React.ReactElement {
  const { data: loaded, loading, error, reload } = useLoad(() => m.loadAuto(), [])
  /**
   * 「手上这份状态」。
   *
   * ── 为什么不用 `reload()` 刷新 ──────────────────────────────
   * `useLoad` 的 loading 是整页级的：`loading=true` 时这个组件直接返回一个
   * 转圈的 Panel。点一下开关就整页闪一次，再点一下再闪一次 —— 而开关恰恰是
   * 用户会连点的地方。所以写入成功后用**接口返回的整份 status** 就地替换，
   * 页面不闪；只有首次进入和用户主动「刷新」才走整体重取。
   */
  const [latest, setLatest] = React.useState<m.AutoStatus | null>(null)
  const [busy, setBusy] = React.useState('')
  const [actionError, setActionError] = React.useState('')
  const [outcome, setOutcome] = React.useState<{ task: string; outcome: m.Outcome } | null>(null)
  const [showAllRuns, setShowAllRuns] = React.useState(false)
  const [draft, setDraft] = React.useState<Draft | null>(null)
  const [savingDraft, setSavingDraft] = React.useState(false)
  const [draftError, setDraftError] = React.useState('')
  /** 开关乐观更新的本地覆盖：点下去立刻变，请求回来再以后端为准。 */
  const [toggles, setToggles] = React.useState<Record<string, boolean>>({})
  const allRuns = useLoad(() => (showAllRuns ? m.loadAutoRuns(200) : Promise.resolve(null)), [showAllRuns])

  const data = latest ?? loaded

  /** 重取一份状态但不进 loading：用于「立即执行」之后刷新统计与执行记录。 */
  const refreshQuietly = async () => {
    try {
      setLatest(await m.loadAuto())
    } catch (caught) {
      setActionError(caught instanceof Error ? caught.message : String(caught))
    }
  }

  const run = async (task: string) => {
    setBusy(task)
    setActionError('')
    setOutcome(null)
    try {
      const result = await m.runAutoTask(task)
      setOutcome(result)
      await refreshQuietly()
      if (showAllRuns) allRuns.reload()
    } catch (caught) {
      setActionError(caught instanceof Error ? caught.message : String(caught))
    } finally {
      setBusy('')
    }
  }

  /**
   * 改开关。
   *
   * ── 为什么先乐观更新再回头对齐 ──────────────────────────────
   * 开关是「指哪打哪」的控件：点下去到请求回来之间（本地网关通常 <50ms，
   * 但磁盘慢的时候能到几百毫秒）如果不动，用户会再点一次 —— 于是两次相反的
   * 写入竞争，最终状态取决于谁后到。先本地翻转，视觉上就是即时生效；
   * 失败时把覆盖撤掉，开关自己弹回真实状态。
   */
  const changeEnabled = async (task: m.AutoTask, label: string, value: boolean) => {
    setActionError('')
    setToggles(current => ({ ...current, [task.task]: value }))
    try {
      setLatest(await m.setAutoTask(task.task, { enabled: value }))
      setToggles(current => {
        const next = { ...current }
        delete next[task.task]
        return next
      })
    } catch (caught) {
      setActionError(
        t('「{name}」的开关没写进去：{error}', {
          name: label,
          error: caught instanceof Error ? caught.message : String(caught),
        }),
      )
      // 撤掉乐观覆盖：`delete` 而不是置回旧值 —— 下一次渲染直接读后端那份，
      // 不会在「后端已经变了」的情况下留一个过期的本地值。
      setToggles(current => {
        const next = { ...current }
        delete next[task.task]
        return next
      })
    }
  }

  const saveDraft = async () => {
    if (!draft) return
    setSavingDraft(true)
    setDraftError('')
    try {
      setLatest(await m.setAutoTask(draft.task, draftPatch(draft)))
      setDraft(null)
    } catch (caught) {
      setDraftError(caught instanceof Error ? caught.message : String(caught))
    } finally {
      setSavingDraft(false)
    }
  }

  // Esc 关弹窗：页内弹窗没有 Base UI 那层焦点管理，键盘退回要自己接。
  React.useEffect(() => {
    if (!draft) return undefined
    const onKeyDown = (event: KeyboardEvent) => {
      if (event.key === 'Escape' && !savingDraft) setDraft(null)
    }
    window.addEventListener('keydown', onKeyDown)
    return () => window.removeEventListener('keydown', onKeyDown)
  }, [draft, savingDraft])

  if (loading) return <Panel loading error=''>{null}</Panel>
  if (error) return <Panel loading={false} error={error} onRetry={reload}>{null}</Panel>
  if (!data) return <Empty>{t('读不到调度器状态。')}</Empty>

  const labelOf = (task: string) => data.labels.find(item => item.task === task)?.label ?? task
  const runs = showAllRuns ? allRuns.data?.items ?? data.recentRuns : data.recentRuns
  // 「查看全部 N 条」用**行数**（翻的是行）；执行次数在任务卡的统计里。
  const runRows = data.runRows
  const enabledCount = data.tasks.filter(task => toggles[task.task] ?? task.enabled).length

  return (
    <div className='mm-auto'>
      {actionError ? <div className='mm-error'>{actionError}</div> : null}

      {/* 调度器状态 + 预算进度已按截图并入裁决卡；这里不再重复一遍「已开启 /
          线程在跑 / 上次 tick」—— 那三项在仪表盘有，两处都摆在页面最上面
          只会让真正要动手的事（裁决积压）被挤到第二眼。 */}
      <VerdictCard pending={data.pendingReview} onGo={() => onGo('browse')} />

      {data.privacyPaused ? (
        <div className='mm-scan-note'>{t('隐私暂停：全部自动化任务停跑。去「配置 → 自动化」解除。')}</div>
      ) : null}
      {data.budgetExhausted ? (
        <div className='mm-scan-note'>
          {t('今日预算已用完，任务按「{mode}」处理（auto.overBudgetAction，在「配置 → 自动化」里改）。', {
            mode: data.overBudgetAction === 'ignore' ? t('不限额继续') : t('暂停任务'),
          })}
        </div>
      ) : null}

      <Section
        title={t('任务')}
        hint={t('零 token 的默认开，花 token 的默认关 —— 它们会在后台消耗你的额度。')}
        action={<span className='mm-auto-count'>{t('{on} / {all} 已开启', { on: enabledCount, all: data.tasks.length })}</span>}
      >
        <div className='mm-auto-grid'>
          {data.tasks.map(task => (
            <TaskCard
              key={task.task}
              task={{ ...task, enabled: toggles[task.task] ?? task.enabled }}
              label={labelOf(task.task)}
              busy={busy === task.task}
              onChangeEnabled={value => void changeEnabled(task, labelOf(task.task), value)}
              onRun={() => void run(task.task)}
              onEditCadence={() => {
                setDraftError('')
                setDraft(draftOf(task, labelOf(task.task)))
              }}
            />
          ))}
        </div>
        {outcome ? (
          <div className='mm-outcome-box'>
            <div className='mm-section-title'>{t('「{task}」的结果', { task: labelOf(outcome.task) })}</div>
            <OutcomeLine outcome={outcome.outcome} />
          </div>
        ) : null}
      </Section>

      <Section
        title={t('执行记录')}
        hint={t('每次任务跑完（含跳过与失败）都落一行。想知道「昨天 11:00 它到底动了什么」看这里。')}
        action={
          <div className='mm-inline-actions'>
            {runRows > data.recentRuns.length ? (
              <Button type='button' size='sm' variant='ghost' onClick={() => setShowAllRuns(value => !value)}>
                {showAllRuns ? t('只看最近 {n} 条', { n: data.recentRuns.length }) : t('查看全部 {n} 条', { n: num(runRows) })}
              </Button>
            ) : null}
            <Button type='button' size='sm' variant='ghost' onClick={() => { void refreshQuietly(); if (showAllRuns) allRuns.reload() }}>
              {t('刷新')}
            </Button>
          </div>
        }
      >
        {runs.length === 0 ? (
          <Empty>{t('还没有执行记录。任务跑过一次之后这里就会有行 —— 自动轮次与「立即执行」都算。')}</Empty>
        ) : (
          <div className='mm-run-table'>
            <div className='mm-run-row mm-run-head'>
              <span>{t('时间')}</span>
              <span>{t('任务')}</span>
              <span>{t('结果')}</span>
              <span>{t('明细')}</span>
              <span>{t('token')}</span>
              <span>{t('耗时')}</span>
            </div>
            {runs.map((row, index) => (
              <div key={`${row.at}-${row.task}-${index}`} className='mm-run-row'>
                <span className='mm-muted' title={row.manual ? t('用户点「立即执行」触发') : t('自动轮次触发')}>
                  {row.atText}
                  {row.repeat > 1 ? <span className='mm-run-manual' title={t('连续 {n} 次相同结果，合并成一行', { n: row.repeat })}>{t('×{n}', { n: row.repeat })}</span> : null}
                  {row.manual ? <span className='mm-run-manual'>{t('手动')}</span> : null}
                </span>
                <span>{labelOf(row.task)}</span>
                <span><StatusBadge status={row.status} /></span>
                {/* 明细是后端 `detail_of` 生成的一句人话，原样显示：
                    失败/跳过的原因已经在里面，不再拼前缀。 */}
                <span className='mm-muted mm-run-detail' title={row.detail}>{row.detail || '—'}</span>
                <span className='mm-mono'>{row.tokens > 0 ? num(row.tokens) : '—'}</span>
                <span className='mm-mono'>{row.latencyMs} ms</span>
              </div>
            ))}
          </div>
        )}
      </Section>

      {draft ? (
        <CadenceDialog
          draft={draft}
          busy={savingDraft}
          error={draftError}
          onChange={setDraft}
          onClose={() => { if (!savingDraft) setDraft(null) }}
          onSave={() => void saveDraft()}
        />
      ) : null}
    </div>
  )
}

export { Auto }