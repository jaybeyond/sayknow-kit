/** @vitest-environment jsdom */
/**
 * G9 red team: adversarial cases against the sync settings UI. The cases
 * labelled "G9 blocker: ..." reproduced pass-1 defects; they are fixed under
 * the U1-U6 decisions in `_sync/plan-amendments.md` and now run as regressions.
 */
import { StrictMode } from "react"
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest"
import { act, cleanup, fireEvent, render, screen, waitFor, within } from "@testing-library/react"
import sectionSrc from "./SyncSection.tsx?raw"
import apiSrc from "@/lib/sync-api.ts?raw"
import copySrc from "@/lib/sync-copy.ts?raw"

/** Source text of the G9 modules, for the static scans (no Node fs: tsc -b has no node types here). */
const SOURCES: Record<string, string> = {
  "components/SyncSection.tsx": sectionSrc,
  "lib/sync-api.ts": apiSrc,
  "lib/sync-copy.ts": copySrc,
}
const readSrc = (rel: string) => SOURCES[rel]

const mocks = vi.hoisted(() => ({
  invoke: vi.fn<(cmd: string, args?: Record<string, unknown>) => Promise<unknown>>(),
  listen: vi.fn<(name: string, cb: () => void) => Promise<() => void>>(),
  unlisten: vi.fn(),
  writeText: vi.fn<(text: string) => Promise<void>>(),
  client: {
    state: { mode: "native", migration: null, finalize: null, error: null, corruptQueue: null, writeFailures: 0 } as Record<
      string,
      unknown
    >,
    subs: new Set<() => void>(),
  },
}))

vi.mock("@tauri-apps/api/core", () => ({ invoke: mocks.invoke }))
vi.mock("@tauri-apps/api/event", () => ({ listen: mocks.listen }))
vi.mock("@/lib/runtime", () => ({ isTauri: () => true, openExternal: vi.fn() }))
vi.mock("@/lib/sync-client", () => ({
  subscribe: (fn: () => void) => {
    mocks.client.subs.add(fn)
    return () => mocks.client.subs.delete(fn)
  },
  syncState: () => mocks.client.state,
}))

import { SyncSection } from "./SyncSection"
import { UI_LOCALES, UI_STRINGS, type UILocale } from "@/i18n/strings"
import { errorText, noticeText, restoredText } from "@/lib/sync-copy"
import {
  STATUS_POLL_MS,
  WIPE_CONFIRMATION,
  type DeviceSummary,
  type EngineNotice,
  type RestoredSummary,
  type SyncStatus,
} from "@/lib/sync-api"

const en = UI_STRINGS.en
const t = (key: string) => en[key] ?? key
const tl = (locale: UILocale) => (key: string) => UI_STRINGS[locale][key] ?? key

const RK = "ABCD-EFGH-IJKL-MNOP-QRST-UVWX-YZ23-4567-ABCD-EFGH-IJKL-MNOP-QRST-UVWX"
const RK_GROUPS = RK.split("-")
const ZWSP = String.fromCharCode(0x200b)

function status(patch: Partial<SyncStatus> = {}): SyncStatus {
  return {
    enabled: false,
    provider: null,
    vault: null,
    last_status: null,
    last_cycle_at: null,
    running: false,
    stats: {
      uploads: 0, upload_bytes: 0, downloads: 0, download_bytes: 0,
      failures: { auth: 0, rate: 0, quota: 0, transient: 0, permanent: 0, binding: 0, unsupported: 0, local: 0 },
      quarantined: 0, rotations: 0, rejected: 0, forked: 0, reprojections: 0, reproject_ms_max: 0,
      ctx_size_max: 0, obs_size_max: 0, cycles: 0, last_cycle_ms: 0, last_error: null,
    },
    notices: [],
    discovery: "none",
    discovery_retry_at: null,
    migration: "none",
    migration_state: "none",
    queue_flushed_qseq: null,
    ack_qseq: null,
    chat_src_fallbacks: 0,
    wipe: "none",
    reissue_pending: false,
    create_pending: false,
    join_preview: null,
    finalize_this_boot: false,
    needs_vault: false,
    store_error: false,
    devices: [],
    restored: [],
    ...patch,
  }
}
const connected = (patch: Partial<SyncStatus> = {}) =>
  status({ enabled: true, provider: { provider: "gdrive", account_label: "gdrive 1a2b3c4d" }, ...patch })
const joined = (patch: Partial<SyncStatus> = {}) =>
  connected({ vault: "9f8e7d6c", last_status: { status: "ok" }, last_cycle_at: Date.now() - 150_000, ...patch })

const ALL_NOTICES: EngineNotice[] = [
  { notice: "not_joined" }, { notice: "missing_vault_key" }, { notice: "identity_lost" },
  { notice: "keychain_unavailable" }, { notice: "wipe_in_progress" }, { notice: "manifest_missing" },
  { notice: "other_vaults", count: 2 }, { notice: "manifest_copies", count: 3 }, { notice: "device_removed" },
  { notice: "reconsent_required" }, { notice: "permission_denied" }, { notice: "rate_limited" },
  { notice: "quota_exceeded" }, { notice: "binding_mismatch" }, { notice: "unsupported" },
  { notice: "configuration_error" }, { notice: "record_unreadable" }, { notice: "rotated", triggers: ["install_fp"] },
  { notice: "upload_rejected" }, { notice: "blob_rejected" }, { notice: "blob_evicted" }, { notice: "fork_detected" },
  { notice: "reprojected" }, { notice: "upload_outcome_unknown" }, { notice: "resend_cap_reached" },
  { notice: "segment_too_large" }, { notice: "snapshot_too_large" }, { notice: "snapshot_verify_failed" },
  { notice: "snapshot_conflict" }, { notice: "quarantined" }, { notice: "stream_held" }, { notice: "stream_gap" },
  { notice: "update_required" },
]

/** Mirrors `KNOWN_ERRORS` in sync-copy.ts. */
const KNOWN_ERROR_CODES = [
  "cancelled", "timeout", "busy", "oauth", "oauth_denied", "browser", "not_configured", "invalid_params",
  "disabled", "store", "store_error", "keychain", "not_connected", "not_joined", "already_connected",
  "activation", "reconsent", "permission_denied", "rate_limited", "quota", "configuration", "conflict",
  "unsupported", "binding_mismatch", "transient", "permanent", "rk_format", "rk_wrong", "manifest_invalid",
  "discovery_required", "ambiguous", "already_member", "join_incomplete", "preview_stale", "not_confirmed",
  "confirmation_mismatch", "self_removal", "invalid_id", "wipe_incomplete", "wipe_in_progress",
  "nothing_pending", "join_rejected", "no_key", "reissue_superseded",
]

/** Routes `invoke` by command; `sync_get_status` answers from `current` unless `statusOverride` is set. */
let current: SyncStatus
let statusOverride: (() => Promise<SyncStatus>) | null
const handlers: Record<string, (args?: Record<string, unknown>) => unknown> = {}
function route() {
  mocks.invoke.mockImplementation(async (cmd, args) => {
    if (cmd === "sync_get_status") return statusOverride ? statusOverride() : current
    const h = handlers[cmd]
    if (!h) throw new Error("unmocked " + cmd)
    return h(args)
  })
}
const calls = (cmd: string) => mocks.invoke.mock.calls.filter(([c]) => c === cmd)

function deferred<T>() {
  let resolve!: (v: T) => void
  let reject!: (e: unknown) => void
  const promise = new Promise<T>((res, rej) => {
    resolve = res
    reject = rej
  })
  return { promise, resolve, reject }
}

async function renderSection(locale: UILocale = "en") {
  const out = render(<SyncSection uiLocale={locale} />)
  const loading = UI_STRINGS[locale]["settings.sync.loading"]
  await waitFor(() => expect(screen.queryByText(loading)).toBeNull())
  return out
}

/** Fake-timer flush: runs due timers and settles promises inside act. */
async function tick(ms = 10) {
  await act(async () => {
    await vi.advanceTimersByTimeAsync(ms)
  })
}

function useFake() {
  vi.useFakeTimers({ toFake: ["setTimeout", "clearTimeout", "setInterval", "clearInterval", "Date"] })
}

async function openCreateToShow() {
  current = connected({ needs_vault: true, discovery: "nothing_found" })
  handlers.sync_discover = () => ({ state: "none", vaults: 0 })
  handlers.sync_create_vault = () => ({ vault: "9f8e7d6c", recovery_key: RK })
  const out = await renderSection()
  fireEvent.click(await screen.findByRole("button", { name: t("settings.sync.vault.create") }))
  await screen.findByTestId("rk-value")
  return out
}

/** Every visible string: no raw key ids, no unfilled `{placeholder}`. */
function expectNoRawKeys(label: string) {
  const text = document.body.textContent ?? ""
  expect(text.match(/\b(settings|common)\.[a-z][A-Za-z_]*(\.[A-Za-z_]+)*/)?.[0], label).toBeUndefined()
  expect(Object.keys(en).find((k) => k.includes(".") && text.includes(k)), label).toBeUndefined()
  expect(text.match(/\{[a-z_]+\}/)?.[0], label).toBeUndefined()
}

beforeEach(() => {
  for (const k of Object.keys(handlers)) delete handlers[k]
  current = status()
  statusOverride = null
  route()
  mocks.listen.mockImplementation(async () => mocks.unlisten)
  mocks.client.state = { mode: "native", migration: null, finalize: null, error: null, corruptQueue: null, writeFailures: 0 }
  Object.defineProperty(navigator, "clipboard", { value: { writeText: mocks.writeText }, configurable: true })
  Object.defineProperty(document, "visibilityState", { value: "visible", configurable: true })
  mocks.writeText.mockResolvedValue()
})

afterEach(() => {
  cleanup()
  vi.clearAllMocks()
  vi.useRealTimers()
})

/* ───────────────────────────── RK lifecycle ───────────────────────────── */

describe("RK lifecycle (D9b)", () => {
  it("RK-1 navigate away (unmount) while the RK is shown leaves no RK; a remount starts clean", async () => {
    const out = await openCreateToShow()
    expect(screen.getByTestId("rk-value").textContent).toBe(RK)
    out.unmount()
    expect(document.body.innerHTML).not.toContain(RK)
    expect(document.body.innerHTML).not.toContain(RK_GROUPS[0] + "-" + RK_GROUPS[1])
    // Remount: no dialog, no RK, no create call replayed from module state.
    const creates = calls("sync_create_vault").length
    await renderSection()
    expect(screen.queryByRole("dialog")).toBeNull()
    expect(screen.queryByTestId("rk-value")).toBeNull()
    expect(document.body.innerHTML).not.toContain(RK)
    expect(calls("sync_create_vault")).toHaveLength(creates)
  })

  it("RK-2 Escape closes the show step and drops the RK; reopening never flashes the old RK", async () => {
    await openCreateToShow()
    fireEvent.keyDown(screen.getByRole("dialog"), { key: "Escape" })
    await waitFor(() => expect(screen.queryByRole("dialog")).toBeNull())
    expect(document.body.innerHTML).not.toContain(RK)
    const second = deferred<unknown>()
    handlers.sync_create_vault = () => second.promise
    fireEvent.click(await screen.findByRole("button", { name: t("settings.sync.vault.create") }))
    await screen.findByRole("dialog")
    expect(screen.queryByTestId("rk-value")).toBeNull()
    expect(document.body.innerHTML).not.toContain(RK)
    expect(screen.getByText(t("settings.sync.working"))).toBeTruthy()
    second.resolve({ vault: "11112222", recovery_key: "ZZZZ-" + RK.slice(5) })
    await screen.findByTestId("rk-value")
  })

  it("RK-3 close during loading: a late sync_create_vault result never renders the RK", async () => {
    current = connected({ needs_vault: true, discovery: "nothing_found" })
    handlers.sync_discover = () => ({ state: "none", vaults: 0 })
    const late = deferred<unknown>()
    handlers.sync_create_vault = () => late.promise
    await renderSection()
    fireEvent.click(await screen.findByRole("button", { name: t("settings.sync.vault.create") }))
    fireEvent.click(await screen.findByRole("button", { name: t("common.close") }))
    await waitFor(() => expect(screen.queryByRole("dialog")).toBeNull())
    await act(async () => {
      late.resolve({ vault: "9f8e7d6c", recovery_key: RK })
      await late.promise
    })
    expect(document.body.innerHTML).not.toContain(RK)
    expect(screen.queryByRole("dialog")).toBeNull()
  })

  it("RK-4 unmount during the confirm step with a typed RK leaves nothing behind", async () => {
    const out = await openCreateToShow()
    fireEvent.click(screen.getByRole("button", { name: t("settings.sync.rk.next") }))
    fireEvent.change(screen.getByLabelText(t("settings.sync.rk.entry")), { target: { value: RK } })
    out.unmount()
    expect(document.body.innerHTML).not.toContain(RK)
  })

  it("RK-5 no console output, no web storage and no module-level mutable state touch the RK", async () => {
    const spies = (["log", "info", "warn", "error", "debug"] as const).map((m) => vi.spyOn(console, m))
    const setItem = vi.spyOn(Storage.prototype, "setItem")
    handlers.sync_confirm_rk = (args) => {
      if (args?.rk !== RK) throw { code: "confirmation_mismatch", message: "" }
      return { result: "created", vault: "9f8e7d6c" }
    }
    await openCreateToShow()
    fireEvent.click(screen.getByRole("button", { name: t("settings.sync.rk.next") }))
    fireEvent.change(screen.getByLabelText(t("settings.sync.rk.entry")), { target: { value: RK } })
    fireEvent.click(screen.getByRole("button", { name: t("settings.sync.rk.confirm") }))
    await screen.findByText(t("settings.sync.rk.created"))
    for (const s of spies) for (const args of s.mock.calls) expect(JSON.stringify(args)).not.toContain(RK_GROUPS[0] + "-")
    expect(setItem).not.toHaveBeenCalled()
    for (const file of ["components/SyncSection.tsx", "lib/sync-api.ts", "lib/sync-copy.ts"]) {
      const src = readSrc(file)
      expect(src, file).not.toMatch(/console\.|localStorage|sessionStorage|indexedDB/)
      expect(src, file).not.toMatch(/^(export )?(let|var) /m)
    }
    for (const s of spies) s.mockRestore()
    setItem.mockRestore()
  })

  it("RK-6a a rapid double click on Confirm sends one sync_confirm_rk", async () => {
    const first = deferred<unknown>()
    handlers.sync_confirm_rk = () => first.promise
    await openCreateToShow()
    fireEvent.click(screen.getByRole("button", { name: t("settings.sync.rk.next") }))
    fireEvent.change(screen.getByLabelText(t("settings.sync.rk.entry")), { target: { value: RK } })
    const confirm = screen.getByRole("button", { name: t("settings.sync.rk.confirm") })
    fireEvent.click(confirm)
    fireEvent.click(confirm)
    expect(calls("sync_confirm_rk")).toHaveLength(1)
    first.resolve({ result: "created", vault: "9f8e7d6c" })
    await screen.findByText(t("settings.sync.rk.created"))
  })

  it("G9 blocker: RK-6b Confirm has an in-flight guard; a re-typed second confirm cannot race the first (Rust answers nothing_pending)", async () => {
    // Rust `confirm_rk` takes `pending_create` before the slow publish, so a
    // concurrent second call sees no pending vault and fails `nothing_pending`
    // immediately; the dialog flips to the error step while the first call
    // still succeeds (or, if the first mismatches, the user is stranded).
    const first = deferred<unknown>()
    let n = 0
    handlers.sync_confirm_rk = () => {
      n += 1
      if (n === 1) return first.promise
      throw { code: "nothing_pending", message: "" }
    }
    await openCreateToShow()
    fireEvent.click(screen.getByRole("button", { name: t("settings.sync.rk.next") }))
    fireEvent.change(screen.getByLabelText(t("settings.sync.rk.entry")), { target: { value: RK } })
    fireEvent.click(screen.getByRole("button", { name: t("settings.sync.rk.confirm") }))
    // Publishing takes seconds with no busy indicator; the user re-types and presses again.
    const again = screen.queryByLabelText(t("settings.sync.rk.entry"))
    if (again) {
      fireEvent.change(again, { target: { value: RK } })
      const btn = screen.queryByRole("button", { name: t("settings.sync.rk.confirm") }) as HTMLButtonElement | null
      if (btn) fireEvent.click(btn)
    }
    await act(async () => {})
    expect(calls("sync_confirm_rk")).toHaveLength(1)
    first.resolve({ result: "created", vault: "9f8e7d6c" })
    await screen.findByText(t("settings.sync.rk.created"))
    expect(screen.queryByText(t("settings.sync.error.nothing_pending"))).toBeNull()
  })

  it("RK-7 pasted RK with spaces, dashes, lower case, newlines reaches Rust verbatim (Rust parse normalizes)", async () => {
    // Replica of recovery.rs `RecoveryKey::parse` normalization.
    const lookAlike: Record<string, string> = { "0": "O", "1": "I", "8": "B" }
    const rustNormalize = (s: string) =>
      [...s]
        .filter((c) => !/\s/.test(c) && c !== "-")
        .map((c) => lookAlike[c.toUpperCase()] ?? c.toUpperCase())
        .join("")
    const canonical = rustNormalize(RK)
    const variants = [
      `  ${RK}\n`,
      RK.replaceAll("-", " "),
      RK.replaceAll("-", ""),
      RK.toLowerCase(),
      RK_GROUPS.join("\t"),
      RK.replaceAll("-", "--"),
    ]
    for (const v of variants) expect(rustNormalize(v), JSON.stringify(v)).toBe(canonical)

    handlers.sync_confirm_rk = (args) => {
      if (rustNormalize(String(args?.rk)) !== canonical) throw { code: "confirmation_mismatch", message: "" }
      return { result: "created", vault: "9f8e7d6c" }
    }
    await openCreateToShow()
    fireEvent.click(screen.getByRole("button", { name: t("settings.sync.rk.next") }))
    const entry = screen.getByLabelText(t("settings.sync.rk.entry"))
    // Whitespace-only paste keeps Confirm disabled.
    fireEvent.change(entry, { target: { value: " \n\t " } })
    expect((screen.getByRole("button", { name: t("settings.sync.rk.confirm") }) as HTMLButtonElement).disabled).toBe(true)
    fireEvent.change(entry, { target: { value: variants[3] } })
    fireEvent.click(screen.getByRole("button", { name: t("settings.sync.rk.confirm") }))
    await screen.findByText(t("settings.sync.rk.created"))
    expect(calls("sync_confirm_rk")[0][1]).toEqual({ rk: variants[3] })
    expect(document.body.innerHTML).not.toContain(variants[3])
  })

  it("RK-7b join preview forwards a dash-less, spaced paste verbatim", async () => {
    current = connected({ needs_vault: true, discovery: "found" })
    handlers.sync_discover = () => ({ state: "found", vaults: 1 })
    handlers.sync_join_preview = () => ({ vault: "9f8e7d6c", summary: { skip: 0, republish: 0, copy: 0, evicted: 0, tombstones: 0 } })
    await renderSection()
    fireEvent.click(await screen.findByRole("button", { name: t("settings.sync.vault.join") }))
    const pasted = ` ${RK_GROUPS.join(" ").toLowerCase()} `
    fireEvent.change(screen.getByLabelText(t("settings.sync.rk.entry")), { target: { value: pasted } })
    fireEvent.click(screen.getByRole("button", { name: t("settings.sync.join.preview") }))
    await waitFor(() => expect(calls("sync_join_preview")).toHaveLength(1))
    expect(calls("sync_join_preview")[0][1]).toEqual({ rk: pasted })
  })

  it("G9 blocker: RK-8 Copy writes exactly the RK through navigator.clipboard; the dialog says it is not kept here, and nothing survives close", async () => {
    // U1 (plan-amendments.md, "G9 pass-1 review decisions"): the Copy button
    // stays on `navigator.clipboard.writeText`. The capture guarantee lives in
    // Rust: `clipboard.rs` `looks_sensitive` treats any checksum-valid RK as
    // sensitive, whatever the copy path (Copy, manual Cmd+C, another app).
    // Owned by the Rust tests `looks_sensitive_drops_any_recovery_key_form`
    // and `looks_sensitive_keeps_a_recovery_key_shape_with_a_bad_checksum`
    // (clipboard.rs) plus `is_recovery_key_matches_parse_and_rejects_everything_else`
    // (recovery.rs). This case pins the webview side of the contract.
    const spies = (["log", "info", "warn", "error", "debug"] as const).map((m) => vi.spyOn(console, m))
    await openCreateToShow()
    // The dialog states the offline rule and that the app keeps nothing.
    const desc = t("settings.sync.rk.showDesc")
    expect(desc).toMatch(/offline/i)
    expect(desc).toMatch(/does not keep/i)
    expect(screen.getByText(desc)).toBeTruthy()
    expect(screen.getByText(t("settings.sync.rk.offline"))).toBeTruthy()
    fireEvent.click(screen.getByRole("button", { name: t("settings.sync.rk.copy") }))
    await act(async () => {})
    // Exactly the RK, once, with nothing appended or logged.
    expect(mocks.writeText).toHaveBeenCalledTimes(1)
    expect(mocks.writeText).toHaveBeenCalledWith(RK)
    for (const s of spies) for (const args of s.mock.calls) expect(JSON.stringify(args)).not.toContain(RK_GROUPS[0] + "-")
    for (const s of spies) s.mockRestore()
    await screen.findByText(t("settings.sync.rk.copied"))
    // Close without confirming: no RK in the DOM, none in any later invoke.
    fireEvent.click(screen.getByRole("button", { name: t("common.cancel") }))
    await waitFor(() => expect(screen.queryByRole("dialog")).toBeNull())
    expect(document.body.innerHTML).not.toContain(RK)
    expect(document.body.innerHTML).not.toContain(RK_GROUPS[0] + "-" + RK_GROUPS[1])
    const before = mocks.invoke.mock.calls.length
    await renderSection()
    for (const [, args] of mocks.invoke.mock.calls.slice(before)) expect(JSON.stringify(args ?? {})).not.toContain(RK_GROUPS[0] + "-")
  })
})

/* ─────────────────────────── Failure handling ─────────────────────────── */

describe("failure handling", () => {
  const cases: Array<{ provider: "gdrive" | "onedrive" | "webdav" | "r2"; code: string; radio: RegExp }> = [
    { provider: "gdrive", code: "oauth_denied", radio: /Google Drive/ },
    { provider: "onedrive", code: "timeout", radio: /OneDrive/ },
    { provider: "webdav", code: "permission_denied", radio: /WebDAV/ },
    { provider: "r2", code: "configuration", radio: /Cloudflare R2/ },
  ]
  for (const c of cases) {
    it(`F-1 connect error for ${c.provider} (${c.code}) shows localized copy, unlocks the form, leaks no secret`, async () => {
      current = status({ enabled: true })
      handlers.sync_connect_start = () => {
        throw { code: c.code, message: "raw rust message s3cret" }
      }
      await renderSection()
      fireEvent.click(screen.getByRole("radio", { name: c.radio }))
      if (c.provider === "webdav") {
        fireEvent.change(screen.getByLabelText(t("settings.sync.connect.field.base_url")), { target: { value: "https://dav.example/" } })
        fireEvent.change(screen.getByLabelText(t("settings.sync.connect.field.password")), { target: { value: "s3cret" } })
      }
      if (c.provider === "r2") {
        fireEvent.change(screen.getByLabelText(t("settings.sync.connect.field.secret_access_key")), { target: { value: "s3cret" } })
      }
      const oauth = c.provider === "gdrive" || c.provider === "onedrive"
      const label = oauth ? t("settings.sync.connect.signIn") : t("settings.sync.connect.connect")
      fireEvent.click(screen.getByRole("button", { name: label }))
      const alert = await screen.findByRole("alert")
      expect(alert.textContent).toBe(t(`settings.sync.error.${c.code}`))
      expect(calls("sync_connect_start")[0][1]).toMatchObject({ provider: c.provider })
      expect(document.body.textContent).not.toContain("s3cret")
      expect(document.body.textContent).not.toContain("raw rust message")
      expect((screen.getByRole("button", { name: label }) as HTMLButtonElement).disabled).toBe(false)
    })
  }

  it("F-1b an unknown Rust code and a non-object rejection both read generic copy", async () => {
    current = status({ enabled: true })
    handlers.sync_connect_start = () => {
      throw { code: "sql", message: "" }
    }
    await renderSection()
    fireEvent.click(screen.getByRole("button", { name: t("settings.sync.connect.signIn") }))
    expect((await screen.findByRole("alert")).textContent).toBe(t("settings.sync.error.generic"))
    handlers.sync_connect_start = () => {
      throw "ipc exploded"
    }
    await waitFor(() =>
      expect((screen.getByRole("button", { name: t("settings.sync.connect.signIn") }) as HTMLButtonElement).disabled).toBe(false),
    )
    fireEvent.click(screen.getByRole("button", { name: t("settings.sync.connect.signIn") }))
    await waitFor(() => expect(calls("sync_connect_start")).toHaveLength(2))
    expect((await screen.findByRole("alert")).textContent).toBe(t("settings.sync.error.generic"))
    expect(document.body.textContent).not.toContain("ipc exploded")
  })

  it("F-2 OAuth cancel: Cancel calls sync_connect_cancel, the cancelled start reads 'cancelled' and the form unlocks", async () => {
    current = status({ enabled: true })
    const start = deferred<unknown>()
    handlers.sync_connect_start = () => start.promise
    handlers.sync_connect_cancel = () => {
      start.reject({ code: "cancelled", message: "" })
      return undefined
    }
    await renderSection()
    fireEvent.click(screen.getByRole("radio", { name: /OneDrive/ }))
    fireEvent.click(screen.getByRole("button", { name: t("settings.sync.connect.signIn") }))
    await screen.findByText(t("settings.sync.connect.waiting"))
    // Every provider card is locked while the browser round-trip runs.
    for (const r of screen.getAllByRole("radio")) expect((r as HTMLButtonElement).disabled).toBe(true)
    fireEvent.click(screen.getByRole("button", { name: t("common.cancel") }))
    expect(calls("sync_connect_cancel")).toHaveLength(1)
    await screen.findByText(t("settings.sync.error.cancelled"))
    expect(screen.queryByText(t("settings.sync.connect.waiting"))).toBeNull()
    expect((screen.getByRole("button", { name: t("settings.sync.connect.signIn") }) as HTMLButtonElement).disabled).toBe(false)
    expect(calls("sync_connect_start")[0][1]).toEqual({ provider: "onedrive", params: null })
  })

  it("F-3 status poll error before any status: store_error and non-object rejections read localized copy, then recover", async () => {
    useFake()
    let fail: unknown = { code: "store_error", message: "db locked" }
    statusOverride = async () => {
      if (fail !== null) throw fail
      return joined()
    }
    render(<SyncSection uiLocale="en" />)
    await tick()
    expect(screen.getByText(t("settings.sync.error.store_error"))).toBeTruthy()
    expect(document.body.textContent).not.toContain("db locked")
    fail = new Error("bridge gone")
    await tick(STATUS_POLL_MS)
    expect(screen.getByText(t("settings.sync.error.generic"))).toBeTruthy()
    fail = null
    await tick(STATUS_POLL_MS)
    expect(screen.getByText("9f8e7d6c")).toBeTruthy()
    expect(screen.queryByText(t("settings.sync.error.generic"))).toBeNull()
  })

  it("F-3b a poll error after a good status keeps the last panel rendered (no crash, no blank)", async () => {
    useFake()
    current = joined()
    render(<SyncSection uiLocale="en" />)
    await tick()
    expect(screen.getByText("9f8e7d6c")).toBeTruthy()
    statusOverride = async () => {
      throw { code: "store_error", message: "" }
    }
    await tick(STATUS_POLL_MS)
    expect(screen.getByText("9f8e7d6c")).toBeTruthy()
  })

  it("F-3c store_error from the status and from syncState() render; queue_write_failed reads the refused-write copy", async () => {
    current = joined({ store_error: true })
    mocks.client.state = { ...mocks.client.state, error: "queue_write_failed", writeFailures: 3 }
    await renderSection()
    expect(screen.getAllByText(t("settings.sync.error.store_error")).length).toBeGreaterThan(0)
    expect(screen.getByText(t("settings.sync.local.refused").replace("{count}", "3"))).toBeTruthy()
    cleanup()
    mocks.client.state = { ...mocks.client.state, error: "store_error", writeFailures: 0 }
    current = joined()
    await renderSection()
    expect(screen.getAllByText(t("settings.sync.error.store_error")).length).toBe(1)
    // A late client update (external store) re-renders without a status poll.
    act(() => {
      mocks.client.state = { ...mocks.client.state, error: null, finalize: "deferred" }
      for (const fn of mocks.client.subs) fn()
    })
    expect(screen.queryByText(t("settings.sync.error.store_error"))).toBeNull()
    expect(screen.getByText(t("settings.sync.migration.deferred"))).toBeTruthy()
  })

  it("F-4 an unknown future EngineNotice variant from Rust does not crash and keeps a dismiss action", async () => {
    current = joined({ notices: [{ notice: "brand_new_thing", extra: 1 } as unknown as EngineNotice, { notice: "rate_limited" }] })
    handlers.sync_dismiss_notice = () => true
    await renderSection()
    const li = document.querySelector('li[data-notice="brand_new_thing"]') as HTMLElement
    expect(li).not.toBeNull()
    expect(document.querySelector('li[data-notice="rate_limited"]')).not.toBeNull()
    fireEvent.click(within(li).getByRole("button", { name: t("settings.sync.notice.action.dismiss") }))
    await waitFor(() => expect(calls("sync_dismiss_notice")).toHaveLength(1))
    expect(calls("sync_dismiss_notice")[0][1]).toEqual({ notice: "brand_new_thing" })
  })

  it("G9 blocker: F-4b an unknown EngineNotice renders localized fallback copy, never its raw snake_case tag", async () => {
    // sync-copy.ts `noticeText` default branch reads `notice.unknown`; the
    // contract (§8 "any other variant") requires one localized message.
    const n = { notice: "brand_new_thing" } as unknown as EngineNotice
    for (const locale of UI_LOCALES) {
      const text = noticeText(tl(locale), n)
      expect(text, locale).not.toContain("brand_new_thing")
      expect(text.trim(), locale).not.toBe("")
    }
  })

  it("F-5 an unknown CycleStatus or failure class does not crash the status panel", async () => {
    current = joined({ last_status: { status: "paused_for_upgrade" } as never, stats: { ...status().stats, last_error: "new_class" as never } })
    await renderSection()
    expect(screen.getByText("9f8e7d6c")).toBeTruthy()
  })
})

/* ───────────────────────────── Other flows ───────────────────────────── */

describe("other flows", () => {
  it("O-1 join: wrong RK shows D-1, typing clears it, the correct RK previews, commit runs once", async () => {
    current = connected({ needs_vault: true, discovery: "found" })
    handlers.sync_discover = () => ({ state: "found", vaults: 1 })
    handlers.sync_join_preview = (args) => {
      if (args?.rk !== RK) throw { code: "rk_wrong", message: "" }
      return { vault: "9f8e7d6c", summary: { skip: 1, republish: 2, copy: 0, evicted: 0, tombstones: 0 } }
    }
    const commit = deferred<unknown>()
    handlers.sync_join_commit = () => commit.promise
    await renderSection()
    fireEvent.click(await screen.findByRole("button", { name: t("settings.sync.vault.join") }))
    const entry = screen.getByLabelText(t("settings.sync.rk.entry"))
    fireEvent.change(entry, { target: { value: "WRONG-" + RK } })
    fireEvent.click(screen.getByRole("button", { name: t("settings.sync.join.preview") }))
    await screen.findByText(t("settings.sync.join.wrongKey"))
    expect((screen.getByLabelText(t("settings.sync.rk.entry")) as HTMLInputElement).value).toBe("")
    expect(document.body.innerHTML).not.toContain("WRONG-")
    fireEvent.change(screen.getByLabelText(t("settings.sync.rk.entry")), { target: { value: RK } })
    expect(screen.queryByText(t("settings.sync.join.wrongKey"))).toBeNull()
    fireEvent.click(screen.getByRole("button", { name: t("settings.sync.join.preview") }))
    await screen.findByText(t("settings.sync.join.skip").replace("{count}", "1"))
    expect(calls("sync_join_preview").map(([, a]) => a)).toEqual([{ rk: "WRONG-" + RK }, { rk: RK }])
    const confirm = screen.getByRole("button", { name: t("settings.sync.join.confirm") })
    fireEvent.click(confirm)
    fireEvent.click(confirm)
    expect(calls("sync_join_commit")).toHaveLength(1)
    commit.resolve({ result: "joined", vault: "9f8e7d6c", ops: 3 })
    await screen.findByText(t("settings.sync.join.done").replace("{count}", "3"))
    expect(document.body.innerHTML).not.toContain(RK)
  })

  it("O-1b join: preview_changed from commit shows the new counts and needs a second confirm", async () => {
    current = connected({ needs_vault: true, discovery: "found" })
    handlers.sync_discover = () => ({ state: "found", vaults: 1 })
    handlers.sync_join_preview = () => ({ vault: "9f8e7d6c", summary: { skip: 1, republish: 1, copy: 0, evicted: 0, tombstones: 0 } })
    let n = 0
    handlers.sync_join_commit = () =>
      ++n === 1
        ? { result: "preview_changed", summary: { skip: 5, republish: 6, copy: 7, evicted: 8, tombstones: 0 } }
        : { result: "joined", vault: "9f8e7d6c", ops: 6 }
    await renderSection()
    fireEvent.click(await screen.findByRole("button", { name: t("settings.sync.vault.join") }))
    fireEvent.change(screen.getByLabelText(t("settings.sync.rk.entry")), { target: { value: RK } })
    fireEvent.click(screen.getByRole("button", { name: t("settings.sync.join.preview") }))
    fireEvent.click(await screen.findByRole("button", { name: t("settings.sync.join.confirm") }))
    await screen.findByText(t("settings.sync.join.evicted").replace("{count}", "8"))
    expect(calls("sync_join_commit")).toHaveLength(1)
    await waitFor(() =>
      expect((screen.getByRole("button", { name: t("settings.sync.join.confirm") }) as HTMLButtonElement).disabled).toBe(false),
    )
    fireEvent.click(screen.getByRole("button", { name: t("settings.sync.join.confirm") }))
    await screen.findByText(t("settings.sync.join.done").replace("{count}", "6"))
  })

  it("O-2 two-vault banner: Join opens the join flow, Later/Keep dismiss other_vaults, nothing is stranded", async () => {
    current = joined({ notices: [{ notice: "other_vaults", count: 1 }] })
    handlers.sync_dismiss_notice = () => {
      current = joined()
      return true
    }
    // U2: the banner's join runs sync_discover before the preview.
    handlers.sync_discover = () => ({ state: "found", vaults: 2 })
    handlers.sync_join_preview = () => {
      throw { code: "rk_wrong", message: "" }
    }
    await renderSection()
    const li = () => document.querySelector('li[data-notice="other_vaults"]') as HTMLElement | null
    fireEvent.click(within(li()!).getByRole("button", { name: t("settings.sync.notice.action.joinOther") }))
    const dialog = await screen.findByRole("dialog")
    expect(within(dialog).getByText(t("settings.sync.join.title"))).toBeTruthy()
    fireEvent.change(within(dialog).getByLabelText(t("settings.sync.rk.entry")), { target: { value: RK } })
    fireEvent.click(within(dialog).getByRole("button", { name: t("settings.sync.join.preview") }))
    await screen.findByText(t("settings.sync.join.wrongKey"))
    expect(calls("sync_discover")).toHaveLength(1)
    fireEvent.click(within(dialog).getByRole("button", { name: t("common.cancel") }))
    await waitFor(() => expect(screen.queryByRole("dialog")).toBeNull())
    expect(li()).not.toBeNull()
    fireEvent.click(within(li()!).getByRole("button", { name: t("settings.sync.notice.action.later") }))
    await waitFor(() => expect(li()).toBeNull())
    // Observation: "Later" and "Keep current" send the identical dismiss.
    expect(calls("sync_dismiss_notice").map(([, a]) => a)).toEqual([{ notice: "other_vaults" }])
  })

  it("O-3 wipe confirm matches Rust check_wipe_confirmation exactly for localized, padded and look-alike words", async () => {
    const rustAccepts = (word: string) => word === "삭제" // commands.rs check_wipe_confirmation / vault::WIPE_CONFIRMATION
    expect(WIPE_CONFIRMATION).toBe("삭제")
    current = joined()
    await renderSection()
    fireEvent.click(screen.getByRole("button", { name: t("settings.sync.disconnect.title") }))
    const dialog = await screen.findByRole("dialog")
    fireEvent.click(within(dialog).getByRole("checkbox"))
    const word = within(dialog).getByLabelText(t("settings.sync.disconnect.wipeWord"))
    const confirm = () => within(dialog).getByRole("button", { name: t("settings.sync.disconnect.confirmWipe") }) as HTMLButtonElement
    const inputs = [
      "", "delete", "Delete", "DELETE", "supprimer", "löschen", "削除", "删除", "xóa", "eliminar",
      " 삭제", "삭제 ", " 삭제 ", "삭제\n", "\t삭제", "삭 제", "삭제.", "삭제삭제", "삭제".normalize("NFD"), "삭제" + ZWSP,
      "삭제",
    ]
    for (const w of inputs) {
      fireEvent.change(word, { target: { value: w } })
      // A text input strips line breaks on paste; Rust receives the sanitized value.
      const sent = (word as HTMLInputElement).value
      expect(confirm().disabled, JSON.stringify(w)).toBe(!rustAccepts(sent))
    }
    // Unchecking resets the typed word: re-checking requires typing it again.
    fireEvent.click(within(dialog).getByRole("checkbox"))
    fireEvent.click(within(dialog).getByRole("checkbox"))
    expect((within(dialog).getByLabelText(t("settings.sync.disconnect.wipeWord")) as HTMLInputElement).value).toBe("")
    expect(confirm().disabled).toBe(true)
  })

  it("O-3b every locale tells the user to type the literal Rust word", () => {
    for (const locale of UI_LOCALES) {
      const line = UI_STRINGS[locale]["settings.sync.disconnect.typeWord"]
      expect(line, locale).toContain("{word}")
    }
  })

  it("G9 blocker: O-4a dismiss race - a status poll issued before the dismiss and resolving after it does not resurrect the notice", async () => {
    useFake()
    current = joined({ notices: [{ notice: "rate_limited" }] })
    render(<SyncSection uiLocale="en" />)
    await tick()
    expect(document.querySelector('li[data-notice="rate_limited"]')).not.toBeNull()
    // The 5 s poll starts and is slow.
    const slow = deferred<SyncStatus>()
    const stale = current
    statusOverride = () => slow.promise
    await tick(STATUS_POLL_MS)
    statusOverride = null
    handlers.sync_dismiss_notice = () => {
      current = joined()
      return true
    }
    fireEvent.click(screen.getByRole("button", { name: t("settings.sync.notice.action.dismiss") }))
    await tick()
    expect(document.querySelector('li[data-notice="rate_limited"]')).toBeNull()
    // The stale poll lands last.
    await act(async () => {
      slow.resolve(stale)
      await slow.promise
    })
    expect(document.querySelector('li[data-notice="rate_limited"]')).toBeNull()
  })

  it("O-4b dismiss: a rejected dismiss keeps the notice and shows an error; while one dismiss runs every notice action is locked", async () => {
    current = joined({ notices: [{ notice: "rate_limited" }, { notice: "quota_exceeded" }] })
    const pending = deferred<unknown>()
    handlers.sync_dismiss_notice = () => pending.promise
    await renderSection()
    const a = document.querySelector('li[data-notice="rate_limited"]') as HTMLElement
    const b = document.querySelector('li[data-notice="quota_exceeded"]') as HTMLElement
    fireEvent.click(within(a).getByRole("button"))
    await waitFor(() => expect((within(b).getByRole("button") as HTMLButtonElement).disabled).toBe(true))
    fireEvent.click(within(b).getByRole("button"))
    expect(calls("sync_dismiss_notice")).toHaveLength(1)
    pending.reject({ code: "transient", message: "" })
    await screen.findByText(t("settings.sync.error.transient"))
    expect(document.querySelector('li[data-notice="rate_limited"]')).not.toBeNull()
  })

  it("O-4c dismiss of a notice Rust already dropped (false) just refreshes", async () => {
    current = joined({ notices: [{ notice: "fork_detected" }] })
    handlers.sync_dismiss_notice = () => {
      current = joined()
      return false
    }
    await renderSection()
    fireEvent.click(screen.getByRole("button", { name: t("settings.sync.notice.action.dismiss") }))
    await waitFor(() => expect(document.querySelector('li[data-notice="fork_detected"]')).toBeNull())
    expect(screen.queryByRole("alert")).toBeNull()
  })

  it("G9 blocker: O-4d two notices sharing a tag (Rust BTreeSet keeps OtherVaults{1} and {2}) collapse to one banner with no duplicate-key warning", async () => {
    const err = vi.spyOn(console, "error").mockImplementation(() => {})
    current = joined({ notices: [{ notice: "other_vaults", count: 1 }, { notice: "other_vaults", count: 2 }] })
    await renderSection()
    const dupKey = err.mock.calls.some((c) => c.map(String).join(" ").includes("same key"))
    err.mockRestore()
    // U6 (O-4d): duplicate tags collapse to one banner, the last one listed.
    const items = document.querySelectorAll('li[data-notice="other_vaults"]')
    expect(items.length).toBe(1)
    expect(items[0].textContent).toContain(noticeText(t, { notice: "other_vaults", count: 2 }))
    expect(dupKey).toBe(false)
  })

  it("O-5 unmount stops the poll, the A9 timers and the event listeners (no leaked timers)", async () => {
    useFake()
    current = connected({ needs_vault: true, discovery: "retry", discovery_retry_at: Date.now() + 60_000 })
    handlers.sync_discover = () => ({ state: "retry", vaults: 0 })
    const out = render(<SyncSection uiLocale="en" />)
    await tick()
    await tick(STATUS_POLL_MS)
    expect(calls("sync_get_status").length).toBeGreaterThanOrEqual(2)
    expect(vi.getTimerCount()).toBeGreaterThan(0)
    out.unmount()
    expect(vi.getTimerCount()).toBe(0)
    const before = mocks.invoke.mock.calls.length
    await tick(STATUS_POLL_MS * 10 + 60_000)
    expect(mocks.invoke.mock.calls.length).toBe(before)
    expect(mocks.unlisten).toHaveBeenCalledTimes(2)
    document.dispatchEvent(new Event("visibilitychange"))
    expect(mocks.invoke.mock.calls.length).toBe(before)
  })

  it("O-5b a listen() that resolves after unmount is unlistened immediately", async () => {
    const late = [deferred<() => void>(), deferred<() => void>()]
    let i = 0
    mocks.listen.mockImplementation(() => late[i++].promise)
    current = joined()
    const out = await renderSection()
    out.unmount()
    const un = vi.fn()
    await act(async () => {
      late[0].resolve(un)
      late[1].resolve(un)
      await Promise.all(late.map((d) => d.promise))
    })
    expect(un).toHaveBeenCalledTimes(2)
  })

  it("O-6 A9: a discovery_retry_at in the past re-discovers promptly, shows 0 s (never negative), and does not loop", async () => {
    useFake()
    const past = Date.now() - 30_000
    current = connected({ needs_vault: true, discovery: "retry", discovery_retry_at: past })
    handlers.sync_discover = () => ({ state: "retry", vaults: 0 })
    render(<SyncSection uiLocale="en" />)
    await tick()
    await tick(10)
    // Arrival discover + the overdue A9 retry.
    expect(calls("sync_discover").length).toBeGreaterThanOrEqual(2)
    expect(screen.getByText(/Checking again in 0 s\./)).toBeTruthy()
    expect(document.body.textContent).not.toMatch(/in -\d/)
    const after = calls("sync_discover").length
    await tick(60_000)
    expect(calls("sync_discover").length).toBe(after)
    // Rust then schedules a fresh retry: A9 re-arms for it.
    current = connected({ needs_vault: true, discovery: "retry", discovery_retry_at: Date.now() + 2_000 })
    await tick(STATUS_POLL_MS)
    await tick(2_100)
    expect(calls("sync_discover").length).toBe(after + 1)
  })
})

/* ─────────────────────────────── Locales ─────────────────────────────── */

describe("locales", () => {
  const src = readSrc("components/SyncSection.tsx")
  const copy = readSrc("lib/sync-copy.ts")
  const literalKeys = [...new Set([...`${src}\n${copy}`.matchAll(/\bt\("([a-zA-Z0-9_.]+)"\)/g)].map((m) => m[1]))]
  const dynamicKeys = [
    ...["base_url", "username", "password", "account_id", "bucket", "access_key_id", "secret_access_key"].map(
      (f) => `settings.sync.connect.field.${f}`,
    ),
    ...["onedrive", "webdav", "r2"].map((k) => `settings.sync.connect.${k}`),
    ...KNOWN_ERROR_CODES.map((c) => `settings.sync.error.${c}`),
    ...ALL_NOTICES.map((n) => `settings.sync.notice.${n.notice}`),
  ]

  it("L-0 the source's literal keys were found (guards the regex)", () => {
    expect(literalKeys.length).toBeGreaterThan(60)
    expect(literalKeys).toContain("settings.sync.rk.offline")
  })

  for (const locale of UI_LOCALES) {
    it(`L-1 ${locale}: every key the UI can ask for exists, is non-empty, and keeps its placeholders`, () => {
      for (const key of [...literalKeys, ...dynamicKeys]) {
        const v = UI_STRINGS[locale][key]
        expect(v, `${locale} ${key}`).toBeTypeOf("string")
        expect(v.trim(), `${locale} ${key}`).not.toBe("")
        for (const ph of en[key].match(/\{[a-z_]+\}/g) ?? []) expect(v, `${locale} ${key} ${ph}`).toContain(ph)
      }
      const tt = tl(locale)
      for (const n of ALL_NOTICES) expect(noticeText(tt, n), `${locale} ${n.notice}`).not.toMatch(/settings\.sync|\{[a-z_]+\}/)
      for (const c of [...KNOWN_ERROR_CODES, "sql", "internal"]) expect(errorText(tt, c), `${locale} ${c}`).not.toMatch(/settings\.sync/)
    })

    it(`L-2 ${locale}: every state renders without key ids or unfilled placeholders`, async () => {
      const tt = tl(locale)
      // off
      current = status()
      await renderSection(locale)
      expectNoRawKeys(`${locale} off`)
      cleanup()

      // connect, every provider card + an error banner
      current = status({ enabled: true })
      handlers.sync_connect_start = () => {
        throw { code: "invalid_params", message: "" }
      }
      await renderSection(locale)
      for (const r of screen.getAllByRole("radio")) {
        fireEvent.click(r)
        expectNoRawKeys(`${locale} connect ${r.textContent}`)
      }
      fireEvent.click(screen.getByRole("button", { name: tt("settings.sync.connect.connect") }))
      await screen.findByRole("alert")
      expectNoRawKeys(`${locale} connect error`)
      cleanup()

      // needs vault: each discovery state
      for (const d of ["nothing_found", "found", "retry", "unknown"] as const) {
        current = connected({ needs_vault: true, discovery: d, discovery_retry_at: d === "retry" ? Date.now() + 9_000 : null })
        handlers.sync_discover = () => ({ state: d === "found" ? "found" : d === "retry" ? "retry" : "none", vaults: 2 })
        await renderSection(locale)
        await waitFor(() => expect(calls("sync_discover").length).toBeGreaterThan(0))
        expectNoRawKeys(`${locale} vault ${d}`)
        cleanup()
      }
      current = connected({ needs_vault: true, create_pending: true })
      await renderSection(locale)
      expectNoRawKeys(`${locale} create pending`)
      cleanup()

      // ready: all notices, deferred migration, wipe, store/local errors, advanced, every cycle status
      for (const last of [
        { status: "ok" },
        { status: "stopped", reason: "reconsent" },
        { status: "rate_limited", retry_after: { secs: 3, nanos: 0 } },
        { status: "failed", class: "quota" },
      ] as const) {
        mocks.client.state = { ...mocks.client.state, error: "queue_write_failed", writeFailures: 2, finalize: "deferred" }
        current = joined({ notices: ALL_NOTICES, last_status: last, wipe: "started", store_error: true, reissue_pending: last.status === "ok" })
        await renderSection(locale)
        fireEvent.click(screen.getByRole("button", { name: tt("settings.sync.advanced.title") }))
        expectNoRawKeys(`${locale} ready ${last.status}`)
        cleanup()
      }
      mocks.client.state = { ...mocks.client.state, error: "marker_write_failed", writeFailures: 0, finalize: null }
      current = joined({ migration: "imported", wipe: "done", last_cycle_at: null, running: true, last_status: null })
      await renderSection(locale)
      expectNoRawKeys(`${locale} ready moving`)

      // disconnect dialog with wipe
      fireEvent.click(screen.getAllByRole("button", { name: tt("settings.sync.disconnect.title") })[0])
      const dialog = await screen.findByRole("dialog")
      fireEvent.click(within(dialog).getByRole("checkbox"))
      expect(dialog.textContent, locale).toContain(WIPE_CONFIRMATION)
      expectNoRawKeys(`${locale} disconnect`)
      cleanup()

      // RK create dialog: show → confirm → mismatch → done
      current = connected({ needs_vault: true, discovery: "nothing_found" })
      handlers.sync_discover = () => ({ state: "none", vaults: 0 })
      handlers.sync_create_vault = () => ({ vault: "9f8e7d6c", recovery_key: RK })
      handlers.sync_confirm_rk = (args) => {
        if (args?.rk !== RK) throw { code: "rk_format", message: "" }
        return { result: "created", vault: "9f8e7d6c" }
      }
      await renderSection(locale)
      fireEvent.click(await screen.findByRole("button", { name: tt("settings.sync.vault.create") }))
      await screen.findByTestId("rk-value")
      expectNoRawKeys(`${locale} rk show`)
      fireEvent.click(screen.getByRole("button", { name: tt("settings.sync.rk.next") }))
      fireEvent.change(screen.getByLabelText(tt("settings.sync.rk.entry")), { target: { value: "x" } })
      fireEvent.click(screen.getByRole("button", { name: tt("settings.sync.rk.confirm") }))
      await screen.findByText(tt("settings.sync.rk.mismatch"))
      expectNoRawKeys(`${locale} rk mismatch`)
      fireEvent.change(screen.getByLabelText(tt("settings.sync.rk.entry")), { target: { value: RK } })
      fireEvent.click(screen.getByRole("button", { name: tt("settings.sync.rk.confirm") }))
      await screen.findByText(tt("settings.sync.rk.created"))
      expectNoRawKeys(`${locale} rk done`)
      cleanup()

      // join dialog: wrong key → preview → done; rejoin with an error
      current = joined({ notices: [{ notice: "other_vaults", count: 1 }, { notice: "device_removed" }] })
      handlers.sync_join_preview = (args) => {
        if (args?.rk !== RK) throw { code: "rk_wrong", message: "" }
        return { vault: "9f8e7d6c", summary: { skip: 1, republish: 2, copy: 3, evicted: 4, tombstones: 0 } }
      }
      handlers.sync_join_commit = () => ({ result: "joined", vault: "9f8e7d6c", ops: 3 })
      handlers.sync_rejoin = () => {
        throw { code: "join_rejected", message: "" }
      }
      await renderSection(locale)
      fireEvent.click(screen.getByRole("button", { name: tt("settings.sync.notice.action.joinOther") }))
      let d = await screen.findByRole("dialog")
      fireEvent.change(within(d).getByLabelText(tt("settings.sync.rk.entry")), { target: { value: "nope" } })
      fireEvent.click(within(d).getByRole("button", { name: tt("settings.sync.join.preview") }))
      await screen.findByText(tt("settings.sync.join.wrongKey"))
      fireEvent.change(within(d).getByLabelText(tt("settings.sync.rk.entry")), { target: { value: RK } })
      fireEvent.click(within(d).getByRole("button", { name: tt("settings.sync.join.preview") }))
      await within(d).findByRole("button", { name: tt("settings.sync.join.confirm") })
      expectNoRawKeys(`${locale} join preview`)
      fireEvent.click(within(d).getByRole("button", { name: tt("settings.sync.join.confirm") }))
      await within(d).findByRole("button", { name: tt("common.close") })
      expectNoRawKeys(`${locale} join done`)
      fireEvent.click(within(d).getByRole("button", { name: tt("common.close") }))
      await waitFor(() => expect(screen.queryByRole("dialog")).toBeNull())
      fireEvent.click(screen.getAllByRole("button", { name: tt("settings.sync.notice.action.rejoin") })[0])
      d = await screen.findByRole("dialog")
      fireEvent.change(within(d).getByLabelText(tt("settings.sync.rk.entry")), { target: { value: RK } })
      fireEvent.click(within(d).getByRole("button", { name: tt("settings.sync.notice.action.rejoin") }))
      await within(d).findByRole("alert")
      expectNoRawKeys(`${locale} rejoin error`)
    })
  }
})

/* ─────────────── Delta (pass 2): attacks on the U1-U6 decisions ─────────────── */

describe("delta: U1-U6 pass-1 decisions", () => {
  const listeners: Record<string, () => void> = {}
  beforeEach(() => {
    for (const k of Object.keys(listeners)) delete listeners[k]
    mocks.listen.mockImplementation(async (name, cb) => {
      listeners[name] = cb
      return mocks.unlisten
    })
  })

  /** One status refresh, as a `sync:chat-changed` event triggers it. */
  async function pushRefresh() {
    await waitFor(() => expect(listeners["sync:chat-changed"]).toBeDefined())
    await act(async () => {
      listeners["sync:chat-changed"]()
      await Promise.resolve()
    })
  }
  /** Commands other than the status poll, in call order. */
  const order = () => mocks.invoke.mock.calls.map(([c]) => c).filter((c) => c !== "sync_get_status")
  const SUMMARY = { skip: 1, republish: 0, copy: 0, evicted: 0, tombstones: 0 }
  const skipLine = t("settings.sync.join.skip").replace("{count}", "1")
  const li = (tag: string) => document.querySelector(`li[data-notice="${tag}"]`) as HTMLElement | null

  /* ── U1: the RK as the webview exposes it to a manual copy ── */

  it("D-U1a the shown RK node holds exactly the RK (select-all), so a manual select + Cmd+C hands Rust the parseable form", async () => {
    await openCreateToShow()
    const node = screen.getByTestId("rk-value")
    expect(node.className).toContain("select-all")
    // No label, whitespace or zero-width text inside the selectable node.
    expect(node.textContent).toBe(RK)
    expect(node.children).toHaveLength(0)
    // The Copy path writes the same text.
    fireEvent.click(screen.getByRole("button", { name: t("settings.sync.rk.copy") }))
    await act(async () => {})
    expect(mocks.writeText).toHaveBeenCalledWith(node.textContent)
  })

  /* ── U2: discovery before every preview ── */

  it("D-U2a discovery fails at vault setup; Join then runs sync_discover again before sync_join_preview", async () => {
    current = connected({ needs_vault: true, discovery: "found" })
    let n = 0
    handlers.sync_discover = () => {
      if (++n === 1) throw { code: "transient", message: "" }
      return { state: "found", vaults: 1 }
    }
    handlers.sync_join_preview = () => ({ vault: "9f8e7d6c", summary: SUMMARY })
    await renderSection()
    await screen.findByText(errorText(t, "transient"))
    fireEvent.click(screen.getByRole("button", { name: t("settings.sync.vault.join") }))
    const dialog = await screen.findByRole("dialog")
    fireEvent.change(within(dialog).getByLabelText(t("settings.sync.rk.entry")), { target: { value: RK } })
    fireEvent.click(within(dialog).getByRole("button", { name: t("settings.sync.join.preview") }))
    await within(dialog).findByText(skipLine)
    expect(order()).toEqual(["sync_discover", "sync_discover", "sync_join_preview"])
  })

  it("D-U2b join banner: the dialog's discovery fails -> localized error, no preview, RK dropped; the retry discovers then previews", async () => {
    current = joined({ notices: [{ notice: "other_vaults", count: 2 }] })
    let n = 0
    handlers.sync_discover = () => {
      if (++n === 1) throw { code: "rate_limited", message: "" }
      return { state: "found", vaults: 2 }
    }
    handlers.sync_join_preview = () => ({ vault: "9f8e7d6c", summary: SUMMARY })
    await renderSection()
    fireEvent.click(within(li("other_vaults")!).getByRole("button", { name: t("settings.sync.notice.action.joinOther") }))
    const dialog = await screen.findByRole("dialog")
    fireEvent.change(within(dialog).getByLabelText(t("settings.sync.rk.entry")), { target: { value: RK } })
    fireEvent.click(within(dialog).getByRole("button", { name: t("settings.sync.join.preview") }))
    expect((await within(dialog).findByRole("alert")).textContent).toBe(errorText(t, "rate_limited"))
    expect(calls("sync_join_preview")).toHaveLength(0)
    expect((within(dialog).getByLabelText(t("settings.sync.rk.entry")) as HTMLInputElement).value).toBe("")
    expect(document.body.innerHTML).not.toContain(RK)
    fireEvent.change(within(dialog).getByLabelText(t("settings.sync.rk.entry")), { target: { value: RK } })
    fireEvent.click(within(dialog).getByRole("button", { name: t("settings.sync.join.preview") }))
    await within(dialog).findByText(skipLine)
    expect(order()).toEqual(["sync_discover", "sync_discover", "sync_join_preview"])
  })

  it("D-U2c discovery_required twice: exactly one re-discovery, then the localized discovery_required copy (no loop)", async () => {
    current = joined({ notices: [{ notice: "other_vaults", count: 1 }] })
    handlers.sync_discover = () => ({ state: "found", vaults: 2 })
    handlers.sync_join_preview = () => {
      throw { code: "discovery_required", message: "" }
    }
    await renderSection()
    fireEvent.click(within(li("other_vaults")!).getByRole("button", { name: t("settings.sync.notice.action.joinOther") }))
    const dialog = await screen.findByRole("dialog")
    fireEvent.change(within(dialog).getByLabelText(t("settings.sync.rk.entry")), { target: { value: RK } })
    fireEvent.click(within(dialog).getByRole("button", { name: t("settings.sync.join.preview") }))
    expect((await within(dialog).findByRole("alert")).textContent).toBe(errorText(t, "discovery_required"))
    await act(async () => {})
    expect(order()).toEqual(["sync_discover", "sync_join_preview", "sync_discover", "sync_join_preview"])
    // Both previews carried the RK the user typed once.
    expect(calls("sync_join_preview").map(([, a]) => a)).toEqual([{ rk: RK }, { rk: RK }])
  })

  it("D-U2d discovery_required, then the re-discovery itself fails: its error is shown, no second preview", async () => {
    current = connected({ needs_vault: true, discovery: "found" })
    let n = 0
    handlers.sync_discover = () => {
      n += 1
      if (n === 3) throw { code: "transient", message: "" }
      return { state: "found", vaults: 1 }
    }
    handlers.sync_join_preview = () => {
      throw { code: "discovery_required", message: "" }
    }
    await renderSection()
    fireEvent.click(await screen.findByRole("button", { name: t("settings.sync.vault.join") }))
    const dialog = await screen.findByRole("dialog")
    fireEvent.change(within(dialog).getByLabelText(t("settings.sync.rk.entry")), { target: { value: RK } })
    fireEvent.click(within(dialog).getByRole("button", { name: t("settings.sync.join.preview") }))
    expect((await within(dialog).findByRole("alert")).textContent).toBe(errorText(t, "transient"))
    // Setup discovery, dialog discovery, preview, failed re-discovery.
    expect(order()).toEqual(["sync_discover", "sync_discover", "sync_join_preview", "sync_discover"])
  })

  /* ── U3: reconsent never pre-fills ── */

  it("D-U3a WebDAV reconsent: the form is fixed to WebDAV, every field is empty (not even the account label), and a cancelled entry never comes back", async () => {
    current = joined({ provider: { provider: "webdav", account_label: "alice@dav.example" }, notices: [{ notice: "reconsent_required" }] })
    await renderSection()
    const open = async () => {
      fireEvent.click(screen.getByRole("button", { name: t("settings.sync.notice.action.reconnect") }))
      return screen.findByTestId("reconsent-form")
    }
    let form = await open()
    expect(within(form).queryByRole("radiogroup")).toBeNull()
    expect(within(form).getByText("WebDAV")).toBeTruthy()
    const values = (f: HTMLElement) => [...f.querySelectorAll("input")].map((i) => i.value)
    expect(values(form)).toEqual(["", "", ""])
    expect(form.textContent).not.toContain("alice")
    expect(form.innerHTML).not.toContain("alice")
    expect((within(form).getByLabelText(t("settings.sync.connect.field.password")) as HTMLInputElement).type).toBe("password")
    expect(calls("sync_connect_start")).toHaveLength(0)
    fireEvent.change(within(form).getByLabelText(t("settings.sync.connect.field.username")), { target: { value: "alice" } })
    fireEvent.change(within(form).getByLabelText(t("settings.sync.connect.field.password")), { target: { value: "hunter2-secret" } })
    fireEvent.click(within(form).getByRole("button", { name: t("common.cancel") }))
    await waitFor(() => expect(screen.queryByTestId("reconsent-form")).toBeNull())
    expect(document.body.innerHTML).not.toContain("hunter2-secret")
    form = await open()
    expect(values(form)).toEqual(["", "", ""])
    expect(calls("sync_connect_start")).toHaveLength(0)
  })

  it("D-U3b R2 reconsent: four empty fields; submit clears them before Rust answers and a rejection leaves them empty", async () => {
    current = joined({ provider: { provider: "r2", account_label: "r2 acct-1234" }, notices: [{ notice: "reconsent_required" }] })
    const pending = deferred<unknown>()
    handlers.sync_connect_start = () => pending.promise
    await renderSection()
    fireEvent.click(screen.getByRole("button", { name: t("settings.sync.notice.action.reconnect") }))
    const form = await screen.findByTestId("reconsent-form")
    const inputs = () => [...form.querySelectorAll("input")]
    expect(inputs().map((i) => i.value)).toEqual(["", "", "", ""])
    const typed = { account_id: "acct-1234", bucket: "b", access_key_id: "AKIDEXAMPLE", secret_access_key: "s3cr3t-r2-key" }
    for (const [k, v] of Object.entries(typed)) {
      fireEvent.change(within(form).getByLabelText(t(`settings.sync.connect.field.${k}`)), { target: { value: v } })
    }
    fireEvent.click(within(form).getByRole("button", { name: t("settings.sync.connect.connect") }))
    expect(calls("sync_connect_start").map(([, a]) => a)).toEqual([{ provider: "r2", params: typed }])
    expect(inputs().map((i) => i.value)).toEqual(["", "", "", ""])
    expect(document.body.innerHTML).not.toContain("s3cr3t-r2-key")
    await act(async () => {
      pending.reject({ code: "invalid_params", message: "" })
      await pending.promise.catch(() => {})
    })
    await screen.findByText(errorText(t, "invalid_params"))
    expect(screen.getByTestId("reconsent-form")).toBeTruthy()
    expect(inputs().map((i) => i.value)).toEqual(["", "", "", ""])
  })

  it("D-U3c Drive and OneDrive reconsent sign in with OAuth again: no credential form, no params", async () => {
    for (const provider of ["gdrive", "onedrive"] as const) {
      current = joined({ provider: { provider, account_label: `${provider} 1a2b` }, notices: [{ notice: "reconsent_required" }] })
      handlers.sync_connect_start = () => ({ provider, account_label: `${provider} 1a2b` })
      await renderSection()
      fireEvent.click(screen.getByRole("button", { name: t("settings.sync.notice.action.reconnect") }))
      await waitFor(() => expect(calls("sync_connect_start")).toHaveLength(1))
      expect(calls("sync_connect_start")[0][1]).toEqual({ provider, params: null })
      expect(screen.queryByTestId("reconsent-form")).toBeNull()
      cleanup()
      mocks.invoke.mockClear()
    }
  })

  /* ── U4: device picker ── */

  const SELF: DeviceSummary = { id: "dev-self-0001aaaa", short: "dev-self", last_seen: Date.now() - 60_000, is_self: true, removed: false }
  const OTHER: DeviceSummary = { id: "dev-othr-0002bbbb", short: "dev-othr", last_seen: Date.now() - 3_600_000, is_self: false, removed: false }
  const GONE: DeviceSummary = { id: "dev-gone-0003cccc", short: "dev-gone", last_seen: null, is_self: false, removed: true }
  const row = (d: DeviceSummary) => document.querySelector(`[data-device="${d.id}"]`) as HTMLButtonElement
  const removeBtn = () => screen.getByRole("button", { name: t("settings.sync.advanced.removeDevice") }) as HTMLButtonElement
  const openAdvanced = () => fireEvent.click(screen.getByRole("button", { name: t("settings.sync.advanced.title") }))

  it("D-U4a picker: this device and a removed device cannot be picked; another one is removed by id, once, and then shows as removed", async () => {
    current = joined({ devices: [SELF, OTHER, GONE] })
    handlers.sync_remove_device = () => {
      current = joined({ devices: [SELF, { ...OTHER, removed: true }, GONE] })
      return null
    }
    await renderSection()
    openAdvanced()
    expect(screen.queryByRole("textbox")).toBeNull()
    expect(row(SELF).disabled).toBe(true)
    expect(row(SELF).textContent).toContain(t("settings.sync.advanced.thisDevice"))
    expect(row(GONE).disabled).toBe(true)
    expect(row(GONE).textContent).toContain(t("settings.sync.advanced.removedDevice"))
    expect(row(OTHER).disabled).toBe(false)
    expect(removeBtn().disabled).toBe(true)
    fireEvent.click(row(OTHER))
    expect(row(OTHER).getAttribute("aria-checked")).toBe("true")
    fireEvent.click(removeBtn())
    expect(screen.getByTestId("remove-limit").textContent).toContain(OTHER.short)
    const confirm = screen.getByRole("button", { name: t("settings.sync.advanced.removeConfirm") })
    fireEvent.click(confirm)
    fireEvent.click(confirm)
    expect(calls("sync_remove_device").map(([, a]) => a)).toEqual([{ deviceId: OTHER.id }])
    await waitFor(() => expect(row(OTHER).disabled).toBe(true))
    expect(row(OTHER).textContent).toContain(t("settings.sync.advanced.removedDevice"))
    expect(removeBtn().disabled).toBe(true)
  })

  it("D-U4b a single device (this one): nothing is pickable and Remove never fires", async () => {
    current = joined({ devices: [SELF] })
    await renderSection()
    openAdvanced()
    fireEvent.click(row(SELF))
    expect(row(SELF).getAttribute("aria-checked")).toBe("false")
    expect(removeBtn().disabled).toBe(true)
    fireEvent.click(removeBtn())
    expect(screen.queryByTestId("remove-limit")).toBeNull()
    expect(calls("sync_remove_device")).toHaveLength(0)
  })

  it("D-U4c self-remove attempt: the own row is inert; if a stale list lets it through, Rust's self_removal reads localized copy", async () => {
    current = joined({ devices: [SELF, OTHER] })
    await renderSection()
    openAdvanced()
    fireEvent.click(row(SELF))
    fireEvent.keyDown(row(SELF), { key: " " })
    expect(removeBtn().disabled).toBe(true)
    expect(calls("sync_remove_device")).toHaveLength(0)
    cleanup()
    // Stale snapshot: Rust knows OTHER is this device now (is_self disagrees).
    current = joined({ devices: [OTHER] })
    handlers.sync_remove_device = () => {
      throw { code: "self_removal", message: "" }
    }
    await renderSection()
    openAdvanced()
    fireEvent.click(row(OTHER))
    fireEvent.click(removeBtn())
    fireEvent.click(screen.getByRole("button", { name: t("settings.sync.advanced.removeConfirm") }))
    await screen.findByText(errorText(t, "self_removal"))
    expect(document.body.textContent).not.toContain("self_removal")
  })

  it("D-U4d the picked device is removed elsewhere before the confirm: the confirm closes and nothing is sent", async () => {
    current = joined({ devices: [SELF, OTHER] })
    await renderSection()
    openAdvanced()
    fireEvent.click(row(OTHER))
    fireEvent.click(removeBtn())
    expect(screen.getByRole("button", { name: t("settings.sync.advanced.removeConfirm") })).toBeTruthy()
    current = joined({ devices: [SELF, { ...OTHER, removed: true }] })
    await pushRefresh()
    await waitFor(() => expect(screen.queryByRole("button", { name: t("settings.sync.advanced.removeConfirm") })).toBeNull())
    expect(removeBtn().disabled).toBe(true)
    expect(calls("sync_remove_device")).toHaveLength(0)
  })

  /* ── U5: restored counts ── */

  const RESTORED: RestoredSummary[] = [{ kind: "memo", count: 2 }, { kind: "tr", count: 1 }]

  it("D-U5a restored counts are one stable banner across polls (same node, no re-entry) and follow count changes", async () => {
    current = joined({ restored: RESTORED })
    await renderSection()
    const node = li("restored")!
    expect(node.textContent).toContain(restoredText(t, RESTORED))
    expect(restoredText(t, RESTORED)).toContain("3")
    const before = calls("sync_get_status").length
    current = joined({ restored: RESTORED.map((r) => ({ ...r })) })
    await pushRefresh()
    await waitFor(() => expect(calls("sync_get_status").length).toBe(before + 1))
    expect(li("restored")).toBe(node)
    const next: RestoredSummary[] = [{ kind: "memo", count: 4 }]
    current = joined({ restored: next })
    await pushRefresh()
    await waitFor(() => expect(li("restored")!.textContent).toContain(restoredText(t, next)))
    expect(li("restored")).toBe(node)
    expect(document.querySelectorAll('li[data-notice="restored"]')).toHaveLength(1)
  })

  it("D-U5b dismiss sends sync_dismiss_notice('restored') once; it stays gone across polls until Rust reports a new restore", async () => {
    current = joined({ restored: RESTORED })
    handlers.sync_dismiss_notice = () => {
      current = joined()
      return true
    }
    await renderSection()
    const btn = within(li("restored")!).getByRole("button", { name: t("settings.sync.notice.action.dismiss") })
    fireEvent.click(btn)
    fireEvent.click(btn)
    await waitFor(() => expect(li("restored")).toBeNull())
    expect(calls("sync_dismiss_notice").map(([, a]) => a)).toEqual([{ notice: "restored" }])
    await pushRefresh()
    await act(async () => {})
    expect(li("restored")).toBeNull()
    current = joined({ restored: [{ kind: "conv", count: 1 }] })
    await pushRefresh()
    await waitFor(() => expect(li("restored")).not.toBeNull())
  })

  it("D-U5c a poll issued before the dismiss and landing after it does not bring the restored banner back", async () => {
    current = joined({ restored: RESTORED })
    await renderSection()
    const slow = deferred<SyncStatus>()
    const stale = current
    statusOverride = () => slow.promise
    await pushRefresh()
    statusOverride = null
    handlers.sync_dismiss_notice = () => {
      current = joined()
      return true
    }
    fireEvent.click(within(li("restored")!).getByRole("button", { name: t("settings.sync.notice.action.dismiss") }))
    await waitFor(() => expect(li("restored")).toBeNull())
    await act(async () => {
      slow.resolve(stale)
      await slow.promise
    })
    expect(li("restored")).toBeNull()
  })

  it("D-U5d a rejected restored dismiss keeps the banner and shows the error", async () => {
    current = joined({ restored: RESTORED })
    handlers.sync_dismiss_notice = () => {
      throw { code: "transient", message: "" }
    }
    await renderSection()
    fireEvent.click(within(li("restored")!).getByRole("button", { name: t("settings.sync.notice.action.dismiss") }))
    await screen.findByText(errorText(t, "transient"))
    expect(li("restored")).not.toBeNull()
  })

  /* ── U6: in-flight guards, ordering, duplicates ── */

  it("D-U6a join: a double click on Preview sends one discovery and one preview", async () => {
    current = connected({ needs_vault: true, discovery: "found" })
    handlers.sync_discover = () => ({ state: "found", vaults: 1 })
    const preview = deferred<unknown>()
    handlers.sync_join_preview = () => preview.promise
    await renderSection()
    await waitFor(() => expect(calls("sync_discover")).toHaveLength(1))
    fireEvent.click(screen.getByRole("button", { name: t("settings.sync.vault.join") }))
    const dialog = await screen.findByRole("dialog")
    fireEvent.change(within(dialog).getByLabelText(t("settings.sync.rk.entry")), { target: { value: RK } })
    const btn = within(dialog).getByRole("button", { name: t("settings.sync.join.preview") })
    fireEvent.click(btn)
    fireEvent.click(btn)
    await waitFor(() => expect(calls("sync_join_preview")).toHaveLength(1))
    preview.resolve({ vault: "9f8e7d6c", summary: SUMMARY })
    await within(dialog).findByText(skipLine)
    expect(order()).toEqual(["sync_discover", "sync_discover", "sync_join_preview"])
  })

  it("D-U6b reissue: one sync_reissue_rk per open, and a double click on Confirm sends one sync_confirm_rk", async () => {
    current = joined({ devices: [SELF] })
    const reissue = deferred<unknown>()
    handlers.sync_reissue_rk = () => reissue.promise
    const confirm = deferred<unknown>()
    handlers.sync_confirm_rk = () => confirm.promise
    await renderSection()
    openAdvanced()
    fireEvent.click(screen.getByRole("button", { name: t("settings.sync.advanced.reissue") }))
    await screen.findByRole("dialog")
    expect(calls("sync_reissue_rk")).toHaveLength(1)
    await act(async () => {
      reissue.resolve({ result: "show_key", recovery_key: RK })
      await reissue.promise
    })
    await screen.findByTestId("rk-value")
    fireEvent.click(screen.getByRole("button", { name: t("settings.sync.rk.next") }))
    fireEvent.change(screen.getByLabelText(t("settings.sync.rk.entry")), { target: { value: RK } })
    const btn = screen.getByRole("button", { name: t("settings.sync.rk.confirm") })
    fireEvent.click(btn)
    fireEvent.click(btn)
    expect(calls("sync_confirm_rk")).toHaveLength(1)
    confirm.resolve({ result: "reissued", not_revocation: true })
    await screen.findByText(t("settings.sync.rk.reissued"))
    expect(calls("sync_reissue_rk")).toHaveLength(1)
  })

  it("D-U6c rejoin: a double click sends one sync_rejoin", async () => {
    current = joined({ notices: [{ notice: "device_removed" }] })
    const rejoin = deferred<unknown>()
    handlers.sync_rejoin = () => rejoin.promise
    await renderSection()
    fireEvent.click(within(li("device_removed")!).getByRole("button", { name: t("settings.sync.notice.action.rejoin") }))
    const dialog = await screen.findByRole("dialog")
    fireEvent.change(within(dialog).getByLabelText(t("settings.sync.rk.entry")), { target: { value: RK } })
    const btn = within(dialog).getByRole("button", { name: t("settings.sync.notice.action.rejoin") })
    fireEvent.click(btn)
    fireEvent.click(btn)
    expect(calls("sync_rejoin")).toHaveLength(1)
    rejoin.resolve({ vault: "9f8e7d6c" })
    await within(dialog).findByText(t("settings.sync.rejoin.done"))
  })

  it("D-U6d out of order: the newest status wins; an older success or error landing after it is ignored", async () => {
    current = joined({ notices: [{ notice: "rate_limited" }] })
    await renderSection()
    const a = deferred<SyncStatus>()
    const b = deferred<SyncStatus>()
    const queue = [a, b]
    statusOverride = () => queue.shift()!.promise
    await pushRefresh()
    await pushRefresh()
    statusOverride = null
    await act(async () => {
      b.resolve(joined())
      await b.promise
    })
    await waitFor(() => expect(li("rate_limited")).toBeNull())
    await act(async () => {
      a.resolve(joined({ notices: [{ notice: "rate_limited" }, { notice: "fork_detected" }] }))
      await a.promise
    })
    expect(li("rate_limited")).toBeNull()
    expect(li("fork_detected")).toBeNull()
    // An older rejection after a newer success shows no error.
    const c = deferred<SyncStatus>()
    const d = deferred<SyncStatus>()
    const q2 = [c, d]
    statusOverride = () => q2.shift()!.promise
    await pushRefresh()
    await pushRefresh()
    statusOverride = null
    await act(async () => {
      d.resolve(joined({ notices: [{ notice: "quota_exceeded" }] }))
      await d.promise
    })
    await waitFor(() => expect(li("quota_exceeded")).not.toBeNull())
    await act(async () => {
      c.reject({ code: "store_error", message: "" })
      await c.promise.catch(() => {})
    })
    expect(li("quota_exceeded")).not.toBeNull()
    expect(screen.queryByText(errorText(t, "store_error"))).toBeNull()
  })

  it("D-U6e older first, newest last: the end state is the newest response", async () => {
    current = joined()
    await renderSection()
    const a = deferred<SyncStatus>()
    const b = deferred<SyncStatus>()
    const queue = [a, b]
    statusOverride = () => queue.shift()!.promise
    await pushRefresh()
    await pushRefresh()
    statusOverride = null
    await act(async () => {
      a.resolve(joined({ notices: [{ notice: "fork_detected" }] }))
      await a.promise
    })
    // Observation: an older response that lands while the newest is still
    // pending is applied (seq > last applied); it is replaced below.
    await act(async () => {
      b.resolve(joined({ notices: [{ notice: "quota_exceeded" }] }))
      await b.promise
    })
    await waitFor(() => expect(li("quota_exceeded")).not.toBeNull())
    expect(li("fork_detected")).toBeNull()
  })

  it("D-U6f duplicate and unknown notices: one banner per tag (last listed), localized unknown copy, no raw tags, no duplicate keys", async () => {
    const err = vi.spyOn(console, "error").mockImplementation(() => {})
    const unknown = (tag: string, extra: Record<string, unknown> = {}) => ({ notice: tag, ...extra }) as unknown as EngineNotice
    current = joined({
      notices: [
        { notice: "other_vaults", count: 1 },
        { notice: "rate_limited" },
        { notice: "other_vaults", count: 3 },
        { notice: "rate_limited" },
        unknown("future_thing", { n: 1 }),
        unknown("future_thing", { n: 2 }),
        unknown("future_other"),
      ],
    })
    handlers.sync_dismiss_notice = () => true
    await renderSection()
    const dupKey = err.mock.calls.some((c) => c.map(String).join(" ").includes("same key"))
    err.mockRestore()
    expect(dupKey).toBe(false)
    expect(document.querySelectorAll('li[data-notice="other_vaults"]')).toHaveLength(1)
    expect(li("other_vaults")!.textContent).toContain(noticeText(t, { notice: "other_vaults", count: 3 }))
    expect(document.querySelectorAll('li[data-notice="rate_limited"]')).toHaveLength(1)
    expect(document.querySelectorAll('li[data-notice="future_thing"]')).toHaveLength(1)
    expect(document.querySelectorAll('li[data-notice="future_other"]')).toHaveLength(1)
    expect(li("future_thing")!.textContent).toContain(t("settings.sync.notice.unknown"))
    expect(document.body.textContent).not.toMatch(/future_|other_vaults|rate_limited/)
    fireEvent.click(within(li("other_vaults")!).getByRole("button", { name: t("settings.sync.notice.action.later") }))
    await waitFor(() => expect(calls("sync_dismiss_notice")).toHaveLength(1))
    expect(calls("sync_dismiss_notice")[0][1]).toEqual({ notice: "other_vaults" })
  })

  /* ── Adjacent (A6 / RK-6): the app mounts under StrictMode (src/main.tsx) ── */

  // Reproduced: RkDialog's one-shot effect (`begun` ref, deps [mode]) sets
  // `cancelled` in the StrictMode cleanup and the re-run returns early, so
  // the create / reissue result is dropped and the dialog stays on
  // "Working..." in dev builds (`tauri dev`). Production does not re-run
  // effects. Rust already holds the pending vault, so each retry hangs too.
  it("G9 blocker: D-S1 under StrictMode the create dialog still shows the RK after one sync_create_vault", async () => {
    current = connected({ needs_vault: true, discovery: "nothing_found" })
    handlers.sync_discover = () => ({ state: "none", vaults: 0 })
    handlers.sync_create_vault = () => ({ vault: "9f8e7d6c", recovery_key: RK })
    render(
      <StrictMode>
        <SyncSection uiLocale="en" />
      </StrictMode>,
    )
    fireEvent.click(await screen.findByRole("button", { name: t("settings.sync.vault.create") }))
    await screen.findByRole("dialog")
    await waitFor(() => expect(screen.queryByTestId("rk-value")).not.toBeNull(), { timeout: 1500 })
    expect(calls("sync_create_vault")).toHaveLength(1)
  })
})
