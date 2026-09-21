import { beforeEach, describe, expect, it, vi } from "vitest"

const invokeMock = vi.fn()
vi.mock("@tauri-apps/api/core", () => ({
  invoke: (...args: unknown[]) => invokeMock(...args),
}))

type Listener = (event: { payload: unknown }) => void
const listeners = new Map<string, Listener[]>()
const unlistenSpy = vi.fn()

vi.mock("@tauri-apps/api/event", () => ({
  listen: async (name: string, handler: Listener) => {
    listeners.set(name, [...(listeners.get(name) ?? []), handler])
    return () => {
      unlistenSpy()
      listeners.set(name, (listeners.get(name) ?? []).filter((h) => h !== handler))
    }
  },
}))

function emit(name: string, payload: unknown) {
  for (const handler of listeners.get(name) ?? []) handler({ payload })
}

const { chatCursor, listCursorModels, CursorAuthRequired, CURSOR_AUTH_ERROR_PREFIX } =
  await import("./cursor-chat")

beforeEach(() => {
  invokeMock.mockReset()
  unlistenSpy.mockReset()
  listeners.clear()
})

describe("chatCursor", () => {
  it("sends the turn to the Rust command and returns its text", async () => {
    invokeMock.mockResolvedValue({ content: "answer", model: "composer-1", tokens: 12 })

    const result = await chatCursor({
      accessToken: "token",
      model: "composer-1",
      messages: [{ role: "user", content: "question" }],
    })

    expect(result).toEqual({
      content: "answer",
      model: "composer-1",
      usage: { completion_tokens: 12 },
    })

    const [command, payload] = invokeMock.mock.calls[0]
    expect(command).toBe("cursor_chat_send")
    const request = (payload as { request: Record<string, unknown> }).request
    expect(request.access_token).toBe("token")
    expect(request.model).toBe("composer-1")
    expect(request.messages).toEqual([{ role: "user", content: "question" }])
    expect(typeof request.request_id).toBe("string")
  })

  it("reports streamed text in arrival order, ignoring other turns", async () => {
    const seen: string[] = []
    let requestId = ""

    invokeMock.mockImplementation(async (_command: string, payload: unknown) => {
      requestId = (payload as { request: { request_id: string } }).request.request_id
      emit("cursor:delta", { request_id: requestId, text: "Hello " })
      emit("cursor:delta", { request_id: "some-other-turn", text: "IGNORED" })
      emit("cursor:delta", { request_id: requestId, text: "world" })
      return { content: "Hello world", model: "composer-1", tokens: 0 }
    })

    const result = await chatCursor({
      accessToken: "token",
      model: "composer-1",
      messages: [],
      onDelta: (text) => seen.push(text),
    })

    expect(seen).toEqual(["Hello ", "world"])
    expect(result.content).toBe("Hello world")
  })

  it("surfaces tool calls with their completion state", async () => {
    const tools: { name: string; completed: boolean }[] = []

    invokeMock.mockImplementation(async (_command: string, payload: unknown) => {
      const id = (payload as { request: { request_id: string } }).request.request_id
      emit("cursor:tool", { request_id: id, call_id: "c1", name: "shell", completed: false })
      emit("cursor:tool", { request_id: id, call_id: "c1", name: "shell", completed: true })
      return { content: "done", model: "m", tokens: 0 }
    })

    await chatCursor({
      accessToken: "token",
      model: "m",
      messages: [],
      onTool: (tool) => tools.push({ name: tool.name, completed: tool.completed }),
    })

    expect(tools).toEqual([
      { name: "shell", completed: false },
      { name: "shell", completed: true },
    ])
  })

  it("tells the Rust side to cancel when the caller aborts", async () => {
    const controller = new AbortController()
    let requestId = ""

    invokeMock.mockImplementation(async (command: string, payload: unknown) => {
      if (command === "cursor_chat_send") {
        requestId = (payload as { request: { request_id: string } }).request.request_id
        controller.abort()
        return { content: "partial", model: "m", tokens: 0 }
      }
      return undefined
    })

    await chatCursor({
      accessToken: "token",
      model: "m",
      messages: [],
      signal: controller.signal,
    })

    // Dropping the promise is not enough: the stream and its child processes
    // live in Rust and have to be told.
    expect(invokeMock).toHaveBeenCalledWith("cursor_chat_cancel", { requestId })
  })

  it("refuses to start when the signal is already aborted", async () => {
    const controller = new AbortController()
    controller.abort()

    await expect(
      chatCursor({
        accessToken: "token",
        model: "m",
        messages: [],
        signal: controller.signal,
      }),
    ).rejects.toThrow(/cancelled/i)

    expect(invokeMock).not.toHaveBeenCalledWith("cursor_chat_send", expect.anything())
  })

  it("turns the tagged auth failure into a typed re-login error", async () => {
    invokeMock.mockRejectedValue(new Error(`${CURSOR_AUTH_ERROR_PREFIX}token expired`))

    await expect(
      chatCursor({ accessToken: "token", model: "m", messages: [] }),
    ).rejects.toBeInstanceOf(CursorAuthRequired)
  })

  it("leaves ordinary failures as ordinary errors", async () => {
    invokeMock.mockRejectedValue(new Error("connection reset"))

    const failure = chatCursor({ accessToken: "token", model: "m", messages: [] })
    await expect(failure).rejects.toThrow("connection reset")
    await expect(failure).rejects.not.toBeInstanceOf(CursorAuthRequired)
  })

  it("detaches its event listeners when the turn ends", async () => {
    invokeMock.mockResolvedValue({ content: "x", model: "m", tokens: 0 })

    await chatCursor({
      accessToken: "token",
      model: "m",
      messages: [],
      onDelta: () => {},
      onTool: () => {},
    })

    expect(unlistenSpy).toHaveBeenCalledTimes(2)
    expect(listeners.get("cursor:delta")).toEqual([])
  })
})

describe("listCursorModels", () => {
  it("asks the account for its own usable models", async () => {
    invokeMock.mockResolvedValue([{ id: "composer-1", name: "Composer 1" }])

    await expect(listCursorModels("token")).resolves.toEqual([
      { id: "composer-1", name: "Composer 1" },
    ])
    expect(invokeMock).toHaveBeenCalledWith("cursor_list_models", { accessToken: "token" })
  })
})
