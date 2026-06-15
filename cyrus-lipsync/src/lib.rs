//! cyrus-lipsync — the lipsync responses-shim harness.
//!
//! # What this is
//!
//! lipsync makes the real codex CLI render free chatgpt.com answers with ZERO
//! codex source changes (pure config). It is a "responses shim": a tiny HTTP
//! server that impersonates OpenAI's `POST /v1/responses` endpoint. codex is
//! pointed at it with a custom model provider, so codex believes it is talking to
//! a normal Responses backend. Behind the endpoint we drive a logged-in
//! chatgpt.com tab over the Chrome DevTools Protocol (CDP), tap that page's
//! WebSocket frames, reconstruct the answer tokens from ChatGPT's "v1" JSON-patch
//! delta encoding, and re-emit them as codex-shaped Responses SSE events.
//!
//! This layer is a thin translation between two streaming dialects:
//!   - [`tab_factory`] — a fresh tab has no service worker, so the turn streams
//!     inline through the `/f/conversation` SSE body (no CDP-invisible shared
//!     worker), plus the arm-before-navigate helper.
//!   - [`wstap`] — browser-level token tap (CDP `Network.*` + the inline-SSE fetch
//!     tee), via the `FETCH_WRAPPER` override that also forces
//!     `supports_buffering=false` and the model/effort axes.
//!   - [`v1delta`] — ChatGPT's v1 delta encoding -> ("token" | "thinking" |
//!     "turn_complete", str).
//!   - [`cdp`] — one page socket per tab so two threads' streams cannot cross-talk.
//!
//! # Wire contract codex actually enforces (verified against codex-rs)
//!
//! - codex POSTs to `base_url + "/responses"` with `Accept: text/event-stream`.
//! - It parses ONLY each frame's `data:` JSON and reads `type` from inside it (the
//!   `event:` line is cosmetic). Per turn we emit:
//!   ```text
//!   data: {"type":"response.created","response":{}}
//!   data: {"type":"response.output_item.added","item":{...message, content:[]...}}
//!   data: {"type":"response.output_text.delta","delta":"<tok>"}   (xN)
//!   data: {"type":"response.output_item.done","item":{...message...}}
//!   data: {"type":"response.completed","response":{"id":"resp_...","usage":{...}}}
//!   ```
//!   A missing `response.completed` is a hard stream error on codex's side. An
//!   `output_text.delta` with no active item makes codex's aggregator panic — the
//!   item must be ADDED first (content:[]), then deltas, then done.
//! - The request body is uncompressed for a non-"OpenAI" provider with no chatgpt
//!   auth (codex only zstd-compresses when `provider.is_openai()` AND chatgpt auth
//!   is present), so we parse plain JSON.
//! - These wire shapes are reverse-engineered and fragile: the v1delta decoding,
//!   the wstap frame handling, the `FETCH_WRAPPER` injection string, and the SSE
//!   event shapes must match exactly. The authoritative codex-rs references are
//!   codex-api/src/sse/responses.rs and protocol/src/models.rs (`ResponseItem`,
//!   `#[serde(tag="type", rename_all="snake_case")]`).
//!
//! # Tools (ReAct text bridge)
//!
//! The consumer chat does not emit codex-shaped function calls, so [`responses`]
//! bridges it with a text protocol: ChatGPT writes a command in a fenced ` ```run `
//! block, which we parse into a codex `function_call`. codex executes it in its
//! real sandbox, renders the native tool card, and relays the result back as the
//! next turn. A `conductor`-mode variant runs the tools through the real
//! repo-agent MCP connector instead (see [`conductor`] / [`subagent_mux`]).
//!
//! # Threads: per-thread-id conductors
//!
//! codex tags every Responses request with a `thread-id` header (the MAIN session
//! and each native subagent thread carry distinct ids). The shim's router
//! ([`responses`]'s `ShadowResponsesShim`) dispatches a request to the
//! [`conductor`]`::ThreadConductor` for its thread-id, lazily opening a tab the
//! first time a thread is seen. All conductors share ONE browser control socket
//! (the shim's [`tab_factory`]) but each opens its OWN page socket, so two threads'
//! token streams cannot cross-talk. [`provider`] is the single-tab driver used by
//! the pre-conductor path.

pub mod cli;
pub mod config;
pub mod responses;
pub mod cdp;
pub mod tab_factory;
pub mod wstap;
pub mod v1delta;
pub mod provider;
pub mod conductor;
pub mod subagent_mux;
pub mod runtime;

/// Crate-wide error type shared by every lipsync module.
///
/// Covers config resolution, the CDP transport, plain JSON parsing of codex's
/// uncompressed request body, and I/O. `Other` carries an `anyhow::Error` so
/// module internals can bubble up ad-hoc context without a bespoke variant.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("config error: {0}")]
    Config(String),
    #[error("cdp error: {0}")]
    Cdp(String),
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
    #[error(transparent)]
    Other(#[from] anyhow::Error),
}

pub type Result<T> = std::result::Result<T, Error>;
