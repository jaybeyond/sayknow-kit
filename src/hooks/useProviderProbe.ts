import { useEffect, useState } from "react"
import { isTauri } from "@/lib/runtime"

async function probeFetch(url: string, init?: RequestInit): Promise<Response> {
  if (isTauri()) {
    const { fetch: tauriFetch } = await import("@tauri-apps/plugin-http")
    return tauriFetch(url, init)
  }
  return fetch(url, init)
}

export type ProbeStatus =
  | "idle"
  | "checking"
  | "ready"
  | "auth-required"
  | "down"

/**
 * Pings `${baseURL}/models` and reports whether the endpoint is reachable
 * and whether it needs auth. Used to auto-detect OCP / custom endpoints.
 */
export function useProviderProbe(
  baseURL: string,
  apiKey: string,
  enabled: boolean,
): ProbeStatus {
  // Keyed by the endpoint being probed. Both "idle" (disabled) and "checking"
  // (result is for a different endpoint) fall out of the key comparison, so
  // the effect only ever writes the settled answer.
  const probeKey = enabled && baseURL ? `${baseURL}|${apiKey}` : ""
  const [result, setResult] = useState<{ key: string; status: ProbeStatus }>({
    key: "",
    status: "idle",
  })
  const status: ProbeStatus = !probeKey
    ? "idle"
    : result.key === probeKey
      ? result.status
      : "checking"

  useEffect(() => {
    if (!enabled || !baseURL) return
    let cancelled = false

    const ctrl = new AbortController()
    const timer = window.setTimeout(() => ctrl.abort(), 4000)

    probeFetch(`${baseURL.replace(/\/$/, "")}/models`, {
      headers: apiKey ? { Authorization: `Bearer ${apiKey}` } : {},
      signal: ctrl.signal,
    })
      .then((res) => {
        if (cancelled) return
        if (res.ok) setResult({ key: probeKey, status: "ready" })
        else if (res.status === 401 || res.status === 403)
          setResult({ key: probeKey, status: "auth-required" })
        else setResult({ key: probeKey, status: "down" })
      })
      .catch(() => {
        if (!cancelled) setResult({ key: probeKey, status: "down" })
      })
      .finally(() => {
        window.clearTimeout(timer)
      })

    return () => {
      cancelled = true
      window.clearTimeout(timer)
      ctrl.abort()
    }
  }, [probeKey, enabled, baseURL, apiKey])

  return status
}
