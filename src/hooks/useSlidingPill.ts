// "Tabs sliding" from transitions.dev (MIT): the active pill of a segmented
// control slides to the new option instead of jumping.
//
// Only when the change came from the pointer. Tabs are also switched from the
// keyboard (⌘1–4, the global ⌃⌥⌘ keys) dozens of times a day, and a keyboard
// action must land instantly — so the pill jumps unless a press inside this
// control happened just before.
import { useLayoutEffect, useRef, type RefObject } from "react"

const POINTER_WINDOW_MS = 600

export function useSlidingPill(
  containerRef: RefObject<HTMLElement | null>,
  pillRef: RefObject<HTMLElement | null>,
  activeKey: string,
) {
  const pressedAt = useRef(Number.NEGATIVE_INFINITY)

  useLayoutEffect(() => {
    const container = containerRef.current
    if (!container) return
    const onDown = () => {
      pressedAt.current = performance.now()
    }
    // Any key press means the next change is the keyboard's.
    const onKey = () => {
      pressedAt.current = Number.NEGATIVE_INFINITY
    }
    container.addEventListener("pointerdown", onDown)
    window.addEventListener("keydown", onKey, true)
    return () => {
      container.removeEventListener("pointerdown", onDown)
      window.removeEventListener("keydown", onKey, true)
    }
  }, [containerRef])

  useLayoutEffect(() => {
    const container = containerRef.current
    const pill = pillRef.current
    if (!container || !pill) return
    const place = (animate: boolean) => {
      const active = container.querySelector<HTMLElement>("[data-pill-active='true']")
      if (!active) {
        pill.style.opacity = "0"
        return
      }
      pill.classList.toggle("t-pill-instant", !animate)
      pill.style.opacity = "1"
      pill.style.width = `${active.offsetWidth}px`
      pill.style.height = `${active.offsetHeight}px`
      pill.style.transform = `translate(${active.offsetLeft}px, ${active.offsetTop}px)`
    }
    const fromPointer = performance.now() - pressedAt.current < POINTER_WINDOW_MS
    const placed = pill.dataset.placed === "true"
    place(placed && fromPointer)
    pill.dataset.placed = "true"
    // Label widths change with the UI language or a font load; follow them
    // without animating.
    if (typeof ResizeObserver === "undefined") return
    // An observer reports once as soon as it starts; only a real change in
    // size should re-snap, or it would cut the slide that just began.
    let size = `${container.offsetWidth}x${container.offsetHeight}`
    const ro = new ResizeObserver(() => {
      const next = `${container.offsetWidth}x${container.offsetHeight}`
      if (next === size) return
      size = next
      place(false)
    })
    ro.observe(container)
    return () => ro.disconnect()
  }, [containerRef, pillRef, activeKey])
}
