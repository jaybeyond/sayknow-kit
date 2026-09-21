import { beforeEach, describe, expect, it, vi } from "vitest"
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
  return {
    ...actual,
    chatCursor: (...args: unknown[]) => chatCursorMock(...args),
  }
})

const { oauthChat, OAuthAuthError, OAuthUnsupportedError } = await import("./chat")

const credentials: OAuthCredentials = {
  access: "access-token",
  refresh: "refresh-token",
  expires: Date.now() + 60_000,
}

function jsonResponse(body: unknown, status = 200): Response {
  return new Response(JSON.stringify(body), {
    status,
    headers: { "Content-Type": "application/json" },
  })
}

beforeEach(() => {
  fetchMock.mockReset()
  ensureAccessTokenMock.mockReset()
  ensureAccessTokenMock.mockResolvedValue({ status: "ready", credentials })
  chatCursorMock.mockReset()
})

describe("oauthChat auth handling", () => {
  it("reports signed-out as an auth error the UI can act on", async () => {
    ensureAccessTokenMock.mockResolvedValue({ status: "signed-out" })
    await expect(
      oauthChat({ provider: "anthropic", model: "m", messages: [] }),
    ).rejects.toBeInstanceOf(OAuthAuthError)
    expect(fetchMock).not.toHaveBeenCalled()
  })

  it("reports a failed refresh as an auth error, not a network failure", async () => {
    ensureAccessTokenMock.mockResolvedValue({ status: "reauth-required", reason: "expired" })
    await expect(
      oauthChat({ provider: "xai", model: "m", messages: [] }),
    ).rejects.toBeInstanceOf(OAuthAuthError)
  })

  it("routes cursor to the Rust-side stream instead of refusing it", async () => {
    chatCursorMock.mockResolvedValue({ content: "from cursor", model: "composer-1" })

    const result = await oauthChat({ provider: "cursor", model: "composer-1", messages: [] })

    expect(result.content).toBe("from cursor")
    expect(chatCursorMock).toHaveBeenCalledWith(
      expect.objectContaining({ accessToken: credentials.access, model: "composer-1" }),
    )
    // The webview never talks to Cursor directly; the stream lives in Rust.
    expect(fetchMock).not.toHaveBeenCalled()
  })

  it("turns a cursor re-login into the same auth error every provider raises", async () => {
    const { CursorAuthRequired } = await import("./cursor-chat")
    chatCursorMock.mockRejectedValue(new CursorAuthRequired("token expired"))

    await expect(
      oauthChat({ provider: "cursor", model: "composer-1", messages: [] }),
    ).rejects.toBeInstanceOf(OAuthAuthError)
  })

  it("keeps the unsupported-provider guard available for future providers", () => {
    const error = new OAuthUnsupportedError("cursor")
    expect(error).toBeInstanceOf(Error)
    expect(error.provider).toBe("cursor")
  })
})

describe("anthropic", () => {
  it("sends the OAuth bearer and the beta headers the token requires", async () => {
    fetchMock.mockResolvedValue(
      jsonResponse({ content: [{ type: "text", text: "hi" }], model: "claude-x" }),
    )

    await oauthChat({
      provider: "anthropic",
      model: "claude-x",
      messages: [{ role: "user", content: "hello" }],
    })

    const [url, init] = fetchMock.mock.calls[0] as [string, RequestInit]
    expect(url).toBe("https://api.anthropic.com/v1/messages")
    const headers = init.headers as Record<string, string>
    // A subscription token is not an API key — x-api-key would be rejected.
    expect(headers.Authorization).toBe("Bearer access-token")
    expect(headers["x-api-key"]).toBeUndefined()
    expect(headers["Anthropic-Beta"]).toContain("oauth-2025-04-20")
    // Anthropic only honours a subscription token for traffic that identifies
    // as Claude Code; without these it authenticates and then 429s.
    expect(headers["X-App"]).toBe("cli")
    expect(headers["User-Agent"]).toMatch(/^claude-cli\//)
  })

  it("declares direct browser access, which Anthropic demands of any request carrying an Origin", async () => {
    fetchMock.mockResolvedValue(
      jsonResponse({ content: [{ type: "text", text: "hi" }], model: "claude-x" }),
    )

    await oauthChat({
      provider: "anthropic",
      model: "claude-x",
      messages: [{ role: "user", content: "hello" }],
    })

    // The Tauri HTTP plugin runs the request from Rust but still forwards the
    // webview's Origin, so without this the API answers 401 "CORS requests
    // must set 'anthropic-dangerous-direct-browser-access'".
    const headers = (fetchMock.mock.calls[0] as [string, RequestInit])[1].headers as Record<
      string,
      string
    >
    expect(headers["Anthropic-Dangerous-Direct-Browser-Access"]).toBe("true")
    // An empty Origin makes the Tauri plugin strip the header instead of
    // sending one. With an Origin present, an org that forbids CORS rejects
    // the request outright, beta header or not.
    expect(headers.Origin).toBe("")
  })

  it("lifts system turns into the top-level field the Messages API expects", async () => {
    fetchMock.mockResolvedValue(
      jsonResponse({ content: [{ type: "text", text: "ok" }], model: "claude-x" }),
    )

    await oauthChat({
      provider: "anthropic",
      model: "claude-x",
      messages: [
        { role: "system", content: "be terse" },
        { role: "user", content: "hello" },
      ],
    })

    const body = JSON.parse((fetchMock.mock.calls[0] as [string, RequestInit])[1].body as string)
    // The client identity has to lead; the caller's prompt follows it.
    expect(body.system[0].text).toContain("Claude Agent SDK")
    expect(body.system[1]).toEqual({ type: "text", text: "be terse" })
    expect(body.messages).toEqual([{ role: "user", content: "hello" }])
  })

  it("still leads with the client identity when the caller sends no system turn", async () => {
    fetchMock.mockResolvedValue(
      jsonResponse({ content: [{ type: "text", text: "ok" }], model: "claude-x" }),
    )

    await oauthChat({
      provider: "anthropic",
      model: "claude-x",
      messages: [{ role: "user", content: "hello" }],
    })

    const body = JSON.parse((fetchMock.mock.calls[0] as [string, RequestInit])[1].body as string)
    // Dropping the block when there is no caller prompt is exactly the case
    // that reads as unmetered traffic and comes back 429.
    expect(body.system).toHaveLength(1)
    expect(body.system[0].text).toContain("Claude Agent SDK")
  })

  it("joins text blocks and maps token usage", async () => {
    fetchMock.mockResolvedValue(
      jsonResponse({
        content: [
          { type: "text", text: "he" },
          { type: "thinking", text: "ignored" },
          { type: "text", text: "llo" },
        ],
        model: "claude-x",
        usage: { input_tokens: 7, output_tokens: 3 },
      }),
    )

    const result = await oauthChat({
      provider: "anthropic",
      model: "claude-x",
      messages: [{ role: "user", content: "hi" }],
    })

    expect(result.content).toBe("hello")
    expect(result.usage).toEqual({ prompt_tokens: 7, completion_tokens: 3 })
  })

  it("turns a 401 into an auth error so the UI asks for sign-in again", async () => {
    fetchMock.mockResolvedValue(jsonResponse({ error: "unauthorized" }, 401))
    await expect(
      oauthChat({ provider: "anthropic", model: "m", messages: [] }),
    ).rejects.toBeInstanceOf(OAuthAuthError)
  })
})

describe("xai", () => {
  it("uses the OpenAI-compatible endpoint with the bearer token", async () => {
    fetchMock.mockResolvedValue(
      jsonResponse({ choices: [{ message: { content: "yo" } }], model: "grok-x" }),
    )

    const result = await oauthChat({
      provider: "xai",
      model: "grok-x",
      messages: [{ role: "user", content: "hi" }],
    })

    const [url, init] = fetchMock.mock.calls[0] as [string, RequestInit]
    expect(url).toBe("https://api.x.ai/v1/chat/completions")
    expect((init.headers as Record<string, string>).Authorization).toBe("Bearer access-token")
    expect(result.content).toBe("yo")
  })

  it("rejects an empty completion instead of returning a blank answer", async () => {
    fetchMock.mockResolvedValue(jsonResponse({ choices: [], model: "grok-x" }))
    await expect(
      oauthChat({ provider: "xai", model: "m", messages: [] }),
    ).rejects.toThrow("Empty response")
  })
})
