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
  const [apiKeyId, setApiKeyId] = React.useState('')
  /** 正在忙的那张卡（空串 = 都没忙）。逐卡而不是全局：同一时刻只允许一个写动作，
   *  但别的卡要照常可读、可展开，不能整页变灰。 */
  const [busyId, setBusyId] = React.useState('')
  const [actionError, setActionError] = React.useState('')
  /** 每张卡最近一次动作的结果（就地显示在卡内，不另起「上次操作结果」区块）。 */
  const [notes, setNotes] = React.useState<Record<string, { ok: boolean; text: string }>>({})
  const [snippet, setSnippet] = React.useState<m.AgentSnippet | null>(null)
  const [snippetFor, setSnippetFor] = React.useState('')

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
   * 一键注入：写 MCP 条目（带 `X-Agent` 身份）+ 按配置写指令文件受控块。
   *
   * 提示里带上「重启客户端」这一步 —— 配置只在启动时读一次，写完不重启等于没写，
   * 用户会盯着「尚未观察到真实调用」以为功能坏了。
   */
  const injectOne = (card: m.AgentCard) => run(card.id, async () => {
    const result = await m.installAgents([card.id], apiKeyId)
    const row = result.results.find(item => item.id === card.id)
    if (row && !row.ok) throw new Error(row.error ?? t('写入失败'))
    const wrote = [row?.mcp, row?.instruction].filter(Boolean).join(' · ')
    return `${wrote || t('已写入')}${t('。重启 {name} 后生效 —— 配置只在启动时读一次。', { name: card.name })}`
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

  if (loading) return <Panel loading error=''>{null}</Panel>
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
            <Input
              className='mm-key-input'
              value={apiKeyId}
              placeholder={t('用哪个 Key（留空 = 默认）')}
              onChange={event => setApiKeyId(event.target.value)}
            />
            <Button
              type='button'
              size='sm'
              variant='ghost'
              disabled={busyId !== ''}
              onClick={() => void run('__all__', async () => {
                const result = await m.verifyAgents([])
                // 分母只算「配置里真有我们这一条」的客户端：把没接入的算成失败，
                // 用户会看到「0 / 7 个正常」这种毫无信息量的结论 —— 那 6 个从来
                // 就没接过，谈不上好坏。
                const wired = result.rows.filter(row => row.mcp)
                if (!wired.length) throw new Error(t('没有任何客户端写入过记忆配置'))
                const dead = wired.filter(row => !row.ok)
                if (dead.length) {
                  throw new Error(t('{ok} / {total} 个握手成功；{names} 连不上', {
                    ok: wired.length - dead.length,
                    total: wired.length,
                    names: dead.map(row => row.name ?? row.id).join('、'),
                  }))
                }
                return t('{n} 个已接入的客户端全部握手成功', { n: wired.length })
              })}
            >
              {busyId === '__all__' ? t('校验中…') : t('全部校验')}
            </Button>
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
        {notes.__all__ ? (
          <p className={notes.__all__.ok ? 'mm-hint' : 'mm-warn'}>{notes.__all__.text}</p>
        ) : null}
      </Section>

      <Section
        title={t('客户端')}
        hint={t('每张卡上直接测、直接写、直接卸。写入前会自动备份原配置文件。')}
      >
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
