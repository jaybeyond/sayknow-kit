/** @vitest-environment jsdom */
import { afterEach, describe, expect, it, vi } from "vitest"
import { cleanup, fireEvent, render, screen } from "@testing-library/react"
import type { MoleStore } from "@/lib/mole-store"

const mocks = vi.hoisted(() => ({
  state: {
    info: { version: "1.38.1", path: "/opt/homebrew/bin/mo", supported: true, required_version: "1.38.1" },
    busy: "cache" as "disk" | "cache" | "tune" | null,
    sessions: {
      disk: { progress: [], items: [], analyze: null, result: null, error: null, lastAction: null },
      cache: { progress: ["Cleaning cache"], items: [], analyze: null, result: null, error: null, lastAction: "clean" },
      tune: { progress: [], items: [], analyze: null, result: null, error: null, lastAction: null },
    },
  } as MoleStore,
  detect: vi.fn(),
  run: vi.fn(),
}))

vi.mock("@/lib/mole-store", () => ({
  detect: mocks.detect,
  getSnapshot: () => mocks.state,
  run: mocks.run,
  subscribe: () => () => undefined,
}))
vi.mock("@/lib/system-metrics-store", () => ({ formatBytes: (bytes: number) => `${bytes} B` }))

import { MolePanel } from "./MolePanel"

const labels: Record<string, string> = {
  "tools.mole.session.disk": "Disk",
  "tools.mole.session.diskHint": "Disk hint",
  "tools.mole.session.cache": "Cache",
  "tools.mole.session.cacheHint": "Cache hint",
  "tools.mole.session.tune": "Tune",
  "tools.mole.session.tuneHint": "Tune hint",
  "tools.mole.analyze": "Analyze",
  "tools.mole.cleanPreview": "Preview clean",
  "tools.mole.cleanNow": "Clean now",
  "tools.mole.optimizePreview": "Preview optimize",
  "tools.mole.optimizeNow": "Optimize now",
  "tools.mole.progress": "Progress",
  "tools.mole.running": "Running",
  "tools.mole.title": "Clean",
  "tools.mole.unsupported": "Audited for Mole {required} only. Installed: {found}.",
}
const t = (key: string) => labels[key] ?? key

afterEach(() => {
  cleanup()
  vi.clearAllMocks()
})

describe("MolePanel", () => {
  it("disables every action while a different session is active, while showing progress only there", () => {
    render(<MolePanel active t={t} />)

    for (const name of ["Analyze", "Preview clean", "Clean now", "Preview optimize", "Optimize now"]) {
      const button = screen.getByRole<HTMLButtonElement>("button", { name })
      expect(button.disabled).toBe(true)
      fireEvent.click(button)
    }
    expect(mocks.run).not.toHaveBeenCalled()
    expect(screen.getByText("Cleaning cache")).toBeTruthy()
    expect(screen.getAllByText("Progress")).toHaveLength(1)
    expect(screen.queryByText(/%$/)).toBeNull()
  })

  it("keeps session results when the panel unmounts and remounts without scanning", () => {
    mocks.state.busy = null
    mocks.state.sessions.cache.items = [{ name: "Cached files", detail: "", bytes: 25, skipped: false }]
    const view = render(<MolePanel active t={t} />)
    expect(screen.getByText("Cached files")).toBeTruthy()
    view.unmount()

    render(<MolePanel active t={t} />)
    expect(screen.getByText("Cached files")).toBeTruthy()
    expect(mocks.run).not.toHaveBeenCalled()
  })

  it("names the audited and installed versions instead of offering an unaudited run", () => {
    mocks.state.info = {
      version: "1.39.0",
      path: "/opt/homebrew/bin/mo",
      supported: false,
      required_version: "1.38.1",
    }
    render(<MolePanel active t={t} />)

    expect(screen.getByText("Audited for Mole 1.38.1 only. Installed: 1.39.0.")).toBeTruthy()
    expect(screen.getByText("brew upgrade mole")).toBeTruthy()
    expect(screen.queryByRole("button")).toBeNull()
  })
})
