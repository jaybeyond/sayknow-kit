import { useEffect, useRef, useState, useSyncExternalStore, type ReactNode } from "react"
import { AlertTriangle, CheckCircle2, ChevronLeft, ChevronRight, Folder, HardDrive, Package, ShieldAlert, SlidersHorizontal, Trash2 } from "lucide-react"
import { Button } from "@/components/ui/button"
import { Dialog, DialogContent, DialogDescription, DialogFooter, DialogHeader, DialogTitle } from "@/components/ui/dialog"
import { Input } from "@/components/ui/input"
import { formatBytes } from "@/lib/system-metrics-store"
import {
  cancelRemoval, getSnapshot, initialize, openAppRemoval, removeSelected,
  run, subscribe, type SessionState,
} from "@/lib/mole-store"
import { MAINTENANCE_IDS, openFullDiskAccessSettings, type CleanedItem, type CleanPreviewItem, type MaintenanceTask, type MoleAnalyze, type MoleResult } from "@/lib/mole"
import { relaunchApp } from "@/lib/tools-store"

type T = (key: string) => string
type Props = { t: T; active: boolean }
/** The storage overview is the landing page; every other tool opens on its own page. */
type Page = "home" | "cache" | "tune" | "apps"
type ToolPage = Exclude<Page, "home">
const press = "transition-none active:scale-[0.97] motion-reduce:active:scale-100"
const listStyle = "rounded-lg border bg-background/40 divide-y"
const pathStyle = "break-all text-[11px] leading-relaxed text-muted-foreground select-text"
const time = (value: number) => new Date(value).toLocaleTimeString([], { hour: "2-digit", minute: "2-digit" })
function label(t: T, key: string, fallback: string) { const value = t(key); return value === key ? fallback : value }
function errorLabel(error: string, t: T) {
  const code = error.match(/\bmole_[a-z_]+\b/)?.[0]
  const message = code ? label(t, `tools.mole.error.${code}`, t("tools.mole.failed")) : t("tools.mole.failed")
  return code === "mole_app_running_unverified" ? message.replace("{pid}", error.match(/\bpid=(\d+)\b/)?.[1] ?? "?") : message
}
function ErrorNote({ error, t }: { error: string | null; t: T }) {
  if (!error) return null
  return <div role="alert" className="rounded-lg border border-destructive/30 bg-destructive/5 p-2.5 text-xs">
    <p className="text-destructive">{errorLabel(error, t)}</p>
    <details className="mt-1 text-muted-foreground"><summary className="cursor-pointer text-[11px]">{t("tools.mole.technical")}</summary><p className="mt-1 break-all select-text">{error}</p></details>
  </div>
}
function Section({ icon: Icon, title, hint, detailsLabel, note, count, children }: { icon: typeof HardDrive; title: string; hint: string; detailsLabel: string; note?: string; count?: number; children: ReactNode }) {
  return <section aria-label={title} className="space-y-2 border-t pt-3">
    <div className="flex items-center gap-2 text-[13px] font-semibold"><Icon aria-hidden="true" className="size-4 text-muted-foreground" /><h3>{title}</h3>{count !== undefined && <span className="rounded bg-muted px-1.5 py-0.5 text-[10px] font-normal tabular-nums text-muted-foreground">{count}</span>}</div>
    {children}
    <details className="text-xs leading-relaxed text-muted-foreground">
      <summary className="cursor-pointer">{detailsLabel}</summary>
      <p className="mt-1">{hint}</p>
      {note && <p className="mt-1">{note}</p>}
    </details>
  </section>
}
function ScanState({ state, busy, hideStatus, t }: { state: Pick<SessionState, "updatedAt" | "stale" | "error">; busy: boolean; hideStatus?: boolean; t: T }) {
  return <>
    {!hideStatus && (busy ? <p role="status" className="text-[11px] text-muted-foreground">{t("tools.mole.scanning")}</p>
      : state.updatedAt ? <p className="text-[11px] text-muted-foreground">{state.stale ? t("tools.mole.stale") : t("tools.mole.updated").replace("{time}", time(state.updatedAt))}</p>
        : !state.error && <p className="text-[11px] text-muted-foreground">{t("tools.mole.waiting")}</p>)}
    <ErrorNote error={state.error} t={t} />
  </>
}

/** Only reports observed tool output; a new heading is not proof the previous step succeeded. */
function RunProgress({ lines, busy, scanning, t }: { lines: string[]; busy: boolean; scanning: boolean; t: T }) {
  if (!busy && !lines.length) return null
  let stageIndex = -1
  for (let index = lines.length - 1; index >= 0; index--) {
    if (lines[index].startsWith("➤")) { stageIndex = index; break }
  }
  const stage = stageIndex < 0 ? "" : lines[stageIndex].slice(1).trim()
  const taskId = MAINTENANCE_IDS[stage]
  const stageTitle = taskId ? t(`tools.mole.task.${taskId}.title`) : label(t, `tools.mole.section.${stage}`, stage)
  const recent = lines.slice(stageIndex + 1).filter((line) => /^[✓◎→●○•*]\s/.test(line)).slice(-3)
  return <section aria-label={t("tools.mole.progress")} className="space-y-1.5 rounded-lg border border-primary/25 bg-primary/5 px-3 py-2.5">
    <div role="status" aria-live="polite" aria-atomic="true" className="flex flex-wrap items-center gap-x-2 gap-y-1 text-xs">
      <span className="text-muted-foreground">{busy ? t(scanning ? "tools.mole.scanning" : "tools.mole.running") : t("tools.mole.progress")}</span>
      {stage && <span className="font-medium">{stageTitle}</span>}
    </div>
    {recent.length > 0 && <ul className="space-y-1 text-xs">{recent.map((line, index) => <li key={`${index}:${line}`} className="break-words select-text">{line}</li>)}</ul>}
    {lines.length > 0 && <details className="text-[11px] text-muted-foreground">
      <summary className="cursor-pointer">{t("tools.mole.technical")}</summary>
      <pre className="mt-1 whitespace-pre-wrap break-words font-sans select-text">{lines.join("\n")}</pre>
    </details>}
  </section>
}
function Empty({ t }: { t: T }) { return <p className="rounded-lg border border-dashed p-3 text-xs text-muted-foreground">{t("tools.mole.empty")}</p> }

/** Rows Mole itself reported as removed; the preview estimate is never shown as freed space. */
function CleanResult({ session, t }: { session: SessionState; t: T }) {
  const result = session.result as MoleResult
  const freed = result.bytes !== null ? formatBytes(result.bytes, 1000) : t("tools.mole.unknownSize")
  const top = [...session.cleaned].sort((a, b) => (b.bytes ?? -1) - (a.bytes ?? -1))
  const row = (item: CleanedItem) => <li key={item.id} className="flex items-baseline justify-between gap-2">
    <span className="min-w-0 break-words">{label(t, `tools.mole.item.${item.name}`, item.name)}</span>
    <span className="shrink-0 tabular-nums text-muted-foreground">{item.bytes !== null ? formatBytes(item.bytes, 1000) : "—"}</span>
  </li>
  return <section aria-label={t("tools.mole.cleanResult")} className="space-y-2 rounded-lg border border-emerald-500/30 bg-emerald-500/5 px-3 py-2.5 text-xs">
    <div className="flex items-center gap-1.5 text-[11px] text-muted-foreground"><CheckCircle2 aria-hidden="true" className="size-3.5 text-emerald-600 dark:text-emerald-500" />{t("tools.mole.cleanResult")}{session.lastRunAt ? ` · ${time(session.lastRunAt)}` : ""}</div>
    <div>
      <div className="text-[11px] text-muted-foreground">{t("tools.mole.reported")}</div>
      <div className="mt-0.5 flex flex-wrap items-baseline gap-x-2 text-xl font-semibold tabular-nums">{freed}
        {(result.items ?? session.cleaned.length) > 0 && <span className="text-xs font-normal text-muted-foreground">{t("tools.mole.cleanedCount").replace("{count}", String(result.items ?? session.cleaned.length))}</span>}
      </div>
    </div>
    {top.length === 0 ? <p className="text-muted-foreground">{t("tools.mole.cleanedNone")}</p> : <>
      <ul className="space-y-1">{top.slice(0, 5).map(row)}</ul>
      {top.length > 5 && <details><summary className="cursor-pointer text-[11px] text-muted-foreground">{t("tools.mole.cleanedAll")} ({top.length})</summary><ul className="mt-1 space-y-1">{top.slice(5).map(row)}</ul></details>}
    </>}
    {session.runLog.length > 0 && <details className="text-[11px] text-muted-foreground">
      <summary className="cursor-pointer">{t("tools.mole.technical")}</summary>
      <pre className="mt-1 whitespace-pre-wrap break-words font-sans select-text">{session.runLog.join("\n")}</pre>
    </details>}
    <p className="text-[11px] leading-relaxed text-muted-foreground">{t("tools.mole.afterClean")}</p>
  </section>
}
/** Mole runs with this app's permissions, so without Full Disk Access the Trash looks empty and stays full. */
function FullDiskAccessNote({ t, run }: { t: T; run: (promise: Promise<void>) => void }) {
  return <div role="note" className="flex items-start gap-2 rounded-lg border border-amber-500/30 bg-amber-500/5 p-2.5 text-xs leading-relaxed">
    <ShieldAlert aria-hidden="true" className="mt-0.5 size-3.5 shrink-0" />
    <div className="min-w-0 space-y-2">
      <p>{t("tools.mole.fdaMissing")}</p>
      <div className="flex flex-wrap gap-1.5">
        <Button size="sm" variant="outline" className={`h-7 text-[11px] ${press}`} onClick={() => run(openFullDiskAccessSettings())}>{t("tools.mole.fdaOpen")}</Button>
        <Button size="sm" variant="ghost" className={`h-7 text-[11px] ${press}`} onClick={() => run(relaunchApp())}>{t("tools.mole.fdaRestart")}</Button>
      </div>
    </div>
  </div>
}

export function MolePanel({ t, active }: Props) {
  const state = useSyncExternalStore(subscribe, getSnapshot)
  const [query, setQuery] = useState("")
  const [selected, setSelected] = useState<string[]>([])
  const [confirmAction, setConfirmAction] = useState<{ action: "clean" | "optimize"; snapshot: SessionState } | null>(null)
  const [actionError, setActionError] = useState<string | null>(null)
  const [expiredToken, setExpiredToken] = useState<string | null>(null)
  const origin = useRef<HTMLButtonElement | null>(null)
  const cancel = useRef<HTMLButtonElement | null>(null)
  const search = useRef<HTMLInputElement | null>(null)
  const [page, setPage] = useState<Page>("home")
  const back = useRef<HTMLButtonElement | null>(null)
  const scroller = useRef<HTMLDivElement | null>(null)
  const openedFrom = useRef<ToolPage | null>(null)
  const pageButtons = useRef<Partial<Record<ToolPage, HTMLButtonElement | null>>>({})
  const { info, sessions, apps, busy, preview } = state
  const available = info !== "loading" && info?.supported === true && !state.detectionError
  const disabled = busy !== null || !available
  const expired = preview !== null && expiredToken === preview.token
  const acting = busy === "remove"
  const selectedRows = preview ? [...(preview.shortcut ? [preview.shortcut] : []), preview.app, ...preview.related.filter((row) => selected.includes(row.id))] : []
  const knownSizes = selectedRows.flatMap((row) => row.size_bytes !== null ? [row.size_bytes] : [])
  const selectedBytes = knownSizes.reduce((sum, bytes) => sum + bytes, 0)
  const hasUnknownSize = knownSizes.length !== selectedRows.length
  const filteredApps = (apps.inventory?.apps ?? []).filter((app) => `${app.name} ${app.path} ${app.resolved_path ?? ""}`.toLocaleLowerCase().includes(query.toLocaleLowerCase()))
  const execute = (promise: Promise<void>) => { setActionError(null); void promise.catch((error: unknown) => setActionError(String(error))) }
  const closeRemoval = () => execute(cancelRemoval())
  const restoreFocus = () => { if (origin.current?.isConnected && !origin.current.disabled) origin.current.focus(); else search.current?.focus() }
  useEffect(() => {
    if (active) void initialize().catch((error: unknown) => setActionError(String(error)))
  }, [active])
  useEffect(() => {
    if (!preview) return
    const timer = window.setTimeout(() => setExpiredToken(preview.token), Math.max(0, preview.expires_at_ms - Date.now()))
    return () => window.clearTimeout(timer)
  }, [preview])
  // Moving between pages moves focus with it: into the page on the way in,
  // back to the button that opened it on the way out. Nothing on first render.
  useEffect(() => {
    // A page starts at its top; the scroll offset of the previous page means nothing here.
    if (scroller.current) scroller.current.scrollTop = 0
    if (page !== "home") back.current?.focus()
    else if (openedFrom.current) pageButtons.current[openedFrom.current]?.focus()
  }, [page])
  const openPage = (next: ToolPage) => { openedFrom.current = next; setPage(next) }
  const tools: { page: ToolPage; icon: typeof HardDrive; title: string; count?: number; busy: boolean; summary: string | null }[] = [
    { page: "cache", icon: Trash2, title: t("tools.mole.session.cache"), busy: busy === "cache",
      count: sessions.cache.updatedAt !== null ? sessions.cache.items.length : undefined,
      summary: sessions.cache.scanResult?.bytes != null ? formatBytes(sessions.cache.scanResult.bytes, 1000) : null },
    { page: "tune", icon: SlidersHorizontal, title: t("tools.mole.session.tune"), busy: busy === "tune",
      count: sessions.tune.updatedAt !== null ? sessions.tune.maintenance.length : undefined, summary: null },
    { page: "apps", icon: Package, title: t("tools.mole.appsHeading"), busy: busy === "apps" || busy === "preview" || busy === "remove",
      count: apps.inventory?.apps.length, summary: null },
  ]
  const confirmationSession = confirmAction?.action === "clean" ? sessions.cache : sessions.tune
  const canConfirm = confirmAction && !disabled && !confirmationSession.stale && confirmationSession === confirmAction.snapshot
  const progressId = busy === "cache" || busy === "tune" ? busy : page === "cache" || page === "tune" ? page : null

  return <div className="flex min-h-0 flex-1 flex-col">
    {/* The page bar sits under the tool tabs, outside the scrolled page, so nothing scrolls between them.
        The tools header already refreshes these scans, so the bar carries no second refresh button. */}
    <header className="flex h-10 shrink-0 items-center justify-between gap-2 border-b bg-muted/30 px-2.5" data-mole-toolbar>
      {page === "home" ? <h2 className="truncate text-sm font-semibold">{t("tools.mole.title")}</h2> : <Button ref={back} size="sm" variant="ghost" className={`-ml-2 h-7 text-xs ${press}`} onClick={() => setPage("home")}>
        <ChevronLeft aria-hidden="true" className="size-3.5" />{t("tools.mole.back")}
      </Button>}
      {page === "cache" && <Button size="sm" variant="secondary" className={`h-7 text-xs ${press}`} disabled={disabled || sessions.cache.stale || !sessions.cache.updatedAt} onClick={(event) => { origin.current = event.currentTarget; setConfirmAction({ action: "clean", snapshot: sessions.cache }) }}>
        <Trash2 aria-hidden="true" className="size-3.5" />{t("tools.mole.cleanNow")}
      </Button>}
      {page === "tune" && <Button size="sm" variant="secondary" className={`h-7 text-xs ${press}`} disabled={disabled || sessions.tune.stale || !sessions.tune.updatedAt} onClick={(event) => { origin.current = event.currentTarget; setConfirmAction({ action: "optimize", snapshot: sessions.tune }) }}>
        <SlidersHorizontal aria-hidden="true" className="size-3.5" />{t("tools.mole.optimizeNow")}
      </Button>}
    </header>
    <div ref={scroller} className="min-h-0 flex-1 overflow-y-auto p-2.5" data-tools-scroll>
    <div className="space-y-3" data-testid="mole-panel">
    {info === "loading" && <p role="status" className="text-xs text-muted-foreground">{t("tools.mole.detecting")}</p>}
    {info !== "loading" && !info && <div className="rounded-lg border bg-muted/30 p-3 text-xs"><p>{t("tools.mole.missing")}</p><code className="mt-2 block select-text">brew install mole</code></div>}
    {info !== "loading" && info && !info.supported && <p className="rounded-lg border bg-muted/30 p-3 text-xs">{t("tools.mole.unsupported").replace("{required}", info.required_version).replace("{found}", info.version)}</p>}
    <ErrorNote error={state.detectionError ?? actionError} t={t} />
    {progressId && <RunProgress lines={sessions[progressId].progress} busy={busy === progressId} scanning={state.refreshing} t={t} />}

    {page === "home" && <>
    <nav aria-label={t("tools.mole.moreTools")} className="space-y-1.5">
      {tools.map((tool) => <button key={tool.page} ref={(node) => { pageButtons.current[tool.page] = node }} type="button" onClick={() => openPage(tool.page)}
        className={`flex w-full items-center gap-2 rounded-lg border bg-background/40 px-3 py-2 text-left hover:bg-muted/60 focus-visible:outline-2 focus-visible:outline-ring ${press}`}>
        <tool.icon aria-hidden="true" className="size-4 shrink-0 text-muted-foreground" />
        <span className="min-w-0 flex-1 text-xs font-medium">{tool.title}</span>
        {tool.busy ? <span className="text-[11px] text-muted-foreground">{t(state.refreshing ? "tools.mole.scanning" : "tools.mole.running")}</span>
          : tool.summary && <span className="shrink-0 text-xs tabular-nums text-muted-foreground">{tool.summary}</span>}
        {tool.count !== undefined && <span className="rounded bg-muted px-1.5 py-0.5 text-[10px] tabular-nums text-muted-foreground">{tool.count}</span>}
        <ChevronRight aria-hidden="true" className="size-3.5 shrink-0 text-muted-foreground" />
      </button>)}
    </nav>
    <Section icon={HardDrive} title={t("tools.mole.session.disk")} hint={t("tools.mole.session.diskHint")} detailsLabel={t("tools.mole.technical")} note={`${t("tools.mole.intro")} ${t("tools.mole.diskOverlap")}`} count={sessions.disk.analyze?.entries.length}>
      <ScanState state={sessions.disk} busy={busy === "disk"} t={t} />
      {sessions.disk.analyze ? <DiskList analyze={sessions.disk.analyze} t={t} /> : <Empty t={t} />}
    </Section>
    </>}

    {page === "cache" && <Section icon={Trash2} title={t("tools.mole.session.cache")} hint={t("tools.mole.session.cacheHint")} detailsLabel={t("tools.mole.technical")} count={sessions.cache.updatedAt !== null ? sessions.cache.items.length : undefined}>
      <ScanState state={sessions.cache} busy={busy === "cache"} hideStatus={busy === "cache"} t={t} />
      {state.fullDiskAccess === false && <FullDiskAccessNote t={t} run={execute} />}
      {sessions.cache.result && <CleanResult session={sessions.cache} t={t} />}
      {sessions.cache.scanResult && <div className="rounded-lg bg-muted/40 px-3 py-2.5">
        <div className="text-[11px] text-muted-foreground">{t("tools.mole.expected")}</div>
        <div className="mt-0.5 text-xl font-semibold tabular-nums">{sessions.cache.scanResult.bytes !== null ? formatBytes(sessions.cache.scanResult.bytes, 1000) : t("tools.mole.unknownSize")}</div>
        {sessions.cache.scanResult.partial && <p className="mt-1 text-[11px] text-muted-foreground">{t("tools.mole.partialEstimate")}</p>}
      </div>}
      <CleanList items={sessions.cache.items} t={t} />
    </Section>}

    {page === "tune" && <Section icon={SlidersHorizontal} title={t("tools.mole.session.tune")} hint={t("tools.mole.session.tuneHint")} detailsLabel={t("tools.mole.technical")} note={t("tools.mole.adminNotice")} count={sessions.tune.updatedAt !== null ? sessions.tune.maintenance.length : undefined}>
      <ScanState state={sessions.tune} busy={busy === "tune"} hideStatus={busy === "tune"} t={t} />
      <MaintenanceList tasks={sessions.tune.maintenance} t={t} />
      {sessions.tune.maintenanceResult.length > 0 && <div className="space-y-2 text-xs">
        <p className="font-medium">{t("tools.mole.lastRun")}{sessions.tune.lastRunAt ? ` · ${time(sessions.tune.lastRunAt)}` : ""}</p>
        <MaintenanceList tasks={sessions.tune.maintenanceResult} t={t} />
      </div>}
    </Section>}

    {page === "apps" && <Section icon={Package} title={t("tools.mole.appsHeading")} hint={t("tools.mole.appsHint")} detailsLabel={t("tools.mole.technical")} count={apps.inventory?.apps.length}>
      <ScanState state={apps} busy={busy === "apps"} t={t} />
      <Input ref={search} aria-label={t("tools.mole.searchApps")} placeholder={t("tools.mole.searchApps")} value={query} onChange={(event) => setQuery(event.target.value)} className="h-8 text-xs" />
      {filteredApps.length ? <ul aria-label={t("tools.mole.appsHeading")} className={listStyle}>
        {filteredApps.map((app) => <li key={app.id} className="p-3">
          <div className="flex items-start justify-between gap-3">
            <div className="min-w-0"><p className="break-words text-xs font-medium">{app.name}</p><p className="mt-0.5 text-[11px] tabular-nums text-muted-foreground">{app.size_label || t("tools.mole.unknownSize")} · {app.source}</p></div>
            <Button size="sm" variant="outline" className={`h-7 text-[11px] ${press}`} aria-label={`${app.name} · ${t("tools.mole.reviewApp")} · ${app.path}`} disabled={disabled || apps.stale || !!app.blocked_reason} onClick={(event) => {
              origin.current = event.currentTarget
              setSelected([])
              execute(openAppRemoval(app))
            }}>{t("tools.mole.reviewApp")}</Button>
          </div>
          <p className={`mt-1 ${pathStyle}`}>{app.path}</p>
          {app.resolved_path && app.resolved_path !== app.path && <p className={`mt-1 ${pathStyle}`}><span className="font-medium">{t("tools.mole.resolvedPath")}: </span>{app.resolved_path}</p>}
          {app.blocked_reason && <p className="mt-1 text-[11px] text-muted-foreground">{errorLabel(app.blocked_reason, t)}</p>}
        </li>)}
      </ul> : query ? <p className="p-3 text-xs text-muted-foreground">{t("tools.mole.noApps")}</p> : <Empty t={t} />}
      <ErrorNote error={state.removalError} t={t} />
      {state.removalResult && <div role="status" className="rounded-lg border p-3 text-xs">
        <p className="font-medium">{state.removalResult.items.length > 0 && !state.removalResult.stopped_reason && state.removalResult.items.every((row) => row.status === "moved" && !row.error) ? t("tools.mole.removeDone") : t("tools.mole.removePartial")}</p>
        <ul className="mt-2 space-y-2">{state.removalResult.items.map((row) => <li key={row.candidate_id}>
          <div className="flex justify-between gap-2"><span>{t(`tools.mole.kind.${row.kind}`)}</span><span>{t(`tools.mole.status.${row.status}`)}</span></div>
          <p className={pathStyle}>{row.path}</p>{row.error && <p className="mt-0.5 text-destructive">{errorLabel(row.error, t)}</p>}
        </li>)}</ul>
        {state.removalResult.stopped_reason && <p className="mt-2 text-destructive">{errorLabel(state.removalResult.stopped_reason, t)}</p>}
      </div>}
    </Section>}

    </div>
    </div>

    <Dialog open={confirmAction !== null} onOpenChange={(open) => { if (!open) setConfirmAction(null) }}>
      <DialogContent animate={false} showCloseButton={false} className="w-[420px] max-h-[85vh] gap-3 overflow-y-auto p-4" onOpenAutoFocus={(event) => { event.preventDefault(); cancel.current?.focus() }} onCloseAutoFocus={(event) => { event.preventDefault(); restoreFocus() }}>
        <DialogHeader className="text-left"><DialogTitle className="text-sm leading-relaxed">{t(confirmAction?.action === "clean" ? "tools.mole.cleanConfirm" : "tools.mole.optimizeConfirm")}</DialogTitle><DialogDescription className="text-xs leading-relaxed">{t(confirmAction?.action === "clean" ? "tools.mole.cleanWarning" : "tools.mole.optimizeWarning")}</DialogDescription></DialogHeader>
        <p className="text-xs leading-relaxed text-muted-foreground">{t("tools.mole.previewWarning")}</p>
        {!canConfirm && <p className="text-xs text-destructive">{t("tools.mole.stale")}</p>}
        <DialogFooter className="flex-row flex-wrap gap-2">
          <Button ref={cancel} variant="ghost" size="sm" className={press} onClick={() => setConfirmAction(null)}>{t("tools.mole.cancel")}</Button>
          <Button variant="destructive" size="sm" className={`h-auto min-h-8 whitespace-normal text-xs ${press}`} disabled={!canConfirm} onClick={() => {
            if (!confirmAction || !canConfirm) return
            const action = confirmAction.action
            setConfirmAction(null)
            execute(run(action === "clean" ? "cache" : "tune", action))
          }}>{t("tools.mole.confirm")}</Button>
        </DialogFooter>
      </DialogContent>
    </Dialog>

    <Dialog open={state.selectedApp !== null} onOpenChange={(open) => { if (!open && !acting) closeRemoval() }}>
      <DialogContent animate={false} showCloseButton={false} className="w-[440px] max-h-[88vh] gap-3 overflow-y-auto p-4" onOpenAutoFocus={(event) => { event.preventDefault(); cancel.current?.focus() }} onCloseAutoFocus={(event) => { event.preventDefault(); restoreFocus() }} onEscapeKeyDown={(event) => { if (acting) event.preventDefault() }} onInteractOutside={(event) => { if (acting) event.preventDefault() }}>
        <DialogHeader className="text-left"><DialogTitle className="break-words text-sm leading-relaxed">{t("tools.mole.removeHeading").replace("{name}", state.selectedApp?.name ?? "")}</DialogTitle><DialogDescription className="text-xs leading-relaxed">{t("tools.mole.relatedHint")}</DialogDescription></DialogHeader>
        <ErrorNote error={state.previewError} t={t} />
        {busy === "preview" && <p role="status" className="text-xs text-muted-foreground">{t("tools.mole.scanning")}</p>}
        {preview && <>
          {preview.shortcut && <div className="rounded-lg border bg-muted/30 p-2.5">
            <p className="text-xs font-medium">{t("tools.mole.kind.shortcut")}</p>
            <p className={`mt-1 ${pathStyle}`}>{preview.shortcut.path}</p>
            <p className="mt-2 text-xs leading-relaxed text-muted-foreground">{t("tools.mole.shortcutNotice")}</p>
          </div>}
          <div className="rounded-lg border bg-muted/30 p-2.5"><p className="text-xs font-medium">{t("tools.mole.kind.app")}</p><p className={`mt-1 ${pathStyle}`}>{preview.app.path}</p></div>
          {preview.related.length > 0 ? <ul className="space-y-2">{preview.related.map((row) => <li key={row.id}>
            <label className="flex cursor-pointer items-start gap-2 rounded-lg border p-2.5 has-[:focus-visible]:ring-2 has-[:focus-visible]:ring-ring">
              <input type="checkbox" className="mt-0.5 size-4 shrink-0 accent-primary" disabled={acting || expired} checked={selected.includes(row.id)} onChange={(event) => setSelected((current) => event.target.checked ? [...current, row.id] : current.filter((id) => id !== row.id))} />
              <span className="min-w-0 flex-1"><span className="flex justify-between gap-2 text-xs"><span>{t(`tools.mole.kind.${row.kind}`)}</span><span className="shrink-0 tabular-nums text-muted-foreground">{row.size_bytes !== null ? formatBytes(row.size_bytes, 1000) : t("tools.mole.unknownSize")}</span></span><span className={`mt-1 block ${pathStyle}`}>{row.path}</span></span>
            </label>
          </li>)}</ul> : <p className="text-xs text-muted-foreground">{t("tools.mole.noneRelated")}</p>}
          {preview.excluded.length > 0 && <details className="text-xs text-muted-foreground"><summary className="cursor-pointer">{t("tools.mole.excluded")} ({preview.excluded.length})</summary><ul className="mt-2 space-y-2">{preview.excluded.map((row) => <li key={`${row.kind}:${row.path}`}><p className={pathStyle}>{row.path}</p><p>{errorLabel(row.reason, t)}</p></li>)}</ul></details>}
          <div className="flex items-start gap-2 rounded-lg border border-amber-500/30 bg-amber-500/5 p-2.5 text-xs leading-relaxed"><AlertTriangle aria-hidden="true" className="mt-0.5 size-3.5 shrink-0" /><p>{t("tools.mole.dataWarning")}</p></div>
          <p className="text-[11px] leading-relaxed text-muted-foreground">{t("tools.mole.scopeWarning")}</p>
          <p className="text-xs font-medium">{t("tools.mole.selectionCount").replace("{count}", String(selected.length))}</p>
          <div aria-live="polite" className="text-xs">
            <p>{t("tools.mole.selectedSize")}: <span className="tabular-nums">{knownSizes.length > 0 ? formatBytes(selectedBytes, 1000) : t("tools.mole.unknownSize")}</span></p>
            {hasUnknownSize && <p className="mt-1 text-[11px] text-muted-foreground">{t("tools.mole.includesUnknown")}</p>}
          </div>
          {expired && <p role="alert" className="text-xs text-destructive">{t("tools.mole.previewExpired")}</p>}
        </>}
        {(state.previewError || expired) && <Button variant="outline" size="sm" className={`text-xs ${press}`} disabled={busy !== null} onClick={() => {
          if (state.selectedApp) { setSelected([]); execute(openAppRemoval(state.selectedApp)) }
        }}>{t("tools.mole.retry")}</Button>}
        <DialogFooter className="flex-row flex-wrap gap-2">
          <Button ref={cancel} variant="ghost" size="sm" className={press} disabled={acting} onClick={closeRemoval}>{t("tools.mole.cancel")}</Button>
          <Button variant="destructive" size="sm" className={`h-auto min-h-8 min-w-0 max-w-full whitespace-normal text-xs ${press}`} disabled={busy !== null || !preview || expired} onClick={() => execute(removeSelected(selected))}>{acting ? t("tools.mole.removing") : t("tools.mole.removeSelected")}</Button>
        </DialogFooter>
      </DialogContent>
    </Dialog>
  </div>
}

function DiskList({ analyze, t }: { analyze: MoleAnalyze; t: T }) {
  const entries = [...analyze.entries].sort((a, b) => b.size - a.size)
  const max = Math.max(1, ...entries.map((entry) => entry.size))
  if (!entries.length) return <Empty t={t} />
  return <ul aria-label={t("tools.mole.session.disk")} className={listStyle}>{entries.map((entry) => <li key={`${entry.path}:${entry.name}`} className="px-3 py-2">
    <div className="flex items-center justify-between gap-2 text-xs"><span className="flex min-w-0 items-center gap-1.5 break-words"><Folder aria-hidden="true" className="size-3.5 shrink-0 text-muted-foreground" />{entry.name}</span><span className="shrink-0 tabular-nums text-muted-foreground">{formatBytes(entry.size, 1000)}</span></div>
    <p className={`mt-1 ${pathStyle}`}>{entry.path}</p>
    <div aria-hidden="true" className="mt-1.5 h-1 overflow-hidden rounded-full bg-muted"><div className="h-full rounded-full bg-primary/50" style={{ width: `${entry.size / max * 100}%` }} /></div>
  </li>)}</ul>
}
function CleanList({ items, t }: { items: CleanPreviewItem[]; t: T }) {
  if (!items.length) return <Empty t={t} />
  const groups = new Map<string, CleanPreviewItem[]>()
  for (const item of items) {
    const rows = groups.get(item.section)
    if (rows) rows.push(item)
    else groups.set(item.section, [item])
  }
  return <div role="region" aria-label={t("tools.mole.session.cache")} className={listStyle}>
    {[...groups].map(([section, rows]) => <div key={section} className="p-3">
      <h4 className="mb-2 text-[11px] font-medium text-muted-foreground">{label(t, `tools.mole.section.${section}`, section)}</h4>
      <ul className="space-y-2">{rows.map((item) => <li key={item.id}>
        <details className="group text-xs">
          <summary className="flex cursor-pointer items-start gap-2"><ChevronRight aria-hidden="true" className="mt-0.5 size-3 shrink-0 text-muted-foreground group-open:rotate-90" /><span className="min-w-0 flex-1 break-words">{label(t, `tools.mole.item.${item.name}`, item.name)}</span><span className="shrink-0 tabular-nums text-muted-foreground">{item.status === "skipped" ? t("tools.mole.status.skipped") : item.bytes !== null ? formatBytes(item.bytes, 1000) : "—"}</span></summary>
          <p className="mt-1 break-words text-[11px] leading-relaxed text-muted-foreground select-text">{item.detail}</p>
        </details>
        {item.status === "manual" && <p className="mt-0.5 text-[11px] text-muted-foreground">{t("tools.mole.manual")}</p>}
      </li>)}</ul>
    </div>)}
  </div>
}
function MaintenanceList({ tasks, t }: { tasks: MaintenanceTask[]; t: T }) {
  if (!tasks.length) return <Empty t={t} />
  return <ul aria-label={t("tools.mole.session.tune")} className={listStyle}>{tasks.map((task) => <li key={task.id} className="px-3 py-2">
    <details className="group text-xs">
      <summary className="flex cursor-pointer items-start gap-2"><ChevronRight aria-hidden="true" className="mt-0.5 size-3 shrink-0 text-muted-foreground group-open:rotate-90" /><span className="min-w-0 flex-1 font-medium">{label(t, `tools.mole.task.${task.id}.title`, task.name)}</span><span className="shrink-0 text-[11px] text-muted-foreground">{t(`tools.mole.status.${task.status}`)}</span></summary>
      {MAINTENANCE_IDS[task.name] && <p className="mt-1 text-[11px] leading-relaxed text-muted-foreground">{t(`tools.mole.task.${task.id}.hint`)}</p>}
      <ul className="mt-1 space-y-1 text-[11px] text-muted-foreground">{task.details.map((line, index) => <li key={index} className="break-words select-text">{line}</li>)}</ul>
    </details>
  </li>)}</ul>
}
