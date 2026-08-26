// Module-level store for the agent-usage scan.
//
// The scan is disk-heavy (hundreds of MB on a cold cache) and the app runs two
// windows, so it must not be per-component state: both windows share one
// result and one in-flight guard. Keeping it outside React also means the
// visibility effect is a pure subscription rather than an effect that writes
// component state on mount.

import { isTauri } from "./runtime"
import type { AgentReport } from "./agent-usage"
import { deeplUsage, type DeeplUsage } from "./deepl"

/** Cold scans are expensive; don't re-run on every tab switch. */
const MIN_REFRESH_MS = 30_000

export type UsageState = {
  agents: AgentReport[]
  loading: boolean
  error: string | null
  scannedAt: number | null
  /** DeepL's live quota. Unlike the CLI snapshots this is current, not a
   *  reading left behind by the last session. */
  deepl: DeeplUsage | null
  deeplError: string | null
}

let state: UsageState = {
  agents: [],
  loading: false,
  error: null,
  scannedAt: null,
  deepl: null,
  deeplError: null,
}

const listeners = new Set<() => void>()
let lastRun = 0
let inFlight = false

function set(patch: Partial<UsageState>) {
  state = { ...state, ...patch }
  for (const l of listeners) l()
}

export function subscribe(listener: () => void): () => void {
  listeners.add(listener)
  return () => {
    listeners.delete(listener)
  }
}

export function getSnapshot(): UsageState {
  return state
}

export async function scanAgentUsage(
  force = false,
  deeplKey?: string,
): Promise<void> {
  if (!isTauri() || inFlight) return
  if (!force && Date.now() - lastRun < MIN_REFRESH_MS) return
  inFlight = true
  set({ loading: true })
  try {
    const { invoke } = await import("@tauri-apps/api/core")
    // The DeepL call is a metadata lookup, not a translation — it costs no
    // characters, so it rides along with the scan instead of needing its own
    // refresh button.
    const [agents, deepl] = await Promise.all([
      invoke<AgentReport[]>("agent_usage"),
      deeplKey?.trim()
        ? deeplUsage(deeplKey).then(
            (u) => ({ ok: true as const, u }),
            (e: unknown) => ({ ok: false as const, e }),
          )
        : Promise.resolve(null),
    ])
    lastRun = Date.now()
    set({
      agents,
      error: null,
      scannedAt: lastRun,
      loading: false,
      deepl: deepl?.ok ? deepl.u : null,
      deeplError: deepl && !deepl.ok ? String(deepl.e) : null,
    })
  } catch (e) {
    set({ error: String(e), loading: false })
  } finally {
    inFlight = false
  }
}
