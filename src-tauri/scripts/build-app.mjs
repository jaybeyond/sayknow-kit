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
if (result.status) process.exit(result.status)

// A silently ad-hoc local build is worse than a failed one: its designated
// requirement is a cdhash, so every rebuild invalidates the Accessibility grant
// and the app keeps asking for a permission the user already gave.
if (process.platform === "darwin" && env.APPLE_SIGNING_IDENTITY && env.APPLE_SIGNING_IDENTITY !== "-") {
  const bundles = spawnSync(
    "find",
    ["src-tauri/target", "-type", "d", "-name", "SayKnow Kit.app", "-path", "*/bundle/macos/*"],
    { encoding: "utf8" },
  )
  if (bundles.error) throw bundles.error
  const paths = bundles.stdout.split("\n").filter(Boolean)
  if (paths.length === 0) throw new Error("no macOS bundle was produced to verify")
  for (const path of paths) {
    const signature = spawnSync("codesign", ["-dv", "--verbose=2", path], { encoding: "utf8" })
    const output = `${signature.stdout}${signature.stderr}`
    if (signature.status !== 0) throw new Error(`codesign failed for ${path}: ${output}`)
    if (!output.includes(`Authority=${env.APPLE_SIGNING_IDENTITY}`)) {
      throw new Error(`${path} is not signed with ${env.APPLE_SIGNING_IDENTITY}:\n${output}`)
    }
    console.log(`[build-app] verified signing identity on ${path}`)
  }
}

process.exit(0)
