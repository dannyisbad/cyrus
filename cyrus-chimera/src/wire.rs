//! Assembly glue: concrete adapters wiring the ported modules together.
//!
//! Source: repo-agent-mcp/src/index.ts (private original)
//!         (`runHttp()` construction order + `runStdio()`)
//!
//! `http.rs` owns the routing/auth/tunnel semantics and consumes three trait
//! objects ([`StateAccess`], [`OAuthProvider`], [`McpDispatch`]). This module
//! provides the concrete impls over the already-ported `state` / `oauth` / `mcp`
//! modules, plus the config-view conversions (`config::RepoAgentConfig` ->
//! `state::RepoAgentConfig` / `tools::Config`) and the two transports'
//! assembly entry points used by `http::run_http` / `http::run_stdio`.
//!
//! ## Sync-over-async bridging
//!
//! The router traits are synchronous (they mirror the TS handlers, which are
//! synchronous closures over in-memory state), but the ported `RepoState` /
//! `OAuth` / MCP transport live behind tokio primitives (`tokio::sync::Mutex`,
//! async fns). The bridge here is `block_in_place` + `Handle::block_on`, which
//! is safe on the multi-threaded runtime the server always runs under
//! (`#[tokio::main]` defaults to multi-thread; the boot tests use
//! `flavor = "multi_thread"`). Do NOT drive this server from a
//! `current_thread` runtime.
//!
//! ## Session attribution
//!
//! Both task-locals exist (`http::CURRENT_SESSION` is set around the MCP
//! dispatch by the router; `mcp::SESSION` is set by the transport's
//! `run_with_session`), but the state layer never reads either: the session is
//! passed down EXPLICITLY (`McpRequest.session` ->
//! `handle_streamable_http_request(.., session)` -> tool handlers ->
//! `RepoState::event(input, session)`), which is the same capture point the TS
//! AsyncLocalStorage had. The two task-locals are therefore both bound and
//! consistent, and nothing depends on which one is read.

use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use axum::http::{HeaderMap, HeaderValue, StatusCode};
use axum::response::Response;
use serde_json::Value;
use tokio::sync::Mutex as TokioMutex;

use crate::config::{self, Config};
use crate::http::{
    AppState, HttpConfig, McpDispatch, McpRequest, OAuthProvider, OAuthRequest, StateAccess,
    ToolEventLite,
};
use crate::mcp::{self, create_repo_mcp_server, RepoMcpServer};
use crate::oauth::{self, OAuth};
use crate::register::{register_repo_tools, ToolCtx};
use crate::state::{
    self, HandbackCapsule, RepoState, SubagentJobPatch, SubagentProgress,
};
use crate::tools;

// ---------------------------------------------------------------------------
// Sync-over-async bridge.
// ---------------------------------------------------------------------------

/// Drive an async future to completion from a synchronous trait method.
///
/// Inside a runtime this is `block_in_place` + `Handle::block_on` (requires the
/// multi-thread runtime flavor — see the module docs). Outside any runtime a
/// throwaway current-thread runtime is built (only reachable from plain test
/// threads; the server itself always calls this from a worker).
pub(crate) fn bridge_block_on<F: Future>(fut: F) -> F::Output {
    match tokio::runtime::Handle::try_current() {
        Ok(handle) => tokio::task::block_in_place(move || handle.block_on(fut)),
        Err(_) => tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("build fallback tokio runtime")
            .block_on(fut),
    }
}

// ---------------------------------------------------------------------------
// Config views: config::RepoAgentConfig -> the per-module config shapes.
// ---------------------------------------------------------------------------

/// Serialize a string-enum (`SpiceLevel` / `SandboxMode` / ...) to its wire
/// string (e.g. `SandboxMode::WorkspaceWrite` -> `"workspace-write"`), reusing
/// the serde renames so the mapping can never drift from the type definition.
fn enum_str<T: serde::Serialize>(v: &T) -> String {
    serde_json::to_value(v)
        .ok()
        .and_then(|j| j.as_str().map(str::to_string))
        .unwrap_or_default()
}

/// The subset view `state::RepoState` reads (state.rs declares its own
/// `RepoAgentConfig`; this is the `From`-conversion its module docs anticipate).
pub fn state_config_view(cfg: &Config) -> state::RepoAgentConfig {
    state::RepoAgentConfig {
        root: cfg.root.clone(),
        home_root: cfg.home_root.clone(),
        current_project: cfg.current_project.clone(),
        sandbox_mode: enum_str(&cfg.sandbox_mode),
        approval_policy: enum_str(&cfg.approval_policy),
        approvals_reviewer: enum_str(&cfg.approvals_reviewer),
        writable_roots: cfg.writable_roots.clone(),
        auto_compact: state::AutoCompactConfig {
            enabled: cfg.auto_compact.enabled,
            event_soft_limit: cfg.auto_compact.event_soft_limit.max(0) as usize,
            event_hard_limit: cfg.auto_compact.event_hard_limit.max(0) as usize,
            bytes_soft_limit: cfg.auto_compact.bytes_soft_limit.max(0) as usize,
            hot_event_count: cfg.auto_compact.hot_event_count.max(0) as usize,
            hot_file_count: cfg.auto_compact.hot_file_count.max(0) as usize,
            capsule_budget_chars: cfg.auto_compact.capsule_budget_chars.max(0) as usize,
            return_capsule_every_n_events: cfg
                .auto_compact
                .return_capsule_every_n_events
                .max(0) as usize,
        },
        max_subagents: cfg.max_subagents,
        max_subagent_spawns: cfg.max_subagent_spawns,
    }
}

fn sandbox_to_tools(m: config::SandboxMode) -> tools::SandboxMode {
    match m {
        config::SandboxMode::ReadOnly => tools::SandboxMode::ReadOnly,
        config::SandboxMode::WorkspaceWrite => tools::SandboxMode::WorkspaceWrite,
        config::SandboxMode::DangerFullAccess => tools::SandboxMode::DangerFullAccess,
    }
}

fn approval_to_tools(p: config::ApprovalPolicy) -> tools::ApprovalPolicy {
    match p {
        config::ApprovalPolicy::Untrusted => tools::ApprovalPolicy::Untrusted,
        config::ApprovalPolicy::OnRequest => tools::ApprovalPolicy::OnRequest,
        config::ApprovalPolicy::Never => tools::ApprovalPolicy::Never,
    }
}

fn reviewer_to_tools(r: config::ApprovalReviewer) -> tools::ApprovalReviewer {
    match r {
        config::ApprovalReviewer::User => tools::ApprovalReviewer::User,
        config::ApprovalReviewer::AutoReview => tools::ApprovalReviewer::AutoReview,
    }
}

fn profile_to_tools(p: &config::PermissionProfile) -> tools::PermissionProfile {
    tools::PermissionProfile {
        sandbox_mode: p.sandbox_mode.map(sandbox_to_tools),
        approval_policy: p.approval_policy.map(approval_to_tools),
        reviewer: p.reviewer.map(reviewer_to_tools),
        writable_roots: p.writable_roots.clone(),
        command_allow_prefixes: p.command_allow_prefixes.clone(),
        command_prompt_prefixes: p.command_prompt_prefixes.clone(),
    }
}

/// The mutable config view the tool surface holds (`tools::Config`), populated
/// from the loaded `config::RepoAgentConfig` instead of `Config::with_root`'s
/// conservative defaults.
pub fn tools_config_view(cfg: &Config) -> tools::Config {
    tools::Config {
        root: PathBuf::from(&cfg.root),
        home_root: PathBuf::from(&cfg.home_root),
        current_project: cfg.current_project.clone(),
        project_search_roots: cfg.project_search_roots.iter().map(PathBuf::from).collect(),
        projects: cfg
            .projects
            .iter()
            .map(|p| tools::ProjectConfig {
                name: p.name.clone(),
                root: PathBuf::from(&p.root),
                description: p.description.clone(),
                tags: p.tags.clone(),
            })
            .collect(),
        // JS number -> integer depth: NaN -> 0, otherwise saturating cast.
        max_project_scan_depth: cfg.max_project_scan_depth as i64,
        spice_level: enum_str(&cfg.spice_level),
        allow_model_write_file: cfg.allow_model_write_file,
        allow_model_dev_shell: cfg.allow_model_dev_shell,
        allow_secrets_read: cfg.allow_secrets_read,
        allow_hidden_files: cfg.allow_hidden_files,
        hide_hidden_dirs: cfg.hide_hidden_dirs,
        sandbox_mode: sandbox_to_tools(cfg.sandbox_mode),
        approval_policy: approval_to_tools(cfg.approval_policy),
        approvals_reviewer: reviewer_to_tools(cfg.approvals_reviewer),
        writable_roots: cfg.writable_roots.iter().map(PathBuf::from).collect(),
        command_allow_prefixes: cfg.command_allow_prefixes.clone(),
        command_prompt_prefixes: cfg.command_prompt_prefixes.clone(),
        permission_profiles: cfg
            .permission_profiles
            .iter()
            .map(|(k, v)| (k.clone(), profile_to_tools(v)))
            .collect(),
        max_read_bytes: cfg.max_read_bytes.max(0) as usize,
        max_write_bytes: cfg.max_write_bytes.max(0) as usize,
        max_command_output_bytes: cfg.max_command_output_bytes.max(0) as usize,
        default_command_timeout_ms: cfg.default_command_timeout_ms.max(0) as u64,
        blocked_path_globs: cfg.blocked_path_globs.clone(),
        command_profiles: cfg.command_profiles.clone(),
        command_deny_regex: cfg.command_deny_regex.clone(),
        env_passthrough: cfg.env_passthrough.clone(),
    }
}

/// The `(config as { jwtSigningKey?: string }).jwtSigningKey` read in runHttp.
///
/// The TS merged object spreads `...fromFile`, so an (untyped) `jwtSigningKey`
/// key in repo-agent.config.json survives onto the config. The Rust
/// `RepoAgentConfig` struct has no such field, so we re-read it from the same
/// config file `loadConfig()` used (same resolution order: `--config` argv,
/// `REPO_AGENT_CONFIG` env, `<root>/repo-agent.config.json`). JS `||`
/// truthiness: only a non-empty string counts.
pub fn config_file_jwt_signing_key(cfg: &Config) -> Option<String> {
    let args: Vec<String> = std::env::args().collect();
    let path = arg_value(&args, "--config")
        .or_else(|| std::env::var("REPO_AGENT_CONFIG").ok())
        .map(PathBuf::from)
        .unwrap_or_else(|| Path::new(&cfg.root).join("repo-agent.config.json"));
    let raw = std::fs::read_to_string(path).ok()?;
    let v: Value = serde_json::from_str(&raw).ok()?;
    match v.get("jwtSigningKey") {
        Some(Value::String(s)) if !s.is_empty() => Some(s.clone()),
        _ => None,
    }
}

/// `argValue(name)` — `--name <value>` or `--name=value` (mirror of the private
/// helper in config.rs; duplicated here to avoid widening that module's API).
fn arg_value(args: &[String], name: &str) -> Option<String> {
    if let Some(idx) = args.iter().position(|a| a == name) {
        return args.get(idx + 1).cloned();
    }
    let prefix = format!("{name}=");
    args.iter()
        .find(|a| a.starts_with(&prefix))
        .map(|a| a[prefix.len()..].to_string())
}

// ---------------------------------------------------------------------------
// StateAccess adapter.
// ---------------------------------------------------------------------------

/// [`StateAccess`] over the shared `Arc<tokio::sync::Mutex<RepoState>>` (the
/// SAME handle the tool handlers hold via [`ToolCtx`], so events emitted by
/// tools are visible on `/events` / `/snapshot`).
pub struct StateBridge {
    state: Arc<TokioMutex<RepoState>>,
}

impl StateBridge {
    pub fn new(state: Arc<TokioMutex<RepoState>>) -> Self {
        StateBridge { state }
    }

    /// Take the state lock from a synchronous trait method (see module docs for
    /// the runtime-flavor requirement).
    fn lock(&self) -> tokio::sync::MutexGuard<'_, RepoState> {
        match tokio::runtime::Handle::try_current() {
            Ok(_) => tokio::task::block_in_place(|| self.state.blocking_lock()),
            Err(_) => self.state.blocking_lock(),
        }
    }
}

impl StateAccess for StateBridge {
    fn recent_events_since(&self, since_seq: i64, agent: Option<&str>) -> Vec<ToolEventLite> {
        // seqs start at 1, so a negative cursor (`Number(...)` weirdness in the
        // TS) is equivalent to 0.
        let since = since_seq.max(0) as u64;
        self.lock()
            .recent_events_since(since, agent)
            .iter()
            .map(|e| ToolEventLite(serde_json::to_value(e).unwrap_or(Value::Null)))
            .collect()
    }

    fn snapshot(&self) -> Value {
        self.lock().snapshot()
    }

    fn list_jobs(&self) -> Value {
        serde_json::to_value(self.lock().subagents.list_jobs()).unwrap_or(Value::Null)
    }

    fn unbound_sessions(&self) -> Value {
        serde_json::to_value(self.lock().unbound_sessions()).unwrap_or(Value::Null)
    }

    fn bound_agents(&self) -> Value {
        serde_json::to_value(self.lock().bound_agents()).unwrap_or(Value::Null)
    }

    fn set_subagent_result(&self, agent_id: &str, capsule: Value) {
        // index.ts: state.subagents.setResult(agentId, capsule);
        //           state.releaseLeasesForAgent(agentId);
        let capsule = capsule_from_value(agent_id, capsule);
        let mut st = self.lock();
        st.subagents.set_result(agent_id, capsule);
        st.release_leases_for_agent(agent_id);
    }

    fn update_subagent_job(&self, agent_id: &str, patch: Value) -> bool {
        let patch = job_patch_from_value(&patch);
        self.lock().subagents.update_job(agent_id, patch).is_some()
    }

    fn restamp_agent(&self, session: &str, agent_id: &str) {
        self.lock().restamp_agent(session, agent_id);
    }
}

/// Deserialize a control-plane capsule. Strict serde first (the lipsync harness
/// sends complete `HandbackCapsule`s); on shape mismatch fall back to a lenient
/// field-by-field read so a partial capsule still lands instead of being
/// silently dropped (the TS stores whatever object it was handed).
fn capsule_from_value(agent_id: &str, v: Value) -> HandbackCapsule {
    if let Ok(c) = serde_json::from_value::<HandbackCapsule>(v.clone()) {
        return c;
    }
    let str_or = |key: &str, default: &str| -> String {
        v.get(key)
            .and_then(Value::as_str)
            .unwrap_or(default)
            .to_string()
    };
    HandbackCapsule {
        agent_id: str_or("agentId", agent_id),
        status: str_or("status", "done"),
        summary: str_or("summary", ""),
        files_touched: str_vec(v.get("filesTouched")),
        bg_ids: str_vec(v.get("bgIds")),
        duration_ms: v.get("durationMs").and_then(Value::as_u64).unwrap_or(0),
    }
}

fn str_vec(v: Option<&Value>) -> Vec<String> {
    v.and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(|x| x.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default()
}

/// Convert a JSON control-plane patch into [`SubagentJobPatch`] — mirrors the TS
/// `{ ...job, ...patch }` spread: an ABSENT key leaves the field unchanged; a
/// present key (including an explicit `null` for the nullable fields) replaces
/// it.
fn job_patch_from_value(patch: &Value) -> SubagentJobPatch {
    let mut p = SubagentJobPatch::default();
    let Some(obj) = patch.as_object() else {
        return p;
    };
    let get_str = |key: &str| obj.get(key).and_then(Value::as_str).map(String::from);

    if let Some(v) = get_str("parentAgentId") {
        p.parent_agent_id = Some(v);
    }
    if let Some(v) = get_str("label") {
        p.label = Some(v);
    }
    if let Some(v) = get_str("task") {
        p.task = Some(v);
    }
    if obj.contains_key("scopePaths") {
        p.scope_paths = Some(str_vec(obj.get("scopePaths")));
    }
    if let Some(v) = get_str("status") {
        p.status = Some(v);
    }
    if let Some(v) = get_str("createdTs") {
        p.created_ts = Some(v);
    }
    if obj.contains_key("lastHeartbeatTs") {
        p.last_heartbeat_ts = Some(get_str("lastHeartbeatTs"));
    }
    if obj.contains_key("targetId") {
        p.target_id = Some(get_str("targetId"));
    }
    if let Some(v) = obj.get("progress") {
        p.progress = Some(serde_json::from_value::<SubagentProgress>(v.clone()).ok());
    }
    if let Some(v) = obj.get("result") {
        p.result = Some(serde_json::from_value::<HandbackCapsule>(v.clone()).ok());
    }
    if let Some(v) = obj.get("collected").and_then(Value::as_bool) {
        p.collected = Some(v);
    }
    if obj.contains_key("leaseIds") {
        p.lease_ids = Some(str_vec(obj.get("leaseIds")));
    }
    if obj.contains_key("model") {
        p.model = Some(get_str("model"));
    }
    if obj.contains_key("effort") {
        p.effort = Some(get_str("effort"));
    }
    p
}

// ---------------------------------------------------------------------------
// OAuthProvider adapter.
// ---------------------------------------------------------------------------

/// [`OAuthProvider`] over the ported [`OAuth`] handler bundle, adapting the
/// router's [`OAuthRequest`] shape onto oauth.rs's per-endpoint signatures.
pub struct OAuthBridge {
    oauth: OAuth,
}

impl OAuthBridge {
    pub fn new(oauth: OAuth) -> Self {
        OAuthBridge { oauth }
    }

    /// `getClientIp(req)` — first x-forwarded-for hop, else the socket peer.
    fn ip_of(req: &OAuthRequest) -> String {
        let peer = if req.peer_ip.is_empty() {
            None
        } else {
            Some(req.peer_ip.as_str())
        };
        oauth::client_ip(&req.headers, peer)
    }

    fn body_str(req: &OAuthRequest) -> String {
        String::from_utf8_lossy(&req.body).into_owned()
    }
}

impl OAuthProvider for OAuthBridge {
    fn validate_token(&self, token: &str) -> bool {
        self.oauth.validate_token(token)
    }

    fn arm_consent(&self, nonce: &str, ttl_sec: u64) {
        bridge_block_on(self.oauth.arm_consent(nonce, ttl_sec));
    }

    fn protected_resource_metadata(&self, req: &OAuthRequest) -> Response {
        self.oauth.protected_resource_metadata(req.uri.path())
    }

    fn authorization_server_metadata(&self, _req: &OAuthRequest) -> Response {
        self.oauth.authorization_server_metadata()
    }

    fn openid_configuration(&self, _req: &OAuthRequest) -> Response {
        self.oauth.openid_configuration()
    }

    fn user_info(&self, req: &OAuthRequest) -> Response {
        self.oauth.user_info(&req.headers)
    }

    fn jwks(&self, _req: &OAuthRequest) -> Response {
        self.oauth.jwks()
    }

    fn authorize(&self, req: &OAuthRequest) -> Response {
        let query = req.uri.query().unwrap_or("");
        let body = Self::body_str(req);
        let ip = Self::ip_of(req);
        bridge_block_on(self.oauth.authorize(&req.method, query, &body, &ip))
    }

    fn token(&self, req: &OAuthRequest) -> Response {
        let body = Self::body_str(req);
        let ip = Self::ip_of(req);
        bridge_block_on(self.oauth.token(&req.method, &body, &ip))
    }
}

// ---------------------------------------------------------------------------
// McpDispatch adapter.
// ---------------------------------------------------------------------------

/// [`McpDispatch`] over the process-wide [`RepoMcpServer`]. Per-request
/// statelessness lives inside `handle_streamable_http_request` (one logical
/// transport per call, `sessionIdGenerator: undefined`,
/// `enableJsonResponse: false`); the session is threaded explicitly and ALSO
/// bound to `mcp::SESSION` inside the transport.
pub struct McpBridge {
    server: RepoMcpServer,
}

impl McpBridge {
    pub fn new(server: RepoMcpServer) -> Self {
        McpBridge { server }
    }
}

impl McpDispatch for McpBridge {
    fn handle_request(&self, req: McpRequest) -> Response {
        let server = self.server.clone();
        bridge_block_on(async move {
            mcp::handle_streamable_http_request(
                &server,
                req.method.as_str(),
                &req.headers,
                &req.body,
                req.session.clone(),
            )
            .await
        })
    }
}

// ---------------------------------------------------------------------------
// Assembly: runHttp / runStdio construction order.
// ---------------------------------------------------------------------------

/// `new RepoState(config)` + `createRepoMcpServer(config, state)`: build the
/// shared state handle, the tool context, and the registered MCP server.
pub fn build_runtime(cfg: &Config) -> std::io::Result<(Arc<TokioMutex<RepoState>>, RepoMcpServer)> {
    let repo_state = RepoState::new(state_config_view(cfg))?;
    let state = Arc::new(TokioMutex::new(repo_state));
    let tools_config = Arc::new(TokioMutex::new(tools_config_view(cfg)));
    let ctx = ToolCtx::new(state.clone(), tools_config);
    let server = create_repo_mcp_server(|b| register_repo_tools(b, &ctx));
    Ok((state, server))
}

/// Build the fully-wired [`AppState`] for [`crate::http::serve`], mirroring
/// `runHttp()`'s construction order: state -> jwtSigningKey -> `oauth` iff
/// `publicUrl && bearerToken` -> server.
///
/// Must be called from within a tokio runtime (the OAuth cleanup task is
/// spawned here, standing in for the TS `setInterval`).
pub fn build_app_state(cfg: &Config) -> anyhow::Result<AppState> {
    let http_cfg = HttpConfig::from_config(cfg);
    let (state, server) = build_runtime(cfg)?;

    // `const oauth = config.publicUrl && config.bearerToken ? createOAuthHandlers(...)
    //  : undefined;` — both must be truthy (HttpConfig::from_config already
    // filtered empty strings to None, matching JS truthiness).
    let oauth: Option<Arc<dyn OAuthProvider>> =
        match (http_cfg.public_url.as_deref(), http_cfg.bearer_token.as_deref()) {
            (Some(public_url), Some(bearer)) => {
                let o = OAuth::new(
                    public_url,
                    bearer,
                    "repo-agent-mcp-client",
                    http_cfg.jwt_signing_key.clone(),
                    cfg.root.clone(),
                );
                o.spawn_cleanup();
                Some(Arc::new(OAuthBridge::new(o)))
            }
            _ => None,
        };

    Ok(AppState {
        config: Arc::new(http_cfg),
        state: Arc::new(StateBridge::new(state)),
        oauth,
        mcp: Arc::new(McpBridge::new(server)),
    })
}

// ---------------------------------------------------------------------------
// Stdio transport (runStdio).
// ---------------------------------------------------------------------------

/// Dispatch one newline-delimited JSON-RPC line against the server, returning
/// the JSON-RPC responses to write back (possibly empty for notifications).
///
/// Reuses the SAME message dispatch as the HTTP transport by synthesizing a
/// stateless POST (correct Accept/Content-Type) and unwrapping the SSE frames —
/// so stdio and HTTP can never drift. Stdio carries no per-conversation
/// `x-openai-session`, so the session is `None` (events attribute to "main"),
/// exactly like the TS `StdioServerTransport` path.
pub async fn dispatch_stdio_line(server: &RepoMcpServer, line: &str) -> Vec<Value> {
    let mut headers = HeaderMap::new();
    headers.insert(
        "accept",
        HeaderValue::from_static("application/json, text/event-stream"),
    );
    headers.insert("content-type", HeaderValue::from_static("application/json"));

    let resp =
        mcp::handle_streamable_http_request(server, "POST", &headers, line.as_bytes(), None).await;

    let status = resp.status();
    let is_sse = resp
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.contains("text/event-stream"))
        .unwrap_or(false);
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap_or_default();

    // Notification-only input -> 202 with no body: nothing to write back.
    if status == StatusCode::ACCEPTED {
        return Vec::new();
    }
    if is_sse {
        // One `event: message\ndata: <json>` frame per JSON-RPC response.
        String::from_utf8_lossy(&bytes)
            .lines()
            .filter_map(|l| l.strip_prefix("data: "))
            .filter_map(|d| serde_json::from_str::<Value>(d).ok())
            .collect()
    } else {
        // Transport-level JSON error envelope (parse error / invalid request):
        // surface it as the JSON-RPC error object it already is.
        serde_json::from_slice::<Value>(&bytes).ok().into_iter().collect()
    }
}

/// `runStdio()`'s transport loop: newline-delimited JSON-RPC over
/// stdin/stdout, like the SDK's `StdioServerTransport` (which reads
/// `\n`-delimited messages and writes `JSON.stringify(msg) + "\n"`).
pub async fn run_stdio_transport(server: RepoMcpServer) -> anyhow::Result<()> {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

    let stdin = tokio::io::stdin();
    let mut lines = BufReader::new(stdin).lines();
    let mut stdout = tokio::io::stdout();

    while let Some(line) = lines.next_line().await? {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        for response in dispatch_stdio_line(&server, trimmed).await {
            let text = serde_json::to_string(&response)?;
            stdout.write_all(text.as_bytes()).await?;
            stdout.write_all(b"\n").await?;
            stdout.flush().await?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn job_patch_maps_present_keys_only() {
        let p = job_patch_from_value(&json!({
            "status": "running",
            "lastHeartbeatTs": null,
            "progress": { "turns": 3, "lastSummary": "working" },
            "leaseIds": ["l1", "l2"],
        }));
        assert_eq!(p.status.as_deref(), Some("running"));
        // Present-null -> explicit clear (Some(None)).
        assert_eq!(p.last_heartbeat_ts, Some(None));
        let progress = p.progress.expect("present").expect("parsed");
        assert_eq!(progress.turns, 3);
        assert_eq!(p.lease_ids, Some(vec!["l1".to_string(), "l2".to_string()]));
        // Absent keys stay unchanged.
        assert!(p.label.is_none());
        assert!(p.task.is_none());
        assert!(p.collected.is_none());
    }

    #[test]
    fn capsule_strict_then_lenient() {
        // Full capsule deserializes strictly.
        let full = capsule_from_value(
            "a1",
            json!({
                "agentId": "a1", "status": "done", "summary": "s",
                "filesTouched": ["f"], "bgIds": [], "durationMs": 5
            }),
        );
        assert_eq!(full.status, "done");
        assert_eq!(full.files_touched, vec!["f".to_string()]);
        // Partial capsule falls back leniently, defaulting the agent id.
        let partial = capsule_from_value("a9", json!({ "summary": "only summary" }));
        assert_eq!(partial.agent_id, "a9");
        assert_eq!(partial.summary, "only summary");
        assert_eq!(partial.duration_ms, 0);
    }

    #[test]
    fn enum_str_uses_serde_renames() {
        assert_eq!(enum_str(&config::SpiceLevel::Spicy), "spicy");
        assert_eq!(enum_str(&config::SandboxMode::WorkspaceWrite), "workspace-write");
        assert_eq!(enum_str(&config::ApprovalPolicy::OnRequest), "on-request");
        assert_eq!(enum_str(&config::ApprovalReviewer::AutoReview), "auto_review");
    }
}
