import { describe, expect, it } from "vitest"

import { windowIsCurrent, type RateWindow } from "./agent-usage"

const window = (over: Partial<RateWindow>): RateWindow => ({
  used_percent: 40,
  window_minutes: 300,
  resets_at: 0,
  resets_estimated: false,
  scope: null,
  ...over,
})

const NOW = Date.parse("2026-09-28T10:20:00Z")

describe("windowIsCurrent", () => {
  it("trusts a known reset time over the capture time", () => {
    const renewsLater = window({ resets_at: NOW / 1000 + 60 })
    const renewedAlready = window({ resets_at: NOW / 1000 - 60 })
    // Captured a day ago, but the window is still open: current.
    expect(windowIsCurrent(renewsLater, "2026-09-27T10:20:00Z", NOW)).toBe(true)
    // Captured a second ago, but the window has renewed: not current.
    expect(windowIsCurrent(renewedAlready, "2026-09-28T10:19:59Z", NOW)).toBe(false)
  })

  it("treats an unknown reset as current only within the window's own length", () => {
    // The Claude app's weekly window before any renewal was seen, and a
    // five-hour window with nothing used: no reset time either way.
    const week = window({ window_minutes: 10_080 })
    expect(windowIsCurrent(week, "2026-09-25T10:20:00Z", NOW)).toBe(true)
    expect(windowIsCurrent(week, "2026-09-21T10:19:00Z", NOW)).toBe(false)
    const session = window({ window_minutes: 300 })
    expect(windowIsCurrent(session, "2026-09-28T06:00:00Z", NOW)).toBe(true)
    expect(windowIsCurrent(session, "2026-09-28T05:00:00Z", NOW)).toBe(false)
  })

  it("never presents a days-old Codex log as a current quota", () => {
    const monthly = window({ window_minutes: 43_200, resets_at: NOW / 1000 + 15 * 86_400 })
    expect(windowIsCurrent(monthly, "2026-09-16T03:52:35Z", NOW, "session_log")).toBe(false)
    expect(windowIsCurrent(monthly, "2026-09-28T10:16:00Z", NOW, "session_log")).toBe(true)
    expect(windowIsCurrent(monthly, "2026-09-28T10:14:59Z", NOW, "session_log")).toBe(false)
  })

  it("ages out live Codex readings while preserving a recent observation", () => {
    const weekly = window({ window_minutes: 10_080, resets_at: NOW / 1000 + 86_400 })
    expect(windowIsCurrent(weekly, "2026-09-28T10:19:00Z", NOW, "live_api")).toBe(true)
    expect(windowIsCurrent(weekly, "2026-09-28T10:17:59Z", NOW, "live_api")).toBe(false)
    expect(windowIsCurrent(weekly, "2026-09-28T10:21:01Z", NOW, "live_api")).toBe(false)
  })

  it("never calls an undatable reading current", () => {
    expect(windowIsCurrent(window({}), "not a date", NOW)).toBe(false)
  })
})
