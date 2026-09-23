/** @vitest-environment jsdom */
import { afterEach, describe, expect, it, vi } from "vitest"
import { act, cleanup, fireEvent, render, screen } from "@testing-library/react"
import type { UpdateStatus } from "@/lib/update"

const mocks = vi.hoisted(() => ({
  status: { state: "idle" } as UpdateStatus,
  check: vi.fn(),
  openExternal: vi.fn(),
  installUpdate: vi.fn(),
}))

vi.mock("@/hooks/useUpdateStatus", () => ({
  useUpdateStatus: () => ({ status: mocks.status, check: mocks.check }),
}))
vi.mock("@/lib/runtime", () => ({ isTauri: () => true, openExternal: mocks.openExternal }))
vi.mock("@/lib/update", async (importOriginal) => ({
  ...(await importOriginal<typeof import("@/lib/update")>()),
  installUpdate: mocks.installUpdate,
}))

import { UpdateRow } from "./UpdateRow"

const labels: Record<string, string> = {
  "update.check": "Check for updates",
  "update.checking": "Checking…",
  "update.upToDate": "Up to date",
  "update.failed": "Update check failed",
  "update.available": "Get version {version}",
  "update.install": "Install now",
  "update.installing": "Downloading…",
  "update.restarting": "Installing and restarting",
  "update.installFailed": "Install failed",
}
const t = (key: string) => labels[key] ?? key

afterEach(() => {
  cleanup()
  vi.clearAllMocks()
})

describe("About panel release check", () => {
  it("sits beside the version with a manual check that forces a fresh lookup", () => {
    mocks.status = { state: "current", current: "0.2.28" }
    render(<UpdateRow supported t={t} />)

    expect(screen.getByText("Up to date")).toBeTruthy()
    fireEvent.click(screen.getByRole("button", { name: "Check for updates" }))
    expect(mocks.check).toHaveBeenCalledWith(true)
  })

  it("names the newer version and opens its release page instead of installing", () => {
    mocks.status = {
      state: "outdated",
      current: "0.2.28",
      latest: "0.2.29",
      url: "https://example.test/v0.2.29",
    }
    render(<UpdateRow supported t={t} />)

    fireEvent.click(screen.getByRole("button", { name: "Get version 0.2.29" }))
    expect(mocks.openExternal).toHaveBeenCalledWith("https://example.test/v0.2.29")
  })

  it("blocks a second check while one is running, and says why it is waiting", () => {
    mocks.status = { state: "checking" }
    render(<UpdateRow supported t={t} />)

    expect(screen.getByText("Checking…")).toBeTruthy()
    const button = screen.getByRole<HTMLButtonElement>("button", { name: "Check for updates" })
    expect(button.disabled).toBe(true)
    fireEvent.click(button)
    expect(mocks.check).not.toHaveBeenCalled()
  })

  it("reports a failed check as a state, not as a thrown error the user must dismiss", () => {
    mocks.status = { state: "failed", reason: "offline" }
    render(<UpdateRow supported t={t} />)

    expect(screen.getByText("Update check failed")).toBeTruthy()
    expect(screen.getByRole<HTMLButtonElement>("button", { name: "Check for updates" }).disabled).toBe(false)
  })

  it("stays out of the web preview, which has no installed version to compare", () => {
    mocks.status = { state: "outdated", current: "0.2.28", latest: "0.2.29", url: "https://example.test" }
    const { container } = render(<UpdateRow supported={false} t={t} />)

    expect(container.firstChild).toBeNull()
  })

  it("installs the signed payload in place and reports download progress", async () => {
    mocks.status = {
      state: "outdated",
      current: "0.2.29",
      latest: "0.2.30",
      url: "https://example.test/v0.2.30",
    }
    let report!: (p: { phase: "downloading"; downloaded: number; total: number | null }) => void
    mocks.installUpdate.mockImplementation(
      (onProgress: (p: { phase: "downloading"; downloaded: number; total: number | null }) => void) => {
        report = onProgress
        return new Promise<void>(() => {})
      },
    )
    render(<UpdateRow supported t={t} />)

    fireEvent.click(screen.getByRole("button", { name: "Install now" }))
    expect(mocks.installUpdate).toHaveBeenCalledOnce()

    await act(async () => report({ phase: "downloading", downloaded: 25, total: 100 }))
    expect(screen.getByRole("button", { name: "25%" })).toBeTruthy()
    // A check must not race an install that is already writing the app.
    expect(
      screen.getByRole<HTMLButtonElement>("button", { name: "Check for updates" }).disabled,
    ).toBe(true)
  })

  it("keeps the release page reachable when the signed install cannot be done", async () => {
    mocks.status = {
      state: "outdated",
      current: "0.2.29",
      latest: "0.2.30",
      url: "https://example.test/v0.2.30",
    }
    mocks.installUpdate.mockRejectedValue(new Error("no_signed_update_available"))
    render(<UpdateRow supported t={t} />)

    await act(async () => {
      fireEvent.click(screen.getByRole("button", { name: "Install now" }))
    })

    expect(screen.getByText(/no_signed_update_available/)).toBeTruthy()
    fireEvent.click(screen.getByRole("button", { name: /Get version 0\.2\.30/ }))
    expect(mocks.openExternal).toHaveBeenCalledWith("https://example.test/v0.2.30")
  })
})
