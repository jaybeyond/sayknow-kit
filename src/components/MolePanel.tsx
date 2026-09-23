import { useCallback, useEffect, useSyncExternalStore } from "react"
import { HardDrive, RefreshCw, Sparkles, Trash2 } from "lucide-react"
import { Button } from "@/components/ui/button"
import { formatBytes } from "@/lib/system-metrics-store"
import {
  detect,
  getSnapshot,
  run,
  subscribe,
  type SessionId,
  type SessionState,
} from "@/lib/mole-store"
import type {
  CleanPreviewItem,
  MoleAnalyze,
  MoleDiskEntry,
  MoleResult,
} from "@/lib/mole"

type Props = {
  t: (key: string) => string
  active: boolean
}

export function MolePanel({ t, active }: Props) {
  const { info, sessions, busy } = useSyncExternalStore(subscribe, getSnapshot)

  useEffect(() => {
    if (!active) return
    if (info === "loading") void detect()
  }, [active, info])

  const start = useCallback((id: SessionId, action: string) => {
    void run(id, action)
  }, [])

  if (info === "loading") {
    return <p className="p-2 text-[11px] text-muted-foreground">{t("tools.mole.detecting")}</p>
  }

  if (!info) {
    return (
      <section className="rounded-lg border bg-muted/30 p-2.5 text-[11px]">
        <div className="mb-1.5 font-medium">{t("tools.mole.title")}</div>
        <p className="text-muted-foreground">{t("tools.mole.missing")}</p>
        <pre className="mt-2 overflow-x-auto rounded-md bg-background/70 p-2 text-[10px]">
          brew install mole
        </pre>
      </section>
    )
  }

  // The backend only audited its noninteractive contract against one release,
  // so an unaudited Mole is reported here instead of failing on the first run.
  if (!info.supported) {
    return (
      <section className="rounded-lg border bg-muted/30 p-2.5 text-[11px]">
        <div className="mb-1.5 font-medium">{t("tools.mole.title")}</div>
        <p className="text-muted-foreground">
          {t("tools.mole.unsupported")
            .replace("{required}", info.required_version)
            .replace("{found}", info.version)}
        </p>
        <pre className="mt-2 overflow-x-auto rounded-md bg-background/70 p-2 text-[10px]">
          brew upgrade mole
        </pre>
      </section>
    )
  }

  return (
    <div className="space-y-2">
      <SessionCard
        busy={busy === "disk"}
        disabled={busy !== null}
        icon={HardDrive}
        t={t}
        title={t("tools.mole.session.disk")}
        hint={t("tools.mole.session.diskHint")}
        scanLabel={t("tools.mole.analyze")}
        onScan={() => start("disk", "analyze")}
        state={sessions.disk}
        kind="disk"
      />
      <SessionCard
        busy={busy === "cache"}
        disabled={busy !== null}
        icon={Trash2}
        t={t}
        title={t("tools.mole.session.cache")}
        hint={t("tools.mole.session.cacheHint")}
        scanLabel={t("tools.mole.cleanPreview")}
        runLabel={t("tools.mole.cleanNow")}
        onScan={() => start("cache", "clean-preview")}
        onRun={() => start("cache", "clean")}
        state={sessions.cache}
        kind="preview"
      />
      <SessionCard
        busy={busy === "tune"}
        disabled={busy !== null}
        icon={Sparkles}
        t={t}
        title={t("tools.mole.session.tune")}
        hint={t("tools.mole.session.tuneHint")}
        scanLabel={t("tools.mole.optimizePreview")}
        runLabel={t("tools.mole.optimizeNow")}
        onScan={() => start("tune", "optimize-preview")}
        onRun={() => start("tune", "optimize")}
        state={sessions.tune}
        kind="preview"
      />
    </div>
  )
}

function SessionCard({
  busy,
  disabled,
  hint,
  icon: Icon,
  kind,
  onRun,
  onScan,
  runLabel,
  scanLabel,
  state,
  t,
  title,
}: {
  busy: boolean
  disabled: boolean
  hint: string
  icon: typeof HardDrive
  kind: "disk" | "preview"
  onRun?: () => void
  onScan: () => void
  runLabel?: string
  scanLabel: string
  state: SessionState
  t: (k: string) => string
  title: string
}) {
  return (
    <section className="rounded-lg border bg-muted/30 p-2.5">
      <div className="mb-1 flex items-center gap-1.5 text-[11px] font-medium">
        <Icon className="h-3.5 w-3.5" />
        {title}
      </div>
      <p className="mb-2 text-[10px] leading-relaxed text-muted-foreground">{hint}</p>
      <div className="flex flex-wrap gap-1.5">
        <Button
          className="h-7 text-[11px] active:scale-[0.98]"
          disabled={disabled}
          onClick={onScan}
          size="sm"
          variant="secondary"
        >
          {busy ? <RefreshCw className="mr-1 h-3 w-3 animate-spin" /> : null}
          {scanLabel}
        </Button>
        {onRun && runLabel && (
          <Button
            className="h-7 text-[11px] active:scale-[0.98]"
            disabled={disabled}
            onClick={onRun}
            size="sm"
            variant="outline"
          >
            {runLabel}
          </Button>
        )}
      </div>
      {state.error && (
        <p className="mt-2 rounded-md border border-destructive/40 bg-destructive/10 px-2 py-1.5 text-[10px] text-destructive">
          {state.error === "mole_failed" ? t("tools.mole.failed") : state.error}
        </p>
      )}
      {busy && <ProgressBlock lines={state.progress} t={t} />}
      {state.result && <ResultBlock result={state.result} t={t} />}
      {kind === "disk" && state.analyze && <AnalyzeCard analyze={state.analyze} t={t} />}
      {kind === "preview" && state.items.length > 0 && <PreviewCard items={state.items} t={t} />}
    </section>
  )
}

function ProgressBlock({ lines, t }: { lines: string[]; t: (k: string) => string }) {
  const last = lines[lines.length - 1] ?? t("tools.mole.running")
  return (
    <div className="mt-2">
      <div className="mb-1 text-[10px] text-muted-foreground">{t("tools.mole.progress")}</div>
      <p className="truncate text-[10px] text-muted-foreground">{last}</p>
    </div>
  )
}

function ResultBlock({ result, t }: { result: MoleResult; t: (k: string) => string }) {
  return (
    <div className="mt-2 rounded-md bg-background/70 p-2">
      <div className="text-[11px] font-medium">{t("tools.mole.result")}</div>
      <p className="mt-0.5 text-[12px] font-semibold">{result.heading}</p>
      <div className="mt-1.5 grid grid-cols-2 gap-2">
        <div>
          <div className="text-[10px] text-muted-foreground">{t("tools.mole.freed")}</div>
          <div className="text-sm font-semibold tabular-nums">
            {result.freedBytes != null ? formatBytes(result.freedBytes, 1000) : "—"}
          </div>
        </div>
        <div>
          <div className="text-[10px] text-muted-foreground">{t("tools.mole.items")}</div>
          <div className="text-sm font-semibold tabular-nums">{result.items ?? "—"}</div>
        </div>
      </div>
    </div>
  )
}

function AnalyzeCard({ analyze, t }: { analyze: MoleAnalyze; t: (k: string) => string }) {
  const entries = [...(analyze.entries ?? [])].sort((a, b) => b.size - a.size).slice(0, 12)
  const max = Math.max(1, ...entries.map((e) => e.size), analyze.total_size ?? 0)
  return (
    <div className="mt-2 space-y-1">
      <div className="flex items-baseline justify-between gap-2 text-[10px] text-muted-foreground">
        <span>{t("tools.mole.disk")}</span>
        <span className="tabular-nums">
          {analyze.total_size != null ? formatBytes(analyze.total_size, 1000) : ""}
        </span>
      </div>
      {entries.map((entry) => (
        <DiskRow key={entry.path || entry.name} entry={entry} max={max} />
      ))}
    </div>
  )
}

function PreviewCard({ items, t }: { items: CleanPreviewItem[]; t: (k: string) => string }) {
  const sized = items.filter((item) => item.bytes && item.bytes > 0)
  const max = Math.max(1, ...sized.map((item) => item.bytes ?? 0))
  const total = sized.reduce((sum, item) => sum + (item.bytes ?? 0), 0)
  return (
    <div className="mt-2 space-y-1">
      <div className="flex items-baseline justify-between gap-2 text-[10px] text-muted-foreground">
        <span>{t("tools.mole.preview")}</span>
        <span className="tabular-nums">{total > 0 ? formatBytes(total, 1000) : `${items.length}`}</span>
      </div>
      {items.slice(0, 16).map((item) => (
        <div key={`${item.name}-${item.detail}-${item.bytes ?? 0}-${item.skipped}`}>
          <div className="flex items-baseline justify-between gap-2 text-[10px]">
            <span className="min-w-0 truncate">{item.name}</span>
            <span className="shrink-0 tabular-nums text-muted-foreground">
              {item.skipped
                ? t("tools.mole.skipped")
                : item.bytes
                  ? formatBytes(item.bytes, 1000)
                  : item.detail}
            </span>
          </div>
          {item.bytes != null && item.bytes > 0 && (
            <div className="mt-0.5 h-1.5 overflow-hidden rounded-full bg-background">
              <div
                className="h-full rounded-full bg-primary"
                style={{ width: `${Math.max(2, (item.bytes / max) * 100)}%` }}
              />
            </div>
          )}
        </div>
      ))}
    </div>
  )
}

function DiskRow({ entry, max }: { entry: MoleDiskEntry; max: number }) {
  const pct = Math.max(2, (entry.size / max) * 100)
  return (
    <div>
      <div className="flex items-baseline justify-between gap-2 text-[10px]">
        <span className="min-w-0 truncate">{entry.name}</span>
        <span className="shrink-0 tabular-nums text-muted-foreground">
          {formatBytes(entry.size, 1000)}
        </span>
      </div>
      <div className="mt-0.5 h-1.5 overflow-hidden rounded-full bg-background">
        <div className="h-full rounded-full bg-primary" style={{ width: `${pct}%` }} />
      </div>
    </div>
  )
}

