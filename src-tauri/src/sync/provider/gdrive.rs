//! Google Drive adapter over Drive v3 in the hidden `appDataFolder` (§4.2 N-5).
//!
//! Drive has no folders here and no create-only write: every object is a flat
//! file in `appDataFolder` whose `name` is the full remote key and whose
//! `appProperties` are `{vault_id, kind, device_id}` parsed from that key.
//! Listings query only those properties, never names, and every result is
//! re-checked by parsing its name. Names may repeat (two devices can create
//! the same key concurrently), so `get_all` returns every copy by file id and
//! the resolver in `publish` decides what a duplicate means.
//!
//! Requests carry `Authorization: Bearer <token>` from an [`AccessToken`]. A
//! 401 invalidates the token and retries once with a fresh one; a second 401
//! means the user must sign in again. Tokens never reach an error string.

use std::collections::BTreeMap;

use percent_encoding::{utf8_percent_encode, AsciiSet, NON_ALPHANUMERIC};
use serde::Deserialize;

use super::transport::{HttpRequest, HttpResponse, HttpTransport};
use super::{
    check_key, check_object_size, map_status, parse_layout_key, props_match, send_bearer, AccessToken, ListQuery,
    ListedObject, ObjectStore, ProviderError, Quota, RemoteKind,
};
use crate::sync::envelope::is_id;
use crate::sync::publish::RemoteObject;

const API_BASE: &str = "https://www.googleapis.com/drive/v3";
const UPLOAD_BASE: &str = "https://www.googleapis.com/upload/drive/v3";
/// The per-app hidden space; the only parent any object ever has.
const APP_DATA: &str = "appDataFolder";
const LIST_FIELDS: &str = "nextPageToken,files(id,name,size,appProperties)";
const PAGE_SIZE: u32 = 1000;
/// More pages than this means the listing is not converging.
const MAX_LIST_PAGES: usize = 100_000;
/// Drive file ids are short `[A-Za-z0-9_-]` strings; anything longer is not one.
const MAX_FILE_ID_LEN: usize = 256;

/// RFC 3986 unreserved characters stay literal in query values.
const QUERY_VALUE: &AsciiSet = &NON_ALPHANUMERIC.remove(b'-').remove(b'.').remove(b'_').remove(b'~');

pub(crate) struct GDriveStore<T, A> {
    transport: T,
    tokens: A,
    /// The Google account's stable `sub` claim; identity is `gdrive|<sub>`.
    sub: String,
}

/// One file from a listing whose name is a layout key matching the query.
#[derive(Debug, Clone, PartialEq, Eq)]
struct DriveCopy {
    id: String,
    name: String,
    size: Option<u64>,
}

#[derive(Deserialize)]
struct FileList {
    #[serde(rename = "nextPageToken")]
    next_page_token: Option<String>,
    /// Required: a 200 without `files` is not a listing, never an empty one.
    files: Vec<FileEntry>,
}

#[derive(Deserialize)]
struct FileEntry {
    id: String,
    name: String,
    /// Drive encodes int64 as a JSON string.
    size: Option<String>,
    #[serde(rename = "appProperties", default)]
    app_properties: BTreeMap<String, String>,
}

#[derive(Deserialize)]
struct CreatedFile {
    id: String,
}

#[derive(Deserialize)]
struct About {
    #[cfg_attr(not(test), allow(dead_code))] // only tests call this so far
    #[serde(rename = "storageQuota")]
    storage_quota: StorageQuota,
}

#[derive(Deserialize)]
struct StorageQuota {
    /// Absent for unlimited storage.
    #[cfg_attr(not(test), allow(dead_code))] // only tests call this so far
    limit: Option<String>,
    #[cfg_attr(not(test), allow(dead_code))] // only tests call this so far
    usage: Option<String>,
}

#[derive(Deserialize)]
struct ErrorBody {
    error: Option<ErrorInner>,
}

#[derive(Deserialize)]
struct ErrorInner {
    #[serde(default)]
    errors: Vec<ErrorItem>,
}

#[derive(Deserialize)]
struct ErrorItem {
    reason: Option<String>,
}

impl<T: HttpTransport, A: AccessToken> GDriveStore<T, A> {
    /// `sub` comes from the sign-in's `id_token` (see `sync::oauth`).
    pub(crate) fn new(transport: T, tokens: A, sub: String) -> Result<Self, ProviderError> {
        if sub.is_empty() || sub.chars().any(|c| c.is_control() || c.is_whitespace() || c == '|') {
            return Err(ProviderError::Permanent("Drive account id is missing or malformed".to_owned()));
        }
        Ok(Self { transport, tokens, sub })
    }

    /// Sends with a bearer token; on a 401 invalidates it and resends once.
    async fn send(
        &self,
        method: &str,
        url: String,
        headers: &[(&str, &str)],
        body: Vec<u8>,
    ) -> Result<HttpResponse, ProviderError> {
        let req = HttpRequest {
            method: method.to_owned(),
            url,
            headers: headers.iter().map(|(n, v)| ((*n).to_owned(), (*v).to_owned())).collect(),
            body,
        };
        send_bearer(&self.transport, &self.tokens, req).await
    }

    /// Every file in `appDataFolder` whose properties match and whose name is
    /// a layout key matching the same fields exactly (and agreeing with its
    /// own properties). Foreign or malicious names are skipped.
    async fn list_props(
        &self,
        vault_id: Option<&str>,
        kind: Option<RemoteKind>,
        device_id: Option<&str>,
    ) -> Result<Vec<DriveCopy>, ProviderError> {
        if vault_id.is_some_and(|v| !is_id(v)) || device_id.is_some_and(|d| !is_id(d)) {
            return Err(ProviderError::Permanent("unsafe list properties".to_owned()));
        }
        let q = props_query(vault_id, kind, device_id);
        let mut out = Vec::new();
        let mut token: Option<String> = None;
        for _ in 0..MAX_LIST_PAGES {
            let resp = self.send("GET", list_url(&q, token.as_deref()), &[], Vec::new()).await?;
            if resp.status != 200 {
                return Err(drive_error(&resp));
            }
            let page: FileList = serde_json::from_slice(&resp.body)
                .map_err(|_| ProviderError::Transient("Drive list response is not a file list".to_owned()))?;
            for file in page.files {
                if !props_match(&file.name, vault_id, kind, device_id) || !props_agree(&file.name, &file.app_properties)
                {
                    continue;
                }
                if !is_file_id(&file.id) {
                    return Err(ProviderError::Permanent("Drive listing returned a malformed file id".to_owned()));
                }
                let size = file.size.as_deref().and_then(|s| s.parse().ok());
                out.push(DriveCopy { id: file.id, name: file.name, size });
            }
            match page.next_page_token {
                None => return Ok(out),
                Some(next) if !next.is_empty() && token.as_deref() != Some(next.as_str()) => token = Some(next),
                // A page token that does not move would end the listing early
                // or loop forever: never treat it as complete.
                Some(_) => return Err(ProviderError::Permanent("Drive listing is not making progress".to_owned())),
            }
        }
        Err(ProviderError::Permanent("Drive listing did not finish".to_owned()))
    }

    /// Every copy named exactly `key`, metadata only.
    async fn copies(&self, key: &str) -> Result<Vec<DriveCopy>, ProviderError> {
        check_key(key)?;
        let (vault_id, kind, device_id) = key_props(key)?;
        // A manifest or blob has no device; it is filtered by name, not queried.
        let device = (!device_id.is_empty()).then_some(device_id.as_str());
        let mut copies = self.list_props(Some(vault_id.as_str()), Some(kind), device).await?;
        copies.retain(|copy| copy.name == key);
        Ok(copies)
    }

    /// The bytes of one file; `None` if it vanished since it was listed.
    async fn download(&self, id: &str) -> Result<Option<Vec<u8>>, ProviderError> {
        let resp = self.send("GET", format!("{API_BASE}/files/{id}?alt=media"), &[], Vec::new()).await?;
        match resp.status {
            200 => Ok(Some(resp.body)),
            404 => Ok(None),
            _ => Err(drive_error(&resp)),
        }
    }

    /// One multipart upload of a new file named `key` in `appDataFolder`.
    async fn upload(&self, key: &str, bytes: &[u8]) -> Result<ListedObject, ProviderError> {
        let (vault_id, kind, device_id) = key_props(key)?;
        let metadata = serde_json::json!({
            "name": key,
            "parents": [APP_DATA],
            "appProperties": { "vault_id": vault_id, "kind": kind.as_str(), "device_id": device_id },
        });
        let metadata = serde_json::to_vec(&metadata)
            .map_err(|_| ProviderError::Permanent("could not encode Drive metadata".to_owned()))?;
        let (content_type, body) = multipart(&metadata, bytes);
        let url = format!("{UPLOAD_BASE}/files?uploadType=multipart&fields=id");
        let resp = self.send("POST", url, &[("Content-Type", content_type.as_str())], body).await?;
        if resp.status != 200 {
            return Err(drive_error(&resp));
        }
        let id = created_id(&resp)?;
        Ok(ListedObject { key: key.to_owned(), provider_ref: id, size: Some(bytes.len() as u64), etag: None })
    }
}

impl<T: HttpTransport, A: AccessToken> ObjectStore for GDriveStore<T, A> {
    /// Drive accepts duplicate names; `put_create` only pre-checks.
    fn create_only(&self) -> bool {
        false
    }

    fn identity(&self) -> String {
        format!("gdrive|{}", self.sub)
    }

    async fn list(&self, q: ListQuery<'_>) -> Result<Vec<ListedObject>, ProviderError> {
        let (vault_id, kind, device_id) = match q {
            ListQuery::Props { vault_id, kind, device_id } => (vault_id, kind, device_id),
            ListQuery::Prefix(_) => {
                return Err(ProviderError::Unsupported("Drive lists by appProperties, not by name prefix".to_owned()))
            }
        };
        let copies = self.list_props(vault_id, kind, device_id).await?;
        Ok(copies
            .into_iter()
            .map(|copy| ListedObject { key: copy.name, provider_ref: copy.id, size: copy.size, etag: None })
            .collect())
    }

    /// The single object under `key`. Copies with identical bytes are one
    /// object; copies that differ are refused (callers that handle duplicate
    /// names use `get_all`).
    async fn get(&self, key: &str) -> Result<Vec<u8>, ProviderError> {
        let mut copies = self.get_all(key).await?.into_iter();
        let Some(first) = copies.next() else {
            return Err(ProviderError::NotFound);
        };
        if copies.any(|other| other.bytes != first.bytes) {
            return Err(ProviderError::Permanent("duplicate name".to_owned()));
        }
        Ok(first.bytes)
    }

    async fn get_all(&self, key: &str) -> Result<Vec<RemoteObject>, ProviderError> {
        let mut out = Vec::new();
        for copy in self.copies(key).await? {
            if let Some(bytes) = self.download(&copy.id).await? {
                out.push(RemoteObject { provider_ref: copy.id, bytes });
            }
        }
        Ok(out)
    }

    /// Pre-checks the name, then uploads. Drive cannot refuse a duplicate, so
    /// a concurrent create can still leave two copies; `publish` sees both
    /// through `get_all` and settles a fork or a self-copy.
    async fn put_create(&self, key: &str, bytes: &[u8]) -> Result<ListedObject, ProviderError> {
        check_key(key)?;
        check_object_size(bytes)?;
        if !self.copies(key).await?.is_empty() {
            return Err(ProviderError::AlreadyExists);
        }
        self.upload(key, bytes).await
    }

    /// Uploads with no pre-check, next to any copy already under `key`.
    async fn put_create_unchecked(&self, key: &str, bytes: &[u8]) -> Result<ListedObject, ProviderError> {
        check_key(key)?;
        check_object_size(bytes)?;
        self.upload(key, bytes).await
    }

    /// Drive v3 has no etag precondition, so `if_match` is refused. Without
    /// one, the single existing file is overwritten (or created if missing);
    /// with duplicates there is no single object to replace.
    async fn put_replace(&self, key: &str, bytes: &[u8], if_match: Option<&str>) -> Result<ListedObject, ProviderError> {
        check_key(key)?;
        check_object_size(bytes)?;
        if if_match.is_some() {
            return Err(ProviderError::Unsupported("Drive has no conditional replace".to_owned()));
        }
        let copies = self.copies(key).await?;
        let id = match copies.as_slice() {
            [] => return self.upload(key, bytes).await,
            [only] => only.id.clone(),
            _ => return Err(ProviderError::Permanent("duplicate name".to_owned())),
        };
        let url = format!("{UPLOAD_BASE}/files/{id}?uploadType=media&fields=id");
        let resp = self.send("PATCH", url, &[("Content-Type", "application/octet-stream")], bytes.to_vec()).await?;
        if resp.status != 200 {
            return Err(drive_error(&resp));
        }
        Ok(ListedObject { key: key.to_owned(), provider_ref: id, size: Some(bytes.len() as u64), etag: None })
    }

    /// A media update of exactly the file `provider_ref` names (D1a): no
    /// name lookup, no upload. A file that is gone answers 404 → NotFound.
    async fn put_replace_ref(&self, key: &str, provider_ref: &str, bytes: &[u8]) -> Result<ListedObject, ProviderError> {
        check_key(key)?;
        check_object_size(bytes)?;
        if !is_file_id(provider_ref) {
            return Err(ProviderError::Permanent("malformed Drive file id".to_owned()));
        }
        let url = format!("{UPLOAD_BASE}/files/{provider_ref}?uploadType=media&fields=id");
        let resp = self.send("PATCH", url, &[("Content-Type", "application/octet-stream")], bytes.to_vec()).await?;
        match resp.status {
            200 => Ok(ListedObject {
                key: key.to_owned(),
                provider_ref: provider_ref.to_owned(),
                size: Some(bytes.len() as u64),
                etag: None,
            }),
            404 => Err(ProviderError::NotFound),
            _ => Err(drive_error(&resp)),
        }
    }

    /// Deletes every copy under `key`.
    async fn delete(&self, key: &str) -> Result<(), ProviderError> {
        for copy in self.copies(key).await? {
            self.delete_ref(&copy.id).await?;
        }
        Ok(())
    }

    /// Deletes one file by id (appData files bypass the trash). Missing → Ok.
    async fn delete_ref(&self, provider_ref: &str) -> Result<(), ProviderError> {
        if !is_file_id(provider_ref) {
            return Err(ProviderError::Permanent("malformed Drive file id".to_owned()));
        }
        let resp = self.send("DELETE", format!("{API_BASE}/files/{provider_ref}"), &[], Vec::new()).await?;
        match resp.status {
            200 | 204 | 404 => Ok(()),
            _ => Err(drive_error(&resp)),
        }
    }

    async fn quota(&self) -> Result<Quota, ProviderError> {
        let resp = self.send("GET", format!("{API_BASE}/about?fields=storageQuota"), &[], Vec::new()).await?;
        if resp.status != 200 {
            return Err(drive_error(&resp));
        }
        let about: About = serde_json::from_slice(&resp.body)
            .map_err(|_| ProviderError::Transient("Drive about response is malformed".to_owned()))?;
        let used: Option<u64> = about.storage_quota.usage.as_deref().and_then(|s| s.parse().ok());
        let limit: Option<u64> = about.storage_quota.limit.as_deref().and_then(|s| s.parse().ok());
        let available = match (limit, used) {
            (Some(limit), Some(used)) => Some(limit.saturating_sub(used)),
            _ => None,
        };
        Ok(Quota { used, available })
    }

    /// `appDataFolder` always exists and no folder is ever created.
    async fn ensure_container(&self) -> Result<(), ProviderError> {
        Ok(())
    }
}

/// The `q` for a Props listing: only `appProperties has {...}` clauses plus
/// `trashed=false`, never a name condition.
fn props_query(vault_id: Option<&str>, kind: Option<RemoteKind>, device_id: Option<&str>) -> String {
    let mut clauses = Vec::new();
    let pairs = [("vault_id", vault_id), ("kind", kind.map(RemoteKind::as_str)), ("device_id", device_id)];
    for (name, value) in pairs {
        if let Some(value) = value {
            clauses.push(format!("appProperties has {{ key='{name}' and value='{}' }}", escape_query_value(value)));
        }
    }
    clauses.push("trashed=false".to_owned());
    clauses.join(" and ")
}

/// Drive query string literals escape `\` and `'` with a backslash.
fn escape_query_value(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for c in value.chars() {
        if matches!(c, '\\' | '\'') {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

fn list_url(q: &str, page_token: Option<&str>) -> String {
    let mut url = format!(
        "{API_BASE}/files?spaces={APP_DATA}&q={}&fields={}&pageSize={PAGE_SIZE}",
        utf8_percent_encode(q, QUERY_VALUE),
        utf8_percent_encode(LIST_FIELDS, QUERY_VALUE)
    );
    if let Some(token) = page_token {
        url.push_str("&pageToken=");
        url.push_str(&utf8_percent_encode(token, QUERY_VALUE).to_string());
    }
    url
}

/// A `multipart/related` body (metadata JSON, then the bytes) and its
/// content type. The boundary is chosen so it occurs in neither part.
fn multipart(metadata: &[u8], bytes: &[u8]) -> (String, Vec<u8>) {
    let mut n: u64 = 0;
    let boundary = loop {
        let candidate = format!("sayknow-part-{n:016x}");
        if !contains(metadata, candidate.as_bytes()) && !contains(bytes, candidate.as_bytes()) {
            break candidate;
        }
        n += 1;
    };
    let mut body = Vec::with_capacity(metadata.len() + bytes.len() + 4 * boundary.len() + 160);
    body.extend_from_slice(format!("--{boundary}\r\nContent-Type: application/json; charset=UTF-8\r\n\r\n").as_bytes());
    body.extend_from_slice(metadata);
    body.extend_from_slice(format!("\r\n--{boundary}\r\nContent-Type: application/octet-stream\r\n\r\n").as_bytes());
    body.extend_from_slice(bytes);
    body.extend_from_slice(format!("\r\n--{boundary}--\r\n").as_bytes());
    (format!("multipart/related; boundary={boundary}"), body)
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    haystack.windows(needle.len()).any(|window| window == needle)
}

fn created_id(resp: &HttpResponse) -> Result<String, ProviderError> {
    // The upload happened; an unreadable answer leaves its id unknown, which
    // the caller resolves by listing (never assumed failed).
    let created: CreatedFile = serde_json::from_slice(&resp.body)
        .map_err(|_| ProviderError::Transient("Drive upload response is malformed".to_owned()))?;
    if !is_file_id(&created.id) {
        return Err(ProviderError::Transient("Drive upload returned a malformed file id".to_owned()));
    }
    Ok(created.id)
}

/// `(vault_id, kind, device_id)` for a layout key, the device `""` for a
/// manifest or blob (the value stored in `appProperties`). Anything that is
/// not an exact §4.2 key is `Permanent`: there is nothing it could name.
fn key_props(key: &str) -> Result<(String, RemoteKind, String), ProviderError> {
    let props = parse_layout_key(key).ok_or_else(|| ProviderError::Permanent("not a remote layout key".to_owned()))?;
    Ok((props.vault_id, props.kind, props.device_id.unwrap_or_default()))
}

/// A listed file's `vault_id` and `kind` properties must be the ones its name
/// implies, and so must `device_id` unless it is empty (manifest, blob) or
/// absent: a file the query matched by properties that its name contradicts is
/// not ours to read.
fn props_agree(name: &str, props: &BTreeMap<String, String>) -> bool {
    let Ok((vault_id, kind, device_id)) = key_props(name) else {
        return false;
    };
    let device_ok = match props.get("device_id") {
        Some(value) => *value == device_id,
        None => device_id.is_empty(),
    };
    props.get("vault_id").map(String::as_str) == Some(vault_id.as_str())
        && props.get("kind").map(String::as_str) == Some(kind.as_str())
        && device_ok
}

fn is_file_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= MAX_FILE_ID_LEN
        && id.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_'))
}

/// Drive reports rate limits and a full Drive as 403 with a reason; any
/// other status goes through the shared table. The body is never echoed.
fn drive_error(resp: &HttpResponse) -> ProviderError {
    if resp.status == 403 {
        let reasons: Vec<String> = serde_json::from_slice::<ErrorBody>(&resp.body)
            .ok()
            .and_then(|body| body.error)
            .map(|inner| inner.errors.into_iter().filter_map(|item| item.reason).collect())
            .unwrap_or_default();
        for reason in &reasons {
            match reason.as_str() {
                "rateLimitExceeded" | "userRateLimitExceeded" | "dailyLimitExceeded" => {
                    return map_status(429, &resp.headers)
                }
                "storageQuotaExceeded" => return ProviderError::QuotaExceeded,
                _ => {}
            }
        }
    }
    map_status(resp.status, &resp.headers)
}

/// An in-memory Drive v3 `appDataFolder` behind a [`MockTransport`]: bearer
/// token checks, appProperties queries (anything else in `q` is a 400),
/// multipart upload, `alt=media`, media PATCH, pagination, and duplicate
/// names. The transport records every request.
#[cfg(test)]
pub(crate) mod tests_support {
    use std::sync::{Arc, Mutex};

    use super::*;
    use crate::sync::provider::transport::MockTransport;
    use crate::sync::provider::StaticToken;

    pub(crate) const SUB: &str = "109876543210987654321";
    pub(crate) const TOKEN: &str = "ya29.test-access-token-7f3a9c";
    pub(crate) const FOLDER_MIME: &str = "application/vnd.google-apps.folder";

    #[derive(Debug, Clone)]
    pub(crate) struct StoredFile {
        pub id: String,
        pub name: String,
        pub parents: Vec<String>,
        pub app_properties: BTreeMap<String, String>,
        pub mime_type: Option<String>,
        pub bytes: Vec<u8>,
        pub trashed: bool,
    }

    pub(crate) struct DriveServer {
        /// In creation order; names may repeat.
        pub files: Vec<StoredFile>,
        pub page_size: usize,
        /// Bearer tokens the server accepts; anything else is a 401.
        pub accepted_tokens: Vec<String>,
        /// Requests that tried to create a folder.
        pub folder_creates: usize,
        next_id: u64,
    }

    impl DriveServer {
        pub(crate) fn new() -> Self {
            Self { files: Vec::new(), page_size: 1000, accepted_tokens: vec![TOKEN.to_owned()], folder_creates: 0, next_id: 0 }
        }

        fn new_id(&mut self) -> String {
            self.next_id += 1;
            format!("1drv_file-{:04}", self.next_id)
        }

        /// Stores a file in `appDataFolder` directly; returns its id.
        pub(crate) fn seed(&mut self, name: &str, props: &[(&str, &str)], bytes: &[u8]) -> String {
            let id = self.new_id();
            self.files.push(StoredFile {
                id: id.clone(),
                name: name.to_owned(),
                parents: vec![APP_DATA.to_owned()],
                app_properties: props.iter().map(|(k, v)| ((*k).to_owned(), (*v).to_owned())).collect(),
                mime_type: None,
                bytes: bytes.to_vec(),
                trashed: false,
            });
            id
        }

        /// Stores a file under a layout key with the properties the adapter
        /// would give it.
        pub(crate) fn seed_key(&mut self, key: &str, bytes: &[u8]) -> String {
            let (vault_id, kind, device_id) = key_props(key).expect("layout key");
            self.seed(key, &[("vault_id", &vault_id), ("kind", kind.as_str()), ("device_id", &device_id)], bytes)
        }

        pub(crate) fn copies(&self, name: &str) -> Vec<Vec<u8>> {
            self.files.iter().filter(|f| f.name == name).map(|f| f.bytes.clone()).collect()
        }

        pub(crate) fn handle(&mut self, req: &HttpRequest) -> HttpResponse {
            let token = req.header("authorization").and_then(|v| v.strip_prefix("Bearer "));
            if !token.is_some_and(|t| self.accepted_tokens.iter().any(|a| a == t)) {
                return error(401, "authError");
            }
            let url = url::Url::parse(&req.url).unwrap();
            if url.scheme() != "https" || url.host_str() != Some("www.googleapis.com") {
                return error(400, "badHost");
            }
            let query: BTreeMap<String, String> = url.query_pairs().into_owned().collect();
            let path = url.path().to_owned();
            match (req.method.as_str(), path.as_str()) {
                ("GET", "/drive/v3/files") => self.list(&query),
                ("POST", "/drive/v3/files") => {
                    // Metadata-only create (how folders are made).
                    let Ok(meta) = serde_json::from_slice::<serde_json::Value>(&req.body) else {
                        return error(400, "parseError");
                    };
                    self.create(&meta, Vec::new())
                }
                ("POST", "/upload/drive/v3/files") => {
                    if query.get("uploadType").map(String::as_str) != Some("multipart") {
                        return error(400, "badUploadType");
                    }
                    match parse_multipart(req) {
                        Some((meta, media)) => self.create(&meta, media),
                        None => error(400, "badMultipart"),
                    }
                }
                ("GET", "/drive/v3/about") => {
                    let used: usize = self.files.iter().map(|f| f.bytes.len()).sum();
                    json(200, &serde_json::json!({"storageQuota": {"limit": "16106127360", "usage": used.to_string()}}))
                }
                (method, path) => {
                    if let Some(id) = path.strip_prefix("/drive/v3/files/") {
                        let Some(index) = self.files.iter().position(|f| f.id == id) else {
                            return error(404, "notFound");
                        };
                        match method {
                            "GET" if query.get("alt").map(String::as_str) == Some("media") => HttpResponse {
                                status: 200,
                                headers: Vec::new(),
                                body: self.files[index].bytes.clone(),
                            },
                            "DELETE" => {
                                self.files.remove(index);
                                HttpResponse { status: 204, headers: Vec::new(), body: Vec::new() }
                            }
                            _ => error(400, "badRequest"),
                        }
                    } else if let Some(id) = path.strip_prefix("/upload/drive/v3/files/") {
                        if method != "PATCH" || query.get("uploadType").map(String::as_str) != Some("media") {
                            return error(400, "badRequest");
                        }
                        let Some(file) = self.files.iter_mut().find(|f| f.id == id) else {
                            return error(404, "notFound");
                        };
                        file.bytes = req.body.clone();
                        json(200, &serde_json::json!({"id": id}))
                    } else {
                        error(404, "notFound")
                    }
                }
            }
        }

        fn create(&mut self, meta: &serde_json::Value, bytes: Vec<u8>) -> HttpResponse {
            let mime_type = meta.get("mimeType").and_then(|v| v.as_str()).map(str::to_owned);
            if mime_type.as_deref() == Some(FOLDER_MIME) {
                self.folder_creates += 1;
            }
            let Some(name) = meta.get("name").and_then(|v| v.as_str()) else {
                return error(400, "missingName");
            };
            // Without parents, Drive puts the file in My Drive's root.
            let parents = match meta.get("parents").and_then(|v| v.as_array()) {
                Some(parents) => parents.iter().filter_map(|p| p.as_str().map(str::to_owned)).collect(),
                None => vec!["root".to_owned()],
            };
            let app_properties = meta
                .get("appProperties")
                .and_then(|v| v.as_object())
                .map(|props| {
                    props.iter().filter_map(|(k, v)| v.as_str().map(|v| (k.clone(), v.to_owned()))).collect()
                })
                .unwrap_or_default();
            let id = self.new_id();
            self.files.push(StoredFile {
                id: id.clone(),
                name: name.to_owned(),
                parents,
                app_properties,
                mime_type,
                bytes,
                trashed: false,
            });
            json(200, &serde_json::json!({"id": id}))
        }

        fn list(&self, query: &BTreeMap<String, String>) -> HttpResponse {
            if query.get("spaces").map(String::as_str) != Some(APP_DATA) {
                return error(400, "spacesRequired");
            }
            let Some((props, trashed_false)) = query.get("q").and_then(|q| parse_q(q)) else {
                return error(400, "invalidQuery");
            };
            let start: usize = match query.get("pageToken") {
                Some(token) => match token.strip_prefix("p").and_then(|n| n.parse().ok()) {
                    Some(start) => start,
                    None => return error(400, "invalidPageToken"),
                },
                None => 0,
            };
            let matching: Vec<&StoredFile> = self
                .files
                .iter()
                .filter(|f| f.parents.iter().any(|p| p == APP_DATA))
                .filter(|f| !(trashed_false && f.trashed))
                .filter(|f| props.iter().all(|(k, v)| f.app_properties.get(k) == Some(v)))
                .collect();
            let end = matching.len().min(start + self.page_size);
            let files: Vec<serde_json::Value> = matching[start.min(end)..end]
                .iter()
                .map(|f| {
                    serde_json::json!({
                        "id": f.id, "name": f.name, "size": f.bytes.len().to_string(), "appProperties": f.app_properties,
                    })
                })
                .collect();
            let mut page = serde_json::json!({ "files": files });
            if end < matching.len() {
                page["nextPageToken"] = serde_json::Value::String(format!("p{end}"));
            }
            json(200, &page)
        }
    }

    /// Parses the only `q` grammar the adapter may send:
    /// `appProperties has { key='K' and value='V' }` clauses and
    /// `trashed=false`, joined by ` and `. Anything else is `None`.
    pub(crate) fn parse_q(q: &str) -> Option<(Vec<(String, String)>, bool)> {
        let mut rest = q;
        let mut props = Vec::new();
        let mut trashed_false = false;
        loop {
            if let Some(r) = rest.strip_prefix("appProperties has { key=") {
                let (key, r) = quoted(r)?;
                let (value, r) = quoted(r.strip_prefix(" and value=")?)?;
                rest = r.strip_prefix(" }")?;
                props.push((key, value));
            } else if let Some(r) = rest.strip_prefix("trashed=false") {
                trashed_false = true;
                rest = r;
            } else {
                return None;
            }
            if rest.is_empty() {
                return Some((props, trashed_false));
            }
            rest = rest.strip_prefix(" and ")?;
        }
    }

    fn quoted(s: &str) -> Option<(String, &str)> {
        let s = s.strip_prefix('\'')?;
        let mut out = String::new();
        let mut chars = s.char_indices();
        while let Some((i, c)) = chars.next() {
            match c {
                '\\' => out.push(chars.next()?.1),
                '\'' => return Some((out, &s[i + 1..])),
                c => out.push(c),
            }
        }
        None
    }

    /// Splits a `multipart/related` upload into its metadata JSON and media.
    pub(crate) fn parse_multipart(req: &HttpRequest) -> Option<(serde_json::Value, Vec<u8>)> {
        let boundary = req.header("content-type")?.strip_prefix("multipart/related; boundary=")?;
        let delimiter = format!("\r\n--{boundary}");
        let rest = req.body.strip_prefix(format!("--{boundary}\r\n").as_bytes())?;
        let (first, rest) = split_once(rest, delimiter.as_bytes())?;
        let (second, rest) = split_once(rest.strip_prefix(b"\r\n")?, delimiter.as_bytes())?;
        if rest != b"--\r\n" {
            return None;
        }
        let (_, meta) = split_once(first, b"\r\n\r\n")?;
        let (_, media) = split_once(second, b"\r\n\r\n")?;
        Some((serde_json::from_slice(meta).ok()?, media.to_vec()))
    }

    fn split_once<'a>(haystack: &'a [u8], needle: &[u8]) -> Option<(&'a [u8], &'a [u8])> {
        let at = haystack.windows(needle.len()).position(|window| window == needle)?;
        Some((&haystack[..at], &haystack[at + needle.len()..]))
    }

    fn json(status: u16, value: &serde_json::Value) -> HttpResponse {
        HttpResponse {
            status,
            headers: vec![("Content-Type".to_owned(), "application/json".to_owned())],
            body: serde_json::to_vec(value).unwrap(),
        }
    }

    pub(crate) fn error(status: u16, reason: &str) -> HttpResponse {
        json(
            status,
            &serde_json::json!({"error": {"code": status, "message": reason, "errors": [{"reason": reason}]}}),
        )
    }

    pub(crate) type EmulatedDrive = GDriveStore<MockTransport, StaticToken>;

    pub(crate) fn over(
        server: DriveServer,
        tokens: StaticToken,
    ) -> (EmulatedDrive, MockTransport, Arc<Mutex<DriveServer>>) {
        let state = Arc::new(Mutex::new(server));
        let shared = Arc::clone(&state);
        let transport = MockTransport::new(move |req| shared.lock().unwrap().handle(req));
        let store = GDriveStore::new(transport.clone(), tokens, SUB.to_owned()).unwrap();
        (store, transport, state)
    }

    /// A store over an empty server that accepts [`TOKEN`].
    pub(crate) fn emulated() -> (EmulatedDrive, MockTransport, Arc<Mutex<DriveServer>>) {
        over(DriveServer::new(), StaticToken::new(TOKEN))
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::tests_support::*;
    use super::*;
    use crate::sync::provider::transport::{MockTransport, TransportFailure};
    use crate::sync::provider::{binding_fp, Bound, StaticToken};
    use crate::sync::publish::{settle_attempted, Outcome, Pause, SegmentRemote, Verdict};

    const VAULT: &str = "vault-a";
    const SEG: &str = "v1/vault-a/log/dev-a/000000000001-000000000003.seg";
    const REC: &str = "v1/vault-a/devices/dev-a.rec";
    const BLOB: &str = "v1/vault-a/blob/ab/ab12cd.blob";
    const SNAP: &str = "v1/vault-a/snap/dev-a/snap-1.snap";
    const MANIFEST: &str = "v1/vault-a/manifest.json";
    const OURS: &[u8] = b"sealed segment bytes";
    const THEIRS: &[u8] = b"someone else's segment";

    fn props(vault_id: Option<&'static str>, kind: Option<RemoteKind>, device_id: Option<&'static str>) -> ListQuery<'static> {
        ListQuery::Props { vault_id, kind, device_id }
    }

    fn keys(listed: Vec<ListedObject>) -> Vec<String> {
        let mut keys: Vec<String> = listed.into_iter().map(|o| o.key).collect();
        keys.sort();
        keys
    }

    fn uploads(transport: &MockTransport) -> Vec<HttpRequest> {
        transport.requests().into_iter().filter(|r| r.url.starts_with(UPLOAD_BASE) && r.method == "POST").collect()
    }

    fn list_requests(transport: &MockTransport) -> Vec<url::Url> {
        transport
            .requests()
            .into_iter()
            .filter(|r| r.method == "GET")
            .map(|r| url::Url::parse(&r.url).unwrap())
            .filter(|u| u.path() == "/drive/v3/files")
            .collect()
    }

    fn query_param(url: &url::Url, name: &str) -> Option<String> {
        url.query_pairs().find(|(k, _)| k == name).map(|(_, v)| v.into_owned())
    }

    fn permanent(err: &ProviderError) -> bool {
        matches!(err, ProviderError::Permanent(_))
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

    /// AC 31: no folder is ever created, every object is a flat file in
    /// `appDataFolder` named by its full key, with the key's properties.
    #[tokio::test]
    async fn creates_are_flat_app_data_files_with_layout_properties() {
        let (store, transport, server) = emulated();
        store.ensure_container().await.unwrap();
        assert_eq!(transport.request_count(), 0, "ensure_container sends nothing");
        let cases: [(&str, &str, &str); 5] = [
            (SEG, "seg", "dev-a"),
            (REC, "rec", "dev-a"),
            (BLOB, "blob", ""),
            (SNAP, "snap", "dev-a"),
            (MANIFEST, "manifest", ""),
        ];
        for (key, _, _) in cases {
            store.put_create(key, key.as_bytes()).await.unwrap();
        }
        let uploads = uploads(&transport);
        assert_eq!(uploads.len(), cases.len());
        for (req, (key, kind, device_id)) in uploads.iter().zip(cases) {
            assert!(req.url.contains("uploadType=multipart"), "{}", req.url);
            let (meta, media) = parse_multipart(req).expect("multipart body");
            assert_eq!(media, key.as_bytes());
            assert_eq!(meta["name"], key, "name is the full key");
            assert_eq!(meta["parents"], serde_json::json!(["appDataFolder"]));
            assert_eq!(meta["appProperties"], serde_json::json!({"vault_id": VAULT, "kind": kind, "device_id": device_id}));
            assert!(meta.get("mimeType").is_none());
        }
        for req in transport.requests() {
            assert!(!contains(&req.body, FOLDER_MIME.as_bytes()), "folder create: {req:?}");
            assert!(!(req.method == "POST" && req.url.starts_with(&format!("{API_BASE}/files"))), "metadata create: {req:?}");
        }
        let server = server.lock().unwrap();
        assert_eq!(server.folder_creates, 0);
        assert!(server.files.iter().all(|f| f.parents == [APP_DATA] && f.mime_type.is_none()));
        assert_eq!(server.files.len(), cases.len());
    }

    /// AC 31: vault discovery and stream listings query only appProperties
    /// in `appDataFolder`; no request filters by name.
    #[tokio::test]
    async fn listings_use_only_app_properties_queries() {
        let (store, transport, server) = emulated();
        {
            let mut server = server.lock().unwrap();
            server.seed_key(MANIFEST, b"m");
            server.seed_key(SEG, OURS);
            server.seed_key("v1/vault-a/log/dev-b/000000000001-000000000001.seg", b"b");
        }
        let discovered = store.list(props(None, Some(RemoteKind::Manifest), None)).await.unwrap();
        assert_eq!(keys(discovered), [MANIFEST]);
        let stream = store.list(props(Some(VAULT), Some(RemoteKind::Seg), Some("dev-a"))).await.unwrap();
        assert_eq!(keys(stream), [SEG]);
        assert_eq!(store.get(SEG).await.unwrap(), OURS);

        let lists = list_requests(&transport);
        assert_eq!(lists.len(), 3);
        for url in &lists {
            assert_eq!(query_param(url, "spaces").as_deref(), Some("appDataFolder"));
            let q = query_param(url, "q").unwrap();
            assert!(parse_q(&q).is_some(), "only appProperties clauses: {q}");
            assert!(!q.contains("name"), "no name query: {q}");
            assert!(q.ends_with("trashed=false"), "{q}");
            assert_eq!(query_param(url, "fields").as_deref(), Some(LIST_FIELDS));
            assert_eq!(query_param(url, "pageSize").as_deref(), Some("1000"));
        }
        assert_eq!(
            query_param(&lists[0], "q").unwrap(),
            "appProperties has { key='kind' and value='manifest' } and trashed=false"
        );
        let stream_q = "appProperties has { key='vault_id' and value='vault-a' } and appProperties has { key='kind' and value='seg' } \
             and appProperties has { key='device_id' and value='dev-a' } and trashed=false";
        assert_eq!(query_param(&lists[1], "q").unwrap(), stream_q);
        assert_eq!(query_param(&lists[2], "q").unwrap(), stream_q, "get_all lists the key's own properties");
        let media = transport.requests().into_iter().filter(|r| r.url.contains("alt=media")).count();
        assert_eq!(media, 1);
    }

    /// AC 31: two devices setting up different vaults at the same time are
    /// both discovered: no shared folder, so neither create can hide the other.
    #[tokio::test]
    async fn concurrent_first_setup_discovers_both_vaults() {
        let (device_a, transport, server) = emulated();
        let device_b = GDriveStore::new(transport.clone(), StaticToken::new(TOKEN), SUB.to_owned()).unwrap();
        let (a, b) = tokio::join!(
            device_a.put_create("v1/vault-a/manifest.json", b"manifest a"),
            device_b.put_create("v1/vault-b/manifest.json", b"manifest b"),
        );
        a.unwrap();
        b.unwrap();
        for store in [&device_a, &device_b] {
            let found = store.list(props(None, Some(RemoteKind::Manifest), None)).await.unwrap();
            assert_eq!(keys(found), ["v1/vault-a/manifest.json", "v1/vault-b/manifest.json"]);
        }
        assert_eq!(server.lock().unwrap().folder_creates, 0);
    }

    /// Duplicate names: every copy reaches `get_all`, the bridge, and the
    /// resolver, which reports the fork.
    #[tokio::test]
    async fn duplicate_names_reach_the_bridge_and_settle_as_a_fork() {
        let (store, _transport, server) = emulated();
        let (ours_id, theirs_id) = {
            let mut server = server.lock().unwrap();
            (server.seed_key(SEG, OURS), server.seed_key(SEG, THEIRS))
        };
        let all = store.get_all(SEG).await.unwrap();
        assert_eq!(
            all,
            vec![
                RemoteObject { provider_ref: ours_id.clone(), bytes: OURS.to_vec() },
                RemoteObject { provider_ref: theirs_id.clone(), bytes: THEIRS.to_vec() },
            ]
        );
        assert_eq!(store.get(SEG).await, Err(ProviderError::Permanent("duplicate name".to_owned())));
        assert_eq!(store.put_create(SEG, OURS).await, Err(ProviderError::AlreadyExists));

        let remote = Bound::new(store, binding_fp(&format!("gdrive|{SUB}")));
        assert!(!SegmentRemote::create_only(&remote));
        let bridged = SegmentRemote::get_all(&remote, SEG).await.unwrap();
        assert_eq!(bridged.len(), 2);
        assert_eq!(bridged.iter().map(|o| o.provider_ref.as_str()).collect::<Vec<_>>(), [&ours_id, &theirs_id]);
        let settled = settle_attempted(&remote, &NoPause, SEG, OURS, judge).await;
        assert_eq!(settled.outcome, Outcome::PublishedWithFork);
        assert_eq!(server.lock().unwrap().copies(SEG), [OURS.to_vec(), THEIRS.to_vec()], "nothing is removed");
    }

    /// Amendment B1 (architect regression): only another writer's copy holds
    /// our key and our first upload was lost. The resend uploads ours next to
    /// it with no pre-check, and settle reports the fork.
    #[tokio::test]
    async fn settle_resends_next_to_another_writers_copy_and_reports_the_fork() {
        let (store, transport, server) = emulated();
        server.lock().unwrap().seed_key(SEG, THEIRS);
        let remote = Bound::new(store, binding_fp(&format!("gdrive|{SUB}")));
        let settled = settle_attempted(&remote, &NoPause, SEG, OURS, judge).await;
        assert_eq!(settled.outcome, Outcome::PublishedWithFork);
        let multipart = transport
            .requests()
            .iter()
            .filter(|r| r.method == "POST" && r.url.starts_with(&format!("{UPLOAD_BASE}/files?uploadType=multipart")))
            .count();
        assert_eq!(multipart, 1, "exactly one upload");
        assert_eq!(server.lock().unwrap().copies(SEG), [THEIRS.to_vec(), OURS.to_vec()], "nothing is removed");
    }

    /// The unchecked create uploads with no listing even over an existing name;
    /// it still refuses an unsafe key or an oversize body with no request, and
    /// through `Bound` a mismatched binding sends nothing.
    #[tokio::test]
    async fn put_create_unchecked_uploads_without_a_precheck() {
        let (store, transport, server) = emulated();
        server.lock().unwrap().seed_key(SEG, THEIRS);
        let created = store.put_create_unchecked(SEG, OURS).await.unwrap();
        assert_eq!(created.key, SEG);
        let methods: Vec<String> = transport.requests().iter().map(|r| r.method.clone()).collect();
        assert_eq!(methods, ["POST"]);
        assert_eq!(server.lock().unwrap().copies(SEG), [THEIRS.to_vec(), OURS.to_vec()]);

        assert!(matches!(store.put_create_unchecked("../x", OURS).await, Err(ProviderError::Permanent(_))));
        let oversize = vec![0u8; crate::sync::provider::MAX_OBJECT_BYTES];
        assert!(matches!(store.put_create_unchecked(SEG, &oversize).await, Err(ProviderError::Permanent(_))));
        assert_eq!(transport.request_count(), 1);

        let wrong = Bound::new(store, binding_fp("gdrive|someone-else"));
        assert_eq!(wrong.put_create_unchecked(SEG, OURS).await, Err(ProviderError::BindingMismatch));
        assert_eq!(SegmentRemote::put_create(&wrong, SEG, OURS).await, Err(crate::sync::publish::TransportError));
        assert_eq!(transport.request_count(), 1);
    }

    /// A self-duplicate (identical bytes twice) publishes once and names the
    /// extra copy, which `delete_ref` removes by file id.
    #[tokio::test]
    async fn self_duplicates_are_one_object_and_removed_by_ref() {
        let (store, _transport, server) = emulated();
        let (first, second) = {
            let mut server = server.lock().unwrap();
            (server.seed_key(SEG, OURS), server.seed_key(SEG, OURS))
        };
        assert_eq!(store.get(SEG).await.unwrap(), OURS, "identical copies are one object");
        let remote = Bound::new(store, binding_fp(&format!("gdrive|{SUB}")));
        let settled = settle_attempted(&remote, &NoPause, SEG, OURS, judge).await;
        assert_eq!(settled.outcome, Outcome::Published);
        let mut ids = [first, second];
        ids.sort();
        assert_eq!(settled.extra_self_copies, [ids[1].clone()]);
        remote.delete_ref(&settled.extra_self_copies[0]).await.unwrap();
        remote.delete_ref(&settled.extra_self_copies[0]).await.unwrap();
        let left: Vec<String> = server.lock().unwrap().files.iter().map(|f| f.id.clone()).collect();
        assert_eq!(left, [ids[0].clone()]);
    }

    /// A 401 invalidates the token and retries once; the retry succeeds.
    #[tokio::test]
    async fn unauthorized_refreshes_once_and_retries() {
        let mut server = DriveServer::new();
        server.accepted_tokens = vec!["fresh-token".to_owned()];
        server.seed_key(SEG, OURS);
        let (store, transport, _) = over(server, StaticToken::sequence(&["expired-token", "fresh-token"]));
        assert_eq!(store.get(SEG).await.unwrap(), OURS);
        assert_eq!(store.tokens.invalidations(), 1);
        let auth: Vec<String> =
            transport.requests().iter().map(|r| r.header("authorization").unwrap().to_owned()).collect();
        assert_eq!(auth, ["Bearer expired-token", "Bearer fresh-token", "Bearer fresh-token"]);
    }

    /// Two 401s in a row mean the grant is gone: reconsent, no third try.
    #[tokio::test]
    async fn second_unauthorized_requires_reconsent() {
        let mut server = DriveServer::new();
        server.accepted_tokens = vec!["never-issued".to_owned()];
        let (store, transport, _) = over(server, StaticToken::sequence(&["revoked-1", "revoked-2"]));
        assert_eq!(store.get(SEG).await, Err(ProviderError::Auth { reconsent: true }));
        assert_eq!(transport.request_count(), 2);
        assert_eq!(store.tokens.invalidations(), 1);
        assert_eq!(store.put_create(SEG, OURS).await, Err(ProviderError::Auth { reconsent: true }));
        assert!(uploads(&transport).is_empty(), "nothing uploads without a valid token");
    }

    #[tokio::test]
    async fn listings_follow_every_page() {
        let (store, transport, server) = emulated();
        let mut expected = Vec::new();
        {
            let mut server = server.lock().unwrap();
            server.page_size = 2;
            for n in 1..=5u64 {
                let key = format!("v1/vault-a/log/dev-a/{n:012}-{n:012}.seg");
                server.seed_key(&key, b"s");
                expected.push(key);
            }
        }
        let listed = store.list(props(Some(VAULT), Some(RemoteKind::Seg), Some("dev-a"))).await.unwrap();
        assert_eq!(keys(listed), expected);
        let lists = list_requests(&transport);
        assert_eq!(lists.len(), 3);
        assert_eq!(query_param(&lists[0], "pageToken"), None);
        assert_eq!(query_param(&lists[1], "pageToken").as_deref(), Some("p2"));
        assert_eq!(query_param(&lists[2], "pageToken").as_deref(), Some("p4"));
    }

    #[tokio::test]
    async fn listing_that_does_not_progress_is_refused() {
        let page = serde_json::json!({"files": [], "nextPageToken": "same"});
        let transport = MockTransport::new(move |_| HttpResponse {
            status: 200,
            headers: Vec::new(),
            body: serde_json::to_vec(&page).unwrap(),
        });
        let store = GDriveStore::new(transport.clone(), StaticToken::new(TOKEN), SUB.to_owned()).unwrap();
        assert!(permanent(&store.list(props(Some(VAULT), None, None)).await.unwrap_err()));
        assert_eq!(transport.request_count(), 2);
    }

    #[tokio::test]
    async fn a_200_that_is_not_a_file_list_is_never_empty() {
        for body in [&b""[..], b"{}", b"<html>login</html>", br#"{"kind":"drive#fileList"}"#] {
            let (store, transport, _) = emulated();
            transport.script_next(Ok(HttpResponse { status: 200, headers: Vec::new(), body: body.to_vec() }));
            let err = store.list(props(Some(VAULT), None, None)).await.unwrap_err();
            assert!(matches!(err, ProviderError::Transient(_)), "{body:?} → {err:?}");
            transport.script_next(Ok(HttpResponse { status: 200, headers: Vec::new(), body: body.to_vec() }));
            assert!(matches!(store.get(SEG).await, Err(ProviderError::Transient(_))), "never NotFound");
        }
    }

    /// Files whose properties match but whose name is not the layout key the
    /// query asks for are skipped, never returned or downloaded.
    #[tokio::test]
    async fn malicious_and_foreign_names_are_skipped() {
        let (store, transport, server) = emulated();
        let seg_props = [("vault_id", VAULT), ("kind", "seg"), ("device_id", "dev-a")];
        {
            let mut server = server.lock().unwrap();
            for name in [
                "../../etc/passwd",
                "/v1/vault-a/log/dev-a/000000000001-000000000003.seg",
                "v1/vault-a/log/dev-a/../dev-b/000000000001-000000000003.seg",
                "v1/vault-a/log/dev-a/000000000001-000000000003.seg/",
                "v1/vault-a/log/dev-a/1-3.seg",
                "v1/vault-b/log/dev-a/000000000001-000000000003.seg",
                "v1/vault-a/log/dev-b/000000000001-000000000003.seg",
                "v1/vault-a/snap/dev-a/000000000001-000000000003.seg",
                "v1/vault-a/log/dev-a/000000000001-000000000003.seg\u{0}",
                "v1/vault-a/log/dév/000000000001-000000000003.seg",
                "",
            ] {
                server.seed(name, &seg_props, b"evil");
            }
            // A valid name whose properties contradict it (another device's
            // stream filed under ours) is not returned for either.
            server.seed(
                "v1/vault-a/log/dev-b/000000000001-000000000003.seg",
                &[("vault_id", VAULT), ("kind", "seg"), ("device_id", "dev-a")],
                b"evil",
            );
            server.seed_key(SEG, OURS);
        }
        let listed = store.list(props(Some(VAULT), Some(RemoteKind::Seg), Some("dev-a"))).await.unwrap();
        assert_eq!(keys(listed), [SEG]);
        assert_eq!(store.get_all(SEG).await.unwrap().len(), 1);
        assert_eq!(store.get(SEG).await.unwrap(), OURS);
        let all = store.list(props(None, None, None)).await.unwrap();
        assert_eq!(keys(all), [SEG]);
        let media = transport.requests().into_iter().filter(|r| r.url.contains("alt=media")).count();
        assert_eq!(media, 2, "only the real copy is ever downloaded");
    }

    #[tokio::test]
    async fn query_values_are_escaped_and_hostile_props_send_nothing() {
        assert_eq!(escape_query_value(r"a'b\c"), r"a\'b\\c");
        // Defence in depth: even a value that slipped past the id check stays
        // one literal inside its clause.
        let hostile = r"x' } or name contains 'v1\";
        let q = props_query(Some(hostile), Some(RemoteKind::Seg), None);
        let (clauses, trashed_false) = parse_q(&q).expect("escaped value stays one literal");
        assert_eq!(clauses, [("vault_id".to_owned(), hostile.to_owned()), ("kind".to_owned(), "seg".to_owned())]);
        assert!(trashed_false);

        let (store, transport, server) = emulated();
        server.lock().unwrap().seed_key(SEG, OURS);
        for (vault_id, device_id) in [(Some(hostile), None), (Some(""), None), (Some(VAULT), Some("")), (None, Some("a b"))] {
            let err = store.list(ListQuery::Props { vault_id, kind: None, device_id }).await.unwrap_err();
            assert!(permanent(&err), "{vault_id:?} {device_id:?}");
        }
        assert_eq!(transport.request_count(), 0);
    }

    #[tokio::test]
    async fn prefix_listing_and_conditional_replace_are_unsupported() {
        let (store, transport, _) = emulated();
        assert!(matches!(store.list(ListQuery::Prefix("v1/")).await, Err(ProviderError::Unsupported(_))));
        assert!(matches!(store.list(ListQuery::Prefix("")).await, Err(ProviderError::Unsupported(_))));
        assert!(matches!(store.put_replace(REC, b"r", Some("\"e\"")).await, Err(ProviderError::Unsupported(_))));
        assert_eq!(transport.request_count(), 0);
        assert!(!store.create_only());
        assert_eq!(store.identity(), format!("gdrive|{SUB}"));
    }

    #[tokio::test]
    async fn put_create_prechecks_and_never_uploads_over_an_existing_name() {
        let (store, transport, server) = emulated();
        server.lock().unwrap().seed_key(SEG, THEIRS);
        assert_eq!(store.put_create(SEG, OURS).await, Err(ProviderError::AlreadyExists));
        assert!(uploads(&transport).is_empty());
        assert_eq!(server.lock().unwrap().copies(SEG), [THEIRS.to_vec()]);

        let created = store.put_create(REC, b"r").await.unwrap();
        assert_eq!(created.key, REC);
        assert_eq!(created.size, Some(1));
        let ids: Vec<String> = server.lock().unwrap().files.iter().map(|f| f.id.clone()).collect();
        assert_eq!(created.provider_ref, ids[1], "provider_ref is the new file id");
        // Pre-check list, then upload; no re-list after it.
        let methods: Vec<String> = transport.requests()[1..].iter().map(|r| r.method.clone()).collect();
        assert_eq!(methods, ["GET", "POST"]);
    }

    /// A racing duplicate that appears between the pre-check and the upload
    /// still yields Created; both copies stay for the resolver.
    #[tokio::test]
    async fn racing_duplicate_still_reports_created() {
        let (store, transport, server) = emulated();
        // The pre-check sees nothing; the other device's copy lands before ours.
        transport.script_next(Ok(HttpResponse {
            status: 200,
            headers: Vec::new(),
            body: br#"{"files":[]}"#.to_vec(),
        }));
        server.lock().unwrap().seed_key(SEG, THEIRS);
        let created = store.put_create(SEG, OURS).await.unwrap();
        assert_eq!(created.key, SEG);
        let all = store.get_all(SEG).await.unwrap();
        assert_eq!(all.len(), 2);
        assert!(all.iter().any(|o| o.provider_ref == created.provider_ref && o.bytes == OURS));
    }

    #[tokio::test]
    async fn put_replace_patches_the_single_copy() {
        let (store, transport, server) = emulated();
        let first = store.put_replace(REC, b"rec-1", None).await.unwrap();
        assert_eq!(uploads(&transport).len(), 1, "a missing record is created");
        let second = store.put_replace(REC, b"rec-2", None).await.unwrap();
        assert_eq!(second.provider_ref, first.provider_ref);
        let patch = transport.requests().into_iter().find(|r| r.method == "PATCH").unwrap();
        assert_eq!(patch.url, format!("{UPLOAD_BASE}/files/{}?uploadType=media&fields=id", first.provider_ref));
        assert_eq!(patch.body, b"rec-2");
        assert_eq!(server.lock().unwrap().copies(REC), [b"rec-2".to_vec()]);

        server.lock().unwrap().seed_key(REC, b"rec-other");
        assert_eq!(store.put_replace(REC, b"rec-3", None).await, Err(ProviderError::Permanent("duplicate name".to_owned())));
        assert_eq!(server.lock().unwrap().copies(REC), [b"rec-2".to_vec(), b"rec-other".to_vec()]);
    }

    /// D1a: a ref-targeted media update PATCHes exactly the observed file;
    /// once that file is gone it is NotFound, with no lookup and no upload.
    #[tokio::test]
    async fn d1a_put_replace_ref_patches_the_observed_file_and_never_creates() {
        let (store, transport, server) = emulated();
        let first = store.put_replace(REC, b"rec-1", None).await.unwrap();
        let before = transport.request_count();
        let updated = store.put_replace_ref(REC, &first.provider_ref, b"rec-2").await.unwrap();
        assert_eq!(updated.provider_ref, first.provider_ref);
        let sent: Vec<HttpRequest> = transport.requests().into_iter().skip(before).collect();
        assert_eq!(sent.len(), 1, "one PATCH, no listing: {sent:?}");
        assert_eq!(sent[0].method, "PATCH");
        assert_eq!(sent[0].url, format!("{UPLOAD_BASE}/files/{}?uploadType=media&fields=id", first.provider_ref));
        assert_eq!(server.lock().unwrap().copies(REC), [b"rec-2".to_vec()]);

        store.delete_ref(&first.provider_ref).await.unwrap();
        let uploads_before = uploads(&transport).len();
        assert_eq!(store.put_replace_ref(REC, &first.provider_ref, b"rec-3").await, Err(ProviderError::NotFound));
        assert_eq!(uploads(&transport).len(), uploads_before, "never an upload");
        assert!(server.lock().unwrap().copies(REC).is_empty(), "a removed record is not recreated");
        assert!(matches!(store.put_replace_ref(REC, "../x", b"r").await, Err(ProviderError::Permanent(_))));
    }

    #[tokio::test]
    async fn delete_removes_every_copy() {
        let (store, transport, server) = emulated();
        {
            let mut server = server.lock().unwrap();
            server.seed_key(SEG, OURS);
            server.seed_key(SEG, THEIRS);
            server.seed_key(BLOB, b"keep");
        }
        store.delete(SEG).await.unwrap();
        assert!(server.lock().unwrap().copies(SEG).is_empty());
        assert_eq!(server.lock().unwrap().copies(BLOB), [b"keep".to_vec()]);
        store.delete(SEG).await.unwrap();
        store.delete_ref("1drv_file-9999").await.unwrap();
        let before = transport.request_count();
        for bad in ["", "a/b", "id?alt=media", "../x", "a b"] {
            assert!(permanent(&store.delete_ref(bad).await.unwrap_err()), "{bad:?}");
        }
        assert_eq!(transport.request_count(), before, "a malformed id sends nothing");
    }

    #[tokio::test]
    async fn quota_reads_storage_quota() {
        let (store, transport, server) = emulated();
        server.lock().unwrap().seed_key(SEG, &[0u8; 100]);
        assert_eq!(store.quota().await.unwrap(), Quota { used: Some(100), available: Some(16_106_127_260) });
        let last = transport.requests().pop().unwrap();
        assert_eq!(last.url, format!("{API_BASE}/about?fields=storageQuota"));
        // Unlimited storage has no limit: nothing is available to report.
        transport.script_next(Ok(HttpResponse {
            status: 200,
            headers: Vec::new(),
            body: br#"{"storageQuota":{"usage":"7"}}"#.to_vec(),
        }));
        assert_eq!(store.quota().await.unwrap(), Quota { used: Some(7), available: None });
    }

    /// Drive's 403 carries a reason: rate limits and a full Drive are not
    /// permission problems.
    #[tokio::test]
    async fn forbidden_reasons_map_to_rate_limit_and_quota() {
        let (store, transport, _) = emulated();
        let cases: [(&str, ProviderError); 5] = [
            ("rateLimitExceeded", ProviderError::RateLimited { retry_after: Some(Duration::from_secs(3)) }),
            ("userRateLimitExceeded", ProviderError::RateLimited { retry_after: Some(Duration::from_secs(3)) }),
            ("dailyLimitExceeded", ProviderError::RateLimited { retry_after: Some(Duration::from_secs(3)) }),
            ("storageQuotaExceeded", ProviderError::QuotaExceeded),
            ("insufficientFilePermissions", ProviderError::Auth { reconsent: false }),
        ];
        for (reason, expected) in cases {
            let mut resp = error(403, reason);
            resp.headers.push(("Retry-After".to_owned(), "3".to_owned()));
            transport.script_next(Ok(resp));
            assert_eq!(store.list(props(Some(VAULT), None, None)).await, Err(expected), "{reason}");
        }
        transport.script_next(Ok(error(403, "storageQuotaExceeded")));
        assert_eq!(store.put_create(SEG, OURS).await, Err(ProviderError::QuotaExceeded));
    }

    /// The bearer token never appears in any error, whatever fails.
    #[tokio::test]
    async fn token_never_appears_in_errors() {
        let (store, transport, _) = emulated();
        let mut seen = Vec::new();
        let failures = [
            Err(TransportFailure::Timeout),
            Err(TransportFailure::Connect("connection refused".to_owned())),
            Err(TransportFailure::Tls("bad certificate".to_owned())),
            Ok(error(400, "badRequest")),
            Ok(error(500, "backendError")),
            Ok(HttpResponse { status: 200, headers: Vec::new(), body: TOKEN.as_bytes().to_vec() }),
        ];
        for failure in failures {
            transport.script_next(failure.clone());
            seen.push(store.get(SEG).await.unwrap_err());
            transport.script_next(failure.clone());
            seen.push(store.list(props(Some(VAULT), None, None)).await.unwrap_err());
            transport.script_next(failure);
            seen.push(store.quota().await.unwrap_err());
        }
        transport.fail_next(401, &[]);
        transport.fail_next(401, &[]);
        seen.push(store.get(SEG).await.unwrap_err());
        for err in &seen {
            assert!(!format!("{err:?}").contains(TOKEN), "{err:?}");
            assert!(!format!("{err:?}").contains("ya29"), "{err:?}");
        }
        // Recorded requests print with the token redacted too.
        for req in transport.requests() {
            assert_eq!(req.header("authorization"), Some(format!("Bearer {TOKEN}").as_str()));
            assert!(!format!("{req:?}").contains(TOKEN));
        }
    }

    #[tokio::test]
    async fn unsafe_and_non_layout_keys_send_nothing() {
        let (store, transport, _) = emulated();
        for key in [
            "",
            "/v1/a.seg",
            "v1/../a.seg",
            "v1//a.seg",
            "v1/a b",
            "v1/vault-a/log/dev-a/missing.seg",
            "v1/vault-a/other/x.bin",
            "v2/vault-a/manifest.json",
        ] {
            assert!(permanent(&store.get(key).await.unwrap_err()), "get {key:?}");
            assert!(permanent(&store.get_all(key).await.unwrap_err()), "get_all {key:?}");
            assert!(permanent(&store.put_create(key, b"x").await.unwrap_err()), "put_create {key:?}");
            assert!(permanent(&store.put_replace(key, b"x", None).await.unwrap_err()), "put_replace {key:?}");
            assert!(permanent(&store.delete(key).await.unwrap_err()), "delete {key:?}");
        }
        let big = vec![0u8; crate::sync::provider::MAX_OBJECT_BYTES];
        assert!(permanent(&store.put_create(SEG, &big).await.unwrap_err()));
        assert!(permanent(&store.put_replace(REC, &big, None).await.unwrap_err()));
        assert_eq!(transport.request_count(), 0);
    }

    #[test]
    fn multipart_boundary_never_occurs_in_the_parts() {
        let first = "sayknow-part-0000000000000000";
        let media = format!("{first} and sayknow-part-0000000000000001");
        let (content_type, body) = multipart(br#"{"name":"k"}"#, media.as_bytes());
        assert_eq!(content_type, "multipart/related; boundary=sayknow-part-0000000000000002");
        let req = HttpRequest {
            method: "POST".to_owned(),
            url: String::new(),
            headers: vec![("Content-Type".to_owned(), content_type)],
            body,
        };
        let (meta, parsed) = parse_multipart(&req).unwrap();
        assert_eq!(meta["name"], "k");
        assert_eq!(parsed, media.as_bytes());
    }

    #[tokio::test]
    async fn binary_media_round_trips_through_multipart() {
        let (store, _transport, _) = emulated();
        let binary: Vec<u8> = (0..=255).chain([b'\r', b'\n', b'-', b'-']).collect();
        store.put_create(BLOB, &binary).await.unwrap();
        assert_eq!(store.get(BLOB).await.unwrap(), binary);
    }

    #[tokio::test]
    async fn bound_mismatch_sends_nothing() {
        let (store, transport, server) = emulated();
        server.lock().unwrap().seed_key(SEG, OURS);
        let foreign = Bound::new(store, binding_fp("gdrive|someone-else"));
        assert_eq!(ObjectStore::get_all(&foreign, SEG).await, Err(ProviderError::BindingMismatch));
        assert_eq!(foreign.delete_ref("1drv_file-0001").await, Err(ProviderError::BindingMismatch));
        assert_eq!(foreign.list(props(None, None, None)).await, Err(ProviderError::BindingMismatch));
        assert_eq!(transport.request_count(), 0);
    }

    #[test]
    fn construction_rejects_a_malformed_account_id() {
        for sub in ["", "a|b", "a b", "a\nb"] {
            assert!(GDriveStore::new(MockTransport::new(|_| unreachable!()), StaticToken::new(TOKEN), sub.to_owned()).is_err());
        }
    }

    /// The shared contract (provider/mod.rs) adapted to Drive: `create_only`
    /// is false (the pre-check refuses a known name), listings use Props
    /// (Prefix is Unsupported), and `put_replace` has no etag precondition.
    async fn drive_contract_suite(p: &impl ObjectStore, requests: impl Fn() -> usize, fail_next: impl Fn(u16, &[(&str, &str)])) {
        // 1. ensure_container is idempotent and sends nothing.
        let before = requests();
        p.ensure_container().await.unwrap();
        p.ensure_container().await.unwrap();
        assert_eq!(requests(), before);

        // 2. put_create creates once and never overwrites (AC 18).
        let created = p.put_create(SEG, OURS).await.unwrap();
        assert_eq!(created.key, SEG);
        assert_eq!(p.put_create(SEG, THEIRS).await, Err(ProviderError::AlreadyExists));
        assert_eq!(p.get(SEG).await.unwrap(), OURS, "a refused create must leave the bytes unchanged");

        // 3. get returns the exact bytes; a missing key is NotFound.
        let binary: Vec<u8> = (0..=255).collect();
        p.put_create(BLOB, &binary).await.unwrap();
        assert_eq!(p.get(BLOB).await.unwrap(), binary);
        assert_eq!(p.get("v1/vault-a/log/dev-a/000000000009-000000000009.seg").await, Err(ProviderError::NotFound));

        // 4. list(Props) returns exactly the matching keys.
        let nested = "v1/vault-a/log/dev-b/000000000001-000000000001.seg";
        let other_vault = "v1/vault-b/log/dev-a/000000000001-000000000001.seg";
        p.put_create(nested, b"n").await.unwrap();
        p.put_create(other_vault, b"o").await.unwrap();
        let log = p.list(props(Some(VAULT), Some(RemoteKind::Seg), None)).await.unwrap();
        for object in &log {
            if object.key == SEG {
                assert_eq!(object.size, Some(OURS.len() as u64));
            }
        }
        assert_eq!(keys(log), [SEG, nested]);
        assert_eq!(keys(p.list(props(Some(VAULT), None, None)).await.unwrap()), [BLOB, SEG, nested]);
        assert_eq!(keys(p.list(props(None, None, None)).await.unwrap()), [BLOB, SEG, nested, other_vault]);
        assert_eq!(keys(p.list(props(Some(VAULT), Some(RemoteKind::Blob), None)).await.unwrap()), [BLOB]);
        assert!(p.list(props(Some(VAULT), Some(RemoteKind::Blob), Some("dev-a"))).await.unwrap().is_empty());
        assert!(p.list(props(Some("vault-c"), None, None)).await.unwrap().is_empty());
        assert!(matches!(p.list(ListQuery::Prefix("v1/")).await, Err(ProviderError::Unsupported(_))));

        // 5. put_replace: no etag precondition; unconditional replace works.
        p.put_replace(REC, b"rec-1", None).await.unwrap();
        assert!(matches!(p.put_replace(REC, b"rec-x", Some("\"stale\"")).await, Err(ProviderError::Unsupported(_))));
        assert_eq!(p.get(REC).await.unwrap(), b"rec-1");
        p.put_replace(REC, b"rec-2", None).await.unwrap();
        assert_eq!(p.get(REC).await.unwrap(), b"rec-2");

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
        assert_eq!(requests(), before, "an unsafe key must send no request");

        // 8. Fault mapping (a 401 is retried once, so it is scripted twice).
        type Case<'a> = (u16, &'a [(&'a str, &'a str)], fn(&ProviderError) -> bool);
        let cases: [Case; 7] = [
            (401, &[], |e| *e == ProviderError::Auth { reconsent: true }),
            (403, &[], |e| *e == ProviderError::Auth { reconsent: false }),
            (429, &[("Retry-After", "12")], |e| *e == ProviderError::RateLimited { retry_after: Some(Duration::from_secs(12)) }),
            (503, &[], |e| *e == ProviderError::RateLimited { retry_after: None }),
            (507, &[], |e| *e == ProviderError::QuotaExceeded),
            (500, &[], |e| matches!(e, ProviderError::Transient(_))),
            (400, &[], permanent),
        ];
        for (status, hdrs, expected) in cases {
            fail_next(status, hdrs);
            if status == 401 {
                fail_next(status, hdrs);
            }
            let err = p.get(SEG).await.unwrap_err();
            assert!(expected(&err), "HTTP {status} mapped to {err:?}");
        }
        assert_eq!(p.get(SEG).await.unwrap(), OURS, "faults must not disturb stored objects");
    }

    #[tokio::test]
    async fn contract_suite_drive() {
        let (store, transport, server) = emulated();
        drive_contract_suite(&store, || transport.request_count(), |s, h| transport.fail_next(s, h)).await;
        assert_eq!(server.lock().unwrap().folder_creates, 0);
    }

    #[tokio::test]
    async fn contract_suite_drive_through_bound() {
        let (store, transport, _) = emulated();
        let bound = Bound::new(store, binding_fp(&format!("gdrive|{SUB}")));
        drive_contract_suite(&bound, || transport.request_count(), |s, h| transport.fail_next(s, h)).await;
    }
}