//! Tab lifecycle over a BROWSER-scoped CDP control socket (Target.* operations),
//! plus the arm-before-navigate helper.
//!
//! Creates/closes/lists ChatGPT tabs through a BROWSER-scoped control socket
//! (`Target.*` lives at the browser endpoint, not a page session). Each subagent
//! then attaches its OWN page socket via [`crate::cdp::CdpClient`] `for_target` —
//! that per-tab socket is what makes WS token cross-talk structurally impossible.
//!
//! Hardening baked in:
//!   * [`arm_and_navigate`]: open to about:blank, install document-start scripts +
//!     `Network.enable` BEFORE navigating, so the opening-burst of ChatGPT's own
//!     WebSocket is never missed (the "first turn truncated" failure).
//!   * durable manifest: created target ids persisted so a crashed harness can
//!     reconcile/close orphan tabs on restart.
//!   * human-like creation: small jitter + `Target.activateTarget`, so N tabs
//!     don't appear in a non-human instantaneous burst.

use std::collections::HashMap;
use std::collections::HashSet;
use std::path::PathBuf;
use std::time::Duration;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;

use anyhow::anyhow;
use anyhow::Context;
use futures::SinkExt;
use futures::StreamExt;
use rand::Rng;
use serde::Deserialize;
use serde::Serialize;
use serde_json::json;
use serde_json::Value;
use tokio::net::TcpStream;
use tokio::sync::Mutex;
use tokio::time::timeout;
use tokio_tungstenite::tungstenite::protocol::Message;
use tokio_tungstenite::tungstenite::protocol::WebSocketConfig;
use tokio_tungstenite::MaybeTlsStream;
use tokio_tungstenite::WebSocketStream;

use crate::cdp::CdpClient;

/// Browser control websocket (browser-protocol level, not a page session).
type ControlWs = WebSocketStream<MaybeTlsStream<TcpStream>>;

/// Browser-scoped CDP socket for `Target.*` operations.
///
/// All command traffic is serialized behind `inner`'s lock: a single browser
/// socket carries every `Target.*` call, so concurrent callers must take turns
/// on the wire.
pub struct BrowserControl {
    host: String,
    port: u16,
    http: reqwest::Client,
    inner: Mutex<ControlState>,
}

/// The mutable half of [`BrowserControl`]: the open websocket and the monotonic
/// command id. Behind a single lock so `cmd` is atomic end-to-end (send + read
/// the matching reply).
struct ControlState {
    ws: Option<ControlWs>,
    id: i64,
}

/// One entry of Chrome's `/json` target list (and `Target.getTargets`'
/// `targetInfos`). Only the fields we use are modeled; the wire carries more
/// and we ignore the rest.
#[derive(Debug, Clone, Deserialize)]
pub struct TargetInfo {
    /// `id` from `/json`, or `targetId` from `Target.getTargets` (see
    /// [`TargetInfo::resolve_id`]).
    #[serde(default)]
    pub id: Option<String>,
    #[serde(rename = "targetId", default)]
    pub target_id: Option<String>,
    #[serde(rename = "type", default)]
    pub r#type: Option<String>,
    #[serde(default)]
    pub url: Option<String>,
    #[serde(rename = "webSocketDebuggerUrl", default)]
    pub web_socket_debugger_url: Option<String>,
}

impl TargetInfo {
    /// The page/target id, regardless of which endpoint produced the record:
    /// `/json` uses `id`, `Target.getTargets` uses `targetId`.
    pub fn resolve_id(&self) -> Option<&str> {
        self.id
            .as_deref()
            .or(self.target_id.as_deref())
    }
}

impl BrowserControl {
    pub fn new(host: impl Into<String>, port: u16) -> Self {
        Self {
            host: host.into(),
            port,
            http: reqwest::Client::new(),
            inner: Mutex::new(ControlState { ws: None, id: 0 }),
        }
    }

    /// `http://{host}:{port}`.
    pub fn base(&self) -> String {
        format!("http://{}:{}", self.host, self.port)
    }

    /// Discover the browser endpoint's `webSocketDebuggerUrl` via
    /// `{base}/json/version`, then open the control socket with the frame-size
    /// caps disabled: CDP DOM payloads can be large, and a cap would truncate
    /// them.
    pub async fn connect(&self) -> anyhow::Result<()> {
        let ver: Value = self
            .http
            .get(format!("{}/json/version", self.base()))
            .send()
            .await
            .context("GET /json/version")?
            .json()
            .await
            .context("decode /json/version")?;

        let ws_url = ver
            .get("webSocketDebuggerUrl")
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow!("no webSocketDebuggerUrl in /json/version"))?;

        // Disable the message/frame size caps; without this, large CDP payloads
        // truncate.
        let mut config = WebSocketConfig::default();
        config.max_message_size = None;
        config.max_frame_size = None;

        let (ws, _resp) =
            tokio_tungstenite::connect_async_with_config(ws_url, Some(config), false)
                .await
                .context("connect browser control websocket")?;

        let mut state = self.inner.lock().await;
        state.ws = Some(ws);
        Ok(())
    }

    /// Send a command and await its id-correlated reply.
    ///
    /// Held entirely under the lock so the send and the matching read are
    /// atomic. Default timeout is 20s.
    async fn cmd(&self, method: &str, params: Value) -> anyhow::Result<Value> {
        self.cmd_timeout(method, params, Duration::from_secs(20))
            .await
    }

    async fn cmd_timeout(
        &self,
        method: &str,
        params: Value,
        wait: Duration,
    ) -> anyhow::Result<Value> {
        let mut state = self.inner.lock().await;
        let mid = {
            state.id += 1;
            state.id
        };
        let ws = state
            .ws
            .as_mut()
            .ok_or_else(|| anyhow!("browser control socket not open"))?;

        let payload = json!({ "id": mid, "method": method, "params": params });
        ws.send(Message::Text(payload.to_string()))
            .await
            .context("send Target.* command")?;

        // Read until we see our id (scan the incoming stream for a reply whose
        // `id` matches `mid`). The whole read is wrapped in the command timeout.
        let read = async {
            while let Some(msg) = ws.next().await {
                let msg = msg.context("browser control socket read")?;
                let text = match msg {
                    Message::Text(t) => t,
                    Message::Binary(b) => String::from_utf8_lossy(&b).into_owned(),
                    Message::Close(_) => return Ok(Value::Null),
                    _ => continue,
                };
                let d: Value = match serde_json::from_str(&text) {
                    Ok(v) => v,
                    Err(_) => continue,
                };
                if d.get("id").and_then(Value::as_i64) == Some(mid) {
                    if let Some(err) = d.get("error") {
                        return Err(anyhow!(err.to_string()));
                    }
                    return Ok(d.get("result").cloned().unwrap_or(Value::Null));
                }
                // Not ours (an event, or another id) — keep scanning.
            }
            Ok(Value::Null)
        };

        match timeout(wait, read).await {
            Ok(res) => res,
            Err(_) => Err(anyhow!("browser control timeout: {method}")),
        }
    }

    /// Returns the new `targetId`.
    pub async fn create_target(&self, url: &str) -> anyhow::Result<String> {
        let r = self
            .cmd("Target.createTarget", json!({ "url": url }))
            .await?;
        r.get("targetId")
            .and_then(Value::as_str)
            .map(str::to_owned)
            .ok_or_else(|| anyhow!("Target.createTarget returned no targetId"))
    }

    /// Best-effort; swallows errors.
    pub async fn close_target(&self, target_id: &str) {
        let _ = self
            .cmd("Target.closeTarget", json!({ "targetId": target_id }))
            .await;
    }

    /// Best-effort; swallows errors.
    pub async fn activate_target(&self, target_id: &str) {
        let _ = self
            .cmd("Target.activateTarget", json!({ "targetId": target_id }))
            .await;
    }

    /// `targetInfos` from `Target.getTargets`.
    pub async fn get_targets(&self) -> anyhow::Result<Vec<TargetInfo>> {
        let r = self.cmd("Target.getTargets", json!({})).await?;
        let infos = r
            .get("targetInfos")
            .cloned()
            .unwrap_or_else(|| Value::Array(vec![]));
        Ok(serde_json::from_value(infos).unwrap_or_default())
    }

    /// `BrowserControl.page_ws_url` — poll `{base}/json` until the new target
    /// exposes a `webSocketDebuggerUrl`. Up to `tries` attempts, 0.3s apart
    /// (defaults: `tries = 20`). Returns `None` if it never appears.
    pub async fn page_ws_url(&self, target_id: &str, tries: u32) -> Option<String> {
        for _ in 0..tries {
            if let Ok(resp) = self.http.get(format!("{}/json", self.base())).send().await {
                if let Ok(list) = resp.json::<Vec<TargetInfo>>().await {
                    for t in &list {
                        if t.id.as_deref() == Some(target_id) {
                            if let Some(ws) = t.web_socket_debugger_url.as_deref() {
                                if !ws.is_empty() {
                                    return Some(ws.to_owned());
                                }
                            }
                        }
                    }
                }
            }
            tokio::time::sleep(Duration::from_millis(300)).await;
        }
        None
    }

    /// `BrowserControl.close`.
    pub async fn close(&self) {
        let mut state = self.inner.lock().await;
        if let Some(mut ws) = state.ws.take() {
            let _ = ws.close(None).await;
        }
    }
}

/// `arm_and_navigate(cdp, url, init_scripts)`.
///
/// Arm document-start scripts + Network BEFORE navigating, so the page's own
/// socket can't open ahead of our taps. `cdp` is a connected page-scoped
/// [`CdpClient`].
///
/// ORDERING IS LOAD-BEARING — `Page.enable`, install each init script, then
/// `Network.enable`, and only THEN navigate. Doing the navigate first loses
/// ChatGPT's opening WebSocket burst ("first turn truncated"). Init-script
/// installation is best-effort.
pub async fn arm_and_navigate(
    cdp: &CdpClient,
    url: &str,
    init_scripts: &[String],
) -> anyhow::Result<()> {
    cdp.send("Page.enable", None).await?;
    for src in init_scripts {
        // best-effort: a failing init script must not abort the arm sequence.
        let _ = cdp
            .send(
                "Page.addScriptToEvaluateOnNewDocument",
                Some(json!({ "source": src })),
            )
            .await;
    }
    cdp.send("Network.enable", None).await?;
    // Python `cdp.navigate(url)` is exactly `Page.navigate` with the url.
    cdp.send("Page.navigate", Some(json!({ "url": url }))).await?;
    Ok(())
}

/// One self-created tab tracked in the durable manifest.
///
/// Serializes as `{ "url", "ts", "agent_id" }` to match the Python manifest
/// payload byte-for-byte (a JSON object keyed by target id). `agent_id` is
/// omitted from the map when absent, mirroring Python's `None`/`null`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CreatedTab {
    pub url: String,
    pub ts: f64,
    #[serde(default)]
    pub agent_id: Option<String>,
}

/// Tab lifecycle + durable manifest. See `TabFactory` in tab_factory.py.
///
/// Note on the constructor: the Python `TabFactory(cfg, manifest_path)` reads
/// `cfg.cdp_host` / `cfg.cdp_port` off the shared `ShadowConfig`. The config
/// module is still a stub in this scaffold, so [`TabFactory::new`] takes the two
/// values directly (the conductor/mux will pass `cfg.cdp_host`, `cfg.cdp_port`).
pub struct TabFactory {
    pub browser: BrowserControl,
    manifest_path: Option<PathBuf>,
    /// target_id -> CreatedTab. The manifest of tabs WE opened; the gate that
    /// keeps `reconcile_orphans` from ever touching a human's tab.
    created: Mutex<HashMap<String, CreatedTab>>,
    /// Verified empirically; a probe can downgrade it. Kept as a public field to
    /// mirror the Python attribute `self.transport = "page_ws"`.
    pub transport: Mutex<String>,
}

impl TabFactory {
    /// `TabFactory.__init__` — see the struct note about the constructor shape.
    pub fn new(cdp_host: impl Into<String>, cdp_port: u16, manifest_path: Option<PathBuf>) -> Self {
        Self {
            browser: BrowserControl::new(cdp_host, cdp_port),
            manifest_path,
            created: Mutex::new(HashMap::new()),
            transport: Mutex::new("page_ws".to_string()),
        }
    }

    /// `TabFactory.start`.
    pub async fn start(&self) -> anyhow::Result<()> {
        self.browser.connect().await?;
        self.load_manifest().await;
        Ok(())
    }

    /// `TabFactory.open_tab`.
    ///
    /// Create a tab at about:blank (caller arms+navigates). Returns the
    /// `target_id`. With `human`, a small jitter precedes creation and the new
    /// tab is activated afterward, so N tabs don't appear in a non-human
    /// instantaneous burst.
    ///
    /// HAZARD: the tab MUST open to about:blank first; the caller then runs
    /// [`arm_and_navigate`] before driving to chatgpt.com, or ChatGPT's opening
    /// WebSocket burst is lost.
    pub async fn open_tab(
        &self,
        url: &str,
        agent_id: Option<String>,
        human: bool,
    ) -> anyhow::Result<String> {
        if human {
            let secs = {
                // random.uniform(0.3, 1.1)
                let mut rng = rand::thread_rng();
                rng.gen_range(0.3f64..1.1f64)
            };
            tokio::time::sleep(Duration::from_secs_f64(secs)).await;
        }

        let target_id = self.browser.create_target("about:blank").await?;

        {
            let mut created = self.created.lock().await;
            created.insert(
                target_id.clone(),
                CreatedTab {
                    url: url.to_string(),
                    ts: now_unix(),
                    agent_id,
                },
            );
        }
        self.persist().await;

        if human {
            self.browser.activate_target(&target_id).await;
        }
        Ok(target_id)
    }

    /// `TabFactory.close_tab`.
    pub async fn close_tab(&self, target_id: &str) {
        self.browser.close_target(target_id).await;
        {
            let mut created = self.created.lock().await;
            created.remove(target_id);
        }
        self.persist().await;
    }

    /// `TabFactory.live_chatgpt_targets` — page targets currently on
    /// chatgpt.com.
    pub async fn live_chatgpt_targets(&self) -> anyhow::Result<Vec<TargetInfo>> {
        let targets = self.browser.get_targets().await?;
        Ok(targets
            .into_iter()
            .filter(|t| {
                t.r#type.as_deref() == Some("page")
                    && t.url
                        .as_deref()
                        .unwrap_or("")
                        .contains("chatgpt.com")
            })
            .collect())
    }

    /// `TabFactory.reconcile_orphans`.
    ///
    /// Close tabs WE created (in the manifest) that are no longer tracked by a
    /// live SubProvider. NEVER touches tabs we didn't create (the human's tabs):
    /// the loop only ever considers ids already in `created`, so a human-opened
    /// tab — absent from the manifest — can never be closed here. Returns the
    /// list of closed target ids.
    pub async fn reconcile_orphans(&self, active_target_ids: &HashSet<String>) -> Vec<String> {
        let mut closed = Vec::new();
        // Snapshot the manifest keys first (the Python iterates `list(...)` so it
        // can mutate `_created` via close_tab during the loop).
        let candidates: Vec<String> = {
            let created = self.created.lock().await;
            created.keys().cloned().collect()
        };
        for tid in candidates {
            if !active_target_ids.contains(&tid) {
                self.close_tab(&tid).await;
                closed.push(tid);
            }
        }
        closed
    }

    // ----- durable manifest (crash recovery) -----

    /// `TabFactory._persist` — best-effort write of the manifest. Swallows IO
    /// errors (Python `try/except: pass`).
    async fn persist(&self) {
        let path = match &self.manifest_path {
            Some(p) => p.clone(),
            None => return,
        };
        let snapshot = {
            let created = self.created.lock().await;
            created.clone()
        };
        let body = match serde_json::to_string(&snapshot) {
            Ok(b) => b,
            Err(_) => return,
        };
        if let Some(parent) = path.parent() {
            let _ = tokio::fs::create_dir_all(parent).await;
        }
        let _ = tokio::fs::write(&path, body).await;
    }

    /// `TabFactory._load_manifest` — load the manifest if present; on any parse
    /// error reset to empty (Python `except: self._created = {}`).
    async fn load_manifest(&self) {
        let path = match &self.manifest_path {
            Some(p) => p.clone(),
            None => return,
        };
        let text = match tokio::fs::read_to_string(&path).await {
            Ok(t) => t,
            Err(_) => return, // missing file: leave _created untouched (Python returns early)
        };
        let parsed: HashMap<String, CreatedTab> =
            serde_json::from_str(&text).unwrap_or_default();
        let mut created = self.created.lock().await;
        *created = parsed;
    }

    /// `TabFactory.close`.
    pub async fn close(&self) {
        self.browser.close().await;
    }
}

/// `time.time()` — seconds since the Unix epoch as a float.
fn now_unix() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}
