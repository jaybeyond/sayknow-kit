import appIcon from "../../src-tauri/icons/icon.png"

/**
 * What the popover shows while it works out whether the user is signed in:
 * on launch, and after the idle reload that frees memory. It is the app
 * arriving, not a question — the login screen only appears once the answer
 * really is "signed out".
 *
 * The whole splash waits a beat before fading in (index.css), so an answer
 * that comes back at once never flashes it.
 */
export function BootSplash({ label }: { label: string }) {
  return (
    <div className="boot-splash" role="status" aria-label={label}>
      <img className="boot-splash-icon" src={appIcon} alt="" width={56} height={56} draggable={false} />
      <span className="boot-splash-name">SayKnow Kit</span>
      <span className="boot-splash-track" aria-hidden="true">
        <span />
      </span>
    </div>
  )
}
