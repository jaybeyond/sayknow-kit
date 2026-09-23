import { afterEach, describe, expect, it, vi } from "vitest"

const mocks = vi.hoisted(() => ({
  listen: vi.fn(),
  runMoleAction: vi.fn(),
  unlisten: vi.fn(),
}))

vi.mock("@tauri-apps/api/event", () => ({ listen: mocks.listen }))
vi.mock("./mole", () => ({
  detectMole: vi.fn(),
  parseAnalyze: vi.fn(() => ({ entries: [] })),
  parseCleanPreview: vi.fn((text: string) => (text ? [{ name: "cache", detail: "", bytes: 1 }] : [])),
  parseResult: vi.fn(() => null),
  runMoleAction: mocks.runMoleAction,
  stripAnsi: (text: string) => text,
}))

function deferred<T>() {
  let resolve!: (value: T) => void
  const promise = new Promise<T>((next) => {
    resolve = next
  })
  return { promise, resolve }
}

async function loadStore() {
  vi.resetModules()
  return import("./mole-store")
}

const success = { ok: true, stdout: "done", stderr: "", json: null, command: "mo" }

afterEach(() => {
  vi.clearAllMocks()
})

describe("Mole cleanup workspace admission", () => {
  it("admits one run before listener setup completes and ignores duplicate destructive clicks", async () => {
    const listenerReady = deferred<() => void>()
    mocks.listen.mockReturnValue(listenerReady.promise)
    mocks.runMoleAction.mockResolvedValue(success)
    const store = await loadStore()

    const first = store.run("cache", "clean")
    const duplicate = store.run("tune", "optimize")
    const refresh = store.refreshScans()
    expect(store.getSnapshot().busy).toBe("cache")
    expect(mocks.runMoleAction).not.toHaveBeenCalled()

    listenerReady.resolve(mocks.unlisten)
    await Promise.all([first, duplicate, refresh])

    expect(mocks.runMoleAction).toHaveBeenCalledTimes(1)
    expect(mocks.runMoleAction).toHaveBeenCalledWith("clean")
    expect(mocks.unlisten).toHaveBeenCalledOnce()
  })

  it("owns line callbacks locally, cleans them up, and recovers after an unsuccessful JSON result", async () => {
    let firstLine: ((event: { payload: string }) => void) | undefined
    let secondLine: ((event: { payload: string }) => void) | undefined
    mocks.listen
      .mockImplementationOnce(async (_event: string, callback: (event: { payload: string }) => void) => {
        firstLine = callback
        return mocks.unlisten
      })
      .mockImplementationOnce(async (_event: string, callback: (event: { payload: string }) => void) => {
        secondLine = callback
        return mocks.unlisten
      })
    mocks.runMoleAction
      .mockResolvedValueOnce(success)
      .mockResolvedValueOnce({ ok: false, stdout: "", stderr: "failed", json: { misleading: true }, command: "mo" })
    const store = await loadStore()

    await store.run("cache", "clean-preview")
    firstLine?.({ payload: "stale line" })
    expect(store.getSnapshot().sessions.cache.progress).toEqual([])

    await store.run("tune", "optimize")
    secondLine?.({ payload: "new line" })
    expect(store.getSnapshot().sessions.tune.progress).toEqual([])
    expect(store.getSnapshot().sessions.tune.error).toBe("failed")
    expect(store.getSnapshot().busy).toBeNull()
    expect(mocks.unlisten).toHaveBeenCalledTimes(2)

    mocks.runMoleAction.mockResolvedValueOnce(success)
    await store.run("disk", "analyze")
    expect(mocks.runMoleAction).toHaveBeenLastCalledWith("analyze")
  })

  it("refreshes used sessions with safe scans serially", async () => {
    mocks.listen.mockResolvedValue(mocks.unlisten)
    mocks.runMoleAction.mockResolvedValue(success)
    const store = await loadStore()
    await store.run("disk", "analyze")
    await store.run("cache", "clean")
    await store.run("tune", "optimize")
    mocks.runMoleAction.mockClear()

    const first = deferred<typeof success>()
    const second = deferred<typeof success>()
    const third = deferred<typeof success>()
    mocks.runMoleAction
      .mockReturnValueOnce(first.promise)
      .mockReturnValueOnce(second.promise)
      .mockReturnValueOnce(third.promise)

    const refresh = store.refreshScans()
    await vi.waitFor(() => expect(mocks.runMoleAction).toHaveBeenCalledWith("analyze"))
    const duplicate = store.run("cache", "clean")
    expect(duplicate).toBe(refresh)
    expect(mocks.runMoleAction).toHaveBeenCalledTimes(1)
    first.resolve(success)
    await vi.waitFor(() => expect(mocks.runMoleAction).toHaveBeenLastCalledWith("clean-preview"))
    expect(mocks.runMoleAction).toHaveBeenCalledTimes(2)
    second.resolve(success)
    await vi.waitFor(() => expect(mocks.runMoleAction).toHaveBeenLastCalledWith("optimize-preview"))
    expect(mocks.runMoleAction).toHaveBeenCalledTimes(3)
    third.resolve(success)
    await refresh
    expect(mocks.runMoleAction.mock.calls.flat()).toEqual(["analyze", "clean-preview", "optimize-preview"])
  })
  it("recovers from listener setup and invoke failures without leaving admission locked", async () => {
    mocks.listen.mockRejectedValueOnce(new Error("event bus unavailable"))
    mocks.runMoleAction.mockRejectedValueOnce(new Error("spawn failed"))
    const store = await loadStore()
    await store.run("cache", "clean-preview")
    expect(store.getSnapshot().sessions.cache.error).toContain("spawn failed")
    expect(store.getSnapshot().busy).toBeNull()
    mocks.listen.mockResolvedValueOnce(mocks.unlisten)
    mocks.runMoleAction.mockResolvedValueOnce(success)
    await store.run("cache", "clean-preview")
    expect(store.getSnapshot().sessions.cache.error).toBeNull()
    expect(mocks.unlisten).toHaveBeenCalledOnce()
  })
})
