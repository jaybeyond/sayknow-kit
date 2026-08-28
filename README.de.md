<div align="center">

# SayKnow Kit

**KI-Kit in der Menüleiste — Übersetzen, Chat und Zwischenablage in einem Popover.**

`say` (sagen) + `know` (wissen) — sag's, er versteht.

[한국어](README.ko.md) · [English](README.md) · [日本語](README.ja.md) · [中文](README.zh.md) · [Español](README.es.md) · [Français](README.fr.md) · **Deutsch** · [Tiếng Việt](README.vi.md)

[![macOS](https://img.shields.io/badge/macOS-11.0%2B-black?logo=apple)](https://www.apple.com/macos/)
[![Tauri](https://img.shields.io/badge/Tauri-2-FFC131?logo=tauri&logoColor=black)](https://tauri.app)
[![License: MIT](https://img.shields.io/badge/License-MIT-green.svg)](LICENSE)

</div>

---

## Übersicht

SayKnow Kit lebt in der **macOS-Menüleiste**. Ein Tastenkürzel öffnet ein kleines Fenster mit den drei Werkzeugen für den Alltag: **Übersetzen** (startet von selbst, sobald du aufhörst zu tippen), **Chat** und **Zwischenablage-Verlauf**. Kein Hin- und Herwechseln zwischen Tabs und Copy-Paste mehr.

**Drei Anbieter** im selben Fenster (OpenRouter / OCP / Custom-Endpoint). Schon OpenRouter allein bringt Zugriff auf über 360 Modelle (GPT-4o, Claude, Gemini, Llama, ...) mit nur einem Schlüssel.

## Funktionen

- 🌞 **Werkzeuge-Tab** — Helligkeit aller angeschlossenen Displays auf Hardwareebene (DDC/CI für externe, IOKit für das eingebaute, wo erlaubt) sowie externe Monitoren ein-/ausschalten (DDC-Standby)
- 🌞 **Werkzeuge-Tab** — Helligkeit aller angeschlossenen Displays auf Hardwareebene (DDC/CI für externe, IOKit für das eingebaute, wo erlaubt) sowie externen Monitoren ein-/ausschalten (DDC-Standby)
- 📊 **Verbrauchs-Tab** — Claude Code, Codex und SayKnow CLI, gelesen aus den Sitzungsprotokollen, die sie ohnehin lokal schreiben. Zeigt den 5-Stunden-Abrechnungsblock (Restzeit, Verbrauchsrate) sowie die von der CLI aufgezeichneten echten 5-Stunden- und Wochenwerte samt Reset-Zeit. Ein bereits zurückgesetztes Fenster wird durchgestrichen und nie als aktueller Stand gezeigt. Ohne Netzwerk und ohne zusätzliche Anmeldung
- 📋 **Zwischenablage-Tab** — im Hintergrund erfasster Verlauf, durchsuchbar über Text und Notizen, mit Anheften, Senden an die Übersetzung und zweistufigem Löschen. Leeres, OTP-artige Strings und PEM-Schlüsselblöcke werden nie gespeichert
- 💬 **Chat-Tab** — leichtes Q&A im selben Fenster, Multi-Konversation in der Seitenleiste, pro Nachricht: regenerieren / bearbeiten / kopieren / stoppen
- 🤖 **Mehrere Anbieter** — OpenRouter / OCP / beliebiger OpenAI-kompatibler Endpoint
- 📦 **OCP-Einrichtung per Klick** — die App führt `git clone → npm install → setup.mjs` selbst aus, mit Live-Log
- 📐 **Kompakter Horizontalmodus** — 720×240 nebeneinander, fürs Dauerhaft-Offenhalten gedacht
- 🪟 **Fenstergröße umschalten** — Kompakt ↔ Normal direkt im Header
- ⚡ **Auto-Übersetzung** — 1,5 s nach Tippen-Stopp
- ⌨️ **Manueller Modus** — nur bei `⌘⏎` oder Übersetzen-Button
- 🪄 **Verfeinern** — Förmlich / Casual / Kürzer / Geschäftlich / Wörtlich + freier Prompt
- 🌐 **OpenRouter BYOK** — durchsuchbare Combobox mit 360+ Modellen
- 🔁 **Fallback-Modell** — OpenRouter wechselt automatisch
- ⏹ **Stopp** — laufenden Aufruf abbrechen
- 📋 **Zwischenablage automatisch** — `⌘⇧T` füllt das Eingabefeld
- 🕘 **Verlauf** — durchsuchbar, anheften möglich
- 📌 **Fenster anheften** — Auto-Hide deaktivieren
- 📚 **Glossar** — konsistente Übersetzung von Eigennamen
- ✏️ **System-Prompt anpassbar**
- 💰 **Verbrauch** — Tokens und Kosten pro Tag/Monat
- 🌓 **Hell / dunkel / System**
- 🌍 **8 UI-Sprachen** — automatische Erkennung
- 🔄 **36 Übersetzungssprachen**
- 🔒 **macOS Keychain** — API-Schlüssel AES-256-verschlüsselt

## Anforderungen

- macOS 11.0 (Big Sur) oder neuer
- Apple Silicon (aarch64)
- OpenRouter API-Key — [openrouter.ai/keys](https://openrouter.ai/keys)

## Installation

### Option 1 — Vorgefertigtes DMG (empfohlen)

1. Lade `SayKnow Kit_x.x.x_aarch64.dmg` von [Releases](https://github.com/jaybeyond/sayknow-kit/releases) herunter.
2. DMG öffnen, SayKnow Kit.app nach `/Applications` ziehen.
3. Da nicht signiert, blockiert Gatekeeper beim ersten Start:
   ```bash
   xattr -dr com.apple.quarantine "/Applications/SayKnow Kit.app"
   ```

### Option 2 — Aus dem Quellcode bauen

```bash
git clone https://github.com/jaybeyond/sayknow-kit.git
cd sayknow-kit
pnpm install
pnpm tauri build
```

## Nutzung

1. Ein kleines Symbol erscheint in der Menüleiste (neben der Uhr). Nicht im Dock.
2. Klick auf das Symbol → OpenRouter API-Key eingeben → **Verbinden & starten**.
3. Der Schlüssel wird in macOS Keychain gespeichert.
4. Symbol klicken oder `⌘⇧T` → tippen → Auto-Übersetzung nach 1,5 s.

### Tastenkürzel

| Kürzel | Aktion |
|---|---|
| `⌘⇧T` | Fenster ein/aus (global) |
| `⌘⏎` | Sofort übersetzen (manueller Modus) |

### Einstellungen (separates Fenster)

⚙️ → **Einstellungen** öffnet ein Fenster mit Seitenleiste:
- **Allgemein** — Modus, Zwischenablage, Anheften, Theme, App-Sprache
- **Verbindung** — Haupt-/Fallback-Modell, Abmelden
- **Glossar** — Begriffspaare
- **System-Prompt** — Übersetzen/Verfeinern-Prompts editieren
- **Verbrauch** — Tokens und Kosten
- **Über**

## Sicherheit

Dein OpenRouter-Key ist abrechnungsrelevant — SayKnow Kit speichert ihn nie im Klartext:

- macOS **Keychain** (`com.sayknow.app`)
- AES-256-Verschlüsselung mit Schlüssel aus deinem macOS-Login-Passwort
- Andere Apps, die ihn lesen wollen, lösen einen Allow/Deny-Prompt aus

## Lizenz

[MIT](LICENSE) — Details im [englischen README](README.md)
