/** @vitest-environment jsdom */
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest"

type Responder = (args: Record<string, unknown> | undefined) => unknown

const mocks = vi.hoisted(() => ({
  responders: new Map<string, Responder>(),
  renders: [] as string[][],
}))

vi.mock("@tauri-apps/api/core", () => ({
  invoke: vi.fn(async (cmd: string, args?: Record<string, unknown>) => {
    const r = mocks.responders.get(cmd)
    if (!r) throw { code: "unmocked", message: cmd }
    return r(args)
  }),
}))
vi.mock("@tauri-apps/api/event", () => ({ listen: vi.fn(async () => () => {}) }))
vi.mock("./lib/runtime", () => ({ isTauri: () => true }))
vi.mock("./index.css", () => ({}))
vi.mock("./hooks/useModels", () => ({ dropRetiredModelCache: () => {} }))
vi.mock("./App.tsx", () => ({ default: () => null }))
// What the first render would show: the conversation ids at render time.
vi.mock("react-dom/client", () => ({
  createRoot: () => ({
    render: async () => {
      const { conversations } = await import("./lib/chat-history")
      mocks.renders.push(conversations.list().map((c) => c.id))
    },
  }),
}))

function deferred<T>() {
  let resolve!: (v: T) => void
  const promise = new Promise<T>((r) => {
    resolve = r
  })
  return { promise, resolve }
}

beforeEach(() => {
  vi.resetModules()
  localStorage.clear()
  mocks.responders.clear()
  mocks.renders.length = 0
  document.body.innerHTML = '<div id="root"></div>'
})

afterEach(() => {
  vi.useRealTimers()
})

describe("main.tsx boot (E5)", () => {
  it("a native boot hydrates before the first render (never the frozen originals)", async () => {
    localStorage.setItem("sayknow:sync-native", "true")
    localStorage.setItem("sayknow:conversations", JSON.stringify([{ id: "stale", title: "t", messages: [], createdAt: 1, updatedAt: 1 }]))
    mocks.responders.set("sync_get_status", () => ({ enabled: true, migration: "native" }))
    const load = deferred<unknown>()
    mocks.responders.set("sync_chat_load", () => load.promise)
    await import("./main.tsx")
    await new Promise((r) => setTimeout(r, 10))
    expect(mocks.renders).toEqual([])
    load.resolve({ conversations: [{ id: "fresh", title: "t", messages: [], updatedAt: 2 }], history: [] })
    await vi.waitFor(() => expect(mocks.renders).toEqual([["fresh"]]))
  })

  it("renders after the bound when the boot is slow, still holding writes", async () => {
    vi.useFakeTimers()
    localStorage.setItem("sayknow:sync-native", "true")
    mocks.responders.set("sync_get_status", () => new Promise(() => {}))
    await import("./main.tsx")
    await vi.advanceTimersByTimeAsync(2999)
    expect(mocks.renders).toEqual([])
    await vi.advanceTimersByTimeAsync(1)
    expect(mocks.renders).toEqual([[]])
    const { isReady } = await import("./lib/sync-client")
    expect(isReady()).toBe(false)
  })

  it("the settings window renders without booting sync", async () => {
    window.history.replaceState(null, "", "/?window=settings")
    try {
      const { invoke } = await import("@tauri-apps/api/core")
      vi.mocked(invoke).mockClear()
      await import("./main.tsx")
      await vi.waitFor(() => expect(mocks.renders).toHaveLength(1))
      expect(invoke).not.toHaveBeenCalled()
    } finally {
      window.history.replaceState(null, "", "/")
    }
  })
})
