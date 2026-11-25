import * as React from 'react'
import { Badge, Button, Switch } from '@ui'
import { t } from '../i18n'
import * as s from './skills-hub-api'
import { Empty, Panel, Section, num, sizeText, toast, useLoad } from './skills-hub-shared'

/**
 * 技能仓库 · 配置。
 *
 * ── 为什么这一页把「渠道 / 行为 / 回收站」放一起而不拆成三个页签 ──
 * 这三组旋钮共享同一个判断：**要不要让这个功能动我的磁盘、动到什么程度**。
 * 渠道决定扫哪里、挂载模式决定怎么落、回收站决定删了还能不能救回来 —— 拆开之后
 * 用户会在「渠道页加了目录」之后不知道「它已经开始往那儿写链接了」。放一页能
 * 一次看全后果。
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

function Settings({ onChanged }: Props): React.ReactElement {
  const config = useLoad(() => s.loadConfig(), [])
  const tools = useLoad(() => s.loadTools(), [])
  const trash = useLoad(() => s.loadTrash(), [])
  const probe = useLoad(() => s.loadProbe(), [])
  const [busy, setBusy] = React.useState('')
  const [customPath, setCustomPath] = React.useState('')

  const values = config.data?.values
  const dirRows = tools.data?.items ?? []
  const trashItems = trash.data?.items ?? []
  const suggestions = probe.data?.items ?? []

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

  const addCustom = async () => {
    const path = customPath.trim()
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

  return (
    <Panel loading={config.loading} error={config.error} onRetry={config.reload}>
      {!values ? <Empty>{t('还没读到配置')}</Empty> : (
        <div className='sk-settings'>
          {/* ── 行为 ── */}
          <Section title={t('同步与挂载')} hint={t('这些开关直接决定同步会怎么动你的磁盘')}>
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
                    type='number'
                    min={0.3}
                    max={0.99}
                    step={0.01}
                    defaultValue={values.l3Threshold}
                    onBlur={event => {
                      const next = Number(event.target.value)
                      if (Number.isFinite(next)) void save({ l3Threshold: next })
                    }}
                  />
                ) : null}
              </div>
            </div>

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
                  type='number'
                  min={0}
                  max={3650}
                  defaultValue={values.trashDays}
                  onBlur={event => {
                    const next = Number(event.target.value)
                    if (Number.isFinite(next)) void save({ trashDays: Math.max(0, Math.floor(next)) })
                  }}
                />
                <span className='sk-field-value'>{t('天')}</span>
              </div>
            </div>
          </Section>

          {/* ── 渠道 ── */}
          <Section
            title={t('渠道')}
            hint={t('每个 AI 客户端把技能放在自己家目录下的 skills/ 里。停用一条就不再扫它、也不再往它挂载。')}
            action={
              <Button type='button' size='sm' variant='ghost' onClick={() => { tools.reload(); probe.reload() }}>
                {t('重新探测')}
              </Button>
            }
          >
            <div className='sk-table'>
              <div className='sk-table-head'>
                <span>{t('渠道')}</span>
                <span>{t('实际目录')}</span>
                <span className='sk-num'>{t('技能')}</span>
                <span>{t('渠道启用')}</span>
              </div>
              {dirRows.map(row => (
                <div key={row.id} className='sk-table-row'>
                  <span className='sk-matrix-name'>
                    {row.name}
                    {row.builtin ? null : <Badge variant='outline' shape='tag'>{t('自定义')}</Badge>}
                  </span>
                  <code className='sk-matrix-dir' title={row.dir ?? row.candidatePaths.join('\n')}>
                    {row.dir ?? t('未找到（候选：{list}）', { list: row.candidatePaths.join(' / ') || t('无') })}
                  </code>
                  <span className='sk-num'>{num(row.skillCount)}</span>
                  <span>
                    <Switch checked={row.enabled} onCheckedChange={next => void toggleTool(row.id, next)} />
                  </span>
                </div>
              ))}
            </div>

            {(values.customDirs ?? []).length > 0 ? (
              <div className='sk-custom-dirs'>
                <div className='sk-field-label'>{t('自定义目录')}</div>
                {values.customDirs.map(path => (
                  <div key={path} className='sk-custom-dir'>
                    <code>{path}</code>
                    <Button
                      type='button'
                      size='sm'
                      variant='ghost'
                      disabled={busy === `drop-${path}`}
                      onClick={() => void dropCustom(path)}
                    >
                      {t('移除')}
                    </Button>
                  </div>
                ))}
              </div>
            ) : null}

            <div className='sk-add-row'>
              <input
                className='sk-input'
                type='text'
                value={customPath}
                placeholder={t('再扫一个目录（支持 ~ 与 %APPDATA%）')}
                onChange={event => setCustomPath(event.target.value)}
                onKeyDown={event => { if (event.key === 'Enter') void addCustom() }}
              />
              <Button type='button' size='sm' variant='outline' disabled={busy === 'custom'} onClick={() => void addCustom()}>
                {t('加入目录')}
              </Button>
            </div>

            {suggestions.length > 0 ? (
              <div className='sk-suggest'>
                <div className='sk-field-label'>{t('探测到这些目录还没接进来')}</div>
                <div className='sk-suggest-list'>
                  {suggestions.map(row => (
                    <div key={row.suggestId} className='sk-suggest-row'>
                      <span className='sk-suggest-name'>{row.name}</span>
                      <code className='sk-suggest-dir' title={row.hitDirs.join('\n')}>{row.firstDir}</code>
                      <span className='sk-muted'>{t('{n} 个技能', { n: row.skillCount })}</span>
                      <Button
                        type='button'
                        size='sm'
                        variant='ghost'
                        disabled={busy === 'custom'}
                        onClick={() => {
                          setCustomPath(row.firstDir)
                          void s.addCustomDir(row.firstDir)
                            .then(() => {
                              setCustomPath('')
                              config.reload()
                              tools.reload()
                              probe.reload()
                              toast(t('已加入 {name}', { name: row.name }), 'ok')
                            })
                            .catch(error => toast(t('加入失败：{error}', { error: String(error) }), 'err'))
                        }}
                      >
                        {t('加进来')}
                      </Button>
                    </div>
                  ))}
                </div>
              </div>
            ) : null}
          </Section>

          {/* ── 回收站 ── */}
          <Section
            title={t('回收站')}
            hint={t('删技能、原位转挂载、冲突裁决落选 —— 三处移除的真实内容都先进这里')}
            action={
              <Button
                type='button'
                size='sm'
                variant='ghost'
                disabled={busy === 'purge'}
                onClick={() => {
                  setBusy('purge')
                  void s.purgeTrash()
                    .then(result => {
                      toast(t('清掉 {n} 项', { n: result.removed }), 'ok')
                      trash.reload()
                      onChanged()
                    })
                    .catch(error => toast(t('清理失败：{error}', { error: String(error) }), 'err'))
                    .finally(() => setBusy(''))
                }}
              >
                {t('清理过期项')}
              </Button>
            }
          >
            {trash.loading && !trash.data ? (
              <Empty>{t('正在读取…')}</Empty>
            ) : trashItems.length === 0 ? (
              <Empty>{t('回收站是空的')}</Empty>
            ) : (
              <div className='sk-table'>
                <div className='sk-table-head'>
                  <span>{t('技能')}</span>
                  <span>{t('体积')}</span>
                  <span>{t('存了')}</span>
                  <span>{t('还剩')}</span>
                  <span />
                </div>
                {trashItems.map(row => (
                  <div key={row.file} className='sk-table-row'>
                    <span className='sk-cell-name' title={row.file}>{row.name}</span>
                    <span>{sizeText(row.size)}</span>
                    <span>{t('{n} 天前', { n: row.ageDays })}</span>
                    <span>{t('还剩 {n} 天', { n: row.daysLeft })}</span>
                    <span>
                      <Button
                        type='button'
                        size='sm'
                        variant='ghost'
                        disabled={busy === `restore-${row.file}`}
                        onClick={() => {
                          setBusy(`restore-${row.file}`)
                          void s.restoreTrash(row.file)
                            .then(result => {
                              toast(t('已还原「{name}」到中央', { name: result.name }), 'ok')
                              trash.reload()
                              onChanged()
                            })
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
          </Section>

          {/* ── 位置 ── */}
          <Section title={t('位置')} hint={t('这两个目录归技能仓库自己管，别手动往里放别的东西')}>
            <div className='sk-kv'>
              <span>{t('中央仓库')}</span>
              <code>{hubDir(trash.data?.dir)}</code>
              <span>{t('回收站')}</span>
              <code>{trash.data?.dir || '—'}</code>
            </div>
          </Section>
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