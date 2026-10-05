import { describe, expect, it, vi } from "vitest"
import { CODEX_CLIENT_VERSION_FALLBACK, discoverOAuthModels, isDiscoverable } from "./model-discovery"
import type { OAuthCredentials } from "./types"

const creds: OAuthCredentials = { access: "tok", refresh: "r", expires: 0, accountId: "acct" }

function json(body: unknown, status = 200) {
  return new Response(JSON.stringify(body), { status, headers: { "Content-Type": "application/json" } })
}

type Call = { url: string; headers: Record<string, string> }

function recorder(handler: (url: string) => Response | Promise<Response>) {
  const calls: Call[] = []
  const fetchImpl = vi.fn(async (input: RequestInfo | URL, init?: RequestInit) => {
    const url = String(input)
    calls.push({ url, headers: (init?.headers ?? {}) as Record<string, string> })
    return handler(url)
  }) as unknown as typeof fetch
  return { calls, fetchImpl }
}

describe("isDiscoverable", () => {
  it("covers the providers with a list endpoint and leaves the rest bundled", () => {
    expect(isDiscoverable("anthropic")).toBe(true)
    expect(isDiscoverable("openai-codex")).toBe(true)
    expect(isDiscoverable("xai")).toBe(true)
    expect(isDiscoverable("google-gemini-cli")).toBe(false)
    expect(isDiscoverable("cursor")).toBe(false)
  })
})

describe("anthropic", () => {
  it("keeps the API's newest-first order and its display names", async () => {
    const { calls, fetchImpl } = recorder(() =>
      json({
        data: [
          { id: "claude-sonnet-5-5", display_name: "Claude Sonnet 5.5" },
          { id: "claude-opus-5-5", display_name: "Claude Opus 5.5" },
          { id: "claude-new" },
          { id: "  " },
        ],
      }),
    )
    const list = await discoverOAuthModels("anthropic", creds, fetchImpl)
    expect(list).toEqual([
      { id: "claude-sonnet-5-5", name: "Claude Sonnet 5.5" },
      { id: "claude-opus-5-5", name: "Claude Opus 5.5" },
      { id: "claude-new", name: "claude-new" },
    ])
    expect(calls[0]?.url).toBe("https://api.anthropic.com/v1/models?limit=100")
    expect(calls[0]?.headers.Authorization).toBe("Bearer tok")
    expect(calls[0]?.headers["Anthropic-Beta"]).toContain("oauth-2025-04-20")
    expect(calls[0]?.headers.Origin).toBe("")
  })

  it("returns null on a refused request so the bundled list stays", async () => {
    const { fetchImpl } = recorder(() => json({ error: "nope" }, 401))
    expect(await discoverOAuthModels("anthropic", creds, fetchImpl)).toBeNull()
  })

  it("returns null on an empty list and on a network failure", async () => {
    expect(await discoverOAuthModels("anthropic", creds, recorder(() => json({ data: [] })).fetchImpl)).toBeNull()
    const failing = vi.fn(async () => {
      throw new Error("offline")
    }) as unknown as typeof fetch
    expect(await discoverOAuthModels("anthropic", creds, failing)).toBeNull()
  })
})

describe("openai-codex", () => {
  const backend = {
    models: [
      { slug: "gpt-5.5", display_name: "GPT-5.5", priority: 13, visibility: "list" },
      { slug: "codex-auto-review", display_name: "Codex Auto Review", priority: 43, visibility: "hide" },
      { slug: "gpt-6.1-sol", display_name: "GPT-6.1-Sol", priority: 1, visibility: "list" },
      { slug: "gpt-api-only", priority: 0, supported_in_api: false },
      { slug: "gpt-reserve", priority: 4, visibility: "hide" },
    ],
  }

  it("asks with the current CLI version and offers only listed models by priority", async () => {
    const { calls, fetchImpl } = recorder((url) =>
      url.startsWith("https://registry.npmjs.org/") ? json({ version: "0.161.2" }) : json(backend),
    )
    const list = await discoverOAuthModels("openai-codex", creds, fetchImpl)
    expect(list).toEqual([
      { id: "gpt-6.1-sol", name: "GPT-6.1-Sol" },
      { id: "gpt-5.5", name: "GPT-5.5" },
    ])
    const modelsCall = calls.find((c) => c.url.startsWith("https://chatgpt.com/"))
    expect(modelsCall?.url).toBe("https://chatgpt.com/backend-api/codex/models?client_version=0.161.2")
    expect(modelsCall?.headers["chatgpt-account-id"]).toBe("acct")
    expect(modelsCall?.headers.originator).toBe("codex_cli_rs")
  })

  it("falls back to a current version when npm is unreachable or malformed", async () => {
    for (const npm of [() => json({}, 503), () => json({ version: "latest" })]) {
      const { calls, fetchImpl } = recorder((url) =>
        url.startsWith("https://registry.npmjs.org/") ? npm() : json(backend),
      )
      await discoverOAuthModels("openai-codex", creds, fetchImpl)
      const modelsCall = calls.find((c) => c.url.startsWith("https://chatgpt.com/"))
      expect(modelsCall?.url).toContain(`client_version=${CODEX_CLIENT_VERSION_FALLBACK}`)
    }
  })

  it("does not ask without an account id", async () => {
    const { calls, fetchImpl } = recorder(() => json(backend))
    const list = await discoverOAuthModels("openai-codex", { ...creds, accountId: undefined }, fetchImpl)
    expect(list).toBeNull()
    expect(calls).toHaveLength(0)
  })
})

describe("xai", () => {
  it("lists language models newest first", async () => {
    const { calls, fetchImpl } = recorder(() =>
      json({
        models: [
          { id: "grok-4.6", created: 1785974400 },
          { id: "grok-4.7", created: 1788307200 },
          { id: "grok-4.20-0309-reasoning", created: 1773014400 },
          { id: "grok-build-0.1", created: 1776297600 },
        ],
      }),
    )
    const list = await discoverOAuthModels("xai", creds, fetchImpl)
    expect(list).toEqual([
      { id: "grok-4.7", name: "Grok 4.7" },
      { id: "grok-4.6", name: "Grok 4.6" },
      { id: "grok-build-0.1", name: "Grok Build 0.1" },
      { id: "grok-4.20-0309-reasoning", name: "Grok 4.20 (Reasoning)" },
    ])
    expect(calls[0]?.url).toBe("https://api.x.ai/v1/language-models")
  })
})
