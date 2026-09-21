/**
 * Shared authorization-code flow for Google-style providers.
 *
 * Source: `packages/ai/src/utils/oauth/google-oauth-shared.ts` @
 * @sayknow-cli/ai 0.5.20. Parameters, `access_type=offline`,
 * `prompt=consent`, the refresh-token requirement, and the five-minute expiry
 * margin are unchanged.
 *
 * Adaptation: every request takes the fetch from `OAuthController`, so token
 * and userinfo calls go through the Tauri HTTP plugin.
 */
import { OAuthCallbackFlow } from "./callback-server"
import type { OAuthController, OAuthCredentials } from "./types"

export interface GoogleOAuthFlowConfig {
  clientId: string
  clientSecret: string
  authUrl: string
  tokenUrl: string
  scopes: string[]
  callbackPort: number
  callbackPath: string
  discoverProject: (
    accessToken: string,
    fetchImpl: typeof globalThis.fetch,
    onProgress?: (message: string) => void,
  ) => Promise<string>
}

/** Email is a nice-to-have; a failure here must not fail the login. */
async function getUserEmail(
  accessToken: string,
  fetchImpl: typeof globalThis.fetch,
): Promise<string | undefined> {
  try {
    const response = await fetchImpl("https://www.googleapis.com/oauth2/v1/userinfo?alt=json", {
      headers: { Authorization: `Bearer ${accessToken}` },
    })
    if (response.ok) {
      const data = (await response.json()) as { email?: string }
      return data.email
    }
  } catch {
    // Ignore errors, email is optional
  }
  return undefined
}

export class GoogleOAuthFlow extends OAuthCallbackFlow {
  readonly #config: GoogleOAuthFlowConfig
  readonly #fetch: typeof globalThis.fetch

  constructor(ctrl: OAuthController, config: GoogleOAuthFlowConfig) {
    super(ctrl, config.callbackPort, config.callbackPath)
    this.#config = config
    this.#fetch = ctrl.fetch ?? globalThis.fetch
  }

  async generateAuthUrl(state: string, redirectUri: string): Promise<{ url: string; instructions?: string }> {
    const authParams = new URLSearchParams({
      client_id: this.#config.clientId,
      response_type: "code",
      redirect_uri: redirectUri,
      scope: this.#config.scopes.join(" "),
      state,
      // Offline access plus a forced consent screen is what makes Google
      // return a refresh token; without both, re-logins come back without one.
      access_type: "offline",
      prompt: "consent",
    })

    return {
      url: `${this.#config.authUrl}?${authParams.toString()}`,
      instructions: "Complete the sign-in in your browser.",
    }
  }

  async exchangeToken(code: string, _state: string, redirectUri: string): Promise<OAuthCredentials> {
    this.ctrl.onProgress?.("Exchanging authorization code for tokens...")

    const tokenResponse = await this.#fetch(this.#config.tokenUrl, {
      method: "POST",
      headers: { "Content-Type": "application/x-www-form-urlencoded" },
      body: new URLSearchParams({
        client_id: this.#config.clientId,
        client_secret: this.#config.clientSecret,
        code,
        grant_type: "authorization_code",
        redirect_uri: redirectUri,
      }),
    })

    if (!tokenResponse.ok) {
      throw new Error(`Token exchange failed: ${await tokenResponse.text()}`)
    }

    const tokenData = (await tokenResponse.json()) as {
      access_token: string
      refresh_token: string
      expires_in: number
    }

    if (!tokenData.refresh_token) {
      throw new Error("No refresh token received. Please try again.")
    }

    this.ctrl.onProgress?.("Getting user info...")
    const email = await getUserEmail(tokenData.access_token, this.#fetch)
    const projectId = await this.#config.discoverProject(
      tokenData.access_token,
      this.#fetch,
      this.ctrl.onProgress,
    )

    return {
      refresh: tokenData.refresh_token,
      access: tokenData.access_token,
      expires: Date.now() + tokenData.expires_in * 1000 - 5 * 60 * 1000,
      projectId,
      email,
    }
  }
}

export async function runGoogleOAuthLogin(
  ctrl: OAuthController,
  config: GoogleOAuthFlowConfig,
): Promise<OAuthCredentials> {
  return new GoogleOAuthFlow(ctrl, config).login()
}
