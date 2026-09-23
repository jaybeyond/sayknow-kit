import { lazy, Suspense, useCallback, useEffect, useRef } from "react"
import { Loader2 } from "lucide-react"
import { invoke } from "@tauri-apps/api/core"
import { getCurrentWebviewWindow } from "@tauri-apps/api/webviewWindow"
import { useSettings } from "./hooks/useSettings"
import { useTheme } from "./hooks/useTheme"
import { useT } from "./i18n"
import { LoginPanel } from "./components/LoginPanel"
import { TabbedPanel } from "./components/TabbedPanel"
// The settings screen lives in its own window, so the popover should not pay
// for it in the chunk it loads on every open.
const SettingsWindow = lazy(() =>
  import("./components/SettingsWindow").then((m) => ({
    default: m.SettingsWindow,
  })),
)
import { isTauri } from "./lib/runtime"

function isSettingsWindow(): boolean {
  if (typeof window === "undefined") return false
  return new URLSearchParams(window.location.search).get("window") === "settings"
}

function App() {
  if (isSettingsWindow()) return <SettingsRoot />
  return <MainRoot />
}

function MainRoot() {
  const { settings, update, clearKey, isLoggedIn, loaded } = useSettings()
  const { mode: themeMode, setMode: setThemeMode } = useTheme()
  const { t } = useT(settings.uiLocale)
  const contentRef = useRef<HTMLDivElement>(null)

  // Push the tray tooltip whenever the UI locale settles. The Rust side only
  // bakes in a locale-neutral default; this localizes it for all 8 locales.
  // There is no tray menu to label: clicking the icon opens the popover, and
  // quitting lives in the About panel.
  useEffect(() => {
    if (!isTauri() || !loaded) return
    const tagline = t("app.tagline")
    if (tagline) {
      void invoke("set_tray_tooltip", { tooltip: `SayKnow Kit — ${tagline}` }).catch(() => {})
    }
  }, [t, loaded])

  useEffect(() => {
    function play() {
      const el = contentRef.current
      if (!el) return
      el.classList.remove("appear")
      void el.offsetWidth
      el.classList.add("appear")
    }
    play()
    // Animation is decorative: content stays visible even without show/focus.
    let disposed = false
    let unlistenOpen: (() => void) | undefined
    if (isTauri()) {
      void import("@tauri-apps/api/event")
        .then(({ listen }) => listen("sayknow:open", () => play()))
        .then((un) => {
          if (disposed) un()
          else unlistenOpen = un
        })
        .catch(() => {})
    }
    window.addEventListener("focus", play)
    return () => {
      disposed = true
      unlistenOpen?.()
      window.removeEventListener("focus", play)
    }
  }, [])


  return (
    // Outer shell — always rendered with full bg/border/shadow/blur so the
    // popover skin never disappears. Animation lives on the inner content layer.
    <div className="h-svh w-svw overflow-hidden rounded-xl border border-border/50 bg-background/85 shadow-2xl ring-1 ring-black/5 backdrop-blur-xl dark:bg-background/85 dark:ring-white/10">
      <div
        ref={contentRef}
        className="popover-content appear h-full w-full"
      >
        {!loaded ? (
          <div className="flex h-full items-center justify-center">
            <Loader2 className="h-4 w-4 animate-spin text-muted-foreground" />
          </div>
        ) : isLoggedIn ? (
          <TabbedPanel
            settings={settings}
            update={update}
            onLogout={clearKey}
            themeMode={themeMode}
            setThemeMode={setThemeMode}
          />
        ) : (
          <LoginPanel update={update} uiLocale={settings.uiLocale} />
        )}
      </div>
    </div>
  )
}

function SettingsRoot() {
  const { settings, update, clearKey, loaded, credentialError, refreshOAuth } =
    useSettings()
  const { mode: themeMode, setMode: setThemeMode } = useTheme()

  // Settings window has no logged-out UI of its own; the only visible signal
  // is the main popover flipping back to LoginPanel. To make logout feel
  // responsive in *this* window too, close it after the Keychain write +
  // rev-signal has fully completed.
  const handleLogout = useCallback(async () => {
    await clearKey()
    if (isTauri()) {
      try {
        await getCurrentWebviewWindow().close()
      } catch {
        /* window already gone */
      }
    }
  }, [clearKey])

  if (!loaded) {
    return (
      <div className="flex h-svh items-center justify-center bg-background">
        <Loader2 className="h-4 w-4 animate-spin text-muted-foreground" />
      </div>
    )
  }

  return (
    <Suspense
      fallback={
        <div className="flex h-svh items-center justify-center bg-background">
          <Loader2 className="h-4 w-4 animate-spin text-muted-foreground" />
        </div>
      }
    >
      <SettingsWindow
        settings={settings}
        update={update}
        onLogout={handleLogout}
        themeMode={themeMode}
        setThemeMode={setThemeMode}
        credentialError={credentialError}
        refreshOAuth={refreshOAuth}
      />
    </Suspense>
  )
}

export default App
