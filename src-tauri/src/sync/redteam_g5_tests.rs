//! G5 red team (ultragoal G005): adversarial tests for the provider trait,
//! the fake, WebDAV and R2/SigV4 (approved plan §4.2.3, §4.7, §4.9; AC 11,
//! 18, 29, 31; `_sync/g5-design.md`; A4), written against the contract rather
//! than the implementation. Everything runs over `MockTransport` emulators or
//! an in-process transport: no socket is ever opened.
//!
//! A real defect is reproduced as `#[ignore = "G5 blocker: ..."]`; every
//! other case is a normal test.

use std::collections::{BTreeMap, VecDeque};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, UNIX_EPOCH};

use super::provider::fake::FakeStore;
use super::provider::r2::tests_support::{self as r2s, S3Server};
use super::provider::r2::{R2Config, R2Store};
use super::provider::sigv4::{self, Credentials};
use super::provider::transport::{HttpRequest, HttpResponse, HttpTransport, MockTransport, TransportFailure};
use super::provider::webdav::tests_support::{self as davs, DavServer, Node};
use super::provider::webdav::{WebDavConfig, WebDavStore};
use super::provider::{
    binding_fp, check_prefix, is_remote_key, map_status, Bound, ListQuery, ListedObject, ObjectStore, ProviderError,
};
use super::publish::{settle_attempted, Outcome, Pause, PutOutcome, SegmentRemote, TransportError, Verdict};

const SEG: &str = "v1/vault-a/log/dev-a/000000000001-000000000003.seg";
const REC: &str = "v1/vault-a/devices/dev-a.rec";
const OURS: &[u8] = b"our sealed segment";
const THEIRS: &[u8] = b"their sealed segment";
const DAV_HOST: &str = "dav.example.test";
const DAV_ROOT_PATH: &str = "/remote.php/dav/files/alice/SayKnowKit/";

fn resp(status: u16, headers: &[(&str, &str)], body: &[u8]) -> HttpResponse {
    HttpResponse {
        status,
        headers: headers.iter().map(|(n, v)| ((*n).to_owned(), (*v).to_owned())).collect(),
        body: body.to_vec(),
    }
}

fn dav_root_url() -> String {
    format!("{}SayKnowKit/", davs::BASE)
}

/// An unprobed WebDAV store over an arbitrary handler.
fn dav(handler: impl Fn(&HttpRequest) -> HttpResponse + Send + Sync + 'static) -> (WebDavStore<MockTransport>, MockTransport) {
    let transport = MockTransport::new(handler);
    (WebDavStore::new(davs::config(), transport.clone()).unwrap(), transport)
}

/// An R2 store (real clock, so the handler never checks signatures).
fn r2(handler: impl Fn(&HttpRequest) -> HttpResponse + Send + Sync + 'static) -> (R2Store<MockTransport>, MockTransport) {
    let transport = MockTransport::new(handler);
    (R2Store::new(r2s::config(), transport.clone()).unwrap(), transport)
}

fn permanent<T: std::fmt::Debug>(result: &Result<T, ProviderError>) -> bool {
    matches!(result, Err(ProviderError::Permanent(_)))
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

// ---------------------------------------------------------------------------
// 1. Overwrite attempts (AC 18, §4.2.3, "어댑터는 덮어쓰기 폴백 금지")
// ---------------------------------------------------------------------------

#[tokio::test]
async fn fake_refuses_every_create_over_an_existing_key() {
    let store = FakeStore::new(true);
    store.seed(SEG, THEIRS);
    assert_eq!(store.put_create(SEG, OURS).await, Err(ProviderError::AlreadyExists));
    assert_eq!(store.put_create(SEG, THEIRS).await, Err(ProviderError::AlreadyExists), "same bytes are refused too");
    assert_eq!(store.put_replace(SEG, OURS, Some("\"stale\"")).await, Err(ProviderError::PreconditionFailed));
    assert_eq!(store.copies(SEG), vec![THEIRS.to_vec()]);

    let bound = Bound::new(store, binding_fp("fake|default"));
    assert_eq!(SegmentRemote::put_create(&bound, SEG, OURS).await, Ok(PutOutcome::AlreadyExists));
    let settled = settle_attempted(&bound, &NoPause, SEG, OURS, judge).await;
    assert_eq!(settled.outcome, Outcome::Rejected);
    assert_eq!(bound.inner().copies(SEG), vec![THEIRS.to_vec()], "never overwritten");
}

#[tokio::test]
#[should_panic(expected = "overwrite of create-only key")]
async fn fake_detector_fires_on_put_replace_of_a_create_only_key_never_fetched() {
    // Another device's segment; this caller never listed or fetched it.
    let store = FakeStore::new(true);
    store.seed(SEG, THEIRS);
    let _ = store.put_replace(SEG, OURS, None).await;
}

#[tokio::test]
#[should_panic(expected = "overwrite of create-only key")]
async fn fake_detector_fires_on_put_replace_even_with_the_current_etag() {
    let store = FakeStore::new(true);
    store.put_create(SEG, OURS).await.unwrap();
    let etag = store.list(ListQuery::Prefix(SEG)).await.unwrap()[0].etag.clone().unwrap();
    let _ = store.put_replace(SEG, THEIRS, Some(&etag)).await;
}

#[tokio::test]
#[should_panic(expected = "overwrite of create-only key")]
async fn fake_detector_fires_in_drive_mode_put_replace() {
    let store = FakeStore::new(false);
    store.put_create(SEG, OURS).await.unwrap();
    let _ = store.put_replace(SEG, THEIRS, None).await;
}

#[tokio::test]
async fn webdav_put_create_never_overwrites_through_any_path() {
    let (store, transport, state) = davs::over(DavServer::new());
    store.probe_conditional_create().await.unwrap();
    // Created through the 409 → MKCOL parents → retry path.
    store.put_create(SEG, THEIRS).await.unwrap();
    assert_eq!(store.put_create(SEG, OURS).await, Err(ProviderError::AlreadyExists));
    let file = |s: &Arc<Mutex<DavServer>>| s.lock().unwrap().file(&format!("SayKnowKit/{SEG}"));
    assert_eq!(file(&state), Some(THEIRS.to_vec()));

    // A collection sitting at the key: 405 → Permanent, the collection stays.
    let coll = "v1/vault-a/log/dev-a/coll.seg";
    state.lock().unwrap().nodes.insert(DavServer::path(&format!("SayKnowKit/{coll}")), Node::Collection);
    assert!(permanent(&store.put_create(coll, OURS).await));
    assert!(matches!(state.lock().unwrap().nodes.get(&DavServer::path(&format!("SayKnowKit/{coll}"))), Some(Node::Collection)));

    // A stale If-Match never writes.
    assert_eq!(store.put_replace(SEG, OURS, Some("\"stale\"")).await, Err(ProviderError::PreconditionFailed));
    assert_eq!(file(&state), Some(THEIRS.to_vec()));

    // Every create PUT carried If-None-Match: *.
    let seg_url = format!("{}{SEG}", dav_root_url());
    let creates: Vec<HttpRequest> =
        transport.requests().into_iter().filter(|r| r.method == "PUT" && r.header("if-match").is_none()).collect();
    assert!(creates.iter().any(|r| r.url == seg_url));
    assert!(creates.iter().all(|r| r.header("if-none-match") == Some("*")), "{creates:?}");

    // Through the bridge and the resolver.
    let id = store.identity();
    let bound = Bound::new(store, binding_fp(&id));
    assert_eq!(SegmentRemote::put_create(&bound, SEG, OURS).await, Ok(PutOutcome::AlreadyExists));
    assert_eq!(settle_attempted(&bound, &NoPause, SEG, OURS, judge).await.outcome, Outcome::Rejected);
    assert_eq!(file(&state), Some(THEIRS.to_vec()));
}

#[tokio::test]
async fn r2_put_create_never_overwrites_through_any_path() {
    let (store, transport, state) = r2s::over(S3Server::new());
    store.put_create(SEG, THEIRS).await.unwrap();
    assert_eq!(store.put_create(SEG, OURS).await, Err(ProviderError::AlreadyExists));
    assert_eq!(store.put_replace(SEG, OURS, Some("\"stale\"")).await, Err(ProviderError::PreconditionFailed));
    let stored = |s: &Arc<Mutex<S3Server>>| s.lock().unwrap().objects.get(&format!("sayknow/{SEG}")).map(|o| o.0.clone());
    assert_eq!(stored(&state), Some(THEIRS.to_vec()));
    let creates: Vec<HttpRequest> =
        transport.requests().into_iter().filter(|r| r.method == "PUT" && r.header("if-match").is_none()).collect();
    assert_eq!(creates.len(), 2);
    assert!(creates.iter().all(|r| r.header("if-none-match") == Some("*")));

    let id = store.identity();
    let bound = Bound::new(store, binding_fp(&id));
    assert_eq!(SegmentRemote::put_create(&bound, SEG, OURS).await, Ok(PutOutcome::AlreadyExists));
    assert_eq!(settle_attempted(&bound, &NoPause, SEG, OURS, judge).await.outcome, Outcome::Rejected);
    assert_eq!(stored(&state), Some(THEIRS.to_vec()));
}

/// put_replace(_, _, None) is an unconditional PUT on both HTTP adapters: the
/// adapter cannot tell a create-only key from `devices/<self>.rec`, so AC 18
/// for put_replace rests on the engine (G7) only using it for `.rec`, and on
/// the fake detector above failing any engine test that does otherwise.
#[tokio::test]
async fn put_replace_without_if_match_sends_no_condition_on_http_adapters() {
    let (dav_store, dav_t) = davs::emulated();
    dav_store.put_replace(REC, b"rec", None).await.unwrap();
    let (r2_store, r2_t) = r2s::emulated();
    r2_store.put_replace(REC, b"rec", None).await.unwrap();
    for req in dav_t.requests().into_iter().chain(r2_t.requests()).filter(|r| r.method == "PUT") {
        assert!(req.header("if-match").is_none() && req.header("if-none-match").is_none(), "{req:?}");
    }
}

// ---------------------------------------------------------------------------
// 2. WebDAV conditional-create probe (A4)
// ---------------------------------------------------------------------------

/// MKCOL 201, PUT answers `first` then `second`, DELETE 204.
fn probe_server(first: u16, second: u16) -> (WebDavStore<MockTransport>, MockTransport) {
    let puts = AtomicUsize::new(0);
    dav(move |req| match req.method.as_str() {
        "MKCOL" => resp(201, &[], b""),
        "PUT" => {
            let status = if puts.fetch_add(1, Ordering::SeqCst) == 0 { first } else { second };
            resp(status, &[("ETag", "\"p\"")], b"")
        }
        "DELETE" => resp(204, &[], b""),
        _ => resp(404, &[], b""),
    })
}

fn put_count(transport: &MockTransport) -> usize {
    transport.requests().iter().filter(|r| r.method == "PUT").count()
}

#[tokio::test]
async fn webdav_probe_refuses_servers_that_do_not_prove_atomic_create() {
    // (first PUT, second PUT, PUTs the probe may send)
    let cases = [(201, 201, 2), (201, 204, 2), (201, 200, 2), (201, 207, 2), (204, 412, 1), (200, 412, 1), (207, 412, 1)];
    for (first, second, puts) in cases {
        let (store, transport) = probe_server(first, second);
        let err = store.probe_conditional_create().await;
        assert!(matches!(err, Err(ProviderError::Unsupported(_))), "{first}/{second}: {err:?}");
        assert!(!store.create_only(), "{first}/{second}");
        assert_eq!(put_count(&transport), puts, "{first}/{second}");
        let before = put_count(&transport);
        assert!(matches!(store.put_create(SEG, OURS).await, Err(ProviderError::Unsupported(_))));
        assert_eq!(put_count(&transport), before, "a refused create sends no PUT");
    }
}

#[tokio::test]
async fn webdav_probe_412_on_the_first_put_never_unlocks_create() {
    let (store, transport) = probe_server(412, 412);
    let err = store.probe_conditional_create().await;
    // A fresh uuid name cannot be taken; the code reports Transient (retry at
    // connect time). What matters for AC 18: nothing is unlocked.
    assert!(matches!(err, Err(ProviderError::Transient(_))), "{err:?}");
    assert!(!store.create_only());
    assert_eq!(put_count(&transport), 1);
}

#[tokio::test]
async fn webdav_probe_redirects_and_errors_never_unlock_create() {
    for (first, second) in [(301, 412), (302, 412), (307, 412), (308, 412), (201, 301), (201, 307), (201, 500), (201, 401), (201, 409)] {
        let (store, transport) = probe_server(first, second);
        assert!(store.probe_conditional_create().await.is_err(), "{first}/{second}");
        assert!(!store.create_only(), "{first}/{second}");
        assert!(transport.requests().iter().all(|r| url::Url::parse(&r.url).unwrap().host_str() == Some(DAV_HOST)));
    }
}

#[tokio::test]
async fn webdav_probe_unlocks_only_on_201_then_412() {
    let (store, transport) = probe_server(201, 412);
    store.probe_conditional_create().await.unwrap();
    assert!(store.create_only());
    let reqs = transport.requests();
    assert!(reqs.iter().filter(|r| r.method == "PUT").all(|r| r.header("if-none-match") == Some("*")));
    assert!(reqs.last().is_some_and(|r| r.method == "DELETE" && r.url.contains("/SayKnowKit/.probe/")));
}

#[tokio::test]
async fn webdav_emulated_server_ignoring_if_none_match_is_refused() {
    let mut server = DavServer::new();
    server.honour_if_none_match = false;
    let (store, transport, state) = davs::over(server);
    assert!(matches!(store.probe_conditional_create().await, Err(ProviderError::Unsupported(_))));
    assert!(matches!(store.put_create(SEG, OURS).await, Err(ProviderError::Unsupported(_))));
    assert!(!transport.requests().iter().any(|r| r.url.ends_with(SEG)));
    assert_eq!(state.lock().unwrap().file(&format!("SayKnowKit/{SEG}")), None);
}

// ---------------------------------------------------------------------------
// 3. MKCOL 405 on a file, MKCOL 409 loops (AC 31)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn webdav_mkcol_405_on_a_file_is_permanent_on_every_level() {
    // The root itself is a file (servers name a file without a trailing slash).
    let file_self = multistatus(&[entry(DAV_ROOT_PATH.trim_end_matches('/'), false, Some("\"f\""), Some("4"))]);
    let (store, transport) = dav(move |req| match req.method.as_str() {
        "MKCOL" => resp(405, &[], b""),
        "PROPFIND" => resp(207, &[], &file_self),
        _ => resp(404, &[], b""),
    });
    assert!(permanent(&store.ensure_container().await));
    assert_eq!(transport.request_count(), 2);

    // An intermediate parent of a key is a file.
    let (store, _, state) = davs::over(DavServer::new());
    store.probe_conditional_create().await.unwrap();
    let v1 = DavServer::path("SayKnowKit/v1");
    state.lock().unwrap().nodes.insert(v1.clone(), Node::File { bytes: b"oops".to_vec(), etag: "\"x\"".into() });
    assert!(permanent(&store.put_create(SEG, OURS).await));
    assert!(matches!(state.lock().unwrap().nodes.get(&v1), Some(Node::File { bytes, .. }) if bytes == b"oops"));
}

/// Observation (non-blocking): an explicit non-collection `<D:resourcetype/>`
/// is overridden by a trailing `/` in the href, so a server that echoes the
/// requested `SayKnowKit/` URL for a file makes MKCOL 405 look like "exists".
/// No byte is overwritten: later PUTs under it fail. Reported, not gated.
#[tokio::test]
async fn observation_trailing_slash_href_overrides_an_explicit_file_resourcetype() {
    let file_self = multistatus(&[entry(DAV_ROOT_PATH, false, Some("\"f\""), Some("4"))]);
    let (store, _) = dav(move |req| match req.method.as_str() {
        "MKCOL" => resp(405, &[], b""),
        "PROPFIND" => resp(207, &[], &file_self),
        _ => resp(404, &[], b""),
    });
    assert_eq!(store.ensure_container().await, Ok(()), "documents the override; flip when resourcetype wins");
}

#[tokio::test]
async fn webdav_mkcol_405_with_an_empty_or_failed_propfind_is_never_success() {
    for (status, body) in [(207, multistatus(&[])), (404, Vec::new()), (500, Vec::new()), (200, Vec::new())] {
        let (store, _) = dav(move |req| match req.method.as_str() {
            "MKCOL" => resp(405, &[], b""),
            "PROPFIND" => resp(status, &[], &body),
            _ => resp(404, &[], b""),
        });
        assert!(store.ensure_container().await.is_err(), "PROPFIND {status}");
    }
}

/// A probe-capable server whose MKCOL and object PUT answers are scripted per URL.
fn mkcol_server(
    mkcol: impl Fn(&str) -> u16 + Send + Sync + 'static,
    put: impl Fn(&str) -> u16 + Send + Sync + 'static,
) -> (WebDavStore<MockTransport>, MockTransport) {
    let probe_puts = AtomicUsize::new(0);
    dav(move |req| match req.method.as_str() {
        "MKCOL" => resp(mkcol(&req.url), &[], b""),
        "PUT" if req.url.contains("/.probe/") => {
            resp(if probe_puts.fetch_add(1, Ordering::SeqCst) == 0 { 201 } else { 412 }, &[], b"")
        }
        "PUT" => resp(put(&req.url), &[], b""),
        "DELETE" => resp(204, &[], b""),
        _ => resp(404, &[], b""),
    })
}

#[tokio::test]
async fn webdav_mkcol_409_loops_are_bounded_and_never_success() {
    let root = dav_root_url();

    // MKCOL always 409: the base is missing → Permanent after one request.
    let (store, transport) = mkcol_server(|_| 409, |_| 201);
    assert!(permanent(&store.ensure_container().await));
    assert_eq!(transport.request_count(), 1);

    // Root creatable, every deeper MKCOL 409 forever, PUT 409 forever.
    let r = root.clone();
    let (store, transport) = mkcol_server(move |url| if url == r { 201 } else { 409 }, |_| 409);
    store.probe_conditional_create().await.unwrap();
    let before = transport.request_count();
    let err = store.put_create(SEG, OURS).await;
    assert!(matches!(err, Err(ProviderError::Transient(_))), "{err:?}");
    assert!(transport.request_count() - before <= 8, "bounded: {}", transport.request_count() - before);

    // MKCOL always succeeds but PUT keeps answering 409: one retry, then Permanent.
    let (store, transport) = mkcol_server(|_| 201, |_| 409);
    store.probe_conditional_create().await.unwrap();
    let before = transport.request_count();
    assert!(permanent(&store.put_create(SEG, OURS).await));
    assert_eq!(transport.requests()[before..].iter().filter(|r| r.method == "PUT").count(), 2);

    // put_replace takes the same bounded path.
    let r = root;
    let (store, transport) = mkcol_server(move |url| if url == r { 201 } else { 409 }, |_| 409);
    assert!(store.put_replace(REC, b"rec", None).await.is_err());
    assert!(transport.request_count() <= 8);
}

// ---------------------------------------------------------------------------
// 4. Malicious multistatus
// ---------------------------------------------------------------------------

fn entry(href: &str, collection: bool, etag: Option<&str>, size: Option<&str>) -> String {
    let mut props =
        String::from(if collection { "<D:resourcetype><D:collection/></D:resourcetype>" } else { "<D:resourcetype/>" });
    if let Some(etag) = etag {
        props.push_str(&format!("<D:getetag>{etag}</D:getetag>"));
    }
    if let Some(size) = size {
        props.push_str(&format!("<D:getcontentlength>{size}</D:getcontentlength>"));
    }
    format!(
        "<D:response><D:href>{href}</D:href><D:propstat><D:prop>{props}</D:prop><D:status>HTTP/1.1 200 OK</D:status></D:propstat></D:response>"
    )
}

fn multistatus(entries: &[String]) -> Vec<u8> {
    format!(r#"<?xml version="1.0" encoding="utf-8"?><D:multistatus xmlns:D="DAV:">{}</D:multistatus>"#, entries.concat())
        .into_bytes()
}

fn root_self() -> String {
    entry(DAV_ROOT_PATH, true, None, None)
}

fn file(rel: &str) -> String {
    entry(&format!("{DAV_ROOT_PATH}{rel}"), false, Some("\"e\""), Some("1"))
}

/// PROPFIND answers by URL from `bodies` (207), anything else 404.
fn listing(bodies: BTreeMap<String, Vec<u8>>) -> (WebDavStore<MockTransport>, MockTransport) {
    dav(move |req| match (req.method.as_str(), bodies.get(&req.url)) {
        ("PROPFIND", Some(body)) => resp(207, &[], body),
        _ => resp(404, &[], b""),
    })
}

async fn list_root(body: Vec<u8>) -> (Result<Vec<ListedObject>, ProviderError>, MockTransport) {
    let (store, transport) = listing(BTreeMap::from([(dav_root_url(), body)]));
    (store.list(ListQuery::Prefix("")).await, transport)
}

fn keys(listed: &[ListedObject]) -> Vec<&str> {
    listed.iter().map(|o| o.key.as_str()).collect()
}

#[tokio::test]
async fn multistatus_hrefs_outside_the_root_fail_closed() {
    for href in [
        "/remote.php/dav/files/bob/SayKnowKit/v1/x.seg".to_owned(),
        format!("{DAV_ROOT_PATH}../../bob/SayKnowKit/x.seg"),
        "https://evil.example/other/v1/x.seg".to_owned(),
        "/remote.php/dav/files/alice/SayKnowKitX/v1/x.seg".to_owned(),
        "/".to_owned(),
    ] {
        let (result, _) = list_root(multistatus(&[root_self(), entry(&href, false, Some("\"e\""), Some("1"))])).await;
        assert!(permanent(&result), "{href}: {result:?}");
    }
}

#[tokio::test]
async fn multistatus_encoded_dot_segments_never_yield_a_key_outside_the_root() {
    for tail in ["%2e%2e/%2e%2e/bob/x.seg", "%2E%2E/x.seg", "..%2F..%2Fbob%2Fx.seg", "v1/%2e%2e%2f%2e%2e%2fbob.seg", "v1/a%2Fb.seg"] {
        let (result, _) = list_root(multistatus(&[root_self(), file(tail)])).await;
        match result {
            Err(ProviderError::Permanent(_)) => {}
            Ok(listed) => {
                for key in keys(&listed) {
                    assert!(is_remote_key(key) && !key.contains("bob"), "{tail}: {key}");
                }
            }
            Err(other) => panic!("{tail}: {other:?}"),
        }
    }
    // A `..` that stays inside the root is normalized, not trusted verbatim.
    let (result, _) = list_root(multistatus(&[root_self(), file("v1/../inside.seg")])).await;
    assert_eq!(keys(&result.unwrap()), ["inside.seg"]);
}

#[tokio::test]
async fn multistatus_absolute_url_to_another_host_is_placed_by_path_only() {
    // Design: only the path is compared (proxies rewrite the authority). The
    // object is listed under the configured root and nothing is ever sent to
    // the other host.
    let href = format!("https://evil.example{DAV_ROOT_PATH}v1/x.seg");
    let (result, transport) = list_root(multistatus(&[root_self(), entry(&href, false, Some("\"e\""), Some("1"))])).await;
    assert_eq!(keys(&result.unwrap()), ["v1/x.seg"]);
    assert!(transport.requests().iter().all(|r| url::Url::parse(&r.url).unwrap().host_str() == Some(DAV_HOST)));
}

#[tokio::test]
async fn multistatus_missing_etag_duplicates_and_huge_sizes() {
    let body = multistatus(&[
        root_self(),
        entry(&format!("{DAV_ROOT_PATH}no-etag.seg"), false, None, Some("3")),
        entry(&format!("{DAV_ROOT_PATH}dup.seg"), false, Some("\"d1\""), Some("1")),
        entry(&format!("{DAV_ROOT_PATH}dup.seg"), false, Some("\"d2\""), Some("1")),
        entry(&format!("{DAV_ROOT_PATH}huge.seg"), false, Some("\"h\""), Some("184467440737095516160")),
        entry(&format!("{DAV_ROOT_PATH}neg.seg"), false, Some("\"n\""), Some("-1")),
        root_self(),
    ]);
    let listed = list_root(body).await.0.unwrap();
    assert_eq!(keys(&listed), ["dup.seg", "huge.seg", "neg.seg", "no-etag.seg"], "duplicates collapse to one key");
    let by_key: BTreeMap<&str, &ListedObject> = listed.iter().map(|o| (o.key.as_str(), o)).collect();
    assert_eq!(by_key["no-etag.seg"].etag, None);
    assert_eq!(by_key["no-etag.seg"].size, Some(3));
    assert_eq!(by_key["huge.seg"].size, None);
    assert_eq!(by_key["neg.seg"].size, None);
}

#[tokio::test]
async fn multistatus_huge_listing_is_complete() {
    let mut entries = vec![root_self()];
    entries.extend((0..20_000).map(|i| file(&format!("f{i:05}.seg"))));
    let listed = list_root(multistatus(&entries)).await.0.unwrap();
    assert_eq!(listed.len(), 20_000);
    assert!(listed.iter().all(|o| is_remote_key(&o.key)));
}

#[tokio::test]
async fn multistatus_malformed_bodies_fail_closed() {
    let valid = multistatus(&[root_self(), file("a.seg")]);
    let truncated = valid[..valid.len() / 2].to_vec();
    let entity_bomb = br#"<?xml version="1.0"?><!DOCTYPE D:multistatus [<!ENTITY x "SayKnowKit"><!ENTITY y "&x;&x;&x;&x;">]><D:multistatus xmlns:D="DAV:"><D:response><D:href>/remote.php/dav/files/alice/&y;/</D:href><D:propstat><D:prop><D:resourcetype><D:collection/></D:resourcetype></D:prop><D:status>HTTP/1.1 200 OK</D:status></D:propstat></D:response></D:multistatus>"#.to_vec();
    let wrong_ns = String::from_utf8(valid.clone()).unwrap().replace("xmlns:D=\"DAV:\"", "xmlns:D=\"urn:evil\"").into_bytes();
    for (name, body) in [
        ("truncated", truncated),
        ("entity", entity_bomb),
        ("not utf-8", vec![0xff, 0xfe, 0x00]),
        ("missing self", multistatus(&[file("a.seg")])),
        ("wrong namespace", wrong_ns),
        ("empty body", Vec::new()),
        ("html", b"<html><body>login</body></html>".to_vec()),
        ("error root", br#"<D:error xmlns:D="DAV:"><D:need-privileges/></D:error>"#.to_vec()),
    ] {
        let (result, _) = list_root(body).await;
        assert!(permanent(&result), "{name}: {result:?}");
    }
}

#[tokio::test]
async fn multistatus_collection_cycles_terminate() {
    let root = dav_root_url();
    let v1 = format!("{root}v1/");
    let v1a = format!("{root}v1/a/");
    let bodies = BTreeMap::from([
        (root.clone(), multistatus(&[root_self(), entry(&format!("{DAV_ROOT_PATH}v1/"), true, None, None)])),
        (
            v1.clone(),
            multistatus(&[
                entry(&format!("{DAV_ROOT_PATH}v1/"), true, None, None),
                root_self(),
                entry(&format!("{DAV_ROOT_PATH}v1/a/"), true, None, None),
            ]),
        ),
        (
            v1a,
            multistatus(&[
                entry(&format!("{DAV_ROOT_PATH}v1/a/"), true, None, None),
                entry(&format!("{DAV_ROOT_PATH}v1/"), true, None, None),
                root_self(),
                file("v1/a/k.seg"),
            ]),
        ),
    ]);
    let (store, transport) = listing(bodies);
    assert_eq!(keys(&store.list(ListQuery::Prefix("")).await.unwrap()), ["v1/a/k.seg"]);
    assert_eq!(transport.request_count(), 3);
}

// ---------------------------------------------------------------------------
// 5. Redirects 301/302/303/307/308: never followed, never success
// ---------------------------------------------------------------------------

const REDIRECTS: [u16; 5] = [301, 302, 303, 307, 308];
const EVIL: (&str, &str) = ("Location", "https://evil.example/steal");

async fn redirect_never_followed<P: ObjectStore>(store: &P, transport: &MockTransport, label: &str) {
    for code in REDIRECTS {
        let mut results: Vec<(&str, bool)> = Vec::new();
        transport.fail_next(code, &[EVIL]);
        results.push(("get", permanent(&store.get(SEG).await)));
        transport.fail_next(code, &[EVIL]);
        results.push(("put_create", permanent(&store.put_create(SEG, OURS).await)));
        transport.fail_next(code, &[EVIL]);
        results.push(("put_replace", permanent(&store.put_replace(REC, b"r", None).await)));
        transport.fail_next(code, &[EVIL]);
        results.push(("delete", permanent(&store.delete(SEG).await)));
        transport.fail_next(code, &[EVIL]);
        results.push(("list", permanent(&store.list(ListQuery::Prefix("")).await)));
        for (op, ok) in results {
            assert!(ok, "{label} {op} HTTP {code} must be Permanent");
        }
    }
    assert_eq!(transport.request_count(), REDIRECTS.len() * 5, "{label}: exactly one request per op, no follow-up");
    assert!(transport.requests().iter().all(|r| !r.url.contains("evil.example")));
}

#[tokio::test]
async fn webdav_redirects_are_never_followed() {
    let (store, transport) = davs::emulated();
    redirect_never_followed(&store, &transport, "webdav").await;
    for code in REDIRECTS {
        transport.fail_next(code, &[EVIL]);
        assert!(permanent(&store.ensure_container().await), "MKCOL {code}");
        transport.fail_next(code, &[EVIL]);
        assert!(permanent(&store.quota().await), "quota {code}");
    }
}

#[tokio::test]
async fn r2_redirects_are_never_followed() {
    let (store, transport) = r2s::emulated();
    redirect_never_followed(&store, &transport, "r2").await;
}

// ---------------------------------------------------------------------------
// 6. WebDAV base URL: HTTP, userinfo, query, fragment (D10)
// ---------------------------------------------------------------------------

#[test]
fn webdav_base_url_refuses_http_userinfo_and_extras_without_echoing_secrets() {
    let secret = "pw-SECRET-7f3a";
    let bad = [
        "http://dav.example.test/".to_owned(),
        "HTTP://dav.example.test/".to_owned(),
        " http://dav.example.test/ ".to_owned(),
        format!("https://user:{secret}@dav.example.test/"),
        format!("https://:{secret}@dav.example.test/"),
        "https://user@dav.example.test/".to_owned(),
        format!("https://dav.example.test/?token={secret}"),
        format!("https://dav.example.test/#{secret}"),
        format!("http://user:{secret}@dav.example.test/"),
        "https://".to_owned(),
        String::new(),
        "file:///etc/passwd".to_owned(),
        "javascript:alert(1)".to_owned(),
        "davs://dav.example.test/".to_owned(),
        "webdav://dav.example.test/".to_owned(),
        format!("ftp://u:{secret}@dav.example.test/"),
    ];
    for base_url in bad {
        let config = WebDavConfig { base_url: base_url.clone(), username: "alice".into(), password: secret.into() };
        let err = WebDavStore::new(config, MockTransport::new(|_| panic!("no request may be sent"))).err();
        let shown = format!("{err:?}");
        assert!(matches!(err, Some(ProviderError::Permanent(_))), "{base_url:?}: {shown}");
        assert!(!shown.contains(secret), "{shown}");
    }
}

// ---------------------------------------------------------------------------
// 7. R2: keys at the safety boundary, pagination, 409 vs 412, 200 + error body
// ---------------------------------------------------------------------------

const VALID_KEYS: [&str; 9] = ["a", "A-Z_0.9", ".probe/x", "-", "_", "a.", ".a", "a/b/c/d/e/f/g/h", "v1/vault-a/x.seg"];
const INVALID_KEYS: [&str; 36] = [
    "", "/", "a/", "/a", "a//b", ".", "..", "a/..", "../a", "a/../b", "a/./b", "...", "a..b", "a b", "a+b", "a%20b",
    "a%2Fb", "a?x=1", "a#f", "a;b", "a&b", "a=b", "a:b", "a@b", "a~b", "a\\b", "a\nb", "a\rb", "a\0b", "a\tb", "ä",
    "a\u{FF0E}\u{FF0E}/b", "a\u{2215}b", "\u{0130}", "a/\u{200B}", "https://evil.example/x",
];

#[tokio::test]
async fn r2_keys_at_the_safety_boundary() {
    let (store, transport) = r2(|_| resp(404, &[], b"<Error><Code>NoSuchKey</Code></Error>"));
    for key in INVALID_KEYS {
        assert!(permanent(&store.get(key).await), "get {key:?}");
        assert!(permanent(&store.put_create(key, b"x").await), "put_create {key:?}");
        assert!(permanent(&store.put_replace(key, b"x", Some("\"e\"")).await), "put_replace {key:?}");
        assert!(permanent(&store.delete(key).await), "delete {key:?}");
    }
    assert_eq!(transport.request_count(), 0);
    let long = "k".repeat(1100);
    for key in VALID_KEYS.iter().copied().chain([long.as_str()]) {
        assert_eq!(store.get(key).await, Err(ProviderError::NotFound), "{key}");
        let sent = transport.requests().pop().unwrap();
        let url = url::Url::parse(&sent.url).unwrap();
        assert_eq!(url.host_str(), Some(format!("{}.r2.cloudflarestorage.com", r2s::ACCOUNT).as_str()));
        assert_eq!(url.path(), format!("/{}/sayknow/{key}", r2s::BUCKET), "{key}");
        assert_eq!(url.query(), None);
    }
}

fn list_page(objects: &[&str], truncated: bool, token: Option<&str>) -> Vec<u8> {
    let mut xml = String::from(r#"<?xml version="1.0" encoding="UTF-8"?><ListBucketResult xmlns="http://s3.amazonaws.com/doc/2006-03-01/">"#);
    for key in objects {
        xml.push_str(&format!("<Contents><Key>{key}</Key><Size>1</Size><ETag>\"e\"</ETag></Contents>"));
    }
    xml.push_str(&format!("<IsTruncated>{truncated}</IsTruncated>"));
    if let Some(token) = token {
        xml.push_str(&format!("<NextContinuationToken>{token}</NextContinuationToken>"));
    }
    xml.push_str("</ListBucketResult>");
    xml.into_bytes()
}

#[tokio::test]
async fn r2_pagination_without_progress_fails_closed() {
    // The same continuation token comes back.
    let (store, transport) = r2(|_| resp(200, &[], &list_page(&["sayknow/v1/a.seg"], true, Some("T1"))));
    assert!(permanent(&store.list(ListQuery::Prefix("")).await));
    assert_eq!(transport.request_count(), 2);

    // Truncated without a token, and with an empty token.
    for page in [list_page(&["sayknow/v1/a.seg"], true, None), list_page(&["sayknow/v1/a.seg"], true, Some(""))] {
        let (store, transport) = r2(move |_| resp(200, &[], &page));
        assert!(permanent(&store.list(ListQuery::Prefix("")).await));
        assert_eq!(transport.request_count(), 1);
    }

    // Truncated XML on a later page fails the whole listing.
    let (store, _) = r2(|req| {
        if req.url.contains("continuation-token=") {
            let full = list_page(&["sayknow/v1/b.seg"], false, None);
            resp(200, &[], &full[..full.len() - 30])
        } else {
            resp(200, &[], &list_page(&["sayknow/v1/a.seg"], true, Some("T1")))
        }
    });
    assert!(permanent(&store.list(ListQuery::Prefix("")).await));

    // A foreign key outside `sayknow/` is refused, not silently re-rooted.
    let (store, _) = r2(|_| resp(200, &[], &list_page(&["other/v1/a.seg"], false, None)));
    assert!(permanent(&store.list(ListQuery::Prefix("")).await));
}

/// A transport that answers every list call with an empty truncated page whose
/// token alternates A, B, A, ... (a 2-cycle). No requests are recorded, so the
/// MAX_LIST_PAGES bound can be exercised cheaply.
struct CyclingTokens {
    calls: Arc<AtomicUsize>,
}

impl HttpTransport for CyclingTokens {
    async fn send(&self, _req: HttpRequest) -> Result<HttpResponse, TransportFailure> {
        let n = self.calls.fetch_add(1, Ordering::SeqCst);
        let token = if n % 2 == 0 { "A" } else { "B" };
        Ok(resp(200, &[], &list_page(&[], true, Some(token))))
    }
}

#[tokio::test]
async fn r2_pagination_token_cycle_terminates_with_an_error() {
    let calls = Arc::new(AtomicUsize::new(0));
    let store = R2Store::new(r2s::config(), CyclingTokens { calls: Arc::clone(&calls) }).unwrap();
    assert!(permanent(&store.list(ListQuery::Prefix("")).await));
    // Only consecutive repeats are detected: a 2-cycle runs to the page cap.
    assert_eq!(calls.load(Ordering::SeqCst), 100_000);
}

#[tokio::test]
async fn r2_409_and_412_are_distinct_and_never_success() {
    let (store, transport) = r2s::emulated();
    transport.fail_next(412, &[]);
    assert_eq!(store.put_create(SEG, OURS).await, Err(ProviderError::AlreadyExists));
    transport.fail_next(409, &[]);
    assert!(matches!(store.put_create(SEG, OURS).await, Err(ProviderError::Transient(_))));
    transport.fail_next(412, &[]);
    assert_eq!(store.put_replace(REC, b"r", Some("\"e\"")).await, Err(ProviderError::PreconditionFailed));
    transport.fail_next(409, &[]);
    assert!(matches!(store.put_replace(REC, b"r", Some("\"e\"")).await, Err(ProviderError::Transient(_))));
    for _ in 0..3 {
        transport.fail_next(409, &[]);
    }
    assert!(permanent(&store.get(SEG).await));
    assert!(permanent(&store.delete(SEG).await));
    assert!(permanent(&store.list(ListQuery::Prefix("")).await));

    // Through the bridge: 409 is unknown, 412 is "taken"; neither is Created.
    let id = store.identity();
    let bound = Bound::new(store, binding_fp(&id));
    transport.fail_next(409, &[]);
    assert_eq!(SegmentRemote::put_create(&bound, SEG, OURS).await, Err(TransportError));
    transport.fail_next(412, &[]);
    assert_eq!(SegmentRemote::put_create(&bound, SEG, OURS).await, Ok(PutOutcome::AlreadyExists));
}

#[tokio::test]
async fn r2_list_with_a_valid_empty_result_is_empty() {
    let (store, _) = r2(|_| resp(200, &[], &list_page(&[], false, None)));
    assert_eq!(store.list(ListQuery::Prefix("")).await, Ok(Vec::new()));
}

#[tokio::test]
async fn r2_list_200_with_an_error_or_empty_body_must_not_be_an_empty_listing() {
    let bodies: [(&str, &[u8]); 5] = [
        ("empty body", b""),
        ("xml declaration only", br#"<?xml version="1.0" encoding="UTF-8"?>"#),
        ("S3 error document", br#"<?xml version="1.0" encoding="UTF-8"?><Error><Code>InternalError</Code><Message>We encountered an internal error.</Message></Error>"#),
        ("HTML proxy page", b"<html><body><p>Sign in to continue</p></body></html>"),
        ("other root", br#"<ListAllMyBucketsResult><Buckets/></ListAllMyBucketsResult>"#),
    ];
    let mut failures = Vec::new();
    for (name, body) in bodies {
        let body = body.to_vec();
        let (store, _) = r2(move |_| resp(200, &[], &body));
        let result = store.list(ListQuery::Prefix("")).await;
        if result.is_ok() {
            failures.push(format!("{name}: {result:?}"));
        }
    }
    assert!(failures.is_empty(), "a non-ListBucketResult 200 was taken as a listing: {failures:#?}");
}

// ---------------------------------------------------------------------------
// 8. SigV4 header trimming (AWS get-header-value-trim / get-header-key-duplicate)
// ---------------------------------------------------------------------------

fn sigv4_auth(headers: &[(&str, &str)]) -> String {
    let mut req = HttpRequest {
        method: "GET".to_owned(),
        url: "https://example.amazonaws.com/".to_owned(),
        headers: headers.iter().map(|(n, v)| ((*n).to_owned(), (*v).to_owned())).collect(),
        body: Vec::new(),
    };
    let creds = Credentials {
        access_key: "AKIDEXAMPLE".to_owned(),
        secret_key: "wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY".to_owned(),
    };
    let now = UNIX_EPOCH + Duration::from_secs(1_440_938_160);
    sigv4::sign(&mut req, &creds, "us-east-1", "service", now, &sigv4::sha256_hex(b"")).unwrap();
    req.header("authorization").unwrap().to_owned()
}

#[test]
fn sigv4_trims_and_collapses_header_values_like_the_aws_suite() {
    // Canonical form from the AWS suite: `my-header1:value1`, `my-header2:"a b c"`.
    let canonical = sigv4_auth(&[("my-header1", "value1"), ("my-header2", "\"a b c\"")]);
    for variant in [
        [("My-Header1", " value1"), ("My-Header2", " \"a   b   c\"")],
        [("MY-HEADER1", "value1   "), ("my-header2", "\"a b c\"   ")],
        [("My-Header1", "\tvalue1\t"), ("My-Header2", "\t\"a \t b\t\tc\"\t")],
        [("my-header1", "  value1  "), ("my-header2", "\"a  b c\"")],
    ] {
        assert_eq!(sigv4_auth(&variant), canonical, "{variant:?}");
    }
    // Header order and name case never matter; SignedHeaders is lowercase and sorted.
    assert_eq!(sigv4_auth(&[("My-Header2", "\"a b c\""), ("My-Header1", "value1")]), canonical);
    assert!(canonical.contains("SignedHeaders=host;my-header1;my-header2;x-amz-content-sha256;x-amz-date,"), "{canonical}");
    // Whitespace-only and empty values are the same canonical empty value.
    assert_eq!(sigv4_auth(&[("x-empty", "   ")]), sigv4_auth(&[("x-empty", "")]));
    // Real differences still change the signature.
    assert_ne!(sigv4_auth(&[("my-header1", "value 1")]), sigv4_auth(&[("my-header1", "value1")]));
    assert_ne!(sigv4_auth(&[("my-header1", "value1")]), sigv4_auth(&[("my-header1", "value2")]));
}

#[test]
fn sigv4_joins_duplicate_headers_in_order_like_the_aws_suite() {
    // get-header-key-duplicate: three My-Header1 lines → `my-header1:value2,value2,value1`.
    let split = sigv4_auth(&[("My-Header1", "value2"), ("My-Header1", "value2"), ("my-header1", " value1 ")]);
    assert_eq!(split, sigv4_auth(&[("my-header1", "value2,value2,value1")]));
    assert_ne!(split, sigv4_auth(&[("my-header1", "value1,value2,value2")]), "value order is significant");
}

#[test]
fn sigv4_resigning_with_a_spoofed_signature_header_replaces_it() {
    let clean = sigv4_auth(&[]);
    let spoofed = sigv4_auth(&[("authorization", "AWS4-HMAC-SHA256 forged"), ("X-Amz-Date", "19990101T000000Z")]);
    assert_eq!(spoofed, clean);
}

// ---------------------------------------------------------------------------
// 9. Key safety fuzz
// ---------------------------------------------------------------------------

/// xorshift64*: deterministic, dependency-free.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x.wrapping_mul(0x2545_f491_4f6c_dd1d)
    }

    fn pick<'a>(&mut self, items: &[&'a str]) -> &'a str {
        items[(self.next() % items.len() as u64) as usize]
    }
}

/// The contract, restated independently: `/`-joined non-empty segments of
/// `[A-Za-z0-9._-]`, never `.`, no `..` anywhere.
fn oracle(key: &str) -> bool {
    !key.is_empty()
        && !key.contains("..")
        && key
            .split('/')
            .all(|s| !s.is_empty() && s != "." && s.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-')))
}

fn fuzz_keys(count: usize) -> Vec<String> {
    const SAFE: [&str; 8] = ["a", "Z", "0", ".", "-", "_", "/", "v1"];
    const NASTY: [&str; 31] = [
        "a", "Z", "0", ".", "..", "-", "_", "/", "//", "%", "%2e", "%2F", "\\", "?", "#", " ", "\t", "\n", "\0", "~",
        "+", "&", "=", ";", ":", "@", "ä", "\u{FF0E}", "\u{2215}", "\u{0130}", "\u{200B}",
    ];
    let mut rng = Rng(0x9e37_79b9_7f4a_7c15);
    (0..count)
        .map(|i| {
            let alphabet: &[&str] = if i % 2 == 0 { &SAFE } else { &NASTY };
            let len = 1 + (rng.next() % 10) as usize;
            (0..len).map(|_| rng.pick(alphabet)).collect()
        })
        .collect()
}

#[test]
fn key_and_prefix_safety_match_the_contract_under_fuzz() {
    let mut accepted = 0;
    for key in fuzz_keys(20_000) {
        assert_eq!(is_remote_key(&key), oracle(&key), "{key:?}");
        accepted += usize::from(oracle(&key));
        let prefix_ok = key.is_empty() || oracle(key.strip_suffix('/').unwrap_or(&key));
        assert_eq!(check_prefix(&key).is_ok(), prefix_ok, "prefix {key:?}");
    }
    assert!(accepted > 1_000, "the fuzz must reach the accepted side too ({accepted})");
}

#[tokio::test]
async fn unsafe_keys_send_nothing_and_safe_keys_stay_under_the_root_on_every_adapter() {
    let (dav_store, dav_t) = dav(|_| resp(404, &[], b""));
    let (r2_store, r2_t) = r2(|_| resp(404, &[], b""));
    let fake = FakeStore::new(true);
    let r2_host = format!("{}.r2.cloudflarestorage.com", r2s::ACCOUNT);
    for key in fuzz_keys(4_000).iter().map(String::as_str).chain(INVALID_KEYS).chain(VALID_KEYS) {
        let (dav_before, r2_before, fake_before) = (dav_t.request_count(), r2_t.request_count(), fake.requests());
        if !oracle(key) {
            assert!(permanent(&dav_store.get(key).await) && permanent(&r2_store.get(key).await) && permanent(&fake.get(key).await));
            assert!(permanent(&dav_store.put_create(key, b"x").await) && permanent(&r2_store.put_create(key, b"x").await));
            assert!(permanent(&fake.put_create(key, b"x").await));
            assert!(permanent(&dav_store.put_replace(key, b"x", None).await) && permanent(&r2_store.put_replace(key, b"x", None).await));
            assert!(permanent(&fake.put_replace(key, b"x", None).await));
            assert!(permanent(&dav_store.delete(key).await) && permanent(&r2_store.delete(key).await) && permanent(&fake.delete(key).await));
            assert!(permanent(&dav_store.list(ListQuery::Prefix(&format!("{key}/"))).await), "list {key:?}");
            assert_eq!((dav_t.request_count(), r2_t.request_count(), fake.requests()), (dav_before, r2_before, fake_before), "{key:?}");
            continue;
        }
        assert_eq!(dav_store.get(key).await, Err(ProviderError::NotFound), "{key}");
        assert_eq!(r2_store.get(key).await, Err(ProviderError::NotFound), "{key}");
        let dav_url = url::Url::parse(&dav_t.requests().pop().unwrap().url).unwrap();
        assert_eq!(dav_url.host_str(), Some(DAV_HOST));
        assert_eq!(dav_url.path(), format!("{DAV_ROOT_PATH}{key}"), "{key}");
        let r2_url = url::Url::parse(&r2_t.requests().pop().unwrap().url).unwrap();
        assert_eq!(r2_url.host_str(), Some(r2_host.as_str()));
        assert_eq!(r2_url.path(), format!("/{}/sayknow/{key}", r2s::BUCKET), "{key}");
    }
}

// ---------------------------------------------------------------------------
// 10. Bound: mismatch on every op sends nothing (AC 11)
// ---------------------------------------------------------------------------

async fn assert_bound_refuses<P: ObjectStore>(bound: &Bound<P>, requests: impl Fn() -> usize, label: &str) {
    let before = requests();
    let mm = ProviderError::BindingMismatch;
    assert_eq!(bound.list(ListQuery::Prefix("")).await, Err(mm.clone()), "{label}");
    assert_eq!(bound.get(SEG).await, Err(mm.clone()), "{label}");
    assert_eq!(ObjectStore::put_create(bound, SEG, OURS).await.map(drop), Err(mm.clone()), "{label}");
    assert_eq!(bound.put_replace(REC, b"r", None).await.map(drop), Err(mm.clone()), "{label}");
    assert_eq!(bound.put_replace(REC, b"r", Some("\"e\"")).await.map(drop), Err(mm.clone()), "{label}");
    assert_eq!(bound.delete(SEG).await, Err(mm.clone()), "{label}");
    assert_eq!(bound.quota().await.map(drop), Err(mm.clone()), "{label}");
    assert_eq!(bound.ensure_container().await, Err(mm.clone()), "{label}");
    // Unsafe keys too: the mismatch wins and still nothing is sent.
    assert_eq!(bound.get("../x").await, Err(mm), "{label}");
    assert_eq!(SegmentRemote::get_all(bound, SEG).await, Err(TransportError), "{label}");
    assert_eq!(SegmentRemote::put_create(bound, SEG, OURS).await, Err(TransportError), "{label}");
    assert_eq!(settle_attempted(bound, &NoPause, SEG, OURS, judge).await.outcome, Outcome::StillAttempted, "{label}");
    assert_eq!(requests(), before, "{label}: a mismatched binding must send nothing");
}

#[tokio::test]
async fn bound_mismatch_refuses_every_op_on_every_backend() {
    let right = binding_fp(&davs::emulated().0.identity());
    let wrong_fps = [
        binding_fp("webdav|https://dav.example.test/remote.php/dav/files/bob/|alice"),
        binding_fp("webdav|https://dav.example.test/remote.php/dav/files/alice/|bob"),
        right.to_uppercase(),
        format!("{right}\n"),
        String::new(),
    ];
    for (i, fp) in wrong_fps.into_iter().enumerate() {
        let (dav_store, dav_t) = davs::emulated();
        let bound = Bound::new(dav_store, fp);
        assert_bound_refuses(&bound, || dav_t.request_count(), &format!("webdav #{i}")).await;
        assert_eq!(dav_t.request_count(), 0);
    }

    let (r2_store, r2_t) = r2s::emulated();
    let other_bucket = format!("r2|{}|other-bucket|https://{}.r2.cloudflarestorage.com", r2s::ACCOUNT, r2s::ACCOUNT);
    let bound = Bound::new(r2_store, binding_fp(&other_bucket));
    assert_bound_refuses(&bound, || r2_t.request_count(), "r2").await;
    assert_eq!(r2_t.request_count(), 0);

    let fake = FakeStore::new(true);
    fake.set_identity("fake|someone-else");
    let bound = Bound::new(fake, binding_fp("fake|default"));
    assert_bound_refuses(&bound, || bound.inner().requests(), "fake").await;
}

#[tokio::test]
async fn bound_accepts_the_same_identity_however_the_url_was_typed() {
    let typed = WebDavConfig { base_url: "HTTPS://DAV.Example.TEST:443/remote.php/dav/files/alice".into(), ..davs::config() };
    let (store, transport) = davs::emulated();
    let bound = Bound::new(store, binding_fp(&WebDavStore::new(typed, MockTransport::new(|_| unreachable!())).unwrap().identity()));
    bound.ensure_container().await.unwrap();
    assert!(transport.request_count() > 0);
}

/// Observation (non-blocking): the WebDAV identity text is not injective when a
/// username contains `|` and the base path contains `/|`, because neither part
/// is escaped. Two different (URL, user) pairs then share one fingerprint and
/// one `sync:cred:webdav:<fp>` slot. Requires a `|` in a username; reported,
/// not gated.
#[test]
fn observation_webdav_identity_pipe_ambiguity() {
    let a = WebDavConfig { base_url: "https://dav.example.test/a/|b/".into(), username: "u".into(), password: "p".into() };
    let b = WebDavConfig { base_url: "https://dav.example.test/a/".into(), username: "b/|u".into(), password: "p".into() };
    let id = |c: WebDavConfig| WebDavStore::new(c, MockTransport::new(|_| unreachable!())).unwrap().identity();
    assert_eq!(id(a), id(b), "documents the ambiguity; flip this when the identity is escaped");
}

// ---------------------------------------------------------------------------
// 11. Retry-After: 0, huge, negative, HTTP-date
// ---------------------------------------------------------------------------

fn retry_after_table() -> Vec<(&'static str, Option<Duration>)> {
    vec![
        ("0", Some(Duration::ZERO)),
        ("7", Some(Duration::from_secs(7))),
        (" 7 ", Some(Duration::from_secs(7))),
        ("007", Some(Duration::from_secs(7))),
        // Huge values pass through uncapped; capping is the scheduler's (G7, ≤30 min backoff).
        ("31536000000", Some(Duration::from_secs(31_536_000_000))),
        ("18446744073709551615", Some(Duration::from_secs(u64::MAX))),
        ("18446744073709551616", None),
        ("99999999999999999999999999999", None),
        ("-1", None),
        ("-0", None),
        ("+5", None),
        ("1.5", None),
        ("5s", None),
        ("1e3", None),
        ("0x10", None),
        ("\u{0663}", None),
        ("", None),
        ("Wed, 21 Oct 2015 07:28:00 GMT", None),
        ("Sun, 06 Nov 1994 08:49:37 GMT", None),
        ("Sunday, 06-Nov-94 08:49:37 GMT", None),
    ]
}

#[test]
fn retry_after_parses_only_delta_seconds() {
    for status in [429, 503] {
        for (value, expected) in retry_after_table() {
            let headers = vec![("Retry-After".to_owned(), value.to_owned())];
            assert_eq!(map_status(status, &headers), ProviderError::RateLimited { retry_after: expected }, "{status} {value:?}");
        }
    }
    let twice = vec![("retry-after".to_owned(), "3".to_owned()), ("Retry-After".to_owned(), "9".to_owned())];
    assert_eq!(map_status(429, &twice), ProviderError::RateLimited { retry_after: Some(Duration::from_secs(3)) });
    // Retry-After on other statuses never turns them into a rate limit.
    let header = vec![("Retry-After".to_owned(), "5".to_owned())];
    assert!(matches!(map_status(500, &header), ProviderError::Transient(_)));
    assert!(matches!(map_status(400, &header), ProviderError::Permanent(_)));
}

#[tokio::test]
async fn retry_after_reaches_the_caller_through_both_http_adapters() {
    let (dav_store, dav_t) = davs::emulated();
    let (r2_store, r2_t) = r2s::emulated();
    for (value, expected) in retry_after_table() {
        let want: Result<(), ProviderError> = Err(ProviderError::RateLimited { retry_after: expected });
        dav_t.fail_next(429, &[("Retry-After", value)]);
        assert_eq!(dav_store.get(SEG).await.map(drop), want, "webdav {value:?}");
        r2_t.fail_next(503, &[("Retry-After", value)]);
        assert_eq!(r2_store.put_create(SEG, OURS).await.map(drop), want, "r2 {value:?}");
    }
}

// ---------------------------------------------------------------------------
// 12. Secrets never reach an error's Debug (ProviderError has no Display)
// ---------------------------------------------------------------------------

const STATUSES: [u16; 24] =
    [200, 201, 204, 207, 301, 302, 307, 400, 401, 403, 404, 405, 409, 410, 412, 413, 415, 423, 429, 500, 502, 503, 507, 520];

fn assert_clean(texts: &[String], secrets: &[&str]) {
    assert!(!texts.is_empty());
    for text in texts {
        for secret in secrets {
            assert!(!text.contains(secret), "secret {secret:?} leaked in {text}");
        }
    }
}

#[tokio::test]
async fn webdav_secrets_never_appear_in_errors_or_request_debug() {
    const PW: &str = "pw-SECRET-7f3a-dav";
    let basic = data_encoding::BASE64.encode(format!("alice:{PW}").as_bytes());
    let status = Arc::new(Mutex::new(500u16));
    let shared = Arc::clone(&status);
    let transport = MockTransport::new(move |_| {
        let code = *shared.lock().unwrap();
        resp(code, &[("WWW-Authenticate", "Basic realm=\"x\"")], b"<D:error xmlns:D=\"DAV:\"/>")
    });
    let config = WebDavConfig { base_url: davs::BASE.into(), username: "alice".into(), password: PW.into() };
    let store = WebDavStore::new(config.clone(), transport.clone()).unwrap();
    let mut errors = Vec::new();
    for code in STATUSES {
        *status.lock().unwrap() = code;
        errors.push(format!("{:?}", store.probe_conditional_create().await));
        errors.push(format!("{:?}", store.ensure_container().await));
        errors.push(format!("{:?}", store.list(ListQuery::Prefix("v1/")).await));
        errors.push(format!("{:?}", store.get(SEG).await));
        errors.push(format!("{:?}", store.put_create(SEG, OURS).await));
        errors.push(format!("{:?}", store.put_replace(REC, b"r", Some("\"e\"")).await));
        errors.push(format!("{:?}", store.delete(SEG).await));
        errors.push(format!("{:?}", store.quota().await));
    }
    for failure in [
        TransportFailure::Timeout,
        TransportFailure::Connect("refused".into()),
        TransportFailure::Tls("bad certificate".into()),
        TransportFailure::Other("reset".into()),
    ] {
        transport.script_next(Err(failure));
        errors.push(format!("{:?}", store.get(SEG).await));
    }
    errors.push(format!("{:?}", WebDavStore::new(WebDavConfig { base_url: format!("https://alice:{PW}@h/"), ..config.clone() }, MockTransport::new(|_| unreachable!())).err()));
    errors.push(format!("{config:?}"));
    errors.push(format!("{:?}", transport.requests()));
    assert_clean(&errors, &[PW, &basic]);
    assert!(errors.last().unwrap().contains("<redacted>"));
}

#[tokio::test]
async fn r2_secrets_never_appear_in_errors_or_request_debug() {
    const AK: &str = "AKIAREDTEAMACCESS0001";
    const SK: &str = "redteam/SECRET+key=0123456789abcdef0123456789";
    let status = Arc::new(Mutex::new(500u16));
    let shared = Arc::clone(&status);
    let transport = MockTransport::new(move |_| {
        let code = *shared.lock().unwrap();
        resp(code, &[], format!("<Error><Code>X</Code><AWSAccessKeyId>{AK}</AWSAccessKeyId></Error>").as_bytes())
    });
    let config = R2Config {
        account_id: r2s::ACCOUNT.into(),
        bucket: r2s::BUCKET.into(),
        access_key_id: AK.into(),
        secret_access_key: SK.into(),
    };
    let store = R2Store::new(config.clone(), transport.clone()).unwrap();
    let mut errors = Vec::new();
    for code in STATUSES {
        *status.lock().unwrap() = code;
        errors.push(format!("{:?}", store.list(ListQuery::Prefix("v1/")).await));
        errors.push(format!("{:?}", store.get(SEG).await.map(drop)));
        errors.push(format!("{:?}", store.put_create(SEG, OURS).await));
        errors.push(format!("{:?}", store.put_replace(REC, b"r", Some("\"e\"")).await));
        errors.push(format!("{:?}", store.delete(SEG).await));
        errors.push(format!("{:?}", store.quota().await));
    }
    for failure in [TransportFailure::Timeout, TransportFailure::Tls("bad certificate".into())] {
        transport.script_next(Err(failure));
        errors.push(format!("{:?}", store.get(SEG).await));
    }
    for bad in [
        R2Config { account_id: "NOT-HEX".into(), ..config.clone() },
        R2Config { bucket: "Bad_Bucket".into(), ..config.clone() },
        R2Config { secret_access_key: String::new(), ..config.clone() },
    ] {
        errors.push(format!("{:?}", R2Store::new(bad, MockTransport::new(|_| unreachable!())).err()));
    }
    let requests = format!("{:?}", transport.requests());
    assert_clean(&errors, &[AK, SK, "Signature=", "Credential="]);
    assert_clean(&[requests, format!("{config:?}"), format!("{:?}", Credentials { access_key: AK.into(), secret_key: SK.into() })], &[SK]);
}

// ---------------------------------------------------------------------------
// 12. Final gate: attacks on the RT-B1/ADV-2, ADV-1 and ADV-7 fixes
// ---------------------------------------------------------------------------

const S3_NS: &str = "http://s3.amazonaws.com/doc/2006-03-01/";

/// Page 1 (no continuation token) answers `first`, every later page `rest`.
fn r2_two_pages(first: Vec<u8>, rest: Vec<u8>) -> (R2Store<MockTransport>, MockTransport) {
    r2(move |req| resp(200, &[], if req.url.contains("continuation-token=") { &rest } else { &first }))
}

fn transient<T: std::fmt::Debug>(result: &Result<T, ProviderError>) -> bool {
    matches!(result, Err(ProviderError::Transient(_)))
}

#[tokio::test]
async fn final_r2_prefixed_default_and_absent_namespaces_all_parse() {
    let prefixed = format!(
        r#"<?xml version="1.0"?><s3:ListBucketResult xmlns:s3="{S3_NS}"><s3:Contents><s3:Key>sayknow/v1/a.seg</s3:Key><s3:Size>1</s3:Size></s3:Contents><s3:IsTruncated>true</s3:IsTruncated><s3:NextContinuationToken>T1</s3:NextContinuationToken></s3:ListBucketResult>"#
    );
    // Whitespace, a comment, CDATA and upper case around IsTruncated.
    let bare = "<ListBucketResult>\n  <!-- page 2 -->\n  <Contents><Key>sayknow/v1/b.seg</Key></Contents>\n  <IsTruncated> <![CDATA[FALSE]]> </IsTruncated>\n</ListBucketResult>\n";
    let (store, transport) = r2_two_pages(prefixed.into_bytes(), bare.as_bytes().to_vec());
    let keys: Vec<String> = store.list(ListQuery::Prefix("")).await.unwrap().into_iter().map(|o| o.key).collect();
    assert_eq!(keys, ["v1/a.seg", "v1/b.seg"]);
    assert_eq!(transport.request_count(), 2);
}

#[tokio::test]
async fn final_r2_near_valid_list_roots_are_transient_never_a_listing() {
    let bodies: Vec<(&str, String)> = vec![
        ("self-closing root", "<ListBucketResult/>".to_owned()),
        ("namespaced root without IsTruncated", format!(r#"<ListBucketResult xmlns="{S3_NS}"><Contents><Key>sayknow/v1/a.seg</Key></Contents></ListBucketResult>"#)),
        ("self-closing IsTruncated", "<ListBucketResult><IsTruncated/></ListBucketResult>".to_owned()),
        ("IsTruncated nested in Contents", "<ListBucketResult><Contents><Key>sayknow/v1/a.seg</Key><IsTruncated>false</IsTruncated></Contents></ListBucketResult>".to_owned()),
        ("IsTruncated nested two deep", "<ListBucketResult><CommonPrefixes><IsTruncated>false</IsTruncated></CommonPrefixes></ListBucketResult>".to_owned()),
        ("listing wrapped in an Error", "<Error><ListBucketResult><IsTruncated>false</IsTruncated></ListBucketResult></Error>".to_owned()),
        ("prefixed Error root", r#"<s3:Error xmlns:s3="urn:x"><s3:Code>SlowDown</s3:Code></s3:Error>"#.to_owned()),
        ("comment then Error", "<?xml version=\"1.0\"?><!-- cf --><Error><Code>InternalError</Code></Error>".to_owned()),
        ("lower-case root", "<listbucketresult><IsTruncated>false</IsTruncated></listbucketresult>".to_owned()),
        ("longer root name", "<ListBucketResultV2><IsTruncated>false</IsTruncated></ListBucketResultV2>".to_owned()),
        ("Error root then listing", "<Error><Code>X</Code></Error><ListBucketResult><IsTruncated>false</IsTruncated></ListBucketResult>".to_owned()),
    ];
    for (name, body) in bodies {
        let body = body.into_bytes();
        let (store, transport) = r2(move |_| resp(200, &[], &body));
        let result = store.list(ListQuery::Prefix("")).await;
        assert!(transient(&result), "{name}: {result:?}");
        assert_eq!(transport.request_count(), 1, "{name}: the adapter does not retry by itself");
    }
}

#[tokio::test]
async fn final_r2_a_bad_later_page_fails_the_whole_listing_not_a_prefix_of_it() {
    let first = list_page(&["sayknow/v1/a.seg"], true, Some("T1"));
    let later: [(&str, Vec<u8>); 4] = [
        ("last page without IsTruncated", format!(r#"<ListBucketResult xmlns="{S3_NS}"><Contents><Key>sayknow/v1/b.seg</Key></Contents></ListBucketResult>"#).into_bytes()),
        ("last page with an empty root", b"<ListBucketResult/>".to_vec()),
        ("error document", br#"<?xml version="1.0"?><Error><Code>InternalError</Code></Error>"#.to_vec()),
        ("empty body", Vec::new()),
    ];
    for (name, body) in later {
        let (store, transport) = r2_two_pages(first.clone(), body);
        let result = store.list(ListQuery::Prefix("")).await;
        assert!(transient(&result), "{name}: {result:?}");
        assert_eq!(transport.request_count(), 2, "{name}");
    }
}

/// Observation (non-blocking): roots are matched by local name only and only
/// start tags are checked at depth 0. A stray self-closing `<Error/>` beside
/// the root, an `<Error>` child, and a non-S3 namespace are all accepted. In
/// each case the document still holds exactly one `ListBucketResult` that
/// states its own completeness, so nothing is hidden. Flip when tightened.
#[tokio::test]
async fn observation_r2_list_ignores_stray_empty_roots_error_children_and_namespace() {
    let bodies = [
        "<Error/><ListBucketResult><IsTruncated>false</IsTruncated></ListBucketResult>",
        "<ListBucketResult><IsTruncated>false</IsTruncated></ListBucketResult><Error/>",
        "<ListBucketResult><Error><Code>InternalError</Code></Error><IsTruncated>false</IsTruncated></ListBucketResult>",
        r#"<x:ListBucketResult xmlns:x="urn:not-s3"><x:IsTruncated>false</x:IsTruncated></x:ListBucketResult>"#,
    ];
    for body in bodies {
        let (store, _) = r2(move |_| resp(200, &[], body.as_bytes()));
        assert_eq!(store.list(ListQuery::Prefix("")).await, Ok(Vec::new()), "{body}");
    }
}

/// G5-RT-B2: `IsTruncated` is an `xs:boolean` ("true", "false", "1", "0"),
/// but anything other than case-insensitive "true" reads as false. A page
/// that says `1`, is empty, carries garbage, or repeats the element with a
/// final `false` ends the listing after page 1 even with a continuation
/// token present, hiding every later object: the RT-B1 failure class.
#[tokio::test]
async fn r2_is_truncated_values_other_than_true_or_false_must_not_end_the_listing() {
    let second = list_page(&["sayknow/v1/b.seg"], false, None);
    let mut failures = Vec::new();
    for value in ["<IsTruncated>1</IsTruncated>", "<IsTruncated></IsTruncated>", "<IsTruncated>yes</IsTruncated>", "<IsTruncated>true</IsTruncated><IsTruncated>false</IsTruncated>"] {
        let first = format!(
            r#"<ListBucketResult xmlns="{S3_NS}"><Contents><Key>sayknow/v1/a.seg</Key></Contents>{value}<NextContinuationToken>T1</NextContinuationToken></ListBucketResult>"#
        );
        let (store, _) = r2_two_pages(first.into_bytes(), second.clone());
        let result = store.list(ListQuery::Prefix("")).await;
        if let Ok(objects) = &result {
            let keys: Vec<&str> = objects.iter().map(|o| o.key.as_str()).collect();
            if keys != ["v1/a.seg", "v1/b.seg"] {
                failures.push(format!("{value}: {keys:?}"));
            }
        }
    }
    assert!(failures.is_empty(), "a listing ended early and hid page 2: {failures:#?}");
}

/// MKCOL 201, DELETE 204; probe PUTs and object PUTs answer from their own
/// queues, so a test scripts the server's behaviour across probes and creates.
type Statuses = Arc<Mutex<VecDeque<u16>>>;

fn scripted_dav() -> (WebDavStore<MockTransport>, MockTransport, Statuses, Statuses) {
    let probe: Statuses = Arc::default();
    let objects: Statuses = Arc::default();
    let (p, o) = (Arc::clone(&probe), Arc::clone(&objects));
    let (store, transport) = dav(move |req| match req.method.as_str() {
        "MKCOL" => resp(201, &[], b""),
        "DELETE" => resp(204, &[], b""),
        "PUT" => {
            let queue = if req.url.contains("/.probe/") { &p } else { &o };
            let status = queue.lock().unwrap().pop_front().expect("unscripted PUT");
            resp(status, &[("ETag", "\"e\"")], b"")
        }
        _ => resp(404, &[], b""),
    });
    (store, transport, probe, objects)
}

fn push(queue: &Statuses, statuses: &[u16]) {
    queue.lock().unwrap().extend(statuses);
}

#[tokio::test]
async fn final_webdav_create_200_or_204_revokes_trust_until_an_honest_reprobe() {
    let seg2 = "v1/vault-a/log/dev-a/000000000004-000000000004.seg";
    let seg3 = "v1/vault-a/log/dev-a/000000000005-000000000005.seg";
    for overwrite in [200u16, 204] {
        let (store, transport, probe, objects) = scripted_dav();
        push(&probe, &[201, 412]);
        store.probe_conditional_create().await.unwrap();
        push(&objects, &[201, overwrite]);
        assert!(store.put_create(SEG, OURS).await.is_ok(), "{overwrite}");
        assert_eq!(store.put_create(seg2, OURS).await, Err(ProviderError::Unsupported("server does not honour If-None-Match".to_owned())));
        assert!(!store.create_only(), "{overwrite}");
        let puts = put_count(&transport);
        assert!(matches!(store.put_create(seg3, OURS).await, Err(ProviderError::Unsupported(_))));
        assert_eq!(put_count(&transport), puts, "{overwrite}: a revoked store sends no create");

        // Dishonest re-probes keep it locked; a transient one does too. A
        // non-201 first answer ends the probe after one PUT.
        let answers: [&[u16]; 6] = [&[201, 204], &[201, 200], &[201, 201], &[204], &[200], &[201, 500]];
        for answers in answers {
            push(&probe, answers);
            assert!(store.probe_conditional_create().await.is_err(), "{overwrite} then {answers:?}");
            assert!(!store.create_only(), "{overwrite} then {answers:?}");
        }
        assert!(probe.lock().unwrap().is_empty(), "every scripted probe answer was used");

        // An honest re-probe restores it; 412 then keeps trust, 201 creates.
        push(&probe, &[201, 412]);
        store.probe_conditional_create().await.unwrap();
        push(&objects, &[412, 201]);
        assert_eq!(store.put_create(SEG, THEIRS).await, Err(ProviderError::AlreadyExists));
        assert!(store.create_only());
        assert!(store.put_create(seg3, OURS).await.is_ok());

        // A 200/204 on the retry after 409 (parents created) also revokes.
        push(&objects, &[409, overwrite]);
        assert!(matches!(store.put_create(seg2, OURS).await, Err(ProviderError::Unsupported(_))));
        assert!(!store.create_only(), "{overwrite} after 409 retry");
        assert!(objects.lock().unwrap().is_empty());
    }
}

/// G5-RT-B3: the probe only ever sets the flag. A store that passed once and
/// is re-probed (G7 re-probes at every session start) keeps `create_only()`
/// and keeps sending creates after the server has just proven it overwrites:
/// the next create over an existing key is the A4 data loss, revoked only
/// after the fact by the 200/204 answer.
#[tokio::test]
async fn webdav_reprobe_that_disproves_atomic_create_must_revoke_trust() {
    let mut failures = Vec::new();
    for answers in [[201u16, 204], [201, 200], [201, 201], [204, 412], [200, 412]] {
        let (store, transport, probe, objects) = scripted_dav();
        push(&probe, &[201, 412]);
        store.probe_conditional_create().await.unwrap();
        push(&probe, &answers);
        let verdict = store.probe_conditional_create().await;
        assert!(matches!(verdict, Err(ProviderError::Unsupported(_))), "{answers:?}: {verdict:?}");
        push(&objects, &[204]);
        let unlocked = store.create_only();
        let puts = put_count(&transport);
        let create = store.put_create(SEG, OURS).await;
        if unlocked || put_count(&transport) != puts {
            failures.push(format!("{answers:?}: create_only()={unlocked} after the failed re-probe; create PUT sent ({create:?})"));
        }
    }
    assert!(failures.is_empty(), "{failures:#?}");
}

/// Observation (non-blocking): ADV-1 revokes only on 200/204. Any other 2xx
/// to a create (202, 203, 205, 206, 207) is `Permanent`, never success, but
/// trust is kept, unlike the probe, which refuses every non-201 2xx.
#[tokio::test]
async fn observation_webdav_create_other_2xx_is_permanent_but_keeps_trust() {
    for status in [202u16, 203, 205, 206, 207] {
        let (store, _, probe, objects) = scripted_dav();
        push(&probe, &[201, 412]);
        store.probe_conditional_create().await.unwrap();
        push(&objects, &[status]);
        assert!(permanent(&store.put_create(SEG, OURS).await), "{status}");
        assert!(store.create_only(), "{status}: documents current behaviour; flip if revoked");
    }
}

#[tokio::test]
async fn final_body_too_large_is_permanent_on_every_operation_and_never_partial() {
    let too_large = || Err(TransportFailure::BodyTooLarge);

    // WebDAV: every operation, first request over the cap, no internal retry.
    let (store, transport, _) = davs::over(DavServer::new());
    store.probe_conditional_create().await.unwrap();
    store.ensure_container().await.unwrap();
    store.put_create(SEG, OURS).await.unwrap();
    for op in 0..7 {
        transport.script_next(too_large());
        let before = transport.request_count();
        let failed = match op {
            0 => permanent(&store.list(ListQuery::Prefix("")).await),
            1 => permanent(&store.get(SEG).await),
            2 => permanent(&store.put_create(REC, OURS).await),
            3 => permanent(&store.put_replace(REC, OURS, None).await),
            4 => permanent(&store.delete(SEG).await),
            5 => permanent(&store.quota().await),
            _ => permanent(&store.ensure_container().await),
        };
        assert!(failed, "webdav op {op}");
        assert_eq!(transport.request_count(), before + 1, "webdav op {op}");
    }
    assert!(store.create_only(), "an over-cap body is not evidence of overwrite");

    // WebDAV probe: over the cap on the second PUT never unlocks.
    let (store, transport) = probe_server(201, 412);
    transport.script_next(Ok(resp(201, &[], b"")));
    transport.script_next(Ok(resp(201, &[], b"")));
    transport.script_next(too_large());
    assert!(permanent(&store.probe_conditional_create().await));
    assert!(!store.create_only());

    // R2: over the cap on page 2 fails the listing, never returns page 1.
    let (store, transport) = r2_two_pages(list_page(&["sayknow/v1/a.seg"], true, Some("T1")), list_page(&[], false, None));
    transport.script_next(Ok(resp(200, &[], &list_page(&["sayknow/v1/a.seg"], true, Some("T1")))));
    transport.script_next(too_large());
    assert!(permanent(&store.list(ListQuery::Prefix("")).await));
    for op in 0..4 {
        transport.script_next(too_large());
        let failed = match op {
            0 => permanent(&store.get(SEG).await),
            1 => permanent(&store.put_create(SEG, OURS).await),
            2 => permanent(&store.put_replace(REC, OURS, None).await),
            _ => permanent(&store.delete(SEG).await),
        };
        assert!(failed, "r2 op {op}");
    }

    // Through the bridge an over-cap create is unknown, never Created.
    let id = store.identity();
    let bound = Bound::new(store, binding_fp(&id));
    transport.script_next(too_large());
    assert_eq!(SegmentRemote::put_create(&bound, SEG, OURS).await, Err(TransportError));
}

// ---------------------------------------------------------------------------
// 13. Final gate 2: attacks on the RT-B2, RT-B3 and AF-1 fixes
// ---------------------------------------------------------------------------

/// A listing page whose `IsTruncated` markup is `is_truncated` verbatim.
fn page_spelled(keys: &[&str], is_truncated: &str, token: Option<&str>) -> Vec<u8> {
    let mut xml = format!(r#"<?xml version="1.0" encoding="UTF-8"?><ListBucketResult xmlns="{S3_NS}">"#);
    for key in keys {
        xml.push_str(&format!("<Contents><Key>{key}</Key><Size>1</Size><ETag>\"e\"</ETag></Contents>"));
    }
    xml.push_str(is_truncated);
    if let Some(token) = token {
        xml.push_str(&format!("<NextContinuationToken>{token}</NextContinuationToken>"));
    }
    xml.push_str("</ListBucketResult>");
    xml.into_bytes()
}

/// Page 1 has no continuation token; page `n + 1` answers token `Tn`.
fn r2_pages(pages: Vec<Vec<u8>>) -> (R2Store<MockTransport>, MockTransport) {
    r2(move |req| {
        let index = (1..pages.len()).rev().find(|n| req.url.contains(&format!("continuation-token=T{n}"))).unwrap_or(0);
        resp(200, &[], &pages[index])
    })
}

fn keys_of(result: &Result<Vec<ListedObject>, ProviderError>) -> Option<Vec<&str>> {
    result.as_ref().ok().map(|objects| objects.iter().map(|o| o.key.as_str()).collect())
}

#[tokio::test]
async fn final2_r2_is_truncated_spelled_through_whitespace_entities_cdata_and_comments_parses() {
    let trues = ["\n\t true \r\n", "&#116;rue", "&#x74;&#x72;&#x75;&#x65;", "<![CDATA[true]]>", "<![CDATA[ TRUE ]]>", "tr<![CDATA[ue]]>", "t<!-- c -->rue", "&#32;True&#9;", "tr<?pi x?>ue"];
    let falses = ["\r\n false\t", "&#102;alse", "<![CDATA[false]]>", "fa<![CDATA[ls]]>e", "FALSE<!-- end -->", "&#x20;false&#xA;"];
    for t in trues {
        for f in falses {
            let first = page_spelled(&["sayknow/v1/a.seg"], &format!("<IsTruncated>{t}</IsTruncated>"), Some("T1"));
            let last = page_spelled(&["sayknow/v1/b.seg"], &format!("<IsTruncated>{f}</IsTruncated>"), None);
            let (store, transport) = r2_pages(vec![first, last]);
            let result = store.list(ListQuery::Prefix("")).await;
            assert_eq!(keys_of(&result), Some(vec!["v1/a.seg", "v1/b.seg"]), "{t:?} / {f:?}: {result:?}");
            assert_eq!(transport.request_count(), 2, "{t:?} / {f:?}");
        }
    }
}

#[tokio::test]
async fn final2_r2_is_truncated_that_decodes_to_anything_else_fails_the_listing_on_any_page() {
    let values = [
        "&amp;true", "&lt;true&gt;", "<![CDATA[]]>", "<![CDATA[tru]]>", "<![CDATA[true]]>x", "t r u e", "&#116;rue&#116;",
        "&quot;false&quot;", "falsefalse", "1", "0", "&#102;", "&unknown;false", "fa&#00;lse", "<![CDATA[fal]]>",
    ];
    for value in values {
        let spelled = format!("<IsTruncated>{value}</IsTruncated>");
        // On page 1, with a token that would lead to page 2.
        let (store, transport) =
            r2_pages(vec![page_spelled(&["sayknow/v1/a.seg"], &spelled, Some("T1")), list_page(&["sayknow/v1/b.seg"], false, None)]);
        let result = store.list(ListQuery::Prefix("")).await;
        assert!(result.is_err(), "page 1 {value:?}: {result:?}");
        assert_eq!(transport.request_count(), 1, "page 1 {value:?}");
        // On the last page: the whole listing fails, never page 1 alone.
        let (store, transport) =
            r2_pages(vec![list_page(&["sayknow/v1/a.seg"], true, Some("T1")), page_spelled(&["sayknow/v1/b.seg"], &spelled, None)]);
        let result = store.list(ListQuery::Prefix("")).await;
        assert!(result.is_err(), "last page {value:?}: {result:?}");
        assert_eq!(transport.request_count(), 2, "last page {value:?}");
    }
}

#[tokio::test]
async fn final2_r2_nested_is_truncated_is_refused_and_non_top_level_copies_are_ignored() {
    let refused = [
        "<IsTruncated><IsTruncated>false</IsTruncated></IsTruncated>".to_owned(),
        "<IsTruncated><x>false</x></IsTruncated>".to_owned(),
        "<IsTruncated><Contents><IsTruncated>false</IsTruncated></Contents></IsTruncated>".to_owned(),
        format!(r#"<IsTruncated><s3:IsTruncated xmlns:s3="{S3_NS}">true</s3:IsTruncated></IsTruncated>"#),
    ];
    for spelled in refused {
        let (store, transport) =
            r2_pages(vec![page_spelled(&["sayknow/v1/a.seg"], &spelled, Some("T1")), list_page(&["sayknow/v1/b.seg"], false, None)]);
        let result = store.list(ListQuery::Prefix("")).await;
        assert!(transient(&result), "{spelled}: {result:?}");
        assert_eq!(transport.request_count(), 1, "{spelled}");
    }

    // A nested `false` never ends a top-level `true` listing...
    let nested_false = "<Contents><Key>sayknow/v1/n.seg</Key><IsTruncated>false</IsTruncated></Contents><IsTruncated>true</IsTruncated>";
    let (store, transport) =
        r2_pages(vec![page_spelled(&["sayknow/v1/a.seg"], nested_false, Some("T1")), list_page(&["sayknow/v1/b.seg"], false, None)]);
    let result = store.list(ListQuery::Prefix("")).await;
    assert_eq!(keys_of(&result), Some(vec!["v1/a.seg", "v1/n.seg", "v1/b.seg"]), "{result:?}");
    assert_eq!(transport.request_count(), 2);
    // ...and a nested `true` never extends a top-level `false` one.
    let nested_true = "<CommonPrefixes><IsTruncated>true</IsTruncated></CommonPrefixes><IsTruncated>false</IsTruncated>";
    let (store, transport) =
        r2_pages(vec![page_spelled(&["sayknow/v1/a.seg"], nested_true, Some("T1")), list_page(&["sayknow/v1/b.seg"], false, None)]);
    assert_eq!(keys_of(&store.list(ListQuery::Prefix("")).await), Some(vec!["v1/a.seg"]));
    assert_eq!(transport.request_count(), 1);
}

#[tokio::test]
async fn final2_r2_duplicate_is_truncated_on_any_later_page_fails_the_whole_listing() {
    let duplicates = [
        "<IsTruncated>false</IsTruncated><IsTruncated>false</IsTruncated>".to_owned(),
        "<IsTruncated>true</IsTruncated><IsTruncated>true</IsTruncated>".to_owned(),
        "<IsTruncated>false</IsTruncated><IsTruncated/>".to_owned(),
        "<IsTruncated/><IsTruncated>false</IsTruncated>".to_owned(),
        format!(r#"<IsTruncated>false</IsTruncated><s3:IsTruncated xmlns:s3="{S3_NS}">false</s3:IsTruncated>"#),
        "<IsTruncated>false</IsTruncated><Contents><Key>sayknow/v1/z.seg</Key></Contents><IsTruncated>false</IsTruncated>".to_owned(),
    ];
    let once = |keys: &[&str], truncated: bool, token: Option<&str>| list_page(keys, truncated, token);

    // One IsTruncated per page across three pages is not a duplicate.
    let (store, transport) = r2_pages(vec![
        once(&["sayknow/v1/a.seg"], true, Some("T1")),
        once(&["sayknow/v1/b.seg"], true, Some("T2")),
        once(&["sayknow/v1/c.seg"], false, None),
    ]);
    assert_eq!(keys_of(&store.list(ListQuery::Prefix("")).await), Some(vec!["v1/a.seg", "v1/b.seg", "v1/c.seg"]));
    assert_eq!(transport.request_count(), 3);

    for dup in duplicates {
        // Duplicated on page 2 (with a token onward) and on the last page 3.
        let (store, transport) = r2_pages(vec![
            once(&["sayknow/v1/a.seg"], true, Some("T1")),
            page_spelled(&["sayknow/v1/b.seg"], &dup, Some("T2")),
            once(&["sayknow/v1/c.seg"], false, None),
        ]);
        let result = store.list(ListQuery::Prefix("")).await;
        assert!(transient(&result), "page 2 {dup}: {result:?}");
        assert_eq!(transport.request_count(), 2, "page 2 {dup}");

        let (store, transport) = r2_pages(vec![
            once(&["sayknow/v1/a.seg"], true, Some("T1")),
            once(&["sayknow/v1/b.seg"], true, Some("T2")),
            page_spelled(&["sayknow/v1/c.seg"], &dup, None),
        ]);
        let result = store.list(ListQuery::Prefix("")).await;
        assert!(transient(&result), "page 3 {dup}: {result:?}");
        assert_eq!(transport.request_count(), 3, "page 3 {dup}");
    }
}

/// G5-RT-B4: the RT-B2 fix compares `buf.trim()`, and `buf` is cleared at
/// every start/end tag. `str::trim` strips Unicode White_Space (U+00A0,
/// U+2028, U+3000, U+0085), which is not XML whitespace, and a child element
/// inside `IsTruncated` discards the text before it. Content that is not
/// strictly `true`/`false` (`\u{a0}false`, `true<x></x>false`) reads as
/// `false` and ends the listing with a continuation token present, hiding
/// page 2: the RT-B2 failure class, still open on these spellings.
#[tokio::test]
async fn r2_is_truncated_with_non_xml_whitespace_or_child_elements_must_not_end_the_listing() {
    let values = [
        "\u{a0}false", "&#xA0;false", "false&#x3000;", "&#x2028;false", "&#x85;false", "true<x></x>false", "<x>true</x>false",
        "true<x>y</x>false",
    ];
    let mut failures = Vec::new();
    for value in values {
        let first = page_spelled(&["sayknow/v1/a.seg"], &format!("<IsTruncated>{value}</IsTruncated>"), Some("T1"));
        let (store, _) = r2_pages(vec![first, list_page(&["sayknow/v1/b.seg"], false, None)]);
        let result = store.list(ListQuery::Prefix("")).await;
        if let Some(keys) = keys_of(&result) {
            if keys != ["v1/a.seg", "v1/b.seg"] {
                failures.push(format!("{value:?}: {keys:?}"));
            }
        }
    }
    assert!(failures.is_empty(), "a listing ended early and hid page 2: {failures:#?}");
}

/// A request matching `matches` announces itself on `reached`, then waits for
/// one `release` permit before the inner transport records and answers it.
struct Gate {
    matches: Box<dyn Fn(&HttpRequest) -> bool + Send + Sync>,
    reached: tokio::sync::Notify,
    release: tokio::sync::Semaphore,
}

impl Gate {
    fn open(&self) {
        self.release.add_permits(1);
    }
}

fn gate(matches: impl Fn(&HttpRequest) -> bool + Send + Sync + 'static) -> Gate {
    Gate { matches: Box::new(matches), reached: tokio::sync::Notify::new(), release: tokio::sync::Semaphore::new(0) }
}

/// The `n`-th (0-based) probe PUT the store sends.
fn nth_probe_put(n: usize) -> impl Fn(&HttpRequest) -> bool + Send + Sync + 'static {
    let seen = AtomicUsize::new(0);
    move |req| req.method == "PUT" && req.url.contains("/.probe/") && seen.fetch_add(1, Ordering::SeqCst) == n
}

fn object_put(key: &'static str) -> impl Fn(&HttpRequest) -> bool + Send + Sync + 'static {
    move |req| req.method == "PUT" && req.url.ends_with(key)
}

struct Gated {
    inner: MockTransport,
    gates: Arc<Vec<Gate>>,
}

impl HttpTransport for Gated {
    async fn send(&self, req: HttpRequest) -> Result<HttpResponse, TransportFailure> {
        if let Some(gate) = self.gates.iter().find(|gate| (gate.matches)(&req)) {
            gate.reached.notify_one();
            gate.release.acquire().await.expect("gates are never closed").forget();
        }
        HttpTransport::send(&self.inner, req).await
    }
}

/// [`scripted_dav`] behind gates: the first matching gate holds a request.
fn gated_dav(gates: Vec<Gate>) -> (WebDavStore<Gated>, MockTransport, Arc<Vec<Gate>>, Statuses, Statuses) {
    let probe: Statuses = Arc::default();
    let objects: Statuses = Arc::default();
    let (p, o) = (Arc::clone(&probe), Arc::clone(&objects));
    let inner = MockTransport::new(move |req| match req.method.as_str() {
        "MKCOL" => resp(201, &[], b""),
        "DELETE" => resp(204, &[], b""),
        "PUT" => {
            let queue = if req.url.contains("/.probe/") { &p } else { &o };
            let status = queue.lock().unwrap().pop_front().expect("unscripted PUT");
            resp(status, &[("ETag", "\"e\"")], b"")
        }
        _ => resp(404, &[], b""),
    });
    let gates = Arc::new(gates);
    let store = WebDavStore::new(davs::config(), Gated { inner: inner.clone(), gates: Arc::clone(&gates) }).unwrap();
    (store, inner, gates, probe, objects)
}

#[tokio::test]
async fn final2_webdav_a_create_racing_a_reprobe_is_refused_without_a_put() {
    // Probe PUTs #0/#1 are the first probe; #2 is the re-probe's first PUT.
    let (store, transport, gates, probe, objects) = gated_dav(vec![gate(nth_probe_put(2))]);
    push(&probe, &[201, 412]);
    store.probe_conditional_create().await.unwrap();
    push(&probe, &[201, 412]);
    let (reprobe, ()) = tokio::join!(store.probe_conditional_create(), async {
        gates[0].reached.notified().await;
        assert!(!store.create_only(), "trust is revoked before the re-probe's first PUT");
        let puts = put_count(&transport);
        assert!(matches!(store.put_create(SEG, OURS).await, Err(ProviderError::Unsupported(_))));
        assert_eq!(put_count(&transport), puts, "no create PUT while a re-probe is in flight");
        gates[0].open();
    });
    reprobe.unwrap();
    assert!(store.create_only());
    push(&objects, &[201]);
    assert!(store.put_create(SEG, OURS).await.is_ok());
    assert!(objects.lock().unwrap().is_empty() && probe.lock().unwrap().is_empty());
}

#[tokio::test]
async fn final2_webdav_a_failed_reprobe_during_an_in_flight_create_leaves_the_store_locked() {
    let seg2 = "v1/vault-a/log/dev-a/000000000004-000000000004.seg";
    let reprobes: [&[u16]; 5] = [&[201, 204], &[201, 200], &[201, 201], &[204], &[412]];
    for answer in [201u16, 412, 204, 200] {
        for reprobe in reprobes {
            let (store, transport, gates, probe, objects) = gated_dav(vec![gate(object_put(SEG))]);
            push(&probe, &[201, 412]);
            store.probe_conditional_create().await.unwrap();
            push(&objects, &[answer]);
            // The create passed its trust check before the re-probe started;
            // its PUT is held while the re-probe fails.
            let (create, ()) = tokio::join!(store.put_create(SEG, OURS), async {
                gates[0].reached.notified().await;
                push(&probe, reprobe);
                let verdict = store.probe_conditional_create().await;
                assert!(verdict.is_err(), "{answer} / {reprobe:?}: {verdict:?}");
                assert!(!store.create_only(), "{answer} / {reprobe:?}");
                gates[0].open();
            });
            match answer {
                201 => assert!(create.is_ok(), "{answer} / {reprobe:?}: {create:?}"),
                412 => assert_eq!(create.map(drop), Err(ProviderError::AlreadyExists), "{reprobe:?}"),
                _ => assert!(matches!(create, Err(ProviderError::Unsupported(_))), "{answer} / {reprobe:?}: {create:?}"),
            }
            assert!(!store.create_only(), "{answer} / {reprobe:?}: the in-flight answer never restores trust");
            let puts = put_count(&transport);
            assert!(matches!(store.put_create(seg2, OURS).await, Err(ProviderError::Unsupported(_))));
            assert_eq!(put_count(&transport), puts, "{answer} / {reprobe:?}: a locked store sends no create");
            assert!(objects.lock().unwrap().is_empty() && probe.lock().unwrap().is_empty(), "{answer} / {reprobe:?}");
        }
    }
}

/// Observation (non-blocking, owner G7): revocation is a plain flag store.
/// A create's 200/204 that lands while a re-probe is in flight is overridden
/// when that probe passes, and two overlapping probes end trusted if the
/// passing one sets the flag after the failing one cleared it. Both need a
/// re-probe concurrent with other calls on the same store; G7's connect step
/// probes before the store reaches the engine and must keep probes serial
/// and exclusive (plan-amendments "every session start", ADV-5). Flip if the
/// adapter gains a revocation generation.
#[tokio::test]
async fn observation_webdav_a_204_seen_during_a_passing_reprobe_is_overridden_by_it() {
    // Probe PUT #3 is the re-probe's second PUT.
    let (store, _, gates, probe, objects) = gated_dav(vec![gate(object_put(SEG)), gate(nth_probe_put(3))]);
    push(&probe, &[201, 412]);
    store.probe_conditional_create().await.unwrap();
    push(&objects, &[204]);
    push(&probe, &[201, 412]);
    let created = tokio::sync::Notify::new();
    let (create, reprobe, ()) = tokio::join!(
        async {
            let result = store.put_create(SEG, OURS).await;
            created.notify_one();
            result
        },
        async {
            gates[0].reached.notified().await;
            store.probe_conditional_create().await
        },
        async {
            gates[1].reached.notified().await;
            gates[0].open();
            created.notified().await;
            assert!(!store.create_only(), "the 204 revoked trust");
            gates[1].open();
        },
    );
    assert!(matches!(create, Err(ProviderError::Unsupported(_))), "{create:?}");
    reprobe.unwrap();
    assert!(store.create_only(), "documents current behaviour: the passing probe re-grants trust after the 204");
}

#[tokio::test]
async fn observation_webdav_overlapping_probes_end_trusted_when_the_failing_one_finishes_last() {
    // B: PUT #0 → 201, #1 held. A: #2 → 201, #3 → 412 (passes). B's #1 → 204.
    let (store, _, gates, probe, _) = gated_dav(vec![gate(nth_probe_put(1))]);
    push(&probe, &[201, 201, 412, 204]);
    let (b, a) = tokio::join!(store.probe_conditional_create(), async {
        gates[0].reached.notified().await;
        let a = store.probe_conditional_create().await;
        gates[0].open();
        a
    });
    a.unwrap();
    assert!(matches!(b, Err(ProviderError::Unsupported(_))), "{b:?}");
    assert!(store.create_only(), "documents current behaviour: the failed probe finished last yet trust stays granted");
}

const CAP: usize = super::provider::MAX_OBJECT_BYTES;

/// Bare adapter, `put_replace` (fresh and with If-Match) at cap-1, then every
/// at/over-cap put, including one with a stale If-Match, sends nothing.
async fn cap_boundary<P: ObjectStore>(store: &P, requests: impl Fn() -> usize, name: &str) {
    let first = vec![0xa5u8; CAP - 1];
    let created = store.put_replace(REC, &first, None).await.unwrap();
    let mut second = vec![0x5au8; CAP - 1];
    let replaced = store.put_replace(REC, &second, created.etag.as_deref()).await.unwrap();
    assert!(store.get(REC).await.unwrap() == second, "{name}: cap-1 replace round-trips byte for byte");

    let before = requests();
    for len in [CAP, CAP + 1] {
        second.resize(len, 0x5a);
        for if_match in [None, replaced.etag.as_deref(), Some("\"stale\"")] {
            let result = store.put_replace(REC, &second, if_match).await;
            assert!(permanent(&result), "{name} put_replace {len} {if_match:?}: {result:?}");
        }
        assert!(permanent(&store.put_create(SEG, &second).await), "{name} put_create {len}");
    }
    assert_eq!(requests(), before, "{name}: an oversize put sends no request");
    assert_eq!(store.get(REC).await.unwrap().len(), CAP - 1, "{name}: the stored object is untouched");
    assert_eq!(store.get(SEG).await, Err(ProviderError::NotFound), "{name}");
}

#[tokio::test]
async fn final2_size_cap_boundary_on_bare_put_replace_and_put_create_per_http_backend() {
    {
        let (store, transport, _) = davs::over(DavServer::new());
        store.probe_conditional_create().await.unwrap();
        cap_boundary(&store, || transport.request_count(), "webdav").await;
    }
    let (store, transport) = r2s::emulated();
    cap_boundary(&store, || transport.request_count(), "r2").await;
}

#[tokio::test]
async fn final2_oversize_is_refused_before_the_probe_gate_and_never_reads_as_overwrite() {
    let body = vec![0u8; CAP];

    // Unprobed WebDAV: size is checked first, nothing is sent, still locked.
    let (store, transport, _) = davs::over(DavServer::new());
    assert!(permanent(&store.put_create(SEG, &body).await));
    assert!(permanent(&store.put_replace(REC, &body, Some("\"stale\"")).await));
    assert_eq!(transport.request_count(), 0);
    assert!(!store.create_only());
    // Probed: an oversize create is not evidence of overwrite.
    store.probe_conditional_create().await.unwrap();
    let before = transport.request_count();
    assert!(permanent(&store.put_create(SEG, &body).await));
    assert!(store.create_only());
    assert_eq!(transport.request_count(), before);

    // R2: a stale If-Match never turns an oversize put into PreconditionFailed.
    let (store, transport) = r2s::emulated();
    assert!(permanent(&store.put_replace(REC, &body, Some("\"stale\"")).await));
    assert!(permanent(&store.put_create(SEG, &body).await));
    assert_eq!(transport.request_count(), 0);

    // Bound: a mismatched binding is reported first; nothing reaches the store.
    let fake = FakeStore::new(true);
    fake.set_identity("fake|other");
    let bound = Bound::new(fake, binding_fp("fake|default"));
    assert_eq!(ObjectStore::put_create(&bound, SEG, &body).await.map(drop), Err(ProviderError::BindingMismatch));
    assert_eq!(bound.inner().requests(), 0);

    // The resolver bridge: unknown, never Created, never stored.
    let bound = Bound::new(FakeStore::new(true), binding_fp("fake|default"));
    assert_eq!(SegmentRemote::put_create(&bound, SEG, &body).await, Err(TransportError));
    assert_eq!(bound.inner().requests(), 0);
    assert!(bound.inner().copies(SEG).is_empty());
}

async fn empty_round_trip<P: ObjectStore>(store: P, name: &str) {
    let id = store.identity();
    let bound = Bound::new(store, binding_fp(&id));
    assert!(ObjectStore::put_create(&bound, SEG, b"").await.is_ok(), "{name}");
    assert_eq!(bound.get(SEG).await, Ok(Vec::new()), "{name}");
    assert_eq!(ObjectStore::put_create(&bound, SEG, b"").await.map(drop), Err(ProviderError::AlreadyExists), "{name}");
    assert_eq!(SegmentRemote::put_create(&bound, SEG, b"").await, Ok(PutOutcome::AlreadyExists), "{name}");
    let rec = bound.put_replace(REC, b"", None).await.unwrap();
    assert!(bound.put_replace(REC, b"", rec.etag.as_deref()).await.is_ok(), "{name}");
    assert_eq!(bound.get(REC).await, Ok(Vec::new()), "{name}");
    let listed = bound.list(ListQuery::Prefix("v1/vault-a/")).await.unwrap();
    for key in [SEG, REC] {
        let object = listed.iter().find(|o| o.key == key);
        assert!(object.is_some_and(|o| o.size.unwrap_or(0) == 0), "{name} {key}: {listed:?}");
    }
}

#[tokio::test]
async fn final2_empty_body_round_trips_on_every_backend() {
    empty_round_trip(FakeStore::new(true), "fake").await;
    let (store, transport, _) = davs::over(DavServer::new());
    store.probe_conditional_create().await.unwrap();
    empty_round_trip(store, "webdav").await;
    let (r2_store, r2_transport) = r2s::emulated();
    empty_round_trip(r2_store, "r2").await;
    let puts = transport.requests().into_iter().chain(r2_transport.requests()).filter(|r| r.method == "PUT" && !r.url.contains("/.probe/"));
    for put in puts {
        assert!(put.body.is_empty(), "{put:?}");
    }
}
