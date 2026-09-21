import { describe, expect, it } from "vitest"
import { OAUTH_MODELS } from "./models"
import { OAUTH_PROVIDER_IDS } from "./registry"

/**
 * Model ids are hand-copied from sayknow-cli's catalogue, so they rot: a
 * retired id keeps the picker looking fine and only fails at request time,
 * where the provider reports it as a generic 400. `gpt-5.2-codex` did exactly
 * that ("not supported when using Codex with a ChatGPT account").
 *
 * These checks cannot reach the live catalogue, so they pin the shape and the
 * generation instead — enough to catch a stale or malformed entry.
 */
describe("model ids", () => {
  it.each(OAUTH_PROVIDER_IDS)("%s entries are well formed", (provider) => {
    for (const model of OAUTH_MODELS[provider]) {
      expect(model.id.trim()).toBe(model.id)
      expect(model.id).not.toBe("")
      expect(model.name?.trim()).toBeTruthy()
      // An aggregator-style `vendor/model` id belongs to OpenRouter and is
      // rejected by these providers' own APIs.
      expect(model.id).not.toContain("/")
    }
  })

  it("leads with a current generation for each supported provider", () => {
    // Guards against the list silently drifting back to an older line.
    expect(OAUTH_MODELS.anthropic[0]?.id).toMatch(/^claude-(opus|sonnet)-5/)
    expect(OAUTH_MODELS.xai[0]?.id).toMatch(/^grok-4\./)
    expect(OAUTH_MODELS["openai-codex"][0]?.id).toMatch(/^gpt-(6|5\.6)/)
    expect(OAUTH_MODELS["google-gemini-cli"][0]?.id).toMatch(/^gemini-3/)
  })

  it("keeps Codex off the -codex suffixed ids a ChatGPT account rejects", () => {
    for (const model of OAUTH_MODELS["openai-codex"]) {
      expect(model.id).not.toMatch(/-codex(-|$)/)
    }
  })

  it("offers a usable fallback for cursor until the account answers", () => {
    expect(OAUTH_MODELS.cursor.length).toBeGreaterThan(0)
    for (const model of OAUTH_MODELS.cursor) {
      expect(model.id.trim()).not.toBe("")
      expect(model.name.trim()).not.toBe("")
    }
  })
})
