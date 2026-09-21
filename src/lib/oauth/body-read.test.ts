import { describe, expect, it } from "vitest"

/**
 * The Tauri HTTP plugin hands back a Response whose body is a native resource
 * handle, not a buffered stream. Reading it twice — including via `clone()` —
 * fails at runtime with "The resource id ... is invalid", which surfaces as a
 * broken sign-in rather than as an obvious bug.
 *
 * A browser Response tolerates this, so it is easy to reintroduce when porting
 * code from sayknow-cli, which runs on Bun.
 *
 * `?raw` keeps this a build-time read, so the test needs no node typings.
 */
const SOURCES = import.meta.glob("./*.ts", { query: "?raw", import: "default", eager: true }) as Record<
  string,
  string
>

function source(name: string): string {
  const entry = SOURCES[`./${name}.ts`]
  if (!entry) throw new Error(`missing source: ${name}.ts`)
  return entry
}

const ADAPTERS = [
  "anthropic",
  "chat",
  "codex-chat",
  "cursor",
  "gemini-chat",
  "google-gemini-cli",
  "google-oauth-shared",
  "openai-codex",
  "xai",
] as const

describe("response bodies are read once", () => {
  it.each(ADAPTERS)("%s does not clone a response", (name) => {
    expect(source(name)).not.toMatch(/\.clone\(\)/)
  })

  it("gemini's loadCodeAssist error path reads the body a single time", () => {
    const text = source("google-gemini-cli")
    // The error branch only. The `else` reads the body too, but the two are
    // mutually exclusive — what matters is that neither path reads twice.
    const start = text.indexOf("if (!loadResponse.ok)")
    const errorBranch = text.slice(start, text.indexOf("} else {", start))
    expect(errorBranch).toContain("loadResponse.text()")
    expect(errorBranch.match(/loadResponse\.(text|json)\(\)/g)).toHaveLength(1)
    // The payload check reuses the text already read instead of reading again.
    expect(errorBranch).toContain("JSON.parse(errorText)")
  })
})
