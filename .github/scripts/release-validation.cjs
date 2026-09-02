"use strict";

function canonicalize(value) {
  if (Array.isArray(value)) return `[${value.map(canonicalize).join(",")}]`;
  if (value && typeof value === "object") {
    return `{${Object.keys(value).sort().map((key) => `${JSON.stringify(key)}:${canonicalize(value[key])}`).join(",")}}`;
  }
  return JSON.stringify(value);
}

function isUtcRfc3339(value) {
  return typeof value === "string" && /^\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}(?:\.\d+)?Z$/.test(value);
}

function hasExactLines(text, expectedLines) {
  const lines = text.split(/\r?\n/);
  return expectedLines.every((line) => lines.includes(line));
}

function parseChecksums(text) {
  const entries = new Map();
  const lines = text.trim().split(/\r?\n/);
  if (lines.length === 1 && lines[0] === "") throw new Error("checksum file is empty");
  for (const line of lines) {
    const match = line.match(/^([0-9a-f]{64}) [ *](.+)$/);
    if (!match || entries.has(match[2])) throw new Error("invalid or duplicate checksum entry");
    entries.set(match[2], match[1]);
  }
  return entries;
}

function validateRuleset(ruleset, expected) {
  if (ruleset.name !== expected.name || ruleset.target !== expected.target || ruleset.enforcement !== "active") {
    throw new Error(`invalid ruleset identity: ${expected.name}`);
  }
  const includes = [...(ruleset.conditions?.ref_name?.include ?? [])].sort();
  const excludes = ruleset.conditions?.ref_name?.exclude ?? [];
  if (JSON.stringify(includes) !== JSON.stringify([...expected.patterns].sort()) || excludes.length !== 0) {
    throw new Error(`${expected.name} has unexpected ref conditions`);
  }
  const ruleList = ruleset.rules ?? [];
  const ruleTypes = ruleList.map((rule) => rule.type).sort();
  if (JSON.stringify(ruleTypes) !== JSON.stringify([...expected.rules].sort())) {
    throw new Error(`${expected.name} has unexpected rules`);
  }
  const byType = new Map(ruleList.map((rule) => [rule.type, rule]));
  if (byType.get("update")?.parameters?.update_allows_fetch_and_merge === true) {
    throw new Error(`${expected.name} permits fetch-and-merge updates`);
  }
  if (expected.pullRequest) {
    const parameters = byType.get("pull_request")?.parameters;
    for (const [key, value] of Object.entries(expected.pullRequest)) {
      if (JSON.stringify(parameters?.[key]) !== JSON.stringify(value)) {
        throw new Error(`${expected.name} has invalid pull_request.${key}`);
      }
    }
  }
  // The repository GITHUB_TOKEN may redact bypass actor identities. Validate
  // the sole actor exactly whenever visible; all enforceable rule data remains exact.
  const bypass = ruleset.bypass_actors ?? [];
  if (bypass.length > 1) throw new Error(`${expected.name} has unauthorized bypass actors`);
  if (bypass.length === 1) {
    const actor = bypass[0];
    const actorIdMatches = actor.actor_id == null || Number(actor.actor_id) === 201892478;
    if (actor.actor_type !== "User" || !actorIdMatches || actor.bypass_mode !== "always") {
      throw new Error(`${expected.name} has an unauthorized bypass`);
    }
  }
}

function validateRehearsalBinding(approval, rehearsal, context) {
  if (rehearsal.schema !== "sayknow.rehearsal/v1" || rehearsal.candidate_sha !== context.candidateSha || rehearsal.version !== context.version) {
    throw new Error("invalid rehearsal manifest identity");
  }
  if (rehearsal.run_id !== context.runId || rehearsal.run_attempt !== context.runAttempt) {
    throw new Error("invalid rehearsal run identity");
  }
  if (rehearsal.workflow_sha256 !== context.workflowSha256) throw new Error("rehearsal workflow digest mismatch");
  if (JSON.stringify(rehearsal.artifacts) !== JSON.stringify(approval.artifacts)) {
    throw new Error("approved artifacts differ from rehearsal");
  }
  if (JSON.stringify(rehearsal.signing_posture) !== JSON.stringify(approval.signing_posture)) {
    throw new Error("approved signing posture differs from rehearsal");
  }
}

module.exports = { canonicalize, hasExactLines, isUtcRfc3339, parseChecksums, validateRehearsalBinding, validateRuleset };
