import { useState } from "react"
import { Download, ExternalLink, Loader2, RefreshCw } from "lucide-react"
import { Button } from "@/components/ui/button"
import { useUpdateStatus } from "@/hooks/useUpdateStatus"
import { openExternal } from "@/lib/runtime"
import { installUpdate, type InstallProgress } from "@/lib/update"
import { cn } from "@/lib/utils"

/**
 * The release check as it appears in the About panel, beside the running
 * version.
 *
 * Installing is minisign-verified by the updater plugin, which is a different
 * trust root from the ad-hoc/unsigned installers — so an in-place update is
 * safe even though downloading an installer by hand is a deliberate act. If
 * verification or the endpoint fails, the release page is still offered.
 */
export function UpdateRow({
  supported,
  t,
}: {
  supported: boolean
  t: (key: string) => string
}) {
  const { status, check } = useUpdateStatus()
  const [progress, setProgress] = useState<InstallProgress | null>(null)
  const [installError, setInstallError] = useState<string | null>(null)

  if (!supported) return null

  const installing = progress !== null
  const percent =
    progress?.phase === "downloading" && progress.total
      ? Math.min(100, Math.round((progress.downloaded / progress.total) * 100))
      : null

  async function install() {
    setInstallError(null)
    setProgress({ phase: "downloading", downloaded: 0, total: null })
    try {
      await installUpdate(setProgress)
    } catch (e) {
      setInstallError(e instanceof Error ? e.message : String(e))
      setProgress(null)
    }
  }

  return (
    <div className="space-y-1.5">
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
        <div className="flex shrink-0 items-center gap-1.5">
          {status.state === "outdated" && (
            <Button size="sm" disabled={installing} onClick={() => void install()}>
              {installing ? (
                <Loader2 className="mr-1.5 h-3.5 w-3.5 animate-spin" />
              ) : (
                <Download className="mr-1.5 h-3.5 w-3.5" />
              )}
              {installing
                ? progress?.phase === "installing"
                  ? t("update.restarting")
                  : percent == null
                    ? t("update.installing")
                    : `${percent}%`
                : t("update.install")}
            </Button>
          )}
          <Button
            variant="outline"
            size="sm"
            disabled={status.state === "checking" || installing}
            onClick={() => void check(true)}
          >
            <RefreshCw
              className={cn("mr-1.5 h-3.5 w-3.5", status.state === "checking" && "animate-spin")}
            />
            {t("update.check")}
          </Button>
        </div>
      </div>
      {installError && (
        <p className="text-[10px] text-destructive">
          {t("update.installFailed")} — {installError}
        </p>
      )}
    </div>
  )
}
