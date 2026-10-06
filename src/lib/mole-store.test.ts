import { afterEach, beforeEach, describe, expect, it, vi } from "vitest"
import type { AppInventory, AppRemovalOutcome, AppRemovalPreview, MoleRun } from "./mole"
const mocks = vi.hoisted(() => ({
  listen: vi.fn(), unlisten: vi.fn(), detect: vi.fn(), run: vi.fn(), list: vi.fn(),
  preview: vi.fn(), cancel: vi.fn(), trash: vi.fn(), release: vi.fn(),
}))
vi.mock("@tauri-apps/api/event", () => ({ listen: mocks.listen }))
vi.mock("./idle-reload", () => ({ holdReload: () => mocks.release }))
vi.mock("./mole", async (original) => ({
  ...await original<typeof import("./mole")>(), detectMole: mocks.detect, runMoleAction: mocks.run,
  listMoleApps: mocks.list, previewMoleAppRemoval: mocks.preview, cancelMoleAppRemoval: mocks.cancel,
  trashMoleAppSelection: mocks.trash,
}))
const inventory: AppInventory = { generation: "g1", apps: [
  { id: "a1", name: "Sample", bundle_id: "org.test.sample", path: "/Applications/Sample.app", size_label: "1MB", source: "App", blocked_reason: null },
] }
const preview = (): AppRemovalPreview => ({
  token: "p1", generation: "g1", expires_at_ms: Date.now() + 60_000,
  app: { id: "bundle", kind: "app", path: inventory.apps[0].path, size_bytes: 1000 },
  related: [
    { id: "data-a", kind: "cache", path: "/Users/test/Library/Caches/org.test.sample", size_bytes: 100 },
    { id: "data-b", kind: "preferences", path: "/Users/test/Library/Preferences/org.test.sample.plist", size_bytes: 50 },
  ], excluded: [],
})
const success = (action: string): MoleRun => ({
  ok: true, command: "mo", stderr: "",
  stdout: action === "clean-preview" ? "➤ User essentials\n→ User app cache 2 items, 2MB dry\nPotential space: 2MB | Items: 2"
    : action === "optimize-preview" ? "➤ Dock Refresh\n→ Dock refreshed"
      : action === "optimize" ? "➤ Dock Refresh\n✓ Dock refreshed"
        : "Space freed: 1MB | Items cleaned: 1",
  json: action === "analyze" ? { entries: [{ name: "Home", path: "/Users/test", size: 1000 }] } : null,
})
function deferred<T>() {
  let resolve!: (value: T) => void
  let reject!: (error: unknown) => void
  const promise = new Promise<T>((yes, no) => { resolve = yes; reject = no })
  return { promise, resolve, reject }
}
async function loadStore() { vi.resetModules(); return import("./mole-store") }
beforeEach(() => {
  vi.resetAllMocks()
  mocks.listen.mockResolvedValue(mocks.unlisten)
  mocks.detect.mockResolvedValue({ path: "/opt/homebrew/bin/mo", version: "1.38.1", supported: true, required_version: "1.38.1" })
  mocks.run.mockImplementation(async (action: string) => success(action))
  mocks.list.mockResolvedValue(inventory)
  mocks.preview.mockImplementation(async () => preview())
  mocks.cancel.mockResolvedValue(undefined)
  mocks.trash.mockResolvedValue({ items: [], stopped_reason: null })
})
afterEach(() => vi.useRealTimers())

describe("automatic read-only scan lifecycle", () => {
  it("coalesces first activation and refresh-before-first-scan, then keeps lists on re-entry", async () => {
    const store = await loadStore()
    const first = store.refreshScans()
    expect(store.initialize()).toBe(first)
    expect(store.refreshScans()).toBe(first)
    await first
    expect(mocks.run.mock.calls.flat()).toEqual(["analyze", "clean-preview", "optimize-preview"])
    expect(mocks.list).toHaveBeenCalledOnce()
    expect(mocks.trash).not.toHaveBeenCalled()
    await store.initialize()
    expect(mocks.run).toHaveBeenCalledTimes(3)
    expect(store.getSnapshot().sessions.cache.items[0].name).toBe("User app cache")
    expect(store.getSnapshot().apps.inventory).toEqual(inventory)
    expect(store.getSnapshot().busy).toBeNull()
  })
  it.each([null, { version: "9", supported: false, required_version: "1.38.1" }])("does not invoke unsupported/missing Mole (%s)", async (info) => {
    mocks.detect.mockResolvedValue(info)
    const store = await loadStore()
    await store.initialize()
    expect(mocks.run).not.toHaveBeenCalled()
    expect(mocks.list).not.toHaveBeenCalled()
    expect(store.getSnapshot().busy).toBeNull()
  })
  it("retains completed rows while refresh progresses and after one independent scan fails", async () => {
    const store = await loadStore()
    await store.initialize()
    const old = store.getSnapshot().sessions.cache.items
    const pending = deferred<MoleRun>()
    const callbacks: Array<(event: { payload: string }) => void> = []
    mocks.listen.mockImplementation(async (_event, callback: (event: { payload: string }) => void) => { callbacks.push(callback); return mocks.unlisten })
    mocks.run.mockImplementation(async (action: string) => action === "clean-preview" ? pending.promise : success(action))
    const refresh = store.refreshScans()
    await vi.waitFor(() => expect(callbacks).toHaveLength(2))
    callbacks[1]({ payload: "→ Half-read new cache 10GB" })
    expect(store.getSnapshot().sessions.cache.items).toBe(old)
    pending.reject(new Error("scan failed"))
    await refresh
    expect(store.getSnapshot().sessions.cache.items).toBe(old)
    expect(store.getSnapshot().sessions.cache.stale).toBe(true)
    expect(store.getSnapshot().sessions.cache.error).toContain("scan failed")
    expect(store.getSnapshot().sessions.tune.stale).toBe(false)
    expect(store.getSnapshot().apps.stale).toBe(false)
    await expect(store.run("cache", "clean")).rejects.toThrow("mole_scan_required")
    callbacks[1]({ payload: "late callback" })
    expect(store.getSnapshot().sessions.cache.progress).toEqual([])
  })
  it.each(["", " \n\t", "unrecognized output", "Potential space: unknown", "➤ App caches\n• arbitrary note, 9GB"])("retains the prior cache scan and refuses clean after invalid successful output (%j)", async (stdout) => {
    const store = await loadStore()
    await store.initialize()
    const old = store.getSnapshot().sessions.cache
    mocks.run.mockImplementation(async (action: string) => action === "clean-preview" ? { ...success(action), stdout } : success(action))
    await store.refreshScans()
    const current = store.getSnapshot().sessions.cache
    expect(current.items).toBe(old.items)
    expect(current.scanResult).toBe(old.scanResult)
    expect(current.updatedAt).toBe(old.updatedAt)
    expect(current.stale).toBe(true)
    expect(current.error).toContain("mole_inventory_invalid")
    expect(store.getSnapshot().sessions.tune.stale).toBe(false)
    expect(store.getSnapshot().apps.stale).toBe(false)
    mocks.run.mockClear()
    await expect(store.run("cache", "clean")).rejects.toThrow("mole_scan_required")
    expect(mocks.run).not.toHaveBeenCalled()
  })
  it("does not authorize clean when the first cache scan is blank", async () => {
    mocks.run.mockImplementation(async (action: string) => action === "clean-preview" ? { ...success(action), stdout: "" } : success(action))
    const store = await loadStore()
    await store.initialize()
    expect(store.getSnapshot().sessions.cache).toMatchObject({
      items: [], scanResult: null, updatedAt: null, stale: true, error: expect.stringContaining("mole_inventory_invalid"),
    })
    mocks.run.mockClear()
    await expect(store.run("cache", "clean")).rejects.toThrow("mole_scan_required")
    expect(mocks.run).not.toHaveBeenCalled()
  })
  it("accepts an explicit zero preview and restores validity after an invalid scan", async () => {
    const store = await loadStore()
    await store.initialize()
    mocks.run.mockResolvedValueOnce({ ...success("clean-preview"), stdout: "" })
    await store.run("cache", "clean-preview")
    expect(store.getSnapshot().sessions.cache.stale).toBe(true)
    mocks.run.mockResolvedValueOnce({ ...success("clean-preview"), stdout: "Potential space: 0B | Items: 0 | Categories: 0" })
    await store.run("cache", "clean-preview")
    expect(store.getSnapshot().sessions.cache).toMatchObject({
      items: [], scanResult: { mode: "preview", bytes: 0, items: 0, partial: false }, stale: false, error: null,
    })
    mocks.run.mockClear()
    await store.run("cache", "clean")
    expect(mocks.run).toHaveBeenCalledExactlyOnceWith("clean")
  })
  it("accepts recognizable preview rows with unknown sizes as a valid scan", async () => {
    const store = await loadStore()
    await store.initialize()
    mocks.run.mockResolvedValueOnce({ ...success("clean-preview"), stdout: "➤ Developer tools\n→ npm cache · would clean" })
    await store.run("cache", "clean-preview")
    const current = store.getSnapshot().sessions.cache
    expect(current.items).toEqual([expect.objectContaining({ name: "npm cache", status: "candidate", bytes: null })])
    expect(current.scanResult).toEqual({ mode: "preview", bytes: null, items: null, partial: true })
    expect(current.stale).toBe(false)
    expect(current.error).toBeNull()
    mocks.run.mockClear()
    await store.run("cache", "clean")
    expect(mocks.run).toHaveBeenCalledExactlyOnceWith("clean")
  })
  it("treats unsuccessful or malformed analyze output as failure without discarding the old inventory", async () => {
    const store = await loadStore()
    await store.initialize()
    const old = store.getSnapshot().sessions.disk.analyze
    mocks.run.mockImplementation(async (action: string) => action === "analyze" ? { ...success(action), json: { wrong: true } } : success(action))
    await store.refreshScans()
    expect(store.getSnapshot().sessions.disk.analyze).toBe(old)
    expect(store.getSnapshot().sessions.disk.error).toContain("mole_inventory_invalid")
    expect(store.getSnapshot().apps.stale).toBe(false)
    mocks.run.mockResolvedValueOnce({ ...success("analyze"), ok: false, stderr: "failed", json: { entries: [] } })
    await store.run("disk", "analyze")
    expect(store.getSnapshot().sessions.disk.analyze).toBe(old)
  })
  it("unlocks after detection failure and permits explicit retry", async () => {
    mocks.detect.mockRejectedValueOnce(new Error("detect failed"))
    const store = await loadStore()
    await store.initialize()
    expect(store.getSnapshot().detectionError).toContain("detect failed")
    await store.refreshScans()
    expect(store.getSnapshot().detectionError).toBeNull()
    expect(store.getSnapshot().apps.inventory).toEqual(inventory)
    expect(mocks.release).toHaveBeenCalledTimes(2)
  })
})

describe("destructive admission and preview authorization", () => {
  it("rejects, rather than queues or coalesces, repeated writes while listener setup is pending", async () => {
    const store = await loadStore()
    await store.initialize()
    mocks.run.mockClear()
    const listener = deferred<() => void>()
    mocks.listen.mockReturnValueOnce(listener.promise)
    const first = store.run("cache", "clean")
    await expect(store.run("tune", "optimize")).rejects.toThrow("mole_busy")
    await expect(store.refreshScans()).rejects.toThrow("mole_busy")
    expect(mocks.run).not.toHaveBeenCalled()
    listener.resolve(mocks.unlisten)
    await first
    expect(mocks.run.mock.calls.flat()).toEqual(["clean"])
    expect(store.getSnapshot().sessions.cache.result?.bytes).toBe(1_000_000)
    expect(store.getSnapshot().sessions.cache.stale).toBe(true)
    await store.refreshScans()
    expect(store.getSnapshot().sessions.cache.result?.bytes).toBe(1_000_000)
    expect(store.getSnapshot().sessions.cache.scanResult?.bytes).toBe(2_000_000)
  })
  it("continues without line events but records invocation errors and releases admission", async () => {
    const store = await loadStore()
    await store.initialize()
    mocks.listen.mockRejectedValueOnce(new Error("no event bus"))
    mocks.run.mockRejectedValueOnce(new Error("spawn failed"))
    await store.run("disk", "analyze")
    expect(store.getSnapshot().sessions.disk.error).toContain("spawn failed")
    expect(store.getSnapshot().busy).toBeNull()
    await store.run("disk", "analyze")
    expect(store.getSnapshot().sessions.disk.error).toBeNull()
  })
  it("invalidates a pending preview on cancellation and discards the late token without a write", async () => {
    const store = await loadStore()
    await store.initialize()
    const pending = deferred<AppRemovalPreview>()
    mocks.preview.mockReturnValueOnce(pending.promise)
    const opening = store.openAppRemoval(inventory.apps[0])
    await vi.waitFor(() => expect(mocks.preview).toHaveBeenCalledOnce())
    await store.cancelRemoval()
    pending.resolve(preview())
    await opening
    expect(store.getSnapshot().preview).toBeNull()
    expect(store.getSnapshot().selectedApp).toBeNull()
    expect(mocks.cancel).toHaveBeenCalledWith("p1")
    expect(mocks.trash).not.toHaveBeenCalled()
  })
  it("revokes preview rights at refresh admission even when the new app inventory fails", async () => {
    const store = await loadStore()
    await store.initialize()
    await store.openAppRemoval(inventory.apps[0])
    mocks.list.mockRejectedValueOnce(new Error("inventory failed"))
    const refresh = store.refreshScans()
    expect(store.getSnapshot().preview).toBeNull()
    expect(store.getSnapshot().apps.stale).toBe(true)
    await refresh
    expect(mocks.cancel).toHaveBeenCalledWith("p1")
    expect(store.getSnapshot().apps.inventory).toEqual(inventory)
    await expect(store.openAppRemoval(inventory.apps[0])).rejects.toThrow("mole_inventory_stale")
    await expect(store.removeSelected([])).rejects.toThrow("mole_preview_stale")
    expect(mocks.trash).not.toHaveBeenCalled()
  })
  it.each([{ ids: ["unknown"] }, { ids: ["data-a", "data-a"] }, { ids: ["bundle"] }])("rejects invalid related-id subset $ids before invoking native writes", async ({ ids }) => {
    const store = await loadStore()
    await store.initialize()
    await store.openAppRemoval(inventory.apps[0])
    await expect(store.removeSelected(ids)).rejects.toThrow("mole_invalid_selection")
    expect(mocks.trash).not.toHaveBeenCalled()
  })
  it("rejects an expired preview without silently refreshing authorization", async () => {
    mocks.preview.mockResolvedValueOnce({ ...preview(), expires_at_ms: Date.now() - 1 })
    const store = await loadStore()
    await store.initialize()
    await store.openAppRemoval(inventory.apps[0])
    await expect(store.removeSelected([])).rejects.toThrow("mole_preview_stale")
    expect(mocks.preview).toHaveBeenCalledOnce()
    expect(mocks.trash).not.toHaveBeenCalled()
  })
  it("submits the exact subset once, keeps partial outcomes through failed read-only reconciliation, and never retries", async () => {
    const store = await loadStore()
    await store.initialize()
    await store.openAppRemoval(inventory.apps[0])
    const pending = deferred<AppRemovalOutcome>()
    mocks.trash.mockReturnValueOnce(pending.promise)
    mocks.list.mockRejectedValueOnce(new Error("post-write scan failed"))
    const removing = store.removeSelected(["data-b"])
    await expect(store.removeSelected(["data-b"])).rejects.toThrow("mole_busy")
    await expect(store.cancelRemoval()).rejects.toThrow("mole_busy")
    const outcome: AppRemovalOutcome = { items: [
      { ...preview().app, candidate_id: "bundle", status: "moved", error: null, trash_path: "/Users/test/.Trash/Sample.app" },
      { ...preview().related[1], candidate_id: "data-b", status: "unknown", error: "mole_result_unknown", trash_path: null },
    ], stopped_reason: "mole_result_unknown" }
    pending.resolve(outcome)
    await removing
    expect(mocks.trash).toHaveBeenCalledExactlyOnceWith("p1", ["data-b"])
    expect(store.getSnapshot().removalResult).toEqual(outcome)
    expect(store.getSnapshot().apps.stale).toBe(true)
    expect(store.getSnapshot().preview).toBeNull()
    expect(store.getSnapshot().busy).toBeNull()
    await store.refreshScans()
    expect(store.getSnapshot().removalResult).toEqual(outcome)
    expect(mocks.trash).toHaveBeenCalledOnce()
  })
})
