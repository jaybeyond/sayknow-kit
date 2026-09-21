/**
 * Gemini CLI OAuth flow (Google Cloud Code Assist).
 *
 * Source: `packages/ai/src/utils/oauth/google-gemini-cli.ts` @ @sayknow-cli/ai
 * 0.5.20. Client credentials, scopes, callback port, the tier rules, the
 * VPC-SC special case, and the onboarding long-running-operation poll are
 * unchanged.
 *
 * Adaptations:
 * - `Bun.sleep` becomes the shared abortable `delay`.
 * - `$env.GOOGLE_CLOUD_PROJECT` does not exist in a webview. Upstream reads
 *   the environment; here the caller passes the project id explicitly via
 *   `setGeminiProjectOverride`, and the error messages point at that setting
 *   rather than at an environment variable the user cannot set.
 * - Requests take the injected fetch.
 */
import { delay } from "./delay"
import { getGeminiCliHeaders } from "./google-gemini-headers"
import { runGoogleOAuthLogin } from "./google-oauth-shared"
import type { OAuthController, OAuthCredentials } from "./types"

const decode = (s: string) => atob(s)
const CLIENT_ID = decode(
  "NjgxMjU1ODA5Mzk1LW9vOGZ0Mm9wcmRybnA5ZTNhcWY2YXYzaG1kaWIxMzVqLmFwcHMuZ29vZ2xldXNlcmNvbnRlbnQuY29t",
)
const CLIENT_SECRET = decode("R09DU1BYLTR1SGdNUG0tMW83U2stZ2VWNkN1NWNsWEZzeGw=")
const CALLBACK_PORT = 8085
const CALLBACK_PATH = "/oauth2callback"
const SCOPES = [
  "https://www.googleapis.com/auth/cloud-platform",
  "https://www.googleapis.com/auth/userinfo.email",
  "https://www.googleapis.com/auth/userinfo.profile",
]
const AUTH_URL = "https://accounts.google.com/o/oauth2/v2/auth"
const TOKEN_URL = "https://oauth2.googleapis.com/token"
const CODE_ASSIST_ENDPOINT = "https://cloudcode-pa.googleapis.com"

const PROJECT_REQUIRED_MESSAGE =
  "This Google account requires a Cloud project id. Set it in SayKnow Kit's Gemini settings. " +
  "See https://goo.gle/gemini-cli-auth-docs#workspace-gca"

/**
 * Stands in for upstream's `GOOGLE_CLOUD_PROJECT` / `GOOGLE_CLOUD_PROJECT_ID`
 * environment variables. Set from settings before starting a login.
 */
let projectOverride: string | undefined

export function setGeminiProjectOverride(projectId: string | undefined): void {
  projectOverride = projectId?.trim() || undefined
}

interface LoadCodeAssistPayload {
  cloudaicompanionProject?: string
  currentTier?: { id?: string }
  allowedTiers?: Array<{ id?: string; isDefault?: boolean }>
}

interface LongRunningOperationResponse {
  name?: string
  done?: boolean
  response?: { cloudaicompanionProject?: { id?: string } }
}

const TIER_FREE = "free-tier"
const TIER_LEGACY = "legacy-tier"
const TIER_STANDARD = "standard-tier"

interface GoogleRpcErrorResponse {
  error?: { details?: Array<{ reason?: string }> }
}

function getDefaultTier(
  allowedTiers?: Array<{ id?: string; isDefault?: boolean }>,
): { id?: string } {
  if (!allowedTiers || allowedTiers.length === 0) return { id: TIER_LEGACY }
  return allowedTiers.find((t) => t.isDefault) ?? { id: TIER_LEGACY }
}

/**
 * A VPC Service Controls org rejects `loadCodeAssist` outright. That is not a
 * failed login — the account is simply on the standard tier.
 */
function isVpcScAffectedUser(payload: unknown): boolean {
  if (!payload || typeof payload !== "object") return false
  if (!("error" in payload)) return false
  const error = (payload as GoogleRpcErrorResponse).error
  if (!error?.details || !Array.isArray(error.details)) return false
  return error.details.some((detail) => detail.reason === "SECURITY_POLICY_VIOLATED")
}

async function pollOperation(
  operationName: string,
  headers: Record<string, string>,
  fetchImpl: typeof globalThis.fetch,
  onProgress?: (message: string) => void,
): Promise<LongRunningOperationResponse> {
  let attempt = 0
  while (true) {
    if (attempt > 0) {
      onProgress?.(`Waiting for project provisioning (attempt ${attempt + 1})...`)
      await delay(5000)
    }

    const response = await fetchImpl(`${CODE_ASSIST_ENDPOINT}/v1internal/${operationName}`, {
      method: "GET",
      headers,
    })

    if (!response.ok) {
      throw new Error(`Failed to poll operation: ${response.status} ${response.statusText}`)
    }

    const data = (await response.json()) as LongRunningOperationResponse
    if (data.done) return data

    attempt += 1
  }
}

async function discoverProject(
  accessToken: string,
  fetchImpl: typeof globalThis.fetch,
  onProgress?: (message: string) => void,
): Promise<string> {
  const envProjectId = projectOverride

  const headers = {
    Authorization: `Bearer ${accessToken}`,
    "Content-Type": "application/json",
    ...getGeminiCliHeaders(),
  }

  onProgress?.("Checking for existing Cloud Code Assist project...")
  const loadResponse = await fetchImpl(`${CODE_ASSIST_ENDPOINT}/v1internal:loadCodeAssist`, {
    method: "POST",
    headers,
    body: JSON.stringify({
      cloudaicompanionProject: envProjectId,
      metadata: {
        ideType: "IDE_UNSPECIFIED",
        platform: "PLATFORM_UNSPECIFIED",
        pluginType: "GEMINI",
        duetProject: envProjectId,
      },
    }),
  })

  let data: LoadCodeAssistPayload

  if (!loadResponse.ok) {
    // Read the body exactly once. Upstream does `clone().json()` and then
    // `text()`, which works on a browser Response but not through the Tauri
    // HTTP plugin: the body is a native resource handle, so the second read
    // fails with "The resource id ... is invalid" and sign-in dies here.
    const errorText = await loadResponse.text().catch(() => "")
    let errorPayload: unknown
    try {
      errorPayload = JSON.parse(errorText)
    } catch {
      errorPayload = undefined
    }

    if (isVpcScAffectedUser(errorPayload)) {
      data = { currentTier: { id: TIER_STANDARD } }
    } else {
      throw new Error(
        `loadCodeAssist failed: ${loadResponse.status} ${loadResponse.statusText}: ${errorText}`,
      )
    }
  } else {
    data = (await loadResponse.json()) as LoadCodeAssistPayload
  }

  if (data.currentTier) {
    if (data.cloudaicompanionProject) return data.cloudaicompanionProject
    if (envProjectId) return envProjectId
    throw new Error(PROJECT_REQUIRED_MESSAGE)
  }

  const tierId = getDefaultTier(data.allowedTiers)?.id ?? TIER_FREE

  // Only the free tier gets a project provisioned for it.
  if (tierId !== TIER_FREE && !envProjectId) {
    throw new Error(PROJECT_REQUIRED_MESSAGE)
  }

  onProgress?.("Provisioning Cloud Code Assist project (this may take a moment)...")

  const onboardBody: Record<string, unknown> = {
    tierId,
    metadata: {
      ideType: "IDE_UNSPECIFIED",
      platform: "PLATFORM_UNSPECIFIED",
      pluginType: "GEMINI",
    },
  }

  if (tierId !== TIER_FREE && envProjectId) {
    onboardBody.cloudaicompanionProject = envProjectId
    ;(onboardBody.metadata as Record<string, unknown>).duetProject = envProjectId
  }

  const onboardResponse = await fetchImpl(`${CODE_ASSIST_ENDPOINT}/v1internal:onboardUser`, {
    method: "POST",
    headers,
    body: JSON.stringify(onboardBody),
  })

  if (!onboardResponse.ok) {
    const errorText = await onboardResponse.text()
    throw new Error(
      `onboardUser failed: ${onboardResponse.status} ${onboardResponse.statusText}: ${errorText}`,
    )
  }

  let lroData = (await onboardResponse.json()) as LongRunningOperationResponse

  if (!lroData.done && lroData.name) {
    lroData = await pollOperation(lroData.name, headers, fetchImpl, onProgress)
  }

  const projectId = lroData.response?.cloudaicompanionProject?.id
  if (projectId) return projectId
  if (envProjectId) return envProjectId

  throw new Error(`Could not discover or provision a Google Cloud project. ${PROJECT_REQUIRED_MESSAGE}`)
}

export async function loginGeminiCli(ctrl: OAuthController): Promise<OAuthCredentials> {
  return runGoogleOAuthLogin(ctrl, {
    clientId: CLIENT_ID,
    clientSecret: CLIENT_SECRET,
    authUrl: AUTH_URL,
    tokenUrl: TOKEN_URL,
    scopes: SCOPES,
    callbackPort: CALLBACK_PORT,
    callbackPath: CALLBACK_PATH,
    discoverProject,
  })
}

export async function refreshGoogleCloudToken(
  refreshToken: string,
  projectId: string,
  fetchImpl: typeof globalThis.fetch = globalThis.fetch,
): Promise<OAuthCredentials> {
  const response = await fetchImpl(TOKEN_URL, {
    method: "POST",
    headers: { "Content-Type": "application/x-www-form-urlencoded" },
    body: new URLSearchParams({
      client_id: CLIENT_ID,
      client_secret: CLIENT_SECRET,
      refresh_token: refreshToken,
      grant_type: "refresh_token",
    }),
  })

  if (!response.ok) {
    throw new Error(`Google Cloud token refresh failed: ${await response.text()}`)
  }

  const data = (await response.json()) as {
    access_token: string
    expires_in: number
    refresh_token?: string
  }

  return {
    // Google usually omits the refresh token on a refresh; keep the old one.
    refresh: data.refresh_token || refreshToken,
    access: data.access_token,
    expires: Date.now() + data.expires_in * 1000 - 5 * 60 * 1000,
    projectId,
  }
}
