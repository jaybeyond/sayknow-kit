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
}

export type ToolsState = {
  displays: DisplayRow[]
  error: string | null
  loaded: boolean
}

let state: ToolsState = { displays: [], error: null, loaded: false }
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

export async function scanDisplays(force = false): Promise<void> {
  if (!isTauri() || (inFlight && !force)) return
  inFlight = true
  try {
    const { invoke } = await import("@tauri-apps/api/core")
    const displays = await invoke<DisplayRow[]>("list_displays")
    set({ displays, error: null, loaded: true })
  } catch (e) {
    set({ error: String(e), loaded: true })
  } finally {
    inFlight = false
  }
}
