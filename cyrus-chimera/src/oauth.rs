//! OAuth 2.0 / OIDC authorization server for the MCP connector.
//!
//! HS256 JWTs (HMAC-SHA256 over base64url `header.body`), PKCE S256, a
//! redirect_uri allowlist, the lenient "both ChatGPT hosts" token-endpoint
//! match, the cyrus consent pre-authorization one-time nonce (`arm_consent`),
//! and per-response security headers.
//!
//! Hazards:
//!   - Constant-time compare on the password/secret and on the JWT signature
//!     (here via `subtle`-style byte compare + `hmac`'s `verify_slice`).
//!   - `is_valid_redirect_uri` allowlist (chatgpt.com / openai.com /
//!     codex.openai.com / platform.openai.com / baseUrl host, plus subdomains).
//!   - `signing_key` defaults to `secret` when `JWT_SIGNING_KEY` is unset.
//!   - Security headers (nosniff, frame-deny, strict CSP, referrer policy) on
//!     every OAuth response.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use axum::body::Body;
use axum::http::{header, HeaderMap, Method, StatusCode};
use axum::response::Response;
use hmac::{Hmac, Mac};
use sha2::{Digest, Sha256};
use tokio::sync::Mutex;
use url::Url;

type HmacSha256 = Hmac<Sha256>;

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

const RATE_LIMIT_WINDOW_MS: u128 = 60_000;
const RATE_LIMIT_MAX: u32 = 10;
const CODE_LIFETIME_MS: u128 = 5 * 60 * 1000;
const ACCESS_TOKEN_LIFETIME_S: u64 = 3600;
const REFRESH_TOKEN_LIFETIME_S: u64 = 7 * 24 * 3600;
const ARM_MAX_TTL_S: u64 = 300;

// ---------------------------------------------------------------------------
// Time helpers
// ---------------------------------------------------------------------------

/// `Date.now()` — milliseconds since the Unix epoch.
fn now_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or(Duration::ZERO)
        .as_millis()
}

/// `Math.floor(Date.now() / 1000)` — integer seconds since the Unix epoch.
fn now_s() -> u64 {
    (now_ms() / 1000) as u64
}

// ---------------------------------------------------------------------------
// base64url (no padding) — byte-for-byte match for the TS helpers
// ---------------------------------------------------------------------------

const B64_STD: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

/// `base64UrlEncode(buf)` — standard base64, `+`→`-`, `/`→`_`, strip `=`.
fn base64_url_encode(buf: &[u8]) -> String {
    let mut out = String::with_capacity((buf.len() + 2) / 3 * 4);
    let mut chunks = buf.chunks_exact(3);
    for c in &mut chunks {
        let n = ((c[0] as u32) << 16) | ((c[1] as u32) << 8) | (c[2] as u32);
        out.push(B64_STD[(n >> 18) as usize & 63] as char);
        out.push(B64_STD[(n >> 12) as usize & 63] as char);
        out.push(B64_STD[(n >> 6) as usize & 63] as char);
        out.push(B64_STD[n as usize & 63] as char);
    }
    let rem = chunks.remainder();
    match rem.len() {
        1 => {
            let n = (rem[0] as u32) << 16;
            out.push(B64_STD[(n >> 18) as usize & 63] as char);
            out.push(B64_STD[(n >> 12) as usize & 63] as char);
        }
        2 => {
            let n = ((rem[0] as u32) << 16) | ((rem[1] as u32) << 8);
            out.push(B64_STD[(n >> 18) as usize & 63] as char);
            out.push(B64_STD[(n >> 12) as usize & 63] as char);
            out.push(B64_STD[(n >> 6) as usize & 63] as char);
        }
        _ => {}
    }
    // Translate to the URL-safe alphabet. (We emitted `+`/`/` above to keep the
    // table simple; swap them now exactly as the TS does.)
    out.replace('+', "-").replace('/', "_")
}

/// `base64UrlDecode(str)` — re-pad, `-`→`+`, `_`→`/`, then standard base64
/// decode. Mirrors Node's lenient `Buffer.from(..., "base64")`: unknown
/// characters are skipped rather than erroring.
fn base64_url_decode(s: &str) -> Vec<u8> {
    let padded_len = s.len() + ((4 - (s.len() % 4)) % 4);
    let mut normalized = String::with_capacity(padded_len);
    for ch in s.chars() {
        match ch {
            '-' => normalized.push('+'),
            '_' => normalized.push('/'),
            other => normalized.push(other),
        }
    }
    while normalized.len() < padded_len {
        normalized.push('=');
    }
    decode_std_base64(&normalized)
}

/// Minimal, lenient standard-base64 decoder matching Node's `Buffer.from`
/// behavior closely enough for our inputs: skip non-alphabet bytes, stop at `=`.
fn decode_std_base64(s: &str) -> Vec<u8> {
    fn val(c: u8) -> Option<u8> {
        match c {
            b'A'..=b'Z' => Some(c - b'A'),
            b'a'..=b'z' => Some(c - b'a' + 26),
            b'0'..=b'9' => Some(c - b'0' + 52),
            b'+' => Some(62),
            b'/' => Some(63),
            _ => None,
        }
    }
    let mut out = Vec::with_capacity(s.len() / 4 * 3);
    let mut acc: u32 = 0;
    let mut bits = 0u32;
    for &b in s.as_bytes() {
        if b == b'=' {
            break;
        }
        let v = match val(b) {
            Some(v) => v as u32,
            None => continue, // skip whitespace / stray chars, like Node
        };
        acc = (acc << 6) | v;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
        }
    }
    out
}

// ---------------------------------------------------------------------------
// JWT
// ---------------------------------------------------------------------------

/// A single JWT claim. We build the payload JSON by hand, in order, so the
/// signed bytes match `JSON.stringify(payload)` regardless of whether
/// serde_json's `preserve_order` feature is active (the default `BTreeMap`
/// backing would otherwise re-sort the keys and change the signature).
enum Claim {
    Str(&'static str, String),
    Num(&'static str, u64),
}

/// Serialize claims to a compact JSON object in the given order, matching
/// `JSON.stringify` (no spaces; `"` and `\` and control chars escaped).
fn claims_to_json(claims: &[Claim]) -> String {
    let mut s = String::from("{");
    for (i, claim) in claims.iter().enumerate() {
        if i > 0 {
            s.push(',');
        }
        match claim {
            Claim::Str(k, v) => {
                s.push_str(&json_string(k));
                s.push(':');
                s.push_str(&json_string(v));
            }
            Claim::Num(k, v) => {
                s.push_str(&json_string(k));
                s.push(':');
                s.push_str(&v.to_string());
            }
        }
    }
    s.push('}');
    s
}

/// Encode a string as a JSON string literal the way `JSON.stringify` does for
/// our inputs (ASCII identifiers and base64url tokens never need more than the
/// minimal escaping, but we handle the standard set for safety).
fn json_string(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\u{0008}' => out.push_str("\\b"),
            '\u{000c}' => out.push_str("\\f"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// `signJwt(payload, secret)`. `payload` is the pre-serialized compact JSON body
/// (built by [`claims_to_json`] so claim order is preserved byte-for-byte).
fn sign_jwt(payload_json: &str, secret: &str) -> String {
    let header = base64_url_encode(br#"{"alg":"HS256","typ":"JWT"}"#);
    let body = base64_url_encode(payload_json.as_bytes());
    let signing_input = format!("{header}.{body}");
    let sig = hmac_sha256(secret.as_bytes(), signing_input.as_bytes());
    let sig_b64 = base64_url_encode(&sig);
    format!("{header}.{body}.{sig_b64}")
}

/// `verifyJwt(token, secret)` — split on `.`, recompute the HMAC over
/// `header.body`, length-check then constant-time compare against the decoded
/// signature, parse the body JSON, and reject if `exp < now`.
fn verify_jwt(token: &str, secret: &str) -> Option<serde_json::Map<String, serde_json::Value>> {
    let parts: Vec<&str> = token.split('.').collect();
    if parts.len() != 3 {
        return None;
    }
    let (header, body, signature) = (parts[0], parts[1], parts[2]);
    let signing_input = format!("{header}.{body}");
    let expected = hmac_sha256(secret.as_bytes(), signing_input.as_bytes());
    let actual = base64_url_decode(signature);
    // Length guard mirrors the TS `expectedSig.length !== actualSig.length`.
    if expected.len() != actual.len() {
        return None;
    }
    // Constant-time signature comparison (TS `timingSafeEqual`). `verify_slice`
    // recomputes the MAC internally and compares in constant time.
    let mut mac = HmacSha256::new_from_slice(secret.as_bytes()).ok()?;
    mac.update(signing_input.as_bytes());
    if mac.verify_slice(&actual).is_err() {
        return None;
    }

    let decoded = base64_url_decode(body);
    let text = String::from_utf8(decoded).ok()?;
    let value: serde_json::Value = serde_json::from_str(&text).ok()?;
    let payload = value.as_object()?.clone();
    // `if (typeof payload.exp === "number" && payload.exp < Date.now()/1000)`.
    if let Some(exp) = payload.get("exp").and_then(|v| v.as_f64()) {
        let now = now_ms() as f64 / 1000.0;
        if exp < now {
            return None;
        }
    }
    Some(payload)
}

/// Raw HMAC-SHA256 digest of `data` under `key` (matches `createHmac`).
fn hmac_sha256(key: &[u8], data: &[u8]) -> Vec<u8> {
    let mut mac = HmacSha256::new_from_slice(key).expect("HMAC accepts any key length");
    mac.update(data);
    mac.finalize().into_bytes().to_vec()
}

// ---------------------------------------------------------------------------
// Random + constant-time compare
// ---------------------------------------------------------------------------

/// `randomBytes(n)` then `base64UrlEncode`.
fn random_b64url(n: usize) -> String {
    let mut buf = vec![0u8; n];
    getrandom::getrandom(&mut buf).expect("OS RNG unavailable");
    base64_url_encode(&buf)
}

fn generate_code() -> String {
    random_b64url(32)
}

fn generate_csrf_token() -> String {
    random_b64url(16)
}

/// Constant-time byte comparison for equal-length slices (TS `timingSafeEqual`).
/// The caller is responsible for the length check, exactly as the TS does (it
/// compares `length` first, then calls `timingSafeEqual`).
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff: u8 = 0;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

// ---------------------------------------------------------------------------
// HTML escaping (TS escapeHtml)
// ---------------------------------------------------------------------------

fn escape_html(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

// ---------------------------------------------------------------------------
// redirect_uri allowlist (TS isValidRedirectUri)
// ---------------------------------------------------------------------------

fn is_valid_redirect_uri(uri: &str, base_url: &str) -> bool {
    let url = match Url::parse(uri) {
        Ok(u) => u,
        Err(_) => return false,
    };
    let scheme = url.scheme();
    let hostname = match url.host_str() {
        Some(h) => h.to_string(),
        None => return false,
    };

    let prod = base_url.starts_with("https://");
    // Only allow HTTPS redirects in production.
    if prod && scheme != "https" {
        return false;
    }
    // Block localhost redirects in production.
    if prod && (hostname == "localhost" || hostname == "127.0.0.1") {
        return false;
    }

    // Allow only known ChatGPT/Codex domains and the baseUrl domain.
    let base_host = Url::parse(base_url)
        .ok()
        .and_then(|u| u.host_str().map(|h| h.to_string()));
    let mut allowed: Vec<String> = vec![
        "chatgpt.com".into(),
        "openai.com".into(),
        "codex.openai.com".into(),
        "platform.openai.com".into(),
    ];
    if let Some(h) = base_host {
        allowed.push(h);
    }
    // Allow exact matches and subdomains.
    allowed
        .iter()
        .any(|host| hostname == *host || hostname.ends_with(&format!(".{host}")))
}

/// `isBothChatGPT` from the token endpoint — true only if BOTH URIs resolve to a
/// hostname in {chatgpt.com, openai.com}. Used to leniently accept ChatGPT's
/// redirect_uri quirks during the code exchange.
fn is_both_chatgpt(uri1: &str, uri2: &str) -> bool {
    let allowed = ["chatgpt.com", "openai.com"];
    let h1 = Url::parse(uri1).ok().and_then(|u| u.host_str().map(String::from));
    let h2 = Url::parse(uri2).ok().and_then(|u| u.host_str().map(String::from));
    match (h1, h2) {
        (Some(a), Some(b)) => allowed.contains(&a.as_str()) && allowed.contains(&b.as_str()),
        _ => false,
    }
}

// ---------------------------------------------------------------------------
// Shared in-memory state
// ---------------------------------------------------------------------------

#[derive(Clone)]
struct AuthCode {
    #[allow(dead_code)]
    code: String,
    #[allow(dead_code)]
    client_id: String,
    redirect_uri: String,
    code_challenge: Option<String>,
    expires_at: u128,
    used: bool,
}

struct RateLimitEntry {
    count: u32,
    window_start: u128,
}

struct CsrfEntry {
    token: String,
    expires_at: u128,
}

/// Per-server in-memory state. In the TS, `codes`/`rateLimits` are module-level
/// singletons while `csrfTokens`/`armedNonces` are per-`createOAuthHandlers`
/// closures. We keep all of it inside the per-instance struct so each `OAuth`
/// is self-contained; behavior is identical for a single server instance.
#[derive(Default)]
struct State {
    codes: HashMap<String, AuthCode>,
    rate_limits: HashMap<String, RateLimitEntry>,
    csrf_tokens: HashMap<String, CsrfEntry>,
    armed_nonces: HashMap<String, u128>, // nonce -> expiresAt (ms)
}

struct Inner {
    base_url: String,
    secret: String,
    client_id: String,
    /// JWT signing key — distinct from `secret` when provided, else `secret`.
    signing_key: String,
    /// Repo root shown on the consent page so the user can see what they are
    /// granting access to. Display-only; never used for authorization.
    repo_root: String,
    state: Mutex<State>,
}

/// Handle bundle returned by `createOAuthHandlers`. Cheap to clone (Arc).
#[derive(Clone)]
pub struct OAuth {
    inner: Arc<Inner>,
}

impl std::fmt::Debug for OAuth {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OAuth")
            .field("base_url", &self.inner.base_url)
            .field("client_id", &self.inner.client_id)
            .finish_non_exhaustive()
    }
}

impl OAuth {
    /// `createOAuthHandlers(config)`. `signing_key` falls back to `secret` when
    /// `None`/empty, matching `config.signingKey || secret`. `repo_root` is the
    /// directory shown on the consent page.
    pub fn new(
        base_url: impl Into<String>,
        secret: impl Into<String>,
        client_id: impl Into<String>,
        signing_key: Option<String>,
        repo_root: impl Into<String>,
    ) -> Self {
        let secret = secret.into();
        let signing_key = match signing_key {
            Some(k) if !k.is_empty() => k,
            _ => secret.clone(),
        };
        OAuth {
            inner: Arc::new(Inner {
                base_url: base_url.into(),
                secret,
                client_id: client_id.into(),
                signing_key,
                repo_root: repo_root.into(),
                state: Mutex::new(State::default()),
            }),
        }
    }

    /// Replaces the TS `setInterval(..., 60_000)`: prune expired codes, CSRF
    /// tokens, rate-limit windows, and armed nonces every 60s. Spawn this once
    /// after construction; the task lives as long as the returned `OAuth` (it
    /// holds a `Weak`, so it self-terminates when the last `OAuth` is dropped).
    pub fn spawn_cleanup(&self) {
        let weak = Arc::downgrade(&self.inner);
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(Duration::from_millis(60_000));
            tick.tick().await; // consume the immediate first tick
            loop {
                tick.tick().await;
                let Some(inner) = weak.upgrade() else {
                    break;
                };
                let now = now_ms();
                let mut st = inner.state.lock().await;
                st.codes.retain(|_, c| c.expires_at >= now);
                st.csrf_tokens.retain(|_, e| e.expires_at >= now);
                st.rate_limits
                    .retain(|_, e| now.saturating_sub(e.window_start) <= RATE_LIMIT_WINDOW_MS);
                st.armed_nonces.retain(|_, &mut exp| exp >= now);
            }
        });
    }

    // -- Consent pre-authorization (cyrus) ---------------------------------

    /// `armConsent(nonce, ttlSec)` — store a one-time, short-TTL nonce. TTL is
    /// clamped to `[1, ARM_MAX_TTL_S]` with a default of 90 when `ttl_sec == 0`.
    pub async fn arm_consent(&self, nonce: &str, ttl_sec: u64) {
        let base = if ttl_sec == 0 { 90 } else { ttl_sec };
        let ttl = base.clamp(1, ARM_MAX_TTL_S);
        let expires_at = now_ms() + (ttl as u128) * 1000;
        self.inner
            .state
            .lock()
            .await
            .armed_nonces
            .insert(nonce.to_string(), expires_at);
    }

    /// `consumeArmedNonce(nonce)` — single-use: any matching nonce is removed on
    /// lookup. Returns true only if present AND unexpired, so a malformed/expired
    /// flow still burns it.
    async fn consume_armed_nonce(&self, nonce: &str) -> bool {
        if nonce.is_empty() {
            return false;
        }
        let mut st = self.inner.state.lock().await;
        match st.armed_nonces.remove(nonce) {
            Some(expires_at) => expires_at >= now_ms(),
            None => false,
        }
    }

    // -- Rate limiting ------------------------------------------------------

    /// `checkRateLimit(key)` — fixed 60s window, max 10 hits.
    async fn check_rate_limit(&self, key: &str) -> bool {
        let now = now_ms();
        let mut st = self.inner.state.lock().await;
        match st.rate_limits.get_mut(key) {
            Some(entry) if now.saturating_sub(entry.window_start) <= RATE_LIMIT_WINDOW_MS => {
                if entry.count >= RATE_LIMIT_MAX {
                    false
                } else {
                    entry.count += 1;
                    true
                }
            }
            _ => {
                st.rate_limits.insert(
                    key.to_string(),
                    RateLimitEntry {
                        count: 1,
                        window_start: now,
                    },
                );
                true
            }
        }
    }

    // -- Token validation ---------------------------------------------------

    /// `validateToken(token)` — HS256 verify under the signing key + scope check.
    pub fn validate_token(&self, token: &str) -> bool {
        validate_access_token(token, &self.inner.signing_key)
    }

    // ----------------------------------------------------------------------
    // Endpoint handlers. Each applies `securityHeaders` to its response.
    // ----------------------------------------------------------------------

    /// `GET /.well-known/oauth-protected-resource[/mcp]`.
    pub fn protected_resource_metadata(&self, path: &str) -> Response {
        let is_root = path == "/.well-known/oauth-protected-resource";
        let resource_path = if is_root { "" } else { "/mcp" };
        let base_url = &self.inner.base_url;
        let body = serde_json::json!({
            "resource": format!("{base_url}{resource_path}"),
            "authorization_servers": [base_url],
            "scopes_supported": ["mcp"],
            "bearer_methods_supported": ["header"],
        });
        json_response(StatusCode::OK, &pretty(&body))
    }

    /// `GET /.well-known/oauth-authorization-server`.
    pub fn authorization_server_metadata(&self) -> Response {
        let base = trim_trailing_slash(&self.inner.base_url);
        let body = serde_json::json!({
            "issuer": base,
            "authorization_endpoint": format!("{base}/oauth/authorize"),
            "token_endpoint": format!("{base}/oauth/token"),
            "token_endpoint_auth_methods_supported": ["none"],
            "grant_types_supported": ["authorization_code", "refresh_token"],
            "response_types_supported": ["code"],
            "code_challenge_methods_supported": ["S256"],
            "scopes_supported": ["mcp"],
        });
        json_response(StatusCode::OK, &pretty(&body))
    }

    /// `GET /.well-known/openid-configuration`.
    pub fn openid_configuration(&self) -> Response {
        let base = trim_trailing_slash(&self.inner.base_url);
        let body = serde_json::json!({
            "issuer": base,
            "authorization_endpoint": format!("{base}/oauth/authorize"),
            "token_endpoint": format!("{base}/oauth/token"),
            "userinfo_endpoint": format!("{base}/oauth/userinfo"),
            "jwks_uri": format!("{base}/oauth/jwks"),
            "token_endpoint_auth_methods_supported": ["none"],
            "grant_types_supported": ["authorization_code", "refresh_token"],
            "response_types_supported": ["code"],
            "code_challenge_methods_supported": ["S256"],
            "scopes_supported": ["mcp", "openid", "profile", "email"],
            "subject_types_supported": ["public"],
            "id_token_signing_alg_values_supported": ["HS256"],
            "claims_supported": ["sub", "email", "email_verified", "name"],
        });
        json_response(StatusCode::OK, &pretty(&body))
    }

    /// `GET /oauth/userinfo`. Accepts the raw `secret` as a bearer or a valid
    /// HS256 access token.
    pub fn user_info(&self, headers: &HeaderMap) -> Response {
        let header = headers
            .get(header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");
        let bearer = header.strip_prefix("Bearer ").unwrap_or("");
        let authorized = !bearer.is_empty()
            && (bearer == self.inner.secret || self.validate_token(bearer));
        if !authorized {
            let mut resp = json_response(
                StatusCode::UNAUTHORIZED,
                &serde_json::json!({ "error": "invalid_token" }).to_string(),
            );
            resp.headers_mut().insert(
                header::WWW_AUTHENTICATE,
                header::HeaderValue::from_static("Bearer"),
            );
            return resp;
        }
        json_response(
            StatusCode::OK,
            &serde_json::json!({
                "sub": "repo-agent-local-user",
                "name": "Repo Agent Local User",
                "email": "repo-agent-local@example.invalid",
                "email_verified": true,
            })
            .to_string(),
        )
    }

    /// `GET /oauth/jwks` — empty key set (HS256 is symmetric).
    pub fn jwks(&self) -> Response {
        json_response(
            StatusCode::OK,
            &serde_json::json!({ "keys": [] }).to_string(),
        )
    }

    /// `GET /oauth/authorize` — consent page or, with a live cyrus nonce + PKCE +
    /// allowlisted redirect, auto-issue the code. Rate limiting is applied by the
    /// [`OAuth::authorize`] dispatcher before this runs (matching the TS, which
    /// rate-limits ahead of the method branch).
    ///
    /// `query` is the raw query string (everything after `?`).
    pub async fn authorize_get(&self, query: &str) -> Response {
        let params = parse_query(query);
        let redirect_uri = params.get("redirect_uri").cloned().unwrap_or_default();
        let state = params.get("state").cloned().unwrap_or_default();
        // `?? undefined` — absent stays None, present (even empty) is Some.
        let code_challenge = params.get("code_challenge").cloned();

        // Validate redirect_uri on GET.
        if !is_valid_redirect_uri(&redirect_uri, &self.inner.base_url) {
            return html_response(StatusCode::BAD_REQUEST, "<h1>Invalid redirect_uri</h1>");
        }

        // Consent pre-authorization (cyrus). Nonce checked LAST so a malformed
        // request never burns it.
        let cyrus_nonce = params.get("cyrus_nonce").cloned().unwrap_or_default();
        if !cyrus_nonce.is_empty()
            && code_challenge.is_some()
            && self.consume_armed_nonce(&cyrus_nonce).await
        {
            let code = generate_code();
            {
                let mut st = self.inner.state.lock().await;
                st.codes.insert(
                    code.clone(),
                    AuthCode {
                        code: code.clone(),
                        client_id: self.inner.client_id.clone(),
                        redirect_uri: redirect_uri.clone(),
                        code_challenge: code_challenge.clone(),
                        expires_at: now_ms() + CODE_LIFETIME_MS,
                        used: false,
                    },
                );
            }
            let location = build_redirect(&redirect_uri, &code, &state);
            return redirect_response(&location);
        }

        // Render the consent/password page.
        let csrf_token = generate_csrf_token();
        let session_id = generate_csrf_token();
        {
            let mut st = self.inner.state.lock().await;
            st.csrf_tokens.insert(
                session_id.clone(),
                CsrfEntry {
                    token: csrf_token.clone(),
                    expires_at: now_ms() + 10 * 60 * 1000,
                },
            );
        }

        let html = consent_html(
            &redirect_uri,
            &state,
            code_challenge.as_deref().unwrap_or(""),
            &csrf_token,
            &session_id,
            &self.inner.repo_root,
        );
        html_response(StatusCode::OK, &html)
    }

    /// `POST /oauth/authorize` — validate CSRF + redirect + token, then issue a
    /// code (or redirect with `error=access_denied` on deny).
    ///
    /// `body` is the raw `application/x-www-form-urlencoded` request body.
    pub async fn authorize_post(&self, body: &str) -> Response {
        let params = parse_query(body);
        let action = params.get("action").cloned();
        let redirect_uri_post = params.get("redirect_uri").cloned().unwrap_or_default();
        let state_post = params.get("state").cloned().unwrap_or_default();
        // `params.get("code_challenge") || undefined` — empty string => None.
        let code_challenge_post = params
            .get("code_challenge")
            .filter(|s| !s.is_empty())
            .cloned();
        let csrf_token = params.get("csrf_token").cloned().unwrap_or_default();
        let csrf_session = params.get("csrf_session").cloned().unwrap_or_default();
        let password = params.get("password").cloned().unwrap_or_default();

        // Validate CSRF (and consume the session on success).
        {
            let mut st = self.inner.state.lock().await;
            let valid = match st.csrf_tokens.get(&csrf_session) {
                Some(entry) => entry.expires_at >= now_ms() && entry.token == csrf_token,
                None => false,
            };
            if !valid {
                return html_response(
                    StatusCode::FORBIDDEN,
                    "<h1>Invalid or expired session</h1>",
                );
            }
            st.csrf_tokens.remove(&csrf_session);
        }

        // Re-validate redirect_uri on POST.
        if !is_valid_redirect_uri(&redirect_uri_post, &self.inner.base_url) {
            return html_response(StatusCode::BAD_REQUEST, "<h1>Invalid redirect_uri</h1>");
        }

        // Deny path.
        if action.as_deref() != Some("approve") {
            let location = build_redirect_error(&redirect_uri_post, "access_denied", &state_post);
            return redirect_response(&location);
        }

        // Validate password against secret (constant-time, length-guarded).
        let expected = self.inner.secret.as_bytes();
        let actual = password.as_bytes();
        if expected.len() != actual.len() || !constant_time_eq(expected, actual) {
            return html_response(
                StatusCode::UNAUTHORIZED,
                "<h1>Unauthorized: Invalid Token</h1>",
            );
        }

        let code = generate_code();
        {
            let mut st = self.inner.state.lock().await;
            st.codes.insert(
                code.clone(),
                AuthCode {
                    code: code.clone(),
                    client_id: self.inner.client_id.clone(),
                    redirect_uri: redirect_uri_post.clone(),
                    code_challenge: code_challenge_post.clone(),
                    expires_at: now_ms() + CODE_LIFETIME_MS,
                    used: false,
                },
            );
        }
        let location = build_redirect(&redirect_uri_post, &code, &state_post);
        redirect_response(&location)
    }

    /// Dispatch `authorize` by method, mirroring the TS single handler. The TS
    /// rate-limits by IP BEFORE the method branch (so it applies to GET, POST,
    /// and any other method), and returns a 405 only after passing the rate
    /// limit. We replicate that ordering exactly: rate-limit first, then branch
    /// GET/POST/other.
    pub async fn authorize(
        &self,
        method: &Method,
        query: &str,
        body: &str,
        client_ip: &str,
    ) -> Response {
 // Rate limit by IP.
        if !self.check_rate_limit(&format!("auth:{client_ip}")).await {
            return html_response(StatusCode::TOO_MANY_REQUESTS, "<h1>Too many requests</h1>");
        }
        match *method {
            Method::GET => self.authorize_get(query).await,
            Method::POST => self.authorize_post(body).await,
            _ => method_not_allowed(),
        }
    }

    /// `POST /oauth/token` — authorization_code + refresh_token grants.
    pub async fn token(&self, method: &Method, body: &str, client_ip: &str) -> Response {
        if *method != Method::POST {
            return method_not_allowed();
        }

        // Rate limit token endpoint.
        if !self.check_rate_limit(&format!("token:{client_ip}")).await {
            return json_response(
                StatusCode::TOO_MANY_REQUESTS,
                &serde_json::json!({ "error": "slow_down" }).to_string(),
            );
        }

        let params = parse_query(body);
        let grant_type = params.get("grant_type").map(String::as_str);

        match grant_type {
            Some("authorization_code") => self.token_authorization_code(&params).await,
            Some("refresh_token") => self.token_refresh(&params).await,
            _ => json_response(
                StatusCode::BAD_REQUEST,
                &serde_json::json!({ "error": "unsupported_grant_type" }).to_string(),
            ),
        }
    }

    async fn token_authorization_code(&self, params: &HashMap<String, String>) -> Response {
        let code = params.get("code").cloned().unwrap_or_default();
        let redirect_uri = params.get("redirect_uri").cloned().unwrap_or_default();
        let code_verifier = params.get("code_verifier").cloned().unwrap_or_default();

        // Look up the auth code. We snapshot it under the lock; mutation/removal
        // happens after validation (matching the TS, which only flips `used` and
        // deletes once everything else passes).
        let auth_code = {
            let st = self.inner.state.lock().await;
            st.codes.get(&code).cloned()
        };

        let auth_code = match auth_code {
            Some(c) if c.expires_at >= now_ms() && !c.used => c,
            _ => {
                return json_response(
                    StatusCode::BAD_REQUEST,
                    &serde_json::json!({ "error": "invalid_grant" }).to_string(),
                );
            }
        };

        // Lenient redirect_uri validation for ChatGPT quirks.
        if auth_code.redirect_uri != redirect_uri
            && !is_both_chatgpt(&auth_code.redirect_uri, &redirect_uri)
        {
            return json_response(
                StatusCode::BAD_REQUEST,
                &serde_json::json!({ "error": "invalid_grant" }).to_string(),
            );
        }

        // Enforce PKCE for public clients.
        if let Some(challenge_expected) = &auth_code.code_challenge {
            if code_verifier.is_empty() {
                return json_response(
                    StatusCode::BAD_REQUEST,
                    &serde_json::json!({
                        "error": "invalid_grant",
                        "error_description": "PKCE code_verifier required"
                    })
                    .to_string(),
                );
            }
            let hash = Sha256::digest(code_verifier.as_bytes());
            let challenge = base64_url_encode(&hash);
            if &challenge != challenge_expected {
                return json_response(
                    StatusCode::BAD_REQUEST,
                    &serde_json::json!({ "error": "invalid_grant" }).to_string(),
                );
            }
        }

        // Mark code as used (prevent replay) — delete it.
        {
            let mut st = self.inner.state.lock().await;
            st.codes.remove(&code);
        }

        let now = now_s();
        let signing_key = &self.inner.signing_key;
        // Claim order matches the TS object literal exactly so the signed bytes
        // are identical: sub, scope, iat, exp[, type, jti].
        let access_token = sign_jwt(
            &claims_to_json(&[
                Claim::Str("sub", "user".into()),
                Claim::Str("scope", "mcp".into()),
                Claim::Num("iat", now),
                Claim::Num("exp", now + ACCESS_TOKEN_LIFETIME_S),
            ]),
            signing_key,
        );
        let refresh_token = sign_jwt(
            &claims_to_json(&[
                Claim::Str("sub", "user".into()),
                Claim::Str("scope", "mcp".into()),
                Claim::Num("iat", now),
                Claim::Num("exp", now + REFRESH_TOKEN_LIFETIME_S),
                Claim::Str("type", "refresh".into()),
                Claim::Str("jti", random_b64url(16)),
            ]),
            signing_key,
        );

        json_response(
            StatusCode::OK,
            &serde_json::json!({
                "access_token": access_token,
                "token_type": "Bearer",
                "expires_in": ACCESS_TOKEN_LIFETIME_S,
                "refresh_token": refresh_token,
                "scope": "mcp",
            })
            .to_string(),
        )
    }

    async fn token_refresh(&self, params: &HashMap<String, String>) -> Response {
        let refresh_token = params.get("refresh_token").cloned().unwrap_or_default();
        let payload = verify_jwt(&refresh_token, &self.inner.signing_key);
        let valid = match &payload {
            Some(p) => p.get("type").and_then(|v| v.as_str()) == Some("refresh"),
            None => false,
        };
        if !valid {
            return json_response(
                StatusCode::BAD_REQUEST,
                &serde_json::json!({ "error": "invalid_grant" }).to_string(),
            );
        }

        let now = now_s();
        let access_token = sign_jwt(
            &claims_to_json(&[
                Claim::Str("sub", "user".into()),
                Claim::Str("scope", "mcp".into()),
                Claim::Num("iat", now),
                Claim::Num("exp", now + ACCESS_TOKEN_LIFETIME_S),
            ]),
            &self.inner.signing_key,
        );
        json_response(
            StatusCode::OK,
            &serde_json::json!({
                "access_token": access_token,
                "token_type": "Bearer",
                "expires_in": ACCESS_TOKEN_LIFETIME_S,
                "scope": "mcp",
            })
            .to_string(),
        )
    }
}

/// `validateAccessToken(token, secret)` — exported standalone in the TS:
/// HS256 verify and require `scope === "mcp"`.
pub fn validate_access_token(token: &str, secret: &str) -> bool {
    match verify_jwt(token, secret) {
        Some(payload) => payload.get("scope").and_then(|v| v.as_str()) == Some("mcp"),
        None => false,
    }
}

// ---------------------------------------------------------------------------
// Request helpers (porting getClientIp / URLSearchParams / collectBody)
// ---------------------------------------------------------------------------

/// `getClientIp(req)` — first `x-forwarded-for` hop, else the provided socket
/// peer address, else `"unknown"`. The caller supplies `remote_addr` because
/// axum exposes the socket address out-of-band (via `ConnectInfo`).
pub fn client_ip(headers: &HeaderMap, remote_addr: Option<&str>) -> String {
    if let Some(fwd) = headers.get("x-forwarded-for").and_then(|v| v.to_str().ok()) {
        if let Some(first) = fwd.split(',').next() {
            return first.trim().to_string();
        }
    }
    remote_addr.unwrap_or("unknown").to_string()
}

/// Parse an `application/x-www-form-urlencoded` string (query or body) into a
/// map, taking the FIRST value for any repeated key — matching the behavior the
/// TS relies on via `URLSearchParams.get`.
fn parse_query(s: &str) -> HashMap<String, String> {
    let mut map = HashMap::new();
    for (k, v) in form_urlencoded::parse(s.as_bytes()) {
        map.entry(k.into_owned()).or_insert_with(|| v.into_owned());
    }
    map
}

// ---------------------------------------------------------------------------
// Response builders
// ---------------------------------------------------------------------------

/// `securityHeaders(res)` — applied to every OAuth response.
fn apply_security_headers(resp: &mut Response) {
    let h = resp.headers_mut();
    h.insert(
        "X-Content-Type-Options",
        header::HeaderValue::from_static("nosniff"),
    );
    h.insert(
        "X-Frame-Options",
        header::HeaderValue::from_static("DENY"),
    );
    h.insert(
        "Content-Security-Policy",
        header::HeaderValue::from_static(
            "default-src 'self'; script-src 'none'; object-src 'none'",
        ),
    );
    h.insert(
        "Referrer-Policy",
        header::HeaderValue::from_static("strict-origin-when-cross-origin"),
    );
}

fn json_response(status: StatusCode, body: &str) -> Response {
    let mut resp = Response::builder()
        .status(status)
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(body.to_owned()))
        .expect("valid response");
    apply_security_headers(&mut resp);
    resp
}

fn html_response(status: StatusCode, body: &str) -> Response {
    let mut resp = Response::builder()
        .status(status)
        .header(header::CONTENT_TYPE, "text/html")
        .body(Body::from(body.to_owned()))
        .expect("valid response");
    apply_security_headers(&mut resp);
    resp
}

/// `res.writeHead(302, { location }).end()`.
fn redirect_response(location: &str) -> Response {
    let mut resp = Response::builder()
        .status(StatusCode::FOUND)
        .header(header::LOCATION, location)
        .body(Body::empty())
        .expect("valid response");
    apply_security_headers(&mut resp);
    resp
}

/// `res.writeHead(405).end("Method Not Allowed")`.
fn method_not_allowed() -> Response {
    let mut resp = Response::builder()
        .status(StatusCode::METHOD_NOT_ALLOWED)
        .body(Body::from("Method Not Allowed"))
        .expect("valid response");
    apply_security_headers(&mut resp);
    resp
}

// ---------------------------------------------------------------------------
// URL building (mirrors `new URL(redirectUri); url.searchParams.set(...)`)
// ---------------------------------------------------------------------------

/// `const r = new URL(redirectUri); r.searchParams.set("code", code); if (state)
/// r.searchParams.set("state", state); return r.toString();`
fn build_redirect(redirect_uri: &str, code: &str, state: &str) -> String {
    let mut url = Url::parse(redirect_uri).expect("redirect_uri validated upstream");
    url.query_pairs_mut().append_pair("code", code);
    if !state.is_empty() {
        url.query_pairs_mut().append_pair("state", state);
    }
    url.to_string()
}

/// `url.searchParams.set("error", "access_denied"); if (state) set("state", ...)`.
fn build_redirect_error(redirect_uri: &str, error: &str, state: &str) -> String {
    let mut url = Url::parse(redirect_uri).expect("redirect_uri validated upstream");
    url.query_pairs_mut().append_pair("error", error);
    if !state.is_empty() {
        url.query_pairs_mut().append_pair("state", state);
    }
    url.to_string()
}

fn trim_trailing_slash(s: &str) -> String {
    s.strip_suffix('/').unwrap_or(s).to_string()
}

/// `JSON.stringify(value, null, 2)` — pretty-printed with 2-space indentation.
fn pretty(value: &serde_json::Value) -> String {
    serde_json::to_string_pretty(value).unwrap_or_default()
}

// ---------------------------------------------------------------------------
// Consent page HTML (matches oauth.ts, except the repo path is dynamic — the
// TS original hardcoded the author's local directory)
// ---------------------------------------------------------------------------

fn consent_html(
    redirect_uri: &str,
    state: &str,
    code_challenge: &str,
    csrf_token: &str,
    session_id: &str,
    repo_root: &str,
) -> String {
    format!(
        r#"<!doctype html>
<html><head><meta charset="utf-8"><title>Authorize Repo Agent</title>
<style>
body{{font-family:system-ui,sans-serif;max-width:480px;margin:60px auto;padding:0 20px;background:#f5f5f5}}
.card{{border:1px solid #ddd;border-radius:12px;padding:24px;background:#fff;box-shadow:0 2px 8px rgba(0,0,0,0.1)}}
h2{{margin-top:0;color:#111}}
p{{color:#555;line-height:1.5}}
input[type="password"]{{width:100%;padding:12px;border:1px solid #ddd;border-radius:8px;margin-bottom:16px;box-sizing:border-box;font-size:16px}}
button{{background:#111;color:#fff;border:0;border-radius:8px;padding:12px 24px;font-size:16px;cursor:pointer;margin-right:12px;font-weight:500}}
button:hover{{background:#333}}
button.deny{{background:#fff;color:#111;border:1px solid #ddd}}
button.deny:hover{{background:#f5f5f5}}
.warning{{color:#d97706;font-size:14px;margin-top:16px;padding:12px;background:#fffbeb;border-radius:6px;border:1px solid #fcd34d}}
</style></head>
<body>
<div class="card">
<h2>Authorize Repo Agent MCP</h2>
<p><strong>ChatGPT</strong> is requesting access to your local repo-agent-mcp server.</p>
<p style="font-size:14px;color:#666">This will allow ChatGPT to read, search, and modify files in:<br><code style="background:#f3f4f6;padding:2px 6px;border-radius:4px">{repo_root}</code></p>
<div class="warning" style="margin-bottom:16px">Only approve if you initiated this connection.</div>
<form method="POST" action="/oauth/authorize">
<p style="font-size:14px;font-weight:bold;margin-bottom:8px">Enter your Repo Agent Token to authorize:</p>
<input type="password" name="password" placeholder="Enter Token" required autocomplete="off">
<input type="hidden" name="redirect_uri" value="{redirect_uri}">
<input type="hidden" name="state" value="{state}">
<input type="hidden" name="code_challenge" value="{code_challenge}">
<input type="hidden" name="csrf_token" value="{csrf_token}">
<input type="hidden" name="csrf_session" value="{session_id}">
<button type="submit" name="action" value="approve">Approve</button>
<button type="submit" name="action" value="deny" class="deny">Deny</button>
</form>
</div></body></html>"#,
        redirect_uri = escape_html(redirect_uri),
        state = escape_html(state),
        code_challenge = escape_html(code_challenge),
        csrf_token = escape_html(csrf_token),
        session_id = escape_html(session_id),
        repo_root = escape_html(repo_root),
    )
}

// ---------------------------------------------------------------------------
// Differential-test support surface
// ---------------------------------------------------------------------------
//
// Thin re-exports of the byte-exact JWT/PKCE/redirect primitives so the external
// differential harness (tests/differential) can drive the SAME port code the
// server uses and byte-compare it against the Node original. These are wrappers
// only — they add no behavior, just visibility. Kept `#[doc(hidden)]` so they do
// not widen the public docs.
pub mod difftest {
    use super::*;

    /// `signJwt(payload, secret)` over a pre-serialized compact JSON body.
    #[doc(hidden)]
    pub fn sign_jwt(payload_json: &str, secret: &str) -> String {
        super::sign_jwt(payload_json, secret)
    }

    /// `base64UrlEncode` (no padding, URL-safe alphabet).
    #[doc(hidden)]
    pub fn base64_url_encode(buf: &[u8]) -> String {
        super::base64_url_encode(buf)
    }

    /// PKCE S256 challenge: `base64url(SHA256(verifier))`.
    #[doc(hidden)]
    pub fn pkce_s256_challenge(verifier: &str) -> String {
        let hash = Sha256::digest(verifier.as_bytes());
        super::base64_url_encode(&hash)
    }

    /// `isValidRedirectUri(uri, baseUrl)`.
    #[doc(hidden)]
    pub fn is_valid_redirect_uri(uri: &str, base_url: &str) -> bool {
        super::is_valid_redirect_uri(uri, base_url)
    }

    /// `isBothChatGPT(uri1, uri2)`.
    #[doc(hidden)]
    pub fn is_both_chatgpt(uri1: &str, uri2: &str) -> bool {
        super::is_both_chatgpt(uri1, uri2)
    }

    /// `validateAccessToken(token, secret)`.
    #[doc(hidden)]
    pub fn validate_access_token(token: &str, secret: &str) -> bool {
        super::validate_access_token(token, secret)
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64url_roundtrip_matches_node() {
        // Known vectors: Node base64url of these byte strings.
        assert_eq!(base64_url_encode(b""), "");
        assert_eq!(base64_url_encode(b"f"), "Zg");
        assert_eq!(base64_url_encode(b"fo"), "Zm8");
        assert_eq!(base64_url_encode(b"foo"), "Zm9v");
        assert_eq!(base64_url_encode(b"foob"), "Zm9vYg");
        assert_eq!(base64_url_encode(b"fooba"), "Zm9vYmE");
        assert_eq!(base64_url_encode(b"foobar"), "Zm9vYmFy");
        // URL-safe alphabet: bytes that produce + and / in std base64.
        assert_eq!(base64_url_encode(&[0xfb, 0xff, 0xfe]), "-__-");
        for v in [&b""[..], b"f", b"fo", b"foo", b"foob", b"hello world!"] {
            assert_eq!(base64_url_decode(&base64_url_encode(v)), v);
        }
    }

    #[test]
    fn jwt_sign_verify_roundtrip() {
        let secret = "s3cr3t";
        let payload = claims_to_json(&[
            Claim::Str("sub", "user".into()),
            Claim::Str("scope", "mcp".into()),
            Claim::Num("iat", now_s()),
            Claim::Num("exp", now_s() + 3600),
        ]);
        // Claim order is preserved exactly (not alphabetized).
        assert!(payload.starts_with(r#"{"sub":"user","scope":"mcp","iat":"#));
        let token = sign_jwt(&payload, secret);
        let verified = verify_jwt(&token, secret).expect("verifies");
        assert_eq!(verified.get("scope").unwrap().as_str(), Some("mcp"));
        // Tampered signature fails.
        assert!(verify_jwt(&format!("{token}x"), secret).is_none());
        // Wrong secret fails.
        assert!(verify_jwt(&token, "other").is_none());
        assert!(validate_access_token(&token, secret));
    }

    #[test]
    fn jwt_matches_node_byte_for_byte() {
        // Verify the full token string equals what the Node implementation
        // produces for a fixed payload + secret. Computed independently:
        //   header = base64url('{"alg":"HS256","typ":"JWT"}')
        //   body   = base64url('{"sub":"user","scope":"mcp","iat":1700000000,"exp":1700003600}')
        //   sig    = base64url(HMAC_SHA256(secret, header + "." + body))
        let payload = claims_to_json(&[
            Claim::Str("sub", "user".into()),
            Claim::Str("scope", "mcp".into()),
            Claim::Num("iat", 1_700_000_000),
            Claim::Num("exp", 1_700_003_600),
        ]);
        assert_eq!(
            payload,
            r#"{"sub":"user","scope":"mcp","iat":1700000000,"exp":1700003600}"#
        );
        let token = sign_jwt(&payload, "test-secret");
        let parts: Vec<&str> = token.split('.').collect();
        assert_eq!(parts.len(), 3);
        assert_eq!(parts[0], "eyJhbGciOiJIUzI1NiIsInR5cCI6IkpXVCJ9");
        assert_eq!(
            parts[1],
            "eyJzdWIiOiJ1c2VyIiwic2NvcGUiOiJtY3AiLCJpYXQiOjE3MDAwMDAwMDAsImV4cCI6MTcwMDAwMzYwMH0"
        );
        // Signature recomputed with hmac to confirm the wiring (self-consistent).
        let expected_sig = base64_url_encode(&hmac_sha256(b"test-secret", parts[0..2].join(".").as_bytes()));
        assert_eq!(parts[2], expected_sig);
    }

    #[test]
    fn jwt_header_is_canonical() {
        // The HS256/JWT header must encode to this exact base64url string so
        // tokens byte-match the Node output.
        assert_eq!(
            base64_url_encode(br#"{"alg":"HS256","typ":"JWT"}"#),
            "eyJhbGciOiJIUzI1NiIsInR5cCI6IkpXVCJ9"
        );
    }

    #[test]
    fn pkce_s256_challenge() {
        // RFC 7636 worked example verifier/challenge pair.
        let verifier = "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk";
        let hash = Sha256::digest(verifier.as_bytes());
        assert_eq!(
            base64_url_encode(&hash),
            "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"
        );
    }

    #[test]
    fn redirect_allowlist() {
        let base = "https://example.cloudflare.dev";
        assert!(is_valid_redirect_uri("https://chatgpt.com/cb", base));
        assert!(is_valid_redirect_uri("https://sub.openai.com/cb", base));
        assert!(is_valid_redirect_uri("https://example.cloudflare.dev/cb", base));
        // localhost blocked in prod.
        assert!(!is_valid_redirect_uri("https://localhost/cb", base));
        // http blocked in prod.
        assert!(!is_valid_redirect_uri("http://chatgpt.com/cb", base));
        // arbitrary domain blocked.
        assert!(!is_valid_redirect_uri("https://evil.com/cb", base));
        // Dev (http base) allows localhost + http.
        let dev = "http://localhost:8787";
        assert!(is_valid_redirect_uri("http://localhost:8787/cb", dev));
    }

    #[test]
    fn both_chatgpt_lenient_match() {
        assert!(is_both_chatgpt(
            "https://chatgpt.com/a",
            "https://openai.com/b"
        ));
        assert!(!is_both_chatgpt(
            "https://chatgpt.com/a",
            "https://evil.com/b"
        ));
    }

    #[tokio::test]
    async fn armed_nonce_is_single_use() {
        let oauth = OAuth::new("https://h.dev", "secret", "client", None, "/repo");
        oauth.arm_consent("abc", 90).await;
        assert!(oauth.consume_armed_nonce("abc").await);
        // Second use burns nothing left.
        assert!(!oauth.consume_armed_nonce("abc").await);
        // Empty nonce never matches.
        assert!(!oauth.consume_armed_nonce("").await);
    }

    #[tokio::test]
    async fn rate_limit_caps_at_max() {
        let oauth = OAuth::new("https://h.dev", "secret", "client", None, "/repo");
        for _ in 0..RATE_LIMIT_MAX {
            assert!(oauth.check_rate_limit("k").await);
        }
        assert!(!oauth.check_rate_limit("k").await);
    }

    #[test]
    fn signing_key_defaults_to_secret() {
        let oauth = OAuth::new("https://h.dev", "thesecret", "client", None, "/repo");
        assert_eq!(oauth.inner.signing_key, "thesecret");
        let oauth2 = OAuth::new("https://h.dev", "thesecret", "client", Some("k".into()), "/repo");
        assert_eq!(oauth2.inner.signing_key, "k");
        // Empty signing key falls back to secret.
        let oauth3 = OAuth::new("https://h.dev", "thesecret", "client", Some(String::new()), "/repo");
        assert_eq!(oauth3.inner.signing_key, "thesecret");
    }
}
