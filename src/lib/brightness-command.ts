import type { DisplayRow } from "@/lib/tools-store"

/**
 * Which Tauri command actually changes the light on this card.
 *
 * The built-in panel and an external monitor are driven by different
 * mechanisms and must never share one:
 *
 * - External: `set_display_brightness` speaks DDC over the cable.
 * - Built-in: the panel has no DDC. `set_builtin_backlight` writes the real
 *   backlight through the same Control Center control the F1/F2 keys drive.
 *   Sending the built-in through the DDC command lands in the gamma dimmer,
 *   which only darkens the picture — the backlight, and the keys' own
 *   level, never move. That is the "externals work, built-in does not" bug.
 *
 * Lives outside the component file so that file exports components only,
 * which is what keeps fast refresh working.
 */
export function brightnessCommand(
  display: Pick<DisplayRow, "id" | "kind" | "method" | "controllable">,
):
  | { command: "set_builtin_backlight" }
  | { command: "set_display_brightness"; id: string }
  | null {
  if (!display.controllable) return null
  if (display.kind === "builtin") {
    return { command: "set_builtin_backlight" }
  }
  return { command: "set_display_brightness", id: display.id }
}
