import * as React from 'react'
import { Button, Dialog, DialogBody, DialogContent, DialogHeader, DialogTitle, Input, Switch } from '@ui'
import { errorMessage, forwardUpdateResult, shared, toast } from './update-shared'
import { GithubUpdateSettings } from './update-github-settings'
import { t } from '../i18n'

const TASK_ID = 'updateCheck'
const MIN_INTERVAL = 1
const MAX_INTERVAL = 1440

export function UpdateSettingsDialog({ open, onClose }: { open: boolean; onClose: () => void }) {
  const [checkTask, setCheckTask] = React.useState<{ enabled: boolean; interval: number } | null>(null)
  const [intervalDraft, setIntervalDraft] = React.useState('')
  const [checkBusy, setCheckBusy] = React.useState(false)
  const [beta, setBeta] = React.useState<{ enabled: boolean } | null>(null)
  const [betaBusy, setBetaBusy] = React.useState(false)

  React.useEffect(() => {
    if (!open) return
    let alive = true
    setCheckTask(null)
    setBeta(null)
    void shared().workbuddyDesktop?.getUpdateBeta().then(result => {
      if (alive && result) setBeta({ enabled: result.enabled === true })
    }).catch(error => toast(t('读取设置失败：{error}', { error: errorMessage(error) }), 'err'))
    void shared().workbuddyDesktop?.getScheduledTasks?.().then(result => {
      const task = result?.tasks?.find(task => task.id === TASK_ID)
      if (!alive || !task) return
      const interval = Math.max(MIN_INTERVAL, Math.round(Number(task.interval) || 0))
      setCheckTask({ enabled: task.enabled === true, interval })
      setIntervalDraft(String(interval))
    }).catch(error => toast(t('读取设置失败：{error}', { error: errorMessage(error) }), 'err'))
    return () => { alive = false }
  }, [open])

  async function saveCheck(patch: { enabled?: boolean; interval?: number }): Promise<void> {
    if (checkBusy) return
    setCheckBusy(true)
    try {
      const result = await shared().workbuddyDesktop?.saveScheduledTask?.(TASK_ID, patch)
      if (!result) throw new Error(t('请求失败'))
      const interval = Math.max(MIN_INTERVAL, Math.round(Number(result.interval) || 0))
      setCheckTask({ enabled: result.enabled === true, interval })
      setIntervalDraft(String(interval))
      toast(t('✅ 自动检查更新已保存'))
    } catch (error) {
      toast(t('保存失败：{error}', { error: errorMessage(error) }), 'err')
    } finally { setCheckBusy(false) }
  }

  async function saveInterval(): Promise<void> {
    const interval = Number(intervalDraft)
    if (!Number.isInteger(interval) || interval < MIN_INTERVAL || interval > MAX_INTERVAL) {
      toast(t('检查间隔必须是 {min} ~ {max} 分钟', { min: MIN_INTERVAL, max: MAX_INTERVAL }), 'err')
      return
    }
    if (interval !== checkTask?.interval) await saveCheck({ interval })
  }

  async function saveBeta(enabled: boolean): Promise<void> {
    if (betaBusy) return
    setBetaBusy(true)
    let switched = false
    try {
      const result = await shared().workbuddyDesktop?.setUpdateBeta({ enabled })
      if (!result) throw new Error(t('请求失败'))
      setBeta({ enabled: result.enabled === true })
      switched = true
      if (result.saved === false) toast(t('通道已切换，但写入磁盘失败（重启后会回到原设置）'), 'err')
      else toast(enabled
        ? t('✅ 已加入 beta 版，之后的检查会包含 beta 版')
        : t('✅ 已退出 beta 版，之后的检查只看正式版'))
    } catch (error) {
      toast(t('保存失败：{error}', { error: errorMessage(error) }), 'err')
    } finally { setBetaBusy(false) }
    if (switched) {
      try { await shared().workbuddyDesktop?.checkUpdate(); await forwardUpdateResult() } catch { /* 面板显示检查结果 */ }
    }
  }

  return (
    <Dialog open={open} onOpenChange={next => { if (!next) onClose() }}>
      <DialogContent className='w-[min(460px,calc(100vw-48px))]'>
        <DialogHeader><DialogTitle>{t('更新设置')}</DialogTitle></DialogHeader>
        <DialogBody className='gap-3.5'>
          <div>
            <div className='flex items-center justify-between gap-2'>
              <div className='text-[13px] font-semibold text-foreground'>{t('自动检查更新')}</div>
              <Switch checked={checkTask?.enabled === true} disabled={checkBusy || !checkTask}
                onCheckedChange={next => void saveCheck({ enabled: next === true })}
                aria-label={t('自动检查更新')} />
            </div>
            <div className='mt-1.5 flex items-center gap-2'>
              <Input type='number' min={MIN_INTERVAL} max={MAX_INTERVAL} step={1} className='w-[110px]'
                disabled={checkBusy || !checkTask?.enabled} value={intervalDraft}
                onChange={event => setIntervalDraft(event.currentTarget.value)}
                onKeyDown={event => { if (event.key === 'Enter') { event.preventDefault(); void saveInterval() } }}
                aria-label={t('检查间隔（分钟）')} />
              <span className='text-[12px] text-subtle'>{t('分钟（{min} ~ {max}）', {
                min: MIN_INTERVAL, max: MAX_INTERVAL,
              })}</span>
              <Button variant='outline' size='sm' disabled={checkBusy || !checkTask
                || Number(intervalDraft) === checkTask.interval} onClick={() => void saveInterval()}>{t('保存')}</Button>
            </div>
            <div className='mt-1.5 text-[12px] text-subtle'>
              {t('到点自动向 GitHub 查询新版本，查到就走「检测到更新」弹窗；关闭后「检查更新」按钮仍可手动触发')}
            </div>
          </div>
          <div className='h-px bg-hairline' />
          <div>
            <div className='flex items-center justify-between gap-2'>
              <div className='text-[13px] font-semibold text-foreground'>{t('加入 beta 版')}</div>
              <Switch checked={beta?.enabled === true} disabled={betaBusy || !beta}
                onCheckedChange={next => void saveBeta(next === true)} aria-label={t('加入 beta 版')} />
            </div>
            <div className='mt-1.5 text-[12px] text-subtle'>
              {t('开启后只更新到 beta 版（预发布），取版本最高的一个；beta 版可能不稳定。开启期间不会因为正式版发布而自动切回正式版 —— 想切回请关闭本开关。关闭后只更新到正式版，且不会降级到版本号更低的版本。')}
            </div>
          </div>
          <div className='h-px bg-hairline' />
          {open ? <GithubUpdateSettings /> : null}
        </DialogBody>
      </DialogContent>
    </Dialog>
  )
}
