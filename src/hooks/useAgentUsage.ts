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
  useEffect(() => {
    if (!active || !supported) return
    void scanAgentUsage(false, deeplKey)
    const onFocus = () => void scanAgentUsage(false, deeplKey)
    window.addEventListener("focus", onFocus)
    return () => window.removeEventListener("focus", onFocus)
  }, [active, supported, deeplKey])

  const refresh = useCallback(
    (force = true) => scanAgentUsage(force, deeplKey),
    [deeplKey],
  )

  return { ...state, supported, refresh }
}
