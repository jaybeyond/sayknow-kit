// "Input clear with dissolve", adapted from transitions.dev (MIT,
// github.com/Jakubantalik/transitions.dev): the cleared text drops, blurs and
// fades while a soft streak lights up under each word, and the placeholder
// falls in from above.
//
// Two changes from the original. It runs on a throwaway overlay fixed over the
// field, so React keeps sole ownership of the field's own DOM. And it measures
// each word where the browser actually laid it out (a span per word), instead
// of summing canvas text widths, so it also works on multi-line, scrolled
// textareas. Timings come from the --clear-* / --glow-* variables in
// index.css; the motion runs on Web Animations rather than a frame loop.

type Field = HTMLInputElement | HTMLTextAreaElement

const MAX_GLOW_WORDS = 80

function cssNumber(name: string, fallback: number): number {
  const v = parseFloat(getComputedStyle(document.documentElement).getPropertyValue(name))
  return Number.isFinite(v) ? v : fallback
}

function cssEasing(name: string, fallback: string): string {
  const v = getComputedStyle(document.documentElement).getPropertyValue(name).trim()
  return v || fallback
}

export function prefersReducedMotion(): boolean {
  return typeof window.matchMedia === "function" && window.matchMedia("(prefers-reduced-motion: reduce)").matches
}

/** Words and the whitespace between them, in order; whitespace is kept so the
 *  mirror wraps exactly like the field did. */
export function splitWords(text: string): { text: string; word: boolean }[] {
  return text
    .split(/(\s+)/)
    .filter((part) => part.length > 0)
    .map((part) => ({ text: part, word: /\S/.test(part) }))
}

/** Where in the envelope the glow peaks, as a keyframe offset. */
export function glowKeyframes(total: number, delay: number, peakAt: number, opacity: number): Keyframe[] {
  const start = Math.min(0.95, Math.max(0, delay / total))
  const peak = start + (1 - start) * Math.min(0.95, Math.max(0.01, peakAt))
  return [
    { opacity: 0, offset: 0 },
    { opacity: 0, offset: start },
    { opacity, offset: peak },
    { opacity: 0, offset: 1 },
  ]
}

/** The same per-word streak stack as the original: four soft ellipses under
 *  each word, anchored to the bottom of that word's line. */
export function streakLayers(
  words: { cx: number; bottom: number; width: number }[],
  rgb: string,
  spread: number,
): string {
  const layers: string[] = []
  for (const w of words.slice(0, MAX_GLOW_WORDS)) {
    const hw = Math.max(w.width * 0.45, 8) * spread
    for (const [dx, rwm, rh, a] of [
      [0, 0.8, 7, 0.22],
      [hw * 0.45, 0.55, 8, 0.18],
      [-hw * 0.4, 0.65, 6, 0.16],
      [hw * 0.15, 0.9, 5, 0.14],
    ]) {
      layers.push(
        `radial-gradient(ellipse ${Math.max(hw * rwm, 2).toFixed(1)}px ${rh}px at ${(w.cx + dx).toFixed(1)}px ${w.bottom.toFixed(1)}px, rgba(${rgb},${a}), transparent)`,
      )
    }
  }
  return layers.join(", ")
}

function copyTextStyle(from: CSSStyleDeclaration, to: CSSStyleDeclaration) {
  for (const prop of [
    "fontFamily",
    "fontSize",
    "fontWeight",
    "fontStyle",
    "lineHeight",
    "letterSpacing",
    "textAlign",
    "textIndent",
    "wordBreak",
    "overflowWrap",
    "tabSize",
    "paddingTop",
    "paddingRight",
    "paddingBottom",
    "paddingLeft",
    "borderTopWidth",
    "borderRightWidth",
    "borderBottomWidth",
    "borderLeftWidth",
  ] as const) {
    to[prop] = from[prop]
  }
  to.borderStyle = "solid"
  to.borderColor = "transparent"
  to.boxSizing = "border-box"
}

/**
 * Clear `field` with the dissolve. `clear` must empty the field's value (a
 * React state setter, typically); it runs straight away either way, so the
 * visual is never on the critical path. Without motion (reduced motion, empty
 * field, no Web Animations) it is just `clear()`.
 */
export function dissolveClear(field: Field | null, clear: () => void): void {
  const text = field?.value ?? ""
  if (!field || !text || prefersReducedMotion() || typeof field.animate !== "function") {
    clear()
    return
  }

  const rect = field.getBoundingClientRect()
  const fieldStyle = getComputedStyle(field)
  const multiline = field instanceof HTMLTextAreaElement
  const isDark = document.documentElement.classList.contains("dark")

  // Overlay: the field's own box, fixed on screen, clipping everything inside.
  const overlay = document.createElement("div")
  overlay.setAttribute("aria-hidden", "true")
  overlay.className = "t-clear-overlay"
  Object.assign(overlay.style, {
    left: `${rect.left}px`,
    top: `${rect.top}px`,
    width: `${rect.width}px`,
    height: `${rect.height}px`,
    borderRadius: fieldStyle.borderRadius,
  })

  // Mirror: the text as it sat in the field, one span per word.
  const mirror = document.createElement("div")
  mirror.className = "t-clear-mirror"
  copyTextStyle(fieldStyle, mirror.style)
  mirror.style.color = fieldStyle.color
  mirror.style.whiteSpace = multiline ? "pre-wrap" : "pre"
  if (!multiline) {
    mirror.style.display = "flex"
    mirror.style.alignItems = "center"
  }
  const inner = document.createElement("div")
  inner.style.transform = multiline
    ? `translateY(${-field.scrollTop}px)`
    : `translateX(${-field.scrollLeft}px)`
  const spans: HTMLSpanElement[] = []
  for (const part of splitWords(text)) {
    const span = document.createElement("span")
    span.textContent = part.text
    inner.appendChild(span)
    if (part.word) spans.push(span)
  }
  mirror.appendChild(inner)

  // Placeholder stand-in that falls in; the real one is hidden until the end
  // and sits in exactly the same place, so the hand-off is invisible.
  const placeholderText = field.placeholder
  const phold = document.createElement("div")
  phold.className = "t-clear-mirror"
  copyTextStyle(fieldStyle, phold.style)
  phold.style.whiteSpace = multiline ? "pre-wrap" : "pre"
  phold.style.color = getComputedStyle(field, "::placeholder").color
  if (!multiline) {
    phold.style.display = "flex"
    phold.style.alignItems = "center"
  }
  phold.textContent = placeholderText

  const glow = document.createElement("div")
  glow.className = "t-clear-glow"
  glow.style.mixBlendMode = isDark ? "screen" : "multiply"

  overlay.append(glow, mirror, phold)
  document.body.appendChild(overlay)

  // Measure every visible word where it actually wrapped.
  const box = overlay.getBoundingClientRect()
  const words = spans
    .map((s) => s.getBoundingClientRect())
    .filter((r) => r.width > 0 && r.bottom > box.top && r.top < box.bottom)
    .map((r) => ({ cx: r.left - box.left + r.width / 2, bottom: r.bottom - box.top, width: r.width }))
  glow.style.background = streakLayers(words, isDark ? "255,255,255" : "0,0,0", cssNumber("--glow-spread", 1.5))

  field.classList.add("t-clear-hide-placeholder")
  clear()

  const total = cssNumber("--clear-dur", 1000)
  const outDur = cssNumber("--clear-out-dur", 400)
  const inDur = cssNumber("--clear-in-dur", 400)
  const outFly = cssNumber("--clear-out-fly", 12)
  const inFly = cssNumber("--clear-in-fly", 12)
  const blur = cssNumber("--clear-blur", 2)
  const outEase = cssEasing("--clear-out-ease", "cubic-bezier(0.22, 1, 0.36, 1)")
  const inEase = cssEasing("--clear-in-ease", "cubic-bezier(0.22, 1, 0.36, 1)")
  const glowOpacity = cssNumber(isDark ? "--glow-opacity-dark" : "--glow-opacity", isDark ? 0.85 : 0.42)

  mirror.animate(
    [
      { transform: "translateY(0)", opacity: 1, filter: "blur(0)" },
      { transform: `translateY(${outFly}px)`, opacity: 0, filter: `blur(${blur}px)` },
    ],
    { duration: outDur, easing: outEase, fill: "forwards" },
  )
  phold.animate(
    [
      { transform: `translateY(${-inFly}px)`, opacity: 0.9, filter: `blur(${blur}px)` },
      { transform: "translateY(0)", opacity: 1, filter: "blur(0)" },
    ],
    { duration: inDur, easing: inEase, fill: "forwards" },
  )
  const glowAnim = glow.animate(
    glowKeyframes(total, cssNumber("--glow-delay", 50), cssNumber("--glow-peak-at", 0.15), glowOpacity),
    { duration: total, easing: "linear", fill: "forwards" },
  )

  let done = false
  const finish = () => {
    if (done) return
    done = true
    field.classList.remove("t-clear-hide-placeholder")
    overlay.remove()
  }
  glowAnim.finished.then(finish, finish)
  // The field moving (resize, tab switch) would leave the overlay stranded;
  // typing again means the user has moved on. Either way, end it now.
  field.addEventListener("input", finish, { once: true })
  window.addEventListener("resize", finish, { once: true })
  window.setTimeout(finish, total + 200)
}
