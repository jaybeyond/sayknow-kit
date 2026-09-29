import { useRef, type Ref } from "react"
import { Search, X } from "lucide-react"
import { Input } from "@/components/ui/input"
import { dissolveClear } from "@/lib/dissolve-clear"

/** A compact search box with a clear button that dissolves the query away. */
export function SearchField({
  value,
  onChange,
  placeholder,
  clearLabel,
  ref,
}: {
  value: string
  onChange: (next: string) => void
  placeholder: string
  clearLabel: string
  ref?: Ref<HTMLInputElement>
}) {
  const inputRef = useRef<HTMLInputElement | null>(null)
  const setRef = (el: HTMLInputElement | null) => {
    inputRef.current = el
    if (typeof ref === "function") ref(el)
    else if (ref) ref.current = el
  }
  return (
    <div className="relative flex-1">
      <Search className="absolute left-2 top-1/2 h-3 w-3 -translate-y-1/2 text-muted-foreground" />
      <Input
        ref={setRef}
        value={value}
        onChange={(e) => onChange(e.target.value)}
        placeholder={placeholder}
        className="h-7 pl-6 pr-7 text-xs"
      />
      {value && (
        <button
          type="button"
          aria-label={clearLabel}
          title={clearLabel}
          // Keep focus in the field, so the next query can be typed at once.
          onPointerDown={(e) => {
            if (document.activeElement === inputRef.current) e.preventDefault()
          }}
          onClick={() => dissolveClear(inputRef.current, () => onChange(""))}
          className="absolute right-1 top-1/2 flex h-5 w-5 -translate-y-1/2 items-center justify-center rounded-full text-muted-foreground transition-[background-color,color,transform] duration-150 hover:bg-foreground/10 hover:text-foreground active:scale-90"
        >
          <X className="h-3 w-3" />
        </button>
      )}
    </div>
  )
}
