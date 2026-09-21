/** @vitest-environment jsdom */
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest"

/**
 * localStorage that starts refusing writes past a byte budget, the way the
 * real one does at ~5 MB. Lets the eviction order be asserted without
 * generating megabytes of base64.
 */
function limitedStorage(limit: number) {
  const map = new Map<string, string>()
  return {
    getItem: (k: string) => map.get(k) ?? null,
    setItem: (k: string, v: string) => {
      if (v.length > limit) throw new DOMException("quota", "QuotaExceededError")
      map.set(k, v)
    },
    removeItem: (k: string) => void map.delete(k),
    clear: () => map.clear(),
    key: (i: number) => [...map.keys()][i] ?? null,
    get length() {
      return map.size
    },
  }
}

const big = "x".repeat(400)

describe("chat history with images", () => {
  beforeEach(() => {
    vi.resetModules()
    vi.stubGlobal("localStorage", limitedStorage(1_600))
  })
  afterEach(() => {
    vi.unstubAllGlobals()
  })

  it("keeps images when they fit", async () => {
    const { conversations } = await import("./chat-history")
    const conv = conversations.create()
    conversations.appendMessage(conv.id, {
      role: "user",
      content: "look",
      images: [{ id: "a", mimeType: "image/png", data: "AAAA", width: 1, height: 1 }],
    })
    expect(conversations.get(conv.id)?.messages[0].images?.[0].data).toBe("AAAA")
  })

  it("sheds the oldest image payload first and keeps every message's text", async () => {
    const { conversations } = await import("./chat-history")
    const conv = conversations.create()
    conversations.appendMessage(conv.id, {
      role: "user",
      content: "first",
      images: [{ id: "a", mimeType: "image/png", data: big, width: 1, height: 1 }],
    })
    conversations.appendMessage(conv.id, {
      role: "user",
      content: "second",
      images: [{ id: "b", mimeType: "image/png", data: big, width: 1, height: 1 }],
    })
    // Third one pushes the record past the budget.
    conversations.appendMessage(conv.id, {
      role: "user",
      content: "third",
      images: [{ id: "c", mimeType: "image/png", data: big, width: 1, height: 1 }],
    })

    const stored = conversations.get(conv.id)
    expect(stored?.messages.map((m) => m.content)).toEqual(["first", "second", "third"])
    // The slot survives so the bubble can explain itself; only the bytes go.
    expect(stored?.messages[0].images?.[0]).toMatchObject({ id: "a", data: "" })
    expect(stored?.messages[2].images?.[0].data).toBe(big)
  })

  it("titles an image-only opener as an image, not a blank conversation", async () => {
    const { conversations } = await import("./chat-history")
    const conv = conversations.create()
    conversations.appendMessage(conv.id, {
      role: "user",
      content: "",
      images: [{ id: "a", mimeType: "image/png", data: "AAAA", width: 1, height: 1 }],
    })
    expect(conversations.get(conv.id)?.title).toBe("이미지")
  })
})
