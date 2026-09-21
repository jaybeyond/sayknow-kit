import { useState } from "react"
import {
  ExternalLink,
  Eye,
  EyeOff,
  Globe,
  Loader2,
  Wand2,
  Zap,
} from "lucide-react"
import { Button } from "@/components/ui/button"
import { Input } from "@/components/ui/input"
import { Label } from "@/components/ui/label"
import { ProviderPicker } from "./ProviderPicker"
import {
  isOAuthProvider,
  OPENROUTER_BASE,
  PROVIDER_PRESETS,
  verifyKey,
  type ProviderId,
} from "@/lib/openrouter"
import type { Settings } from "@/hooks/useSettings"
import type { UILocaleSetting } from "@/i18n"
import { useT } from "@/i18n"
import { openExternal } from "@/lib/runtime"

type Props = {
  update: (patch: Partial<Settings>) => void
  uiLocale: UILocaleSetting
}

export function LoginPanel({ update, uiLocale }: Props) {
  const { t } = useT(uiLocale)
  const [provider, setProvider] = useState<ProviderId>("openrouter")
  const [baseURL, setBaseURL] = useState<string>(OPENROUTER_BASE)
  const [key, setKey] = useState("")
  const [show, setShow] = useState(false)
  const [status, setStatus] = useState<"idle" | "checking" | "error">("idle")
  const [error, setError] = useState<string | null>(null)

  function handleProviderChange(next: {
    provider: ProviderId
    baseURL: string
    model?: string
  }) {
    setProvider(next.provider)
    setBaseURL(next.baseURL)
    // An OAuth provider is only selectable once its card holds a live token,
    // so picking one *is* the login — there is no key to type and nothing to
    // verify. Commit it now; waiting for Connect would strand the user on
    // this screen with a sign-in that already succeeded.
    if (isOAuthProvider(next.provider)) {
      update({
        provider: next.provider,
        baseURL: next.baseURL,
        apiKey: "",
        ...(next.model ? { model: next.model } : {}),
      })
    }
  }

  function handleAutoReachable() {
    // Auto-login when a custom endpoint is reachable without a key.
    // OpenRouter is excluded because it always needs a key.
    if (provider === "openrouter") return
    if (isOAuthProvider(provider)) return
    if (key.trim()) return // User typed a key — wait for explicit Connect.
    update({
      provider,
      baseURL: baseURL.trim(),
      apiKey: "",
    })
  }

  async function handleConnect() {
    // OAuth providers carry their own credentials; `verifyKey` would probe an
    // endpoint that has nothing to do with them and never settle.
    if (isOAuthProvider(provider)) {
      update({ provider, baseURL: baseURL.trim(), apiKey: "" })
      return
    }
    if (!baseURL.trim()) return
    // A custom endpoint may not require a key for `/models`. Only enforce it
    // for OpenRouter.
    if (provider === "openrouter" && !key.trim()) return
    setStatus("checking")
    setError(null)
    try {
      const ok = await verifyKey(key.trim(), baseURL.trim())
      if (!ok) {
        setStatus("error")
        setError(t("login.invalidKey"))
        return
      }
      update({
        provider,
        baseURL: baseURL.trim(),
        apiKey: key.trim(),
      })
    } catch (e) {
      setStatus("error")
      setError(e instanceof Error ? e.message : t("login.connectFail"))
    }
  }

  const FEATURES = [
    { icon: Zap, title: t("feature.auto.title"), body: t("feature.auto.body") },
    { icon: Wand2, title: t("feature.refine.title"), body: t("feature.refine.body") },
    { icon: Globe, title: t("feature.byok.title"), body: t("feature.byok.body") },
  ] as const

  return (
    <div
      className="flex h-full flex-col overflow-y-auto px-5 pt-5 pb-4"
      data-tauri-drag-region
    >
      {/* Hero */}
      <div className="text-center" data-tauri-drag-region>
        <div className="mx-auto flex h-11 w-11 items-center justify-center rounded-2xl bg-foreground text-background shadow-md">
          <svg
            viewBox="0 0 24 24"
            fill="none"
            stroke="currentColor"
            strokeWidth="2"
            strokeLinecap="round"
            strokeLinejoin="round"
            className="h-5 w-5"
          >
            <path d="m5 8 6 6" />
            <path d="m4 14 6-6 2-3" />
            <path d="M2 5h12" />
            <path d="M7 2h1" />
            <path d="m22 22-5-10-5 10" />
            <path d="M14 18h6" />
          </svg>
        </div>
        <h1 className="mt-2 text-base font-semibold leading-tight">
          {t("login.title")}
        </h1>
        <p className="mt-0.5 text-[11px] text-muted-foreground">
          {t("app.tagline")}
        </p>
      </div>

      {/* Provider selector */}
      <div className="mt-4">
        <ProviderPicker
          provider={provider}
          baseURL={baseURL}
          apiKey={key}
          uiLocale={uiLocale}
          onChange={handleProviderChange}
          onAutoReachable={handleAutoReachable}
          // Signing out of the selected provider must drop us back to a
          // disconnected state rather than leave a stale selection behind.
          onAuthChanged={() => {
            if (isOAuthProvider(provider)) update({ provider, baseURL, apiKey: "" })
          }}
          compact
        />
      </div>

      {/* Key form */}
      <div className="mt-3">
        <Label htmlFor="api-key" className="text-[11px]">
          {t("login.label")}
          {provider !== "openrouter" && (
            <span className="ml-1 text-muted-foreground">
              (API)
            </span>
          )}
        </Label>
        <div className="relative mt-1.5">
          <Input
            id="api-key"
            type={show ? "text" : "password"}
            placeholder={provider === "openrouter" ? "sk-or-..." : "API key"}
            autoComplete="off"
            value={key}
            onChange={(e) => setKey(e.target.value)}
            onKeyDown={(e) => {
              if (e.key === "Enter") handleConnect()
            }}
            className="pr-9 text-sm"
          />
          <button
            type="button"
            onClick={() => setShow((s) => !s)}
            className="absolute right-2 top-1/2 -translate-y-1/2 text-muted-foreground hover:text-foreground"
            aria-label={show ? t("common.close") : ""}
          >
            {show ? <EyeOff className="h-4 w-4" /> : <Eye className="h-4 w-4" />}
          </button>
        </div>

        <Button
          onClick={handleConnect}
          disabled={
            // An OAuth provider needs no endpoint and no key, so the only
            // gate is that a request is not already in flight.
            isOAuthProvider(provider)
              ? status === "checking"
              : !baseURL.trim() ||
                (provider === "openrouter" && !key.trim()) ||
                status === "checking"
          }
          className="mt-2 w-full"
          size="sm"
        >
          {status === "checking" ? (
            <>
              <Loader2 className="h-3.5 w-3.5 animate-spin" />
              {t("login.connecting")}
            </>
          ) : (
            t("login.connect")
          )}
        </Button>

        {error && (
          <div className="mt-2 rounded-md border border-destructive/40 bg-destructive/10 px-2.5 py-1.5 text-[11px] text-destructive">
            {error}
          </div>
        )}

        {provider === "openrouter" && (
          <button
            type="button"
            onClick={() => openExternal("https://openrouter.ai/keys")}
            className="mt-2 inline-flex w-full items-center justify-center gap-1 text-[10px] text-muted-foreground hover:text-foreground"
          >
            {t("login.getKey")}
            <ExternalLink className="h-2.5 w-2.5" />
          </button>
        )}
      </div>

      {/* Feature showcase */}
      <div className="mt-auto grid grid-cols-3 gap-1.5 pt-4">
        {FEATURES.map((f) => {
          const Icon = f.icon
          return (
            <div
              key={f.title}
              className="rounded-lg border bg-muted/30 p-2.5 text-left"
            >
              <Icon className="h-3.5 w-3.5 text-muted-foreground" />
              <div className="mt-1.5 text-[11px] font-semibold leading-tight">
                {f.title}
              </div>
              <div className="mt-0.5 text-[10px] leading-snug text-muted-foreground">
                {f.body}
              </div>
            </div>
          )
        })}
      </div>

      <p className="mt-2.5 text-center text-[10px] text-muted-foreground">
        {t("login.keychainNote")}
      </p>

      {/* Silence unused-import warning during dev. */}
      <span className="hidden">{PROVIDER_PRESETS.openrouter.label}</span>
    </div>
  )
}
