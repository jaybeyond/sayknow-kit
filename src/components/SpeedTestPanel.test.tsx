/** @vitest-environment jsdom */
import { afterEach, describe, expect, it, vi } from "vitest"
import { act, cleanup, fireEvent, render, screen } from "@testing-library/react"

const mocks = vi.hoisted(() => ({
  invoke: vi.fn(),
  mac: true,
  holds: [] as boolean[],
}))

vi.mock("@tauri-apps/api/core", () => ({ invoke: mocks.invoke }))
vi.mock("@/lib/shortcuts", () => ({ isMacPlatform: () => mocks.mac }))
vi.mock("@/lib/idle-reload", () => ({ useReloadHold: (active: boolean) => mocks.holds.push(active) }))

import { SpeedTestPanel } from "./SpeedTestPanel"
import { formatMbps, speedErrorKey } from "@/lib/speed-test"

const t = (key: string) => key
const result = {
  download_bps: 155_087_616,
  upload_bps: 9_400_000,
  idle_latency_ms: 114.86,
  bytes_used: 676_453_067,
  interface: "en0",
  server: "jptyo5-edge-fx-005.aaplimg.com",
}

function deferred<T>() {
  let resolve!: (value: T) => void
  let reject!: (error: unknown) => void
  const promise = new Promise<T>((ok, fail) => {
    resolve = ok
    reject = fail
  })
  return { promise, resolve, reject }
}

afterEach(() => {
  cleanup()
  vi.clearAllMocks()
  mocks.mac = true
  mocks.holds = []
})

describe("formatMbps", () => {
  it("uses megabits and keeps unknown values unknown", () => {
    expect(formatMbps(155_087_616)).toBe("155 Mbps")
    expect(formatMbps(9_400_000)).toBe("9.4 Mbps")
    expect(formatMbps(0)).toBe("0.0 Mbps")
    expect(formatMbps(null)).toBeNull()
    expect(formatMbps(-1)).toBeNull()
    expect(formatMbps(Number.NaN)).toBeNull()
  })

  it("maps only known backend codes to their messages", () => {
    expect(speedErrorKey("speed_timeout")).toBe("tools.speed.error.timeout")
    expect(speedErrorKey(new Error("speed_network_error"))).toBe("tools.speed.error.network_error")
    expect(speedErrorKey("speed_failed")).toBe("tools.speed.error.failed")
    expect(speedErrorKey("speed_new_thing")).toBe("tools.speed.error.failed")
    expect(speedErrorKey("ipc broke")).toBe("tools.speed.error.failed")
  })
})

describe("SpeedTestPanel", () => {
  it("never starts a test by itself and warns about data use first", () => {
    render(<SpeedTestPanel t={t} />)
    expect(mocks.invoke).not.toHaveBeenCalled()
    expect(screen.getByText("tools.speed.dataNote")).toBeTruthy()
    expect(screen.getByRole("button", { name: "tools.speed.start" })).toBeTruthy()
  })

  it("runs once, holds the idle reload while running, and shows the result", async () => {
    const run = deferred<typeof result>()
    mocks.invoke.mockImplementation((command: string) => (command === "run_speed_test" ? run.promise : Promise.resolve()))
    render(<SpeedTestPanel t={t} />)

    fireEvent.click(screen.getByRole("button", { name: "tools.speed.start" }))
    expect(await screen.findByText("tools.speed.running")).toBeTruthy()
    expect((screen.getByRole("button", { name: "tools.speed.start" }) as HTMLButtonElement).disabled).toBe(true)
    expect(screen.getByRole("button", { name: "tools.speed.cancel" })).toBeTruthy()
    expect(mocks.holds.at(-1)).toBe(true)

    await act(async () => run.resolve(result))
    expect(screen.getByText("155 Mbps")).toBeTruthy()
    expect(screen.getByText("9.4 Mbps")).toBeTruthy()
    expect(screen.getByText("115 ms")).toBeTruthy()
    expect(screen.getByText("jptyo5-edge-fx-005.aaplimg.com")).toBeTruthy()
    expect(screen.queryByRole("button", { name: "tools.speed.cancel" })).toBeNull()
    expect(screen.getByRole("button", { name: "tools.speed.again" })).toBeTruthy()
    expect(mocks.holds.at(-1)).toBe(false)
    expect(mocks.invoke.mock.calls.filter(([c]) => c === "run_speed_test")).toHaveLength(1)
  })

  it("shows missing figures as unknown, not zero", async () => {
    mocks.invoke.mockResolvedValue({ ...result, upload_bps: null, idle_latency_ms: null, server: null })
    render(<SpeedTestPanel t={t} />)
    fireEvent.click(screen.getByRole("button", { name: "tools.speed.start" }))
    expect(await screen.findByText("155 Mbps")).toBeTruthy()
    expect(screen.getAllByText("tools.speed.unknown")).toHaveLength(3)
    expect(screen.queryByText("0.0 Mbps")).toBeNull()
  })

  it("cancels through the backend and reports the cancellation", async () => {
    const run = deferred<typeof result>()
    mocks.invoke.mockImplementation((command: string) => {
      if (command === "run_speed_test") return run.promise
      if (command === "cancel_speed_test") run.reject("speed_cancelled")
      return Promise.resolve()
    })
    render(<SpeedTestPanel t={t} />)
    fireEvent.click(screen.getByRole("button", { name: "tools.speed.start" }))
    fireEvent.click(await screen.findByRole("button", { name: "tools.speed.cancel" }))
    expect((await screen.findByRole("alert")).textContent).toBe("tools.speed.error.cancelled")
    expect(mocks.invoke).toHaveBeenCalledWith("cancel_speed_test")
  })

  it("keeps the previous result, labelled, when a later test fails", async () => {
    mocks.invoke.mockResolvedValueOnce(result).mockRejectedValueOnce("speed_network_error")
    render(<SpeedTestPanel t={t} />)
    fireEvent.click(screen.getByRole("button", { name: "tools.speed.start" }))
    fireEvent.click(await screen.findByRole("button", { name: "tools.speed.again" }))
    expect((await screen.findByRole("alert")).textContent).toBe("tools.speed.error.network_error")
    expect(screen.getByText("tools.speed.previous")).toBeTruthy()
    expect(screen.getByText("155 Mbps")).toBeTruthy()
  })

  it("does not cancel a new test just because the previous one finished", async () => {
    const second = deferred<typeof result>()
    mocks.invoke
      .mockResolvedValueOnce(result)
      .mockImplementationOnce(() => second.promise)
      .mockImplementation(() => Promise.resolve())
    render(<SpeedTestPanel t={t} />)
    fireEvent.click(screen.getByRole("button", { name: "tools.speed.start" }))
    fireEvent.click(await screen.findByRole("button", { name: "tools.speed.again" }))
    await screen.findByText("tools.speed.running")
    await act(async () => second.resolve({ ...result, download_bps: 50_000_000 }))
    expect(screen.getByText("50.0 Mbps")).toBeTruthy()
    expect(mocks.invoke).not.toHaveBeenCalledWith("cancel_speed_test")
  })

  it("maps unknown backend errors to the generic message instead of a raw code", async () => {
    mocks.invoke.mockRejectedValue(new Error("speed_something_new"))
    render(<SpeedTestPanel t={t} />)
    fireEvent.click(screen.getByRole("button", { name: "tools.speed.start" }))
    expect((await screen.findByRole("alert")).textContent).toBe("tools.speed.error.failed")
  })

  it("stops a running test when the panel goes away", async () => {
    mocks.invoke.mockImplementation((command: string) => (command === "run_speed_test" ? new Promise(() => {}) : Promise.resolve()))
    const view = render(<SpeedTestPanel t={t} />)
    fireEvent.click(screen.getByRole("button", { name: "tools.speed.start" }))
    await screen.findByText("tools.speed.running")
    view.unmount()
    await vi.waitFor(() => expect(mocks.invoke).toHaveBeenCalledWith("cancel_speed_test"))
  })

  it("explains instead of offering a button that cannot work off macOS", () => {
    mocks.mac = false
    render(<SpeedTestPanel t={t} />)
    expect(screen.getByText("tools.speed.error.unsupported")).toBeTruthy()
    expect((screen.getByRole("button", { name: "tools.speed.start" }) as HTMLButtonElement).disabled).toBe(true)
  })
})
