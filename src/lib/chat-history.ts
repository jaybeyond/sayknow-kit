import type { ChatImage } from "./chat-image"
import { storage } from "./storage"
import {
  cachedConversations,
  commitNative,
  enqueue,
  isNative,
  setCachedConversations,
  whenWritable,
  wsMsg,
  type QueuedOp,
} from "./sync-client"

export type ChatRole = "user" | "assistant"

export type ChatMsg = {
  id: string
  role: ChatRole
  content: string
  ts: number
  /** Model id that produced this assistant turn (assistant only). */
  model?: string
  /** Attached images (user turns). Stored downscaled; see chat-image.ts. */
  images?: ChatImage[]
}

export type Conversation = {
  id: string
  title: string
  messages: ChatMsg[]
  createdAt: number
  updatedAt: number
}

const CONVS_KEY = "conversations"
const CURRENT_KEY = "current-conversation"
const LEGACY_HISTORY_KEY = "chat-history"

const MAX_CONVS = 100
const MAX_MSGS_PER_CONV = 200

function genId(): string {
  return crypto.randomUUID()
}

function genTitle(messages: ChatMsg[]): string {
  const firstUser = messages.find((m) => m.role === "user")
  const first = firstUser?.content?.trim() ?? ""
  if (!first) return firstUser?.images?.length ? "이미지" : "새 대화"
  return first.length > 32 ? first.slice(0, 32).trimEnd() + "…" : first
}

function migrateLegacy(): void {
  const oldMsgs = storage.get<ChatMsg[]>(LEGACY_HISTORY_KEY)
  const existingConvs = storage.get<Conversation[]>(CONVS_KEY)
  if (!oldMsgs || !oldMsgs.length || (existingConvs && existingConvs.length))
    return
  const now = Date.now()
  const conv: Conversation = {
    id: genId(),
    title: genTitle(oldMsgs),
    messages: oldMsgs.slice(-MAX_MSGS_PER_CONV),
    createdAt: oldMsgs[0]?.ts ?? now,
    updatedAt: oldMsgs[oldMsgs.length - 1]?.ts ?? now,
  }
  storage.set(CONVS_KEY, [conv])
  storage.set(CURRENT_KEY, conv.id)
  storage.remove(LEGACY_HISTORY_KEY)
}

migrateLegacy()

/** The stored list: the native cache under `native`, else localStorage. */
function readAll(): Conversation[] {
  if (isNative()) return cachedConversations()
  return storage.get<Conversation[]>(CONVS_KEY) ?? []
}

/**
 * Under `native` the trims are display-only: the cache keeps everything
 * and nothing is ever deleted because of them.
 */
function display(conv: Conversation): Conversation {
  if (!isNative() || conv.messages.length <= MAX_MSGS_PER_CONV) return conv
  return { ...conv, messages: conv.messages.slice(-MAX_MSGS_PER_CONV) }
}

/**
 * Persist `list` and queue `ops`. Under `native` the cache is updated only
 * once the ops are durable (`commitNative`, E2); otherwise localStorage is
 * written exactly as before and the ops are queued only during write-hold
 * (`enqueue` is a no-op with the flag off). Quota eviction in `writeAll`
 * never queues anything.
 */
function commit(list: Conversation[], ops: QueuedOp[]) {
  if (isNative()) {
    commitNative(ops, () => setCachedConversations(list))
    return
  }
  writeAll(list)
  enqueue(...ops)
}

/**
 * The ops that turn the active path `prev` into `next`: every new message
 * id becomes `msg_add` with the preceding message as parent, then
 * `set_active` points at the new last id. A pure truncation only moves the
 * active leaf (siblings stay in the native tree, so regenerate is safe). An
 * empty `next` has no leaf to activate and emits nothing.
 */
export function diffMessages(convId: string, prev: ChatMsg[], next: ChatMsg[]): QueuedOp[] {
  const ops: QueuedOp[] = []
  const known = new Set(prev.map((m) => m.id))
  next.forEach((m, i) => {
    if (known.has(m.id)) return
    ops.push({ op: "msg_add", conv: convId, msg: wsMsg(m), parent: i > 0 ? next[i - 1].id : null })
  })
  const leaf = next[next.length - 1]?.id
  if (leaf && (ops.length > 0 || leaf !== prev[prev.length - 1]?.id)) {
    ops.push({ op: "set_active", conv: convId, leaf })
  }
  return ops
}

/**
 * E4: `next` is relative to the displayed window of the full native path
 * `prev`; the hidden prefix before the window is kept. A list starting at a
 * known message keeps everything before it; an empty or new-first list
 * keeps exactly the hidden prefix, so a truncation lands on the message
 * before the window and the first new message is parented to it. Without a
 * hidden prefix an empty list stays empty (the documented deviation).
 */
function withHiddenPrefix(prev: ChatMsg[], next: ChatMsg[]): ChatMsg[] {
  const off = Math.max(0, prev.length - MAX_MSGS_PER_CONV)
  const at = next.length ? prev.findIndex((m) => m.id === next[0].id) : -1
  const keep = at >= 0 ? at : off
  return keep > 0 ? [...prev.slice(0, keep), ...next] : next
}

/**
 * Persist, shedding image payloads oldest-first if the store overflows.
 *
 * A dropped image keeps its slot with `data: ""` so the bubble can still say
 * "image no longer stored" instead of the message quietly changing shape. The
 * text of every conversation always survives; only pixels are sacrificed.
 */
function writeAll(list: Conversation[]) {
  const trimmed = list.slice(0, MAX_CONVS)
  if (storage.set(CONVS_KEY, trimmed)) return

  // Oldest conversation first, oldest message first, until it fits.
  const shed = trimmed.map((c) => ({ ...c, messages: c.messages.map((m) => ({ ...m })) }))
  const order = [...shed].sort((a, b) => a.updatedAt - b.updatedAt)
  for (const conv of order) {
    for (const msg of conv.messages) {
      if (!msg.images?.some((img) => img.data)) continue
      msg.images = msg.images.map((img) => ({ ...img, data: "" }))
      if (storage.set(CONVS_KEY, shed)) return
    }
  }
  // Still too big with no images at all: fall back to dropping conversations.
  for (let keep = shed.length - 1; keep > 0; keep--) {
    if (storage.set(CONVS_KEY, shed.slice(0, keep))) return
  }
}

function appendNow(id: string, next: ChatMsg) {
  const all = [...readAll()]
  const idx = all.findIndex((c) => c.id === id)
  if (idx < 0) throw new Error("conversation not found: " + id)
  const conv = all[idx]
  const full = [...conv.messages, next]
  const messages = isNative() ? full : full.slice(-MAX_MSGS_PER_CONV)
  const isFirstUser = next.role === "user" && !conv.messages.some((m) => m.role === "user")
  const title = isFirstUser ? genTitle(messages) : conv.title
  all[idx] = {
    ...conv,
    messages,
    title,
    updatedAt: next.ts,
  }
  const ops = diffMessages(id, conv.messages, full)
  if (title !== conv.title) ops.push({ op: "rename", conv: id, title })
  commit(all, ops)
}

export const conversations = {
  list(): Conversation[] {
    // Most recently updated first.
    const sorted = [...readAll()].sort((a, b) => b.updatedAt - a.updatedAt)
    return isNative() ? sorted.slice(0, MAX_CONVS).map(display) : sorted
  },
  get(id: string): Conversation | null {
    const found = readAll().find((c) => c.id === id) ?? null
    return found && display(found)
  },
  currentId(): string | null {
    return storage.get<string>(CURRENT_KEY)
  },
  setCurrent(id: string | null) {
    if (id) storage.set(CURRENT_KEY, id)
    else storage.remove(CURRENT_KEY)
  },
  create(): Conversation {
    const now = Date.now()
    const conv: Conversation = {
      id: genId(),
      title: "새 대화",
      messages: [],
      createdAt: now,
      updatedAt: now,
    }
    // E5: on a native boot the write is held until the cache is hydrated.
    whenWritable(() => {
      commit([conv, ...readAll()], [{ op: "conv_create", conv: conv.id, title: conv.title }])
      storage.set(CURRENT_KEY, conv.id)
    })
    return conv
  },
  delete(id: string) {
    whenWritable(() => {
      const all = readAll().filter((c) => c.id !== id)
      commit(all, [{ op: "conv_delete", conv: id }])
      if (storage.get<string>(CURRENT_KEY) === id) {
        const next = all[0]?.id ?? null
        conversations.setCurrent(next)
      }
    })
  },
  rename(id: string, title: string) {
    whenWritable(() => {
      const all = [...readAll()]
      const idx = all.findIndex((c) => c.id === id)
      if (idx < 0) return
      all[idx] = { ...all[idx], title, updatedAt: Date.now() }
      commit(all, [{ op: "rename", conv: id, title }])
    })
  },
  /**
   * Append a message to the conversation. Auto-generates title if first user
   * msg. Held until hydrate on a native boot (E5); the returned message is
   * the one appended then, parented to the hydrated last message.
   */
  appendMessage(id: string, msg: Omit<ChatMsg, "id" | "ts">): ChatMsg {
    const next: ChatMsg = { ...msg, id: genId(), ts: Date.now() }
    whenWritable(() => appendNow(id, next))
    return next
  },
  /** Replace the entire message list (used by truncate / edit / regenerate). */
  setMessages(id: string, messages: ChatMsg[]) {
    whenWritable(() => {
      const all = [...readAll()]
      const idx = all.findIndex((c) => c.id === id)
      if (idx < 0) return
      const prev = all[idx].messages
      // E4: under native `messages` is relative to the display window.
      const full = isNative() ? withHiddenPrefix(prev, messages) : messages
      all[idx] = {
        ...all[idx],
        messages: isNative() ? full : messages.slice(-MAX_MSGS_PER_CONV),
        updatedAt: Date.now(),
      }
      commit(all, diffMessages(id, prev, full))
    })
  },
}

export function timeAgo(ts: number): string {
  const diff = Date.now() - ts
  const s = Math.floor(diff / 1000)
  if (s < 60) return "방금"
  const m = Math.floor(s / 60)
  if (m < 60) return `${m}분 전`
  const h = Math.floor(m / 60)
  if (h < 24) return `${h}시간 전`
  const d = Math.floor(h / 24)
  if (d < 7) return `${d}일 전`
  return new Date(ts).toLocaleDateString()
}
