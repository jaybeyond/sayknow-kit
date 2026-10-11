//! Storage providers behind one object-store contract (§4.9).
//!
//! Every adapter stores opaque sealed bytes under the full remote key from
//! `envelope::ObjectAddress::remote_key`. Create-only writes never overwrite
//! (AC 18), every call goes through [`Bound`] so a provider whose identity no
//! longer matches the vault binding sends nothing (AC 11), and the resolver in
//! `publish` reaches a provider only through the [`SegmentRemote`] bridge.

use std::future::Future;
use std::time::Duration;

use data_encoding::HEXLOWER;
use sha2::{Digest, Sha256};

use super::envelope::{is_id, ObjectAddress, ObjectKind};
use super::publish::{PutOutcome, RemoteObject, SegmentRemote, TransportError};
use transport::{header_value, TransportFailure};

#[cfg(test)]
pub(crate) mod fake;
pub(crate) mod gdrive;
pub(crate) mod onedrive;
pub(crate) mod r2;
pub(crate) mod sigv4;
pub(crate) mod transport;
pub(crate) mod webdav;

/// A provider failure. Strings never carry a credential, a signature, or an
/// authenticated URL.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ProviderError {
    /// `reconsent`: the credentials are rejected (401) rather than lacking a
    /// permission (403); the user must sign in again.
    Auth { reconsent: bool },
    RateLimited { retry_after: Option<Duration> },
    QuotaExceeded,
    NotFound,
    AlreadyExists,
    PreconditionFailed,
    Unsupported(String),
    BindingMismatch,
    Transient(String),
    Permanent(String),
}

/// What to list.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ListQuery<'a> {
    /// Every object whose full key starts with this prefix, nested ones too.
    /// Drive has no name-prefix search and answers `Unsupported`.
    Prefix(&'a str),
    /// Every layout key (§4.2) matching each given field exactly. A manifest
    /// or blob has no device, so a query naming a device never matches one.
    Props { vault_id: Option<&'a str>, kind: Option<RemoteKind>, device_id: Option<&'a str> },
}

/// The kind of object a layout key (§4.2) names.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RemoteKind {
    Manifest,
    Rec,
    Seg,
    Snap,
    Blob,
}

impl RemoteKind {
    /// The `kind` text Drive stores in `appProperties`.
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Manifest => "manifest",
            Self::Rec => "rec",
            Self::Seg => "seg",
            Self::Snap => "snap",
            Self::Blob => "blob",
        }
    }
}

/// The Props a layout key carries. `device_id` is `None` for a manifest and
/// a blob.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct KeyProps {
    pub vault_id: String,
    pub kind: RemoteKind,
    pub device_id: Option<String>,
}

const MANIFEST_FILE: &str = "manifest.json";

/// Parses a full layout key (§4.2); anything else, including a malformed
/// segment range or blob shard, is `None`.
pub(crate) fn parse_layout_key(key: &str) -> Option<KeyProps> {
    let parts: Vec<&str> = key.split('/').collect();
    if let ["v1", vault_id, MANIFEST_FILE] = parts.as_slice() {
        return is_id(vault_id).then(|| KeyProps {
            vault_id: (*vault_id).to_owned(),
            kind: RemoteKind::Manifest,
            device_id: None,
        });
    }
    let address = ObjectAddress::from_remote_key(key, 0)?;
    let kind = match address.kind {
        ObjectKind::Segment => RemoteKind::Seg,
        ObjectKind::Snapshot => RemoteKind::Snap,
        ObjectKind::Blob => RemoteKind::Blob,
        ObjectKind::DeviceRecord => RemoteKind::Rec,
        ObjectKind::WrappedDek => return None,
    };
    let device_id = (!address.device_id.is_empty()).then_some(address.device_id);
    Some(KeyProps { vault_id: address.vault_id, kind, device_id })
}

/// Whether `key` is a layout key whose Props equal every field the query names.
pub(crate) fn props_match(
    key: &str,
    vault_id: Option<&str>,
    kind: Option<RemoteKind>,
    device_id: Option<&str>,
) -> bool {
    parse_layout_key(key).is_some_and(|props| {
        vault_id.is_none_or(|v| props.vault_id == v)
            && kind.is_none_or(|k| props.kind == k)
            && device_id.is_none_or(|d| props.device_id.as_deref() == Some(d))
    })
}

/// How a key-addressed store answers a query: list `prefix`, then keep only
/// the keys `matches` accepts.
pub(crate) struct KeyedListing<'a> {
    pub prefix: std::borrow::Cow<'a, str>,
    query: ListQuery<'a>,
}

impl KeyedListing<'_> {
    pub(crate) fn matches(&self, key: &str) -> bool {
        match self.query {
            ListQuery::Prefix(prefix) => key.starts_with(prefix),
            ListQuery::Props { vault_id, kind, device_id } => {
                key.starts_with(&*self.prefix) && props_match(key, vault_id, kind, device_id)
            }
        }
    }
}

/// Translates a query for WebDAV, R2 and the fake. Props become the exact
/// §4.2 prefix where one exists (a device's log, snapshots or record, a
/// vault's blobs); otherwise the nearest enclosing prefix, with `matches`
/// filtering exactly. Props ids outside the id alphabet are `Permanent`
/// before any request, so they can never widen or escape a prefix.
pub(crate) fn keyed_listing(q: ListQuery<'_>) -> Result<KeyedListing<'_>, ProviderError> {
    use std::borrow::Cow;
    let prefix = match q {
        ListQuery::Prefix(prefix) => {
            check_prefix(prefix)?;
            Cow::Borrowed(prefix)
        }
        ListQuery::Props { vault_id, kind, device_id } => {
            if vault_id.is_some_and(|v| !is_id(v)) || device_id.is_some_and(|d| !is_id(d)) {
                return Err(ProviderError::Permanent("unsafe list properties".to_owned()));
            }
            Cow::Owned(match (vault_id, kind, device_id) {
                (None, _, _) => "v1/".to_owned(),
                (Some(v), Some(RemoteKind::Seg), Some(d)) => format!("v1/{v}/log/{d}/"),
                (Some(v), Some(RemoteKind::Seg), None) => format!("v1/{v}/log/"),
                (Some(v), Some(RemoteKind::Snap), Some(d)) => format!("v1/{v}/snap/{d}/"),
                (Some(v), Some(RemoteKind::Snap), None) => format!("v1/{v}/snap/"),
                (Some(v), Some(RemoteKind::Rec), Some(d)) => format!("v1/{v}/devices/{d}.rec"),
                (Some(v), Some(RemoteKind::Rec), None) => format!("v1/{v}/devices/"),
                (Some(v), Some(RemoteKind::Blob), _) => format!("v1/{v}/blob/"),
                (Some(v), Some(RemoteKind::Manifest), _) => format!("v1/{v}/{MANIFEST_FILE}"),
                (Some(v), None, _) => format!("v1/{v}/"),
            })
        }
    };
    Ok(KeyedListing { prefix, query: q })
}

/// A source of OAuth access tokens. Implementations never log or format the
/// token; adapters put it only into an `Authorization` header.
pub(crate) trait AccessToken: Send + Sync {
    fn token(&self) -> impl Future<Output = Result<String, ProviderError>> + Send;
    /// Drops a cached token after a 401 so the next `token()` fetches anew.
    fn invalidate(&self);
}

/// Sends `req` with a bearer token from `tokens`; Drive and OneDrive share
/// it. A 401 invalidates the token and resends once with a fresh one; a
/// second 401 means the user must sign in again. The token goes only into the
/// `Authorization` header, never into an error.
pub(crate) async fn send_bearer<T: transport::HttpTransport, A: AccessToken>(
    transport: &T,
    tokens: &A,
    req: transport::HttpRequest,
) -> Result<transport::HttpResponse, ProviderError> {
    let first = send_with_token(transport, tokens, req.clone()).await?;
    if first.status != 401 {
        return Ok(first);
    }
    tokens.invalidate();
    let second = send_with_token(transport, tokens, req).await?;
    if second.status == 401 {
        return Err(ProviderError::Auth { reconsent: true });
    }
    Ok(second)
}

async fn send_with_token<T: transport::HttpTransport, A: AccessToken>(
    transport: &T,
    tokens: &A,
    mut req: transport::HttpRequest,
) -> Result<transport::HttpResponse, ProviderError> {
    let token = tokens.token().await?;
    req.headers.push(("Authorization".to_owned(), format!("Bearer {token}")));
    transport.send(req).await.map_err(ProviderError::from)
}

/// A test token source: hands out `tokens` in order, moving to the next one
/// only after `invalidate`, and staying on the last.
#[cfg(test)]
pub(crate) struct StaticToken {
    tokens: Vec<String>,
    index: std::sync::atomic::AtomicUsize,
    invalidations: std::sync::atomic::AtomicUsize,
}

#[cfg(test)]
impl StaticToken {
    pub(crate) fn new(token: &str) -> Self {
        Self::sequence(&[token])
    }

    pub(crate) fn sequence(tokens: &[&str]) -> Self {
        assert!(!tokens.is_empty(), "StaticToken needs a token");
        Self {
            tokens: tokens.iter().map(|t| (*t).to_owned()).collect(),
            index: std::sync::atomic::AtomicUsize::new(0),
            invalidations: std::sync::atomic::AtomicUsize::new(0),
        }
    }

    pub(crate) fn invalidations(&self) -> usize {
        self.invalidations.load(std::sync::atomic::Ordering::SeqCst)
    }
}

#[cfg(test)]
impl AccessToken for StaticToken {
    async fn token(&self) -> Result<String, ProviderError> {
        let index = self.index.load(std::sync::atomic::Ordering::SeqCst).min(self.tokens.len() - 1);
        Ok(self.tokens[index].clone())
    }

    fn invalidate(&self) {
        self.invalidations.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        self.index.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ListedObject {
    pub key: String,
    /// Names this exact copy: the Drive file id, elsewhere the key itself.
    pub provider_ref: String,
    pub size: Option<u64>,
    pub etag: Option<String>,
}

#[cfg_attr(not(test), allow(dead_code))] // only tests call this so far
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Quota {
    pub used: Option<u64>,
    pub available: Option<u64>,
}

#[cfg_attr(not(test), allow(dead_code))] // only tests call this so far
pub(crate) trait ObjectStore: Send + Sync {
    /// Whether `put_create` refuses an existing name (everywhere but Drive).
    fn create_only(&self) -> bool;
    /// Local, no network: canonical identity text (§4.7 fingerprint input).
    fn identity(&self) -> String;
    fn list(&self, q: ListQuery<'_>) -> impl Future<Output = Result<Vec<ListedObject>, ProviderError>> + Send;
    fn get(&self, key: &str) -> impl Future<Output = Result<Vec<u8>, ProviderError>> + Send;
    /// Never overwrites. Existing name → Err(AlreadyExists).
    fn put_create(&self, key: &str, bytes: &[u8]) -> impl Future<Output = Result<ListedObject, ProviderError>> + Send;
    /// Creates `key` with no existence pre-check, for the resolver's resend on
    /// a store that is not create-only (Drive), where a copy another writer
    /// holds must not stop ours from landing next to it. Defaults to
    /// `put_create`, which is already the whole answer on create-only stores.
    fn put_create_unchecked(
        &self,
        key: &str,
        bytes: &[u8],
    ) -> impl Future<Output = Result<ListedObject, ProviderError>> + Send {
        self.put_create(key, bytes)
    }
    fn put_replace(
        &self,
        key: &str,
        bytes: &[u8],
        if_match: Option<&str>,
    ) -> impl Future<Output = Result<ListedObject, ProviderError>> + Send;
    /// A missing object is already deleted: NotFound → Ok.
    fn delete(&self, key: &str) -> impl Future<Output = Result<(), ProviderError>> + Send;
    /// Every copy stored under exactly `key` (Drive allows several). The
    /// default serves keyed stores, which hold at most one: NotFound → empty.
    fn get_all(&self, key: &str) -> impl Future<Output = Result<Vec<RemoteObject>, ProviderError>> + Send {
        async move {
            match self.get(key).await {
                Ok(bytes) => Ok(vec![RemoteObject { provider_ref: key.to_owned(), bytes }]),
                Err(ProviderError::NotFound) => Ok(Vec::new()),
                Err(err) => Err(err),
            }
        }
    }
    /// Deletes the one copy `provider_ref` names. On a keyed store the
    /// reference is the key. A missing copy is already deleted.
    fn delete_ref(&self, provider_ref: &str) -> impl Future<Output = Result<(), ProviderError>> + Send {
        self.delete(provider_ref)
    }
    /// Overwrites the one copy `provider_ref` names (observed under `key`)
    /// and nothing else: no re-list, never a create. A copy that is gone is
    /// `NotFound` (D1a: a removed Drive record is never resurrected). Only
    /// stores that are not create-only (Drive) need it; the default refuses,
    /// so no adapter silently falls back to a racy check-then-put.
    fn put_replace_ref(
        &self,
        key: &str,
        provider_ref: &str,
        bytes: &[u8],
    ) -> impl Future<Output = Result<ListedObject, ProviderError>> + Send {
        let _ = (key, provider_ref, bytes);
        async { Err(ProviderError::Unsupported("no replace by reference".to_owned())) }
    }
    /// May be `Unsupported`.
    #[cfg_attr(not(test), allow(dead_code))] // only tests call this so far
    fn quota(&self) -> impl Future<Output = Result<Quota, ProviderError>> + Send;
    fn ensure_container(&self) -> impl Future<Output = Result<(), ProviderError>> + Send;
}

/// The binding fingerprint (§4.7): lowercase hex SHA-256 of the identity.
pub(crate) fn binding_fp(identity: &str) -> String {
    HEXLOWER.encode(&Sha256::digest(identity.as_bytes()))
}

/// A provider pinned to the identity the vault was bound to. Every call first
/// re-derives the fingerprint; on a mismatch it fails with `BindingMismatch`
/// and the inner store is never called, so nothing is sent (AC 11).
pub(crate) struct Bound<P> {
    inner: P,
    expected_fp: String,
}

impl<P: ObjectStore> Bound<P> {
    pub(crate) fn new(inner: P, expected_fp: String) -> Self {
        Self { inner, expected_fp }
    }

    #[cfg_attr(not(test), allow(dead_code))] // only tests call this so far
    pub(crate) fn inner(&self) -> &P {
        &self.inner
    }

    fn check(&self) -> Result<&P, ProviderError> {
        if binding_fp(&self.inner.identity()) == self.expected_fp {
            Ok(&self.inner)
        } else {
            Err(ProviderError::BindingMismatch)
        }
    }
}

impl<P: ObjectStore> ObjectStore for Bound<P> {
    fn create_only(&self) -> bool {
        self.inner.create_only()
    }

    fn identity(&self) -> String {
        self.inner.identity()
    }

    async fn list(&self, q: ListQuery<'_>) -> Result<Vec<ListedObject>, ProviderError> {
        self.check()?.list(q).await
    }

    async fn get(&self, key: &str) -> Result<Vec<u8>, ProviderError> {
        self.check()?.get(key).await
    }

    async fn put_create(&self, key: &str, bytes: &[u8]) -> Result<ListedObject, ProviderError> {
        let inner = self.check()?;
        check_object_size(bytes)?;
        inner.put_create(key, bytes).await
    }

    async fn put_create_unchecked(&self, key: &str, bytes: &[u8]) -> Result<ListedObject, ProviderError> {
        let inner = self.check()?;
        check_object_size(bytes)?;
        inner.put_create_unchecked(key, bytes).await
    }

    async fn put_replace(&self, key: &str, bytes: &[u8], if_match: Option<&str>) -> Result<ListedObject, ProviderError> {
        let inner = self.check()?;
        check_object_size(bytes)?;
        inner.put_replace(key, bytes, if_match).await
    }

    async fn put_replace_ref(&self, key: &str, provider_ref: &str, bytes: &[u8]) -> Result<ListedObject, ProviderError> {
        let inner = self.check()?;
        check_object_size(bytes)?;
        inner.put_replace_ref(key, provider_ref, bytes).await
    }

    async fn delete(&self, key: &str) -> Result<(), ProviderError> {
        self.check()?.delete(key).await
    }

    async fn get_all(&self, key: &str) -> Result<Vec<RemoteObject>, ProviderError> {
        self.check()?.get_all(key).await
    }

    async fn delete_ref(&self, provider_ref: &str) -> Result<(), ProviderError> {
        self.check()?.delete_ref(provider_ref).await
    }

    async fn quota(&self) -> Result<Quota, ProviderError> {
        self.check()?.quota().await
    }

    async fn ensure_container(&self) -> Result<(), ProviderError> {
        self.check()?.ensure_container().await
    }
}

/// The resolver's view of a bound store. Only a provider's definite answers
/// become outcomes; every other failure (including a binding mismatch) is
/// `TransportError`, which the resolver treats as "still unknown".
///
/// `get_all` goes through [`ObjectStore::get_all`], so every copy of a
/// duplicated Drive name reaches `publish`, which sees the fork. "No copy" is
/// only ever an `Ok` empty list from `get_all`; any error, a stray `NotFound`
/// included, stays unknown rather than claiming the key is free.
impl<P: ObjectStore> SegmentRemote for Bound<P> {
    fn create_only(&self) -> bool {
        self.inner.create_only()
    }

    async fn get_all(&self, key: &str) -> Result<Vec<RemoteObject>, TransportError> {
        ObjectStore::get_all(self, key).await.map_err(|_| TransportError)
    }

    /// On a store that is not create-only (Drive) the resend uploads with no
    /// pre-check: our bytes land next to another writer's copy, and the
    /// resolver reports the fork it then reads through `get_all`.
    async fn put_create(&self, key: &str, bytes: &[u8]) -> Result<PutOutcome, TransportError> {
        let result = if self.inner.create_only() {
            ObjectStore::put_create(self, key, bytes).await
        } else {
            ObjectStore::put_create_unchecked(self, key, bytes).await
        };
        match result {
            Ok(_) => Ok(PutOutcome::Created),
            Err(ProviderError::AlreadyExists) => Ok(PutOutcome::AlreadyExists),
            Err(_) => Err(TransportError),
        }
    }
}

/// The shared HTTP status mapping for WebDAV and R2. Call it only for a status
/// the adapter did not handle itself (2xx successes, adapter-specific 409).
/// A 409 that reaches here is `Permanent`: never assumed to be success.
pub(crate) fn map_status(status: u16, headers: &[(String, String)]) -> ProviderError {
    match status {
        401 => ProviderError::Auth { reconsent: true },
        403 => ProviderError::Auth { reconsent: false },
        404 => ProviderError::NotFound,
        412 => ProviderError::PreconditionFailed,
        429 | 503 => ProviderError::RateLimited { retry_after: retry_after(headers) },
        507 => ProviderError::QuotaExceeded,
        500..=599 => ProviderError::Transient(format!("HTTP {status}")),
        _ => ProviderError::Permanent(format!("HTTP {status}")),
    }
}

/// `Retry-After` in delta-seconds; an HTTP-date (or anything else) is `None`.
fn retry_after(headers: &[(String, String)]) -> Option<Duration> {
    let value = header_value(headers, "retry-after")?.trim();
    if value.is_empty() || !value.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    value.parse().ok().map(Duration::from_secs)
}

impl From<TransportFailure> for ProviderError {
    fn from(failure: TransportFailure) -> Self {
        match failure {
            TransportFailure::Timeout => ProviderError::Transient("request timed out".to_owned()),
            TransportFailure::Connect(text) => ProviderError::Transient(format!("connect failed: {text}")),
            TransportFailure::Tls(text) => ProviderError::Permanent(format!("TLS failed: {text}")),
            TransportFailure::Other(text) => ProviderError::Transient(text),
            TransportFailure::BodyTooLarge => ProviderError::Permanent("response body exceeds cap".to_owned()),
        }
    }
}

/// A full remote key adapters may send: `/`-joined non-empty segments of
/// `[A-Za-z0-9._-]`, no leading `/`, and no `..` anywhere.
pub(crate) fn is_remote_key(key: &str) -> bool {
    !key.is_empty() && !key.contains("..") && key.split('/').all(is_safe_segment)
}

fn is_safe_segment(segment: &str) -> bool {
    !segment.is_empty()
        && segment != "."
        && segment.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
}

/// Refuses an unsafe key before any request is built.
pub(crate) fn check_key(key: &str) -> Result<(), ProviderError> {
    if is_remote_key(key) {
        Ok(())
    } else {
        Err(ProviderError::Permanent("unsafe remote key".to_owned()))
    }
}

/// Exclusive upper bound on an uploaded object body, equal to the transport's
/// response cap: a body at or above it could be uploaded, but every later read
/// would fail `Permanent`, leaving an object that can never be read (AF-1).
pub(crate) const MAX_OBJECT_BYTES: usize = transport::DEFAULT_MAX_BODY;

/// Refuses an upload body at or above [`MAX_OBJECT_BYTES`] before any request
/// is built. [`Bound`] and every adapter's `put_create`/`put_replace` call it.
pub(crate) fn check_object_size(bytes: &[u8]) -> Result<(), ProviderError> {
    if bytes.len() < MAX_OBJECT_BYTES {
        Ok(())
    } else {
        Err(ProviderError::Permanent("object exceeds size limit".to_owned()))
    }
}

/// A list prefix: empty (everything), or a safe key that may end in `/`.
pub(crate) fn check_prefix(prefix: &str) -> Result<(), ProviderError> {
    let body = prefix.strip_suffix('/').unwrap_or(prefix);
    if prefix.is_empty() || is_remote_key(body) {
        Ok(())
    } else {
        Err(ProviderError::Permanent("unsafe remote prefix".to_owned()))
    }
}

#[cfg(test)]
mod tests {
    use super::fake::{FakeStore, Op};
    use super::*;
    use crate::sync::envelope::ObjectAddress;
    use crate::sync::publish::{settle_attempted, Outcome, Pause, Verdict};

    const SEG: &str = "v1/vault-a/log/dev-a/000000000001-000000000003.seg";
    const OURS: &[u8] = b"sealed segment bytes";
    const THEIRS: &[u8] = b"someone else's segment";

    fn headers(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
        pairs.iter().map(|(k, v)| ((*k).to_owned(), (*v).to_owned())).collect()
    }

    fn transient(err: &ProviderError) -> bool {
        matches!(err, ProviderError::Transient(_))
    }

    fn permanent(err: &ProviderError) -> bool {
        matches!(err, ProviderError::Permanent(_))
    }

    /// The contract every backend honours (design § "Shared contract suite").
    /// `requests` counts requests the backend received; `fail_next(status,
    /// headers)` makes the backend answer its next request with that HTTP
    /// status (one `get` is one request on every backend).
    async fn contract_suite<P: ObjectStore>(
        p: &P,
        requests: impl Fn() -> usize,
        fail_next: impl Fn(u16, &[(&str, &str)]),
    ) {
        // 1. ensure_container is idempotent.
        p.ensure_container().await.unwrap();
        p.ensure_container().await.unwrap();

        // 2. put_create creates once and never overwrites (AC 18).
        let created = p.put_create(SEG, OURS).await.unwrap();
        assert_eq!(created.key, SEG);
        assert_eq!(p.put_create(SEG, THEIRS).await, Err(ProviderError::AlreadyExists));
        assert_eq!(p.get(SEG).await.unwrap(), OURS, "a refused create must leave the bytes unchanged");

        // 3. get returns the exact bytes; a missing key is NotFound.
        let binary: Vec<u8> = (0..=255).collect();
        let blob = "v1/vault-a/blob/ab/ab12.blob";
        p.put_create(blob, &binary).await.unwrap();
        assert_eq!(p.get(blob).await.unwrap(), binary);
        assert_eq!(p.get("v1/vault-a/log/dev-a/missing.seg").await, Err(ProviderError::NotFound));

        // 4. list(Prefix) returns exactly the keys under the prefix, nested
        // ones included, and never the container or probe entries.
        let nested = "v1/vault-a/log/dev-b/000000000001-000000000001.seg";
        let other_vault = "v1/vault-b/log/dev-a/000000000001-000000000001.seg";
        p.put_create(nested, b"n").await.unwrap();
        p.put_create(other_vault, b"o").await.unwrap();
        let keys = |listed: Vec<ListedObject>| {
            let mut keys: Vec<String> = listed.into_iter().map(|o| o.key).collect();
            keys.sort();
            keys
        };
        let log = p.list(ListQuery::Prefix("v1/vault-a/log/")).await.unwrap();
        for object in &log {
            if object.key == SEG {
                assert!(object.size.is_none_or(|size| size == OURS.len() as u64), "{object:?}");
            }
        }
        assert_eq!(keys(log), [SEG, nested]);
        let all = keys(p.list(ListQuery::Prefix("")).await.unwrap());
        assert_eq!(all, [blob, SEG, nested, other_vault]);
        assert!(keys(p.list(ListQuery::Prefix("v1/vault-c/")).await.unwrap()).is_empty());

        // 5. put_replace honours If-Match.
        let rec = "v1/vault-a/devices/dev-a.rec";
        let first = p.put_replace(rec, b"rec-1", None).await.unwrap();
        let etag = match first.etag {
            Some(etag) => etag,
            None => p.list(ListQuery::Prefix(rec)).await.unwrap()[0].etag.clone().expect("etag"),
        };
        assert_eq!(p.put_replace(rec, b"rec-x", Some("\"stale-etag\"")).await, Err(ProviderError::PreconditionFailed));
        assert_eq!(p.get(rec).await.unwrap(), b"rec-1");
        p.put_replace(rec, b"rec-2", Some(&etag)).await.unwrap();
        assert_eq!(p.get(rec).await.unwrap(), b"rec-2");
        assert_eq!(p.put_replace(rec, b"rec-3", Some(&etag)).await, Err(ProviderError::PreconditionFailed));
        assert_eq!(p.get(rec).await.unwrap(), b"rec-2");

        // 6. delete succeeds, and deleting again is Ok.
        p.delete(nested).await.unwrap();
        assert_eq!(p.get(nested).await, Err(ProviderError::NotFound));
        p.delete(nested).await.unwrap();

        // 7. An unsafe key is Permanent and sends nothing.
        let before = requests();
        for key in ["", "/v1/a.seg", "v1/../a.seg", "v1/a..b", "v1//a.seg", "v1/a.seg/", "v1/./a", "v1/a b", "v1/a%2Fb", "v1\\a"] {
            assert!(permanent(&p.get(key).await.unwrap_err()), "get {key:?}");
            assert!(permanent(&p.put_create(key, b"x").await.unwrap_err()), "put_create {key:?}");
            assert!(permanent(&p.put_replace(key, b"x", None).await.unwrap_err()), "put_replace {key:?}");
            assert!(permanent(&p.delete(key).await.unwrap_err()), "delete {key:?}");
        }
        for prefix in ["/v1/", "v1/../", "v1//", "v1/a b"] {
            assert!(permanent(&p.list(ListQuery::Prefix(prefix)).await.unwrap_err()), "list {prefix:?}");
        }
        assert_eq!(requests(), before, "an unsafe key must send no request");

        // 8. Fault mapping.
        type Case<'a> = (u16, &'a [(&'a str, &'a str)], fn(&ProviderError) -> bool);
        let cases: [Case; 7] = [
            (401, &[], |e| *e == ProviderError::Auth { reconsent: true }),
            (403, &[], |e| *e == ProviderError::Auth { reconsent: false }),
            (429, &[("Retry-After", "12")], |e| *e == ProviderError::RateLimited { retry_after: Some(Duration::from_secs(12)) }),
            (503, &[], |e| *e == ProviderError::RateLimited { retry_after: None }),
            (507, &[], |e| *e == ProviderError::QuotaExceeded),
            (500, &[], transient),
            (400, &[], permanent),
        ];
        for (status, hdrs, expected) in cases {
            fail_next(status, hdrs);
            let err = p.get(SEG).await.unwrap_err();
            assert!(expected(&err), "HTTP {status} mapped to {err:?}");
        }
        assert_eq!(p.get(SEG).await.unwrap(), OURS, "faults must not disturb stored objects");
    }

    /// For the fake, an HTTP fault is the error `map_status` gives for it.
    fn fake_fail_next(store: &FakeStore) -> impl Fn(u16, &[(&str, &str)]) + '_ {
        |status, hdrs| store.fail_next(Op::Get, map_status(status, &headers(hdrs)))
    }

    #[tokio::test]
    async fn contract_suite_fake() {
        let store = FakeStore::new(true);
        contract_suite(&store, || store.requests(), fake_fail_next(&store)).await;
    }

    #[tokio::test]
    async fn contract_suite_fake_through_bound() {
        let store = FakeStore::new(true);
        let bound = Bound::new(store, binding_fp("fake|default"));
        let inner = bound.inner();
        contract_suite(&bound, || inner.requests(), fake_fail_next(inner)).await;
    }

    #[tokio::test]
    async fn contract_suite_webdav() {
        let (store, transport) = super::webdav::tests_support::emulated();
        contract_suite(&store, || transport.request_count(), |status, hdrs| transport.fail_next(status, hdrs)).await;
    }

    #[tokio::test]
    async fn contract_suite_r2() {
        let (store, transport) = super::r2::tests_support::emulated();
        contract_suite(&store, || transport.request_count(), |status, hdrs| transport.fail_next(status, hdrs)).await;
    }

    const M_A: &str = "v1/vault-a/manifest.json";
    const M_B: &str = "v1/vault-b/manifest.json";
    const REC_A: &str = "v1/vault-a/devices/dev-a.rec";
    const REC_B: &str = "v1/vault-a/devices/dev-b.rec";
    const SEG_A2: &str = "v1/vault-a/log/dev-a/000000000004-000000000009.seg";
    const SEG_AB: &str = "v1/vault-a/log/dev-ab/000000000001-000000000001.seg";
    const SEG_B: &str = "v1/vault-a/log/dev-b/000000000001-000000000002.seg";
    const SEG_VB: &str = "v1/vault-b/log/dev-a/000000000001-000000000001.seg";
    const SNAP_A: &str = "v1/vault-a/snap/dev-a/snap-9.snap";
    const BLOB_1: &str = "v1/vault-a/blob/ab/ab12.blob";
    const BLOB_2: &str = "v1/vault-a/blob/cd/cd34.blob";
    const BLOB_VB: &str = "v1/vault-b/blob/ab/ab12.blob";
    /// Safe keys under the matching prefixes that are not layout keys.
    const DECOYS: [&str; 5] = [
        "v1/vault-a/manifest.json.bak",
        "v1/vault-a/devices/dev-a.rec.old",
        "v1/vault-a/log/dev-a/notes.txt",
        "v1/vault-a/blob/zz/ab12.blob",
        "v1/vault-a/blob/ab/cd34.blob",
    ];

    /// ADV-3: `ListQuery::Props` on a key-addressed store lists exactly the
    /// layout keys whose Props match, never a decoy that only shares a
    /// prefix, never another vault or device; plus the `get_all` and
    /// `delete_ref` defaults.
    async fn props_suite<P: ObjectStore>(p: &P, requests: impl Fn() -> usize) {
        let layout = [M_A, M_B, REC_A, REC_B, SEG, SEG_A2, SEG_AB, SEG_B, SEG_VB, SNAP_A, BLOB_1, BLOB_2, BLOB_VB];
        for key in layout.iter().chain(&DECOYS) {
            p.put_create(key, key.as_bytes()).await.unwrap();
        }
        let list = |vault_id, kind, device_id| async move {
            let listed = p.list(ListQuery::Props { vault_id, kind, device_id }).await.unwrap();
            for object in &listed {
                assert_eq!(object.provider_ref, object.key, "keyed stores name a copy by its key");
            }
            let mut keys: Vec<String> = listed.into_iter().map(|o| o.key).collect();
            keys.sort();
            keys
        };
        use RemoteKind::*;

        // Manifest discovery across vaults: both vaults, no decoy.
        assert_eq!(list(None, Some(Manifest), None).await, [M_A, M_B]);
        assert_eq!(list(Some("vault-b"), Some(Manifest), None).await, [M_B]);
        assert!(list(Some("vault-c"), Some(Manifest), None).await.is_empty());

        // Per-device logs: dev-a never picks up dev-ab, another vault, or a stray file.
        assert_eq!(list(Some("vault-a"), Some(Seg), Some("dev-a")).await, [SEG, SEG_A2]);
        assert_eq!(list(Some("vault-a"), Some(Seg), Some("dev-ab")).await, [SEG_AB]);
        assert_eq!(list(Some("vault-a"), Some(Seg), None).await, [SEG, SEG_A2, SEG_AB, SEG_B]);
        assert_eq!(list(None, Some(Seg), Some("dev-a")).await, [SEG, SEG_A2, SEG_VB]);
        assert_eq!(list(Some("vault-a"), Some(Snap), Some("dev-a")).await, [SNAP_A]);
        assert!(list(Some("vault-a"), Some(Snap), Some("dev-b")).await.is_empty());

        // Blobs: only well-sharded ones of the vault; a blob has no device.
        assert_eq!(list(Some("vault-a"), Some(Blob), None).await, [BLOB_1, BLOB_2]);
        assert_eq!(list(None, Some(Blob), None).await, [BLOB_1, BLOB_2, BLOB_VB]);
        assert!(list(Some("vault-a"), Some(Blob), Some("dev-a")).await.is_empty());
        assert!(list(Some("vault-a"), Some(Manifest), Some("dev-a")).await.is_empty());

        // Recs: one device's record exactly, or every record of the vault.
        assert_eq!(list(Some("vault-a"), Some(Rec), Some("dev-a")).await, [REC_A]);
        assert_eq!(list(Some("vault-a"), Some(Rec), None).await, [REC_A, REC_B]);
        assert!(list(Some("vault-b"), Some(Rec), None).await.is_empty());

        // No kind: everything the vault (or device) holds, decoys excluded.
        assert_eq!(list(Some("vault-b"), None, None).await, [BLOB_VB, SEG_VB, M_B]);
        assert_eq!(list(Some("vault-a"), None, Some("dev-b")).await, [REC_B, SEG_B]);
        let mut every = layout.to_vec();
        every.sort();
        assert_eq!(list(None, None, None).await, every);

        // Unsafe ids are Permanent before any request: they can neither
        // widen a prefix nor leave the vault.
        let before = requests();
        for (vault_id, device_id) in [
            (Some(""), None),
            (Some(".."), None),
            (Some("vault-a/log"), None),
            (Some("vault-*"), None),
            (Some("vault-a"), Some("")),
            (Some("vault-a"), Some("dev-a/../dev-b")),
            (None, Some("-dev")),
        ] {
            let result = p.list(ListQuery::Props { vault_id, kind: Some(Seg), device_id }).await;
            assert!(permanent(&result.unwrap_err()), "{vault_id:?} {device_id:?}");
        }
        assert_eq!(requests(), before, "unsafe Props must send no request");

        // get_all / delete_ref defaults on a keyed store.
        let copies = p.get_all(SEG).await.unwrap();
        assert_eq!(copies, [RemoteObject { provider_ref: SEG.to_owned(), bytes: SEG.as_bytes().to_vec() }]);
        assert_eq!(p.get_all("v1/vault-a/log/dev-a/000000000010-000000000011.seg").await, Ok(Vec::new()));
        assert!(permanent(&p.get_all("v1/../a.seg").await.unwrap_err()));
        p.delete_ref(&copies[0].provider_ref).await.unwrap();
        assert_eq!(p.get_all(SEG).await, Ok(Vec::new()));
        p.delete_ref(SEG).await.unwrap();
        assert_eq!(list(Some("vault-a"), Some(Seg), Some("dev-a")).await, [SEG_A2]);
    }

    #[tokio::test]
    async fn props_suite_fake() {
        let store = FakeStore::new(true);
        props_suite(&store, || store.requests()).await;
    }

    #[tokio::test]
    async fn props_suite_fake_through_bound() {
        let bound = bound(FakeStore::new(true));
        props_suite(&bound, || bound.inner().requests()).await;
    }

    #[tokio::test]
    async fn props_suite_webdav() {
        let (store, transport) = super::webdav::tests_support::emulated();
        props_suite(&store, || transport.request_count()).await;
    }

    #[tokio::test]
    async fn props_suite_r2() {
        let (store, transport) = super::r2::tests_support::emulated();
        props_suite(&store, || transport.request_count()).await;
    }

    /// Props become the exact §4.2 prefix where one exists, else the nearest.
    #[test]
    fn props_translate_to_layout_prefixes() {
        use RemoteKind::*;
        let prefix = |vault_id, kind, device_id| {
            keyed_listing(ListQuery::Props { vault_id, kind, device_id }).unwrap().prefix.into_owned()
        };
        assert_eq!(prefix(Some("v"), Some(Seg), Some("d")), "v1/v/log/d/");
        assert_eq!(prefix(Some("v"), Some(Snap), Some("d")), "v1/v/snap/d/");
        assert_eq!(prefix(Some("v"), Some(Rec), Some("d")), "v1/v/devices/d.rec");
        assert_eq!(prefix(Some("v"), Some(Rec), None), "v1/v/devices/");
        assert_eq!(prefix(Some("v"), Some(Blob), None), "v1/v/blob/");
        assert_eq!(prefix(Some("v"), Some(Manifest), None), "v1/v/manifest.json");
        assert_eq!(prefix(Some("v"), None, Some("d")), "v1/v/");
        assert_eq!(prefix(None, Some(Manifest), None), "v1/");
        assert_eq!(keyed_listing(ListQuery::Prefix("v1/x/")).unwrap().prefix, "v1/x/");
        assert!(permanent(&keyed_listing(ListQuery::Prefix("/v1/")).err().unwrap()));
    }

    #[test]
    fn layout_keys_parse_to_props() {
        let props = |vault: &str, kind, device: Option<&str>| {
            Some(KeyProps { vault_id: vault.to_owned(), kind, device_id: device.map(str::to_owned) })
        };
        assert_eq!(parse_layout_key(M_A), props("vault-a", RemoteKind::Manifest, None));
        assert_eq!(parse_layout_key(REC_B), props("vault-a", RemoteKind::Rec, Some("dev-b")));
        assert_eq!(parse_layout_key(SEG_VB), props("vault-b", RemoteKind::Seg, Some("dev-a")));
        assert_eq!(parse_layout_key(SNAP_A), props("vault-a", RemoteKind::Snap, Some("dev-a")));
        assert_eq!(parse_layout_key(BLOB_1), props("vault-a", RemoteKind::Blob, None));
        for key in DECOYS.iter().chain(&["", "v1/manifest.json", "v1/../manifest.json", "v2/vault-a/manifest.json", ".probe/x"]) {
            assert_eq!(parse_layout_key(key), None, "{key}");
        }
        let kinds = [RemoteKind::Manifest, RemoteKind::Rec, RemoteKind::Seg, RemoteKind::Snap, RemoteKind::Blob];
        assert_eq!(kinds.map(RemoteKind::as_str), ["manifest", "rec", "seg", "snap", "blob"]);
    }

    /// ADV-3: with duplicate names (Drive mode) the bridge hands every copy
    /// to `publish`, which sees the fork; `delete_ref` removes one copy.
    #[tokio::test]
    async fn bridge_passes_every_duplicate_copy_to_publish() {
        let remote = bound(FakeStore::new(false));
        assert!(!SegmentRemote::create_only(&remote));
        remote.inner().seed(SEG, THEIRS);
        let settled = settle_attempted(&remote, &NoPause, SEG, OURS, judge).await;
        assert_eq!(settled.outcome, Outcome::PublishedWithFork);
        let copies = SegmentRemote::get_all(&remote, SEG).await.unwrap();
        assert_eq!(copies.iter().map(|c| c.bytes.as_slice()).collect::<Vec<_>>(), [THEIRS, OURS]);
        assert_ne!(copies[0].provider_ref, copies[1].provider_ref);

        ObjectStore::delete_ref(&remote, &copies[0].provider_ref).await.unwrap();
        assert_eq!(remote.inner().copies(SEG), vec![OURS.to_vec()]);
        ObjectStore::delete_ref(&remote, &copies[0].provider_ref).await.unwrap();
        assert_eq!(remote.inner().copies(SEG), vec![OURS.to_vec()]);
    }

    /// A stray `NotFound` from `get_all` is never read as "no copy".
    #[tokio::test]
    async fn bridge_keeps_get_all_errors_unknown() {
        struct Missing;
        impl ObjectStore for Missing {
            fn create_only(&self) -> bool {
                false
            }
            fn identity(&self) -> String {
                "missing".to_owned()
            }
            async fn list(&self, _: ListQuery<'_>) -> Result<Vec<ListedObject>, ProviderError> {
                Ok(Vec::new())
            }
            async fn get(&self, _: &str) -> Result<Vec<u8>, ProviderError> {
                Err(ProviderError::NotFound)
            }
            async fn get_all(&self, _: &str) -> Result<Vec<RemoteObject>, ProviderError> {
                Err(ProviderError::NotFound)
            }
            async fn put_create(&self, _: &str, _: &[u8]) -> Result<ListedObject, ProviderError> {
                Err(ProviderError::Transient("down".to_owned()))
            }
            async fn put_replace(&self, _: &str, _: &[u8], _: Option<&str>) -> Result<ListedObject, ProviderError> {
                Err(ProviderError::Transient("down".to_owned()))
            }
            async fn delete(&self, _: &str) -> Result<(), ProviderError> {
                Ok(())
            }
            async fn quota(&self) -> Result<Quota, ProviderError> {
                Err(ProviderError::Unsupported("quota".to_owned()))
            }
            async fn ensure_container(&self) -> Result<(), ProviderError> {
                Ok(())
            }
        }
        let remote = Bound::new(Missing, binding_fp("missing"));
        assert_eq!(SegmentRemote::get_all(&remote, SEG).await, Err(TransportError));
    }

    #[tokio::test]
    async fn static_token_moves_on_only_after_invalidate() {
        let tokens = StaticToken::sequence(&["t1", "t2"]);
        assert_eq!(tokens.token().await.unwrap(), "t1");
        assert_eq!(tokens.token().await.unwrap(), "t1");
        tokens.invalidate();
        assert_eq!(tokens.token().await.unwrap(), "t2");
        tokens.invalidate();
        assert_eq!(tokens.token().await.unwrap(), "t2");
        assert_eq!(tokens.invalidations(), 2);
        assert_eq!(StaticToken::new("only").token().await.unwrap(), "only");
    }

    #[test]
    fn map_status_table() {
        let none = headers(&[]);
        assert_eq!(map_status(401, &none), ProviderError::Auth { reconsent: true });
        assert_eq!(map_status(403, &none), ProviderError::Auth { reconsent: false });
        assert_eq!(map_status(404, &none), ProviderError::NotFound);
        assert_eq!(map_status(412, &none), ProviderError::PreconditionFailed);
        assert_eq!(map_status(507, &none), ProviderError::QuotaExceeded);
        assert_eq!(
            map_status(429, &headers(&[("retry-after", " 30 ")])),
            ProviderError::RateLimited { retry_after: Some(Duration::from_secs(30)) }
        );
        assert_eq!(
            map_status(503, &headers(&[("Retry-After", "120")])),
            ProviderError::RateLimited { retry_after: Some(Duration::from_secs(120)) }
        );
        for ignored in ["Wed, 21 Oct 2015 07:28:00 GMT", "-5", "1.5", "", "99999999999999999999999"] {
            assert_eq!(
                map_status(429, &headers(&[("Retry-After", ignored)])),
                ProviderError::RateLimited { retry_after: None },
                "{ignored:?}"
            );
        }
        assert_eq!(map_status(503, &none), ProviderError::RateLimited { retry_after: None });
        for status in [500, 502, 504, 599] {
            assert!(transient(&map_status(status, &none)), "{status}");
        }
        // 409 is adapter-specific; one that reaches the shared table is never success.
        for status in [400, 405, 409, 410, 413, 422, 302, 304, 100] {
            assert!(permanent(&map_status(status, &none)), "{status}");
        }
    }

    #[test]
    fn transport_failures_map_to_transient_except_tls() {
        assert!(transient(&TransportFailure::Timeout.into()));
        assert!(transient(&TransportFailure::Connect("refused".into()).into()));
        assert!(transient(&TransportFailure::Other("reset".into()).into()));
        assert!(permanent(&TransportFailure::Tls("bad certificate".into()).into()));
    }

    #[test]
    fn remote_key_safety() {
        let addresses = [
            ObjectAddress::segment("vault-a", 1, "dev-a", 1, 3),
            ObjectAddress::snapshot("vault-a", 1, "dev-a", "snap-9"),
            ObjectAddress::device_record("vault-a", 1, "dev-a"),
        ];
        for address in addresses {
            let key = address.remote_key().unwrap();
            assert!(is_remote_key(&key), "{key}");
        }
        assert!(is_remote_key("v1/vault-a/manifest.json"));
        assert!(is_remote_key(".probe/0b9c"));
        for bad in ["", "/a", "a/", "a//b", "..", "a/../b", "a/..b", "./a", "a/.", "a b", "a%2e", "ä", "a\\b", "a?b", "a#b"] {
            assert!(!is_remote_key(bad), "{bad:?}");
        }
        assert_eq!(check_prefix(""), Ok(()));
        assert_eq!(check_prefix("v1/vault-a/"), Ok(()));
        assert_eq!(check_prefix("v1/vault-a/log/dev-a/0000"), Ok(()));
        assert!(permanent(&check_prefix("/").unwrap_err()));
        assert!(permanent(&check_prefix("v1//").unwrap_err()));
    }

    #[test]
    fn binding_fp_is_lowercase_hex_sha256() {
        // SHA-256("abc"), FIPS 180-2 appendix B.1.
        assert_eq!(binding_fp("abc"), "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad");
        assert_ne!(binding_fp("webdav|https://a/|u"), binding_fp("webdav|https://a/|v"));
    }

    #[tokio::test]
    async fn bound_mismatch_refuses_every_op_with_zero_requests() {
        let store = FakeStore::new(true);
        store.set_identity("r2|acct|bucket|https://acct.r2.cloudflarestorage.com");
        let bound = Bound::new(store, binding_fp("r2|acct|other-bucket|https://acct.r2.cloudflarestorage.com"));
        let mismatch = Err(ProviderError::BindingMismatch);
        assert_eq!(bound.list(ListQuery::Prefix("")).await, mismatch.clone().map(|()| Vec::new()));
        assert_eq!(bound.get(SEG).await, mismatch.clone().map(|()| Vec::new()));
        assert_eq!(ObjectStore::put_create(&bound, SEG, OURS).await.map(drop), mismatch);
        assert_eq!(bound.put_replace(SEG, OURS, None).await.map(drop), mismatch);
        assert_eq!(bound.delete(SEG).await, mismatch);
        assert_eq!(bound.quota().await.map(drop), mismatch);
        assert_eq!(bound.ensure_container().await, mismatch);
        assert_eq!(SegmentRemote::get_all(&bound, SEG).await, Err(TransportError));
        assert_eq!(SegmentRemote::put_create(&bound, SEG, OURS).await, Err(TransportError));
        assert_eq!(bound.inner().requests(), 0);
        assert!(bound.inner().copies(SEG).is_empty());
    }

    #[tokio::test]
    async fn bound_rechecks_identity_on_every_call() {
        let store = FakeStore::new(true);
        let bound = Bound::new(store, binding_fp("fake|default"));
        bound.ensure_container().await.unwrap();
        ObjectStore::put_create(&bound, SEG, OURS).await.unwrap();
        assert_eq!(bound.get(SEG).await.unwrap(), OURS);
        assert_eq!(bound.inner().requests(), 3);
        // The account behind the provider changes after binding.
        bound.inner().set_identity("fake|someone-else");
        assert_eq!(bound.get(SEG).await, Err(ProviderError::BindingMismatch));
        assert_eq!(bound.inner().requests(), 3);
    }

    #[test]
    fn max_object_bytes_is_the_transport_body_cap() {
        assert_eq!(MAX_OBJECT_BYTES, transport::DEFAULT_MAX_BODY);
    }

    /// AF-1: a body at or over the cap is `Permanent` with no request through
    /// `Bound` (every backend) and, with `bare_checks`, on the bare adapter
    /// too; exactly cap-1 is stored either way.
    async fn object_size_limit_suite<P: ObjectStore>(store: P, bare_checks: bool, requests: impl Fn(&P) -> usize) {
        let fp = binding_fp(&store.identity());
        let bound = Bound::new(store, fp);
        let bare = bound.inner();
        let mut body = vec![0x5a; MAX_OBJECT_BYTES];
        let oversize = || Err(ProviderError::Permanent("object exceeds size limit".to_owned()));
        let before = requests(bare);
        for len in [MAX_OBJECT_BYTES, MAX_OBJECT_BYTES + 1] {
            body.resize(len, 0x5a);
            if bare_checks {
                assert_eq!(bare.put_create(SEG, &body).await, oversize(), "put_create {len}");
                assert_eq!(bare.put_replace(SEG, &body, None).await, oversize(), "put_replace {len}");
            }
            assert_eq!(ObjectStore::put_create(&bound, SEG, &body).await, oversize(), "bound put_create {len}");
            assert_eq!(bound.put_replace(SEG, &body, None).await, oversize(), "bound put_replace {len}");
            assert_eq!(bound.put_replace(SEG, &body, Some("\"e\"")).await, oversize(), "bound put_replace if-match {len}");
            assert_eq!(SegmentRemote::put_create(&bound, SEG, &body).await, Err(TransportError), "bridge {len}");
        }
        assert_eq!(requests(bare), before, "an oversize put must send no request");
        assert_eq!(bound.get(SEG).await, Err(ProviderError::NotFound));

        body.truncate(MAX_OBJECT_BYTES - 1);
        ObjectStore::put_create(&bound, SEG, &body).await.unwrap();
        assert_eq!(bound.get(SEG).await.unwrap().len(), MAX_OBJECT_BYTES - 1);
        let rec = "v1/vault-a/devices/dev-a.rec";
        bound.put_replace(rec, &body, None).await.unwrap();
        assert_eq!(bound.get(rec).await.unwrap().len(), MAX_OBJECT_BYTES - 1);
        if bare_checks {
            let blob = "v1/vault-a/blob/ab/ab12.blob";
            bare.put_create(blob, &body).await.unwrap();
            assert_eq!(bare.get(blob).await.unwrap().len(), MAX_OBJECT_BYTES - 1);
        }
    }

    #[tokio::test]
    async fn object_size_limit_fake() {
        // The test fake has no adapter-level check; `Bound` enforces it.
        object_size_limit_suite(FakeStore::new(true), false, FakeStore::requests).await;
    }

    #[tokio::test]
    async fn object_size_limit_webdav() {
        let (store, transport) = super::webdav::tests_support::emulated();
        object_size_limit_suite(store, true, move |_| transport.request_count()).await;
    }

    #[tokio::test]
    async fn object_size_limit_r2() {
        let (store, transport) = super::r2::tests_support::emulated();
        object_size_limit_suite(store, true, move |_| transport.request_count()).await;
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

    fn bound(store: FakeStore) -> Bound<FakeStore> {
        Bound::new(store, binding_fp("fake|default"))
    }

    #[tokio::test]
    async fn bridge_maps_provider_answers() {
        let remote = bound(FakeStore::new(true));
        assert!(SegmentRemote::create_only(&remote));
        assert_eq!(SegmentRemote::get_all(&remote, SEG).await, Ok(Vec::new()));
        assert_eq!(SegmentRemote::put_create(&remote, SEG, OURS).await, Ok(PutOutcome::Created));
        assert_eq!(SegmentRemote::put_create(&remote, SEG, THEIRS).await, Ok(PutOutcome::AlreadyExists));
        assert_eq!(
            SegmentRemote::get_all(&remote, SEG).await,
            Ok(vec![RemoteObject { provider_ref: SEG.to_owned(), bytes: OURS.to_vec() }])
        );
        for err in [
            ProviderError::Transient("reset".into()),
            ProviderError::RateLimited { retry_after: None },
            ProviderError::Auth { reconsent: true },
            ProviderError::PreconditionFailed,
            ProviderError::Permanent("HTTP 400".into()),
        ] {
            remote.inner().fail_next(Op::Get, err.clone());
            assert_eq!(SegmentRemote::get_all(&remote, SEG).await, Err(TransportError), "{err:?}");
            remote.inner().fail_next(Op::PutCreate, err.clone());
            assert_eq!(SegmentRemote::put_create(&remote, SEG, OURS).await, Err(TransportError), "{err:?}");
        }
        assert_eq!(remote.inner().copies(SEG), vec![OURS.to_vec()]);
    }

    #[tokio::test]
    async fn settle_publishes_after_resend_through_bridge() {
        let remote = bound(FakeStore::new(true));
        let settled = settle_attempted(&remote, &NoPause, SEG, OURS, judge).await;
        assert_eq!(settled.outcome, Outcome::Published);
        assert!(settled.extra_self_copies.is_empty());
        assert_eq!(remote.inner().copies(SEG), vec![OURS.to_vec()]);
    }

    #[tokio::test]
    async fn settle_sees_our_landed_upload_without_resending() {
        let remote = bound(FakeStore::new(true));
        remote.inner().seed(SEG, OURS);
        let settled = settle_attempted(&remote, &NoPause, SEG, OURS, judge).await;
        assert_eq!(settled.outcome, Outcome::Published);
        assert_eq!(remote.inner().requests(), 1, "one probe, no resend");
    }

    #[tokio::test]
    async fn settle_rejects_when_another_writer_holds_the_key() {
        let remote = bound(FakeStore::new(true));
        remote.inner().seed(SEG, THEIRS);
        let settled = settle_attempted(&remote, &NoPause, SEG, OURS, judge).await;
        assert_eq!(settled.outcome, Outcome::Rejected);
        assert_eq!(remote.inner().copies(SEG), vec![THEIRS.to_vec()]);
    }

    #[tokio::test]
    async fn settle_rejects_when_the_resend_finds_the_key_taken() {
        // Every probe misses the other writer's object (it surfaces as
        // NotFound), the resend gets AlreadyExists, and the verify sees it.
        let remote = bound(FakeStore::new(true));
        remote.inner().seed(SEG, THEIRS);
        remote.inner().fail_next_n(Op::Get, ProviderError::NotFound, 3);
        let settled = settle_attempted(&remote, &NoPause, SEG, OURS, judge).await;
        assert_eq!(settled.outcome, Outcome::Rejected);
        assert_eq!(remote.inner().copies(SEG), vec![THEIRS.to_vec()], "never overwritten");
    }

    #[tokio::test]
    async fn settle_stays_attempted_on_provider_failures() {
        let remote = bound(FakeStore::new(true));
        remote.inner().fail_next(Op::Get, ProviderError::Transient("reset".into()));
        let settled = settle_attempted(&remote, &NoPause, SEG, OURS, judge).await;
        assert_eq!(settled.outcome, Outcome::StillAttempted);

        remote.inner().fail_next(Op::PutCreate, ProviderError::RateLimited { retry_after: None });
        let settled = settle_attempted(&remote, &NoPause, SEG, OURS, judge).await;
        assert_eq!(settled.outcome, Outcome::StillAttempted);
        assert!(remote.inner().copies(SEG).is_empty());

        // A binding mismatch is also unknown, never a verdict, and sends nothing.
        let foreign = Bound::new(FakeStore::new(true), binding_fp("fake|other"));
        let settled = settle_attempted(&foreign, &NoPause, SEG, OURS, judge).await;
        assert_eq!(settled.outcome, Outcome::StillAttempted);
        assert_eq!(foreign.inner().requests(), 0);
    }

    #[tokio::test]
    async fn contract_suite_onedrive() {
        let (store, transport) = super::onedrive::tests_support::emulated();
        contract_suite(&store, || transport.request_count(), |status, hdrs| {
            super::onedrive::tests_support::fail_next(&transport, status, hdrs)
        })
        .await;
    }

    #[tokio::test]
    async fn props_suite_onedrive() {
        let (store, transport) = super::onedrive::tests_support::emulated();
        props_suite(&store, || transport.request_count()).await;
    }

    #[tokio::test]
    async fn object_size_limit_onedrive() {
        let (store, transport) = super::onedrive::tests_support::emulated();
        object_size_limit_suite(store, true, move |_| transport.request_count()).await;
    }
}
