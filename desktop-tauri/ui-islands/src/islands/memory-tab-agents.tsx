import * as React from 'react'
import { Badge, Button, Input, Popover, PopoverContent, PopoverTrigger } from '@ui'
import { t } from '../i18n'
import * as m from './memory-api'
import { Empty, Panel, Section, StateBadge, num, useLoad } from './memory-shared'

/**
 * Agent 接入页：每张卡「检测 / 注入 / 手动接入 / 卸载」四件事都在卡上。
 *
 * ── 为什么动作从「勾选 + 顶部批量」改成「逐卡按钮」 ──────────
 * 勾选式适合「一次装十个」的初始配置，但接入页真正的日常是**排查**：
 * 某个客户端突然不写了，用户要知道它配置在不在、指令块还在不在、最近有没有
 * 真的调用过，然后就地测一下、重写一次。这套动作全是对着**某一个**客户端做的，
 * 却要先勾选再点顶部按钮 —— 每修一个都多两步，而且「选中 3 个」时点「卸载选中」
 * 的后果是三个一起掉。
 *
 * ── 为什么两枚徽标是「已安装 / 已接入」而不是别的说法 ────────
 * 这两个词回答的是用户脑子里最先冒出来的两个问题，顺序也对：
 *   · 这台机器上装了这个客户端吗？→ 已安装 / 未检出
 *   · 它的记忆接入写进去了吗？    → 已接入 / 未接入
 * 早先用过「配置已写入 / 仅检测」，它们描述的是**我们这边的动作**
 * （写了、只测了），用户得先翻译一次才对应到「装了没、通了没」。
 * 徽标是扫视用的，不该带这层翻译。
 *
 * ── 为什么「真实调用」要给写 / 检索 / 错误三个数 ────────────
 * 只说「共 30 次」回答不了「它到底在干什么」。写 17 说明记忆在被改，检索 3
 * 说明只是偶尔查一下，错误非零说明接入是坏的 —— 三种情况的处理方式完全不同。
 * （分类是从这次升级之后开始记的，所以三项之和可能小于总次数。）
 */

/** 清单还没到手时的空数组。
 *
 * 必须是**模块级常量**而不是每次渲染的 `?? []`：下面的 `useMemo` 依赖里有它，
 * 每次渲染都造一个新数组会让记忆化每轮都失效（依赖比较是 `Object.is`）。
 * 结果一样、也不会死循环，但「派生值」本来是为了省钱，别先自己花了。 */
const EMPTY_KEYS: m.GatewayKey[] = []

/**
 * 卡片右上角的两枚徽标：装了没、接上了没。
 *
 * 两枚都只判真假，不掺程度（不写「检测通过 N 次」）—— 次数在「真实调用」
 * 那一行有，挤进徽标只会让人以为它是另一个维度。词汇固定四个：
 * 已安装 / 未检出、已接入 / 未接入，不因为细分状态再长出新词。
 *
 * ── 为什么「已接入」有时是黄的 ──────────────────────────────
 * 条目在 = 客户端会去连；但老版本安装器写的条目**没有 `X-Agent`**，连上之后
 * 记忆会全部落进共用的 `l1/mcp/`，事后分不清是哪个客户端写的。这时候状态还是
 * 「已接入」（事实没变），颜色降成 warn，具体原因写在卡片内的提示行 ——
 * 徽标负责扫视，解释归正文，两件事不混。
 */
function DetectState({ card }: { card: m.AgentCard }): React.ReactElement {
  const missingIdentity = card.mcp.installed && !card.mcp.agentHeader
  return (
    <>
      <StateBadge state={card.detected ? 'ok' : 'idle'} label={card.detected ? t('已安装') : t('未检出')} />
      <StateBadge
        state={card.mcp.installed ? (missingIdentity ? 'warn' : 'ok') : 'idle'}
        label={card.mcp.installed ? t('已接入') : t('未接入')}
      />
    </>
  )
}

/** 「最近调用」那一行的人话。 */
function callText(card: m.AgentCard): React.ReactNode {
  const beat = card.heartbeat
  if (!beat || beat.calls <= 0) return <span className='mm-muted'>{t('尚未观察到真实调用')}</span>
  const parts = [
    t('写 {n}', { n: num(beat.writes ?? 0) }),
    t('检索 {n}', { n: num(beat.reads ?? 0) }),
    t('错误 {n}', { n: num(beat.errors ?? 0) }),
  ]
  return t('{when}，共 {calls} 次（{parts}）', {
    when: beat.lastSeenText,
    calls: num(beat.calls),
    parts: parts.join('/'),
  })
}

function Agents(): React.ReactElement {
  const { data, loading, error, reload } = useLoad(() => m.loadAgents(), [])
  /**
   * 用哪把 key 写进客户端。
   *
   * ── 为什么是下拉而不是输入框 ────────────────────────────────
   * 输入框要求用户自己记住 key 的 id（`legacy` / 一串 uuid），而他要表达的只是
   * 「用哪一把」。这个值唯一的来源就是网关的 key 清单，没有自由输入的空间 ——
   * 敲错一个字符会得到「所选 API Key 不存在」，而用户没有任何线索判断错在哪。
   * 下拉把「可能的取值」直接摆在眼前，选不出来就是没有，一目了然。
   *
   * ── 为什么默认就选中第一把 ──────────────────────────────────
   * 空值必须有个明确归宿。旧文案写「留空 = 默认」但后端会直接 400，等于让用户
   * 照着一句谎话操作。现在后端会把空值落到第一把可用 key，前端也就默认选中它，
   * 并在注入成功后回执「Key：某某」—— 隐式决策要变成可见事实。
   */
  const [apiKeyId, setApiKeyId] = React.useState('')
  const keys = useLoad(() => m.loadGatewayKeys(), [])
  /** 正在忙的那张卡（空串 = 都没忙）。逐卡而不是全局：同一时刻只允许一个写动作，
   *  但别的卡要照常可读、可展开，不能整页变灰。 */
  const [busyId, setBusyId] = React.useState('')
  const [actionError, setActionError] = React.useState('')
  /** 每张卡最近一次动作的结果（就地显示在卡内，不另起「上次操作结果」区块）。 */
  const [notes, setNotes] = React.useState<Record<string, { ok: boolean; text: string }>>({})
  const [snippet, setSnippet] = React.useState<m.AgentSnippet | null>(null)
  const [snippetFor, setSnippetFor] = React.useState('')

  /**
   * 真正要提交的那把 key = 「state 里的选择」∩「清单里真实存在的」。
   *
   * ── 为什么在渲染期派生，而不是用 effect 回写 state ──────────
   * 两件事必须同时成立：下拉**显示**的是 A，提交上去的**也是** A。用 effect 回写
   * 的话中间有一帧 state 还是旧值 —— 而 `<select value>` 指向不存在的 option 时
   * 浏览器会把显示落到第一项，于是「看着 A、提交 B」。派生值没有这一帧：显示的
   * 和提交的就是同一个变量，不必再验一次。
   *
   * 两种失效都要管：还没选过（空串）、选过的那把后来被删了或停用了。后者很常见 ——
   * 用户去「账号与密钥」删掉旧 key，回来接着点「一键注入」，不该 400。
   */
  const keyList = keys.data?.keys ?? EMPTY_KEYS
  const pickedKeyId = React.useMemo(() => {
    if (keyList.some(key => key.id === apiKeyId)) return apiKeyId
    return keyList[0]?.id ?? ''
  }, [apiKeyId, keyList])

  /** 跑一个逐卡动作：结果写进 `notes[id]`，不弹全局提示条。 */
  const run = async (id: string, task: () => Promise<string>) => {
    setBusyId(id)
    setActionError('')
    try {
      const text = await task()
      setNotes(current => ({ ...current, [id]: { ok: true, text } }))
      reload()
    } catch (caught) {
      const message = caught instanceof Error ? caught.message : String(caught)
      setNotes(current => ({ ...current, [id]: { ok: false, text: message } }))
    } finally {
      setBusyId('')
    }
  }

  /**
   * 测试连接：现在真的发一次 MCP `initialize`，不是读盘。
   *
   * 失败时把后端给的 `detail` 原样显示 —— 它带着具体地址与错误（连不上 / 401 /
   * 不是 MCP 握手），比「检测没通过」有用得多，用户照着就能定位是端口、密钥
   * 还是服务没起。
   */
  const testOne = (card: m.AgentCard) => run(card.id, async () => {
    const result = await m.verifyAgents([card.id])
    const row = result.rows[0]
    const detailText = row?.detail || ''
    if (!row?.ok) throw new Error(detailText || t('检测没通过'))
    return detailText || t('检测通过')
  })

  /**
   * 全部校验：对每个「配置里真有这一条」的客户端各发一次 MCP `initialize`。
   *
   * 结论必须**逐卡落回** `notes[id]`，不能只在顶部留一句汇总 —— 汇总说「2 个连不上」
   * 等于把「是哪两个」又推回给用户，还得对着卡片一张张点「测试连接」重测一遍，
   * 那这个按钮就白点了。所以先按行写回，再决定要不要额外抛一句错。
   *
   * 分母只算已写入配置的客户端：把没接入的算作失败，用户会看到「0 / 7 个正常」
   * 这种毫无信息量的结论 —— 那 6 个从来就没接过，谈不上好坏。
   */
  const verifyAll = () => run('__all__', async () => {
    const result = await m.verifyAgents([])
    const wired = result.rows.filter(row => row.mcp)
    if (!wired.length) throw new Error(t('没有任何客户端写入过记忆配置'))
    setNotes(current => {
      const next = { ...current }
      for (const row of wired) {
        next[row.id] = {
          ok: row.ok,
          text: row.detail || (row.ok ? t('检测通过') : t('检测没通过')),
        }
      }
      return next
    })
    const dead = wired.filter(row => !row.ok)
    if (!dead.length) return t('{n} 个已接入的客户端全部握手成功', { n: wired.length })
    throw new Error(t('{ok} / {total} 个握手成功；{names} 连不上', {
      ok: wired.length - dead.length,
      total: wired.length,
      names: dead.map(row => row.name ?? row.id).join('、'),
    }))
  })

  /**
   * 一键注入：写 MCP 条目（带 `X-Agent` 身份）+ 按配置写指令文件受控块。
   *
   * 提示里带上「重启客户端」这一步 —— 配置只在启动时读一次，写完不重启等于没写，
   * 用户会盯着「尚未观察到真实调用」以为功能坏了。
   */
  const injectOne = (card: m.AgentCard) => run(card.id, async () => {
    const result = await m.installAgents([card.id], pickedKeyId)
    const row = result.results.find(item => item.id === card.id)
    if (row && !row.ok) throw new Error(row.error ?? t('写入失败'))
    const wrote = [row?.mcp, row?.instruction].filter(Boolean).join(' · ')
    // 带上「用了哪把 key」：用户可能在别处把它停用了，回来点注入仍会成功（后端
    // 会挑第一把），这条回执是事后唯一能查到「客户端拿的是哪把凭证」的线索。
    const usedKey = result.apiKey
      ? t('（Key：{name}）', { name: result.apiKey.name })
      : ''
    return `${wrote || t('已写入')}${usedKey}${t('。重启 {name} 后生效 —— 配置只在启动时读一次。', { name: card.name })}`
  })

  const dropOne = (card: m.AgentCard) => run(card.id, async () => {
    const result = await m.uninstallAgents([card.id])
    const row = result.results.find(item => item.id === card.id)
    if (row && !row.ok) throw new Error(row.error ?? t('卸载失败'))
    return t('已从该客户端移除配置')
  })

  const openSnippet = async () => {
    if (snippet) return
    try {
      setSnippet(await m.loadAgentSnippet())
    } catch (caught) {
      setActionError(caught instanceof Error ? caught.message : String(caught))
    }
  }

  // 只在「第一次拿数据」时占满页面。刷新 / 注入 / 卸载后 `reload()` 会再跑一次，
  // 那一次数据还在手上 —— 整页退回「正在读取…」既没必要，也让人以为刚才那一下
  // 把页面弄坏了。保留旧卡片、就地更新，才是这张表该有的手感。
  if (loading && !data) return <Panel loading error=''>{null}</Panel>
  if (error) return <Panel loading={false} error={error} onRetry={reload}>{null}</Panel>
  if (!data) return <Empty>{t('读不到客户端清单。')}</Empty>

  const installed = data.cards.filter(card => card.mcp.installed).length
  const alive = data.cards.filter(card => (card.heartbeat?.calls ?? 0) > 0).length

  return (
    <div className='mm-agents'>
      {actionError ? <div className='mm-error'>{actionError}</div> : null}

      <Section
        title={t('接入概况')}
        hint={t('记忆只走 MCP：客户端自己决定什么时候读，网关不改任何请求体。')}
        action={(
          <div className='mm-inline-actions'>
            {/* key 选择器：清单为空时给出「去哪建」，而不是一个点不动的空下拉。
                用原生 `select` + `.mm-select`：本页其余 9 处下拉（归类 / 层级 /
                项目 / 格式…）全是这个形态，混进一个 @ui Select 会让同一页出现两套
                下拉外观（高度、箭头、阴影都不一样）。 */}
            {keyList.length ? (
              <select
                className='mm-select mm-key-select'
                aria-label={t('写进客户端用的 API Key')}
                value={pickedKeyId}
                onChange={event => setApiKeyId(event.target.value)}
              >
                {keyList.map(key => (
                  <option key={key.id} value={key.id}>{key.name}（{key.masked}）</option>
                ))}
              </select>
            ) : (
              <span className={keys.error ? 'mm-warn mm-key-empty' : 'mm-muted mm-key-empty'}>
                {keys.error
                  // 「读不到清单」和「一把都没有」是两件事，处理方式也不同：
                  // 前者要重试，后者要新建。合并成一句「没有可用的 Key」会让读失败
                  // 的用户跑去建第二把 key —— 而问题根本不在那里。
                  ? t('读不到 Key 清单：{error}', { error: keys.error })
                  : keys.loading
                    ? t('正在读取 Key…')
                    : t('还没有可用的 API Key —— 先到「账号与密钥 → API Key」建一把')}
              </span>
            )}
          </div>
        )}
      >
        <div className='mm-inline-meta'>
          <StateBadge state={installed ? 'ok' : 'warn'} label={t('{n} / {total} 个客户端已写入配置', { n: installed, total: data.cards.length })} />
          <StateBadge state={alive ? 'ok' : 'idle'} label={t('{n} 个有真实调用', { n: alive })} />
          <Badge variant='outline' shape='tag'>{data.tools} {t('个工具')}</Badge>
          <Badge variant='outline' shape='tag'>{data.injectAgentsMd ? t('会写指令文件') : t('只写 MCP 配置')}</Badge>
        </div>
        <div className='mm-status-grid mm-status-compact'>
          <span>{t('服务名')}</span><code>{data.serverName}</code>
          <span>{t('端点')}</span><code>{data.endpoint}</code>
        </div>
      </Section>

      <Section
        title={t('客户端')}
        hint={t('每张卡上直接测、直接写、直接卸。写入前会自动备份原配置文件。')}
        action={(
          <Button
            type='button'
            size='sm'
            variant='ghost'
            disabled={busyId !== ''}
            onClick={() => void verifyAll()}
          >
            {busyId === '__all__' ? t('校验中…') : t('全部校验')}
          </Button>
        )}
      >
        {/* 批量校验的结论已经逐卡写回 `notes[id]`，这里只留「一条都没接」这种
            没法落到任何一张卡上的全局失败，否则用户点完看不见任何反应。 */}
        {notes.__all__ && !notes.__all__.ok ? (
          <p className='mm-warn'>{notes.__all__.text}</p>
        ) : null}
        <div className='mm-agent-grid'>
          {data.cards.map(card => {
            const busy = busyId === card.id
            const note = notes[card.id]
            return (
              <div key={card.id} className={`mm-agent-card${card.supported ? '' : ' disabled'}`}>
                <div className='mm-agent-head'>
                  {/* 品牌色小圆点：卡片密排时它比字母方块更快被认出来，
                      又不占宽度（三列网格里每像素都要用在信息上）。 */}
                  <span className='mm-agent-dot' style={{ background: `hsl(${card.hue ?? 210} 62% 52%)` }} aria-hidden />
                  <div className='mm-agent-id'>
                    <strong>{card.name}</strong>
                    <span className='mm-muted'>
                      {card.vendor ?? card.id}{card.version ? ` · v${card.version}` : ''}
                    </span>
                  </div>
                  <div className='mm-agent-badges'>
                    <DetectState card={card} />
                  </div>
                </div>

                <div className='mm-agent-rows'>
                  <div className='mm-row-line'>
                    <span className='mm-row-label'>{t('配置文件')}</span>
                    <span className='mm-row-value tight'>
                      {card.configPath
                        ? <code className={card.configExists ? undefined : 'mm-muted'}>{card.configPath}</code>
                        : <span className='mm-muted'>{t('不适用')}</span>}
                      {card.configPath ? (
                        card.configExists
                          ? <Badge variant='success' shape='tag'>{t('存在')}</Badge>
                          : <Badge variant='destructive' shape='tag'>{t('不存在')}</Badge>
                      ) : null}
                    </span>
                  </div>

                  <div className='mm-row-line'>
                    <span className='mm-row-label'>{t('指令文件')}</span>
                    <span className='mm-row-value tight'>
                      {card.instruction?.path
                        ? <code className='mm-muted'>{card.instruction.path}</code>
                        : <span className='mm-muted'>{t('不适用')}</span>}
                      {card.instruction ? (
                        card.instruction.broken
                          ? <Badge variant='destructive' shape='tag'>{t('标记损坏')}</Badge>
                          : card.instruction.injected
                            ? <Badge variant='success' shape='tag'>{t('已写入')}</Badge>
                            : card.instruction.exists
                              ? <Badge variant='outline' shape='tag'>{t('未写入')}</Badge>
                              : <Badge variant='outline' shape='tag'>{t('待新建')}</Badge>
                      ) : null}
                    </span>
                  </div>

                  <div className='mm-row-line'>
                    <span className='mm-row-label'>{t('真实调用')}</span>
                    <span className='mm-row-value'>{callText(card)}</span>
                  </div>
                </div>

                {card.mcp.installed && !card.mcp.agentHeader ? (
                  <p className='mm-agent-note'>
                    {t('这条配置是旧版本写的，缺客户端身份 —— 记忆会记到共用的 mcp 名下。点「一键注入」重写一次即可补齐。')}
                  </p>
                ) : null}
                {card.heartbeat && card.heartbeat.calls > 0 && !card.heartbeat.ok ? (
                  <p className='mm-agent-note'>{t('最近一次调用失败了 —— 看服务端日志里的 memory 工具报错。')}</p>
                ) : null}
                {!card.heartbeat || card.heartbeat.calls === 0 ? (
                  <p className='mm-agent-hint'>
                    {t('一直没有调用？确认客户端重启过 —— 配置只在启动时读一次。')}
                  </p>
                ) : null}
                {!card.supported && card.note ? (
                  <p className='mm-agent-note'>{card.note}</p>
                ) : null}

                {note ? (
                  <p className={note.ok ? 'mm-agent-result ok' : 'mm-agent-result bad'}>{note.text}</p>
                ) : null}

                <div className='mm-agent-foot'>
                  <Button type='button' size='sm' variant='outline' disabled={busy} onClick={() => void testOne(card)}>
                    {busy ? t('执行中…') : t('测试连接')}
                  </Button>
                  <Button type='button' size='sm' disabled={busy || !card.supported} onClick={() => void injectOne(card)}>
                    {t('一键注入')}
                  </Button>
                  <Popover
                    open={snippetFor === card.id}
                    onOpenChange={open => {
                      setSnippetFor(open ? card.id : '')
                      if (open) void openSnippet()
                    }}
                  >
                    <PopoverTrigger render={<Button type='button' size='sm' variant='ghost' />}>
                      {t('手动接入片段')}
                    </PopoverTrigger>
                    <PopoverContent align='start' sideOffset={4} className='w-[420px]'>
                      <div className='mm-snippet'>
                        <div className='mm-section-title'>
                          {t('{name} 的配置片段', { name: card.name })}
                          {card.configPath ? <code className='mm-muted'> · {card.configPath}</code> : null}
                        </div>
                        {snippet ? (
                          card.format === 'toml'
                            ? <pre className='mm-body'>{snippet.toml}</pre>
                            : <pre className='mm-body'>{JSON.stringify(snippet.json, null, 2)}</pre>
                        ) : <div className='mm-muted'>{t('读取中…')}</div>}
                        {snippet ? (
                          <>
                            <div className='mm-section-title'>{t('建议写进指令文件的约定')}</div>
                            <pre className='mm-body'>{snippet.instruction}</pre>
                          </>
                        ) : null}
                      </div>
                    </PopoverContent>
                  </Popover>
                  <Button
                    type='button'
                    size='sm'
                    variant='ghost'
                    disabled={busy || !card.mcp.installed}
                    onClick={() => void dropOne(card)}
                  >
                    {t('卸载')}
                  </Button>
                </div>
              </div>
            )
          })}

          {data.custom.map(item => {
            const busy = busyId === item.id
            const note = notes[item.id]
            return (
              <div key={`custom:${item.id}`} className='mm-agent-card'>
                <div className='mm-agent-head'>
                  <span className='mm-agent-dot' style={{ background: 'var(--text-3)' }} aria-hidden />
                  <div className='mm-agent-id'>
                    <strong>{item.name}</strong>
                    <span className='mm-muted'>{t('自定义')} · {item.format === 'toml' ? 'TOML' : 'JSON'}</span>
                  </div>
                  <div className='mm-agent-badges'>
                    <StateBadge state={item.detected ? 'ok' : 'idle'} label={item.detected ? t('已安装') : t('未检出')} />
                    <StateBadge state={item.installed ? 'ok' : 'idle'} label={item.installed ? t('已接入') : t('未接入')} />
                  </div>
                </div>

                <div className='mm-agent-rows'>
                  <div className='mm-row-line'>
                    <span className='mm-row-label'>{t('配置文件')}</span>
                    <span className='mm-row-value tight'>
                      {item.configPath
                        ? <code className={item.configExists ? undefined : 'mm-muted'} title={item.configPath}>{item.configPath}</code>
                        : <span className='mm-muted'>{t('未填写')}</span>}
                      {item.configPath ? (
                        item.configExists
                          ? <Badge variant='success' shape='tag'>{t('存在')}</Badge>
                          : <Badge variant='destructive' shape='tag'>{t('不存在')}</Badge>
                      ) : null}
                    </span>
                  </div>
                  <div className='mm-row-line'>
                    <span className='mm-row-label'>{t('指令文件')}</span>
                    <span className='mm-row-value tight'>
                      {item.instructionPath
                        ? <code className='mm-muted' title={item.instructionPath}>{item.instructionPath}</code>
                        : <span className='mm-muted'>{t('未指定')}</span>}
                      {item.instructionPath && item.instructionExists
                        ? <Badge variant='outline' shape='tag'>{t('存在')}</Badge>
                        : null}
                    </span>
                  </div>
                  <div className='mm-row-line'>
                    <span className='mm-row-label'>{t('真实调用')}</span>
                    <span className='mm-row-value'>
                      <span className='mm-muted'>{t('不记心跳（看它自己的日志）')}</span>
                    </span>
                  </div>
                </div>

                {note ? (
                  <p className={note.ok ? 'mm-agent-result ok' : 'mm-agent-result bad'}>{note.text}</p>
                ) : null}

                <div className='mm-agent-foot'>
                  {/* 自定义 Agent 没有资源写入器：我们不知道它家配置的合并语义，
                      写进去可能覆盖用户手写的别的键。所以「一键注入」只摆着不可点，
                      真要写就用手动片段 —— 那也是这张卡存在的意义。 */}
                  <Button type='button' size='sm' variant='outline' disabled title={t('自定义 Agent 没有内置写入器，请用「手动接入片段」')}>
                    {t('一键注入')}
                  </Button>
                  <Button
                    type='button'
                    size='sm'
                    variant='ghost'
                    disabled={busy}
                    onClick={() => void run(item.id, async () => {
                      await m.removeCustomAgent(item.name)
                      return t('已从列表移除')
                    })}
                  >
                    {t('移除')}
                  </Button>
                </div>
              </div>
            )
          })}
        </div>
      </Section>

      <ManualPanel
        cards={data.cards}
        custom={data.custom}
        snippet={snippet}
        onLoadSnippet={() => void openSnippet()}
      />

      <CustomForm onSaved={reload} />
    </div>
  )
}

export { Agents }
/* ─── 手动接入面板 ─────────────────────────────────────────── */

type ManualTab = 'json' | 'toml' | 'command'

/**
 * 手动接入：选一个客户端，拿到「贴哪儿、贴什么」。
 *
 * ── 为什么要有这块，而不是只留每张卡上的「手动接入片段」 ──────
 * 卡片上的弹层假设「这个客户端在我们清单里」。真实情况里有一半是：
 * 用户用的客户端我们没收录、或者路径被他自己改过。这时候他需要的是一个
 * **不依赖卡片存在**的地方：任意挑一个目标，直接看片段与写入位置。
 *
 * ── 为什么三种形态都要给 ────────────────────────────────────
 * JSON 是多数客户端的标准；TOML 是 Codex 一类用 `[mcp_servers]` 表的工具；
 * 命令行只给 Claude Code —— 其它客户端的 CLI 参数各家不同还会随版本改，
 * 猜一条错的命令比不给更糟，这一点在页面上写清楚。
 */
function ManualPanel({
  cards,
  custom,
  snippet,
  onLoadSnippet,
}: {
  cards: m.AgentCard[]
  custom: m.AgentCustom[]
  snippet: m.AgentSnippet | null
  onLoadSnippet: () => void
}): React.ReactElement {
  const targets = [
    ...cards.map(card => ({
      id: card.id,
      name: card.name,
      path: card.configPath ?? '',
      format: card.format ?? 'json',
    })),
    ...custom.map(item => ({
      id: `custom:${item.id}`,
      name: `${item.name}（自定义）`,
      path: item.configPath,
      format: item.format,
    })),
  ]
  const [open, setOpen] = React.useState(true)
  const [target, setTarget] = React.useState(targets[0]?.id ?? '')
  const [tab, setTab] = React.useState<ManualTab>('json')
  const [showBlock, setShowBlock] = React.useState(false)
  const [copied, setCopied] = React.useState('')

  React.useEffect(() => {
    if (open && !snippet) onLoadSnippet()
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [open])

  // 数据是后到的：挂载时 cards 还是空的，`target` 会停在空串上，
  // 结果下拉有选项但没有选中项。选中的 id 一旦不在清单里就回落到第一项。
  React.useEffect(() => {
    if (targets.length && !targets.some(item => item.id === target)) {
      setTarget(targets[0].id)
    }
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [cards, custom])

  const picked = targets.find(item => item.id === target) ?? targets[0]
  const body = !snippet
    ? t('读取中…')
    : tab === 'toml'
      ? snippet.toml
      : tab === 'json'
        ? JSON.stringify(snippet.json, null, 2)
        : snippet.commandLine && picked?.id === snippet.commandLineFor
          ? snippet.commandLine
          : t('命令行只在 Claude Code 上有稳定写法；这个客户端请用 JSON / TOML 粘贴。')

  const copy = async (key: string, text: string) => {
    try {
      await navigator.clipboard.writeText(text)
      setCopied(key)
      window.setTimeout(() => setCopied(current => (current === key ? '' : current)), 1600)
    } catch {
      setCopied('')
    }
  }

  const TABS: [ManualTab, string][] = [
    ['json', t('JSON')],
    ['toml', t('TOML')],
    ['command', t('命令行')],
  ]

  return (
    <Section
      title={t('手动接入')}
      hint={t('不在这份清单里的客户端，或者你想自己贴，用这里。')}
      action={(
        <Button type='button' size='sm' variant='ghost' onClick={() => setOpen(value => !value)}>
          {open ? t('收起') : t('展开')}
        </Button>
      )}
    >
      {open ? (
        <>
          <div className='mm-manual-bar'>
            <select
              className='mm-select mm-manual-target'
              value={picked?.id ?? ''}
              onChange={event => setTarget(event.target.value)}
            >
              {targets.map(item => <option key={item.id} value={item.id}>{item.name}</option>)}
            </select>

            <div className='mm-seg'>
              {TABS.map(([key, label]) => (
                <button
                  key={key}
                  type='button'
                  className={tab === key ? 'mm-seg-item on' : 'mm-seg-item'}
                  onClick={() => setTab(key)}
                >
                  {label}
                </button>
              ))}
            </div>

            <div className='mm-inline-actions'>
              <Button type='button' size='sm' variant='outline' disabled={!snippet} onClick={() => void copy('body', body)}>
                {copied === 'body' ? t('已复制') : t('复制配置')}
              </Button>
              <Button
                type='button'
                size='sm'
                variant='ghost'
                disabled={!snippet}
                onClick={() => void copy('block', snippet?.instruction ?? '')}
              >
                {copied === 'block' ? t('已复制') : t('复制指令块')}
              </Button>
              <Button
                type='button'
                size='sm'
                variant='ghost'
                disabled={!snippet?.commandLine}
                onClick={() => void copy('cmd', snippet?.commandLine ?? '')}
              >
                {copied === 'cmd' ? t('已复制') : t('复制启动命令')}
              </Button>
            </div>
          </div>

          {picked?.path ? (
            <p className='mm-hint'>
              {t('写入 {path} 的 {key}', {
                path: picked.path,
                key: picked.format === 'toml' ? '[mcp_servers]' : 'mcpServers',
              })}
            </p>
          ) : (
            <p className='mm-hint'>{t('这个客户端在清单里没有可写路径 —— 下面这段贴到它自己的 MCP 配置里。')}</p>
          )}

          <pre className='mm-body mm-manual-code'>{body}</pre>

          <button type='button' className='mm-manual-toggle' onClick={() => setShowBlock(value => !value)}>
            {showBlock ? '▾' : '▸'} {t('指令受控块（写进 AGENTS.md / CLAUDE.md 的内容）')}
          </button>
          {showBlock ? <pre className='mm-body mm-manual-code'>{snippet?.instruction ?? ''}</pre> : null}

          <p className='mm-hint'>
            {t('手动步骤：① 打开配置文件 → ② 粘贴上面的片段（或直接在 Agent 里执行命令行）→ ③ 重启对应 Agent → ④ 回到本页「测试连接」确认。')}
          </p>
        </>
      ) : null}
    </Section>
  )
}

/* ─── 自定义 Agent 表单 ────────────────────────────────────── */

/**
 * 自定义 Agent（扩展位）。
 *
 * 收录清单里没有的客户端：给它一个名字、一个配置文件的绝对路径、以及
 * 「MCP 条目写在哪个键下」。保存后它会以卡片出现在上面的网格里，
 * 配置路径是否存在、有没有我们的服务名，都直接读盘判断。
 *
 * 格式选错会被后端拦下（.toml 配 JSON 这类），不在前端做这个判断 ——
 * 判断规则只该有一份。
 */
function CustomForm({ onSaved }: { onSaved: () => void }): React.ReactElement {
  const [open, setOpen] = React.useState(false)
  const [name, setName] = React.useState('')
  const [configPath, setConfigPath] = React.useState('')
  const [format, setFormat] = React.useState('json')
  const [instructionPath, setInstructionPath] = React.useState('')
  const [busy, setBusy] = React.useState(false)
  const [error, setError] = React.useState('')

  const save = async () => {
    setBusy(true)
    setError('')
    try {
      await m.addCustomAgent(name, configPath, format, instructionPath)
      setName('')
      setConfigPath('')
      setInstructionPath('')
      onSaved()
    } catch (caught) {
      setError(caught instanceof Error ? caught.message : String(caught))
    } finally {
      setBusy(false)
    }
  }

  return (
    <Section
      title={t('自定义 Agent（扩展位）')}
      hint={t('清单里没有的客户端，给它一个名字和配置文件路径，它就会出现在上面的网格里。')}
      action={(
        <Button type='button' size='sm' variant='ghost' onClick={() => setOpen(value => !value)}>
          {open ? t('收起') : t('添加')}
        </Button>
      )}
    >
      {open ? (
        <>
          <div className='mm-custom-form'>
            <Input
              className='mm-custom-name'
              value={name}
              placeholder={t('名称，如 Cline')}
              onChange={event => setName(event.target.value)}
            />
            <Input
              value={configPath}
              spellCheck={false}
              placeholder={t('配置文件绝对路径（如 ~/.cline/mcp.json）')}
              onChange={event => setConfigPath(event.target.value)}
            />
            <select className='mm-select' value={format} onChange={event => setFormat(event.target.value)}>
              <option value='json'>{t('JSON · mcpServers')}</option>
              <option value='toml'>{t('TOML · mcp_servers')}</option>
            </select>
            <Input
              value={instructionPath}
              spellCheck={false}
              placeholder={t('指令文件路径（可选）')}
              onChange={event => setInstructionPath(event.target.value)}
            />
            <Button type='button' size='sm' disabled={busy || !name.trim() || !configPath.trim()} onClick={() => void save()}>
              {busy ? t('保存中…') : t('保存')}
            </Button>
          </div>
          {error ? <div className='mm-error'>{error}</div> : null}
          <p className='mm-hint'>
            {t('格式选项对应「MCP 条目写在哪个键下」：JSON·mcpServers 是多数工具的标准；TOML 适用于 Codex 一类用 [mcp_servers] 表的工具。选错保存会被拦下并提示。')}
          </p>
        </>
      ) : null}
    </Section>
  )
}
