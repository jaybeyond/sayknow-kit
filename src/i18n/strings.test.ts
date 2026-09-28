import { describe, expect, it } from "vitest"
import { UI_LOCALES, UI_STRINGS } from "./strings"

const REQUIRED_METRIC_KEYS = [
  "tools.metrics.title",
  "tools.metrics.cpu",
  "tools.metrics.cpuSystem",
  "tools.metrics.cpuUser",
  "tools.metrics.cpuIdle",
  "tools.metrics.gpu",
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
describe("system monitor copy", () => {
  it("names every readout and alert, and carries the reading into each notification", () => {
    const kinds = ["cpu", "memory", "temperature", "storage", "battery"]
    const readouts = ["off", "cpu", "memory", "gpu", "temperature"]
    for (const locale of UI_LOCALES) {
      const strings = UI_STRINGS[locale]
      for (const key of [
        "settings.section.monitor",
        "monitor.readout.label",
        "monitor.readout.desc",
        "monitor.readout.macOnly",
        "monitor.alerts.label",
        "monitor.alerts.desc",
        ...readouts.map((r) => `monitor.readout.${r}`),
        ...kinds.flatMap((k) => [`monitor.alert.${k}.label`, `monitor.alert.${k}.hint`, `monitor.alert.${k}.title`]),
      ]) {
        expect(strings[key], `${locale}:${key}`).toBeTruthy()
      }
      for (const kind of kinds) {
        // The Rust side fills in the reading; a body without the slot would
        // send an alert that never says how bad it is.
        expect(strings[`monitor.alert.${kind}.body`], `${locale}:${kind}`).toContain("{value}")
      }
    }
  })

  it("has the activity panel strings in every locale", () => {
    for (const locale of UI_LOCALES) {
      const strings = UI_STRINGS[locale]
      expect(strings["tools.activity.history"], locale).toContain("{minutes}")
      for (const key of ["historyEmpty", "topCpu", "topMemory", "measuring", "none", "cpuNote"]) {
        expect(strings[`tools.activity.${key}`], `${locale}:${key}`).toBeTruthy()
      }
    }
  })
})
