/** @vitest-environment jsdom */
import { afterEach, describe, expect, it, vi } from "vitest"
import { cleanup, fireEvent, render, screen, waitFor, within } from "@testing-library/react"
import type { Settings } from "@/hooks/useSettings"

const mocks = vi.hoisted(() => ({
  refreshMetrics: vi.fn(),
  setMetricsActive: vi.fn(),
  scanDisplays: vi.fn(() => Promise.resolve()),
  syncBuiltin: vi.fn(() => Promise.resolve()),
  toolsState: { displays: [], error: null, loaded: true },
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
    })[key] ?? key,
  }),
}))
vi.mock("@/lib/tools-store", () => ({
  getSnapshot: () => mocks.toolsState,
  subscribe: () => () => undefined,
  scanDisplays: mocks.scanDisplays,
  syncBuiltin: mocks.syncBuiltin,
}))
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
