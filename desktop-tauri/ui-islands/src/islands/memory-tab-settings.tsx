import * as React from 'react'
import { Badge, Button, Input, Switch } from '@ui'
import { t } from '../i18n'
import * as m from './memory-api'
import { Empty, Panel, useLoad } from './memory-shared'

/**
 * 配置页：模块与存储 + 按标签的路由 + 按分组的旋钮。
 *
 * ── 为什么有「草稿」这一层 ──────────────────────────────────
 * 原来每改一个旋钮就 `PUT` 一次。调一屏参数（比如把检索权重试三组）等于
 * 连续写十几次 config.json，中间任何一次都是「已经生效的半成品」；而且没有
 * 「我刚才改了什么」的回退点 —— 页头的「已同步 / N 项未保存」也就无从谈起。
 *
 * 现在改动先进本地 `draft`，点「保存配置」一次性 `PUT`。刻意**不做**自动保存：
 * 这个页面里有 `storage.root`（换根目录）这种改错就要重启并手工搬数据的键，
 * 自动落盘会让「手滑」直接变成事故。
 *
 * ── 为什么分组导航是一排 chip 而不是左栏 ────────────────────
 * 十六个分组，左栏会占掉 200px 宽，而每个分组最多十几个旋钮 —— 左栏比内容还宽。
 * 横排 chip 可以换行、不占常驻宽度，切分组时正文原地换掉，视线不动。
 *
 * ── 哪些东西是「按当前配置推导」而不是「读回来的」────────────
 * 路由表最右列的「将使用」是前端按 `llm::model_for` 的同一套规则算的（指定 →
 * 标签 → 兜底 → 目录首个）。后端没有「解析给我看」的接口，前端复算一遍是唯一
 * 不新增接口的做法，所以它在界面上明确标成「推导」—— 真实生效结果以调用台账
 * 为准。改 `llm.rs` 的选模型规则时，这里的 `resolveModel` 要跟着改。
 */

/** 真正会调 LLM 的任务。`classify` / `dedup` 是纯本地规则，不在这张表里。 */
/**
 * 路由表里的任务行 —— 与 `schedule.rs` 的 `TASKS` 对齐，一个不漏。
 *
 * `llm: false` 的任务**确实存在、也确实在跑**，但走本地规则（0 token），
 * 没有「指定模型 / 标签 / 思考强度」可配。这种行把三列合成一句说明，
 * 而不是摆三个禁用下拉装作能配。
 *
 * `todo: true` 是后端还没有实现的任务，单独标出来，不留空壳控件。
 */
const ROUTE_TASKS: {
  task: string
  label: string
  llm: boolean
  note?: string
  todo?: boolean
}[] = [
  { task: 'extract', label: '提取', llm: true },
  { task: 'summarize', label: '摘要', llm: true },
  { task: 'tag', label: '打标', llm: true },
  {
    task: 'classify',
    label: '分类',
    llm: false,
    note: '本地规则：slug 模糊匹配 + Git 地址 / 目录名，0 token',
  },
  { task: 'supersede', label: '失效判定', llm: true },
  { task: 'distill', label: '蒸馏', llm: true },
  { task: 'consolidate', label: '合并', llm: true },
  { task: 'profile', label: '画像', llm: true },
  {
    task: 'dedup',
    label: '去重',
    llm: false,
    note: '本地规则：L1 哈希 + L2 文本近似（L3/L4 未实现）',
  },
  {
    task: 'optimize',
    label: '提示词优化',
    llm: false,
    todo: true,
    note: '后端还没有这个任务',
  },
]

const EFFORT_OPTIONS: { value: string; label: string }[] = [
  { value: '', label: '用模型默认' },
  { value: 'minimal', label: '最小（minimal）' },
  { value: 'low', label: '低（low）' },
  { value: 'medium', label: '中（medium）' },
  { value: 'high', label: '高（high）' },
]

/** 分组 → 图标（`ui/icons.js` 里的键）。没列的用 info 兜底。 */
const GROUP_ICONS: Record<string, string> = {
  存储: 'database',
  索引: 'sort',
  检索: 'eye',
  归类: 'route',
  'Agent 接入': 'agent',
  深层记忆: 'memory',
  双时间轴: 'timer',
  自动化: 'spark',
  去重: 'traffic',
  待确认: 'feedback',
  清理: 'refresh',
  模型与网关: 'gateway',
  请求级注入: 'memory',
  导入: 'download',
  隐私: 'shield',
  同步: 'refresh',
  界面: 'display',
}

type RouteRow = { task: string; tags: string[]; model: string }

/** 「请求级注入」那一组。下面要把预览卡片挂到这一组下面。 */
const INJECT_GROUP = '请求级注入'

function Settings({ onChanged }: { onChanged: () => void }): React.ReactElement {
  const { data, loading, error, reload } = useLoad(() => m.loadConfig(), [])
  const [draft, setDraft] = React.useState<Record<string, unknown>>({})
  const [category, setCategory] = React.useState('')
  const [advancedOpen, setAdvancedOpen] = React.useState<Record<string, boolean>>({})
  const [busy, setBusy] = React.useState('')
  const [actionError, setActionError] = React.useState('')
  const [confirmReset, setConfirmReset] = React.useState(false)
  const fileRef = React.useRef<HTMLInputElement | null>(null)

  const groups = React.useMemo(() => {
    const seen: string[] = []
    for (const knob of data?.schema ?? []) {
      if (!seen.includes(knob.group)) seen.push(knob.group)
    }
    return seen
  }, [data])

  /**
   * 接住别的页签发来的「打开某个分组」请求（`mm:group`）。
   *
   * 同步页的「修改凭据」就是这么跳过来的 —— 它要去的不是配置页首页，而是
   * 「同步」那一组旋钮。分组是组件内部状态，跨页签只能靠事件。
   *
   * 必须放在下面的提前 return **之前**：Hook 的调用顺序不能随数据加载状态变化，
   * 放在 return 后面会在「先 loading 后 loaded」时触发 "rendered fewer hooks"。
   */
  React.useEffect(() => {
    const onGroup = (event: Event) => {
      const next = (event as CustomEvent<{ group?: string }>).detail?.group
      if (next) setCategory(next)
    }
    window.addEventListener('mm:group', onGroup)
    return () => window.removeEventListener('mm:group', onGroup)
  }, [])

  if (loading) return <Panel loading error=''>{null}</Panel>
  if (error) return <Panel loading={false} error={error} onRetry={reload}>{null}</Panel>
  if (!data) return <Empty>{t('读不到配置。')}</Empty>

  const active = category && groups.includes(category) ? category : groups[0] ?? ''
  const saved = data.values
  const defaultOf = (key: string) => data.schema.find(item => item.key === key)?.default

  /** 生效值：草稿 > 已保存 > 默认。 */
  const value = (key: string): unknown => {
    if (key in draft) return draft[key]
    if (key in saved) return saved[key]
    return defaultOf(key)
  }

  /**
   * 基线值（用来判「有没有改过」）。
   *
   * `enabled` 不在 schema 里，也不在 `values` 里 —— 它是 `config.enabled` 这个
   * 独立字段（后端 `patch` 单独处理它）。不特殊对待的话，把它拨回原位也算「未保存」，
   * 页头的计数永远清不掉。
   */
  const baseOf = (key: string): unknown => {
    if (key === 'enabled') return data.config.enabled
    return key in saved ? saved[key] : defaultOf(key)
  }

  const isDirty = (key: string) => key in draft && JSON.stringify(draft[key]) !== JSON.stringify(baseOf(key))

  const dirtyKeys = Object.keys(draft).filter(isDirty)

  const put = (key: string, next: unknown) => {
    setActionError('')
    setDraft(current => ({ ...current, [key]: next }))
  }

  const commit = async () => {
    const patch: Record<string, unknown> = {}
    for (const key of dirtyKeys) patch[key] = draft[key]
    if (Object.keys(patch).length === 0) return
    setBusy('commit')
    setActionError('')
    try {
      await m.saveConfig(patch)
      setDraft({})
      reload()
      onChanged()
    } catch (caught) {
      setActionError(caught instanceof Error ? caught.message : String(caught))
    } finally {
      setBusy('')
    }
  }

  const resetAll = async () => {
    setConfirmReset(false)
    setBusy('reset')
    setActionError('')
    try {
      await m.resetConfig()
      setDraft({})
      reload()
      onChanged()
    } catch (caught) {
      setActionError(caught instanceof Error ? caught.message : String(caught))
    } finally {
      setBusy('')
    }
  }

  const reloadOnly = () => {
    setDraft({})
    setActionError('')
    reload()
  }

  /**
   * 导出：把「生效值」整份写成 JSON 文件。
   * 导的是值不是 schema —— 导入方拿到的就是可以直接 PUT 回去的形状。
   */
  const exportJson = () => {
    const payload: Record<string, unknown> = { enabled: data.config.enabled }
    for (const knob of data.schema) payload[knob.key] = value(knob.key)
    const blob = new Blob([JSON.stringify(payload, null, 2)], { type: 'application/json' })
    const url = URL.createObjectURL(blob)
    const anchor = document.createElement('a')
    anchor.href = url
    anchor.download = 'memory.config.json'
    anchor.click()
    URL.revokeObjectURL(url)
  }

  /** 导入：只认 schema 里有的键，其余静默丢掉（老版本导出的文件里可能有已删的键）。 */
  const importJson = async (file: File) => {
    setActionError('')
    try {
      const text = await file.text()
      const parsed = JSON.parse(text) as Record<string, unknown>
      const known = new Set(data.schema.map(knob => knob.key))
      const next: Record<string, unknown> = {}
      for (const [key, item] of Object.entries(parsed)) {
        if (key === 'enabled') continue
        if (!known.has(key)) continue
        next[key] = item
      }
      if (Object.keys(next).length === 0) {
        setActionError(t('这份 JSON 里没有认识的配置键。'))
        return
      }
      setDraft(current => ({ ...current, ...next }))
    } catch (caught) {
      setActionError(caught instanceof Error ? caught.message : t('JSON 解析失败'))
    }
  }

  const items = data.schema.filter(knob => knob.group === active)
  const basic = items.filter(knob => knob.tier === 'basic')
  const advanced = items.filter(knob => knob.tier !== 'basic')
  const advOpen = advancedOpen[active] ?? false
  const enabledNow = 'enabled' in draft ? draft.enabled === true : data.config.enabled

  return (
    <div className='mm-cfg'>
      {/* ── 页头 ── */}
      <div className='mm-cfg-head'>
        <div className='mm-cfg-head-text'>
          <div className='mm-cfg-title'>{t('配置 · 记忆中枢')}</div>
          <div className='mm-cfg-sub'>
            {t('配置事实源：{path}（随 WebDAV 同步）', {
              path: `${data.status.root.replace(/[\\/]+$/, '')}/config.json`,
            })}
          </div>
        </div>
        <div className='mm-cfg-head-actions'>
          <Badge variant={dirtyKeys.length > 0 ? 'warning' : 'success'} shape='tag'>
            {dirtyKeys.length > 0 ? t('{n} 项未保存', { n: dirtyKeys.length }) : t('已同步')}
          </Badge>
          <Button type='button' size='sm' variant='outline' disabled={busy !== ''} onClick={() => setConfirmReset(true)}>
            {t('恢复默认')}
          </Button>
          <Button
            type='button'
            size='sm'
            variant='default'
            disabled={dirtyKeys.length === 0 || busy !== ''}
            onClick={() => void commit()}
          >
            {busy === 'commit' ? t('保存中…') : t('保存配置')}
          </Button>
        </div>
      </div>

      {actionError ? <div className='mm-error'>{actionError}</div> : null}

      {confirmReset ? (
        <div className='mm-cfg-confirm'>
          <span>{t('会把 config.json 里的自定义值全部清掉（记忆本身不动）。确定恢复默认？')}</span>
          <div className='mm-inline-actions'>
            <Button type='button' size='sm' variant='destructive' onClick={() => void resetAll()}>
              {t('确认恢复')}
            </Button>
            <Button type='button' size='sm' variant='ghost' onClick={() => setConfirmReset(false)}>
              {t('取消')}
            </Button>
          </div>
        </div>
      ) : null}

      {/* ── 模块与存储 ── */}
      <section className='mm-cfg-card'>
        <div className='mm-cfg-card-head'>
          <div className='mm-cfg-card-title'>{t('模块与存储')}</div>
          <div className='mm-cfg-card-note'>{t('根目录改动需重启，旧目录里的文件不会自动搬')}</div>
        </div>

        <div className='mm-cfg-item'>
          <div className='mm-cfg-item-label'>{t('启用状态')}</div>
          <div className='mm-cfg-item-body'>
            <div className='mm-cfg-item-control'>
              <Switch checked={enabledNow} onCheckedChange={next => put('enabled', next)} />
            </div>
            <div className='mm-cfg-item-desc'>{enabledNow ? t('已启用') : t('已关闭')}</div>
          </div>
        </div>

        <div className='mm-cfg-item'>
          <div className='mm-cfg-item-label'>{t('当前根目录')}</div>
          <div className='mm-cfg-item-body'>
            <div className='mm-cfg-item-control mm-cfg-path'>
              <code>{data.status.root}</code>
              <Button
                type='button'
                size='sm'
                variant='ghost'
                onClick={() => void navigator.clipboard?.writeText(data.status.root)}
              >
                {t('复制路径')}
              </Button>
            </div>
            <div className='mm-cfg-item-desc'>
              {t('在下方「存储 → 记忆根目录」里改。改完需重启，旧目录的文件不会自动搬。')}
            </div>
          </div>
        </div>

        <div className='mm-cfg-item'>
          <div className='mm-cfg-item-label'>{t('配置备份')}</div>
          <div className='mm-cfg-item-body'>
            <div className='mm-cfg-item-control'>
              <Button type='button' size='sm' variant='outline' onClick={exportJson}>
                {t('导出 JSON')}
              </Button>
              <Button type='button' size='sm' variant='outline' onClick={() => fileRef.current?.click()}>
                {t('导入 JSON')}
              </Button>
              <input
                ref={fileRef}
                type='file'
                accept='.json,application/json'
                hidden
                onChange={event => {
                  const file = event.target.files?.[0]
                  if (file) void importJson(file)
                  event.target.value = ''
                }}
              />
            </div>
            <div className='mm-cfg-item-desc'>
              {t('导出的是当前生效值。导入只认本页认识的键，其余丢掉；进草稿后仍需点「保存配置」。')}
            </div>
          </div>
        </div>
      </section>

      {/* ── 供应商提示 ── */}
      <div className='mm-cfg-banner'>
        <span>{t('供应商与模型池在「模型管理」里维护；本页只保留记忆中枢自己的路由规则。')}</span>
        <Button
          type='button'
          size='sm'
          variant='outline'
          onClick={() =>
            (window as unknown as { wbApp?: { showPage?: (name: string) => void } })
              .wbApp?.showPage?.('gateway')
          }
        >
          {t('前往设置 →')}
        </Button>
      </div>

      {/* ── 路由表 ── */}
      <section className='mm-cfg-card'>
        <div className='mm-cfg-card-head'>
          <div className='mm-cfg-card-title'>{t('按标签的路由与降级链')}</div>
          <div className='mm-cfg-card-actions'>
            <Button type='button' size='sm' variant='ghost' onClick={reloadOnly}>
              {t('刷新链路')}
            </Button>
          </div>
        </div>
        <div className='mm-cfg-card-sub'>
          {t('每行可指定模型；不指定则按标签在模型池里挑。记忆任务固定走本机网关。')}
        </div>

        <RoutingTable
          routing={toRouting(value('models.routing'))}
          effort={toMap(value('models.taskEffort'))}
          pool={toPool(value('models.models'))}
          fallback={String(value('models.fallbackModel') ?? '')}
          onChange={(task, patch) => {
            const rows = toRouting(value('models.routing'))
            const current = rows.find(row => row.task === task)
            const next = rows.filter(row => row.task !== task)
            const merged: RouteRow = {
              task,
              tags: patch.tags ?? current?.tags ?? [],
              model: patch.model ?? current?.model ?? '',
            }
            // 既没指定模型也没标签的条目没有意义，落盘时不要留空壳
            if (merged.model.trim() || merged.tags.length > 0) next.push(merged)
            put('models.routing', next)
          }}
          onEffortChange={(task, level) => {
            // 不能「删键」：后端对 Map 类型做深合并，删掉的键会被旧值留下。
            // 写空串等价于「没配」—— `model_for` 与 `complete` 都把空串当未设置。
            put('models.taskEffort', { ...toMap(value('models.taskEffort')), [task]: level })
          }}
        />

        <div className='mm-cfg-note'>
          <div>
            {t('链路怎么看：先看是否指定了模型；没指定就按【思考强度】推出标签（高 → heavy、中 → light），在同标签的模型池里取第一个；都没有就用兜底模型；再没有就取模型目录里第一个对话模型。401/403 会立刻换下一个，429/5xx 由网关退避重试。')}
          </div>
          <div>
            {t('分类与去重走本地规则、不消耗 token，所以那两行没有模型可配；提示词优化这个任务后端还没有实现。')}
          </div>
        </div>
      </section>

      {/* ── 分组导航 ── */}
      <nav className='mm-cfg-nav'>
        {groups.map(group => (
          <button
            key={group}
            type='button'
            className={`mm-cfg-nav-item${group === active ? ' on' : ''}`}
            onClick={() => setCategory(group)}
          >
            <span
              className='mm-cfg-nav-icon'
              dangerouslySetInnerHTML={{ __html: iconHtml(GROUP_ICONS[group] ?? 'info') }}
            />
            {t(group)}
          </button>
        ))}
      </nav>

      {/* ── 当前分组的旋钮 ── */}
      <section className='mm-cfg-card'>
        <div className='mm-cfg-card-head'>
          <div className='mm-cfg-card-title'>{t(active)}</div>
          <div className='mm-cfg-card-actions'>
            <span className='mm-cfg-card-note'>
              {t('{n} 项（改完点「保存配置」生效，标「需重启」的还要重启或重建索引）', { n: items.length })}
            </span>
            {advanced.length > 0 ? (
              <Button
                type='button'
                size='sm'
                variant='ghost'
                onClick={() => setAdvancedOpen(current => ({ ...current, [active]: !advOpen }))}
              >
                {advOpen
                  ? t('高级项 {n} ▲ 收起', { n: advanced.length })
                  : t('高级项 {n} ▼ 展开', { n: advanced.length })}
              </Button>
            ) : null}
          </div>
        </div>

        {[...basic, ...(advOpen ? advanced : [])].map(knob => (
          <KnobRow
            key={knob.key}
            knob={knob}
            value={value(knob.key)}
            dirty={isDirty(knob.key)}
            onChange={next => put(knob.key, next)}
            onDefault={() => put(knob.key, knob.default)}
          />
        ))}

        {items.length === 0 ? <div className='mm-cfg-empty'>{t('这一组还没有旋钮。')}</div> : null}
      </section>

      {/* ── 请求级注入：把「这次会塞什么」摊开给用户看 ──
          注入是用户看不见的动作。没有这块，他改了预算、钉了记忆、写了避坑，
          只能靠「模型行为好像变了」去猜有没有生效 —— 猜不出就会来问，而回答
          「应该生效了」没有任何说服力。 */}
      {active === INJECT_GROUP ? <InjectPreview /> : null}
    </div>
  )
}

/* ─── 请求级注入预览 ─────────────────────────────────────── */

/**
 * 一个输入框 + 一句「像用户会说的话」→ 网关这次会注入什么。
 *
 * 走的接口与真实转发同一条函数（`inject::compose`），所以看到的就是线上会
 * 发生的，不是另一套演示逻辑。
 */
function InjectPreview(): React.ReactElement {
  const [probe, setProbe] = React.useState('')
  const [result, setResult] = React.useState<m.InjectPreview | null>(null)
  const [busy, setBusy] = React.useState(false)
  const [error, setError] = React.useState('')

  const run = (query: string) => {
    setBusy(true)
    setError('')
    m.loadInjectPreview(query)
      .then(value => setResult(value))
      .catch((caught: unknown) => setError(caught instanceof Error ? caught.message : String(caught)))
      .finally(() => setBusy(false))
  }

  return (
    <section className='mm-cfg-card'>
      <div className='mm-cfg-card-head'>
        <div className='mm-cfg-card-title'>{t('试一下：这条请求会带上哪些记忆')}</div>
        <div className='mm-cfg-card-actions'>
          <Button type='button' size='sm' variant='ghost' disabled={busy} onClick={() => run(probe)}>
            {busy ? t('正在算…') : t('算一遍')}
          </Button>
        </div>
      </div>
      <div className='mm-cfg-card-sub'>
        {t('贴一句你真会说的话（带上文件名、函数名、命令名效果最好）。网关按这些线索去记忆里找命中，命不中就什么都不注入 —— 宁可不给，也不给错的。')}
      </div>
      <Input
        value={probe}
        placeholder={t('例：改一下 desktop-tauri/src-tauri/server/src/server/api/chat.rs 里的 inject_chat')}
        onChange={event => setProbe(event.target.value)}
        onKeyDown={event => { if (event.key === 'Enter') run(probe) }}
      />
      {error ? <div className='mm-cfg-card-note' style={{ color: 'var(--danger, #d33)' }}>{error}</div> : null}
      {result ? (
        <>
          <div className='mm-cfg-card-note'>
            {!result.enabled
              ? t('注入开关是关的：这段内容现在不会进任何请求。')
              : result.text
                ? t('会注入 {n} token（上限 {max}）· 抽出 {k} 个线索', {
                    n: result.tokens,
                    max: result.settings.maxTokens,
                    k: result.probes.length,
                  })
                : t('没有命中：这次不会注入任何记忆。')}
          </div>
          {result.probes.length > 0 ? (
            <div className='mm-cfg-card-note'>
              {t('线索：')}
              {result.probes.map(item => ` ${item}`).join(' · ')}
            </div>
          ) : null}
          {/* 候选与门槛：解释「为什么没注入」。
              pass=false → 线索不够具体（该补记忆或换说法）；
              pass=true 却没出现在下面那段原文里 → 被预算挤掉（该调大经验预算）。
              没有这块时，「没命中」和「命中了但没塞进去」在界面上长得一模一样。 */}
          {result.hits.length > 0 ? (
            <div style={{ display: 'grid', gap: 4 }}>
              <div className='mm-cfg-card-note'>{t('候选（按得分）')}</div>
              {result.hits.map(hit => (
                <div
                  key={hit.id}
                  style={{
                    display: 'flex',
                    gap: 8,
                    alignItems: 'baseline',
                    fontSize: 11.5,
                    lineHeight: 1.6,
                    color: hit.pass ? 'var(--text-2)' : 'var(--text-3, var(--text-2))',
                    opacity: hit.pass ? 1 : 0.62,
                  }}
                >
                  <code style={{ flex: '0 0 auto' }}>{hit.score.toFixed(1)}</code>
                  <span style={{ flex: '0 0 auto', opacity: 0.7 }}>
                    /{hit.floor.toFixed(1)} · {hit.layer}
                  </span>
                  <span style={{ flex: '1 1 auto', minWidth: 0, overflow: 'hidden', textOverflow: 'ellipsis', whiteSpace: 'nowrap' }}>
                    {hit.title}
                  </span>
                  <span style={{ flex: '0 0 auto', opacity: 0.7 }}>
                    {hit.pass ? t('过线') : t('差一点')}
                  </span>
                </div>
              ))}
            </div>
          ) : null}
          {result.text ? (
            <pre
              style={{
                margin: 0,
                padding: '10px 12px',
                maxHeight: 260,
                overflow: 'auto',
                whiteSpace: 'pre-wrap',
                wordBreak: 'break-word',
                fontSize: 11.5,
                lineHeight: 1.7,
                border: '1px solid var(--border)',
                borderRadius: 'var(--r-sm)',
                background: 'var(--surface-2, transparent)',
                color: 'var(--text-2)',
              }}
            >
              {result.text}
            </pre>
          ) : null}
        </>
      ) : null}
      <div className='mm-cfg-card-sub'>
        {t('注入内容永远是「历史陈述」：块头写明了「不代表当前代码状态，改文件前先读文件」。模型自己深挖时走 MCP 工具，两条路并存。')}
      </div>
    </section>
  )
}

/* ─── 路由表 ─────────────────────────────────────────────── */

function RoutingTable({
  routing,
  effort,
  pool,
  fallback,
  onChange,
  onEffortChange,
}: {
  routing: RouteRow[]
  effort: Record<string, string>
  pool: { model: string; tags: string[] }[]
  fallback: string
  onChange: (task: string, patch: { model?: string; tags?: string[] }) => void
  onEffortChange: (task: string, level: string) => void
}): React.ReactElement {
  // 本地模型池里的名字，只用来给输入框出候选提示 —— 不是可选项的边界。
  const unique = Array.from(new Set(pool.map(item => item.model).filter(Boolean)))

  return (
    <div className='mm-route'>
      <div className='mm-route-row mm-route-head'>
        <span>{t('任务')}</span>
        <span>{t('绑定网关 / 供应商')}</span>
        <span>{t('指定模型')}</span>
        <span>{t('标签')}</span>
        <span>{t('思考强度')}</span>
        <span>{t('将使用（推导）')}</span>
      </div>

      {ROUTE_TASKS.map(({ task, label, llm, note, todo }) => {
        /* 不调模型的任务：三列合并成一句说明。它们真的在跑，只是没有模型可配。 */
        if (!llm) {
          return (
            <div key={task} className={`mm-route-row${todo ? ' todo' : ''}`}>
              <span className='mm-route-task'>
                {t(label)}
                <code>{task}</code>
              </span>
              <span className='mm-route-fixed'>{todo ? '—' : t('不经过网关')}</span>
              <span className='mm-route-inert'>
                {todo ? <em className='mm-route-todo'>{t('未实现')}</em> : null}
                {t(note ?? '')}
              </span>
            </div>
          )
        }

        const row = routing.find(item => item.task === task)
        const level = effort[task] ?? ''
        const tag = level === 'high' ? 'heavy' : level === 'medium' ? 'light' : ''
        const resolved = resolveModel(task, routing, effort, pool, fallback)
        return (
          <div key={task} className='mm-route-row'>
            <span className='mm-route-task'>
              {t(label)}
              <code>{task}</code>
            </span>
            <span className='mm-route-fixed'>{t('本机网关')}</span>
            <ModelInput
              task={task}
              value={row?.model ?? ''}
              options={unique}
              onCommit={model => onChange(task, { model })}
            />
            <span className='mm-route-tag'>
              {tag ? t(tag === 'heavy' ? '重（heavy）' : '轻（light）') : '—'}
            </span>
            <select
              className='mm-select mm-select-sm mm-route-effort'
              value={level}
              onChange={event => onEffortChange(task, event.target.value)}
            >
              {EFFORT_OPTIONS.map(option => (
                <option key={option.value} value={option.value}>{t(option.label)}</option>
              ))}
            </select>
            <span className='mm-route-resolved'>
              {resolved.model ? <code>{resolved.model}</code> : t('模型目录首个')}
              <em>{t(resolved.via)}</em>
            </span>
          </div>
        )
      })}
    </div>
  )
}

/**
 * 「指定模型」—— 可自由输入的模型名，不是下拉。
 *
 * ── 为什么改成输入框 ────────────────────────────────────────
 * 这一格填的是**下游请求体里的 `model` 字段**，取值空间由上游决定，不由本地
 * 模型目录决定。用下拉等于把「能填什么」锁死在 `models.models` 里 —— 想指定一个
 * 只存在于上游、还没登记进池子的模型（新上线的、别家别名、私有部署的），
 * 下拉里根本没有这个选项，用户只能先去模型页登记一遍再回来。
 *
 * ── 为什么不用「下拉 + 允许手输」那种组合控件 ────────────────
 * 原生 `<input list>` 的候选面板由浏览器画，样式与本页其余控件完全不一致，
 * 且只在部分平台可点选。这里用纯 `<input>` + 一行候选提示：候选是参考，不是边界。
 *
 * ── 为什么本地 state 而不是直接受控于 routing ────────────────
 * `onChange` 会把值写进页面 draft，draft 变化会让整张表重渲染。若输入框直接
 * `value={row?.model}`，每敲一个字符都走一遍「draft → 重算 routing → 重渲染」，
 * 中间还会因为「空值不落盘」的规则把行删掉再建回来 —— 光标会跳、输入会丢字
 * （中文输入法尤其明显）。所以框内自己管草稿，失焦 / 回车时才提交。
 */
function ModelInput({
  task,
  value,
  options,
  onCommit,
}: {
  task: string
  value: string
  options: string[]
  onCommit: (model: string) => void
}): React.ReactElement {
  const [text, setText] = React.useState(value)
  // 外部值变了（刷新链路、点「默认」）才回灌，正在输入时不打扰
  const last = React.useRef(value)
  React.useEffect(() => {
    if (last.current !== value) {
      last.current = value
      setText(value)
    }
  }, [value])

  const commit = (next: string) => {
    const trimmed = next.trim()
    last.current = trimmed
    setText(trimmed) // 去掉首尾空白后回显，用户看得出它被怎么理解了
    if (trimmed !== value) onCommit(trimmed)
  }

  // 候选只是提示：输入框接受任何字符串，这里是「本地模型池里像的那几个」
  const typed = text.trim()
  const hints = typed
    ? options
        .filter(model => model !== typed && model.toLowerCase().includes(typed.toLowerCase()))
        .slice(0, 4)
    : []

  return (
    <span className='mm-route-model'>
      <input
        className='mm-input mm-route-model-input'
        type='text'
        value={text}
        placeholder={t('留空 = 按标签 + 优先级')}
        aria-label={t('{task} 指定模型', { task })}
        spellCheck={false}
        autoComplete='off'
        onChange={event => setText(event.target.value)}
        onBlur={() => commit(text)}
        onKeyDown={event => {
          if (event.key === 'Enter') {
            event.preventDefault()
            commit(text)
            ;(event.target as HTMLInputElement).blur()
          }
          if (event.key === 'Escape') {
            // 放弃这次编辑：回到已保存的值，别让「后悔」变成一次脏写入
            setText(value)
            ;(event.target as HTMLInputElement).blur()
          }
        }}
      />
      {hints.length ? (
        <span className='mm-route-model-hints'>
          {hints.map(model => (
            <button
              key={model}
              type='button'
              className='mm-route-model-hint'
              // mousedown 而不是 click：input 先失焦就会 commit 并重渲染，
              // 候选按钮随即被卸载，click 就永远等不到了
              onMouseDown={event => {
                event.preventDefault()
                commit(model)
              }}
            >
              {model}
            </button>
          ))}
        </span>
      ) : null}
    </span>
  )
}

/**
 * 按 `llm::model_for` 的同一套规则推导「将使用哪个模型」。
 *
 * **改后端选模型规则时这里要同步改** —— 它不读接口，是复算。界面上标了「推导」
 * 就是为了不让它看起来像权威结果。
 */
function resolveModel(
  task: string,
  routing: RouteRow[],
  effort: Record<string, string>,
  pool: { model: string; tags: string[] }[],
  fallback: string,
): { model: string; via: string } {
  const hit = routing.find(row => row.task === task && row.model.trim())
  if (hit) return { model: hit.model.trim(), via: '指定' }

  const level = effort[task]
  if (level === 'high' || level === 'medium') {
    const tag = level === 'high' ? 'heavy' : 'light'
    const found = pool.find(item => item.tags.includes(tag) && item.model.trim())
    if (found) return { model: found.model, via: `标签 ${tag}` }
  }

  if (fallback.trim()) return { model: fallback.trim(), via: '兜底' }
  return { model: '', via: '目录首个' }
}

/* ─── 单个旋钮 ───────────────────────────────────────────── */

function KnobRow({
  knob,
  value,
  dirty,
  onChange,
  onDefault,
}: {
  knob: m.Knob
  value: unknown
  dirty: boolean
  onChange: (next: unknown) => void
  onDefault: () => void
}): React.ReactElement {
  // 开关不挂「默认 开/关」：那一行右半边本来就空，再写一句等于噪音。
  const hint = knob.kind === 'boolean' ? '' : defaultHint(knob)
  // `wired === false`（含后端没下发这个字段之外的显式 false）：本仓没有读取点。
  // 走 `=== false` 而不是 `!knob.wired`：老后端不下发 `wired` 时值是
  // `undefined`，那种情况必须当作「已接入」处理，否则升级过程中整页旋钮
  // 会突然全灰 —— 一次版本错配不该让页面变成不可用。
  const unwired = knob.wired === false
  return (
    <div
      className={`mm-cfg-knob${dirty ? ' dirty' : ''}${unwired ? ' unwired' : ''}${
        knob.kind === 'list' || knob.kind === 'map' ? ' wide' : ''
      }`}
    >
      <div className='mm-cfg-knob-main'>
        <div className='mm-cfg-knob-label'>
          <span>{t(knob.label)}</span>
          {unwired ? <em className='mm-cfg-unwired'>{t('本仓未接入')}</em> : null}
          {knob.hot === false ? <em className='mm-cfg-hot'>{t('需重启 / 重建索引')}</em> : null}
          {dirty ? <em className='mm-cfg-dirty'>{t('未保存')}</em> : null}
        </div>
        <code className='mm-cfg-knob-key'>{knob.key}</code>
        {knob.desc ? <div className='mm-cfg-knob-desc'>{t(knob.desc)}</div> : null}
      </div>

      <div className='mm-cfg-knob-control'>
        {/* 禁用而不是隐藏：值仍然要能看见、能导出 —— 它是跨实现共享的配置，
            隐藏会让人以为这个键不存在，进而在另一台机器上「怎么和我这里不一样」 */}
        <fieldset className='mm-cfg-knob-lock' disabled={unwired}>
          <KnobControl knob={knob} value={value} onChange={onChange} />
        </fieldset>
      </div>

      <div className='mm-cfg-knob-default'>
        {hint ? <span className='mm-cfg-default-hint'>{hint}</span> : null}
        <Button type='button' size='sm' variant='ghost' disabled={unwired} onClick={onDefault}>
          {t('默认')}
        </Button>
      </div>
    </div>
  )
}

function KnobControl({
  knob,
  value,
  onChange,
}: {
  knob: m.Knob
  value: unknown
  onChange: (next: unknown) => void
}): React.ReactElement {
  switch (knob.kind) {
    case 'boolean':
      return <Switch checked={value === true} onCheckedChange={next => onChange(next)} />

    case 'number':
      return (
        <Input
          type='number'
          className='mm-cfg-number'
          min={knob.min}
          max={knob.max}
          value={String(value ?? '')}
          onChange={event => {
            const next = Number(event.target.value)
            if (!Number.isFinite(next)) return
            onChange(next)
          }}
        />
      )

    case 'enum':
      return (
        <select
          className='mm-select mm-select-sm'
          value={String(value ?? '')}
          onChange={event => onChange(event.target.value)}
        >
          {(knob.options ?? []).map(option => (
            <option key={option} value={option}>{t(option)}</option>
          ))}
        </select>
      )

    case 'multiselect': {
      const picked = Array.isArray(value) ? value.map(String) : []
      return (
        <div className='mm-cfg-chips'>
          {(knob.options ?? []).map(option => {
            const on = picked.includes(option)
            return (
              <button
                key={option}
                type='button'
                className={`mm-chip${on ? ' on' : ''}`}
                onClick={() => onChange(on ? picked.filter(item => item !== option) : [...picked, option])}
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
      return <JsonField value={value} onChange={onChange} />

    case 'path':
      return (
        <div className='mm-cfg-item-control'>
          <Input
            className='mm-cfg-text'
            value={typeof value === 'string' ? value : ''}
            placeholder={knob.default === null ? '' : String(knob.default ?? '')}
            onChange={event => onChange(event.target.value)}
          />
          <Button
            type='button'
            size='sm'
            variant='outline'
            title={t('选择目录（仅桌面端）')}
            onClick={() => {
              void pickFolder(typeof value === 'string' ? value : '').then(picked => {
                if (picked) onChange(picked)
              })
            }}
          >
            {t('浏览…')}
          </Button>
        </div>
      )

    case 'string':
    default:
      return (
        <Input
          className='mm-cfg-text'
          value={typeof value === 'string' ? value : ''}
          placeholder={knob.default === null ? '' : String(knob.default ?? '')}
          onChange={event => onChange(event.target.value)}
        />
      )

    // ── 秘密字段 ──
    //
    // 后端从不下发原文（只给 `set`），所以这里的 value 永远从空串开始。
    // 空串的语义是「沿用已保存的那份」，不是「清空」—— 与
    // `config::apply_one` 的 Secret 分支同一约定。占位符把这件事写清楚：
    // 配过就写「已保存口令，输入任意字符即进入修改」，没配过就写「未设置」。
    case 'secret':
      return (
        <Input
          className='mm-cfg-text'
          type='password'
          autoComplete='new-password'
          value={typeof value === 'string' ? value : ''}
          placeholder={
            knob.set
              ? t('已保存口令，输入任意字符即进入修改')
              : t('未设置')
          }
          onChange={event => onChange(event.target.value)}
        />
      )
  }
}

/**
 * 调壳的目录选择器。
 *
 * 走 `window.workbuddyDesktop.pickFolder`（`src-tauri/src/bridge.rs` 里已有的
 * `pick_folder`）—— 网页端没有这个桥，返回 null，按钮就保持「点了没反应」的
 * 保守行为，而不是报错。用户也可以直接在输入框里敲路径，这个按钮只是省事。
 */
function pickFolder(defaultDir: string): Promise<string | null> {
  const bridge = (window as unknown as {
    workbuddyDesktop?: { pickFolder?: (dir?: string) => Promise<{ path?: string; canceled?: boolean }> }
  }).workbuddyDesktop
  if (!bridge?.pickFolder) return Promise.resolve(null)
  return bridge
    .pickFolder(defaultDir || undefined)
    .then(result => (result && !result.canceled && result.path ? result.path : null))
    .catch(() => null)
}

/** 结构化值（数组 / 对象）用 JSON 文本框：解析失败就不提交。 */
function JsonField({ value, onChange }: { value: unknown; onChange: (next: unknown) => void }): React.ReactElement {
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
    <div className='mm-cfg-json'>
      <textarea
        className={`mm-cfg-json-input${invalid ? ' invalid' : ''}`}
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
      <div className='mm-inline-actions'>
        <Button
          type='button'
          size='sm'
          variant='outline'
          disabled={invalid}
          onClick={() => {
            try {
              onChange(JSON.parse(draft))
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
        {invalid ? <span className='mm-warn'>{t('JSON 不合法')}</span> : null}
      </div>
    </div>
  )
}

/* ─── 小工具 ─────────────────────────────────────────────── */

/** 「默认 5 · 范围 1~50」—— 让用户不用点开就知道默认值与合法区间。 */
function defaultHint(knob: m.Knob): string {
  const raw = knob.default
  let text: string
  if (raw === null || raw === undefined || raw === '') text = t('空')
  else if (typeof raw === 'boolean') text = raw ? t('开') : t('关')
  else if (typeof raw === 'number') text = String(raw)
  else if (typeof raw === 'string') text = raw
  else text = JSON.stringify(raw)

  if (knob.kind === 'number' && knob.min !== undefined && knob.max !== undefined) {
    return t('默认 {value} · 范围 {min}~{max}', { value: text, min: knob.min, max: knob.max })
  }
  return t('默认 {value}', { value: text })
}

function toRouting(raw: unknown): RouteRow[] {
  if (!Array.isArray(raw)) return []
  const rows: RouteRow[] = []
  for (const item of raw) {
    if (!item || typeof item !== 'object') continue
    const row = item as Record<string, unknown>
    const task = typeof row.task === 'string' ? row.task : ''
    if (!task) continue
    rows.push({
      task,
      tags: Array.isArray(row.tags) ? row.tags.map(String) : [],
      model: typeof row.model === 'string' ? row.model : '',
    })
  }
  return rows
}

function toMap(raw: unknown): Record<string, string> {
  if (!raw || typeof raw !== 'object' || Array.isArray(raw)) return {}
  const out: Record<string, string> = {}
  for (const [key, item] of Object.entries(raw as Record<string, unknown>)) {
    if (typeof item === 'string') out[key] = item
  }
  return out
}

function toPool(raw: unknown): { model: string; tags: string[] }[] {
  if (!Array.isArray(raw)) return []
  const out: { model: string; tags: string[] }[] = []
  for (const item of raw) {
    if (!item || typeof item !== 'object') continue
    const row = item as Record<string, unknown>
    const model = typeof row.model === 'string' ? row.model : ''
    if (!model.trim()) continue
    out.push({ model, tags: Array.isArray(row.tags) ? row.tags.map(String) : [] })
  }
  return out
}

function iconHtml(name: string, size = 14): string {
  const api = (window as unknown as { wbIcons?: { icon?: (key: string, px?: number) => string } }).wbIcons
  return api?.icon?.(name, size) ?? ''
}

export { Settings }
