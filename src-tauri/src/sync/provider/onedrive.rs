//! OneDrive adapter over Microsoft Graph (§4.2 N-5, §4.9, AC 31).
//!
//! Objects live in real folders under the app folder
//! (`/me/drive/special/approot`), at the full layout key. Every create sends
//! `@microsoft.graph.conflictBehavior=fail`, simple uploads and upload
//! sessions alike, so a name another device already holds is never
//! overwritten: a file 409 is `AlreadyExists`, while a folder 409 means the
//! folder is already there and is success once it is confirmed to be a
//! folder.
//!
//! The Graph bearer token goes only to `https://graph.microsoft.com`. The
//! pre-authenticated URLs Graph hands out (download redirects, upload session
//! URLs) are reached without an `Authorization` header and only over HTTPS,
//! and neither those URLs nor the token ever appear in an error string.

use std::collections::BTreeSet;

use serde_json::Value;

use super::transport::{HttpRequest, HttpResponse, HttpTransport};
use super::{
    check_key, check_object_size, is_remote_key, keyed_listing, map_status, send_bearer, AccessToken, ListQuery,
    ListedObject, ObjectStore, ProviderError, Quota,
};

const GRAPH_HOST: &str = "graph.microsoft.com";
#[cfg_attr(not(test), allow(dead_code))] // only tests call this so far
const DRIVE: &str = "https://graph.microsoft.com/v1.0/me/drive";
const APPROOT: &str = "https://graph.microsoft.com/v1.0/me/drive/special/approot";
/// The folder `ensure_container` creates; every layout key lies below it.
const LAYOUT_ROOT: &str = "v1";
/// Largest body sent as one simple upload (Graph's documented limit, 4 MiB);
/// anything larger goes through an upload session.
pub(crate) const SIMPLE_UPLOAD_MAX: usize = 4 * 1024 * 1024;
/// Upload session chunk: Graph requires a multiple of 320 KiB.
pub(crate) const SESSION_CHUNK: usize = 32 * 320 * 1024;
/// Deeper folders than this are not followed while listing.
const MAX_LIST_DEPTH: usize = 16;
/// Children per listing page requested from Graph.
const PAGE_SIZE: usize = 200;

/// How an upload treats an existing name.
#[derive(Clone, Copy)]
enum Mode<'a> {
    /// `conflictBehavior=fail`: an existing name is `AlreadyExists`.
    Create,
    /// `conflictBehavior=replace`, with `If-Match` when an etag is given.
    Replace(Option<&'a str>),
}

impl Mode<'_> {
    fn conflict_behavior(self) -> &'static str {
        match self {
            Mode::Create => "fail",
            Mode::Replace(_) => "replace",
        }
    }

    fn if_match(self) -> Option<(&'static str, String)> {
        match self {
            Mode::Replace(Some(etag)) => Some(("If-Match", etag.to_owned())),
            _ => None,
        }
    }
}

/// One upload attempt's answer.
enum Attempt {
    Done(ListedObject),
    /// The parent folder does not exist (404, or 409 `itemNotFound`).
    MissingParent,
}

pub(crate) struct OneDriveStore<T, A> {
    transport: T,
    token: A,
    tenant_id: String,
    object_id: String,
}

impl<T: HttpTransport, A: AccessToken> OneDriveStore<T, A> {
    /// `tenant_id`/`object_id` are the `tid`/`oid` claims of the signed-in
    /// account (G6 §B); they form the binding identity and must be non-empty
    /// `[A-Za-z0-9-]` text so the identity stays canonical.
    pub(crate) fn new(transport: T, token: A, tenant_id: &str, object_id: &str) -> Result<Self, ProviderError> {
        let valid = |id: &str| !id.is_empty() && id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-');
        if !valid(tenant_id) || !valid(object_id) {
            return Err(ProviderError::Permanent("invalid OneDrive account identity".to_owned()));
        }
        Ok(Self { transport, token, tenant_id: tenant_id.to_owned(), object_id: object_id.to_owned() })
    }

    /// One Graph request with the bearer token. A 401 invalidates the token
    /// and resends once with a fresh one; a second 401 needs a new sign-in.
    async fn graph(&self, method: &str, url: &str, headers: &[(&str, String)], body: &[u8]) -> Result<HttpResponse, ProviderError> {
        let headers = headers.iter().map(|(name, value)| ((*name).to_owned(), value.clone())).collect();
        let req = HttpRequest { method: method.to_owned(), url: url.to_owned(), headers, body: body.to_vec() };
        send_bearer(&self.transport, &self.token, req).await
    }

    /// A request to a pre-authenticated URL: no `Authorization` header.
    async fn preauth(&self, method: &str, url: String, headers: &[(&str, String)], body: Vec<u8>) -> Result<HttpResponse, ProviderError> {
        let headers = headers.iter().map(|(name, value)| ((*name).to_owned(), value.clone())).collect();
        let req = HttpRequest { method: method.to_owned(), url, headers, body };
        self.transport.send(req).await.map_err(ProviderError::from)
    }

    /// POST one folder with `conflictBehavior=fail`. A 409 for the name means
    /// it already exists: success once a GET shows it is a folder.
    async fn create_folder(&self, path: &str) -> Result<(), ProviderError> {
        let (parent, name) = path.rsplit_once('/').unwrap_or(("", path));
        let body = serde_json::json!({ "name": name, "folder": {}, "@microsoft.graph.conflictBehavior": "fail" });
        let resp = self.graph("POST", &children_url(parent), &json_headers(), body.to_string().as_bytes()).await?;
        match resp.status {
            200 | 201 => Ok(()),
            409 if error_code(&resp.body).as_deref() == Some("itemNotFound") => Err(missing_parent_folder()),
            409 => self.confirm_folder(path).await,
            404 => Err(missing_parent_folder()),
            status => Err(map_status(status, &resp.headers)),
        }
    }

    async fn confirm_folder(&self, path: &str) -> Result<(), ProviderError> {
        let resp = self.graph("GET", &item_url(path), &[], &[]).await?;
        match resp.status {
            200 if parse_json(&resp.body, "item")?.get("folder").is_some() => Ok(()),
            200 => Err(ProviderError::Permanent("OneDrive path exists but is not a folder".to_owned())),
            // Deleted between the 409 and the check: try again later.
            404 => Err(ProviderError::Transient("OneDrive folder vanished while being created".to_owned())),
            status => Err(map_status(status, &resp.headers)),
        }
    }

    /// Creates every level of `dir` top-down, so each parent exists first.
    async fn ensure_folders(&self, dir: &str) -> Result<(), ProviderError> {
        let mut end = 0;
        for segment in dir.split('/').filter(|s| !s.is_empty()) {
            end += if end == 0 { segment.len() } else { segment.len() + 1 };
            self.create_folder(&dir[..end]).await?;
        }
        Ok(())
    }

    /// Uploads, creating missing parent folders and retrying exactly once.
    async fn upload_with_parents(&self, key: &str, bytes: &[u8], mode: Mode<'_>) -> Result<ListedObject, ProviderError> {
        if let Attempt::Done(listed) = self.upload(key, bytes, mode).await? {
            return Ok(listed);
        }
        self.ensure_folders(parent_of(key)).await?;
        match self.upload(key, bytes, mode).await? {
            Attempt::Done(listed) => Ok(listed),
            Attempt::MissingParent => {
                Err(ProviderError::Transient("OneDrive parent folder still missing after creating it".to_owned()))
            }
        }
    }

    async fn upload(&self, key: &str, bytes: &[u8], mode: Mode<'_>) -> Result<Attempt, ProviderError> {
        if bytes.len() <= SIMPLE_UPLOAD_MAX {
            self.simple_upload(key, bytes, mode).await
        } else {
            self.session_upload(key, bytes, mode).await
        }
    }

    async fn simple_upload(&self, key: &str, bytes: &[u8], mode: Mode<'_>) -> Result<Attempt, ProviderError> {
        let url = format!("{}?@microsoft.graph.conflictBehavior={}", content_url(key), mode.conflict_behavior());
        let mut headers = vec![("Content-Type", "application/octet-stream".to_owned())];
        headers.extend(mode.if_match());
        let resp = self.graph("PUT", &url, &headers, bytes).await?;
        match resp.status {
            200 | 201 => Ok(Attempt::Done(listed_from(key, bytes.len(), &resp.body))),
            status => upload_refusal(status, &resp, mode),
        }
    }

    /// `createUploadSession` carrying the same conflict behaviour, then the
    /// chunks to the pre-authenticated session URL without `Authorization`.
    async fn session_upload(&self, key: &str, bytes: &[u8], mode: Mode<'_>) -> Result<Attempt, ProviderError> {
        let body = serde_json::json!({ "item": { "@microsoft.graph.conflictBehavior": mode.conflict_behavior() } });
        let mut headers = json_headers();
        headers.extend(mode.if_match());
        let resp = self.graph("POST", &session_url(key), &headers, body.to_string().as_bytes()).await?;
        if resp.status != 200 {
            return upload_refusal(resp.status, &resp, mode);
        }
        let session = parse_json(&resp.body, "upload session")?;
        let upload_url = session
            .get("uploadUrl")
            .and_then(Value::as_str)
            .filter(|url| is_https_preauth(url))
            .ok_or_else(|| ProviderError::Permanent("OneDrive upload session URL refused".to_owned()))?
            .to_owned();
        let total = bytes.len();
        let mut offset = 0;
        while offset < total {
            let end = (offset + SESSION_CHUNK).min(total);
            let range = vec![("Content-Range", format!("bytes {}-{}/{}", offset, end - 1, total))];
            let resp = self.preauth("PUT", upload_url.clone(), &range, bytes[offset..end].to_vec()).await?;
            match (resp.status, end == total) {
                (202, false) => {
                    if !next_range_starts_at(&resp.body, end) {
                        return Err(ProviderError::Transient("OneDrive upload session lost its place".to_owned()));
                    }
                }
                (200 | 201, true) => return Ok(Attempt::Done(listed_from(key, total, &resp.body))),
                (200..=299, _) => {
                    return Err(ProviderError::Transient("OneDrive upload session answered out of order".to_owned()))
                }
                (409, _) if matches!(mode, Mode::Create) => return Err(ProviderError::AlreadyExists),
                // An expired or revoked session URL: start the upload over.
                (401 | 403 | 404, _) => {
                    return Err(ProviderError::Transient("OneDrive upload session rejected".to_owned()))
                }
                (status, _) => return Err(map_status(status, &resp.headers)),
            }
            offset = end;
        }
        Err(ProviderError::Transient("OneDrive upload session never completed".to_owned()))
    }

    /// The single redirect of a content download: HTTPS only, followed once,
    /// without `Authorization`.
    async fn follow_download(&self, resp: &HttpResponse) -> Result<Vec<u8>, ProviderError> {
        let location = resp
            .header("location")
            .filter(|url| is_https_preauth(url))
            .ok_or_else(|| ProviderError::Permanent("OneDrive download redirect refused".to_owned()))?;
        let resp = self.preauth("GET", location.to_owned(), &[], Vec::new()).await?;
        match resp.status {
            200 => Ok(resp.body),
            300..=399 => Err(ProviderError::Permanent("OneDrive download redirected again".to_owned())),
            404 => Err(ProviderError::NotFound),
            // The pre-authenticated URL was refused; the account's token is
            // not involved, so this is never a reason to sign in again.
            401 | 403 => Err(ProviderError::Transient("OneDrive download link rejected".to_owned())),
            status => Err(map_status(status, &resp.headers)),
        }
    }

    /// Every child of `dir` across all pages; `None` when `dir` is missing.
    /// `@odata.nextLink` must stay on `https://graph.microsoft.com`.
    async fn children(&self, dir: &str) -> Result<Option<Vec<Value>>, ProviderError> {
        let mut url = format!("{}?$select=name,eTag,cTag,size,file,folder&$top={PAGE_SIZE}", children_url(dir));
        let mut seen: BTreeSet<String> = BTreeSet::new();
        let mut items = Vec::new();
        loop {
            let resp = self.graph("GET", &url, &[], &[]).await?;
            match resp.status {
                200 => {}
                404 if seen.is_empty() => return Ok(None),
                // A later page vanishing would leave a partial listing.
                404 => return Err(ProviderError::Transient("OneDrive listing page disappeared".to_owned())),
                status => return Err(map_status(status, &resp.headers)),
            }
            let page = parse_json(&resp.body, "listing")?;
            let value = page.get("value").and_then(Value::as_array).ok_or_else(|| malformed("listing"))?;
            items.extend(value.iter().cloned());
            seen.insert(url);
            match page.get("@odata.nextLink") {
                None | Some(Value::Null) => return Ok(Some(items)),
                Some(Value::String(next)) if is_graph_url(next) => {
                    if seen.contains(next) {
                        return Err(ProviderError::Permanent("OneDrive listing repeats a page".to_owned()));
                    }
                    url = next.clone();
                }
                Some(Value::String(_)) => {
                    return Err(ProviderError::Permanent("OneDrive listing link leaves Microsoft Graph".to_owned()))
                }
                Some(_) => return Err(malformed("listing")),
            }
        }
    }
}

impl<T: HttpTransport, A: AccessToken> ObjectStore for OneDriveStore<T, A> {
    fn create_only(&self) -> bool {
        true
    }

    fn identity(&self) -> String {
        format!("onedrive|{}|{}", self.tenant_id, self.object_id)
    }

    /// Walks folders from the query's prefix folder, keeping files whose key
    /// the query accepts. Names that are not safe keys are skipped and never
    /// followed.
    async fn list(&self, q: ListQuery<'_>) -> Result<Vec<ListedObject>, ProviderError> {
        let listing = keyed_listing(q)?;
        let prefix: &str = &listing.prefix;
        let start = parent_of(prefix).to_owned();
        let start = if prefix.ends_with('/') { prefix.trim_end_matches('/').to_owned() } else { start };
        let mut queue = vec![start.clone()];
        let mut visited: BTreeSet<String> = BTreeSet::from([start]);
        let mut found = Vec::new();
        while let Some(dir) = queue.pop() {
            let Some(items) = self.children(&dir).await? else { continue };
            for item in items {
                let name = item.get("name").and_then(Value::as_str).ok_or_else(|| malformed("listing"))?;
                let rel = if dir.is_empty() { name.to_owned() } else { format!("{dir}/{name}") };
                if !is_remote_key(&rel) {
                    continue;
                }
                if item.get("folder").is_some() {
                    let as_dir = format!("{rel}/");
                    let relevant = as_dir.starts_with(prefix) || prefix.starts_with(&as_dir);
                    if relevant && rel.split('/').count() <= MAX_LIST_DEPTH && visited.insert(rel.clone()) {
                        queue.push(rel);
                    }
                } else if item.get("file").is_some() && listing.matches(&rel) {
                    found.push(ListedObject {
                        provider_ref: rel.clone(),
                        size: item.get("size").and_then(Value::as_u64),
                        etag: item_etag(&item),
                        key: rel,
                    });
                }
            }
        }
        found.sort_by(|a, b| a.key.cmp(&b.key));
        Ok(found)
    }

    async fn get(&self, key: &str) -> Result<Vec<u8>, ProviderError> {
        check_key(key)?;
        let resp = self.graph("GET", &content_url(key), &[], &[]).await?;
        match resp.status {
            200 => Ok(resp.body),
            302 => self.follow_download(&resp).await,
            300..=399 => Err(ProviderError::Permanent("unexpected OneDrive redirect".to_owned())),
            status => Err(map_status(status, &resp.headers)),
        }
    }

    async fn put_create(&self, key: &str, bytes: &[u8]) -> Result<ListedObject, ProviderError> {
        check_key(key)?;
        check_object_size(bytes)?;
        self.upload_with_parents(key, bytes, Mode::Create).await
    }

    async fn put_replace(&self, key: &str, bytes: &[u8], if_match: Option<&str>) -> Result<ListedObject, ProviderError> {
        check_key(key)?;
        check_object_size(bytes)?;
        self.upload_with_parents(key, bytes, Mode::Replace(if_match)).await
    }

    async fn delete(&self, key: &str) -> Result<(), ProviderError> {
        check_key(key)?;
        let resp = self.graph("DELETE", &item_url(key), &[], &[]).await?;
        match resp.status {
            200 | 204 | 404 => Ok(()),
            status => Err(map_status(status, &resp.headers)),
        }
    }

    async fn quota(&self) -> Result<Quota, ProviderError> {
        let resp = self.graph("GET", &format!("{DRIVE}?$select=quota"), &[], &[]).await?;
        if resp.status != 200 {
            return Err(map_status(resp.status, &resp.headers));
        }
        let drive = parse_json(&resp.body, "quota")?;
        let quota = drive.get("quota");
        let field = |name: &str| quota.and_then(|q| q.get(name)).and_then(Value::as_u64);
        Ok(Quota { used: field("used"), available: field("remaining") })
    }

    async fn ensure_container(&self) -> Result<(), ProviderError> {
        self.create_folder(LAYOUT_ROOT).await
    }
}

fn item_url(path: &str) -> String {
    format!("{APPROOT}:/{path}:")
}

fn content_url(path: &str) -> String {
    format!("{APPROOT}:/{path}:/content")
}

fn session_url(path: &str) -> String {
    format!("{APPROOT}:/{path}:/createUploadSession")
}

/// `path` is "" for the app folder itself; key paths need no escaping, as
/// they are `[A-Za-z0-9._-]` segments.
fn children_url(path: &str) -> String {
    if path.is_empty() {
        format!("{APPROOT}/children")
    } else {
        format!("{APPROOT}:/{path}:/children")
    }
}

fn json_headers() -> Vec<(&'static str, String)> {
    vec![("Content-Type", "application/json".to_owned())]
}

/// "a/b/c" → "a/b"; "a" → "".
fn parent_of(path: &str) -> &str {
    path.rfind('/').map_or("", |i| &path[..i])
}

fn malformed(what: &str) -> ProviderError {
    ProviderError::Transient(format!("malformed OneDrive {what}"))
}

fn missing_parent_folder() -> ProviderError {
    ProviderError::Transient("OneDrive parent folder missing".to_owned())
}

/// A malformed body is `Transient`: fail closed and retry, never treat it as
/// an empty answer.
fn parse_json(body: &[u8], what: &str) -> Result<Value, ProviderError> {
    serde_json::from_slice(body).map_err(|_| malformed(what))
}

/// Graph's `error.code`, if the body carries one.
fn error_code(body: &[u8]) -> Option<String> {
    let value: Value = serde_json::from_slice(body).ok()?;
    value.get("error")?.get("code")?.as_str().map(str::to_owned)
}

/// The refusal of a simple upload or `createUploadSession`.
fn upload_refusal(status: u16, resp: &HttpResponse, mode: Mode<'_>) -> Result<Attempt, ProviderError> {
    let missing_parent = status == 404 || (status == 409 && error_code(&resp.body).as_deref() == Some("itemNotFound"));
    match (status, mode) {
        // An etag for an object that cannot exist is a failed precondition.
        _ if missing_parent && matches!(mode, Mode::Replace(Some(_))) => Err(ProviderError::PreconditionFailed),
        _ if missing_parent => Ok(Attempt::MissingParent),
        (409, Mode::Create) => Err(ProviderError::AlreadyExists),
        _ => Err(map_status(status, &resp.headers)),
    }
}

/// The created item's etag and size, best effort: the upload already
/// succeeded, so an unreadable body must not turn it into a failure.
fn listed_from(key: &str, size: usize, body: &[u8]) -> ListedObject {
    let item: Value = serde_json::from_slice(body).unwrap_or(Value::Null);
    ListedObject {
        key: key.to_owned(),
        provider_ref: key.to_owned(),
        size: Some(item.get("size").and_then(Value::as_u64).unwrap_or(size as u64)),
        etag: item_etag(&item),
    }
}

/// `eTag`, else `cTag`: either is accepted by `If-Match`.
fn item_etag(item: &Value) -> Option<String> {
    item.get("eTag").or_else(|| item.get("cTag")).and_then(Value::as_str).map(str::to_owned)
}

/// A 202 chunk answer whose `nextExpectedRanges` (when given) resumes at `at`.
fn next_range_starts_at(body: &[u8], at: usize) -> bool {
    let Ok(value) = serde_json::from_slice::<Value>(body) else { return true };
    let Some(first) = value.get("nextExpectedRanges").and_then(Value::as_array).and_then(|r| r.first()) else {
        return true;
    };
    first.as_str().and_then(|r| r.split('-').next()).and_then(|start| start.parse::<usize>().ok()) == Some(at)
}

/// An absolute `https` URL with a host and no embedded credentials.
fn is_https_preauth(raw: &str) -> bool {
    url::Url::parse(raw)
        .is_ok_and(|url| url.scheme() == "https" && url.host_str().is_some() && url.username().is_empty() && url.password().is_none())
}

/// A `nextLink` that stays on Microsoft Graph over HTTPS.
fn is_graph_url(raw: &str) -> bool {
    url::Url::parse(raw).is_ok_and(|url| {
        url.scheme() == "https"
            && url.host_str() == Some(GRAPH_HOST)
            && url.port().is_none()
            && url.username().is_empty()
            && url.password().is_none()
    })
}

/// An in-memory Microsoft Graph behind a [`MockTransport`]: the app folder,
/// folder creation with `conflictBehavior=fail` (409 on an existing name),
/// simple uploads honouring `conflictBehavior` and `If-Match`, upload
/// sessions on a separate pre-authenticated host, content downloads as a 302
/// to a pre-authenticated download host, paginated children with
/// `@odata.nextLink`, DELETE and quota. Uploads never create missing parent
/// folders here, so the adapter's parent creation is always exercised.
///
/// [`MockTransport`]: crate::sync::provider::transport::MockTransport
#[cfg(test)]
pub(crate) mod tests_support {
    use std::collections::BTreeMap;
    use std::sync::{Arc, Mutex};

    use super::*;
    use crate::sync::provider::transport::MockTransport;
    use crate::sync::provider::StaticToken;

    pub(crate) const TOKEN: &str = "graph-access-token-SECRET-1";
    pub(crate) const TENANT: &str = "9188040d-6c67-4c5b-b112-36a304b66dad";
    pub(crate) const OBJECT: &str = "00000000-0000-0000-66f3-3332eca7ea81";
    /// Pre-authenticated hosts: a request carrying `Authorization` is refused.
    pub(crate) const DOWNLOAD_HOST: &str = "dl.onedrive.example.test";
    pub(crate) const UPLOAD_HOST: &str = "up.onedrive.example.test";

    #[derive(Debug, Clone)]
    pub(crate) enum Node {
        Folder,
        File { bytes: Vec<u8>, etag: String, ctag: String },
    }

    struct Session {
        path: String,
        replace: bool,
        received: Vec<u8>,
    }

    pub(crate) struct GraphServer {
        /// App-folder-relative paths; "" is the app folder.
        pub nodes: BTreeMap<String, Node>,
        pub token: String,
        /// Children per page; small so listings paginate.
        pub page_size: usize,
        /// Answer a missing parent with 409 `itemNotFound` instead of 404.
        pub missing_parent_409: bool,
        /// Uploads keep answering "missing parent" even after folders exist.
        pub parents_never_appear: bool,
        /// Host written into `@odata.nextLink`.
        pub next_link_origin: String,
        /// Scheme and host of the download redirect.
        pub download_origin: String,
        /// Query appended to download URLs (a pre-authentication secret).
        pub download_query: String,
        /// Another writer creates the target with these bytes right after a
        /// create upload session opens.
        pub race_on_session: Option<Vec<u8>>,
        pub quota: (u64, u64),
        sessions: BTreeMap<String, Session>,
        next_id: u64,
    }

    impl GraphServer {
        pub(crate) fn new() -> Self {
            Self {
                nodes: BTreeMap::from([(String::new(), Node::Folder)]),
                token: TOKEN.to_owned(),
                page_size: 2,
                missing_parent_409: false,
                parents_never_appear: false,
                next_link_origin: format!("https://{GRAPH_HOST}"),
                download_origin: format!("https://{DOWNLOAD_HOST}"),
                download_query: "?tempauth=PREAUTH-SECRET".to_owned(),
                race_on_session: None,
                quota: (1234, 5678),
                sessions: BTreeMap::new(),
                next_id: 0,
            }
        }

        pub(crate) fn file(&self, path: &str) -> Option<Vec<u8>> {
            match self.nodes.get(path) {
                Some(Node::File { bytes, .. }) => Some(bytes.clone()),
                _ => None,
            }
        }

        /// Stores a file directly, creating its folders.
        pub(crate) fn seed(&mut self, path: &str, bytes: &[u8]) {
            let mut dir = parent_of(path);
            while !dir.is_empty() {
                self.nodes.insert(dir.to_owned(), Node::Folder);
                dir = parent_of(dir);
            }
            let node = self.new_file(bytes.to_vec());
            self.nodes.insert(path.to_owned(), node);
        }

        fn new_file(&mut self, bytes: Vec<u8>) -> Node {
            self.next_id += 1;
            let n = self.next_id;
            Node::File { bytes, etag: format!("\"{{ID-{n}}},1\""), ctag: format!("\"c:{{ID-{n}}},1\"") }
        }

        fn parent_is_folder(&self, path: &str) -> bool {
            !self.parents_never_appear && matches!(self.nodes.get(parent_of(path)), Some(Node::Folder))
        }

        fn missing_parent(&self) -> HttpResponse {
            error(if self.missing_parent_409 { 409 } else { 404 }, "itemNotFound")
        }

        fn handle(&mut self, req: &HttpRequest) -> HttpResponse {
            let url = url::Url::parse(&req.url).expect("absolute URL");
            match url.host_str() {
                Some(DOWNLOAD_HOST) => return self.download(req, &url),
                Some(UPLOAD_HOST) => return self.chunk(req, &url),
                Some(GRAPH_HOST) if url.scheme() == "https" => {}
                _ => return status(404),
            }
            if req.header("authorization") != Some(format!("Bearer {}", self.token).as_str()) {
                return error(401, "InvalidAuthenticationToken");
            }
            let raw = req.url.split('?').next().unwrap_or_default();
            let query = url.query().unwrap_or_default();
            let Some(rest) = raw.strip_prefix(DRIVE) else { return status(404) };
            if rest.is_empty() && req.method == "GET" {
                let (used, remaining) = self.quota;
                return json(200, &serde_json::json!({ "quota": { "used": used, "remaining": remaining, "total": used + remaining } }));
            }
            let Some(sub) = rest.strip_prefix("/special/approot") else { return status(404) };
            let (path, action) = if sub.is_empty() {
                (String::new(), "")
            } else if sub == "/children" {
                (String::new(), "/children")
            } else if let Some(inner) = sub.strip_prefix(":/") {
                let Some((path, action)) = inner.split_once(':') else { return status(400) };
                (path.to_owned(), action)
            } else {
                return status(404);
            };
            match (req.method.as_str(), action) {
                ("GET", "") => match self.nodes.get(&path) {
                    Some(node) => json(200, &item_json(&path, node)),
                    None => error(404, "itemNotFound"),
                },
                ("DELETE", "") => {
                    if path.is_empty() || self.nodes.remove(&path).is_none() {
                        return error(404, "itemNotFound");
                    }
                    let nested = format!("{path}/");
                    self.nodes.retain(|p, _| !p.starts_with(&nested));
                    status(204)
                }
                ("GET", "/children") => self.children(&path, query),
                ("POST", "/children") => self.create_folder(&path, &req.body),
                ("GET", "/content") => match self.nodes.get(&path) {
                    Some(Node::File { .. }) => HttpResponse {
                        status: 302,
                        headers: vec![(
                            "Location".to_owned(),
                            format!("{}/{path}{}", self.download_origin, self.download_query),
                        )],
                        body: Vec::new(),
                    },
                    _ => error(404, "itemNotFound"),
                },
                ("PUT", "/content") => {
                    let fail = query.contains("@microsoft.graph.conflictBehavior=fail");
                    self.write(&path, req.body.clone(), fail, req.header("if-match"))
                }
                ("POST", "/createUploadSession") => self.open_session(&path, req),
                _ => status(405),
            }
        }

        fn children(&self, dir: &str, query: &str) -> HttpResponse {
            if !matches!(self.nodes.get(dir), Some(Node::Folder)) {
                return error(404, "itemNotFound");
            }
            let skip = query
                .split('&')
                .find_map(|pair| pair.strip_prefix("$skiptoken="))
                .and_then(|n| n.parse::<usize>().ok())
                .unwrap_or(0);
            let prefix = if dir.is_empty() { String::new() } else { format!("{dir}/") };
            let children: Vec<(&String, &Node)> = self
                .nodes
                .iter()
                .filter(|(p, _)| !p.is_empty() && p.starts_with(&prefix) && !p[prefix.len()..].contains('/'))
                .collect();
            let page: Vec<Value> =
                children.iter().skip(skip).take(self.page_size).map(|(p, node)| item_json(p, node)).collect();
            let mut body = serde_json::json!({ "value": page });
            if skip + self.page_size < children.len() {
                let base = if dir.is_empty() {
                    "/v1.0/me/drive/special/approot/children".to_owned()
                } else {
                    format!("/v1.0/me/drive/special/approot:/{dir}:/children")
                };
                body["@odata.nextLink"] =
                    Value::String(format!("{}{base}?$skiptoken={}", self.next_link_origin, skip + self.page_size));
            }
            json(200, &body)
        }

        fn create_folder(&mut self, parent: &str, body: &[u8]) -> HttpResponse {
            let Ok(request) = serde_json::from_slice::<Value>(body) else { return status(400) };
            let Some(name) = request.get("name").and_then(Value::as_str) else { return status(400) };
            if request.get("folder").is_none() {
                return status(400);
            }
            let fail = request.get("@microsoft.graph.conflictBehavior").and_then(Value::as_str) == Some("fail");
            if !matches!(self.nodes.get(parent), Some(Node::Folder)) {
                return self.missing_parent();
            }
            let path = if parent.is_empty() { name.to_owned() } else { format!("{parent}/{name}") };
            if self.nodes.contains_key(&path) {
                if fail {
                    return error(409, "nameAlreadyExists");
                }
                return json(200, &item_json(&path, &self.nodes[&path]));
            }
            self.nodes.insert(path.clone(), Node::Folder);
            json(201, &item_json(&path, &Node::Folder))
        }

        /// A completed upload: `fail` refuses an existing name, `if_match`
        /// must name the current eTag or cTag.
        fn write(&mut self, path: &str, bytes: Vec<u8>, fail: bool, if_match: Option<&str>) -> HttpResponse {
            if !self.parent_is_folder(path) {
                return self.missing_parent();
            }
            let existing = self.nodes.get(path).cloned();
            if let Some(refused) = refuse_write(existing.as_ref(), fail, if_match) {
                return refused;
            }
            let node = self.new_file(bytes);
            self.nodes.insert(path.to_owned(), node.clone());
            json(if existing.is_some() { 200 } else { 201 }, &item_json(path, &node))
        }

        fn open_session(&mut self, path: &str, req: &HttpRequest) -> HttpResponse {
            let Ok(request) = serde_json::from_slice::<Value>(&req.body) else { return status(400) };
            let behaviour = request.get("item").and_then(|i| i.get("@microsoft.graph.conflictBehavior")).and_then(Value::as_str);
            let replace = behaviour == Some("replace");
            if !self.parent_is_folder(path) {
                return self.missing_parent();
            }
            if let Some(refused) = refuse_write(self.nodes.get(path), !replace, req.header("if-match")) {
                return refused;
            }
            self.next_id += 1;
            let id = format!("session-{}", self.next_id);
            self.sessions.insert(id.clone(), Session { path: path.to_owned(), replace, received: Vec::new() });
            if !replace {
                if let Some(bytes) = self.race_on_session.take() {
                    let node = self.new_file(bytes);
                    self.nodes.insert(path.to_owned(), node);
                }
            }
            json(200, &serde_json::json!({ "uploadUrl": format!("https://{UPLOAD_HOST}/{id}?sig=UPLOAD-SECRET"), "expirationDateTime": "2030-01-01T00:00:00Z" }))
        }

        fn chunk(&mut self, req: &HttpRequest, url: &url::Url) -> HttpResponse {
            if req.header("authorization").is_some() {
                return status(400);
            }
            let id = url.path().trim_start_matches('/').to_owned();
            let Some(session) = self.sessions.get_mut(&id) else { return error(404, "itemNotFound") };
            let Some((start, total)) = req.header("content-range").and_then(parse_content_range) else { return status(400) };
            if req.method != "PUT" || start != session.received.len() || start + req.body.len() > total {
                return status(416);
            }
            session.received.extend_from_slice(&req.body);
            if session.received.len() < total {
                let next = session.received.len();
                return json(202, &serde_json::json!({ "nextExpectedRanges": [format!("{next}-")] }));
            }
            let session = self.sessions.remove(&id).expect("session");
            self.write(&session.path, session.received, !session.replace, None)
        }

        fn download(&self, req: &HttpRequest, url: &url::Url) -> HttpResponse {
            if req.header("authorization").is_some() || url.scheme() != "https" {
                return status(400);
            }
            match self.nodes.get(url.path().trim_start_matches('/')) {
                Some(Node::File { bytes, .. }) => HttpResponse { status: 200, headers: Vec::new(), body: bytes.clone() },
                _ => status(404),
            }
        }
    }

    fn refuse_write(existing: Option<&Node>, fail: bool, if_match: Option<&str>) -> Option<HttpResponse> {
        if matches!(existing, Some(Node::Folder)) || (fail && existing.is_some()) {
            return Some(error(409, "nameAlreadyExists"));
        }
        if let Some(expected) = if_match {
            let current = match existing {
                Some(Node::File { etag, ctag, .. }) => expected == etag || expected == ctag,
                _ => false,
            };
            if !current {
                return Some(error(412, "preconditionFailed"));
            }
        }
        None
    }

    /// `bytes 0-9/20` → (0, 20).
    fn parse_content_range(value: &str) -> Option<(usize, usize)> {
        let (range, total) = value.strip_prefix("bytes ")?.split_once('/')?;
        let (start, _) = range.split_once('-')?;
        Some((start.parse().ok()?, total.parse().ok()?))
    }

    fn item_json(path: &str, node: &Node) -> Value {
        let name = path.rsplit('/').next().unwrap_or_default();
        match node {
            Node::Folder => serde_json::json!({ "id": format!("id:{path}"), "name": name, "folder": { "childCount": 0 } }),
            Node::File { bytes, etag, ctag } => serde_json::json!({
                "id": format!("id:{path}"), "name": name, "eTag": etag, "cTag": ctag,
                "size": bytes.len(), "file": { "mimeType": "application/octet-stream" }
            }),
        }
    }

    pub(crate) fn status(code: u16) -> HttpResponse {
        HttpResponse { status: code, headers: Vec::new(), body: Vec::new() }
    }

    pub(crate) fn json(code: u16, value: &Value) -> HttpResponse {
        HttpResponse {
            status: code,
            headers: vec![("Content-Type".to_owned(), "application/json".to_owned())],
            body: value.to_string().into_bytes(),
        }
    }

    pub(crate) fn error(code: u16, graph_code: &str) -> HttpResponse {
        json(code, &serde_json::json!({ "error": { "code": graph_code, "message": "emulated" } }))
    }

    /// A store over `server` whose token source hands out `tokens` in order.
    pub(crate) fn over_with(
        server: GraphServer,
        tokens: &[&str],
    ) -> (OneDriveStore<MockTransport, StaticToken>, MockTransport, Arc<Mutex<GraphServer>>) {
        let state = Arc::new(Mutex::new(server));
        let shared = Arc::clone(&state);
        let transport = MockTransport::new(move |req| shared.lock().unwrap().handle(req));
        let store = OneDriveStore::new(transport.clone(), StaticToken::sequence(tokens), TENANT, OBJECT).unwrap();
        (store, transport, state)
    }

    pub(crate) fn over(server: GraphServer) -> (OneDriveStore<MockTransport, StaticToken>, MockTransport, Arc<Mutex<GraphServer>>) {
        over_with(server, &[TOKEN])
    }

    /// The contract-suite backend.
    pub(crate) fn emulated() -> (OneDriveStore<MockTransport, StaticToken>, MockTransport) {
        let (store, transport, _) = over(GraphServer::new());
        (store, transport)
    }

    /// The contract suite's fault injection. The adapter answers a 401 by
    /// refreshing and resending once, so an emulated 401 is scripted for both
    /// attempts; every other status needs one.
    pub(crate) fn fail_next(transport: &MockTransport, status: u16, headers: &[(&str, &str)]) {
        transport.fail_next(status, headers);
        if status == 401 {
            transport.fail_next(status, headers);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::tests_support::*;
    use super::*;
    use crate::sync::provider::transport::{MockTransport, TransportFailure};
    use crate::sync::provider::{binding_fp, Bound, RemoteKind, StaticToken};

    const SEG: &str = "v1/vault-a/log/dev-a/000000000001-000000000003.seg";
    const REC: &str = "v1/vault-a/devices/dev-a.rec";

    fn no_auth(req: &HttpRequest) -> bool {
        req.header("authorization").is_none()
    }

    fn calls(transport: &MockTransport) -> Vec<String> {
        transport
            .requests()
            .iter()
            .map(|r| format!("{} {}", r.method, r.url.trim_start_matches(APPROOT).trim_start_matches(DRIVE)))
            .collect()
    }

    fn big(len: usize) -> Vec<u8> {
        (0..len).map(|i| (i % 251) as u8).collect()
    }

    #[test]
    fn identity_is_tenant_and_object() {
        let (store, _) = emulated();
        assert_eq!(store.identity(), format!("onedrive|{TENANT}|{OBJECT}"));
        assert!(store.create_only());
        for (tid, oid) in [("", OBJECT), (TENANT, ""), ("a|b", OBJECT), (TENANT, "o id"), (TENANT, "ö")] {
            let made = OneDriveStore::new(MockTransport::new(|_| unreachable!()), StaticToken::new(TOKEN), tid, oid);
            assert!(matches!(made, Err(ProviderError::Permanent(_))), "{tid:?} {oid:?}");
        }
    }

    #[tokio::test]
    async fn folder_409_is_success_and_creates_with_fail() {
        let (store, transport, state) = over(GraphServer::new());
        store.ensure_container().await.unwrap();
        store.ensure_container().await.unwrap();
        assert_eq!(calls(&transport), ["POST /children", "POST /children", "GET :/v1:"], "409, then confirmed a folder");
        for req in transport.requests().iter().filter(|r| r.method == "POST") {
            let body: Value = serde_json::from_slice(&req.body).unwrap();
            assert_eq!(body["@microsoft.graph.conflictBehavior"], "fail");
            assert_eq!(body["name"], "v1");
            assert!(body["folder"].is_object());
        }
        assert!(matches!(state.lock().unwrap().nodes.get("v1"), Some(Node::Folder)));

        // A file squatting on the folder name is never treated as the folder.
        let (store, _, state) = over(GraphServer::new());
        state.lock().unwrap().seed("v1", b"not a folder");
        assert!(matches!(store.ensure_container().await, Err(ProviderError::Permanent(_))));
    }

    #[tokio::test]
    async fn file_409_is_already_exists_and_never_overwrites() {
        let (store, transport, state) = over(GraphServer::new());
        store.put_create(SEG, b"ours").await.unwrap();
        assert_eq!(store.put_create(SEG, b"theirs").await, Err(ProviderError::AlreadyExists));
        assert_eq!(state.lock().unwrap().file(SEG).unwrap(), b"ours");
        let puts: Vec<HttpRequest> = transport.requests().into_iter().filter(|r| r.method == "PUT").collect();
        assert!(!puts.is_empty());
        for put in puts {
            assert!(put.url.ends_with(":/content?@microsoft.graph.conflictBehavior=fail"), "{}", put.url);
        }
    }

    #[tokio::test]
    async fn missing_parent_creates_folders_top_down_then_retries_once() {
        for missing_parent_409 in [false, true] {
            let mut server = GraphServer::new();
            server.missing_parent_409 = missing_parent_409;
            let (store, transport, state) = over(server);
            let created = store.put_create(SEG, b"seg").await.unwrap();
            assert_eq!(created.key, SEG);
            assert!(created.etag.is_some());
            assert_eq!(
                calls(&transport),
                [
                    format!("PUT :/{SEG}:/content?@microsoft.graph.conflictBehavior=fail"),
                    "POST /children".to_owned(),
                    "POST :/v1:/children".to_owned(),
                    "POST :/v1/vault-a:/children".to_owned(),
                    "POST :/v1/vault-a/log:/children".to_owned(),
                    format!("PUT :/{SEG}:/content?@microsoft.graph.conflictBehavior=fail"),
                ],
                "409 itemNotFound: {missing_parent_409}"
            );
            assert_eq!(state.lock().unwrap().file(SEG).unwrap(), b"seg");
        }

        // A parent that never appears is retried exactly once, then Transient.
        let mut server = GraphServer::new();
        server.parents_never_appear = true;
        let (store, transport, _) = over(server);
        assert!(matches!(store.put_create(SEG, b"seg").await, Err(ProviderError::Transient(_))));
        assert_eq!(transport.requests().iter().filter(|r| r.method == "PUT").count(), 2);
    }

    #[tokio::test]
    async fn upload_session_uses_conflict_behavior_fail_and_no_authorization() {
        let (store, transport, state) = over(GraphServer::new());
        let body = big(SESSION_CHUNK + 3 * 1024 * 1024);
        let created = store.put_create(SEG, &body).await.unwrap();
        assert_eq!(created.size, Some(body.len() as u64));
        assert_eq!(state.lock().unwrap().file(SEG).unwrap(), body);

        let requests = transport.requests();
        let sessions: Vec<&HttpRequest> = requests.iter().filter(|r| r.url.ends_with(":/createUploadSession")).collect();
        assert!(!sessions.is_empty());
        for session in &sessions {
            let sent: Value = serde_json::from_slice(&session.body).unwrap();
            assert_eq!(sent["item"]["@microsoft.graph.conflictBehavior"], "fail");
            assert!(session.header("authorization").is_some());
        }
        let chunks: Vec<&HttpRequest> = requests.iter().filter(|r| r.url.contains(UPLOAD_HOST)).collect();
        assert_eq!(chunks.len(), 2);
        assert!(chunks.iter().all(|c| no_auth(c)), "chunks go to the session URL without Authorization");
        assert_eq!(chunks[0].header("content-range"), Some(format!("bytes 0-{}/{}", SESSION_CHUNK - 1, body.len()).as_str()));
        assert_eq!(chunks[1].header("content-range"), Some(format!("bytes {SESSION_CHUNK}-{}/{}", body.len() - 1, body.len()).as_str()));
        assert!(!requests.iter().any(|r| r.method == "PUT" && r.url.contains(GRAPH_HOST)), "no simple upload above 4 MiB");

        // An existing name is refused when the session opens.
        assert_eq!(store.put_create(SEG, &big(SIMPLE_UPLOAD_MAX + 1)).await, Err(ProviderError::AlreadyExists));
        assert_eq!(state.lock().unwrap().file(SEG).unwrap(), body);
        assert_eq!(store.get(SEG).await.unwrap(), body);
    }

    #[tokio::test]
    async fn upload_session_commit_conflict_is_already_exists() {
        // Another writer takes the name after our session opened: the commit
        // is refused (409) and their object stays.
        let (store, _, state) = over(GraphServer::new());
        state.lock().unwrap().seed("v1/vault-a/log/dev-a/keep.seg", b"k");
        state.lock().unwrap().race_on_session = Some(b"theirs".to_vec());
        assert_eq!(store.put_create(SEG, &big(SIMPLE_UPLOAD_MAX + 10)).await, Err(ProviderError::AlreadyExists));
        assert_eq!(state.lock().unwrap().file(SEG).unwrap(), b"theirs");
    }

    #[tokio::test]
    async fn upload_session_url_must_be_https() {
        let (store, transport, state) = over(GraphServer::new());
        state.lock().unwrap().seed("v1/vault-a/log/dev-a/keep.seg", b"k");
        let before = transport.request_count();
        transport.script_next(Ok(json(200, &serde_json::json!({ "uploadUrl": format!("http://{UPLOAD_HOST}/s1") }))));
        assert!(matches!(store.put_create(SEG, &big(SIMPLE_UPLOAD_MAX + 1)).await, Err(ProviderError::Permanent(_))));
        assert_eq!(transport.request_count(), before + 1, "no chunk goes to an http session URL");
    }

    #[tokio::test]
    async fn large_replace_uses_session_with_if_match() {
        let (store, transport, state) = over(GraphServer::new());
        let first = store.put_replace(REC, b"rec-1", None).await.unwrap();
        let etag = first.etag.unwrap();
        let body = big(SIMPLE_UPLOAD_MAX + 1);
        assert_eq!(store.put_replace(REC, &body, Some("\"stale\"")).await, Err(ProviderError::PreconditionFailed));
        store.put_replace(REC, &body, Some(&etag)).await.unwrap();
        assert_eq!(state.lock().unwrap().file(REC).unwrap(), body);
        let session = transport.requests().into_iter().rfind(|r| r.url.ends_with(":/createUploadSession")).unwrap();
        assert_eq!(session.header("if-match"), Some(etag.as_str()));
        let sent: Value = serde_json::from_slice(&session.body).unwrap();
        assert_eq!(sent["item"]["@microsoft.graph.conflictBehavior"], "replace");
        // An etag for an object that does not exist is a failed precondition.
        assert_eq!(
            store.put_replace("v1/vault-b/devices/dev-z.rec", b"x", Some(&etag)).await,
            Err(ProviderError::PreconditionFailed)
        );
    }

    #[tokio::test]
    async fn download_redirect_is_followed_once_without_authorization() {
        let (store, transport, state) = over(GraphServer::new());
        state.lock().unwrap().seed(SEG, b"payload");
        assert_eq!(store.get(SEG).await.unwrap(), b"payload");
        let requests = transport.requests();
        assert_eq!(requests.len(), 2);
        assert!(requests[0].header("authorization").is_some());
        assert!(requests[1].url.starts_with(&format!("https://{DOWNLOAD_HOST}/")));
        assert!(no_auth(&requests[1]), "the pre-authenticated URL never gets the token");
        assert!(requests[1].headers.is_empty());
    }

    #[tokio::test]
    async fn download_redirect_to_http_or_elsewhere_is_refused() {
        let (store, transport, state) = over(GraphServer::new());
        state.lock().unwrap().seed(SEG, b"payload");
        state.lock().unwrap().download_origin = format!("http://{DOWNLOAD_HOST}");
        let err = store.get(SEG).await.unwrap_err();
        assert!(matches!(err, ProviderError::Permanent(_)), "{err:?}");
        assert!(!format!("{err:?}").contains("PREAUTH-SECRET"));
        assert_eq!(transport.request_count(), 1, "nothing is sent to an http target");

        let redirect = |status: u16, location: Option<&str>| HttpResponse {
            status,
            headers: location.map(|l| vec![("Location".to_owned(), l.to_owned())]).unwrap_or_default(),
            body: Vec::new(),
        };
        for (resp, what) in [
            (redirect(301, Some("https://dl.onedrive.example.test/x")), "301"),
            (redirect(307, Some("https://dl.onedrive.example.test/x")), "307"),
            (redirect(302, None), "no location"),
            (redirect(302, Some("/relative/path")), "relative"),
            (redirect(302, Some("https://user:pw@dl.onedrive.example.test/x")), "credentials"),
            (redirect(302, Some("javascript:alert(1)")), "scheme"),
        ] {
            let before = transport.request_count();
            transport.script_next(Ok(resp));
            assert!(matches!(store.get(SEG).await, Err(ProviderError::Permanent(_))), "{what}");
            assert_eq!(transport.request_count(), before + 1, "{what}: not followed");
        }

        // A second redirect from the download host is never followed.
        let (store, transport, state) = over(GraphServer::new());
        state.lock().unwrap().seed(SEG, b"payload");
        transport.script_next(Ok(redirect(302, Some("https://dl.onedrive.example.test/a"))));
        transport.script_next(Ok(redirect(302, Some("https://dl.onedrive.example.test/b"))));
        assert!(matches!(store.get(SEG).await, Err(ProviderError::Permanent(_))));
        assert_eq!(transport.request_count(), 2);
    }

    #[tokio::test]
    async fn listing_paginates_with_next_link() {
        let (store, transport, state) = over(GraphServer::new());
        let keys: Vec<String> = (1..=5).map(|i| format!("v1/vault-a/log/dev-a/00000000000{i}-00000000000{i}.seg")).collect();
        for key in &keys {
            state.lock().unwrap().seed(key, b"s");
        }
        let listed = store.list(ListQuery::Prefix("v1/vault-a/log/dev-a/")).await.unwrap();
        assert_eq!(listed.iter().map(|o| o.key.clone()).collect::<Vec<_>>(), keys);
        assert!(listed.iter().all(|o| o.size == Some(1) && o.etag.is_some() && o.provider_ref == o.key));
        assert_eq!(transport.request_count(), 3, "5 children at 2 per page");
        assert!(transport.requests().iter().skip(1).all(|r| r.url.contains("$skiptoken=")));
    }

    #[tokio::test]
    async fn next_link_off_graph_is_refused() {
        for origin in ["https://graph.microsoft.com.evil.test", "http://graph.microsoft.com", "https://evil.test", "https://graph.microsoft.com:8443"] {
            let (store, transport, state) = over(GraphServer::new());
            for i in 1..=3 {
                state.lock().unwrap().seed(&format!("v1/vault-a/log/dev-a/00000000000{i}-00000000000{i}.seg"), b"s");
            }
            state.lock().unwrap().next_link_origin = origin.to_owned();
            let err = store.list(ListQuery::Prefix("v1/vault-a/log/dev-a/")).await.unwrap_err();
            assert!(matches!(err, ProviderError::Permanent(_)), "{origin}: {err:?}");
            assert_eq!(transport.request_count(), 1, "{origin}: the link is never followed");
        }

        // A link that repeats a page would loop forever.
        let (store, transport, _) = over(GraphServer::new());
        let again = format!("{}?$skiptoken=1", children_url(""));
        let page = json(200, &serde_json::json!({ "value": [], "@odata.nextLink": again }));
        transport.script_next(Ok(page.clone()));
        transport.script_next(Ok(page));
        assert!(matches!(store.list(ListQuery::Prefix("")).await, Err(ProviderError::Permanent(_))));
        assert_eq!(transport.request_count(), 2);
    }

    #[tokio::test]
    async fn malformed_listing_is_never_empty() {
        for body in [&b"not json"[..], br#"{"items": []}"#, br#"{"value": [{"size": 1}]}"#, br#"{"value": [], "@odata.nextLink": 7}"#] {
            let (store, transport, _) = over(GraphServer::new());
            transport.script_next(Ok(HttpResponse { status: 200, headers: Vec::new(), body: body.to_vec() }));
            let err = store.list(ListQuery::Prefix("")).await.unwrap_err();
            assert!(matches!(err, ProviderError::Transient(_)), "{}: {err:?}", String::from_utf8_lossy(body));
        }
    }

    #[tokio::test]
    async fn malicious_names_are_skipped_and_never_followed() {
        let (store, transport, state) = over(GraphServer::new());
        {
            let mut server = state.lock().unwrap();
            server.seed(SEG, b"good");
            server.seed("v1/vault-a/log/dev-a/evil name.seg", b"x");
            server.seed("v1/vault-a/log/dev-a/..", b"x");
            server.seed("v1/vault-a/log/dev ö/000000000001-000000000001.seg", b"x");
        }
        let listed = store.list(ListQuery::Prefix("")).await.unwrap();
        assert_eq!(listed.iter().map(|o| o.key.as_str()).collect::<Vec<_>>(), [SEG]);
        assert!(!transport.requests().iter().any(|r| r.url.contains("dev ö") || r.url.contains("dev%20")));
    }

    #[tokio::test]
    async fn props_query_walks_only_the_exact_folder() {
        let (store, transport, state) = over(GraphServer::new());
        let other_device = "v1/vault-a/log/dev-b/000000000001-000000000001.seg";
        {
            let mut server = state.lock().unwrap();
            server.seed(SEG, b"a");
            server.seed(other_device, b"b");
            server.seed("v1/vault-a/manifest.json", b"m");
            server.seed("v1/vault-b/manifest.json", b"m");
            server.seed("v1/vault-a/blob/ab/ab12.blob", b"x");
        }
        let q = ListQuery::Props { vault_id: Some("vault-a"), kind: Some(RemoteKind::Seg), device_id: Some("dev-a") };
        let listed = store.list(q).await.unwrap();
        assert_eq!(listed.iter().map(|o| o.key.as_str()).collect::<Vec<_>>(), [SEG]);
        assert_eq!(calls(&transport), ["GET :/v1/vault-a/log/dev-a:/children?$select=name,eTag,cTag,size,file,folder&$top=200"]);

        let manifests = store.list(ListQuery::Props { vault_id: None, kind: Some(RemoteKind::Manifest), device_id: None }).await.unwrap();
        assert_eq!(
            manifests.iter().map(|o| o.key.as_str()).collect::<Vec<_>>(),
            ["v1/vault-a/manifest.json", "v1/vault-b/manifest.json"]
        );
        let bad = ListQuery::Props { vault_id: Some("../x"), kind: None, device_id: None };
        assert!(matches!(store.list(bad).await, Err(ProviderError::Permanent(_))));
    }

    #[tokio::test]
    async fn unauthorized_refreshes_once_then_needs_reconsent() {
        let mut server = GraphServer::new();
        server.token = "fresh-token-SECRET-2".to_owned();
        let (store, transport, state) = over_with(server, &[TOKEN, "fresh-token-SECRET-2"]);
        state.lock().unwrap().seed(SEG, b"payload");
        assert_eq!(store.get(SEG).await.unwrap(), b"payload");
        assert_eq!(store.token.invalidations(), 1);
        let auth: Vec<Option<String>> =
            transport.requests().iter().map(|r| r.header("authorization").map(str::to_owned)).collect();
        assert_eq!(
            auth,
            [Some(format!("Bearer {TOKEN}")), Some("Bearer fresh-token-SECRET-2".to_owned()), None],
            "retried with the fresh token; the download carries none"
        );

        // A refreshed token that is still refused needs a new sign-in.
        let mut server = GraphServer::new();
        server.token = "never-accepted".to_owned();
        let (store, transport, _) = over_with(server, &[TOKEN, "fresh-token-SECRET-2"]);
        assert_eq!(store.put_create(SEG, b"x").await, Err(ProviderError::Auth { reconsent: true }));
        assert_eq!(store.token.invalidations(), 1);
        assert_eq!(transport.request_count(), 2, "exactly one retry");
    }

    #[tokio::test]
    async fn token_never_appears_in_errors() {
        let (store, transport, state) = over(GraphServer::new());
        state.lock().unwrap().seed(SEG, b"payload");
        let mut errors = Vec::new();
        for status in [400, 401, 403, 404, 409, 412, 429, 500, 503, 507] {
            fail_next(&transport, status, &[("Retry-After", "3")]);
            errors.push(store.get(SEG).await.unwrap_err());
            fail_next(&transport, status, &[]);
            // A 404 here is a missing parent: the folders are made and the
            // retry succeeds, so only failures are collected.
            if let Err(err) = store.put_create(REC, b"x").await {
                errors.push(err);
            }
        }
        transport.script_next(Err(TransportFailure::Other("connection reset".to_owned())));
        errors.push(store.get(SEG).await.unwrap_err());
        state.lock().unwrap().download_origin = format!("http://{DOWNLOAD_HOST}");
        errors.push(store.get(SEG).await.unwrap_err());
        for err in errors {
            let shown = format!("{err:?}");
            for secret in [TOKEN, "PREAUTH-SECRET", "UPLOAD-SECRET", "tempauth", "Bearer"] {
                assert!(!shown.contains(secret), "{shown}");
            }
        }
        let shown = format!("{:?}", transport.requests());
        assert!(!shown.contains(TOKEN), "requests print redacted");
    }

    #[tokio::test]
    async fn replace_honours_if_match_and_delete_is_idempotent() {
        let (store, transport, state) = over(GraphServer::new());
        let first = store.put_replace(REC, b"rec-1", None).await.unwrap();
        let etag = first.etag.clone().unwrap();
        let replace = transport.requests().into_iter().rfind(|r| r.method == "PUT").unwrap();
        assert!(replace.url.ends_with("?@microsoft.graph.conflictBehavior=replace"));
        assert_eq!(store.put_replace(REC, b"rec-x", Some("\"stale\"")).await, Err(ProviderError::PreconditionFailed));
        store.put_replace(REC, b"rec-2", Some(&etag)).await.unwrap();
        assert_eq!(state.lock().unwrap().file(REC).unwrap(), b"rec-2");
        assert_eq!(store.put_replace(REC, b"rec-3", Some(&etag)).await, Err(ProviderError::PreconditionFailed));

        // cTag is accepted as the precondition too.
        let ctag = match state.lock().unwrap().nodes.get(REC) {
            Some(Node::File { ctag, .. }) => ctag.clone(),
            _ => unreachable!(),
        };
        store.put_replace(REC, b"rec-4", Some(&ctag)).await.unwrap();

        store.delete(REC).await.unwrap();
        assert!(state.lock().unwrap().file(REC).is_none());
        store.delete(REC).await.unwrap();
        fail_next(&transport, 500, &[]);
        assert!(matches!(store.delete(REC).await, Err(ProviderError::Transient(_))));
    }

    #[tokio::test]
    async fn quota_reads_used_and_remaining() {
        let (store, transport, _) = over(GraphServer::new());
        assert_eq!(store.quota().await.unwrap(), Quota { used: Some(1234), available: Some(5678) });
        assert_eq!(transport.requests()[0].url, format!("{DRIVE}?$select=quota"));
        transport.script_next(Ok(json(200, &serde_json::json!({ "quota": {} }))));
        assert_eq!(store.quota().await.unwrap(), Quota { used: None, available: None });
    }

    #[tokio::test]
    async fn bound_onedrive_sends_nothing_on_mismatch() {
        let (store, transport) = emulated();
        let bound = Bound::new(store, binding_fp("onedrive|other-tenant|other-object"));
        assert_eq!(bound.get(SEG).await, Err(ProviderError::BindingMismatch));
        assert_eq!(bound.ensure_container().await, Err(ProviderError::BindingMismatch));
        assert_eq!(transport.request_count(), 0);
        let (store, _) = emulated();
        let bound = Bound::new(store, binding_fp(&format!("onedrive|{TENANT}|{OBJECT}")));
        bound.ensure_container().await.unwrap();
    }

    #[test]
    fn url_guards() {
        assert!(is_graph_url("https://graph.microsoft.com/v1.0/me/drive/items/x/children?$skiptoken=a"));
        for bad in [
            "http://graph.microsoft.com/v1.0/x",
            "https://graph.microsoft.com.evil.test/x",
            "https://evil.test/graph.microsoft.com",
            "https://u@graph.microsoft.com/x",
            "https://graph.microsoft.com:444/x",
            "/v1.0/me/drive",
        ] {
            assert!(!is_graph_url(bad), "{bad}");
        }
        assert!(is_https_preauth("https://public.bn.files.1drv.com/y4m?x=1"));
        for bad in ["http://public.bn.files.1drv.com/y", "https://a:b@host/x", "//host/x", "file:///etc/passwd", "data:,x"] {
            assert!(!is_https_preauth(bad), "{bad}");
        }
        assert!(next_range_starts_at(br#"{"nextExpectedRanges":["327680-"]}"#, 327_680));
        assert!(!next_range_starts_at(br#"{"nextExpectedRanges":["0-"]}"#, 327_680));
        assert_eq!(SESSION_CHUNK % (320 * 1024), 0);
    }
}