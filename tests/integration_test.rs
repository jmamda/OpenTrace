//! Integration tests for the trace proxy.
//!
//! Each test:
//!   1. Binds a mock upstream axum server on an OS-assigned port (port 0).
//!   2. Binds the proxy itself on another OS-assigned port.
//!   3. Opens a real SQLite database in the system temp directory so it
//!      is fully isolated and exercises the real storage code path.
//!   4. Posts an HTTP request through the proxy and asserts on both the HTTP
//!      response and the row written to the database.
//!
//! No test-only dependencies are added — everything comes from the existing
//! Cargo.toml (axum, reqwest, tokio, serde_json, tokio::sync::mpsc).

use std::path::PathBuf;
use tokio::sync::mpsc;
use trace::{proxy, store};

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Create a unique temp-directory path for a test database.
/// Uses nanosecond timestamp so that parallel tests don't collide.
fn temp_db_path() -> PathBuf {
    use std::time::{SystemTime, UNIX_EPOCH};
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .subsec_nanos();
    // Include a static atomic counter for extra uniqueness within the same
    // nanosecond (possible under parallel test execution).
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let seq = COUNTER.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!("trace_inttest_{}_{}.db", nanos, seq))
}

/// Start the proxy pointing at `upstream_url`, writing captured records into
/// a database at `db_path`.  Returns the port the proxy is listening on.
///
/// Internally the function spawns two tasks:
///   - a background DB-writer task that reads from the channel and inserts rows
///   - the axum server task
///
/// The test opens its *own* `Store::open_at(&db_path)` to query results,
/// avoiding the `!Sync` constraint on `rusqlite::Connection`.
async fn start_proxy(upstream_url: String, db_path: &PathBuf) -> u16 {
    // Writer task owns its own Store (Connection is Send, so it can be moved
    // into a spawn, but not shared across tasks via Arc which requires Sync).
    let writer_store =
        store::Store::open_at(db_path).expect("open writer store");

    let (store_tx, mut store_rx) = mpsc::channel::<store::CallRecord>(1024);

    tokio::spawn(async move {
        while let Some(record) = store_rx.recv().await {
            if let Err(e) = writer_store.insert(&record) {
                eprintln!("[test] db write error: {e}");
            }
        }
    });

    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(10))
        .build()
        .expect("build http client");

    let state = proxy::ProxyState {
        upstream: std::sync::Arc::new(upstream_url),
        routes: vec![],
        client,
        store_tx,
        verbose: false,
        no_request_bodies: false,
        redact_fields: vec![],
    };

    let app = proxy::router(state);

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind proxy listener");
    let proxy_port = listener.local_addr().unwrap().port();

    tokio::spawn(async move {
        axum::serve(listener, app).await.ok();
    });

    proxy_port
}

/// Poll the database at `db_path` until at least one record appears or we
/// time out (5 seconds).  Opens a new connection each poll — cheap for tests.
async fn wait_for_record(db_path: &PathBuf) -> store::CallRecord {
    let deadline =
        tokio::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        // Re-open is cheap and avoids the Sync constraint entirely.
        if let Ok(s) = store::Store::open_at(db_path) {
            let filter = store::QueryFilter::default();
            if let Ok(rows) = s.query_filtered(10, &filter) {
                if !rows.is_empty() {
                    return rows.into_iter().next().unwrap();
                }
            }
        }
        if tokio::time::Instant::now() >= deadline {
            panic!("timed out waiting for DB record");
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
}

// ---------------------------------------------------------------------------
// Test 1: non-streaming request is captured with correct token counts
// ---------------------------------------------------------------------------

#[tokio::test]
async fn non_streaming_request_captured() {
    use axum::{response::IntoResponse, routing::post, Router};

    // --- Mock upstream --------------------------------------------------------
    let mock_app = Router::new().route(
        "/v1/chat/completions",
        post(|| async {
            let body = concat!(
                r#"{"id":"chatcmpl-xyz","object":"chat.completion","#,
                r#""choices":[{"message":{"role":"assistant","content":"Hello"},"#,
                r#""finish_reason":"stop","index":0}],"#,
                r#""usage":{"prompt_tokens":10,"completion_tokens":5,"total_tokens":15}}"#,
            );
            (
                axum::http::StatusCode::OK,
                [(axum::http::header::CONTENT_TYPE, "application/json")],
                body,
            )
                .into_response()
        }),
    );

    let mock_listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .unwrap();
    let mock_port = mock_listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        axum::serve(mock_listener, mock_app).await.ok();
    });

    // --- Proxy ----------------------------------------------------------------
    let db_path = temp_db_path();
    let upstream_url = format!("http://127.0.0.1:{}", mock_port);
    let proxy_port = start_proxy(upstream_url, &db_path).await;

    // --- Request --------------------------------------------------------------
    let client = reqwest::Client::new();
    let resp = client
        .post(format!(
            "http://127.0.0.1:{}/v1/chat/completions",
            proxy_port
        ))
        .header("content-type", "application/json")
        .body(r#"{"model":"gpt-4o","messages":[{"role":"user","content":"hi"}]}"#)
        .send()
        .await
        .expect("POST to proxy");

    assert_eq!(resp.status(), 200, "proxy should forward 200");
    let text = resp.text().await.unwrap();
    assert!(text.contains("Hello"), "response body should contain Hello");

    // --- DB assertions --------------------------------------------------------
    let record = wait_for_record(&db_path).await;
    assert_eq!(record.model, "gpt-4o");
    assert_eq!(record.status_code, 200);
    assert_eq!(record.input_tokens, Some(10));
    assert_eq!(record.output_tokens, Some(5));
    assert!(record.error.is_none());
    // For non-streaming, ttft_ms should equal latency_ms.
    assert!(record.ttft_ms.is_some(), "ttft_ms should be set for non-streaming");
    assert_eq!(record.ttft_ms, Some(record.latency_ms),
        "for non-streaming, ttft_ms should equal latency_ms");

    // Verify row count via a fresh connection.
    let query_store = store::Store::open_at(&db_path).unwrap();
    assert_eq!(query_store.total_calls().unwrap(), 1);

    std::fs::remove_file(&db_path).ok();
}

// ---------------------------------------------------------------------------
// Test 2: streaming request is captured with token counts and response body
// ---------------------------------------------------------------------------

#[tokio::test]
async fn streaming_request_captured() {
    use axum::{response::IntoResponse, routing::post, Router};

    // SSE payload: first delta chunk, then a chunk with usage, then [DONE].
    let sse_body = concat!(
        "data: {\"id\":\"x\",\"choices\":[{\"delta\":{\"role\":\"assistant\"}}]}\n\n",
        "data: {\"id\":\"x\",\"choices\":[{\"delta\":{\"content\":\"Hi\"}}],",
        "\"usage\":{\"prompt_tokens\":8,\"completion_tokens\":3,\"total_tokens\":11}}\n\n",
        "data: [DONE]\n\n",
    );

    let mock_app = Router::new().route(
        "/v1/chat/completions",
        post(move || async move {
            (
                axum::http::StatusCode::OK,
                [(axum::http::header::CONTENT_TYPE, "text/event-stream")],
                sse_body,
            )
                .into_response()
        }),
    );

    let mock_listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .unwrap();
    let mock_port = mock_listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        axum::serve(mock_listener, mock_app).await.ok();
    });

    // --- Proxy ----------------------------------------------------------------
    let db_path = temp_db_path();
    let upstream_url = format!("http://127.0.0.1:{}", mock_port);
    let proxy_port = start_proxy(upstream_url, &db_path).await;

    // --- Request --------------------------------------------------------------
    let client = reqwest::Client::new();
    let resp = client
        .post(format!(
            "http://127.0.0.1:{}/v1/chat/completions",
            proxy_port
        ))
        .header("content-type", "application/json")
        .body(
            r#"{"model":"claude-3-5-sonnet-20241022","stream":true,"messages":[{"role":"user","content":"hi"}]}"#,
        )
        .send()
        .await
        .expect("POST to proxy");

    assert_eq!(resp.status(), 200);

    // Verify the content-type header is forwarded as SSE.
    let ct = resp
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    assert!(
        ct.contains("text/event-stream"),
        "content-type should be text/event-stream, got: {}",
        ct
    );

    // Consume the entire body so the proxy's streaming task can finish and
    // send the CallRecord into the channel.
    let _body = resp.bytes().await.unwrap();

    // --- DB assertions --------------------------------------------------------
    let record = wait_for_record(&db_path).await;
    assert_eq!(record.model, "claude-3-5-sonnet-20241022");
    assert_eq!(record.status_code, 200);
    assert_eq!(record.input_tokens, Some(8));
    assert_eq!(record.output_tokens, Some(3));
    assert!(record.error.is_none());

    // TTFT should be captured for streaming calls.
    assert!(record.ttft_ms.is_some(), "ttft_ms should be captured for streaming calls");
    assert!(
        record.ttft_ms.unwrap() <= record.latency_ms,
        "ttft_ms ({:?}) should be <= latency_ms ({})",
        record.ttft_ms, record.latency_ms
    );

    // Streaming response body should now be captured (extracted from SSE content deltas).
    assert!(
        record.response_body.is_some(),
        "response_body should be captured for streaming calls"
    );
    assert!(
        record.response_body.as_deref().unwrap_or("").contains("Hi"),
        "response_body should contain the content text 'Hi', got: {:?}",
        record.response_body
    );

    std::fs::remove_file(&db_path).ok();
}

// ---------------------------------------------------------------------------
// Test 3: upstream HTTP 500 is recorded in the DB
// ---------------------------------------------------------------------------

#[tokio::test]
async fn upstream_error_recorded() {
    use axum::{response::IntoResponse, routing::post, Router};

    let mock_app = Router::new().route(
        "/v1/chat/completions",
        post(|| async {
            (
                axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                [(axum::http::header::CONTENT_TYPE, "application/json")],
                r#"{"error":"internal server error"}"#,
            )
                .into_response()
        }),
    );

    let mock_listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .unwrap();
    let mock_port = mock_listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        axum::serve(mock_listener, mock_app).await.ok();
    });

    // --- Proxy ----------------------------------------------------------------
    let db_path = temp_db_path();
    let upstream_url = format!("http://127.0.0.1:{}", mock_port);
    let proxy_port = start_proxy(upstream_url, &db_path).await;

    // --- Request --------------------------------------------------------------
    let client = reqwest::Client::new();
    let resp = client
        .post(format!(
            "http://127.0.0.1:{}/v1/chat/completions",
            proxy_port
        ))
        .header("content-type", "application/json")
        .body(r#"{"model":"gpt-4o","messages":[{"role":"user","content":"hi"}]}"#)
        .send()
        .await
        .expect("POST to proxy");

    // The proxy forwards the upstream status unchanged.
    assert_eq!(resp.status(), 500, "proxy should forward 500");

    // --- DB assertions --------------------------------------------------------
    let record = wait_for_record(&db_path).await;
    assert_eq!(record.status_code, 500, "DB record should have status_code=500");

    std::fs::remove_file(&db_path).ok();
}

// ---------------------------------------------------------------------------
// Test 4: upstream connection failure → 502 + status_code=0 + error IS NOT NULL
// ---------------------------------------------------------------------------

#[tokio::test]
async fn upstream_connection_failure() {
    // Obtain a free port from the OS, then immediately drop the listener so
    // that nothing is actually accepting connections on that port.
    let ephemeral = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .unwrap();
    let dead_port = ephemeral.local_addr().unwrap().port();
    drop(ephemeral);

    let dead_upstream = format!("http://127.0.0.1:{}", dead_port);

    // --- Proxy ----------------------------------------------------------------
    let db_path = temp_db_path();
    let proxy_port = start_proxy(dead_upstream, &db_path).await;

    // --- Request --------------------------------------------------------------
    let client = reqwest::Client::new();
    let resp = client
        .post(format!(
            "http://127.0.0.1:{}/v1/chat/completions",
            proxy_port
        ))
        .header("content-type", "application/json")
        .body(r#"{"model":"gpt-4o","messages":[{"role":"user","content":"hi"}]}"#)
        .send()
        .await
        .expect("POST should reach the proxy even though upstream is dead");

    // The proxy returns 502 Bad Gateway when the upstream connection fails.
    assert_eq!(resp.status(), 502, "proxy should return 502 on connection failure");

    // --- DB assertions --------------------------------------------------------
    let record = wait_for_record(&db_path).await;
    assert_eq!(
        record.status_code, 0,
        "status_code should be 0 for a connection-level failure"
    );
    assert!(
        record.error.is_some(),
        "error field should be set when upstream is unreachable"
    );
    assert!(
        record.ttft_ms.is_none(),
        "ttft_ms should be None when connection fails"
    );

    std::fs::remove_file(&db_path).ok();
}

// ---------------------------------------------------------------------------
// Test 5: /health endpoint returns 200 OK
// ---------------------------------------------------------------------------

#[tokio::test]
async fn health_endpoint_returns_200() {
    // Start proxy with a dummy upstream (health check doesn't touch upstream).
    let db_path = temp_db_path();
    let proxy_port = start_proxy("http://127.0.0.1:1".to_string(), &db_path).await;

    let client = reqwest::Client::new();
    let resp = client
        .get(format!("http://127.0.0.1:{}/health", proxy_port))
        .send()
        .await
        .expect("GET /health should succeed");

    assert_eq!(resp.status(), 200, "/health must return 200 OK");

    std::fs::remove_file(&db_path).ok();
}

// ---------------------------------------------------------------------------
// Test 6: embeddings endpoint — output_tokens defaults to 0
// ---------------------------------------------------------------------------

#[tokio::test]
async fn embeddings_tokens_extracted() {
    use axum::{response::IntoResponse, routing::post, Router};

    let mock_app = Router::new().route(
        "/v1/embeddings",
        post(|| async {
            // OpenAI embeddings response: prompt_tokens only, no completion_tokens.
            let body = r#"{
                "object": "list",
                "data": [{"object":"embedding","embedding":[0.1,0.2],"index":0}],
                "usage": {"prompt_tokens": 50, "total_tokens": 50}
            }"#;
            (
                axum::http::StatusCode::OK,
                [(axum::http::header::CONTENT_TYPE, "application/json")],
                body,
            )
                .into_response()
        }),
    );

    let mock_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let mock_port = mock_listener.local_addr().unwrap().port();
    tokio::spawn(async move { axum::serve(mock_listener, mock_app).await.ok(); });

    let db_path = temp_db_path();
    let upstream_url = format!("http://127.0.0.1:{}", mock_port);
    let proxy_port = start_proxy(upstream_url, &db_path).await;

    let client = reqwest::Client::new();
    let resp = client
        .post(format!("http://127.0.0.1:{}/v1/embeddings", proxy_port))
        .header("content-type", "application/json")
        .body(r#"{"model":"text-embedding-3-small","input":"hello world"}"#)
        .send()
        .await
        .expect("POST to proxy");

    assert_eq!(resp.status(), 200);
    let _body = resp.text().await.unwrap();

    let record = wait_for_record(&db_path).await;
    assert_eq!(record.input_tokens, Some(50),
        "input_tokens should be extracted from embeddings response");
    assert_eq!(record.output_tokens, Some(0),
        "output_tokens should default to 0 for embeddings");
    assert!(record.cost_usd.is_some(), "cost should be calculated for embeddings");
    assert!(record.cost_usd.unwrap() > 0.0, "embedding cost should be > 0");

    std::fs::remove_file(&db_path).ok();
}

// ---------------------------------------------------------------------------
// Test 7: x-request-id from upstream is stored in provider_request_id
// ---------------------------------------------------------------------------

#[tokio::test]
async fn provider_request_id_stored() {
    use axum::{response::IntoResponse, routing::post, Router};

    let mock_app = Router::new().route(
        "/v1/chat/completions",
        post(|| async {
            let body = r#"{"choices":[{"message":{"content":"ok"}}],"usage":{"prompt_tokens":5,"completion_tokens":2}}"#;
            (
                axum::http::StatusCode::OK,
                [
                    (axum::http::header::CONTENT_TYPE, "application/json"),
                    // Provider returns an x-request-id for support correlation.
                    (axum::http::HeaderName::from_static("x-request-id"), "req-abc-123"),
                ],
                body,
            )
                .into_response()
        }),
    );

    let mock_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let mock_port = mock_listener.local_addr().unwrap().port();
    tokio::spawn(async move { axum::serve(mock_listener, mock_app).await.ok(); });

    let db_path = temp_db_path();
    let upstream_url = format!("http://127.0.0.1:{}", mock_port);
    let proxy_port = start_proxy(upstream_url, &db_path).await;

    let client = reqwest::Client::new();
    let _resp = client
        .post(format!("http://127.0.0.1:{}/v1/chat/completions", proxy_port))
        .header("content-type", "application/json")
        .body(r#"{"model":"gpt-4o","messages":[]}"#)
        .send()
        .await
        .expect("POST to proxy");
    let _ = _resp.text().await;

    let record = wait_for_record(&db_path).await;
    assert_eq!(
        record.provider_request_id,
        Some("req-abc-123".to_string()),
        "provider_request_id should be stored from x-request-id header"
    );

    std::fs::remove_file(&db_path).ok();
}

// ---------------------------------------------------------------------------
// Routing helpers
// ---------------------------------------------------------------------------

/// Like `start_proxy` but accepts a routing table.
async fn start_proxy_with_routes(
    default_upstream: String,
    routes: Vec<proxy::UpstreamRoute>,
    db_path: &PathBuf,
) -> u16 {
    let writer_store = store::Store::open_at(db_path).expect("open writer store");
    let (store_tx, mut store_rx) = mpsc::channel::<store::CallRecord>(1024);
    tokio::spawn(async move {
        while let Some(record) = store_rx.recv().await {
            if let Err(e) = writer_store.insert(&record) {
                eprintln!("[test] db write error: {e}");
            }
        }
    });
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(10))
        .build()
        .expect("build http client");
    let state = proxy::ProxyState {
        upstream: std::sync::Arc::new(default_upstream),
        routes,
        client,
        store_tx,
        verbose: false,
        no_request_bodies: false,
        redact_fields: vec![],
    };
    let app = proxy::router(state);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind proxy listener");
    let proxy_port = listener.local_addr().unwrap().port();
    tokio::spawn(async move { axum::serve(listener, app).await.ok(); });
    proxy_port
}

// ---------------------------------------------------------------------------
// Test 9: path routing sends request to the routed upstream
// ---------------------------------------------------------------------------

#[tokio::test]
async fn path_routing_sends_request_to_routed_upstream() {
    use axum::{response::IntoResponse, routing::any, Router};

    // Mock A — the routed upstream (responds to anything with a distinct body)
    let mock_a = Router::new().route("/*path", any(|| async {
        (
            axum::http::StatusCode::OK,
            [(axum::http::header::CONTENT_TYPE, "application/json")],
            r#"{"mock":"upstream_a","choices":[{"message":{"content":"from_a"}}],"usage":{"prompt_tokens":5,"completion_tokens":2}}"#,
        ).into_response()
    })).route("/", any(|| async {
        (axum::http::StatusCode::OK, [(axum::http::header::CONTENT_TYPE, "application/json")], "{}").into_response()
    }));

    // Mock B — the default upstream
    let mock_b = Router::new().route("/*path", any(|| async {
        (
            axum::http::StatusCode::OK,
            [(axum::http::header::CONTENT_TYPE, "application/json")],
            r#"{"mock":"upstream_b","choices":[{"message":{"content":"from_b"}}],"usage":{"prompt_tokens":5,"completion_tokens":2}}"#,
        ).into_response()
    })).route("/", any(|| async {
        (axum::http::StatusCode::OK, [(axum::http::header::CONTENT_TYPE, "application/json")], "{}").into_response()
    }));

    let listener_a = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port_a = listener_a.local_addr().unwrap().port();
    tokio::spawn(async move { axum::serve(listener_a, mock_a).await.ok(); });

    let listener_b = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port_b = listener_b.local_addr().unwrap().port();
    tokio::spawn(async move { axum::serve(listener_b, mock_b).await.ok(); });

    let db_path = temp_db_path();
    let routes = vec![proxy::UpstreamRoute {
        path: "/v1/messages".to_string(),
        upstream: std::sync::Arc::new(format!("http://127.0.0.1:{}", port_a)),
    }];
    let proxy_port = start_proxy_with_routes(
        format!("http://127.0.0.1:{}", port_b),
        routes,
        &db_path,
    ).await;

    let client = reqwest::Client::new();
    let resp = client
        .post(format!("http://127.0.0.1:{}/v1/messages", proxy_port))
        .header("content-type", "application/json")
        .body(r#"{"model":"claude-opus-4-6","messages":[{"role":"user","content":"hi"}],"max_tokens":10}"#)
        .send()
        .await
        .expect("POST to proxy");

    assert_eq!(resp.status(), 200);
    let body = resp.text().await.unwrap();
    assert!(body.contains("upstream_a"), "response should come from mock A, got: {}", body);

    let record = wait_for_record(&db_path).await;
    assert!(
        record.response_body.as_deref().unwrap_or("").contains("upstream_a"),
        "DB response_body should show upstream_a was hit, got: {:?}", record.response_body
    );

    std::fs::remove_file(&db_path).ok();
}

// ---------------------------------------------------------------------------
// Test 10: path routing falls back to default when no route matches
// ---------------------------------------------------------------------------

#[tokio::test]
async fn path_routing_fallback_uses_default_upstream() {
    use axum::{response::IntoResponse, routing::any, Router};

    let mock_a = Router::new().route("/*path", any(|| async {
        (axum::http::StatusCode::OK, [(axum::http::header::CONTENT_TYPE, "application/json")],
         r#"{"mock":"upstream_a","usage":{"prompt_tokens":1,"completion_tokens":1}}"#).into_response()
    }));
    let mock_b = Router::new().route("/*path", any(|| async {
        (axum::http::StatusCode::OK, [(axum::http::header::CONTENT_TYPE, "application/json")],
         r#"{"mock":"upstream_b","choices":[{"message":{"content":"from_b"}}],"usage":{"prompt_tokens":5,"completion_tokens":2}}"#).into_response()
    }));

    let listener_a = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port_a = listener_a.local_addr().unwrap().port();
    tokio::spawn(async move { axum::serve(listener_a, mock_a).await.ok(); });

    let listener_b = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port_b = listener_b.local_addr().unwrap().port();
    tokio::spawn(async move { axum::serve(listener_b, mock_b).await.ok(); });

    let db_path = temp_db_path();
    // Route /v1/messages → mock A, everything else → mock B (default)
    let routes = vec![proxy::UpstreamRoute {
        path: "/v1/messages".to_string(),
        upstream: std::sync::Arc::new(format!("http://127.0.0.1:{}", port_a)),
    }];
    let proxy_port = start_proxy_with_routes(
        format!("http://127.0.0.1:{}", port_b),
        routes,
        &db_path,
    ).await;

    // POST to /v1/chat/completions — no matching route, should hit mock B
    let client = reqwest::Client::new();
    let resp = client
        .post(format!("http://127.0.0.1:{}/v1/chat/completions", proxy_port))
        .header("content-type", "application/json")
        .body(r#"{"model":"gpt-4o","messages":[{"role":"user","content":"hi"}]}"#)
        .send()
        .await
        .expect("POST to proxy");

    assert_eq!(resp.status(), 200);
    let body = resp.text().await.unwrap();
    assert!(body.contains("upstream_b"), "response should come from mock B (default), got: {}", body);

    let record = wait_for_record(&db_path).await;
    assert!(
        record.response_body.as_deref().unwrap_or("").contains("upstream_b"),
        "DB response_body should show upstream_b (default) was hit, got: {:?}", record.response_body
    );

    std::fs::remove_file(&db_path).ok();
}

// ---------------------------------------------------------------------------
// Test 11: sub-path of routed prefix still routes correctly
// ---------------------------------------------------------------------------

#[tokio::test]
async fn path_routing_subpath_matches_prefix() {
    use axum::{response::IntoResponse, routing::any, Router};

    let mock_a = Router::new().route("/*path", any(|| async {
        (axum::http::StatusCode::OK, [(axum::http::header::CONTENT_TYPE, "application/json")],
         r#"{"mock":"upstream_a","usage":{"prompt_tokens":1,"completion_tokens":1}}"#).into_response()
    }));

    // Default is a dead port — if routing works, we should never hit it
    let listener_a = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port_a = listener_a.local_addr().unwrap().port();
    tokio::spawn(async move { axum::serve(listener_a, mock_a).await.ok(); });

    let ephemeral = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let dead_port = ephemeral.local_addr().unwrap().port();
    drop(ephemeral);

    let db_path = temp_db_path();
    let routes = vec![proxy::UpstreamRoute {
        path: "/v1/messages".to_string(),
        upstream: std::sync::Arc::new(format!("http://127.0.0.1:{}", port_a)),
    }];
    let proxy_port = start_proxy_with_routes(
        format!("http://127.0.0.1:{}", dead_port), // default is dead
        routes,
        &db_path,
    ).await;

    // POST to /v1/messages/count — starts with /v1/messages, should hit mock A not dead default
    let client = reqwest::Client::new();
    let resp = client
        .post(format!("http://127.0.0.1:{}/v1/messages/count", proxy_port))
        .header("content-type", "application/json")
        .body(r#"{"model":"claude-sonnet-4-6"}"#)
        .send()
        .await
        .expect("POST to proxy");

    // Should succeed (200) because mock A handled it, not the dead default
    assert_eq!(resp.status(), 200, "subpath /v1/messages/count should route to mock A");

    std::fs::remove_file(&db_path).ok();
}

// ---------------------------------------------------------------------------
// Test 12: path-segment boundary — /v1/messages2 must NOT route to /v1/messages
// ---------------------------------------------------------------------------

#[tokio::test]
async fn path_routing_boundary_no_sibling_bleed() {
    use axum::{response::IntoResponse, routing::any, Router};

    // Mock A: the routed upstream for /v1/messages
    let mock_a = Router::new().route("/*path", any(|| async {
        (axum::http::StatusCode::OK, [(axum::http::header::CONTENT_TYPE, "application/json")],
         r#"{"mock":"upstream_a","usage":{"prompt_tokens":1,"completion_tokens":1}}"#).into_response()
    }));
    // Mock B: the default upstream — should handle /v1/messages2
    let mock_b = Router::new().route("/*path", any(|| async {
        (axum::http::StatusCode::OK, [(axum::http::header::CONTENT_TYPE, "application/json")],
         r#"{"mock":"upstream_b","usage":{"prompt_tokens":1,"completion_tokens":1}}"#).into_response()
    }));

    let listener_a = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port_a = listener_a.local_addr().unwrap().port();
    tokio::spawn(async move { axum::serve(listener_a, mock_a).await.ok(); });

    let listener_b = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port_b = listener_b.local_addr().unwrap().port();
    tokio::spawn(async move { axum::serve(listener_b, mock_b).await.ok(); });

    let db_path = temp_db_path();
    let routes = vec![proxy::UpstreamRoute {
        path: "/v1/messages".to_string(),
        upstream: std::sync::Arc::new(format!("http://127.0.0.1:{}", port_a)),
    }];
    let proxy_port = start_proxy_with_routes(
        format!("http://127.0.0.1:{}", port_b),
        routes,
        &db_path,
    ).await;

    let client = reqwest::Client::new();
    // /v1/messages2 must NOT match the /v1/messages route — the '2' is not a '/'
    let resp = client
        .post(format!("http://127.0.0.1:{}/v1/messages2", proxy_port))
        .header("content-type", "application/json")
        .body(r#"{"model":"gpt-4o","messages":[]}"#)
        .send()
        .await
        .expect("POST to proxy");

    assert_eq!(resp.status(), 200);
    let body = resp.text().await.unwrap();
    assert!(
        body.contains("upstream_b"),
        "/v1/messages2 should fall through to default (upstream_b), got: {}",
        body
    );

    std::fs::remove_file(&db_path).ok();
}

// ---------------------------------------------------------------------------
// Test 8: DB retention pruning removes old records
// ---------------------------------------------------------------------------

#[tokio::test]
async fn retention_prunes_old_records() {
    let store = store::Store::open_in_memory().unwrap();

    // Insert an obviously old record.
    let mut old = store::CallRecord {
        id: "old-id-1".to_string(),
        timestamp: "2020-01-01T00:00:00.000Z".to_string(),
        provider: "openai".to_string(),
        model: "gpt-4o".to_string(),
        endpoint: "/v1/chat/completions".to_string(),
        status_code: 200,
        latency_ms: 100,
        ttft_ms: None,
        input_tokens: Some(10),
        output_tokens: Some(5),
        cost_usd: Some(0.001),
        request_body: None,
        response_body: None,
        error: None,
        provider_request_id: None,
    };
    store.insert(&old).unwrap();

    // Insert a current record.
    old.id = "new-id-1".to_string();
    old.timestamp = store::now_iso();
    store.insert(&old).unwrap();

    assert_eq!(store.total_calls().unwrap(), 2);

    // Prune records older than 1 day — should remove the 2020 record.
    let deleted = store.prune_older_than(1).unwrap();
    assert_eq!(deleted, 1, "one old record should be pruned");
    assert_eq!(store.total_calls().unwrap(), 1, "one recent record should remain");
}
