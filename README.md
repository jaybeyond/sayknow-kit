<div align="center">

<img src="public/sayo-logo.png" alt="Sayo, the SayKnow Kit octopus mascot" width="180" />

# SayKnow Kit

**Cross-platform AI kit for the macOS menu bar and Windows tray — translate, chat, and clipboard history in one popover.**

`say` + `know` — speak it, understand it.

[한국어](README.ko.md) · **English** · [日本語](README.ja.md) · [中文](README.zh.md) · [Español](README.es.md) · [Français](README.fr.md) · [Deutsch](README.de.md) · [Tiếng Việt](README.vi.md)

[![macOS](https://img.shields.io/badge/macOS-11%2B-black?logo=apple)](https://www.apple.com/macos/) [![Windows](https://img.shields.io/badge/Windows-10%2F11-blue?logo=windows)](https://www.microsoft.com/windows)
[![Tauri](https://img.shields.io/badge/Tauri-2-FFC131?logo=tauri&logoColor=black)](https://tauri.app)
[![License: MIT](https://img.shields.io/badge/License-MIT-green.svg)](LICENSE)

</div>

---

## Overview

SayKnow Kit lives in your **macOS menu bar or Windows system tray**. One shortcut opens a small popover holding the three tools you reach for all day: **translate** (fires on its own when you stop typing), **chat**, and **clipboard history**. No more switching tabs to a translator and pasting back and forth.

Three providers, one window: **OpenRouter BYOK** (Bring Your Own Key, 360+ models), **OCP (Open Claude Proxy)** for using your local Claude CLI as an OpenAI-compatible API, or any **Custom** OpenAI-compatible endpoint you point it at.

## Features

### Translate
- ⚡ **Auto translate** — fires 1.5s after you stop typing
- ⌨️ **Manual mode** — press `⌘⏎` on macOS or `Ctrl+Enter` on Windows, or use the button (saves tokens)
- 🪄 **Refine** — polite / casual / shorter / business / literal presets + custom prompt
- ⏹ **Stop** — abort an in-flight call when the model is slow
- 🔄 **36 translation languages** — across Asia, Europe, the Middle East, and Africa
- 📚 **Glossary** — keep company names, jargon, and proper nouns consistent
- ✏️ **Custom system prompt** — edit the translate / refine prompts directly

### Chat
- 💬 **Chat tab** — lightweight Q&A in the same window. Multi-conversation sidebar with auto-generated titles
- ♻️ **Regenerate / ✏️ edit / 📋 copy / ⏹ stop** — per-message actions
- 🧠 **Shared model** — reuses the model you picked for translation

### Clipboard
- 📋 **Clipboard history** — everything you copy, captured in the background and searchable across text and notes
- 📝 **Notes** — annotate an entry so you remember why you kept it
- 📌 **Pin** — pinned entries survive both the size cap and a clear
- ➡️ **Send to translate** — push any entry straight into the translate tab
- 🧹 **Two-tier clear** — drop the unpinned, or wipe everything
- 🔒 **Skips secrets** — blanks, OTP-shaped strings and PEM key blobs are never stored

### Usage
- 📊 **Agent usage** — Claude Code, Codex and SayKnow CLI, read from the session logs they already write locally
- ⏱ **5-hour blocks** — the billing window subscriptions actually meter on, with time left and burn rate
- 🚦 **Real quota** — provider-reported 5-hour and weekly percentages with reset times, where the CLI records them
- 🔍 **Honest staleness** — a window that has already reset is struck through, never drawn as your current level
- 🔌 **No network** — nothing is uploaded and no extra sign-in is required
- 🧰 **Lives in Tools** — the usage cards sit in the Tools tab, right under screen brightness

### Tools
- 🌞 **Screen brightness (macOS-specific)** — hardware-level control for connected displays: DDC/CI for externals (HDMI/DisplayPort/USB-C), IOKit backlight for built-in Mac panels where available. On newer supported Macs where direct IOKit access is unavailable, it uses Control Center accessibility automation. External DDC capabilities vary by hardware; both built-in paths are macOS-only.
- 🔌 **Display power (macOS-specific)** — turn an external monitor off and back on (DDC standby); charging, audio and USB keep working
- 🎚️ **One slider for all** — move every display at once, or adjust each on its own
- 🔍 **Honest readouts** — a monitor that won't report brightness over DDC still adjusts; it just shows —
- 📊 **System status** — system CPU, memory, and system disk, plus SoC die temperature on macOS (Apple Silicon) read through unprivileged AppleVendor sensors; shown as unavailable on Windows or whenever no trustworthy sensor is present
- ⌨️ **Keyboard-friendly** — sliders commit on release, so one drag is one command
### Providers
- 🌐 **OpenRouter BYOK** — searchable combobox over 360+ models, automatic fallback model retry
- 🤖 **OCP (Open Claude Proxy)** — expose the local Claude CLI as an OpenAI-compatible API. **One-tap install** (clones the repo, `npm install`, `node setup.mjs`) with live log streaming
- 🔧 **Custom endpoint** — any OpenAI-compatible base URL (self-hosted, vLLM, LM Studio…)

### Window / UX
- 📐 **Compact horizontal mode** — 720×240 side-by-side layout, designed to stay open all day
- 🪟 **Window size toggle** — flip between compact and normal (480×580) from the header
- 📌 **Pin window** — disable auto-hide when you want it open
- 📋 **Clipboard auto-fill** — copy text anywhere → press `⌘⇧T` on macOS or `Ctrl+Shift+T` on Windows → it lands in the input
- 🕘 **History** — searchable, pin entries to keep them forever
- 💰 **Usage tracking** — daily / monthly tokens and cost (USD)
- 🌓 **Light / dark / system** — auto-follows OS theme
- 🌍 **8 UI languages** — auto-detected from system locale
- 🔒 **System credential storage** — macOS Keychain or Windows Credential Manager; never plaintext

## System requirements

- **macOS 11.0 (Big Sur) or later on Apple Silicon (aarch64)**
- **Windows 10 or 11 on x64**
- An OpenRouter API key — get one at [openrouter.ai/keys](https://openrouter.ai/keys)

## Install

### v0.2.12 prebuilt installers (recommended)

Download only from the [official GitHub Release](https://github.com/jaybeyond/sayknow-kit/releases), and verify the matching file in `SHA256SUMS.txt`.

**macOS (Apple Silicon):** Download `SayKnow-Kit_0.2.12_aarch64.dmg`, open it, and drag `SayKnow Kit.app` into `/Applications`. The v0.2.12 app has an **ad-hoc signature only**—no Developer ID or notarization—so Gatekeeper warnings are expected. macOS 13 and newer no longer accept the right-click bypass: open the app once, then go to **System Settings → Privacy & Security → Open Anyway**. Or clear the quarantine flag yourself:

```bash
xattr -dr com.apple.quarantine "/Applications/SayKnow Kit.app"
```

If macOS keeps asking for Accessibility after you allowed it, the stored entry no longer matches the updated ad-hoc signature. Reset it and restart the app:

```bash
tccutil reset Accessibility com.sayknow.app
```

Always move the app to `/Applications` first. Launched from the DMG or Downloads, macOS runs it from a randomized read-only copy where Accessibility permission for built-in brightness can never be stored.

**Windows (x64):** Download `SayKnow-Kit_0.2.12_x64-setup.exe` (NSIS) or `SayKnow-Kit_0.2.12_x64_en-US.msi`, run it, and follow the prompts. The installers are **unsigned**; Windows SmartScreen warnings are expected. Choose **More info → Run anyway** only after checking the official Release and `SHA256SUMS.txt`.

### Uninstall

- **macOS:** Quit SayKnow Kit, then remove `SayKnow Kit.app` from `/Applications`.
- **Windows:** Settings → Apps → Installed apps → SayKnow Kit → Uninstall (or use the uninstaller in the installation folder).

### Build from source

```bash
git clone https://github.com/jaybeyond/sayknow-kit.git
cd sayknow-kit
pnpm install
pnpm tauri build
```

## Usage

### First run

1. A small icon appears in the menu bar (next to the clock / Wi-Fi) on macOS, or in the system tray on Windows. The app is **not in the Dock / taskbar** by design.
2. Click the tray icon → enter your OpenRouter API key → **Connect & start**.
3. The key is saved into system credential storage (macOS Keychain or Windows Credential Manager) automatically — you won't be asked again.

### Daily flow

1. Click the tray icon or press `⌘⇧T` on macOS, or `Ctrl+Shift+T` on Windows, to open the popover.
2. Pick source / target language (or leave source on **Auto-detect**).
3. Type. After ~1.5s of inactivity, the translation appears below.
4. Click 📋 to copy.

### Refine

Tweak the tone or style after translation:
- Presets: **Polite / Casual / Shorter / Business / Literal**
- ✨ **Custom prompt** — anything you want, e.g. *"warmer tone"*

### Shortcuts

| Shortcut | Action |
|---|---|
| `⌘⇧T` (macOS) | Toggle the popover (global) |
| `Ctrl+Shift+T` (Windows) | Toggle the popover (global) |
| `⌘⏎` (macOS) | Translate immediately (manual mode) |
| `Ctrl+Enter` (Windows) | Translate immediately (manual mode) |

### Settings (separate window)

Click ⚙️ in the popover → **Settings** opens a full window with a sidebar:
- **General** — auto/manual mode, clipboard auto-fill, pin, theme, app language
- **Connection** — primary model, fallback model, sign out
- **Glossary** — term pairs ("backend team" → "Backend Team")
- **System prompt** — edit translate / refine prompts (variables: `{from}`, `{to}`, `{glossary}`)
- **Usage** — daily / monthly tokens and cost
- **About** — version, GitHub, OpenRouter

### Clipboard auto-fill

In **Settings → General**, enable clipboard auto-fill for `⌘⇧T` on macOS or `Ctrl+Shift+T` on Windows. Then:
1. Select text in any app → `⌘C` on macOS or `Ctrl+C` on Windows
2. Press `⌘⇧T` on macOS or `Ctrl+Shift+T` on Windows → SayKnow Kit opens with that text already in the input
3. In auto mode, it translates 1.5s later

## Security

Your OpenRouter key is a **billable credential**, so SayKnow Kit never stores it in plaintext. It uses the operating system's credential store:

- macOS **Keychain** (`com.sayknow.app`)
- Windows **Credential Manager**

Download installers only from the official GitHub Release and verify `SHA256SUMS.txt`. The v0.2.12 macOS app is ad-hoc signed without Developer ID or notarization; Windows installers are unsigned without Authenticode. Gatekeeper and SmartScreen warnings are expected.

## Development

```bash
# Prerequisites
node -v   # v20+
pnpm -v   # v9+
# Rust toolchain
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y
# Xcode CLT
xcode-select --install
```

```bash
pnpm install
pnpm tauri dev      # dev server + window with HMR
pnpm tauri build    # production .app + .dmg
```

## Tech stack

| Area | Tech |
|---|---|
| Framework | Tauri 2 + Vite 8 + React 19 |
| Languages | TypeScript (strict) + Rust |
| Styling | Tailwind v4 + shadcn/ui |
| Icons | Lucide React |
| Storage | localStorage + system credential storage (macOS Keychain / Windows Credential Manager) |
| Tauri plugins | `positioner`, `global-shortcut`, `clipboard-manager`, `opener`, `http`, `log` |
| External API | OpenRouter / OCP (local) / any OpenAI-compatible endpoint |

## What's new

### Features
- **Chat tab** — multi-conversation with regenerate / edit / copy / stop per message
- **Multi-provider** — switch between OpenRouter, OCP, and Custom endpoints in one place
- **One-tap OCP install** — SayKnow Kit runs `git clone → npm install → node setup.mjs` for you and streams the output live
- **Compact horizontal mode** — 720×240 side-by-side layout, designed for keep-on-screen use
- **Window size toggle** — header button flips compact ↔ normal instantly

### Improvements / fixes
- Suppressed macOS "Reopen windows?" dialog (`NSQuitAlwaysKeepsWindows=false`, `LSUIElement=true`)
- Fixed `SIGABRT` crash when `move_window(TrayCenter)` was called before the positioner cached the tray rect
- Restoring a history entry no longer re-fires the auto-translate
- Pinned window no longer hides its body content after focus loss
- Native Chinese labels (`简体中文 / 繁體中文`)
- API-key label now reflects the active provider
- Filled in 43 missing keys across the 8 locales (~250 strings)
- Routed localhost `fetch` through `tauri-plugin-http` to bypass WebKit CORS
- Resolved OCP / Claude CLI via `/bin/sh -lc 'command -v ...'` so GUI launches find them

## Roadmap

- [ ] System-wide text selection → hotkey → instant translate
- [ ] Favorite phrases / pins
- [ ] Launch at login
- [x] Windows support (released x64 support)
- [ ] Apple code-signing + notarization
- [ ] OCR (screenshot region translate)
- [ ] Direct local-LLM adapters (Ollama, LM Studio)
- [ ] Model recommendation / side-by-side compare

## Contributing

PRs welcome. For larger changes, please open an issue first to discuss.

## License

[MIT](LICENSE)
