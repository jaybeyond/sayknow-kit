/**
 * Replacement for `Bun.sleep`, which the ported flows use between polls.
 *
 * Unlike `Bun.sleep` this also wakes on the controller's abort signal, so
 * cancelling a device-code or project-provisioning wait takes effect
 * immediately instead of after the full interval.
 */
export function delay(ms: number, signal?: AbortSignal): Promise<void> {
  return new Promise((resolve) => {
    if (signal?.aborted) {
      resolve()
      return
    }
    const timer = setTimeout(() => {
      signal?.removeEventListener("abort", onAbort)
      resolve()
    }, ms)
    function onAbort() {
      clearTimeout(timer)
      resolve()
    }
    signal?.addEventListener("abort", onAbort, { once: true })
  })
}
