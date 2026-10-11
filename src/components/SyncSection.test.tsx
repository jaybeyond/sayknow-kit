/** @vitest-environment jsdom */
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest"
import { act, cleanup, fireEvent, render, screen, waitFor, within } from "@testing-library/react"

const mocks = vi.hoisted(() => ({
  invoke: vi.fn<(cmd: string, args?: Record<string, unknown>) => Promise<unknown>>(),
  writeText: vi.fn<(text: string) => Promise<void>>(),
  /** `@/lib/sync-client` `syncState()`: the migration client's local state (C-B2). */
  client: {
    state: { mode: "native", migration: null, finalize: null, error: null, corruptQueue: null, writeFailures: 0 } as Record<string, unknown>,
    subs: new Set<() => void>(),
  },
}))

vi.mock("@tauri-apps/api/core", () => ({ invoke: mocks.invoke }))
vi.mock("@tauri-apps/api/event", () => ({ listen: vi.fn(async () => () => {}) }))
vi.mock("@/lib/runtime", () => ({ isTauri: () => true, openExternal: vi.fn() }))
vi.mock("@/lib/sync-client", () => ({
  subscribe: (fn: () => void) => {
    mocks.client.subs.add(fn)
    return () => mocks.client.subs.delete(fn)
  },
  syncState: () => mocks.client.state,
}))

import { SyncSection } from "./SyncSection"
import { UI_STRINGS } from "@/i18n/strings"
import { noticeText } from "@/lib/sync-copy"
import { NOTICE_TAGS, WIPE_CONFIRMATION, type EngineNotice, type SyncStatus } from "@/lib/sync-api"

const en = UI_STRINGS.en
const t = (key: string) => en[key] ?? key

const RK = "ABCD-EFGH-IJKL-MNOP-QRST-UVWX-YZ23-4567-ABCD-EFGH-IJKL-MNOP-QRST-UVWX"

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

/** Routes `invoke` by command; `sync_get_status` answers from `current`. */
let current: SyncStatus
const handlers: Record<string, (args?: Record<string, unknown>) => unknown> = {}
function route() {
  mocks.invoke.mockImplementation(async (cmd, args) => {
    if (cmd === "sync_get_status") return current
    const h = handlers[cmd]
    if (!h) throw new Error("unmocked " + cmd)
    return h(args)
  })
}
const calls = (cmd: string) => mocks.invoke.mock.calls.filter(([c]) => c === cmd)

async function renderSection() {
  const out = render(<SyncSection uiLocale="en" />)
  await waitFor(() => expect(screen.queryByText(t("settings.sync.loading"))).toBeNull())
  return out
}

/** Replaces the mocked client state and notifies subscribers, like `setState` in sync-client. */
function setClientState(patch: Record<string, unknown>) {
  mocks.client.state = { ...mocks.client.state, ...patch }
  for (const fn of mocks.client.subs) fn()
}

beforeEach(() => {
  for (const k of Object.keys(handlers)) delete handlers[k]
  current = status()
  route()
  mocks.client.state = { mode: "native", migration: null, finalize: null, error: null, corruptQueue: null, writeFailures: 0 }
  Object.defineProperty(navigator, "clipboard", { value: { writeText: mocks.writeText }, configurable: true })
  mocks.writeText.mockResolvedValue()
})

afterEach(() => {
  cleanup()
  vi.clearAllMocks()
  vi.useRealTimers()
})

describe("SyncSection states", () => {
  it("off: toggle plus what is and is not synced; turning on calls sync_set_enabled(true)", async () => {
    handlers.sync_set_enabled = () => connected()
    await renderSection()
    expect(screen.getByText(t("settings.sync.intro"))).toBeTruthy()
    expect(screen.getByText(t("settings.sync.syncedList"))).toBeTruthy()
    expect(screen.getByText(t("settings.sync.neverSyncedList"))).toBeTruthy()
    fireEvent.click(screen.getByRole("switch", { name: t("settings.sync.toggle") }))
    await waitFor(() => expect(calls("sync_set_enabled")).toHaveLength(1))
    expect(calls("sync_set_enabled")[0][1]).toEqual({ enabled: true })
  })

  it("connect: four provider cards, Drive preselected, keyed inputs cleared after submit", async () => {
    current = status({ enabled: true })
    handlers.sync_connect_start = () => ({ provider: "webdav", account_label: "u@h" })
    await renderSection()
    const radios = screen.getAllByRole("radio")
    expect(radios).toHaveLength(4)
    expect(radios[0]).toHaveProperty("ariaChecked", "true")
    fireEvent.click(screen.getByRole("radio", { name: /WebDAV/ }))
    const pw = screen.getByLabelText(t("settings.sync.connect.field.password")) as HTMLInputElement
    fireEvent.change(screen.getByLabelText(t("settings.sync.connect.field.base_url")), { target: { value: "https://dav.example/" } })
    fireEvent.change(screen.getByLabelText(t("settings.sync.connect.field.username")), { target: { value: "u" } })
    fireEvent.change(pw, { target: { value: "s3cret" } })
    fireEvent.click(screen.getByRole("button", { name: t("settings.sync.connect.connect") }))
    await waitFor(() => expect(calls("sync_connect_start")).toHaveLength(1))
    expect(calls("sync_connect_start")[0][1]).toEqual({
      provider: "webdav",
      params: { base_url: "https://dav.example/", username: "u", password: "s3cret" },
    })
    // The secret left component state with the call.
    expect((screen.queryByLabelText(t("settings.sync.connect.field.password")) as HTMLInputElement | null)?.value ?? "").toBe("")
    expect(document.body.textContent).not.toContain("s3cret")
  })

  it("connect: OAuth shows progress with Cancel wired to sync_connect_cancel", async () => {
    current = status({ enabled: true })
    let finish!: (v: unknown) => void
    handlers.sync_connect_start = () => new Promise((r) => (finish = r))
    handlers.sync_connect_cancel = () => undefined
    await renderSection()
    fireEvent.click(screen.getByRole("button", { name: t("settings.sync.connect.signIn") }))
    await screen.findByText(t("settings.sync.connect.waiting"))
    fireEvent.click(screen.getByRole("button", { name: t("common.cancel") }))
    expect(calls("sync_connect_cancel")).toHaveLength(1)
    await act(async () => finish({ provider: "gdrive", account_label: "x" }))
  })

  it("needs vault: discovers on arrival and offers Create when nothing is found", async () => {
    current = connected({ needs_vault: true })
    handlers.sync_discover = () => {
      current = connected({ needs_vault: true, discovery: "nothing_found" })
      return { state: "none", vaults: 0 }
    }
    await renderSection()
    await waitFor(() => expect(calls("sync_discover")).toHaveLength(1))
    await screen.findByText(t("settings.sync.vault.none"))
    expect(screen.getByRole("button", { name: t("settings.sync.vault.create") })).toBeTruthy()
    expect(screen.queryByRole("button", { name: t("settings.sync.vault.join") })).toBeNull()
  })

  it("needs vault: offers Join when a vault is found", async () => {
    current = connected({ needs_vault: true })
    handlers.sync_discover = () => {
      current = connected({ needs_vault: true, discovery: "found" })
      return { state: "found", vaults: 2 }
    }
    await renderSection()
    await screen.findByText("Found 2 vault(s). Join with a recovery key, or create a new one.")
    expect(screen.getByRole("button", { name: t("settings.sync.vault.join") })).toBeTruthy()
  })

  it("A9: a retry state re-discovers automatically when discovery_retry_at arrives", async () => {
    vi.useFakeTimers()
    const retryAt = Date.now() + 3000
    current = connected({ needs_vault: true, discovery: "retry", discovery_retry_at: retryAt })
    handlers.sync_discover = () => ({ state: "retry", vaults: 0 })
    render(<SyncSection uiLocale="en" />)
    await act(async () => {
      await vi.advanceTimersByTimeAsync(10)
    })
    // The arrival discover.
    expect(calls("sync_discover")).toHaveLength(1)
    expect(screen.getByText(/Checking again in \d s\./)).toBeTruthy()
    await act(async () => {
      await vi.advanceTimersByTimeAsync(3100)
    })
    expect(calls("sync_discover")).toHaveLength(2)
  })

  it("create flow: shows the RK once, blocks Next→Confirm until re-entry, clears it on close", async () => {
    current = connected({ needs_vault: true, discovery: "nothing_found" })
    handlers.sync_discover = () => ({ state: "none", vaults: 0 })
    handlers.sync_create_vault = () => ({ vault: "9f8e7d6c", recovery_key: RK })
    handlers.sync_confirm_rk = (args) => {
      if (args?.rk !== RK) throw { code: "confirmation_mismatch", message: "" }
      return { result: "created", vault: "9f8e7d6c" }
    }
    await renderSection()
    fireEvent.click(await screen.findByRole("button", { name: t("settings.sync.vault.create") }))
    const shown = await screen.findByTestId("rk-value")
    expect(shown.textContent).toBe(RK)
    expect(shown.className).toContain("font-mono")
    expect(screen.getByText(t("settings.sync.rk.offline"))).toBeTruthy()
    fireEvent.click(screen.getByRole("button", { name: t("settings.sync.rk.copy") }))
    await waitFor(() => expect(mocks.writeText).toHaveBeenCalledWith(RK))

    fireEvent.click(screen.getByRole("button", { name: t("settings.sync.rk.next") }))
    const entry = screen.getByLabelText(t("settings.sync.rk.entry"))
    const confirm = screen.getByRole("button", { name: t("settings.sync.rk.confirm") }) as HTMLButtonElement
    expect(confirm.disabled).toBe(true)
    // The RK is no longer rendered once the user is asked to re-enter it.
    expect(screen.queryByTestId("rk-value")).toBeNull()

    fireEvent.change(entry, { target: { value: "WRONG-KEY" } })
    fireEvent.click(confirm)
    await screen.findByText(t("settings.sync.rk.mismatch"))
    expect(screen.queryByText(t("settings.sync.rk.created"))).toBeNull()
    expect((entry as HTMLInputElement).value).toBe("")

    fireEvent.change(entry, { target: { value: RK } })
    fireEvent.click(screen.getByRole("button", { name: t("settings.sync.rk.confirm") }))
    await screen.findByText(t("settings.sync.rk.created"))
    expect(calls("sync_confirm_rk").map(([, a]) => a)).toEqual([{ rk: "WRONG-KEY" }, { rk: RK }])

    fireEvent.click(screen.getByRole("button", { name: t("common.close") }))
    await waitFor(() => expect(screen.queryByRole("dialog")).toBeNull())
    expect(document.body.innerHTML).not.toContain(RK)
  })

  it("create flow: closing the RK dialog without confirming leaves no RK in the DOM", async () => {
    current = connected({ needs_vault: true, discovery: "nothing_found" })
    handlers.sync_discover = () => ({ state: "none", vaults: 0 })
    handlers.sync_create_vault = () => ({ vault: "9f8e7d6c", recovery_key: RK })
    await renderSection()
    fireEvent.click(await screen.findByRole("button", { name: t("settings.sync.vault.create") }))
    await screen.findByTestId("rk-value")
    fireEvent.click(screen.getByRole("button", { name: t("common.cancel") }))
    await waitFor(() => expect(screen.queryByRole("dialog")).toBeNull())
    expect(document.body.innerHTML).not.toContain(RK)
    expect(document.body.innerHTML).not.toContain("ABCD-EFGH")
  })

  it("create pending: explains the RK cannot be re-shown and offers Finish setup", async () => {
    current = connected({ needs_vault: true, create_pending: true })
    await renderSection()
    expect(screen.getByText(t("settings.sync.vault.createPending"))).toBeTruthy()
    expect(screen.getByRole("button", { name: t("settings.sync.vault.finish") })).toBeTruthy()
    expect(calls("sync_discover")).toHaveLength(0)
  })

  it("join flow: preview shows the counts and commit is called only after confirm", async () => {
    current = connected({ needs_vault: true, discovery: "found" })
    handlers.sync_discover = () => ({ state: "found", vaults: 1 })
    handlers.sync_join_preview = () => ({ vault: "9f8e7d6c", summary: { skip: 3, republish: 4, copy: 1, evicted: 2, tombstones: 0 } })
    handlers.sync_join_commit = () => ({ result: "joined", vault: "9f8e7d6c", ops: 4 })
    await renderSection()
    fireEvent.click(await screen.findByRole("button", { name: t("settings.sync.vault.join") }))
    fireEvent.change(screen.getByLabelText(t("settings.sync.rk.entry")), { target: { value: RK } })
    fireEvent.click(screen.getByRole("button", { name: t("settings.sync.join.preview") }))
    await screen.findByText("3 item(s) already in the vault stay as they are.")
    expect(screen.getByText("4 item(s) from this device are uploaded to the vault.")).toBeTruthy()
    expect(screen.getByText("1 item(s) are kept as copies.")).toBeTruthy()
    expect(screen.getByText("2 attachment(s) must be downloaded again from the vault.")).toBeTruthy()
    expect(calls("sync_join_preview")[0][1]).toEqual({ rk: RK })
    expect(calls("sync_join_commit")).toHaveLength(0)
    fireEvent.click(screen.getByRole("button", { name: t("settings.sync.join.confirm") }))
    await screen.findByText("Joined. 4 item(s) applied.")
    expect(calls("sync_join_commit")).toHaveLength(1)
    expect(document.body.innerHTML).not.toContain(RK)
  })

  it("join flow: a wrong RK shows the D-1 line", async () => {
    current = connected({ needs_vault: true, discovery: "found" })
    handlers.sync_discover = () => ({ state: "found", vaults: 1 })
    handlers.sync_join_preview = () => {
      throw { code: "rk_wrong", message: "" }
    }
    await renderSection()
    fireEvent.click(await screen.findByRole("button", { name: t("settings.sync.vault.join") }))
    fireEvent.change(screen.getByLabelText(t("settings.sync.rk.entry")), { target: { value: "nope" } })
    fireEvent.click(screen.getByRole("button", { name: t("settings.sync.join.preview") }))
    await screen.findByText("This key doesn't open any vault in this storage.")
  })

  it("status panel: account, vault tag, relative last cycle, plain-words status, counters, moving migration, wipe started", async () => {
    current = joined({
      stats: { ...status().stats, uploads: 12, downloads: 7, cycles: 40, quarantined: 1 },
      migration: "imported",
      migration_state: "imported",
      wipe: "started",
      last_status: { status: "rate_limited", retry_after: null },
    })
    await renderSection()
    expect(screen.getByText("Google Drive · gdrive 1a2b3c4d")).toBeTruthy()
    expect(screen.getByText("9f8e7d6c")).toBeTruthy()
    expect(screen.getByText(/2 minutes ago · Storage rate limit, retrying shortly/)).toBeTruthy()
    expect(screen.getByText("Uploaded 12 · downloaded 7 · cycles 40 · quarantined 1")).toBeTruthy()
    expect(screen.getByText(t("settings.sync.migration.moving"))).toBeTruthy()
    expect(screen.getByText(t("settings.sync.wipe.started"))).toBeTruthy()
  })

  it("status panel: Deferred migration from the client's finalize result, and wipe done", async () => {
    mocks.client.state = { ...mocks.client.state, finalize: "deferred" }
    current = joined({ migration: "imported", migration_state: "imported", wipe: "done" })
    await renderSection()
    expect(screen.getByText(t("settings.sync.migration.deferred"))).toBeTruthy()
    expect(screen.queryByText(t("settings.sync.migration.moving"))).toBeNull()
    expect(screen.getByText(t("settings.sync.wipe.done"))).toBeTruthy()
  })

  it("status panel: the client's local errors map to refused-write (with count), marker and generic copy", async () => {
    current = joined()
    await renderSection()
    const alerts = () => screen.queryAllByRole("alert").map((a) => a.textContent)
    expect(alerts()).toEqual([])

    await act(async () => setClientState({ error: "queue_write_failed", writeFailures: 2 }))
    expect(alerts()).toEqual(["2 change(s) could not be saved safely and were rolled back. Check free disk space."])

    await act(async () => setClientState({ error: "marker_write_failed", writeFailures: 0 }))
    expect(alerts()).toEqual([t("settings.sync.local.marker")])

    await act(async () => setClientState({ error: "queue_corrupt", corruptQueue: "k1" }))
    expect(alerts()).toEqual([t("settings.sync.local.generic")])

    // The finish_push failure code also reads the generic line.
    await act(async () => setClientState({ error: "finish_push_failed", corruptQueue: null }))
    expect(alerts()).toEqual([t("settings.sync.local.generic")])

    // Refused writes without an error code still show the count.
    await act(async () => setClientState({ error: null, writeFailures: 3 }))
    expect(alerts()).toEqual(["3 change(s) could not be saved safely and were rolled back. Check free disk space."])

    await act(async () => setClientState({ error: null, writeFailures: 0 }))
    expect(alerts()).toEqual([])
  })

  it("status panel: diagnostics export goes through the clipboard and is content-free", async () => {
    current = joined()
    handlers.sync_export_diagnostics = () => "enabled=on migration=native"
    await renderSection()
    fireEvent.click(screen.getByRole("button", { name: t("settings.sync.status.export") }))
    await waitFor(() => expect(mocks.writeText).toHaveBeenCalledWith("enabled=on migration=native"))
    await screen.findByText(t("settings.sync.status.exported"))
  })

  it("store_error from the status is surfaced", async () => {
    current = joined({ store_error: true })
    await renderSection()
    expect(screen.getByText(t("settings.sync.error.store_error"))).toBeTruthy()
  })

  it("two-vault banner offers join / keep / later; keep dismisses the notice; join runs discovery before the preview (U2)", async () => {
    current = joined({ notices: [{ notice: "other_vaults", count: 1 }] })
    handlers.sync_dismiss_notice = () => true
    // Rust unlocks only among the last discovery; without it the preview fails.
    let discovered = false
    handlers.sync_discover = () => {
      discovered = true
      return { state: "found", vaults: 2 }
    }
    handlers.sync_join_preview = () => {
      if (!discovered) throw { code: "discovery_required", message: "" }
      return { vault: "9f8e7d6c", summary: { skip: 1, republish: 0, copy: 0, evicted: 0, tombstones: 0 } }
    }
    await renderSection()
    const item = screen.getByText("This storage holds 1 other vault(s). Choose which one to use.").closest("li")!
    expect(within(item).getByRole("button", { name: t("settings.sync.notice.action.joinOther") })).toBeTruthy()
    expect(within(item).getByRole("button", { name: t("settings.sync.notice.action.later") })).toBeTruthy()
    fireEvent.click(within(item).getByRole("button", { name: t("settings.sync.notice.action.keepCurrent") }))
    await waitFor(() => expect(calls("sync_dismiss_notice")).toHaveLength(1))
    expect(calls("sync_dismiss_notice")[0][1]).toEqual({ notice: "other_vaults" })
    expect(calls("sync_discover")).toHaveLength(0)
    fireEvent.click(within(item).getByRole("button", { name: t("settings.sync.notice.action.joinOther") }))
    const dialog = await screen.findByRole("dialog")
    expect(screen.getByText(t("settings.sync.join.title"))).toBeTruthy()
    fireEvent.change(within(dialog).getByLabelText(t("settings.sync.rk.entry")), { target: { value: RK } })
    fireEvent.click(within(dialog).getByRole("button", { name: t("settings.sync.join.preview") }))
    await screen.findByText("1 item(s) already in the vault stay as they are.")
    expect(mocks.invoke.mock.calls.filter(([c]) => c === "sync_discover" || c === "sync_join_preview").map(([c]) => c)).toEqual([
      "sync_discover",
      "sync_join_preview",
    ])
    expect(screen.queryByText(t("settings.sync.error.discovery_required"))).toBeNull()
  })

  it("join: a discovery_required answer re-runs discovery once, then shows the result as it is (U2)", async () => {
    current = connected({ needs_vault: true, discovery: "found" })
    // Discovery results expire between the arrival discover and the preview.
    let previews = 0
    handlers.sync_discover = () => ({ state: "found", vaults: 1 })
    handlers.sync_join_preview = () => {
      previews += 1
      throw { code: previews === 1 ? "discovery_required" : "rk_wrong", message: "" }
    }
    await renderSection()
    fireEvent.click(await screen.findByRole("button", { name: t("settings.sync.vault.join") }))
    fireEvent.change(screen.getByLabelText(t("settings.sync.rk.entry")), { target: { value: RK } })
    fireEvent.click(screen.getByRole("button", { name: t("settings.sync.join.preview") }))
    await screen.findByText(t("settings.sync.join.wrongKey"))
    // Arrival discover, pre-preview discover, one retry: three in all, two previews.
    expect(calls("sync_discover")).toHaveLength(3)
    expect(calls("sync_join_preview")).toHaveLength(2)
  })

  it("restored notice (U5): one line with the total, a breakdown per kind, and a dismiss that sends 'restored'", async () => {
    current = joined({ restored: [{ kind: "memo", count: 2 }, { kind: "conv", count: 1 }] })
    handlers.sync_dismiss_notice = () => {
      current = joined()
      return true
    }
    await renderSection()
    const li = document.querySelector('li[data-notice="restored"]') as HTMLElement
    expect(li).not.toBeNull()
    expect(li.textContent).toContain("3 item(s) that conflicted with a delete were kept. (memos 2 · AI chats 1)")
    fireEvent.click(within(li).getByRole("button", { name: t("settings.sync.notice.action.dismiss") }))
    await waitFor(() => expect(calls("sync_dismiss_notice")).toHaveLength(1))
    expect(calls("sync_dismiss_notice")[0][1]).toEqual({ notice: "restored" })
    await waitFor(() => expect(document.querySelector('li[data-notice="restored"]')).toBeNull())
  })

  it("restored notice (U5): a single kind shows the plain line; an unknown kind reads 'items'", async () => {
    current = joined({ restored: [{ kind: "tr", count: 1 }] })
    await renderSection()
    const single = document.querySelector('li[data-notice="restored"]')!.textContent!
    expect(single).toContain("1 item(s) that conflicted with a delete were kept.")
    expect(single).not.toContain("·")
    cleanup()
    current = joined({ restored: [{ kind: "memo", count: 1 }, { kind: "brand_new", count: 4 }] })
    await renderSection()
    expect(document.querySelector('li[data-notice="restored"]')!.textContent).toContain("(memos 1 · items 4)")
    expect(document.body.textContent).not.toContain("brand_new")
  })

  it("every EngineNotice variant renders non-empty localized text with a dismiss action", async () => {
    // Variants with fields get a sample; the rest come from the exported tag list.
    const samples: Partial<Record<EngineNotice["notice"], EngineNotice>> = {
      other_vaults: { notice: "other_vaults", count: 2 },
      manifest_copies: { notice: "manifest_copies", count: 3 },
      rotated: { notice: "rotated", triggers: ["install_fp"] },
    }
    const all = NOTICE_TAGS.map((tag) => samples[tag] ?? ({ notice: tag } as EngineNotice))
    expect(new Set(NOTICE_TAGS).size).toBe(NOTICE_TAGS.length)
    for (const n of all) {
      const text = noticeText(t, n)
      expect(text.trim(), n.notice).toBeTruthy()
      expect(text, n.notice).not.toMatch(/settings\.sync|_/)
    }
    current = joined({ notices: all })
    await renderSection()
    for (const n of all) {
      const li = document.querySelector(`li[data-notice="${n.notice}"]`)!
      expect(li, n.notice).not.toBeNull()
      expect(li.textContent).toContain(noticeText(t, n))
      expect(within(li as HTMLElement).getAllByRole("button").length, n.notice).toBeGreaterThan(0)
    }
    expect(screen.getByRole("button", { name: t("settings.sync.notice.action.reconnect") })).toBeTruthy()
    expect(screen.getByRole("button", { name: t("settings.sync.notice.action.rejoin") })).toBeTruthy()
  })

  it("DeviceRemoved → Rejoin takes the RK to sync_rejoin", async () => {
    current = joined({ notices: [{ notice: "device_removed" }] })
    handlers.sync_rejoin = () => ({ vault: "9f8e7d6c" })
    await renderSection()
    fireEvent.click(screen.getByRole("button", { name: t("settings.sync.notice.action.rejoin") }))
    const dialog = await screen.findByRole("dialog")
    fireEvent.change(within(dialog).getByLabelText(t("settings.sync.rk.entry")), { target: { value: RK } })
    fireEvent.click(within(dialog).getByRole("button", { name: t("settings.sync.notice.action.rejoin") }))
    await screen.findByText(t("settings.sync.rejoin.done"))
    expect(calls("sync_rejoin")[0][1]).toEqual({ rk: RK })
    expect(document.body.innerHTML).not.toContain(RK)
  })

  it("reconsent notice reconnects an OAuth provider directly (U3)", async () => {
    current = joined({ provider: { provider: "onedrive", account_label: "onedrive 1a2b3c4d" }, notices: [{ notice: "reconsent_required" }] })
    handlers.sync_connect_start = () => ({ provider: "onedrive", account_label: "onedrive 1a2b3c4d" })
    await renderSection()
    fireEvent.click(screen.getByRole("button", { name: t("settings.sync.notice.action.reconnect") }))
    await waitFor(() => expect(calls("sync_connect_start")).toHaveLength(1))
    expect(calls("sync_connect_start")[0][1]).toEqual({ provider: "onedrive", params: null })
    expect(screen.queryByTestId("reconsent-form")).toBeNull()
  })

  it("reconsent for WebDAV opens its credential form pre-selected, never pre-filled; the secret leaves state with the call (U3)", async () => {
    current = joined({ provider: { provider: "webdav", account_label: "u@dav.example" }, notices: [{ notice: "reconsent_required" }] })
    handlers.sync_connect_start = () => {
      current = joined({ provider: { provider: "webdav", account_label: "u@dav.example" } })
      return { provider: "webdav", account_label: "u@dav.example" }
    }
    await renderSection()
    fireEvent.click(screen.getByRole("button", { name: t("settings.sync.notice.action.reconnect") }))
    // No connect call yet; the form opens instead.
    expect(calls("sync_connect_start")).toHaveLength(0)
    const form = screen.getByTestId("reconsent-form")
    expect(within(form).getByText(t("settings.sync.connect.reconsentTitle"))).toBeTruthy()
    expect(within(form).getByText("WebDAV")).toBeTruthy()
    // Fixed to the connected provider: no provider choice.
    expect(screen.queryByRole("radiogroup")).toBeNull()
    for (const f of ["base_url", "username", "password"]) {
      expect((within(form).getByLabelText(t(`settings.sync.connect.field.${f}`)) as HTMLInputElement).value, f).toBe("")
    }
    expect(within(form).queryByLabelText(t("settings.sync.connect.field.account_id"))).toBeNull()
    fireEvent.change(within(form).getByLabelText(t("settings.sync.connect.field.base_url")), { target: { value: "https://dav.example/" } })
    fireEvent.change(within(form).getByLabelText(t("settings.sync.connect.field.username")), { target: { value: "u" } })
    fireEvent.change(within(form).getByLabelText(t("settings.sync.connect.field.password")), { target: { value: "n3w-s3cret" } })
    fireEvent.click(within(form).getByRole("button", { name: t("settings.sync.connect.connect") }))
    await waitFor(() => expect(calls("sync_connect_start")).toHaveLength(1))
    expect(calls("sync_connect_start")[0][1]).toEqual({
      provider: "webdav",
      params: { base_url: "https://dav.example/", username: "u", password: "n3w-s3cret" },
    })
    expect(document.body.textContent).not.toContain("n3w-s3cret")
    // Success closes the form.
    await waitFor(() => expect(screen.queryByTestId("reconsent-form")).toBeNull())
  })

  it("reconsent for R2 asks for all four fields; a failed call keeps the form open and shows the code; Cancel closes it (U3)", async () => {
    current = joined({ provider: { provider: "r2", account_label: "bucket-a" }, notices: [{ notice: "reconsent_required" }] })
    handlers.sync_connect_start = () => {
      throw { code: "permission_denied", message: "raw s3cret" }
    }
    await renderSection()
    fireEvent.click(screen.getByRole("button", { name: t("settings.sync.notice.action.reconnect") }))
    const form = screen.getByTestId("reconsent-form")
    expect(within(form).getByText("Cloudflare R2")).toBeTruthy()
    for (const f of ["account_id", "bucket", "access_key_id", "secret_access_key"]) {
      fireEvent.change(within(form).getByLabelText(t(`settings.sync.connect.field.${f}`)), { target: { value: `v-${f}` } })
    }
    fireEvent.click(within(form).getByRole("button", { name: t("settings.sync.connect.connect") }))
    await screen.findByText(t("settings.sync.error.permission_denied"))
    expect(calls("sync_connect_start")[0][1]).toEqual({
      provider: "r2",
      params: { account_id: "v-account_id", bucket: "v-bucket", access_key_id: "v-access_key_id", secret_access_key: "v-secret_access_key" },
    })
    expect(document.body.textContent).not.toContain("v-secret_access_key")
    expect(document.body.textContent).not.toContain("raw s3cret")
    expect(screen.getByTestId("reconsent-form")).toBeTruthy()
    fireEvent.click(within(screen.getByTestId("reconsent-form")).getByRole("button", { name: t("common.cancel") }))
    expect(screen.queryByTestId("reconsent-form")).toBeNull()
  })

  it("advanced: the revocation limit sits next to Reissue and Remove device; reissue reuses the RK dialog and repeats the limit", async () => {
    current = joined()
    handlers.sync_reissue_rk = () => ({ result: "show_key", recovery_key: RK })
    await renderSection()
    expect(screen.queryByTestId("revocation-limit")).toBeNull()
    fireEvent.click(screen.getByRole("button", { name: t("settings.sync.advanced.title") }))
    const limit = screen.getByTestId("revocation-limit")
    expect(limit.textContent).toContain(t("settings.sync.advanced.revocationLimit"))
    const panel = limit.parentElement!
    expect(within(panel).getByRole("button", { name: t("settings.sync.advanced.reissue") })).toBeTruthy()
    expect(within(panel).getByRole("button", { name: t("settings.sync.advanced.removeDevice") })).toBeTruthy()

    fireEvent.click(within(panel).getByRole("button", { name: t("settings.sync.advanced.reissue") }))
    expect((await screen.findByTestId("rk-value")).textContent).toBe(RK)
    expect(screen.getByText(t("settings.sync.rk.reissueTitle"))).toBeTruthy()
    // AC 30: the limit is visible at the moment of the action.
    expect(screen.getByTestId("reissue-limit").textContent).toContain(t("settings.sync.advanced.revocationLimit"))
    fireEvent.click(screen.getByRole("button", { name: t("common.cancel") }))
    await waitFor(() => expect(screen.queryByRole("dialog")).toBeNull())
    expect(document.body.innerHTML).not.toContain(RK)
  })

  const DEVICES = [
    { id: "11111111-aaaa-4bbb-8ccc-000000000001", short: "1111aaaa", last_seen: Date.now() - 60_000, is_self: true, removed: false },
    { id: "22222222-aaaa-4bbb-8ccc-000000000002", short: "2222aaaa", last_seen: Date.now() - 3 * 86_400_000, is_self: false, removed: false },
    { id: "33333333-aaaa-4bbb-8ccc-000000000003", short: "3333aaaa", last_seen: null, is_self: false, removed: false },
    { id: "44444444-aaaa-4bbb-8ccc-000000000004", short: "4444aaaa", last_seen: Date.now() - 10 * 86_400_000, is_self: false, removed: true },
  ]

  it("advanced: remove device is a picker over status.devices; this device and removed ones cannot be picked (U4)", async () => {
    current = joined({ devices: DEVICES })
    handlers.sync_remove_device = () => undefined
    await renderSection()
    fireEvent.click(screen.getByRole("button", { name: t("settings.sync.advanced.title") }))
    // No free-text id anywhere.
    expect(screen.queryByRole("textbox")).toBeNull()
    const group = screen.getByRole("radiogroup", { name: t("settings.sync.advanced.devices") })
    const radios = within(group).getAllByRole("radio") as HTMLButtonElement[]
    expect(radios.map((r) => r.dataset.device)).toEqual(DEVICES.map((d) => d.id))
    // The UUIDs stay in data attributes; the visible text shows the short tags.
    expect(group.textContent).not.toContain(DEVICES[1].id)
    expect(group.textContent).toContain("2222aaaa")
    expect(radios[0].disabled).toBe(true)
    expect(radios[0].textContent).toContain(t("settings.sync.advanced.thisDevice"))
    expect(radios[0].textContent).toContain("1 minute ago")
    expect(radios[1].disabled).toBe(false)
    expect(radios[1].textContent).toContain("3 days ago")
    expect(radios[2].disabled).toBe(false)
    expect(radios[2].textContent).toContain(t("settings.sync.advanced.neverSeen"))
    expect(radios[3].disabled).toBe(true)
    expect(radios[3].textContent).toContain(t("settings.sync.advanced.removedDevice"))

    const remove = () => screen.getByRole("button", { name: t("settings.sync.advanced.removeDevice") }) as HTMLButtonElement
    expect(remove().disabled).toBe(true)
    fireEvent.click(radios[0])
    expect(remove().disabled).toBe(true)
    fireEvent.click(radios[1])
    expect(remove().disabled).toBe(false)
    fireEvent.click(remove())
    expect(calls("sync_remove_device")).toHaveLength(0)
    // AC 30 copy next to the confirm, naming the picked device.
    expect(screen.getByTestId("remove-limit").textContent).toContain("Removing 2222aaaa stops it from uploading new changes.")
    expect(screen.getByTestId("remove-limit").textContent).toContain("it can still read the vault")
    fireEvent.click(screen.getByRole("button", { name: t("settings.sync.advanced.removeConfirm") }))
    await waitFor(() => expect(calls("sync_remove_device")).toHaveLength(1))
    expect(calls("sync_remove_device")[0][1]).toEqual({ deviceId: DEVICES[1].id })
    // The pick is cleared after the call.
    expect(remove().disabled).toBe(true)
  })

  it("advanced: with no other device, Remove device explains that and stays disabled (U4)", async () => {
    current = joined({ devices: [] })
    await renderSection()
    fireEvent.click(screen.getByRole("button", { name: t("settings.sync.advanced.title") }))
    expect(screen.getByText(t("settings.sync.advanced.noDevices"))).toBeTruthy()
    expect(screen.queryByRole("radiogroup")).toBeNull()
    expect((screen.getByRole("button", { name: t("settings.sync.advanced.removeDevice") }) as HTMLButtonElement).disabled).toBe(true)
  })

  it("disconnect: plain disconnect passes an empty confirm; wipe is disabled until the exact word", async () => {
    current = joined()
    handlers.sync_disconnect = () => undefined
    await renderSection()
    fireEvent.click(screen.getByRole("button", { name: t("settings.sync.disconnect.title") }))
    const dialog = await screen.findByRole("dialog")
    const confirm = () => within(dialog).getByRole("button", { name: /Disconnect|Delete and disconnect/ }) as HTMLButtonElement
    expect(confirm().disabled).toBe(false)

    fireEvent.click(within(dialog).getByRole("checkbox"))
    expect(within(dialog).getByText(`To continue, type ${WIPE_CONFIRMATION} exactly.`)).toBeTruthy()
    expect(confirm().textContent).toBe(t("settings.sync.disconnect.confirmWipe"))
    expect(confirm().disabled).toBe(true)
    const word = within(dialog).getByLabelText(t("settings.sync.disconnect.wipeWord"))
    fireEvent.change(word, { target: { value: "delete" } })
    expect(confirm().disabled).toBe(true)
    fireEvent.change(word, { target: { value: "삭제 " } })
    expect(confirm().disabled).toBe(true)
    fireEvent.change(word, { target: { value: WIPE_CONFIRMATION } })
    expect(confirm().disabled).toBe(false)
    fireEvent.click(confirm())
    await waitFor(() => expect(calls("sync_disconnect")).toHaveLength(1))
    expect(calls("sync_disconnect")[0][1]).toEqual({ wipeCloud: true, confirm: "삭제" })
    await waitFor(() => expect(screen.queryByRole("dialog")).toBeNull())
  })

  it("disconnect without wipe sends wipeCloud=false and an empty word", async () => {
    current = joined()
    handlers.sync_disconnect = () => undefined
    await renderSection()
    fireEvent.click(screen.getByRole("button", { name: t("settings.sync.disconnect.title") }))
    const dialog = await screen.findByRole("dialog")
    fireEvent.click(within(dialog).getByRole("button", { name: t("settings.sync.disconnect.confirm") }))
    await waitFor(() => expect(calls("sync_disconnect")).toHaveLength(1))
    expect(calls("sync_disconnect")[0][1]).toEqual({ wipeCloud: false, confirm: "" })
  })

  it("a failed command shows localized copy for its code and generic copy otherwise", async () => {
    handlers.sync_set_enabled = () => {
      throw { code: "activation", message: "sync could not start" }
    }
    await renderSection()
    fireEvent.click(screen.getByRole("switch"))
    await screen.findByText(t("settings.sync.error.activation"))
    handlers.sync_set_enabled = () => {
      throw { code: "something_new", message: "" }
    }
    fireEvent.click(screen.getByRole("switch"))
    await screen.findByText(t("settings.sync.error.generic"))
    expect(document.body.textContent).not.toContain("something_new")
  })

  it("adds the section to the settings navigation", async () => {
    const { SettingsWindow } = await import("./SettingsWindow")
    window.history.replaceState(null, "", "/?section=sync")
    render(
      <SettingsWindow
        settings={{ uiLocale: "en", provider: "openrouter", glossary: [], systemAlerts: [], menuBarReadout: "off" } as never}
        update={vi.fn()}
        onLogout={vi.fn()}
        themeMode="system"
        setThemeMode={vi.fn()}
      />,
    )
    expect(screen.getAllByText(t("settings.section.sync")).length).toBeGreaterThan(0)
    window.history.replaceState(null, "", "/")
  })
})
