//! Parser for ChatGPT's "v1" streaming JSON-patch delta encoding.
//!
//! ChatGPT streams a turn as JSON-patch-style deltas over its WebSocket. Each
//! `data:` payload is one of:
//!
//! ```text
//!   "v1"                                              # encoding declaration
//!   {"type": "input_message", ...}                    # user echo (ignored)
//!   {"p":"", "o":"add", "v":{"message":{...}}}        # a new message appears
//!   {"v":{"message":{...}}}                            # message snapshot/switch
//!   {"p":"/message/content/parts/0","o":"append","v":"tok"}   # a content token
//!   {"o":"patch","v":[ {sub-op}, ... ]}               # a batch of ops
//!   {"v":"tok"}                                        # bare continuation (append to last path/op)
//!   {"type":"message_marker","message_id":X,"marker":"user_visible_token"|"cot_token"|...}
//!   {"type":"message_stream_complete", ...}           # turn is done
//! ```
//!
//! This reconstructs messages and emits high-level events. It only treats the
//! user-visible assistant message as answer text; chain-of-thought (cot) tokens
//! are emitted separately as 'thinking'.
//!
//! # Hazards
//!   - The bare-`{"v":tok}` continuation appends to `last_append_path`. Patches /
//!     replaces must NOT move that target — only an explicit `"append"` updates
//!     it. Getting this wrong scrambles token order.
//!   - The message map must be insertion-ordered for the visible-message "last
//!     one wins" rule. We use a `Vec<(MessageId, Message)>` with linear lookup.

use serde_json::Value;
use std::collections::HashSet;

/// Events emitted by [`V1DeltaParser::feed`]: token / thinking / turn_complete.
///
/// `StreamError` is a typed event matching obvious error shapes (rate-limit /
/// moderation / server error). It carries the raw type plus the best-effort
/// code/message so the conductor can fail the turn immediately instead of
/// waiting out the 90s stall watchdog. The token-byte paths are untouched —
/// this only adds handling for events that would otherwise be dropped.
#[derive(Debug, Clone)]
pub enum Event {
    Token(String),
    Thinking(String),
    TurnComplete(String),
    StreamError {
        /// The raw `type` field of the event.
        etype: String,
        /// `error.code` / top-level `code` when present, else "".
        code: String,
        /// `error.message` / top-level `message` when present, else "".
        message: String,
    },
}

/// A message id. ChatGPT ids arrive either as strings (`message.id`) or as
/// arbitrary JSON scalars (`message_id` on markers, which can be ints). An int
/// `5` and a string `"5"` are distinct keys, so we key on the raw JSON value.
type MessageId = Value;

/// One reconstructed message.
#[derive(Debug, Clone)]
struct Message {
    role: String,
    parts: Vec<String>,
    is_cot: bool,
    visible: bool,
}

#[derive(Debug)]
pub struct V1DeltaParser {
    /// `id -> {role, parts, is_cot, visible}`, kept in insertion order.
    messages: Vec<(MessageId, Message)>,
    current_id: Option<MessageId>,
    /// The v1 streaming optimization: a bare `{"v": "tok"}` appends to the last
    /// APPEND target. Patches / replaces must NOT change this.
    last_append_path: String,
    /// Observability (fix wave 2): unrecognized typed-event types already logged
    /// this turn (parsers are recreated per turn via `tap.reset()`), so each
    /// unknown type is logged at most once per turn.
    seen_unknown_types: HashSet<String>,
}

impl Default for V1DeltaParser {
    fn default() -> Self {
        Self {
            messages: Vec::new(),
            current_id: None,
            last_append_path: "/message/content/parts/0".to_string(),
            seen_unknown_types: HashSet::new(),
        }
    }
}

impl V1DeltaParser {
    // ---- public ----

    pub fn feed(&mut self, data: &str) -> Vec<Event> {
        let data = data.trim();
        if data.is_empty() || data == "\"v1\"" {
            return Vec::new();
        }
        let obj: Value = match serde_json::from_str(data) {
            Ok(v) => v,
            Err(_) => return Vec::new(),
        };
        if !obj.is_object() {
            return Vec::new();
        }

        let t = obj.get("type");
        // An absent `type` key falls through to delta-op handling (the `None`
        // arm). A present JSON `null` is `Value::Null` — treated like an
        // unrecognized type (falls through the typed branches to `return []`).
        match t {
            Some(Value::String(s)) if s == "message_stream_complete" => {
                return vec![Event::TurnComplete(self.answer_text())];
            }
            Some(Value::String(s)) if s == "message_marker" => {
                self.marker(&obj);
                return Vec::new();
            }
            Some(Value::String(s))
                if matches!(
                    s.as_str(),
                    "input_message"
                        | "title_generation"
                        | "server_ste_metadata"
                        | "resume_conversation_token"
                        | "stream_handoff"
                ) =>
            {
                return Vec::new();
            }
            // Any other present `type` (a different string, or a non-string
            // value like null/number): surface an error-shaped event as
            // `StreamError` so rate-limit / moderation / server errors don't
            // degrade to the 90s stall watchdog, and log everything else (once
            // per type per turn) so real error shapes can be learned. Token-byte
            // handling is unchanged.
            Some(t) => {
                let etype = match t {
                    Value::String(s) => s.clone(),
                    other => other.to_string(),
                };
                if let Some((code, message)) = error_fields(&etype, &obj) {
                    return vec![Event::StreamError {
                        etype,
                        code,
                        message,
                    }];
                }
                self.log_unknown_type(&etype, data);
                return Vec::new();
            }
            // type key absent -> fall through to delta-op handling.
            None => {}
        }

        // otherwise it's a delta op
        self.apply(&obj)
    }

    pub fn answer_text(&self) -> String {
        match self.visible_message() {
            Some(msg) => msg.parts.concat(),
            None => String::new(),
        }
    }

    // ---- internals ----

    /// Observability for unrecognized typed events: log the type once per turn
    /// (info), with a truncated raw payload at debug. This is the raw data we
    /// need to learn ChatGPT's real error shapes without spamming the log.
    fn log_unknown_type(&mut self, etype: &str, raw: &str) {
        if self.seen_unknown_types.insert(etype.to_string()) {
            tracing::info!("[shim] tap: unrecognized stream event type={etype}");
            let snippet: String = raw.chars().take(400).collect();
            tracing::debug!("[shim] tap: type={etype} payload[0..400]={snippet}");
        }
    }

    fn marker(&mut self, obj: &Value) {
        let mid = obj.get("message_id");
        let marker = obj
            .get("marker")
            .and_then(Value::as_str)
            .unwrap_or("");
        // absent message_id -> skip.
        let mid = match mid {
            Some(v) => v.clone(),
            None => return,
        };
        // existing message, or a fresh invisible one (visible flipped on below).
        let m = self.get_or_insert_with(&mid, || Message {
            role: "assistant".to_string(),
            parts: vec![String::new()],
            is_cot: false,
            visible: false,
        });
        if marker == "cot_token" {
            m.is_cot = true;
        } else if marker == "user_visible_token" || marker == "final_channel_token" {
            m.visible = true;
        }
    }

    fn register_message(&mut self, message: &Value) {
        let mid = message.get("id");
        // skip on a falsy id (absent, null, empty string, etc.).
        let mid = match mid {
            Some(v) if is_truthy(v) => v.clone(),
            _ => return,
        };

        // role from a truthy author object, defaulting to "assistant".
        let role = message
            .get("author")
            .filter(|a| is_truthy(a))
            .and_then(|a| a.get("role"))
            .and_then(Value::as_str)
            .unwrap_or("assistant")
            .to_string();

        // parts from a truthy content object; non-string entries become "",
        // and any falsy/missing result falls back to `[""]` (see below).
        let raw_parts = message
            .get("content")
            .filter(|c| is_truthy(c))
            .and_then(|c| c.get("parts"));
        let parts: Vec<String> = match raw_parts {
            Some(Value::Array(arr)) if !arr.is_empty() => {
                let mapped: Vec<String> = arr
                    .iter()
                    .map(|p| match p {
                        Value::String(s) => s.clone(),
                        _ => String::new(),
                    })
                    .collect();
                // a non-empty array is kept as-is.
                mapped
            }
            // falsy parts (absent, null, empty array, non-array) -> `[""]`.
            _ => vec![String::new()],
        };

        let existing = self.get(&mid);
        let is_cot = existing.map(|m| m.is_cot).unwrap_or(false);
        let visible = existing
            .map(|m| m.visible)
            .unwrap_or_else(|| role == "assistant");

        self.insert(
            mid.clone(),
            Message {
                role,
                parts,
                is_cot,
                visible,
            },
        );
        self.current_id = Some(mid);
    }

    fn apply(&mut self, obj: &Value) -> Vec<Event> {
        let op = obj.get("o");
        let path = obj.get("p");
        let v = obj.get("v");

        // message add / snapshot (v carries a full message; op add or absent,
        // path root or absent)
        if let Some(v_val) = v {
            if v_val.is_object() && v_val.get("message").is_some() {
                let op_ok = match op {
                    None => true,
                    Some(Value::String(s)) => s == "add",
                    Some(_) => false,
                };
                let path_ok = match path {
                    None => true,
                    Some(Value::String(s)) => s.is_empty(),
                    Some(_) => false,
                };
                if op_ok && path_ok {
                    self.register_message(v_val.get("message").unwrap());
                    return Vec::new();
                }
            }
        }

        // batch of ops
        if matches!(op, Some(Value::String(s)) if s == "patch") {
            if let Some(Value::Array(arr)) = v {
                let mut out: Vec<Event> = Vec::new();
                for sub in arr {
                    if sub.is_object() {
                        out.extend(self.apply(sub));
                    }
                }
                return out;
            }
        }

        // explicit append (a content token) — updates the continuation target
        if matches!(op, Some(Value::String(s)) if s == "append") {
            if let Some(Value::String(p)) = path {
                self.last_append_path = p.clone();
                let pclone = p.clone();
                return self.append(&pclone, v);
            }
        }

        // other explicit ops (replace status/metadata, etc.) — no event, no
        // continuation change.
        if op.is_some() && path.is_some() {
            return Vec::new();
        }

        // bare {"v": ...} — the streaming continuation.
        if v.is_some() && op.is_none() && path.is_none() {
            let v_val = v.unwrap();
            if v_val.is_object() && v_val.get("message").is_some() {
                self.register_message(v_val.get("message").unwrap());
                return Vec::new();
            }
            if let Value::String(s) = v_val {
                let path = self.last_append_path.clone();
                return self.append(&path, Some(&Value::String(s.clone())));
            }
        }
        Vec::new()
    }

    fn append(&mut self, path: &str, v: Option<&Value>) -> Vec<Event> {
        let v_str = match v {
            Some(Value::String(s)) => s.clone(),
            _ => return Vec::new(),
        };
        if !path.contains("/content/parts/") {
            return Vec::new();
        }
        // idx = trailing path segment, falling back to 0 if it doesn't parse.
        let idx: usize = path
            .rsplit('/')
            .next()
            .and_then(|tail| tail.parse::<usize>().ok())
            .unwrap_or(0);

        let current_id = match &self.current_id {
            Some(id) => id.clone(),
            None => return Vec::new(),
        };
        let msg = match self.get_mut(&current_id) {
            Some(m) => m,
            None => return Vec::new(),
        };
        while msg.parts.len() <= idx {
            msg.parts.push(String::new());
        }
        msg.parts[idx].push_str(&v_str);

        if msg.is_cot {
            return vec![Event::Thinking(v_str)];
        }
        if msg.visible && msg.role == "assistant" {
            return vec![Event::Token(v_str)];
        }
        Vec::new()
    }

    fn visible_message(&self) -> Option<&Message> {
        // the assistant message that's user-visible and not chain-of-thought;
        // last one wins (insertion order).
        let mut best: Option<&Message> = None;
        for (_, m) in &self.messages {
            if m.role == "assistant" && !m.is_cot && m.visible {
                best = Some(m);
            }
        }
        best
    }

    // ---- insertion-ordered map helpers ----

    fn get(&self, id: &MessageId) -> Option<&Message> {
        self.messages
            .iter()
            .find(|(k, _)| k == id)
            .map(|(_, m)| m)
    }

    fn get_mut(&mut self, id: &MessageId) -> Option<&mut Message> {
        self.messages
            .iter_mut()
            .find(|(k, _)| k == id)
            .map(|(_, m)| m)
    }

    /// Replace in place (preserving the existing slot's position) or append a
    /// new entry.
    fn insert(&mut self, id: MessageId, value: Message) {
        if let Some(slot) = self.messages.iter_mut().find(|(k, _)| *k == id) {
            slot.1 = value;
        } else {
            self.messages.push((id, value));
        }
    }

    /// Return the existing entry, or insert the default (appended at the end)
    /// and return that.
    fn get_or_insert_with<F>(&mut self, id: &MessageId, default: F) -> &mut Message
    where
        F: FnOnce() -> Message,
    {
        let pos = self.messages.iter().position(|(k, _)| k == id);
        match pos {
            Some(i) => &mut self.messages[i].1,
            None => {
                self.messages.push((id.clone(), default()));
                &mut self.messages.last_mut().unwrap().1
            }
        }
    }
}

/// Heuristic error detection over a typed event the parser doesn't otherwise
/// recognize. Returns `Some((code, message))` when the event looks like a
/// server-side error:
///   - its `type` contains "error" / "rate_limit" (deliberately NOT bare
///     "rate", which substring-matches benign words like "generated"),
///   - OR the payload carries a top-level `error` object,
///   - OR the payload carries BOTH top-level `code` and `message` fields.
///
/// "moderation" is deliberately NOT a type-name trigger. ChatGPT streams benign
/// moderation/safety ANNOTATION events (`url_moderation`, URL-safety tagging) on
/// perfectly healthy turns — the answer streams fine right after — so flagging
/// the bare word produced false "ChatGPT refused the turn (moderation)" fatals
/// on turns that were never refused. A genuine content block still surfaces: it
/// carries an explicit `error` object / `code`+`message` (caught below), or it
/// arrives as a normal safe-completion message (handled as answer text).
///
/// Conservative by design: anything that doesn't match stays on the current
/// behavior (logged + dropped; the stall watchdog remains the backstop).
fn error_fields(etype: &str, obj: &Value) -> Option<(String, String)> {
    let t = etype.to_ascii_lowercase();
    // Known benign moderation/safety annotations: never an error, even if a
    // future variant grows a code/message field.
    const BENIGN: [&str; 3] = ["url_moderation", "url_safe", "safe_url"];
    if BENIGN.iter().any(|b| t.contains(b)) {
        return None;
    }
    let typed_error =
        t.contains("error") || t.contains("rate_limit") || t.contains("rate-limit");
    let err_obj = obj.get("error").filter(|e| !e.is_null());
    let has_code_msg = obj.get("code").is_some() && obj.get("message").is_some();
    if !(typed_error || err_obj.is_some() || has_code_msg) {
        return None;
    }
    let pick = |key: &str| -> String {
        err_obj
            .and_then(|e| e.get(key))
            .and_then(Value::as_str)
            .or_else(|| obj.get(key).and_then(Value::as_str))
            .unwrap_or("")
            .to_string()
    };
    Some((pick("code"), pick("message")))
}

/// Python-style truthiness for the JSON values we branch on (message id,
/// author, content).
///
/// Falsy: `None`/absent (handled by the caller via `Option`), JSON `null`,
/// `false`, `0`/`0.0`, empty string, empty array, empty object.
fn is_truthy(v: &Value) -> bool {
    match v {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                i != 0
            } else if let Some(u) = n.as_u64() {
                u != 0
            } else if let Some(f) = n.as_f64() {
                f != 0.0
            } else {
                true
            }
        }
        Value::String(s) => !s.is_empty(),
        Value::Array(a) => !a.is_empty(),
        Value::Object(o) => !o.is_empty(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tokens(events: &[Event]) -> String {
        events
            .iter()
            .filter_map(|e| match e {
                Event::Token(s) => Some(s.as_str()),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn ignores_v1_declaration_and_blank() {
        let mut p = V1DeltaParser::default();
        assert!(p.feed("\"v1\"").is_empty());
        assert!(p.feed("   ").is_empty());
        assert!(p.feed("").is_empty());
        assert!(p.feed("not json").is_empty());
        assert!(p.feed("[1,2,3]").is_empty()); // not a dict
    }

    #[test]
    fn register_and_append_visible_assistant() {
        let mut p = V1DeltaParser::default();
        p.feed(r#"{"p":"","o":"add","v":{"message":{"id":"m1","author":{"role":"assistant"},"content":{"parts":[""]}}}}"#);
        let ev = p.feed(r#"{"p":"/message/content/parts/0","o":"append","v":"He"}"#);
        assert_eq!(tokens(&ev), "He");
        let ev = p.feed(r#"{"v":"llo"}"#); // bare continuation
        assert_eq!(tokens(&ev), "llo");
        assert_eq!(p.answer_text(), "Hello");
    }

    #[test]
    fn bare_continuation_not_moved_by_patch_or_replace() {
        // Hazard: only an explicit "append" may move last_append_path.
        let mut p = V1DeltaParser::default();
        p.feed(r#"{"v":{"message":{"id":"m1","author":{"role":"assistant"},"content":{"parts":[""]}}}}"#);
        p.feed(r#"{"p":"/message/content/parts/0","o":"append","v":"A"}"#);
        // a replace on some other path must NOT change the continuation target
        p.feed(r#"{"p":"/message/metadata/status","o":"replace","v":"finished"}"#);
        let ev = p.feed(r#"{"v":"B"}"#);
        assert_eq!(tokens(&ev), "B");
        assert_eq!(p.answer_text(), "AB");
    }

    #[test]
    fn patch_batch_recurses() {
        let mut p = V1DeltaParser::default();
        p.feed(r#"{"v":{"message":{"id":"m1","author":{"role":"assistant"},"content":{"parts":[""]}}}}"#);
        let ev = p.feed(
            r#"{"o":"patch","v":[{"p":"/message/content/parts/0","o":"append","v":"X"},{"v":"Y"}]}"#,
        );
        assert_eq!(tokens(&ev), "XY");
    }

    #[test]
    fn cot_emits_thinking_not_token() {
        let mut p = V1DeltaParser::default();
        p.feed(r#"{"v":{"message":{"id":"m1","author":{"role":"assistant"},"content":{"parts":[""]}}}}"#);
        p.feed(r#"{"type":"message_marker","message_id":"m1","marker":"cot_token"}"#);
        let ev = p.feed(r#"{"p":"/message/content/parts/0","o":"append","v":"think"}"#);
        assert!(matches!(ev.as_slice(), [Event::Thinking(s)] if s == "think"));
        // cot text is not the answer text
        assert_eq!(p.answer_text(), "");
    }

    #[test]
    fn last_visible_assistant_wins() {
        let mut p = V1DeltaParser::default();
        p.feed(r#"{"v":{"message":{"id":"m1","author":{"role":"assistant"},"content":{"parts":["first"]}}}}"#);
        p.feed(r#"{"v":{"message":{"id":"m2","author":{"role":"assistant"},"content":{"parts":["second"]}}}}"#);
        assert_eq!(p.answer_text(), "second");
    }

    #[test]
    fn turn_complete_carries_answer_text() {
        let mut p = V1DeltaParser::default();
        p.feed(r#"{"v":{"message":{"id":"m1","author":{"role":"assistant"},"content":{"parts":[""]}}}}"#);
        p.feed(r#"{"p":"/message/content/parts/0","o":"append","v":"done"}"#);
        let ev = p.feed(r#"{"type":"message_stream_complete"}"#);
        assert!(matches!(ev.as_slice(), [Event::TurnComplete(s)] if s == "done"));
    }

    #[test]
    fn user_message_not_treated_as_answer_or_token() {
        let mut p = V1DeltaParser::default();
        p.feed(r#"{"v":{"message":{"id":"u1","author":{"role":"user"},"content":{"parts":[""]}}}}"#);
        let ev = p.feed(r#"{"p":"/message/content/parts/0","o":"append","v":"hi"}"#);
        assert!(ev.is_empty()); // user role, not visible-assistant
        assert_eq!(p.answer_text(), "");
    }

    #[test]
    fn error_typed_event_surfaces_stream_error() {
        let mut p = V1DeltaParser::default();
        // type names the error
        let ev = p.feed(r#"{"type":"rate_limit_error","message":"try again in 30 seconds"}"#);
        assert!(matches!(
            ev.as_slice(),
            [Event::StreamError { etype, message, .. }]
                if etype == "rate_limit_error" && message == "try again in 30 seconds"
        ));
        // nested error object carries code/message
        let ev = p.feed(
            r#"{"type":"server_event","error":{"code":"too_many_requests","message":"slow down"}}"#,
        );
        assert!(matches!(
            ev.as_slice(),
            [Event::StreamError { code, message, .. }]
                if code == "too_many_requests" && message == "slow down"
        ));
        // top-level code+message pair
        let ev = p.feed(r#"{"type":"weird_kind","code":"moderation_blocked","message":"nope"}"#);
        assert!(matches!(
            ev.as_slice(),
            [Event::StreamError { code, .. }] if code == "moderation_blocked"
        ));
    }

    #[test]
    fn benign_url_moderation_is_not_an_error() {
        let mut p = V1DeltaParser::default();
        // ChatGPT's URL-safety annotation: contains "moderation" but never blocks.
        // It must NOT surface as a fatal StreamError (the "it just lied" regression).
        assert!(p
            .feed(r#"{"type":"url_moderation","url_moderation_result":{"full_url":"x"}}"#)
            .is_empty());
        // even if a future variant grows code+message, the type stays benign.
        assert!(p
            .feed(r#"{"type":"url_moderation","code":"safe","message":"ok"}"#)
            .is_empty());
        // a real answer still streams fine afterward.
        p.feed(r#"{"v":{"message":{"id":"m1","author":{"role":"assistant"},"content":{"parts":[""]}}}}"#);
        let ev = p.feed(r#"{"p":"/message/content/parts/0","o":"append","v":"hi"}"#);
        assert_eq!(tokens(&ev), "hi");
    }

    #[test]
    fn benign_unknown_typed_events_still_dropped() {
        let mut p = V1DeltaParser::default();
        // unknown but not error-shaped: dropped (logged once), no events.
        assert!(p.feed(r#"{"type":"tokens_generated","count":5}"#).is_empty());
        assert!(p.feed(r#"{"type":"tokens_generated","count":6}"#).is_empty());
        // a lone top-level "message" field withOUT "code" is not error-shaped.
        assert!(p
            .feed(r#"{"type":"status_update","message":"thinking hard"}"#)
            .is_empty());
        // and the known-ignored types stay ignored.
        assert!(p.feed(r#"{"type":"title_generation","title":"x"}"#).is_empty());
        // token state is unaffected by the above
        p.feed(r#"{"v":{"message":{"id":"m1","author":{"role":"assistant"},"content":{"parts":[""]}}}}"#);
        let ev = p.feed(r#"{"p":"/message/content/parts/0","o":"append","v":"ok"}"#);
        assert_eq!(tokens(&ev), "ok");
    }

    #[test]
    fn marker_creates_invisible_message_until_user_visible() {
        let mut p = V1DeltaParser::default();
        // marker arrives before any register; setdefault creates visible:false
        p.feed(r#"{"type":"message_marker","message_id":"m1","marker":"user_visible_token"}"#);
        // register reuses existing visible (true) because marker set it
        p.feed(r#"{"v":{"message":{"id":"m1","author":{"role":"assistant"},"content":{"parts":[""]}}}}"#);
        let ev = p.feed(r#"{"p":"/message/content/parts/0","o":"append","v":"ok"}"#);
        assert_eq!(tokens(&ev), "ok");
    }
}
