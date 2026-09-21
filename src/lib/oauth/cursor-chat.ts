/**
 * Cursor chat, run by the Rust side.
 *
 * Cursor's agent protocol is Connect-RPC over a bidirectional HTTP/2 stream:
 * after the request is sent the client still has to heartbeat, answer the
 * server's blob reads, and reply to its context handshake. A webview `fetch`
 * cannot do any of that, so the turn runs in `src-tauri/src/cursor_chat.rs`
 * and this module is the thin bridge to it.
 *
 * Tokens arrive as `cursor:delta` events while the turn is in flight; the
 * command itself resolves with the finished text, which keeps the existing
 * non-streaming `oauthChat` contract intact.
 */
import { invoke } from "@tauri-apps/api/core"
import { listen, type UnlistenFn } from "@tauri-apps/api/event"
import type { OAuthChatMessage, OAuthChatResult } from "./chat-types"

/** Prefix the Rust side puts on errors that mean "sign in again". */
export const CURSOR_AUTH_ERROR_PREFIX = "cursor-auth: "

export type CursorDeltaEvent = { request_id: string; text: string }
export type CursorToolEvent = {
  request_id: string
  call_id: string
  name: string
  completed: boolean
}

export type CursorChatRequest = {
  accessToken: string
  model: string
  messages: OAuthChatMessage[]
  signal?: AbortSignal
  /** Called for each streamed chunk of assistant text. */
  onDelta?: (text: string) => void
  /** Called when the agent starts or finishes a tool call. */
  onTool?: (tool: { callId: string; name: string; completed: boolean }) => void
}

type CursorChatResponse = {
  content: string
  model: string
  tokens: number
}

export class CursorAuthRequired extends Error {
  constructor(message: string) {
    super(message)
    this.name = "CursorAuthRequired"
  }
}

function requestId(): string {
  return crypto.randomUUID()
}

/**
 * Run one Cursor turn.
 *
 * Aborting invokes the cancel command rather than just dropping the promise:
 * the stream, and anything it spawned, lives in Rust and has to be told.
 */
export async function chatCursor(request: CursorChatRequest): Promise<OAuthChatResult> {
  const id = requestId()
  const unlisteners: UnlistenFn[] = []

  if (request.onDelta) {
    unlisteners.push(
      await listen<CursorDeltaEvent>("cursor:delta", (event) => {
        if (event.payload.request_id === id) request.onDelta?.(event.payload.text)
      }),
    )
  }
  if (request.onTool) {
    unlisteners.push(
      await listen<CursorToolEvent>("cursor:tool", (event) => {
        if (event.payload.request_id !== id) return
        request.onTool?.({
          callId: event.payload.call_id,
          name: event.payload.name,
          completed: event.payload.completed,
        })
      }),
    )
  }

  const onAbort = () => {
    void invoke("cursor_chat_cancel", { requestId: id })
  }
  request.signal?.addEventListener("abort", onAbort, { once: true })

  try {
    if (request.signal?.aborted) throw new Error("Request was cancelled")

    const response = await invoke<CursorChatResponse>("cursor_chat_send", {
      request: {
        request_id: id,
        access_token: request.accessToken,
        model: request.model,
        messages: request.messages.map((m) => ({ role: m.role, content: m.content })),
      },
    })

    return {
      content: response.content,
      model: response.model,
      usage: { completion_tokens: response.tokens > 0 ? response.tokens : undefined },
    }
  } catch (error) {
    const message = error instanceof Error ? error.message : String(error)
    if (message.startsWith(CURSOR_AUTH_ERROR_PREFIX)) {
      throw new CursorAuthRequired(message.slice(CURSOR_AUTH_ERROR_PREFIX.length))
    }
    throw error instanceof Error ? error : new Error(message)
  } finally {
    request.signal?.removeEventListener("abort", onAbort)
    for (const unlisten of unlisteners) unlisten()
  }
}

/** Models this account may use, straight from Cursor's own list. */
export async function listCursorModels(accessToken: string): Promise<{ id: string; name: string }[]> {
  return invoke<{ id: string; name: string }[]>("cursor_list_models", { accessToken })
}
