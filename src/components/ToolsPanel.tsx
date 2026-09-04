import { useCallback, useEffect, useState, useSyncExternalStore } from "react"
import {
  Monitor,
  MonitorOff,
  Power,
  RefreshCw,
  ShieldAlert,
  Sun,
  Wrench,
} from "lucide-react"
import { UsagePanel } from "@/components/UsagePanel"
import { Button } from "@/components/ui/button"
import { Slider } from "@/components/ui/slider"
import type { Settings } from "@/hooks/useSettings"
import { useT } from "@/i18n"
import { isTauri } from "@/lib/runtime"
import {
  getSnapshot,
  refreshAccessibility,
  relaunchApp,
  requestAccessibility,
  resetAccessibility,
  scanDisplays,
  subscribe,
  syncBuiltin,
  type AccessibilityState,
  type DisplayRow,
} from "@/lib/tools-store"
import {
  getSnapshot as getMetricsSnapshot,
  refresh as refreshMetrics,
  setActive as setMetricsActive,
  subscribe as subscribeMetrics,
  formatBytes,
  formatPercent,
  type CpuMetric,
  type ResourceMetric,
  type TemperatureMetric,
} from "@/lib/system-metrics-store"
import { cn } from "@/lib/utils"

type Props = {
  settings: Settings
  active: boolean
}

/**
 * Tools that talk to the machine rather than to a translation provider. The
 * first one is screen brightness: hardware DDC for externals, real built-in
 * backlight through macOS Control Center accessibility, plus a separate gamma
 * stage for dimming below the hardware range.
 */
export function ToolsPanel({ settings, active }: Props) {
  const { t } = useT(settings.uiLocale)
  const { displays, error, loaded, accessibility } = useSyncExternalStore(
    subscribe,
    getSnapshot,
  )
  const metrics = useSyncExternalStore(subscribeMetrics, getMetricsSnapshot)

  const refreshAll = useCallback(() => {
    void scanDisplays(true)
    void refreshMetrics()
  }, [])

  // DDC reads take tens of ms per display, so only scan while visible. The
  // effect is a pure trigger; state lands in the store.
  useEffect(() => {
    if (!active) return
    void scanDisplays()
  }, [active])
  useEffect(() => {
    setMetricsActive(active)
    return () => setMetricsActive(false)
  }, [active])

  // Follow the keyboard keys: the built-in slider is an absolute brightness
  // whose system half changes under us. The sync command is pure math on the
  // tap-tracked level (no DDC, no registry hit), so 250ms is cheap and makes
  // the thumb feel attached to F1/F2.
  useEffect(() => {
    if (!active) return
    const timer = setInterval(() => void syncBuiltin(), 250)
    return () => clearInterval(timer)
  }, [active])

  // Never auto-prompt. macOS re-shows the same consent dialog on every call,
  // and an app running from quarantine can never keep the grant, so the loop
  // the user saw was infinite. Read the state and say it once, in-app.
  useEffect(() => {
    if (!active) return
    void refreshAccessibility()
  }, [active])

  // While the notice is up the user is in System Settings toggling the switch.
  // Poll so the panel reacts the moment macOS grants trust, instead of looking
  // like it is still demanding permission that was already given.
  const awaitingTrust = active && accessibility !== null && !accessibility.trusted
  useEffect(() => {
    if (!awaitingTrust) return
    const timer = setInterval(() => void refreshAccessibility(), 2000)
    return () => clearInterval(timer)
  }, [awaitingTrust])

  // Rescan when the popover opens: monitors connect, wake, or lock their DDC
  // while the app runs. The backend serves a scan from the last few seconds, so
  // this costs nothing when the panel was just open.
  useEffect(() => {
    if (!active || !isTauri()) return
    let alive = true
    let unlisten: (() => void) | null = null
    void import("@tauri-apps/api/event").then(async ({ listen }) => {
      const stop = await listen("sayknow:open", () => void scanDisplays())
      if (alive) unlisten = stop
      else stop()
    })
    .catch(() => {
      if (alive) void scanDisplays()
    })
    return () => {
      alive = false
      unlisten?.()
    }
  }, [active])

  // Slider commits fire one command each. The optimistic value lives in the
  // row component; a rescan on error is the honest correction.
  const apply = useCallback(async (id: string, value: number) => {
    try {
      const { invoke } = await import("@tauri-apps/api/core")
      await invoke("set_display_brightness", { id, value })
    } catch {
      void scanDisplays(true)
    }
  }, [])

  const applyBacklight = useCallback(async (value: number) => {
    try {
      const { invoke } = await import("@tauri-apps/api/core")
      await invoke<number>("set_builtin_backlight", { value })
      void scanDisplays(true)
    } catch {
      // The first attempt opens macOS Accessibility settings. Once SayKnow
      // Kit is allowed, the next drag controls the real backlight.
    }
  }, [])

  const applyAll = useCallback(
    async (value: number) => {
      await Promise.all(displays.map((d) => apply(d.id, value)))
    },
    [displays, apply],
  )

  const togglePower = useCallback(async (id: string, on: boolean) => {
    try {
      const { invoke } = await import("@tauri-apps/api/core")
      await invoke("set_display_power", { id, on })
      // Keep the optimistic card state while a sleeping display disappears
      // from CoreGraphics; the Rust DDC worker retains its wake handle.
      setTimeout(() => void scanDisplays(true), on ? 1200 : 2500)
      return true
    } catch {
      void scanDisplays(true)
      return false
    }
  }, [])

  if (!isTauri()) {
    return (
      <div className="flex h-full items-center justify-center p-6 text-center text-xs text-muted-foreground">
        {t("tools.desktopOnly")}
      </div>
    )
  }

  const externalCount = displays.filter((d) => d.kind === "external").length

  return (
    <div className="flex h-full flex-col">
      <div className="flex shrink-0 items-center justify-between gap-2 border-b bg-muted/30 px-3 py-2">
        <div className="flex items-center gap-1.5 text-xs font-medium">
          <Wrench className="h-3.5 w-3.5" />
          {t("tools.heading")}
        </div>
        <Button
          variant="ghost"
          size="sm"
          className="h-6 px-2 text-[11px]"
          onClick={() => void refreshAll()}
          title={t("tools.refresh")}
        >
          <RefreshCw className="h-3 w-3" />
        </Button>
      </div>

      <div className="flex-1 space-y-2 overflow-y-auto p-2.5">
        <SystemMetricsSection state={metrics} t={t} />
        <section className="rounded-lg border bg-muted/30 p-2.5">
          <div className="mb-1.5 flex items-center gap-1.5 text-[11px] font-medium">
            <Sun className="h-3.5 w-3.5" />
            {t("tools.brightness.title")}
          </div>
          <p className="mb-2 text-[10px] leading-relaxed text-muted-foreground">
            {t("tools.brightness.body")}
          </p>

          {error && (
            <p className="mb-2 rounded-md border border-destructive/40 bg-destructive/10 px-2 py-1.5 text-[10px] text-destructive">
              {error}
            </p>
          )}

          <AccessibilityNotice
            state={accessibility}
            needed={displays.some(
              (d) => d.kind === "builtin" && d.method !== "backlight",
            )}
            t={t}
          />

          {loaded && displays.length === 0 && (
            <p className="py-4 text-center text-[11px] text-muted-foreground">
              {t("tools.brightness.none")}
            </p>
          )}

          {displays.length > 0 && (
            <AllSlider
              label={t("tools.brightness.all")}
              onCommit={(v) => void applyAll(v)}
              disabled={displays.length < 2}
              hint={t("tools.brightness.allHint")}
            />
          )}

          <div className="mt-1 space-y-1.5">
            {displays.map((d) => (
              <DisplayControl
                key={d.id}
                display={d}
                t={t}
                onCommit={(v) => void apply(d.id, v)}
                onPower={(on) => togglePower(d.id, on)}
                onBacklight={(v) => void applyBacklight(v)}
              />
            ))}
          </div>
        </section>

        {loaded && externalCount > 0 && (
          <p className="px-0.5 text-[10px] leading-relaxed text-muted-foreground">
            {t("tools.brightness.ddcNote")}
          </p>
        )}

        <UsagePanel settings={settings} active={active} />
      </div>
    </div>
  )
}

/** One honest explanation instead of an endless macOS consent dialog. A
 *  quarantined copy cannot keep the grant at all, so that case gets its own
 *  instruction rather than a button that would prompt forever. */
function AccessibilityNotice({
  state,
  needed,
  t,
}: {
  state: AccessibilityState | null
  needed: boolean
  t: (key: string) => string
}) {
  if (!state || state.trusted || !needed) return null
  return (
    <div className="mb-2 rounded-md border border-amber-500/40 bg-amber-500/10 px-2 py-1.5">
      <div className="flex items-center gap-1.5 text-[11px] font-medium">
        <ShieldAlert className="h-3.5 w-3.5" />
        {t("tools.brightness.axTitle")}
      </div>
      <p className="mt-1 text-[10px] leading-relaxed text-muted-foreground">
        {state.translocated
          ? t("tools.brightness.axTranslocated")
          : t("tools.brightness.axBody")}
      </p>
      {/* An ad-hoc build pins the grant to this exact binary, so the entry the
          user already switched on belongs to the previous version. */}
      {!state.translocated && state.adhoc && (
        <p className="mt-1 text-[10px] leading-relaxed text-muted-foreground">
          {t("tools.brightness.axAdhoc")}
        </p>
      )}
      {!state.translocated && (
        <div className="mt-1.5 flex flex-wrap gap-1.5">
          <Button
            variant="outline"
            size="sm"
            className="h-6 px-2 text-[10px]"
            onClick={() => void requestAccessibility()}
          >
            {t("tools.brightness.axGrant")}
          </Button>
          {/* A stale entry cannot be repaired by asking again: the row has to
              go first, and doing it here spares the user a terminal. */}
          {state.adhoc && (
            <Button
              variant="outline"
              size="sm"
              className="h-6 px-2 text-[10px]"
              onClick={() => void resetAccessibility()}
            >
              {t("tools.brightness.axReset")}
            </Button>
          )}
          {/* macOS decides trust at process start: an app that was denied when
              it launched stays denied until it restarts, however many times the
              switch is toggled. */}
          <Button
            variant="ghost"
            size="sm"
            className="h-6 px-2 text-[10px]"
            onClick={() => void relaunchApp()}
          >
            {t("tools.brightness.axRestart")}
          </Button>
        </div>
      )}
    </div>
  )
}

function SystemMetricsSection({ state, t }: { state: ReturnType<typeof getMetricsSnapshot>; t: (key: string) => string }) {
  const age = state.age_ms
  const stale = state.status === "stale" || state.status === "stale_with_error" || (age != null && age > 6000)
  const label = (kind: string) => t(`tools.metrics.${kind}`)
  const value = (metric: CpuMetric | ResourceMetric | TemperatureMetric): string => {
    if (metric.state === "available") {
      if ("percent" in metric) return formatPercent(metric.percent)
      if ("celsius" in metric) return `${metric.celsius.toFixed(1)} °C`
      return `${formatBytes(metric.used_bytes)} / ${formatBytes(metric.total_bytes)}`
    }
    if (metric.state === "warming_up") return label("warming")
    if (metric.reason === "no_verified_package_sensor") return label("temperatureUnavailable")
    return label("unavailable")
  }
  const snapshot = state.snapshot
  const seconds = age == null ? null : label("seconds").replace("{count}", `${Math.floor(age / 1000)}`)
  const statusParts = snapshot
    ? [stale ? label("stale") : "", state.refreshing ? label("refreshing") : ""].filter(Boolean)
    : [label(state.status === "initial_error" ? "error" : "loading")]
  const statusText = statusParts.join(" · ")
  return (
    <section
      aria-label={label("title")}
      className="rounded-lg border bg-muted/30 p-2.5"
    >
      <div className="mb-1.5 flex items-center justify-between text-xs font-medium">
        <span>{label("title")}</span>
        <span aria-atomic="true" aria-live="polite" role="status" className="text-muted-foreground">
          {statusText}
        </span>
      </div>
      {!snapshot ? (
        <p className="text-xs text-muted-foreground">
          {label(state.status === "initial_error" ? "error" : "loading")}
        </p>
      ) : (
        <div className="grid grid-cols-2 gap-1.5 text-xs">
          <div><span className="text-muted-foreground">{label("cpu")}</span><div>{value(snapshot.cpu)}</div></div>
          <div><span className="text-muted-foreground">{label("memory")}</span><div>{value(snapshot.memory)}</div></div>
          <div><span className="text-muted-foreground">{label("storage")}</span><div>{value(snapshot.storage)}</div></div>
          <div><span className="text-muted-foreground">{label("temperature")}</span><div>{value(snapshot.cpu_package_temperature)}</div></div>
        </div>
      )}
      {state.error && (
        <div className="mt-1 flex items-center justify-between gap-2 text-xs text-destructive">
          <span>{label("error")}: {state.error}</span>
          <Button variant="ghost" size="sm" className="h-6 px-2 text-xs" onClick={() => void refreshMetrics()}>
            {label("retry")}
          </Button>
        </div>
      )}
      {state.listener_error && (
        <p className="mt-1 text-xs text-muted-foreground">{label("listenerError")}</p>
      )}
      {seconds != null && (
        <p className="mt-1 text-xs text-muted-foreground">{label("updated").replace("{age}", seconds)}</p>
      )}
    </section>
  )
}
function AllSlider({
  label,
  hint,
  onCommit,
  disabled,
}: {
  label: string
  hint: string
  onCommit: (v: number) => void
  disabled: boolean
}) {
  const [v, setV] = useState(100)
  return (
    <div
      className={cn(
        "rounded-md border p-2",
        disabled && "pointer-events-none opacity-40",
      )}
    >
      <div className="flex items-baseline justify-between text-[11px]">
        <span className="font-medium">{label}</span>
        <span className="tabular-nums text-muted-foreground">{v}%</span>
      </div>
      <Slider
        aria-label={label}
        value={[v]}
        onValueChange={([n]) => setV(n)}
        onValueCommit={([n]) => onCommit(n)}
        className="mt-1.5"
      />
      <p className="mt-0.5 text-[10px] text-muted-foreground">{hint}</p>
    </div>
  )
}

function DisplayControl({
  display,
  t,
  onCommit,
  onPower,
  onBacklight,
}: {
  display: DisplayRow
  t: (k: string) => string
  onCommit: (v: number) => void
  onPower: (on: boolean) => Promise<boolean>
  onBacklight: (v: number) => void
}) {
  const [v, setV] = useState(display.brightness ?? 100)
  const [busy, setBusy] = useState(false)

  const [backlightV, setBacklightV] = useState(display.system_level ?? 100)
  // Hardware value wins when it changes; the local value is only the
  // optimistic drag preview. Adopted during render rather than in an effect.
  const [seenBrightness, setSeenBrightness] = useState(display.brightness)
  if (display.brightness !== seenBrightness) {
    setSeenBrightness(display.brightness)
    if (display.brightness !== null) setV(display.brightness)
  }

  const [seenSystemLevel, setSeenSystemLevel] = useState(display.system_level)
  if (display.system_level !== seenSystemLevel) {
    setSeenSystemLevel(display.system_level)
    if (display.system_level !== null) setBacklightV(display.system_level)
  }

  const [isOn, setIsOn] = useState(display.power !== false)
  const [seenPower, setSeenPower] = useState(display.power)
  if (display.power !== seenPower) {
    setSeenPower(display.power)
    if (display.power !== null) setIsOn(display.power)
  }

  const applyPower = async (on: boolean) => {
    const previous = isOn
    setIsOn(on)
    setBusy(true)
    const accepted = await onPower(on)
    if (!accepted) setIsOn(previous)
    setBusy(false)
  }

  const name =
    display.kind === "builtin" ? t("tools.brightness.builtin") : display.name

  return (
    <div className="rounded-md border p-2">
      <div className="flex items-center gap-1.5">
        {display.kind === "builtin" ? (
          <Sun className="h-3 w-3 shrink-0 text-muted-foreground" />
        ) : isOn ? (
          <Monitor className="h-3 w-3 shrink-0 text-muted-foreground" />
        ) : (
          <MonitorOff className="h-3 w-3 shrink-0 text-muted-foreground/50" />
        )}
        <span className="truncate text-[11px] font-medium" title={display.name}>
          {name}
        </span>
        {display.is_main && (
          <span className="rounded bg-primary/15 px-1 py-px text-[9px] font-medium text-primary">
            {t("tools.brightness.main")}
          </span>
        )}
        {display.method === "gamma" && (
          <span className="rounded bg-muted px-1 py-px text-[9px] text-muted-foreground">
            {t("tools.brightness.softwareDim")}
          </span>
        )}
        {display.kind === "external" && (
          <span className="rounded bg-muted px-1 py-px text-[9px] text-muted-foreground">
            {t("tools.brightness.external")}
          </span>
        )}
        <span className="ml-auto tabular-nums text-[10px] text-muted-foreground">
          {display.brightness === null ? "—" : `${v}%`}
        </span>
        {display.kind === "external" && (
          <div className="flex items-center gap-0.5">
            <Button
              variant="ghost"
              size="sm"
              className={cn(
                "h-5 gap-0.5 px-1 text-[8px]",
                isOn && "bg-emerald-500/10 text-emerald-500",
              )}
              disabled={busy}
              title={t("tools.brightness.powerOn")}
              onClick={() => void applyPower(true)}
            >
              <Power className="h-2.5 w-2.5" />
              ON
            </Button>
            <Button
              variant="ghost"
              size="sm"
              className={cn(
                "h-5 gap-0.5 px-1 text-[8px]",
                !isOn && "bg-muted text-muted-foreground",
              )}
              disabled={busy}
              title={t("tools.brightness.powerOff")}
              onClick={() => void applyPower(false)}
            >
              <MonitorOff className="h-2.5 w-2.5" />
              OFF
            </Button>
          </div>
        )}
      </div>
      {display.method === "gamma" && display.system_level !== null && (
        <div className="mt-1.5 flex items-center gap-1.5">
          <span className="shrink-0 text-[9px] text-muted-foreground">
            {t("tools.brightness.backlight")}
          </span>
          <Slider
            aria-label={`${display.name} ${t("tools.brightness.backlight")}`}
            value={[backlightV]}
            min={0}
            onValueChange={([n]) => setBacklightV(n)}
            onValueCommit={([n]) => onBacklight(n)}
            className="flex-1"
          />
          <span className="w-7 shrink-0 text-right tabular-nums text-[9px] text-muted-foreground">
            {backlightV}%
          </span>
        </div>
      )}
      {display.controllable ? (
        <Slider
          aria-label={`${display.name} ${display.method === "gamma" ? t("tools.brightness.softwareDim") : t("tools.brightness.title")}`}
          value={[v]}
          min={0}
          onValueChange={([n]) => setV(n)}
          onValueCommit={([n]) => onCommit(n)}
          className="mt-1.5"
        />
      ) : (
        // Present but not drivable at all (no backlight API, no gamma):
        // say so instead of a dead slider pretending to work.
        <p className="mt-1.5 rounded bg-muted/60 px-1.5 py-1 text-[10px] leading-snug text-muted-foreground">
          {t("tools.brightness.builtinUnsupported")}
        </p>
      )}

    </div>
  )
}
