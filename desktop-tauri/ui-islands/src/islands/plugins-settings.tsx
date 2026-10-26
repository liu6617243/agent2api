import * as React from 'react'
import { Button, Dialog, DialogBody, DialogContent, DialogHeader, DialogTitle, Input, Switch } from '@ui'
import { GithubUpdateSettings } from './update-github-settings'
import { api, toast } from './plugins-shared'
import type { PluginSettings } from './plugins-types'
import { t } from '../i18n'

export function PluginSettingsDialog({ open, onClose, onChanged }: {
  open: boolean
  onClose: () => void
  onChanged: () => Promise<void>
}) {
  const [settings, setSettings] = React.useState<PluginSettings | null>(null)
  const [draft, setDraft] = React.useState('')
  const [busy, setBusy] = React.useState(false)

  React.useEffect(() => {
    if (!open) return
    let alive = true
    setSettings(null)
    void api<PluginSettings>('GET', '/api/plugins/settings').then(value => {
      if (alive) { setSettings(value); setDraft(String(value.interval)) }
    }).catch(error => toast(t('读取设置失败：{error}', { error: String(error) }), 'err'))
    return () => { alive = false }
  }, [open])

  async function save(patch: Partial<PluginSettings>): Promise<void> {
    if (busy) return
    setBusy(true)
    try {
      const result = await api<PluginSettings>('PATCH', '/api/plugins/settings', patch)
      setSettings(result)
      setDraft(String(result.interval))
      toast(result.saved === false
        ? t('设置已生效，但写入磁盘失败（重启后会恢复原设置）') : t('插件设置已保存'),
      result.saved === false ? 'err' : 'ok')
      if ('beta' in patch) await onChanged()
    } catch (error) {
      toast(t('保存失败：{error}', { error: error instanceof Error ? error.message : String(error) }), 'err')
    } finally { setBusy(false) }
  }

  async function saveInterval(): Promise<void> {
    const interval = Number(draft)
    if (!Number.isInteger(interval) || interval < 1 || interval > 1440) {
      toast(t('检查间隔必须是 {min} ~ {max} 分钟', { min: 1, max: 1440 }), 'err')
      return
    }
    await save({ interval })
  }

  const automatic = settings?.autoCheck || settings?.autoUpdate || settings?.autoInstallOfficial
  return (
    <Dialog open={open} onOpenChange={next => { if (!next) onClose() }}>
      <DialogContent className='w-[min(460px,calc(100vw-48px))]'>
        <DialogHeader><DialogTitle>{t('插件设置')}</DialogTitle></DialogHeader>
        <DialogBody className='gap-3.5'>
          <div>
            <div className='flex items-center justify-between gap-2'>
              <div className='text-[13px] font-semibold text-foreground'>{t('自动检查更新')}</div>
              <Switch checked={settings?.autoCheck === true} disabled={busy || !settings}
                onCheckedChange={value => void save({ autoCheck: value === true })}
                aria-label={t('自动检查插件更新')} />
            </div>
            <div className='mt-1.5 flex items-center gap-2'>
              <Input type='number' min={1} max={1440} step={1} className='w-[110px]'
                disabled={busy || !automatic} value={draft}
                onChange={event => setDraft(event.currentTarget.value)}
                onKeyDown={event => { if (event.key === 'Enter') { event.preventDefault(); void saveInterval() } }}
                aria-label={t('检查间隔（分钟）')} />
              <span className='text-[12px] text-subtle'>{t('分钟（{min} ~ {max}）', { min: 1, max: 1440 })}</span>
              <Button variant='outline' size='sm' disabled={busy || !settings || Number(draft) === settings.interval}
                onClick={() => void saveInterval()}>{t('保存')}</Button>
            </div>
            <p className='mt-1.5 text-[12px] text-subtle'>
              {t('按间隔检查插件版本，在对应卡片显示更新；关闭后仍可手动检查。自动更新或自动下载开启时也使用此间隔。')}
            </p>
          </div>
          <div className='h-px bg-hairline' />
          <div>
            <div className='flex items-center justify-between gap-2'>
              <div className='text-[13px] font-semibold text-foreground'>{t('加入 beta 版')}</div>
              <Switch checked={settings?.beta === true} disabled={busy || !settings}
                onCheckedChange={value => void save({ beta: value === true })} aria-label={t('加入插件 beta 版')} />
            </div>
            <p className='mt-1.5 text-[12px] text-subtle'>
              {t('仅影响插件更新通道，与主程序独立。开启后可安装 beta 版；已使用 beta 时保持该通道，关闭后只看正式版且不会降级。')}
            </p>
          </div>
          <div className='h-px bg-hairline' />
          <div>
            <div className='flex items-center justify-between gap-2'>
              <div className='text-[13px] font-semibold text-foreground'>{t('自动更新插件')}</div>
              <Switch checked={settings?.autoUpdate === true} disabled={busy || !settings}
                onCheckedChange={value => void save({ autoUpdate: value === true })} aria-label={t('自动更新插件')} />
            </div>
            <p className='mt-1.5 text-[12px] text-subtle'>
              {t('检测到更新后自动下载、校验并安装已启用的官方签名插件。未签名或第三方插件仍需手动更新。')}
            </p>
          </div>
          <div className='h-px bg-hairline' />
          <div>
            <div className='flex items-center justify-between gap-2'>
              <div className='text-[13px] font-semibold text-foreground'>{t('自动下载官方插件')}</div>
              <Switch checked={settings?.autoInstallOfficial === true} disabled={busy || !settings}
                onCheckedChange={value => void save({ autoInstallOfficial: value === true })}
                aria-label={t('自动下载官方插件')} />
            </div>
            <p className='mt-1.5 text-[12px] text-subtle'>
              {t('发现未安装的官方插件时自动下载并安装，仅接受当前平台且通过官方签名校验的包；开启期间卸载的官方插件也会重新安装。')}
            </p>
          </div>
          <div className='h-px bg-hairline' />
          {open ? <GithubUpdateSettings plugin /> : null}
        </DialogBody>
      </DialogContent>
    </Dialog>
  )
}
