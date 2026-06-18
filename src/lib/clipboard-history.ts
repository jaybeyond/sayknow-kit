import { invoke } from "@tauri-apps/api/core"
import { isTauri } from "./runtime"

export type ClipEntry = {
  id: string
  text: string
  preview: string
  ts: number
  pinned: boolean
  /** Optional user-authored note. Pass empty string or null to clear. */
  note?: string | null
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
