//! Command-line entry for the lipsync responses-shim harness.
//!
//! Mirrors `responses_shim.py::main` / `_amain`: parse argv, load ShadowConfig
//! from env (applying the `--cdp-port` override), build the ShadowResponsesShim,
//! optionally eager-boot one MAIN chatgpt.com tab, and serve the
//! `/v1/responses` (+ `/responses`, `/health`, `/control/toolcall`,
//! `/control/turn_complete`) HTTP app that codex points at via its custom model
//! provider (OPENAI_BASE_URL).
//!
//! This lives in the library (not `main.rs`) so the single `cyrus` busybox
//! binary can run the shim as a hidden `cyrus lipsync` subcommand by calling
//! [`run_cli`] directly — the standalone `cyrus-lipsync` binary is just a thin
//! shim over the same function. Args are passed in explicitly (the busybox
//! dispatcher strips the leading `lipsync` token before calling).
//!
//! Source: idare/shadow/responses_shim.py (private original)
//!         (main / _amain — the argparse front door + asyncio.run boot)
//!
//! ## subagent_mux decision
//!
//! responses_shim.py runs the off-codex subagent multiplexer as a SEPARATE
//! process: `python -m idare.shadow.subagent_mux` (its own `main()` in
//! subagent_mux.py: `ShadowConfig.from_env()` then `SubagentMux(cfg).start()`).
//!
//! We expose it here as a **`subagent-mux` subcommand** rather than a second
//! entry point. `cyrus-lipsync subagent-mux` (or `cyrus lipsync subagent-mux`)
//! is the direct analogue of `python -m idare.shadow.subagent_mux`. The default
//! (no subcommand) path is the responses shim, exactly like running the module
//! bare.

use std::process::ExitCode;

use crate::config::ShadowConfig;
use crate::responses;
use crate::subagent_mux;

/// Parsed CLI for the shim front door. Mirrors `responses_shim.py::main`'s
/// argparse: --host, --port, --model, --effort, --cdp-port, --lazy.
#[derive(Debug, Clone)]
struct Args {
    host: String,
    port: u16,
    /// model slug or friendly spec (default resolved downstream to "gpt-5-5-thinking").
    model: Option<String>,
    /// thinking effort: min/standard/extended/max.
    effort: Option<String>,
    /// override CDP port (default 9222 / env CDP_PORT).
    cdp_port: Option<u16>,
    /// defer tab boot until the first request.
    lazy: bool,
}

impl Default for Args {
    fn default() -> Self {
        // Defaults copied verbatim from responses_shim.py's add_argument calls.
        Self {
            host: "127.0.0.1".to_string(),
            port: 8765,
            model: None,
            effort: None,
            cdp_port: None,
            lazy: false,
        }
    }
}

/// Which entry to run. `python -m idare.shadow.responses_shim` (default) vs
/// `python -m idare.shadow.subagent_mux` (the `subagent-mux` subcommand).
enum Command {
    /// Run the responses shim (the default, with parsed CLI args).
    Shim(Args),
    /// Run the off-codex subagent multiplexer (no per-CLI args, like its own main()).
    SubagentMux,
    /// `--help` / `-h`: print usage and exit 0.
    Help,
    /// A parse error: message already printed; exit non-zero.
    ParseError(String),
}

const USAGE: &str = "\
Responses-API shim over the chatgpt.com shadow

Usage:
  cyrus-lipsync [OPTIONS]            Run the /v1/responses shim (default)
  cyrus-lipsync subagent-mux        Run the off-codex subagent multiplexer

Options:
  --host <HOST>        bind host (default 127.0.0.1)
  --port <PORT>        bind port (default 8765)
  --model <SPEC>       model slug or friendly spec (default gpt-5-5-thinking)
  --effort <EFFORT>    thinking effort: min/standard/extended/max
  --cdp-port <PORT>    override CDP port (default 9222 / env CDP_PORT)
  --lazy               defer tab boot until the first request
  -h, --help           print this help and exit
";

/// Parse argv into a [`Command`]. Manual parsing (no clap dependency declared in
/// the scaffold) that accepts exactly the flags responses_shim.py's argparse did,
/// in both `--flag value` and `--flag=value` forms.
fn parse_args<I: IntoIterator<Item = String>>(argv: I) -> Command {
    let mut iter = argv.into_iter().peekable();

    // First positional (if any) selects the subcommand, mirroring `python -m`.
    if let Some(first) = iter.peek() {
        match first.as_str() {
            "subagent-mux" | "subagent_mux" => return Command::SubagentMux,
            "-h" | "--help" => return Command::Help,
            _ => {}
        }
    }

    let mut args = Args::default();

    // Helper: pull the value for a flag, supporting both `--flag value` and an
    // already-split `--flag=value` (handled before the dispatch below).
    fn need_value(
        iter: &mut std::iter::Peekable<impl Iterator<Item = String>>,
        flag: &str,
    ) -> Result<String, String> {
        match iter.next() {
            Some(v) => Ok(v),
            None => Err(format!("argument {flag}: expected one argument")),
        }
    }

    while let Some(tok) = iter.next() {
        // Split `--flag=value` into (flag, Some(value)).
        let (flag, inline) = match tok.split_once('=') {
            Some((f, v)) if f.starts_with("--") => (f.to_string(), Some(v.to_string())),
            _ => (tok.clone(), None),
        };

        macro_rules! value_for {
            ($flag:expr) => {
                match inline {
                    Some(v) => v,
                    None => match need_value(&mut iter, $flag) {
                        Ok(v) => v,
                        Err(e) => return Command::ParseError(e),
                    },
                }
            };
        }

        match flag.as_str() {
            "-h" | "--help" => return Command::Help,
            "--host" => args.host = value_for!("--host"),
            "--port" => {
                let raw = value_for!("--port");
                match raw.parse::<u16>() {
                    Ok(p) => args.port = p,
                    Err(_) => {
                        return Command::ParseError(format!(
                            "argument --port: invalid int value: '{raw}'"
                        ))
                    }
                }
            }
            "--model" => args.model = Some(value_for!("--model")),
            "--effort" => args.effort = Some(value_for!("--effort")),
            "--cdp-port" => {
                let raw = value_for!("--cdp-port");
                match raw.parse::<u16>() {
                    Ok(p) => args.cdp_port = Some(p),
                    Err(_) => {
                        return Command::ParseError(format!(
                            "argument --cdp-port: invalid int value: '{raw}'"
                        ))
                    }
                }
            }
            "--lazy" => {
                // store_true: no value. Reject an inline `--lazy=...`.
                if inline.is_some() {
                    return Command::ParseError("argument --lazy: takes no value".to_string());
                }
                args.lazy = true;
            }
            other => return Command::ParseError(format!("unrecognized argument: {other}")),
        }
    }

    Command::Shim(args)
}

/// Run the shim (or the subagent-mux subcommand) to completion. Must be called
/// from within a tokio runtime. `argv` is the argument list **after** the
/// program name (and, in busybox mode, after the `lipsync` subcommand token).
pub async fn run_cli<I: IntoIterator<Item = String>>(argv: I) -> ExitCode {
    init_tracing();

    match parse_args(argv) {
        Command::Help => {
            print!("{USAGE}");
            ExitCode::SUCCESS
        }
        Command::ParseError(msg) => {
            eprintln!("cyrus-lipsync: error: {msg}");
            eprint!("{USAGE}");
            // argparse exits 2 on a usage error.
            ExitCode::from(2)
        }
        Command::SubagentMux => match run_subagent_mux().await {
            Ok(()) => ExitCode::SUCCESS,
            Err(e) => {
                eprintln!("[mux] fatal: {e:#}");
                ExitCode::FAILURE
            }
        },
        Command::Shim(args) => match run_shim(args).await {
            Ok(()) => ExitCode::SUCCESS,
            Err(e) => {
                eprintln!("[shim] fatal: {e:#}");
                ExitCode::FAILURE
            }
        },
    }
}

/// Initialize logging. The Python used bare `print(..., flush=True)`; the port
/// routes the same operator-facing lines through `tracing`. Install a fmt layer
/// honoring `RUST_LOG`, defaulting to `info` so the `[shim] ...` boot/bind lines
/// (the Python's prints) are visible out of the box.
fn init_tracing() {
    use tracing_subscriber::EnvFilter;
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    // try_init: don't panic if a test harness or embedder already installed one.
    let _ = tracing_subscriber::fmt().with_env_filter(filter).try_init();
}

/// `_amain` for the shim: build config (with the --cdp-port override), construct
/// the shim, eager-boot a MAIN tab unless --lazy, then serve until killed.
///
/// Mirrors responses_shim.py::_amain. The shim construction + eager boot +
/// `web.TCPSite(... host, port).start()` + `await asyncio.Event().wait()` all live
/// behind `responses::serve`, which owns the ShadowResponsesShim lifecycle (build
/// + boot + bind to host:port + run + shutdown), the analogue of build_app +
/// AppRunner + TCPSite. We hand it the parsed knobs so it can faithfully reproduce
/// `ShadowResponsesShim(cfg, model=args.model, effort=args.effort)` and the
/// eager-vs-lazy boot branch.
async fn run_shim(args: Args) -> anyhow::Result<()> {
    let mut cfg = ShadowConfig::from_env();

    // responses_shim.py::_amain:  if args.cdp_port: cfg.cdp_port = args.cdp_port
    if let Some(p) = args.cdp_port {
        cfg.cdp_port = p;
    }

    // Hand off to the shim runtime. See the note in `responses::serve` about the
    // extended signature this entry expects (host/port/model/effort/lazy).
    responses::serve(
        cfg,
        responses::ServeOptions {
            host: args.host,
            port: args.port,
            model: args.model,
            effort: args.effort,
            lazy: args.lazy,
        },
    )
    .await
}

/// `subagent_mux.py::main`: `ShadowConfig.from_env()` then
/// `SubagentMux(cfg).start()`, run to completion (Ctrl-C ends it).
async fn run_subagent_mux() -> anyhow::Result<()> {
    let cfg = ShadowConfig::from_env();
    subagent_mux::SubagentMux::new(cfg).start().await
}
