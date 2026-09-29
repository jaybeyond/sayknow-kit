import { REPO_URL } from "./site"

/** The issue forms in .github/ISSUE_TEMPLATE, by what the visitor wants. */
export const ISSUE_TEMPLATES = { bug: "bug_report.yml", idea: "feature_request.yml" } as const

/**
 * A link that opens the GitHub issue form with the version already filled
 * in. GitHub fills an issue-form field from a query parameter named after
 * the field's id, so "version" must stay the id in bug_report.yml.
 */
export function issueFormUrl(kind: keyof typeof ISSUE_TEMPLATES, version: string): string {
  const url = new URL(`${REPO_URL}/issues/new`)
  url.searchParams.set("template", ISSUE_TEMPLATES[kind])
  if (kind === "bug") url.searchParams.set("version", version)
  return url.toString()
}
