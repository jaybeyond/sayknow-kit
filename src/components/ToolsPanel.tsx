import { Fragment, useCallback, useEffect, useRef, useState, useSyncExternalStore } from "react"
import { useSlidingPill } from "@/hooks/useSlidingPill"
import {
  Monitor,
  MonitorOff,
  Power,
  RefreshCw,
  ShieldAlert,
  Sun,
} from "lucide-react"
import { UsagePanel } from "@/components/UsagePanel"
import { MolePanel } from "@/components/MolePanel"
import { SpeedTestPanel } from "@/components/SpeedTestPanel"
import { Button } from "@/components/ui/button"
import { Slider } from "@/components/ui/slider"
import type { Settings } from "@/hooks/useSettings"
import { useT } from "@/i18n"
import { getSnapshot as getMoleSnapshot, refreshScans, subscribe as subscribeMole } from "@/lib/mole-store"
import { scanAgentUsage } from "@/lib/agent-usage-store"
import { isTauri } from "@/lib/runtime"
import {
  getSnapshot,
  refreshAccessibility,
  relaunchApp,
  reportError,
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
  formatRate,
  type BatteryMetric,
  type CpuMetric,
  type GpuMetric,
  type NetworkMetric,
  type ResourceMetric,
  type TemperatureMetric,
} from "@/lib/system-metrics-store"
import {
  GAP_MS,
  getSnapshot as getActivitySnapshot,
  setActive as setActivityActive,
  subscribe as subscribeActivity,
  type HistoryPoint,
  type ProcessUsage,
  type TopProcesses,
} from "@/lib/system-activity-store"
import { spanMinutes, sparklinePath, visibleSamples } from "@/lib/sparkline"
import { cn } from "@/lib/utils"
import { brightnessCommand } from "@/lib/brightness-command"

type Props = {
  settings: Settings
  active: boolean
}

type ToolTab = "status" | "display" | "usage" | "mole"

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
  const moleBusy = useSyncExternalStore(subscribeMole, () => getMoleSnapshot().busy)
  const [tab, setTab] = useState<ToolTab>("status")
  const toolTabsRef = useRef<HTMLDivElement>(null)
  const toolPillRef = useRef<HTMLSpanElement>(null)
  useSlidingPill(toolTabsRef, toolPillRef, tab)
  const [refreshing, setRefreshing] = useState(false)

  const refreshAll = useCallback(async () => {
    if (tab === "mole" && getMoleSnapshot().busy !== null) return
    setRefreshing(true)
    try {
      const jobs: Promise<unknown>[] = [scanDisplays(true), refreshMetrics()]
      if (tab === "mole") jobs.push(refreshScans())
      // The header refresh is the one users reach for; on this tab it must read the logs again.
      if (tab === "usage") jobs.push(scanAgentUsage(true, settings.deeplKey))
      await Promise.all(jobs)
    } finally {
      setRefreshing(false)
    }
  }, [tab, settings.deeplKey])

  // DDC reads take tens of ms per display, so only scan while visible. The
  // effect is a pure trigger; state lands in the store.
  useEffect(() => {
    if (!active) return
    void scanDisplays()
  }, [active])
  useEffect(() => {
    setMetricsActive(active && tab === "status")
    return () => setMetricsActive(false)
  }, [active, tab])
  // Graphs and the process list poll only while they are on screen: walking
  // the process table is the costliest thing this panel does.
  useEffect(() => {
    setActivityActive(active && tab === "status")
    return () => setActivityActive(false)
  }, [active, tab])

  // Follow the keyboard keys: the built-in slider is an absolute brightness
  // whose system half changes under us. The sync command is pure math on the
  // tap-tracked level (no DDC, no registry hit), so 250ms is cheap and makes
  // the thumb feel attached to F1/F2.
  const displayActive = active && tab === "display"
  const hasBuiltin = displays.some((d) => d.kind === "builtin")
  const needsAccessibility = displays.some(
    (d) => d.kind === "builtin" && d.method !== "backlight",
  )
  useEffect(() => {
    if (!displayActive || !hasBuiltin) return
    const timer = setInterval(() => void syncBuiltin(), 250)
    return () => clearInterval(timer)
  }, [displayActive, hasBuiltin])

  // Never auto-prompt. macOS re-shows the same consent dialog on every call,
  // and an app running from quarantine can never keep the grant, so the loop
  // the user saw was infinite. Read the state and say it once, in-app.
  useEffect(() => {
    if (!displayActive) return
    void refreshAccessibility()
  }, [displayActive])

  // While the notice is up the user is in System Settings toggling the switch.
  // Poll so the panel reacts the moment macOS grants trust, instead of looking
  // like it is still demanding permission that was already given.
  const awaitingTrust = displayActive && needsAccessibility && accessibility !== null && !accessibility.trusted
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
    } catch (e) {
      // Never swallow this silently: a slider that moves while the screen
      // does not is exactly the bug that hid behind an empty catch.
      console.error("set_display_brightness failed", id, value, e)
      void scanDisplays(true)
    }
  }, [])

  // The built-in panel's real backlight, through Control Center. The first
  // attempt opens macOS Accessibility settings if the grant is missing.
  const applyBacklight = useCallback(async (value: number) => {
    try {
      const { invoke } = await import("@tauri-apps/api/core")
      await invoke<number>("set_builtin_backlight", { value })
    } catch (e) {
      console.error("set_builtin_backlight failed", value, e)
    }
  }, [])

  // Built-in and external panels are driven by different mechanisms and
  // never share one: DDC for externals, the Control Center backlight for the
  // built-in. brightnessCommand is the single place that decides.
  const applyDisplay = useCallback(
    async (display: DisplayRow, value: number) => {
      const target = brightnessCommand(display)
      if (!target) return
      if (target.command === "set_builtin_backlight") {
        await applyBacklight(value)
        return
      }
      await apply(target.id, value)
    },
    [apply, applyBacklight],
  )

  const applyAll = useCallback(
    async (value: number) => {
      await Promise.all(displays.map((d) => applyDisplay(d, value)))
    },
    [displays, applyDisplay],
  )

  const togglePower = useCallback(async (id: string, on: boolean) => {
    try {
      const { invoke } = await import("@tauri-apps/api/core")
      await invoke("set_display_power", { id, on })
      // Disconnect drops the panel from CoreGraphics. A forced DDC rescan
      // then hangs 10s on mute hubs (Xiaomi) and the card vanishes until
      // Refresh. Reuse the worker cache — keep_when_missing already holds
      // the disconnected row.
      await scanDisplays(false)
      return true
    } catch (e) {
      // A refused power command used to vanish here: the toggle sprang back,
      // the monitor stayed as it was, and nothing said why. The rescan clears
      // the banner on success, so report after it.
      await scanDisplays(false)
      reportError(String(e))
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
      <div className="flex shrink-0 items-center gap-2 border-b bg-muted/30 px-2 py-1.5">
        {/* A segmented control: each tab starts at its label's width and the
            spare room is shared equally, so gaps stay even in every locale and
            "Token usage" in es/de still fits; truncation guards the extreme. */}
        <div
          ref={toolTabsRef}
          aria-label={t("tools.tabs.label")}
          className="relative flex min-w-0 flex-1 gap-0.5 rounded-lg bg-black/10 p-0.5 dark:bg-white/10"
          role="tablist"
        >
          <span
            ref={toolPillRef}
            aria-hidden
            className="t-pill rounded-md bg-background shadow-sm ring-1 ring-black/10 dark:bg-white/[0.12] dark:shadow-none dark:ring-white/15"
          />
          <ToolTabButton
            active={tab === "status"}
            label={t("tools.tabs.status")}
            onClick={() => setTab("status")}
          />
          <ToolTabButton
            active={tab === "display"}
            label={t("tools.tabs.display")}
            onClick={() => setTab("display")}
          />
          <ToolTabButton
            active={tab === "usage"}
            label={t("tools.tabs.usage")}
            onClick={() => setTab("usage")}
          />
          <ToolTabButton
            active={tab === "mole"}
            label={t("tools.tabs.mole")}
            onClick={() => setTab("mole")}
          />
        </div>
        <Button
          variant="ghost"
          size="sm"
          className="h-7 w-7 shrink-0 px-0 active:scale-[0.98]"
          disabled={refreshing || (tab === "mole" && moleBusy !== null)}
          onClick={() => void refreshAll()}
          title={t("tools.refresh")}
        >
          <RefreshCw className={cn("h-3.5 w-3.5", refreshing && "animate-spin")} />
        </Button>
      </div>

      {/* Cleanup owns its page bar and scroll area, so its bar stays attached under these tabs. */}
      {tab === "mole" ? <MolePanel t={t} active={active} /> : <div className="min-h-0 flex-1 overflow-y-auto p-2.5" data-tools-scroll>
        {tab === "status" && (
          <div className="space-y-2">
            <SystemMetricsSection state={metrics} t={t} />
            <SpeedTestPanel t={t} />
            <ActivitySection t={t} />
          </div>
        )}

        {tab === "display" && (
          <div className="space-y-2">
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
                needed={needsAccessibility}
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
                  disabled={displays.filter((d) => d.controllable).length < 2}
                  hint={t("tools.brightness.allHint")}
                />
              )}

              <div className="mt-1 space-y-1.5">
                {displays.map((d) => (
                  <DisplayControl
                    key={d.id}
                    display={d}
                    t={t}
                    // Each card drives its panel the way brightnessCommand says,
                    // exactly like "All displays" does. Sending the built-in id to
                    // set_display_brightness handed it to the DDC worker, which
                    // has no built-in panel, so the built-in slider did nothing.
                    onCommit={(v) => void applyDisplay(d, v)}
                    onPower={(on) => togglePower(d.id, on)}
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
        )}

        {tab === "usage" && <UsagePanel settings={settings} active={active} />}
      </div>}
    </div>
  )
}


function ToolTabButton({
  active,
  label,
  onClick,
}: {
  active: boolean
  label: string
  onClick: () => void
}) {
  return (
    <button
      aria-selected={active}
      data-pill-active={active}
      className={cn(
        // A longer label in some locale must not wrap out of the 28px pill.
        // The active background is the sliding pill behind the tabs.
        "relative z-[1] h-7 min-w-0 flex-auto truncate rounded-md px-1.5 text-[11px] font-medium transition-[background-color,color,transform] duration-150 ease-out active:scale-[0.98]",
        active ? "text-foreground" : "text-foreground/70 hover:bg-background/60 hover:text-foreground",
      )}
      onClick={onClick}
      role="tab"
      title={label}
      type="button"
    >
      {label}
    </button>
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

function metricLine(label: string, value: string) {
  return (
    <div key={label} className="flex items-baseline justify-between gap-2 text-[11px]">
      <span className="shrink-0 text-muted-foreground">{label}</span>
      <span className="min-w-0 text-right font-medium tabular-nums [overflow-wrap:anywhere]">{value}</span>
    </div>
  )
}

function cpuLines(cpu: CpuMetric, label: (kind: string) => string): [string, string][] {
  if (cpu.state === "available") {
    const lines: [string, string][] = [[label("cpu"), formatPercent(cpu.percent)]]
    if (cpu.system_percent != null) lines.push([label("cpuSystem"), formatPercent(cpu.system_percent)])
    if (cpu.user_percent != null) lines.push([label("cpuUser"), formatPercent(cpu.user_percent)])
    if (cpu.idle_percent != null) lines.push([label("cpuIdle"), formatPercent(cpu.idle_percent)])
    return lines
  }
  if (cpu.state === "warming_up") return [[label("cpu"), label("warming")]]
  return [[label("cpu"), label("unavailable")]]
}

function gpuLines(metric: GpuMetric, label: (kind: string) => string): [string, string][] {
  return [[label("gpu"), metric.state === "available" ? formatPercent(metric.percent) : label("unavailable")]]
}

function resourceLines(kind: string, metric: ResourceMetric, label: (kind: string) => string): [string, string][] {
  if (metric.state === "available") {
    const base = kind === "storage" ? 1000 : 1024
    return [[label(kind), `${formatBytes(metric.used_bytes, base)} / ${formatBytes(metric.total_bytes, base)}`]]
  }
  return [[label(kind), label("unavailable")]]
}

function temperatureLines(metric: TemperatureMetric, label: (kind: string) => string): [string, string][] {
  if (metric.state === "available") return [[label("temperature"), `${metric.celsius.toFixed(1)} °C`]]
  if (metric.reason === "no_verified_package_sensor") return [[label("temperature"), label("temperatureUnavailable")]]
  return [[label("temperature"), label("unavailable")]]
}

function batteryLines(metric: BatteryMetric, label: (kind: string) => string): [string, string][] {
  if (metric.state === "not_installed") return [[label("battery"), label("notInstalled")]]
  if (metric.state === "unavailable") return [[label("battery"), label("unavailable")]]
  // Wall power and charging are separate: macOS holds a full or optimised
  // battery on the adapter without charging it, which is not "on battery".
  const state = metric.is_charging
    ? label("charging")
    : metric.external_connected
      ? label("pluggedIn")
      : label("onBattery")
  const powerSource = metric.adapter_name && (metric.is_charging || metric.external_connected)
    ? `${state} · ${metric.adapter_name}`
    : state
  const lines: [string, string][] = [
    [label("battery"), formatPercent(metric.percent)],
    [label("powerSource"), powerSource],
  ]
  if (metric.max_capacity_percent != null) lines.push([label("maxCapacity"), formatPercent(metric.max_capacity_percent)])
  if (metric.cycle_count != null) lines.push([label("cycleCount"), String(metric.cycle_count)])
  if (metric.temperature_celsius != null) lines.push([label("batteryTemperature"), `${metric.temperature_celsius.toFixed(1)} °C`])
  return lines
}

function networkLines(metric: NetworkMetric, label: (kind: string) => string): [string, string][] {
  if (metric.state === "warming_up") return [[label("network"), label("warming")]]
  if (metric.state === "unavailable") return [[label("network"), label("unavailable")]]
  return [
    [label("network"), metric.interface],
    [label("localIp"), metric.ip_address || "—"],
    [label("upload"), formatRate(metric.upload_bytes_per_sec)],
    [label("download"), formatRate(metric.download_bytes_per_sec)],
  ]
}

function SystemMetricsSection({ state, t }: { state: ReturnType<typeof getMetricsSnapshot>; t: (key: string) => string }) {
  const age = state.age_ms
  const stale = state.status === "stale" || state.status === "stale_with_error" || (age != null && age > 6000)
  const label = (kind: string) => t(`tools.metrics.${kind}`)
  const snapshot = state.snapshot
  const seconds = age == null ? null : label("seconds").replace("{count}", `${Math.floor(age / 1000)}`)
  const statusParts = snapshot
    ? [stale ? label("stale") : "", state.refreshing ? label("refreshing") : ""].filter(Boolean)
    : [label(state.status === "initial_error" ? "error" : "loading")]
  const statusText = statusParts.join(" · ")
  const cards = snapshot
    ? [
        cpuLines(snapshot.cpu, label),
        gpuLines(snapshot.gpu, label),
        resourceLines("memory", snapshot.memory, label),
        resourceLines("storage", snapshot.storage, label),
        temperatureLines(snapshot.cpu_package_temperature, label),
        batteryLines(snapshot.battery, label),
        networkLines(snapshot.network, label),
      ]
    : []
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
        <div className="grid grid-cols-1 gap-1.5 text-xs">
          {cards.map((lines) => (
            <div key={lines[0][0]} className="rounded-md bg-background/60 px-2 py-1.5">
              {lines.map(([k, v]) => metricLine(k, v))}
            </div>
          ))}
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

type GraphRow = {
  key: string
  label: string
  /** One line, or two stacked (network: down, then up). */
  value: string[]
  max: number
  lines: { values: (number | null)[]; className: string }[]
}

/** The graph rows worth drawing: a figure this Mac never reported (no GPU
 *  statistics, no temperature sensor) gets no row rather than a flat line
 *  that reads as zero. */
function graphRows(points: HistoryPoint[], t: (key: string) => string): GraphRow[] {
  const latest = points[points.length - 1]
  const series = (pick: (p: HistoryPoint) => number | null) => points.map(pick)
  const seen = (values: (number | null)[]) => values.some((v) => v !== null)
  const peak = (values: (number | null)[]) => Math.max(0, ...values.filter((v): v is number => v !== null))
  const pct = (v: number | null) => [v === null ? "—" : formatPercent(v)]
  const rows: GraphRow[] = []
  const cpu = series((p) => p.cpu)
  if (seen(cpu)) {
    rows.push({ key: "cpu", label: t("tools.metrics.cpu"), value: pct(latest.cpu), max: 100, lines: [{ values: cpu, className: "text-primary" }] })
  }
  const gpu = series((p) => p.gpu)
  if (seen(gpu)) {
    rows.push({ key: "gpu", label: t("tools.metrics.gpu"), value: pct(latest.gpu), max: 100, lines: [{ values: gpu, className: "text-primary" }] })
  }
  const memory = series((p) => p.memory)
  if (seen(memory)) {
    rows.push({ key: "memory", label: t("tools.metrics.memory"), value: pct(latest.memory), max: 100, lines: [{ values: memory, className: "text-primary" }] })
  }
  const temperature = series((p) => p.temperature)
  if (seen(temperature)) {
    rows.push({
      key: "temperature",
      label: t("tools.metrics.temperature"),
      value: [latest.temperature === null ? "—" : `${latest.temperature.toFixed(0)} °C`],
      max: Math.max(100, peak(temperature)),
      lines: [{ values: temperature, className: "text-primary" }],
    })
  }
  const download = series((p) => p.download)
  const upload = series((p) => p.upload)
  if (seen(download) || seen(upload)) {
    rows.push({
      key: "network",
      label: t("tools.metrics.network"),
      value: [
        `↓ ${latest.download === null ? "—" : formatRate(latest.download)}`,
        `↑ ${latest.upload === null ? "—" : formatRate(latest.upload)}`,
      ],
      // Traffic has no ceiling; the busiest moment on screen is the top.
      max: Math.max(1, peak(download), peak(upload)),
      lines: [
        { values: upload, className: "text-muted-foreground/60" },
        { values: download, className: "text-primary" },
      ],
    })
  }
  return rows
}

/** A fixed strip: the same box every row, newest sample on its right edge. */
function Sparkline({ points, row }: { points: HistoryPoint[]; row: GraphRow }) {
  return (
    <div className="h-7 min-w-0 overflow-hidden rounded bg-background/60 px-1 py-1">
      <svg viewBox="0 0 100 24" preserveAspectRatio="none" className="block h-full w-full" aria-hidden="true">
        {row.lines.map((line, i) => (
          <path
            key={i}
            d={sparklinePath(
              points.map((p, j) => ({ at: p.at_ms, value: line.values[j] })),
              row.max,
              GAP_MS,
            )}
            className={cn("stroke-current", line.className)}
            fill="none"
            strokeWidth={1.25}
            strokeLinecap="round"
            strokeLinejoin="round"
            vectorEffect="non-scaling-stroke"
          />
        ))}
      </svg>
    </div>
  )
}

function ProcessList({
  title,
  processes,
  pick,
  format,
  t,
}: {
  title: string
  processes: TopProcesses | null
  pick: (p: Extract<TopProcesses, { state: "available" }>) => ProcessUsage[]
  format: (p: ProcessUsage) => string
  t: (key: string) => string
}) {
  const rows = processes?.state === "available" ? pick(processes) : null
  return (
    <div className="min-w-0 rounded-md bg-background/60 px-2 py-1.5">
      <div className="mb-1 text-[10px] text-muted-foreground">{title}</div>
      {rows === null ? (
        <p className="text-[11px] text-muted-foreground">{t("tools.activity.measuring")}</p>
      ) : rows.length === 0 ? (
        <p className="text-[11px] text-muted-foreground">{t("tools.activity.none")}</p>
      ) : (
        <ol className="space-y-0.5">
          {rows.map((p) => (
            <li key={p.pid} className="flex items-baseline justify-between gap-2 text-[11px]">
              <span className="min-w-0 truncate" title={p.name}>
                {p.name}
              </span>
              <span className="shrink-0 font-medium tabular-nums">{format(p)}</span>
            </li>
          ))}
        </ol>
      )}
    </div>
  )
}

/** Recent readings as small graphs, and who is using the machine right now. */
function ActivitySection({ t }: { t: (key: string) => string }) {
  const { points: all, processes } = useSyncExternalStore(subscribeActivity, getActivitySnapshot)
  // Only what the strips show, so each row's scale (network's peak) and the
  // title's span describe the same samples the lines are drawn from.
  const points = visibleSamples(all)
  const rows = points.length >= 2 ? graphRows(points, t) : []
  const title = t("tools.activity.history").replace("{minutes}", String(spanMinutes(points.map((p) => p.at_ms))))

  return (
    <section aria-label={title} className="space-y-2 rounded-lg border bg-muted/30 p-2.5">
      <div className="text-xs font-medium">{title}</div>
      {rows.length === 0 ? (
        <p className="text-[11px] text-muted-foreground">{t("tools.activity.historyEmpty")}</p>
      ) : (
        // One grid for every row: label, graph and value columns line up and
        // keep their width whatever the numbers read, so nothing shifts as
        // "9%" becomes "100%" or the network rate grows a digit.
        <div className="grid grid-cols-[4.5rem_minmax(0,1fr)_5.5rem] items-center gap-x-2 gap-y-1.5 text-[11px]">
          {rows.map((row) => (
            <Fragment key={row.key}>
              <span className="truncate text-muted-foreground">{row.label}</span>
              <Sparkline points={points} row={row} />
              <span
                className={cn(
                  "whitespace-nowrap text-right font-medium tabular-nums",
                  row.value.length > 1 && "text-[10px] leading-tight",
                )}
              >
                {row.value.map((v, i) => (
                  <span key={i} className="block">
                    {v}
                  </span>
                ))}
              </span>
            </Fragment>
          ))}
        </div>
      )}
      <div className="grid grid-cols-2 gap-1.5">
        <ProcessList
          title={t("tools.activity.topCpu")}
          processes={processes}
          pick={(p) => p.by_cpu}
          // Above 100 on purpose: a process busy on several cores is.
          format={(p) => `${p.cpu_percent.toFixed(0)}%`}
          t={t}
        />
        <ProcessList
          title={t("tools.activity.topMemory")}
          processes={processes}
          pick={(p) => p.by_memory}
          format={(p) => formatBytes(p.memory_bytes)}
          t={t}
        />
      </div>
      <p className="text-[10px] leading-snug text-muted-foreground">{t("tools.activity.cpuNote")}</p>
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
}: {
  display: DisplayRow
  t: (k: string) => string
  onCommit: (v: number) => void
  onPower: (on: boolean) => Promise<boolean>
}) {
  // The built-in slider drives the real backlight, so it shows the system
  // level (what the keys change), not the gamma table. Externals show DDC.
  const level = (row: DisplayRow) =>
    row.kind === "builtin" ? (row.system_level ?? row.brightness) : row.brightness
  const [v, setV] = useState(level(display) ?? 100)
  const [busy, setBusy] = useState(false)

  // Hardware value wins when it changes; the local value is only the
  // optimistic drag preview. Adopted during render rather than in an effect.
  const [seenLevel, setSeenLevel] = useState(level(display))
  if (level(display) !== seenLevel) {
    setSeenLevel(level(display))
    const current = level(display)
    if (current !== null && current !== undefined) setV(current)
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
        {display.kind === "external" && display.method === "gamma" && (
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
        {/* Lunar-style BlackOut: disconnect from WindowServer. Not mirroring,
            not DDC sleep. DDC panels also get luminance/contrast 0 first. */}
        {display.kind === "external" && display.power_capable && (
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
      {/* One slider per panel. For the built-in it drives the real backlight
          through Control Center — the same control the F1/F2 keys move. */}
      {display.controllable ? (
        <Slider
          aria-label={`${display.name} ${display.kind === "external" && display.method === "gamma" ? t("tools.brightness.softwareDim") : t("tools.brightness.title")}`}
          value={[v]}
          min={0}
          onValueChange={([n]) => setV(n)}
          onValueCommit={([n]) => onCommit(n)}
          className="mt-1.5"
        />
      ) : (
        // Present but not drivable at all: no backlight API and no gamma for
        // the built-in, no DDC and no gamma for an external. Say which, instead
        // of showing a dead slider that pretends to work.
        <p className="mt-1.5 rounded bg-muted/60 px-1.5 py-1 text-[10px] leading-snug text-muted-foreground">
          {t(
            display.kind === "external"
              ? "tools.brightness.externalUnsupported"
              : "tools.brightness.builtinUnsupported",
          )}
        </p>
      )}

    </div>
  )
}
