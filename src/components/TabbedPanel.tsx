import { lazy, Suspense, useEffect, useLayoutEffect, useRef, useState, type RefObject } from "react"
import {
  Clipboard as ClipboardIcon,
  Languages as TranslateIcon,
  Loader2,
  Maximize2,
  MessageSquare,
  Minimize2,
  Pin,
  PinOff,
  Wrench,
} from "lucide-react"
import { Button } from "@/components/ui/button"
import { HistoryMenu } from "./HistoryMenu"
import { QuickMenu } from "./QuickMenu"
import { useHistory } from "@/hooks/useHistory"
import type { HistoryEntry } from "@/lib/history"
import { TranslatePanel, type TranslateInjection } from "./TranslatePanel"
// Only the translate tab is on screen when the popover opens, so the other
// three panels load when their tab is first chosen instead of riding along in
// the chunk that has to arrive before anything is visible.
const ChatPanel = lazy(() =>
  import("./ChatPanel").then((m) => ({ default: m.ChatPanel })),
)
const ClipboardPanel = lazy(() =>
  import("./ClipboardPanel").then((m) => ({ default: m.ClipboardPanel })),
)
const ToolsPanel = lazy(() =>
  import("./ToolsPanel").then((m) => ({ default: m.ToolsPanel })),
)
import type { Settings } from "@/hooks/useSettings"
import type { ThemeMode } from "@/hooks/useTheme"
import { useT } from "@/i18n"
import { storage } from "@/lib/storage"
import { cn } from "@/lib/utils"
import { invoke } from "@tauri-apps/api/core"
import { isTauri } from "@/lib/runtime"
import { HELD_RETRY_MS, HIDDEN_RELOAD_AFTER_MS, reloadHeld, reloadNow } from "@/lib/idle-reload"
import { formatCombo, shortcut } from "@/lib/shortcuts"
import { useShortcuts } from "@/hooks/useShortcuts"
import { useSlidingPill } from "@/hooks/useSlidingPill"
import { IconSwap } from "./motion"

type Tab = "translate" | "chat" | "clipboard" | "tools"
const TAB_KEY = "active-tab"
const TABS: readonly Tab[] = ["translate", "chat", "clipboard", "tools"]

/** What a global shortcut asks the popover to show. */
type ShortcutTarget = Tab | "newMemo"

function isShortcutTarget(v: string): v is ShortcutTarget {
  return v === "newMemo" || (TABS as readonly string[]).includes(v)
}

/**
 * A Radix menu, popover, select, or dialog (or the image lightbox) is open.
 * Escape belongs to it then, not to hiding the whole window. Radix closes on
 * the same keydown, but its DOM is still mounted while the event is running.
 */
function overlayOpen(): boolean {
  return !!document.querySelector(
    '[role="dialog"], [role="alertdialog"], [role="menu"], [role="listbox"], [data-radix-popper-content-wrapper]',
  )
}

function openSettings(section?: string) {
  if (!isTauri()) return
  void invoke("open_settings", section ? { section } : {}).catch((e) =>
    console.error("open_settings failed:", e),
  )
}

type Props = {
  settings: Settings
  update: (patch: Partial<Settings>) => void
  onLogout: () => void
  themeMode: ThemeMode
  setThemeMode: (m: ThemeMode) => void
}

export function TabbedPanel(props: Props) {
  const { t } = useT(props.settings.uiLocale)
  const [tab, setTab] = useState<Tab>(() => {
    // "usage" was a tab of its own until it moved under Tools; a stored value
    // from that build must not select a panel that no longer exists.
    const stored = storage.get<string>(TAB_KEY)
    return stored === "usage" ? "tools" : ((stored ?? "translate") as Tab)
  })
  // Native hide/show signals are reliable even when macOS keeps the WebView
  // document visible or does not give the popover focus.
  const [popoverVisible, setPopoverVisible] = useState(() => !isTauri())
  // What the clipboard tab or the history menu wants the translate tab to pick
  // up. nonce changes every dispatch so an identical payload still lands.
  const [pendingTranslateInput, setPendingTranslateInput] =
    useState<TranslateInjection | null>(null)
  // One-shot requests for panels that may mount only because of them; each
  // panel clears its own once handled.
  const [autofillRequest, setAutofillRequest] = useState<number | null>(null)
  const [composeRequest, setComposeRequest] = useState<number | null>(null)
  const [historyOpen, setHistoryOpen] = useState(false)
  const tabRef = useRef(tab)
  const tabsRef = useRef<HTMLDivElement>(null)
  const tabPillRef = useRef<HTMLSpanElement>(null)
  useSlidingPill(tabsRef, tabPillRef, tab)
  useTabsFit(tabsRef, props.settings.uiLocale)
  useEffect(() => {
    tabRef.current = tab
  }, [tab])
  // Once the popover has been used and then left hidden for a while, reload
  // the page so WebKit hands back what the visited panels held on to. A page
  // that has not been shown since it loaded is already lean, so it is left
  // alone; otherwise a hidden popover would reload itself forever.
  const shownSinceLoad = useRef(false)
  useEffect(() => {
    if (popoverVisible) {
      shownSinceLoad.current = true
      return
    }
    if (!isTauri() || !shownSinceLoad.current) return
    let timer: ReturnType<typeof setTimeout>
    const attempt = () => {
      if (reloadHeld()) timer = setTimeout(attempt, HELD_RETRY_MS)
      else reloadNow()
    }
    timer = setTimeout(attempt, HIDDEN_RELOAD_AFTER_MS)
    return () => clearTimeout(timer)
  }, [popoverVisible])
  const {
    entries: historyEntries,
    remove: removeHistory,
    togglePin: toggleHistoryPin,
    clear: clearHistory,
  } = useHistory()
  useEffect(() => {
    if (!isTauri()) return
    void invoke("set_pinned", { pinned: props.settings.pinned }).catch(() => {})
  }, [props.settings.pinned])

  useEffect(() => {
    if (!isTauri()) return
    const [width, height] =
      props.settings.windowMode === "compact" ? [720, 240] : [480, 580]
    void invoke("resize_main_window", { width, height }).catch(() => {})
  }, [props.settings.windowMode])

  function selectTab(next: Tab) {
    setTab(next)
    storage.set(TAB_KEY, next)
  }

  function goTo(target: ShortcutTarget) {
    if (target === "newMemo") {
      selectTab("clipboard")
      setComposeRequest(Date.now())
      return
    }
    selectTab(target)
    if (target === "translate") setAutofillRequest(Date.now())
  }

  // Global shortcuts. "sayknow:open" carries how the popover was just shown;
  // "sayknow:shortcut" arrives while it is already up.
  useEffect(() => {
    if (!isTauri()) return
    let cancelled = false
    let visibilityRevision = 0
    const unlisteners: (() => void)[] = []
    const reconcileVisibility = () => {
      const revision = ++visibilityRevision
      void invoke<boolean>("is_main_window_visible")
        .then((visible) => {
          if (!cancelled && revision === visibilityRevision) setPopoverVisible(visible)
        })
        .catch(() => {})
    }
    void import("@tauri-apps/api/event").then(({ listen }) => {
      const subscriptions = [
        listen<string>("sayknow:open", (event) => {
          visibilityRevision++
          setPopoverVisible(true)
          reconcileVisibility()
          const source = event.payload
          if (source === "shortcut") {
            // The plain toggle keeps the last tab and, on translate, still
            // pulls in the clipboard as it always has.
            if (tabRef.current === "translate") setAutofillRequest(Date.now())
            return
          }
          const target = source.startsWith("shortcut:") ? source.slice("shortcut:".length) : ""
          if (isShortcutTarget(target)) goTo(target)
        }),
        listen("sayknow:hidden", () => {
          visibilityRevision++
          setPopoverVisible(false)
        }),
        listen("sayknow:visibility-change", reconcileVisibility),
        listen<string>("sayknow:shortcut", (event) => {
          const target = event.payload
          if (!isShortcutTarget(target)) return
          // Pressing the key of the panel already in front puts it away,
          // like the plain toggle does.
          if (target === tabRef.current) {
            void invoke("hide_window").catch(() => {})
            return
          }
          goTo(target)
        }),
      ]
      for (const sub of subscriptions) {
        void sub.then((un) => {
          if (cancelled) un()
          else unlisteners.push(un)
        })
      }
      void Promise.all(subscriptions)
        .then(() => { if (!cancelled) reconcileVisibility() })
        .catch(() => {})
    })
    return () => {
      cancelled = true
      for (const un of unlisteners) un()
    }
    // goTo only touches state setters and storage.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [])

  useShortcuts({
    "app.tab.translate": () => selectTab("translate"),
    "app.tab.chat": () => selectTab("chat"),
    "app.tab.clipboard": () => selectTab("clipboard"),
    "app.tab.tools": () => selectTab("tools"),
    "app.history": () => setHistoryOpen((open) => !open),
    "app.pin": () => props.update({ pinned: !props.settings.pinned }),
    "app.compact": () =>
      props.update({
        windowMode: props.settings.windowMode === "compact" ? "normal" : "compact",
      }),
    "app.settings": () => openSettings(),
    "app.shortcuts": () => openSettings("shortcuts"),
    "app.hide": () => {
      if (overlayOpen()) return false
      if (isTauri()) void invoke("hide_window").catch(() => {})
    },
  })

  const tabHint = (id: string) => formatCombo(shortcut(id).combo)

  function sendToTranslate(text: string) {
    setPendingTranslateInput({ text, nonce: Date.now() })
    selectTab("translate")
  }

  // Restoring carries the result and language pair too, so the translate tab
  // shows the entry exactly as it was rather than re-running it.
  function restoreHistory(e: HistoryEntry) {
    setPendingTranslateInput({
      text: e.source,
      output: e.target,
      from: e.from,
      to: e.to,
      nonce: Date.now(),
    })
    selectTab("translate")
  }

  return (
    <div className="flex h-full flex-col">
      {/* Tab strip */}
      <div
        className="flex shrink-0 items-center gap-0.5 border-b bg-muted/40 px-1.5 py-1"
        data-tauri-drag-region
      >
        {/* The tabs give way before the controls do. In locales whose labels
            do not fit the 480px popover (es, de, vi), the inactive tabs fold
            to their icon and keep the label as their accessible name, so the
            active one still reads in full and settings stays on screen. */}
        <div ref={tabsRef} className="relative flex min-w-0 items-center gap-0.5">
        <span
          ref={tabPillRef}
          aria-hidden
          className="t-pill rounded-md bg-background shadow-sm dark:bg-white/[0.12] dark:shadow-none"
        />
        <TabButton
          active={tab === "translate"}
          icon={TranslateIcon}
          label={t("tab.translate")}
          hint={tabHint("app.tab.translate")}
          onClick={() => selectTab("translate")}
        />
        <TabButton
          active={tab === "chat"}
          icon={MessageSquare}
          label={t("tab.chat")}
          hint={tabHint("app.tab.chat")}
          onClick={() => selectTab("chat")}
        />
        <TabButton
          active={tab === "clipboard"}
          icon={ClipboardIcon}
          label={t("tab.clipboard")}
          hint={tabHint("app.tab.clipboard")}
          onClick={() => selectTab("clipboard")}
        />

        <TabButton
          active={tab === "tools"}
          icon={Wrench}
          label={t("tab.tools")}
          hint={tabHint("app.tab.tools")}
          onClick={() => selectTab("tools")}
        />
        </div>

        {/* Window and app-level controls. They used to sit in the translate
            tab's own header, which meant pin, resize and settings vanished the
            moment you switched tabs. */}
        <div className="ml-auto flex shrink-0 items-center">
          <Button
            variant="ghost"
            size="icon"
            className="h-6 w-6"
            onClick={() =>
              props.update({
                windowMode:
                  props.settings.windowMode === "compact"
                    ? "normal"
                    : "compact",
              })
            }
            aria-label={
              props.settings.windowMode === "compact"
                ? t("header.expand")
                : t("header.compact")
            }
            title={`${
              props.settings.windowMode === "compact"
                ? t("header.expand")
                : t("header.compact")
            } (${tabHint("app.compact")})`}
          >
            {props.settings.windowMode === "compact" ? (
              <Maximize2 className="h-3.5 w-3.5" />
            ) : (
              <Minimize2 className="h-3.5 w-3.5" />
            )}
          </Button>
          <Button
            variant="ghost"
            size="icon"
            className="h-6 w-6"
            onClick={() => props.update({ pinned: !props.settings.pinned })}
            aria-label={
              props.settings.pinned ? t("header.unpin") : t("header.pin")
            }
            title={`${
              props.settings.pinned ? t("header.pinned") : t("header.pin")
            } (${tabHint("app.pin")})`}
          >
            <IconSwap
              on={props.settings.pinned}
              a={<PinOff className="h-3.5 w-3.5" />}
              b={<Pin className="h-3.5 w-3.5 fill-current" />}
            />
          </Button>
          <HistoryMenu
            entries={historyEntries}
            onRestore={restoreHistory}
            onRemove={removeHistory}
            onTogglePin={toggleHistoryPin}
            onClear={clearHistory}
            uiLocale={props.settings.uiLocale}
            open={historyOpen}
            onOpenChange={setHistoryOpen}
            shortcutHint={tabHint("app.history")}
          />
          <QuickMenu settings={props.settings} update={props.update} />
        </div>
      </div>

      {/* Active panel */}
      <div className="flex-1 overflow-hidden">
        <Suspense
          fallback={
            <div className="flex h-full items-center justify-center">
              <Loader2 className="h-4 w-4 animate-spin text-muted-foreground" />
            </div>
          }
        >
          {tab === "translate" ? (
            <TranslatePanel
              {...props}
              injectedInput={pendingTranslateInput ?? undefined}
              autofillRequest={autofillRequest}
              onAutofillHandled={() => setAutofillRequest(null)}
            />
          ) : tab === "chat" ? (
            <ChatPanel settings={props.settings} update={props.update} />
          ) : tab === "clipboard" ? (
            <ClipboardPanel
              settings={props.settings}
              onSendToTranslate={sendToTranslate}
              composeRequest={composeRequest}
              onComposeHandled={() => setComposeRequest(null)}
            />
          ) : (
            <ToolsPanel settings={props.settings} active={popoverVisible && tab === "tools"} />
          )}
        </Suspense>
      </div>
    </div>
  )
}

function TabButton({
  active,
  icon: Icon,
  label,
  hint,
  onClick,
}: {
  active: boolean
  icon: typeof MessageSquare
  label: string
  hint: string
  onClick: () => void
}) {
  return (
    <button
      type="button"
      onClick={onClick}
      title={`${label} (${hint})`}
      data-pill-active={active}
      className={cn(
        // The active background is the sliding pill behind the tabs.
        "relative z-[1] inline-flex shrink-0 items-center gap-1.5 whitespace-nowrap rounded-md px-2.5 py-1 text-xs transition-[color,background-color]",
        active
          ? "text-foreground"
          : "text-muted-foreground hover:bg-background/50 hover:text-foreground dark:hover:bg-white/[0.06]",
      )}
    >
      <Icon aria-hidden="true" className="h-3.5 w-3.5 shrink-0" />
      <span className={cn(!active && "in-data-[tabs-fit=icons]:sr-only")}>{label}</span>
    </button>
  )
}

/**
 * Marks the top tabs `data-tabs-fit="icons"` when their full labels do not
 * fit beside the window controls, else `"labels"`. Measured with the labels
 * out every time, so a wider popover (compact mode is 720px) brings them back.
 * Set on the DOM rather than through state: the measurement has to flip the
 * attribute and read layout in one go, before the frame is painted.
 */
function useTabsFit(ref: RefObject<HTMLDivElement | null>, locale: string) {
  useLayoutEffect(() => {
    const tabs = ref.current
    const strip = tabs?.parentElement
    if (!tabs || !strip) return
    const measure = () => {
      tabs.dataset.tabsFit = "labels"
      // The container may shrink (min-w-0) but its buttons do not, so labels
      // that do not fit show up as content wider than the container.
      tabs.dataset.tabsFit = tabs.scrollWidth > tabs.clientWidth + 0.5 ? "icons" : "labels"
    }
    measure()
    let live = true
    void document.fonts?.ready.then(() => {
      if (live) measure()
    })
    // The strip spans the window, so folding the labels never resizes it.
    const ro = typeof ResizeObserver === "undefined" ? null : new ResizeObserver(measure)
    ro?.observe(strip)
    return () => {
      live = false
      ro?.disconnect()
    }
  }, [ref, locale])
}
