// Runs before React mounts so the first paint already has the right theme and
// window class. It used to be an inline <script>; a Content-Security-Policy
// without 'unsafe-inline' cannot allow that, so it is a module now.
try {
  const params = new URLSearchParams(window.location.search)
  if (params.get("window") === "settings") {
    document.documentElement.classList.add("settings-window")
  }
  const saved = localStorage.getItem("sayknow:theme")
  const mode = saved || "system"
  const dark =
    mode === "dark" ||
    (mode === "system" && window.matchMedia("(prefers-color-scheme: dark)").matches)
  if (dark) document.documentElement.classList.add("dark")
} catch {
  /* storage can be unavailable; the app falls back to the system theme */
}
