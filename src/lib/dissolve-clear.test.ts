/** @vitest-environment jsdom */
import { afterEach, describe, expect, it, vi } from "vitest"

import { dissolveClear, glowKeyframes, splitWords, streakLayers } from "./dissolve-clear"

function field(value: string) {
  const input = document.createElement("input")
  input.value = value
  input.placeholder = "Search"
  document.body.appendChild(input)
  return input
}

afterEach(() => {
  delete (Element.prototype as { animate?: unknown }).animate
  document.body.innerHTML = ""
  vi.restoreAllMocks()
  vi.unstubAllGlobals()
})

describe("dissolve helpers", () => {
  it("keeps whitespace between words so the mirror wraps like the field", () => {
    expect(splitWords("hi  there\nyou")).toEqual([
      { text: "hi", word: true },
      { text: "  ", word: false },
      { text: "there", word: true },
      { text: "\n", word: false },
      { text: "you", word: true },
    ])
  })

  it("lights the glow after the delay, peaks early and fades out by the end", () => {
    const k = glowKeyframes(1000, 50, 0.15, 0.42)
    expect(k.map((f) => f.offset)).toEqual([0, 0.05, 0.05 + 0.95 * 0.15, 1])
    expect(k.map((f) => f.opacity)).toEqual([0, 0, 0.42, 0])
  })

  it("puts four streaks under each word at that word's line", () => {
    const css = streakLayers([{ cx: 40, bottom: 18, width: 30 }], "0,0,0", 1.5)
    expect(css.match(/radial-gradient/g)).toHaveLength(4)
    expect(css).toContain("at 40.0px 18.0px")
  })
})

describe("dissolveClear", () => {
  it("clears at once and skips the motion when there is nothing to animate", () => {
    const clear = vi.fn()
    dissolveClear(field(""), clear)
    dissolveClear(null, clear)
    expect(clear).toHaveBeenCalledTimes(2)
    expect(document.querySelector(".t-clear-overlay")).toBeNull()
  })

  it("honours reduced motion: the text goes, nothing moves", () => {
    vi.stubGlobal("matchMedia", () => ({ matches: true }))
    const input = field("hello")
    input.animate = vi.fn() as never
    const clear = vi.fn(() => (input.value = ""))
    dissolveClear(input, clear)
    expect(clear).toHaveBeenCalledOnce()
    expect(input.animate).not.toHaveBeenCalled()
    expect(document.querySelector(".t-clear-overlay")).toBeNull()
  })

  it("mirrors the text, clears the field, and removes the overlay when done", async () => {
    vi.stubGlobal("matchMedia", () => ({ matches: false }))
    let resolve!: () => void
    const finished = new Promise<void>((r) => (resolve = r))
    Element.prototype.animate = vi.fn(() => ({ finished })) as never
    const input = field("hello world")
    dissolveClear(input, () => (input.value = ""))

    expect(input.value).toBe("")
    const overlay = document.querySelector(".t-clear-overlay")
    expect(overlay?.textContent).toContain("hello world")
    expect(input.classList.contains("t-clear-hide-placeholder")).toBe(true)

    resolve()
    await finished
    await Promise.resolve()
    expect(document.querySelector(".t-clear-overlay")).toBeNull()
    expect(input.classList.contains("t-clear-hide-placeholder")).toBe(false)
  })
})
