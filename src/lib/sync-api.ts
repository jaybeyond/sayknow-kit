/**
 * Typed webview side of the `sync_*` Tauri commands (G9). The shapes mirror
 * `src-tauri/src/sync/commands.rs` (`SyncStatus`, `JoinSummaryOut`,
 * `CreatedVault`, `ReissueResult`, ...), `engine.rs` (`EngineNotice`,
 * `CycleStatus`) and `telemetry.rs` (`StatsSnapshot`); Rust is authoritative.
 *
 * Nothing here holds a secret: the recovery key (RK) passes through as a call
 * argument or a one-shot result and is owned by the dialog that showed it
 * (D9b). DEK, KEK and provider tokens never reach the webview.
 *
 * Tauri maps the camelCase argument names to the snake_case Rust parameters.
 */
import { invoke } from "@tauri-apps/api/core"
import { listen } from "@tauri-apps/api/event"
import { useCallback, useEffect, useRef, useState } from "react"
import { isTauri } from "./runtime"

// ---------------------------------------------------------------------------
// Shapes

export type ProviderKind = "gdrive" | "onedrive" | "webdav" | "r2"

export const PROVIDER_KINDS: readonly ProviderKind[] = ["gdrive", "onedrive", "webdav", "r2"]

export type ConnectedInfo = { provider: ProviderKind; account_label: string }

export type FailureClass =
  | "auth"
  | "rate"
  | "quota"
  | "transient"
  | "permanent"
  | "binding"
  | "unsupported"
  | "local"

export type StopReason =
  | "not_joined"
  | "missing_vault_key"
  | "identity_lost"
  | "wipe_in_progress"
  | "manifest_missing"
  | "device_removed"
  | "reconsent"
  | "permission_denied"
  | "binding_mismatch"
  | "unsupported"
  | "configuration"
  | "record_unreadable"
  | "vault_switched"

/** serde: `{ "secs": n, "nanos": n }`. */
export type RustDuration = { secs: number; nanos: number }

export type CycleStatus =
  | { status: "ok" }
  | { status: "stopped"; reason: StopReason }
  | { status: "rate_limited"; retry_after: RustDuration | null }
  | { status: "failed"; class: FailureClass }

/** `#[serde(tag = "notice", rename_all = "snake_case")]` on `EngineNotice`. */
export type EngineNotice =
  | { notice: "not_joined" }
  | { notice: "missing_vault_key" }
  | { notice: "identity_lost" }
  | { notice: "keychain_unavailable" }
  | { notice: "wipe_in_progress" }
  | { notice: "manifest_missing" }
  | { notice: "other_vaults"; count: number }
  | { notice: "manifest_copies"; count: number }
  | { notice: "device_removed" }
  | { notice: "reconsent_required" }
  | { notice: "permission_denied" }
  | { notice: "rate_limited" }
  | { notice: "quota_exceeded" }
  | { notice: "binding_mismatch" }
  | { notice: "unsupported" }
  | { notice: "configuration_error" }
  | { notice: "record_unreadable" }
  | { notice: "rotated"; triggers: string[] }
  | { notice: "upload_rejected" }
  | { notice: "blob_rejected" }
  | { notice: "blob_evicted" }
  | { notice: "fork_detected" }
  | { notice: "reprojected" }
  | { notice: "upload_outcome_unknown" }
  | { notice: "resend_cap_reached" }
  | { notice: "segment_too_large" }
  | { notice: "snapshot_too_large" }
  | { notice: "snapshot_verify_failed" }
  | { notice: "snapshot_conflict" }
  | { notice: "quarantined" }
  | { notice: "stream_held" }
  | { notice: "stream_gap" }
  | { notice: "update_required" }

export type NoticeTag = EngineNotice["notice"]

/**
 * Every `EngineNotice` tag, for tests and exhaustiveness checks. `satisfies`
 * fails to compile when the union and this list drift apart.
 */
export const NOTICE_TAGS = [
  "not_joined", "missing_vault_key", "identity_lost", "keychain_unavailable", "wipe_in_progress",
  "manifest_missing", "other_vaults", "manifest_copies", "device_removed", "reconsent_required",
  "permission_denied", "rate_limited", "quota_exceeded", "binding_mismatch", "unsupported",
  "configuration_error", "record_unreadable", "rotated", "upload_rejected", "blob_rejected",
  "blob_evicted", "fork_detected", "reprojected", "upload_outcome_unknown", "resend_cap_reached",
  "segment_too_large", "snapshot_too_large", "snapshot_verify_failed", "snapshot_conflict",
  "quarantined", "stream_held", "stream_gap", "update_required",
] as const satisfies readonly NoticeTag[]

/** Content-free device record from the vault's verified device list (U4). */
export type DeviceSummary = {
  /** Full device id; the argument of `sync_remove_device`. */
  id: string
  /** Short tag for display. */
  short: string
  /** Unix ms, or null when the device never published. */
  last_seen: number | null
  is_self: boolean
  removed: boolean
}

/** Kinds Rust reports in `restored`: `ops::Kind` serde names. */
export type RestoredKind = "memo" | "conv" | "tr" | "policy"

/**
 * Items kept after a delete conflict (U5): counts per kind from the latest
 * projection, stable until `sync_dismiss_notice("restored")`.
 */
export type RestoredSummary = { kind: RestoredKind | (string & {}); count: number }

export type FailureCounts = {
  auth: number
  rate: number
  quota: number
  transient: number
  permanent: number
  binding: number
  unsupported: number
  local: number
}

export type StatsSnapshot = {
  uploads: number
  upload_bytes: number
  downloads: number
  download_bytes: number
  failures: FailureCounts
  quarantined: number
  rotations: number
  rejected: number
  forked: number
  reprojections: number
  reproject_ms_max: number
  ctx_size_max: number
  obs_size_max: number
  cycles: number
  last_cycle_ms: number
  last_error: FailureClass | null
}

export type JoinSummaryOut = {
  skip: number
  republish: number
  copy: number
  evicted: number
  tombstones: number
}

export type DiscoveryState = "none" | "nothing_found" | "found" | "retry" | "unknown"
export type WipeState = "none" | "started" | "done" | "unknown"
export type MigrationState =
  | "none"
  | "backed_up"
  | "importing"
  | "imported"
  | "verified"
  | "native"
  | "unknown"

export type SyncStatus = {
  enabled: boolean
  provider: ConnectedInfo | null
  /** Short vault tag (8 hex chars), never the vault id. */
  vault: string | null
  last_status: CycleStatus | null
  /** Unix ms. */
  last_cycle_at: number | null
  running: boolean
  stats: StatsSnapshot
  notices: EngineNotice[]
  discovery: DiscoveryState
  /** Unix ms; set while `discovery` is `retry`. */
  discovery_retry_at: number | null
  migration: MigrationState
  migration_state: MigrationState
  queue_flushed_qseq: number | null
  ack_qseq: number | null
  chat_src_fallbacks: number
  wipe: WipeState
  reissue_pending: boolean
  create_pending: boolean
  join_preview: JoinSummaryOut | null
  finalize_this_boot: boolean
  needs_vault: boolean
  store_error: boolean
  devices: DeviceSummary[]
  restored: RestoredSummary[]
}

export type DiscoverySummary = { state: "none" | "found" | "retry"; vaults: number }

/** Shown once; the caller owns and clears it. */
export type CreatedVault = { vault: string; recovery_key: string }

export type ConfirmResult =
  | { result: "created"; vault: string }
  | { result: "reissued"; not_revocation: boolean }

export type ReissueResult =
  | { result: "show_key"; recovery_key: string }
  | { result: "published"; not_revocation: boolean }

export type JoinPreviewOut = { vault: string; summary: JoinSummaryOut }

export type JoinCommitOut =
  | { result: "joined"; vault: string; ops: number }
  | { result: "preview_changed"; summary: JoinSummaryOut }

export type RejoinOut = { vault: string }

export type WebDavParams = { base_url: string; username: string; password: string }
export type R2Params = { account_id: string; bucket: string; access_key_id: string; secret_access_key: string }
export type ConnectParams = WebDavParams | R2Params

/** Rust `CommandError`: a fixed code and a message free of secrets. */
export type CommandError = { code: string; message: string }

/** The exact word Rust requires for a cloud wipe (`vault::WIPE_CONFIRMATION`). */
export const WIPE_CONFIRMATION = "삭제"

/** The code of a rejected `sync_*` call, or `error` for anything else. */
export function commandErrorCode(e: unknown): string {
  if (e && typeof e === "object" && "code" in e && typeof (e as { code: unknown }).code === "string") {
    return (e as { code: string }).code
  }
  return "error"
}

// ---------------------------------------------------------------------------
// Commands

export const syncApi = {
  getStatus: () => invoke<SyncStatus>("sync_get_status"),
  setEnabled: (enabled: boolean) => invoke<SyncStatus>("sync_set_enabled", { enabled }),
  connectStart: (provider: ProviderKind, params?: ConnectParams) =>
    invoke<ConnectedInfo>("sync_connect_start", { provider, params: params ?? null }),
  connectCancel: () => invoke<void>("sync_connect_cancel"),
  disconnect: (wipeCloud: boolean, confirm: string) => invoke<void>("sync_disconnect", { wipeCloud, confirm }),
  discover: () => invoke<DiscoverySummary>("sync_discover"),
  createVault: () => invoke<CreatedVault>("sync_create_vault"),
  confirmRk: (rk: string) => invoke<ConfirmResult>("sync_confirm_rk", { rk }),
  joinPreview: (rk: string) => invoke<JoinPreviewOut>("sync_join_preview", { rk }),
  joinCommit: () => invoke<JoinCommitOut>("sync_join_commit"),
  rejoin: (rk: string) => invoke<RejoinOut>("sync_rejoin", { rk }),
  reissueRk: () => invoke<ReissueResult>("sync_reissue_rk"),
  removeDevice: (deviceId: string) => invoke<void>("sync_remove_device", { deviceId }),
  dismissNotice: (notice: NoticeTag | "restored") => invoke<boolean>("sync_dismiss_notice", { notice }),
  exportDiagnostics: () => invoke<string>("sync_export_diagnostics"),
}

// ---------------------------------------------------------------------------
// Status poll

export const STATUS_POLL_MS = 5000

/**
 * `sync_get_status` on mount, every `STATUS_POLL_MS` while the document is
 * visible, and on `sync:chat-changed` / `clipboard:changed`. Outside Tauri
 * the status stays `null`.
 */
export function useSyncStatus(): {
  status: SyncStatus | null
  /** Unix ms of the last successful fetch (0 before the first). */
  fetchedAt: number
  error: string | null
  refresh: () => Promise<SyncStatus | null>
} {
  const [snapshot, setSnapshot] = useState<{ status: SyncStatus | null; fetchedAt: number }>({
    status: null,
    fetchedAt: 0,
  })
  const [error, setError] = useState<string | null>(null)
  const alive = useRef(true)
  // O-4a: a poll issued before an action must not overwrite the refresh
  // issued after it. Each request takes a sequence number; only the newest
  // issued request may apply its response.
  const issued = useRef(0)
  const applied = useRef(0)

  const refresh = useCallback(async () => {
    if (!isTauri()) return null
    const seq = ++issued.current
    try {
      const next = await syncApi.getStatus()
      if (alive.current && seq > applied.current) {
        applied.current = seq
        setSnapshot({ status: next, fetchedAt: Date.now() })
        setError(null)
      }
      return next
    } catch (e) {
      if (alive.current && seq > applied.current) {
        applied.current = seq
        setError(commandErrorCode(e))
      }
      return null
    }
  }, [])

  useEffect(() => {
    alive.current = true
    if (!isTauri()) return
    // The first fetch is a subscription to the Rust side, not a render-time
    // state write: it resolves asynchronously like the poll and the events.
    const first = setTimeout(() => void refresh(), 0)
    const tick = () => {
      if (document.visibilityState === "visible") void refresh()
    }
    const timer = setInterval(tick, STATUS_POLL_MS)
    document.addEventListener("visibilitychange", tick)
    const unlisteners: Array<() => void> = []
    let cancelled = false
    for (const name of ["sync:chat-changed", "clipboard:changed"]) {
      listen(name, () => void refresh())
        .then((un) => {
          if (cancelled) un()
          else unlisteners.push(un)
        })
        .catch(() => {})
    }
    return () => {
      alive.current = false
      cancelled = true
      clearTimeout(first)
      clearInterval(timer)
      document.removeEventListener("visibilitychange", tick)
      for (const un of unlisteners) un()
    }
  }, [refresh])

  return { status: snapshot.status, fetchedAt: snapshot.fetchedAt, error, refresh }
}
