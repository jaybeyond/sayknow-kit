/** @vitest-environment jsdom */
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest"
import { act, cleanup, renderHook, waitFor } from "@testing-library/react"

const vault = vi.hoisted(() => ({ shared: "", named: new Map<string, string>() }))

vi.mock("@/lib/secrets", () => ({
  DEEPL_ACCOUNT: "deepl_api_key",
  DEEPL_REV_KEY: "deepl:rev",
  SECRETS_REV_KEY: "apiKey:rev",
  secrets: {
    get: async () => vault.shared,
    set: async (k: string) => {
      vault.shared = k
    },
    clear: async () => {
      vault.shared = ""
    },
  },
  namedSecret: {
    get: async (a: string) => vault.named.get(a) ?? "",
    set: async (a: string, k: string) => {
      vault.named.set(a, k)
    },
    clear: async (a: string) => {
      vault.named.delete(a)
    },
  },
}))
vi.mock("@/lib/oauth/registry", () => ({ ensureAccessToken: vi.fn(), OAUTH_PROVIDER_IDS: [] }))

import { popoverOpacity, useSettings } from "./useSettings"

beforeEach(() => {
  localStorage.clear()
  vault.shared = "sk-or-shared"
  vault.named.clear()
})
afterEach(() => cleanup())

async function mount() {
  const hook = renderHook(() => useSettings())
  await waitFor(() => expect(hook.result.current.loaded).toBe(true))
  return hook
}

describe("per-provider API keys", () => {
  it("keeps the OpenRouter key when a z.ai key is entered, and swaps back", async () => {
    const { result } = await mount()
    expect(result.current.settings.apiKey).toBe("sk-or-shared")

    act(() => result.current.update({ provider: "zai", baseURL: "https://api.z.ai/api/paas/v4" }))
    expect(result.current.settings.apiKey).toBe("")
    expect(result.current.isLoggedIn).toBe(false)

    act(() => result.current.update({ apiKey: "zai-key" }))
    await waitFor(() => expect(vault.named.get("zai_api_key")).toBe("zai-key"))
    expect(result.current.settings.apiKey).toBe("zai-key")
    expect(result.current.isLoggedIn).toBe(true)
    expect(vault.shared).toBe("sk-or-shared")

    act(() => result.current.update({ provider: "openrouter", baseURL: "https://openrouter.ai/api/v1" }))
    expect(result.current.settings.apiKey).toBe("sk-or-shared")
  })

  it("stores a key sent together with the provider under that provider", async () => {
    const { result } = await mount()
    act(() =>
      result.current.update({ provider: "nvidia", baseURL: "https://integrate.api.nvidia.com/v1", apiKey: "nvapi-1" }),
    )
    await waitFor(() => expect(vault.named.get("nvidia_api_key")).toBe("nvapi-1"))
    expect(vault.shared).toBe("sk-or-shared")
  })

  it("loads saved provider keys on start", async () => {
    vault.named.set("nvidia_api_key", "nvapi-saved")
    localStorage.setItem("sayknow:prefs", JSON.stringify({ provider: "nvidia", baseURL: "https://integrate.api.nvidia.com/v1" }))
    const { result } = await mount()
    await waitFor(() => expect(result.current.settings.apiKey).toBe("nvapi-saved"))
    expect(result.current.isLoggedIn).toBe(true)
  })

  it("signing out of z.ai forgets only the z.ai key", async () => {
    vault.named.set("zai_api_key", "zai-key")
    localStorage.setItem("sayknow:prefs", JSON.stringify({ provider: "zai", baseURL: "https://api.z.ai/api/paas/v4" }))
    const { result } = await mount()
    await waitFor(() => expect(result.current.settings.apiKey).toBe("zai-key"))
    await act(() => result.current.clearKey())
    expect(vault.named.has("zai_api_key")).toBe(false)
    expect(vault.shared).toBe("sk-or-shared")
    expect(result.current.settings.provider).toBe("openrouter")
    expect(result.current.settings.apiKey).toBe("sk-or-shared")
  })
})

describe("popover opacity", () => {
  it("defaults prefs saved by older builds", async () => {
    localStorage.setItem("sayknow:prefs", JSON.stringify({ windowMode: "compact" }))
    const { result } = await mount()
    expect(result.current.settings.popoverOpacity).toBe(95)
  })

  it.each([
    [undefined, 95],
    ["80", 95],
    [Number.NaN, 95],
    [10, 50],
    [140, 100],
    [72.4, 72],
  ])("renders %s as %s%%", (stored, shown) => {
    expect(popoverOpacity(stored)).toBe(shown)
  })
})
