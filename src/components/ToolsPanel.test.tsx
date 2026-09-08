/** @vitest-environment jsdom */
import { afterEach, describe, expect, it, vi } from "vitest"
import { cleanup, fireEvent, render, screen, waitFor, within } from "@testing-library/react"
import type { Settings } from "@/hooks/useSettings"

const mocks = vi.hoisted(() => ({
  refreshMetrics: vi.fn(),
  setMetricsActive: vi.fn(),
  scanDisplays: vi.fn(() => Promise.resolve()),
  syncBuiltin: vi.fn(() => Promise.resolve()),
  refreshAccessibility: vi.fn(() => Promise.resolve()),
  requestAccessibility: vi.fn(() => Promise.resolve()),
  relaunchApp: vi.fn(() => Promise.resolve()),
  resetAccessibility: vi.fn(() => Promise.resolve()),
  reportError: vi.fn(),
  invoke: vi.fn(() => Promise.resolve()),
  toolsState: {
    displays: [] as Array<Record<string, unknown>>,
    error: null,
    loaded: true,
    accessibility: null as { trusted: boolean; translocated: boolean; adhoc: boolean } | null,
  },
  metricsState: {
    status: "stale_with_error" as const,
    refreshing: true,
    snapshot: {
      schema_version: 1 as const,
      sampled_at_ms: 1_000,
      cpu: { state: "available" as const, percent: 42.6, sample_start_ms: 500, sample_end_ms: 1_000 },
      memory: { state: "available" as const, total_bytes: 2_048, used_bytes: 1_024, available_bytes: 1_024, sampled_at_ms: 1_000 },
      storage: { state: "unavailable" as const, reason: "system_volume_unavailable" },
      cpu_package_temperature: { state: "unavailable" as const, reason: "no_verified_package_sensor" },
    },
    error: "collection_timeout",
    listener_error: null,
    last_updated_ms: 1_000,
    age_ms: 7_000,
  },
}))

vi.mock("@/lib/runtime", () => ({ isTauri: () => true }))
vi.mock("@tauri-apps/api/event", () => ({ listen: vi.fn(async () => vi.fn()) }))
vi.mock("@/i18n", () => ({
  useT: () => ({
    t: (key: string) => ({
      "tools.desktopOnly": "Desktop only",
      "tools.heading": "Tools",
      "tools.refresh": "Refresh",
      "tools.metrics.title": "System status",
      "tools.metrics.cpu": "CPU",
      "tools.metrics.memory": "Memory",
      "tools.metrics.storage": "Storage",
      "tools.metrics.temperature": "CPU temperature",
      "tools.metrics.warming": "Warming up",
      "tools.metrics.unavailable": "Unavailable",
      "tools.metrics.temperatureUnavailable": "No verified CPU package sensor",
      "tools.metrics.error": "Error",
      "tools.metrics.stale": "Stale",
      "tools.metrics.retry": "Retry",
      "tools.metrics.updated": "Updated {age}",
      "tools.metrics.seconds": "{count}s",
      "tools.metrics.refreshing": "Refreshing",
      "tools.metrics.listenerError": "Listener unavailable",
      "tools.metrics.loading": "Loading system status",
      "tools.brightness.title": "Brightness",
      "tools.brightness.body": "Brightness controls",
      "tools.brightness.none": "No display",
      "tools.brightness.ddcNote": "DDC note",
      "tools.brightness.axTitle": "Accessibility permission required",
      "tools.brightness.axBody": "Allow SayKnow Kit in Accessibility",
      "tools.brightness.axTranslocated": "Move SayKnow Kit to Applications",
      "tools.brightness.axGrant": "Request permission",
      "tools.brightness.axRestart": "Restart the app",
      "tools.brightness.axAdhoc": "Ad-hoc build: reset the entry",
      "tools.brightness.axReset": "Reset and ask again",
      "tools.brightness.powerOn": "Power on",
      "tools.brightness.powerOff": "Power off",
      "tools.brightness.softwareDim": "software dim",
      "tools.brightness.external": "external",
      "tools.brightness.backlight": "Backlight",
      "tools.brightness.builtinUnsupported": "This Mac cannot drive the built-in display",
      "tools.brightness.externalUnsupported": "This monitor answers neither DDC brightness control nor software dimming",
    })[key] ?? key,
  }),
}))
vi.mock("@/lib/tools-store", () => ({
  getSnapshot: () => mocks.toolsState,
  subscribe: () => () => undefined,
  scanDisplays: mocks.scanDisplays,
  syncBuiltin: mocks.syncBuiltin,
  refreshAccessibility: mocks.refreshAccessibility,
  requestAccessibility: mocks.requestAccessibility,
  relaunchApp: mocks.relaunchApp,
  resetAccessibility: mocks.resetAccessibility,
  reportError: mocks.reportError,
}))
vi.mock("@tauri-apps/api/core", () => ({ invoke: mocks.invoke }))
vi.mock("@/lib/system-metrics-store", () => ({
  getSnapshot: () => mocks.metricsState,
  subscribe: () => () => undefined,
  refresh: mocks.refreshMetrics,
  setActive: mocks.setMetricsActive,
  formatBytes: (bytes: number) => `${bytes} B`,
  formatPercent: (percent: number) => `${Math.round(percent)}%`,
}))
vi.mock("@/components/UsagePanel", () => ({
  UsagePanel: ({ active }: { active: boolean }) => (
    <section aria-label="Usage" data-active={String(active)} />
  ),
}))

// Radix' slider measures its thumb; jsdom ships no ResizeObserver.
class ResizeObserverStub {
  observe() {}
  unobserve() {}
  disconnect() {}
}
globalThis.ResizeObserver ??= ResizeObserverStub as unknown as typeof ResizeObserver

import { ToolsPanel } from "./ToolsPanel"

afterEach(() => {
  cleanup()
  vi.clearAllMocks()
})

describe("ToolsPanel system metrics", () => {
  it("announces stale state without disguising unsupported temperature", async () => {
    render(<ToolsPanel settings={{ uiLocale: "en" } as Settings} active />)

    const region = screen.getByRole("region", { name: "System status" })
    expect(within(region).getByRole("status").textContent).toBe("Stale · Refreshing")
    const temperature = within(region).getByText("CPU temperature").parentElement
    expect(temperature?.textContent).toContain("No verified CPU package sensor")
    expect(temperature?.textContent).not.toContain("Stale")
    await waitFor(() => expect(mocks.setMetricsActive).toHaveBeenCalledWith(true))
  })

  it("retries metrics without forcing another display scan", async () => {
    render(<ToolsPanel settings={{ uiLocale: "en" } as Settings} active />)
    await waitFor(() => expect(mocks.scanDisplays).toHaveBeenCalledTimes(1))

    fireEvent.click(screen.getByRole("button", { name: "Retry" }))
    expect(mocks.refreshMetrics).toHaveBeenCalledOnce()
    expect(mocks.scanDisplays).toHaveBeenCalledTimes(1)
  })

  it("hosts usage below the brightness section and forwards visibility", () => {
    const { container } = render(<ToolsPanel settings={{ uiLocale: "en" } as Settings} active />)

    const usage = screen.getByRole("region", { name: "Usage" })
    expect(usage.dataset.active).toBe("true")

    const brightness = screen.getByText("Brightness").closest("section")
    expect(brightness).not.toBeNull()
    expect(
      brightness!.compareDocumentPosition(usage) & Node.DOCUMENT_POSITION_FOLLOWING,
    ).toBeTruthy()
    expect(container.querySelector('[aria-label="Usage"]')).toBe(usage)
  })
})

describe("ToolsPanel external monitor cards", () => {
  const external = (over: Record<string, unknown>) => ({
    id: "ddc:?:?:?:cg3",
    name: "ARZOPA",
    kind: "external",
    is_main: false,
    brightness: 40,
    power: true,
    power_capable: true,
    controllable: true,
    method: "ddc",
    system_level: 40,
    ...over,
  })

  afterEach(() => {
    mocks.toolsState.displays = []
    mocks.invoke.mockReset()
    mocks.invoke.mockResolvedValue(undefined)
    mocks.reportError.mockReset()
  })

  it("says what is wrong with an external the machine cannot drive", () => {
    mocks.toolsState.displays = [external({ controllable: false, method: "none", brightness: null })]
    render(<ToolsPanel settings={{ uiLocale: "en" } as Settings} active />)

    expect(screen.getByText(/answers neither DDC brightness control/)).toBeTruthy()
    expect(screen.queryByText(/cannot drive the built-in display/)).toBeNull()
  })

  it("does not offer a system backlight row for a software-dimmed external", () => {
    mocks.toolsState.displays = [external({ method: "gamma" })]
    render(<ToolsPanel settings={{ uiLocale: "en" } as Settings} active />)

    expect(screen.getByText("software dim")).toBeTruthy()
    // The backlight row is the built-in's F1/F2 base level; an external has none.
    expect(screen.queryByLabelText("ARZOPA Backlight")).toBeNull()
    expect(screen.getByLabelText("ARZOPA software dim")).toBeTruthy()
  })

  it("hides the power buttons only when the monitor never answered 0xD6", () => {
    mocks.toolsState.displays = [
      external({ method: "none", power: null, power_capable: false, controllable: false }),
    ]
    render(<ToolsPanel settings={{ uiLocale: "en" } as Settings} active />)

    // Power has no software equivalent, so a button here would do nothing.
    expect(screen.queryByTitle("Power on")).toBeNull()
    expect(screen.queryByTitle("Power off")).toBeNull()
  })

  it("keeps the power buttons on a monitor that answers power but not brightness", () => {
    // Power is 0xD6 and brightness is 0x10. A monitor that refuses the
    // luminance read still switches off and on, and gating these buttons on
    // the brightness method took the working feature away from it.
    mocks.toolsState.displays = [external({ method: "gamma", power: true })]
    render(<ToolsPanel settings={{ uiLocale: "en" } as Settings} active />)

    expect(screen.getByTitle("Power on")).toBeTruthy()
    expect(screen.getByTitle("Power off")).toBeTruthy()
  })

  it("keeps the power buttons through a DDC blackout", () => {
    // DDC on a live desk goes quiet for minutes at a time. Gating the buttons
    // on the live `power` read made them disappear mid-session from a monitor
    // that had been switching on and off all day, and the user reasonably read
    // that as the app losing the feature.
    mocks.toolsState.displays = [
      external({ method: "gamma", power: null, power_capable: true, brightness: null }),
    ]
    render(<ToolsPanel settings={{ uiLocale: "en" } as Settings} active />)

    expect(screen.getByTitle("Power on")).toBeTruthy()
    expect(screen.getByTitle("Power off")).toBeTruthy()
  })

  it("says so when the monitor refuses a power command", async () => {
    // This used to be swallowed: the toggle sprang back to its old position
    // and the user was left pressing a button that said nothing.
    mocks.invoke.mockRejectedValue("MacOS kernel I/O error: 268435459")
    mocks.toolsState.displays = [external({})]
    render(<ToolsPanel settings={{ uiLocale: "en" } as Settings} active />)

    fireEvent.click(screen.getByTitle("Power off"))

    await waitFor(() => {
      expect(mocks.reportError).toHaveBeenCalledWith("MacOS kernel I/O error: 268435459")
    })
  })

  it("keeps two identity-less monitors as two separate cards", () => {
    mocks.toolsState.displays = [
      external({ id: "ddc:?:?:?:cg3", name: "ARZOPA" }),
      external({ id: "ddc:?:?:?:cg7", name: "ARZOPA", brightness: 80 }),
    ]
    render(<ToolsPanel settings={{ uiLocale: "en" } as Settings} active />)

    expect(screen.getAllByLabelText("ARZOPA Brightness")).toHaveLength(2)
  })
})

describe("ToolsPanel accessibility notice", () => {
  const builtinOnGamma = {
    id: "builtin",
    name: "",
    kind: "builtin",
    is_main: true,
    brightness: 50,
    power: null,
    controllable: true,
    method: "gamma",
    system_level: 50,
  }

  afterEach(() => {
    mocks.toolsState.displays = []
    mocks.toolsState.accessibility = null
  })

  it("reads permission state instead of prompting on every visit", async () => {
    mocks.toolsState.displays = [builtinOnGamma]
    mocks.toolsState.accessibility = { trusted: false, translocated: false, adhoc: false }
    render(<ToolsPanel settings={{ uiLocale: "en" } as Settings} active />)

    await waitFor(() => expect(mocks.refreshAccessibility).toHaveBeenCalled())
    expect(mocks.requestAccessibility).not.toHaveBeenCalled()

    fireEvent.click(screen.getByRole("button", { name: "Request permission" }))
    expect(mocks.requestAccessibility).toHaveBeenCalledOnce()
  })

  it("offers a restart, because macOS only grants trust to a new process", () => {
    mocks.toolsState.displays = [builtinOnGamma]
    mocks.toolsState.accessibility = { trusted: false, translocated: false, adhoc: false }
    render(<ToolsPanel settings={{ uiLocale: "en" } as Settings} active />)

    fireEvent.click(screen.getByRole("button", { name: "Restart the app" }))
    expect(mocks.relaunchApp).toHaveBeenCalledOnce()
  })

  it("keeps re-reading trust while the notice is up", async () => {
    vi.useFakeTimers()
    try {
      mocks.toolsState.displays = [builtinOnGamma]
      mocks.toolsState.accessibility = { trusted: false, translocated: false, adhoc: false }
      render(<ToolsPanel settings={{ uiLocale: "en" } as Settings} active />)
      const initial = mocks.refreshAccessibility.mock.calls.length

      await vi.advanceTimersByTimeAsync(4100)
      expect(mocks.refreshAccessibility.mock.calls.length).toBeGreaterThan(initial)
    } finally {
      vi.useRealTimers()
    }
  })

  it("explains a stale grant when the build is ad-hoc signed", () => {
    mocks.toolsState.displays = [builtinOnGamma]
    mocks.toolsState.accessibility = { trusted: false, translocated: false, adhoc: true }
    render(<ToolsPanel settings={{ uiLocale: "en" } as Settings} active />)

    expect(screen.getByText("Ad-hoc build: reset the entry")).toBeTruthy()
    cleanup()

    mocks.toolsState.accessibility = { trusted: false, translocated: false, adhoc: false }
    render(<ToolsPanel settings={{ uiLocale: "en" } as Settings} active />)
    expect(screen.queryByText("Ad-hoc build: reset the entry")).toBeNull()
  })

  it("clears the stale entry in-app so no terminal is needed", () => {
    mocks.toolsState.displays = [builtinOnGamma]
    mocks.toolsState.accessibility = { trusted: false, translocated: false, adhoc: true }
    render(<ToolsPanel settings={{ uiLocale: "en" } as Settings} active />)

    fireEvent.click(screen.getByRole("button", { name: "Reset and ask again" }))
    expect(mocks.resetAccessibility).toHaveBeenCalledOnce()
    cleanup()

    // A certificate-signed build keeps its grant, so the reset is not offered.
    mocks.toolsState.accessibility = { trusted: false, translocated: false, adhoc: false }
    render(<ToolsPanel settings={{ uiLocale: "en" } as Settings} active />)
    expect(screen.queryByRole("button", { name: "Reset and ask again" })).toBeNull()
  })

  it("tells a translocated copy to move instead of offering a futile prompt", () => {
    mocks.toolsState.displays = [builtinOnGamma]
    mocks.toolsState.accessibility = { trusted: false, translocated: true, adhoc: true }
    render(<ToolsPanel settings={{ uiLocale: "en" } as Settings} active />)

    expect(screen.getByText("Move SayKnow Kit to Applications")).toBeTruthy()
    expect(screen.queryByRole("button", { name: "Request permission" })).toBeNull()
  })

  it("stays silent once the built-in backlight is actually controllable", () => {
    mocks.toolsState.displays = [{ ...builtinOnGamma, method: "backlight" }]
    mocks.toolsState.accessibility = { trusted: false, translocated: false, adhoc: false }
    render(<ToolsPanel settings={{ uiLocale: "en" } as Settings} active />)

    expect(screen.queryByText("Accessibility permission required")).toBeNull()
  })
})
