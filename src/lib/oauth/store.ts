/**
 * Persistence for OAuth credentials, on top of the app's existing Keychain
 * wrapper.
 *
 * Deliberately not a new storage layer: `namedSecret` already gives us a
 * per-account Keychain slot, serialized writes, and a browser fallback. Each
 * provider gets its own account for the same reason DeepL has one — signing
 * out of a provider, or of the API-key provider, must not take the others
 * with it.
 *
 * These are credentials this app obtained itself through its own OAuth flow.
 * It never reads another CLI's credential store; see the comment on
 * `codex_auth` in `src-tauri/src/agent_usage.rs` for that boundary.
 */
import { namedSecret } from "../secrets"
import { storage } from "../storage"
import type { OAuthCredentials, OAuthProvider } from "./types"

/** Keychain account per provider. Namespaced so it cannot collide with
 *  `deepl_api_key` or any future plain secret. */
export function oauthAccount(provider: OAuthProvider): string {
  return `oauth_${provider}`
}

/** Bumped on every write so other windows re-read via the `storage` event. */
export const OAUTH_REV_KEY = "oauth:rev"

function isCredentials(value: unknown): value is OAuthCredentials {
  if (!value || typeof value !== "object") return false
  const candidate = value as Partial<OAuthCredentials>
  return (
    typeof candidate.access === "string" &&
    typeof candidate.refresh === "string" &&
    typeof candidate.expires === "number"
  )
}

export const oauthStore = {
  /**
   * Stored credentials, or `null` when absent or unreadable.
   *
   * A corrupt blob returns `null` rather than throwing: the caller's only
   * sane response is to ask the user to sign in again, and that is also what
   * "no credentials" means.
   */
  async get(provider: OAuthProvider): Promise<OAuthCredentials | null> {
    const raw = await namedSecret.get(oauthAccount(provider))
    if (!raw) return null
    try {
      const parsed: unknown = JSON.parse(raw)
      return isCredentials(parsed) ? parsed : null
    } catch {
      return null
    }
  },

  async set(provider: OAuthProvider, credentials: OAuthCredentials): Promise<void> {
    await namedSecret.set(oauthAccount(provider), JSON.stringify(credentials))
    storage.set(OAUTH_REV_KEY, Date.now())
  },

  async clear(provider: OAuthProvider): Promise<void> {
    await namedSecret.clear(oauthAccount(provider))
    storage.set(OAUTH_REV_KEY, Date.now())
  },
}

/**
 * Whether the stored token is past its usable life.
 *
 * `expires` is already written with a provider-specific safety margin baked
 * in by the flow that minted it, so this is a plain comparison.
 */
export function isExpired(credentials: OAuthCredentials, now = Date.now()): boolean {
  return credentials.expires <= now
}
