import * as React from 'react'
import { Badge, Button, Checkbox, Input } from '@ui'
import { t } from '../i18n'
import * as m from './memory-api'
import { Empty, Panel, Section, StateBadge, timeText, useLoad } from './memory-shared'

/**
 * Agent 接入页：11 张客户端卡 + 一键写 MCP + 校验 + 卸载 + 配置片段。
 *
 * ── 为什么这里能「一键接入」而「设置」页只指路 ──────────────
 * 早期版本把接入做在设置页，理由是「MCP & Skills 页已经有完整流程，抄一套会漂移」。
 * 对齐 AgentHub 之后改成两处都有、职责不同：
 *   · 本页是**记忆专用**的接入视图 —— 它要显示「这个客户端的记忆 MCP 装了没、
 *     提示词块还在不在、最近有没有真的调用过」，这些信息只有记忆模块知道；
 *   · 「MCP & Skills」页是**所有工具服务**的通用安装器，负责首次配置与协议细节。
 * 两处共用同一个后端（`memory::agents` 走 `core::agents::resources`），不会漂移。
 *
 * ── 为什么要有「校验」而不只是「已安装」──────���──────────────
 * 「写了配置」和「客户端真的会用」是两件事：用户可能在别处改过配置文件、
 * 客户端可能改过配置格式、提示词块可能被手删了一半。校验是**重新读一遍磁盘**，
 * 而不是读我们自己写的记录 —— 后者只能证明「我们以为写过了」。
 *
 * ── 心跳是什么 ──────────────────────────────────────────────
 * 每次 Agent 通过 MCP 调用工具都会落一条 `agent_beat`。所以「最近活跃」是真实
 * 调用痕迹，不是「配置文件存在」。装上了但从来没调用过的客户端会显示「从未调用」，
 * 那通常意味着配置写对了但客户端没重启。
 */

function Agents(): React.ReactElement {
  const { data, loading, error, reload } = useLoad(() => m.loadAgents(), [])
  const [picked, setPicked] = React.useState<string[]>([])
  const [apiKeyId, setApiKeyId] = React.useState('')
  const [busy, setBusy] = React.useState(false)
  const [actionError, setActionError] = React.useState('')
  const [results, setResults] = React.useState<m.InstallResult[]>([])
  const [verify, setVerify] = React.useState<{ ok: number; total: number; rows: Record<string, unknown>[] } | null>(null)
  const [snippet, setSnippet] = React.useState<m.AgentSnippet | null>(null)

  const act = async (task: () => Promise<unknown>) => {
    setBusy(true)
    setActionError('')
    try {
      await task()
      reload()
    } catch (caught) {
      setActionError(caught instanceof Error ? caught.message : String(caught))
    } finally {
      setBusy(false)
    }
  }

  const toggle = (id: string) => {
    setPicked(current => current.includes(id) ? current.filter(item => item !== id) : [...current, id])
  }

  if (loading) return <Panel loading error=''>{null}</Panel>
  if (error) return <Panel loading={false} error={error} onRetry={reload}>{null}</Panel>
  if (!data) return <Empty>{t('读不到客户端清单。')}</Empty>

  const installed = data.cards.filter(card => card.mcp.installed).length
  const alive = data.cards.filter(card => card.heartbeat?.alive).length

  return (
    <div className='mm-agents'>
      {actionError ? <div className='mm-error'>{actionError}</div> : null}

      <Section
        title={t('接入概况')}
        hint={t('记忆只走 MCP：客户端自己决定什么时候读，网关不改任何请求体。')}
      >
        <div className='mm-inline-meta'>
          <StateBadge state={installed ? 'ok' : 'warn'} label={t('{n} / {total} 个客户端已接入', { n: installed, total: data.cards.length })} />
          <StateBadge state={alive ? 'ok' : 'idle'} label={t('{n} 个最近有调用', { n: alive })} />
          <Badge variant='outline' shape='tag'>{data.tools} {t('个工具')}</Badge>
          <Badge variant='outline' shape='tag'>{data.injectAgentsMd ? t('会写提示词块') : t('只写 MCP 配置')}</Badge>
        </div>
        <div className='mm-status-grid mm-status-compact'>
          <span>{t('服务名')}</span><code>{data.serverName}</code>
          <span>{t('端点')}</span><code>{data.endpoint}</code>
        </div>
      </Section>

      <Section
        title={t('客户端')}
        hint={t('勾选要接入的客户端，然后一次性写入。写入前会自动备份原配置文件。')}
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
              disabled={busy || picked.length === 0}
              onClick={() => void act(async () => {
                const result = await m.installAgents(picked, apiKeyId)
                setResults(result.results)
                setPicked([])
              })}
            >
              {busy ? t('写入中…') : t('接入选中（{n}）', { n: picked.length })}
            </Button>
            <Button
              type='button'
              size='sm'
              variant='ghost'
              disabled={busy || picked.length === 0}
              onClick={() => void act(async () => {
                const result = await m.uninstallAgents(picked)
                setResults(result.results)
                setPicked([])
              })}
            >
              {t('卸载选中')}
            </Button>
            <Button
              type='button'
              size='sm'
              variant='ghost'
              onClick={() => void act(async () => {
                setVerify(await m.verifyAgents(picked))
              })}
            >
              {t('校验磁盘')}
            </Button>
          </div>
        )}
      >
        <div className='mm-agent-grid'>
          {data.cards.map(card => (
            <label key={card.id} className={`mm-agent-card${card.supported ? '' : ' disabled'}`}>
              <div className='mm-agent-head'>
                <Checkbox
                  checked={picked.includes(card.id)}
                  disabled={!card.supported}
                  onCheckedChange={() => toggle(card.id)}
                />
                <div className='mm-agent-id'>
                  <strong>{card.name}</strong>
                  <span className='mm-muted'>{card.vendor ?? card.id}{card.version ? ` · v${card.version}` : ''}</span>
                </div>
                <div className='mm-agent-badges'>
                  <StateBadge state={card.detected ? 'ok' : 'idle'} label={card.detected ? t('已安装') : t('未检出')} />
                  <StateBadge state={card.mcp.installed ? 'ok' : 'idle'} label={card.mcp.installed ? t('记忆已接入') : t('未接入')} />
                </div>
              </div>

              {!card.supported ? (
                <div className='mm-agent-note'>{card.note || t('这个客户端暂不支持自动写入，用下面的配置片段手动贴。')}</div>
              ) : null}

              <div className='mm-agent-rows'>
                <div className='mm-row-line'>
                  <span className='mm-row-label'>{t('配置文件')}</span>
                  <span className='mm-row-value'>
                    {card.configPath
                      ? <code className={card.configExists ? undefined : 'mm-muted'}>{card.configPath}{card.configExists ? '' : ` ${t('（不存在）')}`}</code>
                      : <span className='mm-muted'>{t('不适用')}</span>}
                  </span>
                </div>
                {card.instruction ? (
                  <div className='mm-row-line'>
                    <span className='mm-row-label'>{t('提示词块')}</span>
                    <span className='mm-row-value'>
                      {card.instruction.broken
                        ? <Badge variant='destructive' shape='tag'>{t('标记损坏')}</Badge>
                        : card.instruction.injected
                          ? <Badge variant='success' shape='tag'>{t('已写入')}</Badge>
                          : <Badge variant='outline' shape='tag'>{t('未写入')}</Badge>}
                      {card.instruction.path ? <code className='mm-muted'>{card.instruction.path}</code> : null}
                    </span>
                  </div>
                ) : null}
                <div className='mm-row-line'>
                  <span className='mm-row-label'>{t('最近调用')}</span>
                  <span className='mm-row-value'>
                    {card.heartbeat
                      ? card.heartbeat.calls > 0
                        ? t('{when} · 累计 {n} 次 · 最后工具 {tool}', {
                          when: card.heartbeat.lastSeenText,
                          n: card.heartbeat.calls,
                          tool: card.heartbeat.lastTool ?? '—',
                        })
                        : t('已登记但从未调用')
                      : <span className='mm-muted'>{t('从未调用过（装好后需要重启客户端）')}</span>}
                  </span>
                </div>
              </div>
            </label>
          ))}
        </div>

        {data.custom.length ? (
          <div className='mm-custom-agents'>
            <div className='mm-section-title'>{t('自定义客户端')}</div>
            <div className='mm-muted'>{t('这些是手填的路径，格式未知，不参与一键接入 —— 用下面的片段手动配。')}</div>
            <div className='mm-tags'>
              {data.custom.map(item => <Badge key={item.id} variant='outline' shape='tag'>{item.name}</Badge>)}
            </div>
          </div>
        ) : null}
      </Section>

      {results.length ? (
        <Section title={t('上次操作结果')}>
          <div className='mm-result-list'>
            {results.map(row => (
              <div key={row.id} className='mm-result-row'>
                <StateBadge state={row.ok ? 'ok' : 'bad'} label={row.ok ? t('成功') : t('失败')} />
                <strong>{row.label}</strong>
                <span className='mm-muted'>
                  {row.ok
                    ? [row.mcp, row.instruction].filter(Boolean).join(' · ') || t('已写入')
                    : row.error ?? t('未知错误')}
                </span>
              </div>
            ))}
          </div>
        </Section>
      ) : null}

      {verify ? (
        <Section title={t('校验结果（{ok} / {total}）', { ok: verify.ok, total: verify.total })}>
          <div className='mm-result-list'>
            {verify.rows.map((row, index) => (
              <div key={index} className='mm-result-row'>
                <StateBadge state={row.ok ? 'ok' : 'bad'} label={row.ok ? t('正常') : t('有问题')} />
                <strong>{String(row.label ?? row.id ?? index)}</strong>
                <span className='mm-muted'>{String(row.detail ?? row.message ?? '')}</span>
              </div>
            ))}
          </div>
        </Section>
      ) : null}

      <Section
        title={t('配置片段')}
        hint={t('给不在这份清单里的客户端，或者你想手动粘贴时用。')}
        action={(
          <Button type='button' size='sm' variant='ghost' onClick={() => void act(async () => {
            setSnippet(await m.loadAgentSnippet())
          })}>
            {t('生成片段')}
          </Button>
        )}
      >
        {snippet ? (
          <div className='mm-snippet'>
            <div className='mm-section-title'>mcpServers（JSON）</div>
            <pre className='mm-body'>{JSON.stringify(snippet.json, null, 2)}</pre>
            <div className='mm-section-title'>config.toml（Codex / DSH 形态）</div>
            <pre className='mm-body'>{snippet.toml}</pre>
            <div className='mm-section-title'>{t('建议写进 AGENTS.md 的约定')}</div>
            <pre className='mm-body'>{snippet.instruction}</pre>
          </div>
        ) : <div className='mm-muted'>{t('点「生成片段」看当前端点的配置。')}</div>}
      </Section>
    </div>
  )
}

export { Agents }
