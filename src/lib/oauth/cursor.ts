/**
 * Cursor OAuth flow.
 *
 * Source: `packages/ai/src/utils/oauth/cursor.ts` @ @sayknow-cli/ai 0.5.20.
 * Endpoints, PKCE, the poll backoff, and the JWT expiry parsing are unchanged.
 *
 * Unlike the other providers this one never binds a local port: Cursor hands
 * the browser a `uuid`, and the client polls until the session is approved. So
 * there is no callback listener involved at all.
 *
 * Adaptations: `Bun.sleep` becomes the shared abortable `delay`, and requests
 * take the injected fetch so they go through the Tauri HTTP plugin.
 */
import { decodeJwt } from "./base64"
import { delay } from "./delay"
import { generatePKCE } from "./pkce"
import type { OAuthController, OAuthCredentials } from "./types"

const CURSOR_LOGIN_URL = "https://cursor.com/loginDeepControl"
const CURSOR_POLL_URL = "https://api2.cursor.sh/auth/poll"
const CURSOR_REFRESH_URL = "https://api2.cursor.sh/auth/exchange_user_api_key"

const POLL_MAX_ATTEMPTS = 150
const POLL_BASE_DELAY = 1000
const POLL_MAX_DELAY = 10000
const POLL_BACKOFF_MULTIPLIER = 1.2

export interface CursorAuthParams {
  verifier: string
  challenge: string
  uuid: string
  loginUrl: string
}

export async function generateCursorAuthParams(): Promise<CursorAuthParams> {
  const { verifier, challenge } = await generatePKCE()
  const uuid = crypto.randomUUID()

  const params = new URLSearchParams({
    challenge,
    uuid,
    mode: "login",
    redirectTarget: "cli",
  })

  return { verifier, challenge, uuid, loginUrl: `${CURSOR_LOGIN_URL}?${params.toString()}` }
}

/**
 * Wait for the browser session to be approved.
 *
 * 404 means "not approved yet" and is the normal case, so it backs off rather
 * than failing. Only a run of genuine errors aborts.
 */
export async function pollCursorAuth(
  uuid: string,
  verifier: string,
  fetchImpl: typeof globalThis.fetch,
  signal?: AbortSignal,
): Promise<{ accessToken: string; refreshToken: string }> {
  let pollDelay = POLL_BASE_DELAY
  let consecutiveErrors = 0

  for (let attempt = 0; attempt < POLL_MAX_ATTEMPTS; attempt++) {
    await delay(pollDelay, signal)
    if (signal?.aborted) throw new Error("Cursor authentication cancelled")

    try {
      const response = await fetchImpl(`${CURSOR_POLL_URL}?uuid=${uuid}&verifier=${verifier}`)

      if (response.status === 404) {
        consecutiveErrors = 0
        pollDelay = Math.min(pollDelay * POLL_BACKOFF_MULTIPLIER, POLL_MAX_DELAY)
        continue
      }

      if (response.ok) {
        const data = (await response.json()) as { accessToken: string; refreshToken: string }
        return { accessToken: data.accessToken, refreshToken: data.refreshToken }
      }

      throw new Error(`Poll failed: ${response.status}`)
    } catch {
      consecutiveErrors++
      if (consecutiveErrors >= 3) {
        throw new Error("Too many consecutive errors during Cursor auth polling")
      }
    }
  }

  throw new Error("Cursor authentication polling timeout")
}

/** Expiry from the token's own `exp`, with an hour's fallback. */
function getTokenExpiry(token: string): number {
  const payload = decodeJwt<{ exp?: unknown }>(token)
  if (payload && typeof payload.exp === "number") {
    // Retire five minutes early so an in-flight request cannot straddle it.
    return payload.exp * 1000 - 5 * 60 * 1000
  }
  return Date.now() + 3600 * 1000
}

export async function loginCursor(ctrl: OAuthController): Promise<OAuthCredentials> {
  const fetchImpl = ctrl.fetch ?? globalThis.fetch
  const { verifier, uuid, loginUrl } = await generateCursorAuthParams()

  ctrl.onAuth?.({
    url: loginUrl,
    instructions: "Approve the sign-in in your browser; this window is waiting for it.",
  })
  ctrl.onProgress?.("Waiting for browser approval…")

  const { accessToken, refreshToken } = await pollCursorAuth(
    uuid,
    verifier,
    fetchImpl,
    ctrl.signal,
  )

  return {
    access: accessToken,
    refresh: refreshToken,
    expires: getTokenExpiry(accessToken),
  }
}

export async function refreshCursorToken(
  apiKeyOrRefreshToken: string,
  fetchImpl: typeof globalThis.fetch = globalThis.fetch,
): Promise<OAuthCredentials> {
  const response = await fetchImpl(CURSOR_REFRESH_URL, {
    method: "POST",
    headers: {
      Authorization: `Bearer ${apiKeyOrRefreshToken}`,
      "Content-Type": "application/json",
    },
    body: "{}",
  })

  if (!response.ok) {
    throw new Error(`Cursor token refresh failed: ${await response.text()}`)
  }

  const data = (await response.json()) as { accessToken: string; refreshToken: string }

  return {
    access: data.accessToken,
    // A refresh does not always mint a new refresh token; keep the old one.
    refresh: data.refreshToken || apiKeyOrRefreshToken,
    expires: getTokenExpiry(data.accessToken),
  }
}
