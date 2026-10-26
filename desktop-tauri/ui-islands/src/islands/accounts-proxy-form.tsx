/** 账号设置与批量操作共用的出网代理表单。 */

import * as React from 'react'
import {
  Button, Checkbox, Input, Label, MultiSelect, RadioGroup, RadioGroupItem,
  Select, SelectContent, SelectItem, SelectTrigger, SelectValue,
  Field, FieldGroup, FieldLabel,
} from '@ui'
import { t } from '../i18n'
import {
  errorMessage, esc, poolItemLabel, shared, toast,
  type AccountRecord, type ClashSnapshot, type PoolItem,
} from './accounts-shared'
import { clashOptions, getStore, proxyPoolOptions } from './accounts-data'

/** 每条候选都会真实出网，一轮最多测 8 条。 */
const ROTATE_TEST_LIMIT = 8

export type ProxyPayload =
  | null
  | { source: 'pool'; proxyId: string }
  | { source: 'rotate'; all: boolean; proxyIds?: string[] }
  | { source: 'clash'; listenerUid: string }
  | { source: 'custom'; protocol: 'http' | 'socks5'; host: string; port: number; username: string; password: string }

export type ProxyDraft = {
  mode: 'none' | 'pool' | 'rotate' | 'clash' | 'custom'
  /** 池条目 id（mode === 'pool'） */
  proxyId: string
  /** 轮询档：true = 池里全部启用的出口（"全部"，默认档；false = 只在 rotateIds 里轮） */
  rotateAll: boolean
  /** 轮询档：指定要轮询的池条目 id（rotateAll 为 true 时不用） */
  rotateIds: string[]
  listenerUid: string
  protocol: 'http' | 'socks5'
  host: string
  port: string
  username: string
  password: string
}

/** 账号 proxy 字段 → 表单草稿（形态与后端 workbuddy-proxy.mjs 一致） */
export function draftOfProxy(proxy: AccountRecord['proxy']): ProxyDraft {
  const source = proxy?.config?.source || proxy?.source
  const config = proxy?.config || null
  const rotate = source === 'rotate'
  return {
    mode: source === 'pool' ? 'pool'
      : rotate ? 'rotate'
        : source === 'clash' ? 'clash' : source === 'custom' ? 'custom' : 'none',
    proxyId: source === 'pool' ? String(config?.proxyId || '') : '',
    // `all` 缺键按「全部」读（后端同一个口径：那是这一档的默认语义）
    rotateAll: rotate ? config?.all !== false : true,
    rotateIds: rotate && Array.isArray(config?.proxyIds) ? config.proxyIds.map(String) : [],
    listenerUid: config?.source === 'clash' || source === 'clash' ? String(config?.listenerUid || '') : '',
    protocol: config?.protocol === 'socks5' ? 'socks5' : 'http',
    host: config?.host || '',
    port: config?.port === undefined || config?.port === null ? '' : String(config.port),
    username: config?.username || '',
    password: config?.password || '',
  }
}

/** 草稿 → 接口 payload；非法输入抛出「面向用户」的错误（消息直接进 toast / 弹窗状态行） */
export function readProxyDraft(draft: ProxyDraft): ProxyPayload {
  if (draft.mode === 'none') return null
  if (draft.mode === 'pool') {
    if (!draft.proxyId) throw new Error(t('请先选择已保存的代理'))
    return { source: 'pool', proxyId: draft.proxyId }
  }
  if (draft.mode === 'rotate') {
    // 「全部」不写 proxyIds：池里以后新增的出口自动进轮询（后端归一也是这个形状）
    if (draft.rotateAll) return { source: 'rotate', all: true }
    if (!draft.rotateIds.length) throw new Error(t('轮询至少要选择一条代理（或改选「全部」）'))
    return { source: 'rotate', all: false, proxyIds: [...draft.rotateIds] }
  }
  if (draft.mode === 'clash') {
    if (!draft.listenerUid) throw new Error(t('请先选择 Clash Verge 出口'))
    return { source: 'clash', listenerUid: draft.listenerUid }
  }
  const host = draft.host.trim()
  const port = Number(draft.port)
  if (!host) throw new Error(t('请填写代理主机地址'))
  if (!Number.isInteger(port) || port < 1 || port > 65535) throw new Error(t('代理端口必须是 1-65535 的整数'))
  return {
    source: 'custom',
    protocol: draft.protocol === 'socks5' ? 'socks5' : 'http',
    host,
    port,
    username: draft.username.trim(),
    password: draft.password,
  }
}

function clashOptionLabel(option: NonNullable<ClashSnapshot['options']>[number]): string {
  const inactive = option.profileActive === false ? t('（其他订阅，可能未生效）') : ''
  const disabled = option.enabled === false ? t('（已在 Clash 中禁用）') : ''
  return `${option.name} :${option.port}${inactive}${disabled}`
}

/**
 * 出网代理表单（账号设置与批量改代理共用）。刻意**不带** `data-island-input`：
 * 那是输入框岛（就地升级）的钩子，两个岛同时挂一个输入框会打架。
 *
 * 档位：
 *   · 无代理（直连）/ 自定义 —— 与改造前一致；
 *   · **已保存的代理** —— 引用「网络代理」页的池条目（推荐路径：出口在那里
 *     配一次、测一次，所有引用它的账号一起生效）。选项文案是
 *     「名字（协议 主机:端口）」，与账号页代理列同一格式（见 poolItemLabel）；
 *   · **轮询** —— 逐请求轮换池里的出口（换 IP）。**只在 `allowRotate` 为真时
 *     出现**（OpenCode Zen 免费档，判据见 accounts-domain 的 supportsProxyRotate
 *     —— 那个上游按 IP 限流）。默认「全部代理节点」（池里每个启用的出口轮流
 *     使用），取消勾选后从池里多选出要轮询的几条；
 *   · **Clash Verge** —— **只在账号当前就是直接引用 Clash 出口时出现**：
 *     那种存量记录要能改（切到别的档就消失，因为出口统一走池之后不再提供
 *     「新建一条 Clash 直引」的入口；想引用 Clash 出口请先到「网络代理」页
 *     点「同步 Clash Verge」，再从上面那一档选）。
 */
export function ProxyForm({
  draft,
  onChange,
  idPrefix,
  allowRotate = false,
}: {
  draft: ProxyDraft
  onChange: (next: ProxyDraft) => void
  idPrefix: string
  /** 是否给出「轮询」档（只有 OpenCode Zen 账号为真，见函数说明） */
  allowRotate?: boolean
}) {
  const store = getStore()
  const clash = store.clash
  const options = Array.isArray(clash?.options) ? clash.options : []
  /** Clash 直引档的可见性：见函数说明（存量记录专用） */
  const showClashMode = draft.mode === 'clash'
  const [testing, setTesting] = React.useState(false)
  const [testResult, setTestResult] = React.useState<React.ReactNode>(null)
  /** 池列表：「已保存的代理」与「轮询」两档共用的选项（模块级缓存，见 accounts-data） */
  const [pool, setPool] = React.useState<PoolItem[] | null>(null)
  const [poolError, setPoolError] = React.useState('')
  const set = (patch: Partial<ProxyDraft>): void => onChange({ ...draft, ...patch })

  // 出口列表只在 Clash 直引档出现时才需要（存量记录的下拉）—— 别的档不为它
  // 打这次请求
  React.useEffect(() => {
    if (showClashMode) void clashOptions().catch(() => { /* 画成「不可用」那一支 */ })
  }, [showClashMode])

  /** 读一次代理池（表单里两个入口：首次挂载与「重新读取」） */
  const loadPool = React.useCallback((force = false): void => {
    setPoolError('')
    void proxyPoolOptions({ force })
      .then(items => setPool(items))
      .catch(error => {
        setPool([])
        setPoolError(errorMessage(error))
      })
  }, [])
  // 只有选中（或初始就是）引用池的那两档时才真正去读 —— 其余档的用户不必为一次
  // 多余的请求买单；读完缓存住，切到这一档不会再打网络
  React.useEffect(() => {
    if ((draft.mode === 'pool' || draft.mode === 'rotate') && pool === null) loadPool()
  }, [draft.mode, pool, loadPool])

  /**
   * 轮询档当前会用到哪些条目（与后端 `proxy_rotate::candidate_ids` 同口径：
   * 「全部」= 池里启用的条目，顺序即池序；指定档 = 表单里选的那些）。
   */
  function rotateCandidateIds(): string[] {
    if (draft.rotateAll) {
      return (pool ?? []).filter(item => item.enabled !== false).map(item => item.id)
    }
    return [...draft.rotateIds]
  }

  /** 用当前表单内容测试出口连通性 */
  async function test(): Promise<void> {
    if (testing) return
    // 轮询档：逐条测候选（每条都走 testProxyPoolItem —— 结果会记进那条代理的
    // 「上次测试」，与代理页那颗按钮同一个动作）。候选可能几十条，全测一遍是
    // 几十次往返，所以只测前 ROTATE_TEST_LIMIT 条并把「还有几条没测」说出来；
    // 只为看某一条通不通时，用「网络代理」页的行内「测试」更直接。
    if (draft.mode === 'rotate') {
      const ids = rotateCandidateIds()
      if (!ids.length) {
        toast(t('轮询至少要选择一条代理（或改选「全部」）'), 'err')
        return
      }
      const targets = ids.slice(0, ROTATE_TEST_LIMIT)
      setTesting(true)
      setTestResult(t('正在连接上游…'))
      let ok = 0
      const failures: string[] = []
      for (const id of targets) {
        const name = pool?.find(item => item.id === id)?.name || id
        try {
          const data = await shared().workbuddyDesktop?.testProxyPoolItem?.(id)
          if (data?.success) ok += 1
          else failures.push(`${name}（${data?.error || t('连接失败')}）`)
        } catch (error) {
          failures.push(`${name}（${errorMessage(error)}）`)
        }
      }
      const rest = ids.length - targets.length
      const summary = t('✅ {ok}/{total} 个出口可用', { ok, total: targets.length })
        + (rest > 0 ? t('（只测了前 {limit} 个，还有 {rest} 个未测）', { limit: ROTATE_TEST_LIMIT, rest }) : '')
      setTestResult(
        <span className={failures.length ? 'text-destructive' : 'text-success'}>
          {summary}{failures.length ? `　${esc(failures.join(t('；')))}` : ''}
        </span>,
      )
      setTesting(false)
      return
    }
    // 池引用：测的是**那条已保存的条目**（testProxyPoolItem 会把结果记进它的
    // 「上次测试」，与代理页那一颗按钮同一个动作）—— 未选条目时给出提示
    if (draft.mode === 'pool') {
      if (!draft.proxyId) {
        toast(t('请先选择已保存的代理'), 'err')
        return
      }
      setTesting(true)
      setTestResult(t('正在连接上游…'))
      try {
        const data = await shared().workbuddyDesktop?.testProxyPoolItem?.(draft.proxyId)
        setTestResult(data?.success
          ? (
              <span className='text-success'>
                {t('✅ 出口可用')}
                {data.ip ? t('　出口 IP {ip}', { ip: esc(data.ip) }) : ''}
                {data.durationMs !== undefined && data.durationMs !== '' ? `　${esc(String(data.durationMs))}ms` : ''}
              </span>
            )
          : <span className='text-destructive'>❌ {esc(data?.error || t('连接失败'))}</span>)
      } catch (error) {
        setTestResult(<span className='text-destructive'>❌ {esc(errorMessage(error))}</span>)
      } finally {
        setTesting(false)
      }
      return
    }
    let proxy: ProxyPayload
    try {
      proxy = readProxyDraft(draft)
    } catch (error) {
      toast(errorMessage(error), 'err')
      return
    }
    setTesting(true)
    setTestResult(t('正在连接上游…'))
    try {
      const data = await shared().workbuddyDesktop?.testProxy?.({ proxy })
      if (data?.success) {
        setTestResult(
          <span className='text-success'>
            {t('✅ 出口可用')}
            {data.status !== undefined && data.status !== '' ? `　HTTP ${esc(String(data.status))}` : ''}
            {data.ip ? t('　出口 IP {ip}', { ip: esc(data.ip) }) : ''}
            {data.durationMs !== undefined && data.durationMs !== '' ? `　${esc(String(data.durationMs))}ms` : ''}
          </span>,
        )
      } else {
        setTestResult(<span className='text-destructive'>❌ {esc(data?.error || t('连接失败'))}</span>)
      }
    } catch (error) {
      setTestResult(<span className='text-destructive'>❌ {esc(errorMessage(error))}</span>)
    } finally {
      setTesting(false)
    }
  }

  return (
    <div className='flex flex-col'>
      <RadioGroup value={draft.mode} onValueChange={value => set({ mode: value as ProxyDraft['mode'] })}
        className='flex-row flex-wrap items-center gap-5' aria-label={t('代理方式')}>
        <Label className='inline-flex cursor-pointer items-center gap-2 font-normal'>
          <RadioGroupItem value='none' />{t('无代理（直连）')}
        </Label>
        <Label className='inline-flex cursor-pointer items-center gap-2 font-normal'>
          <RadioGroupItem value='pool' />{t('已保存的代理')}
        </Label>
        {/* 轮询档只对 OpenCode Zen 出现（见 allowRotate 的说明） */}
        {allowRotate ? (
          <Label className='inline-flex cursor-pointer items-center gap-2 font-normal'
            title={t('逐请求轮换出口（换 IP）：OpenCode Zen 免费档按 IP 限流，单个 IP 的额度用完会对它一律 429')}>
            <RadioGroupItem value='rotate' />{t('轮询（换 IP）')}
          </Label>
        ) : null}
        {/* Clash 直引档只对**已经是这种配置**的账号出现（见函数说明）：
            出口统一走代理池之后不再提供新建入口 */}
        {showClashMode ? (
          <Label className='inline-flex cursor-pointer items-center gap-2 font-normal'>
            <RadioGroupItem value='clash' />Clash Verge
          </Label>
        ) : null}
        <Label className='inline-flex cursor-pointer items-center gap-2 font-normal'>
          <RadioGroupItem value='custom' />{t('自定义')}
        </Label>
      </RadioGroup>

      {draft.mode === 'pool' ? (
        <div className='mt-3'>
          <Field orientation='horizontal' className='field-row'>
            <FieldLabel htmlFor={`${idPrefix}-pool-exit`}>{t('代理')}</FieldLabel>
            {/* value 恒为字符串（空串 = 还没选）：受控值从 undefined 切到字符串会被 Base UI
                当成「非受控 → 受控」的切换，所以不给 undefined */}
            <Select value={draft.proxyId} disabled={!pool?.length}
              onValueChange={value => set({ proxyId: String(value) })}>
              <SelectTrigger id={`${idPrefix}-pool-exit`} className='min-w-[220px]'>
                <SelectValue>
                  {pool?.find(item => item.id === draft.proxyId)
                    ? poolItemLabel(pool.find(item => item.id === draft.proxyId)!)
                    : (pool === null ? t('正在读取…') : pool.length ? t('请选择代理') : t('还没有保存的代理'))}
                </SelectValue>
              </SelectTrigger>
              <SelectContent>
                {(pool ?? []).map(item => (
                  // 已禁用的条目置灰不可选：选它保存后解析必然失败（转发回退直连），
                  // 用户只会看到「代理已禁用」。要启用请去「网络代理」页。
                  // 当前值恰好是禁用项时仍正常显示（触发器读的是同一份 label）
                  <SelectItem key={item.id} value={item.id} disabled={item.enabled === false}>
                    {poolItemLabel(item)}
                  </SelectItem>
                ))}
              </SelectContent>
            </Select>
            <Button variant='outline' size='sm' onClick={() => loadPool(true)}>{t('重新读取')}</Button>
          </Field>
          <div className='detail mt-1.5'>
            {poolError
              ? t('读取失败：{error}', { error: poolError })
              : pool === null
                ? t('正在读取代理列表…')
                : pool.length
                  ? t('地址与端口由「网络代理」页管理：那边改一次、测一次，所有引用它的账号一起生效')
                  : t('「网络代理」页还没有出口 —— 去那里新增，或在「订阅管理」里点「同步 Clash Verge」把 Clash 的出口导进来')}
          </div>
        </div>
      ) : null}

      {draft.mode === 'rotate' ? (
        <div className='mt-3'>
          {/* 「全部」是默认档：勾上 = 池里每个启用的出口轮流使用（新加的出口
              自动进轮询，不必回来改这条账号）。取消勾选才出现多选。
              这一行刻意**不走 .field-row**：账号设置弹窗里它的直接 label 子元素
              会被 `.acct-dlg-main .field-row > label` 强制成 76px 定宽字段标签，
              「全部代理节点（推荐）」比 76px 长，nowrap 溢出会压住后面的说明
              （实测叠字）—— 所以用普通 flex 行，文案跟在复选框后面自然排 */}
          <div className='flex flex-wrap items-center gap-x-2.5 gap-y-1'>
            <Checkbox id={`${idPrefix}-rotate-all`} checked={draft.rotateAll}
              aria-label={t('轮询全部代理节点')}
              onCheckedChange={value => set({ rotateAll: value === true })} />
            <Label htmlFor={`${idPrefix}-rotate-all`} className='cursor-pointer text-xs text-subtle'>
              {t('全部代理节点（推荐）')}
            </Label>
            <span className='detail'>
              {t('「网络代理」页里每个启用的出口轮流使用；之后新加的出口自动加入')}
            </span>
          </div>
          {!draft.rotateAll ? (
            <Field orientation='horizontal' className='field-row mt-2.5'>
              <FieldLabel htmlFor={`${idPrefix}-rotate-exits`}>{t('轮询的代理')}</FieldLabel>
              <MultiSelect id={`${idPrefix}-rotate-exits`}
                value={draft.rotateIds}
                onValueChange={values => set({ rotateIds: values.map(String) })}
                // 禁用项置灰：后端轮询取用时会跳过它们（`candidate_ids` 只留启用项），
                // 列出来能选等于让用户白挑一条永不生效的出口
                options={(pool ?? []).map(item => ({
                  value: item.id,
                  label: poolItemLabel(item),
                  disabled: item.enabled === false,
                }))}
                placeholder={t('选择要轮询的代理（可多选）')}
                aria-label={t('轮询的代理')} />
              <Button variant='outline' size='sm' onClick={() => loadPool(true)}>{t('重新读取')}</Button>
            </Field>
          ) : null}
          <div className='detail mt-1.5'>
            {poolError
              ? t('读取失败：{error}', { error: poolError })
              : pool === null
                ? t('正在读取代理列表…')
                : pool.length
                  ? t('每条请求换一个出口（按顺序轮流），上游看到的 IP 因此是变化的；出口的地址与端口由「网络代理」页管理')
                  : t('「网络代理」页还没有出口 —— 去那里新增，或在「订阅管理」里点「同步 Clash Verge」把 Clash 的出口导进来')}
          </div>
        </div>
      ) : null}

      {draft.mode === 'clash' ? (
        <div className='mt-3'>
          <Field orientation='horizontal' className='field-row'>
            <FieldLabel htmlFor={`${idPrefix}-clash-exit`}>{t('出口')}</FieldLabel>
            {/* value 恒为字符串（空串 = 还没选）：受控值从 undefined 切到字符串会被 Base UI
                当成「非受控 → 受控」的切换，所以不给 undefined */}
            <Select value={draft.listenerUid} disabled={!options.length}
              onValueChange={value => set({ listenerUid: String(value) })}>
              <SelectTrigger id={`${idPrefix}-clash-exit`} className='min-w-[220px]'>
                <SelectValue>
                  {options.find(option => String(option.uid) === draft.listenerUid)
                    ? clashOptionLabel(options.find(option => String(option.uid) === draft.listenerUid)!)
                    : (clash?.available === false ? t('未检测到 Clash Verge') : options.length ? t('请选择出口') : t('没有可用出口'))}
                </SelectValue>
              </SelectTrigger>
              <SelectContent>
                {options.map(option => (
                  <SelectItem key={String(option.uid)} value={String(option.uid)}>
                    {clashOptionLabel(option)}
                  </SelectItem>
                ))}
              </SelectContent>
            </Select>
            <Button variant='outline' size='sm' onClick={() => {
              void clashOptions({ force: true })
                .then(() => toast(t('✅ 已重新读取 Clash Verge 配置')))
                .catch(error => toast(t('读取失败：{error}', { error: errorMessage(error) }), 'err'))
            }}>{t('重新读取')}</Button>
          </Field>
          <div className='detail mt-1.5'>
            {clash === null
              ? t('正在读取 Clash Verge 配置…')
              : clash.available === false
                ? (clash.error ? t('不可用：{error}', { error: clash.error }) : t('未检测到 Clash Verge 配置'))
                : options.length
                  ? t('读取自 {dir}；端口由 Clash Verge 管理，这里实时同步', { dir: clash.dir || 'Clash Verge' })
                  : t('Clash Verge 里还没有配置混合监听器或节点端口')}
          </div>
          <p className='detail mt-1.5'>
            {t('这是既有配置（直接引用 Clash 出口，不经过代理池）。推荐改用上面的')}
            <b>{t('「已保存的代理」')}</b>
            {t('：到「网络代理」页的「订阅管理」里点「同步 Clash Verge」把出口导进池后即可选 —— 那样能集中测试、也能被多个账号共用。')}
          </p>
        </div>
      ) : null}

      {draft.mode === 'custom' ? (
        <div className='mt-3'>
          <FieldGroup orientation='horizontal' className='field-row'>
            <Field className='contents'>
              <FieldLabel htmlFor={`${idPrefix}-protocol`}>{t('协议')}</FieldLabel>
              <Select value={draft.protocol} onValueChange={value => set({ protocol: value as 'http' | 'socks5' })}>
                <SelectTrigger id={`${idPrefix}-protocol`} className='min-w-[110px]'>
                  <SelectValue>{draft.protocol === 'socks5' ? 'SOCKS5' : 'HTTP'}</SelectValue>
                </SelectTrigger>
                <SelectContent>
                  <SelectItem value='http'>HTTP</SelectItem>
                  <SelectItem value='socks5'>SOCKS5</SelectItem>
                </SelectContent>
              </Select>
            </Field>
            <Field className='contents'>
              <FieldLabel htmlFor={`${idPrefix}-host`} className='ml-2'>{t('主机')}</FieldLabel>
              <Input id={`${idPrefix}-host`} className='min-w-[140px]' placeholder='127.0.0.1'
                value={draft.host} onChange={event => set({ host: event.currentTarget.value })} />
            </Field>
            <Field className='contents'>
              <FieldLabel htmlFor={`${idPrefix}-port`}>{t('端口')}</FieldLabel>
              <Input id={`${idPrefix}-port`} type='number' min={1} max={65535} className='max-w-[110px]'
                placeholder='7890' value={draft.port} onChange={event => set({ port: event.currentTarget.value })} />
            </Field>
          </FieldGroup>
          <FieldGroup orientation='horizontal' className='field-row mt-2.5'>
            <Field className='contents'>
              <FieldLabel htmlFor={`${idPrefix}-user`}>{t('用户名')}</FieldLabel>
              <Input id={`${idPrefix}-user`} placeholder={t('可选')} autoComplete='off'
                value={draft.username} onChange={event => set({ username: event.currentTarget.value })} />
            </Field>
            <Field className='contents'>
              <FieldLabel htmlFor={`${idPrefix}-pass`}>{t('密码')}</FieldLabel>
              <Input id={`${idPrefix}-pass`} type='password' placeholder={t('可选')} autoComplete='new-password'
                value={draft.password} onChange={event => set({ password: event.currentTarget.value })} />
            </Field>
          </FieldGroup>
        </div>
      ) : null}

      <FieldGroup orientation='horizontal' className='field-row mt-3'>
        <Button variant='outline' size='sm' disabled={testing} onClick={() => void test()}>
          {testing ? t('测试中…') : t('测试出口')}
        </Button>
        <span className='detail'>{testResult}</span>
      </FieldGroup>
    </div>
  )
}
