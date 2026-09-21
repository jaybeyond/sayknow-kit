import { beforeEach, describe, expect, it, vi } from "vitest"

const fetchMock = vi.fn()
vi.mock("../http", () => ({ httpFetch: (...args: unknown[]) => fetchMock(...args) }))

const { chatCodex } = await import("./codex-chat")

/** A token whose JWT claim carries the account id Codex requires. */
function tokenWithAccount(id: string): string {
  const payload = { "https://api.openai.com/auth": { chatgpt_account_id: id } }
  const segment = btoa(JSON.stringify(payload))
    .replace(/\+/g, "-")
    .replace(/\//g, "_")
    .replace(/=+$/, "")
  return `header.${segment}.signature`
}

function sseResponse(text: string): Response {
  const body = `data: ${JSON.stringify({
    type: "response.output_text.delta",
    delta: text,
  })}\n\ndata: [DONE]`
  return new Response(body, { status: 200 })
}

function sentBody(): Record<string, unknown> {
  const [, init] = fetchMock.mock.calls[0] as [string, RequestInit]
  return JSON.parse(init.body as string)
}

beforeEach(() => {
  fetchMock.mockReset()
  fetchMock.mockResolvedValue(sseResponse("ok"))
})

describe("chatCodex request body", () => {
  const base = {
    accessToken: tokenWithAccount("acct-1"),
    model: "gpt-6-astra",
  }

  it("encodes a user turn as input_text", async () => {
    await chatCodex({ ...base, messages: [{ role: "user", content: "hi" }] })

    const input = sentBody().input as Array<{ content: Array<{ type: string }> }>
    expect(input[0].content[0].type).toBe("input_text")
  })

  it("encodes an assistant turn as an output_text message", async () => {
    // The API rejects input_text here: "Supported values are: 'output_text'
    // and 'refusal'", pointing at the assistant item.
    await chatCodex({
      ...base,
      messages: [
        { role: "user", content: "hi" },
        { role: "assistant", content: "hello" },
        { role: "user", content: "again" },
      ],
    })

    const input = sentBody().input as Array<{
      type?: string
      role: string
      status?: string
      content: Array<{ type: string; annotations?: unknown[] }>
    }>
    expect(input).toHaveLength(3)
    expect(input[1].type).toBe("message")
    expect(input[1].role).toBe("assistant")
    expect(input[1].status).toBe("completed")
    expect(input[1].content[0].type).toBe("output_text")
    expect(input[1].content[0].annotations).toEqual([])
    // The surrounding user turns stay on the input side.
    expect(input[0].content[0].type).toBe("input_text")
    expect(input[2].content[0].type).toBe("input_text")
  })

  it("lifts system turns into instructions rather than the conversation", async () => {
    await chatCodex({
      ...base,
      messages: [
        { role: "system", content: "be terse" },
        { role: "user", content: "hi" },
      ],
    })

    const body = sentBody()
    expect(body.instructions).toBe("be terse")
    expect((body.input as unknown[]).length).toBe(1)
  })

  it("always streams, because the endpoint does not answer otherwise", async () => {
    await chatCodex({ ...base, messages: [{ role: "user", content: "hi" }] })
    expect(sentBody().stream).toBe(true)
  })

  it("sends the account id from the token", async () => {
    await chatCodex({ ...base, messages: [{ role: "user", content: "hi" }] })
    const [, init] = fetchMock.mock.calls[0] as [string, RequestInit]
    expect((init.headers as Record<string, string>)["chatgpt-account-id"]).toBe("acct-1")
  })

  it("refuses a token with no account id instead of sending a doomed request", async () => {
    await expect(
      chatCodex({ accessToken: "not-a-jwt", model: "gpt-6-astra", messages: [] }),
    ).rejects.toThrow("account id")
    expect(fetchMock).not.toHaveBeenCalled()
  })
})
