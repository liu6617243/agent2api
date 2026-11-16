import * as React from 'react'
import { Badge, Button, Input } from '@ui'
import { t } from '../i18n'
import * as m from './memory-api'
import { Empty, Panel, Section, timeText } from './memory-shared'

/**
 * 项目页：一张卡一个项目，能改名、置顶、归档、合并。
 *
 * ── 项目卡为什么比「按 project 字段分组」重要 ────────────────
 * 记忆的 `project` 是 slug（`owner--repo` 这种机器名），而人认的是项目名与远程
 * 地址。卡把两者摆在一起，并且允许人工纠正 —— 自动归类必然会有错的时候
 * （同名仓库、monorepo 子目录、换了远程地址），纠正入口必须有。
 *
 * ── 合并为什么要二次确认 ────────────────────────────────────
 * 合并会**逐条搬文件**：记忆的 frontmatter 里 `project` 要改，`.md` 要移到新目录。
 * 这是这套系统里唯一会批量改文件路径的操作，点错一次要手工收拾。所以确认框
 * 里明写「会迁移 N 条」。
 *
 * ── 为什么归档而不是删除 ────────────────────────────────────
 * 项目做完了，记忆仍然有检索价值（「这个仓库当年为什么放弃」）。归档 = 不再
 * 接受新记忆、默认折叠，但内容一条不删。
 */

type ProjectsProps = {
  projects: m.MemoryProject[]
  loading: boolean
  error: string
  onReload: () => void
  onOpenEntry: (id: string) => void
  onChanged: () => void
}

function Projects({
  projects,
  loading,
  error,
  onReload,
  onOpenEntry,
  onChanged,
}: ProjectsProps): React.ReactElement {
  const [renaming, setRenaming] = React.useState<string | null>(null)
  const [nameDraft, setNameDraft] = React.useState('')
  const [merging, setMerging] = React.useState<m.MemoryProject | null>(null)
  const [mergeInto, setMergeInto] = React.useState('')
  const [busy, setBusy] = React.useState(false)
  const [localError, setLocalError] = React.useState('')
  const [showArchived, setShowArchived] = React.useState(false)

  const act = async (task: () => Promise<unknown>) => {
    setBusy(true)
    setLocalError('')
    try {
      await task()
      onReload()
      onChanged()
    } catch (caught) {
      setLocalError(caught instanceof Error ? caught.message : String(caught))
    } finally {
      setBusy(false)
    }
  }

  const visible = projects.filter(item => showArchived || !item.archived)
  const archived = projects.filter(item => item.archived).length

  return (
    <div className='mm-projects-page'>
      {localError ? <div className='mm-error'>{localError}</div> : null}

      <div className='mm-filters'>
        <span className='mm-muted'>
          {t('{n} 个项目，按 git 远程自动归档；同名仓库在不同机器上会合并成同一个 slug。', { n: projects.length })}
        </span>
        {archived > 0 ? (
          <Button type='button' size='sm' variant='ghost' onClick={() => setShowArchived(value => !value)}>
            {showArchived ? t('隐藏已归档（{n}）', { n: archived }) : t('显示已归档（{n}）', { n: archived })}
          </Button>
        ) : null}
        <Button type='button' size='sm' variant='ghost' onClick={onReload}>{t('刷新')}</Button>
      </div>

      {loading ? <Panel loading error=''>{null}</Panel> : null}
      {error ? <Panel loading={false} error={error} onRetry={onReload}>{null}</Panel> : null}

      {!loading && !error && visible.length === 0 ? (
        <Empty>
          {t('还没有项目。写入记忆时带上项目名（或在项目目录里让 Agent 调用 memory_write）就会自动登记。')}
        </Empty>
      ) : null}

      <div className='mm-projects'>
        {visible.map(item => (
          <div key={item.slug} className={`mm-project-card${item.archived ? ' archived' : ''}`}>
            <div className='mm-project-head'>
              {renaming === item.slug ? (
                <div className='mm-rename'>
                  <Input
                    value={nameDraft}
                    autoFocus
                    onChange={event => setNameDraft(event.target.value)}
                    onKeyDown={event => {
                      if (event.key === 'Enter') {
                        void act(async () => {
                          await m.updateProject(item.slug, { name: nameDraft })
                          setRenaming(null)
                        })
                      }
                      if (event.key === 'Escape') setRenaming(null)
                    }}
                  />
                  <Button type='button' size='sm' onClick={() => void act(async () => {
                    await m.updateProject(item.slug, { name: nameDraft })
                    setRenaming(null)
                  })}>{t('保存')}</Button>
                  <Button type='button' size='sm' variant='ghost' onClick={() => setRenaming(null)}>{t('取消')}</Button>
                </div>
              ) : (
                <>
                  <strong>{item.name}</strong>
                  <div className='mm-project-badges'>
                    {item.pinned ? <Badge variant='destructive' shape='tag'>{t('置顶')}</Badge> : null}
                    {item.archived ? <Badge variant='outline' shape='tag'>{t('已归档')}</Badge> : null}
                    <Badge variant='secondary' shape='tag'>{t('{n} 条', { n: item.count })}</Badge>
                  </div>
                </>
              )}
            </div>

            <div className='mm-project-meta'>
              <span>slug: {item.slug}</span>
              <span>l1 {item.l1} · l2 {item.l2}</span>
              <span>{item.updatedText || timeText(item.updatedAt)}</span>
            </div>

            {item.remote
              ? <div className='mm-project-remote'>{item.remote}</div>
              : <div className='mm-project-remote mm-muted'>{t('未记录远程地址')}</div>}
            {item.path ? <div className='mm-project-path'>{item.path}</div> : null}

            {item.latest.length ? (
              <ul className='mm-project-latest'>
                {item.latest.map(entry => (
                  <li key={entry.id}>
                    <button type='button' className='mm-link' onClick={() => onOpenEntry(entry.id)}>{entry.title}</button>
                  </li>
                ))}
              </ul>
            ) : <div className='mm-muted'>{t('这个项目还没有记忆。')}</div>}

            <div className='mm-project-actions'>
              <Button type='button' size='sm' variant='ghost' onClick={() => {
                setRenaming(item.slug)
                setNameDraft(item.name)
              }}>{t('改名')}</Button>
              <Button type='button' size='sm' variant='ghost' onClick={() => void act(() => m.updateProject(item.slug, { pinned: !item.pinned }))}>
                {item.pinned ? t('取消置顶') : t('置顶')}
              </Button>
              <Button type='button' size='sm' variant='ghost' onClick={() => void act(() => m.updateProject(item.slug, { archived: !item.archived }))}>
                {item.archived ? t('取消归档') : t('归档')}
              </Button>
              {projects.length > 1 && !item.archived ? (
                <Button type='button' size='sm' variant='ghost' onClick={() => {
                  setMerging(item)
                  setMergeInto(projects.find(other => other.slug !== item.slug)?.slug ?? '')
                }}>{t('合并到…')}</Button>
              ) : null}
            </div>
          </div>
        ))}
      </div>

      {merging ? (
        <div className='mm-dialog-mask' role='dialog' aria-modal='true'>
          <div className='mm-dialog mm-dialog-narrow'>
            <div className='mm-dialog-head'>
              <strong>{t('把「{name}」合并到另一个项目', { name: merging.name })}</strong>
              <Button type='button' size='sm' variant='ghost' onClick={() => setMerging(null)}>{t('取消')}</Button>
            </div>
            <div className='mm-dialog-body'>
              <p className='mm-hint'>
                {t('会把 {n} 条记忆的 project 字段与磁盘路径逐条改到目标项目下，然后旧项目卡标记为「已合并」。这个操作会移动文件。', { n: merging.count })}
              </p>
              <label className='mm-label'>{t('目标项目')}</label>
              <select className='mm-select' value={mergeInto} onChange={event => setMergeInto(event.target.value)}>
                {projects.filter(other => other.slug !== merging.slug).map(other => (
                  <option key={other.slug} value={other.slug}>{other.name}（{other.count}）</option>
                ))}
              </select>
            </div>
            <div className='mm-dialog-foot'>
              <Button type='button' variant='ghost' onClick={() => setMerging(null)}>{t('取消')}</Button>
              <Button
                type='button'
                variant='destructive'
                disabled={busy || !mergeInto}
                onClick={() => void act(async () => {
                  await m.mergeProject(merging.slug, mergeInto)
                  setMerging(null)
                })}
              >
                {busy ? t('迁移中…') : t('确认合并')}
              </Button>
            </div>
          </div>
        </div>
      ) : null}
    </div>
  )
}

export { Projects }
