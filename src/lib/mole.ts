import { invoke } from "@tauri-apps/api/core"
import { isTauri } from "./runtime"

export type MoleInfo = { path: string; version: string; supported: boolean; required_version: string }
export type MoleRun = { command: string; ok: boolean; stdout: string; stderr: string; json: unknown | null }
export type MoleDiskEntry = { name: string; path: string; size: number; is_dir?: boolean; insight?: boolean; cleanable?: boolean }
export type MoleAnalyze = { path?: string; overview?: boolean; total_size?: number; total_files?: number; entries: MoleDiskEntry[]; large_files: MoleDiskEntry[] }
export type CleanPreviewItem = {
  id: string
  section: string
  name: string
  detail: string
  bytes: number | null
  status: "candidate" | "manual" | "skipped" | "informational" | "unknown"
}
export type MaintenanceTask = {
  id: string
  name: string
  details: string[]
  status: "preview" | "completed" | "unchanged" | "skipped" | "admin_skipped" | "manual" | "failed" | "unknown"
}
export type MoleResult = {
  mode: "preview" | "clean"
  bytes: number | null
  items: number | null
  partial: boolean
}
/** One row Mole printed as done during a real clean. */
export type CleanedItem = { id: string; section: string; name: string; detail: string; bytes: number | null }
export type HistorySession = { command: string; started_at: string; items: number; size: string; removed: number; trashed: number }
export type AppSummary = { id: string; name: string; bundle_id: string; path: string; resolved_path: string | null; size_label: string; source: string; blocked_reason: string | null }
export type AppInventory = { generation: string; apps: AppSummary[] }
export type RemovalKind = "app" | "shortcut" | "cache" | "preferences" | "saved_state" | "webkit" | "http_storage" | "support" | "container"
export type RemovalCandidate = { id: string; kind: RemovalKind; path: string; size_bytes: number | null }
export type RemovalExclusion = { kind: RemovalKind; path: string; reason: string }
export type AppRemovalPreview = { token: string; generation: string; expires_at_ms: number; app: RemovalCandidate; shortcut: RemovalCandidate | null; related: RemovalCandidate[]; excluded: RemovalExclusion[]; running: boolean; needs_admin: boolean }
export type RemovalStatus = "moved" | "failed" | "unknown" | "not_attempted"
export type RemovalItemResult = { candidate_id: string; kind: RemovalKind; path: string; status: RemovalStatus; error: string | null; trash_path: string | null }
export type AppRemovalOutcome = { items: RemovalItemResult[]; stopped_reason: string | null }

export async function detectMole(): Promise<MoleInfo | null> {
  if (!isTauri()) return null
  return invoke<MoleInfo | null>("detect_mole")
}
export const runMoleAction = (action: string) => invoke<MoleRun>("run_mole_action", { action })
export const listMoleApps = () => invoke<AppInventory>("list_mole_apps")
export const previewMoleAppRemoval = (appId: string, generation: string) =>
  invoke<AppRemovalPreview>("preview_mole_app_removal", { appId, generation })
export const cancelMoleAppRemoval = (previewToken: string) =>
  invoke<void>("cancel_mole_app_removal", { previewToken })
export const trashMoleAppSelection = (previewToken: string, selectedCandidateIds: string[]) =>
  invoke<AppRemovalOutcome>("trash_mole_app_selection", { previewToken, selectedCandidateIds })
/** Whether this app (and so every Mole child it runs) has Full Disk Access; null when it cannot be told. */
export async function checkFullDiskAccess(): Promise<boolean | null> {
  if (!isTauri()) return null
  return invoke<boolean | null>("mole_full_disk_access")
}
export const openFullDiskAccessSettings = () => invoke<void>("open_full_disk_access_settings")

export function asRecord(value: unknown): Record<string, unknown> | null {
  return typeof value === "object" && value !== null && !Array.isArray(value) ? value as Record<string, unknown> : null
}
function nonnegative(value: unknown): value is number {
  return typeof value === "number" && Number.isFinite(value) && value >= 0
}
export function parseAnalyze(value: unknown): MoleAnalyze | null {
  const rec = asRecord(value)
  if (!rec || !Array.isArray(rec.entries)) return null
  return {
    path: typeof rec.path === "string" ? rec.path : undefined,
    overview: rec.overview === true,
    total_size: nonnegative(rec.total_size) ? rec.total_size : undefined,
    total_files: nonnegative(rec.total_files) ? rec.total_files : undefined,
    entries: rec.entries.flatMap(diskEntry),
    large_files: Array.isArray(rec.large_files) ? rec.large_files.flatMap(diskEntry) : [],
  }
}
function diskEntry(value: unknown): MoleDiskEntry[] {
  const rec = asRecord(value)
  if (!rec || typeof rec.name !== "string" || !nonnegative(rec.size)) return []
  return [{ name: rec.name, path: typeof rec.path === "string" ? rec.path : rec.name, size: rec.size, is_dir: rec.is_dir === true, insight: rec.insight === true, cleanable: rec.cleanable === true }]
}
export function stripAnsi(text: string): string {
  /* eslint-disable no-control-regex */
  return text.replace(/\u001B\[[0-?]*[ -/]*[@-~]/g, "")
    .replace(/\u001B\][^\u0007]*(?:\u0007|\u001B\\)/g, "")
    .replace(/\r/g, "\n")
  /* eslint-enable no-control-regex */
}
const SIZE = /(\d+(?:\.\d+)?)\s*(TiB|GiB|MiB|KiB|TB|GB|MB|KB|Ti|Gi|Mi|Ki|B)\b/i
export function parseSizeToBytes(text: string): number | null {
  const match = text.match(SIZE)
  if (!match) return null
  const unit = match[2].toUpperCase()
  const power = { T: 4, G: 3, M: 2, K: 1, B: 0 }[unit[0] as "T" | "G" | "M" | "K" | "B"]
  const bytes = Math.round(Number(match[1]) * (unit.includes("I") ? 1024 : 1000) ** power)
  return Number.isSafeInteger(bytes) && bytes >= 0 ? bytes : null
}

/** Section context and row status matter: manual-review examples are not cleanup savings. */
export function parseCleanPreview(text: string): CleanPreviewItem[] {
  const items = new Map<string, CleanPreviewItem>()
  let section = ""
  for (const raw of stripAnsi(text).split("\n")) {
    const line = raw.trim()
    if (line.startsWith("➤")) { section = line.slice(1).trim(); continue }
    const match = line.match(/^([✓◎→●○•*])\s+(.+)$/)
    if (!match || !section) continue
    const [, icon, body] = match
    if (/^(whitelist|system caches need sudo)/i.test(body)) continue
    const status: CleanPreviewItem["status"] = /manual review|review only|potential orphan|candidates.*sampled/i.test(body)
      ? "manual"
      : /skipped|skip\b|already empty|nothing to clean|not available|unavailable|no incomplete|no common/i.test(body)
        ? "skipped"
        : /\b(?:total|subtotal)\s*[:：]/i.test(body)
          ? "informational"
          : icon === "→" || (icon === "✓" && /\bitems?\b/.test(body) && SIZE.test(body))
            ? "candidate" : icon === "✓" || icon === "•" ? "informational" : "unknown"
    const name = body.split(/[·,]/)[0].replace(/\s+\d+\s+(?:old\s+)?items?$/i, "").trim()
    const id = `${section}:${name}`
    items.set(id, { id, section, name, detail: body, bytes: parseSizeToBytes(body), status })
  }
  return [...items.values()]
}

/** Mode comes from the invoked action, never an optimistic word such as 'complete'. */
export function parseResult(text: string, mode: MoleResult["mode"]): MoleResult | null {
  const lines = stripAnsi(text).split("\n").map((line) => line.trim())
  const summary = lines.find((line) => mode === "preview" ? /^Potential space:/i.test(line) : /^(Space freed|Tracked cleanup):/i.test(line))
  if (summary) {
    const count = summary.match(/\bItems(?: cleaned)?:\s*(\d+)/i)
    const bytes = parseSizeToBytes(summary.split("|")[0])
    if (mode === "clean" || bytes !== null || count) {
      return { mode, bytes, items: count ? Number(count[1]) : null, partial: false }
    }
  }
  // An execution without an explicit summary cannot borrow preview or free-disk figures.
  if (mode === "clean") return { mode, bytes: lines.some((line) => /no additional space freed/i.test(line)) ? 0 : null, items: null, partial: false }
  const candidates = parseCleanPreview(text).filter((item) => item.status === "candidate")
  if (!candidates.length) return null
  const measured = candidates.filter((item) => item.bytes !== null)
  return { mode, bytes: measured.length ? measured.reduce((sum, item) => sum + item.bytes!, 0) : null, items: null, partial: true }
}

/** A ✓ row is a deletion only when Mole did not say it skipped, found, or had nothing to do. */
// "removed 3, skipped 2 protected" still removed something; "· skipped (whitelist)" did not.
const NOT_CLEANED = /(?:·|,)\s*skipped(?:\s*\(|\s+whitelist|$)|already|nothing to clean|\bfound\b|^no\b|^great\b|^whitelist|admin access|protected items|\bwould\b|\bdry\b/i
export function parseCleanRun(text: string): CleanedItem[] {
  const rows: CleanedItem[] = []
  let section = ""
  for (const raw of stripAnsi(text).split("\n")) {
    const line = raw.trim()
    if (line.startsWith("➤")) { section = line.slice(1).trim(); continue }
    const body = line.match(/^✓\s+(.+)$/)?.[1]
    if (!body || !section || NOT_CLEANED.test(body)) continue
    const name = body.split(/[·,]/)[0].replace(/\s+\d+\s+(?:old\s+)?items?$/i, "").trim()
    rows.push({ id: `${section}:${name}:${rows.length}`, section, name, detail: body, bytes: parseSizeToBytes(body) })
  }
  return rows
}
/** Only Mole's own row and summary lines; spinner frames and terminal residue are dropped. */
export function cleanRunLog(text: string): string[] {
  return stripAnsi(text).split("\n").map((line) => line.trim())
    .filter((line) => /^(?:➤|[✓◎→●○•☞])\s|^(?:Space freed|System was already clean|Free space now)/.test(line))
    .slice(-300)
}

export const MAINTENANCE_IDS: Record<string, string> = {
  "DNS & Spotlight Check": "dnsSpotlight",
  "Finder Cache Refresh": "finderCache",
  "App State Cleanup": "appState",
  "Broken Config Repair": "brokenConfig",
  "Network Cache Refresh": "networkCache",
  "Database Optimization": "database",
  "LaunchServices Repair": "launchServices",
  "Font Cache Rebuild": "fonts",
  "Dock Refresh": "dock",
  "Prevent Finder .DS_Store": "finderMetadata",
  "Memory Optimization": "memory",
  "Network Stack Refresh": "networkStack",
  "Permission Repair": "permissions",
  "Bluetooth Refresh": "bluetooth",
  "Spotlight Optimization": "spotlight",
  "Periodic Maintenance": "periodic",
  "Shared File Lists": "sharedLists",
  "Disk Health": "diskHealth",
  "Login Items": "loginItems",
  "Quarantine Database Cleanup": "quarantine",
  "Launch Agents Cleanup": "launchAgents",
  Notifications: "notifications",
  "Usage Data": "usageData",
}
export function parseMaintenance(text: string, preview: boolean): MaintenanceTask[] {
  const output = stripAnsi(text)
  const isPreview = preview || /\bDRY RUN\b|\bWould apply \d+ optimizations/i.test(output)
  const tasks: MaintenanceTask[] = []
  let current: MaintenanceTask | undefined
  for (const raw of output.split("\n")) {
    const line = raw.trim()
    if (line.startsWith("➤")) {
      const name = line.slice(1).trim()
      current = { id: MAINTENANCE_IDS[name] ?? name, name, details: [], status: isPreview ? "preview" : "unknown" }
      tasks.push(current)
    } else if (current && /^[→✓◎•]\s/.test(line)) {
      current.details.push(line.replace(/^[→✓◎•]\s+/, ""))
    }
  }
  if (!isPreview) {
    for (const task of tasks) {
      const detail = task.details.join(" ")
      const changed = task.details.some((line) =>
        !/\balready\b|\bwould\b|\bwill\b/i.test(line)
        && /repaired|refreshed|rebuilt|\bcleaned\b|\bcleared\b|released|restarted|\bflushed\b|\boptimized\b/i.test(line))
      task.status = /failed|error|could not/i.test(detail) ? "failed"
        : /requires sudo|admin access required/i.test(detail) ? "admin_skipped"
          : /manual review|review only|review manually/i.test(detail) ? "manual"
            : /skipped|\bskip\b|not found|not available/i.test(detail) ? "skipped"
              : changed ? "completed"
                : /already|healthy|all.*valid|nothing|no .*needed/i.test(detail) ? "unchanged" : "unknown"
    }
  }
  return tasks
}

export function parseHistory(value: unknown): HistorySession[] {
  const rec = asRecord(value)
  const sessions = rec && Array.isArray(rec.sessions) ? rec.sessions : []
  return sessions.flatMap((item): HistorySession[] => {
    const row = asRecord(item)
    if (!row || typeof row.command !== "string") return []
    const actions = asRecord(row.actions)
    return [{ command: row.command, started_at: typeof row.started_at === "string" ? row.started_at : "", items: nonnegative(row.items) ? row.items : 0, size: typeof row.size === "string" ? row.size : "", removed: nonnegative(actions?.removed) ? actions.removed : 0, trashed: nonnegative(actions?.trashed) ? actions.trashed : 0 }]
  })
}
