import { useEffect, useState } from "react"
import { storage } from "@/lib/storage"
import {
  CLAUDE_CLI_MODELS,
  fetchModels,
  parseOAuthProvider,
  type OpenRouterModel,
} from "@/lib/openrouter"
import { OAUTH_MODELS } from "@/lib/oauth/models"
import { listCursorModels } from "@/lib/oauth/cursor-chat"
import { ensureAccessToken } from "@/lib/oauth/registry"

const CACHE_KEY_PREFIX = "models-cache"
const TTL_MS = 24 * 60 * 60 * 1000 // 24h

type Cache = { fetchedAt: number; data: OpenRouterModel[] }

function cacheKey(baseURL: string): string {
  return `${CACHE_KEY_PREFIX}:${baseURL}`
}

/** OCP runs on localhost:3456 by default. Loose match so 127.0.0.1 and
 *  slight URL variations all qualify for the Claude-model fallback. */
function isOcpLike(baseURL: string): boolean {
  return /^https?:\/\/(localhost|127\.0\.0\.1)(:\d+)?\b/.test(baseURL)
}

export function useModels(apiKey: string, baseURL: string, provider?: string) {
  // OAuth providers answer on their own API, not an OpenAI-compatible
  // `/models` endpoint, so their catalogue is bundled rather than probed.
  const oauthProvider = provider ? parseOAuthProvider(provider) : null
  const ocpLike = isOcpLike(baseURL)
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
  // OCP-style endpoints get a known Claude list until the probe returns, so
  // the dropdown is never empty. Derived, so no effect has to seed state.
  const models = fetched.length > 0 ? fetched : ocpLike ? CLAUDE_CLI_MODELS : []
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
        if (list.length === 0 && ocpLike) return
        const sorted = [...list].sort((a, b) =>
          (a.name ?? a.id).localeCompare(b.name ?? b.id),
        )
        setFetched(sorted)
        storage.set(key, { fetchedAt: Date.now(), data: sorted })
      })
      .catch((e) => {
        // The derived fallback already covers OCP, so a failed probe there is
        // not an error the user needs to see.
        if (ocpLike) return
        setError(e instanceof Error ? e.message : String(e))
      })
      .finally(() => setLoading(false))

    return () => {
      cancelled = true
    }
  }, [apiKey, baseURL, ocpLike])

  const [cursorModels, setCursorModels] = useState<OpenRouterModel[] | null>(null)
  useEffect(() => {
    if (oauthProvider !== "cursor") return
    let cancelled = false
    void (async () => {
      const token = await ensureAccessToken("cursor")
      if (cancelled || token.status !== "ready") return
      try {
        const list = await listCursorModels(token.credentials.access)
        if (!cancelled && list.length > 0) setCursorModels(list)
      } catch {
        // The bundled fallback already fills the picker, so a failed probe is
        // not something the user has to act on.
      }
    })()
    return () => {
      cancelled = true
    }
  }, [oauthProvider])

  // Cursor is the one OAuth provider that publishes its own list, and it is
  // account-specific: a plan change adds or removes models. The bundled list
  // is only the seed shown until the account answers.
  if (oauthProvider) {
    const catalogue =
      oauthProvider === "cursor"
        ? cursorModels ?? OAUTH_MODELS.cursor
        : OAUTH_MODELS[oauthProvider]
    return { models: catalogue, loading: false, error: null }
  }
  return { models, loading, error }
}
