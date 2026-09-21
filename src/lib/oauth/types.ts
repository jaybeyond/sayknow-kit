/**
 * OAuth contracts, ported from sayknow-cli.
 *
 * Source: `packages/ai/src/utils/oauth/types.ts` @ @sayknow-cli/ai 0.5.20.
 * Kept structurally identical so a provider flow can be copied across with
 * only its runtime dependencies swapped. The provider union is narrowed to
 * what this app actually ships; the `(string & {})` escape hatch upstream
 * uses is preserved on `OAuthProviderId`.
 */

/** Credentials as returned by every provider flow. This is what we persist. */
export type OAuthCredentials = {
  refresh: string
  access: string
  /** Absolute epoch milliseconds. */
  expires: number
  enterpriseUrl?: string
  projectId?: string
  email?: string
  accountId?: string
}

/** Providers this app supports. Upstream carries ~55; we ship five. */
export type OAuthProvider =
  | "anthropic"
  | "openai-codex"
  | "google-gemini-cli"
  | "xai"
  | "cursor"

export type OAuthProviderId = OAuthProvider | (string & {})

export type OAuthPrompt = {
  message: string
  placeholder?: string
  allowEmpty?: boolean
}

export type OAuthAuthInfo = {
  url: string
  instructions?: string
}

export interface OAuthProviderInfo {
  id: OAuthProviderId
  name: string
  available: boolean
}

/**
 * Host hooks handed to a flow.
 *
 * `fetch` is the reason this port is cheap: the app injects the Tauri HTTP
 * plugin's fetch here, so provider token endpoints are reached from Rust and
 * never hit the webview's CORS policy.
 */
export interface OAuthController {
  onAuth?(info: OAuthAuthInfo): void
  onProgress?(message: string): void
  onManualCodeInput?(): Promise<string>
  onPrompt?(prompt: OAuthPrompt): Promise<string>
  signal?: AbortSignal
  fetch?: typeof globalThis.fetch
}

export interface OAuthLoginCallbacks extends OAuthController {
  onAuth: (info: OAuthAuthInfo) => void
  onPrompt: (prompt: OAuthPrompt) => Promise<string>
}

export interface OAuthProviderInterface {
  readonly id: OAuthProviderId
  readonly name: string
  readonly sourceId?: string
  login(callbacks: OAuthLoginCallbacks): Promise<OAuthCredentials | string>
  refreshToken?(credentials: OAuthCredentials): Promise<OAuthCredentials>
  getApiKey?(credentials: OAuthCredentials): string
}
