/** @vitest-environment jsdom */
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest"

const mocks = vi.hoisted(() => ({ httpFetch: vi.fn() }))
vi.mock("./http", () => ({ httpFetch: mocks.httpFetch }))
vi.mock("./runtime", () => ({ isTauri: () => false }))

import {
  CHECK_INTERVAL_MS,
  checkForUpdate,
  fetchLatestRelease,
  getSnapshot,
  isNewer,
  parseVersion,
} from "./update"

function release(body: unknown, ok = true, status = 200) {
  return { ok, status, json: async () => body }
}

beforeEach(() => {
  localStorage.clear()
  mocks.httpFetch.mockReset()
})

afterEach(() => {
  vi.resetModules()
})

describe("version comparison", () => {
  it("reads a tag with or without the v, and refuses to guess at anything else", () => {
    expect(parseVersion("v0.2.28")).toEqual([0, 2, 28])
    expect(parseVersion(" 1.10.3 ")).toEqual([1, 10, 3])
    expect(parseVersion("0.2")).toBeNull()
    expect(parseVersion("latest")).toBeNull()
  })

  it("compares numerically, not lexically, and never advertises a downgrade", () => {
    expect(isNewer("0.2.29", "0.2.28")).toBe(true)
    expect(isNewer("0.10.0", "0.9.9")).toBe(true)
    expect(isNewer("1.0.0", "0.99.99")).toBe(true)
    expect(isNewer("0.2.28", "0.2.28")).toBe(false)
    expect(isNewer("0.2.27", "0.2.28")).toBe(false)
    // A build suffix carries no ordering here, so it must not count as newer.
    expect(isNewer("0.2.28-beta.1", "0.2.28")).toBe(false)
    expect(isNewer("nightly", "0.2.28")).toBe(false)
  })
})

describe("latest release lookup", () => {
  it("rejects a draft, a pre-release, and an unreadable tag", async () => {
    mocks.httpFetch.mockResolvedValueOnce(release({ tag_name: "v9.9.9", draft: true }))
    await expect(fetchLatestRelease()).rejects.toThrow("unstable_release")
    mocks.httpFetch.mockResolvedValueOnce(release({ tag_name: "v9.9.9", prerelease: true }))
    await expect(fetchLatestRelease()).rejects.toThrow("unstable_release")
    mocks.httpFetch.mockResolvedValueOnce(release({ tag_name: "nightly" }))
    await expect(fetchLatestRelease()).rejects.toThrow("unreadable_release_tag")
    mocks.httpFetch.mockResolvedValueOnce(release({}, false, 403))
    await expect(fetchLatestRelease()).rejects.toThrow("github_status_403")
  })

  it("returns the bare version and the release page it came from", async () => {
    mocks.httpFetch.mockResolvedValueOnce(
      release({ tag_name: "v0.2.29", html_url: "https://example.test/v0.2.29" }),
    )
    await expect(fetchLatestRelease()).resolves.toEqual({
      version: "0.2.29",
      url: "https://example.test/v0.2.29",
    })
  })
})

describe("update check", () => {
  it("reports a newer release and remembers it for a day, then looks again", async () => {
    mocks.httpFetch.mockResolvedValue(
      release({ tag_name: "v0.2.29", html_url: "https://example.test/v0.2.29" }),
    )
    const first = await checkForUpdate("0.2.28", { now: 1_000 })
    expect(first).toEqual({
      state: "outdated",
      current: "0.2.28",
      latest: "0.2.29",
      url: "https://example.test/v0.2.29",
    })
    expect(getSnapshot()).toEqual(first)

    await checkForUpdate("0.2.28", { now: 1_000 + CHECK_INTERVAL_MS - 1 })
    expect(mocks.httpFetch).toHaveBeenCalledTimes(1)

    await checkForUpdate("0.2.28", { now: 1_000 + CHECK_INTERVAL_MS })
    expect(mocks.httpFetch).toHaveBeenCalledTimes(2)
  })

  it("answers from the cache against the running version, so an updated app stops nagging", async () => {
    mocks.httpFetch.mockResolvedValue(
      release({ tag_name: "v0.2.29", html_url: "https://example.test/v0.2.29" }),
    )
    await checkForUpdate("0.2.28", { now: 5_000 })
    const afterUpdating = await checkForUpdate("0.2.29", { now: 6_000 })
    expect(afterUpdating).toEqual({ state: "current", current: "0.2.29" })
    expect(mocks.httpFetch).toHaveBeenCalledTimes(1)
  })

  it("forces a fresh lookup when the user asks", async () => {
    mocks.httpFetch.mockResolvedValue(release({ tag_name: "v0.2.28" }))
    await checkForUpdate("0.2.28", { now: 5_000 })
    await checkForUpdate("0.2.28", { now: 5_100, force: true })
    expect(mocks.httpFetch).toHaveBeenCalledTimes(2)
  })

  it("surfaces a failed check without caching it or throwing at the caller", async () => {
    mocks.httpFetch.mockRejectedValueOnce(new Error("offline"))
    const failed = await checkForUpdate("0.2.28", { now: 7_000 })
    expect(failed).toEqual({ state: "failed", reason: "offline" })
    expect(localStorage.getItem("sayknow:update.lastCheck")).toBeNull()

    mocks.httpFetch.mockResolvedValueOnce(release({ tag_name: "v0.2.28" }))
    const retried = await checkForUpdate("0.2.28", { now: 7_100 })
    expect(retried).toEqual({ state: "current", current: "0.2.28" })
  })

  it("shares one in-flight request between the popover and the settings window", async () => {
    let resolve!: (value: unknown) => void
    mocks.httpFetch.mockReturnValueOnce(new Promise((next) => (resolve = next)))
    const a = checkForUpdate("0.2.28", { now: 9_000 })
    const b = checkForUpdate("0.2.28", { now: 9_000 })
    expect(getSnapshot()).toEqual({ state: "checking" })
    resolve(release({ tag_name: "v0.2.28" }))
    await Promise.all([a, b])
    expect(mocks.httpFetch).toHaveBeenCalledTimes(1)
  })
})
