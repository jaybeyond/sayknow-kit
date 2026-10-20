/** @vitest-environment jsdom */
// A relaunch (or the idle reload that frees memory) must open on the app for
// a signed-in user. These pin when `loaded` turns true and what `isLoggedIn`
// says at that moment: the login screen is rendered for `loaded && !isLoggedIn`.
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest"
import { act, cleanup, renderHook, waitFor } from "@testing-library/react"

type Deferred<T> = { promise: Promise<T>; resolve: (value: T) => void }
function deferred<T>(): Deferred<T> {
  let resolve!: (value: T) => void
  const promise = new Promise<T>((r) => (resolve = r))
  return { promise, resolve }
}

const io = vi.hoisted(() => ({
  shared: null as Deferred<string> | null,
  named: null as Deferred<string> | null,
  stored: null as Deferred<unknown> | null,
  renewal: null as Deferred<unknown> | null,
}))

vi.mock("@/lib/secrets", () => ({
  DEEPL_ACCOUNT: "deepl_api_key",
  DEEPL_REV_KEY: "deepl:rev",
  SECRETS_REV_KEY: "apiKey:rev",
  secrets: { get: () => io.shared!.promise, set: async () => {}, clear: async () => {} },
  namedSecret: { get: () => io.named!.promise, set: async () => {}, clear: async () => {} },
}))
vi.mock("@/lib/oauth/registry", () => ({
  ensureAccessToken: () => io.renewal!.promise,
  OAUTH_PROVIDER_IDS: ["anthropic"],
}))
vi.mock("@/lib/oauth/store", () => ({
  OAUTH_REV_KEY: "oauth:rev",
  oauthStore: { get: () => io.stored!.promise },
}))

import { useSettings } from "./useSettings"

const record = { access: "a", refresh: "r", expires: 0 }

beforeEach(() => {
  localStorage.clear()
  io.shared = deferred()
  io.named = deferred()
  io.stored = deferred()
  io.renewal = deferred()
})
afterEach(() => cleanup())

function relaunchAs(provider: string, baseURL = "") {
  localStorage.setItem("sayknow:prefs", JSON.stringify({ provider, baseURL }))
  return renderHook(() => useSettings())
}

describe("relaunch of a key-based provider", () => {
  it("waits for that provider's own key before deciding, then opens signed in", async () => {
    const { result } = relaunchAs("zai", "https://api.z.ai/api/paas/v4")
    await act(async () => io.shared!.resolve(""))
    // The shared key alone used to settle `loaded` here, with z.ai's key not
    // read yet: a signed-in user got the login screen.
    expect(result.current.loaded).toBe(false)

    await act(async () => io.named!.resolve("zai-key"))
    expect(result.current.loaded).toBe(true)
    expect(result.current.isLoggedIn).toBe(true)
  })
})

describe("relaunch of an OAuth provider", () => {
  async function keysRead() {
    await act(async () => {
      io.shared!.resolve("")
      io.named!.resolve("")
    })
  }

  it("opens signed in from the stored record, without waiting for the renewal", async () => {
    const { result } = relaunchAs("oauth:anthropic")
    await keysRead()
    expect(result.current.loaded).toBe(false)

    await act(async () => io.stored!.resolve(record))
    expect(result.current.loaded).toBe(true)
    expect(result.current.isLoggedIn).toBe(true)
  })

  it("stays signed in when the renewal cannot reach the provider", async () => {
    const { result } = relaunchAs("oauth:anthropic")
    await keysRead()
    await act(async () => io.stored!.resolve(record))
    await act(async () => io.renewal!.resolve({ status: "unavailable", reason: "offline" }))
    expect(result.current.isLoggedIn).toBe(true)
  })

  it("signs out when the provider refuses the renewal", async () => {
    const { result } = relaunchAs("oauth:anthropic")
    await keysRead()
    await act(async () => io.stored!.resolve(record))
    await act(async () => io.renewal!.resolve({ status: "reauth-required", reason: "invalid_grant" }))
    expect(result.current.isLoggedIn).toBe(false)
  })

  it("lets the renewal decide when it answers before the stored record", async () => {
    const { result } = relaunchAs("oauth:anthropic")
    await keysRead()
    await act(async () => io.renewal!.resolve({ status: "reauth-required", reason: "invalid_grant" }))
    await act(async () => io.stored!.resolve(record))
    expect(result.current.isLoggedIn).toBe(false)
  })

  it("still shows the login screen to someone who never signed in", async () => {
    const { result } = relaunchAs("oauth:anthropic")
    await keysRead()
    await act(async () => io.stored!.resolve(null))
    expect(result.current.loaded).toBe(true)
    expect(result.current.isLoggedIn).toBe(false)
  })
})

describe("after the first answer", () => {
  it("never returns to the splash, even while a newly picked provider is checked", async () => {
    const { result } = relaunchAs("openrouter", "https://openrouter.ai/api/v1")
    await act(async () => {
      io.shared!.resolve("sk-or")
      io.named!.resolve("")
    })
    expect(result.current.loaded).toBe(true)

    io.stored = deferred()
    io.renewal = deferred()
    act(() => result.current.update({ provider: "oauth:anthropic" }))
    // A sign-in in progress on the login screen must not be unmounted.
    await waitFor(() => expect(result.current.settings.provider).toBe("oauth:anthropic"))
    expect(result.current.loaded).toBe(true)
  })
})
