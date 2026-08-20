// Local subscription-agent usage, read from each CLI's own session logs by
// the Rust side. Rust hands back UTC hour buckets ("YYYY-MM-DDTHH") so the
// cache stays timezone-independent; folding those into local-time windows is
// this module's job.

export type Bucket = {
  calls: number
  input: number
  output: number
  cache_read: number
  cache_write: number
  total: number
  cost_usd: number
}

export type AgentReport = {
  id: string
  label: string
  /** The agent's data directory exists. `false` = never used on this machine. */
  detected: boolean
  /** The agent's logs carry a real price. When false, never render money. */
  has_cost: boolean
  hours: Record<string, Bucket>
  /** model -> "YYYY-MM-DD" -> tokens. */
  model_days: Record<string, Record<string, number>>
  last_ts: string | null
  files: number
}

export const EMPTY_BUCKET: Bucket = {
  calls: 0,
  input: 0,
  output: 0,
  cache_read: 0,
  cache_write: 0,
  total: 0,
  cost_usd: 0,
}

function add(a: Bucket, b: Bucket): Bucket {
  return {
    calls: a.calls + b.calls,
    input: a.input + b.input,
    output: a.output + b.output,
    cache_read: a.cache_read + b.cache_read,
    cache_write: a.cache_write + b.cache_write,
    total: a.total + b.total,
    cost_usd: a.cost_usd + b.cost_usd,
  }
}

/** "2026-08-19T16" -> epoch ms at the start of that UTC hour. */
export function hourKeyToMs(key: string): number {
  return Date.parse(`${key}:00:00Z`)
}

/** Sum every bucket whose UTC hour starts at or after `sinceMs`. */
export function sumSince(hours: Record<string, Bucket>, sinceMs: number): Bucket {
  let acc = EMPTY_BUCKET
  for (const [key, b] of Object.entries(hours)) {
    if (hourKeyToMs(key) >= sinceMs) acc = add(acc, b)
  }
  return acc
}

/** Sum the buckets falling on `nowMs`'s local calendar day. */
export function sumToday(hours: Record<string, Bucket>, nowMs: number): Bucket {
  const start = new Date(nowMs)
  start.setHours(0, 0, 0, 0)
  return sumSince(hours, start.getTime())
}

export type Windows = {
  /** Rolling 5 hours — the shape of Claude's subscription window. */
  session: Bucket
  today: Bucket
  week: Bucket
  month: Bucket
}

/** All windows are measured from `nowMs` — pass the scan timestamp so a
 *  render is a pure function of its inputs and every card agrees on "now". */
export function windowsOf(hours: Record<string, Bucket>, nowMs: number): Windows {
  const now = nowMs
  return {
    session: sumSince(hours, now - 5 * 3600_000),
    today: sumToday(hours, now),
    week: sumSince(hours, now - 7 * 86_400_000),
    month: sumSince(hours, now - 30 * 86_400_000),
  }
}

/** Per-hour totals for the last `count` hours, oldest first — sparkline input. */
export function recentHours(
  hours: Record<string, Bucket>,
  nowMs: number,
  count = 24,
): { key: string; bucket: Bucket }[] {
  const now = new Date(nowMs)
  now.setMinutes(0, 0, 0)
  const out: { key: string; bucket: Bucket }[] = []
  for (let i = count - 1; i >= 0; i--) {
    const d = new Date(now.getTime() - i * 3600_000)
    const key = d.toISOString().slice(0, 13)
    out.push({ key, bucket: hours[key] ?? EMPTY_BUCKET })
  }
  return out
}

/** Model breakdown restricted to the last `days` local days, descending.
 *  Scoping matters: an agent's lifetime totals will happily show a model the
 *  user retired weeks ago, with a number larger than the window above it. */
export function modelsInWindow(
  modelDays: Record<string, Record<string, number>>,
  nowMs: number,
  days: number,
): { model: string; tokens: number }[] {
  const from = new Date(nowMs - days * 86_400_000)
  const limit = `${from.getFullYear()}-${String(from.getMonth() + 1).padStart(2, "0")}-${String(from.getDate()).padStart(2, "0")}`
  const out: { model: string; tokens: number }[] = []
  for (const [model, byDay] of Object.entries(modelDays)) {
    let tokens = 0
    for (const [day, n] of Object.entries(byDay)) {
      if (day >= limit) tokens += n
    }
    if (tokens > 0) out.push({ model, tokens })
  }
  out.sort((a, b) => b.tokens - a.tokens)
  return out
}

export function formatTokens(n: number): string {
  if (n < 1000) return `${n}`
  if (n < 1_000_000) return `${(n / 1000).toFixed(1)}K`
  if (n < 1_000_000_000) return `${(n / 1_000_000).toFixed(1)}M`
  return `${(n / 1_000_000_000).toFixed(2)}B`
}

export function formatUsd(n: number): string {
  if (n === 0) return "$0"
  if (n < 0.01) return `$${n.toFixed(4)}`
  if (n < 100) return `$${n.toFixed(2)}`
  return `$${Math.round(n).toLocaleString()}`
}
