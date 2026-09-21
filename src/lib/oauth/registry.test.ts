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

  it("asks for re-auth and drops the credentials when refresh fails", async () => {
    vault.set(oauthAccount("anthropic"), JSON.stringify(stale))
    refreshAnthropic.mockRejectedValue(new Error("invalid_grant"))

    const result = await ensureAccessToken("anthropic")

    expect(result).toEqual({ status: "reauth-required", reason: "invalid_grant" })
    // Nothing may retry with a refresh token the provider just rejected.
    expect(vault.has(oauthAccount("anthropic"))).toBe(false)
  })

  it("asks for re-auth when an expired record carries no refresh token", async () => {
    vault.set(oauthAccount("anthropic"), JSON.stringify({ ...stale, refresh: "" }))

    const result = await ensureAccessToken("anthropic")

    expect(result).toMatchObject({ status: "reauth-required" })
    expect(refreshAnthropic).not.toHaveBeenCalled()
  })
})
