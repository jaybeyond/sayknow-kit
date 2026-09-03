import { isTauri } from "./runtime"

export type CpuMetric =
  | { state: "available"; percent: number; sample_start_ms: number; sample_end_ms: number }
  | { state: "warming_up"; reason: string }
  | { state: "unavailable"; reason: string }
export type ResourceMetric =
  | { state: "available"; total_bytes: number; used_bytes: number; available_bytes: number; sampled_at_ms: number }
  | { state: "unavailable"; reason: string }
export type TemperatureProvenance = "verified_cpu_package" | "apple_soc_die_max"
export type TemperatureMetric =
  | { state: "available"; celsius: number; sampled_at_ms: number; provenance: TemperatureProvenance; adapter_id: string }
  | { state: "unavailable"; reason: string }
export type MetricsSnapshot = {
  schema_version: 1
  sampled_at_ms: number
  cpu: CpuMetric
  memory: ResourceMetric
  storage: ResourceMetric
  cpu_package_temperature: TemperatureMetric
}
export type SystemMetricsState = {
  status: "initial_loading" | "ready" | "stale" | "stale_with_error" | "initial_error"
  refreshing: boolean
  snapshot: MetricsSnapshot | null
  error: string | null
  listener_error: string | null
  last_updated_ms: number | null
  age_ms: number | null
}

export function formatPercent(percent: number): string {
  return Number.isFinite(percent) && percent >= 0 && percent <= 100 ? `${Math.round(percent)}%` : "—"
}

export function formatBytes(bytes: number): string {
  if (!Number.isFinite(bytes) || bytes < 0) return "—"
  const units = ["B", "KB", "MB", "GB", "TB"]
  let value = bytes
  let index = 0
  while (value >= 1024 && index < units.length - 1) {
    value /= 1024
    index++
  }
  return `${value >= 100 || index === 0 ? Math.round(value) : value.toFixed(1)} ${units[index]}`
}

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null && !Array.isArray(value)
}

function exactKeys(value: Record<string, unknown>, expected: string[]): boolean {
  const actual = Object.keys(value).sort()
  return actual.length === expected.length && actual.every((key, index) => key === [...expected].sort()[index])
}

function finiteNumber(value: unknown): value is number {
  return typeof value === "number" && Number.isFinite(value)
}

function safeCount(value: unknown): value is number {
  return finiteNumber(value) && Number.isSafeInteger(value) && value >= 0
}

function reasonMetric<State extends "warming_up" | "unavailable">(
  value: Record<string, unknown>,
  state: State,
): { state: State; reason: string } {
  if (!exactKeys(value, ["state", "reason"]) || value.state !== state || typeof value.reason !== "string" || !value.reason) {
    throw new Error(`invalid ${state} metric`)
  }
  return { state, reason: value.reason }
}

function decodeCpu(value: unknown): CpuMetric {
  if (!isRecord(value) || typeof value.state !== "string") throw new Error("invalid cpu metric")
  if (value.state === "warming_up" || value.state === "unavailable") return reasonMetric(value, value.state)
  if (
    value.state !== "available" ||
    !exactKeys(value, ["state", "percent", "sample_start_ms", "sample_end_ms"]) ||
    !finiteNumber(value.percent) ||
    value.percent < 0 ||
    value.percent > 100 ||
    !safeCount(value.sample_start_ms) ||
    !safeCount(value.sample_end_ms) ||
    value.sample_end_ms <= value.sample_start_ms
  ) {
    throw new Error("invalid available cpu metric")
  }
  return {
    state: "available",
    percent: value.percent,
    sample_start_ms: value.sample_start_ms,
    sample_end_ms: value.sample_end_ms,
  }
}

function decodeResource(value: unknown, name: string): ResourceMetric {
  if (!isRecord(value) || typeof value.state !== "string") throw new Error(`invalid ${name} metric`)
  if (value.state === "unavailable") return reasonMetric(value, "unavailable")
  if (
    value.state !== "available" ||
    !exactKeys(value, ["state", "total_bytes", "used_bytes", "available_bytes", "sampled_at_ms"]) ||
    !safeCount(value.total_bytes) ||
    value.total_bytes === 0 ||
    !safeCount(value.used_bytes) ||
    !safeCount(value.available_bytes) ||
    !safeCount(value.sampled_at_ms) ||
    value.used_bytes > value.total_bytes ||
    value.available_bytes !== value.total_bytes - value.used_bytes
  ) {
    throw new Error(`invalid available ${name} metric`)
  }
  return {
    state: "available",
    total_bytes: value.total_bytes,
    used_bytes: value.used_bytes,
    available_bytes: value.available_bytes,
    sampled_at_ms: value.sampled_at_ms,
  }
}

const TEMPERATURE_PROVENANCE: readonly TemperatureProvenance[] = ["verified_cpu_package", "apple_soc_die_max"]

function isTemperatureProvenance(value: unknown): value is TemperatureProvenance {
  return typeof value === "string" && (TEMPERATURE_PROVENANCE as readonly string[]).includes(value)
}

function decodeTemperature(value: unknown): TemperatureMetric {
  if (!isRecord(value) || typeof value.state !== "string") throw new Error("invalid temperature metric")
  if (value.state === "unavailable") return reasonMetric(value, "unavailable")
  if (
    value.state !== "available" ||
    !exactKeys(value, ["state", "celsius", "sampled_at_ms", "provenance", "adapter_id"]) ||
    !finiteNumber(value.celsius) ||
    value.celsius < -273.15 ||
    value.celsius > 200 ||
    !safeCount(value.sampled_at_ms) ||
    !isTemperatureProvenance(value.provenance) ||
    typeof value.adapter_id !== "string" ||
    !value.adapter_id
  ) {
    throw new Error("invalid available temperature metric")
  }
  return {
    state: "available",
    celsius: value.celsius,
    sampled_at_ms: value.sampled_at_ms,
    provenance: value.provenance,
    adapter_id: value.adapter_id,
  }
}

export function decodeMetricsSnapshot(value: unknown): MetricsSnapshot {
  if (
    !isRecord(value) ||
    !exactKeys(value, ["schema_version", "sampled_at_ms", "cpu", "memory", "storage", "cpu_package_temperature"]) ||
    value.schema_version !== 1 ||
    !safeCount(value.sampled_at_ms)
  ) {
    throw new Error("unsupported system metrics snapshot")
  }
  return {
    schema_version: 1,
    sampled_at_ms: value.sampled_at_ms,
    cpu: decodeCpu(value.cpu),
    memory: decodeResource(value.memory, "memory"),
    storage: decodeResource(value.storage, "storage"),
    cpu_package_temperature: decodeTemperature(value.cpu_package_temperature),
  }
}

const REQUEST_TIMEOUT_MS = 2500

const initial: SystemMetricsState = {
  status: "initial_loading",
  refreshing: false,
  snapshot: null,
  error: null,
  listener_error: null,
  last_updated_ms: null,
  age_ms: null,
}
let state = initial
const listeners = new Set<() => void>()
let active = false
let generation = 0
let timer: ReturnType<typeof setInterval> | null = null
let staleTimer: ReturnType<typeof setTimeout> | null = null
let unlisten: (() => void) | null = null
let current: { id: number; promise: Promise<void> } | null = null
let requestId = 0
let trailing = false

function emit() {
  for (const listener of listeners) listener()
}
function setState(next: Partial<SystemMetricsState>) {
  state = { ...state, ...next }
  emit()
}
export function subscribe(listener: () => void) {
  listeners.add(listener)
  return () => listeners.delete(listener)
}
export function getSnapshot() {
  return state
}

function scheduleStaleDeadline(token: number) {
  if (staleTimer) clearTimeout(staleTimer)
  if (!active || state.last_updated_ms == null) return
  const delay = Math.max(0, state.last_updated_ms + 6001 - Date.now())
  staleTimer = setTimeout(() => {
    staleTimer = null
    if (!active || token !== generation || !state.snapshot || state.last_updated_ms == null) return
    const age_ms = Math.max(0, Date.now() - state.last_updated_ms)
    if (age_ms > 6000) setState({ status: state.error ? "stale_with_error" : "stale", age_ms })
  }, delay)
}

async function invokeSnapshot(): Promise<MetricsSnapshot> {
  const { invoke } = await import("@tauri-apps/api/core")
  return decodeMetricsSnapshot(await invoke<unknown>("get_system_metrics"))
}

function withDeadline<Value>(promise: Promise<Value>): Promise<Value> {
  return new Promise((resolve, reject) => {
    const timeout = setTimeout(() => reject(new Error("system_metrics_request_timeout")), REQUEST_TIMEOUT_MS)
    promise.then(
      (value) => {
        clearTimeout(timeout)
        resolve(value)
      },
      (error) => {
        clearTimeout(timeout)
        reject(error)
      },
    )
  })
}

function errorMessage(error: unknown): string {
  return error instanceof Error ? error.message : String(error)
}

function doRefresh(token: number): Promise<void> {
  if (!active || token !== generation) return Promise.resolve()
  const hadData = state.snapshot !== null
  setState({
    status: hadData ? state.status : "initial_loading",
    refreshing: true,
  })
  return withDeadline(invokeSnapshot())
    .then((snapshot) => {
      if (!active || token !== generation) return
      setState({
        status: "ready",
        refreshing: false,
        snapshot,
        error: null,
        last_updated_ms: Date.now(),
        age_ms: 0,
      })
      scheduleStaleDeadline(token)
    })
    .catch((error) => {
      if (!active || token !== generation) return
      const age_ms = state.last_updated_ms == null ? null : Math.max(0, Date.now() - state.last_updated_ms)
      setState({
        status: hadData ? "stale_with_error" : "initial_error",
        refreshing: false,
        error: errorMessage(error),
        age_ms,
      })
    })
}

/** Request a refresh; overlapping calls coalesce and permit one trailing refresh. */
export function refresh(): Promise<void> {
  if (!active) return Promise.resolve()
  if (current) {
    trailing = true
    return current.promise
  }
  const token = generation
  const id = ++requestId
  const promise = doRefresh(token).finally(() => {
    if (current?.id !== id) return
    current = null
    if (trailing && active) {
      trailing = false
      void refresh()
    } else {
      trailing = false
    }
  })
  current = { id, promise }
  return promise
}

async function attachOpenListener(token: number) {
  if (!isTauri()) return
  try {
    const { listen } = await import("@tauri-apps/api/event")
    const stop = await listen("sayknow:open", () => void refresh())
    if (!active || token !== generation) stop()
    else {
      unlisten = stop
      setState({ listener_error: null })
    }
  } catch (error) {
    if (!active || token !== generation) return
    setState({ listener_error: errorMessage(error) })
  }
}

export function setActive(next: boolean) {
  if (next === active) return
  active = next
  generation++
  const token = generation
  if (!next) {
    if (timer) {
      clearInterval(timer)
      timer = null
    }
    if (staleTimer) {
      clearTimeout(staleTimer)
      staleTimer = null
    }
    trailing = false
    if (unlisten) {
      const stop = unlisten
      unlisten = null
      stop()
    }
    state = { ...state, refreshing: false, status: state.snapshot ? state.status : "initial_loading" }
    emit()
    return
  }
  const age_ms = state.last_updated_ms == null ? null : Math.max(0, Date.now() - state.last_updated_ms)
  const status = state.snapshot && age_ms != null && age_ms > 6000
    ? state.error ? "stale_with_error" : "stale"
    : state.status
  setState({ status, refreshing: true, age_ms })
  scheduleStaleDeadline(token)
  timer = setInterval(() => {
    if (state.last_updated_ms != null) {
      const nextAge = Math.max(0, Date.now() - state.last_updated_ms)
      setState({
        status: nextAge > 6000 && state.snapshot
          ? state.error ? "stale_with_error" : "stale"
          : state.status,
        age_ms: nextAge,
      })
    }
    void refresh()
  }, 3000)
  void attachOpenListener(token)
  void refresh()
}

export function resetForTests() {
  active = false
  generation++
  if (timer) clearInterval(timer)
  if (staleTimer) clearTimeout(staleTimer)
  if (unlisten) unlisten()
  timer = null
  staleTimer = null
  unlisten = null
  state = initial
  current = null
  requestId++
  trailing = false
  emit()
}
