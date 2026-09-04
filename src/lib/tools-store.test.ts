import { afterEach, describe, expect, it, vi } from "vitest"

const invoke = vi.hoisted(() => vi.fn(async () => []))

vi.mock("./runtime", () => ({ isTauri: () => true }))
vi.mock("@tauri-apps/api/core", () => ({ invoke }))

import { scanDisplays } from "./tools-store"

afterEach(() => {
  vi.clearAllMocks()
})

describe("display scanning", () => {
  it("lets the backend reuse a recent scan unless the user forced a refresh", async () => {
    // A DDC read costs tens to hundreds of ms per monitor, so opening the
    // panel must not demand a fresh round trip.
    await scanDisplays()
    expect(invoke).toHaveBeenCalledWith("list_displays", { force: false })

    invoke.mockClear()
    await scanDisplays(true)
    expect(invoke).toHaveBeenCalledWith("list_displays", { force: true })
  })
})
