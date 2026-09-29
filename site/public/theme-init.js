// Runs in <head>, before first paint, so the page never flashes the wrong
// palette. A file rather than an inline script: the site's CSP allows no
// inline script. The switch itself is wired in src/scripts/theme.ts.
;(function () {
  var KEY = "sayknow-theme"
  var media = window.matchMedia("(prefers-color-scheme: dark)")
  function stored() {
    try {
      var v = localStorage.getItem(KEY)
      return v === "light" || v === "dark" ? v : null
    } catch (e) {
      return null
    }
  }
  function apply() {
    var theme = stored() || (media.matches ? "dark" : "light")
    document.documentElement.setAttribute("data-theme", theme)
    var color = theme === "dark" ? "#121212" : "#fafaf9"
    var metas = document.querySelectorAll('meta[name="theme-color"]')
    for (var i = 0; i < metas.length; i++) metas[i].setAttribute("content", color)
  }
  apply()
  // Follow the system while the visitor has not picked one.
  media.addEventListener("change", function () {
    if (!stored()) apply()
  })
  document.addEventListener("DOMContentLoaded", apply)
})()
