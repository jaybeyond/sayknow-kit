import { beforeEach, describe, expect, it, vi } from "vitest"
import type { OAuthCredentials } from "./types"

const vault = new Map<string, string>()
vi.mock("../secrets", () => ({
  namedSecret: {
    get: vi.fn(async (account: string) => vault.get(account) ?? ""),
    set: vi.fn(async (account: string, key: string) => {
      vault.set(account, key)
    }),
    clear: vi.fn(async (account: string) => {
      vault.delete(account)
    }),
  },
}))

// The provider's answer to the token request. Each case sets what it returns.
const http = vi.hoisted(() => ({ answer: async (): Promise<Response> => new Response("{}") }))
vi.mock("../http", () => ({ httpFetch: () => http.answer() }))

// The flows themselves are exercised against the real provider endpoints, not
// here. These tests are about what `ensureAccessToken` does around them.
const refreshAnthropic = vi.fn()
vi.mock("./anthropic", () => ({
  loginAnthropic: vi.fn(),
  refreshAnthropicToken: (...args: unknown[]) => refreshAnthropic(...args),
}))
vi.mock("./xai", () => ({ loginXai: vi.fn(), refreshXaiToken: vi.fn() }))
vi.mock("./openai-codex", () => ({
  loginOpenAICodex: vi.fn(),
  refreshOpenAICodexToken: vi.fn(),
}))
vi.mock("./google-gemini-cli", () => ({
  loginGeminiCli: vi.fn(),
  refreshGoogleCloudToken: vi.fn(),
}))

const { ensureAccessToken } = await import("./registry")
const { oauthAccount } = await import("./store")

const live: OAuthCredentials = {
  access: "live-access",
  refresh: "refresh-token",
  expires: Date.now() + 60_000,
}
const stale: OAuthCredentials = { ...live, access: "stale-access", expires: Date.now() - 1 }

beforeEach(() => {
  vault.clear()
  refreshAnthropic.mockReset()
})

describe("ensureAccessToken", () => {
  it("reports signed-out when nothing is stored", async () => {
    expect(await ensureAccessToken("anthropic")).toEqual({ status: "signed-out" })
  })

  it("returns a live token without spending a refresh", async () => {
    vault.set(oauthAccount("anthropic"), JSON.stringify(live))

    const result = await ensureAccessToken("anthropic")

    expect(result).toEqual({ status: "ready", credentials: live })
    expect(refreshAnthropic).not.toHaveBeenCalled()
  })

  it("refreshes an expired token and persists the replacement", async () => {
    vault.set(oauthAccount("anthropic"), JSON.stringify(stale))
    const renewed = { ...live, access: "renewed-access" }
    refreshAnthropic.mockResolvedValue(renewed)

    const result = await ensureAccessToken("anthropic")

    expect(result).toEqual({ status: "ready", credentials: renewed })
    expect(refreshAnthropic).toHaveBeenCalledWith("refresh-token", expect.anything())
    // The new token must survive, or the next call refreshes all over again.
    expect(JSON.parse(vault.get(oauthAccount("anthropic")) ?? "{}")).toEqual(renewed)
  })

  /** The refresh posts to the token endpoint, then fails the way a provider
   *  flow does when that request did not give it a token. */
  function refreshPostsThenFails(message: string) {
    refreshAnthropic.mockImplementation(async (_token: string, fetch: typeof globalThis.fetch) => {
      await fetch("https://token.example", { method: "POST" })
      throw new Error(message)
    })
  }

  it("asks for re-auth and drops the credentials when the provider refuses the refresh", async () => {
    vault.set(oauthAccount("anthropic"), JSON.stringify(stale))
    http.answer = async () => new Response('{"error":"invalid_grant"}', { status: 400 })
    refreshPostsThenFails("invalid_grant")

    const result = await ensureAccessToken("anthropic")

    expect(result).toEqual({ status: "reauth-required", reason: "invalid_grant" })
    // Nothing may retry with a refresh token the provider just rejected.
    expect(vault.has(oauthAccount("anthropic"))).toBe(false)
  })

  it("keeps the sign-in when the provider cannot be reached (offline after waking)", async () => {
    vault.set(oauthAccount("anthropic"), JSON.stringify(stale))
    http.answer = async () => {
      throw new TypeError("Load failed")
    }
    refreshAnthropic.mockImplementation(async (_token: string, fetch: typeof globalThis.fetch) =>
      fetch("https://token.example", { method: "POST" }).then(() => live),
    )

    const result = await ensureAccessToken("anthropic")

    expect(result).toEqual({ status: "unavailable", reason: "Load failed" })
    expect(JSON.parse(vault.get(oauthAccount("anthropic")) ?? "{}")).toEqual(stale)
  })

  it.each([500, 503, 408, 429])("keeps the sign-in when the token endpoint answers %i", async (status) => {
    vault.set(oauthAccount("anthropic"), JSON.stringify(stale))
    http.answer = async () => new Response("busy", { status })
    refreshPostsThenFails(`HTTP ${status}`)

    const result = await ensureAccessToken("anthropic")

    expect(result).toEqual({ status: "unavailable", reason: `HTTP ${status}` })
    expect(vault.has(oauthAccount("anthropic"))).toBe(true)
  })

  it("does not judge the token by a refused request that is not the token POST", async () => {
    vault.set(oauthAccount("anthropic"), JSON.stringify(stale))
    http.answer = async () => new Response("not found", { status: 404 })
    refreshAnthropic.mockImplementation(async (_token: string, fetch: typeof globalThis.fetch) => {
      await fetch("https://discovery.example")
      throw new Error("discovery failed")
    })

    expect(await ensureAccessToken("anthropic")).toMatchObject({ status: "unavailable" })
    expect(vault.has(oauthAccount("anthropic"))).toBe(true)
  })

  it("runs one refresh for callers that arrive together", async () => {
    vault.set(oauthAccount("anthropic"), JSON.stringify(stale))
    const renewed = { ...live, access: "renewed-access" }
    let finish!: (value: OAuthCredentials) => void
    refreshAnthropic.mockImplementation(() => new Promise<OAuthCredentials>((r) => (finish = r)))

    const first = ensureAccessToken("anthropic")
    const second = ensureAccessToken("anthropic")
    await vi.waitFor(() => expect(refreshAnthropic).toHaveBeenCalledTimes(1))
    finish(renewed)

    // A second refresh with the same single-use token would be refused and
    // read as a revoked sign-in.
    expect(await first).toEqual({ status: "ready", credentials: renewed })
    expect(await second).toEqual({ status: "ready", credentials: renewed })
    expect(refreshAnthropic).toHaveBeenCalledTimes(1)
  })

  it("asks for re-auth when an expired record carries no refresh token", async () => {
    vault.set(oauthAccount("anthropic"), JSON.stringify({ ...stale, refresh: "" }))

    const result = await ensureAccessToken("anthropic")

    expect(result).toMatchObject({ status: "reauth-required" })
    expect(refreshAnthropic).not.toHaveBeenCalled()
  })
})
