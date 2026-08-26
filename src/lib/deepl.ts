// DeepL adapter.
//
// The LLM providers all speak one shape (`/chat/completions`), so they share a
// single client. DeepL does not: it is a purpose-built translation endpoint
// with its own auth header, its own language codes, and per-character billing
// with a hard monthly cap. It gets its own adapter rather than being bent into
// the chat interface.

import type { LangCode } from "./openrouter"

const PRO_BASE = "https://api.deepl.com"
const FREE_BASE = "https://api-free.deepl.com"

/** Free keys carry a `:fx` suffix and must go to the free host. */
export function deeplBase(key: string): string {
  return key.trim().endsWith(":fx") ? FREE_BASE : PRO_BASE
}

/**
 * Our language codes to DeepL's, verified against DeepL's supported-language
 * list — all 37 of ours are covered.
 *
 * Where DeepL splits a language into variants we pick the one a user is most
 * likely to mean: simplified Chinese, Brazilian Portuguese, and Bokmål for
 * Norwegian (DeepL has no `NO`). Targets must be variant-qualified; sources
 * must not be, which is why the two maps differ.
 */
const TARGET: Partial<Record<LangCode, string>> = {
  ko: "KO", en: "EN-US", ja: "JA", zh: "ZH-HANS", vi: "VI", th: "TH",
  id: "ID", ms: "MS", tl: "TL", hi: "HI", bn: "BN", ur: "UR", ta: "TA",
  es: "ES", fr: "FR", de: "DE", it: "IT", pt: "PT-BR", nl: "NL", sv: "SV",
  da: "DA", no: "NB", fi: "FI", ru: "RU", uk: "UK", pl: "PL", cs: "CS",
  hu: "HU", ro: "RO", el: "EL", bg: "BG", ar: "AR", he: "HE", fa: "FA",
  tr: "TR", sw: "SW",
}

const SOURCE: Partial<Record<LangCode, string>> = {
  ...TARGET,
  en: "EN",
  pt: "PT",
  zh: "ZH",
}

export function toDeeplTarget(code: LangCode): string | null {
  return TARGET[code] ?? null
}

/** `auto` maps to no source_lang at all, which is DeepL's auto-detect. */
export function toDeeplSource(code: LangCode): string | null {
  if (code === "auto") return null
  return SOURCE[code] ?? null
}

/** True when both ends of the pair are something DeepL can actually do. */
export function deeplSupports(from: LangCode, to: LangCode): boolean {
  if (!toDeeplTarget(to)) return false
  return from === "auto" || !!SOURCE[from]
}

export class DeeplError extends Error {
  readonly status: number

  constructor(message: string, status: number) {
    super(message)
    this.name = "DeeplError"
    this.status = status
  }
  /** Monthly character cap reached — the caller should fall back, not fail. */
  get quotaExceeded(): boolean {
    return this.status === 456
  }
}

async function deeplFetch(
  url: string,
  key: string,
  init?: RequestInit,
): Promise<Response> {
  const headers = {
    Authorization: `DeepL-Auth-Key ${key.trim()}`,
    "Content-Type": "application/json",
    ...(init?.headers ?? {}),
  }
  // Same reason the LLM client routes through the Tauri HTTP plugin: the
  // webview's CORS policy would otherwise block a cross-origin API call.
  if (
    typeof window !== "undefined" &&
    ("__TAURI_INTERNALS__" in window || "__TAURI__" in window)
  ) {
    const { fetch: tauriFetch } = await import("@tauri-apps/plugin-http")
    return tauriFetch(url, { ...init, headers })
  }
  return fetch(url, { ...init, headers })
}

async function raise(res: Response): Promise<never> {
  let detail = ""
  try {
    const body = (await res.json()) as { message?: string }
    detail = body.message ?? ""
  } catch {
    /* DeepL doesn't always send a JSON body */
  }
  throw new DeeplError(
    detail || `DeepL request failed (HTTP ${res.status})`,
    res.status,
  )
}

export type DeeplTranslateResult = {
  text: string
  detectedSource: string | null
  /** Source characters this call consumed against the quota. */
  billedCharacters: number
}

export async function deeplTranslate(opts: {
  key: string
  text: string
  from: LangCode
  to: LangCode
  /** DeepL renders tone natively; the LLM has to be asked in prose. */
  formality?: "default" | "more" | "less"
  signal?: AbortSignal
}): Promise<DeeplTranslateResult> {
  const target = toDeeplTarget(opts.to)
  if (!target) {
    throw new DeeplError(`DeepL has no target language for ${opts.to}`, 400)
  }
  const source = toDeeplSource(opts.from)
  const body: Record<string, unknown> = {
    text: [opts.text],
    target_lang: target,
  }
  if (source) body.source_lang = source
  // Formality is only accepted for languages that have a formal register;
  // asking for it elsewhere is an error, so only send a non-default value.
  if (opts.formality && opts.formality !== "default") {
    body.formality = `prefer_${opts.formality}`
  }

  const res = await deeplFetch(`${deeplBase(opts.key)}/v2/translate`, opts.key, {
    method: "POST",
    body: JSON.stringify(body),
    signal: opts.signal,
  })
  if (!res.ok) await raise(res)

  const data = (await res.json()) as {
    translations?: { text: string; detected_source_language?: string }[]
  }
  const first = data.translations?.[0]
  if (!first) throw new DeeplError("DeepL returned no translation", 502)
  return {
    text: first.text,
    detectedSource: first.detected_source_language ?? null,
    // DeepL bills the source text in Unicode code points.
    billedCharacters: [...opts.text].length,
  }
}

export type DeeplUsage = {
  characterCount: number
  characterLimit: number
  /** Free/Pro-Classic accounts report no product breakdown. */
  plan: "free" | "pro"
}

/** Live quota. Unlike the CLI snapshots on the usage tab, this is current. */
export async function deeplUsage(
  key: string,
  signal?: AbortSignal,
): Promise<DeeplUsage> {
  const res = await deeplFetch(`${deeplBase(key)}/v2/usage`, key, {
    method: "GET",
    signal,
  })
  if (!res.ok) await raise(res)
  const data = (await res.json()) as {
    character_count?: number
    character_limit?: number
  }
  return {
    characterCount: data.character_count ?? 0,
    // Never assume the documented 500k: Pro and Cost Control report their own
    // ceiling, and "no limit" comes back as 1e12.
    characterLimit: data.character_limit ?? 0,
    plan: deeplBase(key) === FREE_BASE ? "free" : "pro",
  }
}

export function formatCharacters(n: number): string {
  if (n >= 1_000_000_000) return "∞"
  if (n >= 1_000_000) return `${(n / 1_000_000).toFixed(2)}M`
  if (n >= 1_000) return `${(n / 1_000).toFixed(1)}K`
  return `${n}`
}
