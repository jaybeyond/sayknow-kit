import { describe, expect, it } from "vitest"
import { UI_LOCALES, UI_STRINGS } from "./strings"

const REQUIRED_METRIC_KEYS = [
  "tools.metrics.title",
  "tools.metrics.cpu",
  "tools.metrics.cpuSystem",
  "tools.metrics.cpuUser",
  "tools.metrics.cpuIdle",
  "tools.metrics.memory",
  "tools.metrics.storage",
  "tools.metrics.temperature",
  "tools.metrics.battery",
  "tools.metrics.charging",
  "tools.metrics.notCharging",
  "tools.metrics.notInstalled",
  "tools.metrics.powerSource",
  "tools.metrics.maxCapacity",
  "tools.metrics.cycleCount",
  "tools.metrics.batteryTemperature",
  "tools.metrics.network",
  "tools.metrics.localIp",
  "tools.metrics.upload",
  "tools.metrics.download",
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
describe("display power copy", () => {
  it("describes WindowServer disconnect, not DDC standby, in every locale", () => {
    for (const locale of UI_LOCALES) {
      const note = UI_STRINGS[locale]["tools.brightness.ddcNote"]
      expect(note, `${locale}:tools.brightness.ddcNote`).toBeTruthy()
      expect(note.toLowerCase()).toContain("lunar")
    }
  })
})
describe("tray quit label", () => {
  it("exists in every locale, because the tray's right-click menu shows it", () => {
    for (const locale of UI_LOCALES) {
      const quit = UI_STRINGS[locale]["tray.quit"]
      expect(quit, `${locale}:tray.quit`).toBeTruthy()
    }
  })
})
describe("about page links", () => {
  it("labels every external account link in every locale", () => {
    for (const locale of UI_LOCALES) {
      for (const key of ["settings.about.repo", "settings.about.openrouter", "settings.about.deepl"]) {
        expect(UI_STRINGS[locale][key], `${locale}:${key}`).toBeTruthy()
      }
    }
  })
})
