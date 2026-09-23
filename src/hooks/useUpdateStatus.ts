import { useCallback, useEffect, useSyncExternalStore } from "react"
import {
  getSnapshot,
  refreshUpdateStatus,
  subscribe,
  type UpdateStatus,
} from "@/lib/update"

/**
 * Shared release-check state. Mounting starts a throttled check, so reaching
 * any surface that shows it — the popover, the sign-in screen, or the About
 * panel — is enough to learn about a new release; the button forces one.
 */
export function useUpdateStatus(): {
  status: UpdateStatus
  check: (force?: boolean) => Promise<void>
} {
  const status = useSyncExternalStore(subscribe, getSnapshot)

  const check = useCallback((force = false) => refreshUpdateStatus(force), [])

  useEffect(() => {
    void check()
  }, [check])

  return { status, check }
}
