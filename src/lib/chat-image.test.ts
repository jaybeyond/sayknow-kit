import { describe, expect, it } from "vitest"
import {
  base64Bytes,
  fitScale,
  imageFilesFrom,
  imageSrc,
  isImageFile,
  MAX_EDGE,
} from "./chat-image"

describe("fitScale", () => {
  it("never upscales a small image", () => {
    expect(fitScale(800, 600)).toBe(1)
    expect(fitScale(MAX_EDGE, 10)).toBe(1)
  })

  it("scales the longest edge down to the cap, whichever axis it is", () => {
    expect(fitScale(MAX_EDGE * 2, 100)).toBeCloseTo(0.5)
    expect(fitScale(100, MAX_EDGE * 4)).toBeCloseTo(0.25)
  })
})

describe("base64Bytes", () => {
  it("accounts for padding", () => {
    // "abc" -> "YWJj" (no padding), "ab" -> "YWI=" (one), "a" -> "YQ==" (two)
    expect(base64Bytes("YWJj")).toBe(3)
    expect(base64Bytes("YWI=")).toBe(2)
    expect(base64Bytes("YQ==")).toBe(1)
  })
})

describe("imageSrc", () => {
  it("builds a data url the <img> tag and every provider can consume", () => {
    expect(imageSrc({ mimeType: "image/png", data: "AAAA" })).toBe("data:image/png;base64,AAAA")
  })
})

describe("file filtering", () => {
  it("accepts the formats vision models take and refuses the rest", () => {
    expect(isImageFile(new File([], "a.png", { type: "image/png" }))).toBe(true)
    expect(isImageFile(new File([], "a.webp", { type: "image/webp" }))).toBe(true)
    expect(isImageFile(new File([], "a.svg", { type: "image/svg+xml" }))).toBe(false)
    expect(isImageFile(new File([], "a.pdf", { type: "application/pdf" }))).toBe(false)
  })

  it("pulls only image files out of a paste or drop", () => {
    const png = new File([], "shot.png", { type: "image/png" })
    const txt = new File([], "notes.txt", { type: "text/plain" })
    const transfer = {
      items: [
        { kind: "file", getAsFile: () => png },
        { kind: "string", getAsFile: () => null },
        { kind: "file", getAsFile: () => txt },
      ],
    } as unknown as DataTransfer
    expect(imageFilesFrom(transfer)).toEqual([png])
    expect(imageFilesFrom(null)).toEqual([])
  })
})
