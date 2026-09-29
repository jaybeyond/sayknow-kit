// Small building blocks for the transitions.dev (MIT) recipes used across the
// app. The motion itself lives in index.css under "transitions.dev motion";
// these only supply the markup each recipe expects.
import type { ReactNode } from "react"
import { cn } from "@/lib/utils"

/** "Icon swap": both icons stay mounted in one slot and cross-fade with a
 *  blur and a scale, so a state change reads as one icon turning into the
 *  other rather than a hard cut. */
export function IconSwap({ on, a, b, className }: { on: boolean; a: ReactNode; b: ReactNode; className?: string }) {
  return (
    <span className={cn("t-icon-swap", className)} data-state={on ? "b" : "a"}>
      <span className="t-icon" data-icon="a" aria-hidden={on}>
        {a}
      </span>
      <span className="t-icon" data-icon="b" aria-hidden={!on}>
        {b}
      </span>
    </span>
  )
}

/** "Shimmer text": an in-progress label with a highlight sweeping across it,
 *  so waiting reads as alive without a spinner. */
export function Shimmer({ text, className }: { text: string; className?: string }) {
  return (
    <span className={cn("t-shimmer", className)} data-text={text}>
      {text}
    </span>
  )
}
