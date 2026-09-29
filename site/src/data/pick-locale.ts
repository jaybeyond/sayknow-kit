/**
 * The site language for a browser's preference list: the first preference
 * whose primary language the site speaks (any Chinese goes to the Simplified
 * pages, the only Chinese there is), otherwise English.
 */
export function pickLocale(preferences: readonly string[], supported: readonly string[]): string {
  for (const pref of preferences) {
    const primary = pref.toLowerCase().split("-")[0]
    if (supported.includes(primary)) return primary
  }
  return supported.includes("en") ? "en" : (supported[0] ?? "en")
}
