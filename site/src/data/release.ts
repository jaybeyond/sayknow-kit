// The release the download buttons point at. Read from GitHub at build time;
// the pages also re-check in the browser (see scripts in Base.astro), so a
// release published after the last deploy is still what people download.
import { RELEASES_URL, REPO } from "./site"

export type Asset = { name: string; url: string; bytes: number | null }

export type Release = {
  version: string
  /** YYYY-MM-DD */
  date: string
  url: string
  mac: Asset
  winExe: Asset
  winMsi: Asset
  sums: Asset
}

/** The installer names the release workflow publishes for a version. */
export function assetNames(version: string) {
  return {
    mac: `SayKnow-Kit_${version}_aarch64.dmg`,
    winExe: `SayKnow-Kit_${version}_x64-setup.exe`,
    winMsi: `SayKnow-Kit_${version}_x64_en-US.msi`,
    sums: "SHA256SUMS.txt",
  }
}

function downloadUrl(version: string, name: string): string {
  return `${RELEASES_URL}/download/v${version}/${name}`
}

/** A release built from its version alone, for when GitHub cannot be asked. */
export function releaseFor(version: string, date: string): Release {
  const names = assetNames(version)
  const asset = (name: string): Asset => ({ name, url: downloadUrl(version, name), bytes: null })
  return {
    version,
    date,
    url: `${RELEASES_URL}/tag/v${version}`,
    mac: asset(names.mac),
    winExe: asset(names.winExe),
    winMsi: asset(names.winMsi),
    sums: asset(names.sums),
  }
}

type ApiRelease = {
  tag_name?: unknown
  published_at?: unknown
  html_url?: unknown
  draft?: unknown
  prerelease?: unknown
  assets?: { name?: unknown; browser_download_url?: unknown; size?: unknown }[]
}

/** The GitHub API's latest release, or null when it is not a usable one. */
export function releaseFromApi(json: unknown): Release | null {
  const r = json as ApiRelease
  if (!r || typeof r.tag_name !== "string" || r.draft === true || r.prerelease === true) return null
  const version = r.tag_name.replace(/^v/, "")
  if (!/^\d+\.\d+\.\d+$/.test(version)) return null
  const date = typeof r.published_at === "string" ? r.published_at.slice(0, 10) : ""
  const base = releaseFor(version, date)
  const assets = Array.isArray(r.assets) ? r.assets : []
  const pick = (fallback: Asset): Asset => {
    const hit = assets.find((a) => a.name === fallback.name)
    if (!hit || typeof hit.browser_download_url !== "string") return fallback
    return { name: fallback.name, url: hit.browser_download_url, bytes: typeof hit.size === "number" ? hit.size : null }
  }
  return {
    ...base,
    url: typeof r.html_url === "string" ? r.html_url : base.url,
    mac: pick(base.mac),
    winExe: pick(base.winExe),
    winMsi: pick(base.winMsi),
    sums: pick(base.sums),
  }
}

/**
 * Latest release at build time. A build must not fail because GitHub is slow
 * or rate-limited, so it falls back to the version the changelog says was
 * shipped last, whose files follow the same naming.
 */
export async function latestRelease(fallback: { version: string; date: string }): Promise<Release> {
  const headers: Record<string, string> = { Accept: "application/vnd.github+json", "User-Agent": "sayknow-kit-site" }
  const token = process.env.GITHUB_TOKEN
  if (token) headers.Authorization = `Bearer ${token}`
  try {
    const res = await fetch(`https://api.github.com/repos/${REPO}/releases/latest`, {
      headers,
      signal: AbortSignal.timeout(8000),
    })
    if (res.ok) {
      const release = releaseFromApi(await res.json())
      if (release) return release
    }
  } catch {
    // Fall through to the changelog's version.
  }
  return releaseFor(fallback.version, fallback.date)
}

export function formatBytes(bytes: number | null): string {
  if (bytes === null) return ""
  if (bytes < 1_000_000) return `${Math.max(1, Math.round(bytes / 1_000))} KB`
  return `${(bytes / 1_000_000).toFixed(1)} MB`
}

/** Commands the download page shows verbatim; never translated. */
export const COMMANDS = {
  quarantine: 'xattr -dr com.apple.quarantine "/Applications/SayKnow Kit.app"',
  shasum: "shasum -a 256 SayKnow-Kit_<version>_aarch64.dmg",
  certutil: "certutil -hashfile SayKnow-Kit_<version>_x64-setup.exe SHA256",
}
