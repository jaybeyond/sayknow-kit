import { storage } from "./storage"
import {
  cachedHistory,
  commitNative,
  enqueue,
  isNative,
  setCachedHistory,
  whenWritable,
  wsTr,
  type QueuedOp,
} from "./sync-client"
import type { LangCode } from "./openrouter"

export type HistoryEntry = {
  id: string
  ts: number
  source: string
  target: string
  from: LangCode
  to: LangCode
  model: string
  pinned?: boolean
}

const KEY = "history"
const MAX_UNPINNED = 50

function sortPinnedFirst(list: HistoryEntry[]): HistoryEntry[] {
  // Stable: pinned first (newest pinned at top), then unpinned by recency.
  return [...list].sort((a, b) => {
    if (!!a.pinned !== !!b.pinned) return a.pinned ? -1 : 1
    return b.ts - a.ts
  })
}

/** The stored entries: the native cache under `native`, else localStorage. */
function readAll(): HistoryEntry[] {
  if (isNative()) return cachedHistory()
  return storage.get<HistoryEntry[]>(KEY) ?? []
}

/**
 * Persist and queue `ops`. Under `native` the cache changes only once the
 * ops are durable (`commitNative`, E2); otherwise localStorage is written
 * exactly as before (`write` runs the original storage call) and the ops are
 * queued only during write-hold. The unpinned cap never queues a delete.
 */
function commit(list: HistoryEntry[], ops: QueuedOp[], write: () => void) {
  if (isNative()) {
    commitNative(ops, () => setCachedHistory(list))
    return
  }
  write()
  enqueue(...ops)
}

export const history = {
  list(): HistoryEntry[] {
    const sorted = sortPinnedFirst(readAll())
    if (!isNative()) return sorted
    // Display-only cap under `native`: nothing is deleted natively.
    const pinned = sorted.filter((e) => e.pinned)
    const unpinned = sorted.filter((e) => !e.pinned).slice(0, MAX_UNPINNED)
    return [...pinned, ...unpinned]
  },
  // E5: every write is held until the native cache is hydrated on a native
  // boot, then applied against it (dedupe and toggles see the real state).
  add(entry: Omit<HistoryEntry, "id" | "ts" | "pinned">) {
    whenWritable(() => addNow(entry))
  },
  togglePin(id: string) {
    whenWritable(() => {
      const list = sortPinnedFirst(readAll())
      const hit = list.find((e) => e.id === id)
      const next = list.map((e) => (e.id === id ? { ...e, pinned: !e.pinned } : e))
      const ops: QueuedOp[] = hit ? [{ op: "tr_pin", tr: id, pinned: !hit.pinned }] : []
      commit(next, ops, () => storage.set(KEY, next))
    })
  },
  clear() {
    whenWritable(() => {
      // Pin'd 항목은 보존. 사용자 의도 보호.
      const all = sortPinnedFirst(readAll())
      const pinned = all.filter((e) => e.pinned)
      const observedIds = all.filter((e) => !e.pinned).map((e) => e.id)
      commit(pinned, [{ op: "tr_clear", observedIds }], () => {
        if (pinned.length > 0) storage.set(KEY, pinned)
        else storage.remove(KEY)
      })
    })
  },
  remove(id: string) {
    whenWritable(() => {
      const next = sortPinnedFirst(readAll()).filter((e) => e.id !== id)
      commit(next, [{ op: "tr_delete", tr: id }], () => storage.set(KEY, next))
    })
  },
}

function addNow(entry: Omit<HistoryEntry, "id" | "ts" | "pinned">) {
  const trimmed = entry.source.trim()
  if (!trimmed || !entry.target.trim()) return
  const prev = history.list()
  if (
    prev[0] &&
    !prev[0].pinned &&
    prev[0].source === entry.source &&
    prev[0].target === entry.target &&
    prev[0].from === entry.from &&
    prev[0].to === entry.to
  ) {
    return
  }
  const next: HistoryEntry = {
    ...entry,
    id: crypto.randomUUID(),
    ts: Date.now(),
    pinned: false,
  }
  const ops: QueuedOp[] = [{ op: "tr_add", tr: wsTr(next) }]
  if (isNative()) {
    commit([next, ...readAll()], ops, () => {})
    return
  }
  const combined = [next, ...prev]
  // Cap only the unpinned subset.
  const pinned = combined.filter((e) => e.pinned)
  const unpinned = combined.filter((e) => !e.pinned).slice(0, MAX_UNPINNED)
  commit([], ops, () => storage.set(KEY, [...pinned, ...unpinned]))
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
  return new Date(ts).toLocaleDateString("ko-KR")
}
