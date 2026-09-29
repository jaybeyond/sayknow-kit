import { useMemo, useState } from "react"
import { ChevronsUpDown } from "lucide-react"
import { Button } from "@/components/ui/button"
import {
  Command,
  CommandEmpty,
  CommandGroup,
  CommandInput,
  CommandItem,
  CommandList,
} from "@/components/ui/command"
import {
  Popover,
  PopoverContent,
  PopoverTrigger,
} from "@/components/ui/popover"
import { LANGS, type LangCode } from "@/lib/openrouter"
import { useT, type UILocaleSetting } from "@/i18n"

type Props = {
  value: LangCode
  onChange: (code: LangCode) => void
  showAuto?: boolean
  uiLocale: UILocaleSetting
}

/** Returns the language name localized into `targetLocale`, or the code as fallback. */
function localizedName(code: string, targetLocale: string): string {
  if (typeof Intl === "undefined" || !Intl.DisplayNames) return code
  try {
    return new Intl.DisplayNames([targetLocale], { type: "language" }).of(code) ?? code
  } catch {
    return code
  }
}

export function LangPicker({ value, onChange, showAuto, uiLocale }: Props) {
  const { t, locale } = useT(uiLocale)
  const [open, setOpen] = useState(false)
  // Controlled so the search box can offer a clear button; emptied on every
  // open, as the uncontrolled input used to be.
  const [search, setSearch] = useState("")
  const toggle = (next: boolean) => {
    if (next) setSearch("")
    setOpen(next)
  }
  const items = LANGS.filter((l) => showAuto || l.code !== "auto")

  // Pre-compute localized names for the right column. Keyed on the
  // resolved locale so it updates when the user changes app language.
  const localized = useMemo(() => {
    const map: Record<string, string> = {}
    for (const l of items) {
      if (l.code === "auto") {
        map[l.code] = t("lang.autoDetect")
      } else {
        map[l.code] = localizedName(l.code, locale)
      }
    }
    return map
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [locale, showAuto])

  const current = items.find((l) => l.code === value)
  const triggerLabel =
    current?.code === "auto"
      ? t("lang.autoDetect")
      : current?.label ?? value

  return (
    <Popover open={open} onOpenChange={toggle}>
      <PopoverTrigger asChild>
        <Button
          variant="ghost"
          role="combobox"
          aria-expanded={open}
          className="h-6 w-[110px] justify-between rounded-full bg-foreground/[0.06] px-2.5 text-[11px] font-medium hover:bg-foreground/10 data-[state=open]:bg-foreground/10 dark:bg-white/[0.08] dark:hover:bg-white/[0.13] dark:data-[state=open]:bg-white/[0.13]"
        >
          <span className="truncate">{triggerLabel}</span>
          <ChevronsUpDown className="ml-1 h-3 w-3 shrink-0 opacity-50" />
        </Button>
      </PopoverTrigger>
      <PopoverContent className="w-[280px] p-0" align="start" sideOffset={4}>
        <Command
          filter={(val, search, keywords) => {
            const haystack = (val + " " + (keywords?.join(" ") ?? "")).toLowerCase()
            return haystack.includes(search.toLowerCase()) ? 1 : 0
          }}
        >
          <CommandInput placeholder={t("lang.search")} className="text-xs" value={search} onValueChange={setSearch} clearLabel={t("search.clear")} />
          <CommandList className="max-h-[300px]">
            <CommandEmpty>{t("common.empty")}</CommandEmpty>
            <CommandGroup>
              {items.map((l) => {
                const left = l.code === "auto" ? t("lang.autoDetect") : l.label
                const right = localized[l.code]
                const showRight = right && right !== left
                return (
                  <CommandItem
                    key={l.code}
                    value={l.code}
                    keywords={[l.label, l.english, l.keywords, right]}
                    onSelect={() => {
                      onChange(l.code)
                      setOpen(false)
                    }}
                    className="flex items-center justify-between gap-2 py-1.5 text-xs"
                  >
                    <span className="truncate">{left}</span>
                    {showRight && (
                      <span className="shrink-0 text-[10px] text-muted-foreground">
                        {right}
                      </span>
                    )}
                  </CommandItem>
                )
              })}
            </CommandGroup>
          </CommandList>
        </Command>
      </PopoverContent>
    </Popover>
  )
}
