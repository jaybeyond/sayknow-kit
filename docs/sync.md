# 기기 간 동기화 (E2EE)

> 상태: **베타 전 단계.** 동기화는 기능 플래그로 기본 꺼져 있습니다(설정 → 동기화 → "기기 간 동기화").
> 출시 정책(Q3): 플래그 off로 출시 → 실계정 외부 게이트(G-EXT)를 통과한 저장소부터 옵트인 베타 → 네 저장소 모두 통과 시 정식.
> 현재 어떤 저장소도 실계정 게이트를 통과하지 않았습니다(`docs/sync-live-testing.md`). 앱 안에 저장소별 "베타" 표시는 없습니다.

## 1. 무엇이 동기화되나

| 동기화 대상 | 동기화하지 않는 것 |
|---|---|
| 메모, AI 대화(이미지 포함), 번역 기록 | API 키, AI 로그인, 클립보드 캡처, 기기 설정 |

(앱 문구: `settings.sync.syncedList`, `settings.sync.neverSyncedList`.) 저장소 로그인 정보(OAuth 토큰, WebDAV 비밀번호, R2 키)는 이 기기의 키체인에만 있고 동기화되지 않습니다.

## 2. 내 저장소만, 종단간 암호화, 운영자 서버 없음

- 데이터는 사용자가 연결한 저장소(Google Drive, OneDrive, WebDAV, Cloudflare R2) 한 곳에만 저장됩니다. 한 번에 한 계정만 연결됩니다. 다른 계정을 연결하려면 먼저 연결을 끊어야 합니다.
- 기기를 떠나는 모든 내용은 보관함 데이터 키(DEK)로 XChaCha20-Poly1305 봉투에 암호화됩니다. 하위 키는 HKDF-SHA256으로 보관함 ID와 키 세대에 묶입니다. DEK는 웹뷰나 우리 서버에 전달되지 않습니다.
- 우리가 운영하는 동기화 서버는 없습니다. 앱은 저장소 API와 직접 통신합니다.

## 3. 복구 키 (RK)

- 보관함을 만들면 56자 복구 키가 **한 번만** 표시됩니다(256비트 무작위 + 체크섬, Base32). 앱은 키를 보관하지 않으며, 표시 직후 다시 입력해 확인해야 보관함이 만들어집니다.
- 종이나 비밀번호 관리자처럼 **오프라인에** 보관하세요. 클립보드 기록 기능은 유효한 복구 키를 기록하지 않습니다.
- 새 기기는 이 키로만 참여할 수 있습니다. **키를 잃으면 새 기기를 추가할 수 없습니다.** 이미 참여한 기기는 계속 동기화합니다.
- 키는 다시 발급할 수 있습니다(설정 → 동기화 → 기기와 키 → "복구 키 다시 발급"). 폐기 한계는 §7.

## 4. 저장소별 설정

설정 → 동기화에서 "기기 간 동기화"를 켜고 저장소를 고릅니다.

### Google Drive (기본 · 권장)
- "브라우저에서 로그인"을 누르고 브라우저에서 로그인합니다. 앱은 `127.0.0.1` 루프백 주소로 응답을 받습니다(PKCE).
- 요청 권한: `openid`, `drive.appdata`(앱 전용 숨김 폴더만). 일반 Drive 파일은 읽거나 쓰지 않습니다.
- 이 빌드에 Google 클라이언트가 설정되지 않았으면 "이 빌드에는 해당 저장소 로그인이 설정되어 있지 않습니다."가 표시됩니다.

### OneDrive
- "Microsoft 계정으로 로그인" 카드 → 브라우저 로그인. 권한: `openid offline_access Files.ReadWrite.AppFolder`(앱 폴더만).

### WebDAV
- 입력: WebDAV 주소, 사용자 이름, 비밀번호.
- **HTTPS 주소만 허용됩니다.** `http://` 주소는 거부됩니다. 주소에 사용자 정보·쿼리·프래그먼트를 넣을 수 없습니다.
- 주소 아래에 `SayKnowKit/` 폴더를 만들어 사용합니다. 주소 자체(기본 경로)는 이미 있어야 합니다.
- 연결할 때와 앱을 시작할 때마다 **조건부 생성(If-None-Match) 검사**를 합니다. 이미 있는 이름에 대한 생성을 412로 거부하지 않는 서버는 다른 기기의 데이터를 덮어쓸 수 있으므로 연결이 거부됩니다("이 저장소는 지원되지 않습니다.").
- 비밀번호는 키체인에만 저장되며 화면에 다시 표시되지 않습니다.

### Cloudflare R2
- 입력: 계정 ID(32자 소문자 16진수), 버킷, 액세스 키 ID, 비밀 액세스 키.
- 버킷은 미리 만들어 두어야 합니다. 없는 버킷은 "저장소 폴더나 버킷을 찾을 수 없습니다."로 거부됩니다.
- **토큰 범위:** 이 버킷 하나에 한정된 "Object Read & Write" API 토큰을 권장합니다. 앱은 버킷 안 `sayknow/` 접두사만 사용하며 버킷 생성·목록 권한은 필요 없습니다.
- 키는 키체인에만 저장됩니다.

## 5. 첫 설정과 두 보관함

- 저장소를 연결하면 앱이 먼저 보관함이 있는지 확인합니다(최대 30초). 결과가 일정하지 않으면 만들기를 막고 다시 확인합니다("저장소 응답이 일정하지 않습니다…").
- 보관함이 있으면 "복구 키로 참여", 없으면 "새 보관함 만들기"를 고릅니다.
- **두 보관함:** 두 기기가 거의 동시에 첫 설정을 하면 한 저장소에 보관함이 두 개 생길 수 있습니다. 앱은 자동으로 승자를 고르거나 삭제하지 않고 "이 저장소에 다른 보관함 N개가 있습니다. 어느 쪽을 쓸지 고르세요." 알림을 띄웁니다.
  - "그 보관함에 참여": 그 보관함의 복구 키로 참여합니다. 참여 전에 미리보기(그대로 둘 항목 / 올릴 항목 / 복사본으로 유지할 항목)를 보여주며, 이 기기의 데이터는 잃지 않습니다. 이 기기는 새 기기 ID로 그 보관함에 들어가고, 이전 보관함에서 삭제한 항목은 다시 나타나지 않습니다.
  - "지금 것 유지" / "나중에": 아무것도 바꾸지 않습니다.

## 6. 알림과 할 일

| 알림 (앱 문구 요지) | 할 일 |
|---|---|
| 아직 보관함에 참여하지 않았습니다 | 보관함을 만들거나 참여 |
| 이 기기에 보관함 키가 없습니다 / 동기화 정보가 손상되었습니다 | 복구 키로 다시 참여 |
| 키체인을 사용할 수 없어 동기화를 멈췄습니다 | 키체인 잠금 해제 후 재시도 |
| 클라우드 삭제가 끝나지 않았습니다 | 삭제를 마칠 때까지 대기 |
| 다른 기기가 이 보관함을 삭제했습니다 | 이 기기 데이터는 그대로. 새 보관함을 만들거나 다른 보관함에 참여 |
| 다른 보관함 N개가 있습니다 | §5 |
| 보관함 정보 사본이 N개 있습니다 | 삭제된 것 없음. 정리 필요(Drive에서 키 재발급이 겹친 경우) |
| 이 기기가 보관함에서 제거되었습니다 | "다시 참여"(복구 키) |
| 저장소 로그인이 만료되었습니다 | "다시 로그인"(WebDAV·R2는 자격 증명 양식이 다시 열림) |
| 저장소가 접근을 거부했습니다 | 계정 권한 확인 |
| 요청 제한 / 용량 부족 | 자동 재시도 / 공간 확보 |
| 저장소 계정이 연결한 계정과 다릅니다 | 연결 끊고 다시 연결 |
| 필요한 기능을 지원하지 않습니다 | 다른 저장소 사용(예: If-None-Match 미지원 WebDAV) |
| 폴더나 버킷을 찾을 수 없습니다 | 연결 설정 확인 |
| 복제본이 감지되어 기기 ID를 새로 만들었습니다 | 조치 불필요(백업 복원·VM 복제 등). 데이터는 그대로 |
| 기록이 갈라졌습니다. 자동으로 합쳤습니다 / 처음부터 다시 계산했습니다 | 조치 불필요 |
| 삭제와 충돌한 항목 N개를 유지했습니다 | 확인 후 "닫기". 한 기기의 삭제가 아직 보지 못한 다른 기기의 수정과 겹치면, 내용을 잃지 않도록 삭제하지 않고 유지합니다 |
| 올리기 결과를 확인할 수 없어 동기화를 잠시 멈췄습니다 | §10 |
| 검증에 실패한 데이터를 격리했습니다 / 보류 중 / 빈 구간 | 다른 데이터는 정상. 계속되면 진단 정보 복사 |
| 더 새로운 앱 버전으로 만들어졌습니다 | 앱 업데이트 |

## 7. 복구 키 재발급과 폐기 한계 (AC 30)

앱 문구(`settings.sync.advanced.revocationLimit`):

> 복구 키를 다시 발급하면 이전 키로는 새 기기를 추가할 수 없습니다. 이미 연결된 기기는 차단되지 않고, 그 기기에 있는 데이터도 지워지지 않습니다.

기기 제거(`settings.sync.advanced.removeDeviceDesc`):

> 제거된 기기는 새 변경을 올리지 못합니다. 그 기기에 이미 있는 데이터는 지워지지 않으며, 보관함을 계속 읽을 수 있습니다.

즉, 재발급과 제거는 **폐기(revocation)가 아닙니다.** 이미 DEK를 가진 기기는 저장소 접근 권한이 있는 한 보관함을 계속 읽을 수 있습니다. 기기를 잃어버렸다면 그 기기의 저장소 로그인도 저장소 쪽(Google/Microsoft 계정 보안, WebDAV 비밀번호, R2 토큰)에서 끊으세요.

## 8. 메타데이터 노출

내용은 암호화되지만 저장소는 다음을 볼 수 있습니다.
- 객체 이름(키): `v1/<vault_id>/…` 형태로 보관함 ID, 기기 ID, 순번 범위, 객체 종류가 드러납니다.
- **Google Drive:** 각 파일의 `appProperties`에 `vault_id`, `kind`, `device_id`가 평문으로 들어갑니다(목록 조회에 사용).
- 객체 크기와 업로드 시각, 업로드 빈도.
- 연결한 계정(Google `sub`, Microsoft 테넌트/객체 ID, WebDAV 주소와 사용자, R2 계정·버킷).

## 9. 연결 끊기와 클라우드 삭제

- "연결 끊기": 이 기기 데이터는 그대로 두고 저장소 연결과 키체인 자격 증명만 지웁니다.
- "클라우드 데이터도 삭제": `삭제`를 정확히 입력해야 합니다. 보관함 정보(manifest)를 먼저 지우고, 두 번 없음을 확인한 뒤 `v1/<vault_id>/` 아래 객체만 지웁니다(Drive는 `appProperties`의 `vault_id`로 찾음). 다른 보관함이나 저장소의 다른 파일은 건드리지 않습니다. 중간에 끊겨도 다음에 이어서 진행합니다.

## 10. 남는 위험 (알려진 한계)

- **올리기 결과 미확정:** 네트워크가 끊겨 업로드 성공 여부를 알 수 없으면, 앱은 같은 바이트로만 다시 확인·재전송하고 그동안 회전(기기 ID 재생성)을 미룹니다. 오래 지속되면 "올리기 결과를 확인할 수 없어 동기화를 잠시 멈췄습니다." 알림이 뜹니다. 저장소 용량·권한 문제를 해결하면 다음 주기에 이어집니다.
- **삭제 후 고아 객체(D7):** 클라우드 삭제 중 다른 기기가 오프라인 상태로 업로드 도중이었다면, 삭제의 마지막 확인(약 15초 후 재확인)보다 늦게 객체 하나가 남을 수 있습니다. 다음 클라우드 삭제나 수동 정리(저장소에서 `v1/<vault_id>/` 삭제)로 지웁니다.
- **두 보관함:** 자동으로 합치지 않습니다(§5).

## 11. 문제 해결

- **동기화가 시작되지 않음:** 설정 → 동기화의 "상태"에서 마지막 동기화 결과를 확인합니다("사용자 조치 필요"면 알림의 버튼을 따름).
- **WebDAV 연결 거부:** HTTPS인지, 서버가 `If-None-Match: *`에 412를 돌려주는지 확인합니다. 일부 서버·프록시는 이 헤더를 무시합니다.
- **R2 "폴더나 버킷을 찾을 수 없습니다":** 버킷 이름과 토큰 범위를 확인합니다.
- **복구 키 오류:** 56자인지 확인합니다. 공백·하이픈·대소문자는 무시됩니다. "이 키로 열리는 보관함이 이 저장소에 없습니다"면 다른 계정이나 이전 키일 수 있습니다.
- **진단:** "진단 정보 복사"는 내용·키·토큰 없이 상태와 집계만 복사합니다.

---

# Sync between devices (E2EE) — English

> Status: **pre-beta.** Sync is behind a feature flag that is off by default (Settings → Sync → the device-sync toggle). Release policy (Q3): ship with the flag off → opt-in beta for each provider that has passed the real-account external gate (G-EXT) → general availability once all four pass. No provider has passed the real-account gate yet (`docs/sync-live-testing.md`). The app shows no per-provider "beta" label.

**What syncs:** memos, AI conversations (with images), translation history. **Never synced:** API keys, AI sign-ins, clipboard captures, device settings. Storage credentials stay in this device's keychain.

**Your storage only, E2EE, no operator server.** Data lives only in the one account you connect (Google Drive, OneDrive, WebDAV, Cloudflare R2). Everything is sealed with the vault key (XChaCha20-Poly1305, HKDF-SHA256 subkeys bound to the vault id and key epoch) before it leaves the device. There is no sync server we operate.

**Recovery key.** A 56-character key (256 random bits + checksum, Base32) is shown once at vault creation and must be re-entered to confirm. The app does not keep it; store it offline. New devices can only join with it: if you lose it, no new device can join (existing devices keep syncing).

**Provider setup.**
- Google Drive (default): browser sign-in over a loopback redirect with PKCE; scopes `openid` and `drive.appdata` (hidden app folder only).
- OneDrive: browser sign-in; scopes `openid offline_access Files.ReadWrite.AppFolder`.
- WebDAV: URL, user name, password. **HTTPS only.** Data goes under `SayKnowKit/` below the URL. Every connect and app start probes conditional create (`If-None-Match: *` must yield 412 on an existing name); a server that ignores it is refused.
- Cloudflare R2: account id (32 lowercase hex), bucket, access key id, secret. The bucket must exist. Use an API token scoped to that one bucket with Object Read & Write; the app uses only the `sayknow/` prefix inside it.

**Two vaults.** Concurrent first setup can leave two vaults in one account. Nothing is auto-merged or deleted; a banner offers "join that vault" (with a preview; no local data is lost, the device gets a new device id there, and items you deleted earlier do not come back) or "keep the current one".

**Notices.** See the Korean table in §6; each notice names its action (sign in again, rejoin with the RK, free up space, update the app, …).

**Revocation limit (AC 30).** Reissuing the recovery key means the old key can no longer add new devices; devices already connected are not blocked and the data on them is not erased. A removed device can no longer upload new changes; data already on it is not erased and it can still read the vault. Neither is revocation. Also revoke a lost device's storage sign-in at the provider.

**Metadata exposure.** Object names reveal vault id, device id, sequence ranges and kind. On Google Drive each file's `appProperties` carry `vault_id`, `kind` and `device_id` in plaintext. Sizes, upload times and the connected account are visible to the provider.

**Disconnect and wipe.** Disconnect keeps local data and forgets the connection and keychain credential. Wipe requires typing `삭제`, deletes the manifest, confirms it is gone twice, then deletes only objects under `v1/<vault_id>/` (Drive: by `appProperties.vault_id`); it resumes after interruption.

**Residual risks.** *Upload outcome unknown:* the app re-checks and resends only the identical bytes and postpones rotation; a long wait raises the "sync paused: upload outcome unknown" notice. *Wipe orphan (D7):* a peer that was offline mid-upload can leave one object after the wipe's final sweep (≈15 s re-check); the next wipe or a manual delete of `v1/<vault_id>/` removes it.

**Troubleshooting.** Check the status row; follow the notice buttons; for WebDAV verify HTTPS and `If-None-Match` support; for R2 verify bucket and token scope; "Copy diagnostics" copies counts and states only, no content, keys or tokens.
