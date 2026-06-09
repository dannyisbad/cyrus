//! Rust-port side of the differential harness.
//!
//! Reads the shared fixtures (../../tests/fixtures) and renders a CANONICAL,
//! line-oriented text report by driving the actual `cyrus-lipsync` /
//! `cyrus-chimera` port code. The Python and Node drivers emit the SAME report
//! format for the SAME fixtures; the integration test byte-diffs them.
//!
//! Usage:
//!   cyrus-diff-emit <area> <fixtures_dir>
//! where <area> is one of: v1delta | sse | parse_tool_call | relay | oauth | all
//!
//! Canonical line grammar (every side must match byte-for-byte):
//!   each logical record is `<tag>\t<json-or-literal>` on one line, `\n`-terminated.
//!   JSON values are serialized compactly (serde_json default: no spaces,
//!   non-ASCII kept verbatim) so the three serializers agree. Where a side would
//!   otherwise emit a random id, the fixture supplies a fixed id.

use std::path::Path;
use std::path::PathBuf;

use serde_json::json;
use serde_json::Value;

use cyrus_chimera::oauth::difftest;
use cyrus_lipsync::responses;
use cyrus_lipsync::v1delta::{Event, V1DeltaParser};

fn read_fixture(dir: &Path, name: &str) -> Value {
    let path = dir.join(name);
    let text = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("read fixture {}: {e}", path.display()));
    serde_json::from_str(&text)
        .unwrap_or_else(|e| panic!("parse fixture {}: {e}", path.display()))
}

/// Compact JSON, matching serde_json's default (no spaces) — the canonical form
/// all three sides must agree on.
fn cj(v: &Value) -> String {
    serde_json::to_string(v).expect("serialize")
}

fn line(out: &mut String, tag: &str, payload: &str) {
    out.push_str(tag);
    out.push('\t');
    out.push_str(payload);
    out.push('\n');
}

// ===== v1delta ==============================================================

fn emit_v1delta(dir: &Path, out: &mut String) {
    // (1) recorded frames file: feed line by line, emit each event.
    let frames_path = dir.join("v1delta_frames.jsonl");
    let frames = std::fs::read_to_string(&frames_path)
        .unwrap_or_else(|e| panic!("read {}: {e}", frames_path.display()));
    let mut p = V1DeltaParser::default();
    for raw in frames.lines() {
        if raw.trim().is_empty() {
            continue;
        }
        for ev in p.feed(raw) {
            emit_event(out, "v1delta.frames", &ev);
        }
    }
    line(out, "v1delta.frames.answer", &cj(&json!(p.answer_text())));

    // (2) adversarial tokens: push each token as an append frame, collect events.
    let adv = read_fixture(dir, "adversarial_tokens.json");
    for case in adv["cases"].as_array().unwrap() {
        let name = case["name"].as_str().unwrap();
        let mut p = V1DeltaParser::default();
        // register a visible assistant message first
        p.feed(r#"{"v":{"message":{"id":"m1","author":{"role":"assistant"},"content":{"parts":[""]}}}}"#);
        for tok in case["tokens"].as_array().unwrap() {
            let tok = tok.as_str().unwrap();
            // an explicit append frame carrying this token
            let frame = json!({"p":"/message/content/parts/0","o":"append","v":tok});
            for ev in p.feed(&cj(&frame)) {
                emit_event(out, &format!("v1delta.adv.{name}"), &ev);
            }
        }
        line(
            out,
            &format!("v1delta.adv.{name}.answer"),
            &cj(&json!(p.answer_text())),
        );
    }
}

fn emit_event(out: &mut String, tag: &str, ev: &Event) {
    let (kind, val) = match ev {
        Event::Token(s) => ("token", s),
        Event::Thinking(s) => ("thinking", s),
        Event::TurnComplete(s) => ("turn_complete", s),
    };
    line(out, tag, &cj(&json!({"kind": kind, "value": val})));
}

// ===== SSE emission =========================================================

fn emit_sse(dir: &Path, out: &mut String) {
    let fx = read_fixture(dir, "sse_sequences.json");

    // extract_prompt
    for case in fx["extract_prompt"].as_array().unwrap() {
        let name = case["name"].as_str().unwrap();
        let got = responses::extract_prompt(&case["body"]);
        line(out, &format!("sse.extract.{name}"), &cj(&json!(got)));
    }

    // full_turn: reproduce run_turn's final-answer frame sequence deterministically.
    for case in fx["full_turn"].as_array().unwrap() {
        let name = case["name"].as_str().unwrap();
        let response_id = case["response_id"].as_str().unwrap();
        let item_id = case["item_id"].as_str().unwrap();
        let tokens: Vec<String> = case["tokens"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t.as_str().unwrap().to_string())
            .collect();
        let frames = sse_final_answer_frames(response_id, item_id, &tokens);
        for f in &frames {
            // JSON-encode the full SSE frame STRING (including its trailing
            // "\n\n") so the record stays on one canonical line.
            let frame_str = responses::sse_frame(f);
            line(out, &format!("sse.turn.{name}"), &cj(&json!(frame_str)));
        }
    }
}

/// The exact frame sequence run_turn emits for a final answer (non-tool turn),
/// with fixed ids so the bytes are reproducible:
///   created -> output_item.added(open, content:[]) -> output_text.delta (per
///   token, only if non-empty acc... actually the originals emit ONE delta with
///   the whole buffered text) -> output_item.done -> completed.
///
/// To match selftest.py::_emit_sequence we stream ONE delta per token (that file
/// streams token-by-token), then a single output_item.done with the full text.
fn sse_final_answer_frames(response_id: &str, item_id: &str, tokens: &[String]) -> Vec<Value> {
    let mut frames = Vec::new();
    frames.push(json!({"type": "response.created", "response": {}}));
    frames.push(json!({
        "type": "response.output_item.added",
        "item": responses::message_item("", Some(item_id)),
    }));
    let mut acc = String::new();
    for tok in tokens {
        acc.push_str(tok);
        frames.push(json!({"type": "response.output_text.delta", "delta": tok}));
    }
    frames.push(json!({
        "type": "response.output_item.done",
        "item": responses::message_item(&acc, Some(item_id)),
    }));
    frames.push(responses::completed(response_id));
    frames
}

// ===== parse_tool_call (chimera ReAct relay) ================================

fn emit_parse_tool_call(dir: &Path, out: &mut String) {
    let fx = read_fixture(dir, "parse_tool_call.json");
    for case in fx["cases"].as_array().unwrap() {
        let name = case["name"].as_str().unwrap();
        let text = case["text"].as_str().unwrap();
        let got = match responses::parse_tool_call(text) {
            Some(call) => json!({"name": call.name, "command": call.command}),
            None => Value::Null,
        };
        line(out, &format!("parse_tool_call.{name}"), &cj(&got));
    }
}

// ===== chimera relay item shaping ==========================================

fn emit_relay(dir: &Path, out: &mut String) {
    let fx = read_fixture(dir, "relay_items.json");
    for case in fx["function_call"].as_array().unwrap() {
        let name = case["name"].as_str().unwrap();
        let tool = case["tool"].as_str().unwrap();
        let call_id = case["call_id"].as_str().unwrap();
        let item = responses::function_call_item(tool, &case["arguments"], Some(call_id));
        line(out, &format!("relay.fc.{name}"), &cj(&item));
    }
    for case in fx["custom_tool_call"].as_array().unwrap() {
        let name = case["name"].as_str().unwrap();
        let tool = case["tool"].as_str().unwrap();
        let input = case["input"].as_str().unwrap();
        let call_id = case["call_id"].as_str().unwrap();
        let item = responses::custom_tool_call_item(tool, input, Some(call_id));
        line(out, &format!("relay.ctc.{name}"), &cj(&item));
    }
}

// ===== OAuth + JWT ==========================================================

fn emit_oauth(dir: &Path, out: &mut String) {
    let fx = read_fixture(dir, "oauth_jwt.json");
    let default_secret = fx["secret"].as_str().unwrap();

    // jwt_sign: build the compact payload in the fixture's claim order, sign,
    // and emit the full token (header.body.sig). Byte-comparable across sides.
    for case in fx["jwt_sign"].as_array().unwrap() {
        let name = case["name"].as_str().unwrap();
        let secret = case
            .get("secret_override")
            .and_then(Value::as_str)
            .unwrap_or(default_secret);
        let payload = build_payload(case);
        let token = difftest::sign_jwt(&payload, secret);
        line(out, &format!("oauth.jwt.{name}"), &cj(&json!(token)));
    }

    // pkce S256
    for case in fx["pkce_s256"].as_array().unwrap() {
        let name = case["name"].as_str().unwrap();
        let verifier = case["verifier"].as_str().unwrap();
        let challenge = difftest::pkce_s256_challenge(verifier);
        line(out, &format!("oauth.pkce.{name}"), &cj(&json!(challenge)));
    }

    // redirect allowlist
    for case in fx["redirect_allowlist"].as_array().unwrap() {
        let name = case["name"].as_str().unwrap();
        let got = difftest::is_valid_redirect_uri(
            case["uri"].as_str().unwrap(),
            case["base"].as_str().unwrap(),
        );
        line(out, &format!("oauth.redir.{name}"), &cj(&json!(got)));
    }

    // both-chatgpt lenient match
    for case in fx["both_chatgpt"].as_array().unwrap() {
        let name = case["name"].as_str().unwrap();
        let got =
            difftest::is_both_chatgpt(case["a"].as_str().unwrap(), case["b"].as_str().unwrap());
        line(out, &format!("oauth.both.{name}"), &cj(&json!(got)));
    }

    // validate_access_token (sign then validate under the same secret)
    for case in fx["validate_access_token"].as_array().unwrap() {
        let name = case["name"].as_str().unwrap();
        let payload = build_payload(case);
        let token = difftest::sign_jwt(&payload, default_secret);
        let got = difftest::validate_access_token(&token, default_secret);
        line(out, &format!("oauth.validate.{name}"), &cj(&json!(got)));
    }
}

/// Build a compact JSON object string from a case's `claims` in `claims_order`,
/// matching `JSON.stringify(payload)` for the object literal the TS builds (key
/// order preserved, numbers as integers, strings escaped).
fn build_payload(case: &Value) -> String {
    let order = case["claims_order"].as_array().unwrap();
    let claims = &case["claims"];
    let mut obj = serde_json::Map::new();
    for k in order {
        let key = k.as_str().unwrap();
        obj.insert(key.to_string(), claims[key].clone());
    }
    // serde_json with preserve_order keeps insertion order -> matches TS.
    serde_json::to_string(&Value::Object(obj)).expect("payload")
}

// ===== driver ===============================================================

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let area = args.get(1).map(String::as_str).unwrap_or("all");
    let dir: PathBuf = args
        .get(2)
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("../fixtures"));

    let mut out = String::new();
    match area {
        "v1delta" => emit_v1delta(&dir, &mut out),
        "sse" => emit_sse(&dir, &mut out),
        "parse_tool_call" => emit_parse_tool_call(&dir, &mut out),
        "relay" => emit_relay(&dir, &mut out),
        "oauth" => emit_oauth(&dir, &mut out),
        "all" => {
            emit_v1delta(&dir, &mut out);
            emit_sse(&dir, &mut out);
            emit_parse_tool_call(&dir, &mut out);
            emit_relay(&dir, &mut out);
            emit_oauth(&dir, &mut out);
        }
        other => {
            eprintln!("unknown area: {other}");
            std::process::exit(2);
        }
    }
    // Write raw bytes to stdout (no extra newline munging).
    use std::io::Write as _;
    std::io::stdout()
        .write_all(out.as_bytes())
        .expect("write stdout");
}
