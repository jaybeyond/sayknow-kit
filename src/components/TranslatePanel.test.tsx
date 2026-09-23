/** @vitest-environment jsdom */
import { useState } from "react"
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest"
import { act, cleanup, fireEvent, render, screen } from "@testing-library/react"
import type { Settings } from "@/hooks/useSettings"
import type { ChatResult } from "@/lib/openrouter"

const mocks = vi.hoisted(() => ({
  chat: vi.fn(),
  addHistory: vi.fn(),
  recordUsage: vi.fn(),
  writeText: vi.fn(async () => {}),
}))

vi.mock("@/lib/runtime", () => ({ isTauri: () => false }))
vi.mock("@/hooks/useModels", () => ({ useModels: () => ({ models: [] }) }))
vi.mock("@/hooks/useHistory", () => ({ useHistory: () => ({ add: mocks.addHistory }) }))
vi.mock("@/hooks/useUsage", () => ({ useUsage: () => ({ record: mocks.recordUsage }) }))
vi.mock("@/lib/translation-memory", () => ({
  translationMemory: { get: () => null, put: vi.fn() },
}))
vi.mock("@/lib/openrouter", async (importOriginal) => ({
  ...await importOriginal<typeof import("@/lib/openrouter")>(),
  chat: mocks.chat,
}))

import { TranslatePanel, type TranslateInjection } from "./TranslatePanel"

const settings: Settings = {
  provider: "openrouter",
  baseURL: "https://example.test/v1",
  apiKey: "test-key",
  model: "test-model",
  fallbackModel: "",
  from: "auto",
  to: "en",
  autoTranslate: false,
  pinned: false,
  clipboardOnHotkey: false,
  glossary: [],
  uiLocale: "en",
  customTranslatePrompt: "",
  customRefinePrompt: "",
  windowMode: "normal",
  translateEngine: "llm",
  deeplFormality: "default",
  deeplKey: "",
  workspaceMode: "rewrite",
}

function Harness({
  initial = {},
  injectedInput,
}: {
  initial?: Partial<Settings>
  injectedInput?: TranslateInjection
}) {
  const [current, setCurrent] = useState({ ...settings, ...initial })
  return (
    <TranslatePanel
      settings={current}
      update={(patch) => setCurrent((previous) => ({ ...previous, ...patch }))}
      injectedInput={injectedInput}
    />
  )
}

function result(content: string): ChatResult {
  return { content, model: "test-model" }
}

async function generateCards() {
  fireEvent.change(screen.getByRole("textbox"), { target: { value: "Original draft" } })
  fireEvent.click(screen.getByRole("button", { name: "Run" }))
  await screen.findByText("Polished draft")
}

beforeEach(() => {
  mocks.chat.mockReset()
  mocks.chat
    .mockResolvedValueOnce(result("Polished draft"))
    .mockResolvedValueOnce(result("Casual draft"))
    .mockResolvedValue(result("Second revision"))
  Object.defineProperty(navigator, "clipboard", {
    value: { writeText: mocks.writeText },
    configurable: true,
  })
})

afterEach(() => {
  cleanup()
  vi.clearAllMocks()
})

describe("rewrite result actions", () => {
  it.each(["normal", "compact"] as const)(
    "uses a card as editable input and reruns from it in %s layout",
    async (windowMode) => {
      render(<Harness initial={{ windowMode }} />)
      await generateCards()

      fireEvent.click(screen.getAllByRole("button", { name: "Use as input" })[0])

      const input = screen.getByRole<HTMLTextAreaElement>("textbox")
      expect(input.value).toBe("Polished draft")
      expect(document.activeElement).toBe(input)
      expect(screen.getByText("Casual draft")).toBeTruthy()
      expect(screen.getAllByRole("button", { name: "Use as input" })).toHaveLength(2)
      expect(mocks.chat).toHaveBeenCalledTimes(2)
      expect(mocks.writeText).not.toHaveBeenCalled()
      expect(mocks.addHistory).not.toHaveBeenCalled()

      fireEvent.click(screen.getByRole("button", { name: "Run" }))
      await screen.findAllByText("Second revision")
      expect(mocks.chat).toHaveBeenCalledTimes(4)
      for (const [request] of mocks.chat.mock.calls.slice(2)) {
        expect(JSON.stringify(request.messages)).toContain("Polished draft")
        expect(JSON.stringify(request.messages)).not.toContain("Original draft")
      }
    },
  )

  it("copies a result without replacing the input", async () => {
    render(<Harness />)
    await generateCards()
    fireEvent.click(screen.getAllByRole("button", { name: "Copy" })[1])
    expect(mocks.writeText).toHaveBeenCalledWith("Casual draft")
    expect(screen.getByRole<HTMLTextAreaElement>("textbox").value).toBe("Original draft")
  })

  it("clears the previous translation instead of storing the selected draft as a translation", async () => {
    render(<Harness injectedInput={{ text: "Original draft", output: "Old translation", nonce: 1 }} />)
    fireEvent.click(screen.getByRole("button", { name: "Run" }))
    await screen.findByText("Polished draft")
    fireEvent.click(screen.getAllByRole("button", { name: "Use as input" })[1])
    fireEvent.click(screen.getByRole("button", { name: "Translate" }))

    expect(screen.getByRole<HTMLTextAreaElement>("textbox").value).toBe("Casual draft")
    expect(screen.queryByText("Old translation")).toBeNull()
    expect(screen.queryByText("Casual draft", { selector: "[aria-live]" })).toBeNull()
    expect(screen.getByRole<HTMLButtonElement>("button", { name: "Copy" }).disabled).toBe(true)
    expect(mocks.chat).toHaveBeenCalledTimes(2)
  })

  it("cancels an old translation so its late result cannot overwrite the new draft", async () => {
    let finishTranslation!: (value: ChatResult) => void
    mocks.chat.mockReset()
    mocks.chat
      .mockImplementationOnce(() => new Promise<ChatResult>((resolve) => { finishTranslation = resolve }))
      .mockResolvedValueOnce(result("Polished draft"))
      .mockResolvedValueOnce(result("Casual draft"))
    render(<Harness initial={{ workspaceMode: "translate" }} />)
    fireEvent.change(screen.getByRole("textbox"), { target: { value: "Original draft" } })
    fireEvent.click(screen.getAllByRole("button", { name: "Translate" })[1])
    const signal = mocks.chat.mock.calls[0][0].signal as AbortSignal
    fireEvent.click(screen.getByRole("button", { name: "Rewrite" }))
    fireEvent.click(screen.getByRole("button", { name: "Run" }))
    await screen.findByText("Polished draft")
    fireEvent.click(screen.getAllByRole("button", { name: "Use as input" })[0])
    expect(signal.aborted).toBe(true)

    await act(async () => { finishTranslation(result("Late translation")) })
    fireEvent.click(screen.getByRole("button", { name: "Translate" }))
    expect(screen.getByRole<HTMLTextAreaElement>("textbox").value).toBe("Polished draft")
    expect(screen.queryByText("Late translation")).toBeNull()
    expect(mocks.addHistory).not.toHaveBeenCalled()
  })

  it("keeps rewriting manual after replacing input and allows clearing retained cards", async () => {
    render(<Harness initial={{ autoTranslate: true }} />)
    await generateCards()
    fireEvent.click(screen.getAllByRole("button", { name: "Use as input" })[0])
    await act(async () => { await new Promise((resolve) => setTimeout(resolve, 1600)) })
    expect(mocks.chat).toHaveBeenCalledTimes(2)
    fireEvent.click(screen.getByRole("button", { name: "Clear input" }))
    expect(screen.getByRole<HTMLTextAreaElement>("textbox").value).toBe("")
    expect(screen.queryByRole("button", { name: "Use as input" })).toBeNull()
  })

  it.each([
    ["ko", "실행", "원문으로 사용"],
    ["zh", "运行", "用作原文"],
  ] as const)("names the action explicitly in %s", async (uiLocale, run, useAsInput) => {
    render(<Harness initial={{ uiLocale }} />)
    fireEvent.change(screen.getByRole("textbox"), { target: { value: "Original draft" } })
    fireEvent.click(screen.getByRole("button", { name: run }))
    await screen.findByText("Polished draft")
    fireEvent.click(screen.getAllByRole("button", { name: useAsInput })[0])
    expect(screen.getByRole<HTMLTextAreaElement>("textbox").value).toBe("Polished draft")
  })
})
