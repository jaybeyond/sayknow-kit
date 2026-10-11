import type { LucideIcon } from "lucide-react"
import { Label } from "@/components/ui/label"

/** Settings page heading: icon plus title, draggable like the title bar. */
export function SectionHeader({ icon: Icon, title }: { icon: LucideIcon; title: string }) {
  return (
    <div className="flex items-center gap-2 pb-2" data-tauri-drag-region>
      <Icon className="h-4 w-4 text-muted-foreground" />
      <h2 className="text-base font-semibold">{title}</h2>
    </div>
  )
}

/** One settings row: label and optional description left, control right. */
export function Row({
  label,
  description,
  children,
}: {
  label: string
  description?: string
  children: React.ReactNode
}) {
  return (
    <div className="flex items-start justify-between gap-4">
      <div className="flex-1">
        <Label className="text-sm">{label}</Label>
        {description && (
          <p className="mt-0.5 text-[11px] leading-relaxed text-muted-foreground">
            {description}
          </p>
        )}
      </div>
      <div className="shrink-0">{children}</div>
    </div>
  )
}
