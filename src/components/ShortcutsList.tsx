import { useEffect, useState } from "react"
import { invoke } from "@tauri-apps/api/core"
import { useT, type UILocaleSetting } from "@/i18n"
import { isTauri } from "@/lib/runtime"
import {
  SHORTCUT_GROUPS,
  SHORTCUTS,
  comboKeys,
  isMacPlatform,
  type ShortcutDef,
} from "@/lib/shortcuts"

type GlobalStatus = { id: string; registered: boolean }

/** The settings page listing: every shortcut, grouped by where it works. */
export function ShortcutsList({
  uiLocale,
  mac = isMacPlatform(),
}: {
  uiLocale: UILocaleSetting
  mac?: boolean
}) {
  const { t } = useT(uiLocale)
  // Ids the OS refused at startup. Unknown (outside Tauri) means none flagged.
  const [refused, setRefused] = useState<ReadonlySet<string>>(new Set())

  useEffect(() => {
    if (!isTauri()) return
    let cancelled = false
    void invoke<GlobalStatus[]>("get_global_shortcuts")
      .then((list) => {
        if (!cancelled) setRefused(new Set(list.filter((s) => !s.registered).map((s) => s.id)))
      })
      .catch(() => {})
    return () => {
      cancelled = true
    }
  }, [])

  return (
    <div className="space-y-6">
      <p className="text-[12px] leading-relaxed text-muted-foreground">{t("shortcuts.intro")}</p>
      {SHORTCUT_GROUPS.map((group) => (
        <section key={group} aria-labelledby={`shortcuts-${group}`}>
          <h3
            id={`shortcuts-${group}`}
            className="mb-2 text-[11px] font-medium uppercase tracking-wide text-muted-foreground"
          >
            {t(`shortcuts.group.${group}`)}
          </h3>
          <ul className="divide-y rounded-lg border">
            {SHORTCUTS.filter((s) => s.group === group).map((s) => (
              <ShortcutRow
                key={s.id}
                def={s}
                label={t(s.label)}
                keys={comboKeys(s.combo, mac)}
                refusedNote={refused.has(s.id) ? t("shortcuts.unregistered") : null}
              />
            ))}
          </ul>
        </section>
      ))}
    </div>
  )
}

function ShortcutRow({
  def,
  label,
  keys,
  refusedNote,
}: {
  def: ShortcutDef
  label: string
  keys: string[]
  refusedNote: string | null
}) {
  return (
    <li data-shortcut={def.id} className="flex items-center justify-between gap-4 px-3 py-2">
      <div className="min-w-0">
        <div className="text-[13px]">{label}</div>
        {refusedNote && (
          <div className="mt-0.5 text-[11px] text-amber-700 dark:text-amber-400">{refusedNote}</div>
        )}
      </div>
      <kbd
        aria-label={keys.join(" ")}
        className={
          "flex shrink-0 items-center gap-1 font-sans" + (refusedNote ? " opacity-50" : "")
        }
      >
        {keys.map((k, i) => (
          <span
            key={`${k}-${i}`}
            aria-hidden
            className="inline-flex h-6 min-w-6 items-center justify-center rounded-md border border-b-2 bg-muted/60 px-1.5 text-[11px] font-medium text-foreground"
          >
            {k}
          </span>
        ))}
      </kbd>
    </li>
  )
}
