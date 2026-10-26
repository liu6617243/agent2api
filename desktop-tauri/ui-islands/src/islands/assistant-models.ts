import { t } from '../i18n'

export type ModelOption = {
  name: string
  provider: string
  supportsImages: boolean
  viaMapping: boolean
}

function row(value: unknown): Record<string, unknown> {
  return value && typeof value === 'object' && !Array.isArray(value)
    ? value as Record<string, unknown> : {}
}

function text(value: unknown): string {
  return typeof value === 'string' ? value.trim() : ''
}

function key(provider: string, name: string): string {
  return `${provider.toLowerCase()}\u0001${name.toLowerCase()}`
}

/** 可请求的名字以 /api/session 的 models 为准；管理视图只补充映射标识。 */
export function modelOptionsFrom(
  publicModels: unknown[],
  managedModels: unknown[],
  mappings: unknown[],
): ModelOption[] {
  const native = new Set(managedModels.map(value => {
    const item = row(value)
    return key(text(item.provider), text(item.id))
  }))
  const aliases = new Set(mappings.filter(value => {
    const item = row(value)
    return item.enabled === true && item.isDefault !== true
  }).map(value => {
    const item = row(value)
    return key(text(item.provider), text(item.alias))
  }))
  const seen = new Set<string>()
  const options: ModelOption[] = []
  for (const value of publicModels) {
    const item = row(value)
    const name = text(item.id)
    if (!name || seen.has(name.toLowerCase())) continue
    const provider = text(item.provider) || text(item.owned_by)
    seen.add(name.toLowerCase())
    options.push({
      name,
      provider: text(item.providerLabel) || provider,
      supportsImages: item.supports_images === true,
      viaMapping: !native.has(key(provider, name)) && aliases.has(key(provider, name)),
    })
  }
  return options
}

export function selectedModel(options: ModelOption[], name: string | null | undefined): ModelOption | undefined {
  const requested = name?.trim().toLowerCase()
  return requested ? options.find(option => option.name.toLowerCase() === requested) : undefined
}

export function unavailableModelMessage(name: string): string {
  return t('模型「{model}」当前不在可用列表中，请重新选择；也可到「模型管理」或「插件」页检查是否已启用。', { model: name })
}
