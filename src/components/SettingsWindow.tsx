import { useState } from "react"
import {
  BookText,
  Check,
  ExternalLink,
  Gauge,
  Loader2,
  Play,
  Zap,
  Eye,
  EyeOff,
  Info,
  Languages as LangIcon,
  LogOut,
  Pin,
  Plug,
  Settings as SettingsIcon,
  Sparkles,
  Wallet,
} from "lucide-react"
import { Button } from "@/components/ui/button"
import { Input } from "@/components/ui/input"
import { Label } from "@/components/ui/label"
import { Textarea } from "@/components/ui/textarea"
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from "@/components/ui/select"
import { Separator } from "@/components/ui/separator"
import { Switch } from "@/components/ui/switch"
import { ScrollArea } from "@/components/ui/scroll-area"
import { ModelPicker } from "./ModelPicker"
import { ProviderPicker } from "./ProviderPicker"
import { GlossaryEditor } from "./GlossaryEditor"
import { useModels } from "@/hooks/useModels"
import { useUsage } from "@/hooks/useUsage"
import type { Settings } from "@/hooks/useSettings"
import type { ThemeMode } from "@/hooks/useTheme"
import { formatCost, formatTokens } from "@/lib/usage"
import {
  DEFAULT_REFINE_PROMPT,
  DEFAULT_TRANSLATE_PROMPT,
  PROVIDER_PRESETS,
} from "@/lib/openrouter"
import {
  UI_LOCALES,
  UI_LOCALE_LABELS,
  useT,
  type UILocaleSetting,
} from "@/i18n"
import { cn } from "@/lib/utils"
import { openExternal } from "@/lib/runtime"
import {
  deeplUsage,
  deeplTranslate,
  formatCharacters,
  type DeeplUsage,
} from "@/lib/deepl"
import { translationMemory } from "@/lib/translation-memory"

type Props = {
  settings: Settings
  update: (patch: Partial<Settings>) => void
  onLogout: () => void
  themeMode: ThemeMode
  setThemeMode: (m: ThemeMode) => void
}

type Section =
  | "general"
  | "connection"
  | "glossary"
  | "prompt"
  | "usage"
  | "about"

export function SettingsWindow({
  settings,
  update,
  onLogout,
  themeMode,
  setThemeMode,
}: Props) {
  const { t } = useT(settings.uiLocale)
  const [section, setSection] = useState<Section>("general")
  const { models, loading: modelsLoading } = useModels(
    settings.apiKey,
    settings.baseURL,
  )
  const { today: usageToday, month: usageMonth, clear: clearUsage } = useUsage()

  const NAV: { id: Section; label: string; icon: typeof SettingsIcon }[] = [
    { id: "general", label: t("settings.section.general"), icon: SettingsIcon },
    { id: "connection", label: t("settings.section.connection"), icon: Plug },
    { id: "glossary", label: t("settings.section.glossary"), icon: BookText },
    { id: "prompt", label: t("settings.section.prompt"), icon: Sparkles },
    { id: "usage", label: t("settings.section.usage"), icon: Wallet },
    { id: "about", label: t("settings.section.about"), icon: Info },
  ]

  return (
    <div className="flex h-svh w-svw bg-background text-foreground">
      {/* Sidebar */}
      <aside className="flex w-[200px] shrink-0 flex-col border-r bg-muted/30">
        <div
          className="flex items-center gap-2 border-b px-4 py-3"
          data-tauri-drag-region
        >
          <div className="flex h-6 w-6 items-center justify-center rounded-md bg-foreground text-background">
            <LangIcon className="h-3.5 w-3.5" />
          </div>
          <div className="text-sm font-semibold">{t("settings.title")}</div>
        </div>
        <nav className="flex-1 space-y-0.5 p-2">
          {NAV.map((n) => {
            const Icon = n.icon
            const active = section === n.id
            return (
              <button
                key={n.id}
                onClick={() => setSection(n.id)}
                className={cn(
                  "flex w-full items-center gap-2 rounded-md px-2.5 py-1.5 text-left text-[13px] transition",
                  active
                    ? "bg-background font-medium text-foreground shadow-sm"
                    : "text-muted-foreground hover:bg-background/60 hover:text-foreground",
                )}
              >
                <Icon className="h-3.5 w-3.5" />
                <span>{n.label}</span>
              </button>
            )
          })}
        </nav>
      </aside>

      {/* Content */}
      <main className="flex-1 overflow-hidden">
        <ScrollArea className="h-full">
          <div className="mx-auto max-w-2xl p-6">
            {section === "general" && (
              <GeneralSection
                settings={settings}
                update={update}
                themeMode={themeMode}
                setThemeMode={setThemeMode}
              />
            )}
            {section === "connection" && (
              <ConnectionSection
                settings={settings}
                update={update}
                onLogout={onLogout}
                models={models}
                modelsLoading={modelsLoading}
              />
            )}
            {section === "glossary" && (
              <GlossarySection settings={settings} update={update} />
            )}
            {section === "prompt" && (
              <PromptSection settings={settings} update={update} />
            )}
            {section === "usage" && (
              <UsageSection
                settings={settings}
                today={usageToday}
                month={usageMonth}
                onClear={clearUsage}
              />
            )}
            {section === "about" && (
              <AboutSection settings={settings} />
            )}
          </div>
        </ScrollArea>
      </main>
    </div>
  )
}

/* ─────────── Section: General ─────────── */
function GeneralSection({
  settings,
  update,
  themeMode,
  setThemeMode,
}: {
  settings: Settings
  update: (p: Partial<Settings>) => void
  themeMode: ThemeMode
  setThemeMode: (m: ThemeMode) => void
}) {
  const { t } = useT(settings.uiLocale)
  return (
    <div className="space-y-6">
      <SectionHeader
        icon={SettingsIcon}
        title={t("settings.section.general")}
      />

      <Row label={t("settings.mode")} description={t(settings.autoTranslate ? "settings.clipboard.body" : "")}>
        <div className="grid w-full max-w-[280px] grid-cols-2 gap-1">
          <Button
            size="sm"
            variant={settings.autoTranslate ? "secondary" : "ghost"}
            className="h-8 text-xs"
            onClick={() => update({ autoTranslate: true })}
          >
            {t("settings.mode.auto")}
          </Button>
          <Button
            size="sm"
            variant={!settings.autoTranslate ? "secondary" : "ghost"}
            className="h-8 text-xs"
            onClick={() => update({ autoTranslate: false })}
          >
            {t("settings.mode.manual")}
          </Button>
        </div>
      </Row>

      <Separator />

      <Row
        label={t("settings.clipboard.title")}
        description={t("settings.clipboard.body")}
      >
        <Switch
          checked={settings.clipboardOnHotkey}
          onCheckedChange={(v) => update({ clipboardOnHotkey: v })}
        />
      </Row>

      <Separator />

      <Row
        label={t("header.pin")}
        description={t("header.pinned")}
      >
        <div className="flex items-center gap-1.5">
          <Pin className="h-3.5 w-3.5 text-muted-foreground" />
          <Switch
            checked={settings.pinned}
            onCheckedChange={(v) => update({ pinned: v })}
          />
        </div>
      </Row>

      <Separator />

      <Row label={t("settings.theme")}>
        <Select
          value={themeMode}
          onValueChange={(v) => setThemeMode(v as ThemeMode)}
        >
          <SelectTrigger className="w-[180px] text-xs">
            <SelectValue />
          </SelectTrigger>
          <SelectContent>
            <SelectItem value="system" className="text-xs">
              {t("settings.theme.system")}
            </SelectItem>
            <SelectItem value="light" className="text-xs">
              {t("settings.theme.light")}
            </SelectItem>
            <SelectItem value="dark" className="text-xs">
              {t("settings.theme.dark")}
            </SelectItem>
          </SelectContent>
        </Select>
      </Row>

      <Separator />

      <Row label={t("settings.uiLocale")}>
        <Select
          value={settings.uiLocale}
          onValueChange={(v) => update({ uiLocale: v as UILocaleSetting })}
        >
          <SelectTrigger className="w-[180px] text-xs">
            <SelectValue />
          </SelectTrigger>
          <SelectContent>
            <SelectItem value="system" className="text-xs">
              {t("settings.uiLocale.system")}
            </SelectItem>
            {UI_LOCALES.map((lc) => (
              <SelectItem key={lc} value={lc} className="text-xs">
                {UI_LOCALE_LABELS[lc]}
              </SelectItem>
            ))}
          </SelectContent>
        </Select>
      </Row>
    </div>
  )
}

/* ─────────── Section: Connection ─────────── */
function ConnectionSection({
  settings,
  update,
  onLogout,
  models,
  modelsLoading,
}: {
  settings: Settings
  update: (p: Partial<Settings>) => void
  onLogout: () => void
  models: import("@/lib/openrouter").OpenRouterModel[]
  modelsLoading: boolean
}) {
  const { t } = useT(settings.uiLocale)
  return (
    <div className="space-y-6">
      <SectionHeader icon={Plug} title={t("settings.section.connection")} />

      <ProviderPicker
        provider={settings.provider}
        baseURL={settings.baseURL}
        apiKey={settings.apiKey}
        uiLocale={settings.uiLocale}
        onChange={(next) => update(next)}
      />

      <ApiKeyRow
        provider={settings.provider}
        apiKey={settings.apiKey}
        onChange={(v) => update({ apiKey: v })}
        uiLocale={settings.uiLocale}
      />

      <Separator />

      <DeeplRow settings={settings} update={update} />

      <Separator />

      <Row label={t("settings.model")}>
        <div className="w-full max-w-[400px]">
          <ModelPicker
            value={settings.model}
            onChange={(id) => update({ model: id })}
            models={models}
            loading={modelsLoading}
          />
          <p className="mt-1 text-[11px] text-muted-foreground">
            {models.length > 0
              ? `${models.length} ${t("settings.model.count")} · ${PROVIDER_PRESETS[settings.provider].label}`
              : t("settings.model.loading")}
          </p>
        </div>
      </Row>

      <Separator />

      <Row label={t("settings.fallback")} description={t("settings.fallback.body")}>
        <div className="w-full max-w-[400px]">
          <div className="flex items-center gap-2">
            <div className="flex-1">
              <ModelPicker
                value={settings.fallbackModel}
                onChange={(id) => update({ fallbackModel: id })}
                models={models}
                loading={modelsLoading}
                placeholder={t("settings.fallback.empty")}
              />
            </div>
            {settings.fallbackModel && (
              <Button
                variant="ghost"
                size="sm"
                onClick={() => update({ fallbackModel: "" })}
                className="h-8 text-xs"
              >
                {t("settings.fallback.clear")}
              </Button>
            )}
          </div>
        </div>
      </Row>

      <Separator />

      <Button
        variant="ghost"
        className="text-destructive hover:bg-destructive/10 hover:text-destructive"
        onClick={onLogout}
      >
        <LogOut className="h-3.5 w-3.5" />
        {t("settings.logout")}
      </Button>
    </div>
  )
}

/**
 * DeepL lives next to the model settings because it is the other half of the
 * same decision: which engine answers a translation. It is configured *and*
 * exercised here — paste the key, hit run, and the live quota comes back in
 * place, rather than having to go and try a translation to find out whether it
 * works.
 */
function DeeplRow({
  settings,
  update,
}: {
  settings: Settings
  update: (p: Partial<Settings>) => void
}) {
  const { t } = useT(settings.uiLocale)
  const [show, setShow] = useState(false)
  const [busy, setBusy] = useState(false)
  const [usage, setUsage] = useState<DeeplUsage | null>(null)
  const [sample, setSample] = useState<string | null>(null)
  const [error, setError] = useState<string | null>(null)

  const enabled = settings.translateEngine === "deepl"

  async function run() {
    const key = settings.deeplKey.trim()
    if (!key || busy) return
    setBusy(true)
    setError(null)
    setSample(null)
    try {
      // Two calls on purpose: the quota proves the key, the translation proves
      // the whole path end to end, which is what "does it work" actually means.
      const [quota, probe] = await Promise.all([
        deeplUsage(key),
        deeplTranslate({
          key,
          text: "안녕하세요",
          from: "ko",
          to: settings.to === "ko" ? "en" : settings.to,
        }),
      ])
      setUsage(quota)
      setSample(probe.text)
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e))
      setUsage(null)
    } finally {
      setBusy(false)
    }
  }

  const pct =
    usage && usage.characterLimit > 0 && usage.characterLimit < 1e12
      ? Math.min(100, (usage.characterCount / usage.characterLimit) * 100)
      : null

  return (
    <div className="space-y-3">
      <Row
        label={t("settings.engine")}
        description={t("settings.engine.body")}
      >
        <div className="grid w-full max-w-[280px] grid-cols-2 gap-1">
          <Button
            size="sm"
            variant={!enabled ? "secondary" : "ghost"}
            className="h-8 text-xs"
            onClick={() => update({ translateEngine: "llm" })}
          >
            {t("settings.engine.llm")}
          </Button>
          <Button
            size="sm"
            variant={enabled ? "secondary" : "ghost"}
            className="h-8 text-xs"
            onClick={() => update({ translateEngine: "deepl" })}
          >
            <Zap className="h-3 w-3" />
            DeepL
          </Button>
        </div>
      </Row>

      <Row label={t("settings.deepl.key")} description={t("settings.deepl.key.body")}>
        <div className="w-full max-w-[400px] space-y-2">
          <div className="flex items-center gap-2">
            <div className="relative flex-1">
              <Input
                type={show ? "text" : "password"}
                value={settings.deeplKey}
                onChange={(e) => update({ deeplKey: e.target.value })}
                placeholder="xxxxxxxx-xxxx-...:fx"
                className="h-8 pr-8 font-mono text-xs"
                spellCheck={false}
              />
              <button
                type="button"
                onClick={() => setShow((v) => !v)}
                className="absolute right-2 top-1/2 -translate-y-1/2 text-muted-foreground hover:text-foreground"
                aria-label={t(show ? "login.hide" : "login.show")}
              >
                {show ? (
                  <EyeOff className="h-3.5 w-3.5" />
                ) : (
                  <Eye className="h-3.5 w-3.5" />
                )}
              </button>
            </div>
            <Button
              size="sm"
              className="h-8 shrink-0 text-xs"
              disabled={!settings.deeplKey.trim() || busy}
              onClick={run}
            >
              {busy ? (
                <Loader2 className="h-3 w-3 animate-spin" />
              ) : (
                <Play className="h-3 w-3" />
              )}
              {t("settings.deepl.run")}
            </Button>
          </div>

          {error && (
            <p className="rounded-md border border-destructive/40 bg-destructive/10 px-2 py-1.5 text-[11px] text-destructive">
              {error}
            </p>
          )}

          {sample && (
            <p className="flex items-center gap-1.5 text-[11px] text-muted-foreground">
              <Check className="h-3 w-3 text-primary" />
              안녕하세요 → <span className="font-medium text-foreground">{sample}</span>
            </p>
          )}

          {usage && (
            <div className="rounded-md border p-2">
              <div className="flex items-baseline justify-between text-[11px]">
                <span className="flex items-center gap-1 text-muted-foreground">
                  <Gauge className="h-3 w-3" />
                  {t("settings.deepl.quota")} · {usage.plan}
                </span>
                <span className="tabular-nums">
                  {formatCharacters(usage.characterCount)} /{" "}
                  {formatCharacters(usage.characterLimit)}
                </span>
              </div>
              {pct !== null && (
                <div className="mt-1 h-1 overflow-hidden rounded-full bg-border">
                  <div
                    className="h-full bg-primary"
                    style={{ width: `${pct}%` }}
                  />
                </div>
              )}
            </div>
          )}

          <div className="flex items-center justify-between">
            <button
              type="button"
              onClick={() => openExternal("https://www.deepl.com/your-account/keys")}
              className="text-[11px] text-muted-foreground underline-offset-2 hover:underline"
            >
              {t("settings.deepl.getKey")}
            </button>
            <button
              type="button"
              onClick={() => translationMemory.clear()}
              className="text-[11px] text-muted-foreground underline-offset-2 hover:underline"
            >
              {t("settings.deepl.clearCache")}
            </button>
          </div>
        </div>
      </Row>
    </div>
  )
}

/* ─────────── Section: Glossary ─────────── */
function GlossarySection({
  settings,
  update,
}: {
  settings: Settings
  update: (p: Partial<Settings>) => void
}) {
  const { t } = useT(settings.uiLocale)
  return (
    <div className="space-y-6">
      <SectionHeader icon={BookText} title={t("settings.section.glossary")} />

      <div className="rounded-md border bg-muted/30 p-4 space-y-3 text-sm">
        <div>
          <div className="text-sm font-semibold">
            {t("settings.glossary.intro.title")}
          </div>
          <p className="mt-1 text-xs leading-relaxed text-muted-foreground">
            {t("settings.glossary.intro.body")}
          </p>
        </div>
        <Separator />
        <div>
          <div className="text-xs font-semibold">
            {t("settings.glossary.example.title")}
          </div>
          <p className="mt-1 text-xs text-muted-foreground">
            {t("settings.glossary.example.body")}
          </p>
        </div>
      </div>

      <GlossaryEditor
        value={settings.glossary}
        onChange={(g) => update({ glossary: g })}
        uiLocale={settings.uiLocale}
      />
    </div>
  )
}

/* ─────────── Section: Prompt ─────────── */
function PromptSection({
  settings,
  update,
}: {
  settings: Settings
  update: (p: Partial<Settings>) => void
}) {
  const { t } = useT(settings.uiLocale)

  function resetTranslate() {
    update({ customTranslatePrompt: "" })
  }
  function resetRefine() {
    update({ customRefinePrompt: "" })
  }

  return (
    <div className="space-y-6">
      <SectionHeader icon={Sparkles} title={t("settings.section.prompt")} />

      <div className="rounded-md border border-amber-500/30 bg-amber-500/10 p-3 text-[11px] text-amber-700 dark:text-amber-400">
        {t("settings.prompt.warning")}
      </div>

      <div className="space-y-2">
        <div className="flex items-center justify-between">
          <Label className="text-xs">{t("settings.prompt.translate")}</Label>
          <Button
            variant="ghost"
            size="sm"
            className="h-6 text-[10px] text-muted-foreground"
            onClick={resetTranslate}
            disabled={!settings.customTranslatePrompt}
          >
            {t("settings.prompt.reset")}
          </Button>
        </div>
        <Textarea
          value={settings.customTranslatePrompt}
          onChange={(e) =>
            update({ customTranslatePrompt: e.target.value })
          }
          placeholder={DEFAULT_TRANSLATE_PROMPT}
          className="min-h-[140px] font-mono text-xs"
        />
      </div>

      <div className="space-y-2">
        <div className="flex items-center justify-between">
          <Label className="text-xs">{t("settings.prompt.refine")}</Label>
          <Button
            variant="ghost"
            size="sm"
            className="h-6 text-[10px] text-muted-foreground"
            onClick={resetRefine}
            disabled={!settings.customRefinePrompt}
          >
            {t("settings.prompt.reset")}
          </Button>
        </div>
        <Textarea
          value={settings.customRefinePrompt}
          onChange={(e) => update({ customRefinePrompt: e.target.value })}
          placeholder={DEFAULT_REFINE_PROMPT}
          className="min-h-[100px] font-mono text-xs"
        />
      </div>
    </div>
  )
}

/* ─────────── Section: Usage ─────────── */
function UsageSection({
  settings,
  today,
  month,
  onClear,
}: {
  settings: Settings
  today: import("@/lib/usage").UsageDay
  month: import("@/lib/usage").UsageDay
  onClear: () => void
}) {
  const { t } = useT(settings.uiLocale)
  const totalToday = today.promptTokens + today.completionTokens
  const totalMonth = month.promptTokens + month.completionTokens

  return (
    <div className="space-y-6">
      <SectionHeader icon={Wallet} title={t("settings.section.usage")} />

      <div className="grid grid-cols-2 gap-3">
        <UsageCard
          title={t("settings.usage.today")}
          calls={today.calls}
          tokens={totalToday}
          cost={today.costUsd}
          uiLocale={settings.uiLocale}
        />
        <UsageCard
          title={t("settings.usage.month")}
          calls={month.calls}
          tokens={totalMonth}
          cost={month.costUsd}
          uiLocale={settings.uiLocale}
        />
      </div>

      <Button
        variant="ghost"
        size="sm"
        onClick={onClear}
        className="text-xs text-muted-foreground hover:text-destructive"
      >
        {t("history.clear")}
      </Button>
    </div>
  )
}

function UsageCard({
  title,
  calls,
  tokens,
  cost,
  uiLocale,
}: {
  title: string
  calls: number
  tokens: number
  cost: number
  uiLocale: UILocaleSetting
}) {
  const { t } = useT(uiLocale)
  return (
    <div className="rounded-md border bg-muted/30 p-4">
      <div className="text-[11px] uppercase tracking-wider text-muted-foreground">
        {title}
      </div>
      <div className="mt-2 text-2xl font-semibold tabular-nums">
        {formatCost(cost)}
      </div>
      <div className="mt-2 space-y-0.5 text-[11px] text-muted-foreground tabular-nums">
        <div>
          {calls.toLocaleString()} {t("settings.usage.calls")}
        </div>
        <div>
          {formatTokens(tokens)} {t("settings.usage.tokens")}
        </div>
      </div>
    </div>
  )
}

/* ─────────── Section: About ─────────── */
function AboutSection({ settings }: { settings: Settings }) {
  const { t } = useT(settings.uiLocale)
  return (
    <div className="space-y-6">
      <SectionHeader icon={Info} title={t("settings.section.about")} />

      <div className="space-y-3">
        <div className="flex items-baseline gap-3">
          <div className="flex h-12 w-12 items-center justify-center rounded-2xl bg-foreground text-background">
            <LangIcon className="h-6 w-6" />
          </div>
          <div>
            <div className="text-lg font-semibold">SayKnow Kit</div>
            <div className="text-xs text-muted-foreground">
              {t("settings.about.version")} 0.1.0
            </div>
          </div>
        </div>
        <p className="text-sm text-muted-foreground">{t("app.tagline")}</p>
      </div>

      <Separator />

      <div className="space-y-2">
        <button
          type="button"
          onClick={() => openExternal("https://github.com/jaybeyond/sayknow-kit")}
          className="inline-flex items-center gap-1 text-sm text-foreground hover:text-foreground/80"
        >
          {t("settings.about.repo")}
          <ExternalLink className="h-3 w-3" />
        </button>
        <br />
        <button
          type="button"
          onClick={() => openExternal("https://openrouter.ai/keys")}
          className="inline-flex items-center gap-1 text-sm text-foreground hover:text-foreground/80"
        >
          {t("settings.about.openrouter")}
          <ExternalLink className="h-3 w-3" />
        </button>
      </div>
    </div>
  )
}

/* ─────────── Helpers ─────────── */
function SectionHeader({
  icon: Icon,
  title,
}: {
  icon: typeof SettingsIcon
  title: string
}) {
  return (
    <div className="flex items-center gap-2 pb-2" data-tauri-drag-region>
      <Icon className="h-4 w-4 text-muted-foreground" />
      <h2 className="text-base font-semibold">{title}</h2>
    </div>
  )
}

function Row({
  label,
  description,
  children,
}: {
  label: string
  description?: string
  children: React.ReactNode
}) {
  return (
    <div className="flex items-start justify-between gap-4">
      <div className="flex-1">
        <Label className="text-sm">{label}</Label>
        {description && (
          <p className="mt-0.5 text-[11px] leading-relaxed text-muted-foreground">
            {description}
          </p>
        )}
      </div>
      <div className="shrink-0">{children}</div>
    </div>
  )
}

function ApiKeyRow({
  provider,
  apiKey,
  onChange,
  uiLocale,
}: {
  provider: Settings["provider"]
  apiKey: string
  onChange: (next: string) => void
  uiLocale: Settings["uiLocale"]
}) {
  const { t } = useT(uiLocale)
  const [show, setShow] = useState(false)
  const [draft, setDraft] = useState(apiKey)

  // Keep the input in sync if the apiKey changes from elsewhere (cross-window).
  if (apiKey !== draft && document.activeElement?.id !== "settings-api-key") {
    setDraft(apiKey)
  }

  const placeholder =
    provider === "openrouter"
      ? "sk-or-..."
      : provider === "ocp"
        ? "OCP token (leave blank for open mode)"
        : "API key"

  const label =
    provider === "openrouter"
      ? "OpenRouter API Key"
      : provider === "ocp"
        ? "OCP Token (optional)"
        : "API Key"

  return (
    <Row label={label}>
      <div className="w-full max-w-[400px]">
        <div className="relative">
          <Input
            id="settings-api-key"
            type={show ? "text" : "password"}
            placeholder={placeholder}
            autoComplete="off"
            value={draft}
            onChange={(e) => {
              setDraft(e.target.value)
              onChange(e.target.value)
            }}
            className="pr-9 text-sm"
          />
          <button
            type="button"
            onClick={() => setShow((s) => !s)}
            className="absolute right-2 top-1/2 -translate-y-1/2 text-muted-foreground hover:text-foreground"
            aria-label={show ? "hide" : "show"}
          >
            {show ? <EyeOff className="h-4 w-4" /> : <Eye className="h-4 w-4" />}
          </button>
        </div>
        <p className="mt-1 text-[11px] text-muted-foreground">
          {t("settings.connection.apiKeyShown")}
        </p>
      </div>
    </Row>
  )
}
