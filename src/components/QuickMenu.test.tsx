/** @vitest-environment jsdom */
import { afterEach, describe, expect, it, vi } from "vitest"
import { cleanup, fireEvent, render, screen } from "@testing-library/react"
import type { UpdateStatus } from "@/lib/update"

const mocks = vi.hoisted(() => ({
  status: { state: "current", current: "0.2.28" } as UpdateStatus,
  check: vi.fn(),
  openExternal: vi.fn(),
  invoke: vi.fn(),
}))

vi.mock("@/hooks/useUpdateStatus", () => ({
  useUpdateStatus: () => ({ status: mocks.status, check: mocks.check }),
}))
vi.mock("@/lib/runtime", () => ({
  isTauri: () => true,
  openExternal: mocks.openExternal,
}))
vi.mock("@tauri-apps/api/core", () => ({ invoke: mocks.invoke }))

import { QuickMenu } from "./QuickMenu"

const settings = { uiLocale: "en", autoTranslate: true, clipboardOnHotkey: false } as never

afterEach(() => {
  cleanup()
  vi.clearAllMocks()
})

describe("QuickMenu release badge", () => {
  it("stays quiet while the running version is the latest one", () => {
    mocks.status = { state: "current", current: "0.2.28" }
    render(<QuickMenu settings={settings} update={() => {}} />)

    const gear = screen.getByRole("button")
    expect(gear.getAttribute("aria-label")).toBe("Settings")
    expect(gear.querySelector("span")).toBeNull()
  })

  it("marks the gear and offers the download when a newer release exists", () => {
    mocks.status = {
      state: "outdated",
      current: "0.2.27",
      latest: "0.2.28",
      url: "https://example.test/v0.2.28",
    }
    render(<QuickMenu settings={settings} update={() => {}} />)

    const gear = screen.getByRole("button")
    expect(gear.getAttribute("aria-label")).toContain("Get version 0.2.28")
    expect(gear.querySelector("span")).not.toBeNull()

    fireEvent.click(gear)
    fireEvent.click(screen.getByRole("button", { name: "Get version 0.2.28" }))
    expect(mocks.openExternal).toHaveBeenCalledWith("https://example.test/v0.2.28")
    // The download goes to the release page; nothing is installed behind the user.
    expect(mocks.invoke).not.toHaveBeenCalled()
  })
})
