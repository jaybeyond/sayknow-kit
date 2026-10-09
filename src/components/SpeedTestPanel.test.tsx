/** @vitest-environment jsdom */
import { afterEach, describe, expect, it, vi } from "vitest"
import { act, cleanup, fireEvent, render, screen } from "@testing-library/react"

const mocks = vi.hoisted(() => ({
  invoke: vi.fn(),
  mac: true,
  holds: [] as boolean[],
  channels: [] as { onmessage: (reading: unknown) => void }[],
}))

vi.mock("@tauri-apps/api/core", () => ({
  invoke: mocks.invoke,
  Channel: class {
    onmessage: (reading: unknown) => void = () => {}
    constructor() {
      mocks.channels.push(this)
    }
  },
}))
vi.mock("@/lib/shortcuts", () => ({ isMacPlatform: () => mocks.mac }))
vi.mock("@/lib/idle-reload", () => ({ useReloadHold: (active: boolean) => mocks.holds.push(active) }))

import { SpeedTestPanel } from "./SpeedTestPanel"
import { formatMbps, gaugeFraction, gaugeScale, speedErrorKey } from "@/lib/speed-test"

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
  mocks.channels = []
})

/** The figure in the middle of a dial: the number without its unit. */
const dial = (name: string) => {
  const gauge = document.querySelector(`[data-gauge="${name}"]`) as HTMLElement
  return {
    value: gauge.querySelector(".text-lg")?.textContent,
    live: gauge.dataset.live === "true",
    offset: Number(gauge.querySelectorAll("path")[1].getAttribute("stroke-dashoffset")),
  }
}

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

  it("grows the dial to the next step instead of pinning the needle", () => {
    expect(gaugeScale(0)).toBe(50)
    expect(gaugeScale(40_000_000)).toBe(50)
    expect(gaugeScale(155_000_000)).toBe(250)
    expect(gaugeScale(980_000_000)).toBe(1000)
    expect(gaugeScale(50_000_000_000)).toBe(10000)
    expect(gaugeScale(Number.NaN)).toBe(50)
    expect(gaugeFraction(125_000_000, 250)).toBe(0.5)
    expect(gaugeFraction(400_000_000, 250)).toBe(1)
    expect(gaugeFraction(-1, 250)).toBe(0)
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
    expect(mocks.invoke).toHaveBeenCalledWith("run_speed_test", { progress: mocks.channels[0] })

    await act(async () => run.resolve(result))
    expect(dial("tools.speed.download").value).toBe("155")
    expect(dial("tools.speed.upload").value).toBe("9.4")
    expect(screen.getByText("115 ms")).toBeTruthy()
    const server = screen.getByText("jptyo5-edge-fx-005.aaplimg.com")
    // A long host name must wrap itself, not squeeze its label into one word per line.
    expect(server.className.split(" ")).toEqual(expect.arrayContaining(["min-w-0", "[overflow-wrap:anywhere]"]))
    expect((server.previousElementSibling as HTMLElement).className.split(" ")).toContain("shrink-0")
    expect(screen.queryByRole("button", { name: "tools.speed.cancel" })).toBeNull()
    expect(screen.getByRole("button", { name: "tools.speed.again" })).toBeTruthy()
    expect(mocks.holds.at(-1)).toBe(false)
    expect(mocks.invoke.mock.calls.filter(([c]) => c === "run_speed_test")).toHaveLength(1)
  })

  it("moves the dials with every live reading: download first, then upload", async () => {
    const run = deferred<typeof result>()
    mocks.invoke.mockImplementation((command: string) => (command === "run_speed_test" ? run.promise : Promise.resolve()))
    render(<SpeedTestPanel t={t} />)
    fireEvent.click(screen.getByRole("button", { name: "tools.speed.start" }))
    await screen.findByText("tools.speed.running")
    expect(screen.getByText("0/20s")).toBeTruthy()
    const send = (elapsed_ms: number, download_bps: number, upload_bps: number) =>
      act(() => mocks.channels[0].onmessage({ elapsed_ms, download_bps, upload_bps }))

    await send(3_000, 42_000_000, 0)
    expect(dial("tools.speed.download")).toMatchObject({ value: "42.0", live: true })
    expect(dial("tools.speed.upload")).toMatchObject({ value: "0.0", live: false })
    expect(screen.getByText("3/20s")).toBeTruthy()
    const early = dial("tools.speed.download").offset

    await send(6_000, 140_000_000, 0)
    expect(dial("tools.speed.download").value).toBe("140")
    expect(screen.getAllByText("250")).toHaveLength(1) // the download dial grew past 100
    expect(dial("tools.speed.download").offset).toBeLessThan(100)
    expect(dial("tools.speed.download").offset).not.toBe(early)

    await send(15_000, 150_000_000, 80_000_000)
    expect(dial("tools.speed.download").live).toBe(false)
    expect(dial("tools.speed.upload")).toMatchObject({ value: "80.0", live: true })

    // The finished result replaces the last reading.
    await act(async () => run.resolve(result))
    expect(dial("tools.speed.download").value).toBe("155")
    expect(dial("tools.speed.upload")).toMatchObject({ value: "9.4", live: false })
    expect(screen.queryByText("15/20s")).toBeNull()
  })

  it("starts the next run from zero, not from the last reading", async () => {
    const second = deferred<typeof result>()
    mocks.invoke.mockResolvedValueOnce(result).mockImplementationOnce(() => second.promise)
    render(<SpeedTestPanel t={t} />)
    fireEvent.click(screen.getByRole("button", { name: "tools.speed.start" }))
    fireEvent.click(await screen.findByRole("button", { name: "tools.speed.again" }))
    await screen.findByText("tools.speed.running")
    expect(dial("tools.speed.download").value).toBe("0.0")
    expect(screen.getByText("0/20s")).toBeTruthy()
  })

  it("shows missing figures as unknown, not zero", async () => {
    mocks.invoke.mockResolvedValue({ ...result, upload_bps: null, idle_latency_ms: null, server: null })
    render(<SpeedTestPanel t={t} />)
    fireEvent.click(screen.getByRole("button", { name: "tools.speed.start" }))
    await screen.findByText("tools.speed.again")
    expect(dial("tools.speed.download").value).toBe("155")
    expect(dial("tools.speed.upload").value).toBe("–")
    expect(screen.getAllByText("tools.speed.unknown")).toHaveLength(3)
    expect(screen.queryByText("0.0")).toBeNull()
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
    expect(dial("tools.speed.download").value).toBe("155")
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
    expect(dial("tools.speed.download").value).toBe("50.0")
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
