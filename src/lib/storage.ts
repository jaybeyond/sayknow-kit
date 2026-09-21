const PREFIX = "sayknow:"

export const storage = {
  get<T = string>(key: string): T | null {
    try {
      const raw = localStorage.getItem(PREFIX + key)
      if (raw === null) return null
      try {
        return JSON.parse(raw) as T
      } catch {
        return raw as unknown as T
      }
    } catch {
      return null
    }
  },
  /**
   * Returns false when the write did not land — almost always a quota
   * overflow. Callers that store large payloads (chat images) use this to
   * shed weight and retry instead of silently losing the whole record.
   */
  set(key: string, value: unknown): boolean {
    try {
      const serialized =
        typeof value === "string" ? value : JSON.stringify(value)
      localStorage.setItem(PREFIX + key, serialized)
      return true
    } catch {
      return false
    }
  },
  remove(key: string) {
    try {
      localStorage.removeItem(PREFIX + key)
    } catch {
      // ignore
    }
  },
}
