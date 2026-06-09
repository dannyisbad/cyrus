//! Per-install secrets: the chimera bearer token (also the OAuth consent
//! secret) and an optional distinct JWT signing key. Stored at
//! `~/.cyrus/secrets.json`, created on first run.
//!
//! SECURITY: the bearer token gates the public `/mcp` endpoint and loopback
//! `/control/arm-consent`; the signing key signs issued OAuth JWTs. Neither
//! ever enters the browser page (the page only sees one-time nonces).

use std::fs;
use std::path::PathBuf;

use anyhow::Context;
use rand::RngCore;
use serde::{Deserialize, Serialize};

use crate::SetupOptions;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Secrets {
    pub bearer_token: String,
    /// Distinct JWT signing key (CONNECTOR_API_FLOW: "cyrus should generate a
    /// separate key per install"). Absent => chimera falls back to the bearer.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub jwt_signing_key: Option<String>,
    /// True when this call created the file (telemetry for the reuse flag).
    #[serde(skip)]
    pub created: bool,
}

fn secrets_path(opts: &SetupOptions) -> PathBuf {
    opts.cyrus_home().join("secrets.json")
}

fn random_hex(bytes: usize) -> String {
    let mut buf = vec![0u8; bytes];
    rand::thread_rng().fill_bytes(&mut buf);
    buf.iter().map(|b| format!("{b:02x}")).collect()
}

pub fn load_or_create(opts: &SetupOptions) -> anyhow::Result<Secrets> {
    let path = secrets_path(opts);
    if let Ok(text) = fs::read_to_string(&path) {
        let mut s: Secrets =
            serde_json::from_str(&text).with_context(|| format!("parse {}", path.display()))?;
        anyhow::ensure!(
            s.bearer_token.len() >= 32,
            "bearer_token in {} is too short — refusing to run with a weak secret",
            path.display()
        );
        s.created = false;
        return Ok(s);
    }

    let s = Secrets {
        // 24 bytes hex = 48 chars, matching the existing deployment's shape.
        bearer_token: random_hex(24),
        jwt_signing_key: Some(random_hex(32)),
        created: true,
    };
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).with_context(|| format!("mkdir {}", parent.display()))?;
    }
    fs::write(&path, serde_json::to_string_pretty(&s)?)
        .with_context(|| format!("write {}", path.display()))?;
    Ok(s)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn create_then_reload_is_stable() {
        let dir = std::env::temp_dir().join(format!("cyrus-setup-secrets-{}", std::process::id()));
        std::env::set_var("CYRUS_HOME", &dir);
        let opts = SetupOptions::new(".");
        let a = load_or_create(&opts).unwrap();
        assert!(a.created);
        assert_eq!(a.bearer_token.len(), 48);
        let b = load_or_create(&opts).unwrap();
        assert!(!b.created);
        assert_eq!(a.bearer_token, b.bearer_token);
        std::env::remove_var("CYRUS_HOME");
        let _ = std::fs::remove_dir_all(dir);
    }
}
