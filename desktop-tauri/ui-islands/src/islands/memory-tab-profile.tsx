import * as React from 'react'
import { Badge, Button } from '@ui'
import { t } from '../i18n'
import * as m from './memory-api'
import { Empty, Panel, Section, StateBadge, timeText, useLoad } from './memory-shared'

/**
 * 画像页：四类长期事实（偏好 / 习惯 / 擅长 / 背景）。
 *
 * ── 画像为什么值得单独一页 ──────────────────────────────────
 * 普通记忆是「关于项目的事实」，画像是「关于这个人的事实」。它的消费方式不同：
 * 前者的读者是下一次会话的模型，后者的读者是**用户自己**——人想确认「系统对
 * 我的理解对不对」。所以这一页的排版重心是「每条结论都能点回证据」，而不是列表
 * 密度。
 *
 * ── 为什么每条都要带证据链 ──────────────────────────────────
 * 模型总结人格最容易犯的错是「从一件事推整个人」。让每条结论都挂着它引用的
 * 记忆 id，用户一眼就能判断「这条是过度推断」并删掉。没有证据链的画像不可信，
 * 不可信就不该被注入。
 *
 * ── 为什么门槛没到时不能强制生成 ────────────────────────────
 * 5 条记忆生成出来的「画像」必然是把偶然当必然。门槛（`deep.personaMinMemories`）
 * 是质量下限，不是性能优化 —— 但用户坚持要看也行，所以「强制生成」按钮保留，
 * 只是明确标注「样本不足，结论可能不准」。
 */

function Profile(): React.ReactElement {
  const { data, loading, error, reload } = useLoad(() => m.loadProfile(), [])
  const [busy, setBusy] = React.useState(false)
  const [actionError, setActionError] = React.useState('')
  const [outcome, setOutcome] = React.useState<m.Outcome | null>(null)

  const build = async () => {
    setBusy(true)
    setActionError('')
    setOutcome(null)
    try {
      const result = await m.buildProfile()
      setOutcome(result.outcome)
      reload()
    } catch (caught) {
      setActionError(caught instanceof Error ? caught.message : String(caught))
    } finally {
      setBusy(false)
    }
  }

  if (loading) return <Panel loading error=''>{null}</Panel>
  if (error) return <Panel loading={false} error={error} onRetry={reload}>{null}</Panel>
  if (!data) return <Empty>{t('读不到画像。')}</Empty>

  const total = data.groups.reduce((sum, group) => sum + group.items.length, 0)

  return (
    <div className='mm-profile'>
      {actionError ? <div className='mm-error'>{actionError}</div> : null}

      <Section
        title={t('画像状态')}
        hint={t('画像由「画像生成」任务从 l2 沉淀里提炼，不是每次对话现算。')}
        action={(
          <Button type='button' size='sm' disabled={busy} onClick={() => void build()}>
            {busy ? t('生成中…') : t('立即重新生成')}
          </Button>
        )}
      >
        <div className='mm-inline-meta'>
          <StateBadge state={data.enabled ? 'ok' : 'idle'} label={data.enabled ? t('画像已启用') : t('画像已关闭')} />
          <StateBadge
            state={data.ready ? 'ok' : 'warn'}
            label={data.ready
              ? t('样本充足（{n} / {min}）', { n: data.total, min: data.minMemories })
              : t('样本不足（{n} / {min}）', { n: data.total, min: data.minMemories })}
          />
          {data.generatedAt
            ? <span className='mm-muted'>{t('上次生成：{when}', { when: data.generatedText ?? timeText(data.generatedAt) })}</span>
            : <span className='mm-muted'>{t('还没生成过')}</span>}
          {data.freshSinceProfile > 0
            ? <Badge variant='warning' shape='tag'>{t('{n} 条新记忆未纳入', { n: data.freshSinceProfile })}</Badge>
            : null}
          {data.evidenceRequired ? <Badge variant='outline' shape='tag'>{t('要求证据链')}</Badge> : null}
        </div>
        {!data.enabled ? (
          <p className='mm-hint'>
            {t('去「配置」页打开「深层画像」（deep.enabled 与 deep.personaEnabled），否则画像任务会直接跳过。')}
          </p>
        ) : null}
        {!data.ready ? (
          <p className='mm-hint'>
            {t('样本少于门槛时生成的结论多半是把偶然当必然。仍然可以强制生成，但结果仅供试验。')}
          </p>
        ) : null}
        {outcome ? <OutcomeLine outcome={outcome} /> : null}
      </Section>

      {total === 0 ? (
        <Empty>
          {t('还没有画像条目。先攒够记忆（当前 {n} 条），再点「立即重新生成」。', { n: data.total })}
        </Empty>
      ) : null}

      <div className='mm-profile-groups'>
        {data.groups.map(group => (
          <Section
            key={group.sub}
            title={group.label}
            hint={group.items.length === 0 ? t('这一类还没有结论') : undefined}
          >
            {group.items.length === 0 ? <div className='mm-muted'>—</div> : (
              <div className='mm-profile-list'>
                {group.items.map(item => (
                  <div key={item.id} className='mm-profile-item'>
                    <div className='mm-profile-title'>
                      {item.title}
                      {item.evidenceCount ? (
                        <Badge variant='outline' shape='tag'>{t('{n} 条证据', { n: item.evidenceCount })}</Badge>
                      ) : (
                        <Badge variant='warning' shape='tag'>{t('无证据')}</Badge>
                      )}
                    </div>
                    <div className='mm-profile-body'>{item.summaryText || item.summary || item.body}</div>
                    <div className='mm-profile-meta'>
                      <span>{timeText(item.updatedAt)}</span>
                      {item.evidence.length ? (
                        <span className='mm-muted'>
                          {t('证据：')}{item.evidence.slice(0, 4).join('、')}{item.evidence.length > 4 ? ' …' : ''}
                        </span>
                      ) : null}
                    </div>
                  </div>
                ))}
              </div>
            )}
          </Section>
        ))}
      </div>
    </div>
  )
}

/** 任务结果的统一呈现：done / skipped / failed 三态文案。 */
export function OutcomeLine({ outcome }: { outcome: m.Outcome }): React.ReactElement {
  if (outcome.status === 'done') {
    return (
      <p className='mm-outcome mm-outcome-done'>
        {t('完成')}
        {outcome.result ? <code>{JSON.stringify(outcome.result).slice(0, 240)}</code> : null}
      </p>
    )
  }
  if (outcome.status === 'skipped') {
    return <p className='mm-outcome mm-outcome-skip'>{t('跳过：{reason}', { reason: outcome.reason ?? '—' })}</p>
  }
  return <p className='mm-outcome mm-outcome-fail'>{t('失败：{reason}', { reason: outcome.error ?? '—' })}</p>
}

export { Profile }
