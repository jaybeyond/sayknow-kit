/** @vitest-environment jsdom */
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest"
import { act, cleanup, render, renderHook, screen } from "@testing-library/react"
import type { AgentReport } from "./agent-usage"

const mocks = vi.hoisted(() => ({ invoke: vi.fn(), deepl: vi.fn() }))
vi.mock("@tauri-apps/api/core", () => ({ invoke: mocks.invoke }))
vi.mock("./runtime", () => ({ isTauri: () => true }))
vi.mock("./deepl", async (original) => ({ ...(await original<typeof import("./deepl")>()), deeplUsage: mocks.deepl }))

type Store = typeof import("./agent-usage-store")

/** A fresh module per test: the store keeps its last scan time and in-flight scan at module level. */
async function loadStore(): Promise<Store> {
  vi.resetModules()
  return import("./agent-usage-store")
}

function deferred<T>() {
  let resolve!: (value: T) => void
  const promise = new Promise<T>((r) => { resolve = r })
  return { promise, resolve }
}

const report = (agent: string): AgentReport => ({ agent }) as unknown as AgentReport

beforeEach(() => {
  mocks.invoke.mockReset()
  mocks.deepl.mockReset()
})

afterEach(() => {
  cleanup()
  vi.useRealTimers()
})

describe("scanAgentUsage", () => {
  it("skips a background scan inside the refresh window but always runs a forced one", async () => {
    const store = await loadStore()
    mocks.invoke.mockResolvedValue([report("a")])
    await store.scanAgentUsage(false)
    await store.scanAgentUsage(false)
    expect(mocks.invoke).toHaveBeenCalledTimes(1)
    await store.scanAgentUsage(true)
    expect(mocks.invoke).toHaveBeenCalledTimes(2)
  })

  it("records the scan time and clears a previous error", async () => {
    const store = await loadStore()
    mocks.invoke.mockRejectedValueOnce(new Error("disk"))
    await store.scanAgentUsage(true)
    expect(store.getSnapshot().error).toContain("disk")
    expect(store.getSnapshot().scannedAt).toBeNull()
    mocks.invoke.mockResolvedValueOnce([report("b")])
    await store.scanAgentUsage(true)
    const snapshot = store.getSnapshot()
    expect(snapshot.error).toBeNull()
    expect(snapshot.agents).toEqual([report("b")])
    expect(snapshot.scannedAt).not.toBeNull()
    expect(snapshot.loading).toBe(false)
  })

  it("runs one more scan after a forced request that arrived mid-scan, and settles after it", async () => {
    const store = await loadStore()
    const first = deferred<AgentReport[]>()
    mocks.invoke.mockReturnValueOnce(first.promise).mockResolvedValueOnce([report("second")])
    const running = store.scanAgentUsage(true)
    const forced = store.scanAgentUsage(true)
    first.resolve([report("first")])
    await forced
    await running
    expect(mocks.invoke).toHaveBeenCalledTimes(2)
    expect(store.getSnapshot().agents).toEqual([report("second")])
  })

  it("does not queue another scan for a background request that arrived mid-scan", async () => {
    const store = await loadStore()
    const first = deferred<AgentReport[]>()
    mocks.invoke.mockReturnValueOnce(first.promise)
    const running = store.scanAgentUsage(true)
    const background = store.scanAgentUsage(false)
    first.resolve([report("only")])
    await Promise.all([running, background])
    expect(mocks.invoke).toHaveBeenCalledTimes(1)
  })

  it("gives up on a stalled DeepL lookup without blocking the scan or the next one", async () => {
    vi.useFakeTimers()
    const store = await loadStore()
    mocks.invoke.mockResolvedValue([report("a")])
    mocks.deepl.mockImplementation((_key: string, signal: AbortSignal) => new Promise((_, reject) => {
      signal.addEventListener("abort", () => reject(new Error("aborted")))
    }))
    const scan = store.scanAgentUsage(true, "key")
    await vi.advanceTimersByTimeAsync(store.DEEPL_TIMEOUT_MS)
    await scan
    expect(store.getSnapshot().deepl).toBeNull()
    expect(mocks.deepl.mock.calls[0][1].aborted).toBe(true)
    expect(store.getSnapshot().deeplError).not.toBeNull()
    expect(store.getSnapshot().agents).toEqual([report("a")])
    mocks.deepl.mockResolvedValueOnce({ count: 1, limit: 10 })
    await store.scanAgentUsage(true, "key")
    expect(store.getSnapshot().deepl).toEqual({ count: 1, limit: 10 })
    expect(store.getSnapshot().deeplError).toBeNull()
  })
})

describe("useAgentUsage", () => {
  it("reads the logs again each time the window is shown, even inside the refresh window", async () => {
    mocks.invoke.mockResolvedValue([])
    vi.resetModules()
    const { useAgentUsage } = await import("@/hooks/useAgentUsage")
    const hook = renderHook(({ active }) => useAgentUsage(active), { initialProps: { active: true } })
    await vi.waitFor(() => expect(mocks.invoke).toHaveBeenCalledTimes(1))
    hook.rerender({ active: false })
    hook.rerender({ active: true })
    await vi.waitFor(() => expect(mocks.invoke).toHaveBeenCalledTimes(2))
  })

  it("lets focus inside the refresh window pass without another read", async () => {
    mocks.invoke.mockResolvedValue([])
    vi.resetModules()
    const { useAgentUsage } = await import("@/hooks/useAgentUsage")
    renderHook(() => useAgentUsage(true))
    await vi.waitFor(() => expect(mocks.invoke).toHaveBeenCalledTimes(1))
    await act(async () => { window.dispatchEvent(new Event("focus")) })
    await new Promise((resolve) => setTimeout(resolve, 0))
    expect(mocks.invoke).toHaveBeenCalledTimes(1)
  })

  it("does not read while the window is hidden", async () => {
    mocks.invoke.mockResolvedValue([])
    vi.resetModules()
    const { useAgentUsage } = await import("@/hooks/useAgentUsage")
    renderHook(() => useAgentUsage(false))
    await new Promise((resolve) => setTimeout(resolve, 0))
    expect(mocks.invoke).not.toHaveBeenCalled()
  })
})

describe("UsagePanel freshness", () => {
  it("shows when the logs were last read and moves it forward when the window is shown again", async () => {
    vi.useFakeTimers({ shouldAdvanceTime: true })
    vi.setSystemTime(new Date(2026, 9, 6, 19, 5, 7))
    mocks.invoke.mockImplementation(async () => [])
    vi.resetModules()
    const { UsagePanel } = await import("@/components/UsagePanel")
    const { UI_STRINGS } = await import("@/i18n/strings")
    const settings = { uiLocale: "ko", deeplKey: "" } as never
    const view = render(<UsagePanel settings={settings} active />)
    const read = (time: string) => UI_STRINGS.ko["usage.updated"].replace("{time}", time)
    expect(await screen.findByText(read("19:05:07"))).toBeTruthy()
    view.rerender(<UsagePanel settings={settings} active={false} />)
    vi.setSystemTime(new Date(2026, 9, 6, 19, 5, 19))
    view.rerender(<UsagePanel settings={settings} active />)
    expect(await screen.findByText(read("19:05:19"))).toBeTruthy()
    expect(mocks.invoke).toHaveBeenCalledTimes(2)
  })
})
