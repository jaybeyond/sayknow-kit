/** @vitest-environment jsdom */
import { afterEach, describe, expect, it, vi } from "vitest"
import { cleanup, renderHook } from "@testing-library/react"
import { UI_LOCALES, UI_STRINGS } from "@/i18n/strings"
import {
  SHORTCUT_GROUPS,
  SHORTCUTS,
  comboKeys,
  formatCombo,
  matchesCombo,
  shortcut,
  useShortcuts,
} from "./shortcuts"

const key = (code: string, mods: Partial<Record<"metaKey" | "ctrlKey" | "altKey" | "shiftKey", boolean>> = {}) => ({
  code,
  metaKey: false,
  ctrlKey: false,
  altKey: false,
  shiftKey: false,
  ...mods,
})

afterEach(() => cleanup())

describe("shortcut registry", () => {
  it("never binds the browser's reopen-closed-tab key", () => {
    for (const mac of [true, false]) {
      for (const s of SHORTCUTS) {
        expect(matchesCombo(s.combo, key("KeyT", { metaKey: mac, ctrlKey: !mac, shiftKey: true }), mac), s.id).toBe(false)
      }
    }
  })

  it("has no two shortcuts that can fire in the same place", () => {
    // App-wide keys are live on every tab, so each tab's keys must not
    // collide with them or with each other.
    for (const group of ["translate", "chat", "clipboard"] as const) {
      const live = SHORTCUTS.filter((s) => (s.group === "app" || s.group === group) && !s.local)
      const combos = live.map((s) => s.combo)
      expect(new Set(combos).size, group).toBe(combos.length)
    }
    const globals = SHORTCUTS.filter((s) => s.group === "global").map((s) => s.combo)
    expect(new Set(globals).size).toBe(globals.length)
  })

  it("labels every shortcut and group in every locale", () => {
    for (const locale of UI_LOCALES) {
      for (const s of SHORTCUTS) expect(UI_STRINGS[locale][s.label], `${locale}:${s.label}`).toBeTruthy()
      for (const g of SHORTCUT_GROUPS) expect(UI_STRINGS[locale][`shortcuts.group.${g}`], `${locale}:${g}`).toBeTruthy()
      expect(UI_STRINGS[locale]["settings.clipboard.body"]).toContain("{keys}")
      expect(UI_STRINGS[locale]["settings.clipboard.title"]).not.toContain("⌘⇧T")
    }
  })

  it("renders global keys as ⌃⌥ on macOS and Ctrl+Alt+Shift elsewhere", () => {
    expect(comboKeys(shortcut("global.toggle").combo, true)).toEqual(["⌃", "⌥", "S"])
    expect(formatCombo(shortcut("global.toggle").combo, true)).toBe("⌃⌥S")
    expect(formatCombo(shortcut("global.toggle").combo, false)).toBe("Ctrl+Alt+Shift+S")
  })

  it("orders modifiers the platform way and names special keys", () => {
    expect(comboKeys("Mod+Shift+P", true)).toEqual(["⇧", "⌘", "P"])
    expect(comboKeys("Mod+Shift+P", false)).toEqual(["Ctrl", "Shift", "P"])
    expect(formatCombo("Mod+Enter", true)).toBe("⌘↩")
    expect(formatCombo("Escape", false)).toBe("Esc")
  })
})

describe("matchesCombo", () => {
  it("maps Mod to ⌘ on macOS and Ctrl elsewhere", () => {
    expect(matchesCombo("Mod+1", key("Digit1", { metaKey: true }), true)).toBe(true)
    expect(matchesCombo("Mod+1", key("Digit1", { ctrlKey: true }), true)).toBe(false)
    expect(matchesCombo("Mod+1", key("Digit1", { ctrlKey: true }), false)).toBe(true)
    expect(matchesCombo("Mod+1", key("Digit1", { metaKey: true }), false)).toBe(false)
  })

  it("requires the exact modifier set", () => {
    expect(matchesCombo("Mod+N", key("KeyN", { metaKey: true, shiftKey: true }), true)).toBe(false)
    expect(matchesCombo("Mod+Shift+S", key("KeyS", { metaKey: true }), true)).toBe(false)
    expect(matchesCombo("Escape", key("Escape", { shiftKey: true }), true)).toBe(false)
  })

  it("matches the physical key, so a Korean input source still works", () => {
    // With 두벌식 active, ⌘N reports key "ㅜ" but code "KeyN".
    expect(matchesCombo("Mod+N", { ...key("KeyN", { metaKey: true }) }, true)).toBe(true)
    expect(matchesCombo("Mod+1", key("Numpad1", { metaKey: true }), true)).toBe(true)
    expect(matchesCombo("Mod+,", key("Comma", { metaKey: true }), true)).toBe(true)
    expect(matchesCombo("Mod+/", key("Slash", { metaKey: true }), true)).toBe(true)
  })
})

describe("useShortcuts", () => {
  function press(init: KeyboardEventInit) {
    const e = new KeyboardEvent("keydown", { bubbles: true, cancelable: true, ...init })
    window.dispatchEvent(e)
    return e
  }

  // jsdom is not a Mac, so Mod is Ctrl here.
  it("runs the bound handler and claims the key", () => {
    const onNew = vi.fn()
    renderHook(() => useShortcuts({ "chat.new": onNew }))
    const e = press({ code: "KeyN", ctrlKey: true })
    expect(onNew).toHaveBeenCalledTimes(1)
    expect(e.defaultPrevented).toBe(true)
  })

  it("lets the key through when the handler has nothing to do", () => {
    renderHook(() => useShortcuts({ "chat.stop": () => false }))
    expect(press({ code: "Period", ctrlKey: true }).defaultPrevented).toBe(false)
  })

  it("leaves keys a focused field already handled, repeats, and IME composition alone", () => {
    const onHide = vi.fn()
    renderHook(() => useShortcuts({ "app.hide": onHide }))
    const handled = new KeyboardEvent("keydown", { code: "Escape", cancelable: true })
    handled.preventDefault()
    window.dispatchEvent(handled)
    press({ code: "Escape", repeat: true })
    press({ code: "Escape", isComposing: true })
    expect(onHide).not.toHaveBeenCalled()
    press({ code: "Escape" })
    expect(onHide).toHaveBeenCalledTimes(1)
  })

  it("never binds shortcuts that belong to a text field or the OS", () => {
    const local = vi.fn()
    const global = vi.fn()
    renderHook(() => useShortcuts({ "translate.run": local, "global.toggle": global }))
    press({ code: "Enter", ctrlKey: true })
    press({ code: "KeyS", ctrlKey: true, altKey: true, shiftKey: true })
    expect(local).not.toHaveBeenCalled()
    expect(global).not.toHaveBeenCalled()
  })

  it("stops listening on unmount", () => {
    const onNew = vi.fn()
    const { unmount } = renderHook(() => useShortcuts({ "chat.new": onNew }))
    unmount()
    press({ code: "KeyN", ctrlKey: true })
    expect(onNew).not.toHaveBeenCalled()
  })
})
