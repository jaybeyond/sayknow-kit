/**
 * One attached image, five providers, five wire shapes. Each assertion is the
 * exact structure the reference implementation emits, so a drift here means
 * the request would be rejected or the picture silently dropped.
 */
import { beforeEach, describe, expect, it, vi } from "vitest"
import type { ChatImage } from "../chat-image"
import type { OAuthCredentials } from "./types"

const fetchMock = vi.fn()
vi.mock("../http", () => ({ httpFetch: (...args: unknown[]) => fetchMock(...args) }))

const ensureAccessTokenMock = vi.fn()
vi.mock("./registry", () => ({
  ensureAccessToken: (...args: unknown[]) => ensureAccessTokenMock(...args),
}))

const chatCursorMock = vi.fn()
vi.mock("./cursor-chat", async () => {
  const actual = await vi.importActual<typeof import("./cursor-chat")>("./cursor-chat")
  return { ...actual, chatCursor: (...args: unknown[]) => chatCursorMock(...args) }
})

const { oauthChat, toAnthropicContent, toOpenAIContent } = await import("./chat")
const { chatCodex } = await import("./codex-chat")
const { chatGemini } = await import("./gemini-chat")

const image: ChatImage = {
  id: "img-1",
  mimeType: "image/png",
  data: "iVBORw0KGgo=",
  width: 2,
  height: 2,
  name: "shot.png",
}
const evicted: ChatImage = { ...image, id: "img-2", data: "" }

const credentials: OAuthCredentials = {
  access: "access-token",
  refresh: "refresh-token",
  expires: Date.now() + 60_000,
  accountId: "acct",
  projectId: "proj",
}

function sentBody(): Record<string, unknown> {
  const [, init] = fetchMock.mock.calls[0] as [string, RequestInit]
  return JSON.parse(init.body as string)
}

function jsonResponse(body: unknown): Response {
  return new Response(JSON.stringify(body), {
    status: 200,
    headers: { "Content-Type": "application/json" },
  })
}

beforeEach(() => {
  fetchMock.mockReset()
  ensureAccessTokenMock.mockReset()
  chatCursorMock.mockReset()
  ensureAccessTokenMock.mockResolvedValue({ status: "ready", credentials })
})

describe("content builders", () => {
  it("keep text-only turns as a plain string so strict servers still accept them", () => {
    const msg = { role: "user" as const, content: "hi" }
    expect(toAnthropicContent(msg)).toBe("hi")
    expect(toOpenAIContent(msg)).toBe("hi")
  })

  it("skip evicted images instead of sending an empty payload", () => {
    const msg = { role: "user" as const, content: "hi", images: [evicted] }
    expect(toAnthropicContent(msg)).toBe("hi")
    expect(toOpenAIContent(msg)).toBe("hi")
  })

  it("allow an image with no text at all", () => {
    const msg = { role: "user" as const, content: "", images: [image] }
    expect(toOpenAIContent(msg)).toEqual([
      { type: "image_url", image_url: { url: "data:image/png;base64,iVBORw0KGgo=" } },
    ])
  })
})

describe("anthropic", () => {
  it("sends base64 image blocks ahead of the text (providers/anthropic.ts:698-702)", async () => {
    fetchMock.mockResolvedValue(
      jsonResponse({ content: [{ type: "text", text: "a cat" }], model: "claude" }),
    )
    await oauthChat({
      provider: "anthropic",
      model: "claude",
      messages: [{ role: "user", content: "what is this?", images: [image] }],
    })
    const body = sentBody() as { messages: Array<{ role: string; content: unknown }> }
    expect(body.messages[0].content).toEqual([
      {
        type: "image",
        source: { type: "base64", media_type: "image/png", data: "iVBORw0KGgo=" },
      },
      { type: "text", text: "what is this?" },
    ])
  })
})

describe("xai", () => {
  it("sends OpenAI image_url parts (providers/openai-completions.ts:1597-1599)", async () => {
    fetchMock.mockResolvedValue(
      jsonResponse({ choices: [{ message: { content: "a cat" } }], model: "grok" }),
    )
    await oauthChat({
      provider: "xai",
      model: "grok",
      messages: [{ role: "user", content: "what is this?", images: [image] }],
    })
    const body = sentBody() as { messages: Array<{ role: string; content: unknown }> }
    expect(body.messages[0].content).toEqual([
      { type: "text", text: "what is this?" },
      { type: "image_url", image_url: { url: "data:image/png;base64,iVBORw0KGgo=" } },
    ])
  })
})

describe("codex", () => {
  it("sends input_image parts with a data url (providers/openai-responses-shared.ts:210-214)", async () => {
    const payload = { "https://api.openai.com/auth": { chatgpt_account_id: "acct" } }
    const segment = btoa(JSON.stringify(payload)).replace(/=+$/, "")
    fetchMock.mockResolvedValue(
      new Response(
        `data: ${JSON.stringify({ type: "response.output_text.delta", delta: "a cat" })}\n\ndata: [DONE]`,
        { status: 200 },
      ),
    )
    await chatCodex({
      accessToken: `h.${segment}.s`,
      model: "gpt",
      messages: [{ role: "user", content: "what is this?", images: [image] }],
    })
    const body = sentBody() as { input: Array<{ content: unknown }> }
    expect(body.input[0].content).toEqual([
      { type: "input_text", text: "what is this?" },
      { type: "input_image", detail: "auto", image_url: "data:image/png;base64,iVBORw0KGgo=" },
    ])
  })
})

describe("gemini", () => {
  it("sends inlineData parts (providers/google-shared.ts:219-225)", async () => {
    fetchMock.mockResolvedValue(
      new Response(
        `data: ${JSON.stringify({
          response: { candidates: [{ content: { parts: [{ text: "a cat" }] } }] },
        })}\n\n`,
        { status: 200 },
      ),
    )
    await chatGemini({
      accessToken: "t",
      projectId: "proj",
      model: "gemini",
      messages: [{ role: "user", content: "what is this?", images: [image] }],
    })
    const body = sentBody() as { request: { contents: Array<{ parts: unknown }> } }
    expect(body.request.contents[0].parts).toEqual([
      { text: "what is this?" },
      { inlineData: { mimeType: "image/png", data: "iVBORw0KGgo=" } },
    ])
  })
})

describe("cursor", () => {
  it("refuses an attachment rather than sending the question without it", async () => {
    await expect(
      oauthChat({
        provider: "cursor",
        model: "composer-1",
        messages: [{ role: "user", content: "what is this?", images: [image] }],
      }),
    ).rejects.toThrow(/image/i)
    expect(chatCursorMock).not.toHaveBeenCalled()
  })

  it("still answers text once the evicted image carries no data", async () => {
    chatCursorMock.mockResolvedValue({ content: "ok", model: "composer-1" })
    await oauthChat({
      provider: "cursor",
      model: "composer-1",
      messages: [{ role: "user", content: "hi", images: [evicted] }],
    })
    expect(chatCursorMock).toHaveBeenCalled()
  })
})
