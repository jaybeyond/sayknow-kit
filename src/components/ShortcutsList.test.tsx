/** @vitest-environment jsdom */
import { afterEach, describe, expect, it, vi } from "vitest"
import { cleanup, render, screen, within } from "@testing-library/react"

const mocks = vi.hoisted(() => ({
  invoke: vi.fn(async () => [
    { id: "global.toggle", registered: true },
    { id: "global.newMemo", registered: false },
  ]),
}))

vi.mock("@tauri-apps/api/core", () => ({ invoke: mocks.invoke }))
vi.mock("@/lib/runtime", () => ({ isTauri: () => true }))
vi.mock("@/i18n", () => ({ useT: () => ({ t: (key: string) => key }) }))

import { SHORTCUTS } from "@/lib/shortcuts"
import { ShortcutsList } from "./ShortcutsList"

afterEach(() => {
  cleanup()
  vi.clearAllMocks()
})

function row(id: string): HTMLElement {
  const el = document.querySelector(`[data-shortcut="${id}"]`)
  if (!(el instanceof HTMLElement)) throw new Error(`no row ${id}`)
  return el
}

describe("ShortcutsList", () => {
  it("lists every shortcut once, under its group", () => {
    render(<ShortcutsList uiLocale="en" mac />)
    expect(document.querySelectorAll("[data-shortcut]")).toHaveLength(SHORTCUTS.length)
    const global = screen.getByRole("region", { name: "shortcuts.group.global" })
    expect(within(global).getByText("shortcuts.global.toggle")).toBeTruthy()
    expect(within(global).queryByText("shortcuts.app.pin")).toBeNull()
  })

  it("shows macOS keycaps on a Mac and spelled-out modifiers elsewhere", () => {
    const { unmount } = render(<ShortcutsList uiLocale="en" mac />)
    expect(row("global.toggle").querySelector("kbd")?.getAttribute("aria-label")).toBe("⌃ ⌥ S")
    expect(row("app.settings").querySelector("kbd")?.getAttribute("aria-label")).toBe("⌘ ,")
    unmount()
    render(<ShortcutsList uiLocale="en" mac={false} />)
    expect(row("global.toggle").querySelector("kbd")?.getAttribute("aria-label")).toBe("Ctrl Alt Shift S")
  })

  it("flags a global shortcut the OS refused to register", async () => {
    render(<ShortcutsList uiLocale="en" mac />)
    expect(await within(row("global.newMemo")).findByText("shortcuts.unregistered")).toBeTruthy()
    expect(within(row("global.toggle")).queryByText("shortcuts.unregistered")).toBeNull()
    expect(mocks.invoke).toHaveBeenCalledWith("get_global_shortcuts")
  })
})
