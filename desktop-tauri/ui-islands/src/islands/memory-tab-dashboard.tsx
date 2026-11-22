import * as React from 'react'
import { Badge, Button, MetricCard, MetricGrid, MetricLabel, MetricValue } from '@ui'
import { t } from '../i18n'
import * as m from './memory-api'
import { Empty, Panel, Section, StateBadge, num, timeText, useLoad } from './memory-shared'

/**
 * 仪表盘：一眼看全「记忆库现在什么状态」。
 *
 * ── 这一页要回答的四个问题 ──────────────────────────────────
 *   1. 有多少条？分层比例对不对（l2 太少说明从没蒸馏过）；
 *   2. 今天有没有新东西进来？（数字不动 = 没人在用，或 Agent 没接上）
 *   3. 有没有积压要人裁决的？（待确认队列是这套系统唯一需要人工的地方）
 *   4. 同步与体检有没有异常？
 *
 * ── 为什么把「待确认」放这么显眼 ────────────────────────────
 * 记忆的自动化默认全关，唯一会「卡住」的就是待确认队列：AI 提了建议没人批，
 * 队列会一直涨，而用户完全不知道。所以它必须在第一屏。
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
  const health = useLoad(() => m.loadHealth(20), [], { refreshOnEnter: true })
  const sync = useLoad(() => m.loadSync(), [], { refreshOnEnter: true })
  const [rebuilding, setRebuilding] = React.useState(false)
  const [showStale, setShowStale] = React.useState(false)
  const [actionError, setActionError] = React.useState('')

  /**
   * 重建索引：原本在「索引」页的主按钮。那一页删掉之后它是唯一没有别处替代的
   * 动作（体检清单、检索、回收站都各有归宿），所以挪到这里，并且只在
   * 「索引漂移」时出现 —— 平时摆着一个「重建」按钮只会让人想去点它。
   */
  const rebuild = async () => {
    setRebuilding(true)
    setActionError('')
    try {
      await m.rebuildIndex()
      health.reload()
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
  // 所有「有效记忆」口径都要先扣掉草稿：草稿既不进检索也不进注入，把它算进
  // 分母只会让「沉淀占比」凭空变低，让「有效记忆」凭空变多。
  const drafts = status.candidates ?? 0
  const effective = Math.max(0, status.live - drafts)
  const l2Ratio = effective > 0 ? Math.round((status.l2 / effective) * 100) : 0
  const pending = status.review?.pending ?? 0
  /** 积压阈值（配置项 `dedup.pendingWarnThreshold`，后端默认 50）。
   *
   *  它现在的职责只有「描述」：超过它才在 detail 里写「已超过提醒阈值」。**不再决定
   *  这一行亮不亮** —— 旧逻辑用它当开关，41 条 < 50 就显示「正常」，而 detail 同时
   *  写着「41 条建议等待裁决」，一行里两个结论互相打脸。 */
  const warnThreshold = status.review?.warnThreshold ?? 50
  const backlogged = pending >= warnThreshold
  const conflicts = sync.data?.conflicts ?? 0
  const stale = health.data?.staleCount ?? 0
  const drift = health.data ? !health.data.index.inSync : false
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
          <MetricValue>{num(effective)}</MetricValue>
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
          {/* 草稿单列一行：它们在 l1 里，但没被确认、进不了检索与注入。
              不写这一行的话，上面那个「普通记忆」会把草稿算进去 —— 用户以为
              自己有一百多条记忆，实际能搜到的只有其中一部分。 */}
          {(status.candidates ?? 0) > 0 ? (
            <div className='mm-metric-sub'>
              {t('另有 {n} 条自动抽取草稿待确认（不进检索与注入）', { n: status.candidates })}
            </div>
          ) : null}
        </MetricCard>
        <MetricCard>
          <MetricLabel>{t('沉淀占比')}</MetricLabel>
          <MetricValue>{l2Ratio}%</MetricValue>
          <div className='mm-metric-sub'>
            {l2Ratio < 15
              ? t('偏低：打开「蒸馏」任务让结论沉下来')
              : t('比例健康')}
          </div>
        </MetricCard>
        <MetricCard>
          <MetricLabel>{t('待确认')}</MetricLabel>
          <MetricValue className={pending ? 'mm-warn-text' : undefined}>{num(pending)}</MetricValue>
          <div className='mm-metric-sub'>
            {pending ? t('需要你点头才会落盘') : t('队列是空的')}
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
                : t('平均 {n} token / 次 · 未命中 {m} 次', {
                    n: num(status.inject?.avgTokens ?? 0),
                    m: num(status.inject?.missed ?? 0),
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
              ? t('{n} 条建议等待裁决（去重 / 归类 / 失效判定）· 在「浏览 → 待确认」逐条批准或忽略{crowded}', {
                  n: pending,
                  crowded: backlogged ? t(' · 已超过提醒阈值 {n}', { n: warnThreshold }) : '',
                })
              : t('没有积压')}
            action={pending
              ? { label: t('去裁决'), run: () => (onReview ? onReview() : onGo('browse')) }
              : undefined}
          />
          <TodoRow
            state={stale > 0 ? 'warn' : 'ok'}
            title={t('可能过时的记忆')}
            detail={stale
              ? t('{n} 条记忆锚定的文件在写下之后被改过', { n: stale })
              : t('没有发现')}
            action={stale > 0
              ? { label: showStale ? t('收起清单') : t('看清单'), run: () => setShowStale(value => !value) }
              : undefined}
          />
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

        {/* 过时清单就地展开在「需要处理」卡里。
            原来它在「索引」页，那一页删掉之后清单无处可去 —— 但它正是这一行
            「N 条可能过时」的证据，展开在原地比跳到另一页更短。 */}
        {showStale && stale > 0 && health.data ? (
          <div className='mm-stale-list'>
            {health.data.stale.map(row => (
              <div key={row.id} className='mm-stale'>
                <div className='mm-stale-head'>
                  <strong>{row.title}</strong>
                  <Badge variant='secondary' shape='tag'>{row.type}</Badge>
                  <Badge variant='outline' shape='tag'>{row.layer}</Badge>
                  <span className='mm-muted'>{t('写下于 {when}', { when: row.updatedText })}</span>
                  {/* ── 这里是「修复路径」──────────────────────────
                      以前清单只给标题和文件名，看到了也没法动手 —— 用户的原话
                      是「没给我修复路径」。处置动作本来就只有两个：读完文件后
                      要么改这条记忆（确认仍成立就保存一次，`updated_at` 会跟进，
                      提醒自然消失），要么把它删掉/标记失效。两条路都从「打开
                      这条记忆」开始，所以这里必须能点进去。 */}
                  {onOpenEntry ? (
                    <Button
                      type='button'
                      size='sm'
                      variant='link'
                      onClick={() => onOpenEntry(row.id)}
                    >
                      {t('打开')}
                    </Button>
                  ) : null}
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
            <div className='mm-muted' style={{ marginTop: 6 }}>
              {t('处理方式：点「打开」读完那条记忆锚定的文件 —— 结论仍成立就保存一次（提醒会消失），已经不成立就改掉或删除。这不是待确认队列，不需要裁决。')}
            </div>
          </div>
        ) : null}
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
  /** 徽标文案（默认「待处理」）。待确认那行用「待确认」——它要人点头，不是要人修。 */
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
