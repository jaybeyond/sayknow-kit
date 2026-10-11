/** @vitest-environment jsdom */
/**
 * G8 red team: adversarial cases against the web storage cutover
 * (`sync-client.ts`, `chat-history.ts`, `history.ts`). `invoke` / `listen`
 * are mocked; localStorage is jsdom's in-memory store (quota simulated by a
 * throwing `setItem`). A case that exposes a real defect is `it.skip` with
 * the label "G8 blocker: ...".
 */
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
const NATIVE = P + "sync-native"

type Op = { op: string } & Record<string, unknown>
type Msg = { id: string; role: string; content: string; ts: number; images?: unknown[] }

async function load() {
  const sync = await import("./sync-client")
  const chat = await import("./chat-history")
  const hist = await import("./history")
  return { sync, conversations: chat.conversations, history: hist.history }
}

/** A fresh module graph (app reload / crash restart); localStorage survives. */
async function reboot() {
  vi.resetModules()
  invoke.mockClear()
  mocks.handlers.clear()
  return load()
}

function queue(): { nextQseq: number; items: { qseq: number; op: Op }[] } {
  return JSON.parse(localStorage.getItem(QUEUE) ?? '{"nextQseq":1,"items":[]}')
}

function calls(cmd: string) {
  return invoke.mock.calls.filter((c) => c[0] === cmd)
}

function applied(): { qseq: number; op: Op }[] {
  return calls("sync_queue_apply").map((c) => c[1] as { qseq: number; op: Op })
}

function status(migration: string, extra: Record<string, unknown> = {}) {
  mocks.responders.set("sync_get_status", () => ({ enabled: true, vault: { id: "v" }, migration, ...extra }))
}

function deferred<T>() {
  let resolve!: (v: T) => void
  let reject!: (e: unknown) => void
  const promise = new Promise<T>((r, j) => {
    resolve = r
    reject = j
  })
  return { promise, resolve, reject }
}

async function sha(s: string): Promise<string> {
  const d = await crypto.subtle.digest("SHA-256", new TextEncoder().encode(s))
  return Array.from(new Uint8Array(d), (b) => b.toString(16).padStart(2, "0")).join("")
}

const msg = (id: string, role = "user", ts = 1): Msg => ({ id, role, content: id, ts, images: [] })
const conv = (id: string, messages: Msg[]) => ({ id, title: "t", messages, updatedAt: 1 })
const tr = (id: string, pinned = false, ts = 1) => ({ id, ts, source: `s-${id}`, target: "t", from: "en", to: "ko", model: "m", pinned })
const entry = { source: "x", target: "y", from: "en" as const, to: "ko" as const, model: "m" }

/** Throws a QuotaExceededError for the queue key while `on()` is true. */
function quotaOnQueue(on: () => boolean) {
  const real = Storage.prototype.setItem
  return vi.spyOn(Storage.prototype, "setItem").mockImplementation(function (this: Storage, k: string, v: string) {
    if (k === QUEUE && on()) throw new DOMException("quota", "QuotaExceededError")
    real.call(this, k, v)
  })
}

/**
 * A tiny stand-in for the Rust projection: `sync_queue_apply` folds the ops
 * the way `migration::apply_queued` does for the cases used here, exactly
 * once by qseq; `sync_chat_load` returns the active paths.
 */
function rustSim(seed: { conversations: ReturnType<typeof conv>[]; history: ReturnType<typeof tr>[] }) {
  const convs = new Map(seed.conversations.map((c) => [c.id, { ...c, messages: [...c.messages] }]))
  const hist = new Map(seed.history.map((t) => [t.id, { ...t }]))
  let flushed = 0
  mocks.responders.set("sync_queue_apply", (a) => {
    const qseq = a!.qseq as number
    if (qseq <= flushed) return false
    flushed = qseq
    const op = a!.op as Op
    if (op.op === "msg_add") {
      const c = convs.get(op.conv as string)
      const m = op.msg as Msg
      if (c && !c.messages.some((x) => x.id === m.id)) {
        const at = op.parent ? c.messages.findIndex((x) => x.id === op.parent) + 1 : 0
        c.messages = [...c.messages.slice(0, at), m]
      }
    } else if (op.op === "conv_create") {
      if (!convs.has(op.conv as string)) convs.set(op.conv as string, conv(op.conv as string, []))
    } else if (op.op === "tr_add") {
      const t = op.tr as ReturnType<typeof tr>
      if (!hist.has(t.id)) hist.set(t.id, t)
    } else if (op.op === "tr_clear") {
      for (const id of op.observedIds as string[]) hist.delete(id)
    }
    return true
  })
  mocks.responders.set("sync_chat_load", () => ({ conversations: [...convs.values()], history: [...hist.values()] }))
  return { convs, hist, flushedQseq: () => flushed }
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

// ---------------------------------------------------------------------------
describe("quota", () => {
  it("RT01 quota on the queue during write-hold: original written, error surfaced, next finalize hashes it", async () => {
    localStorage.setItem(P + "conversations", JSON.stringify([conv("c1", [msg("m1")])]))
    status("verified")
    mocks.responders.set("sync_migrate_finalize", () => ({ result: "deferred", attempts: 3, mismatches: 1 }))
    const first = await load()
    await first.sync.bootSync()
    expect(first.sync.syncState().mode).toBe("hold")
    const spy = quotaOnQueue(() => true)
    const m = first.conversations.appendMessage("c1", { role: "user", content: "late" })
    spy.mockRestore()
    expect(first.sync.syncState().error).toBe("queue_write_failed")
    expect(JSON.parse(localStorage.getItem(P + "conversations")!)[0].messages.map((x: Msg) => x.id)).toEqual(["m1", m.id])
    expect(localStorage.getItem(QUEUE)).toBeNull()

    const second = await reboot()
    status("verified")
    await second.sync.bootSync()
    const text = second.sync.snapshotText()
    expect(text).toContain(m.id)
    expect(calls("sync_migrate_finalize")[0][1]).toMatchObject({ webviewHash: await sha(text) })
  })

  it("G8 blocker: RT02 quota on the queue between the M6 snapshot and the Native ack loses the write", async () => {
    localStorage.setItem(P + "conversations", JSON.stringify([{ ...conv("c1", [msg("m1")]), createdAt: 1 }]))
    status("verified")
    const mods = await load()
    let quota = false
    let snap: { conversations: ReturnType<typeof conv>[]; history: [] } = { conversations: [], history: [] }
    let late = ""
    quotaOnQueue(() => quota)
    mocks.responders.set("sync_migrate_finalize", (a) => {
      // Rust holds the snapshot read before this invoke; the user types now.
      snap = JSON.parse(mods.sync.snapshotText())
      quota = true
      late = mods.conversations.appendMessage("c1", { role: "user", content: "typed during finalize" }).id
      quota = false
      return { result: "native", ack_qseq: a!.queueTail }
    })
    mocks.responders.set("sync_queue_apply", () => true)
    mocks.responders.set("sync_chat_load", () => ({ conversations: snap.conversations.map((c) => ({ ...c, updatedAt: 1 })), history: [] }))
    await mods.sync.bootSync()
    expect(mods.sync.isNative()).toBe(true)
    // The message is in the localStorage original only; native never got it.
    expect(mods.conversations.get("c1")!.messages.map((x) => x.id)).toContain(late)
  })

  it("G8 blocker: RT03 quota on the queue under native: the optimistic write vanishes on the next hydrate", async () => {
    status("native")
    rustSim({ conversations: [conv("c1", [msg("m1")])], history: [] })
    const { sync, conversations } = await load()
    await sync.bootSync()
    const spy = quotaOnQueue(() => true)
    const m = conversations.appendMessage("c1", { role: "user", content: "x" })
    spy.mockRestore()
    expect(sync.syncState().error).toBe("queue_write_failed")
    expect(conversations.get("c1")!.messages.map((x) => x.id)).toContain(m.id)
    await act(async () => {
      mocks.handlers.get("sync:chat-changed")!({ payload: {} })
      await vi.waitFor(() => expect(calls("sync_chat_load")).toHaveLength(2))
    })
    // Neither queued, nor in localStorage (native never writes originals):
    // the hydrate replaces the cache and the message is gone.
    expect(conversations.get("c1")!.messages.map((x) => x.id)).toContain(m.id)
  })
})

// ---------------------------------------------------------------------------
describe("crash and reload", () => {
  it("RT04 crash between persisting the queue and the invoke: replayed once on the next boot", async () => {
    status("native")
    const sim = rustSim({ conversations: [], history: [] })
    const first = await load()
    await first.sync.bootSync()
    // The invoke never reaches Rust (process killed after the key was
    // written): modelled as a request that never returns.
    mocks.responders.set("sync_queue_apply", () => new Promise(() => {}))
    first.history.add(entry)
    expect(queue().items.map((i) => i.qseq)).toEqual([1])
    // Let the dead instance's request leave before the restart.
    await vi.waitFor(() => expect(applied()).toHaveLength(1))
    expect(sim.hist.size).toBe(0)

    const second = await reboot()
    status("native")
    const sim2 = rustSim({ conversations: [], history: [...sim.hist.values()] })
    await second.sync.bootSync()
    expect(applied().map((a) => [a.qseq, a.op.op])).toEqual([[1, "tr_add"]])
    expect(queue()).toEqual({ nextQseq: 2, items: [] })
    expect(sim2.hist.size).toBe(1)
    expect(second.history.list().map((e) => e.source)).toEqual(["x"])
  })

  it("RT05 reload mid-flush (item 2 applied by Rust, removal lost): resend is skipped by qseq, no gap", async () => {
    localStorage.setItem(NATIVE, "true")
    localStorage.setItem(
      QUEUE,
      JSON.stringify({ nextQseq: 4, items: [1, 2, 3].map((q) => ({ qseq: q, op: { op: "tr_add", tr: tr(`t${q}`) } })) }),
    )
    status("native")
    const sim = rustSim({ conversations: [], history: [] })
    const real = mocks.responders.get("sync_queue_apply")!
    mocks.responders.set("sync_queue_apply", (a) => {
      const out = real(a)
      // Rust committed item 2; the webview dies before it removes it.
      if (a!.qseq === 2) return new Promise(() => {})
      return out
    })
    const first = await load()
    void first.sync.bootSync()
    await vi.waitFor(() => expect(applied().map((a) => a.qseq)).toEqual([1, 2]))
    expect(queue().items.map((i) => i.qseq)).toEqual([2, 3])

    const second = await reboot()
    status("native")
    mocks.responders.set("sync_queue_apply", real)
    await second.sync.bootSync()
    expect(applied().map((a) => a.qseq)).toEqual([2, 3])
    expect(sim.flushedQseq()).toBe(3)
    expect([...sim.hist.keys()]).toEqual(["t1", "t2", "t3"])
    expect(second.history.list().map((e) => e.id).sort()).toEqual(["t1", "t2", "t3"])
  })

  it("RT06 an invoke error mid-queue stops in order; the next write retries from the failed head", async () => {
    localStorage.setItem(NATIVE, "true")
    localStorage.setItem(
      QUEUE,
      JSON.stringify({ nextQseq: 4, items: [1, 2, 3].map((q) => ({ qseq: q, op: { op: "tr_add", tr: tr(`t${q}`) } })) }),
    )
    status("native")
    const sim = rustSim({ conversations: [], history: [] })
    const real = mocks.responders.get("sync_queue_apply")!
    let failOnce = true
    mocks.responders.set("sync_queue_apply", (a) => {
      if (a!.qseq === 2 && failOnce) {
        failOnce = false
        throw { code: "io", message: "disk" }
      }
      return real(a)
    })
    const { sync, history } = await load()
    await sync.bootSync()
    expect(applied().map((a) => a.qseq)).toEqual([1, 2])
    expect(queue().items.map((i) => i.qseq)).toEqual([2, 3])
    expect(sync.syncState().error).toBe("io")
    expect(sync.isNative()).toBe(true)
    // No hydrate while items are pending: the optimistic cache stays.
    expect(calls("sync_chat_load")).toHaveLength(0)

    history.add(entry)
    await vi.waitFor(() => expect(queue().items).toEqual([]))
    expect(applied().map((a) => a.qseq)).toEqual([1, 2, 2, 3, 4])
    expect(sim.flushedQseq()).toBe(4)
    expect(sim.hist.size).toBe(4)
  })
})

// ---------------------------------------------------------------------------
describe("events and concurrency", () => {
  it("RT07 sync:chat-changed during a flush: one drain, each qseq applied once, hydrate after the queue empties", async () => {
    status("native")
    const sim = rustSim({ conversations: [], history: [] })
    const real = mocks.responders.get("sync_queue_apply")!
    const gate = deferred<void>()
    mocks.responders.set("sync_queue_apply", async (a) => {
      await gate.promise
      return real(a)
    })
    const { sync, history } = await load()
    await sync.bootSync()
    const loadsAtBoot = calls("sync_chat_load").length
    history.add(entry)
    await vi.waitFor(() => expect(applied()).toHaveLength(1))
    await act(async () => {
      mocks.handlers.get("sync:chat-changed")!({ payload: {} })
      mocks.handlers.get("sync:chat-changed")!({ payload: {} })
      history.add({ ...entry, source: "second" })
      gate.resolve()
      await vi.waitFor(() => expect(queue().items).toEqual([]))
      await vi.waitFor(() => expect(calls("sync_chat_load").length).toBeGreaterThan(loadsAtBoot))
    })
    expect(applied().map((a) => a.qseq)).toEqual([1, 2])
    expect(sim.hist.size).toBe(2)
    expect(sync.isHydrated()).toBe(true)
    expect(history.list().map((e) => e.source).sort()).toEqual(["second", "x"])
  })

  it("G8 blocker: RT07b out-of-order sync_chat_load replies: a stale load resolving last overwrites the cache", async () => {
    status("native")
    const stale = deferred<unknown>()
    let n = 0
    mocks.responders.set("sync_chat_load", () => {
      n++
      if (n === 1) return { conversations: [], history: [] }
      if (n === 2) return stale.promise
      return { conversations: [], history: [tr("fresh")] }
    })
    const { sync, history } = await load()
    await sync.bootSync()
    await act(async () => {
      mocks.handlers.get("sync:chat-changed")!({ payload: {} }) // load #2 (stale, slow)
      mocks.handlers.get("sync:chat-changed")!({ payload: {} }) // load #3 (fresh, fast)
      await vi.waitFor(() => expect(history.list().map((e) => e.id)).toEqual(["fresh"]))
      stale.resolve({ conversations: [], history: [] })
      await stale.promise
      await new Promise((r) => setTimeout(r, 0))
    })
    // No later event arrives: the cache must keep the newest projection.
    expect(history.list().map((e) => e.id)).toEqual(["fresh"])
  })

  it("RT08 concurrent writes from useChat-style and useHistory paths: unique monotonic qseq, applied once in order, both hooks notified", async () => {
    status("native")
    const sim = rustSim({ conversations: [], history: [] })
    const real = mocks.responders.get("sync_queue_apply")!
    const gate = deferred<void>()
    mocks.responders.set("sync_queue_apply", async (a) => {
      await gate.promise
      return real(a)
    })
    const { sync, conversations } = await load()
    const { useHistory } = await import("@/hooks/useHistory")
    const hook = renderHook(() => useHistory())
    await act(async () => {
      await sync.bootSync()
    })
    const c = conversations.create()
    act(() => {
      hook.result.current.add(entry)
    })
    conversations.appendMessage(c.id, { role: "user", content: "hi" })
    act(() => {
      hook.result.current.add({ ...entry, source: "b" })
    })
    conversations.appendMessage(c.id, { role: "assistant", content: "yo" })
    const qseqs = queue().items.map((i) => i.qseq)
    expect(qseqs).toEqual([...qseqs].sort((a, b) => a - b))
    expect(new Set(qseqs).size).toBe(qseqs.length)
    await act(async () => {
      gate.resolve()
      await vi.waitFor(() => expect(queue().items).toEqual([]))
    })
    const sent = applied().map((a) => a.qseq)
    expect(sent).toEqual(qseqs)
    expect(sim.convs.get(c.id)!.messages.map((m) => m.content)).toEqual(["hi", "yo"])
    expect(sim.hist.size).toBe(2)
    await waitFor(() => expect(hook.result.current.entries.map((e) => e.source)).toEqual(["b", "x"]))
  })
})

// ---------------------------------------------------------------------------
describe("setMessages under native", () => {
  async function nativeWith(messages: Msg[]) {
    status("native")
    const sim = rustSim({ conversations: [conv("c1", messages)], history: [] })
    const mods = await load()
    await mods.sync.bootSync()
    invoke.mockClear()
    return { ...mods, sim }
  }

  it("RT09 regenerate keeps the old answer as a sibling (set_active, then msg_add under the user turn)", async () => {
    const { conversations, sync } = await nativeWith([msg("u1"), msg("a1", "assistant")])
    conversations.setMessages("c1", [conversations.get("c1")!.messages[0]])
    const a2 = conversations.appendMessage("c1", { role: "assistant", content: "a2" })
    await vi.waitFor(() => expect(queue().items).toEqual([]))
    expect(applied().map((a) => a.op)).toEqual([
      { op: "set_active", conv: "c1", leaf: "u1" },
      { op: "msg_add", conv: "c1", msg: expect.objectContaining({ id: a2.id }), parent: "u1" },
      { op: "set_active", conv: "c1", leaf: a2.id },
    ])
    expect(applied().some((a) => a.op.op.includes("delete"))).toBe(false)
    expect(sync.cachedConversations()[0].messages.map((m) => m.id)).toEqual(["u1", a2.id])
  })

  it("RT10 truncate then append in one call: msg_add under the kept prefix, then set_active", async () => {
    const { conversations } = await nativeWith([msg("u1"), msg("a1", "assistant"), msg("u2"), msg("a2", "assistant")])
    const kept = conversations.get("c1")!.messages[0]
    conversations.setMessages("c1", [kept, { id: "x", role: "user", content: "x", ts: 2 }])
    await vi.waitFor(() => expect(queue().items).toEqual([]))
    expect(applied().map((a) => a.op)).toEqual([
      { op: "msg_add", conv: "c1", msg: { id: "x", role: "user", content: "x", ts: 2, images: [] }, parent: "u1" },
      { op: "set_active", conv: "c1", leaf: "x" },
    ])
  })

  it("RT11 an empty list emits nothing (documented deviation); the native path returns on the next hydrate", async () => {
    const { conversations } = await nativeWith([msg("u1"), msg("a1", "assistant")])
    conversations.setMessages("c1", [])
    expect(conversations.get("c1")!.messages).toEqual([])
    expect(localStorage.getItem(QUEUE)).toBeNull()
    await act(async () => {
      mocks.handlers.get("sync:chat-changed")!({ payload: {} })
      await vi.waitFor(() => expect(calls("sync_chat_load")).toHaveLength(1))
    })
    expect(calls("sync_queue_apply")).toHaveLength(0)
    expect(conversations.get("c1")!.messages.map((m) => m.id)).toEqual(["u1", "a1"])
  })

  it("RT12 a reordered list re-adds nothing (no duplicate msg_add); only the leaf moves", async () => {
    const { conversations } = await nativeWith([msg("u1"), msg("a1", "assistant")])
    const [u1, a1] = conversations.get("c1")!.messages
    conversations.setMessages("c1", [a1, u1])
    await vi.waitFor(() => expect(queue().items).toEqual([]))
    expect(applied().map((a) => a.op)).toEqual([{ op: "set_active", conv: "c1", leaf: "u1" }])
  })
})

// ---------------------------------------------------------------------------
describe("translation history", () => {
  it("RT13 history.clear with pinned entries: tr_clear observes every unpinned id (beyond the display cap), pinned stay (native)", async () => {
    status("native")
    const unpinned = Array.from({ length: 60 }, (_, i) => tr(`u${i}`, false, i + 10))
    rustSim({ conversations: [], history: [tr("p1", true, 1), tr("p2", true, 2), ...unpinned] })
    const { sync, history } = await load()
    await sync.bootSync()
    expect(history.list()).toHaveLength(52) // display cap only
    history.clear()
    await vi.waitFor(() => expect(queue().items).toEqual([]))
    const op = applied()[0].op
    expect(op.op).toBe("tr_clear")
    expect([...(op.observedIds as string[])].sort()).toEqual(unpinned.map((t) => t.id).sort())
    expect(history.list().map((e) => e.id).sort()).toEqual(["p1", "p2"])
  })

  it("RT14 history.clear with pinned entries under write-hold: original keeps pinned, op lists only unpinned", async () => {
    localStorage.setItem(P + "history", JSON.stringify([tr("p1", true), tr("u1"), tr("u2")]))
    status("verified")
    mocks.responders.set("sync_migrate_finalize", () => ({ result: "deferred", attempts: 3, mismatches: 0 }))
    const { sync, history } = await load()
    await sync.bootSync()
    history.clear()
    expect(JSON.parse(localStorage.getItem(P + "history")!).map((e: { id: string }) => e.id)).toEqual(["p1"])
    expect(queue().items.map((i) => i.op)).toEqual([{ op: "tr_clear", observedIds: ["u1", "u2"] }])
  })

  it("RT15 tr_add dedupe: identical consecutive adds queue one tr_add (hold and native); A,B,A queues three", async () => {
    status("verified")
    mocks.responders.set("sync_migrate_finalize", () => ({ result: "deferred", attempts: 3, mismatches: 0 }))
    const hold = await load()
    await hold.sync.bootSync()
    hold.history.add(entry)
    hold.history.add(entry)
    hold.history.add({ ...entry, source: "z" })
    hold.history.add(entry)
    expect(queue().items.map((i) => (i.op.tr as { source: string }).source)).toEqual(["x", "z", "x"])

    localStorage.clear()
    const nat = await reboot()
    status("native")
    rustSim({ conversations: [], history: [] })
    await nat.sync.bootSync()
    nat.history.add(entry)
    nat.history.add(entry)
    await vi.waitFor(() => expect(queue().items).toEqual([]))
    expect(applied().map((a) => a.op.op)).toEqual(["tr_add"])
    expect(nat.history.list()).toHaveLength(1)
  })
})

// ---------------------------------------------------------------------------
describe("M6 finalize", () => {
  it("RT16 hash mismatch three times: verify_failed, verify_failed, deferred -> stays hold, queue kept, nothing flushed", async () => {
    status("verified")
    let attempt = 0
    mocks.responders.set("sync_migrate_finalize", () => {
      attempt++
      return attempt < 3
        ? { result: "verify_failed", attempts: attempt, mismatches: 1 }
        : { result: "deferred", attempts: attempt, mismatches: 1 }
    })
    localStorage.setItem(QUEUE, JSON.stringify({ nextQseq: 2, items: [{ qseq: 1, op: { op: "tr_delete", tr: "t" } }] }))
    const { sync, conversations } = await load()
    await sync.bootSync()
    expect(calls("sync_migrate_finalize")).toHaveLength(3)
    expect(sync.syncState()).toMatchObject({ mode: "hold", finalize: "deferred" })
    expect(sync.isNative()).toBe(false)
    expect(calls("sync_queue_apply")).toHaveLength(0)
    expect(calls("sync_chat_load")).toHaveLength(0)
    conversations.create()
    expect(queue().items.map((i) => i.qseq)).toEqual([1, 2])
    expect(localStorage.getItem(NATIVE)).toBeNull()
  })

  it("RT17 NeedSnapshot with a write landing in between: resend carries the same hash/tail, the write flushes after native", async () => {
    status("verified")
    const mods = await load()
    const sim = rustSim({ conversations: [], history: [] })
    const fin: Record<string, unknown>[] = []
    mocks.responders.set("sync_migrate_finalize", (a) => {
      fin.push(a!)
      if (fin.length === 1) {
        mods.history.add(entry) // typed while Rust compares
        return { result: "need_snapshot" }
      }
      return { result: "native", ack_qseq: a!.queueTail }
    })
    await mods.sync.bootSync()
    expect(fin).toHaveLength(2)
    expect(fin[1].currentHash).toBe(fin[0].webviewHash)
    expect(await sha(fin[1].current as string)).toBe(fin[0].webviewHash)
    expect(fin[1].current as string).not.toContain('"source":"x"')
    expect(fin[1].queueTail).toBe(0)
    expect(applied().map((a) => [a.qseq, a.op.op])).toEqual([[1, "tr_add"]])
    expect(sim.hist.size).toBe(1)
    expect(mods.sync.isNative()).toBe(true)
  })
})

// ---------------------------------------------------------------------------
describe("flag", () => {
  it("G8 blocker: RT18 flag on -> off -> on across boots: writes made while off are dropped by the native hydrate", async () => {
    localStorage.setItem(P + "conversations", JSON.stringify([{ ...conv("c1", [msg("m1")]), createdAt: 1 }]))
    status("native")
    const sim = rustSim({ conversations: [conv("c1", [msg("m1")])], history: [] })
    const b1 = await load()
    await b1.sync.bootSync()
    b1.conversations.appendMessage("c1", { role: "user", content: "while on" })
    await vi.waitFor(() => expect(queue().items).toEqual([]))

    const b2 = await reboot()
    // E1: the device reached native, so the flag only stops the network;
    // status still reports `native` and the local store stays SQLite.
    mocks.responders.set("sync_get_status", () => ({ enabled: false, migration: "native" }))
    await b2.sync.bootSync()
    expect(invoke.mock.calls.map((c) => c[0])).toEqual(["sync_get_status", "sync_chat_load"])
    const off = b2.conversations.appendMessage("c1", { role: "user", content: "while off" })
    await vi.waitFor(() => expect(queue().items).toEqual([]))

    const b3 = await reboot()
    status("native")
    expect(sim.convs.get("c1")!.messages).toHaveLength(3)
    await b3.sync.bootSync()
    expect(b3.conversations.get("c1")!.messages.map((m) => m.id)).toContain(off.id)
  })

  it("G8 blocker: RT19 flag off with a stale native marker: a write before the status reply is lost and an extra invoke is made (AC 15)", async () => {
    localStorage.setItem(NATIVE, "true")
    const st = deferred<unknown>()
    mocks.responders.set("sync_get_status", () => st.promise)
    const { sync, history } = await load()
    const boot = sync.bootSync()
    history.add(entry)
    st.resolve({ enabled: false })
    await boot
    await new Promise((r) => setTimeout(r, 0))
    expect(JSON.parse(localStorage.getItem(P + "history") ?? "[]").map((e: { source: string }) => e.source)).toEqual(["x"])
    expect(invoke.mock.calls.map((c) => c[0])).toEqual(["sync_get_status"])
  })

  it("RT20 flag off from a clean install: exactly one status invoke, no queue key, no listener", async () => {
    mocks.responders.set("sync_get_status", () => ({ enabled: false }))
    const { sync, conversations, history } = await load()
    await sync.bootSync()
    const c = conversations.create()
    conversations.appendMessage(c.id, { role: "user", content: "a" })
    history.add(entry)
    history.clear()
    expect(invoke.mock.calls).toEqual([["sync_get_status"]])
    expect(listen).not.toHaveBeenCalled()
    expect(localStorage.getItem(QUEUE)).toBeNull()
    expect(localStorage.getItem(NATIVE)).toBeNull()
  })

  it("G8 blocker: RT21 a write before the first hydrate on a native boot branches off a stale leaf", async () => {
    localStorage.setItem(P + "conversations", JSON.stringify([{ ...conv("c1", [msg("m1")]), createdAt: 1 }]))
    localStorage.setItem(NATIVE, "true")
    status("native")
    rustSim({ conversations: [conv("c1", [msg("m1"), msg("m2", "assistant"), msg("m3")])], history: [] })
    const loadGate = deferred<void>()
    const realLoad = mocks.responders.get("sync_chat_load")!
    mocks.responders.set("sync_chat_load", async (a) => {
      await loadGate.promise
      return realLoad(a)
    })
    const { sync, conversations } = await load()
    const boot = sync.bootSync()
    // The UI shows the seeded originals; the user sends before the load lands.
    const m = conversations.appendMessage("c1", { role: "assistant", content: "reply" })
    loadGate.resolve()
    await boot
    const add = applied().find((a) => a.op.op === "msg_add" && (a.op.msg as Msg).id === m.id)
    expect(add?.op.parent).toBe("m3")
  })
})

// ---------------------------------------------------------------------------
describe("queue file integrity", () => {
  it("G8 blocker: RT22 a corrupt (truncated) queue JSON is overwritten silently on the next write", async () => {
    const raw = '{"nextQseq":4,"items":[{"qseq":3,"op":{"op":"tr_del'
    localStorage.setItem(QUEUE, raw)
    status("verified")
    mocks.responders.set("sync_migrate_finalize", () => ({ result: "deferred", attempts: 3, mismatches: 0 }))
    const { sync, history } = await load()
    await sync.bootSync()
    history.add(entry)
    const preserved = Object.keys(localStorage).some((k) => localStorage.getItem(k) === raw)
    expect(preserved || sync.syncState().error !== null).toBe(true)
  })

  it("G8 blocker: RT23 a malformed queue item is dropped silently on the next rewrite", async () => {
    localStorage.setItem(
      QUEUE,
      JSON.stringify({ nextQseq: 3, items: [{ qseq: 1, op: { op: "tr_delete", tr: "a" } }, { qseq: "2", op: { op: "tr_delete", tr: "b" } }] }),
    )
    status("verified")
    mocks.responders.set("sync_migrate_finalize", () => ({ result: "deferred", attempts: 3, mismatches: 0 }))
    const { sync, history } = await load()
    await sync.bootSync()
    history.add(entry)
    const all = Object.keys(localStorage).map((k) => localStorage.getItem(k) ?? "").join("\n")
    expect(all.includes('"tr":"b"') || sync.syncState().error !== null).toBe(true)
  })

  it("G8 blocker: RT24 qseq restarts at 1 after the queue key is lost; Rust skips it as already flushed", async () => {
    // Rust flushed up to 7 (status exposes it); the queue key is gone
    // (corruption, manual clear, or a profile restore).
    status("native", { queue_flushed_qseq: 7, ack_qseq: 3 })
    rustSim({ conversations: [], history: [] })
    const { sync, history } = await load()
    await sync.bootSync()
    mocks.responders.set("sync_queue_apply", () => new Promise(() => {}))
    history.add(entry)
    // `migration::apply_queued` returns `changed:false` for qseq <= 7 and
    // advances nothing (migration.rs:1406, test at migration.rs:3037-3040).
    expect(queue().items[0].qseq).toBeGreaterThan(7)
  })

  it("RT25 qseq stays monotonic: a stale nextQseq below the max item, a non-numeric nextQseq, and a large value", async () => {
    status("verified")
    mocks.responders.set("sync_migrate_finalize", () => ({ result: "deferred", attempts: 3, mismatches: 0 }))
    localStorage.setItem(QUEUE, JSON.stringify({ nextQseq: 2, items: [{ qseq: 9, op: { op: "tr_delete", tr: "a" } }] }))
    let m = await load()
    await m.sync.bootSync()
    m.history.add(entry)
    expect(queue().items.map((i) => i.qseq)).toEqual([9, 10])

    localStorage.setItem(QUEUE, JSON.stringify({ nextQseq: "x", items: [{ qseq: 5, op: { op: "tr_delete", tr: "a" } }] }))
    m = await reboot()
    status("verified")
    await m.sync.bootSync()
    m.history.add({ ...entry, source: "q" })
    expect(queue().items.map((i) => i.qseq)).toEqual([5, 6])

    const big = 1_000_000_000_000
    localStorage.setItem(QUEUE, JSON.stringify({ nextQseq: big, items: [] }))
    m = await reboot()
    status("verified")
    await m.sync.bootSync()
    m.history.add({ ...entry, source: "r" })
    m.history.add({ ...entry, source: "s" })
    expect(queue()).toMatchObject({ nextQseq: big + 2, items: [{ qseq: big }, { qseq: big + 1 }] })
    // 2^53 is not reachable by user writes; the arithmetic is exact below it.
    expect(Number.isSafeInteger(queue().nextQseq)).toBe(true)
  })

  it("RT26 the queue stays bounded after a drain: nextQseq persists with an empty item list (no reuse)", async () => {
    status("native")
    rustSim({ conversations: [], history: [] })
    const { sync, history } = await load()
    await sync.bootSync()
    history.add(entry)
    await vi.waitFor(() => expect(queue().items).toEqual([]))
    expect(queue().nextQseq).toBe(2)
    const again = await reboot()
    status("native")
    rustSim({ conversations: [], history: [] })
    await again.sync.bootSync()
    again.history.add({ ...entry, source: "n" })
    await vi.waitFor(() => expect(queue().items).toEqual([]))
    expect(applied().map((a) => a.qseq)).toEqual([2])
  })
})

// ---------------------------------------------------------------------------
describe("scale", () => {
  it("RT27 sync_chat_load cache with 10k messages and 5k translations: sync reads stay fast, trims are display-only", async () => {
    status("native")
    const messages = Array.from({ length: 10_000 }, (_, i) => msg(`m${i}`, i % 2 ? "assistant" : "user", i + 1))
    const hist = Array.from({ length: 5_000 }, (_, i) => tr(`t${i}`, i < 3, i + 1))
    rustSim({ conversations: [conv("c1", messages)], history: hist })
    const { sync, conversations, history } = await load()
    await sync.bootSync()
    const t0 = performance.now()
    for (let i = 0; i < 20; i++) {
      conversations.list()
      conversations.get("c1")
      history.list()
    }
    const readMs = (performance.now() - t0) / 20
    expect(readMs).toBeLessThan(100)
    expect(conversations.get("c1")!.messages).toHaveLength(200)
    expect(history.list()).toHaveLength(53)
    expect(sync.cachedConversations()[0].messages).toHaveLength(10_000)
    const t1 = performance.now()
    const m = conversations.appendMessage("c1", { role: "user", content: "next" })
    expect(performance.now() - t1).toBeLessThan(200)
    await vi.waitFor(() => expect(queue().items).toEqual([]))
    expect(applied()[0].op).toMatchObject({ op: "msg_add", parent: "m9999" })
    expect(applied().some((a) => a.op.op.includes("delete"))).toBe(false)
    expect(sync.cachedConversations()[0].messages).toHaveLength(10_001)
    expect(sync.cachedConversations()[0].messages[10_000].id).toBe(m.id)
  })
})

// ===========================================================================
// Delta pass (G8 pass-1 decisions E1-E5): attacks on the fixes. Uses a
// tree-aware Rust stand-in that answers like `migration::apply_queue_item`
// after E3: `applied{changed}`, `duplicate` (same qseq, same op hash) or
// `stale_qseq{watermark}` (at or below the watermark, other/unknown hash),
// with a bounded op-hash window.
// ===========================================================================

function canon(v: unknown): string {
  if (Array.isArray(v)) return "[" + v.map(canon).join(",") + "]"
  if (v && typeof v === "object") {
    const o = v as Record<string, unknown>
    return "{" + Object.keys(o).sort().map((k) => JSON.stringify(k) + ":" + canon(o[k])).join(",") + "}"
  }
  return JSON.stringify(v)
}

type SimNode = { msg: Msg; parent: string | null }
type SimConv = { id: string; title: string; nodes: Map<string, SimNode>; leaf: string | null }

function rustE3(
  seed: { conversations: ReturnType<typeof conv>[]; history: ReturnType<typeof tr>[] },
  opts: { flushed?: number; window?: number } = {},
) {
  const convs = new Map<string, SimConv>()
  for (const c of seed.conversations) {
    const nodes = new Map<string, SimNode>()
    c.messages.forEach((m, i) => nodes.set(m.id, { msg: m, parent: i ? c.messages[i - 1].id : null }))
    convs.set(c.id, { id: c.id, title: c.title, nodes, leaf: c.messages[c.messages.length - 1]?.id ?? null })
  }
  const hist = new Map(seed.history.map((t) => [t.id, { ...t }]))
  const activePath = (c: SimConv): Msg[] => {
    const out: Msg[] = []
    for (let at = c.leaf; at; at = c.nodes.get(at)!.parent) out.unshift(c.nodes.get(at)!.msg)
    return out
  }
  const fold = (op: Op): boolean => {
    const c = typeof op.conv === "string" ? convs.get(op.conv) : undefined
    switch (op.op) {
      case "conv_create":
        if (c) return false
        convs.set(op.conv as string, { id: op.conv as string, title: op.title as string, nodes: new Map(), leaf: null })
        return true
      case "msg_add": {
        const m = op.msg as Msg
        if (!c || c.nodes.has(m.id)) return false
        c.nodes.set(m.id, { msg: m, parent: (op.parent as string | null) ?? null })
        c.leaf = m.id
        return true
      }
      case "set_active":
        if (!c || !c.nodes.has(op.leaf as string) || c.leaf === op.leaf) return false
        c.leaf = op.leaf as string
        return true
      case "rename":
        if (!c || c.title === op.title) return false
        c.title = op.title as string
        return true
      case "conv_delete":
        return convs.delete(op.conv as string)
      case "tr_add": {
        const t = op.tr as ReturnType<typeof tr>
        if (hist.has(t.id)) return false
        hist.set(t.id, t)
        return true
      }
      case "tr_delete":
        return hist.delete(op.tr as string)
      case "tr_pin": {
        const t = hist.get(op.tr as string)
        if (!t || t.pinned === op.pinned) return false
        t.pinned = op.pinned as boolean
        return true
      }
      case "tr_clear": {
        let any = false
        for (const id of op.observedIds as string[]) {
          if (hist.get(id) && !hist.get(id)!.pinned) any = hist.delete(id) || any
        }
        return any
      }
    }
    return false
  }
  const sim = {
    convs,
    hist,
    flushed: opts.flushed ?? 0,
    hashes: new Map<number, string>(),
    window: opts.window ?? 1024,
    results: [] as string[],
    changed: [] as boolean[],
    path: (id: string) => activePath(convs.get(id)!).map((m) => m.id),
    /** A change that arrives from another device (a sync cycle). */
    remote(id: string, m: Msg) {
      const c = convs.get(id)!
      c.nodes.set(m.id, { msg: m, parent: c.leaf })
      c.leaf = m.id
    },
  }
  mocks.responders.set("sync_queue_apply", (a) => {
    const qseq = a!.qseq as number
    const op = a!.op as Op
    const h = canon(op)
    if (qseq <= sim.flushed) {
      const out = sim.hashes.get(qseq) === h ? { result: "duplicate" } : { result: "stale_qseq", watermark: sim.flushed }
      sim.results.push(out.result)
      return out
    }
    sim.flushed = qseq
    sim.hashes.set(qseq, h)
    const keys = [...sim.hashes.keys()].sort((x, y) => x - y)
    for (const k of keys.slice(0, Math.max(0, keys.length - sim.window))) sim.hashes.delete(k)
    const changed = fold(op)
    sim.results.push("applied")
    sim.changed.push(changed)
    return { result: "applied", changed }
  })
  mocks.responders.set("sync_chat_load", () => ({
    conversations: [...convs.values()].map((c) => ({ id: c.id, title: c.title, messages: activePath(c), updatedAt: 1 })),
    history: [...hist.values()].map((t) => ({ ...t })),
  }))
  return sim
}

function chatChanged() {
  mocks.handlers.get("sync:chat-changed")!({ payload: {} })
}

async function ticks(n = 10) {
  for (let i = 0; i < n; i++) await new Promise((r) => setTimeout(r, 0))
}

const ids = (list: { id: string }[]) => list.map((m) => m.id)

// ---------------------------------------------------------------------------
describe("delta E2 durable or refused", () => {
  it("D01 a queued item whose reply is lost after Rust applied it is resent with the same qseq: duplicate, applied once", async () => {
    status("native")
    const sim = rustE3({ conversations: [conv("c1", [msg("m1")])], history: [] })
    const { sync, conversations } = await load()
    await sync.bootSync()
    const real = mocks.responders.get("sync_queue_apply")!
    let lose = true
    mocks.responders.set("sync_queue_apply", (a) => {
      const out = real(a)
      if (lose) {
        lose = false
        throw { code: "ipc", message: "reply lost" }
      }
      return out
    })
    const m = conversations.appendMessage("c1", { role: "assistant", content: "a" })
    await vi.waitFor(() => expect(sync.syncState().error).toBe("ipc"))
    // Durable (queue key), so it is kept, shown and not counted as refused.
    expect(queue().items.map((i) => i.qseq)).toEqual([1, 2])
    expect(ids(conversations.get("c1")!.messages)).toEqual(["m1", m.id])
    expect(sync.syncState().writeFailures).toBe(0)

    const n = conversations.appendMessage("c1", { role: "user", content: "b" })
    await vi.waitFor(() => expect(queue().items).toEqual([]))
    expect(applied().map((a) => a.qseq)).toEqual([1, 1, 2, 3, 4])
    expect(sim.results).toEqual(["applied", "duplicate", "applied", "applied", "applied"])
    expect(sim.path("c1")).toEqual(["m1", m.id, n.id])
    expect([...sim.convs.get("c1")!.nodes.keys()]).toEqual(["m1", m.id, n.id])
  })

  it("D02 a quota-failed (memory) item whose reply is lost is refused and never resent; qseqs stay unique, no stale loop", async () => {
    status("native")
    const sim = rustE3({ conversations: [conv("c1", [msg("m1")])], history: [] })
    const { sync, conversations } = await load()
    await sync.bootSync()
    const real = mocks.responders.get("sync_queue_apply")!
    let lose = true
    mocks.responders.set("sync_queue_apply", (a) => {
      const out = real(a)
      if (lose) {
        lose = false
        throw { code: "ipc", message: "reply lost" }
      }
      return out
    })
    const spy = quotaOnQueue(() => true)
    const m = conversations.appendMessage("c1", { role: "assistant", content: "a" })
    spy.mockRestore()
    await vi.waitFor(() => expect(sync.syncState().writeFailures).toBe(1))
    // E2.2: refused, the cache change is undone and the error is surfaced.
    expect(ids(conversations.get("c1")!.messages)).toEqual(["m1"])
    expect(sync.syncState().error).toBe("ipc")
    // The ambiguous outcome: Rust did apply the msg_add before the reply was lost.
    expect(sim.path("c1")).toEqual(["m1", m.id])

    const n = conversations.appendMessage("c1", { role: "user", content: "b" })
    await vi.waitFor(() => expect(queue().items).toEqual([]))
    const qs = applied().map((a) => a.qseq)
    expect(qs).toEqual([1, 3, 4])
    expect(sim.results).toEqual(["applied", "applied", "applied"])
    // The next write is parented to what the cache shows; the refused one is a
    // hidden sibling. After the event the cache equals the native active path.
    expect(applied()[1].op).toMatchObject({ op: "msg_add", parent: "m1" })
    await act(async () => {
      chatChanged()
      await vi.waitFor(() => expect(ids(conversations.get("c1")!.messages)).toEqual(sim.path("c1")))
    })
    expect(sim.path("c1")).toEqual(["m1", n.id])
  })

  it("D03 observation: a lost reply whose sync:chat-changed arrived while the call was in flight leaves the refused-but-stored write hidden until the next event", async () => {
    status("native")
    const sim = rustE3({ conversations: [conv("c1", [msg("m1")])], history: [] })
    const { sync, conversations } = await load()
    await sync.bootSync()
    const loadsAtBoot = calls("sync_chat_load").length
    const real = mocks.responders.get("sync_queue_apply")!
    let lose = true
    mocks.responders.set("sync_queue_apply", (a) => {
      const out = real(a)
      if (lose) {
        lose = false
        chatChanged() // Rust emits before the reply is lost
        throw { code: "ipc", message: "reply lost" }
      }
      return out
    })
    const spy = quotaOnQueue(() => true)
    const m = conversations.appendMessage("c1", { role: "assistant", content: "a" })
    spy.mockRestore()
    await act(async () => {
      await vi.waitFor(() => expect(sync.syncState().writeFailures).toBe(1))
      await ticks()
    })
    // Conformant with E2 (refused, error surfaced, no resend): the event that
    // would re-read SQLite rode on the failed drain and is not replayed.
    expect(sim.path("c1")).toEqual(["m1", m.id])
    expect(ids(conversations.get("c1")!.messages)).toEqual(["m1"])
    expect(calls("sync_chat_load").length).toBe(loadsAtBoot)
    expect(applied().map((a) => a.qseq)).toEqual([1])
  })

  it("D04 quota recovers mid-session under native while the memory items are in flight: one qseq order, all applied once", async () => {
    status("native")
    const sim = rustE3({ conversations: [conv("c1", [msg("m1")])], history: [] })
    const { sync, conversations } = await load()
    await sync.bootSync()
    const real = mocks.responders.get("sync_queue_apply")!
    const gate = deferred<void>()
    mocks.responders.set("sync_queue_apply", async (a) => {
      await gate.promise
      return real(a)
    })
    let quota = true
    quotaOnQueue(() => quota)
    const a = conversations.appendMessage("c1", { role: "assistant", content: "a" })
    expect(sync.syncState().error).toBe("queue_write_failed")
    expect(localStorage.getItem(QUEUE)).toBeNull()
    quota = false
    const b = conversations.appendMessage("c1", { role: "user", content: "b" })
    expect(queue().items.map((i) => i.qseq)).toEqual([3, 4])
    gate.resolve()
    await vi.waitFor(() => expect(sim.results).toHaveLength(4))
    await vi.waitFor(() => expect(queue().items).toEqual([]))
    expect(applied().map((x) => x.qseq)).toEqual([1, 2, 3, 4])
    expect(sim.results).toEqual(["applied", "applied", "applied", "applied"])
    expect(sim.path("c1")).toEqual(["m1", a.id, b.id])
    expect(ids(conversations.get("c1")!.messages)).toEqual(["m1", a.id, b.id])
    expect(sync.syncState().writeFailures).toBe(0)
    expect(queue().nextQseq).toBe(5)
  })

  it("D05 quota recovers mid-hold during finalize: the memory item and the queued one both reach Rust after the cutover, in order", async () => {
    status("verified")
    const sim = rustE3({ conversations: [], history: [] })
    const mods = await load()
    let quota = false
    quotaOnQueue(() => quota)
    mocks.responders.set("sync_migrate_finalize", (args) => {
      quota = true
      mods.history.add({ ...entry, source: "a" })
      quota = false
      mods.history.add({ ...entry, source: "b" })
      return { result: "native", ack_qseq: args!.queueTail }
    })
    await mods.sync.bootSync()
    await vi.waitFor(() => expect(sim.hist.size).toBe(2))
    expect(applied().map((x) => [x.qseq, (x.op.tr as { source: string }).source])).toEqual([
      [1, "a"],
      [2, "b"],
    ])
    expect(queue().items).toEqual([])
    expect(mods.sync.isNative()).toBe(true)
    expect(mods.sync.syncState().writeFailures).toBe(0)
    expect(mods.history.list().map((e) => e.source).sort()).toEqual(["a", "b"])
  })
})

// ---------------------------------------------------------------------------
describe("delta E3 qseq integrity", () => {
  it("D06 stale_qseq while later items were queued during the flight: the head and every later item renumber in order, each applied once", async () => {
    status("native")
    const sim = rustE3({ conversations: [conv("c1", [msg("m1")])], history: [] })
    const { sync, conversations, history } = await load()
    await sync.bootSync()
    const real = mocks.responders.get("sync_queue_apply")!
    const gate = deferred<void>()
    let first = true
    mocks.responders.set("sync_queue_apply", async (a) => {
      if (first) {
        first = false
        await gate.promise
      }
      return real(a)
    })
    history.add(entry)
    await vi.waitFor(() => expect(applied()).toHaveLength(1))
    history.add({ ...entry, source: "y" })
    const z = conversations.appendMessage("c1", { role: "assistant", content: "z" })
    expect(queue().items.map((i) => i.qseq)).toEqual([1, 2, 3, 4])
    // Rust's watermark moved past the in-flight qseq (restored store).
    sim.flushed = 20
    gate.resolve()
    await vi.waitFor(() => expect(queue().items).toEqual([]))
    expect(applied().map((a) => a.qseq)).toEqual([1, 21, 22, 23, 24])
    expect(sim.results).toEqual(["stale_qseq", "applied", "applied", "applied", "applied"])
    expect(applied().slice(1).map((a) => a.op.op)).toEqual(["tr_add", "tr_add", "msg_add", "set_active"])
    expect([...sim.hist.values()].map((t) => t.source)).toEqual(["x", "y"])
    expect(sim.path("c1")).toEqual(["m1", z.id])
    expect(queue().nextQseq).toBe(25)
  })

  it("D07 a stale_qseq loop is bounded per drain, keeps the durable item, and refuses a memory item behind it", async () => {
    status("native")
    rustE3({ conversations: [], history: [] })
    const { sync, history } = await load()
    await sync.bootSync()
    mocks.responders.set("sync_queue_apply", (a) => ({ result: "stale_qseq", watermark: (a!.qseq as number) + 10 }))
    history.add(entry)
    await vi.waitFor(() => expect(sync.syncState().error).toBe("stale_qseq"))
    await ticks()
    expect(applied().map((a) => a.qseq)).toEqual([1, 12, 23, 34])
    expect(queue().items.map((i) => i.qseq)).toEqual([34])
    expect(queue().nextQseq).toBe(35)
    expect(history.list().map((e) => e.source)).toEqual(["x"])

    const spy = quotaOnQueue(() => true)
    history.add({ ...entry, source: "mem" })
    spy.mockRestore()
    await vi.waitFor(() => expect(sync.syncState().writeFailures).toBe(1))
    await ticks()
    expect(applied()).toHaveLength(8)
    expect(history.list().map((e) => e.source)).toEqual(["x"])
    expect(queue().items).toHaveLength(1)
  })

  it("D08 an item replayed after its hash left the 1024 window is stale, renumbered above the watermark and applied once (idempotent by id)", async () => {
    localStorage.setItem(NATIVE, "true")
    localStorage.setItem(QUEUE, JSON.stringify({ nextQseq: 6, items: [{ qseq: 5, op: { op: "tr_add", tr: tr("old") } }] }))
    status("native", { queue_flushed_qseq: 2000 })
    const sim = rustE3({ conversations: [], history: [tr("old")] }, { flushed: 2000 })
    for (let q = 977; q <= 2000; q++) sim.hashes.set(q, "h" + q)
    const { sync, history } = await load()
    await sync.bootSync()
    expect(applied().map((a) => a.qseq)).toEqual([5, 2001])
    expect(sim.results).toEqual(["stale_qseq", "applied"])
    expect(sim.changed).toEqual([false])
    expect(queue()).toEqual({ nextQseq: 2002, items: [] })
    expect(history.list().map((e) => e.id)).toEqual(["old"])
  })

  it("D08b observation: an evicted register op (rename) replayed this way re-applies over a newer value", async () => {
    localStorage.setItem(NATIVE, "true")
    localStorage.setItem(QUEUE, JSON.stringify({ nextQseq: 6, items: [{ qseq: 5, op: { op: "rename", conv: "c1", title: "old" } }] }))
    status("native", { queue_flushed_qseq: 2000 })
    const sim = rustE3({ conversations: [{ ...conv("c1", [msg("m1")]), title: "newer" }], history: [] }, { flushed: 2000 })
    const { sync } = await load()
    await sync.bootSync()
    // E3 as decided: unknown hash -> stale -> renumber -> apply. Reachable only
    // when a queue key outlives 1024 later applies (profile restore).
    expect(applied().map((a) => a.qseq)).toEqual([5, 2001])
    expect(sim.convs.get("c1")!.title).toBe("old")
  })

  it("D09 the floor from status applies to a write made before the status reply (native marker: held, then qseq above the watermark)", async () => {
    localStorage.setItem(NATIVE, "true")
    const st = deferred<unknown>()
    mocks.responders.set("sync_get_status", () => st.promise)
    const sim = rustE3({ conversations: [], history: [] }, { flushed: 7 })
    const { sync, history } = await load()
    const boot = sync.bootSync()
    history.add(entry)
    expect(sync.isReady()).toBe(false)
    expect(localStorage.getItem(QUEUE)).toBeNull()
    expect(localStorage.getItem(P + "history")).toBeNull()
    st.resolve({ enabled: true, vault: { id: "v" }, migration: "native", queue_flushed_qseq: 7, ack_qseq: 3 })
    await boot
    await vi.waitFor(() => expect(sim.results).toHaveLength(1))
    expect(applied().map((a) => a.qseq)).toEqual([8])
    expect(sim.results).toEqual(["applied"])
    expect(history.list().map((e) => e.source)).toEqual(["x"])
  })

  it("D09b a pre-status write on a hold boot (queue left, no marker) is queued below the floor and renumbered after status", async () => {
    localStorage.setItem(QUEUE, JSON.stringify({ nextQseq: 2, items: [{ qseq: 1, op: { op: "tr_add", tr: tr("t1") } }] }))
    const st = deferred<unknown>()
    mocks.responders.set("sync_get_status", () => st.promise)
    const sim = rustE3({ conversations: [], history: [] }, { flushed: 7 })
    const { sync, history } = await load()
    const boot = sync.bootSync()
    history.add(entry)
    expect(queue().items.map((i) => i.qseq)).toEqual([1, 2])
    st.resolve({ enabled: true, vault: { id: "v" }, migration: "native", queue_flushed_qseq: 7 })
    await boot
    await vi.waitFor(() => expect(sim.hist.size).toBe(2))
    expect(applied().map((a) => a.qseq)).toEqual([1, 8, 9])
    expect(sim.results).toEqual(["stale_qseq", "applied", "applied"])
    await vi.waitFor(() => expect(history.list().map((e) => e.source).sort()).toEqual(["s-t1", "x"]))
  })

  it("G8 blocker: D09c native marker lost to quota at the cutover: the next boot's pre-status write lands only in the frozen originals", async () => {
    localStorage.setItem(P + "conversations", JSON.stringify([{ ...conv("c1", [msg("m1")]), createdAt: 1 }]))
    status("verified")
    mocks.responders.set("sync_migrate_finalize", (a) => ({ result: "native", ack_qseq: a!.queueTail }))
    rustE3({ conversations: [conv("c1", [msg("m1")])], history: [] })
    const realSet = Storage.prototype.setItem
    const spy = vi.spyOn(Storage.prototype, "setItem").mockImplementation(function (this: Storage, k: string, v: string) {
      if (k === NATIVE) throw new DOMException("quota", "QuotaExceededError")
      realSet.call(this, k, v)
    })
    const b1 = await load()
    await b1.sync.bootSync()
    expect(b1.sync.isNative()).toBe(true)
    spy.mockRestore()
    expect(localStorage.getItem(NATIVE)).toBeNull()

    const b2 = await reboot()
    const st = deferred<unknown>()
    mocks.responders.set("sync_get_status", () => st.promise)
    const boot = b2.sync.bootSync()
    const m = b2.conversations.appendMessage("c1", { role: "user", content: "before status" })
    st.resolve({ enabled: true, vault: { id: "v" }, migration: "native" })
    await boot
    await ticks()
    expect(b2.sync.isNative()).toBe(true)
    expect(ids(b2.conversations.get("c1")!.messages)).toContain(m.id)
  })
})

// ---------------------------------------------------------------------------
describe("delta E4 display trim is display-only", () => {
  const many = (n: number) => Array.from({ length: n }, (_, i) => msg(`m${i}`, i % 2 ? "assistant" : "user", i + 1))

  async function nativeConv(n: number) {
    status("native")
    const sim = rustE3({ conversations: [conv("c1", many(n))], history: [] })
    const mods = await load()
    await mods.sync.bootSync()
    invoke.mockClear()
    return { ...mods, sim }
  }

  it("D10 regenerate twice on a trimmed view: siblings under the same user turn, hidden prefix kept, no delete", async () => {
    const { sync, conversations, sim } = await nativeConv(250)
    let shown = conversations.get("c1")!.messages
    expect(shown).toHaveLength(200)
    expect(shown[0].id).toBe("m50")
    conversations.setMessages("c1", shown.slice(0, -1))
    const a2 = conversations.appendMessage("c1", { role: "assistant", content: "a2" })
    shown = conversations.get("c1")!.messages
    expect(shown[0].id).toBe("m50")
    conversations.setMessages("c1", shown.slice(0, -1))
    const a3 = conversations.appendMessage("c1", { role: "assistant", content: "a3" })
    await vi.waitFor(() => expect(queue().items).toEqual([]))
    expect(applied().map((a) => a.op)).toEqual([
      { op: "set_active", conv: "c1", leaf: "m248" },
      { op: "msg_add", conv: "c1", msg: expect.objectContaining({ id: a2.id }), parent: "m248" },
      { op: "set_active", conv: "c1", leaf: a2.id },
      { op: "set_active", conv: "c1", leaf: "m248" },
      { op: "msg_add", conv: "c1", msg: expect.objectContaining({ id: a3.id }), parent: "m248" },
      { op: "set_active", conv: "c1", leaf: a3.id },
    ])
    const cached = sync.cachedConversations()[0].messages
    expect(cached).toHaveLength(250)
    expect([cached[0].id, cached[248].id, cached[249].id]).toEqual(["m0", "m248", a3.id])
    expect(sim.path("c1")).toEqual([...many(249).map((m) => m.id), a3.id])
    const siblings = [...sim.convs.get("c1")!.nodes.values()].filter((n) => n.parent === "m248").map((n) => n.msg.id)
    expect(siblings).toEqual(["m249", a2.id, a3.id])
  })

  it("D11 truncate past the window: [] lands before the window, a known hidden-prefix head keeps exactly its prefix", async () => {
    const { sync, conversations, sim } = await nativeConv(250)
    conversations.setMessages("c1", [])
    expect(ids(sync.cachedConversations()[0].messages)).toEqual(many(50).map((m) => m.id))
    const e = conversations.appendMessage("c1", { role: "user", content: "edited" })
    await vi.waitFor(() => expect(queue().items).toEqual([]))
    expect(applied().map((a) => a.op)).toEqual([
      { op: "set_active", conv: "c1", leaf: "m49" },
      { op: "msg_add", conv: "c1", msg: expect.objectContaining({ id: e.id }), parent: "m49" },
      { op: "set_active", conv: "c1", leaf: e.id },
    ])
    expect(sim.path("c1")).toEqual([...many(50).map((m) => m.id), e.id])

    // A list headed by a hidden-prefix message keeps exactly the messages before it.
    const { sync: s2, conversations: c2, sim: sim2 } = await (async () => {
      vi.resetModules()
      localStorage.clear()
      mocks.responders.clear()
      return nativeConv(250)
    })()
    const m10 = s2.cachedConversations()[0].messages[10]
    const m11 = s2.cachedConversations()[0].messages[11]
    c2.setMessages("c1", [m10, m11])
    await vi.waitFor(() => expect(queue().items).toEqual([]))
    expect(applied().map((a) => a.op)).toEqual([{ op: "set_active", conv: "c1", leaf: "m11" }])
    expect(sim2.path("c1")).toEqual(many(12).map((m) => m.id))
  })

  it("D12 exactly 200 then 201 messages: no hidden prefix at 200 (deviation), one hidden message at 201", async () => {
    const { sync, conversations, sim } = await nativeConv(200)
    const shown = conversations.get("c1")!.messages
    expect(shown).toHaveLength(200)
    expect(shown[0].id).toBe("m0")
    conversations.setMessages("c1", shown)
    expect(localStorage.getItem(QUEUE)).toBeNull()
    const x = conversations.appendMessage("c1", { role: "user", content: "x" })
    expect(conversations.get("c1")!.messages[0].id).toBe("m1")
    // The 201-message view hides m0: [] truncates to m0, not to nothing.
    conversations.setMessages("c1", [])
    await vi.waitFor(() => expect(queue().items).toEqual([]))
    expect(applied().map((a) => a.op)).toEqual([
      { op: "msg_add", conv: "c1", msg: expect.objectContaining({ id: x.id }), parent: "m199" },
      { op: "set_active", conv: "c1", leaf: x.id },
      { op: "set_active", conv: "c1", leaf: "m0" },
    ])
    expect(ids(sync.cachedConversations()[0].messages)).toEqual(["m0"])
    expect(sim.path("c1")).toEqual(["m0"])
    // Now an untrimmed one-message view: [] keeps the documented deviation.
    invoke.mockClear()
    conversations.setMessages("c1", [])
    await ticks()
    expect(calls("sync_queue_apply")).toHaveLength(0)
    expect(sim.path("c1")).toEqual(["m0"])
  })
})

// ---------------------------------------------------------------------------
describe("delta E5 native boot", () => {
  it("D13 bootSyncWithin renders at 3 s while the hydrate is slow; writes stay held, the frozen originals are never shown, and land after hydrate", async () => {
    localStorage.setItem(NATIVE, "true")
    localStorage.setItem(P + "conversations", JSON.stringify([{ ...conv("c1", [msg("m1")]), createdAt: 1 }]))
    status("native")
    const sim = rustE3({ conversations: [conv("c1", [msg("m1"), msg("m2", "assistant"), msg("m3")])], history: [] })
    const realLoad = mocks.responders.get("sync_chat_load")!
    const gate = deferred<void>()
    mocks.responders.set("sync_chat_load", async (a) => {
      await gate.promise
      return realLoad(a)
    })
    vi.useFakeTimers({ toFake: ["setTimeout", "clearTimeout"] })
    let m!: Msg
    let sync!: Awaited<ReturnType<typeof load>>["sync"]
    let conversations!: Awaited<ReturnType<typeof load>>["conversations"]
    try {
      ;({ sync, conversations } = await load())
      expect(sync.BOOT_WAIT_MS).toBe(3000)
      let rendered = false
      void sync.bootSyncWithin().then(() => {
        rendered = true
      })
      await vi.advanceTimersByTimeAsync(2999)
      expect(rendered).toBe(false)
      await vi.advanceTimersByTimeAsync(1)
      expect(rendered).toBe(true)
      expect(sync.isReady()).toBe(false)
      expect(conversations.list()).toEqual([])
      m = conversations.appendMessage("c1", { role: "assistant", content: "held" }) as Msg
      await vi.advanceTimersByTimeAsync(5000)
      expect(calls("sync_queue_apply")).toHaveLength(0)
      expect(calls("sync_chat_load")).toHaveLength(1)
      expect(localStorage.getItem(QUEUE)).toBeNull()
    } finally {
      vi.useRealTimers()
    }
    gate.resolve()
    await vi.waitFor(() => expect(sim.path("c1")).toEqual(["m1", "m2", "m3", m.id]))
    expect(sync.isReady()).toBe(true)
    expect(applied()[0].op).toMatchObject({ op: "msg_add", parent: "m3" })
    expect(JSON.parse(localStorage.getItem(P + "conversations")!)[0].messages).toHaveLength(1)
  })

  it("D14 writes held during a failing hydrate: backoff retries, error surfaced, nothing written anywhere until a hydrate lands", async () => {
    localStorage.setItem(NATIVE, "true")
    status("native")
    const sim = rustE3({ conversations: [conv("c1", [msg("m1")])], history: [] })
    const realLoad = mocks.responders.get("sync_chat_load")!
    let fails = 2
    mocks.responders.set("sync_chat_load", (a) => {
      if (fails > 0) {
        fails--
        throw { code: "io", message: "disk" }
      }
      return realLoad(a)
    })
    vi.useFakeTimers({ toFake: ["setTimeout", "clearTimeout"] })
    let m!: Msg
    let sync!: Awaited<ReturnType<typeof load>>["sync"]
    try {
      const mods = await load()
      sync = mods.sync
      await sync.bootSync()
      expect(sync.syncState().error).toBe("io")
      expect(sync.isReady()).toBe(false)
      expect(calls("sync_chat_load")).toHaveLength(1)
      m = mods.conversations.appendMessage("c1", { role: "user", content: "held" }) as Msg
      await vi.advanceTimersByTimeAsync(0)
      // The held write kicks an immediate retry, which fails too (backoff 2 s).
      expect(calls("sync_chat_load")).toHaveLength(2)
      expect(sync.isReady()).toBe(false)
      expect(localStorage.getItem(QUEUE)).toBeNull()
      expect(localStorage.getItem(P + "conversations")).toBeNull()
      await vi.advanceTimersByTimeAsync(1999)
      expect(calls("sync_chat_load")).toHaveLength(2)
      await vi.advanceTimersByTimeAsync(1)
    } finally {
      vi.useRealTimers()
    }
    await vi.waitFor(() => expect(sim.path("c1")).toEqual(["m1", m.id]))
    expect(calls("sync_chat_load")).toHaveLength(3)
    expect(sync.isReady()).toBe(true)
    expect(sync.syncState().error).toBeNull()
    expect(applied().filter((a) => a.op.op === "msg_add")).toHaveLength(1)
  })

  it("D15 a sync:chat-changed storm resolving out of order leaves the newest load in the cache", async () => {
    status("native")
    rustE3({ conversations: [], history: [] })
    const { sync, history } = await load()
    await sync.bootSync()
    const pend: { d: ReturnType<typeof deferred<unknown>>; k: number }[] = []
    let k = 0
    mocks.responders.set("sync_chat_load", () => {
      const d = deferred<unknown>()
      pend.push({ d, k: ++k })
      return d.promise
    })
    await act(async () => {
      for (let i = 0; i < 8; i++) chatChanged()
      await vi.waitFor(() => expect(pend).toHaveLength(8))
      for (const i of [3, 0, 7, 1, 5, 2, 6, 4]) {
        pend[i].d.resolve({ conversations: [], history: [tr(`g${pend[i].k}`)] })
        await ticks(2)
      }
    })
    expect(history.list().map((e) => e.id)).toEqual(["g8"])
  })

  it("G8 blocker: D15b a write Rust treats as a no-op drops an in-flight newer load; the cache misses a remote turn and the next append forks", async () => {
    status("native")
    const sim = rustE3({ conversations: [conv("c1", [msg("m1"), msg("a1", "assistant")])], history: [tr("p", true)] })
    const { sync, conversations, history } = await load()
    await sync.bootSync()
    // Another device's turn arrives through a sync cycle; Rust signals.
    sim.remote("c1", msg("r1"))
    const realLoad = mocks.responders.get("sync_chat_load")!
    const gate = deferred<void>()
    mocks.responders.set("sync_chat_load", async (a) => {
      const out = realLoad(a) // Rust reads now: the chat-changed baseline includes r1
      await gate.promise
      return out
    })
    await act(async () => {
      chatChanged()
      await vi.waitFor(() => expect(calls("sync_chat_load")).toHaveLength(2))
      // "Clear" with only pinned entries: tr_clear [] is a no-op in Rust, so
      // no sync:chat-changed follows (queue_apply signals on changed only).
      history.clear()
      await vi.waitFor(() => expect(queue().items).toEqual([]))
      gate.resolve()
      await ticks()
    })
    expect(sim.changed.at(-1)).toBe(false)
    expect(ids(conversations.get("c1")!.messages)).toEqual(["m1", "a1", "r1"])
    conversations.appendMessage("c1", { role: "assistant", content: "next" })
    await vi.waitFor(() => expect(queue().items).toEqual([]))
    expect(applied().find((a) => a.op.op === "msg_add")!.op.parent).toBe("r1")
  })
})

// ---------------------------------------------------------------------------
describe("delta E1 native stays native", () => {
  it("D16 native + flag off: only status, chat_load and queue_apply are invoked (no migrate, no network command)", async () => {
    localStorage.setItem(NATIVE, "true")
    mocks.responders.set("sync_get_status", () => ({ enabled: false, migration: "native" }))
    const sim = rustE3({ conversations: [conv("c1", [msg("m1")])], history: [] })
    const { sync, conversations, history } = await load()
    await sync.bootSync()
    expect(sync.isNative()).toBe(true)
    const c = conversations.create()
    conversations.appendMessage(c.id, { role: "user", content: "off" })
    history.add(entry)
    history.clear()
    await vi.waitFor(() => expect(queue().items).toEqual([]))
    await act(async () => {
      chatChanged()
      await ticks()
    })
    expect([...new Set(invoke.mock.calls.map((x) => x[0]))].sort()).toEqual([
      "sync_chat_load",
      "sync_get_status",
      "sync_queue_apply",
    ])
    expect(calls("sync_get_status")).toHaveLength(1)
    expect(listen.mock.calls.map((x) => x[0])).toEqual(["sync:chat-changed"])
    expect(sim.convs.has(c.id)).toBe(true)
    expect(localStorage.getItem(P + "conversations")).toBeNull()
  })

  it("D17 never migrated + flag off: exactly one invoke with a queue left by an earlier push, and with a failing status", async () => {
    localStorage.setItem(QUEUE, JSON.stringify({ nextQseq: 2, items: [{ qseq: 1, op: { op: "tr_delete", tr: "a" } }] }))
    mocks.responders.set("sync_get_status", () => ({ enabled: false, vault: { id: "v" }, migration: "verified" }))
    let mods = await load()
    const boot = mods.sync.bootSync()
    mods.history.add(entry)
    await boot
    mods.conversations.create()
    mods.history.add({ ...entry, source: "after" })
    await ticks()
    expect(invoke.mock.calls).toEqual([["sync_get_status"]])
    expect(listen).not.toHaveBeenCalled()
    expect(mods.sync.syncState().mode).toBe("off")
    expect(JSON.parse(localStorage.getItem(P + "history")!).map((e: { source: string }) => e.source)).toEqual(["after", "x"])

    localStorage.clear()
    mocks.responders.set("sync_get_status", () => {
      throw { code: "io", message: "x" }
    })
    mods = await reboot()
    await mods.sync.bootSync()
    mods.history.add(entry)
    await ticks()
    expect(invoke.mock.calls).toEqual([["sync_get_status"]])
    expect(mods.sync.syncState().mode).toBe("off")
  })
})

// ===========================================================================
// Final pass (G8 delta-review decisions E2a, E5a, E5b, E1a): attacks on the
// delta fixes, with the tree-aware Rust stand-in above.
// ===========================================================================

const img = (id: string) => ({
  id,
  mimeType: "image/png" as const,
  data: `${id}:`.padEnd(4000, "A"),
  width: 8,
  height: 8,
})

/** A size-based quota on the queue key; records every queue write that landed. */
function quotaBySize(limit: number) {
  const real = Storage.prototype.setItem
  const stored: string[] = []
  const spy = vi.spyOn(Storage.prototype, "setItem").mockImplementation(function (this: Storage, k: string, v: string) {
    if (k === QUEUE && v.length > limit) throw new DOMException("quota", "QuotaExceededError")
    real.call(this, k, v)
    if (k === QUEUE) stored.push(v)
  })
  return { spy, stored }
}

/** Every image payload handed to `sync_queue_apply` (failed calls included). */
function sentImages(): { id: string; data: string }[] {
  return applied()
    .filter((a) => a.op.op === "msg_add")
    .flatMap((a) => ((a.op.msg as Msg).images ?? []) as { id: string; data: string }[])
}

/** `sync_chat_load` reads at invoke time and replies when its gate opens. */
function gateLoads() {
  const real = mocks.responders.get("sync_chat_load")!
  const gates: ReturnType<typeof deferred<void>>[] = []
  mocks.responders.set("sync_chat_load", async (a) => {
    const out = real(a)
    const g = deferred<void>()
    gates.push(g)
    await g.promise
    return out
  })
  return gates
}

const storedConvIds = () =>
  ids((JSON.parse(localStorage.getItem(P + "conversations") ?? "[]") as { messages: Msg[] }[])[0]?.messages ?? [])

describe("final E2a native quota never sheds pixels", () => {
  it("F01 native + size quota + several images, direct apply ok: full pixels reach Rust, no stripped queue write, the hydrate keeps them", async () => {
    status("native")
    const sim = rustE3({ conversations: [conv("c1", [msg("m1")])], history: [] })
    const { sync, conversations } = await load()
    await sync.bootSync()
    const q = quotaBySize(3000)
    const a = conversations.appendMessage("c1", { role: "user", content: "pics", images: [img("i1"), img("i2"), img("i3")] })
    const b = conversations.appendMessage("c1", { role: "user", content: "more", images: [img("i4"), img("i5")] })
    const t = conversations.appendMessage("c1", { role: "assistant", content: "text only" })
    expect(sync.syncState().error).toBe("queue_write_failed")
    // E2.1: shown while the direct apply is in flight.
    expect(ids(conversations.get("c1")!.messages)).toEqual(["m1", a.id, b.id, t.id])
    await vi.waitFor(() => expect(sim.path("c1")).toEqual(["m1", a.id, b.id, t.id]))
    await vi.waitFor(() => expect(queue().items).toEqual([]))
    q.spy.mockRestore()
    // E2a: the image-free retry is never attempted under native.
    expect(q.stored.some((v) => v.includes('"data":""'))).toBe(false)
    // Each append is msg_add + set_active; memory and queued items in one order.
    expect(applied().map((x) => x.qseq)).toEqual([1, 2, 3, 4, 5, 6])
    const sent = sentImages()
    expect(sent.map((i) => i.id)).toEqual(["i1", "i2", "i3", "i4", "i5"])
    expect(sent.every((i) => i.data.length === 4000)).toBe(true)
    expect(sync.syncState().writeFailures).toBe(0)
    await act(async () => {
      chatChanged()
      await vi.waitFor(() => expect(calls("sync_chat_load")).toHaveLength(2))
      await ticks()
    })
    const shown = conversations.get("c1")!.messages
    expect(ids(shown)).toEqual(["m1", a.id, b.id, t.id])
    expect(shown.flatMap((m) => m.images ?? []).map((i) => i.data.length)).toEqual([4000, 4000, 4000, 4000, 4000])
  })

  it("F02 native + size quota + several images, direct apply fails: refused and rolled back, never retried image-free, the next write parents to m1", async () => {
    status("native")
    const sim = rustE3({ conversations: [conv("c1", [msg("m1")])], history: [] })
    const { sync, conversations } = await load()
    await sync.bootSync()
    const real = mocks.responders.get("sync_queue_apply")!
    mocks.responders.set("sync_queue_apply", (a) => {
      const op = a!.op as Op
      if (op.op === "msg_add" && ((op.msg as Msg).images ?? []).length) throw { code: "io", message: "disk" }
      return real(a)
    })
    const q = quotaBySize(3000)
    conversations.appendMessage("c1", { role: "user", content: "pics", images: [img("i1"), img("i2"), img("i3")] })
    await vi.waitFor(() => expect(sync.syncState().writeFailures).toBe(1))
    q.spy.mockRestore()
    expect(ids(conversations.get("c1")!.messages)).toEqual(["m1"])
    expect(sync.syncState().error).toBe("io")
    expect(q.stored).toEqual([])
    expect(sim.path("c1")).toEqual(["m1"])
    const t = conversations.appendMessage("c1", { role: "assistant", content: "after" })
    await vi.waitFor(() => expect(queue().items).toEqual([]))
    expect(sim.path("c1")).toEqual(["m1", t.id])
    expect(applied().filter((x) => x.op.op === "msg_add").map((x) => x.op.parent)).toEqual(["m1", "m1"])
    // One attempt with full pixels, then nothing image-related was ever resent.
    const sent = sentImages()
    expect(sent.map((i) => i.id)).toEqual(["i1", "i2", "i3"])
    expect(sent.every((i) => i.data.length === 4000)).toBe(true)
  })

  it("F03 write-hold + size quota: the image-free retry is allowed there and the localStorage original keeps the pixels", async () => {
    localStorage.setItem(P + "conversations", JSON.stringify([conv("c1", [msg("m1")])]))
    status("verified")
    mocks.responders.set("sync_migrate_finalize", () => ({ result: "deferred", attempts: 3, mismatches: 1 }))
    const { sync, conversations } = await load()
    await sync.bootSync()
    expect(sync.syncState().mode).toBe("hold")
    const q = quotaBySize(3000)
    const a = conversations.appendMessage("c1", { role: "user", content: "pics", images: [img("i1"), img("i2"), img("i3")] })
    q.spy.mockRestore()
    const item = queue().items.find((i) => i.op.op === "msg_add")!
    expect(item.op).toMatchObject({ op: "msg_add", msg: { id: a.id } })
    expect(((item.op.msg as Msg).images as { id: string; data: string }[]).map((i) => [i.id, i.data])).toEqual([
      ["i1", ""],
      ["i2", ""],
      ["i3", ""],
    ])
    const original = (JSON.parse(localStorage.getItem(P + "conversations")!) as { messages: { images?: { data: string }[] }[] }[])[0]
    expect(original.messages.at(-1)!.images!.map((i) => i.data.length)).toEqual([4000, 4000, 4000])
    expect(sync.syncState().error).not.toBe("queue_write_failed")
    expect(calls("sync_queue_apply")).toHaveLength(0)
  })
})

/** Boot 1: hold -> native cutover whose marker write fails on quota. */
async function cutoverMarkerLost() {
  localStorage.setItem(P + "conversations", JSON.stringify([{ ...conv("c1", [msg("m1")]), createdAt: 1 }]))
  status("verified")
  mocks.responders.set("sync_migrate_finalize", (a) => ({ result: "native", ack_qseq: a!.queueTail }))
  const sim = rustE3({ conversations: [conv("c1", [msg("m1")])], history: [] })
  const spy = markerQuota()
  const b1 = await load()
  await b1.sync.bootSync()
  spy.mockRestore()
  return { sim, b1 }
}

function markerQuota() {
  const real = Storage.prototype.setItem
  return vi.spyOn(Storage.prototype, "setItem").mockImplementation(function (this: Storage, k: string, v: string) {
    if (k === NATIVE) throw new DOMException("quota", "QuotaExceededError")
    real.call(this, k, v)
  })
}

describe("final E5a the native marker is advisory", () => {
  it("F04 a failed marker write at the cutover is surfaced; the session stays native and writes reach Rust", async () => {
    const { sim, b1 } = await cutoverMarkerLost()
    expect(b1.sync.isNative()).toBe(true)
    expect(localStorage.getItem(NATIVE)).toBeNull()
    expect(b1.sync.syncState().error).toBe("marker_write_failed")
    const m = b1.conversations.appendMessage("c1", { role: "user", content: "after cutover" })
    await vi.waitFor(() => expect(sim.path("c1")).toEqual(["m1", m.id]))
    expect(storedConvIds()).toEqual(["m1"])
  })

  it("F05 next boot: no marker and a store_error status keep the localStorage path, retry with backoff, then the native status re-establishes the marker and replays the write", async () => {
    const { sim } = await cutoverMarkerLost()
    const b2 = await reboot()
    let n = 0
    mocks.responders.set("sync_get_status", () =>
      ++n <= 2
        ? { enabled: true, vault: { id: "v" }, migration: "none", store_error: true }
        : { enabled: true, vault: { id: "v" }, migration: "native" },
    )
    vi.useFakeTimers({ toFake: ["setTimeout", "clearTimeout"] })
    let m!: Msg
    try {
      await b2.sync.bootSync()
      expect(b2.sync.syncState().mode).toBe("off")
      expect(b2.sync.syncState().error).toBe("store_error")
      expect(localStorage.getItem(NATIVE)).toBeNull()
      m = b2.conversations.appendMessage("c1", { role: "user", content: "during store_error" }) as Msg
      expect(storedConvIds()).toEqual(["m1", m.id])
      await vi.advanceTimersByTimeAsync(999)
      expect(calls("sync_get_status")).toHaveLength(1)
      await vi.advanceTimersByTimeAsync(1)
      expect(calls("sync_get_status")).toHaveLength(2)
      expect(b2.sync.syncState().mode).toBe("off")
      await vi.advanceTimersByTimeAsync(1999)
      expect(calls("sync_get_status")).toHaveLength(2)
      await vi.advanceTimersByTimeAsync(1)
      expect(calls("sync_get_status")).toHaveLength(3)
    } finally {
      vi.useRealTimers()
    }
    await vi.waitFor(() => expect(sim.path("c1")).toEqual(["m1", m.id]))
    await vi.waitFor(() => expect(queue().items).toEqual([]))
    expect(b2.sync.isNative()).toBe(true)
    expect(localStorage.getItem(NATIVE)).toBe("true")
    expect(b2.sync.syncState().error).toBeNull()
    expect(ids(b2.conversations.get("c1")!.messages)).toEqual(["m1", m.id])
    expect(applied().filter((x) => x.op.op === "msg_add")).toHaveLength(1)
  })

  it("F06 observation: an unreachable status (invoke error) with no marker stays on localStorage without retry; the next boot recovers native (marker failure surfaced again, then re-written) without that write", async () => {
    const { sim } = await cutoverMarkerLost()
    let b = await reboot()
    mocks.responders.set("sync_get_status", () => {
      throw { code: "ipc", message: "down" }
    })
    vi.useFakeTimers({ toFake: ["setTimeout", "clearTimeout"] })
    let m!: Msg
    try {
      await b.sync.bootSync()
      m = b.conversations.appendMessage("c1", { role: "user", content: "status down" }) as Msg
      await vi.advanceTimersByTimeAsync(60_000)
      expect(calls("sync_get_status")).toHaveLength(1)
      expect(b.sync.syncState().mode).toBe("off")
    } finally {
      vi.useRealTimers()
    }
    expect(storedConvIds()).toEqual(["m1", m.id])

    b = await reboot()
    status("native")
    const spy = markerQuota()
    await b.sync.bootSync()
    spy.mockRestore()
    expect(b.sync.isNative()).toBe(true)
    expect(b.sync.syncState().error).toBe("marker_write_failed")
    expect(localStorage.getItem(NATIVE)).toBeNull()
    // Accepted by E5a (no marker, no status = pre-migration path): the write
    // stays in the frozen original only.
    expect(ids(b.conversations.get("c1")!.messages)).toEqual(["m1"])
    expect(sim.path("c1")).toEqual(["m1"])

    b = await reboot()
    await b.sync.bootSync()
    expect(localStorage.getItem(NATIVE)).toBe("true")
    expect(b.sync.syncState().error).toBeNull()
  })
})

describe("final E5b only a projection change invalidates a load", () => {
  it("F07 no-op writes interleaved with chat-changed: a load landing after, or during, a no-op apply is applied; the real write parents to the newest remote turn", async () => {
    status("native")
    const sim = rustE3({ conversations: [conv("c1", [msg("m1"), msg("a1", "assistant")])], history: [tr("p", true)] })
    const { sync, conversations, history } = await load()
    await sync.bootSync()
    sim.remote("c1", msg("r1"))
    const gates = gateLoads()
    await act(async () => {
      chatChanged()
      await vi.waitFor(() => expect(gates).toHaveLength(1))
      conversations.rename("c1", "t") // same title: a no-op in Rust
      await vi.waitFor(() => expect(sim.changed).toEqual([false]))
      await vi.waitFor(() => expect(queue().items).toEqual([]))
      gates[0].resolve()
      await ticks()
    })
    expect(ids(conversations.get("c1")!.messages)).toEqual(["m1", "a1", "r1"])

    sim.remote("c1", msg("r2"))
    const realApply = mocks.responders.get("sync_queue_apply")!
    const hold = deferred<void>()
    mocks.responders.set("sync_queue_apply", async (a) => {
      const out = realApply(a)
      await hold.promise
      return out
    })
    await act(async () => {
      chatChanged()
      await vi.waitFor(() => expect(gates).toHaveLength(2))
      history.clear() // only a pinned entry: tr_clear [] is a no-op
      await vi.waitFor(() => expect(applied().filter((x) => x.op.op === "tr_clear")).toHaveLength(1))
      gates[1].resolve()
      await ticks()
      hold.resolve()
      await vi.waitFor(() => expect(queue().items).toEqual([]))
      await ticks()
    })
    expect(sim.changed).toEqual([false, false])
    expect(ids(conversations.get("c1")!.messages)).toEqual(["m1", "a1", "r1", "r2"])
    mocks.responders.set("sync_queue_apply", realApply)
    const n = conversations.appendMessage("c1", { role: "user", content: "real" })
    await vi.waitFor(() => expect(queue().items).toEqual([]))
    expect(applied().filter((x) => x.op.op === "msg_add").at(-1)!.op.parent).toBe("r2")
    expect(sim.path("c1")).toEqual(["m1", "a1", "r1", "r2", n.id])
  })

  it("F08 a real write supersedes a load read before it; a no-op during the next load does not; the stale reply resolving last is dropped", async () => {
    status("native")
    const sim = rustE3({ conversations: [conv("c1", [msg("m1"), msg("a1", "assistant")])], history: [] })
    const { sync, conversations } = await load()
    await sync.bootSync()
    const realApply = mocks.responders.get("sync_queue_apply")!
    mocks.responders.set("sync_queue_apply", (a) => {
      const out = realApply(a) as { result: string; changed?: boolean }
      // Rust `notify_chat`: an apply that moved the projection signals.
      if (out.result === "applied" && out.changed) setTimeout(chatChanged, 0)
      return out
    })
    const gates = gateLoads()
    let u!: Msg
    await act(async () => {
      chatChanged() // reads [m1, a1] before the write
      await vi.waitFor(() => expect(gates).toHaveLength(1))
      u = conversations.appendMessage("c1", { role: "user", content: "real" }) as Msg
      await vi.waitFor(() => expect(gates).toHaveLength(2)) // from the write's signal
      conversations.rename("c1", "t")
      await vi.waitFor(() => expect(sim.changed).toEqual([true, false]))
      await vi.waitFor(() => expect(queue().items).toEqual([]))
      gates[1].resolve()
      await ticks()
      gates[0].resolve()
      await ticks()
    })
    expect(sim.path("c1")).toEqual(["m1", "a1", u.id])
    expect(ids(conversations.get("c1")!.messages)).toEqual(["m1", "a1", u.id])
    expect(calls("sync_chat_load")).toHaveLength(3)
  })

  it("G8 blocker: F09 a refused (quota + failed direct apply) write while a newer load is in flight: the kept load is never applied, the next append parents to the stale leaf", async () => {
    status("native")
    const sim = rustE3({ conversations: [conv("c1", [msg("m1"), msg("a1", "assistant")])], history: [] })
    const { sync, conversations, history } = await load()
    await sync.bootSync()
    sim.remote("c1", msg("r1"))
    const gates = gateLoads()
    const realApply = mocks.responders.get("sync_queue_apply")!
    const fail = deferred<void>()
    mocks.responders.set("sync_queue_apply", async (a) => {
      if ((a!.op as Op).op === "tr_add") {
        await fail.promise
        throw { code: "io", message: "disk" }
      }
      return realApply(a)
    })
    await act(async () => {
      chatChanged()
      await vi.waitFor(() => expect(gates).toHaveLength(1))
      const q = quotaOnQueue(() => true)
      history.add(entry)
      q.mockRestore()
      await vi.waitFor(() => expect(calls("sync_queue_apply")).toHaveLength(1))
      gates[0].resolve()
      await ticks()
      fail.resolve()
      await vi.waitFor(() => expect(sync.syncState().writeFailures).toBe(1))
      await ticks()
    })
    expect(history.list()).toEqual([])
    expect(ids(conversations.get("c1")!.messages)).toEqual(["m1", "a1", "r1"])
    conversations.appendMessage("c1", { role: "user", content: "next" })
    await vi.waitFor(() => expect(queue().items).toEqual([]))
    expect(applied().filter((x) => x.op.op === "msg_add").at(-1)!.op.parent).toBe("r1")
  })
})

describe("final E1a store errors are never defaults", () => {
  const variants: [string, Record<string, unknown>][] = [
    ["flag on store_error", { enabled: true, vault: { id: "v" }, migration: "none", store_error: true }],
    ["flag off store_error", { enabled: false, migration: "none", store_error: true }],
    ["flag on unknown state", { enabled: true, vault: { id: "v" }, migration: "unknown", store_error: false }],
  ]
  for (const [name, bad] of variants) {
    it(`F10 ${name} on a marked device: stays native and holding, marker kept, surfaced, backoff 1/2/4 s, then recovers and applies the held write`, async () => {
      localStorage.setItem(NATIVE, "true")
      const sim = rustE3({ conversations: [conv("c1", [msg("m1")])], history: [] })
      let n = 0
      mocks.responders.set("sync_get_status", () => (++n <= 3 ? bad : { enabled: bad.enabled, migration: "native" }))
      vi.useFakeTimers({ toFake: ["setTimeout", "clearTimeout"] })
      let m!: Msg
      let sync!: Awaited<ReturnType<typeof load>>["sync"]
      try {
        const mods = await load()
        sync = mods.sync
        const boot = sync.bootSync()
        m = mods.conversations.appendMessage("c1", { role: "user", content: "held" }) as Msg
        await boot
        expect(sync.isNative()).toBe(true)
        expect(sync.isReady()).toBe(false)
        expect(sync.syncState().error).toBe("store_error")
        expect(localStorage.getItem(NATIVE)).toBe("true")
        expect(localStorage.getItem(P + "conversations")).toBeNull()
        expect(localStorage.getItem(QUEUE)).toBeNull()
        expect(calls("sync_chat_load")).toHaveLength(0)
        for (const [wait, count] of [
          [999, 1],
          [1, 2],
          [1999, 2],
          [1, 3],
          [3999, 3],
          [1, 4],
        ]) {
          await vi.advanceTimersByTimeAsync(wait)
          expect(calls("sync_get_status")).toHaveLength(count)
          if (count < 4) expect(localStorage.getItem(NATIVE)).toBe("true")
        }
      } finally {
        vi.useRealTimers()
      }
      await vi.waitFor(() => expect(sim.path("c1")).toEqual(["m1", m.id]))
      expect(sync.isReady()).toBe(true)
      expect(sync.syncState().error).toBeNull()
      expect(localStorage.getItem(NATIVE)).toBe("true")
      expect(localStorage.getItem(P + "conversations")).toBeNull()
    })
  }

  it("F11 flag off, no marker, store_error then a definite not-native answer: localStorage path throughout, one retry, then one invoke total per answer and no listener", async () => {
    let n = 0
    mocks.responders.set("sync_get_status", () =>
      ++n === 1 ? { enabled: false, migration: "none", store_error: true } : { enabled: false, migration: "none" },
    )
    vi.useFakeTimers({ toFake: ["setTimeout", "clearTimeout"] })
    try {
      const { sync, history } = await load()
      await sync.bootSync()
      expect(sync.syncState().mode).toBe("off")
      expect(sync.syncState().error).toBe("store_error")
      history.add(entry)
      expect(JSON.parse(localStorage.getItem(P + "history")!)).toHaveLength(1)
      await vi.advanceTimersByTimeAsync(1000)
      expect(calls("sync_get_status")).toHaveLength(2)
      await vi.advanceTimersByTimeAsync(60_000)
      expect(calls("sync_get_status")).toHaveLength(2)
      expect(sync.syncState().mode).toBe("off")
      expect(localStorage.getItem(QUEUE)).toBeNull()
      expect(localStorage.getItem(NATIVE)).toBeNull()
      expect(listen).not.toHaveBeenCalled()
      expect(invoke.mock.calls.map((c) => c[0])).toEqual(["sync_get_status", "sync_get_status"])
    } finally {
      vi.useRealTimers()
    }
  })

  it("F12 flag off native boot, sync_chat_load store_error twice: mode and marker kept, surfaced, backoff, then hydrated", async () => {
    localStorage.setItem(NATIVE, "true")
    mocks.responders.set("sync_get_status", () => ({ enabled: false, migration: "native" }))
    rustE3({ conversations: [conv("c1", [msg("m1")])], history: [] })
    const realLoad = mocks.responders.get("sync_chat_load")!
    let fails = 2
    mocks.responders.set("sync_chat_load", (a) => {
      if (fails > 0) {
        fails--
        throw { code: "store_error", message: "load" }
      }
      return realLoad(a)
    })
    vi.useFakeTimers({ toFake: ["setTimeout", "clearTimeout"] })
    let sync!: Awaited<ReturnType<typeof load>>["sync"]
    try {
      sync = (await load()).sync
      await sync.bootSync()
      expect(sync.isNative()).toBe(true)
      expect(sync.isReady()).toBe(false)
      expect(sync.syncState().error).toBe("store_error")
      expect(localStorage.getItem(NATIVE)).toBe("true")
      await vi.advanceTimersByTimeAsync(1000)
      expect(calls("sync_chat_load")).toHaveLength(2)
      expect(localStorage.getItem(NATIVE)).toBe("true")
      await vi.advanceTimersByTimeAsync(2000)
      expect(calls("sync_chat_load")).toHaveLength(3)
    } finally {
      vi.useRealTimers()
    }
    await vi.waitFor(() => expect(sync.isReady()).toBe(true))
    expect(sync.syncState().error).toBeNull()
    expect(localStorage.getItem(NATIVE)).toBe("true")
  })

  it("G8 blocker: F13 a store_error on a chat-changed hydrate after boot is surfaced but never retried: the remote turn stays out of the cache", async () => {
    status("native")
    const sim = rustE3({ conversations: [conv("c1", [msg("m1"), msg("a1", "assistant")])], history: [] })
    const { sync, conversations } = await load()
    await sync.bootSync()
    sim.remote("c1", msg("r1"))
    const realLoad = mocks.responders.get("sync_chat_load")!
    let fails = 1
    mocks.responders.set("sync_chat_load", (a) => {
      if (fails > 0) {
        fails--
        throw { code: "store_error", message: "load" }
      }
      return realLoad(a)
    })
    vi.useFakeTimers({ toFake: ["setTimeout", "clearTimeout"] })
    try {
      chatChanged()
      await vi.advanceTimersByTimeAsync(0)
      expect(sync.syncState().error).toBe("store_error")
      expect(sync.isNative()).toBe(true)
      expect(localStorage.getItem(NATIVE)).toBe("true")
      await vi.advanceTimersByTimeAsync(31_000)
    } finally {
      vi.useRealTimers()
    }
    await vi.waitFor(() => expect(ids(conversations.get("c1")!.messages)).toEqual(["m1", "a1", "r1"]))
  })

  it("G8 blocker: F14 a store_error from sync_queue_apply after boot keeps the durable item but is never retried with backoff", async () => {
    status("native")
    const sim = rustE3({ conversations: [conv("c1", [msg("m1")])], history: [] })
    const { sync, conversations } = await load()
    await sync.bootSync()
    const realApply = mocks.responders.get("sync_queue_apply")!
    let fails = 1
    mocks.responders.set("sync_queue_apply", (a) => {
      if (fails > 0) {
        fails--
        throw { code: "store_error", message: "load" }
      }
      return realApply(a)
    })
    vi.useFakeTimers({ toFake: ["setTimeout", "clearTimeout"] })
    let m!: Msg
    try {
      m = conversations.appendMessage("c1", { role: "user", content: "x" }) as Msg
      await vi.advanceTimersByTimeAsync(0)
      expect(sync.syncState().error).toBe("store_error")
      expect(queue().items.map((i) => i.op.op)).toEqual(["msg_add", "set_active"])
      expect(sync.isNative()).toBe(true)
      expect(localStorage.getItem(NATIVE)).toBe("true")
      await vi.advanceTimersByTimeAsync(31_000)
    } finally {
      vi.useRealTimers()
    }
    await vi.waitFor(() => expect(sim.path("c1")).toEqual(["m1", m.id]))
  })
})

// ===========================================================================
// G8 confirm: narrow attacks on the last fixes (BF1, F09, F13/F14). A real
// defect is `it.skip` with the label "G8 blocker: ...".
// ===========================================================================

/**
 * `sync_chat_load` reads Rust at invoke time; each reply waits for its own
 * gate, which the test resolves or rejects. With `auto`, replies are direct.
 */
function ctlLoads() {
  const real = mocks.responders.get("sync_chat_load")!
  const ctl = { auto: false, pend: [] as ReturnType<typeof deferred<void>>[] }
  mocks.responders.set("sync_chat_load", async (a) => {
    const out = real(a)
    if (ctl.auto) return out
    const g = deferred<void>()
    ctl.pend.push(g)
    await g.promise
    return out
  })
  return ctl
}

/** `sync_queue_apply` for `tr_add` with `source`: waits for `gate`, then fails (io). */
function failTrAdd(source: string, gate: Promise<void>) {
  const real = mocks.responders.get("sync_queue_apply")!
  mocks.responders.set("sync_queue_apply", async (a) => {
    const op = a!.op as Op
    if (op.op === "tr_add" && (op.tr as { source: string }).source === source) {
      await gate
      throw { code: "io", message: "disk" }
    }
    return real(a)
  })
}

const storeError = { code: "store_error", message: "load" }
const sources = (list: { source: string }[]) => list.map((e) => e.source).sort()

describe("G8 confirm BF1 stale-reply storms during boot", () => {
  it("C01 interleaved chat-changed storm during the boot load; boot reply first, stale replies in mixed order, newest last: keeps holding, held writes (before and after boot returns) land on the newest", async () => {
    localStorage.setItem(NATIVE, "true")
    localStorage.setItem(P + "conversations", JSON.stringify([{ ...conv("c1", [msg("m1")]), createdAt: 1 }]))
    status("native")
    const sim = rustE3({ conversations: [conv("c1", [msg("m1"), msg("a1", "assistant")])], history: [] })
    const ctl = ctlLoads()
    const { sync, conversations } = await load()
    const boot = sync.bootSync()
    await vi.waitFor(() => expect(ctl.pend).toHaveLength(1))
    const h1 = conversations.appendMessage("c1", { role: "user", content: "h1" }) as Msg
    sim.remote("c1", msg("r1"))
    // Each event lands after the previous refresh started its load.
    for (let i = 0; i < 5; i++) {
      chatChanged()
      await vi.waitFor(() => expect(ctl.pend).toHaveLength(i + 2))
    }
    ctl.pend[0].resolve()
    await boot
    const h2 = conversations.appendMessage("c1", { role: "user", content: "h2" }) as Msg
    for (const i of [3, 1, 4, 2]) {
      ctl.pend[i].resolve()
      await ticks(3)
      expect(sync.isReady()).toBe(false)
      expect(sync.isHydrated()).toBe(false)
      expect(conversations.list()).toEqual([])
      expect(calls("sync_queue_apply")).toHaveLength(0)
      expect(sync.syncState().writeFailures).toBe(0)
      expect(sync.syncState().error).not.toBe("write_failed")
    }
    ctl.auto = true
    ctl.pend[5].resolve()
    await vi.waitFor(() => expect(sim.path("c1")).toEqual(["m1", "a1", "r1", h1.id, h2.id]))
    expect(sync.isReady()).toBe(true)
    expect(sync.syncState().writeFailures).toBe(0)
    expect(applied().find((x) => x.op.op === "msg_add")!.op.parent).toBe("r1")
    await vi.waitFor(() => expect(ids(conversations.get("c1")!.messages)).toEqual(["m1", "a1", "r1", h1.id, h2.id]))
    // The frozen original was never written.
    expect(storedConvIds()).toEqual(["m1"])
  })

  it("C02 a burst (no interleave) during the boot load: boot reply dropped, a middle reply applies first, the rest are ordered; held writes parent to r1 once", async () => {
    localStorage.setItem(NATIVE, "true")
    status("native")
    const sim = rustE3({ conversations: [conv("c1", [msg("m1")])], history: [] })
    const ctl = ctlLoads()
    const { sync, conversations } = await load()
    void sync.bootSync()
    await vi.waitFor(() => expect(ctl.pend).toHaveLength(1))
    const h = conversations.appendMessage("c1", { role: "user", content: "h" }) as Msg
    sim.remote("c1", msg("r1"))
    for (let i = 0; i < 4; i++) chatChanged()
    await vi.waitFor(() => expect(ctl.pend).toHaveLength(5))
    ctl.auto = true
    ctl.pend[0].resolve()
    await ticks(3)
    expect(sync.isReady()).toBe(false)
    for (const i of [3, 1, 4, 2]) {
      ctl.pend[i].resolve()
      await ticks(3)
    }
    await vi.waitFor(() => expect(sim.path("c1")).toEqual(["m1", "r1", h.id]))
    expect(applied().filter((x) => x.op.op === "msg_add")).toHaveLength(1)
    expect(sync.syncState().writeFailures).toBe(0)
    await vi.waitFor(() => expect(ids(conversations.get("c1")!.messages)).toEqual(["m1", "r1", h.id]))
  })

  it("C03 the newest boot-storm load fails (store_error) while stale replies succeed: keeps holding, surfaced, the backoff retry hydrates and releases", async () => {
    localStorage.setItem(NATIVE, "true")
    status("native")
    const sim = rustE3({ conversations: [conv("c1", [msg("m1")])], history: [] })
    const ctl = ctlLoads()
    const { sync, conversations } = await load()
    void sync.bootSync()
    await vi.waitFor(() => expect(ctl.pend).toHaveLength(1))
    const h = conversations.appendMessage("c1", { role: "user", content: "h" }) as Msg
    sim.remote("c1", msg("r1"))
    for (let i = 0; i < 3; i++) {
      chatChanged()
      await vi.waitFor(() => expect(ctl.pend).toHaveLength(i + 2))
    }
    ctl.pend[3].reject(storeError)
    await ticks(3)
    for (const i of [0, 2, 1]) {
      ctl.pend[i].resolve()
      await ticks(3)
      expect(sync.isReady()).toBe(false)
      expect(conversations.list()).toEqual([])
    }
    expect(sync.syncState().error).toBe("store_error")
    expect(calls("sync_queue_apply")).toHaveLength(0)
    ctl.auto = true
    await vi.waitFor(() => expect(sim.path("c1")).toEqual(["m1", "r1", h.id]), { timeout: 4000 })
    expect(sync.isReady()).toBe(true)
    await vi.waitFor(() => expect(sync.syncState().error).toBeNull())
    expect(sync.syncState().writeFailures).toBe(0)
  })

  it("C04 a storm during a store_error boot retry: the retry's own reply is dropped, the device keeps holding until the newest lands", async () => {
    localStorage.setItem(NATIVE, "true")
    status("native")
    const sim = rustE3({ conversations: [conv("c1", [msg("m1")])], history: [] })
    const ctl = ctlLoads()
    const { sync, conversations } = await load()
    void sync.bootSync()
    await vi.waitFor(() => expect(ctl.pend).toHaveLength(1))
    const h = conversations.appendMessage("c1", { role: "user", content: "h" }) as Msg
    ctl.pend[0].reject(storeError)
    await vi.waitFor(() => expect(sync.syncState().error).toBe("store_error"))
    // Backoff retry (1 s) starts the second load.
    await vi.waitFor(() => expect(ctl.pend).toHaveLength(2), { timeout: 3000 })
    sim.remote("c1", msg("r1"))
    for (let i = 0; i < 2; i++) {
      chatChanged()
      await vi.waitFor(() => expect(ctl.pend).toHaveLength(i + 3))
    }
    ctl.pend[1].resolve()
    await ticks(3)
    ctl.pend[2].resolve()
    await ticks(3)
    expect(sync.isReady()).toBe(false)
    expect(conversations.list()).toEqual([])
    expect(calls("sync_queue_apply")).toHaveLength(0)
    ctl.auto = true
    ctl.pend[3].resolve()
    await vi.waitFor(() => expect(sim.path("c1")).toEqual(["m1", "r1", h.id]))
    expect(sync.isReady()).toBe(true)
    await vi.waitFor(() => expect(sync.syncState().error).toBeNull())
  })
})

describe("G8 confirm F09 mixed pending and failed direct applies", () => {
  it("C05 a refused memory head with a durable item behind it and a kept load: refused write dropped, the durable one lands, the cache reaches the newest projection, next append parents to r1", async () => {
    status("native")
    const sim = rustE3({ conversations: [conv("c1", [msg("m1"), msg("a1", "assistant")])], history: [] })
    const { sync, conversations, history } = await load()
    await sync.bootSync()
    sim.remote("c1", msg("r1"))
    const ctl = ctlLoads()
    const fail = deferred<void>()
    failTrAdd("A", fail.promise)
    await act(async () => {
      chatChanged()
      await vi.waitFor(() => expect(ctl.pend).toHaveLength(1))
      const q = quotaOnQueue(() => true)
      history.add({ ...entry, source: "A" })
      q.mockRestore()
      history.add({ ...entry, source: "B" })
      expect(queue().items.map((i) => i.op.op)).toEqual(["tr_add"])
      await vi.waitFor(() => expect(calls("sync_queue_apply")).toHaveLength(1))
      ctl.auto = true
      ctl.pend[0].resolve()
      await ticks()
      fail.resolve()
      await vi.waitFor(() => expect(sync.syncState().writeFailures).toBe(1))
      await vi.waitFor(() => expect(queue().items).toEqual([]), { timeout: 4000 })
      await vi.waitFor(() => expect(ids(conversations.get("c1")!.messages)).toEqual(["m1", "a1", "r1"]), { timeout: 4000 })
    })
    await vi.waitFor(() => expect(sources(history.list())).toEqual(["B"]))
    expect(sources([...sim.hist.values()])).toEqual(["B"])
    conversations.appendMessage("c1", { role: "user", content: "next" })
    await vi.waitFor(() => expect(queue().items).toEqual([]))
    expect(applied().filter((x) => x.op.op === "msg_add").at(-1)!.op.parent).toBe("r1")
  })

  it("C06 a no-op durable item applied first, then a refused memory item, with a kept load: the kept load is applied, next append parents to r1", async () => {
    status("native")
    const sim = rustE3({ conversations: [conv("c1", [msg("m1"), msg("a1", "assistant")])], history: [tr("p", true)] })
    const { sync, conversations, history } = await load()
    await sync.bootSync()
    sim.remote("c1", msg("r1"))
    const ctl = ctlLoads()
    const fail = deferred<void>()
    failTrAdd("A", fail.promise)
    await act(async () => {
      chatChanged()
      await vi.waitFor(() => expect(ctl.pend).toHaveLength(1))
      history.clear()
      const q = quotaOnQueue(() => true)
      history.add({ ...entry, source: "A" })
      q.mockRestore()
      await vi.waitFor(() => expect(calls("sync_queue_apply")).toHaveLength(2))
      expect(sim.changed.at(-1)).toBe(false)
      ctl.auto = true
      ctl.pend[0].resolve()
      await ticks()
      fail.resolve()
      await vi.waitFor(() => expect(sync.syncState().writeFailures).toBe(1))
      await ticks()
    })
    expect(ids(conversations.get("c1")!.messages)).toEqual(["m1", "a1", "r1"])
    expect(history.list().map((e) => e.id)).toEqual(["p"])
    conversations.appendMessage("c1", { role: "user", content: "next" })
    await vi.waitFor(() => expect(queue().items).toEqual([]))
    expect(applied().filter((x) => x.op.op === "msg_add").at(-1)!.op.parent).toBe("r1")
  })

  it("C07 a refused memory item while a kept load is superseded by a newer chat-changed: the newest projection (r1, r2) wins, next append parents to r2", async () => {
    status("native")
    const sim = rustE3({ conversations: [conv("c1", [msg("m1"), msg("a1", "assistant")])], history: [] })
    const { sync, conversations, history } = await load()
    await sync.bootSync()
    sim.remote("c1", msg("r1"))
    const ctl = ctlLoads()
    const fail = deferred<void>()
    failTrAdd("A", fail.promise)
    await act(async () => {
      chatChanged()
      await vi.waitFor(() => expect(ctl.pend).toHaveLength(1))
      const q = quotaOnQueue(() => true)
      history.add({ ...entry, source: "A" })
      q.mockRestore()
      await vi.waitFor(() => expect(calls("sync_queue_apply")).toHaveLength(1))
      ctl.pend[0].resolve()
      await ticks()
      sim.remote("c1", msg("r2"))
      chatChanged()
      ctl.auto = true
      await ticks()
      fail.resolve()
      await vi.waitFor(() => expect(sync.syncState().writeFailures).toBe(1))
      await vi.waitFor(() => expect(ids(conversations.get("c1")!.messages)).toEqual(["m1", "a1", "r1", "r2"]), { timeout: 4000 })
    })
    expect(history.list()).toEqual([])
    conversations.appendMessage("c1", { role: "user", content: "next" })
    await vi.waitFor(() => expect(queue().items).toEqual([]))
    expect(applied().filter((x) => x.op.op === "msg_add").at(-1)!.op.parent).toBe("r2")
  })

  it("C08 two memory items, the first applied (moved), the second refused, with a kept load: the stale stash is re-read; cache = Rust (A, r1, no B)", async () => {
    status("native")
    const sim = rustE3({ conversations: [conv("c1", [msg("m1"), msg("a1", "assistant")])], history: [] })
    const { sync, conversations, history } = await load()
    await sync.bootSync()
    sim.remote("c1", msg("r1"))
    const ctl = ctlLoads()
    const realApply = mocks.responders.get("sync_queue_apply")!
    const gateA = deferred<void>()
    const failB = deferred<void>()
    mocks.responders.set("sync_queue_apply", async (a) => {
      const op = a!.op as Op
      const src = op.op === "tr_add" ? (op.tr as { source: string }).source : ""
      if (src === "A") await gateA.promise
      if (src === "B") {
        await failB.promise
        throw { code: "io", message: "disk" }
      }
      return realApply(a)
    })
    await act(async () => {
      chatChanged()
      await vi.waitFor(() => expect(ctl.pend).toHaveLength(1))
      const q = quotaOnQueue(() => true)
      history.add({ ...entry, source: "A" })
      history.add({ ...entry, source: "B" })
      q.mockRestore()
      await vi.waitFor(() => expect(calls("sync_queue_apply")).toHaveLength(1))
      ctl.auto = true
      ctl.pend[0].resolve()
      await ticks()
      gateA.resolve()
      await vi.waitFor(() => expect(calls("sync_queue_apply")).toHaveLength(2))
      expect(sim.changed.at(-1)).toBe(true)
      failB.resolve()
      await vi.waitFor(() => expect(sync.syncState().writeFailures).toBe(1))
      await vi.waitFor(() => expect(ids(conversations.get("c1")!.messages)).toEqual(["m1", "a1", "r1"]))
    })
    await vi.waitFor(() => expect(sources(history.list())).toEqual(["A"]))
    expect(sources([...sim.hist.values()])).toEqual(["A"])
  })
})

describe("G8 confirm F13/F14 store_error after boot is retried with backoff", () => {
  it("C09 load store_error three times after boot (chat-changed): retries at 1/2/4 s, recovers, clears the error; a later failure restarts at 1 s", async () => {
    status("native")
    const sim = rustE3({ conversations: [conv("c1", [msg("m1")])], history: [] })
    const { sync, conversations } = await load()
    await sync.bootSync()
    const realLoad = mocks.responders.get("sync_chat_load")!
    let fails = 3
    mocks.responders.set("sync_chat_load", (a) => {
      if (fails > 0) {
        fails--
        throw storeError
      }
      return realLoad(a)
    })
    const base = calls("sync_chat_load").length
    sim.remote("c1", msg("r1"))
    vi.useFakeTimers({ toFake: ["setTimeout", "clearTimeout"] })
    try {
      chatChanged()
      await vi.advanceTimersByTimeAsync(0)
      expect(sync.syncState().error).toBe("store_error")
      for (const [wait, count] of [
        [999, 1],
        [1, 2],
        [1999, 2],
        [1, 3],
        [3999, 3],
        [1, 4],
      ]) {
        await vi.advanceTimersByTimeAsync(wait)
        expect(calls("sync_chat_load").length - base).toBe(count)
      }
      expect(ids(conversations.get("c1")!.messages)).toEqual(["m1", "r1"])
      expect(sync.syncState().error).toBeNull()
      expect(sync.isNative()).toBe(true)
      expect(localStorage.getItem(NATIVE)).toBe("true")
      // Reset: one more failure retries after 1 s, not 8 s.
      fails = 1
      sim.remote("c1", msg("r2"))
      chatChanged()
      await vi.advanceTimersByTimeAsync(0)
      expect(sync.syncState().error).toBe("store_error")
      await vi.advanceTimersByTimeAsync(999)
      expect(calls("sync_chat_load").length - base).toBe(5)
      await vi.advanceTimersByTimeAsync(1)
      expect(calls("sync_chat_load").length - base).toBe(6)
      expect(ids(conversations.get("c1")!.messages)).toEqual(["m1", "r1", "r2"])
      expect(sync.syncState().error).toBeNull()
      // Nothing owed any more: no further invokes.
      await vi.advanceTimersByTimeAsync(60_000)
      expect(calls("sync_chat_load").length - base).toBe(6)
    } finally {
      vi.useRealTimers()
    }
  })

  it("C10 queue_apply store_error three times after boot: the durable items stay, retries at 1/2/4 s, applied once each, error cleared; a later failure restarts at 1 s", async () => {
    status("native")
    const sim = rustE3({ conversations: [conv("c1", [msg("m1")])], history: [] })
    const { sync, conversations } = await load()
    await sync.bootSync()
    const realApply = mocks.responders.get("sync_queue_apply")!
    let fails = 3
    mocks.responders.set("sync_queue_apply", (a) => {
      if (fails > 0) {
        fails--
        throw storeError
      }
      return realApply(a)
    })
    vi.useFakeTimers({ toFake: ["setTimeout", "clearTimeout"] })
    try {
      const m = conversations.appendMessage("c1", { role: "user", content: "x" }) as Msg
      await vi.advanceTimersByTimeAsync(0)
      expect(sync.syncState().error).toBe("store_error")
      for (const [wait, count] of [
        [999, 1],
        [1, 2],
        [1999, 2],
        [1, 3],
        [3999, 3],
      ]) {
        await vi.advanceTimersByTimeAsync(wait)
        expect(calls("sync_queue_apply")).toHaveLength(count)
        expect(queue().items.map((i) => i.op.op)).toEqual(["msg_add", "set_active"])
      }
      await vi.advanceTimersByTimeAsync(1)
      expect(queue().items).toEqual([])
      expect(sim.path("c1")).toEqual(["m1", m.id])
      expect(sim.results).toEqual(["applied", "applied"])
      expect(sync.syncState().error).toBeNull()
      expect(sync.syncState().writeFailures).toBe(0)
      fails = 1
      const n = calls("sync_queue_apply").length
      const m2 = conversations.appendMessage("c1", { role: "user", content: "y" }) as Msg
      await vi.advanceTimersByTimeAsync(0)
      expect(sync.syncState().error).toBe("store_error")
      await vi.advanceTimersByTimeAsync(999)
      expect(calls("sync_queue_apply")).toHaveLength(n + 1)
      await vi.advanceTimersByTimeAsync(1)
      expect(queue().items).toEqual([])
      expect(sim.path("c1")).toEqual(["m1", m.id, m2.id])
      await vi.advanceTimersByTimeAsync(60_000)
      expect(calls("sync_queue_apply")).toHaveLength(n + 3)
    } finally {
      vi.useRealTimers()
    }
  })

  it("C11 load and drain both fail after boot: one backoff timer drives both to recovery (item applied, remote translation hydrated, error cleared)", async () => {
    status("native")
    const sim = rustE3({ conversations: [conv("c1", [msg("m1")])], history: [] })
    const { sync, conversations, history } = await load()
    await sync.bootSync()
    const realApply = mocks.responders.get("sync_queue_apply")!
    const realLoad = mocks.responders.get("sync_chat_load")!
    let applyFails = 2
    let loadFails = 1
    mocks.responders.set("sync_queue_apply", (a) => {
      if (applyFails > 0) {
        applyFails--
        throw storeError
      }
      return realApply(a)
    })
    mocks.responders.set("sync_chat_load", (a) => {
      if (loadFails > 0) {
        loadFails--
        throw storeError
      }
      return realLoad(a)
    })
    vi.useFakeTimers({ toFake: ["setTimeout", "clearTimeout"] })
    let m!: Msg
    try {
      m = conversations.appendMessage("c1", { role: "user", content: "x" }) as Msg
      await vi.advanceTimersByTimeAsync(0)
      // A remote translation (no chat tree fork in the stand-in).
      sim.hist.set("r1", tr("r1"))
      chatChanged()
      await vi.advanceTimersByTimeAsync(0)
      expect(sync.syncState().error).toBe("store_error")
      await vi.advanceTimersByTimeAsync(31_000)
    } finally {
      vi.useRealTimers()
    }
    expect(queue().items).toEqual([])
    expect(sim.path("c1")).toEqual(["m1", m.id])
    expect(ids(conversations.get("c1")!.messages)).toEqual(["m1", m.id])
    expect(history.list().map((e) => e.id)).toEqual(["r1"])
    expect(sync.syncState().error).toBeNull()
  })

  it("G8 blocker: C12 (F13 residual) a load store_error followed by a stale (superseded) reply resolving ok before the retry fires: hydrate reports the dropped reply as ok, settle clears owed and the error, the retry timer then does nothing; the remote turn stays out of the cache", async () => {
    status("native")
    const sim = rustE3({ conversations: [conv("c1", [msg("m1"), msg("a1", "assistant")])], history: [] })
    const { sync, conversations } = await load()
    await sync.bootSync()
    sim.remote("c1", msg("r1"))
    const ctl = ctlLoads()
    vi.useFakeTimers({ toFake: ["setTimeout", "clearTimeout"] })
    try {
      chatChanged()
      await vi.advanceTimersByTimeAsync(0)
      expect(ctl.pend).toHaveLength(1)
      chatChanged()
      await vi.advanceTimersByTimeAsync(0)
      expect(ctl.pend).toHaveLength(2)
      ctl.auto = true
      ctl.pend[1].reject(storeError)
      await vi.advanceTimersByTimeAsync(0)
      expect(sync.syncState().error).toBe("store_error")
      ctl.pend[0].resolve()
      await vi.advanceTimersByTimeAsync(0)
      const errAfterStale = sync.syncState().error
      const loads = calls("sync_chat_load").length
      await vi.advanceTimersByTimeAsync(31_000)
      const why = `error after the stale reply: ${errAfterStale}; loads before/after 31 s: ${loads}/${calls("sync_chat_load").length}`
      expect(calls("sync_chat_load").length, why).toBeGreaterThan(loads)
    } finally {
      vi.useRealTimers()
    }
    await vi.waitFor(() => expect(ids(conversations.get("c1")!.messages)).toEqual(["m1", "a1", "r1"]))
  })

  it("G8 blocker: C13 (F14 residual) a queue_apply store_error followed by a stale (superseded) load reply resolving ok before the retry fires: settle clears owed and the error, the retry timer does nothing, the durable item is never retried (and the next newer reply is only stashed)", async () => {
    status("native")
    const sim = rustE3({ conversations: [conv("c1", [msg("m1")])], history: [] })
    const { sync, conversations } = await load()
    await sync.bootSync()
    const ctl = ctlLoads()
    const realApply = mocks.responders.get("sync_queue_apply")!
    let fails = 1
    mocks.responders.set("sync_queue_apply", (a) => {
      if (fails > 0) {
        fails--
        throw storeError
      }
      return realApply(a)
    })
    vi.useFakeTimers({ toFake: ["setTimeout", "clearTimeout"] })
    let m!: Msg
    try {
      chatChanged()
      await vi.advanceTimersByTimeAsync(0)
      chatChanged()
      await vi.advanceTimersByTimeAsync(0)
      expect(ctl.pend).toHaveLength(2)
      m = conversations.appendMessage("c1", { role: "user", content: "x" }) as Msg
      await vi.advanceTimersByTimeAsync(0)
      expect(sync.syncState().error).toBe("store_error")
      expect(queue().items).toHaveLength(2)
      ctl.auto = true
      ctl.pend[0].resolve()
      await vi.advanceTimersByTimeAsync(0)
      const errAfterStale = sync.syncState().error
      ctl.pend[1].resolve()
      const applies = calls("sync_queue_apply").length
      await vi.advanceTimersByTimeAsync(31_000)
      const why = `error after the stale reply: ${errAfterStale}; applies before/after 31 s: ${applies}/${calls("sync_queue_apply").length}; queue: ${queue().items.length}`
      expect(calls("sync_queue_apply").length, why).toBeGreaterThan(applies)
    } finally {
      vi.useRealTimers()
    }
    await vi.waitFor(() => expect(sim.path("c1")).toEqual(["m1", m.id]))
    expect(queue().items).toEqual([])
  })

  it("A-C12-1 owed survives a superseded retry reply and a stash applied by settleStash: retries continue until a load succeeds through settle and clears the error", async () => {
    status("native")
    // Only a pinned entry: history.clear() queues tr_clear [], a non-moving apply (changed:false).
    const sim = rustE3({ conversations: [conv("c1", [msg("m1")])], history: [tr("p1", true, 1)] })
    const { sync, history } = await load()
    await sync.bootSync()
    const ctl = ctlLoads()
    const realApply = mocks.responders.get("sync_queue_apply")!
    const applyGate = deferred<void>()
    mocks.responders.set("sync_queue_apply", async (a) => {
      await applyGate.promise
      return realApply(a)
    })
    vi.useFakeTimers({ toFake: ["setTimeout", "clearTimeout"] })
    try {
      // 1. A load failure sets owed and arms the retry timer T (1 s).
      chatChanged()
      await vi.advanceTimersByTimeAsync(0)
      expect(ctl.pend).toHaveLength(1)
      ctl.pend[0].reject(storeError)
      await vi.advanceTimersByTimeAsync(0)
      expect(sync.syncState().error).toBe("store_error")
      // 2. T fires: the retry settle R issues load r.
      await vi.advanceTimersByTimeAsync(1000)
      expect(ctl.pend).toHaveLength(2)
      // 3. chat-changed supersedes r; refresh N issues load n.
      chatChanged()
      await vi.advanceTimersByTimeAsync(0)
      expect(ctl.pend).toHaveLength(3)
      // 4. A write is queued while n is in flight; its apply is held, so n is stashed.
      history.clear()
      await vi.advanceTimersByTimeAsync(0)
      expect(queue().items.map((i) => i.op)).toEqual([{ op: "tr_clear", observedIds: [] }])
      ctl.pend[2].resolve()
      await vi.advanceTimersByTimeAsync(0)
      // 5. The apply succeeds without moving the projection; drain's settleStash applies n.
      applyGate.resolve()
      await vi.advanceTimersByTimeAsync(0)
      expect(queue().items).toEqual([])
      expect(sim.changed).toEqual([false])
      // 6. R's reply lands superseded (kept).
      ctl.auto = true
      ctl.pend[1].resolve()
      await vi.advanceTimersByTimeAsync(0)
      const errAfterStale = sync.syncState().error
      const loads = calls("sync_chat_load").length
      await vi.advanceTimersByTimeAsync(31_000)
      const why = `error after the stale reply: ${errAfterStale}; loads before/after 31 s: ${loads}/${calls("sync_chat_load").length}`
      expect(calls("sync_chat_load").length, why).toBeGreaterThan(loads)
      expect(sync.syncState().error, why).toBeNull()
    } finally {
      vi.useRealTimers()
    }
  })
})
