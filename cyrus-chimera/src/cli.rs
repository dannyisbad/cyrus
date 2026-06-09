//! Command-line entry for the chimera server.
//!
//! Direct port of `index.ts` (the Node entry module). Responsibilities, in the
//! same order as the original:
//!   1. Install SIGINT / SIGTERM handlers that kill all background processes
//!      (`killAllBackground`) and then exit with code 0.
//!   2. Decide the transport with `wantsHttp()` — HTTP is the default; `--stdio`
//!      (without `--http`) selects the stdio MCP loop.
//!   3. Run the chosen server; the error is returned to the caller, which logs
//!      it and exits with code 1.
//!
//! This lives in the library (not `main.rs`) so the single `cyrus` busybox
//! binary can run the server as a hidden `cyrus chimera` subcommand by calling
//! [`run_cli`] directly — the standalone `cyrus-chimera` binary is just a thin
//! shim over the same function. Both read flags off `std::env::args()` via
//! [`crate::config`], whose lookups are flag-based, so an extra leading
//! `chimera` positional (present in busybox mode) is ignored.
//!
//! Source: repo-agent-mcp/src/index.ts (private original)
//!
//! ## Shutdown hazard (preserved from the TS)
//!
//! index.ts registers, for both SIGINT and SIGTERM:
//!
//! ```js
//! process.on(sig, () => { killAllBackground(); process.exit(0); });
//! ```
//!
//! i.e. every spawned background child (the `core/bg.ts` REGISTRY) MUST be
//! tree-killed *before* the process exits — otherwise detached children (own
//! process group on POSIX, `taskkill /T` on Windows) are orphaned. We reproduce
//! that here: on either signal we call `kill_all_background()` and exit(0).

use crate::config;
use crate::http;

const USAGE: &str = "\
repo-agent MCP + tool-relay + OAuth server (cyrus-chimera)

Usage:
  cyrus-chimera [OPTIONS]    Run the HTTP server (default)
  cyrus-chimera --stdio      Run the stdio MCP loop instead

Options:
  --http               force the HTTP transport (default unless --stdio)
  --stdio              run the stdio MCP loop (ignored if --http also given)
  --repo <PATH>        repo root (default REPO_AGENT_ROOT or cwd)
  --config <PATH>      config file (default REPO_AGENT_CONFIG or <root>/repo-agent.config.json)
  --host <HOST>        bind host (default 127.0.0.1, HTTP only)
  --port <PORT>        bind port (default 8787, HTTP only)
  -h, --help           print this help and exit

Environment:
  REPO_AGENT_ROOT, REPO_AGENT_CONFIG, REPO_AGENT_HOME_ROOT,
  REPO_AGENT_TOKEN, REPO_AGENT_PUBLIC_URL
";

/// Run the chimera server to completion. Must be called from within a tokio
/// runtime (it spawns the signal-handler task). Returns the server's result;
/// `--help` prints usage and returns `Ok(())` without starting a server.
pub async fn run_cli() -> anyhow::Result<()> {
    // `-h` / `--help`: print usage and return before any server work. (The
    // original index.ts has no help text — this is a port-side affordance only;
    // it changes nothing when the flag is absent.)
    if std::env::args().skip(1).any(|a| a == "-h" || a == "--help") {
        print!("{USAGE}");
        return Ok(());
    }

    // index.ts logs via plain `console.log` / `console.error` (stdout / stderr);
    // the startup println!s are kept for parity. But the tool dispatcher and the
    // scan/walk paths log through `tracing`, so a subscriber must actually be
    // installed — without one every tracing macro is a no-op and the redirected
    // chimera.log stays 0 bytes (the incident). Write to STDERR: Rust's stderr is
    // unbuffered, and cyrus-setup's stack.rs dups stderr into
    // ~/.cyrus/logs/chimera.log, so events land in the file immediately.
    init_tracing();

    // Mirror index.ts top-level: install the signal handlers up front, before
    // any transport starts, so a Ctrl-C during startup still tears down cleanly.
    install_signal_handlers();

    // `loadConfig()` — merge DEFAULTS < repo-agent.config.json < env < argv.
    let cfg = config::load_config();

    // `wantsHttp()` — `--http` present OR `--stdio` absent.
    if config::wants_http() {
        http::run_http(cfg).await
    } else {
        http::run_stdio(cfg).await
    }
}

/// Install the global tracing subscriber: fmt layer, `RUST_LOG` env filter
/// (default "info"), no ANSI (the output is a redirected log file, not a TTY),
/// stderr writer (unbuffered; dup'd into chimera.log by cyrus-setup). Idempotent
/// in spirit: `try_init` so an embedding caller that already installed a
/// subscriber (busybox mode, tests) wins without a panic.
fn init_tracing() {
    use tracing_subscriber::EnvFilter;

    let _ = tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .with_ansi(false)
        .with_writer(std::io::stderr)
        .try_init();
}

/// Spawn a background task that waits on the OS termination signals and, on the
/// first one, kills all background children and exits the process with code 0.
///
/// Reproduces:
/// ```js
/// for (const sig of ["SIGINT", "SIGTERM"] as const) {
///   process.on(sig, () => { killAllBackground(); process.exit(0); });
/// }
/// ```
///
/// On Windows there is no SIGTERM in the POSIX sense; the closest equivalents
/// tokio exposes are Ctrl-C, Ctrl-Break, and the console-shutdown event, so we
/// listen on those. On Unix we listen on SIGINT + SIGTERM exactly as the TS does.
fn install_signal_handlers() {
    tokio::spawn(async move {
        wait_for_terminate_signal().await;
        // Kill all background processes first, then exit(0) — order matters.
        kill_all_background();
        std::process::exit(0);
    });
}

#[cfg(unix)]
async fn wait_for_terminate_signal() {
    use tokio::signal::unix::{signal, SignalKind};

    // If either stream fails to install we fall back to just awaiting Ctrl-C so
    // the process is still interruptible (the TS has no such failure mode, but
    // registering a Unix signal handler can fail and we must not panic startup).
    let mut sigint = match signal(SignalKind::interrupt()) {
        Ok(s) => s,
        Err(_) => {
            let _ = tokio::signal::ctrl_c().await;
            return;
        }
    };
    let mut sigterm = match signal(SignalKind::terminate()) {
        Ok(s) => s,
        Err(_) => {
            sigint.recv().await;
            return;
        }
    };

    tokio::select! {
        _ = sigint.recv() => {}
        _ = sigterm.recv() => {}
    }
}

#[cfg(windows)]
async fn wait_for_terminate_signal() {
    use tokio::signal::windows::{ctrl_break, ctrl_c, ctrl_shutdown};

    // ctrl_c maps to SIGINT; ctrl_break / ctrl_shutdown stand in for SIGTERM-like
    // termination on Windows. Any failure to register falls back to ctrl_c.
    let mut c = match ctrl_c() {
        Ok(s) => s,
        Err(_) => {
            let _ = tokio::signal::ctrl_c().await;
            return;
        }
    };
    let mut brk = ctrl_break().ok();
    let mut shutdown = ctrl_shutdown().ok();

    match (brk.as_mut(), shutdown.as_mut()) {
        (Some(brk), Some(shutdown)) => {
            tokio::select! {
                _ = c.recv() => {}
                _ = brk.recv() => {}
                _ = shutdown.recv() => {}
            }
        }
        (Some(brk), None) => {
            tokio::select! {
                _ = c.recv() => {}
                _ = brk.recv() => {}
            }
        }
        (None, Some(shutdown)) => {
            tokio::select! {
                _ = c.recv() => {}
                _ = shutdown.recv() => {}
            }
        }
        (None, None) => {
            c.recv().await;
        }
    }
}

/// `killAllBackground()` from `core/bg.ts`: tree-kill every still-running
/// background child in the registry, marking each "killed".
///
/// `core::bg`'s registry is ported into `tools` (the background-process surface
/// backing repo_shell({background:true}) / repo_bg_*). Drain it on shutdown.
fn kill_all_background() {
    crate::tools::kill_all_background();
}
