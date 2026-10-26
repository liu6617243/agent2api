/**
 * 插件申报的页面清单（`GET /api/plugins` 的 `packs[].ui[]`）。
 *
 * ── 为什么单独一个模块 ──────────────────────────────────────
 * 读它的是**路由层**（store 的菜单键、更多页的宫格、App 的页面分派），
 * 渲染它的是 `pages/PluginPage.tsx`。把解析放进页面文件会让 store 反向依赖
 * 页面层 —— store 的注释里已经写明了不这么做的理由。
 *
 * ── 键的拼法 ────────────────────────────────────────────────
 * `plugin:<pack>:<page>`，与桌面端 `ui/app.js` 的 `PLUGIN_PAGE_PREFIX`
 * 完全一致。两端读的是同一份后端字段，拼法不一样只会让排查时对不上号。
 */

/** 一个插件页面的描述 */
export type PluginPage = {
  /** 菜单键：`plugin:<pack>:<page>` */
  key: string
  title: string
  /** 网关托管的相对路径（`/plugins/<pack>/ui/<entry>`） */
  url: string
  icon?: string
}

/** 把 `/api/plugins` 的响应展开成插件页面清单（停用的包跳过）。 */
export function pluginPagesOf(payload: unknown): PluginPage[] {
  const packs = (payload as { packs?: unknown })?.packs
  if (!Array.isArray(packs)) return []
  const pages: PluginPage[] = []
  for (const pack of packs) {
    if (!pack || typeof pack !== 'object') continue
    const record = pack as { id?: unknown; enabled?: unknown; ui?: unknown }
    // 停用的包：页面也不该出现（与「插件不在，这家就不存在」同一原则）
    if (record.enabled === false) continue
    const id = typeof record.id === 'string' ? record.id : ''
    if (!id || !Array.isArray(record.ui)) continue
    for (const page of record.ui) {
      if (!page || typeof page !== 'object') continue
      const item = page as { id?: unknown; title?: unknown; icon?: unknown; url?: unknown }
      const pageId = typeof item.id === 'string' ? item.id : ''
      const url = typeof item.url === 'string' ? item.url : ''
      if (!pageId || !url) continue
      pages.push({
        key: `plugin:${id}:${pageId}`,
        title: typeof item.title === 'string' && item.title ? item.title : pageId,
        url,
        icon: typeof item.icon === 'string' ? item.icon : undefined,
      })
    }
  }
  return pages
}

/** 是不是插件页面的菜单键（store 的 hash 白名单按这个前缀放行） */
export function isPluginKey(key: string): boolean {
  return key.startsWith('plugin:')
}
