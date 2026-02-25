//! OpenTelemetry OTLP span exporter — manual HTTP/JSON implementation.
//!
//! Exports each captured LLM call as a single OpenTelemetry span to an OTLP
//! HTTP endpoint (e.g. Jaeger, Grafana Tempo, Honeycomb, Datadog).
//!
//! Uses OTLP HTTP with JSON encoding — compatible with all major collectors.
//! No external OTel crates are required; only `reqwest` and `serde_json`
//! (already present in Cargo.toml) are used.
//!
//! ## Span mapping (gen_ai.* semantic conventions)
//! | OTel field                     | Source                                    |
//! |-------------------------------|-------------------------------------------|
//! | `name`                        | `"{operation} {model}"` e.g. `"chat gpt-4o"` |
//! | `startTimeUnixNano`            | `CallRecord.timestamp` → nanoseconds      |
//! | `endTimeUnixNano`              | start + `latency_ms` × 10⁶               |
//! | `gen_ai.system`               | mapped from `CallRecord.provider`         |
//! | `gen_ai.operation.name`        | `"chat"` or `"embeddings"`               |
//! | `gen_ai.request.model`         | `CallRecord.model`                        |
//! | `gen_ai.usage.input_tokens`    | `CallRecord.input_tokens` (when present)  |
//! | `gen_ai.usage.output_tokens`   | `CallRecord.output_tokens` (when present) |
//! | `openinference.span.kind`      | `"LLM"`                                  |
//! | `llm.model_name`              | `CallRecord.model`                        |
//! | `llm.provider`                | `CallRecord.provider`                     |
//! | `llm.token_count.prompt`      | `CallRecord.input_tokens` (when present)  |
//! | `llm.token_count.completion`  | `CallRecord.output_tokens` (when present) |
//! | `llm.token_count.total`       | sum of tokens (when both present)         |
//! | `http.status_code`            | `CallRecord.status_code` (backward compat)|
//! | `llm.cost_usd`                | `CallRecord.cost_usd` (when present)      |
//! | `llm.ttft_ms`                 | `CallRecord.ttft_ms` (when present)       |
//! | `error.type`                  | `CallRecord.error` text (when present)    |
//! | `otel.status_code`            | `"ERROR"` on failures, `"OK"` otherwise  |
//! | `exception.message`           | `CallRecord.error` (when present)         |

use reqwest::Client;
use serde_json::{json, Value};
use uuid::Uuid;

use crate::store::CallRecord;

/// Fire-and-forget OTLP span exporter.
///
/// Cloneable via `Arc` — share one exporter across tasks.
pub struct OtelExporter {
    /// OTLP HTTP base URL, e.g. `http://localhost:4318`.
    endpoint: String,
    client: Client,
}

impl OtelExporter {
    /// Create a new exporter.  `endpoint` is the OTLP HTTP base URL.
    pub fn new(endpoint: String) -> Self {
        let client = Client::builder()
            // Short timeout — OTel export is best-effort and must not slow
            // down the main request path.
            .timeout(std::time::Duration::from_secs(5))
            .build()
            .unwrap_or_default();
        OtelExporter {
            endpoint: endpoint.trim_end_matches('/').to_string(),
            client,
        }
    }

    /// Build an OTLP JSON span object from a `CallRecord`.
    ///
    /// Uses OTel `gen_ai.*` semantic conventions plus OpenInference attributes
    /// for compatibility with Arize Phoenix and other OpenInference-aware backends.
    pub fn build_span(r: &CallRecord) -> Value {
        let start_nanos = timestamp_to_nanos(&r.timestamp);
        let end_nanos = start_nanos.saturating_add(r.latency_ms.saturating_mul(1_000_000));

        // 128-bit trace ID (16 bytes) + 64-bit span ID (8 bytes), both base64-encoded.
        let trace_id = base64_16();
        let span_id = base64_8();

        let operation = derive_operation(&r.endpoint);
        let span_name = format!("{} {}", operation, r.model);

        let mut attrs: Vec<Value> = vec![
            // gen_ai.* semantic conventions (OTel standard)
            json!({"key": "gen_ai.system",         "value": {"stringValue": map_provider(&r.provider)}}),
            json!({"key": "gen_ai.operation.name", "value": {"stringValue": operation}}),
            json!({"key": "gen_ai.request.model",  "value": {"stringValue": r.model}}),
            // backward compat — kept for collectors that filter on status code
            json!({"key": "http.status_code",      "value": {"intValue": r.status_code as i64}}),
            // OpenInference attributes for Arize Phoenix compatibility
            json!({"key": "openinference.span.kind", "value": {"stringValue": "LLM"}}),
            json!({"key": "llm.model_name",          "value": {"stringValue": r.model}}),
            json!({"key": "llm.provider",            "value": {"stringValue": r.provider}}),
        ];

        if let Some(i) = r.input_tokens {
            attrs.push(json!({"key": "gen_ai.usage.input_tokens",  "value": {"intValue": i}}));
            attrs.push(json!({"key": "llm.token_count.prompt",     "value": {"intValue": i}}));
        }
        if let Some(o) = r.output_tokens {
            attrs.push(json!({"key": "gen_ai.usage.output_tokens", "value": {"intValue": o}}));
            attrs.push(json!({"key": "llm.token_count.completion", "value": {"intValue": o}}));
        }
        if let (Some(i), Some(o)) = (r.input_tokens, r.output_tokens) {
            attrs.push(json!({"key": "llm.token_count.total", "value": {"intValue": i + o}}));
        }
        if let Some(c) = r.cost_usd {
            attrs.push(json!({"key": "llm.cost_usd", "value": {"doubleValue": c}}));
        }
        if let Some(t) = r.ttft_ms {
            attrs.push(json!({"key": "llm.ttft_ms", "value": {"intValue": t as i64}}));
        }
        if let Some(ref err) = r.error {
            attrs.push(json!({"key": "error.type", "value": {"stringValue": err}}));
        }

        let is_error = r.status_code == 0 || r.status_code >= 400 || r.error.is_some();
        let status = if is_error {
            json!({
                "code": 2,   // STATUS_CODE_ERROR
                "message": r.error.as_deref().unwrap_or("upstream error")
            })
        } else {
            json!({"code": 1}) // STATUS_CODE_OK
        };

        let mut span = json!({
            "traceId":           trace_id,
            "spanId":            span_id,
            "name":              span_name,
            "kind":              3,   // SPAN_KIND_CLIENT
            "startTimeUnixNano": start_nanos.to_string(),
            "endTimeUnixNano":   end_nanos.to_string(),
            "attributes":        attrs,
            "status":            status,
        });

        if let Some(ref err) = r.error {
            span["events"] = json!([{
                "name": "exception",
                "attributes": [
                    {"key": "exception.message", "value": {"stringValue": err}}
                ]
            }]);
        }

        span
    }

    /// Export a single span to the OTLP HTTP endpoint.
    ///
    /// Fire-and-forget — errors are silently swallowed so the caller is never
    /// blocked or errored.
    pub async fn export_span(&self, r: &CallRecord) {
        let span = Self::build_span(r);

        let payload = json!({
            "resourceSpans": [{
                "resource": {
                    "attributes": [
                        {"key": "service.name",    "value": {"stringValue": "opentrace"}},
                        {"key": "service.version", "value": {"stringValue": env!("CARGO_PKG_VERSION")}}
                    ]
                },
                "scopeSpans": [{
                    "scope": {
                        "name":    "opentrace",
                        "version": env!("CARGO_PKG_VERSION")
                    },
                    "spans": [span]
                }]
            }]
        });

        let url = format!("{}/v1/traces", self.endpoint);
        // Use manual serialization — reqwest's `json` feature flag is not enabled.
        if let Ok(body) = serde_json::to_string(&payload) {
            let _ = self
                .client
                .post(&url)
                .header("content-type", "application/json")
                .body(body)
                .send()
                .await;
        }
        // Errors are intentionally ignored — OTel export is best-effort.
    }
}

// ---------------------------------------------------------------------------
// Internal helpers
// ---------------------------------------------------------------------------

/// Derive the gen_ai operation name from the endpoint path.
fn derive_operation(endpoint: &str) -> &'static str {
    if endpoint.contains("embedding") {
        "embeddings"
    } else {
        "chat"
    }
}

/// Map an internal provider slug to the OTel `gen_ai.system` well-known value.
fn map_provider(p: &str) -> &str {
    match p {
        "openai" => "openai",
        "anthropic" => "anthropic",
        "bedrock" => "aws.bedrock",
        "azure" => "azure.ai.openai",
        "google" => "gcp.vertex_ai",
        "cohere" => "cohere",
        "deepseek" => "deepseek",
        "groq" => "groq",
        "mistral" => "mistral_ai",
        "perplexity" => "perplexity",
        "xai" => "x_ai",
        "ollama" => "ollama",
        other => other,
    }
}

/// Convert an ISO 8601 timestamp string to nanoseconds since Unix epoch.
pub fn timestamp_to_nanos(ts: &str) -> u64 {
    chrono::DateTime::parse_from_rfc3339(ts)
        .ok()
        .and_then(|dt| dt.timestamp_nanos_opt())
        .map(|n| n as u64)
        .unwrap_or(0)
}

/// Standard Base64 encoder (RFC 4648, no line breaks).
pub fn base64_encode(bytes: &[u8]) -> String {
    const TABLE: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut result = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b0 = chunk[0] as usize;
        let b1 = if chunk.len() > 1 {
            chunk[1] as usize
        } else {
            0
        };
        let b2 = if chunk.len() > 2 {
            chunk[2] as usize
        } else {
            0
        };
        result.push(TABLE[b0 >> 2] as char);
        result.push(TABLE[((b0 & 0x3) << 4) | (b1 >> 4)] as char);
        if chunk.len() > 1 {
            result.push(TABLE[((b1 & 0xf) << 2) | (b2 >> 6)] as char);
        } else {
            result.push('=');
        }
        if chunk.len() > 2 {
            result.push(TABLE[b2 & 0x3f] as char);
        } else {
            result.push('=');
        }
    }
    result
}

/// Generate a random 128-bit (16-byte) trace ID as base64.
pub fn base64_16() -> String {
    base64_encode(Uuid::new_v4().as_bytes())
}

/// Generate a random 64-bit (8-byte) span ID as base64.
pub fn base64_8() -> String {
    base64_encode(&Uuid::new_v4().as_bytes()[..8])
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::now_iso;

    fn make_record(status: u16, latency_ms: u64, error: Option<&str>) -> CallRecord {
        CallRecord {
            id: Uuid::new_v4().to_string(),
            timestamp: now_iso(),
            provider: "openai".to_string(),
            model: "gpt-4o".to_string(),
            endpoint: "/v1/chat/completions".to_string(),
            status_code: status,
            latency_ms,
            ttft_ms: Some(latency_ms / 2),
            input_tokens: Some(100),
            output_tokens: Some(50),
            cost_usd: Some(0.001),
            request_body: None,
            response_body: None,
            error: error.map(String::from),
            provider_request_id: Some("req-abc".to_string()),
            trace_id: None,
            parent_id: None,
            prompt_hash: None,
            tags: None,
        }
    }

    #[test]
    fn otel_span_built_from_call_record() {
        let r = make_record(200, 350, None);
        let span = OtelExporter::build_span(&r);

        // Span name now uses gen_ai convention: "{operation} {model}"
        assert_eq!(span["name"].as_str().unwrap(), "chat gpt-4o");
        assert_eq!(span["kind"].as_u64().unwrap(), 3); // CLIENT

        // Attributes must include all expected keys.
        let attrs = span["attributes"].as_array().unwrap();
        let find =
            |key: &str| -> Option<&Value> { attrs.iter().find(|a| a["key"].as_str() == Some(key)) };

        // gen_ai.* semantic convention attributes
        assert_eq!(
            find("gen_ai.request.model").unwrap()["value"]["stringValue"]
                .as_str()
                .unwrap(),
            "gpt-4o"
        );
        assert_eq!(
            find("gen_ai.system").unwrap()["value"]["stringValue"]
                .as_str()
                .unwrap(),
            "openai"
        );
        assert_eq!(
            find("http.status_code").unwrap()["value"]["intValue"]
                .as_i64()
                .unwrap(),
            200
        );
        assert_eq!(
            find("gen_ai.usage.input_tokens").unwrap()["value"]["intValue"]
                .as_i64()
                .unwrap(),
            100
        );
        assert_eq!(
            find("gen_ai.usage.output_tokens").unwrap()["value"]["intValue"]
                .as_i64()
                .unwrap(),
            50
        );
        assert!(
            find("llm.cost_usd").is_some(),
            "cost_usd attr should be present"
        );
        assert!(
            find("llm.ttft_ms").is_some(),
            "ttft_ms attr should be present"
        );

        // OpenInference attributes
        assert_eq!(
            find("openinference.span.kind").unwrap()["value"]["stringValue"]
                .as_str()
                .unwrap(),
            "LLM"
        );
        assert_eq!(
            find("llm.model_name").unwrap()["value"]["stringValue"]
                .as_str()
                .unwrap(),
            "gpt-4o"
        );
        assert!(
            find("llm.token_count.total").is_some(),
            "token total should be present"
        );
    }

    #[test]
    fn otel_span_error_status() {
        let r = make_record(500, 1000, Some("internal server error"));
        let span = OtelExporter::build_span(&r);

        // status.code should be 2 (ERROR)
        assert_eq!(span["status"]["code"].as_u64().unwrap(), 2);
        assert!(span["status"]["message"].as_str().is_some());
        // exception event should be present
        assert!(
            span["events"].as_array().is_some(),
            "error should produce exception event"
        );
    }

    #[test]
    fn otel_span_ok_status() {
        let r = make_record(200, 200, None);
        let span = OtelExporter::build_span(&r);
        assert_eq!(span["status"]["code"].as_u64().unwrap(), 1); // OK
        assert!(
            span["events"].is_null()
                || span["events"]
                    .as_array()
                    .map(|a| a.is_empty())
                    .unwrap_or(true)
        );
    }

    #[test]
    fn otel_build_span_timestamps() {
        // Verify that endTimeUnixNano = startTimeUnixNano + latency_ms * 1_000_000.
        let r = make_record(200, 500, None); // 500 ms
        let span = OtelExporter::build_span(&r);

        let start: u64 = span["startTimeUnixNano"].as_str().unwrap().parse().unwrap();
        let end: u64 = span["endTimeUnixNano"].as_str().unwrap().parse().unwrap();

        assert!(start > 0, "startTimeUnixNano should be non-zero");
        assert_eq!(
            end - start,
            500 * 1_000_000,
            "end - start should equal latency_ms * 1e6"
        );
    }

    #[test]
    fn otel_span_connection_failure_status_code_zero() {
        let mut r = make_record(0, 100, Some("connection refused"));
        r.input_tokens = None;
        r.output_tokens = None;
        r.cost_usd = None;
        let span = OtelExporter::build_span(&r);
        assert_eq!(
            span["status"]["code"].as_u64().unwrap(),
            2,
            "status_code=0 must be ERROR"
        );
    }

    #[test]
    fn otel_span_embeddings_operation() {
        let mut r = make_record(200, 100, None);
        r.endpoint = "/v1/embeddings".to_string();
        let span = OtelExporter::build_span(&r);
        assert_eq!(span["name"].as_str().unwrap(), "embeddings gpt-4o");
        let attrs = span["attributes"].as_array().unwrap();
        let find =
            |key: &str| -> Option<&Value> { attrs.iter().find(|a| a["key"].as_str() == Some(key)) };
        assert_eq!(
            find("gen_ai.operation.name").unwrap()["value"]["stringValue"]
                .as_str()
                .unwrap(),
            "embeddings"
        );
    }

    #[test]
    fn otel_provider_mapping() {
        assert_eq!(map_provider("bedrock"), "aws.bedrock");
        assert_eq!(map_provider("azure"), "azure.ai.openai");
        assert_eq!(map_provider("google"), "gcp.vertex_ai");
        assert_eq!(map_provider("xai"), "x_ai");
        assert_eq!(map_provider("mistral"), "mistral_ai");
        assert_eq!(map_provider("unknown_provider"), "unknown_provider");
    }

    #[test]
    fn base64_encode_known_values() {
        // RFC 4648 test vectors
        assert_eq!(base64_encode(b""), "");
        assert_eq!(base64_encode(b"f"), "Zg==");
        assert_eq!(base64_encode(b"fo"), "Zm8=");
        assert_eq!(base64_encode(b"foo"), "Zm9v");
        assert_eq!(base64_encode(b"foob"), "Zm9vYg==");
        assert_eq!(base64_encode(b"fooba"), "Zm9vYmE=");
        assert_eq!(base64_encode(b"foobar"), "Zm9vYmFy");
    }

    #[test]
    fn base64_16_produces_24_char_string() {
        // 16 bytes → ceil(16/3)*4 = 24 chars (with padding)
        let s = base64_16();
        assert_eq!(s.len(), 24, "16-byte base64 should be 24 chars");
    }

    #[test]
    fn base64_8_produces_12_char_string() {
        // 8 bytes → ceil(8/3)*4 = 12 chars
        let s = base64_8();
        assert_eq!(s.len(), 12, "8-byte base64 should be 12 chars");
    }
}
