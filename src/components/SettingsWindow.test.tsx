/** @vitest-environment jsdom */
import { afterEach, describe, expect, it, vi } from "vitest"
import { cleanup, render, screen } from "@testing-library/react"

vi.mock("@/lib/runtime", () => ({ isTauri: () => false, openExternal: vi.fn() }))
vi.mock("@/hooks/useModels", () => ({ useModels: () => ({ models: [], loading: false }) }))
vi.mock("@/hooks/useProviderProbe", () => ({ useProviderProbe: () => "idle" }))
vi.mock("@/components/OAuthProviderCard", () => ({ OAuthProviderCard: () => null }))

import { SettingsWindow } from "./SettingsWindow"
import { UI_STRINGS } from "@/i18n/strings"
import type { Settings } from "@/hooks/useSettings"

afterEach(() => {
  cleanup()
  window.history.replaceState(null, "", "/")
})

const settings = {
  provider: "openrouter",
  baseURL: "https://openrouter.ai/api/v1",
  model: "openai/gpt-4o-mini",
  fallbackModel: "",
  from: "auto",
  to: "en",
  autoTranslate: true,
  pinned: false,
  clipboardOnHotkey: false,
  glossary: [],
  uiLocale: "es",
  customTranslatePrompt: "",
  customRefinePrompt: "",
  windowMode: "normal",
  translateEngine: "llm",
  deeplFormality: "default",
  workspaceMode: "translate",
  menuBarReadout: "off",
  systemAlerts: [],
  popoverOpacity: 95,
  apiKey: "",
  deeplKey: "",
} as Settings

describe("SettingsWindow API key row", () => {
  it("keeps the key field at a set width so a long localized hint cannot widen it", () => {
    window.history.replaceState(null, "", "/?section=connection")
    render(
      <SettingsWindow
        settings={settings}
        update={vi.fn()}
        onLogout={vi.fn()}
        themeMode="system"
        setThemeMode={vi.fn()}
      />,
    )
    const field = document.getElementById("settings-api-key")
    expect(field).not.toBeNull()
    const column = field!.parentElement!.parentElement!
    expect(column.className.split(/\s+/)).toContain("w-64")
    expect(column.className).not.toMatch(/\bw-full\b|max-w-/)
    const hint = screen.getByText(UI_STRINGS.es["settings.connection.apiKeyShown"])
    expect(hint.className.split(/\s+/)).toContain("break-keep")
  })
})
