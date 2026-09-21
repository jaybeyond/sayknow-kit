/** @vitest-environment jsdom */
import { act, renderHook, waitFor } from "@testing-library/react"
import { beforeEach, describe, expect, it, vi } from "vitest"

type ChatCall = {
  onDelta?: (text: string) => void
  onTool?: (tool: { callId: string; name: string; completed: boolean }) => void
  signal?: AbortSignal
}

const chatMock = vi.fn()
vi.mock("@/lib/openrouter", async () => {
  const actual = await vi.importActual<typeof import("@/lib/openrouter")>("@/lib/openrouter")
  return {
    ...actual,
    chat: (...args: unknown[]) => chatMock(...args),
  }
})

const { useChat } = await import("./useChat")

const args = {
  apiKey: "key",
  baseURL: "https://example.invalid",
  provider: "oauth:cursor" as const,
  model: "composer-1",
}

beforeEach(() => {
  chatMock.mockReset()
  localStorage.clear()
})

describe("useChat streaming", () => {
  it("shows assistant text while it is still arriving, then clears it", async () => {
    let release: (() => void) | undefined
    const finished = new Promise<void>((resolve) => {
      release = resolve
    })

    chatMock.mockImplementation(async (call: ChatCall) => {
      call.onDelta?.("Hello ")
      call.onDelta?.("world")
      await finished
      return { content: "Hello world", model: "composer-1" }
    })

    const { result } = renderHook(() => useChat(args))

    void act(() => {
      void result.current.send("question")
    })

    await waitFor(() => expect(result.current.streamingText).toBe("Hello world"))
    expect(result.current.sending).toBe(true)

    await act(async () => {
      release?.()
      await finished
    })

    // The finished turn becomes a stored message, so the live buffer must not
    // linger and double the answer on screen.
    await waitFor(() => expect(result.current.streamingText).toBe(""))
    await waitFor(() =>
      expect(result.current.messages.at(-1)).toMatchObject({
        role: "assistant",
        content: "Hello world",
      }),
    )
  })

  it("stops passing a turn's deltas through once it is aborted", async () => {
    chatMock.mockImplementation(async (call: ChatCall) => {
      call.onDelta?.("first")
      // The stop button fires here, mid-turn.
      await new Promise((resolve) => setTimeout(resolve, 0))
      call.onDelta?.("after-abort")
      return { content: "unused", model: "composer-1" }
    })

    const { result } = renderHook(() => useChat(args))

    void act(() => {
      void result.current.send("question")
    })

    await waitFor(() => expect(result.current.streamingText).toBe("first"))

    act(() => {
      result.current.stop()
    })

    await waitFor(() => expect(result.current.sending).toBe(false))
    expect(result.current.streamingText).not.toContain("after-abort")
  })

  it("leaves streaming empty for providers that answer in one piece", async () => {
    chatMock.mockResolvedValue({ content: "whole answer", model: "gpt" })

    const { result } = renderHook(() => useChat(args))

    await act(async () => {
      await result.current.send("question")
    })

    expect(result.current.streamingText).toBe("")
    expect(result.current.messages.at(-1)).toMatchObject({ content: "whole answer" })
  })

  it("shows a running tool while it runs and drops it when it finishes", async () => {
    let finishTool: (() => void) | undefined
    const toolFinished = new Promise<void>((resolve) => {
      finishTool = resolve
    })

    chatMock.mockImplementation(async (call: ChatCall) => {
      call.onTool?.({ callId: "c1", name: "shell", completed: false })
      await toolFinished
      call.onTool?.({ callId: "c1", name: "shell", completed: true })
      return { content: "ran it", model: "composer-1" }
    })

    const { result } = renderHook(() => useChat(args))

    void act(() => {
      void result.current.send("run the build")
    })

    await waitFor(() => expect(result.current.activeTools).toEqual(["shell"]))

    await act(async () => {
      finishTool?.()
      await toolFinished
    })

    await waitFor(() => expect(result.current.activeTools).toEqual([]))
  })
})
