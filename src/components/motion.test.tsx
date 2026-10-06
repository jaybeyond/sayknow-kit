/** @vitest-environment jsdom */
import { afterEach, describe, expect, it, vi } from "vitest"
import { cleanup, fireEvent, render, screen } from "@testing-library/react"
import { useRef, useState } from "react"

import { IconSwap, Shimmer } from "./motion"
import { useSlidingPill } from "@/hooks/useSlidingPill"
import { shake } from "@/lib/shake"

afterEach(() => {
  cleanup()
  vi.unstubAllGlobals()
})

function Tabs() {
  const [tab, setTab] = useState("a")
  const box = useRef<HTMLDivElement>(null)
  const pill = useRef<HTMLSpanElement>(null)
  useSlidingPill(box, pill, tab)
  return (
    <>
      <div ref={box}>
        <span ref={pill} data-testid="pill" className="t-pill" />
        {["a", "b"].map((id) => (
          <button key={id} data-pill-active={tab === id} onClick={() => setTab(id)}>
            {id}
          </button>
        ))}
      </div>
      <button onClick={() => setTab("b")}>keyboard</button>
    </>
  )
}

describe("useSlidingPill", () => {
  it("slides after a press in the control, and jumps for any other change", () => {
    render(<Tabs />)
    const pill = screen.getByTestId("pill")
    expect(pill.style.opacity).toBe("1")

    // A change that did not start with a press inside the control (a
    // shortcut) lands instantly.
    fireEvent.click(screen.getByText("keyboard"))
    expect(pill.classList.contains("t-pill-instant")).toBe(true)

    fireEvent.pointerDown(screen.getByText("a"))
    fireEvent.click(screen.getByText("a"))
    expect(pill.classList.contains("t-pill-instant")).toBe(false)

    // A shortcut right after a click is still the keyboard's change.
    fireEvent.pointerDown(screen.getByText("a"))
    fireEvent.keyDown(window, { key: "2", metaKey: true })
    fireEvent.click(screen.getByText("keyboard"))
    expect(pill.classList.contains("t-pill-instant")).toBe(true)
  })

  it("re-snaps when the active option moves while the control keeps its size", () => {
    const observers: { cb: () => void; targets: Element[] }[] = []
    vi.stubGlobal(
      "ResizeObserver",
      class {
        targets: Element[] = []
        constructor(cb: () => void) {
          observers.push({ cb, targets: this.targets })
        }
        observe(el: Element) {
          this.targets.push(el)
        }
        disconnect() {}
      },
    )
    render(<Tabs />)
    const pill = screen.getByTestId("pill")
    const active = screen.getByText("a")
    const live = observers[observers.length - 1]
    expect(live.targets).toContain(active)

    // A new language widens an earlier label: the active option shifts right.
    Object.defineProperty(active, "offsetLeft", { configurable: true, value: 40 })
    Object.defineProperty(active, "offsetWidth", { configurable: true, value: 52 })
    live.cb()
    expect(pill.style.transform).toBe("translate(40px, 0px)")
    expect(pill.style.width).toBe("52px")
    expect(pill.classList.contains("t-pill-instant")).toBe(true)
  })
})

describe("IconSwap", () => {
  it("keeps both icons in place but hides the inactive one from assistive tech", () => {
    const { container, rerender } = render(<IconSwap on={false} a={<i>copy</i>} b={<i>done</i>} />)
    const swap = container.querySelector(".t-icon-swap")!
    expect(swap.getAttribute("data-state")).toBe("a")
    expect(container.querySelector('[data-icon="b"]')!.getAttribute("aria-hidden")).toBe("true")
    rerender(<IconSwap on a={<i>copy</i>} b={<i>done</i>} />)
    expect(swap.getAttribute("data-state")).toBe("b")
    expect(container.querySelector('[data-icon="a"]')!.getAttribute("aria-hidden")).toBe("true")
  })
})

describe("Shimmer", () => {
  it("carries its text for the sweeping highlight layer", () => {
    render(<Shimmer text="Translating…" />)
    expect(screen.getByText("Translating…").getAttribute("data-text")).toBe("Translating…")
  })
})

describe("shake", () => {
  it("does nothing under reduced motion", () => {
    vi.stubGlobal("matchMedia", () => ({ matches: true }))
    const el = document.createElement("div")
    el.animate = vi.fn() as never
    shake(el)
    expect(el.animate).not.toHaveBeenCalled()
  })

  it("plays the recipe's four legs otherwise", () => {
    vi.stubGlobal("matchMedia", () => ({ matches: false }))
    const el = document.createElement("div")
    el.animate = vi.fn() as never
    shake(el)
    const [frames, options] = (el.animate as unknown as ReturnType<typeof vi.fn>).mock.calls[0]
    expect(frames).toHaveLength(5)
    expect(options).toEqual({ duration: 280 })
  })
})
