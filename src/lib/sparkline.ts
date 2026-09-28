// Geometry for the status panel's small history graphs. Pure, so the shape a
// run of samples turns into can be tested without rendering anything.
//
// Each graph is a fixed strip of SLOTS sample positions, newest at the right
// edge, like Activity Monitor. A time axis (oldest sample to now) made the
// plot jump: history only grows while the panel or the monitor is sampling,
// so reopening after a gap squeezed a few minutes of readings into a sliver
// between long empty stretches, and every new sample rescaled the whole axis.

export type Sample = { at: number; value: number | null }

/** Sample positions per graph: at the 3 s refresh, the last six minutes. */
export const SLOTS = 120

/** The samples a graph shows: the newest `SLOTS`, oldest first. */
export function visibleSamples<T>(points: T[], slots = SLOTS): T[] {
  return points.length > slots ? points.slice(points.length - slots) : points
}

/** Whole minutes the shown samples span, at least one. */
export function spanMinutes(times: number[]): number {
  if (times.length < 2) return 1
  return Math.max(1, Math.round((times[times.length - 1] - times[0]) / 60_000))
}

/** An SVG path through `samples` in a `width`×`height` box (y down) with the
 *  newest sample on the right edge and one slot per sample, so the strip only
 *  ever scrolls left by one step. Values clamp to `0`–`max`. A missing reading,
 *  or two samples further apart than `gapMs` (the Mac slept, or nothing was
 *  sampling), starts a new segment: a line across a gap would invent readings. */
export function sparklinePath(
  samples: Sample[],
  max: number,
  gapMs: number,
  slots = SLOTS,
  width = 100,
  height = 24,
): string {
  if (max <= 0 || slots < 2) return ""
  const shown = visibleSamples(samples, slots)
  const offset = slots - shown.length
  const x = (i: number) => ((offset + i) / (slots - 1)) * width
  const y = (value: number) => height - (Math.min(max, Math.max(0, value)) / max) * height
  const parts: string[] = []
  let previous: number | null = null
  shown.forEach((sample, i) => {
    if (sample.value === null) {
      previous = null
      return
    }
    const command = previous !== null && sample.at - previous <= gapMs ? "L" : "M"
    parts.push(`${command}${x(i).toFixed(2)} ${y(sample.value).toFixed(2)}`)
    previous = sample.at
  })
  return parts.join(" ")
}
