import { apiCall } from './client'
import { serverIdentity } from '../lib/servers'
import { t } from '../i18n'

export type Entitlement = {
  showName?: string
  unitType?: string
  grantUnits?: number
  period?: string
  effectiveAt?: number | null
}

export type PreviewPlan = {
  planId?: string
  name?: string
  description?: string
  priority?: number
  startsAt?: number | null
  endsAt?: number | null
  entitlements?: Entitlement[]
}

export type OwnedPlan = PreviewPlan & {
  state?: string
  pendingUntil?: number | null
}

export type PreviewResult = {
  deployed?: boolean
  plans?: PreviewPlan[]
  activationError?: string | null
}

export type ClaimResult = {
  ok?: boolean
  failure?: string
  failureLabel?: string
  message?: string
  planId?: string
  claimedAt?: number
  startsAt?: number
  endsAt?: number
}

type CaptchaConfig = { enabled?: boolean; region?: string; prefix?: string; sceneId?: string }
type CaptchaApi = {
  solve<T>(config: CaptchaConfig, request: (param: string) => Promise<{ captchaResult: boolean; bizResult: boolean; result: T }>, signal?: AbortSignal): Promise<{ result: T }>
  cancel(): boolean
  isBusy(): boolean
}

const captchaApi = () => (window as Window & { wbAliyunCaptcha?: CaptchaApi }).wbAliyunCaptcha
let captchaLoading: Promise<CaptchaApi> | null = null

/** 只复用主程序的验证码实现，原生包也携带这份脚本，不从插件加载页面。 */
function loadCaptcha(): Promise<CaptchaApi> {
  const api = captchaApi()
  if (api) return Promise.resolve(api)
  if (captchaLoading) return captchaLoading
  captchaLoading = new Promise<CaptchaApi>((resolve, reject) => {
    const script = document.createElement('script')
    const timer = window.setTimeout(() => finish(new Error(t('验证码组件加载超时，请重试'))), 15_000)
    const finish = (error?: Error) => {
      window.clearTimeout(timer)
      script.onload = null
      script.onerror = null
      const loaded = captchaApi()
      if (error || !loaded) { script.remove(); reject(error || new Error(t('验证码组件未加载，请重试'))) }
      else resolve(loaded)
    }
    script.src = new URL('aliyun-captcha.js', document.baseURI).href
    script.onload = () => finish()
    script.onerror = () => finish(new Error(t('验证码组件加载失败，请重试')))
    document.head.appendChild(script)
  }).catch(error => { captchaLoading = null; throw error })
  return captchaLoading
}

const accountPath = (id: string) => `/api/accounts/${encodeURIComponent(id)}`

export function previewPlans(id: string): Promise<PreviewResult> {
  return apiCall(`${accountPath(id)}/zcode-claim/preview`, { method: 'POST' })
}

export async function claimPlan(id: string, planId: string, signal?: AbortSignal): Promise<ClaimResult> {
  const identity = serverIdentity()
  const requireCurrent = () => {
    if (signal?.aborted || serverIdentity() !== identity) throw new DOMException('请求已取消', 'AbortError')
  }
  requireCurrent()
  const config = await apiCall<CaptchaConfig>(`${accountPath(id)}/zcode-claim/captcha-config`, { method: 'POST', signal })
  const claim = (captchaVerifyParam: string) => {
    requireCurrent()
    return apiCall<ClaimResult>(`${accountPath(id)}/zcode-claim`, {
      method: 'POST', body: { planId, captchaVerifyParam, captchaRegion: config.region || '' }, signal,
    })
  }
  if (!config.enabled) return claim('')
  const captcha = await loadCaptcha()
  requireCurrent()
  const response = await captcha.solve({ ...config, region: config.region || 'ga' }, async param => {
    const result = await claim(param)
    return { captchaResult: result.failure !== 'captcha', bizResult: true, result }
  }, signal)
  requireCurrent()
  return response.result
}

export function cancelPlanCaptcha(): void {
  captchaApi()?.cancel()
}
