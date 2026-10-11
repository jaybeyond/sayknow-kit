//! WebDAV adapter (§4.9, D10, A4).
//!
//! Objects live under `<base>/SayKnowKit/<key>`. The adapter is HTTPS only,
//! sends HTTP Basic auth on every request, and creates with
//! `If-None-Match: *`. A server that ignores that condition would let two
//! devices silently overwrite each other's segments, so `put_create` stays
//! `Unsupported` until [`WebDavStore::probe_conditional_create`] has shown
//! the server honours it. There is no non-atomic fallback.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::sync::atomic::{AtomicBool, Ordering};

use percent_encoding::{percent_decode_str, utf8_percent_encode, AsciiSet, NON_ALPHANUMERIC};
use quick_xml::events::{BytesRef, Event};
use quick_xml::name::{Namespace, ResolveResult};
use quick_xml::reader::NsReader;

use super::transport::{HttpRequest, HttpResponse, HttpTransport};
use super::{check_key, check_object_size, is_remote_key, keyed_listing, map_status, ListQuery, ListedObject, ObjectStore, ProviderError, Quota};

/// The collection every object lives under, relative to the base URL.
const ROOT_DIR: &str = "SayKnowKit";
/// Holds the conditional-create probe; never listed.
const PROBE_DIR: &str = ".probe";
/// Deeper collections than this are not followed while listing.
const MAX_LIST_DEPTH: usize = 16;

/// A path segment keeps only RFC 3986 unreserved characters literal.
const SEGMENT: &AsciiSet = &NON_ALPHANUMERIC.remove(b'-').remove(b'.').remove(b'_').remove(b'~');

const PROPFIND_LIST: &str = r#"<?xml version="1.0" encoding="utf-8"?><D:propfind xmlns:D="DAV:"><D:prop><D:resourcetype/><D:getetag/><D:getcontentlength/></D:prop></D:propfind>"#;
const PROPFIND_TYPE: &str = r#"<?xml version="1.0" encoding="utf-8"?><D:propfind xmlns:D="DAV:"><D:prop><D:resourcetype/></D:prop></D:propfind>"#;
#[cfg_attr(not(test), allow(dead_code))] // only tests call this so far
const PROPFIND_QUOTA: &str = r#"<?xml version="1.0" encoding="utf-8"?><D:propfind xmlns:D="DAV:"><D:prop><D:quota-available-bytes/><D:quota-used-bytes/></D:prop></D:propfind>"#;

#[derive(Clone)]
pub(crate) struct WebDavConfig {
    pub base_url: String,
    pub username: String,
    pub password: String,
}

impl fmt::Debug for WebDavConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("WebDavConfig")
            .field("base_url", &self.base_url)
            .field("username", &self.username)
            .field("password", &"<redacted>")
            .finish()
    }
}

pub(crate) struct WebDavStore<T> {
    transport: T,
    /// Normalized base URL, always ending in `/`.
    base: String,
    username: String,
    /// `Basic <base64>`; never printed (the store has no `Debug`).
    authorization: String,
    /// `<base>SayKnowKit/`.
    root: String,
    /// Percent-decoded path of `root`, ending in `/`.
    root_path: String,
    root_url: url::Url,
    probed: AtomicBool,
}

impl<T: HttpTransport> WebDavStore<T> {
    /// Validates the config. Anything but `https://` is refused (D10).
    pub(crate) fn new(config: WebDavConfig, transport: T) -> Result<Self, ProviderError> {
        let base = normalize_base(&config.base_url)?;
        let root = format!("{base}{ROOT_DIR}/");
        let root_url = url::Url::parse(&root).map_err(|_| invalid_url())?;
        let root_path = decode_path(root_url.path()).ok_or_else(invalid_url)?;
        let credentials = format!("{}:{}", config.username, config.password);
        Ok(Self {
            transport,
            base,
            username: config.username,
            authorization: format!("Basic {}", data_encoding::BASE64.encode(credentials.as_bytes())),
            root,
            root_path,
            root_url,
            probed: AtomicBool::new(false),
        })
    }

    /// The A4 capability gate, run at connect time: create a probe object
    /// with `If-None-Match: *` (expect 201), repeat the create (expect 412),
    /// then delete it. Passing unlocks `put_create`; a server that accepts the
    /// second create ignores the condition and is refused as `Unsupported`.
    /// Trust is revoked first, so every path but a passing probe (Unsupported,
    /// Transient, any other error) leaves the store locked, even one that an
    /// earlier probe had unlocked.
    pub(crate) async fn probe_conditional_create(&self) -> Result<(), ProviderError> {
        self.probed.store(false, Ordering::SeqCst);
        self.ensure_collection("").await?;
        let key = format!("{PROBE_DIR}/{}", uuid::Uuid::new_v4().simple());
        let first = self.put_with_parents(&key, b"probe", &[("If-None-Match", "*")]).await?;
        match first.status {
            201 => {}
            412 => return Err(ProviderError::Transient("probe name already taken".to_owned())),
            // 200/204 to a create means the server reports an overwrite: not a
            // clean create, so the condition cannot be trusted.
            200..=299 => return Err(unsupported_condition()),
            status => return Err(map_status(status, &first.headers)),
        }
        let second = self.put_with_parents(&key, b"probe", &[("If-None-Match", "*")]).await?;
        let verdict = match second.status {
            412 => Ok(()),
            200..=299 => Err(unsupported_condition()),
            status => Err(map_status(status, &second.headers)),
        };
        // Best effort: a leftover probe object is harmless and never listed.
        let _ = self.send("DELETE", self.object_url(&key), &[], Vec::new()).await;
        verdict?;
        self.probed.store(true, Ordering::SeqCst);
        Ok(())
    }

    async fn send(&self, method: &str, url: String, headers: &[(&str, &str)], body: Vec<u8>) -> Result<HttpResponse, ProviderError> {
        let mut all = vec![("Authorization".to_owned(), self.authorization.clone())];
        all.extend(headers.iter().map(|(n, v)| ((*n).to_owned(), (*v).to_owned())));
        let req = HttpRequest { method: method.to_owned(), url, headers: all, body };
        self.transport.send(req).await.map_err(ProviderError::from)
    }

    fn object_url(&self, key: &str) -> String {
        format!("{}{}", self.root, encode_path(key))
    }

    /// `rel` is "" for the root or a `/`-joined path without trailing slash.
    fn collection_url(&self, rel: &str) -> String {
        if rel.is_empty() {
            self.root.clone()
        } else {
            format!("{}{}/", self.root, encode_path(rel))
        }
    }

    /// MKCOL `rel`, creating missing parents first when the server answers
    /// 409. The base URL itself is never created: it must already exist.
    async fn ensure_collection(&self, rel: &str) -> Result<(), ProviderError> {
        let mut pending: Vec<&str> = vec![rel];
        let mut conflicted: BTreeSet<&str> = BTreeSet::new();
        while let Some(&dir) = pending.last() {
            if self.mkcol(dir).await? {
                pending.pop();
                continue;
            }
            if dir.is_empty() {
                return Err(ProviderError::Permanent("WebDAV base collection does not exist".to_owned()));
            }
            if !conflicted.insert(dir) {
                return Err(ProviderError::Transient("MKCOL still conflicts after creating parents".to_owned()));
            }
            pending.push(parent_of(dir));
        }
        Ok(())
    }

    /// `Ok(true)`: the collection exists now. `Ok(false)`: 409, a parent is
    /// missing (never success).
    async fn mkcol(&self, rel: &str) -> Result<bool, ProviderError> {
        let resp = self.send("MKCOL", self.collection_url(rel), &[], Vec::new()).await?;
        match resp.status {
            201 => Ok(true),
            405 => {
                if self.is_collection(rel).await? {
                    Ok(true)
                } else {
                    Err(ProviderError::Permanent("WebDAV path exists but is not a collection".to_owned()))
                }
            }
            409 => Ok(false),
            status => Err(map_status(status, &resp.headers)),
        }
    }

    async fn is_collection(&self, rel: &str) -> Result<bool, ProviderError> {
        let resp = self
            .send("PROPFIND", self.collection_url(rel), &XML_DEPTH_0, PROPFIND_TYPE.as_bytes().to_vec())
            .await?;
        if resp.status != 207 {
            return Err(map_status(resp.status, &resp.headers));
        }
        let entries = parse_multistatus(&resp.body)?;
        let entry = entries.first().ok_or_else(|| bad_xml("empty multistatus"))?;
        Ok(entry.collection)
    }

    async fn ensure_parents(&self, key: &str) -> Result<(), ProviderError> {
        self.ensure_collection(parent_of(key)).await
    }

    /// PUT; on 409 (parent missing) create the parents and retry once.
    async fn put_with_parents(&self, key: &str, bytes: &[u8], headers: &[(&str, &str)]) -> Result<HttpResponse, ProviderError> {
        let resp = self.send("PUT", self.object_url(key), headers, bytes.to_vec()).await?;
        if resp.status != 409 {
            return Ok(resp);
        }
        self.ensure_parents(key).await?;
        self.send("PUT", self.object_url(key), headers, bytes.to_vec()).await
    }

    fn listed(&self, key: &str, size: u64, resp: &HttpResponse) -> ListedObject {
        ListedObject {
            key: key.to_owned(),
            provider_ref: key.to_owned(),
            size: Some(size),
            etag: resp.header("etag").map(str::to_owned),
        }
    }

    /// The decoded path of `href` below the root, without trailing slash, or
    /// `None` when it does not lie under the root. Only the path is compared:
    /// servers behind proxies often answer with a different authority.
    fn relative_path(&self, href: &str) -> Option<String> {
        let url = self.root_url.join(href.trim()).ok()?;
        let path = decode_path(url.path())?;
        if path == self.root_path || path == self.root_path.trim_end_matches('/') {
            return Some(String::new());
        }
        let rel = path.strip_prefix(&self.root_path)?;
        Some(rel.trim_end_matches('/').to_owned())
    }
}

impl<T: HttpTransport> ObjectStore for WebDavStore<T> {
    fn create_only(&self) -> bool {
        self.probed.load(Ordering::SeqCst)
    }

    fn identity(&self) -> String {
        format!("webdav|{}|{}", self.base, self.username)
    }

    async fn list(&self, q: ListQuery<'_>) -> Result<Vec<ListedObject>, ProviderError> {
        let listing = keyed_listing(q)?;
        let prefix: &str = &listing.prefix;
        let start = prefix.rfind('/').map_or("", |i| &prefix[..i]).to_owned();
        let mut queue = vec![start.clone()];
        let mut visited: BTreeSet<String> = BTreeSet::from([start]);
        let mut found: BTreeMap<String, ListedObject> = BTreeMap::new();
        while let Some(dir) = queue.pop() {
            let resp = self
                .send("PROPFIND", self.collection_url(&dir), &XML_DEPTH_1, PROPFIND_LIST.as_bytes().to_vec())
                .await?;
            match resp.status {
                207 => {}
                // A collection that does not exist holds nothing.
                404 => continue,
                status => return Err(map_status(status, &resp.headers)),
            }
            let mut saw_self = false;
            for entry in parse_multistatus(&resp.body)? {
                let Some(rel) = self.relative_path(&entry.href) else {
                    // Never drop an entry silently: an href we cannot place
                    // could hide objects from the engine.
                    return Err(ProviderError::Permanent("WebDAV listing returned an entry outside the root".to_owned()));
                };
                if rel == dir {
                    saw_self = true;
                    continue;
                }
                if rel.is_empty() || rel == PROBE_DIR || rel.starts_with(".probe/") || !is_remote_key(&rel) {
                    continue;
                }
                if entry.collection {
                    let as_dir = format!("{rel}/");
                    let relevant = as_dir.starts_with(prefix) || prefix.starts_with(&as_dir);
                    if relevant && rel.split('/').count() <= MAX_LIST_DEPTH && visited.insert(rel.clone()) {
                        queue.push(rel);
                    }
                } else if listing.matches(&rel) {
                    found.insert(
                        rel.clone(),
                        ListedObject { key: rel.clone(), provider_ref: rel, size: entry.size, etag: entry.etag },
                    );
                }
            }
            if !saw_self {
                return Err(ProviderError::Permanent("WebDAV listing did not describe the requested collection".to_owned()));
            }
        }
        Ok(found.into_values().collect())
    }

    async fn get(&self, key: &str) -> Result<Vec<u8>, ProviderError> {
        check_key(key)?;
        let resp = self.send("GET", self.object_url(key), &[], Vec::new()).await?;
        match resp.status {
            200 => Ok(resp.body),
            status => Err(map_status(status, &resp.headers)),
        }
    }

    async fn put_create(&self, key: &str, bytes: &[u8]) -> Result<ListedObject, ProviderError> {
        check_key(key)?;
        check_object_size(bytes)?;
        if !self.create_only() {
            return Err(ProviderError::Unsupported("conditional create not verified for this WebDAV server".to_owned()));
        }
        let resp = self.put_with_parents(key, bytes, &[("If-None-Match", "*")]).await?;
        match resp.status {
            201 => Ok(self.listed(key, bytes.len() as u64, &resp)),
            // RFC 9110: a PUT that creates answers 201, so 200/204 means an
            // existing object was replaced and the server ignored the
            // condition. Stop trusting it: later creates are refused too.
            200 | 204 => {
                self.probed.store(false, Ordering::SeqCst);
                Err(unsupported_condition())
            }
            412 => Err(ProviderError::AlreadyExists),
            status => Err(map_status(status, &resp.headers)),
        }
    }

    async fn put_replace(&self, key: &str, bytes: &[u8], if_match: Option<&str>) -> Result<ListedObject, ProviderError> {
        check_key(key)?;
        check_object_size(bytes)?;
        let headers: Vec<(&str, &str)> = if_match.map(|etag| ("If-Match", etag)).into_iter().collect();
        let resp = self.put_with_parents(key, bytes, &headers).await?;
        match resp.status {
            200 | 201 | 204 => Ok(self.listed(key, bytes.len() as u64, &resp)),
            412 => Err(ProviderError::PreconditionFailed),
            status => Err(map_status(status, &resp.headers)),
        }
    }

    async fn delete(&self, key: &str) -> Result<(), ProviderError> {
        check_key(key)?;
        let resp = self.send("DELETE", self.object_url(key), &[], Vec::new()).await?;
        match resp.status {
            200 | 202 | 204 | 404 => Ok(()),
            status => Err(map_status(status, &resp.headers)),
        }
    }

    async fn quota(&self) -> Result<Quota, ProviderError> {
        let resp = self
            .send("PROPFIND", self.collection_url(""), &XML_DEPTH_0, PROPFIND_QUOTA.as_bytes().to_vec())
            .await?;
        if resp.status != 207 {
            return Err(map_status(resp.status, &resp.headers));
        }
        let entry = parse_multistatus(&resp.body)?.into_iter().next().unwrap_or_default();
        Ok(Quota { used: entry.quota_used, available: entry.quota_available })
    }

    async fn ensure_container(&self) -> Result<(), ProviderError> {
        self.ensure_collection("").await
    }
}

const XML_DEPTH_0: [(&str, &str); 2] = [("Depth", "0"), ("Content-Type", "application/xml; charset=utf-8")];
const XML_DEPTH_1: [(&str, &str); 2] = [("Depth", "1"), ("Content-Type", "application/xml; charset=utf-8")];

fn invalid_url() -> ProviderError {
    ProviderError::Permanent("invalid WebDAV URL".to_owned())
}

fn unsupported_condition() -> ProviderError {
    ProviderError::Unsupported("server does not honour If-None-Match".to_owned())
}

fn bad_xml(what: &str) -> ProviderError {
    ProviderError::Permanent(format!("malformed WebDAV response: {what}"))
}

/// Lowercases scheme and host, drops the default port, refuses embedded
/// credentials, query and fragment, and ends the path with `/`.
fn normalize_base(raw: &str) -> Result<String, ProviderError> {
    let mut url = url::Url::parse(raw.trim()).map_err(|_| {
        if raw.trim().to_ascii_lowercase().starts_with("https://") {
            invalid_url()
        } else {
            ProviderError::Permanent("WebDAV requires HTTPS".to_owned())
        }
    })?;
    if url.scheme() != "https" {
        return Err(ProviderError::Permanent("WebDAV requires HTTPS".to_owned()));
    }
    if url.host_str().is_none_or(str::is_empty) || !url.username().is_empty() || url.password().is_some() {
        return Err(invalid_url());
    }
    if url.query().is_some() || url.fragment().is_some() {
        return Err(invalid_url());
    }
    if !url.path().ends_with('/') {
        let path = format!("{}/", url.path());
        url.set_path(&path);
    }
    Ok(url.to_string())
}

fn decode_path(path: &str) -> Option<String> {
    percent_decode_str(path).decode_utf8().ok().map(|p| p.into_owned())
}

fn encode_path(rel: &str) -> String {
    rel.split('/').map(|s| utf8_percent_encode(s, SEGMENT).to_string()).collect::<Vec<_>>().join("/")
}

/// "a/b/c" → "a/b"; "a" → "".
fn parent_of(path: &str) -> &str {
    path.rfind('/').map_or("", |i| &path[..i])
}

/// One `<D:response>` of a multistatus, merged from its 2xx propstats.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
struct DavEntry {
    href: String,
    collection: bool,
    etag: Option<String>,
    size: Option<u64>,
    quota_used: Option<u64>,
    quota_available: Option<u64>,
}

#[derive(Default)]
struct PropStat {
    ok: bool,
    collection: bool,
    etag: Option<String>,
    size: Option<u64>,
    quota_used: Option<u64>,
    quota_available: Option<u64>,
}

#[derive(Default)]
struct MultistatusState {
    entries: Vec<DavEntry>,
    response: Option<(DavEntry, bool)>,
    propstat: Option<PropStat>,
}

impl MultistatusState {
    fn open(&mut self, name: Option<&str>, parent: Option<&str>) {
        match name {
            Some("response") => self.response = Some((DavEntry::default(), false)),
            Some("propstat") => self.propstat = Some(PropStat::default()),
            Some("collection") if parent == Some("resourcetype") => {
                if let Some(ps) = self.propstat.as_mut() {
                    ps.collection = true;
                }
            }
            _ => {}
        }
    }

    fn close(&mut self, name: Option<&str>, text: &str) {
        let text = text.trim();
        let value = (!text.is_empty()).then(|| text.to_owned());
        match (name, self.propstat.as_mut()) {
            (Some("href"), None) => {
                if let Some((entry, _)) = self.response.as_mut() {
                    entry.href = text.to_owned();
                }
            }
            (Some("status"), Some(ps)) => ps.ok = status_is_success(text),
            (Some("status"), None) => {
                if let Some((_, skip)) = self.response.as_mut() {
                    *skip = !status_is_success(text);
                }
            }
            (Some("getetag"), Some(ps)) => ps.etag = value,
            (Some("getcontentlength"), Some(ps)) => ps.size = text.parse().ok(),
            (Some("quota-used-bytes"), Some(ps)) => ps.quota_used = text.parse().ok(),
            (Some("quota-available-bytes"), Some(ps)) => ps.quota_available = text.parse().ok(),
            (Some("propstat"), Some(_)) => {
                let ps = self.propstat.take().unwrap_or_default();
                if let (true, Some((entry, _))) = (ps.ok, self.response.as_mut()) {
                    entry.collection |= ps.collection;
                    entry.etag = ps.etag.or(entry.etag.take());
                    entry.size = ps.size.or(entry.size);
                    entry.quota_used = ps.quota_used.or(entry.quota_used);
                    entry.quota_available = ps.quota_available.or(entry.quota_available);
                }
            }
            (Some("response"), None) => {
                if let Some((mut entry, skip)) = self.response.take() {
                    if !skip && !entry.href.is_empty() {
                        // A trailing slash names a collection even when the
                        // server omitted resourcetype.
                        entry.collection |= entry.href.ends_with('/');
                        self.entries.push(entry);
                    }
                }
            }
            _ => {}
        }
    }
}

/// `HTTP/1.1 200 OK` → true for any 2xx.
fn status_is_success(line: &str) -> bool {
    line.split_whitespace().nth(1).and_then(|code| code.parse::<u16>().ok()).is_some_and(|code| (200..300).contains(&code))
}

/// Appends the text an entity or character reference stands for. `false`
/// for an unknown entity.
pub(super) fn push_reference(out: &mut String, reference: &BytesRef<'_>) -> bool {
    if let Ok(Some(ch)) = reference.resolve_char_ref() {
        out.push(ch);
        return true;
    }
    let Ok(name) = reference.decode() else { return false };
    let entity = format!("&{name};");
    match quick_xml::escape::unescape(&entity) {
        Ok(text) => {
            out.push_str(&text);
            true
        }
        Err(_) => false,
    }
}

/// Parses a `DAV:` multistatus with any namespace prefix.
fn parse_multistatus(body: &[u8]) -> Result<Vec<DavEntry>, ProviderError> {
    let text = std::str::from_utf8(body).map_err(|_| bad_xml("not UTF-8"))?;
    let mut reader = NsReader::from_str(text);
    let mut stack: Vec<Option<String>> = Vec::new();
    let mut state = MultistatusState::default();
    let mut buf = String::new();
    loop {
        let (ns, event) = reader.read_resolved_event().map_err(|_| bad_xml("invalid XML"))?;
        let name = match &event {
            Event::Start(e) | Event::Empty(e) => dav_local(&ns, e.local_name().as_ref()),
            _ => None,
        };
        match event {
            Event::Start(_) => {
                state.open(name.as_deref(), stack.last().cloned().flatten().as_deref());
                stack.push(name);
                buf.clear();
            }
            Event::Empty(_) => {
                state.open(name.as_deref(), stack.last().cloned().flatten().as_deref());
                state.close(name.as_deref(), "");
                buf.clear();
            }
            Event::End(_) => {
                let closed = stack.pop().ok_or_else(|| bad_xml("unbalanced elements"))?;
                state.close(closed.as_deref(), &buf);
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
    Ok(state.entries)
}

fn dav_local(ns: &ResolveResult<'_>, local: &[u8]) -> Option<String> {
    match ns {
        ResolveResult::Bound(Namespace(uri)) if *uri == b"DAV:" => Some(String::from_utf8_lossy(local).into_owned()),
        _ => None,
    }
}

/// An in-memory WebDAV server behind a [`MockTransport`]: Basic auth,
/// MKCOL 201/405/409, PUT with If-None-Match / If-Match, GET, DELETE, and
/// PROPFIND Depth 0/1 multistatus with quota.
#[cfg(test)]
pub(crate) mod tests_support {
    use std::collections::BTreeMap;
    use std::sync::{Arc, Mutex};

    use super::*;
    use crate::sync::provider::transport::MockTransport;

    pub(crate) const BASE: &str = "https://dav.example.test/remote.php/dav/files/alice/";
    pub(crate) const USER: &str = "alice";
    pub(crate) const PASSWORD: &str = "app-password-1";

    #[derive(Debug, Clone)]
    pub(crate) enum Node {
        Collection,
        File { bytes: Vec<u8>, etag: String },
    }

    pub(crate) struct DavServer {
        /// Decoded absolute paths without trailing slash; "" is `/`.
        pub nodes: BTreeMap<String, Node>,
        pub honour_if_none_match: bool,
        pub quota: Option<(u64, u64)>,
        next_etag: u64,
    }

    impl DavServer {
        /// Every ancestor of the base path exists, as on a real server.
        pub(crate) fn new() -> Self {
            let mut nodes = BTreeMap::new();
            let mut path = String::new();
            nodes.insert(String::new(), Node::Collection);
            for segment in url::Url::parse(BASE).unwrap().path().split('/').filter(|s| !s.is_empty()) {
                path = format!("{path}/{segment}");
                nodes.insert(path.clone(), Node::Collection);
            }
            Self { nodes, honour_if_none_match: true, quota: None, next_etag: 0 }
        }

        /// The base-relative path, decoded: `rel("SayKnowKit/a")`.
        pub(crate) fn path(rel: &str) -> String {
            let base = url::Url::parse(BASE).unwrap();
            format!("{}{}", base.path(), rel).trim_end_matches('/').to_owned()
        }

        pub(crate) fn file(&self, rel: &str) -> Option<Vec<u8>> {
            match self.nodes.get(&Self::path(rel)) {
                Some(Node::File { bytes, .. }) => Some(bytes.clone()),
                _ => None,
            }
        }

        fn etag(&mut self) -> String {
            self.next_etag += 1;
            format!("\"e{}\"", self.next_etag)
        }

        fn parent_is_collection(&self, path: &str) -> bool {
            let parent = path.rfind('/').map_or("", |i| &path[..i]);
            matches!(self.nodes.get(parent), Some(Node::Collection))
        }

        fn handle(&mut self, req: &HttpRequest) -> HttpResponse {
            let expected = format!("Basic {}", data_encoding::BASE64.encode(format!("{USER}:{PASSWORD}").as_bytes()));
            if req.header("authorization") != Some(expected.as_str()) {
                return status(401);
            }
            let url = url::Url::parse(&req.url).unwrap();
            let path = decode_path(url.path()).unwrap().trim_end_matches('/').to_owned();
            match req.method.as_str() {
                "MKCOL" => {
                    if self.nodes.contains_key(&path) {
                        status(405)
                    } else if !self.parent_is_collection(&path) {
                        status(409)
                    } else {
                        self.nodes.insert(path, Node::Collection);
                        status(201)
                    }
                }
                "PUT" => {
                    if !self.parent_is_collection(&path) {
                        return status(409);
                    }
                    let existing = match self.nodes.get(&path) {
                        Some(Node::Collection) => return status(405),
                        Some(Node::File { etag, .. }) => Some(etag.clone()),
                        None => None,
                    };
                    if self.honour_if_none_match && req.header("if-none-match") == Some("*") && existing.is_some() {
                        return status(412);
                    }
                    if let Some(expected) = req.header("if-match") {
                        if existing.as_deref() != Some(expected) {
                            return status(412);
                        }
                    }
                    let etag = self.etag();
                    self.nodes.insert(path, Node::File { bytes: req.body.clone(), etag: etag.clone() });
                    let code = if existing.is_some() { 204 } else { 201 };
                    HttpResponse { status: code, headers: vec![("ETag".to_owned(), etag)], body: Vec::new() }
                }
                "GET" => match self.nodes.get(&path) {
                    Some(Node::File { bytes, etag }) => {
                        HttpResponse { status: 200, headers: vec![("ETag".to_owned(), etag.clone())], body: bytes.clone() }
                    }
                    Some(Node::Collection) => status(405),
                    None => status(404),
                },
                "DELETE" => {
                    if self.nodes.remove(&path).is_none() {
                        return status(404);
                    }
                    let nested = format!("{path}/");
                    self.nodes.retain(|p, _| !p.starts_with(&nested));
                    status(204)
                }
                "PROPFIND" => self.propfind(&path, req),
                _ => status(405),
            }
        }

        fn propfind(&self, path: &str, req: &HttpRequest) -> HttpResponse {
            let Some(node) = self.nodes.get(path) else { return status(404) };
            let depth = req.header("depth").unwrap_or("infinity");
            if depth != "0" && depth != "1" {
                return status(403);
            }
            let with_quota = String::from_utf8_lossy(&req.body).contains("quota-available-bytes");
            let mut xml = String::from(r#"<?xml version="1.0" encoding="utf-8"?><D:multistatus xmlns:D="DAV:">"#);
            xml.push_str(&self.entry_xml(path, node, with_quota));
            if depth == "1" && matches!(node, Node::Collection) {
                let prefix = format!("{path}/");
                for (child, child_node) in self.nodes.range(prefix.clone()..) {
                    let Some(name) = child.strip_prefix(&prefix) else { break };
                    if !name.contains('/') {
                        xml.push_str(&self.entry_xml(child, child_node, false));
                    }
                }
            }
            xml.push_str("</D:multistatus>");
            HttpResponse { status: 207, headers: Vec::new(), body: xml.into_bytes() }
        }

        fn entry_xml(&self, path: &str, node: &Node, with_quota: bool) -> String {
            let mut href = path.split('/').map(|s| utf8_percent_encode(s, SEGMENT).to_string()).collect::<Vec<_>>().join("/");
            let mut props = String::new();
            match node {
                Node::Collection => {
                    href.push('/');
                    props.push_str("<D:resourcetype><D:collection/></D:resourcetype>");
                }
                Node::File { bytes, etag } => {
                    props.push_str("<D:resourcetype/>");
                    props.push_str(&format!("<D:getcontentlength>{}</D:getcontentlength>", bytes.len()));
                    props.push_str(&format!("<D:getetag>{}</D:getetag>", quick_xml::escape::escape(etag.as_str())));
                }
            }
            let mut missing = String::new();
            if with_quota {
                match self.quota {
                    Some((used, available)) => props.push_str(&format!(
                        "<D:quota-used-bytes>{used}</D:quota-used-bytes><D:quota-available-bytes>{available}</D:quota-available-bytes>"
                    )),
                    None => missing.push_str("<D:quota-used-bytes/><D:quota-available-bytes/>"),
                }
            }
            let mut xml = format!(
                "<D:response><D:href>{href}</D:href><D:propstat><D:prop>{props}</D:prop><D:status>HTTP/1.1 200 OK</D:status></D:propstat>"
            );
            if !missing.is_empty() {
                xml.push_str(&format!(
                    "<D:propstat><D:prop>{missing}</D:prop><D:status>HTTP/1.1 404 Not Found</D:status></D:propstat>"
                ));
            }
            xml.push_str("</D:response>");
            xml
        }
    }

    fn status(code: u16) -> HttpResponse {
        HttpResponse { status: code, headers: Vec::new(), body: Vec::new() }
    }

    pub(crate) fn config() -> WebDavConfig {
        WebDavConfig { base_url: BASE.to_owned(), username: USER.to_owned(), password: PASSWORD.to_owned() }
    }

    /// A store over `server`, not yet probed.
    pub(crate) fn over(server: DavServer) -> (WebDavStore<MockTransport>, MockTransport, Arc<Mutex<DavServer>>) {
        let state = Arc::new(Mutex::new(server));
        let shared = Arc::clone(&state);
        let transport = MockTransport::new(move |req| shared.lock().unwrap().handle(req));
        let store = WebDavStore::new(config(), transport.clone()).unwrap();
        (store, transport, state)
    }

    /// The contract-suite backend: an If-None-Match-honouring server and a
    /// store whose conditional-create capability is already established
    /// (the probe itself is covered by the adapter tests).
    pub(crate) fn emulated() -> (WebDavStore<MockTransport>, MockTransport) {
        let (store, transport, _) = over(DavServer::new());
        store.probed.store(true, Ordering::SeqCst);
        (store, transport)
    }
}

#[cfg(test)]
mod tests {
    use super::tests_support::*;
    use super::*;
    use crate::sync::provider::transport::MockTransport;

    const SEG: &str = "v1/vault-a/log/dev-a/000000000001-000000000003.seg";

    fn methods(transport: &MockTransport) -> Vec<String> {
        transport.requests().iter().map(|r| format!("{} {}", r.method, r.url.trim_start_matches(BASE))).collect()
    }

    fn probed(server: DavServer) -> (WebDavStore<MockTransport>, MockTransport, std::sync::Arc<std::sync::Mutex<DavServer>>) {
        let (store, transport, state) = over(server);
        store.probed.store(true, Ordering::SeqCst);
        (store, transport, state)
    }

    #[test]
    fn construction_requires_https() {
        for base in ["http://dav.example.test/", "ftp://dav.example.test/", "dav.example.test/", "HTTP://dav.example.test/"] {
            let config = WebDavConfig { base_url: base.to_owned(), ..config() };
            let err = WebDavStore::new(config, MockTransport::new(|_| unreachable!())).err().unwrap();
            assert_eq!(err, ProviderError::Permanent("WebDAV requires HTTPS".to_owned()), "{base}");
        }
        for base in ["https://user:pw@dav.example.test/", "https://dav.example.test/?a=1", "https://dav.example.test/#x"] {
            let config = WebDavConfig { base_url: base.to_owned(), ..config() };
            assert!(WebDavStore::new(config, MockTransport::new(|_| unreachable!())).is_err(), "{base}");
        }
        assert!(WebDavStore::new(config(), MockTransport::new(|_| unreachable!())).is_ok());
    }

    #[test]
    fn identity_normalizes_the_base_url() {
        let config = WebDavConfig {
            base_url: "HTTPS://DAV.Example.TEST:443/remote.php/dav/files/alice".to_owned(),
            ..config()
        };
        let store = WebDavStore::new(config, MockTransport::new(|_| unreachable!())).unwrap();
        assert_eq!(store.identity(), "webdav|https://dav.example.test/remote.php/dav/files/alice/|alice");
        let custom_port = WebDavConfig { base_url: "https://dav.example.test:8443/x".to_owned(), ..super::tests_support::config() };
        let store = WebDavStore::new(custom_port, MockTransport::new(|_| unreachable!())).unwrap();
        assert_eq!(store.identity(), "webdav|https://dav.example.test:8443/x/|alice");
    }

    #[tokio::test]
    async fn every_request_carries_basic_auth_and_errors_hide_credentials() {
        let (store, transport, _) = probed(DavServer::new());
        store.ensure_container().await.unwrap();
        store.put_create(SEG, b"x").await.unwrap();
        let expected = format!("Basic {}", data_encoding::BASE64.encode(b"alice:app-password-1"));
        for req in transport.requests() {
            assert_eq!(req.header("authorization"), Some(expected.as_str()), "{req:?}");
        }
        transport.fail_next(500, &[]);
        let err = format!("{:?}", store.get(SEG).await.unwrap_err());
        assert!(!err.contains(PASSWORD) && !err.contains(&expected), "{err}");
    }

    #[tokio::test]
    async fn mkcol_405_on_existing_collection_is_ok() {
        let (store, transport, _) = probed(DavServer::new());
        store.ensure_container().await.unwrap();
        store.ensure_container().await.unwrap();
        assert_eq!(methods(&transport), ["MKCOL SayKnowKit/", "MKCOL SayKnowKit/", "PROPFIND SayKnowKit/"]);
        assert_eq!(transport.requests()[2].header("depth"), Some("0"));
    }

    #[tokio::test]
    async fn mkcol_405_where_propfind_shows_a_file_is_permanent() {
        let mut server = DavServer::new();
        server.nodes.insert(DavServer::path("SayKnowKit"), Node::File { bytes: b"oops".to_vec(), etag: "\"f\"".to_owned() });
        let (store, transport, state) = probed(server);
        let err = store.ensure_container().await.unwrap_err();
        assert!(matches!(err, ProviderError::Permanent(_)), "{err:?}");
        assert_eq!(methods(&transport), ["MKCOL SayKnowKit/", "PROPFIND SayKnowKit/"]);
        assert_eq!(state.lock().unwrap().file("SayKnowKit"), Some(b"oops".to_vec()), "never touched");
    }

    #[tokio::test]
    async fn mkcol_409_creates_parents_first() {
        let (store, transport, state) = probed(DavServer::new());
        store.ensure_container().await.unwrap();
        let created = store.put_create(SEG, b"seg").await.unwrap();
        assert_eq!(created.etag.as_deref(), Some("\"e1\""));
        assert_eq!(
            methods(&transport),
            [
                "MKCOL SayKnowKit/",
                "PUT SayKnowKit/v1/vault-a/log/dev-a/000000000001-000000000003.seg",
                "MKCOL SayKnowKit/v1/vault-a/log/dev-a/",
                "MKCOL SayKnowKit/v1/vault-a/log/",
                "MKCOL SayKnowKit/v1/vault-a/",
                "MKCOL SayKnowKit/v1/",
                "MKCOL SayKnowKit/v1/vault-a/",
                "MKCOL SayKnowKit/v1/vault-a/log/",
                "MKCOL SayKnowKit/v1/vault-a/log/dev-a/",
                "PUT SayKnowKit/v1/vault-a/log/dev-a/000000000001-000000000003.seg",
            ]
        );
        assert_eq!(state.lock().unwrap().file(&format!("SayKnowKit/{SEG}")), Some(b"seg".to_vec()));
    }

    #[tokio::test]
    async fn missing_base_collection_is_permanent_not_created() {
        let mut server = DavServer::new();
        server.nodes.remove(&DavServer::path(""));
        let (store, transport, _) = probed(server);
        let err = store.ensure_container().await.unwrap_err();
        assert!(matches!(err, ProviderError::Permanent(_)), "{err:?}");
        assert_eq!(methods(&transport), ["MKCOL SayKnowKit/"]);
    }

    #[tokio::test]
    async fn probe_passes_on_an_honouring_server_and_unlocks_create() {
        let (store, transport, state) = over(DavServer::new());
        assert!(!store.create_only());
        store.probe_conditional_create().await.unwrap();
        assert!(store.create_only());
        let log = methods(&transport);
        assert_eq!(log[0], "MKCOL SayKnowKit/");
        assert!(log[1].starts_with("PUT SayKnowKit/.probe/"), "{log:?}");
        assert_eq!(log.last().map(|s| s.starts_with("DELETE SayKnowKit/.probe/")), Some(true), "{log:?}");
        let puts: Vec<_> = transport.requests().into_iter().filter(|r| r.method == "PUT").collect();
        assert!(puts.iter().all(|r| r.header("if-none-match") == Some("*")));
        // The probe object is gone; the probe collection is never listed.
        assert!(!state.lock().unwrap().nodes.keys().any(|p| p.contains("/.probe/")));
        assert!(store.list(ListQuery::Prefix("")).await.unwrap().is_empty());
        store.put_create(SEG, b"x").await.unwrap();
    }

    #[tokio::test]
    async fn server_ignoring_if_none_match_fails_the_probe_and_refuses_create() {
        let mut server = DavServer::new();
        server.honour_if_none_match = false;
        let (store, transport, state) = over(server);
        // Before any probe, put_create is refused without a request.
        let err = store.put_create(SEG, b"x").await.unwrap_err();
        assert!(matches!(err, ProviderError::Unsupported(_)), "{err:?}");
        assert_eq!(transport.request_count(), 0);
        assert_eq!(
            store.probe_conditional_create().await,
            Err(ProviderError::Unsupported("server does not honour If-None-Match".to_owned()))
        );
        assert!(!store.create_only());
        let err = store.put_create(SEG, b"x").await.unwrap_err();
        assert!(matches!(err, ProviderError::Unsupported(_)), "{err:?}");
        assert!(state.lock().unwrap().file(&format!("SayKnowKit/{SEG}")).is_none());
        assert!(!state.lock().unwrap().nodes.keys().any(|p| p.contains("/.probe/")), "probe cleaned up");
    }

    #[tokio::test]
    async fn put_create_maps_412_and_never_overwrites() {
        let (store, _, state) = probed(DavServer::new());
        store.put_create(SEG, b"ours").await.unwrap();
        assert_eq!(store.put_create(SEG, b"theirs").await, Err(ProviderError::AlreadyExists));
        assert_eq!(state.lock().unwrap().file(&format!("SayKnowKit/{SEG}")), Some(b"ours".to_vec()));
    }

    #[tokio::test]
    async fn put_create_persistent_409_is_not_success() {
        let (store, transport, _) = probed(DavServer::new());
        store.ensure_container().await.unwrap();
        store.put_create(SEG, b"x").await.unwrap();
        let other = "v1/vault-a/log/dev-a/000000000004-000000000004.seg";
        // PUT → 409, the parent MKCOL then reports success, and the retried
        // PUT still conflicts: that is an error, never a create.
        transport.fail_next(409, &[]);
        transport.fail_next(201, &[]);
        transport.fail_next(409, &[]);
        let err = store.put_create(other, b"y").await.unwrap_err();
        assert!(matches!(err, ProviderError::Permanent(_)), "{err:?}");
    }

    #[tokio::test]
    async fn put_create_204_clears_the_probe_and_is_unsupported() {
        let (store, transport, _) = probed(DavServer::new());
        store.ensure_container().await.unwrap();
        assert!(store.create_only());
        transport.fail_next(204, &[]);
        assert_eq!(
            store.put_create(SEG, b"x").await,
            Err(ProviderError::Unsupported("server does not honour If-None-Match".to_owned()))
        );
        assert!(!store.create_only());
        let before = transport.request_count();
        let err = store.put_create(SEG, b"x").await.unwrap_err();
        assert!(matches!(err, ProviderError::Unsupported(_)), "{err:?}");
        assert_eq!(transport.request_count(), before, "an unprobed store sends nothing");
    }

    #[test]
    fn multistatus_with_custom_prefix_and_encoded_hrefs() {
        let xml = r#"<?xml version="1.0"?>
<lp:multistatus xmlns:lp="DAV:" xmlns:x="urn:other">
  <lp:response>
    <lp:href>/remote.php/dav/files/alice/SayKnowKit/v1/</lp:href>
    <lp:propstat><lp:prop><lp:resourcetype><lp:collection/></lp:resourcetype></lp:prop><lp:status>HTTP/1.1 200 OK</lp:status></lp:propstat>
  </lp:response>
  <lp:response>
    <lp:href>https://other-host.example/remote.php/dav/files/alice/SayKnowKit/v1/a%2Db%5Fc.seg</lp:href>
    <lp:propstat>
      <lp:prop>
        <lp:resourcetype/>
        <lp:getetag>&quot;abc&amp;1&quot;</lp:getetag>
        <lp:getcontentlength>42</lp:getcontentlength>
        <x:collection/>
      </lp:prop>
      <lp:status>HTTP/1.1 200 OK</lp:status>
    </lp:propstat>
    <lp:propstat><lp:prop><lp:getcontentlength>7</lp:getcontentlength></lp:prop><lp:status>HTTP/1.1 404 Not Found</lp:status></lp:propstat>
  </lp:response>
  <x:response><x:href>/ignored</x:href></x:response>
</lp:multistatus>"#;
        let entries = parse_multistatus(xml.as_bytes()).unwrap();
        assert_eq!(entries.len(), 2);
        assert!(entries[0].collection);
        assert_eq!(entries[1].etag.as_deref(), Some("\"abc&1\""));
        assert_eq!(entries[1].size, Some(42));
        assert!(!entries[1].collection, "a collection element outside DAV: is not a resourcetype");
        let store = WebDavStore::new(config(), MockTransport::new(|_| unreachable!())).unwrap();
        assert_eq!(store.relative_path(&entries[0].href).as_deref(), Some("v1"));
        assert_eq!(store.relative_path(&entries[1].href).as_deref(), Some("v1/a-b_c.seg"));
        assert_eq!(store.relative_path("/remote.php/dav/files/alice/").as_deref(), None);
        assert_eq!(store.relative_path("/remote.php/dav/files/alice/SayKnowKit").as_deref(), Some(""));
    }

    #[test]
    fn multistatus_rejects_malformed_xml() {
        for bad in ["<D:multistatus xmlns:D=\"DAV:\"><D:response>", "<a></b>", "<D:x xmlns:D=\"DAV:\">&bogus;</D:x>"] {
            let result = parse_multistatus(bad.as_bytes());
            assert!(matches!(result, Err(ProviderError::Permanent(_))), "{bad}: {result:?}");
        }
    }

    #[tokio::test]
    async fn list_recurses_decodes_and_excludes_probe_and_foreign_entries() {
        let (store, transport, state) = probed(DavServer::new());
        store.probe_conditional_create().await.unwrap();
        store.put_create(SEG, b"seg").await.unwrap();
        store.put_create("v1/vault-a/snap/dev-a/s1.snap", b"snap").await.unwrap();
        {
            let mut server = state.lock().unwrap();
            // A foreign file with a name no key can have, and a stray probe.
            server.nodes.insert(DavServer::path("SayKnowKit/v1/vault-a/notes and stuff.txt"), Node::File { bytes: vec![1], etag: "\"z\"".into() });
            server.nodes.insert(DavServer::path("SayKnowKit/.probe/left-over"), Node::File { bytes: vec![1], etag: "\"p\"".into() });
        }
        let listed = store.list(ListQuery::Prefix("")).await.unwrap();
        let keys: Vec<&str> = listed.iter().map(|o| o.key.as_str()).collect();
        assert_eq!(keys, ["v1/vault-a/log/dev-a/000000000001-000000000003.seg", "v1/vault-a/snap/dev-a/s1.snap"]);
        assert_eq!(listed[0].size, Some(3));
        assert!(listed[0].etag.is_some());
        assert_eq!(listed[0].provider_ref, listed[0].key);
        let before = transport.request_count();
        let snaps = store.list(ListQuery::Prefix("v1/vault-a/snap/dev-a/s")).await.unwrap();
        assert_eq!(snaps.len(), 1);
        // Only the deepest named collection is walked, not the whole tree.
        assert_eq!(transport.request_count() - before, 1);
        assert!(transport.requests().iter().filter(|r| r.method == "PROPFIND").all(|r| r.header("depth") != Some("infinity")));
    }

    #[tokio::test]
    async fn list_refuses_entries_outside_the_root() {
        let (store, transport, _) = probed(DavServer::new());
        store.ensure_container().await.unwrap();
        let xml = r#"<D:multistatus xmlns:D="DAV:"><D:response><D:href>/elsewhere/x</D:href><D:propstat><D:prop><D:resourcetype/></D:prop><D:status>HTTP/1.1 200 OK</D:status></D:propstat></D:response></D:multistatus>"#;
        transport.script_next(Ok(HttpResponse { status: 207, headers: Vec::new(), body: xml.as_bytes().to_vec() }));
        assert!(matches!(store.list(ListQuery::Prefix("")).await, Err(ProviderError::Permanent(_))));
    }

    #[tokio::test]
    async fn quota_reads_both_values_or_none() {
        let (store, _, state) = probed(DavServer::new());
        store.ensure_container().await.unwrap();
        assert_eq!(store.quota().await, Ok(Quota { used: None, available: None }));
        state.lock().unwrap().quota = Some((1_000, 9_000));
        assert_eq!(store.quota().await, Ok(Quota { used: Some(1_000), available: Some(9_000) }));
    }

    #[tokio::test]
    async fn delete_missing_is_ok_and_get_missing_is_not_found() {
        let (store, _, _) = probed(DavServer::new());
        store.ensure_container().await.unwrap();
        assert_eq!(store.delete(SEG).await, Ok(()));
        assert_eq!(store.get(SEG).await, Err(ProviderError::NotFound));
    }
}
