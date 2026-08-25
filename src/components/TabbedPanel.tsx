import { useState } from "react"
import {
  Clipboard as ClipboardIcon,
  Gauge,
  Languages as TranslateIcon,
  Maximize2,
  MessageSquare,
  Minimize2,
  Pin,
  PinOff,
} from "lucide-react"
import { Button } from "@/components/ui/button"
import { HistoryMenu } from "./HistoryMenu"
import { QuickMenu } from "./QuickMenu"
import { useHistory } from "@/hooks/useHistory"
import type { HistoryEntry } from "@/lib/history"
import { TranslatePanel, type TranslateInjection } from "./TranslatePanel"
import { ChatPanel } from "./ChatPanel"
import { ClipboardPanel } from "./ClipboardPanel"
import { UsagePanel } from "./UsagePanel"
import type { Settings } from "@/hooks/useSettings"
import type { ThemeMode } from "@/hooks/useTheme"
import { useT } from "@/i18n"
import { storage } from "@/lib/storage"
import { cn } from "@/lib/utils"

type Tab = "translate" | "chat" | "clipboard" | "usage"
const TAB_KEY = "active-tab"

type Props = {
  settings: Settings
  update: (patch: Partial<Settings>) => void
  onLogout: () => void
  themeMode: ThemeMode
  setThemeMode: (m: ThemeMode) => void
}

export function TabbedPanel(props: Props) {
  const { t } = useT(props.settings.uiLocale)
  const [tab, setTab] = useState<Tab>(
    () => (storage.get<Tab>(TAB_KEY) ?? "translate") as Tab,
  )
  // What the clipboard tab or the history menu wants the translate tab to pick
  // up. nonce changes every dispatch so an identical payload still lands.
  const [pendingTranslateInput, setPendingTranslateInput] =
    useState<TranslateInjection | null>(null)
  const {
    entries: historyEntries,
    remove: removeHistory,
    togglePin: toggleHistoryPin,
    clear: clearHistory,
  } = useHistory()

  function selectTab(next: Tab) {
    setTab(next)
    storage.set(TAB_KEY, next)
  }

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
        <TabButton
          active={tab === "translate"}
          icon={TranslateIcon}
          label={t("tab.translate")}
          onClick={() => selectTab("translate")}
        />
        <TabButton
          active={tab === "chat"}
          icon={MessageSquare}
          label={t("tab.chat")}
          onClick={() => selectTab("chat")}
        />
        <TabButton
          active={tab === "clipboard"}
          icon={ClipboardIcon}
          label={t("tab.clipboard")}
          onClick={() => selectTab("clipboard")}
        />
        <TabButton
          active={tab === "usage"}
          icon={Gauge}
          label={t("tab.usage")}
          onClick={() => selectTab("usage")}
        />

        {/* Window and app-level controls. They used to sit in the translate
            tab's own header, which meant pin, resize and settings vanished the
            moment you switched tabs. */}
        <div className="ml-auto flex items-center">
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
            title={
              props.settings.windowMode === "compact"
                ? t("header.expand")
                : t("header.compact")
            }
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
            title={
              props.settings.pinned ? t("header.pinned") : t("header.pin")
            }
          >
            {props.settings.pinned ? (
              <Pin className="h-3.5 w-3.5 fill-current" />
            ) : (
              <PinOff className="h-3.5 w-3.5" />
            )}
          </Button>
          <HistoryMenu
            entries={historyEntries}
            onRestore={restoreHistory}
            onRemove={removeHistory}
            onTogglePin={toggleHistoryPin}
            onClear={clearHistory}
            uiLocale={props.settings.uiLocale}
          />
          <QuickMenu settings={props.settings} update={props.update} />
        </div>
      </div>

      {/* Active panel */}
      <div className="flex-1 overflow-hidden">
        {tab === "translate" ? (
          <TranslatePanel
            {...props}
            injectedInput={pendingTranslateInput ?? undefined}
          />
        ) : tab === "chat" ? (
          <ChatPanel settings={props.settings} update={props.update} />
        ) : tab === "clipboard" ? (
          <ClipboardPanel
            settings={props.settings}
            onSendToTranslate={sendToTranslate}
          />
        ) : (
          <UsagePanel settings={props.settings} active={tab === "usage"} />
        )}
      </div>
    </div>
  )
}

function TabButton({
  active,
  icon: Icon,
  label,
  onClick,
}: {
  active: boolean
  icon: typeof MessageSquare
  label: string
  onClick: () => void
}) {
  return (
    <button
      type="button"
      onClick={onClick}
      className={cn(
        "inline-flex items-center gap-1.5 rounded-md px-2.5 py-1 text-xs transition",
        active
          ? "bg-background text-foreground shadow-sm"
          : "text-muted-foreground hover:bg-background/50 hover:text-foreground",
      )}
    >
      <Icon className="h-3.5 w-3.5" />
      {label}
    </button>
  )
}
