export type ProxyService = 'update' | 'kernelDownload'

export type ProxyUsage = {
  usedBy?: Array<{ id?: string; name?: string; enabled?: boolean }>
  usedByServices?: ProxyService[]
}

export function hasProxyUsage(item: ProxyUsage): boolean {
  return Boolean(item.usedBy?.length || item.usedByServices?.length)
}

export function proxyServiceLabels(item: ProxyUsage, t: (key: string) => string): string[] {
  return [...new Set(item.usedByServices || [])].map(service => {
    switch (service) {
      case 'update': return t('软件 / 插件更新')
      case 'kernelDownload': return t('内核下载')
    }
  })
}
