import { Button, Progress, Switch , Card, Label, Collapsible, CollapsibleTrigger, CollapsibleContent } from '@ui'
import { Badge } from '../../components/ui'
import type { PackSummary, PluginOperation, PluginUpdate, StorePlugin } from '../../api/plugins'
import { t } from '../../i18n'

function PluginProgress({ operation }: { operation?: PluginOperation }) {
  if (!operation) return null
  const stage = operation.stage === 'downloading' ? t('网关正在下载…')
    : operation.stage === 'verifying' ? t('网关正在校验…') : t('网关正在安装…')
  return <>
    {operation.active ? <div className='my-3' aria-live='polite'>
      <p className='m-ck-note'>{stage}{operation.stage === 'downloading' && operation.percent != null ? ` ${Math.round(operation.percent)}%` : ''}</p>
      <Progress value={operation.stage === 'downloading' ? operation.percent : null} aria-label={stage} />
    </div> : null}
    {operation.error ? <p className='m-ck-error'>{operation.error}</p> : null}
    {operation.hostError ? <p className='m-ck-error'>{operation.hostError}</p> : null}
  </>
}

function Head({ name, version, icon }: { name: string; version?: string | null; icon?: string | null }) {
  return <div className='m-ck-toolbar'>
    <span className='m-logo'>{icon ? <img src={icon} alt='' /> : name.slice(0, 1)}</span>
    <div className='m-ck-grow'>
      <div className='m-ck-title'>{t(name)}</div>
      <div className='m-ck-note'>{version ? `v${version}` : t('暂未发布')}</div>
    </div>
  </div>
}

export function StorePluginCard({ item, target, busy, operation, onInstall }: {
  item: StorePlugin
  target: string
  busy: boolean
  operation?: PluginOperation
  onInstall: () => void
}) {
  const asset = item.platforms?.find(platform => platform.target === target)
  return <Card render={<article />} className='m-card m-card-pad'>
    <Head name={item.name || item.id} version={item.version} icon={item.icon} />
    <p className='m-ck-note'>{t(item.description)}</p>
    {item.official ? <Badge tone='brand'>{t('官方')}</Badge> : null}
    {item.reason ? <p className={item.error ? 'm-ck-error' : 'm-ck-note'}>{item.reason}</p> : null}
    <PluginProgress operation={operation} />
    {!operation?.active ? <div className='m-ck-actions'>
      <Button size='sm' disabled={busy || !asset || !item.version || item.error} onClick={onInstall}>{t('在网关下载并安装')}</Button>
    </div> : null}
    {!asset ? <p className='m-ck-note'>{t('当前网关平台暂无插件包')}</p> : null}
  </Card>
}

export function PluginCard({ pack, target, busy, update, operation, onEnabled, onProvider, onUpdate, onRemove }: {
  pack: PackSummary
  target: string
  busy: boolean
  update?: PluginUpdate
  operation?: PluginOperation
  onEnabled: (enabled: boolean) => void
  onProvider: (id: string, enabled: boolean) => void
  onUpdate: () => void
  onRemove: () => void
}) {
  const locked = busy || operation?.active === true
  return <Card render={<article />} className='m-card m-card-pad'>
    <Head name={pack.name || pack.id} version={pack.version} icon={pack.icon} />
    <div className='m-ck-actions'>
      <Badge tone={pack.enabled ? 'ok' : 'mute'}>{pack.enabled ? t('已启用') : t('已停用')}</Badge>
      <Badge tone={pack.signed ? 'ok' : 'warn'}>{pack.signed ? t('已签名') : t('未签名')}</Badge>
      <Badge tone={pack.providers?.some(provider => provider.loaded) ? 'ok' : 'warn'}>{t('已加载 {n} 家', { n: pack.providers?.filter(provider => provider.loaded).length || 0 })}</Badge>
    </div>
    {pack.description ? <p className='m-ck-note'>{t(pack.description)}</p> : null}
    {!pack.entryPresent ? <p className='m-ck-error'>{t('插件文件缺失，请重新安装插件包')}</p> : null}
    {pack.host?.lastError ? <p className='m-ck-error'>{pack.host.lastError}</p> : null}
    {update?.reason ? <p className={update.error ? 'm-ck-error' : 'm-ck-note'}>{update.reason}</p> : null}
    <PluginProgress operation={operation} />
    <div className='m-ck-actions'>
      <Label className='m-ck-toolbar m-ck-note'>
        {t('启用')}<Switch checked={pack.enabled} disabled={locked} aria-label={t('启用插件 {name}', { name: pack.name })}
          onCheckedChange={onEnabled} />
      </Label>
      <Button size='sm' variant='outline' disabled={locked} onClick={onRemove}>{t('卸载')}</Button>
      {update?.available ? <Button size='sm' disabled={locked || update.error || !update.asset || update.asset.target !== target} onClick={onUpdate}>
        {t('更新到 v{version}', { version: update.latest || '' })}
      </Button> : null}
    </div>
    <Collapsible className='mt-3 border-t border-hairline pt-3'>
      <CollapsibleTrigger className='m-ck-title'>{t('提供商启停')}</CollapsibleTrigger><CollapsibleContent keepMounted>
      {pack.providers?.map(provider => <Label className='m-ck-toolbar py-2' key={provider.id}>
        <Switch checked={provider.enabled} disabled={locked || !pack.enabled} aria-label={t(provider.label)}
          onCheckedChange={enabled => onProvider(provider.id, enabled)} />
        <span className='m-ck-note m-ck-grow'>{t(provider.label)}</span>
        {provider.enabled && !provider.loaded ? <Badge tone='warn'>{t('未加载')}</Badge> : null}
      </Label>)}
    </CollapsibleContent></Collapsible>
  </Card>
}
