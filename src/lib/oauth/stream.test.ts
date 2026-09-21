import { describe, expect, it } from "vitest"
import { codexAccountId, collectCodexText, splitSseFrames } from "./codex-chat"
import { collectGeminiText } from "./gemini-chat"

function sse(...events: unknown[]): string[] {
  return splitSseFrames(events.map((e) => `data: ${JSON.stringify(e)}`).join("\n\n"))
}

describe("collectCodexText", () => {
  it("concatenates output_text deltas in order", () => {
    const frames = sse(
      { type: "response.output_text.delta", delta: "he" },
      { type: "response.output_text.delta", delta: "llo" },
    )
    expect(collectCodexText(frames)).toBe("hello")
  })

  it("ignores reasoning and lifecycle events", () => {
    const frames = sse(
      { type: "response.created" },
      { type: "response.reasoning_summary_text.delta", delta: "thinking" },
      { type: "response.output_text.delta", delta: "answer" },
      { type: "response.completed" },
    )
    // Rendering reasoning as the answer would show the user the model's
    // scratchpad.
    expect(collectCodexText(frames)).toBe("answer")
  })

  it("survives [DONE] and malformed frames", () => {
    const frames = splitSseFrames(
      [
        'data: {"type":"response.output_text.delta","delta":"ok"}',
        "data: {not json",
        "data: [DONE]",
      ].join("\n\n"),
    )
    expect(collectCodexText(frames)).toBe("ok")
  })

  it("returns empty when the stream carried no text", () => {
    expect(collectCodexText(sse({ type: "response.completed" }))).toBe("")
  })
})

describe("splitSseFrames", () => {
  it("handles CRLF and multi-line blocks", () => {
    const body = 'data: {"type":"a"}\r\n\r\ndata: {"type":"b"}\r\n'
    expect(splitSseFrames(body)).toHaveLength(2)
  })

  it("drops non-data lines such as event: and comments", () => {
    const body = 'event: message\ndata: {"type":"a"}\n\n: keep-alive\n'
    expect(splitSseFrames(body)).toEqual(['data: {"type":"a"}'])
  })
})

describe("codexAccountId", () => {
  it("reads the account id out of the token claim", () => {
    const payload = { "https://api.openai.com/auth": { chatgpt_account_id: "acct-1" } }
    const segment = btoa(JSON.stringify(payload))
      .replace(/\+/g, "-")
      .replace(/\//g, "_")
      .replace(/=+$/, "")
    expect(codexAccountId(`h.${segment}.s`)).toBe("acct-1")
  })

  it("returns undefined for a token without the claim", () => {
    const segment = btoa(JSON.stringify({ sub: "x" }))
    expect(codexAccountId(`h.${segment}.s`)).toBeUndefined()
    expect(codexAccountId("not-a-jwt")).toBeUndefined()
  })
})

describe("collectGeminiText", () => {
  function geminiFrames(...chunks: unknown[]): string[] {
    return splitSseFrames(chunks.map((c) => `data: ${JSON.stringify(c)}`).join("\n\n"))
  }

  it("reads text out of the nested response envelope", () => {
    const frames = geminiFrames(
      { response: { candidates: [{ content: { parts: [{ text: "he" }] } }] } },
      { response: { candidates: [{ content: { parts: [{ text: "llo" }] } }] } },
    )
    expect(collectGeminiText(frames).text).toBe("hello")
  })

  it("skips parts flagged as thought", () => {
    const frames = geminiFrames({
      response: {
        candidates: [
          { content: { parts: [{ text: "reasoning", thought: true }, { text: "answer" }] } },
        ],
      },
    })
    expect(collectGeminiText(frames).text).toBe("answer")
  })

  it("carries usage metadata when the stream reports it", () => {
    const frames = geminiFrames({
      response: {
        candidates: [{ content: { parts: [{ text: "hi" }] } }],
        usageMetadata: { promptTokenCount: 5, candidatesTokenCount: 2 },
      },
    })
    expect(collectGeminiText(frames).usage).toEqual({ prompt_tokens: 5, completion_tokens: 2 })
  })

  it("returns empty text rather than throwing on an empty stream", () => {
    expect(collectGeminiText([]).text).toBe("")
  })
})
