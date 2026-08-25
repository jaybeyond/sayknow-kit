<div align="center">

# SayKnow Kit

**メニューバーのAIキット — 翻訳・チャット・クリップボードを1つのポップオーバーに。**

`say`(話す) + `know`(知る) — 言えばすぐに伝わる。

[한국어](README.ko.md) · [English](README.md) · **日本語** · [中文](README.zh.md) · [Español](README.es.md) · [Français](README.fr.md) · [Deutsch](README.de.md) · [Tiếng Việt](README.vi.md)

[![macOS](https://img.shields.io/badge/macOS-11.0%2B-black?logo=apple)](https://www.apple.com/macos/)
[![Tauri](https://img.shields.io/badge/Tauri-2-FFC131?logo=tauri&logoColor=black)](https://tauri.app)
[![License: MIT](https://img.shields.io/badge/License-MIT-green.svg)](LICENSE)

</div>

---

## 概要

SayKnow Kit は **macOS のメニューバー**に常駐する AI キットです。ショートカット一つでポップオーバーが開き、毎日使う 3 つの道具がそこに揃っています — **翻訳**(タイピングが止まると自動実行)、**チャット**、**クリップボード履歴**。翻訳サイトと行き来してコピペを繰り返す手間がなくなります。

**3 つのプロバイダー**(OpenRouter / OCP / Custom エンドポイント)を 1 つの画面から切り替えられます。OpenRouter 単体でも GPT-4o, Claude, Gemini, Llama など 360 種類以上のモデルをキー一つで利用可能。

## 主な機能

- 📊 **エージェント使用量タブ** — Claude Code / Codex / SayKnow CLI がローカルに残すセッションログを直接読み取り。5 時間の課金ブロック(残り時間・消費レート)と、CLI が記録した 5 時間・週間の実際の使用率をリセット時刻付きで表示。リセット済みのウィンドウは取り消し線で明示し、現在値としては描画しない。通信も追加ログインも不要
- 📋 **クリップボード履歴タブ** — コピー内容をバックグラウンド収集。本文とメモを横断検索、ピン留め、翻訳タブへ送信、2 段階の削除。空文字・OTP 形式・PEM 鍵ブロックは保存対象外
- 💬 **チャットタブ** — 同じウィンドウで軽い Q&A、マルチ会話サイドバー、メッセージごとの 再生成 / 編集 / コピー / 停止
- 🤖 **マルチプロバイダー** — OpenRouter / OCP / Custom OpenAI 互換エンドポイント
- 📦 **OCP ワンタップインストール** — アプリが自動で `git clone → npm install → setup.mjs` を実行、進行ログをライブ表示
- 📐 **コンパクト横並びモード** — 720×240 の左右分割レイアウト、常時表示向け
- 🪟 **ウィンドウサイズ切替** — ヘッダーからコンパクト ↔ ノーマル即時切替
- ⚡ **自動翻訳** — タイピング停止 1.5 秒後に翻訳
- ⌨️ **手動モード** — `⌘⏎` または翻訳ボタン押下時のみ呼び出し(コスト節約)
- 🪄 **再翻訳** — 丁寧 / カジュアル / 短く / ビジネス / 直訳 + 自由プロンプト
- 🌐 **OpenRouter BYOK** — 360 種類以上のモデルを検索可能
- 🔁 **フォールバックモデル** — メインモデル失敗時に自動切り替え
- ⏹ **停止** — 進行中の呼び出しを即座に中止
- 📋 **クリップボード自動取得** — `⌘⇧T` で開く時、他アプリでコピーしたテキストを自動入力
- 🕘 **翻訳履歴** — 検索可能、ピン留めで永久保存
- 📌 **ウィンドウのピン留め** — 自動非表示を無効化
- 📚 **用語集**(Glossary)— 会社名・固有名詞の一貫翻訳
- ✏️ **システムプロンプトのカスタマイズ**
- 💰 **使用量トラッキング** — 日次/月次 トークンとコスト
- 🌓 **ダーク/ライト/システム** — OS テーマ自動追従
- 🌍 **8 つの UI 言語** — システム言語自動検出
- 🔄 **36 の翻訳言語**
- 🔒 **macOS Keychain** — API キー AES-256 暗号化保存

## システム要件

- macOS 11.0(Big Sur)以降
- Apple Silicon(aarch64)
- OpenRouter API キー — [openrouter.ai/keys](https://openrouter.ai/keys) で発行

## インストール

### 方法 1 — DMG(推奨)

1. [Releases](https://github.com/jaybeyond/sayknow-kit/releases) から `SayKnow Kit_x.x.x_aarch64.dmg` をダウンロード
2. DMG を開いて SayKnow Kit.app を `/Applications` にドラッグ
3. コードサインなしのため、初回起動時に Gatekeeper がブロックしたら:
   ```bash
   xattr -dr com.apple.quarantine "/Applications/SayKnow Kit.app"
   ```

### 方法 2 — ソースからビルド

```bash
git clone https://github.com/jaybeyond/sayknow-kit.git
cd sayknow-kit
pnpm install
pnpm tauri build
```

## 使い方

1. メニューバー右側(時計の隣)にアイコンが表示される(Dock には表示されない)
2. アイコンをクリック → OpenRouter API キーを入力 → **接続して開始**
3. キーは macOS Keychain に自動保存
4. アイコンクリックまたは `⌘⇧T` で起動 → テキスト入力 → 1.5 秒後に翻訳

### ショートカット

| キー | 動作 |
|---|---|
| `⌘⇧T` | ポップアップの開閉(グローバル) |
| `⌘⏎` | 即座に翻訳(手動モード) |

### 設定(独立ウィンドウ)

⚙️ → **設定** ボタンで別ウィンドウが開く:
- **一般** — モード、クリップボード、ピン、テーマ、アプリ言語
- **接続** — メイン/フォールバックモデル、ログアウト
- **用語集** — 用語ペア("백엔드팀" → "Backend Team")
- **システムプロンプト** — 翻訳/再翻訳プロンプト編集
- **使用量** — 日次/月次 トークン・コスト
- **情報**

## セキュリティ

OpenRouter API キーは課金が発生する認証情報のため、平文では保存しません。

- macOS **Keychain**(`com.sayknow.app`)
- macOS ログインパスワードから派生した鍵で AES-256 暗号化
- 他のアプリが読み取ろうとすると OS が許可/拒否プロンプトを表示

## ライセンス

[MIT](LICENSE) — 詳細は [English README](README.md) を参照
