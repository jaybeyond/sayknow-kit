// Default OpenAI-compatible provider. Any endpoint that speaks the
// `/chat/completions` and `/models` shape works — OpenRouter, OCP
// (https://github.com/dtzp555-max/ocp), Ollama, LM Studio, etc.
export const OPENROUTER_BASE = "https://openrouter.ai/api/v1"

import type { ChatImage } from "./chat-image"
import { httpFetch } from "./http"
import { OAUTH_PROVIDER_IDS } from "./oauth/registry"
import { oauthChat, toOpenAIContent } from "./oauth/chat"
import type { OAuthProvider } from "./oauth/types"

export { httpFetch }
export const OCP_BASE = "http://127.0.0.1:3456/v1"

/**
 * Which backend answers a request.
 *
 * `oauth:<provider>` entries are signed in through the app's own browser OAuth
 * flow and carry no API key; everything else is an OpenAI-compatible endpoint
 * reached with a key (or unauthenticated, for local servers).
 */
export type ProviderId = "openrouter" | "ocp" | "custom" | OAuthProviderRef

/** A provider backed by OAuth credentials rather than an API key. */
export type OAuthProviderRef = `oauth:${OAuthProvider}`

export function oauthProviderRef(provider: OAuthProvider): OAuthProviderRef {
  return `oauth:${provider}`
}

/** The provider id when `value` is an OAuth selection, else `null`. */
export function parseOAuthProvider(value: string): OAuthProvider | null {
  if (!value.startsWith("oauth:")) return null
  const id = value.slice("oauth:".length)
  return (OAUTH_PROVIDER_IDS as readonly string[]).includes(id) ? (id as OAuthProvider) : null
}

export function isOAuthProvider(value: string): value is OAuthProviderRef {
  return parseOAuthProvider(value) !== null
}

/**
 * Endpoint defaults for the key-based providers.
 *
 * OAuth providers are intentionally absent: they have no user-editable base
 * URL and no key field, so they are described by `OAUTH_PROVIDERS` instead.
 */
export type EndpointProviderId = "openrouter" | "ocp" | "custom"

export const PROVIDER_PRESETS: Record<
  EndpointProviderId,
  { label: string; baseURL: string; description: string }
> = {
  openrouter: {
    label: "OpenRouter",
    baseURL: OPENROUTER_BASE,
    description: "BYOK · 360+ models with one key",
  },
  ocp: {
    label: "OCP (Claude Pro/Max — fast)",
    baseURL: OCP_BASE,
    description: "Persistent local proxy — fastest for frequent calls (auto-translate)",
  },
  custom: {
    label: "Custom",
    baseURL: "",
    description: "Any OpenAI-compatible endpoint (Ollama, LM Studio, vLLM, ...)",
  },
}

/** Hardcoded Claude model list used as a fallback when an OCP-style
 *  endpoint doesn't expose `/v1/models` (or returns empty). */
export const CLAUDE_CLI_MODELS: OpenRouterModel[] = [
  { id: "claude-opus-4-5", name: "Claude Opus 4.5" },
  { id: "claude-sonnet-4-5", name: "Claude Sonnet 4.5" },
  { id: "claude-haiku-4-5", name: "Claude Haiku 4.5" },
  { id: "claude-opus-4-1", name: "Claude Opus 4.1" },
  { id: "claude-sonnet-4", name: "Claude Sonnet 4" },
]

function trimSlash(s: string): string {
  return s.endsWith("/") ? s.slice(0, -1) : s
}

export type ChatMessage = {
  role: "system" | "user" | "assistant"
  content: string
  /** Attached images; only user turns carry them. */
  images?: ChatImage[]
}

export type ChatOptions = {
  apiKey: string
  /** Base URL of the OpenAI-compatible endpoint. Defaults to OpenRouter. */
  baseURL?: string
  model: string
  /** Optional fallback model. Sent in the OpenRouter-style `models` array so
   * the upstream can retry server-side if the primary fails. Endpoints that
   * don't understand `models` will just ignore the field. */
  fallbackModel?: string
  messages: ChatMessage[]
  signal?: AbortSignal
  /**
   * Which backend to use. When this names an OAuth provider the request is
   * routed to that provider's own API with the stored token, and `apiKey` /
   * `baseURL` are ignored. Omitted means the OpenAI-compatible path, which is
   * what every existing caller already did.
   */
  provider?: ProviderId
  temperature?: number
  /**
   * Streamed assistant text, chunk by chunk.
   *
   * Only the OAuth providers whose transport streams call this — today that
   * is Cursor. Every path still resolves with the complete text, so this is
   * additive for callers that want to render as it arrives.
   */
  onDelta?: (text: string) => void
  /**
   * Tool-call activity from an agentic provider. Cursor is the only one that
   * reports it; the rest never call this.
   */
  onTool?: (tool: { callId: string; name: string; completed: boolean }) => void
}

export type ChatUsage = {
  prompt_tokens: number
  completion_tokens: number
  total_tokens: number
}

export type ChatResult = {
  content: string
  /** The model the upstream actually used. */
  model: string
  usage?: ChatUsage
}

export async function chat(opts: ChatOptions): Promise<ChatResult> {
  const oauthProvider = opts.provider ? parseOAuthProvider(opts.provider) : null
  if (oauthProvider) {
    const result = await oauthChat({
      provider: oauthProvider,
      model: opts.model,
      messages: opts.messages,
      temperature: opts.temperature,
      signal: opts.signal,
      onDelta: opts.onDelta,
      onTool: opts.onTool,
    })
    return {
      content: result.content,
      model: result.model,
      usage: result.usage
        ? {
            prompt_tokens: result.usage.prompt_tokens ?? 0,
            completion_tokens: result.usage.completion_tokens ?? 0,
            total_tokens:
              (result.usage.prompt_tokens ?? 0) + (result.usage.completion_tokens ?? 0),
          }
        : undefined,
    }
  }

  const base = trimSlash(opts.baseURL ?? OPENROUTER_BASE)
  const fallback = opts.fallbackModel?.trim()
  const body: Record<string, unknown> = {
    messages: opts.messages.map((m) => ({ role: m.role, content: toOpenAIContent(m) })),
    temperature: opts.temperature ?? 0.3,
  }
  if (fallback && fallback !== opts.model) {
    body.models = [opts.model, fallback]
  } else {
    body.model = opts.model
  }

  const res = await httpFetch(`${base}/chat/completions`, {
    method: "POST",
    headers: {
      "Content-Type": "application/json",
      Authorization: `Bearer ${opts.apiKey}`,
      // OpenRouter-specific attribution headers. Other providers ignore them.
      "HTTP-Referer": window.location.origin,
      "X-Title": "SayKnow Kit",
    },
    body: JSON.stringify(body),
    signal: opts.signal,
  })

  if (!res.ok) {
    const text = await res.text().catch(() => "")
    throw new Error(
      `${res.status}: ${text.slice(0, 200) || res.statusText}`,
    )
  }

  const data = (await res.json()) as {
    choices?: { message?: { content?: string } }[]
    model?: string
    usage?: ChatUsage
  }
  const content = data.choices?.[0]?.message?.content
  if (!content) throw new Error("Empty response from model")
  return {
    content: content.trim(),
    model: data.model ?? opts.model,
    usage: data.usage,
  }
}

/**
 * Cross-provider auth check. Hits `/models` with the supplied key — any
 * 2xx response means the endpoint accepts the key. OpenRouter, OCP, Ollama
 * (open), and LM Studio all expose this endpoint.
 */
export async function verifyKey(
  apiKey: string,
  baseURL?: string,
): Promise<boolean> {
  const base = trimSlash(baseURL ?? OPENROUTER_BASE)
  try {
    const res = await httpFetch(`${base}/models`, {
      headers: apiKey ? { Authorization: `Bearer ${apiKey}` } : {},
    })
    return res.ok
  } catch {
    return false
  }
}

export type OpenRouterModel = {
  id: string
  name: string
  context_length?: number
  pricing?: { prompt?: string; completion?: string }
}

export async function fetchModels(
  apiKey: string,
  baseURL?: string,
): Promise<OpenRouterModel[]> {
  const base = trimSlash(baseURL ?? OPENROUTER_BASE)
  const res = await httpFetch(`${base}/models`, {
    headers: apiKey ? { Authorization: `Bearer ${apiKey}` } : {},
  })
  if (!res.ok) throw new Error(`models ${res.status}`)
  const data = (await res.json()) as { data?: OpenRouterModel[] }
  // Normalize: OCP/Ollama may return entries without a friendly `name`.
  return (data.data ?? []).map((m) => ({
    ...m,
    name: m.name ?? m.id,
  }))
}

export const LANGS = [
  { code: "auto",  label: "자동 감지", english: "Auto-detect", keywords: "auto detect" },
  // East Asia
  { code: "ko", label: "한국어",   english: "Korean",     keywords: "korean ko hangul" },
  { code: "en", label: "English",  english: "English",    keywords: "english en" },
  { code: "ja", label: "日本語",   english: "Japanese",   keywords: "japanese ja nihongo" },
  { code: "zh", label: "简体中文", english: "Chinese (Simplified)", keywords: "chinese zh mandarin simplified 简体" },
  { code: "zh-Hant", label: "繁體中文", english: "Chinese (Traditional)", keywords: "chinese traditional zh-hant taiwan 繁體" },
  // Southeast Asia
  { code: "vi", label: "Tiếng Việt", english: "Vietnamese", keywords: "vietnamese vi" },
  { code: "th", label: "ไทย",      english: "Thai",       keywords: "thai th" },
  { code: "id", label: "Bahasa Indonesia", english: "Indonesian", keywords: "indonesian id bahasa" },
  { code: "ms", label: "Bahasa Melayu",    english: "Malay",      keywords: "malay ms" },
  { code: "tl", label: "Filipino", english: "Filipino",   keywords: "filipino tagalog tl" },
  // South Asia
  { code: "hi", label: "हिन्दी",    english: "Hindi",      keywords: "hindi hi" },
  { code: "bn", label: "বাংলা",     english: "Bengali",    keywords: "bengali bn bangla" },
  { code: "ur", label: "اردو",      english: "Urdu",       keywords: "urdu ur" },
  { code: "ta", label: "தமிழ்",     english: "Tamil",      keywords: "tamil ta" },
  // Europe (West)
  { code: "es", label: "Español",  english: "Spanish",    keywords: "spanish es castellano" },
  { code: "fr", label: "Français", english: "French",     keywords: "french fr" },
  { code: "de", label: "Deutsch",  english: "German",     keywords: "german de deutsch" },
  { code: "it", label: "Italiano", english: "Italian",    keywords: "italian it" },
  { code: "pt", label: "Português",english: "Portuguese", keywords: "portuguese pt" },
  { code: "nl", label: "Nederlands", english: "Dutch",    keywords: "dutch nl nederlands" },
  // Europe (North)
  { code: "sv", label: "Svenska",  english: "Swedish",    keywords: "swedish sv svenska" },
  { code: "da", label: "Dansk",    english: "Danish",     keywords: "danish da dansk" },
  { code: "no", label: "Norsk",    english: "Norwegian",  keywords: "norwegian no norsk" },
  { code: "fi", label: "Suomi",    english: "Finnish",    keywords: "finnish fi suomi" },
  // Europe (East)
  { code: "ru", label: "Русский",  english: "Russian",    keywords: "russian ru" },
  { code: "uk", label: "Українська", english: "Ukrainian", keywords: "ukrainian uk" },
  { code: "pl", label: "Polski",   english: "Polish",     keywords: "polish pl polski" },
  { code: "cs", label: "Čeština",  english: "Czech",      keywords: "czech cs cestina" },
  { code: "hu", label: "Magyar",   english: "Hungarian",  keywords: "hungarian hu magyar" },
  { code: "ro", label: "Română",   english: "Romanian",   keywords: "romanian ro romana" },
  { code: "el", label: "Ελληνικά", english: "Greek",      keywords: "greek el" },
  { code: "bg", label: "Български",english: "Bulgarian",  keywords: "bulgarian bg" },
  // Middle East / Africa
  { code: "ar", label: "العربية",  english: "Arabic",     keywords: "arabic ar" },
  { code: "he", label: "עברית",    english: "Hebrew",     keywords: "hebrew he ivrit" },
  { code: "fa", label: "فارسی",    english: "Persian",    keywords: "persian farsi fa" },
  { code: "tr", label: "Türkçe",   english: "Turkish",    keywords: "turkish tr turkce" },
  { code: "sw", label: "Kiswahili",english: "Swahili",    keywords: "swahili sw kiswahili" },
] as const

export type LangCode = (typeof LANGS)[number]["code"]

export function langLabel(code: string): string {
  return LANGS.find((l) => l.code === code)?.english ?? code
}

export type GlossaryPair = { source: string; target: string }

export const DEFAULT_TRANSLATE_PROMPT =
  "You are a professional translator. Translate the user's text naturally and concisely. " +
  "Preserve meaning, tone, and formatting. Output ONLY the translation, no explanations, no quotes."

export const DEFAULT_REFINE_PROMPT =
  "You are a professional translator. Revise the existing translation per the user's instruction. " +
  "Output ONLY the revised translation, no explanations, no quotes."

function glossaryClause(glossary?: GlossaryPair[]): string {
  const entries = (glossary ?? []).filter(
    (g) => g.source.trim() && g.target.trim(),
  )
  if (entries.length === 0) return ""
  const lines = entries.map((g) => `- "${g.source}" → "${g.target}"`).join("\n")
  return (
    "\n\nGlossary — always translate these terms exactly as specified " +
    "(case-insensitive matching, preserve surrounding text):\n" +
    lines
  )
}

function applyTemplate(
  template: string,
  vars: { from: string; to: string; glossary: string },
): string {
  return template
    .replace(/\{from\}/g, vars.from)
    .replace(/\{to\}/g, vars.to)
    .replace(/\{glossary\}/g, vars.glossary)
}

export type PromptOverrides = {
  translate?: string
  refine?: string
}

export function buildTranslatePrompt(
  text: string,
  from: LangCode,
  to: LangCode,
  glossary?: GlossaryPair[],
  overrides?: PromptOverrides,
): ChatMessage[] {
  const sourceLine =
    from === "auto" ? "Detect the source language." : `Source language: ${langLabel(from)}.`
  const baseSystem = overrides?.translate?.trim()
    ? applyTemplate(overrides.translate, {
        from: from === "auto" ? "auto-detect" : langLabel(from),
        to: langLabel(to),
        glossary: glossaryClause(glossary).trim(),
      })
    : DEFAULT_TRANSLATE_PROMPT + glossaryClause(glossary)
  return [
    { role: "system", content: baseSystem },
    {
      role: "user",
      content: `${sourceLine}\nTarget language: ${langLabel(to)}.\n\nText:\n${text}`,
    },
  ]
}

export function buildRefinePrompt(
  original: string,
  current: string,
  to: LangCode,
  instruction: string,
  glossary?: GlossaryPair[],
  overrides?: PromptOverrides,
): ChatMessage[] {
  const baseSystem = overrides?.refine?.trim()
    ? applyTemplate(overrides.refine, {
        from: "",
        to: langLabel(to),
        glossary: glossaryClause(glossary).trim(),
      })
    : DEFAULT_REFINE_PROMPT + glossaryClause(glossary)
  return [
    { role: "system", content: baseSystem },
    {
      role: "user",
      content:
        `Target language: ${langLabel(to)}.\n` +
        `Instruction: ${instruction}\n\n` +
        `Original:\n${original}\n\n` +
        `Current translation:\n${current}`,
    },
  ]
}
