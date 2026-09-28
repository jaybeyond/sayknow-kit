import { describe, expect, it } from "vitest"

import { monitorConfig } from "./system-monitor"

const t = (key: string) => `[${key}]`

describe("monitorConfig", () => {
  it("passes the choice through with alert text in the user's language", () => {
    const config = monitorConfig("gpu", ["battery", "cpu"], t)
    expect(config.readout).toBe("gpu")
    // Always in the canonical order, whatever order they were switched on.
    expect(config.alerts).toEqual(["cpu", "battery"])
    expect(config.text.cpu).toEqual({ title: "[monitor.alert.cpu.title]", body: "[monitor.alert.cpu.body]" })
    expect(Object.keys(config.text).sort()).toEqual(["battery", "cpu", "memory", "storage", "temperature"])
  })

  it("never sends a value the Rust side would reject", () => {
    // Prefs are read back from storage and may come from another build.
    expect(monitorConfig("disk", ["cpu", "fans", 3], t)).toMatchObject({ readout: "off", alerts: ["cpu"] })
    expect(monitorConfig(undefined, "cpu", t)).toMatchObject({ readout: "off", alerts: [] })
  })
})
