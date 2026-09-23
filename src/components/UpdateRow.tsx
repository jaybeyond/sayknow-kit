import { ExternalLink, RefreshCw } from "lucide-react"
import { Button } from "@/components/ui/button"
import { useUpdateStatus } from "@/hooks/useUpdateStatus"
import { openExternal } from "@/lib/runtime"
import { cn } from "@/lib/utils"

/**
 * The release check as it appears in the About panel, beside the running
 * version. The installers are ad-hoc signed (macOS) and unsigned (Windows), so
 * this never swaps the app underneath the user: it names the newer version and
 * opens the release page, checksums included.
 */
export function UpdateRow({
  supported,
  t,
}: {
  supported: boolean
  t: (key: string) => string
}) {
  const { status, check } = useUpdateStatus()
  // The web preview has no installed version to compare against.
  if (!supported) return null

  return (
    <div className="flex items-center justify-between gap-3">
      <div className="min-w-0 text-xs text-muted-foreground">
        {status.state === "checking" && t("update.checking")}
        {status.state === "current" && t("update.upToDate")}
        {status.state === "failed" && t("update.failed")}
        {status.state === "outdated" && (
          <button
            type="button"
            onClick={() => openExternal(status.url)}
            className="inline-flex items-center gap-1 text-foreground hover:text-foreground/80"
          >
            {t("update.available").replace("{version}", status.latest)}
            <ExternalLink className="h-3 w-3" />
          </button>
        )}
      </div>
      <Button
        variant="outline"
        size="sm"
        disabled={status.state === "checking"}
        onClick={() => void check(true)}
      >
        <RefreshCw
          className={cn("mr-1.5 h-3.5 w-3.5", status.state === "checking" && "animate-spin")}
        />
        {t("update.check")}
      </Button>
    </div>
  )
}
