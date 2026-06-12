//! Embeds external binaries into the `cyrus` single-file build.
//!
//! When `CYRUS_EMBED_CODEX` (and/or `CYRUS_EMBED_CLOUDFLARED`) points at a real
//! executable at build time, this script zstd-compresses it into `OUT_DIR` and
//! records a short content hash, then sets a `cfg` flag the runtime uses to
//! `include_bytes!` the blob. When the env vars are unset (the normal dev build)
//! nothing is embedded and `cyrus.exe` stays small — it then resolves codex /
//! cloudflared from disk (override → sibling → PATH) as before.
//!
//! Release packaging (`scripts/release.ps1`) builds the pinned-fork codex, then
//! builds `cyrus` with these env vars set, producing one self-contained binary.

use std::io::Write;
use std::path::{Path, PathBuf};

fn main() {
    println!("cargo:rerun-if-env-changed=CYRUS_EMBED_CODEX");
    println!("cargo:rerun-if-env-changed=CYRUS_EMBED_CLOUDFLARED");
    let out_dir = PathBuf::from(std::env::var("OUT_DIR").expect("OUT_DIR"));

    embed("CYRUS_EMBED_CODEX", "codex", &out_dir, "embed_codex");
    embed(
        "CYRUS_EMBED_CLOUDFLARED",
        "cloudflared",
        &out_dir,
        "embed_cloudflared",
    );

    // Declare the cfgs so `#[cfg(embed_codex)]` never warns as unexpected.
    println!("cargo:rustc-check-cfg=cfg(embed_codex)");
    println!("cargo:rustc-check-cfg=cfg(embed_cloudflared)");
}

/// If `env_var` names a readable file, compress it to `<out>/<name>.zst`, write
/// its content hash to `<out>/<name>.hash`, and enable `cfg(<cfg_flag>)`.
fn embed(env_var: &str, name: &str, out_dir: &Path, cfg_flag: &str) {
    let Some(src) = std::env::var_os(env_var).map(PathBuf::from).filter(|p| !p.as_os_str().is_empty())
    else {
        return;
    };
    println!("cargo:rerun-if-changed={}", src.display());
    let bytes = std::fs::read(&src)
        .unwrap_or_else(|e| panic!("{env_var}={} could not be read: {e}", src.display()));

    // Short, stable content hash (FNV-1a, no extra dep) — keys the runtime cache
    // so a new cyrus build re-extracts the matching binary.
    let hash = fnv1a_hex(&bytes);
    std::fs::write(out_dir.join(format!("{name}.hash")), &hash).expect("write hash");

    // zstd level: good ratio without an absurd build time. Tunable via
    // CYRUS_EMBED_ZSTD_LEVEL (release packaging can bump it for a smaller exe).
    let level: i32 = std::env::var("CYRUS_EMBED_ZSTD_LEVEL")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(9);
    println!("cargo:rerun-if-env-changed=CYRUS_EMBED_ZSTD_LEVEL");
    let compressed = zstd::stream::encode_all(bytes.as_slice(), level).expect("zstd compress");
    let mut f = std::fs::File::create(out_dir.join(format!("{name}.zst"))).expect("create blob");
    f.write_all(&compressed).expect("write blob");

    println!("cargo:rustc-cfg={cfg_flag}");
}

fn fnv1a_hex(bytes: &[u8]) -> String {
    let mut h: u64 = 0xcbf29ce484222325;
    for &b in bytes {
        h ^= b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    format!("{h:016x}")
}
