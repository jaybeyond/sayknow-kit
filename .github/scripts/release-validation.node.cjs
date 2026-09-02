"use strict";

const assert = require("node:assert/strict");
const test = require("node:test");
const {
  canonicalize,
  hasExactLines,
  isUtcRfc3339,
  parseChecksums,
  validateRehearsalBinding,
  validateRuleset,
} = require("./release-validation.cjs");

const digest = "a".repeat(64);
const artifact = {
  architecture: "aarch64",
  bytes: 42,
  candidate_sha: "b".repeat(40),
  format: "dmg",
  name: "SayKnow-Kit_0.2.9_aarch64.dmg",
  platform: "macos-aarch64",
  product_name: "SayKnow Kit",
  sha256: digest,
  smoke: { format: "dmg", installed: true, launched: true, os_build: "24E263", runner: "macos15-20260801", uninstalled: true },
  version: "0.2.9",
};
const approval = { artifacts: [artifact], signing_posture: { macos: "adhoc", windows: "unsigned" } };
const context = {
  candidateSha: artifact.candidate_sha,
  runAttempt: 1,
  runId: 123,
  version: "0.2.9",
  workflowSha256: "c".repeat(64),
};
const rehearsal = {
  artifacts: approval.artifacts,
  candidate_sha: context.candidateSha,
  run_attempt: context.runAttempt,
  run_id: context.runId,
  schema: "sayknow.rehearsal/v1",
  signing_posture: approval.signing_posture,
  version: context.version,
  workflow_sha256: context.workflowSha256,
};

const pullRequest = {
  allowed_merge_methods: ["squash", "rebase"],
  dismiss_stale_reviews_on_push: true,
  require_code_owner_review: true,
  require_last_push_approval: false,
  required_approving_review_count: 1,
  required_review_thread_resolution: true,
};
const expectedRuleset = {
  name: "release-approvals-protection",
  target: "branch",
  patterns: ["refs/heads/release-approvals", "refs/heads/release-approval-v*"],
  rules: ["creation", "update", "pull_request"],
  pullRequest,
};
const ruleset = {
  name: expectedRuleset.name,
  target: expectedRuleset.target,
  enforcement: "active",
  conditions: { ref_name: { include: expectedRuleset.patterns, exclude: [] } },
  rules: [
    { type: "creation" },
    { type: "update" },
    { type: "pull_request", parameters: pullRequest },
  ],
  bypass_actors: [{ actor_id: 201892478, actor_type: "User", bypass_mode: "always" }],
};

test("canonical JSON has stable sorted bytes", () => {
  assert.equal(canonicalize({ z: 1, a: [true, { y: 2, b: 3 }] }), '{"a":[true,{"b":3,"y":2}],"z":1}');
});

test("UTC RFC3339 validation accepts GitHub and ISO timestamps", () => {
  assert.equal(isUtcRfc3339("2026-09-02T15:11:10Z"), true);
  assert.equal(isUtcRfc3339("2026-09-02T15:12:52.309Z"), true);
  assert.equal(isUtcRfc3339("2026-09-02 15:11:10Z"), false);
});

test("tag message matching uses real LF and CRLF line boundaries", () => {
  assert.equal(hasExactLines("version=0.2.9\ncandidate=abc\n", ["version=0.2.9", "candidate=abc"]), true);
  assert.equal(hasExactLines("version=0.2.9\r\ncandidate=abc\r\n", ["candidate=abc"]), true);
  assert.equal(hasExactLines("version=0.2.9\\ncandidate=abc", ["candidate=abc"]), false);
});

test("checksum parser rejects malformed and duplicate entries", () => {
  assert.equal(parseChecksums(`${digest}  artifact.dmg\n`).get("artifact.dmg"), digest);
  assert.throws(() => parseChecksums("not-a-digest  artifact.dmg\n"));
  assert.throws(() => parseChecksums(`${digest}  artifact.dmg\n${digest}  artifact.dmg\n`));
});

test("rehearsal binding rejects stale candidate, run, workflow, artifact, and posture", () => {
  validateRehearsalBinding(approval, rehearsal, context);
  for (const [key, value] of [
    ["candidate_sha", "d".repeat(40)],
    ["run_id", 124],
    ["workflow_sha256", "e".repeat(64)],
  ]) {
    assert.throws(() => validateRehearsalBinding(approval, { ...rehearsal, [key]: value }, context));
  }
  assert.throws(() => validateRehearsalBinding({ ...approval, artifacts: [] }, rehearsal, context));
  assert.throws(() => validateRehearsalBinding({ ...approval, signing_posture: { macos: "developer-id", windows: "unsigned" } }, rehearsal, context));
});

test("ruleset validation rejects missing review policy, extra refs, and unauthorized bypass", () => {
  validateRuleset(ruleset, expectedRuleset);
  validateRuleset({ ...ruleset, bypass_actors: [] }, expectedRuleset);
  validateRuleset({ ...ruleset, bypass_actors: [{ actor_id: null, actor_type: "User", bypass_mode: "always" }] }, expectedRuleset);
  assert.throws(() => validateRuleset({ ...ruleset, rules: ruleset.rules.filter((rule) => rule.type !== "pull_request") }, expectedRuleset));
  assert.throws(() => validateRuleset({ ...ruleset, conditions: { ref_name: { include: [...expectedRuleset.patterns, "refs/heads/main"], exclude: [] } } }, expectedRuleset));
  assert.throws(() => validateRuleset({ ...ruleset, bypass_actors: [{ actor_id: 1, actor_type: "User", bypass_mode: "always" }] }, expectedRuleset));
});
