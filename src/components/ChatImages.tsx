import { useEffect } from "react"
import { ImageOff, X } from "lucide-react"
import { imageSrc, type ChatImage } from "@/lib/chat-image"
import { cn } from "@/lib/utils"

/**
 * Thumbnails pending in the composer. Each has a remove affordance that is
 * always reachable — hover-only controls are invisible to keyboard users and
 * to anyone on a trackpad who has not learned to hunt for them.
 */
export function AttachmentStrip({
  images,
  onRemove,
  removeLabel,
}: {
  images: ChatImage[]
  onRemove: (id: string) => void
  removeLabel: string
}) {
  if (images.length === 0) return null
  return (
    <div className="mb-1.5 flex flex-wrap gap-1.5">
      {images.map((img) => (
        <div
          key={img.id}
          className="group relative h-14 w-14 overflow-hidden rounded-lg border bg-muted shadow-sm animate-in fade-in zoom-in-95 duration-150"
        >
          <img
            src={imageSrc(img)}
            alt={img.name ?? ""}
            className="h-full w-full object-cover"
            draggable={false}
          />
          <button
            type="button"
            onClick={() => onRemove(img.id)}
            aria-label={removeLabel}
            title={removeLabel}
            className="absolute right-0.5 top-0.5 flex h-4 w-4 items-center justify-center rounded-full bg-background/90 text-foreground shadow ring-1 ring-border transition-[transform,opacity] duration-150 ease-out hover:scale-110 active:scale-95"
          >
            <X className="h-2.5 w-2.5" />
          </button>
        </div>
      ))}
    </div>
  )
}

/**
 * Images inside a sent bubble. One image fills the bubble width; several
 * tile in a 2-up grid so a batch of screenshots stays scannable at 480px.
 */
export function MessageImages({
  images,
  evictedLabel,
  altLabel,
  onOpen,
}: {
  images: ChatImage[]
  evictedLabel: string
  altLabel: string
  onOpen: (image: ChatImage) => void
}) {
  if (images.length === 0) return null
  const single = images.length === 1
  return (
    <div className={cn("grid gap-1", single ? "grid-cols-1" : "grid-cols-2")}>
      {images.map((img) =>
        img.data ? (
          <button
            key={img.id}
            type="button"
            onClick={() => onOpen(img)}
            className={cn(
              "overflow-hidden rounded-xl bg-black/5 transition-transform duration-150 ease-out active:scale-[0.98]",
              single ? "max-h-64" : "aspect-square",
            )}
            aria-label={altLabel}
          >
            <img
              src={imageSrc(img)}
              alt={img.name ?? altLabel}
              width={img.width}
              height={img.height}
              className={cn("h-full w-full", single ? "object-contain" : "object-cover")}
              draggable={false}
            />
          </button>
        ) : (
          <div
            key={img.id}
            className="flex aspect-square items-center justify-center gap-1 rounded-xl bg-black/10 px-2 text-center text-[10px] text-muted-foreground"
          >
            <ImageOff className="h-3 w-3 shrink-0" />
            <span>{evictedLabel}</span>
          </div>
        ),
      )}
    </div>
  )
}

/**
 * Full-size view. Escape or a click anywhere closes it. Fades rather than
 * scales from nothing — the thumbnail the user clicked is already on screen,
 * so the transition reads as "expand", not "appear". Entry uses
 * `@starting-style` (see index.css `.lightbox`) so no effect has to flip a
 * mounted flag after first paint.
 */
export function ImageLightbox({
  image,
  closeLabel,
  onClose,
}: {
  image: ChatImage | null
  closeLabel: string
  onClose: () => void
}) {
  useEffect(() => {
    if (!image) return
    const onKey = (e: KeyboardEvent) => {
      if (e.key === "Escape") onClose()
    }
    window.addEventListener("keydown", onKey)
    return () => window.removeEventListener("keydown", onKey)
  }, [image, onClose])

  if (!image) return null
  return (
    <div
      role="dialog"
      aria-modal="true"
      aria-label={image.name ?? closeLabel}
      onClick={onClose}
      className="lightbox absolute inset-0 z-50 flex items-center justify-center bg-background/85 p-4 backdrop-blur-sm"
    >
      <img
        src={imageSrc(image)}
        alt={image.name ?? ""}
        className="lightbox-image max-h-full max-w-full rounded-lg object-contain shadow-2xl"
        draggable={false}
      />
      <button
        type="button"
        onClick={onClose}
        aria-label={closeLabel}
        title={closeLabel}
        className="absolute right-2 top-2 flex h-7 w-7 items-center justify-center rounded-full bg-background/90 text-foreground shadow ring-1 ring-border transition-transform duration-150 ease-out active:scale-95"
      >
        <X className="h-3.5 w-3.5" />
      </button>
    </div>
  )
}
