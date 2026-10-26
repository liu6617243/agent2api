import React from 'react'
import { t } from '../i18n'
import type { PluginPage } from '../lib/pluginPages'

/**
 * 插件页面的宿主。
 *
 * ── 这是什么 ────────────────────────────────────────────────
 * 插件包可以在 `pack.json` 的 `ui` 里申报页面（签到中心这类「只有装了插件才
 * 有意义」的功能）。页面是包里的静态资源，由网关在 `/plugins/<pack>/ui/` 下
 * 托管；手机端只负责把它挂到「更多」的宫格里，点进去用 iframe 嵌。
 *
 * ── 为什么手机端也要有 ──────────────────────────────────────
 * 原先手机端的签到中心是**手写的第二份实现**，而它对应的是插件的能力 ——
 * 插件升级、新插件接入，手机端都得跟着改一次。改成 iframe 之后，手机端只认
 * 「插件申报了哪些页面」，页面内容由插件自己维护，两端天然一致。
 *
 * ── 为什么不复用桌面端的 iframe 宿主 ─────────────────────────
 * 两端是独立产物（桌面 `ui/islands`、手机 `mobile`），没有共享运行时。
 * 但两边的行为约定一致：`postMessage` 的 `agent2api:panel-credentials` 这条
 * 消息、`no-store` 缓存、切页即销毁 —— 改动时两边一起改。
 *
 * 清单解析在 `lib/pluginPages.ts`（路由层也要用它，不能反向依赖页面层）。
 */

/**
 * 插件页面本体：一个占满剩余空间的 iframe。
 *
 * `url` 是网关托管的相对路径（`/plugins/.../ui/...`）。PWA 形态下与页面同源，
 * 直接用；APK 形态下页面 origin 是 `https://localhost`，而网关是另一台机器 ——
 * 由 `api/client.ts` 的 `resolveUrl` 负责拼基址。
 */
export function PluginPageView({ page, resolveUrl }: {
  page: PluginPage
  /** 把相对路径拼成完整地址（PWA 原样返回；APK 补网关基址） */
  resolveUrl: (path: string) => string
}): React.ReactElement {
  const frameRef = React.useRef<HTMLIFrameElement | null>(null)
  const src = React.useMemo(() => {
    try {
      return resolveUrl(page.url)
    } catch {
      // 没配服务器地址时 `resolveUrl()` 会抛（APK 首启）—— 渲染一个空 iframe，
      // 外层已经有「先配服务器」的整屏拦截，走不到这里
      return ''
    }
  }, [page.url, resolveUrl])

  React.useEffect(() => {
    const frame = frameRef.current
    if (!frame) return
    // 每次挂载 / 页面重新加载后递一次凭证：iframe 是跨源文档，带不上 cookie
    const push = async (): Promise<void> => {
      try {
        const credentials = await panelCredentials()
        frame.contentWindow?.postMessage(
          { type: 'agent2api:panel-credentials', headers: credentials },
          '*',
        )
      } catch {
        /* 没有凭证就不传：插件页会退化成只用同源 cookie */
      }
    }
    const onLoad = (): void => {
      void push()
      // 主题与凭证同一时机递一次：iframe 是独立文档，读不到本页的
      // `workbuddy-desktop-theme`，也读不到 `<html data-theme>`。
      // 不递的话插件页在「用户显式选了深色、系统却是浅色」时会一直偏亮。
      pushTheme(frame)
    }
    frame.addEventListener('load', onLoad)
    return () => frame.removeEventListener('load', onLoad)
  }, [src])

  /**
   * 插件页会主动索取凭证 / 主题（`agent2api:request-*`）。
   *
   * 那条路比 `load` 钩子可靠：页面在脚本里就发，而 `load` 要等全部资源就绪
   * —— 页面首轮请求往往落在两者之间。桌面端 `ui/app.js` 有同一份监听。
   */
  React.useEffect(() => {
    const onMessage = (event: MessageEvent): void => {
      const data = event.data
      if (!data || typeof data !== 'object') return
      const type = (data as { type?: string }).type
      if (type === 'agent2api:request-theme') {
        const mode = readThemeMode()
        event.source?.postMessage(
          { type: 'agent2api:panel-theme', theme: mode },
          { targetOrigin: '*' } as WindowPostMessageOptions,
        )
        return
      }
      if (type !== 'agent2api:request-credentials') return
      void panelCredentials().then(headers => {
        event.source?.postMessage(
          { type: 'agent2api:panel-credentials', headers },
          { targetOrigin: '*' } as WindowPostMessageOptions,
        )
      })
    }
    window.addEventListener('message', onMessage)
    return () => window.removeEventListener('message', onMessage)
  }, [])

  return (
    <iframe
      ref={frameRef}
      className='m-plugin-frame'
      title={page.title || t('插件页面')}
      src={src}
      style={{
        display: 'block',
        width: '100%',
        height: '100%',
        border: 0,
        background: 'transparent',
      }}
    />
  )
}

/**
 * 读当前主题模式（`light` / `dark` / `system`）。
 *
 * 与 `state/store.ts` 的 `readTheme()` 同一个存储键 —— 这里直接读 localStorage
 * 而不是 import store，是因为本文件在路由层之上、不该反向依赖状态层；
 * 而且主题只是一个字符串，不值得为它建一条订阅链。
 */
function readThemeMode(): string {
  try {
    const mode = localStorage.getItem('workbuddy-desktop-theme')
    return mode === 'light' || mode === 'dark' ? mode : 'system'
  } catch {
    return 'system'
  }
}

/** 把主题模式推给插件页（iframe 读不到宿主的 localStorage / `<html>`） */
function pushTheme(frame: HTMLIFrameElement): void {
  try {
    frame.contentWindow?.postMessage(
      { type: 'agent2api:panel-theme', theme: readThemeMode() },
      '*',
    )
  } catch {
    /* iframe 已销毁：下次打开会再推一次 */
  }
}

/**
 * 取管理 API 的凭证头（`{ 'x-panel-token': …, Authorization: … }`）。
 *
 * 与桌面壳的 `directCredentials` 同形：PWA 形态下网关自己的 cookie 就够，
 * 这里返回空对象；APK 形态下要把面板令牌显式放进头里。两端的判定在
 * `lib/servers.ts` 的 `readServerAuth` / `readServerKey` 里，这里只负责拼。
 */
async function panelCredentials(): Promise<Record<string, string>> {
  const headers: Record<string, string> = {}
  try {
    const mod = await import('../lib/servers')
    const serverId = mod.activeServerId()
    if (!serverId) return headers
    const auth = mod.readServerAuth(serverId)
    if (auth?.access) {
      headers['x-panel-token'] = auth.access
      headers.Authorization = `Bearer ${auth.access}`
    }
    const key = mod.readServerKey(serverId)
    if (key) headers['x-api-key'] = key
  } catch {
    /* 读不到就不带头 */
  }
  return headers
}
