// Every keyboard shortcut the app has, in one table. The key handlers and the
// settings page both read from here, so the list the user sees is the list
// that actually runs.
//
// Combo tokens, joined by "+":
//   Mod    ⌘ on macOS, Ctrl elsewhere (in-app shortcuts)
//   Global the system-wide modifier set: ⌃⌥ on macOS, Ctrl+Alt+Shift elsewhere
//   Ctrl / Alt / Shift  literal modifiers
//   then exactly one key: A–Z, 0–9, "," "/" "." Enter Escape
//
// Global shortcuts are registered by the Rust side (src-tauri/src/global_shortcuts.rs),
// which keeps its own table; a Rust test fails if the two drift apart.
//
// Why ⌃⌥ for global: ⌘⇧ letters are almost all taken by browsers (⌘⇧T is
// "reopen closed tab"), and ⌃⌥ types no characters on macOS. Windows gets
// Ctrl+Alt+Shift because Ctrl+Alt is AltGr there: Ctrl+Alt+2 is how a Spanish
// keyboard types "@", and a global hotkey on it would swallow the character.

import { useEffect, useRef } from "react"

export type ShortcutGroup = "global" | "app" | "translate" | "chat" | "clipboard"

export type ShortcutDef = {
  id: string
  group: ShortcutGroup
  combo: string
  /** i18n key for the action's description. */
  label: string
  /**
   * Handled by the focused text field itself (for example ⌘↵ inside the
   * translate input). Listed on the settings page, never bound globally.
   */
  local?: true
}

export const SHORTCUTS: readonly ShortcutDef[] = [
  { id: "global.toggle", group: "global", combo: "Global+S", label: "shortcuts.global.toggle" },
  { id: "global.translate", group: "global", combo: "Global+1", label: "shortcuts.global.translate" },
  { id: "global.chat", group: "global", combo: "Global+2", label: "shortcuts.global.chat" },
  { id: "global.clipboard", group: "global", combo: "Global+3", label: "shortcuts.global.clipboard" },
  { id: "global.tools", group: "global", combo: "Global+4", label: "shortcuts.global.tools" },
  { id: "global.newMemo", group: "global", combo: "Global+M", label: "shortcuts.global.newMemo" },

  { id: "app.tab.translate", group: "app", combo: "Mod+1", label: "shortcuts.app.tab.translate" },
  { id: "app.tab.chat", group: "app", combo: "Mod+2", label: "shortcuts.app.tab.chat" },
  { id: "app.tab.clipboard", group: "app", combo: "Mod+3", label: "shortcuts.app.tab.clipboard" },
  { id: "app.tab.tools", group: "app", combo: "Mod+4", label: "shortcuts.app.tab.tools" },
  { id: "app.history", group: "app", combo: "Mod+Y", label: "shortcuts.app.history" },
  { id: "app.pin", group: "app", combo: "Mod+Shift+P", label: "shortcuts.app.pin" },
  { id: "app.compact", group: "app", combo: "Mod+Shift+M", label: "shortcuts.app.compact" },
  { id: "app.settings", group: "app", combo: "Mod+,", label: "shortcuts.app.settings" },
  { id: "app.shortcuts", group: "app", combo: "Mod+/", label: "shortcuts.app.shortcuts" },
  { id: "app.hide", group: "app", combo: "Escape", label: "shortcuts.app.hide" },

  { id: "translate.run", group: "translate", combo: "Mod+Enter", label: "shortcuts.translate.run", local: true },
  { id: "translate.new", group: "translate", combo: "Mod+N", label: "shortcuts.translate.new" },
  { id: "translate.swap", group: "translate", combo: "Mod+Shift+S", label: "shortcuts.translate.swap" },
  { id: "translate.copy", group: "translate", combo: "Mod+Shift+C", label: "shortcuts.translate.copy" },
  { id: "translate.stop", group: "translate", combo: "Mod+.", label: "shortcuts.translate.stop" },

  { id: "chat.send", group: "chat", combo: "Enter", label: "shortcuts.chat.send", local: true },
  { id: "chat.newline", group: "chat", combo: "Shift+Enter", label: "shortcuts.chat.newline", local: true },
  { id: "chat.new", group: "chat", combo: "Mod+N", label: "shortcuts.chat.new" },
  { id: "chat.stop", group: "chat", combo: "Mod+.", label: "shortcuts.chat.stop" },
  { id: "chat.cancelEdit", group: "chat", combo: "Escape", label: "shortcuts.chat.cancelEdit", local: true },

  { id: "clipboard.newMemo", group: "clipboard", combo: "Mod+N", label: "shortcuts.clipboard.newMemo" },
  { id: "clipboard.search", group: "clipboard", combo: "Mod+F", label: "shortcuts.clipboard.search" },
  { id: "clipboard.save", group: "clipboard", combo: "Mod+Enter", label: "shortcuts.clipboard.save", local: true },
  { id: "clipboard.cancel", group: "clipboard", combo: "Escape", label: "shortcuts.clipboard.cancel", local: true },
]

export const SHORTCUT_GROUPS: readonly ShortcutGroup[] = ["global", "app", "translate", "chat", "clipboard"]

const BY_ID = new Map(SHORTCUTS.map((s) => [s.id, s]))

export function shortcut(id: string): ShortcutDef {
  const def = BY_ID.get(id)
  if (!def) throw new Error(`unknown shortcut ${id}`)
  return def
}

export function isMacPlatform(): boolean {
  if (typeof navigator === "undefined") return false
  return /Mac|iPhone|iPad/.test(navigator.platform || navigator.userAgent)
}

type Parsed = { mod: boolean; ctrl: boolean; alt: boolean; shift: boolean; key: string }

function parse(combo: string, mac: boolean): Parsed {
  const parts = combo.split("+")
  // "+" is not a key here, so the last part is always the key.
  const key = parts.pop() ?? ""
  const p: Parsed = { mod: false, ctrl: false, alt: false, shift: false, key }
  for (const part of parts) {
    if (part === "Mod") p.mod = true
    else if (part === "Ctrl") p.ctrl = true
    else if (part === "Alt") p.alt = true
    else if (part === "Shift") p.shift = true
    else if (part === "Global") {
      p.ctrl = true
      p.alt = true
      if (!mac) p.shift = true
    } else throw new Error(`unknown modifier ${part} in ${combo}`)
  }
  return p
}

/** Physical key codes, so ⌘N still works while a Korean input source is active. */
function codesFor(key: string): string[] {
  if (/^[A-Z]$/.test(key)) return [`Key${key}`]
  if (/^[0-9]$/.test(key)) return [`Digit${key}`, `Numpad${key}`]
  switch (key) {
    case ",":
      return ["Comma"]
    case "/":
      return ["Slash"]
    case ".":
      return ["Period"]
    case "Enter":
      return ["Enter", "NumpadEnter"]
    case "Escape":
      return ["Escape"]
  }
  throw new Error(`unknown key ${key}`)
}

type KeyLike = Pick<KeyboardEvent, "code" | "metaKey" | "ctrlKey" | "altKey" | "shiftKey">

export function matchesCombo(combo: string, e: KeyLike, mac = isMacPlatform()): boolean {
  const p = parse(combo, mac)
  if (!codesFor(p.key).includes(e.code)) return false
  const wantMeta = mac && p.mod
  const wantCtrl = p.ctrl || (!mac && p.mod)
  return (
    e.metaKey === wantMeta &&
    e.ctrlKey === wantCtrl &&
    e.altKey === p.alt &&
    e.shiftKey === p.shift
  )
}

/** Keycaps in the platform's conventional order: ⌃⌥⇧⌘ on macOS, Ctrl Alt Shift elsewhere. */
export function comboKeys(combo: string, mac = isMacPlatform()): string[] {
  const p = parse(combo, mac)
  const keys: string[] = []
  if (mac) {
    if (p.ctrl) keys.push("⌃")
    if (p.alt) keys.push("⌥")
    if (p.shift) keys.push("⇧")
    if (p.mod) keys.push("⌘")
  } else {
    if (p.ctrl || p.mod) keys.push("Ctrl")
    if (p.alt) keys.push("Alt")
    if (p.shift) keys.push("Shift")
  }
  const named: Record<string, [string, string]> = {
    Enter: ["↩", "Enter"],
    Escape: ["esc", "Esc"],
  }
  keys.push(named[p.key] ? named[p.key][mac ? 0 : 1] : p.key)
  return keys
}

/** One-line form for tooltips and prose: ⌃⌥S on macOS, Ctrl+Alt+Shift+S elsewhere. */
export function formatCombo(combo: string, mac = isMacPlatform()): string {
  return comboKeys(combo, mac).join(mac ? "" : "+")
}

/** Return false to let the key through (nothing to act on right now). */
export type ShortcutHandler = (e: KeyboardEvent) => boolean | void

/**
 * Bind in-app shortcuts by id while the calling component is mounted. Only
 * the active panel is mounted, so tab-specific ids never fire in another tab.
 * A key a focused field already handled (defaultPrevented) is left alone.
 */
export function useShortcuts(bindings: Record<string, ShortcutHandler | undefined>): void {
  const ref = useRef(bindings)
  useEffect(() => {
    ref.current = bindings
  })
  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      if (e.defaultPrevented || e.repeat || e.isComposing) return
      for (const [id, handler] of Object.entries(ref.current)) {
        if (!handler) continue
        const def = shortcut(id)
        if (def.local || def.group === "global") continue
        if (!matchesCombo(def.combo, e)) continue
        if (handler(e) === false) return
        e.preventDefault()
        return
      }
    }
    window.addEventListener("keydown", onKey)
    return () => window.removeEventListener("keydown", onKey)
  }, [])
}
