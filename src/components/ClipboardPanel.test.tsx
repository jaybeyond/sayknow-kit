/** @vitest-environment jsdom */
import { afterEach, describe, expect, it, vi } from "vitest"
import { cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react"
import type { Settings } from "@/hooks/useSettings"

const mocks = vi.hoisted(() => ({
  clear: vi.fn(async () => undefined),
  wipe: vi.fn(async () => undefined),
}))

vi.mock("@/hooks/useClipboardHistory", () => ({
  useClipboardHistory: () => ({
    entries: [
      { id: "a", text: "pinned entry", pinned: true, created_at: 1_000, note: null },
      { id: "b", text: "loose entry", pinned: false, created_at: 1_000, note: null },
    ],
    loaded: true,
    captureEnabled: true,
    setCaptureEnabled: vi.fn(),
    reuse: vi.fn(),
    remove: vi.fn(),
    togglePin: vi.fn(),
    setNote: vi.fn(),
    clear: mocks.clear,
    wipe: mocks.wipe,
  }),
}))

vi.mock("@/i18n", () => ({
  useT: () => ({
    t: (key: string) => ({
      "clipboard.clearMenuTooltip": "Clear options",
      "clipboard.clearUnpinned": "Clear unpinned only",
      "clipboard.clearAll": "Delete all",
      "clipboard.confirmClear": "Clear unpinned clipboard history?",
      "clipboard.confirmWipe": "Delete the entire clipboard history?",
      "common.delete": "Delete",
      "common.cancel": "Cancel",
    })[key] ?? key,
  }),
}))

import { ClipboardPanel } from "./ClipboardPanel"

const settings = { uiLocale: "en" } as unknown as Settings

async function openClearMenu() {
  render(<ClipboardPanel settings={settings} />)
  fireEvent.pointerDown(
    screen.getByLabelText("Clear options"),
    new window.PointerEvent("pointerdown", { bubbles: true, ctrlKey: false, button: 0 }),
  )
  return waitFor(() => screen.getByText("Delete all"))
}

afterEach(() => {
  cleanup()
  vi.clearAllMocks()
})

describe("ClipboardPanel destructive actions", () => {
  it("asks in-app before wiping and then calls wipe", async () => {
    await openClearMenu()
    fireEvent.click(screen.getByText("Delete all"))

    const dialog = await screen.findByRole("dialog")
    expect(dialog.textContent).toContain("Delete the entire clipboard history?")
    expect(mocks.wipe).not.toHaveBeenCalled()

    fireEvent.click(screen.getByRole("button", { name: "Delete" }))
    await waitFor(() => expect(mocks.wipe).toHaveBeenCalledTimes(1))
    expect(mocks.clear).not.toHaveBeenCalled()
  })

  it("cancelling leaves the history untouched", async () => {
    await openClearMenu()
    fireEvent.click(screen.getByText("Delete all"))
    await screen.findByRole("dialog")

    fireEvent.click(screen.getByRole("button", { name: "Cancel" }))
    await waitFor(() => expect(screen.queryByRole("dialog")).toBeNull())
    expect(mocks.wipe).not.toHaveBeenCalled()
    expect(mocks.clear).not.toHaveBeenCalled()
  })

  it("clearing unpinned entries uses the unpinned command", async () => {
    await openClearMenu()
    fireEvent.click(screen.getByText("Clear unpinned only"))

    const dialog = await screen.findByRole("dialog")
    expect(dialog.textContent).toContain("Clear unpinned clipboard history?")

    fireEvent.click(screen.getByRole("button", { name: "Delete" }))
    await waitFor(() => expect(mocks.clear).toHaveBeenCalledTimes(1))
    expect(mocks.wipe).not.toHaveBeenCalled()
  })
})
