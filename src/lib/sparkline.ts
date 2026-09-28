// Geometry for the status panel's small history graphs. Pure, so the shape a
// run of samples turns into can be tested without rendering anything.

export type Sample = { at: number; value: number | null }

const MIN_SPAN_MS = 10 * 60 * 1000
const MAX_SPAN_MS = 60 * 60 * 1000

/** The time axis for a history: ending at the newest sample and reaching back
 *  as far as there is data, between ten minutes and an hour. */
export function historyWindow(times: number[]): { from: number; to: number; minutes: number } | null {
  if (times.length === 0) return null
  const to = times[times.length - 1]
  const span = Math.min(MAX_SPAN_MS, Math.max(MIN_SPAN_MS, to - times[0]))
  return { from: to - span, to, minutes: Math.round(span / 60_000) }
}

/** An SVG path through `samples` on a `from`–`to` time axis and a `0`–`max`
 *  value axis, in a `width`×`height` box with y growing downward. A missing
 *  reading, or two samples further apart than `gapMs`, starts a new segment:
 *  a line drawn across a gap would invent readings nobody took. */
export function sparklinePath(
  samples: Sample[],
  from: number,
  to: number,
  max: number,
  gapMs: number,
  width = 100,
  height = 24,
): string {
  if (to <= from || max <= 0) return ""
  const x = (at: number) => ((at - from) / (to - from)) * width
  const y = (value: number) => height - (Math.min(max, Math.max(0, value)) / max) * height
  const parts: string[] = []
  let previous: number | null = null
  for (const sample of samples) {
    if (sample.at < from || sample.at > to) continue
    if (sample.value === null) {
      previous = null
      continue
    }
    const command = previous !== null && sample.at - previous <= gapMs ? "L" : "M"
    parts.push(`${command}${x(sample.at).toFixed(2)} ${y(sample.value).toFixed(2)}`)
    previous = sample.at
  }
  return parts.join(" ")
}
