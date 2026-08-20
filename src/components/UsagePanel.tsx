import { useMemo } from "react"
import { Gauge, RefreshCw } from "lucide-react"
import { Button } from "@/components/ui/button"
import { useAgentUsage } from "@/hooks/useAgentUsage"
import { useUsage } from "@/hooks/useUsage"
import type { Settings } from "@/hooks/useSettings"
import { useT } from "@/i18n"
import {
  activeBlock,
  burnRate,
  formatTokens,
  formatUsd,
  modelsInWindow,
  recentHours,
  windowsOf,
  type AgentReport,
  type Block,
  type RateLimits,
  type RateWindow,
} from "@/lib/agent-usage"
import { formatCost, formatTokens as formatAppTokens } from "@/lib/usage"
import { cn } from "@/lib/utils"

type Props = {
  settings: Settings
  /** Only the visible tab scans; the scan is disk-heavy. */
  active: boolean
}

export function UsagePanel({ settings, active }: Props) {
  const { t, locale } = useT(settings.uiLocale)
  const { agents, loading, error, scannedAt, supported, refresh } =
    useAgentUsage(active)
  const appUsage = useUsage()

  // Recomputed per scan rather than per render: reading the clock during
  // render is impure, and "3 minutes ago" only needs to be as fresh as the
  // data it labels.
  const lastActive = useMemo(() => {
    const relative = new Intl.RelativeTimeFormat(locale, { numeric: "auto" })
    const labels = new Map<string, string | null>()
    // Nothing has been scanned yet, so there is nothing to label.
    if (scannedAt === null) return labels
    const now = scannedAt
    for (const a of agents) {
      labels.set(a.id, label(a.last_ts))
    }
    return labels

    function label(iso: string | null): string | null {
      if (!iso) return null
      const ms = Date.parse(iso)
      if (Number.isNaN(ms)) return null
      const diffMin = Math.round((ms - now) / 60000)
      // Intl renders 0 minutes as "this minute" / "현재 분", which reads like
      // a label rather than a timestamp. Say "just now" ourselves.
      if (diffMin === 0) return t("usage.justNow")
      if (Math.abs(diffMin) < 60) return relative.format(diffMin, "minute")
      const diffHour = Math.round(diffMin / 60)
      if (Math.abs(diffHour) < 24) return relative.format(diffHour, "hour")
      return relative.format(Math.round(diffHour / 24), "day")
    }
  }, [agents, scannedAt, locale, t])

  if (!supported) {
    return (
      <div className="flex h-full items-center justify-center p-6 text-center text-xs text-muted-foreground">
        {t("usage.desktopOnly")}
      </div>
    )
  }

  return (
    <div className="flex h-full flex-col">
      <div className="flex shrink-0 items-center justify-between gap-2 border-b px-3 py-2">
        <div className="flex items-center gap-1.5 text-xs font-medium">
          <Gauge className="h-3.5 w-3.5" />
          {t("usage.heading")}
        </div>
        <Button
          variant="ghost"
          size="sm"
          className="h-6 px-2 text-[11px]"
          onClick={() => void refresh(true)}
          disabled={loading}
          title={t("usage.refresh")}
        >
          <RefreshCw className={cn("h-3 w-3", loading && "animate-spin")} />
        </Button>
      </div>

      <div className="flex-1 space-y-2 overflow-y-auto p-2.5">
        {/* This app's own API spend — separate from the CLI agents below. */}
        <section className="rounded-lg border bg-muted/30 p-2.5">
          <div className="mb-1.5 text-[11px] font-medium">{t("usage.appOwn")}</div>
          <div className="grid grid-cols-2 gap-2">
            <Stat
              label={t("usage.window.today")}
              value={formatAppTokens(
                appUsage.today.promptTokens + appUsage.today.completionTokens,
              )}
              sub={`${appUsage.today.calls} · ${formatCost(appUsage.today.costUsd)}`}
            />
            <Stat
              label={t("usage.window.month")}
              value={formatAppTokens(
                appUsage.month.promptTokens + appUsage.month.completionTokens,
              )}
              sub={`${appUsage.month.calls} · ${formatCost(appUsage.month.costUsd)}`}
            />
          </div>
        </section>

        {agents.map((a) => (
          <AgentCard
            key={a.id}
            agent={a}
            t={t}
            nowMs={scannedAt ?? 0}
            lastActive={lastActive.get(a.id) ?? null}
          />
        ))}

        {agents.length === 0 && loading && (
          <div className="py-8 text-center text-xs text-muted-foreground">
            {t("usage.scanning")}
          </div>
        )}

        {error && (
          <div className="rounded-md border border-destructive/40 bg-destructive/10 px-2.5 py-2 text-[11px] text-destructive">
            {error}
          </div>
        )}

        <p className="px-0.5 pt-0.5 text-[10px] leading-relaxed text-muted-foreground">
          {t("usage.sourceNote")}
        </p>
      </div>
    </div>
  )
}

function AgentCard({
  agent,
  t,
  nowMs,
  lastActive,
}: {
  agent: AgentReport
  t: (k: string) => string
  /** Scan timestamp used as the reference clock for every window. */
  nowMs: number
  lastActive: string | null
}) {
  const w = useMemo(() => windowsOf(agent.hours, nowMs), [agent.hours, nowMs])
  // Subscription usage is metered in 5-hour billing blocks, so the headline is
  // the block you're actually inside — not a rolling 5-hour sum.
  const block = useMemo(
    () => activeBlock(agent.hours, nowMs),
    [agent.hours, nowMs],
  )
  // Same 30-day window as the widest total above, so the chips can never sum
  // to more than the number they sit under.
  const models = useMemo(
    () => modelsInWindow(agent.model_days, nowMs, 30),
    [agent.model_days, nowMs],
  )
  const spark = useMemo(
    () => recentHours(agent.hours, nowMs, 24),
    [agent.hours, nowMs],
  )
  const peak = Math.max(1, ...spark.map((s) => s.bucket.total))
  // Gate on the whole scan window, not on the 30-day total: an agent last used
  // 38 days ago still has real numbers to show, and hiding them behind
  // "no records" while the scan window is 45 days was simply wrong.
  const hasAny = Object.keys(agent.hours).length > 0

  return (
    <section className="rounded-lg border p-2.5">
      <div className="mb-2 flex items-baseline justify-between gap-2">
        <span className="text-xs font-medium">{agent.label}</span>
        <span className="text-[10px] text-muted-foreground">
          {/* Last activity is the most useful fact even when every window
              reads zero, so it outranks the empty-state label. */}
          {lastActive ??
            (agent.detected ? t("usage.noRecent") : t("usage.notDetected"))}
        </span>
      </div>

      {agent.detected && hasAny ? (
        <>
          {agent.rate_limits ? (
            <RateLimitRows limits={agent.rate_limits} t={t} nowMs={nowMs} />
          ) : (
            <BlockRow
              block={block}
              nowMs={nowMs}
              hasCost={agent.has_cost}
              t={t}
            />
          )}

          <div className="mt-2 grid grid-cols-3 gap-1.5">
            <Stat
              label={t("usage.window.today")}
              value={formatTokens(w.today.total)}
              sub={agent.has_cost ? formatUsd(w.today.cost_usd) : undefined}
            />
            <Stat
              label={t("usage.window.week")}
              value={formatTokens(w.week.total)}
              sub={agent.has_cost ? formatUsd(w.week.cost_usd) : undefined}
            />
            <Stat
              label={t("usage.window.month")}
              value={formatTokens(w.month.total)}
              sub={agent.has_cost ? formatUsd(w.month.cost_usd) : undefined}
            />
          </div>

          {/* 24h shape — relative bars, no axis. Enough to spot a burn spike. */}
          <div className="mt-2 flex h-6 items-end gap-[1px]">
            {spark.map((s) => (
              <div
                key={s.key}
                title={`${s.key}Z · ${formatTokens(s.bucket.total)}`}
                className="flex-1 rounded-sm bg-primary/25"
                style={{
                  height: `${Math.max(2, (s.bucket.total / peak) * 100)}%`,
                }}
              />
            ))}
          </div>

          {models.length > 0 && (
            <div className="mt-2 flex flex-wrap items-center gap-1">
              <span className="text-[10px] text-muted-foreground">
                {t("usage.modelsWindow")}
              </span>
              {models.slice(0, 3).map((m) => (
                <span
                  key={m.model}
                  className="rounded bg-muted px-1.5 py-0.5 text-[10px] text-muted-foreground"
                >
                  {m.model} · {formatTokens(m.tokens)}
                </span>
              ))}
            </div>
          )}

          {!agent.has_cost && (
            <p className="mt-1.5 text-[10px] text-muted-foreground">
              {t("usage.tokensOnly")}
            </p>
          )}
        </>
      ) : (
        <p className="text-[11px] text-muted-foreground">
          {agent.detected ? t("usage.noRecentBody") : t("usage.notDetectedBody")}
        </p>
      )}
    </section>
  )
}

/** Remaining time rendered as "2시간 12분" / "12분". */
function formatRemaining(ms: number, t: (k: string) => string): string {
  if (ms <= 0) return t("usage.block.expired")
  const totalMin = Math.floor(ms / 60_000)
  const h = Math.floor(totalMin / 60)
  const m = totalMin % 60
  return h > 0 ? `${h}${t("usage.unit.hour")} ${m}${t("usage.unit.min")}` : `${m}${t("usage.unit.min")}`
}

/** The 5-hour billing block the user is currently inside, derived from
 *  timestamps because these agents log no quota headers. */
function BlockRow({
  block,
  nowMs,
  hasCost,
  t,
}: {
  block: Block | null
  nowMs: number
  hasCost: boolean
  t: (k: string) => string
}) {
  if (!block) {
    return (
      <div className="rounded-md bg-muted/40 px-2 py-1.5">
        <div className="text-[10px] text-muted-foreground">
          {t("usage.block.label")}
        </div>
        <div className="text-[11px] text-muted-foreground">
          {t("usage.block.idle")}
        </div>
      </div>
    )
  }
  const elapsed = nowMs - block.startMs
  const span = block.endMs - block.startMs
  const pct = Math.min(100, Math.max(0, (elapsed / span) * 100))
  const rate = burnRate(block, nowMs)
  return (
    <div className="rounded-md bg-muted/40 px-2 py-1.5">
      <div className="flex items-baseline justify-between gap-2">
        <span className="text-[10px] text-muted-foreground">
          {t("usage.block.label")}
        </span>
        <span className="text-[10px] tabular-nums text-muted-foreground">
          {t("usage.block.remaining")} {formatRemaining(block.endMs - nowMs, t)}
        </span>
      </div>
      <div className="mt-0.5 flex items-baseline gap-2">
        <span className="text-sm font-semibold tabular-nums text-primary">
          {formatTokens(block.bucket.total)}
        </span>
        {hasCost && (
          <span className="text-[11px] tabular-nums text-muted-foreground">
            {formatUsd(block.bucket.cost_usd)}
          </span>
        )}
        <span className="ml-auto text-[10px] tabular-nums text-muted-foreground">
          {formatTokens(Math.round(rate))}/{t("usage.unit.min")}
        </span>
      </div>
      {/* Elapsed share of the 5-hour window, not a quota bar — these agents
          don't publish a quota to measure against. */}
      <div className="mt-1 h-1 overflow-hidden rounded-full bg-border">
        <div className="h-full bg-primary/50" style={{ width: `${pct}%` }} />
      </div>
    </div>
  )
}

/** Name a quota window from its length rather than its position in the
 *  payload — 300 minutes is the 5-hour window, 10080 the weekly one. */
function windowLabel(minutes: number, t: (k: string) => string): string {
  if (minutes === 300) return t("usage.limit.short")
  if (minutes === 10080) return t("usage.limit.long")
  if (minutes % 1440 === 0) return `${minutes / 1440}${t("usage.unit.day")}`
  if (minutes % 60 === 0) return `${minutes / 60}${t("usage.unit.hour")}`
  return `${minutes}${t("usage.unit.min")}`
}

/** Provider-reported quota windows. Codex writes real percentages into its
 *  logs, so we show those verbatim rather than guessing from token counts. */
function RateLimitRows({
  limits,
  t,
  nowMs,
}: {
  limits: RateLimits
  t: (k: string) => string
  nowMs: number
}) {
  // Label by the window the provider reports, never by field position:
  // Codex puts the weekly window in `primary` when no 5-hour window applies,
  // so keying off "primary = 5 hours" mislabels the row outright.
  const rows: { label: string; w: RateWindow }[] = []
  for (const w of [limits.primary, limits.secondary]) {
    if (w) rows.push({ label: windowLabel(w.window_minutes, t), w })
  }

  // Every window is judged on its own reset time. Once that has passed the
  // quota has rolled over, so the recorded percentage describes a window that
  // no longer exists — it must not be drawn as if it were the current level.
  const allExpired = rows.every((r) => r.w.resets_at * 1000 < nowMs)

  return (
    <div className="rounded-md bg-muted/40 px-2 py-1.5">
      <div className="mb-1 flex items-baseline justify-between gap-2">
        <span className="text-[10px] text-muted-foreground">
          {allExpired ? t("usage.limit.lastSeen") : t("usage.limit.label")}
          {limits.plan_type ? ` · ${limits.plan_type}` : ""}
        </span>
        <span className="text-[10px] text-muted-foreground">
          {limits.captured_at.slice(0, 10)}
        </span>
      </div>
      {rows.map((r) => {
        const expired = r.w.resets_at * 1000 < nowMs
        return (
          <div key={r.label} className="mb-1 last:mb-0">
            <div className="flex items-baseline justify-between gap-2 text-[10px]">
              <span className="text-muted-foreground">{r.label}</span>
              <span
                className={cn(
                  "tabular-nums",
                  expired && "text-muted-foreground line-through",
                )}
              >
                {r.w.used_percent.toFixed(1)}%
              </span>
            </div>
            {expired ? (
              // No bar: a filled bar reads as "this is where you are now".
              <div className="text-[10px] text-muted-foreground">
                {t("usage.limit.notCurrent")}
              </div>
            ) : (
              <>
                <div className="mt-0.5 h-1 overflow-hidden rounded-full bg-border">
                  <div
                    className="h-full bg-primary"
                    style={{ width: `${Math.min(100, r.w.used_percent)}%` }}
                  />
                </div>
                <div className="mt-0.5 text-right text-[10px] tabular-nums text-muted-foreground">
                  {t("usage.limit.resetIn")}{" "}
                  {formatRemaining(r.w.resets_at * 1000 - nowMs, t)}
                </div>
              </>
            )}
          </div>
        )
      })}
      {allExpired && (
        <p className="mt-1 text-[10px] leading-snug text-muted-foreground">
          {t("usage.limit.refreshHint")}
        </p>
      )}
    </div>
  )
}

function Stat({
  label,
  value,
  sub,
  accent,
}: {
  label: string
  value: string
  sub?: string
  accent?: boolean
}) {
  return (
    <div>
      <div className="text-[10px] text-muted-foreground">{label}</div>
      <div
        className={cn(
          "text-sm font-semibold tabular-nums",
          accent && "text-primary",
        )}
      >
        {value}
      </div>
      {sub && (
        <div className="text-[10px] tabular-nums text-muted-foreground">{sub}</div>
      )}
    </div>
  )
}
