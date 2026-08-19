<div align="center">

# SayKnow Kit

**번역 · 채팅 · 클립보드를 팝오버 하나에 담은 macOS 메뉴바 AI 키트**

`say` (말하다) + `know` (알다) — 말하면 바로 이해되는 컨셉.

**한국어** · [English](README.md) · [日本語](README.ja.md) · [中文](README.zh.md) · [Español](README.es.md) · [Français](README.fr.md) · [Deutsch](README.de.md) · [Tiếng Việt](README.vi.md)

[![macOS](https://img.shields.io/badge/macOS-11.0%2B-black?logo=apple)](https://www.apple.com/macos/)
[![Tauri](https://img.shields.io/badge/Tauri-2-FFC131?logo=tauri&logoColor=black)](https://tauri.app)
[![License: MIT](https://img.shields.io/badge/License-MIT-green.svg)](LICENSE)

</div>

---

## 개요

SayKnow Kit는 **macOS 메뉴바에 상주**하는 AI 키트입니다. 단축키 한 번이면 팝오버가 열리고, 하루 종일 손이 가는 도구 세 가지가 한자리에 있습니다 — **번역**(타이핑을 멈추면 알아서 실행), **채팅**, **클립보드 히스토리**. 번역기 사이트를 새 탭으로 띄우고 복사·붙여넣기를 반복할 일이 없어집니다.

**OpenRouter BYOK (Bring Your Own Key)** + **OCP (Open Claude Proxy)** + **Custom 엔드포인트** — 세 프로바이더 중 골라 쓰며, OpenRouter만으로도 GPT-4o · Claude · Gemini · Llama 등 360+ 모델을 키 하나로 호출할 수 있습니다.

## 주요 기능

### 번역
- ⚡ **자동 번역** — 타이핑이 멈추면 1.5초 후 자동 호출
- ⌨️ **수동 번역** — `⌘⏎` 또는 번역 버튼만 누를 때 호출 (비용 절약)
- 🪄 **수정 번역 (Refine)** — 정중히·캐주얼·짧게·비즈니스·직역 프리셋 + 자유 프롬프트
- ⏹ **정지** — 응답 느릴 때 진행 중 호출 즉시 취소
- 🔄 **36개 번역 언어** — 동·서·북·동유럽, 동남아, 남아시아, 중동, 아프리카
- 📚 **용어집 (Glossary)** — 회사명·고유명사 일관 번역
- ✏️ **시스템 프롬프트 커스터마이징** — 번역/수정 프롬프트 직접 편집

### 챗
- 💬 **챗 탭** — 가벼운 Q&A를 같은 창에서. 멀티 대화 사이드바, 자동 제목 생성
- ♻️ **재생성 / ✏️ 편집 / 📋 복사 / ⏹ 정지** — 메시지마다 적용 가능
- 🧠 **모델 공유** — 번역 탭의 기본 모델 그대로 사용

### 프로바이더
- 🌐 **OpenRouter BYOK** — 360+ 모델 검색 콤보박스, 폴백 모델 자동 재시도
- 🤖 **OCP (Open Claude Proxy)** — 로컬 Claude CLI를 OpenAI 호환 API로 노출. **한 번 클릭으로 자동 설치** (git clone → npm install → setup.mjs), 실시간 로그 스트리밍
- 🔧 **Custom 엔드포인트** — 임의의 OpenAI 호환 베이스 URL 등록 (자체 호스팅, vLLM, LM Studio 등)

### 창 / UX
- 📐 **컴팩트 가로 모드** — 720×240 좌우 분할 레이아웃. 늘 띄워놓고 사용하기에 적합
- 🪟 **창 크기 토글** — 헤더에서 컴팩트 ↔ 노멀(480×580) 한 번에 전환
- 📌 **윈도우 핀** — 자동 숨김 끄기 (긴 글 다듬을 때)
- 📋 **클립보드 자동 가져오기** — `⌘⇧T`로 열 때 다른 앱에서 복사한 텍스트 자동 채움
- 🕘 **번역 기록** — 검색 가능, 핀으로 영구 보존
- 💰 **사용량 추적** — 일/월 토큰 + 비용 집계
- 🌓 **다크/라이트/시스템** 자동 추종
- 🌍 **8개 UI 언어** — 시스템 언어 자동 감지 (한·영·일·중·스페인·프랑스·독일·베트남)
- 🔒 **macOS Keychain** — API 키 평문 저장 금지, AES-256 암호화

## 시스템 요구사항

- macOS 11.0 (Big Sur) 이상
- Apple Silicon (aarch64) — Intel은 직접 빌드 시 가능
- OpenRouter API 키 — [openrouter.ai/keys](https://openrouter.ai/keys) 에서 발급

## 설치

### 방법 1 — 빌드 결과물 사용 (권장)

1. [Releases](https://github.com/jaybeyond/sayknow-kit/releases) 페이지에서 `SayKnow Kit_x.x.x_aarch64.dmg` 다운로드
2. DMG 열고 SayKnow Kit.app을 `/Applications`로 드래그
3. 코드사이닝 안 된 빌드라 처음 실행 시 Gatekeeper가 막을 수 있음:
   ```bash
   xattr -dr com.apple.quarantine "/Applications/SayKnow Kit.app"
   ```
   또는 Finder에서 SayKnow Kit.app **우클릭 → 열기** → "열기" 한 번 더.

### 방법 2 — 소스 빌드

```bash
git clone https://github.com/jaybeyond/sayknow-kit.git
cd sayknow-kit
pnpm install
pnpm tauri build
# → src-tauri/target/release/bundle/dmg/SayKnow Kit_x.x.x_aarch64.dmg
```

## 사용법

### 첫 실행

1. 메뉴 바 우측(시계·와이파이 옆)에 작은 아이콘 등장. **Dock에는 안 보임**.
2. 트레이 아이콘 클릭 → 팝업 → OpenRouter API 키 입력 → "연결하고 시작"
3. 키는 **macOS Keychain**에 자동 저장 (다음부터 묻지 않음)

### 일반 사용 (자동 번역)

1. 트레이 클릭 또는 `⌘⇧T` → 팝업 열림
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
| `⌘⇧T` | 팝업 열기/닫기 (글로벌) |
| `⌘⏎` | 수동 모드에서 즉시 번역 |

### 설정 (별도 창)

메뉴 popover의 ⚙️ → **"설정"** 버튼 → 별도 윈도우 (사이드바 + 페이지):
- **일반** — 자동/수동 모드, 클립보드 자동, 핀, 테마, 앱 언어
- **연결** — 기본 모델, 폴백 모델, 로그아웃
- **용어집** — 용어 페어 등록 ("백엔드팀" → "Backend Team")
- **시스템 프롬프트** — 번역/수정 프롬프트 직접 편집 (변수: `{from}`, `{to}`, `{glossary}`)
- **사용량** — 일/월 토큰·비용
- **정보** — 버전, GitHub, OpenRouter 링크

### 클립보드 자동 가져오기

설정 → 일반 → "⌘⇧T로 열 때 클립보드 자동 가져오기" 켠 뒤:
1. 다른 앱에서 텍스트 선택 → ⌘C
2. ⌘⇧T → SayKnow Kit 열림 → 입력창에 자동 채움
3. (자동 모드면) 1.5초 후 번역됨

## 보안

OpenRouter API 키는 **돈이 나가는 자격증명**이라 평문으로 저장하지 않습니다.

- macOS **Keychain** (`com.sayknow.app` / `openrouter_api_key`)
- macOS 로그인 비밀번호 파생 키로 AES-256 암호화
- 다른 앱이 읽으려 하면 OS가 사용자에게 "허용/거부" 프롬프트
- 디스크 도난, 백업 유출, 다른 앱 무단 접근으로부터 보호

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
- [x] Windows 지원 (CI 빌드 추가, QA 진행 중)
- [ ] Apple 코드사이닝 + 노터라이즈
- [ ] OCR (스크린샷 영역 번역)
- [ ] 로컬 LLM 직접 통합 (Ollama / LM Studio)
- [ ] 모델 추천 / 비교 모드

## 기여

PR 환영합니다. 큰 변경은 이슈 먼저 열어 논의해 주세요.

## 라이선스

[MIT](LICENSE)
