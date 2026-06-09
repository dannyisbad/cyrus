//! cyrus-chimera — Rust port of the chimera Node/TypeScript service.
//!
//! Original: repo-agent-mcp/src (private original)
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
//! ## Crate layout
//!
//! This file is the crate root. It declares one module per logical area and
//! defines the crate-wide [`Error`] / [`Result`] types. The server entry point
//! itself (the `runHttp` / `runStdio` orchestration from `index.ts`) lives in
//! [`http`] + the binary's `main.rs`, not here — the root only wires the
//! modules together and owns the shared error vocabulary.
//!
//! The modules mirror the TS source tree:
//!
//! | module       | TS source                              |
//! |--------------|----------------------------------------|
//! | [`config`]   | `config.ts` (+ `types.ts`)             |
//! | [`http`]     | `index.ts` (`runHttp` / `runStdio`)    |
//! | [`oauth`]    | `oauth.ts`                             |
//! | [`mcp`]      | `index.ts` (`createRepoMcpServer`)     |
//! | [`tools`]    | `tools/`                               |
//! | [`state`]    | `state/`                               |
//! | [`subagent`] | `state/` (subagent jobs + leases)      |

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
/// The TypeScript service never threw typed errors across the wire — every
/// failure surfaced as a loosely-typed JSON object: the tool layer returned
/// `errReply(message)` → `{ ok: false, error: <message>, ...extra }`, and the
/// control plane in `index.ts` returned the same shape inline
/// (`{ ok: false, error: "agent_id and capsule required" }`). This enum is the
/// Rust home for those messages: each variant carries the human-readable string
/// that the TS put in the `error` field, so it can be rendered straight back
/// into the original wire shape via [`Error::to_error_json`].
///
/// Hazard (per the port brief): **keep these variants additive.** Modules are
/// filled in one at a time; adding a variant must never force a churn on the
/// modules already ported. Prefer a new message-bearing variant over reshaping
/// an existing one, and avoid `#[non_exhaustive]`-breaking field changes.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// Configuration load/merge/validation failure (see [`config`]).
    #[error("config error: {0}")]
    Config(String),

    /// OAuth / OIDC authorization-server failure (see [`oauth`]).
    #[error("oauth error: {0}")]
    OAuth(String),

    /// A tool (`repo_*`) failed. Mirrors `errReply(message)` from `result.ts`:
    /// the string is exactly what the TS placed in the `error` field.
    #[error("{0}")]
    Tool(String),

    /// A loopback `/control/*` request failed. Mirrors the inline
    /// `{ ok: false, error: ... }` responses in `index.ts`'s control plane.
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

    /// Escape hatch for errors that don't (yet) have a typed home. Keeps the
    /// variant set additive while modules are still landing.
    #[error(transparent)]
    Other(#[from] anyhow::Error),
}

impl Error {
    /// Render this error as the loosely-typed JSON the TS returned on failure:
    /// `{ "ok": false, "error": "<message>" }`.
    ///
    /// This is the Rust counterpart to `errReply` / the control-plane's inline
    /// `sendJson(res, status, { ok: false, error })`. The `error` string is the
    /// `Display` of this error, which for the message-bearing variants
    /// ([`Error::Tool`], [`Error::Control`]) is byte-for-byte the original TS
    /// message.
    pub fn to_error_json(&self) -> serde_json::Value {
        serde_json::json!({ "ok": false, "error": self.to_string() })
    }
}

/// Crate-wide `Result` alias over [`enum@Error`].
pub type Result<T> = std::result::Result<T, Error>;
