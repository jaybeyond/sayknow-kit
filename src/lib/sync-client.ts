/**
 * Web side of the conversation / translation-history storage cutover (G8,
 * plan §4.10.1 M2–M9, review decisions E1–E5).
 *
 * - Flag off on a device that never migrated: nothing but one
 *   `sync_get_status`; `chat-history.ts` and `history.ts` keep writing
 *   localStorage exactly as before.
 * - Write-hold (M3, from the push until `native`): every conversation and
 *   translation write still lands in localStorage, and is also appended to
 *   the durable op queue (`sayknow:sync-migration-queue`).
 * - Native (E1: whatever the flag says once `migration` is `native`): reads
 *   come from an in-memory cache hydrated by `sync_chat_load`; writes go
 *   through the queue, which is flushed with `sync_queue_apply` (exactly
 *   once by qseq and op hash on the Rust side). The localStorage originals
 *   are never deleted (M9).
 * - E2: under native a write is reported only once it is durable: the queue
 *   key was written, or a direct `sync_queue_apply` is in flight and the
 *   cache change is rolled back if it fails.
 * - E5: a native boot starts from an empty cache; writes are held in memory
 *   until the first hydrate lands and are then applied against it.
 *
 * The queue ops mirror Rust `migration::QueuedOp`: tag `op` in snake_case,
 * fields in camelCase, entities addressed by their source ids.
 */
import { invoke } from "@tauri-apps/api/core"
import { listen } from "@tauri-apps/api/event"
import type { ChatMsg, ChatRole, Conversation } from "./chat-history"
import type { HistoryEntry } from "./history"
import type { LangCode } from "./openrouter"
import { isTauri } from "./runtime"
import { storage } from "./storage"

// ---------------------------------------------------------------------------
// Wire shapes (Rust `WebStore`, `WsConv`, `WsMsg`, `WsImage`, `WsTr`)

export type WsImage = {
  id: string
  mimeType: string
  data: string
  width: number
  height: number
  name?: string
}

export type WsMsg = {
  id: string
  role: string
  content: string
  ts: number
  model?: string
  images: WsImage[]
}

export type WsConv = {
  id: string
  title: string
  messages: WsMsg[]
  /** Only on `sync_chat_load` output; never part of a push. */
  createdAt?: number
  updatedAt?: number
}

export type WsTr = {
  id: string
  ts: number
  source: string
  target: string
  from: string
  to: string
  model: string
  pinned: boolean
}

export type WebStore = { conversations: WsConv[]; history: WsTr[] }

export type QueuedOp =
  | { op: "msg_add"; conv: string; msg: WsMsg; parent: string | null }
  | { op: "conv_create"; conv: string; title: string }
  | { op: "rename"; conv: string; title: string }
  | { op: "conv_delete"; conv: string }
  | { op: "set_active"; conv: string; leaf: string }
  | { op: "tr_add"; tr: WsTr }
  | { op: "tr_pin"; tr: string; pinned: boolean }
  | { op: "tr_delete"; tr: string }
  | { op: "tr_clear"; observedIds: string[] }

export type QueueItem = { qseq: number; op: QueuedOp }
export type QueueFile = { nextQseq: number; items: QueueItem[] }

type FinalizeOut =
  | { result: "need_snapshot" }
  | { result: "native"; ack_qseq: number }
  | { result: "verify_failed"; attempts: number; mismatches: number }
  | { result: "deferred"; attempts: number; mismatches: number }

type StatusOut = {
  enabled?: boolean
  vault?: unknown
  migration?: string
  finalize_this_boot?: boolean
  queue_flushed_qseq?: number | null
  ack_qseq?: number | null
  /** E1a: `sync.db` could not be read; the other fields are defaults. */
  store_error?: boolean
}

/** E3: `sync_queue_apply` result (Rust `QueueApplyOut`). */
type QueueApplyOut =
  | { result: "applied"; changed?: boolean }
  | { result: "duplicate" }
  | { result: "stale_qseq"; watermark: number }

// ---------------------------------------------------------------------------
// Keys and limits

/** Storage keys, without the `sayknow:` prefix `storage` adds. */
export const QUEUE_KEY = "sync-migration-queue"
/** E3: side key for unparsable queue bytes (`<QUEUE_KEY>.corrupt-<ts>`). */
const CORRUPT_PREFIX = QUEUE_KEY + ".corrupt-"
/** Device-local marker: the last boot reached `native`. */
export const NATIVE_KEY = "sync-native"
const CONVS_KEY = "conversations"
const HISTORY_KEY = "history"

/** UTF-16 units per import chunk, well under the IPC payload cap. */
let chunkUnits = 256 * 1024
/** M6: finalize rounds per boot (Rust defers from its 3rd failed attempt). */
const MAX_FINALIZE_ROUNDS = 3
/** E3: renumber rounds per drain before giving up on `stale_qseq`. */
const MAX_STALE_ROUNDS = 3
/** E5: hydrate / status retry backoff. */
const RETRY_BASE_MS = 1000
const RETRY_MAX_MS = 30_000
/** E5: `main.tsx` waits at most this long for the boot before rendering. */
export const BOOT_WAIT_MS = 3000

// ---------------------------------------------------------------------------
// Mode, cache, subscribers

export type SyncMode = "off" | "hold" | "native"

export type SyncState = {
  mode: SyncMode
  /** `sync_get_status().migration`, once known. */
  migration: string | null
  /** The last finalize result (`deferred`, `verify_failed`, ...). */
  finalize: string | null
  /** The last error code, if a step failed (surfaced to the G9 UI). */
  error: string | null
  /** E3: the side key holding unparsable queue bytes, once preserved. */
  corruptQueue: string | null
  /** E2: writes refused (rolled back) because they never became durable. */
  writeFailures: number
}

type Cache = { conversations: Conversation[]; history: HistoryEntry[] }

/**
 * An op that is not in the queue key (E2): a quota-failed queue write.
 * `native` items were applied to the cache optimistically and carry the
 * rollback; `hold` items are also in the localStorage original.
 */
type MemItem = QueueItem & { origin: "native" | "hold"; rollback?: () => void }

let mode: SyncMode = "off"
let cache: Cache | null = null
let hydrated = false
/** E5: writes run now; otherwise they are held until hydrate (or off). */
let ready = true
let state: SyncState = { mode, migration: null, finalize: null, error: null, corruptQueue: null, writeFailures: 0 }
const listeners = new Set<() => void>()
const held: (() => void)[] = []
let memItems: MemItem[] = []
/** E3: the lowest qseq a new op may take (status watermark, allocations). */
let qseqFloor = 1
/** The queue key holds bytes that could not be preserved: never rewrite it. */
let queueBlocked = false
/**
 * E5/E5b: hydrate generations. A reply at or below `appliedGen` is dropped;
 * only a `sync:chat-changed` event or an apply that moved the projection
 * raises it past the loads already in flight.
 */
let loadGen = 0
let appliedGen = 0
/**
 * E5b: a newer load skipped while writes were pending, applied once nothing
 * is pending. `moved`: an apply moved the projection after it was kept, so
 * it is stale and re-read instead (F09).
 */
let stash: { gen: number; ws: WebStore; moved?: boolean } | null = null
/**
 * E5a: the device has no native marker and the first status has not
 * answered yet. Writes then take the localStorage path and are also kept
 * here, to be replayed against the hydrate if the status says `native`.
 */
let awaitingStatus = false
const preStatus: (() => void)[] = []
let booting: Promise<void> | null = null
let flushing: Promise<boolean> | null = null
let unlisten: (() => void) | null = null
let statusKnown = false
let retryTimer: ReturnType<typeof setTimeout> | null = null
let retryDelay = RETRY_BASE_MS
let busy = 0
/** E1a (F13/F14): a settle or flush failed after boot; a backoff retry is owed. */
let owed = false

/**
 * A module instance superseded by a reload (HMR, tests) must not keep
 * retrying: its timers check this token first.
 */
const INSTANCE = Symbol("sync-client")
const instanceSlot = globalThis as { __sayknowSyncClient?: symbol }
instanceSlot.__sayknowSyncClient = INSTANCE
const current = () => instanceSlot.__sayknowSyncClient === INSTANCE

function setState(patch: Partial<SyncState>) {
  state = { ...state, ...patch, mode }
}

function notify() {
  for (const fn of [...listeners]) fn()
}

export function subscribe(fn: () => void): () => void {
  listeners.add(fn)
  return () => {
    listeners.delete(fn)
  }
}

export function syncState(): SyncState {
  return state
}

/** Reads come from the native cache. */
export function isNative(): boolean {
  return mode === "native"
}

/** Writes are queued (write-hold or native). */
export function isQueuing(): boolean {
  return mode !== "off"
}

/** Whether the cache holds a native load. */
export function isHydrated(): boolean {
  return hydrated
}

/** E5: writes are applied now (not held until the first hydrate). */
export function isReady(): boolean {
  return ready
}

/**
 * E5: run `fn` now, or hold it until the native cache is hydrated (or the
 * device turns out not to be native). Held writes are applied in order
 * against the state they then see, never against the frozen originals.
 *
 * E5a: before the first status answers on a device without the native
 * marker, `fn` runs on the localStorage path and is also kept; a status
 * that says `native` replays it against the hydrated cache.
 */
export function whenWritable(fn: () => void): void {
  if (ready) {
    fn()
    if (awaitingStatus && mode === "off") preStatus.push(fn)
    return
  }
  held.push(fn)
  kick()
}

/** Resolves once writes are applied directly (see `whenWritable`). */
export function whenReady(): Promise<void> {
  return ready ? Promise.resolve() : new Promise((resolve) => whenWritable(resolve))
}

function releaseHeld() {
  ready = true
  const fns = held.splice(0)
  for (const fn of fns) {
    try {
      fn()
    } catch {
      // The caller already returned; the target vanished meanwhile.
      setState({ error: "write_failed", writeFailures: state.writeFailures + 1 })
    }
  }
  if (fns.length) notify()
}

function fromStorage(): Cache {
  const convs = storage.get<Conversation[]>(CONVS_KEY)
  const hist = storage.get<HistoryEntry[]>(HISTORY_KEY)
  return {
    conversations: Array.isArray(convs) ? convs : [],
    history: Array.isArray(hist) ? hist : [],
  }
}

/**
 * The native cache. At the hold→native cutover it is seeded from the
 * dual-written originals (current up to there); on a native boot it starts
 * empty until `sync_chat_load` lands (E5), never from the frozen originals.
 */
function ensureCache(): Cache {
  if (!cache) cache = mode === "native" ? { conversations: [], history: [] } : fromStorage()
  return cache
}

export function cachedConversations(): Conversation[] {
  return ensureCache().conversations
}

export function setCachedConversations(list: Conversation[]) {
  ensureCache().conversations = list
  notify()
}

export function cachedHistory(): HistoryEntry[] {
  return ensureCache().history
}

export function setCachedHistory(list: HistoryEntry[]) {
  ensureCache().history = list
  notify()
}

// ---------------------------------------------------------------------------
// Queue

function isItem(i: unknown): i is QueueItem {
  if (!i || typeof i !== "object") return false
  const { qseq, op } = i as { qseq?: unknown; op?: unknown }
  return Number.isSafeInteger(qseq) && (qseq as number) > 0 && !!op && typeof op === "object"
}

/**
 * E3: keep bytes that would otherwise be dropped under a side key and
 * surface it. The queue key is rewritten with the salvaged items only once
 * the bytes are safe; if they cannot be preserved, it is never rewritten.
 */
function quarantine(raw: string, salvaged: QueueFile) {
  if (state.corruptQueue) {
    queueBlocked = true
    return
  }
  const key = CORRUPT_PREFIX + Date.now()
  if (storage.set(key, raw) && storage.set(QUEUE_KEY, salvaged)) {
    setState({ corruptQueue: key, error: "queue_corrupt" })
    return
  }
  queueBlocked = true
  setState({ error: "queue_corrupt" })
}

function readQueueRaw(): QueueFile {
  const raw = storage.get<unknown>(QUEUE_KEY)
  if (raw === null) return { nextQseq: 1, items: [] }
  if (!raw || typeof raw !== "object" || !Array.isArray((raw as QueueFile).items)) {
    const empty = { nextQseq: 1, items: [] }
    quarantine(typeof raw === "string" ? raw : JSON.stringify(raw), empty)
    return empty
  }
  const file = raw as { nextQseq?: unknown; items: unknown[] }
  const items = file.items.filter(isItem)
  const maxSeen = items.reduce((m, i) => Math.max(m, i.qseq), 0)
  const n = file.nextQseq
  const next = Number.isSafeInteger(n) && (n as number) > maxSeen ? (n as number) : maxSeen + 1
  const out = { nextQseq: next, items }
  if (items.length !== file.items.length) quarantine(JSON.stringify(raw), out)
  return out
}

export function readQueue(): QueueFile {
  return readQueueRaw()
}

function initialMode(): SyncMode {
  if (storage.get<boolean>(NATIVE_KEY) === true) {
    // E5: hold writes until the status says whether this is still native.
    ready = false
    return "native"
  }
  // A queue left by an earlier boot means a push already started: keep
  // holding so a write before the status arrives is not missed.
  return readQueueRaw().items.length > 0 ? "hold" : "off"
}

mode = initialMode()
setState({})

function stripImages(op: QueuedOp): QueuedOp {
  if (op.op !== "msg_add" || !op.msg.images.some((img) => img.data)) return op
  // Quota shedding: the attachment survives as evicted metadata, exactly
  // like `writeAll` does for the originals. Eviction is never a delete.
  return { ...op, msg: { ...op.msg, images: op.msg.images.map((img) => ({ ...img, data: "" })) } }
}

/**
 * Give `ops` qseqs and persist them. Returns the items that only live in
 * memory (the queue write failed, E2); they are applied directly by the
 * next drain without image shedding (Rust has no quota).
 *
 * E2a: images are shed for a retry only under write-hold, where the
 * localStorage original keeps the bytes. Under native a failed queue write
 * goes straight to the direct apply with full images.
 */
function append(ops: QueuedOp[]): MemItem[] {
  const q = readQueueRaw()
  let next = Math.max(q.nextQseq, qseqFloor)
  const added: QueueItem[] = ops.map((op) => ({ qseq: next++, op }))
  qseqFloor = next
  if (!queueBlocked) {
    const items = [...q.items, ...added]
    if (storage.set(QUEUE_KEY, { nextQseq: next, items })) return []
    if (mode === "hold") {
      const shed = [...q.items, ...added.map((i) => ({ ...i, op: stripImages(i.op) }))]
      if (storage.set(QUEUE_KEY, { nextQseq: next, items: shed })) return []
    }
  }
  setState({ error: "queue_write_failed" })
  const origin = mode === "native" ? "native" : "hold"
  const mem = added.map((i) => ({ ...i, origin }) as MemItem)
  memItems.push(...mem)
  return mem
}

/**
 * Append `ops` to the durable queue (no-op when the flag is off). Returns
 * false when the queue key could not be written: under write-hold the
 * localStorage original still holds the change and the op is applied
 * directly at the cutover (E2.3). Under `native` a flush is scheduled.
 */
export function enqueue(...ops: QueuedOp[]): boolean {
  if (mode === "off" || ops.length === 0) return true
  const mem = append(ops)
  if (mode === "native") void flush()
  return mem.length === 0
}

/**
 * E2: a native write. `apply` updates the cache only after the ops are
 * durable (queue key written) or, when the queue write fails, handed to a
 * direct `sync_queue_apply`; if that fails too the cache change is rolled
 * back and `writeFailures` grows, so the UI never keeps a write that is
 * not stored anywhere.
 */
export function commitNative(ops: QueuedOp[], apply: () => void) {
  if (mode !== "native") throw new Error("commitNative outside native")
  if (ops.length === 0) {
    apply()
    return
  }
  const c = ensureCache()
  const before = { conversations: c.conversations, history: c.history }
  const mem = append(ops)
  apply()
  // E5b: a write alone never drops an in-flight load. A load that returns
  // while the write is pending is kept and applied once the queue drains,
  // unless the write's apply moved the projection (`changed`).
  if (mem.length) {
    const after = { conversations: c.conversations, history: c.history }
    const rollback = () => {
      if (cache !== c) return
      if (c.conversations === after.conversations) c.conversations = before.conversations
      if (c.history === after.history) c.history = before.history
    }
    for (const m of mem) m.rollback = rollback
  }
  void flush()
}

function errorCode(e: unknown): string {
  if (e && typeof e === "object" && "code" in e && typeof (e as { code: unknown }).code === "string") {
    return (e as { code: string }).code
  }
  return "error"
}

/** `stale_qseq` watermark, or null for applied / duplicate (and legacy `bool`). */
function staleWatermark(out: unknown): number | null {
  if (!out || typeof out !== "object") return null
  const o = out as Partial<QueueApplyOut> & { watermark?: unknown }
  return o.result === "stale_qseq" && Number.isSafeInteger(o.watermark) ? (o.watermark as number) : null
}

/**
 * E5b: whether an apply may have moved the projection. `applied` reports
 * `changed` (Rust always sends it; a reply without it is taken as moved);
 * a `duplicate` was signalled by its first apply; a legacy `true` is moved.
 */
function movedProjection(out: unknown): boolean {
  if (out === true) return true
  if (!out || typeof out !== "object") return false
  const o = out as { result?: unknown; changed?: unknown }
  return o.result === "applied" && o.changed !== false
}

/** E2: drop every native-only item and undo its cache change. */
function failMemory() {
  const dropped = memItems.filter((m) => m.origin === "native")
  if (!dropped.length) return
  memItems = memItems.filter((m) => m.origin !== "native")
  const seen = new Set<() => void>()
  for (const m of [...dropped].reverse()) {
    if (m.rollback && !seen.has(m.rollback)) {
      seen.add(m.rollback)
      m.rollback()
    }
  }
  setState({ writeFailures: state.writeFailures + 1 })
  notify()
}

/**
 * E3: Rust has applied up to `watermark` and does not know the op at
 * `from`. Renumber it and every later item (queue key and memory) from
 * watermark+1, keeping their order, and persist the result.
 */
function renumber(from: number, watermark: number): boolean {
  const q = readQueueRaw()
  const tail = [
    ...q.items.filter((i) => i.qseq >= from).map((i) => ({ item: i, mem: null as MemItem | null })),
    ...memItems.filter((m) => m.qseq >= from).map((m) => ({ item: m as QueueItem, mem: m })),
  ].sort((a, b) => a.item.qseq - b.item.qseq)
  let next = watermark + 1
  const renamed = new Map<QueueItem, number>()
  for (const t of tail) renamed.set(t.item, next++)
  const fileItems = q.items.map((i) => (renamed.has(i) ? { ...i, qseq: renamed.get(i)! } : i))
  const nextQseq = Math.max(q.nextQseq, next)
  if (queueBlocked && fileItems.some((i, k) => i !== q.items[k])) return false
  if (!queueBlocked && !storage.set(QUEUE_KEY, { nextQseq, items: fileItems })) return false
  for (const t of tail) if (t.mem) t.mem.qseq = renamed.get(t.item)!
  qseqFloor = Math.max(qseqFloor, nextQseq)
  return true
}

/**
 * M8: flush the queue (and the memory items) in qseq order. Each item is
 * removed only after its `sync_queue_apply` succeeded; the first error
 * stops the flush, which is retried with backoff while items remain (E1a),
 * and on the next boot, write or `sync:chat-changed`.
 * Resolves `true` once both are empty.
 */
export function flush(): Promise<boolean> {
  if (mode !== "native") return Promise.resolve(false)
  if (!flushing) flushing = drain()
  return flushing
}

function pending(): boolean {
  return readQueueRaw().items.length > 0 || memItems.length > 0
}

async function drain(): Promise<boolean> {
  await null
  let ok = false
  try {
    ok = await drainLoop()
    return ok
  } finally {
    flushing = null
    // Native items behind a stuck head are not durable: refuse them (E2).
    if (!ok && memItems.some((m) => m.origin === "native")) failMemory()
    // F09: whatever the outcome, a kept load is settled once nothing is
    // pending; while durable items remain the optimistic cache stays.
    settleStash()
    // F14: durable items left behind a failure are retried with backoff.
    if (!ok && mode === "native" && pending()) {
      owed = true
      scheduleRetry()
    }
  }
}

async function drainLoop(): Promise<boolean> {
  let staleRounds = 0
  for (;;) {
    if (mode !== "native") return false
    const fileHead = readQueueRaw().items[0]
    const memHead = memItems.reduce<MemItem | undefined>((m, i) => (!m || i.qseq < m.qseq ? i : m), undefined)
    const head: QueueItem | undefined =
      fileHead && (!memHead || fileHead.qseq < memHead.qseq) ? fileHead : memHead
    if (!head) return true
    let out: unknown
    try {
      out = await invoke<QueueApplyOut>("sync_queue_apply", { qseq: head.qseq, op: head.op })
    } catch (e) {
      const code = errorCode(e)
      setState({ error: code })
      // `migration_state` (WrongState): not native on the Rust side; the
      // item stays queued.
      if (code === "disabled" || code === "not_native" || code === "migration_state") leaveNative()
      return false
    }
    // E5b: an apply that moved the projection supersedes loads in flight.
    if (movedProjection(out)) {
      appliedGen = loadGen
      if (stash) stash.moved = true
    }
    const watermark = staleWatermark(out)
    if (watermark !== null) {
      if (++staleRounds > MAX_STALE_ROUNDS || !renumber(head.qseq, watermark)) {
        setState({ error: "stale_qseq" })
        return false
      }
      continue
    }
    if (head === memHead) {
      memItems = memItems.filter((m) => m !== memHead)
      continue
    }
    if (queueBlocked) return false
    const q = readQueueRaw()
    const rest = q.items.filter((i) => i.qseq !== head.qseq)
    // Re-sent after a failed removal: Rust answers `duplicate` for the same
    // qseq and op, so a replay is never applied twice.
    if (!storage.set(QUEUE_KEY, { nextQseq: Math.max(q.nextQseq, qseqFloor), items: rest })) return false
  }
}

// ---------------------------------------------------------------------------
// Snapshot (M2/M6)

function str(v: unknown): string {
  return typeof v === "string" ? v : typeof v === "number" ? String(v) : ""
}

function num(v: unknown): number {
  return typeof v === "number" && Number.isFinite(v) && v > 0 ? Math.floor(v) : 0
}

export function wsImage(img: Partial<WsImage>): WsImage {
  const out: WsImage = {
    id: str(img.id),
    mimeType: str(img.mimeType),
    data: str(img.data),
    width: num(img.width),
    height: num(img.height),
  }
  if (typeof img.name === "string") out.name = img.name
  return out
}

/** A message in Rust `WsMsg` field order (`id, role, content, ts, model?, images`). */
export function wsMsg(m: Partial<ChatMsg>): WsMsg {
  const images = Array.isArray(m.images) ? m.images.map((img) => wsImage(img ?? {})) : []
  const head = { id: str(m.id), role: str(m.role), content: str(m.content), ts: num(m.ts) }
  return typeof m.model === "string" ? { ...head, model: m.model, images } : { ...head, images }
}

export function wsTr(e: Partial<HistoryEntry>): WsTr {
  return {
    id: str(e.id),
    ts: num(e.ts),
    source: str(e.source),
    target: str(e.target),
    from: str(e.from),
    to: str(e.to),
    model: str(e.model),
    pinned: e.pinned === true,
  }
}

/**
 * The canonical snapshot text: the localStorage `conversations` and
 * `history` in Rust serde field order (`WebStore { conversations, history }`).
 * Rust parses exactly these bytes, so serializing them back is identical and
 * the hash is deterministic for the same stored data.
 */
export function snapshotText(): string {
  const { conversations, history } = fromStorage()
  const ws: WebStore = {
    conversations: conversations
      .filter((c) => c && str(c.id))
      .map((c) => ({
        id: str(c.id),
        title: str(c.title),
        messages: (Array.isArray(c.messages) ? c.messages : []).filter((m) => m && str(m.id)).map(wsMsg),
      })),
    history: history.filter((e) => e && str(e.id)).map(wsTr),
  }
  return JSON.stringify(ws)
}

/** Lowercase hex SHA-256 of the UTF-8 bytes of `text`. */
export async function snapshotHash(text: string): Promise<string> {
  const digest = await crypto.subtle.digest("SHA-256", new TextEncoder().encode(text))
  return Array.from(new Uint8Array(digest), (b) => b.toString(16).padStart(2, "0")).join("")
}

/** Split without cutting a surrogate pair (each chunk stays valid UTF-8). */
export function chunkText(text: string, units = chunkUnits): string[] {
  if (!text) return [""]
  const out: string[] = []
  let at = 0
  while (at < text.length) {
    let end = Math.min(at + units, text.length)
    const last = text.charCodeAt(end - 1)
    if (end < text.length && last >= 0xd800 && last < 0xdc00 && end - 1 > at) end -= 1
    out.push(text.slice(at, end))
    at = end
  }
  return out
}


/**
 * Load the native projection into the cache. Every load carries a
 * generation; a reply at or below `appliedGen` is dropped (E5). A reply that
 * lands while ops are pending is not applied over the optimistic cache but
 * kept (E5b) and applied once nothing is pending, unless an apply that moved
 * the projection or a newer load supersedes it.
 *
 * `ok`: this reply was applied. `kept`: no error, but this reply was not
 * applied (stashed, or superseded by a newer load: BF1/C12, so a boot keeps
 * holding and a pending retry or owed flush is not cleared by a stale reply;
 * the newer load's own settle resolves them). `failed`: the load failed and
 * was surfaced.
 */
async function hydrate(): Promise<"ok" | "kept" | "failed"> {
  const gen = ++loadGen
  let ws: WebStore
  try {
    ws = await invoke<WebStore>("sync_chat_load")
  } catch (e) {
    const code = errorCode(e)
    setState({ error: code })
    // E1a: only a definite "not native" leaves; `store_error` keeps the mode.
    if (code === "disabled" || code === "not_native") leaveNative()
    return "failed"
  }
  // BF1/C12/C13: a superseded reply never counts as success. The settle
  // that issued the newer load clears retry/owed state when it lands.
  if (gen <= appliedGen) return "kept"
  if (mode !== "native") return "kept"
  if (pending()) {
    if (!stash || stash.gen < gen) stash = { gen, ws }
    return "kept"
  }
  applyLoad(gen, ws)
  return "ok"
}

function applyLoad(gen: number, ws: WebStore) {
  appliedGen = gen
  if (stash && stash.gen <= gen) stash = null
  cache = {
    conversations: (ws.conversations ?? []).map(toConversation),
    history: (ws.history ?? []).map(toHistoryEntry),
  }
  hydrated = true
  notify()
}

/**
 * E5b/F09: nothing is pending any more; settle a kept load. One superseded
 * by a newer applied load is dropped; one an apply moved past since it was
 * kept is stale and re-read (a fresh generation); otherwise it is applied.
 */
function settleStash() {
  const kept = stash
  if (!kept) return
  if (kept.gen <= appliedGen && !kept.moved) {
    stash = null
    return
  }
  if (mode !== "native" || !ready || pending()) return
  stash = null
  if (kept.moved) void refresh()
  else applyLoad(kept.gen, kept.ws)
}

/**
 * Flush, then re-read the projection, then release held writes (boot,
 * `sync:chat-changed`, retry). Not ready after a failure: retried with
 * backoff and surfaced through `syncState().error`.
 */
async function settle(skipHydrate = false): Promise<void> {
  if (mode !== "native") return
  busy++
  try {
    let result: "ok" | "kept" | "failed" = (await flush()) ? "ok" : "failed"
    if (result === "ok" && !skipHydrate) result = await hydrate()
    if (mode !== "native") return
    if (result === "ok") {
      retryDelay = RETRY_BASE_MS
      const recovered = !ready || owed
      owed = false
      // Recovered (E5/E1a): the retry's error no longer applies.
      if (recovered) setState({ error: null })
      if (!ready) {
        releaseHeld()
        await flush()
      }
    } else if (result === "failed") {
      // E1a (F13): a failure after boot is retried with backoff as well.
      owed = true
      scheduleRetry()
    } else if (!ready || owed) {
      // Not ready yet, or (A-C12-1) a superseded reply left an earlier
      // failure unresolved: keep retrying with backoff until a load lands.
      scheduleRetry()
    }
  } finally {
    busy--
  }
}

export async function refresh(): Promise<void> {
  await settle()
}

function retryNow(): Promise<void> {
  return statusKnown ? settle() : run()
}

function scheduleRetry() {
  if (retryTimer || !current()) return
  const delay = retryDelay
  retryDelay = Math.min(retryDelay * 2, RETRY_MAX_MS)
  retryTimer = setTimeout(() => {
    retryTimer = null
    // E1a: a status that answered with `store_error` is retried as well.
    if (current() && (!ready || !statusKnown || owed)) void retryNow()
  }, delay)
}

/** A held write: retry now unless a boot step is already running. */
function kick() {
  if (busy > 0 || !current()) return
  if (retryTimer) {
    clearTimeout(retryTimer)
    retryTimer = null
  }
  void retryNow()
}

// ---------------------------------------------------------------------------
// Boot

const PRE_NATIVE = new Set(["backed_up", "importing", "imported", "verified"])

/** Rust says this device is not native (any more): back to localStorage. */
function leaveNative() {
  if (mode !== "native") return
  failMemory()
  storage.remove(NATIVE_KEY)
  mode = "off"
  cache = null
  stash = null
  hydrated = false
  owed = false
  setState({})
  releaseHeld()
  notify()
}

/** Not native: localStorage, as before (held writes land there). */
function goOff() {
  const was = mode
  storage.remove(NATIVE_KEY)
  mode = "off"
  cache = null
  stash = null
  hydrated = false
  owed = false
  setState({})
  releaseHeld()
  if (was === "native") notify()
}

/** M2: push the canonical snapshot in chunks. Returns the resulting state. */
async function push(): Promise<string> {
  const text = snapshotText()
  const hash = await snapshotHash(text)
  const chunks = chunkText(text)
  let result = "none"
  for (let attempt = 0; attempt < 2; attempt++) {
    try {
      for (let index = 0; index < chunks.length; index++) {
        result = await invoke<string>("sync_migrate_import_webstore", {
          index,
          total: chunks.length,
          chunk: chunks[index],
          snapshotHash: hash,
        })
        if (result !== "none") break
      }
      return result
    } catch (e) {
      // A stale partial push (other hash) is reset by Rust on the first
      // chunk; restart once from chunk 0.
      if (attempt === 0 && errorCode(e) === "migration_chunk") continue
      throw e
    }
  }
  return result
}

/** M6/M7: finalize with the current hash; resend bytes on `NeedSnapshot`. */
async function finalizeLoop(): Promise<FinalizeOut | null> {
  let last: FinalizeOut | null = null
  for (let round = 0; round < MAX_FINALIZE_ROUNDS; round++) {
    // Snapshot and queue tail are read together, synchronously: every
    // queued item above the tail is a write after this snapshot.
    const text = snapshotText()
    const tail = readQueueRaw().nextQseq - 1
    const hash = await snapshotHash(text)
    last = await invoke<FinalizeOut>("sync_migrate_finalize", {
      webviewHash: hash,
      current: null,
      currentHash: null,
      queueTail: tail,
    })
    if (last.result === "need_snapshot") {
      last = await invoke<FinalizeOut>("sync_migrate_finalize", {
        webviewHash: hash,
        current: text,
        currentHash: hash,
        queueTail: tail,
      })
    }
    setState({ finalize: last.result })
    if (last.result === "native" || last.result === "deferred") return last
  }
  return last
}

export function toConversation(c: WsConv): Conversation {
  const messages: ChatMsg[] = c.messages.map((m) => {
    const msg: ChatMsg = { id: m.id, role: m.role as ChatRole, content: m.content, ts: m.ts }
    if (m.model !== undefined) msg.model = m.model
    if (m.images.length) msg.images = m.images.map((img) => ({ ...img })) as ChatMsg["images"]
    return msg
  })
  const first = messages[0]?.ts ?? 0
  const updatedAt = c.updatedAt ?? messages.reduce((mx, m) => Math.max(mx, m.ts), first)
  return { id: c.id, title: c.title, messages, createdAt: c.createdAt ?? (first || updatedAt), updatedAt }
}

export function toHistoryEntry(t: WsTr): HistoryEntry {
  return { ...t, from: t.from as LangCode, to: t.to as LangCode }
}


/**
 * Switch to native. `cutover`: this boot's finalize just acknowledged it, so
 * the dual-written originals are current and seed the cache (writes keep
 * flowing). Otherwise (a native boot) the cache starts empty and writes are
 * held until the first hydrate (E5).
 *
 * E5a: the marker is advisory. Its write is checked and a failure surfaced
 * (`marker_write_failed`); every boot whose status says `native` writes it
 * again, and a boot without it recovers native mode from the status.
 */
async function enterNative(cutover: boolean) {
  const was = mode
  const marked = storage.set(NATIVE_KEY, true)
  mode = "native"
  if (cutover) {
    cache = fromStorage()
    hydrated = false
    ready = true
  } else if (was !== "native") {
    cache = null
    stash = null
    hydrated = false
    ready = false
  }
  setState({ error: null })
  notify()
  // Hold-time ops whose queue write failed (E2.3) are applied directly now.
  // The cache already shows them (seeded from the originals) and their
  // apply emits `sync:chat-changed`, which hydrates.
  const orphans = cutover && memItems.some((m) => m.origin === "hold")
  await settle(orphans)
  // Surfaced after the settle, whose recovery clears its own retry error.
  if (!marked && mode === "native" && storage.get<boolean>(NATIVE_KEY) !== true) {
    setState({ error: "marker_write_failed" })
    notify()
  }
}

async function listenOnce() {
  if (unlisten) return
  try {
    unlisten = await listen("sync:chat-changed", () => {
      // E5b: the projection moved; loads already in flight are stale.
      appliedGen = loadGen
      void refresh()
    })
  } catch {
    // Without the event the cache still refreshes on boot.
  }
}

async function run(): Promise<void> {
  busy++
  // E5a: until the first status answers, writes on a device without the
  // marker are kept for a replay should it turn out to be native.
  if (!statusKnown && mode === "off") awaitingStatus = true
  try {
    await runInner()
  } finally {
    busy--
  }
}

/** E1a: the status could not read `sync.db`: keep the current mode, retry. */
function keepMode(code: string) {
  setState({ error: code })
  scheduleRetry()
}

async function runInner(): Promise<void> {
  let status: StatusOut
  try {
    status = await invoke<StatusOut>("sync_get_status")
  } catch (e) {
    if (mode === "native") {
      // A native device must not fall back to the frozen originals: keep
      // holding and retry.
      keepMode(errorCode(e))
      return
    }
    awaitingStatus = false
    preStatus.length = 0
    goOff()
    return
  }
  // E1a: a store read error (or an unreadable migration state) is not an
  // answer: never `goOff` or drop the marker on it.
  if (status?.store_error === true || status?.migration === "unknown") {
    keepMode("store_error")
    return
  }
  statusKnown = true
  awaitingStatus = false
  const replay = preStatus.splice(0)
  const floor = Math.max(status?.queue_flushed_qseq ?? 0, status?.ack_qseq ?? 0) + 1
  if (Number.isSafeInteger(floor)) qseqFloor = Math.max(qseqFloor, floor)
  // E1: native stays native whatever the flag says; flag off only stops
  // the network on the Rust side.
  if (status?.migration === "native") {
    setState({ migration: "native" })
    await listenOnce()
    // E5a: writes made on the localStorage path before this status are
    // replayed against the hydrated cache (held until it lands).
    if (mode !== "native") held.push(...replay)
    await enterNative(false)
    return
  }
  if (status?.enabled !== true) {
    goOff()
    return
  }
  setState({ migration: status.migration ?? null })
  await listenOnce()
  let migration = status.migration ?? "none"
  if (!status.vault) {
    // Nothing to push into yet; an earlier hold (queue) keeps holding.
    if (mode === "native") goOff()
    setState({})
    return
  }
  if (mode === "native") goOff()
  // M3: from the push on, every write is queued as well.
  mode = "hold"
  setState({})
  try {
    if (migration === "none") {
      migration = await push()
      setState({ migration })
    }
    if (!PRE_NATIVE.has(migration) && migration !== "native") return
    const out = await finalizeLoop()
    if (out?.result === "native") {
      setState({ migration: "native" })
      await enterNative(true)
    }
  } catch (e) {
    // Deferred or failed: stay on localStorage, keep the queue.
    setState({ error: errorCode(e) })
  }
}

/** Called once at app start, after `migrateLegacy()`. */
export function bootSync(): Promise<void> {
  if (!booting) {
    booting = isTauri() ? run() : Promise.resolve(goOff())
  }
  return booting
}

/**
 * E5: `main.tsx` awaits this before the first render, so a native device
 * shows the hydrated cache, not an empty or stale one. Bounded: a slow boot
 * (first push, a stuck queue) renders after `ms` and keeps holding writes.
 */
export function bootSyncWithin(ms = BOOT_WAIT_MS): Promise<void> {
  return new Promise((resolve) => {
    const timer = setTimeout(resolve, ms)
    bootSync()
      .catch(() => {})
      .finally(() => {
        clearTimeout(timer)
        resolve()
      })
  })
}

/** Test hook. */
export function setChunkUnitsForTests(units: number) {
  chunkUnits = units
}
