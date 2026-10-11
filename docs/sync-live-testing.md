# 동기화 실계정 검증 (G-EXT, S1–S9) 운영자 체크리스트

"구현 완료(모의)" ≠ "수락 완료(실계정)". 이 문서의 단계를 실제 계정으로 수행하고 결과를 기록하기 전에는 어떤 저장소도 베타로 열지 않습니다(Q3).

자동화된 부분은 `src-tauri/src/sync/live_tests.rs`, 수동 부분은 아래 S1–S9입니다. 결과는 `_sync/g10-gext-status.md` 표에 기록합니다.

## 1. 필요한 앱 등록

### Google (G-EXT-1)
1. GCP 프로젝트를 만들고 **Google Drive API**를 켭니다.
2. OAuth 동의 화면을 구성합니다. 범위: `openid`, `https://www.googleapis.com/auth/drive.appdata`. 테스트 단계면 테스트 사용자에 검증 계정을 넣습니다.
3. 사용자 인증 정보 → OAuth 클라이언트 ID → 애플리케이션 유형 **데스크톱 앱**. 앱은 PKCE(S256)와 `http://127.0.0.1:<임의 포트>/callback` 루프백 리디렉션을 씁니다.
4. **무비밀 PKCE 확인:** `client_secret` 없이 토큰 교환이 되는지 확인합니다. 안 되면 Q1 결정대로 비밀을 빌드에 포함합니다(비기밀 설치형 값).
5. 빌드 환경 변수: `SAYKNOW_SYNC_GDRIVE_CLIENT_ID`, 필요 시 `SAYKNOW_SYNC_GDRIVE_CLIENT_SECRET`(컴파일 시 `option_env!`로 읽음, 없으면 앱이 `not_configured`).

### Microsoft (G-EXT-2)
1. Entra ID → 앱 등록. 지원 계정 유형: **조직 디렉터리 + 개인 Microsoft 계정**(앱은 `common` 엔드포인트 사용).
2. 플랫폼: **모바일 및 데스크톱(public client)**, 리디렉션 URI `http://127.0.0.1/callback`(Entra는 localhost 리디렉션의 포트를 무시하는지 이 단계에서 확인). "공용 클라이언트 흐름 허용" 켜기. 클라이언트 비밀 없음.
3. 위임 권한: `openid`, `offline_access`, `Files.ReadWrite.AppFolder`.
4. 게시자 확인(조직 계정 동의에 필요할 수 있음)이 가능한지 확인합니다. 불가하면 에스컬레이션.
5. 빌드 환경 변수: `SAYKNOW_SYNC_ONEDRIVE_CLIENT_ID`(선택 `SAYKNOW_SYNC_ONEDRIVE_CLIENT_SECRET`).

### WebDAV (G-EXT-3)
HTTPS WebDAV 서버 계정 하나. 기본 경로는 미리 존재해야 합니다. 전용 테스트 디렉터리를 권장합니다.

### Cloudflare R2 (G-EXT-4)
전용 테스트 버킷 하나와, 그 버킷에만 한정된 **Object Read & Write** API 토큰(액세스 키 ID / 비밀 액세스 키).

## 2. 자동 하네스 (live opt-in)

### 환경 변수

| 변수 | 용도 |
|---|---|
| `SAYKNOW_SYNC_LIVE` | 실행할 저장소: `webdav`, `r2`, `gdrive`, `onedrive` (쉼표로 여러 개) |
| `SAYKNOW_LIVE_WEBDAV_URL` / `_USER` / `_PASS` | WebDAV 주소(HTTPS), 사용자, 비밀번호 |
| `SAYKNOW_LIVE_R2_ACCOUNT` / `_BUCKET` / `_KEY` / `_SECRET` | R2 계정 ID, 버킷, 액세스 키 ID, 비밀 액세스 키 |
| `SAYKNOW_LIVE_GDRIVE_REFRESH` + `SAYKNOW_LIVE_GDRIVE_CLIENT_ID` (+ 선택 `SAYKNOW_LIVE_GDRIVE_CLIENT_SECRET`) | 위 Desktop 클라이언트로 받은 refresh token |
| `SAYKNOW_LIVE_ONEDRIVE_REFRESH` + `SAYKNOW_LIVE_ONEDRIVE_CLIENT_ID` (+ 선택 `SAYKNOW_LIVE_ONEDRIVE_CLIENT_SECRET`) | 위 Entra 클라이언트로 받은 refresh token |

- refresh token은 **같은 client id**로, 앱과 같은 범위(`openid` 포함)로 발급받아야 합니다. 하네스는 토큰 갱신 응답의 `id_token`에서 계정 식별자를 얻으므로 `openid`가 없으면 실패합니다.
- 자격 증명은 환경 변수에서만 읽고, OAuth 토큰은 메모리 비밀 저장소에만 둡니다(키체인 미사용). 누락된 변수는 **이름만** 출력하고 실패합니다.
- **전용·빈 테스트 계정/버킷/디렉터리를 쓰세요.** 왕복 검사는 보관함 탐색 결과가 "없음"이어야 새 보관함을 만듭니다. 기존 보관함이 있으면 `account_not_empty`로 실패하고 아무것도 만들지 않습니다.

### 실행

```sh
CARGO_TARGET_DIR=… SAYKNOW_SYNC_LIVE=r2 SAYKNOW_LIVE_R2_ACCOUNT=… … \
  cargo +1.88.0 test --locked --manifest-path src-tauri/Cargo.toml --lib sync::live_tests -- --ignored --nocapture --test-threads=1
```

- 환경 변수 없이 `--ignored`로 돌리면 각 테스트가 `live skipped: SAYKNOW_SYNC_LIVE unset`을 출력하고 즉시 끝납니다. 이 "통과"는 **실계정 통과가 아닙니다.**
- `self_test_*`(무시되지 않음)는 같은 검사를 네트워크 없이 에뮬레이터에 돌려 하네스 자체를 검증합니다. 이것도 실계정 결과가 아닙니다.

### 검사 (저장소당, 순서대로)

각 실행은 새 무작위 프로브 보관함 `live-<32hex>`와 엔진 보관함(UUID)을 쓰며, 끝에 **이 두 접두사만** 삭제합니다.

| check | 내용 | G-EXT 항목 |
|---|---|---|
| `conditional_create_probe` (WebDAV만) | 연결 시 A4 프로브: `If-None-Match: *` 재생성 → 412여야 통과. 실패 시 이후 검사 생략 | G-EXT-3 If-None-Match |
| `container_idempotent` | `ensure_container` 2회: WebDAV MKCOL 405, OneDrive 폴더 409를 성공으로 처리 | G-EXT-3 MKCOL 405 / G-EXT-2 409 |
| `list_visibility_lag` | 생성 후 목록에 보일 때까지 측정(2초 간격, 최대 60회) | G-EXT-1 목록 지연 실측 등 |
| `create_only_conflict` | 같은 키 재생성 → `AlreadyExists`, 원본 바이트 유지, 사본 1개 (WebDAV/R2 412, OneDrive `conflictBehavior=fail` 409, Drive 이름 사전 조회) | G-EXT-2 409, G-EXT-3, G-EXT-4 412 |
| `stale_if_match_replace` | 오래된 `If-Match`로 교체 → 412 `PreconditionFailed`, 내용 불변. Drive는 `Unsupported`여야 함 | G-EXT-4 412 |
| `duplicate_name_kept` (Drive만) | 같은 이름 무검사 생성 → 두 사본 모두 존재(덮어쓰기 없음) | G-EXT-1 |
| `props_query` | Props 질의(Drive: `appProperties` 질의)가 보관함/종류/기기로 정확히 거름, 다른 보관함 0건, Drive는 접두사 목록 `Unsupported` | G-EXT-1 appProperties 질의 실동작 |
| `two_engine_round_trip` | 기기 A가 보관함 생성(RK 재입력)·메모 업로드 → 기기 B가 RK로 참여(J1–J7)·메모 확인·수정 업로드 → A가 수정 적용 → 두 projection 해시 동일 | 전 저장소 |
| `wipe_own_prefix_only` | 엔진 보관함 클라우드 삭제 후 그 보관함 객체 0건, 프로브 보관함(이웃)은 그대로 | AC 27 실계정 |
| `cleanup_run_prefixes` | 이번 실행의 두 접두사만 삭제하고 비었는지 확인 | — |

### 출력 기록

검사마다 stdout에 JSON 한 줄이 나옵니다. 내용·토큰·키·URL은 없고 저장소, 실행 태그, 검사 이름, 성공 여부, 시간(ms), 집계, 고정 오류 분류만 있습니다.

```json
{"check":"list_visibility_lag","live":true,"metrics":{"polls":3,"put_ms":412,"visible_after_ms":4120},"ms":4532,"ok":true,"provider":"gdrive","run":"1a2b3c4d"}
```

`"live":true` 줄만 실계정 결과입니다(에뮬레이터 자체 검사는 `"live":false`). 실패한 검사가 하나라도 있으면 테스트가 실패합니다. 크래시 등으로 정리가 안 됐으면 `run` 태그로 남은 `v1/live-<run>…` 및 해당 실행의 엔진 보관함 접두사를 수동 삭제합니다.

## 3. 수동 E2E (G-EXT-5: macOS 2대 + Windows 1대)

각 시나리오를 저장소마다 수행합니다. 기기 A·B는 macOS, C는 Windows입니다.

| # | 단계 | 통과 기준 |
|---|---|---|
| S1 메모 동시 편집 | A·B가 같은 메모를 오프라인으로 각각 수정 → 둘 다 온라인 | 승자 1 + 사본 1(같은 텍스트면 사본 0), 세 기기 동일 |
| S2 이미지 분기 | A에서 이미지 포함 AI 대화, B에서 같은 대화 재생성 | 형제 2개, 이미지 표시, 세 기기 동일 |
| S3 번역 pin·clear | A가 번역 고정, B가 기록 지우기(clear) | 고정 항목·B가 못 본 새 항목 유지 |
| S4 RK 합류 | C가 RK로 참여, 이어 A에서 RK 재발급 후 이전 RK로 새 기기 참여 시도 | C 정상 합류. 재발급 후 이전 RK 참여 실패, 기존 기기 계속 동기화, 폐기 한계 문구 표시 |
| S5 토큰 폐기·계정 불일치 | 저장소 쪽에서 앱 권한 철회 / 다른 계정으로 다시 로그인 | "다시 로그인" 알림 / 계정 불일치 알림, 업로드 0 |
| S6 할당량·7일 오프라인+압축 | 저장소를 가득 채움; 한 기기를 7일 오프라인 후 복귀 | 용량 알림 후 공간 확보 시 재개; 복귀 기기 손실 0 |
| S7 Migration Assistant·VM 복제 | A를 Migration Assistant 또는 VM 복제로 A′ 생성, A′에서 메모 삭제·A에서 같은 메모 편집 | A′ 회전 알림, 메모 유지 + "삭제와 충돌한 항목" 알림, 세 기기 동일 |
| S8 동시 첫 설정 → 합류 | 빈 저장소에서 A·B가 동시에 보관함 만들기 → B가 A 보관함에 참여 | 두 보관함 알림, 자동 삭제 0, 참여 미리보기 후 손실 0 |
| S9 마이그레이션 중 대화 계속 | 기존 데이터가 많은 기기에서 동기화 켜기 직후 AI 대화·번역 계속 | 손실·중복 0, "기존 데이터 이동을 마쳤습니다" |

모든 시나리오 후: 설정 → 동기화 → "진단 정보 복사"로 세 기기의 집계를 저장하고, 내용·키·토큰이 없는지 확인합니다.

## 4. 결과 기록

`_sync/g10-gext-status.md`의 표에서 해당 행을 갱신합니다.
- 상태: `passed` / `failed` / `human-blocked`. **실계정에서 실제로 실행한 것만** `passed`로 적습니다.
- 증거: 날짜, 빌드(커밋), 기기/OS, 하네스 JSON 줄(또는 파일 경로), 수동 시나리오는 스크린샷·진단 내보내기 경로.
- 실패 시: 오류 분류와 재현 단계. 외부 정책 불가(무비밀 PKCE 불가 + Q1 거절, MS 게시자 확인 불가, WebDAV 조건부 PUT 미지원, Drive `appProperties` 질의 불가)는 에스컬레이션합니다.

---

## English summary

Operator checklist for real-account acceptance. Register a GCP **Desktop** OAuth client (PKCE, loopback `http://127.0.0.1:<port>/callback`, scopes `openid` + `drive.appdata`; verify secret-less PKCE) and an Entra **public client** (personal + org accounts, `openid offline_access Files.ReadWrite.AppFolder`, no secret). Build with `SAYKNOW_SYNC_{GDRIVE,ONEDRIVE}_CLIENT_ID`.

Automated harness: `src-tauri/src/sync/live_tests.rs`, opt-in via `SAYKNOW_SYNC_LIVE=<provider>[,…]` plus the credential variables in the table above; run with `-- --ignored --nocapture --test-threads=1`. Without the variable each test prints `live skipped: SAYKNOW_SYNC_LIVE unset`, which is not a pass. Use a dedicated empty account/bucket/folder: the round trip refuses to create a vault when discovery finds one (`account_not_empty`). Each run uses fresh random vault ids and deletes only those prefixes. Output is one content-free JSON line per check; only `"live":true` lines are real-account evidence.

Manual E2E S1–S9 on two macOS devices and one Windows device as in the table. Record every row in `_sync/g10-gext-status.md`; never mark `passed` anything not executed against a real account.
