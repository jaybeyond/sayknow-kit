import { useState } from "react"
import {
  Clipboard as ClipboardIcon,
  Languages as TranslateIcon,
  MessageSquare,
} from "lucide-react"
import { TranslatePanel } from "./TranslatePanel"
import { ChatPanel } from "./ChatPanel"
import { ClipboardPanel } from "./ClipboardPanel"
import type { Settings } from "@/hooks/useSettings"
import type { ThemeMode } from "@/hooks/useTheme"
import { useT } from "@/i18n"
import { storage } from "@/lib/storage"
import { cn } from "@/lib/utils"

type Tab = "translate" | "chat" | "clipboard"
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
  // Text the clipboard tab wants the translate tab to pick up. nonce changes
  // every dispatch so identical text can be re-sent.
  const [pendingTranslateInput, setPendingTranslateInput] = useState<{
    text: string
    nonce: number
  } | null>(null)

  function selectTab(next: Tab) {
    setTab(next)
    storage.set(TAB_KEY, next)
  }

  function sendToTranslate(text: string) {
    setPendingTranslateInput({ text, nonce: Date.now() })
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
        ) : (
          <ClipboardPanel
            settings={props.settings}
            onSendToTranslate={sendToTranslate}
          />
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
