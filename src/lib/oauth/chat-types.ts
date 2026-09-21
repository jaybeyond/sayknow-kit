/**
 * Shapes shared by the per-provider chat adapters.
 *
 * Separate from `chat.ts` so an adapter can import them without pulling in the
 * dispatcher that imports the adapter back.
 */
import type { ChatImage } from "../chat-image"
import type { OAuthProvider } from "./types"

export type OAuthChatMessage = {
  role: "system" | "user" | "assistant"
  content: string
  /** Attached images; only user turns carry them. */
  images?: ChatImage[]
}

export type OAuthChatRequest = {
  provider: OAuthProvider
  model: string
  messages: OAuthChatMessage[]
  temperature?: number
  signal?: AbortSignal
  /**
   * Called with each chunk of assistant text as it arrives.
   *
   * Only providers whose transport streams honour this; the others answer in
   * one piece and never call it. The promise resolves with the full text
   * either way, so a caller that ignores this sees no difference.
   */
  onDelta?: (text: string) => void
  /**
   * Called when the provider's agent starts or finishes a tool call.
   *
   * Only Cursor runs tools today; every other provider answers with text
   * alone and never calls this.
   */
  onTool?: (tool: { callId: string; name: string; completed: boolean }) => void
}

export type OAuthChatResult = {
  content: string
  model: string
  usage?: { prompt_tokens?: number; completion_tokens?: number }
}
