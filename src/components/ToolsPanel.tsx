import { useCallback, useEffect, useState, useSyncExternalStore } from "react"
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
 * first one is screen brightness: hardware DDC for externals, IOKit backlight
 * for the built-in panel — no gamma overlay, so what you set is what the
 * monitor does.
 */
export function ToolsPanel({ settings, active }: Props) {
  const { t } = useT(settings.uiLocale)
  const { displays, error, loaded } = useSyncExternalStore(subscribe, getSnapshot)

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
      // A monitor entering standby stops acking DDC reads; don't rescan
      // immediately or every field shows as unknown.
      setTimeout(() => void scanDisplays(true), 2500)
    } catch {
      void scanDisplays(true)
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
                onPower={(on) => void togglePower(d.id, on)}
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
}: {
  display: DisplayRow
  t: (k: string) => string
  onCommit: (v: number) => void
  onPower: (on: boolean) => void
}) {
  const [v, setV] = useState(display.brightness ?? 100)
  const [busy, setBusy] = useState(false)

  // Hardware value wins when it changes; the local value is only the
  // optimistic drag preview. Adopted during render rather than in an effect.
  const [seenBrightness, setSeenBrightness] = useState(display.brightness)
  if (display.brightness !== seenBrightness) {
    setSeenBrightness(display.brightness)
    if (display.brightness !== null) setV(display.brightness)
  }

  const name =
    display.kind === "builtin" ? t("tools.brightness.builtin") : display.name
  const isOn = display.power !== false

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
          <Button
            variant="ghost"
            size="sm"
            className="h-5 w-5 p-0"
            disabled={busy}
            title={isOn ? t("tools.brightness.powerOff") : t("tools.brightness.powerOn")}
            onClick={() => {
              setBusy(true)
              onPower(!isOn)
              setTimeout(() => setBusy(false), 3000)
            }}
          >
            <Power
              className={cn(
                "h-3 w-3",
                isOn ? "text-muted-foreground" : "text-muted-foreground/40",
              )}
            />
          </Button>
        )}
      </div>
      {display.controllable ? (
        <Slider
          value={[v]}
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
