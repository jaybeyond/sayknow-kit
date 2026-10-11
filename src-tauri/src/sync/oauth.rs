//! Native OAuth for Google Drive and OneDrive (§4.8).
//!
//! Rust runs the whole flow: PKCE (S256) and `state` are made here, the code
//! arrives on the native loopback listener (`oauth_callback`), and the token
//! exchange happens over an [`HttpTransport`]. The refresh token lives only in
//! the keychain under `SyncSecret::Credential { provider, binding_fp }`; the
//! access token lives only in memory. Neither reaches the webview, a log, or
//! an error: every type holding one prints `<redacted>`, and token endpoint
//! bodies are never copied into an error string.

use std::fmt;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use data_encoding::BASE64URL_NOPAD;
use serde_json::Value;
use sha2::{Digest, Sha256};
use url::form_urlencoded;
use zeroize::Zeroizing;

use super::crypto::fill_random;
use super::provider::transport::{HttpRequest, HttpTransport};
use super::provider::{binding_fp, map_status, AccessToken, ProviderError};
use super::secrets::{read_secret, write_secret, CloudProvider, SecretError, SecretStore, SyncSecret};

/// The callback path both providers redirect to.
pub(crate) const CALLBACK_PATH: &str = "/callback";
/// A cached access token is refreshed this long before it expires.
const EARLY_REFRESH: Duration = Duration::from_secs(60);
/// `expires_in` above this is clamped: a bogus huge value must not pin a token.
const MAX_TOKEN_LIFETIME: Duration = Duration::from_secs(24 * 60 * 60);

/// App registration for one provider, from configuration; nothing is
/// committed. Whether Drive needs the secret is G-EXT-1, so it is optional.
#[derive(Clone, PartialEq, Eq)]
pub(crate) struct OAuthClientConfig {
    pub client_id: String,
    pub client_secret: Option<String>,
}

pub(crate) type DriveOAuthConfig = OAuthClientConfig;
pub(crate) type OneDriveOAuthConfig = OAuthClientConfig;

impl fmt::Debug for OAuthClientConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("OAuthClientConfig")
            .field("client_id", &self.client_id)
            .field("client_secret", &self.client_secret.as_ref().map(|_| "<redacted>"))
            .finish()
    }
}

/// One provider's endpoints and scopes plus the app registration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ProviderAuth {
    pub provider: CloudProvider,
    pub auth_endpoint: &'static str,
    pub token_endpoint: &'static str,
    pub scope: &'static str,
    /// Extra authorization-request parameters.
    pub extra: &'static [(&'static str, &'static str)],
    pub client: OAuthClientConfig,
}

impl ProviderAuth {
    pub(crate) fn drive(client: DriveOAuthConfig) -> Self {
        Self {
            provider: CloudProvider::GoogleDrive,
            auth_endpoint: "https://accounts.google.com/o/oauth2/v2/auth",
            token_endpoint: "https://oauth2.googleapis.com/token",
            scope: "openid https://www.googleapis.com/auth/drive.appdata",
            // Offline + consent so Google always returns a refresh token.
            extra: &[("access_type", "offline"), ("prompt", "consent")],
            client,
        }
    }

    pub(crate) fn onedrive(client: OneDriveOAuthConfig) -> Self {
        Self {
            provider: CloudProvider::OneDrive,
            auth_endpoint: "https://login.microsoftonline.com/common/oauth2/v2.0/authorize",
            token_endpoint: "https://login.microsoftonline.com/common/oauth2/v2.0/token",
            scope: "openid offline_access Files.ReadWrite.AppFolder",
            extra: &[],
            client,
        }
    }
}

/// The loopback redirect URI for a listener bound on `port`.
pub(crate) fn redirect_uri(port: u16) -> String {
    format!("http://127.0.0.1:{port}{CALLBACK_PATH}")
}

fn rng_failed() -> ProviderError {
    ProviderError::Permanent("system random source unavailable".to_owned())
}

/// A PKCE S256 pair. The verifier stays in Rust until the code exchange.
pub(crate) struct Pkce {
    pub verifier: Zeroizing<String>,
    pub challenge: String,
}

impl fmt::Debug for Pkce {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Pkce").field("verifier", &"<redacted>").field("challenge", &self.challenge).finish()
    }
}

impl Pkce {
    /// 64 random bytes as base64url without padding: an 86-character verifier.
    pub(crate) fn generate() -> Result<Self, ProviderError> {
        let mut bytes = Zeroizing::new([0u8; 64]);
        fill_random(&mut bytes[..]).map_err(|_| rng_failed())?;
        let verifier = Zeroizing::new(BASE64URL_NOPAD.encode(&bytes[..]));
        let challenge = pkce_challenge(&verifier);
        Ok(Self { verifier, challenge })
    }
}

/// `base64url-nopad(SHA-256(verifier))` (RFC 7636 §4.2, S256).
pub(crate) fn pkce_challenge(verifier: &str) -> String {
    BASE64URL_NOPAD.encode(&Sha256::digest(verifier.as_bytes()))
}

/// 32 random bytes as base64url: the CSRF `state` the listener must see back.
pub(crate) fn new_state() -> Result<String, ProviderError> {
    let mut bytes = [0u8; 32];
    fill_random(&mut bytes).map_err(|_| rng_failed())?;
    Ok(BASE64URL_NOPAD.encode(&bytes))
}

/// The authorization URL the browser opens.
pub(crate) fn auth_url(cfg: &ProviderAuth, challenge: &str, state: &str, redirect: &str) -> String {
    let mut query = form_urlencoded::Serializer::new(String::new());
    query
        .append_pair("response_type", "code")
        .append_pair("client_id", &cfg.client.client_id)
        .append_pair("redirect_uri", redirect)
        .append_pair("scope", cfg.scope)
        .append_pair("state", state)
        .append_pair("code_challenge", challenge)
        .append_pair("code_challenge_method", "S256");
    for (name, value) in cfg.extra {
        query.append_pair(name, value);
    }
    format!("{}?{}", cfg.auth_endpoint, query.finish())
}

/// A token endpoint answer. Every credential prints `<redacted>`.
#[derive(Clone)]
pub(crate) struct TokenSet {
    pub access_token: Zeroizing<String>,
    pub refresh_token: Option<Zeroizing<String>>,
    pub expires_in: Option<Duration>,
    pub id_token: Option<Zeroizing<String>>,
}

impl fmt::Debug for TokenSet {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let shown = |present: bool| if present { "<redacted>" } else { "<none>" };
        f.debug_struct("TokenSet")
            .field("access_token", &"<redacted>")
            .field("refresh_token", &shown(self.refresh_token.is_some()))
            .field("expires_in", &self.expires_in)
            .field("id_token", &shown(self.id_token.is_some()))
            .finish()
    }
}

/// Exchanges an authorization code (with its PKCE verifier) for tokens.
pub(crate) async fn exchange_code<T: HttpTransport>(
    transport: &T,
    cfg: &ProviderAuth,
    code: &str,
    verifier: &str,
    redirect: &str,
) -> Result<TokenSet, ProviderError> {
    let form = token_form(
        cfg,
        &[("grant_type", "authorization_code"), ("code", code), ("code_verifier", verifier), ("redirect_uri", redirect)],
        false,
    );
    post_token(transport, cfg, form).await
}

/// Redeems a refresh token. A response without a `refresh_token` keeps the
/// old one; a rotated one replaces it in the returned set.
pub(crate) async fn refresh<T: HttpTransport>(
    transport: &T,
    cfg: &ProviderAuth,
    refresh_token: &str,
) -> Result<TokenSet, ProviderError> {
    // Microsoft takes the scope on refresh; Google documents no scope there.
    let with_scope = cfg.provider == CloudProvider::OneDrive;
    let form = token_form(cfg, &[("grant_type", "refresh_token"), ("refresh_token", refresh_token)], with_scope);
    let mut set = post_token(transport, cfg, form).await?;
    if set.refresh_token.is_none() {
        set.refresh_token = Some(Zeroizing::new(refresh_token.to_owned()));
    }
    Ok(set)
}

/// A token-endpoint form body: `pairs`, then the client credentials. Built
/// synchronously (the serializer is not `Send`) and zeroized on drop.
fn token_form(cfg: &ProviderAuth, pairs: &[(&str, &str)], with_scope: bool) -> Zeroizing<String> {
    let mut form = form_urlencoded::Serializer::new(String::new());
    form.extend_pairs(pairs);
    form.append_pair("client_id", &cfg.client.client_id);
    if with_scope {
        form.append_pair("scope", cfg.scope);
    }
    if let Some(secret) = &cfg.client.client_secret {
        form.append_pair("client_secret", secret);
    }
    Zeroizing::new(form.finish())
}

/// POSTs a form to the token endpoint. Error strings name the failure kind
/// only; the response body (which may echo a token) is never copied.
async fn post_token<T: HttpTransport>(
    transport: &T,
    cfg: &ProviderAuth,
    form: Zeroizing<String>,
) -> Result<TokenSet, ProviderError> {
    let request = HttpRequest {
        method: "POST".to_owned(),
        url: cfg.token_endpoint.to_owned(),
        headers: vec![
            ("Content-Type".to_owned(), "application/x-www-form-urlencoded".to_owned()),
            ("Accept".to_owned(), "application/json".to_owned()),
        ],
        body: form.as_bytes().to_vec(),
    };
    let response = transport.send(request).await?;
    let body: Option<Value> = serde_json::from_slice(&response.body).ok();
    if !(200..300).contains(&response.status) {
        let error = body.as_ref().and_then(|v| v.get("error")).and_then(Value::as_str);
        if error == Some("invalid_grant") {
            return Err(ProviderError::Auth { reconsent: true });
        }
        return Err(map_status(response.status, &response.headers));
    }
    let Some(body) = body else {
        return Err(ProviderError::Permanent("token response is not JSON".to_owned()));
    };
    parse_token_set(&body)
}

fn parse_token_set(body: &Value) -> Result<TokenSet, ProviderError> {
    let text = |name: &str| body.get(name).and_then(Value::as_str).filter(|s| !s.is_empty());
    let access_token = text("access_token")
        .ok_or_else(|| ProviderError::Permanent("token response has no access_token".to_owned()))?;
    if let Some(kind) = body.get("token_type").and_then(Value::as_str) {
        if !kind.eq_ignore_ascii_case("bearer") {
            return Err(ProviderError::Permanent("token response is not a bearer token".to_owned()));
        }
    }
    // Microsoft has sent `expires_in` as a string; accept both forms.
    let expires_in = match body.get("expires_in") {
        Some(Value::Number(n)) => n.as_u64(),
        Some(Value::String(s)) => s.trim().parse().ok(),
        _ => None,
    }
    .map(|secs| Duration::from_secs(secs).min(MAX_TOKEN_LIFETIME));
    Ok(TokenSet {
        access_token: Zeroizing::new(access_token.to_owned()),
        refresh_token: text("refresh_token").map(|t| Zeroizing::new(t.to_owned())),
        expires_in,
        id_token: text("id_token").map(|t| Zeroizing::new(t.to_owned())),
    })
}

/// The account identity (§4.7 binding input) from an `id_token`.
///
/// The payload is decoded without a signature check. That is sound here only
/// because the token comes straight from the provider's token endpoint over
/// verified TLS in the same exchange; never pass an id_token from elsewhere.
pub(crate) fn identity_from_id_token(provider: CloudProvider, id_token: &str) -> Result<String, ProviderError> {
    let invalid = || ProviderError::Permanent("id_token is malformed".to_owned());
    let mut parts = id_token.split('.');
    let (Some(_), Some(payload), Some(_), None) = (parts.next(), parts.next(), parts.next(), parts.next()) else {
        return Err(invalid());
    };
    let payload = payload.trim_end_matches('=');
    let bytes = BASE64URL_NOPAD.decode(payload.as_bytes()).map_err(|_| invalid())?;
    let claims: Value = serde_json::from_slice(&bytes).map_err(|_| invalid())?;
    let claim = |name: &str| {
        claims
            .get(name)
            .and_then(Value::as_str)
            .filter(|v| is_identity_part(v))
            .ok_or_else(|| ProviderError::Permanent(format!("id_token has no usable `{name}` claim")))
    };
    match provider {
        CloudProvider::GoogleDrive => Ok(format!("gdrive|{}", claim("sub")?)),
        CloudProvider::OneDrive => Ok(format!("onedrive|{}|{}", claim("tid")?, claim("oid")?)),
        CloudProvider::WebDav | CloudProvider::R2 => {
            Err(ProviderError::Unsupported("provider does not use OAuth".to_owned()))
        }
    }
}

/// A non-empty claim that cannot blur the `|`-joined identity.
fn is_identity_part(value: &str) -> bool {
    !value.is_empty() && value.len() <= 256 && value.bytes().all(|b| b.is_ascii_graphic() && b != b'|')
}

/// The in-memory access token and when it stops being usable.
struct Cached {
    token: Zeroizing<String>,
    fresh_until: Instant,
}

impl Cached {
    /// Usable until `EARLY_REFRESH` before expiry. Without an `expires_in`
    /// the token is used once and not cached.
    fn new(token: Zeroizing<String>, expires_in: Option<Duration>) -> Option<Self> {
        let lifetime = expires_in?.checked_sub(EARLY_REFRESH)?;
        let fresh_until = Instant::now().checked_add(lifetime)?;
        Some(Self { token, fresh_until })
    }

    fn is_fresh(&self) -> bool {
        Instant::now() < self.fresh_until
    }
}

/// Access tokens for one bound account: cached in memory, refreshed through
/// the keychain refresh token, rotated refresh tokens persisted.
pub(crate) struct OAuthTokens<S, T> {
    store: S,
    transport: T,
    cfg: ProviderAuth,
    binding_fp: String,
    cached: Mutex<Option<Cached>>,
    /// A rotated refresh token the keychain refused to store. It is used (and
    /// re-persisted) before the keychain copy, so the rotation is not lost.
    unsaved_refresh: Mutex<Option<Zeroizing<String>>>,
    /// One refresh at a time.
    refreshing: tokio::sync::Mutex<()>,
}

impl<S, T> fmt::Debug for OAuthTokens<S, T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("OAuthTokens")
            .field("provider", &self.cfg.provider)
            .field("binding_fp", &self.binding_fp)
            .finish_non_exhaustive()
    }
}

fn lock<V>(mutex: &Mutex<V>) -> std::sync::MutexGuard<'_, V> {
    mutex.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn secret_error(err: SecretError) -> ProviderError {
    match err {
        // A locked keychain may unlock; the stored token is untouched.
        SecretError::Backend(_) => ProviderError::Transient("keychain unavailable".to_owned()),
        SecretError::InvalidName => ProviderError::Permanent("invalid credential binding".to_owned()),
        SecretError::Corrupt => ProviderError::Auth { reconsent: true },
    }
}

impl<S: SecretStore + Send + Sync, T: HttpTransport> OAuthTokens<S, T> {
    /// An account already bound: the refresh token is read from the keychain
    /// on the first `token()` call.
    pub(crate) fn new(store: S, transport: T, cfg: ProviderAuth, binding_fp: String) -> Self {
        Self {
            store,
            transport,
            cfg,
            binding_fp,
            cached: Mutex::new(None),
            unsaved_refresh: Mutex::new(None),
            refreshing: tokio::sync::Mutex::new(()),
        }
    }

    /// Completes a sign-in: exchanges `code`, derives the identity from the
    /// id_token, stores the refresh token under its binding fingerprint, and
    /// caches the access token. Returns the token source and the identity.
    /// Nothing is stored unless the exchange yields both an identity and a
    /// refresh token.
    pub(crate) async fn connect(
        store: S,
        transport: T,
        cfg: ProviderAuth,
        code: &str,
        verifier: &str,
        redirect: &str,
    ) -> Result<(Self, String), ProviderError> {
        let set = exchange_code(&transport, &cfg, code, verifier, redirect).await?;
        let id_token =
            set.id_token.as_ref().ok_or_else(|| ProviderError::Permanent("token response has no id_token".to_owned()))?;
        let identity = identity_from_id_token(cfg.provider, id_token)?;
        let refresh_token = set
            .refresh_token
            .as_ref()
            .ok_or_else(|| ProviderError::Permanent("token response has no refresh_token".to_owned()))?;
        let fp = binding_fp(&identity);
        write_secret(&store, SyncSecret::Credential { provider: cfg.provider, binding_fp: &fp }, refresh_token.as_bytes())
            .map_err(secret_error)?;
        let tokens = Self::new(store, transport, cfg, fp);
        *lock(&tokens.cached) = Cached::new(set.access_token.clone(), set.expires_in);
        Ok((tokens, identity))
    }

    pub(crate) fn binding_fp(&self) -> &str {
        &self.binding_fp
    }

    fn secret(&self) -> SyncSecret<'_> {
        SyncSecret::Credential { provider: self.cfg.provider, binding_fp: &self.binding_fp }
    }

    fn cached_token(&self) -> Option<String> {
        lock(&self.cached).as_ref().filter(|c| c.is_fresh()).map(|c| c.token.to_string())
    }

    fn load_refresh_token(&self) -> Result<Zeroizing<String>, ProviderError> {
        if let Some(unsaved) = lock(&self.unsaved_refresh).clone() {
            return Ok(unsaved);
        }
        let bytes = read_secret(&self.store, self.secret()).map_err(secret_error)?;
        // No stored token: the account was never connected or was removed.
        let bytes = bytes.ok_or(ProviderError::Auth { reconsent: true })?;
        let text = std::str::from_utf8(&bytes).map_err(|_| ProviderError::Auth { reconsent: true })?;
        Ok(Zeroizing::new(text.to_owned()))
    }

    async fn refresh_now(&self) -> Result<String, ProviderError> {
        let _single = self.refreshing.lock().await;
        // Another caller may have refreshed while this one waited.
        if let Some(token) = self.cached_token() {
            return Ok(token);
        }
        let old = self.load_refresh_token()?;
        // `invalid_grant` is `Auth { reconsent: true }`; the stored token is
        // left in place (the user re-consents; nothing is deleted).
        let set = refresh(&self.transport, &self.cfg, &old).await?;
        if let Some(new) = set.refresh_token.as_ref().filter(|new| new.as_str() != old.as_str()) {
            match write_secret(&self.store, self.secret(), new.as_bytes()) {
                Ok(()) => *lock(&self.unsaved_refresh) = None,
                // The provider may already have revoked `old`: keep `new` in
                // memory and persist it on the next refresh.
                Err(_) => *lock(&self.unsaved_refresh) = Some(new.clone()),
            }
        } else if lock(&self.unsaved_refresh).is_some()
            && write_secret(&self.store, self.secret(), old.as_bytes()).is_ok()
        {
            *lock(&self.unsaved_refresh) = None;
        }
        let token = set.access_token.to_string();
        *lock(&self.cached) = Cached::new(set.access_token, set.expires_in);
        Ok(token)
    }
}

impl<S: SecretStore + Send + Sync, T: HttpTransport> AccessToken for OAuthTokens<S, T> {
    async fn token(&self) -> Result<String, ProviderError> {
        match self.cached_token() {
            Some(token) => Ok(token),
            None => self.refresh_now().await,
        }
    }

    fn invalidate(&self) {
        *lock(&self.cached) = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sync::provider::transport::{HttpResponse, MockTransport, TransportFailure};
    use crate::sync::secrets::MemoryStore;
    use data_encoding::BASE64;

    /// Lets a test keep reading the store the token source writes to.
    struct Shared<'a>(&'a MemoryStore);

    impl SecretStore for Shared<'_> {
        fn get(&self, account: &crate::sync::secrets::Account) -> Result<Option<Zeroizing<String>>, SecretError> {
            self.0.get(account)
        }
        fn set(&self, account: &crate::sync::secrets::Account, value: &str) -> Result<(), SecretError> {
            self.0.set(account, value)
        }
        fn delete(&self, account: &crate::sync::secrets::Account) -> Result<(), SecretError> {
            self.0.delete(account)
        }
    }

    const CLIENT_ID: &str = "test-client.apps.example";
    const SECRET: &str = "client-secret-value";

    fn drive() -> ProviderAuth {
        ProviderAuth::drive(DriveOAuthConfig { client_id: CLIENT_ID.into(), client_secret: Some(SECRET.into()) })
    }

    fn onedrive() -> ProviderAuth {
        ProviderAuth::onedrive(OneDriveOAuthConfig { client_id: CLIENT_ID.into(), client_secret: None })
    }

    fn json(status: u16, body: &str) -> HttpResponse {
        HttpResponse {
            status,
            headers: vec![("Content-Type".into(), "application/json".into())],
            body: body.as_bytes().to_vec(),
        }
    }

    fn form(req: &HttpRequest) -> Vec<(String, String)> {
        form_urlencoded::parse(&req.body).into_owned().collect()
    }

    fn field(req: &HttpRequest, name: &str) -> Option<String> {
        form(req).into_iter().find(|(k, _)| k == name).map(|(_, v)| v)
    }

    fn id_token(claims: &str) -> String {
        format!("eyJhbGciOiJSUzI1NiJ9.{}.c2ln", BASE64URL_NOPAD.encode(claims.as_bytes()))
    }

    fn stored_refresh(store: &MemoryStore, cfg: &ProviderAuth, fp: &str) -> Option<String> {
        read_secret(store, SyncSecret::Credential { provider: cfg.provider, binding_fp: fp })
            .unwrap()
            .map(|b| String::from_utf8(b.to_vec()).unwrap())
    }

    fn seed_refresh(store: &MemoryStore, cfg: &ProviderAuth, fp: &str, token: &str) {
        write_secret(store, SyncSecret::Credential { provider: cfg.provider, binding_fp: fp }, token.as_bytes())
            .unwrap();
    }

    fn fp() -> String {
        binding_fp("gdrive|1234")
    }

    #[test]
    fn pkce_matches_the_rfc_7636_appendix_b_vector() {
        assert_eq!(
            pkce_challenge("dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk"),
            // The RFC value; g6-design.md misquotes the tail of it.
            "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"
        );
    }

    #[test]
    fn generated_pkce_and_state_have_the_specified_shape() {
        let pkce = Pkce::generate().unwrap();
        assert_eq!(pkce.verifier.len(), 86);
        assert!(pkce.verifier.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_'));
        assert_eq!(pkce.challenge, pkce_challenge(&pkce.verifier));
        assert_ne!(Pkce::generate().unwrap().verifier, pkce.verifier);
        let state = new_state().unwrap();
        assert_eq!(state.len(), 43);
        assert_eq!(BASE64URL_NOPAD.decode(state.as_bytes()).unwrap().len(), 32);
        assert_ne!(new_state().unwrap(), state);
        assert!(!format!("{pkce:?}").contains(pkce.verifier.as_str()));
    }

    #[test]
    fn auth_urls_carry_the_provider_endpoints_scopes_and_pkce() {
        let redirect = redirect_uri(53682);
        assert_eq!(redirect, "http://127.0.0.1:53682/callback");
        let url = url::Url::parse(&auth_url(&drive(), "CHAL", "ST", &redirect)).unwrap();
        assert_eq!(format!("{}{}", url.origin().ascii_serialization(), url.path()), drive().auth_endpoint);
        let q: Vec<(String, String)> = url.query_pairs().into_owned().collect();
        let get = |k: &str| q.iter().find(|(n, _)| n == k).map(|(_, v)| v.as_str());
        assert_eq!(get("response_type"), Some("code"));
        assert_eq!(get("client_id"), Some(CLIENT_ID));
        assert_eq!(get("redirect_uri"), Some(redirect.as_str()));
        assert_eq!(get("scope"), Some("openid https://www.googleapis.com/auth/drive.appdata"));
        assert_eq!(get("state"), Some("ST"));
        assert_eq!(get("code_challenge"), Some("CHAL"));
        assert_eq!(get("code_challenge_method"), Some("S256"));
        assert_eq!(get("access_type"), Some("offline"));
        assert_eq!(get("prompt"), Some("consent"));
        assert_eq!(get("client_secret"), None, "the secret never goes to the browser");

        let one = url::Url::parse(&auth_url(&onedrive(), "C", "S", &redirect)).unwrap();
        assert_eq!(one.host_str(), Some("login.microsoftonline.com"));
        assert_eq!(one.path(), "/common/oauth2/v2.0/authorize");
        let scope = one.query_pairs().find(|(k, _)| k == "scope").unwrap().1.into_owned();
        assert_eq!(scope, "openid offline_access Files.ReadWrite.AppFolder");
        assert_eq!(onedrive().token_endpoint, "https://login.microsoftonline.com/common/oauth2/v2.0/token");
        assert_eq!(drive().token_endpoint, "https://oauth2.googleapis.com/token");
    }

    #[tokio::test]
    async fn exchange_posts_the_code_and_verifier_as_a_form() {
        let mock = MockTransport::new(|_| {
            json(200, r#"{"access_token":"at-1","refresh_token":"rt-1","expires_in":3599,"token_type":"Bearer","id_token":"x.y.z"}"#)
        });
        let set = exchange_code(&mock, &drive(), "the code", "verif", "http://127.0.0.1:1/callback").await.unwrap();
        assert_eq!(set.access_token.as_str(), "at-1");
        assert_eq!(set.refresh_token.as_deref().map(String::as_str), Some("rt-1"));
        assert_eq!(set.expires_in, Some(Duration::from_secs(3599)));
        let req = &mock.requests()[0];
        assert_eq!(req.method, "POST");
        assert_eq!(req.url, "https://oauth2.googleapis.com/token");
        assert_eq!(req.header("content-type"), Some("application/x-www-form-urlencoded"));
        assert_eq!(field(req, "grant_type").as_deref(), Some("authorization_code"));
        assert_eq!(field(req, "code").as_deref(), Some("the code"));
        assert_eq!(field(req, "code_verifier").as_deref(), Some("verif"));
        assert_eq!(field(req, "redirect_uri").as_deref(), Some("http://127.0.0.1:1/callback"));
        assert_eq!(field(req, "client_id").as_deref(), Some(CLIENT_ID));
        assert_eq!(field(req, "client_secret").as_deref(), Some(SECRET));

        // A public client (OneDrive) sends no secret.
        let mock = MockTransport::new(|_| json(200, r#"{"access_token":"a","expires_in":"3600"}"#));
        let set = exchange_code(&mock, &onedrive(), "c", "v", "r").await.unwrap();
        assert_eq!(set.expires_in, Some(Duration::from_secs(3600)), "string expires_in is accepted");
        assert_eq!(field(&mock.requests()[0], "client_secret"), None);
    }

    #[tokio::test]
    async fn token_endpoint_errors_map_without_echoing_the_body() {
        let leaky = r#"{"error":"invalid_grant","error_description":"token rt-SECRET is revoked"}"#;
        let mock = MockTransport::new(move |_| json(400, leaky));
        assert_eq!(refresh(&mock, &drive(), "rt-SECRET").await.unwrap_err(), ProviderError::Auth { reconsent: true });

        let cases: [(u16, &str, ProviderError); 5] = [
            (400, r#"{"error":"invalid_request","error_description":"rt-SECRET"}"#, ProviderError::Permanent("HTTP 400".into())),
            (401, r#"{"error":"invalid_client"}"#, ProviderError::Auth { reconsent: true }),
            (503, "rt-SECRET", ProviderError::RateLimited { retry_after: None }),
            (500, "", ProviderError::Transient("HTTP 500".into())),
            (200, r#"{"token_type":"Bearer","note":"rt-SECRET"}"#, ProviderError::Permanent("token response has no access_token".into())),
        ];
        for (status, body, expected) in cases {
            let body = body.to_owned();
            let mock = MockTransport::new(move |_| json(status, &body));
            let err = refresh(&mock, &drive(), "rt-SECRET").await.unwrap_err();
            assert_eq!(err, expected, "{status}");
            assert!(!format!("{err:?}").contains("rt-SECRET"), "{err:?}");
        }
        let mock = MockTransport::new(|_| json(200, r#"{"access_token":"a","token_type":"mac"}"#));
        assert!(matches!(refresh(&mock, &drive(), "r").await, Err(ProviderError::Permanent(_))));
        let mock = MockTransport::new(|_| json(200, "not json"));
        assert!(matches!(refresh(&mock, &drive(), "r").await, Err(ProviderError::Permanent(_))));
        let mock = MockTransport::new(|_| json(200, ""));
        mock.script_next(Err(TransportFailure::Timeout));
        assert!(matches!(refresh(&mock, &drive(), "r").await, Err(ProviderError::Transient(_))));
    }

    #[tokio::test]
    async fn refresh_keeps_the_old_token_unless_one_is_rotated_in() {
        let mock = MockTransport::new(|_| json(200, r#"{"access_token":"at","expires_in":3600}"#));
        let kept = refresh(&mock, &drive(), "rt-old").await.unwrap();
        assert_eq!(kept.refresh_token.as_deref().map(String::as_str), Some("rt-old"));
        let req = &mock.requests()[0];
        assert_eq!(field(req, "grant_type").as_deref(), Some("refresh_token"));
        assert_eq!(field(req, "refresh_token").as_deref(), Some("rt-old"));
        assert_eq!(field(req, "scope"), None, "Google takes no scope on refresh");

        let mock = MockTransport::new(|_| json(200, r#"{"access_token":"at","refresh_token":"rt-new"}"#));
        let rotated = refresh(&mock, &onedrive(), "rt-old").await.unwrap();
        assert_eq!(
            field(&mock.requests()[0], "scope").as_deref(),
            Some("openid offline_access Files.ReadWrite.AppFolder")
        );
        assert_eq!(rotated.refresh_token.as_deref().map(String::as_str), Some("rt-new"));
    }

    #[test]
    fn identity_comes_from_the_id_token_claims() {
        let google = id_token(r#"{"sub":"1234","aud":"x","email":"a@b.c"}"#);
        assert_eq!(identity_from_id_token(CloudProvider::GoogleDrive, &google).unwrap(), "gdrive|1234");
        let ms = id_token(r#"{"tid":"tenant-1","oid":"object-2","sub":"pairwise"}"#);
        assert_eq!(identity_from_id_token(CloudProvider::OneDrive, &ms).unwrap(), "onedrive|tenant-1|object-2");
        // Padded payloads (some encoders add `=`) still decode.
        let padded = format!(
            "h.{}.s",
            data_encoding::BASE64URL.encode(br#"{"sub":"778"}"#)
        );
        assert_eq!(identity_from_id_token(CloudProvider::GoogleDrive, &padded).unwrap(), "gdrive|778");
    }

    #[test]
    fn a_missing_or_unusable_claim_is_permanent() {
        let bad = [
            (CloudProvider::GoogleDrive, id_token(r#"{"email":"a@b.c"}"#)),
            (CloudProvider::GoogleDrive, id_token(r#"{"sub":""}"#)),
            (CloudProvider::GoogleDrive, id_token(r#"{"sub":42}"#)),
            (CloudProvider::OneDrive, id_token(r#"{"tid":"t"}"#)),
            (CloudProvider::OneDrive, id_token(r#"{"oid":"o"}"#)),
            (CloudProvider::OneDrive, id_token(r#"{"tid":"t|x","oid":"o"}"#)),
            (CloudProvider::GoogleDrive, "only.two".to_owned()),
            (CloudProvider::GoogleDrive, "a.!!!.c".to_owned()),
            (CloudProvider::GoogleDrive, format!("a.{}.c", BASE64URL_NOPAD.encode(b"not json"))),
        ];
        for (provider, token) in bad {
            assert!(
                matches!(identity_from_id_token(provider, &token), Err(ProviderError::Permanent(_))),
                "{token}"
            );
        }
    }

    #[tokio::test]
    async fn connect_stores_the_refresh_token_under_the_binding_and_caches_access() {
        let token = id_token(r#"{"sub":"1234"}"#);
        let body = format!(r#"{{"access_token":"at-1","refresh_token":"rt-1","expires_in":3600,"id_token":"{token}"}}"#);
        let mock = MockTransport::new(move |_| json(200, &body));
        let handle = mock.clone();
        let store = MemoryStore::default();
        let (tokens, identity) =
            OAuthTokens::connect(store, mock, drive(), "code", "verifier", "http://127.0.0.1:9/callback").await.unwrap();
        assert_eq!(identity, "gdrive|1234");
        assert_eq!(tokens.binding_fp(), fp());
        assert_eq!(stored_refresh(&tokens.store, &tokens.cfg, &fp()).as_deref(), Some("rt-1"));
        let account = format!("sync:cred:gdrive:{}", fp());
        assert_eq!(tokens.store.raw(&account), Some(BASE64.encode(b"rt-1")));
        assert_eq!(tokens.store.accounts(), vec![account], "only the refresh token is persisted");
        assert_eq!(tokens.token().await.unwrap(), "at-1");
        assert_eq!(handle.request_count(), 1, "the exchanged access token is served from memory");
    }

    #[tokio::test]
    async fn connect_without_a_refresh_token_or_identity_stores_nothing() {
        for body in [
            format!(r#"{{"access_token":"a","id_token":"{}"}}"#, id_token(r#"{"sub":"1"}"#)),
            r#"{"access_token":"a","refresh_token":"r"}"#.to_owned(),
            format!(r#"{{"access_token":"a","refresh_token":"r","id_token":"{}"}}"#, id_token("{}")),
        ] {
            let mock = MockTransport::new(move |_| json(200, &body));
            let store = MemoryStore::default();
            let err = OAuthTokens::connect(Shared(&store), mock, drive(), "c", "v", "r").await.unwrap_err();
            assert!(matches!(err, ProviderError::Permanent(_)), "{err:?}");
            assert!(store.accounts().is_empty());
        }
    }

    #[tokio::test]
    async fn tokens_are_cached_and_refreshed_only_when_stale() {
        let store = MemoryStore::default();
        seed_refresh(&store, &drive(), &fp(), "rt-1");
        let counter = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let seen = counter.clone();
        let mock = MockTransport::new(move |_| {
            let n = seen.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1;
            json(200, &format!(r#"{{"access_token":"at-{n}","expires_in":3600}}"#))
        });
        let tokens = OAuthTokens::new(Shared(&store), mock.clone(), drive(), fp());
        assert_eq!(tokens.token().await.unwrap(), "at-1");
        assert_eq!(tokens.token().await.unwrap(), "at-1");
        assert_eq!(mock.request_count(), 1, "a fresh token is cached");
        assert_eq!(field(&mock.requests()[0], "refresh_token").as_deref(), Some("rt-1"));
        tokens.invalidate();
        assert_eq!(tokens.token().await.unwrap(), "at-2");
        assert_eq!(mock.request_count(), 2, "invalidate forces one refresh");
    }

    #[tokio::test]
    async fn a_token_inside_the_early_refresh_window_is_refreshed() {
        let store = MemoryStore::default();
        seed_refresh(&store, &drive(), &fp(), "rt-1");
        for expires in ["60", "30", "null"] {
            let body = format!(r#"{{"access_token":"at","expires_in":{expires}}}"#);
            let mock = MockTransport::new(move |_| json(200, &body));
            let tokens = OAuthTokens::new(Shared(&store), mock.clone(), drive(), fp());
            tokens.token().await.unwrap();
            tokens.token().await.unwrap();
            assert_eq!(mock.request_count(), 2, "expires_in {expires} is never served from cache");
        }
        // 61 s leaves one second of use: still cached right away.
        let mock = MockTransport::new(|_| json(200, r#"{"access_token":"at","expires_in":61}"#));
        let tokens = OAuthTokens::new(Shared(&store), mock.clone(), drive(), fp());
        tokens.token().await.unwrap();
        tokens.token().await.unwrap();
        assert_eq!(mock.request_count(), 1);
    }

    #[tokio::test]
    async fn a_rotated_refresh_token_is_persisted_and_used_next() {
        let store = MemoryStore::default();
        seed_refresh(&store, &onedrive(), &fp(), "rt-1");
        let mock = MockTransport::new(|req| {
            let next = match field(req, "refresh_token").as_deref() {
                Some("rt-1") => "rt-2",
                Some("rt-2") => "rt-3",
                _ => return json(400, r#"{"error":"invalid_grant"}"#),
            };
            json(200, &format!(r#"{{"access_token":"at-{next}","refresh_token":"{next}","expires_in":3600}}"#))
        });
        let tokens = OAuthTokens::new(Shared(&store), mock.clone(), onedrive(), fp());
        assert_eq!(tokens.token().await.unwrap(), "at-rt-2");
        assert_eq!(stored_refresh(&store, &onedrive(), &fp()).as_deref(), Some("rt-2"));
        tokens.invalidate();
        assert_eq!(tokens.token().await.unwrap(), "at-rt-3");
        assert_eq!(stored_refresh(&store, &onedrive(), &fp()).as_deref(), Some("rt-3"));

        // Without a rotation the stored token is left as is.
        let mock = MockTransport::new(|_| json(200, r#"{"access_token":"a","expires_in":3600}"#));
        OAuthTokens::new(Shared(&store), mock, onedrive(), fp()).token().await.unwrap();
        assert_eq!(stored_refresh(&store, &onedrive(), &fp()).as_deref(), Some("rt-3"));
    }

    #[tokio::test]
    async fn a_rotation_the_keychain_refuses_is_kept_in_memory_and_retried() {
        let store = MemoryStore::default();
        seed_refresh(&store, &drive(), &fp(), "rt-1");
        let mock = MockTransport::new(|req| match field(req, "refresh_token").as_deref() {
            Some("rt-1") => json(200, r#"{"access_token":"a1","refresh_token":"rt-2","expires_in":3600}"#),
            Some("rt-2") => json(200, r#"{"access_token":"a2","expires_in":3600}"#),
            _ => json(400, r#"{"error":"invalid_grant"}"#),
        });
        let tokens = OAuthTokens::new(Shared(&store), mock.clone(), drive(), fp());
        store.fail_next_sets(1);
        assert_eq!(tokens.token().await.unwrap(), "a1");
        assert_eq!(stored_refresh(&store, &drive(), &fp()).as_deref(), Some("rt-1"));
        tokens.invalidate();
        assert_eq!(tokens.token().await.unwrap(), "a2", "the unsaved rotation is used, not the stale keychain copy");
        assert_eq!(field(&mock.requests()[1], "refresh_token").as_deref(), Some("rt-2"));
        assert_eq!(stored_refresh(&store, &drive(), &fp()).as_deref(), Some("rt-2"), "persisted on the retry");
    }

    #[tokio::test]
    async fn invalid_grant_asks_for_reconsent_and_keeps_the_stored_token() {
        let store = MemoryStore::default();
        seed_refresh(&store, &drive(), &fp(), "rt-revoked");
        let mock = MockTransport::new(|_| json(400, r#"{"error":"invalid_grant","error_description":"rt-revoked"}"#));
        let tokens = OAuthTokens::new(Shared(&store), mock, drive(), fp());
        let err = tokens.token().await.unwrap_err();
        assert_eq!(err, ProviderError::Auth { reconsent: true });
        assert!(!format!("{err:?}").contains("rt-revoked"));
        assert_eq!(stored_refresh(&store, &drive(), &fp()).as_deref(), Some("rt-revoked"));
    }

    #[tokio::test]
    async fn missing_or_unreadable_keychain_entries_are_classified() {
        let mock = MockTransport::new(|_| json(200, r#"{"access_token":"a"}"#));
        let empty = MemoryStore::default();
        let tokens = OAuthTokens::new(Shared(&empty), mock.clone(), drive(), fp());
        assert_eq!(tokens.token().await.unwrap_err(), ProviderError::Auth { reconsent: true });

        let locked = MemoryStore::default();
        seed_refresh(&locked, &drive(), &fp(), "rt");
        locked.fail_next_gets(1);
        let tokens = OAuthTokens::new(Shared(&locked), mock.clone(), drive(), fp());
        assert!(matches!(tokens.token().await, Err(ProviderError::Transient(_))));
        assert_eq!(tokens.token().await.unwrap(), "a", "the keychain unlocked; the token was untouched");
        assert_eq!(mock.request_count(), 1, "nothing was sent while the keychain was locked");

        let bad = OAuthTokens::new(Shared(&empty), mock, drive(), "not:an:id".to_owned());
        assert!(matches!(bad.token().await, Err(ProviderError::Permanent(_))));
    }

    #[tokio::test]
    async fn no_token_reaches_debug_output() {
        let store = MemoryStore::default();
        seed_refresh(&store, &drive(), &fp(), "rt-hidden");
        let mock = MockTransport::new(|_| {
            json(200, r#"{"access_token":"at-hidden","refresh_token":"rt-rotated","expires_in":3600,"id_token":"id-hidden"}"#)
        });
        let tokens = OAuthTokens::new(Shared(&store), mock.clone(), drive(), fp());
        tokens.token().await.unwrap();
        let set = refresh(&mock, &drive(), "rt-hidden").await.unwrap();
        let shown = format!("{tokens:?} {set:?} {:?} {:?}", drive(), mock.requests());
        for secret in ["at-hidden", "rt-hidden", "rt-rotated", "id-hidden", SECRET] {
            assert!(!shown.contains(secret), "{secret} leaked: {shown}");
        }
    }
}
