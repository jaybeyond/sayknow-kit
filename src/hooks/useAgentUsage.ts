import { useEffect, useSyncExternalStore } from "react"
import { isTauri } from "@/lib/runtime"
import {
  getSnapshot,
  scanAgentUsage,
  subscribe,
} from "@/lib/agent-usage-store"

export function useAgentUsage(active: boolean) {
  // Whether we're in the desktop shell is constant for the window's lifetime.
  const supported = isTauri()
  const state = useSyncExternalStore(subscribe, getSnapshot)

  // Pure subscription: the scan writes to the module store, and this component
  // re-renders through useSyncExternalStore rather than a setState in here.
  useEffect(() => {
    if (!active || !supported) return
    void scanAgentUsage()
    const onFocus = () => void scanAgentUsage()
    window.addEventListener("focus", onFocus)
    return () => window.removeEventListener("focus", onFocus)
  }, [active, supported])

  return { ...state, supported, refresh: scanAgentUsage }
}
