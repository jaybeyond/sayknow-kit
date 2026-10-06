import { beforeEach, describe, expect, it, vi } from "vitest"
import {
  cancelMoleAppRemoval, listMoleApps, parseAnalyze, parseCleanPreview, parseHistory,
  parseMaintenance, parseResult, parseSizeToBytes, previewMoleAppRemoval, stripAnsi,
  trashMoleAppSelection,
} from "./mole"
const invoke = vi.hoisted(() => vi.fn())
vi.mock("@tauri-apps/api/core", () => ({ invoke }))
beforeEach(() => invoke.mockReset())

const preview = `Clean Your Mac
Dry Run Mode, Preview only, no deletions
➤ User essentials
  → User app cache 18 items, 2.4GB dry
  → User app logs 7 items, 12.8MB dry
  → Trash · would empty, 1 items
➤ Developer tools
  ◎ Docker unused data · skipped by default
  ◎ Codex runtimes · manual review (9.68GB)
➤ Large files
  ◎ pnpm store (review only): 12.80GB, Path: /Users/test/Library/pnpm/store
➤ Project artifacts
  • 38+ candidates, at least 916.0MB sampled from 3 items
Potential space: 2.41GB | Items: 25 | Categories: 2
`

describe("Mole output semantics", () => {
  it("validates disk numeric fields without pretending invalid JSON is an empty scan", () => {
    expect(parseAnalyze({ wrong: [] })).toBeNull()
    const parsed = parseAnalyze({ entries: [
      { name: "Home", path: "/Users/a", size: 75, is_dir: true },
      { name: "empty", size: 0 }, { name: "negative", size: -1 },
      { name: "infinite", size: Infinity }, { name: "bad" },
    ], total_size: NaN })
    expect(parsed?.entries.map((entry) => entry.name)).toEqual(["Home", "empty"])
    expect(parsed?.total_size).toBeUndefined()
  })
  it("keeps categories, zero/unknown sizes, and manual/skipped rows distinct", () => {
    const items = parseCleanPreview(preview)
    expect(items[0]).toMatchObject({ section: "User essentials", name: "User app cache", bytes: 2_400_000_000, status: "candidate" })
    expect(items.find((item) => item.name === "Trash")).toMatchObject({ bytes: null, status: "candidate" })
    expect(items.find((item) => item.name === "Codex runtimes")?.status).toBe("manual")
    expect(items.find((item) => item.name === "Docker unused data")?.status).toBe("skipped")
    expect(items.find((item) => item.section === "Project artifacts")?.status).toBe("manual")
    expect(parseCleanPreview("➤ App caches\n→ Zero cache, 0B dry")[0].bytes).toBe(0)
  })
  it("keeps preview estimates separate from actual cleanup summaries", () => {
    expect(parseResult(preview, "preview")).toEqual({ mode: "preview", bytes: 2_410_000_000, items: 25, partial: false })
    expect(parseResult(preview, "clean")).toEqual({ mode: "clean", bytes: null, items: null, partial: false })
    const actual = "Cleanup complete\nSpace freed: 4.5GB | Items cleaned: 97 | Categories: 4\nFree space: 223.5GB (+4.5GB)"
    expect(parseResult(actual, "clean")).toEqual({ mode: "clean", bytes: 4_500_000_000, items: 97, partial: false })
    expect(parseResult("Tracked cleanup: 0B | Items cleaned: 0", "clean")?.bytes).toBe(0)
    expect(parseResult("Free space: 223.5GB", "clean")?.bytes).toBeNull()
  })
  it("never adds manual-only examples to eligible cleanup estimates", () => {
    const text = preview.replace(/Potential space:.*/, "")
    expect(parseResult(text, "preview")).toEqual({ mode: "preview", bytes: 2_412_800_000, items: null, partial: true })
    expect(parseResult("➤ Large files\n◎ store (review only): 200GB", "preview")).toBeNull()
  })
  it.each(["", " \n\t", "unrecognized output", "Potential space: unknown", "➤ App caches\n• arbitrary note, 9GB"])("does not recognize unknown preview output as a cleanup estimate (%j)", (text) => {
    expect(parseResult(text, "preview")).toBeNull()
    expect(parseCleanPreview(text).some((row) => row.status === "candidate")).toBe(false)
  })
  it("recognizes an explicit zero preview summary without candidate rows", () => {
    const text = "Potential space: 0B | Items: 0 | Categories: 0"
    expect(parseCleanPreview(text)).toEqual([])
    expect(parseResult(text, "preview")).toEqual({ mode: "preview", bytes: 0, items: 0, partial: false })
  })
  it("recognizes actual preview candidate rows with unknown sizes without estimating zero", () => {
    const text = "➤ Developer tools\n→ npm cache · would clean\n→ Homebrew · would cleanup and autoremove"
    expect(parseCleanPreview(text).map((row) => [row.status, row.bytes])).toEqual([["candidate", null], ["candidate", null]])
    expect(parseResult(text, "preview")).toEqual({ mode: "preview", bytes: null, items: null, partial: true })
  })
  it("strips terminal codes and distinguishes binary units", () => {
    expect(stripAnsi("\u001b[0;33m→\u001b[0m test\rnext")).toBe("→ test\nnext")
    expect(parseSizeToBytes("248.5MB")).toBe(248_500_000)
    expect(parseSizeToBytes("1GiB")).toBe(1_073_741_824)
    expect(parseSizeToBytes("unknown")).toBeNull()
    expect(parseSizeToBytes("999999999999999999999TB")).toBeNull()
  })
  it("does not count repeated progress rows or discard same-name rows in distinct categories", () => {
    const rows = parseCleanPreview("➤ First\n→ Cache, 1MB dry\n→ Cache, 2MB dry\n➤ Second\n→ Cache, 3MB dry")
    expect(rows.map((row) => row.bytes)).toEqual([2_000_000, 3_000_000])
  })
  it("does not convert optimistic maintenance dry-run language into completed work", () => {
    const text = "➤ Font Cache Rebuild\n→ Font cache cleared\n➤ Dock Refresh\n→ Dock refreshed"
    expect(parseMaintenance(text, true).map((task) => task.status)).toEqual(["preview", "preview"])
    const actual = "➤ Font Cache Rebuild\n◎ Font cache rebuild skipped · admin access required\n➤ Dock Refresh\n✓ Dock refreshed\n➤ Database Optimization\n→ All databases already optimized\n➤ New task\n→ Something unexpected\n➤ Broken Config Repair\n◎ Repair failed"
    expect(parseMaintenance(actual, false).map((task) => [task.id, task.status])).toEqual([
      ["fonts", "admin_skipped"], ["dock", "completed"], ["database", "unchanged"], ["New task", "unknown"], ["brokenConfig", "failed"],
    ])
  })
  it("excludes subtotals and unclassified rows from fallback estimates", () => {
    const text = "➤ First\n→ Cache, 1MB dry\n→ Subtotal: 8GB\n◎ Something unclear, 9GB\n→ Total: 12GB"
    expect(parseResult(text, "preview")?.bytes).toBe(1_000_000)
    expect(parseCleanPreview(text).find((row) => row.detail.includes("unclear"))?.status).toBe("unknown")
  })
  it("recognizes dry-run output even on an actual-action response", () => {
    for (const marker of ["DRY RUN", "Would apply 2 optimizations"]) {
      expect(parseMaintenance(`${marker}\n➤ Dock Refresh\n✓ Dock refreshed`, false)[0].status).toBe("preview")
    }
  })
  it("does not count already-completed or prospective maintenance as a new change", () => {
    const text = "➤ Dock Refresh\n→ Dock already refreshed\n➤ Database Optimization\n→ Database will be optimized\n➤ Login Items\n◎ Manual review needed\n➤ Network Cache Refresh\n✓ Cache flushed"
    expect(parseMaintenance(text, false).map((task) => task.status)).toEqual(["unchanged", "unknown", "manual", "completed"])
  })
  it("preserves the existing history reader", () => {
    expect(parseHistory({ sessions: [{ command: "clean", started_at: "2026-09-21", items: 12, size: "1.2GB", actions: { removed: 10, trashed: 2 } }] })[0].removed).toBe(10)
  })
})

describe("opaque app-removal command boundary", () => {
  it("sends identity/generation and candidate ids, never frontend-supplied paths", async () => {
    invoke.mockResolvedValue(undefined)
    await listMoleApps()
    await previewMoleAppRemoval("app-a", "generation-a")
    await trashMoleAppSelection("preview-a", ["candidate-a"])
    await cancelMoleAppRemoval("preview-a")
    expect(invoke.mock.calls).toEqual([
      ["list_mole_apps"],
      ["preview_mole_app_removal", { appId: "app-a", generation: "generation-a" }],
      ["trash_mole_app_selection", { previewToken: "preview-a", selectedCandidateIds: ["candidate-a"] }],
      ["cancel_mole_app_removal", { previewToken: "preview-a" }],
    ])
  })
})
