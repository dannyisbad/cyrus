//! The ChatGPT MCP connector, driven through the validated page-context flow
//! (`cyrus_connect.js`, embedded): account prep + dedupe + OAuth discovery +
//! create as same-origin `fetch()` from the logged-in chatgpt.com tab, then
//! the ONE navigation — `/oauth/authorize?...&cyrus_nonce=` — auto-issued by
//! chimera via a loopback-armed one-time nonce. No settings UI, no password.
//!
//! Reuse is VERIFIED, not assumed: a deduped link must pass refresh_actions
//! (a link whose token died — e.g. rotated secrets — gets deleted and
//! recreated instead of being returned as a false success).

use std::path::PathBuf;
use std::time::Duration;

use anyhow::Context;
use cyrus_lipsync::cdp::CdpClient;
use cyrus_lipsync::tab_factory::BrowserControl;
use rand::RngCore;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::chrome::{attach_with_retry, ChromeOutcome};
use crate::secrets::Secrets;
use crate::{SetupOptions, CYRUS_CONNECT_JS};

pub struct ConnectorOutcome {
    pub connector_id: String,
    pub link_id: String,
    pub tool_count: usize,
    pub reused: bool,
}

/// The connector(s) cyrus has created, persisted at `~/.cyrus/connector.json`.
///
/// This is the basis of SURGICAL cleanup: when the tunnel URL changes (the
/// common case for quick/ephemeral tunnels), the previous connector is stale.
/// We delete ONLY ids in this file — connectors cyrus itself created — and
/// never a domain/host match, so a shared tunnel apex (trycloudflare.com,
/// ngrok-free.app) can never cause us to delete the user's other connectors.
#[derive(Debug, Default, Serialize, Deserialize)]
struct ConnectorState {
    /// The connector for the current (or most recent) mcp_url.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    current: Option<ConnectorRecord>,
    /// Ids we created for older URLs that haven't been deleted yet.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    retired: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ConnectorRecord {
    mcp_url: String,
    connector_id: String,
    #[serde(default)]
    link_id: String,
}

fn state_path(opts: &SetupOptions) -> PathBuf {
    opts.cyrus_home().join("connector.json")
}

/// A connector cyrus has recorded for the current tunnel URL. Read-only summary
/// for `cyrus check` (the full `ConnectorState` stays private).
pub struct RecordedConnector {
    pub mcp_url: String,
    pub connector_id: String,
    pub link_id: String,
}

/// The connector cyrus last recorded (`~/.cyrus/connector.json`'s `current`),
/// if any. Does not contact ChatGPT — just reads local state.
pub fn recorded_connector(opts: &SetupOptions) -> Option<RecordedConnector> {
    load_state(opts).current.map(|c| RecordedConnector {
        mcp_url: c.mcp_url,
        connector_id: c.connector_id,
        link_id: c.link_id,
    })
}

fn load_state(opts: &SetupOptions) -> ConnectorState {
    std::fs::read_to_string(state_path(opts))
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_default()
}

fn save_state(opts: &SetupOptions, state: &ConnectorState) {
    let path = state_path(opts);
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Ok(text) = serde_json::to_string_pretty(state) {
        let _ = std::fs::write(path, text);
    }
}

/// Decide which connector ids to delete given the current mcp_url, mutating
/// `state` to the post-cleanup shape (stale current retired then drained). Pure
/// (no I/O) so the surgical-cleanup invariant is unit-testable: the returned
/// ids are ONLY connectors cyrus recorded — never anything keyed on a domain.
fn take_retire_ids(state: &mut ConnectorState, mcp_url: &str) -> Vec<String> {
    if let Some(cur) = state.current.as_ref().filter(|c| c.mcp_url != mcp_url) {
        let id = cur.connector_id.clone();
        if !state.retired.contains(&id) {
            state.retired.push(id);
        }
        state.current = None;
    }
    std::mem::take(&mut state.retired)
}

fn nonce_hex() -> String {
    let mut buf = [0u8; 16];
    rand::thread_rng().fill_bytes(&mut buf);
    buf.iter().map(|b| format!("{b:02x}")).collect()
}

async fn inject(cdp: &CdpClient) -> anyhow::Result<()> {
    let v = cdp
        .eval(CYRUS_CONNECT_JS, 20.0)
        .await
        .map_err(|e| anyhow::anyhow!("inject cyrus_connect.js: {e}"))?;
    anyhow::ensure!(
        v.as_str() == Some("cyrus_connect ready"),
        "unexpected inject result: {v}"
    );
    Ok(())
}

/// `__cyrus.setup(...)` — account prep, dedupe, discovery, create.
async fn js_setup(cdp: &CdpClient, base: &str, name: &str) -> anyhow::Result<Value> {
    let expr = format!(
        "__cyrus.setup({{base: {}, name: {}}})",
        serde_json::to_string(base)?,
        serde_json::to_string(name)?
    );
    cdp.eval(&expr, 90.0)
        .await
        .map_err(|e| anyhow::anyhow!("__cyrus.setup: {e}"))
}

/// `__cyrus.finishByConnector(...)` — find link, refresh tools (slow), lock to
/// never-ask. Retried: the link materializes only after ChatGPT's server-side
/// code exchange completes.
async fn js_finish(cdp: &CdpClient, connector_id: &str) -> anyhow::Result<Value> {
    let expr = format!(
        "__cyrus.finishByConnector({})",
        serde_json::to_string(connector_id)?
    );
    let mut last = String::new();
    for _ in 0..4 {
        match cdp.eval(&expr, 150.0).await {
            Ok(v) => return Ok(v),
            Err(e) => {
                last = e.to_string();
                tokio::time::sleep(Duration::from_secs(3)).await;
            }
        }
    }
    anyhow::bail!("finishByConnector failed: {last}")
}

async fn js_delete_connector(cdp: &CdpClient, connector_id: &str) {
    let expr = format!(
        "__cyrus._api.deleteConnector({})",
        serde_json::to_string(connector_id).unwrap_or_default()
    );
    let _ = cdp.eval(&expr, 30.0).await;
}

/// Arm the one-time consent nonce over loopback — the bearer NEVER enters the
/// page; the page only ever sees the nonce.
async fn arm_consent(opts: &SetupOptions, secrets: &Secrets, nonce: &str) -> anyhow::Result<()> {
    let res = reqwest::Client::new()
        .post(format!(
            "http://127.0.0.1:{}/control/arm-consent",
            opts.chimera_port
        ))
        .bearer_auth(&secrets.bearer_token)
        .json(&json!({"nonce": nonce, "ttl_sec": 90}))
        .timeout(Duration::from_secs(10))
        .send()
        .await
        .context("arm-consent POST")?;
    anyhow::ensure!(
        res.status().is_success(),
        "arm-consent -> HTTP {}",
        res.status()
    );
    Ok(())
}

/// Wait for the OAuth navigation round trip to land back on chatgpt.com.
async fn wait_back_on_chatgpt(opts: &SetupOptions, target_id: &str) -> anyhow::Result<()> {
    let browser = BrowserControl::new(opts.cdp_host.clone(), opts.cdp_port);
    browser.connect().await?;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(90);
    loop {
        let targets = browser.get_targets().await.unwrap_or_default();
        let url = targets
            .iter()
            .find(|t| t.resolve_id() == Some(target_id))
            .and_then(|t| t.url.clone())
            .unwrap_or_default();
        if url.starts_with("https://chatgpt.com") && !url.contains("oauth") {
            browser.close().await;
            // settle: let the SPA finish booting before we re-inject.
            tokio::time::sleep(Duration::from_secs(2)).await;
            return Ok(());
        }
        if tokio::time::Instant::now() >= deadline {
            browser.close().await;
            anyhow::bail!("OAuth round trip did not return to chatgpt.com within 90s (tab at {url})");
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
}

/// Loopback MCP verification: initialize -> initialized -> tools/list count.
pub async fn mcp_tool_count(opts: &SetupOptions, secrets: &Secrets) -> anyhow::Result<usize> {
    let client = reqwest::Client::new();
    let url = format!("http://127.0.0.1:{}/mcp", opts.chimera_port);
    let post = |body: Value| {
        let client = client.clone();
        let url = url.clone();
        let bearer = secrets.bearer_token.clone();
        async move {
            client
                .post(url)
                .bearer_auth(bearer)
                .header("content-type", "application/json")
                .header("accept", "application/json, text/event-stream")
                .json(&body)
                .timeout(Duration::from_secs(15))
                .send()
                .await
        }
    };

    let init = post(json!({
        "jsonrpc": "2.0", "id": 1, "method": "initialize",
        "params": {"protocolVersion": "2025-06-18", "capabilities": {},
                   "clientInfo": {"name": "cyrus-setup", "version": "0.1"}}
    }))
    .await
    .context("mcp initialize")?;
    anyhow::ensure!(init.status().is_success(), "initialize -> {}", init.status());

    let _ = post(json!({"jsonrpc": "2.0", "method": "notifications/initialized"})).await;

    let list = post(json!({"jsonrpc": "2.0", "id": 2, "method": "tools/list"}))
        .await
        .context("tools/list")?;
    let text = list.text().await?;
    let data_line = text
        .lines()
        .find_map(|l| l.strip_prefix("data: "))
        .context("tools/list: no SSE data frame")?;
    let v: Value = serde_json::from_str(data_line)?;
    let count = v["result"]["tools"]
        .as_array()
        .map(|a| a.len())
        .context("tools/list: no tools array")?;
    Ok(count)
}

pub async fn ensure_connector(
    opts: &SetupOptions,
    secrets: &Secrets,
    public_url: &str,
    chrome: &ChromeOutcome,
) -> anyhow::Result<ConnectorOutcome> {
    let mcp_url = format!("{}/mcp", public_url.trim_end_matches('/'));
    let cdp = attach_with_retry(opts, &chrome.login_target_id).await?;
    inject(&cdp).await?;

    // Surgical cleanup: if our previously-created connector was for a DIFFERENT
    // mcp_url (the tunnel URL changed), retire it. We delete only ids we
    // recorded — never a domain match — so other apps' connectors on a shared
    // tunnel apex are always safe.
    let mut state = load_state(opts);
    let retire = take_retire_ids(&mut state, &mcp_url);
    if !retire.is_empty() {
        for id in retire {
            js_delete_connector(&cdp, &id).await; // best-effort
        }
        save_state(opts, &state);
    }

    let mut setup_res = js_setup(&cdp, public_url, &opts.connector_name).await?;
    let mut reused = false;

    // Dedupe path: a connector for this exact mcp_url already has a link.
    // VERIFY it (refresh_actions exercises the link's token); a dead link is
    // deleted and the whole setup re-runs to create fresh.
    if setup_res.get("reusedLinkId").and_then(Value::as_str).is_some() {
        let connector_id = setup_res
            .get("connectorId")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        match js_finish(&cdp, &connector_id).await {
            Ok(fin) => {
                let link_id = fin["linkId"].as_str().unwrap_or_default().to_string();
                let actions = fin["actions"].as_array().map(|a| a.len()).unwrap_or(0);
                let tool_count = mcp_tool_count(opts, secrets).await.unwrap_or(actions);
                state.current = Some(ConnectorRecord {
                    mcp_url: mcp_url.clone(),
                    connector_id: connector_id.clone(),
                    link_id: link_id.clone(),
                });
                save_state(opts, &state);
                return Ok(ConnectorOutcome {
                    connector_id,
                    link_id,
                    tool_count,
                    reused: true,
                });
            }
            Err(e) => {
                tracing::warn!("reused link failed verification ({e}); recreating connector");
                js_delete_connector(&cdp, &connector_id).await;
                setup_res = js_setup(&cdp, public_url, &opts.connector_name).await?;
                reused = false;
            }
        }
    }

    let connector_id = setup_res
        .get("connectorId")
        .and_then(Value::as_str)
        .with_context(|| format!("__cyrus.setup returned no connectorId: {setup_res}"))?
        .to_string();

    // The ONE navigation: arm a one-time nonce over loopback, then send the
    // tab through /oauth/authorize — chimera auto-issues the code (302), no
    // password page.
    let nonce = nonce_hex();
    arm_consent(opts, secrets, &nonce).await?;
    let nav_expr = format!(
        "__cyrus.linkAndNavigate({}, {})",
        serde_json::to_string(&connector_id)?,
        serde_json::to_string(&nonce)?
    );
    // The eval's execution context dies with the navigation — both a clean
    // {navigating:true} and a context-destroyed error mean "it's going".
    let _ = cdp.eval(&nav_expr, 30.0).await;
    cdp.close().await;

    wait_back_on_chatgpt(opts, &chrome.login_target_id).await?;

    // Navigation cleared window.__cyrus — fresh socket, re-inject, finish.
    let cdp = attach_with_retry(opts, &chrome.login_target_id).await?;
    inject(&cdp).await?;
    let fin = js_finish(&cdp, &connector_id).await?;
    cdp.close().await;

    let link_id = fin["linkId"]
        .as_str()
        .with_context(|| format!("finishByConnector returned no linkId: {fin}"))?
        .to_string();
    let actions = fin["actions"].as_array().map(|a| a.len()).unwrap_or(0);
    anyhow::ensure!(
        actions >= 20,
        "connector linked but only {actions} tools enumerated — refresh_actions looks wrong"
    );
    let tool_count = mcp_tool_count(opts, secrets).await.unwrap_or(actions);

    state.current = Some(ConnectorRecord {
        mcp_url: mcp_url.clone(),
        connector_id: connector_id.clone(),
        link_id: link_id.clone(),
    });
    save_state(opts, &state);

    Ok(ConnectorOutcome {
        connector_id,
        link_id,
        tool_count,
        reused,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rec(url: &str, id: &str) -> ConnectorRecord {
        ConnectorRecord {
            mcp_url: url.to_string(),
            connector_id: id.to_string(),
            link_id: String::new(),
        }
    }

    #[test]
    fn same_url_retires_nothing() {
        let mut s = ConnectorState {
            current: Some(rec("https://x/mcp", "c1")),
            retired: vec![],
        };
        assert!(take_retire_ids(&mut s, "https://x/mcp").is_empty());
        assert_eq!(s.current.as_ref().unwrap().connector_id, "c1");
    }

    #[test]
    fn changed_url_retires_only_our_recorded_ids() {
        // current was for the OLD url; an even-older id sits in retired.
        let mut s = ConnectorState {
            current: Some(rec("https://old/mcp", "c_old")),
            retired: vec!["c_older".to_string()],
        };
        let ids = take_retire_ids(&mut s, "https://new/mcp");
        // both ids we created are returned for deletion; nothing else.
        assert!(ids.contains(&"c_old".to_string()));
        assert!(ids.contains(&"c_older".to_string()));
        assert_eq!(ids.len(), 2);
        // post-state: current dropped, retired drained.
        assert!(s.current.is_none());
        assert!(s.retired.is_empty());
    }

    #[test]
    fn state_round_trips_and_missing_is_default() {
        let s = ConnectorState {
            current: Some(rec("https://x/mcp", "c1")),
            retired: vec!["c0".to_string()],
        };
        let text = serde_json::to_string(&s).unwrap();
        let back: ConnectorState = serde_json::from_str(&text).unwrap();
        assert_eq!(back.current.unwrap().connector_id, "c1");
        assert_eq!(back.retired, vec!["c0".to_string()]);
        // empty/garbage -> default (no panic).
        let def: ConnectorState = serde_json::from_str("{}").unwrap();
        assert!(def.current.is_none() && def.retired.is_empty());
    }
}
