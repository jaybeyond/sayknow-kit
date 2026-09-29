// Scroll reveal and the pinned story stage. `start(root)` wires whatever it
// finds under root and returns a teardown, so the harness can re-mount a
// variant (and replay its entrances) without a page reload.

export function startReveal(root: ParentNode): () => void {
  document.documentElement.setAttribute("data-reveal", "")
  const items = [...root.querySelectorAll<HTMLElement>(".reveal")]
  for (const el of items) el.removeAttribute("data-shown")
  const io = new IntersectionObserver(
    (entries) => {
      for (const e of entries) {
        if (!e.isIntersecting) continue
        ;(e.target as HTMLElement).setAttribute("data-shown", "")
        io.unobserve(e.target)
      }
    },
    { rootMargin: "0px 0px -12% 0px" },
  )
  for (const el of items) io.observe(el)
  return () => io.disconnect()
}

export function startStory(root: ParentNode): () => void {
  const story = root.querySelector<HTMLElement>("[data-story]")
  if (!story) return () => {}
  const stage = story.querySelector<HTMLElement>(".st-stage")
  const mocks = stage ? [...stage.querySelectorAll<HTMLElement>(".mock")] : []
  const chapters = [...story.querySelectorAll<HTMLElement>("[data-chapter]")]
  stage?.setAttribute("data-live", "")
  const show = (i: number) => {
    mocks.forEach((m, j) => m.toggleAttribute("data-on", j === i))
    chapters.forEach((c, j) => c.toggleAttribute("data-on", j === i))
  }
  show(0)
  // The chapter crossing the middle band of the screen is the current one.
  const io = new IntersectionObserver(
    (entries) => {
      for (const e of entries) {
        if (e.isIntersecting) show(Number((e.target as HTMLElement).dataset.chapter))
      }
    },
    { rootMargin: "-45% 0px -45% 0px" },
  )
  for (const c of chapters) io.observe(c)
  return () => io.disconnect()
}
