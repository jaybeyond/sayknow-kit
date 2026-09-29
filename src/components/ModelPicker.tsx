import { useState } from "react"
import { Check, ChevronsUpDown, Loader2 } from "lucide-react"
import { Button } from "@/components/ui/button"
import {
  Command,
  CommandEmpty,
  CommandGroup,
  CommandInput,
  CommandItem,
  CommandList,
} from "@/components/ui/command"
import { Popover, PopoverContent, PopoverTrigger } from "@/components/ui/popover"
import { cn } from "@/lib/utils"
import type { OpenRouterModel } from "@/lib/openrouter"

type Props = {
  value: string
  onChange: (id: string) => void
  models: OpenRouterModel[]
  loading?: boolean
  placeholder?: string
  /** Label for the search box's clear button. */
  clearLabel?: string
}

function priceLabel(m: OpenRouterModel): string {
  const p = m.pricing?.prompt
  const c = m.pricing?.completion
  if (!p && !c) return ""
  const pn = parseFloat(p ?? "0") * 1_000_000
  const cn = parseFloat(c ?? "0") * 1_000_000
  if (pn === 0 && cn === 0) return "free"
  return `$${pn.toFixed(2)} / $${cn.toFixed(2)} per M`
}

export function ModelPicker({
  value,
  onChange,
  models,
  loading,
  placeholder,
  clearLabel,
}: Props) {
  const [open, setOpen] = useState(false)
  // Controlled for the clear button; emptied on every open.
  const [search, setSearch] = useState("")
  const toggle = (next: boolean) => {
    if (next) setSearch("")
    setOpen(next)
  }
  const current = models.find((m) => m.id === value)
  const label = current?.name ?? (value || placeholder || "선택...")

  return (
    <Popover open={open} onOpenChange={toggle}>
      <PopoverTrigger asChild>
        <Button
          variant="outline"
          role="combobox"
          aria-expanded={open}
          className="w-full justify-between truncate text-xs h-8"
        >
          <span className={`truncate ${!value ? "text-muted-foreground font-normal" : ""}`}>
            {label}
          </span>
          {loading ? (
            <Loader2 className="ml-2 h-3 w-3 shrink-0 animate-spin opacity-50" />
          ) : (
            <ChevronsUpDown className="ml-2 h-3 w-3 shrink-0 opacity-50" />
          )}
        </Button>
      </PopoverTrigger>
      <PopoverContent
        className="w-[360px] p-0"
        align="end"
        sideOffset={4}
      >
        <Command
          filter={(val, search, keywords) => {
            const haystack = (val + " " + (keywords?.join(" ") ?? "")).toLowerCase()
            return haystack.includes(search.toLowerCase()) ? 1 : 0
          }}
        >
          <CommandInput placeholder="모델 검색 (예: claude, gpt, free)..." value={search} onValueChange={setSearch} clearLabel={clearLabel} />
          <CommandList className="max-h-[320px]">
            <CommandEmpty>
              {loading ? "불러오는 중..." : "결과 없음"}
            </CommandEmpty>
            <CommandGroup>
              {models.map((m) => {
                const price = priceLabel(m)
                return (
                  <CommandItem
                    key={m.id}
                    value={m.id}
                    keywords={[m.name ?? "", price]}
                    onSelect={() => {
                      onChange(m.id)
                      setOpen(false)
                    }}
                    className="flex items-start gap-2 py-1.5"
                  >
                    <Check
                      className={cn(
                        "mt-0.5 h-3.5 w-3.5",
                        value === m.id ? "opacity-100" : "opacity-0",
                      )}
                    />
                    <div className="flex min-w-0 flex-1 flex-col">
                      <span className="truncate text-xs font-medium">
                        {m.name ?? m.id}
                      </span>
                      <span className="truncate text-[10px] text-muted-foreground">
                        {m.id}
                        {price && <> · {price}</>}
                      </span>
                    </div>
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
