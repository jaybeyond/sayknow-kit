import { invoke } from "@tauri-apps/api/core"
import { isTauri } from "./runtime"

/** `clip` = captured from the system clipboard; `memo` = written in the app. */
export type ClipKind = "clip" | "memo"

export type ClipEntry = {
  id: string
  text: string
  preview: string
  ts: number
  pinned: boolean
  /** Optional user-authored note. Pass empty string or null to clear. */
  note?: string | null
  /** Absent in entries persisted before memos existed; treat as `clip`. */
  kind?: ClipKind
}

export function isMemo(entry: Pick<ClipEntry, "kind">): boolean {
  return entry.kind === "memo"
}

async function safeInvoke<T>(cmd: string, args?: Record<string, unknown>): Promise<T | null> {
  if (!isTauri()) return null
  try {
    return await invoke<T>(cmd, args)
  } catch {
    return null
  }
}

export const clipboardHistory = {
  async list(): Promise<ClipEntry[]> {
    return (await safeInvoke<ClipEntry[]>("get_clipboard_history")) ?? []
  },
  async reuse(text: string): Promise<void> {
    await safeInvoke<void>("set_clipboard_text", { text })
  },
  async remove(id: string): Promise<void> {
    await safeInvoke<void>("delete_clipboard_entry", { id })
  },
  async togglePin(id: string): Promise<void> {
    await safeInvoke<void>("toggle_clipboard_pin", { id })
  },
  async setNote(id: string, note: string | null): Promise<void> {
    await safeInvoke<void>("set_clipboard_entry_note", { id, note })
  },
  /** Returns the stored memo, or null when the text is blank or outside Tauri. */
  async createMemo(text: string): Promise<ClipEntry | null> {
    return safeInvoke<ClipEntry>("create_clipboard_memo", { text })
  },
  async updateMemo(id: string, text: string): Promise<ClipEntry | null> {
    return safeInvoke<ClipEntry>("update_clipboard_memo", { id, text })
  },
  async clear(): Promise<void> {
    await safeInvoke<void>("clear_clipboard_history")
  },
  async wipe(): Promise<void> {
    await safeInvoke<void>("wipe_clipboard_history")
  },
  async setCapture(enabled: boolean): Promise<void> {
    await safeInvoke<void>("set_clipboard_capture", { enabled })
  },
  async getCapture(): Promise<boolean> {
    const v = await safeInvoke<boolean>("get_clipboard_capture")
    return v ?? true
  },
  async setMaxEntries(max: number): Promise<void> {
    await safeInvoke<void>("set_clipboard_max_entries", { max })
  },
}
