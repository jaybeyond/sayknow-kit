// Translation memory.
//
// Auto-translate fires on every typing pause and re-sends the whole field each
// time, so finishing one sentence can cost three to five calls for the same
// text. Against a token-billed LLM that was a rounding error; against DeepL's
// per-character monthly cap it is the difference between 500k characters
// lasting a month and lasting a week. Repeats are served from here for free.

import { storage } from "./storage"
import type { LangCode } from "./openrouter"

const KEY = "tm"
/** Entries, not bytes — each is short, and localStorage is the budget. */
const MAX_ENTRIES = 400

export type TmEngine = "deepl" | "llm"

type Entry = {
  k: string
  text: string
  /** Epoch ms of last use, for LRU eviction. */
  at: number
}

function hash(s: string): string {
  // FNV-1a. Not cryptographic — this only has to distinguish source strings.
  let h = 0x811c9dc5
  for (let i = 0; i < s.length; i++) {
    h ^= s.charCodeAt(i)
    h = Math.imul(h, 0x01000193) >>> 0
  }
  return h.toString(16)
}

function makeKey(
  engine: TmEngine,
  from: LangCode,
  to: LangCode,
  source: string,
): string {
  // The engine is part of the key: the same sentence through DeepL and through
  // an LLM are different answers, and swapping engines should not silently
  // serve the other one's result.
  return `${engine}|${from}|${to}|${hash(source.trim())}`
}

function load(): Entry[] {
  return storage.get<Entry[]>(KEY) ?? []
}

export const translationMemory = {
  get(
    engine: TmEngine,
    from: LangCode,
    to: LangCode,
    source: string,
  ): string | null {
    const k = makeKey(engine, from, to, source)
    const all = load()
    const idx = all.findIndex((e) => e.k === k)
    if (idx < 0) return null
    all[idx] = { ...all[idx], at: Date.now() }
    storage.set(KEY, all)
    return all[idx].text
  },

  put(
    engine: TmEngine,
    from: LangCode,
    to: LangCode,
    source: string,
    text: string,
  ): void {
    if (!source.trim() || !text.trim()) return
    const k = makeKey(engine, from, to, source)
    const all = load().filter((e) => e.k !== k)
    all.push({ k, text, at: Date.now() })
    // Drop the least recently used once over budget.
    all.sort((a, b) => b.at - a.at)
    storage.set(KEY, all.slice(0, MAX_ENTRIES))
  },

  clear(): void {
    storage.remove(KEY)
  },

  size(): number {
    return load().length
  },
}
