/**
 * Codex chat over the ChatGPT backend Responses API.
 *
 * Shape taken from sayknow-cli's `providers/openai-codex-responses.ts` and
 * `providers/openai-codex/constants.ts`:
 *
 * - `POST https://chatgpt.com/backend-api/codex/responses`
 * - `OpenAI-Beta: responses=experimental`, plus the account id lifted out of
 *   the token's JWT and an `originator` tag
 * - system turns become `instructions`; the conversation becomes `input`
 *   items whose content blocks are `input_text`
 * - the endpoint only answers as SSE, so `stream: true` is not optional; the
 *   text arrives as `response.output_text.delta` events
 */
import { decodeJwt } from "./base64"
import { imageSrc } from "../chat-image"
import { httpFetch } from "../http"
import type { OAuthChatMessage, OAuthChatResult } from "./chat-types"

const CODEX_URL = "https://chatgpt.com/backend-api/codex/responses"
const JWT_CLAIM_PATH = "https://api.openai.com/auth"

/** Every Codex request is scoped to an account carried inside the token. */
export function codexAccountId(accessToken: string): string | undefined {
  const payload = decodeJwt<Record<string, unknown>>(accessToken)
  const auth = payload?.[JWT_CLAIM_PATH] as { chatgpt_account_id?: string } | undefined
  return auth?.chatgpt_account_id
}

export type CodexRequest = {
  accessToken: string
  accountId?: string
  model: string
  messages: OAuthChatMessage[]
  signal?: AbortSignal
}

/**
 * Pull the assistant text out of the SSE stream.
 *
 * Only `response.output_text.delta` carries visible text. Reasoning and tool
 * events are ignored rather than rendered — this app shows a plain answer.
 */
export function collectCodexText(events: string[]): string {
  let text = ""
  for (const raw of events) {
    if (!raw.startsWith("data:")) continue
    const payload = raw.slice(5).trim()
    if (!payload || payload === "[DONE]") continue
    try {
      const event = JSON.parse(payload) as { type?: string; delta?: string }
      if (event.type === "response.output_text.delta" && typeof event.delta === "string") {
        text += event.delta
      }
    } catch {
      // A partial or non-JSON frame is not fatal; the stream continues.
    }
  }
  return text
}

/** Split a raw SSE body into frames. */
export function splitSseFrames(body: string): string[] {
  return body
    .split(/\r?\n\r?\n/)
    .flatMap((block) => block.split(/\r?\n/))
    .filter((line) => line.startsWith("data:"))
}

export async function chatCodex(request: CodexRequest): Promise<OAuthChatResult> {
  const accountId = request.accountId ?? codexAccountId(request.accessToken)
  if (!accountId) {
    throw new Error("Codex token carries no account id")
  }

  const instructions = request.messages
    .filter((m) => m.role === "system")
    .map((m) => m.content.trim())
    .filter(Boolean)
    .join("\n\n")

  // The two roles take different content types. A user turn is *input* to the
  // model (`input_text`); an assistant turn is a replayed *output* item and
  // must be `output_text` inside a `message`. Using input_text for both is
  // rejected with
  // `Invalid value: 'input_text'. Supported values are: 'output_text' and 'refusal'`
  // pointing at the first assistant turn.
  const input = request.messages
    .filter((m) => m.role !== "system")
    .map((m) =>
      m.role === "assistant"
        ? {
            type: "message",
            role: "assistant",
            content: [{ type: "output_text", text: m.content, annotations: [] }],
            status: "completed",
          }
        : {
            role: m.role,
            // Images ride as `input_image` parts with a data URL
            // (sayknow-cli `providers/openai-responses-shared.ts:210-214`).
            content: [
              ...(m.content ? [{ type: "input_text", text: m.content }] : []),
              ...(m.images ?? [])
                .filter((img) => img.data)
                .map((img) => ({
                  type: "input_image",
                  detail: "auto",
                  image_url: imageSrc(img),
                })),
            ],
          },
    )

  const response = await httpFetch(CODEX_URL, {
    method: "POST",
    headers: {
      "Content-Type": "application/json",
      Accept: "text/event-stream",
      Authorization: `Bearer ${request.accessToken}`,
      "OpenAI-Beta": "responses=experimental",
      "chatgpt-account-id": accountId,
      originator: "codex_cli_rs",
      // The plugin otherwise attaches the webview origin and the request is
      // treated as browser traffic. See the note in chat.ts.
      Origin: "",
    },
    body: JSON.stringify({
      model: request.model,
      instructions: instructions || undefined,
      input,
      // Not a preference: this endpoint does not answer non-streaming.
      stream: true,
      store: false,
    }),
    signal: request.signal,
  })

  if (!response.ok) {
    const body = await response.text().catch(() => "")
    throw new Error(`${response.status}: ${body.slice(0, 300) || response.statusText}`)
  }

  const text = collectCodexText(splitSseFrames(await response.text()))
  if (!text) throw new Error("Empty response from model")

  return { content: text.trim(), model: request.model }
}
