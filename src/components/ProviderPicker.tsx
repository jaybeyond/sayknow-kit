import { useEffect, useRef } from "react"
import { Check, Loader2 } from "lucide-react"
import { Label } from "@/components/ui/label"
import { Input } from "@/components/ui/input"
import { OAuthProviderCard } from "@/components/OAuthProviderCard"
import {
  endpointPreset,
  oauthProviderRef,
  parseOAuthProvider,
  PROVIDER_PRESETS,
  ZAI_BASE,
  ZAI_CODING_BASE,
  type EndpointProviderId,
  type ProviderId,
} from "@/lib/openrouter"
import { OAUTH_PROVIDER_IDS } from "@/lib/oauth/registry"
import { defaultOAuthModel } from "@/lib/oauth/models"
import { useT, type UILocaleSetting } from "@/i18n"
import { useProviderProbe, type ProbeStatus } from "@/hooks/useProviderProbe"
import { cn } from "@/lib/utils"

type Props = {
  provider: ProviderId
  baseURL: string
  apiKey?: string
  uiLocale: UILocaleSetting
  onChange: (next: {
    provider: ProviderId
    baseURL: string
    model?: string
    /** Cleared along with the model: another provider's id means nothing here. */
    fallbackModel?: string
  }) => void
  /** Called once when the selected provider becomes reachable without auth.
   * The parent can use this for one-tap login (custom open-mode endpoint). */
  onAutoReachable?: () => void
  /** Raised after an OAuth sign-in or sign-out so settings re-derive state. */
  onAuthChanged?: () => void
  compact?: boolean
}

const PROVIDER_ORDER: EndpointProviderId[] = ["openrouter", "nvidia", "zai", "custom"]

/** z.ai bills the same key two ways, each on its own path. */
const ZAI_PLANS = [
  { base: ZAI_BASE, label: "provider.zai.plan.api" },
  { base: ZAI_CODING_BASE, label: "provider.zai.plan.coding" },
] as const

export function ProviderPicker({
  provider,
  baseURL,
  apiKey = "",
  uiLocale,
  onChange,
  onAutoReachable,
  onAuthChanged,
  compact,
}: Props) {
  const { t } = useT(uiLocale)

  function pick(id: EndpointProviderId) {
    if (id === provider) return
    const preset = PROVIDER_PRESETS[id]
    // The previous provider's model id would be rejected here, so switch to
    // this provider's default and drop the fallback with it.
    onChange({
      provider: id,
      baseURL: preset.baseURL,
      ...(preset.defaultModel ? { model: preset.defaultModel, fallbackModel: "" } : {}),
    })
  }

  const activePreset = endpointPreset(provider)

  // Probe only providers that are "self-detectable" — a custom endpoint may
  // be a local server that answers `/v1/models` without auth.
  const shouldProbe = provider === "custom" && baseURL.length > 0
  const status: ProbeStatus = useProviderProbe(baseURL, apiKey, shouldProbe)

  // Fire onAutoReachable once per ready edge so the parent can auto-login.
  const lastFiredFor = useRef<string>("")
  useEffect(() => {
    if (status !== "ready") return
    const sig = `${provider}|${baseURL}`
    if (lastFiredFor.current === sig) return
    lastFiredFor.current = sig
    onAutoReachable?.()
  }, [status, provider, baseURL, onAutoReachable])

  return (
    <div className="space-y-2">
      <Label className="text-[11px]">{t("provider.label")}</Label>
      <div className={cn("grid gap-1.5", compact ? "grid-cols-1" : "grid-cols-1")}>
        {PROVIDER_ORDER.map((id) => {
          const active = provider === id
          const showStatus = active && id === "custom"
          return (
            <button
              key={id}
              type="button"
              onClick={() => pick(id)}
              className={cn(
                "flex items-start gap-2 rounded-md border px-2.5 py-2 text-left text-xs transition",
                active
                  ? "border-primary/70 bg-primary/[0.06]"
                  : "border-border hover:border-border/80 hover:bg-accent/20",
              )}
            >
              <div
                className={cn(
                  "mt-0.5 flex h-3.5 w-3.5 shrink-0 items-center justify-center rounded-full border",
                  active
                    ? "border-primary bg-primary text-primary-foreground"
                    : "border-border",
                )}
              >
                {active && <Check className="h-2.5 w-2.5" />}
              </div>
              <div className="min-w-0 flex-1">
                <div className="flex items-center gap-1.5">
                  <div className="text-[12px] font-medium leading-tight">
                    {t(`provider.preset.${id}`)}
                  </div>
                  {showStatus && (
                    <StatusBadge status={status} uiLocale={uiLocale} />
                  )}
                </div>
                <div className="mt-0.5 line-clamp-2 text-[10px] leading-snug text-muted-foreground">
                  {t(`provider.${id}.body`)}
                </div>
              </div>
            </button>
          )
        })}
      </div>

      {provider === "zai" && (
        <div
          role="radiogroup"
          aria-label={t("provider.zai.plan.label")}
          className="grid grid-cols-2 gap-1 rounded-md bg-muted/50 p-0.5"
        >
          {ZAI_PLANS.map((plan) => {
            const selected = baseURL === plan.base
            return (
              <button
                key={plan.base}
                type="button"
                role="radio"
                aria-checked={selected}
                onClick={() => onChange({ provider, baseURL: plan.base })}
                className={cn(
                  "rounded px-2 py-1 text-[11px] transition-colors duration-150 active:scale-[0.98]",
                  selected
                    ? "bg-background font-medium text-foreground shadow-sm dark:bg-white/[0.12] dark:shadow-none"
                    : "text-muted-foreground hover:text-foreground",
                )}
              >
                {t(plan.label)}
              </button>
            )
          })}
        </div>
      )}

      <div>
        <Label className="text-[10px] text-muted-foreground">
          {t("provider.baseURL")}
        </Label>
        <Input
          value={baseURL}
          onChange={(e) =>
            onChange({ provider, baseURL: e.target.value })
          }
          readOnly={provider !== "custom"}
          placeholder={
            provider === "custom" || !activePreset
              ? "https://your-endpoint/v1"
              : activePreset.baseURL
          }
          className={cn(
            "mt-1 h-8 text-[11px] font-mono",
            provider !== "custom" && "text-muted-foreground",
          )}
        />
      </div>

      <div className="space-y-1.5">
        <Label className="text-[10px] text-muted-foreground">
          {t("provider.oauth.label")}
        </Label>
        {OAUTH_PROVIDER_IDS.map((id) => (
          <OAuthProviderCard
            key={id}
            provider={id}
            active={parseOAuthProvider(provider) === id}
            uiLocale={uiLocale}
            // OAuth providers carry no endpoint of their own; the base URL
            // stays whatever the key-based provider had, unused.
            onSelect={() => {
              // Carry the model over too. Leaving the previous provider's id
              // in place is what sent an OpenRouter model name to Anthropic.
              onChange({
                provider: oauthProviderRef(id),
                baseURL,
                model: defaultOAuthModel(id),
              })
            }}
            onAuthChanged={() => onAuthChanged?.()}
          />
        ))}
      </div>
    </div>
  )
}

function StatusBadge({
  status,
  uiLocale,
}: {
  status: ProbeStatus
  uiLocale: UILocaleSetting
}) {
  const { t } = useT(uiLocale)
  if (status === "idle") return null
  const map: Record<
    Exclude<ProbeStatus, "idle">,
    { label: string; color: string }
  > = {
    checking: {
      label: t("provider.status.checking"),
      color: "bg-muted-foreground/30 text-muted-foreground",
    },
    ready: {
      label: t("provider.status.ready"),
      color: "bg-emerald-500/15 text-emerald-700 dark:text-emerald-400",
    },
    "auth-required": {
      label: t("provider.status.authRequired"),
      color: "bg-amber-500/15 text-amber-700 dark:text-amber-400",
    },
    down: {
      label: t("provider.status.down"),
      color: "bg-destructive/15 text-destructive",
    },
  }
  const v = map[status]
  return (
    <span
      className={cn(
        "inline-flex items-center gap-1 rounded-full px-1.5 py-0.5 text-[9px] font-medium",
        v.color,
      )}
    >
      {status === "checking" && <Loader2 className="h-2 w-2 animate-spin" />}
      {v.label}
    </span>
  )
}
