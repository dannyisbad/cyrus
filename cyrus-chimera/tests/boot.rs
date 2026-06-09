//! Boot smoke tests: assemble the REAL server (config -> state -> tools ->
//! MCP server -> oauth -> router) and exercise it over actual TCP on an
//! ephemeral port, asserting the behaviors verified live against the Node
//! original (2026-06-10).
//!
//! Original behaviors under test: repo-agent-mcp/src/index.ts (private original)
//!
//! Multi-thread runtime flavor is REQUIRED: the wire-layer adapters bridge the
//! sync router traits onto the async state/oauth/mcp modules with
//! `block_in_place`.

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};

use cyrus_chimera::config::{
    ApprovalPolicy, ApprovalReviewer, AutoCompactConfig, RepoAgentConfig, SandboxMode, SpiceLevel,
};
use cyrus_chimera::{http, wire};
use serde_json::{json, Value};

const BEARER: &str = "test-bearer-token-1234";
const PUBLIC_URL: &str = "https://example.test";

// ---------------------------------------------------------------------------
// Harness
// ---------------------------------------------------------------------------

fn temp_root(tag: &str) -> PathBuf {
    let root = std::env::temp_dir().join(format!(
        "cyrus-chimera-boot-{tag}-{}-{}",
        std::process::id(),
        uuid::Uuid::new_v4()
    ));
    std::fs::create_dir_all(&root).expect("create temp repo root");
    root
}

fn test_config(root: &Path, bearer: Option<&str>, public_url: Option<&str>) -> RepoAgentConfig {
    let root_str = root.to_string_lossy().to_string();
    RepoAgentConfig {
        root: root_str.clone(),
        home_root: root_str.clone(),
        current_project: None,
        project_search_roots: vec![root_str],
        projects: vec![],
        max_project_scan_depth: 3.0,
        port: 0.0, // unused: the harness binds its own ephemeral listener
        host: "127.0.0.1".to_string(),
        bearer_token: bearer.map(str::to_string),
        public_url: public_url.map(str::to_string),
        spice_level: SpiceLevel::Spicy,
        allow_model_write_file: true,
        allow_model_dev_shell: true,
        allow_secrets_read: false,
        allow_hidden_files: true,
        hide_hidden_dirs: true,
        sandbox_mode: SandboxMode::WorkspaceWrite,
        approval_policy: ApprovalPolicy::OnRequest,
        approvals_reviewer: ApprovalReviewer::User,
        writable_roots: vec![],
        command_allow_prefixes: vec![],
        command_prompt_prefixes: vec![],
        permission_profiles: BTreeMap::new(),
        max_read_bytes: 180_000,
        max_write_bytes: 350_000,
        max_command_output_bytes: 160_000,
        default_command_timeout_ms: 120_000,
        blocked_path_globs: vec![],
        command_profiles: BTreeMap::new(),
        command_deny_regex: vec![],
        env_passthrough: vec![],
        auto_compact: AutoCompactConfig {
            enabled: false,
            event_soft_limit: 28,
            event_hard_limit: 64,
            bytes_soft_limit: 220_000,
            hot_event_count: 12,
            hot_file_count: 12,
            capsule_budget_chars: 12_000,
            return_capsule_every_n_events: 10,
        },
        max_subagents: 2.0,
        max_subagent_spawns: 12.0,
        subagent_idle_timeout_ms: 90_000.0,
    }
}

/// Boot the real assembly (the same `AppState`/router `run_http` serves) on an
/// ephemeral loopback port; returns the base URL.
async fn boot(cfg: RepoAgentConfig) -> String {
    let app = wire::build_app_state(&cfg).expect("build AppState");
    let router = http::build_router(app);
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
        .await
        .expect("bind ephemeral port");
    let addr = listener.local_addr().expect("local addr");
    tokio::spawn(async move {
        let _ = axum::serve(
            listener,
            router.into_make_service_with_connect_info::<SocketAddr>(),
        )
        .await;
    });
    format!("http://{addr}")
}

async fn boot_open(tag: &str) -> String {
    boot(test_config(&temp_root(tag), None, None)).await
}

async fn boot_secured(tag: &str) -> String {
    boot(test_config(&temp_root(tag), Some(BEARER), Some(PUBLIC_URL))).await
}

fn client() -> reqwest::Client {
    reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .expect("reqwest client")
}

/// Parse the JSON payloads out of an MCP `text/event-stream` body
/// (`event: message\ndata: <json>\n\n` frames).
fn sse_data(body: &str) -> Vec<Value> {
    body.lines()
        .filter_map(|l| l.strip_prefix("data: "))
        .filter_map(|d| serde_json::from_str(d).ok())
        .collect()
}

async fn mcp_post(
    client: &reqwest::Client,
    base: &str,
    bearer: Option<&str>,
    message: &Value,
) -> reqwest::Response {
    let mut req = client
        .post(format!("{base}/mcp"))
        .header("accept", "application/json, text/event-stream")
        .header("content-type", "application/json")
        .body(message.to_string());
    if let Some(b) = bearer {
        req = req.header("authorization", format!("Bearer {b}"));
    }
    req.send().await.expect("POST /mcp")
}

// ---------------------------------------------------------------------------
// a. GET / responds with the server-info JSON.
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn root_status_route_responds() {
    let base = boot_open("root").await;
    let resp = client().get(&base).send().await.expect("GET /");
    assert_eq!(resp.status(), 200);
    let info: Value = resp.json().await.expect("info json");
    assert_eq!(info["name"], json!("repo-agent-mcp"));
    assert_eq!(info["ok"], json!(true));
    assert_eq!(info["mcp"], json!("/mcp"));
    assert_eq!(info["oauth"], json!("disabled"));

    // And with oauth configured the flag flips.
    let secured = boot_secured("root2").await;
    let info: Value = client()
        .get(&secured)
        .send()
        .await
        .expect("GET /")
        .json()
        .await
        .expect("json");
    assert_eq!(info["oauth"], json!("enabled"));
}

// ---------------------------------------------------------------------------
// b. Unauthenticated /mcp with a bearer configured -> 401 + WWW-Authenticate
//    carrying resource_metadata.
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn mcp_unauthenticated_is_401_with_resource_metadata_challenge() {
    let base = boot_secured("401").await;
    let resp = client()
        .get(format!("{base}/mcp"))
        .send()
        .await
        .expect("GET /mcp");
    assert_eq!(resp.status(), 401);
    let challenge = resp
        .headers()
        .get("www-authenticate")
        .expect("WWW-Authenticate header present")
        .to_str()
        .expect("header is ascii");
    assert!(
        challenge.contains(&format!(
            "resource_metadata=\"{PUBLIC_URL}/.well-known/oauth-protected-resource\""
        )),
        "challenge missing resource_metadata: {challenge}"
    );
    assert!(challenge.contains("error=\"invalid_token\""));
}

// ---------------------------------------------------------------------------
// c. /mcp via tunnel (cf-connecting-ip) with NO bearer configured -> 403
//    (fail-closed).
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn mcp_via_tunnel_without_bearer_fails_closed_403() {
    let base = boot_open("403").await;
    let resp = client()
        .post(format!("{base}/mcp"))
        .header("accept", "application/json, text/event-stream")
        .header("content-type", "application/json")
        .header("cf-connecting-ip", "203.0.113.7")
        .body(json!({"jsonrpc":"2.0","id":1,"method":"ping"}).to_string())
        .send()
        .await
        .expect("POST /mcp via tunnel");
    assert_eq!(resp.status(), 403);
    let body = resp.text().await.expect("text");
    assert!(body.contains("Refusing to serve MCP over a public tunnel"));
}

// ---------------------------------------------------------------------------
// d. Loopback-only surfaces: 404 through the tunnel, alive from loopback.
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn loopback_surfaces_404_through_tunnel_but_alive_locally() {
    let base = boot_open("tunnel").await;
    let c = client();

    for path in ["/events", "/snapshot", "/control/sessions", "/control/anything"] {
        let resp = c
            .get(format!("{base}{path}"))
            .header("cf-connecting-ip", "203.0.113.7")
            .send()
            .await
            .expect("tunneled GET");
        assert_eq!(resp.status(), 404, "{path} must 404 through the tunnel");
    }

    // Same paths from loopback (no CF header) are NOT 404.
    let snapshot = c
        .get(format!("{base}/snapshot"))
        .send()
        .await
        .expect("GET /snapshot");
    assert_eq!(snapshot.status(), 200);
    let snap: Value = snapshot.json().await.expect("snapshot json");
    assert!(snap.get("permissions").is_some());

    let events = c
        .get(format!("{base}/events"))
        .send()
        .await
        .expect("GET /events");
    assert_eq!(events.status(), 200);
    assert_eq!(
        events
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok()),
        Some("text/event-stream")
    );
    drop(events); // disconnect the SSE tail

    // No bearer configured -> auth is open locally; the control plane answers.
    let sessions = c
        .get(format!("{base}/control/sessions"))
        .send()
        .await
        .expect("GET /control/sessions");
    assert_eq!(sessions.status(), 200);
    let body: Value = sessions.json().await.expect("sessions json");
    assert!(body.get("unbound").is_some());
    assert!(body.get("bound").is_some());
}

// ---------------------------------------------------------------------------
// e. arm-consent -> auto-issued OAuth code (302 with code=); nonce is one-shot.
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn armed_nonce_auto_issues_code_exactly_once() {
    let base = boot_secured("consent").await;
    let c = client();

    let nonce = "nonce_abcdefghij12345678"; // 24 chars
    let armed = c
        .post(format!("{base}/control/arm-consent"))
        .header("authorization", format!("Bearer {BEARER}"))
        .header("content-type", "application/json")
        .body(json!({ "nonce": nonce, "ttl_sec": 90 }).to_string())
        .send()
        .await
        .expect("POST /control/arm-consent");
    assert!(
        armed.status().is_success(),
        "arm-consent must be 2xx, got {}",
        armed.status()
    );
    let armed_body: Value = armed.json().await.expect("arm json");
    assert_eq!(armed_body["ok"], json!(true));
    assert_eq!(armed_body["ttl_sec"], json!(90));

    let challenge = cyrus_chimera::oauth::difftest::pkce_s256_challenge(
        "boot-test-code-verifier-0123456789abcdefghijklmn",
    );
    let authorize_url = format!(
        "{base}/oauth/authorize?response_type=code&client_id=repo-agent-mcp-client&redirect_uri=https%3A%2F%2Fchatgpt.com%2Fconnector%2Foauth%2Fcb_test&scope=mcp&code_challenge={challenge}&code_challenge_method=S256&state=oauth_s_x&cyrus_nonce={nonce}"
    );

    // First use: the armed nonce auto-issues -> 302 with ?code=.
    let first = c
        .get(&authorize_url)
        .send()
        .await
        .expect("GET /oauth/authorize (armed)");
    assert_eq!(first.status(), 302);
    let location = first
        .headers()
        .get("location")
        .expect("Location header")
        .to_str()
        .expect("ascii");
    assert!(
        location.starts_with("https://chatgpt.com/connector/oauth/cb_test"),
        "unexpected redirect target: {location}"
    );
    assert!(location.contains("code="), "no code in: {location}");
    assert!(location.contains("state=oauth_s_x"));

    // Second use of the SAME nonce: burnt -> NOT auto-issued (password page).
    let second = c
        .get(&authorize_url)
        .send()
        .await
        .expect("GET /oauth/authorize (burnt nonce)");
    assert_eq!(second.status(), 200, "burnt nonce must not auto-issue");
    let html = second.text().await.expect("consent html");
    assert!(html.contains("Authorize Repo Agent MCP"));
}

// ---------------------------------------------------------------------------
// f. OAuth discovery metadata derives from the public URL.
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn well_known_metadata_uses_public_url() {
    let base = boot_secured("wellknown").await;
    let c = client();

    let pr: Value = c
        .get(format!("{base}/.well-known/oauth-protected-resource"))
        .send()
        .await
        .expect("GET protected-resource")
        .json()
        .await
        .expect("json");
    assert_eq!(pr["resource"], json!(PUBLIC_URL));
    assert_eq!(pr["authorization_servers"], json!([PUBLIC_URL]));
    assert_eq!(pr["scopes_supported"], json!(["mcp"]));

    let asm: Value = c
        .get(format!("{base}/.well-known/oauth-authorization-server"))
        .send()
        .await
        .expect("GET authorization-server")
        .json()
        .await
        .expect("json");
    assert_eq!(asm["issuer"], json!(PUBLIC_URL));
    assert_eq!(
        asm["authorization_endpoint"],
        json!(format!("{PUBLIC_URL}/oauth/authorize"))
    );
    assert_eq!(
        asm["token_endpoint"],
        json!(format!("{PUBLIC_URL}/oauth/token"))
    );
    assert_eq!(asm["code_challenge_methods_supported"], json!(["S256"]));
}

// ---------------------------------------------------------------------------
// g. Full MCP handshake over POST /mcp with a valid bearer.
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn mcp_handshake_lists_default_34_tools() {
    let base = boot_secured("handshake").await;
    let c = client();

    // initialize
    let init = mcp_post(
        &c,
        &base,
        Some(BEARER),
        &json!({
            "jsonrpc": "2.0", "id": 1, "method": "initialize",
            "params": {
                "protocolVersion": "2025-06-18",
                "capabilities": {},
                "clientInfo": { "name": "boot-test", "version": "0.0.1" }
            }
        }),
    )
    .await;
    assert_eq!(init.status(), 200);
    assert_eq!(
        init.headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok()),
        Some("text/event-stream")
    );
    let frames = sse_data(&init.text().await.expect("init body"));
    assert_eq!(frames.len(), 1);
    assert_eq!(frames[0]["result"]["protocolVersion"], json!("2025-06-18"));
    assert_eq!(
        frames[0]["result"]["serverInfo"]["name"],
        json!("repo-agent-mcp")
    );

    // initialized notification -> 202 Accepted, empty body.
    let inited = mcp_post(
        &c,
        &base,
        Some(BEARER),
        &json!({ "jsonrpc": "2.0", "method": "notifications/initialized" }),
    )
    .await;
    assert_eq!(inited.status(), 202);

    // tools/list -> exactly 34 tools by default (the four CHIMERA_LEGACY_SUBAGENTS
    // legacy tools stay hidden, matching the live Node server's tools/list),
    // including the codex-native trio.
    let list = mcp_post(
        &c,
        &base,
        Some(BEARER),
        &json!({ "jsonrpc": "2.0", "id": 2, "method": "tools/list" }),
    )
    .await;
    assert_eq!(list.status(), 200);
    let frames = sse_data(&list.text().await.expect("list body"));
    assert_eq!(frames.len(), 1);
    let tools = frames[0]["result"]["tools"]
        .as_array()
        .expect("tools array");
    assert_eq!(tools.len(), 34, "expected exactly 34 default-registered tools");
    let names: Vec<&str> = tools
        .iter()
        .filter_map(|t| t["name"].as_str())
        .collect();
    for want in ["shell_command", "apply_patch", "update_plan"] {
        assert!(names.contains(&want), "missing tool {want} in {names:?}");
    }
    for hidden in [
        "repo_await",
        "repo_spawn_subagent",
        "repo_subagent_kill",
        "repo_subagent_list",
    ] {
        assert!(
            !names.contains(&hidden),
            "legacy tool {hidden} must be gated off by default (live parity)"
        );
    }
}

// ---------------------------------------------------------------------------
// h. GET /mcp (SSE probe) is text/event-stream, not application/json.
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn mcp_get_probe_is_event_stream() {
    let base = boot_open("sse").await;
    let resp = client()
        .get(format!("{base}/mcp"))
        .header("accept", "text/event-stream")
        .send()
        .await
        .expect("GET /mcp");
    assert_eq!(resp.status(), 200);
    let content_type = resp
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    assert!(
        content_type.contains("text/event-stream"),
        "GET /mcp content-type must be SSE, got {content_type}"
    );
    assert!(
        !content_type.contains("application/json"),
        "GET /mcp must not be application/json"
    );
}
