import { useEffect, useRef } from "react"

import { matchesCombo, shortcut } from "@/lib/shortcuts"

/** Return false to let the key through (nothing to act on right now). */
export type ShortcutHandler = (e: KeyboardEvent) => boolean | void

/**
 * Bind in-app shortcuts by id while the calling component is mounted. Only
 * the active panel is mounted, so tab-specific ids never fire in another tab.
 * A key a focused field already handled (defaultPrevented) is left alone.
 */
export function useShortcuts(bindings: Record<string, ShortcutHandler | undefined>): void {
  const ref = useRef(bindings)
  useEffect(() => {
    ref.current = bindings
  })
  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      if (e.defaultPrevented || e.repeat || e.isComposing) return
      for (const [id, handler] of Object.entries(ref.current)) {
        if (!handler) continue
        const def = shortcut(id)
        if (def.local || def.group === "global") continue
        if (!matchesCombo(def.combo, e)) continue
        if (handler(e) === false) return
        e.preventDefault()
        return
      }
    }
    window.addEventListener("keydown", onKey)
    return () => window.removeEventListener("keydown", onKey)
  }, [])
}
