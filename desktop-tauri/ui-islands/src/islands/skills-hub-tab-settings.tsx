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

function Settings({ onChanged }: Props): React.ReactElement {
  const config = useLoad(() => s.loadConfig(), [])
  const tools = useLoad(() => s.loadTools(), [])
  const trash = useLoad(() => s.loadTrash(), [])
  const probe = useLoad(() => s.loadProbe(), [])

  const [section, setSection] = React.useState<Section>('tools')
  const [toolFilter, setToolFilter] = React.useState<ToolFilter>('all')
  const [toolQuery, setToolQuery] = React.useState('')
  const [busy, setBusy] = React.useState('')
  const [customPath, setCustomPath] = React.useState('')
  const addInputRef = React.useRef<HTMLInputElement | null>(null)

  const values = config.data?.values
  const dirRows = tools.data?.items ?? []
  const trashItems = trash.data?.items ?? []
  const suggestions = probe.data?.items ?? []

  const hubPath = hubDir(trash.data?.dir)

  const save = async (patch: Partial<s.HubConfig>) => {
    setBusy('save')
    try {
      await s.saveConfig(patch)
      config.reload()
      tools.reload()
      onChanged()
    } catch (error) {
      toast(t('保存失败：{error}', { error: String(error instanceof Error ? error.message : error) }), 'err')
    } finally {
      setBusy('')
    }
  }

  const toggleTool = async (id: string, enabled: boolean) => {
    if (!values) return
    const nextTools = { ...values.tools }
    nextTools[id] = { ...(nextTools[id] ?? {}), enabled }
    await save({ tools: nextTools })
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

  // 渠道档的三个计数与筛选。
  const toolCounts = {
    all: dirRows.length,
    connected: dirRows.filter(row => row.dir !== null).length,
    enabled: dirRows.filter(row => row.enabled).length,
    unconfigured: dirRows.filter(row => row.dir === null).length,
  }
  const visibleTools = dirRows
    .filter(row => {
      switch (toolFilter) {
        case 'connected': return row.dir !== null
        case 'enabled': return row.enabled
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
      {!values ? <Empty>{t('还没读到配置')}</Empty> : (
        <div className='sk-cfg'>
          {/* ── 页头：标题 + 两个动作 ── */}
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
                disabled={busy === 'scan'}
                onClick={() => {
                  setBusy('scan')
                  void s.runDailyCheck(false)
                    .then(result => { toast(result.summary, result.ok ? 'ok' : 'err'); tools.reload(); probe.reload() })
                    .catch(error => toast(t('检测失败：{error}', { error: String(error) }), 'err'))
                    .finally(() => setBusy(''))
                }}
              >
                {t('立即检测')}
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
              {t('检测与调度')}
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
                    const ready = row.enabled && row.dir !== null
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
                      <span className='sk-cfg-rec-state'>{t('+ 启用')}</span>
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
                    return (
                      <div key={row.id} className={`sk-cfg-card${row.enabled ? '' : ' is-off'}`}>
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
                          <Switch checked={row.enabled} onCheckedChange={next => void toggleTool(row.id, next)} />
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
                {(values.customDirs ?? []).length > 0 ? (
                  <div className='sk-cfg-custom-list'>
                    {values.customDirs.map(path => (
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
            <div className='sk-cfg-panel'>
              <div className='sk-cfg-panel-title'>{t('同步与挂载')}</div>
              <div className='sk-cfg-panel-hint'>{t('这些开关直接决定同步会怎么动你的磁盘')}</div>

              <div className='sk-field'>
                <div className='sk-field-copy'>
                  <div className='sk-field-label'>{t('挂载模式')}</div>
                  <div className='sk-field-hint'>
                    {t('目录链接：渠道目录里放一个指向中央的链接，磁盘上只有一份内容（推荐）。复制：真的复制一份，之后两边各自独立 —— 只在前一种建不起来时用。')}
                  </div>
                </div>
                <div className='sk-field-control'>
                  <Switch
                    checked={values.mountMode === 'copy'}
                    onCheckedChange={next => void save({ mountMode: next ? 'copy' : 'junction' })}
                  />
                  <span className='sk-field-value'>{values.mountMode === 'copy' ? t('复制副本') : t('目录链接')}</span>
                </div>
              </div>

              <div className='sk-field'>
                <div className='sk-field-copy'>
                  <div className='sk-field-label'>{t('相似度提示（L3）')}</div>
                  <div className='sk-field-hint'>
                    {t('按描述文本的余弦相似度列出疑似重复。默认关：0.85 在「把 X 转成 Y」这类写法上误报率很高，自动合并会造成技能莫名消失。')}
                  </div>
                </div>
                <div className='sk-field-control'>
                  <Switch
                    checked={values.l3Enabled}
                    onCheckedChange={next => void save({ l3Enabled: next })}
                  />
                  {values.l3Enabled ? (
                    <input
                      className='sk-input sk-input-num'
                      type='number' min={0.3} max={0.99} step={0.01}
                      defaultValue={values.l3Threshold}
                      onBlur={event => {
                        const next = Number(event.target.value)
                        if (Number.isFinite(next)) void save({ l3Threshold: next })
                      }}
                    />
                  ) : null}
                </div>
              </div>
            </div>
          ) : null}

          {/* ══ 回收站与存储 ══ */}
          {section === 'trash' ? (
            <div className='sk-cfg-panel'>
              <div className='sk-cfg-panel-title'>{t('回收站')}</div>
              <div className='sk-cfg-panel-hint'>{t('删技能、原位转挂载、冲突裁决落选 —— 三处移除的真实内容都先进这里')}</div>

              <div className='sk-field'>
                <div className='sk-field-copy'>
                  <div className='sk-field-label'>{t('回收站保留天数')}</div>
                  <div className='sk-field-hint'>
                    {t('被删的真身、被替换的旧版本都先进回收站。删错了没有第二次机会（内容只有你自己写得出来），所以这里别设太小。')}
                  </div>
                </div>
                <div className='sk-field-control'>
                  <input
                    className='sk-input sk-input-num'
                    type='number' min={0} max={3650}
                    defaultValue={values.trashDays}
                    onBlur={event => {
                      const next = Number(event.target.value)
                      if (Number.isFinite(next)) void save({ trashDays: Math.max(0, Math.floor(next)) })
                    }}
                  />
                  <span className='sk-field-value'>{t('天')}</span>
                </div>
              </div>

              <div className='sk-cfg-trash-head'>
                <span className='sk-field-label'>{t('回收站内容')}</span>
                <Button
                  type='button' size='sm' variant='ghost'
                  disabled={busy === 'purge'}
                  onClick={() => {
                    setBusy('purge')
                    void s.purgeTrash()
                      .then(result => { toast(t('清掉 {n} 项', { n: result.removed }), 'ok'); trash.reload(); onChanged() })
                      .catch(error => toast(t('清理失败：{error}', { error: String(error) }), 'err'))
                      .finally(() => setBusy(''))
                  }}
                >
                  {t('清理过期项')}
                </Button>
              </div>
              {trash.loading && !trash.data ? (
                <Empty>{t('正在读取…')}</Empty>
              ) : trashItems.length === 0 ? (
                <Empty>{t('回收站是空的')}</Empty>
              ) : (
                <div className='sk-table'>
                  <div className='sk-table-head'>
                    <span>{t('技能')}</span><span>{t('体积')}</span><span>{t('存了')}</span><span>{t('还剩')}</span><span />
                  </div>
                  {trashItems.map(row => (
                    <div key={row.file} className='sk-table-row'>
                      <span className='sk-cell-name' title={row.file}>{row.name}</span>
                      <span>{sizeText(row.size)}</span>
                      <span>{t('{n} 天前', { n: row.ageDays })}</span>
                      <span>{t('还剩 {n} 天', { n: row.daysLeft })}</span>
                      <span>
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
                          {t('放回中央库')}
                        </Button>
                      </span>
                    </div>
                  ))}
                </div>
              )}

              <div className='sk-cfg-panel-title' style={{ marginTop: 14 }}>{t('位置')}</div>
              <div className='sk-cfg-panel-hint'>{t('这两个目录归技能仓库自己管，别手动往里放别的东西')}</div>
              <div className='sk-kv'>
                <span>{t('中央仓库')}</span><code>{hubPath}</code>
                <span>{t('回收站')}</span><code>{trash.data?.dir || '—'}</code>
              </div>
            </div>
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
function hubDir(trashPath: string | undefined): string {
  if (!trashPath) return '—'
  const hit = trashPath.replace(/[\\/][^\\/]*$/, '')
  return hit && hit !== trashPath ? hit : '—'
}

export { Settings }
