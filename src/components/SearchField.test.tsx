/** @vitest-environment jsdom */
import { afterEach, describe, expect, it, vi } from "vitest"
import { cleanup, fireEvent, render, screen } from "@testing-library/react"
import { useState } from "react"

import { SearchField } from "./SearchField"

function Harness() {
  const [q, setQ] = useState("")
  return <SearchField value={q} onChange={setQ} placeholder="Search" clearLabel="Clear search" />
}

afterEach(() => {
  cleanup()
  vi.unstubAllGlobals()
})

describe("SearchField", () => {
  it("shows the clear button only with a query, and clearing empties it", () => {
    vi.stubGlobal("matchMedia", () => ({ matches: true }))
    render(<Harness />)
    const input = screen.getByPlaceholderText("Search") as HTMLInputElement
    expect(screen.queryByRole("button", { name: "Clear search" })).toBeNull()

    fireEvent.change(input, { target: { value: "memo" } })
    fireEvent.click(screen.getByRole("button", { name: "Clear search" }))
    expect(input.value).toBe("")
    expect(screen.queryByRole("button", { name: "Clear search" })).toBeNull()
  })

  it("keeps focus in the field when the clear button is pressed", () => {
    render(<Harness />)
    const input = screen.getByPlaceholderText("Search") as HTMLInputElement
    fireEvent.change(input, { target: { value: "memo" } })
    input.focus()
    const down = new PointerEvent("pointerdown", { bubbles: true, cancelable: true })
    screen.getByRole("button", { name: "Clear search" }).dispatchEvent(down)
    expect(down.defaultPrevented).toBe(true)
  })
})
