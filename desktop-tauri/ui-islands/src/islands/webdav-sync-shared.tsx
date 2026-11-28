import * as React from 'react'

/**
 * WebDAV 同步 · 本页专用的「重新进入」信号与加载器。
 *
 * ── 为什么又写一份 `useLoad`（第三份了）──────────────────────
 * 这个函数目前有三份：`memory-shared.tsx`（记忆中枢）、`skills-hub-shared.tsx`
 * （技能仓库）、这里。看着像该抽公共实现，但抽不得 —— 差别不在函数体，在
 * **信号变量属于谁**：三份各自持有一个模块级的 `enterStamp`，各自监听自己那个
 * 页面的 `wb:page`。合并成一份就等于让三个页面共用一个戳，症状是「切到 WebDAV 同步，
 * 技能仓库跟着重扫磁盘」。
 *
 * 函数体本身是同一套口径（`loading` 只表示「还没拿到过数据」，重新进入时静默
 * 重拉、不闪骨架），抄的代价是 40 行，抽错的代价是三页互相串味。选前者。
 *
 * ── 为什么 WebDAV 同步要单独一份，而不是继续用记忆中枢那份 ─────────
 * 这一页原来是记忆中枢的第八个页签，`useLoad` 绑的是记忆页的进入信号。提升成
 * 一级页面后它有自己的菜单项、自己的 `data-page`，用户从侧边栏直接进来时
 * 记忆页的 `wb:page` 根本不会派发 —— 继续用旧信号的表现是「进来看到的是上次
 * 离开时的快照，只能手点刷新」。真机反馈就是这一条。
 */

let enterStamp = 0
let forceStamp = 0
const watchers = new Set<() => void>()

function bump(kind: 'enter' | 'force'): void {
  if (kind === 'enter') enterStamp += 1
  else forceStamp += 1
  for (const watcher of watchers) watcher()
}

/** WebDAV 同步页重新成为当前页（切回来、窗口重新可见）。 */
export function notifyEnter(): void {
  bump('enter')
}

/** 用户点了页面上的「刷新」：整页重拉，包括豁免了「进页面重拉」的那些。 */
export function notifyRefresh(): void {
  bump('force')
}

function watch(watcher: () => void): () => void {
  watchers.add(watcher)
  return () => { watchers.delete(watcher) }
}

/**
 * 本页的加载器。签名与 `memory-shared` 的同名函数一致，所以搬过来的组件体
 * 一行都不用改，只换 import 来源。
 */
export function useLoad<T>(
  loader: () => Promise<T>,
  deps: React.DependencyList,
  options?: { refreshOnEnter?: boolean },
): { data: T | null; loading: boolean; error: string; reload: () => void } {
  const [data, setData] = React.useState<T | null>(null)
  const [loading, setLoading] = React.useState(true)
  const [error, setError] = React.useState('')
  const [tick, setTick] = React.useState(0)
  /** 只为「信号变了要重渲染一次」而存在；真正的判据是下面的 `signal`。 */
  const [, rerender] = React.useState(0)
  const loaderRef = React.useRef(loader)
  loaderRef.current = loader
  /** 是否已经有过一次请求结束（成功或失败都算）。决定 `loading` 还该不该为真。 */
  const settled = React.useRef(false)

  React.useEffect(() => watch(() => rerender(value => value + 1)), [])

  // 进入页面 / 手动刷新合成的依赖项。没开启 `refreshOnEnter` 的端点只看
  // `forceStamp`，所以 `enterStamp` 变化时它的这一项不变，effect 不会重跑。
  const signal = `${options?.refreshOnEnter === true ? String(enterStamp) : 'stay'}:${forceStamp}`

  React.useEffect(() => {
    let alive = true
    if (!settled.current) setLoading(true)
    setError('')
    loaderRef.current()
      .then(value => { if (alive) setData(value) })
      .catch((caught: unknown) => {
        if (alive) setError(caught instanceof Error ? caught.message : String(caught))
      })
      .finally(() => {
        if (!alive) return
        settled.current = true
        setLoading(false)
      })
    return () => { alive = false }
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [...deps, tick, signal])

  return { data, loading, error, reload: () => setTick(value => value + 1) }
}
