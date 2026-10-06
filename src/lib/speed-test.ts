export type SpeedTestResult = {
  /** Bits per second. */
  download_bps: number | null
  /** Bits per second. */
  upload_bps: number | null
  idle_latency_ms: number | null
  bytes_used: number | null
  interface: string | null
  server: string | null
}

/** Megabits per second, the unit every speed test and ISP plan uses. */
export function formatMbps(bps: number | null): string | null {
  if (bps == null || !Number.isFinite(bps) || bps < 0) return null
  const mbps = bps / 1_000_000
  return `${mbps >= 100 ? mbps.toFixed(0) : mbps.toFixed(1)} Mbps`
}

const KNOWN_ERRORS = ["busy", "cancelled", "timeout", "network_error", "unsupported"]

/** The translation key for a backend error; anything unrecognized is the generic one. */
export function speedErrorKey(error: unknown): string {
  const code = String(error).match(/\bspeed_([a-z_]+)\b/)?.[1]
  return `tools.speed.error.${code && KNOWN_ERRORS.includes(code) ? code : "failed"}`
}
