//! `board mcp-http`: remote Streamable HTTP fleet-ops MCP + OAuth 2.1 gateway.
//!
//! Runs as a separate process/container alongside `board serve`, talking to the core
//! server over HTTP using a revocable `--user` device token (never forwarding
//! `Tailscale-User-Login`). Exposes only OAuth discovery/handshake routes, `/mcp`
//! (Streamable HTTP), and `/healthz`.

pub mod oauth;
pub mod tools;

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use axum::body::Bytes;
use axum::extract::{ConnectInfo, Query, Request, State};
use axum::http::{Extensions, HeaderMap, HeaderValue, StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::{Html, IntoResponse, Redirect, Response};
use axum::routing::{get, post};
use axum::{Form, Json, Router};
use board_proto::{AgentId, DeviceName};
use rmcp::transport::streamable_http_server::session::local::LocalSessionManager;
use rmcp::transport::streamable_http_server::{StreamableHttpServerConfig, StreamableHttpService};
use serde::Deserialize;
use serde_json::json;

use crate::client::Client;
use oauth::{
    AuthorizeFormError, AuthorizeQuery, Clock, OAuthError, OAuthSettings, OAuthStore,
    RegisterClientRequest, StaticClient, TokenRequest, render_authorize_html, sha256_bytes,
    system_clock, validate_public_url,
};
use tools::FleetMcp;

pub const DEFAULT_REDIRECT_PREFIXES: &[&str] = &[
    "https://oauth-redirect.googleusercontent.com/r/",
    "https://oauth-redirect.googleusercontent.com/a/",
];

const DEFAULT_AGENT_ID: &str = "antigravity:mcp-http";
const DEFAULT_DEVICE_NAME: &str = "mcp-http";

/// Configuration loaded from `--config /etc/agent-board/mcp-http.toml` plus secret env vars.
#[derive(Debug, Clone, Deserialize)]
pub struct McpHttpConfig {
    /// Address this gateway binds to (e.g. `127.0.0.1:7378`).
    pub listen: String,
    /// Public HTTPS origin of this gateway (e.g. `https://board-mcp.example.com`).
    pub public_url: String,
    /// Upstream core board server URL (e.g. `http://127.0.0.1:7377`).
    pub board_server: String,
    /// Path to the OAuth SQLite database (e.g. `/var/lib/agent-board/mcp-oauth.db`).
    pub oauth_db: PathBuf,
    /// Device name recorded when registering the gateway's agent id.
    #[serde(default = "default_device_name")]
    pub device: String,
    /// Agent id used for agent-stamped board writes (tasks, notes, ranking).
    #[serde(default = "default_agent_id")]
    pub agent_id: String,
    /// Allowed `redirect_uri` prefixes for DCR and OAuth authorization.
    #[serde(default = "default_redirect_prefixes")]
    pub allowed_redirect_prefixes: Vec<String>,
    /// Board `--user` bearer token (prefer `BOARD_MCP_TOKEN` env var over config file).
    #[serde(default)]
    pub board_token: Option<String>,
    /// Single-user login passphrase (prefer `BOARD_MCP_PASSPHRASE` env var over config file).
    #[serde(default)]
    pub passphrase: Option<String>,
    /// Optional pre-configured static OAuth client id (`BOARD_MCP_STATIC_CLIENT_ID`).
    #[serde(default)]
    pub static_client_id: Option<String>,
    /// Optional pre-configured static OAuth client secret (`BOARD_MCP_STATIC_CLIENT_SECRET`).
    #[serde(default)]
    pub static_client_secret: Option<String>,
}

fn default_device_name() -> String {
    DEFAULT_DEVICE_NAME.to_owned()
}

fn default_agent_id() -> String {
    DEFAULT_AGENT_ID.to_owned()
}

fn default_redirect_prefixes() -> Vec<String> {
    DEFAULT_REDIRECT_PREFIXES
        .iter()
        .map(|s| (*s).to_owned())
        .collect()
}

#[derive(Clone)]
struct GatewayState {
    oauth: OAuthStore,
}

/// Starts the remote MCP + OAuth 2.1 HTTP gateway until `SIGINT`/`SIGTERM`.
pub async fn serve(config_path: &Path) -> Result<()> {
    let raw = std::fs::read_to_string(config_path)
        .with_context(|| format!("reading {}", config_path.display()))?;
    let mut cfg: McpHttpConfig =
        toml::from_str(&raw).with_context(|| format!("parsing {}", config_path.display()))?;

    if let Ok(token) = std::env::var("BOARD_MCP_TOKEN")
        && !token.trim().is_empty()
    {
        cfg.board_token = Some(token.trim().to_owned());
    }
    if let Ok(pass) = std::env::var("BOARD_MCP_PASSPHRASE")
        && !pass.is_empty()
    {
        cfg.passphrase = Some(pass);
    }
    if let Ok(cid) = std::env::var("BOARD_MCP_STATIC_CLIENT_ID")
        && !cid.trim().is_empty()
    {
        cfg.static_client_id = Some(cid.trim().to_owned());
    }
    if let Ok(csec) = std::env::var("BOARD_MCP_STATIC_CLIENT_SECRET")
        && !csec.is_empty()
    {
        cfg.static_client_secret = Some(csec);
    }

    let listen_addr: SocketAddr = cfg
        .listen
        .parse()
        .with_context(|| format!("invalid listen address {:?}", cfg.listen))?;

    let (router, _oauth) = build_router(&cfg, system_clock())?;

    let listener = tokio::net::TcpListener::bind(listen_addr)
        .await
        .with_context(|| format!("binding {listen_addr}"))?;
    tracing::info!(
        listen = %listen_addr,
        public_url = %cfg.public_url,
        "agent-board remote MCP gateway listening"
    );
    axum::serve(
        listener,
        router.into_make_service_with_connect_info::<SocketAddr>(),
    )
    .with_graceful_shutdown(shutdown_signal())
    .await
    .context("running remote MCP HTTP server")?;
    Ok(())
}

/// Builds the full Axum router (also used by integration tests).
pub fn build_router(cfg: &McpHttpConfig, clock: Clock) -> Result<(Router, OAuthStore)> {
    let public = validate_public_url(&cfg.public_url)?;
    let public_url = cfg.public_url.trim_end_matches('/').to_owned();

    let board_token = cfg
        .board_token
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .context("board_token or BOARD_MCP_TOKEN is required")?
        .to_owned();

    let passphrase = cfg
        .passphrase
        .as_deref()
        .filter(|s| s.len() >= 12)
        .context("passphrase or BOARD_MCP_PASSPHRASE is required (at least 12 characters)")?;

    if cfg.allowed_redirect_prefixes.is_empty() {
        bail!("allowed_redirect_prefixes must not be empty");
    }

    let static_client = match (&cfg.static_client_id, &cfg.static_client_secret) {
        (Some(id), Some(secret)) if !id.trim().is_empty() && !secret.is_empty() => {
            Some(StaticClient {
                client_id: id.trim().to_owned(),
                client_secret: secret.clone(),
            })
        }
        (None, None) => None,
        _ => bail!("static_client_id and static_client_secret must both be set or both omitted"),
    };

    let agent_id: AgentId = cfg
        .agent_id
        .parse()
        .with_context(|| format!("invalid agent_id {:?}", cfg.agent_id))?;
    let device_name: DeviceName = cfg
        .device
        .parse()
        .with_context(|| format!("invalid device {:?}", cfg.device))?;

    let user_client = Client::new(
        &cfg.board_server,
        board_token,
        None,
        Some(Duration::from_secs(30)),
    )?
    .with_read_retries(crate::client::READ_RETRY_DELAYS);

    let oauth = OAuthStore::open(
        &cfg.oauth_db,
        OAuthSettings {
            public_url: public_url.clone(),
            passphrase_sha256: sha256_bytes(passphrase),
            allowed_redirect_prefixes: cfg.allowed_redirect_prefixes.clone(),
            static_client,
        },
        clock,
    )?;

    let fleet = FleetMcp::new(user_client, agent_id, device_name);

    let mut allowed_hosts = vec![
        "localhost".to_owned(),
        "127.0.0.1".to_owned(),
        "::1".to_owned(),
    ];
    if let Some(host) = public.host_str() {
        allowed_hosts.push(host.to_owned());
        if let Some(port) = public.port() {
            allowed_hosts.push(format!("{host}:{port}"));
        }
    }

    let mut http_cfg = StreamableHttpServerConfig::default().with_allowed_hosts(allowed_hosts);
    http_cfg.legacy_session_mode = false;
    http_cfg.json_response = true;
    http_cfg.max_request_body_bytes = 256 * 1024;

    let mcp_service = StreamableHttpService::new(
        move || Ok(fleet.clone()),
        Arc::new(LocalSessionManager::default()),
        http_cfg,
    );

    let state = GatewayState {
        oauth: oauth.clone(),
    };

    let mcp_router =
        Router::new()
            .nest_service("/mcp", mcp_service)
            .layer(middleware::from_fn_with_state(
                state.clone(),
                require_oauth_bearer,
            ));

    let router = Router::new()
        .route("/healthz", get(healthz))
        .route(
            "/.well-known/oauth-protected-resource",
            get(protected_resource_metadata),
        )
        .route(
            "/.well-known/oauth-protected-resource/mcp",
            get(protected_resource_metadata),
        )
        .route(
            "/.well-known/oauth-authorization-server",
            get(authorization_server_metadata),
        )
        .route("/oauth/register", post(register_client))
        .route("/oauth/authorize", get(authorize_get).post(authorize_post))
        .route("/oauth/token", post(token_post))
        .merge(mcp_router)
        .with_state(state);

    Ok((router, oauth))
}

async fn healthz() -> impl IntoResponse {
    Json(json!({
        "ok": true,
        "service": "agent-board-mcp-http",
        "version": env!("CARGO_PKG_VERSION"),
        "commit": crate::COMMIT,
    }))
}

async fn protected_resource_metadata(State(state): State<GatewayState>) -> impl IntoResponse {
    Json(state.oauth.protected_resource_metadata())
}

async fn authorization_server_metadata(State(state): State<GatewayState>) -> impl IntoResponse {
    Json(state.oauth.authorization_server_metadata())
}

async fn register_client(
    State(state): State<GatewayState>,
    Json(req): Json<RegisterClientRequest>,
) -> Response {
    match state.oauth.register_client(req) {
        Ok(resp) => (StatusCode::CREATED, Json(resp)).into_response(),
        Err(err) => (StatusCode::BAD_REQUEST, Json(err)).into_response(),
    }
}

async fn authorize_get(
    State(state): State<GatewayState>,
    Query(query): Query<AuthorizeQuery>,
) -> Response {
    match state.oauth.begin_authorize(&query) {
        Ok(pending) => html_response(StatusCode::OK, render_authorize_html(&pending, None)),
        Err(err) => (StatusCode::BAD_REQUEST, Json(err)).into_response(),
    }
}

#[derive(Deserialize)]
struct AuthorizeForm {
    request_id: String,
    #[serde(default)]
    passphrase: String,
    #[serde(default)]
    decision: String,
}

async fn authorize_post(
    State(state): State<GatewayState>,
    headers: HeaderMap,
    extensions: Extensions,
    Form(form): Form<AuthorizeForm>,
) -> Response {
    let connect = extensions.get::<ConnectInfo<SocketAddr>>().copied();
    let client_ip = extract_client_ip(&headers, connect);
    let approve = form.decision == "approve";
    match state
        .oauth
        .complete_authorize(&form.request_id, &form.passphrase, approve, &client_ip)
    {
        Ok(redirect_url) => Redirect::to(&redirect_url).into_response(),
        Err(AuthorizeFormError::BadPassphrase(pending)) => html_response(
            StatusCode::UNAUTHORIZED,
            render_authorize_html(&pending, Some("Invalid passphrase. Please try again.")),
        ),
        Err(AuthorizeFormError::RateLimited) => (
            StatusCode::TOO_MANY_REQUESTS,
            Json(OAuthError::new(
                "temporarily_unavailable",
                "too many failed login attempts; try again later",
            )),
        )
            .into_response(),
        Err(AuthorizeFormError::Expired) => (
            StatusCode::BAD_REQUEST,
            Json(OAuthError::new(
                "invalid_request",
                "authorization request expired; start again from your client",
            )),
        )
            .into_response(),
        Err(AuthorizeFormError::Invalid(msg)) => (
            StatusCode::BAD_REQUEST,
            Json(OAuthError::new("invalid_request", msg)),
        )
            .into_response(),
    }
}

async fn token_post(
    State(state): State<GatewayState>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let content_type = headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    let req: TokenRequest = if content_type.starts_with("application/json") {
        match serde_json::from_slice(&body) {
            Ok(req) => req,
            Err(err) => {
                return (
                    StatusCode::BAD_REQUEST,
                    Json(OAuthError::new(
                        "invalid_request",
                        format!("bad JSON: {err}"),
                    )),
                )
                    .into_response();
            }
        }
    } else {
        match parse_form_urlencoded(&body) {
            Ok(req) => req,
            Err(err) => {
                return (
                    StatusCode::BAD_REQUEST,
                    Json(OAuthError::new(
                        "invalid_request",
                        format!("bad form: {err}"),
                    )),
                )
                    .into_response();
            }
        }
    };

    let basic = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok());

    match state.oauth.exchange_token(&req, basic) {
        Ok(resp) => {
            let mut r = (StatusCode::OK, Json(resp)).into_response();
            r.headers_mut()
                .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
            r.headers_mut()
                .insert(header::PRAGMA, HeaderValue::from_static("no-cache"));
            r
        }
        Err(err) => {
            let status = if err.error == "invalid_client" {
                StatusCode::UNAUTHORIZED
            } else {
                StatusCode::BAD_REQUEST
            };
            (status, Json(err)).into_response()
        }
    }
}

async fn require_oauth_bearer(
    State(state): State<GatewayState>,
    request: Request,
    next: Next,
) -> Response {
    let token = request
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| {
            v.strip_prefix("Bearer ")
                .or_else(|| v.strip_prefix("bearer "))
        })
        .map(str::trim);

    if let Some(token) = token
        && state.oauth.verify_access_token(token)
    {
        return next.run(request).await;
    }

    let mut resp = (
        StatusCode::UNAUTHORIZED,
        Json(OAuthError::new(
            "invalid_token",
            "valid OAuth bearer token is required",
        )),
    )
        .into_response();
    if let Ok(val) = HeaderValue::from_str(&state.oauth.www_authenticate_header()) {
        resp.headers_mut().insert(header::WWW_AUTHENTICATE, val);
    }
    resp
}

fn parse_form_urlencoded(body: &[u8]) -> Result<TokenRequest> {
    let mut grant_type = None;
    let mut code = None;
    let mut redirect_uri = None;
    let mut client_id = None;
    let mut client_secret = None;
    let mut code_verifier = None;
    let mut refresh_token = None;

    for (k, v) in url::form_urlencoded::parse(body) {
        let v = v.into_owned();
        match k.as_ref() {
            "grant_type" => grant_type = Some(v),
            "code" => code = Some(v),
            "redirect_uri" => redirect_uri = Some(v),
            "client_id" => client_id = Some(v),
            "client_secret" => client_secret = Some(v),
            "code_verifier" => code_verifier = Some(v),
            "refresh_token" => refresh_token = Some(v),
            _ => {}
        }
    }

    Ok(TokenRequest {
        grant_type: grant_type.context("missing grant_type")?,
        code,
        redirect_uri,
        client_id,
        client_secret,
        code_verifier,
        refresh_token,
    })
}

fn extract_client_ip(headers: &HeaderMap, connect: Option<ConnectInfo<SocketAddr>>) -> String {
    let peer = connect.map(|c| c.0.ip());
    // Only honor proxy-supplied CF-Connecting-IP when the direct peer is loopback
    // (a trusted local reverse proxy / tunnel connector on the same host).
    if peer.is_some_and(|ip| ip.is_loopback())
        && let Some(cf) = headers
            .get("cf-connecting-ip")
            .and_then(|v| v.to_str().ok())
        && !cf.trim().is_empty()
        && cf.trim().len() <= 64
    {
        return cf.trim().to_owned();
    }
    peer.map(|ip| ip.to_string())
        .unwrap_or_else(|| "unknown".to_owned())
}

fn html_response(status: StatusCode, body: String) -> Response {
    let mut resp = (status, Html(body)).into_response();
    let h = resp.headers_mut();
    h.insert(
        header::CONTENT_SECURITY_POLICY,
        HeaderValue::from_static(
            "default-src 'none'; style-src 'unsafe-inline'; form-action 'self'; frame-ancestors 'none'; base-uri 'none'",
        ),
    );
    h.insert(header::X_FRAME_OPTIONS, HeaderValue::from_static("DENY"));
    h.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    h.insert(
        header::REFERRER_POLICY,
        HeaderValue::from_static("no-referrer"),
    );
    h.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    resp
}

async fn shutdown_signal() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };
    #[cfg(unix)]
    let terminate = async {
        if let Ok(mut sig) =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        {
            sig.recv().await;
        }
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();
    tokio::select! {
        () = ctrl_c => {},
        () = terminate => {},
    }
}
