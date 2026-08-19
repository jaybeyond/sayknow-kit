<div align="center">

# SayKnow Kit

**Kit de IA en la barra de menú — traducción, chat y portapapeles en una sola ventana.**

`say` (decir) + `know` (saber) — díselo, lo entenderá al instante.

[한국어](README.ko.md) · [English](README.md) · [日本語](README.ja.md) · [中文](README.zh.md) · **Español** · [Français](README.fr.md) · [Deutsch](README.de.md) · [Tiếng Việt](README.vi.md)

[![macOS](https://img.shields.io/badge/macOS-11.0%2B-black?logo=apple)](https://www.apple.com/macos/)
[![Tauri](https://img.shields.io/badge/Tauri-2-FFC131?logo=tauri&logoColor=black)](https://tauri.app)
[![License: MIT](https://img.shields.io/badge/License-MIT-green.svg)](LICENSE)

</div>

---

## Resumen

SayKnow Kit vive en la **barra de menú de macOS**. Un atajo abre una ventana pequeña con las tres herramientas que usas a diario: **traducción** (se lanza sola cuando dejas de escribir), **chat** e **historial del portapapeles**. Adiós a saltar entre pestañas y pegar texto.

**Tres proveedores** en la misma ventana (OpenRouter / OCP / endpoint personalizado). Solo con OpenRouter ya tienes acceso a más de 360 modelos (GPT-4o, Claude, Gemini, Llama, etc.) con una única clave.

## Funciones

- 💬 **Pestaña de chat** — Q&R ligero en la misma ventana, barra lateral multi-conversación, acciones por mensaje: regenerar / editar / copiar / detener
- 🤖 **Multi-proveedor** — OpenRouter / OCP / cualquier endpoint compatible con OpenAI
- 📦 **Instalación de OCP con un clic** — la app ejecuta `git clone → npm install → setup.mjs` por ti, con logs en vivo
- 📐 **Modo compacto horizontal** — 720×240 lado a lado, pensado para tenerlo siempre abierto
- 🪟 **Cambio de tamaño** — alterna compacto ↔ normal desde la cabecera
- ⚡ **Traducción automática** — 1,5 s después de parar de escribir
- ⌨️ **Modo manual** — solo con `⌘⏎` o el botón Traducir (ahorra coste)
- 🪄 **Refinar** — Formal / Casual / Corto / Negocios / Literal + prompt libre
- 🌐 **OpenRouter BYOK** — combobox con búsqueda en 360+ modelos
- 🔁 **Modelo de respaldo** — OpenRouter reintenta si el principal falla
- ⏹ **Detener** — cancela una llamada en curso
- 📋 **Pegado automático** — `⌘⇧T` rellena la entrada con el portapapeles
- 🕘 **Historial** — buscable, fija entradas para conservarlas
- 📌 **Fijar ventana** — desactiva el auto-ocultar
- 📚 **Glosario** — traducciones consistentes para nombres y términos propios
- ✏️ **Prompt del sistema personalizable**
- 💰 **Seguimiento de uso** — tokens y coste diarios/mensuales
- 🌓 **Claro / oscuro / sistema** — sigue el tema del SO
- 🌍 **8 idiomas de interfaz** — detección automática
- 🔄 **36 idiomas de traducción**
- 🔒 **macOS Keychain** — clave API cifrada con AES-256

## Requisitos

- macOS 11.0 (Big Sur) o superior
- Apple Silicon (aarch64)
- Clave API de OpenRouter — [openrouter.ai/keys](https://openrouter.ai/keys)

## Instalación

### Opción 1 — DMG precompilado (recomendado)

1. Descarga `SayKnow Kit_x.x.x_aarch64.dmg` desde [Releases](https://github.com/jaybeyond/sayknow-kit/releases).
2. Abre el DMG y arrastra SayKnow Kit.app a `/Applications`.
3. La build no está firmada, así que Gatekeeper lo bloqueará la primera vez:
   ```bash
   xattr -dr com.apple.quarantine "/Applications/SayKnow Kit.app"
   ```

### Opción 2 — Compilar desde el código

```bash
git clone https://github.com/jaybeyond/sayknow-kit.git
cd sayknow-kit
pnpm install
pnpm tauri build
```

## Uso

1. Aparece un pequeño icono en la barra de menú (junto al reloj). No aparece en el Dock.
2. Haz clic en el icono → introduce la clave de OpenRouter → **Conectar y empezar**.
3. La clave se guarda en macOS Keychain automáticamente.
4. Clic en el icono o `⌘⇧T` para abrir → escribe → traducción automática 1,5 s después.

### Atajos

| Atajo | Acción |
|---|---|
| `⌘⇧T` | Abrir/cerrar la ventana (global) |
| `⌘⏎` | Traducir al instante (modo manual) |

### Ajustes (ventana aparte)

⚙️ → **Ajustes** abre una ventana con barra lateral:
- **General** — modo, portapapeles, pin, tema, idioma de la app
- **Conexión** — modelo principal/respaldo, cerrar sesión
- **Glosario** — pares de términos
- **Prompt del sistema** — editar prompts de traducción/refinar
- **Uso** — tokens y coste
- **Acerca de**

## Seguridad

La clave API es una credencial facturable, así que SayKnow Kit nunca la guarda en texto plano:

- macOS **Keychain** (`com.sayknow.app`)
- Cifrado AES-256 derivado de tu contraseña de inicio de sesión
- Otras apps que intenten leerla disparan un prompt del sistema (Permitir / Denegar)

## Licencia

[MIT](LICENSE) — más detalles en el [README en inglés](README.md)
