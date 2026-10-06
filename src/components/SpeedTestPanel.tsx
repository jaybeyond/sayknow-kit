import { useEffect, useRef, useState } from "react"
import { Gauge, X } from "lucide-react"
import { Button } from "@/components/ui/button"
import { formatBytes } from "@/lib/system-metrics-store"
import { useReloadHold } from "@/lib/idle-reload"
import { isMacPlatform } from "@/lib/shortcuts"
import { formatMbps, speedErrorKey, type SpeedTestResult } from "@/lib/speed-test"

type T = (key: string) => string
type Status = "idle" | "running" | "done" | "error"

const press = "transition-none active:scale-[0.97] motion-reduce:active:scale-100"

const time = (value: number) => new Date(value).toLocaleTimeString([], { hour: "2-digit", minute: "2-digit" })

/** Runs only when asked: one test moves a few hundred MB, so it never starts by itself. */
export function SpeedTestPanel({ t }: { t: T }) {
  const [status, setStatus] = useState<Status>("idle")
  const [result, setResult] = useState<SpeedTestResult | null>(null)
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
    try {
      const { invoke } = await import("@tauri-apps/api/core")
      const next = await invoke<SpeedTestResult>("run_speed_test")
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
  const rows: [string, string | null][] = result
    ? [
        [t("tools.speed.download"), formatMbps(result.download_bps)],
        [t("tools.speed.upload"), formatMbps(result.upload_bps)],
        [t("tools.speed.latency"), result.idle_latency_ms == null ? null : `${Math.round(result.idle_latency_ms)} ms`],
        [t("tools.speed.dataUsed"), result.bytes_used == null ? null : formatBytes(result.bytes_used, 1000)],
        [t("tools.speed.connection"), result.interface],
        [t("tools.speed.server"), result.server],
      ]
    : []

  return (
    <section aria-label={t("tools.speed.title")} className="space-y-2 rounded-lg border bg-muted/30 p-2.5">
      <div className="flex items-center gap-1.5 text-sm font-semibold">
        <Gauge aria-hidden="true" className="size-4" />
        {t("tools.speed.title")}
      </div>
      <p className="text-xs leading-relaxed text-muted-foreground">{t("tools.speed.intro")}</p>
      <p className="text-xs leading-relaxed text-muted-foreground">{t("tools.speed.dataNote")}</p>

      {!mac && <p className="rounded-md bg-background/60 px-2 py-1.5 text-xs">{t("tools.speed.error.unsupported")}</p>}

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

      <p aria-atomic="true" aria-live="polite" role="status" className="text-xs text-muted-foreground">
        {running
          ? t("tools.speed.running")
          : status === "done" && finishedAt != null
            ? t("tools.speed.measuredAt").replace("{time}", time(finishedAt))
            : ""}
      </p>

      {error && (
        <p role="alert" className="rounded-md border border-destructive/40 bg-destructive/10 px-2 py-1.5 text-xs text-destructive">
          {t(error)}
        </p>
      )}

      {rows.length > 0 && (
        <div className={`rounded-md bg-background/60 px-2 py-1.5 ${running ? "opacity-60" : ""}`}>
          {status === "error" && <p className="mb-1 text-xs text-muted-foreground">{t("tools.speed.previous")}</p>}
          {rows.map(([label, value]) => (
            <div key={label} className="flex items-baseline justify-between gap-2 py-0.5 text-xs">
              <span className="text-muted-foreground">{label}</span>
              <span className="min-w-0 break-all text-right font-medium tabular-nums select-text">{value ?? t("tools.speed.unknown")}</span>
            </div>
          ))}
        </div>
      )}
    </section>
  )
}
