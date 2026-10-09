/** @vitest-environment jsdom */
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest"
import { act, cleanup, fireEvent, render, screen, waitFor, within } from "@testing-library/react"
import type { MoleStore, SessionState } from "@/lib/mole-store"
import type { AppRemovalPreview } from "@/lib/mole"
import { UI_STRINGS } from "@/i18n/strings"
const mocks = vi.hoisted(() => ({
  state: undefined as unknown as MoleStore,
  listeners: new Set<() => void>(), initialize: vi.fn(), refresh: vi.fn(), run: vi.fn(),
  open: vi.fn(), cancel: vi.fn(), remove: vi.fn(), openSettings: vi.fn(), relaunch: vi.fn(),
}))
vi.mock("@/lib/mole-store", () => ({
  getSnapshot: () => mocks.state,
  subscribe: (fn: () => void) => { mocks.listeners.add(fn); return () => { mocks.listeners.delete(fn) } },
  initialize: mocks.initialize, refreshScans: mocks.refresh, run: mocks.run,
  openAppRemoval: mocks.open, cancelRemoval: mocks.cancel, removeSelected: mocks.remove,
}))
vi.mock("@/lib/system-metrics-store", () => ({ formatBytes: (bytes: number) => `${bytes} B` }))
vi.mock("@/lib/mole", async (original) => ({ ...await original<typeof import("@/lib/mole")>(), openFullDiskAccessSettings: mocks.openSettings }))
vi.mock("@/lib/tools-store", () => ({ relaunchApp: mocks.relaunch }))
import { MolePanel } from "./MolePanel"
const t = (key: string) => UI_STRINGS.ko[key] ?? key
const app = { id: "a1", name: "Sample", path: "/Applications/Sample.app", resolved_path: null, bundle_id: "org.test.sample", source: "App", size_label: "1MB", blocked_reason: null }
const makePreview = (): AppRemovalPreview => ({
  token: "p1", generation: "g1", expires_at_ms: Date.now() + 60_000,
  app: { id: "bundle", kind: "app", path: app.path, size_bytes: 1000 }, shortcut: null,
  related: [
    { id: "cache", kind: "cache", path: "/Users/test/Library/Caches/org.test.sample", size_bytes: 100 },
    { id: "prefs", kind: "preferences", path: "/Users/test/Library/Preferences/org.test.sample.plist", size_bytes: null },
  ], excluded: [],
})
const empty = (): SessionState => ({
  progress: [], items: [], analyze: null, maintenance: [], scanResult: null, result: null, cleaned: [], runLog: [],
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
    initialized: true, refreshing: false, detectionError: null, fullDiskAccess: true, busy: null,
    sessions: {
      disk: { ...empty(), analyze: { entries: [{ name: "Home", path: "/Users/test", size: 2000 }], large_files: [] } },
      cache: { ...empty(), items: [{ id: "one", section: "User essentials", name: "User app cache", detail: "Raw cache line", bytes: 1000, status: "candidate" }], scanResult: { mode: "preview", bytes: 1000, items: 1, partial: false } },
      tune: { ...empty(), maintenance: [{ id: "dock", name: "Dock Refresh", details: ["Dock refreshed"], status: "preview" }] },
    },
    apps: { inventory: { generation: "g1", apps: [app] }, updatedAt: 1000, stale: false, error: null },
    selectedApp: null, preview: null, previewError: null, removalResult: null, removalError: null,
  }
  for (const fn of [mocks.initialize, mocks.refresh, mocks.run, mocks.remove, mocks.openSettings, mocks.relaunch]) fn.mockResolvedValue(undefined)
  mocks.open.mockImplementation(async () => publish({ selectedApp: app, preview: makePreview() }))
  mocks.cancel.mockImplementation(async () => publish({ selectedApp: null, preview: null }))
})
afterEach(() => { cleanup(); vi.useRealTimers() })
/** Opens one tool page from the overview through its button. */
const openPage = (title: string) => fireEvent.click(screen.getByRole("button", { name: new RegExp(`^${title}`) }))
const goBack = () => fireEvent.click(screen.getByRole("button", { name: t("tools.mole.back") }))

describe("always-visible cleanup workspace", () => {
  it("activates only while visible and displays every complete list without scan buttons", () => {
    const view = render(<MolePanel active={false} t={t} />)
    expect(mocks.initialize).not.toHaveBeenCalled()
    view.rerender(<MolePanel active t={t} />)
    expect(mocks.initialize).toHaveBeenCalledOnce()
    expect(screen.getByRole("heading", { name: t("tools.mole.session.disk") })).toBeTruthy()
    expect(screen.getByText("Home")).toBeTruthy()
    openPage(t("tools.mole.session.cache"))
    expect(screen.getByText(t("tools.mole.item.User app cache"))).toBeTruthy()
    expect(screen.getByText(t("tools.mole.expected"))).toBeTruthy()
    expect(screen.queryByText(t("tools.mole.reported"))).toBeNull()
    goBack()
    openPage(t("tools.mole.session.tune"))
    expect(screen.getByText(t("tools.mole.task.dock.title"))).toBeTruthy()
    goBack()
    openPage(t("tools.mole.appsHeading"))
    expect(screen.getByText(app.path)).toBeTruthy()
    // Refreshing lives once, in the tools header; the page bar carries no duplicate.
    expect(screen.queryByRole("button", { name: t("tools.refresh") })).toBeNull()
    expect(mocks.run).not.toHaveBeenCalled()
    expect(mocks.remove).not.toHaveBeenCalled()
  })
  it("keeps previous rows visible while busy and disables every destructive entry", () => {
    mocks.state.busy = "cache"
    mocks.state.refreshing = true
    render(<MolePanel active t={t} />)
    // The overview button says the page behind it is busy; the page itself is still reachable.
    expect(screen.getByRole("button", { name: new RegExp(`^${t("tools.mole.session.cache")}.*${t("tools.mole.scanning")}`) })).toBeTruthy()
    for (const [page, name] of [[t("tools.mole.session.cache"), t("tools.mole.cleanNow")], [t("tools.mole.session.tune"), t("tools.mole.optimizeNow")], [t("tools.mole.appsHeading"), /Sample.*삭제…/]] as const) {
      openPage(page)
      expect(screen.getByRole<HTMLButtonElement>("button", { name }).disabled).toBe(true)
      if (page === t("tools.mole.session.cache")) {
        expect(screen.getByText(t("tools.mole.item.User app cache"))).toBeTruthy()
        expect(screen.getByRole("status").textContent).toContain(t("tools.mole.scanning"))
      }
      goBack()
    }
  })
  it("shows missing/version support instead of allowing native operations", () => {
    mocks.state.info = { version: "9", path: "/mo", supported: false, required_version: "1.38.1" }
    const view = render(<MolePanel active t={t} />)
    expect(screen.getByText(/현재 버전: 9/)).toBeTruthy()
    openPage(t("tools.mole.session.cache"))
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
    openPage(t("tools.mole.session.cache"))
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
  it("puts what Mole deleted first, with the freed size, and labels the list below as the re-scan", () => {
    const cleaned = ["User app cache", "Go build cache", "Homebrew", "a", "b", "c"].map((name, index) => ({ id: `s:${name}:${index}`, section: "s", name, detail: name, bytes: name === "Homebrew" ? null : (index + 1) * 10 }))
    mocks.state.sessions.cache = { ...mocks.state.sessions.cache, lastRunAt: 2000, result: { mode: "clean", bytes: 1_220_000_000, items: 66, partial: false }, cleaned, runLog: ["➤ User essentials", "✓ User app cache, 10MB"] }
    render(<MolePanel active t={t} />)
    openPage(t("tools.mole.session.cache"))
    const card = screen.getByRole("region", { name: t("tools.mole.cleanResult") })
    expect(within(card).getByText("1220000000 B")).toBeTruthy()
    expect(within(card).getByText(t("tools.mole.cleanedCount").replace("{count}", "66"))).toBeTruthy()
    expect(within(card).getByText(t("tools.mole.afterClean"))).toBeTruthy()
    // Largest first; the rest folds away instead of growing the page.
    const visible = within(card).getAllByRole("listitem").filter((row) => !row.closest("details"))
    expect(visible.map((row) => row.firstElementChild?.textContent)).toEqual(["c", "b", "a", "Go build cache", t("tools.mole.item.User app cache")])
    expect(within(card).getByText(new RegExp(t("tools.mole.cleanedAll"))).closest("summary")).not.toBeNull()
    expect(card.compareDocumentPosition(screen.getByText(t("tools.mole.expected"))) & Node.DOCUMENT_POSITION_FOLLOWING).toBeTruthy()
  })
  it("says plainly when a clean deleted nothing", () => {
    mocks.state.sessions.cache = { ...mocks.state.sessions.cache, lastRunAt: 2000, result: { mode: "clean", bytes: 0, items: null, partial: false } }
    render(<MolePanel active t={t} />)
    openPage(t("tools.mole.session.cache"))
    expect(screen.getByText(t("tools.mole.cleanedNone"))).toBeTruthy()
    expect(screen.queryByText(t("tools.mole.cleanedCount").replace("{count}", "0"))).toBeNull()
  })
  it("explains missing Full Disk Access and offers the settings and a restart", async () => {
    mocks.state = { ...mocks.state, fullDiskAccess: false }
    render(<MolePanel active t={t} />)
    openPage(t("tools.mole.session.cache"))
    const note = screen.getByRole("note")
    expect(within(note).getByText(t("tools.mole.fdaMissing"))).toBeTruthy()
    fireEvent.click(within(note).getByRole("button", { name: t("tools.mole.fdaOpen") }))
    fireEvent.click(within(note).getByRole("button", { name: t("tools.mole.fdaRestart") }))
    await waitFor(() => expect(mocks.relaunch).toHaveBeenCalledOnce())
    expect(mocks.openSettings).toHaveBeenCalledOnce()
    expect(mocks.run).not.toHaveBeenCalled()
  })
  it("does not warn about Full Disk Access when it is granted or unknown", () => {
    for (const fullDiskAccess of [true, null]) {
      mocks.state = { ...mocks.state, fullDiskAccess }
      render(<MolePanel active t={t} />)
      openPage(t("tools.mole.session.cache"))
      expect(screen.queryByText(t("tools.mole.fdaMissing"))).toBeNull()
      cleanup()
    }
  })
  it("keeps actual maintenance outcomes visible in row summaries while explanations are collapsed", () => {
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
    openPage(t("tools.mole.session.tune"))
    for (const status of ["preview", "completed", "admin_skipped", "manual"]) {
      const label = screen.getByText(t(`tools.mole.status.${status}`))
      expect(label.closest("summary")).not.toBeNull()
    }
    expect(screen.getByText(new RegExp(t("tools.mole.lastRun")))).toBeTruthy()
  })
  it("names the real history and settings scope and makes memory release exclusions and history loss clear before running", () => {
    mocks.state.sessions.tune.maintenance = [
      { id: "memory", name: "Memory Optimization", details: ["Inactive memory released"], status: "preview" },
      { id: "usageData", name: "Usage Data", details: ["Knowledge database cleaned"], status: "preview" },
    ]
    render(<MolePanel active t={t} />)
    expect(screen.queryByRole("button", { name: /Mac 동작 정리|유지보수/ })).toBeNull()
    openPage("기록·설정 정리")
    expect(screen.getByRole("heading", { name: "기록·설정 정리" })).toBeTruthy()
    expect(screen.getByTestId("mole-panel").textContent).not.toMatch(/유지보수/)
    expect(screen.queryByText("메모리 정리")).toBeNull()
    const memory = screen.getByText("메모리 압력 확인").closest("details")!
    fireEvent.click(memory.querySelector("summary")!)
    expect(memory.open).toBe(true)
    expect(within(memory).getByText(/purge는 이 앱에서 실행하지 않습니다/)).toBeTruthy()
    fireEvent.click(screen.getByRole("button", { name: "기록·설정 정리…" }))
    const dialog = screen.getByRole("dialog")
    expect(within(dialog).getByRole("heading").textContent).toBe("Mole의 전체 기록·설정 정리를 실행할까요?")
    expect(within(dialog).getByText(/다운로드·알림·사용 기록이 삭제될 수/)).toBeTruthy()
    expect(within(dialog).getByText(/WAL 삭제로 기록이 손실될 수/)).toBeTruthy()
    expect(within(dialog).getByText(/메모리 해제와 네트워크·Bluetooth 재시작 등 관리자 작업은 실행하지/)).toBeTruthy()
    fireEvent.click(within(dialog).getByRole("button", { name: t("tools.mole.cancel") }))
    expect(mocks.run).not.toHaveBeenCalled()
  })
  it("cannot confirm an old maintenance snapshot after refresh, even with an identical clock timestamp", () => {
    render(<MolePanel active t={t} />)
    openPage(t("tools.mole.session.tune"))
    fireEvent.click(screen.getByRole("button", { name: t("tools.mole.optimizeNow") }))
    expect(screen.getByText(t("tools.mole.optimizeWarning"))).toBeTruthy()
    act(() => publish({ sessions: { ...mocks.state.sessions, tune: { ...mocks.state.sessions.tune } } }))
    const confirm = screen.getByRole<HTMLButtonElement>("button", { name: t("tools.mole.confirm") })
    expect(confirm.disabled).toBe(true)
    fireEvent.click(confirm)
    expect(mocks.run).not.toHaveBeenCalled()
  })
})

describe("tool pages", () => {
  it("keeps storage on the overview and opens each other tool on its own page", () => {
    render(<MolePanel active t={t} />)
    expect(screen.getByRole("heading", { name: "Mac 청소" })).toBeTruthy()
    expect(screen.getByRole("navigation", { name: t("tools.mole.moreTools") })).toBeTruthy()
    for (const title of [t("tools.mole.session.cache"), t("tools.mole.session.tune"), t("tools.mole.appsHeading")]) {
      expect(screen.queryByRole("heading", { name: title })).toBeNull()
    }
    openPage(t("tools.mole.session.tune"))
    expect(screen.getByRole("heading", { name: t("tools.mole.session.tune") })).toBeTruthy()
    expect(screen.queryByRole("heading", { name: t("tools.mole.session.disk") })).toBeNull()
    expect(screen.queryByRole("heading", { name: t("tools.mole.session.cache") })).toBeNull()
    expect(screen.queryByRole("navigation")).toBeNull()
    expect(mocks.run).not.toHaveBeenCalled()
  })
  it("shows counts and the cleanable size on the overview buttons", () => {
    render(<MolePanel active t={t} />)
    const cache = screen.getByRole("button", { name: new RegExp(`^${t("tools.mole.session.cache")}`) })
    expect(cache.textContent).toContain("1000 B")
    expect(screen.getByRole("button", { name: new RegExp(`^${t("tools.mole.appsHeading")}`) }).textContent).toContain("1")
  })
  it("moves focus into a page and back to the button that opened it", () => {
    render(<MolePanel active t={t} />)
    const trigger = screen.getByRole("button", { name: new RegExp(`^${t("tools.mole.appsHeading")}`) })
    expect(document.activeElement).not.toBe(trigger)
    fireEvent.click(trigger)
    expect(document.activeElement).toBe(screen.getByRole("button", { name: t("tools.mole.back") }))
    goBack()
    expect(document.activeElement).toBe(screen.getByRole("button", { name: new RegExp(`^${t("tools.mole.appsHeading")}`) }))
  })
  it("keeps help collapsed, tool links before the long inventory, and lists free of nested scrolling", () => {
    mocks.state.sessions.disk.analyze!.entries = Array.from({ length: 40 }, (_, index) => ({ name: `Folder ${index}`, path: `/test/${index}`, size: index + 1 }))
    render(<MolePanel active t={t} />)
    const navigation = screen.getByRole("navigation", { name: t("tools.mole.moreTools") })
    const disk = screen.getByRole("heading", { name: t("tools.mole.session.disk") })
    expect(navigation.compareDocumentPosition(disk) & Node.DOCUMENT_POSITION_FOLLOWING).toBeTruthy()
    expect(navigation.textContent).not.toContain(t("tools.mole.appsHint"))
    for (const title of [t("tools.mole.session.cache"), t("tools.mole.session.tune"), t("tools.mole.appsHeading")]) {
      openPage(title)
      const panel = screen.getByTestId("mole-panel")
      expect(panel.querySelector('[class*="overflow-y"], [class*="overscroll-contain"]')).toBeNull()
      for (const details of panel.querySelectorAll("details")) expect(details.open).toBe(false)
      goBack()
    }
    openPage(t("tools.mole.session.cache"))
    const button = screen.getByRole("button", { name: t("tools.mole.cleanNow") })
    const bar = button.closest("[data-mole-toolbar]")
    expect(bar).not.toBeNull()
    // The bar is not part of the scrolled page, so content can never slide between it and the tabs.
    const scroller = screen.getByTestId("mole-panel").closest("[data-tools-scroll]")
    expect(scroller).not.toBeNull()
    expect(scroller!.contains(bar)).toBe(false)
    expect(bar!.compareDocumentPosition(scroller!) & Node.DOCUMENT_POSITION_FOLLOWING).toBeTruthy()
  })
  it("opens every page at its top instead of the previous page's scroll offset", () => {
    render(<MolePanel active t={t} />)
    const scroller = screen.getByTestId("mole-panel").closest<HTMLElement>("[data-tools-scroll]")!
    scroller.scrollTop = 400
    openPage(t("tools.mole.session.cache"))
    expect(scroller.scrollTop).toBe(0)
    scroller.scrollTop = 300
    goBack()
    expect(scroller.scrollTop).toBe(0)
  })
})

describe("observed cleanup progress", () => {
  it("shows real phases before the command finishes and does not pretend a phase change is success", () => {
    render(<MolePanel active t={t} />)
    openPage(t("tools.mole.session.cache"))
    act(() => publish({ busy: "cache", sessions: { ...mocks.state.sessions, cache: { ...mocks.state.sessions.cache, progress: ["➤ User essentials", "→ User app cache 2 items, 2MB"] } } }))
    const progress = screen.getByRole("region", { name: t("tools.mole.progress") })
    expect(within(progress).getByRole("status").textContent).toContain(t("tools.mole.running"))
    expect(within(progress).getByRole("status").textContent).toContain(t("tools.mole.section.User essentials"))
    expect(within(progress).getByText("→ User app cache 2 items, 2MB")).toBeTruthy()
    expect(mocks.state.sessions.cache.result).toBeNull()
    act(() => publish({ sessions: { ...mocks.state.sessions, cache: { ...mocks.state.sessions.cache, progress: ["➤ User essentials", "→ User app cache 2 items, 2MB", "➤ App caches", "◎ Browser cache skipped"] } } }))
    expect(within(progress).getByRole("status").textContent).toContain(t("tools.mole.section.App caches"))
    expect(within(progress).getByText("◎ Browser cache skipped")).toBeTruthy()
    expect(within(progress).queryByText(t("tools.mole.status.completed"))).toBeNull()
    expect(progress.querySelector('[role="progressbar"]')).toBeNull()
    act(() => publish({ busy: null, sessions: { ...mocks.state.sessions, cache: { ...mocks.state.sessions.cache, error: "mole_timeout" } } }))
    expect(screen.getByRole("alert")).toBeTruthy()
    expect(within(progress).getByText("◎ Browser cache skipped")).toBeTruthy()
    expect(within(progress).queryByText(t("tools.mole.running"))).toBeNull()
  })
  it("uses the scan label for previews and translates maintenance phases without replacing the last inventory", () => {
    mocks.state.refreshing = true
    mocks.state.busy = "tune"
    mocks.state.sessions.tune.progress = ["➤ Dock Refresh", "→ Would refresh Dock"]
    render(<MolePanel active t={t} />)
    openPage(t("tools.mole.session.tune"))
    const progress = screen.getByRole("region", { name: t("tools.mole.progress") })
    expect(within(progress).getByRole("status").textContent).toContain(t("tools.mole.scanning"))
    expect(within(progress).getByRole("status").textContent).toContain(t("tools.mole.task.dock.title"))
    expect(screen.getByRole("list", { name: t("tools.mole.session.tune") })).toBeTruthy()
    expect(within(progress).queryByText(t("tools.mole.running"))).toBeNull()
  })
})

describe("selected related-data removal", () => {
  it("opens confirmation without writing and displays compulsory shortcut plus the real bundle", async () => {
    const linked = { ...app, path: "/Users/test/Applications/Sample.app", resolved_path: "/Volumes/Fixture/APPS/Sample.app" }
    mocks.state.apps.inventory!.apps = [linked]
    mocks.open.mockImplementation(async () => publish({ selectedApp: linked, preview: {
      ...makePreview(), app: { ...makePreview().app, path: linked.resolved_path },
      shortcut: { id: "shortcut", kind: "shortcut", path: linked.path, size_bytes: null },
    } }))
    render(<MolePanel active t={t} />)
    openPage(t("tools.mole.appsHeading"))
    expect(screen.getByText(linked.resolved_path, { exact: false })).toBeTruthy()
    fireEvent.change(screen.getByRole("textbox", { name: t("tools.mole.searchApps") }), { target: { value: "/Volumes/Fixture" } })
    const trigger = screen.getByRole("button", { name: /Sample.*삭제…/ })
    expect(mocks.remove).not.toHaveBeenCalled()
    fireEvent.click(trigger)
    await act(async () => {})
    const dialog = screen.getByRole("dialog")
    expect(within(dialog).getByText(linked.resolved_path)).toBeTruthy()
    expect(within(dialog).getByText(linked.path)).toBeTruthy()
    expect(within(dialog).getByText(t("tools.mole.kind.shortcut"))).toBeTruthy()
    expect(within(dialog).getByText(t("tools.mole.shortcutNotice"))).toBeTruthy()
    expect(within(dialog).getAllByRole<HTMLInputElement>("checkbox").map((row) => row.checked)).toEqual([false, false])
    expect(within(dialog).getByText("1000 B")).toBeTruthy()
    expect(within(dialog).getByText(t("tools.mole.includesUnknown"))).toBeTruthy()
    expect(mocks.remove).not.toHaveBeenCalled()
    fireEvent.click(within(dialog).getByRole("button", { name: t("tools.mole.removeSelected") }))
    expect(mocks.remove).toHaveBeenCalledExactlyOnceWith([])
  })
  it("shows the blocking process PID without allowing removal or exposing interpolation tokens", () => {
    mocks.state.apps.inventory!.apps = [{ ...app, blocked_reason: "mole_app_running_unverified: pid=10818" }]
    render(<MolePanel active t={t} />)
    openPage(t("tools.mole.appsHeading"))
    expect(screen.getByText(t("tools.mole.error.mole_app_running_unverified").replace("{pid}", "10818"))).toBeTruthy()
    expect(screen.getByRole<HTMLButtonElement>("button", { name: /Sample.*삭제…/ }).disabled).toBe(true)
    expect(mocks.open).not.toHaveBeenCalled()
    expect(mocks.remove).not.toHaveBeenCalled()
  })
  it("does not report successful app removal when only the shortcut moved", () => {
    mocks.state.removalResult = { items: [
      { candidate_id: "shortcut", kind: "shortcut", path: "/Users/test/Applications/Sample.app", status: "moved", error: null, trash_path: "/Users/test/.Trash/Sample.app" },
      { candidate_id: "bundle", kind: "app", path: "/Volumes/Fixture/APPS/Sample.app", status: "unknown", error: "mole_result_unknown", trash_path: null },
    ], stopped_reason: "mole_result_unknown" }
    render(<MolePanel active t={t} />)
    openPage(t("tools.mole.appsHeading"))
    expect(screen.getByText(t("tools.mole.removePartial"))).toBeTruthy()
    expect(screen.queryByText(t("tools.mole.removeDone"))).toBeNull()
    expect(screen.getByText(t("tools.mole.kind.shortcut"))).toBeTruthy()
    expect(screen.getByText(t("tools.mole.status.unknown"))).toBeTruthy()
    expect(mocks.remove).not.toHaveBeenCalled()
  })
  it("keeps duplicate names distinct by full path and search", () => {
    mocks.state.apps.inventory!.apps = [app, { ...app, id: "a2", path: "/Users/test/Applications/Sample.app" }]
    render(<MolePanel active t={t} />)
    openPage(t("tools.mole.appsHeading"))
    expect(screen.getAllByRole("button", { name: /Sample.*삭제…/ })).toHaveLength(2)
    fireEvent.change(screen.getByRole("textbox", { name: t("tools.mole.searchApps") }), { target: { value: "/Users/test/Applications" } })
    expect(screen.getAllByRole("button", { name: /Sample.*삭제…/ })).toHaveLength(1)
    expect(screen.queryByText("/Applications/Sample.app")).toBeNull()
  })
  it("defaults all related data unchecked, shows exact paths/loss warnings, and sends only selected IDs", async () => {
    render(<MolePanel active t={t} />)
    openPage(t("tools.mole.appsHeading"))
    fireEvent.click(screen.getByRole("button", { name: /Sample.*삭제…/ }))
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
    openPage(t("tools.mole.appsHeading"))
    const trigger = screen.getByRole<HTMLButtonElement>("button", { name: /Sample.*삭제…/ })
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
    openPage(t("tools.mole.appsHeading"))
    fireEvent.click(screen.getByRole("button", { name: /Sample.*삭제…/ }))
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
    openPage(t("tools.mole.appsHeading"))
    expect(screen.getByRole<HTMLButtonElement>("button", { name: /Sample.*삭제…/ }).disabled).toBe(true)
    expect(screen.getByText(t("tools.mole.removePartial"))).toBeTruthy()
    expect(screen.queryByText(t("tools.mole.removeDone"))).toBeNull()
    expect(screen.getByText(t("tools.mole.status.unknown"))).toBeTruthy()
  })
})
