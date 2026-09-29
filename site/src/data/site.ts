// Facts the whole site shares. Anything the app itself defines (languages,
// shortcut table, UI labels) is imported from the app's own source, so the
// site cannot describe a different app than the one that ships.
import { UI_LOCALES, UI_LOCALE_LABELS, type UILocale } from "../../../src/i18n/strings"

export const REPO = "jaybeyond/sayknow-kit"
export const REPO_URL = `https://github.com/${REPO}`
export const RELEASES_URL = `${REPO_URL}/releases`
export const ISSUES_URL = `${REPO_URL}/issues`

export const LOCALES = UI_LOCALES
export type Locale = UILocale
export const LOCALE_NAMES = UI_LOCALE_LABELS

/** BCP 47 tags for <html lang> and hreflang. The app's Chinese is Simplified. */
export const HTML_LANG: Record<Locale, string> = {
  ko: "ko",
  en: "en",
  ja: "ja",
  zh: "zh-Hans",
  es: "es",
  fr: "fr",
  de: "de",
  vi: "vi",
}

export function isLocale(value: string): value is Locale {
  return (LOCALES as readonly string[]).includes(value)
}

/** Every page that exists in every language, as paths under /{lang}/. */
export const FEATURE_IDS = ["translate", "chat", "clipboard", "system", "usage", "providers"] as const
export type FeatureId = (typeof FEATURE_IDS)[number]

export function localePath(locale: Locale, path = ""): string {
  const clean = path.replace(/^\/+|\/+$/g, "")
  return clean ? `/${locale}/${clean}/` : `/${locale}/`
}
