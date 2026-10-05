/** @vitest-environment jsdom */
import { afterEach, describe, expect, it, vi } from "vitest"
import { cleanup, renderHook } from "@testing-library/react"
import {
  carry,
  holdReload,
  page,
  reloadHeld,
  reloadNow,
  resetReloadHoldsForTests,
  takeCarried,
  useBeforeReload,
  useReloadHold,
} from "./idle-reload"

afterEach(() => {
  cleanup()
  resetReloadHoldsForTests()
  sessionStorage.clear()
  vi.restoreAllMocks()
})

describe("reload holds", () => {
  it("is held until every hold is released", () => {
    const a = holdReload()
    const b = holdReload()
    a()
    expect(reloadHeld()).toBe(true)
    b()
    expect(reloadHeld()).toBe(false)
  })

  it("releasing the same hold twice does not drop another one", () => {
    const a = holdReload()
    holdReload()
    a()
    a()
    expect(reloadHeld()).toBe(true)
  })

  it("follows the hook's flag and lets go on unmount", () => {
    const { rerender, unmount } = renderHook(({ on }) => useReloadHold(on), { initialProps: { on: false } })
    expect(reloadHeld()).toBe(false)
    rerender({ on: true })
    expect(reloadHeld()).toBe(true)
    rerender({ on: false })
    expect(reloadHeld()).toBe(false)
    rerender({ on: true })
    unmount()
    expect(reloadHeld()).toBe(false)
  })
})

describe("carrying state across the reload", () => {
  it("saves the latest value of every panel before reloading", () => {
    const reload = vi.spyOn(page, "reload").mockImplementation(() => {})
    const { rerender } = renderHook(({ text }) => useBeforeReload(() => carry("draft", text)), {
      initialProps: { text: "first" },
    })
    rerender({ text: "latest" })
    reloadNow()
    expect(reload).toHaveBeenCalledTimes(1)
    expect(takeCarried<string>("draft")).toBe("latest")
  })

  it("hands a carried value back once", () => {
    carry("draft", { input: "a", output: "b" })
    expect(takeCarried("draft")).toEqual({ input: "a", output: "b" })
    expect(takeCarried("draft")).toBeNull()
  })

  it("still reloads when a panel fails to save", () => {
    const reload = vi.spyOn(page, "reload").mockImplementation(() => {})
    renderHook(() =>
      useBeforeReload(() => {
        throw new Error("quota")
      }),
    )
    reloadNow()
    expect(reload).toHaveBeenCalledTimes(1)
  })

  it("does not save for a panel that has unmounted", () => {
    vi.spyOn(page, "reload").mockImplementation(() => {})
    const { unmount } = renderHook(() => useBeforeReload(() => carry("gone", "x")))
    unmount()
    reloadNow()
    expect(takeCarried("gone")).toBeNull()
  })
})
