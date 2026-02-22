use anyhow::Result;
use axum::{
    body::Body,
    extract::State,
    http::{HeaderMap, Method, StatusCode, Uri},
    response::Response,
    routing::{any, get},
    Router,
};
use bytes::Bytes;
use futures_util::StreamExt;
use std::sync::{
    atomic::{AtomicU64, Ordering},
    Arc,
};
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;
use uuid::Uuid;

use crate::capture;
use crate::store::{self, CallRecord};

/// Maximum request body buffered in memory before forwarding upstream.
const MAX_REQUEST_BODY_BYTES: usize = 16 * 1024 * 1024; // 16 MB

/// Maximum non-streaming response body size stored in the database.
/// Larger bodies are forwarded to the client but not persisted.
const MAX_STORED_RESPONSE_BYTES: usize = 10 * 1024 * 1024; // 10 MB

/// Maximum size of the extracted text content stored from a streaming response.
const MAX_STORED_STREAM_RESPONSE_BYTES: usize = 256 * 1024; // 256 KB

/// Maximum raw SSE bytes accumulated in memory per streaming call.
/// Prevents OOM at high concurrency with large streaming responses.
const MAX_ACCUMULATION_BYTES: usize = 4 * 1024 * 1024; // 4 MB

/// Records silently dropped due to the store channel being full.
/// Visible via `trace stats` when > 0.
pub static DROPPED_RECORDS: AtomicU64 = AtomicU64::new(0);

/// A single routing rule: requests whose path starts with `path` are forwarded
/// to `upstream` instead of the default.
#[derive(Clone, Debug)]
pub struct UpstreamRoute {
    pub path: String,
    pub upstream: Arc<String>,
}

#[derive(Clone)]
pub struct ProxyState {
    pub upstream: Arc<String>,
    /// Path-prefix routing table — empty means all requests go to `upstream`.
    pub routes: Vec<UpstreamRoute>,
    pub client: reqwest::Client,
    /// Bounded sender — DB writes happen in a background task.
    pub store_tx: mpsc::Sender<CallRecord>,
    pub verbose: bool,
    /// When true, request bodies (prompts) are not stored in the database.
    pub no_request_bodies: bool,
    /// Top-level JSON keys whose values are replaced with "[REDACTED]" before
    /// storing.  Forwarded traffic is never modified.
    pub redact_fields: Vec<String>,
}

pub fn router(state: ProxyState) -> Router {
    Router::new()
        .route("/health", get(|| async { StatusCode::OK }))
        .route("/*path", any(handle))
        .route("/", any(handle))
        .with_state(state)
}

/// Send a record to the DB writer task, incrementing the dropped counter if
/// the channel is full.  Non-blocking so hot request paths are never stalled.
fn try_store(store_tx: &mpsc::Sender<CallRecord>, record: CallRecord, verbose: bool) {
    match store_tx.try_send(record) {
        Ok(()) => {}
        Err(e) => {
            DROPPED_RECORDS.fetch_add(1, Ordering::Relaxed);
            if verbose {
                eprintln!("[trace] db write dropped (channel full): {e}");
            }
        }
    }
}

/// Return the upstream URL to use for this request path.
/// Evaluates routes in insertion order; first prefix match wins.
/// Falls back to `default` when no route matches or the table is empty.
///
/// Matching rules (same logic used in the shadowing-detection pass):
/// - The literal `"/"` route matches every path (catch-all).
/// - An exact match always wins.
/// - A prefix match only wins when the next character after the prefix is `'/'`,
///   preventing `/v1/messages` from accidentally matching `/v1/messages2`.
fn resolve_upstream<'a>(
    default: &'a Arc<String>,
    routes: &'a [UpstreamRoute],
    path: &str,
) -> &'a Arc<String> {
    for route in routes {
        let p = route.path.as_str();
        if p == "/" || path == p || (path.starts_with(p) && path[p.len()..].starts_with('/')) {
            return &route.upstream;
        }
    }
    default
}

async fn handle(
    State(state): State<ProxyState>,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    body: Body,
) -> Result<Response<Body>, StatusCode> {
    let id = Uuid::new_v4().to_string();
    let start = std::time::Instant::now();
    let timestamp = store::now_iso();
    // Store only the path in the DB; query strings may contain sensitive tokens.
    let endpoint = uri.path().to_string();
    let verbose = state.verbose;
    let no_request_bodies = state.no_request_bodies;
    let redact_fields = state.redact_fields.clone();
    let upstream = resolve_upstream(&state.upstream, &state.routes, uri.path()).clone();
    let client = state.client.clone();
    let store_tx = state.store_tx.clone();
    let provider = capture::detect_provider(&upstream);

    // Read and buffer request body.
    let req_bytes = axum::body::to_bytes(body, MAX_REQUEST_BODY_BYTES)
        .await
        .map_err(|_| StatusCode::BAD_REQUEST)?;

    // Avoid to_vec() — read directly from the Bytes buffer.
    let req_str = match std::str::from_utf8(&req_bytes) {
        Ok(s) => s.to_string(),
        Err(_) => {
            if verbose { eprintln!("[trace] warning: request body is not UTF-8"); }
            "(binary body)".to_string()
        }
    };

    // Single parse extracts both model and streaming flag.
    let (model, streaming) = capture::extract_request_info(&req_str);

    // Pre-compute the stored request body: apply field redaction (if configured)
    // and respect the no_request_bodies flag.  Forwarded bytes are never modified.
    let stored_req_body: Option<String> = if no_request_bodies {
        None
    } else {
        Some(capture::redact_json_fields(&req_str, &redact_fields))
    };

    // Warn on first use of unknown model so cost estimates are transparent.
    if verbose && !capture::is_known_model(&model) {
        eprintln!("[trace] warning: unknown model '{}', cost estimated at $1/$3 per MTok", model);
    }

    // Build upstream URL — include query string so params like ?limit=10 are forwarded.
    let upstream_url = {
        let mut url = format!("{}{}", upstream.trim_end_matches('/'), uri.path());
        if let Some(q) = uri.query() {
            url.push('?');
            url.push_str(q);
        }
        url
    };

    // Forward headers, skipping hop-by-hop and privacy-sensitive headers.
    //
    // Hop-by-hop (RFC 7230 §6.1): must not be forwarded.
    // Proxy credentials: proxy-authorization must not reach upstream (it is for
    //   the proxy, not the origin server).
    // Network topology: x-forwarded-for / x-real-ip / forwarded expose the
    //   client's internal IP to the upstream LLM provider — strip them.
    let mut req_builder = client
        .request(method.clone(), &upstream_url)
        .body(req_bytes.clone());

    for (name, value) in &headers {
        let n = name.as_str().to_lowercase();
        if matches!(n.as_str(),
            // Standard hop-by-hop headers
            "host" | "connection" | "transfer-encoding" | "content-length"
            | "te" | "trailers" | "upgrade" | "keep-alive"
            // Proxy credentials — must not reach the origin server
            | "proxy-authorization" | "proxy-connection"
            // Internal network topology — must not leak to LLM providers
            | "x-forwarded-for" | "x-real-ip" | "forwarded"
        ) {
            continue;
        }
        req_builder = req_builder.header(name, value);
    }

    if verbose {
        // Redact query string in the log line — it may contain API keys
        // (e.g. Google Gemini uses ?key=AIza...).
        let logged_url = match upstream_url.find('?') {
            Some(i) => format!("{}?[redacted]", &upstream_url[..i]),
            None => upstream_url.clone(),
        };
        eprintln!("[trace] → {} {} (model={}, stream={})", method, logged_url, model, streaming);
    }

    let upstream_resp = match req_builder.send().await {
        Ok(r) => r,
        Err(e) => {
            let latency_ms = start.elapsed().as_millis() as u64;
            let record = CallRecord {
                id,
                timestamp,
                provider,
                model,
                endpoint,
                status_code: 0,
                latency_ms,
                ttft_ms: None,
                input_tokens: None,
                output_tokens: None,
                cost_usd: None,
                request_body: stored_req_body,
                response_body: None,
                error: Some(e.to_string()),
                provider_request_id: None,
            };
            try_store(&store_tx, record, verbose);
            return Err(StatusCode::BAD_GATEWAY);
        }
    };

    let status = upstream_resp.status();
    let resp_headers = upstream_resp.headers().clone();

    // Extract provider's request-id for support-ticket correlation.
    let provider_request_id = resp_headers
        .get("x-request-id")
        .and_then(|v| v.to_str().ok())
        .map(String::from);

    // Trust the upstream Content-Type over the request's `stream` flag.
    // Some APIs return SSE even when the flag was false, or vice versa.
    let upstream_streaming = resp_headers
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .map(|ct| ct.contains("text/event-stream"))
        .unwrap_or(streaming);

    // Build response, skipping hop-by-hop headers.
    let mut builder = Response::builder().status(status.as_u16());
    for (k, v) in &resp_headers {
        let n = k.as_str().to_lowercase();
        if matches!(n.as_str(), "connection" | "transfer-encoding" | "keep-alive") {
            continue;
        }
        builder = builder.header(k, v);
    }

    if upstream_streaming {
        let (tx, rx) = mpsc::channel::<Result<Bytes, std::io::Error>>(1024);

        // Clone stored_req_body for the spawned closure (error path already
        // consumed the original via early return above).
        let req_body_for_spawn = stored_req_body;

        tokio::spawn(async move {
            // Accumulate all chunks (cheap: Bytes::clone increments Arc refcount)
            // up to MAX_ACCUMULATION_BYTES to prevent OOM on huge responses.
            // All chunks are still forwarded to the client regardless of cap.
            let mut accumulated: Vec<Bytes> = Vec::new();
            let mut accumulated_bytes: usize = 0;
            let mut overflow = false;
            let mut ttft_ms: Option<u64> = None;
            let mut stream = upstream_resp.bytes_stream();

            while let Some(chunk_result) = stream.next().await {
                match chunk_result {
                    Ok(chunk) => {
                        // Capture time-to-first-token on the first non-empty chunk.
                        if ttft_ms.is_none() && !chunk.is_empty() {
                            ttft_ms = Some(start.elapsed().as_millis() as u64);
                        }

                        if !overflow {
                            let chunk_len = chunk.len();
                            if accumulated_bytes + chunk_len <= MAX_ACCUMULATION_BYTES {
                                accumulated.push(chunk.clone()); // cheap: Arc refcount bump
                                accumulated_bytes += chunk_len;
                            } else {
                                overflow = true;
                                // Stop accumulating but keep forwarding to the client.
                            }
                        }

                        if tx.send(Ok(chunk)).await.is_err() {
                            break; // client disconnected
                        }
                    }
                    Err(e) => {
                        let _ = tx.send(Err(std::io::Error::new(
                            std::io::ErrorKind::Other, e.to_string()
                        ))).await;
                        break;
                    }
                }
            }

            let latency_ms = start.elapsed().as_millis() as u64;
            let (input_tokens, output_tokens) = capture::extract_usage_from_chunks(&accumulated)
                .map(|(i, o)| (Some(i), Some(o)))
                .unwrap_or((None, None));

            let cost_usd = match (input_tokens, output_tokens) {
                (Some(i), Some(o)) => Some(capture::estimate_cost(&model, i, o)),
                _ => None,
            };

            // Extract and store the text content from SSE chunks.
            let response_body = if overflow {
                Some(format!("(too large to store — {} bytes accumulated)", accumulated_bytes))
            } else {
                let text = capture::extract_response_text_from_chunks(&accumulated);
                if text.is_empty() {
                    None
                } else if text.len() > MAX_STORED_STREAM_RESPONSE_BYTES {
                    Some(format!("(truncated at 256KB — {} bytes total)", text.len()))
                } else {
                    Some(text)
                }
            };

            if verbose {
                eprintln!(
                    "[trace] ← {} {}ms ttft={:?}ms in={:?} out={:?} cost=${:.6}",
                    status.as_u16(), latency_ms,
                    ttft_ms, input_tokens, output_tokens,
                    cost_usd.unwrap_or(0.0)
                );
            }

            let record = CallRecord {
                id,
                timestamp,
                provider,
                model,
                endpoint,
                status_code: status.as_u16(),
                latency_ms,
                ttft_ms,
                input_tokens,
                output_tokens,
                cost_usd,
                request_body: req_body_for_spawn,
                response_body,
                error: None,
                provider_request_id,
            };
            try_store(&store_tx, record, verbose);
        });

        let stream_body = Body::from_stream(ReceiverStream::new(rx));
        Ok(builder.body(stream_body).map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?)
    } else {
        let resp_bytes = upstream_resp.bytes().await.map_err(|_| StatusCode::BAD_GATEWAY)?;
        let latency_ms = start.elapsed().as_millis() as u64;
        // For non-streaming, TTFT equals total latency — the full response IS
        // the first (and only) delivery of tokens.
        let ttft_ms = Some(latency_ms);

        // Only parse and store the body if it's within a reasonable size.
        // Very large binary responses are forwarded but not persisted.
        let resp_str = if resp_bytes.len() > MAX_STORED_RESPONSE_BYTES {
            if verbose {
                eprintln!("[trace] warning: response body ({} bytes) exceeds storage limit, body not stored",
                    resp_bytes.len());
            }
            // Store a truncation marker so the record is clearly incomplete,
            // rather than storing None which looks like a missing response.
            Some(format!(
                "(response body {} bytes — exceeds {}MB storage limit, not stored)",
                resp_bytes.len(),
                MAX_STORED_RESPONSE_BYTES / (1024 * 1024)
            ))
        } else {
            match std::str::from_utf8(&resp_bytes) {
                Ok(s) => Some(s.to_string()),
                Err(_) => {
                    if verbose { eprintln!("[trace] warning: response body is not UTF-8"); }
                    None
                }
            }
        };
        let (input_tokens, output_tokens) = resp_str.as_deref()
            .and_then(capture::extract_usage)
            .map(|(i, o)| (Some(i), Some(o)))
            .unwrap_or((None, None));

        let cost_usd = match (input_tokens, output_tokens) {
            (Some(i), Some(o)) => Some(capture::estimate_cost(&model, i, o)),
            _ => None,
        };

        if verbose {
            eprintln!(
                "[trace] ← {} {}ms in={:?} out={:?} cost=${:.6}",
                status.as_u16(), latency_ms,
                input_tokens, output_tokens,
                cost_usd.unwrap_or(0.0)
            );
        }

        let record = CallRecord {
            id,
            timestamp,
            provider,
            model,
            endpoint,
            status_code: status.as_u16(),
            latency_ms,
            ttft_ms,
            input_tokens,
            output_tokens,
            cost_usd,
            request_body: stored_req_body,
            response_body: resp_str,
            error: None,
            provider_request_id,
        };
        try_store(&store_tx, record, verbose);

        Ok(builder.body(Body::from(resp_bytes)).map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn route(path: &str, upstream: &str) -> UpstreamRoute {
        UpstreamRoute { path: path.to_string(), upstream: Arc::new(upstream.to_string()) }
    }

    #[test]
    fn resolve_upstream_empty_routes_always_default() {
        let default = Arc::new("https://api.openai.com".to_string());
        let result = resolve_upstream(&default, &[], "/v1/chat/completions");
        assert_eq!(result.as_str(), "https://api.openai.com");
    }

    #[test]
    fn resolve_upstream_matching_route_takes_precedence() {
        let default = Arc::new("https://api.openai.com".to_string());
        let routes = vec![route("/v1/messages", "https://api.anthropic.com")];
        let result = resolve_upstream(&default, &routes, "/v1/messages");
        assert_eq!(result.as_str(), "https://api.anthropic.com");
    }

    #[test]
    fn resolve_upstream_non_matching_path_uses_default() {
        let default = Arc::new("https://api.openai.com".to_string());
        let routes = vec![route("/v1/messages", "https://api.anthropic.com")];
        let result = resolve_upstream(&default, &routes, "/v1/chat/completions");
        assert_eq!(result.as_str(), "https://api.openai.com");
    }

    #[test]
    fn resolve_upstream_first_match_wins() {
        let default = Arc::new("https://default.example.com".to_string());
        let routes = vec![
            route("/v1", "https://first.example.com"),
            route("/v1/messages", "https://second.example.com"),
        ];
        // /v1 is listed first and is a prefix of /v1/messages → first wins
        let result = resolve_upstream(&default, &routes, "/v1/messages");
        assert_eq!(result.as_str(), "https://first.example.com");
    }

    #[test]
    fn resolve_upstream_subpath_matches_prefix() {
        let default = Arc::new("https://api.openai.com".to_string());
        let routes = vec![route("/v1/messages", "https://api.anthropic.com")];
        let result = resolve_upstream(&default, &routes, "/v1/messages/stream");
        assert_eq!(result.as_str(), "https://api.anthropic.com");
    }

    #[test]
    fn resolve_upstream_root_matches_all() {
        let default = Arc::new("https://api.openai.com".to_string());
        let routes = vec![route("/", "https://api.anthropic.com")];
        let result = resolve_upstream(&default, &routes, "/v1/chat/completions");
        assert_eq!(result.as_str(), "https://api.anthropic.com");
    }

    #[test]
    fn resolve_upstream_no_boundary_bleed_to_sibling() {
        // /v1/messages must NOT match /v1/messages2 — no segment boundary after prefix.
        let default = Arc::new("https://api.openai.com".to_string());
        let routes = vec![route("/v1/messages", "https://api.anthropic.com")];
        let result = resolve_upstream(&default, &routes, "/v1/messages2");
        assert_eq!(
            result.as_str(), "https://api.openai.com",
            "/v1/messages should not match /v1/messages2 — no path-segment boundary"
        );
    }

    #[test]
    fn resolve_upstream_slash_terminated_route_does_not_match_sibling() {
        // /v1 must NOT match /v1-legacy — hyphen is not a path separator.
        let default = Arc::new("https://default.example.com".to_string());
        let routes = vec![route("/v1", "https://routed.example.com")];
        // /v1/anything DOES match (segment boundary present)
        assert_eq!(
            resolve_upstream(&default, &routes, "/v1/chat").as_str(),
            "https://routed.example.com"
        );
        // /v1-legacy does NOT match (no boundary at position 3)
        assert_eq!(
            resolve_upstream(&default, &routes, "/v1-legacy").as_str(),
            "https://default.example.com"
        );
    }

    #[test]
    fn resolve_upstream_multiple_routes_correct_dispatch() {
        let default = Arc::new("https://default.example.com".to_string());
        let routes = vec![
            route("/v1/messages", "https://anthropic.example.com"),
            route("/v1/chat", "https://openai.example.com"),
        ];
        assert_eq!(
            resolve_upstream(&default, &routes, "/v1/messages").as_str(),
            "https://anthropic.example.com"
        );
        assert_eq!(
            resolve_upstream(&default, &routes, "/v1/chat/completions").as_str(),
            "https://openai.example.com"
        );
        assert_eq!(
            resolve_upstream(&default, &routes, "/v1/embeddings").as_str(),
            "https://default.example.com"
        );
    }
}
