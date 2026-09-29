// Keeps the download links current between deploys. The page ships with the
// release that existed at build time; if GitHub already has a newer one, the
// version, date and file links are swapped for it. Anything that fails leaves
// the built values in place, which still point at a real release.
import { releaseFromApi, type Release } from "../data/release"
import { REPO } from "../data/site"

const CACHE_KEY = "sayknow-kit:latest-release"
const CACHE_MS = 10 * 60 * 1000

function newer(a: string, b: string): boolean {
  const pa = a.split(".").map(Number)
  const pb = b.split(".").map(Number)
  for (let i = 0; i < 3; i++) {
    if ((pa[i] ?? 0) !== (pb[i] ?? 0)) return (pa[i] ?? 0) > (pb[i] ?? 0)
  }
  return false
}

async function fetchLatest(): Promise<Release | null> {
  try {
    const cached = sessionStorage.getItem(CACHE_KEY)
    if (cached) {
      const { at, release } = JSON.parse(cached) as { at: number; release: Release }
      if (Date.now() - at < CACHE_MS) return release
    }
  } catch {
    // Storage unavailable: just ask.
  }
  try {
    const res = await fetch(`https://api.github.com/repos/${REPO}/releases/latest`, {
      headers: { Accept: "application/vnd.github+json" },
    })
    if (!res.ok) return null
    const release = releaseFromApi(await res.json())
    if (release) {
      try {
        sessionStorage.setItem(CACHE_KEY, JSON.stringify({ at: Date.now(), release }))
      } catch {
        // Not cached; fine.
      }
    }
    return release
  } catch {
    return null
  }
}

function formatDate(iso: string, lang: string): string {
  const d = new Date(`${iso}T00:00:00Z`)
  if (Number.isNaN(d.getTime())) return iso
  return new Intl.DateTimeFormat(lang, { year: "numeric", month: "long", day: "numeric", timeZone: "UTC" }).format(d)
}

async function refresh() {
  const root = document.querySelector<HTMLElement>("[data-release]")
  if (!root) return
  const built = root.dataset.release ?? ""
  const latest = await fetchLatest()
  if (!latest || !newer(latest.version, built)) return
  const lang = document.documentElement.lang
  for (const el of document.querySelectorAll<HTMLElement>("[data-release-version]")) el.textContent = `v${latest.version}`
  for (const el of document.querySelectorAll<HTMLElement>("[data-release-date]")) {
    const template = el.dataset.releaseDate ?? "{date}"
    el.textContent = template.replace("{date}", formatDate(latest.date, lang))
  }
  const assets = { mac: latest.mac, winExe: latest.winExe, winMsi: latest.winMsi, sums: latest.sums }
  for (const el of document.querySelectorAll<HTMLAnchorElement>("[data-asset]")) {
    const asset = assets[el.dataset.asset as keyof typeof assets]
    if (!asset) continue
    el.href = asset.url
    const name = el.querySelector("[data-asset-name]")
    if (name) name.textContent = asset.name
  }
  for (const el of document.querySelectorAll<HTMLAnchorElement>("[data-release-url]")) el.href = latest.url
  root.dataset.release = latest.version
}

/** Put the visitor's own platform first, without hiding the other. */
function preferPlatform() {
  const ua = navigator.userAgent
  const os = /Windows/i.test(ua) ? "win" : /Mac/i.test(ua) ? "mac" : null
  if (!os) return
  for (const group of document.querySelectorAll<HTMLElement>("[data-platform-group]")) {
    const preferred = group.querySelector<HTMLElement>(`[data-platform="${os}"]`)
    if (!preferred) continue
    group.prepend(preferred)
    for (const btn of group.querySelectorAll<HTMLElement>("[data-platform]")) {
      btn.classList.toggle("primary", btn === preferred)
    }
  }
}

preferPlatform()
void refresh()
