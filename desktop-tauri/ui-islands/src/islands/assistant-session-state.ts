import * as React from 'react'
import { t } from '../i18n'
import { clearSessions, deleteSession, saveSession, toast, type AssistantConfig, type ChatMessage, type SessionSummary } from './assistant-state'
import type { ImageAsset } from '../../../ui-shared/assistant-image-tools'

export type CollectedTool = { toolCallId: string; content: string; images?: ImageAsset[] }

type SessionRefs = {
  config: { current: AssistantConfig | null }
  messages: { current: ChatMessage[] }
  activeId: { current: string }
  title: { current: string }
}

export function titleFrom(messages: ChatMessage[]): string {
  const first = messages.find(message => message.role === 'user')
  const text = first?.content.trim().replace(/\s+/g, ' ') || ''
  return text.length > 40 ? `${text.slice(0, 40)}…` : text
}

export function useAssistantSessions(refs: SessionRefs) {
  const [sessions, setSessions] = React.useState<SessionSummary[]>([])
  const queueRef = React.useRef<Promise<void>>(Promise.resolve())
  const deletedRef = React.useRef(new Set<string>())
  const clearingRef = React.useRef(false)
  const generationRef = React.useRef(0)

  const persist = React.useCallback((bump = true, collected: CollectedTool[] = []): Promise<void> => {
    const id = refs.activeId.current
    if (!id || deletedRef.current.has(id) || clearingRef.current) return Promise.resolve()
    const messages: ChatMessage[] = [...refs.messages.current, ...collected.map(item => ({ role: 'tool' as const, ...item }))]
    if (!messages.length) return Promise.resolve()
    const generation = generationRef.current
    const payload = {
      id, messages, title: refs.title.current || titleFrom(messages),
      model: refs.config.current?.model ?? null, reasoning: refs.config.current?.reasoning ?? null,
    }
    queueRef.current = queueRef.current.catch(() => {}).then(async () => {
      if (generation !== generationRef.current || deletedRef.current.has(id)) return
      try {
        const saved = await saveSession(payload)
        if (generation !== generationRef.current || deletedRef.current.has(id)) return
        if (refs.activeId.current === id) refs.title.current = saved.title
        setSessions(previous => {
          const index = previous.findIndex(item => item.id === id)
          if (index < 0 || bump) return [saved, ...previous.filter(item => item.id !== id)]
          return previous.map(item => item.id === id ? saved : item)
        })
      } catch (failure) {
        toast(t('这条会话没能存进历史：{message}', { message: failure instanceof Error ? failure.message : String(failure) }), 'err')
      }
    })
    return queueRef.current
  }, [refs.activeId, refs.config, refs.messages, refs.title])

  const remove = React.useCallback(async (id: string) => {
    deletedRef.current.add(id)
    const pending = queueRef.current.catch(() => {}).then(() => deleteSession(id))
    queueRef.current = pending.catch(() => {})
    try {
      await pending
      setSessions(previous => previous.filter(item => item.id !== id))
    } catch (failure) {
      deletedRef.current.delete(id)
      throw failure
    }
  }, [])

  const clear = React.useCallback(async () => {
    clearingRef.current = true
    generationRef.current += 1
    const id = refs.activeId.current
    const pending = queueRef.current.catch(() => {}).then(() => clearSessions())
    queueRef.current = pending.then(() => {}, () => {})
    try {
      await pending
      deletedRef.current.add(id)
      setSessions([])
    } finally {
      clearingRef.current = false
    }
  }, [refs.activeId])

  return { sessions, setSessions, persist, remove, clear }
}
