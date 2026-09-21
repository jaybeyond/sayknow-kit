import { beforeEach, describe, expect, it, vi } from "vitest"
import type { OAuthCredentials } from "./types"

// The store sits on the app's Keychain wrapper; stub that boundary so these
// tests exercise the serialization and refresh policy, not Tauri.
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

const { isExpired, oauthAccount, oauthStore } = await import("./store")

const creds: OAuthCredentials = {
  access: "access-token",
  refresh: "refresh-token",
  expires: 1_000_000,
  email: "user@example.com",
}

beforeEach(() => {
  vault.clear()
})

describe("oauthAccount", () => {
  it("namespaces each provider so they cannot collide with plain secrets", () => {
    expect(oauthAccount("anthropic")).toBe("oauth_anthropic")
    expect(oauthAccount("xai")).toBe("oauth_xai")
    expect(oauthAccount("anthropic")).not.toBe(oauthAccount("openai-codex"))
    // `deepl_api_key` is the existing non-OAuth account; nothing may shadow it.
    expect(oauthAccount("anthropic")).not.toBe("deepl_api_key")
  })
})

describe("oauthStore", () => {
  it("round-trips credentials", async () => {
    await oauthStore.set("anthropic", creds)
    expect(await oauthStore.get("anthropic")).toEqual(creds)
  })

  it("keeps providers in separate slots", async () => {
    await oauthStore.set("anthropic", creds)
    await oauthStore.set("xai", { ...creds, access: "grok-token" })

    expect((await oauthStore.get("anthropic"))?.access).toBe("access-token")
    expect((await oauthStore.get("xai"))?.access).toBe("grok-token")
  })

  it("clearing one provider leaves the others signed in", async () => {
    await oauthStore.set("anthropic", creds)
    await oauthStore.set("xai", creds)

    await oauthStore.clear("anthropic")

    expect(await oauthStore.get("anthropic")).toBeNull()
    expect(await oauthStore.get("xai")).not.toBeNull()
  })

  it("reports absent credentials as null", async () => {
    expect(await oauthStore.get("google-gemini-cli")).toBeNull()
  })

  it("treats an unparseable blob as signed out instead of throwing", async () => {
    vault.set(oauthAccount("anthropic"), "{not json")
    expect(await oauthStore.get("anthropic")).toBeNull()
  })

  it("rejects a well-formed blob that is missing required fields", async () => {
    vault.set(oauthAccount("anthropic"), JSON.stringify({ access: "only-access" }))
    expect(await oauthStore.get("anthropic")).toBeNull()
  })
})

describe("isExpired", () => {
  it("is true once the stored deadline has passed", () => {
    expect(isExpired({ ...creds, expires: 500 }, 1_000)).toBe(true)
  })

  it("is false while the deadline is still ahead", () => {
    expect(isExpired({ ...creds, expires: 2_000 }, 1_000)).toBe(false)
  })

  it("treats the exact deadline as expired", () => {
    expect(isExpired({ ...creds, expires: 1_000 }, 1_000)).toBe(true)
  })
})
