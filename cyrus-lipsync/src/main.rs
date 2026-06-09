//! cyrus-lipsync standalone binary — a thin shim over [`cyrus_lipsync::cli`].
//!
//! The full entry logic (argparse, tracing init, shim/subagent-mux dispatch)
//! lives in the library's `cli` module so the single `cyrus` busybox binary can
//! run the same shim as `cyrus lipsync ...`. This binary remains for
//! development and for anyone who wants to run lipsync directly.

use std::process::ExitCode;

#[tokio::main]
async fn main() -> ExitCode {
    // Skip argv[0] (the program name), like argparse.
    cyrus_lipsync::cli::run_cli(std::env::args().skip(1)).await
}
