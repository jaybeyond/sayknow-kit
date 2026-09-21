import { describe, expect, it } from "vitest"
import { defaultOAuthModel, OAUTH_MODELS } from "./models"
import { OAUTH_PROVIDER_IDS } from "./registry"

describe("OAUTH_MODELS", () => {
  it("covers every registered provider", () => {
    for (const id of OAUTH_PROVIDER_IDS) {
      expect(OAUTH_MODELS[id]).toBeDefined()
    }
  })

  it("offers models only for providers whose request path exists", () => {
    // Listing a model for a provider we refuse to call would promise a
    // request that always throws OAuthUnsupportedError.
    expect(OAUTH_MODELS.anthropic.length).toBeGreaterThan(0)
    expect(OAUTH_MODELS.xai.length).toBeGreaterThan(0)
    expect(OAUTH_MODELS["openai-codex"].length).toBeGreaterThan(0)
    expect(OAUTH_MODELS["google-gemini-cli"].length).toBeGreaterThan(0)
    // Cursor ships a static fallback so the picker is never empty before the
    // account's own GetUsableModels answers.
    expect(OAUTH_MODELS.cursor.length).toBeGreaterThan(0)
  })

  it("lists ids for the providers that can actually answer", () => {
    // These are the ids sayknow-cli's model profiles actually run on these
    // accounts. A subscription rejects the API-key-only entries that
    // models.json also lists, which is how `gpt-5.2-codex` produced
    // `400 ... not supported when using Codex with a ChatGPT account`.
    expect(OAUTH_MODELS["openai-codex"].map((m) => m.id)).toContain("gpt-5.6-terra")
    expect(OAUTH_MODELS["google-gemini-cli"].map((m) => m.id)).toContain("gemini-3-pro-preview")
    expect(OAUTH_MODELS.anthropic.map((m) => m.id)).toContain("claude-opus-5")
    expect(OAUTH_MODELS.xai.map((m) => m.id)).toContain("grok-4.6")
  })

  it("uses each provider's own id format, not an aggregator's", () => {
    // `openai/gpt-4o-mini` style ids belong to OpenRouter; sending one to
    // api.anthropic.com is what made chat fail after signing in.
    for (const model of OAUTH_MODELS.anthropic) {
      expect(model.id).toMatch(/^claude-/)
      expect(model.id).not.toContain("/")
    }
    for (const model of OAUTH_MODELS.xai) {
      expect(model.id).toMatch(/^grok-/)
      expect(model.id).not.toContain("/")
    }
  })

  it("has no duplicate ids within a provider", () => {
    for (const id of OAUTH_PROVIDER_IDS) {
      const ids = OAUTH_MODELS[id].map((m) => m.id)
      expect(new Set(ids).size).toBe(ids.length)
    }
  })
})

describe("defaultOAuthModel", () => {
  it("returns a listed model for supported providers", () => {
    expect(OAUTH_MODELS.anthropic.map((m) => m.id)).toContain(defaultOAuthModel("anthropic"))
    expect(OAUTH_MODELS.xai.map((m) => m.id)).toContain(defaultOAuthModel("xai"))
  })

  it("defaults cursor to a model from its own list", () => {
    expect(OAUTH_MODELS.cursor.map((m) => m.id)).toContain(defaultOAuthModel("cursor"))
  })
})
