// "Error state shake" from transitions.dev (MIT): the field shakes left and
// right with a small overshoot when what was entered is rejected. Web
// Animations rather than a class, so it can replay on every failed attempt
// without remounting the field (which would drop focus).
import { prefersReducedMotion } from "./dissolve-clear"

function cssNumber(name: string, fallback: number): number {
  const v = parseFloat(getComputedStyle(document.documentElement).getPropertyValue(name))
  return Number.isFinite(v) ? v : fallback
}

export function shake(el: HTMLElement | null): void {
  if (!el || prefersReducedMotion() || typeof el.animate !== "function") return
  const d = cssNumber("--shake-distance", 6)
  const over = cssNumber("--shake-overshoot", 4)
  const a = cssNumber("--shake-dur-a", 80)
  const b = cssNumber("--shake-dur-b", 60)
  const ease = getComputedStyle(document.documentElement).getPropertyValue("--shake-ease").trim() || "cubic-bezier(0.22, 1, 0.36, 1)"
  const total = a * 2 + b * 2
  el.animate(
    [
      { transform: "translateX(0)", easing: ease, offset: 0 },
      { transform: `translateX(${d}px)`, easing: ease, offset: a / total },
      { transform: `translateX(${-d}px)`, easing: ease, offset: (a + b) / total },
      { transform: `translateX(${over}px)`, easing: ease, offset: (2 * a + b) / total },
      { transform: "translateX(0)", offset: 1 },
    ],
    { duration: total },
  )
}
