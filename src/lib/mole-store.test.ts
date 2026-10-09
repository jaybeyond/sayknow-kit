import { afterEach, beforeEach, describe, expect, it, vi } from "vitest"
import type { AppInventory, AppRemovalOutcome, AppRemovalPreview, MoleRun } from "./mole"
const mocks = vi.hoisted(() => ({
  listen: vi.fn(), unlisten: vi.fn(), detect: vi.fn(), run: vi.fn(), list: vi.fn(),
  preview: vi.fn(), cancel: vi.fn(), trash: vi.fn(), release: vi.fn(), fda: vi.fn(),
}))
vi.mock("@tauri-apps/api/event", () => ({ listen: mocks.listen }))
vi.mock("./idle-reload", () => ({ holdReload: () => mocks.release }))
vi.mock("./mole", async (original) => ({
  ...await original<typeof import("./mole")>(), detectMole: mocks.detect, runMoleAction: mocks.run,
  listMoleApps: mocks.list, previewMoleAppRemoval: mocks.preview, cancelMoleAppRemoval: mocks.cancel,
  trashMoleAppSelection: mocks.trash, checkFullDiskAccess: mocks.fda,
}))
const inventory: AppInventory = { generation: "g1", apps: [
  { id: "a1", name: "Sample", bundle_id: "org.test.sample", path: "/Applications/Sample.app", resolved_path: null, size_label: "1MB", source: "App", blocked_reason: null },
] }
const preview = (): AppRemovalPreview => ({
  token: "p1", generation: "g1", expires_at_ms: Date.now() + 60_000,
  app: { id: "bundle", kind: "app", path: inventory.apps[0].path, size_bytes: 1000 }, shortcut: null,
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
  mocks.fda.mockResolvedValue(true)
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
    expect(mocks.run.mock.calls.flat()).toEqual(["clean", "clean-preview"])
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
    expect(mocks.run.mock.calls.flat()).toEqual(["clean", "clean-preview"])
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

describe("line progress lifecycle", () => {
  describe.each([
    { id: "cache" as const, action: "clean", previewAction: "clean-preview" },
    { id: "tune" as const, action: "optimize", previewAction: "optimize-preview" },
  ])("$action", ({ id, action, previewAction }) => {
    // A successful clean is followed by a fresh scan; that path has its own test below.
    it.each(id === "cache" ? ["failed outcome", "rejected invocation"] : ["success", "failed outcome", "rejected invocation"])("retains real lines after %s, ignores late events, and clears them for the next invocation", async (settlement) => {
      const store = await loadStore()
      await store.initialize()
      const old = store.getSnapshot().sessions[id]
      const pending = deferred<MoleRun>()
      const callbacks: Array<(event: { payload: string }) => void> = []
      mocks.listen.mockClear()
      mocks.unlisten.mockClear()
      mocks.run.mockClear()
      mocks.listen.mockImplementation(async (_event, callback: (event: { payload: string }) => void) => { callbacks.push(callback); return mocks.unlisten })
      mocks.run.mockReturnValueOnce(pending.promise)
      const running = store.run(id, action)
      await vi.waitFor(() => expect(mocks.run).toHaveBeenCalledExactlyOnceWith(action))
      expect(mocks.listen).toHaveBeenCalledWith("mole:line", expect.any(Function))
      expect(store.getSnapshot().sessions[id].progress).toEqual([])
      callbacks[0]({ payload: "  \u001b[32mReading actual phase\u001b[0m  " })
      for (const payload of ["", " \n\t", '{"items": []}', "[", '"json fragment"', "}"]) callbacks[0]({ payload })
      expect(store.getSnapshot().sessions[id].progress).toEqual(["Reading actual phase"])
      for (let i = 0; i < 45; i++) callbacks[0]({ payload: `\u001b[32mPhase ${i}\u001b[0m` })
      const retained = Array.from({ length: 40 }, (_, i) => `Phase ${i + 5}`)
      expect(store.getSnapshot().sessions[id].progress).toEqual(retained)
      expect(store.getSnapshot().busy).toBe(id)
      expect(store.getSnapshot().sessions[id].result).toBeNull()
      expect(store.getSnapshot().sessions[id].maintenanceResult).toEqual([])
      expect(store.getSnapshot().sessions[id].lastRunAt).toBeNull()
      if (settlement === "rejected invocation") pending.reject(new Error("command rejected"))
      else pending.resolve(settlement === "success" ? success(action) : { ...success(action), ok: false, stderr: "command failed" })
      await running
      const completed = store.getSnapshot().sessions[id]
      expect(completed.progress).toEqual(retained)
      expect(completed.items).toBe(old.items)
      expect(completed.scanResult).toBe(old.scanResult)
      expect(completed.maintenance).toBe(old.maintenance)
      expect(completed.updatedAt).toBe(old.updatedAt)
      expect(completed.stale).toBe(true)
      expect(store.getSnapshot().busy).toBeNull()
      expect(mocks.unlisten).toHaveBeenCalledOnce()
      if (settlement === "success") {
        expect(completed.error).toBeNull()
        expect(completed.lastRunAt).not.toBeNull()
        if (id === "cache") expect(completed.result?.bytes).toBe(1_000_000)
        else expect(completed.maintenanceResult).toEqual([expect.objectContaining({ status: "completed" })])
      } else {
        expect(completed.error).toContain(settlement === "rejected invocation" ? "command rejected" : "command failed")
        expect(completed.result).toBeNull()
        expect(completed.maintenanceResult).toEqual([])
        expect(completed.lastRunAt).toBeNull()
      }
      callbacks[0]({ payload: "late destructive callback" })
      expect(store.getSnapshot().sessions[id].progress).toEqual(retained)
      const nextPending = deferred<MoleRun>()
      mocks.run.mockReturnValueOnce(nextPending.promise)
      const next = store.run(id, previewAction)
      await vi.waitFor(() => expect(callbacks).toHaveLength(2))
      expect(store.getSnapshot().sessions[id].progress).toEqual([])
      callbacks[0]({ payload: "old callback during next run" })
      expect(store.getSnapshot().sessions[id].progress).toEqual([])
      callbacks[1]({ payload: "Current preview phase" })
      expect(store.getSnapshot().sessions[id].progress).toEqual(["Current preview phase"])
      nextPending.resolve(success(previewAction))
      await next
      expect(store.getSnapshot().sessions[id].progress).toEqual([])
      expect(mocks.unlisten).toHaveBeenCalledTimes(2)
      callbacks[0]({ payload: "old callback after next run" })
      callbacks[1]({ payload: "late preview callback" })
      expect(store.getSnapshot().sessions[id].progress).toEqual([])
      expect(store.getSnapshot().sessions[id].result).toBe(completed.result)
      expect(store.getSnapshot().sessions[id].maintenanceResult).toBe(completed.maintenanceResult)
    })
  })
  it("re-scans after a real clean, keeps what Mole deleted, and drops the stale estimate", async () => {
    const store = await loadStore()
    await store.initialize()
    const pending = deferred<MoleRun>()
    const scan = deferred<MoleRun>()
    const callbacks: Array<(event: { payload: string }) => void> = []
    mocks.run.mockClear()
    mocks.listen.mockImplementation(async (_event, callback: (event: { payload: string }) => void) => { callbacks.push(callback); return mocks.unlisten })
    mocks.run.mockReturnValueOnce(pending.promise).mockReturnValueOnce(scan.promise)
    const running = store.run("cache", "clean")
    await vi.waitFor(() => expect(callbacks).toHaveLength(1))
    callbacks[0]({ payload: "➤ User essentials" })
    pending.resolve({ ...success("clean"), stdout: [
      "➤ User essentials", "  ✓ User app cache 64 items, 851.3MB", "  ✓ Trash · already empty", "  ✓ Whitelist: 3 core patterns active",
      "➤ Developer tools", "  ✓ Go build cache, 369.5MB", "  ✓ npm cache · skipped (whitelist)", "  ✓ Homebrew · removed 3, skipped 2 protected",
      "Space freed: 1.22GB | Items cleaned: 66 | Categories: 2",
    ].join("\n") })
    await vi.waitFor(() => expect(callbacks).toHaveLength(2))
    const between = store.getSnapshot().sessions.cache
    expect(between.scanResult).toBeNull()
    expect(between.items).toEqual([])
    // The follow-up scan reports its own phases; the clean's log moved to runLog.
    expect(between.progress).toEqual([])
    expect(between.runLog).toContain("➤ Developer tools")
    expect(store.getSnapshot()).toMatchObject({ busy: "cache", refreshing: true })
    scan.resolve({ ...success("clean-preview"), stdout: "➤ User essentials\n→ User app cache 1 items, 40KB dry\nPotential space: 40KB | Items: 1" })
    await running
    const after = store.getSnapshot().sessions.cache
    expect(mocks.run.mock.calls.flat()).toEqual(["clean", "clean-preview"])
    expect(after.result).toEqual({ mode: "clean", bytes: 1_220_000_000, items: 66, partial: false })
    expect(after.cleaned.map((row) => [row.name, row.bytes])).toEqual([["User app cache", 851_300_000], ["Go build cache", 369_500_000], ["Homebrew", null]])
    expect(after.runLog).toContain("Space freed: 1.22GB | Items cleaned: 66 | Categories: 2")
    expect(after.scanResult?.bytes).toBe(40_000)
    expect(after).toMatchObject({ stale: false, error: null, progress: [] })
    expect(store.getSnapshot()).toMatchObject({ busy: null, refreshing: false })
  })
  it("keeps a failed clean's error instead of hiding it behind a re-scan", async () => {
    const store = await loadStore()
    await store.initialize()
    mocks.run.mockClear()
    mocks.run.mockResolvedValueOnce({ ...success("clean"), ok: false, stderr: "command failed" })
    await store.run("cache", "clean")
    expect(mocks.run.mock.calls.flat()).toEqual(["clean"])
    expect(store.getSnapshot().sessions.cache.error).toContain("command failed")
    expect(store.getSnapshot().sessions.cache.scanResult?.bytes).toBe(2_000_000)
  })
  it("records whether Full Disk Access was granted during detection", async () => {
    mocks.fda.mockResolvedValueOnce(false)
    const store = await loadStore()
    await store.initialize()
    expect(store.getSnapshot().fullDiskAccess).toBe(false)
  })
  it.each([
    { id: "disk" as const, action: "analyze" },
    { id: "cache" as const, action: "clean-preview" },
    { id: "tune" as const, action: "optimize-preview" },
  ].flatMap((command) => ["success", "failed outcome", "rejected invocation"].map((settlement) => ({ ...command, settlement }))))("clears $action lines and detaches its callback after $settlement", async ({ id, action, settlement }) => {
    const store = await loadStore()
    await store.initialize()
    const pending = deferred<MoleRun>()
    const callbacks: Array<(event: { payload: string }) => void> = []
    mocks.unlisten.mockClear()
    mocks.run.mockClear()
    mocks.listen.mockImplementation(async (_event, callback: (event: { payload: string }) => void) => { callbacks.push(callback); return mocks.unlisten })
    mocks.run.mockReturnValueOnce(pending.promise)
    const running = store.run(id, action)
    await vi.waitFor(() => expect(mocks.run).toHaveBeenCalledExactlyOnceWith(action))
    callbacks[0]({ payload: "Current read-only phase" })
    expect(store.getSnapshot().sessions[id].progress).toEqual(["Current read-only phase"])
    if (settlement === "rejected invocation") pending.reject(new Error("read-only command rejected"))
    else pending.resolve(settlement === "success" ? success(action) : { ...success(action), ok: false, stderr: "read-only command failed" })
    await running
    expect(store.getSnapshot().sessions[id].error).toEqual(settlement === "success" ? null : expect.stringContaining(settlement === "rejected invocation" ? "read-only command rejected" : "read-only command failed"))
    expect(store.getSnapshot().sessions[id].progress).toEqual([])
    expect(mocks.unlisten).toHaveBeenCalledOnce()
    callbacks[0]({ payload: "late read-only callback" })
    expect(store.getSnapshot().sessions[id].progress).toEqual([])
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
    expect(mocks.run.mock.calls.flat()).toEqual(["clean", "clean-preview"])
    expect(store.getSnapshot().sessions.cache.result?.bytes).toBe(1_000_000)
    expect(store.getSnapshot().sessions.cache.stale).toBe(false)
    expect(store.getSnapshot().sessions.tune.stale).toBe(true)
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
