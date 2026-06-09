//! Offline integration tests for the assembled shim server: `serve()` boots and
//! answers `/health`, a turn against a missing Chrome terminates gracefully
//! (no hang), and the HTTP `/control/*` + `/v1/responses` routes round-trip
//! through the installed `TurnDriver`.
//!
//! No live browser is available in CI, so these tests pin the CDP and chimera
//! ports to freshly-freed local ports nothing listens on; every request must
//! resolve quickly with the python-shaped error surfaces (502 boot JSON /
//! `response.failed`) rather than hanging.

use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;

use serde_json::json;
use serde_json::Value;
use tokio::time::timeout;

use cyrus_lipsync::config::ShadowConfig;
use cyrus_lipsync::responses::build_app;
use cyrus_lipsync::responses::serve;
use cyrus_lipsync::responses::ServeOptions;
use cyrus_lipsync::responses::ShadowResponsesShim;
use cyrus_lipsync::responses::TurnDriver;

/// Grab an ephemeral local port and free it again (tiny race, fine for tests).
async fn free_port() -> u16 {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind ephemeral");
    listener.local_addr().expect("local addr").port()
}

/// `serve()` with `lazy=true` and NO Chrome: `/health` answers 200 with
/// `booted:false`, and a `/v1/responses` turn terminates gracefully (the
/// python-shaped 502 "shadow boot failed" JSON — or, if a stream did start, a
/// terminal `response.failed`/`response.completed` frame) instead of hanging.
#[tokio::test]
async fn serve_lazy_health_and_graceful_turn_failure_without_chrome() {
    let port = free_port().await;
    let mut cfg = ShadowConfig::default();
    // Point CDP + chimera at ports nothing listens on, so the test never
    // touches a real Chrome/chimera even on a dev machine.
    cfg.cdp_port = free_port().await;
    cfg.server_url = format!("http://127.0.0.1:{}", free_port().await);

    let opts = ServeOptions {
        host: "127.0.0.1".to_string(),
        port,
        model: None,
        effort: None,
        lazy: true,
    };
    tokio::spawn(async move {
        let _ = serve(cfg, opts).await;
    });

    let client = reqwest::Client::new();
    let base = format!("http://127.0.0.1:{port}");

    // /health comes up and reports the python shape.
    let mut health: Option<Value> = None;
    for _ in 0..100 {
        if let Ok(resp) = client.get(format!("{base}/health")).send().await {
            if resp.status() == reqwest::StatusCode::OK {
                health = resp.json().await.ok();
                break;
            }
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let health = health.expect("/health never answered 200");
    assert_eq!(health["ok"], true);
    assert_eq!(health["booted"], false, "lazy + no chrome => not booted");
    assert_eq!(health["model"], "gpt-5-5-thinking");

    // A minimal codex Responses body. With no Chrome the boot must fail FAST
    // and gracefully (python: 502 {"error": "shadow boot failed: ..."}).
    let body = json!({
        "model": "gpt-5-5-thinking",
        "instructions": "be helpful",
        "input": [
            {"type": "message", "role": "user",
             "content": [{"type": "input_text", "text": "hello"}]}
        ],
        "tools": [],
    });
    let resp = timeout(
        Duration::from_secs(60),
        client
            .post(format!("{base}/v1/responses"))
            .header("thread-id", "t-int-1")
            .json(&body)
            .send(),
    )
    .await
    .expect("request must not hang")
    .expect("request must complete");

    let status = resp.status();
    let text = timeout(Duration::from_secs(60), resp.text())
        .await
        .expect("body must not hang")
        .expect("body must read");

    if status == reqwest::StatusCode::OK {
        // If a stream started, it must end with a terminal frame, not silence.
        assert!(
            text.contains("response.failed") || text.contains("response.completed"),
            "SSE stream must terminate with a terminal frame, got: {text}"
        );
    } else {
        assert_eq!(status, reqwest::StatusCode::BAD_GATEWAY, "body: {text}");
        assert!(
            text.contains("shadow boot failed"),
            "python-shaped boot error, got: {text}"
        );
    }
}

// ===== mock-driver route tests ===============================================

/// Records control traffic and answers canned shapes; `collect_turn` returns a
/// fixed final answer so `/v1/responses` exercises the full default SSE path.
struct RecordingDriver {
    control: Mutex<Vec<(String, Value)>>,
}

impl RecordingDriver {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            control: Mutex::new(Vec::new()),
        })
    }
}

#[async_trait::async_trait]
impl TurnDriver for RecordingDriver {
    async fn collect_turn(
        &self,
        _thread_id: Option<&str>,
        _subagent_kind: Option<&str>,
        _body: &Value,
        _inject_text: &str,
    ) -> anyhow::Result<String> {
        Ok("the mock answer".to_string())
    }

    async fn route_control_toolcall(&self, data: &Value) -> (u16, Value) {
        self.control
            .lock()
            .unwrap()
            .push(("toolcall".to_string(), data.clone()));
        (
            200,
            json!({"output": "mock-out", "call_id": data.get("call_id").cloned().unwrap_or(Value::Null)}),
        )
    }

    async fn route_control_turn_complete(&self, data: &Value) -> (u16, Value) {
        self.control
            .lock()
            .unwrap()
            .push(("turn_complete".to_string(), data.clone()));
        (200, json!({"ok": true}))
    }
}

/// A driver that keeps the trait defaults for the control routes (the
/// no-conductor 409s).
struct DefaultControlDriver;

#[async_trait::async_trait]
impl TurnDriver for DefaultControlDriver {
    async fn collect_turn(
        &self,
        _thread_id: Option<&str>,
        _subagent_kind: Option<&str>,
        _body: &Value,
        _inject_text: &str,
    ) -> anyhow::Result<String> {
        Ok(String::new())
    }
}

async fn spawn_app(driver: Arc<dyn TurnDriver>) -> String {
    let shim = ShadowResponsesShim::new(ShadowConfig::default(), "mock-model", driver);
    let app = build_app(shim);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind app");
    let port = listener.local_addr().expect("addr").port();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    format!("http://127.0.0.1:{port}")
}

/// `/control/toolcall` + `/control/turn_complete` reach the installed driver
/// and pass its (status, body) through; the python 400 guards stay in front.
#[tokio::test]
async fn control_routes_round_trip_into_the_driver() {
    let driver = RecordingDriver::new();
    let base = spawn_app(driver.clone()).await;
    let client = reqwest::Client::new();

    // Happy path: the driver sees the exact body and shapes the response.
    let resp = client
        .post(format!("{base}/control/toolcall"))
        .json(&json!({"name": "shell_command", "call_id": "c1", "arguments": {"command": "x"}}))
        .send()
        .await
        .expect("toolcall");
    assert_eq!(resp.status(), reqwest::StatusCode::OK);
    let out: Value = resp.json().await.expect("json");
    assert_eq!(out["output"], "mock-out");
    assert_eq!(out["call_id"], "c1");

    // python: missing/empty name -> 400 {"error": "name required"} BEFORE routing.
    let resp = client
        .post(format!("{base}/control/toolcall"))
        .json(&json!({"arguments": {}}))
        .send()
        .await
        .expect("no-name toolcall");
    assert_eq!(resp.status(), reqwest::StatusCode::BAD_REQUEST);
    let out: Value = resp.json().await.expect("json");
    assert_eq!(out["error"], "name required");

    // python: invalid JSON -> 400 {"error": "invalid JSON"}.
    let resp = client
        .post(format!("{base}/control/toolcall"))
        .body("not json")
        .send()
        .await
        .expect("bad json toolcall");
    assert_eq!(resp.status(), reqwest::StatusCode::BAD_REQUEST);
    let out: Value = resp.json().await.expect("json");
    assert_eq!(out["error"], "invalid JSON");

    // turn_complete routes through with the driver's shape.
    let resp = client
        .post(format!("{base}/control/turn_complete"))
        .json(&json!({"text": "final text", "thread_id": "T"}))
        .send()
        .await
        .expect("turn_complete");
    assert_eq!(resp.status(), reqwest::StatusCode::OK);
    let out: Value = resp.json().await.expect("json");
    assert_eq!(out["ok"], true);

    // The driver saw both control calls, bodies intact.
    let seen = driver.control.lock().unwrap().clone();
    assert_eq!(seen.len(), 2);
    assert_eq!(seen[0].0, "toolcall");
    assert_eq!(seen[0].1["name"], "shell_command");
    assert_eq!(seen[1].0, "turn_complete");
    assert_eq!(seen[1].1["text"], "final text");
}

/// Without a conductor router the control routes answer the python-shaped 409s
/// (the trait defaults) instead of erroring out.
#[tokio::test]
async fn control_routes_default_to_python_409_shapes() {
    let base = spawn_app(Arc::new(DefaultControlDriver)).await;
    let client = reqwest::Client::new();

    let resp = client
        .post(format!("{base}/control/toolcall"))
        .json(&json!({"name": "shell_command"}))
        .send()
        .await
        .expect("toolcall");
    assert_eq!(resp.status(), reqwest::StatusCode::CONFLICT);
    let out: Value = resp.json().await.expect("json");
    assert_eq!(out["error"], "no active conductor for thread");

    let resp = client
        .post(format!("{base}/control/turn_complete"))
        .json(&json!({"text": "x"}))
        .send()
        .await
        .expect("turn_complete");
    assert_eq!(resp.status(), reqwest::StatusCode::CONFLICT);
    let out: Value = resp.json().await.expect("json");
    assert_eq!(out["ok"], false);
}

/// `/v1/responses` with a mock driver streams the full codex-shaped SSE
/// sequence for a final answer (created -> added -> delta -> done -> completed).
#[tokio::test]
async fn responses_route_streams_codex_sse_via_mock_driver() {
    let base = spawn_app(RecordingDriver::new()).await;
    let client = reqwest::Client::new();

    let body = json!({
        "input": [
            {"type": "message", "role": "user",
             "content": [{"type": "input_text", "text": "hello"}]}
        ],
        "tools": [],
    });
    let resp = timeout(
        Duration::from_secs(30),
        client
            .post(format!("{base}/v1/responses"))
            .header("thread-id", "t-mock-1")
            .json(&body)
            .send(),
    )
    .await
    .expect("must not hang")
    .expect("request");
    assert_eq!(resp.status(), reqwest::StatusCode::OK);
    let text = timeout(Duration::from_secs(30), resp.text())
        .await
        .expect("body must not hang")
        .expect("body");

    let kinds: Vec<String> = text
        .lines()
        .filter_map(|l| l.strip_prefix("data: "))
        .map(|p| serde_json::from_str::<Value>(p).expect("frame json"))
        .map(|f| f["type"].as_str().unwrap_or("").to_string())
        .collect();
    assert_eq!(
        kinds,
        vec![
            "response.created",
            "response.output_item.added",
            "response.output_text.delta",
            "response.output_item.done",
            "response.completed",
        ]
    );
    assert!(text.contains("the mock answer"));
}
