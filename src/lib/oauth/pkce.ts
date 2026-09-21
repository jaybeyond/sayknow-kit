/**
 * PKCE verifier/challenge generation.
 *
 * Source: `packages/ai/src/utils/oauth/pkce.ts` @ @sayknow-cli/ai 0.5.20.
 * The algorithm is unchanged. The only edit is base64url encoding: upstream
 * runs on Bun and reaches for `Buffer`, which is not present in the webview,
 * so the two conversions go through `btoa` instead.
 */

function base64url(bytes: Uint8Array): string {
  let binary = ""
  for (const byte of bytes) binary += String.fromCharCode(byte)
  return btoa(binary).replace(/\+/g, "-").replace(/\//g, "_").replace(/=+$/, "")
}

export async function generatePKCE(): Promise<{ verifier: string; challenge: string }> {
  // Generate random verifier
  const verifierBytes = new Uint8Array(96)
  crypto.getRandomValues(verifierBytes)
  const verifier = base64url(verifierBytes)

  // Compute SHA-256 challenge
  const encoder = new TextEncoder()
  const data = encoder.encode(verifier)
  const hashBuffer = await crypto.subtle.digest("SHA-256", data)
  const challenge = base64url(new Uint8Array(hashBuffer))

  return { verifier, challenge }
}
