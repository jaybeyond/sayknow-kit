import { describe, expect, it } from "vitest"
import { REWRITE_PRESETS, buildRewritePrompt } from "./rewrite"

describe("rewrite presets", () => {
  it("covers retone-style jobs without replacing translate", () => {
    expect(REWRITE_PRESETS.map((p) => p.id)).toEqual([
      "polish",
      "polite",
      "casual",
      "witty",
      "shorter",
      "viral",
      "business",
    ])
  })

  it("asks the model to rewrite the draft, not translate it", () => {
    const messages = buildRewritePrompt("오늘 회의 늦을 듯", "Make it casual.")
    expect(messages[0]?.content).toMatch(/original language/i)
    expect(messages[1]?.content).toContain("오늘 회의 늦을 듯")
    expect(messages[1]?.content).toContain("Make it casual.")
  })
})
