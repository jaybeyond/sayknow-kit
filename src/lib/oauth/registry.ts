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
  refresh?(credentials: OAuthCredentials): Promise<OAuthCredentials>
}

export const OAUTH_PROVIDERS: Record<OAuthProvider, ProviderDefinition> = {
  anthropic: {
    id: "anthropic",
    name: "Claude",
    login: (ctrl) => loginAnthropic(ctrl),
    refresh: (credentials) => refreshAnthropicToken(credentials.refresh, httpFetch),
  },
  "openai-codex": {
    id: "openai-codex",
    name: "Codex",
    login: (ctrl) => loginOpenAICodex(ctrl),
    refresh: (credentials) => refreshOpenAICodexToken(credentials.refresh, httpFetch),
  },
  "google-gemini-cli": {
    id: "google-gemini-cli",
    name: "Gemini",
    login: (ctrl) => loginGeminiCli(ctrl),
    // Google scopes the token to a Cloud project, so the refresh has to carry
    // the project id forward or the new token addresses nothing.
    refresh: (credentials) =>
      refreshGoogleCloudToken(credentials.refresh, credentials.projectId ?? "", httpFetch),
  },
  xai: {
    id: "xai",
    name: "Grok",
    login: (ctrl) => loginXai(ctrl),
    refresh: (credentials) => refreshXaiToken(credentials.refresh, { fetch: httpFetch }),
  },
  cursor: {
    id: "cursor",
    name: "Cursor",
    login: (ctrl) => loginCursor(ctrl),
    // Cursor mints a fresh pair from the stored refresh token.
    refresh: (credentials) => refreshCursorToken(credentials.refresh, httpFetch),
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

/**
 * Usable credentials for `provider`, refreshing first when they have aged out.
 *
 * A failed refresh is reported as `reauth-required` rather than thrown: an
 * expired or revoked refresh token is a normal state the UI has to render,
 * not an exception. The stale copy is dropped so nothing retries with it.
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

  try {
    const refreshed = await definition.refresh(stored)
    await oauthStore.set(provider, refreshed)
    return { status: "ready", credentials: refreshed }
  } catch (error) {
    await oauthStore.clear(provider)
    return {
      status: "reauth-required",
      reason: error instanceof Error ? error.message : String(error),
    }
  }
}
