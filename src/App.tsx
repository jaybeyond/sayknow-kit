import { useCallback, useEffect, useRef } from "react"
import { Loader2 } from "lucide-react"
import { invoke } from "@tauri-apps/api/core"
import { getCurrentWebviewWindow } from "@tauri-apps/api/webviewWindow"
import { useSettings } from "./hooks/useSettings"
import { useTheme } from "./hooks/useTheme"
import { useT } from "./i18n"
import { LoginPanel } from "./components/LoginPanel"
import { TabbedPanel } from "./components/TabbedPanel"
import { SettingsWindow } from "./components/SettingsWindow"
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
  // Read pin state via ref so the listeners don't need to re-bind every toggle.
  const pinnedRef = useRef(settings.pinned)
  pinnedRef.current = settings.pinned

  // Push the resolved "Quit SayKnow Kit" label and the tray tooltip to the native
  // tray whenever the UI locale settles. The Rust side only bakes in a
  // locale-neutral default; this localizes both for all 8 locales.
  useEffect(() => {
    if (!isTauri() || !loaded) return
    const label = t("tray.quit")
    if (label) void invoke("set_tray_quit_label", { label }).catch(() => {})
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
    function reset() {
      // When pinned, the window stays visible on blur — don't strip the
      // `appear` class or the content snaps back to opacity:0 leaving the
      // user with a blank popover.
      if (pinnedRef.current) return
      contentRef.current?.classList.remove("appear")
    }
    play()
    window.addEventListener("focus", play)
    window.addEventListener("blur", reset)
    return () => {
      window.removeEventListener("focus", play)
      window.removeEventListener("blur", reset)
    }
  }, [])

  // Defensive: if the user pins after the content has already been hidden by
  // a previous blur, force the content visible again.
  useEffect(() => {
    if (!settings.pinned) return
    const el = contentRef.current
    if (!el || el.classList.contains("appear")) return
    void el.offsetWidth
    el.classList.add("appear")
  }, [settings.pinned])

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
  const { settings, update, clearKey, loaded } = useSettings()
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
    <SettingsWindow
      settings={settings}
      update={update}
      onLogout={handleLogout}
      themeMode={themeMode}
      setThemeMode={setThemeMode}
    />
  )
}

export default App
