<div align="center">

<img src="public/sayo-logo.png" alt="SayKnow Kit 문어 마스코트 Sayo" width="180" />

# SayKnow Kit

**macOS 메뉴바와 Windows 트레이를 위한 크로스 플랫폼 AI 키트 — 번역 · 채팅 · 클립보드를 팝오버 하나에 담았습니다.**

`say` (말하다) + `know` (알다) — 말하면 바로 이해되는 컨셉.

**한국어** · [English](README.md) · [日本語](README.ja.md) · [中文](README.zh.md) · [Español](README.es.md) · [Français](README.fr.md) · [Deutsch](README.de.md) · [Tiếng Việt](README.vi.md)

[![macOS](https://img.shields.io/badge/macOS-11%2B-black?logo=apple)](https://www.apple.com/macos/) [![Windows](https://img.shields.io/badge/Windows-10%2F11-blue?logo=windows)](https://www.microsoft.com/windows)
[![Tauri](https://img.shields.io/badge/Tauri-2-FFC131?logo=tauri&logoColor=black)](https://tauri.app)
[![License: MIT](https://img.shields.io/badge/License-MIT-green.svg)](LICENSE)

</div>

---

## 개요

SayKnow Kit는 **macOS 메뉴바 또는 Windows 시스템 트레이에 상주**하는 AI 키트입니다. 단축키 한 번이면 팝오버가 열리고 번역, 채팅, 클립보드 히스토리를 한곳에서 사용합니다.

**OpenRouter BYOK (Bring Your Own Key)** + **OCP (Open Claude Proxy)** + **Custom 엔드포인트** — 세 프로바이더 중 골라 쓰며, OpenRouter만으로도 GPT-4o · Claude · Gemini · Llama 등 360+ 모델을 키 하나로 호출할 수 있습니다.

## 주요 기능

### 번역
- ⚡ **자동 번역** — 타이핑이 멈추면 1.5초 후 자동 호출
- ⌨️ **수동 번역** — macOS에서는 `⌘⏎`, Windows에서는 `Ctrl+Enter` 또는 번역 버튼을 누를 때만 호출 (비용 절약)
- 🪄 **수정 번역 (Refine)** — 정중히·캐주얼·짧게·비즈니스·직역 프리셋 + 자유 프롬프트
- ⏹ **정지** — 응답 느릴 때 진행 중 호출 즉시 취소
- 🔄 **36개 번역 언어** — 동·서·북·동유럽, 동남아, 남아시아, 중동, 아프리카
- 📚 **용어집 (Glossary)** — 회사명·고유명사 일관 번역
- ✏️ **시스템 프롬프트 커스터마이징** — 번역/수정 프롬프트 직접 편집

### 챗
- 💬 **챗 탭** — 가벼운 Q&A를 같은 창에서. 멀티 대화 사이드바, 자동 제목 생성
- ♻️ **재생성 / ✏️ 편집 / 📋 복사 / ⏹ 정지** — 메시지마다 적용 가능
- 🧠 **모델 공유** — 번역 탭의 기본 모델 그대로 사용

### 클립보드
- 📋 **클립보드 히스토리** — 복사한 내용을 백그라운드로 수집, 본문과 메모를 함께 검색
- 📝 **메모** — 왜 남겼는지 항목마다 적어둘 수 있음
- 📌 **고정** — 고정한 항목은 개수 제한과 삭제를 모두 통과
- ➡️ **번역창으로 보내기** — 항목을 바로 번역 탭으로 전달
- 🧹 **2단계 삭제** — 고정 안 된 것만 지우기 / 전체 삭제
- 🔒 **민감정보 회피** — 빈 값, OTP 형태 문자열, PEM 키 블록은 저장하지 않음

### 사용량
- 📊 **에이전트 사용량** — Claude Code · Codex · SayKnow CLI가 로컬에 남긴 세션 로그를 그대로 읽음
- ⏱ **5시간 블록** — 구독이 실제로 계량하는 과금 창, 남은 시간과 분당 소모율 표시
- 🚦 **실제 한도** — CLI가 기록해 둔 5시간 / 주간 사용률과 리셋 시각
- 🔍 **오래된 값 표시** — 이미 리셋된 창은 취소선 처리, 현재 값처럼 그리지 않음
- 🔌 **네트워크 없음** — 아무것도 전송하지 않고 추가 로그인도 필요 없음
- 🧰 **도구 탭 안에 위치** — 사용량 카드는 도구 탭의 화면 밝기 바로 아래에 있습니다

### 도구
- 🌞 **화면 밝기(macOS 전용)** — 외부 화면은 DDC/CI(HDMI/DP/USB-C), 지원되는 Mac 내장 패널은 IOKit 백라이트로 하드웨어 수준에서 제어합니다. 외부 DDC 기능은 하드웨어에 따라 다릅니다. IOKit 직접 접근을 사용할 수 없는 최신 지원 Mac에서는 macOS 제어 센터 접근성 UI 자동화를 사용하며, 두 내장 경로 모두 macOS 전용입니다.
- 🔌 **화면 전원(macOS 전용)** — 외부 모니터를 DDC 대기로 끄고 켭니다.
- 🎚️ **한 번에 전부** — 슬라이더 하나로 모든 화면을 함께, 또는 각각 조절
- 📊 **시스템 상태** — 시스템 CPU·메모리·시스템 디스크와 함께, macOS(Apple Silicon)에서는 권한 상승 없이 읽는 SoC 다이 온도를 표시합니다. Windows이거나 신뢰할 수 있는 센서를 읽지 못하면 사용할 수 없음으로 표시합니다.
- ⌨️ **키보드 친화적** — 슬라이더는 놓을 때 한 번만 커맨드를 보냅니다

### 프로바이더
- 🌐 **OpenRouter BYOK** — 360+ 모델 검색 콤보박스, 폴백 모델 자동 재시도
- 🤖 **OCP (Open Claude Proxy)** — 로컬 Claude CLI를 OpenAI 호환 API로 노출. **한 번 클릭으로 자동 설치** (git clone → npm install → setup.mjs), 실시간 로그 스트리밍
- 🔧 **Custom 엔드포인트** — 임의의 OpenAI 호환 베이스 URL 등록 (자체 호스팅, vLLM, LM Studio 등)

### 창 / UX
- 📐 **컴팩트 가로 모드** — 720×240 좌우 분할 레이아웃. 늘 띄워놓고 사용하기에 적합
- 🪟 **창 크기 토글** — 헤더에서 컴팩트 ↔ 노멀(480×580) 한 번에 전환
- 📌 **윈도우 핀** — 자동 숨김 끄기 (긴 글 다듬을 때)
- 📋 **클립보드 자동 가져오기** — macOS에서는 `⌘⇧T`, Windows에서는 `Ctrl+Shift+T`로 열 때 다른 앱에서 복사한 텍스트 자동 채움
- 🕘 **번역 기록** — 검색 가능, 핀으로 영구 보존
- 💰 **사용량 추적** — 일/월 토큰 + 비용 집계
- 🌓 **다크/라이트/시스템** 자동 추종
- 🌍 **8개 UI 언어** — 시스템 언어 자동 감지 (한·영·일·중·스페인·프랑스·독일·베트남)
- 🔒 **시스템 자격 증명 저장소** — macOS Keychain / Windows Credential Manager에 저장하며 평문으로 저장하지 않음

## 시스템 요구사항

- **macOS 11.0 (Big Sur) 이상, Apple Silicon (aarch64)**
- **Windows 10 또는 11, x64**
- OpenRouter API 키 — [openrouter.ai/keys](https://openrouter.ai/keys) 에서 발급

## 설치

### v0.2.12 공식 설치 프로그램 (권장)

[공식 GitHub Release](https://github.com/jaybeyond/sayknow-kit/releases)에서만 다운로드하고 `SHA256SUMS.txt`로 확인하세요.

**macOS (Apple Silicon):** `SayKnow-Kit_0.2.12_aarch64.dmg`를 열어 앱을 `/Applications`로 드래그합니다. v0.2.12 앱은 **임시(ad-hoc) 서명만** 적용되며 Developer ID와 공증(notarization)이 없으므로 Gatekeeper 경고가 예상됩니다. macOS 13 이상에서는 우클릭 우회가 통하지 않습니다. 앱을 한 번 실행한 뒤 **시스템 설정 → 개인정보 보호와 보안 → 그래도 열기**를 누르세요. 또는 격리 속성을 직접 제거해도 됩니다:

```bash
xattr -dr com.apple.quarantine "/Applications/SayKnow Kit.app"
```

손쉬운 사용 권한을 허용했는데도 계속 요청한다면, 저장된 항목이 업데이트된 임시 서명과 더 이상 일치하지 않는 경우입니다. 항목을 초기화하고 앱을 다시 시작하세요:

```bash
tccutil reset Accessibility com.sayknow.app
```

반드시 먼저 `/Applications`로 옮기세요. DMG나 다운로드 폴더에서 바로 실행하면 macOS가 임의의 읽기 전용 위치에서 앱을 실행해, 내장 화면 밝기에 필요한 손쉬운 사용 권한이 저장되지 않습니다.

**Windows (x64):** `SayKnow-Kit_0.2.12_x64-setup.exe`(NSIS) 또는 `SayKnow-Kit_0.2.12_x64_en-US.msi`를 실행합니다. 설치 프로그램은 **서명되지 않았으며** SmartScreen 경고가 예상됩니다. 공식 Release와 `SHA256SUMS.txt` 확인 후에만 “추가 정보 → 실행”을 선택하세요.

### 제거

- **macOS:** SayKnow Kit를 종료한 뒤 `/Applications/SayKnow Kit.app`을 삭제
- **Windows:** 설정 → 앱 → 설치된 앱 → SayKnow Kit → 제거
### 소스에서 빌드

```bash
git clone https://github.com/jaybeyond/sayknow-kit.git
cd sayknow-kit
pnpm install
pnpm tauri build
# → src-tauri/target/release/bundle/dmg/SayKnow Kit_x.x.x_aarch64.dmg
```

## 사용법

### 첫 실행

1. macOS에서는 메뉴바(시계·와이파이 옆), Windows에서는 시스템 트레이에 아이콘이 나타납니다. **Dock/작업 표시줄에는 표시되지 않습니다**.
2. 트레이 아이콘 클릭 → 팝업 → OpenRouter API 키 입력 → "연결하고 시작"
3. 키는 시스템 자격 증명 저장소(macOS Keychain 또는 Windows Credential Manager)에 자동 저장됩니다.

### 일반 사용 (자동 번역)

1. 트레이 클릭 또는 macOS `⌘⇧T` / Windows `Ctrl+Shift+T` → 팝업 열림
2. 상단에서 원본/대상 언어 선택 (또는 자동 감지)
3. 입력창에 텍스트 입력 → 1.5초 후 자동 번역
4. 결과 복사 (📋 아이콘)

### 수정 번역 (Refine)

번역 결과 위에서 톤 다듬기:
- **정중히 / 캐주얼 / 짧게 / 비즈니스 / 직역** 프리셋
- ✨ **직접 지시** — 자유 프롬프트 (예: "좀 더 다정한 어조로")

### 단축키

| 단축키 | 동작 |
|---|---|
| `⌘⇧T` (macOS) | 팝업 열기/닫기 (글로벌) |
| `Ctrl+Shift+T` (Windows) | 팝업 열기/닫기 (글로벌) |
| `⌘⏎` (macOS) | 수동 모드에서 즉시 번역 |
| `Ctrl+Enter` (Windows) | 수동 모드에서 즉시 번역 |

### 설정 (별도 창)

메뉴 popover의 ⚙️ → **"설정"** 버튼 → 별도 윈도우 (사이드바 + 페이지):
- **일반** — 자동/수동 모드, 클립보드 자동, 핀, 테마, 앱 언어
- **연결** — 기본 모델, 폴백 모델, 로그아웃
- **용어집** — 용어 페어 등록 ("백엔드팀" → "Backend Team")
- **시스템 프롬프트** — 번역/수정 프롬프트 직접 편집 (변수: `{from}`, `{to}`, `{glossary}`)
- **사용량** — 일/월 토큰·비용
- **정보** — 버전, GitHub, OpenRouter 링크

### 클립보드 자동 가져오기

설정 → 일반 → macOS에서는 `⌘⇧T`, Windows에서는 `Ctrl+Shift+T`로 열 때 클립보드 자동 가져오기를 켠 뒤:
1. 다른 앱에서 텍스트 선택 → macOS에서는 `⌘C`, Windows에서는 `Ctrl+C`
2. macOS에서는 `⌘⇧T`, Windows에서는 `Ctrl+Shift+T` → SayKnow Kit 열림 → 입력창에 자동 채움
3. (자동 모드면) 1.5초 후 번역됨

## 보안

OpenRouter API 키는 **청구되는 자격증명**이라 평문으로 저장하지 않습니다. 운영체제의 자격 증명 저장소를 사용합니다.

- macOS **Keychain** (`com.sayknow.app`)
- Windows **Credential Manager**

설치 프로그램은 공식 GitHub Release에서만 받고 `SHA256SUMS.txt`를 확인하세요. v0.2.12 macOS 앱은 Developer ID·공증 없이 임시(ad-hoc) 서명되며, Windows 설치 프로그램은 Authenticode 없이 서명되지 않았습니다. Gatekeeper와 SmartScreen 경고가 예상됩니다.

## 개발 환경

```bash
# Node 20+ + pnpm 9+
node -v && pnpm -v

# Rust (Tauri 빌드용)
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y
. "$HOME/.cargo/env"

# Xcode Command Line Tools
xcode-select --install

# 아이콘 변환용 (선택)
brew install librsvg
```

```bash
pnpm install
pnpm tauri dev      # 개발 서버 + 윈도우
pnpm tauri build    # 프로덕션 .app + .dmg
```

## 기술 스택

| 영역 | 기술 |
|---|---|
| 프레임워크 | Tauri 2 + Vite 8 + React 19 |
| 언어 | TypeScript (strict) + Rust |
| 스타일 | Tailwind v4 + shadcn/ui |
| 아이콘 | Lucide React |
| 저장 | localStorage + macOS Keychain (`keyring` crate) |
| Tauri 플러그인 | `positioner`, `global-shortcut`, `clipboard-manager`, `opener`, `http`, `log` |
| 외부 API | OpenRouter / OCP (로컬) / 임의 OpenAI 호환 엔드포인트 |

## 최근 업데이트

### 새 기능
- **챗 탭** — 멀티 대화 + 재생성 / 편집 / 복사 / 정지
- **멀티 프로바이더** — OpenRouter, OCP, Custom 엔드포인트 한 화면에서 전환
- **OCP 원터치 설치** — 앱 안에서 `git clone → npm install → node setup.mjs` 자동 실행, 진행 로그 실시간 스트리밍
- **컴팩트 가로 모드** — 720×240 좌/우 분할 레이아웃, 상시 띄워두기 좋음
- **창 크기 토글** — 헤더 버튼으로 컴팩트 ↔ 노멀 즉시 전환

### 개선 / 버그 픽스
- macOS "복원하시겠습니까?" 대화상자 차단 (`NSQuitAlwaysKeepsWindows=false`, `LSUIElement=true`)
- positioner 트레이 좌표 미캐시 상태에서 `move_window` 호출 시 발생하던 macOS 크래시(`SIGABRT`) 차단
- 기록을 클릭해 복원할 때 같은 텍스트가 재번역되던 문제 수정
- 핀 상태에서 본문이 사라지던 애니메이션 회귀 수정
- 중국어 라벨을 네이티브 표기로(`简体中文 / 繁體中文`)
- 프로바이더에 따라 API 키 라벨이 동적으로 바뀌도록
- 8개 로케일의 누락된 43개 키 보충 (총 ~250개 문자열)
- localhost로 향하는 `fetch`의 CORS 우회를 위해 `tauri-plugin-http` 적용
- OCP / Claude CLI를 GUI 환경에서도 찾을 수 있도록 `/bin/sh -lc 'command -v ...'` 기반 PATH 해석

## 로드맵

- [ ] 시스템 전역 텍스트 선택 → 단축키 → 즉시 번역
- [ ] 즐겨찾는 표현 저장
- [ ] 시스템 시작 시 자동 실행
- [x] Windows 지원 (출시된 x64 지원)
- [ ] Apple 코드사이닝 + 노터라이즈
- [ ] OCR (스크린샷 영역 번역)
- [ ] 로컬 LLM 직접 통합 (Ollama / LM Studio)
- [ ] 모델 추천 / 비교 모드

## 기여

PR 환영합니다. 큰 변경은 이슈 먼저 열어 논의해 주세요.

## 라이선스

[MIT](LICENSE)
