import {
  cancelMoleAppRemoval, checkFullDiskAccess, cleanRunLog, detectMole, listMoleApps, parseAnalyze,
  parseCleanPreview, parseCleanRun, parseMaintenance, parseResult, previewMoleAppRemoval, runMoleAction,
  stripAnsi, trashMoleAppSelection,
  type AppInventory, type AppRemovalOutcome, type AppRemovalPreview, type AppSummary,
  type CleanedItem, type CleanPreviewItem, type MaintenanceTask, type MoleAnalyze, type MoleInfo, type MoleResult,
} from "./mole"
import { holdReload } from "./idle-reload"

export type SessionId = "disk" | "cache" | "tune"
export type SessionState = {
  progress: string[]
  items: CleanPreviewItem[]
  analyze: MoleAnalyze | null
  maintenance: MaintenanceTask[]
  scanResult: MoleResult | null
  result: MoleResult | null
  /** Rows Mole reported as deleted by the last real clean, and that run's own log. */
  cleaned: CleanedItem[]
  runLog: string[]
  maintenanceResult: MaintenanceTask[]
  error: string | null
  updatedAt: number | null
  lastRunAt: number | null
  stale: boolean
}
export type MoleStore = {
  info: MoleInfo | null | "loading"
  detectionError: string | null
  /** Mole inherits this app's Full Disk Access; null when it could not be determined. */
  fullDiskAccess: boolean | null
  initialized: boolean
  refreshing: boolean
  sessions: Record<SessionId, SessionState>
  apps: { inventory: AppInventory | null; error: string | null; updatedAt: number | null; stale: boolean }
  busy: SessionId | "apps" | "detect" | "preview" | "remove" | null
  selectedApp: AppSummary | null
  preview: AppRemovalPreview | null
  previewError: string | null
  removalResult: AppRemovalOutcome | null
  removalError: string | null
}
const emptySession = (): SessionState => ({
  progress: [], items: [], analyze: null, maintenance: [], scanResult: null,
  result: null, cleaned: [], runLog: [], maintenanceResult: [], error: null, updatedAt: null, lastRunAt: null, stale: false,
})
let state: MoleStore = {
  info: "loading", detectionError: null, fullDiskAccess: null, initialized: false, refreshing: false,
  sessions: { disk: emptySession(), cache: emptySession(), tune: emptySession() },
  apps: { inventory: null, error: null, updatedAt: null, stale: false },
  busy: null, selectedApp: null, preview: null, previewError: null, removalResult: null, removalError: null,
}
const listeners = new Set<() => void>()
let activeOperation: Promise<void> | null = null
let activeKind: "read" | "exclusive" | null = null
let previewEpoch = 0
function set(patch: Partial<MoleStore>) {
  state = { ...state, ...patch }
  for (const listener of listeners) listener()
}
function patchSession(id: SessionId, patch: Partial<SessionState>) {
  set({ sessions: { ...state.sessions, [id]: { ...state.sessions[id], ...patch } } })
}
export function subscribe(listener: () => void) {
  listeners.add(listener)
  return () => { listeners.delete(listener) }
}
export function getSnapshot(): MoleStore { return state }

/** Only duplicate read rounds coalesce. A write is never queued or mistaken for another operation. */
function admit(kind: "read" | "exclusive", operation: () => Promise<void>): Promise<void> {
  if (activeOperation) return kind === "read" && activeKind === "read" ? activeOperation : Promise.reject(new Error("mole_busy"))
  let resolve!: () => void
  let reject!: (error: unknown) => void
  const promise = new Promise<void>((ok, fail) => { resolve = ok; reject = fail })
  activeOperation = promise
  activeKind = kind
  const release = holdReload()
  set({ busy: kind === "read" ? "detect" : "preview" })
  const finish = () => {
    activeOperation = null
    activeKind = null
    release()
    set({ busy: null, refreshing: false })
  }
  void operation().then(
    () => { finish(); resolve() },
    (error) => { finish(); reject(error) },
  )
  return promise
}
function supported() {
  return state.info !== "loading" && state.info?.supported === true && !state.detectionError
}
function invalidatePreview() {
  previewEpoch++
  const token = state.preview?.token
  set({ preview: null, selectedApp: null, previewError: null })
  return token
}
async function discardPreview() {
  const token = invalidatePreview()
  if (token) await cancelMoleAppRemoval(token)
}
function markStale() {
  set({
    sessions: Object.fromEntries(Object.entries(state.sessions).map(([id, session]) => [id, { ...session, stale: true }])) as Record<SessionId, SessionState>,
    apps: { ...state.apps, stale: true },
  })
}
async function executeRun(id: SessionId, action: string) {
  const preview = action.endsWith("-preview") || action === "analyze"
  set({ busy: id })
  patchSession(id, { progress: [], error: null })
  let unlisten: (() => void) | undefined
  let acceptingLines = true
  const lines: string[] = []
  try {
    try {
      const { listen } = await import("@tauri-apps/api/event")
      unlisten = await listen<string>("mole:line", (event) => {
        const line = stripAnsi(event.payload).trim()
        if (!acceptingLines || !line || /^[{}[\]"]/.test(line)) return
        lines.push(line)
        if (lines.length > 40) lines.shift()
        // Progress must not replace the last complete list or its summary.
        patchSession(id, { progress: [...lines] })
      })
    } catch { /* A web preview has no native event bus; invocation still reports errors. */ }
    const outcome = await runMoleAction(action)
    if (!outcome.ok) throw new Error(outcome.stderr || "mole_failed")
    const text = `${outcome.stdout}\n${outcome.stderr}`
    if (action === "analyze") {
      const analyze = parseAnalyze(outcome.json)
      if (!analyze) throw new Error("mole_inventory_invalid")
      patchSession(id, { analyze })
    } else if (id === "cache") {
      if (preview) {
        const scanResult = parseResult(text, "preview")
        if (!scanResult) throw new Error("mole_inventory_invalid")
        patchSession(id, { items: parseCleanPreview(text), scanResult })
      } else {
        // The pre-clean estimate no longer describes the disk; the follow-up scan replaces it.
        patchSession(id, { result: parseResult(text, "clean"), cleaned: parseCleanRun(text), runLog: cleanRunLog(text), items: [], scanResult: null, updatedAt: null, lastRunAt: Date.now() })
      }
    } else if (id === "tune") {
      const maintenance = parseMaintenance(text, preview)
      if (!maintenance.length) throw new Error("mole_inventory_invalid")
      if (preview) patchSession(id, { maintenance })
      else patchSession(id, { maintenanceResult: maintenance, lastRunAt: Date.now() })
    }
    patchSession(id, preview ? { updatedAt: Date.now(), stale: false } : { stale: true })
  } catch (error) {
    patchSession(id, { error: String(error), stale: true })
  } finally {
    acceptingLines = false
    unlisten?.()
    if (preview) patchSession(id, { progress: [] })
  }
}
async function scanApps() {
  set({ busy: "apps", apps: { ...state.apps, error: null, stale: true } })
  try {
    const inventory = await listMoleApps()
    if (!inventory || !Array.isArray(inventory.apps) || typeof inventory.generation !== "string") throw new Error("mole_inventory_invalid")
    set({ apps: { inventory, error: null, updatedAt: Date.now(), stale: false } })
  } catch (error) {
    set({ apps: { ...state.apps, error: String(error), stale: true } })
  }
}
async function scanAll() {
  set({ initialized: true, refreshing: true, busy: "detect", detectionError: null })
  markStale()
  try {
    await discardPreview()
    set({ info: await detectMole() })
  } catch (error) {
    set({ detectionError: String(error) })
    return
  }
  set({ fullDiskAccess: await checkFullDiskAccess().catch(() => null) })
  if (!supported()) return
  await executeRun("disk", "analyze")
  await executeRun("cache", "clean-preview")
  await executeRun("tune", "optimize-preview")
  await scanApps()
}
export function initialize(): Promise<void> {
  if (state.initialized) return activeKind === "read" && activeOperation ? activeOperation : Promise.resolve()
  return refreshScans()
}
export function refreshScans(): Promise<void> { return admit("read", scanAll) }
export function run(id: SessionId, action: string): Promise<void> {
  const allowed: Record<SessionId, string[]> = { disk: ["analyze"], cache: ["clean-preview", "clean"], tune: ["optimize-preview", "optimize"] }
  if (!allowed[id].includes(action)) return Promise.reject(new Error("mole_invalid_selection"))
  const destructive = action === "clean" || action === "optimize"
  return admit("exclusive", async () => {
    if (!supported()) throw new Error("mole_not_installed")
    if (destructive && (state.sessions[id].stale || !state.sessions[id].updatedAt)) throw new Error("mole_scan_required")
    await discardPreview()
    if (destructive) markStale()
    await executeRun(id, action)
    // The list must describe the disk after the clean, not the preview taken before it.
    // A failed clean keeps its error on screen instead of being replaced by a scan.
    if (action === "clean" && !state.sessions.cache.error) {
      set({ refreshing: true })
      await executeRun("cache", "clean-preview")
    }
  })
}
export function openAppRemoval(app: AppSummary): Promise<void> {
  return admit("exclusive", async () => {
    const inventory = state.apps.inventory
    if (!supported() || state.apps.stale || !inventory || !inventory.apps.some((row) => row.id === app.id)) throw new Error("mole_inventory_stale")
    await discardPreview()
    const epoch = ++previewEpoch
    set({ busy: "preview", selectedApp: app, previewError: null })
    try {
      const preview = await previewMoleAppRemoval(app.id, inventory.generation)
      if (epoch !== previewEpoch) {
        await cancelMoleAppRemoval(preview.token)
        return
      }
      set({ preview })
    } catch (error) {
      if (epoch === previewEpoch) set({ previewError: String(error) })
    }
  })
}
export async function cancelRemoval(): Promise<void> {
  if (state.busy === "remove") throw new Error("mole_busy")
  await discardPreview()
}
export function removeSelected(selectedCandidateIds: string[]): Promise<void> {
  const preview = state.preview
  return admit("exclusive", async () => {
    if (!preview || state.preview?.token !== preview.token || Date.now() >= preview.expires_at_ms) throw new Error("mole_preview_stale")
    if (new Set(selectedCandidateIds).size !== selectedCandidateIds.length || selectedCandidateIds.some((id) => !preview.related.some((row) => row.id === id))) throw new Error("mole_invalid_selection")
    set({ busy: "remove", removalResult: null, removalError: null })
    previewEpoch++
    try {
      const removalResult = await trashMoleAppSelection(preview.token, selectedCandidateIds)
      set({ removalResult })
    } catch (error) {
      set({ removalError: String(error) })
    } finally {
      set({ preview: null, selectedApp: null, previewError: null })
      // Read-only reconciliation, never a retry of the destructive operation.
      await scanApps()
    }
  })
}
