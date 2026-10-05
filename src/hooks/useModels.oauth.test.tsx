/** @vitest-environment jsdom */
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest"
import { cleanup, renderHook, waitFor } from "@testing-library/react"

const mocks = vi.hoisted(() => ({
  ensureAccessToken: vi.fn(),
  discoverOAuthModels: vi.fn(),
  listCursorModels: vi.fn(),
}))

vi.mock("@/lib/oauth/registry", () => ({
  ensureAccessToken: mocks.ensureAccessToken,
  OAUTH_PROVIDER_IDS: ["anthropic", "openai-codex", "google-gemini-cli", "xai", "cursor"],
}))
vi.mock("@/lib/oauth/model-discovery", () => ({
  discoverOAuthModels: mocks.discoverOAuthModels,
  isDiscoverable: (p: string) => p === "anthropic" || p === "openai-codex" || p === "xai",
}))
vi.mock("@/lib/oauth/cursor-chat", () => ({ listCursorModels: mocks.listCursorModels }))

import { OAUTH_MODELS } from "@/lib/oauth/models"
import { useModels } from "./useModels"

const ready = { status: "ready", credentials: { access: "tok", refresh: "r", expires: 0 } }
const live = [{ id: "claude-next", name: "Claude Next" }]

beforeEach(() => {
  localStorage.clear()
  mocks.ensureAccessToken.mockReset().mockResolvedValue(ready)
  mocks.discoverOAuthModels.mockReset().mockResolvedValue(live)
  mocks.listCursorModels.mockReset()
})
afterEach(() => cleanup())

const mount = (provider: string) => renderHook(({ p }) => useModels("", "", p), { initialProps: { p: provider } })

describe("useModels with an OAuth provider", () => {
  it("shows the bundled seed, then the account's own list", async () => {
    const { result } = mount("oauth:anthropic")
    expect(result.current.models).toBe(OAUTH_MODELS.anthropic)
    await waitFor(() => expect(result.current.models).toEqual(live))
    expect(mocks.discoverOAuthModels).toHaveBeenCalledWith("anthropic", ready.credentials)
  })

  it("serves a fresh cached list without asking again", async () => {
    const first = mount("oauth:anthropic")
    await waitFor(() => expect(first.result.current.models).toEqual(live))
    first.unmount()

    const second = mount("oauth:anthropic")
    expect(second.result.current.models).toEqual(live)
    await Promise.resolve()
    expect(mocks.discoverOAuthModels).toHaveBeenCalledTimes(1)
  })

  it("asks again once the cached list is an hour old", async () => {
    localStorage.setItem(
      "sayknow:oauth-models:anthropic",
      JSON.stringify({ fetchedAt: Date.now() - 61 * 60 * 1000, data: [{ id: "old", name: "Old" }] }),
    )
    const { result } = mount("oauth:anthropic")
    expect(result.current.models).toEqual([{ id: "old", name: "Old" }])
    await waitFor(() => expect(result.current.models).toEqual(live))
  })

  it("keeps the bundled list when the probe or the sign-in fails", async () => {
    mocks.discoverOAuthModels.mockResolvedValue(null)
    const failed = mount("oauth:xai")
    await waitFor(() => expect(mocks.discoverOAuthModels).toHaveBeenCalled())
    expect(failed.result.current.models).toBe(OAUTH_MODELS.xai)
    expect(localStorage.getItem("sayknow:oauth-models:xai")).toBeNull()
    failed.unmount()

    mocks.ensureAccessToken.mockResolvedValue({ status: "signed-out" })
    const signedOut = mount("oauth:openai-codex")
    await waitFor(() => expect(mocks.ensureAccessToken).toHaveBeenCalledWith("openai-codex"))
    expect(signedOut.result.current.models).toBe(OAUTH_MODELS["openai-codex"])
  })

  it("never probes Gemini, which has no list endpoint here", async () => {
    const { result } = mount("oauth:google-gemini-cli")
    await Promise.resolve()
    expect(mocks.ensureAccessToken).not.toHaveBeenCalled()
    expect(result.current.models).toBe(OAUTH_MODELS["google-gemini-cli"])
  })

  it("uses Cursor's own command for Cursor", async () => {
    mocks.listCursorModels.mockResolvedValue([{ id: "composer-3", name: "Composer 3" }])
    const { result } = mount("oauth:cursor")
    await waitFor(() => expect(result.current.models).toEqual([{ id: "composer-3", name: "Composer 3" }]))
    expect(mocks.discoverOAuthModels).not.toHaveBeenCalled()
  })

  it("switches provider without showing the previous provider's list", async () => {
    const hook = mount("oauth:anthropic")
    await waitFor(() => expect(hook.result.current.models).toEqual(live))
    mocks.discoverOAuthModels.mockReturnValue(new Promise(() => {}))
    hook.rerender({ p: "oauth:xai" })
    expect(hook.result.current.models).toBe(OAUTH_MODELS.xai)
  })
})
