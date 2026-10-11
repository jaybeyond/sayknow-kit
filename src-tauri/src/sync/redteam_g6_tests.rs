//! G6 red team (ultragoal G006): adversarial tests for native OAuth
//! (`sync::oauth`), the Google Drive adapter, the OneDrive adapter, and the
//! `ListQuery::Props` translation of the keyed stores (approved plan §4.2,
//! §4.2.3, §4.8, §4.9; AC 14, AC 31; `_sync/g6-design.md`;
//! `_sync/plan-amendments.md` B1/ADV-3). Written against the contract rather
//! than the implementation. Everything runs over `MockTransport` emulators or
//! an in-process transport: no socket is ever opened here (the native
//! loopback listener attacks live in `oauth_callback::redteam_g6_oauth`,
//! because they need the private slot fields).
//!
//! A real defect is reproduced as `#[ignore = "G6 blocker: ..."]`; every
//! other case is a normal test.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use data_encoding::{BASE64, BASE64URL_NOPAD};
use zeroize::Zeroizing;

use super::oauth::{
    exchange_code, identity_from_id_token, refresh, DriveOAuthConfig, OAuthTokens, OneDriveOAuthConfig, Pkce,
    ProviderAuth,
};
use super::provider::fake::{FakeStore, Op};
use super::provider::gdrive::tests_support::{self as drives, DriveServer};
use super::provider::gdrive::GDriveStore;
use super::provider::onedrive::tests_support::{self as ones, GraphServer};
use super::provider::onedrive::SIMPLE_UPLOAD_MAX;
use super::provider::transport::{HttpRequest, HttpResponse, HttpTransport, MockTransport, TransportFailure};
use super::provider::{
    binding_fp, AccessToken, Bound, ListQuery, ObjectStore, ProviderError, RemoteKind, StaticToken, MAX_OBJECT_BYTES,
};
use super::publish::{settle_attempted, Outcome, Pause, PutOutcome, SegmentRemote, TransportError, Verdict};
use super::secrets::{read_secret, write_secret, Account, CloudProvider, MemoryStore, SecretError, SecretStore, SyncSecret};

const SEG: &str = "v1/vault-a/log/dev-a/000000000001-000000000003.seg";
const REC: &str = "v1/vault-a/devices/dev-a.rec";
const OURS: &[u8] = b"our sealed segment";
const THEIRS: &[u8] = b"their sealed segment";
const CLIENT_ID: &str = "rt-client.apps.example";
const CLIENT_SECRET: &str = "rt-client-SECRET-value";

fn json(status: u16, body: &str) -> HttpResponse {
    HttpResponse {
        status,
        headers: vec![("Content-Type".to_owned(), "application/json".to_owned())],
        body: body.as_bytes().to_vec(),
    }
}

fn permanent(err: &ProviderError) -> bool {
    matches!(err, ProviderError::Permanent(_))
}

fn props<'a>(vault_id: Option<&'a str>, kind: Option<RemoteKind>, device_id: Option<&'a str>) -> ListQuery<'a> {
    ListQuery::Props { vault_id, kind, device_id }
}

struct NoPause;

impl Pause for NoPause {
    async fn pause(&self, _: Duration) {}
}

fn judge(bytes: &[u8]) -> Verdict {
    match bytes {
        OURS => Verdict::Ours,
        THEIRS => Verdict::Other,
        _ => Verdict::Invalid,
    }
}

// ===========================================================================
// OAuth (§4.8, g6-design §B)
// ===========================================================================

fn drive_auth() -> ProviderAuth {
    ProviderAuth::drive(DriveOAuthConfig { client_id: CLIENT_ID.into(), client_secret: Some(CLIENT_SECRET.into()) })
}

fn onedrive_auth() -> ProviderAuth {
    ProviderAuth::onedrive(OneDriveOAuthConfig { client_id: CLIENT_ID.into(), client_secret: None })
}

fn fp() -> String {
    binding_fp("gdrive|1234")
}

/// A keychain the test keeps reading after the token source took ownership.
#[derive(Clone, Default)]
struct SharedStore(Arc<MemoryStore>);

impl SecretStore for SharedStore {
    fn get(&self, account: &Account) -> Result<Option<Zeroizing<String>>, SecretError> {
        self.0.get(account)
    }
    fn set(&self, account: &Account, value: &str) -> Result<(), SecretError> {
        self.0.set(account, value)
    }
    fn delete(&self, account: &Account) -> Result<(), SecretError> {
        self.0.delete(account)
    }
}

fn seed_refresh(store: &SharedStore, provider: CloudProvider, token: &str) {
    write_secret(&*store.0, SyncSecret::Credential { provider, binding_fp: &fp() }, token.as_bytes()).unwrap();
}

fn stored_refresh(store: &SharedStore, provider: CloudProvider) -> Option<String> {
    read_secret(&*store.0, SyncSecret::Credential { provider, binding_fp: &fp() })
        .unwrap()
        .map(|b| String::from_utf8(b.to_vec()).unwrap())
}

fn form_field(req: &HttpRequest, name: &str) -> Option<String> {
    url::form_urlencoded::parse(&req.body).into_owned().find(|(k, _)| k == name).map(|(_, v)| v)
}

type Endpoint = dyn Fn(usize, &HttpRequest) -> HttpResponse + Send + Sync;

/// A token endpoint that answers after a real await point, so concurrent
/// `token()` calls genuinely overlap inside the refresh.
#[derive(Clone)]
struct SlowEndpoint {
    calls: Arc<AtomicUsize>,
    seen: Arc<Mutex<Vec<HttpRequest>>>,
    answer: Arc<Endpoint>,
}

impl SlowEndpoint {
    fn new(answer: impl Fn(usize, &HttpRequest) -> HttpResponse + Send + Sync + 'static) -> Self {
        Self { calls: Arc::new(AtomicUsize::new(0)), seen: Arc::default(), answer: Arc::new(answer) }
    }

    fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }
}

impl HttpTransport for SlowEndpoint {
    async fn send(&self, req: HttpRequest) -> Result<HttpResponse, TransportFailure> {
        let n = self.calls.fetch_add(1, Ordering::SeqCst) + 1;
        tokio::time::sleep(Duration::from_millis(40)).await;
        let answer = (self.answer)(n, &req);
        self.seen.lock().unwrap().push(req);
        Ok(answer)
    }
}

fn id_token(claims: &[u8]) -> String {
    format!("eyJhbGciOiJSUzI1NiJ9.{}.c2ln", BASE64URL_NOPAD.encode(claims))
}

/// RT-OAUTH-1: malformed `id_token`s (bad base64, missing claims, non-JSON
/// payloads) are `Permanent`, never a panic, never an identity.
#[test]
fn id_token_bad_base64_missing_claims_and_non_json_are_permanent() {
    let std_b64 = format!("h.{}.s", BASE64.encode(b"{\"sub\":\"1\",\"x\":\"\xfb\xff\"}"));
    let huge = id_token(format!(r#"{{"sub":"{}"}}"#, "1".repeat(257)).as_bytes());
    let cases: Vec<(CloudProvider, String, &str)> = vec![
        (CloudProvider::GoogleDrive, std_b64, "standard base64 alphabet (+ /)"),
        (CloudProvider::GoogleDrive, "h.eyJzdWIiOiIxIn0=x.s".to_owned(), "padding in the middle"),
        (CloudProvider::GoogleDrive, "h. eyJzdWIiOiIxIn0.s".to_owned(), "whitespace"),
        (CloudProvider::GoogleDrive, "h..s".to_owned(), "empty payload"),
        (CloudProvider::GoogleDrive, "h.e.s".to_owned(), "1-char payload (invalid length)"),
        (CloudProvider::GoogleDrive, ".eyJzdWIiOiIxIn0".to_owned(), "two parts"),
        (CloudProvider::GoogleDrive, format!("a.{}.c.d.e", BASE64URL_NOPAD.encode(br#"{"sub":"1"}"#)), "JWE (5 parts)"),
        (CloudProvider::GoogleDrive, id_token(b"[]"), "array payload"),
        (CloudProvider::GoogleDrive, id_token(b"null"), "null payload"),
        (CloudProvider::GoogleDrive, id_token(br#""sub""#), "string payload"),
        (CloudProvider::GoogleDrive, id_token(b"\xff\xfe{}"), "non-UTF-8 payload"),
        (CloudProvider::GoogleDrive, id_token(br#"{"sub":null}"#), "null sub"),
        (CloudProvider::GoogleDrive, id_token(br#"{"sub":{"id":"1"}}"#), "object sub"),
        (CloudProvider::GoogleDrive, id_token(br#"{"sub":"1 2"}"#), "space in sub"),
        (CloudProvider::GoogleDrive, id_token("{\"sub\":\"\u{e9}\"}".as_bytes()), "non-ASCII sub"),
        (CloudProvider::GoogleDrive, id_token(br#"{"sub":"a|b"}"#), "separator in sub"),
        (CloudProvider::GoogleDrive, huge, "257-byte sub"),
        (CloudProvider::GoogleDrive, id_token(br#"{"oid":"o","tid":"t"}"#), "Microsoft claims for Drive"),
        (CloudProvider::OneDrive, id_token(br#"{"sub":"1"}"#), "Google claims for OneDrive"),
        (CloudProvider::OneDrive, id_token(br#"{"tid":"t","oid":""}"#), "empty oid"),
        (CloudProvider::OneDrive, id_token(br#"{"tid":"t\n","oid":"o"}"#), "control char tid"),
    ];
    for (provider, token, what) in cases {
        let result = identity_from_id_token(provider, &token);
        assert!(matches!(result, Err(ProviderError::Permanent(_))), "{what}: {result:?}");
    }
    // Non-OAuth providers are refused, not mapped.
    let ok = id_token(br#"{"sub":"1"}"#);
    assert!(matches!(identity_from_id_token(CloudProvider::WebDav, &ok), Err(ProviderError::Unsupported(_))));
    // Duplicate keys: serde_json keeps the last one; still one canonical id.
    let dup = id_token(br#"{"sub":"first","sub":"second"}"#);
    let identity = identity_from_id_token(CloudProvider::GoogleDrive, &dup).unwrap();
    assert!(identity == "gdrive|first" || identity == "gdrive|second", "{identity}");
}

/// RT-OAUTH-2: two (three) concurrent `token()` calls on a cold cache cause
/// exactly one refresh and all get the same access token.
#[tokio::test]
async fn concurrent_token_calls_refresh_once() {
    let store = SharedStore::default();
    seed_refresh(&store, CloudProvider::GoogleDrive, "rt-1");
    let endpoint = SlowEndpoint::new(|n, _| json(200, &format!(r#"{{"access_token":"at-{n}","expires_in":3600}}"#)));
    let tokens = OAuthTokens::new(store.clone(), endpoint.clone(), drive_auth(), fp());
    let (a, b, c) = tokio::join!(tokens.token(), tokens.token(), tokens.token());
    assert_eq!((a.unwrap(), b.unwrap(), c.unwrap()), ("at-1".into(), "at-1".into(), "at-1".into()));
    assert_eq!(endpoint.calls(), 1, "one refresh for concurrent callers");

    // Two callers that both saw a 401 invalidate, then fetch concurrently:
    // still one refresh.
    tokens.invalidate();
    tokens.invalidate();
    let (a, b) = tokio::join!(tokens.token(), tokens.token());
    assert_eq!((a.unwrap(), b.unwrap()), ("at-2".into(), "at-2".into()));
    assert_eq!(endpoint.calls(), 2);
}

/// RT-OAUTH-3: concurrent refreshes with a rotating refresh token: the single
/// refresh rotates once, the rotation is persisted, and the next refresh
/// presents the rotated token (never the stale one).
#[tokio::test]
async fn concurrent_refresh_with_rotation_persists_and_uses_the_new_token() {
    let store = SharedStore::default();
    seed_refresh(&store, CloudProvider::OneDrive, "rt-1");
    let endpoint = SlowEndpoint::new(|n, req| {
        let expected = format!("rt-{n}");
        if form_field(req, "refresh_token").as_deref() != Some(expected.as_str()) {
            return json(400, r#"{"error":"invalid_grant"}"#);
        }
        json(200, &format!(r#"{{"access_token":"at-{n}","refresh_token":"rt-{}","expires_in":3600}}"#, n + 1))
    });
    let tokens = OAuthTokens::new(store.clone(), endpoint.clone(), onedrive_auth(), fp());
    let (a, b) = tokio::join!(tokens.token(), tokens.token());
    assert_eq!((a.unwrap(), b.unwrap()), ("at-1".into(), "at-1".into()));
    assert_eq!(stored_refresh(&store, CloudProvider::OneDrive).as_deref(), Some("rt-2"));
    tokens.invalidate();
    assert_eq!(tokens.token().await.unwrap(), "at-2");
    assert_eq!(stored_refresh(&store, CloudProvider::OneDrive).as_deref(), Some("rt-3"));
    assert_eq!(endpoint.calls(), 2);
}

/// RT-OAUTH-4: a rotated refresh token the keychain refuses is kept in
/// memory and is what the next refresh presents, even if that next refresh
/// fails; the keychain write is retried on the following success.
#[tokio::test]
async fn rotated_token_survives_keychain_failure_and_a_later_error() {
    let store = SharedStore::default();
    seed_refresh(&store, CloudProvider::GoogleDrive, "rt-1");
    let endpoint = SlowEndpoint::new(|n, req| match (n, form_field(req, "refresh_token").as_deref()) {
        (1, Some("rt-1")) => json(200, r#"{"access_token":"a1","refresh_token":"rt-2","expires_in":3600}"#),
        (2, Some("rt-2")) => json(503, "busy rt-2"),
        (3, Some("rt-2")) => json(200, r#"{"access_token":"a3","expires_in":3600}"#),
        _ => json(400, r#"{"error":"invalid_grant"}"#),
    });
    let tokens = OAuthTokens::new(store.clone(), endpoint.clone(), drive_auth(), fp());
    store.0.fail_next_sets(1);
    assert_eq!(tokens.token().await.unwrap(), "a1");
    assert_eq!(stored_refresh(&store, CloudProvider::GoogleDrive).as_deref(), Some("rt-1"), "write refused");
    tokens.invalidate();
    let err = tokens.token().await.unwrap_err();
    assert!(matches!(err, ProviderError::RateLimited { .. }), "{err:?}");
    assert!(!format!("{err:?}").contains("rt-2"));
    assert_eq!(tokens.token().await.unwrap(), "a3", "the in-memory rotation is still presented");
    assert_eq!(stored_refresh(&store, CloudProvider::GoogleDrive).as_deref(), Some("rt-2"), "persisted on success");
}

/// RT-OAUTH-5 (observation): a 200 that rotates the refresh token but is
/// otherwise unusable (no access token / non-bearer) fails closed; the
/// rotation in that unusable answer is not adopted and the old token stays.
#[tokio::test]
async fn unusable_rotation_answer_fails_closed_and_keeps_the_old_token() {
    for body in [
        r#"{"refresh_token":"rt-NEW-SECRET","expires_in":3600}"#,
        r#"{"access_token":"a","token_type":"mac","refresh_token":"rt-NEW-SECRET"}"#,
    ] {
        let store = SharedStore::default();
        seed_refresh(&store, CloudProvider::OneDrive, "rt-1");
        let body = body.to_owned();
        let endpoint = SlowEndpoint::new(move |_, _| json(200, &body));
        let tokens = OAuthTokens::new(store.clone(), endpoint, onedrive_auth(), fp());
        let err = tokens.token().await.unwrap_err();
        assert!(permanent(&err), "{err:?}");
        assert!(!format!("{err:?} {err:?}").contains("rt-NEW-SECRET"));
        assert_eq!(stored_refresh(&store, CloudProvider::OneDrive).as_deref(), Some("rt-1"));
    }
}

/// RT-OAUTH-6: no secret (client secret, verifier, code, refresh, access or
/// id token) reaches Debug output of any OAuth type, request, or error.
#[tokio::test]
async fn no_oauth_secret_reaches_debug_or_errors() {
    let pkce = Pkce::generate().unwrap();
    let verifier = pkce.verifier.to_string();
    let store = SharedStore::default();
    let endpoint = SlowEndpoint::new(|n, _| match n {
        1 => json(400, r#"{"error":"invalid_request","error_description":"code-SECRET ver rt-SECRET"}"#),
        2 => json(500, "code-SECRET at-SECRET"),
        _ => json(
            200,
            &format!(
                r#"{{"access_token":"at-SECRET","refresh_token":"rt-SECRET","expires_in":3600,"id_token":"{}"}}"#,
                id_token(br#"{"sub":"1234"}"#)
            ),
        ),
    });
    let mut shown = String::new();
    for _ in 0..2 {
        let err = exchange_code(&endpoint, &drive_auth(), "code-SECRET", &verifier, "http://127.0.0.1:1/callback")
            .await
            .unwrap_err();
        shown.push_str(&format!("{err:?} {err:?}"));
    }
    let (tokens, identity) =
        OAuthTokens::connect(store.clone(), endpoint.clone(), drive_auth(), "code-SECRET", &verifier, "r").await.unwrap();
    assert_eq!(identity, "gdrive|1234");
    let set = refresh(&endpoint, &drive_auth(), "rt-SECRET").await.unwrap();
    shown.push_str(&format!(
        "{pkce:?} {set:?} {tokens:?} {:?} {:?} {:?}",
        drive_auth(),
        onedrive_auth(),
        endpoint.seen.lock().unwrap()
    ));
    for secret in ["code-SECRET", "rt-SECRET", "at-SECRET", CLIENT_SECRET, verifier.as_str(), "eyJhbGciOiJSUzI1NiJ9"] {
        assert!(!shown.contains(secret), "{secret} leaked: {shown}");
    }
}

// ===========================================================================
// Google Drive (§4.2 N-5, g6-design §D)
// ===========================================================================

fn drive_props_of(req: &HttpRequest) -> Option<(Vec<(String, String)>, bool)> {
    let url = url::Url::parse(&req.url).ok()?;
    let q: Vec<String> = url.query_pairs().filter(|(k, _)| k == "q").map(|(_, v)| v.into_owned()).collect();
    match q.as_slice() {
        [only] => drives::parse_q(only),
        _ => None,
    }
}

/// RT-DRIVE-1: names with quotes or backslashes never reach a Drive query
/// (keys and Props ids are refused before any request), and every query the
/// adapter does send parses as exactly the expected appProperties clauses.
#[tokio::test]
async fn drive_quotes_and_backslashes_never_reach_a_query() {
    let (store, transport, server) = drives::emulated();
    server.lock().unwrap().seed_key(SEG, OURS);
    for key in [
        "v1/vault-a/log/dev'a/000000000001-000000000001.seg",
        r"v1/vault\a/manifest.json",
        "v1/vault-a/manifest.json' or name contains '",
        r"v1/vault-a/devices/dev-a\.rec",
    ] {
        assert!(permanent(&store.get(key).await.unwrap_err()), "{key}");
        assert!(permanent(&store.get_all(key).await.unwrap_err()), "{key}");
        assert!(permanent(&store.put_create(key, OURS).await.unwrap_err()), "{key}");
        assert!(permanent(&store.put_replace(key, OURS, None).await.unwrap_err()), "{key}");
        assert!(permanent(&store.delete(key).await.unwrap_err()), "{key}");
    }
    for (vault_id, device_id) in [
        (Some("vault'a"), None),
        (Some(r"vault\"), None),
        (Some("vault-a"), Some("d' or trashed=true or 'x")),
        (Some("vault-a"), Some(r"d\'")),
        (None, Some("dev-a' }")),
    ] {
        let err = store.list(props(vault_id, Some(RemoteKind::Seg), device_id)).await.unwrap_err();
        assert!(permanent(&err), "{vault_id:?} {device_id:?}");
    }
    assert_eq!(transport.request_count(), 0, "nothing hostile was sent");

    assert_eq!(store.get(SEG).await.unwrap(), OURS);
    let lists: Vec<HttpRequest> = transport.requests().into_iter().filter(|r| r.url.contains("/drive/v3/files?")).collect();
    assert!(!lists.is_empty());
    for req in lists {
        let (clauses, trashed_false) = drive_props_of(&req).expect("one parseable q");
        assert!(trashed_false);
        assert_eq!(
            clauses,
            [
                ("vault_id".to_owned(), "vault-a".to_owned()),
                ("kind".to_owned(), "seg".to_owned()),
                ("device_id".to_owned(), "dev-a".to_owned())
            ]
        );
    }
}

/// RT-DRIVE-2: a file whose name equals our key but whose appProperties name
/// another vault is never read, listed, replaced, or deleted as ours, and does
/// not block our create.
#[tokio::test]
async fn drive_same_name_with_another_vaults_properties_is_not_ours() {
    let (store, transport, server) = drives::emulated();
    let decoy = server
        .lock()
        .unwrap()
        .seed(SEG, &[("vault_id", "vault-b"), ("kind", "seg"), ("device_id", "dev-a")], THEIRS);
    assert_eq!(store.get(SEG).await, Err(ProviderError::NotFound));
    assert!(store.list(props(Some("vault-b"), Some(RemoteKind::Seg), Some("dev-a"))).await.unwrap().is_empty());
    assert!(store.list(props(None, Some(RemoteKind::Seg), None)).await.unwrap().is_empty());

    let created = store.put_create(SEG, OURS).await.unwrap();
    assert_ne!(created.provider_ref, decoy);
    let all = store.get_all(SEG).await.unwrap();
    assert_eq!(all.len(), 1);
    assert_eq!((all[0].provider_ref.as_str(), all[0].bytes.as_slice()), (created.provider_ref.as_str(), OURS));
    store.put_replace(SEG, b"replaced", None).await.unwrap();
    store.delete(SEG).await.unwrap();
    let left: Vec<(String, Vec<u8>)> = server.lock().unwrap().files.iter().map(|f| (f.id.clone(), f.bytes.clone())).collect();
    assert_eq!(left, [(decoy.clone(), THEIRS.to_vec())], "the other vault's file is untouched");

    // Even a server that answers our vault query with that file is ignored.
    let hostile = format!(
        r#"{{"files":[{{"id":"{decoy}","name":"{SEG}","size":"5","appProperties":{{"vault_id":"vault-b","kind":"seg","device_id":"dev-a"}}}}]}}"#
    );
    transport.script_next(Ok(json(200, &hostile)));
    assert_eq!(store.get_all(SEG).await, Ok(Vec::new()));
}

/// RT-DRIVE-3: pagination loops. A self-repeating token stops at once; a page
/// token that smuggles query syntax stays one literal value; a 2-cycle fails
/// closed (never an Ok listing).
#[tokio::test]
async fn drive_pagination_loops_fail_closed() {
    let page = |next: Option<&str>| {
        let token = next.map(|t| format!(r#","nextPageToken":{}"#, serde_json::to_string(t).unwrap())).unwrap_or_default();
        json(200, &format!(r#"{{"files":[]{token}}}"#))
    };
    let query = props(Some("vault-a"), Some(RemoteKind::Seg), Some("dev-a"));

    // Self-loop: A -> A.
    let (store, transport, _) = drives::emulated();
    transport.script_next(Ok(page(Some("A"))));
    transport.script_next(Ok(page(Some("A"))));
    assert!(permanent(&store.list(query).await.unwrap_err()));
    assert_eq!(transport.request_count(), 2);

    // Empty token.
    let (store, transport, _) = drives::emulated();
    transport.script_next(Ok(page(Some(""))));
    assert!(permanent(&store.list(query).await.unwrap_err()));

    // Injection in the page token.
    let (store, transport, _) = drives::emulated();
    let evil = "x&q=name contains 'v1'&spaces=drive&pageToken=y";
    transport.script_next(Ok(page(Some(evil))));
    transport.script_next(Ok(page(None)));
    assert_eq!(store.list(query).await, Ok(Vec::new()));
    let second = url::Url::parse(&transport.requests()[1].url).unwrap();
    let tokens: Vec<String> = second.query_pairs().filter(|(k, _)| k == "pageToken").map(|(_, v)| v.into_owned()).collect();
    assert_eq!(tokens, [evil]);
    assert_eq!(second.query_pairs().filter(|(k, _)| k == "spaces").count(), 1);
    assert!(drive_props_of(&transport.requests()[1]).is_some());

    // 2-cycle: A -> B -> A -> ... The adapter only detects an immediate
    // repeat, so it keeps paging; an external stop after 1000 pages proves it
    // never returns a (partial or duplicated) listing as complete.
    let calls = Arc::new(AtomicUsize::new(0));
    let counter = calls.clone();
    let transport = MockTransport::new(move |req| {
        let n = counter.fetch_add(1, Ordering::SeqCst) + 1;
        if n > 1000 {
            return json(500, "{}");
        }
        let token = url::Url::parse(&req.url).unwrap().query_pairs().find(|(k, _)| k == "pageToken").map(|(_, v)| v.into_owned());
        let next = if token.as_deref() == Some("A") { "B" } else { "A" };
        json(200, &format!(r#"{{"files":[{{"id":"f1","name":"{SEG}","appProperties":{{"vault_id":"vault-a","kind":"seg","device_id":"dev-a"}}}}],"nextPageToken":"{next}"}}"#))
    });
    let store = GDriveStore::new(transport, StaticToken::new(drives::TOKEN), drives::SUB.to_owned()).unwrap();
    assert!(store.list(query).await.is_err(), "a cycling listing never completes");
    assert_eq!(calls.load(Ordering::SeqCst), 1001, "observation: a 2-cycle is not detected early");
}

/// RT-DRIVE-4: duplicates. Identical bytes are one object for `get`;
/// differing bytes are `Permanent("duplicate name")`; `put_replace` refuses
/// several copies; `put_create` reports AlreadyExists; `delete` removes all.
#[tokio::test]
async fn drive_duplicates_identical_vs_different_bytes() {
    let (store, _transport, server) = drives::emulated();
    {
        let mut server = server.lock().unwrap();
        server.seed_key(SEG, OURS);
        server.seed_key(SEG, OURS);
    }
    assert_eq!(store.get(SEG).await.unwrap(), OURS);
    assert_eq!(store.get_all(SEG).await.unwrap().len(), 2);
    assert!(permanent(&store.put_replace(SEG, OURS, None).await.unwrap_err()), "two copies: nothing single to replace");
    server.lock().unwrap().seed_key(SEG, THEIRS);
    assert_eq!(store.get(SEG).await, Err(ProviderError::Permanent("duplicate name".to_owned())));
    let all = store.get_all(SEG).await.unwrap();
    assert_eq!(all.iter().map(|o| o.bytes.as_slice()).collect::<Vec<_>>(), [OURS, OURS, THEIRS]);
    let mut refs: Vec<&str> = all.iter().map(|o| o.provider_ref.as_str()).collect();
    refs.dedup();
    assert_eq!(refs.len(), 3, "every copy has its own provider_ref");
    assert_eq!(store.put_create(SEG, OURS).await, Err(ProviderError::AlreadyExists));
    store.delete(SEG).await.unwrap();
    assert_eq!(store.get(SEG).await, Err(ProviderError::NotFound));
    assert!(server.lock().unwrap().copies(SEG).is_empty());
}

/// Yields before every request, so two adapters' requests interleave.
#[derive(Clone)]
struct Yielding(MockTransport);

impl HttpTransport for Yielding {
    async fn send(&self, req: HttpRequest) -> Result<HttpResponse, TransportFailure> {
        tokio::task::yield_now().await;
        self.0.send(req).await
    }
}

/// RT-DRIVE-5: two devices race `put_create` on the same key. Both pre-checks
/// see nothing, both upload, both report Created; the duplicate is visible
/// to `get_all` and the resolver reports the fork for either side.
#[tokio::test]
async fn drive_put_create_prequery_race_yields_a_visible_fork() {
    let server = Arc::new(Mutex::new(DriveServer::new()));
    let make = || {
        let shared = server.clone();
        let transport = Yielding(MockTransport::new(move |req| shared.lock().unwrap().handle(req)));
        GDriveStore::new(transport, StaticToken::new(drives::TOKEN), drives::SUB.to_owned()).unwrap()
    };
    let (a, b) = (make(), make());
    let (ra, rb) = tokio::join!(a.put_create(SEG, OURS), b.put_create(SEG, THEIRS));
    let (ra, rb) = (ra.unwrap(), rb.unwrap());
    assert_ne!(ra.provider_ref, rb.provider_ref);
    assert_eq!(server.lock().unwrap().copies(SEG).len(), 2);
    assert_eq!(a.get(SEG).await, Err(ProviderError::Permanent("duplicate name".to_owned())));
    let remote = Bound::new(a, binding_fp(&format!("gdrive|{}", drives::SUB)));
    let settled = settle_attempted(&remote, &NoPause, SEG, OURS, judge).await;
    assert_eq!(settled.outcome, Outcome::PublishedWithFork);
}

/// RT-DRIVE-6 (regression for resolved blocker B-G6-1): plan-amendments B1
/// says that on Drive another writer's object under our key is never a
/// rejection: settle resends our identical bytes and reports
/// `PublishedWithFork`. The pass-1 `put_create` pre-query answered
/// AlreadyExists, so the resend never landed and the segment stayed
/// `StillAttempted`; the bridge now resends through `put_create_unchecked`.
#[tokio::test]
async fn drive_settle_with_only_another_writers_object_publishes_a_fork() {
    let (store, transport, server) = drives::emulated();
    server.lock().unwrap().seed_key(SEG, THEIRS);
    let remote = Bound::new(store, binding_fp(&format!("gdrive|{}", drives::SUB)));
    let mut outcomes = Vec::new();
    for _ in 0..3 {
        let settled = settle_attempted(&remote, &NoPause, SEG, OURS, judge).await;
        outcomes.push(settled.outcome);
        if settled.outcome != Outcome::StillAttempted {
            break;
        }
    }
    let uploads = transport.requests().iter().filter(|r| r.url.contains("/upload/drive/v3/files") && r.method == "POST").count();
    assert_eq!(
        outcomes.last(),
        Some(&Outcome::PublishedWithFork),
        "outcomes {outcomes:?}, uploads {uploads}, copies {:?}",
        server.lock().unwrap().copies(SEG).len()
    );
}

// ===========================================================================
// OneDrive (§4.2, g6-design §E)
// ===========================================================================

fn redirect(status: u16, location: Option<&str>) -> HttpResponse {
    HttpResponse {
        status,
        headers: location.map(|l| vec![("Location".to_owned(), l.to_owned())]).unwrap_or_default(),
        body: Vec::new(),
    }
}

/// RT-ONE-1: a download redirect is followed only to an absolute https URL,
/// once, without Authorization. http, other schemes, scheme-relative and
/// non-302 redirects send nothing further.
#[tokio::test]
async fn onedrive_download_redirect_scheme_and_status_checks() {
    let (store, transport, state) = ones::over(GraphServer::new());
    state.lock().unwrap().seed(SEG, b"payload");
    let dl = ones::DOWNLOAD_HOST;
    let refused = [
        (302, Some(format!("http://{dl}/{SEG}"))),
        (302, Some(format!("ftp://{dl}/{SEG}"))),
        (302, Some("file:///etc/passwd".to_owned())),
        (302, Some("data:text/plain,payload".to_owned())),
        (302, Some(format!("//{dl}/{SEG}"))),
        (302, Some(format!("https://:pw@{dl}/{SEG}"))),
        (302, Some("https://".to_owned())),
        (302, Some(String::new())),
        (303, Some(format!("https://{dl}/{SEG}"))),
        (308, Some(format!("https://{dl}/{SEG}"))),
        (300, Some(format!("https://{dl}/{SEG}"))),
    ];
    for (status, location) in refused {
        let before = transport.request_count();
        transport.script_next(Ok(redirect(status, location.as_deref())));
        let err = store.get(SEG).await.unwrap_err();
        assert!(permanent(&err), "{status} {location:?}: {err:?}");
        assert_eq!(transport.request_count(), before + 1, "{status} {location:?}: not followed");
    }

    // Upper-case scheme/host normalise to the same https origin: followed once,
    // without Authorization.
    let before = transport.request_count();
    transport.script_next(Ok(redirect(302, Some(&format!("HTTPS://{}/{SEG}", dl.to_uppercase())))));
    assert_eq!(store.get(SEG).await.unwrap(), b"payload");
    let requests = transport.requests();
    assert_eq!(requests.len(), before + 2);
    assert!(requests[before + 1].header("authorization").is_none());
    assert!(requests[before].header("authorization").is_some());
}

/// RT-ONE-2: `@odata.nextLink` must stay on https://graph.microsoft.com
/// (default port, no userinfo); userinfo tricks, look-alike hosts, relative
/// links and http are refused after one request; a longer page cycle is
/// detected.
#[tokio::test]
async fn onedrive_next_link_host_tricks_and_cycles() {
    let query = props(Some("vault-a"), Some(RemoteKind::Seg), Some("dev-a"));
    for link in [
        "https://graph.microsoft.com@evil.test/v1.0/me/drive/special/approot/children",
        "https://evil.test/v1.0/x?h=graph.microsoft.com",
        "https://graph.microsoft.com.evil.test/v1.0/x",
        "https://evil.test#graph.microsoft.com",
        "//graph.microsoft.com/v1.0/x",
        "/v1.0/me/drive/special/approot/children?$skiptoken=2",
        "http://graph.microsoft.com/v1.0/x",
        "https://graph.microsoft.com:444/v1.0/x",
        "https://user@graph.microsoft.com/v1.0/x",
        "ftp://graph.microsoft.com/v1.0/x",
        "",
    ] {
        let (store, transport, _) = ones::over(GraphServer::new());
        let body = serde_json::json!({ "value": [], "@odata.nextLink": link }).to_string();
        transport.script_next(Ok(json(200, &body)));
        let result = store.list(query).await;
        assert!(result.is_err(), "{link:?}: {result:?}");
        assert_eq!(transport.request_count(), 1, "{link:?}: never followed");
    }

    // A -> B -> A page cycle on Graph itself is refused, not looped.
    let first: Arc<Mutex<Option<String>>> = Arc::default();
    let seen = first.clone();
    let calls = Arc::new(AtomicUsize::new(0));
    let counter = calls.clone();
    let transport = MockTransport::new(move |req| {
        let n = counter.fetch_add(1, Ordering::SeqCst) + 1;
        if n > 50 {
            return json(500, "{}");
        }
        let mut first = seen.lock().unwrap();
        let start = first.get_or_insert_with(|| req.url.clone()).clone();
        let next = if req.url == start { "https://graph.microsoft.com/v1.0/me/drive/items/x/children?$skiptoken=B".to_owned() } else { start };
        json(200, &serde_json::json!({ "value": [], "@odata.nextLink": next }).to_string())
    });
    let store = super::provider::onedrive::OneDriveStore::new(transport, StaticToken::new(ones::TOKEN), ones::TENANT, ones::OBJECT)
        .unwrap();
    assert!(permanent(&store.list(query).await.unwrap_err()));
    assert_eq!(calls.load(Ordering::SeqCst), 2, "the repeat is detected on the second page");
}

/// RT-ONE-3: a 409 on a folder create is success only when the name is a
/// folder; a file squatting a folder name is Permanent; a 409 on a file
/// create is AlreadyExists and never overwrites; a replace onto a folder is
/// an error, never Ok.
#[tokio::test]
async fn onedrive_409_file_vs_folder() {
    let (store, transport, state) = ones::over(GraphServer::new());
    store.ensure_container().await.unwrap();
    store.ensure_container().await.unwrap();
    assert!(matches!(state.lock().unwrap().nodes.get("v1"), Some(ones::Node::Folder)));
    assert!(transport.requests().iter().filter(|r| r.method == "POST").all(|r| r.body.windows(4).any(|w| w == b"fail")));

    let (store, _transport, state2) = ones::over(GraphServer::new());
    state2.lock().unwrap().seed("v1", b"not a folder");
    assert!(permanent(&store.ensure_container().await.unwrap_err()));
    assert_eq!(state2.lock().unwrap().file("v1").as_deref(), Some(&b"not a folder"[..]));

    // A file squatting an intermediate folder of the key.
    let (store, _transport, state3) = ones::over(GraphServer::new());
    state3.lock().unwrap().seed("v1/vault-a/log", b"squatter");
    let err = store.put_create(SEG, OURS).await.unwrap_err();
    assert!(permanent(&err), "{err:?}");
    assert_eq!(state3.lock().unwrap().file("v1/vault-a/log").as_deref(), Some(&b"squatter"[..]));

    // File 409 → AlreadyExists, bytes untouched.
    let (store, _transport, state4) = ones::over(GraphServer::new());
    state4.lock().unwrap().seed(SEG, THEIRS);
    assert_eq!(store.put_create(SEG, OURS).await, Err(ProviderError::AlreadyExists));
    assert_eq!(state4.lock().unwrap().file(SEG).as_deref(), Some(THEIRS));

    // A folder occupying the key path: never Ok, never replaced.
    let (store, _transport, state6) = ones::over(GraphServer::new());
    state6.lock().unwrap().seed(&format!("{SEG}/inner.seg"), b"x");
    assert!(store.put_create(SEG, OURS).await.is_err());
    assert!(matches!(state6.lock().unwrap().nodes.get(SEG), Some(ones::Node::Folder)));
    let (store, _transport, state5) = ones::over(GraphServer::new());
    state5.lock().unwrap().seed(&format!("{REC}/child"), b"x");
    let err = store.put_replace(REC, b"rec", None).await.unwrap_err();
    assert!(permanent(&err), "{err:?}");
    assert!(matches!(state5.lock().unwrap().nodes.get(REC), Some(ones::Node::Folder)));
}

/// RT-ONE-4: the upload session URL. http, credentials, relative, missing or
/// non-string URLs send nothing further; an accepted session URL never
/// receives the bearer token. (Observation: any https host is accepted, per
/// g6-design §E; no Authorization goes there.)
#[tokio::test]
async fn onedrive_upload_session_url_checks() {
    let body = vec![7u8; SIMPLE_UPLOAD_MAX + 1];
    for upload_url in [
        serde_json::json!(format!("http://{}/s", ones::UPLOAD_HOST)),
        serde_json::json!(format!("https://u:p@{}/s", ones::UPLOAD_HOST)),
        serde_json::json!("/relative/session"),
        serde_json::json!(format!("ftp://{}/s", ones::UPLOAD_HOST)),
        serde_json::json!(""),
        serde_json::json!(42),
        serde_json::Value::Null,
    ] {
        let (store, transport, state) = ones::over(GraphServer::new());
        transport.script_next(Ok(json(200, &serde_json::json!({ "uploadUrl": upload_url }).to_string())));
        let err = store.put_create(SEG, &body).await.unwrap_err();
        assert!(permanent(&err), "{upload_url}: {err:?}");
        assert_eq!(transport.request_count(), 1, "{upload_url}: no chunk sent");
        assert!(state.lock().unwrap().file(SEG).is_none());
    }

    let (store, transport, _) = ones::over(GraphServer::new());
    transport.script_next(Ok(json(200, r#"{"uploadUrl":"https://elsewhere.example.test/session?sig=1"}"#)));
    let err = store.put_create(SEG, &body).await.unwrap_err();
    assert!(!matches!(err, ProviderError::Auth { .. }), "a refused session URL is never a reconsent: {err:?}");
    let requests = transport.requests();
    assert_eq!(requests[1].url, "https://elsewhere.example.test/session?sig=1");
    assert!(requests[1].header("authorization").is_none(), "the bearer token never leaves Graph");
    assert!(!format!("{err:?}").contains(ones::TOKEN));
}

// ===========================================================================
// Props translation on keyed stores (g6-design §C, ADV-3)
// ===========================================================================

const REC_DEV: &str = "v1/vault-a/devices/dev.rec";
const REC_DOT: &str = "v1/vault-a/devices/dev.a.rec";
const REC_DOT_REC: &str = "v1/vault-a/devices/dev.a.rec.rec";
const SEG_VAULT: &str = "v1/vault/log/dev-a/000000000001-000000000001.seg";
const SEG_VAULT_DOT: &str = "v1/vault.a/log/dev-a/000000000001-000000000001.seg";
const M_VAULT: &str = "v1/vault/manifest.json";
const SNAP_DOT: &str = "v1/vault-a/snap/dev-a/snap.1.snap";
const SEG_DEV_DOT: &str = "v1/vault-a/log/dev.a/000000000001-000000000001.seg";
const EDGE_KEYS: [&str; 8] = [REC_DEV, REC_DOT, REC_DOT_REC, SEG_VAULT, SEG_VAULT_DOT, M_VAULT, SNAP_DOT, SEG_DEV_DOT];

async fn props_edge_suite<P: ObjectStore>(p: &P, requests: impl Fn() -> usize, keyed_refs: bool) {
    for key in EDGE_KEYS {
        p.put_create(key, key.as_bytes()).await.unwrap();
    }
    let list = |vault_id, kind, device_id| async move {
        let mut keys: Vec<String> =
            p.list(ListQuery::Props { vault_id, kind, device_id }).await.unwrap().into_iter().map(|o| o.key).collect();
        keys.sort();
        keys
    };
    use RemoteKind::*;
    // Dotted device ids: `dev`, `dev.a` and `dev.a.rec` never bleed into each
    // other through the `devices/<d>.rec` prefix.
    assert_eq!(list(Some("vault-a"), Some(Rec), Some("dev")).await, [REC_DEV]);
    assert_eq!(list(Some("vault-a"), Some(Rec), Some("dev.a")).await, [REC_DOT]);
    assert_eq!(list(Some("vault-a"), Some(Rec), Some("dev.a.rec")).await, [REC_DOT_REC]);
    assert_eq!(list(Some("vault-a"), Some(Rec), None).await, [REC_DOT, REC_DOT_REC, REC_DEV]);
    assert!(list(Some("vault-a"), Some(Rec), Some("dev.a.rec.rec")).await.is_empty());
    // Vault ids that are prefixes / dotted variants of each other stay apart.
    assert_eq!(list(Some("vault"), None, None).await, [SEG_VAULT, M_VAULT]);
    assert_eq!(list(Some("vault.a"), None, None).await, [SEG_VAULT_DOT]);
    assert_eq!(list(None, Some(Seg), Some("dev-a")).await, [SEG_VAULT_DOT, SEG_VAULT]);
    assert_eq!(list(None, Some(Seg), Some("dev.a")).await, [SEG_DEV_DOT]);
    assert_eq!(list(Some("vault-a"), Some(Snap), Some("dev-a")).await, [SNAP_DOT]);
    assert_eq!(list(None, Some(Manifest), None).await, [M_VAULT]);
    assert!(list(Some("vault"), Some(Manifest), Some("dev-a")).await.is_empty());

    // Id length boundary: 64 is an id, 65 is refused before any request.
    let max = "v".repeat(64);
    assert!(list(Some(max.as_str()), Some(Seg), None).await.is_empty());
    let before = requests();
    let long = "v".repeat(65);
    for (vault_id, device_id) in [(Some(long.as_str()), None), (Some("vault-a"), Some(long.as_str())), (Some(".a"), None)] {
        let err = p.list(ListQuery::Props { vault_id, kind: None, device_id }).await.unwrap_err();
        assert!(permanent(&err), "{vault_id:?} {device_id:?}");
    }
    assert_eq!(requests(), before, "refused Props send nothing");

    // `a..b` is an id but no key can contain `..`: never a listing entry,
    // never a panic.
    match p.list(ListQuery::Props { vault_id: Some("a..b"), kind: None, device_id: None }).await {
        Ok(found) => assert!(found.is_empty(), "{found:?}"),
        Err(err) => assert!(permanent(&err), "{err:?}"),
    }

    if keyed_refs {
        // The default `delete_ref` is `delete(key)`: an unsafe ref is refused
        // before any request.
        let before = requests();
        for bad in ["../x", "v1/../v1/vault/manifest.json", "/v1/vault/manifest.json", ""] {
            assert!(permanent(&p.delete_ref(bad).await.unwrap_err()), "{bad}");
        }
        assert_eq!(requests(), before);
    }
    // A ref equal to the key removes exactly that object.
    p.delete_ref(REC_DOT).await.unwrap();
    assert_eq!(list(Some("vault-a"), Some(Rec), None).await, [REC_DOT_REC, REC_DEV]);
}

#[tokio::test]
async fn props_edges_fake() {
    let store = FakeStore::new(true);
    props_edge_suite(&store, || store.requests(), false).await;
}

#[tokio::test]
async fn props_edges_webdav() {
    let (store, transport) = super::provider::webdav::tests_support::emulated();
    props_edge_suite(&store, || transport.request_count(), true).await;
}

#[tokio::test]
async fn props_edges_r2() {
    let (store, transport) = super::provider::r2::tests_support::emulated();
    props_edge_suite(&store, || transport.request_count(), true).await;
}

#[tokio::test]
async fn props_edges_onedrive() {
    let (store, transport) = ones::emulated();
    props_edge_suite(&store, || transport.request_count(), true).await;
}

/// RT-PROPS-2: the Drive-like fake (`create_only == false`) lists every copy
/// of a duplicated name under Props, each with its own ref, and `delete_ref`
/// removes exactly one.
#[tokio::test]
async fn props_fake_duplicates_have_distinct_refs() {
    let store = FakeStore::new(false);
    store.put_create(SEG, OURS).await.unwrap();
    store.put_create(SEG, THEIRS).await.unwrap();
    let listed = store.list(props(Some("vault-a"), Some(RemoteKind::Seg), Some("dev-a"))).await.unwrap();
    assert_eq!(listed.len(), 2);
    assert_ne!(listed[0].provider_ref, listed[1].provider_ref);
    store.delete_ref(&listed[0].provider_ref).await.unwrap();
    assert_eq!(store.get_all(SEG).await.unwrap().len(), 1);
}

// ===========================================================================
// Pass-2 fixes (plan-amendments "G6 OAuth and adapter decisions"): the
// unchecked create the bridge uses on stores that are not create-only, and
// the shared 401 retry helper.
// ===========================================================================

fn drive_fp() -> String {
    binding_fp(&format!("gdrive|{}", drives::SUB))
}

fn drive_uploads(transport: &MockTransport) -> usize {
    transport.requests().iter().filter(|r| r.method == "POST" && r.url.contains("/upload/drive/v3/files")).count()
}

/// Seeds THEIRS as another writer, then drives every create path the bridge
/// and the store expose with OURS. `hide` makes the next `get_all` probe miss
/// THEIRS, so settle reaches the resend with the key taken. Nothing may
/// replace THEIRS.
async fn create_only_bridge_never_overwrites<P: ObjectStore>(store: P, hide: impl Fn(&P)) {
    assert!(store.create_only());
    store.put_create(SEG, THEIRS).await.unwrap();
    let fp = binding_fp(&store.identity());
    let remote = Bound::new(store, fp);
    assert_eq!(SegmentRemote::put_create(&remote, SEG, OURS).await, Ok(PutOutcome::AlreadyExists));
    assert_eq!(ObjectStore::put_create_unchecked(&remote, SEG, OURS).await, Err(ProviderError::AlreadyExists));
    assert_eq!(remote.inner().put_create_unchecked(SEG, OURS).await, Err(ProviderError::AlreadyExists));
    assert_eq!(settle_attempted(&remote, &NoPause, SEG, OURS, judge).await.outcome, Outcome::Rejected);
    for _ in 0..3 {
        hide(remote.inner());
    }
    let settled = settle_attempted(&remote, &NoPause, SEG, OURS, judge).await;
    assert_eq!(settled.outcome, Outcome::Rejected, "the resend collided and the final probe saw theirs");
    assert_eq!(remote.inner().get(SEG).await.unwrap(), THEIRS);
}

/// RT-UNCHK-1: on every create-only store (WebDAV with a verified probe, R2,
/// OneDrive, the create-only fake) the bridge keeps the checked create: a
/// resend onto another writer's key is AlreadyExists/Rejected and the stored
/// bytes never change, also when the probes missed the object.
#[tokio::test]
async fn create_only_stores_never_take_the_unchecked_path() {
    let (webdav, transport) = super::provider::webdav::tests_support::emulated();
    create_only_bridge_never_overwrites(webdav, move |_| transport.fail_next(404, &[])).await;
    let (r2, transport) = super::provider::r2::tests_support::emulated();
    create_only_bridge_never_overwrites(r2, move |_| transport.fail_next(404, &[])).await;
    let (onedrive, transport) = ones::emulated();
    create_only_bridge_never_overwrites(onedrive, move |_| transport.fail_next(404, &[])).await;
    create_only_bridge_never_overwrites(FakeStore::new(true), |fake: &FakeStore| {
        fake.fail_next(Op::Get, ProviderError::NotFound)
    })
    .await;
}

/// RT-UNCHK-2: a WebDAV store whose conditional create is not verified
/// reports `create_only() == false`, so the bridge picks the unchecked path;
/// its default is the checked `put_create`, which refuses: no PUT is ever
/// sent, the segment stays attempted, and the other writer's bytes stay.
#[tokio::test]
async fn unprobed_webdav_unchecked_resend_is_refused_without_a_put() {
    use super::provider::webdav::tests_support::{over, DavServer};
    let (store, transport, _) = over(DavServer::new());
    assert!(!store.create_only());
    store.put_replace(SEG, THEIRS, None).await.unwrap();
    let before = transport.request_count();
    let fp = binding_fp(&store.identity());
    let remote = Bound::new(store, fp);
    assert_eq!(SegmentRemote::put_create(&remote, SEG, OURS).await, Err(TransportError));
    let err = ObjectStore::put_create_unchecked(&remote, SEG, OURS).await.unwrap_err();
    assert!(matches!(err, ProviderError::Unsupported(_)), "{err:?}");
    let settled = settle_attempted(&remote, &NoPause, SEG, OURS, judge).await;
    assert_eq!(settled.outcome, Outcome::StillAttempted);
    let puts = transport.requests()[before..].iter().filter(|r| r.method == "PUT").count();
    assert_eq!(puts, 0, "no unconditional PUT reaches the server");
    assert_eq!(remote.inner().get(SEG).await.unwrap(), THEIRS);
}

/// RT-UNCHK-3: Drive resend idempotence. The first settle uploads ours once
/// next to theirs (PublishedWithFork); later settles see ours and never
/// upload again; ours already present before any settle means no upload.
#[tokio::test]
async fn drive_unchecked_resend_lands_once_and_never_repeats() {
    let (store, transport, server) = drives::emulated();
    server.lock().unwrap().seed_key(SEG, THEIRS);
    let remote = Bound::new(store, drive_fp());
    for round in 0..3 {
        let settled = settle_attempted(&remote, &NoPause, SEG, OURS, judge).await;
        assert_eq!(settled.outcome, Outcome::PublishedWithFork, "round {round}");
        assert!(settled.extra_self_copies.is_empty());
    }
    assert_eq!(drive_uploads(&transport), 1);
    assert_eq!(server.lock().unwrap().copies(SEG), [THEIRS.to_vec(), OURS.to_vec()]);

    let (store, transport, server) = drives::emulated();
    server.lock().unwrap().seed_key(SEG, OURS);
    let remote = Bound::new(store, drive_fp());
    assert_eq!(settle_attempted(&remote, &NoPause, SEG, OURS, judge).await.outcome, Outcome::Published);
    assert_eq!(drive_uploads(&transport), 0);
}

/// RT-UNCHK-4: an unknown Drive listing (5xx, timeout) never reaches the
/// unchecked upload: the segment stays attempted and nothing is written.
#[tokio::test]
async fn drive_unknown_listing_never_reaches_the_unchecked_upload() {
    let (store, transport, server) = drives::emulated();
    server.lock().unwrap().seed_key(SEG, THEIRS);
    let remote = Bound::new(store, drive_fp());
    transport.fail_next(500, &[]);
    assert_eq!(settle_attempted(&remote, &NoPause, SEG, OURS, judge).await.outcome, Outcome::StillAttempted);
    transport.script_next(Err(TransportFailure::Timeout));
    assert_eq!(settle_attempted(&remote, &NoPause, SEG, OURS, judge).await.outcome, Outcome::StillAttempted);
    assert_eq!(drive_uploads(&transport), 0);
    assert_eq!(server.lock().unwrap().copies(SEG).len(), 1);
}

/// Fails the first request after an upload POST once, like a listing lost
/// right after the resend landed.
#[derive(Clone)]
struct LoseListingAfterUpload {
    inner: MockTransport,
    armed: Arc<AtomicUsize>,
}

impl HttpTransport for LoseListingAfterUpload {
    async fn send(&self, req: HttpRequest) -> Result<HttpResponse, TransportFailure> {
        let upload = req.method == "POST" && req.url.contains("/upload/drive/v3/files");
        if !upload && self.armed.compare_exchange(1, 2, Ordering::SeqCst, Ordering::SeqCst).is_ok() {
            return Err(TransportFailure::Timeout);
        }
        let resp = self.inner.send(req).await;
        if upload {
            let _ = self.armed.compare_exchange(0, 1, Ordering::SeqCst, Ordering::SeqCst);
        }
        resp
    }
}

/// RT-UNCHK-5: the resend lands but the post-upload listing is lost (the
/// dropped re-list): this cycle stays attempted; the next cycle sees ours
/// next to theirs and reports the fork without a second upload.
#[tokio::test]
async fn drive_lost_listing_after_the_resend_settles_next_cycle_without_a_second_upload() {
    let server = Arc::new(Mutex::new(DriveServer::new()));
    server.lock().unwrap().seed_key(SEG, THEIRS);
    let shared = server.clone();
    let mock = MockTransport::new(move |req| shared.lock().unwrap().handle(req));
    let transport = LoseListingAfterUpload { inner: mock.clone(), armed: Arc::new(AtomicUsize::new(0)) };
    let store = GDriveStore::new(transport, StaticToken::new(drives::TOKEN), drives::SUB.to_owned()).unwrap();
    let remote = Bound::new(store, drive_fp());
    assert_eq!(settle_attempted(&remote, &NoPause, SEG, OURS, judge).await.outcome, Outcome::StillAttempted);
    assert_eq!(drive_uploads(&mock), 1);
    assert_eq!(settle_attempted(&remote, &NoPause, SEG, OURS, judge).await.outcome, Outcome::PublishedWithFork);
    assert_eq!(drive_uploads(&mock), 1, "no second upload");
    assert_eq!(server.lock().unwrap().copies(SEG).len(), 2);
}

/// RT-UNCHK-6: binding and size checks on the unchecked path. A wrong
/// binding refuses before any request on every route (store, bridge,
/// settle); an object at the size cap and keys that are unsafe or not layout
/// keys are refused before any request.
#[tokio::test]
async fn drive_unchecked_path_keeps_binding_size_and_key_checks() {
    let (store, transport, server) = drives::emulated();
    server.lock().unwrap().seed_key(SEG, THEIRS);
    let wrong = Bound::new(store, binding_fp("gdrive|someone-else"));
    assert_eq!(ObjectStore::put_create_unchecked(&wrong, SEG, OURS).await, Err(ProviderError::BindingMismatch));
    assert_eq!(SegmentRemote::put_create(&wrong, SEG, OURS).await, Err(TransportError));
    assert_eq!(settle_attempted(&wrong, &NoPause, SEG, OURS, judge).await.outcome, Outcome::StillAttempted);
    assert_eq!(transport.request_count(), 0, "a wrong binding sends nothing");

    let (store, transport, _) = drives::emulated();
    let remote = Bound::new(store, drive_fp());
    let at_cap = vec![0u8; MAX_OBJECT_BYTES];
    assert!(permanent(&ObjectStore::put_create_unchecked(&remote, SEG, &at_cap).await.unwrap_err()));
    assert!(permanent(&remote.inner().put_create_unchecked(SEG, &at_cap).await.unwrap_err()));
    assert_eq!(SegmentRemote::put_create(&remote, SEG, &at_cap).await, Err(TransportError));
    drop(at_cap);
    for key in ["../x", "", "/v1/vault-a/log/dev-a/000000000001-000000000003.seg", "v1/vault-a/notes/x.txt", "v1/vault-a/log/dev-a/x.txt"] {
        let err = remote.inner().put_create_unchecked(key, OURS).await.unwrap_err();
        assert!(permanent(&err), "{key}: {err:?}");
        assert_eq!(SegmentRemote::put_create(&remote, key, OURS).await, Err(TransportError), "{key}");
    }
    assert_eq!(transport.request_count(), 0, "every refusal is local");
}

/// RT-UNCHK-7: the Drive-like fake (`create_only == false`) through `Bound`:
/// one byte under the cap is accepted on the unchecked path, the cap is
/// refused without reaching the store, an identity change mid-session is a
/// binding mismatch with no request, and the bridge publishes ours next to
/// theirs exactly once.
#[tokio::test]
async fn drive_like_fake_unchecked_path_boundaries_and_fork() {
    let fake = FakeStore::new(false);
    fake.seed(SEG, THEIRS);
    let remote = Bound::new(fake, binding_fp("fake|default"));
    let under = vec![1u8; MAX_OBJECT_BYTES - 1];
    let other_key = "v1/vault-a/log/dev-b/000000000001-000000000001.seg";
    ObjectStore::put_create_unchecked(&remote, other_key, &under).await.unwrap();
    drop(under);
    let requests = remote.inner().requests();
    let at_cap = vec![1u8; MAX_OBJECT_BYTES];
    assert!(permanent(&ObjectStore::put_create_unchecked(&remote, other_key, &at_cap).await.unwrap_err()));
    drop(at_cap);
    assert_eq!(remote.inner().requests(), requests, "the cap is checked before the store");

    for round in 0..2 {
        let settled = settle_attempted(&remote, &NoPause, SEG, OURS, judge).await;
        assert_eq!(settled.outcome, Outcome::PublishedWithFork, "round {round}");
    }
    assert_eq!(remote.inner().copies(SEG), [THEIRS.to_vec(), OURS.to_vec()]);

    remote.inner().set_identity("fake|switched-account");
    let requests = remote.inner().requests();
    assert_eq!(ObjectStore::put_create_unchecked(&remote, SEG, OURS).await, Err(ProviderError::BindingMismatch));
    assert_eq!(SegmentRemote::put_create(&remote, SEG, OURS).await, Err(TransportError));
    assert_eq!(remote.inner().requests(), requests);
    assert_eq!(remote.inner().copies(SEG).len(), 2);
}

/// A token source with an observable log: hands out `tokens` in order,
/// moving on only after `invalidate`; after an invalidate, `refresh_error`
/// (if set) is what the refresh yields instead.
#[derive(Clone)]
struct LoggedToken(Arc<TokenLog>);

struct TokenLog {
    tokens: Vec<String>,
    index: AtomicUsize,
    invalidations: AtomicUsize,
    refresh_error: Option<ProviderError>,
}

impl LoggedToken {
    fn new(tokens: &[&str], refresh_error: Option<ProviderError>) -> Self {
        Self(Arc::new(TokenLog {
            tokens: tokens.iter().map(|t| (*t).to_owned()).collect(),
            index: AtomicUsize::new(0),
            invalidations: AtomicUsize::new(0),
            refresh_error,
        }))
    }

    fn invalidations(&self) -> usize {
        self.0.invalidations.load(Ordering::SeqCst)
    }
}

impl AccessToken for LoggedToken {
    async fn token(&self) -> Result<String, ProviderError> {
        let index = self.0.index.load(Ordering::SeqCst);
        if index > 0 {
            if let Some(err) = &self.0.refresh_error {
                return Err(err.clone());
            }
        }
        Ok(self.0.tokens[index.min(self.0.tokens.len() - 1)].clone())
    }

    fn invalidate(&self) {
        self.0.invalidations.fetch_add(1, Ordering::SeqCst);
        self.0.index.fetch_add(1, Ordering::SeqCst);
    }
}

const OLD_TOKEN: &str = "rt-old-ACCESS-SECRET";
const NEW_TOKEN: &str = "rt-new-ACCESS-SECRET";

fn bearers(transport: &MockTransport) -> Vec<Option<String>> {
    transport.requests().iter().map(|r| r.header("authorization").map(str::to_owned)).collect()
}

/// The shared 401 cases, for one adapter built by `make` over a transport
/// that answers 401 to everything unless a response is scripted.
async fn bearer_helper_cases<S: ObjectStore>(make: impl Fn(MockTransport, LoggedToken) -> S) {
    let unauthorized = || MockTransport::new(|_| HttpResponse { status: 401, headers: Vec::new(), body: Vec::new() });
    let old = Some(format!("Bearer {OLD_TOKEN}"));
    let new = Some(format!("Bearer {NEW_TOKEN}"));

    // 401, 401: reconsent after exactly one refresh and one resend, on reads
    // and writes alike.
    for write in [false, true] {
        let (transport, tokens) = (unauthorized(), LoggedToken::new(&[OLD_TOKEN, NEW_TOKEN], None));
        let store = make(transport.clone(), tokens.clone());
        let err = if write { store.put_create(SEG, OURS).await.unwrap_err() } else { store.get(SEG).await.unwrap_err() };
        assert_eq!(err, ProviderError::Auth { reconsent: true }, "write={write}");
        assert_eq!(bearers(&transport), [old.clone(), new.clone()], "write={write}");
        assert_eq!(tokens.invalidations(), 1);
        assert!(!format!("{err:?}").contains("ACCESS-SECRET"));
    }

    // A refresh that fails after the invalidate: its error is returned and
    // nothing is resent.
    let (transport, tokens) =
        (unauthorized(), LoggedToken::new(&[OLD_TOKEN], Some(ProviderError::Transient("refresh down".to_owned()))));
    let store = make(transport.clone(), tokens.clone());
    assert_eq!(store.get(SEG).await, Err(ProviderError::Transient("refresh down".to_owned())));
    assert_eq!(bearers(&transport), [old.clone()]);
    assert_eq!(tokens.invalidations(), 1);

    // 401 then a 5xx: the resend's own answer wins, no third request.
    let (transport, tokens) = (unauthorized(), LoggedToken::new(&[OLD_TOKEN, NEW_TOKEN], None));
    transport.fail_next(401, &[]);
    transport.fail_next(500, &[]);
    let store = make(transport.clone(), tokens.clone());
    assert!(matches!(store.get(SEG).await, Err(ProviderError::Transient(_))));
    assert_eq!(transport.request_count(), 2);
    assert_eq!(tokens.invalidations(), 1);

    // 401 then a transport failure: Transient, never reconsent, no third try.
    let (transport, tokens) = (unauthorized(), LoggedToken::new(&[OLD_TOKEN, NEW_TOKEN], None));
    transport.fail_next(401, &[]);
    transport.script_next(Err(TransportFailure::Timeout));
    let store = make(transport.clone(), tokens.clone());
    assert!(matches!(store.get(SEG).await, Err(ProviderError::Transient(_))));
    assert_eq!(transport.request_count(), 2);

    // A 403, a 5xx or a transport failure first: no invalidate, no resend.
    for first in [Ok(403), Ok(500), Err(TransportFailure::Timeout)] {
        let (transport, tokens) = (unauthorized(), LoggedToken::new(&[OLD_TOKEN, NEW_TOKEN], None));
        match first {
            Ok(status) => transport.fail_next(status, &[]),
            Err(failure) => transport.script_next(Err(failure)),
        }
        let store = make(transport.clone(), tokens.clone());
        let err = store.get(SEG).await.unwrap_err();
        assert_ne!(err, ProviderError::Auth { reconsent: true }, "{err:?}");
        assert_eq!(transport.request_count(), 1, "{err:?}");
        assert_eq!(tokens.invalidations(), 0);
    }
}

/// RT-401-1: the shared helper behaves the same in both adapters: one
/// invalidate and one resend per 401, reconsent on the second 401, a failed
/// refresh or a non-401 retry answer is returned as is, and nothing else
/// ever invalidates.
#[tokio::test]
async fn shared_401_helper_cases_on_drive_and_onedrive() {
    bearer_helper_cases(|transport, tokens| GDriveStore::new(transport, tokens, drives::SUB.to_owned()).unwrap()).await;
    bearer_helper_cases(|transport, tokens| {
        super::provider::onedrive::OneDriveStore::new(transport, tokens, ones::TENANT, ones::OBJECT).unwrap()
    })
    .await;
}

/// RT-401-2: a 401 on the Drive unchecked upload refreshes and resends the
/// same body once: exactly one copy of ours lands next to theirs, and every
/// later request carries the fresh token without another invalidate.
#[tokio::test]
async fn drive_401_on_the_unchecked_upload_lands_exactly_one_copy() {
    let mut server = DriveServer::new();
    server.accepted_tokens = vec![NEW_TOKEN.to_owned()];
    server.seed_key(SEG, THEIRS);
    let server = Arc::new(Mutex::new(server));
    let shared = server.clone();
    let transport = MockTransport::new(move |req| shared.lock().unwrap().handle(req));
    let tokens = LoggedToken::new(&[OLD_TOKEN, NEW_TOKEN], None);
    let store = GDriveStore::new(transport.clone(), tokens.clone(), drives::SUB.to_owned()).unwrap();
    let remote = Bound::new(store, drive_fp());
    assert_eq!(SegmentRemote::put_create(&remote, SEG, OURS).await, Ok(PutOutcome::Created));
    assert_eq!(drive_uploads(&transport), 2, "the refused upload and its one resend");
    assert_eq!(server.lock().unwrap().copies(SEG), [THEIRS.to_vec(), OURS.to_vec()]);
    assert_eq!(settle_attempted(&remote, &NoPause, SEG, OURS, judge).await.outcome, Outcome::PublishedWithFork);
    assert_eq!(tokens.invalidations(), 1);
    let bearers = bearers(&transport);
    assert_eq!(bearers.iter().filter(|b| b.as_deref() == Some(&format!("Bearer {OLD_TOKEN}"))).count(), 1);
}

/// RT-401-3: OneDrive. A 401 on the checked create refreshes once and the
/// resend still refuses the taken name (never an overwrite). A 401 from a
/// pre-authenticated URL (download link, upload session) is not the
/// account's token: no invalidate, no reconsent, and the token keeps working.
#[tokio::test]
async fn onedrive_401_refreshes_graph_only_and_never_overwrites() {
    let mut server = GraphServer::new();
    server.token = NEW_TOKEN.to_owned();
    server.seed(SEG, THEIRS);
    let (store, transport, state) = ones::over_with(server, &[OLD_TOKEN, NEW_TOKEN]);
    let fp = binding_fp(&store.identity());
    let remote = Bound::new(store, fp);
    assert_eq!(SegmentRemote::put_create(&remote, SEG, OURS).await, Ok(PutOutcome::AlreadyExists));
    assert_eq!(state.lock().unwrap().file(SEG), Some(THEIRS.to_vec()));
    let old = Some(format!("Bearer {OLD_TOKEN}"));
    assert_eq!(bearers(&transport).iter().filter(|b| **b == old).count(), 1);

    /// Answers 401 for the pre-authenticated hosts while `refuse` is set.
    #[derive(Clone)]
    struct RefusePreauth {
        inner: MockTransport,
        refuse: Arc<AtomicUsize>,
    }
    impl HttpTransport for RefusePreauth {
        async fn send(&self, req: HttpRequest) -> Result<HttpResponse, TransportFailure> {
            let preauth = req.url.contains(ones::DOWNLOAD_HOST) || req.url.contains(ones::UPLOAD_HOST);
            if preauth && self.refuse.load(Ordering::SeqCst) == 1 {
                return Ok(HttpResponse { status: 401, headers: Vec::new(), body: Vec::new() });
            }
            self.inner.send(req).await
        }
    }
    let (_, mock, state) = ones::over(GraphServer::new());
    state.lock().unwrap().seed(SEG, OURS);
    let refuse = Arc::new(AtomicUsize::new(1));
    let tokens = LoggedToken::new(&[ones::TOKEN, "never-issued"], None);
    let wrapped = RefusePreauth { inner: mock.clone(), refuse: refuse.clone() };
    let store = super::provider::onedrive::OneDriveStore::new(wrapped, tokens.clone(), ones::TENANT, ones::OBJECT).unwrap();
    let big = vec![9u8; SIMPLE_UPLOAD_MAX + 1];
    let read = store.get(SEG).await.unwrap_err();
    let write = store.put_create(REC, &big).await.unwrap_err();
    for err in [&read, &write] {
        assert!(matches!(err, ProviderError::Transient(_)), "{err:?}");
    }
    assert_eq!(tokens.invalidations(), 0, "a pre-authenticated 401 never invalidates");
    refuse.store(0, Ordering::SeqCst);
    assert_eq!(store.get(SEG).await.unwrap(), OURS);
    store.put_create(REC, &big).await.unwrap();
    assert_eq!(tokens.invalidations(), 0);
    let graph_bearers: Vec<_> = mock.requests().iter().filter_map(|r| r.header("authorization").map(str::to_owned)).collect();
    assert!(graph_bearers.iter().all(|b| *b == format!("Bearer {}", ones::TOKEN)), "{graph_bearers:?}");
}
