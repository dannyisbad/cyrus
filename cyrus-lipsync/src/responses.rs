//! The `/v1/responses` shim: HTTP app + codex-shaped Responses SSE emission +
//! the ReAct tool bridge (parse ChatGPT's ```run fences into codex function_call
//! items, flatten function_call_output back to text).
//!
//! The codex-facing SSE frame shapes MUST match codex-rs exactly. The
//! authoritative reference is:
//!   - codex-rs/codex-api/src/sse/responses.rs  (ResponsesStreamEvent + the
//!     process_responses_event kind dispatch; ResponseCompleted usage shape)
//!   - codex-rs/codex-api/src/common.rs          (ResponseEvent enum)
//!   - codex-rs/protocol/src/models.rs           (ResponseItem, #[serde(tag="type",
//!     rename_all="snake_case")]: message / function_call / custom_tool_call /
//!     reasoning / function_call_output / ...)
//!
//! Frames codex's eventsource parser expects (each a single-line `data: {json}`):
//!   {"type":"response.created","response":{...}}
//!   {"type":"response.output_item.added","item":{"type":"message","role":"assistant","content":[]}}
//!   {"type":"response.output_text.delta","delta":"tok"}
//!   {"type":"response.output_item.done","item":{...}}
//!   {"type":"response.completed","response":{"id":...,"usage":{...}}}
//!   {"type":"response.failed","response":{"error":{"code","message"}}}
//!   function_call items: {"type":"function_call","name","arguments":<JSON STRING>,"call_id"}
//!   freeform tools:      {"type":"custom_tool_call","call_id","name","input":<raw text>}
//!
//! Hazards:
//!   - An output_text.delta with NO active item makes codex's aggregator panic —
//!     ALWAYS emit output_item.added (content:[]) to OPEN the item before deltas,
//!     then output_item.done to close it.
//!   - `arguments` on a function_call MUST be a JSON *string* on the wire (codex
//!     parses it later), not a nested object.
//!   - json-serialize each frame on ONE line (escape newlines inside tokens) — the
//!     eventsource parser is line-oriented. serde_json never emits a raw newline.
//!   - response.completed.usage uses the exact 5-field ResponseCompletedUsage shape
//!     (input_tokens/input_tokens_details/output_tokens/output_tokens_details/
//!     total_tokens) or codex's deserializer rejects it.
//!   - extract_prompt: pull the LAST user message text; fall back to instructions.
//!   - parse_tool_call: ```run fence is primary; a lone shell fence counts only
//!     when it dominates the message (<=200 chars of surrounding prose).

use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use axum::extract::State;
use axum::http::HeaderMap;
use axum::http::StatusCode;
use axum::http::header;
use axum::response::IntoResponse;
use axum::response::Response;
use axum::routing::get;
use axum::routing::post;
use futures::StreamExt as _;
use regex::Regex;
use serde_json::Value;
use serde_json::json;
use std::sync::OnceLock;
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;

use crate::config::ShadowConfig;

// ===== Responses request parsing =============================================

/// Flatten a Responses message `content` into plain text.
///
/// `content` is either a bare string or a list of parts like
/// `{"type":"input_text"|"output_text"|"text","text":"..."}`. Mirrors
/// `_content_text`.
pub fn content_text(content: &Value) -> String {
    if let Some(s) = content.as_str() {
        return s.to_string();
    }
    let Some(parts) = content.as_array() else {
        return String::new();
    };
    let mut out = String::new();
    for part in parts {
        if let Some(s) = part.as_str() {
            out.push_str(s);
        } else if let Some(obj) = part.as_object() {
            // type is one of (None, "input_text", "output_text", "text") AND
            // "text" is a string.
            let t = obj.get("type").and_then(Value::as_str);
            let type_ok = matches!(t, None | Some("input_text") | Some("output_text") | Some("text"));
            if type_ok {
                if let Some(text) = obj.get("text").and_then(Value::as_str) {
                    out.push_str(text);
                }
            }
        }
    }
    out
}

/// Pull the new user turn out of a Responses request body (`extract_prompt`).
///
/// Codex is stateless: every request carries the full conversation in `input`.
/// The chatgpt.com tab is stateful, so we forward ONLY the last user message —
/// exactly the new turn. Falls back to `instructions` so the turn is never empty.
pub fn extract_prompt(body: &Value) -> String {
    let inp = body.get("input");
    if let Some(s) = inp.and_then(Value::as_str) {
        return s.trim().to_string();
    }
    let mut user_texts: Vec<String> = Vec::new();
    if let Some(items) = inp.and_then(Value::as_array) {
        for item in items {
            let Some(obj) = item.as_object() else {
                continue;
            };
            let role = obj.get("role").and_then(Value::as_str);
            let itype = obj.get("type").and_then(Value::as_str);
            let type_ok = matches!(itype, None | Some("message"));
            if role == Some("user") && type_ok && obj.contains_key("content") {
                let txt = content_text(obj.get("content").unwrap_or(&Value::Null));
                let txt = txt.trim();
                if !txt.is_empty() {
                    user_texts.push(txt.to_string());
                }
            }
        }
    }
    if let Some(last) = user_texts.last() {
        return last.clone();
    }
    // Fallback: nothing recognizable — use instructions so the turn isn't empty.
    body.get("instructions")
        .and_then(Value::as_str)
        .map(|s| s.trim().to_string())
        .unwrap_or_default()
}

/// Context replay for a fresh ChatGPT chat (fix wave 2, L5).
///
/// codex is fully stateless: EVERY request body carries the COMPLETE
/// conversation in `input`. The chatgpt.com tab is stateful, so normally only
/// the last user message is forwarded — but after a bridge restart (or
/// `codex resume` onto a rebooted shim) the fresh chat has seen NONE of the
/// history, and a last-message-only injection loses it ("what codeword?" ->
/// UNKNOWN). This builds a clearly-delimited, bounded transcript of the PRIOR
/// turns — user + assistant messages, tool runs collapsed to one marker line —
/// excluding the trailing user message (forwarded separately as the current
/// turn). Returns `None` when the request carries no prior conversation, so
/// genuine first turns are unchanged.
///
/// Bounds: per-entry cap 2000 chars, then the most recent 30 entries / 16k
/// chars total (older history drops first).
pub fn build_context_replay(body: &Value) -> Option<String> {
    const MAX_ENTRIES: usize = 30;
    const MAX_CHARS: usize = 16_000;
    const MAX_ENTRY_CHARS: usize = 2_000;
    const TOOL_MARKER: &str = "(assistant ran tools)";

    let items = body.get("input")?.as_array()?;
    let mut entries: Vec<String> = Vec::new();
    let mut pending_tools = false;
    for item in items {
        let Some(obj) = item.as_object() else {
            continue;
        };
        let itype = obj.get("type").and_then(Value::as_str);
        let role = obj.get("role").and_then(Value::as_str);
        let is_message = matches!(itype, None | Some("message")) && obj.contains_key("content");
        if is_message && matches!(role, Some("user") | Some("assistant")) {
            let txt = content_text(obj.get("content").unwrap_or(&Value::Null));
            let txt = txt.trim();
            if txt.is_empty() {
                continue;
            }
            // Context blobs codex re-stamps on every request: not conversation.
            if txt.starts_with("<environment_context>") || txt.starts_with("<user_instructions>")
            {
                continue;
            }
            if pending_tools {
                entries.push(TOOL_MARKER.to_string());
                pending_tools = false;
            }
            let mut t = txt.to_string();
            if t.chars().count() > MAX_ENTRY_CHARS {
                t = t.chars().take(MAX_ENTRY_CHARS).collect::<String>();
                t.push_str(" …[truncated]");
            }
            let label = if role == Some("user") { "USER" } else { "ASSISTANT" };
            entries.push(format!("{label}: {t}"));
        } else if matches!(
            itype,
            Some("function_call")
                | Some("custom_tool_call")
                | Some("function_call_output")
                | Some("custom_tool_call_output")
                | Some("local_shell_call")
                | Some("local_shell_call_output")
                | Some("web_search_call")
                | Some("reasoning")
        ) {
            // Tool noise: summarize a run of these as one marker line.
            pending_tools = true;
        }
    }

    // Drop the LAST user entry: that's the current turn, forwarded separately
    // by extract_prompt (which also takes the last user message).
    if let Some(pos) = entries.iter().rposition(|e| e.starts_with("USER: ")) {
        entries.remove(pos);
    }
    // A trailing tool marker with no message after it adds nothing.
    while entries.last().map(|e| e == TOOL_MARKER).unwrap_or(false) {
        entries.pop();
    }
    if entries.is_empty() {
        return None;
    }

    // Keep the most recent entries within both bounds.
    if entries.len() > MAX_ENTRIES {
        entries.drain(..entries.len() - MAX_ENTRIES);
    }
    let mut total: usize = entries.iter().map(|e| e.chars().count() + 1).sum();
    while entries.len() > 1 && total > MAX_CHARS {
        let dropped = entries.remove(0);
        total -= dropped.chars().count() + 1;
    }

    Some(format!(
        "[context restored from the previous session — the bridge restarted, so this is a \
fresh chat but the task is mid-flight]\nThe transcript below already happened. Treat it as \
ground truth, do NOT redo completed work, and continue from where it left off.\n\
=== PRIOR CONVERSATION ===\n{}\n=== END PRIOR CONVERSATION ===\n\nCurrent message:\n",
        entries.join("\n")
    ))
}

/// Flatten a FunctionCallOutputPayload (string | {content} | list) to text
/// (`_payload_text`).
pub fn payload_text(output: &Value) -> String {
    match output {
        Value::String(s) => s.clone(),
        Value::Object(map) => {
            match map.get("content") {
                Some(Value::String(s)) => s.clone(),
                Some(arr @ Value::Array(_)) => content_text(arr),
                _ => serde_json::to_string(output).unwrap_or_default(),
            }
        }
        Value::Array(_) => content_text(output),
        // Python `str(output)` for the bare-scalar fall-through (None/number/bool).
        Value::Null => "None".to_string(),
        Value::Bool(b) => {
            if *b {
                "True".to_string()
            } else {
                "False".to_string()
            }
        }
        Value::Number(n) => n.to_string(),
    }
}

/// If the newest input item is a function_call_output / custom_tool_call_output
/// (codex just executed a tool), return its flattened text (`last_tool_output`).
pub fn last_tool_output(body: &Value) -> Option<String> {
    let items = body.get("input")?.as_array()?;
    let last = items.last()?;
    let obj = last.as_object()?;
    let t = obj.get("type").and_then(Value::as_str);
    if matches!(t, Some("function_call_output") | Some("custom_tool_call_output")) {
        Some(payload_text(obj.get("output").unwrap_or(&Value::Null)))
    } else {
        None
    }
}

// ===== SSE emission ==========================================================

/// Format one SSE frame. `serde_json` escapes newlines inside token strings, so
/// each frame stays single-line (codex's eventsource parser needs that). Mirrors
/// `_sse`, returning the bytes rather than writing to a socket.
pub fn sse_frame(obj: &Value) -> String {
    // ensure_ascii=False in Python; serde_json keeps non-ASCII verbatim by default.
    format!("data: {}\n\n", serde_json::to_string(obj).unwrap_or_default())
}

/// A ResponseItem::Message — exactly the fixture shape codex deserializes.
///
/// `content=[]` is valid and is what `response.output_item.added` carries to OPEN
/// the item (codex's aggregator panics on an output_text.delta with no active
/// item — the item must be added first, then deltas, then done). Mirrors
/// `_message_item`.
pub fn message_item(text: &str, item_id: Option<&str>) -> Value {
    let content = if text.is_empty() {
        json!([])
    } else {
        json!([{"type": "output_text", "text": text}])
    };
    let mut item = json!({
        "type": "message",
        "role": "assistant",
        "content": content,
    });
    if let Some(id) = item_id {
        item["id"] = json!(id);
    }
    item
}

/// The `response.completed` frame body with the exact 5-field usage shape
/// codex's `ResponseCompletedUsage` deserializer requires (`_completed`).
pub fn completed(response_id: &str) -> Value {
    json!({
        "type": "response.completed",
        "response": {
            "id": response_id,
            "usage": {
                "input_tokens": 0,
                "input_tokens_details": null,
                "output_tokens": 0,
                "output_tokens_details": null,
                "total_tokens": 0,
            },
        },
    })
}

/// A ResponseItem::CustomToolCall — for freeform tools like apply_patch, whose
/// payload is raw text (the patch), not JSON (`_custom_tool_call_item`).
pub fn custom_tool_call_item(name: &str, input_text: &str, call_id: Option<&str>) -> Value {
    let call_id = call_id
        .map(str::to_string)
        .unwrap_or_else(|| format!("call_{}", uuid::Uuid::new_v4().simple()));
    json!({
        "type": "custom_tool_call",
        "call_id": call_id,
        "name": name,
        "input": input_text,
    })
}

/// Serialize a `Value` exactly the way CPython's `json.dumps(v, ensure_ascii=False)`
/// does with its DEFAULT separators: `", "` between items and `": "` between an
/// object key and its value (a space after every `,` and `:`). serde_json's
/// default `to_string` is compact (no spaces), so it does NOT byte-match the
/// Python original on the wire. We reuse serde_json's own string-escaping and
/// number formatting (which agree with Python for the values we emit) and only
/// widen the separators via a custom `Formatter`.
fn json_dumps_default(value: &Value) -> String {
    use serde::Serialize as _;
    use serde_json::ser::Formatter;

    /// A `Formatter` that mirrors CPython's `json.dumps` default separators
    /// `(", ", ": ")`. Empty containers stay `{}` / `[]` because the
    /// begin_*_value / begin_object_key hooks only fire for present elements.
    struct PyDefaultFormatter;
    impl Formatter for PyDefaultFormatter {
        fn begin_array_value<W>(&mut self, writer: &mut W, first: bool) -> std::io::Result<()>
        where
            W: ?Sized + std::io::Write,
        {
            if first {
                Ok(())
            } else {
                writer.write_all(b", ")
            }
        }

        fn begin_object_key<W>(&mut self, writer: &mut W, first: bool) -> std::io::Result<()>
        where
            W: ?Sized + std::io::Write,
        {
            if first {
                Ok(())
            } else {
                writer.write_all(b", ")
            }
        }

        fn begin_object_value<W>(&mut self, writer: &mut W) -> std::io::Result<()>
        where
            W: ?Sized + std::io::Write,
        {
            writer.write_all(b": ")
        }
    }

    let mut buf = Vec::new();
    let mut ser = serde_json::Serializer::with_formatter(&mut buf, PyDefaultFormatter);
    // Serializing a `Value` into a Vec<u8> writer is infallible for our inputs.
    value
        .serialize(&mut ser)
        .expect("serialize value with python-default formatter");
    String::from_utf8(buf).expect("serde_json emits valid utf-8")
}

/// Whether a JSON value is *falsy* the way CPython is (`bool(x)`), used to model
/// Python's `arguments or {}`: null, false, the number 0, the empty string,
/// the empty array, and the empty object are all falsy. (The empty-string case
/// is unreachable here because strings are handled before this is consulted, but
/// it is included for completeness.) `pub(crate)`: the `/control/toolcall`
/// router (`crate::runtime`) models the same `data.get("arguments") or {}`.
pub(crate) fn is_py_falsy(v: &Value) -> bool {
    match v {
        Value::Null => true,
        Value::Bool(b) => !b,
        Value::Number(n) => n.as_f64().map(|f| f == 0.0).unwrap_or(false),
        Value::String(s) => s.is_empty(),
        Value::Array(a) => a.is_empty(),
        Value::Object(o) => o.is_empty(),
    }
}

/// A ResponseItem::FunctionCall. `arguments` MUST be a JSON *string* on the wire
/// (codex parses it later), so a non-string value is `json.dumps`-ed here
/// (`_function_call_item`).
///
/// Byte-fidelity note: the Python original does
/// `json.dumps(arguments or {}, ensure_ascii=False)`, which uses json's DEFAULT
/// separators (a space after `:` and `,`), e.g. `{"command": "echo hi"}`. We must
/// reproduce that spacing exactly — see `json_dumps_default`.
pub fn function_call_item(name: &str, arguments: &Value, call_id: Option<&str>) -> Value {
    // arguments: if already a string, keep it verbatim (Python's `isinstance(str)`
    // branch never touches it — even `""`); otherwise `json.dumps(arguments or {})`.
    // Python's `or {}` replaces any *falsy* non-string with `{}`: null, false, 0,
    // 0.0, empty array, empty object. Replicate that so the wire string matches.
    let arguments_str: String = match arguments {
        Value::String(s) => s.clone(),
        other if is_py_falsy(other) => "{}".to_string(),
        other => json_dumps_default(other),
    };
    let call_id = call_id
        .map(str::to_string)
        .unwrap_or_else(|| format!("call_{}", uuid::Uuid::new_v4().simple()));
    json!({
        "type": "function_call",
        "name": name,
        "arguments": arguments_str,
        "call_id": call_id,
    })
}

// ===== ReAct tool bridge =====================================================
//
// The consumer chatgpt.com chat does not emit codex-shaped function calls, so we
// give it a text protocol: list the tools codex offered, and ask it to emit a
// single fenced ```run block when it wants to call one. We parse that block back
// into a codex `function_call` item.

/// The Hannah Montana protocol (`TOOL_PROTOCOL`). Load-bearing prose — copied
/// verbatim from the Python; do not paraphrase.
pub const TOOL_PROTOCOL: &str = concat!(
    "You are a coding agent connected to the user's REAL machine (Windows, ",
    "PowerShell). You can actually run commands on it — but only through ME, the ",
    "bridge. You do NOT have a working sandbox of your own; your built-in ",
    "python/code-interpreter runs on a different throwaway Linux box and its results ",
    "are WRONG here, so never use it and never guess or invent command output.\n\n",
    "HOW TO RUN A COMMAND: write the PowerShell command inside a fenced block tagged ",
    "`run`, and end your message right after the closing fence:\n",
    "```run\n",
    "<one PowerShell command here>\n",
    "```\n",
    "I will execute it on the real machine and reply with a `TOOL RESULT` message ",
    "containing the actual output. Then you continue — inspect the result and either ",
    "run another command (another ```run block) or, when the task is fully done, give ",
    "your final answer as a normal message with NO ```run block.\n\n",
    "Rules:\n",
    "- If a task needs real machine state (files, env vars, command output), your ",
    "first action MUST be a ```run block — do not answer from memory.\n",
    "- One command per ```run block. Put nothing after the closing fence.\n",
    "- Only use a ```run block for a command you want executed NOW. Use a normal ",
    "```powershell block (no `run` tag) if you are only showing example code.\n\n",
    "--- EXAMPLE ---\n",
    "User: How many items are in the current directory?\n",
    "Assistant:\n",
    "```run\n",
    "(Get-ChildItem | Measure-Object).Count\n",
    "```\n",
    "TOOL RESULT:\n```\n7\n```\n",
    "Assistant: There are 7 items in the current directory.\n",
    "--- END EXAMPLE ---\n\n",
    "TASK:\n",
);

/// Render the Hannah Montana command-bridge preamble. v1 surfaces the shell
/// executor only (the `run` fence). Mirrors `build_tool_preamble` — the `tools`
/// argument is currently unused, exactly as in the Python.
pub fn build_tool_preamble(_tools: &[Value]) -> String {
    TOOL_PROTOCOL.to_string()
}

// A command the model wants executed NOW: a ```run fence (primary), or a natural
// shell fence (powershell/pwsh/ps1/bash/sh/shell/cmd/console) as fallback.
// re.DOTALL | re.IGNORECASE -> (?is) inline flags; `.*?` is lazy.
fn run_fence_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        #[allow(clippy::expect_used)]
        Regex::new(r"(?is)```run\s*\n(.*?)```").expect("run fence regex")
    })
}

fn shell_fence_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        #[allow(clippy::expect_used)]
        Regex::new(r"(?is)```(?:powershell|pwsh|ps1|bash|sh|shell|cmd|console)\s*\n(.*?)```")
            .expect("shell fence regex")
    })
}

/// A parsed tool call: `{name:"shell_command", arguments:{command}}`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolCall {
    pub name: String,
    pub command: String,
}

impl ToolCall {
    /// The `arguments` Value handed to `function_call_item` (`{"command": cmd}`).
    pub fn arguments(&self) -> Value {
        json!({ "command": self.command })
    }
}

/// If ChatGPT's turn contains a command to run, return a codex function_call spec
/// `{name:'shell_command', arguments:{command}}`. Else None (final answer).
/// Mirrors `parse_tool_call`.
///
/// Primary signal is a ```run fence. As a fallback we accept a single natural
/// shell fence ONLY when it's effectively the whole message (<=200 chars of
/// surrounding prose) — so a final answer showing example code isn't misread.
pub fn parse_tool_call(text: &str) -> Option<ToolCall> {
    if text.is_empty() {
        return None;
    }
    if let Some(caps) = run_fence_re().captures(text) {
        let cmd = caps.get(1).map(|m| m.as_str().trim()).unwrap_or("");
        return if cmd.is_empty() {
            None
        } else {
            Some(ToolCall {
                name: "shell_command".to_string(),
                command: cmd.to_string(),
            })
        };
    }
    // Fallback: a lone shell fence that dominates the message.
    let fences: Vec<&str> = shell_fence_re()
        .captures_iter(text)
        .filter_map(|c| c.get(1).map(|m| m.as_str()))
        .collect();
    if fences.len() == 1 {
        let cmd = fences[0].trim();
        let stripped = shell_fence_re().replace_all(text, "");
        let stripped = stripped.trim();
        // "dominates" = little prose around it. <=200 chars of surrounding prose.
        if !cmd.is_empty() && stripped.chars().count() <= 200 {
            return Some(ToolCall {
                name: "shell_command".to_string(),
                command: cmd.to_string(),
            });
        }
    }
    None
}

// ===== HTTP server ===========================================================
//
// The aiohttp `build_app` exposed:
//   GET  /health
//   POST /v1/responses   (also /responses — lenient base_url with/without /v1)
//   POST /control/toolcall
//   POST /control/turn_complete
//
// The per-thread browser driving (ThreadConductor: open a chatgpt.com tab, tap
// its WS, collect a turn) lives in `crate::conductor`; the assembled runtime
// (shared TabFactory rail, per-thread routing, the chimera bind queue) lives in
// `crate::runtime`. To keep responses.rs the pure translation layer the brief
// describes — the SSE shapes + tool bridge — the turn itself is produced through
// a `TurnDriver` (the `runtime::ConductorMux` in the live server). The simple
// driver path returns the full buffered ChatGPT answer for one codex request;
// this module turns that into the exact Responses SSE frame sequence.

/// One codex request -> one ChatGPT turn. Implementors own the chatgpt.com tab
/// (the conductor). `run_turn` buffers the full visible answer for THIS request
/// (we must see the whole turn to know whether it is a tool call), and returns
/// it. Routing is by codex's `thread-id` header; `subagent_kind` carries the
/// optional `x-openai-subagent` header value.
#[async_trait::async_trait]
pub trait TurnDriver: Send + Sync {
    /// Drive one turn for the given request body and return the full visible
    /// ChatGPT answer text. `inject_text` is the already-built prompt to send;
    /// when None the caller already decided there is nothing to inject.
    async fn collect_turn(
        &self,
        thread_id: Option<&str>,
        subagent_kind: Option<&str>,
        body: &Value,
        inject_text: &str,
    ) -> anyhow::Result<String>;

    /// Build the text to inject for this request: forward a tool result if codex
    /// just executed a tool, else the last user message (with the tool-protocol
    /// preamble prepended once per thread when tools are offered). Returns the
    /// injection text (may be empty). Mirrors `ThreadConductor._build_injection`.
    fn build_injection(
        &self,
        thread_id: Option<&str>,
        body: &Value,
        tools: &[Value],
    ) -> String {
        let _ = thread_id;
        default_build_injection(body, tools, &mut false)
    }

    /// Resolve + boot this thread's resources BEFORE the SSE stream starts.
    /// Mirrors the Python `responses` handler's `get_conductor(...)` +
    /// `ensure_booted()` (which it runs ahead of `resp.prepare`, answering a 502
    /// JSON on boot failure instead of a stream). Default: nothing to prepare.
    async fn prepare_turn(
        &self,
        thread_id: Option<&str>,
        subagent_kind: Option<&str>,
    ) -> anyhow::Result<()> {
        let _ = (thread_id, subagent_kind);
        Ok(())
    }

    /// Drive one full turn, writing SSE frames to `tx` — the Python
    /// `tc.run_turn(resp, body)` call. The default reproduces the pure
    /// translation-layer flow (build the injection, then either the empty-turn
    /// frames or `collect_turn` + the buffered emission); the conductor router
    /// overrides it to hand the whole turn to the per-thread `ThreadConductor`
    /// (which owns the `SHIM_CONDUCTOR` dispatch and the per-thread
    /// `protocol_sent` preamble state — so this default's stateless
    /// `build_injection` path is NOT used on the conductor path).
    async fn drive_turn(
        &self,
        thread_id: Option<&str>,
        subagent_kind: Option<&str>,
        tx: &mpsc::Sender<String>,
        body: &Value,
    ) -> anyhow::Result<()> {
        let response_id = format!("resp_{}", uuid::Uuid::new_v4().simple());
        let tools: Vec<Value> = body
            .get("tools")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let tools_offered = !tools.is_empty();
        let inject_text = self.build_injection(thread_id, body, &tools);
        if inject_text.is_empty() {
            // Nothing to inject — emit the empty-turn frames without driving.
            emit_turn(tx, &response_id, tools_offered, "", "").await;
            return Ok(());
        }
        let full = self
            .collect_turn(thread_id, subagent_kind, body, &inject_text)
            .await?;
        emit_turn(tx, &response_id, tools_offered, &inject_text, &full).await;
        Ok(())
    }

    /// The `/health` `booted` aggregate (the Python `shim._booted` property: any
    /// thread booted, or an eager-main tab pre-booted). Default: not booted.
    async fn booted(&self) -> bool {
        false
    }

    /// Route a `/control/toolcall` body to the right per-thread conductor and
    /// hold the request until codex executed the tool. Returns `(status, JSON
    /// body)` shaped exactly like the Python handler. Default (no conductor
    /// router installed): the 409 the Python returns when no conductor matches.
    async fn route_control_toolcall(&self, data: &Value) -> (u16, Value) {
        let _ = data;
        (409, json!({"error": "no active conductor for thread"}))
    }

    /// Route a `/control/turn_complete` body to the right conductor. Returns
    /// `(status, JSON body)`; default mirrors the Python's no-active-turn 409.
    async fn route_control_turn_complete(&self, data: &Value) -> (u16, Value) {
        let _ = data;
        (409, json!({"ok": false, "error": "no active turn"}))
    }
}

/// The default `_build_injection` logic, factored out so a driver can reuse it.
/// `protocol_sent` is the per-thread "preamble already injected" flag; it is set
/// to true the first time the preamble is prepended.
pub fn default_build_injection(body: &Value, tools: &[Value], protocol_sent: &mut bool) -> String {
    if let Some(tool_out) = last_tool_output(body) {
        return format!(
            "TOOL RESULT (the backend executed your tool call and returned this \
real output):\n```\n{}\n```\nContinue: call another tool if needed, or give your \
final answer with no tool block if the task is complete.",
            tool_out.trim()
        );
    }
    let user = extract_prompt(body);
    if !tools.is_empty() && !*protocol_sent {
        *protocol_sent = true;
        return format!("{}{}", build_tool_preamble(tools), user);
    }
    user
}

/// Emit the Responses SSE frame sequence for one turn into `tx`, given the full
/// buffered ChatGPT answer `full`. This is the codex-facing contract — the exact
/// `run_turn` emission shape from the Python (non-conductor path).
///
/// * empty injection -> an empty message item (added/done) + completed.
/// * tool call -> a single function_call output item + completed (no message).
/// * final answer -> open message, one output_text.delta, close, completed.
async fn emit_turn(
    tx: &mpsc::Sender<String>,
    response_id: &str,
    tools_offered: bool,
    inject_text: &str,
    full: &str,
) {
    let send = |frame: Value| {
        let line = sse_frame(&frame);
        let tx = tx.clone();
        async move {
            let _ = tx.send(line).await;
        }
    };

    send(json!({"type": "response.created", "response": {}})).await;

    if inject_text.is_empty() {
        let item_id = format!("msg_{}", uuid::Uuid::new_v4().simple());
        send(json!({
            "type": "response.output_item.added",
            "item": message_item("", Some(&item_id)),
        }))
        .await;
        send(json!({
            "type": "response.output_item.done",
            "item": message_item("", Some(&item_id)),
        }))
        .await;
        send(completed(response_id)).await;
        return;
    }

    let call = if tools_offered { parse_tool_call(full) } else { None };
    if let Some(call) = call {
        // Tool call: a single function_call output item triggers codex to run it
        // (no message item / no text deltas -> no active-item needed).
        send(json!({
            "type": "response.output_item.done",
            "item": function_call_item(&call.name, &call.arguments(), None),
        }))
        .await;
        send(completed(response_id)).await;
    } else {
        // Final answer: open the message item, stream the buffered text, done.
        let item_id = format!("msg_{}", uuid::Uuid::new_v4().simple());
        send(json!({
            "type": "response.output_item.added",
            "item": message_item("", Some(&item_id)),
        }))
        .await;
        if !full.is_empty() {
            send(json!({"type": "response.output_text.delta", "delta": full})).await;
        }
        send(json!({
            "type": "response.output_item.done",
            "item": message_item(full, Some(&item_id)),
        }))
        .await;
        send(completed(response_id)).await;
    }
}

/// Router over the (per-thread) turn drivers + the shared config. Mirrors
/// `ShadowResponsesShim`'s HTTP surface; the per-thread `ThreadConductor` map and
/// the chimera `/control` binding rail live in `crate::conductor` /
/// `crate::subagent_mux` (which own the browser sockets).
#[derive(Clone)]
pub struct ShadowResponsesShim {
    pub cfg: ShadowConfig,
    pub model: String,
    driver: Arc<dyn TurnDriver>,
}

impl ShadowResponsesShim {
    pub fn new(cfg: ShadowConfig, model: impl Into<String>, driver: Arc<dyn TurnDriver>) -> Self {
        Self {
            cfg,
            model: model.into(),
            driver,
        }
    }
}

/// Build the axum app (`build_app`). Routes mirror the aiohttp router 1:1,
/// including the `/control/*` chimera-relay endpoints, which route into the
/// per-thread conductors through the installed [`TurnDriver`]
/// (`route_control_toolcall` / `route_control_turn_complete`).
pub fn build_app(shim: ShadowResponsesShim) -> Router {
    Router::new()
        .route("/health", get(health))
        .route("/v1/responses", post(responses))
        .route("/responses", post(responses)) // lenient: base_url with/without /v1
        // Unary codex endpoints the shim does NOT implement. They are
        // unreachable in the cyrus config (the shadow provider's name is not
        // "OpenAI", so codex's supports_remote_compaction() gate keeps it on
        // LOCAL compaction, and setup writes features.memories=false) — but if
        // a future codex ever calls them, answer a clean, well-formed 4xx
        // (NEVER 401, which trips codex's auth-refresh machinery; never a hang).
        .route("/v1/responses/compact", post(responses_compact_stub))
        .route("/responses/compact", post(responses_compact_stub))
        .route("/v1/memories/trace_summarize", post(memories_stub))
        .route("/memories/trace_summarize", post(memories_stub))
        // codex probes `GET /v1/models?client_version=...` at startup (and the
        // doctor route-probe does too). Without a route it 404s and codex logs a
        // (non-fatal) error every run. Answer a clean OpenAI-shaped models list
        // so the probe passes. GET and POST, with and without the /v1 prefix,
        // mirroring the lenient base_url handling above.
        .route("/v1/models", get(models_list).post(models_list))
        .route("/models", get(models_list).post(models_list))
        .route("/control/toolcall", post(control_toolcall))
        .route("/control/turn_complete", post(control_turn_complete))
        .with_state(Arc::new(shim))
}

/// `GET/POST /v1/models` (and `/models`) — a minimal models-list stub serving
/// BOTH shapes codex pokes at this endpoint with:
///
///   - the OpenAI route-probe (`object`/`data`) — the doctor only checks the
///     HTTP status, but we keep a well-formed list for any stricter consumer;
///   - codex's models-manager, which decodes the body as `ModelsResponse`
///     (`{"models":[...]}`). We return an EMPTY `models` list on purpose: the
///     shim is not a real model catalog, and an empty remote makes the manager
///     KEEP its bundled catalog (the curated cyrus lanes incl. GPT-5.5 Pro)
///     instead of replacing it. The previous OpenAI-only body had no `models`
///     key, so the manager's decode FAILED every refresh — logging an error and
///     making the picker's contents depend on fetch/cache timing (the flaky
///     "GPT-5.5 Pro disappears" symptom). An empty-but-valid `models` resolves
///     cleanly and deterministically to the bundled catalog.
///
/// Must be 200 and valid JSON, and MUST NOT be 401 (401 trips codex's
/// auth-refresh machinery — same constraint the other stubs honor).
async fn models_list(State(shim): State<Arc<ShadowResponsesShim>>) -> Response {
    let model = shim.model.clone();
    (
        StatusCode::OK,
        axum::Json(json!({
            "object": "list",
            "data": [
                {
                    "id": model,
                    "object": "model",
                    "owned_by": "cyrus",
                }
            ],
            // codex models-manager shape: empty -> keep the bundled catalog.
            "models": [],
        })),
    )
        .into_response()
}

/// `POST /v1/responses/compact` — remote compaction is intentionally not
/// implemented: under the shadow provider codex compacts LOCALLY (a normal
/// `/v1/responses` turn with request_kind "compaction", which the conductor
/// already serves headless). Log + clean 400 so any unexpected caller fails
/// fast and visibly.
async fn responses_compact_stub() -> Response {
    tracing::warn!(
        "[shim] POST /responses/compact — remote compaction not supported; codex should be \
using LOCAL compaction under the shadow provider (returning a clean 400)"
    );
    (
        StatusCode::BAD_REQUEST,
        axum::Json(json!({"error": {
            "code": "unsupported",
            "message": "remote compaction is not supported by the cyrus shim; \
codex uses local compaction with this provider",
        }})),
    )
        .into_response()
}

/// `POST /v1/memories/trace_summarize` — memories are disabled under shadow
/// (cyrus-setup writes `features.memories = false`). Same clean-4xx policy.
async fn memories_stub() -> Response {
    tracing::warn!(
        "[shim] POST /memories/trace_summarize — memories are disabled under shadow \
(returning a clean 400)"
    );
    (
        StatusCode::BAD_REQUEST,
        axum::Json(json!({"error": {
            "code": "unsupported",
            "message": "memory trace summarization is not supported by the cyrus shim",
        }})),
    )
        .into_response()
}

async fn health(State(shim): State<Arc<ShadowResponsesShim>>) -> Response {
    // Python: {"ok": True, "booted": shim._booted, "model": shim.model} — the
    // booted aggregate comes from the driver (any thread / eager tab booted).
    let booted = shim.driver.booted().await;
    axum::Json(json!({
        "ok": true,
        "booted": booted,
        "model": shim.model,
    }))
    .into_response()
}

/// `POST /v1/responses` (and `/responses`). Reads codex's `thread-id` /
/// `x-openai-subagent` headers, drives one ChatGPT turn through the conductor,
/// and streams back the codex-shaped Responses SSE frames.
async fn responses(
    State(shim): State<Arc<ShadowResponsesShim>>,
    headers: HeaderMap,
    raw_body: axum::body::Bytes,
) -> Response {
    let body: Value = match serde_json::from_slice(&raw_body) {
        Ok(v) => v,
        Err(_) => {
            return (
                StatusCode::BAD_REQUEST,
                axum::Json(json!({"error": "invalid JSON body"})),
            )
                .into_response();
        }
    };

    // Route by codex's thread-id header. The kind that actually distinguishes a
    // background request from the interactive session is NOT x-openai-subagent
    // (codex 0.137 never sets it) — it's the `request_kind` field inside the
    // `x-codex-turn-metadata` JSON header:
    //   "turn"   -> the real interactive session: gets the conductor + the
    //               one-time first-turn preamble (codex system prompt) + tools.
    //   "memory" -> background memory-consolidation. Shares the session's
    //               thread-id, so without a distinct routing key it lands in the
    //               interactive ChatGPT conversation, steals the preamble latch,
    //               and the user gets rollout-JSON answers. Must be isolated.
    // Any non-"turn" kind is treated as background. A legacy x-openai-subagent
    // header (older codex) still works as a fallback signal.
    let thread_id = headers
        .get("thread-id")
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    let request_kind = headers
        .get("x-codex-turn-metadata")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| serde_json::from_str::<Value>(s).ok())
        .and_then(|m| {
            m.get("request_kind")
                .and_then(Value::as_str)
                .map(str::to_string)
        });
    let legacy_subagent = headers
        .get("x-openai-subagent")
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    let subagent_kind = match request_kind.as_deref() {
        // Interactive turn (or pre-0.137 codex with no request_kind): main session.
        Some("turn") | None => legacy_subagent,
        // "memory", "compact", … — background work that must not touch the session.
        Some(other) => Some(other.to_string()),
    };

    // Header audit (SHIM_LOG_HEADERS=1): dump the routing-relevant headers + a
    // prompt snippet for every incoming request, so we can see EXACTLY what codex
    // sends for interactive vs background (memory/compaction) turns.
    if std::env::var("SHIM_LOG_HEADERS").as_deref() == Ok("1") {
        let prompt = crate::responses::extract_prompt(&body);
        let snippet: String = prompt.chars().take(80).collect();
        // Every header name=value (lowercased names), sorted-ish by insertion.
        // SECURITY: codex attaches the user's REAL ChatGPT `Authorization`
        // bearer + `ChatGPT-Account-ID` whenever a ChatGPT login exists on the
        // machine (the shim never uses them — /v1/responses ignores both).
        // Never log credential values: name + length only.
        let mut hdrs: Vec<String> = Vec::new();
        for (name, value) in headers.iter() {
            let n = name.as_str(); // HeaderName is always lowercase
            let v = value.to_str().unwrap_or("<bin>");
            let sensitive = matches!(
                n,
                "authorization"
                    | "chatgpt-account-id"
                    | "proxy-authorization"
                    | "cookie"
                    | "x-api-key"
            );
            if sensitive {
                hdrs.push(format!("{n}=<redacted len={}>", v.len()));
            } else {
                hdrs.push(format!("{n}={v}"));
            }
        }
        let instr = body
            .get("instructions")
            .and_then(Value::as_str)
            .map(|s| s.chars().take(80).collect::<String>())
            .unwrap_or_else(|| "-".to_string());
        let model = body.get("model").and_then(Value::as_str).unwrap_or("-");
        let store = body.get("store").map(|v| v.to_string()).unwrap_or_default();
        let r_effort = body
            .get("reasoning")
            .and_then(|r| r.get("effort"))
            .and_then(Value::as_str)
            .unwrap_or("-");
        tracing::info!(
            "[shim] REQ model={model} effort={r_effort} store={store} | HEADERS[{}] | instr[0..80]={:?} | prompt[0..80]={:?}",
            hdrs.join(" "),
            instr,
            snippet,
        );
    }

    // Python: tc = get_conductor(...); unless SHIM_NO_BROWSER, ensure_booted()
    // BEFORE the stream is prepared — a boot failure answers a 502 JSON, not SSE.
    // EXCEPTION: a typed TurnFailed (e.g. the tab is LOGGED OUT) is a condition
    // codex must treat by its error-code contract — a 502 would just feed its
    // HTTP retry loop. Emit a single response.failed SSE instead, carrying the
    // fatal/retryable code and the actionable message.
    if let Err(e) = shim
        .driver
        .prepare_turn(thread_id.as_deref(), subagent_kind.as_deref())
        .await
    {
        if let Some(tf) = e.downcast_ref::<crate::conductor::TurnFailed>() {
            let frame = json!({
                "type": "response.failed",
                "response": {"error": {"code": tf.code, "message": tf.message}},
            });
            return Response::builder()
                .status(StatusCode::OK)
                .header(header::CONTENT_TYPE, "text/event-stream")
                .header(header::CACHE_CONTROL, "no-cache")
                .body(Body::from(sse_frame(&frame)))
                .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response());
        }
        return (
            StatusCode::BAD_GATEWAY,
            axum::Json(json!({"error": format!("shadow boot failed: {e}")})),
        )
            .into_response();
    }

    // SSE channel -> streaming body. Each frame is one `data: {json}\n\n` chunk.
    let (tx, rx) = mpsc::channel::<String>(256);

    let driver = shim.driver.clone();
    tokio::spawn(async move {
        // Python: tc.run_turn(resp, body) under try/except -> response.failed.
        if let Err(e) = driver
            .drive_turn(thread_id.as_deref(), subagent_kind.as_deref(), &tx, &body)
            .await
        {
            // Best-effort: surface a Responses failure so codex stops cleanly.
            // A typed TurnFailed carries its own codex-facing code (e.g. FATAL
            // "invalid_prompt" for moderation/logout, retryable "shim_error"
            // for rate limits); anything else stays the retryable default.
            let (code, message) = match e.downcast_ref::<crate::conductor::TurnFailed>() {
                Some(tf) => (tf.code.clone(), tf.message.clone()),
                None => ("shim_error".to_string(), e.to_string()),
            };
            let frame = json!({
                "type": "response.failed",
                "response": {"error": {"code": code, "message": message}},
            });
            let _ = tx.send(sse_frame(&frame)).await;
        }
    });

    let stream = ReceiverStream::new(rx).map(Ok::<_, std::io::Error>);
    let body = Body::from_stream(stream);

    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "text/event-stream")
        .header(header::CACHE_CONTROL, "no-cache")
        .header(header::CONNECTION, "keep-alive")
        .body(body)
        .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response())
}

/// Build a `(status, json)` control response, mirroring aiohttp's
/// `web.json_response(body, status=...)`.
fn control_response(status: u16, body: Value) -> Response {
    let code = StatusCode::from_u16(status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
    (code, axum::Json(body)).into_response()
}

/// `POST /control/toolcall` — chimera (or a simulated chimera) long-polls here:
/// it posts the tool call ChatGPT made and BLOCKS until codex has executed it
/// and returned the output. This single held request IS "chimera holds ChatGPT's
/// MCP call open". The driver routes to the conductor for `data.thread_id`
/// (absent -> MAIN) and owns the dedupe/dispatch/timeout policy.
async fn control_toolcall(
    State(shim): State<Arc<ShadowResponsesShim>>,
    raw_body: axum::body::Bytes,
) -> Response {
    let data: Value = match serde_json::from_slice(&raw_body) {
        Ok(v) => v,
        Err(_) => return control_response(400, json!({"error": "invalid JSON"})),
    };
    // Python: `name = data.get("name"); if not name:` — absent, non-string, or
    // empty (falsy) -> 400 before any conductor lookup.
    let name_ok = data
        .get("name")
        .and_then(Value::as_str)
        .map(|s| !s.is_empty())
        .unwrap_or(false);
    if !name_ok {
        return control_response(400, json!({"error": "name required"}));
    }
    let (status, body) = shim.driver.route_control_toolcall(&data).await;
    control_response(status, body)
}

/// `POST /control/turn_complete` — ChatGPT finished its turn with final text;
/// end the codex turn. An unparseable body degrades to `{}` (Python `except:
/// data = {}`), which then 409s on the missing active turn.
async fn control_turn_complete(
    State(shim): State<Arc<ShadowResponsesShim>>,
    raw_body: axum::body::Bytes,
) -> Response {
    let data: Value = serde_json::from_slice(&raw_body).unwrap_or_else(|_| json!({}));
    let (status, body) = shim.driver.route_control_turn_complete(&data).await;
    control_response(status, body)
}

// ===== entry point ===========================================================

/// Runtime knobs parsed from the CLI and handed
/// to [`serve`]. Mirrors the `ShadowResponsesShim(cfg, model=..., effort=...)`
/// construction plus the bind host/port and the eager-vs-lazy boot branch.
#[derive(Debug, Clone, Default)]
pub struct ServeOptions {
    /// Bind host (default `127.0.0.1`).
    pub host: String,
    /// Bind port (default `8765`).
    pub port: u16,
    /// Model slug or friendly spec; `None` resolves to the default lane.
    pub model: Option<String>,
    /// Thinking effort: min/standard/extended/max. Passed into every
    /// per-thread conductor (the Python `ShadowResponsesShim(..., effort=...)`).
    pub effort: Option<String>,
    /// Defer tab boot until the first request (the eager-vs-lazy branch).
    pub lazy: bool,
}

/// Serve the `/v1/responses` (+ `/responses`, `/health`, `/control/*`) HTTP app.
/// The binary's `main` calls this. Mirrors `responses_shim.py::_amain`:
/// construct the runtime (shared TabFactory rail + bind queue) and the
/// per-thread conductor router, eager-boot one MAIN tab unless `--lazy` (a boot
/// failure logs and serving continues — it retries on the first request), then
/// bind and serve until killed.
///
/// Model/effort resolution mirrors `ShadowResponsesShim.__init__`:
/// `model or cfg.model_slug or "gpt-5-5-thinking"` and
/// `effort if effort is not None else cfg.thinking_effort`.
pub async fn serve(cfg: ShadowConfig, opts: ServeOptions) -> anyhow::Result<()> {
    // Python truthiness on the model fallback chain (None OR "" falls through).
    let model = opts
        .model
        .clone()
        .filter(|s| !s.is_empty())
        .or_else(|| cfg.model_slug.clone().filter(|s| !s.is_empty()))
        .unwrap_or_else(|| "gpt-5-5-thinking".to_string());
    // `is not None` (NOT truthiness): an explicit empty effort is kept.
    let effort = opts.effort.clone().or_else(|| cfg.thinking_effort.clone());

    let runtime = Arc::new(crate::runtime::ShimRuntime::new(cfg.clone()));
    let mux = Arc::new(crate::runtime::ConductorMux::new(
        runtime,
        cfg.clone(),
        model.clone(),
        effort.clone(),
    ));

    if !opts.lazy {
        // Python _amain: eager-boot one MAIN tab; on failure print and continue
        // serving ("will retry on first request").
        match mux.eager_boot().await {
            Ok(()) => tracing::info!(
                "[shim] booted shadow tab (model={model}, effort={})",
                effort.as_deref().unwrap_or("None")
            ),
            Err(e) => {
                tracing::info!("[shim] eager boot failed ({e}); will retry on first request")
            }
        }
    }

    let driver: Arc<dyn TurnDriver> = mux;
    let shim = ShadowResponsesShim::new(cfg, model, driver);
    let app = build_app(shim);

    let host: std::net::IpAddr = opts
        .host
        .parse()
        .unwrap_or(std::net::IpAddr::from([127, 0, 0, 1]));
    let addr = std::net::SocketAddr::new(host, opts.port);
    let listener = tokio::net::TcpListener::bind(addr).await?;
    tracing::info!("[shim] listening on http://{addr}  (POST /v1/responses)");
    axum::serve(listener, app).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    // ----- content_text / extract_prompt -----

    #[test]
    fn content_text_bare_string() {
        assert_eq!(content_text(&json!("hello")), "hello");
    }

    #[test]
    fn content_text_parts() {
        let c = json!([
            {"type": "input_text", "text": "a"},
            {"type": "output_text", "text": "b"},
            {"type": "text", "text": "c"},
            {"text": "d"},                // type absent -> included
            {"type": "image", "text": "x"}, // unknown type -> skipped
            "lit",                           // bare string part
        ]);
        assert_eq!(content_text(&c), "abcdlit");
    }

    #[test]
    fn extract_prompt_string_input() {
        let body = json!({"input": "  hi there \n"});
        assert_eq!(extract_prompt(&body), "hi there");
    }

    #[test]
    fn extract_prompt_takes_last_user_message() {
        let body = json!({
            "input": [
                {"role": "user", "content": [{"type": "input_text", "text": "first"}]},
                {"role": "assistant", "content": [{"type": "output_text", "text": "mid"}]},
                {"role": "user", "type": "message", "content": [{"type": "input_text", "text": "second"}]},
            ]
        });
        assert_eq!(extract_prompt(&body), "second");
    }

    #[test]
    fn extract_prompt_falls_back_to_instructions() {
        let body = json!({
            "input": [
                {"role": "system", "content": [{"type": "text", "text": "sys"}]}
            ],
            "instructions": "  do the thing  "
        });
        assert_eq!(extract_prompt(&body), "do the thing");
    }

    #[test]
    fn extract_prompt_empty_when_nothing() {
        let body = json!({"input": []});
        assert_eq!(extract_prompt(&body), "");
    }

    // ----- build_context_replay (L5: restart/resume context replay) -----

    fn msg(role: &str, text: &str) -> Value {
        json!({"role": role, "type": "message", "content": [{"type": "input_text", "text": text}]})
    }

    #[test]
    fn context_replay_builds_transcript_excluding_current_message() {
        let body = json!({"input": [
            msg("user", "the codeword is BANANA"),
            msg("assistant", "noted, codeword stored"),
            {"type": "function_call", "name": "shell_command", "arguments": "{}", "call_id": "c1"},
            {"type": "function_call_output", "call_id": "c1", "output": "ok"},
            msg("assistant", "ran the check"),
            msg("user", "what codeword?"),
        ]});
        let replay = build_context_replay(&body).expect("prior turns -> replay");
        assert!(replay.contains("USER: the codeword is BANANA"));
        assert!(replay.contains("ASSISTANT: noted, codeword stored"));
        assert!(replay.contains("(assistant ran tools)"));
        assert!(replay.contains("ASSISTANT: ran the check"));
        // The trailing user message is the CURRENT turn — never in the replay.
        assert!(!replay.contains("what codeword?"));
        // Clearly delimited + hands off to the current message.
        assert!(replay.contains("=== PRIOR CONVERSATION ==="));
        assert!(replay.contains("=== END PRIOR CONVERSATION ==="));
        assert!(replay.ends_with("Current message:\n"));
        // Order preserved: codeword line precedes the tool marker.
        let a = replay.find("BANANA").unwrap();
        let b = replay.find("(assistant ran tools)").unwrap();
        assert!(a < b);
    }

    #[test]
    fn context_replay_none_for_a_genuine_first_turn() {
        // Only the new user message (+ codex's per-request context blobs):
        // nothing to replay, behavior unchanged.
        let body = json!({"input": [
            msg("user", "<environment_context>\n<cwd>C:\\x</cwd>\n</environment_context>"),
            msg("user", "hello"),
        ]});
        assert!(build_context_replay(&body).is_none());
        assert!(build_context_replay(&json!({"input": "bare string"})).is_none());
        assert!(build_context_replay(&json!({"input": []})).is_none());
    }

    #[test]
    fn context_replay_is_bounded() {
        // 50 prior exchanges -> only the most recent 30 entries survive.
        let mut items: Vec<Value> = Vec::new();
        for i in 0..50 {
            items.push(msg("user", &format!("question {i}")));
            items.push(msg("assistant", &format!("answer {i}")));
        }
        items.push(msg("user", "current"));
        let replay = build_context_replay(&json!({"input": items})).expect("replay");
        assert!(!replay.contains("question 0")); // oldest dropped
        assert!(replay.contains("answer 49")); // newest kept
        assert!(!replay.contains("USER: current"));
        // A single huge message is truncated per-entry.
        let big = "x".repeat(10_000);
        let body = json!({"input": [msg("assistant", &big), msg("user", "now")]});
        let replay = build_context_replay(&body).expect("replay");
        assert!(replay.contains("…[truncated]"));
        assert!(replay.chars().count() < 17_000);
    }

    // ----- payload_text / last_tool_output -----

    #[test]
    fn payload_text_variants() {
        assert_eq!(payload_text(&json!("raw")), "raw");
        assert_eq!(payload_text(&json!({"content": "txt"})), "txt");
        assert_eq!(
            payload_text(&json!({"content": [{"type": "output_text", "text": "li"}]})),
            "li"
        );
        // dict without content -> json.dumps(dict)
        assert_eq!(payload_text(&json!({"x": 1})), "{\"x\":1}");
        assert_eq!(
            payload_text(&json!([{"type": "text", "text": "ab"}])),
            "ab"
        );
    }

    #[test]
    fn last_tool_output_detects_function_call_output() {
        let body = json!({
            "input": [
                {"role": "user", "content": "q"},
                {"type": "function_call_output", "output": "the result"}
            ]
        });
        assert_eq!(last_tool_output(&body).as_deref(), Some("the result"));
    }

    #[test]
    fn last_tool_output_none_for_user_turn() {
        let body = json!({"input": [{"role": "user", "content": "q"}]});
        assert!(last_tool_output(&body).is_none());
    }

    // ----- item builders -----

    #[test]
    fn message_item_empty_has_empty_content_array() {
        let item = message_item("", Some("msg_1"));
        assert_eq!(item["type"], "message");
        assert_eq!(item["role"], "assistant");
        assert_eq!(item["content"], json!([]));
        assert_eq!(item["id"], "msg_1");
    }

    #[test]
    fn message_item_with_text() {
        let item = message_item("hi", None);
        assert_eq!(item["content"], json!([{"type": "output_text", "text": "hi"}]));
        assert!(item.get("id").is_none());
    }

    #[test]
    fn completed_has_exact_usage_shape() {
        let c = completed("resp_x");
        let usage = &c["response"]["usage"];
        assert_eq!(c["response"]["id"], "resp_x");
        assert!(usage.get("input_tokens").is_some());
        assert!(usage["input_tokens_details"].is_null());
        assert!(usage.get("output_tokens").is_some());
        assert!(usage["output_tokens_details"].is_null());
        assert!(usage.get("total_tokens").is_some());
        // exactly 5 fields
        assert_eq!(usage.as_object().unwrap().len(), 5);
    }

    #[test]
    fn function_call_arguments_is_a_json_string() {
        let item = function_call_item("shell_command", &json!({"command": "ls"}), Some("call_1"));
        assert_eq!(item["type"], "function_call");
        assert_eq!(item["name"], "shell_command");
        // arguments MUST be a JSON *string* on the wire, not an object.
        assert!(item["arguments"].is_string());
        // The Python original json.dumps()-es with DEFAULT separators (a space
        // after `:` and `,`), so the wire string is `{"command": "ls"}`, NOT the
        // serde-compact `{"command":"ls"}`. Byte-fidelity with the original.
        assert_eq!(item["arguments"], "{\"command\": \"ls\"}");
        assert_eq!(item["call_id"], "call_1");
    }

    #[test]
    fn function_call_string_arguments_passthrough() {
        let item = function_call_item("t", &json!("{\"a\":1}"), Some("c"));
        assert_eq!(item["arguments"], "{\"a\":1}");
    }

    #[test]
    fn custom_tool_call_carries_raw_input() {
        let item = custom_tool_call_item("apply_patch", "*** Begin Patch", Some("c1"));
        assert_eq!(item["type"], "custom_tool_call");
        assert_eq!(item["name"], "apply_patch");
        assert_eq!(item["input"], "*** Begin Patch");
        assert_eq!(item["call_id"], "c1");
    }

    // ----- parse_tool_call -----

    #[test]
    fn parse_run_fence_primary() {
        let text = "Sure.\n```run\nGet-ChildItem\n```";
        let call = parse_tool_call(text).expect("a run fence is a tool call");
        assert_eq!(call.name, "shell_command");
        assert_eq!(call.command, "Get-ChildItem");
        assert_eq!(call.arguments(), json!({"command": "Get-ChildItem"}));
    }

    #[test]
    fn parse_empty_run_fence_is_none() {
        assert!(parse_tool_call("```run\n\n```").is_none());
    }

    #[test]
    fn parse_lone_shell_fence_dominates() {
        let text = "```powershell\nGet-Date\n```";
        let call = parse_tool_call(text).expect("a dominating shell fence is a tool call");
        assert_eq!(call.command, "Get-Date");
    }

    #[test]
    fn parse_shell_fence_with_long_prose_is_not_a_call() {
        let prose = "x".repeat(250);
        let text = format!("{prose}\n```bash\necho hi\n```");
        assert!(parse_tool_call(&text).is_none());
    }

    #[test]
    fn parse_two_shell_fences_is_not_a_call() {
        let text = "```sh\na\n```\nand\n```sh\nb\n```";
        assert!(parse_tool_call(text).is_none());
    }

    #[test]
    fn parse_final_answer_no_fence_is_none() {
        assert!(parse_tool_call("There are 7 items.").is_none());
    }

    #[test]
    fn run_fence_takes_precedence_over_shell_fence() {
        let text = "```run\nreal\n```\n```powershell\nexample\n```";
        let call = parse_tool_call(text).expect("run fence wins");
        assert_eq!(call.command, "real");
    }

    // ----- preamble -----

    #[test]
    fn tool_preamble_ends_with_task_marker() {
        let p = build_tool_preamble(&[]);
        assert!(p.ends_with("TASK:\n"));
        assert!(p.contains("```run"));
    }

    // ----- default_build_injection -----

    #[test]
    fn build_injection_forwards_tool_result() {
        let body = json!({
            "input": [{"type": "function_call_output", "output": "  42  "}]
        });
        let mut sent = false;
        let inj = default_build_injection(&body, &[json!({})], &mut sent);
        assert!(inj.starts_with("TOOL RESULT"));
        assert!(inj.contains("42"));
        assert!(!sent); // preamble flag untouched on a tool-result turn
    }

    #[test]
    fn build_injection_prepends_preamble_once() {
        let body = json!({"input": "hello"});
        let tools = vec![json!({"type": "function"})];
        let mut sent = false;
        let first = default_build_injection(&body, &tools, &mut sent);
        assert!(sent);
        assert!(first.starts_with(TOOL_PROTOCOL));
        assert!(first.ends_with("hello"));
        // second call: preamble already sent -> just the user text
        let second = default_build_injection(&body, &tools, &mut sent);
        assert_eq!(second, "hello");
    }

    #[test]
    fn build_injection_no_tools_is_plain_user() {
        let body = json!({"input": "hello"});
        let mut sent = false;
        let inj = default_build_injection(&body, &[], &mut sent);
        assert_eq!(inj, "hello");
        assert!(!sent);
    }

    // ----- sse_frame -----

    #[test]
    fn sse_frame_is_single_line_and_escapes_newlines() {
        let frame = sse_frame(&json!({"type": "response.output_text.delta", "delta": "a\nb"}));
        assert!(frame.starts_with("data: "));
        assert!(frame.ends_with("\n\n"));
        // the payload itself must not contain a raw newline (only the trailing \n\n)
        let payload = frame.trim_end_matches("\n\n").trim_start_matches("data: ");
        assert!(!payload.contains('\n'));
        assert!(payload.contains("a\\nb"));
    }

    // ----- emit_turn sequences -----

    async fn drain(rx: &mut mpsc::Receiver<String>) -> Vec<Value> {
        let mut out = Vec::new();
        while let Some(line) = rx.recv().await {
            let payload = line
                .trim_end_matches("\n\n")
                .trim_start_matches("data: ")
                .to_string();
            out.push(serde_json::from_str::<Value>(&payload).expect("valid json frame"));
        }
        out
    }

    #[tokio::test]
    async fn emit_turn_final_answer_sequence() {
        let (tx, mut rx) = mpsc::channel::<String>(16);
        emit_turn(&tx, "resp_1", true, "prompt", "the answer").await;
        drop(tx);
        let frames = drain(&mut rx).await;
        let kinds: Vec<&str> = frames.iter().map(|f| f["type"].as_str().unwrap()).collect();
        assert_eq!(
            kinds,
            vec![
                "response.created",
                "response.output_item.added",
                "response.output_text.delta",
                "response.output_item.done",
                "response.completed",
            ]
        );
        // added carries an OPEN (content:[]) item; done carries the text.
        assert_eq!(frames[1]["item"]["content"], json!([]));
        assert_eq!(frames[2]["delta"], "the answer");
        assert_eq!(
            frames[3]["item"]["content"],
            json!([{"type": "output_text", "text": "the answer"}])
        );
    }

    #[tokio::test]
    async fn emit_turn_tool_call_sequence() {
        let (tx, mut rx) = mpsc::channel::<String>(16);
        emit_turn(&tx, "resp_2", true, "prompt", "```run\nGet-Date\n```").await;
        drop(tx);
        let frames = drain(&mut rx).await;
        let kinds: Vec<&str> = frames.iter().map(|f| f["type"].as_str().unwrap()).collect();
        assert_eq!(
            kinds,
            vec![
                "response.created",
                "response.output_item.done",
                "response.completed",
            ]
        );
        let item = &frames[1]["item"];
        assert_eq!(item["type"], "function_call");
        assert_eq!(item["name"], "shell_command");
        assert!(item["arguments"].is_string());
        // json.dumps default separators -> a space after the colon, matching the
        // Python original (`{"command": "Get-Date"}`), not serde-compact.
        assert_eq!(item["arguments"], "{\"command\": \"Get-Date\"}");
    }

    #[tokio::test]
    async fn emit_turn_empty_injection_sequence() {
        let (tx, mut rx) = mpsc::channel::<String>(16);
        emit_turn(&tx, "resp_3", false, "", "").await;
        drop(tx);
        let frames = drain(&mut rx).await;
        let kinds: Vec<&str> = frames.iter().map(|f| f["type"].as_str().unwrap()).collect();
        assert_eq!(
            kinds,
            vec![
                "response.created",
                "response.output_item.added",
                "response.output_item.done",
                "response.completed",
            ]
        );
    }

    // ----- /v1/models stub (FIX B) -----

    /// Bare driver: only the required `collect_turn`; control routes keep their
    /// trait defaults. Enough to build the app for an HTTP-route test.
    struct ModelsTestDriver;

    #[async_trait::async_trait]
    impl TurnDriver for ModelsTestDriver {
        async fn collect_turn(
            &self,
            _thread_id: Option<&str>,
            _subagent_kind: Option<&str>,
            _body: &Value,
            _inject_text: &str,
        ) -> anyhow::Result<String> {
            Ok(String::new())
        }
    }

    /// `GET /v1/models` (and the `/models`, POST, no-`/v1` variants) must answer
    /// 200 with a parseable OpenAI models-list that includes the configured model
    /// — never 404, never 401 — so codex's startup/doctor probe stops erroring.
    #[tokio::test]
    async fn models_route_returns_200_parseable_list() {
        let shim = ShadowResponsesShim::new(
            crate::config::ShadowConfig::default(),
            "gpt-5-5-thinking",
            Arc::new(ModelsTestDriver),
        );
        let app = build_app(shim);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind app");
        let port = listener.local_addr().expect("addr").port();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        let base = format!("http://127.0.0.1:{port}");
        let client = reqwest::Client::new();

        // The exact probe codex makes at startup, including the query string.
        for url in [
            format!("{base}/v1/models?client_version=0.139.0"),
            format!("{base}/models"),
        ] {
            let resp = client.get(&url).send().await.expect("request");
            assert_eq!(
                resp.status(),
                reqwest::StatusCode::OK,
                "{url} must be 200 (never 404/401)"
            );
            let body: Value = resp.json().await.expect("parseable JSON list");
            assert_eq!(body["object"], "list");
            let data = body["data"].as_array().expect("data array");
            assert!(!data.is_empty(), "at least one model id");
            assert_eq!(data[0]["id"], "gpt-5-5-thinking");
            assert_eq!(data[0]["object"], "model");
            assert_eq!(data[0]["owned_by"], "cyrus");
        }

        // POST is also accepted (some clients probe with POST).
        let resp = client
            .post(format!("{base}/v1/models"))
            .send()
            .await
            .expect("post request");
        assert_eq!(resp.status(), reqwest::StatusCode::OK);
    }
}
