import { useMemo, useState } from "react"
import { History as HistoryIcon, Pin, PinOff, Search, Trash2, X } from "lucide-react"
import { Button } from "@/components/ui/button"
import { Input } from "@/components/ui/input"
import {
  Popover,
  PopoverContent,
  PopoverTrigger,
} from "@/components/ui/popover"
import { Separator } from "@/components/ui/separator"
import { timeAgo, type HistoryEntry } from "@/lib/history"
import { cn } from "@/lib/utils"
import { useT, type UILocaleSetting } from "@/i18n"

type Props = {
  entries: HistoryEntry[]
  onRestore: (e: HistoryEntry) => void
  onRemove: (id: string) => void
  onTogglePin: (id: string) => void
  onClear: () => void
  uiLocale: UILocaleSetting
  /** Controlled open state, so ⌘Y can toggle the menu. */
  open?: boolean
  onOpenChange?: (open: boolean) => void
  /** Appended to the button tooltip, e.g. "⌘Y". */
  shortcutHint?: string
}

export function HistoryMenu({
  entries,
  onRestore,
  onRemove,
  onTogglePin,
  onClear,
  uiLocale,
  open,
  onOpenChange,
  shortcutHint,
}: Props) {
  const { t } = useT(uiLocale)
  const [q, setQ] = useState("")

  const filtered = useMemo(() => {
    const t = q.trim().toLowerCase()
    if (!t) return entries
    return entries.filter(
      (e) =>
        e.source.toLowerCase().includes(t) ||
        e.target.toLowerCase().includes(t) ||
        e.model.toLowerCase().includes(t),
    )
  }, [entries, q])

  return (
    <Popover open={open} onOpenChange={onOpenChange}>
      <PopoverTrigger asChild>
        <Button
          variant="ghost"
          size="icon"
          className="h-7 w-7"
          aria-label={t("header.history")}
          title={shortcutHint ? `${t("header.history")} (${shortcutHint})` : t("header.history")}
        >
          <HistoryIcon className="h-3.5 w-3.5" />
        </Button>
      </PopoverTrigger>
      <PopoverContent
        align="end"
        sideOffset={6}
        className="w-[400px] p-0"
      >
        <div className="flex items-center gap-1.5 px-2.5 py-1.5">
          <div className="relative flex-1">
            <Search className="absolute left-2 top-1/2 h-3 w-3 -translate-y-1/2 text-muted-foreground" />
            <Input
              value={q}
              onChange={(e) => setQ(e.target.value)}
              placeholder={t("history.search")}
              className="h-7 pl-6 text-xs"
            />
          </div>
          {entries.length > 0 && (
            <Button
              variant="ghost"
              size="sm"
              className="h-7 px-2 text-[10px] text-muted-foreground hover:text-destructive"
              onClick={onClear}
              title={t("history.clearTooltip")}
            >
              <Trash2 className="h-3 w-3" />
              {t("history.clear")}
            </Button>
          )}
        </div>
        <Separator />
        <div className="max-h-[360px] overflow-y-auto">
          {filtered.length === 0 ? (
            <div className="px-3 py-8 text-center text-[11px] text-muted-foreground">
              {q ? t("history.noResult") : t("history.empty")}
            </div>
          ) : (
            filtered.map((e) => (
              <div
                key={e.id}
                className={cn(
                  "group relative border-b text-left text-xs hover:bg-accent/40 last:border-b-0",
                  e.pinned && "bg-accent/20",
                )}
              >
                <button
                  type="button"
                  onClick={() => onRestore(e)}
                  className="flex w-full flex-col gap-0.5 px-3 py-2 pr-14 text-left"
                >
                  <div className="line-clamp-1 text-foreground">{e.source}</div>
                  <div className="line-clamp-1 text-muted-foreground">
                    → {e.target}
                  </div>
                  <div className="mt-0.5 flex items-center gap-1.5 text-[10px] text-muted-foreground">
                    {e.pinned && <Pin className="h-2.5 w-2.5 text-foreground/70" />}
                    <span>{timeAgo(e.ts)}</span>
                    <span>·</span>
                    <span className="truncate">{e.model}</span>
                  </div>
                </button>
                <div className="absolute right-1.5 top-1.5 flex items-center gap-0.5 opacity-0 group-hover:opacity-100">
                  <button
                    type="button"
                    onClick={(ev) => {
                      ev.stopPropagation()
                      onTogglePin(e.id)
                    }}
                    className="rounded p-1 text-muted-foreground hover:bg-background hover:text-foreground"
                    aria-label={e.pinned ? t("history.unpin") : t("history.pin")}
                  >
                    {e.pinned ? (
                      <PinOff className="h-3 w-3" />
                    ) : (
                      <Pin className="h-3 w-3" />
                    )}
                  </button>
                  <button
                    type="button"
                    onClick={(ev) => {
                      ev.stopPropagation()
                      onRemove(e.id)
                    }}
                    className="rounded p-1 text-muted-foreground hover:bg-background hover:text-destructive"
                    aria-label={t("history.delete")}
                  >
                    <X className="h-3 w-3" />
                  </button>
                </div>
              </div>
            ))
          )}
        </div>
      </PopoverContent>
    </Popover>
  )
}
