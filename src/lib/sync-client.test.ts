/** @vitest-environment jsdom */
import { act, renderHook, waitFor } from "@testing-library/react"
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest"

type Handler = (event: { payload: unknown }) => void
type Responder = (args: Record<string, unknown> | undefined) => unknown

const mocks = vi.hoisted(() => ({
  responders: new Map<string, Responder>(),
  handlers: new Map<string, Handler>(),
}))

const invoke = vi.hoisted(() =>
  vi.fn(async (cmd: string, args?: Record<string, unknown>) => {
    const r = mocks.responders.get(cmd)
    if (!r) throw { code: "unmocked", message: cmd }
    return r(args)
  }),
)
const listen = vi.hoisted(() =>
  vi.fn(async (name: string, handler: Handler) => {
    mocks.handlers.set(name, handler)
    return () => mocks.handlers.delete(name)
  }),
)

vi.mock("@tauri-apps/api/core", () => ({ invoke }))
vi.mock("@tauri-apps/api/event", () => ({ listen }))
vi.mock("./runtime", () => ({ isTauri: () => true }))

const P = "sayknow:"
const QUEUE = P + "sync-migration-queue"

async function load() {
  const sync = await import("./sync-client")
  const chat = await import("./chat-history")
  const hist = await import("./history")
  return { sync, conversations: chat.conversations, diffMessages: chat.diffMessages, history: hist.history }
}

function queue(): { nextQseq: number; items: { qseq: number; op: { op: string } & Record<string, unknown> }[] } {
  return JSON.parse(localStorage.getItem(QUEUE) ?? '{"nextQseq":1,"items":[]}')
}

function calls(cmd: string) {
  return invoke.mock.calls.filter((c) => c[0] === cmd)
}

function status(migration: string, extra: Record<string, unknown> = {}) {
  mocks.responders.set("sync_get_status", () => ({ enabled: true, vault: { id: "v" }, migration, ...extra }))
}

async function sha(s: string): Promise<string> {
  const d = await crypto.subtle.digest("SHA-256", new TextEncoder().encode(s))
  return Array.from(new Uint8Array(d), (b) => b.toString(16).padStart(2, "0")).join("")
}

beforeEach(() => {
  vi.resetModules()
  localStorage.clear()
  invoke.mockClear()
  listen.mockClear()
  mocks.responders.clear()
  mocks.handlers.clear()
})

afterEach(() => {
  vi.restoreAllMocks()
  vi.unstubAllGlobals()
})

describe("flag off", () => {
  it("makes exactly one status invoke and writes localStorage as before", async () => {
    let n = 0
    vi.spyOn(crypto, "randomUUID").mockImplementation(() => `00000000-0000-4000-8000-00000000000${++n}` as `${string}-${string}-${string}-${string}-${string}`)
    vi.spyOn(Date, "now").mockReturnValue(1000)
    mocks.responders.set("sync_get_status", () => ({ enabled: false, migration: "none" }))
    const { sync, conversations, history } = await load()
    await sync.bootSync()
    expect(sync.syncState().mode).toBe("off")

    const conv = conversations.create()
    conversations.appendMessage(conv.id, {
      role: "user",
      content: "hello",
      images: [{ id: "i", mimeType: "image/png", data: "AAAA", width: 1, height: 1 }],
    })
    conversations.rename(conv.id, "renamed")
    history.add({ source: "a", target: "b", from: "en", to: "ko", model: "m" })
    history.add({ source: "c", target: "d", from: "en", to: "ko", model: "m" })
    history.togglePin("00000000-0000-4000-8000-000000000003")
    history.remove("00000000-0000-4000-8000-000000000004")
    history.clear()
    conversations.setMessages(conv.id, [])

    const id = (k: number) => `00000000-0000-4000-8000-00000000000${k}`
    expect(Object.keys(localStorage).sort()).toEqual([P + "conversations", P + "current-conversation", P + "history"])
    expect(JSON.parse(localStorage.getItem(P + "conversations")!)).toEqual([
      { id: id(1), title: "renamed", messages: [], createdAt: 1000, updatedAt: 1000 },
    ])
    expect(localStorage.getItem(P + "current-conversation")).toBe(id(1))
    expect(JSON.parse(localStorage.getItem(P + "history")!)).toEqual([
      { source: "a", target: "b", from: "en", to: "ko", model: "m", id: id(3), ts: 1000, pinned: true },
    ])
    expect(invoke.mock.calls).toEqual([["sync_get_status"]])
    expect(listen).not.toHaveBeenCalled()
  })

  it("treats a status error as off", async () => {
    mocks.responders.set("sync_get_status", () => {
      throw { code: "io", message: "x" }
    })
    const { sync, conversations } = await load()
    await sync.bootSync()
    conversations.create()
    expect(sync.syncState().mode).toBe("off")
    expect(localStorage.getItem(QUEUE)).toBeNull()
    expect(invoke).toHaveBeenCalledTimes(1)
  })
})

describe("queue", () => {
  function seed(items: number[]) {
    localStorage.setItem(
      QUEUE,
      JSON.stringify({ nextQseq: Math.max(...items) + 1, items: items.map((q) => ({ qseq: q, op: { op: "tr_delete", tr: `t${q}` } })) }),
    )
  }

  it("replays after a crash mid-flush with no duplicate and no gap", async () => {
    localStorage.setItem(P + "sync-native", "true")
    seed([1, 2, 3])
    status("native")
    mocks.responders.set("sync_chat_load", () => ({ conversations: [], history: [] }))
    let applied = 0
    mocks.responders.set("sync_queue_apply", () => {
      if (++applied === 2) throw { code: "io", message: "crash" }
      return true
    })
    const first = await load()
    await first.sync.bootSync()
    expect(calls("sync_queue_apply").map((c) => c[1]?.qseq)).toEqual([1, 2])
    expect(queue().items.map((i) => i.qseq)).toEqual([2, 3])
    expect(calls("sync_chat_load")).toHaveLength(0)

    // Reload: the rest is flushed in order, item 1 is never re-sent.
    vi.resetModules()
    invoke.mockClear()
    mocks.responders.set("sync_queue_apply", () => true)
    const second = await load()
    await second.sync.bootSync()
    expect(calls("sync_queue_apply").map((c) => c[1]?.qseq)).toEqual([2, 3])
    expect(queue().items).toEqual([])
    expect(queue().nextQseq).toBe(4)
    expect(calls("sync_chat_load")).toHaveLength(1)
  })

  it("re-sends the same qseq when the removal did not persist", async () => {
    localStorage.setItem(P + "sync-native", "true")
    seed([7])
    status("native")
    mocks.responders.set("sync_chat_load", () => ({ conversations: [], history: [] }))
    const real = localStorage.setItem.bind(localStorage)
    const spy = vi.spyOn(Storage.prototype, "setItem").mockImplementation(function (this: Storage, k: string, v: string) {
      if (k === QUEUE) throw new DOMException("quota", "QuotaExceededError")
      real(k, v)
    })
    mocks.responders.set("sync_queue_apply", () => true)
    const first = await load()
    await first.sync.bootSync()
    expect(queue().items.map((i) => i.qseq)).toEqual([7])
    spy.mockRestore()

    vi.resetModules()
    invoke.mockClear()
    const second = await load()
    await second.sync.bootSync()
    // Same qseq: Rust skips one it already flushed (exactly once).
    expect(calls("sync_queue_apply").map((c) => c[1]?.qseq)).toEqual([7])
    expect(queue().items).toEqual([])
  })

  it("a WrongState reply leaves native and keeps the item queued", async () => {
    localStorage.setItem(P + "sync-native", "true")
    seed([1])
    status("native")
    mocks.responders.set("sync_queue_apply", () => {
      throw { code: "migration_state", message: "verified" }
    })
    const { sync } = await load()
    await sync.bootSync()
    expect(sync.isNative()).toBe(false)
    expect(localStorage.getItem(P + "sync-native")).toBeNull()
    expect(queue().items.map((i) => i.qseq)).toEqual([1])
  })

  it("never reuses a qseq", async () => {
    seed([1, 2])
    status("verified")
    mocks.responders.set("sync_migrate_finalize", () => ({ result: "deferred", attempts: 3, mismatches: 1 }))
    const { sync, history } = await load()
    await sync.bootSync()
    history.add({ source: "x", target: "y", from: "en", to: "ko", model: "m" })
    expect(queue().items.map((i) => i.qseq)).toEqual([1, 2, 3])
    expect(queue().nextQseq).toBe(4)
  })
})

describe("write-hold", () => {
  it("dual-writes localStorage and the queue from the push until native", async () => {
    status("none")
    mocks.responders.set("sync_migrate_import_webstore", () => "verified")
    mocks.responders.set("sync_migrate_finalize", () => ({ result: "deferred", attempts: 3, mismatches: 2 }))
    const { sync, conversations, history } = await load()
    await sync.bootSync()
    expect(sync.syncState()).toMatchObject({ mode: "hold", finalize: "deferred" })

    const conv = conversations.create()
    const msg = conversations.appendMessage(conv.id, { role: "user", content: "hi" })
    conversations.rename(conv.id, "t")
    history.add({ source: "a", target: "b", from: "en", to: "ko", model: "m" })
    const tr = history.list()[0]
    history.togglePin(tr.id)
    history.clear()
    conversations.delete(conv.id)

    expect(JSON.parse(localStorage.getItem(P + "conversations")!)).toEqual([])
    expect(JSON.parse(localStorage.getItem(P + "history")!)).toMatchObject([{ id: tr.id, pinned: true }])
    expect(queue().items.map((i) => i.op)).toEqual([
      { op: "conv_create", conv: conv.id, title: "새 대화" },
      { op: "msg_add", conv: conv.id, msg: { id: msg.id, role: "user", content: "hi", ts: msg.ts, images: [] }, parent: null },
      { op: "set_active", conv: conv.id, leaf: msg.id },
      { op: "rename", conv: conv.id, title: "hi" },
      { op: "rename", conv: conv.id, title: "t" },
      { op: "tr_add", tr: { ...tr, pinned: false } },
      { op: "tr_pin", tr: tr.id, pinned: true },
      { op: "tr_clear", observedIds: [] },
      { op: "conv_delete", conv: conv.id },
    ])
    // Nothing is flushed before native.
    expect(calls("sync_queue_apply")).toHaveLength(0)
  })

  it("eviction never enqueues a delete", async () => {
    const map = new Map<string, string>()
    vi.stubGlobal("localStorage", {
      getItem: (k: string) => map.get(k) ?? null,
      setItem: (k: string, v: string) => {
        if (k === P + "conversations" && v.length > 900) throw new DOMException("quota", "QuotaExceededError")
        map.set(k, v)
      },
      removeItem: (k: string) => void map.delete(k),
      clear: () => map.clear(),
      key: (i: number) => [...map.keys()][i] ?? null,
      get length() {
        return map.size
      },
    })
    status("verified")
    mocks.responders.set("sync_migrate_finalize", () => ({ result: "deferred", attempts: 3, mismatches: 0 }))
    const { sync, conversations } = await load()
    await sync.bootSync()
    expect(sync.isQueuing()).toBe(true)
    const big = "x".repeat(300)
    const conv = conversations.create()
    for (const content of ["a", "b", "c"]) {
      conversations.appendMessage(conv.id, {
        role: "user",
        content,
        images: [{ id: content, mimeType: "image/png", data: big, width: 1, height: 1 }],
      })
    }
    // The original shed its oldest pixels; the queue kept every add.
    expect(conversations.get(conv.id)?.messages[0].images?.[0].data).toBe("")
    const q = JSON.parse(map.get(QUEUE)!) as ReturnType<typeof queue>
    expect(q.items.some((i) => i.op.op.endsWith("delete") || i.op.op === "tr_clear")).toBe(false)
    const adds = q.items.filter((i) => i.op.op === "msg_add")
    expect(adds).toHaveLength(3)
    expect(adds.every((i) => (i.op.msg as { images: { data: string }[] }).images[0].data === big)).toBe(true)
  })
})

describe("setMessages diff", () => {
  const m = (id: string, role: "user" | "assistant" = "user") => ({ id, role, content: id, ts: 1 })

  it("adds new ids with the previous message as parent, then activates the last", async () => {
    const { diffMessages } = await load()
    expect(diffMessages("c", [m("a")], [m("a"), m("b", "assistant"), m("c")])).toEqual([
      { op: "msg_add", conv: "c", msg: { id: "b", role: "assistant", content: "b", ts: 1, images: [] }, parent: "a" },
      { op: "msg_add", conv: "c", msg: { id: "c", role: "user", content: "c", ts: 1, images: [] }, parent: "b" },
      { op: "set_active", conv: "c", leaf: "c" },
    ])
    expect(diffMessages("c", [], [m("a")])).toEqual([
      { op: "msg_add", conv: "c", msg: { id: "a", role: "user", content: "a", ts: 1, images: [] }, parent: null },
      { op: "set_active", conv: "c", leaf: "a" },
    ])
  })

  it("a pure truncation only moves the active leaf", async () => {
    const { diffMessages } = await load()
    expect(diffMessages("c", [m("a"), m("b"), m("c")], [m("a")])).toEqual([{ op: "set_active", conv: "c", leaf: "a" }])
  })

  it("an empty list or an unchanged list emits nothing", async () => {
    const { diffMessages } = await load()
    expect(diffMessages("c", [m("a"), m("b")], [])).toEqual([])
    expect(diffMessages("c", [m("a"), m("b")], [m("a"), m("b")])).toEqual([])
  })

  it("regenerate under native keeps the sibling natively and the hidden prefix locally", async () => {
    localStorage.setItem(P + "sync-native", "true")
    status("native")
    const msgs = Array.from({ length: 205 }, (_, i) => ({ id: `m${i}`, role: i % 2 ? "assistant" : "user", content: "", ts: i + 1, images: [] }))
    mocks.responders.set("sync_chat_load", () => ({ conversations: [{ id: "c", title: "t", messages: msgs, updatedAt: 205 }], history: [] }))
    mocks.responders.set("sync_queue_apply", () => true)
    const { sync, conversations } = await load()
    await sync.bootSync()
    const shown = conversations.get("c")!.messages
    expect(shown).toHaveLength(200) // display-only trim
    // Regenerate drops the trailing turn: only the active leaf moves.
    conversations.setMessages("c", shown.slice(0, -1))
    await vi.waitFor(() => expect(queue().items).toEqual([]))
    expect(calls("sync_queue_apply").map((c) => c[1]?.op)).toEqual([{ op: "set_active", conv: "c", leaf: "m203" }])
    expect(sync.cachedConversations()[0].messages).toHaveLength(204)
    expect(sync.cachedConversations()[0].messages[0].id).toBe("m0")
  })
})

describe("migration boot", () => {
  it("pushes the canonical snapshot in chunks under one hash", async () => {
    localStorage.setItem(
      P + "conversations",
      JSON.stringify([
        { id: "c1", title: "가", createdAt: 1, updatedAt: 2, messages: [{ id: "m1", role: "user", content: "😀 hi", ts: 2, model: "x", images: [{ id: "i", mimeType: "image/png", data: "AA", width: 1, height: 2 }] }] },
      ]),
    )
    localStorage.setItem(P + "history", JSON.stringify([{ id: "t1", ts: 3, source: "s", target: "t", from: "en", to: "ko", model: "m", pinned: true }]))
    status("none")
    const chunks: string[] = []
    mocks.responders.set("sync_migrate_import_webstore", (a) => {
      chunks.push(a!.chunk as string)
      return a!.index === (a!.total as number) - 1 ? "verified" : "none"
    })
    mocks.responders.set("sync_migrate_finalize", () => ({ result: "deferred", attempts: 3, mismatches: 0 }))
    const { sync } = await load()
    sync.setChunkUnitsForTests(17)
    await sync.bootSync()
    const text = chunks.join("")
    expect(text).toBe(
      '{"conversations":[{"id":"c1","title":"가","messages":[{"id":"m1","role":"user","content":"😀 hi","ts":2,"model":"x","images":[{"id":"i","mimeType":"image/png","data":"AA","width":1,"height":2}]}]}],"history":[{"id":"t1","ts":3,"source":"s","target":"t","from":"en","to":"ko","model":"m","pinned":true}]}',
    )
    const pushes = calls("sync_migrate_import_webstore")
    expect(pushes.length).toBeGreaterThan(2)
    expect(new Set(pushes.map((c) => c[1]?.snapshotHash))).toEqual(new Set([await sha(text)]))
    expect(pushes.map((c) => c[1]?.index)).toEqual(pushes.map((_, i) => i))
    // No chunk ends on a high surrogate.
    for (const c of chunks) {
      const last = c.charCodeAt(c.length - 1)
      expect(last >= 0xd800 && last < 0xdc00).toBe(false)
    }
    expect(calls("sync_migrate_finalize")[0][1]).toMatchObject({ webviewHash: await sha(text), current: null, queueTail: 0 })
  })

  it("M6: resends bytes on NeedSnapshot, retries a failed verify, then flushes on native", async () => {
    status("verified")
    localStorage.setItem(QUEUE, JSON.stringify({ nextQseq: 3, items: [1, 2].map((q) => ({ qseq: q, op: { op: "tr_delete", tr: `t${q}` } })) }))
    const outs = [
      { result: "need_snapshot" },
      { result: "verify_failed", attempts: 1, mismatches: 1 },
      { result: "need_snapshot" },
      { result: "native", ack_qseq: 2 },
    ]
    mocks.responders.set("sync_migrate_finalize", () => outs.shift())
    mocks.responders.set("sync_queue_apply", () => true)
    mocks.responders.set("sync_chat_load", () => ({ conversations: [], history: [] }))
    const { sync } = await load()
    await sync.bootSync()
    const fin = calls("sync_migrate_finalize").map((c) => c[1]!)
    expect(fin).toHaveLength(4)
    const text = sync.snapshotText()
    const h = await sha(text)
    // Lowercase hex SHA-256 (FIPS 180-2 vector).
    expect(await sync.snapshotHash("abc")).toBe("ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad")
    expect(fin[0]).toMatchObject({ webviewHash: h, current: null, queueTail: 2 })
    expect(fin[1]).toMatchObject({ webviewHash: h, current: text, currentHash: h, queueTail: 2 })
    expect(sync.isNative()).toBe(true)
    expect(localStorage.getItem(P + "sync-native")).toBe("true")
    expect(calls("sync_queue_apply").map((c) => c[1]?.qseq)).toEqual([1, 2])
    expect(queue().items).toEqual([])
  })

  it("Deferred stays on localStorage and keeps the queue", async () => {
    status("verified")
    localStorage.setItem(QUEUE, JSON.stringify({ nextQseq: 2, items: [{ qseq: 1, op: { op: "tr_delete", tr: "t" } }] }))
    mocks.responders.set("sync_migrate_finalize", () => ({ result: "deferred", attempts: 3, mismatches: 4 }))
    const { sync, conversations } = await load()
    await sync.bootSync()
    expect(sync.syncState()).toMatchObject({ mode: "hold", finalize: "deferred" })
    expect(calls("sync_migrate_finalize")).toHaveLength(1)
    expect(calls("sync_queue_apply")).toHaveLength(0)
    expect(calls("sync_chat_load")).toHaveLength(0)
    expect(queue().items).toHaveLength(1)
    conversations.create()
    expect(JSON.parse(localStorage.getItem(P + "conversations")!)).toHaveLength(1)
  })

  it("a finalize error keeps write-hold and reports the code", async () => {
    status("verified")
    mocks.responders.set("sync_migrate_finalize", () => {
      throw { code: "not_this_boot", message: "x" }
    })
    const { sync } = await load()
    await sync.bootSync()
    expect(sync.syncState()).toMatchObject({ mode: "hold", error: "not_this_boot" })
    expect(sync.isNative()).toBe(false)
  })
})

describe("native cache", () => {
  const ws = (title: string) => ({
    conversations: [{ id: "c1", title, updatedAt: 9, messages: [{ id: "m1", role: "user", content: "x", ts: 5, images: [{ id: "i", mimeType: "image/png", data: "", width: 1, height: 1 }] }] }],
    history: [{ id: "t1", ts: 4, source: "s", target: "t", from: "en", to: "ko", model: "m", pinned: false }],
  })

  it("hydrates at boot and refreshes on sync:chat-changed", async () => {
    status("native")
    let title = "first"
    mocks.responders.set("sync_chat_load", () => ws(title))
    const { sync, conversations, history } = await load()
    const seen = vi.fn()
    sync.subscribe(seen)
    await sync.bootSync()
    expect(conversations.list()).toEqual([
      { id: "c1", title: "first", createdAt: 5, updatedAt: 9, messages: [{ id: "m1", role: "user", content: "x", ts: 5, images: [{ id: "i", mimeType: "image/png", data: "", width: 1, height: 1 }] }] },
    ])
    expect(history.list()).toEqual([{ id: "t1", ts: 4, source: "s", target: "t", from: "en", to: "ko", model: "m", pinned: false }])
    expect(seen).toHaveBeenCalled()
    // Native writes never touch the originals.
    expect(localStorage.getItem(P + "conversations")).toBeNull()

    title = "second"
    seen.mockClear()
    await act(async () => {
      mocks.handlers.get("sync:chat-changed")!({ payload: {} })
      await vi.waitFor(() => expect(seen).toHaveBeenCalled())
    })
    expect(conversations.get("c1")?.title).toBe("second")
  })

  it("writes go to the cache and the queue, then flush", async () => {
    status("native")
    mocks.responders.set("sync_chat_load", () => ws("t"))
    mocks.responders.set("sync_queue_apply", () => true)
    const { sync, history } = await load()
    await sync.bootSync()
    history.togglePin("t1")
    expect(history.list()[0].pinned).toBe(true)
    await vi.waitFor(() => expect(queue().items).toEqual([]))
    expect(calls("sync_queue_apply").map((c) => c[1])).toEqual([{ qseq: 1, op: { op: "tr_pin", tr: "t1", pinned: true } }])
    expect(localStorage.getItem(P + "history")).toBeNull()
  })

  it("useHistory re-reads on the cache notify", async () => {
    status("native")
    let title = "a"
    mocks.responders.set("sync_chat_load", () => ({ ...ws(title), history: title === "a" ? [] : ws(title).history }))
    const { sync } = await load()
    const { useHistory } = await import("@/hooks/useHistory")
    const { result } = renderHook(() => useHistory())
    await act(async () => {
      await sync.bootSync()
    })
    expect(result.current.entries).toEqual([])
    title = "b"
    await act(async () => {
      await sync.refresh()
    })
    await waitFor(() => expect(result.current.entries.map((e) => e.id)).toEqual(["t1"]))
  })
})

// ---------------------------------------------------------------------------
// G8 pass-1 review decisions E1–E5

type Op = { op: string } & Record<string, unknown>
type Msg = { id: string; role: string; content: string; ts: number; images: unknown[] }

const NATIVE = P + "sync-native"
const nmsg = (id: string, role = "user", ts = 1): Msg => ({ id, role, content: id, ts, images: [] })
const nconv = (id: string, messages: Msg[]) => ({ id, title: "t", messages, updatedAt: 1 })
const entry = { source: "x", target: "y", from: "en" as const, to: "ko" as const, model: "m" }

function deferred<T>() {
  let resolve!: (v: T) => void
  const promise = new Promise<T>((r) => {
    resolve = r
  })
  return { promise, resolve }
}

function applied(): { qseq: number; op: Op }[] {
  return calls("sync_queue_apply").map((c) => c[1] as { qseq: number; op: Op })
}

/** Rust stand-in: exactly once by qseq (E3 result shape), active paths. */
function sim(seed: { conversations: ReturnType<typeof nconv>[]; history: unknown[] }) {
  const convs = new Map(seed.conversations.map((c) => [c.id, { ...c, messages: [...c.messages] }]))
  const hist = new Map((seed.history as { id: string }[]).map((t) => [t.id, t]))
  let flushed = 0
  mocks.responders.set("sync_queue_apply", (a) => {
    const qseq = a!.qseq as number
    if (qseq <= flushed) return { result: "duplicate" }
    flushed = qseq
    const op = a!.op as Op
    if (op.op === "msg_add") {
      const c = convs.get(op.conv as string)!
      const at = op.parent ? c.messages.findIndex((x) => x.id === op.parent) + 1 : 0
      c.messages = [...c.messages.slice(0, at), op.msg as Msg]
    } else if (op.op === "set_active") {
      const c = convs.get(op.conv as string)!
      const at = c.messages.findIndex((x) => x.id === op.leaf)
      if (at >= 0) c.messages = c.messages.slice(0, at + 1)
    } else if (op.op === "conv_create") {
      convs.set(op.conv as string, nconv(op.conv as string, []))
    } else if (op.op === "tr_add") {
      const t = op.tr as { id: string }
      hist.set(t.id, t)
    }
    return { result: "applied" }
  })
  mocks.responders.set("sync_chat_load", () => ({ conversations: [...convs.values()], history: [...hist.values()] }))
  return { convs, hist }
}

function quotaOn(key: string, on: () => boolean = () => true) {
  const real = Storage.prototype.setItem
  return vi.spyOn(Storage.prototype, "setItem").mockImplementation(function (this: Storage, k: string, v: string) {
    if (k === key && on()) throw new DOMException("quota", "QuotaExceededError")
    real.call(this, k, v)
  })
}

describe("E2 durable or refused", () => {
  it("native: a quota-failed queue write is applied directly; the cache keeps it across a hydrate", async () => {
    status("native")
    const rust = sim({ conversations: [nconv("c1", [nmsg("m1")])], history: [] })
    const { sync, conversations } = await load()
    await sync.bootSync()
    const spy = quotaOn(QUEUE)
    const m = conversations.appendMessage("c1", { role: "user", content: "x" })
    expect(sync.syncState().error).toBe("queue_write_failed")
    expect(conversations.get("c1")!.messages.map((x) => x.id)).toEqual(["m1", m.id])
    await vi.waitFor(() => expect(applied().map((a) => a.op.op)).toEqual(["msg_add", "set_active"]))
    expect(localStorage.getItem(QUEUE)).toBeNull()
    spy.mockRestore()
    expect(rust.convs.get("c1")!.messages.map((x) => x.id)).toEqual(["m1", m.id])
    await act(async () => {
      await sync.refresh()
    })
    expect(conversations.get("c1")!.messages.map((x) => x.id)).toEqual(["m1", m.id])
    expect(sync.syncState().writeFailures).toBe(0)
  })

  it("native: queue write and direct apply both fail -> the cache is unchanged and useChat shows an error", async () => {
    status("native")
    sim({ conversations: [nconv("c1", [nmsg("m1")])], history: [] })
    const { sync, conversations, history } = await load()
    const { useChat } = await import("@/hooks/useChat")
    const hook = renderHook(() => useChat({ apiKey: "k", baseURL: "u", model: "m" }))
    await act(async () => {
      await sync.bootSync()
    })
    mocks.responders.set("sync_queue_apply", () => {
      throw { code: "io", message: "disk" }
    })
    quotaOn(QUEUE)
    await act(async () => {
      conversations.appendMessage("c1", { role: "user", content: "lost?" })
      history.add(entry)
      await vi.waitFor(() => expect(sync.syncState().writeFailures).toBeGreaterThan(0))
    })
    expect(conversations.get("c1")!.messages.map((x) => x.id)).toEqual(["m1"])
    expect(history.list()).toEqual([])
    expect(sync.syncState().error).toBe("io")
    await waitFor(() => expect(hook.result.current.error).toBe("대화를 저장하지 못했습니다. 다시 시도해 주세요."))
  })

  it("write-hold: a quota-failed queue write is surfaced and applied directly at the cutover", async () => {
    localStorage.setItem(P + "conversations", JSON.stringify([{ ...nconv("c1", [nmsg("m1")]), createdAt: 1 }]))
    status("verified")
    let quota = false
    quotaOn(QUEUE, () => quota)
    const mods = await load()
    let late = ""
    let errorSeen: string | null = null
    mocks.responders.set("sync_migrate_finalize", (a) => {
      quota = true
      late = mods.conversations.appendMessage("c1", { role: "user", content: "late" }).id
      errorSeen = mods.sync.syncState().error
      quota = false
      return { result: "native", ack_qseq: a!.queueTail }
    })
    const rust = sim({ conversations: [nconv("c1", [nmsg("m1")])], history: [] })
    await mods.sync.bootSync()
    expect(errorSeen).toBe("queue_write_failed")
    expect(JSON.parse(localStorage.getItem(P + "conversations")!)[0].messages.map((x: Msg) => x.id)).toEqual(["m1", late])
    expect(applied().map((a) => a.op.op)).toEqual(["msg_add", "set_active"])
    expect(rust.convs.get("c1")!.messages.map((x) => x.id)).toEqual(["m1", late])
    expect(mods.conversations.get("c1")!.messages.map((x) => x.id)).toEqual(["m1", late])
  })
})

describe("E3 qseq integrity", () => {
  it("stale_qseq renumbers the item and every later one from watermark+1 (persisted) and retries", async () => {
    localStorage.setItem(NATIVE, "true")
    localStorage.setItem(
      QUEUE,
      JSON.stringify({ nextQseq: 4, items: [1, 2, 3].map((q) => ({ qseq: q, op: { op: "tr_delete", tr: `t${q}` } })) }),
    )
    status("native")
    mocks.responders.set("sync_chat_load", () => ({ conversations: [], history: [] }))
    const seen: number[] = []
    const gate = deferred<void>()
    mocks.responders.set("sync_queue_apply", async (a) => {
      const q = a!.qseq as number
      seen.push(q)
      if (q === 2) return { result: "stale_qseq", watermark: 9 }
      if (q === 11) await gate.promise
      return { result: "applied" }
    })
    const { sync } = await load()
    const boot = sync.bootSync()
    await vi.waitFor(() => expect(seen).toEqual([1, 2, 10, 11]))
    // Renumbered before the retry, so a reload resends the new numbers.
    expect(queue()).toEqual({
      nextQseq: 12,
      items: [
        { qseq: 11, op: { op: "tr_delete", tr: "t3" } },
      ],
    })
    gate.resolve()
    await boot
    expect(applied().map((a) => [a.qseq, a.op.tr])).toEqual([
      [1, "t1"],
      [2, "t2"],
      [10, "t2"],
      [11, "t3"],
    ])
    expect(queue()).toEqual({ nextQseq: 12, items: [] })
  })

  it("floors nextQseq at the status watermark", async () => {
    status("native", { queue_flushed_qseq: 41, ack_qseq: 7 })
    mocks.responders.set("sync_chat_load", () => ({ conversations: [], history: [] }))
    mocks.responders.set("sync_queue_apply", () => new Promise(() => {}))
    const { sync, history } = await load()
    await sync.bootSync()
    history.add(entry)
    expect(queue().items.map((i) => i.qseq)).toEqual([42])
    expect(queue().nextQseq).toBe(43)
  })

  it("preserves a corrupt queue under a side key, surfaces it, and keeps the valid items", async () => {
    vi.spyOn(Date, "now").mockReturnValue(1234)
    const raw = JSON.stringify({ nextQseq: 3, items: [{ qseq: 1, op: { op: "tr_delete", tr: "a" } }, { qseq: "2", op: { op: "tr_delete", tr: "b" } }] })
    localStorage.setItem(QUEUE, raw)
    status("verified")
    mocks.responders.set("sync_migrate_finalize", () => ({ result: "deferred", attempts: 3, mismatches: 0 }))
    const { sync, history } = await load()
    await sync.bootSync()
    expect(localStorage.getItem(QUEUE + ".corrupt-1234")).toBe(raw)
    expect(sync.syncState().corruptQueue).toBe("sync-migration-queue.corrupt-1234")
    expect(sync.syncState().error).toBe("queue_corrupt")
    history.add(entry)
    expect(queue().items.map((i) => i.qseq)).toEqual([1, 3])
    expect(localStorage.getItem(QUEUE + ".corrupt-1234")).toBe(raw)
  })

  it("never overwrites corrupt queue bytes it could not preserve", async () => {
    const raw = '{"nextQseq":4,"items":[{"qseq":3,"op":{"op":"tr_del'
    localStorage.setItem(QUEUE, raw)
    const real = Storage.prototype.setItem
    vi.spyOn(Storage.prototype, "setItem").mockImplementation(function (this: Storage, k: string, v: string) {
      if (k.startsWith(QUEUE)) throw new DOMException("quota", "QuotaExceededError")
      real.call(this, k, v)
    })
    status("verified")
    mocks.responders.set("sync_migrate_finalize", () => ({ result: "deferred", attempts: 3, mismatches: 0 }))
    const { sync, history } = await load()
    await sync.bootSync()
    expect(sync.syncState()).toMatchObject({ error: "queue_corrupt", corruptQueue: null })
    history.add(entry)
    expect(localStorage.getItem(QUEUE)).toBe(raw)
    // The write still lands in the original (write-hold safety net).
    expect(JSON.parse(localStorage.getItem(P + "history")!).map((e: { source: string }) => e.source)).toEqual(["x"])
  })
})

describe("E4 display trim is display-only", () => {
  it("editAndResend of the first shown message in a 206-message conversation keeps the hidden prefix", async () => {
    vi.doMock("@/lib/openrouter", () => ({ chat: vi.fn(async () => ({ content: "answer", model: "m" })) }))
    const msgs = Array.from({ length: 206 }, (_, i) => nmsg(`m${i}`, i % 2 ? "assistant" : "user", i + 1))
    status("native")
    const rust = sim({ conversations: [nconv("c1", msgs)], history: [] })
    localStorage.setItem(P + "current-conversation", "c1")
    const { sync } = await load()
    await sync.bootSync()
    const { useChat } = await import("@/hooks/useChat")
    const hook = renderHook(() => useChat({ apiKey: "k", baseURL: "u", model: "m" }))
    const shown = hook.result.current.messages
    expect(shown).toHaveLength(200)
    expect(shown[0].id).toBe("m6") // a user turn: the first one displayed
    invoke.mockClear()
    await act(async () => {
      await hook.result.current.editAndResend("m6", "edited")
    })
    await vi.waitFor(() => expect(queue().items).toEqual([]))
    const ops = applied().map((a) => a.op)
    expect(ops[0]).toEqual({ op: "set_active", conv: "c1", leaf: "m5" })
    expect(ops[1]).toMatchObject({ op: "msg_add", parent: "m5", msg: { content: "edited" } })
    const edited = (ops[1].msg as Msg).id
    expect(ops[3]).toMatchObject({ op: "msg_add", parent: edited, msg: { content: "answer" } })
    const cached = sync.cachedConversations()[0].messages
    expect(cached.map((m) => m.id).slice(0, 6)).toEqual(["m0", "m1", "m2", "m3", "m4", "m5"])
    expect(cached.slice(6).map((m) => m.content)).toEqual(["edited", "answer"])
    expect(rust.convs.get("c1")!.messages.map((m) => m.content)).toEqual(["m0", "m1", "m2", "m3", "m4", "m5", "edited", "answer"])
    vi.doUnmock("@/lib/openrouter")
  })

  it("setMessages(id, []) on an untrimmed conversation still emits nothing (documented deviation)", async () => {
    status("native")
    sim({ conversations: [nconv("c1", [nmsg("u1"), nmsg("a1", "assistant")])], history: [] })
    const { sync, conversations } = await load()
    await sync.bootSync()
    conversations.setMessages("c1", [])
    expect(conversations.get("c1")!.messages).toEqual([])
    expect(localStorage.getItem(QUEUE)).toBeNull()
  })
})

describe("E5 native boot", () => {
  it("holds writes until the first hydrate and applies them against it (never the frozen originals)", async () => {
    localStorage.setItem(P + "conversations", JSON.stringify([{ ...nconv("c1", [nmsg("m1")]), createdAt: 1 }]))
    localStorage.setItem(NATIVE, "true")
    status("native")
    sim({ conversations: [nconv("c1", [nmsg("m1"), nmsg("m2", "assistant"), nmsg("m3")])], history: [{ id: "t0", ts: 1, ...entry, pinned: false }] })
    const gate = deferred<void>()
    const realLoad = mocks.responders.get("sync_chat_load")!
    mocks.responders.set("sync_chat_load", async (a) => {
      await gate.promise
      return realLoad(a)
    })
    const { sync, conversations, history } = await load()
    const boot = sync.bootSync()
    // Not the frozen originals: empty until hydrated.
    expect(conversations.list()).toEqual([])
    expect(sync.isReady()).toBe(false)
    const m = conversations.appendMessage("c1", { role: "assistant", content: "reply" })
    history.add(entry) // a duplicate of the hydrated head: deduped after hydrate
    expect(calls("sync_queue_apply")).toHaveLength(0)
    gate.resolve()
    await boot
    expect(sync.isReady()).toBe(true)
    await vi.waitFor(() => expect(queue().items).toEqual([]))
    expect(applied().map((a) => a.op)).toEqual([
      { op: "msg_add", conv: "c1", msg: expect.objectContaining({ id: m.id }), parent: "m3" },
      { op: "set_active", conv: "c1", leaf: m.id },
    ])
    expect(conversations.get("c1")!.messages.map((x) => x.id)).toEqual(["m1", "m2", "m3", m.id])
    expect(localStorage.getItem(P + "conversations")).toContain('"m1"')
    expect(localStorage.getItem(P + "conversations")).not.toContain(m.id)
  })

  it("retries a failed hydrate with backoff, surfaces the error, then releases held writes", async () => {
    vi.useFakeTimers()
    try {
      localStorage.setItem(NATIVE, "true")
      status("native")
      sim({ conversations: [], history: [] })
      const realLoad = mocks.responders.get("sync_chat_load")!
      let fail = true
      mocks.responders.set("sync_chat_load", (a) => {
        if (fail) throw { code: "io", message: "x" }
        return realLoad(a)
      })
      const { sync, history } = await load()
      await sync.bootSync()
      expect(sync.syncState().error).toBe("io")
      expect(sync.isReady()).toBe(false)
      expect(calls("sync_chat_load")).toHaveLength(1)
      await vi.advanceTimersByTimeAsync(1000)
      expect(calls("sync_chat_load")).toHaveLength(2)
      expect(sync.isReady()).toBe(false)
      fail = false
      await vi.advanceTimersByTimeAsync(2000)
      expect(calls("sync_chat_load")).toHaveLength(3)
      expect(sync.isReady()).toBe(true)
      expect(sync.syncState().error).toBeNull()
      history.add(entry)
      await vi.advanceTimersByTimeAsync(0)
      expect(applied().map((a) => a.op.op)).toEqual(["tr_add"])
    } finally {
      vi.useRealTimers()
    }
  })

  it("a held write retries the hydrate at once instead of waiting for the backoff", async () => {
    localStorage.setItem(NATIVE, "true")
    status("native")
    sim({ conversations: [], history: [] })
    const realLoad = mocks.responders.get("sync_chat_load")!
    let fail = true
    mocks.responders.set("sync_chat_load", (a) => {
      if (fail) throw { code: "io", message: "x" }
      return realLoad(a)
    })
    const { sync, history } = await load()
    await sync.bootSync()
    fail = false
    history.add(entry)
    await vi.waitFor(() => expect(applied().map((a) => a.op.op)).toEqual(["tr_add"]))
    expect(history.list().map((e) => e.source)).toEqual(["x"])
  })

  it("drops a hydrate reply older than a local write or a newer applied load", async () => {
    status("native")
    sim({ conversations: [], history: [] })
    const { sync, history } = await load()
    await sync.bootSync()
    const slow = deferred<unknown>()
    mocks.responders.set("sync_chat_load", () => slow.promise)
    const refreshing = sync.refresh()
    await vi.waitFor(() => expect(calls("sync_chat_load")).toHaveLength(2))
    history.add(entry)
    await vi.waitFor(() => expect(queue().items).toEqual([]))
    slow.resolve({ conversations: [], history: [] })
    await refreshing
    expect(history.list().map((e) => e.source)).toEqual(["x"])
  })
})

describe("E1 native stays native", () => {
  it("flag off on a native device keeps native reads and writes (no localStorage writes)", async () => {
    localStorage.setItem(NATIVE, "true")
    mocks.responders.set("sync_get_status", () => ({ enabled: false, migration: "native", queue_flushed_qseq: 0, ack_qseq: 0 }))
    sim({ conversations: [nconv("c1", [nmsg("m1")])], history: [] })
    const { sync, conversations, history } = await load()
    await sync.bootSync()
    expect(sync.isNative()).toBe(true)
    expect(conversations.get("c1")!.messages.map((m) => m.id)).toEqual(["m1"])
    const m = conversations.appendMessage("c1", { role: "user", content: "x" })
    history.add(entry)
    await vi.waitFor(() => expect(queue().items).toEqual([]))
    expect(applied().map((a) => a.op.op)).toEqual(["msg_add", "set_active", "tr_add"])
    expect(conversations.get("c1")!.messages.map((x) => x.id)).toEqual(["m1", m.id])
    expect(localStorage.getItem(P + "conversations")).toBeNull()
    expect(localStorage.getItem(P + "history")).toBeNull()
    expect(localStorage.getItem(NATIVE)).toBe("true")
  })

  it("flag off with a stale native marker: held writes land in localStorage after the single status invoke", async () => {
    localStorage.setItem(NATIVE, "true")
    mocks.responders.set("sync_get_status", () => ({ enabled: false, migration: "none" }))
    const { sync, history } = await load()
    const boot = sync.bootSync()
    history.add(entry)
    await boot
    expect(sync.syncState().mode).toBe("off")
    expect(JSON.parse(localStorage.getItem(P + "history")!).map((e: { source: string }) => e.source)).toEqual(["x"])
    expect(invoke.mock.calls).toEqual([["sync_get_status"]])
    expect(localStorage.getItem(NATIVE)).toBeNull()
  })
})

describe("G8 delta decisions", () => {
  it("E2a native + quota with an image goes to a direct apply and the image is preserved", async () => {
    const B64 = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mNk"
    status("native")
    const rust = sim({ conversations: [nconv("c1", [nmsg("m1")])], history: [] })
    const { sync, conversations } = await load()
    await sync.bootSync()
    // A size quota: only a queue value that carries the pixels is too big,
    // so an image-free retry would fit.
    const real = Storage.prototype.setItem
    const spy = vi.spyOn(Storage.prototype, "setItem").mockImplementation(function (this: Storage, k: string, v: string) {
      if (k === QUEUE && v.includes(B64)) throw new DOMException("quota", "QuotaExceededError")
      real.call(this, k, v)
    })
    const m = conversations.appendMessage("c1", {
      role: "user",
      content: "pic",
      images: [{ id: "i1", mimeType: "image/png", data: B64, width: 1, height: 1 }],
    })
    expect(sync.syncState().error).toBe("queue_write_failed")
    await vi.waitFor(() => expect(applied().map((a) => a.op.op)).toEqual(["msg_add", "set_active"]))
    const add = applied()[0].op as unknown as { msg: { images: { data: string }[] } }
    expect(add.msg.images[0].data).toBe(B64)
    // No stripped copy was persisted as the durable write.
    expect(localStorage.getItem(QUEUE) ?? "").not.toContain('"i1"')
    spy.mockRestore()
    await act(async () => {
      await sync.refresh()
    })
    const shown = conversations.get("c1")!.messages.find((x) => x.id === m.id)!
    expect(shown.images![0].data).toBe(B64)
    expect((rust.convs.get("c1")!.messages.at(-1)!.images[0] as { data: string }).data).toBe(B64)
    expect(sync.syncState().writeFailures).toBe(0)
  })

  it("E5a a failed marker write is surfaced and the next boot recovers native mode from status", async () => {
    localStorage.setItem(P + "conversations", JSON.stringify([{ ...nconv("c1", [nmsg("m1")]), createdAt: 1 }]))
    status("verified")
    mocks.responders.set("sync_migrate_finalize", (a) => ({ result: "native", ack_qseq: a!.queueTail }))
    const rust = sim({ conversations: [nconv("c1", [nmsg("m1")])], history: [] })
    const spy = quotaOn(NATIVE)
    const b1 = await load()
    await b1.sync.bootSync()
    expect(b1.sync.isNative()).toBe(true)
    expect(b1.sync.syncState().error).toBe("marker_write_failed")
    spy.mockRestore()
    expect(localStorage.getItem(NATIVE)).toBeNull()

    vi.resetModules()
    invoke.mockClear()
    mocks.handlers.clear()
    status("native")
    const gate = deferred<void>()
    const realStatus = mocks.responders.get("sync_get_status")!
    mocks.responders.set("sync_get_status", async (a) => {
      await gate.promise
      return realStatus(a)
    })
    const b2 = await load()
    const boot = b2.sync.bootSync()
    // No marker: the pre-status write takes the localStorage path ...
    expect(b2.sync.isNative()).toBe(false)
    const m = b2.conversations.appendMessage("c1", { role: "user", content: "before status" })
    gate.resolve()
    await boot
    // ... and the native status re-establishes the mode and the marker and
    // replays the write against the hydrated cache.
    expect(b2.sync.isNative()).toBe(true)
    expect(localStorage.getItem(NATIVE)).toBe("true")
    await vi.waitFor(() => expect(rust.convs.get("c1")!.messages.map((x) => x.id)).toEqual(["m1", m.id]))
    expect(b2.conversations.get("c1")!.messages.map((x) => x.id)).toEqual(["m1", m.id])
    expect(b2.sync.syncState().error).toBeNull()
  })

  it("E5b a no-op write does not drop an in-flight hydrate", async () => {
    status("native")
    sim({ conversations: [], history: [] })
    // Rust treats every write here as a no-op (`changed: false`, no event).
    const applyGate = deferred<void>()
    mocks.responders.set("sync_queue_apply", async () => {
      await applyGate.promise
      return { result: "applied", changed: false }
    })
    const { sync, history } = await load()
    await sync.bootSync()
    const slow = deferred<unknown>()
    mocks.responders.set("sync_chat_load", () => slow.promise)
    const refreshing = sync.refresh()
    await vi.waitFor(() => expect(calls("sync_chat_load")).toHaveLength(2))
    history.add(entry)
    // The newer projection lands while the write is still pending: kept.
    slow.resolve({
      conversations: [],
      history: [{ id: "remote", ts: 5, source: "r", target: "t", from: "en", to: "ko", model: "m", pinned: false }],
    })
    await refreshing
    expect(history.list().map((e) => e.source)).toEqual(["x"])
    applyGate.resolve()
    await vi.waitFor(() => expect(queue().items).toEqual([]))
    await vi.waitFor(() => expect(history.list().map((e) => e.source)).toEqual(["r"]))
    expect(calls("sync_chat_load")).toHaveLength(2)
  })

  it("E1a store_error keeps the current mode and the marker, holds writes and retries", async () => {
    localStorage.setItem(NATIVE, "true")
    localStorage.setItem(P + "conversations", JSON.stringify([{ ...nconv("c1", [nmsg("frozen")]), createdAt: 1 }]))
    const rust = sim({ conversations: [nconv("c1", [nmsg("m1")])], history: [] })
    let statusCalls = 0
    mocks.responders.set("sync_get_status", () =>
      ++statusCalls === 1
        ? { enabled: false, vault: null, migration: "none", store_error: true }
        : { enabled: false, migration: "native" },
    )
    const realLoad = mocks.responders.get("sync_chat_load")!
    let loadFails = 1
    mocks.responders.set("sync_chat_load", (a) => {
      if (loadFails-- > 0) throw { code: "store_error", message: "x" }
      return realLoad(a)
    })
    const { sync, conversations } = await load()
    await sync.bootSync()
    // Not `goOff`: still native, marker kept, frozen originals never shown.
    expect(sync.isNative()).toBe(true)
    expect(sync.isReady()).toBe(false)
    expect(sync.syncState().error).toBe("store_error")
    expect(localStorage.getItem(NATIVE)).toBe("true")
    expect(conversations.list()).toEqual([])
    const m = conversations.appendMessage("c1", { role: "user", content: "held" })
    // The held write retries: status native, then a chat_load store_error
    // (still native, marker kept), then a backoff retry that lands.
    await vi.waitFor(() => expect(calls("sync_chat_load").length).toBeGreaterThanOrEqual(1))
    expect(sync.isNative()).toBe(true)
    expect(localStorage.getItem(NATIVE)).toBe("true")
    await vi.waitFor(() => expect(rust.convs.get("c1")!.messages.map((x) => x.id)).toEqual(["m1", m.id]), {
      timeout: 4000,
    })
    expect(sync.isReady()).toBe(true)
    expect(sync.syncState().error).toBeNull()
    expect(conversations.get("c1")!.messages.map((x) => x.id)).toEqual(["m1", m.id])
    expect(JSON.parse(localStorage.getItem(P + "conversations")!)[0].messages.map((x: Msg) => x.id)).toEqual(["frozen"])
  })

  it("BF1 a boot load superseded by a chat-changed load does not release the held writes; the newer load does", async () => {
    localStorage.setItem(NATIVE, "true")
    status("native")
    const rust = sim({ conversations: [nconv("c1", [nmsg("m1")])], history: [] })
    const realLoad = mocks.responders.get("sync_chat_load")!
    const loads: ReturnType<typeof deferred<void>>[] = []
    mocks.responders.set("sync_chat_load", async (a) => {
      const gate = deferred<void>()
      loads.push(gate)
      await gate.promise
      return realLoad(a)
    })
    const { sync, conversations } = await load()
    const boot = sync.bootSync()
    const m = conversations.appendMessage("c1", { role: "user", content: "held" })
    await vi.waitFor(() => expect(loads).toHaveLength(1))
    // The projection moves while the boot load is in flight.
    mocks.handlers.get("sync:chat-changed")!({ payload: {} })
    await vi.waitFor(() => expect(loads).toHaveLength(2))
    // The superseded boot load resolves first: not a hydrate.
    loads[0].resolve()
    await boot
    expect(sync.isReady()).toBe(false)
    expect(sync.isHydrated()).toBe(false)
    expect(sync.syncState().writeFailures).toBe(0)
    expect(applied()).toEqual([])
    // The newer load hydrates and releases the held write against it.
    loads[1].resolve()
    await vi.waitFor(() => expect(sync.isReady()).toBe(true))
    await vi.waitFor(() => expect(rust.convs.get("c1")!.messages.map((x) => x.id)).toEqual(["m1", m.id]))
    expect(conversations.get("c1")!.messages.map((x) => x.id)).toEqual(["m1", m.id])
    expect(sync.syncState().writeFailures).toBe(0)
    expect(sync.syncState().error).toBeNull()
  })

  it("F09 a kept load an apply moved past is re-read, not applied; the optimistic cache stays while the write is pending", async () => {
    status("native")
    const rust = sim({ conversations: [], history: [] })
    const realApply = mocks.responders.get("sync_queue_apply")!
    const applyGate = deferred<void>()
    mocks.responders.set("sync_queue_apply", async (a) => {
      await applyGate.promise
      return realApply(a)
    })
    const { sync, history } = await load()
    await sync.bootSync()
    const slow = deferred<unknown>()
    mocks.responders.set("sync_chat_load", () => slow.promise)
    const refreshing = sync.refresh()
    await vi.waitFor(() => expect(calls("sync_chat_load")).toHaveLength(2))
    history.add(entry)
    // An older projection (without the write) lands while it is pending: kept.
    slow.resolve({ conversations: [], history: [] })
    await refreshing
    expect(history.list().map((e) => e.source)).toEqual(["x"])
    mocks.responders.set("sync_chat_load", () => ({ conversations: [], history: [...rust.hist.values()] }))
    // The apply moves the projection (`changed` defaults to moved): the kept
    // load is stale and a fresh one is read instead.
    applyGate.resolve()
    await vi.waitFor(() => expect(queue().items).toEqual([]))
    await vi.waitFor(() => expect(calls("sync_chat_load")).toHaveLength(3))
    expect(history.list().map((e) => e.source)).toEqual(["x"])
    expect(sync.syncState().writeFailures).toBe(0)
  })

  it("F09/F14 a store_error leaves a durable item: the cache keeps it and the kept load waits for the backoff retry", async () => {
    status("native")
    const rust = sim({ conversations: [], history: [] })
    const realApply = mocks.responders.get("sync_queue_apply")!
    let fails = 1
    mocks.responders.set("sync_queue_apply", (a) => {
      if (fails-- > 0) throw { code: "store_error", message: "x" }
      return realApply(a)
    })
    const { sync, history } = await load()
    await sync.bootSync()
    // A remote load without the write is in flight when the write fails.
    const slow = deferred<unknown>()
    mocks.responders.set("sync_chat_load", () => slow.promise)
    const refreshing = sync.refresh()
    await vi.waitFor(() => expect(calls("sync_chat_load")).toHaveLength(2))
    vi.useFakeTimers({ toFake: ["setTimeout", "clearTimeout"] })
    try {
      history.add(entry)
      await vi.advanceTimersByTimeAsync(0)
      expect(sync.syncState().error).toBe("store_error")
      // It lands while the durable item is pending: kept, not applied.
      slow.resolve({ conversations: [], history: [] })
      await refreshing
      await vi.advanceTimersByTimeAsync(0)
      expect(queue().items.map((i) => i.op.op)).toEqual(["tr_add"])
      expect(history.list().map((e) => e.source)).toEqual(["x"])
      mocks.responders.set("sync_chat_load", () => ({ conversations: [], history: [...rust.hist.values()] }))
      await vi.advanceTimersByTimeAsync(1000)
    } finally {
      vi.useRealTimers()
    }
    await vi.waitFor(() => expect(queue().items).toEqual([]))
    await vi.waitFor(() => expect(sync.syncState().error).toBeNull())
    expect(history.list().map((e) => e.source)).toEqual(["x"])
    expect(rust.hist.size).toBe(1)
    expect(sync.syncState().writeFailures).toBe(0)
  })
})
