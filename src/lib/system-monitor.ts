// Settings for the background system monitor: the figure next to the menu bar
// icon and the alerts. The Rust side does the watching; this builds what it is
// told, including notification text in the user's language (it has no i18n).

export const MENU_BAR_READOUTS = ["off", "cpu", "memory", "gpu", "temperature"] as const
export type MenuBarReadout = (typeof MENU_BAR_READOUTS)[number]

export const SYSTEM_ALERT_KINDS = ["cpu", "memory", "temperature", "storage", "battery"] as const
export type SystemAlertKind = (typeof SYSTEM_ALERT_KINDS)[number]

export type MonitorConfig = {
  readout: MenuBarReadout
  alerts: SystemAlertKind[]
  text: Record<SystemAlertKind, { title: string; body: string }>
}

/** What the Rust watch should do. Prefs come from storage written by any
 *  version of the app, so unknown values are dropped rather than sent. */
export function monitorConfig(
  readout: unknown,
  alerts: unknown,
  t: (key: string) => string,
): MonitorConfig {
  const chosen = Array.isArray(alerts) ? alerts : []
  return {
    readout: (MENU_BAR_READOUTS as readonly unknown[]).includes(readout) ? (readout as MenuBarReadout) : "off",
    alerts: SYSTEM_ALERT_KINDS.filter((kind) => chosen.includes(kind)),
    text: Object.fromEntries(
      SYSTEM_ALERT_KINDS.map((kind) => [
        kind,
        { title: t(`monitor.alert.${kind}.title`), body: t(`monitor.alert.${kind}.body`) },
      ]),
    ) as MonitorConfig["text"],
  }
}
