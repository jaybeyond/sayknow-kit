/**
 * Settings › Sync (G9). Status-driven: every state below renders from
 * `SyncStatus` plus `syncState()` from the migration client.
 *
 *   off → connect (no provider) → needs vault (discover; create / join) →
 *   status panel, notices, advanced (reissue RK, remove device), disconnect.
 *
 * Secrets (D9b): the recovery key exists only inside `RkDialog` state while
 * that dialog is open and is cleared on close and on unmount. Connection
 * passwords and keys live in `ConnectCards` state and are cleared after the
 * call. Nothing is logged, persisted, or kept after submit.
 *
 * Motion (emil-design-eng): settings navigation, toggles and the 5 s poll
 * never animate. Banners and notices enter once with a 180 ms opacity +
 * `scale(0.97)` ease-out (`.sync-enter`, reduced motion: fade only). Dialogs
 * use the shared `dialog-content` recipe. Pressables use `.sync-press`.
 */
import { useCallback, useEffect, useRef, useState, useSyncExternalStore } from "react"
import {
  Check,
  ChevronDown,
  ChevronRight,
  CloudSync,
  Copy,
  Download,
  KeyRound,
  LoaderCircle,
  TriangleAlert,
  X,
} from "lucide-react"
import { Button } from "@/components/ui/button"
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogFooter,
  DialogHeader,
  DialogTitle,
} from "@/components/ui/dialog"
import { Input } from "@/components/ui/input"
import { Separator } from "@/components/ui/separator"
import { Switch } from "@/components/ui/switch"
import { Row, SectionHeader } from "./settings-primitives"
import { useT, type UILocale, type UILocaleSetting } from "@/i18n"
import { cn } from "@/lib/utils"
import { subscribe, syncState } from "@/lib/sync-client"
import {
  commandErrorCode,
  PROVIDER_KINDS,
  syncApi,
  useSyncStatus,
  WIPE_CONFIRMATION,
  type ConnectParams,
  type DeviceSummary,
  type EngineNotice,
  type JoinSummaryOut,
  type NoticeTag,
  type ProviderKind,
  type RestoredSummary,
  type SyncStatus,
} from "@/lib/sync-api"

import {
  collapseNotices,
  cycleText,
  errorText,
  fill,
  noticeKey,
  noticeText,
  PROVIDER_LABELS,
  relativeTime,
  restoredText,
  type T,
} from "@/lib/sync-copy"

function useMigrationState() {
  return useSyncExternalStore(subscribe, syncState, syncState)
}

/* ─────────── Section ─────────── */

export function SyncSection({ uiLocale }: { uiLocale: UILocaleSetting }) {
  const { t, locale } = useT(uiLocale)
  const { status, fetchedAt, error: statusError, refresh } = useSyncStatus()
  const migration = useMigrationState()
  const [busy, setBusy] = useState<string | null>(null)
  const [error, setError] = useState<string | null>(null)
  const [rkDialog, setRkDialog] = useState<RkDialogMode | null>(null)
  const [joinOpen, setJoinOpen] = useState(false)
  const [rejoinOpen, setRejoinOpen] = useState(false)
  // U3: a keyed provider's reconsent opens its credential form, pre-selected.
  const [reconsent, setReconsent] = useState<"webdav" | "r2" | null>(null)

  const run = useCallback(
    async (name: string, fn: () => Promise<unknown>) => {
      setBusy(name)
      setError(null)
      try {
        await fn()
      } catch (e) {
        setError(commandErrorCode(e))
      } finally {
        setBusy(null)
        void refresh()
      }
    },
    [refresh],
  )

  if (!status) {
    return (
      <div className="space-y-6">
        <SectionHeader icon={CloudSync} title={t("settings.section.sync")} />
        <p className="text-[11px] text-muted-foreground">
          {statusError ? errorText(t, statusError) : t("settings.sync.loading")}
        </p>
      </div>
    )
  }

  const enabled = status.enabled
  const connected = enabled && status.provider !== null
  const needsVault = connected && (status.needs_vault || status.vault === null)
  const ready = connected && !needsVault

  return (
    <div className="space-y-6">
      <SectionHeader icon={CloudSync} title={t("settings.section.sync")} />

      <Row label={t("settings.sync.toggle")} description={t("settings.sync.intro")}>
        <Switch
          aria-label={t("settings.sync.toggle")}
          checked={enabled}
          disabled={busy !== null}
          onCheckedChange={(on) => void run("toggle", () => syncApi.setEnabled(on))}
        />
      </Row>

      {!enabled && (
        <div className="space-y-2 text-[11px] leading-relaxed text-muted-foreground">
          <p>
            <span className="font-medium text-foreground">{t("settings.sync.synced")}</span> {t("settings.sync.syncedList")}
          </p>
          <p>
            <span className="font-medium text-foreground">{t("settings.sync.neverSynced")}</span>{" "}
            {t("settings.sync.neverSyncedList")}
          </p>
        </div>
      )}

      {error && <ErrorBanner text={errorText(t, error)} />}
      {status.store_error && <ErrorBanner text={t("settings.sync.error.store_error")} />}

      {enabled && !connected && (
        <>
          <Separator />
          <ConnectCards t={t} busy={busy} run={run} />
        </>
      )}

      {connected && reconsent !== null && (
        <>
          <Separator />
          <ConnectCards
            t={t}
            busy={busy}
            run={run}
            reconsent={reconsent}
            onDone={() => setReconsent(null)}
          />
        </>
      )}

      {needsVault && (
        <>
          <Separator />
          <VaultSetup
            t={t}
            status={status}
            busy={busy}
            run={run}
            onCreate={() => setRkDialog({ kind: "create" })}
            onJoin={() => setJoinOpen(true)}
          />
        </>
      )}

      {ready && (
        <>
          <Separator />
          <StatusPanel t={t} locale={locale} status={status} now={fetchedAt} migration={migration} busy={busy} run={run} />
        </>
      )}

      {connected && (status.notices.length > 0 || status.restored.length > 0) && (
        <>
          <Separator />
          <Notices
            t={t}
            notices={status.notices}
            restored={status.restored}
            provider={status.provider?.provider ?? null}
            busy={busy}
            run={run}
            onJoinOther={() => setJoinOpen(true)}
            onRejoin={() => setRejoinOpen(true)}
            onReconsentKeyed={(kind) => setReconsent(kind)}
          />
        </>
      )}

      {ready && (
        <>
          <Separator />
          <Advanced t={t} locale={locale} status={status} now={fetchedAt} busy={busy} run={run} onReissue={() => setRkDialog({ kind: "reissue" })} />
        </>
      )}

      {connected && (
        <>
          <Separator />
          <Disconnect t={t} busy={busy} run={run} />
        </>
      )}

      {/* Dialogs mount only while open: closing unmounts them, so no RK or
          entered text can outlive the dialog (D9b). */}
      {rkDialog && (
        <RkDialog
          t={t}
          mode={rkDialog}
          onClose={() => {
            setRkDialog(null)
            void refresh()
          }}
        />
      )}
      {joinOpen && (
        <JoinDialog
          t={t}
          onClose={() => {
            setJoinOpen(false)
            void refresh()
          }}
        />
      )}
      {rejoinOpen && (
        <RejoinDialog
          t={t}
          onClose={() => {
            setRejoinOpen(false)
            void refresh()
          }}
        />
      )}
    </div>
  )
}

/* ─────────── Pieces ─────────── */

function ErrorBanner({ text }: { text: string }) {
  return (
    <div role="alert" className="sync-enter rounded-md border border-destructive/40 bg-destructive/10 px-3 py-2">
      <p className="text-[11px] font-medium text-destructive">{text}</p>
    </div>
  )
}

/**
 * A spinner next to the label while a call runs. The label stays, so the
 * button keeps its width and its accessible name.
 */
function Busy({ on, children }: { on: boolean; children: React.ReactNode }) {
  return (
    <>
      {on && <LoaderCircle className="h-3.5 w-3.5 animate-spin" aria-hidden />}
      {children}
    </>
  )
}

type Run = (name: string, fn: () => Promise<unknown>) => Promise<void>

/**
 * Provider cards and the keyed credential forms. With `reconsent` set (U3)
 * the card is fixed to the connected WebDAV or R2 account and the form asks
 * for every field again: nothing is pre-filled, the store never hands a
 * secret back to the webview.
 */
function ConnectCards({
  t,
  busy,
  run,
  reconsent = null,
  onDone,
}: {
  t: T
  busy: string | null
  run: Run
  reconsent?: "webdav" | "r2" | null
  onDone?: () => void
}) {
  const [picked, setPicked] = useState<ProviderKind>(reconsent ?? "gdrive")
  const [form, setForm] = useState<Record<string, string>>({})
  const oauth = picked === "gdrive" || picked === "onedrive"
  const connecting = busy === "connect"

  const field = (name: string, type: "text" | "password" = "text") => (
    <Input
      key={name}
      type={type}
      autoComplete="off"
      aria-label={t(`settings.sync.connect.field.${name}`)}
      placeholder={t(`settings.sync.connect.field.${name}`)}
      value={form[name] ?? ""}
      onChange={(e) => setForm((f) => ({ ...f, [name]: e.target.value }))}
      className="h-8 text-xs"
    />
  )

  const connect = () => {
    let params: ConnectParams | undefined
    if (picked === "webdav") {
      params = { base_url: form.base_url ?? "", username: form.username ?? "", password: form.password ?? "" }
    } else if (picked === "r2") {
      params = {
        account_id: form.account_id ?? "",
        bucket: form.bucket ?? "",
        access_key_id: form.access_key_id ?? "",
        secret_access_key: form.secret_access_key ?? "",
      }
    }
    // Secrets leave component state the moment the call is made.
    setForm({})
    void run("connect", async () => {
      await syncApi.connectStart(picked, params)
      onDone?.()
    })
  }

  return (
    <div className="space-y-3" data-testid={reconsent ? "reconsent-form" : undefined}>
      <div className="flex items-start justify-between gap-2">
        <div>
          <p className="text-sm font-medium">
            {reconsent ? t("settings.sync.connect.reconsentTitle") : t("settings.sync.connect.title")}
          </p>
          <p className="mt-0.5 text-[11px] leading-relaxed text-muted-foreground">
            {reconsent ? t("settings.sync.connect.reconsentDesc") : t("settings.sync.connect.desc")}
          </p>
        </div>
        {reconsent && (
          <Button
            variant="ghost"
            size="icon-xs"
            className="sync-press shrink-0"
            aria-label={t("common.cancel")}
            disabled={connecting}
            onClick={onDone}
          >
            <X className="h-3.5 w-3.5" />
          </Button>
        )}
      </div>
      {!reconsent && (
        <div role="radiogroup" aria-label={t("settings.sync.connect.title")} className="grid grid-cols-2 gap-2">
          {PROVIDER_KINDS.map((kind) => {
            const on = picked === kind
            return (
              <button
                key={kind}
                type="button"
                role="radio"
                aria-checked={on}
                disabled={connecting}
                onClick={() => {
                  setPicked(kind)
                  setForm({})
                }}
                className={cn(
                  "sync-press flex flex-col items-start rounded-md border px-3 py-2 text-left",
                  on ? "border-foreground/40 bg-muted/40" : "border-border",
                )}
              >
                <span className="text-xs font-medium">{PROVIDER_LABELS[kind]}</span>
                <span className="text-[11px] text-muted-foreground">
                  {kind === "gdrive" ? t("settings.sync.connect.recommended") : t(`settings.sync.connect.${kind}`)}
                </span>
              </button>
            )
          })}
        </div>
      )}
      {reconsent && <p className="text-xs font-medium">{PROVIDER_LABELS[reconsent]}</p>}
      {picked === "webdav" && (
        <div className="grid gap-2">{[field("base_url"), field("username"), field("password", "password")]}</div>
      )}
      {picked === "r2" && (
        <div className="grid gap-2">
          {[field("account_id"), field("bucket"), field("access_key_id"), field("secret_access_key", "password")]}
        </div>
      )}
      <div className="flex items-center gap-2">
        <Button size="sm" className="sync-press h-8 text-xs" disabled={busy !== null} onClick={connect}>
          <Busy on={connecting}>{oauth ? t("settings.sync.connect.signIn") : t("settings.sync.connect.connect")}</Busy>
        </Button>
        {connecting && oauth && (
          <>
            <span className="text-[11px] text-muted-foreground">{t("settings.sync.connect.waiting")}</span>
            <Button variant="ghost" size="sm" className="sync-press h-8 text-xs" onClick={() => void syncApi.connectCancel().catch(() => {})}>
              {t("common.cancel")}
            </Button>
          </>
        )}
      </div>
    </div>
  )
}

function VaultSetup({
  t,
  status,
  busy,
  run,
  onCreate,
  onJoin,
}: {
  t: T
  status: SyncStatus
  busy: string | null
  run: Run
  onCreate: () => void
  onJoin: () => void
}) {
  const [found, setFound] = useState<number | null>(null)
  const [now, setNow] = useState(() => Date.now())
  const discovered = useRef(false)

  const discover = useCallback(
    () =>
      run("discover", async () => {
        const out = await syncApi.discover()
        setFound(out.vaults)
      }),
    [run],
  )

  // Discover once on arrival; the status poll carries the recorded state.
  useEffect(() => {
    if (discovered.current || status.create_pending) return
    discovered.current = true
    void discover()
  }, [discover, status.create_pending])

  // A9: re-discover when `discovery_retry_at` arrives; a 1 s tick shows the
  // countdown until then.
  const retryAt = status.discovery === "retry" ? status.discovery_retry_at : null
  useEffect(() => {
    if (retryAt === null) return
    const wait = Math.max(0, retryAt - Date.now())
    const timer = setTimeout(() => void discover(), wait)
    const tick = setInterval(() => setNow(Date.now()), 1000)
    return () => {
      clearTimeout(timer)
      clearInterval(tick)
    }
  }, [retryAt, discover])

  const discovering = busy === "discover"
  const seconds = retryAt === null ? 0 : Math.max(0, Math.ceil((retryAt - now) / 1000))
  const state = status.discovery

  let line: string
  if (status.create_pending) line = t("settings.sync.vault.createPending")
  else if (discovering) line = t("settings.sync.vault.discovering")
  else if (state === "retry") line = fill(t("settings.sync.vault.retry"), { seconds })
  else if (state === "found")
    line = found === null ? t("settings.sync.vault.foundSome") : fill(t("settings.sync.vault.found"), { count: found })
  else if (state === "nothing_found") line = t("settings.sync.vault.none")
  else line = t("settings.sync.vault.discovering")

  return (
    <div className="space-y-3">
      <div>
        <p className="text-sm font-medium">{t("settings.sync.vault.title")}</p>
        <p className="mt-0.5 text-[11px] leading-relaxed text-muted-foreground">{line}</p>
      </div>
      <div className="flex flex-wrap items-center gap-2">
        {status.create_pending ? (
          <>
            <Button size="sm" className="sync-press h-8 text-xs" disabled={busy !== null} onClick={onCreate}>
              {t("settings.sync.vault.finish")}
            </Button>
            <Button variant="ghost" size="sm" className="sync-press h-8 text-xs" disabled={busy !== null} onClick={onCreate}>
              {t("settings.sync.vault.create")}
            </Button>
          </>
        ) : (
          <>
            {(state === "nothing_found" || state === "none" || state === "unknown") && (
              <Button size="sm" className="sync-press h-8 text-xs" disabled={busy !== null} onClick={onCreate}>
                {t("settings.sync.vault.create")}
              </Button>
            )}
            {state === "found" && (
              <Button size="sm" className="sync-press h-8 text-xs" disabled={busy !== null} onClick={onJoin}>
                {t("settings.sync.vault.join")}
              </Button>
            )}
            {state === "found" && (
              <Button variant="ghost" size="sm" className="sync-press h-8 text-xs" disabled={busy !== null} onClick={onCreate}>
                {t("settings.sync.vault.create")}
              </Button>
            )}
            <Button variant="ghost" size="sm" className="sync-press h-8 text-xs" disabled={busy !== null} onClick={() => void discover()}>
              <Busy on={discovering}>{t("settings.sync.vault.retryNow")}</Busy>
            </Button>
          </>
        )}
      </div>
    </div>
  )
}

function StatusPanel({
  t,
  locale,
  status,
  now,
  migration,
  busy,
  run,
}: {
  t: T
  locale: UILocale
  status: SyncStatus
  /** When `status` was fetched; the relative time is computed against it. */
  now: number
  migration: ReturnType<typeof syncState>
  busy: string | null
  run: Run
}) {
  const [exported, setExported] = useState(false)
  const provider = status.provider!
  const stats = status.stats

  const exportDiagnostics = () =>
    run("export", async () => {
      const text = await syncApi.exportDiagnostics()
      await navigator.clipboard.writeText(text)
      setExported(true)
      setTimeout(() => setExported(false), 2000)
    })

  const migrationLine =
    migration.finalize === "deferred" || (migration.error === "deferred" && status.migration !== "native")
      ? t("settings.sync.migration.deferred")
      : status.migration === "native"
        ? t("settings.sync.migration.native")
        : status.migration === "none"
          ? null
          : t("settings.sync.migration.moving")

  const localError =
    migration.error && migration.error !== "deferred"
      ? migration.error === "store_error"
        ? t("settings.sync.error.store_error")
        : migration.error === "queue_write_failed" || migration.writeFailures > 0
          ? fill(t("settings.sync.local.refused"), { count: Math.max(1, migration.writeFailures) })
          : migration.error === "marker_write_failed"
            ? t("settings.sync.local.marker")
            : t("settings.sync.local.generic")
      : migration.writeFailures > 0
        ? fill(t("settings.sync.local.refused"), { count: migration.writeFailures })
        : null

  const wipeLine =
    status.wipe === "started" ? t("settings.sync.wipe.started") : status.wipe === "done" ? t("settings.sync.wipe.done") : null

  return (
    <div className="space-y-3">
      <p className="text-sm font-medium">{t("settings.sync.status.title")}</p>
      <dl className="grid grid-cols-[auto_1fr] gap-x-4 gap-y-1 text-[11px]">
        <dt className="text-muted-foreground">{t("settings.sync.status.account")}</dt>
        <dd>
          {PROVIDER_LABELS[provider.provider]} · {provider.account_label}
        </dd>
        <dt className="text-muted-foreground">{t("settings.sync.status.vault")}</dt>
        <dd className="font-mono">{status.vault}</dd>
        <dt className="text-muted-foreground">{t("settings.sync.status.lastCycle")}</dt>
        <dd>
          {status.last_cycle_at === null ? t("settings.sync.cycle.never") : relativeTime(locale, status.last_cycle_at, now)}
          {" · "}
          {cycleText(t, status.last_status, status.running)}
        </dd>
        <dt className="text-muted-foreground">{t("settings.sync.status.counts")}</dt>
        <dd>
          {fill(t("settings.sync.status.countsLine"), {
            up: stats.uploads,
            down: stats.downloads,
            cycles: stats.cycles,
            quarantined: stats.quarantined,
          })}
        </dd>
      </dl>
      {migrationLine && <p className="text-[11px] text-muted-foreground">{migrationLine}</p>}
      {wipeLine && <p className="text-[11px] text-muted-foreground">{wipeLine}</p>}
      {localError && <ErrorBanner text={localError} />}
      <Button variant="ghost" size="sm" className="sync-press h-8 text-xs" disabled={busy !== null} onClick={exportDiagnostics}>
        <Busy on={busy === "export"}>
          {exported ? <Check className="h-3.5 w-3.5" /> : <Download className="h-3.5 w-3.5" />}
          {exported ? t("settings.sync.status.exported") : t("settings.sync.status.export")}
        </Busy>
      </Button>
    </div>
  )
}

function Notices({
  t,
  notices,
  restored,
  provider,
  busy,
  run,
  onJoinOther,
  onRejoin,
  onReconsentKeyed,
}: {
  t: T
  notices: EngineNotice[]
  /** U5: items kept after a delete conflict, until dismissed. */
  restored: RestoredSummary[]
  /** The connected provider: a reconsent signs in to the same one. */
  provider: ProviderKind | null
  busy: string | null
  run: Run
  onJoinOther: () => void
  onRejoin: () => void
  /** U3: a WebDAV or R2 reconsent opens the credential form instead of OAuth. */
  onReconsentKeyed: (kind: "webdav" | "r2") => void
}) {
  const dismiss = (tag: NoticeTag | "restored") => run(`dismiss:${tag}`, () => syncApi.dismissNotice(tag))
  const reconnect = () => {
    if (provider === null) return
    if (provider === "webdav" || provider === "r2") onReconsentKeyed(provider)
    else void run("reconnect", () => syncApi.connectStart(provider))
  }
  // O-4d: Rust keeps notices in a set by value, so one tag can appear twice
  // (`other_vaults` 1 and 2). One banner per tag, keyed by the whole notice.
  const shown = collapseNotices(notices)
  return (
    <div className="space-y-2">
      <p className="text-sm font-medium">{t("settings.sync.notices.title")}</p>
      <ul className="space-y-2">
        {restored.length > 0 && (
          <li data-notice="restored" className="sync-enter rounded-md border bg-muted/30 px-3 py-2">
            <p className="text-[11px] leading-relaxed">{restoredText(t, restored)}</p>
            <div className="mt-1.5 flex flex-wrap gap-1.5">
              <Button variant="ghost" size="xs" className="sync-press" disabled={busy !== null} onClick={() => void dismiss("restored")}>
                {t("settings.sync.notice.action.dismiss")}
              </Button>
            </div>
          </li>
        )}
        {shown.map((notice) => {
          const tag = notice.notice
          return (
            <li key={noticeKey(notice)} data-notice={tag} className="sync-enter rounded-md border bg-muted/30 px-3 py-2">
              <p className="text-[11px] leading-relaxed">{noticeText(t, notice)}</p>
              <div className="mt-1.5 flex flex-wrap gap-1.5">
                {tag === "other_vaults" && (
                  <Button size="xs" className="sync-press" disabled={busy !== null} onClick={onJoinOther}>
                    {t("settings.sync.notice.action.joinOther")}
                  </Button>
                )}
                {tag === "other_vaults" && (
                  <Button variant="outline" size="xs" className="sync-press" disabled={busy !== null} onClick={() => void dismiss(tag)}>
                    {t("settings.sync.notice.action.keepCurrent")}
                  </Button>
                )}
                {tag === "reconsent_required" && (
                  <Button size="xs" className="sync-press" disabled={busy !== null || provider === null} onClick={reconnect}>
                    {t("settings.sync.notice.action.reconnect")}
                  </Button>
                )}
                {tag === "device_removed" && (
                  <Button size="xs" className="sync-press" disabled={busy !== null} onClick={onRejoin}>
                    {t("settings.sync.notice.action.rejoin")}
                  </Button>
                )}
                <Button variant="ghost" size="xs" className="sync-press" disabled={busy !== null} onClick={() => void dismiss(tag)}>
                  {tag === "other_vaults" ? t("settings.sync.notice.action.later") : t("settings.sync.notice.action.dismiss")}
                </Button>
              </div>
            </li>
          )
        })}
      </ul>
    </div>
  )
}

function Advanced({
  t,
  locale,
  status,
  now,
  busy,
  run,
  onReissue,
}: {
  t: T
  locale: UILocale
  status: SyncStatus
  /** When `status` was fetched; `last_seen` is shown relative to it. */
  now: number
  busy: string | null
  run: Run
  onReissue: () => void
}) {
  const [open, setOpen] = useState(false)
  const [picked, setPicked] = useState<string | null>(null)
  const [confirmRemove, setConfirmRemove] = useState(false)
  const Chevron = open ? ChevronDown : ChevronRight
  // U4: the picker lists the vault's verified devices. The current device is
  // shown but cannot be picked (Rust refuses `self_removal` too); removed
  // devices are listed for context and cannot be picked again.
  const devices = status.devices
  const pickable = devices.filter((d) => !d.is_self && !d.removed)
  const pickedDevice = pickable.find((d) => d.id === picked) ?? null

  return (
    <div className="space-y-3">
      <button
        type="button"
        aria-expanded={open}
        onClick={() => setOpen((o) => !o)}
        className="flex items-center gap-1.5 text-sm font-medium text-muted-foreground hover:text-foreground"
      >
        <Chevron className="h-3.5 w-3.5" />
        {t("settings.sync.advanced.title")}
      </button>
      {open && (
        <div className="space-y-4">
          <p className="rounded-md border border-amber-500/40 bg-amber-500/10 px-3 py-2 text-[11px] leading-relaxed" data-testid="revocation-limit">
            <TriangleAlert className="mr-1 inline h-3 w-3 text-amber-600" aria-hidden />
            {t("settings.sync.advanced.revocationLimit")}
          </p>
          <Row label={t("settings.sync.advanced.reissue")} description={status.reissue_pending ? t("settings.sync.advanced.reissuePending") : t("settings.sync.advanced.reissueDesc")}>
            <Button variant="outline" size="sm" className="sync-press h-8 text-xs" disabled={busy !== null} onClick={onReissue}>
              <KeyRound className="h-3.5 w-3.5" />
              {status.reissue_pending ? t("settings.sync.advanced.reissueFinish") : t("settings.sync.advanced.reissue")}
            </Button>
          </Row>
          <div className="space-y-2">
            <div>
              <p className="text-xs font-medium">{t("settings.sync.advanced.removeDevice")}</p>
              <p className="mt-0.5 text-[11px] leading-relaxed text-muted-foreground">{t("settings.sync.advanced.removeDeviceDesc")}</p>
            </div>
            {devices.length === 0 ? (
              <p className="text-[11px] text-muted-foreground">{t("settings.sync.advanced.noDevices")}</p>
            ) : (
              <div role="radiogroup" aria-label={t("settings.sync.advanced.devices")} className="grid gap-1.5">
                {devices.map((d) => (
                  <DeviceOption
                    key={d.id}
                    t={t}
                    locale={locale}
                    device={d}
                    now={now}
                    checked={picked === d.id}
                    disabled={busy !== null || d.is_self || d.removed}
                    onPick={() => {
                      setPicked(d.id)
                      setConfirmRemove(false)
                    }}
                  />
                ))}
              </div>
            )}
            {confirmRemove && pickedDevice ? (
              <div className="space-y-1.5">
                <p className="text-[11px] leading-relaxed text-muted-foreground" data-testid="remove-limit">
                  {fill(t("settings.sync.advanced.removeLimit"), { device: pickedDevice.short })}
                </p>
                <div className="flex gap-1.5">
                  <Button
                    variant="destructive"
                    size="sm"
                    className="sync-press h-8 text-xs"
                    disabled={busy !== null}
                    onClick={() => {
                      const id = pickedDevice.id
                      setConfirmRemove(false)
                      setPicked(null)
                      void run("remove", () => syncApi.removeDevice(id))
                    }}
                  >
                    {t("settings.sync.advanced.removeConfirm")}
                  </Button>
                  <Button variant="ghost" size="sm" className="sync-press h-8 text-xs" onClick={() => setConfirmRemove(false)}>
                    {t("common.cancel")}
                  </Button>
                </div>
              </div>
            ) : (
              <Button
                variant="outline"
                size="sm"
                className="sync-press h-8 text-xs"
                disabled={busy !== null || pickedDevice === null}
                onClick={() => setConfirmRemove(true)}
              >
                {t("settings.sync.advanced.removeDevice")}
              </Button>
            )}
          </div>
        </div>
      )}
    </div>
  )
}

/** One row of the device picker: short tag, this-device / removed badge, last seen. */
function DeviceOption({
  t,
  locale,
  device,
  now,
  checked,
  disabled,
  onPick,
}: {
  t: T
  locale: UILocale
  device: DeviceSummary
  now: number
  checked: boolean
  disabled: boolean
  onPick: () => void
}) {
  const badge = device.is_self
    ? t("settings.sync.advanced.thisDevice")
    : device.removed
      ? t("settings.sync.advanced.removedDevice")
      : null
  const seen =
    device.last_seen === null ? t("settings.sync.advanced.neverSeen") : fill(t("settings.sync.advanced.lastSeen"), { when: relativeTime(locale, device.last_seen, now) })
  return (
    <button
      type="button"
      role="radio"
      aria-checked={checked}
      aria-disabled={disabled || undefined}
      disabled={disabled}
      data-device={device.id}
      data-self={device.is_self || undefined}
      onClick={onPick}
      className={cn(
        "sync-press flex items-center justify-between gap-3 rounded-md border px-3 py-1.5 text-left disabled:opacity-60",
        checked ? "border-foreground/40 bg-muted/40" : "border-border",
      )}
    >
      <span className="flex items-center gap-2">
        <span className="font-mono text-xs">{device.short}</span>
        {badge && <span className="rounded-sm bg-muted px-1 text-[10px] text-muted-foreground">{badge}</span>}
      </span>
      <span className="text-[11px] text-muted-foreground">{seen}</span>
    </button>
  )
}

function Disconnect({ t, busy, run }: { t: T; busy: string | null; run: Run }) {
  const [open, setOpen] = useState(false)
  const [wipe, setWipe] = useState(false)
  const [word, setWord] = useState("")
  const canConfirm = busy === null && (!wipe || word === WIPE_CONFIRMATION)

  const close = () => {
    setOpen(false)
    setWipe(false)
    setWord("")
  }

  return (
    <>
      <Row label={t("settings.sync.disconnect.title")} description={t("settings.sync.disconnect.desc")}>
        <Button
          variant="ghost"
          size="sm"
          className="sync-press h-8 text-xs text-destructive hover:bg-destructive/10 hover:text-destructive"
          disabled={busy !== null}
          onClick={() => setOpen(true)}
        >
          {t("settings.sync.disconnect.title")}
        </Button>
      </Row>
      <Dialog open={open} onOpenChange={(o) => { if (!o) close() }}>
        <DialogContent className="w-[360px] gap-3 p-4" showCloseButton={false}>
          <DialogHeader className="text-left">
            <DialogTitle className="text-sm">{t("settings.sync.disconnect.title")}</DialogTitle>
            <DialogDescription className="text-xs leading-relaxed">{t("settings.sync.disconnect.desc")}</DialogDescription>
          </DialogHeader>
          <label className="flex items-start gap-2 text-xs">
            <input
              type="checkbox"
              className="mt-0.5"
              checked={wipe}
              onChange={(e) => {
                setWipe(e.target.checked)
                setWord("")
              }}
            />
            <span>
              <span className="font-medium">{t("settings.sync.disconnect.wipe")}</span>
              <span className="block text-[11px] leading-relaxed text-muted-foreground">{t("settings.sync.disconnect.wipeDesc")}</span>
            </span>
          </label>
          {wipe && (
            <div className="space-y-1.5">
              <p className="text-[11px] text-muted-foreground">
                {fill(t("settings.sync.disconnect.typeWord"), { word: WIPE_CONFIRMATION })}
              </p>
              <Input
                aria-label={t("settings.sync.disconnect.wipeWord")}
                autoComplete="off"
                value={word}
                onChange={(e) => setWord(e.target.value)}
                className="h-8 text-xs"
              />
            </div>
          )}
          <DialogFooter className="flex-row justify-end gap-2">
            <Button variant="ghost" size="sm" className="sync-press" onClick={close}>
              {t("common.cancel")}
            </Button>
            <Button
              variant="destructive"
              size="sm"
              className="sync-press"
              disabled={!canConfirm}
              onClick={() => {
                const wipeCloud = wipe
                const confirm = wipe ? word : ""
                close()
                void run("disconnect", () => syncApi.disconnect(wipeCloud, confirm))
              }}
            >
              {wipe ? t("settings.sync.disconnect.confirmWipe") : t("settings.sync.disconnect.confirm")}
            </Button>
          </DialogFooter>
        </DialogContent>
      </Dialog>
    </>
  )
}

/* ─────────── RK dialogs (mounted only while open) ─────────── */

type RkDialogMode = { kind: "create" } | { kind: "reissue" }

type RkStep =
  | { step: "loading" }
  | { step: "show"; rk: string }
  | { step: "confirm"; rk: string }
  | { step: "done"; result: "created" | "reissued" }
  | { step: "error"; code: string }

/**
 * Create and reissue share this: Rust returns the RK once; the user copies
 * it, then re-enters it. The RK lives only in `step`; the parent unmounts the
 * dialog on close, which drops it (D9b).
 *
 * Copy (U1) writes the RK with `navigator.clipboard.writeText`. The app's own
 * clipboard poller never records a recovery key, whatever the copy path:
 * `clipboard.rs` `looks_sensitive` recognises a checksummed RK. Nothing here
 * logs or stores it.
 */
function RkDialog({ t, mode, onClose }: { t: T; mode: RkDialogMode; onClose: () => void }) {
  const [step, setStep] = useState<RkStep>({ step: "loading" })
  const [entry, setEntry] = useState("")
  const [mismatch, setMismatch] = useState(false)
  const [copied, setCopied] = useState(false)
  // RK-6b: one `sync_confirm_rk` at a time. Rust takes the pending vault
  // before its slow publish, so a second call fails `nothing_pending` while
  // the first still succeeds.
  const [confirming, setConfirming] = useState(false)
  const inFlight = useRef(false)
  const begun = useRef(false)
  // D-B1: liveness is tracked per mount, not per effect run. StrictMode runs
  // effect → cleanup → effect; a closure-local `cancelled` would drop the one
  // in-flight result (the second run returns early), leaving "loading" and a
  // revealed RK never shown. The ref is true again after the re-run.
  const mounted = useRef(false)

  useEffect(() => {
    mounted.current = true
    return () => {
      mounted.current = false
    }
  }, [])

  useEffect(() => {
    // One-shot per dialog: a locale change or a StrictMode re-run must not
    // call `sync_create_vault` or `sync_reissue_rk` again (A6).
    if (begun.current) return
    begun.current = true
    const begin = async () => {
      try {
        if (mode.kind === "create") {
          const out = await syncApi.createVault()
          if (mounted.current) setStep({ step: "show", rk: out.recovery_key })
        } else {
          const out = await syncApi.reissueRk()
          if (!mounted.current) return
          if (out.result === "show_key") setStep({ step: "show", rk: out.recovery_key })
          else setStep({ step: "done", result: "reissued" })
        }
      } catch (e) {
        if (mounted.current) setStep({ step: "error", code: commandErrorCode(e) })
      }
    }
    void begin()
  }, [mode])

  const confirm = async () => {
    if (step.step !== "confirm" || inFlight.current) return
    const typed = entry
    if (typed.trim() === "") return
    inFlight.current = true
    setConfirming(true)
    setEntry("")
    try {
      const out = await syncApi.confirmRk(typed)
      setMismatch(false)
      setStep({ step: "done", result: out.result })
    } catch (e) {
      const code = commandErrorCode(e)
      if (code === "confirmation_mismatch" || code === "rk_wrong" || code === "rk_format") setMismatch(true)
      else setStep({ step: "error", code })
    } finally {
      inFlight.current = false
      setConfirming(false)
    }
  }

  const title = mode.kind === "reissue" ? t("settings.sync.rk.reissueTitle") : t("settings.sync.rk.createTitle")
  const doneText = step.step === "done" ? t(step.result === "created" ? "settings.sync.rk.created" : "settings.sync.rk.reissued") : null

  return (
    <Dialog open onOpenChange={(o) => { if (!o) onClose() }}>
      <DialogContent className="w-[420px] gap-3 p-4" showCloseButton={false}>
        <DialogHeader className="text-left">
          <DialogTitle className="text-sm">{title}</DialogTitle>
          <DialogDescription className="text-xs leading-relaxed">
            {step.step === "confirm" ? t("settings.sync.rk.confirmDesc") : t("settings.sync.rk.showDesc")}
          </DialogDescription>
        </DialogHeader>

        {step.step === "loading" && (
          <p className="text-[11px] text-muted-foreground">{t("settings.sync.working")}</p>
        )}

        {step.step === "show" && (
          <div className="space-y-2">
            <output
              data-testid="rk-value"
              className="block select-all rounded-md border bg-muted/40 px-3 py-2 font-mono text-xs leading-relaxed tracking-wider break-all"
            >
              {step.rk}
            </output>
            <p className="text-[11px] leading-relaxed text-amber-700 dark:text-amber-400">
              <TriangleAlert className="mr-1 inline h-3 w-3" aria-hidden />
              {t("settings.sync.rk.offline")}
            </p>
            {mode.kind === "reissue" && (
              <p className="text-[11px] leading-relaxed text-muted-foreground" data-testid="reissue-limit">
                {t("settings.sync.advanced.revocationLimit")}
              </p>
            )}
            <Button
              variant="outline"
              size="sm"
              className="sync-press h-8 text-xs"
              onClick={() =>
                void navigator.clipboard.writeText(step.rk).then(() => {
                  setCopied(true)
                  setTimeout(() => setCopied(false), 2000)
                })
              }
            >
              {copied ? <Check className="h-3.5 w-3.5" /> : <Copy className="h-3.5 w-3.5" />}
              {copied ? t("settings.sync.rk.copied") : t("settings.sync.rk.copy")}
            </Button>
          </div>
        )}

        {step.step === "confirm" && (
          <div className="space-y-2">
            <Input
              aria-label={t("settings.sync.rk.entry")}
              placeholder={t("settings.sync.rk.entry")}
              autoComplete="off"
              spellCheck={false}
              disabled={confirming}
              value={entry}
              onChange={(e) => {
                setEntry(e.target.value)
                setMismatch(false)
              }}
              className="h-8 font-mono text-xs"
            />
            {mismatch && (
              <p role="alert" className="text-[11px] text-destructive">
                {t("settings.sync.rk.mismatch")}
              </p>
            )}
          </div>
        )}

        {doneText && <p className="text-xs">{doneText}</p>}
        {step.step === "error" && (
          <p role="alert" className="text-[11px] text-destructive">
            {errorText(t, step.code)}
          </p>
        )}

        <DialogFooter className="flex-row justify-end gap-2">
          {(step.step === "show" || step.step === "confirm") && (
            <Button variant="ghost" size="sm" className="sync-press" disabled={confirming} onClick={onClose}>
              {t("common.cancel")}
            </Button>
          )}
          {step.step === "show" && (
            <Button size="sm" className="sync-press" onClick={() => setStep({ step: "confirm", rk: step.rk })}>
              {t("settings.sync.rk.next")}
            </Button>
          )}
          {step.step === "confirm" && (
            <Button
              size="sm"
              className="sync-press"
              aria-busy={confirming || undefined}
              disabled={confirming || entry.trim() === ""}
              onClick={() => void confirm()}
            >
              <Busy on={confirming}>{t("settings.sync.rk.confirm")}</Busy>
            </Button>
          )}
          {(step.step === "done" || step.step === "error" || step.step === "loading") && (
            <Button size="sm" className="sync-press" onClick={onClose}>
              {t("common.close")}
            </Button>
          )}
        </DialogFooter>
      </DialogContent>
    </Dialog>
  )
}

function JoinSummary({ t, summary }: { t: T; summary: JoinSummaryOut }) {
  return (
    <ul className="space-y-0.5 text-[11px] leading-relaxed">
      <li>{fill(t("settings.sync.join.skip"), { count: summary.skip })}</li>
      <li>{fill(t("settings.sync.join.republish"), { count: summary.republish })}</li>
      <li>{fill(t("settings.sync.join.copy"), { count: summary.copy })}</li>
      <li>{fill(t("settings.sync.join.evicted"), { count: summary.evicted })}</li>
    </ul>
  )
}

type JoinStep =
  | { step: "entry" }
  | { step: "preview"; summary: JoinSummaryOut }
  | { step: "done"; ops: number }
  | { step: "error"; code: string }

/**
 * Join: RK → `sync_join_preview` → counts → confirm → `sync_join_commit`.
 *
 * U2: every entry point (vault setup and the two-vault banner) runs
 * `sync_discover` before the preview. Rust unlocks only among the vaults of
 * its last discovery, which is empty after a restart or in the ready state.
 * A `discovery_required` answer re-runs discovery once and then shows the
 * result as it is.
 */
function JoinDialog({ t, onClose }: { t: T; onClose: () => void }) {
  const [step, setStep] = useState<JoinStep>({ step: "entry" })
  const [rk, setRk] = useState("")
  const [busy, setBusy] = useState(false)
  const inFlight = useRef(false)

  const preview = async () => {
    if (inFlight.current) return
    const typed = rk
    if (typed.trim() === "") return
    inFlight.current = true
    setRk("")
    setBusy(true)
    try {
      await syncApi.discover()
      let out
      try {
        out = await syncApi.joinPreview(typed)
      } catch (e) {
        if (commandErrorCode(e) !== "discovery_required") throw e
        await syncApi.discover()
        out = await syncApi.joinPreview(typed)
      }
      setStep({ step: "preview", summary: out.summary })
    } catch (e) {
      setStep({ step: "error", code: commandErrorCode(e) })
    } finally {
      inFlight.current = false
      setBusy(false)
    }
  }

  const commit = async () => {
    if (inFlight.current) return
    inFlight.current = true
    setBusy(true)
    try {
      const out = await syncApi.joinCommit()
      if (out.result === "joined") setStep({ step: "done", ops: out.ops })
      else setStep({ step: "preview", summary: out.summary })
    } catch (e) {
      setStep({ step: "error", code: commandErrorCode(e) })
    } finally {
      inFlight.current = false
      setBusy(false)
    }
  }

  return (
    <Dialog open onOpenChange={(o) => { if (!o) onClose() }}>
      <DialogContent className="w-[420px] gap-3 p-4" showCloseButton={false}>
        <DialogHeader className="text-left">
          <DialogTitle className="text-sm">{t("settings.sync.join.title")}</DialogTitle>
          <DialogDescription className="text-xs leading-relaxed">
            {step.step === "preview" ? t("settings.sync.join.previewDesc") : t("settings.sync.join.desc")}
          </DialogDescription>
        </DialogHeader>

        {(step.step === "entry" || step.step === "error") && (
          <div className="space-y-2">
            <Input
              aria-label={t("settings.sync.rk.entry")}
              placeholder={t("settings.sync.rk.entry")}
              autoComplete="off"
              spellCheck={false}
              value={rk}
              onChange={(e) => {
                setRk(e.target.value)
                if (step.step === "error") setStep({ step: "entry" })
              }}
              className="h-8 font-mono text-xs"
            />
            {step.step === "error" && (
              <p role="alert" className="text-[11px] text-destructive">
                {step.code === "rk_wrong" ? t("settings.sync.join.wrongKey") : errorText(t, step.code)}
              </p>
            )}
          </div>
        )}

        {step.step === "preview" && <JoinSummary t={t} summary={step.summary} />}
        {step.step === "done" && <p className="text-xs">{fill(t("settings.sync.join.done"), { count: step.ops })}</p>}

        <DialogFooter className="flex-row justify-end gap-2">
          {step.step !== "done" && (
            <Button variant="ghost" size="sm" className="sync-press" onClick={onClose}>
              {t("common.cancel")}
            </Button>
          )}
          {(step.step === "entry" || step.step === "error") && (
            <Button size="sm" className="sync-press" disabled={busy || rk.trim() === ""} onClick={() => void preview()}>
              <Busy on={busy}>{t("settings.sync.join.preview")}</Busy>
            </Button>
          )}
          {step.step === "preview" && (
            <Button size="sm" className="sync-press" disabled={busy} onClick={() => void commit()}>
              <Busy on={busy}>{t("settings.sync.join.confirm")}</Busy>
            </Button>
          )}
          {step.step === "done" && (
            <Button size="sm" className="sync-press" onClick={onClose}>
              {t("common.close")}
            </Button>
          )}
        </DialogFooter>
      </DialogContent>
    </Dialog>
  )
}

/** `DeviceRemoved` → RK → `sync_rejoin` (D1). */
function RejoinDialog({ t, onClose }: { t: T; onClose: () => void }) {
  const [rk, setRk] = useState("")
  const [busy, setBusy] = useState(false)
  const [result, setResult] = useState<"done" | string | null>(null)
  const inFlight = useRef(false)

  const rejoin = async () => {
    if (inFlight.current) return
    const typed = rk
    if (typed.trim() === "") return
    inFlight.current = true
    setRk("")
    setBusy(true)
    try {
      await syncApi.rejoin(typed)
      setResult("done")
    } catch (e) {
      setResult(commandErrorCode(e))
    } finally {
      inFlight.current = false
      setBusy(false)
    }
  }

  return (
    <Dialog open onOpenChange={(o) => { if (!o) onClose() }}>
      <DialogContent className="w-[420px] gap-3 p-4" showCloseButton={false}>
        <DialogHeader className="text-left">
          <DialogTitle className="text-sm">{t("settings.sync.rejoin.title")}</DialogTitle>
          <DialogDescription className="text-xs leading-relaxed">{t("settings.sync.rejoin.desc")}</DialogDescription>
        </DialogHeader>
        {result === "done" ? (
          <p className="text-xs">{t("settings.sync.rejoin.done")}</p>
        ) : (
          <div className="space-y-2">
            <Input
              aria-label={t("settings.sync.rk.entry")}
              placeholder={t("settings.sync.rk.entry")}
              autoComplete="off"
              spellCheck={false}
              value={rk}
              onChange={(e) => {
                setRk(e.target.value)
                setResult(null)
              }}
              className="h-8 font-mono text-xs"
            />
            {result && result !== "done" && (
              <p role="alert" className="text-[11px] text-destructive">
                {result === "rk_wrong" ? t("settings.sync.join.wrongKey") : errorText(t, result)}
              </p>
            )}
          </div>
        )}
        <DialogFooter className="flex-row justify-end gap-2">
          {result === "done" ? (
            <Button size="sm" className="sync-press" onClick={onClose}>
              {t("common.close")}
            </Button>
          ) : (
            <>
              <Button variant="ghost" size="sm" className="sync-press" onClick={onClose}>
                {t("common.cancel")}
              </Button>
              <Button size="sm" className="sync-press" disabled={busy || rk.trim() === ""} onClick={() => void rejoin()}>
                <Busy on={busy}>{t("settings.sync.notice.action.rejoin")}</Busy>
              </Button>
            </>
          )}
        </DialogFooter>
      </DialogContent>
    </Dialog>
  )
}
