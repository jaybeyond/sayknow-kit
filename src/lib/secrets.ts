import { invoke } from "@tauri-apps/api/core"
import { isTauri } from "./runtime"
import { storage } from "./storage"

const LS_KEY = "apiKey"
// Bumped on every set/clear so other windows can react via the `storage` event.
// The value is just a timestamp — the actual key still lives in Keychain.
export const SECRETS_REV_KEY = "apiKey:rev"

function bumpRev() {
  storage.set(SECRETS_REV_KEY, Date.now())
}

/** Secondary credentials, kept under their own Keychain account so signing
 *  out of the LLM provider doesn't take them with it. */
export const DEEPL_ACCOUNT = "deepl_api_key"
export const DEEPL_REV_KEY = "deepl:rev"

// Writes are chained per account. The key field saves on every keystroke, and
// two overlapping invokes can land out of order — which would leave a
// truncated key in the Keychain while the field shows the whole thing, i.e. a
// key that looks right and doesn't work.
const writeQueue = new Map<string, Promise<void>>()

function enqueue(account: string, op: () => Promise<void>): Promise<void> {
  const next = (writeQueue.get(account) ?? Promise.resolve())
    .catch(() => {})
    .then(op)
  writeQueue.set(account, next)
  return next
}

export const namedSecret = {
  async get(account: string): Promise<string> {
    if (isTauri()) {
      const v = await invoke<string | null>("get_secret", { account })
      return v ?? ""
    }
    return storage.get<string>(`secret:${account}`) ?? ""
  },
  set(account: string, key: string): Promise<void> {
    return enqueue(account, async () => {
      if (isTauri()) await invoke("set_secret", { account, key })
      else storage.set(`secret:${account}`, key)
      storage.set(DEEPL_REV_KEY, Date.now())
    })
  },
  clear(account: string): Promise<void> {
    return enqueue(account, async () => {
      if (isTauri()) await invoke("delete_secret", { account })
      else storage.remove(`secret:${account}`)
      storage.set(DEEPL_REV_KEY, Date.now())
    })
  },
}

export const secrets = {
  async get(): Promise<string> {
    if (isTauri()) {
      const v = await invoke<string | null>("get_api_key")
      return v ?? ""
    }
    return storage.get<string>(LS_KEY) ?? ""
  },
  async set(key: string): Promise<void> {
    if (isTauri()) {
      await invoke("set_api_key", { key })
    } else {
      storage.set(LS_KEY, key)
    }
    bumpRev()
  },
  async clear(): Promise<void> {
    if (isTauri()) {
      await invoke("delete_api_key")
      // "Sign out (delete key)" should also tear down the OCP daemon the
      // user connected — otherwise launchd keeps respawning OCP on :3456
      // and the next login auto-reconnects. Best-effort: a machine that
      // never used OCP just silently no-ops.
      try {
        await invoke("disconnect_ocp")
      } catch {
        /* best effort */
      }
    } else {
      storage.remove(LS_KEY)
    }
    bumpRev()
  },
}
