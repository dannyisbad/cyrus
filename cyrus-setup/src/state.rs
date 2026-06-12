//! `~/.cyrus/state.json` — cyrus's own record of a completed setup. Enrollment
//! itself is implied by `connector.json` (a recorded connector means setup ran);
//! this file is additive, holding the inputs an enrolled launch needs to
//! self-heal the stack WITHOUT asking again:
//!
//!   * `tunnel`    — the lane the user picked, so repair re-opens the same one
//!                   (quick / ngrok / named) instead of re-running the Auto
//!                   precedence ladder and possibly landing on a different lane.
//!   * `repo_root` — the repo the stack was set up to serve, so a `cyrus` run
//!                   from some unrelated directory repairs the original stack
//!                   rather than repointing chimera at the cwd.
//!
//! Source of truth for "is this user set up?" lives in cyrus's own home, NEVER
//! in the user's codex config — that's the whole point of the launch model.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::{SetupOptions, TunnelChoice};

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct CyrusState {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repo_root: Option<PathBuf>,
    #[serde(default)]
    pub tunnel: TunnelChoice,
}

fn state_path(opts: &SetupOptions) -> PathBuf {
    opts.cyrus_home().join("state.json")
}

/// The recorded setup state, or defaults when none has been written yet (older
/// installs that predate this file). Never errors — missing/garbage is default.
pub fn load(opts: &SetupOptions) -> CyrusState {
    std::fs::read_to_string(state_path(opts))
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_default()
}

/// Persist the repair hints. Best-effort: a failure here doesn't fail setup
/// (repair just falls back to Auto tunnel + cwd repo).
pub fn save(opts: &SetupOptions, state: &CyrusState) {
    let path = state_path(opts);
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Ok(text) = serde_json::to_string_pretty(state) {
        let _ = std::fs::write(path, text);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_tunnel_and_repo() {
        let s = CyrusState {
            repo_root: Some(PathBuf::from("C:/x/repo")),
            tunnel: TunnelChoice::Ngrok {
                domain: Some("a.ngrok-free.app".to_string()),
            },
        };
        let text = serde_json::to_string(&s).unwrap();
        let back: CyrusState = serde_json::from_str(&text).unwrap();
        assert_eq!(back.repo_root, s.repo_root);
        assert_eq!(back.tunnel, s.tunnel);
        // missing file shape -> default (no panic, Auto lane).
        let def: CyrusState = serde_json::from_str("{}").unwrap();
        assert!(def.repo_root.is_none());
        assert_eq!(def.tunnel, TunnelChoice::Auto);
    }
}
