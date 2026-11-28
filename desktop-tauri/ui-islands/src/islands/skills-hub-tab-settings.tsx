import * as React from 'react'
import { Badge, Button, Switch } from '@ui'
import { t } from '../i18n'
import * as s from './skills-hub-api'
import { Empty, Panel, sizeText, toast, useLoad } from './skills-hub-shared'

/**
 * 技能仓库 · 配置。
 *
 * ── 为什么这一页把「渠道 / 行为 / 回收站」放一起而不拆成三个页签 ──
 * 这三组旋钮共享同一个判断：**要不要让这个功能动我的磁盘、动到什么程度**。
 * 渠道决定扫哪里、挂载模式决定怎么落、回收站决定删了还能不能救回来 —— 拆开之后
 * 用户会在「渠道页加了目录」之后不知道「它已经开始往那儿写链接了」。放一页能
 * 一次看全后果。
 *
 * ── 为什么要分成「渠道 / 检测 / 回收站」三档 ────────────────
 * 上面的理由说的是「别拆成独立页签」，但渠道一多（十几条）整页会很长，行为开关
 * 被顶到最上面反而难找。于是用**同页内的档位**收纳：默认落在「渠道」这一档，
 * 行为开关和回收站各占一档，谁都不会把别人挤没。三个档位仍属于同一页，共享同
 * 一次读取与保存。
 *
 * ── 为什么渠道是卡片而不是表格 ─────────────────────────────
 * 渠道行有「图标 / 名字 / id / 内置标记 / 命中状态 / 开关 / 候选路径数」七样东西，
 * 塞进表格列会把「候选目录未命中」这种要人动手的警告压成一行灰字。卡片按「一个
 * 渠道一块」排，命中状态能整行显示成橙字警告，一眼看出哪条要改。
 *
 * ── 为什么渠道只存「用户改过的覆盖」────────────────────────
 * 内置渠道的默认路径随客户端版本变（某家会把 skills 从 `~/.x` 挪到
 * `%APPDATA%/x`）。若把整份表存进配置，升级后老配置里的旧路径会一直生效，
 * 用户看到的永远是「找不到目录」。所以默认值在代码里（`tools.rs::TOOLS`），
 * 配置只记改名 / 改路径 / 停用；新增内置渠道时老用户自动拿到。
 *
 * ── 为什么「探测到的渠道」只给建议不自动加 ──────────────────
 * 猜错一条目录就会把别人的技能扫进仓库。用户没确认过的目录一律不进扫描目标 ——
 * 所以 `probe` 是只读的，加不加由用户点。
 */

type Props = { onChanged: () => void }

type Section = 'tools' | 'auto' | 'trash'
type ToolFilter = 'all' | 'connected' | 'enabled' | 'unconfigured'

/** 渠道图标底色：按名字哈希从一组柔和的语义色里挑，同名的颜色稳定。 */
const ICON_COLORS = [
  'var(--primary)', 'var(--ok)', 'var(--warn)', 'var(--danger)',
  'var(--info, var(--primary))',
]

function iconColor(seed: string): string {
  let hash = 0
  for (let at = 0; at < seed.length; at += 1) hash = (hash * 31 + seed.charCodeAt(at)) >>> 0
  return ICON_COLORS[hash % ICON_COLORS.length]
}

function iconLetter(name: string): string {
  const trimmed = name.trim()
  if (!trimmed) return '?'
  // 中文取首字，英文取首字母大写。
  return /[a-zA-Z]/.test(trimmed[0]) ? trimmed[0].toUpperCase() : trimmed[0]
}

/**
 * epoch 毫秒 → `MM-DD HH:mm`。
 *
 * 只给「上次巡检是什么时候」用：不带年份，因为这块信息永远说的是最近一次，
 * 加年份会让一行短文案变长，反而更难扫。坏值给 `—`，不抛错。
 */
function formatWhen(at: number | undefined): string {
  if (!at || !Number.isFinite(at)) return '—'
  const date = new Date(at)
  if (Number.isNaN(date.getTime())) return '—'
  const pad = (value: number): string => String(value).padStart(2, '0')
  return `${pad(date.getMonth() + 1)}-${pad(date.getDate())} ${pad(date.getHours())}:${pad(date.getMinutes())}`
}

function Settings({ onChanged }: Props): React.ReactElement {
  const config = useLoad(() => s.loadConfig(), [])
  const tools = useLoad(() => s.loadTools(), [])
  const trash = useLoad(() => s.loadTrash(), [])
  const probe = useLoad(() => s.loadProbe(), [])
  // 本机现成的技能仓库（带台账）：真身目录那一栏不用手打路径。
  const repos = useLoad(() => s.loadRepos(), [])  // 巡检状态条要显示「上次巡检的时间与结论」，不读它这块只能画个空壳。
  const check = useLoad(() => s.loadDailyCheck(), [])

  const [section, setSection] = React.useState<Section>('tools')
  const [toolFilter, setToolFilter] = React.useState<ToolFilter>('all')
  const [toolQuery, setToolQuery] = React.useState('')
  const [busy, setBusy] = React.useState('')
  const [customPath, setCustomPath] = React.useState('')
  const addInputRef = React.useRef<HTMLInputElement | null>(null)
  const [draft, setDraft] = React.useState<s.HubConfig | null>(null)
  const [pending, setPending] = React.useState(false)

  const values = config.data?.values
  const dirRows = tools.data?.items ?? []
  const trashItems = trash.data?.items ?? []
  const suggestions = probe.data?.items ?? []
  const repoRows = repos.data?.items ?? []

  const hubPath = hubDir(trash.data?.dir)
  // 真身目录留空时的默认值，只用于输入框占位提示（真正解析在后端 `store::skills_dir`）。
  const defaultSkillsDir =
    hubPath && hubPath !== '—' ? `${hubPath.replace(/[\\/]+$/, '')}/skills` : ''

  // 界面读草稿（草稿只在用户改过之后存在，否则就是服务端值）。
  const cfg = draft ?? values
  // 自定义目录是「立刻生效」的动作（加/删各自独立打盘），不走草稿。
  const customDirs = values?.customDirs ?? []

  // 开关与输入框改的是**草稿**，点「保存配置」才落盘 —— 这是参考页的交互：
  // 一次改三个渠道再统一保存，不至于点一下开关就打一次盘、顺手触发一次全量扫描。
  // 有未保存改动时不要被后台重载覆盖掉（`pending` 为真就保持草稿）。
  React.useEffect(() => {
    if (values && !pending) setDraft({ ...values })
  }, [values, pending])

  // 「有没有未保存改动」要真比对，不能只记「点过」——
  // 开关点开再点回去（或改了数字又改回原值）就不该再提示保存。
  const savedShape = (value: s.HubConfig | null | undefined): string => {
    if (!value) return ''
    // 逐字段列出而不是 `JSON.stringify(value)`：后者一旦后端新增字段
    // （比如这次的 `schedule`）就会出现「没改任何东西也显示未保存」。
    // 新增字段记得往这里补一项 —— 漏了的表现是「改了不提示保存」。
    return JSON.stringify([
      value.mountMode,
      value.trashDays,
      value.l3Enabled,
      value.l3Threshold,
      value.hubDir,
      value.skillsDir,
      value.tools,
      value.schedule,
    ])
  }
  const dirty = draft !== null && savedShape(draft) !== savedShape(values)

  const edit = (patch: Partial<s.HubConfig>) => {
    const base = draft ?? values
    if (!base) return
    const next = { ...base, ...patch }
    setDraft(next)
    // 改回原值就把 pending 放开，让后台重载能重新同步（否则草稿会一直挂着）
    setPending(savedShape(next) !== savedShape(values))
  }

  // 渠道开关只改草稿；真正落盘在页头「保存配置」。原来点一下开关就打一次盘，
  // 打盘会顺手触发一次全量扫描 —— 连开三个渠道就扫三次，还容易被后到的响应覆盖。
  const toggleTool = (id: string, enabled: boolean) => {
    const base = draft ?? values
    if (!base) return
    const nextTools = { ...base.tools }
    nextTools[id] = { ...(nextTools[id] ?? {}), enabled }
    edit({ tools: nextTools })
  }

  const saveDraft = async () => {
    if (!draft) return
    setBusy('save')
    try {
      await s.saveConfig({
        mountMode: draft.mountMode,
        trashDays: draft.trashDays,
        l3Enabled: draft.l3Enabled,
        l3Threshold: draft.l3Threshold,
        hubDir: draft.hubDir,
        skillsDir: draft.skillsDir,
        tools: draft.tools,
        schedule: draft.schedule,
      })
      setPending(false)
      config.reload()
      tools.reload()
      probe.reload()
      onChanged()
      toast(t('配置已保存'), 'ok')
    } catch (error) {
      toast(t('保存失败：{error}', { error: String(error instanceof Error ? error.message : error) }), 'err')
    } finally {
      setBusy('')
    }
  }

  const addCustom = async (raw?: string) => {
    const path = (raw ?? customPath).trim()
    if (!path) return
    setBusy('custom')
    try {
      await s.addCustomDir(path)
      setCustomPath('')
      config.reload()
      tools.reload()
      probe.reload()
      toast(t('已加入自定义目录'), 'ok')
    } catch (error) {
      toast(t('加入失败：{error}', { error: String(error instanceof Error ? error.message : error) }), 'err')
    } finally {
      setBusy('')
    }
  }

  const dropCustom = async (path: string) => {
    setBusy(`drop-${path}`)
    try {
      await s.removeCustomDir(path)
      config.reload()
      tools.reload()
      probe.reload()
    } catch (error) {
      toast(t('移除失败：{error}', { error: String(error instanceof Error ? error.message : error) }), 'err')
    } finally {
      setBusy('')
    }
  }

  const copyHub = () => {
    if (!hubPath || hubPath === '—') return
    const clip = navigator.clipboard
    if (!clip) { toast(t('复制失败'), 'err'); return }
    void clip.writeText(hubPath).then(
      () => toast(t('已复制中央仓库路径'), 'ok'),
      () => toast(t('复制失败'), 'err'),
    )
  }

  const scanNow = () => {
    tools.reload()
    probe.reload()
    toast(t('正在重新扫描本机渠道…'))
  }

  const manualAdd = () => {
    addInputRef.current?.scrollIntoView({ behavior: 'smooth', block: 'center' })
    addInputRef.current?.focus()
  }

  // 开关的显示值也要读草稿：否则点完开关、服务端还没存，`row.enabled` 一刷新就弹回原样。
  const toolEnabled = (row: s.ToolRow): boolean => cfg?.tools?.[row.id]?.enabled ?? row.enabled

  // 渠道档的三个计数与筛选。
  const toolCounts = {
    all: dirRows.length,
    connected: dirRows.filter(row => row.dir !== null).length,
    enabled: dirRows.filter(row => toolEnabled(row)).length,
    unconfigured: dirRows.filter(row => row.dir === null).length,
  }
  const visibleTools = dirRows
    .filter(row => {
      switch (toolFilter) {
        case 'connected': return row.dir !== null
        case 'enabled': return toolEnabled(row)
        case 'unconfigured': return row.dir === null
        default: return true
      }
    })
    .filter(row => {
      const needle = toolQuery.trim().toLowerCase()
      if (!needle) return true
      return row.name.toLowerCase().includes(needle) || row.id.toLowerCase().includes(needle)
    })

  const TOOL_FILTERS: { key: ToolFilter; label: string }[] = [
    { key: 'all', label: t('全部') },
    { key: 'connected', label: t('有效已连接') },
    { key: 'enabled', label: t('已启用') },
    { key: 'unconfigured', label: t('待配置路径') },
  ]

  return (
    <Panel loading={config.loading} error={config.error} onRetry={config.reload}>
      {!cfg ? <Empty>{t('还没读到配置')}</Empty> : (
        <div className='sk-cfg'>
          {/* ── 页头：标题 + 中央仓库路径 + 保存配置（改完统一落盘） ── */}
          <div className='sk-cfg-head'>
            <div className='sk-cfg-title'>{t('配置 · 技能仓库')}</div>
            <div className='sk-cfg-head-actions'>
              <Button type='button' size='sm' variant='outline' onClick={copyHub}>
                {t('中央仓库目录')}
              </Button>
              <Button
                type='button'
                size='sm'
                variant='default'
                disabled={!dirty || busy === 'save'}
                onClick={() => void saveDraft()}
              >
                {busy === 'save' ? t('保存中…') : dirty ? t('保存配置') : t('已保存')}
              </Button>
            </div>
          </div>

          {/* ── 档位 ── */}
          <div className='sk-cfg-tabs' role='tablist' aria-label={t('配置分区')}>
            <button
              type='button' role='tab' aria-selected={section === 'tools'}
              className={`sk-cfg-tab${section === 'tools' ? ' is-active' : ''}`}
              onClick={() => setSection('tools')}
            >
              {t('渠道工具适配器')}
              <span className='sk-cfg-tab-count'>{dirRows.length}</span>
            </button>
            <button
              type='button' role='tab' aria-selected={section === 'auto'}
              className={`sk-cfg-tab${section === 'auto' ? ' is-active' : ''}`}
              onClick={() => setSection('auto')}
            >
              {t('全自动检测与调度')}
            </button>
            <button
              type='button' role='tab' aria-selected={section === 'trash'}
              className={`sk-cfg-tab${section === 'trash' ? ' is-active' : ''}`}
              onClick={() => setSection('trash')}
            >
              {t('回收站与存储')}
              <span className='sk-cfg-tab-count'>{trashItems.length}</span>
            </button>
          </div>

          {/* ══ 渠道工具适配器 ══ */}
          {section === 'tools' ? (
            <>
              {/* 常用渠道推荐：已就绪的绿标，探测到未接的给「+启用」 */}
              <div className='sk-cfg-recommend'>
                <div className='sk-cfg-recommend-title'>{t('常用 Agent 渠道推荐')}</div>
                <div className='sk-cfg-recommend-chips'>
                  {dirRows.map(row => {
                    const ready = toolEnabled(row) && row.dir !== null
                    return (
                      <button
                        key={`rec-${row.id}`}
                        type='button'
                        className={`sk-cfg-rec-chip${ready ? ' is-ready' : ''}`}
                        title={row.dir ?? t('候选目录未命中')}
                        onClick={() => {
                          if (ready) return
                          if (row.dir === null) { manualAdd(); return }
                          void toggleTool(row.id, true)
                        }}
                      >
                        <span className='sk-cfg-rec-dot' style={{ background: iconColor(row.name) }}>
                          {iconLetter(row.name)}
                        </span>
                        <span className='sk-cfg-rec-name'>{row.name}</span>
                        <span className='sk-cfg-rec-state'>{ready ? t('已就绪') : t('+ 启用')}</span>
                      </button>
                    )
                  })}
                  {suggestions.map(row => (
                    <button
                      key={`rec-s-${row.suggestId}`}
                      type='button'
                      className='sk-cfg-rec-chip'
                      title={row.firstDir}
                      disabled={busy === 'custom'}
                      onClick={() => void addCustom(row.firstDir)}
                    >
                      <span className='sk-cfg-rec-dot' style={{ background: iconColor(row.name) }}>
                        {iconLetter(row.name)}
                      </span>
                      <span className='sk-cfg-rec-name'>{row.name}</span>
                      <span className='sk-cfg-rec-state'>{t('+ 应用')}</span>
                    </button>
                  ))}
                </div>
              </div>

              {/* 工具栏：档位计数（左）+ 搜索 / 扫描 / 手动新增（右） */}
              <div className='sk-cfg-toolbar'>
                <div className='sk-cfg-filters'>
                  {TOOL_FILTERS.map(item => (
                    <button
                      key={item.key}
                      type='button'
                      className={`sk-cfg-filter${toolFilter === item.key ? ' is-active' : ''}`}
                      onClick={() => setToolFilter(item.key)}
                    >
                      {item.label}
                      <span className='sk-cfg-filter-count'>{toolCounts[item.key]}</span>
                    </button>
                  ))}
                </div>
                <div className='sk-cfg-searchbox'>
                  <svg className='sk-cfg-search-icon' viewBox='0 0 24 24' fill='none' stroke='currentColor' strokeWidth='2' strokeLinecap='round' strokeLinejoin='round' aria-hidden='true'>
                    <circle cx='11' cy='11' r='7' /><path d='m20 20-3.2-3.2' />
                  </svg>
                  <input
                    className='sk-cfg-search'
                    type='search'
                    value={toolQuery}
                    placeholder={t('快速搜索渠道名称或 ID…')}
                    onChange={event => setToolQuery(event.target.value)}
                  />
                </div>
                <Button
                  type='button' size='sm' variant='ghost'
                  disabled={busy === 'scan'}
                  onClick={() => {
                    setBusy('scan')
                    void s.runDailyCheck(false)
                      .then(result => { toast(result.summary, result.ok ? 'ok' : 'err'); tools.reload(); probe.reload() })
                      .catch(error => toast(t('检测失败：{error}', { error: String(error) }), 'err'))
                      .finally(() => setBusy(''))
                  }}
                >
                  {busy === 'scan' ? t('检测中…') : t('立即检测')}
                </Button>
                <Button type='button' size='sm' variant='ghost' onClick={scanNow}>
                  {t('扫描电脑发现')}
                </Button>
                <Button type='button' size='sm' variant='outline' onClick={manualAdd}>
                  {t('手动新增')}
                </Button>
              </div>

              {/* 渠道卡片网格 */}
              {visibleTools.length === 0 ? (
                <Empty>{dirRows.length === 0 ? t('还没读到任何渠道') : t('没有符合筛选的渠道')}</Empty>
              ) : (
                <div className='sk-cfg-grid'>
                  {visibleTools.map(row => {
                    const hit = row.dir !== null
                    const paths = row.candidatePaths ?? []
                    const enabled = toolEnabled(row)
                    // 改过但没保存的卡片描一圈，让人一眼看到「这几张还没落盘」
                    const edited = enabled !== row.enabled
                    return (
                      <div key={row.id} className={`sk-cfg-card${enabled ? '' : ' is-off'}${edited ? ' is-edited' : ''}`}>
                        <span className='sk-cfg-icon' style={{ background: iconColor(row.name) }}>
                          {iconLetter(row.name)}
                        </span>
                        <div className='sk-cfg-card-main'>
                          <div className='sk-cfg-card-top'>
                            <span className='sk-cfg-card-name'>{row.name}</span>
                            <span className='sk-cfg-card-id'>{row.id}</span>
                            <Badge variant='outline' shape='tag'>{row.builtin ? t('内置') : t('自定义')}</Badge>
                          </div>
                          <div className={`sk-cfg-card-status${hit ? ' is-hit' : ''}`} title={row.dir ?? paths.join('\n')}>
                            {hit ? (
                              <><span className='sk-cfg-check'>✓</span><span className='sk-cfg-path'>{row.dir}</span></>
                            ) : (
                              <><span className='sk-cfg-warn'>⚠</span>{t('候选目录未命中，请核对下方路径')}</>
                            )}
                          </div>
                        </div>
                        <div className='sk-cfg-card-control'>
                          <Switch checked={enabled} onCheckedChange={next => toggleTool(row.id, next)} />
                          <span className='sk-cfg-path-pill' title={paths.join('\n')}>
                            {t('{n} 路径', { n: paths.length })}
                          </span>
                        </div>
                      </div>
                    )
                  })}
                </div>
              )}

              {/* 自定义技能目录 */}
              <div className='sk-cfg-custom'>
                <div className='sk-cfg-custom-title'>{t('自定义技能目录')}</div>
                <div className='sk-cfg-custom-hint'>
                  {t('没有自带目录的独立技能库，或想额外扫一个私有的 skills/ 都在这里加。')}
                </div>
                {customDirs.length > 0 ? (
                  <div className='sk-cfg-custom-list'>
                    {customDirs.map(path => (
                      <div key={path} className='sk-cfg-custom-item'>
                        <code>{path}</code>
                        <Button
                          type='button' size='sm' variant='ghost'
                          disabled={busy === `drop-${path}`}
                          onClick={() => void dropCustom(path)}
                        >
                          {t('移除')}
                        </Button>
                      </div>
                    ))}
                  </div>
                ) : null}
                <div className='sk-cfg-add-row'>
                  <input
                    ref={addInputRef}
                    className='sk-input'
                    type='text'
                    value={customPath}
                    placeholder={t('再扫一个目录（支持 ~ 与 %APPDATA%）')}
                    onChange={event => setCustomPath(event.target.value)}
                    onKeyDown={event => { if (event.key === 'Enter') void addCustom() }}
                  />
                  <Button type='button' size='sm' variant='outline' disabled={busy === 'custom'} onClick={() => void addCustom()}>
                    {t('+ 添加自定义目录')}
                  </Button>
                </div>
              </div>
            </>
          ) : null}

          {/* ══ 检测与调度（行为开关）══ */}
          {section === 'auto' ? (
            <>
              {/* ── 第一块：每日自动健康检测 ──
                  参考实现的顺序是「开关 → 时刻 → 自动修复 → 上次结果」，
                  这里照抄。开关默认开：这是保底健康检查，装完就该有。 */}
              <div className='sk-cfg-panel'>
                <div className='sk-cfg-panel-title'>{t('全自动每日健康检测与自动对账')}</div>

                <div className='sk-field'>
                  <div className='sk-field-copy'>
                    <div className='sk-field-label'>{t('每日全自动健康检测开关')}</div>
                    <div className='sk-field-hint'>
                      {t('开启后系统每天在设定时刻自动扫描全部渠道连通性、新技能与挂载完整性')}
                    </div>
                  </div>
                  <div className='sk-field-control'>
                    <Switch
                      checked={cfg.schedule.dailyCheck}
                      onCheckedChange={next => edit({ schedule: { ...cfg.schedule, dailyCheck: next } })}
                    />
                  </div>
                </div>

                <div className='sk-field'>
                  <div className='sk-field-copy'>
                    <div className='sk-field-label'>{t('每日巡检执行时刻')}</div>
                    <div className='sk-field-hint'>
                      {t('格式为 24 小时制 HH:mm，如错过设定时刻（睡眠关机），开机首次 tick 自动补测')}
                    </div>
                  </div>
                  <div className='sk-field-control'>
                    <input
                      className='sk-input sk-input-num'
                      type='time'
                      value={cfg.schedule.dailyCheckTime}
                      disabled={!cfg.schedule.dailyCheck}
                      onChange={event => edit({ schedule: { ...cfg.schedule, dailyCheckTime: event.target.value } })}
                    />
                  </div>
                </div>

                <div className='sk-field'>
                  <div className='sk-field-copy'>
                    <div className='sk-field-label'>{t('巡检自动修复与纳管')}</div>
                    <div className='sk-field-hint'>
                      {t('检测到失效的 Junction 挂载时自动重建，无冲突孤儿目录自动尝试安全收纳')}
                    </div>
                  </div>
                  <div className='sk-field-control'>
                    <Switch
                      checked={cfg.schedule.dailyCheckAutoMount}
                      onCheckedChange={next => edit({ schedule: { ...cfg.schedule, dailyCheckAutoMount: next } })}
                    />
                  </div>
                </div>

                {/* 上次巡检结论。没跑过就不画这一条 —— 画一个「尚未运行」的空盒子
                    比不画更让人怀疑功能坏了。 */}
                {check.data?.last ? (
                  <div className='sk-check-strip'>
                    <div className='sk-check-strip-head'>
                      <div className='sk-check-strip-left'>
                        <Badge variant={check.data.last.ok ? 'success' : 'warning'}>
                          {check.data.last.ok ? t('全渠道健康良好') : t('发现待处理项')}
                        </Badge>
                        <span className='sk-check-strip-when'>
                          {t('上次巡检：{when}', { when: formatWhen(check.data.last.checkedAt) })}
                        </span>
                      </div>
                      <Button
                        type='button' size='sm' variant='ghost'
                        disabled={busy === 'check'}
                        onClick={() => {
                          setBusy('check')
                          void s.runDailyCheck(cfg.schedule.dailyCheckAutoMount)
                            .then(() => { check.reload(); onChanged(); toast(t('巡检完成'), 'ok') })
                            .catch(error => toast(t('巡检失败：{error}', { error: String(error) }), 'err'))
                            .finally(() => setBusy(''))
                        }}
                      >
                        {busy === 'check' ? t('巡检中…') : t('立即执行一次全自动检测')}
                      </Button>
                    </div>
                    <div className='sk-check-strip-summary'>{check.data.last.summary}</div>
                  </div>
                ) : null}
              </div>

              {/* ── 第二块：挂载模式与生命周期 ──
                  挂载模式用卡片单选而不是开关。两个选项各有适用场景，
                  开关只能表示「是不是 A」，说不清「B 是什么、什么时候该用 B」。 */}
              <div className='sk-cfg-panel'>
                <div className='sk-cfg-panel-title'>{t('挂载模式与生命周期')}</div>
                <div className='sk-cfg-panel-hint'>{t('调整仅影响之后的收纳与挂载动作')}</div>

                <div className='sk-field sk-field-block'>
                  <div className='sk-field-label'>{t('挂载模式')}</div>
                  <div className='sk-mount-choice'>
                    {([
                      { value: 'junction', title: t('Junction（推荐）'), desc: t('按技能粒度建立目录联接，零拷贝、即时生效。') },
                      { value: 'copy', title: t('直接复制'), desc: t('独立文件拷贝，兼容性好但可能产生漂移。') },
                    ] as const).map(item => {
                      const active = cfg.mountMode === item.value
                      return (
                        <button
                          key={item.value}
                          type='button'
                          className={`sk-mount-card${active ? ' is-active' : ''}`}
                          aria-pressed={active}
                          onClick={() => edit({ mountMode: item.value })}
                        >
                          <span className='sk-mount-card-title'>{item.title}</span>
                          <span className='sk-mount-card-desc'>{item.desc}</span>
                        </button>
                      )
                    })}
                  </div>
                </div>

                <div className='sk-field'>
                  <div className='sk-field-copy'>
                    <div className='sk-field-label'>{t('L3 语义去重提示')}</div>
                    <div className='sk-field-hint'>{t('本地向量计算，仅提示建议，绝不强行合并')}</div>
                  </div>
                  <div className='sk-field-control'>
                    <Switch
                      checked={cfg.l3Enabled}
                      onCheckedChange={next => edit({ l3Enabled: next })}
                    />
                  </div>
                </div>

                {/* 保留天数归「生命周期」不归「回收站」：它约束的是所有移除动作
                    多久之后自动过期，由收纳流程顺手清理（AgentHub 同）。 */}
                <div className='sk-field'>
                  <div className='sk-field-copy'>
                    <div className='sk-field-label'>{t('回收站保留天数')}</div>
                    <div className='sk-field-hint'>
                      {t('超期后由清理动作自动清除')}
                    </div>
                  </div>
                  <div className='sk-field-control'>
                    <input
                      className='sk-input sk-input-num'
                      type='number' min={0} max={3650}
                      defaultValue={cfg.trashDays}
                      onBlur={event => {
                        const next = Number(event.target.value)
                        if (Number.isFinite(next)) edit({ trashDays: Math.max(0, Math.floor(next)) })
                      }}
                    />
                    <span className='sk-field-value'>{t('天')}</span>
                  </div>
                </div>
              </div>

              {/* ── 第三块：中央数据仓库位置 ── */}
              <div className='sk-cfg-panel'>
                <div className='sk-cfg-panel-title'>{t('中央数据仓库位置')}</div>
                <div className='sk-cfg-panel-hint'>{t('技能真身（skills/）、清单（manifest.json）存放位置')}</div>

                <div className='sk-field sk-field-block'>
                  <div className='sk-field-label'>{t('中央仓库物理路径')}</div>
                  <div className='sk-path-row'>
                    <input className='sk-input' value={hubPath} readOnly />
                    <Button
                      type='button' size='sm' variant='ghost'
                      onClick={copyHub}
                    >
                      {t('复制')}
                    </Button>
                  </div>
                </div>

                {/* 真身目录可指到**现成仓库**：共用同一批技能，不复制、不停用对方。
                    留空 = `<中央仓库>/skills`。填错的表现是「中央技能库 0」，
                    所以占位里把生效值写出来，别让人靠猜。 */}
                <div className='sk-field sk-field-block'>
                  <div className='sk-field-label'>{t('技能真身目录')}</div>
                  <input
                    className='sk-input'
                    value={cfg?.skillsDir ?? ''}
                    placeholder={defaultSkillsDir ? t('留空 = {path}', { path: defaultSkillsDir }) : ''}
                    onChange={event => edit({ skillsDir: event.target.value })}
                  />
                  {/* 探测到的现成仓库：一键填入，不用手打路径。
                      只列「有 manifest.json 的仓库」—— 空 skills 目录指过去等于把中央换成空的。 */}
                  {repoRows.length > 0 ? (
                    <div className='sk-path-row'>
                      <select
                        className='sk-input'
                        value=''
                        onChange={event => {
                          const picked = repoRows.find(row => row.key === event.target.value)
                          if (picked) edit({ skillsDir: picked.skillsDir })
                        }}
                      >
                        <option value=''>{t('从本机现成仓库填入…')}</option>
                        {repoRows.map(row => (
                          <option key={row.key} value={row.key}>
                            {`${row.path}（${row.count} 个技能）`}
                          </option>
                        ))}
                      </select>
                      {cfg?.skillsDir ? (
                        <Button
                          type='button' size='sm' variant='ghost'
                          onClick={() => edit({ skillsDir: '' })}
                        >
                          {t('恢复默认')}
                        </Button>
                      ) : null}
                    </div>
                  ) : null}
                  <div className='sk-field-hint'>
                    {t('可指向现成仓库（例如 AgentHub 的 .agent_skills\\skills），两边共用同一批技能；在这里新增技能也会写进去。两边的台账仍各存一份。')}
                  </div>
                </div>

                <div className='sk-field'>
                  <div className='sk-field-copy'>
                    <div className='sk-field-label'>{t('全局同步与跨设备 WebDAV')}</div>
                    <div className='sk-field-hint'>
                      {t('记忆模块每 24 小时一轮自动同步；节奏在「设置 · 同步时间」里改。')}
                    </div>
                  </div>
                  <div className='sk-field-control'>
                    <Button
                      type='button' size='sm' variant='ghost'
                      onClick={() => toast(t('跨设备同步节奏在「设置 · 同步时间」里调'), 'ok')}
                    >
                      {t('去 设置 · 同步时间')}
                    </Button>
                  </div>
                </div>
              </div>
            </>
          ) : null}

          {/* ══ 回收站与存储 ══
              对齐 AgentHub 设置页「回收站与存储」档：只有两块。
              ── 为什么保留天数不在这 ──
              它管的是「所有移除动作的保留期」，属于生命周期策略，跟挂载模式
              是一组（AgentHub 也把它放在「挂载模式与生命周期」里）。摆在回收站
              档会让用户以为它只管回收站自己的容量。 */}
          {section === 'trash' ? (
            <>
              <div className='sk-cfg-panel'>
                <div className='sk-cfg-panel-title'>
                  {t('回收站（{n} 项）', { n: trashItems.length })}
                </div>
                <div className='sk-cfg-panel-hint'>
                  {t('被替换或删除的技能目录先进回收站，保留 {n} 天可还原。', { n: cfg.trashDays })}
                </div>

                {trash.loading && !trash.data ? (
                  <Empty>{t('正在读取…')}</Empty>
                ) : trashItems.length === 0 ? (
                  <Empty>{t('回收站是空的')}</Empty>
                ) : (
                  <div>
                    {trashItems.map(row => (
                      <div key={row.file} className='sk-trash-row'>
                        <span className='sk-trash-icon' aria-hidden>
                          <svg viewBox='0 0 24 24' fill='none' stroke='currentColor' strokeWidth='1.8' strokeLinecap='round' strokeLinejoin='round'>
                            <path d='M3 6h18' /><path d='M8 6V4h8v2' />
                            <path d='M19 6l-1 14H6L5 6' /><path d='M10 11v5M14 11v5' />
                          </svg>
                        </span>
                        <div className='sk-trash-copy'>
                          <div className='sk-trash-name' title={row.file}>{row.name}</div>
                          <div className='sk-trash-meta'>
                            {t('{when} · {size} · 还剩 {n} 天', {
                              when: clockText(row.mtimeMs),
                              size: sizeText(row.size),
                              n: row.daysLeft,
                            })}
                          </div>
                        </div>
                        <Button
                          type='button' size='sm' variant='ghost'
                          disabled={busy === `restore-${row.file}`}
                          onClick={() => {
                            setBusy(`restore-${row.file}`)
                            void s.restoreTrash(row.file)
                              .then(result => { toast(t('已还原「{name}」到中央', { name: result.name }), 'ok'); trash.reload(); onChanged() })
                              .catch(error => toast(t('还原失败：{error}', { error: String(error) }), 'err'))
                              .finally(() => setBusy(''))
                          }}
                        >
                          {t('还原')}
                        </Button>
                      </div>
                    ))}
                  </div>
                )}
              </div>

              {/* ── 危险区 ──
                  为什么单独一块、且只在这里出现：这是全页唯一「删了就没了」的动作。
                  AgentHub 把它做成独立区块并描红；混在普通操作里点错的代价不可逆。 */}
              <div className='sk-cfg-panel sk-danger-panel'>
                <div className='sk-cfg-panel-title sk-danger-title'>{t('危险区')}</div>
                <div className='sk-field'>
                  <div className='sk-field-copy'>
                    <div className='sk-field-label'>{t('清空回收站')}</div>
                    <div className='sk-field-hint'>
                      {t('立即永久删除 .trash 内的历史技能版本，不可恢复。')}
                    </div>
                  </div>
                  <div className='sk-field-control'>
                    <Button
                      type='button' size='sm' variant='destructive'
                      disabled={busy === 'purge' || trashItems.length === 0}
                      onClick={() => {
                        // 二次确认：这是全页唯一不可恢复的动作，AgentHub 同款语义。
                        if (!window.confirm(t('确定清空回收站里的 {n} 项？该操作不可恢复。', { n: trashItems.length }))) return
                        setBusy('purge')
                        void s.emptyTrash()
                          .then(result => { toast(t('已清空回收站（{n} 项）', { n: result.removed }), 'ok'); trash.reload(); onChanged() })
                          .catch(error => toast(t('清空失败：{error}', { error: String(error) }), 'err'))
                          .finally(() => setBusy(''))
                      }}
                    >
                      {t('清空')}
                    </Button>
                  </div>
                </div>
              </div>
            </>
          ) : null}
        </div>
      )}
    </Panel>
  )
}

/**
 * 回收站路径反推中央仓库根目录。
 *
 * 为什么不另开一个接口：回收站路径是 `<hub>/.trash` 这种固定形状（见
 * `store.rs` 的目录布局），反推一次比多一次请求划算。反推失败宁可显示 `—`，
 * 也不要把回收站路径当成中央仓库路径显示 —— 用户会照着它往错的地方放文件。
 */
/**
 * 毫秒时间戳 → 「11-27 22:14」。
 *
 * 为什么不直接 `toLocaleString`：它给出「2026/11/27 22:14:31」——年份和秒对
 * 「什么时候被删的」这个问题都是噪声。回收站条目只活几天，月日时分足够定位；
 * 完整文件名（含时间戳）已经放在行首的 `title` 里，需要精确值悬停看。
 */
function clockText(timestamp: number | undefined): string {
  if (!timestamp) return '—'
  const at = new Date(timestamp)
  if (Number.isNaN(at.getTime())) return '—'
  const pad = (value: number) => String(value).padStart(2, '0')
  return `${pad(at.getMonth() + 1)}-${pad(at.getDate())} ${pad(at.getHours())}:${pad(at.getMinutes())}`
}

function hubDir(trashPath: string | undefined): string {
  if (!trashPath) return '—'
  const hit = trashPath.replace(/[\\/][^\\/]*$/, '')
  return hit && hit !== trashPath ? hit : '—'
}

export { Settings }
