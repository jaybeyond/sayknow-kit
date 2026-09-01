import { useCallback, useEffect, useRef, useState, useSyncExternalStore } from "react"
import {
  Monitor,
  MonitorOff,
  Power,
  RefreshCw,
  Sun,
  Wrench,
} from "lucide-react"
import { Button } from "@/components/ui/button"
import { Slider } from "@/components/ui/slider"
import type { Settings } from "@/hooks/useSettings"
import { useT } from "@/i18n"
import { isTauri } from "@/lib/runtime"
import {
  getSnapshot,
  scanDisplays,
  subscribe,
  syncBuiltin,
  type DisplayRow,
} from "@/lib/tools-store"
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
  const { displays, error, loaded } = useSyncExternalStore(subscribe, getSnapshot)
  const accessibilityRequested = useRef(false)

  const refresh = useCallback(() => void scanDisplays(true), [])

  // DDC reads take tens of ms per display, so only scan while visible. The
  // effect is a pure trigger; state lands in the store.
  useEffect(() => {
    if (!active) return
    void scanDisplays()
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

  // Real built-in backlight control is local Control Center automation. Ask
  // once when the Tools tab first sees the built-in display; macOS owns the
  // consent UI, and subsequent launches are silent once permission is granted.
  useEffect(() => {
    if (
      !active ||
      accessibilityRequested.current ||
      !displays.some((d) => d.kind === "builtin" && d.method === "gamma")
    ) {
      return
    }
    accessibilityRequested.current = true
    void import("@tauri-apps/api/core")
      .then(({ invoke }) =>
        invoke<boolean>("request_accessibility_permission").then((trusted) => {
          if (trusted) void scanDisplays(true)
        }),
      )
      .catch(() => {
        // Non-macOS builds expose the command as a no-op.
      })
  }, [active, displays])

  // Rescan every time the popover opens: monitors connect, wake, or lock
  // their DDC while the app runs, and a list scanned once at tab-activation
  // goes stale — an external that came back showed as nothing at all.
  useEffect(() => {
    if (!isTauri()) return
    let alive = true
    void import("@tauri-apps/api/event").then(({ listen }) => {
      if (!alive) return
      listen("sayknow:open", () => void scanDisplays(true))
    })
    return () => {
      alive = false
    }
  }, [])

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
          onClick={() => void refresh()}
          title={t("tools.refresh")}
        >
          <RefreshCw className="h-3 w-3" />
        </Button>
      </div>

      <div className="flex-1 space-y-2 overflow-y-auto p-2.5">
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
      </div>
    </div>
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
