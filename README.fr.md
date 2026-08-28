<div align="center">

# SayKnow Kit

**Kit IA dans la barre de menu — traduction, chat et presse-papiers dans une seule fenêtre.**

`say` (dire) + `know` (savoir) — dites-le, il comprendra.

[한국어](README.ko.md) · [English](README.md) · [日本語](README.ja.md) · [中文](README.zh.md) · [Español](README.es.md) · **Français** · [Deutsch](README.de.md) · [Tiếng Việt](README.vi.md)

[![macOS](https://img.shields.io/badge/macOS-11.0%2B-black?logo=apple)](https://www.apple.com/macos/)
[![Tauri](https://img.shields.io/badge/Tauri-2-FFC131?logo=tauri&logoColor=black)](https://tauri.app)
[![License: MIT](https://img.shields.io/badge/License-MIT-green.svg)](LICENSE)

</div>

---

## Aperçu

SayKnow Kit vit dans la **barre de menu macOS**. Un raccourci ouvre une petite fenêtre contenant les trois outils du quotidien : **traduction** (déclenchée dès que vous arrêtez de taper), **chat** et **historique du presse-papiers**. Plus besoin de jongler entre onglets et copier-coller.

**Trois fournisseurs** dans la même fenêtre (OpenRouter / OCP / endpoint personnalisé). OpenRouter seul donne déjà accès à plus de 360 modèles (GPT-4o, Claude, Gemini, Llama, ...) avec une unique clé.

## Fonctionnalités

- 🌞 **Onglet outils** — luminosité de tous les écrans connectés au niveau matériel (DDC/CI pour les externes, IOKit pour l'intégré quand c'est permis) et marche/arrêt des écrans externes (veille DDC)
- 🌞 **Onglet outils** — luminosité de tous les écrans connectés au niveau matériel (DDC/CI pour les externes, IOKit pour l'intégré quand c'est permis) et marche/arrêt des écrans externes (veille DDC)
- 📊 **Onglet usage** — Claude Code, Codex et SayKnow CLI lus dans les journaux de session qu'ils écrivent déjà en local. Affiche le bloc de facturation de 5 h (temps restant, rythme de consommation) et les pourcentages réels 5 h et hebdomadaires enregistrés par la CLI, avec leur heure de réinitialisation. Une fenêtre déjà réinitialisée est barrée, jamais présentée comme votre niveau actuel. Sans réseau ni connexion supplémentaire
- 📋 **Onglet presse-papiers** — historique capturé en arrière-plan, cherchable sur le texte et les notes, avec épinglage, envoi vers la traduction et effacement à deux niveaux. Vides, chaînes de type OTP et blocs de clé PEM ne sont jamais stockés
- 💬 **Onglet Chat** — Q&R léger dans la même fenêtre, barre latérale multi-conversation, actions par message : régénérer / éditer / copier / arrêter
- 🤖 **Multi-fournisseur** — OpenRouter / OCP / tout endpoint compatible OpenAI
- 📦 **Installation OCP en un clic** — l'app lance `git clone → npm install → setup.mjs` pour vous, logs en direct
- 📐 **Mode compact horizontal** — 720×240 côte à côte, conçu pour rester ouvert en permanence
- 🪟 **Bascule de taille** — compact ↔ normal depuis l'en-tête
- ⚡ **Traduction automatique** — 1,5 s après l'arrêt de la frappe
- ⌨️ **Mode manuel** — seulement sur `⌘⏎` ou le bouton (économise les tokens)
- 🪄 **Affiner** — Formel / Décontracté / Plus court / Pro / Littéral + prompt libre
- 🌐 **OpenRouter BYOK** — recherche dans 360+ modèles
- 🔁 **Modèle de secours** — OpenRouter bascule si le principal échoue
- ⏹ **Arrêter** — annule un appel en cours
- 📋 **Coller automatique** — `⌘⇧T` remplit l'entrée depuis le presse-papiers
- 🕘 **Historique** — recherche, épingler des entrées
- 📌 **Épingler la fenêtre** — désactive l'auto-masquage
- 📚 **Glossaire** — traductions cohérentes pour les noms propres
- ✏️ **Prompt système personnalisable**
- 💰 **Suivi de l'usage** — tokens et coût quotidien/mensuel
- 🌓 **Clair / sombre / système**
- 🌍 **8 langues d'interface** — détection auto
- 🔄 **36 langues de traduction**
- 🔒 **macOS Keychain** — clé API chiffrée AES-256

## Prérequis

- macOS 11.0 (Big Sur) ou plus récent
- Apple Silicon (aarch64)
- Clé API OpenRouter — [openrouter.ai/keys](https://openrouter.ai/keys)

## Installation

### Option 1 — DMG (recommandé)

1. Téléchargez `SayKnow Kit_x.x.x_aarch64.dmg` depuis [Releases](https://github.com/jaybeyond/sayknow-kit/releases).
2. Ouvrez le DMG, glissez SayKnow Kit.app dans `/Applications`.
3. Build non signée, Gatekeeper bloquera au premier lancement :
   ```bash
   xattr -dr com.apple.quarantine "/Applications/SayKnow Kit.app"
   ```

### Option 2 — Build depuis les sources

```bash
git clone https://github.com/jaybeyond/sayknow-kit.git
cd sayknow-kit
pnpm install
pnpm tauri build
```

## Utilisation

1. Une petite icône apparaît dans la barre de menu (à côté de l'horloge). Pas dans le Dock.
2. Clic sur l'icône → entrez la clé OpenRouter → **Connecter & démarrer**.
3. La clé est sauvée dans macOS Keychain.
4. Clic sur l'icône ou `⌘⇧T` → tapez → traduction automatique 1,5 s plus tard.

### Raccourcis

| Raccourci | Action |
|---|---|
| `⌘⇧T` | Ouvrir/fermer la fenêtre (global) |
| `⌘⏎` | Traduire immédiatement (mode manuel) |

### Réglages (fenêtre séparée)

⚙️ → **Réglages** ouvre une fenêtre avec barre latérale :
- **Général** — mode, presse-papiers, épinglage, thème, langue de l'app
- **Connexion** — modèle principal/secours, déconnexion
- **Glossaire** — paires de termes
- **Prompt système** — édition des prompts traduire/affiner
- **Usage** — tokens et coût
- **À propos**

## Sécurité

Votre clé API OpenRouter est une donnée facturable, jamais stockée en clair :

- macOS **Keychain** (`com.sayknow.app`)
- Chiffrement AES-256 dérivé du mot de passe de session macOS
- Toute autre app qui tente de lire la clé déclenche une invite système

## Licence

[MIT](LICENSE) — voir le [README anglais](README.md) pour plus de détails
