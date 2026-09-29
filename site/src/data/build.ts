// Values every page computes the same way at build time.
import { LATEST_ENTRY } from "./changelog"
import { latestRelease, type Release } from "./release"
import { LOCALES, type Locale } from "./site"
import { HTML_LANG } from "./site"

let cached: Promise<Release> | null = null

/** One GitHub request per build, however many pages ask. */
export function buildRelease(): Promise<Release> {
  cached ??= latestRelease({ version: LATEST_ENTRY.version, date: LATEST_ENTRY.date })
  return cached
}

export function localeParams() {
  return LOCALES.map((lang) => ({ params: { lang } }))
}

export function formatDate(iso: string, locale: Locale): string {
  const d = new Date(`${iso}T00:00:00Z`)
  if (Number.isNaN(d.getTime())) return iso
  return new Intl.DateTimeFormat(HTML_LANG[locale], { year: "numeric", month: "long", day: "numeric", timeZone: "UTC" }).format(d)
}

/** Anchor id for a version on the changelog page. */
export function versionAnchor(version: string): string {
  return `v${version.replaceAll(".", "-")}`
}
