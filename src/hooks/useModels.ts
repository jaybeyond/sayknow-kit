import { useEffect, useState } from "react"
import { storage } from "@/lib/storage"
import {
  CLAUDE_CLI_MODELS,
  endpointKind,
  fetchModels,
  ZAI_MODELS,
  parseOAuthProvider,
  type OpenRouterModel,
} from "@/lib/openrouter"
import { OAUTH_MODELS } from "@/lib/oauth/models"
import { listCursorModels } from "@/lib/oauth/cursor-chat"
import { discoverOAuthModels, isDiscoverable } from "@/lib/oauth/model-discovery"
import { ensureAccessToken } from "@/lib/oauth/registry"
import type { OAuthProvider } from "@/lib/oauth/types"

const CACHE_KEY_PREFIX = "models-cache"
const TTL_MS = 24 * 60 * 60 * 1000 // 24h
/**
 * OAuth lists change when a provider ships a model, not per request. An hour
 * keeps a new release visible the same day without asking on every open.
 */
const OAUTH_TTL_MS = 60 * 60 * 1000

type Cache = { fetchedAt: number; data: OpenRouterModel[] }

function cacheKey(baseURL: string): string {
  return `${CACHE_KEY_PREFIX}:${baseURL}`
}

function oauthCacheKey(provider: OAuthProvider): string {
  return `oauth-models:${provider}`
}

/** The account's own list, or null to keep the bundled one. */
async function liveOAuthModels(provider: OAuthProvider): Promise<OpenRouterModel[] | null> {
  if (provider !== "cursor" && !isDiscoverable(provider)) return null
  const token = await ensureAccessToken(provider)
  if (token.status !== "ready") return null
  if (provider === "cursor") {
    // The bundled fallback already fills the picker, so a failed probe is
    // not something the user has to act on.
    const list = await listCursorModels(token.credentials.access).catch(() => [])
    return list.length > 0 ? list : null
  }
  return discoverOAuthModels(provider, token.credentials)
}

/**
 * Builds before per-endpoint caching kept one list under the bare prefix. No
 * build reads it any more, but WebKit still loads every localStorage item of
 * the origin into memory: about 0.85 MB of OpenRouter catalogue held twice.
 */
export function dropRetiredModelCache() {
  storage.remove(CACHE_KEY_PREFIX)
}

/** OCP runs on localhost:3456 by default. Loose match so 127.0.0.1 and
 *  slight URL variations all qualify for the Claude-model fallback. */
function isOcpLike(baseURL: string): boolean {
  return /^https?:\/\/(localhost|127\.0\.0\.1)(:\d+)?\b/.test(baseURL)
}

export function useModels(apiKey: string, baseURL: string, provider?: string) {
  // OAuth providers answer on their own API, not an OpenAI-compatible
  // `/models` endpoint, so they are probed per provider further down.
  const oauthProvider = provider ? parseOAuthProvider(provider) : null
  const ocpLike = isOcpLike(baseURL)
  // Known lists shown until (or instead of) the endpoint's own answer.
  const bundled = ocpLike ? CLAUDE_CLI_MODELS : endpointKind(baseURL) === "zai" ? ZAI_MODELS : null
  const [fetched, setFetched] = useState<OpenRouterModel[]>(() => {
    const cached = storage.get<Cache>(cacheKey(baseURL))
    return cached?.data ?? []
  })
  // Switching endpoints adopts that endpoint's cache during render instead of
  // in an effect, so the dropdown never shows the previous provider's models
  // for a frame.
  const [loadedFor, setLoadedFor] = useState(baseURL)
  if (loadedFor !== baseURL) {
    setLoadedFor(baseURL)
    setFetched(storage.get<Cache>(cacheKey(baseURL))?.data ?? [])
  }
  // OCP-style endpoints and z.ai get a known list until the probe returns, so
  // the dropdown is never empty. Derived, so no effect has to seed state.
  const models = fetched.length > 0 ? fetched : (bundled ?? [])
  const [loading, setLoading] = useState(false)
  const [error, setError] = useState<string | null>(null)

  useEffect(() => {
    if (!baseURL) return
    if (!apiKey && !ocpLike) return
    const key = cacheKey(baseURL)
    const cached = storage.get<Cache>(key)
    const fresh = cached && Date.now() - cached.fetchedAt < TTL_MS
    if (fresh && cached.data.length > 0) {
      // Already have it; the render-time key check below adopts the cache.
      return
    }
    let cancelled = false
    // Flags are flipped from the async callbacks, not synchronously here, so
    // mounting doesn't cascade an extra render before the request even starts.
    void Promise.resolve().then(() => {
      if (cancelled) return
      setLoading(true)
      setError(null)
    })
    fetchModels(apiKey, baseURL)
      .then((list) => {
        // Empty list from OCP / Ollama is common before they're configured —
        // keep the fallback list rather than showing an empty dropdown.
        if (list.length === 0 && bundled) return
        const sorted = [...list].sort((a, b) =>
          (a.name ?? a.id).localeCompare(b.name ?? b.id),
        )
        setFetched(sorted)
        storage.set(key, { fetchedAt: Date.now(), data: sorted })
      })
      .catch((e) => {
        // The derived fallback already covers OCP and z.ai, so a failed probe
        // there is not an error the user needs to see.
        if (bundled) return
        setError(e instanceof Error ? e.message : String(e))
      })
      .finally(() => setLoading(false))

    return () => {
      cancelled = true
    }
    // `bundled` is a module constant or null, so it only changes with baseURL.
  }, [apiKey, baseURL, ocpLike, bundled])

  // Every OAuth provider that publishes a list answers per account: a plan
  // change or a new release adds models. The bundled list is the seed shown
  // until the account answers and the fallback when it does not.
  const [oauthLive, setOauthLive] = useState(() =>
    oauthProvider ? storage.get<Cache>(oauthCacheKey(oauthProvider))?.data ?? null : null,
  )
  const [oauthFor, setOauthFor] = useState(oauthProvider)
  if (oauthFor !== oauthProvider) {
    setOauthFor(oauthProvider)
    setOauthLive(oauthProvider ? storage.get<Cache>(oauthCacheKey(oauthProvider))?.data ?? null : null)
  }
  useEffect(() => {
    if (!oauthProvider) return
    const key = oauthCacheKey(oauthProvider)
    const cached = storage.get<Cache>(key)
    if (cached && Date.now() - cached.fetchedAt < OAUTH_TTL_MS) return
    let cancelled = false
    void liveOAuthModels(oauthProvider).then((list) => {
      if (cancelled || !list) return
      setOauthLive(list)
      storage.set(key, { fetchedAt: Date.now(), data: list })
    })
    return () => {
      cancelled = true
    }
  }, [oauthProvider])

  if (oauthProvider) {
    return { models: oauthLive ?? OAUTH_MODELS[oauthProvider], loading: false, error: null }
  }
  return { models, loading, error }
}
