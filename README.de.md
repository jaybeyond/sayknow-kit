<div align="center">

<img src="public/sayo-logo.png" alt="Sayo, das Oktopus-Maskottchen von SayKnow Kit" width="180" />

# SayKnow Kit

**KI-Kit in der Menüleiste — Übersetzen, Chat und Zwischenablage in einem Popover.**

`say` (sagen) + `know` (wissen) — sag's, er versteht.

[한국어](README.ko.md) · [English](README.md) · [日本語](README.ja.md) · [中文](README.zh.md) · [Español](README.es.md) · [Français](README.fr.md) · **Deutsch** · [Tiếng Việt](README.vi.md)

[![macOS](https://img.shields.io/badge/macOS-11.0%2B-black?logo=apple)](https://www.apple.com/macos/) · [![Windows](https://img.shields.io/badge/Windows-10%2F11-blue?logo=windows)](https://www.microsoft.com/windows/)
[![Tauri](https://img.shields.io/badge/Tauri-2-FFC131?logo=tauri&logoColor=black)](https://tauri.app)
[![License: MIT](https://img.shields.io/badge/License-MIT-green.svg)](LICENSE)

</div>

---

## Übersicht

SayKnow Kit läuft auf macOS und Windows: Es erscheint in der **macOS-Menüleiste** oder im **Windows-Infobereich**. Ein Tastenkürzel öffnet ein kleines Fenster mit den drei Werkzeugen für den Alltag: **Übersetzen**, **Chat** und **Zwischenablage-Verlauf**.

**Drei Anbieter** im selben Fenster (OpenRouter / OCP / Custom-Endpoint). Schon OpenRouter allein bringt Zugriff auf über 360 Modelle (GPT-4o, Claude, Gemini, Llama, ...) mit nur einem Schlüssel.

## Funktionen

- 🌞 **Werkzeuge-Tab** — unter macOS steuert die App die Helligkeit (DDC/CI für externe Displays; IOKit für die integrierte Hintergrundbeleuchtung, sofern unterstützt). Auf neueren unterstützten Macs, auf denen IOKit nicht verfügbar ist, wird die Bedienungshilfe-Automatisierung des Control Centers verwendet. IOKit und diese Automatisierung sind macOS-spezifisch; DDC-Funktionen hängen von der Hardware ab.
- 📊 **Systemstatus** — CPU, Arbeitsspeicher und Systemdatenträger; die CPU-Package-Temperatur wird in v0.2.6 mangels verifiziertem Temperaturadapter als „nicht verfügbar“ angezeigt, ebenso auf nicht unterstützten Systemen.
- 📊 **Verbrauchs-Tab** — Claude Code, Codex und SayKnow CLI, gelesen aus den Sitzungsprotokollen, die sie ohnehin lokal schreiben. Zeigt den 5-Stunden-Abrechnungsblock (Restzeit, Verbrauchsrate) sowie die von der CLI aufgezeichneten echten 5-Stunden- und Wochenwerte samt Reset-Zeit. Ein bereits zurückgesetztes Fenster wird durchgestrichen und nie als aktueller Stand gezeigt. Ohne Netzwerk und ohne zusätzliche Anmeldung
- 📋 **Zwischenablage-Tab** — im Hintergrund erfasster Verlauf, durchsuchbar über Text und Notizen, mit Anheften, Senden an die Übersetzung und zweistufigem Löschen. Leeres, OTP-artige Strings und PEM-Schlüsselblöcke werden nie gespeichert
- 💬 **Chat-Tab** — leichtes Q&A im selben Fenster, Multi-Konversation in der Seitenleiste, pro Nachricht: regenerieren / bearbeiten / kopieren / stoppen
- 🤖 **Mehrere Anbieter** — OpenRouter / OCP / beliebiger OpenAI-kompatibler Endpoint
- 📦 **OCP-Einrichtung per Klick** — die App führt `git clone → npm install → setup.mjs` selbst aus, mit Live-Log
- 📐 **Kompakter Horizontalmodus** — 720×240 nebeneinander, fürs Dauerhaft-Offenhalten gedacht
- 🪟 **Fenstergröße umschalten** — Kompakt ↔ Normal direkt im Header
- ⚡ **Auto-Übersetzung** — 1,5 s nach Tippen-Stopp
- ⌨️ **Manueller Modus** — nur mit `⌘⏎` (macOS), `Ctrl+Enter` (Windows) oder dem Übersetzen-Button
- 🪄 **Verfeinern** — Förmlich / Casual / Kürzer / Geschäftlich / Wörtlich + freier Prompt
- 🌐 **OpenRouter BYOK** — durchsuchbare Combobox mit 360+ Modellen
- 🔁 **Fallback-Modell** — OpenRouter wechselt automatisch
- ⏹ **Stopp** — laufenden Aufruf abbrechen
- 📋 **Zwischenablage automatisch** — `⌘⇧T` (macOS) / `Ctrl+Shift+T` (Windows) füllt das Eingabefeld
- 🕘 **Verlauf** — durchsuchbar, anheften möglich
- 📌 **Fenster anheften** — Auto-Hide deaktivieren
- 📚 **Glossar** — konsistente Übersetzung von Eigennamen
- ✏️ **System-Prompt anpassbar**
- 💰 **Verbrauch** — Tokens und Kosten pro Tag/Monat
- 🌓 **Hell / dunkel / System**
- 🌍 **8 UI-Sprachen** — automatische Erkennung
- 🔄 **36 Übersetzungssprachen**
- 🔒 **Sicherer Speicher** — Anmeldedaten im System-Credential-Speicher (macOS Keychain / Windows Credential Manager)

## Anforderungen

- macOS 11.0 (Big Sur) oder neuer auf Apple Silicon (aarch64)
- Windows 10/11 x64
- OpenRouter API-Key — [openrouter.ai/keys](https://openrouter.ai/keys)

## Installation

### macOS — DMG

1. Lade das macOS-aarch64-DMG vom [offiziellen GitHub-Release](https://github.com/jaybeyond/sayknow-kit/releases) herunter.
2. Öffne das DMG und ziehe SayKnow Kit.app nach `/Applications`.
3. Die v0.2.6-App ist nur **ad hoc** signiert, ohne Developer ID oder Notarisierung; Gatekeeper-Warnungen sind zu erwarten.
4. Prüfe vor dem Öffnen die veröffentlichte `SHA256SUMS.txt`.

### Windows — EXE oder MSI

1. Lade den x64-NSIS-Installer (`.exe`) oder den x64-MSI-Installer (`.msi`) ausschließlich vom offiziellen GitHub-Release.
2. Windows SmartScreen kann warnen, da v0.2.6 unsigniert ist; das ist erwartbar. Kein Authenticode.
3. Prüfe `SHA256SUMS.txt` und führe den Installationsassistenten aus.
4. Deinstallation: **Einstellungen → Apps → Installierte Apps → SayKnow Kit → Deinstallieren**.

Unter macOS zum Deinstallieren SayKnow Kit.app aus `/Applications` löschen und bei Bedarf die Keychain-Daten entfernen.

Lade ausschließlich vom [offiziellen GitHub-Release](https://github.com/jaybeyond/sayknow-kit/releases).

### Option 2 — Aus dem Quellcode bauen

```bash
git clone https://github.com/jaybeyond/sayknow-kit.git
cd sayknow-kit
pnpm install
pnpm tauri build
```

## Nutzung

1. Ein Symbol erscheint in der macOS-Menüleiste oder im Windows-Infobereich und bleibt dort verfügbar.
2. Klicke auf das Symbol → OpenRouter API-Key eingeben → **Verbinden & starten**. Der Schlüssel wird im System-Credential-Speicher abgelegt.
3. Symbol anklicken oder `⌘⇧T` (macOS) / `Ctrl+Shift+T` (Windows) drücken, um das Fenster zu öffnen → tippen → Auto-Übersetzung nach 1,5 s.

### Tastenkürzel

| Kürzel | Aktion |
|---|---|
| `⌘⇧T` (macOS) | Fenster global öffnen/schließen |
| `Ctrl+Shift+T` (Windows) | Fenster global öffnen/schließen |
| `⌘⏎` (macOS) / `Ctrl+Enter` (Windows) | Sofort übersetzen (manueller Modus) |

### Einstellungen (separates Fenster)

⚙️ → **Einstellungen** öffnet ein Fenster mit Seitenleiste:
- **Allgemein** — Modus, Zwischenablage, Anheften, Theme, App-Sprache
- **Verbindung** — Haupt-/Fallback-Modell, Abmelden
- **Glossar** — Begriffspaare
- **System-Prompt** — Übersetzen/Verfeinern-Prompts editieren
- **Verbrauch** — Tokens und Kosten
- **Über**

## Sicherheit

Der OpenRouter-Key ist abrechnungsrelevant und wird ausschließlich im sicheren System-Speicher abgelegt:

- macOS **Keychain** und Windows **Credential Manager**
- Nie im Klartext und nicht aus dem Login-Passwort abgeleitet
- Nur vom offiziellen GitHub-Release laden und `SHA256SUMS.txt` prüfen
- Die macOS-App v0.2.6 ist ad hoc signiert, ohne Developer ID oder Notarisierung; die Windows-Installer haben keine Authenticode-Signatur. Gatekeeper- und SmartScreen-Warnungen sind zu erwarten.

## Lizenz

[MIT](LICENSE) — Details im [englischen README](README.md)
