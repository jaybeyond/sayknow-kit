<div align="center">

<img src="public/sayo-logo.png" alt="SayKnow Kit 章鱼吉祥物 Sayo" width="180" />

# SayKnow Kit

**跨平台 AI 工具包 — macOS 菜单栏与 Windows 系统托盘中的翻译、聊天、剪贴板弹窗。**

`say`(说) + `know`(懂) — 一说就懂。

[한국어](README.ko.md) · [English](README.md) · [日本語](README.ja.md) · **中文** · [Español](README.es.md) · [Français](README.fr.md) · [Deutsch](README.de.md) · [Tiếng Việt](README.vi.md)

[![macOS](https://img.shields.io/badge/macOS-11%2B-black?logo=apple)](https://www.apple.com/macos/) [![Windows](https://img.shields.io/badge/Windows-10%2F11-blue?logo=windows)](https://www.microsoft.com/windows)
[![Tauri](https://img.shields.io/badge/Tauri-2-FFC131?logo=tauri&logoColor=black)](https://tauri.app)
[![License: MIT](https://img.shields.io/badge/License-MIT-green.svg)](LICENSE)

</div>

---

## 简介

SayKnow Kit 常驻 **macOS 菜单栏或 Windows 系统托盘**。一个快捷键打开弹窗，里面是翻译、聊天和剪贴板历史三件工具。

**三种提供商**(OpenRouter / OCP / 自定义端点)可在同一界面切换;仅 OpenRouter 即可一把密钥调用 GPT-4o、Claude、Gemini、Llama 等 360+ 模型。

## 主要功能

- 🌞 **工具标签页（macOS 专属显示功能）** — 外接屏幕使用 DDC/CI，支持的 Mac 内建面板在可用时使用 IOKit 背光控制亮度。外接 DDC 功能因硬件而异。在较新的受支持 Mac 上，如果无法直接访问 IOKit，则通过 macOS 控制中心辅助功能自动化操作；两种内建显示路径均仅适用于 macOS。
- 📊 **系统状态** — 显示系统 CPU、内存和系统磁盘；在 macOS（Apple Silicon）上还会显示无需提权即可读取的 SoC 芯片温度。Windows 或无法读取可信传感器时显示为不可用
- 📊 **用量（位于工具标签页）** — 直接读取 Claude Code、Codex、SayKnow CLI 的本地会话日志。显示 5 小时计费区块(剩余时间与消耗速率),以及 CLI 记录的 5 小时 / 每周真实使用率和重置时间。已重置的窗口加删除线标记,不当作当前值展示。无需联网,也不需要额外登录
- 📋 **剪贴板历史标签页** — 后台采集复制内容。正文与备注可一并搜索,支持置顶、发送到翻译、两级清理。空值、OTP 形式字符串与 PEM 密钥块不予保存
- 💬 **聊天标签** — 同窗口轻量 Q&A,多对话侧栏,每条消息支持 重新生成 / 编辑 / 复制 / 停止
- 🤖 **多提供商** — OpenRouter / OCP / 自定义 OpenAI 兼容端点
- 📦 **OCP 一键安装** — 应用内自动执行 `git clone → npm install → setup.mjs`,实时日志
- 📐 **紧凑横向模式** — 720×240 左右分屏,适合常驻
- 🪟 **窗口大小切换** — 标题栏一键切换紧凑 ↔ 标准
- ⚡ **自动翻译** — 停止输入 1.5 秒后自动调用
- ⌨️ **手动模式** — 仅在 macOS 按下 `⌘⏎`、Windows 按下 `Ctrl+Enter` 或翻译按钮时调用(节省费用)
- 🪄 **修订翻译** — 礼貌/随意/更短/商务/直译预设 + 自定义提示词
- 🌐 **OpenRouter BYOK** — 可搜索 360+ 种模型
- 🔁 **备用模型** — 主模型失败时 OpenRouter 自动切换
- ⏹ **停止** — 立即取消进行中的调用
- 📋 **剪贴板自动获取** — 在 macOS 按 `⌘⇧T`、Windows 按 `Ctrl+Shift+T` 打开时,其他应用复制的文本自动填入
- 🕘 **翻译历史** — 可搜索,固定项永久保留
- 📌 **窗口固定** — 关闭自动隐藏
- 📚 **术语库**(Glossary)— 公司名、专有名词一致翻译
- ✏️ **自定义系统提示词**
- 💰 **用量追踪** — 每日/每月 token 与费用
- 🌓 **深色/浅色/系统** — 自动跟随系统主题
- 🌍 **8 种界面语言** — 自动检测系统语言
- 🔄 **36 种翻译语言**
- 🔒 **系统凭据存储** — API 密钥保存于 macOS Keychain / Windows Credential Manager，不以明文保存

## 系统要求

- **macOS 11.0 (Big Sur) 或更高版本，Apple Silicon (aarch64)**
- **Windows 10 或 11，x64**
- OpenRouter API 密钥 — 在 [openrouter.ai/keys](https://openrouter.ai/keys) 获取

## 安装

### v0.2.13 官方安装程序（推荐）

仅从[官方 GitHub Release](https://github.com/jaybeyond/sayknow-kit/releases)下载，并使用 `SHA256SUMS.txt` 校验。

**macOS（Apple Silicon）：** 下载 `SayKnow-Kit_0.2.13_aarch64.dmg`，打开后将应用拖入 `/Applications`。v0.2.13 应用仅使用**临时签名（ad-hoc）**，没有 Developer ID 或 notarization，因此出现 Gatekeeper 警告是正常的。macOS 13 及以上版本不再接受右键绕过：先运行一次,然后前往**系统设置 → 隐私与安全性 → 仍要打开**。也可以自行清除隔离属性:

```bash
xattr -dr com.apple.quarantine "/Applications/SayKnow Kit.app"
```

如果已经允许辅助功能却仍反复索取权限,说明已保存的条目与更新后的临时签名不再匹配。重置后重启应用:

```bash
tccutil reset Accessibility com.sayknow.app
```

请务必先移动到 `/Applications`。若直接从 DMG 或下载文件夹启动,macOS 会在随机的只读位置运行应用,内置屏幕亮度所需的辅助功能权限将无法保存。

**Windows（x64）：** 下载 `SayKnow-Kit_0.2.13_x64-setup.exe`（NSIS）或 `SayKnow-Kit_0.2.13_x64_en-US.msi` 并运行。安装程序**未签名**，出现 SmartScreen 警告是正常的。确认官方 Release 与 `SHA256SUMS.txt` 后，再选择“更多信息 → 仍要运行”。

### 卸载

- **macOS：**退出应用，然后删除 `/Applications/SayKnow Kit.app`
- **Windows：**设置 → 应用 → 已安装的应用 → SayKnow Kit → 卸载
### 从源码构建

```bash
git clone https://github.com/jaybeyond/sayknow-kit.git
cd sayknow-kit
pnpm install
pnpm tauri build
```

## 使用方法

1. macOS 中图标出现在菜单栏（时钟旁），Windows 中出现在系统托盘（不会显示在 Dock/任务栏）。
2. 点击图标 → 输入 OpenRouter API 密钥 → **连接并开始**
3. 密钥自动保存到系统凭据存储（macOS Keychain / Windows Credential Manager）
4. 点击图标或按 `⌘⇧T`（macOS）/ `Ctrl+Shift+T`（Windows）打开 → 输入文本 → 1.5 秒后自动翻译

### 快捷键

| 快捷键 | 操作 |
|---|---|
| `⌘⇧T` (macOS) | 打开/关闭弹窗（全局） |
| `Ctrl+Shift+T` (Windows) | 打开/关闭弹窗（全局） |
| `⌘⏎` (macOS) | 立即翻译（手动模式） |
| `Ctrl+Enter` (Windows) | 立即翻译（手动模式） |

### 设置(独立窗口)

点击 ⚙️ → **设置** 打开侧边栏窗口:
- **常规** — 模式、剪贴板、固定、主题、应用语言
- **连接** — 主模型/备用模型、登出
- **术语库** — 术语对照("后端团队" → "Backend Team")
- **系统提示词** — 编辑翻译/修订提示词
- **用量** — 每日/每月 token 和费用
- **关于**

## 安全

OpenRouter API 密钥涉及计费，SayKnow Kit 不以明文存储，使用操作系统的凭据存储：

- macOS **Keychain** (`com.sayknow.app`)
- Windows **Credential Manager**

安装程序仅从官方 GitHub Release 获取，并使用 `SHA256SUMS.txt` 校验。v0.2.13 的 macOS 应用仅使用临时签名，没有 Developer ID 或 notarization；Windows 安装程序未使用 Authenticode 签名。Gatekeeper 和 SmartScreen 警告是正常现象。

## 许可

[MIT](LICENSE) — 详情见 [English README](README.md)
