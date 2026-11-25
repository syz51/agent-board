//! Single-user OAuth 2.1 Authorization Server for the remote MCP gateway.
//!
//! Implements:
//! - RFC 9728 Protected Resource Metadata (`/.well-known/oauth-protected-resource`)
//! - RFC 8414 Authorization Server Metadata (`/.well-known/oauth-authorization-server`)
//! - RFC 7591 Dynamic Client Registration (`POST /oauth/register`) with redirect URI prefix pinning
//! - Optional pre-configured static OAuth client (`client_id` + `client_secret`)
//! - OAuth 2.1 Authorization Code flow with mandatory PKCE (`S256`), long `state` support
//!   (up to 8 KiB for Gemini), CSRF-protected single-user passphrase login, and rate limiting
//! - Rotating refresh tokens and SHA-256-hashed token persistence in SQLite.

use std::collections::{HashMap, VecDeque};
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use base64::Engine as _;
use base64::engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD};
use board_proto::Timestamp;
use chrono::{TimeDelta, Utc};
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq as _;
use url::Url;

/// Maximum `state` parameter length in bytes. Gemini sends opaque state strings
/// exceeding 2,000 characters, so 8 KiB gives ample headroom while bounding storage.
pub const MAX_STATE_BYTES: usize = 8192;
pub const MAX_REDIRECT_URI_BYTES: usize = 2048;
pub const MAX_CLIENT_NAME_BYTES: usize = 200;
pub const MAX_SCOPE_BYTES: usize = 512;
pub const DEFAULT_SCOPE: &str = "board:admin";

const AUTH_REQUEST_TTL_SECS: i64 = 600; // 10 minutes
const AUTH_CODE_TTL_SECS: i64 = 300; // 5 minutes
const ACCESS_TOKEN_TTL_SECS: i64 = 3600; // 1 hour
const REFRESH_TOKEN_TTL_SECS: i64 = 90 * 24 * 3600; // 90 days
const MAX_LOGIN_FAILURES_PER_WINDOW: usize = 10;
const MAX_GLOBAL_LOGIN_FAILURES_PER_WINDOW: usize = 20;
const MAX_TRACKED_FAILURE_IPS: usize = 1024;
const LOGIN_FAILURE_WINDOW_SECS: i64 = 600;

/// Injectable clock for deterministic expiry and rate-limit tests.
pub type Clock = Arc<dyn Fn() -> Timestamp + Send + Sync>;

pub fn system_clock() -> Clock {
    Arc::new(Utc::now)
}

/// Optional pre-configured static OAuth client (for clients using static Client ID/Secret).
#[derive(Debug, Clone)]
pub struct StaticClient {
    pub client_id: String,
    pub client_secret: String,
}

#[derive(Debug, Clone)]
pub struct OAuthSettings {
    /// Public HTTPS base URL without trailing slash (e.g. `https://board-mcp.example.com`).
    pub public_url: String,
    /// SHA-256 digest of the single-user admin passphrase.
    pub passphrase_sha256: [u8; 32],
    /// Allowed `redirect_uri` prefixes for DCR and authorization requests.
    pub allowed_redirect_prefixes: Vec<String>,
    /// Optional pre-registered static client credentials.
    pub static_client: Option<StaticClient>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ProtectedResourceMetadata {
    pub resource: String,
    pub authorization_servers: Vec<String>,
    pub bearer_methods_supported: Vec<&'static str>,
    pub scopes_supported: Vec<&'static str>,
}

#[derive(Debug, Clone, Serialize)]
pub struct AuthorizationServerMetadata {
    pub issuer: String,
    pub authorization_endpoint: String,
    pub token_endpoint: String,
    pub registration_endpoint: String,
    pub response_types_supported: Vec<&'static str>,
    pub grant_types_supported: Vec<&'static str>,
    pub code_challenge_methods_supported: Vec<&'static str>,
    pub token_endpoint_auth_methods_supported: Vec<&'static str>,
    pub scopes_supported: Vec<&'static str>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct RegisterClientRequest {
    pub redirect_uris: Vec<String>,
    #[serde(default)]
    pub client_name: Option<String>,
    #[serde(default)]
    pub token_endpoint_auth_method: Option<String>,
    #[serde(default)]
    pub grant_types: Option<Vec<String>>,
    #[serde(default)]
    pub response_types: Option<Vec<String>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RegisterClientResponse {
    pub client_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub client_secret: Option<String>,
    pub client_id_issued_at: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub client_secret_expires_at: Option<i64>,
    pub redirect_uris: Vec<String>,
    pub token_endpoint_auth_method: String,
    pub grant_types: Vec<String>,
    pub response_types: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub client_name: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct AuthorizeQuery {
    pub response_type: String,
    pub client_id: String,
    pub redirect_uri: String,
    pub code_challenge: String,
    pub code_challenge_method: String,
    #[serde(default)]
    pub state: Option<String>,
    #[serde(default)]
    pub scope: Option<String>,
}

#[derive(Debug, Clone)]
pub struct PendingAuthorization {
    pub request_id: String,
    pub client_id: String,
    pub client_name: String,
    pub redirect_uri: String,
    pub scope: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct TokenRequest {
    pub grant_type: String,
    #[serde(default)]
    pub code: Option<String>,
    #[serde(default)]
    pub redirect_uri: Option<String>,
    #[serde(default)]
    pub client_id: Option<String>,
    #[serde(default)]
    pub client_secret: Option<String>,
    #[serde(default)]
    pub code_verifier: Option<String>,
    #[serde(default)]
    pub refresh_token: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TokenResponse {
    pub access_token: String,
    pub token_type: &'static str,
    pub expires_in: i64,
    pub refresh_token: String,
    pub scope: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct OAuthError {
    pub error: &'static str,
    pub error_description: String,
}

impl OAuthError {
    pub fn new(error: &'static str, description: impl Into<String>) -> Self {
        Self {
            error,
            error_description: description.into(),
        }
    }
}

#[derive(Default)]
struct LoginFailureTracker {
    global: VecDeque<Timestamp>,
    per_ip: HashMap<String, VecDeque<Timestamp>>,
}

#[derive(Clone)]
pub struct OAuthStore {
    db: Arc<Mutex<Connection>>,
    settings: OAuthSettings,
    clock: Clock,
    login_failures: Arc<Mutex<LoginFailureTracker>>,
}

impl OAuthStore {
    pub fn open(path: &Path, settings: OAuthSettings, clock: Clock) -> Result<Self> {
        if let Some(parent) = path.parent()
            && !parent.as_os_str().is_empty()
        {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating {}", parent.display()))?;
        }
        let conn = Connection::open(path)
            .with_context(|| format!("opening oauth database {}", path.display()))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
        }
        conn.busy_timeout(Duration::from_secs(5))?;
        conn.execute_batch(
            "PRAGMA journal_mode = WAL;
             PRAGMA synchronous = NORMAL;
             PRAGMA foreign_keys = ON;
             CREATE TABLE IF NOT EXISTS oauth_clients (
                 client_id TEXT PRIMARY KEY,
                 client_secret_hash TEXT,
                 auth_method TEXT NOT NULL,
                 client_name TEXT,
                 redirect_uris_json TEXT NOT NULL,
                 created_at INTEGER NOT NULL
             );
             CREATE TABLE IF NOT EXISTS oauth_auth_requests (
                 request_id TEXT PRIMARY KEY,
                 client_id TEXT NOT NULL,
                 redirect_uri TEXT NOT NULL,
                 code_challenge TEXT NOT NULL,
                 state TEXT,
                 scope TEXT NOT NULL,
                 expires_at INTEGER NOT NULL
             );
             CREATE TABLE IF NOT EXISTS oauth_auth_codes (
                 code_hash TEXT PRIMARY KEY,
                 client_id TEXT NOT NULL,
                 redirect_uri TEXT NOT NULL,
                 code_challenge TEXT NOT NULL,
                 scope TEXT NOT NULL,
                 expires_at INTEGER NOT NULL,
                 used_at INTEGER
             );
             CREATE TABLE IF NOT EXISTS oauth_access_tokens (
                 token_hash TEXT PRIMARY KEY,
                 client_id TEXT NOT NULL,
                 scope TEXT NOT NULL,
                 expires_at INTEGER NOT NULL,
                 revoked_at INTEGER
             );
             CREATE TABLE IF NOT EXISTS oauth_refresh_tokens (
                 token_hash TEXT PRIMARY KEY,
                 client_id TEXT NOT NULL,
                 scope TEXT NOT NULL,
                 expires_at INTEGER NOT NULL,
                 revoked_at INTEGER
             );",
        )
        .context("initializing oauth schema")?;

        Ok(Self {
            db: Arc::new(Mutex::new(conn)),
            settings,
            clock,
            login_failures: Arc::new(Mutex::new(LoginFailureTracker::default())),
        })
    }

    pub fn public_url(&self) -> &str {
        &self.settings.public_url
    }

    pub fn protected_resource_metadata(&self) -> ProtectedResourceMetadata {
        let base = self.settings.public_url.trim_end_matches('/');
        ProtectedResourceMetadata {
            resource: format!("{base}/mcp"),
            authorization_servers: vec![base.to_owned()],
            bearer_methods_supported: vec!["header"],
            scopes_supported: vec![DEFAULT_SCOPE],
        }
    }

    pub fn authorization_server_metadata(&self) -> AuthorizationServerMetadata {
        let base = self.settings.public_url.trim_end_matches('/');
        AuthorizationServerMetadata {
            issuer: base.to_owned(),
            authorization_endpoint: format!("{base}/oauth/authorize"),
            token_endpoint: format!("{base}/oauth/token"),
            registration_endpoint: format!("{base}/oauth/register"),
            response_types_supported: vec!["code"],
            grant_types_supported: vec!["authorization_code", "refresh_token"],
            code_challenge_methods_supported: vec!["S256"],
            token_endpoint_auth_methods_supported: vec![
                "none",
                "client_secret_post",
                "client_secret_basic",
            ],
            scopes_supported: vec![DEFAULT_SCOPE],
        }
    }

    pub fn www_authenticate_header(&self) -> String {
        let base = self.settings.public_url.trim_end_matches('/');
        format!(
            "Bearer resource_metadata=\"{base}/.well-known/oauth-protected-resource\", \
             scope=\"{DEFAULT_SCOPE}\""
        )
    }

    pub fn register_client(
        &self,
        req: RegisterClientRequest,
    ) -> std::result::Result<RegisterClientResponse, OAuthError> {
        if req.redirect_uris.is_empty() || req.redirect_uris.len() > 10 {
            return Err(OAuthError::new(
                "invalid_redirect_uri",
                "redirect_uris must contain between 1 and 10 URIs",
            ));
        }
        for uri in &req.redirect_uris {
            self.validate_redirect_uri(uri)?;
        }
        let auth_method = req
            .token_endpoint_auth_method
            .unwrap_or_else(|| "none".to_owned());
        if !matches!(
            auth_method.as_str(),
            "none" | "client_secret_post" | "client_secret_basic"
        ) {
            return Err(OAuthError::new(
                "invalid_client_metadata",
                format!("unsupported token_endpoint_auth_method: {auth_method}"),
            ));
        }
        if let Some(ref grants) = req.grant_types
            && grants
                .iter()
                .any(|g| !matches!(g.as_str(), "authorization_code" | "refresh_token"))
        {
            return Err(OAuthError::new(
                "invalid_client_metadata",
                "only authorization_code and refresh_token grant types are supported",
            ));
        }
        if let Some(ref resp) = req.response_types
            && resp.iter().any(|r| r != "code")
        {
            return Err(OAuthError::new(
                "invalid_client_metadata",
                "only response_type=code is supported",
            ));
        }
        let client_name = req.client_name.map(|n| {
            n.trim()
                .chars()
                .take(MAX_CLIENT_NAME_BYTES)
                .collect::<String>()
        });

        let client_id = format!("dcr_{}", random_token::<16>());
        let (client_secret, client_secret_hash, secret_expires) = if auth_method == "none" {
            (None, None, None)
        } else {
            let secret = random_token::<32>();
            let hash = sha256_hex(&secret);
            (Some(secret), Some(hash), Some(0))
        };
        let now = (self.clock)().timestamp();
        let redirect_uris_json = serde_json::to_string(&req.redirect_uris)
            .map_err(|e| OAuthError::new("server_error", format!("serializing URIs: {e}")))?;

        let db = self
            .db
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        db.execute(
            "INSERT INTO oauth_clients \
             (client_id, client_secret_hash, auth_method, client_name, redirect_uris_json, created_at) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                &client_id,
                &client_secret_hash,
                &auth_method,
                &client_name,
                &redirect_uris_json,
                now
            ],
        )
        .map_err(|e| OAuthError::new("server_error", format!("saving client: {e}")))?;

        Ok(RegisterClientResponse {
            client_id,
            client_secret,
            client_id_issued_at: now,
            client_secret_expires_at: secret_expires,
            redirect_uris: req.redirect_uris,
            token_endpoint_auth_method: auth_method,
            grant_types: vec!["authorization_code".into(), "refresh_token".into()],
            response_types: vec!["code".into()],
            client_name,
        })
    }

    pub fn begin_authorize(
        &self,
        query: &AuthorizeQuery,
    ) -> std::result::Result<PendingAuthorization, OAuthError> {
        if query.response_type != "code" {
            return Err(OAuthError::new(
                "unsupported_response_type",
                "response_type must be code",
            ));
        }
        if query.code_challenge_method != "S256" {
            return Err(OAuthError::new(
                "invalid_request",
                "code_challenge_method must be S256 (PKCE is required)",
            ));
        }
        if !is_valid_pkce_challenge(&query.code_challenge) {
            return Err(OAuthError::new(
                "invalid_request",
                "code_challenge must be a valid base64url S256 digest",
            ));
        }
        if let Some(ref state) = query.state
            && state.len() > MAX_STATE_BYTES
        {
            return Err(OAuthError::new(
                "invalid_request",
                format!("state exceeds {MAX_STATE_BYTES} bytes"),
            ));
        }
        self.validate_redirect_uri(&query.redirect_uri)?;

        let client_name = self.verify_client_redirect(&query.client_id, &query.redirect_uri)?;
        let scope = sanitize_scope(query.scope.as_deref())?;
        let request_id = random_token::<24>();
        let now = (self.clock)().timestamp();
        let expires_at = now + AUTH_REQUEST_TTL_SECS;

        let db = self
            .db
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let _ = db.execute(
            "DELETE FROM oauth_auth_requests WHERE expires_at <= ?1",
            [now],
        );
        db.execute(
            "INSERT INTO oauth_auth_requests \
             (request_id, client_id, redirect_uri, code_challenge, state, scope, expires_at) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                &request_id,
                &query.client_id,
                &query.redirect_uri,
                &query.code_challenge,
                &query.state,
                &scope,
                expires_at
            ],
        )
        .map_err(|e| OAuthError::new("server_error", format!("saving auth request: {e}")))?;

        Ok(PendingAuthorization {
            request_id,
            client_id: query.client_id.clone(),
            client_name,
            redirect_uri: query.redirect_uri.clone(),
            scope,
        })
    }

    /// Completes the single-user passphrase consent form and returns the redirect URL
    /// containing `?code=...&state=...` (or `?error=access_denied...` if `approve` is false).
    pub fn complete_authorize(
        &self,
        request_id: &str,
        passphrase: &str,
        approve: bool,
        client_ip: &str,
    ) -> std::result::Result<String, AuthorizeFormError> {
        let now_ts = (self.clock)();
        let now = now_ts.timestamp();

        if !approve {
            let req = self.take_auth_request(request_id, now)?;
            return build_redirect(
                &req.redirect_uri,
                &[("error", "access_denied")],
                req.state.as_deref(),
            )
            .map_err(|e| AuthorizeFormError::Invalid(e.to_string()));
        }

        if self.is_rate_limited(client_ip, now_ts) {
            return Err(AuthorizeFormError::RateLimited);
        }

        let candidate_hash: [u8; 32] = Sha256::digest(passphrase.as_bytes()).into();
        let ok: bool = candidate_hash
            .ct_eq(&self.settings.passphrase_sha256)
            .into();
        if !ok {
            self.record_login_failure(client_ip, now_ts);
            let pending = self.peek_auth_request(request_id, now)?;
            return Err(AuthorizeFormError::BadPassphrase(pending));
        }

        let req = self.take_auth_request(request_id, now)?;
        let code = random_token::<32>();
        let code_hash = sha256_hex(&code);
        let code_expires = now + AUTH_CODE_TTL_SECS;

        let db = self
            .db
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        db.execute(
            "INSERT INTO oauth_auth_codes \
             (code_hash, client_id, redirect_uri, code_challenge, scope, expires_at) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                &code_hash,
                &req.client_id,
                &req.redirect_uri,
                &req.code_challenge,
                &req.scope,
                code_expires
            ],
        )
        .map_err(|e| AuthorizeFormError::Invalid(format!("saving auth code: {e}")))?;

        build_redirect(&req.redirect_uri, &[("code", &code)], req.state.as_deref())
            .map_err(|e| AuthorizeFormError::Invalid(e.to_string()))
    }

    pub fn exchange_token(
        &self,
        req: &TokenRequest,
        basic_auth: Option<&str>,
    ) -> std::result::Result<TokenResponse, OAuthError> {
        let (basic_id, basic_secret) = parse_basic_auth(basic_auth)?;
        let client_id = req
            .client_id
            .as_deref()
            .or(basic_id.as_deref())
            .ok_or_else(|| OAuthError::new("invalid_client", "client_id is required"))?;
        let client_secret = req.client_secret.as_deref().or(basic_secret.as_deref());

        self.verify_client_credentials(client_id, client_secret)?;

        match req.grant_type.as_str() {
            "authorization_code" => self.exchange_auth_code(client_id, req),
            "refresh_token" => self.exchange_refresh_token(client_id, req),
            other => Err(OAuthError::new(
                "unsupported_grant_type",
                format!("unsupported grant_type: {other}"),
            )),
        }
    }

    pub fn verify_access_token(&self, token: &str) -> bool {
        if token.is_empty() || token.len() > 512 {
            return false;
        }
        let hash = sha256_hex(token);
        let now = (self.clock)().timestamp();
        let db = self
            .db
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        db.query_row(
            "SELECT 1 FROM oauth_access_tokens \
             WHERE token_hash = ?1 AND revoked_at IS NULL AND expires_at > ?2",
            params![&hash, now],
            |_| Ok(()),
        )
        .optional()
        .ok()
        .flatten()
        .is_some()
    }

    fn exchange_auth_code(
        &self,
        client_id: &str,
        req: &TokenRequest,
    ) -> std::result::Result<TokenResponse, OAuthError> {
        let code = req
            .code
            .as_deref()
            .filter(|s| !s.is_empty())
            .ok_or_else(|| OAuthError::new("invalid_request", "code is required"))?;
        let redirect_uri = req
            .redirect_uri
            .as_deref()
            .filter(|s| !s.is_empty())
            .ok_or_else(|| OAuthError::new("invalid_request", "redirect_uri is required"))?;
        let verifier = req
            .code_verifier
            .as_deref()
            .filter(|s| !s.is_empty())
            .ok_or_else(|| OAuthError::new("invalid_request", "code_verifier is required"))?;

        let code_hash = sha256_hex(code);
        let now = (self.clock)().timestamp();
        let mut db = self
            .db
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let tx = db
            .transaction()
            .map_err(|e| OAuthError::new("server_error", format!("starting tx: {e}")))?;
        let row: Option<(String, String, String, String, i64, Option<i64>)> = tx
            .query_row(
                "SELECT client_id, redirect_uri, code_challenge, scope, expires_at, used_at \
                 FROM oauth_auth_codes WHERE code_hash = ?1",
                [&code_hash],
                |r| {
                    Ok((
                        r.get(0)?,
                        r.get(1)?,
                        r.get(2)?,
                        r.get(3)?,
                        r.get(4)?,
                        r.get(5)?,
                    ))
                },
            )
            .optional()
            .map_err(|e| OAuthError::new("server_error", format!("reading code: {e}")))?;

        let Some((stored_client, stored_redirect, stored_challenge, scope, expires_at, used_at)) =
            row
        else {
            return Err(OAuthError::new(
                "invalid_grant",
                "unknown authorization code",
            ));
        };
        if used_at.is_some() || expires_at <= now {
            return Err(OAuthError::new(
                "invalid_grant",
                "authorization code expired or already used",
            ));
        }
        if stored_client != client_id || stored_redirect != redirect_uri {
            return Err(OAuthError::new(
                "invalid_grant",
                "authorization code client or redirect_uri mismatch",
            ));
        }
        if !verify_pkce_s256(verifier, &stored_challenge) {
            return Err(OAuthError::new("invalid_grant", "PKCE verification failed"));
        }

        tx.execute(
            "UPDATE oauth_auth_codes SET used_at = ?1 WHERE code_hash = ?2",
            params![now, &code_hash],
        )
        .map_err(|e| OAuthError::new("server_error", format!("marking code used: {e}")))?;

        let resp = issue_tokens(&tx, client_id, &scope, now)?;
        tx.commit()
            .map_err(|e| OAuthError::new("server_error", format!("committing token tx: {e}")))?;
        Ok(resp)
    }

    fn exchange_refresh_token(
        &self,
        client_id: &str,
        req: &TokenRequest,
    ) -> std::result::Result<TokenResponse, OAuthError> {
        let refresh = req
            .refresh_token
            .as_deref()
            .filter(|s| !s.is_empty())
            .ok_or_else(|| OAuthError::new("invalid_request", "refresh_token is required"))?;

        let hash = sha256_hex(refresh);
        let now = (self.clock)().timestamp();
        let mut db = self
            .db
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let tx = db
            .transaction()
            .map_err(|e| OAuthError::new("server_error", format!("starting tx: {e}")))?;
        let row: Option<(String, String, i64, Option<i64>)> = tx
            .query_row(
                "SELECT client_id, scope, expires_at, revoked_at \
                 FROM oauth_refresh_tokens WHERE token_hash = ?1",
                [&hash],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )
            .optional()
            .map_err(|e| OAuthError::new("server_error", format!("reading refresh token: {e}")))?;

        let Some((stored_client, scope, expires_at, revoked_at)) = row else {
            return Err(OAuthError::new("invalid_grant", "unknown refresh token"));
        };
        if revoked_at.is_some() || expires_at <= now || stored_client != client_id {
            return Err(OAuthError::new(
                "invalid_grant",
                "refresh token expired, revoked, or client mismatch",
            ));
        }

        // Rotate refresh token atomically.
        tx.execute(
            "UPDATE oauth_refresh_tokens SET revoked_at = ?1 WHERE token_hash = ?2",
            params![now, &hash],
        )
        .map_err(|e| OAuthError::new("server_error", format!("rotating refresh token: {e}")))?;

        let resp = issue_tokens(&tx, client_id, &scope, now)?;
        tx.commit()
            .map_err(|e| OAuthError::new("server_error", format!("committing refresh tx: {e}")))?;
        Ok(resp)
    }

    fn validate_redirect_uri(&self, uri: &str) -> std::result::Result<(), OAuthError> {
        if uri.is_empty() || uri.len() > MAX_REDIRECT_URI_BYTES {
            return Err(OAuthError::new(
                "invalid_redirect_uri",
                "redirect_uri is empty or too long",
            ));
        }
        if uri.contains('\\') || uri.contains("..") {
            return Err(OAuthError::new(
                "invalid_redirect_uri",
                "redirect_uri must not contain path traversal or backslashes",
            ));
        }
        let parsed = Url::parse(uri).map_err(|_| {
            OAuthError::new("invalid_redirect_uri", "redirect_uri is not a valid URL")
        })?;
        if parsed.fragment().is_some() {
            return Err(OAuthError::new(
                "invalid_redirect_uri",
                "redirect_uri must not contain a fragment",
            ));
        }
        if !parsed.username().is_empty() || parsed.password().is_some() {
            return Err(OAuthError::new(
                "invalid_redirect_uri",
                "redirect_uri must not contain userinfo",
            ));
        }
        let allowed = self
            .settings
            .allowed_redirect_prefixes
            .iter()
            .any(|prefix| {
                let Ok(prefix_url) = Url::parse(prefix) else {
                    return false;
                };
                if parsed.scheme() != prefix_url.scheme()
                    || parsed.host_str() != prefix_url.host_str()
                    || parsed.port_or_known_default() != prefix_url.port_or_known_default()
                {
                    return false;
                }
                let p_path = prefix_url.path();
                let u_path = parsed.path();
                if p_path.ends_with('/') {
                    u_path.starts_with(p_path)
                } else {
                    u_path == p_path
                        || u_path
                            .strip_prefix(p_path)
                            .is_some_and(|rest| rest.starts_with('/'))
                }
            });
        if !allowed {
            return Err(OAuthError::new(
                "invalid_redirect_uri",
                "redirect_uri is not in the server's allowed_redirect_prefixes",
            ));
        }
        Ok(())
    }

    fn verify_client_redirect(
        &self,
        client_id: &str,
        redirect_uri: &str,
    ) -> std::result::Result<String, OAuthError> {
        if let Some(ref static_client) = self.settings.static_client
            && constant_time_str_eq(client_id, &static_client.client_id)
        {
            return Ok("Configured OAuth Client".to_owned());
        }
        let db = self
            .db
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let row: Option<(Option<String>, String)> = db
            .query_row(
                "SELECT client_name, redirect_uris_json FROM oauth_clients WHERE client_id = ?1",
                [client_id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()
            .map_err(|e| OAuthError::new("server_error", format!("reading client: {e}")))?;
        let Some((name, uris_json)) = row else {
            return Err(OAuthError::new("invalid_client", "unknown client_id"));
        };
        let uris: Vec<String> = serde_json::from_str(&uris_json)
            .map_err(|e| OAuthError::new("server_error", format!("parsing client URIs: {e}")))?;
        if !uris.iter().any(|u| u == redirect_uri) {
            return Err(OAuthError::new(
                "invalid_redirect_uri",
                "redirect_uri does not match registered client",
            ));
        }
        Ok(name.unwrap_or_else(|| client_id.to_owned()))
    }

    fn verify_client_credentials(
        &self,
        client_id: &str,
        client_secret: Option<&str>,
    ) -> std::result::Result<(), OAuthError> {
        if let Some(ref static_client) = self.settings.static_client
            && constant_time_str_eq(client_id, &static_client.client_id)
        {
            let Some(secret) = client_secret else {
                return Err(OAuthError::new(
                    "invalid_client",
                    "client_secret is required for this client",
                ));
            };
            if constant_time_str_eq(secret, &static_client.client_secret) {
                return Ok(());
            }
            return Err(OAuthError::new("invalid_client", "invalid client_secret"));
        }

        let db = self
            .db
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let row: Option<(Option<String>, String)> = db
            .query_row(
                "SELECT client_secret_hash, auth_method FROM oauth_clients WHERE client_id = ?1",
                [client_id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()
            .map_err(|e| OAuthError::new("server_error", format!("reading client: {e}")))?;
        let Some((secret_hash, auth_method)) = row else {
            return Err(OAuthError::new("invalid_client", "unknown client_id"));
        };
        if auth_method == "none" {
            return Ok(());
        }
        let Some(expected_hash) = secret_hash else {
            return Err(OAuthError::new("invalid_client", "client secret missing"));
        };
        let Some(secret) = client_secret else {
            return Err(OAuthError::new(
                "invalid_client",
                "client_secret is required for this client",
            ));
        };
        if constant_time_str_eq(&sha256_hex(secret), &expected_hash) {
            Ok(())
        } else {
            Err(OAuthError::new("invalid_client", "invalid client_secret"))
        }
    }

    fn peek_auth_request(
        &self,
        request_id: &str,
        now: i64,
    ) -> std::result::Result<PendingAuthorization, AuthorizeFormError> {
        let db = self
            .db
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let row: Option<(String, String, String, i64)> = db
            .query_row(
                "SELECT client_id, redirect_uri, scope, expires_at \
                 FROM oauth_auth_requests WHERE request_id = ?1",
                [request_id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )
            .optional()
            .map_err(|e| AuthorizeFormError::Invalid(format!("reading auth request: {e}")))?;
        let Some((client_id, redirect_uri, scope, expires_at)) = row else {
            return Err(AuthorizeFormError::Expired);
        };
        if expires_at <= now {
            return Err(AuthorizeFormError::Expired);
        }
        Ok(PendingAuthorization {
            request_id: request_id.to_owned(),
            client_name: client_id.clone(),
            client_id,
            redirect_uri,
            scope,
        })
    }

    fn take_auth_request(
        &self,
        request_id: &str,
        now: i64,
    ) -> std::result::Result<StoredAuthRequest, AuthorizeFormError> {
        let mut db = self
            .db
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let tx = db
            .transaction()
            .map_err(|e| AuthorizeFormError::Invalid(format!("starting tx: {e}")))?;
        let row: Option<StoredAuthRequest> = tx
            .query_row(
                "SELECT client_id, redirect_uri, code_challenge, state, scope, expires_at \
                 FROM oauth_auth_requests WHERE request_id = ?1",
                [request_id],
                |r| {
                    Ok(StoredAuthRequest {
                        client_id: r.get(0)?,
                        redirect_uri: r.get(1)?,
                        code_challenge: r.get(2)?,
                        state: r.get(3)?,
                        scope: r.get(4)?,
                        expires_at: r.get(5)?,
                    })
                },
            )
            .optional()
            .map_err(|e| AuthorizeFormError::Invalid(format!("reading auth request: {e}")))?;
        let Some(req) = row else {
            return Err(AuthorizeFormError::Expired);
        };
        tx.execute(
            "DELETE FROM oauth_auth_requests WHERE request_id = ?1",
            [request_id],
        )
        .map_err(|e| AuthorizeFormError::Invalid(format!("consuming auth request: {e}")))?;
        tx.commit()
            .map_err(|e| AuthorizeFormError::Invalid(format!("committing auth request: {e}")))?;
        if req.expires_at <= now {
            return Err(AuthorizeFormError::Expired);
        }
        Ok(req)
    }

    fn is_rate_limited(&self, client_ip: &str, now: Timestamp) -> bool {
        let mut tracker = self
            .login_failures
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let cutoff = now - TimeDelta::seconds(LOGIN_FAILURE_WINDOW_SECS);
        while tracker.global.front().is_some_and(|t| *t < cutoff) {
            tracker.global.pop_front();
        }
        if tracker.global.len() >= MAX_GLOBAL_LOGIN_FAILURES_PER_WINDOW {
            return true;
        }
        if let Some(entry) = tracker.per_ip.get_mut(client_ip) {
            while entry.front().is_some_and(|t| *t < cutoff) {
                entry.pop_front();
            }
            if entry.is_empty() {
                tracker.per_ip.remove(client_ip);
                return false;
            }
            return entry.len() >= MAX_LOGIN_FAILURES_PER_WINDOW;
        }
        false
    }

    fn record_login_failure(&self, client_ip: &str, now: Timestamp) {
        let mut tracker = self
            .login_failures
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let cutoff = now - TimeDelta::seconds(LOGIN_FAILURE_WINDOW_SECS);
        while tracker.global.front().is_some_and(|t| *t < cutoff) {
            tracker.global.pop_front();
        }
        tracker.global.push_back(now);

        tracker.per_ip.retain(|_, q| {
            while q.front().is_some_and(|t| *t < cutoff) {
                q.pop_front();
            }
            !q.is_empty()
        });
        if !tracker.per_ip.contains_key(client_ip)
            && tracker.per_ip.len() >= MAX_TRACKED_FAILURE_IPS
            && let Some(oldest_key) = tracker
                .per_ip
                .iter()
                .min_by_key(|(_, q)| q.back().copied())
                .map(|(k, _)| k.clone())
        {
            tracker.per_ip.remove(&oldest_key);
        }
        tracker
            .per_ip
            .entry(client_ip.to_owned())
            .or_default()
            .push_back(now);
    }
}

#[derive(Debug)]
struct StoredAuthRequest {
    client_id: String,
    redirect_uri: String,
    code_challenge: String,
    state: Option<String>,
    scope: String,
    expires_at: i64,
}

#[derive(Debug)]
pub enum AuthorizeFormError {
    Expired,
    RateLimited,
    BadPassphrase(PendingAuthorization),
    Invalid(String),
}

fn issue_tokens(
    tx: &rusqlite::Transaction<'_>,
    client_id: &str,
    scope: &str,
    now: i64,
) -> std::result::Result<TokenResponse, OAuthError> {
    let access_token = format!("ab_at_{}", random_token::<32>());
    let refresh_token = format!("ab_rt_{}", random_token::<32>());
    let access_hash = sha256_hex(&access_token);
    let refresh_hash = sha256_hex(&refresh_token);

    tx.execute(
        "INSERT INTO oauth_access_tokens (token_hash, client_id, scope, expires_at) \
         VALUES (?1, ?2, ?3, ?4)",
        params![&access_hash, client_id, scope, now + ACCESS_TOKEN_TTL_SECS],
    )
    .map_err(|e| OAuthError::new("server_error", format!("saving access token: {e}")))?;

    tx.execute(
        "INSERT INTO oauth_refresh_tokens (token_hash, client_id, scope, expires_at) \
         VALUES (?1, ?2, ?3, ?4)",
        params![
            &refresh_hash,
            client_id,
            scope,
            now + REFRESH_TOKEN_TTL_SECS
        ],
    )
    .map_err(|e| OAuthError::new("server_error", format!("saving refresh token: {e}")))?;

    Ok(TokenResponse {
        access_token,
        token_type: "Bearer",
        expires_in: ACCESS_TOKEN_TTL_SECS,
        refresh_token,
        scope: scope.to_owned(),
    })
}

fn build_redirect(base: &str, params: &[(&str, &str)], state: Option<&str>) -> Result<String> {
    let mut url = Url::parse(base).context("parsing redirect_uri")?;
    {
        let mut qp = url.query_pairs_mut();
        for (k, v) in params {
            qp.append_pair(k, v);
        }
        if let Some(state) = state {
            qp.append_pair("state", state);
        }
    }
    Ok(url.into())
}

fn sanitize_scope(scope: Option<&str>) -> std::result::Result<String, OAuthError> {
    let Some(scope) = scope.map(str::trim).filter(|s| !s.is_empty()) else {
        return Ok(DEFAULT_SCOPE.to_owned());
    };
    if scope.len() > MAX_SCOPE_BYTES {
        return Err(OAuthError::new("invalid_scope", "scope is too long"));
    }
    Ok(scope.to_owned())
}

fn is_valid_pkce_challenge(challenge: &str) -> bool {
    (43..=128).contains(&challenge.len())
        && challenge
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_'))
}

pub fn verify_pkce_s256(verifier: &str, expected_challenge: &str) -> bool {
    if !(43..=128).contains(&verifier.len())
        || !verifier
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~'))
    {
        return false;
    }
    let digest = Sha256::digest(verifier.as_bytes());
    let computed = URL_SAFE_NO_PAD.encode(digest);
    constant_time_str_eq(&computed, expected_challenge)
}

fn parse_basic_auth(
    header: Option<&str>,
) -> std::result::Result<(Option<String>, Option<String>), OAuthError> {
    let Some(header) = header else {
        return Ok((None, None));
    };
    let Some(encoded) = header
        .strip_prefix("Basic ")
        .or_else(|| header.strip_prefix("basic "))
    else {
        return Ok((None, None));
    };
    let bytes = STANDARD
        .decode(encoded.trim())
        .map_err(|_| OAuthError::new("invalid_client", "malformed Basic authorization header"))?;
    let decoded = String::from_utf8(bytes)
        .map_err(|_| OAuthError::new("invalid_client", "Basic credentials must be UTF-8"))?;
    let Some((id, secret)) = decoded.split_once(':') else {
        return Err(OAuthError::new(
            "invalid_client",
            "Basic credentials must be client_id:client_secret",
        ));
    };
    let decode_form = |raw: &str| -> String {
        url::form_urlencoded::parse(format!("v={raw}").as_bytes())
            .next()
            .map(|(_, v)| v.into_owned())
            .unwrap_or_else(|| raw.to_owned())
    };
    Ok((Some(decode_form(id)), Some(decode_form(secret))))
}

pub fn sha256_bytes(input: &str) -> [u8; 32] {
    Sha256::digest(input.as_bytes()).into()
}

fn sha256_hex(input: &str) -> String {
    hex(&Sha256::digest(input.as_bytes()))
}

fn random_token<const N: usize>() -> String {
    let mut bytes = [0u8; N];
    rand::fill(&mut bytes);
    URL_SAFE_NO_PAD.encode(bytes)
}

fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        let _ = write!(out, "{b:02x}");
    }
    out
}

fn constant_time_str_eq(a: &str, b: &str) -> bool {
    let ha = Sha256::digest(a.as_bytes());
    let hb = Sha256::digest(b.as_bytes());
    let hashes_eq: bool = ha.ct_eq(&hb).into();
    hashes_eq && a.len() == b.len() && bool::from(a.as_bytes().ct_eq(b.as_bytes()))
}

pub fn render_authorize_html(
    pending: &PendingAuthorization,
    error_message: Option<&str>,
) -> String {
    let client_name = escape_html(&pending.client_name);
    let redirect_host = Url::parse(&pending.redirect_uri)
        .ok()
        .and_then(|u| u.host_str().map(str::to_owned))
        .unwrap_or_else(|| "unknown".to_owned());
    let redirect_host = escape_html(&redirect_host);
    let scope = escape_html(&pending.scope);
    let request_id = escape_html(&pending.request_id);
    let err_html = error_message
        .map(|m| format!(r#"<p class="err">{}</p>"#, escape_html(m)))
        .unwrap_or_default();

    format!(
        r#"<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>Authorize agent-board MCP</title>
<style>
  body {{ font-family: system-ui, -apple-system, sans-serif; max-width: 28rem; margin: 3rem auto; padding: 0 1.25rem; background: #0f1115; color: #e6e8ec; }}
  .card {{ background: #181c24; border: 1px solid #2b3140; border-radius: 12px; padding: 1.5rem; }}
  h1 {{ font-size: 1.25rem; margin: 0 0 0.75rem; }}
  p {{ color: #aeb6c7; line-height: 1.45; font-size: 0.95rem; }}
  code {{ background: #232938; padding: 0.1rem 0.35rem; border-radius: 4px; color: #e6e8ec; }}
  label {{ display: block; margin: 1rem 0 0.35rem; font-weight: 600; font-size: 0.9rem; }}
  input[type=password] {{ width: 100%; box-sizing: border-box; padding: 0.65rem 0.75rem; border-radius: 8px; border: 1px solid #394256; background: #0f1115; color: #e6e8ec; font-size: 1rem; }}
  .actions {{ display: flex; gap: 0.75rem; margin-top: 1.25rem; }}
  button {{ flex: 1; padding: 0.7rem 1rem; border-radius: 8px; border: 0; font-weight: 600; font-size: 0.95rem; cursor: pointer; }}
  .allow {{ background: #4f8cff; color: #0b1020; }}
  .deny {{ background: #2a3142; color: #d5dce8; }}
  .err {{ background: #3c1820; border: 1px solid #8a2c3e; color: #ffb4c2; padding: 0.65rem 0.75rem; border-radius: 8px; }}
</style>
</head>
<body>
  <div class="card">
    <h1>Authorize agent-board MCP</h1>
    <p>Client <code>{client_name}</code> wants access to manage your agent-board fleet (scope <code>{scope}</code>) and will redirect back to <code>{redirect_host}</code>.</p>
    {err_html}
    <form method="post" action="/oauth/authorize">
      <input type="hidden" name="request_id" value="{request_id}">
      <label for="passphrase">Admin passphrase</label>
      <input id="passphrase" name="passphrase" type="password" autocomplete="current-password" required autofocus>
      <div class="actions">
        <button class="deny" type="submit" name="decision" value="deny" formnovalidate>Deny</button>
        <button class="allow" type="submit" name="decision" value="approve">Authorize</button>
      </div>
    </form>
  </div>
</body>
</html>"#
    )
}

fn escape_html(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
}

pub fn validate_public_url(raw: &str) -> Result<Url> {
    let parsed = Url::parse(raw).with_context(|| format!("invalid public_url {raw:?}"))?;
    if !matches!(parsed.scheme(), "https" | "http") || parsed.host_str().is_none() {
        bail!("public_url must be an http(s) URL with a host");
    }
    Ok(parsed)
}
