import { useEffect, useRef, useState } from "react"
import { Gauge, X } from "lucide-react"
import { Button } from "@/components/ui/button"
import { formatBytes } from "@/lib/system-metrics-store"
import { useReloadHold } from "@/lib/idle-reload"
import { isMacPlatform } from "@/lib/shortcuts"
import {
  formatMbps,
  gaugeFraction,
  gaugeScale,
  speedErrorKey,
  type SpeedTestProgress,
  type SpeedTestResult,
} from "@/lib/speed-test"
import { cn } from "@/lib/utils"

type T = (key: string) => string
type Status = "idle" | "running" | "done" | "error"
type Direction = "download" | "upload"

const press = "transition-none active:scale-[0.97] motion-reduce:active:scale-100"

/** `-M 20` on the backend; the clock stops there even if the tool runs a little over. */
const RUN_SECONDS = 20

const time = (value: number) => new Date(value).toLocaleTimeString([], { hour: "2-digit", minute: "2-digit" })

/** A 270° dial opening at the bottom, centre (50,50), radius 40. */
const ARC = "M 21.716 78.284 A 40 40 0 1 1 78.284 78.284"

function SpeedGauge({
  label,
  bps,
  scaleMbps,
  live,
  waiting,
  unknown,
}: {
  label: string
  bps: number | null
  scaleMbps: number
  live: boolean
  waiting: boolean
  unknown: string
}) {
  const fraction = gaugeFraction(bps ?? 0, scaleMbps)
  const value = formatMbps(bps)
  return (
    <div
      data-gauge={label}
      data-live={live ? "true" : "false"}
      className={cn("flex flex-col items-center", waiting && "opacity-50")}
    >
      <div className="relative w-full max-w-[132px]">
        <svg aria-hidden="true" viewBox="0 0 100 86" className="block w-full">
          <path d={ARC} fill="none" stroke="currentColor" strokeLinecap="round" strokeWidth="7" className="text-foreground/10" />
          {/* Readings land every ~0.25 s; a linear glide of the same length
              keeps the needle moving instead of stepping, and reduced motion
              gets the plain step. */}
          <path
            d={ARC}
            fill="none"
            pathLength={100}
            stroke="currentColor"
            strokeDasharray="100"
            strokeDashoffset={100 - fraction * 100}
            strokeLinecap="round"
            strokeWidth="7"
            className={cn(
              "text-primary transition-[stroke-dashoffset] duration-[250ms] ease-linear motion-reduce:transition-none",
              fraction === 0 && "opacity-0",
            )}
          />
        </svg>
        <div className="absolute inset-x-0 top-[34%] flex flex-col items-center leading-none">
          <span className="text-lg font-semibold tabular-nums select-text">
            {value ? value.replace(" Mbps", "") : "–"}
          </span>
          <span className="mt-0.5 text-[10px] text-muted-foreground">{value ? "Mbps" : unknown}</span>
        </div>
        <div aria-hidden="true" className="absolute inset-x-[14%] bottom-0 flex justify-between text-[10px] text-muted-foreground tabular-nums">
          <span>0</span>
          <span>{scaleMbps}</span>
        </div>
      </div>
      <span className="mt-1 flex items-center gap-1 text-xs font-medium">
        {live && <span aria-hidden="true" className="size-1.5 rounded-full bg-primary" />}
        {label}
      </span>
    </div>
  )
}

/** Runs only when asked: one test moves a few hundred MB, so it never starts by itself. */
export function SpeedTestPanel({ t }: { t: T }) {
  const [status, setStatus] = useState<Status>("idle")
  const [result, setResult] = useState<SpeedTestResult | null>(null)
  const [progress, setProgress] = useState<SpeedTestProgress | null>(null)
  const [peak, setPeak] = useState({ download: 0, upload: 0 })
  const [finishedAt, setFinishedAt] = useState<number | null>(null)
  const [error, setError] = useState<string | null>(null)
  const mounted = useRef(true)
  const running = status === "running"
  const runningRef = useRef(false)
  useEffect(() => {
    runningRef.current = running
  }, [running])
  useReloadHold(running)

  // Leaving the tab must not leave a test eating the connection unseen. Only
  // on unmount: cancelling when a run merely ends could land on the next run.
  useEffect(() => {
    mounted.current = true
    return () => {
      mounted.current = false
      if (!runningRef.current) return
      void import("@tauri-apps/api/core").then(({ invoke }) => invoke("cancel_speed_test")).catch(() => {})
    }
  }, [])

  const start = async () => {
    setStatus("running")
    setError(null)
    setProgress(null)
    setPeak({ download: 0, upload: 0 })
    try {
      const { Channel, invoke } = await import("@tauri-apps/api/core")
      const channel = new Channel<SpeedTestProgress>()
      channel.onmessage = (reading) => {
        if (!mounted.current) return
        setProgress(reading)
        setPeak((p) => ({
          download: Math.max(p.download, reading.download_bps),
          upload: Math.max(p.upload, reading.upload_bps),
        }))
      }
      const next = await invoke<SpeedTestResult>("run_speed_test", { progress: channel })
      if (!mounted.current) return
      setResult(next)
      setFinishedAt(Date.now())
      setStatus("done")
    } catch (e) {
      if (!mounted.current) return
      setError(speedErrorKey(e))
      setStatus("error")
    }
  }

  const cancel = async () => {
    try {
      const { invoke } = await import("@tauri-apps/api/core")
      await invoke("cancel_speed_test")
    } catch {
      // The run itself reports how it ended.
    }
  }

  const mac = isMacPlatform()
  // Sequential mode measures download first; upload starts reading above 0.
  const phase: Direction = progress && progress.upload_bps > 0 ? "upload" : "download"
  const reading = (direction: Direction): number | null =>
    running ? (progress?.[`${direction}_bps`] ?? 0) : (result?.[`${direction}_bps`] ?? null)
  const scale = (direction: Direction) =>
    gaugeScale(Math.max(running ? peak[direction] : 0, reading(direction) ?? 0))
  const seconds = Math.min(RUN_SECONDS, Math.floor((progress?.elapsed_ms ?? 0) / 1000))
  const showGauges = running || result != null

  const rows: [string, string | null][] = result
    ? [
        [t("tools.speed.latency"), result.idle_latency_ms == null ? null : `${Math.round(result.idle_latency_ms)} ms`],
        [t("tools.speed.dataUsed"), result.bytes_used == null ? null : formatBytes(result.bytes_used, 1000)],
        [t("tools.speed.connection"), result.interface],
        [t("tools.speed.server"), result.server],
      ]
    : []

  return (
    <section aria-label={t("tools.speed.title")} className="space-y-2 rounded-lg border bg-muted/30 p-2.5">
      <div className="flex items-center justify-between gap-2 text-xs font-medium">
        <span className="flex items-center gap-1.5">
          <Gauge aria-hidden="true" className="size-3.5" />
          {t("tools.speed.title")}
        </span>
        <span aria-atomic="true" aria-live="polite" role="status" className="text-muted-foreground">
          {running
            ? t("tools.speed.running")
            : status === "done" && finishedAt != null
              ? t("tools.speed.measuredAt").replace("{time}", time(finishedAt))
              : ""}
        </span>
      </div>

      {!showGauges && (
        <>
          <p className="text-xs leading-relaxed text-muted-foreground">{t("tools.speed.intro")}</p>
          <p className="text-xs leading-relaxed text-muted-foreground">{t("tools.speed.dataNote")}</p>
        </>
      )}

      {!mac && <p className="rounded-md bg-background/60 px-2 py-1.5 text-xs">{t("tools.speed.error.unsupported")}</p>}

      {showGauges && (
        <div className={cn("rounded-md bg-background/60 px-2 pt-2 pb-1.5", !running && status === "error" && "opacity-60")}>
          {!running && status === "error" && <p className="mb-1 text-xs text-muted-foreground">{t("tools.speed.previous")}</p>}
          <div className="grid grid-cols-2 gap-2">
            {(["download", "upload"] as const).map((direction) => (
              <SpeedGauge
                key={direction}
                label={t(`tools.speed.${direction}`)}
                bps={reading(direction)}
                scaleMbps={scale(direction)}
                live={running && phase === direction}
                waiting={running && direction === "upload" && phase === "download"}
                unknown={t("tools.speed.unknown")}
              />
            ))}
          </div>
          {running && (
            <div className="mt-2 flex items-center gap-2" aria-hidden="true">
              <div className="h-1 flex-1 overflow-hidden rounded-full bg-foreground/10">
                <div
                  className="h-full origin-left rounded-full bg-primary transition-transform duration-[250ms] ease-linear motion-reduce:transition-none"
                  style={{ transform: `scaleX(${Math.min(1, (progress?.elapsed_ms ?? 0) / (RUN_SECONDS * 1000))})` }}
                />
              </div>
              <span className="text-[10px] text-muted-foreground tabular-nums">
                {seconds}/{RUN_SECONDS}s
              </span>
            </div>
          )}
          {!running &&
            rows.map(([label, value]) => (
              <div key={label} className="flex items-baseline justify-between gap-2 py-0.5 text-xs">
                <span className="text-muted-foreground">{label}</span>
                <span className="min-w-0 break-all text-right font-medium tabular-nums select-text">
                  {value ?? t("tools.speed.unknown")}
                </span>
              </div>
            ))}
        </div>
      )}

      {error && (
        <p role="alert" className="rounded-md border border-destructive/40 bg-destructive/10 px-2 py-1.5 text-xs text-destructive">
          {t(error)}
        </p>
      )}

      <div className="flex flex-wrap items-center gap-2">
        <Button size="sm" variant="secondary" className={`text-xs ${press}`} disabled={running || !mac} onClick={() => void start()}>
          <Gauge aria-hidden="true" className="size-3.5" />
          {result ? t("tools.speed.again") : t("tools.speed.start")}
        </Button>
        {running && (
          <Button size="sm" variant="ghost" className={`text-xs ${press}`} onClick={() => void cancel()}>
            <X aria-hidden="true" className="size-3.5" />
            {t("tools.speed.cancel")}
          </Button>
        )}
      </div>
    </section>
  )
}
