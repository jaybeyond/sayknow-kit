// What the status panel draws over time: the last hour of samples the Rust
// side keeps, and the heaviest processes right now. Kept apart from the
// snapshot store, which owns the per-poll contract its tests pin down; this
// one only polls while the status tab is on screen.
import { isTauri } from "./runtime"

export type HistoryPoint = {
  at_ms: number
  cpu: number | null
  gpu: number | null
  /** Percent of physical memory in use. */
  memory: number | null
  temperature: number | null
  upload: number | null
  download: number | null
}

export type ProcessUsage = {
  pid: number
  name: string
  /** Share of one core, as Activity Monitor counts it; can exceed 100. */
  cpu_percent: number
  memory_bytes: number
}

export type TopProcesses =
  | { state: "available"; by_cpu: ProcessUsage[]; by_memory: ProcessUsage[] }
  | { state: "warming_up" }

export type SystemActivityState = {
  points: HistoryPoint[]
  processes: TopProcesses | null
  error: string | null
}

export const HISTORY_SPAN_MS = 60 * 60 * 1000
const POLL_MS = 3_000
/** Samples further apart than this are drawn as a gap, not joined. */
export const GAP_MS = 30_000

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null && !Array.isArray(value)
}

function optionalNumber(value: unknown): number | null | undefined {
  if (value === null) return null
  return typeof value === "number" && Number.isFinite(value) ? value : undefined
}

/** Points the panel can draw; anything malformed is dropped, not guessed. */
export function decodeHistory(value: unknown): HistoryPoint[] {
  if (!Array.isArray(value)) return []
  const points: HistoryPoint[] = []
  for (const raw of value) {
    if (!isRecord(raw) || typeof raw.at_ms !== "number" || !Number.isSafeInteger(raw.at_ms)) continue
    const fields = ["cpu", "gpu", "memory", "temperature", "upload", "download"] as const
    const decoded = fields.map((key) => optionalNumber(raw[key]))
    if (decoded.some((v) => v === undefined)) continue
    const [cpu, gpu, memory, temperature, upload, download] = decoded as (number | null)[]
    points.push({ at_ms: raw.at_ms, cpu, gpu, memory, temperature, upload, download })
  }
  return points
}

/** Append `incoming` to `current`, oldest first, one point per timestamp, and
 *  nothing older than `spanMs` before the newest. */
export function mergeHistory(current: HistoryPoint[], incoming: HistoryPoint[], spanMs = HISTORY_SPAN_MS): HistoryPoint[] {
  if (incoming.length === 0) return current
  const byTime = new Map<number, HistoryPoint>()
  for (const point of current) byTime.set(point.at_ms, point)
  for (const point of incoming) byTime.set(point.at_ms, point)
  const merged = [...byTime.values()].sort((a, b) => a.at_ms - b.at_ms)
  const newest = merged[merged.length - 1].at_ms
  return merged.filter((point) => point.at_ms >= newest - spanMs)
}

function decodeProcess(value: unknown): ProcessUsage | null {
  if (!isRecord(value)) return null
  const { pid, name, cpu_percent, memory_bytes } = value
  if (
    typeof pid !== "number" ||
    typeof name !== "string" ||
    typeof cpu_percent !== "number" ||
    !Number.isFinite(cpu_percent) ||
    typeof memory_bytes !== "number" ||
    !Number.isFinite(memory_bytes)
  ) {
    return null
  }
  return { pid, name, cpu_percent, memory_bytes }
}

export function decodeTopProcesses(value: unknown): TopProcesses | null {
  if (!isRecord(value)) return null
  if (value.state === "warming_up") return { state: "warming_up" }
  if (value.state !== "available" || !Array.isArray(value.by_cpu) || !Array.isArray(value.by_memory)) return null
  const list = (items: unknown[]) => items.map(decodeProcess).filter((p): p is ProcessUsage => p !== null)
  return { state: "available", by_cpu: list(value.by_cpu), by_memory: list(value.by_memory) }
}

const initial: SystemActivityState = { points: [], processes: null, error: null }
let state = initial
const listeners = new Set<() => void>()
let active = false
let generation = 0
let timer: ReturnType<typeof setInterval> | null = null
let inFlight = false

function setState(next: Partial<SystemActivityState>) {
  state = { ...state, ...next }
  for (const listener of listeners) listener()
}

export function subscribe(listener: () => void) {
  listeners.add(listener)
  return () => listeners.delete(listener)
}

export function getSnapshot() {
  return state
}

async function poll(token: number) {
  if (!active || token !== generation || inFlight) return
  inFlight = true
  try {
    const { invoke } = await import("@tauri-apps/api/core")
    const since = state.points.length > 0 ? state.points[state.points.length - 1].at_ms : null
    const [history, processes] = await Promise.allSettled([
      invoke<unknown>("get_system_history", { sinceMs: since }),
      invoke<unknown>("get_top_processes"),
    ])
    if (!active || token !== generation) return
    const next: Partial<SystemActivityState> = { error: null }
    if (history.status === "fulfilled") next.points = mergeHistory(state.points, decodeHistory(history.value))
    else next.error = String(history.reason)
    if (processes.status === "fulfilled") next.processes = decodeTopProcesses(processes.value) ?? state.processes
    else next.error = String(processes.reason)
    setState(next)
  } finally {
    inFlight = false
  }
}

/** Poll while the status tab is on screen; stop the moment it is not. */
export function setActive(next: boolean) {
  if (next === active) return
  active = next
  generation++
  if (timer) {
    clearInterval(timer)
    timer = null
  }
  if (!next || !isTauri()) return
  const token = generation
  void poll(token)
  timer = setInterval(() => void poll(token), POLL_MS)
}

export function resetForTests() {
  active = false
  generation++
  if (timer) clearInterval(timer)
  timer = null
  inFlight = false
  state = initial
  for (const listener of listeners) listener()
}
