/**
 * Bundled model catalogue for the OAuth providers.
 *
 * These providers are not OpenAI-compatible `/models` endpoints, so the usual
 * `useModels(apiKey, baseURL)` probe cannot discover them. Without a list of
 * their own, the picker kept showing whatever the previously selected
 * endpoint returned — an OpenRouter id like `openai/gpt-4o-mini` would then be
 * sent to `api.anthropic.com` and rejected.
 *
 * Anthropic, Codex, xAI and Cursor publish an account-scoped list that
 * replaces these once the account answers (`./model-discovery`,
 * `listCursorModels`). The bundled ids are the seed shown until then and the
 * fallback when the probe fails; Gemini has no probe and relies on them alone.
 */
import type { OpenRouterModel } from "../openrouter"
import type { OAuthProvider } from "./types"

/**
 * Ids and order as the providers' own lists answered on 2026-10-04:
 * Anthropic `GET /v1/models` (newest first), xAI `GET /v1/language-models`.
 */
const ANTHROPIC_MODELS: OpenRouterModel[] = [
  { id: "claude-opus-5-5", name: "Claude Opus 5.5" },
  { id: "claude-sonnet-5-5", name: "Claude Sonnet 5.5" },
  { id: "claude-fable-5-1", name: "Claude Fable 5.1" },
  { id: "claude-opus-5", name: "Claude Opus 5" },
  { id: "claude-sonnet-5", name: "Claude Sonnet 5" },
  { id: "claude-fable-5", name: "Claude Fable 5" },
  { id: "claude-opus-4-8", name: "Claude Opus 4.8" },
  { id: "claude-sonnet-4-6", name: "Claude Sonnet 4.6" },
  { id: "claude-haiku-4-5-20251001", name: "Claude Haiku 4.5" },
]

const XAI_MODELS: OpenRouterModel[] = [
  { id: "grok-4.7", name: "Grok 4.7" },
  { id: "grok-4.6", name: "Grok 4.6" },
  { id: "grok-4.5", name: "Grok 4.5" },
  { id: "grok-4.3", name: "Grok 4.3" },
  { id: "grok-4.20-0309-reasoning", name: "Grok 4.20 (Reasoning)" },
  { id: "grok-4.20-0309-non-reasoning", name: "Grok 4.20 (Non Reasoning)" },
]

/**
 * Models for an OAuth provider. Empty for providers whose request path is not
 * implemented yet — offering a model there would promise a call we refuse to
 * make.
 */
export const OAUTH_MODELS: Record<OAuthProvider, OpenRouterModel[]> = {
  anthropic: ANTHROPIC_MODELS,
  xai: XAI_MODELS,
  // A ChatGPT subscription does not unlock every Codex model — the backend
  // answers `400 "... is not supported when using Codex with a ChatGPT
  // account"` for the rest, which is what `gpt-5.2-codex` hit. These are the
  // `visibility: "list"` entries of the backend's own `/codex/models` answer,
  // in its priority order.
  "openai-codex": [
    { id: "gpt-6.1-sol", name: "GPT-6.1 Sol" },
    { id: "gpt-6-astra", name: "GPT-6 Astra" },
    { id: "gpt-6-sol", name: "GPT-6 Sol" },
    { id: "gpt-6-luna", name: "GPT-6 Luna" },
    { id: "gpt-5.6-sol", name: "GPT-5.6 Sol" },
    { id: "gpt-5.6-terra", name: "GPT-5.6 Terra" },
    { id: "gpt-5.6-luna", name: "GPT-5.6 Luna" },
    { id: "gpt-5.5", name: "GPT-5.5" },
  ],
  "google-gemini-cli": [
    { id: "gemini-3.5-flash", name: "Gemini 3.5 Flash" },
    { id: "gemini-3.1-pro-preview", name: "Gemini 3.1 Pro" },
    { id: "gemini-3.1-flash-lite-preview", name: "Gemini 3.1 Flash Lite" },
    { id: "gemini-3-pro-preview", name: "Gemini 3 Pro" },
    { id: "gemini-3-flash-preview", name: "Gemini 3 Flash" },
  ],
  // Cursor's own `GetUsableModels` is authoritative, but it needs a live
  // token and a round trip. These are the fallback so the picker is never
  // empty; `listCursorModels` replaces them once the account answers.
  cursor: [
    { id: "composer-2.5", name: "Composer 2.5" },
    { id: "claude-4.5-sonnet", name: "Claude 4.5 Sonnet" },
    { id: "gpt-5.5-none", name: "GPT-5.5" },
    { id: "gemini-3.5-flash", name: "Gemini 3.5 Flash" },
  ],
}

/** The model selected when the user first picks this provider. */
export function defaultOAuthModel(provider: OAuthProvider): string {
  return OAUTH_MODELS[provider][0]?.id ?? ""
}
