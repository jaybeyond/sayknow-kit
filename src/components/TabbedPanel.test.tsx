/** @vitest-environment jsdom */
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest"
import { act, cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react"
import type { Settings } from "@/hooks/useSettings"

type Handler = (event: { payload: string }) => void

const mocks = vi.hoisted(() => ({
  invoke: vi.fn<(cmd: string, args?: unknown) => Promise<undefined>>(async () => undefined),
  handlers: new Map<string, Handler>(),
  store: new Map<string, unknown>(),
}))

vi.mock("@tauri-apps/api/core", () => ({ invoke: mocks.invoke }))
vi.mock("@tauri-apps/api/event", () => ({
  listen: vi.fn(async (name: string, handler: Handler) => {
    mocks.handlers.set(name, handler)
    return () => mocks.handlers.delete(name)
  }),
}))
vi.mock("@/lib/runtime", () => ({ isTauri: () => true }))
vi.mock("@/lib/storage", () => ({
  storage: {
    get: (k: string) => mocks.store.get(k) ?? null,
    set: (k: string, v: unknown) => mocks.store.set(k, v),
  },
}))
vi.mock("@/i18n", () => ({ useT: () => ({ t: (key: string) => key }) }))
vi.mock("@/hooks/useHistory", () => ({
  useHistory: () => ({ entries: [], remove: vi.fn(), togglePin: vi.fn(), clear: vi.fn() }),
}))
vi.mock("./HistoryMenu", () => ({
  HistoryMenu: ({ open }: { open?: boolean }) => <div data-testid="history" data-open={String(!!open)} />,
}))
vi.mock("./QuickMenu", () => ({ QuickMenu: () => null }))
vi.mock("./TranslatePanel", () => ({
  TranslatePanel: ({ autofillRequest }: { autofillRequest?: number | null }) => (
    <div data-testid="panel">translate:{autofillRequest ? "autofill" : "plain"}</div>
  ),
}))
vi.mock("./ChatPanel", () => ({ ChatPanel: () => <div data-testid="panel">chat</div> }))
vi.mock("./ToolsPanel", () => ({ ToolsPanel: () => <div data-testid="panel">tools</div> }))
vi.mock("./ClipboardPanel", () => ({
  ClipboardPanel: ({ composeRequest }: { composeRequest?: number | null }) => (
    <div data-testid="panel">clipboard:{composeRequest ? "compose" : "list"}</div>
  ),
}))

import { TabbedPanel } from "./TabbedPanel"

const update = vi.fn()
const settings = { uiLocale: "en", pinned: false, windowMode: "normal" } as unknown as Settings

function mount(tab = "chat") {
  mocks.store.set("active-tab", tab)
  render(
    <TabbedPanel settings={settings} update={update} onLogout={vi.fn()} themeMode="system" setThemeMode={vi.fn()} />,
  )
  return waitFor(() => expect(mocks.handlers.has("sayknow:shortcut")).toBe(true))
}

function emit(name: string, payload: string) {
  act(() => mocks.handlers.get(name)?.({ payload }))
}

const panel = () => screen.findByTestId("panel").then((el) => el.textContent)

beforeEach(() => mocks.store.clear())
afterEach(() => {
  cleanup()
  vi.clearAllMocks()
  mocks.handlers.clear()
})

describe("TabbedPanel global shortcuts", () => {
  it("shows the requested panel when a panel shortcut opened the popover", async () => {
    await mount("chat")
    emit("sayknow:open", "shortcut:clipboard")
    expect(await panel()).toBe("clipboard:list")
  })

  it("asks translate to pull in the clipboard when opened on it by shortcut", async () => {
    await mount("chat")
    emit("sayknow:open", "shortcut:translate")
    expect(await panel()).toBe("translate:autofill")
  })

  it("keeps the plain toggle's autofill on the translate tab only", async () => {
    await mount("chat")
    emit("sayknow:open", "shortcut")
    expect(await panel()).toBe("chat")
    emit("sayknow:open", "tray")
    expect(await panel()).toBe("chat")
  })

  it("opens a new memo on the clipboard tab", async () => {
    await mount("tools")
    emit("sayknow:open", "shortcut:newMemo")
    expect(await panel()).toBe("clipboard:compose")
  })

  it("hides when the shortcut names the panel already in front, switches otherwise", async () => {
    await mount("chat")
    emit("sayknow:shortcut", "tools")
    expect(await panel()).toBe("tools")
    expect(mocks.invoke).not.toHaveBeenCalledWith("hide_window")

    emit("sayknow:shortcut", "tools")
    expect(mocks.invoke).toHaveBeenCalledWith("hide_window")
    expect(await panel()).toBe("tools")
  })

  it("ignores unknown targets", async () => {
    await mount("chat")
    emit("sayknow:shortcut", "settings")
    emit("sayknow:open", "shortcut:nope")
    expect(await panel()).toBe("chat")
    expect(mocks.invoke).not.toHaveBeenCalledWith("hide_window")
  })
})

describe("TabbedPanel in-app shortcuts", () => {
  // jsdom is not a Mac, so the Mod key is Ctrl here.
  it("switches tabs with Mod+1..4 and remembers the choice", async () => {
    await mount("chat")
    fireEvent.keyDown(window, { code: "Digit3", ctrlKey: true })
    expect(await panel()).toBe("clipboard:list")
    expect(mocks.store.get("active-tab")).toBe("clipboard")
  })

  it("opens settings, or settings at the shortcuts page", async () => {
    await mount()
    fireEvent.keyDown(window, { code: "Comma", ctrlKey: true })
    fireEvent.keyDown(window, { code: "Slash", ctrlKey: true })
    expect(mocks.invoke).toHaveBeenCalledWith("open_settings", {})
    expect(mocks.invoke).toHaveBeenCalledWith("open_settings", { section: "shortcuts" })
  })

  it("toggles pin and the history menu", async () => {
    await mount()
    fireEvent.keyDown(window, { code: "KeyP", ctrlKey: true, shiftKey: true })
    expect(update).toHaveBeenCalledWith({ pinned: true })
    fireEvent.keyDown(window, { code: "KeyY", ctrlKey: true })
    expect(screen.getByTestId("history").dataset.open).toBe("true")
  })

  it("hides on Escape unless an overlay owns it", async () => {
    await mount()
    const overlay = document.createElement("div")
    overlay.setAttribute("role", "dialog")
    document.body.appendChild(overlay)
    fireEvent.keyDown(window, { code: "Escape" })
    expect(mocks.invoke).not.toHaveBeenCalledWith("hide_window")
    overlay.remove()
    fireEvent.keyDown(window, { code: "Escape" })
    expect(mocks.invoke).toHaveBeenCalledWith("hide_window")
  })
})
