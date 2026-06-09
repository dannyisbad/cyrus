//! HTTP server: routing, auth gating, CORS, tunnel detection, body limits.
//!
//! Source: repo-agent-mcp/src/index.ts (private original)
//!
//! axum router (mirrors `runHttp()` in index.ts):
//!   GET  /                      -> server info JSON
//!   GET  /events                -> SSE tool-event tail (loopback-only; seq cursor,
//!                                  ?agent= scope, Last-Event-ID / ?since= resume)
//!   GET  /snapshot              -> state.snapshot() JSON (loopback-only)
//!   *    /control/*             -> loopback-only + bearer-gated control plane
//!                                  (arm-consent, subagents, sessions, bind,
//!                                   subagent/result, subagent/update)
//!   GET  /.well-known/*         -> OAuth/OIDC metadata (oauth.rs)
//!   *    /oauth/{authorize,token,userinfo,jwks}
//!   POST|GET|DELETE /mcp        -> MCP Streamable-HTTP transport (mcp.rs)
//!
//! Hazards (all reproduced below, byte-for-byte where it matters):
//!   - `via_tunnel()` keys off Cloudflare headers (cf-connecting-ip / cf-ray /
//!     cdn-loop). Loopback surfaces (`/events`, `/snapshot`, `/control/*`) must
//!     **404** (not 401) through the tunnel so their existence isn't leaked.
//!   - Fail-closed rule: serving `/mcp` over a tunnel with NO bearer configured
//!     must **403**, never fall through to the open `auth_ok` path (which is open
//!     when no token + no oauth are configured).
//!   - The TS attributes events to subagents via the `x-openai-session` header,
//!     propagated through AsyncLocalStorage. There is no implicit async context in
//!     Rust, so the session id is read synchronously off the request and passed
//!     down explicitly (also stashed in a `tokio::task_local!` for the duration of
//!     the MCP dispatch so the state layer can read it the way `currentSession()`
//!     does in the TS).
//!
//! Note on module boundaries: this port owns ONLY `http.rs`. The sibling modules
//! (`config`, `state`, `oauth`, `mcp`, `subagent`) are still skeleton stubs, so
//! the behavioral contract this router needs from them is expressed here as small
//! traits (`StateAccess`, `OAuthProvider`) plus a config view (`HttpConfig`). When
//! those modules are filled in they implement these traits (or the wiring is
//! reconciled), and the routing/auth/tunnel semantics in this file stay fixed.

use std::collections::BTreeMap;
use std::convert::Infallible;
use std::sync::Arc;
use std::time::Duration;

use axum::body::Body;
use axum::extract::{ConnectInfo, RawQuery, State};
use axum::http::{header, HeaderMap, HeaderValue, Method, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use axum::routing::{any, options};
use axum::Router;
use serde::Serialize;
use serde_json::{json, Value};
use std::net::SocketAddr;
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;

use crate::config::Config;

const MCP_PATH: &str = "/mcp";

// ---------------------------------------------------------------------------
// Behavioral contracts the router needs from the (still-stub) sibling modules.
//
// These mirror the methods index.ts calls on `config`, `state`, and `oauth`.
// They are object-safe so the app can hold them as `Arc<dyn ...>` and the real
// `RepoState` / `OAuth` implement them once those modules are ported.
// ---------------------------------------------------------------------------

/// Config fields the HTTP layer reads. Mirrors the subset of `RepoAgentConfig`
/// (types.ts) touched by `runHttp()`. Carried as a plain view so this module
/// compiles against the current placeholder `Config` without depending on its
/// (not-yet-ported) field layout.
#[derive(Debug, Clone, Default)]
pub struct HttpConfig {
    pub root: String,
    pub host: String,
    pub port: u16,
    pub spice_level: String,
    pub sandbox_mode: String,
    pub approval_policy: String,
    pub current_project: Option<String>,
    pub bearer_token: Option<String>,
    pub public_url: Option<String>,
    /// JWT signing key (JWT_SIGNING_KEY env or config.jwtSigningKey), distinct
    /// from `bearer_token` when provisioned; falls back to it in `oauth.rs`.
    pub jwt_signing_key: Option<String>,
}

impl HttpConfig {
    /// Build the HTTP-relevant view from the crate `Config` (the real
    /// `RepoAgentConfig` from config.rs).
    ///
    /// - `port` is a JS `number` (f64) in the config; the `as u16` cast is
    ///   saturating (negative -> 0, > 65535 -> 65535) and NaN -> 0, which is the
    ///   closest total mapping of what Node's `listen(port)` would accept.
    /// - `bearer_token` / `public_url` apply JS `||` truthiness: an empty string
    ///   behaves exactly like an unset value everywhere the TS reads them
    ///   (`!config.bearerToken`, the oauth construction guard), so they are
    ///   normalized to `None` here.
    /// - `jwt_signing_key` mirrors `process.env.JWT_SIGNING_KEY ||
    ///   config.jwtSigningKey || undefined` from runHttp. The config half is an
    ///   untyped key the TS picks up via the `...fromFile` spread; the ported
    ///   struct doesn't carry it, so it is re-read from the same config file
    ///   `loadConfig()` used (see `wire::config_file_jwt_signing_key`).
    pub fn from_config(cfg: &Config) -> Self {
        fn enum_str<T: serde::Serialize>(v: &T) -> String {
            serde_json::to_value(v)
                .ok()
                .and_then(|j| j.as_str().map(str::to_string))
                .unwrap_or_default()
        }
        let jwt_signing_key = std::env::var("JWT_SIGNING_KEY")
            .ok()
            .filter(|s| !s.is_empty())
            .or_else(|| crate::wire::config_file_jwt_signing_key(cfg));
        HttpConfig {
            root: cfg.root.clone(),
            host: cfg.host.clone(),
            port: cfg.port as u16,
            spice_level: enum_str(&cfg.spice_level),
            sandbox_mode: enum_str(&cfg.sandbox_mode),
            approval_policy: enum_str(&cfg.approval_policy),
            current_project: cfg.current_project.clone(),
            bearer_token: cfg.bearer_token.clone().filter(|s| !s.is_empty()),
            public_url: cfg.public_url.clone().filter(|s| !s.is_empty()),
            jwt_signing_key,
        }
    }
}

/// What the read-only + control-plane routes need from `RepoState`
/// (state/store.ts). All methods are synchronous in the TS (in-memory reads /
/// queued writes); they stay sync here and the impl serializes I/O internally.
pub trait StateAccess: Send + Sync + 'static {
    /// `recentEventsSince(sinceSeq, agent)` — events with `seq > since`, optionally
    /// scoped to one agent (treating a missing agent as `"main"`). Returns the
    /// already-serialized event objects so the SSE writer can emit them verbatim.
    fn recent_events_since(&self, since_seq: i64, agent: Option<&str>) -> Vec<ToolEventLite>;
    /// `snapshot()` — the loopback `/snapshot` JSON.
    fn snapshot(&self) -> Value;

    // ----- control plane -----
    /// `subagents.listJobs()`
    fn list_jobs(&self) -> Value;
    /// `unboundSessions()`
    fn unbound_sessions(&self) -> Value;
    /// `boundAgents()`
    fn bound_agents(&self) -> Value;
    /// `subagents.setResult(agentId, capsule)` + `releaseLeasesForAgent(agentId)`.
    fn set_subagent_result(&self, agent_id: &str, capsule: Value);
    /// `subagents.updateJob(agentId, patch)` — returns whether a job matched.
    fn update_subagent_job(&self, agent_id: &str, patch: Value) -> bool;
    /// `restampAgent(session, agentId)`
    fn restamp_agent(&self, session: &str, agent_id: &str);
}

/// A single tool event as emitted on the `/events` SSE stream.
///
/// The inner `Value` is the full `ToolEvent` object exactly as the TS would
/// `JSON.stringify(evt)` it — `seq` lives **inside** it (the TS event carries its
/// own `.seq`). [`ToolEventLite::seq`] reads that out for the cursor and the
/// `id:` line so the payload is never duplicated.
#[derive(Debug, Clone, Serialize)]
#[serde(transparent)]
pub struct ToolEventLite(pub Value);

impl ToolEventLite {
    /// `evt.seq` (the cursor). `None` mirrors `evt.seq ?? ""` (empty `id:` line).
    pub fn seq(&self) -> Option<i64> {
        self.0.get("seq").and_then(Value::as_i64)
    }
}

/// What the OAuth/OIDC routes delegate to (`createOAuthHandlers` in oauth.ts).
/// Each method returns a fully-formed HTTP response (the TS handlers write the
/// status/headers/body themselves); the router just maps the path and the parsed
/// request to the right one and forwards the response.
pub trait OAuthProvider: Send + Sync + 'static {
    /// `validateToken` — HS256 verify + `scope == "mcp"`. Used by `auth_ok`.
    fn validate_token(&self, token: &str) -> bool;
    /// `armConsent(nonce, ttlSec)` — store a one-time short-TTL nonce (loopback).
    fn arm_consent(&self, nonce: &str, ttl_sec: u64);

    fn protected_resource_metadata(&self, req: &OAuthRequest) -> Response;
    fn authorization_server_metadata(&self, req: &OAuthRequest) -> Response;
    fn openid_configuration(&self, req: &OAuthRequest) -> Response;
    fn user_info(&self, req: &OAuthRequest) -> Response;
    fn jwks(&self, req: &OAuthRequest) -> Response;
    fn authorize(&self, req: &OAuthRequest) -> Response;
    fn token(&self, req: &OAuthRequest) -> Response;
}

/// What the MCP transport (`mcp.rs`) needs per request. One transport per request
/// (stateless: `sessionIdGenerator: undefined`, `enableJsonResponse: false`). The
/// `session` is the attributed `x-openai-session` (or `None`).
pub trait McpDispatch: Send + Sync + 'static {
    fn handle_request(&self, req: McpRequest) -> Response;
}

/// Parsed request handed to the OAuth handlers. Carries everything the TS handlers
/// read off `IncomingMessage` (method, full URL incl. query, headers, body).
#[derive(Debug, Clone)]
pub struct OAuthRequest {
    pub method: Method,
    /// Full path + query, e.g. `/oauth/authorize?response_type=code&...`.
    pub uri: Uri,
    pub host: String,
    pub headers: HeaderMap,
    pub body: Vec<u8>,
    /// Source IP (rate-limit keying in oauth.ts uses the connection peer).
    pub peer_ip: String,
}

/// Parsed request handed to the MCP transport.
#[derive(Debug, Clone)]
pub struct McpRequest {
    pub method: Method,
    pub headers: HeaderMap,
    pub body: Vec<u8>,
    /// Attributed `x-openai-session` (the explicit pass-down replacing ALS).
    pub session: Option<String>,
}

tokio::task_local! {
    /// Task-local replacement for the TS `requestContext` AsyncLocalStorage. Set
    /// for the duration of an MCP dispatch so the state layer's `event()` can read
    /// the owning session synchronously (mirrors `currentSession()`), in addition
    /// to the explicit `McpRequest.session` pass-down.
    pub static CURRENT_SESSION: Option<String>;
}

/// Read the current request's attributed session, if the dispatch set it.
/// Mirrors `currentSession()` in core/context.ts.
pub fn current_session() -> Option<String> {
    CURRENT_SESSION.try_with(|s| s.clone()).ok().flatten()
}

/// Shared application state threaded through every handler (the axum analogue of
/// the closures captured by `createServer`'s callback in index.ts).
#[derive(Clone)]
pub struct AppState {
    pub config: Arc<HttpConfig>,
    pub state: Arc<dyn StateAccess>,
    /// `Some` only when `publicUrl && bearerToken` (mirrors the TS `oauth` guard).
    pub oauth: Option<Arc<dyn OAuthProvider>>,
    pub mcp: Arc<dyn McpDispatch>,
}

// ---------------------------------------------------------------------------
// Header / auth / tunnel helpers (1:1 with the TS free functions).
// ---------------------------------------------------------------------------

fn header_str<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    headers.get(name).and_then(|v| v.to_str().ok())
}

/// `authOk(req, token, oauthValidate)` — exact port.
///
/// - When neither a bearer token nor an oauth validator is configured, auth is
///   **open** (returns true). This is the path the `/mcp` fail-closed 403 guards.
/// - Otherwise require `Authorization: Bearer <t>` and accept it if it equals the
///   configured token OR the oauth validator accepts it.
fn auth_ok(headers: &HeaderMap, token: Option<&str>, oauth: Option<&Arc<dyn OAuthProvider>>) -> bool {
    if token.is_none() && oauth.is_none() {
        return true;
    }
    let header = header_str(headers, "authorization").unwrap_or("");
    let bearer = header.strip_prefix("Bearer ").unwrap_or("");
    if bearer.is_empty() {
        return false;
    }
    if let Some(t) = token {
        if bearer == t {
            return true;
        }
    }
    if let Some(o) = oauth {
        if o.validate_token(bearer) {
            return true;
        }
    }
    false
}

/// `viaTunnel(req)` — requests through the cloudflared tunnel always carry a CF
/// header; a direct loopback request (the local shadow) carries none. Used to keep
/// the read-only `/events` + `/snapshot` streams and the `/control/*` plane open
/// locally but invisible publicly.
fn via_tunnel(headers: &HeaderMap) -> bool {
    headers.contains_key("cf-connecting-ip")
        || headers.contains_key("cf-ray")
        || headers.contains_key("cdn-loop")
}

/// CORS headers applied to every response (`cors(res)` in the TS).
fn apply_cors(headers: &mut HeaderMap) {
    headers.insert(
        header::ACCESS_CONTROL_ALLOW_ORIGIN,
        HeaderValue::from_static("*"),
    );
    headers.insert(
        header::ACCESS_CONTROL_ALLOW_METHODS,
        HeaderValue::from_static("POST, GET, DELETE, OPTIONS"),
    );
    headers.insert(
        header::ACCESS_CONTROL_ALLOW_HEADERS,
        HeaderValue::from_static("content-type, mcp-session-id, authorization"),
    );
    headers.insert(
        header::ACCESS_CONTROL_EXPOSE_HEADERS,
        HeaderValue::from_static("Mcp-Session-Id"),
    );
}

/// `sendJson(res, status, body)`.
fn send_json(status: StatusCode, body: Value) -> Response {
    let mut resp = (
        status,
        [(header::CONTENT_TYPE, "application/json")],
        serde_json::to_string(&body).unwrap_or_else(|_| "null".into()),
    )
        .into_response();
    apply_cors(resp.headers_mut());
    resp
}

/// Pretty-printed JSON (the TS uses `JSON.stringify(obj, null, 2)` for `/`,
/// `/snapshot`).
fn send_json_pretty(status: StatusCode, body: &Value) -> Response {
    let mut resp = (
        status,
        [(header::CONTENT_TYPE, "application/json")],
        serde_json::to_string_pretty(body).unwrap_or_else(|_| "null".into()),
    )
        .into_response();
    apply_cors(resp.headers_mut());
    resp
}

/// Plain-text response with CORS (`res.writeHead(code).end(text)`).
fn text(status: StatusCode, body: &'static str) -> Response {
    let mut resp = (status, body).into_response();
    apply_cors(resp.headers_mut());
    resp
}

fn text_owned(status: StatusCode, body: String) -> Response {
    let mut resp = (status, body).into_response();
    apply_cors(resp.headers_mut());
    resp
}

/// Parse a request body into a JSON object, mirroring `readJsonBody`: empty body
/// becomes `{}`, parse failure / over-limit becomes `None` (`undefined`).
fn read_json_body(body: &[u8], limit_bytes: usize) -> Option<Value> {
    if body.len() > limit_bytes {
        return None;
    }
    let raw = if body.is_empty() { "{}" } else { std::str::from_utf8(body).ok()? };
    serde_json::from_str::<Value>(raw).ok()
}

/// `Content-Length > limit` => 413 (`handleBodyLimit`). Returns `Some(response)`
/// when the limit is exceeded.
fn body_limit_response(headers: &HeaderMap, body_len: usize, limit_bytes: usize) -> Option<Response> {
    // The TS keys off the declared content-length header; fall back to the actual
    // length when absent so the guard still holds for chunked/unsized bodies.
    let len = header_str(headers, "content-length")
        .and_then(|v| v.parse::<usize>().ok())
        .unwrap_or(body_len);
    if len > limit_bytes {
        return Some(text_owned(
            StatusCode::PAYLOAD_TOO_LARGE,
            format!("Request too large: {len} > {limit_bytes}"),
        ));
    }
    None
}

/// Parse `?a=b&c=d` into a small map (last value wins, matching `URLSearchParams`
/// single-value `.get`).
fn parse_query(raw: Option<&str>) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    if let Some(q) = raw {
        for pair in q.split('&') {
            if pair.is_empty() {
                continue;
            }
            let mut it = pair.splitn(2, '=');
            let k = it.next().unwrap_or("");
            let v = it.next().unwrap_or("");
            out.insert(url_decode(k), url_decode(v));
        }
    }
    out
}

/// Minimal `application/x-www-form-urlencoded` decode (`%XX` + `+` -> space).
fn url_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'%' if i + 2 < bytes.len() => {
                let h = hex_val(bytes[i + 1]);
                let l = hex_val(bytes[i + 2]);
                if let (Some(h), Some(l)) = (h, l) {
                    out.push((h << 4) | l);
                    i += 3;
                    continue;
                }
                out.push(b'%');
                i += 1;
            }
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn hex_val(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

fn host_of(headers: &HeaderMap) -> String {
    header_str(headers, "host").unwrap_or("localhost").to_string()
}

// ---------------------------------------------------------------------------
// Router construction.
// ---------------------------------------------------------------------------

/// Build the axum router. A single fallback handler dispatches by `(method, path)`
/// to preserve the TS's exact ordering and 404-vs-401-vs-403 semantics, which a
/// route table alone can't express (e.g. a loopback path must 404 — not 405 — for
/// the wrong method through the tunnel, and `/control/*` is a prefix dispatch).
pub fn build_router(app: AppState) -> Router {
    Router::new()
        // OPTIONS short-circuits everything else (the TS handles it first, before
        // any path match) — but a 204 with CORS is the same for every path, so a
        // single global OPTIONS route is faithful. Non-OPTIONS methods on "/"
        // must still flow to the dispatcher (a bare method route would otherwise
        // answer them with axum's default 405), so the method router's own
        // fallback is the dispatcher too.
        .route("/", options(preflight).fallback(dispatch))
        .fallback(any(dispatch))
        .with_state(app)
}

async fn preflight() -> Response {
    let mut resp = StatusCode::NO_CONTENT.into_response();
    apply_cors(resp.headers_mut());
    resp
}

/// The single dispatcher mirroring the `createServer` callback's if-chain.
async fn dispatch(
    State(app): State<AppState>,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    RawQuery(raw_query): RawQuery,
    connect_info: Option<ConnectInfo<SocketAddr>>,
    body: Body,
) -> Response {
    let path = uri.path().to_string();
    let query = parse_query(raw_query.as_deref());

    // OPTIONS -> 204 (covered for non-"/" paths here too).
    if method == Method::OPTIONS {
        let mut resp = StatusCode::NO_CONTENT.into_response();
        apply_cors(resp.headers_mut());
        return resp;
    }

    let tunneled = via_tunnel(&headers);
    let cfg = &app.config;

    // --- GET / : server info -------------------------------------------------
    if method == Method::GET && path == "/" {
        let info = json!({
            "name": "repo-agent-mcp",
            "ok": true,
            "mcp": MCP_PATH,
            "root": cfg.root,
            "spiceLevel": cfg.spice_level,
            "sandboxMode": cfg.sandbox_mode,
            "approvalPolicy": cfg.approval_policy,
            "currentProject": cfg.current_project,
            "workbench": "Ask ChatGPT to call repo_ui",
            "slash": ["/permissions", "/project", "/status", "/diff", "/logs", "/compact"],
            "oauth": if app.oauth.is_some() { "enabled" } else { "disabled" },
        });
        return send_json_pretty(StatusCode::OK, &info);
    }

    // --- GET /events : loopback-only SSE tail --------------------------------
    if method == Method::GET && path == "/events" {
        if tunneled {
            return text(StatusCode::NOT_FOUND, "Not Found");
        }
        return events_stream(app.clone(), &headers, &query);
    }

    // --- GET /snapshot : loopback-only state JSON ----------------------------
    if method == Method::GET && path == "/snapshot" {
        if tunneled {
            return text(StatusCode::NOT_FOUND, "Not Found");
        }
        return send_json_pretty(StatusCode::OK, &app.state.snapshot());
    }

    // --- /control/* : loopback-only + bearer-gated control plane -------------
    if path.starts_with("/control/") {
        if tunneled {
            return text(StatusCode::NOT_FOUND, "Not Found");
        }
        if !auth_ok(&headers, cfg.bearer_token.as_deref(), app.oauth.as_ref()) {
            return text(StatusCode::UNAUTHORIZED, "Unauthorized");
        }
        let body_bytes = collect_body(body).await;
        return handle_control(&app, &method, &path, &body_bytes);
    }

    // --- OAuth / OIDC --------------------------------------------------------
    if let Some(oauth) = app.oauth.clone() {
        let well_known_pr = path == "/.well-known/oauth-protected-resource/mcp"
            || path == "/.well-known/oauth-protected-resource";
        let make_req = |body: Vec<u8>| OAuthRequest {
            method: method.clone(),
            uri: uri.clone(),
            host: host_of(&headers),
            headers: headers.clone(),
            body,
            peer_ip: connect_info
                .as_ref()
                .map(|ci| ci.0.ip().to_string())
                .unwrap_or_default(),
        };

        if method == Method::GET && well_known_pr {
            return oauth.protected_resource_metadata(&make_req(Vec::new()));
        }
        if method == Method::GET && path == "/.well-known/oauth-authorization-server" {
            return oauth.authorization_server_metadata(&make_req(Vec::new()));
        }
        if method == Method::GET && path == "/.well-known/openid-configuration" {
            return oauth.openid_configuration(&make_req(Vec::new()));
        }
        if method == Method::GET && path == "/oauth/userinfo" {
            return oauth.user_info(&make_req(Vec::new()));
        }
        if method == Method::GET && path == "/oauth/jwks" {
            return oauth.jwks(&make_req(Vec::new()));
        }
        // authorize / token accept GET (consent page / metadata) and POST (form);
        // the TS dispatches on path alone and the handler branches on method.
        if path == "/oauth/authorize" {
            let b = collect_body(body).await;
            return oauth.authorize(&make_req(b));
        }
        if path == "/oauth/token" {
            let b = collect_body(body).await;
            return oauth.token(&make_req(b));
        }
    }

    // --- /mcp : MCP Streamable-HTTP transport --------------------------------
    let mcp_method = matches!(method, Method::POST | Method::GET | Method::DELETE);
    if path == MCP_PATH && mcp_method {
        // Fail closed: never serve the tool surface to the public tunnel unless an
        // auth credential is configured. Without this, tunnel + no token = an
        // internet-reachable shell on the user's repo (auth_ok is open when unset).
        if tunneled && cfg.bearer_token.is_none() {
            return text(
                StatusCode::FORBIDDEN,
                "Refusing to serve MCP over a public tunnel with no auth configured. Set a bearer token.",
            );
        }
        if !auth_ok(&headers, cfg.bearer_token.as_deref(), app.oauth.as_ref()) {
            let mut resp = text(StatusCode::UNAUTHORIZED, "Unauthorized");
            if let Some(public_url) = cfg.public_url.as_deref() {
                let challenge = format!(
                    "Bearer resource_metadata=\"{public_url}/.well-known/oauth-protected-resource\", error=\"invalid_token\", error_description=\"Authorization required\""
                );
                if let Ok(hv) = HeaderValue::from_str(&challenge) {
                    resp.headers_mut().insert(header::WWW_AUTHENTICATE, hv);
                }
            }
            return resp;
        }

        let body_bytes = collect_body(body).await;
        if let Some(resp) = body_limit_response(&headers, body_bytes.len(), 5_000_000) {
            return resp;
        }

        // ChatGPT sends a per-conversation token on every MCP call; we attribute
        // tool events to subagents by it (bound to an agentId via repo_register).
        let session = header_str(&headers, "x-openai-session").map(|s| s.to_string());

        let mcp_req = McpRequest {
            method: method.clone(),
            headers: headers.clone(),
            body: body_bytes,
            session: session.clone(),
        };
        let mcp = app.mcp.clone();
        // Run the dispatch inside the task-local session scope so the state layer's
        // `event()` can read it synchronously (the AsyncLocalStorage analogue),
        // while `mcp_req.session` carries it explicitly as well.
        return CURRENT_SESSION
            .scope(session, async move { mcp.handle_request(mcp_req) })
            .await;
    }

    // --- default 404 ---------------------------------------------------------
    text(StatusCode::NOT_FOUND, "Not Found")
}

/// `/control/*` body, after the loopback + auth gates have passed.
fn handle_control(app: &AppState, method: &Method, path: &str, body: &[u8]) -> Response {
    // POST /control/arm-consent
    if method == Method::POST && path == "/control/arm-consent" {
        let Some(oauth) = app.oauth.as_ref() else {
            return send_json(
                StatusCode::BAD_REQUEST,
                json!({ "ok": false, "error": "oauth not enabled (publicUrl + bearerToken required)" }),
            );
        };
        let body = read_json_body(body, 1_000_000);
        let nonce = body
            .as_ref()
            .and_then(|b| b.get("nonce"))
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        // `Number(body?.ttl_sec ?? 90)` — non-numeric coerces toward the default.
        let ttl_sec = body
            .as_ref()
            .and_then(|b| b.get("ttl_sec"))
            .and_then(num_from_value)
            .unwrap_or(90.0);
        if nonce.chars().count() < 16 {
            return send_json(
                StatusCode::BAD_REQUEST,
                json!({ "ok": false, "error": "nonce (>=16 chars) required" }),
            );
        }
        oauth.arm_consent(&nonce, ttl_sec as u64);
        // `Math.max(1, Math.min(ttlSec || 90, 300))` — note `||` so a 0 ttl => 90.
        let effective = if ttl_sec == 0.0 { 90.0 } else { ttl_sec };
        let clamped = effective.min(300.0).max(1.0) as u64;
        return send_json(StatusCode::OK, json!({ "ok": true, "ttl_sec": clamped }));
    }

    // GET /control/subagents
    if method == Method::GET && path == "/control/subagents" {
        return send_json(StatusCode::OK, json!({ "subagents": app.state.list_jobs() }));
    }

    // GET /control/sessions
    if method == Method::GET && path == "/control/sessions" {
        return send_json(
            StatusCode::OK,
            json!({ "unbound": app.state.unbound_sessions(), "bound": app.state.bound_agents() }),
        );
    }

    // POST /control/subagent/result
    if method == Method::POST && path == "/control/subagent/result" {
        let body = read_json_body(body, 1_000_000);
        let agent_id = body
            .as_ref()
            .and_then(|b| b.get("agent_id"))
            .and_then(Value::as_str);
        let capsule = body.as_ref().and_then(|b| b.get("capsule")).cloned();
        match (body.as_ref(), agent_id, capsule) {
            (Some(_), Some(agent_id), Some(capsule)) if !capsule.is_null() => {
                app.state.set_subagent_result(agent_id, capsule);
                send_json(StatusCode::OK, json!({ "ok": true }))
            }
            _ => send_json(
                StatusCode::BAD_REQUEST,
                json!({ "ok": false, "error": "agent_id and capsule required" }),
            ),
        }
    }
    // POST /control/subagent/update
    else if method == Method::POST && path == "/control/subagent/update" {
        let body = read_json_body(body, 1_000_000);
        let agent_id = body
            .as_ref()
            .and_then(|b| b.get("agent_id"))
            .and_then(Value::as_str);
        let patch = body.as_ref().and_then(|b| b.get("patch")).cloned();
        match (body.as_ref(), agent_id, patch) {
            (Some(_), Some(agent_id), Some(patch)) if !patch.is_null() => {
                let updated = app.state.update_subagent_job(agent_id, patch);
                let status = if updated { StatusCode::OK } else { StatusCode::NOT_FOUND };
                send_json(status, json!({ "ok": updated }))
            }
            _ => send_json(
                StatusCode::BAD_REQUEST,
                json!({ "ok": false, "error": "agent_id and patch required" }),
            ),
        }
    }
    // POST /control/bind
    else if method == Method::POST && path == "/control/bind" {
        let body = read_json_body(body, 1_000_000);
        let session = body
            .as_ref()
            .and_then(|b| b.get("session"))
            .and_then(Value::as_str);
        let agent_id = body
            .as_ref()
            .and_then(|b| b.get("agent_id"))
            .and_then(Value::as_str);
        match (body.as_ref(), session, agent_id) {
            (Some(_), Some(session), Some(agent_id)) => {
                app.state.restamp_agent(session, agent_id);
                send_json(StatusCode::OK, json!({ "ok": true }))
            }
            _ => send_json(
                StatusCode::BAD_REQUEST,
                json!({ "ok": false, "error": "session and agent_id required" }),
            ),
        }
    } else {
        send_json(
            StatusCode::NOT_FOUND,
            json!({ "ok": false, "error": "unknown control endpoint" }),
        )
    }
}

/// `Number(...)` coercion for JSON values used as numbers (string or number).
fn num_from_value(v: &Value) -> Option<f64> {
    match v {
        Value::Number(n) => n.as_f64(),
        Value::String(s) => s.trim().parse::<f64>().ok(),
        _ => None,
    }
}

/// Collect a request body into bytes.
async fn collect_body(body: Body) -> Vec<u8> {
    match axum::body::to_bytes(body, usize::MAX).await {
        Ok(b) => b.to_vec(),
        Err(_) => Vec::new(),
    }
}

// ---------------------------------------------------------------------------
// /events SSE stream.
//
// Seq-cursored, per-agent tail. Replaces the index-based cursor that saturated
// once the event window filled (slice(last) -> [] forever). `?agent=<id>` scopes
// to one subagent; `?since=<seq>` or `Last-Event-ID` resumes after a reconnect.
// ---------------------------------------------------------------------------

fn events_stream(app: AppState, headers: &HeaderMap, query: &BTreeMap<String, String>) -> Response {
    let agent = query.get("agent").filter(|s| !s.is_empty()).cloned();

    // `Number(last-event-id ?? since ?? 0) || 0` — non-numeric / NaN => 0.
    let cursor_seed = header_str(headers, "last-event-id")
        .map(|s| s.to_string())
        .or_else(|| query.get("since").cloned())
        .and_then(|s| s.trim().parse::<f64>().ok())
        .map(|n| if n.is_finite() { n as i64 } else { 0 })
        .unwrap_or(0);

    let (tx, rx) = mpsc::channel::<Result<axum::body::Bytes, Infallible>>(64);

    tokio::spawn(async move {
        let mut cursor = cursor_seed;
        let mut tick = tokio::time::interval(Duration::from_secs(1));
        let mut heartbeat = tokio::time::interval(Duration::from_secs(15));
        // The TS sends the first batch immediately (a synchronous `tick()` before
        // the interval). `interval` fires immediately on first poll, so the first
        // `tick.tick()` plays that role; skip the immediate heartbeat to match.
        heartbeat.tick().await;

        loop {
            tokio::select! {
                _ = tick.tick() => {
                    let agent_ref = agent.as_deref();
                    let events = app.state.recent_events_since(cursor, agent_ref);
                    for evt in events {
                        let seq = evt.seq();
                        if let Some(seq) = seq {
                            cursor = cursor.max(seq);
                        }
                        let id = seq.map(|s| s.to_string()).unwrap_or_default();
                        let data = serde_json::to_string(&evt)
                            .unwrap_or_else(|_| "{}".into());
                        let frame = format!("id: {id}\nevent: tool\ndata: {data}\n\n");
                        if tx.send(Ok(axum::body::Bytes::from(frame))).await.is_err() {
                            return; // client disconnected (`req.on("close")`).
                        }
                    }
                }
                _ = heartbeat.tick() => {
                    if tx.send(Ok(axum::body::Bytes::from_static(b": hb\n\n"))).await.is_err() {
                        return;
                    }
                }
            }
        }
    });

    let stream = ReceiverStream::new(rx);
    let mut resp = Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "text/event-stream")
        .header(header::CACHE_CONTROL, "no-cache")
        .header(header::CONNECTION, "keep-alive")
        .body(Body::from_stream(stream))
        .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response());
    apply_cors(resp.headers_mut());
    resp
}

// ---------------------------------------------------------------------------
// Server entry points (mirror `runHttp` / `runStdio`).
// ---------------------------------------------------------------------------

/// Start the axum HTTP server. See `runHttp()` in index.ts.
///
/// The caller supplies the wired `AppState` (config view + state + oauth + mcp).
/// This binds `host:port` and serves until shutdown.
pub async fn serve(app: AppState) -> anyhow::Result<()> {
    let cfg = app.config.clone();
    let addr = format!("{}:{}", cfg.host, cfg.port);
    let router = build_router(app);

    let listener = tokio::net::TcpListener::bind(&addr).await?;
    tracing::info!(
        "repo-agent-mcp listening on http://{}:{}{}",
        cfg.host,
        cfg.port,
        MCP_PATH
    );
    tracing::info!(
        "root={} spice={} sandbox={}/{}",
        cfg.root,
        cfg.spice_level,
        cfg.sandbox_mode,
        cfg.approval_policy
    );
    if let Some(public_url) = cfg.public_url.as_deref() {
        tracing::info!("oauth enabled at {public_url}");
    }

    // `ConnectInfo` is needed so the OAuth rate limiter can key on the peer IP.
    axum::serve(
        listener,
        router.into_make_service_with_connect_info::<SocketAddr>(),
    )
    .await?;
    Ok(())
}

/// Start the axum HTTP server from the crate `Config`. Port of `runHttp()`:
/// `RepoState::new` -> jwtSigningKey -> oauth iff publicUrl+bearerToken ->
/// `createRepoMcpServer` -> listen. The concrete `StateAccess` /
/// `OAuthProvider` / `McpDispatch` impls live in [`crate::wire`].
pub async fn run_http(cfg: Config) -> anyhow::Result<()> {
    let app = crate::wire::build_app_state(&cfg)?;
    // Mirror the TS startup `console.log` lines (loosely — the TS prints them in
    // the listen callback; [`serve`] also emits them as tracing events).
    println!(
        "repo-agent-mcp listening on http://{}:{}{}",
        app.config.host, app.config.port, MCP_PATH
    );
    println!(
        "root={} spice={} sandbox={}/{}",
        app.config.root, app.config.spice_level, app.config.sandbox_mode, app.config.approval_policy
    );
    if let Some(public_url) = app.config.public_url.as_deref() {
        println!("oauth enabled at {public_url}");
    }
    serve(app).await
}

/// Stdio MCP transport. Port of `runStdio()`: the same registered
/// `RepoMcpServer` connected to a newline-delimited JSON-RPC stdin/stdout loop
/// (the SDK's `StdioServerTransport` framing).
pub async fn run_stdio(cfg: Config) -> anyhow::Result<()> {
    let (_state, server) = crate::wire::build_runtime(&cfg)?;
    crate::wire::run_stdio_transport(server).await
}

// Retain the original placeholder symbol referenced by the skeleton so nothing
// that imports it breaks during the port.
pub fn placeholder() {}
