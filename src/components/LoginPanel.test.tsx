/** @vitest-environment jsdom */
import { afterEach, describe, expect, it, vi } from "vitest"
import { cleanup, fireEvent, render, screen } from "@testing-library/react"
import type { UpdateStatus } from "@/lib/update"

const mocks = vi.hoisted(() => ({
  status: { state: "idle" } as UpdateStatus,
  check: vi.fn(),
  openExternal: vi.fn(),
}))

vi.mock("@/hooks/useUpdateStatus", () => ({
  useUpdateStatus: () => ({ status: mocks.status, check: mocks.check }),
}))
vi.mock("@/lib/runtime", () => ({ isTauri: () => true, openExternal: mocks.openExternal }))
vi.mock("@/lib/openrouter", async (importOriginal) => {
  const actual = await importOriginal<typeof import("@/lib/openrouter")>()
  return { ...actual, verifyKey: vi.fn() }
})

import { LoginPanel } from "./LoginPanel"

afterEach(() => {
  cleanup()
  vi.clearAllMocks()
})

describe("LoginPanel release check", () => {
  it("offers the newer release to a signed-out user, who never sees the gear", () => {
    mocks.status = {
      state: "outdated",
      current: "0.2.29",
      latest: "0.2.30",
      url: "https://example.test/v0.2.30",
    }
    render(<LoginPanel update={() => {}} uiLocale="en" />)

    const link = screen.getByRole("button", { name: /0\.2\.30/ })
    fireEvent.click(link)
    expect(mocks.openExternal).toHaveBeenCalledWith("https://example.test/v0.2.30")
  })

  it("says nothing about updates while the running build is the latest", () => {
    mocks.status = { state: "current", current: "0.2.30" }
    render(<LoginPanel update={() => {}} uiLocale="en" />)

    expect(screen.queryByRole("button", { name: /Get version/ })).toBeNull()
  })
})
