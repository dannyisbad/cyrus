//! Minimal async Chrome DevTools Protocol client (one page socket per tab).
//!
//! Source: idare/shadow/cdp.py (private original)
//!
//! Port of the Python `CDPClient`: connect to a page target, send id-correlated
//! commands with a timeout, evaluate JS, synthesize input, navigate, register
//! event handlers, and reconnect (by target_id) if the socket drops.
//!
//! The Python used aiohttp; this uses tokio-tungstenite for the page WebSocket
//! and a tiny raw-TCP HTTP/1.1 GET for the `{base}/json` target-discovery
//! endpoint (avoids pulling in a full HTTP client crate — the endpoint is a
//! localhost JSON array).
//!
//! Hazards preserved from the original:
//!   - aiohttp set `max_msg_size=0` to disable its 4MB frame cap (CDP DOM
//!     payloads are large). Here `WebSocketConfig::max_message_size` /
//!     `max_frame_size` are set to `None` (unbounded) so large frames are never
//!     truncated.
//!   - On socket close, every pending request future is failed with
//!     `CdpError::SocketClosed` so callers never hang (mirrors the read loop's
//!     `finally` that resolves all pending futures).
//!   - `eval` surfaces `exceptionDetails` as an error and returns
//!     `result.result.value` (`returnByValue:true`, `awaitPromise:true`).
//!   - `for_target` attaches to a SPECIFIC page target by id (one socket per
//!     tab — the basis of per-thread WS isolation); reconnect re-resolves by
//!     `target_id`, never by first-URL-match.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use futures::stream::StreamExt;
use futures::SinkExt;
use serde_json::{json, Value};
use tokio::net::TcpStream;
use tokio::sync::{oneshot, Mutex};
use tokio::task::JoinHandle;
use tokio_tungstenite::tungstenite::protocol::{Message, WebSocketConfig};

/// Mirror of the Python `CDPError` (a `RuntimeError` subclass).
#[derive(Debug, Clone, thiserror::Error)]
pub enum CdpError {
    /// A CDP command came back with an `error` object, or `eval` saw
    /// `exceptionDetails`. Carries the surfaced message text.
    #[error("{0}")]
    Protocol(String),
    /// `send`/`eval` timed out waiting for the matching response id.
    #[error("CDP timeout: {0}")]
    Timeout(String),
    /// The page socket is not open (never connected, or already closed).
    #[error("CDP socket not open")]
    SocketNotOpen,
    /// The page socket closed while a request was still in flight.
    #[error("CDP socket closed")]
    SocketClosed,
    /// Target discovery / HTTP transport failure (the `{base}/json` GET).
    #[error("{0}")]
    Transport(String),
}

type Result<T> = std::result::Result<T, CdpError>;

/// An event handler: receives the CDP event's `params` object.
///
/// Handlers are stored as reference-counted closures. They are called
/// synchronously from the read loop (like the Python, which invokes
/// `h(params)` inline), so they must not block. Panics/errors inside a handler
/// are isolated per call (the read loop keeps going), matching the Python
/// `try/except: pass`. `Arc` (not `Box`) so callers can build the closure with
/// `Arc::new(...)` — the shape `wstap.rs` already relies on.
pub type EventHandler = Arc<dyn Fn(Value) + Send + Sync + 'static>;

/// Shared state touched by both the public API and the read loop.
///
/// `pending` is on a tokio `Mutex` (only ever locked from async code).
/// `handlers` is on a `std::sync::Mutex` so `on` can be a plain synchronous
/// method — `wstap.rs` calls `cdp.on(...)` without `.await`. The read loop only
/// holds the handlers lock briefly to clone out the matching list, never across
/// an await, so a std mutex is safe here.
struct Shared {
    /// Outstanding requests keyed by message id -> oneshot result channel.
    pending: Mutex<HashMap<u64, oneshot::Sender<Result<Value>>>>,
    /// Event-method -> registered handlers (e.g. "Runtime.bindingCalled").
    handlers: std::sync::Mutex<HashMap<String, Vec<EventHandler>>>,
}

/// The write half of the page WebSocket (behind a mutex so `send` is safe to
/// call concurrently from multiple tasks).
type WsSink = futures::stream::SplitSink<
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<TcpStream>>,
    Message,
>;

/// One CDP page socket. See `CDPClient` in cdp.py.
pub struct CdpClient {
    host: String,
    port: u16,
    tab_match: String,

    /// Monotonic message-id counter (`self._id` in the Python).
    next_id: Arc<AtomicU64>,
    /// Pending futures + event handler registry, shared with the read loop.
    shared: Arc<Shared>,
    /// Write half of the current socket; `None` until connected / after close.
    sink: Mutex<Option<WsSink>>,
    /// The current read-loop task handle (so we can abort it on reconnect/close).
    reader: Mutex<Option<JoinHandle<()>>>,

    /// The picked target object (the `/json` entry).
    pub target: Mutex<Option<Value>>,
    /// The target id this client is bound to (drives reconnect).
    pub target_id: Mutex<Option<String>>,
}

impl CdpClient {
    /// Construct an unconnected client. Mirrors `CDPClient.__init__`.
    pub fn new(host: impl Into<String>, port: u16, tab_match: impl Into<String>) -> Self {
        Self {
            host: host.into(),
            port,
            tab_match: tab_match.into(),
            next_id: Arc::new(AtomicU64::new(0)),
            shared: Arc::new(Shared {
                pending: Mutex::new(HashMap::new()),
                handlers: std::sync::Mutex::new(HashMap::new()),
            }),
            sink: Mutex::new(None),
            reader: Mutex::new(None),
            target: Mutex::new(None),
            target_id: Mutex::new(None),
        }
    }

    /// Convenience constructor matching the Python defaults
    /// (`host="127.0.0.1", port=9222, tab_match="chatgpt.com"`).
    pub fn with_defaults() -> Self {
        Self::new("127.0.0.1", 9222, "chatgpt.com")
    }

    /// `http://{host}:{port}` (the `base` property).
    pub fn base(&self) -> String {
        format!("http://{}:{}", self.host, self.port)
    }

    /// Register a callback for a CDP event (e.g. `"Network.webSocketFrameReceived"`
    /// or `"Runtime.bindingCalled"`). Mirrors `CDPClient.on`.
    ///
    /// Synchronous (no `.await`) and takes an `Arc<dyn Fn(Value) + Send + Sync>`,
    /// matching the call sites in `wstap.rs`
    /// (`cdp.on("...", Arc::new(move |params| {...}))`).
    pub fn on(&self, method: impl Into<String>, handler: EventHandler) {
        self.shared
            .handlers
            .lock()
            .expect("cdp handlers mutex poisoned")
            .entry(method.into())
            .or_default()
            .push(handler);
    }

    /// Pick the first matching page target and open its socket. `CDPClient.connect`.
    pub async fn connect(&self) -> Result<()> {
        let target = self.pick_target().await?;
        let id = target.get("id").and_then(Value::as_str).map(str::to_owned);
        *self.target.lock().await = Some(target);
        *self.target_id.lock().await = id;
        self.open_ws().await
    }

    /// Attach to a SPECIFIC page target by id (one socket per tab — the basis of
    /// per-subagent WS isolation). Reconnect uses `target_id`, never
    /// first-URL-match. Mirrors `CDPClient.for_target`.
    pub async fn for_target(
        host: impl Into<String>,
        port: u16,
        target_id: impl Into<String>,
        tab_match: impl Into<String>,
    ) -> Result<Self> {
        let this = Self::new(host, port, tab_match);
        let target_id = target_id.into();
        let target = this.find_target_by_id(&target_id).await?;
        *this.target_id.lock().await = Some(target_id);
        *this.target.lock().await = Some(target);
        this.open_ws().await?;
        Ok(this)
    }

    /// GET `{base}/json` and return the entry whose `id` matches and which has a
    /// `webSocketDebuggerUrl`. Mirrors `_find_target_by_id`.
    async fn find_target_by_id(&self, target_id: &str) -> Result<Value> {
        let targets = self.fetch_targets().await?;
        for t in targets {
            if t.get("id").and_then(Value::as_str) == Some(target_id)
                && t.get("webSocketDebuggerUrl")
                    .and_then(Value::as_str)
                    .is_some()
            {
                return Ok(t);
            }
        }
        Err(CdpError::Protocol(format!(
            "no target with id {target_id} on {}",
            self.base()
        )))
    }

    /// GET `{base}/json` and return the first `type=="page"` target whose URL
    /// contains `tab_match` and which has a `webSocketDebuggerUrl`. `_pick_target`.
    async fn pick_target(&self) -> Result<Value> {
        let targets = self.fetch_targets().await?;
        for t in targets {
            let is_page = t.get("type").and_then(Value::as_str) == Some("page");
            let url = t.get("url").and_then(Value::as_str).unwrap_or("");
            let has_ws = t
                .get("webSocketDebuggerUrl")
                .and_then(Value::as_str)
                .is_some();
            if is_page && url.contains(&self.tab_match) && has_ws {
                return Ok(t);
            }
        }
        Err(CdpError::Protocol(format!(
            "no page tab matching {:?} on {}",
            self.tab_match,
            self.base()
        )))
    }

    /// Fetch and JSON-parse `{base}/json` (the DevTools target list).
    async fn fetch_targets(&self) -> Result<Vec<Value>> {
        let body = http_get(&self.host, self.port, "/json").await?;
        let parsed: Value = serde_json::from_str(&body)
            .map_err(|e| CdpError::Transport(format!("bad /json response: {e}")))?;
        match parsed {
            Value::Array(items) => Ok(items),
            other => Err(CdpError::Transport(format!(
                "/json did not return an array: {other}"
            ))),
        }
    }

    /// Open the page WebSocket, spawn the read loop, and `Runtime.enable`.
    /// Mirrors `_open_ws` (incl. the `max_msg_size=0` cap lift via
    /// `WebSocketConfig` with unbounded message/frame sizes).
    async fn open_ws(&self) -> Result<()> {
        let ws_url = self
            .target
            .lock()
            .await
            .as_ref()
            .and_then(|t| t.get("webSocketDebuggerUrl").and_then(Value::as_str))
            .map(str::to_owned)
            .ok_or_else(|| CdpError::Transport("target has no webSocketDebuggerUrl".into()))?;

        // max_msg_size=0 in aiohttp == no cap. tokio-tungstenite: None == unbounded.
        let config = WebSocketConfig {
            max_message_size: None,
            max_frame_size: None,
            ..Default::default()
        };

        let (ws, _resp) =
            tokio_tungstenite::connect_async_with_config(&ws_url, Some(config), false)
                .await
                .map_err(|e| CdpError::Transport(format!("ws_connect {ws_url}: {e}")))?;

        let (sink, stream) = ws.split();
        *self.sink.lock().await = Some(sink);

        // Spawn the read loop over the shared state.
        let shared = Arc::clone(&self.shared);
        let handle = tokio::spawn(read_loop(stream, shared));
        *self.reader.lock().await = Some(handle);

        // Like the Python, enable the Runtime domain immediately on open.
        self.send("Runtime.enable", None).await?;
        Ok(())
    }

    /// Default per-command timeout (seconds), matching the Python `send`'s
    /// `timeout: float = 30.0`.
    const DEFAULT_TIMEOUT_SECS: f64 = 30.0;

    /// Send a CDP command and await its id-correlated response, using the
    /// default 30s timeout. Mirrors `CDPClient.send` with its default timeout.
    ///
    /// The two-argument shape matches the call sites in `wstap.rs` /
    /// `tab_factory.rs` (`cdp.send("Network.enable", None)`). For a custom
    /// timeout use [`CdpClient::send_timeout`].
    pub async fn send(&self, method: &str, params: Option<Value>) -> Result<Value> {
        self.send_timeout(method, params, Self::DEFAULT_TIMEOUT_SECS)
            .await
    }

    /// Send a CDP command with an explicit timeout (seconds). The full form of
    /// `CDPClient.send(method, params, timeout)`.
    pub async fn send_timeout(
        &self,
        method: &str,
        params: Option<Value>,
        timeout: f64,
    ) -> Result<Value> {
        let mid = self.next_id.fetch_add(1, Ordering::SeqCst) + 1;

        let (tx, rx) = oneshot::channel::<Result<Value>>();
        self.shared.pending.lock().await.insert(mid, tx);

        let frame = json!({
            "id": mid,
            "method": method,
            "params": params.unwrap_or_else(|| json!({})),
        });
        let text = serde_json::to_string(&frame)
            .map_err(|e| CdpError::Transport(format!("serialize command: {e}")))?;

        // Send under the sink lock; if it's not open, drop the pending entry.
        {
            let mut guard = self.sink.lock().await;
            let sink = match guard.as_mut() {
                Some(s) => s,
                None => {
                    self.shared.pending.lock().await.remove(&mid);
                    return Err(CdpError::SocketNotOpen);
                }
            };
            if let Err(e) = sink.send(Message::Text(text)).await {
                self.shared.pending.lock().await.remove(&mid);
                return Err(CdpError::Transport(format!("ws send: {e}")));
            }
        }

        let dur = Duration::from_secs_f64(timeout);
        match tokio::time::timeout(dur, rx).await {
            // Resolved by the read loop (Ok=result, Err=protocol/socket-closed).
            Ok(Ok(res)) => res,
            // Sender dropped without sending (read loop ended without resolving).
            Ok(Err(_)) => Err(CdpError::SocketClosed),
            // Timed out: remove the pending entry, mirror the Python message.
            Err(_) => {
                self.shared.pending.lock().await.remove(&mid);
                Err(CdpError::Timeout(method.to_string()))
            }
        }
    }

    /// `Runtime.evaluate` with `returnByValue:true, awaitPromise:true`; surfaces
    /// `exceptionDetails` as an error and returns `result.result.value`.
    /// Mirrors `CDPClient.eval`.
    pub async fn eval(&self, expression: &str, timeout: f64) -> Result<Value> {
        let result = self
            .send_timeout(
                "Runtime.evaluate",
                Some(json!({
                    "expression": expression,
                    "returnByValue": true,
                    "awaitPromise": true,
                })),
                timeout,
            )
            .await?;

        if let Some(details) = result.get("exceptionDetails") {
            let text = details
                .get("text")
                .and_then(Value::as_str)
                .unwrap_or("exception");
            return Err(CdpError::Protocol(format!("eval: {text}")));
        }

        Ok(result
            .get("result")
            .and_then(|r| r.get("value"))
            .cloned()
            .unwrap_or(Value::Null))
    }

    /// `Input.insertText`. Mirrors `CDPClient.insert_text`.
    pub async fn insert_text(&self, text: &str) -> Result<()> {
        self.send("Input.insertText", Some(json!({ "text": text })))
            .await?;
        Ok(())
    }

    /// Dispatch a keyDown then keyUp for the given key. Mirrors `CDPClient.key`.
    pub async fn key(&self, key: &str, code: &str, vk: i64) -> Result<()> {
        for kind in ["keyDown", "keyUp"] {
            self.send(
                "Input.dispatchKeyEvent",
                Some(json!({
                    "type": kind,
                    "key": key,
                    "code": code,
                    "windowsVirtualKeyCode": vk,
                    "nativeVirtualKeyCode": vk,
                })),
            )
            .await?;
        }
        Ok(())
    }

    /// `Page.navigate`. Mirrors `CDPClient.navigate`.
    pub async fn navigate(&self, url: &str) -> Result<()> {
        self.send("Page.navigate", Some(json!({ "url": url })))
            .await?;
        Ok(())
    }

    /// Expose `window.<name>(str)` in the page; calls surface as
    /// `Runtime.bindingCalled` events. Mirrors `CDPClient.add_binding`.
    pub async fn add_binding(&self, name: &str) -> Result<()> {
        self.send("Runtime.addBinding", Some(json!({ "name": name })))
            .await?;
        Ok(())
    }

    /// Install JS that runs at document-start on every future navigation/reload
    /// (`Page.enable` then `Page.addScriptToEvaluateOnNewDocument`). Survives
    /// navigations for the life of this session; re-call after `reconnect`.
    /// Mirrors `CDPClient.add_init_script` (returns the CDP result object).
    pub async fn add_init_script(&self, source: &str) -> Result<Value> {
        self.send("Page.enable", None).await?;
        self.send(
            "Page.addScriptToEvaluateOnNewDocument",
            Some(json!({ "source": source })),
        )
        .await
    }

    /// `Network.enable`. Not a named method on the Python class, but `Network`
    /// is one of the domains the brief lists; callers (wstap) enable it via the
    /// generic `send`. Provided here as a convenience wrapper.
    pub async fn enable_network(&self) -> Result<()> {
        self.send("Network.enable", None).await?;
        Ok(())
    }

    /// Tear down the current socket+reader and re-open, re-resolving the target
    /// by `target_id` (falling back to a fresh URL-match pick only if no id is
    /// bound). Mirrors `CDPClient.reconnect`.
    pub async fn reconnect(&self) -> Result<()> {
        self.teardown_socket().await;

        let target_id = self.target_id.lock().await.clone();
        let target = match target_id {
            Some(id) => self.find_target_by_id(&id).await?,
            None => self.pick_target().await?,
        };
        *self.target.lock().await = Some(target);
        self.open_ws().await
    }

    /// Close the socket and reader. Mirrors `CDPClient.close` (aiohttp also
    /// closed the session; here there is no persistent HTTP session to close —
    /// each `{base}/json` GET is a one-shot connection).
    pub async fn close(&self) {
        self.teardown_socket().await;
    }

    /// Abort the reader, close the write half, and fail any in-flight requests.
    async fn teardown_socket(&self) {
        if let Some(handle) = self.reader.lock().await.take() {
            handle.abort();
        }
        if let Some(mut sink) = self.sink.lock().await.take() {
            let _ = sink.close().await;
        }
        // Aborting the reader skips its socket-close cleanup, so fail any pending
        // futures here too (callers must never hang).
        fail_all_pending(&self.shared).await;
    }
}

/// Read loop: resolve id-correlated responses and dispatch events. Mirrors
/// `_read_loop`, including the `finally` that fails every still-pending future
/// when the socket closes.
async fn read_loop<S>(mut stream: S, shared: Arc<Shared>)
where
    S: futures::Stream<
            Item = std::result::Result<Message, tokio_tungstenite::tungstenite::Error>,
        > + Unpin,
{
    while let Some(item) = stream.next().await {
        let msg = match item {
            Ok(m) => m,
            // Transport error: stop reading; the `finally`-equivalent below runs.
            Err(_) => break,
        };

        // Only TEXT frames carry CDP JSON (Python skips non-TEXT).
        let text = match msg {
            Message::Text(t) => t,
            Message::Close(_) => break,
            _ => continue,
        };

        let data: Value = match serde_json::from_str(&text) {
            Ok(v) => v,
            Err(_) => continue, // malformed JSON ignored, like the Python
        };

        // Response to a command (`id` present) vs. an event (`method` present).
        if let Some(mid) = data.get("id").and_then(Value::as_u64) {
            let sender = shared.pending.lock().await.remove(&mid);
            if let Some(tx) = sender {
                if let Some(err) = data.get("error") {
                    let message = err
                        .get("message")
                        .and_then(Value::as_str)
                        .unwrap_or("CDP error")
                        .to_string();
                    let _ = tx.send(Err(CdpError::Protocol(message)));
                } else {
                    // Python resolves with data.get("result") (may be absent).
                    let result = data.get("result").cloned().unwrap_or(Value::Null);
                    let _ = tx.send(Ok(result));
                }
            }
        } else if let Some(method) = data.get("method").and_then(Value::as_str) {
            // Dispatch to registered handlers with params (or {} if absent).
            let params = match data.get("params") {
                Some(Value::Null) | None => json!({}),
                Some(p) => p.clone(),
            };
            // Clone the matching handlers out of the registry, then drop the lock
            // BEFORE invoking them. Handlers are `Arc` (cheap to clone), and this
            // avoids holding the std mutex across a handler that might re-enter
            // `on()` (which would self-deadlock a std mutex). Handlers run inline
            // and synchronously, like the Python `h(params)`.
            let handlers: Vec<EventHandler> = {
                let guard = shared
                    .handlers
                    .lock()
                    .expect("cdp handlers mutex poisoned");
                guard.get(method).cloned().unwrap_or_default()
            };
            for h in handlers {
                // Each handler is isolated; the read loop keeps going regardless
                // (the Python wraps each call in `try/except: pass`).
                h(params.clone());
            }
        }
    }

    // socket closed: fail anything still waiting so callers don't hang.
    fail_all_pending(&shared).await;
}

/// Fail every pending request with `SocketClosed` and clear the map.
async fn fail_all_pending(shared: &Shared) {
    let mut pending = shared.pending.lock().await;
    for (_, tx) in pending.drain() {
        let _ = tx.send(Err(CdpError::SocketClosed));
    }
}

/// Minimal HTTP/1.1 GET against a localhost endpoint, returning the response
/// body as a String. Used only for the DevTools `{base}/json` target list,
/// which is a small JSON array served over plain HTTP on the debug port.
///
/// Uses raw TCP (`Connection: close`, read to EOF) to avoid adding a full HTTP
/// client dependency for one localhost GET.
async fn http_get(host: &str, port: u16, path: &str) -> Result<String> {
    // Chrome's DevTools HTTP endpoint IGNORES `Connection: close` (it sends
    // Content-Length and keeps the socket open), so a read-to-EOF strategy
    // hangs forever. Read incrementally: headers first, then exactly the body
    // the framing describes (Content-Length / chunked terminator / EOF), all
    // under one overall deadline so a stalled socket can never wedge a boot.
    tokio::time::timeout(Duration::from_secs(15), http_get_inner(host, port, path))
        .await
        .map_err(|_| CdpError::Transport(format!("GET {path}: timed out after 15s")))?
}

async fn http_get_inner(host: &str, port: u16, path: &str) -> Result<String> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let addr = format!("{host}:{port}");
    let mut stream = TcpStream::connect(&addr)
        .await
        .map_err(|e| CdpError::Transport(format!("connect {addr}: {e}")))?;

    let request = format!(
        "GET {path} HTTP/1.1\r\nHost: {host}:{port}\r\nAccept: application/json\r\nConnection: close\r\n\r\n"
    );
    stream
        .write_all(request.as_bytes())
        .await
        .map_err(|e| CdpError::Transport(format!("http write: {e}")))?;
    stream
        .flush()
        .await
        .map_err(|e| CdpError::Transport(format!("http flush: {e}")))?;

    // Read until the header terminator is in the buffer.
    let mut raw = Vec::new();
    let split = loop {
        if let Some(pos) = raw.windows(4).position(|w| w == b"\r\n\r\n") {
            break pos;
        }
        let mut chunk = [0u8; 16384];
        let n = stream
            .read(&mut chunk)
            .await
            .map_err(|e| CdpError::Transport(format!("http read: {e}")))?;
        if n == 0 {
            return Err(CdpError::Transport(
                "malformed HTTP response (no header terminator)".into(),
            ));
        }
        raw.extend_from_slice(&chunk[..n]);
    };
    let header_end = split + 4;

    // Decide how much body to read from the framing headers.
    let headers_text = String::from_utf8_lossy(&raw[..split]).to_ascii_lowercase();
    let content_length: Option<usize> = headers_text
        .lines()
        .find_map(|l| l.strip_prefix("content-length:"))
        .and_then(|v| v.trim().parse().ok());
    let chunked = headers_text.contains("transfer-encoding: chunked");

    loop {
        let body_len = raw.len() - header_end;
        let done = if let Some(cl) = content_length {
            body_len >= cl
        } else if chunked {
            // Complete once the terminating 0-length chunk has arrived.
            raw[header_end..]
                .windows(5)
                .any(|w| w == b"0\r\n\r\n")
        } else {
            false // no framing: read to EOF below
        };
        if done {
            break;
        }
        let mut chunk = [0u8; 16384];
        let n = stream
            .read(&mut chunk)
            .await
            .map_err(|e| CdpError::Transport(format!("http read: {e}")))?;
        if n == 0 {
            break; // EOF: whatever we have is the body
        }
        raw.extend_from_slice(&chunk[..n]);
    }

    let header_bytes = &raw[..split];
    let mut body = &raw[header_end..];
    // Trim any keep-alive over-read past Content-Length.
    if let Some(cl) = content_length {
        if body.len() > cl {
            body = &body[..cl];
        }
    }

    let headers = String::from_utf8_lossy(header_bytes);
    let mut lines = headers.lines();
    let status_line = lines.next().unwrap_or("");
    let ok = status_line
        .split_whitespace()
        .nth(1)
        .map(|c| c.starts_with('2'))
        .unwrap_or(false);
    if !ok {
        return Err(CdpError::Transport(format!(
            "GET {path} -> {status_line}"
        )));
    }

    // Handle Transfer-Encoding: chunked (DevTools may use it for /json).
    let is_chunked = headers
        .to_ascii_lowercase()
        .contains("transfer-encoding: chunked");
    if is_chunked {
        let decoded = dechunk(body)?;
        return String::from_utf8(decoded)
            .map_err(|e| CdpError::Transport(format!("non-utf8 body: {e}")));
    }

    // Otherwise the body is the remaining bytes (Connection: close read to EOF).
    // (If Content-Length is shorter than what we read, the JSON parser will
    // still succeed on the array; we keep the full remainder.)
    if let Some(end) = body.iter().rposition(|&b| !b.is_ascii_whitespace()) {
        body = &body[..=end];
    }
    String::from_utf8(body.to_vec())
        .map_err(|e| CdpError::Transport(format!("non-utf8 body: {e}")))
}

/// Decode an HTTP/1.1 chunked transfer body into its raw bytes.
fn dechunk(mut data: &[u8]) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    loop {
        // Read the chunk-size line (hex, up to CRLF).
        let nl = data
            .windows(2)
            .position(|w| w == b"\r\n")
            .ok_or_else(|| CdpError::Transport("chunked: missing size CRLF".into()))?;
        let size_line = std::str::from_utf8(&data[..nl])
            .map_err(|_| CdpError::Transport("chunked: bad size line".into()))?;
        // chunk extensions (after ';') are ignored.
        let size_hex = size_line.split(';').next().unwrap_or("").trim();
        let size = usize::from_str_radix(size_hex, 16)
            .map_err(|_| CdpError::Transport("chunked: bad size hex".into()))?;
        data = &data[nl + 2..];
        if size == 0 {
            break;
        }
        if data.len() < size {
            return Err(CdpError::Transport("chunked: truncated chunk".into()));
        }
        out.extend_from_slice(&data[..size]);
        data = &data[size..];
        // Skip the trailing CRLF after the chunk data.
        if data.len() >= 2 && &data[..2] == b"\r\n" {
            data = &data[2..];
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dechunk_basic() {
        // "Wiki" + "pedia" classic chunked example -> "Wikipedia"
        let body = b"4\r\nWiki\r\n5\r\npedia\r\n0\r\n\r\n";
        let out = dechunk(body).unwrap();
        assert_eq!(out, b"Wikipedia");
    }

    #[test]
    fn dechunk_with_extension_and_array() {
        let body = b"2;ext=1\r\n[]\r\n0\r\n\r\n";
        let out = dechunk(body).unwrap();
        assert_eq!(out, b"[]");
    }
}
