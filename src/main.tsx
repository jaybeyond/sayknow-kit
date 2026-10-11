import { StrictMode } from 'react'
import { createRoot } from 'react-dom/client'
import './index.css'
import App from './App.tsx'
import { dropRetiredModelCache } from './hooks/useModels'
// Evaluated first: its module body runs `migrateLegacy()`.
import './lib/chat-history'
import { bootSyncWithin } from './lib/sync-client'

dropRetiredModelCache()
// The settings window never touches conversations or translation history.
// Elsewhere the first render waits for the boot (bounded, E5), so a native
// device renders the hydrated cache.
const ready =
  new URLSearchParams(window.location.search).get('window') !== 'settings'
    ? bootSyncWithin()
    : Promise.resolve()

void ready.then(() => {
  createRoot(document.getElementById('root')!).render(
    <StrictMode>
      <App />
    </StrictMode>,
  )
})
