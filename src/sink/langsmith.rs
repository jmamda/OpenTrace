//! LangSmith sink — forwards each captured LLM call to the LangSmith runs API.
//!
//! Fire-and-forget: all HTTP errors are silently swallowed so the main
//! request path is never blocked.

use serde_json::json;

/// LangSmith runs-API sink.
///
/// Clone via `Arc<LangSmithSink>` to share across tasks.
#[derive(Clone)]
pub struct LangSmithSink {
    /// Full endpoint URL: `{base_url}/runs`
    endpoint: String,
    api_key: String,
    client: reqwest::Client,
}

impl LangSmithSink {
    /// Create a new sink.
    ///
    /// `base_url` defaults to `https://api.smith.langchain.com`;
    /// override via `--langsmith-endpoint` for self-hosted.
    pub fn new(base_url: &str, api_key: &str) -> Self {
        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(5))
            .build()
            .unwrap_or_default();
        LangSmithSink {
            endpoint: format!("{}/runs", base_url.trim_end_matches('/')),
            api_key: api_key.to_string(),
            client,
        }
    }

    /// Forward one `CallRecord` to LangSmith. Fire-and-forget.
    pub async fn send(&self, record: &crate::store::CallRecord) {
        let start_ms = to_ms(&record.timestamp);
        let end_ms = start_ms + record.latency_ms as i64;

        let inputs = parse_body(record.request_body.as_deref());
        let outputs = parse_body(record.response_body.as_deref());

        let payload = json!({
            "id": record.id,
            "name": "llm-call",
            "run_type": "llm",
            "inputs": {"messages": inputs},
            "outputs": {"output": outputs},
            "start_time": start_ms,
            "end_time": end_ms,
            "extra": {
                "model": record.model,
                "provider": record.provider,
                "input_tokens": record.input_tokens,
                "output_tokens": record.output_tokens,
                "cost_usd": record.cost_usd,
                "status_code": record.status_code
            },
            "error": record.error,
            "session_name": "opentrace",
            "tags": ["opentrace", record.provider]
        });

        let body = serde_json::to_string(&payload).unwrap_or_default();
        let _ = self
            .client
            .post(&self.endpoint)
            .header("x-api-key", &self.api_key)
            .header("content-type", "application/json")
            .body(body)
            .send()
            .await;
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Parse RFC3339 timestamp to milliseconds since Unix epoch. Returns 0 on error.
fn to_ms(ts: &str) -> i64 {
    chrono::DateTime::parse_from_rfc3339(ts)
        .map(|dt| dt.timestamp_millis())
        .unwrap_or(0)
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
            provider: "anthropic".to_string(),
            model: "claude-3-7-sonnet".to_string(),
            endpoint: "/v1/messages".to_string(),
            status_code: 200,
            latency_ms: 600,
            ttft_ms: Some(200),
            input_tokens: Some(120),
            output_tokens: Some(60),
            cost_usd: Some(0.002),
            request_body: None,
            response_body: None,
            error: None,
            provider_request_id: None,
            trace_id: None,
            parent_id: None,
            prompt_hash: None,
        }
    }

    #[test]
    fn langsmith_endpoint_formed_correctly() {
        let s = LangSmithSink::new("https://api.smith.langchain.com", "ls-test");
        assert_eq!(s.endpoint, "https://api.smith.langchain.com/runs");
    }

    #[test]
    fn to_ms_is_positive_for_valid_timestamp() {
        let ts = "2026-02-23T10:00:00Z";
        assert!(to_ms(ts) > 0);
    }

    #[test]
    fn to_ms_fallback_zero_on_bad_input() {
        assert_eq!(to_ms("bad"), 0);
    }

    #[test]
    fn langsmith_end_time_after_start() {
        let r = make_record();
        let sm = to_ms(&r.timestamp);
        let em = sm + r.latency_ms as i64;
        assert!(em >= sm);
        assert_eq!(em - sm, 600);
    }
}
