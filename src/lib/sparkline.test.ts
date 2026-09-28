import { describe, expect, it } from "vitest"

import { SLOTS, sparklinePath, spanMinutes, visibleSamples } from "./sparkline"
import { decodeHistory, decodeTopProcesses, mergeHistory, type HistoryPoint } from "./system-activity-store"

const MIN = 60_000

describe("visibleSamples and spanMinutes", () => {
  it("keeps the newest slots' worth and names the span they cover", () => {
    const many = Array.from({ length: SLOTS + 5 }, (_, i) => i)
    expect(visibleSamples(many)).toHaveLength(SLOTS)
    expect(visibleSamples(many)[0]).toBe(5)
    expect(visibleSamples([1, 2])).toEqual([1, 2])
    expect(spanMinutes([])).toBe(1)
    expect(spanMinutes([0, 20_000])).toBe(1)
    expect(spanMinutes([0, 6 * MIN])).toBe(6)
  })
})

describe("sparklinePath", () => {
  it("puts the newest sample on the right edge, one slot per sample", () => {
    const path = sparklinePath(
      [
        { at: 0, value: 0 },
        { at: 1, value: 50 },
        { at: 2, value: 150 },
      ],
      100,
      1_000,
      5,
    )
    // Five slots, three samples: they fill the last three, right-aligned.
    expect(path).toBe("M50.00 24.00 L75.00 12.00 L100.00 0.00")
  })

  it("moves one step left per new sample instead of rescaling the axis", () => {
    const a = sparklinePath([{ at: 0, value: 10 }, { at: 3, value: 10 }], 100, 30, 5)
    const b = sparklinePath([{ at: 0, value: 10 }, { at: 3, value: 10 }, { at: 6, value: 10 }], 100, 30, 5)
    expect(a).toBe("M75.00 21.60 L100.00 21.60")
    expect(b).toBe("M50.00 21.60 L75.00 21.60 L100.00 21.60")
  })

  it("breaks the line at a missing reading and at a gap instead of inventing one", () => {
    const path = sparklinePath(
      [
        { at: 0, value: 10 },
        { at: 10, value: null },
        { at: 20, value: 10 },
        { at: 30, value: 10 },
        { at: 900, value: 10 },
        { at: 910, value: 10 },
      ],
      100,
      30,
      6,
    )
    expect(path.split(" ").filter((p) => p.startsWith("M"))).toHaveLength(3)
    expect(path).toContain("L60.00")
  })

  it("draws nothing without a scale", () => {
    expect(sparklinePath([{ at: 5, value: 1 }], 0, 30)).toBe("")
  })
})

const point = (at_ms: number, cpu: number | null = 10): HistoryPoint => ({
  at_ms,
  cpu,
  gpu: null,
  memory: 50,
  temperature: null,
  upload: 0,
  download: 0,
})

describe("history store helpers", () => {
  it("drops malformed points rather than drawing guesses", () => {
    const decoded = decodeHistory([
      point(1),
      { ...point(2), cpu: "high" },
      { ...point(3), at_ms: 3.5 },
      { at_ms: 4 },
      point(5, null),
    ])
    expect(decoded.map((p) => p.at_ms)).toEqual([1, 5])
    expect(decodeHistory({ at_ms: 1 })).toEqual([])
  })

  it("merges in time order, one point per timestamp, within the span", () => {
    const merged = mergeHistory([point(1_000), point(2_000)], [point(2_000, 99), point(3_000)], 1_500)
    expect(merged.map((p) => [p.at_ms, p.cpu])).toEqual([
      [2_000, 99],
      [3_000, 10],
    ])
    const same = [point(1)]
    expect(mergeHistory(same, [])).toBe(same)
  })

  it("reads the top-process answer and refuses anything else", () => {
    expect(decodeTopProcesses({ state: "warming_up" })).toEqual({ state: "warming_up" })
    expect(
      decodeTopProcesses({
        state: "available",
        by_cpu: [{ pid: 7, name: "ChatGPT", cpu_percent: 43.5, memory_bytes: 1_402_552_320 }, { pid: "x" }],
        by_memory: [],
      }),
    ).toEqual({
      state: "available",
      by_cpu: [{ pid: 7, name: "ChatGPT", cpu_percent: 43.5, memory_bytes: 1_402_552_320 }],
      by_memory: [],
    })
    expect(decodeTopProcesses({ state: "available" })).toBeNull()
    expect(decodeTopProcesses(null)).toBeNull()
  })
})
