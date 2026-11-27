import * as React from 'react'
import { Badge, Button } from '@ui'
import { t } from '../i18n'
import * as s from './skills-hub-api'
import { Empty, Panel, Section, errorText, num, toast, useLoad } from './skills-hub-shared'

/**
 * 技能仓库 · 去重与冲突（处理页）。
 *
 * ── 这一页必须覆盖「待处理」那格的全部四类来源 ──────────────
 * 仪表盘「待处理优化」的 N = 冲突 + 孤儿 + 未登记 + 挂载失效。原来这页只渲染
 * 冲突 / 已合并 / 相似度，后三类一条都不显示 —— 于是仪表盘写「4 项」、副标题
 * 三个 0、点「去裁决」进来是空白页。用户没法判断是「真的没问题」还是「页面坏了」。
 *
 * 现在的对应关系（每类都有真实接口，不是只列出来）：
 *
 * | 类别 | 接口 | 动作粒度 |
 * |---|---|---|
 * | 冲突（同名异容 / 链接异常 / 疑似同一） | `resolveConflict(id, choice)` | 逐条 |
 * | 挂载失效（链接指不到中央真身） | `repairMounts()` | **全量**，无单条修复接口 |
 * | 中央未登记（中央有目录、台账无记录） | `adoptSkill(name)` | 逐条 |
 * | 孤儿目录（工具里有、中央没有） | 无收纳接口，跳技能详情 | 逐条 |
 *
 * 孤儿为什么不做「一键收纳」：收纳 = 把工具目录的内容搬进中央并登记，
 * 是同步（`runSync`）的职责，单个孤儿走同步会把整批一起收进来。这里只负责
 * 让人看见它、并能跳到技能详情去处理，避免造一个语义含糊的按钮。
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
  // 后三类问题只在 `overview` 里 —— 冲突接口不返回它们。两个请求并行，各自独立
  // 报错：overview 挂了不该把冲突列表也一起拖成错误页。
  const overview = useLoad(() => s.loadOverview(), [enterKey], { refreshOnEnter: true })
  const [busy, setBusy] = React.useState('')
  const [expanded, setExpanded] = React.useState('')

  const act = async (id: string, choice: string) => {
    setBusy(id)
    try {
      const result = await s.resolveConflict(id, choice)
      toast(result.message, 'ok')
      conflicts.reload()
      overview.reload()
      onChanged()
    } catch (error) {
      toast(t('裁决失败：{error}', { error: errorText(error) }), 'err')
    } finally {
      setBusy('')
    }
  }

  // 修复挂载只有全量接口（`mount::repair_all`），没有「修这一条」。所以这一区
  // 只给一个按钮，不假装能逐行点 —— 界面上放一排点了没用的按钮比没有按钮更糟。
  // 失败明细必须展示：只 toast 修好的条数，外链没被接管 / 真实目录没被摘
  // 这些「为什么还剩」的原因就全被吞掉了。
  const [repairNote, setRepairNote] = React.useState('')
  const repair = async () => {
    setBusy('repair')
    try {
      const result = await s.repairMounts()
      const failed = result.failed ?? []
      if (failed.length > 0) {
        setRepairNote(failed.map(item => `${item.skill} @ ${item.tool}：${item.message}`).join('\n'))
        toast(t('部分失败：{n} 处未修复', { n: num(failed.length) }), 'err')
      } else {
        setRepairNote('')
        toast(t('重建完成：修复 {n} 处', { n: num(result.repaired) }), 'ok')
      }
      overview.reload()
      onChanged()
    } catch (error) {
      toast(t('重建失败：{error}', { error: errorText(error) }), 'err')
    } finally {
      setBusy('')
    }
  }

  // 解除登记：只删台账，不动磁盘链接。用于别的中枢在管的链接位 ——
  // 摘掉重挂会被抢回去，两个软件抢同一条路径。
  const unregister = async (name: string, tool: string) => {
    setBusy(`unregister:${tool}:${name}`)
    try {
      const result = await s.unregisterMount(name, tool)
      toast(t('已解除 {name} 在 {tool} 的挂载登记', { name, tool }), 'ok')
      overview.reload()
      onChanged()
    } catch (error) {
      toast(t('解除失败：{error}', { error: errorText(error) }), 'err')
    } finally {
      setBusy('')
    }
  }

  // `adopt` 只对「中央已有真实目录、但台账里没登记」的条目有效（后端会拒绝链接与
  // 不存在的目录），正好就是 `hubExtra` 这一类，所以只在这里给按钮。
  const adopt = async (name: string) => {
    setBusy(`adopt:${name}`)
    try {
      await s.adoptSkill(name)
      toast(t('已纳入台账：{name}', { name }), 'ok')
      overview.reload()
      onChanged()
    } catch (error) {
      toast(t('纳入失败：{error}', { error: errorText(error) }), 'err')
    } finally {
      setBusy('')
    }
  }

  const items = conflicts.data?.items ?? []
  const duplicates = conflicts.data?.duplicates ?? []
  const hints = conflicts.data?.hints ?? []

  // 失效 = 不是有效挂载、也不是外部链接。外链是别的中枢在管的健康链接，
  // 放进「失效」会让用户点「全部重建」，然后看它修完又回来。
  const broken = (overview.data?.mountHealth ?? []).filter(item => !item.valid && !item.external)
  const externals = (overview.data?.mountHealth ?? []).filter(item => item.external)
  const extras = overview.data?.hubExtra ?? []
  const orphans = overview.data?.orphans ?? []
  const clean =
    items.length === 0 && duplicates.length === 0 && hints.length === 0
    && broken.length === 0 && extras.length === 0 && orphans.length === 0

  return (
    <Panel loading={conflicts.loading} error={conflicts.error} onRetry={conflicts.reload}>
      <div className='sk-dedup'>
        {clean ? (
          <Empty>{t('没有待处理项。同名异容、链接异常、挂载失效、孤儿目录都会出现在这里。')}</Empty>
        ) : null}

        {items.length > 0 ? (
          <Section
            title={t('待裁决的冲突')}
            hint={t('{n} 条。同名异容必须选一边，链接异常只需重建链接，疑似同一只是提示。', { n: num(items.length) })}
          >
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
                          {open ? t('收起') : t('展开')}
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
          </Section>
        ) : null}

        {broken.length > 0 || repairNote ? (
          <Section
            title={t('挂载失效')}
            hint={t('{n} 处。客户端目录里的链接指不到中央真身 —— 重建只改链接，不动内容。', { n: num(broken.length) })}
            action={(
              <Button type='button' size='sm' variant='outline' disabled={busy === 'repair'} onClick={() => void repair()}>
                {t('全部重建')}
              </Button>
            )}
          >
            {repairNote ? (
              <div className='sk-conflict-detail' style={{ whiteSpace: 'pre-line' }}>
                <b>{t('失败明细')}</b>
                {repairNote}
              </div>
            ) : null}
            {broken.length > 0 ? (
              <div className='sk-table'>
                <div className='sk-table-head'>
                  <span>{t('技能')}</span>
                  <span>{t('渠道')}</span>
                  <span>{t('链接路径')}</span>
                  <span>{t('操作')}</span>
                </div>
                {broken.map(item => (
                  <div key={`${item.skill}-${item.tool}-${item.path}`} className='sk-table-row'>
                    <span className='sk-cell-name'>{item.skill}</span>
                    <span><Badge variant='outline' shape='tag'>{item.tool}</Badge></span>
                    <span className='sk-cell-note'>
                      <code>{item.path}</code>
                      {item.centralExists === false ? <span className='sk-muted'> {t('中央真身也不在了')}</span> : null}
                    </span>
                    <span className='sk-row-actions'>
                      <Button type='button' size='sm' variant='ghost' onClick={() => onOpenSkill(item.skill)}>
                        {t('看详情')}
                      </Button>
                    </span>
                  </div>
                ))}
              </div>
            ) : null}
          </Section>
        ) : null}

        {externals.length > 0 ? (
          <Section
            title={t('外部链接')}
            hint={t('这是其他软件维护的健康链接，本程序不接管。')}
          >
            <div className='sk-table'>
              <div className='sk-table-head'>
                <span>{t('技能')}</span>
                <span>{t('渠道')}</span>
                <span>{t('链接路径')}</span>
                <span>{t('操作')}</span>
              </div>
              {externals.map(item => (
                <div key={`${item.skill}-${item.tool}-${item.path}`} className='sk-table-row'>
                  <span className='sk-cell-name'>{item.skill}</span>
                  <span><Badge variant='outline' shape='tag'>{item.tool}</Badge></span>
                  <span className='sk-cell-note'><code>{item.path}</code></span>
                  <span className='sk-row-actions'>
                    <Button
                      type='button'
                      size='sm'
                      variant='ghost'
                      disabled={busy === `unregister:${item.tool}:${item.skill}`}
                      onClick={() => void unregister(item.skill, item.tool)}
                    >
                      {t('解除登记')}
                    </Button>
                    <Button type='button' size='sm' variant='ghost' onClick={() => onOpenSkill(item.skill)}>
                      {t('看详情')}
                    </Button>
                  </span>
                </div>
              ))}
            </div>
          </Section>
        ) : null}

        {extras.length > 0 ? (
          <Section
            title={t('中央未登记')}
            hint={t('{n} 个。中央有目录，台账里没有记录 —— 不纳入就永远不参与同步与去重。', { n: num(extras.length) })}
          >
            <div className='sk-table'>
              <div className='sk-table-head'>
                <span>{t('技能')}</span>
                <span>{t('路径')}</span>
                <span>{t('类型')}</span>
                <span>{t('操作')}</span>
              </div>
              {extras.map(item => (
                <div key={item.name} className='sk-table-row'>
                  <span className='sk-cell-name'>{item.name}</span>
                  <span className='sk-cell-note'><code>{item.path}</code></span>
                  <span>
                    {item.isLink
                      ? <Badge variant='warning' shape='tag'>{t('悬空链接')}</Badge>
                      : <Badge variant='outline' shape='tag'>{t('真实目录')}</Badge>}
                  </span>
                  <span className='sk-row-actions'>
                    {item.isLink ? (
                      // 链接不是真身，后端会直接拒绝纳管；给按钮等于给一个必然失败的按钮
                      <Button type='button' size='sm' variant='ghost' onClick={() => onOpenSkill(item.name)}>
                        {t('看详情')}
                      </Button>
                    ) : (
                      <Button
                        type='button'
                        size='sm'
                        variant='outline'
                        disabled={busy === `adopt:${item.name}`}
                        onClick={() => void adopt(item.name)}
                      >
                        {t('纳入台账')}
                      </Button>
                    )}
                  </span>
                </div>
              ))}
            </div>
          </Section>
        ) : null}

        {orphans.length > 0 ? (
          <Section
            title={t('孤儿目录')}
            hint={t('{n} 个。只存在于客户端目录、中央还没有 —— 收纳由同步统一完成，这里只能看。', { n: num(orphans.length) })}
          >
            <div className='sk-table'>
              <div className='sk-table-head'>
                <span>{t('技能')}</span>
                <span>{t('渠道')}</span>
                <span>{t('目录')}</span>
                <span>{t('操作')}</span>
              </div>
              {orphans.map(item => (
                <div key={`${item.tool}-${item.name}`} className='sk-table-row'>
                  <span className='sk-cell-name'>{item.name}</span>
                  <span><Badge variant='outline' shape='tag'>{item.tool}</Badge></span>
                  <span className='sk-cell-note'><code>{item.dir}</code></span>
                  <span className='sk-row-actions'>
                    <Button type='button' size='sm' variant='ghost' onClick={() => onOpenSkill(item.name)}>
                      {t('看详情')}
                    </Button>
                  </span>
                </div>
              ))}
            </div>
          </Section>
        ) : null}

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
