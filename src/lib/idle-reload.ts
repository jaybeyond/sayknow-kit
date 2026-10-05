import { useEffect, useRef } from "react"

/**
 * A popover that has been used keeps every panel it rendered, plus the
 * JavaScript compiled for them, alive in the WebContent process for as long as
 * the app runs. Measured on a three-day-old instance: about 255 MB across the
 * app and its WebKit helpers, against 100 MB right after launch. Reloading the
 * page once it has stayed hidden for a while hands that back; reopening then
 * loads the bundled assets from disk again.
 *
 * Work that a reload would cut short (a translation or chat answer in flight,
 * a memo being edited, a cleanup run) holds the reload off until it settles.
 * Text that is merely sitting in a field is carried across the reload in
 * sessionStorage instead, so an abandoned draft cannot pin the memory forever.
 */
export const HIDDEN_RELOAD_AFTER_MS = 10 * 60 * 1000
/** How soon to look again when something was holding the reload off. */
export const HELD_RETRY_MS = 60 * 1000

const holds = new Set<symbol>()

/** Keep the page from being reloaded until the returned release is called. */
export function holdReload(): () => void {
  const hold = Symbol("reload-hold")
  holds.add(hold)
  return () => {
    holds.delete(hold)
  }
}

/** Holds the reload off for as long as `active` is true. */
export function useReloadHold(active: boolean) {
  useEffect(() => {
    if (!active) return
    return holdReload()
  }, [active])
}

export function reloadHeld(): boolean {
  return holds.size > 0
}

const savers = new Set<() => void>()

/** Runs `save` right before the idle reload. */
export function useBeforeReload(save: () => void) {
  const latest = useRef(save)
  useEffect(() => {
    latest.current = save
  })
  useEffect(() => {
    const run = () => latest.current()
    savers.add(run)
    return () => {
      savers.delete(run)
    }
  }, [])
}

/** Indirection so tests can observe the reload without navigating jsdom. */
export const page = {
  reload: () => window.location.reload(),
}

export function reloadNow() {
  for (const save of savers) {
    try {
      save()
    } catch {
      // A panel that cannot save must not keep the memory pinned.
    }
  }
  page.reload()
}

const CARRY_PREFIX = "sayknow:carry:"

/** Stores `value` for the page that loads after the idle reload. */
export function carry(key: string, value: unknown) {
  try {
    sessionStorage.setItem(CARRY_PREFIX + key, JSON.stringify(value))
  } catch {
    // Quota or a disabled store: the draft is lost, nothing else is.
  }
}

/** Takes back what `carry` stored before the reload, once. */
export function takeCarried<T>(key: string): T | null {
  try {
    const raw = sessionStorage.getItem(CARRY_PREFIX + key)
    if (raw === null) return null
    sessionStorage.removeItem(CARRY_PREFIX + key)
    return JSON.parse(raw) as T
  } catch {
    return null
  }
}

export function resetReloadHoldsForTests() {
  holds.clear()
  savers.clear()
}