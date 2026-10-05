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
export const NVIDIA_BASE = "https://integrate.api.nvidia.com/v1"
/** z.ai pay-as-you-go API. */
export const ZAI_BASE = "https://api.z.ai/api/paas/v4"
/** z.ai GLM Coding Plan: same key, subscription billing, its own path. */
export const ZAI_CODING_BASE = "https://api.z.ai/api/coding/paas/v4"

/**
 * Which backend answers a request.
 *
 * `oauth:<provider>` entries are signed in through the app's own browser OAuth
 * flow and carry no API key; everything else is an OpenAI-compatible endpoint
 * reached with a key (or unauthenticated, for local servers).
 */
export type ProviderId = EndpointProviderId | OAuthProviderRef

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
export type EndpointProviderId = "openrouter" | "nvidia" | "zai" | "ocp" | "custom"

export type ProviderPreset = {
  label: string
  baseURL: string
  description: string
  /** Chosen when switching to this provider, so the previous provider's model
   *  id is never sent here. Absent: keep whatever is selected. */
  defaultModel?: string
  /** The endpoint rejects requests without a key; no open mode. */
  requiresKey?: boolean
  /** Keychain account for this provider's own key. Absent: the shared legacy
   *  key used by OpenRouter / OCP / Custom. A separate account means switching
   *  providers does not overwrite the other provider's key. */
  keyAccount?: string
  keyPlaceholder?: string
  /** Where to create a key. */
  keyUrl?: string
}

export const PROVIDER_PRESETS: Record<EndpointProviderId, ProviderPreset> = {
  openrouter: {
    label: "OpenRouter",
    baseURL: OPENROUTER_BASE,
    description: "BYOK · 360+ models with one key",
    defaultModel: "openai/gpt-4o-mini",
    requiresKey: true,
    keyPlaceholder: "sk-or-...",
    keyUrl: "https://openrouter.ai/keys",
  },
  nvidia: {
    label: "NVIDIA",
    baseURL: NVIDIA_BASE,
    description: "build.nvidia.com hosted models (DeepSeek, Kimi, Nemotron, ...)",
    // Checked live against integrate.api.nvidia.com: listed by /v1/models
    // and not end-of-life (retired models answer 410).
    defaultModel: "deepseek-ai/deepseek-v4.1-flash",
    requiresKey: true,
    keyAccount: "nvidia_api_key",
    keyPlaceholder: "nvapi-...",
    keyUrl: "https://build.nvidia.com/settings/api-keys",
  },
  zai: {
    label: "Z.AI",
    baseURL: ZAI_BASE,
    description: "GLM models from Zhipu (z.ai), pay-as-you-go or GLM Coding Plan",
    defaultModel: "glm-5.3-flash",
    requiresKey: true,
    keyAccount: "zai_api_key",
    keyPlaceholder: "API key",
    keyUrl: "https://z.ai/manage-apikey/apikey-list",
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

/** Endpoint providers are the ones with a preset; anything else is OAuth. */
export function endpointPreset(provider: string): ProviderPreset | null {
  return Object.hasOwn(PROVIDER_PRESETS, provider)
    ? PROVIDER_PRESETS[provider as EndpointProviderId]
    : null
}

/**
 * z.ai model ids, from the chat-completion API reference (docs.z.ai). Shown
 * until the account's own `/models` answers, so the picker is never empty.
 */
export const ZAI_MODELS: OpenRouterModel[] = [
  { id: "glm-5.3-flash", name: "GLM-5.3 Flash" },
  { id: "glm-5.3", name: "GLM-5.3" },
  { id: "glm-5.3-highspeed", name: "GLM-5.3 Highspeed" },
  { id: "glm-5.2", name: "GLM-5.2" },
  { id: "glm-5.1", name: "GLM-5.1" },
  { id: "glm-5", name: "GLM-5" },
  { id: "glm-4.7", name: "GLM-4.7" },
  { id: "glm-4.7-flash", name: "GLM-4.7 Flash (free)" },
  { id: "glm-4.6", name: "GLM-4.6" },
  { id: "glm-4.5-air", name: "GLM-4.5 Air" },
  { id: "glm-4.5-flash", name: "GLM-4.5 Flash (free)" },
]

/** Which upstream a base URL points at, whatever provider the user picked:
 *  a Custom entry with the NVIDIA URL needs the same handling. */
export type EndpointKind = "openrouter" | "nvidia" | "zai" | "other"

export function endpointKind(baseURL: string): EndpointKind {
  let host: string
  try {
    host = new URL(baseURL).hostname
  } catch {
    return "other"
  }
  if (host === "openrouter.ai") return "openrouter"
  if (host === "integrate.api.nvidia.com") return "nvidia"
  if (host === "api.z.ai") return "zai"
  return "other"
}

/**
 * NVIDIA's `/models` also lists embedding, reranking, safety-classifier and
 * document-parsing models that fail on `/chat/completions`. Drop those so the
 * picker only offers models that can answer.
 */
const NVIDIA_NON_CHAT = /(embed|rerank|retriever|guard|safety|reward|clip|parse|detector|calibration|deplot|kosmos)/

export function isChatModel(kind: EndpointKind, id: string): boolean {
  return kind !== "nvidia" || !NVIDIA_NON_CHAT.test(id)
}

/**
 * z.ai GLM models reason before answering by default, which turns a one-line
 * translation into a multi-second wait. Per the API reference, GLM-5.3 /
 * GLM-5.3-Flash can only think (depth set by `reasoning_effort`, lowest
 * `low`); the other GLM-4.5+ models accept `thinking: disabled`.
 */
export function zaiReasoningParams(model: string): Record<string, unknown> {
  const id = model.toLowerCase()
  if (id.startsWith("glm-5.3")) return { reasoning_effort: "low" }
  if (/^glm-(4\.[5-9]|5)/.test(id)) return { thinking: { type: "disabled" } }
  return {}
}

/** Hardcoded Claude model list used as a fallback when an OCP-style
 *  endpoint doesn't expose `/v1/models` (or returns empty). Same ids as the
 *  Anthropic OAuth seed, which Anthropic's `/v1/models` answered. */
export const CLAUDE_CLI_MODELS: OpenRouterModel[] = [
  { id: "claude-opus-5-5", name: "Claude Opus 5.5" },
  { id: "claude-sonnet-5-5", name: "Claude Sonnet 5.5" },
  { id: "claude-opus-5", name: "Claude Opus 5" },
  { id: "claude-sonnet-5", name: "Claude Sonnet 5" },
  { id: "claude-opus-4-8", name: "Claude Opus 4.8" },
  { id: "claude-sonnet-4-6", name: "Claude Sonnet 4.6" },
  { id: "claude-haiku-4-5", name: "Claude Haiku 4.5" },
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
  const kind = endpointKind(base)
  const fallback = opts.fallbackModel?.trim()
  const hasFallback = !!fallback && fallback !== opts.model
  // Only OpenRouter understands the `models` array. Everywhere else `model`
  // is required, so the fallback is tried here instead, after the primary
  // fails for a reason other than the key.
  if (hasFallback && kind === "openrouter") {
    return completeOpenAI(base, kind, opts, { models: [opts.model, fallback] }, opts.model)
  }
  try {
    return await completeOpenAI(base, kind, opts, { model: opts.model }, opts.model)
  } catch (e) {
    if (!hasFallback || opts.signal?.aborted || isAuthError(e)) throw e
    return completeOpenAI(base, kind, opts, { model: fallback }, fallback)
  }
}

class HttpStatusError extends Error {
  readonly status: number
  constructor(status: number, message: string) {
    super(message)
    this.status = status
  }
}

function isAuthError(e: unknown): boolean {
  return e instanceof HttpStatusError && (e.status === 401 || e.status === 403)
}

async function completeOpenAI(
  base: string,
  kind: EndpointKind,
  opts: ChatOptions,
  modelFields: Record<string, unknown>,
  model: string,
): Promise<ChatResult> {
  const body: Record<string, unknown> = {
    messages: opts.messages.map((m) => ({ role: m.role, content: toOpenAIContent(m) })),
    temperature: opts.temperature ?? 0.3,
    ...modelFields,
    ...(kind === "zai" ? zaiReasoningParams(model) : {}),
  }

  const res = await httpFetch(`${base}/chat/completions`, {
    method: "POST",
    headers: {
      "Content-Type": "application/json",
      Authorization: `Bearer ${opts.apiKey}`,
      // OpenRouter's app-attribution headers; nobody else needs to see them.
      ...(kind === "openrouter"
        ? { "HTTP-Referer": window.location.origin, "X-Title": "SayKnow Kit" }
        : {}),
    },
    body: JSON.stringify(body),
    signal: opts.signal,
  })

  if (!res.ok) {
    const text = await res.text().catch(() => "")
    throw new HttpStatusError(
      res.status,
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
    model: data.model ?? model,
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
  const kind = endpointKind(base)
  // Normalize: OCP/Ollama may return entries without a friendly `name`.
  return (data.data ?? [])
    .filter((m) => isChatModel(kind, m.id))
    .map((m) => ({
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
