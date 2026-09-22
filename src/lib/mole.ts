import { invoke } from "@tauri-apps/api/core"
import { isTauri } from "./runtime"

export type MoleInfo = {
  path: string
  version: string
}

export type MoleRun = {
  command: string
  ok: boolean
  stdout: string
  stderr: string
  json: unknown | null
}

export type MoleDiskEntry = {
  name: string
  path: string
  size: number
  is_dir?: boolean
  insight?: boolean
  cleanable?: boolean
}

export type MoleAnalyze = {
  path?: string
  overview?: boolean
  total_size?: number
  total_files?: number
  entries?: MoleDiskEntry[]
  large_files?: MoleDiskEntry[]
}

export type CleanPreviewItem = {
  name: string
  detail: string
  bytes: number | null
  skipped: boolean
}

export type HistorySession = {
  command: string
  started_at: string
  items: number
  size: string
  removed: number
  trashed: number
}
export type MoleResult = {
  heading: string
  freedBytes: number | null
  items: number | null
  extra: string[]
}

export async function detectMole(): Promise<MoleInfo | null> {
  if (!isTauri()) return null
  return invoke<MoleInfo | null>("detect_mole")
}

export async function runMoleAction(action: string): Promise<MoleRun> {
  return invoke<MoleRun>("run_mole_action", { action })
}


export function asRecord(value: unknown): Record<string, unknown> | null {
  return typeof value === "object" && value !== null && !Array.isArray(value)
    ? (value as Record<string, unknown>)
    : null
}

export function parseAnalyze(value: unknown): MoleAnalyze | null {
  const rec = asRecord(value)
  if (!rec) return null
  const entries = Array.isArray(rec.entries) ? rec.entries.flatMap(diskEntry) : []
  return {
    path: typeof rec.path === "string" ? rec.path : undefined,
    overview: rec.overview === true,
    total_size: typeof rec.total_size === "number" ? rec.total_size : undefined,
    total_files: typeof rec.total_files === "number" ? rec.total_files : undefined,
    entries,
    large_files: Array.isArray(rec.large_files) ? rec.large_files.flatMap(diskEntry) : [],
  }
}

function diskEntry(value: unknown): MoleDiskEntry[] {
  const rec = asRecord(value)
  if (!rec || typeof rec.name !== "string" || typeof rec.size !== "number") return []
  return [{
    name: rec.name,
    path: typeof rec.path === "string" ? rec.path : rec.name,
    size: rec.size,
    is_dir: rec.is_dir === true,
    insight: rec.insight === true,
    cleanable: rec.cleanable === true,
  }]
}

export function stripAnsi(text: string): string {
  return text
    .replace(/\u001B\[[0-9;]*[A-Za-z]/g, "")
    .replace(/\u001B\][^\u0007]*\u0007/g, "")
    .replace(/\r/g, "")
}

const SIZE = /(\d+(?:\.\d+)?)\s*(B|KB|MB|GB|TB|GiB|MiB|KiB|Gi|Mi|Ki)\b/i

export function parseSizeToBytes(text: string): number | null {
  const match = text.match(SIZE)
  if (!match) return null
  const n = Number(match[1])
  if (!Number.isFinite(n)) return null
  const unit = match[2].toUpperCase()
  const mul =
    unit.startsWith("T") ? 1e12 :
    unit.startsWith("G") ? 1e9 :
    unit.startsWith("M") ? 1e6 :
    unit.startsWith("K") ? 1e3 :
    1
  return Math.round(n * mul)
}

/** Mole 1.38 dry-run is TTY text, not JSON. Pull category rows with sizes. */
export function parseCleanPreview(text: string): CleanPreviewItem[] {
  const items: CleanPreviewItem[] = []
  const seen = new Set<string>()
  for (const raw of stripAnsi(text).split("\n")) {
    const line = raw.trim()
    if (!line || line.startsWith("↳") || /^(whitelist|system caches need sudo)/i.test(line)) continue
    const row = line.match(/^[✓◎→●○*\-|/\\]\s+(.+)$/)
    if (!row) continue
    const body = row[1]
    if (/scanning|cleaning old|need sudo|whitelist/i.test(body) && !SIZE.test(body)) continue
    const name = body.split(/[·,]/)[0]?.trim() || body
    const skipped = /skip|skipped|unchanged|busy|already empty/i.test(body)
    if (seen.has(name)) continue
    seen.add(name)
    items.push({
      name,
      detail: body,
      bytes: parseSizeToBytes(body),
      skipped,
    })
  }
  return items
}

export function parseHistory(value: unknown): HistorySession[] {
  const rec = asRecord(value)
  const sessions = rec && Array.isArray(rec.sessions) ? rec.sessions : []
  const out: HistorySession[] = []
  for (const item of sessions) {
    const row = asRecord(item)
    if (!row || typeof row.command !== "string") continue
    const actions = asRecord(row.actions)
    out.push({
      command: row.command,
      started_at: typeof row.started_at === "string" ? row.started_at : "",
      items: typeof row.items === "number" ? row.items : 0,
      size: typeof row.size === "string" ? row.size : "",
      removed: typeof actions?.removed === "number" ? actions.removed : 0,
      trashed: typeof actions?.trashed === "number" ? actions.trashed : 0,
    })
  }
  return out
}

export function parseResult(text: string): MoleResult | null {
  const lines = stripAnsi(text).split("\n").map((line) => line.trim()).filter(Boolean)
  const heading = lines.find((line) =>
    /complete|완료|freed|would free|tracked cleanup/i.test(line),
  )
  const sized = parseCleanPreview(text)
  const previewBytes = sized.reduce((sum, item) => sum + (item.bytes ?? 0), 0)
  if (!heading && previewBytes <= 0) return null
  const joined = lines.join(" ")
  const cleaned = joined.match(/Items(?: cleaned)?:?\s*(\d+)/i)
  const itemMatches = [...joined.matchAll(/(\d+)\s+(?:old\s+)?items/gi)]
  const items = cleaned
    ? Number(cleaned[1])
    : itemMatches.reduce((sum, m) => sum + Number(m[1]), 0)
  const extra = lines.filter((line) =>
    /free space|categories|applied|unchanged|skipped|unavailable|dry run mode/i.test(line),
  )
  return {
    heading: heading ?? (joined.toLowerCase().includes("dry run") ? "Dry run" : "Cleanup"),
    freedBytes: previewBytes > 0 ? previewBytes : parseSizeToBytes(joined),
    items: items > 0 ? items : sized.length || null,
    extra: extra.slice(0, 4),
  }
}
