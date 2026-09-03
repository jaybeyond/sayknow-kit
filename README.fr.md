<div align="center">

<img src="public/sayo-logo.png" alt="Sayo, la mascotte pieuvre de SayKnow Kit" width="180" />

# SayKnow Kit

**Kit IA dans la barre de menu — traduction, chat et presse-papiers dans une seule fenêtre.**

`say` (dire) + `know` (savoir) — dites-le, il comprendra.

[한국어](README.ko.md) · [English](README.md) · [日本語](README.ja.md) · [中文](README.zh.md) · [Español](README.es.md) · **Français** · [Deutsch](README.de.md) · [Tiếng Việt](README.vi.md)

[![macOS](https://img.shields.io/badge/macOS-11.0%2B-black?logo=apple)](https://www.apple.com/macos/) · [![Windows](https://img.shields.io/badge/Windows-10%2F11-blue?logo=windows)](https://www.microsoft.com/windows/)
[![Tauri](https://img.shields.io/badge/Tauri-2-FFC131?logo=tauri&logoColor=black)](https://tauri.app)
[![License: MIT](https://img.shields.io/badge/License-MIT-green.svg)](LICENSE)

</div>

---

## Aperçu

SayKnow Kit fonctionne sur macOS et Windows : il apparaît dans la **barre de menu macOS** ou la **zone de notification Windows**. Un raccourci ouvre une petite fenêtre avec les trois outils du quotidien : **traduction**, **chat** et **historique du presse-papiers**.

**Trois fournisseurs** dans la même fenêtre (OpenRouter / OCP / endpoint personnalisé). OpenRouter seul donne déjà accès à plus de 360 modèles (GPT-4o, Claude, Gemini, Llama, ...) avec une unique clé.

## Fonctionnalités

- 🌞 **Onglet outils** — sur macOS, contrôle la luminosité (DDC/CI pour les écrans externes ; IOKit pour le rétroéclairage intégré lorsqu'il est pris en charge). Sur les Mac récents compatibles où IOKit n'est pas disponible, l'automatisation d'Accessibilité du Centre de contrôle est utilisée. IOKit et cette automatisation sont réservés à macOS ; les capacités DDC varient selon le matériel.
- 📊 **État du système** — CPU, mémoire et disque système ; sur macOS (Apple Silicon), la température du SoC est lue via des capteurs sans privilèges. Elle reste « indisponible » sous Windows et lorsqu'aucun capteur fiable n'est lisible.
- 📊 **Usage (dans l'onglet Outils)** — Claude Code, Codex et SayKnow CLI lus dans les journaux de session qu'ils écrivent déjà en local. Affiche le bloc de facturation de 5 h (temps restant, rythme de consommation) et les pourcentages réels 5 h et hebdomadaires enregistrés par la CLI, avec leur heure de réinitialisation. Une fenêtre déjà réinitialisée est barrée, jamais présentée comme votre niveau actuel. Sans réseau ni connexion supplémentaire
- 📋 **Onglet presse-papiers** — historique capturé en arrière-plan, cherchable sur le texte et les notes, avec épinglage, envoi vers la traduction et effacement à deux niveaux. Vides, chaînes de type OTP et blocs de clé PEM ne sont jamais stockés
- 💬 **Onglet Chat** — questions-réponses légères dans la même fenêtre, barre latérale multi-conversation, actions par message : régénérer / éditer / copier / arrêter
- 🤖 **Multi-fournisseur** — OpenRouter / OCP / tout endpoint compatible OpenAI
- 📦 **Installation OCP en un clic** — l'app lance `git clone → npm install → setup.mjs` pour vous, logs en direct
- 📐 **Mode compact horizontal** — 720×240 côte à côte, conçu pour rester ouvert en permanence
- 🪟 **Bascule de taille** — compact ↔ normal depuis l'en-tête
- ⚡ **Traduction automatique** — 1,5 s après l'arrêt de la frappe
- ⌨️ **Mode manuel** — seulement avec `⌘⏎` (macOS), `Ctrl+Enter` (Windows) ou le bouton (économise les tokens)
- 🪄 **Affiner** — Formel / Décontracté / Plus court / Pro / Littéral + prompt libre
- 🌐 **OpenRouter BYOK** — recherche dans 360+ modèles
- 🔁 **Modèle de secours** — OpenRouter bascule si le principal échoue
- ⏹ **Arrêter** — annule un appel en cours
- 📋 **Coller automatiquement** — `⌘⇧T` (macOS) / `Ctrl+Shift+T` (Windows) remplit l'entrée depuis le presse-papiers
- 🕘 **Historique** — recherche, épingler des entrées
- 📌 **Épingler la fenêtre** — désactive l'auto-masquage
- 📚 **Glossaire** — traductions cohérentes pour les noms propres
- ✏️ **Prompt système personnalisable**
- 💰 **Suivi de l'usage** — tokens et coût quotidien/mensuel
- 🌓 **Clair / sombre / système**
- 🌍 **8 langues d'interface** — détection auto
- 🔄 **36 langues de traduction**
- 🔒 **Stockage sécurisé** — identifiants dans le stockage système (macOS Keychain / Windows Credential Manager)

## Prérequis

- macOS 11.0 (Big Sur) ou plus récent sur Apple Silicon (aarch64)
- Windows 10/11 x64
- Clé API OpenRouter — [openrouter.ai/keys](https://openrouter.ai/keys)

## Installation

### macOS — DMG

1. Téléchargez le DMG macOS aarch64 depuis la [version officielle GitHub Releases](https://github.com/jaybeyond/sayknow-kit/releases).
2. Ouvrez le DMG et glissez SayKnow Kit.app dans `/Applications`.
3. L’application v0.2.13 utilise uniquement une signature **ad hoc**, sans Developer ID ni notarisation ; les avertissements Gatekeeper sont attendus.
4. macOS 13+ n'accepte plus le contournement par clic droit : lancez-la une fois, puis **Réglages Système → Confidentialité et sécurité → Ouvrir quand même**. Ou lancez `xattr -dr com.apple.quarantine "/Applications/SayKnow Kit.app"`. Ouverte depuis le DMG, l'app s'exécute depuis une copie aléatoire en lecture seule où l'autorisation d'accessibilité n'est jamais conservée. Si macOS redemande l'accessibilité malgré l'autorisation, l'entrée enregistrée ne correspond plus à la signature ad hoc mise à jour : lancez `tccutil reset Accessibility com.sayknow.app` puis redémarrez l'app.
5. Vérifiez `SHA256SUMS.txt` publié avec la version.

### Windows — EXE ou MSI

1. Téléchargez l'installateur NSIS `.exe` ou l'installateur MSI `.msi` x64 depuis GitHub Releases officiel.
2. Windows SmartScreen peut avertir car v0.2.13 n'est pas signé ; c'est normal. Aucun Authenticode.
3. Vérifiez `SHA256SUMS.txt`, puis lancez l'assistant.
4. Désinstallez via **Paramètres → Applications → Applications installées → SayKnow Kit → Désinstaller**.

Sur macOS, désinstallez en supprimant SayKnow Kit.app de `/Applications` et, si nécessaire, ses données du Keychain.

Téléchargez uniquement depuis la [version officielle GitHub Releases](https://github.com/jaybeyond/sayknow-kit/releases).

### Option 2 — Build depuis les sources

```bash
git clone https://github.com/jaybeyond/sayknow-kit.git
cd sayknow-kit
pnpm install
pnpm tauri build
```

## Utilisation

1. Une icône apparaît dans la barre de menu macOS ou la zone de notification Windows ; l'application y reste accessible.
2. Cliquez sur l'icône → saisissez la clé OpenRouter → **Connecter & démarrer**. Elle est enregistrée dans le stockage d'identifiants du système.
3. Cliquez sur l'icône ou utilisez `⌘⇧T` (macOS) / `Ctrl+Shift+T` (Windows) pour ouvrir la fenêtre → tapez → traduction automatique après 1,5 s.

### Raccourcis

| Raccourci | Action |
|---|---|
| `⌘⇧T` (macOS) | Ouvrir/fermer la fenêtre (global) |
| `Ctrl+Shift+T` (Windows) | Ouvrir/fermer la fenêtre (global) |
| `⌘⏎` (macOS) / `Ctrl+Enter` (Windows) | Traduire immédiatement (mode manuel) |

### Réglages (fenêtre séparée)

⚙️ → **Réglages** ouvre une fenêtre avec barre latérale :
- **Général** — mode, presse-papiers, épinglage, thème, langue de l'app
- **Connexion** — modèle principal/secours, déconnexion
- **Glossaire** — paires de termes
- **Prompt système** — édition des prompts traduire/affiner
- **Usage** — tokens et coût
- **À propos**

## Sécurité

La clé API OpenRouter est une donnée facturable et est conservée uniquement dans le stockage sécurisé du système :

- macOS **Keychain** et Windows **Credential Manager**
- Jamais en clair et jamais dérivée du mot de passe de session
- Téléchargez depuis GitHub Releases officiel et vérifiez `SHA256SUMS.txt`
- L’application macOS v0.2.13 est signée ad hoc, sans Developer ID ni notarisation ; les installateurs Windows ne sont pas signés avec Authenticode. Les avertissements Gatekeeper et SmartScreen sont attendus.

## Licence

[MIT](LICENSE) — voir le [README anglais](README.md) pour plus de détails
