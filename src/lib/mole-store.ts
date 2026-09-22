import {
  detectMole,
  parseAnalyze,
  parseCleanPreview,
  parseResult,
  runMoleAction,
  stripAnsi,
  type CleanPreviewItem,
  type MoleAnalyze,
  type MoleInfo,
  type MoleResult,
  type MoleRun,
} from "./mole"

export type SessionId = "disk" | "cache" | "tune"

export type SessionState = {
  progress: string[]
  items: CleanPreviewItem[]
  analyze: MoleAnalyze | null
  result: MoleResult | null
  error: string | null
  lastAction: string | null
}

export type MoleStore = {
  info: MoleInfo | null | "loading"
  sessions: Record<SessionId, SessionState>
  busy: SessionId | null
}

const EMPTY: SessionState = {
  progress: [],
  items: [],
  analyze: null,
  result: null,
  error: null,
  lastAction: null,
}

const emptySessions = (): Record<SessionId, SessionState> => ({
  disk: { ...EMPTY },
  cache: { ...EMPTY },
  tune: { ...EMPTY },
})

let state: MoleStore = {
  info: "loading",
  sessions: emptySessions(),
  busy: null,
}

const listeners = new Set<() => void>()
const lines: Record<SessionId, string[]> = {
  disk: [],
  cache: [],
  tune: [],
}
let current: SessionId | null = null
let unlisten: (() => void) | null = null

function emit() {
  for (const listener of listeners) listener()
}

function set(patch: Partial<MoleStore>) {
  state = { ...state, ...patch }
  emit()
}

function patchSession(id: SessionId, next: Partial<SessionState>) {
  state = {
    ...state,
    sessions: { ...state.sessions, [id]: { ...state.sessions[id], ...next } },
  }
  emit()
}

export function subscribe(listener: () => void) {
  listeners.add(listener)
  return () => {
    listeners.delete(listener)
  }
}

export function getSnapshot(): MoleStore {
  return state
}

function isJsonJunk(line: string): boolean {
  const t = line.trim()
  return t === "{" || t === "}" || t === "[" || t === "]" || t.startsWith('"')
}

export async function detect(): Promise<void> {
  try {
    set({ info: await detectMole() })
  } catch {
    set({ info: null })
  }
}

export async function run(id: SessionId, action: string): Promise<void> {
  set({ busy: id })
  current = id
  lines[id] = []
  patchSession(id, { progress: [], result: null, error: null, lastAction: action })
  unlisten?.()
  try {
    const { listen } = await import("@tauri-apps/api/event")
    unlisten = await listen<string>("mole:line", (event) => {
      const line = stripAnsi(event.payload).trim()
      if (!line || current !== id || isJsonJunk(line)) return
      lines[id] = [...lines[id].slice(-40), line]
      patchSession(id, {
        progress: lines[id],
        items: parseCleanPreview(lines[id].join("\n")),
      })
    })
  } catch {
    /* web preview has no event bus */
  }
  try {
    const outcome: MoleRun = await runMoleAction(action)
    const text = `${outcome.stdout}\n${outcome.stderr}`
    if (!outcome.ok && !outcome.json) {
      patchSession(id, {
        error: outcome.stderr || outcome.stdout || "mole_failed",
        progress: [],
      })
      return
    }
    patchSession(id, {
      ...(action === "analyze" ? { analyze: parseAnalyze(outcome.json) } : {}),
      items: parseCleanPreview(text),
      result: parseResult(text),
      progress: [],
      lastAction: action,
    })
  } catch (e) {
    patchSession(id, { error: String(e), progress: [] })
  } finally {
    unlisten?.()
    unlisten = null
    if (current === id) current = null
    set({ busy: state.busy === id ? null : state.busy })
  }
}

/** Header refresh re-runs the last non-destructive scan in each session. */
export async function refreshScans(): Promise<void> {
  const jobs: Promise<void>[] = []
  if (state.sessions.disk.analyze || state.sessions.disk.lastAction === "analyze") {
    jobs.push(run("disk", "analyze"))
  }
  if (state.sessions.cache.items.length > 0 || state.sessions.cache.lastAction) {
    jobs.push(run("cache", "clean-preview"))
  }
  if (state.sessions.tune.items.length > 0 || state.sessions.tune.lastAction) {
    jobs.push(run("tune", "optimize-preview"))
  }
  await Promise.all(jobs)
}
