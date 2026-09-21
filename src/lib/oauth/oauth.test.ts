import { describe, expect, it, vi } from "vitest"
import { decodeBase64Url, decodeJwt } from "./base64"
import { parseCallbackInput } from "./callback-server"
import { delay } from "./delay"
import { generatePKCE } from "./pkce"

describe("decodeBase64Url", () => {
  it("decodes both alphabets, padded or not", () => {
    // "~~~?" exercises the two characters that differ between base64 and
    // base64url, which is exactly what `atob` alone gets wrong.
    const bytes = new Uint8Array([0xfb, 0xef, 0xbe, 0x3f])
    const standard = btoa(String.fromCharCode(...bytes))
    const urlSafe = standard.replace(/\+/g, "-").replace(/\//g, "_").replace(/=+$/, "")
    expect(decodeBase64Url(urlSafe)).toBe(decodeBase64Url(standard))
  })

  it("round-trips utf8 beyond ascii", () => {
    const text = "안녕 Grok · café"
    const encoded = btoa(String.fromCharCode(...new TextEncoder().encode(text)))
    expect(decodeBase64Url(encoded)).toBe(text)
  })
})

describe("decodeJwt", () => {
  it("reads the payload segment", () => {
    const payload = { sub: "user-1", email: "A@Example.com" }
    const segment = btoa(JSON.stringify(payload))
      .replace(/\+/g, "-")
      .replace(/\//g, "_")
      .replace(/=+$/, "")
    expect(decodeJwt(`header.${segment}.signature`)).toEqual(payload)
  })

  it("returns null instead of throwing on malformed input", () => {
    expect(decodeJwt("not-a-jwt")).toBeNull()
    expect(decodeJwt("a.b.c")).toBeNull()
    expect(decodeJwt("")).toBeNull()
  })
})

describe("delay", () => {
  it("resolves after the interval", async () => {
    vi.useFakeTimers()
    try {
      let done = false
      const pending = delay(5000).then(() => {
        done = true
      })
      expect(done).toBe(false)
      await vi.advanceTimersByTimeAsync(5000)
      await pending
      expect(done).toBe(true)
    } finally {
      vi.useRealTimers()
    }
  })

  it("wakes immediately when the signal aborts, rather than waiting it out", async () => {
    const controller = new AbortController()
    const pending = delay(60_000, controller.signal)
    controller.abort()
    await expect(pending).resolves.toBeUndefined()
  })

  it("returns at once for an already-aborted signal", async () => {
    await expect(delay(60_000, AbortSignal.abort())).resolves.toBeUndefined()
  })
})

describe("generatePKCE", () => {
  it("produces a base64url verifier with no padding or unsafe characters", async () => {
    const { verifier } = await generatePKCE()
    // 96 random bytes base64-encode to 128 characters, and base64url keeps
    // that length because only the alphabet changes.
    expect(verifier).toHaveLength(128)
    expect(verifier).toMatch(/^[A-Za-z0-9_-]+$/)
  })

  it("derives the challenge as the SHA-256 of the verifier", async () => {
    const { verifier, challenge } = await generatePKCE()
    const digest = await crypto.subtle.digest("SHA-256", new TextEncoder().encode(verifier))
    const expected = btoa(String.fromCharCode(...new Uint8Array(digest)))
      .replace(/\+/g, "-")
      .replace(/\//g, "_")
      .replace(/=+$/, "")
    expect(challenge).toBe(expected)
  })

  it("does not repeat a verifier", async () => {
    const [first, second] = await Promise.all([generatePKCE(), generatePKCE()])
    expect(first.verifier).not.toBe(second.verifier)
  })
})

describe("parseCallbackInput", () => {
  it("reads code and state out of a pasted redirect URL", () => {
    expect(parseCallbackInput("http://127.0.0.1:54545/callback?code=abc&state=xyz")).toEqual({
      code: "abc",
      state: "xyz",
    })
  })

  it("reads a bare query fragment", () => {
    expect(parseCallbackInput("?code=abc&state=xyz")).toEqual({ code: "abc", state: "xyz" })
  })

  it("treats a bare value as the code, splitting state off the fragment", () => {
    expect(parseCallbackInput("abc#xyz")).toEqual({ code: "abc", state: "xyz" })
    expect(parseCallbackInput("abc")).toEqual({ code: "abc", state: undefined })
  })

  it("returns nothing for blank input", () => {
    expect(parseCallbackInput("   ")).toEqual({})
  })

  it("reports a missing code rather than inventing one", () => {
    expect(parseCallbackInput("http://127.0.0.1:54545/callback?state=xyz").code).toBeUndefined()
  })
})
