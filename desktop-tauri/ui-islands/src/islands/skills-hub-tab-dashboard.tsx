import * as React from 'react'
import { Badge, Button, MetricCard, MetricGrid, MetricLabel, MetricValue } from '@ui'
import { t } from '../i18n'
import * as s from './skills-hub-api'
import { Empty, HealthBadge, Panel, Section, ago, num, sizeText, toast, useLoad } from './skills-hub-shared'

/**
 * 技能仓库 · 仪表盘。
 *
 * ── 这一页要回答的四个问题 ──────────────────────────────────
 *   1. 收敛到什么程度？（来源条数 / 真身条数 —— 两者的差就是「份消除」）
 *   2. 各渠道通没通？目录在哪？
 *   3. 有什么要人处理的？（待裁决 / 孤儿 / 中央未登记 / 失效挂载）
 *   4. 上次同步是什么时候、做了什么？
 *
 * ── 为什么首屏就把「待办」算出来 ────────────────────────────
 * 「这次能收几个」和「有几个要裁决」只有完整规划算得出来。多走一遍规划换来
 * 首屏就是准确的待办数，比先给一个乐观数字再让用户点开预览更省事 —— 所以
 * 后端 `overview()` 里直接调了 `plan()`。
 *
 * ── 为什么「份消除」要单列一格 ──────────────────────────────
 * 技能数只能说明「有东西」，说明不了「收敛做得怎么样」。`l1Merged` 才是这个
 * 功能创造的价值：它是这一轮扫描里被合并掉的冗余副本数。用户判断「还需要做
 * 什么」时看的正是它与「来源条数 - 真身条数」的对比。
 *
 * ── 为什么渠道用矩阵而不是列表 ──────────────────────────────
 * 渠道是「同一件事在 N 个地方」的形状（每条都有：名字 / 目录 / 技能数 / 挂载数
 * / 通没通）。列表要读两遍才看得出哪个没通；矩阵一列一列对齐，缺口自己跳出来。
 */

type Props = {
  onGo: (tab: 'library' | 'sync' | 'dedup' | 'settings') => void
  onOpenSkill: (name: string) => void
  onChanged: () => void
}

function Dashboard({ onGo, onOpenSkill, onChanged }: Props): React.ReactElement {
  // 仪表盘是全套里唯一「一眼看全现在」的一页，切回来还显示几分钟前的数字等于
  // 这一页白设 —— 显式开启「进页面重拉」。
  const overview = useLoad(() => s.loadOverview(), [], { refreshOnEnter: true })
  const check = useLoad(() => s.loadDailyCheck(), [], { refreshOnEnter: true })
  const [checking, setChecking] = React.useState(false)

  const runCheck = async (autoMount: boolean) => {
    setChecking(true)
    try {
      const result = await s.runDailyCheck(autoMount)
      toast(result.summary, result.ok ? 'ok' : 'err')
      check.reload()
      overview.reload()
      onChanged()
    } catch (error) {
      toast(t('检测失败：{error}', { error: String(error instanceof Error ? error.message : error) }), 'err')
    } finally {
      setChecking(false)
    }
  }

  const data = overview.data
  const last = check.data?.last ?? null

  return (
    <Panel loading={overview.loading} error={overview.error} onRetry={overview.reload}>
      {!data ? <Empty>{t('还没读到技能仓库的状态')}</Empty> : (
        <div className='sk-dash'>
          {/* ── 顶部状态条：路径 + 上次检测 + 三个动作 ── */}
          <div className='sk-hero'>
            <div className='sk-hero-main'>
              <div className='sk-hero-path' title={data.hubDir}>
                <span className='sk-hero-label'>{t('中央仓库')}</span>
                <code>{data.hubDir}</code>
              </div>
              <div className='sk-hero-meta'>
                {last ? (
                  <Badge variant={last.ok ? 'success' : 'warning'} shape='tag'>
                    {last.ok
                      ? t('{ago}检测通过', { ago: ago(last.checkedAt) })
                      : t('{ago}检测有 {n} 项待处理', { ago: ago(last.checkedAt), n: last.brokenMounts + last.conflicts + last.orphans + last.hubExtra })}
                  </Badge>
                ) : (
                  <Badge variant='outline' shape='tag'>{t('尚未运行检测')}</Badge>
                )}
                <span className='sk-mode'>{t('挂载模式：{mode}', { mode: data.mode === 'copy' ? t('复制') : t('目录链接') })}</span>
              </div>
            </div>
            <div className='sk-hero-actions'>
              <Button type='button' size='sm' variant='outline' disabled={checking} onClick={() => void runCheck(false)}>
                {checking ? t('检测中…') : t('立即检测')}
              </Button>
              <Button type='button' size='sm' variant='ghost' onClick={() => onGo('settings')}>
                {t('配置渠道')}
              </Button>
              <Button type='button' size='sm' variant='ghost' onClick={() => onGo('sync')}>
                {t('去同步')}
              </Button>
            </div>
          </div>

          {/* ── 四宫格统计 ── */}
          <MetricGrid>
            <MetricCard>
              <MetricLabel>{t('中央技能库')}</MetricLabel>
              <MetricValue>{t('{n} 个技能', { n: num(data.skillCount) })}</MetricValue>
              <div className='sk-metric-sub'>{t('台账登记 {n} 个', { n: num(data.manifestCount) })}</div>
            </MetricCard>
            <MetricCard>
              <MetricLabel>{t('已连接渠道')}</MetricLabel>
              <MetricValue>
                {t('{hit} / {total}', {
                  hit: num(data.tools.filter(row => row.dir).length),
                  total: num(data.tools.length),
                })}
              </MetricValue>
              <div className='sk-metric-sub'>
                {t('共 {n} 个技能目录', { n: num(data.tools.reduce((sum, row) => sum + row.skillCount, 0)) })}
              </div>
            </MetricCard>
            <MetricCard>
              <MetricLabel>{t('份消除')}</MetricLabel>
              <MetricValue>{t('{n} 份', { n: num(data.l1Merged) })}</MetricValue>
              <div className='sk-metric-sub'>
                {t('来源 {src} 处 → 真身 {n} 份', { src: num(data.sourceCount), n: num(data.skillCount) })}
              </div>
            </MetricCard>
            <MetricCard>
              <MetricLabel>{t('待处理')}</MetricLabel>
              <MetricValue>
                {t('{n} 项', {
                  n: num(data.pendingConflicts.length + data.orphans.length + data.hubExtra.length + data.brokenMounts),
                })}
              </MetricValue>
              <div className='sk-metric-sub'>
                {t('裁决 {c} · 孤儿 {o} · 未登记 {h} · 失效挂载 {b}', {
                  c: num(data.pendingConflicts.length),
                  o: num(data.orphans.length),
                  h: num(data.hubExtra.length),
                  b: num(data.brokenMounts),
                })}
              </div>
            </MetricCard>
          </MetricGrid>

          {/* ── 渠道矩阵 ── */}
          <Section
            title={t('渠道连接')}
            hint={t('每个 AI 客户端都有一个自己的技能目录；连通才有东西可收敛')}
            action={
              <Button type='button' size='sm' variant='ghost' onClick={() => onGo('settings')}>
                {t('管理渠道')}
              </Button>
            }
          >
            {data.tools.length === 0 ? (
              <Empty>{t('没有可用的渠道，先去配置页加一个目录')}</Empty>
            ) : (
              <div className='sk-matrix'>
                <div className='sk-matrix-head'>
                  <span>{t('渠道')}</span>
                  <span>{t('目录')}</span>
                  <span className='sk-num'>{t('技能')}</span>
                  <span className='sk-num'>{t('挂载')}</span>
                  <span>{t('状态')}</span>
                </div>
                {data.tools.map(row => (
                  <div key={row.id} className='sk-matrix-row'>
                    <span className='sk-matrix-name'>
                      {row.name}
                      {row.builtin ? null : <Badge variant='outline' shape='tag'>{t('自定义')}</Badge>}
                    </span>
                    <code className='sk-matrix-dir' title={row.dir ?? row.candidatePaths.join('\n')}>
                      {row.dir ?? t('未找到（候选：{list}）', { list: row.candidatePaths.join(' / ') || t('无') })}
                    </code>
                    <span className='sk-num'>{num(row.skillCount)}</span>
                    <span className='sk-num'>{num(row.mountCount)}</span>
                    <span>
                      {row.dir ? (
                        <Badge variant='success' shape='tag'>{t('已连接')}</Badge>
                      ) : row.enabled ? (
                        <Badge variant='outline' shape='tag'>{t('未找到目录')}</Badge>
                      ) : (
                        <Badge variant='ghost' shape='tag'>{t('已停用')}</Badge>
                      )}
                    </span>
                  </div>
                ))}
              </div>
            )}
          </Section>

          {/* ── 待办清单 ── */}
          <div className='sk-dash-cols'>
            <Section
              title={t('待裁决')}
              hint={t('同名不同内容、链接指向异常 —— 只有你能决定留哪一份')}
              action={
                <Button type='button' size='sm' variant='ghost' onClick={() => onGo('dedup')}>
                  {t('去裁决')}
                </Button>
              }
            >
              {data.pendingConflicts.length === 0 ? (
                <Empty>{t('没有待裁决项')}</Empty>
              ) : (
                <ul className='sk-mini-list'>
                  {data.pendingConflicts.slice(0, 6).map(item => (
                    <li key={item.id}>
                      <span className='sk-mini-title'>{item.title}</span>
                      <span className='sk-mini-sub'>{item.detail}</span>
                    </li>
                  ))}
                </ul>
              )}
            </Section>

            <Section
              title={t('孤儿与未登记')}
              hint={t('工具目录里有、台账里没有；或中央有目录、台账没记 —— 都只标记，不动')}
            >
              {data.orphans.length === 0 && data.hubExtra.length === 0 ? (
                <Empty>{t('没有游离目录')}</Empty>
              ) : (
                <ul className='sk-mini-list'>
                  {data.orphans.slice(0, 4).map(item => (
                    <li key={`o-${item.tool}-${item.name}`}>
                      <span className='sk-mini-title'>{item.name}</span>
                      <span className='sk-mini-sub'>
                        {t('孤儿 · {tool} · {ago}', { tool: item.tool, ago: ago(item.mtimeMs) })}
                      </span>
                    </li>
                  ))}
                  {data.hubExtra.slice(0, 4).map(item => (
                    <li key={`h-${item.name}`}>
                      <span className='sk-mini-title'>{item.name}</span>
                      <span className='sk-mini-sub'>
                        {item.isLink ? t('中央未登记 · 悬空链接') : t('中央未登记 · {size}', { size: sizeText(item.size) })}
                      </span>
                    </li>
                  ))}
                </ul>
              )}
            </Section>
          </div>

          {/* ── 快捷技能速览 ── */}
          {data.mountHealth.some(item => !item.valid) ? (
            <Section
              title={t('失效挂载')}
              hint={t('台账说已挂载，但链接丢了或指错了地方 —— 客户端现在读不到这个技能')}
            >
              <div className='sk-chip-row'>
                {data.mountHealth.filter(item => !item.valid).map(item => (
                  <button
                    key={`${item.skill}-${item.path}`}
                    type='button'
                    className='sk-chip sk-chip-bad'
                    onClick={() => onOpenSkill(item.skill)}
                  >
                    {item.skill}
                    <span className='sk-chip-sub'>{item.tool}</span>
                  </button>
                ))}
              </div>
            </Section>
          ) : null}

          {/* ── 最近同步 ── */}
          <Section
            title={t('最近同步')}
            hint={t('每次同步都会落一份 Markdown 报告，含改了哪些文件')}
            action={
              <Button type='button' size='sm' variant='ghost' onClick={() => onGo('sync')}>
                {t('同步记录')}
              </Button>
            }
          >
            {data.recentReports.length === 0 ? (
              <Empty>{t('还没同步过')}</Empty>
            ) : (
              <ul className='sk-mini-list'>
                {data.recentReports.slice(0, 5).map(row => (
                  <li key={row.file}>
                    <span className='sk-mini-title'>{row.file}</span>
                    <span className='sk-mini-sub'>{ago(row.mtimeMs)} · {sizeText(row.size)}</span>
                  </li>
                ))}
              </ul>
            )}
          </Section>
        </div>
      )}
    </Panel>
  )
}

export { Dashboard }