import { storage } from "./storage"
import type { OpenRouterModel } from "./openrouter"

export type ModelTokens = {
  promptTokens: number
  completionTokens: number
  costUsd: number
  calls: number
}

export type UsageDay = {
  /** YYYY-MM-DD in local time. */
  date: string
  calls: number
  promptTokens: number
  completionTokens: number
  costUsd: number
  /** model id -> tokens for that local day. Missing on records written
   *  before per-model tracking; those days still count toward totals. */
  models?: Record<string, ModelTokens>
}

const KEY = "usage"
const MAX_DAYS = 90

function today(): string {
  const d = new Date()
  const y = d.getFullYear()
  const m = String(d.getMonth() + 1).padStart(2, "0")
  const day = String(d.getDate()).padStart(2, "0")
  return `${y}-${m}-${day}`
}

function modelCost(
  model: OpenRouterModel | undefined,
  promptTokens: number,
  completionTokens: number,
): number {
  if (!model?.pricing) return 0
  const p = parseFloat(model.pricing.prompt ?? "0")
  const c = parseFloat(model.pricing.completion ?? "0")
  return promptTokens * p + completionTokens * c
}

export const usage = {
  list(): UsageDay[] {
    return storage.get<UsageDay[]>(KEY) ?? []
  },
  record(args: {
    modelId: string
    models: OpenRouterModel[]
    promptTokens: number
    completionTokens: number
  }) {
    const date = today()
    const all = usage.list()
    const idx = all.findIndex((d) => d.date === date)
    const found = args.models.find((m) => m.id === args.modelId)
    const cost = modelCost(found, args.promptTokens, args.completionTokens)
    const modelId = args.modelId || "unknown"
    const modelEntry: ModelTokens = {
      promptTokens: args.promptTokens,
      completionTokens: args.completionTokens,
      costUsd: cost,
      calls: 1,
    }
    if (idx >= 0) {
      const prev = all[idx]
      const models = { ...(prev.models ?? {}) }
      const existing = models[modelId]
      models[modelId] = existing
        ? {
            promptTokens: existing.promptTokens + args.promptTokens,
            completionTokens: existing.completionTokens + args.completionTokens,
            costUsd: existing.costUsd + cost,
            calls: existing.calls + 1,
          }
        : modelEntry
      all[idx] = {
        date,
        calls: prev.calls + 1,
        promptTokens: prev.promptTokens + args.promptTokens,
        completionTokens: prev.completionTokens + args.completionTokens,
        costUsd: prev.costUsd + cost,
        models,
      }
    } else {
      all.unshift({
        date,
        calls: 1,
        promptTokens: args.promptTokens,
        completionTokens: args.completionTokens,
        costUsd: cost,
        models: { [modelId]: modelEntry },
      })
    }
    // Keep newest 90 days.
    storage.set(KEY, all.slice(0, MAX_DAYS))
  },
  todaySummary(): UsageDay {
    const date = today()
    const found = usage.list().find((d) => d.date === date)
    return (
      found ?? {
        date,
        calls: 0,
        promptTokens: 0,
        completionTokens: 0,
        costUsd: 0,
        models: {},
      }
    )
  },
  monthSummary(): UsageDay {
    const now = new Date()
    const ym = `${now.getFullYear()}-${String(now.getMonth() + 1).padStart(2, "0")}`
    const days = usage.list().filter((d) => d.date.startsWith(ym))
    return days.reduce<UsageDay>(
      (acc, d) => ({
        date: ym,
        calls: acc.calls + d.calls,
        promptTokens: acc.promptTokens + d.promptTokens,
        completionTokens: acc.completionTokens + d.completionTokens,
        costUsd: acc.costUsd + d.costUsd,
        models: mergeModels(acc.models, d.models),
      }),
      {
        date: ym,
        calls: 0,
        promptTokens: 0,
        completionTokens: 0,
        costUsd: 0,
        models: {},
      },
    )
  },
  clear() {
    storage.remove(KEY)
  },
}


function mergeModels(
  a: Record<string, ModelTokens> | undefined,
  b: Record<string, ModelTokens> | undefined,
): Record<string, ModelTokens> {
  const out: Record<string, ModelTokens> = { ...(a ?? {}) }
  for (const [id, entry] of Object.entries(b ?? {})) {
    const prev = out[id]
    out[id] = prev
      ? {
          promptTokens: prev.promptTokens + entry.promptTokens,
          completionTokens: prev.completionTokens + entry.completionTokens,
          costUsd: prev.costUsd + entry.costUsd,
          calls: prev.calls + entry.calls,
        }
      : { ...entry }
  }
  return out
}

export function modelsFromDay(day: UsageDay): { model: string; promptTokens: number; completionTokens: number; tokens: number; costUsd: number; calls: number }[] {
  const out = Object.entries(day.models ?? {}).map(([model, m]) => ({
    model,
    promptTokens: m.promptTokens,
    completionTokens: m.completionTokens,
    tokens: m.promptTokens + m.completionTokens,
    costUsd: m.costUsd,
    calls: m.calls,
  }))
  out.sort((a, b) => b.tokens - a.tokens)
  return out
}

function shortenModelName(id: string): string {
  const slash = id.lastIndexOf("/")
  const name = slash >= 0 ? id.slice(slash + 1) : id
  return name.replace(/:free$/, "")
}

export { shortenModelName }
export function formatCost(usd: number): string {
  if (usd < 0.01) return `$${usd.toFixed(4)}`
  if (usd < 1) return `$${usd.toFixed(3)}`
  return `$${usd.toFixed(2)}`
}

export function formatTokens(n: number): string {
  if (n < 1000) return `${n}`
  if (n < 1_000_000) return `${(n / 1000).toFixed(1)}K`
  return `${(n / 1_000_000).toFixed(2)}M`
}
