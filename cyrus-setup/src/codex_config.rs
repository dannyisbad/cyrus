//! `${CODEX_HOME}/config.toml` — cyrus no longer WRITES anything here. The
//! `shadow` provider is injected by the wrapper as per-invocation `-c` overrides
//! at launch, so a plain `codex` stays byte-for-byte pristine and "is cyrus set
//! up?" is answered by cyrus's own home, never by this file. The only thing we
//! touch config.toml for now is removing a stale `[model_providers.shadow]`
//! block an OLDER cyrus persisted (format-preserving via toml_edit).

use std::path::PathBuf;

use anyhow::Context;
use toml_edit::DocumentMut;

use crate::home_dir;

pub const SHADOW_PROVIDER_ID: &str = "shadow";

fn codex_config_path() -> PathBuf {
    let home = match std::env::var("CODEX_HOME") {
        Ok(h) if !h.is_empty() => PathBuf::from(h),
        _ => home_dir().join(".codex"),
    };
    home.join("config.toml")
}

/// Remove a `[model_providers.shadow]` block a previous cyrus persisted, leaving
/// the rest of config.toml byte-identical (toml_edit is format-preserving).
/// Returns true when something was removed. A missing/unparseable file or an
/// absent block is a no-op (`Ok(false)`) — there's nothing to clean.
pub fn remove_shadow_provider() -> anyhow::Result<bool> {
    let path = codex_config_path();
    let Ok(text) = std::fs::read_to_string(&path) else {
        return Ok(false);
    };
    let Ok(mut doc) = text.parse::<DocumentMut>() else {
        // Don't fight a config we can't parse — leave it untouched.
        return Ok(false);
    };

    let Some(mp) = doc.get_mut("model_providers").and_then(|mp| mp.as_table_mut()) else {
        return Ok(false);
    };
    if mp.remove(SHADOW_PROVIDER_ID).is_none() {
        return Ok(false);
    }
    // Drop a now-empty [model_providers] header rather than leave the noise.
    if mp.is_empty() {
        doc.as_table_mut().remove("model_providers");
    }

    std::fs::write(&path, doc.to_string()).with_context(|| format!("write {}", path.display()))?;
    Ok(true)
}

/// The `base_url` currently recorded for `[model_providers.shadow]`, if any.
/// Read-only probe for `cyrus check` — never writes.
pub fn current_provider_base_url() -> Option<String> {
    let text = std::fs::read_to_string(codex_config_path()).ok()?;
    let doc: DocumentMut = text.parse().ok()?;
    doc.get("model_providers")
        .and_then(|mp| mp.get(SHADOW_PROVIDER_ID))
        .and_then(|p| p.get("base_url"))
        .and_then(|v| v.as_str())
        .map(str::to_string)
}

#[cfg(test)]
mod tests {
    use super::*;

    // Single test: these mutate the process-global CODEX_HOME, so they must not
    // run alongside each other (parallel siblings would clobber the env). One
    // sequential test keeps it the only CODEX_HOME mutator in the crate.
    #[test]
    fn remove_strips_only_our_block_and_drops_empty_header() {
        let dir = std::env::temp_dir().join(format!("cyrus-setup-codexcfg-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::env::set_var("CODEX_HOME", &dir);
        let cfg = dir.join("config.toml");

        // Case 1: shadow alongside the user's keys + an unrelated provider that
        // must survive, and the [model_providers] header must stay (other lives).
        std::fs::write(
            &cfg,
            "model = \"gpt-5.5\"\n\
             [model_providers.shadow]\n\
             name = \"cyrus\"\n\
             base_url = \"http://127.0.0.1:8765/v1\"\n\
             requires_openai_auth = false\n\
             [model_providers.other]\n\
             name = \"keepme\"\n",
        )
        .unwrap();

        assert!(remove_shadow_provider().unwrap());
        let text = std::fs::read_to_string(&cfg).unwrap();
        assert!(!text.contains("model_providers.shadow"));
        assert!(!text.contains("127.0.0.1:8765"));
        assert!(text.contains("model = \"gpt-5.5\""));
        assert!(text.contains("[model_providers.other]"));
        assert!(text.contains("keepme"));
        assert_eq!(current_provider_base_url(), None);
        // second pass is a clean no-op.
        assert!(!remove_shadow_provider().unwrap());

        // Case 2: shadow is the ONLY provider — the bare header goes too.
        std::fs::write(
            &cfg,
            "model = \"gpt-5.5\"\n\
             [model_providers.shadow]\n\
             base_url = \"http://127.0.0.1:8765/v1\"\n",
        )
        .unwrap();
        assert!(remove_shadow_provider().unwrap());
        let text = std::fs::read_to_string(&cfg).unwrap();
        assert!(!text.contains("model_providers"));
        assert!(text.contains("model = \"gpt-5.5\""));

        std::env::remove_var("CODEX_HOME");
        let _ = std::fs::remove_dir_all(dir);
    }
}
