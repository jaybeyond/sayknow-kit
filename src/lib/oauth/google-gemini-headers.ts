/**
 * Headers that identify this client as Gemini CLI to Cloud Code Assist.
 *
 * Source: `packages/ai/src/providers/google-gemini-headers.ts` @
 * @sayknow-cli/ai 0.5.20, narrowed to the two values the OAuth/project
 * discovery calls need. The Antigravity system instruction and user agent in
 * the original file are not used here and were left behind.
 *
 * Upstream reads `process.platform` / `process.arch`, which the webview does
 * not expose, so the platform pair is derived from the user agent instead.
 * The value only has to be well-formed; Cloud Code Assist keys off the
 * `GeminiCLI/` prefix.
 */

export const DEFAULT_GEMINI_CLI_VERSION = "0.52.0"

function detectPlatform(): { platform: string; arch: string } {
  const ua = typeof navigator === "undefined" ? "" : navigator.userAgent
  const platform = /Mac/i.test(ua)
    ? "darwin"
    : /Win/i.test(ua)
      ? "win32"
      : /Linux/i.test(ua)
        ? "linux"
        : "unknown"
  const arch = /arm64|aarch64/i.test(ua) ? "arm64" : "x64"
  return { platform, arch }
}

export function getGeminiCliUserAgent(modelId = "gemini-3.1-pro-preview"): string {
  const { platform, arch } = detectPlatform()
  return `GeminiCLI/${DEFAULT_GEMINI_CLI_VERSION}/${modelId} (${platform}; ${arch}; terminal)`
}

export const getGeminiCliHeaders = (modelId?: string) => ({
  "User-Agent": getGeminiCliUserAgent(modelId),
  "Client-Metadata": "ideType=IDE_UNSPECIFIED,platform=PLATFORM_UNSPECIFIED,pluginType=GEMINI",
})
