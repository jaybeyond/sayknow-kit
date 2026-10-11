/**
 * Localized copy helpers for the sync settings (G9). Pure functions over the
 * `t()` of the current locale; no React, so `SyncSection.tsx` stays a
 * components-only module and the notice switch is unit-testable on its own.
 */
import type { UILocale } from "@/i18n"
import type { CycleStatus, EngineNotice, ProviderKind, RestoredSummary } from "./sync-api"

export type T = (key: string) => string

export const PROVIDER_LABELS: Record<ProviderKind, string> = {
  gdrive: "Google Drive",
  onedrive: "OneDrive",
  webdav: "WebDAV",
  r2: "Cloudflare R2",
}

/** Error codes with their own copy; anything else reads `error.generic`. */
const KNOWN_ERRORS = new Set([
  "cancelled", "timeout", "busy", "oauth", "oauth_denied", "browser", "not_configured", "invalid_params",
  "disabled", "store", "store_error", "keychain", "not_connected", "not_joined", "already_connected",
  "activation", "reconsent", "permission_denied", "rate_limited", "quota", "configuration", "conflict",
  "unsupported", "binding_mismatch", "transient", "permanent", "rk_format", "rk_wrong", "manifest_invalid",
  "discovery_required", "ambiguous", "already_member", "join_incomplete", "preview_stale", "not_confirmed",
  "confirmation_mismatch", "self_removal", "invalid_id", "wipe_incomplete", "wipe_in_progress",
  "nothing_pending", "join_rejected", "no_key", "reissue_superseded",
])

export function errorText(t: T, code: string): string {
  return t(KNOWN_ERRORS.has(code) ? `settings.sync.error.${code}` : "settings.sync.error.generic")
}

export function fill(text: string, values: Record<string, string | number>): string {
  let out = text
  for (const [key, value] of Object.entries(values)) out = out.replaceAll(`{${key}}`, String(value))
  return out
}

/** Short relative time in the UI locale ("3 minutes ago"). */
export function relativeTime(locale: UILocale, at: number, now: number): string {
  const diff = Math.round((at - now) / 1000)
  const abs = Math.abs(diff)
  const rtf = new Intl.RelativeTimeFormat(locale, { numeric: "auto" })
  if (abs < 60) return rtf.format(diff, "second")
  if (abs < 3600) return rtf.format(Math.round(diff / 60), "minute")
  if (abs < 86_400) return rtf.format(Math.round(diff / 3600), "hour")
  return rtf.format(Math.round(diff / 86_400), "day")
}

export function cycleText(t: T, status: CycleStatus | null, running: boolean): string {
  if (!status) return t(running ? "settings.sync.cycle.running" : "settings.sync.cycle.never")
  switch (status.status) {
    case "ok":
      return t("settings.sync.cycle.ok")
    case "stopped":
      return t("settings.sync.cycle.stopped")
    case "rate_limited":
      return t("settings.sync.cycle.rateLimited")
    case "failed":
      return t("settings.sync.cycle.failed")
  }
}

/**
 * A stable key for a notice: the serialized value, so two notices with the
 * same tag but different fields (`other_vaults` 1 and 2) are distinct.
 */
export function noticeKey(notice: EngineNotice): string {
  return JSON.stringify(notice)
}

/** One notice per tag, keeping the last one Rust listed (duplicates collapse). */
export function collapseNotices(notices: EngineNotice[]): EngineNotice[] {
  const byTag = new Map<string, EngineNotice>()
  for (const n of notices) byTag.set(n.notice, n)
  return [...byTag.values()]
}

/**
 * One localized line per `EngineNotice` variant. The `never` default keeps
 * the switch exhaustive against `sync-api.ts`; at runtime a variant from a
 * newer Rust reads `notice.unknown` instead of its raw tag (§8).
 */
export function noticeText(t: T, notice: EngineNotice): string {
  switch (notice.notice) {
    case "not_joined":
    case "missing_vault_key":
    case "identity_lost":
    case "keychain_unavailable":
    case "wipe_in_progress":
    case "manifest_missing":
    case "device_removed":
    case "reconsent_required":
    case "permission_denied":
    case "rate_limited":
    case "quota_exceeded":
    case "binding_mismatch":
    case "unsupported":
    case "configuration_error":
    case "record_unreadable":
    case "rotated":
    case "upload_rejected":
    case "blob_rejected":
    case "blob_evicted":
    case "fork_detected":
    case "reprojected":
    case "upload_outcome_unknown":
    case "resend_cap_reached":
    case "segment_too_large":
    case "snapshot_too_large":
    case "snapshot_verify_failed":
    case "snapshot_conflict":
    case "quarantined":
    case "stream_held":
    case "stream_gap":
    case "update_required":
      return t(`settings.sync.notice.${notice.notice}`)
    case "other_vaults":
      return fill(t("settings.sync.notice.other_vaults"), { count: notice.count })
    case "manifest_copies":
      return fill(t("settings.sync.notice.manifest_copies"), { count: notice.count })
    default: {
      const unreachable: never = notice
      void unreachable
      return t("settings.sync.notice.unknown")
    }
  }
}

/** Localized label for a `restored[].kind`; anything unknown reads "items". */
export function restoredKindText(t: T, kind: string): string {
  switch (kind) {
    case "memo":
    case "conv":
    case "tr":
    case "policy":
      return t(`settings.sync.restored.kind.${kind}`)
    default:
      return t("settings.sync.restored.kind.other")
  }
}

/**
 * "N items that conflicted with a delete were kept" (U5): the total plus a
 * per-kind breakdown when more than one kind is present.
 */
export function restoredText(t: T, restored: RestoredSummary[]): string {
  const total = restored.reduce((sum, r) => sum + r.count, 0)
  const line = fill(t("settings.sync.restored.line"), { count: total })
  if (restored.length < 2) return line
  const parts = restored.map((r) => `${restoredKindText(t, r.kind)} ${r.count}`).join(" · ")
  return `${line} (${parts})`
}
