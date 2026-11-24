import * as React from 'react'
import { Badge, Button, MetricCard, MetricGrid, MetricLabel, MetricValue } from '@ui'
import { t } from '../i18n'
import * as m from './memory-api'
import { Empty, Panel, Section, StateBadge, num, timeText, useLoad } from './memory-shared'

/**
 * 仪表盘：一眼看全「记忆库现在什么状态」。
 *
 * ── 这一页要回答的四个问题 ──────────────────────────────────
 *   1. 有多少条？流水与沉淀各多少（l2 太少说明从没蒸馏过）；
 *   2. 今天有没有新东西进来？（数字不动 = 没人在用，或 Agent 没接上）
 *   3. 有没有积压的？队列里剩的是不是「你关掉的那几类」？
 *   4. 同步与索引有没有异常？
 *
 * ── 为什么没有「沉淀占比」这一格（2026-11-23）──────────────
 * 原来有一张 `l2 / trusted` 的百分比卡。它的问题不在算法（分子分母都已是可信
 * 口径），而在**分母里混着画像**：画像一次生成十几条、全部落 l2，于是每点一次
 * 「重生成画像」，沉淀占比就凭空跳几个点，而沉淀本身一条没多。用户看到的
 * 「沉淀」和他理解的「反复确认后固化下来的结论」不是一回事。
 *
 * 现在后端把画像从 `l1` / `l2` 里摘出来单列（`status.profile`），仪表盘也就不再
 * 需要那个百分比 —— 两个绝对数字并排比比例更直接，也不会被派生品带偏。
 *
 * ── 为什么把「待确认」放这么显眼 ────────────────────────────
 * 队列正常情况应该是空的：每一类建议都有判据，`auto_resolve` 扫一遍就清干净。
 * 所以这个数字一旦不是 0，含义就变了 —— 它不是「有活要干」，而是「有一类的
 * 自动裁决被关掉了，或者落地在磁盘上失败了」。这两种都需要人看一眼，而且用户
 * 完全不会主动去查。所以它必须在第一屏。
 *
 * ── 为什么没有「最近 7 天热力图」───────────────────────────
 * 后端目前没有按天聚合的接口（`llm_call` / `recall_log` 有原始行但没有日聚合）。
 * 画一个假的热力图不如不画 —— 这里改成「今日 token 消耗」这一个真实数字。
 * 要热力图就得先加一个按天 GROUP BY 的端点，那是另一件事。
 */

type DashboardProps = {
  status: m.MemoryStatus | null
  onGo: (tab: 'browse' | 'profile' | 'auto' | 'sync' | 'agents') => void
  /**
   * 「去裁决」的专用出口：跳浏览页**并直接落在「待确认」那一段**。
   *
   * 以前这个按钮只调 `onGo('browse')`，用户落到的是「共 N 条」的普通列表 ——
   * 待确认是四段控件里的第三段，得自己发现并点进去。真机反馈就是
   * 「显示 41 条待确认，但不知道去哪处理」。跳页签必须带视图。
   */
  onReview?: () => void
  /** 打开某条记忆（过时清单里的「打开」用它跳到浏览页详情抽屉）。 */
  onOpenEntry?: (id: string) => void
  /** 重建索引会改条数，重建完让外层刷新一次 status。 */
  onChanged?: () => void
}

function Dashboard({ status, onGo, onReview, onOpenEntry, onChanged }: DashboardProps): React.ReactElement {
  // 这一页三份数据都显式开启「重新进入就重拉」：状态页是全套里唯一需要「一眼看全
  // 现在」的一页，切回来还显示几分钟前的数字等于这一页白设。
  const auto = useLoad(() => m.loadAuto(), [], { refreshOnEnter: true })
  const sync = useLoad(() => m.loadSync(), [], { refreshOnEnter: true })
  const [rebuilding, setRebuilding] = React.useState(false)
  const [actionError, setActionError] = React.useState('')

  /**
   * 重建索引：原本在「索引」页的主按钮。那一页删掉之后它是唯一没有别处替代的
   * 动作（检索、回收站都各有归宿），所以挪到这里，并且只在「索引漂移」时出现
   * —— 平时摆着一个「重建」按钮只会让人想去点它。
   *
   * 重建完只需要 `onChanged()` 重拉 `status`：`drift` 现在读的就是
   * `status.index.inSync`（早先读 `health`，那个端点已删）。
   */
  const rebuild = async () => {
    setRebuilding(true)
    setActionError('')
    try {
      await m.rebuildIndex()
      onChanged?.()
    } catch (caught) {
      setActionError(caught instanceof Error ? caught.message : String(caught))
    } finally {
      setRebuilding(false)
    }
  }

  if (!status) {
    return <Panel loading error=''>{null}</Panel>
  }

  // `candidates` 是本轮新加的字段（旧后端 + 新界面会读到 undefined）。
  // 草稿既不进检索也不进注入，把它算进「有效记忆」只会让那个数字虚高 ——
  // 用户以为能搜到，实际一条也搜不到。
  const drafts = status.candidates ?? 0
  // `refuted` 是「被判错但仍活着」的条数，仍算在 `live` 里。它和草稿一样
  // 不进检索 / 注入 / 核心记忆。
  const refuted = status.refuted ?? 0
  // 分母直接用后端给的 `trusted`（活着 + 没被反驳 + 不是草稿），**不在这里相减**：
  // `candidates` 与 `refuted` 两桶有交集（一条被判错的草稿同时属于两边），
  // 前端做 `live - drafts - refuted` 会把那条扣两次，数字少一条且没人看得出。
  // 老后端没有这个字段时回落到旧算式，保证升级过渡期页面不崩。
  const trusted = status.trusted ?? Math.max(0, status.live - drafts - refuted)
  // 画像条数。它**不在** `l1` / `l2` 之内（后端已单列，见 `status()` 的说明），
  // 所以下面「普通记忆 / 深层记忆」两个数字都不含它 —— 这正是要的效果：
  // 重生成一次画像，那两个数字不该动。
  const profiles = status.profile ?? 0
  const pending = status.review?.pending ?? 0
  /** 积压阈值（配置项 `dedup.pendingWarnThreshold`，后端默认 50）。
   *
   *  它现在的职责只有「描述」：超过它才在 detail 里写「已超过提醒阈值」。**不再决定
   *  这一行亮不亮** —— 旧逻辑用它当开关，41 条 < 50 就显示「正常」，而 detail 同时
   *  写着「41 条建议等待裁决」，一行里两个结论互相打脸。 */
  const warnThreshold = status.review?.warnThreshold ?? 50
  const backlogged = pending >= warnThreshold
  const conflicts = sync.data?.conflicts ?? 0
  // 索引漂移只从 `status.index` 取。这里早先读的是 `health.data.index.inSync`
  // —— 那个端点 2026-11-24 删了（它唯一的增量价值「过时记忆」从来没报出过东西），
  // 而 `status()` 本来就报同一口径的 `index.inSync`（含同样处理「读不到给 null」
  // 的语义），留下两份等于同一屏两个面板读同一份数、还可能取数时刻不同而打架。
  const drift = status.index ? status.index.inSync === false : false
  // `today` / `yesterday` / `agents` 是本轮新加的字段。桌面端前端 bundle 与后端
  // 二进制不一定同批更新（旧后端 + 新界面会读到 undefined），缺字段时退成 0/0
  // 而不是让整页白屏 —— 这张卡坏了不该带走整个仪表盘。
  const today = status.today ?? 0
  const yesterday = status.yesterday ?? 0
  const agentCalled = status.agents?.called ?? 0
  const agentInjected = status.agents?.injected ?? 0

  return (
    <div className='mm-dashboard'>
      <MetricGrid>
        <MetricCard>
          <MetricLabel>{t('有效记忆')}</MetricLabel>
          <MetricValue>{num(trusted)}</MetricValue>
          {/* 分层用「两个大数字 + 各自标签」并排，不用「沉淀 X · 流水 Y」一行：
              后者把两个不同的量塞进一句里，读的时候要先在心里拆开才知道谁是谁；
              并排之后左边永远是普通、右边永远是深层，扫一眼就能比。
              数字大、单位小、标签在下一行 —— 与 AgentHub 仪表盘同一读法。

              这里刻意不用 `num()`：它带千分位（`24,286`），副行本来就是小字号，
              多出来的逗号会把两位数字挤得更长、也更吵。目标界面的副行是裸数字。 */}
          <div className='mm-metric-pair'>
            <div>
              <span className='mm-mp-value'>
                {status.l1 - (status.candidates ?? 0)}<i className='mm-mp-unit'>{t('条')}</i>
              </span>
              <span className='mm-mp-label'>{t('普通记忆')}</span>
            </div>
            <div>
              <span className='mm-mp-value'>
                {status.l2}<i className='mm-mp-unit'>{t('条')}</i>
              </span>
              <span className='mm-mp-label'>{t('深层记忆')}</span>
            </div>
          </div>
          {/* 画像这一行不是注释，是**加数**：`有效记忆` 含画像（`trusted` 不排它），
              而上面两个数字都排掉了画像（见后端 `status()`）。所以
              `普通 + 深层` 永远比总数少正好「画像」那么多 —— 真机 269 + 338 = 607，
              总数 623，差的 16 就是画像。不写这一行，用户看到的是三个对不上的数，
              只能自己猜「少掉的 16 条去哪了」。
              文案压到最短（一行放得下 178px 的卡），「由深层记忆提炼、不算沉淀」
              那套解释挪进 title —— 悬停才看，不占版面。 */}
          {profiles > 0 ? (
            <div
              className='mm-metric-sub'
              title={t('画像：由深层记忆提炼的派生条目，不算沉淀')}
            >
              {t('另含画像 {n} 条', { n: profiles })}
            </div>
          ) : null}
          {/* 草稿（`candidates`）刻意**不**在这里出现：
              ① 它不在 `有效记忆` 里（`trusted` 已排掉草稿，`普通记忆` 也已减掉它），
                 所以它不是加数，写出来不解释任何数字；
              ② 它没有出口 —— 这一格是只读文字，点不进「浏览 → 列表」里那几条带
                 「草稿」标签的行，「去采纳」得先自己翻页找。一个既不对账、也
                 点不动的数字，就是纯噪音。
              草稿仍在两处可见：浏览页列表里的「草稿」标签（可「采纳为结论」），
              以及列表详情里的 `layerCaveat` 说明。 */}
          {/* 被判错的条数：0 时隐藏，避免给大多数库添一行噪音。 */}
          {refuted > 0 ? (
            <div className='mm-metric-sub'>
              {t('已纠错 {n} 条（不进检索与注入）', { n: refuted })}
            </div>
          ) : null}
        </MetricCard>
        <MetricCard>
          <MetricLabel>{t('待确认')}</MetricLabel>
          <MetricValue className={pending ? 'mm-warn-text' : undefined}>{num(pending)}</MetricValue>
          <div className='mm-metric-sub'>
            {pending ? t('判据没能自己收尾（多为某类自动裁决被关）') : t('判据已把该做的都做完')}
          </div>
        </MetricCard>
        <MetricCard>
          <MetricLabel>{t('已连通 Agent')}</MetricLabel>
          <MetricValue>{agentCalled}/{agentInjected}</MetricValue>
          <div className='mm-metric-sub'>{t('真实调用过 / 已配置')}</div>
        </MetricCard>
        <MetricCard>
          <MetricLabel>{t('今日新增')}</MetricLabel>
          <MetricValue>{num(today)}</MetricValue>
          <div className='mm-metric-sub'>{t('昨日 {n}', { n: num(yesterday) })}</div>
        </MetricCard>
        <MetricCard>
          <MetricLabel>{t('回收站')}</MetricLabel>
          <MetricValue>{num(status.deleted)}</MetricValue>
          <div className='mm-metric-sub'>{t('已取代 {n} 条', { n: num(status.superseded) })}</div>
        </MetricCard>
        {/* 「注入」这一格回答的是「记忆到底有没有被用上」。
            只看 MCP 的调用次数是不够的 —— 模型很少主动查（实测装了好几天，
            真检索 0 次）。请求级注入是网关替它塞的，所以这一格涨了才说明
            记忆真的进了对话。旧后端没有这个字段，缺了就按 0 渲染。

            副标题要能区分「没命中」和「没跑」：只有 count 时，0 有两种含义 ——
            机制正常但没东西可注，或者注入根本没被调用（代码没接上）。两种处理
            方式完全相反，所以用 attempts 一起判。 */}
        <MetricCard>
          <MetricLabel>{t('请求级注入')}</MetricLabel>
          <MetricValue>{num(status.inject?.count ?? 0)}</MetricValue>
          <div className='mm-metric-sub'>
            {status.inject?.enabled === false
              ? t('开关已关')
              : (status.inject?.attempts ?? 0) === 0
                ? t('还没有请求走过注入')
                /* `missed` 是「跑过但没命中」。真机上出现过一次假性 missed：
                   attempts 一路涨、count 恒为 0，根因是判重逻辑把会话里的
                   MARKER 字面量当成「已注入」直接跳过。修完之后这种形态会
                   重新变成真实的「没命中」，所以数字本身没有特别标注。 */
                : t('平均 {n} token / 次 · 未命中 {m} 次{trace}', {
                    n: num(status.inject?.avgTokens ?? 0),
                    m: num(status.inject?.missed ?? 0),
                    /* 「已留痕」= 注入顺手写下的检索台账行数。
                       没有它只能证明「注入发生过」，证明不了「哪几条记忆被
                       用了」—— 注入是自动读记忆的入口，不留痕的话使用侧
                       整条链就没有闭环证据。旧后端没有这个字段，那时显示
                       一个假的 0 反而会误导，所以整段省略。 */
                    trace: status.inject?.traced === undefined
                      ? ''
                      : t(' · 已留痕 {t} 次', { t: num(status.inject.traced) }),
                  })}
          </div>
        </MetricCard>
        {/* 「留存」这一格与上一格成对：上一格证明「记忆读进去了」，这一格证明
            「这一轮写回来了」。两边都只是本次启动以来的计数。

            副标题必须写出 skipped：只有 count 时，0 会被读成「没在工作」，
            而实际上「跳过」才是常态（寒暄 / 太短 / 重复都不写）。 */}
        <MetricCard>
          <MetricLabel>{t('请求后留存')}</MetricLabel>
          <MetricValue>{num(status.retain?.count ?? 0)}</MetricValue>
          <div className='mm-metric-sub'>
            {status.retain?.enabled === false
              ? t('开关已关')
              : t('已跳过 {n} 轮：工具轮 {tool} · 太短 {short} · 未过闸 {gate} · 重复 {dup}', {
                  n: num(status.retain?.skipped ?? 0),
                  tool: num(status.retain?.skippedBy?.toolTurn ?? 0),
                  short: num(status.retain?.skippedBy?.tooShort ?? 0),
                  gate: num(status.retain?.skippedBy?.gate ?? 0),
                  dup: num(status.retain?.skippedBy?.duplicate ?? 0),
                })}
          </div>
        </MetricCard>
      </MetricGrid>

      <Section
        title={t('需要处理')}
        hint={t('这四类问题都不会自己消失，也不会阻塞 Agent —— 但放着会越来越难查。')}
      >
        {actionError ? <div className='mm-error'>{actionError}</div> : null}
        <div className='mm-todo-list'>
          <TodoRow
            /* 只要队列里有东西等人点头，这一行就不该是「正常」。
               以前判据是 `pending >= warnThreshold`（真机阈值 50），41 条积压
               照样亮绿「正常」，detail 却写着「41 条建议等待裁决」—— 同一行里
               自相矛盾，用户当然会问「有 41 条你说正常？」。
               阈值现在只用来区分「有积压」和「积压严重」，不再决定要不要提醒。 */
            state={pending ? 'warn' : 'ok'}
            label={t('待确认')}
            title={t('待确认队列')}
            detail={pending
              ? t('{n} 条还挂着（去重 / 归类 / 失效判定）—— 每类都有判据，正常应该清空，剩下的多半是某类自动裁决被关掉或落地失败 · 在「浏览 → 待确认」看{crowded}', {
                  n: pending,
                  crowded: backlogged ? t(' · 已超过提醒阈值 {n}', { n: warnThreshold }) : '',
                })
              : t('队列是空的（判据把该做的都做完了）')}
            action={pending
              ? { label: t('去裁决'), run: () => (onReview ? onReview() : onGo('browse')) }
              : undefined}
          />
          {/* 这一行早先是「可能过时的记忆」（`health.stale`）。删掉的原因不是
              「不好看」，是那个信号从来没成立过：真机 73 条带 `files` 的条目、
              482 个 files 项，`stat` 成功 0 次 —— `files` 存的是仓库相对路径，
              检查用的是记忆根基准。按正确基准复跑，66 条能解析、66 条全报「过时」，
              100% 误报。用户看到的永远是「没有发现」，等于一个恒绿的假指示灯。
              取舍细节见 `memory/mod.rs` 里 `TASK_LABELS` 上方那段。 */}
          <TodoRow
            state={conflicts > 0 ? 'bad' : 'ok'}
            title={t('同步冲突')}
            detail={conflicts ? t('{n} 个文件两边都改过，需要选一边', { n: conflicts }) : t('没有冲突')}
            action={conflicts > 0 ? { label: t('去裁决'), run: () => onGo('sync') } : undefined}
          />
          <TodoRow
            state={drift ? 'warn' : 'ok'}
            title={t('索引一致性')}
            detail={drift
              ? t('索引条数与事实源不一致（可重建，不影响数据）')
              : t('索引与事实源一致')}
            action={drift
              ? { label: rebuilding ? t('重建中…') : t('重建索引'), run: () => void rebuild() }
              : undefined}
          />
        </div>

        </Section>

      <div className='mm-two-col'>
        <Section title={t('自动化')} hint={t('后台任务默认只开零 token 的那几个。')}>
          {/* `loading` 只在「一条数据都还没到」时为真（口径见 memory-shared.tsx 的
              `useLoad`）：重新进入页面的静默重拉不会把这张卡闪成骨架。 */}
          {auto.loading ? <Panel loading error=''>{null}</Panel> : auto.data ? (
            <>
              <div className='mm-inline-meta'>
                <StateBadge
                  state={auto.data.globalEnabled ? (auto.data.running ? 'ok' : 'warn') : 'idle'}
                  label={auto.data.globalEnabled
                    ? (auto.data.running ? t('调度器运行中') : t('已开启但未在跑'))
                    : t('已关闭')}
                />
                {auto.data.privacyPaused ? <Badge variant='warning' shape='tag'>{t('隐私暂停')}</Badge> : null}
                {/* 这枚徽标只在**用户自己填了上限**之后才可能出现
                    （`auto.dailyTokenLimit` 默认 0 = 不限，`budgetExhausted` 恒为 false）。
                    它说的是自动化任务自己的日 token 用量，不是上游账号余额 ——
                    只印「已用完」，第一反应是「是不是该充值了」，所以连用量一起印。
                    改哪里在「配置 → 自动化」；本卡下面的「查看全部任务」通到那一页。 */}
                {auto.data.budgetExhausted ? (
                  <Badge variant='destructive' shape='tag'>
                    {t('今日预算已用完')} {num(auto.data.tokensToday)} / {num(auto.data.dailyTokenLimit)}
                  </Badge>
                ) : null}
              </div>
              <ul className='mm-plain-list'>
                {auto.data.tasks.slice(0, 6).map(task => {
                  const label = auto.data?.labels.find(item => item.task === task.task)?.label ?? task.task
                  return (
                    <li key={task.task}>
                      <span>{label}</span>
                      <span className='mm-muted'>
                        {task.enabled
                          ? (task.lastRunText ? t('上次 {when}', { when: task.lastRunText }) : t('还没跑过'))
                          : t('已关闭')}
                      </span>
                    </li>
                  )
                })}
              </ul>
              <Button type='button' size='sm' variant='ghost' onClick={() => onGo('auto')}>
                {t('查看全部 {n} 个任务', { n: auto.data.tasks.length })}
              </Button>
            </>
          ) : <Empty>{auto.error || t('读不到调度器状态')}</Empty>}
        </Section>

        <Section title={t('同步与接入')} hint={t('记忆跟着 WebDAV 走，Agent 通过 MCP 读它。')}>
          <div className='mm-status-grid mm-status-compact'>
            <span>{t('同步')}</span>
            <code>
              {sync.data
                ? sync.data.configured
                  ? (sync.data.lastSyncAt ? t('上次 {when}', { when: timeText(sync.data.lastSyncAt) }) : t('已配置，还没同步过'))
                  /* `configured` 的判据是账号非空（地址有默认值）。所以这句话
                     不能说「未配置 WebDAV」—— 地址一直是有的，缺的是账号。
                     在仪表盘这一格里，用户需要知道的是「还差哪一步」。 */
                  : t('待填 WebDAV 账号')
                : '—'}
            </code>
            <span>{t('设备')}</span>
            {/* 顺序：显示名 → 系统电脑名 → 设备 id。
                `deviceName` 没填过时用 `hostName`（系统电脑名）而不是直接掉到那串
                十六进制 id —— 仪表盘上「设备」这一格是给人看的，`3f7a1c0e` 谁也认不出。 */}
            <code>{sync.data?.deviceName || sync.data?.hostName || sync.data?.device || '—'}</code>
            <span>{t('记忆根')}</span>
            <code>{status.root}</code>
            <span>{t('索引')}</span>
            {/* `count` 为 null = 索引正在整理（重建/压缩独占着连接），后端特意
                不排队直接给 null。写成 0 会被读成「索引空了」，所以显式说「整理中」。 */}
            <code>
              {status.index.count == null ? t('索引整理中') : t('{n} 条', { n: status.index.count })}
              {' · '}
              {status.index.fts ? 'FTS5' : t('内存匹配')}
            </code>
          </div>
          <div className='mm-inline-actions'>
            <Button type='button' size='sm' variant='ghost' onClick={() => onGo('sync')}>{t('同步设置')}</Button>
            <Button type='button' size='sm' variant='ghost' onClick={() => onGo('agents')}>{t('Agent 接入')}</Button>
            <Button type='button' size='sm' variant='ghost' onClick={() => onGo('profile')}>{t('画像')}</Button>
          </div>
        </Section>
      </div>
    </div>
  )
}

function TodoRow({
  state,
  label,
  title,
  detail,
  action,
}: {
  state: 'ok' | 'warn' | 'bad' | 'idle'
  /** 徽标文案（默认「待处理」）。待确认那行用「待确认」——那里的数字不为 0 本身就是异常。 */
  label?: string
  title: string
  detail: string
  action?: { label: string; run: () => void }
}): React.ReactElement {
  const variant = state === 'ok' ? 'success' : state === 'warn' ? 'warning' : state === 'bad' ? 'destructive' : 'outline'
  return (
    <div className='mm-todo'>
      <span className={`mm-todo-dot mm-todo-${state}`} aria-hidden='true' />
      <div className='mm-todo-body'>
        <div className='mm-todo-title'>
          {title}
          <Badge variant={variant} shape='tag'>
            {state === 'ok' ? t('正常') : (label ?? t('待处理'))}
          </Badge>
        </div>
        <div className='mm-todo-detail'>{detail}</div>
      </div>
      {action ? (
        <Button type='button' size='sm' variant='ghost' onClick={action.run}>{action.label}</Button>
      ) : null}
    </div>
  )
}

export { Dashboard }
