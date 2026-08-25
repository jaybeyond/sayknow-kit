import { useEffect, useState } from "react"
import { storage } from "@/lib/storage"
import {
  CLAUDE_CLI_MODELS,
  fetchModels,
  type OpenRouterModel,
} from "@/lib/openrouter"

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

export function useModels(apiKey: string, baseURL: string) {
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

  return { models, loading, error }
}
