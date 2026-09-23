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
let activeOperation: Promise<void> | null = null

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

async function executeRun(id: SessionId, action: string): Promise<void> {
  set({ busy: id })
  const runLines: string[] = []
  let unlisten: (() => void) | undefined
  let acceptingLines = true
  patchSession(id, { progress: [], result: null, error: null, lastAction: action })
  try {
    try {
      const { listen } = await import("@tauri-apps/api/event")
      unlisten = await listen<string>("mole:line", (event) => {
        const line = stripAnsi(event.payload).trim()
        if (!acceptingLines || !line || isJsonJunk(line)) return
        runLines.push(line)
        if (runLines.length > 40) runLines.shift()
        patchSession(id, {
          progress: [...runLines],
          items: parseCleanPreview(runLines.join("\n")),
        })
      })
    } catch {
      /* web preview has no event bus */
    }

    const outcome: MoleRun = await runMoleAction(action)
    const text = `${outcome.stdout}\n${outcome.stderr}`
    if (!outcome.ok) {
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
    acceptingLines = false
    try {
      unlisten?.()
    } finally {
      set({ busy: null })
    }
  }
}

function admit(operation: () => Promise<void>): Promise<void> {
  if (activeOperation) return activeOperation

  let resolve!: () => void
  let reject!: (reason: unknown) => void
  const admitted = new Promise<void>((next, fail) => {
    resolve = next
    reject = fail
  })
  activeOperation = admitted
  void operation().then(
    () => {
      if (activeOperation === admitted) activeOperation = null
      resolve()
    },
    (error) => {
      if (activeOperation === admitted) activeOperation = null
      reject(error)
    },
  )
  return admitted
}

export function run(id: SessionId, action: string): Promise<void> {
  return admit(() => executeRun(id, action))
}

/** Header refresh re-runs the last non-destructive scan in each session. */
export function refreshScans(): Promise<void> {
  return admit(async () => {
    if (state.sessions.disk.analyze || state.sessions.disk.lastAction === "analyze") {
      await executeRun("disk", "analyze")
    }
    if (state.sessions.cache.items.length > 0 || state.sessions.cache.lastAction) {
      await executeRun("cache", "clean-preview")
    }
    if (state.sessions.tune.items.length > 0 || state.sessions.tune.lastAction) {
      await executeRun("tune", "optimize-preview")
    }
  })
}
