/**
 * Base class for OAuth flows that complete through a loopback redirect.
 *
 * Source: `packages/ai/src/utils/oauth/callback-server.ts` @ @sayknow-cli/ai
 * 0.5.20. The flow — state generation, port fallback, callback wait, manual
 * code race, redirect parsing — is preserved. Only the transport changed:
 * upstream calls `Bun.serve()` inline, which does not exist here, so the
 * socket lives in Rust (`src-tauri/src/oauth_callback.rs`) and reports back
 * over the `oauth:callback` event.
 *
 * Subclasses implement `generateAuthUrl` and `exchangeToken` exactly as they
 * do upstream, so a provider file can be copied across almost verbatim.
 */
import { invoke } from "@tauri-apps/api/core"
import { listen } from "@tauri-apps/api/event"
import type { OAuthController, OAuthCredentials } from "./types"

const DEFAULT_TIMEOUT = 300_000
const DEFAULT_HOSTNAME = "localhost"
const CALLBACK_PATH = "/callback"

export type CallbackResult = { code: string; state: string }

export interface OAuthCallbackFlowOptions {
  preferredPort: number
  callbackPath?: string
  callbackHostname?: string
  /** Exact redirect URI advertised to the provider; disables port fallback. */
  redirectUri?: string
}

/** Wire shape of the `oauth:callback` event emitted by the Rust listener. */
type CallbackEvent = {
  listener_id: number
  code: string | null
  state: string | null
  error: string | null
}

/** Copy for the page the browser lands on. Kept here so it stays localizable. */
export type CallbackPageCopy = {
  successHeading: string
  successDetail: string
  failureHeading: string
}

const DEFAULT_PAGE_COPY: CallbackPageCopy = {
  successHeading: "Signed in",
  successDetail: "You can close this tab and return to SayKnow Kit.",
  failureHeading: "Sign-in failed",
}

export abstract class OAuthCallbackFlow {
  ctrl: OAuthController
  preferredPort: number
  callbackPath: string
  callbackHostname: string
  redirectUri?: string
  pageCopy: CallbackPageCopy = DEFAULT_PAGE_COPY

  constructor(
    ctrl: OAuthController,
    preferredPortOrOptions: number | OAuthCallbackFlowOptions,
    callbackPath: string = CALLBACK_PATH,
  ) {
    this.ctrl = ctrl
    if (typeof preferredPortOrOptions === "number") {
      this.preferredPort = preferredPortOrOptions
      this.callbackPath = callbackPath
      this.callbackHostname = DEFAULT_HOSTNAME
      return
    }
    this.preferredPort = preferredPortOrOptions.preferredPort
    this.callbackPath = preferredPortOrOptions.callbackPath ?? CALLBACK_PATH
    this.callbackHostname = preferredPortOrOptions.callbackHostname ?? DEFAULT_HOSTNAME
    this.redirectUri = preferredPortOrOptions.redirectUri
  }

  /**
   * Build the provider authorization URL.
   * `redirectUri` is the one actually bound, which may differ from the
   * preferred port when that port was taken.
   */
  abstract generateAuthUrl(
    state: string,
    redirectUri: string,
  ): Promise<{ url: string; instructions?: string }>

  /** Trade the authorization code for tokens. */
  abstract exchangeToken(
    code: string,
    state: string,
    redirectUri: string,
  ): Promise<OAuthCredentials>

  /** CSRF state token. Override when a provider needs its own shape. */
  generateState(): string {
    const bytes = new Uint8Array(16)
    crypto.getRandomValues(bytes)
    return Array.from(bytes)
      .map((value) => value.toString(16).padStart(2, "0"))
      .join("")
  }

  async login(): Promise<OAuthCredentials> {
    const state = this.generateState()

    // Bind first: the real port decides the redirect URI we must advertise.
    const { listenerId, redirectUri } = await this.#startCallbackServer(state)

    try {
      const { url: authUrl, instructions } = await this.generateAuthUrl(state, redirectUri)

      this.ctrl.onAuth?.({ url: authUrl, instructions })
      this.ctrl.onProgress?.("Waiting for browser authentication...")

      const { code } = await this.#waitForCallback(state, listenerId)

      this.ctrl.onProgress?.("Exchanging authorization code for tokens...")

      return await this.exchangeToken(code, state, redirectUri)
    } finally {
      await invoke("oauth_callback_stop").catch(() => {})
    }
  }

  async #startCallbackServer(
    expectedState: string,
  ): Promise<{ listenerId: number; redirectUri: string }> {
    const [listenerId, port] = await invoke<[number, number]>("oauth_callback_start", {
      preferredPort: this.preferredPort,
      callbackPath: this.callbackPath,
      expectedState,
      // A provider that registered an exact redirect URI cannot accept a
      // different port, so the native side must fail instead of falling back.
      allowPortFallback: !this.redirectUri,
      successHeading: this.pageCopy.successHeading,
      successDetail: this.pageCopy.successDetail,
      failureHeading: this.pageCopy.failureHeading,
    })

    if (this.redirectUri) return { listenerId, redirectUri: this.redirectUri }

    if (port !== this.preferredPort) {
      this.ctrl.onProgress?.(
        `Preferred port ${this.preferredPort} unavailable, using port ${port}`,
      )
    }
    return {
      listenerId,
      redirectUri: `http://${this.callbackHostname}:${port}${this.callbackPath}`,
    }
  }

  /** Resolve on the redirect, or on a pasted URL/code when the host offers it. */
  #waitForCallback(expectedState: string, listenerId: number): Promise<CallbackResult> {
    const timeoutSignal = AbortSignal.timeout(DEFAULT_TIMEOUT)
    const signal = this.ctrl.signal
      ? AbortSignal.any([this.ctrl.signal, timeoutSignal])
      : timeoutSignal

    let unlisten: (() => void) | undefined
    const callbackPromise = new Promise<CallbackResult>((resolve, reject) => {
      signal.addEventListener("abort", () => {
        reject(new Error(`OAuth callback cancelled: ${signal.reason}`))
      })

      void listen<CallbackEvent>("oauth:callback", (event) => {
        // A listener from a previous, abandoned attempt must not resolve this
        // one — the native side stamps every event with its own id.
        if (event.payload.listener_id !== listenerId) return
        if (event.payload.error) {
          reject(new Error(event.payload.error))
          return
        }
        if (!event.payload.code) {
          reject(new Error("Missing authorization code"))
          return
        }
        resolve({ code: event.payload.code, state: event.payload.state ?? "" })
      })
        .then((fn) => {
          unlisten = fn
          if (signal.aborted) fn()
        })
        .catch(reject)
    }).finally(() => unlisten?.())

    if (!this.ctrl.onManualCodeInput) return callbackPromise

    const requestManualInput = this.ctrl.onManualCodeInput
    const manualPromise = (async (): Promise<CallbackResult> => {
      while (true) {
        const result = await Promise.race([
          callbackPromise,
          requestManualInput()
            .then((input): CallbackResult | null => {
              const parsed = parseCallbackInput(input)
              if (!parsed.code) return null
              if (expectedState && parsed.state && parsed.state !== expectedState) return null
              return { code: parsed.code, state: parsed.state ?? "" }
            })
            .catch((): CallbackResult | null => null),
        ])
        if (result) return result
      }
    })()

    return Promise.race([callbackPromise, manualPromise])
  }
}

/**
 * Pull `code`/`state` out of a pasted redirect URL, query fragment, or bare
 * code. Unchanged from upstream.
 */
export function parseCallbackInput(input: string): { code?: string; state?: string } {
  const value = input.trim()
  if (!value) return {}

  try {
    const url = new URL(value)
    return {
      code: url.searchParams.get("code") ?? undefined,
      state: url.searchParams.get("state") ?? undefined,
    }
  } catch {
    // Not a URL - check for query string format
  }

  if (value.includes("code=")) {
    const params = new URLSearchParams(value.replace(/^[?#]/, ""))
    return {
      code: params.get("code") ?? undefined,
      state: params.get("state") ?? undefined,
    }
  }

  // Assume raw code, possibly with state after #
  const [code, state] = value.split("#", 2)
  return { code, state }
}
