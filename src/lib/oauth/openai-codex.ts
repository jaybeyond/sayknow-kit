/**
 * OpenAI Codex (ChatGPT OAuth) — browser and device-code flows.
 *
 * Source: `packages/ai/src/utils/oauth/openai-codex.ts` @ @sayknow-cli/ai
 * 0.5.20. Client id, endpoints, scopes, the fixed callback port, the JWT
 * claim paths, and the device-poll timing are unchanged.
 *
 * Adaptations:
 * - `Bun.sleep` becomes the shared abortable `delay`, so cancelling no longer
 *   waits out a full poll interval.
 * - `Buffer`-based JWT decoding moves to the shared `decodeJwt` helper.
 * - Network calls take the fetch from `OAuthController` (Tauri HTTP plugin).
 */
import { decodeJwt } from "./base64"
import { OAuthCallbackFlow, type OAuthCallbackFlowOptions } from "./callback-server"
import { delay } from "./delay"
import { generatePKCE } from "./pkce"
import type { OAuthController, OAuthCredentials } from "./types"

const CLIENT_ID = "app_EMoamEEZ73f0CkXaXp7hrann"
const AUTHORIZE_URL = "https://auth.openai.com/oauth/authorize"
const TOKEN_URL = "https://auth.openai.com/oauth/token"
const CALLBACK_PORT = 1455
const CALLBACK_PATH = "/auth/callback"
const SCOPE = "openid profile email offline_access"
const JWT_CLAIM_PATH = "https://api.openai.com/auth"
const JWT_PROFILE_CLAIM = "https://api.openai.com/profile"
const TOKEN_REQUEST_TIMEOUT_MS = 15_000
const DEVICE_USERCODE_URL = "https://auth.openai.com/api/accounts/deviceauth/usercode"
const DEVICE_TOKEN_URL = "https://auth.openai.com/api/accounts/deviceauth/token"
const DEVICE_REDIRECT_URI = "https://auth.openai.com/deviceauth/callback"
const DEVICE_AUTH_URL = "https://auth.openai.com/codex/device"
const DEVICE_POLL_INTERVAL_MS = 5_000
const DEVICE_POLL_SAFETY_MARGIN_MS = 3_000
/** Upper bound on device-code polling to avoid infinite loops on server errors. */
const DEVICE_MAX_POLLS = 120

/**
 * Where a Codex chat request goes once the token is in hand. Source:
 * `packages/ai/src/providers/openai-codex/constants.ts`. This is the Responses
 * API on the ChatGPT backend, not the public `api.openai.com` surface.
 */
export const CODEX_BASE_URL = "https://chatgpt.com/backend-api"
export const CODEX_RESPONSES_PATH = "/codex/responses"
export const CODEX_HEADERS = {
  BETA: "OpenAI-Beta",
  ACCOUNT_ID: "chatgpt-account-id",
  ORIGINATOR: "originator",
} as const
export const CODEX_BETA_RESPONSES = "responses=experimental"

type JwtPayload = {
  [JWT_CLAIM_PATH]?: { chatgpt_account_id?: string }
  [JWT_PROFILE_CLAIM]?: { email?: string }
  [key: string]: unknown
}


function getTokenProfile(accessToken: string): { accountId?: string; email?: string } {
  const payload = decodeJwt<JwtPayload>(accessToken)
  const accountId = payload?.[JWT_CLAIM_PATH]?.chatgpt_account_id
  const email = payload?.[JWT_PROFILE_CLAIM]?.email?.trim().toLowerCase()
  return {
    accountId: typeof accountId === "string" && accountId.length > 0 ? accountId : undefined,
    email: typeof email === "string" && email.length > 0 ? email : undefined,
  }
}

interface PKCE {
  verifier: string
  challenge: string
}

class OpenAICodexOAuthFlow extends OAuthCallbackFlow {
  readonly #pkce: PKCE
  readonly #originator: string

  constructor(ctrl: OAuthController, pkce: PKCE, originator: string) {
    super(ctrl, {
      preferredPort: CALLBACK_PORT,
      callbackPath: CALLBACK_PATH,
      // OpenAI only allowlists http://localhost:1455/auth/callback. Falling
      // back to a random port would make the exchange fail with 403 because
      // the redirect_uri no longer matches, so pin it and fail loudly.
      redirectUri: `http://localhost:${CALLBACK_PORT}${CALLBACK_PATH}`,
    } satisfies OAuthCallbackFlowOptions)
    this.#pkce = pkce
    this.#originator = originator
  }

  async generateAuthUrl(state: string, redirectUri: string): Promise<{ url: string; instructions?: string }> {
    const searchParams = new URLSearchParams({
      response_type: "code",
      client_id: CLIENT_ID,
      redirect_uri: redirectUri,
      scope: SCOPE,
      code_challenge: this.#pkce.challenge,
      code_challenge_method: "S256",
      state,
      id_token_add_organizations: "true",
      codex_cli_simplified_flow: "true",
      originator: this.#originator,
    })

    return {
      url: `${AUTHORIZE_URL}?${searchParams.toString()}`,
      instructions: "A browser window should open. Complete login to finish.",
    }
  }

  async exchangeToken(code: string, _state: string, redirectUri: string): Promise<OAuthCredentials> {
    return exchangeCodeForToken(
      code,
      this.#pkce.verifier,
      redirectUri,
      this.ctrl.fetch ?? globalThis.fetch,
    )
  }
}

async function readErrorDetail(response: Response): Promise<string> {
  let detail = `${response.status}`
  try {
    const body = (await response.json()) as { error?: string; error_description?: string }
    if (body.error) {
      detail = `${response.status} ${body.error}${
        body.error_description ? `: ${body.error_description}` : ""
      }`
    }
  } catch {
    // Non-JSON error body; the status alone is the best we have.
  }
  return detail
}

async function exchangeCodeForToken(
  code: string,
  verifier: string,
  redirectUri: string,
  fetchImpl: typeof globalThis.fetch,
): Promise<OAuthCredentials> {
  const tokenResponse = await fetchImpl(TOKEN_URL, {
    method: "POST",
    headers: { "Content-Type": "application/x-www-form-urlencoded" },
    body: new URLSearchParams({
      grant_type: "authorization_code",
      client_id: CLIENT_ID,
      code,
      code_verifier: verifier,
      redirect_uri: redirectUri,
    }),
    signal: AbortSignal.timeout(TOKEN_REQUEST_TIMEOUT_MS),
  })

  if (!tokenResponse.ok) {
    throw new Error(`Token exchange failed: ${await readErrorDetail(tokenResponse)}`)
  }

  const tokenData = (await tokenResponse.json()) as {
    access_token?: string
    refresh_token?: string
    expires_in?: number
  }

  if (!tokenData.access_token || !tokenData.refresh_token || typeof tokenData.expires_in !== "number") {
    throw new Error("Token response missing required fields")
  }

  const { accountId, email } = getTokenProfile(tokenData.access_token)
  // Every Codex request carries `chatgpt-account-id`, so a token we cannot
  // attribute to an account is unusable — fail here rather than at send time.
  if (!accountId) {
    throw new Error("Failed to extract accountId from token")
  }

  return {
    access: tokenData.access_token,
    refresh: tokenData.refresh_token,
    expires: Date.now() + tokenData.expires_in * 1000,
    accountId,
    email,
  }
}

export type OpenAICodexLoginOptions = OAuthController & {
  /** Originator value sent to OpenAI. Default: "opencode". */
  originator?: string
}

export async function loginOpenAICodex(options: OpenAICodexLoginOptions): Promise<OAuthCredentials> {
  const pkce = await generatePKCE()
  const originator = options.originator?.trim() || "opencode"
  return new OpenAICodexOAuthFlow(options, pkce, originator).login()
}

/**
 * Device-code login. No local listener, so this still works when port 1455 is
 * taken or the browser cannot reach this machine.
 */
export async function loginOpenAICodexDevice(ctrl: OAuthController): Promise<OAuthCredentials> {
  const fetchImpl = ctrl.fetch ?? globalThis.fetch
  ctrl.onProgress?.("Initiating device authorization…")

  const initResponse = await fetchImpl(DEVICE_USERCODE_URL, {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify({ client_id: CLIENT_ID }),
    signal: AbortSignal.timeout(TOKEN_REQUEST_TIMEOUT_MS),
  })

  if (!initResponse.ok) {
    throw new Error(`Device authorization initiation failed: ${initResponse.status}`)
  }

  const initData = (await initResponse.json()) as {
    device_auth_id?: string
    user_code?: string
    interval?: string | number
  }

  if (!initData.device_auth_id || !initData.user_code) {
    throw new Error("Device authorization response missing required fields")
  }

  const userCode = initData.user_code
  const pollIntervalMs =
    (typeof initData.interval === "number"
      ? initData.interval
      : parseInt(String(initData.interval ?? "5"), 10) || 5) *
      1000 +
    DEVICE_POLL_SAFETY_MARGIN_MS

  ctrl.onAuth?.({ url: DEVICE_AUTH_URL, instructions: `Enter code: ${userCode}` })
  ctrl.onProgress?.(`Waiting for browser authorization (code: ${userCode})…`)

  for (let poll = 0; poll < DEVICE_MAX_POLLS; poll++) {
    await delay(
      poll === 0 ? Math.min(pollIntervalMs, DEVICE_POLL_INTERVAL_MS) : pollIntervalMs,
      ctrl.signal,
    )

    if (ctrl.signal?.aborted) {
      throw new Error("Device authorization cancelled")
    }

    const pollResponse = await fetchImpl(DEVICE_TOKEN_URL, {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify({ device_auth_id: initData.device_auth_id, user_code: userCode }),
      signal: AbortSignal.timeout(TOKEN_REQUEST_TIMEOUT_MS),
    })

    // 403/404 = authorization pending, keep polling
    if (pollResponse.status === 403 || pollResponse.status === 404) continue

    if (!pollResponse.ok) {
      throw new Error(`Device token polling failed: ${pollResponse.status}`)
    }

    const pollData = (await pollResponse.json()) as {
      authorization_code?: string
      code_verifier?: string
    }

    if (!pollData.authorization_code || !pollData.code_verifier) {
      throw new Error("Device token response missing authorization_code or code_verifier")
    }

    ctrl.onProgress?.("Exchanging authorization code for tokens…")
    return exchangeCodeForToken(
      pollData.authorization_code,
      pollData.code_verifier,
      DEVICE_REDIRECT_URI,
      fetchImpl,
    )
  }

  throw new Error("Device authorization timed out — user did not complete login in time")
}

export async function refreshOpenAICodexToken(
  refreshToken: string,
  fetchImpl: typeof globalThis.fetch = globalThis.fetch,
): Promise<OAuthCredentials> {
  const response = await fetchImpl(TOKEN_URL, {
    method: "POST",
    headers: { "Content-Type": "application/x-www-form-urlencoded" },
    body: new URLSearchParams({
      grant_type: "refresh_token",
      refresh_token: refreshToken,
      client_id: CLIENT_ID,
    }),
    signal: AbortSignal.timeout(TOKEN_REQUEST_TIMEOUT_MS),
  })

  if (!response.ok) {
    throw new Error(`OpenAI Codex token refresh failed: ${await readErrorDetail(response)}`)
  }

  const tokenData = (await response.json()) as {
    access_token?: string
    refresh_token?: string
    expires_in?: number
  }

  if (!tokenData.access_token || !tokenData.refresh_token || typeof tokenData.expires_in !== "number") {
    throw new Error("Token response missing required fields")
  }

  const { accountId, email } = getTokenProfile(tokenData.access_token)

  return {
    access: tokenData.access_token,
    refresh: tokenData.refresh_token || refreshToken,
    expires: Date.now() + tokenData.expires_in * 1000,
    accountId: accountId ?? undefined,
    email,
  }
}
