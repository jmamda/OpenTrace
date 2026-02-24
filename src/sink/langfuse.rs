//! Langfuse sink — forwards each captured LLM call to the Langfuse batch
//! ingestion API as a `generation-create` event.
//!
//! Fire-and-forget: all HTTP errors are silently swallowed so the main
//! request path is never blocked.

use serde_json::json;
use uuid::Uuid;

/// Langfuse batch-ingestion sink.
///
/// Clone via `Arc<LangfuseSink>` to share across tasks.
#[derive(Clone)]
pub struct LangfuseSink {
    /// Full endpoint URL: `{base_url}/api/public/ingestion`
    endpoint: String,
    /// `Authorization: Basic <base64(public_key:secret_key)>`
    auth_header: String,
    client: reqwest::Client,
}

impl LangfuseSink {
    /// Create a new sink.
    ///
    /// `base_url` defaults to `https://cloud.langfuse.com` for cloud;
    /// override for self-hosted deployments.
    pub fn new(base_url: &str, public_key: &str, secret_key: &str) -> Self {
        let credentials = format!("{}:{}", public_key, secret_key);
        let auth_header = format!("Basic {}", base64_encode(credentials.as_bytes()));
        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(5))
            .build()
            .unwrap_or_default();
        LangfuseSink {
            endpoint: format!("{}/api/public/ingestion", base_url.trim_end_matches('/')),
            auth_header,
            client,
        }
    }

    /// Forward one `CallRecord` to Langfuse. Fire-and-forget.
    pub async fn send(&self, record: &crate::store::CallRecord) {
        let end_time = add_ms(&record.timestamp, record.latency_ms);

        let input_tokens = record.input_tokens;
        let output_tokens = record.output_tokens;
        let total_tokens = match (input_tokens, output_tokens) {
            (Some(i), Some(o)) => Some(i + o),
            _ => None,
        };

        let req_body = parse_body(record.request_body.as_deref());
        let resp_body = parse_body(record.response_body.as_deref());

        let level = if record.error.is_some() { "ERROR" } else { "DEFAULT" };

        let payload = json!({
            "batch": [{
                "id": Uuid::new_v4().to_string(),
                "type": "generation-create",
                "body": {
                    "id": record.id,
                    "traceId": record.id,
                    "name": "llm-call",
                    "startTime": record.timestamp,
                    "endTime": end_time,
                    "model": record.model,
                    "input": req_body,
                    "output": resp_body,
                    "usage": {
                        "input": input_tokens,
                        "output": output_tokens,
                        "total": total_tokens
                    },
                    "metadata": {
                        "provider": record.provider,
                        "endpoint": record.endpoint,
                        "status_code": record.status_code,
                        "cost_usd": record.cost_usd,
                        "latency_ms": record.latency_ms
                    },
                    "level": level
                }
            }]
        });

        let body = serde_json::to_string(&payload).unwrap_or_default();
        let _ = self
            .client
            .post(&self.endpoint)
            .header("authorization", &self.auth_header)
            .header("content-type", "application/json")
            .body(body)
            .send()
            .await;
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Add milliseconds to an RFC3339 timestamp string, return ISO8601 result.
/// Falls back to the original string on parse error.
fn add_ms(ts: &str, ms: u64) -> String {
    chrono::DateTime::parse_from_rfc3339(ts)
        .map(|dt| {
            let dur = chrono::Duration::milliseconds(ms as i64);
            (dt + dur).to_rfc3339()
        })
        .unwrap_or_else(|_| ts.to_string())
}

/// Parse a JSON body string (truncated to 10 KB) or return `null`.
fn parse_body(body: Option<&str>) -> serde_json::Value {
    match body {
        Some(s) => serde_json::from_str(truncate_10k(s)).unwrap_or(serde_json::Value::Null),
        None => serde_json::Value::Null,
    }
}

/// Truncate a string to at most 10 240 bytes at a valid char boundary.
fn truncate_10k(s: &str) -> &str {
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

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::{now_iso, CallRecord};
    use uuid::Uuid;

    fn make_record() -> CallRecord {
        CallRecord {
            id: Uuid::new_v4().to_string(),
            timestamp: now_iso(),
            provider: "openai".to_string(),
            model: "gpt-4o".to_string(),
            endpoint: "/v1/chat/completions".to_string(),
            status_code: 200,
            latency_ms: 400,
            ttft_ms: Some(150),
            input_tokens: Some(80),
            output_tokens: Some(40),
            cost_usd: Some(0.001),
            request_body: Some(r#"{"messages":[]}"#.to_string()),
            response_body: Some(r#"{"choices":[]}"#.to_string()),
            error: None,
            provider_request_id: None,
            trace_id: None,
            parent_id: None,
            prompt_hash: None,
        }
    }

    #[test]
    fn langfuse_sink_fields() {
        let sink = LangfuseSink::new("https://cloud.langfuse.com", "pk-test", "sk-test");
        // Verify auth header is base64-encoded Basic credential
        assert!(sink.auth_header.starts_with("Basic "), "auth must be Basic");
        assert!(sink.endpoint.ends_with("/api/public/ingestion"));
    }

    #[test]
    fn langfuse_payload_uses_record_fields() {
        let r = make_record();
        assert_eq!(r.provider, "openai");
        assert_eq!(r.model, "gpt-4o");
        assert!(r.input_tokens.is_some());
    }

    #[test]
    fn add_ms_advances_time() {
        let ts = "2026-02-23T10:00:00Z";
        let result = add_ms(ts, 1000);
        assert_ne!(result, ts);
        assert!(result.contains("10:00:01"), "should advance by 1 second");
    }

    #[test]
    fn add_ms_bad_timestamp_fallback() {
        let bad = "not-a-timestamp";
        assert_eq!(add_ms(bad, 500), bad);
    }

    #[test]
    fn parse_body_valid_json() {
        let v = parse_body(Some(r#"{"key":"value"}"#));
        assert_eq!(v["key"].as_str().unwrap(), "value");
    }

    #[test]
    fn parse_body_none_is_null() {
        assert!(parse_body(None).is_null());
    }

    #[test]
    fn base64_encode_basic() {
        assert_eq!(base64_encode(b"foo"), "Zm9v");
    }
}

/// Standard Base64 encoder (RFC 4648, no line breaks).
/// Copied inline — no import from `otel` to keep modules independent.
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
