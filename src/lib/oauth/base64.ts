/**
 * base64 / base64url decoding for the webview.
 *
 * Upstream (sayknow-cli) runs on Bun and decodes JWT segments with
 * `Buffer.from(value, "base64")` / `"base64url"`. Neither exists here, and
 * `atob` rejects both the URL alphabet and missing padding, so every ported
 * flow funnels through this instead of repeating the conversion.
 */

/** Decode standard base64 *or* base64url, with or without padding, to utf8. */
export function decodeBase64Url(value: string): string {
  const normalized = value.replace(/-/g, "+").replace(/_/g, "/")
  const padded = normalized.padEnd(Math.ceil(normalized.length / 4) * 4, "=")
  const binary = atob(padded)
  const bytes = Uint8Array.from(binary, (char) => char.charCodeAt(0))
  return new TextDecoder().decode(bytes)
}

/**
 * Decode a JWT's payload segment. Returns `null` for anything that is not a
 * three-part token with decodable JSON — callers treat that as "no identity",
 * never as an error, matching upstream.
 */
export function decodeJwt<T = Record<string, unknown>>(token: string): T | null {
  try {
    const parts = token.split(".")
    if (parts.length !== 3) return null
    const payload = parts[1] ?? ""
    return JSON.parse(decodeBase64Url(payload)) as T
  } catch {
    return null
  }
}
