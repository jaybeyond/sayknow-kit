import { describe, expect, it } from "vitest"
import { UI_LOCALES, UI_STRINGS } from "./strings"

const REQUIRED_METRIC_KEYS = [
  "tools.metrics.title",
  "tools.metrics.cpu",
  "tools.metrics.memory",
  "tools.metrics.storage",
  "tools.metrics.temperature",
  "tools.metrics.warming",
  "tools.metrics.unavailable",
  "tools.metrics.error",
  "tools.metrics.stale",
  "tools.metrics.retry",
  "tools.metrics.updated",
  "tools.metrics.loading",
  "tools.metrics.refreshing",
  "tools.metrics.temperatureUnavailable",
  "tools.metrics.seconds",
  "tools.metrics.listenerError",
] as const

describe("system metric translations", () => {
  it("has the explicit nonempty contract in every locale", () => {
    for (const locale of UI_LOCALES) {
      const strings = UI_STRINGS[locale]
      for (const key of REQUIRED_METRIC_KEYS) {
        expect(strings[key], `${locale}:${key}`).toBeTruthy()
      }
      expect(strings["tools.metrics.updated"]).toContain("{age}")
      expect(strings["tools.metrics.seconds"]).toContain("{count}")
      expect(Object.keys(strings).filter((key) => key.startsWith("tools.metrics.")).sort()).toEqual(
        [...REQUIRED_METRIC_KEYS].sort(),
      )
    }
  })
})
