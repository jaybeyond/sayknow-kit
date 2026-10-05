import { afterEach, describe, expect, it, vi } from "vitest"

const invoke = vi.hoisted(() => vi.fn<(...args: unknown[]) => Promise<unknown>>(async () => []))

vi.mock("./runtime", () => ({ isTauri: () => true }))
vi.mock("@tauri-apps/api/core", () => ({ invoke }))

import { getSnapshot, scanDisplays, subscribe, syncBuiltin, type DisplayRow } from "./tools-store"

afterEach(() => {
  vi.clearAllMocks()
  invoke.mockReset()
  invoke.mockResolvedValue([])
})

describe("display scanning", () => {
  it("lets the backend reuse a recent scan unless the user forced a refresh", async () => {
    // A DDC read costs tens to hundreds of ms per monitor, so opening the
    // panel must not demand a fresh round trip.
    await scanDisplays()
    expect(invoke).toHaveBeenCalledWith("list_displays", { force: false })

    invoke.mockClear()
    await scanDisplays(true)
    expect(invoke).toHaveBeenCalledWith("list_displays", { force: true })
  })
})

const builtin: DisplayRow = { id: "builtin", name: "Built-in", kind: "builtin", is_main: true, brightness: 50, power: null, power_capable: false, controllable: true, method: "backlight", system_level: 0.5 }
const external: DisplayRow = { ...builtin, id: "external", name: "External", kind: "external", is_main: false, method: "ddc", system_level: null }

describe("builtin brightness notifications", () => {
  it("preserves snapshot and row identities without notifying on unchanged values", async () => {
    invoke.mockResolvedValueOnce([builtin, external])
    await scanDisplays()
    const before = getSnapshot()
    const listener = vi.fn()
    const stop = subscribe(listener)
    try {
      invoke.mockResolvedValue({ brightness: 50, system_level: 0.5 })
      for (let i = 0; i < 20; i++) await syncBuiltin()
      expect(listener).not.toHaveBeenCalled()
      expect(getSnapshot()).toBe(before)
      expect(getSnapshot().displays).toBe(before.displays)
    } finally { stop() }
  })

  it("notifies on a changed native reading and leaves external rows untouched", async () => {
    invoke.mockResolvedValueOnce([builtin, external])
    await scanDisplays()
    const before = getSnapshot()
    const listener = vi.fn()
    const stop = subscribe(listener)
    try {
      invoke.mockResolvedValueOnce({ brightness: 60, system_level: 0.6 })
      await syncBuiltin()
      expect(listener).toHaveBeenCalledOnce()
      expect(getSnapshot().displays[0]).toEqual({ ...builtin, brightness: 60, system_level: 0.6 })
      expect(getSnapshot().displays[1]).toBe(before.displays[1])
    } finally { stop() }
  })

  it("does not invoke builtin synchronization when no built-in row exists", async () => {
    invoke.mockResolvedValueOnce([external])
    await scanDisplays()
    invoke.mockClear()
    const before = getSnapshot()
    await syncBuiltin()
    expect(invoke).not.toHaveBeenCalled()
    expect(getSnapshot()).toBe(before)
  })

  it("retains the last value without notifying when the native read fails", async () => {
    invoke.mockResolvedValueOnce([builtin])
    await scanDisplays()
    const before = getSnapshot()
    const listener = vi.fn()
    const stop = subscribe(listener)
    try {
      invoke.mockRejectedValueOnce(new Error("native read failed"))
      await syncBuiltin()
      expect(getSnapshot()).toBe(before)
      expect(listener).not.toHaveBeenCalled()
    } finally { stop() }
  })
})
