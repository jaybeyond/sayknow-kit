import { spawnSync } from "node:child_process"

const env = { ...process.env }

// Local macOS builds need a stable Apple Development identity so Accessibility
// permission survives rebuilds. The release workflow does not use this wrapper:
// it explicitly requests and verifies an ad-hoc signature until Developer ID
// and notarization credentials are provisioned.
if (process.platform === "darwin" && !env.APPLE_SIGNING_IDENTITY) {
  const identities = spawnSync("security", ["find-identity", "-v", "-p", "codesigning"], {
    encoding: "utf8",
  })
  if (identities.error) throw identities.error
  if (identities.status !== 0) {
    throw new Error(`security find-identity failed with exit code ${identities.status}`)
  }
  const matches = [...identities.stdout.matchAll(/"(Apple Development:[^"]+)"/g)].map((match) => match[1])
  const unique = [...new Set(matches)]
  if (unique.length === 1) {
    env.APPLE_SIGNING_IDENTITY = unique[0]
    console.log(`[build-app] using local signing identity: ${unique[0]}`)
  } else if (env.SAYKNOW_ALLOW_UNSIGNED_LOCAL_BUILD === "1") {
    console.warn(`[build-app] explicitly building unsigned; found ${unique.length} Apple Development identities`)
  } else {
    throw new Error(`expected exactly one Apple Development identity, found ${unique.length}`)
  }
}

const command = process.platform === "win32" ? "pnpm.cmd" : "pnpm"
const result = spawnSync(command, ["tauri", "build", ...process.argv.slice(2)], {
  env,
  stdio: "inherit",
})

if (result.error) throw result.error
process.exit(result.status ?? 1)
