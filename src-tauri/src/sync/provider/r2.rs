//! Cloudflare R2 adapter over the S3 API (§4.9).
//!
//! Path-style URLs `https://<account>.r2.cloudflarestorage.com/<bucket>/sayknow/<key>`,
//! region `auto`, service `s3`, every request SigV4-signed over the body's
//! SHA-256. Creates use `If-None-Match: *`, which R2 enforces atomically, so
//! the store is always create-only. Secrets never reach an error string.

use std::fmt;
use std::time::SystemTime;

use percent_encoding::{utf8_percent_encode, AsciiSet, NON_ALPHANUMERIC};
use quick_xml::events::Event;
use quick_xml::reader::NsReader;

use super::sigv4::{self, Credentials};
use super::transport::{HttpRequest, HttpResponse, HttpTransport};
use super::webdav::push_reference;
use super::{check_key, check_object_size, is_remote_key, keyed_listing, map_status, ListQuery, ListedObject, ObjectStore, ProviderError, Quota};

const REGION: &str = "auto";
const SERVICE: &str = "s3";
/// Every object key starts with this, so the bucket may hold other data.
const KEY_PREFIX: &str = "sayknow/";
/// More pages than this means the listing is not converging.
const MAX_LIST_PAGES: usize = 100_000;

/// RFC 3986 unreserved characters stay literal in query values.
const QUERY_VALUE: &AsciiSet = &NON_ALPHANUMERIC.remove(b'-').remove(b'.').remove(b'_').remove(b'~');

#[derive(Clone)]
pub(crate) struct R2Config {
    pub account_id: String,
    pub bucket: String,
    pub access_key_id: String,
    pub secret_access_key: String,
}

impl fmt::Debug for R2Config {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("R2Config")
            .field("account_id", &self.account_id)
            .field("bucket", &self.bucket)
            .field("access_key_id", &self.access_key_id)
            .field("secret_access_key", &"<redacted>")
            .finish()
    }
}

pub(crate) struct R2Store<T> {
    transport: T,
    account_id: String,
    bucket: String,
    credentials: Credentials,
    /// `https://<account_id>.r2.cloudflarestorage.com`, no trailing slash.
    endpoint: String,
    clock: fn() -> SystemTime,
}

impl<T: HttpTransport> R2Store<T> {
    pub(crate) fn new(config: R2Config, transport: T) -> Result<Self, ProviderError> {
        if !is_account_id(&config.account_id) {
            return Err(ProviderError::Permanent("R2 account id must be 32 lowercase hex characters".to_owned()));
        }
        if !is_bucket_name(&config.bucket) {
            return Err(ProviderError::Permanent("invalid R2 bucket name".to_owned()));
        }
        if config.access_key_id.trim().is_empty() || config.secret_access_key.is_empty() {
            return Err(ProviderError::Permanent("R2 credentials are missing".to_owned()));
        }
        let endpoint = format!("https://{}.r2.cloudflarestorage.com", config.account_id);
        Ok(Self {
            transport,
            account_id: config.account_id,
            bucket: config.bucket,
            credentials: Credentials { access_key: config.access_key_id, secret_key: config.secret_access_key },
            endpoint,
            clock: SystemTime::now,
        })
    }

    fn object_url(&self, key: &str) -> String {
        // Keys are `is_remote_key`-safe, so no byte needs encoding.
        format!("{}/{}/{KEY_PREFIX}{key}", self.endpoint, self.bucket)
    }

    fn list_url(&self, prefix: &str, token: Option<&str>) -> String {
        let mut url = format!(
            "{}/{}?list-type=2&prefix={}",
            self.endpoint,
            self.bucket,
            utf8_percent_encode(&format!("{KEY_PREFIX}{prefix}"), QUERY_VALUE)
        );
        if let Some(token) = token {
            url.push_str("&continuation-token=");
            url.push_str(&utf8_percent_encode(token, QUERY_VALUE).to_string());
        }
        url
    }

    async fn send(&self, method: &str, url: String, headers: &[(&str, &str)], body: Vec<u8>) -> Result<HttpResponse, ProviderError> {
        let mut req = HttpRequest {
            method: method.to_owned(),
            url,
            headers: headers.iter().map(|(n, v)| ((*n).to_owned(), (*v).to_owned())).collect(),
            body,
        };
        let payload = sigv4::sha256_hex(&req.body);
        sigv4::sign(&mut req, &self.credentials, REGION, SERVICE, (self.clock)(), &payload)
            .map_err(|_| ProviderError::Permanent("could not sign R2 request".to_owned()))?;
        self.transport.send(req).await.map_err(ProviderError::from)
    }

    fn listed(&self, key: &str, size: u64, resp: &HttpResponse) -> ListedObject {
        ListedObject {
            key: key.to_owned(),
            provider_ref: key.to_owned(),
            size: Some(size),
            etag: resp.header("etag").map(str::to_owned),
        }
    }
}

impl<T: HttpTransport> ObjectStore for R2Store<T> {
    fn create_only(&self) -> bool {
        true
    }

    fn identity(&self) -> String {
        format!("r2|{}|{}|{}", self.account_id, self.bucket, self.endpoint)
    }

    async fn list(&self, q: ListQuery<'_>) -> Result<Vec<ListedObject>, ProviderError> {
        let listing = keyed_listing(q)?;
        let prefix: &str = &listing.prefix;
        let mut out = Vec::new();
        let mut token: Option<String> = None;
        for _ in 0..MAX_LIST_PAGES {
            let resp = self.send("GET", self.list_url(prefix, token.as_deref()), &[], Vec::new()).await?;
            if resp.status != 200 {
                return Err(status_error(&resp));
            }
            let page = parse_list_page(&resp.body)?;
            for object in page.objects {
                let Some(key) = object.key.strip_prefix(KEY_PREFIX) else {
                    return Err(ProviderError::Permanent("R2 listing returned a key outside the prefix".to_owned()));
                };
                // Foreign objects no remote key can name are not ours.
                if is_remote_key(key) && listing.matches(key) {
                    out.push(ListedObject { key: key.to_owned(), provider_ref: key.to_owned(), size: object.size, etag: object.etag });
                }
            }
            if !page.truncated {
                return Ok(out);
            }
            match page.next_token {
                Some(next) if token.as_deref() != Some(next.as_str()) => token = Some(next),
                // A truncated page without a fresh token would end the
                // listing early and hide objects: never treat it as complete.
                _ => return Err(ProviderError::Permanent("R2 listing is truncated without a continuation token".to_owned())),
            }
        }
        Err(ProviderError::Permanent("R2 listing did not finish".to_owned()))
    }

    async fn get(&self, key: &str) -> Result<Vec<u8>, ProviderError> {
        check_key(key)?;
        let resp = self.send("GET", self.object_url(key), &[], Vec::new()).await?;
        match resp.status {
            200 => Ok(resp.body),
            _ => Err(status_error(&resp)),
        }
    }

    async fn put_create(&self, key: &str, bytes: &[u8]) -> Result<ListedObject, ProviderError> {
        check_key(key)?;
        check_object_size(bytes)?;
        let resp = self.send("PUT", self.object_url(key), &[("If-None-Match", "*")], bytes.to_vec()).await?;
        match resp.status {
            200 => Ok(self.listed(key, bytes.len() as u64, &resp)),
            412 => Err(ProviderError::AlreadyExists),
            // A concurrent conditional write: the outcome is unknown and the
            // caller retries; it is never assumed to be ours.
            409 => Err(ProviderError::Transient("R2 conditional write conflict".to_owned())),
            _ => Err(status_error(&resp)),
        }
    }

    async fn put_replace(&self, key: &str, bytes: &[u8], if_match: Option<&str>) -> Result<ListedObject, ProviderError> {
        check_key(key)?;
        check_object_size(bytes)?;
        let headers: Vec<(&str, &str)> = if_match.map(|etag| ("If-Match", etag)).into_iter().collect();
        let resp = self.send("PUT", self.object_url(key), &headers, bytes.to_vec()).await?;
        match resp.status {
            200 => Ok(self.listed(key, bytes.len() as u64, &resp)),
            412 => Err(ProviderError::PreconditionFailed),
            409 => Err(ProviderError::Transient("R2 conditional write conflict".to_owned())),
            _ => Err(status_error(&resp)),
        }
    }

    async fn delete(&self, key: &str) -> Result<(), ProviderError> {
        check_key(key)?;
        let resp = self.send("DELETE", self.object_url(key), &[], Vec::new()).await?;
        match resp.status {
            200 | 204 | 404 => Ok(()),
            _ => Err(status_error(&resp)),
        }
    }

    async fn quota(&self) -> Result<Quota, ProviderError> {
        Err(ProviderError::Unsupported("R2 has no quota API".to_owned()))
    }

    async fn ensure_container(&self) -> Result<(), ProviderError> {
        Ok(())
    }
}

/// Maps a non-success response. A 403 whose S3 error code is
/// `RequestTimeTooSkewed` is this device's clock, not a revoked credential:
/// retryable (`Transient`, A7) instead of a reconsent-free auth stop. The
/// error text carries only the fixed code, never the body.
fn status_error(resp: &HttpResponse) -> ProviderError {
    if resp.status == 403 && s3_error_code(&resp.body).as_deref() == Some("RequestTimeTooSkewed") {
        return ProviderError::Transient("RequestTimeTooSkewed".to_owned());
    }
    map_status(resp.status, &resp.headers)
}

/// The `<Error><Code>` text of an S3 error body, if it is one.
fn s3_error_code(body: &[u8]) -> Option<String> {
    let text = std::str::from_utf8(body).ok()?;
    let mut reader = NsReader::from_str(text);
    let mut path: Vec<Vec<u8>> = Vec::new();
    loop {
        match reader.read_event().ok()? {
            Event::Start(e) => path.push(e.local_name().as_ref().to_vec()),
            Event::End(_) => {
                path.pop();
            }
            Event::Text(t) if path.len() == 2 && path[0] == b"Error" && path[1] == b"Code" => {
                return t.xml10_content().ok().map(|s| s.trim().to_owned());
            }
            Event::Eof => return None,
            _ => {}
        }
    }
}

fn is_account_id(id: &str) -> bool {
    id.len() == 32 && id.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

/// S3 bucket naming rules: 3-63 of `[a-z0-9.-]`, alphanumeric at both ends,
/// no `..`, not an IPv4 address, no reserved prefix or suffix.
fn is_bucket_name(name: &str) -> bool {
    let bytes = name.as_bytes();
    (3..=63).contains(&bytes.len())
        && bytes.iter().all(|b| matches!(b, b'a'..=b'z' | b'0'..=b'9' | b'.' | b'-'))
        && bytes[0].is_ascii_alphanumeric()
        && bytes[bytes.len() - 1].is_ascii_alphanumeric()
        && !name.contains("..")
        && name.parse::<std::net::Ipv4Addr>().is_err()
        && !name.starts_with("xn--")
        && !name.starts_with("sthree-")
        && !name.ends_with("-s3alias")
        && !name.ends_with("--ol-s3")
}

#[derive(Debug, Default, PartialEq, Eq)]
struct S3Object {
    key: String,
    size: Option<u64>,
    etag: Option<String>,
}

#[derive(Debug, Default)]
struct ListPage {
    objects: Vec<S3Object>,
    truncated: bool,
    next_token: Option<String>,
}

fn bad_xml(what: &str) -> ProviderError {
    ProviderError::Permanent(format!("malformed R2 listing: {what}"))
}

/// A 200 whose body is not a ListObjectsV2 result (empty, an `<Error>`
/// document, a proxy's HTML page) says nothing about the bucket. It is never
/// an empty listing; the caller retries.
fn not_a_listing(what: &str) -> ProviderError {
    ProviderError::Transient(format!("R2 list response is not a ListBucketResult: {what}"))
}

/// Parses a ListObjectsV2 `ListBucketResult` by local element name. The root
/// must be `ListBucketResult` and carry exactly one `IsTruncated` whose trimmed
/// text is `true` or `false` (ASCII case-insensitive), or the page is refused:
/// any other value could end the listing early and hide later objects.
fn parse_list_page(body: &[u8]) -> Result<ListPage, ProviderError> {
    let text = std::str::from_utf8(body).map_err(|_| bad_xml("not UTF-8"))?;
    let mut reader = NsReader::from_str(text);
    let mut stack: Vec<String> = Vec::new();
    let mut page = ListPage::default();
    let mut current: Option<S3Object> = None;
    let mut buf = String::new();
    let mut saw_root = false;
    let mut saw_truncated = false;
    loop {
        match reader.read_event().map_err(|_| bad_xml("invalid XML"))? {
            Event::Start(e) => {
                let name = String::from_utf8_lossy(e.local_name().as_ref()).into_owned();
                // IsTruncated holds text only; markup inside it hides part of
                // its value, so the page is not a listing we can trust.
                if stack.len() == 2 && stack[1] == "IsTruncated" {
                    return Err(not_a_listing("IsTruncated contains markup"));
                }
                if stack.is_empty() {
                    if saw_root || name != "ListBucketResult" {
                        return Err(not_a_listing("unexpected root element"));
                    }
                    saw_root = true;
                }
                if name == "Contents" && stack.len() == 1 {
                    current = Some(S3Object::default());
                }
                stack.push(name);
                buf.clear();
            }
            // `<IsTruncated/>` states nothing; a self-closing duplicate must not
            // slip past the exactly-once check either.
            Event::Empty(e) if stack.len() == 1 && e.local_name().as_ref() == b"IsTruncated" => {
                return Err(not_a_listing("IsTruncated is neither true nor false"));
            }
            Event::Empty(_) if stack.len() == 2 && stack[1] == "IsTruncated" => {
                return Err(not_a_listing("IsTruncated contains markup"));
            }
            Event::End(_) => {
                let name = stack.pop().ok_or_else(|| bad_xml("unbalanced elements"))?;
                let value = buf.trim();
                match (stack.len(), name.as_str(), current.as_mut()) {
                    (2, "Key", Some(object)) => object.key = buf.clone(),
                    (2, "Size", Some(object)) => object.size = value.parse().ok(),
                    (2, "ETag", Some(object)) => object.etag = (!value.is_empty()).then(|| value.to_owned()),
                    (1, "Contents", _) => {
                        let object = current.take().unwrap_or_default();
                        if object.key.is_empty() {
                            return Err(bad_xml("object without a key"));
                        }
                        page.objects.push(object);
                    }
                    (1, "IsTruncated", _) => {
                        if saw_truncated {
                            return Err(not_a_listing("duplicate IsTruncated"));
                        }
                        saw_truncated = true;
                        // XML whitespace only (S 3.3.3): `str::trim` would also
                        // strip Unicode spaces such as U+00A0.
                        let value = buf.trim_matches(|c| matches!(c, ' ' | '\t' | '\n' | '\r'));
                        page.truncated = if value.eq_ignore_ascii_case("true") {
                            true
                        } else if value.eq_ignore_ascii_case("false") {
                            false
                        } else {
                            return Err(not_a_listing("IsTruncated is neither true nor false"));
                        };
                    }
                    (1, "NextContinuationToken", _) => page.next_token = (!value.is_empty()).then(|| value.to_owned()),
                    _ => {}
                }
                buf.clear();
            }
            Event::Text(t) => buf.push_str(&t.xml10_content().map_err(|_| bad_xml("bad text"))?),
            Event::CData(c) => buf.push_str(&c.decode().map_err(|_| bad_xml("bad CDATA"))?),
            Event::GeneralRef(r) => {
                if !push_reference(&mut buf, &r) {
                    return Err(bad_xml("unknown entity"));
                }
            }
            Event::Eof if stack.is_empty() => break,
            Event::Eof => return Err(bad_xml("truncated document")),
            _ => {}
        }
    }
    if !saw_root {
        return Err(not_a_listing("no root element"));
    }
    if !saw_truncated {
        return Err(not_a_listing("no IsTruncated"));
    }
    Ok(page)
}

/// An in-memory S3/R2 endpoint behind a [`MockTransport`]. It verifies each
/// request's SigV4 signature by re-signing it, honours If-None-Match and
/// If-Match, and pages ListObjectsV2 results.
#[cfg(test)]
pub(crate) mod tests_support {
    use std::collections::BTreeMap;
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, UNIX_EPOCH};

    use super::*;
    use crate::sync::provider::transport::MockTransport;

    pub(crate) const ACCOUNT: &str = "0123456789abcdef0123456789abcdef";
    pub(crate) const BUCKET: &str = "sayknow-vault";
    pub(crate) const ACCESS_KEY: &str = "R2TESTACCESSKEYID0000";
    pub(crate) const SECRET: &str = "r2-test-secret-access-key-0000000000000000000000000000000000000000";

    /// 2026-10-10T12:00:00Z.
    pub(crate) fn fixed_clock() -> SystemTime {
        UNIX_EPOCH + Duration::from_secs(1_791_633_600)
    }

    pub(crate) struct S3Server {
        /// Full object keys (with the `sayknow/` prefix) → (bytes, etag).
        pub objects: BTreeMap<String, (Vec<u8>, String)>,
        /// ListObjectsV2 page size (max-keys).
        pub page_size: usize,
        next_etag: u64,
    }

    impl S3Server {
        pub(crate) fn new() -> Self {
            Self { objects: BTreeMap::new(), page_size: 1000, next_etag: 0 }
        }

        pub(crate) fn handle(&mut self, req: &HttpRequest) -> HttpResponse {
            if !signature_valid(req) {
                return error(403, "SignatureDoesNotMatch");
            }
            let url = url::Url::parse(&req.url).unwrap();
            if url.host_str() != Some(&format!("{ACCOUNT}.r2.cloudflarestorage.com")) {
                return error(400, "InvalidHost");
            }
            let path = url.path().to_owned();
            let Some(rest) = path.strip_prefix(&format!("/{BUCKET}")) else { return error(404, "NoSuchBucket") };
            let query: BTreeMap<String, String> = url.query_pairs().into_owned().collect();
            match (req.method.as_str(), rest) {
                ("GET", "" | "/") if query.get("list-type").map(String::as_str) == Some("2") => self.list(&query),
                (method, key) => {
                    let key = key.trim_start_matches('/').to_owned();
                    if key.is_empty() {
                        return error(400, "InvalidRequest");
                    }
                    self.object(method, key, req)
                }
            }
        }

        fn object(&mut self, method: &str, key: String, req: &HttpRequest) -> HttpResponse {
            match method {
                "PUT" => {
                    let existing = self.objects.get(&key).map(|(_, etag)| etag.clone());
                    if req.header("if-none-match") == Some("*") && existing.is_some() {
                        return error(412, "PreconditionFailed");
                    }
                    if let Some(expected) = req.header("if-match") {
                        if existing.as_deref() != Some(expected) {
                            return error(412, "PreconditionFailed");
                        }
                    }
                    self.next_etag += 1;
                    let etag = format!("\"r{}\"", self.next_etag);
                    self.objects.insert(key, (req.body.clone(), etag.clone()));
                    HttpResponse { status: 200, headers: vec![("ETag".to_owned(), etag)], body: Vec::new() }
                }
                "GET" => match self.objects.get(&key) {
                    Some((bytes, etag)) => {
                        HttpResponse { status: 200, headers: vec![("ETag".to_owned(), etag.clone())], body: bytes.clone() }
                    }
                    None => error(404, "NoSuchKey"),
                },
                "DELETE" => {
                    self.objects.remove(&key);
                    HttpResponse { status: 204, headers: Vec::new(), body: Vec::new() }
                }
                _ => error(405, "MethodNotAllowed"),
            }
        }

        fn list(&self, query: &BTreeMap<String, String>) -> HttpResponse {
            let prefix = query.get("prefix").cloned().unwrap_or_default();
            let start_after = query.get("continuation-token").map(|t| {
                String::from_utf8(data_encoding::BASE64URL_NOPAD.decode(t.as_bytes()).unwrap()).unwrap()
            });
            let matching: Vec<_> = self
                .objects
                .iter()
                .filter(|(k, _)| k.starts_with(&prefix))
                .filter(|(k, _)| start_after.as_ref().is_none_or(|after| *k > after))
                .collect();
            let truncated = matching.len() > self.page_size;
            let page = &matching[..matching.len().min(self.page_size)];
            let mut xml = String::from(
                r#"<?xml version="1.0" encoding="UTF-8"?><ListBucketResult xmlns="http://s3.amazonaws.com/doc/2006-03-01/">"#,
            );
            xml.push_str(&format!("<Name>{BUCKET}</Name><Prefix>{prefix}</Prefix><KeyCount>{}</KeyCount>", page.len()));
            for (key, (bytes, etag)) in page {
                xml.push_str(&format!(
                    "<Contents><Key>{}</Key><Size>{}</Size><ETag>{}</ETag><StorageClass>STANDARD</StorageClass></Contents>",
                    quick_xml::escape::escape(key.as_str()),
                    bytes.len(),
                    quick_xml::escape::escape(etag.as_str())
                ));
            }
            xml.push_str(&format!("<IsTruncated>{truncated}</IsTruncated>"));
            if truncated {
                let last = page.last().unwrap().0;
                xml.push_str(&format!(
                    "<NextContinuationToken>{}</NextContinuationToken>",
                    data_encoding::BASE64URL_NOPAD.encode(last.as_bytes())
                ));
            }
            xml.push_str("</ListBucketResult>");
            HttpResponse { status: 200, headers: Vec::new(), body: xml.into_bytes() }
        }
    }

    /// Re-signs the request without its signature headers at the fixed clock
    /// and compares: any change to method, URL, signed headers or body fails.
    fn signature_valid(req: &HttpRequest) -> bool {
        let Some(given) = req.header("authorization").map(str::to_owned) else { return false };
        if req.header("x-amz-content-sha256") != Some(sigv4::sha256_hex(&req.body).as_str()) {
            return false;
        }
        let mut copy = req.clone();
        copy.headers.retain(|(n, _)| !n.eq_ignore_ascii_case("authorization"));
        let creds = Credentials { access_key: ACCESS_KEY.to_owned(), secret_key: SECRET.to_owned() };
        let payload = sigv4::sha256_hex(&copy.body);
        if sigv4::sign(&mut copy, &creds, REGION, SERVICE, fixed_clock(), &payload).is_err() {
            return false;
        }
        copy.header("authorization") == Some(given.as_str())
    }

    fn error(status: u16, code: &str) -> HttpResponse {
        let body = format!(r#"<?xml version="1.0" encoding="UTF-8"?><Error><Code>{code}</Code></Error>"#);
        HttpResponse { status, headers: Vec::new(), body: body.into_bytes() }
    }

    pub(crate) fn config() -> R2Config {
        R2Config {
            account_id: ACCOUNT.to_owned(),
            bucket: BUCKET.to_owned(),
            access_key_id: ACCESS_KEY.to_owned(),
            secret_access_key: SECRET.to_owned(),
        }
    }

    pub(crate) fn over(server: S3Server) -> (R2Store<MockTransport>, MockTransport, Arc<Mutex<S3Server>>) {
        let state = Arc::new(Mutex::new(server));
        let shared = Arc::clone(&state);
        let transport = MockTransport::new(move |req| shared.lock().unwrap().handle(req));
        let mut store = R2Store::new(config(), transport.clone()).unwrap();
        store.clock = fixed_clock;
        (store, transport, state)
    }

    /// The contract-suite backend.
    pub(crate) fn emulated() -> (R2Store<MockTransport>, MockTransport) {
        let (store, transport, _) = over(S3Server::new());
        (store, transport)
    }
}

#[cfg(test)]
mod tests {
    use super::tests_support::*;
    use super::*;
    use crate::sync::provider::transport::MockTransport;

    const SEG: &str = "v1/vault-a/log/dev-a/1-3.seg";

    #[test]
    fn config_validation() {
        let ok = config();
        assert!(R2Store::new(ok.clone(), MockTransport::new(|_| unreachable!())).is_ok());
        let bad_accounts = ["0123456789ABCDEF0123456789abcdef", "0123", "0123456789abcdef0123456789abcdeg", ""];
        for account_id in bad_accounts {
            let config = R2Config { account_id: account_id.to_owned(), ..ok.clone() };
            assert!(R2Store::new(config, MockTransport::new(|_| unreachable!())).is_err(), "{account_id}");
        }
        for bucket in ["ab", "Upper", "-lead", "trail-", "a..b", "192.168.1.1", "xn--abc", "a_b", "x-s3alias", &"a".repeat(64)] {
            let config = R2Config { bucket: bucket.to_owned(), ..ok.clone() };
            assert!(R2Store::new(config, MockTransport::new(|_| unreachable!())).is_err(), "{bucket}");
        }
        for bucket in ["abc", "my-bucket.2", &"a".repeat(63)] {
            let config = R2Config { bucket: bucket.to_owned(), ..ok.clone() };
            assert!(R2Store::new(config, MockTransport::new(|_| unreachable!())).is_ok(), "{bucket}");
        }
        let no_secret = R2Config { secret_access_key: String::new(), ..ok.clone() };
        assert!(R2Store::new(no_secret, MockTransport::new(|_| unreachable!())).is_err());
        let debug = format!("{ok:?}");
        assert!(!debug.contains(SECRET), "{debug}");
    }

    #[test]
    fn identity_names_account_bucket_and_endpoint() {
        let store = R2Store::new(config(), MockTransport::new(|_| unreachable!())).unwrap();
        assert_eq!(
            store.identity(),
            format!("r2|{ACCOUNT}|{BUCKET}|https://{ACCOUNT}.r2.cloudflarestorage.com")
        );
        assert!(store.create_only());
    }

    #[tokio::test]
    async fn signed_request_has_path_style_url_and_sigv4_headers() {
        let (store, transport, _) = over(S3Server::new());
        store.put_create(SEG, b"segment-bytes").await.unwrap();
        let req = &transport.requests()[0];
        assert_eq!(req.method, "PUT");
        assert_eq!(req.url, format!("https://{ACCOUNT}.r2.cloudflarestorage.com/{BUCKET}/sayknow/{SEG}"));
        assert_eq!(req.header("if-none-match"), Some("*"));
        assert_eq!(req.header("host"), Some(format!("{ACCOUNT}.r2.cloudflarestorage.com").as_str()));
        assert_eq!(req.header("x-amz-date"), Some("20261010T120000Z"));
        assert_eq!(req.header("x-amz-content-sha256"), Some(sigv4::sha256_hex(b"segment-bytes").as_str()));
        let auth = req.header("authorization").unwrap();
        assert!(
            auth.starts_with(&format!(
                "AWS4-HMAC-SHA256 Credential={ACCESS_KEY}/20261010/auto/s3/aws4_request, \
                 SignedHeaders=host;if-none-match;x-amz-content-sha256;x-amz-date, Signature="
            )),
            "{auth}"
        );
        assert!(!auth.contains(SECRET));
    }

    #[tokio::test]
    async fn emulator_rejects_a_tampered_signature() {
        let (store, transport, state) = over(S3Server::new());
        store.put_create(SEG, b"x").await.unwrap();
        let mut req = transport.requests()[0].clone();
        req.body = b"tampered".to_vec();
        assert_eq!(state.lock().unwrap().handle(&req).status, 403);
        let mut wrong_key = R2Store::new(R2Config { secret_access_key: "other".to_owned(), ..config() }, transport.clone()).unwrap();
        wrong_key.clock = fixed_clock;
        assert_eq!(wrong_key.get(SEG).await, Err(ProviderError::Auth { reconsent: false }));
    }

    #[tokio::test]
    async fn create_maps_412_to_already_exists_and_409_to_transient() {
        let (store, transport, state) = over(S3Server::new());
        store.put_create(SEG, b"ours").await.unwrap();
        assert_eq!(store.put_create(SEG, b"theirs").await, Err(ProviderError::AlreadyExists));
        transport.fail_next(409, &[]);
        let other = "v1/vault-a/log/dev-a/4-4.seg";
        assert!(matches!(store.put_create(other, b"x").await, Err(ProviderError::Transient(_))));
        let server = state.lock().unwrap();
        assert_eq!(server.objects[&format!("sayknow/{SEG}")].0, b"ours");
        assert!(!server.objects.contains_key(&format!("sayknow/{other}")));
    }

    /// A7: a 403 `RequestTimeTooSkewed` is this device's clock, retried as
    /// `Transient` on every operation; any other 403 stays an auth stop.
    #[tokio::test]
    async fn request_time_too_skewed_is_transient_and_other_403s_stay_auth() {
        let skewed = r#"<?xml version="1.0" encoding="UTF-8"?><Error><Code>RequestTimeTooSkewed</Code><Message>The difference between the request time and the server's time is too large.</Message></Error>"#;
        let denied = r#"<?xml version="1.0" encoding="UTF-8"?><Error><Code>AccessDenied</Code><Message>RequestTimeTooSkewed</Message></Error>"#;
        let (store, transport, _) = over(S3Server::new());
        let respond = |body: &str| Ok(HttpResponse { status: 403, headers: Vec::new(), body: body.as_bytes().to_vec() });
        transport.script_next(respond(skewed));
        assert_eq!(store.get(SEG).await, Err(ProviderError::Transient("RequestTimeTooSkewed".to_owned())));
        transport.script_next(respond(skewed));
        assert!(matches!(store.put_create(SEG, b"x").await, Err(ProviderError::Transient(_))));
        transport.script_next(respond(skewed));
        assert!(matches!(store.put_replace(SEG, b"x", None).await, Err(ProviderError::Transient(_))));
        transport.script_next(respond(skewed));
        assert!(matches!(store.delete(SEG).await, Err(ProviderError::Transient(_))));
        transport.script_next(respond(skewed));
        assert!(matches!(store.list(ListQuery::Prefix("v1/")).await, Err(ProviderError::Transient(_))));
        for body in [denied, "", "<Error><Code>", "not xml"] {
            transport.script_next(respond(body));
            assert_eq!(store.get(SEG).await, Err(ProviderError::Auth { reconsent: false }), "{body}");
        }
    }

    #[tokio::test]
    async fn list_follows_continuation_tokens() {
        let mut server = S3Server::new();
        server.page_size = 2;
        let (store, transport, state) = over(server);
        let keys: Vec<String> = (1..=5).map(|i| format!("v1/vault-a/log/dev-a/{i}-{i}.seg")).collect();
        for key in &keys {
            store.put_create(key, key.as_bytes()).await.unwrap();
        }
        store.put_create("v1/vault-b/devices/dev-a.rec", b"r").await.unwrap();
        state.lock().unwrap().objects.insert("sayknow/v1/vault-a/odd name".to_owned(), (vec![1], "\"x\"".to_owned()));
        state.lock().unwrap().objects.insert("other-app/file".to_owned(), (vec![1], "\"y\"".to_owned()));
        let before = transport.request_count();
        let listed = store.list(ListQuery::Prefix("v1/vault-a/")).await.unwrap();
        let mut got: Vec<String> = listed.iter().map(|o| o.key.clone()).collect();
        got.sort();
        let mut want = keys.clone();
        want.sort();
        assert_eq!(got, want);
        assert_eq!(listed[0].size, Some(listed[0].key.len() as u64));
        assert!(listed[0].etag.as_deref().is_some_and(|e| e.starts_with('"')));
        let pages: Vec<HttpRequest> = transport.requests()[before..].to_vec();
        assert_eq!(pages.len(), 3, "6 matches at 2 per page");
        assert!(pages[0].url.ends_with("?list-type=2&prefix=sayknow%2Fv1%2Fvault-a%2F"), "{}", pages[0].url);
        assert!(pages[1].url.contains("&continuation-token="));
    }

    #[tokio::test]
    async fn truncated_listing_without_token_is_an_error() {
        let (store, transport, _) = over(S3Server::new());
        let xml = r#"<ListBucketResult xmlns="http://s3.amazonaws.com/doc/2006-03-01/"><Contents><Key>sayknow/v1/a.seg</Key></Contents><IsTruncated>true</IsTruncated></ListBucketResult>"#;
        transport.script_next(Ok(HttpResponse { status: 200, headers: Vec::new(), body: xml.as_bytes().to_vec() }));
        assert!(matches!(store.list(ListQuery::Prefix("")).await, Err(ProviderError::Permanent(_))));
    }

    #[test]
    fn list_page_parses_entities_and_rejects_malformed_xml() {
        let xml = r#"<?xml version="1.0"?><ListBucketResult xmlns="http://s3.amazonaws.com/doc/2006-03-01/"><Contents><Key>sayknow/v1/a.seg</Key><Size>7</Size><ETag>&quot;abc&quot;</ETag></Contents><IsTruncated>false</IsTruncated></ListBucketResult>"#;
        let page = parse_list_page(xml.as_bytes()).unwrap();
        assert_eq!(page.objects, [S3Object { key: "sayknow/v1/a.seg".into(), size: Some(7), etag: Some("\"abc\"".into()) }]);
        assert!(!page.truncated);
        for bad in ["<ListBucketResult><Contents>", "<a></b>", "<L><Contents><Size>1</Size></Contents></L>"] {
            assert!(parse_list_page(bad.as_bytes()).is_err(), "{bad}");
        }
    }

    #[test]
    fn list_page_without_a_list_bucket_result_root_or_is_truncated_is_transient() {
        let bodies = [
            "",
            "hello",
            r#"<?xml version="1.0"?>"#,
            "<Error><Code>X</Code></Error>",
            "<html><body>proxy</body></html>",
            "<ListBucketResult/>",
            "<ListBucketResult><Contents><Key>sayknow/v1/a.seg</Key></Contents></ListBucketResult>",
            "<ListBucketResult><IsTruncated>false</IsTruncated></ListBucketResult><ListBucketResult><IsTruncated>false</IsTruncated></ListBucketResult>",
            "<Error><IsTruncated>false</IsTruncated></Error>",
        ];
        for body in bodies {
            let err = parse_list_page(body.as_bytes()).unwrap_err();
            assert!(matches!(err, ProviderError::Transient(_)), "{body}: {err:?}");
        }
        let minimal = parse_list_page(b"<ListBucketResult><IsTruncated>false</IsTruncated></ListBucketResult>").unwrap();
        assert!(minimal.objects.is_empty() && !minimal.truncated);

        for body in [
            "<ListBucketResult><IsTruncated>1</IsTruncated></ListBucketResult>",
            "<ListBucketResult><IsTruncated>0</IsTruncated></ListBucketResult>",
            "<ListBucketResult><IsTruncated></IsTruncated></ListBucketResult>",
            "<ListBucketResult><IsTruncated/></ListBucketResult>",
            "<ListBucketResult><IsTruncated>yes</IsTruncated></ListBucketResult>",
            "<ListBucketResult><IsTruncated>true false</IsTruncated></ListBucketResult>",
            "<ListBucketResult><IsTruncated>false</IsTruncated><IsTruncated>false</IsTruncated></ListBucketResult>",
            "<ListBucketResult><IsTruncated>true</IsTruncated><IsTruncated>false</IsTruncated></ListBucketResult>",
            "<ListBucketResult><IsTruncated>false</IsTruncated><IsTruncated/></ListBucketResult>",
        ] {
            let err = parse_list_page(body.as_bytes()).unwrap_err();
            assert!(matches!(err, ProviderError::Transient(_)), "{body}: {err:?}");
        }
        for (body, truncated) in [
            ("<ListBucketResult><IsTruncated> TRUE </IsTruncated><NextContinuationToken>T</NextContinuationToken></ListBucketResult>", true),
            ("<ListBucketResult><IsTruncated>\n\tFalse\n</IsTruncated></ListBucketResult>", false),
        ] {
            assert_eq!(parse_list_page(body.as_bytes()).unwrap().truncated, truncated, "{body}");
        }
    }

    #[tokio::test]
    async fn delete_quota_and_container() {
        let (store, transport, _) = over(S3Server::new());
        assert_eq!(store.delete(SEG).await, Ok(()));
        transport.fail_next(404, &[]);
        assert_eq!(store.delete(SEG).await, Ok(()));
        assert!(matches!(store.quota().await, Err(ProviderError::Unsupported(_))));
        let before = transport.request_count();
        store.ensure_container().await.unwrap();
        assert_eq!(transport.request_count(), before, "ensure_container sends nothing");
    }
}
