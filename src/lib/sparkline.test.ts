import { describe, expect, it } from "vitest"

import { historyWindow, sparklinePath } from "./sparkline"
import { decodeHistory, decodeTopProcesses, mergeHistory, type HistoryPoint } from "./system-activity-store"

const MIN = 60_000

describe("historyWindow", () => {
  it("shows at least ten minutes and at most an hour, ending at the newest sample", () => {
    expect(historyWindow([])).toBeNull()
    expect(historyWindow([1_000, 61_000])).toEqual({ from: 61_000 - 10 * MIN, to: 61_000, minutes: 10 })
    expect(historyWindow([0, 25 * MIN])).toEqual({ from: 0, to: 25 * MIN, minutes: 25 })
    expect(historyWindow([0, 3 * 60 * MIN])?.minutes).toBe(60)
  })
})

describe("sparklinePath", () => {
  it("maps time to x and value to y, clamped to the axis", () => {
    const path = sparklinePath(
      [
        { at: 0, value: 0 },
        { at: 50, value: 50 },
        { at: 100, value: 150 },
      ],
      0,
      100,
      100,
      1_000,
    )
    expect(path).toBe("M0.00 24.00 L50.00 12.00 L100.00 0.00")
  })

  it("breaks the line at a missing reading and at a gap instead of inventing one", () => {
    const path = sparklinePath(
      [
        { at: 0, value: 10 },
        { at: 10, value: null },
        { at: 20, value: 10 },
        { at: 30, value: 10 },
        { at: 90, value: 10 },
      ],
      0,
      100,
      100,
      30,
    )
    expect(path.split(" ").filter((p) => p.startsWith("M"))).toHaveLength(3)
    expect(path).toContain("L30.00")
  })

  it("draws nothing for an empty axis", () => {
    expect(sparklinePath([{ at: 5, value: 1 }], 10, 10, 100, 30)).toBe("")
    expect(sparklinePath([{ at: 5, value: 1 }], 0, 10, 0, 30)).toBe("")
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
