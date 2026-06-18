import { useCallback, useEffect, useState } from "react"
import { listen } from "@tauri-apps/api/event"
import { clipboardHistory, type ClipEntry } from "@/lib/clipboard-history"
import { isTauri } from "@/lib/runtime"

export function useClipboardHistory() {
  const [entries, setEntries] = useState<ClipEntry[]>([])
  const [captureEnabled, setCaptureEnabledState] = useState(true)
  const [loaded, setLoaded] = useState(false)

  useEffect(() => {
    let cancelled = false
    void Promise.all([clipboardHistory.list(), clipboardHistory.getCapture()]).then(
      ([list, capture]) => {
        if (cancelled) return
        setEntries(list)
        setCaptureEnabledState(capture)
        setLoaded(true)
      },
    )
    return () => {
      cancelled = true
    }
  }, [])

  // Live updates from the Rust poller.
  useEffect(() => {
    if (!isTauri()) return
    const unNew = listen<ClipEntry>("clipboard:new", (event) => {
      const fresh = event.payload
      setEntries((prev) => {
        const without = prev.filter((e) => e.id !== fresh.id)
        return [fresh, ...without]
      })
    })
    const unCleared = listen("clipboard:cleared", () => {
      // Rust kept pinned items; refetch to get the authoritative list.
      void clipboardHistory.list().then(setEntries)
    })
    return () => {
      void unNew.then((fn) => fn())
      void unCleared.then((fn) => fn())
    }
  }, [])

  const reuse = useCallback(async (text: string) => {
    await clipboardHistory.reuse(text)
    // Optimistically float the entry to the top.
    setEntries((prev) => {
      const idx = prev.findIndex((e) => e.text === text)
      if (idx < 0) return prev
      const next = [...prev]
      const [moved] = next.splice(idx, 1)
      return [{ ...moved, ts: Date.now() }, ...next]
    })
  }, [])

  const remove = useCallback(async (id: string) => {
    setEntries((prev) => prev.filter((e) => e.id !== id))
    await clipboardHistory.remove(id)
  }, [])

  const togglePin = useCallback(async (id: string) => {
    setEntries((prev) =>
      prev.map((e) => (e.id === id ? { ...e, pinned: !e.pinned } : e)),
    )
    await clipboardHistory.togglePin(id)
  }, [])

  const setNote = useCallback(async (id: string, note: string | null) => {
    const normalized = note?.trim() ? note.trim() : null
    setEntries((prev) =>
      prev.map((e) => (e.id === id ? { ...e, note: normalized } : e)),
    )
    await clipboardHistory.setNote(id, normalized)
  }, [])

  const clear = useCallback(async () => {
    setEntries((prev) => prev.filter((e) => e.pinned))
    await clipboardHistory.clear()
  }, [])

  const wipe = useCallback(async () => {
    setEntries([])
    await clipboardHistory.wipe()
  }, [])

  const setCaptureEnabled = useCallback(async (enabled: boolean) => {
    setCaptureEnabledState(enabled)
    await clipboardHistory.setCapture(enabled)
  }, [])

  return {
    entries,
    loaded,
    captureEnabled,
    setCaptureEnabled,
    reuse,
    remove,
    togglePin,
    setNote,
    clear,
    wipe,
  }
}
