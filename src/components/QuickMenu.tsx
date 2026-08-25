import { useState } from "react"
import { Settings as SettingsIcon, Zap, ZapOff } from "lucide-react"
import { Button } from "@/components/ui/button"
import { Label } from "@/components/ui/label"
import {
  Popover,
  PopoverContent,
  PopoverTrigger,
} from "@/components/ui/popover"
import { Separator } from "@/components/ui/separator"
import { Switch } from "@/components/ui/switch"
import type { Settings } from "@/hooks/useSettings"
import { useT } from "@/i18n"
import { isTauri } from "@/lib/runtime"
import { invoke } from "@tauri-apps/api/core"

/**
 * The gear popover: translate mode, clipboard-on-hotkey, and the door to the
 * full settings window. Lives beside the tab strip so it reaches every tab,
 * not just the one that happened to own the header first.
 */
export function QuickMenu({
  settings,
  update,
}: {
  settings: Settings
  update: (patch: Partial<Settings>) => void
}) {
  const { t } = useT(settings.uiLocale)
  const [open, setOpen] = useState(false)

  async function openSettings() {
    setOpen(false)
    if (isTauri()) {
      try {
        await invoke("open_settings")
      } catch (e) {
        console.error("open_settings failed:", e)
      }
    }
  }

  return (
    <Popover open={open} onOpenChange={setOpen}>
      <PopoverTrigger asChild>
        <Button
          variant="ghost"
          size="icon"
          className="h-7 w-7"
          aria-label={t("header.settings")}
        >
          <SettingsIcon className="h-3.5 w-3.5" />
        </Button>
      </PopoverTrigger>
      <PopoverContent align="end" sideOffset={6} className="w-[260px] p-3">
        <div className="space-y-3">
          <div>
            <Label className="text-[11px]">{t("settings.mode")}</Label>
            <div className="mt-1.5 grid grid-cols-2 gap-1">
              <Button
                size="sm"
                variant={settings.autoTranslate ? "secondary" : "ghost"}
                className="h-8 text-[11px]"
                onClick={() => update({ autoTranslate: true })}
              >
                <Zap className="h-3 w-3" />
                {t("settings.mode.auto")}
              </Button>
              <Button
                size="sm"
                variant={!settings.autoTranslate ? "secondary" : "ghost"}
                className="h-8 text-[11px]"
                onClick={() => update({ autoTranslate: false })}
              >
                <ZapOff className="h-3 w-3" />
                {t("settings.mode.manual")}
              </Button>
            </div>
          </div>

          <Separator />

          <div className="flex items-center justify-between gap-2">
            <Label htmlFor="qm-clipboard" className="text-[11px]">
              {t("settings.clipboard.title")}
            </Label>
            <Switch
              id="qm-clipboard"
              checked={settings.clipboardOnHotkey}
              onCheckedChange={(v) => update({ clipboardOnHotkey: v })}
            />
          </div>

          <Separator />

          <Button
            variant="default"
            size="sm"
            className="w-full text-xs"
            onClick={openSettings}
          >
            <SettingsIcon className="h-3 w-3" />
            {t("settings.openButton")}
          </Button>
        </div>
      </PopoverContent>
    </Popover>
  )
}
