//! ShimRuntime + ConductorMux — the ASSEMBLED live runtime behind the
//! `/v1/responses` shim: the shared TabFactory rail, the per-thread
//! `ThreadConductor` router, the chimera model-free bind queue, and the
//! per-folder ChatGPT Project cache.
//!
//! Source: idare/shadow/responses_shim.py (private original)
//!         (class ShadowResponsesShim: tabs/_ensure_tabs, _threads/get_conductor,
//!          _main_thread_id, _eager_main + boot(), conductor_for_control /
//!          await_control_conductor, _chimera_get/_chimera_post,
//!          _seed_binds_once / enqueue_bind / _bind_watcher, resolve_project +
//!          the file-backed project cache, the `/control/*` handler bodies, and
//!          the `_booted` health aggregate)
//!
//! The Python folds the HTTP router and the runtime into ONE class. The port
//! splits them along the seams the ported modules already drew:
//!   - [`ShimRuntime`] implements the conductor-facing [`ConductorShim`] trait
//!     (tab bring-up, page-socket opening, bind rail, project resolution) plus
//!     the sync [`MainRegistry`] half of the `_main_thread_id` bookkeeping.
//!   - [`ConductorMux`] implements [`TurnDriver`] for `responses::build_app`,
//!     owning the `thread-id -> ThreadConductor` map (`get_conductor`), the
//!     eager-main adoption, and the `/control/*` routing.
//!   - [`PageSurface`] adapts the concrete `provider::ChatSurface` (chat.py's
//!     DOM driving) + this tab's own `CdpClient`/`WsTap` to the conductor's
//!     `ChatSurface` trait — one adapter per tab, so per-thread WS isolation is
//!     preserved (one tab = one page socket = one tap).
//!
//! Hazards carried from the Python:
//!   - `open_page` must arm BEFORE navigating (tab_factory.arm_and_navigate with
//!     FETCH_WRAPPER_JS) so ChatGPT's opening burst is never missed, and each
//!     conductor gets its OWN page socket.
//!   - `_seed_binds_once` must claim pre-existing unbound chimera sessions for
//!     "main" BEFORE the first subagent tab opens, or elimination binding can
//!     hand the long-lived main session to a subagent.
//!   - A repeated `/control/toolcall` signature is re-delivered from cache and
//!     NEVER re-executed (`ThreadConductor::control_toolcall` owns that policy).

use std::collections::HashMap;
use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use serde_json::json;
use serde_json::Value;

use crate::cdp::CdpClient;
use crate::conductor::project_name_for_cwd;
use crate::conductor::ChatOverrides;
use crate::conductor::ChatSurface;
use crate::conductor::ConductorShim;
use crate::conductor::ControlToolcall;
use crate::conductor::ServerEventCallback;
use crate::conductor::ThreadConductor;
use crate::conductor::WsCallback;
use crate::config::ShadowConfig;
use crate::provider;
use crate::responses::TurnDriver;
use crate::tab_factory::arm_and_navigate;
use crate::tab_factory::TabFactory;
use crate::wstap::WsTap;

// ===== the page-surface adapter ==============================================

/// One booted tab's page surface: the concrete chat.py port
/// (`provider::ChatSurface`) + this tab's own CDP socket + WS tap, adapted to
/// the conductor's [`ChatSurface`] trait.
struct PageSurface {
    cdp: Arc<CdpClient>,
    chat: provider::ChatSurface,
    /// Kept alive for the tab's lifetime; `reset()` per inject (the conductor
    /// port's contract: "the page layer's tap.reset() is invoked by the page
    /// surface on inject", mirroring the Python's per-turn `self.tap.reset()`).
    tap: WsTap,
}

#[async_trait]
impl ChatSurface for PageSurface {
    async fn inject(&self, text: &str) -> anyhow::Result<()> {
        // Fresh parser state for the new turn (Python: `self.tap.reset()` right
        // before `chat.inject`), then paste + send.
        self.tap.reset();
        self.chat.inject(text).await
    }

    async fn state(&self) -> anyhow::Result<Value> {
        let st = self.chat.state().await?;
        Ok(json!({
            "generating": st.generating,
            "composerReady": st.composer_ready,
            "hasApprove": st.has_approve,
        }))
    }

    async fn approve(&self) -> anyhow::Result<()> {
        self.chat.approve().await
    }

    async fn stop(&self) -> anyhow::Result<()> {
        // chat.py's stop returns a bool ("stably stopped"); the conductor only
        // needs best-effort interruption, so the bool is dropped here.
        let _ = self.chat.stop().await;
        Ok(())
    }

    async fn resolve_slug(&self, model: &str) -> anyhow::Result<Option<String>> {
        Ok(self.chat.resolve_slug(Some(model)).await)
    }

    /// chat.py `set_overrides` WITH the gizmo/no_history axes (the provider
    /// port's narrower `set_overrides(model, effort)` lacks them, so the adapter
    /// ports the full version against the same `__shadow_overrides` contract).
    async fn set_overrides(&self, ov: ChatOverrides) -> anyhow::Result<()> {
        let mut map = serde_json::Map::new();
        // Python: `slug = await self.resolve_slug(model) if model else None`.
        if let Some(m) = ov.model.as_deref().filter(|m| !m.is_empty()) {
            if let Some(slug) = self.chat.resolve_slug(Some(m)).await {
                map.insert("model".to_string(), Value::String(slug));
            }
        }
        if let Some(eff) = provider::resolve_effort(ov.thinking_effort.as_deref()) {
            map.insert("thinking_effort".to_string(), Value::String(eff));
        }
        // Python truthiness: an empty gizmo_id is skipped; no_history only when true.
        if let Some(g) = ov.gizmo_id.as_deref().filter(|g| !g.is_empty()) {
            map.insert("gizmo_id".to_string(), Value::String(g.to_string()));
        }
        if ov.no_history == Some(true) {
            map.insert("no_history".to_string(), Value::Bool(true));
        }
        if map.is_empty() {
            self.cdp
                .eval("localStorage.removeItem('__shadow_overrides')", 30.0)
                .await?;
        } else {
            // localStorage.setItem('__shadow_overrides', json.dumps(json.dumps(ov)))
            let inner = serde_json::to_string(&Value::Object(map))?;
            let arg = serde_json::to_string(&inner)?;
            self.cdp
                .eval(
                    &format!("localStorage.setItem('__shadow_overrides',{arg})"),
                    30.0,
                )
                .await?;
        }
        Ok(())
    }

    async fn current_conversation_id(&self) -> anyhow::Result<Option<String>> {
        // chat.py: location.pathname; "/c/<id>/..." carries the conversation id.
        let path = self.cdp.eval("location.pathname", 30.0).await?;
        let path = path.as_str().unwrap_or("");
        if let Some(rest) = path.split("/c/").nth(1) {
            return Ok(Some(rest.split('/').next().unwrap_or("").to_string()));
        }
        Ok(None)
    }

    /// chat.py `create_project` — create a memory-off ChatGPT Project through the
    /// web app's OWN backend API, authed with the page session. Returns the
    /// gizmo id (`g-p-...`) or None.
    async fn create_project(&self, name: &str) -> anyhow::Result<Option<String>> {
        let obj = json!({
            "instructions": "",
            "name": name,
            "memory_scope": "project_v2",
        });
        let payload = serde_json::to_string(&obj)?;
        let js = [
            "(async()=>{try{",
            "const s=await (await fetch('/api/auth/session')).json();const tok=s.accessToken;",
            "const r=await fetch('/backend-api/projects',{method:'POST',",
            "headers:{'Authorization':'Bearer '+tok,'Content-Type':'application/json'},credentials:'include',",
            "body:JSON.stringify(",
            &payload,
            ")});",
            "const j=await r.json();",
            "return (j.resource&&j.resource.gizmo&&j.resource.gizmo.id)||('err:'+r.status);",
            "}catch(e){return 'err:'+e.message;}})()",
        ]
        .concat();
        let res = self.cdp.eval(&js, 20.0).await?;
        Ok(res
            .as_str()
            .filter(|s| s.starts_with("g-p-"))
            .map(str::to_string))
    }

    /// PATCH a conversation's title via the web app's own backend, authed with
    /// the page session. Best-effort: a non-2xx just leaves the chat unnamed.
    async fn set_conversation_title(&self, conv_id: &str, title: &str) -> anyhow::Result<()> {
        let id = serde_json::to_string(conv_id)?;
        let t = serde_json::to_string(title)?;
        let js = [
            "(async()=>{try{",
            "const s=await (await fetch('/api/auth/session')).json();const tok=s.accessToken;",
            "const r=await fetch('/backend-api/conversation/'+encodeURIComponent(",
            &id,
            "),{method:'PATCH',",
            "headers:{'Authorization':'Bearer '+tok,'Content-Type':'application/json'},credentials:'include',",
            "body:JSON.stringify({title:",
            &t,
            "})});return 'ok:'+r.status;",
            "}catch(e){return 'err:'+e.message;}})()",
        ]
        .concat();
        let _ = self.cdp.eval(&js, 15.0).await?;
        Ok(())
    }

    async fn wait_composer(&self) -> anyhow::Result<()> {
        // chat.py returns a bool; the Python callers proceed regardless, so the
        // adapter mirrors that (a timed-out composer is not an error).
        let _ = self.chat.wait_composer().await;
        Ok(())
    }

    /// The same `/api/auth/session` probe cyrus-setup's chrome step uses: a
    /// logged-out chatgpt.com tab still evals fine, so this — not `state()` —
    /// is what detects the login wall. Unparseable result -> `true`
    /// (conservative: never fail a turn on a flaky probe).
    async fn is_logged_in(&self) -> anyhow::Result<bool> {
        let expr = r#"fetch('/api/auth/session',{credentials:'include'})
            .then(r=>r.json()).then(s=>!!(s&&s.accessToken)).catch(()=>false)"#;
        let v = self.cdp.eval(expr, 15.0).await?;
        Ok(v.as_bool().unwrap_or(true))
    }
}

// ===== the chimera /control rail =============================================

/// The authed HTTP rail to chimera's `/control/*` endpoints
/// (`_chimera_headers` / `_chimera_get` / `_chimera_post`): best-effort, any
/// failure or non-200 collapses to `None` exactly like the Python `try/except`.
#[derive(Clone)]
struct ChimeraRail {
    http: reqwest::Client,
    /// `cfg.server_url` with the trailing slash trimmed.
    base: String,
    bearer: Option<String>,
}

impl ChimeraRail {
    fn new(cfg: &ShadowConfig) -> Self {
        Self {
            http: reqwest::Client::new(),
            base: cfg.server_url.trim_end_matches('/').to_string(),
            bearer: cfg.server_bearer.clone(),
        }
    }

    fn auth(&self, req: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        let req = req.header("content-type", "application/json");
        match &self.bearer {
            Some(b) => req.header("authorization", format!("Bearer {b}")),
            None => req,
        }
    }

    async fn get(&self, path: &str) -> Option<Value> {
        let req = self.auth(self.http.get(format!("{}{}", self.base, path)));
        match req.send().await {
            Ok(r) if r.status() == reqwest::StatusCode::OK => r.json().await.ok(),
            _ => None,
        }
    }

    async fn post(&self, path: &str, body: &Value) -> Option<Value> {
        let req = self.auth(self.http.post(format!("{}{}", self.base, path)));
        match req.json(body).send().await {
            Ok(r) if r.status() == reqwest::StatusCode::OK => r.json().await.ok(),
            _ => None,
        }
    }
}

// ===== main-thread bookkeeping (router side) =================================

/// The router-side half of `_main_thread_id`: the runtime OWNS the value (the
/// conductors read it via [`ConductorShim::main_thread_id`]), but it is the
/// router's `get_conductor` that assigns it — same single attribute, two sides,
/// exactly as in the Python class. Sync because the router consults it under
/// its (sync) threads-map lock.
pub trait MainRegistry: Send + Sync {
    /// `if self._main_thread_id is None and not subagent_kind:
    ///      self._main_thread_id = key` — set-if-unset.
    fn register_main_thread(&self, thread_id: &str);
    /// Sync read of the main thread id.
    fn main_thread_sync(&self) -> Option<String>;
}

// ===== ShimRuntime ===========================================================

/// The shared live runtime: one browser control socket (TabFactory), the
/// model-free chimera bind rail, the per-folder project cache, and the main-
/// thread bookkeeping. Implements [`ConductorShim`] — the surface
/// `ThreadConductor` drives.
pub struct ShimRuntime {
    cfg: ShadowConfig,
    jitter: Arc<provider::Jitter>,
    /// `self.tabs` + `self._tabs_lock`.
    tabs: tokio::sync::Mutex<Option<Arc<TabFactory>>>,
    /// `self._main_thread_id`.
    main_thread_id: std::sync::Mutex<Option<String>>,
    rail: ChimeraRail,
    /// `self._pending_bind` — "codex:<T>", tab-open arrival order.
    pending_bind: Arc<tokio::sync::Mutex<Vec<String>>>,
    /// `self._bound_sessions` — session tokens already bound/claimed.
    bound_sessions: Arc<tokio::sync::Mutex<HashSet<String>>>,
    /// `self._bind_task`.
    bind_task: tokio::sync::Mutex<Option<tokio::task::JoinHandle<()>>>,
    /// `self._bind_seeded`.
    bind_seeded: tokio::sync::Mutex<bool>,
    /// `self._project_cache` (+ `self._project_lock` — the mutex doubles as it).
    project_cache: tokio::sync::Mutex<HashMap<String, String>>,
    /// `self._cache_path`.
    cache_path: PathBuf,
}

impl ShimRuntime {
    /// `ShadowResponsesShim.__init__`'s runtime half: load the file-backed
    /// project cache (any read/parse failure resets to empty).
    pub fn new(cfg: ShadowConfig) -> Self {
        let cache_path = std::env::var("SHIM_PROJECT_CACHE")
            .ok()
            .filter(|s| !s.is_empty())
            .map(PathBuf::from)
            .unwrap_or_else(|| home_dir().join(".codex-shadow-projects.json"));
        let project_cache: HashMap<String, String> = std::fs::read_to_string(&cache_path)
            .ok()
            .and_then(|t| serde_json::from_str(&t).ok())
            .unwrap_or_default();
        let rail = ChimeraRail::new(&cfg);
        Self {
            cfg,
            jitter: Arc::new(provider::Jitter::new()),
            tabs: tokio::sync::Mutex::new(None),
            main_thread_id: std::sync::Mutex::new(None),
            rail,
            pending_bind: Arc::new(tokio::sync::Mutex::new(Vec::new())),
            bound_sessions: Arc::new(tokio::sync::Mutex::new(HashSet::new())),
            bind_task: tokio::sync::Mutex::new(None),
            bind_seeded: tokio::sync::Mutex::new(false),
            project_cache: tokio::sync::Mutex::new(project_cache),
            cache_path,
        }
    }
}

/// `os.path.expanduser("~")` — USERPROFILE on Windows, HOME elsewhere; falls
/// back to the cwd so the cache stays best-effort.
fn home_dir() -> PathBuf {
    std::env::var("USERPROFILE")
        .or_else(|_| std::env::var("HOME"))
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("."))
}

/// The durable tab manifest (crash recovery): `~/.cyrus/tabs.json`, overridable
/// with `SHIM_TAB_MANIFEST` (same env-override convention as
/// `SHIM_PROJECT_CACHE` above).
fn tab_manifest_path() -> PathBuf {
    std::env::var("SHIM_TAB_MANIFEST")
        .ok()
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| home_dir().join(".cyrus").join("tabs.json"))
}

impl MainRegistry for ShimRuntime {
    fn register_main_thread(&self, thread_id: &str) {
        let mut g = self
            .main_thread_id
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if g.is_none() {
            *g = Some(thread_id.to_string());
        }
    }

    fn main_thread_sync(&self) -> Option<String> {
        self.main_thread_id
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }
}

#[async_trait]
impl ConductorShim for ShimRuntime {
    async fn main_thread_id(&self) -> Option<String> {
        self.main_thread_sync()
    }

    /// `shim._ensure_tabs()` — bring up the shared TabFactory once, WITH the
    /// durable tab manifest so chatgpt.com tabs we opened in a crashed/killed
    /// previous run are reconciled (closed) instead of leaking across restarts.
    /// At startup no tab of ours is live yet, so every manifest survivor is an
    /// orphan. The manifest only ever lists tabs WE created — a human's tabs
    /// can never be touched.
    async fn ensure_tabs(&self) -> anyhow::Result<()> {
        let mut g = self.tabs.lock().await;
        if g.is_none() {
            let tf = Arc::new(TabFactory::new(
                self.cfg.cdp_host.clone(),
                self.cfg.cdp_port,
                Some(tab_manifest_path()),
            ));
            tf.start().await?;
            let closed = tf.reconcile_orphans(&HashSet::new()).await;
            if !closed.is_empty() {
                tracing::info!(
                    "[shim] closed {} orphan chatgpt.com tab(s) left by a previous run",
                    closed.len()
                );
            }
            *g = Some(tf);
        }
        Ok(())
    }

    async fn open_tab(
        &self,
        url: &str,
        agent_id: Option<&str>,
        human: bool,
    ) -> anyhow::Result<String> {
        let tabs = self
            .tabs
            .lock()
            .await
            .clone()
            .ok_or_else(|| anyhow::anyhow!("tab factory not started (ensure_tabs first)"))?;
        tabs.open_tab(url, agent_id.map(str::to_string), human).await
    }

    async fn close_tab(&self, target_id: &str) {
        // Best-effort, like the Python's try/except around tabs.close_tab.
        let tabs = self.tabs.lock().await.clone();
        if let Some(tabs) = tabs {
            tabs.close_tab(target_id).await;
        }
    }

    /// The ThreadConductor.boot page bring-up: this tab's OWN page socket
    /// (`CDPClient.for_target`) + chat surface + WS tap, then the load-bearing
    /// arm-BEFORE-navigate with the FETCH_WRAPPER and the composer wait. The
    /// model/effort/gizmo axes are forced afterwards by the conductor through
    /// `set_overrides` (the fetch-wrapper reads `__shadow_overrides`).
    async fn open_page(
        &self,
        target_id: &str,
        on_ws: WsCallback,
    ) -> anyhow::Result<Arc<dyn ChatSurface>> {
        tracing::debug!("[shim] open_page target={target_id}: attaching page socket");
        let cdp = Arc::new(
            CdpClient::for_target(
                self.cfg.cdp_host.clone(),
                self.cfg.cdp_port,
                target_id,
                self.cfg.tab_match.clone(),
            )
            .await?,
        );
        tracing::debug!("[shim] open_page target={target_id}: page socket up, starting tap");
        let chat = provider::ChatSurface::new(cdp.clone(), self.cfg.clone(), self.jitter.clone());
        // The conductor's WsCallback and wstap's OnEvent are the same shape:
        // (kind, value) with kind in token|thinking|turn_complete.
        let mut tap = WsTap::new(cdp.clone(), on_ws);
        tap.start().await?;
        tracing::debug!("[shim] open_page target={target_id}: tap armed, navigating");
        // Python boot: arm_and_navigate(cdp, url, init_scripts=[FETCH_WRAPPER_JS])
        // then chat._wait_composer(). The ?model= query is unnecessary here: the
        // conductor resolves the slug afterwards and pins it via set_overrides,
        // which the FETCH_WRAPPER applies to every /f/conversation turn body.
        arm_and_navigate(
            &cdp,
            "https://chatgpt.com/",
            &[provider::FETCH_WRAPPER_JS.to_string()],
        )
        .await?;
        tracing::debug!("[shim] open_page target={target_id}: navigated, waiting composer");
        let surface = PageSurface { cdp, chat, tap };
        let _ = surface.chat.wait_composer().await;
        tracing::debug!("[shim] open_page target={target_id}: composer ready");
        Ok(Arc::new(surface))
    }

    /// `shim._seed_binds_once()` — one-time, BEFORE the first subagent tab
    /// opens: explicitly bind every unbound chimera session that already exists
    /// to "main", so elimination binding can never hand MAIN's long-lived
    /// session to the first subagent.
    async fn seed_binds_once(&self) -> anyhow::Result<()> {
        {
            let mut seeded = self.bind_seeded.lock().await;
            if *seeded {
                return Ok(());
            }
            *seeded = true;
        }
        let data = self.rail.get("/control/sessions").await;
        let unbound = data
            .as_ref()
            .and_then(|d| d.get("unbound"))
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        for s in unbound {
            let Some(tok) = s.get("session").and_then(Value::as_str) else {
                continue;
            };
            let fresh = self.bound_sessions.lock().await.insert(tok.to_string());
            if fresh {
                self.rail
                    .post(
                        "/control/bind",
                        &json!({"session": tok, "agent_id": "main"}),
                    )
                    .await;
                tracing::info!("[shim] claimed pre-existing session for main");
            }
        }
        Ok(())
    }

    /// `shim.enqueue_bind(thread_id)` — queue a freshly-tabbed non-main thread
    /// for elimination binding and make sure the watcher is running.
    async fn enqueue_bind(&self, thread_id: &str) {
        self.pending_bind
            .lock()
            .await
            .push(format!("codex:{thread_id}"));
        let mut task = self.bind_task.lock().await;
        let running = task.as_ref().map(|t| !t.is_finished()).unwrap_or(false);
        if !running {
            let rail = self.rail.clone();
            let pending = Arc::clone(&self.pending_bind);
            let bound = Arc::clone(&self.bound_sessions);
            *task = Some(tokio::spawn(bind_watcher(rail, pending, bound)));
        }
    }

    /// `shim.resolve_project(cwd, chat)` — map a repo cwd to a memory-off
    /// ChatGPT Project gizmo, creating + caching (file-backed) the first time a
    /// folder is seen.
    async fn resolve_project(
        &self,
        cwd: &str,
        chat: &Arc<dyn ChatSurface>,
    ) -> anyhow::Result<Option<String>> {
        // The cache mutex doubles as the Python's `_project_lock`.
        let mut cache = self.project_cache.lock().await;
        if let Some(giz) = cache.get(cwd) {
            return Ok(Some(giz.clone()));
        }
        let giz = chat.create_project(&project_name_for_cwd(cwd)).await?;
        if let Some(g) = &giz {
            cache.insert(cwd.to_string(), g.clone());
            // Best-effort persist (Python try/except: pass), indent=2 like
            // json.dump(..., indent=2).
            if let Ok(body) = serde_json::to_string_pretty(&*cache) {
                let _ = std::fs::write(&self.cache_path, body);
            }
            tracing::info!(
                "[shim] project for {cwd} -> {g} ({})",
                project_name_for_cwd(cwd)
            );
        }
        Ok(giz)
    }

    /// Tail chimera's `/events` SSE feed (the same stream the subagent driver
    /// consumes, via `provider::stream_server_events`) and ping `on_event` once
    /// per event: connector-tool liveness for the conductor's stall watchdog.
    ///
    /// Scoped to THIS conversation's agent (`/events?agent=<id>`): chimera
    /// stamps every event with the agent its session bound via repo_register
    /// ("main" / "codex:<thread>"), so only this conversation's tool activity
    /// resets this turn's watchdog — another thread's traffic can no longer
    /// mask a genuinely dead, rate-limited turn for up to max_minutes.
    /// A down/unreachable chimera ends the tail immediately and silently (one
    /// debug line), degrading to the WS-only watchdog. No reconnects: each
    /// fresh turn spawns a fresh tail.
    fn spawn_server_events_tail(
        &self,
        agent: &str,
        on_event: ServerEventCallback,
    ) -> Option<tokio::task::JoinHandle<()>> {
        let url = self.cfg.server_url.clone();
        let agent = agent.to_string();
        Some(tokio::spawn(async move {
            let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Value>();
            // Tie the inner SSE-tail task's lifetime to THIS task: when the
            // conductor aborts us at turn end, the guard drops and aborts it too.
            let _tail = AbortOnDrop(provider::stream_server_events(url, Some(agent), tx));
            while rx.recv().await.is_some() {
                on_event();
            }
            tracing::debug!("[shim] /events liveness tail ended (chimera down or stream closed)");
        }))
    }
}

/// Abort a spawned task when dropped: ties a helper task's lifetime to the task
/// holding the guard, so one `JoinHandle::abort` tears both down.
struct AbortOnDrop(tokio::task::JoinHandle<()>);

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// `shim._bind_watcher` — elimination binding: each NEW unbound chimera session
/// binds to the next pending `codex:<T>` in tab arrival order. If the model
/// self-bound (the injected repo_register directive), the pending entry shows
/// up in `bound` and is dropped instead.
async fn bind_watcher(
    rail: ChimeraRail,
    pending: Arc<tokio::sync::Mutex<Vec<String>>>,
    bound_sessions: Arc<tokio::sync::Mutex<HashSet<String>>>,
) {
    loop {
        if pending.lock().await.is_empty() {
            tokio::time::sleep(Duration::from_secs(1)).await;
            continue;
        }
        let Some(data) = rail.get("/control/sessions").await else {
            tokio::time::sleep(Duration::from_secs(1)).await;
            continue;
        };
        let bound_ids: HashSet<String> = data
            .get("bound")
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(|v| v.as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default();
        pending.lock().await.retain(|a| !bound_ids.contains(a));
        // oldest-first within a poll: lastSeq orders new one-event sessions by
        // their first call's arrival.
        let mut unbound: Vec<Value> = data
            .get("unbound")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        unbound.sort_by(|a, b| {
            let ka = a.get("lastSeq").and_then(Value::as_f64).unwrap_or(0.0);
            let kb = b.get("lastSeq").and_then(Value::as_f64).unwrap_or(0.0);
            ka.partial_cmp(&kb).unwrap_or(std::cmp::Ordering::Equal)
        });
        for s in unbound {
            let Some(tok) = s.get("session").and_then(Value::as_str) else {
                continue;
            };
            if bound_sessions.lock().await.contains(tok) {
                continue;
            }
            let agent_id = {
                let mut p = pending.lock().await;
                if p.is_empty() {
                    break;
                }
                p.remove(0)
            };
            bound_sessions.lock().await.insert(tok.to_string());
            let ok = rail
                .post(
                    "/control/bind",
                    &json!({"session": tok, "agent_id": agent_id}),
                )
                .await;
            tracing::info!(
                "[shim] bound chimera session -> {agent_id}{}",
                if ok.is_some() { "" } else { " (POST failed)" }
            );
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
}

// ===== ConductorMux ==========================================================

/// The per-thread router (`get_conductor` + the `/control/*` lookup), installed
/// into [`crate::responses::ShadowResponsesShim`] as its [`TurnDriver`].
pub struct ConductorMux {
    /// The runtime, conductor-facing (handed to every `ThreadConductor::new`).
    shim: Arc<dyn ConductorShim>,
    /// The same runtime object, router-facing (main-thread bookkeeping).
    registry: Arc<dyn MainRegistry>,
    cfg: ShadowConfig,
    model: String,
    effort: Option<String>,
    /// `self._threads` (+ `_threads_lock`). Sync mutex: get-or-create holds it
    /// only across map ops, never across an await.
    threads: std::sync::Mutex<HashMap<String, Arc<ThreadConductor>>>,
    /// `self._eager_main` — a pre-booted tab from eager boot; the first
    /// non-subagent thread-id adopts it so single-agent never opens two tabs.
    eager_main: std::sync::Mutex<Option<Arc<ThreadConductor>>>,
}

impl ConductorMux {
    /// `ShadowResponsesShim.__init__`'s router half. `shim` is the runtime
    /// (typically [`ShimRuntime`]; tests pass a mock implementing both traits).
    pub fn new<S>(shim: Arc<S>, cfg: ShadowConfig, model: String, effort: Option<String>) -> Self
    where
        S: ConductorShim + MainRegistry + 'static,
    {
        Self {
            shim: shim.clone(),
            registry: shim,
            cfg,
            model,
            effort,
            threads: std::sync::Mutex::new(HashMap::new()),
            eager_main: std::sync::Mutex::new(None),
        }
    }

    fn new_conductor(&self, key: &str) -> Arc<ThreadConductor> {
        Arc::new(ThreadConductor::new(
            self.shim.clone(),
            self.cfg.clone(),
            key,
            self.model.clone(),
            self.effort.clone(),
        ))
    }

    /// `get_conductor(thread_id, subagent_kind)` — return (creating if needed)
    /// the conductor for a thread-id. The first NON-subagent thread becomes MAIN
    /// and adopts any eager-booted tab; a subagent thread never steals that tab
    /// and is never marked MAIN.
    pub async fn get_conductor(
        &self,
        thread_id: Option<&str>,
        subagent_kind: Option<&str>,
    ) -> Arc<ThreadConductor> {
        // Python truthiness: an absent OR empty header falls back / is ignored.
        let base = thread_id.filter(|t| !t.is_empty()).unwrap_or("default");
        let subagent_kind = subagent_kind.filter(|s| !s.is_empty());
        // codex reuses the SESSION's thread-id for background subagent requests
        // (x-openai-subagent: memory_consolidation / compact / review). Keying by
        // thread-id alone would land those in the SAME tab+conversation as the
        // interactive session — and seed the main conductor as a subagent — so
        // the key carries the kind.
        let key = match subagent_kind {
            Some(kind) => format!("{base}#{kind}"),
            None => base.to_string(),
        };

        let tc = {
            let mut threads = self.threads.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(tc) = threads.get(&key) {
                tc.clone()
            } else {
                let tc = if subagent_kind.is_none() {
                    match self
                        .eager_main
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .take()
                    {
                        Some(eager) => {
                            // Rebind the pre-booted eager tab to this (the first
                            // main) thread-id.
                            eager.rebind_thread_id(&key);
                            eager
                        }
                        None => self.new_conductor(&key),
                    }
                } else {
                    self.new_conductor(&key)
                };
                threads.insert(key.clone(), tc.clone());
                if subagent_kind.is_none() {
                    // `if self._main_thread_id is None and not subagent_kind`.
                    self.registry.register_main_thread(&key);
                }
                tc
            }
        };
        // Existing conductor: `if subagent_kind and not tc.subagent_kind`;
        // fresh conductor: the unconditional `tc.subagent_kind = subagent_kind`.
        // `set_subagent_kind` (set-if-none) covers both.
        if let Some(kind) = subagent_kind {
            tc.set_subagent_kind(Some(kind.to_string())).await;
        }
        tc
    }

    /// `conductor_for_control` — resolve a `/control` request to a conductor.
    /// An explicit thread_id wins; absent one, fall back to MAIN. Tolerates the
    /// `codex:<T>` binding form.
    pub fn conductor_for_control(&self, thread_id: Option<&str>) -> Option<Arc<ThreadConductor>> {
        let key = match thread_id.filter(|t| !t.is_empty()) {
            Some(t) => t.to_string(),
            None => self.registry.main_thread_sync()?,
        };
        let key = key.strip_prefix("codex:").unwrap_or(&key).to_string();
        self.threads
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&key)
            .cloned()
    }

    /// `await_control_conductor` — a chimera tool call can land a beat before
    /// codex's Responses request has registered the conductor; poll briefly
    /// rather than racing to a spurious 409.
    pub async fn await_control_conductor(
        &self,
        thread_id: Option<&str>,
        timeout: Duration,
    ) -> Option<Arc<ThreadConductor>> {
        let deadline = tokio::time::Instant::now() + timeout;
        while tokio::time::Instant::now() < deadline {
            if let Some(tc) = self.conductor_for_control(thread_id) {
                return Some(tc);
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        None
    }

    /// `shim.boot()` — eager pre-boot of one MAIN tab (the live stack runs
    /// non-lazy): opens an armed tab now so the first codex turn is fast and
    /// login issues surface early. The first request's thread-id adopts it via
    /// [`Self::get_conductor`].
    pub async fn eager_boot(&self) -> anyhow::Result<()> {
        self.shim.ensure_tabs().await?;
        let tc = self.new_conductor("__eager_main__");
        tc.boot().await?;
        *self.eager_main.lock().unwrap_or_else(|e| e.into_inner()) = Some(tc);
        Ok(())
    }
}

#[async_trait]
impl TurnDriver for ConductorMux {
    /// Route to the thread's conductor and delegate to ITS `TurnDriver` impl
    /// (which seeds the subagent kind, boots the tab, and serializes the turn).
    async fn collect_turn(
        &self,
        thread_id: Option<&str>,
        subagent_kind: Option<&str>,
        body: &Value,
        inject_text: &str,
    ) -> anyhow::Result<String> {
        let tc = self.get_conductor(thread_id, subagent_kind).await;
        tc.collect_turn(thread_id, subagent_kind, body, inject_text)
            .await
    }

    /// Per-thread injection: the `protocol_sent` preamble latch lives in the
    /// conductor, so the conductor's own `build_injection` is used whenever the
    /// conductor exists (it always does on the live path — `prepare_turn`
    /// registers it before any injection is built). The stateless default is
    /// only the pre-registration fallback.
    fn build_injection(&self, thread_id: Option<&str>, body: &Value, tools: &[Value]) -> String {
        let key = thread_id.filter(|t| !t.is_empty()).unwrap_or("default");
        let tc = self
            .threads
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(key)
            .cloned();
        match tc {
            Some(tc) => tc.build_injection(thread_id, body, tools),
            None => crate::responses::default_build_injection(body, tools, &mut false),
        }
    }

    /// The Python `responses` handler's pre-stream step: `get_conductor` (which
    /// registers the thread + main bookkeeping) then `ensure_booted()` unless
    /// SHIM_NO_BROWSER. A boot error here becomes the 502 JSON.
    async fn prepare_turn(
        &self,
        thread_id: Option<&str>,
        subagent_kind: Option<&str>,
    ) -> anyhow::Result<()> {
        let tc = self.get_conductor(thread_id, subagent_kind).await;
        let no_browser = std::env::var("SHIM_NO_BROWSER")
            .map(|v| !v.is_empty())
            .unwrap_or(false);
        if no_browser {
            return Ok(());
        }
        tc.ensure_booted().await
    }

    /// `tc.run_turn(resp, body)` — the conductor owns the whole turn (including
    /// the SHIM_CONDUCTOR dispatch and its own per-thread `_build_injection`).
    async fn drive_turn(
        &self,
        thread_id: Option<&str>,
        subagent_kind: Option<&str>,
        tx: &tokio::sync::mpsc::Sender<String>,
        body: &Value,
    ) -> anyhow::Result<()> {
        let tc = self.get_conductor(thread_id, subagent_kind).await;
        tc.run_turn(tx, body).await
    }

    /// `shim._booted` — any thread booted, or an eager-main tab pre-booted.
    async fn booted(&self) -> bool {
        let eager = self
            .eager_main
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        if let Some(tc) = eager {
            if tc.is_booted().await {
                return true;
            }
        }
        let tcs: Vec<Arc<ThreadConductor>> = self
            .threads
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .values()
            .cloned()
            .collect();
        for tc in tcs {
            if tc.is_booted().await {
                return true;
            }
        }
        false
    }

    /// The `/control/toolcall` handler body, scoped to the routed conductor:
    /// dedupe/soft-redeliver/hard-stop via `ThreadConductor::control_toolcall`,
    /// then hold the request until codex executed the fresh call (bounded by
    /// the blocking-tool timeout policy) and cache the result.
    async fn route_control_toolcall(&self, data: &Value) -> (u16, Value) {
        let thread_id = data.get("thread_id").and_then(Value::as_str);
        let Some(tc) = self
            .await_control_conductor(thread_id, Duration::from_secs(60))
            .await
        else {
            return (409, json!({"error": "no active conductor for thread"}));
        };

        let name = data.get("name").and_then(Value::as_str).unwrap_or("");
        // Python: `args = data.get("arguments") or {}` (falsy -> {}).
        let raw_args = data.get("arguments").cloned().unwrap_or(Value::Null);
        let args = if crate::responses::is_py_falsy(&raw_args) {
            json!({})
        } else {
            raw_args
        };
        // Python: `call_id = data.get("call_id") or "call_" + uuid4().hex`.
        let call_id = data
            .get("call_id")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .unwrap_or_else(|| format!("call_{}", uuid::Uuid::new_v4().simple()));
        // Python: `kind = data.get("kind") or "function"` — the conductor maps
        // an empty kind to "function".
        let kind = data.get("kind").and_then(Value::as_str).unwrap_or("");
        let input = data.get("input").and_then(Value::as_str).unwrap_or("");

        match tc
            .control_toolcall(name, kind, &args, input, &call_id)
            .await
        {
            ControlToolcall::Recovered(body) => (200, body),
            ControlToolcall::Dispatched {
                sig,
                call_id,
                result_rx,
                timeout_secs,
            } => {
                match tokio::time::timeout(Duration::from_secs(timeout_secs), result_rx).await {
                    Ok(Ok(out)) => {
                        tc.record_tool_result(&sig, &out).await;
                        (200, json!({"output": out, "call_id": call_id}))
                    }
                    // Deadline, or the conductor was torn down (sender dropped) —
                    // the Python's wait_for times out in both cases. Drop the
                    // parked future too: chimera already saw the 504, so a late
                    // resolution can't reach it, and a stale armed inflight
                    // would misroute the next tool-result turn.
                    _ => {
                        tc.clear_inflight_call().await;
                        (504, json!({"error": "timeout waiting for codex execution"}))
                    }
                }
            }
        }
    }

    /// The `/control/turn_complete` handler body: resolve the conductor, end the
    /// active codex turn with the final text.
    async fn route_control_turn_complete(&self, data: &Value) -> (u16, Value) {
        let text = data.get("text").and_then(Value::as_str).unwrap_or("");
        let thread_id = data.get("thread_id").and_then(Value::as_str);
        if let Some(tc) = self
            .await_control_conductor(thread_id, Duration::from_secs(60))
            .await
        {
            if tc.control_turn_complete(text).await {
                return (200, json!({"ok": true}));
            }
        }
        (409, json!({"ok": false, "error": "no active turn"}))
    }
}

// ===== tests =================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicU32;
    use std::sync::atomic::Ordering;
    use tokio::sync::mpsc;
    use tokio::time::timeout;

    /// Serializes tests that mutate `SHIM_TURN_STALL_SECS` (process-global env):
    /// without this, one test's `remove_var` can race another's `set_var` and
    /// flip the other's stall budget back to 90s mid-test.
    static STALL_ENV: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// Mock runtime: records tab/bind traffic; `open_page` hands out a no-op
    /// page surface so `ThreadConductor::boot` runs fully offline.
    struct MockShim {
        main: std::sync::Mutex<Option<String>>,
        tab_seq: AtomicU32,
        binds: std::sync::Mutex<Vec<String>>,
        seeds: AtomicU32,
        /// Mock chimera /events feed: when non-zero, `spawn_server_events_tail`
        /// pings `on_event` every this-many milliseconds (a connector tool
        /// "running" forever). Zero = no feed, the trait default.
        tail_ms: AtomicU32,
        /// The agent id the conductor asked the /events tail to scope to.
        tail_agent: std::sync::Mutex<Option<String>>,
        /// The conductor's WS callback, captured at `open_page` so tests can
        /// simulate tap events (tokens / turn_complete / typed errors).
        on_ws: std::sync::Mutex<Option<WsCallback>>,
        /// Every text injected into the mock chat, in order.
        injects: Arc<std::sync::Mutex<Vec<String>>>,
        /// Every `set_overrides` call against the mock chat, in order — so tests
        /// can assert which model/effort the conductor pinned each turn.
        overrides: Arc<std::sync::Mutex<Vec<ChatOverrides>>>,
        /// The `generating` flag MockChat.state() reports. Defaults to TRUE (a
        /// turn in progress) so the stall watchdog's "is it still generating?"
        /// poll exercises the abort path; a test flips it false to simulate
        /// ChatGPT finishing without the tap forwarding a completion.
        generating: Arc<std::sync::atomic::AtomicBool>,
    }

    impl MockShim {
        fn new() -> Arc<Self> {
            Arc::new(Self {
                main: std::sync::Mutex::new(None),
                tab_seq: AtomicU32::new(0),
                binds: std::sync::Mutex::new(Vec::new()),
                seeds: AtomicU32::new(0),
                tail_ms: AtomicU32::new(0),
                tail_agent: std::sync::Mutex::new(None),
                on_ws: std::sync::Mutex::new(None),
                injects: Arc::new(std::sync::Mutex::new(Vec::new())),
                overrides: Arc::new(std::sync::Mutex::new(Vec::new())),
                generating: Arc::new(std::sync::atomic::AtomicBool::new(true)),
            })
        }

        /// The captured tap callback (panics if `boot` hasn't opened a page).
        fn ws(&self) -> WsCallback {
            self.on_ws
                .lock()
                .unwrap()
                .clone()
                .expect("open_page captured the WS callback")
        }

        fn injected(&self) -> Vec<String> {
            self.injects.lock().unwrap().clone()
        }

        fn overrides(&self) -> Vec<ChatOverrides> {
            self.overrides.lock().unwrap().clone()
        }

        /// Simulate ChatGPT finishing (or still generating) for the stall poll.
        fn set_generating(&self, on: bool) {
            self.generating.store(on, Ordering::SeqCst);
        }
    }

    impl MainRegistry for MockShim {
        fn register_main_thread(&self, thread_id: &str) {
            let mut g = self.main.lock().unwrap();
            if g.is_none() {
                *g = Some(thread_id.to_string());
            }
        }
        fn main_thread_sync(&self) -> Option<String> {
            self.main.lock().unwrap().clone()
        }
    }

    #[async_trait]
    impl ConductorShim for MockShim {
        async fn main_thread_id(&self) -> Option<String> {
            self.main_thread_sync()
        }
        async fn ensure_tabs(&self) -> anyhow::Result<()> {
            Ok(())
        }
        async fn open_tab(
            &self,
            _url: &str,
            _agent_id: Option<&str>,
            _human: bool,
        ) -> anyhow::Result<String> {
            let n = self.tab_seq.fetch_add(1, Ordering::SeqCst);
            Ok(format!("tab-{n}"))
        }
        async fn close_tab(&self, _target_id: &str) {}
        async fn open_page(
            &self,
            _target_id: &str,
            on_ws: WsCallback,
        ) -> anyhow::Result<Arc<dyn ChatSurface>> {
            *self.on_ws.lock().unwrap() = Some(on_ws);
            Ok(Arc::new(MockChat {
                injects: Arc::clone(&self.injects),
                overrides: Arc::clone(&self.overrides),
                generating: Arc::clone(&self.generating),
            }))
        }
        async fn seed_binds_once(&self) -> anyhow::Result<()> {
            self.seeds.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
        async fn enqueue_bind(&self, thread_id: &str) {
            self.binds.lock().unwrap().push(thread_id.to_string());
        }
        async fn resolve_project(
            &self,
            _cwd: &str,
            _chat: &Arc<dyn ChatSurface>,
        ) -> anyhow::Result<Option<String>> {
            Ok(None)
        }
        /// Channel-driven stand-in for the chimera /events SSE tail: a ticking
        /// task pings `on_event` (a connector tool alive server-side) so tests
        /// can exercise the stall watchdog without any HTTP. `tail_ms == 0`
        /// keeps the trait default (no feed). Records the agent scope.
        fn spawn_server_events_tail(
            &self,
            agent: &str,
            on_event: ServerEventCallback,
        ) -> Option<tokio::task::JoinHandle<()>> {
            *self.tail_agent.lock().unwrap() = Some(agent.to_string());
            let ms = self.tail_ms.load(Ordering::SeqCst);
            if ms == 0 {
                return None;
            }
            Some(tokio::spawn(async move {
                loop {
                    tokio::time::sleep(Duration::from_millis(ms as u64)).await;
                    on_event();
                }
            }))
        }
    }

    struct MockChat {
        injects: Arc<std::sync::Mutex<Vec<String>>>,
        overrides: Arc<std::sync::Mutex<Vec<ChatOverrides>>>,
        generating: Arc<std::sync::atomic::AtomicBool>,
    }

    #[async_trait]
    impl ChatSurface for MockChat {
        async fn inject(&self, text: &str) -> anyhow::Result<()> {
            self.injects.lock().unwrap().push(text.to_string());
            Ok(())
        }
        async fn state(&self) -> anyhow::Result<Value> {
            let generating = self.generating.load(Ordering::SeqCst);
            Ok(json!({"generating": generating, "composerReady": !generating, "hasApprove": false}))
        }
        async fn approve(&self) -> anyhow::Result<()> {
            Ok(())
        }
        async fn stop(&self) -> anyhow::Result<()> {
            Ok(())
        }
        async fn resolve_slug(&self, model: &str) -> anyhow::Result<Option<String>> {
            // Echo the spec back so boot pins a concrete model (matches the real
            // adapter closely enough for assertions; the conductor passes the raw
            // codex spec to set_overrides regardless).
            Ok(Some(model.to_string()))
        }
        async fn set_overrides(&self, ov: ChatOverrides) -> anyhow::Result<()> {
            self.overrides.lock().unwrap().push(ov);
            Ok(())
        }
        async fn current_conversation_id(&self) -> anyhow::Result<Option<String>> {
            Ok(None) // fresh tab: no resumed conversation
        }
        async fn create_project(&self, _name: &str) -> anyhow::Result<Option<String>> {
            Ok(None)
        }
        async fn wait_composer(&self) -> anyhow::Result<()> {
            Ok(())
        }
    }

    fn mux_with(shim: Arc<MockShim>) -> Arc<ConductorMux> {
        Arc::new(ConductorMux::new(
            shim,
            ShadowConfig::default(),
            "gpt-5-5-thinking".to_string(),
            Some("extended".to_string()),
        ))
    }

    // ----- get_conductor routing (python get_conductor semantics) -----

    #[tokio::test]
    async fn same_thread_id_returns_same_conductor() {
        let mux = mux_with(MockShim::new());
        let a1 = mux.get_conductor(Some("A"), None).await;
        let a2 = mux.get_conductor(Some("A"), None).await;
        let b = mux.get_conductor(Some("B"), None).await;
        assert!(Arc::ptr_eq(&a1, &a2));
        assert!(!Arc::ptr_eq(&a1, &b));
    }

    #[tokio::test]
    async fn missing_or_empty_thread_id_maps_to_default() {
        // python: key = thread_id or "default" (empty string is falsy).
        let mux = mux_with(MockShim::new());
        let none = mux.get_conductor(None, None).await;
        let empty = mux.get_conductor(Some(""), None).await;
        assert!(Arc::ptr_eq(&none, &empty));
        assert_eq!(none.thread_id(), "default");
    }

    #[tokio::test]
    async fn subagent_kind_partitions_the_thread_id() {
        // codex reuses the SESSION's thread-id for background subagent requests
        // (only x-openai-subagent differs), so the kind is part of the key:
        // each kind gets its OWN conductor/tab/conversation.
        let mux = mux_with(MockShim::new());
        let review = mux.get_conductor(Some("S"), Some("review")).await;
        assert_eq!(review.subagent_kind().await.as_deref(), Some("review"));
        assert_eq!(review.thread_id(), "S#review");
        // Same (thread, kind) -> same conductor.
        let review2 = mux.get_conductor(Some("S"), Some("review")).await;
        assert!(Arc::ptr_eq(&review, &review2));
        // Different kind on the same thread-id -> a different conductor.
        let compact = mux.get_conductor(Some("S"), Some("compact")).await;
        assert!(!Arc::ptr_eq(&review, &compact));
        assert_eq!(compact.subagent_kind().await.as_deref(), Some("compact"));
        // The kind-less (interactive) request is its own conductor too.
        let main = mux.get_conductor(Some("S"), None).await;
        assert!(!Arc::ptr_eq(&review, &main));
        assert!(!Arc::ptr_eq(&compact, &main));
        assert_eq!(main.subagent_kind().await, None);
        assert_eq!(main.thread_id(), "S");
    }

    #[tokio::test]
    async fn memory_consolidation_first_does_not_hijack_the_session() {
        // Regression: codex 0.137 fires a memory_consolidation request at session
        // start with the SAME thread-id as the interactive session. It must not
        // claim the conductor, seed it as a subagent, or block MAIN registration.
        let shim = MockShim::new();
        let mux = mux_with(shim.clone());
        let memory = mux
            .get_conductor(Some("T"), Some("memory_consolidation"))
            .await;
        assert_eq!(shim.main_thread_sync(), None, "background task is not MAIN");
        let session = mux.get_conductor(Some("T"), None).await;
        assert!(
            !Arc::ptr_eq(&memory, &session),
            "interactive session must not share the memory task's conversation"
        );
        assert_eq!(session.subagent_kind().await, None);
        assert_eq!(shim.main_thread_sync().as_deref(), Some("T"));
    }

    #[tokio::test]
    async fn stalled_turn_aborts_instead_of_hanging() {
        // Watchdog regression: a ChatGPT turn that produces NO stream activity
        // (the rate-limit failure mode) must abort with an error, not hang forever.
        // MockChat never feeds the WS tap AND the mock /events feed is off
        // (tail_ms == 0), so the turn is permanently silent on BOTH liveness
        // channels. With a 1s stall budget, run_turn_conductor must return Err
        // well before the 10s outer timeout (which would itself fail the test,
        // proving a hang).
        let _env = STALL_ENV.lock().unwrap_or_else(|e| e.into_inner());
        std::env::set_var("SHIM_TURN_STALL_SECS", "1");
        let mux = mux_with(MockShim::new());
        let c = mux.get_conductor(Some("stall"), None).await;
        c.boot().await.expect("mock boot");
        let (tx, _rx) = mpsc::channel::<String>(256);
        let body = json!({
            "input": [{"role": "user", "content": [{"type": "input_text", "text": "hi"}]}],
            "tools": [],
        });
        let outcome = timeout(
            std::time::Duration::from_secs(10),
            c.run_turn_conductor(&tx, &body),
        )
        .await;
        std::env::remove_var("SHIM_TURN_STALL_SECS");
        let res = outcome.expect("turn must return (no infinite hang) within 10s");
        let err = res.expect_err("a silent ChatGPT turn must abort with an error");
        let msg = err.to_string();
        assert!(
            msg.contains("no output") && msg.contains("connector-tool"),
            "stall error should name both silent channels: {msg}"
        );
    }

    /// End-of-turn regression: a long tool-heavy turn hands its stream off to the
    /// shared-worker socket, so the final answer + completion arrive INVISIBLE to
    /// the CDP tap — the turn is DONE but no turn_complete is ever forwarded.
    /// The watchdog must notice ChatGPT stopped GENERATING and close the turn
    /// CLEANLY (Ok), not burn the budget and abort (which forced codex into a
    /// stall-retry loop after the work was already done).
    #[tokio::test]
    async fn missed_completion_closes_cleanly_when_generation_stops() {
        let _env = STALL_ENV.lock().unwrap_or_else(|e| e.into_inner());
        std::env::set_var("SHIM_TURN_STALL_SECS", "1"); // poll = min(15s, 1s) = 1s
        let shim = MockShim::new();
        let mux = mux_with(shim.clone());
        let c = mux.get_conductor(Some("missed"), None).await;
        c.boot().await.expect("mock boot");
        let (tx, _rx) = mpsc::channel::<String>(256);
        let body = json!({"input": "hi", "tools": []});
        let c1 = c.clone();
        let b1 = body.clone();
        let turn = tokio::spawn(async move { c1.run_turn_conductor(&tx, &b1).await });
        tokio::time::sleep(std::time::Duration::from_millis(150)).await;
        // Some visible text streamed, then ChatGPT finishes — but the tap never
        // forwards turn_complete (the handoff). Generation halts.
        let ws = shim.ws();
        ws("token", "partial visible answer");
        shim.set_generating(false);
        let outcome = timeout(std::time::Duration::from_secs(8), turn).await;
        std::env::remove_var("SHIM_TURN_STALL_SECS");
        let res = outcome
            .expect("turn must close within 8s, not hang")
            .expect("join");
        assert!(
            res.is_ok(),
            "a finished turn whose completion the tap missed must close CLEANLY, got: {res:?}"
        );
    }

    #[tokio::test]
    async fn connector_tool_keepalives_hold_off_stall_watchdog() {
        // Incident regression: ChatGPT blocked on a long/hung chimera-local MCP
        // connector tool streams NO tokens, but chimera's /events feed shows the
        // tool activity. Those events must keep resetting the stall watchdog so
        // the turn is NOT aborted as "rate-limited". MockChat never feeds the WS
        // tap (token-silent, exactly like the incident); the mock /events tail
        // pings every 250ms. With a 1s stall budget the turn must still be alive
        // at 3s — the outer timeout elapsing (Err) is the success signal, while
        // an early Ok(Err(stall)) is the false positive this fix removes.
        let _env = STALL_ENV.lock().unwrap_or_else(|e| e.into_inner());
        std::env::set_var("SHIM_TURN_STALL_SECS", "1");
        let shim = MockShim::new();
        shim.tail_ms.store(250, Ordering::SeqCst);
        let mux = mux_with(shim);
        let c = mux.get_conductor(Some("connector"), None).await;
        c.boot().await.expect("mock boot");
        let (tx, _rx) = mpsc::channel::<String>(256);
        let body = json!({
            "input": [{"role": "user", "content": [{"type": "input_text", "text": "hi"}]}],
            "tools": [],
        });
        let outcome = timeout(
            std::time::Duration::from_secs(3),
            c.run_turn_conductor(&tx, &body),
        )
        .await;
        std::env::remove_var("SHIM_TURN_STALL_SECS");
        assert!(
            outcome.is_err(),
            "a token-silent turn with connector-tool /events activity must \
survive past the stall budget, got: {outcome:?}"
        );
    }

    #[tokio::test]
    async fn events_tail_is_scoped_to_this_conversations_agent() {
        let _env = STALL_ENV.lock().unwrap_or_else(|e| e.into_inner());
        std::env::set_var("SHIM_TURN_STALL_SECS", "1");
        let shim = MockShim::new();
        let mux = ConductorMux::new(shim.clone(), ShadowConfig::default(),
            "gpt-5-5-thinking".to_string(), Some("extended".to_string()));
        // MAIN thread -> agent "main".
        let main = mux.get_conductor(Some("root"), None).await;
        main.boot().await.expect("mock boot");
        let (tx, _rx) = mpsc::channel::<String>(256);
        let body = json!({"input": "hi", "tools": []});
        let _ = timeout(
            std::time::Duration::from_secs(3),
            main.run_turn_conductor(&tx, &body),
        )
        .await;
        assert_eq!(shim.tail_agent.lock().unwrap().as_deref(), Some("main"));
        // A non-main thread -> the bound "codex:<thread>" identity.
        let sub = mux.get_conductor(Some("T2"), None).await;
        sub.boot().await.expect("mock boot");
        let (tx2, _rx2) = mpsc::channel::<String>(256);
        let _ = timeout(
            std::time::Duration::from_secs(3),
            sub.run_turn_conductor(&tx2, &body),
        )
        .await;
        std::env::remove_var("SHIM_TURN_STALL_SECS");
        assert_eq!(
            shim.tail_agent.lock().unwrap().as_deref(),
            Some("codex:T2"),
            "non-main threads scope their /events tail to codex:<thread>"
        );
    }

    /// Resolve the active turn with `text`, polling until the turn-done future
    /// exists (the spawned request may not have registered it yet).
    async fn complete_turn(c: &Arc<ThreadConductor>, text: &str) {
        for _ in 0..200 {
            if c.control_turn_complete(text).await {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        panic!("no active turn to complete");
    }

    /// L1: two concurrent codex requests for ONE thread must serialize on the
    /// conductor's turn_lock — the second emits NOTHING until the first turn
    /// completed, and each stream carries only its own turn's text.
    #[tokio::test]
    async fn concurrent_turns_on_one_thread_serialize() {
        let mux = mux_with(MockShim::new());
        // No boot: test mode (no chat surface), turns are driven via
        // control_turn_complete exactly like the round-trip test.
        let c = mux.get_conductor(Some("ser"), None).await;
        let body = json!({"input": "go", "tools": []});

        let (tx1, mut rx1) = mpsc::channel::<String>(256);
        let c1 = c.clone();
        let b1 = body.clone();
        let t1 = tokio::spawn(async move { c1.run_turn_conductor(&tx1, &b1).await });
        // Turn A is in flight once its response.created lands.
        let first = timeout(std::time::Duration::from_secs(5), rx1.recv())
            .await
            .expect("turn A starts")
            .expect("created frame");
        assert!(first.contains("response.created"));

        let (tx2, mut rx2) = mpsc::channel::<String>(256);
        let c2 = c.clone();
        let b2 = body.clone();
        let t2 = tokio::spawn(async move { c2.run_turn_conductor(&tx2, &b2).await });
        // While A holds the turn, B must not have started streaming.
        let blocked = timeout(std::time::Duration::from_millis(300), rx2.recv()).await;
        assert!(
            blocked.is_err(),
            "second request must block on the turn lock, got: {blocked:?}"
        );

        // Finish A; B may then proceed and finish.
        complete_turn(&c, "answer A").await;
        t1.await.unwrap().expect("turn A ok");
        let b_created = timeout(std::time::Duration::from_secs(5), rx2.recv())
            .await
            .expect("turn B proceeds after A")
            .expect("created frame");
        assert!(b_created.contains("response.created"));
        complete_turn(&c, "answer B").await;
        t2.await.unwrap().expect("turn B ok");

        // No crossed streams: A's channel never saw B's text and vice versa.
        let mut a_frames = String::new();
        while let Ok(f) = rx1.try_recv() {
            a_frames.push_str(&f);
        }
        let mut b_frames = String::new();
        while let Ok(f) = rx2.try_recv() {
            b_frames.push_str(&f);
        }
        assert!(a_frames.contains("answer A") && !a_frames.contains("answer B"));
        assert!(b_frames.contains("answer B") && !b_frames.contains("answer A"));
    }

    /// L3b: a typed error event from the tap fails the turn IMMEDIATELY (no
    /// 90s stall) with the classified codex-facing code on the error.
    #[tokio::test]
    async fn stream_error_event_fails_turn_immediately() {
        // Default 90s stall budget on purpose: finishing fast PROVES the error
        // path (not the watchdog) ended the turn.
        let shim = MockShim::new();
        let mux = mux_with(shim.clone());
        let c = mux.get_conductor(Some("err"), None).await;
        c.boot().await.expect("mock boot");
        let body = json!({"input": "hi", "tools": []});

        // Moderation-looking error -> FATAL invalid_prompt.
        let (tx, _rx) = mpsc::channel::<String>(256);
        let c1 = c.clone();
        let b1 = body.clone();
        let turn = tokio::spawn(async move { c1.run_turn_conductor(&tx, &b1).await });
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        let ws = shim.ws();
        ws(
            "error",
            r#"{"etype":"moderation_error","code":"","message":"blocked for safety"}"#,
        );
        let res = timeout(std::time::Duration::from_secs(5), turn)
            .await
            .expect("turn must fail fast, not stall")
            .unwrap();
        let err = res.expect_err("stream error must fail the turn");
        let tf = err
            .downcast_ref::<crate::conductor::TurnFailed>()
            .expect("typed TurnFailed");
        assert_eq!(tf.code, "invalid_prompt");
        assert!(tf.message.contains("blocked for safety"));

        // Rate-limit-looking error -> retryable shim_error with the hint intact.
        let (tx2, _rx2) = mpsc::channel::<String>(256);
        let c2 = c.clone();
        let b2 = body.clone();
        let turn2 = tokio::spawn(async move { c2.run_turn_conductor(&tx2, &b2).await });
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        let ws = shim.ws();
        ws(
            "error",
            r#"{"etype":"rate_limit_error","code":"","message":"try again in 30 seconds"}"#,
        );
        let res2 = timeout(std::time::Duration::from_secs(5), turn2)
            .await
            .expect("turn must fail fast")
            .unwrap();
        let err2 = res2.expect_err("stream error must fail the turn");
        let tf2 = err2
            .downcast_ref::<crate::conductor::TurnFailed>()
            .expect("typed TurnFailed");
        assert_eq!(tf2.code, "shim_error");
        assert!(tf2.message.contains("try again in 30 seconds"));
    }

    /// Regression: codex's per-request `body.model` (the live picker choice) must
    /// win over the launch default — INCLUDING the first turn, and again when the
    /// picker switches mid-session. Boot pins the launch model; the conductor must
    /// re-pin from the body before each inject. This is the "selected model
    /// doesn't apply / first turn applies no model" bug.
    #[tokio::test]
    async fn body_model_overrides_launch_model_each_turn() {
        let shim = MockShim::new();
        // launch default = gpt-5-5-thinking, effort extended (mux_with).
        let mux = mux_with(shim.clone());
        let c = mux.get_conductor(Some("model"), None).await;
        c.boot().await.expect("mock boot");
        let ws = shim.ws();

        let last_model = |s: &MockShim| -> Option<String> {
            s.overrides().iter().rev().find_map(|o| o.model.clone())
        };

        // Turn 1: the picker is on gpt-5-5-pro, even though boot pinned thinking.
        let body_pro = json!({
            "model": "gpt-5-5-pro",
            "input": [{"role": "user", "content": [{"type": "input_text", "text": "hi"}]}],
            "tools": [],
            "reasoning": {"effort": "high"},
        });
        let (tx, _rx) = mpsc::channel::<String>(256);
        let c1 = c.clone();
        let b1 = body_pro.clone();
        let t = tokio::spawn(async move { c1.run_turn_conductor(&tx, &b1).await });
        tokio::time::sleep(std::time::Duration::from_millis(150)).await;
        ws("token", "ok");
        ws("turn_complete", "ok");
        t.await.unwrap().expect("turn 1 ok");
        assert_eq!(
            last_model(&shim).as_deref(),
            Some("gpt-5-5-pro"),
            "turn 1 must pin the body model, not the launch default: {:?}",
            shim.overrides()
        );
        // effort high -> extended
        assert_eq!(
            shim.overrides()
                .iter()
                .rev()
                .find_map(|o| o.thinking_effort.clone())
                .as_deref(),
            Some("extended")
        );

        // Turn 2: the picker switches back to thinking — must re-pin, not stick.
        let body_thinking = json!({
            "model": "gpt-5-5-thinking",
            "input": [{"role": "user", "content": [{"type": "input_text", "text": "again"}]}],
            "tools": [],
            "reasoning": {"effort": "high"},
        });
        let (tx2, _rx2) = mpsc::channel::<String>(256);
        let c2 = c.clone();
        let b2 = body_thinking.clone();
        let t2 = tokio::spawn(async move { c2.run_turn_conductor(&tx2, &b2).await });
        tokio::time::sleep(std::time::Duration::from_millis(150)).await;
        ws("token", "ok");
        ws("turn_complete", "ok");
        t2.await.unwrap().expect("turn 2 ok");
        assert_eq!(
            last_model(&shim).as_deref(),
            Some("gpt-5-5-thinking"),
            "turn 2 must re-pin the new body model: {:?}",
            shim.overrides()
        );
    }

    /// L5: a restarted bridge gets the prior conversation replayed into the
    /// fresh chat on the FIRST injection only; later turns stay last-message.
    #[tokio::test]
    async fn restart_replays_context_on_first_injection_only() {
        let shim = MockShim::new();
        let mux = mux_with(shim.clone());
        let c = mux.get_conductor(Some("resume"), None).await;
        c.boot().await.expect("mock boot");
        let ws = shim.ws();

        // Turn 1 after restart: codex sends the FULL history (stateless).
        let body1 = json!({"input": [
            {"role": "user", "type": "message", "content": [{"type": "input_text", "text": "the codeword is BANANA"}]},
            {"role": "assistant", "type": "message", "content": [{"type": "output_text", "text": "noted"}]},
            {"role": "user", "type": "message", "content": [{"type": "input_text", "text": "what codeword?"}]},
        ], "tools": []});
        let (tx, _rx) = mpsc::channel::<String>(256);
        let c1 = c.clone();
        let t = tokio::spawn(async move { c1.run_turn_conductor(&tx, &body1).await });
        tokio::time::sleep(std::time::Duration::from_millis(150)).await;
        ws("token", "BANANA");
        ws("turn_complete", "BANANA");
        t.await.unwrap().expect("turn 1 ok");
        let injected = shim.injected();
        assert_eq!(injected.len(), 1);
        assert!(injected[0].contains("=== PRIOR CONVERSATION ==="), "replay present");
        assert!(injected[0].contains("USER: the codeword is BANANA"));
        assert!(injected[0].contains("ASSISTANT: noted"));
        assert!(injected[0].contains("Current message:\nwhat codeword?"));

        // Turn 2 (same conductor, conversation now warm): last message only.
        let body2 = json!({"input": [
            {"role": "user", "type": "message", "content": [{"type": "input_text", "text": "the codeword is BANANA"}]},
            {"role": "assistant", "type": "message", "content": [{"type": "output_text", "text": "noted"}]},
            {"role": "user", "type": "message", "content": [{"type": "input_text", "text": "what codeword?"}]},
            {"role": "assistant", "type": "message", "content": [{"type": "output_text", "text": "BANANA"}]},
            {"role": "user", "type": "message", "content": [{"type": "input_text", "text": "thanks"}]},
        ], "tools": []});
        let (tx2, _rx2) = mpsc::channel::<String>(256);
        let c2 = c.clone();
        let t2 = tokio::spawn(async move { c2.run_turn_conductor(&tx2, &body2).await });
        tokio::time::sleep(std::time::Duration::from_millis(150)).await;
        ws("token", "yw");
        ws("turn_complete", "yw");
        t2.await.unwrap().expect("turn 2 ok");
        let injected = shim.injected();
        assert_eq!(injected.len(), 2);
        assert_eq!(injected[1], "thanks", "warm conversation: last message only");
    }

    /// L7: an aborted turn (stall watchdog) clears the parked inflight call so
    /// the retry starts clean instead of firing a stale future.
    #[tokio::test]
    async fn stall_abort_clears_inflight_call() {
        let _env = STALL_ENV.lock().unwrap_or_else(|e| e.into_inner());
        let mux = mux_with(MockShim::new());
        let c = mux.get_conductor(Some("inflight"), None).await;
        // Slice 1 (test mode, no chat): chimera posts a tool call; the slice
        // emits the function_call and parks the future.
        let (tx1, _rx1) = mpsc::channel::<String>(64);
        let body = json!({"input": "do it", "tools": [{"type": "function"}]});
        let c1 = c.clone();
        let b1 = body.clone();
        let slice1 = tokio::spawn(async move { c1.run_turn_conductor(&tx1, &b1).await });
        let mux2 = mux.clone();
        let toolcall = tokio::spawn(async move {
            mux2.route_control_toolcall(&json!({
                "thread_id": "inflight", "name": "shell_command",
                "arguments": {"command": "echo hi"}, "call_id": "c1",
            }))
            .await
        });
        slice1.await.unwrap().expect("slice 1 dispatches the call");
        assert!(c.has_inflight_call().await, "future parked");

        // A fresh turn now stalls out (no liveness at all, 1s budget): the
        // abort must clear the stale inflight call.
        std::env::set_var("SHIM_TURN_STALL_SECS", "1");
        let (tx2, _rx2) = mpsc::channel::<String>(64);
        let fresh = json!({"input": "next task", "tools": []});
        let res = timeout(
            std::time::Duration::from_secs(10),
            c.run_turn_conductor(&tx2, &fresh),
        )
        .await
        .expect("no hang");
        std::env::remove_var("SHIM_TURN_STALL_SECS");
        res.expect_err("silent turn aborts");
        assert!(
            !c.has_inflight_call().await,
            "abort must clear the parked inflight call"
        );
        toolcall.abort(); // the held /control request is no longer relevant
    }

    #[tokio::test]
    async fn first_non_subagent_thread_becomes_main() {
        let shim = MockShim::new();
        let mux = mux_with(shim.clone());
        // A subagent-first arrival is never marked MAIN.
        mux.get_conductor(Some("sub1"), Some("review")).await;
        assert_eq!(shim.main_thread_sync(), None);
        // The first non-subagent thread becomes MAIN...
        mux.get_conductor(Some("root"), None).await;
        assert_eq!(shim.main_thread_sync().as_deref(), Some("root"));
        // ...and stays MAIN.
        mux.get_conductor(Some("late"), None).await;
        assert_eq!(shim.main_thread_sync().as_deref(), Some("root"));
    }

    #[tokio::test]
    async fn eager_main_is_adopted_by_first_main_thread_only() {
        let shim = MockShim::new();
        let mux = mux_with(shim.clone());
        mux.eager_boot().await.expect("offline eager boot");
        assert!(mux.booted().await, "eager tab counts as booted");

        // A subagent thread never steals the eager tab.
        let sub = mux.get_conductor(Some("S"), Some("review")).await;
        assert!(!sub.is_booted().await);

        // The first main thread adopts the pre-booted tab and is rebound to it.
        let main = mux.get_conductor(Some("T1"), None).await;
        assert!(main.is_booted().await, "adopted the eager pre-booted tab");
        assert_eq!(main.thread_id(), "T1");
        assert_eq!(shim.main_thread_sync().as_deref(), Some("T1"));

        // The eager slot is consumed: the next main-ish thread gets a fresh one.
        let other = mux.get_conductor(Some("T2"), None).await;
        assert!(!Arc::ptr_eq(&main, &other));
        assert!(!other.is_booted().await);
    }

    // ----- /control routing -----

    #[tokio::test]
    async fn control_lookup_falls_back_to_main_and_strips_codex_prefix() {
        let mux = mux_with(MockShim::new());
        assert!(mux.conductor_for_control(None).is_none()); // no main yet
        let root = mux.get_conductor(Some("root"), None).await;
        let by_default = mux.conductor_for_control(None).expect("falls back to MAIN");
        assert!(Arc::ptr_eq(&root, &by_default));
        let by_bind_form = mux
            .conductor_for_control(Some("codex:root"))
            .expect("codex:<T> binding form tolerated");
        assert!(Arc::ptr_eq(&root, &by_bind_form));
        assert!(mux.conductor_for_control(Some("nope")).is_none());
    }

    /// The full chimera round-trip on the REAL conductor path (the Python "test
    /// mode (no browser): a simulated chimera drives via /control/*"):
    /// codex request 1 streams the dispatched tool call, the held
    /// /control/toolcall resolves with codex's output (request 2), a repeat of
    /// the same call is re-delivered from cache without re-executing, and
    /// /control/turn_complete ends the turn with the final text.
    #[tokio::test]
    async fn control_toolcall_and_turn_complete_round_trip() {
        let mux = mux_with(MockShim::new());
        let tc = mux.get_conductor(Some("T"), None).await;

        // Slice 1: a fresh codex request opens the ChatGPT turn (no chat surface
        // booted -> test mode; chimera drives via /control/*).
        let (tx1, mut rx1) = mpsc::channel::<String>(64);
        let body1 = json!({"input": "do the thing", "tools": [{"type": "function"}]});
        let tc1 = tc.clone();
        let slice1 = tokio::spawn(async move { tc1.run_turn_conductor(&tx1, &body1).await });

        // chimera posts the tool call; this held request blocks until codex
        // has executed it.
        let mux_call = mux.clone();
        let toolcall = tokio::spawn(async move {
            mux_call
                .route_control_toolcall(&json!({
                    "name": "shell_command",
                    "kind": "function",
                    "arguments": {"command": "Get-Date"},
                    "call_id": "call_rt1",
                    "thread_id": "T",
                }))
                .await
        });

        // Slice 1 ends by dispatching the tool call to codex.
        timeout(Duration::from_secs(10), slice1)
            .await
            .expect("slice1 must not hang")
            .expect("join")
            .expect("run_turn_conductor");
        let frames1 = drain_frames(&mut rx1);
        let kinds1: Vec<&str> = frames1
            .iter()
            .map(|f| f["type"].as_str().unwrap())
            .collect();
        assert_eq!(
            kinds1,
            vec![
                "response.created",
                "response.output_item.done",
                "response.completed"
            ]
        );
        assert_eq!(frames1[1]["item"]["type"], "function_call");
        assert_eq!(frames1[1]["item"]["name"], "shell_command");
        assert_eq!(frames1[1]["item"]["call_id"], "call_rt1");

        // Slice 2: codex executed the tool; its function_call_output turn
        // resolves the parked chimera call.
        let (tx2, mut rx2) = mpsc::channel::<String>(64);
        let body2 = json!({"input": [{"type": "function_call_output", "output": "OK-42"}]});
        let tc2 = tc.clone();
        let slice2 = tokio::spawn(async move { tc2.run_turn_conductor(&tx2, &body2).await });

        let (status, out) = timeout(Duration::from_secs(10), toolcall)
            .await
            .expect("toolcall must unblock")
            .expect("join");
        assert_eq!(status, 200);
        assert_eq!(out["output"], "OK-42");
        assert_eq!(out["call_id"], "call_rt1");

        // A moderation re-call of the SAME signature is re-delivered from cache
        // (recovered=true) and NEVER re-executed.
        let (st_rep, rep) = timeout(
            Duration::from_secs(10),
            mux.route_control_toolcall(&json!({
                "name": "shell_command",
                "kind": "function",
                "arguments": {"command": "Get-Date"},
                "call_id": "call_rt2",
                "thread_id": "T",
            })),
        )
        .await
        .expect("repeat must not block");
        assert_eq!(st_rep, 200);
        assert_eq!(rep["recovered"], true);
        assert!(rep["output"].as_str().unwrap().contains("OK-42"));

        // ChatGPT finishes its turn -> /control/turn_complete ends the codex turn.
        let (st_done, done) = timeout(
            Duration::from_secs(10),
            mux.route_control_turn_complete(&json!({"text": "all done", "thread_id": "T"})),
        )
        .await
        .expect("turn_complete must not block");
        assert_eq!(st_done, 200);
        assert_eq!(done["ok"], true);

        timeout(Duration::from_secs(10), slice2)
            .await
            .expect("slice2 must not hang")
            .expect("join")
            .expect("run_turn_conductor");
        let frames2 = drain_frames(&mut rx2);
        let kinds2: Vec<&str> = frames2
            .iter()
            .map(|f| f["type"].as_str().unwrap())
            .collect();
        assert_eq!(
            kinds2,
            vec![
                "response.created",
                "response.output_item.added",
                "response.output_text.delta",
                "response.output_item.done",
                "response.completed"
            ]
        );
        assert_eq!(frames2[2]["delta"], "all done");

        // A second turn_complete has no active turn -> the python 409 shape.
        let (st_409, body_409) = timeout(
            Duration::from_secs(10),
            mux.route_control_turn_complete(&json!({"thread_id": "T"})),
        )
        .await
        .expect("no-active-turn must answer fast");
        assert_eq!(st_409, 409);
        assert_eq!(body_409["ok"], false);
    }

    fn drain_frames(rx: &mut mpsc::Receiver<String>) -> Vec<Value> {
        let mut out = Vec::new();
        while let Ok(line) = rx.try_recv() {
            let payload = line
                .trim_end_matches("\n\n")
                .trim_start_matches("data: ")
                .to_string();
            out.push(serde_json::from_str::<Value>(&payload).expect("valid json frame"));
        }
        out
    }
}
