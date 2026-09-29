// Runs with the app's own `vitest run`, so the site cannot fall out of step
// with the app it describes.
import { describe, expect, it } from "vitest"
import { readFileSync } from "node:fs"
import { fileURLToPath } from "node:url"

import { CHANGELOG } from "./changelog"
import { issueFormUrl } from "./feedback"
import { pickLocale } from "./pick-locale"
import { assetNames, formatBytes, releaseFromApi } from "./release"
import { FEATURE_IDS, LOCALES } from "./site"
import { COPY } from "../i18n"
import type { Copy } from "../i18n"

const appVersion = JSON.parse(readFileSync(fileURLToPath(new URL("../../../package.json", import.meta.url)), "utf8")).version

describe("changelog", () => {
  it("says what the version being shipped changed", () => {
    expect(CHANGELOG[0].version).toBe(appVersion)
  })

  it("is newest first, dated, and written in every language", () => {
    const key = (v: string) => v.split(".").map((n) => n.padStart(4, "0")).join(".")
    for (let i = 1; i < CHANGELOG.length; i++) {
      expect(key(CHANGELOG[i - 1].version) > key(CHANGELOG[i].version), CHANGELOG[i].version).toBe(true)
      expect(CHANGELOG[i - 1].date >= CHANGELOG[i].date).toBe(true)
    }
    for (const entry of CHANGELOG) {
      expect(entry.date).toMatch(/^\d{4}-\d{2}-\d{2}$/)
      const counts = LOCALES.map((l) => entry.notes[l]?.length ?? 0)
      // Every language says the same number of things about a release.
      expect(new Set(counts).size, `${entry.version}: ${counts.join(",")}`).toBe(1)
      expect(counts[0]).toBeGreaterThan(0)
      for (const l of LOCALES) for (const note of entry.notes[l]) expect(note.trim(), `${entry.version}/${l}`).not.toBe("")
    }
  })
})

/** Every string leaf with its path, so two locales can be compared shape for shape. */
function leaves(value: unknown, path = ""): [string, string][] {
  if (typeof value === "string") return [[path, value]]
  if (Array.isArray(value)) return value.flatMap((v, i) => leaves(v, `${path}[${i}]`))
  if (value && typeof value === "object") return Object.entries(value).flatMap(([k, v]) => leaves(v, path ? `${path}.${k}` : k))
  return []
}

describe("site copy", () => {
  const reference = leaves(COPY.en).map(([p]) => p)

  it("has the same fields, lists of the same length, in every language", () => {
    for (const locale of LOCALES) {
      expect(leaves(COPY[locale]).map(([p]) => p), locale).toEqual(reference)
    }
  })

  it("leaves nothing blank and keeps the date slot", () => {
    for (const locale of LOCALES) {
      for (const [path, text] of leaves(COPY[locale])) expect(text.trim(), `${locale}:${path}`).not.toBe("")
      expect(COPY[locale].home.released, locale).toContain("{date}")
    }
  })

  it("describes every feature page", () => {
    for (const locale of LOCALES) {
      const items: Copy["features"]["items"] = COPY[locale].features.items
      expect(Object.keys(items).sort(), locale).toEqual([...FEATURE_IDS].sort())
    }
  })

  it("keeps commands out of the translations; the page renders them as code", () => {
    for (const locale of LOCALES) {
      for (const [path, text] of leaves(COPY[locale].download)) {
        expect(text, `${locale}:${path}`).not.toMatch(/xattr|shasum|certutil/)
      }
    }
  })
})

describe("latest release", () => {
  const api = {
    tag_name: "v0.3.7",
    published_at: "2026-09-28T15:33:20Z",
    html_url: "https://github.com/jaybeyond/sayknow-kit/releases/tag/v0.3.7",
    draft: false,
    prerelease: false,
    assets: [
      { name: "SayKnow-Kit_0.3.7_aarch64.dmg", browser_download_url: "https://example.test/mac.dmg", size: 10_600_000 },
      { name: "SHA256SUMS.txt", browser_download_url: "https://example.test/sums", size: 412 },
    ],
  }

  it("reads the version, date and the published files", () => {
    const r = releaseFromApi(api)!
    expect(r.version).toBe("0.3.7")
    expect(r.date).toBe("2026-09-28")
    expect(r.mac).toEqual({ name: "SayKnow-Kit_0.3.7_aarch64.dmg", url: "https://example.test/mac.dmg", bytes: 10_600_000 })
    // A file the API did not list still gets the release's own download URL.
    expect(r.winExe.url).toBe(
      "https://github.com/jaybeyond/sayknow-kit/releases/download/v0.3.7/SayKnow-Kit_0.3.7_x64-setup.exe",
    )
    expect(r.winExe.bytes).toBeNull()
  })

  it("never points the site at a draft, a prerelease or an odd tag", () => {
    expect(releaseFromApi({ ...api, draft: true })).toBeNull()
    expect(releaseFromApi({ ...api, prerelease: true })).toBeNull()
    expect(releaseFromApi({ ...api, tag_name: "nightly" })).toBeNull()
    expect(releaseFromApi(null)).toBeNull()
  })

  it("names files the way the release workflow publishes them", () => {
    const workflow = readFileSync(fileURLToPath(new URL("../../../.github/workflows/release.yml", import.meta.url)), "utf8")
    const names = assetNames("0.0.0")
    for (const name of [names.mac, names.winExe, names.winMsi]) {
      const published = name.replace("0.0.0", "${process.env.VERSION}")
      expect(workflow, name).toContain(`\`${published}\``)
    }
  })

  it("shows sizes people can read", () => {
    expect(formatBytes(null)).toBe("")
    expect(formatBytes(412)).toBe("1 KB")
    expect(formatBytes(10_600_000)).toBe("10.6 MB")
  })
})

describe("feedback links", () => {
  it("open the matching issue form, with the version filled in for bugs", () => {
    const bug = new URL(issueFormUrl("bug", "0.3.7"))
    expect(bug.pathname).toBe("/jaybeyond/sayknow-kit/issues/new")
    expect(bug.searchParams.get("template")).toBe("bug_report.yml")
    expect(bug.searchParams.get("version")).toBe("0.3.7")
    const idea = new URL(issueFormUrl("idea", "0.3.7"))
    expect(idea.searchParams.get("template")).toBe("feature_request.yml")
    expect(idea.searchParams.has("version")).toBe(false)
  })

  it("point at forms that exist, and the bug form has the version field they fill", () => {
    const dir = new URL("../../../.github/ISSUE_TEMPLATE/", import.meta.url)
    // A Windows checkout turns these files into CRLF, so match either ending.
    const bug = readFileSync(fileURLToPath(new URL("bug_report.yml", dir)), "utf8")
    readFileSync(fileURLToPath(new URL("feature_request.yml", dir)), "utf8")
    expect(bug).toMatch(/\r?\n\s+id: version\r?\n/)
  })
})

describe("language choice", () => {
  const supported = [...LOCALES]
  it("follows the browser, falling back to English", () => {
    expect(pickLocale(["ja-JP", "en"], supported)).toBe("ja")
    expect(pickLocale(["zh-TW"], supported)).toBe("zh")
    expect(pickLocale(["pt-BR", "de-AT"], supported)).toBe("de")
    expect(pickLocale(["pt-BR"], supported)).toBe("en")
    expect(pickLocale([], supported)).toBe("en")
  })
})
