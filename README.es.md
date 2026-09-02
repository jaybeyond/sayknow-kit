<div align="center">

<img src="public/sayo-logo.png" alt="Sayo, la mascota pulpo de SayKnow Kit" width="180" />

# SayKnow Kit

**Kit de IA en la barra de menú — traducción, chat y portapapeles en una sola ventana.**

`say` (decir) + `know` (saber) — díselo, lo entenderá al instante.

[한국어](README.ko.md) · [English](README.md) · [日本語](README.ja.md) · [中文](README.zh.md) · **Español** · [Français](README.fr.md) · [Deutsch](README.de.md) · [Tiếng Việt](README.vi.md)

[![macOS](https://img.shields.io/badge/macOS-11.0%2B-black?logo=apple)](https://www.apple.com/macos/) · [![Windows](https://img.shields.io/badge/Windows-10%2F11-blue?logo=windows)](https://www.microsoft.com/windows/)
[![Tauri](https://img.shields.io/badge/Tauri-2-FFC131?logo=tauri&logoColor=black)](https://tauri.app)
[![License: MIT](https://img.shields.io/badge/License-MIT-green.svg)](LICENSE)

</div>

---

## Resumen

SayKnow Kit funciona en macOS y Windows: aparece en la **barra de menú de macOS** o en la **bandeja del sistema de Windows**. Un atajo abre una pequeña ventana con las tres herramientas que usas a diario: **traducción**, **chat** e **historial del portapapeles**. Adiós a saltar entre pestañas y pegar texto.

**Tres proveedores** en la misma ventana (OpenRouter / OCP / endpoint personalizado). Solo con OpenRouter ya tienes acceso a más de 360 modelos (GPT-4o, Claude, Gemini, Llama, etc.) con una única clave.

## Funciones

- 🌞 **Pestaña de herramientas** — en macOS controla el brillo de las pantallas (DDC/CI para externas; IOKit para la retroiluminación integrada cuando sea compatible). En los Mac compatibles más recientes donde IOKit no está disponible, usa la automatización de Accesibilidad del Centro de control. Tanto IOKit como la automatización del Centro de control son funciones exclusivas de macOS; las capacidades DDC externas dependen del hardware.
- 📊 **Estado del sistema** — CPU, memoria y disco del sistema; la temperatura del paquete de CPU se muestra como «no disponible» en v0.2.5 porque no hay un adaptador de temperatura verificado, y también en sistemas no compatibles.
- 📊 **Pestaña de uso** — Claude Code, Codex y SayKnow CLI leídos de los registros de sesión que ya escriben en local. Muestra el bloque de facturación de 5 h (tiempo restante y ritmo de consumo) y los porcentajes reales de 5 h y semanales que registra la CLI, con su hora de reinicio. Una ventana ya reiniciada aparece tachada y nunca como tu nivel actual. Sin red y sin inicio de sesión adicional
- 📋 **Pestaña de portapapeles** — historial capturado en segundo plano, buscable por texto y notas, con fijado, envío a traducir y borrado en dos niveles. Vacíos, cadenas tipo OTP y bloques de clave PEM nunca se guardan
- 💬 **Pestaña de chat** — preguntas y respuestas ligeras en la misma ventana, barra lateral multi-conversación, acciones por mensaje: regenerar / editar / copiar / detener
- 🤖 **Multi-proveedor** — OpenRouter / OCP / cualquier endpoint compatible con OpenAI
- 📦 **Instalación de OCP con un clic** — la app ejecuta `git clone → npm install → setup.mjs` por ti, con logs en vivo
- 📐 **Modo compacto horizontal** — 720×240 lado a lado, pensado para tenerlo siempre abierto
- 🪟 **Cambio de tamaño** — alterna compacto ↔ normal desde la cabecera
- ⚡ **Traducción automática** — 1,5 s después de parar de escribir
- ⌨️ **Modo manual** — solo con `⌘⏎` (macOS), `Ctrl+Enter` (Windows) o el botón Traducir (ahorra coste)
- 🪄 **Refinar** — Formal / Casual / Corto / Negocios / Literal + prompt libre
- 🌐 **OpenRouter BYOK** — combobox con búsqueda en 360+ modelos
- 🔁 **Modelo de respaldo** — OpenRouter reintenta si el principal falla
- ⏹ **Detener** — cancela una llamada en curso
- 📋 **Pegado automático** — `⌘⇧T` (macOS) / `Ctrl+Shift+T` (Windows) rellena la entrada con el portapapeles
- 🕘 **Historial** — buscable, fija entradas para conservarlas
- 📌 **Fijar ventana** — desactiva el auto-ocultar
- 📚 **Glosario** — traducciones consistentes para nombres y términos propios
- ✏️ **Prompt del sistema personalizable**
- 💰 **Seguimiento de uso** — tokens y coste diarios/mensuales
- 🌓 **Claro / oscuro / sistema** — sigue el tema del SO
- 🌍 **8 idiomas de interfaz** — detección automática
- 🔄 **36 idiomas de traducción**
- 🔒 **Almacenamiento seguro** — credenciales en el almacén del sistema (macOS Keychain / Windows Credential Manager)

## Requisitos

- macOS 11.0 (Big Sur) o superior en Apple Silicon (aarch64)
- Windows 10/11 en x64
- Clave API de OpenRouter — [openrouter.ai/keys](https://openrouter.ai/keys)

## Instalación

### macOS — DMG

1. Descarga el DMG macOS aarch64 de la [versión oficial en GitHub Releases](https://github.com/jaybeyond/sayknow-kit/releases).
2. Abre el DMG y arrastra SayKnow Kit.app a `/Applications`.
3. La aplicación v0.2.5 solo lleva una firma **ad hoc**, sin Developer ID ni notarización; las advertencias de Gatekeeper son esperadas.
4. Verifica el archivo `SHA256SUMS.txt` publicado junto a la versión antes de abrirlo.

### Windows — EXE o MSI

1. Descarga el instalador NSIS `.exe` o el instalador MSI `.msi` x64 de la versión oficial en GitHub Releases.
2. Windows SmartScreen puede mostrar una advertencia porque v0.2.5 no está firmado; es normal. No hay Authenticode.
3. Verifica `SHA256SUMS.txt` antes de ejecutar el instalador y sigue el asistente.
4. Para desinstalar, usa **Configuración → Aplicaciones → Aplicaciones instaladas → SayKnow Kit → Desinstalar**.

Para desinstalar en macOS, elimina SayKnow Kit.app de `/Applications` y borra sus datos del Keychain si ya no los necesitas.

Descarga únicamente desde la versión oficial de [GitHub Releases](https://github.com/jaybeyond/sayknow-kit/releases).

### Opción 2 — Compilar desde el código

```bash
git clone https://github.com/jaybeyond/sayknow-kit.git
cd sayknow-kit
pnpm install
pnpm tauri build
```

## Uso

1. En macOS aparece un icono en la barra de menú; en Windows, en la bandeja del sistema. La aplicación permanece allí y puede abrirse desde el menú.
2. Haz clic en el icono → introduce la clave de OpenRouter → **Conectar y empezar**. La clave se guarda en el almacén de credenciales del sistema.
3. Haz clic en el icono o usa `⌘⇧T` (macOS) / `Ctrl+Shift+T` (Windows) para abrir la ventana → escribe → traducción automática 1,5 s después.

### Atajos

| Atajo | Acción |
|---|---|
| `⌘⇧T` (macOS) | Abrir/cerrar la ventana (global) |
| `Ctrl+Shift+T` (Windows) | Abrir/cerrar la ventana (global) |
| `⌘⏎` (macOS) / `Ctrl+Enter` (Windows) | Traducir al instante (modo manual) |

### Ajustes (ventana aparte)

⚙️ → **Ajustes** abre una ventana con barra lateral:
- **General** — modo, portapapeles, pin, tema, idioma de la app
- **Conexión** — modelo principal/respaldo, cerrar sesión
- **Glosario** — pares de términos
- **Prompt del sistema** — editar prompts de traducción/refinar
- **Uso** — tokens y coste
- **Acerca de**

## Seguridad

La clave API es una credencial facturable y se guarda únicamente en el almacenamiento seguro del sistema:

- macOS **Keychain** y Windows **Credential Manager**
- Nunca se almacena en texto plano ni se deriva del inicio de sesión
- Descarga solo desde GitHub Releases oficial y verifica `SHA256SUMS.txt`
- La aplicación macOS v0.2.5 usa firma ad hoc sin Developer ID ni notarización; los instaladores de Windows no tienen firma Authenticode. Las advertencias de Gatekeeper y SmartScreen son esperadas.

## Licencia

[MIT](LICENSE) — más detalles en el [README en inglés](README.md)
