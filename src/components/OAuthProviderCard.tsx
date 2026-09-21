import { useCallback, useEffect, useRef, useState } from "react"
import { Check, Loader2, LogIn, LogOut } from "lucide-react"
import { Button } from "@/components/ui/button"
import { useT, type UILocaleSetting } from "@/i18n"
import { ensureAccessToken, OAUTH_PROVIDERS, signIn, signOut } from "@/lib/oauth/registry"
import { OAUTH_MODELS } from "@/lib/oauth/models"
import type { OAuthProvider } from "@/lib/oauth/types"
import { openExternal } from "@/lib/runtime"
import { cn } from "@/lib/utils"

type Props = {
  provider: OAuthProvider
  active: boolean
  uiLocale: UILocaleSetting
  onSelect: () => void
  /** Raised after a sign-in or sign-out so settings can re-derive login state. */
  onAuthChanged: () => void
}

type State =
  | { kind: "checking" }
  | { kind: "signed-in"; email?: string }
  | { kind: "signed-out" }
  | { kind: "working"; message: string }
  | { kind: "error"; message: string }

/**
 * One provider row: shows whether we hold a usable token, and runs the
 * browser sign-in.
 *
 * The flow opens the provider's page in the user's real browser and waits on
 * the loopback listener in Rust. Nothing here ever sees a password, and the
 * tokens go straight to the Keychain via the store.
 */
export function OAuthProviderCard({
  provider,
  active,
  uiLocale,
  onSelect,
  onAuthChanged,
}: Props) {
  const { t } = useT(uiLocale)
  const definition = OAUTH_PROVIDERS[provider]
  const [state, setState] = useState<State>({ kind: "checking" })
  // Lets an in-flight login be cancelled when the card unmounts.
  const abortRef = useRef<AbortController | null>(null)

  // Resolves to the card's state rather than setting it, so the effect below
  // only ever writes from inside an async callback.
  const resolveState = useCallback(async (): Promise<State> => {
    try {
      const result = await ensureAccessToken(provider)
      if (result.status === "ready") {
        return { kind: "signed-in", email: result.credentials.email }
      }
      if (result.status === "signed-out") return { kind: "signed-out" }
      return { kind: "error", message: result.reason }
    } catch {
      return { kind: "signed-out" }
    }
  }, [provider])

  useEffect(() => {
    let cancelled = false
    void resolveState().then((next) => {
      if (!cancelled) setState(next)
    })
    return () => {
      cancelled = true
    }
  }, [resolveState])

  useEffect(
    () => () => {
      abortRef.current?.abort()
    },
    [],
  )

  async function handleSignIn() {
    abortRef.current?.abort()
    const controller = new AbortController()
    abortRef.current = controller
    setState({ kind: "working", message: t("provider.oauth.opening") })
    try {
      const credentials = await signIn(provider, {
        signal: controller.signal,
        // The provider page must open in the user's browser, not in the
        // popover webview — that is where their session already lives.
        onAuth: ({ url }) => {
          setState({ kind: "working", message: t("provider.oauth.waiting") })
          void openExternal(url)
        },
        onProgress: (message) => setState({ kind: "working", message }),
      })
      setState({ kind: "signed-in", email: credentials.email })
      onSelect()
      onAuthChanged()
    } catch (error) {
      if (controller.signal.aborted) return
      setState({
        kind: "error",
        message: error instanceof Error ? error.message : String(error),
      })
    }
  }

  async function handleSignOut() {
    await signOut(provider)
    setState({ kind: "signed-out" })
    onAuthChanged()
  }

  const signedIn = state.kind === "signed-in"
  const busy = state.kind === "working" || state.kind === "checking"
  // Signing in works for every provider, but only some have a chat request
  // path. Selecting one without it would leave the app "connected" to
  // something that refuses every request.
  const chatReady = OAUTH_MODELS[provider].length > 0

  return (
    <div
      className={cn(
        "rounded-md border px-2.5 py-2 transition-colors",
        active ? "border-primary/60 bg-primary/5" : "border-border",
      )}
    >
      <div className="flex items-center gap-2">
        <button
          type="button"
          // Needs both a token and a usable request path; either missing
          // means selecting this would be a dead end.
          disabled={!signedIn || !chatReady}
          onClick={onSelect}
          className="flex flex-1 items-center gap-2 text-left disabled:cursor-default"
        >
          <span className="text-[12px] font-medium">{definition.name}</span>
          {active && signedIn && <Check className="h-3 w-3 text-primary" />}
        </button>

        {signedIn ? (
          <Button size="sm" variant="ghost" className="h-6 px-2" onClick={handleSignOut}>
            <LogOut className="mr-1 h-3 w-3" />
            <span className="text-[11px]">{t("provider.oauth.signOut")}</span>
          </Button>
        ) : (
          <Button
            size="sm"
            variant="outline"
            className="h-6 px-2"
            disabled={busy}
            onClick={handleSignIn}
          >
            {busy ? (
              <Loader2 className="mr-1 h-3 w-3 animate-spin" />
            ) : (
              <LogIn className="mr-1 h-3 w-3" />
            )}
            <span className="text-[11px]">{t("provider.oauth.signIn")}</span>
          </Button>
        )}
      </div>

      {state.kind === "signed-in" && state.email && (
        <p className="mt-1 text-[11px] text-muted-foreground">{state.email}</p>
      )}
      {!chatReady && (
        <p className="mt-1 text-[11px] text-muted-foreground">
          {t("provider.oauth.chatUnsupported")}
        </p>
      )}
      {state.kind === "working" && (
        <p className="mt-1 text-[11px] text-muted-foreground">{state.message}</p>
      )}
      {state.kind === "error" && (
        <p className="mt-1 text-[11px] text-destructive">{state.message}</p>
      )}
    </div>
  )
}
