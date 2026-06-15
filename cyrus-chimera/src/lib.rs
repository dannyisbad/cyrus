//! cyrus-chimera — the chimera MCP service.
//!
//! chimera is an MCP server that exposes a repository to ChatGPT (over a
//! Streamable-HTTP MCP transport) as a suite of `repo_*` tools, plus:
//!   - an OAuth 2.0 / OIDC authorization server (so ChatGPT's connector can
//!     bearer-auth against the local server through a cloudflared tunnel),
//!   - a loopback-only control plane that the lipsync harness drives to bind
//!     ChatGPT sessions to subagent ids and hand back subagent results,
//!   - a durable, append-only event/state store (events.jsonl + state.json),
//!   - a path-lease coordinator for parallel subagents.
//!
//! This file is the crate root: it declares one module per logical area and
//! defines the crate-wide [`Error`] / [`Result`] types. The server entry point
//! (the `run_http` / `run_stdio` orchestration) lives in [`http`] + the binary's
//! `main.rs`, not here — the root only wires the modules together and owns the
//! shared error vocabulary.

pub mod cli;
pub mod config;
pub mod http;
pub mod oauth;
pub mod mcp;
pub mod tools;
pub mod state;
pub mod subagent;
pub mod register;
pub mod wire;

/// Crate-wide error type.
///
/// Failures surface across the wire as a loosely-typed JSON object: the tool
/// layer returns `{ ok: false, error: <message>, ...extra }`, and the control
/// plane returns the same shape inline. Each variant carries the human-readable
/// string placed in the `error` field, rendered back via [`Error::to_error_json`].
///
/// Keep these variants additive: prefer a new message-bearing variant over
/// reshaping an existing one.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// Configuration load/merge/validation failure (see [`config`]).
    #[error("config error: {0}")]
    Config(String),

    /// OAuth / OIDC authorization-server failure (see [`oauth`]).
    #[error("oauth error: {0}")]
    OAuth(String),

    /// A tool (`repo_*`) failed. The string is the `error`-field message.
    #[error("{0}")]
    Tool(String),

    /// A loopback `/control/*` request failed (the inline
    /// `{ ok: false, error: ... }` control-plane responses).
    #[error("{0}")]
    Control(String),

    /// MCP transport / JSON-RPC failure (see [`mcp`]).
    #[error("mcp error: {0}")]
    Mcp(String),

    /// State-store failure: events.jsonl / state.json read/write, leases, etc.
    /// (see [`state`] and [`subagent`]).
    #[error("state error: {0}")]
    State(String),

    /// Underlying I/O error (filesystem, sockets).
    #[error(transparent)]
    Io(#[from] std::io::Error),

    /// JSON (de)serialization error.
    #[error(transparent)]
    Json(#[from] serde_json::Error),

    /// Escape hatch for errors that don't have a typed home.
    #[error(transparent)]
    Other(#[from] anyhow::Error),
}

impl Error {
    /// Render this error as the loosely-typed failure JSON:
    /// `{ "ok": false, "error": "<message>" }`. The `error` string is the
    /// `Display` of this error.
    pub fn to_error_json(&self) -> serde_json::Value {
        serde_json::json!({ "ok": false, "error": self.to_string() })
    }
}

/// Crate-wide `Result` alias over [`enum@Error`].
pub type Result<T> = std::result::Result<T, Error>;
