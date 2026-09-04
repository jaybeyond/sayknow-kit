// Module store for display-control state. Same rationale as the usage scan:
// the effect in the panel stays a pure trigger, no setState cascades, and
// the in-flight guard lives once instead of per-component.

import { isTauri } from "./runtime"

export type DisplayRow = {
  id: string
  name: string
  kind: "builtin" | "external"
  is_main: boolean
  brightness: number | null
  power: boolean | null
  controllable: boolean
  method: "backlight" | "ddc" | "gamma" | "none"
  system_level: number | null
}

export type AccessibilityState = {
  trusted: boolean
  /** Quarantined copy running from a randomized read-only path: macOS throws
   *  the granted permission away on the next launch, so consent cannot fix it. */
  translocated: boolean
  /** Ad-hoc signed build: the grant is pinned to this binary's hash, so an
   *  update leaves a stale entry that still shows as enabled but is denied. */
  adhoc: boolean
}

export type ToolsState = {
  displays: DisplayRow[]
  error: string | null
  loaded: boolean
  accessibility: AccessibilityState | null
}

let state: ToolsState = {
  displays: [],
  error: null,
  loaded: false,
  accessibility: null,
}
const listeners = new Set<() => void>()
let inFlight = false

function set(patch: Partial<ToolsState>) {
  state = { ...state, ...patch }
  for (const l of listeners) l()
}

export function subscribe(l: () => void) {
  listeners.add(l)
  return () => {
    listeners.delete(l)
  }
}

export function getSnapshot(): ToolsState {
  return state
}


/** Refresh only the built-in row — a registry read, no DDC traffic — so the
 *  slider can follow the keyboard brightness keys while the tab is visible. */
export async function syncBuiltin(): Promise<void> {
  if (!isTauri()) return
  try {
    const { invoke } = await import("@tauri-apps/api/core")
    const current = await invoke<{
      brightness: number
      system_level: number
    } | null>("sync_builtin_brightness")
    if (current === null || current === undefined) return
    set({
      displays: state.displays.map((d) =>
        d.kind === "builtin"
          ? {
              ...d,
              brightness: current.brightness,
              system_level: current.system_level,
            }
          : d,
      ),
    })
  } catch {
    /* registry read failed; keep the last known value */
  }
}

export async function scanDisplays(force = false): Promise<void> {
  if (!isTauri() || (inFlight && !force)) return
  inFlight = true
  try {
    const { invoke } = await import("@tauri-apps/api/core")
    // Only an explicit refresh pays for a full DDC round trip; everything else
    // accepts the backend's recent scan so opening the panel stays instant.
    const displays = await invoke<DisplayRow[]>("list_displays", { force })
    set({ displays, error: null, loaded: true })
  } catch (e) {
    set({ error: String(e), loaded: true })
  } finally {
    inFlight = false
  }
}

/** Whether macOS lets us drive the built-in backlight. This reads, never
 *  prompts: the consent dialog belongs to an explicit user action, because
 *  macOS re-shows it on every call while the grant cannot be stored. */
export async function refreshAccessibility(): Promise<void> {
  if (!isTauri()) return
  try {
    const { invoke } = await import("@tauri-apps/api/core")
    const accessibility = await invoke<AccessibilityState>("accessibility_status")
    set({ accessibility })
  } catch {
    /* command unavailable; keep the last known state */
  }
}

/** Explicit user request for the macOS consent dialog. */
export async function requestAccessibility(): Promise<void> {
  if (!isTauri()) return
  try {
    const { invoke } = await import("@tauri-apps/api/core")
    const trusted = await invoke<boolean>("request_accessibility_permission")
    if (trusted) await scanDisplays(true)
  } finally {
    await refreshAccessibility()
  }
}

/** Remove our stale TCC entry and ask again in one step. The user should not
 *  need a terminal to recover from an ad-hoc update invalidating the grant. */
export async function resetAccessibility(): Promise<void> {
  if (!isTauri()) return
  try {
    const { invoke } = await import("@tauri-apps/api/core")
    const trusted = await invoke<boolean>("reset_accessibility_permission")
    if (trusted) await scanDisplays(true)
  } finally {
    await refreshAccessibility()
  }
}

/** Restart so a fresh process picks up a just-granted Accessibility permission.
 *  macOS decides trust when the process starts; the running one stays denied. */
export async function relaunchApp(): Promise<void> {
  if (!isTauri()) return
  const { invoke } = await import("@tauri-apps/api/core")
  await invoke("relaunch_app")
}
