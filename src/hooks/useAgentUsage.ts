import { useCallback, useEffect, useSyncExternalStore } from "react"
import { isTauri } from "@/lib/runtime"
import {
  getSnapshot,
  scanAgentUsage,
  subscribe,
} from "@/lib/agent-usage-store"

export function useAgentUsage(active: boolean, deeplKey = "") {
  // Whether we're in the desktop shell is constant for the window's lifetime.
  const supported = isTauri()
  const state = useSyncExternalStore(subscribe, getSnapshot)

  // Pure subscription: the scan writes to the module store, and this component
  // re-renders through useSyncExternalStore rather than a setState in here.
  // `active` turns true whenever the popover is shown again or the tab is
  // opened; that moment always reads the logs again, because the user is
  // looking at it now. Focus and the timer only fill the gaps in between.
  useEffect(() => {
    if (!active || !supported) return
    void scanAgentUsage(true, deeplKey)
    const onFocus = () => void scanAgentUsage(false, deeplKey)
    window.addEventListener("focus", onFocus)
    const interval = window.setInterval(() => void scanAgentUsage(false, deeplKey), 60_000)
    return () => {
      window.removeEventListener("focus", onFocus)
      window.clearInterval(interval)
    }
  }, [active, supported, deeplKey])

  const refresh = useCallback(
    (force = true) => scanAgentUsage(force, deeplKey),
    [deeplKey],
  )

  return { ...state, supported, refresh }
}
