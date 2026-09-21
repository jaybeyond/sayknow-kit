/**
 * Anthropic OAuth flow (Claude Pro/Max).
 *
 * Source: `packages/ai/src/utils/oauth/anthropic.ts` @ @sayknow-cli/ai 0.5.20.
 * Endpoints, client id, scopes, PKCE handling, the `#`-fragment split in the
 * pasted code, and the five-minute expiry safety margin are all unchanged.
 *
 * One adaptation: upstream's `postJson` closes over the global `fetch`. Here
 * it takes the fetch from `OAuthController`, so token requests go through the
 * Tauri HTTP plugin instead of the webview, which has no CORS grant for
 * `api.anthropic.com`.
 */
import { OAuthCallbackFlow } from "./callback-server"
import { generatePKCE } from "./pkce"
import type { OAuthController, OAuthCredentials } from "./types"

const decode = (s: string) => atob(s)
const CLIENT_ID = decode("OWQxYzI1MGEtZTYxYi00NGQ5LTg4ZWQtNTk0NGQxOTYyZjVl")
const AUTHORIZE_URL = "https://claude.ai/oauth/authorize"
const TOKEN_URL = "https://api.anthropic.com/v1/oauth/token"
const CALLBACK_PORT = 54545
const CALLBACK_PATH = "/callback"
const SCOPES = "org:create_api_key user:profile user:inference"

/**
 * Beta features Anthropic requires when the caller authenticates with an
 * OAuth subscription token rather than an API key. Source:
 * `packages/ai/src/providers/anthropic.ts` (`claudeCodeBetaDefaults`).
 * Sending a request without these is rejected.
 */
export const ANTHROPIC_OAUTH_BETAS = [
  "claude-code-20250219",
  "oauth-2025-04-20",
  "context-management-2025-06-27",
  "prompt-caching-scope-2026-01-05",
] as const

function formatErrorDetails(error: unknown): string {
  if (error instanceof Error) {
    const details: string[] = [`${error.name}: ${error.message}`]
    const withCode = error as Error & { code?: string; errno?: number | string }
    if (withCode.code) details.push(`code=${withCode.code}`)
    if (typeof withCode.errno !== "undefined") details.push(`errno=${String(withCode.errno)}`)
    if (typeof error.cause !== "undefined") details.push(`cause=${formatErrorDetails(error.cause)}`)
    return details.join("; ")
  }
  return String(error)
}

async function postJson(
  fetchImpl: typeof globalThis.fetch,
  url: string,
  body: Record<string, string | number>,
): Promise<string> {
  const response = await fetchImpl(url, {
    method: "POST",
    headers: {
      "Content-Type": "application/json",
      Accept: "application/json",
    },
    body: JSON.stringify(body),
    signal: AbortSignal.timeout(30_000),
  })

  const responseBody = await response.text()
  if (!response.ok) {
    throw new Error(`HTTP request failed. status=${response.status}; url=${url}; body=${responseBody}`)
  }
  return responseBody
}

/**
 * Shape of `/v1/oauth/token` for both the authorization_code exchange and the
 * refresh. The `account` block rides along, so identity needs no extra call.
 */
interface AnthropicTokenResponse {
  access_token: string
  refresh_token: string
  expires_in: number
  account?: { uuid?: string; email_address?: string }
}

function parseOAuthTokenResponse(responseBody: string, operation: string): AnthropicTokenResponse {
  try {
    return JSON.parse(responseBody) as AnthropicTokenResponse
  } catch (error) {
    throw new Error(
      `Anthropic ${operation} returned invalid JSON. url=${TOKEN_URL}; body=${responseBody}; details=${formatErrorDetails(error)}`,
      { cause: error },
    )
  }
}

function extractAccountFromTokenResponse(data: AnthropicTokenResponse): {
  accountId?: string
  email?: string
} {
  const accountUuid = data.account?.uuid
  const emailAddress = data.account?.email_address
  return {
    accountId: typeof accountUuid === "string" && accountUuid.length > 0 ? accountUuid : undefined,
    email: typeof emailAddress === "string" && emailAddress.length > 0 ? emailAddress : undefined,
  }
}

export class AnthropicOAuthFlow extends OAuthCallbackFlow {
  #verifier: string = ""
  #fetch: typeof globalThis.fetch

  constructor(ctrl: OAuthController) {
    super(ctrl, CALLBACK_PORT, CALLBACK_PATH)
    this.#fetch = ctrl.fetch ?? globalThis.fetch
  }

  async generateAuthUrl(state: string, redirectUri: string): Promise<{ url: string; instructions?: string }> {
    const pkce = await generatePKCE()
    this.#verifier = pkce.verifier

    const authParams = new URLSearchParams({
      code: "true",
      client_id: CLIENT_ID,
      response_type: "code",
      redirect_uri: redirectUri,
      scope: SCOPES,
      code_challenge: pkce.challenge,
      code_challenge_method: "S256",
      state,
    })

    return {
      url: `${AUTHORIZE_URL}?${authParams.toString()}`,
      instructions:
        "Complete login in your browser. If the browser cannot reach this machine, paste the final redirect URL or authorization code when prompted.",
    }
  }

  async exchangeToken(code: string, state: string, redirectUri: string): Promise<OAuthCredentials> {
    // A pasted code can arrive as `code#state`; the fragment wins over the
    // state we generated, because that is what the browser actually saw.
    let exchangeCode = code
    let exchangeState = state
    const codeFragmentIndex = code.indexOf("#")
    if (codeFragmentIndex >= 0) {
      exchangeCode = code.slice(0, codeFragmentIndex)
      const codeFragmentState = code.slice(codeFragmentIndex + 1)
      if (codeFragmentState.length > 0) exchangeState = codeFragmentState
    }

    let responseBody: string
    try {
      responseBody = await postJson(this.#fetch, TOKEN_URL, {
        grant_type: "authorization_code",
        client_id: CLIENT_ID,
        code: exchangeCode,
        state: exchangeState,
        redirect_uri: redirectUri,
        code_verifier: this.#verifier,
      })
    } catch (error) {
      throw new Error(
        `Token exchange request failed. url=${TOKEN_URL}; redirect_uri=${redirectUri}; response_type=authorization_code; details=${formatErrorDetails(error)}`,
        { cause: error },
      )
    }

    const tokenData = parseOAuthTokenResponse(responseBody, "token exchange")
    const { accountId, email } = extractAccountFromTokenResponse(tokenData)

    return {
      refresh: tokenData.refresh_token,
      access: tokenData.access_token,
      // Retire the token five minutes early so an in-flight request cannot
      // straddle the real expiry.
      expires: Date.now() + tokenData.expires_in * 1000 - 5 * 60 * 1000,
      accountId,
      email,
    }
  }
}

export async function loginAnthropic(ctrl: OAuthController): Promise<OAuthCredentials> {
  const flow = new AnthropicOAuthFlow(ctrl)
  return flow.login()
}

export async function refreshAnthropicToken(
  refreshToken: string,
  fetchImpl: typeof globalThis.fetch = globalThis.fetch,
): Promise<OAuthCredentials> {
  let responseBody: string
  try {
    responseBody = await postJson(fetchImpl, TOKEN_URL, {
      grant_type: "refresh_token",
      client_id: CLIENT_ID,
      refresh_token: refreshToken,
    })
  } catch (error) {
    throw new Error(
      `Anthropic token refresh request failed. url=${TOKEN_URL}; details=${formatErrorDetails(error)}`,
      { cause: error },
    )
  }

  const data = parseOAuthTokenResponse(responseBody, "token refresh")
  const { accountId, email } = extractAccountFromTokenResponse(data)

  return {
    // A refresh does not always mint a new refresh token; keep the old one.
    refresh: data.refresh_token || refreshToken,
    access: data.access_token,
    expires: Date.now() + data.expires_in * 1000 - 5 * 60 * 1000,
    accountId,
    email,
  }
}
