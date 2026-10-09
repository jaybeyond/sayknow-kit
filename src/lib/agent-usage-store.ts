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

/** Background triggers (focus, the minute timer) skip a scan this recent. Opening
 *  the window or the tab, and the refresh buttons, always scan. */
const MIN_REFRESH_MS = 30_000
/** DeepL's quota lookup rides along with the scan; a stalled connection must
 *  not keep the scan, and every later one, waiting forever. */
export const DEEPL_TIMEOUT_MS = 15_000

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
let current: Promise<void> | null = null
/** A forced request that arrived while a scan was already reading the logs.
 *  That scan may have passed a file before it changed, so one more follows. */
let rerun = false
let rerunKey: string | undefined

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

/** Settles with whatever DeepL answered within the limit, or an error. */
function deeplWithin(key: string): Promise<{ ok: true; u: DeeplUsage } | { ok: false; e: unknown }> {
  const controller = new AbortController()
  let timer: ReturnType<typeof setTimeout> | undefined
  const timeout = new Promise<never>((_, reject) => {
    timer = setTimeout(() => {
      controller.abort()
      reject(new Error("DeepL usage request timed out"))
    }, DEEPL_TIMEOUT_MS)
  })
  return Promise.race([deeplUsage(key, controller.signal), timeout]).then(
    (u) => ({ ok: true as const, u }),
    (e: unknown) => ({ ok: false as const, e }),
  ).finally(() => clearTimeout(timer))
}

async function scanOnce(deeplKey?: string): Promise<void> {
  try {
    const { invoke } = await import("@tauri-apps/api/core")
    // The DeepL call is a metadata lookup, not a translation — it costs no
    // characters, so it rides along with the scan instead of needing its own
    // refresh button.
    const [agents, deepl] = await Promise.all([
      invoke<AgentReport[]>("agent_usage"),
      deeplKey?.trim() ? deeplWithin(deeplKey) : Promise.resolve(null),
    ])
    lastRun = Date.now()
    set({
      agents,
      error: null,
      scannedAt: lastRun,
      deepl: deepl?.ok ? deepl.u : null,
      deeplError: deepl && !deepl.ok ? String(deepl.e) : null,
    })
  } catch (e) {
    set({ error: String(e) })
  }
}

async function scanLoop(deeplKey?: string): Promise<void> {
  set({ loading: true })
  try {
    let key = deeplKey
    for (;;) {
      rerun = false
      await scanOnce(key)
      if (!rerun) break
      key = rerunKey
    }
  } finally {
    current = null
    set({ loading: false })
  }
}

/**
 * Reads the session logs again. `force` is for the moments the user is looking:
 * the window or tab opening and the refresh buttons. A forced request during a
 * running scan is not dropped; the returned promise settles after the scan that
 * started after it.
 */
export function scanAgentUsage(force = false, deeplKey?: string): Promise<void> {
  if (!isTauri()) return Promise.resolve()
  if (current) {
    if (force) {
      rerun = true
      rerunKey = deeplKey
    }
    return current
  }
  if (!force && Date.now() - lastRun < MIN_REFRESH_MS) return Promise.resolve()
  current = scanLoop(deeplKey)
  return current
}
