/** @vitest-environment jsdom */
import { afterEach, describe, expect, it, vi } from "vitest"
import { cleanup, fireEvent, render, screen, waitFor, within } from "@testing-library/react"
import type { Settings } from "@/hooks/useSettings"
import type { ClipEntry } from "@/lib/clipboard-history"

const mocks = vi.hoisted(() => ({
  clear: vi.fn(async () => undefined),
  wipe: vi.fn(async () => undefined),
  remove: vi.fn<(id: string) => Promise<void>>(async () => undefined),
  createMemo: vi.fn<(text: string) => Promise<boolean>>(async () => true),
  updateMemo: vi.fn<(id: string, text: string) => Promise<boolean>>(async () => true),
}))

const baseEntries: ClipEntry[] = [
  { id: "m", text: "call the landlord\nabout the lease", preview: "call the landlord", ts: 1_000, pinned: false, note: null, kind: "memo" },
  { id: "a", text: "pinned entry", preview: "pinned entry", ts: 1_000, pinned: true, note: null },
  { id: "b", text: "loose entry", preview: "loose entry", ts: 1_000, pinned: false, note: null, kind: "clip" },
]

vi.mock("@/hooks/useClipboardHistory", () => ({
  useClipboardHistory: () => ({
    entries: baseEntries,
    loaded: true,
    captureEnabled: true,
    setCaptureEnabled: vi.fn(),
    reuse: vi.fn(),
    remove: mocks.remove,
    togglePin: vi.fn(),
    setNote: vi.fn(),
    createMemo: mocks.createMemo,
    updateMemo: mocks.updateMemo,
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
      "clipboard.newMemo": "New memo",
      "clipboard.memoPlaceholder": "Write a memo",
      "clipboard.editMemo": "Edit memo",
      "clipboard.confirmDeleteMemo": "Delete this memo?",
      "clipboard.filterAll": "All",
      "clipboard.filterClips": "Clips",
      "clipboard.filterMemos": "Memos",
      "clipboard.noteSave": "Save",
      "clipboard.delete": "Remove entry",
      "clipboard.pin": "Pin",
      "clipboard.unpin": "Unpin",
      "common.delete": "Delete",
      "common.cancel": "Cancel",
    })[key] ?? key,
  }),
}))

import { ClipboardPanel } from "./ClipboardPanel"

const settings = { uiLocale: "en" } as unknown as Settings

function entryRow(text: string): HTMLElement {
  const row = screen.getByText((_, el) => el?.textContent === text && el.tagName === "DIV").closest("[data-kind]")
  if (!(row instanceof HTMLElement)) throw new Error(`no row for ${text}`)
  return row
}

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

describe("ClipboardPanel memos", () => {
  it("writes a new memo from the toolbar and saves it with cmd+Enter", async () => {
    render(<ClipboardPanel settings={settings} />)
    fireEvent.click(screen.getByRole("button", { name: "New memo" }))

    const editor = screen.getByRole("textbox", { name: "Write a memo" })
    expect(document.activeElement).toBe(editor)
    expect(screen.getByRole("button", { name: "Save" })).toHaveProperty("disabled", true)

    fireEvent.keyDown(editor, { key: "Enter", metaKey: true })
    expect(mocks.createMemo).not.toHaveBeenCalled()

    fireEvent.change(editor, { target: { value: "buy milk" } })
    fireEvent.keyDown(editor, { key: "Enter", metaKey: true })
    await waitFor(() => expect(mocks.createMemo).toHaveBeenCalledWith("buy milk"))
    await waitFor(() => expect(screen.queryByRole("textbox", { name: "Write a memo" })).toBeNull())
  })

  it("keeps the composer open when the backend refuses the memo", async () => {
    mocks.createMemo.mockResolvedValueOnce(false)
    render(<ClipboardPanel settings={settings} />)
    fireEvent.click(screen.getByRole("button", { name: "New memo" }))
    const editor = screen.getByRole("textbox", { name: "Write a memo" })
    fireEvent.change(editor, { target: { value: "draft" } })
    fireEvent.click(screen.getByRole("button", { name: "Save" }))

    await waitFor(() => expect(mocks.createMemo).toHaveBeenCalledTimes(1))
    expect(screen.getByRole("textbox", { name: "Write a memo" })).toHaveProperty("value", "draft")
  })

  it("edits a memo in place and offers no pin or clip-note controls on it", async () => {
    render(<ClipboardPanel settings={settings} />)
    const memo = entryRow("call the landlord\nabout the lease")
    expect(memo.dataset.kind).toBe("memo")
    expect(within(memo).queryByRole("button", { name: "Pin" })).toBeNull()

    fireEvent.click(within(memo).getByRole("button", { name: "Edit memo" }))
    const editor = screen.getByRole("textbox", { name: "Write a memo" })
    expect(editor).toHaveProperty("value", "call the landlord\nabout the lease")

    fireEvent.change(editor, { target: { value: "landlord: done" } })
    fireEvent.click(screen.getByRole("button", { name: "Save" }))
    await waitFor(() => expect(mocks.updateMemo).toHaveBeenCalledWith("m", "landlord: done"))
  })

  it("Escape abandons an edit without saving", () => {
    render(<ClipboardPanel settings={settings} />)
    fireEvent.click(within(entryRow("call the landlord\nabout the lease")).getByRole("button", { name: "Edit memo" }))
    fireEvent.keyDown(screen.getByRole("textbox", { name: "Write a memo" }), { key: "Escape" })

    expect(screen.queryByRole("textbox", { name: "Write a memo" })).toBeNull()
    expect(mocks.updateMemo).not.toHaveBeenCalled()
  })

  it("confirms before deleting a memo but deletes a clip immediately", async () => {
    render(<ClipboardPanel settings={settings} />)

    fireEvent.click(within(entryRow("loose entry")).getByRole("button", { name: "Remove entry" }))
    expect(mocks.remove).toHaveBeenCalledWith("b")
    expect(screen.queryByRole("dialog")).toBeNull()

    fireEvent.click(within(entryRow("call the landlord\nabout the lease")).getByRole("button", { name: "Remove entry" }))
    const dialog = await screen.findByRole("dialog")
    expect(dialog.textContent).toContain("Delete this memo?")
    expect(mocks.remove).toHaveBeenCalledTimes(1)

    fireEvent.click(screen.getByRole("button", { name: "Delete" }))
    await waitFor(() => expect(mocks.remove).toHaveBeenCalledWith("m"))
  })

  it("filters the list by kind, treating entries without a kind as clips", () => {
    render(<ClipboardPanel settings={settings} />)
    const kinds = () => Array.from(document.querySelectorAll<HTMLElement>("[data-kind]")).map((el) => el.dataset.kind)
    expect(kinds()).toEqual(["memo", "clip", "clip"])

    fireEvent.click(screen.getByRole("radio", { name: "Memos" }))
    expect(kinds()).toEqual(["memo"])

    fireEvent.click(screen.getByRole("radio", { name: "Clips" }))
    expect(kinds()).toEqual(["clip", "clip"])
    expect(screen.getByRole("radio", { name: "Clips" }).getAttribute("aria-checked")).toBe("true")
  })

  it("switches away from the clips-only view when starting a memo", () => {
    render(<ClipboardPanel settings={settings} />)
    fireEvent.click(screen.getByRole("radio", { name: "Clips" }))
    fireEvent.click(screen.getByRole("button", { name: "New memo" }))
    expect(screen.getByRole("radio", { name: "All" }).getAttribute("aria-checked")).toBe("true")
  })
})
