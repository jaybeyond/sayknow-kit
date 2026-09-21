import { describe, expect, it } from "vitest"
import { isOAuthProvider, oauthProviderRef, parseOAuthProvider } from "./openrouter"
import { OAUTH_PROVIDER_IDS } from "./oauth/registry"

describe("oauthProviderRef", () => {
  it("round-trips every registered provider", () => {
    for (const id of OAUTH_PROVIDER_IDS) {
      expect(parseOAuthProvider(oauthProviderRef(id))).toBe(id)
    }
  })
})

describe("parseOAuthProvider", () => {
  it("returns null for the endpoint providers", () => {
    expect(parseOAuthProvider("openrouter")).toBeNull()
    expect(parseOAuthProvider("ocp")).toBeNull()
    expect(parseOAuthProvider("custom")).toBeNull()
  })

  it("rejects an unknown provider even with the prefix", () => {
    // A stale pref from a future/older build must not become a live selection.
    expect(parseOAuthProvider("oauth:not-a-provider")).toBeNull()
    expect(parseOAuthProvider("oauth:")).toBeNull()
  })

  it("does not match a provider name without the prefix", () => {
    expect(parseOAuthProvider("anthropic")).toBeNull()
  })
})

describe("isOAuthProvider", () => {
  it("separates OAuth selections from endpoint ones", () => {
    expect(isOAuthProvider("oauth:anthropic")).toBe(true)
    expect(isOAuthProvider("oauth:xai")).toBe(true)
    expect(isOAuthProvider("openrouter")).toBe(false)
    expect(isOAuthProvider("oauth:bogus")).toBe(false)
  })
})
