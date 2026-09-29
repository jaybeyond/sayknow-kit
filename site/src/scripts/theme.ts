// The header switch. It flips to the palette the visitor is not looking at
// and remembers the choice; picking the same palette the system uses forgets
// it again, so the page goes back to following the system.
const KEY = "sayknow-theme"

export type Theme = "light" | "dark"

export function nextChoice(current: Theme, system: Theme): Theme | null {
  const next: Theme = current === "dark" ? "light" : "dark"
  return next === system ? null : next
}

function current(): Theme {
  return document.documentElement.dataset.theme === "dark" ? "dark" : "light"
}

function label(button: HTMLElement) {
  button.setAttribute("aria-label", current() === "dark" ? button.dataset.labelLight! : button.dataset.labelDark!)
  button.title = button.getAttribute("aria-label") ?? ""
}

export function startThemeToggle() {
  const media = window.matchMedia("(prefers-color-scheme: dark)")
  for (const button of document.querySelectorAll<HTMLElement>("[data-theme-toggle]")) {
    label(button)
    button.addEventListener("click", () => {
      const system: Theme = media.matches ? "dark" : "light"
      const choice = nextChoice(current(), system)
      try {
        if (choice) localStorage.setItem(KEY, choice)
        else localStorage.removeItem(KEY)
      } catch {
        // Not remembered; the flip still applies to this page.
      }
      const theme = choice ?? system
      document.documentElement.dataset.theme = theme
      const color = theme === "dark" ? "#121212" : "#fafaf9"
      for (const meta of document.querySelectorAll('meta[name="theme-color"]')) meta.setAttribute("content", color)
      label(button)
    })
    media.addEventListener("change", () => label(button))
  }
}
