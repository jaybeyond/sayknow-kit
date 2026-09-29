import { describe, expect, it } from "vitest"

import { nextChoice } from "./theme"

describe("theme switch", () => {
  it("flips to the other palette and remembers it only when it differs from the system", () => {
    // System is light: switching to dark is a choice worth keeping…
    expect(nextChoice("light", "light")).toBe("dark")
    // …and switching back matches the system again, so it is forgotten.
    expect(nextChoice("dark", "light")).toBeNull()
    expect(nextChoice("dark", "dark")).toBe("light")
    expect(nextChoice("light", "dark")).toBeNull()
  })
})
