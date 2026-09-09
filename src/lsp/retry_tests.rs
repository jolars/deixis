use super::*;
use crate::config::Config;

async fn server() -> (ActiveServer, mpsc::Receiver<Vec<u8>>) {
    let config = Config::from_toml_str(
        r#"
[servers.test]
command = "test-lsp"
file_extensions = { ".test" = "test" }
[servers.test.limits]
max_concurrent_requests = 1
"#,
    )
    .unwrap();
    let (sender, receiver) = mpsc::channel(16);
    let active = ActiveServer::new(
        config.server("test").unwrap(),
        Path::new("/project"),
        sender,
        Arc::new(StderrCapture::default()),
    );
    active.readiness.lock().await.mark_initialized();
    (active, receiver)
}

fn request(
    active: &ActiveServer,
    method: &'static str,
    timeout_ms: u64,
    token: &CancellationToken,
) -> JoinHandle<Result<JsonValue, LspError>> {
    let active = active.clone();
    let token = token.clone();
    tokio::spawn(async move {
        active
            .request_value(
                method,
                json!({"query": "needle"}),
                Duration::from_millis(timeout_ms),
                &token,
            )
            .await
    })
}

async fn receive(receiver: &mut mpsc::Receiver<Vec<u8>>) -> JsonValue {
    let body = tokio::time::timeout(Duration::from_secs(10), receiver.recv())
        .await
        .expect("expected an outbound LSP message")
        .unwrap();
    serde_json::from_slice(&body).unwrap()
}

async fn respond(active: &ActiveServer, message: &JsonValue, value: JsonValue) {
    let mut response = value;
    response["jsonrpc"] = json!("2.0");
    response["id"] = message["id"].clone();
    handle_incoming_message("test", active, response).await;
    tokio::task::yield_now().await;
}

fn canceled(code: i64, data: JsonValue) -> JsonValue {
    json!({"error": {"code": code, "message": "indexing", "data": data}})
}

async fn readiness(active: &ActiveServer, progress: bool, ready: bool) {
    let message = if progress {
        json!({"method": "$/progress", "params": {
            "token": "index", "value": {"kind": if ready {"end"} else {"begin"}}
        }})
    } else {
        json!({"method": "experimental/serverStatus", "params": {
            "health": "ok", "quiescent": ready
        }})
    };
    handle_incoming_message("test", active, message).await;
}

#[tokio::test(start_paused = true)]
async fn retry_waits_for_readiness_and_releases_the_concurrency_slot() {
    for progress in [false, true] {
        let (active, mut receiver) = server().await;
        readiness(&active, progress, false).await;
        let token = CancellationToken::new();
        let pending = request(&active, "workspace/symbol", 5_000, &token);
        let first = receive(&mut receiver).await;
        respond(
            &active,
            &first,
            canceled(-32802, json!({"retriggerRequest": true})),
        )
        .await;
        assert!(!pending.is_finished());
        assert_eq!(active.request_permits.available_permits(), 1);

        let other = request(&active, "textDocument/hover", 1_000, &token);
        let other_message = receive(&mut receiver).await;
        assert_eq!(other_message["method"], "textDocument/hover");
        respond(&active, &other_message, json!({"result": "other"})).await;
        assert_eq!(other.await.unwrap().unwrap(), "other");

        tokio::time::advance(Duration::from_millis(300)).await;
        assert!(receiver.try_recv().is_err());
        let ready_at = Instant::now();
        readiness(&active, progress, true).await;
        let second = receive(&mut receiver).await;
        assert!(Instant::now() - ready_at < Duration::from_millis(50));
        assert_ne!(first["id"], second["id"]);
        assert_eq!(first["params"], second["params"]);
        assert_eq!(first["method"], second["method"]);
        respond(&active, &second, json!({"result": ["found"]})).await;
        assert_eq!(pending.await.unwrap().unwrap(), json!(["found"]));
        assert!(active.requests.lock().await.pending.is_empty());
    }
}

#[tokio::test(start_paused = true)]
async fn retry_wait_is_bounded_even_without_a_readiness_transition() {
    for state in ["unknown", "busy", "ready"] {
        let (active, mut receiver) = server().await;
        if state != "unknown" {
            readiness(&active, true, state == "ready").await;
        }
        let pending = request(
            &active,
            "textDocument/diagnostic",
            5_000,
            &CancellationToken::new(),
        );
        let first = receive(&mut receiver).await;
        let started = Instant::now();
        respond(
            &active,
            &first,
            canceled(-32802, json!({"retriggerRequest": true})),
        )
        .await;
        let second = receive(&mut receiver).await;
        let elapsed = Instant::now() - started;
        assert!(elapsed >= Duration::from_millis(50));
        assert!(elapsed <= Duration::from_millis(1_010));
        if state == "busy" {
            assert!(elapsed >= Duration::from_secs(1));
        }
        respond(&active, &second, json!({"result": null})).await;
        pending.await.unwrap().unwrap();
    }
}

#[tokio::test(start_paused = true)]
async fn retry_limit_reports_the_last_lsp_failure() {
    for (code, data) in [
        (-32802, json!({"retriggerRequest": true})),
        (-32802, JsonValue::Null),
        (-32800, JsonValue::Null),
    ] {
        let (active, mut receiver) = server().await;
        let pending = request(
            &active,
            "textDocument/hover",
            5_000,
            &CancellationToken::new(),
        );
        for _ in 0..4 {
            let message = receive(&mut receiver).await;
            respond(&active, &message, canceled(code, data.clone())).await;
        }
        let error = pending.await.unwrap().unwrap_err();
        assert!(
            error
                .to_string()
                .contains("retry limit reached after 4 attempts"),
            "{error}"
        );
        assert!(
            error
                .to_string()
                .contains(&format!("LSP error {code}: indexing"))
        );
        assert!(receiver.try_recv().is_err());
        assert!(active.requests.lock().await.pending.is_empty());
    }
}

#[tokio::test(start_paused = true)]
async fn retry_does_not_replay_non_retriggerable_or_lifecycle_errors() {
    for (method, code, data) in [
        (
            "textDocument/hover",
            -32802,
            json!({"retriggerRequest": false}),
        ),
        (
            "textDocument/hover",
            -32802,
            json!({"retriggerRequest": "true"}),
        ),
        (
            "textDocument/hover",
            -32800,
            json!({"retriggerRequest": false}),
        ),
        (
            "textDocument/hover",
            -32801,
            json!({"retriggerRequest": true}),
        ),
        (
            "textDocument/hover",
            -32603,
            json!({"retriggerRequest": true}),
        ),
        ("initialize", -32802, json!({"retriggerRequest": true})),
        ("shutdown", -32802, json!({"retriggerRequest": true})),
    ] {
        let (active, mut receiver) = server().await;
        let pending =
            request(&active, method, 5_000, &CancellationToken::new());
        let first = receive(&mut receiver).await;
        respond(&active, &first, canceled(code, data.clone())).await;
        let error = pending.await.unwrap().unwrap_err();
        assert!(
            matches!(error, LspError::ResponseError {code: actual, data: actual_data, ..} if actual == code && actual_data == Some(data))
        );
        assert!(receiver.try_recv().is_err());
    }
}

#[tokio::test(start_paused = true)]
async fn retry_deadline_includes_the_initial_slot_response_and_readiness_wait()
{
    let (active, mut receiver) = server().await;
    readiness(&active, true, false).await;
    let permit = active.request_permits.acquire().await.unwrap();
    let pending = request(
        &active,
        "textDocument/hover",
        600,
        &CancellationToken::new(),
    );
    tokio::task::yield_now().await;
    tokio::time::advance(Duration::from_millis(200)).await;
    drop(permit);
    let first = receive(&mut receiver).await;
    tokio::time::advance(Duration::from_millis(300)).await;
    respond(&active, &first, canceled(-32802, JsonValue::Null)).await;
    let started = Instant::now();
    let error = pending.await.unwrap().unwrap_err();
    assert!(
        matches!(error, LspError::RequestTimeout {timeout, ..} if timeout == Duration::from_millis(600))
    );
    assert!(Instant::now() - started <= Duration::from_millis(101));
    assert!(receiver.try_recv().is_err());
}

#[tokio::test(start_paused = true)]
async fn retry_cancellation_during_readiness_prevents_replay() {
    let (active, mut receiver) = server().await;
    readiness(&active, true, false).await;
    let token = CancellationToken::new();
    let pending = request(&active, "textDocument/hover", 5_000, &token);
    let first = receive(&mut receiver).await;
    respond(&active, &first, canceled(-32802, JsonValue::Null)).await;
    tokio::time::advance(Duration::from_millis(100)).await;
    token.cancel();
    readiness(&active, true, true).await;
    assert!(matches!(
        pending.await.unwrap(),
        Err(LspError::RequestCanceled { .. })
    ));
    assert!(receiver.try_recv().is_err());
    assert!(active.requests.lock().await.pending.is_empty());
}

#[tokio::test(start_paused = true)]
async fn retry_in_flight_preserves_deadline_and_forwards_cancellation_to_its_id()
 {
    for cancel in [false, true] {
        let (active, mut receiver) = server().await;
        let token = CancellationToken::new();
        let pending = request(&active, "textDocument/hover", 600, &token);
        let first = receive(&mut receiver).await;
        tokio::time::advance(Duration::from_millis(200)).await;
        respond(&active, &first, canceled(-32802, JsonValue::Null)).await;
        let second = receive(&mut receiver).await;
        assert_ne!(first["id"], second["id"]);
        let started = Instant::now();
        if cancel {
            token.cancel();
        }
        let error = pending.await.unwrap().unwrap_err();
        if cancel {
            assert!(matches!(error, LspError::RequestCanceled { .. }));
        } else {
            assert!(
                matches!(error, LspError::RequestTimeout {timeout, ..} if timeout == Duration::from_millis(600))
            );
            assert!(Instant::now() - started <= Duration::from_millis(351));
        }
        let notification = receive(&mut receiver).await;
        assert_eq!(notification["method"], "$/cancelRequest");
        assert_eq!(notification["params"]["id"], second["id"]);
        respond(&active, &first, json!({"result": "stale"})).await;
        respond(&active, &second, json!({"result": "late"})).await;
        assert!(active.requests.lock().await.pending.is_empty());
        let next = request(
            &active,
            "textDocument/hover",
            600,
            &CancellationToken::new(),
        );
        let message = receive(&mut receiver).await;
        respond(&active, &message, json!({"result": "current"})).await;
        assert_eq!(next.await.unwrap().unwrap(), "current");
    }
}
