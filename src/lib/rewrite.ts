import type { ChatMessage } from "./openrouter"

export type RewritePreset = {
  id: string
  labelKey: string
  instruction: string
}

/** Retone-style rewrite jobs. Translation stays a separate workspace mode. */
export const REWRITE_PRESETS: RewritePreset[] = [
  {
    id: "polish",
    labelKey: "rewrite.polish",
    instruction: "Polish the draft so it reads cleanly. Keep the original language and meaning.",
  },
  {
    id: "polite",
    labelKey: "refine.polite",
    instruction: "Make it more polite and formal. Soften harsh wording. Keep the original language.",
  },
  {
    id: "casual",
    labelKey: "refine.casual",
    instruction: "Make it casual and conversational, as if talking to a friend. Keep the original language.",
  },
  {
    id: "witty",
    labelKey: "rewrite.witty",
    instruction: "Add a light touch of wit. Do not overdo it. Keep the original meaning and language.",
  },
  {
    id: "shorter",
    labelKey: "refine.shorter",
    instruction: "Make it shorter and more concise. Keep the original language.",
  },
  {
    id: "viral",
    labelKey: "rewrite.viral",
    instruction:
      "Rewrite with a strong first-line hook and short punchy sentences, like a post that stops the scroll. Keep the original language.",
  },
  {
    id: "business",
    labelKey: "refine.business",
    instruction: "Use a professional business tone. Keep the original language.",
  },
]

export const DEFAULT_REWRITE_PROMPT =
  "You rewrite the user's draft according to the instruction. " +
  "Keep the original language unless the instruction asks to translate. " +
  "Preserve meaning, hashtags, @mentions, and URLs. " +
  "Do not invent facts. Output ONLY the rewritten text, no explanations, no quotes."

export function buildRewritePrompt(
  draft: string,
  instruction: string,
  override?: string,
): ChatMessage[] {
  const system = override?.trim() || DEFAULT_REWRITE_PROMPT
  return [
    { role: "system", content: system },
    {
      role: "user",
      content: `Instruction: ${instruction}\n\nDraft:\n${draft}`,
    },
  ]
}
