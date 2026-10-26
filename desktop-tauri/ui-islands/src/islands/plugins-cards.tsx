import * as React from 'react'
import { Badge, Button, Progress, Switch } from '@ui'
import { formatSize, formatTime, platformLabel, pluginDescription, pluginName } from './plugins-shared'
import type { HostState, PackProvider, PackSummary, PluginOperation, PluginUpdate, StorePlugin } from './plugins-types'
import { t } from '../i18n'

function Card({ children, installed }: { children: React.ReactNode; installed?: boolean }) {
  return <article className={'flex flex-col overflow-hidden rounded-lg border bg-surface shadow-1 '
    + (installed ? 'border-success/40' : 'border-border')}>{children}</article>
}

function CardHead({ id, name, version, icon, children }: {
  id: string; name: string; version?: string | null; icon?: string | null; children?: React.ReactNode
}) {
  const title = pluginName(id, name)
  return <div className='flex gap-3 px-4 pt-4'>
    <div className='grid size-11 shrink-0 place-items-center overflow-hidden rounded-xl bg-primary-soft text-[19px] font-bold text-primary-fg'>
      {icon ? <img src={icon} alt='' className='size-full object-cover' /> : title.slice(0, 1)}
    </div>
    <div className='min-w-0 flex-1'>
      <h3 className='mb-1 flex flex-wrap items-center gap-2 text-[14px] font-semibold leading-snug'>
        {title}{version ? <span className='text-[11.5px] font-normal tabular-nums text-muted-foreground'>v{version}</span> : null}
      </h3>
      <div className='flex flex-wrap gap-1.5'>{children}</div>
    </div>
  </div>
}

export function OperationProgress({ operation }: { operation: PluginOperation }) {
  const stage = operation.stage === 'downloading' ? t('正在下载…')
    : operation.stage === 'verifying' ? t('正在校验包内容…') : t('正在安装…')
  const download = operation.stage === 'downloading'
  return <div className='px-4 pb-4 pt-3' aria-live='polite'>
    <div className='mb-1.5 flex items-center justify-between gap-2 text-[11.5px]'>
      <span className='text-subtle'>{stage}</span>
      <span className='tabular-nums text-muted-foreground'>
        {download ? (operation.total > 0
          ? `${formatSize(operation.received) || '0 B'} / ${formatSize(operation.total)} · ${Math.round(operation.percent || 0)}%`
          : formatSize(operation.received) || '0 B') : t('请稍候')}
      </span>
    </div>
    <Progress value={download ? operation.percent : null} aria-label={stage} />
  </div>
}

export function StorePluginCard({ item, target, operation, disabled, onInstall, onNotes }: {
  item: StorePlugin
  target: string
  operation?: PluginOperation
  disabled: boolean
  onInstall: () => void
  onNotes: () => void
}) {
  const platform = item.platforms.find(platform => platform.target === target)
  const installable = Boolean(item.version && platform && !item.error)
  return <Card>
    <CardHead id={item.id} name={item.name} version={item.version} icon={item.icon}>
      {item.official ? <Badge variant='brand'>{t('官方')}</Badge> : null}
      {item.prerelease ? <Badge variant='warning'>{t('beta 版')}</Badge> : null}
      {!item.version ? <Badge variant='outline'>{t('暂未发布')}</Badge> : null}
    </CardHead>
    <p className='px-4 pt-2.5 text-[12px] leading-relaxed text-subtle'>
      {pluginDescription(item.id, item.description)}
    </p>
    <div className='flex flex-wrap gap-3 px-4 pt-3 text-[11px] text-muted-foreground'>
      <span>{platformLabel(target)}</span><span>{formatSize(platform?.size || 0) || t('当前平台暂无包')}</span>
    </div>
    {item.error ? <p className='px-4 pt-2 text-[12px] text-destructive'>{item.reason}</p> : null}
    {operation?.error ? <p className='px-4 pt-2 text-[12px] text-destructive'>{operation.error}</p> : null}
    {operation?.active ? <OperationProgress operation={operation} /> : <div className='mt-auto flex gap-2 px-4 pb-4 pt-3.5'>
      <Button size='sm' disabled={disabled || !installable} onClick={onInstall}>{t('下载并安装')}</Button>
      {item.version ? <Button variant='outline' size='sm' onClick={onNotes}>{t('查看更新日志')}</Button> : null}
    </div>}
  </Card>
}

export function InstalledPluginCard({ pack, store, host, update, operation, disabled, onEnabled, onProvider, onRemove, onUpdate, onNotes }: {
  pack: PackSummary
  store?: StorePlugin
  host: HostState
  update?: PluginUpdate
  operation?: PluginOperation
  disabled: boolean
  onEnabled: (enabled: boolean) => void
  onProvider: (provider: PackProvider, enabled: boolean) => void
  onRemove: () => void
  onUpdate: () => void
  onNotes: () => void
}) {
  const blocked = update?.error === true
  return <Card installed>
    <CardHead id={pack.id} name={store?.name || pack.name} version={pack.version} icon={pack.icon || store?.icon}>
      <Badge variant={pack.enabled ? 'default' : 'secondary'}>{pack.enabled ? t('已启用') : t('已停用')}</Badge>
      <Badge variant={pack.signed ? 'success' : 'warning'}>{pack.signed ? t('已签名') : t('未签名')}</Badge>
      {pack.version.includes('-') ? <Badge variant='warning'>{t('beta 版')}</Badge> : null}
      {update?.available ? <Badge variant='brand'>{t('有更新')}</Badge> : null}
      {!pack.entryPresent ? <Badge variant='destructive'>{t('文件缺失')}</Badge> : null}
    </CardHead>
    <p className='px-4 pt-2.5 text-[12px] leading-relaxed text-subtle'>
      {pluginDescription(pack.id, store?.description || pack.description)}
    </p>
    <p className='px-4 pt-2 text-[11px] text-muted-foreground'>
      {t('{count} 家提供商 · 安装于 {time}', { count: String(pack.providers.length), time: formatTime(pack.installedAt) })}
    </p>
    <div className='flex flex-wrap gap-3 px-4 pt-3 text-[11px] text-muted-foreground'>
      <span>{host?.running ? t('运行中') : t('未运行')}</span>
      <span>{t('已加载')} {t('{count} 家', { count: String(pack.providers.filter(provider => provider.loaded).length) })}</span>
    </div>
    {!pack.entryPresent ? <p className='px-4 pt-2 text-[12px] text-destructive'>
      {t('插件包的可执行文件不在磁盘上（可能被手工删除或移动），请重新安装。')}
    </p> : null}
    {update?.available ? <div className='mx-4 mt-3 flex flex-wrap items-center justify-between gap-2 border-t border-hairline pt-3'>
      <span className='text-[12px] font-medium text-primary-fg'>
        {t('可更新到 v{version}', { version: update.latest || '' })}
      </span>
      <div className='flex items-center gap-2 text-[12px] text-subtle'>
        {t('更新日志')}<Button variant='outline' size='xs' onClick={onNotes}>{t('查看')}</Button>
      </div>
    </div> : null}
    {update?.error ? <p className='px-4 pt-2 text-[12px] text-destructive'>{update.reason}</p> : null}
    {operation?.error ? <p className='px-4 pt-2 text-[12px] text-destructive'>{operation.error}</p> : null}
    {operation?.hostError ? <p className='px-4 pt-2 text-[12px] text-warning'>{operation.hostError}</p> : null}
    {operation?.active ? <OperationProgress operation={operation} /> : <div className='mt-auto flex flex-wrap items-center gap-2 px-4 pb-4 pt-3.5'>
      <label className='flex items-center gap-1.5 text-[12px] text-subtle'>
        {t('启用')}<Switch checked={pack.enabled} disabled={disabled} onCheckedChange={value => onEnabled(value === true)} />
      </label>
      <Button variant='outline' size='sm' disabled={disabled} onClick={onRemove}>{t('卸载')}</Button>
      <span className='flex-1' />
      {update?.available ? <Button size='sm' disabled={disabled || blocked} onClick={onUpdate}>
        {t('更新到 v{version}', { version: update.latest || '' })}
      </Button> : <span className={'text-[11px] ' + (update?.error ? 'text-destructive' : 'text-muted-foreground')}>
        {update?.checked ? update.reason || t('已是最新') : t('尚未检查更新')}
      </span>}
    </div>}
    <details className='border-t border-hairline'>
      <summary className='cursor-pointer select-none px-4 py-2 text-[12px] text-subtle'>
        {t('这个包提供的提供商（可单独停用某一家）')}
      </summary>
      <div className='grid gap-1 px-3 pb-3 sm:grid-cols-2'>
        {pack.providers.map(provider => <label key={provider.id} className='flex items-center gap-2 rounded px-1.5 py-1 text-[12.5px]'>
          <Switch checked={provider.enabled} disabled={disabled || !pack.enabled}
            onCheckedChange={value => onProvider(provider, value === true)} />
          <span className='min-w-0 truncate'>{provider.label}</span>
          <code className='text-[11px] text-muted-foreground'>{provider.id}</code>
          {provider.enabled && !provider.loaded ? <Badge variant='secondary'>{t('未加载')}</Badge> : null}
        </label>)}
      </div>
    </details>
  </Card>
}
