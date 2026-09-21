/**
 * Chat requests for OAuth-backed providers.
 *
 * Once a browser sign-in has produced an access token, each provider is just
 * an HTTPS endpoint — but not the same one. Only the wire shapes that are
 * verified against sayknow-cli's provider code are implemented here; anything
 * unverified fails loudly instead of sending a guessed request.
 */
import { httpFetch } from "../http"
import { ANTHROPIC_OAUTH_BETAS } from "./anthropic"
import { chatCodex } from "./codex-chat"
import { chatGemini } from "./gemini-chat"
import { chatCursor, CursorAuthRequired } from "./cursor-chat"
import { ensureAccessToken } from "./registry"
import type { OAuthCredentials, OAuthProvider } from "./types"
import type { OAuthChatMessage, OAuthChatRequest, OAuthChatResult } from "./chat-types"
import { imageSrc } from "../chat-image"

/**
 * Anthropic Messages content for one turn.
 *
 * Text-only stays a string. With images, the content is a block array with
 * `image` blocks carrying base64 source — the shape in sayknow-cli
 * `providers/anthropic.ts:698-702`. Images go first so the text reads as a
 * question about them.
 */
export function toAnthropicContent(message: OAuthChatMessage):
  | string
  | Array<
      | { type: "text"; text: string }
      | { type: "image"; source: { type: "base64"; media_type: string; data: string } }
    > {
  const images = (message.images ?? []).filter((img) => img.data)
  if (images.length === 0) return message.content
  return [
    ...images.map((img) => ({
      type: "image" as const,
      source: { type: "base64" as const, media_type: img.mimeType, data: img.data },
    })),
    ...(message.content ? [{ type: "text" as const, text: message.content }] : []),
  ]
}

/**
 * OpenAI chat-completions content, shared with xAI which speaks the same
 * shape (`providers/openai-completions.ts:1597-1599`).
 */
export function toOpenAIContent(
  message: OAuthChatMessage,
): string | Array<{ type: "text"; text: string } | { type: "image_url"; image_url: { url: string } }> {
  const images = (message.images ?? []).filter((img) => img.data)
  if (images.length === 0) return message.content
  return [
    ...(message.content ? [{ type: "text" as const, text: message.content }] : []),
    ...images.map((img) => ({
      type: "image_url" as const,
      image_url: { url: imageSrc(img) },
    })),
  ]
}

export type { OAuthChatMessage, OAuthChatRequest, OAuthChatResult }

/** Raised when the caller has to send the user back through sign-in. */
export class OAuthAuthError extends Error {
  readonly provider: OAuthProvider

  constructor(message: string, provider: OAuthProvider) {
    super(message)
    this.name = "OAuthAuthError"
    this.provider = provider
  }
}

/**
 * Raised for a provider whose request contract is not yet implemented.
 *
 * No provider throws this today. It stays because it is the mechanism that
 * keeps an unimplemented provider honest: a new one refuses the request
 * instead of guessing a body.
 */
export class OAuthUnsupportedError extends Error {
  readonly provider: OAuthProvider

  constructor(provider: OAuthProvider) {
    super(`${provider} chat is not wired up yet`)
    this.name = "OAuthUnsupportedError"
    this.provider = provider
  }
}

async function readError(response: Response): Promise<string> {
  const body = await response.text().catch(() => "")
  return `${response.status}: ${body.slice(0, 300) || response.statusText}`
}

/**
 * Anthropic Messages API with a Claude subscription token.
 *
 * A subscription OAuth token is not an API key. Anthropic only honours it for
 * traffic that identifies as Claude Code, and the shape is copied from
 * sayknow-cli's `providers/anthropic.ts` (`buildAnthropicHeaders`, its OAuth
 * branch, and `claudeCodeSystemInstruction`):
 *
 * - the token rides in `Authorization`, never `x-api-key`
 * - the Claude Code beta set must be declared
 * - `X-App: cli` plus a `claude-cli/...` user agent
 * - the first system block identifies the client
 *
 * Sending a bare Messages request with just the token authenticates but is
 * then rejected as unmetered traffic — which surfaced as a 429.
 *
 * The Messages API also takes the system prompt as its own field rather than
 * a message, so system turns are lifted out of the conversation.
 */
const CLAUDE_CODE_VERSION = "2.1.267"
const CLAUDE_CODE_SYSTEM_INSTRUCTION =
  "You are a Claude agent, built on Anthropic's Claude Agent SDK."

async function chatAnthropic(
  request: OAuthChatRequest,
  credentials: OAuthCredentials,
): Promise<OAuthChatResult> {
  const callerSystem = request.messages
    .filter((m) => m.role === "system")
    .map((m) => m.content.trim())
    .filter(Boolean)
  const messages = request.messages
    .filter((m) => m.role !== "system")
    .map((m) => ({ role: m.role, content: toAnthropicContent(m) }))

  // Block array, not a string: the client identity has to be its own leading
  // block ahead of whatever the app asked for.
  const system = [
    { type: "text", text: CLAUDE_CODE_SYSTEM_INSTRUCTION },
    ...callerSystem.map((text) => ({ type: "text", text })),
  ]

  const response = await httpFetch("https://api.anthropic.com/v1/messages", {
    method: "POST",
    headers: {
      "Content-Type": "application/json",
      Authorization: `Bearer ${credentials.access}`,
      "Anthropic-Version": "2023-06-01",
      "Anthropic-Beta": ANTHROPIC_OAUTH_BETAS.join(","),
      "X-App": "cli",
      "User-Agent": `claude-cli/${CLAUDE_CODE_VERSION} (external, cli)`,
      // The Tauri HTTP plugin sets an `Origin` on every request, which makes
      // Anthropic treat the call as browser traffic — an organization can
      // refuse those outright. An empty value is the plugin's documented
      // opt-out: it strips the header rather than sending one (needs its
      // `unsafe-headers` feature, enabled in src-tauri/Cargo.toml).
      Origin: "",
      "Anthropic-Dangerous-Direct-Browser-Access": "true",
    },
    body: JSON.stringify({
      model: request.model,
      max_tokens: 4096,
      system,
      messages,
      ...(request.temperature !== undefined ? { temperature: request.temperature } : {}),
    }),
    signal: request.signal,
  })

  if (response.status === 401 || response.status === 403) {
    throw new OAuthAuthError(await readError(response), "anthropic")
  }
  if (!response.ok) throw new Error(await readError(response))

  const data = (await response.json()) as {
    content?: { type?: string; text?: string }[]
    model?: string
    usage?: { input_tokens?: number; output_tokens?: number }
  }
  const content = (data.content ?? [])
    .filter((block) => block.type === "text" && typeof block.text === "string")
    .map((block) => block.text as string)
    .join("")
  if (!content) throw new Error("Empty response from model")

  return {
    content: content.trim(),
    model: data.model ?? request.model,
    usage: {
      prompt_tokens: data.usage?.input_tokens,
      completion_tokens: data.usage?.output_tokens,
    },
  }
}

/**
 * xAI speaks the OpenAI chat-completions shape, so this is the plain path.
 * Confirmed by `providers/openai-completions-compat.ts`, which routes
 * `api.x.ai` through its OpenAI-compatible handler.
 */
async function chatXai(
  request: OAuthChatRequest,
  credentials: OAuthCredentials,
): Promise<OAuthChatResult> {
  const response = await httpFetch("https://api.x.ai/v1/chat/completions", {
    method: "POST",
    headers: {
      "Content-Type": "application/json",
      Authorization: `Bearer ${credentials.access}`,
      // Same reason as Anthropic: this is an app making a server-style call,
      // not a web page, so it should not advertise a browser origin.
      Origin: "",
    },
    body: JSON.stringify({
      model: request.model,
      messages: request.messages.map((m) => ({ role: m.role, content: toOpenAIContent(m) })),
      ...(request.temperature !== undefined ? { temperature: request.temperature } : {}),
    }),
    signal: request.signal,
  })

  if (response.status === 401 || response.status === 403) {
    throw new OAuthAuthError(await readError(response), "xai")
  }
  if (!response.ok) throw new Error(await readError(response))

  const data = (await response.json()) as {
    choices?: { message?: { content?: string } }[]
    model?: string
    usage?: { prompt_tokens?: number; completion_tokens?: number }
  }
  const content = data.choices?.[0]?.message?.content
  if (!content) throw new Error("Empty response from model")

  return { content: content.trim(), model: data.model ?? request.model, usage: data.usage }
}

/**
 * Send one chat turn through an OAuth provider.
 *
 * Refreshes the token first when it has aged out; a provider that needs a new
 * browser sign-in surfaces as `OAuthAuthError` so the UI can say so instead of
 * showing a bare HTTP failure.
 */
export async function oauthChat(request: OAuthChatRequest): Promise<OAuthChatResult> {
  const token = await ensureAccessToken(request.provider)
  if (token.status === "signed-out") {
    throw new OAuthAuthError("Not signed in", request.provider)
  }
  if (token.status === "reauth-required") {
    throw new OAuthAuthError(token.reason, request.provider)
  }

  switch (request.provider) {
    case "anthropic":
      return chatAnthropic(request, token.credentials)
    case "xai":
      return chatXai(request, token.credentials)
    case "openai-codex":
      return chatCodex({
        accessToken: token.credentials.access,
        accountId: token.credentials.accountId,
        model: request.model,
        messages: request.messages,
        signal: request.signal,
      })
    case "google-gemini-cli":
      return chatGemini({
        accessToken: token.credentials.access,
        projectId: token.credentials.projectId ?? "",
        model: request.model,
        messages: request.messages,
        temperature: request.temperature,
        signal: request.signal,
      })
    // Cursor runs in Rust: its agent protocol is a bidirectional HTTP/2
    // stream the webview cannot host. Streaming callbacks are optional, so
    // this call still resolves with the finished text like every other
    // provider here.
    case "cursor":
      // The Rust request builder sends text only. Dropping an attachment on
      // the floor would make the model answer a question it never saw, so
      // refuse instead until images are wired through the agent protocol.
      if (request.messages.some((m) => m.images?.some((img) => img.data))) {
        throw new Error("Cursor does not support image attachments yet")
      }
      try {
        return await chatCursor({
          accessToken: token.credentials.access,
          model: request.model,
          messages: request.messages,
          signal: request.signal,
          onDelta: request.onDelta,
          onTool: request.onTool,
        })
      } catch (error) {
        if (error instanceof CursorAuthRequired) {
          throw new OAuthAuthError(error.message, request.provider)
        }
        throw error
      }
  }
}
