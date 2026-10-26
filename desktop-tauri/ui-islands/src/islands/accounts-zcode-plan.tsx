/** ZCode 套餐通道选择与验证码令牌池状态。 */

import * as React from 'react'
import { Select, SelectContent, SelectItem, SelectTrigger, SelectValue } from '@ui'
import { t } from '../i18n'
import { errorMessage, shared, type AccountsBridge } from './accounts-shared'
import { zcodePlanLabel, ZCODE_PLAN_CODING, ZCODE_PLAN_START } from './accounts-domain'

type PoolStats = NonNullable<Awaited<ReturnType<NonNullable<AccountsBridge['zcodeCaptchaStats']>>>>
type MintStatus = { state: string; error: string }

function poolStatus(pool: PoolStats | null, mint: MintStatus | null): string {
  if (!pool) return t('读取中…')
  if (pool.error) return t('读取失败：{error}', { error: pool.error })
  if (pool.required === false) return t('当前不需要验证码令牌')
  const counts = `${Number(pool.ready) || 0} / ${Number(pool.target) || 3}`
  const prefix = t('验证码令牌：{counts}', { counts })
  if (pool.needsTokens === false && !Number(pool.ready)) {
    return `${prefix}${t('（未启用套餐池账号，暂不补铸）')}`
  }
  if (mint?.state === 'error') return `${prefix} · ${t('补铸失败：{error}', { error: mint.error })}`
  if (mint?.state === 'waiting') return `${prefix} · ${t('等待当前验证码流程结束')}`
  if (mint?.state === 'minting') return `${prefix} · ${t('正在补铸验证码令牌')}`
  return Number(pool.ready) === 0
    ? `${prefix}${t('（库存为空，等待补铸；请保持主窗口或浏览器面板打开）')}`
    : prefix
}

export function PlanChannelField({
  plan, hasJwt, onChange,
}: {
  plan: string
  hasJwt: boolean
  onChange: (next: string) => void
}) {
  const [pool, setPool] = React.useState<PoolStats | null>(null)
  const [mint, setMint] = React.useState<MintStatus | null>(null)
  React.useEffect(() => {
    if (plan !== ZCODE_PLAN_START) return
    let alive = true
    let running = false
    setPool(null)
    const tick = async () => {
      if (running) return
      running = true
      try {
        const bridge = shared().workbuddyDesktop
        const stats = bridge?.zcodeCaptchaStats ? await bridge.zcodeCaptchaStats() : null
        if (alive) {
          setPool(stats || { error: t('验证码令牌池接口不可用') })
          setMint(shared().wbZcodeCaptchaPool?.status?.() || null)
        }
      } catch (error) {
        if (alive) setPool({ error: errorMessage(error) })
      } finally {
        running = false
      }
    }
    void tick()
    const timer = window.setInterval(() => void tick(), 2500)
    return () => { alive = false; window.clearInterval(timer) }
  }, [plan])
  return (
    <div className='field-row mt-2.5'>
      <label htmlFor='account-plan-channel'>{t('使用套餐')}</label>
      <Select value={plan} onValueChange={value => onChange(String(value))}>
        <SelectTrigger id='account-plan-channel' className='min-w-[220px]'>
          <SelectValue>{zcodePlanLabel(plan)}</SelectValue>
        </SelectTrigger>
        <SelectContent>
          <SelectItem value={ZCODE_PLAN_CODING}>
            {t('编码套餐（Coding Plan）— 走开放平台端点，消耗开放平台的额度（订阅套餐 / 资源包 / 充值余额）')}
          </SelectItem>
          <SelectItem value={ZCODE_PLAN_START}>
            {t('套餐池（Start Plan）— 走 zcode.z.ai 端点，消耗 ZCode 套餐池的额度（新用户体验额度 / 活动领取）')}
          </SelectItem>
        </SelectContent>
      </Select>
      {plan === ZCODE_PLAN_START && !hasJwt ? (
        <span className='detail text-destructive'>
          {t('该账号没有套餐登录态（jwt），套餐池通道会失败：请重新登录该账号')}
        </span>
      ) : null}
      {plan === ZCODE_PLAN_START ? (
        <span className={`detail${pool?.error || (pool?.required !== false && mint?.state === 'error') ? ' text-destructive' : ''}`}
          title={t('需要验证码时由打开着的主窗口或浏览器面板补铸；库存、补铸状态和失败原因分别显示')}>
          {poolStatus(pool, mint)}
        </span>
      ) : null}
    </div>
  )
}
