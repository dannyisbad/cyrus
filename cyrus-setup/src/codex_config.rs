//! `${CODEX_HOME}/config.toml` — write the `[model_providers.shadow]` entry
//! the codex TUI's option 4 selects. Format-preserving (toml_edit) and
//! idempotent: an entry that already matches is left untouched.

use std::path::PathBuf;

use anyhow::Context;
use toml_edit::{table, value, DocumentMut};

use crate::home_dir;

pub const SHADOW_PROVIDER_ID: &str = "shadow";

/// User-facing provider name shown in codex `/status` and at startup. Just the
/// brand: "via ChatGPT" collides with codex's native ChatGPT sign-in (which mints
/// an API token), and internal component names (chimera/lipsync) mean nothing to
/// users — so the brand stands alone.
const SHADOW_PROVIDER_NAME: &str = "cyrus";

fn codex_config_path() -> PathBuf {
    let home = match std::env::var("CODEX_HOME") {
        Ok(h) if !h.is_empty() => PathBuf::from(h),
        _ => home_dir().join(".codex"),
    };
    home.join("config.toml")
}

/// Ensure `[model_providers.shadow]` points at the shim. Returns true when the
/// file changed.
pub fn ensure_shadow_provider(shim_base_url: &str) -> anyhow::Result<bool> {
    let path = codex_config_path();
    let text = std::fs::read_to_string(&path).unwrap_or_default();
    let mut doc: DocumentMut = text
        .parse()
        .with_context(|| format!("parse {}", path.display()))?;

    let current = doc
        .get("model_providers")
        .and_then(|mp| mp.get(SHADOW_PROVIDER_ID))
        .and_then(|p| p.get("base_url"))
        .and_then(|v| v.as_str());
    let current_auth = doc
        .get("model_providers")
        .and_then(|mp| mp.get(SHADOW_PROVIDER_ID))
        .and_then(|p| p.get("requires_openai_auth"))
        .and_then(|v| v.as_bool());
    let current_memories = doc
        .get("features")
        .and_then(|f| f.get("memories"))
        .and_then(|v| v.as_bool());
    let current_name = doc
        .get("model_providers")
        .and_then(|mp| mp.get(SHADOW_PROVIDER_ID))
        .and_then(|p| p.get("name"))
        .and_then(|v| v.as_str());
    if current == Some(shim_base_url)
        && current_auth == Some(false)
        && current_memories == Some(false)
        && current_name == Some(SHADOW_PROVIDER_NAME)
    {
        return Ok(false);
    }

    if doc.get("model_providers").is_none() {
        doc["model_providers"] = table();
        // A bare [model_providers] header is noise; mark implicit so only the
        // [model_providers.shadow] table renders.
        if let Some(t) = doc["model_providers"].as_table_mut() {
            t.set_implicit(true);
        }
    }
    let mp = doc["model_providers"]
        .as_table_mut()
        .context("model_providers is not a table in codex config.toml")?;
    if mp.get(SHADOW_PROVIDER_ID).is_none() {
        mp[SHADOW_PROVIDER_ID] = table();
    }
    let p = mp[SHADOW_PROVIDER_ID]
        .as_table_mut()
        .context("model_providers.shadow is not a table")?;
    p["name"] = value(SHADOW_PROVIDER_NAME);
    p["base_url"] = value(shim_base_url);
    p["wire_api"] = value("responses");
    p["requires_openai_auth"] = value(false);

    // Memory consolidation fires background model requests (request_kind
    // "memory" + /v1/memories/* unary calls) that burn the user's ChatGPT
    // quota through the shim for little benefit; keep it off under shadow.
    if doc.get("features").is_none() {
        doc["features"] = table();
        if let Some(t) = doc["features"].as_table_mut() {
            t.set_implicit(true);
        }
    }
    doc["features"]["memories"] = value(false);

    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).ok();
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

    #[test]
    fn writes_and_is_idempotent() {
        let dir = std::env::temp_dir().join(format!("cyrus-setup-codexcfg-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::env::set_var("CODEX_HOME", &dir);
        std::fs::write(
            dir.join("config.toml"),
            "model = \"gpt-5.5\"\napproval_policy = \"never\"\n",
        )
        .unwrap();

        let changed = ensure_shadow_provider("http://127.0.0.1:8765/v1").unwrap();
        assert!(changed);
        let text = std::fs::read_to_string(dir.join("config.toml")).unwrap();
        assert!(text.contains("[model_providers.shadow]"));
        assert!(text.contains("base_url = \"http://127.0.0.1:8765/v1\""));
        assert!(text.contains("requires_openai_auth = false"));
        assert!(text.contains("memories = false"));
        // user's existing keys preserved
        assert!(text.contains("model = \"gpt-5.5\""));

        let changed2 = ensure_shadow_provider("http://127.0.0.1:8765/v1").unwrap();
        assert!(!changed2);

        std::env::remove_var("CODEX_HOME");
        let _ = std::fs::remove_dir_all(dir);
    }
}
