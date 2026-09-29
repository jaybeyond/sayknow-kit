import type { Locale } from "../data/site"
import { de } from "./de"
import { en } from "./en"
import { es } from "./es"
import { fr } from "./fr"
import { ja } from "./ja"
import { ko } from "./ko"
import { LANDING } from "./landing"
import type { Copy, LocaleCopy } from "./types"
import { vi } from "./vi"
import { zh } from "./zh"

const withLanding = (locale: Locale, copy: LocaleCopy): Copy => ({ ...copy, landing: LANDING[locale] })

export const COPY: Record<Locale, Copy> = {
  ko: withLanding("ko", ko),
  en: withLanding("en", en),
  ja: withLanding("ja", ja),
  zh: withLanding("zh", zh),
  es: withLanding("es", es),
  fr: withLanding("fr", fr),
  de: withLanding("de", de),
  vi: withLanding("vi", vi),
}

export function copyFor(locale: Locale): Copy {
  return COPY[locale]
}

export type { Copy } from "./types"
