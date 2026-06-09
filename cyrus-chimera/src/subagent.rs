//! The four MAIN-thread subagent MCP tools the ChatGPT MAIN thread calls to
//! delegate scoped subtasks to parallel ChatGPT subagents.
//!
//! Source: repo-agent-mcp/src/tools/subagent.ts (private original)
//!
//! Scope note: the durable `SubagentRegistry` itself (createJob/collectResult/
//! findByTask/…) lives inside `store.ts` and is already ported into
//! `crate::state` (`SubagentRegistry` + `RepoState`). This module is the port of
//! `subagent.ts` proper — i.e. ONLY the tool layer:
//!   - `repo_spawn_subagent`: depth-1 only (only "main" may spawn); task-hash
//!     dedup; spawn-budget then concurrency caps; else mint a job.
//!   - `repo_await`: collect-once CAS on terminal status; omit `agent_id` =>
//!     first uncollected. A finished result is delivered EXACTLY once.
//!   - `repo_subagent_list`: every subagent + status/turns/collected.
//!   - `repo_subagent_kill`: synthesize a "crashed" capsule + release leases when
//!     the job is not already terminal; return the final status.
//!
//! Coupling note: these tools only RECORD intent + read durable result state. The
//! actual ChatGPT tabs are opened/torn-down by the lipsync harness, which polls
//! `/control/subagents` and POSTs results back via `/control/subagent/*`. So this
//! module is the SERVER half of a two-process protocol with
//! `cyrus-lipsync::subagent_mux` — the control-plane contract (the camelCase wire
//! shapes of `SubagentJob` / `HandbackCapsule`) is owned by `crate::state` and
//! must stay in lockstep with the harness.
//!
//! Registration shape: the TS registers each tool via the ext-apps
//! `registerAppTool`, which has no Rust MCP-SDK equivalent yet (`mcp.rs` is still
//! a stub). Rather than invent a binding, this module exposes:
//!   - `subagent_tool_defs()` — the four `ToolDef`s (title/description/schemas/
//!     annotations/`_meta`) for the registration layer to surface on `tools/list`,
//!   - `dispatch()` plus four `handle_*` methods — the handler bodies, ported
//!     line-for-line, that the registration layer calls with parsed args + the
//!     request session and that return a `ToolReply`.
//! When `mcp.rs` lands its `registerAppTool` analogue it wires these together; the
//! behavior captured here is already complete and order-faithful to the original.

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::state::{
    CreateJobInput, EventInput, HandbackCapsule, RepoState, SubagentStatus,
};

/// `SUBAGENT_TOOL_NAMES` — the four tool names exported from subagent.ts, in
/// declaration order.
pub const SUBAGENT_TOOL_NAMES: [&str; 4] = [
    "repo_spawn_subagent",
    "repo_await",
    "repo_subagent_list",
    "repo_subagent_kill",
];

// ---------------------------------------------------------------------------
// ToolReply — port of the subset of result.ts these tools use.
// ---------------------------------------------------------------------------

/// `ToolReply` from types.ts: `{ structuredContent, content, _meta? }`. The
/// structured object is always `{ ok, ...structured }`; `content` is a single
/// text part. Serializes to exactly the TS object shape.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolReply {
    #[serde(rename = "structuredContent")]
    pub structured_content: Value,
    pub content: Vec<TextContent>,
    #[serde(rename = "_meta", default, skip_serializing_if = "Option::is_none")]
    pub meta: Option<Value>,
}

/// One `{ type: "text", text }` content part.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TextContent {
    #[serde(rename = "type")]
    pub kind: String,
    pub text: String,
}

/// Build a `structuredContent` object from `(key, value)` pairs where a `None`
/// value means the key is JS-`undefined` and must be OMITTED entirely (matching
/// `JSON.stringify` dropping `undefined`-valued keys, while keeping explicit
/// `null`s). Used for `repo_await`, whose `progress`/`result` fields are
/// `undefined` when absent rather than `null`.
fn structured_obj(pairs: Vec<(&str, Option<Value>)>) -> Value {
    let mut map = serde_json::Map::new();
    for (k, v) in pairs {
        if let Some(value) = v {
            map.insert(k.to_string(), value);
        }
    }
    Value::Object(map)
}

/// `textReply(ok, structured, text, meta?)` from result.ts.
///
/// The TS builds `structuredContent: { ok, ...structured }`. `structured` here is
/// a JSON object (`Value::Object`); `ok` is spliced in FIRST so a later `ok` key
/// in `structured` would override it — but the callers never put `ok` in
/// `structured`, exactly as in the TS, so this matches the spread order.
pub fn text_reply(ok: bool, structured: Value, text: impl Into<String>, meta: Option<Value>) -> ToolReply {
    let mut obj = serde_json::Map::new();
    obj.insert("ok".to_string(), Value::Bool(ok));
    if let Value::Object(map) = structured {
        for (k, v) in map {
            obj.insert(k, v);
        }
    }
    ToolReply {
        structured_content: Value::Object(obj),
        content: vec![TextContent {
            kind: "text".to_string(),
            text: text.into(),
        }],
        meta,
    }
}

// ---------------------------------------------------------------------------
// Tool metadata (appCallableMeta + the four ToolDefs).
// ---------------------------------------------------------------------------

/// `Visibility = Array<"model" | "app">` from harness.ts.
pub type Visibility = Vec<&'static str>;

/// `appCallableMeta(visibility = ["model", "app"])`:
/// `{ ui: { visibility }, "openai/widgetAccessible": visibility.includes("app") }`.
fn app_callable_meta(visibility: &Visibility) -> Value {
    json!({
        "ui": { "visibility": visibility },
        "openai/widgetAccessible": visibility.iter().any(|v| *v == "app"),
    })
}

/// A registrable tool definition: the second argument the TS passes to
/// `registerAppTool`. Schemas are emitted as JSON Schema (what zod-to-json-schema
/// produces for `tools/list`); the registration layer attaches the handler.
#[derive(Debug, Clone, Serialize)]
pub struct ToolDef {
    pub name: &'static str,
    pub title: &'static str,
    pub description: &'static str,
    #[serde(rename = "inputSchema")]
    pub input_schema: Value,
    #[serde(rename = "outputSchema")]
    pub output_schema: Value,
    pub annotations: Value,
    #[serde(rename = "_meta")]
    pub meta: Value,
}

/// `registerSubagentTools` — the four tool definitions, in declaration order.
/// (The handlers live on `SubagentTools`; the registration layer pairs each name
/// with its `dispatch` arm.)
pub fn subagent_tool_defs() -> Vec<ToolDef> {
    let app_meta = app_callable_meta(&vec!["model", "app"]);

    vec![
        ToolDef {
            name: "repo_spawn_subagent",
            title: "Spawn subagent",
            description:
                "Delegate a scoped subtask to a parallel ChatGPT subagent. Returns an agent_id handle immediately (the subagent runs in its own tab); collect its result later with repo_await. Optionally pass scope_paths to declare the files it owns. Subagents cannot spawn their own subagents.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "task": { "type": "string", "minLength": 1 },
                    "label": { "type": "string" },
                    "scope_paths": { "type": "array", "items": { "type": "string" } },
                    "model": { "type": "string" },
                    "effort": { "type": "string" }
                },
                "required": ["task"],
                "additionalProperties": false
            }),
            output_schema: json!({
                "type": "object",
                "properties": {
                    "ok": { "type": "boolean" },
                    "agent_id": { "type": "string" },
                    "status": { "type": "string" },
                    "queued": { "type": "boolean" },
                    "error": { "type": "string" }
                },
                "required": ["ok"],
                "additionalProperties": true
            }),
            annotations: json!({ "readOnlyHint": false, "openWorldHint": true, "destructiveHint": false }),
            meta: app_meta.clone(),
        },
        ToolDef {
            name: "repo_await",
            title: "Await subagent",
            description:
                "Collect a subagent's result. Pass agent_id to collect a specific one; omit it to collect the first finished-and-uncollected subagent. Returns promptly: if the subagent is still running you get its current status/progress (re-await on a later turn). A finished result is delivered exactly once.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "agent_id": { "type": "string" },
                    "timeout_ms": { "type": "integer", "minimum": 0, "maximum": 10000 }
                },
                "additionalProperties": false
            }),
            output_schema: json!({
                "type": "object",
                "properties": {
                    "ok": { "type": "boolean" },
                    "agent_id": { "type": "string" },
                    "status": { "type": "string" },
                    "result": {},
                    "progress": {},
                    "error": { "type": "string" }
                },
                "required": ["ok"],
                "additionalProperties": true
            }),
            annotations: json!({ "readOnlyHint": false, "openWorldHint": false, "destructiveHint": false }),
            meta: app_meta.clone(),
        },
        ToolDef {
            name: "repo_subagent_list",
            title: "List subagents",
            description:
                "List all subagents this session with their status, turn count, and whether their result was already collected.",
            input_schema: json!({
                "type": "object",
                "properties": {},
                "additionalProperties": false
            }),
            output_schema: json!({
                "type": "object",
                "properties": {
                    "ok": { "type": "boolean" },
                    "subagents": { "type": "array", "items": {} }
                },
                "required": ["ok"],
                "additionalProperties": true
            }),
            annotations: json!({ "readOnlyHint": true }),
            meta: app_meta.clone(),
        },
        ToolDef {
            name: "repo_subagent_kill",
            title: "Kill subagent",
            description:
                "Terminate a subagent. Marks it crashed with a synthesized result if it has not already finished. The harness tears down its tab. Returns the final status.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "agent_id": { "type": "string", "minLength": 1 }
                },
                "required": ["agent_id"],
                "additionalProperties": false
            }),
            output_schema: json!({
                "type": "object",
                "properties": {
                    "ok": { "type": "boolean" },
                    "agent_id": { "type": "string" },
                    "status": { "type": "string" },
                    "error": { "type": "string" }
                },
                "required": ["ok"],
                "additionalProperties": true
            }),
            annotations: json!({ "readOnlyHint": false, "openWorldHint": false, "destructiveHint": true }),
            meta: app_meta,
        },
    ]
}

// ---------------------------------------------------------------------------
// Handler argument shapes (the parsed zod inputs).
// ---------------------------------------------------------------------------

/// `{ task, label?, scope_paths?, model?, effort? }`.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct SpawnArgs {
    pub task: String,
    #[serde(default)]
    pub label: Option<String>,
    #[serde(default)]
    pub scope_paths: Option<Vec<String>>,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub effort: Option<String>,
}

/// `{ agent_id?, timeout_ms? }`. `timeout_ms` is accepted and ignored, exactly as
/// in the TS handler (the await returns promptly regardless).
#[derive(Debug, Clone, Default, Deserialize)]
pub struct AwaitArgs {
    #[serde(default)]
    pub agent_id: Option<String>,
    #[serde(default)]
    pub timeout_ms: Option<i64>,
}

/// `{ agent_id }`.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct KillArgs {
    pub agent_id: String,
}

// ---------------------------------------------------------------------------
// Handlers — ported line-for-line from registerSubagentTools.
// ---------------------------------------------------------------------------

/// Stateless handler namespace for the four tools. Each method takes the shared
/// `RepoState` (held behind the crate's `tokio::sync::Mutex`), the parsed args,
/// and the request session (`x-openai-session`, captured synchronously by the
/// caller just like the TS reads it from AsyncLocalStorage).
pub struct SubagentTools;

impl SubagentTools {
    /// Dispatch by tool name. Unknown names return `None` so the registration
    /// layer can fall through to its own 404 handling.
    ///
    /// `args` is the raw parsed input object; each handler deserializes its slice
    /// of it. A malformed input (failing the zod schema upstream) never reaches
    /// here, so a deserialize error degrades to the field defaults.
    pub fn dispatch(
        state: &mut RepoState,
        name: &str,
        args: &Value,
        session: Option<&str>,
    ) -> Option<ToolReply> {
        match name {
            "repo_spawn_subagent" => {
                let a: SpawnArgs = serde_json::from_value(args.clone()).unwrap_or_default();
                Some(Self::spawn_subagent(state, a, session))
            }
            "repo_await" => {
                let a: AwaitArgs = serde_json::from_value(args.clone()).unwrap_or_default();
                Some(Self::await_subagent(state, a, session))
            }
            "repo_subagent_list" => Some(Self::subagent_list(state, session)),
            "repo_subagent_kill" => {
                let a: KillArgs = serde_json::from_value(args.clone()).unwrap_or_default();
                Some(Self::subagent_kill(state, a, session))
            }
            _ => None,
        }
    }

    /// `repo_spawn_subagent` handler.
    pub fn spawn_subagent(state: &mut RepoState, args: SpawnArgs, session: Option<&str>) -> ToolReply {
        let SpawnArgs {
            task,
            label,
            scope_paths,
            model,
            effort,
        } = args;

        let parent_agent_id = state.agent_for_session(session);

        // Depth = 1: only the main thread may spawn subagents.
        if parent_agent_id != "main" {
            let reply = text_reply(
                false,
                json!({ "error": "subagents cannot spawn subagents", "queued": false }),
                "Subagents cannot spawn subagents (depth limited to 1).",
                None,
            );
            state.event(
                EventInput {
                    agent: Some(parent_agent_id.clone()),
                    ..EventInput::new("repo_spawn_subagent", false, "depth>1 rejected")
                },
                session,
            );
            return reply;
        }

        // Task-hash dedup: an identical task already running/recently-done -> reuse it.
        if let Some(existing) = state.subagents.find_by_task(&task) {
            let reply = text_reply(
                true,
                json!({ "agent_id": existing.agent_id, "status": existing.status }),
                format!(
                    "Reusing existing subagent {} for an identical task.",
                    existing.agent_id
                ),
                None,
            );
            state.event(
                EventInput {
                    agent: Some(parent_agent_id.clone()),
                    ..EventInput::new(
                        "repo_spawn_subagent",
                        true,
                        format!("dedup -> {}", existing.agent_id),
                    )
                },
                session,
            );
            return reply;
        }

        // Spawn budget: total spawns this session must stay under maxSubagentSpawns.
        if (state.subagents.spawn_count() as f64) >= state.config.max_subagent_spawns {
            let reply = text_reply(
                false,
                json!({ "error": "spawn budget exhausted", "queued": true }),
                format!(
                    "Spawn budget exhausted ({}). Run remaining work sequentially.",
                    js_number_to_string(state.config.max_subagent_spawns)
                ),
                None,
            );
            state.event(
                EventInput {
                    agent: Some(parent_agent_id.clone()),
                    ..EventInput::new("repo_spawn_subagent", false, "budget exhausted")
                },
                session,
            );
            return reply;
        }

        // Concurrency cap: live (non-terminal) jobs must stay under maxSubagents.
        if (state.subagents.live_jobs().len() as f64) >= state.config.max_subagents {
            let reply = text_reply(
                false,
                json!({ "error": "max concurrent subagents reached", "queued": true }),
                format!(
                    "Max concurrent subagents reached ({}). Await one before spawning another.",
                    js_number_to_string(state.config.max_subagents)
                ),
                None,
            );
            state.event(
                EventInput {
                    agent: Some(parent_agent_id.clone()),
                    ..EventInput::new("repo_spawn_subagent", false, "max concurrent reached")
                },
                session,
            );
            return reply;
        }

        let job = state.subagents.create_job(CreateJobInput {
            task,
            label,
            scope_paths,
            parent_agent_id: parent_agent_id.clone(),
            model,
            effort,
        });
        let reply = text_reply(
            true,
            json!({ "agent_id": job.agent_id, "status": "pending" }),
            format!(
                "Spawned subagent {} ({}). It runs in its own tab; call repo_await({{ agent_id: \"{}\" }}) to collect its result.",
                job.agent_id, job.label, job.agent_id
            ),
            None,
        );
        state.event(
            EventInput {
                agent: Some(parent_agent_id),
                ..EventInput::new(
                    "repo_spawn_subagent",
                    true,
                    format!("spawn {}: {}", job.agent_id, job.label),
                )
            },
            session,
        );
        reply
    }

    /// `repo_await` handler.
    pub fn await_subagent(state: &mut RepoState, args: AwaitArgs, session: Option<&str>) -> ToolReply {
        let parent_agent_id = state.agent_for_session(session);

        if let Some(agent_id) = args.agent_id {
            let job = match state.subagents.get_job(&agent_id) {
                Some(j) => j,
                None => {
                    let reply = text_reply(
                        false,
                        json!({ "agent_id": agent_id, "error": "unknown agent_id" }),
                        format!("No subagent named {agent_id}."),
                        None,
                    );
                    state.event(
                        EventInput {
                            agent: Some(parent_agent_id.clone()),
                            ..EventInput::new("repo_await", false, format!("unknown {agent_id}"))
                        },
                        session,
                    );
                    return reply;
                }
            };
            let collected = state.subagents.collect_result(&agent_id);
            if collected.status == "pending"
                || collected.status == "spawning"
                || collected.status == "running"
            {
                // `progress: job.progress` — when `job.progress` is `undefined`
                // the key is dropped by JSON.stringify; only emit it when present.
                let reply = text_reply(
                    true,
                    structured_obj(vec![
                        ("agent_id", Some(json!(agent_id))),
                        ("status", Some(json!("running"))),
                        (
                            "progress",
                            job.progress.as_ref().map(|p| serde_json::to_value(p).unwrap_or(Value::Null)),
                        ),
                    ]),
                    format!("Subagent {agent_id} is {}. Re-await on a later turn.", collected.status),
                    None,
                );
                state.event(
                    EventInput {
                        agent: Some(parent_agent_id.clone()),
                        ..EventInput::new("repo_await", true, format!("{agent_id} running"))
                    },
                    session,
                );
                return reply;
            }
            // result: collected.collected ? collected.result : undefined
            // The ternary yields `undefined` when not collected OR when there is no
            // result capsule; either way JSON.stringify drops the `result` key.
            let result_value: Option<Value> = if collected.collected {
                collected
                    .result
                    .as_ref()
                    .map(|c| serde_json::to_value(c).unwrap_or(Value::Null))
            } else {
                None
            };
            let reply = text_reply(
                true,
                structured_obj(vec![
                    ("agent_id", Some(json!(agent_id))),
                    ("status", Some(json!(collected.status))),
                    ("result", result_value),
                ]),
                summarize_result(&agent_id, &collected.status, collected.result.as_ref(), collected.collected),
                None,
            );
            state.event(
                EventInput {
                    agent: Some(parent_agent_id.clone()),
                    ..EventInput::new("repo_await", true, format!("{agent_id} {}", collected.status))
                },
                session,
            );
            return reply;
        }

        // No agent_id: collect the first terminal-uncollected job, if any.
        let next = match state.subagents.first_uncollected() {
            Some(n) => n,
            None => {
                let reply = text_reply(
                    true,
                    json!({ "status": "none_ready" }),
                    "No finished subagents to collect.",
                    None,
                );
                state.event(
                    EventInput {
                        agent: Some(parent_agent_id.clone()),
                        ..EventInput::new("repo_await", true, "none_ready")
                    },
                    session,
                );
                return reply;
            }
        };
        let collected = state.subagents.collect_result(&next.agent_id);
        // result: collected.result (NOTE: this branch does NOT gate on collected.collected,
        // unlike the agent_id branch — preserved exactly from the TS). When the
        // result is `undefined` the key is dropped by JSON.stringify.
        let result_value: Option<Value> = collected
            .result
            .as_ref()
            .map(|c| serde_json::to_value(c).unwrap_or(Value::Null));
        let reply = text_reply(
            true,
            structured_obj(vec![
                ("agent_id", Some(json!(next.agent_id))),
                ("status", Some(json!(collected.status))),
                ("result", result_value),
            ]),
            summarize_result(&next.agent_id, &collected.status, collected.result.as_ref(), collected.collected),
            None,
        );
        state.event(
            EventInput {
                agent: Some(parent_agent_id),
                ..EventInput::new(
                    "repo_await",
                    true,
                    format!("collected {} {}", next.agent_id, collected.status),
                )
            },
            session,
        );
        reply
    }

    /// `repo_subagent_list` handler.
    pub fn subagent_list(state: &mut RepoState, session: Option<&str>) -> ToolReply {
        let subagents: Vec<Value> = state
            .subagents
            .list_jobs()
            .into_iter()
            .map(|j| {
                let turns = j.progress.as_ref().map(|p| p.turns).unwrap_or(0);
                json!({
                    "agent_id": j.agent_id,
                    "label": j.label,
                    "status": j.status,
                    "turns": turns,
                    "collected": j.collected,
                })
            })
            .collect();

        let text = if subagents.is_empty() {
            "No subagents.".to_string()
        } else {
            subagents
                .iter()
                .map(|s| {
                    let agent_id = s["agent_id"].as_str().unwrap_or("");
                    let status = s["status"].as_str().unwrap_or("");
                    let label = s["label"].as_str().unwrap_or("");
                    let turns = s["turns"].as_u64().unwrap_or(0);
                    let collected = s["collected"].as_bool().unwrap_or(false);
                    format!(
                        "{agent_id} [{status}] {label} · {turns} turns{}",
                        if collected { " · collected" } else { "" }
                    )
                })
                .collect::<Vec<_>>()
                .join("\n")
        };

        let count = subagents.len();
        let reply = text_reply(true, json!({ "subagents": subagents }), text, None);
        // The TS emits this event with NO `agent` field (defaults to the session's
        // agent), unlike the spawn/await events above.
        state.event(
            EventInput::new("repo_subagent_list", true, format!("{count} subagents")),
            session,
        );
        reply
    }

    /// `repo_subagent_kill` handler.
    pub fn subagent_kill(state: &mut RepoState, args: KillArgs, session: Option<&str>) -> ToolReply {
        let agent_id = args.agent_id;

        let job = match state.subagents.get_job(&agent_id) {
            Some(j) => j,
            None => {
                let reply = text_reply(
                    false,
                    json!({ "agent_id": agent_id, "error": "unknown agent_id" }),
                    format!("No subagent named {agent_id}."),
                    None,
                );
                state.event(
                    EventInput::new("repo_subagent_kill", false, format!("unknown {agent_id}")),
                    session,
                );
                return reply;
            }
        };

        let terminal = job.status == "done"
            || job.status == "blocked"
            || job.status == "timeout"
            || job.status == "crashed";
        if !terminal {
            let capsule = HandbackCapsule {
                agent_id: agent_id.clone(),
                status: "crashed".to_string(),
                summary: job
                    .progress
                    .as_ref()
                    .map(|p| p.last_summary.clone())
                    .unwrap_or_else(|| "Killed by parent before completion.".to_string()),
                files_touched: Vec::new(),
                bg_ids: Vec::new(),
                duration_ms: duration_since_created(&job.created_ts),
            };
            state.subagents.set_result(&agent_id, capsule);
            state.release_leases_for_agent(&agent_id);
        }

        let final_status: SubagentStatus = state
            .subagents
            .get_job(&agent_id)
            .map(|j| j.status)
            .unwrap_or_else(|| "crashed".to_string());
        let reply = text_reply(
            true,
            json!({ "agent_id": agent_id, "status": final_status }),
            format!("Subagent {agent_id} is now {final_status}."),
            None,
        );
        state.event(
            EventInput::new(
                "repo_subagent_kill",
                true,
                format!("kill {agent_id} -> {final_status}"),
            ),
            session,
        );
        reply
    }
}

// ---------------------------------------------------------------------------
// summarizeResult + small JS-parity helpers.
// ---------------------------------------------------------------------------

/// `summarizeResult(agentId, status, result, collected)` from subagent.ts.
pub fn summarize_result(
    agent_id: &str,
    status: &str,
    result: Option<&HandbackCapsule>,
    collected: bool,
) -> String {
    if !collected {
        return format!(
            "Subagent {agent_id} ({status}) was already collected; its result is not re-delivered."
        );
    }
    let result = match result {
        Some(r) => r,
        None => {
            return format!("Subagent {agent_id} finished ({status}) but produced no result capsule.");
        }
    };
    let files = if result.files_touched.is_empty() {
        String::new()
    } else {
        format!("\nFiles: {}", result.files_touched.join(", "))
    };
    format!("Subagent {agent_id} ({status}):\n{}{}", result.summary, files)
}

/// `Date.now() - new Date(createdTs).getTime()` — milliseconds elapsed since the
/// job's ISO-8601 `createdTs`. A malformed/unparsable timestamp yields 0 (the
/// closest faithful degrade: `new Date("garbage").getTime()` is NaN, and
/// `Date.now() - NaN` is NaN, which would serialize as null; we choose 0 so the
/// `durationMs: number` field stays a number — the harness only reads it for
/// display and never on this kill path).
fn duration_since_created(created_ts: &str) -> u64 {
    match chrono::DateTime::parse_from_rfc3339(created_ts) {
        Ok(created) => {
            let now = chrono::Utc::now();
            let delta = now.timestamp_millis() - created.timestamp_millis();
            delta.max(0) as u64
        }
        Err(_) => 0,
    }
}

/// Render a config numeric cap (`f64`) the way the TS template literal would: an
/// integral value prints with no decimal point (`12`), matching JS `${number}`
/// where `maxSubagentSpawns` is always an integer in practice.
fn js_number_to_string(n: f64) -> String {
    if n.is_finite() && n.fract() == 0.0 {
        format!("{}", n as i64)
    } else {
        // Non-integral or non-finite: fall back to the default Rust rendering.
        // (Not reachable for the integer caps, but kept faithful for safety.)
        format!("{n}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn summarize_result_not_collected() {
        let s = summarize_result("a3", "done", None, false);
        assert_eq!(
            s,
            "Subagent a3 (done) was already collected; its result is not re-delivered."
        );
    }

    #[test]
    fn summarize_result_no_capsule() {
        let s = summarize_result("a3", "done", None, true);
        assert_eq!(
            s,
            "Subagent a3 finished (done) but produced no result capsule."
        );
    }

    #[test]
    fn summarize_result_with_files() {
        let cap = HandbackCapsule {
            agent_id: "a3".into(),
            status: "done".into(),
            summary: "did the thing".into(),
            files_touched: vec!["a.rs".into(), "b.rs".into()],
            bg_ids: vec![],
            duration_ms: 10,
        };
        let s = summarize_result("a3", "done", Some(&cap), true);
        assert_eq!(s, "Subagent a3 (done):\ndid the thing\nFiles: a.rs, b.rs");
    }

    #[test]
    fn summarize_result_without_files() {
        let cap = HandbackCapsule {
            agent_id: "a3".into(),
            status: "blocked".into(),
            summary: "stuck".into(),
            files_touched: vec![],
            bg_ids: vec![],
            duration_ms: 10,
        };
        let s = summarize_result("a3", "blocked", Some(&cap), true);
        assert_eq!(s, "Subagent a3 (blocked):\nstuck");
    }

    #[test]
    fn text_reply_splices_ok_first() {
        let r = text_reply(true, json!({ "agent_id": "a1", "status": "pending" }), "hi", None);
        let sc = &r.structured_content;
        assert_eq!(sc["ok"], json!(true));
        assert_eq!(sc["agent_id"], json!("a1"));
        assert_eq!(sc["status"], json!("pending"));
        assert_eq!(r.content[0].kind, "text");
        assert_eq!(r.content[0].text, "hi");
        assert!(r.meta.is_none());
    }

    #[test]
    fn app_callable_meta_shape() {
        let m = app_callable_meta(&vec!["model", "app"]);
        assert_eq!(m["ui"]["visibility"], json!(["model", "app"]));
        assert_eq!(m["openai/widgetAccessible"], json!(true));

        let m2 = app_callable_meta(&vec!["model"]);
        assert_eq!(m2["openai/widgetAccessible"], json!(false));
    }

    #[test]
    fn tool_defs_cover_all_four_names() {
        let defs = subagent_tool_defs();
        let names: Vec<&str> = defs.iter().map(|d| d.name).collect();
        assert_eq!(names, SUBAGENT_TOOL_NAMES.to_vec());
    }

    #[test]
    fn js_number_to_string_integral() {
        assert_eq!(js_number_to_string(12.0), "12");
        assert_eq!(js_number_to_string(2.0), "2");
    }

    #[test]
    fn duration_zero_on_garbage() {
        assert_eq!(duration_since_created("not-a-date"), 0);
    }
}
