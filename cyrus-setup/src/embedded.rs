//! Single-binary support: codex and cloudflared can be embedded into `cyrus.exe`
//! (zstd-compressed, see `build.rs`) and extracted to `~/.cyrus/bin` on first run.
//!
//! In a normal dev build nothing is embedded and every `embedded_*_path()`
//! returns `None`, so the callers fall back to disk resolution (override →
//! sibling → PATH). In a release single-binary build (`CYRUS_EMBED_*` set at
//! build time) the embedded copy is extracted once and preferred over PATH — so
//! a stray `npm` codex can never shadow the patched fork.

use std::path::PathBuf;

// --- embedded blobs (present only in single-binary release builds) ----------

#[cfg(embed_codex)]
fn codex_compressed() -> &'static [u8] {
    include_bytes!(concat!(env!("OUT_DIR"), "/codex.zst"))
}
#[cfg(embed_codex)]
fn codex_hash() -> &'static str {
    include_str!(concat!(env!("OUT_DIR"), "/codex.hash"))
}

#[cfg(embed_cloudflared)]
fn cloudflared_compressed() -> &'static [u8] {
    include_bytes!(concat!(env!("OUT_DIR"), "/cloudflared.zst"))
}
#[cfg(embed_cloudflared)]
fn cloudflared_hash() -> &'static str {
    include_str!(concat!(env!("OUT_DIR"), "/cloudflared.hash"))
}

/// True when this build embeds codex (the shipped single-binary).
pub fn has_embedded_codex() -> bool {
    cfg!(embed_codex)
}

// --- extraction -------------------------------------------------------------

/// `~/.cyrus/bin` (honors `CYRUS_HOME`) — where extracted binaries are cached.
#[cfg(any(embed_codex, embed_cloudflared))]
fn cache_dir() -> PathBuf {
    let home = std::env::var("CYRUS_HOME")
        .ok()
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| crate::home_dir().join(".cyrus"));
    home.join("bin")
}

#[cfg(any(embed_codex, embed_cloudflared))]
fn exe_file_name(name: &str, hash: &str) -> String {
    if cfg!(windows) {
        format!("{name}-{hash}.exe")
    } else {
        format!("{name}-{hash}")
    }
}

/// Materialize `compressed` to `~/.cyrus/bin/<name>-<hash>.exe` exactly once.
/// Reuses the file if it already exists (the hash keys the content, so a new
/// cyrus build lands a new path). Writes via a temp file + atomic rename so
/// concurrent invocations never observe a half-written executable.
#[cfg(any(embed_codex, embed_cloudflared))]
fn extract_once(name: &str, hash: &str, compressed: &[u8]) -> std::io::Result<PathBuf> {
    let dir = cache_dir();
    let final_path = dir.join(exe_file_name(name, hash));
    if final_path.exists() {
        return Ok(final_path);
    }
    std::fs::create_dir_all(&dir)?;
    let bytes = zstd::stream::decode_all(compressed)?;

    let tmp = dir.join(format!(".{name}-{hash}.{}.tmp", std::process::id()));
    {
        use std::io::Write;
        let mut f = std::fs::File::create(&tmp)?;
        f.write_all(&bytes)?;
        f.sync_all()?;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o755))?;
    }
    match std::fs::rename(&tmp, &final_path) {
        Ok(()) => Ok(final_path),
        // Lost a race: another invocation already extracted it. Use theirs.
        Err(_) if final_path.exists() => {
            let _ = std::fs::remove_file(&tmp);
            Ok(final_path)
        }
        Err(e) => {
            let _ = std::fs::remove_file(&tmp);
            Err(e)
        }
    }
}

/// Path to the embedded codex, extracted on first use. `None` when this build
/// embeds nothing (dev) — callers then fall back to disk resolution.
#[cfg(embed_codex)]
pub fn embedded_codex_path() -> Option<PathBuf> {
    extract_once("codex", codex_hash().trim(), codex_compressed())
        .map_err(|e| tracing::warn!("failed to extract embedded codex: {e}"))
        .ok()
}
#[cfg(not(embed_codex))]
pub fn embedded_codex_path() -> Option<PathBuf> {
    None
}

/// Path to the embedded cloudflared, extracted on first use. `None` when this
/// build embeds nothing.
#[cfg(embed_cloudflared)]
pub fn embedded_cloudflared_path() -> Option<PathBuf> {
    extract_once("cloudflared", cloudflared_hash().trim(), cloudflared_compressed())
        .map_err(|e| tracing::warn!("failed to extract embedded cloudflared: {e}"))
        .ok()
}
#[cfg(not(embed_cloudflared))]
pub fn embedded_cloudflared_path() -> Option<PathBuf> {
    None
}
