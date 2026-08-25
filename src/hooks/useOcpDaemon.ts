import { useCallback, useEffect, useRef, useState } from "react"
import { invoke } from "@tauri-apps/api/core"
import { listen } from "@tauri-apps/api/event"
import { isTauri } from "@/lib/runtime"

export type OcpEnv = {
  ocpPath: string | null
  npmPath: string | null
  claudePath: string | null
  /** Either system PATH or our private ~/.sayknow-runtime/ binary. */
  nodePath: string | null
  /** True only when the private runtime has been downloaded already. */
  privateNodeAvailable: boolean
  running: boolean
  /** Port where OCP is actually responding. May differ from 3456 if the user
   *  (or our fallback) chose a different port. `null` when not running. */
  runningPort: number | null
}

export type OcpAction = "idle" | "installing" | "starting" | "stopping"

type RawEnv = {
  ocp_path: string | null
  npm_path: string | null
  claude_path: string | null
  node_path: string | null
  private_node_available: boolean
  running: boolean
  running_port: number | null
}

function normalize(raw: RawEnv): OcpEnv {
  return {
    ocpPath: raw.ocp_path,
    npmPath: raw.npm_path,
    claudePath: raw.claude_path,
    nodePath: raw.node_path,
    privateNodeAvailable: raw.private_node_available,
    running: raw.running,
    runningPort: raw.running_port ?? null,
  }
}

export function useOcpDaemon(active: boolean, baseURL?: string) {
  const [env, setEnv] = useState<OcpEnv>({
    ocpPath: null,
    npmPath: null,
    claudePath: null,
    nodePath: null,
    privateNodeAvailable: false,
    running: false,
    runningPort: null,
  })
  const [action, setAction] = useState<OcpAction>("idle")
  const [error, setError] = useState<string | null>(null)
  const [logs, setLogs] = useState<string[]>([])
  const [startedAt, setStartedAt] = useState<number | null>(null)
  const startedAtRef = useRef<number | null>(null)
  useEffect(() => {
    startedAtRef.current = startedAt
  }, [startedAt])

  const refresh = useCallback(async () => {
    if (!isTauri()) return
    try {
      // Pass the user's configured baseURL so Rust can scan that port too —
      // not just the OCP default. Lets us pick up custom-port OCP instances.
      const raw = await invoke<RawEnv>("detect_ocp_env", { baseUrl: baseURL })
      setEnv(normalize(raw))
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e))
    }
  }, [baseURL])

  // Initial + periodic poll while the OCP card is active. This is an effect
  // synchronising with an external system (a daemon we don't control), which
  // is what effects are for — the lint rule can't tell that apart from
  // derived state being written back.
  useEffect(() => {
    if (!active || !isTauri()) return
    // eslint-disable-next-line react-hooks/set-state-in-effect
    void refresh()
    const id = window.setInterval(refresh, 2500)
    return () => window.clearInterval(id)
  }, [active, refresh])

  // Subscribe to streaming logs and status events from start_ocp.
  useEffect(() => {
    if (!isTauri()) return
    let unlistenLog: (() => void) | null = null
    let unlistenStatus: (() => void) | null = null
    void listen<string>("ocp:log", (e) => {
      setLogs((prev) => {
        const next = [...prev, e.payload]
        return next.length > 200 ? next.slice(-200) : next
      })
    }).then((fn) => {
      unlistenLog = fn
    })
    void listen<string>("ocp:status", () => {
      // Just trigger a refresh so the UI picks up the running flag fast.
      void refresh()
    }).then((fn) => {
      unlistenStatus = fn
    })
    return () => {
      unlistenLog?.()
      unlistenStatus?.()
    }
  }, [refresh])

  const install = useCallback(async () => {
    if (!isTauri()) return
    setAction("installing")
    setError(null)
    try {
      await invoke("install_ocp")
      await refresh()
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e))
    } finally {
      setAction("idle")
    }
  }, [refresh])

  const start = useCallback(async () => {
    if (!isTauri()) return
    setAction("starting")
    setError(null)
    setLogs([])
    setStartedAt(Date.now())
    try {
      await invoke("start_ocp")
      await refresh()
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e))
    } finally {
      setAction("idle")
      setStartedAt(null)
    }
  }, [refresh])

  const stop = useCallback(async () => {
    if (!isTauri()) return
    setAction("stopping")
    try {
      await invoke("stop_ocp")
      await refresh()
    } finally {
      setAction("idle")
    }
  }, [refresh])

  /** Top-level convenience: get OCP running.
   *  Relies on start_ocp's `npx -y ocp` fallback, so we don't need a separate
   *  install step. Install is still exposed for explicit reinstall. */
  const ensureRunning = useCallback(async () => {
    if (!isTauri()) return
    if (env.running) return
    await start()
  }, [env.running, start])

  // Auto-start when the user has already paid the install cost: repo cloned,
  // node + claude detected, and the daemon currently isn't running. The first
  // visit (no ocpPath / no claude) still requires an explicit click so the
  // user consents to the ~80 MB download.
  const autoStartedRef = useRef(false)
  useEffect(() => {
    if (!active || !isTauri()) return
    if (autoStartedRef.current) return
    if (env.running) return
    if (action !== "idle") return
    // Repo is considered already-installed when ocp CLI / runtime is visible;
    // we don't require the launchd plist (we no longer create one).
    const alreadyInstalled = !!env.ocpPath && !!env.claudePath && !!env.nodePath
    if (!alreadyInstalled) return
    autoStartedRef.current = true
    // Same rationale as the poll above: this starts an external process once
    // the environment reports it is already installed.
    // eslint-disable-next-line react-hooks/set-state-in-effect
    void start()
  }, [active, env.running, env.ocpPath, env.claudePath, env.nodePath, action, start])

  return {
    env,
    action,
    error,
    setError,
    refresh,
    install,
    start,
    stop,
    ensureRunning,
    logs,
    startedAt,
  }
}
