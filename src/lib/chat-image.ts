/**
 * Images attached to a chat turn.
 *
 * One shape is stored and shown; each provider adapter turns it into its own
 * wire format (Anthropic `image` block, OpenAI `image_url`, Codex
 * `input_image`, Gemini `inlineData`). Base64 is used rather than a blob URL
 * because the conversation lives in localStorage and has to survive a
 * relaunch — which is also why every image is downscaled first: a single
 * 12 MP photo would blow the ~5 MB storage budget on its own.
 */

export type ChatImage = {
  /** Stable id for React keys and removal. */
  id: string
  mimeType: "image/jpeg" | "image/png" | "image/webp" | "image/gif"
  /** Raw base64, no `data:` prefix. */
  data: string
  width: number
  height: number
  /** Original file name when the image came from a file; absent for pastes. */
  name?: string
}

/** Longest edge after downscaling. Plenty for vision models, small enough to store. */
export const MAX_EDGE = 1568
/** Per-image storage ceiling after encoding. */
export const MAX_BYTES = 1_200_000
/** Attachments per turn. Past this the request is unwieldy and the strip unreadable. */
export const MAX_IMAGES_PER_MESSAGE = 4

const ACCEPTED: ReadonlySet<string> = new Set([
  "image/jpeg",
  "image/png",
  "image/webp",
  "image/gif",
])

export function isImageFile(file: File): boolean {
  return ACCEPTED.has(file.type)
}

/** `data:` URL for an `<img src>`. */
export function imageSrc(image: Pick<ChatImage, "mimeType" | "data">): string {
  return `data:${image.mimeType};base64,${image.data}`
}

/** Approximate decoded size of a base64 payload. */
export function base64Bytes(data: string): number {
  const padding = data.endsWith("==") ? 2 : data.endsWith("=") ? 1 : 0
  return Math.floor((data.length * 3) / 4) - padding
}

/**
 * Scale factor that fits `w x h` inside `MAX_EDGE`, never upscaling.
 * Pure so the sizing rule is testable without a canvas.
 */
export function fitScale(w: number, h: number, maxEdge = MAX_EDGE): number {
  const longest = Math.max(w, h)
  return longest > maxEdge ? maxEdge / longest : 1
}

function stripDataUrl(dataUrl: string): string {
  const comma = dataUrl.indexOf(",")
  return comma >= 0 ? dataUrl.slice(comma + 1) : dataUrl
}

async function decode(file: Blob): Promise<ImageBitmap> {
  return createImageBitmap(file)
}

/**
 * Turn a file or pasted blob into a stored `ChatImage`.
 *
 * Downscales to `MAX_EDGE`, re-encodes as JPEG (or PNG when the source has
 * an alpha channel worth keeping), and steps the JPEG quality down until the
 * result fits `MAX_BYTES`. GIFs lose animation — vision models see one frame
 * anyway.
 */
export async function encodeImage(file: Blob, name?: string): Promise<ChatImage> {
  const bitmap = await decode(file)
  try {
    const scale = fitScale(bitmap.width, bitmap.height)
    const width = Math.max(1, Math.round(bitmap.width * scale))
    const height = Math.max(1, Math.round(bitmap.height * scale))

    const canvas = document.createElement("canvas")
    canvas.width = width
    canvas.height = height
    const ctx = canvas.getContext("2d")
    if (!ctx) throw new Error("canvas is unavailable")
    ctx.drawImage(bitmap, 0, 0, width, height)

    // PNG keeps transparency, which screenshots of UI often rely on. Only
    // pay for it when the source was PNG and it fits.
    if (file.type === "image/png") {
      const png = stripDataUrl(canvas.toDataURL("image/png"))
      if (base64Bytes(png) <= MAX_BYTES) {
        return { id: crypto.randomUUID(), mimeType: "image/png", data: png, width, height, name }
      }
    }

    let quality = 0.9
    let jpeg = stripDataUrl(canvas.toDataURL("image/jpeg", quality))
    while (base64Bytes(jpeg) > MAX_BYTES && quality > 0.4) {
      quality -= 0.1
      jpeg = stripDataUrl(canvas.toDataURL("image/jpeg", quality))
    }
    if (base64Bytes(jpeg) > MAX_BYTES) {
      throw new Error("image is too large even after compression")
    }
    return { id: crypto.randomUUID(), mimeType: "image/jpeg", data: jpeg, width, height, name }
  } finally {
    bitmap.close()
  }
}

/** Image files out of a paste or drop, in the order the OS gave them. */
export function imageFilesFrom(transfer: DataTransfer | null): File[] {
  if (!transfer) return []
  const out: File[] = []
  for (const item of Array.from(transfer.items)) {
    if (item.kind !== "file") continue
    const file = item.getAsFile()
    if (file && isImageFile(file)) out.push(file)
  }
  return out
}
