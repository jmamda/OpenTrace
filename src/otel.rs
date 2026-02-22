//! OpenTelemetry OTLP span exporter — manual HTTP/JSON implementation.
//!
//! Exports each captured LLM call as a single OpenTelemetry span to an OTLP
//! HTTP endpoint (e.g. Jaeger, Grafana Tempo, Honeycomb, Datadog).
//!
//! Uses OTLP HTTP with JSON encoding — compatible with all major collectors.
//! No external OTel crates are required; only `reqwest` and `serde_json`
//! (already present in Cargo.toml) are used.
//!
//! ## Span mapping
//! | OTel field              | Source                                    |
//! |------------------------|-------------------------------------------|
//! | `name`                 | `"llm.{provider}"`                        |
//! | `startTimeUnixNano`    | `CallRecord.timestamp` → nanoseconds      |
//! | `endTimeUnixNano`      | start + `latency_ms` × 10⁶               |
//! | `llm.model`            | `CallRecord.model`                        |
//! | `llm.provider`         | `CallRecord.provider`                     |
//! | `llm.endpoint`         | `CallRecord.endpoint`                     |
//! | `http.status_code`     | `CallRecord.status_code`                  |
//! | `llm.input_tokens`     | `CallRecord.input_tokens` (when present)  |
//! | `llm.output_tokens`    | `CallRecord.output_tokens` (when present) |
//! | `llm.cost_usd`         | `CallRecord.cost_usd` (when present)      |
//! | `llm.ttft_ms`          | `CallRecord.ttft_ms` (when present)       |
//! | `otel.status_code`     | `"ERROR"` on failures, `"OK"` otherwise   |
//! | `exception.message`    | `CallRecord.error` (when present)         |

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
    pub fn build_span(r: &CallRecord) -> Value {
        let start_nanos = timestamp_to_nanos(&r.timestamp);
        let end_nanos = start_nanos.saturating_add(r.latency_ms.saturating_mul(1_000_000));

        // 128-bit trace ID (16 bytes) + 64-bit span ID (8 bytes), both base64-encoded.
        let trace_id = base64_16();
        let span_id = base64_8();

        let mut attrs: Vec<Value> = vec![
            json!({"key": "llm.model",       "value": {"stringValue": r.model}}),
            json!({"key": "llm.provider",    "value": {"stringValue": r.provider}}),
            json!({"key": "llm.endpoint",    "value": {"stringValue": r.endpoint}}),
            json!({"key": "http.status_code","value": {"intValue": r.status_code as i64}}),
        ];
        if let Some(i) = r.input_tokens {
            attrs.push(json!({"key": "llm.input_tokens",  "value": {"intValue": i}}));
        }
        if let Some(o) = r.output_tokens {
            attrs.push(json!({"key": "llm.output_tokens", "value": {"intValue": o}}));
        }
        if let Some(c) = r.cost_usd {
            attrs.push(json!({"key": "llm.cost_usd",      "value": {"doubleValue": c}}));
        }
        if let Some(t) = r.ttft_ms {
            attrs.push(json!({"key": "llm.ttft_ms",       "value": {"intValue": t as i64}}));
        }

        let is_error =
            r.status_code == 0 || r.status_code >= 400 || r.error.is_some();
        let status = if is_error {
            json!({
                "code": 2,   // STATUS_CODE_ERROR
                "message": r.error.as_deref().unwrap_or("upstream error")
            })
        } else {
            json!({"code": 1})  // STATUS_CODE_OK
        };

        let mut span = json!({
            "traceId":           trace_id,
            "spanId":            span_id,
            "name":              format!("llm.{}", r.provider),
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

/// Convert an ISO 8601 timestamp string to nanoseconds since Unix epoch.
fn timestamp_to_nanos(ts: &str) -> u64 {
    chrono::DateTime::parse_from_rfc3339(ts)
        .ok()
        .and_then(|dt| dt.timestamp_nanos_opt())
        .map(|n| n as u64)
        .unwrap_or(0)
}

/// Standard Base64 encoder (RFC 4648, no line breaks).
fn base64_encode(bytes: &[u8]) -> String {
    const TABLE: &[u8] =
        b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut result = String::with_capacity((bytes.len() + 2) / 3 * 4);
    for chunk in bytes.chunks(3) {
        let b0 = chunk[0] as usize;
        let b1 = if chunk.len() > 1 { chunk[1] as usize } else { 0 };
        let b2 = if chunk.len() > 2 { chunk[2] as usize } else { 0 };
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
fn base64_16() -> String {
    base64_encode(Uuid::new_v4().as_bytes())
}

/// Generate a random 64-bit (8-byte) span ID as base64.
fn base64_8() -> String {
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
        }
    }

    #[test]
    fn otel_span_built_from_call_record() {
        let r = make_record(200, 350, None);
        let span = OtelExporter::build_span(&r);

        assert_eq!(span["name"].as_str().unwrap(), "llm.openai");
        assert_eq!(span["kind"].as_u64().unwrap(), 3); // CLIENT

        // Attributes must include all expected keys.
        let attrs = span["attributes"].as_array().unwrap();
        let find = |key: &str| -> Option<&Value> {
            attrs.iter().find(|a| a["key"].as_str() == Some(key))
        };

        assert_eq!(
            find("llm.model").unwrap()["value"]["stringValue"].as_str().unwrap(),
            "gpt-4o"
        );
        assert_eq!(
            find("llm.provider").unwrap()["value"]["stringValue"].as_str().unwrap(),
            "openai"
        );
        assert_eq!(
            find("http.status_code").unwrap()["value"]["intValue"].as_i64().unwrap(),
            200
        );
        assert_eq!(
            find("llm.input_tokens").unwrap()["value"]["intValue"].as_i64().unwrap(),
            100
        );
        assert_eq!(
            find("llm.output_tokens").unwrap()["value"]["intValue"].as_i64().unwrap(),
            50
        );
        assert!(find("llm.cost_usd").is_some(), "cost_usd attr should be present");
        assert!(find("llm.ttft_ms").is_some(), "ttft_ms attr should be present");
    }

    #[test]
    fn otel_span_error_status() {
        let r = make_record(500, 1000, Some("internal server error"));
        let span = OtelExporter::build_span(&r);

        // status.code should be 2 (ERROR)
        assert_eq!(span["status"]["code"].as_u64().unwrap(), 2);
        assert!(span["status"]["message"].as_str().is_some());
        // exception event should be present
        assert!(span["events"].as_array().is_some(), "error should produce exception event");
    }

    #[test]
    fn otel_span_ok_status() {
        let r = make_record(200, 200, None);
        let span = OtelExporter::build_span(&r);
        assert_eq!(span["status"]["code"].as_u64().unwrap(), 1); // OK
        assert!(span["events"].is_null() || span["events"].as_array().map(|a| a.is_empty()).unwrap_or(true));
    }

    #[test]
    fn otel_build_span_timestamps() {
        // Verify that endTimeUnixNano = startTimeUnixNano + latency_ms * 1_000_000.
        let r = make_record(200, 500, None); // 500 ms
        let span = OtelExporter::build_span(&r);

        let start: u64 = span["startTimeUnixNano"]
            .as_str()
            .unwrap()
            .parse()
            .unwrap();
        let end: u64 = span["endTimeUnixNano"]
            .as_str()
            .unwrap()
            .parse()
            .unwrap();

        assert!(start > 0, "startTimeUnixNano should be non-zero");
        assert_eq!(end - start, 500 * 1_000_000, "end - start should equal latency_ms * 1e6");
    }

    #[test]
    fn otel_span_connection_failure_status_code_zero() {
        let mut r = make_record(0, 100, Some("connection refused"));
        r.input_tokens = None;
        r.output_tokens = None;
        r.cost_usd = None;
        let span = OtelExporter::build_span(&r);
        assert_eq!(span["status"]["code"].as_u64().unwrap(), 2, "status_code=0 must be ERROR");
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
