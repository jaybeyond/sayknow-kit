/** @vitest-environment jsdom */
import { afterEach, describe, expect, it, vi } from "vitest"
import { act, cleanup, fireEvent, render, screen, waitFor, within } from "@testing-library/react"
import type { Settings } from "@/hooks/useSettings"
import type { MoleStore } from "@/lib/mole-store"

const mocks = vi.hoisted(() => ({
  moleState: { busy: null as MoleStore["busy"] },
  moleListeners: new Set<() => void>(),
  refreshScans: vi.fn(() => Promise.resolve()),
  setActivityActive: vi.fn(),
  activityState: {
    points: [] as {
      at_ms: number
      cpu: number | null
      gpu: number | null
      memory: number | null
      temperature: number | null
      upload: number | null
      download: number | null
    }[],
    processes: null as
      | null
      | { state: "warming_up" }
      | {
          state: "available"
          by_cpu: { pid: number; name: string; cpu_percent: number; memory_bytes: number }[]
          by_memory: { pid: number; name: string; cpu_percent: number; memory_bytes: number }[]
        },
    error: null as string | null,
  },
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
      schema_version: 2 as const,
      sampled_at_ms: 1_000,
      cpu: { state: "available" as const, percent: 42.6, system_percent: 12.1, user_percent: 30.5, idle_percent: 57.4, sample_start_ms: 500, sample_end_ms: 1_000 },
      gpu: { state: "available" as const, percent: 33 },
      memory: { state: "available" as const, total_bytes: 2_048, used_bytes: 1_024, available_bytes: 1_024, sampled_at_ms: 1_000 },
      storage: { state: "unavailable" as const, reason: "system_volume_unavailable" },
      cpu_package_temperature: { state: "unavailable" as const, reason: "no_verified_package_sensor" },
      battery: { state: "available" as const, percent: 82, is_charging: true, adapter_name: "140W", max_capacity_percent: 95.7, cycle_count: 12, temperature_celsius: 30.2 },
      network: { state: "available" as const, interface: "en0", ip_address: "192.0.2.1", upload_bytes_per_sec: 50700, download_bytes_per_sec: 1700 },
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
      "tools.tabs.label": "Tool sections",
      "tools.tabs.status": "Status",
      "tools.tabs.display": "Displays",
      "tools.tabs.usage": "Token usage",
      "tools.tabs.mole": "Clean",
      "tools.refresh": "Refresh",
      "tools.metrics.title": "System status",
      "tools.metrics.cpu": "CPU",
      "tools.metrics.cpuSystem": "System",
      "tools.metrics.cpuUser": "User",
      "tools.metrics.cpuIdle": "Idle",
      "tools.metrics.gpu": "GPU",
      "tools.activity.history": "Last {minutes} min",
      "tools.activity.historyEmpty": "Collecting readings",
      "tools.activity.topCpu": "Top apps by CPU",
      "tools.activity.topMemory": "Top apps by memory",
      "tools.activity.measuring": "Measuring…",
      "tools.activity.none": "Nothing noticeable",
      "tools.metrics.memory": "Memory",
      "tools.metrics.storage": "Storage",
      "tools.metrics.temperature": "CPU temperature",
      "tools.metrics.battery": "Battery",
      "tools.metrics.charging": "Charging",
      "tools.metrics.notCharging": "On battery",
      "tools.metrics.notInstalled": "Not installed",
      "tools.metrics.powerSource": "Power source",
      "tools.metrics.maxCapacity": "Max capacity",
      "tools.metrics.cycleCount": "Cycles",
      "tools.metrics.batteryTemperature": "Battery temperature",
      "tools.metrics.network": "Network",
      "tools.metrics.localIp": "Local IP",
      "tools.metrics.upload": "Upload",
      "tools.metrics.download": "Download",
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
      "tools.brightness.all": "All displays",
      "tools.brightness.allHint": "Applies to every display below at once.",
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
  formatRate: (bytes: number) => `${bytes} B/s`,
}))
vi.mock("@/lib/system-activity-store", () => ({
  GAP_MS: 30_000,
  getSnapshot: () => mocks.activityState,
  subscribe: () => () => undefined,
  setActive: mocks.setActivityActive,
}))
vi.mock("@/lib/mole-store", () => ({
  getSnapshot: () => mocks.moleState,
  subscribe: (listener: () => void) => {
    mocks.moleListeners.add(listener)
    return () => { mocks.moleListeners.delete(listener) }
  },
  refreshScans: mocks.refreshScans,
}))
vi.mock("@/components/UsagePanel", () => ({
  UsagePanel: ({ active }: { active: boolean }) => (
    <section aria-label="Usage" data-active={String(active)} />
  ),
}))
vi.mock("@/components/MolePanel", () => ({
  MolePanel: ({ active }: { active: boolean }) => <section aria-label="Clean" data-active={String(active)} />,
}))

// Radix' slider measures its thumb; jsdom ships no ResizeObserver.
class ResizeObserverStub {
  observe() {}
  unobserve() {}
  disconnect() {}
}
globalThis.ResizeObserver ??= ResizeObserverStub as unknown as typeof ResizeObserver

import { ToolsPanel } from "./ToolsPanel"
import { brightnessCommand } from "@/lib/brightness-command"

function openDisplayTab() {
  fireEvent.click(screen.getByRole("tab", { name: "Displays" }))
}

function openUsageTab() {
  fireEvent.click(screen.getByRole("tab", { name: "Token usage" }))
}

afterEach(() => {
  cleanup()
  vi.useRealTimers()
  mocks.toolsState.displays = []
  mocks.toolsState.accessibility = null
  vi.clearAllMocks()
  mocks.activityState.points = []
  mocks.activityState.processes = null
  mocks.scanDisplays.mockImplementation(() => Promise.resolve())
  mocks.moleState = { busy: null }
  mocks.moleListeners.clear()
  mocks.refreshScans.mockImplementation(() => Promise.resolve())
})

describe("ToolsPanel polling eligibility", () => {
  const builtin = { id: "builtin", name: "Built-in", kind: "builtin", method: "backlight", brightness: 50, system_level: 0.5, controllable: true, power: null, power_capable: false, is_main: true }

  it("stops metrics and process polling off status and when inactive", () => {
    const view = render(<ToolsPanel settings={{ uiLocale: "en" } as Settings} active />)
    expect(mocks.setMetricsActive).toHaveBeenLastCalledWith(true)
    expect(mocks.setActivityActive).toHaveBeenLastCalledWith(true)
    openUsageTab()
    expect(mocks.setMetricsActive).toHaveBeenLastCalledWith(false)
    expect(mocks.setActivityActive).toHaveBeenLastCalledWith(false)
    fireEvent.click(screen.getByRole("tab", { name: "Status" }))
    expect(mocks.setMetricsActive).toHaveBeenLastCalledWith(true)
    view.rerender(<ToolsPanel settings={{ uiLocale: "en" } as Settings} active={false} />)
    expect(mocks.setMetricsActive).toHaveBeenLastCalledWith(false)
    expect(mocks.setActivityActive).toHaveBeenLastCalledWith(false)
  })

  it("runs brightness ticks only on the visible built-in display tab", async () => {
    vi.useFakeTimers()
    mocks.toolsState.displays = [builtin]
    const view = render(<ToolsPanel settings={{ uiLocale: "en" } as Settings} active />)
    await act(async () => { await vi.advanceTimersByTimeAsync(1_000) })
    expect(mocks.syncBuiltin).not.toHaveBeenCalled()
    openDisplayTab()
    await act(async () => { await vi.advanceTimersByTimeAsync(1_000) })
    expect(mocks.syncBuiltin).toHaveBeenCalledTimes(4)
    openUsageTab()
    await act(async () => { await vi.advanceTimersByTimeAsync(1_000) })
    expect(mocks.syncBuiltin).toHaveBeenCalledTimes(4)
    openDisplayTab()
    await act(async () => { await vi.advanceTimersByTimeAsync(1_000) })
    expect(mocks.syncBuiltin).toHaveBeenCalledTimes(8)
    view.rerender(<ToolsPanel settings={{ uiLocale: "en" } as Settings} active={false} />)
    await act(async () => { await vi.advanceTimersByTimeAsync(1_000) })
    expect(mocks.syncBuiltin).toHaveBeenCalledTimes(8)
  })

  it("polls trust only while the built-in permission notice is visible", async () => {
    vi.useFakeTimers()
    mocks.toolsState.displays = [{ ...builtin, method: "gamma" }]
    mocks.toolsState.accessibility = { trusted: false, translocated: false, adhoc: false }
    const view = render(<ToolsPanel settings={{ uiLocale: "en" } as Settings} active />)
    await act(async () => { await vi.advanceTimersByTimeAsync(4_000) })
    expect(mocks.refreshAccessibility).not.toHaveBeenCalled()
    openDisplayTab()
    await act(async () => { await vi.advanceTimersByTimeAsync(2_000) })
    expect(mocks.refreshAccessibility).toHaveBeenCalledTimes(2)
    openUsageTab()
    await act(async () => { await vi.advanceTimersByTimeAsync(4_000) })
    expect(mocks.refreshAccessibility).toHaveBeenCalledTimes(2)
    openDisplayTab()
    await act(async () => { await vi.advanceTimersByTimeAsync(2_000) })
    expect(mocks.refreshAccessibility).toHaveBeenCalledTimes(4)
    view.rerender(<ToolsPanel settings={{ uiLocale: "en" } as Settings} active={false} />)
    await act(async () => { await vi.advanceTimersByTimeAsync(4_000) })
    expect(mocks.refreshAccessibility).toHaveBeenCalledTimes(4)
  })

  it("does not poll builtin brightness or trust for external-only displays", async () => {
    vi.useFakeTimers()
    mocks.toolsState.displays = [{ ...builtin, id: "external", kind: "external", method: "ddc" }]
    mocks.toolsState.accessibility = { trusted: false, translocated: false, adhoc: false }
    render(<ToolsPanel settings={{ uiLocale: "en" } as Settings} active />)
    openDisplayTab()
    await act(async () => { await vi.advanceTimersByTimeAsync(4_000) })
    expect(mocks.syncBuiltin).not.toHaveBeenCalled()
    expect(mocks.refreshAccessibility).toHaveBeenCalledTimes(1)
  })

  it("passes native window inactivity through to cleanup tools", () => {
    const view = render(<ToolsPanel settings={{ uiLocale: "en" } as Settings} active />)
    fireEvent.click(screen.getByRole("tab", { name: "Clean" }))
    expect(screen.getByRole("region", { name: "Clean" }).dataset.active).toBe("true")
    view.rerender(<ToolsPanel settings={{ uiLocale: "en" } as Settings} active={false} />)
    expect(screen.getByRole("region", { name: "Clean" }).dataset.active).toBe("false")
  })
})

describe("ToolsPanel cleanup header refresh", () => {
  it("refreshes idle Mole scans and keeps the header disabled until they finish", async () => {
    let finishScans!: () => void
    mocks.refreshScans.mockImplementation(() => new Promise<void>((resolve) => {
      finishScans = resolve
    }))
    render(<ToolsPanel settings={{ uiLocale: "en" } as Settings} active />)
    fireEvent.click(screen.getByRole("tab", { name: "Clean" }))
    const refresh = screen.getByTitle("Refresh") as HTMLButtonElement
    mocks.scanDisplays.mockClear()

    expect(refresh.disabled).toBe(false)
    fireEvent.click(refresh)
    expect(mocks.refreshScans).toHaveBeenCalledOnce()
    expect(mocks.scanDisplays).toHaveBeenCalledExactlyOnceWith(true)
    expect(mocks.refreshMetrics).toHaveBeenCalledOnce()
    expect(refresh.disabled).toBe(true)
    fireEvent.click(refresh)
    expect(mocks.refreshScans).toHaveBeenCalledOnce()

    await act(async () => { finishScans() })
    expect(refresh.disabled).toBe(false)
  })

  it.each(["disk", "cache", "tune", "apps", "detect", "preview", "remove"] as const)(
    "disables the header on a %s notification and re-enables it when Mole becomes idle",
    async (busy) => {
      render(<ToolsPanel settings={{ uiLocale: "en" } as Settings} active />)
      fireEvent.click(screen.getByRole("tab", { name: "Clean" }))
      const refresh = screen.getByTitle("Refresh") as HTMLButtonElement
      mocks.scanDisplays.mockClear()

      expect(refresh.disabled).toBe(false)
      act(() => {
        mocks.moleState = { busy }
        mocks.moleListeners.forEach((listener) => listener())
      })
      expect(refresh.disabled).toBe(true)
      fireEvent.click(refresh)
      expect(mocks.refreshScans).not.toHaveBeenCalled()
      expect(mocks.scanDisplays).not.toHaveBeenCalled()
      expect(mocks.refreshMetrics).not.toHaveBeenCalled()

      act(() => {
        mocks.moleState = { busy: null }
        mocks.moleListeners.forEach((listener) => listener())
      })
      expect(refresh.disabled).toBe(false)
      expect(mocks.refreshScans).not.toHaveBeenCalled()
      await act(async () => { fireEvent.click(refresh) })
      expect(mocks.refreshScans).toHaveBeenCalledOnce()
      expect(mocks.scanDisplays).toHaveBeenCalledExactlyOnceWith(true)
      expect(mocks.refreshMetrics).toHaveBeenCalledOnce()
    },
  )

  it("rejects refresh using the latest busy snapshot before a subscription notification", async () => {
    render(<ToolsPanel settings={{ uiLocale: "en" } as Settings} active />)
    fireEvent.click(screen.getByRole("tab", { name: "Clean" }))
    const refresh = screen.getByTitle("Refresh") as HTMLButtonElement
    mocks.scanDisplays.mockClear()

    // No notification: the rendered button still reflects the previous idle snapshot.
    mocks.moleState = { busy: "remove" }
    expect(refresh.disabled).toBe(false)
    await act(async () => { fireEvent.click(refresh) })
    expect(mocks.refreshScans).not.toHaveBeenCalled()
    expect(mocks.scanDisplays).not.toHaveBeenCalled()
    expect(mocks.refreshMetrics).not.toHaveBeenCalled()

    act(() => {
      mocks.moleState = { busy: null }
      mocks.moleListeners.forEach((listener) => listener())
    })
    expect(mocks.refreshScans).not.toHaveBeenCalled()
    expect(mocks.scanDisplays).not.toHaveBeenCalled()
    expect(mocks.refreshMetrics).not.toHaveBeenCalled()
    await act(async () => { fireEvent.click(refresh) })
    expect(mocks.refreshScans).toHaveBeenCalledOnce()
  })

  it("unsubscribes from Mole state when the parent unmounts", () => {
    const view = render(<ToolsPanel settings={{ uiLocale: "en" } as Settings} active />)
    expect(mocks.moleListeners.size).toBe(1)
    view.unmount()
    expect(mocks.moleListeners.size).toBe(0)
  })

  it.each(["Status", "Displays", "Token usage"])(
    "preserves %s header refresh while Mole is busy",
    async (tab) => {
      mocks.moleState = { busy: "remove" }
      render(<ToolsPanel settings={{ uiLocale: "en" } as Settings} active />)
      fireEvent.click(screen.getByRole("tab", { name: "Clean" }))
      const refresh = screen.getByTitle("Refresh") as HTMLButtonElement
      expect(refresh.disabled).toBe(true)

      fireEvent.click(screen.getByRole("tab", { name: tab }))
      mocks.scanDisplays.mockClear()
      expect(refresh.disabled).toBe(false)
      await act(async () => { fireEvent.click(refresh) })
      expect(mocks.scanDisplays).toHaveBeenCalledExactlyOnceWith(true)
      expect(mocks.refreshMetrics).toHaveBeenCalledOnce()
      expect(mocks.refreshScans).not.toHaveBeenCalled()
      expect(refresh.disabled).toBe(false)

      fireEvent.click(screen.getByRole("tab", { name: "Clean" }))
      expect(refresh.disabled).toBe(true)
    },
  )
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
  it("spins the header refresh control while a rescan is in flight", async () => {
    let resolveScan: () => void = () => {}
    mocks.scanDisplays.mockImplementation(
      () => new Promise<void>((resolve) => {
        resolveScan = resolve
      }),
    )
    render(<ToolsPanel settings={{ uiLocale: "en" } as Settings} active />)
    const refresh = screen.getByTitle("Refresh")
    fireEvent.click(refresh)
    await waitFor(() => {
      expect(refresh.querySelector("svg")?.getAttribute("class") ?? "").toMatch(/animate-spin/)
    })
    resolveScan()
    await waitFor(() => {
      expect(refresh.querySelector("svg")?.getAttribute("class") ?? "").not.toMatch(/animate-spin/)
    })
  })

  it("separates status, display controls, and usage into secondary tabs", () => {
    render(<ToolsPanel settings={{ uiLocale: "en" } as Settings} active />)

    expect(screen.getByRole("tab", { name: "Status" }).getAttribute("aria-selected")).toBe("true")
    expect(screen.getByRole("region", { name: "System status" })).toBeTruthy()
    expect(screen.queryByText("Brightness")).toBeNull()

    openDisplayTab()
    expect(screen.getByRole("tab", { name: "Displays" }).getAttribute("aria-selected")).toBe("true")
    expect(screen.getByText("Brightness")).toBeTruthy()
    expect(screen.queryByRole("region", { name: "Usage" })).toBeNull()

    openUsageTab()
    const usage = screen.getByRole("region", { name: "Usage" })
    expect(usage.dataset.active).toBe("true")
    expect(screen.getByRole("tab", { name: "Token usage" }).getAttribute("aria-selected")).toBe("true")
    fireEvent.click(screen.getByRole("tab", { name: "Clean" }))
    expect(screen.getByRole("tab", { name: "Clean" }).getAttribute("aria-selected")).toBe("true")
    expect(screen.getByRole("region", { name: "Clean" })).toBeTruthy()
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
    openDisplayTab()

    expect(screen.getByText(/answers neither DDC brightness control/)).toBeTruthy()
    expect(screen.queryByText(/cannot drive the built-in display/)).toBeNull()
  })

  it("does not offer a system backlight row for a software-dimmed external", () => {
    mocks.toolsState.displays = [external({ method: "gamma" })]
    render(<ToolsPanel settings={{ uiLocale: "en" } as Settings} active />)
    openDisplayTab()

    expect(screen.getByText("software dim")).toBeTruthy()
    // The backlight row is the built-in's F1/F2 base level; an external has none.
    expect(screen.queryByLabelText("ARZOPA Backlight")).toBeNull()
    expect(screen.getByLabelText("ARZOPA software dim")).toBeTruthy()
  })

  it("hides the power buttons only when neither path can drive the panel", () => {
    mocks.toolsState.displays = [
      external({ method: "none", power: null, power_capable: false, controllable: false }),
    ]
    render(<ToolsPanel settings={{ uiLocale: "en" } as Settings} active />)
    openDisplayTab()

    // No DDC answer and no software blackout path means a button here would
    // do nothing.
    expect(screen.queryByTitle("Power on")).toBeNull()
    expect(screen.queryByTitle("Power off")).toBeNull()
  })

  it("keeps the power buttons on a monitor that answers power but not brightness", () => {
    // Power is 0xD6 and brightness is 0x10. A monitor that refuses the
    // luminance read still switches off and on, and gating these buttons on
    // the brightness method took the working feature away from it.
    mocks.toolsState.displays = [external({ method: "gamma", power: true })]
    render(<ToolsPanel settings={{ uiLocale: "en" } as Settings} active />)
    openDisplayTab()

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
    openDisplayTab()

    expect(screen.getByTitle("Power on")).toBeTruthy()
    expect(screen.getByTitle("Power off")).toBeTruthy()
  })

  it("says so when the monitor refuses a power command", async () => {
    // This used to be swallowed: the toggle sprang back to its old position
    // and the user was left pressing a button that said nothing.
    mocks.invoke.mockRejectedValue("MacOS kernel I/O error: 268435459")
    mocks.toolsState.displays = [external({})]
    render(<ToolsPanel settings={{ uiLocale: "en" } as Settings} active />)
    openDisplayTab()

    fireEvent.click(screen.getByTitle("Power off"))

    await waitFor(() => {
      expect(mocks.reportError).toHaveBeenCalledWith("MacOS kernel I/O error: 268435459")
    })
  })

  it("reuses the cached display list after a power toggle", async () => {
    mocks.invoke.mockResolvedValue(undefined)
    mocks.toolsState.displays = [external({})]
    render(<ToolsPanel settings={{ uiLocale: "en" } as Settings} active />)
    openDisplayTab()

    fireEvent.click(screen.getByTitle("Power off"))

    await waitFor(() => {
      expect(mocks.invoke).toHaveBeenCalledWith("set_display_power", {
        id: "ddc:?:?:?:cg3",
        on: false,
      })
    })
    expect(mocks.scanDisplays).toHaveBeenCalledWith(false)
    expect(mocks.scanDisplays).not.toHaveBeenCalledWith(true)
  })

  it("keeps two identity-less monitors as two separate cards", () => {
    mocks.toolsState.displays = [
      external({ id: "ddc:?:?:?:cg3", name: "ARZOPA" }),
      external({ id: "ddc:?:?:?:cg7", name: "ARZOPA", brightness: 80 }),
    ]
    render(<ToolsPanel settings={{ uiLocale: "en" } as Settings} active />)
    openDisplayTab()

    expect(screen.getAllByLabelText("ARZOPA Brightness")).toHaveLength(2)
  })
})
describe("ToolsPanel all-displays slider", () => {
  const builtin = {
    id: "builtin",
    name: "",
    kind: "builtin" as const,
    is_main: true,
    brightness: 50,
    power: null,
    power_capable: false,
    controllable: true,
    method: "gamma" as const,
    system_level: 80,
  }
  const external = {
    id: "ddc:lg",
    name: "LG",
    kind: "external" as const,
    is_main: false,
    brightness: 40,
    power: true,
    power_capable: true,
    controllable: true,
    method: "ddc" as const,
    system_level: 40,
  }

  afterEach(() => {
    mocks.toolsState.displays = []
  })

  it("drives the built-in and an external through different mechanisms", () => {
    // The built-in panel has no DDC. Its slider must reach the real backlight
    // (the Control Center control the F1/F2 keys move), never the DDC/gamma
    // command: that only darkens the picture and is exactly how the built-in
    // slider went dead while externals kept working.
    expect(brightnessCommand(builtin)).toEqual({ command: "set_builtin_backlight" })
    expect(brightnessCommand({ ...builtin, method: "backlight" })).toEqual({
      command: "set_builtin_backlight",
    })
    expect(brightnessCommand(external)).toEqual({
      command: "set_display_brightness",
      id: "ddc:lg",
    })
  })

  it("skips a panel that cannot be driven at all", () => {
    expect(brightnessCommand({ ...external, controllable: false, method: "none" })).toBeNull()
  })

  it("offers the all-displays slider when built-in and external are both present", () => {
    mocks.toolsState.displays = [builtin, external]
    render(<ToolsPanel settings={{ uiLocale: "en" } as Settings} active />)
    openDisplayTab()
    expect(screen.getByLabelText("All displays")).toBeTruthy()
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
    openDisplayTab()

    await waitFor(() => expect(mocks.refreshAccessibility).toHaveBeenCalled())
    expect(mocks.requestAccessibility).not.toHaveBeenCalled()

    fireEvent.click(screen.getByRole("button", { name: "Request permission" }))
    expect(mocks.requestAccessibility).toHaveBeenCalledOnce()
  })

  it("offers a restart, because macOS only grants trust to a new process", () => {
    mocks.toolsState.displays = [builtinOnGamma]
    mocks.toolsState.accessibility = { trusted: false, translocated: false, adhoc: false }
    render(<ToolsPanel settings={{ uiLocale: "en" } as Settings} active />)
    openDisplayTab()

    fireEvent.click(screen.getByRole("button", { name: "Restart the app" }))
    expect(mocks.relaunchApp).toHaveBeenCalledOnce()
  })

  it("keeps re-reading trust while the notice is up", async () => {
    vi.useFakeTimers()
    try {
      mocks.toolsState.displays = [builtinOnGamma]
      mocks.toolsState.accessibility = { trusted: false, translocated: false, adhoc: false }
      render(<ToolsPanel settings={{ uiLocale: "en" } as Settings} active />)
      openDisplayTab()
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
    openDisplayTab()

    expect(screen.getByText("Ad-hoc build: reset the entry")).toBeTruthy()
    cleanup()

    mocks.toolsState.accessibility = { trusted: false, translocated: false, adhoc: false }
    render(<ToolsPanel settings={{ uiLocale: "en" } as Settings} active />)
    openDisplayTab()
    expect(screen.queryByText("Ad-hoc build: reset the entry")).toBeNull()
  })

  it("clears the stale entry in-app so no terminal is needed", () => {
    mocks.toolsState.displays = [builtinOnGamma]
    mocks.toolsState.accessibility = { trusted: false, translocated: false, adhoc: true }
    render(<ToolsPanel settings={{ uiLocale: "en" } as Settings} active />)
    openDisplayTab()

    fireEvent.click(screen.getByRole("button", { name: "Reset and ask again" }))
    expect(mocks.resetAccessibility).toHaveBeenCalledOnce()
    cleanup()

    // A certificate-signed build keeps its grant, so the reset is not offered.
    mocks.toolsState.accessibility = { trusted: false, translocated: false, adhoc: false }
    render(<ToolsPanel settings={{ uiLocale: "en" } as Settings} active />)
    openDisplayTab()
    expect(screen.queryByRole("button", { name: "Reset and ask again" })).toBeNull()
  })

  it("tells a translocated copy to move instead of offering a futile prompt", () => {
    mocks.toolsState.displays = [builtinOnGamma]
    mocks.toolsState.accessibility = { trusted: false, translocated: true, adhoc: true }
    render(<ToolsPanel settings={{ uiLocale: "en" } as Settings} active />)
    openDisplayTab()

    expect(screen.getByText("Move SayKnow Kit to Applications")).toBeTruthy()
    expect(screen.queryByRole("button", { name: "Request permission" })).toBeNull()
  })

  it("stays silent once the built-in backlight is actually controllable", () => {
    mocks.toolsState.displays = [{ ...builtinOnGamma, method: "backlight" }]
    mocks.toolsState.accessibility = { trusted: false, translocated: false, adhoc: false }
    render(<ToolsPanel settings={{ uiLocale: "en" } as Settings} active />)
    openDisplayTab()

    expect(screen.queryByText("Accessibility permission required")).toBeNull()
  })
})

describe("ToolsPanel activity", () => {
  const point = (at_ms: number, over: Partial<(typeof mocks.activityState.points)[number]> = {}) => ({
    at_ms,
    cpu: 20,
    gpu: null,
    memory: 60,
    temperature: null,
    upload: 100,
    download: 2_000,
    ...over,
  })

  it("polls history only while the status tab is on screen", () => {
    render(<ToolsPanel settings={{ uiLocale: "en" } as Settings} active />)
    expect(mocks.setActivityActive).toHaveBeenLastCalledWith(true)
    openDisplayTab()
    expect(mocks.setActivityActive).toHaveBeenLastCalledWith(false)
  })

  it("shows the GPU reading beside the CPU", () => {
    render(<ToolsPanel settings={{ uiLocale: "en" } as Settings} active />)
    const region = screen.getByRole("region", { name: "System status" })
    expect(within(region).getByText("GPU").parentElement?.textContent).toContain("33%")
  })

  it("draws a graph only for readings this Mac reported", () => {
    mocks.activityState.points = [point(0), point(5_000), point(10_000, { cpu: 35 })]
    render(<ToolsPanel settings={{ uiLocale: "en" } as Settings} active />)
    const region = screen.getByRole("region", { name: "Last 1 min" })
    expect(within(region).getByText("CPU")).toBeTruthy()
    expect(within(region).getByText("35%")).toBeTruthy()
    expect(within(region).getByText("Memory")).toBeTruthy()
    // No GPU statistics and no temperature sensor: no flat line posing as 0.
    expect(within(region).queryByText("GPU")).toBeNull()
    expect(within(region).queryByText("CPU temperature")).toBeNull()
    expect(region.querySelectorAll("svg path").length).toBeGreaterThanOrEqual(3)
  })

  it("says it is collecting until there are two readings to join", () => {
    mocks.activityState.points = [point(0)]
    render(<ToolsPanel settings={{ uiLocale: "en" } as Settings} active />)
    expect(screen.getByText("Collecting readings")).toBeTruthy()
  })

  it("lists the heaviest apps, with multi-core CPU above 100%", () => {
    mocks.activityState.processes = {
      state: "available",
      by_cpu: [{ pid: 1, name: "cargo", cpu_percent: 412.4, memory_bytes: 10 }],
      by_memory: [],
    }
    render(<ToolsPanel settings={{ uiLocale: "en" } as Settings} active />)
    const region = screen.getByRole("region", { name: "Last 1 min" })
    expect(within(region).getByText("cargo").parentElement?.textContent).toContain("412%")
    expect(within(region).getByText("Nothing noticeable")).toBeTruthy()
  })

  it("does not invent a list before the first CPU comparison", () => {
    mocks.activityState.processes = { state: "warming_up" }
    render(<ToolsPanel settings={{ uiLocale: "en" } as Settings} active />)
    expect(screen.getAllByText("Measuring…")).toHaveLength(2)
  })
})
