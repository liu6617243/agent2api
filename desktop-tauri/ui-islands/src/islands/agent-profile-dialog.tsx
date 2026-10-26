import * as React from 'react'
import { Alert, Button, Dialog, DialogBody, DialogContent, DialogFooter, DialogHeader, DialogTitle, Field, FieldLabel, Switch } from '@ui'
import { t } from '../i18n'
import { confirm, errorMessage, requestPreview, saveProfiles, toast } from './agent-state'
import { cloneProfile, defaultOperation, profileSignature, validateDraft } from './agent-model'
import { ConnectionFields, SingleModelFields, TextField } from './agent-fields'
import { ModelsFields } from './agent-model-fields'
import { loadCatalog, type CatalogModel } from './agent-catalog'
import { WritePreview } from './agent-preview'
import type { AgentClient, AgentPreview, AgentProfile } from './agent-types'

export function ProfileDialog({ client, profile, creating, onClose }: { client: AgentClient; profile: AgentProfile; creating: boolean; onClose: () => void }) {
  const [draft, setDraft] = React.useState(() => cloneProfile(profile))
  const [error, setError] = React.useState('')
  const [saving, setSaving] = React.useState(false)
  const [preview, setPreview] = React.useState<AgentPreview | null>(null)
  const [previewing, setPreviewing] = React.useState(false)
  const [catalog, setCatalog] = React.useState<CatalogModel[]>([])
  const [catalogError, setCatalogError] = React.useState('')
  const initial = React.useRef(profileSignature(profile))
  const previewGeneration = React.useRef(0)
  const dirty = profileSignature(draft) !== initial.current
  React.useEffect(() => {
    let alive = true
    void loadCatalog().then(models => { if (alive) setCatalog(models) }).catch(error => { if (alive) setCatalogError(errorMessage(error)) })
    return () => { alive = false; previewGeneration.current += 1 }
  }, [])
  const update = (values: Partial<AgentProfile>) => {
    previewGeneration.current += 1
    setDraft(current => ({ ...current, ...values })); setPreview(null); setPreviewing(false); setError('')
  }
  const close = async () => {
    if (saving) return
    if (dirty && !await confirm(t('放弃未保存的修改？'), t('关闭后丢弃本次草稿，不会保存空方案，也不会修改客户端。'), t('放弃修改'))) return
    onClose()
  }
  const save = async () => {
    if (saving) return
    const message = validateDraft(client,draft)
    if (message) { setError(message); return }
    setSaving(true); setError('')
    try {
      const profiles = creating ? [...client.profiles,draft] : client.profiles.map(item => item.id === draft.id ? draft : item)
      await saveProfiles(client,profiles)
      onClose(); toast(t('方案已保存，尚未应用到客户端'))
    } catch (error) { setError(errorMessage(error)); setSaving(false) }
  }
  const showPreview = async () => {
    const message = validateDraft(client,draft)
    if (message) { setError(message); return }
    const generation = ++previewGeneration.current
    setPreviewing(true); setError('')
    try { const result = await requestPreview(client,draft,defaultOperation(client)); if (generation === previewGeneration.current) setPreview(result) }
    catch (error) { if (generation === previewGeneration.current) setError(errorMessage(error)) }
    finally { if (generation === previewGeneration.current) setPreviewing(false) }
  }
  return <Dialog open onOpenChange={(open,details) => { if (!open) { details.cancel(); void close() } }}>
    <DialogContent className='agent-config-dialog'>
      <DialogHeader><DialogTitle>{creating ? t('新建配置方案') : t('编辑配置方案')} · {client.name}</DialogTitle></DialogHeader>
      <DialogBody>
        <Alert>{t('保存只记录方案，不修改客户端。应用时会另外展示文件差异并请求确认。')}</Alert>
        <TextField label={t('方案名称')} fieldKey='' value={draft.name} onChange={name => update({name})} disabled={saving} />
        <ConnectionFields client={client} draft={draft} update={update} disabled={saving} />
        {catalogError && draft.kind === 'gateway' && <Alert variant='warning'>{t('模型目录读取失败，可手动填写真实模型 ID。')} {catalogError}</Alert>}
        {client.supportsModels ? <ModelsFields client={client} draft={draft} update={update} catalog={catalog} disabled={saving} />
          : <SingleModelFields client={client} draft={draft} update={update} catalog={catalog} disabled={saving} />}
        {client.supportsToggle && <Field><FieldLabel>{t('客户端条目启用状态')}</FieldLabel><Switch checked={draft.enabled} disabled={saving} onCheckedChange={enabled => update({enabled})} aria-label={t('客户端条目启用状态')} />
          <p className='text-xs text-subtle'>{t('这是待应用状态；保存不会立即改变客户端开关。')}</p></Field>}
        <TextField label={t('备注')} fieldKey='' value={draft.note} onChange={note => update({note})} disabled={saving} />
        {preview && <WritePreview preview={preview} />}
        {error && <Alert variant='destructive'>{error}</Alert>}
      </DialogBody>
      <DialogFooter>
        <Button variant='ghost' disabled={saving || previewing} onClick={() => void showPreview()}>{previewing ? t('生成中…') : t('预览草稿改动')}</Button><span className='mr-auto' />
        <Button variant='outline' disabled={saving} onClick={() => void close()}>{t('取消')}</Button>
        <Button disabled={saving} onClick={() => void save()}>{saving ? t('保存中…') : t('保存方案')}</Button>
      </DialogFooter>
    </DialogContent>
  </Dialog>
}
