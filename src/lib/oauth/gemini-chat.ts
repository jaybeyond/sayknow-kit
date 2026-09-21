/**
 * Gemini chat over Cloud Code Assist.
 *
 * Shape taken from sayknow-cli's `providers/google-gemini-cli.ts`
 * (`CloudCodeAssistRequest` / `CloudCodeAssistResponseChunk`):
 *
 * - `POST https://cloudcode-pa.googleapis.com/v1internal:streamGenerateContent?alt=sse`
 * - the real request is nested: `{ project, model, request: { contents, ... } }`,
 *   not the public Gemini `generateContent` body
 * - `project` is the Cloud project id discovered during sign-in and stored on
 *   the credentials; without it the endpoint has nothing to bill
 * - Gemini uses `model`/`user` roles, so assistant turns are renamed
 * - responses are SSE chunks wrapped in a `response` envelope
 */
import { httpFetch } from "../http"
import { getGeminiCliHeaders } from "./google-gemini-headers"
import type { OAuthChatMessage, OAuthChatResult } from "./chat-types"

const CODE_ASSIST_ENDPOINT = "https://cloudcode-pa.googleapis.com"
const STREAM_PATH = "/v1internal:streamGenerateContent?alt=sse"

export type GeminiRequest = {
  accessToken: string
  projectId: string
  model: string
  messages: OAuthChatMessage[]
  temperature?: number
  signal?: AbortSignal
}

type ResponseChunk = {
  response?: {
    candidates?: Array<{ content?: { parts?: Array<{ text?: string; thought?: boolean }> } }>
    usageMetadata?: { promptTokenCount?: number; candidatesTokenCount?: number }
  }
}

/**
 * Concatenate the visible text from the SSE chunks.
 *
 * Parts flagged `thought` are the model's reasoning, not its answer, so they
 * are skipped rather than shown.
 */
export function collectGeminiText(frames: string[]): {
  text: string
  usage?: { prompt_tokens?: number; completion_tokens?: number }
} {
  let text = ""
  let usage: { prompt_tokens?: number; completion_tokens?: number } | undefined

  for (const raw of frames) {
    if (!raw.startsWith("data:")) continue
    const payload = raw.slice(5).trim()
    if (!payload || payload === "[DONE]") continue
    try {
      const chunk = JSON.parse(payload) as ResponseChunk
      for (const part of chunk.response?.candidates?.[0]?.content?.parts ?? []) {
        if (part.thought) continue
        if (typeof part.text === "string") text += part.text
      }
      const meta = chunk.response?.usageMetadata
      if (meta) {
        usage = {
          prompt_tokens: meta.promptTokenCount,
          completion_tokens: meta.candidatesTokenCount,
        }
      }
    } catch {
      // Partial frame; the stream continues.
    }
  }

  return { text, usage }
}

export function splitSseFrames(body: string): string[] {
  return body
    .split(/\r?\n\r?\n/)
    .flatMap((block) => block.split(/\r?\n/))
    .filter((line) => line.startsWith("data:"))
}

export async function chatGemini(request: GeminiRequest): Promise<OAuthChatResult> {
  if (!request.projectId) {
    throw new Error("Gemini credentials carry no Cloud project id — sign in again")
  }

  const systemText = request.messages
    .filter((m) => m.role === "system")
    .map((m) => m.content.trim())
    .filter(Boolean)
    .join("\n\n")

  const contents = request.messages
    .filter((m) => m.role !== "system")
    .map((m) => ({
      // Gemini names the assistant "model".
      role: m.role === "assistant" ? "model" : "user",
      // Images are `inlineData` parts alongside the text
      // (sayknow-cli `providers/google-shared.ts:219-225`).
      parts: [
        ...(m.content ? [{ text: m.content }] : []),
        ...(m.images ?? [])
          .filter((img) => img.data)
          .map((img) => ({ inlineData: { mimeType: img.mimeType, data: img.data } })),
      ],
    }))

  const response = await httpFetch(`${CODE_ASSIST_ENDPOINT}${STREAM_PATH}`, {
    method: "POST",
    headers: {
      "Content-Type": "application/json",
      Accept: "text/event-stream",
      Authorization: `Bearer ${request.accessToken}`,
      ...getGeminiCliHeaders(request.model),
      // See the note in chat.ts: the plugin otherwise attaches the webview
      // origin and the call is treated as browser traffic.
      Origin: "",
    },
    body: JSON.stringify({
      project: request.projectId,
      model: request.model,
      request: {
        contents,
        ...(systemText
          ? // Upstream only puts a `role` on systemInstruction for the
            // Antigravity variant; the plain Cloud Code path omits it.
            { systemInstruction: { parts: [{ text: systemText }] } }
          : {}),
        ...(request.temperature !== undefined
          ? { generationConfig: { temperature: request.temperature } }
          : {}),
      },
    }),
    signal: request.signal,
  })

  if (!response.ok) {
    const body = await response.text().catch(() => "")
    throw new Error(`${response.status}: ${body.slice(0, 300) || response.statusText}`)
  }

  const { text, usage } = collectGeminiText(splitSseFrames(await response.text()))
  if (!text) throw new Error("Empty response from model")

  return { content: text.trim(), model: request.model, usage }
}
