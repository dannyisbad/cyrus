//! Differential integration test.
//!
//! For each area, this runs BOTH the original (python `idare/shadow` and/or node
//! `repo-agent-mcp`) and the Rust port (the `cyrus-diff-emit` binary), then
//! byte-diffs the two canonical reports. A mismatch fails the test and prints the
//! first differing lines.
//!
//! Run:  cargo test -p cyrus-differential
//!
//! Areas + which original owns them:
//!   v1delta | sse | parse_tool_call | relay  -> python (idare/shadow)
//!   oauth                                     -> node   (repo-agent-mcp)
//!
//! The originals are NOT part of this repo. To run the vs-original halves, set
//!   CYRUS_SHADOW_PY_ROOT = dir containing the original `idare` python package
//!   CYRUS_OAUTH_TS       = path to the original repo-agent-mcp src/oauth.ts
//! If an interpreter is missing or an env var is unset, the corresponding case
//! is reported as SKIPPED (not failed) so the suite passes in any environment;
//! the runnable script (run_differential.ps1 / .sh) prints the same notices.

use std::path::Path;
use std::path::PathBuf;
use std::process::Command;

/// Absolute path to this crate's directory (tests/differential).
fn crate_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

/// The shared fixtures dir: ../../tests/fixtures relative to this crate, i.e.
/// cyrus/tests/fixtures.
fn fixtures_dir() -> PathBuf {
    crate_dir().join("..").join("fixtures")
}

/// Path to the freshly built `cyrus-diff-emit` binary. Cargo exposes it to the
/// integration test via the CARGO_BIN_EXE_<name> env var.
fn emit_bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_cyrus-diff-emit"))
}

/// Run the Rust port emitter for `area` and capture stdout bytes.
fn rust_emit(area: &str) -> Vec<u8> {
    let out = Command::new(emit_bin())
        .arg(area)
        .arg(fixtures_dir())
        .output()
        .unwrap_or_else(|e| panic!("spawn cyrus-diff-emit: {e}"));
    assert!(
        out.status.success(),
        "cyrus-diff-emit {area} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    out.stdout
}

/// Run a driver command (python/node) and capture stdout. Returns None — with
/// a SKIP notice — if the interpreter is missing, or if the driver exits 86
/// (its "original source tree not configured" sentinel: the originals live
/// outside this repo and are located via CYRUS_SHADOW_PY_ROOT / CYRUS_OAUTH_TS).
fn driver_emit(program: &str, script: &Path, area: &str) -> Option<Vec<u8>> {
    let out = Command::new(program)
        .arg(script)
        .arg(area)
        .arg(fixtures_dir())
        .output();
    match out {
        Ok(o) => {
            if o.status.code() == Some(86) {
                eprintln!(
                    "SKIP   {area:<16} ({})",
                    String::from_utf8_lossy(&o.stderr).trim()
                );
                return None;
            }
            if !o.status.success() {
                panic!(
                    "{program} {} {area} failed (exit {:?}):\n{}",
                    script.display(),
                    o.status.code(),
                    String::from_utf8_lossy(&o.stderr)
                );
            }
            Some(o.stdout)
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            eprintln!("SKIP   {area:<16} ({program} not found)");
            None
        }
        Err(e) => panic!("spawn {program}: {e}"),
    }
}

fn python_driver() -> PathBuf {
    crate_dir().join("drivers").join("emit_python.py")
}

fn node_driver() -> PathBuf {
    crate_dir().join("drivers").join("emit_node.mjs")
}

/// Find the first differing line (1-based) between two byte reports, for a
/// helpful failure message.
fn first_diff(a: &[u8], b: &[u8]) -> Option<(usize, String, String)> {
    let sa = String::from_utf8_lossy(a);
    let sb = String::from_utf8_lossy(b);
    let mut la = sa.lines();
    let mut lb = sb.lines();
    let mut n = 0usize;
    loop {
        n += 1;
        match (la.next(), lb.next()) {
            (Some(x), Some(y)) if x == y => continue,
            (None, None) => return None,
            (x, y) => {
                return Some((
                    n,
                    x.unwrap_or("<EOF>").to_string(),
                    y.unwrap_or("<EOF>").to_string(),
                ));
            }
        }
    }
}

/// Compare a Rust report against an original report for `area`. `rust` is the
/// port; `orig` is the original (python/node). Asserts byte equality.
fn assert_match(area: &str, rust: &[u8], orig: &[u8]) {
    if rust == orig {
        eprintln!(
            "MATCH  {area:<16} ({} bytes, {} lines)",
            rust.len(),
            String::from_utf8_lossy(rust).lines().count()
        );
        return;
    }
    let detail = match first_diff(rust, orig) {
        Some((n, r, o)) => format!(
            "\n  first diff at line {n}:\n    rust: {r}\n    orig: {o}"
        ),
        None => " (length differs only)".to_string(),
    };
    panic!("DIFFER {area}: Rust port output != original output{detail}");
}

// ---- python-owned areas ----------------------------------------------------

#[test]
fn diff_v1delta_vs_python() {
    let rust = rust_emit("v1delta");
    if let Some(orig) = driver_emit("python", &python_driver(), "v1delta") {
        assert_match("v1delta", &rust, &orig);
    }
}

#[test]
fn diff_sse_vs_python() {
    let rust = rust_emit("sse");
    if let Some(orig) = driver_emit("python", &python_driver(), "sse") {
        assert_match("sse", &rust, &orig);
    }
}

#[test]
fn diff_parse_tool_call_vs_python() {
    let rust = rust_emit("parse_tool_call");
    if let Some(orig) = driver_emit("python", &python_driver(), "parse_tool_call") {
        assert_match("parse_tool_call", &rust, &orig);
    }
}

#[test]
fn diff_relay_vs_python() {
    let rust = rust_emit("relay");
    if let Some(orig) = driver_emit("python", &python_driver(), "relay") {
        assert_match("relay", &rust, &orig);
    }
}

// ---- node-owned area -------------------------------------------------------

#[test]
fn diff_oauth_vs_node() {
    let rust = rust_emit("oauth");
    if let Some(orig) = driver_emit("node", &node_driver(), "oauth") {
        assert_match("oauth", &rust, &orig);
    }
}
