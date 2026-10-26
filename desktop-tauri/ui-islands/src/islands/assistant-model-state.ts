import * as React from 'react'
import { loadModelOptions } from './assistant-tools'
import type { ModelOption } from './assistant-models'

export function useAssistantModels() {
  const [models, setModels] = React.useState<ModelOption[]>([])
  const [ready, setReady] = React.useState(false)
  const [loading, setLoading] = React.useState(false)
  const [error, setError] = React.useState('')
  const taskRef = React.useRef<Promise<ModelOption[]> | null>(null)
  const mountedRef = React.useRef(true)

  const refresh = React.useCallback((): Promise<ModelOption[]> => {
    if (taskRef.current) return taskRef.current
    setLoading(true)
    taskRef.current = loadModelOptions().then(options => {
      if (mountedRef.current) {
        setModels(options)
        setReady(true)
        setError('')
      }
      return options
    }).catch(failure => {
      if (mountedRef.current) {
        setReady(false)
        setError(failure instanceof Error ? failure.message : String(failure))
      }
      throw failure
    }).finally(() => {
      taskRef.current = null
      if (mountedRef.current) setLoading(false)
    })
    return taskRef.current
  }, [])

  React.useEffect(() => {
    mountedRef.current = true
    const refreshVisible = () => {
      if (document.hidden || !document.querySelector('.page.active[data-page="assistant"]')) return
      void refresh().catch(() => {})
    }
    const interval = window.setInterval(refreshVisible, 20_000)
    document.addEventListener('visibilitychange', refreshVisible)
    return () => {
      mountedRef.current = false
      window.clearInterval(interval)
      document.removeEventListener('visibilitychange', refreshVisible)
    }
  }, [refresh])

  return { models, ready, loading, error, refresh }
}
