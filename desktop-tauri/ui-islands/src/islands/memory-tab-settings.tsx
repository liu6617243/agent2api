import * as React from 'react'
import { Badge, Button, Input, Switch } from '@ui'
import { t } from '../i18n'
import * as m from './memory-api'
import { Empty, Panel, Section, StateBadge, Grid, useLoad } from './memory-shared'

/**
 * 配置页：schema 驱动的自动表单，三档折叠。
 *
 * ── 为什么是「schema 驱动」而不是手写表单 ────────────────────
 * 后端有一百多个旋钮（`config.rs` 的 SCHEMA 表）。前端手抄一份枚举的代价不是
 * 「多写一次」，是**必然漂移**：后端加了键，前端不知道，用户在页面上永远改不到
 * 它；后端删了键，前端还在提交一个不存在的键，静默失效。把 schema 取回来按
 * `kind` 渲染、按 `tier` 折叠，前端就永远不需要知道「有哪些旋钮」。
 *
 * ── 三档是什么意思 ──────────────────────────────────────────
 *   · basic    —— 日常要改的（开关、预算、保留天数）。默认展开。
 *   · advanced —— 调行为偏好的（阈值、权重档、同步策略）。默认折叠。
 *   · internal —— 排障与深度定制（具体权重数值、prompt 覆盖、正则）。默认折叠且
 *                 带一句「改之前先知道自己在改什么」。
 *
 * 分档不是权限，是**认知负荷**：一百个旋钮平铺等于没有旋钮。
 *
 * ── 为什么每个键单独提交而不是整表单提交 ────────────────────
 * 一次改一个键，出错能立刻定位到是哪个键；整表单提交时后端只回一个错误，
 * 用户得自己猜是哪一栏不合法。而且「改完即生效」比「填完点保存」更符合这类
 * 调参页的使用方式（改一下、回去看看效果、再改）。
 */

type Tier = 'basic' | 'advanced' | 'internal'

function Settings({ onChanged }: { onChanged: () => void }): React.ReactElement {
  const { data, loading, error, reload } = useLoad(() => m.loadConfig(), [])
  const [open, setOpen] = React.useState<Record<Tier, boolean>>({
    basic: true,
    advanced: false,
    internal: false,
  })
  const [busy, setBusy] = React.useState('')
  const [actionError, setActionError] = React.useState('')
  const [saved, setSaved] = React.useState('')

  const save = async (key: string, value: unknown) => {
    setBusy(key)
    setActionError('')
    setSaved('')
    try {
      await m.saveConfig({ [key]: value })
      setSaved(key)
      reload()
      onChanged()
      window.setTimeout(() => setSaved(current => current === key ? '' : current), 1600)
    } catch (caught) {
      setActionError(caught instanceof Error ? caught.message : String(caught))
    } finally {
      setBusy('')
    }
  }

  if (loading) return <Panel loading error=''>{null}</Panel>
  if (error) return <Panel loading={false} error={error} onRetry={reload}>{null}</Panel>
  if (!data) return <Empty>{t('读不到配置。')}</Empty>

  const groups: string[] = []
  for (const knob of data.schema) {
    if (!groups.includes(knob.group)) groups.push(knob.group)
  }

  return (
    <div className='mm-settings-page'>
      {actionError ? <div className='mm-error'>{actionError}</div> : null}

      <Section
        title={t('记忆服务')}
        hint={t('关掉总开关后，Agent 的读写类工具一律被拒绝，自检仍可用。用途是临时让 Agent 忘记，不必去每个客户端逐个卸载。')}
      >
        <div className='mm-field'>
          <label>
            {t('启用记忆中枢')}
            <Switch
              checked={data.config.enabled}
              onCheckedChange={value => void save('enabled', value)}
            />
          </label>
        </div>
        <div className='mm-field'>
          <label>{t('核心记忆默认预算（token）')}</label>
          <Input
            type='number'
            min={200}
            max={4000}
            value={String(data.config.coreMaxTokens)}
            onChange={event => {
              const value = Number(event.target.value)
              if (!Number.isFinite(value) || value < 200 || value > 4000) return
              void save('agents.coreMaxTokens', Math.round(value))
            }}
          />
          <p className='mm-hint'>
            {t('200–4000。Agent 调 memory_core 时不传 maxTokens 就用这个值；超预算的条目整条丢掉，不截半句。')}
          </p>
        </div>
      </Section>

      <Section
        title={t('全部旋钮（{n} 个）', { n: data.schema.length })}
        hint={t('按分组列出。改一个键立刻生效，不用保存整张表。')}
        action={(
          <div className='mm-inline-actions'>
            {(['basic', 'advanced', 'internal'] as Tier[]).map(tier => (
              <Button
                key={tier}
                type='button'
                size='sm'
                variant={open[tier] ? 'default' : 'ghost'}
                onClick={() => setOpen(current => ({ ...current, [tier]: !current[tier] }))}
              >
                {tierLabel(tier)}
              </Button>
            ))}
          </div>
        )}
      >
        {(['basic', 'advanced', 'internal'] as Tier[]).map(tier => {
          if (!open[tier]) return null
          const knobs = data.schema.filter(knob => knob.tier === tier)
          if (knobs.length === 0) return null
          return (
            <div key={tier} className='mm-tier'>
              <div className='mm-tier-head'>
                <StateBadge
                  state={tier === 'basic' ? 'ok' : tier === 'advanced' ? 'warn' : 'idle'}
                  label={tierLabel(tier)}
                />
                <span className='mm-muted'>{tierHint(tier)}</span>
              </div>
              {groups.map(group => {
                const items = knobs.filter(knob => knob.group === group)
                if (items.length === 0) return null
                return (
                  <div key={group} className='mm-group'>
                    <div className='mm-group-title'>{group}</div>
                    {items.map(knob => (
                      <KnobField
                        key={knob.key}
                        knob={knob}
                        value={data.values[knob.key]}
                        busy={busy === knob.key}
                        saved={saved === knob.key}
                        onSave={value => void save(knob.key, value)}
                      />
                    ))}
                  </div>
                )
              })}
            </div>
          )
        })}
      </Section>

      <Section title={t('状态')} hint={t('这些是只读的运行时事实。')}>
        <Grid
          rows={[
            [t('记忆根'), data.status.root],
            [t('索引文件'), data.status.indexFile],
            [t('设备 id'), data.status.device],
            [t('条数'), `${data.status.live} 有效 / ${data.status.deleted} 回收站 / ${data.status.superseded} 已取代`],
            [t('索引'), `${data.status.index.count} 条 · ${data.status.index.fts ? 'FTS5' : t('内存匹配')} · ${data.status.index.inSync ? t('一致') : t('待重建')}`],
            [t('内容版本'), `r${data.status.revision}`],
          ]}
        />
        <div className='mm-inline-actions'>
          <Button
            type='button'
            size='sm'
            variant='ghost'
            onClick={() => void (async () => {
              try {
                await m.resetConfig()
                reload()
                onChanged()
              } catch (caught) {
                setActionError(caught instanceof Error ? caught.message : String(caught))
              }
            })()}
          >
            {t('全部恢复默认')}
          </Button>
          <span className='mm-muted'>{t('会把 config.json 里的自定义值全部清掉（不动记忆本身）。')}</span>
        </div>
      </Section>
    </div>
  )
}

function tierLabel(tier: Tier): string {
  if (tier === 'basic') return t('常用')
  if (tier === 'advanced') return t('进阶')
  return t('内部')
}

function tierHint(tier: Tier): string {
  if (tier === 'basic') return t('日常会改的：开关、预算、保留天数。')
  if (tier === 'advanced') return t('调行为偏好：阈值、权重档、同步策略。改错了可以恢复默认。')
  return t('具体权重数值、prompt 覆盖、正则。改之前先知道自己在改什么。')
}

/** 一个旋钮 → 一个控件。按 `kind` 分发，这是「前端不需要知道有哪些旋钮」的落点。 */
function KnobField({
  knob,
  value,
  busy,
  saved,
  onSave,
}: {
  knob: m.Knob
  value: unknown
  busy: boolean
  saved: boolean
  onSave: (value: unknown) => void
}): React.ReactElement {
  const current = value ?? knob.default

  return (
    <div className='mm-knob'>
      <div className='mm-knob-label'>
        <span>{knob.label}</span>
        <code className='mm-knob-key'>{knob.key}</code>
        {saved ? <Badge variant='success' shape='tag'>{t('已保存')}</Badge> : null}
        {busy ? <Badge variant='outline' shape='tag'>{t('保存中')}</Badge> : null}
        {knob.hot ? <Badge variant='warning' shape='tag'>{t('需重启')}</Badge> : null}
      </div>
      <div className='mm-knob-control'>
        <KnobControl knob={knob} value={current} onSave={onSave} />
      </div>
      {knob.desc ? <div className='mm-knob-desc'>{knob.desc}</div> : null}
    </div>
  )
}

function KnobControl({
  knob,
  value,
  onSave,
}: {
  knob: m.Knob
  value: unknown
  onSave: (value: unknown) => void
}): React.ReactElement {
  switch (knob.kind) {
    case 'boolean':
      return <Switch checked={value === true} onCheckedChange={next => onSave(next)} />

    case 'number':
      return (
        <Input
          type='number'
          className='mm-knob-number'
          min={knob.min}
          max={knob.max}
          value={String(value ?? '')}
          onChange={event => {
            const next = Number(event.target.value)
            if (!Number.isFinite(next)) return
            onSave(next)
          }}
        />
      )

    case 'enum':
      return (
        <select
          className='mm-select'
          value={String(value ?? '')}
          onChange={event => onSave(event.target.value)}
        >
          {(knob.options ?? []).map(option => (
            <option key={option} value={option}>{option}</option>
          ))}
        </select>
      )

    case 'multiselect': {
      const picked = Array.isArray(value) ? value.map(String) : []
      return (
        <div className='mm-multi'>
          {(knob.options ?? []).map(option => {
            const on = picked.includes(option)
            return (
              <button
                key={option}
                type='button'
                className={`mm-chip${on ? ' on' : ''}`}
                onClick={() => onSave(on ? picked.filter(item => item !== option) : [...picked, option])}
              >
                {option}
              </button>
            )
          })}
        </div>
      )
    }

    case 'list':
    case 'map':
      return <JsonKnob value={value} onSave={onSave} />

    case 'path':
    case 'string':
    default:
      return (
        <Input
          value={typeof value === 'string' ? value : ''}
          placeholder={knob.default === null ? '' : String(knob.default)}
          onChange={event => onSave(event.target.value)}
        />
      )
  }
}

/**
 * 结构化值（数组 / 对象）用 JSON 文本框。
 *
 * 为什么不做一个结构化编辑器：这些键的值形状千差万别（来源清单是字符串数组、
 * 模型池是对象数组、权重是对象），给每种形状写一个编辑器等于把 schema 的知识
 * 又抄一遍到前端。JSON 文本框加「解析失败就不提交」是这一层的正确粒度 ——
 * 这些本来就是内部/进阶旋钮。
 */
function JsonKnob({ value, onSave }: { value: unknown; onSave: (value: unknown) => void }): React.ReactElement {
  const text = React.useMemo(() => {
    try {
      return JSON.stringify(value ?? null, null, 2)
    } catch {
      return 'null'
    }
  }, [value])
  const [draft, setDraft] = React.useState(text)
  const [invalid, setInvalid] = React.useState(false)
  const [dirty, setDirty] = React.useState(false)

  React.useEffect(() => {
    if (!dirty) setDraft(text)
  }, [text, dirty])

  return (
    <div className='mm-json'>
      <textarea
        className={`mm-json-input${invalid ? ' invalid' : ''}`}
        rows={Math.min(10, Math.max(2, draft.split('\n').length))}
        value={draft}
        onChange={event => {
          setDraft(event.target.value)
          setDirty(true)
          try {
            JSON.parse(event.target.value)
            setInvalid(false)
          } catch {
            setInvalid(true)
          }
        }}
      />
      <div className='mm-json-actions'>
        <Button
          type='button'
          size='sm'
          disabled={invalid}
          onClick={() => {
            try {
              onSave(JSON.parse(draft))
              setDirty(false)
            } catch {
              setInvalid(true)
            }
          }}
        >
          {t('应用')}
        </Button>
        <Button
          type='button'
          size='sm'
          variant='ghost'
          onClick={() => {
            setDraft(text)
            setDirty(false)
            setInvalid(false)
          }}
        >
          {t('还原')}
        </Button>
        {invalid ? <span className='mm-warn'>{t('JSON 不合法，改对了才能应用')}</span> : null}
      </div>
    </div>
  )
}

export { Settings }
