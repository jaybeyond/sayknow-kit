/**
 * Live model lists for the OAuth providers that publish one.
 *
 * The bundled ids in `./models` rot: every new model needs a release before
 * the picker can offer it. Each provider below answers its own account-scoped
 * list, so the picker asks it and keeps the bundled list only as the seed and
 * the fallback. Endpoints and headers follow sayknow-cli's discovery code
 * (`packages/ai/src/utils/discovery/codex.ts`, `provider-models/openai-compat.ts`).
 *
 * Gemini is not here: its list endpoint is an internal Cloud Code call this
 * app has no verified request for, so it stays bundled. Cursor has its own
 * Rust command (`listCursorModels`).
 */
import { httpFetch } from "../http"
import type { OpenRouterModel } from "../openrouter"
import { ANTHROPIC_OAUTH_BETAS, CLAUDE_CODE_VERSION } from "./anthropic"
import { codexAccountId } from "./codex-chat"
import type { OAuthCredentials, OAuthProvider } from "./types"

type Fetch = typeof globalThis.fetch

/** Providers whose list is fetched here. */
export type DiscoverableProvider = Extract<OAuthProvider, "anthropic" | "openai-codex" | "xai">

export function isDiscoverable(provider: OAuthProvider): provider is DiscoverableProvider {
  return provider === "anthropic" || provider === "openai-codex" || provider === "xai"
}

/**
 * The Codex backend filters its list by `client_version`: an old version gets
 * only the models that existed then (`0.99.0` returns a single internal entry)
 * and no version is a 400. The current CLI release is read from npm, as
 * sayknow-cli does; this is the floor when npm cannot be reached.
 */
export const CODEX_CLIENT_VERSION_FALLBACK = "0.160.0"
const NPM_CODEX_LATEST_URL = "https://registry.npmjs.org/@openai%2Fcodex/latest"

const ANTHROPIC_MODELS_URL = "https://api.anthropic.com/v1/models?limit=100"
const CODEX_MODELS_URL = "https://chatgpt.com/backend-api/codex/models"
const XAI_MODELS_URL = "https://api.x.ai/v1/language-models"

/**
 * The account's model list, newest or most preferred first. `null` when the
 * provider did not answer with a usable list — the caller keeps the bundled
 * one rather than showing an empty picker.
 */
export async function discoverOAuthModels(
  provider: DiscoverableProvider,
  credentials: OAuthCredentials,
  fetchImpl: Fetch = httpFetch,
): Promise<OpenRouterModel[] | null> {
  try {
    const list =
      provider === "anthropic"
        ? await anthropicModels(credentials, fetchImpl)
        : provider === "openai-codex"
          ? await codexModels(credentials, fetchImpl)
          : await xaiModels(credentials, fetchImpl)
    return list && list.length > 0 ? list : null
  } catch {
    return null
  }
}

async function getJson(fetchImpl: Fetch, url: string, headers: Record<string, string>): Promise<unknown> {
  // `Origin: ""` strips the header the Tauri plugin would otherwise attach,
  // as the chat requests do: these are server-style calls, not a web page.
  const response = await fetchImpl(url, { method: "GET", headers: { ...headers, Origin: "" } })
  if (!response.ok) return null
  return response.json()
}

function text(value: unknown): string | undefined {
  return typeof value === "string" && value.trim() !== "" ? value.trim() : undefined
}

function records(value: unknown): Record<string, unknown>[] {
  return Array.isArray(value)
    ? value.filter((v): v is Record<string, unknown> => typeof v === "object" && v !== null)
    : []
}

/** `GET /v1/models` answers newest first, which is the order the picker wants. */
async function anthropicModels(credentials: OAuthCredentials, fetchImpl: Fetch) {
  const body = (await getJson(fetchImpl, ANTHROPIC_MODELS_URL, {
    Authorization: `Bearer ${credentials.access}`,
    "Anthropic-Version": "2023-06-01",
    "Anthropic-Beta": ANTHROPIC_OAUTH_BETAS.join(","),
    "Anthropic-Dangerous-Direct-Browser-Access": "true",
    "X-App": "cli",
    "User-Agent": `claude-cli/${CLAUDE_CODE_VERSION} (external, cli)`,
  })) as { data?: unknown } | null
  if (!body) return null
  return records(body.data).flatMap((m) => {
    const id = text(m.id)
    return id ? [{ id, name: text(m.display_name) ?? id }] : []
  })
}

async function codexClientVersion(fetchImpl: Fetch): Promise<string> {
  try {
    const body = (await getJson(fetchImpl, NPM_CODEX_LATEST_URL, { Accept: "application/json" })) as {
      version?: unknown
    } | null
    const version = text(body?.version)
    return version && /^\d+\.\d+\.\d+$/.test(version) ? version : CODEX_CLIENT_VERSION_FALLBACK
  } catch {
    return CODEX_CLIENT_VERSION_FALLBACK
  }
}

/**
 * Only `visibility: "list"` entries are offered: the backend also returns
 * hidden internal ones (an auto-review model, a reserve slot) that its own
 * picker leaves out. Ordered by the backend's `priority`.
 */
async function codexModels(credentials: OAuthCredentials, fetchImpl: Fetch) {
  const accountId = credentials.accountId ?? codexAccountId(credentials.access)
  if (!accountId) return null
  const version = await codexClientVersion(fetchImpl)
  const body = (await getJson(
    fetchImpl,
    `${CODEX_MODELS_URL}?client_version=${encodeURIComponent(version)}`,
    {
      Accept: "application/json",
      Authorization: `Bearer ${credentials.access}`,
      "chatgpt-account-id": accountId,
      "OpenAI-Beta": "responses=experimental",
      originator: "codex_cli_rs",
    },
  )) as { models?: unknown; data?: unknown } | null
  if (!body) return null
  return records(body.models ?? body.data)
    .filter((m) => m.supported_in_api !== false && (m.visibility === undefined || m.visibility === "list"))
    .flatMap((m) => {
      const id = text(m.slug) ?? text(m.id)
      if (!id) return []
      const priority = typeof m.priority === "number" ? m.priority : Number.MAX_SAFE_INTEGER
      return [{ id, name: text(m.display_name) ?? id, priority }]
    })
    .sort((a, b) => a.priority - b.priority || a.id.localeCompare(b.id))
    .map(({ id, name }) => ({ id, name }))
}

/**
 * The endpoint carries no display name. `grok-4.7` → `Grok 4.7`;
 * `grok-4.20-0309-reasoning` → `Grok 4.20 (Reasoning)`, dropping the
 * four-digit release stamp the id carries.
 */
function grokName(id: string): string {
  const words = id
    .replace(/^grok-/, "")
    .split("-")
    .filter((part) => !/^\d{4}$/.test(part))
  const cap = (part: string) => part.charAt(0).toUpperCase() + part.slice(1)
  const [version = "", ...rest] = words
  // `grok-build-0.1` has no leading version, so there is no variant to split off.
  if (!/^\d/.test(version)) return `Grok ${words.map(cap).join(" ")}`
  const variant = rest.map(cap).join(" ")
  return variant ? `Grok ${version} (${variant})` : `Grok ${version}`
}

/**
 * `/v1/language-models` lists only text models, unlike `/v1/models`, which
 * also carries the image and video generators. Sorted newest first.
 */
async function xaiModels(credentials: OAuthCredentials, fetchImpl: Fetch) {
  const body = (await getJson(fetchImpl, XAI_MODELS_URL, {
    Authorization: `Bearer ${credentials.access}`,
  })) as { models?: unknown; data?: unknown } | null
  if (!body) return null
  return records(body.models ?? body.data)
    .flatMap((m) => {
      const id = text(m.id)
      if (!id) return []
      const created = typeof m.created === "number" ? m.created : 0
      return [{ id, name: grokName(id), created }]
    })
    .sort((a, b) => b.created - a.created || a.id.localeCompare(b.id))
    .map(({ id, name }) => ({ id, name }))
}
