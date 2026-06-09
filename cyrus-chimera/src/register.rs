//! `register_repo_tools` — the concrete MCP tool surface.
//!
//! Source: repo-agent-mcp/src/tools/register.ts (private original)
//!         (+ src/tools/harness.ts, src/tools/subagent.ts)
//!
//! This is the port of `registerRepoTools(server, config, state)`: it registers
//! every named `repo_*` / codex-native tool (name + JSON input schema + exposure
//! + handler) on a [`RepoMcpServerBuilder`]. The registration framework
//! (`register_tool`, `ToolSpec`, `ToolReply`, `_meta` synthesis) lives in
//! [`crate::mcp`]; the repo OPERATIONS (file read/write/edit/glob/grep, shell, the
//! background registry, projects, command review, permissions) live in
//! [`crate::tools`]; the durable state (events, notes, capsules, blobs, leases,
//! subagents) lives in [`crate::state`]; the four MAIN-thread subagent handlers
//! live in [`crate::subagent`].
//!
//! Threading model. The TS captured `config` + `state` in the registration
//! closures and read the request session from AsyncLocalStorage. Here each handler
//! captures a shared [`ToolCtx`] (the `state::RepoState` behind a tokio Mutex, the
//! `tools::Config` behind a tokio Mutex, and the relay endpoint) and receives the
//! request session explicitly from the dispatcher. `tools::Config` and
//! `state::RepoAgentConfig` are two separate config views in this crate; the
//! handlers mutate `tools::Config` (permissions / project switch) and mirror the
//! few fields the event/snapshot layer reads back into `RepoState.config`.
//!
//! Relay. `shell_command` / `apply_patch` / `update_plan` (and the codex-native
//! request_user_input / goals / multi-agent tools) relay to codex over the shim's
//! `/control/toolcall` long-poll when `CHIMERA_RELAY_URL` is set and the session's
//! agent-id namespace is `main` or `codex:<thread>` — exactly the three-way gate
//! from register.ts's `relayTarget`. With relay disabled (or a legacy off-codex
//! subagent), the file tools fall back to a local apply and the relay-only tools
//! return the same no-retry guidance the TS does.

use std::sync::Arc;
use std::time::Duration;

use serde_json::{json, Value};
use tokio::sync::Mutex;

use crate::mcp::{RepoMcpServerBuilder, ToolHandler, ToolReply};
use crate::state::{ApprovalInput, EventInput, RepoState};
use crate::subagent::{subagent_tool_defs, SubagentTools};
use crate::tools::{self, Config};

// ---------------------------------------------------------------------------
// Shared handler context.
// ---------------------------------------------------------------------------

/// Everything a tool handler needs: the durable state, the (mutable) config view,
/// and the relay endpoint. Cloned (Arc) into each handler closure.
#[derive(Clone)]
pub struct ToolCtx {
    pub state: Arc<Mutex<RepoState>>,
    pub config: Arc<Mutex<Config>>,
    /// `CHIMERA_RELAY_URL`, captured once at registration (the TS reads
    /// `process.env.CHIMERA_RELAY_URL` per call; the value is process-stable).
    pub relay_url: Option<String>,
    /// `CHIMERA_LEGACY_SUBAGENTS === "1"` — gates the dead-end legacy subagent
    /// rail (repo_spawn_subagent / repo_await / repo_subagent_list /
    /// repo_subagent_kill) exactly like register.ts:1200. Default off: the live
    /// server exposes 34 tools, not 38.
    pub legacy_subagents: bool,
}

impl ToolCtx {
    pub fn new(state: Arc<Mutex<RepoState>>, config: Arc<Mutex<Config>>) -> Self {
        ToolCtx {
            state,
            config,
            relay_url: std::env::var("CHIMERA_RELAY_URL").ok().filter(|s| !s.is_empty()),
            legacy_subagents: std::env::var("CHIMERA_LEGACY_SUBAGENTS").as_deref() == Ok("1"),
        }
    }
}

// ---------------------------------------------------------------------------
// result.ts shims over the mcp::ToolReply shape.
// ---------------------------------------------------------------------------

/// `textReply(ok, structured, text, meta?)` producing an [`mcp::ToolReply`].
fn text_reply(ok: bool, structured: Value, text: impl Into<String>, meta: Option<Value>) -> ToolReply {
    let mut sc = if structured.is_object() { structured } else { json!({}) };
    // `{ ok, ...structured }`: ok first, never overridden (callers never set ok).
    if let Value::Object(map) = &mut sc {
        let mut ordered = serde_json::Map::new();
        ordered.insert("ok".to_string(), Value::Bool(ok));
        for (k, v) in map.iter() {
            if k != "ok" {
                ordered.insert(k.clone(), v.clone());
            }
        }
        sc = Value::Object(ordered);
    }
    ToolReply {
        content: vec![json!({ "type": "text", "text": text.into() })],
        structured_content: sc,
        meta,
    }
}

/// `errReply(message, extra?)` => textReply(false, { error, ...extra }, "Error: …").
fn err_reply(message: impl Into<String>, extra: Value) -> ToolReply {
    let message = message.into();
    let mut structured = if extra.is_object() { extra } else { json!({}) };
    if let Value::Object(map) = &mut structured {
        map.insert("error".to_string(), Value::String(message.clone()));
    }
    text_reply(false, structured, format!("Error: {message}"), None)
}

/// `brief(s, n=240)` — collapse whitespace, trim, trimMiddle to n.
fn brief(s: &str) -> String {
    tools::brief(s, 240)
}

/// Cap error text handed back to the model at ~2000 chars (middle-trimmed) so a
/// failing subprocess can never dump a wall of stderr into the reply.
fn cap_error_text(s: &str) -> String {
    tools::trim_middle(s.trim(), 2_000).0
}

/// A short, clean message for a failed pure-git invocation. The common case —
/// the configured root is not a git repository — gets a one-liner; anything else
/// is capped instead of dumped raw.
fn git_failure_text(config: &Config, raw: &str) -> String {
    if raw.to_lowercase().contains("not a git repository") {
        format!("not a git repository: {}", config.root.to_string_lossy())
    } else {
        cap_error_text(raw)
    }
}

// ---------------------------------------------------------------------------
// _meta builders (renderMeta / appCallableMeta / modelMeta from register.ts).
// ---------------------------------------------------------------------------

const WORKBENCH_URI: &str = "ui://widget/repo-workbench.html";

/// `renderMeta(visibility)` — the repo_ui render `_meta` with the workbench
/// template + invocation strings + `ui.resourceUri`. Visibility is model+app.
fn render_meta() -> Value {
    json!({
        "ui": { "resourceUri": WORKBENCH_URI, "visibility": ["model", "app"] },
        "openai/outputTemplate": WORKBENCH_URI,
        "openai/widgetAccessible": true,
        "openai/toolInvocation/invoking": "Opening repo surface…",
        "openai/toolInvocation/invoked": "Repo surface ready",
    })
}

/// `appCallableMeta(visibility)` — `{ ui: { visibility }, openai/widgetAccessible }`.
fn app_callable_meta(visibility: &[&str]) -> Value {
    json!({
        "ui": { "visibility": visibility },
        "openai/widgetAccessible": visibility.contains(&"app"),
    })
}

/// `modelMeta()` — `{ ui: { visibility: ["model"] } }`.
fn model_meta() -> Value {
    json!({ "ui": { "visibility": ["model"] } })
}

/// Exposure equivalent of `config.allowModelWriteFile ? ["model","app"] : ["app"]`.
fn write_visibility(allow_model_write: bool) -> Vec<&'static str> {
    if allow_model_write {
        vec!["model", "app"]
    } else {
        vec!["app"]
    }
}

/// Exposure equivalent of `config.allowModelDevShell ? ["model","app"] : ["app"]`.
fn shell_visibility(allow_model_shell: bool) -> Vec<&'static str> {
    if allow_model_shell {
        vec!["model", "app"]
    } else {
        vec!["app"]
    }
}

// ---------------------------------------------------------------------------
// Handler glue: register_tool with a pre-built _meta object.
// ---------------------------------------------------------------------------

#[allow(clippy::too_many_arguments)]
fn register(
    b: &mut RepoMcpServerBuilder,
    name: &str,
    title: &str,
    description: &str,
    input_schema: Value,
    output_schema: Value,
    annotations: Value,
    meta: Value,
    handler: ToolHandler,
) {
    b.register_tool_with_meta(
        name.to_string(),
        title.to_string(),
        description.to_string(),
        Some(input_schema),
        Some(output_schema),
        Some(annotations),
        meta,
        handler,
    );
}

/// A boxed handler from an async closure over `(args, session, ctx)`. The body is
/// an expression block producing a [`ToolReply`] (it may `return` a `ToolReply`
/// early); the macro runs it and wraps the result in `Ok` for the handler type.
macro_rules! handler {
    ($ctx:ident, $args:ident, $session:ident, $body:block) => {{
        // Bind the OWNED context in the caller's hygiene context (using the
        // caller's own `$ctx` ident) so the `$body` — which spells `$ctx` — picks
        // up this owned clone, not the `&ToolCtx` function parameter. Shadowing in
        // the macro's own hygiene would not be visible to `$body`.
        let owned_ctx: ToolCtx = ToolCtx::clone($ctx);
        let h: ToolHandler = Arc::new(
            move |$args: Value, $session: Option<String>| -> ::std::pin::Pin<
                Box<dyn ::std::future::Future<Output = ::anyhow::Result<ToolReply>> + Send>,
            > {
                let $ctx: ToolCtx = owned_ctx.clone();
                Box::pin(async move {
                    let reply: ToolReply = async move {
                        let _ = &$ctx;
                        $body
                    }
                    .await;
                    Ok::<ToolReply, ::anyhow::Error>(reply)
                })
            },
        );
        h
    }};
}

// ---------------------------------------------------------------------------
// Snapshot / event helpers shared across handlers.
// ---------------------------------------------------------------------------

/// `withCompaction(state, reply, reason)` — run `maybeCompact` and stamp the
/// returned notice onto the reply's structuredContent.compaction.
fn with_compaction(state: &mut RepoState, mut reply: ToolReply, reason: &str) -> ToolReply {
    if let Some(notice) = state.maybe_compact(reason) {
        if let Value::Object(map) = &mut reply.structured_content {
            map.insert(
                "compaction".to_string(),
                serde_json::to_value(&notice).unwrap_or(Value::Null),
            );
        }
    }
    reply
}

/// `eventThen(state, input, reply, reason?)` — record the event then compact.
fn event_then(
    state: &mut RepoState,
    input: EventInput,
    session: Option<&str>,
    reply: ToolReply,
) -> ToolReply {
    state.event(input, session);
    with_compaction(state, reply, "tool-loop")
}

/// `snapshot(config, state, extra)` — the repo_ui/repo_status payload.
fn snapshot(config: &Config, state: &RepoState, extra: Value) -> Value {
    let perms = tools::effective_permissions(config);
    let mut obj = serde_json::Map::new();
    obj.insert("mode".to_string(), json!(config.spice_level));
    obj.insert(
        "repoRoot".to_string(),
        json!(config.root.to_string_lossy()),
    );
    obj.insert(
        "project".to_string(),
        json!({
            "name": config.current_project,
            "root": config.root.to_string_lossy(),
            "homeRoot": config.home_root.to_string_lossy(),
        }),
    );
    obj.insert(
        "permissions".to_string(),
        serde_json::to_value(&perms).unwrap_or(Value::Null),
    );
    obj.insert("snapshot".to_string(), state.snapshot());
    if let Value::Object(extra) = extra {
        for (k, v) in extra {
            obj.insert(k, v);
        }
    }
    Value::Object(obj)
}

const LOG_BLOB_MIN_CHARS: usize = 2_000;

fn should_keep_log(ok: bool, timed_out: bool, text: &str, truncated: bool) -> bool {
    !text.trim().is_empty() && (!ok || timed_out || truncated || text.len() >= LOG_BLOB_MIN_CHARS)
}

/// `logMeta(title, text, blob?, truncated)` — the `_meta.currentLog` block.
fn log_meta(title: &str, text: &str, blob: Option<&crate::state::BlobRef>, truncated: bool) -> Value {
    let (blob_id, bytes) = match blob {
        Some(b) => (Value::String(b.id.clone()), b.bytes),
        None => (Value::Null, text.len() as u64),
    };
    json!({
        "currentLog": {
            "title": title,
            "text": text,
            "blobId": blob_id,
            "bytes": bytes,
            "truncated": truncated,
            "ts": now_iso(),
        }
    })
}

fn now_iso() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}

/// `keepLogBlob(state, label, text, force)` — persist a blob iff non-empty and
/// (forced or over the inline-size floor). Returns the ref.
fn keep_log_blob(
    state: &mut RepoState,
    label: &str,
    text: &str,
    force: bool,
    session: Option<&str>,
) -> Option<crate::state::BlobRef> {
    if text.trim().is_empty() {
        return None;
    }
    if !force && text.len() < LOG_BLOB_MIN_CHARS {
        return None;
    }
    state.put_blob(text, label, session).ok()
}

/// Drop `git status --short` lines pointing at hidden dirs (keeps `##` + dotfiles).
fn strip_hidden_dir_status(config: &Config, text: &str) -> String {
    if !config.hide_hidden_dirs {
        return text.to_string();
    }
    text.split('\n')
        .filter(|line| {
            if line.trim().is_empty() || line.starts_with("##") {
                return true;
            }
            let path = line.get(3..).unwrap_or("");
            let path = path.split(" -> ").last().unwrap_or("");
            !tools::in_hidden_dir(path)
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Build an `ApprovalInput` and the standard blocked reply, mirroring
/// `approvalBlockedReply`. Records the approval (which itself emits an event).
fn approval_blocked_reply(
    state: &mut RepoState,
    config: &Config,
    tool: &str,
    reason: &str,
    command: Option<String>,
    files: Option<Vec<String>>,
    retry: Value,
    mut extra: Value,
    session: Option<&str>,
) -> ToolReply {
    let req = state.request_approval(
        ApprovalInput {
            tool: tool.to_string(),
            reason: reason.to_string(),
            command,
            files,
            retry: Some(retry),
            sandbox_mode: config.sandbox_mode.as_str().to_string(),
            approval_policy: config.approval_policy.as_str().to_string(),
            reviewer: match config.approvals_reviewer {
                tools::ApprovalReviewer::User => "user".to_string(),
                tools::ApprovalReviewer::AutoReview => "auto_review".to_string(),
            },
            status: "pending".to_string(),
        },
        session,
    );
    let text = format!(
        "Approval needed ({}/{}): {}\n\nUse /permissions to change policy, or retry the tool with approval: \"approved\" after reviewing the risk.",
        config.sandbox_mode.as_str(),
        config.approval_policy.as_str(),
        req.reason
    );
    let approval_val = serde_json::to_value(&req).unwrap_or(Value::Null);
    if let Value::Object(map) = &mut extra {
        map.insert("needsApproval".to_string(), json!(true));
        map.insert("approval".to_string(), approval_val.clone());
        map.insert(
            "retry".to_string(),
            req.retry.clone().unwrap_or(Value::Null),
        );
        map.insert("suggestedRenderMode".to_string(), json!("permissions"));
    }
    text_reply(false, extra, text, Some(json!({ "approval": approval_val })))
}

// ---------------------------------------------------------------------------
// Relay (register.ts relayTarget / relayShell / relayPatch / relayPlan /
// relayFunction) — the codex toolcall long-poll over CHIMERA_RELAY_URL.
// ---------------------------------------------------------------------------

struct RelayTarget {
    url: String,
    thread_id: Option<String>,
}

/// `relayTarget(agentId)` — three-way gate keyed on the agent-id namespace.
fn relay_target(relay_url: &Option<String>, agent_id: &str) -> Option<RelayTarget> {
    let url = relay_url.clone()?;
    if agent_id == "main" {
        return Some(RelayTarget { url, thread_id: None });
    }
    if let Some(thread) = agent_id.strip_prefix("codex:") {
        return Some(RelayTarget {
            url,
            thread_id: Some(thread.to_string()),
        });
    }
    None
}

/// A normalized shell-style relay result (matches register.ts's shaped object).
struct RelayShellResult {
    exit_code: i32,
    combined: String,
    timed_out: bool,
    truncated: bool,
    duration_ms: u128,
}

/// `relayShell(command, agentId, workdir?)`.
async fn relay_shell(
    relay_url: &Option<String>,
    command: &str,
    agent_id: &str,
    workdir: Option<&str>,
) -> Option<RelayShellResult> {
    let target = relay_target(relay_url, agent_id)?;
    let t0 = std::time::Instant::now();
    let mut args = serde_json::Map::new();
    args.insert("command".to_string(), json!(command));
    if let Some(w) = workdir {
        args.insert("workdir".to_string(), json!(w));
    }
    let mut body = json!({ "name": "shell_command", "arguments": Value::Object(args) });
    if let Some(thread) = &target.thread_id {
        body["thread_id"] = json!(thread);
    }
    match relay_post(&target.url, &body, relay_timeout_for("shell_command")).await {
        Ok((ok, status, output, error)) => {
            if !ok || output.is_none() {
                let msg = format!(
                    "relay to codex failed: {}",
                    error.unwrap_or_else(|| format!("HTTP {status}"))
                );
                Some(RelayShellResult {
                    exit_code: 1,
                    combined: msg,
                    timed_out: status == 504,
                    truncated: false,
                    duration_ms: t0.elapsed().as_millis(),
                })
            } else {
                let out = output.unwrap_or_default();
                Some(RelayShellResult {
                    exit_code: 0,
                    combined: out,
                    timed_out: false,
                    truncated: false,
                    duration_ms: t0.elapsed().as_millis(),
                })
            }
        }
        Err(e) => Some(RelayShellResult {
            exit_code: 1,
            combined: format!("relay to codex failed: {e}"),
            timed_out: false,
            truncated: false,
            duration_ms: t0.elapsed().as_millis(),
        }),
    }
}

/// `relayPatch(patch, agentId)` — relay a freeform apply_patch envelope. Returns
/// codex's output text, or None when relay is disabled / off-codex subagent.
async fn relay_patch(relay_url: &Option<String>, patch: &str, agent_id: &str) -> Option<String> {
    let target = relay_target(relay_url, agent_id)?;
    let mut body = json!({ "name": "apply_patch", "kind": "custom", "input": patch });
    if let Some(thread) = &target.thread_id {
        body["thread_id"] = json!(thread);
    }
    Some(match relay_post(&target.url, &body, relay_timeout_for("apply_patch")).await {
        Ok((ok, status, output, error)) => {
            if !ok || output.is_none() {
                format!(
                    "relay to codex failed: {}",
                    error.unwrap_or_else(|| format!("HTTP {status}"))
                )
            } else {
                output.unwrap_or_default()
            }
        }
        Err(e) => format!("relay to codex failed: {e}"),
    })
}

/// `relayPlan(plan, explanation, agentId)`.
async fn relay_plan(
    relay_url: &Option<String>,
    plan: &Value,
    explanation: Option<&str>,
    agent_id: &str,
) -> Option<String> {
    let target = relay_target(relay_url, agent_id)?;
    let mut body = json!({
        "name": "update_plan",
        "arguments": { "plan": plan, "explanation": explanation },
    });
    if let Some(thread) = &target.thread_id {
        body["thread_id"] = json!(thread);
    }
    Some(match relay_post(&target.url, &body, relay_timeout_for("update_plan")).await {
        Ok((ok, status, output, error)) => {
            if !ok || output.is_none() {
                format!(
                    "relay to codex failed: {}",
                    error.unwrap_or_else(|| format!("HTTP {status}"))
                )
            } else {
                output.unwrap_or_default()
            }
        }
        Err(e) => format!("relay to codex failed: {e}"),
    })
}

/// `relayFunction(name, args, agentId)` — generic codex-native function relay.
async fn relay_function(
    relay_url: &Option<String>,
    name: &str,
    args: Value,
    agent_id: &str,
) -> Option<String> {
    let target = relay_target(relay_url, agent_id)?;
    let mut body = json!({ "name": name, "arguments": args });
    if let Some(thread) = &target.thread_id {
        body["thread_id"] = json!(thread);
    }
    Some(match relay_post(&target.url, &body, relay_timeout_for(name)).await {
        Ok((ok, status, output, error)) => {
            if !ok || output.is_none() {
                format!(
                    "relay to codex failed: {}",
                    error.unwrap_or_else(|| format!("HTTP {status}"))
                )
            } else {
                output.unwrap_or_default()
            }
        }
        Err(e) => format!("relay to codex failed: {e}"),
    })
}

/// The ONE process-wide relay HTTP client. Connect timeout only — relay
/// long-polls legitimately run for minutes, so there is no global read timeout;
/// each request instead carries a per-call `.timeout()` sized to its tool (see
/// [`relay_timeout_for`]). Building a fresh `Client::new()` per call (the old
/// shape) leaked connection pools and, with no timeouts at all, let one wedged
/// half-open lipsync socket pin a relay tool for the full 3700s dispatch budget.
fn relay_client() -> &'static reqwest::Client {
    static CLIENT: std::sync::OnceLock<reqwest::Client> = std::sync::OnceLock::new();
    CLIENT.get_or_init(|| {
        reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(5))
            .build()
            // Builder only fails on TLS/resolver misconfig; fall back to the
            // default client rather than panicking the whole tool surface.
            .unwrap_or_else(|_| reqwest::Client::new())
    })
}

/// Per-request relay deadline for `tool`: the dispatcher's budget for that tool
/// minus a 10s margin, so the relay errors cleanly ("relay to codex failed:
/// timed out") and the handler can still shape a reply BEFORE the outer
/// per-tool timeout fires. Floor of 5s guards tiny/overridden budgets.
fn relay_timeout_for(tool: &str) -> Duration {
    Duration::from_secs(crate::mcp::tool_timeout_secs(tool).saturating_sub(10).max(5))
}

/// POST the relay body and parse `{ output?, error? }`. Returns
/// `(res.ok, status, output, error)`. `timeout` is the whole-request deadline
/// (connect is separately capped by the shared client).
async fn relay_post(
    url: &str,
    body: &Value,
    timeout: Duration,
) -> Result<(bool, u16, Option<String>, Option<String>), String> {
    let map_err = |e: reqwest::Error| {
        if e.is_timeout() {
            "timed out".to_string()
        } else {
            e.to_string()
        }
    };
    let res = relay_client()
        .post(url)
        .header("content-type", "application/json")
        .json(body)
        .timeout(timeout)
        .send()
        .await
        .map_err(map_err)?;
    let status = res.status().as_u16();
    let ok = res.status().is_success();
    let j: Value = res.json().await.map_err(map_err)?;
    let output = j.get("output").and_then(|v| v.as_str()).map(String::from);
    let error = j.get("error").and_then(|v| v.as_str()).map(String::from);
    Ok((ok, status, output, error))
}

/// `buildUpdatePatch(path, old, new)` — codex apply_patch "Update File" envelope.
fn build_update_patch(file_path: &str, old_str: &str, new_str: &str) -> String {
    let rel = file_path.replace('\\', "/");
    let minus = old_str
        .split('\n')
        .map(|l| format!("-{l}"))
        .collect::<Vec<_>>()
        .join("\n");
    let plus = new_str
        .split('\n')
        .map(|l| format!("+{l}"))
        .collect::<Vec<_>>()
        .join("\n");
    format!("*** Begin Patch\n*** Update File: {rel}\n@@\n{minus}\n{plus}\n*** End Patch\n")
}

/// `buildAddPatch(path, content)` — codex apply_patch "Add File" envelope.
fn build_add_patch(file_path: &str, content: &str) -> String {
    let rel = file_path.replace('\\', "/");
    let body = content
        .split('\n')
        .map(|l| format!("+{l}"))
        .collect::<Vec<_>>()
        .join("\n");
    format!("*** Begin Patch\n*** Add File: {rel}\n{body}\n*** End Patch\n")
}

fn patch_succeeded(out: &str, allow_added: bool) -> bool {
    let lower = out.to_lowercase();
    let success = lower.contains("success")
        || lower.contains("updated the following")
        || (allow_added && lower.contains("added the following"));
    let failed = lower.contains("failed")
        || lower.contains("already exists")
        || lower.contains("did not apply")
        || lower.contains("error");
    success && !failed
}

// Small arg helpers --------------------------------------------------------

fn arg_str(args: &Value, key: &str) -> Option<String> {
    args.get(key).and_then(|v| v.as_str()).map(String::from)
}
fn arg_bool(args: &Value, key: &str) -> Option<bool> {
    args.get(key).and_then(|v| v.as_bool())
}
fn arg_u64(args: &Value, key: &str) -> Option<u64> {
    args.get(key).and_then(|v| v.as_u64())
}
fn arg_str_array(args: &Value, key: &str) -> Vec<String> {
    args.get(key)
        .and_then(|v| v.as_array())
        .map(|a| a.iter().filter_map(|x| x.as_str().map(String::from)).collect())
        .unwrap_or_default()
}

// Output schema shorthands (zod-to-json-schema produces object schemas; the exact
// property typing is not load-bearing for the tools/list contract, but the field
// SET is, so each tool below lists its fields).
fn obj_schema(props: Value) -> Value {
    json!({ "type": "object", "properties": props })
}

// ===========================================================================
// register_repo_tools — the 28-tool surface, in register.ts order.
// ===========================================================================

/// Port of `registerRepoTools(server, config, state)`. Registers the workbench
/// resource and every tool on `b`. `ctx` carries the shared state/config/relay.
pub fn register_repo_tools(b: &mut RepoMcpServerBuilder, ctx: &ToolCtx) {
    // Started-event hook: the dispatcher fires this before every tool handler so
    // the /events feed shows a call beginning even when the handler later times
    // out. Shape note: the lipsync consumers (subagent_mux / provider) treat ANY
    // event with a non-empty `tool` as a completed ToolCall+ToolResult and skip
    // events whose `tool` is empty — so started events keep `tool` empty (they
    // are safely ignored there) and carry the real name in `kind`/`args.tool`.
    // Delivery is EPHEMERAL (live SSE tail only): a durable event here would do
    // an events.jsonl append + full state.json rewrite before every handler.
    {
        let state = ctx.state.clone();
        b.set_tool_started_observer(Arc::new(move |tool: String, session: Option<String>| {
            let state = state.clone();
            Box::pin(async move {
                let mut state = state.lock().await;
                state.ephemeral_event(
                    EventInput {
                        kind: Some("tool_started".to_string()),
                        args: Some(json!({ "tool": tool })),
                        ..EventInput::new("", true, format!("started {tool}"))
                    },
                    session.as_deref(),
                );
            })
        }));
    }

    // Snapshot the config flags that gate write/shell visibility at registration
    // time (the TS reads `config.allowModelWriteFile` / `allowModelDevShell` when
    // building each `_meta`, before any request can mutate them).
    let (allow_write, allow_shell) = {
        // Best-effort synchronous read; the Mutex is uncontended at registration.
        match ctx.config.try_lock() {
            Ok(c) => (c.allow_model_write_file, c.allow_model_dev_shell),
            Err(_) => (true, true),
        }
    };

    register_workbench_resource(b);

    register_repo_ui(b, ctx);
    register_repo_status(b, ctx);
    register_repo_register(b, ctx);
    register_repo_glob(b, ctx);
    register_repo_read(b, ctx);
    register_repo_grep(b, ctx);
    register_repo_write(b, ctx, allow_write);
    register_repo_diff(b, ctx);
    register_repo_edit(b, ctx, allow_write);
    register_repo_run(b, ctx);
    register_repo_shell(b, ctx, allow_shell);
    register_shell_command(b, ctx, allow_shell);
    register_apply_patch(b, ctx, allow_write);
    register_update_plan(b, ctx);
    register_repo_bg_output(b, ctx);
    register_repo_bg_stop(b, ctx);
    register_repo_bg_list(b, ctx);
    register_repo_permissions(b, ctx);
    register_repo_project(b, ctx);
    register_repo_slash_command(b, ctx);
    register_repo_remember(b, ctx);
    register_repo_compact(b, ctx);
    register_repo_resume(b, ctx);
    register_repo_logs(b, ctx);

    // codex-native function tools (relay-only): interactive prompt + goal store.
    register_request_user_input(b, ctx);
    register_goal_tools(b, ctx);

    // codex-native multi-agent surface (relay-only).
    register_relay_agent_tools(b, ctx);

    // Legacy off-codex subagent rail (repo_spawn_subagent / repo_await /
    // repo_subagent_list / repo_subagent_kill), gated behind
    // CHIMERA_LEGACY_SUBAGENTS=1 exactly like register.ts:1200 ("dead-end legacy
    // tools (they need subagent_mux running)"). Default off: the live tools/list
    // is 34 names, matching the Node server.
    if ctx.legacy_subagents {
        register_subagent_tools(b, ctx);
    }
}

fn register_workbench_resource(b: &mut RepoMcpServerBuilder) {
    b.register_resource(
        "repo-workbench",
        WORKBENCH_URI,
        json!({}),
        Arc::new(|| {
            json!({
                "contents": [{
                    "uri": WORKBENCH_URI,
                    "mimeType": "text/html+skybridge",
                    "text": "<!doctype html><title>Repo Workbench</title>",
                }]
            })
        }),
    );
}

// --- repo_ui ---------------------------------------------------------------

fn register_repo_ui(b: &mut RepoMcpServerBuilder, ctx: &ToolCtx) {
    let h = handler!(ctx, args, session, {
        let mode = arg_str(&args, "mode").unwrap_or_else(|| "open".to_string());
        let config = ctx.config.lock().await.clone();
        let diff = tools::safe_git_diff(&config, None).await;
        let projects = if mode == "projects" {
            Some(serde_json::to_value(tools::discover_projects(
                &config,
                tools::DiscoverOpts { query: None, max: Some(18) },
            )).unwrap_or(Value::Null))
        } else {
            None
        };
        let mut state = ctx.state.lock().await;
        let extra = json!({ "renderMode": mode, "projects": projects });
        let reply = text_reply(
            true,
            snapshot(&config, &state, extra),
            format!("Opened repo surface ({mode})."),
            Some(json!({ "currentDiff": diff, "projects": projects })),
        );
        event_then(
            &mut state,
            EventInput::new("repo_ui", true, format!("render {mode}")),
            session.as_deref(),
            reply,
        )
    });
    register(
        b,
        "repo_ui",
        "Render repo surface",
        "Open or refresh the focused repo surface. Use only when the user asks for UI or when inspecting diffs, failed/long logs, approvals, projects, or compaction state; keep ordinary repo work conversational.",
        obj_schema(json!({
            "mode": { "type": "string", "enum": ["open","refresh","diff","logs","timeline","capsules","notes","projects","permissions","command"] }
        })),
        obj_schema(json!({
            "ok": {"type":"boolean"}, "mode": {"type":"string"}, "repoRoot": {"type":"string"},
            "snapshot": {}, "renderMode": {"type":"string"}
        })),
        json!({ "readOnlyHint": true }),
        render_meta(),
        h,
    );
}

// --- repo_status -----------------------------------------------------------

fn register_repo_status(b: &mut RepoMcpServerBuilder, ctx: &ToolCtx) {
    let h = handler!(ctx, _args, session, {
        let config = ctx.config.lock().await.clone();
        // Direct git (no PowerShell) — see tools::run_git. A non-repo root gets a
        // short, clean message instead of a raw stderr wall.
        let status = tools::run_git(&config, &["status", "--short", "--branch"], 40_000).await;
        let perms = tools::effective_permissions(&config);
        let (status_combined, status_command, status_exit, status_dur) = match &status {
            Ok(s) if s.exit_code == Some(0) => {
                (s.combined.clone(), s.command.clone(), s.exit_code, s.duration_ms)
            }
            Ok(s) => (
                git_failure_text(&config, &s.combined),
                s.command.clone(),
                s.exit_code,
                s.duration_ms,
            ),
            Err(e) => (
                cap_error_text(e),
                "git status --short --branch".to_string(),
                None,
                0,
            ),
        };
        // The old shell chained `git diff --stat && git diff --cached --stat`;
        // run the two invocations directly and concatenate.
        let diffstat_combined = if matches!(&status, Ok(s) if s.exit_code == Some(0)) {
            let mut combined = String::new();
            for args in [&["diff", "--stat"][..], &["diff", "--cached", "--stat"][..]] {
                match tools::run_git(&config, args, 40_000).await {
                    Ok(d) => {
                        if !combined.is_empty() && !d.combined.is_empty() {
                            combined.push('\n');
                        }
                        combined.push_str(&d.combined);
                    }
                    Err(e) => {
                        combined = cap_error_text(&e);
                        break;
                    }
                }
            }
            combined
        } else {
            String::new()
        };
        let clean = strip_hidden_dir_status(&config, &status_combined);
        let mut state = ctx.state.lock().await;
        let text = format!(
            "Project: {}\nPermissions: {}/{}\n\nRepo status:\n{clean}\n\nDiffstat:\n{diffstat_combined}",
            config.root.to_string_lossy(),
            perms.sandbox_mode,
            perms.approval_policy
        );
        let extra = json!({
            "status": clean,
            "diffstat": diffstat_combined,
            "permissions": serde_json::to_value(&perms).unwrap_or(Value::Null),
        });
        let reply = text_reply(true, snapshot(&config, &state, extra), text, None);
        event_then(
            &mut state,
            EventInput {
                command: Some(status_command),
                exit_code: Some(status_exit.map(|c| c as i64)),
                bytes: Some(clean.len() as u64),
                duration_ms: Some(status_dur as u64),
                ..EventInput::new("repo_status", true, brief(if clean.is_empty() { "clean" } else { &clean }))
            },
            session.as_deref(),
            reply,
        )
    });
    register(
        b,
        "repo_status",
        "Repo status",
        "Get current git status, branch, diffstat, active project, sandbox, and recent repo-agent state. Data-only: does not open the UI.",
        obj_schema(json!({})),
        obj_schema(json!({
            "ok": {"type":"boolean"}, "mode": {"type":"string"}, "repoRoot": {"type":"string"},
            "status": {"type":"string"}, "diffstat": {"type":"string"}, "permissions": {}, "snapshot": {}
        })),
        json!({ "readOnlyHint": true }),
        app_callable_meta(&["model", "app"]),
        h,
    );
}

// --- repo_register ---------------------------------------------------------

fn register_repo_register(b: &mut RepoMcpServerBuilder, ctx: &ToolCtx) {
    let h = handler!(ctx, args, session, {
        let agent_id = arg_str(&args, "agent_id").unwrap_or_default();
        let mut state = ctx.state.lock().await;
        let bound = session.as_deref().filter(|s| !s.is_empty()).is_some();
        if let Some(s) = session.as_deref().filter(|s| !s.is_empty()) {
            state.bind_session(s, &agent_id);
        }
        let reply = text_reply(
            true,
            json!({ "agent_id": agent_id, "bound": bound }),
            format!("Registered as {agent_id}."),
            None,
        );
        event_then(
            &mut state,
            EventInput {
                agent: Some(agent_id.clone()),
                ..EventInput::new("repo_register", true, format!("registered {agent_id}"))
            },
            session.as_deref(),
            reply,
        )
    });
    register(
        b,
        "repo_register",
        "Register subagent identity",
        "Bind THIS conversation to the agent_id the harness assigned you, so your tool activity is attributed to you. If your setup message gave you an agent_id, call this ONCE as your very first action, before any other tool. The main thread does not need this.",
        json!({
            "type": "object",
            "properties": { "agent_id": { "type": "string", "minLength": 1 } },
            "required": ["agent_id"]
        }),
        obj_schema(json!({ "ok": {"type":"boolean"}, "agent_id": {"type":"string"}, "bound": {"type":"boolean"} })),
        json!({ "readOnlyHint": false, "openWorldHint": false, "destructiveHint": false }),
        app_callable_meta(&["model", "app"]),
        h,
    );
}

// --- repo_glob -------------------------------------------------------------

fn register_repo_glob(b: &mut RepoMcpServerBuilder, ctx: &ToolCtx) {
    let h = handler!(ctx, args, session, {
        let config = ctx.config.lock().await.clone();
        let mut globs = arg_str_array(&args, "patterns");
        if let Some(p) = arg_str(&args, "pattern") {
            globs.push(p);
        }
        let listed = tools::list_repo_files(
            &config,
            tools::ListOpts {
                globs: globs.clone(),
                max: arg_u64(&args, "max").map(|n| n as usize),
                sort: arg_str(&args, "sort"),
            },
        )
        .await;
        let files = listed.files;
        let label = if globs.is_empty() {
            String::new()
        } else {
            format!(" matching {}", globs.join(", "))
        };
        let mut state = ctx.state.lock().await;
        let mut text = if files.is_empty() {
            format!("No files{label}.")
        } else {
            files.join("\n")
        };
        // A bounded walk is not exhaustive — say so instead of silently lying.
        if listed.truncated {
            text.push_str(&format!(
                "\n(truncated: scan budget exhausted; {} files scanned — register a project directory, not a home directory)",
                listed.scanned
            ));
        }
        let reply = text_reply(
            true,
            json!({ "files": files, "count": files.len(), "truncated": listed.truncated }),
            text,
            None,
        );
        let preview: Vec<String> = files.iter().take(20).cloned().collect();
        let bytes = serde_json::to_string(&files).map(|s| s.len()).unwrap_or(0) as u64;
        event_then(
            &mut state,
            EventInput {
                files: Some(preview),
                bytes: Some(bytes),
                ..EventInput::new("repo_glob", true, format!("{} files{label}", files.len()))
            },
            session.as_deref(),
            reply,
        )
    });
    register(
        b,
        "repo_glob",
        "Find files by name",
        "Find files by glob pattern, returned most-recently-modified first. Supports patterns like `**/*.ts`, `src/**/test_*.py`, or a bare substring. Pass multiple patterns in `patterns` to union them. Use this to locate files by name; use repo_grep to search file contents. Only lists tracked/unignored files.",
        obj_schema(json!({
            "pattern": {"type":"string"},
            "patterns": {"type":"array","items":{"type":"string"}},
            "max": {"type":"integer","minimum":1,"maximum":5000},
            "sort": {"type":"string","enum":["mtime","path"]}
        })),
        obj_schema(json!({ "ok": {"type":"boolean"}, "files": {"type":"array","items":{"type":"string"}}, "count": {"type":"number"} })),
        json!({ "readOnlyHint": true }),
        model_meta(),
        h,
    );
}

// --- repo_read -------------------------------------------------------------

fn register_repo_read(b: &mut RepoMcpServerBuilder, ctx: &ToolCtx) {
    let h = handler!(ctx, args, session, {
        let config = ctx.config.lock().await.clone();
        let path = arg_str(&args, "path").unwrap_or_default();
        let r = tools::read_repo_file(
            &config,
            &path,
            tools::ReadOpts {
                start_line: arg_u64(&args, "start_line").map(|n| n as usize),
                end_line: arg_u64(&args, "end_line").map(|n| n as usize),
                max_bytes: arg_u64(&args, "max_bytes").map(|n| n as usize),
            },
        )
        .await;
        let mut state = ctx.state.lock().await;
        match r {
            Ok(r) => {
                let header = format!(
                    "{} (lines {}-{} of {}){}",
                    r.path,
                    r.start_line.unwrap_or(0),
                    r.end_line.unwrap_or(0),
                    r.lines.unwrap_or(0),
                    if r.truncated { " [truncated]" } else { "" }
                );
                let text = if r.binary { r.content.clone() } else { format!("{header}\n{}", r.content) };
                let structured = serde_json::to_value(&r).unwrap_or(json!({}));
                let reply = text_reply(true, structured, text, None);
                event_then(
                    &mut state,
                    EventInput {
                        files: Some(vec![r.path.clone()]),
                        bytes: Some(r.content.len() as u64),
                        ..EventInput::new(
                            "repo_read",
                            true,
                            format!("{} {}b{}", r.path, r.bytes, if r.truncated { " truncated" } else { "" }),
                        )
                    },
                    session.as_deref(),
                    reply,
                )
            }
            Err(message) => event_then(
                &mut state,
                EventInput {
                    files: Some(vec![path.clone()]),
                    ..EventInput::new("repo_read", false, message.clone())
                },
                session.as_deref(),
                err_reply(message, json!({})),
            ),
        }
    });
    register(
        b,
        "repo_read",
        "Read file",
        "Read a text file under the active project root. Output is line-numbered (use the numbers to target repo_edit and to cite locations). Reads the whole file by default; pass start_line/end_line to read a slice of a large file. Large files are trimmed in the middle. Always read a file before editing it.",
        json!({
            "type": "object",
            "properties": {
                "path": {"type":"string","minLength":1},
                "start_line": {"type":"integer","minimum":1},
                "end_line": {"type":"integer","minimum":1},
                "max_bytes": {"type":"integer","minimum":1000,"maximum":500000}
            },
            "required": ["path"]
        }),
        obj_schema(json!({
            "ok": {"type":"boolean"}, "path": {"type":"string"}, "bytes": {"type":"number"},
            "sha256": {"type":"string"}, "content": {"type":"string"}, "truncated": {"type":"boolean"},
            "binary": {"type":"boolean"}, "startLine": {"type":"number"}, "endLine": {"type":"number"}, "lines": {"type":"number"}
        })),
        json!({ "readOnlyHint": true }),
        model_meta(),
        h,
    );
}

// --- repo_grep -------------------------------------------------------------

fn register_repo_grep(b: &mut RepoMcpServerBuilder, ctx: &ToolCtx) {
    let h = handler!(ctx, args, session, {
        let config = ctx.config.lock().await.clone();
        let query = arg_str(&args, "query").unwrap_or_default();
        let r = tools::search_repo(
            &config,
            &query,
            tools::SearchOpts {
                globs: arg_str_array(&args, "globs"),
                typ: arg_str(&args, "type"),
                max_matches: arg_u64(&args, "max_matches").map(|n| n as usize),
                context: arg_u64(&args, "context").map(|n| n as usize),
                output_mode: arg_str(&args, "output_mode"),
                regex: arg_bool(&args, "regex").unwrap_or(false),
            },
        )
        .await;
        let mut state = ctx.state.lock().await;
        let text = if r.output.is_empty() { "No matches.".to_string() } else { r.output.clone() };
        let reply = text_reply(
            true,
            json!({
                "query": query, "output": r.output, "used": r.used,
                "outputMode": r.output_mode, "exitCode": r.exit_code
            }),
            text,
            None,
        );
        event_then(
            &mut state,
            EventInput {
                command: Some(r.command.clone()),
                bytes: Some(r.output.len() as u64),
                ..EventInput::new("repo_grep", true, format!("{query} ({}) via {}", r.output_mode, r.used))
            },
            session.as_deref(),
            reply,
        )
    });
    register(
        b,
        "repo_grep",
        "Search file contents",
        "Search file contents with ripgrep. `query` is a literal string by default; set regex:true for a regex pattern. output_mode: 'content' (matching lines with line numbers + context, default), 'files' (just file paths that match), 'count' (match count per file). Narrow with `type` (e.g. ts, py, rust, go, js) or `globs` (e.g. ['**/*.test.ts']). Use this to find where something is defined or used before reading whole files; use repo_glob to find files by name.",
        json!({
            "type": "object",
            "properties": {
                "query": {"type":"string","minLength":1},
                "output_mode": {"type":"string","enum":["content","files","count"]},
                "type": {"type":"string"},
                "globs": {"type":"array","items":{"type":"string"}},
                "regex": {"type":"boolean"},
                "max_matches": {"type":"integer","minimum":1,"maximum":300},
                "context": {"type":"integer","minimum":0,"maximum":8}
            },
            "required": ["query"]
        }),
        obj_schema(json!({
            "ok": {"type":"boolean"}, "query": {"type":"string"}, "output": {"type":"string"},
            "used": {"type":"string"}, "outputMode": {"type":"string"}, "exitCode": {"type":["number","null"]}
        })),
        json!({ "readOnlyHint": true }),
        model_meta(),
        h,
    );
}

// --- repo_write ------------------------------------------------------------

fn register_repo_write(b: &mut RepoMcpServerBuilder, ctx: &ToolCtx, allow_write: bool) {
    let h = handler!(ctx, args, session, {
        let config = ctx.config.lock().await.clone();
        let path = arg_str(&args, "path").unwrap_or_default();
        let content = arg_str(&args, "content").unwrap_or_default();
        let create_dirs = arg_bool(&args, "create_dirs").unwrap_or(true);
        let expected_sha256 = arg_str(&args, "expected_sha256");
        let approval = arg_str(&args, "approval");
        let approved = approval.as_deref() == Some("approved");

        let mut state = ctx.state.lock().await;
        if config.sandbox_mode == tools::SandboxMode::ReadOnly && !approved {
            return approval_blocked_reply(
                &mut state, &config, "repo_write", &format!("write {path}"),
                None, Some(vec![path.clone()]),
                json!({ "path": path, "content": content, "create_dirs": create_dirs, "expected_sha256": expected_sha256, "approval": "approved" }),
                json!({ "path": path }), session.as_deref(),
            );
        }
        if !config.allow_model_write_file && !approved {
            return event_then(
                &mut state,
                EventInput { files: Some(vec![path.clone()]), ..EventInput::new("repo_write", false, "model writes disabled") },
                session.as_deref(),
                err_reply("Model writes disabled by config; enable allowModelWriteFile or approve a one-shot write.", json!({})),
            );
        }
        let agent_id = state.agent_for_session(session.as_deref());
        let lease = state.acquire_lease(&agent_id, vec![path.clone()], "write");
        if !lease.ok {
            let conflict = lease.conflict.clone().unwrap_or_else(|| format!("{path} is leased by another agent"));
            return event_then(
                &mut state,
                EventInput { files: Some(vec![path.clone()]), ..EventInput::new("repo_write", false, conflict.clone()) },
                session.as_deref(),
                err_reply(conflict, json!({ "path": path })),
            );
        }
        let result = (|| async {
            // New file: relay an Add File apply_patch to codex (native diff card).
            if expected_sha256.is_none() {
                if let Some(out) = relay_patch(&ctx.relay_url, &build_add_patch(&path, &content), &agent_id).await {
                    if patch_succeeded(&out, false) {
                        return Ok(text_reply(true, json!({ "ok": true, "path": path, "output": out }), out, None));
                    }
                }
            }
            match tools::write_repo_file(&config, &path, &content, tools::WriteOpts { create_dirs, expected_sha256: expected_sha256.clone() }) {
                Ok(r) => {
                    let text = format!("Wrote {} ({} bytes).", r.path, r.bytes);
                    Ok(text_reply(true, serde_json::to_value(&r).unwrap_or(json!({})), text, None))
                }
                Err(e) => Err(e),
            }
        })().await;
        if let Some(lease_id) = &lease.lease_id {
            state.release_lease(lease_id);
        }
        match result {
            Ok(reply) => event_then(
                &mut state,
                EventInput { files: Some(vec![path.clone()]), ..EventInput::new("repo_write", true, format!("wrote {path}")) },
                session.as_deref(),
                reply,
            ),
            Err(message) => event_then(
                &mut state,
                EventInput { files: Some(vec![path.clone()]), ..EventInput::new("repo_write", false, message.clone()) },
                session.as_deref(),
                err_reply(message, json!({})),
            ),
        }
    });
    register(
        b,
        "repo_write",
        "Write file",
        "Write a complete UTF-8 file under the active project root, creating it or overwriting it wholesale. Use this for brand-new files or a full rewrite; for changing part of an existing file use repo_edit instead (it's safer and cheaper). Pass expected_sha256 (from repo_read) to guard against clobbering concurrent edits.",
        json!({
            "type": "object",
            "properties": {
                "path": {"type":"string","minLength":1},
                "content": {"type":"string"},
                "create_dirs": {"type":"boolean"},
                "expected_sha256": {"type":"string"},
                "approval": {"type":"string","enum":["approved"]}
            },
            "required": ["path","content"]
        }),
        obj_schema(json!({
            "ok": {"type":"boolean"}, "path": {"type":"string"}, "bytes": {"type":"number"}, "sha256": {"type":"string"},
            "needsApproval": {"type":"boolean"}, "approval": {}, "error": {"type":"string"}
        })),
        json!({ "readOnlyHint": false, "openWorldHint": false, "destructiveHint": false }),
        app_callable_meta(&write_visibility(allow_write)),
        h,
    );
}

// --- repo_diff -------------------------------------------------------------

fn register_repo_diff(b: &mut RepoMcpServerBuilder, ctx: &ToolCtx) {
    let h = handler!(ctx, args, session, {
        let config = ctx.config.lock().await.clone();
        let max_bytes = arg_u64(&args, "max_bytes").map(|n| n as usize);
        let diff = tools::safe_git_diff(&config, max_bytes).await;
        let truncated = diff.contains("…[trimmed");
        let mut state = ctx.state.lock().await;
        let text = if diff.is_empty() { "No diff.".to_string() } else { diff.clone() };
        let reply = text_reply(
            true,
            json!({ "diff": diff, "truncated": truncated, "suggestedRenderMode": if diff.is_empty() { Value::Null } else { json!("diff") } }),
            text,
            Some(json!({ "currentDiff": diff })),
        );
        let summary = if diff.is_empty() { "clean".to_string() } else { format!("{} chars", diff.len()) };
        event_then(
            &mut state,
            EventInput { bytes: Some(diff.len() as u64), ..EventInput::new("repo_diff", true, summary) },
            session.as_deref(),
            reply,
        )
    });
    register(
        b,
        "repo_diff",
        "Read git diff",
        "Read the current working tree and staged diff, trimmed if large. Data-only; render repo_ui({mode:'diff'}) when visual review helps.",
        obj_schema(json!({ "max_bytes": {"type":"integer","minimum":1000,"maximum":500000} })),
        obj_schema(json!({ "ok": {"type":"boolean"}, "diff": {"type":"string"}, "truncated": {"type":"boolean"}, "suggestedRenderMode": {"type":"string"} })),
        json!({ "readOnlyHint": true }),
        app_callable_meta(&["model", "app"]),
        h,
    );
}

// --- repo_edit -------------------------------------------------------------

fn register_repo_edit(b: &mut RepoMcpServerBuilder, ctx: &ToolCtx, allow_write: bool) {
    let h = handler!(ctx, args, session, {
        let config = ctx.config.lock().await.clone();
        let path = arg_str(&args, "path").unwrap_or_default();
        let old_string = arg_str(&args, "old_string").unwrap_or_default();
        let new_string = arg_str(&args, "new_string").unwrap_or_default();
        let replace_all = arg_bool(&args, "replace_all").unwrap_or(false);
        let approved = arg_str(&args, "approval").as_deref() == Some("approved");

        let mut state = ctx.state.lock().await;
        if config.sandbox_mode == tools::SandboxMode::ReadOnly && !approved {
            return approval_blocked_reply(
                &mut state, &config, "repo_edit", &format!("edit {path}"),
                None, Some(vec![path.clone()]),
                json!({ "path": path, "old_string": old_string, "new_string": new_string, "replace_all": replace_all, "approval": "approved" }),
                json!({ "path": path }), session.as_deref(),
            );
        }
        if !config.allow_model_write_file && !approved {
            return event_then(
                &mut state,
                EventInput { files: Some(vec![path.clone()]), ..EventInput::new("repo_edit", false, "model writes disabled") },
                session.as_deref(),
                err_reply("Model writes disabled by config.", json!({})),
            );
        }
        let agent_id = state.agent_for_session(session.as_deref());
        let lease = state.acquire_lease(&agent_id, vec![path.clone()], "write");
        if !lease.ok {
            let conflict = lease.conflict.clone().unwrap_or_else(|| format!("{path} is leased by another agent"));
            return event_then(
                &mut state,
                EventInput { files: Some(vec![path.clone()]), ..EventInput::new("repo_edit", false, conflict.clone()) },
                session.as_deref(),
                err_reply(conflict, json!({ "path": path })),
            );
        }
        let result = (|| async {
            // Single-string edit relays to codex as apply_patch (replace_all stays local).
            if !replace_all {
                if let Some(out) = relay_patch(&ctx.relay_url, &build_update_patch(&path, &old_string, &new_string), &agent_id).await {
                    let ok = patch_succeeded(&out, false);
                    let reply = text_reply(ok, json!({ "ok": ok, "path": path, "output": out }), out, None);
                    return (ok, if ok { format!("edited {path} (codex apply_patch)") } else { format!("apply_patch failed: {path}") }, reply);
                }
            }
            match tools::edit_repo_file(&config, &path, &old_string, &new_string, replace_all) {
                Ok(r) => {
                    let text = format!("Edited {} ({} replacement{}).", r.path, r.replacements, if r.replacements == 1 { "" } else { "s" });
                    (true, format!("edited {} ({}x)", r.path, r.replacements),
                     text_reply(true, json!({ "ok": true, "path": r.path, "replacements": r.replacements }), text, None))
                }
                Err(e) => (false, e.clone(), err_reply(e, json!({}))),
            }
        })().await;
        if let Some(lease_id) = &lease.lease_id {
            state.release_lease(lease_id);
        }
        let (ok, summary, reply) = result;
        event_then(
            &mut state,
            EventInput { files: Some(vec![path.clone()]), ..EventInput::new("repo_edit", ok, summary) },
            session.as_deref(),
            reply,
        )
    });
    register(
        b,
        "repo_edit",
        "Edit file",
        "Make an exact string replacement in an existing file — the primary way to change code. Read the file first, then pass old_string copied VERBATIM (include enough surrounding lines to be unique) and the new_string to replace it with. Set replace_all to change every occurrence. Use repo_write only for brand-new files or a full rewrite. Prefer this over rewriting whole files.",
        json!({
            "type": "object",
            "properties": {
                "path": {"type":"string","minLength":1},
                "old_string": {"type":"string"},
                "new_string": {"type":"string"},
                "replace_all": {"type":"boolean"},
                "approval": {"type":"string","enum":["approved"]}
            },
            "required": ["path","old_string","new_string"]
        }),
        obj_schema(json!({
            "ok": {"type":"boolean"}, "path": {"type":"string"}, "replacements": {"type":"number"},
            "needsApproval": {"type":"boolean"}, "approval": {}, "error": {"type":"string"}
        })),
        json!({ "readOnlyHint": false, "openWorldHint": false, "destructiveHint": false }),
        app_callable_meta(&write_visibility(allow_write)),
        h,
    );
}

// --- repo_run --------------------------------------------------------------

fn register_repo_run(b: &mut RepoMcpServerBuilder, ctx: &ToolCtx) {
    let h = handler!(ctx, args, session, {
        let config = ctx.config.lock().await.clone();
        let profile = arg_str(&args, "profile").unwrap_or_default();
        let timeout_ms = arg_u64(&args, "timeout_ms");
        let approved = arg_str(&args, "approval").as_deref() == Some("approved");

        let command = match config.command_profiles.get(&profile) {
            Some(c) => c.clone(),
            None => {
                let known = config.command_profiles.keys().cloned().collect::<Vec<_>>().join(", ");
                let mut state = ctx.state.lock().await;
                return event_then(
                    &mut state,
                    EventInput::new("repo_run", false, format!("unknown profile {profile}")),
                    session.as_deref(),
                    err_reply(format!("Unknown task profile {profile}. Known: {known}"), json!({})),
                );
            }
        };
        let review = tools::review_command(&config, &command, tools::ReviewOpts { approved, task_profile: true });
        let parsed = serde_json::to_value(&review.parsed).unwrap_or(Value::Null);
        let mut state = ctx.state.lock().await;
        if !review.allowed {
            if review.needs_approval {
                return approval_blocked_reply(
                    &mut state, &config, "repo_run",
                    review.reason.as_deref().unwrap_or(&format!("run {profile}")),
                    Some(command.clone()), None,
                    json!({ "profile": profile, "timeout_ms": timeout_ms, "approval": "approved" }),
                    json!({ "profile": profile, "command": command, "parsedCommand": parsed }),
                    session.as_deref(),
                );
            }
            let reason = review.reason.clone().unwrap_or_else(|| "Command blocked by policy".to_string());
            return event_then(
                &mut state,
                EventInput { command: Some(command.clone()), parsed_command: Some(review.parsed.clone().into_iter().map(state_parsed).collect()), ..EventInput::new("repo_run", false, reason.clone()) },
                session.as_deref(),
                err_reply(reason, json!({ "command": command, "parsedCommand": parsed })),
            );
        }
        let agent_id = state.agent_for_session(session.as_deref());
        drop(state);
        let r = run_or_relay(&ctx.relay_url, &config, &command, &agent_id, timeout_ms, None).await;
        let mut state = ctx.state.lock().await;
        match r {
            Ok(rr) => {
                let ok = rr.exit_code == Some(0);
                let blob = keep_log_blob(&mut state, &profile, &rr.combined, should_keep_log(ok, rr.timed_out, &rr.combined, rr.truncated), session.as_deref());
                let needs_review = !ok || blob.is_some();
                let meta = if needs_review {
                    Some(log_meta(&format!("{profile} exit {:?}{}", rr.exit_code, if rr.timed_out { " timed out" } else { "" }), &rr.combined, blob.as_ref(), rr.truncated))
                } else { None };
                let reply = text_reply(
                    ok,
                    json!({
                        "profile": profile, "command": command, "exitCode": rr.exit_code,
                        "output": rr.combined, "timedOut": rr.timed_out, "needsReview": needs_review,
                        "suggestedRenderMode": if needs_review { json!("logs") } else { Value::Null },
                        "parsedCommand": parsed,
                    }),
                    format!("{profile} {} ({:?}).\n{}", if ok { "passed" } else { "failed" }, rr.exit_code, rr.combined),
                    meta,
                );
                event_then(
                    &mut state,
                    EventInput {
                        command: Some(command.clone()),
                        exit_code: Some(rr.exit_code.map(|c| c as i64)),
                        blobs: blob.as_ref().map(|b| vec![b.id.clone()]),
                        bytes: Some(rr.combined.len() as u64),
                        duration_ms: Some(rr.duration_ms as u64),
                        parsed_command: Some(review.parsed.into_iter().map(state_parsed).collect()),
                        ..EventInput::new("repo_run", ok, format!("{profile} exit {:?}{}", rr.exit_code, if rr.timed_out { " timed out" } else { "" }))
                    },
                    session.as_deref(),
                    reply,
                )
            }
            Err(message) => event_then(
                &mut state,
                EventInput { command: Some(command.clone()), ..EventInput::new("repo_run", false, message.clone()) },
                session.as_deref(),
                err_reply(message, json!({})),
            ),
        }
    });
    register(
        b,
        "repo_run",
        "Run task profile",
        "Run an allowlisted command profile like typecheck, test, lint, build, or configured project commands. Checks sandbox policy before execution.",
        json!({
            "type": "object",
            "properties": {
                "profile": {"type":"string","minLength":1},
                "timeout_ms": {"type":"integer","minimum":1000,"maximum":900000},
                "approval": {"type":"string","enum":["approved"]}
            },
            "required": ["profile"]
        }),
        obj_schema(json!({
            "ok": {"type":"boolean"}, "profile": {"type":"string"}, "command": {"type":"string"},
            "exitCode": {"type":["number","null"]}, "output": {"type":"string"}, "timedOut": {"type":"boolean"},
            "blob": {}, "needsReview": {"type":"boolean"}, "suggestedRenderMode": {"type":"string"},
            "needsApproval": {"type":"boolean"}, "approval": {}, "error": {"type":"string"}
        })),
        json!({ "readOnlyHint": false, "openWorldHint": false, "destructiveHint": false }),
        app_callable_meta(&["model", "app"]),
        h,
    );
}

/// Convert a tools::ParsedCommandSummary into the state crate's struct for events.
fn state_parsed(p: tools::ParsedCommandSummary) -> crate::state::ParsedCommandSummary {
    crate::state::ParsedCommandSummary {
        kind: p.kind,
        cmd: p.cmd,
        safe: p.safe,
        reason: p.reason,
        path: p.path,
        query: p.query,
    }
}

/// `(relayShell(...) ?? runShell(...))` — try the codex relay first, then local.
async fn run_or_relay(
    relay_url: &Option<String>,
    config: &Config,
    command: &str,
    agent_id: &str,
    timeout_ms: Option<u64>,
    workdir: Option<&str>,
) -> Result<LocalShell, String> {
    if let Some(r) = relay_shell(relay_url, command, agent_id, workdir).await {
        return Ok(LocalShell {
            exit_code: Some(r.exit_code),
            combined: r.combined,
            timed_out: r.timed_out,
            truncated: r.truncated,
            duration_ms: r.duration_ms,
        });
    }
    let r = tools::run_shell(
        config,
        command,
        tools::RunShellOpts { timeout_ms, cwd: workdir.map(String::from), ..Default::default() },
    )
    .await?;
    Ok(LocalShell {
        exit_code: r.exit_code,
        combined: r.combined,
        timed_out: r.timed_out,
        truncated: r.truncated,
        duration_ms: r.duration_ms,
    })
}

struct LocalShell {
    exit_code: Option<i32>,
    combined: String,
    timed_out: bool,
    truncated: bool,
    duration_ms: u128,
}

// --- repo_shell ------------------------------------------------------------

fn register_repo_shell(b: &mut RepoMcpServerBuilder, ctx: &ToolCtx, allow_shell: bool) {
    let h = handler!(ctx, args, session, {
        let config = ctx.config.lock().await.clone();
        let command = arg_str(&args, "command").unwrap_or_default();
        let background = arg_bool(&args, "background").unwrap_or(false);
        let name = arg_str(&args, "name");
        let timeout_ms = arg_u64(&args, "timeout_ms");
        let approved = arg_str(&args, "approval").as_deref() == Some("approved");

        let mut state = ctx.state.lock().await;
        if !config.allow_model_dev_shell && !approved {
            return event_then(
                &mut state,
                EventInput { command: Some(command.clone()), ..EventInput::new("repo_shell", false, "raw shell disabled") },
                session.as_deref(),
                err_reply("Raw shell disabled by config.", json!({})),
            );
        }
        let review = tools::review_command(&config, &command, tools::ReviewOpts { approved, task_profile: false });
        let parsed = serde_json::to_value(&review.parsed).unwrap_or(Value::Null);
        if !review.allowed {
            if review.needs_approval {
                return approval_blocked_reply(
                    &mut state, &config, "repo_shell",
                    review.reason.as_deref().unwrap_or(&format!("run {command}")),
                    Some(command.clone()), None,
                    json!({ "command": command, "background": background, "name": name, "timeout_ms": timeout_ms, "approval": "approved" }),
                    json!({ "command": command, "parsedCommand": parsed }),
                    session.as_deref(),
                );
            }
            let reason = review.reason.clone().unwrap_or_else(|| "Command blocked by policy".to_string());
            return event_then(
                &mut state,
                EventInput { command: Some(command.clone()), parsed_command: Some(review.parsed.into_iter().map(state_parsed).collect()), ..EventInput::new("repo_shell", false, reason.clone()) },
                session.as_deref(),
                err_reply(reason, json!({ "command": command, "parsedCommand": parsed })),
            );
        }
        if background {
            return match tools::start_background(&config, &command, tools::BgStartOpts { name: name.clone(), cwd: None }) {
                Ok(bg) => {
                    let text = format!(
                        "Started {} ({}, pid {}): {command}\nPoll with repo_bg_output({{ id: \"{}\" }}), stop with repo_bg_stop({{ id: \"{}\" }}).",
                        bg.name, bg.id, bg.pid.map(|p| p.to_string()).unwrap_or_else(|| "?".to_string()), bg.id, bg.id
                    );
                    let reply = text_reply(true, json!({ "background": true, "id": bg.id, "name": bg.name, "pid": bg.pid, "command": bg.command, "parsedCommand": parsed }), text, None);
                    event_then(
                        &mut state,
                        EventInput { command: Some(command.clone()), ..EventInput::new("repo_shell", true, format!("bg start {} ({})", bg.name, bg.id)) },
                        session.as_deref(),
                        reply,
                    )
                }
                Err(message) => event_then(
                    &mut state,
                    EventInput { command: Some(command.clone()), ..EventInput::new("repo_shell", false, message.clone()) },
                    session.as_deref(),
                    err_reply(message, json!({})),
                ),
            };
        }
        let agent_id = state.agent_for_session(session.as_deref());
        drop(state);
        let r = run_or_relay(&ctx.relay_url, &config, &command, &agent_id, timeout_ms, None).await;
        let mut state = ctx.state.lock().await;
        match r {
            Ok(rr) => {
                let ok = rr.exit_code == Some(0);
                let blob = keep_log_blob(&mut state, "dev_shell", &rr.combined, should_keep_log(ok, rr.timed_out, &rr.combined, rr.truncated), session.as_deref());
                let needs_review = !ok || blob.is_some();
                let meta = if needs_review {
                    Some(log_meta(&format!("$ {command} -> {:?}{}", rr.exit_code, if rr.timed_out { " timed out" } else { "" }), &rr.combined, blob.as_ref(), rr.truncated))
                } else { None };
                let reply = text_reply(
                    ok,
                    json!({
                        "command": command, "parsedCommand": parsed, "exitCode": rr.exit_code,
                        "output": rr.combined, "timedOut": rr.timed_out, "needsReview": needs_review,
                        "suggestedRenderMode": if needs_review { json!("logs") } else { Value::Null },
                    }),
                    format!("$ {command}\nexit={:?}\n{}", rr.exit_code, rr.combined),
                    meta,
                );
                event_then(
                    &mut state,
                    EventInput {
                        command: Some(command.clone()),
                        exit_code: Some(rr.exit_code.map(|c| c as i64)),
                        blobs: blob.as_ref().map(|bl| vec![bl.id.clone()]),
                        bytes: Some(rr.combined.len() as u64),
                        duration_ms: Some(rr.duration_ms as u64),
                        parsed_command: Some(review.parsed.into_iter().map(state_parsed).collect()),
                        ..EventInput::new("repo_shell", ok, brief(&format!("$ {command} -> {:?}", rr.exit_code)))
                    },
                    session.as_deref(),
                    reply,
                )
            }
            Err(message) => event_then(
                &mut state,
                EventInput { command: Some(command.clone()), ..EventInput::new("repo_shell", false, message.clone()) },
                session.as_deref(),
                err_reply(message, json!({})),
            ),
        }
    });
    register(
        b,
        "repo_shell",
        "Run shell command",
        "Run a shell command in the active project root. Commands are parsed and policy-checked before running. By default it waits and returns the command's output. Set background:true for a long-running command (dev server, watcher, tunnel) — it returns immediately with a process id you poll via repo_bg_output and stop via repo_bg_stop.",
        json!({
            "type": "object",
            "properties": {
                "command": {"type":"string","minLength":1},
                "background": {"type":"boolean"},
                "name": {"type":"string"},
                "timeout_ms": {"type":"integer","minimum":1000,"maximum":900000},
                "approval": {"type":"string","enum":["approved"]}
            },
            "required": ["command"]
        }),
        obj_schema(json!({
            "ok": {"type":"boolean"}, "command": {"type":"string"}, "parsedCommand": {},
            "exitCode": {"type":["number","null"]}, "output": {"type":"string"}, "timedOut": {"type":"boolean"},
            "blob": {}, "needsReview": {"type":"boolean"}, "suggestedRenderMode": {"type":"string"},
            "background": {"type":"boolean"}, "id": {"type":"string"}, "name": {"type":"string"}, "pid": {"type":"number"},
            "needsApproval": {"type":"boolean"}, "approval": {}, "error": {"type":"string"}
        })),
        json!({ "readOnlyHint": false, "openWorldHint": true, "destructiveHint": false }),
        app_callable_meta(&shell_visibility(allow_shell)),
        h,
    );
}

// --- shell_command (codex-native) ------------------------------------------

fn register_shell_command(b: &mut RepoMcpServerBuilder, ctx: &ToolCtx, allow_shell: bool) {
    let h = handler!(ctx, args, session, {
        let config = ctx.config.lock().await.clone();
        let command = arg_str(&args, "command").unwrap_or_default();
        let workdir = arg_str(&args, "workdir");
        let timeout_ms = arg_u64(&args, "timeout_ms");
        let approved = arg_str(&args, "approval").as_deref() == Some("approved");

        let mut state = ctx.state.lock().await;
        if !config.allow_model_dev_shell && !approved {
            return event_then(
                &mut state,
                EventInput { command: Some(command.clone()), ..EventInput::new("shell_command", false, "raw shell disabled") },
                session.as_deref(),
                err_reply("Raw shell disabled by config.", json!({})),
            );
        }
        let review = tools::review_command(&config, &command, tools::ReviewOpts { approved, task_profile: false });
        let parsed = serde_json::to_value(&review.parsed).unwrap_or(Value::Null);
        if !review.allowed {
            if review.needs_approval {
                return approval_blocked_reply(
                    &mut state, &config, "shell_command",
                    review.reason.as_deref().unwrap_or(&format!("run {command}")),
                    Some(command.clone()), None,
                    json!({ "command": command, "workdir": workdir, "timeout_ms": timeout_ms, "approval": "approved" }),
                    json!({ "command": command, "parsedCommand": parsed }),
                    session.as_deref(),
                );
            }
            let reason = review.reason.clone().unwrap_or_else(|| "Command blocked by policy".to_string());
            return event_then(
                &mut state,
                EventInput { command: Some(command.clone()), parsed_command: Some(review.parsed.into_iter().map(state_parsed).collect()), ..EventInput::new("shell_command", false, reason.clone()) },
                session.as_deref(),
                err_reply(reason, json!({ "command": command, "parsedCommand": parsed })),
            );
        }
        let agent_id = state.agent_for_session(session.as_deref());
        drop(state);
        let r = run_or_relay(&ctx.relay_url, &config, &command, &agent_id, timeout_ms, workdir.as_deref()).await;
        let mut state = ctx.state.lock().await;
        match r {
            Ok(rr) => {
                let ok = rr.exit_code == Some(0);
                let blob = keep_log_blob(&mut state, "shell_command", &rr.combined, should_keep_log(ok, rr.timed_out, &rr.combined, rr.truncated), session.as_deref());
                let needs_review = !ok || blob.is_some();
                let meta = if needs_review {
                    Some(log_meta(&format!("$ {command} -> {:?}{}", rr.exit_code, if rr.timed_out { " timed out" } else { "" }), &rr.combined, blob.as_ref(), rr.truncated))
                } else { None };
                let reply = text_reply(
                    ok,
                    json!({
                        "command": command, "exitCode": rr.exit_code, "output": rr.combined,
                        "timedOut": rr.timed_out, "needsReview": needs_review,
                        "suggestedRenderMode": if needs_review { json!("logs") } else { Value::Null },
                        "parsedCommand": parsed,
                    }),
                    format!("$ {command}\nexit={:?}\n{}", rr.exit_code, rr.combined),
                    meta,
                );
                event_then(
                    &mut state,
                    EventInput {
                        command: Some(command.clone()),
                        exit_code: Some(rr.exit_code.map(|c| c as i64)),
                        blobs: blob.as_ref().map(|bl| vec![bl.id.clone()]),
                        bytes: Some(rr.combined.len() as u64),
                        duration_ms: Some(rr.duration_ms as u64),
                        parsed_command: Some(review.parsed.into_iter().map(state_parsed).collect()),
                        ..EventInput::new("shell_command", ok, brief(&format!("$ {command} -> {:?}", rr.exit_code)))
                    },
                    session.as_deref(),
                    reply,
                )
            }
            Err(message) => event_then(
                &mut state,
                EventInput { command: Some(command.clone()), ..EventInput::new("shell_command", false, message.clone()) },
                session.as_deref(),
                err_reply(message, json!({})),
            ),
        }
    });
    register(
        b,
        "shell_command",
        "Run shell command",
        "Runs a shell command (PowerShell on Windows) and returns its output. Your primary tool: read files (Get-Content/cat), search (Select-String/rg), list (Get-ChildItem/ls), run builds and tests — all through this. Set workdir to run somewhere other than the project root.",
        json!({
            "type": "object",
            "properties": {
                "command": {"type":"string","minLength":1},
                "workdir": {"type":"string"},
                "timeout_ms": {"type":"integer","minimum":1000,"maximum":900000},
                "approval": {"type":"string","enum":["approved"]}
            },
            "required": ["command"]
        }),
        obj_schema(json!({
            "ok": {"type":"boolean"}, "command": {"type":"string"}, "exitCode": {"type":["number","null"]},
            "output": {"type":"string"}, "timedOut": {"type":"boolean"}, "needsApproval": {"type":"boolean"},
            "approval": {}, "error": {"type":"string"}
        })),
        json!({ "readOnlyHint": false, "openWorldHint": true, "destructiveHint": false }),
        app_callable_meta(&shell_visibility(allow_shell)),
        h,
    );
}

// --- apply_patch (codex-native) --------------------------------------------

fn register_apply_patch(b: &mut RepoMcpServerBuilder, ctx: &ToolCtx, allow_write: bool) {
    let h = handler!(ctx, args, session, {
        let config = ctx.config.lock().await.clone();
        let input = arg_str(&args, "input").unwrap_or_default();
        let approved = arg_str(&args, "approval").as_deref() == Some("approved");

        let mut state = ctx.state.lock().await;
        if config.sandbox_mode == tools::SandboxMode::ReadOnly && !approved {
            return approval_blocked_reply(
                &mut state, &config, "apply_patch", "apply patch",
                None, None,
                json!({ "input": input, "approval": "approved" }),
                json!({}), session.as_deref(),
            );
        }
        let agent_id = state.agent_for_session(session.as_deref());
        drop(state);
        let patch_out = relay_patch(&ctx.relay_url, &input, &agent_id).await;
        let mut state = ctx.state.lock().await;
        match patch_out {
            None => event_then(
                &mut state,
                EventInput::new("apply_patch", false, "no codex relay"),
                session.as_deref(),
                err_reply("apply_patch needs the codex relay (disabled, or this is a local-subagent session). Use repo_edit for an existing file or repo_write for a new one.", json!({})),
            ),
            Some(out) => {
                let ok = patch_succeeded(&out, true);
                let reply = text_reply(ok, json!({ "ok": ok, "output": out }), out, None);
                event_then(
                    &mut state,
                    EventInput::new("apply_patch", ok, if ok { "apply_patch ok (codex)".to_string() } else { "apply_patch failed".to_string() }),
                    session.as_deref(),
                    reply,
                )
            }
        }
    });
    register(
        b,
        "apply_patch",
        "Apply patch",
        "Create, edit, or delete files by submitting a patch in the apply_patch envelope format:\n*** Begin Patch\n*** Update File: path/to/file\n@@ context\n-old line\n+new line\n*** End Patch\nUse *** Add File: for new files (each line prefixed +) and *** Delete File: to remove one. Pass the whole envelope as `input`. This is the primary way to change files; renders a native diff.",
        json!({
            "type": "object",
            "properties": { "input": {"type":"string","minLength":1}, "approval": {"type":"string","enum":["approved"]} },
            "required": ["input"]
        }),
        obj_schema(json!({
            "ok": {"type":"boolean"}, "output": {"type":"string"}, "needsApproval": {"type":"boolean"},
            "approval": {}, "error": {"type":"string"}
        })),
        json!({ "readOnlyHint": false, "openWorldHint": false, "destructiveHint": false }),
        app_callable_meta(&write_visibility(allow_write)),
        h,
    );
}

// --- update_plan (codex-native) --------------------------------------------

fn register_update_plan(b: &mut RepoMcpServerBuilder, ctx: &ToolCtx) {
    let h = handler!(ctx, args, session, {
        let plan = args.get("plan").cloned().unwrap_or(json!([]));
        let explanation = arg_str(&args, "explanation");
        let agent_id = ctx.state.lock().await.agent_for_session(session.as_deref());
        let out = relay_plan(&ctx.relay_url, &plan, explanation.as_deref(), &agent_id).await;
        let steps = plan.as_array().cloned().unwrap_or_default();
        let summary_lines = steps
            .iter()
            .map(|p| {
                let status = p.get("status").and_then(|s| s.as_str()).unwrap_or("");
                let step = p.get("step").and_then(|s| s.as_str()).unwrap_or("");
                let mark = match status { "completed" => "x", "in_progress" => "~", _ => " " };
                format!("[{mark}] {step}")
            })
            .collect::<Vec<_>>()
            .join("\n");
        let mut state = ctx.state.lock().await;
        match out {
            None => event_then(
                &mut state,
                EventInput::new("update_plan", true, format!("plan noted ({} steps)", steps.len())),
                session.as_deref(),
                text_reply(true, json!({ "ok": true, "output": summary_lines }), summary_lines, None),
            ),
            Some(out) => {
                let ok = !out.starts_with("relay to codex failed");
                let text = if out.is_empty() { summary_lines.clone() } else { out.clone() };
                let summary = if ok { format!("plan updated ({} steps)", steps.len()) } else { brief(&out) };
                event_then(
                    &mut state,
                    EventInput::new("update_plan", ok, summary),
                    session.as_deref(),
                    text_reply(ok, json!({ "ok": ok, "output": text }), text, None),
                )
            }
        }
    });
    register(
        b,
        "update_plan",
        "Update plan",
        "Update the task plan (TODO list) shown to the user. Provide a list of steps, each with a status of pending, in_progress, or completed; at most one step in_progress at a time. Use for non-trivial multi-step work; skip it for single-step queries. Renders natively in the codex TUI.",
        json!({
            "type": "object",
            "properties": {
                "explanation": {"type":"string"},
                "plan": {
                    "type": "array", "minItems": 1,
                    "items": { "type": "object", "properties": {
                        "step": {"type":"string","minLength":1},
                        "status": {"type":"string","enum":["pending","in_progress","completed"]}
                    }, "required": ["step","status"] }
                }
            },
            "required": ["plan"]
        }),
        obj_schema(json!({ "ok": {"type":"boolean"}, "output": {"type":"string"}, "error": {"type":"string"} })),
        json!({ "readOnlyHint": false, "openWorldHint": false, "destructiveHint": false }),
        app_callable_meta(&["model", "app"]),
        h,
    );
}

// --- repo_bg_output --------------------------------------------------------

fn register_repo_bg_output(b: &mut RepoMcpServerBuilder, ctx: &ToolCtx) {
    let h = handler!(ctx, args, session, {
        let id = arg_str(&args, "id").unwrap_or_default();
        let full = arg_bool(&args, "full").unwrap_or(false);
        let max_bytes = arg_u64(&args, "max_bytes").map(|n| n as usize);
        let mut state = ctx.state.lock().await;
        match tools::read_background(&id, tools::BgReadOpts { full, max_bytes }) {
            Ok(r) => {
                let head = format!(
                    "{} ({}) — {}",
                    r.name, r.id,
                    if r.running { "running".to_string() } else { format!("{} exit {:?}", r.status, r.exit_code) }
                );
                let body = if r.output.is_empty() { "[no new output]".to_string() } else { r.output.clone() };
                let meta = if !r.output.is_empty() { Some(log_meta(&head, &r.output, None, false)) } else { None };
                let reply = text_reply(
                    true,
                    json!({ "id": r.id, "name": r.name, "status": r.status, "running": r.running, "exitCode": r.exit_code, "output": r.output, "newBytes": r.new_bytes }),
                    format!("{head}\n{body}"),
                    meta,
                );
                event_then(
                    &mut state,
                    EventInput { bytes: Some(r.output.len() as u64), ..EventInput::new("repo_bg_output", true, format!("{} {} +{}b", r.name, if r.running { "running".to_string() } else { format!("exit {:?}", r.exit_code) }, r.new_bytes)) },
                    session.as_deref(),
                    reply,
                )
            }
            Err(message) => event_then(
                &mut state,
                EventInput::new("repo_bg_output", false, message.clone()),
                session.as_deref(),
                err_reply(message, json!({})),
            ),
        }
    });
    register(
        b,
        "repo_bg_output",
        "Read background output",
        "Read output produced by a background process since the last time you read it (pass full:true for the whole buffer). Returns whether it is still running and its exit code if it finished. Poll this to watch a dev server boot, a build progress, or a watcher re-run.",
        json!({
            "type": "object",
            "properties": {
                "id": {"type":"string","minLength":1},
                "full": {"type":"boolean"},
                "max_bytes": {"type":"integer","minimum":1000,"maximum":1000000}
            },
            "required": ["id"]
        }),
        obj_schema(json!({
            "ok": {"type":"boolean"}, "id": {"type":"string"}, "name": {"type":"string"}, "status": {"type":"string"},
            "running": {"type":"boolean"}, "exitCode": {"type":["number","null"]}, "output": {"type":"string"},
            "newBytes": {"type":"number"}, "error": {"type":"string"}
        })),
        json!({ "readOnlyHint": true }),
        app_callable_meta(&["model", "app"]),
        h,
    );
}

// --- repo_bg_stop ----------------------------------------------------------

fn register_repo_bg_stop(b: &mut RepoMcpServerBuilder, ctx: &ToolCtx) {
    let h = handler!(ctx, args, session, {
        let id = arg_str(&args, "id").unwrap_or_default();
        let mut state = ctx.state.lock().await;
        match tools::stop_background(&id) {
            Ok(r) => {
                let reply = text_reply(true, json!({ "id": r.id, "name": r.name, "status": r.status, "exitCode": r.exit_code }), format!("Stopped {} ({}): {}.", r.name, r.id, r.status), None);
                event_then(&mut state, EventInput::new("repo_bg_stop", true, format!("stop {} ({})", r.name, r.id)), session.as_deref(), reply)
            }
            Err(message) => event_then(&mut state, EventInput::new("repo_bg_stop", false, message.clone()), session.as_deref(), err_reply(message, json!({}))),
        }
    });
    register(
        b,
        "repo_bg_stop",
        "Stop background process",
        "Terminate a background process and its child tree by id. Always stop background processes you started once you no longer need them.",
        json!({ "type": "object", "properties": { "id": {"type":"string","minLength":1} }, "required": ["id"] }),
        obj_schema(json!({ "ok": {"type":"boolean"}, "id": {"type":"string"}, "name": {"type":"string"}, "status": {"type":"string"}, "exitCode": {"type":["number","null"]}, "error": {"type":"string"} })),
        json!({ "readOnlyHint": false, "openWorldHint": false, "destructiveHint": true }),
        app_callable_meta(&["model", "app"]),
        h,
    );
}

// --- repo_bg_list ----------------------------------------------------------

fn register_repo_bg_list(b: &mut RepoMcpServerBuilder, ctx: &ToolCtx) {
    let h = handler!(ctx, _args, session, {
        let procs = tools::list_background();
        let mut state = ctx.state.lock().await;
        let text = if procs.is_empty() {
            "No background processes.".to_string()
        } else {
            procs.iter().map(|p| {
                let exit = match p.exit_code { Some(c) => format!(" exit {c}"), None => String::new() };
                format!("{} {} [{}{}] {}", p.id, p.name, p.status, exit, p.command)
            }).collect::<Vec<_>>().join("\n")
        };
        let reply = text_reply(true, json!({ "processes": serde_json::to_value(&procs).unwrap_or(json!([])) }), text, None);
        event_then(&mut state, EventInput::new("repo_bg_list", true, format!("{} bg procs", procs.len())), session.as_deref(), reply)
    });
    register(
        b,
        "repo_bg_list",
        "List background processes",
        "List background processes started this session with their status, exit code, and buffered output size. Use this to find an id you lost track of.",
        obj_schema(json!({})),
        obj_schema(json!({ "ok": {"type":"boolean"}, "processes": {"type":"array","items":{}} })),
        json!({ "readOnlyHint": true }),
        app_callable_meta(&["model", "app"]),
        h,
    );
}

// --- repo_permissions ------------------------------------------------------

fn register_repo_permissions(b: &mut RepoMcpServerBuilder, ctx: &ToolCtx) {
    let h = handler!(ctx, args, session, {
        let action = arg_str(&args, "action").unwrap_or_else(|| "show".to_string());
        let profile = arg_str(&args, "profile");
        let sandbox_mode = arg_str(&args, "sandbox_mode");
        let approval_policy = arg_str(&args, "approval_policy");
        let approval_id = arg_str(&args, "approval_id");

        let mut config = ctx.config.lock().await;
        let mut state = ctx.state.lock().await;
        let result: Result<ToolReply, String> = (|| {
            if action == "profiles" {
                let perms = tools::effective_permissions(&config);
                let text = serde_json::to_string_pretty(&json!({
                    "permissions": serde_json::to_value(&perms).unwrap_or(Value::Null),
                    "profiles": config_profiles_value(&config),
                })).unwrap_or_default();
                let reply = text_reply(true, json!({
                    "profiles": config_profiles_value(&config),
                    "permissions": serde_json::to_value(&perms).unwrap_or(Value::Null),
                    "approvals": serde_json::to_value(state.approvals()).unwrap_or(json!([])),
                }), text, None);
                state.event(EventInput::new("repo_permissions", true, "profiles"), session.as_deref());
                return Ok(reply);
            }
            if action == "approve" || action == "deny" {
                let approval_id = approval_id.clone().ok_or_else(|| "approval_id is required".to_string())?;
                let status = if action == "approve" { "approved" } else { "denied" };
                let approval = state.decide_approval(&approval_id, status, session.as_deref())
                    .ok_or_else(|| format!("Approval not found: {approval_id}"))?;
                let perms = tools::effective_permissions(&config);
                return Ok(text_reply(true, json!({
                    "approval": serde_json::to_value(&approval).unwrap_or(Value::Null),
                    "permissions": serde_json::to_value(&perms).unwrap_or(Value::Null),
                    "approvals": serde_json::to_value(state.approvals()).unwrap_or(json!([])),
                }), format!("{} {approval_id}.", if action == "approve" { "Approved" } else { "Denied" }), None));
            }
            if action == "set" || profile.is_some() || sandbox_mode.is_some() || approval_policy.is_some() {
                let perms = tools::set_permissions(&mut config, tools::SetPermsOpts {
                    profile: profile.clone(),
                    sandbox_mode: sandbox_mode.as_deref().and_then(tools::SandboxMode::parse),
                    approval_policy: approval_policy.as_deref().and_then(tools::ApprovalPolicy::parse),
                    reviewer: None,
                }).map_err(|e| e.0)?;
                sync_config_into_state(&config, &mut state);
                let reply = text_reply(true, json!({
                    "permissions": serde_json::to_value(&perms).unwrap_or(Value::Null),
                    "approvals": serde_json::to_value(state.approvals()).unwrap_or(json!([])),
                }), format!("Permissions now {}/{}.", perms.sandbox_mode, perms.approval_policy), None);
                state.event(EventInput::new("repo_permissions", true, format!("{}/{}", perms.sandbox_mode, perms.approval_policy)), session.as_deref());
                return Ok(reply);
            }
            let perms = tools::effective_permissions(&config);
            let pending: Vec<_> = state.approvals().into_iter().filter(|a| a.status == "pending").collect();
            let text = serde_json::to_string_pretty(&json!({
                "permissions": serde_json::to_value(&perms).unwrap_or(Value::Null),
                "pendingApprovals": serde_json::to_value(&pending).unwrap_or(json!([])),
            })).unwrap_or_default();
            let reply = text_reply(true, json!({
                "permissions": serde_json::to_value(&perms).unwrap_or(Value::Null),
                "profiles": config_profiles_value(&config),
                "approvals": serde_json::to_value(state.approvals()).unwrap_or(json!([])),
            }), text, None);
            state.event(EventInput::new("repo_permissions", true, format!("{}/{}", perms.sandbox_mode, perms.approval_policy)), session.as_deref());
            Ok(reply)
        })();
        match result {
            Ok(reply) => reply,
            Err(message) => {
                state.event(EventInput::new("repo_permissions", false, message.clone()), session.as_deref());
                err_reply(message, json!({}))
            }
        }
    });
    register(
        b,
        "repo_permissions",
        "Permissions",
        "Show or update Codex-style permission posture: read-only, workspace-write, danger-full-access; approval policies untrusted, on-request, never; pending approvals.",
        obj_schema(json!({
            "action": {"type":"string","enum":["show","set","profiles","approve","deny"]},
            "profile": {"type":"string"},
            "sandbox_mode": {"type":"string","enum":["read-only","workspace-write","danger-full-access"]},
            "approval_policy": {"type":"string","enum":["untrusted","on-request","never"]},
            "approval_id": {"type":"string"}
        })),
        obj_schema(json!({ "ok": {"type":"boolean"}, "permissions": {}, "profiles": {}, "approvals": {}, "approval": {}, "error": {"type":"string"} })),
        json!({ "readOnlyHint": false, "openWorldHint": false, "destructiveHint": false }),
        app_callable_meta(&["model", "app"]),
        h,
    );
}

/// `config.permissionProfiles` as a JSON object (names -> profile fields).
fn config_profiles_value(config: &Config) -> Value {
    let mut map = serde_json::Map::new();
    for (name, p) in &config.permission_profiles {
        map.insert(name.clone(), json!({
            "sandboxMode": p.sandbox_mode.map(|m| m.as_str()),
            "approvalPolicy": p.approval_policy.map(|a| a.as_str()),
        }));
    }
    Value::Object(map)
}

/// Mirror the config fields the event/snapshot layer reads back into RepoState.
fn sync_config_into_state(config: &Config, state: &mut RepoState) {
    state.config.root = config.root.to_string_lossy().to_string();
    state.config.current_project = config.current_project.clone();
    state.config.sandbox_mode = config.sandbox_mode.as_str().to_string();
    state.config.approval_policy = config.approval_policy.as_str().to_string();
    state.config.approvals_reviewer = match config.approvals_reviewer {
        tools::ApprovalReviewer::User => "user".to_string(),
        tools::ApprovalReviewer::AutoReview => "auto_review".to_string(),
    };
    state.config.writable_roots = config.writable_roots.iter().map(|p| p.to_string_lossy().to_string()).collect();
}

// --- repo_project ----------------------------------------------------------

fn register_repo_project(b: &mut RepoMcpServerBuilder, ctx: &ToolCtx) {
    let h = handler!(ctx, args, session, {
        let action = arg_str(&args, "action").unwrap_or_else(|| "list".to_string());
        let query = arg_str(&args, "query");
        let path = arg_str(&args, "path");
        let max = arg_u64(&args, "max").map(|n| n as usize);

        let mut config = ctx.config.lock().await;
        if action == "switch" {
            let target = path.clone().or_else(|| query.clone());
            let target = match target {
                Some(t) => t,
                None => {
                    let mut state = ctx.state.lock().await;
                    state.event(EventInput::new("repo_project", false, "Provide path or query to switch projects."), session.as_deref());
                    return err_reply("Provide path or query to switch projects.", json!({}));
                }
            };
            match tools::switch_project(&mut config, &target) {
                Ok(project) => {
                    let mut state = ctx.state.lock().await;
                    // Repoint the durable state at the new root + mirror config fields.
                    let _ = state.switch_root(&config.root.to_string_lossy());
                    sync_config_into_state(&config, &mut state);
                    state.event(EventInput { project: Some(config.root.to_string_lossy().to_string()), ..EventInput::new("repo_project", true, format!("switched to {}", project.name)) }, session.as_deref());
                    let reply = text_reply(true, json!({
                        "activeProject": serde_json::to_value(&project).unwrap_or(Value::Null),
                        "repoRoot": config.root.to_string_lossy(),
                        "suggestedRenderMode": "projects",
                    }), format!("Switched to {}\n{}", project.name, config.root.to_string_lossy()), None);
                    return with_compaction(&mut state, reply, "tool-loop");
                }
                Err(message) => {
                    let mut state = ctx.state.lock().await;
                    state.event(EventInput::new("repo_project", false, message.clone()), session.as_deref());
                    return err_reply(message, json!({}));
                }
            }
        }
        let projects = tools::discover_projects(&config, tools::DiscoverOpts {
            query: if action == "search" { query.clone() } else { None },
            max,
        });
        let text = if projects.is_empty() {
            "No projects found.".to_string()
        } else {
            projects.iter().map(|p| {
                let marker = if p.selected { "*" } else { "-" };
                let desc = p.description.as_ref().map(|d| format!("\n  {}", tools::brief(d, 180))).unwrap_or_default();
                format!("{marker} {} — {}{desc}", p.name, p.root)
            }).collect::<Vec<_>>().join("\n")
        };
        let mut state = ctx.state.lock().await;
        let reply = text_reply(true, json!({
            "projects": serde_json::to_value(&projects).unwrap_or(json!([])),
            "repoRoot": config.root.to_string_lossy(),
            "suggestedRenderMode": "projects",
        }), text, None);
        event_then(&mut state, EventInput::new("repo_project", true, format!("{action} {} projects", projects.len())), session.as_deref(), reply)
    });
    register(
        b,
        "repo_project",
        "Project atlas",
        "List, search, summarize, or switch active project. Lets the agent work outside a single repo by choosing from configured/discovered project roots instead of arbitrary filesystem roaming.",
        obj_schema(json!({
            "action": {"type":"string","enum":["list","search","switch","summarize"]},
            "query": {"type":"string"},
            "path": {"type":"string"},
            "max": {"type":"integer","minimum":1,"maximum":100}
        })),
        obj_schema(json!({ "ok": {"type":"boolean"}, "projects": {"type":"array","items":{}}, "project": {}, "repoRoot": {"type":"string"}, "suggestedRenderMode": {"type":"string"}, "error": {"type":"string"} })),
        json!({ "readOnlyHint": false, "openWorldHint": false, "destructiveHint": false }),
        app_callable_meta(&["model", "app"]),
        h,
    );
}

// --- repo_slash_command ----------------------------------------------------

fn register_repo_slash_command(b: &mut RepoMcpServerBuilder, ctx: &ToolCtx) {
    let h = handler!(ctx, args, session, {
        let text_in = arg_str(&args, "text").unwrap_or_default();
        let mut config = ctx.config.lock().await;
        let mut state = ctx.state.lock().await;
        match handle_slash(&ctx.relay_url, &mut config, &mut state, &text_in, session.as_deref()).await {
            Ok(result) => {
                let render = result.get("suggestedRenderMode").cloned();
                let meta = result.get("meta").cloned();
                let body = result.get("text").and_then(|v| v.as_str()).map(String::from)
                    .unwrap_or_else(|| serde_json::to_string_pretty(&result).unwrap_or_default());
                let reply = text_reply(true, json!({ "command": text_in, "result": result, "suggestedRenderMode": render }), body, meta);
                event_then(&mut state, EventInput::new("repo_slash_command", true, text_in.clone()), session.as_deref(), reply)
            }
            Err(message) => event_then(
                &mut state,
                EventInput::new("repo_slash_command", false, format!("{text_in}: {message}")),
                session.as_deref(),
                err_reply(message, json!({ "command": text_in })),
            ),
        }
    });
    register(
        b,
        "repo_slash_command",
        "Slash command",
        "Execute a slash command such as /permissions, /project, /status, /diff, /logs, /compact, /init, /mcp, or /help. Data-only unless the caller then renders the UI.",
        json!({ "type": "object", "properties": { "text": {"type":"string","minLength":1} }, "required": ["text"] }),
        obj_schema(json!({ "ok": {"type":"boolean"}, "command": {"type":"string"}, "result": {}, "suggestedRenderMode": {"type":"string"}, "error": {"type":"string"} })),
        json!({ "readOnlyHint": false, "openWorldHint": false, "destructiveHint": false }),
        app_callable_meta(&["model", "app"]),
        h,
    );
}

// --- repo_remember ---------------------------------------------------------

fn register_repo_remember(b: &mut RepoMcpServerBuilder, ctx: &ToolCtx) {
    let h = handler!(ctx, args, session, {
        let kind = arg_str(&args, "kind").unwrap_or_default();
        let text_in = arg_str(&args, "text").unwrap_or_default();
        let files = arg_str_array(&args, "files");
        let mut state = ctx.state.lock().await;
        let note = state.remember(&kind, &text_in, files, session.as_deref());
        let reply = text_reply(true, json!({ "note": serde_json::to_value(&note).unwrap_or(Value::Null) }), format!("Remembered {kind}: {text_in}"), None);
        with_compaction(&mut state, reply, "tool-loop")
    });
    register(
        b,
        "repo_remember",
        "Remember repo note",
        "Store a durable agent note: decisions, constraints, TODOs, or watchpoints. Auto-compaction uses these as context anchors.",
        json!({
            "type": "object",
            "properties": {
                "kind": {"type":"string","enum":["decision","constraint","todo","watch","note"]},
                "text": {"type":"string","minLength":1},
                "files": {"type":"array","items":{"type":"string"}}
            },
            "required": ["kind","text"]
        }),
        obj_schema(json!({ "ok": {"type":"boolean"}, "note": {}, "snapshot": {} })),
        json!({ "readOnlyHint": false, "openWorldHint": false, "destructiveHint": false }),
        model_meta(),
        h,
    );
}

// --- repo_compact ----------------------------------------------------------

fn register_repo_compact(b: &mut RepoMcpServerBuilder, ctx: &ToolCtx) {
    let h = handler!(ctx, args, session, {
        let reason = arg_str(&args, "reason").unwrap_or_else(|| "manual".to_string());
        let mut state = ctx.state.lock().await;
        let capsule = state.create_capsule(&reason);
        let reply = text_reply(true, json!({ "capsule": serde_json::to_value(&capsule).unwrap_or(Value::Null), "suggestedRenderMode": "capsules" }), capsule.markdown.clone(), None);
        state.event(
            EventInput {
                blobs: Some(capsule.hot_blobs.clone()),
                files: Some(capsule.hot_files.clone()),
                bytes: Some(capsule.markdown.len() as u64),
                ..EventInput::new("repo_compact", true, format!("{}: {reason}", capsule.id))
            },
            session.as_deref(),
        );
        reply
    });
    register(
        b,
        "repo_compact",
        "Force context compaction",
        "Generate a loss-minimized context capsule from recent tool events, hot files, approvals, failures, and durable notes. Data-only; render capsules only when human review is useful.",
        obj_schema(json!({ "reason": {"type":"string"} })),
        obj_schema(json!({ "ok": {"type":"boolean"}, "capsule": {}, "snapshot": {}, "suggestedRenderMode": {"type":"string"} })),
        json!({ "readOnlyHint": false, "openWorldHint": false, "destructiveHint": false }),
        app_callable_meta(&["model", "app"]),
        h,
    );
}

// --- repo_resume -----------------------------------------------------------

fn register_repo_resume(b: &mut RepoMcpServerBuilder, ctx: &ToolCtx) {
    let h = handler!(ctx, args, session, {
        let mode = arg_str(&args, "mode").unwrap_or_else(|| "groove".to_string());
        let config = ctx.config.lock().await.clone();
        let diff_bytes = match mode.as_str() { "max" => 400_000, "tiny" => 40_000, _ => 140_000 };
        let diff = tools::safe_git_diff(&config, Some(diff_bytes)).await;
        let mut state = ctx.state.lock().await;
        let cap_take = if mode == "tiny" { 2 } else { 6 };
        let caps = state.capsules().into_iter().take(cap_take).map(|c| c.markdown).collect::<Vec<_>>().join("\n\n---\n\n");
        let note_take = if mode == "tiny" { 10 } else { 40 };
        let notes = state.notes().into_iter().take(note_take).map(|n| {
            let files = if n.files.is_empty() { String::new() } else { format!(" ({})", n.files.join(", ")) };
            format!("- [{}] {}{}", n.kind, n.text, files)
        }).collect::<Vec<_>>().join("\n");
        let approvals = state.approvals().into_iter().filter(|a| a.status == "pending")
            .map(|a| format!("- {} {}: {}", a.id, a.tool, a.reason)).collect::<Vec<_>>().join("\n");
        let pack = format!(
            "# Repo Context Pack ({mode})\n\nRoot: {}\nSandbox: {}\nApproval: {}\n\n## Durable Notes\n{}\n\n## Pending Approvals\n{}\n\n## Current Diff\n```diff\n{}\n```\n\n## Capsules\n{}\n\n## Instruction\nContinue from this pack. Prefer exact tool reads over asking the user to restate context.",
            config.root.to_string_lossy(), config.sandbox_mode.as_str(), config.approval_policy.as_str(),
            if notes.is_empty() { "none" } else { &notes },
            if approvals.is_empty() { "none" } else { &approvals },
            diff,
            if caps.is_empty() { "none yet" } else { &caps },
        );
        let reply = text_reply(true, json!({ "pack": pack, "packMode": mode }), pack.clone(), None);
        event_then(&mut state, EventInput { bytes: Some(pack.len() as u64), ..EventInput::new("repo_resume", true, format!("mode={mode} {} chars", pack.len())) }, session.as_deref(), reply)
    });
    register(
        b,
        "repo_resume",
        "Resume context",
        "Return the compact canonical state (notes, pending approvals, current diff, capsules) so you can continue work without replaying the whole conversation. Call this before a long loop or after a compaction notice.",
        obj_schema(json!({ "mode": {"type":"string","enum":["tiny","groove","max"]} })),
        obj_schema(json!({ "ok": {"type":"boolean"}, "pack": {"type":"string"}, "mode": {"type":"string"}, "snapshot": {} })),
        json!({ "readOnlyHint": true }),
        app_callable_meta(&["model", "app"]),
        h,
    );
}

// --- repo_logs -------------------------------------------------------------

fn register_repo_logs(b: &mut RepoMcpServerBuilder, ctx: &ToolCtx) {
    let h = handler!(ctx, args, session, {
        let blob_id = arg_str(&args, "blob_id").unwrap_or_default();
        let max_bytes = arg_u64(&args, "max_bytes").map(|n| n as usize);
        let config = ctx.config.lock().await.clone();
        let mut state = ctx.state.lock().await;
        match state.read_blob(&blob_id) {
            None => event_then(
                &mut state,
                EventInput { blobs: Some(vec![blob_id.clone()]), ..EventInput::new("repo_logs", false, format!("missing {blob_id}")) },
                session.as_deref(),
                err_reply(format!("Blob not found: {blob_id}"), json!({ "blob_id": blob_id })),
            ),
            Some(content) => {
                let (trimmed, truncated) = tools::trim_middle(&content, max_bytes.unwrap_or(config.max_read_bytes));
                let meta = log_meta(&blob_id, &trimmed, None, truncated);
                let reply = text_reply(true, json!({ "blob_id": blob_id, "content": trimmed, "truncated": truncated }), trimmed.clone(), Some(meta));
                event_then(
                    &mut state,
                    EventInput { blobs: Some(vec![blob_id.clone()]), bytes: Some(trimmed.len() as u64), ..EventInput::new("repo_logs", true, format!("{blob_id} {} chars", content.len())) },
                    session.as_deref(),
                    reply,
                )
            }
        }
    });
    register(
        b,
        "repo_logs",
        "Read saved log",
        "Read the full saved output of an earlier command that was too long to inline. Pass the blob_id referenced in a previous tool result (e.g. a failed/long repo_run or repo_shell). Use this when you need the part of a log that got trimmed.",
        json!({
            "type": "object",
            "properties": { "blob_id": {"type":"string","minLength":1}, "max_bytes": {"type":"integer","minimum":1000,"maximum":1000000} },
            "required": ["blob_id"]
        }),
        obj_schema(json!({ "ok": {"type":"boolean"}, "blob_id": {"type":"string"}, "content": {"type":"string"}, "truncated": {"type":"boolean"}, "error": {"type":"string"} })),
        json!({ "readOnlyHint": true }),
        app_callable_meta(&["model", "app"]),
        h,
    );
}

// --- request_user_input (codex-native) -------------------------------------

fn register_request_user_input(b: &mut RepoMcpServerBuilder, ctx: &ToolCtx) {
    let h = handler!(ctx, args, session, {
        let questions = args.get("questions").cloned().unwrap_or(json!([]));
        let count = questions.as_array().map(|a| a.len()).unwrap_or(0);
        let agent_id = ctx.state.lock().await.agent_for_session(session.as_deref());
        let out = relay_function(&ctx.relay_url, "request_user_input", json!({ "questions": questions }), &agent_id).await;
        let mut state = ctx.state.lock().await;
        match out {
            None => event_then(
                &mut state,
                EventInput::new("request_user_input", false, "no interactive surface (no relay)"),
                session.as_deref(),
                err_reply("request_user_input needs the codex relay (disabled, or this is a local-subagent session). Do not retry; proceed with your best judgment and state the assumption, or ask the question in your final message.", json!({})),
            ),
            Some(out) => {
                let ok = !out.starts_with("relay to codex failed");
                let summary = if ok { format!("user answered ({count} question{})", if count == 1 { "" } else { "s" }) } else { brief(&out) };
                event_then(&mut state, EventInput::new("request_user_input", ok, summary), session.as_deref(), text_reply(ok, json!({ "ok": ok, "output": out }), out, None))
            }
        }
    });
    register(
        b,
        "request_user_input",
        "Request user input",
        "Ask the user one to three short multiple-choice questions and wait for their answers; the prompt renders natively in the codex TUI. Use only when blocked on a decision you cannot make yourself. Give each question 2-3 mutually exclusive options, recommended option first with its label suffixed '(Recommended)'. Do not include an 'Other' option — the client adds one automatically.",
        json!({
            "type": "object",
            "properties": {
                "questions": {
                    "type": "array", "minItems": 1, "maxItems": 3,
                    "items": { "type": "object", "properties": {
                        "id": {"type":"string","minLength":1},
                        "header": {"type":"string","minLength":1},
                        "question": {"type":"string","minLength":1},
                        "options": { "type": "array", "minItems": 1, "items": { "type": "object", "properties": {
                            "label": {"type":"string","minLength":1},
                            "description": {"type":"string"}
                        }, "required": ["label","description"] } }
                    }, "required": ["id","header","question","options"] }
                }
            },
            "required": ["questions"]
        }),
        obj_schema(json!({ "ok": {"type":"boolean"}, "output": {"type":"string"}, "error": {"type":"string"} })),
        json!({ "readOnlyHint": false, "openWorldHint": false, "destructiveHint": false }),
        app_callable_meta(&["model", "app"]),
        h,
    );
}

// --- goal tools (codex ext/goal) -------------------------------------------

fn register_goal_tools(b: &mut RepoMcpServerBuilder, ctx: &ToolCtx) {
    // get_goal
    let h = handler!(ctx, _args, session, {
        let agent_id = ctx.state.lock().await.agent_for_session(session.as_deref());
        let out = relay_function(&ctx.relay_url, "get_goal", json!({}), &agent_id).await;
        let mut state = ctx.state.lock().await;
        match out {
            None => {
                let msg = "No goal is defined for this thread.";
                event_then(&mut state, EventInput::new("get_goal", true, "no goal (no relay)"), session.as_deref(), text_reply(true, json!({ "ok": true, "output": msg }), msg, None))
            }
            Some(out) => {
                let ok = !out.starts_with("relay to codex failed");
                event_then(&mut state, EventInput::new("get_goal", ok, brief(&out)), session.as_deref(), text_reply(ok, json!({ "ok": ok, "output": out }), out, None))
            }
        }
    });
    register(
        b, "get_goal", "Get goal",
        "Get the current goal for this thread, including status, budgets, token and elapsed-time usage, and remaining token budget.",
        obj_schema(json!({})),
        obj_schema(json!({ "ok": {"type":"boolean"}, "output": {"type":"string"}, "error": {"type":"string"} })),
        json!({ "readOnlyHint": true }),
        app_callable_meta(&["model", "app"]),
        h,
    );

    // create_goal
    let h = handler!(ctx, args, session, {
        let objective = arg_str(&args, "objective").unwrap_or_default();
        let token_budget = arg_u64(&args, "token_budget");
        let agent_id = ctx.state.lock().await.agent_for_session(session.as_deref());
        let relay_args = match token_budget { Some(tb) => json!({ "objective": objective, "token_budget": tb }), None => json!({ "objective": objective }) };
        let out = relay_function(&ctx.relay_url, "create_goal", relay_args, &agent_id).await;
        let mut state = ctx.state.lock().await;
        match out {
            None => event_then(&mut state, EventInput::new("create_goal", false, "no goal system (no relay)"), session.as_deref(),
                err_reply("Goal tracking lives in codex (relay disabled, or local-subagent session). Do not retry; continue the task without a tracked goal.", json!({}))),
            Some(out) => {
                let ok = !out.starts_with("relay to codex failed");
                let summary = if ok { brief(&format!("goal: {objective}")) } else { brief(&out) };
                event_then(&mut state, EventInput::new("create_goal", ok, summary), session.as_deref(), text_reply(ok, json!({ "ok": ok, "output": out }), out, None))
            }
        }
    });
    register(
        b, "create_goal", "Create goal",
        "Create a goal only when explicitly requested by the user or system/developer instructions; do not infer goals from ordinary tasks. Set token_budget only when an explicit token budget is requested. Fails if a goal already exists; use update_goal only for status.",
        json!({
            "type": "object",
            "properties": { "objective": {"type":"string","minLength":1}, "token_budget": {"type":"integer","minimum":1} },
            "required": ["objective"]
        }),
        obj_schema(json!({ "ok": {"type":"boolean"}, "output": {"type":"string"}, "error": {"type":"string"} })),
        json!({ "readOnlyHint": false, "openWorldHint": false, "destructiveHint": false }),
        app_callable_meta(&["model", "app"]),
        h,
    );

    // update_goal
    let h = handler!(ctx, args, session, {
        let status = arg_str(&args, "status").unwrap_or_default();
        let agent_id = ctx.state.lock().await.agent_for_session(session.as_deref());
        let out = relay_function(&ctx.relay_url, "update_goal", json!({ "status": status }), &agent_id).await;
        let mut state = ctx.state.lock().await;
        match out {
            None => event_then(&mut state, EventInput::new("update_goal", false, "no goal system (no relay)"), session.as_deref(),
                err_reply("Goal tracking lives in codex (relay disabled, or local-subagent session). Do not retry; there is no goal to update here.", json!({}))),
            Some(out) => {
                let ok = !out.starts_with("relay to codex failed");
                let summary = if ok { format!("goal {status}") } else { brief(&out) };
                event_then(&mut state, EventInput::new("update_goal", ok, summary), session.as_deref(), text_reply(ok, json!({ "ok": ok, "output": out }), out, None))
            }
        }
    });
    register(
        b, "update_goal", "Update goal",
        "Update the existing goal — only to mark it achieved or genuinely blocked. Set status to 'complete' only when the objective is achieved and no required work remains. Set 'blocked' only after the same blocking condition has recurred for at least three consecutive goal turns and no progress is possible without user input or an external-state change; not merely because the work is hard, slow, or incomplete. Cannot pause, resume, or change budgets — those are user/system controlled.",
        json!({
            "type": "object",
            "properties": { "status": {"type":"string","enum":["complete","blocked"]} },
            "required": ["status"]
        }),
        obj_schema(json!({ "ok": {"type":"boolean"}, "output": {"type":"string"}, "error": {"type":"string"} })),
        json!({ "readOnlyHint": false, "openWorldHint": false, "destructiveHint": false }),
        app_callable_meta(&["model", "app"]),
        h,
    );
}

// --- codex-native multi-agent (spawn_agent / wait_agent / …) ---------------
// NOTE: these six relay-only tools (spawn_agent, wait_agent, list_agents,
// send_message, followup_task, interrupt_agent) are NOT in the 28-name
// completeness set, but register.ts registers them, so they are ported for
// fidelity. They relay to codex via relayFunction; off-codex sessions get a
// no-retry error.

fn register_relay_agent_tools(b: &mut RepoMcpServerBuilder, ctx: &ToolCtx) {
    let specs: &[(&str, &str, &str, Value)] = &[
        ("spawn_agent", "Spawn agent",
         "Spawn a sub-agent to work on a concrete, bounded subtask that can run independently. task_name = lowercase letters/digits/underscores; message = the initial task for it. The sub-agent has the same tools and can spawn its own; its final answer comes back via wait_agent.",
         json!({ "type":"object","properties":{ "task_name":{"type":"string","minLength":1}, "message":{"type":"string","minLength":1}, "fork_turns":{"type":"string"} }, "required":["task_name","message"] })),
        ("wait_agent", "Wait for agents",
         "Wait for a mailbox update from any live sub-agent (queued messages or final-status notifications). Returns which agents have updates, or a timeout summary if none arrive before the deadline.",
         json!({ "type":"object","properties":{ "timeout_ms":{"type":"integer"} } })),
        ("list_agents", "List agents",
         "List live agents in the current root thread tree. Optionally filter by task-path prefix.",
         json!({ "type":"object","properties":{ "path_prefix":{"type":"string"} } })),
        ("send_message", "Message agent",
         "Queue a plain-text message to another live agent by its relative or canonical task name (from spawn_agent).",
         json!({ "type":"object","properties":{ "target":{"type":"string","minLength":1}, "message":{"type":"string","minLength":1} }, "required":["target","message"] })),
        ("followup_task", "Follow-up task",
         "Send a follow-up task to an existing agent by id or canonical task name (from spawn_agent).",
         json!({ "type":"object","properties":{ "target":{"type":"string","minLength":1}, "message":{"type":"string","minLength":1} }, "required":["target","message"] })),
        ("interrupt_agent", "Interrupt agent",
         "Interrupt a running sub-agent by its task name.",
         json!({ "type":"object","properties":{ "target":{"type":"string","minLength":1} }, "required":["target"] })),
    ];
    for (name, title, description, input_schema) in specs.iter() {
        let name_owned = name.to_string();
        let ctx_cloned: ToolCtx = ToolCtx::clone(ctx);
        let h: ToolHandler = Arc::new(move |args: Value, session: Option<String>| {
            let ctx = ctx_cloned.clone();
            let name_owned = name_owned.clone();
            Box::pin(async move {
                let reply: ToolReply = async move {
                    let agent_id = ctx.state.lock().await.agent_for_session(session.as_deref());
                    let out = relay_function(&ctx.relay_url, &name_owned, args.clone(), &agent_id).await;
                    let mut state = ctx.state.lock().await;
                    match out {
                        None => event_then(
                            &mut state,
                            EventInput::new(name_owned.clone(), false, "no codex relay"),
                            session.as_deref(),
                            err_reply("Multi-agent tools live in codex (relay disabled / local-subagent session). Do not retry.", json!({})),
                        ),
                        Some(out) => {
                            let ok = !out.starts_with("relay to codex failed");
                            let args_brief = brief(&format!("{name_owned}: {}", serde_json::to_string(&args).unwrap_or_default()));
                            let summary = if ok { args_brief } else { brief(&out) };
                            event_then(&mut state, EventInput::new(name_owned.clone(), ok, summary), session.as_deref(), text_reply(ok, json!({ "ok": ok, "output": out }), out, None))
                        }
                    }
                }
                .await;
                Ok::<ToolReply, ::anyhow::Error>(reply)
            }) as ::std::pin::Pin<Box<dyn ::std::future::Future<Output = ::anyhow::Result<ToolReply>> + Send>>
        });
        register(
            b,
            name,
            title,
            description,
            input_schema.clone(),
            obj_schema(json!({ "ok": {"type":"boolean"}, "output": {"type":"string"}, "error": {"type":"string"} })),
            json!({ "readOnlyHint": false, "openWorldHint": true, "destructiveHint": false }),
            app_callable_meta(&["model", "app"]),
            h,
        );
    }
}

// --- legacy off-codex subagent rail (4 tools) ------------------------------

fn register_subagent_tools(b: &mut RepoMcpServerBuilder, ctx: &ToolCtx) {
    for def in subagent_tool_defs() {
        let name = def.name;
        let h = handler!(ctx, args, session, {
            let mut state = ctx.state.lock().await;
            // SubagentTools::dispatch returns a subagent::ToolReply; re-shape it
            // into the mcp::ToolReply this layer hands back.
            match SubagentTools::dispatch(&mut state, name, &args, session.as_deref()) {
                Some(sr) => ToolReply {
                    content: sr.content.into_iter().map(|c| json!({ "type": c.kind, "text": c.text })).collect(),
                    structured_content: sr.structured_content,
                    meta: sr.meta,
                },
                None => err_reply(format!("Tool {name} not found"), json!({})),
            }
        });
        register(
            b,
            def.name,
            def.title,
            def.description,
            def.input_schema.clone(),
            def.output_schema.clone(),
            def.annotations.clone(),
            def.meta.clone(),
            h,
        );
    }
}

// ===========================================================================
// handleSlash — port of register.ts's slash dispatcher (subset of commands).
// ===========================================================================

/// `handleSlash(config, state, text)` — returns a `SlashResult` JSON object with
/// at least a `text` field (plus optional `suggestedRenderMode` / `meta`).
async fn handle_slash(
    _relay_url: &Option<String>,
    config: &mut Config,
    state: &mut RepoState,
    text: &str,
    session: Option<&str>,
) -> Result<Value, String> {
    let trimmed = text.trim();
    if !trimmed.starts_with('/') {
        return Err("Slash commands must start with /.".to_string());
    }
    let rest = &trimmed[1..];
    let mut parts = rest.split_whitespace();
    let command = parts.next().unwrap_or("").to_lowercase();
    let args = parts.collect::<Vec<_>>().join(" ");
    let args = args.trim().to_string();

    match command.as_str() {
        "help" => Ok(json!({ "text": slash_help(), "commands": TOOL_NAMES, "suggestedRenderMode": "command" })),
        "mcp" => {
            let body = TOOL_NAMES.iter().map(|t| format!("- {t}")).collect::<Vec<_>>().join("\n");
            Ok(json!({ "text": body, "tools": TOOL_NAMES }))
        }
        "status" => {
            let status = tools::run_shell(config, "git status --short --branch",
                tools::RunShellOpts { max_output_bytes: Some(40_000), timeout_ms: Some(20_000), ..Default::default() }).await;
            let combined = status.map(|s| s.combined).unwrap_or_else(|e| e);
            let clean = strip_hidden_dir_status(config, &combined);
            let perms = tools::effective_permissions(config);
            Ok(json!({
                "text": format!("Project: {}\nPermissions: {}/{}\n\n{clean}", config.root.to_string_lossy(), config.sandbox_mode.as_str(), config.approval_policy.as_str()),
                "status": clean,
                "permissions": serde_json::to_value(&perms).unwrap_or(Value::Null),
            }))
        }
        "permissions" => {
            let toks: Vec<&str> = args.split_whitespace().collect();
            let profile = toks.iter().find(|p| config.permission_profiles.contains_key(**p)).map(|s| s.to_string());
            let sandbox = toks.iter().find_map(|p| tools::SandboxMode::parse(p));
            let approval = toks.iter().find_map(|p| tools::ApprovalPolicy::parse(p));
            if profile.is_some() || sandbox.is_some() || approval.is_some() {
                tools::set_permissions(config, tools::SetPermsOpts { profile, sandbox_mode: sandbox, approval_policy: approval, reviewer: None }).map_err(|e| e.0)?;
                sync_config_into_state(config, state);
            }
            let perms = tools::effective_permissions(config);
            Ok(json!({
                "text": format!("Permissions: {}/{}\n{}", perms.sandbox_mode, perms.approval_policy, perms.note),
                "permissions": serde_json::to_value(&perms).unwrap_or(Value::Null),
                "approvals": serde_json::to_value(state.approvals()).unwrap_or(json!([])),
                "suggestedRenderMode": "permissions",
            }))
        }
        "project" | "projects" => {
            let mut tail = args.split_whitespace();
            let verb = tail.next().unwrap_or("");
            let rest_args = tail.collect::<Vec<_>>().join(" ");
            if verb == "switch" {
                let project = tools::switch_project(config, &rest_args).map_err(|e| e)?;
                let _ = state.switch_root(&config.root.to_string_lossy());
                sync_config_into_state(config, state);
                return Ok(json!({ "text": format!("Switched to {}\n{}", project.name, config.root.to_string_lossy()), "project": serde_json::to_value(&project).unwrap_or(Value::Null), "suggestedRenderMode": "projects" }));
            }
            let q = if verb == "search" { rest_args } else { args.clone() };
            let projects = tools::discover_projects(config, tools::DiscoverOpts { query: if q.is_empty() { None } else { Some(q) }, max: Some(24) });
            let body = projects.iter().map(|p| format!("{} {} — {}", if p.selected { "*" } else { "-" }, p.name, p.root)).collect::<Vec<_>>().join("\n");
            Ok(json!({ "text": if body.is_empty() { "No projects found.".to_string() } else { body }, "projects": serde_json::to_value(&projects).unwrap_or(json!([])), "suggestedRenderMode": "projects" }))
        }
        "diff" => {
            let diff = tools::safe_git_diff(config, None).await;
            Ok(json!({ "text": if diff.is_empty() { "No diff.".to_string() } else { diff.clone() }, "diff": diff, "suggestedRenderMode": "diff", "meta": { "currentDiff": diff } }))
        }
        "logs" => {
            let events: Vec<_> = state.recent_events(40).into_iter().filter(|e| e.blobs.as_ref().map(|b| !b.is_empty()).unwrap_or(false) || !e.ok || e.exit_code.is_some()).collect();
            let body = events.iter().map(|e| {
                let blobs = e.blobs.as_ref().map(|b| if b.is_empty() { String::new() } else { format!(" blobs={}", b.join(",")) }).unwrap_or_default();
                format!("{} {} {}: {}{}", e.ts, if e.ok { "✓" } else { "✗" }, e.tool, e.summary, blobs)
            }).collect::<Vec<_>>().join("\n");
            Ok(json!({ "text": if body.is_empty() { "No logs yet.".to_string() } else { body }, "events": serde_json::to_value(&events).unwrap_or(json!([])), "suggestedRenderMode": "logs" }))
        }
        "compact" => {
            let capsule = state.create_capsule(if args.is_empty() { "slash" } else { &args });
            Ok(json!({ "text": capsule.markdown, "capsule": serde_json::to_value(&capsule).unwrap_or(Value::Null), "suggestedRenderMode": "capsules" }))
        }
        "init" => {
            let content = format!(
                "# AGENTS.md\n\n## Project\n- Root: {}\n- Agent workflow: chat-first; use the repo_* tools for exact reads/edits and open the repo surface only for diff/log/permission/project review.\n\n## Commands\n- Test: fill in\n- Lint: fill in\n- Build: fill in\n\n## Conventions\n- Keep changes scoped to the user's request.\n- Edit with repo_edit; reserve repo_write for new files or full rewrites.\n- Do not read secrets or credentials.\n",
                config.root.to_string_lossy()
            );
            let r = tools::write_repo_file(config, "AGENTS.md", &content, tools::WriteOpts { create_dirs: true, expected_sha256: None }).map_err(|e| e)?;
            Ok(json!({ "text": format!("Created {}. Review and edit it for your repo.", r.path), "file": serde_json::to_value(&r).unwrap_or(Value::Null), "suggestedRenderMode": "diff" }))
        }
        "new" => {
            state.remember("note", "User requested /new; next answer should start a fresh task frame while keeping repo state available through repo_resume.", vec![], session);
            Ok(json!({ "text": "Started a fresh task frame for this repo session. Use repo_resume if the next turn needs compressed prior state." }))
        }
        "review" => {
            let diff = tools::safe_git_diff(config, Some(220_000)).await;
            Ok(json!({
                "text": if diff.is_empty() { "No working-tree diff to review.".to_string() } else { format!("Review target diff is ready. Ask the model to review behavior changes and tests.\n\n{diff}") },
                "diff": diff, "suggestedRenderMode": "diff", "meta": { "currentDiff": diff }
            }))
        }
        other => Err(format!("Unknown slash command /{other}. Try /help.")),
    }
}

fn slash_help() -> String {
    [
        "Repo Agent slash commands:",
        "- /permissions [read-only|workspace-write|danger-full-access] [untrusted|on-request|never]",
        "- /project [search <query>|switch <name-or-path>]",
        "- /status",
        "- /diff",
        "- /logs",
        "- /compact [reason]",
        "- /init",
        "- /mcp",
        "- /new",
        "- /review",
    ]
    .join("\n")
}

#[allow(dead_code)]
/// `TOOL_NAMES` from register.ts (the slash `/mcp` + `/help` command list).
const TOOL_NAMES: &[&str] = &[
    "repo_ui", "repo_status", "repo_register", "repo_glob", "repo_read", "repo_grep",
    "repo_write", "repo_edit", "repo_diff", "repo_run", "repo_shell", "shell_command",
    "apply_patch", "update_plan", "request_user_input", "get_goal", "create_goal",
    "update_goal", "spawn_agent", "wait_agent", "list_agents", "send_message",
    "followup_task", "interrupt_agent", "repo_bg_output", "repo_bg_stop", "repo_bg_list",
    "repo_permissions", "repo_project", "repo_slash_command", "repo_remember",
    "repo_compact", "repo_resume", "repo_logs", "repo_spawn_subagent", "repo_await",
    "repo_subagent_list", "repo_subagent_kill",
];

// ===========================================================================
// Tests — completeness guard for the 28-tool contract.
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mcp::create_repo_mcp_server;
    use crate::state::{AutoCompactConfig, RepoAgentConfig, RepoState};

    /// The 28 named MCP tools register.ts's `registerRepoTools` MUST expose. This
    /// list is the contract ChatGPT depends on; the test below fails if any name
    /// is unregistered. (register.ts also registers the codex-native goal /
    /// request_user_input / multi-agent relay tools, which are checked separately.)
    const REQUIRED_28: [&str; 28] = [
        "apply_patch",
        "repo_await",
        "repo_bg_list",
        "repo_bg_output",
        "repo_bg_stop",
        "repo_compact",
        "repo_diff",
        "repo_edit",
        "repo_glob",
        "repo_grep",
        "repo_logs",
        "repo_permissions",
        "repo_project",
        "repo_read",
        "repo_register",
        "repo_remember",
        "repo_resume",
        "repo_run",
        "repo_shell",
        "repo_slash_command",
        "repo_spawn_subagent",
        "repo_status",
        "repo_subagent_kill",
        "repo_subagent_list",
        "repo_ui",
        "repo_write",
        "shell_command",
        "update_plan",
    ];

    fn test_ctx() -> ToolCtx {
        // A throwaway repo root under the OS temp dir; RepoState writes its state
        // dir there. A unique suffix keeps parallel test runs isolated.
        let mut root = std::env::temp_dir();
        let uniq = format!(
            "cyrus-chimera-register-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        );
        root.push(uniq);
        std::fs::create_dir_all(&root).expect("create temp root");
        let root_str = root.to_string_lossy().to_string();

        let state_config = RepoAgentConfig {
            root: root_str.clone(),
            home_root: root_str.clone(),
            current_project: None,
            sandbox_mode: "workspace-write".to_string(),
            approval_policy: "on-request".to_string(),
            approvals_reviewer: "user".to_string(),
            writable_roots: vec![],
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
        };
        let state = RepoState::new(state_config).expect("RepoState::new");
        let config = Config::with_root(root);
        ToolCtx {
            state: Arc::new(Mutex::new(state)),
            config: Arc::new(Mutex::new(config)),
            relay_url: None,
            // Gate ON for the completeness guard: every contract name must be
            // registerable. Default-off parity is covered separately below.
            legacy_subagents: true,
        }
    }

    fn build_test_server() -> crate::mcp::RepoMcpServer {
        let ctx = test_ctx();
        create_repo_mcp_server(|b| register_repo_tools(b, &ctx))
    }

    /// The four dead-end legacy subagent tools register.ts:1200 hides unless
    /// CHIMERA_LEGACY_SUBAGENTS=1. Off by default — the live server lists 34
    /// tools, and ChatGPT's connector sees exactly that set.
    const LEGACY_4: [&str; 4] = [
        "repo_await",
        "repo_spawn_subagent",
        "repo_subagent_kill",
        "repo_subagent_list",
    ];

    /// Default-gate parity: with `legacy_subagents: false` the four legacy names
    /// must be ABSENT and every other contract tool still present (34 total),
    /// matching the live Node server's tools/list.
    #[test]
    fn legacy_subagent_tools_hidden_by_default() {
        let mut ctx = test_ctx();
        ctx.legacy_subagents = false;
        let server = create_repo_mcp_server(|b| register_repo_tools(b, &ctx));
        let names = server.tool_names();
        let leaked: Vec<&str> = LEGACY_4
            .iter()
            .copied()
            .filter(|n| names.contains(n))
            .collect();
        assert!(
            leaked.is_empty(),
            "legacy subagent tools leaked past the default-off gate: {leaked:?}"
        );
        let missing: Vec<&str> = REQUIRED_28
            .iter()
            .copied()
            .filter(|want| !LEGACY_4.contains(want) && !names.contains(want))
            .collect();
        assert!(
            missing.is_empty(),
            "non-legacy required tools missing with gate off: {missing:?}"
        );
        assert_eq!(
            names.len(),
            34,
            "default tools/list must match the live Node server (34): {names:?}"
        );
    }

    /// THE COMPLETENESS GUARD: every one of the 28 contract tool names must be
    /// present in the registered tool list. Fails (listing the missing names) if
    /// any tool is unregistered.
    #[test]
    fn all_28_required_tools_are_registered() {
        let server = build_test_server();
        let names = server.tool_names();
        let missing: Vec<&str> = REQUIRED_28
            .iter()
            .copied()
            .filter(|want| !names.contains(want))
            .collect();
        assert!(
            missing.is_empty(),
            "missing required MCP tools: {missing:?}\nregistered: {names:?}"
        );
    }

    /// No duplicate registrations (a duplicate name would shadow on tools/call and
    /// silently break the contract).
    #[test]
    fn no_duplicate_tool_names() {
        let server = build_test_server();
        let names = server.tool_names();
        let mut seen = std::collections::HashSet::new();
        for n in &names {
            assert!(seen.insert(*n), "duplicate tool registered: {n}");
        }
    }

    /// register.ts also exposes the codex-native goal / request_user_input /
    /// multi-agent relay tools; assert those are present too (fidelity check).
    #[test]
    fn codex_native_relay_tools_are_registered() {
        let server = build_test_server();
        let names = server.tool_names();
        for want in [
            "request_user_input",
            "get_goal",
            "create_goal",
            "update_goal",
            "spawn_agent",
            "wait_agent",
            "list_agents",
            "send_message",
            "followup_task",
            "interrupt_agent",
        ] {
            assert!(names.contains(&want), "missing codex-native tool: {want}");
        }
    }

    /// The repo_ui tool carries the workbench render `_meta` (outputTemplate +
    /// resourceUri mirror), distinguishing it from the data-only tools.
    #[test]
    fn repo_ui_has_render_meta() {
        let server = build_test_server();
        let list = server.tools_list_result_for_test();
        let ui = list["tools"]
            .as_array()
            .unwrap()
            .iter()
            .find(|t| t["name"] == "repo_ui")
            .expect("repo_ui present");
        assert_eq!(ui["_meta"]["openai/outputTemplate"], json!(WORKBENCH_URI));
        assert_eq!(ui["_meta"]["ui/resourceUri"], json!(WORKBENCH_URI));
    }
}
