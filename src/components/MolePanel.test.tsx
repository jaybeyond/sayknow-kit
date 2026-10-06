/** @vitest-environment jsdom */
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest"
import { act, cleanup, fireEvent, render, screen, waitFor, within } from "@testing-library/react"
import type { MoleStore, SessionState } from "@/lib/mole-store"
import type { AppRemovalPreview } from "@/lib/mole"
import { UI_STRINGS } from "@/i18n/strings"
const mocks = vi.hoisted(() => ({
  state: undefined as unknown as MoleStore,
  listeners: new Set<() => void>(), initialize: vi.fn(), refresh: vi.fn(), run: vi.fn(),
  open: vi.fn(), cancel: vi.fn(), remove: vi.fn(),
}))
vi.mock("@/lib/mole-store", () => ({
  getSnapshot: () => mocks.state,
  subscribe: (fn: () => void) => { mocks.listeners.add(fn); return () => { mocks.listeners.delete(fn) } },
  initialize: mocks.initialize, refreshScans: mocks.refresh, run: mocks.run,
  openAppRemoval: mocks.open, cancelRemoval: mocks.cancel, removeSelected: mocks.remove,
}))
vi.mock("@/lib/system-metrics-store", () => ({ formatBytes: (bytes: number) => `${bytes} B` }))
import { MolePanel } from "./MolePanel"
const t = (key: string) => UI_STRINGS.ko[key] ?? key
const app = { id: "a1", name: "Sample", path: "/Applications/Sample.app", bundle_id: "org.test.sample", source: "App", size_label: "1MB", blocked_reason: null }
const makePreview = (): AppRemovalPreview => ({
  token: "p1", generation: "g1", expires_at_ms: Date.now() + 60_000,
  app: { id: "bundle", kind: "app", path: app.path, size_bytes: 1000 },
  related: [
    { id: "cache", kind: "cache", path: "/Users/test/Library/Caches/org.test.sample", size_bytes: 100 },
    { id: "prefs", kind: "preferences", path: "/Users/test/Library/Preferences/org.test.sample.plist", size_bytes: null },
  ], excluded: [],
})
const empty = (): SessionState => ({
  progress: [], items: [], analyze: null, maintenance: [], scanResult: null, result: null,
  maintenanceResult: [], error: null, updatedAt: 1000, lastRunAt: null, stale: false,
})
function publish(patch: Partial<MoleStore>) {
  mocks.state = { ...mocks.state, ...patch }
  for (const listener of mocks.listeners) listener()
}
beforeEach(() => {
  vi.resetAllMocks()
  mocks.listeners.clear()
  mocks.state = {
    info: { version: "1.38.1", path: "/opt/homebrew/bin/mo", supported: true, required_version: "1.38.1" },
    initialized: true, refreshing: false, detectionError: null, busy: null,
    sessions: {
      disk: { ...empty(), analyze: { entries: [{ name: "Home", path: "/Users/test", size: 2000 }], large_files: [] } },
      cache: { ...empty(), items: [{ id: "one", section: "User essentials", name: "User app cache", detail: "Raw cache line", bytes: 1000, status: "candidate" }], scanResult: { mode: "preview", bytes: 1000, items: 1, partial: false } },
      tune: { ...empty(), maintenance: [{ id: "dock", name: "Dock Refresh", details: ["Dock refreshed"], status: "preview" }] },
    },
    apps: { inventory: { generation: "g1", apps: [app] }, updatedAt: 1000, stale: false, error: null },
    selectedApp: null, preview: null, previewError: null, removalResult: null, removalError: null,
  }
  for (const fn of [mocks.initialize, mocks.refresh, mocks.run, mocks.remove]) fn.mockResolvedValue(undefined)
  mocks.open.mockImplementation(async () => publish({ selectedApp: app, preview: makePreview() }))
  mocks.cancel.mockImplementation(async () => publish({ selectedApp: null, preview: null }))
})
afterEach(() => { cleanup(); vi.useRealTimers() })

describe("always-visible cleanup workspace", () => {
  it("activates only while visible and displays every complete list without scan buttons", () => {
    const view = render(<MolePanel active={false} t={t} />)
    expect(mocks.initialize).not.toHaveBeenCalled()
    view.rerender(<MolePanel active t={t} />)
    expect(mocks.initialize).toHaveBeenCalledOnce()
    for (const key of ["session.disk", "session.cache", "session.tune", "appsHeading"]) expect(screen.getByRole("heading", { name: t(`tools.mole.${key}`) })).toBeTruthy()
    expect(screen.getByText("Home")).toBeTruthy()
    expect(screen.getByText(t("tools.mole.item.User app cache"))).toBeTruthy()
    expect(screen.getByText(t("tools.mole.task.dock.title"))).toBeTruthy()
    expect(screen.getByText(app.path)).toBeTruthy()
    expect(screen.getByText(t("tools.mole.expected"))).toBeTruthy()
    expect(screen.queryByText(t("tools.mole.reported"))).toBeNull()
    fireEvent.click(screen.getByRole("button", { name: t("tools.refresh") }))
    expect(mocks.refresh).toHaveBeenCalledOnce()
    expect(mocks.run).not.toHaveBeenCalled()
    expect(mocks.remove).not.toHaveBeenCalled()
  })
  it("keeps previous rows visible while busy and disables every destructive entry", () => {
    mocks.state.busy = "cache"
    render(<MolePanel active t={t} />)
    for (const name of [t("tools.mole.cleanNow"), t("tools.mole.optimizeNow"), /Sample.*삭제 검토/]) {
      expect(screen.getByRole<HTMLButtonElement>("button", { name }).disabled).toBe(true)
    }
    expect(screen.getByText(t("tools.mole.item.User app cache"))).toBeTruthy()
    expect(screen.getByRole("status").textContent).toContain(t("tools.mole.scanning"))
  })
  it("shows missing/version support instead of allowing native operations", () => {
    mocks.state.info = { version: "9", path: "/mo", supported: false, required_version: "1.38.1" }
    const view = render(<MolePanel active t={t} />)
    expect(screen.getByText(/현재 버전: 9/)).toBeTruthy()
    expect(screen.getByRole<HTMLButtonElement>("button", { name: t("tools.mole.cleanNow") }).disabled).toBe(true)
    mocks.state = { ...mocks.state, info: null }
    view.rerender(<MolePanel active t={t} />)
    expect(screen.getByText(t("tools.mole.missing"))).toBeTruthy()
    expect(screen.getByText("brew install mole")).toBeTruthy()
  })
  it("requires an in-app exact-action confirmation and never uses window.confirm", () => {
    const nativeConfirm = vi.spyOn(window, "confirm")
    render(<MolePanel active t={t} />)
    fireEvent.click(screen.getByRole("button", { name: t("tools.mole.cleanNow") }))
    const dialog = screen.getByRole("dialog")
    expect(within(dialog).getByText(t("tools.mole.cleanWarning"))).toBeTruthy()
    expect(document.activeElement).toBe(within(dialog).getByRole("button", { name: t("tools.mole.cancel") }))
    expect(mocks.run).not.toHaveBeenCalled()
    fireEvent.click(within(dialog).getByRole("button", { name: t("tools.mole.confirm") }))
    expect(mocks.run).toHaveBeenCalledExactlyOnceWith("cache", "clean")
    expect(nativeConfirm).not.toHaveBeenCalled()
    nativeConfirm.mockRestore()
  })
  it("shows actual maintenance outcomes separately from preview without hiding them in details", () => {
    mocks.state.sessions.tune = {
      ...mocks.state.sessions.tune,
      lastRunAt: 2000,
      maintenanceResult: [
        { id: "dock", name: "Dock Refresh", details: ["Dock refreshed"], status: "completed" },
        { id: "fonts", name: "Font Cache Rebuild", details: ["admin access required"], status: "admin_skipped" },
        { id: "manual", name: "Manual item", details: ["Manual review"], status: "manual" },
      ],
    }
    render(<MolePanel active t={t} />)
    for (const status of ["preview", "completed", "admin_skipped", "manual"]) {
      const label = screen.getByText(t(`tools.mole.status.${status}`))
      expect(label.closest("details")).toBeNull()
    }
    expect(screen.getByText(new RegExp(t("tools.mole.lastRun")))).toBeTruthy()
  })
  it("cannot confirm an old maintenance snapshot after refresh, even with an identical clock timestamp", () => {
    render(<MolePanel active t={t} />)
    fireEvent.click(screen.getByRole("button", { name: t("tools.mole.optimizeNow") }))
    expect(screen.getByText(t("tools.mole.optimizeWarning"))).toBeTruthy()
    act(() => publish({ sessions: { ...mocks.state.sessions, tune: { ...mocks.state.sessions.tune } } }))
    const confirm = screen.getByRole<HTMLButtonElement>("button", { name: t("tools.mole.confirm") })
    expect(confirm.disabled).toBe(true)
    fireEvent.click(confirm)
    expect(mocks.run).not.toHaveBeenCalled()
  })
})

describe("selected related-data removal", () => {
  it("keeps duplicate names distinct by full path and search", () => {
    mocks.state.apps.inventory!.apps = [app, { ...app, id: "a2", path: "/Users/test/Applications/Sample.app" }]
    render(<MolePanel active t={t} />)
    expect(screen.getAllByRole("button", { name: /Sample.*삭제 검토/ })).toHaveLength(2)
    fireEvent.change(screen.getByRole("textbox", { name: t("tools.mole.searchApps") }), { target: { value: "/Users/test/Applications" } })
    expect(screen.getAllByRole("button", { name: /Sample.*삭제 검토/ })).toHaveLength(1)
    expect(screen.queryByText("/Applications/Sample.app")).toBeNull()
  })
  it("defaults all related data unchecked, shows exact paths/loss warnings, and sends only selected IDs", async () => {
    render(<MolePanel active t={t} />)
    fireEvent.click(screen.getByRole("button", { name: /Sample.*삭제 검토/ }))
    const dialog = screen.getByRole("dialog")
    const checks = within(dialog).getAllByRole<HTMLInputElement>("checkbox")
    expect(checks.map((check) => check.checked)).toEqual([false, false])
    expect(within(dialog).getByText(makePreview().related[1].path)).toBeTruthy()
    expect(within(dialog).getByText(t("tools.mole.dataWarning"))).toBeTruthy()
    expect(within(dialog).getByText(t("tools.mole.scopeWarning"))).toBeTruthy()
    expect(dialog.style.animation).toBe("none")
    expect(document.querySelector<HTMLElement>('[data-slot="dialog-overlay"]')?.style.animation).toBe("none")
    expect(within(dialog).getByText("1000 B")).toBeTruthy()
    expect(within(dialog).queryByText(t("tools.mole.includesUnknown"))).toBeNull()
    fireEvent.click(checks[0])
    expect(within(dialog).getByText("1100 B")).toBeTruthy()
    fireEvent.click(checks[0])
    fireEvent.click(checks[1])
    expect(within(dialog).getByText(t("tools.mole.includesUnknown"))).toBeTruthy()
    expect(within(dialog).getByText("1000 B")).toBeTruthy()
    fireEvent.click(within(dialog).getByRole("button", { name: t("tools.mole.removeSelected") }))
    expect(mocks.remove).toHaveBeenCalledExactlyOnceWith(["prefs"])
    await act(async () => {})
  })
  it("cancels without removing and returns focus to the app action", async () => {
    render(<MolePanel active t={t} />)
    const trigger = screen.getByRole<HTMLButtonElement>("button", { name: /Sample.*삭제 검토/ })
    fireEvent.click(trigger)
    fireEvent.click(screen.getByRole("button", { name: t("tools.mole.cancel") }))
    await act(async () => {})
    expect(mocks.cancel).toHaveBeenCalledOnce()
    expect(mocks.remove).not.toHaveBeenCalled()
    expect(screen.queryByRole("dialog")).toBeNull()
    await waitFor(() => expect(document.activeElement).toBe(trigger))
  })
  it("returns focus to app search when reconciliation disables the original action", async () => {
    render(<MolePanel active t={t} />)
    fireEvent.click(screen.getByRole("button", { name: /Sample.*삭제 검토/ }))
    act(() => publish({ selectedApp: null, preview: null, busy: "apps" }))
    await waitFor(() => expect(document.activeElement).toBe(screen.getByRole("textbox", { name: t("tools.mole.searchApps") })))
  })
  it("expires preview permission without automatically refreshing or checking any data", () => {
    vi.useFakeTimers()
    mocks.state.selectedApp = app
    mocks.state.preview = { ...makePreview(), expires_at_ms: Date.now() + 10 }
    render(<MolePanel active t={t} />)
    act(() => vi.advanceTimersByTime(11))
    expect(screen.getByRole<HTMLButtonElement>("button", { name: t("tools.mole.removeSelected") }).disabled).toBe(true)
    expect(screen.getByText(t("tools.mole.previewExpired"))).toBeTruthy()
    expect(mocks.open).not.toHaveBeenCalled()
    expect(mocks.remove).not.toHaveBeenCalled()
  })
  it("keeps stale app rows read-only and reports unknown/partial outcomes without a success claim", () => {
    mocks.state.apps.stale = true
    mocks.state.removalResult = { items: [{ candidate_id: "bundle", kind: "app", path: app.path, status: "unknown", error: "mole_result_unknown", trash_path: null }], stopped_reason: "mole_result_unknown" }
    render(<MolePanel active t={t} />)
    expect(screen.getByRole<HTMLButtonElement>("button", { name: /Sample.*삭제 검토/ }).disabled).toBe(true)
    expect(screen.getByText(t("tools.mole.removePartial"))).toBeTruthy()
    expect(screen.queryByText(t("tools.mole.removeDone"))).toBeNull()
    expect(screen.getByText(t("tools.mole.status.unknown"))).toBeTruthy()
  })
})
