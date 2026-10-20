/**
 * One place that knows how to sign in to, and refresh, each provider.
 *
 * Keeps the ported per-provider flows behind a single shape so the UI and the
 * request path never branch on provider id themselves.
 */
import { httpFetch } from "../http"
import { loginAnthropic, refreshAnthropicToken } from "./anthropic"
import { loginCursor, refreshCursorToken } from "./cursor"
import { loginGeminiCli, refreshGoogleCloudToken } from "./google-gemini-cli"
import { loginOpenAICodex, refreshOpenAICodexToken } from "./openai-codex"
import { isExpired, oauthStore } from "./store"
import type { OAuthController, OAuthCredentials, OAuthProvider } from "./types"
import { loginXai, refreshXaiToken } from "./xai"

export type ProviderDefinition = {
  id: OAuthProvider
  /** Shown in the provider picker. */
  name: string
  login(ctrl: OAuthController): Promise<OAuthCredentials>
  /**
   * Exchange a refresh token for a fresh access token. Absent when the
   * provider has no refresh path, in which case expiry means re-login.
   */
  refresh?(credentials: OAuthCredentials, http: typeof globalThis.fetch): Promise<OAuthCredentials>
}

export const OAUTH_PROVIDERS: Record<OAuthProvider, ProviderDefinition> = {
  anthropic: {
    id: "anthropic",
    name: "Claude",
    login: (ctrl) => loginAnthropic(ctrl),
    refresh: (credentials, http) => refreshAnthropicToken(credentials.refresh, http),
  },
  "openai-codex": {
    id: "openai-codex",
    name: "Codex",
    login: (ctrl) => loginOpenAICodex(ctrl),
    refresh: (credentials, http) => refreshOpenAICodexToken(credentials.refresh, http),
  },
  "google-gemini-cli": {
    id: "google-gemini-cli",
    name: "Gemini",
    login: (ctrl) => loginGeminiCli(ctrl),
    // Google scopes the token to a Cloud project, so the refresh has to carry
    // the project id forward or the new token addresses nothing.
    refresh: (credentials, http) =>
      refreshGoogleCloudToken(credentials.refresh, credentials.projectId ?? "", http),
  },
  xai: {
    id: "xai",
    name: "Grok",
    login: (ctrl) => loginXai(ctrl),
    refresh: (credentials, http) => refreshXaiToken(credentials.refresh, { fetch: http }),
  },
  cursor: {
    id: "cursor",
    name: "Cursor",
    login: (ctrl) => loginCursor(ctrl),
    // Cursor mints a fresh pair from the stored refresh token.
    refresh: (credentials, http) => refreshCursorToken(credentials.refresh, http),
  },
}

export const OAUTH_PROVIDER_IDS = Object.keys(OAUTH_PROVIDERS) as OAuthProvider[]

/** Run a provider's browser login and persist whatever it returns. */
export async function signIn(
  provider: OAuthProvider,
  ctrl: OAuthController,
): Promise<OAuthCredentials> {
  const credentials = await OAUTH_PROVIDERS[provider].login({ fetch: httpFetch, ...ctrl })
  try {
    await oauthStore.set(provider, credentials)
  } catch (error) {
    // A sign-in that cannot be persisted is a failed sign-in: the browser
    // said "signed in" but the next request would find nothing. Say so here
    // rather than letting the card settle back to "signed out" with no reason.
    throw new Error(
      `Signed in, but storing the credentials failed: ${
        error instanceof Error ? error.message : String(error)
      }`,
      { cause: error },
    )
  }
  return credentials
}

export async function signOut(provider: OAuthProvider): Promise<void> {
  // Local only. The provider session and any CLI the user has installed are
  // untouched; this just forgets our copy.
  await oauthStore.clear(provider)
}

export type TokenState =
  | { status: "ready"; credentials: OAuthCredentials }
  | { status: "signed-out" }
  | { status: "reauth-required"; reason: string }
  /** Still signed in, but the token could not be renewed right now (offline,
   *  provider down). The credentials are kept; the next call tries again. */
  | { status: "unavailable"; reason: string }

/** One refresh per provider at a time. Refresh tokens can be single-use, so a
 *  second concurrent refresh with the same token would be refused and read as
 *  a revoked sign-in. Callers that arrive mid-refresh share its result. */
const refreshing = new Map<OAuthProvider, Promise<TokenState>>()

/**
 * Usable credentials for `provider`, refreshing first when they have aged out.
 *
 * A failed refresh is reported rather than thrown. Only an explicit refusal
 * from the provider (a 4xx other than 408/429) means the sign-in is gone: the
 * stale copy is dropped so nothing retries with it, and the UI asks for a new
 * sign-in. Anything else — no network after waking, a timeout, a 5xx, an
 * unreadable answer — keeps the credentials and reports `unavailable`, so a
 * bad moment never signs the user out.
 */
export async function ensureAccessToken(provider: OAuthProvider): Promise<TokenState> {
  const stored = await oauthStore.get(provider)
  if (!stored) return { status: "signed-out" }
  if (!isExpired(stored)) return { status: "ready", credentials: stored }

  const definition = OAUTH_PROVIDERS[provider]
  if (!definition.refresh || !stored.refresh) {
    await oauthStore.clear(provider)
    return { status: "reauth-required", reason: "Session expired" }
  }

  const pending = refreshing.get(provider)
  if (pending) return pending
  const run = refreshStored(provider, definition.refresh, stored).finally(() => refreshing.delete(provider))
  refreshing.set(provider, run)
  return run
}

/** HTTP statuses that are the provider refusing the refresh token itself. */
function isRefusal(status: number): boolean {
  return status >= 400 && status < 500 && status !== 408 && status !== 429
}

async function refreshStored(
  provider: OAuthProvider,
  refresh: NonNullable<ProviderDefinition["refresh"]>,
  stored: OAuthCredentials,
): Promise<TokenState> {
  // Watches the provider's answer to the token request (every provider POSTs
  // it; xAI's discovery GET before it is not the token being judged), so a
  // failure can be told apart: refused, or never properly answered.
  let refused = false
  const watched: typeof globalThis.fetch = async (input, init) => {
    const response = await httpFetch(input, init)
    if (init?.method?.toUpperCase() === "POST" && isRefusal(response.status)) refused = true
    return response
  }
  try {
    const refreshed = await refresh(stored, watched)
    await oauthStore.set(provider, refreshed)
    return { status: "ready", credentials: refreshed }
  } catch (error) {
    const reason = error instanceof Error ? error.message : String(error)
    if (!refused) return { status: "unavailable", reason }
    await oauthStore.clear(provider)
    return { status: "reauth-required", reason }
  }
}
