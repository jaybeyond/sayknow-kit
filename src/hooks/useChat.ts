import { useCallback, useEffect, useRef, useState } from "react"
import {
  conversations,
  type ChatMsg,
  type Conversation,
} from "@/lib/chat-history"
import type { ChatImage } from "@/lib/chat-image"
import { chat as openrouterChat, type ChatMessage, type ProviderId } from "@/lib/openrouter"

const SYSTEM_PROMPT =
  "You are a helpful, concise assistant. Answer in the user's language. " +
  "Keep responses focused and to the point — no filler, no unnecessary preamble."

type Args = {
  apiKey: string
  baseURL: string
  /** Selected backend. OAuth providers route to their own API. */
  provider?: ProviderId
  model: string
  fallbackModel?: string
}

export function useChat({ apiKey, baseURL, provider, model, fallbackModel }: Args) {
  const [list, setList] = useState<Conversation[]>(() => conversations.list())
  const [currentId, setCurrentId] = useState<string | null>(
    () => conversations.currentId(),
  )
  const [sending, setSending] = useState(false)
  /**
   * Assistant text as it arrives, before the turn finishes.
   *
   * Providers that answer in one piece leave this empty; the finished message
   * is appended to the conversation either way, so this is only what the UI
   * shows while the answer is still being written.
   */
  const [streamingText, setStreamingText] = useState("")
  /**
   * Tools the provider's agent is running right now.
   *
   * Cursor can execute shell commands and file operations mid-answer; showing
   * them is the difference between "it froze" and "it is running your build".
   */
  const [activeTools, setActiveTools] = useState<string[]>([])
  const [error, setError] = useState<string | null>(null)
  const abortRef = useRef<AbortController | null>(null)

  const refresh = useCallback(() => {
    setList(conversations.list())
    setCurrentId(conversations.currentId())
  }, [])

  // Cross-window sync.
  useEffect(() => {
    function onStorage(e: StorageEvent) {
      if (e.key === "sayknow:conversations" || e.key === "sayknow:current-conversation") {
        refresh()
      }
    }
    window.addEventListener("storage", onStorage)
    return () => window.removeEventListener("storage", onStorage)
  }, [refresh])

  function ensureCurrent(): Conversation {
    let id = currentId
    if (!id) {
      const conv = conversations.create()
      id = conv.id
      setCurrentId(id)
      setList(conversations.list())
      return conv
    }
    const found = conversations.get(id)
    if (found) return found
    // Stored id no longer exists — create fresh.
    const conv = conversations.create()
    setCurrentId(conv.id)
    setList(conversations.list())
    return conv
  }

  const current: Conversation | null = currentId
    ? list.find((c) => c.id === currentId) ?? null
    : null
  const messages: ChatMsg[] = current?.messages ?? []

  const requestAssistant = useCallback(
    async (convId: string, currentMessages: ChatMsg[]) => {
      abortRef.current?.abort()
      const ctrl = new AbortController()
      abortRef.current = ctrl

      setError(null)
      setSending(true)

      const apiMessages: ChatMessage[] = [
        { role: "system", content: SYSTEM_PROMPT },
        ...currentMessages.map<ChatMessage>((m) => ({
          role: m.role,
          content: m.content,
          // Evicted images have data "" and are filtered at the wire; the
          // model then sees only the text, which is the honest degradation.
          ...(m.images?.length ? { images: m.images } : {}),
        })),
      ]

      try {
        setStreamingText("")
        setActiveTools([])
        const result = await openrouterChat({
          apiKey,
          baseURL,
          provider,
          model,
          fallbackModel,
          messages: apiMessages,
          signal: ctrl.signal,
          onDelta: (text) => {
            if (ctrl.signal.aborted) return
            setStreamingText((previous) => previous + text)
          },
          onTool: (tool) => {
            if (ctrl.signal.aborted) return
            setActiveTools((previous) =>
              tool.completed
                ? previous.filter((name) => name !== tool.name)
                : previous.includes(tool.name)
                  ? previous
                  : [...previous, tool.name],
            )
          },
        })
        if (ctrl.signal.aborted) return
        conversations.appendMessage(convId, {
          role: "assistant",
          content: result.content,
          model: result.model,
        })
        refresh()
      } catch (e) {
        if (ctrl.signal.aborted) return
        setError(e instanceof Error ? e.message : String(e))
      } finally {
        setStreamingText("")
        setActiveTools([])
        if (!ctrl.signal.aborted) setSending(false)
      }
    },
    [apiKey, baseURL, provider, model, fallbackModel, refresh],
  )

  const send = useCallback(
    async (text: string, images: ChatImage[] = []) => {
      const trimmed = text.trim()
      if (!trimmed && images.length === 0) return
      const conv = ensureCurrent()
      conversations.appendMessage(conv.id, {
        role: "user",
        content: trimmed,
        ...(images.length ? { images } : {}),
      })
      refresh()
      const next = conversations.get(conv.id)?.messages ?? []
      await requestAssistant(conv.id, next)
    },
    // eslint-disable-next-line react-hooks/exhaustive-deps
    [requestAssistant, currentId],
  )

  const regenerate = useCallback(
    async (assistantId: string) => {
      if (!currentId) return
      const conv = conversations.get(currentId)
      if (!conv) return
      const idx = conv.messages.findIndex((m) => m.id === assistantId)
      if (idx < 0) return
      if (conv.messages[idx].role !== "assistant") return
      const trimmed = conv.messages.slice(0, idx)
      const last = trimmed[trimmed.length - 1]
      if (!last || last.role !== "user") return
      conversations.setMessages(currentId, trimmed)
      refresh()
      await requestAssistant(currentId, trimmed)
    },
    [currentId, refresh, requestAssistant],
  )

  const editAndResend = useCallback(
    async (userId: string, newText: string, images: ChatImage[] = []) => {
      const trimmed = newText.trim()
      if ((!trimmed && images.length === 0) || !currentId) return
      const conv = conversations.get(currentId)
      if (!conv) return
      const idx = conv.messages.findIndex((m) => m.id === userId)
      if (idx < 0) return
      if (conv.messages[idx].role !== "user") return
      conversations.setMessages(currentId, conv.messages.slice(0, idx))
      refresh()
      await send(trimmed, images)
    },
    [currentId, refresh, send],
  )

  const stop = useCallback(() => {
    abortRef.current?.abort()
    setSending(false)
  }, [])

  const newConversation = useCallback(() => {
    abortRef.current?.abort()
    setSending(false)
    setError(null)
    const conv = conversations.create()
    setCurrentId(conv.id)
    setList(conversations.list())
  }, [])

  const switchTo = useCallback((id: string) => {
    abortRef.current?.abort()
    setSending(false)
    setError(null)
    conversations.setCurrent(id)
    setCurrentId(id)
  }, [])

  const deleteConversation = useCallback((id: string) => {
    abortRef.current?.abort()
    setSending(false)
    conversations.delete(id)
    refresh()
  }, [refresh])

  const rename = useCallback((id: string, title: string) => {
    conversations.rename(id, title)
    refresh()
  }, [refresh])

  return {
    list,
    current,
    messages,
    sending,
    streamingText,
    activeTools,
    error,
    setError,
    send,
    stop,
    regenerate,
    editAndResend,
    newConversation,
    switchTo,
    deleteConversation,
    rename,
  }
}
