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

/** One live reading; the direction not yet measured reads 0. */
export type SpeedTestProgress = {
  elapsed_ms: number
  /** Bits per second. */
  download_bps: number
  /** Bits per second. */
  upload_bps: number
}

/** Megabits per second, the unit every speed test and ISP plan uses. */
export function formatMbps(bps: number | null): string | null {
  if (bps == null || !Number.isFinite(bps) || bps < 0) return null
  const mbps = bps / 1_000_000
  return `${mbps >= 100 ? mbps.toFixed(0) : mbps.toFixed(1)} Mbps`
}

/** Dial limits in Mbps; a dial grows to the next one when a reading passes it. */
const GAUGE_STEPS = [50, 100, 250, 500, 1000, 2500, 5000, 10000]

/** The dial's full-scale value for the fastest reading seen so far, in Mbps. */
export function gaugeScale(peakBps: number): number {
  const mbps = Number.isFinite(peakBps) && peakBps > 0 ? peakBps / 1_000_000 : 0
  return GAUGE_STEPS.find((step) => mbps <= step) ?? GAUGE_STEPS[GAUGE_STEPS.length - 1]
}

/** How full the dial is, 0–1, never past the end. */
export function gaugeFraction(bps: number, scaleMbps: number): number {
  if (!Number.isFinite(bps) || bps <= 0 || scaleMbps <= 0) return 0
  return Math.min(1, bps / 1_000_000 / scaleMbps)
}

const KNOWN_ERRORS = ["busy", "cancelled", "timeout", "network_error", "unsupported"]

/** The translation key for a backend error; anything unrecognized is the generic one. */
export function speedErrorKey(error: unknown): string {
  const code = String(error).match(/\bspeed_([a-z_]+)\b/)?.[1]
  return `tools.speed.error.${code && KNOWN_ERRORS.includes(code) ? code : "failed"}`
}
