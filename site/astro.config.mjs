import { defineConfig } from "astro/config"

// The public address, for canonical and hreflang links. SITE_URL overrides
// it, e.g. for a preview deployment.
export default defineConfig({
  site: process.env.SITE_URL || "https://sayknow.ai",
  output: "static",
  devToolbar: { enabled: false },
  trailingSlash: "always",
  build: {
    format: "directory",
    // Everything in files, nothing inline: the Content-Security-Policy in
    // public/_headers and vercel.json allows no inline style or script.
    inlineStylesheets: "never",
  },
  // The site imports the app's shortcut table and UI strings from ../src.
  vite: {
    server: { fs: { allow: [".."] } },
    // Scripts as files too, never inlined (see the CSP note above).
    build: { assetsInlineLimit: 0 },
  },
})
