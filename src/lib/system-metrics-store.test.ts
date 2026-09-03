import { afterEach, beforeEach, describe, expect, it, vi } from "vitest"

const mocks = vi.hoisted(() => ({
  invoke: vi.fn(),
  listen: vi.fn(),
}))

vi.mock("./runtime", () => ({ isTauri: () => true }))
vi.mock("@tauri-apps/api/core", () => ({ invoke: mocks.invoke }))
vi.mock("@tauri-apps/api/event", () => ({ listen: mocks.listen }))

import {
  decodeMetricsSnapshot,
  formatBytes,
  formatPercent,
  getSnapshot,
  refresh,
  resetForTests,
  setActive,
} from "./system-metrics-store"
import type { MetricsSnapshot } from "./system-metrics-store"

const snapshot: MetricsSnapshot = {
  schema_version: 1,
  sampled_at_ms: 1_000,
  cpu: { state: "available", percent: 42.6, sample_start_ms: 500, sample_end_ms: 1_000 },
  memory: {
    state: "available",
    total_bytes: 16 * 1024 ** 3,
    used_bytes: 8 * 1024 ** 3,
    available_bytes: 8 * 1024 ** 3,
    sampled_at_ms: 1_000,
  },
  storage: {
    state: "available",
    total_bytes: 512 * 1024 ** 3,
    used_bytes: 128 * 1024 ** 3,
    available_bytes: 384 * 1024 ** 3,
    sampled_at_ms: 1_000,
  },
  cpu_package_temperature: { state: "unavailable", reason: "no_verified_package_sensor" },
}

function deferred<T>() {
  let resolve!: (value: T) => void
  let reject!: (reason: unknown) => void
  const promise = new Promise<T>((ok, fail) => {
    resolve = ok
    reject = fail
  })
  return { promise, resolve, reject }
}

beforeEach(() => {
  vi.useFakeTimers()
  mocks.invoke.mockReset()
  mocks.listen.mockReset()
  mocks.listen.mockResolvedValue(vi.fn())
  resetForTests()
})

afterEach(() => {
  resetForTests()
  vi.useRealTimers()
})

describe("system metrics formatting", () => {
  it("formats percentages and byte boundaries deterministically", () => {
    expect(formatPercent(42.6)).toBe("43%")
    expect(formatPercent(Number.NaN)).toBe("—")
    expect(formatPercent(-1)).toBe("—")
    expect(formatPercent(101)).toBe("—")
    expect(formatBytes(0)).toBe("0 B")
    expect(formatBytes(1024)).toBe("1.0 KB")
    expect(formatBytes(1024 ** 3)).toBe("1.0 GB")
    expect(formatBytes(-1)).toBe("—")
    expect(formatBytes(Number.NaN)).toBe("—")
  })
})

describe("system metrics decoder", () => {
  it("accepts every supported tagged branch", () => {
    expect(decodeMetricsSnapshot(snapshot)).toEqual(snapshot)
    expect(decodeMetricsSnapshot({
      ...snapshot,
      cpu: { state: "unavailable", reason: "cpu_unavailable" },
      memory: { state: "unavailable", reason: "memory_unavailable" },
      storage: { state: "unavailable", reason: "storage_unavailable" },
      cpu_package_temperature: {
        state: "available",
        celsius: 55.25,
        sampled_at_ms: 1_000,
        provenance: "verified_cpu_package",
        adapter_id: "verified-adapter",
      },
    }).cpu.state).toBe("unavailable")
    expect(decodeMetricsSnapshot({
      ...snapshot,
      cpu: { state: "warming_up", reason: "baseline_pending" },
    }).cpu.state).toBe("warming_up")
  })

  it.each([
    [{ ...snapshot, schema_version: 2 }, "schema"],
    [{ ...snapshot, extra: true }, "extra key"],
    [{ ...snapshot, cpu: { ...snapshot.cpu, percent: Number.NaN } }, "non-finite CPU"],
    [{ ...snapshot, cpu: { ...snapshot.cpu, sample_end_ms: 500 } }, "invalid CPU interval"],
    [{ ...snapshot, memory: { ...snapshot.memory, used_bytes: 1 } }, "invalid capacity"],
    [{
      ...snapshot,
      memory: {
        ...snapshot.memory,
        total_bytes: Number.MAX_SAFE_INTEGER,
        used_bytes: Number.MAX_SAFE_INTEGER,
        available_bytes: 1,
      },
    }, "capacity overflow"],
    [{
      ...snapshot,
      cpu_package_temperature: {
        state: "available",
        celsius: 50,
        sampled_at_ms: 1_000,
        provenance: "generic_sensor",
        adapter_id: "unknown",
      },
    }, "unverified temperature"],
  ])("rejects malformed payloads: %s", (payload, label) => {
    expect(label).toBeTruthy()
    expect(() => decodeMetricsSnapshot(payload)).toThrow()
  })

  it("accepts the macOS SoC die adapter as a known provenance", () => {
    const decoded = decodeMetricsSnapshot({
      ...snapshot,
      cpu_package_temperature: {
        state: "available",
        celsius: 45.88,
        sampled_at_ms: 1_000,
        provenance: "apple_soc_die_max",
        adapter_id: "macos.iohid.applevendor.die.v1",
      },
    })
    expect(decoded.cpu_package_temperature).toEqual({
      state: "available",
      celsius: 45.88,
      sampled_at_ms: 1_000,
      provenance: "apple_soc_die_max",
      adapter_id: "macos.iohid.applevendor.die.v1",
    })
  })
})

describe("system metrics lifecycle", () => {
  it("does not invoke or attach a listener while inactive", async () => {
    await refresh()
    await vi.advanceTimersByTimeAsync(9_000)
    expect(mocks.invoke).not.toHaveBeenCalled()
    expect(mocks.listen).not.toHaveBeenCalled()
    expect(getSnapshot().snapshot).toBeNull()
  })

  it("loads once on activation and polls every three seconds", async () => {
    mocks.invoke.mockResolvedValue(snapshot)
    setActive(true)
    await vi.waitFor(() => expect(mocks.invoke).toHaveBeenCalledTimes(1))
    await vi.waitFor(() => expect(getSnapshot().status).toBe("ready"))
    expect(getSnapshot().snapshot?.cpu.state).toBe("available")
    expect(mocks.listen).toHaveBeenCalledWith("sayknow:open", expect.any(Function))

    await vi.advanceTimersByTimeAsync(3_000)
    await vi.waitFor(() => expect(mocks.invoke).toHaveBeenCalledTimes(2))
  })

  it("coalesces overlap and runs at most one trailing refresh", async () => {
    const first = deferred<MetricsSnapshot>()
    mocks.invoke
      .mockReturnValueOnce(first.promise)
      .mockResolvedValueOnce(snapshot)

    setActive(true)
    await vi.waitFor(() => expect(mocks.invoke).toHaveBeenCalledTimes(1))
    const joinedA = refresh()
    const joinedB = refresh()
    expect(mocks.invoke).toHaveBeenCalledTimes(1)

    first.resolve(snapshot)
    await Promise.all([joinedA, joinedB])
    await vi.waitFor(() => expect(mocks.invoke).toHaveBeenCalledTimes(2))
    expect(mocks.invoke).toHaveBeenCalledTimes(2)
  })

  it("marks retained data stale when a refresh fails", async () => {
    mocks.invoke.mockResolvedValueOnce(snapshot).mockRejectedValueOnce(new Error("offline"))
    setActive(true)
    await vi.waitFor(() => expect(getSnapshot().status).toBe("ready"))

    await refresh()
    expect(getSnapshot().status).toBe("stale_with_error")
    expect(getSnapshot().snapshot).toEqual(snapshot)
    expect(getSnapshot().error).toContain("offline")
  })

  it("preserves stale data and error while a reactivation refresh is pending", async () => {
    const pending = deferred<MetricsSnapshot>()
    mocks.invoke
      .mockResolvedValueOnce(snapshot)
      .mockRejectedValueOnce(new Error("offline"))
      .mockReturnValueOnce(pending.promise)

    setActive(true)
    await vi.waitFor(() => expect(getSnapshot().status).toBe("ready"))
    await refresh()
    setActive(false)
    setActive(true)

    expect(getSnapshot().status).toBe("stale_with_error")
    expect(getSnapshot().refreshing).toBe(true)
    expect(getSnapshot().error).toContain("offline")
    pending.resolve(snapshot)
  })

  it("keeps listener diagnostics separate and clears them after recovery", async () => {
    const stop = vi.fn()
    const recovered = deferred<() => void>()
    mocks.listen.mockRejectedValueOnce(new Error("listener denied")).mockReturnValueOnce(recovered.promise)
    mocks.invoke.mockResolvedValue(snapshot)
    setActive(true)

    await vi.waitFor(() => expect(getSnapshot().status).toBe("ready"))
    await vi.waitFor(() => expect(getSnapshot().listener_error).toContain("listener denied"))
    expect(getSnapshot().error).toBeNull()

    setActive(false)
    setActive(true)
    await vi.waitFor(() => expect(mocks.listen).toHaveBeenCalledTimes(2))
    expect(getSnapshot().listener_error).toBe("listener denied")
    recovered.resolve(stop)
    await vi.waitFor(() => expect(getSnapshot().listener_error).toBeNull())
  })

  it("times out an initial request and lets Retry start a fresh invoke", async () => {
    const pending = deferred<MetricsSnapshot>()
    mocks.invoke.mockReturnValueOnce(pending.promise).mockResolvedValueOnce(snapshot)
    setActive(true)
    await vi.waitFor(() => expect(mocks.invoke).toHaveBeenCalledTimes(1))

    await vi.advanceTimersByTimeAsync(2_500)
    expect(getSnapshot().status).toBe("initial_error")
    expect(getSnapshot().error).toContain("system_metrics_request_timeout")

    await refresh()
    expect(mocks.invoke).toHaveBeenCalledTimes(2)
    expect(getSnapshot().status).toBe("ready")
    pending.resolve({ ...snapshot, sampled_at_ms: 999 })
    await Promise.resolve()
    expect(getSnapshot().snapshot).toEqual(snapshot)
  })

  it("times out retained refresh data and discards the late old result", async () => {
    const pending = deferred<MetricsSnapshot>()
    const lateSnapshot = { ...snapshot, sampled_at_ms: 999 }
    mocks.invoke
      .mockResolvedValueOnce(snapshot)
      .mockReturnValueOnce(pending.promise)
      .mockResolvedValueOnce(snapshot)
    setActive(true)
    await vi.waitFor(() => expect(getSnapshot().status).toBe("ready"))

    void refresh()
    await vi.advanceTimersByTimeAsync(2_500)
    expect(getSnapshot().status).toBe("stale_with_error")
    expect(getSnapshot().error).toContain("system_metrics_request_timeout")

    await refresh()
    expect(mocks.invoke).toHaveBeenCalledTimes(3)
    pending.resolve(lateSnapshot)
    await Promise.resolve()
    expect(getSnapshot().snapshot).toEqual(snapshot)
  })

  it("drops late invoke data and releases a listener resolved after deactivation", async () => {
    const invokeResult = deferred<MetricsSnapshot>()
    const listenerResult = deferred<() => void>()
    const stop = vi.fn()
    mocks.invoke.mockReturnValue(invokeResult.promise)
    mocks.listen.mockReturnValue(listenerResult.promise)

    setActive(true)
    await vi.waitFor(() => expect(mocks.invoke).toHaveBeenCalledTimes(1))
    setActive(false)
    listenerResult.resolve(stop)
    invokeResult.resolve(snapshot)
    await Promise.resolve()
    await Promise.resolve()

    expect(stop).toHaveBeenCalledOnce()
    expect(getSnapshot().snapshot).toBeNull()
    await vi.advanceTimersByTimeAsync(9_000)
    expect(mocks.invoke).toHaveBeenCalledTimes(1)
  })

  it("reactivates after an old invoke settles and performs a fresh trailing read", async () => {
    const old = deferred<MetricsSnapshot>()
    mocks.invoke.mockReturnValueOnce(old.promise).mockResolvedValueOnce(snapshot)

    setActive(true)
    await vi.waitFor(() => expect(mocks.invoke).toHaveBeenCalledTimes(1))
    setActive(false)
    setActive(true)
    expect(mocks.invoke).toHaveBeenCalledTimes(1)

    old.resolve(snapshot)
    await vi.waitFor(() => expect(mocks.invoke).toHaveBeenCalledTimes(2))
    await vi.waitFor(() => expect(getSnapshot().status).toBe("ready"))
  })
})
