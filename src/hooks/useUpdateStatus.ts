import { useCallback, useEffect, useSyncExternalStore } from "react"
import {
  appVersion,
  checkForUpdate,
  getSnapshot,
  subscribe,
  type UpdateStatus,
} from "@/lib/update"

/**
 * Shared release-check state. Mounting starts a throttled check, so opening the
 * popover is enough to learn about a new release; the explicit button forces one.
 */
export function useUpdateStatus(): {
  status: UpdateStatus
  check: (force?: boolean) => Promise<void>
} {
  const status = useSyncExternalStore(subscribe, getSnapshot)

  const check = useCallback(async (force = false) => {
    const current = await appVersion()
    if (!current) return
    await checkForUpdate(current, { force })
  }, [])

  useEffect(() => {
    void check()
  }, [check])

  return { status, check }
}
