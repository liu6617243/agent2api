import * as React from 'react'
import { Badge, Button } from '@ui'
import { t } from '../i18n'
import * as s from './skills-hub-api'
import { Empty, Panel, Section, errorText, num, toast, useLoad } from './skills-hub-shared'

/**
 * 技能仓库 · 去重与冲突（裁决页）。
 *
 * ── 这一页只做一件事：裁决冲突 ──────────────────────────────
 *
 * 参考 AgentHub（`SkillsDedupView`）的站位：它的「去重与冲突」页从上到下只有
 * 三块 —— 一行漏斗图标的汇总（「本轮扫描自动合并了 N 组重复副本；剩下 M 条
 * 疑似冲突需要你逐条确认」）、一个「需要你决定的冲突（M）」列表、以及一个
 * 「没有待裁决的冲突」空状态。**没有**挂载失效、没有外部链接、没有中央未登记、
 * 没有孤儿目录。
 *
 * 原因是这些事需要的动作性质完全不同，混在一页会同时压垮两件事：
 *   * 冲突要人对着两份内容做判断，必须一次只看一条；
 *   * 挂载失效 / 未登记 / 外部链接是机械动作，丢进同一个列表会让人以为
 *     「裁决」按钮也能顺手把这些办了。
 *
 * 所以 Agent2API 也照这个分工拆（2026-11-28，用户报「去重界面和 AgentHub
 * 不一样」）：
 *
 * | 事项 | 归属页面 | 形态 |
 * |---|---|---|
 * | 冲突（同名异容 / 链接异常 / 疑似同一） | 本页 | 逐条裁决 |
 * | 已合并 / 相似度提示 | 本页 | 只读展示（证明「机器做了什么」） |
 * | 挂载失效、外部链接、中央未登记、孤儿目录 | 仪表盘「待办」区 | 逐条动作（纳管 / 解除登记 / 看详情） |
 *
 * 仪表盘那边已经具备全部动作入口（逐条纳管 / 解除登记 / 看详情），失效挂载另有
 * 「挂载与去重」卡上的「一键修复」，所以这里删掉这些区块不会让任何事项失去处理
 * 路径。**仪表盘刻意不提供跨类「一键处理」**：AgentHub 也没有，而且它会让用户
 * 以为冲突也能一键办掉，而冲突永远需要人做选择。
 *
 * ── 三类冲突的处置动作完全不同 ──────────────────────────────
 * * **content**（同名不同内容）：必须选一边。四个动作：保留中央版 / 保留工具版 /
 *   双保留（改名）- 忽略。选哪边都是「删一份内容」，所以两边 SKILL.md 摘要并排
 *   摆出来，用户对着看。
 * * **diff-link**（位置上是链接，但指错了地方）：先给出「现在指向哪」「中央真身
 *   在哪」，再让用户选重建还是纳管。这个动作不动内容，只重建链接。
 * * **norm**（名称归一后疑似同一技能，内容不同）：纯提示。两个名字指同一技能的
 *   情况确实存在（`gsap` / `gsap-skill`），但不能自动合并 —— 合并了用户会丢掉
 *   一份内容。所以只有「确认同一 / 标记不同」两个动作，都不动文件。
 *
 * ── 为什么裁决面板要给「差异」而不是让用户去翻文件 ──────────
 * 用户此刻要回答的问题是「这两个是不是同一个技能」。让他打开两个目录逐字比对
 * 是在惩罚他。把两边 SKILL.md 的 frontmatter 与描述并排、把 hash 摆出来，
 * 判断成本降到一眼。
 *
 * ── 为什么「忽略」永远存在 ──────────────────────────────────
 * 冲突队列不是任务清单，是**提醒**。有些冲突用户就是打算两个都留着（比如故意
 * 维护两个变体）。没有「忽略」，队列会永远挂着几条，直到用户开始无视整个列表。
 */

function label(kind: string): string {
  switch (kind) {
    case 'content':
      return t('同名不同内容')
    case 'diff-link':
      return t('链接指向异常')
    case 'norm':
      return t('疑似同一技能')
    case 'missing':
      return t('中央真身丢失')
    default:
      return kind
  }
}

function variantOf(kind: string): 'destructive' | 'warning' | 'outline' {
  if (kind === 'content' || kind === 'missing') return 'destructive'
  if (kind === 'diff-link') return 'warning'
  return 'outline'
}

type Props = { onChanged: () => void; onOpenSkill: (name: string) => void; enterKey: number }

function Dedup({ onChanged, onOpenSkill, enterKey }: Props): React.ReactElement {
  // `enterKey`：每次切入处理页 +1。保活后第一次挂载的数据会一直留着，
  // `refreshOnEnter` 又只认整页重进 —— 不加这个信号，「仪表盘有 4 项、
  // 点进来是空的」会重现（切进来看到的是上一次的旧快照）。
  const conflicts = useLoad(() => s.loadConflicts(), [enterKey], { refreshOnEnter: true })
  const [busy, setBusy] = React.useState('')
  const [expanded, setExpanded] = React.useState('')

  const act = async (id: string, choice: string) => {
    setBusy(id)
    try {
      const result = await s.resolveConflict(id, choice)
      toast(result.message, 'ok')
      conflicts.reload()
      onChanged()
    } catch (error) {
      toast(t('裁决失败：{error}', { error: errorText(error) }), 'err')
    } finally {
      setBusy('')
    }
  }

  const items = conflicts.data?.items ?? []
  const duplicates = conflicts.data?.duplicates ?? []
  const hints = conflicts.data?.hints ?? []

  return (
    <Panel loading={conflicts.loading} error={conflicts.error} onRetry={conflicts.reload}>
      <div className='sk-dedup'>
        {/* ── 汇总条（对齐 AgentHub：一行说清「机器做了什么、还剩多少要人拦」）──
            没有这一行时，用户看到空列表无法区分「真的合并了 0 份」与「页面没加载」。
            数字直接来自同一份 `resolveConflict` 快照（duplicates / items），
            不做任何二次推算。 */}
        <div className='sk-dedup-summary'>
          <span className='sk-dedup-summary-glyph' aria-hidden='true'>
            <svg viewBox='0 0 24 24' fill='none' stroke='currentColor' strokeWidth='2' strokeLinecap='round' strokeLinejoin='round'>
              <path d='M3 5h18l-7 8v6l-4 2v-8z' />
            </svg>
          </span>
          <span>
            {t('本轮扫描自动合并了 {n} 组重复副本；剩下 {m} 条疑似冲突需要你逐条确认。', {
              n: num(duplicates.length),
              m: num(items.length),
            })}
          </span>
        </div>

        <Section
          title={t('需要你决定的冲突（{n}）', { n: num(items.length) })}
          action={
            <Button type='button' size='sm' variant='ghost' disabled={conflicts.loading} onClick={conflicts.reload}>
              {t('重新检测')}
            </Button>
          }
        >
          {items.length === 0 ? (
            <Empty>{t('没有待裁决的冲突')}</Empty>
          ) : (
            <div className='sk-conflict-list'>
              {items.map(item => {
                const open = expanded === item.id
                return (
                  <div key={item.id} className={`sk-conflict${open ? ' is-open' : ''}`}>
                    <div className='sk-conflict-head'>
                      <span className='sk-conflict-title'>
                        <Badge variant={variantOf(item.kind)} shape='tag'>{label(item.kind)}</Badge>
                        <span>{item.title}</span>
                      </span>
                      <span className='sk-row-actions'>
                        {item.skill ? (
                          <Button type='button' size='sm' variant='ghost' onClick={() => onOpenSkill(item.skill!)}>
                            {t('看详情')}
                          </Button>
                        ) : null}
                        <Button
                          type='button'
                          size='sm'
                          variant='ghost'
                          onClick={() => setExpanded(open ? '' : item.id)}
                        >
                          {open ? t('收起') : t('查看 / 裁决')}
                        </Button>
                      </span>
                    </div>
                    <div className='sk-conflict-detail'>{item.detail}</div>

                    {open ? (
                      <div className='sk-conflict-body'>
                        {item.kind === 'content' ? (
                          <div className='sk-diff-cols'>
                            <div className='sk-diff-col'>
                              <div className='sk-diff-col-head'>{t('中央版（唯一真身）')}</div>
                              <code className='sk-diff-path'>{item.centralPath ?? '—'}</code>
                              <pre className='sk-diff-md'>{item.centralSkillMd || t('（读不到 SKILL.md）')}</pre>
                            </div>
                            <div className='sk-diff-col'>
                              <div className='sk-diff-col-head'>{t('工具版（{tool}）', { tool: item.toolId ?? '—' })}</div>
                              <code className='sk-diff-path'>{item.linkPath ?? item.dir ?? '—'}</code>
                              <pre className='sk-diff-md'>{item.toolSkillMd || t('（读不到 SKILL.md）')}</pre>
                            </div>
                          </div>
                        ) : null}

                        {item.kind === 'diff-link' ? (
                          <div className='sk-kv'>
                            <span>{t('现有链接')}</span><code>{item.linkPath ?? '—'}</code>
                            <span>{t('现在指向')}</span><code>{item.linkTarget || t('（读不出）')}</code>
                            <span>{t('中央真身')}</span>
                            <code>{item.centralPath ?? '—'} {item.centralExists ? '' : t('（不存在）')}</code>
                          </div>
                        ) : null}

                        {item.kind === 'norm' ? (
                          <div className='sk-kv'>
                            <span>{t('归一后的键')}</span><code>{item.key ?? '—'}</code>
                            <span>{t('两个名字')}</span><code>{item.a ?? '—'} / {item.b ?? '—'}</code>
                          </div>
                        ) : null}

                        <div className='sk-conflict-actions'>
                          {item.kind === 'content' ? (
                            <>
                              <Button type='button' size='sm' variant='outline' disabled={busy === item.id} onClick={() => void act(item.id, 'keepCentral')}>
                                {t('保留中央版（工具版进回收站）')}
                              </Button>
                              <Button type='button' size='sm' variant='outline' disabled={busy === item.id} onClick={() => void act(item.id, 'keepTool')}>
                                {t('保留工具版（覆盖中央）')}
                              </Button>
                              <Button type='button' size='sm' variant='outline' disabled={busy === item.id} onClick={() => void act(item.id, 'keepBoth')}>
                                {t('两份都留（改个名）')}
                              </Button>
                            </>
                          ) : null}
                          {item.kind === 'diff-link' ? (
                            <>
                              <Button type='button' size='sm' variant='outline' disabled={busy === item.id} onClick={() => void act(item.id, 'rebuildLink')}>
                                {t('重建为指向中央')}
                              </Button>
                              <Button type='button' size='sm' variant='outline' disabled={busy === item.id} onClick={() => void act(item.id, 'adoptLink')}>
                                {t('把它指向的那份收进中央')}
                              </Button>
                            </>
                          ) : null}
                          {item.kind === 'norm' ? (
                            <>
                              <Button type='button' size='sm' variant='outline' disabled={busy === item.id} onClick={() => void act(item.id, 'same')}>
                                {t('确认是同一技能')}
                              </Button>
                              <Button type='button' size='sm' variant='outline' disabled={busy === item.id} onClick={() => void act(item.id, 'different')}>
                                {t('标记为不同')}
                              </Button>
                            </>
                          ) : null}
                          <Button type='button' size='sm' variant='ghost' disabled={busy === item.id} onClick={() => void act(item.id, 'ignore')}>
                            {t('忽略（不动任何文件）')}
                          </Button>
                        </div>
                      </div>
                    ) : null}
                  </div>
                )
              })}
            </div>
          )}
        </Section>

        {duplicates.length > 0 ? (
          <Section
            title={t('已自动合并的重复')}
            hint={t('内容树哈希完全一致 —— 同一份内容换个名字出现，「它们是同一个技能」没有歧义，不需要人点')}
          >
            <div className='sk-table'>
              <div className='sk-table-head'>
                <span>{t('保留')}</span>
                <span>{t('合并掉')}</span>
                <span>{t('规则')}</span>
                <span>{t('判据')}</span>
              </div>
              {duplicates.map((row, at) => (
                <div key={`${row.kept.tool}-${row.removed.tool}-${at}`} className='sk-table-row'>
                  <span className='sk-cell-name'>{row.kept.name} <span className='sk-muted'>（{row.kept.tool}）</span></span>
                  <span className='sk-cell-name'>{row.removed.name} <span className='sk-muted'>（{row.removed.tool}）</span></span>
                  <span><Badge variant='success' shape='tag'>{row.rule}</Badge></span>
                  <span className='sk-cell-note'>{row.basis}</span>
                </div>
              ))}
            </div>
          </Section>
        ) : null}

        {hints.length > 0 ? (
          <Section
            title={t('相似度提示（L3）')}
            hint={t('描述文本的余弦相似度。0.85 在「把 X 转成 Y」这类写法上误报率很高，所以只提示、不自动合并')}
          >
            <div className='sk-chip-row'>
              {hints.map((hint, at) => (
                <span key={`${hint.a}-${hint.b}-${at}`} className='sk-chip'>
                  {hint.a} ≈ {hint.b}
                  <span className='sk-chip-sub'>{hint.sim}</span>
                </span>
              ))}
            </div>
          </Section>
        ) : null}
      </div>
    </Panel>
  )
}

export { Dedup }
