<div align="center">

<img src="public/sayo-logo.png" alt="SayKnow Kit 章鱼吉祥物 Sayo" width="180" />

# SayKnow Kit

**菜单栏 AI 工具包 — 翻译、聊天、剪贴板集于一个弹窗。**

`say`(说) + `know`(懂) — 一说就懂。

[한국어](README.ko.md) · [English](README.md) · [日本語](README.ja.md) · **中文** · [Español](README.es.md) · [Français](README.fr.md) · [Deutsch](README.de.md) · [Tiếng Việt](README.vi.md)

[![macOS](https://img.shields.io/badge/macOS-11.0%2B-black?logo=apple)](https://www.apple.com/macos/)
[![Tauri](https://img.shields.io/badge/Tauri-2-FFC131?logo=tauri&logoColor=black)](https://tauri.app)
[![License: MIT](https://img.shields.io/badge/License-MIT-green.svg)](LICENSE)

</div>

---

## 简介

SayKnow Kit 常驻 **macOS 菜单栏**。一个快捷键打开弹窗,里面是每天都会用到的三件工具 —— **翻译**(停止输入后自动执行)、**聊天**、**剪贴板历史**,无需在翻译网站和当前应用间来回切换复制粘贴。

**三种提供商**(OpenRouter / OCP / 自定义端点)可在同一界面切换;仅 OpenRouter 即可一把密钥调用 GPT-4o、Claude、Gemini、Llama 等 360+ 模型。

## 主要功能

- 🌞 **工具标签页** — 在硬件层面控制所有已连接屏幕的亮度。外接走 DDC/CI,内建在支持的 Mac 上走 IOKit 背光;还支持外接显示器的开关(DDC 待机)
- 📊 **用量标签页** — 直接读取 Claude Code、Codex、SayKnow CLI 的本地会话日志。显示 5 小时计费区块(剩余时间与消耗速率),以及 CLI 记录的 5 小时 / 每周真实使用率和重置时间。已重置的窗口加删除线标记,不当作当前值展示。无需联网,也不需要额外登录
- 📋 **剪贴板历史标签页** — 后台采集复制内容。正文与备注可一并搜索,支持置顶、发送到翻译、两级清理。空值、OTP 形式字符串与 PEM 密钥块不予保存
- 💬 **聊天标签** — 同窗口轻量 Q&A,多对话侧栏,每条消息支持 重新生成 / 编辑 / 复制 / 停止
- 🤖 **多提供商** — OpenRouter / OCP / 自定义 OpenAI 兼容端点
- 📦 **OCP 一键安装** — 应用内自动执行 `git clone → npm install → setup.mjs`,实时日志
- 📐 **紧凑横向模式** — 720×240 左右分屏,适合常驻
- 🪟 **窗口大小切换** — 标题栏一键切换紧凑 ↔ 标准
- ⚡ **自动翻译** — 停止输入 1.5 秒后自动调用
- ⌨️ **手动模式** — 仅在按下 `⌘⏎` 或翻译按钮时调用(节省费用)
- 🪄 **修订翻译** — 礼貌/随意/更短/商务/直译预设 + 自定义提示词
- 🌐 **OpenRouter BYOK** — 可搜索 360+ 种模型
- 🔁 **备用模型** — 主模型失败时 OpenRouter 自动切换
- ⏹ **停止** — 立即取消进行中的调用
- 📋 **剪贴板自动获取** — `⌘⇧T` 打开时,其他应用复制的文本自动填入
- 🕘 **翻译历史** — 可搜索,固定项永久保留
- 📌 **窗口固定** — 关闭自动隐藏
- 📚 **术语库**(Glossary)— 公司名、专有名词一致翻译
- ✏️ **自定义系统提示词**
- 💰 **用量追踪** — 每日/每月 token 与费用
- 🌓 **深色/浅色/系统** — 自动跟随系统主题
- 🌍 **8 种界面语言** — 自动检测系统语言
- 🔄 **36 种翻译语言**
- 🔒 **macOS Keychain** — API 密钥 AES-256 加密存储

## 系统要求

- macOS 11.0(Big Sur)或更高版本
- Apple Silicon(aarch64)
- OpenRouter API 密钥 — 在 [openrouter.ai/keys](https://openrouter.ai/keys) 获取

## 安装

### 方式 1 — 预编译 DMG(推荐)

1. 从 [Releases](https://github.com/jaybeyond/sayknow-kit/releases) 下载 `SayKnow Kit_x.x.x_aarch64.dmg`
2. 打开 DMG 将 SayKnow Kit.app 拖入 `/Applications`
3. 由于未代码签名,首次启动会被 Gatekeeper 拦截:
   ```bash
   xattr -dr com.apple.quarantine "/Applications/SayKnow Kit.app"
   ```

### 方式 2 — 从源码构建

```bash
git clone https://github.com/jaybeyond/sayknow-kit.git
cd sayknow-kit
pnpm install
pnpm tauri build
```

## 使用方法

1. 菜单栏右侧(时钟旁边)出现小图标(Dock 中不显示)
2. 点击图标 → 输入 OpenRouter API 密钥 → **连接并开始**
3. 密钥自动保存到 macOS Keychain
4. 点击图标或按 `⌘⇧T` 打开 → 输入文本 → 1.5 秒后自动翻译

### 快捷键

| 快捷键 | 操作 |
|---|---|
| `⌘⇧T` | 打开/关闭弹窗(全局) |
| `⌘⏎` | 立即翻译(手动模式) |

### 设置(独立窗口)

点击 ⚙️ → **设置** 打开侧边栏窗口:
- **常规** — 模式、剪贴板、固定、主题、应用语言
- **连接** — 主模型/备用模型、登出
- **术语库** — 术语对照("白엔드팀" → "Backend Team")
- **系统提示词** — 编辑翻译/修订提示词
- **用量** — 每日/每月 token 和费用
- **关于**

## 安全

OpenRouter API 密钥涉及计费,SayKnow Kit 不以明文存储:

- macOS **Keychain**(`com.sayknow.app`)
- 由 macOS 登录密码派生的密钥进行 AES-256 加密
- 其他应用读取时会触发系统的允许/拒绝提示

## 许可

[MIT](LICENSE) — 详情见 [English README](README.md)
