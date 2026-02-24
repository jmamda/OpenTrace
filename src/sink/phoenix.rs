//! Arize Phoenix sink — forwards each captured LLM call to a Phoenix
//! instance via OTLP HTTP with OpenInference semantic conventions.
//!
//! Fire-and-forget: all HTTP errors are silently swallowed so the main
//! request path is never blocked.

use serde_json::json;

/// Arize Phoenix OTLP sink.
///
/// Clone via `Arc<PhoenixSink>` to share across tasks.
#[derive(Clone)]
pub struct PhoenixSink {
    /// Full endpoint URL: `{phoenix_url}/v1/traces`
    endpoint: String,
    /// `Some("Bearer {key}")` for cloud; `None` for local unauthenticated.
    auth_header: Option<String>,
    client: reqwest::Client,
}

impl PhoenixSink {
    /// Create a new sink.
    ///
    /// `phoenix_url` defaults to `http://localhost:6006` for local Phoenix.
    /// `api_key` is optional — required for Arize Phoenix cloud.
    pub fn new(phoenix_url: &str, api_key: Option<&str>) -> Self {
        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(5))
            .build()
            .unwrap_or_default();
        PhoenixSink {
            endpoint: format!("{}/v1/traces", phoenix_url.trim_end_matches('/')),
            auth_header: api_key.map(|k| format!("Bearer {}", k)),
            client,
        }
    }

    /// Forward one `CallRecord` to Phoenix as an OTLP span. Fire-and-forget.
    pub async fn send(&self, record: &crate::store::CallRecord) {
        let start_nanos = timestamp_to_nanos(&record.timestamp);
        let end_nanos = start_nanos.saturating_add(record.latency_ms.saturating_mul(1_000_000));

        let mut attrs: Vec<serde_json::Value> = vec![
            json!({"key": "openinference.span.kind", "value": {"stringValue": "LLM"}}),
            json!({"key": "llm.model_name",   "value": {"stringValue": record.model}}),
            json!({"key": "llm.provider",     "value": {"stringValue": record.provider}}),
            json!({"key": "http.status_code", "value": {"intValue": record.status_code as i64}}),
        ];

        if let Some(i) = record.input_tokens {
            attrs.push(json!({"key": "llm.token_count.prompt",     "value": {"intValue": i}}));
        }
        if let Some(o) = record.output_tokens {
            attrs.push(json!({"key": "llm.token_count.completion", "value": {"intValue": o}}));
        }
        if let (Some(i), Some(o)) = (record.input_tokens, record.output_tokens) {
            attrs.push(json!({"key": "llm.token_count.total", "value": {"intValue": i + o}}));
        }

        if let Some(ref body) = record.request_body {
            let t = truncate_to_10k(body);
            attrs.push(json!({"key": "input.value",     "value": {"stringValue": t}}));
            attrs.push(json!({"key": "input.mime_type", "value": {"stringValue": "application/json"}}));
        }
        if let Some(ref body) = record.response_body {
            let t = truncate_to_10k(body);
            attrs.push(json!({"key": "output.value",     "value": {"stringValue": t}}));
            attrs.push(json!({"key": "output.mime_type", "value": {"stringValue": "application/json"}}));
        }

        let is_error =
            record.status_code == 0 || record.status_code >= 400 || record.error.is_some();
        let status_obj = if is_error {
            json!({
                "code": 2,
                "message": record.error.as_deref().unwrap_or("upstream error")
            })
        } else {
            json!({"code": 1})
        };

        let payload = json!({
            "resourceSpans": [{
                "resource": {
                    "attributes": [
                        {"key": "service.name",    "value": {"stringValue": "opentrace"}},
                        {"key": "service.version", "value": {"stringValue": env!("CARGO_PKG_VERSION")}}
                    ]
                },
                "scopeSpans": [{
                    "scope": {"name": "openinference", "version": "1.0.0"},
                    "spans": [{
                        "traceId":           base64_16(),
                        "spanId":            base64_8(),
                        "name":              format!("llm {}", record.model),
                        "kind":              3,
                        "startTimeUnixNano": start_nanos.to_string(),
                        "endTimeUnixNano":   end_nanos.to_string(),
                        "attributes":        attrs,
                        "status":            status_obj,
                    }]
                }]
            }]
        });

        let mut req = self
            .client
            .post(&self.endpoint)
            .header("content-type", "application/json");
        if let Some(ref auth) = self.auth_header {
            req = req.header("authorization", auth);
        }
        let body = serde_json::to_string(&payload).unwrap_or_default();
        let _ = req.body(body).send().await;
    }
}

// ---------------------------------------------------------------------------
// Helpers (copied inline — no import from otel.rs to keep modules independent)
// ---------------------------------------------------------------------------

/// Truncate a string to at most 10 240 bytes at a valid char boundary.
fn truncate_to_10k(s: &str) -> &str {
    const LIMIT: usize = 10_240;
    if s.len() <= LIMIT {
        return s;
    }
    let mut end = LIMIT;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

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

fn base64_16() -> String {
    base64_encode(uuid::Uuid::new_v4().as_bytes())
}

fn base64_8() -> String {
    base64_encode(&uuid::Uuid::new_v4().as_bytes()[..8])
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::{now_iso, CallRecord};
    use uuid::Uuid;

    fn make_record(status: u16, error: Option<&str>) -> CallRecord {
        CallRecord {
            id: Uuid::new_v4().to_string(),
            timestamp: now_iso(),
            provider: "openai".to_string(),
            model: "gpt-4o".to_string(),
            endpoint: "/v1/chat/completions".to_string(),
            status_code: status,
            latency_ms: 250,
            ttft_ms: None,
            input_tokens: Some(70),
            output_tokens: Some(35),
            cost_usd: Some(0.001),
            request_body: Some(r#"{"messages":[]}"#.to_string()),
            response_body: None,
            error: error.map(String::from),
            provider_request_id: None,
            trace_id: None,
            parent_id: None,
            prompt_hash: None,
        }
    }

    #[test]
    fn phoenix_endpoint_formed_correctly() {
        let s = PhoenixSink::new("http://localhost:6006", None);
        assert_eq!(s.endpoint, "http://localhost:6006/v1/traces");
        assert!(s.auth_header.is_none());
    }

    #[test]
    fn phoenix_cloud_bearer_auth() {
        let s = PhoenixSink::new("https://app.phoenix.arize.com", Some("mykey"));
        assert_eq!(s.auth_header.as_deref(), Some("Bearer mykey"));
    }

    #[test]
    fn timestamp_to_nanos_is_positive() {
        let ts = "2026-02-23T10:00:00Z";
        assert!(timestamp_to_nanos(ts) > 0);
    }

    #[test]
    fn end_nanos_after_start() {
        let ts = "2026-02-23T10:00:00Z";
        let start = timestamp_to_nanos(ts);
        let end = start.saturating_add(250u64.saturating_mul(1_000_000));
        assert_eq!(end - start, 250 * 1_000_000);
    }

    #[test]
    fn error_status_code_zero_is_error() {
        let r = make_record(0, Some("connection refused"));
        let is_error =
            r.status_code == 0 || r.status_code >= 400 || r.error.is_some();
        assert!(is_error);
    }

    #[test]
    fn base64_16_and_8_lengths() {
        assert_eq!(base64_16().len(), 24);
        assert_eq!(base64_8().len(), 12);
    }
}
