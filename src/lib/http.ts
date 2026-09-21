/**
 * Fetch that routes through the Tauri HTTP plugin (Rust) when running inside
 * the desktop app, and falls back to the platform fetch elsewhere.
 *
 * The plugin bypasses webview CORS policy. That was originally needed for
 * localhost endpoints, and it is what makes the ported OAuth flows work at
 * all: provider token endpoints send no `Access-Control-Allow-Origin` for
 * `tauri://localhost`, so a webview fetch would be blocked before it left.
 *
 * Extracted from `openrouter.ts` so the OAuth layer can share it without
 * importing the OpenAI-compatible client.
 */
export const httpFetch: typeof globalThis.fetch = async (input, init) => {
  if (
    typeof window !== "undefined" &&
    ("__TAURI_INTERNALS__" in window || "__TAURI__" in window)
  ) {
    const { fetch: tauriFetch } = await import("@tauri-apps/plugin-http")
    return tauriFetch(input, init)
  }
  return fetch(input, init)
}
