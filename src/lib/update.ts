/**
 * Release check against the repository's own GitHub releases.
 *
 * The app ships ad-hoc signed (macOS) and unsigned (Windows) installers, so it
 * deliberately does not replace itself in the background: it reports that a
 * newer release exists and sends the user to the official release page, where
 * `SHA256SUMS.txt` is published beside the installers.
 */
import { httpFetch } from "./http"
import { isTauri } from "./runtime"
import { storage } from "./storage"

const LATEST_API = "https://api.github.com/repos/jaybeyond/sayknow-kit/releases/latest"
export const RELEASES_PAGE = "https://github.com/jaybeyond/sayknow-kit/releases/latest"
const CACHE_KEY = "update.lastCheck"
const REQUEST_TIMEOUT_MS = 10_000
/** A release check is never urgent; once a day is enough to stay informed. */
export const CHECK_INTERVAL_MS = 24 * 60 * 60 * 1000

export type UpdateStatus =
  | { state: "idle" }
  | { state: "checking" }
  | { state: "current"; current: string }
  | { state: "outdated"; current: string; latest: string; url: string }
  | { state: "failed"; reason: string }

type CachedCheck = { checkedAt: number; latest: string; url: string }

export type LatestRelease = { version: string; url: string }

/** `v0.2.28` and `0.2.28` both parse; a build suffix is ignored, not guessed. */
export function parseVersion(value: string): [number, number, number] | null {
  const match = value.trim().match(/^v?(\d+)\.(\d+)\.(\d+)/)
  if (!match) return null
  return [Number(match[1]), Number(match[2]), Number(match[3])]
}

export function isNewer(latest: string, current: string): boolean {
  const a = parseVersion(latest)
  const b = parseVersion(current)
  if (!a || !b) return false
  for (let i = 0; i < 3; i++) {
    if (a[i] !== b[i]) return a[i] > b[i]
  }
  return false
}

/** The running app's own version; the web preview has none to compare. */
export async function appVersion(): Promise<string> {
  if (!isTauri()) return ""
  const { getVersion } = await import("@tauri-apps/api/app")
  return getVersion()
}

export async function fetchLatestRelease(): Promise<LatestRelease> {
  const controller = new AbortController()
  const timer = setTimeout(() => controller.abort(), REQUEST_TIMEOUT_MS)
  try {
    const response = await httpFetch(LATEST_API, {
      headers: { Accept: "application/vnd.github+json" },
      signal: controller.signal,
    })
    if (!response.ok) throw new Error(`github_status_${response.status}`)
    const body = (await response.json()) as {
      tag_name?: unknown
      html_url?: unknown
      draft?: unknown
      prerelease?: unknown
    }
    // The endpoint already excludes drafts and pre-releases; refusing them here
    // as well keeps an unstable build from ever being advertised as an update.
    if (body.draft === true || body.prerelease === true) throw new Error("unstable_release")
    const tag = typeof body.tag_name === "string" ? body.tag_name : ""
    if (!parseVersion(tag)) throw new Error("unreadable_release_tag")
    const url = typeof body.html_url === "string" ? body.html_url : RELEASES_PAGE
    return { version: tag.replace(/^v/, ""), url }
  } finally {
    clearTimeout(timer)
  }
}

let state: UpdateStatus = { state: "idle" }
let inFlight: Promise<UpdateStatus> | null = null
const listeners = new Set<() => void>()

function set(next: UpdateStatus) {
  state = next
  for (const listener of listeners) listener()
}

export function subscribe(listener: () => void) {
  listeners.add(listener)
  return () => {
    listeners.delete(listener)
  }
}

export function getSnapshot(): UpdateStatus {
  return state
}

function settle(current: string, latest: string, url: string): UpdateStatus {
  return isNewer(latest, current)
    ? { state: "outdated", current, latest, url }
    : { state: "current", current }
}

/**
 * @param force skips the once-a-day throttle for an explicit user request.
 */
export function checkForUpdate(
  current: string,
  options: { force?: boolean; now?: number } = {},
): Promise<UpdateStatus> {
  if (inFlight) return inFlight
  const now = options.now ?? Date.now()
  const cached = storage.get<CachedCheck>(CACHE_KEY)
  if (
    !options.force &&
    cached &&
    typeof cached.checkedAt === "number" &&
    typeof cached.latest === "string" &&
    now - cached.checkedAt < CHECK_INTERVAL_MS
  ) {
    const resolved = settle(current, cached.latest, cached.url || RELEASES_PAGE)
    set(resolved)
    return Promise.resolve(resolved)
  }

  set({ state: "checking" })
  const run = fetchLatestRelease().then(
    ({ version, url }) => {
      storage.set(CACHE_KEY, { checkedAt: now, latest: version, url })
      const resolved = settle(current, version, url)
      set(resolved)
      inFlight = null
      return resolved
    },
    (error: unknown) => {
      // A failed check is not an app failure: keep the last known answer out of
      // the way and let the next call try again immediately.
      const resolved: UpdateStatus = {
        state: "failed",
        reason: error instanceof Error ? error.message : String(error),
      }
      set(resolved)
      inFlight = null
      return resolved
    },
  )
  inFlight = run
  return run
}

/**
 * Throttled check that resolves the running version itself, so any surface —
 * signed in or not — can start one without knowing the version.
 */
export async function refreshUpdateStatus(force = false): Promise<void> {
  const current = await appVersion()
  if (!current) return
  await checkForUpdate(current, { force })
}

export type InstallProgress =
  | { phase: "downloading"; downloaded: number; total: number | null }
  | { phase: "installing" }

/**
 * Downloads and installs the update the endpoint advertises, then relaunches.
 *
 * The payload is verified against the minisign public key compiled into the
 * app before anything is written, so this path does not inherit the installers'
 * ad-hoc/unsigned posture. A machine that cannot verify simply fails here and
 * the release page stays the way out.
 */
export async function installUpdate(
  onProgress?: (progress: InstallProgress) => void,
): Promise<void> {
  const { check } = await import("@tauri-apps/plugin-updater")
  const update = await check()
  if (!update) throw new Error("no_signed_update_available")

  let downloaded = 0
  let total: number | null = null
  await update.downloadAndInstall((event) => {
    if (event.event === "Started") {
      total = event.data.contentLength ?? null
      onProgress?.({ phase: "downloading", downloaded: 0, total })
    } else if (event.event === "Progress") {
      downloaded += event.data.chunkLength
      onProgress?.({ phase: "downloading", downloaded, total })
    } else if (event.event === "Finished") {
      onProgress?.({ phase: "installing" })
    }
  })

  const { relaunch } = await import("@tauri-apps/plugin-process")
  await relaunch()
}
