import { describe, expect, it } from "vitest"
import { parseAnalyze, parseCleanPreview, parseHistory, parseResult, parseSizeToBytes } from "./mole"

describe("mole parsers", () => {
  it("reads a disk analysis snapshot", () => {
    const analyze = parseAnalyze({
      path: "/",
      overview: true,
      total_size: 100,
      entries: [
        { name: "Home", path: "/Users/a", size: 75, is_dir: true },
        { name: "bad" },
      ],
    })
    expect(analyze?.entries).toHaveLength(1)
    expect(analyze?.entries?.[0].name).toBe("Home")
  })

  it("turns dry-run text into sized rows", () => {
    const items = parseCleanPreview(`
Clean Your Mac
➤ User essentials
  ✓ User app cache · 18 items, 2.4GB
  ✓ User app logs · 7 items, 12.8MB
  ◎ pnpm cache · skipped (pnpm busy)
`)
    expect(items.map((i) => i.name)).toEqual([
      "User app cache",
      "User app logs",
      "pnpm cache",
    ])
    expect(items[0].bytes).toBe(2_400_000_000)
    expect(items[2].skipped).toBe(true)
    expect(parseSizeToBytes("248.5MB")).toBe(248_500_000)
  })

  it("parses live mole 1.38 dry-run arrows", () => {
    const raw = `\u001B[0;33m→\u001B[0m User app cache 115 items\u001B[0m, \u001B[0;33m2.95GB dry\u001B[0m
  → Darwin user temp files, 362 old items, 315.9MB dry
  ✓ Trash · already empty
  ◎ System caches need sudo, run sudo -v && mo clean --dry-run for full preview
  ✓ Whitelist: 21 core patterns active
`
    const items = parseCleanPreview(raw)
    expect(items.find((i) => i.name.includes("User app cache"))?.bytes).toBe(2_950_000_000)
    expect(items.find((i) => i.name.includes("Trash"))?.skipped).toBe(true)
    expect(items.some((i) => /sudo|Whitelist/i.test(i.name))).toBe(false)
    const result = parseResult(raw)
    expect(result?.freedBytes).toBeGreaterThan(3_000_000_000)
    expect(result?.items).toBeGreaterThan(400)
  })

  it("reads history sessions", () => {
    const sessions = parseHistory({
      sessions: [
        { command: "clean", started_at: "2026-09-21", items: 12, size: "1.2GB", actions: { removed: 10, trashed: 2 } },
      ],
    })
    expect(sessions[0].removed).toBe(10)
  })
  it("reads a cleanup summary", () => {
    const result = parseResult(`
======================================================================
Cleanup complete
Tracked cleanup: 4.5GB | Items cleaned: 97 | Categories: 4
Free space: 223.5GB (+4.5GB)
======================================================================
`)
    expect(result?.heading).toMatch(/Cleanup complete/)
    expect(result?.freedBytes).toBe(4_500_000_000)
    expect(result?.items).toBe(97)
  })
})
