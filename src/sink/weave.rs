//! W&B Weave sink — forwards each captured LLM call to the Weave
//! batch upsert API.
//!
//! Fire-and-forget: all HTTP errors are silently swallowed so the main
//! request path is never blocked.

use serde_json::json;

/// W&B Weave batch-upsert sink.
///
/// Clone via `Arc<WeaveSink>` to share across tasks.
#[derive(Clone)]
pub struct WeaveSink {
    /// Full endpoint URL: `{base_url}/call/upsert-batch`
    endpoint: String,
    /// `Authorization: Bearer {api_key}`
    auth_header: String,
    /// W&B project in `entity/project` format.
    project_id: String,
    client: reqwest::Client,
}

impl WeaveSink {
    /// Create a new sink.
    ///
    /// `base_url` is typically `https://trace.wandb.ai`.
    /// `project_id` must be `"entity/project"`.
    pub fn new(base_url: &str, api_key: &str, project_id: &str) -> Self {
        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(5))
            .build()
            .unwrap_or_default();
        WeaveSink {
            endpoint: format!("{}/call/upsert-batch", base_url.trim_end_matches('/')),
            auth_header: format!("Bearer {}", api_key),
            project_id: project_id.to_string(),
            client,
        }
    }

    /// Forward one `CallRecord` to W&B Weave. Fire-and-forget.
    pub async fn send(&self, record: &crate::store::CallRecord) {
        let ended_at = add_ms(&record.timestamp, record.latency_ms);
        let inputs_messages = parse_body(record.request_body.as_deref());
        let output = parse_body(record.response_body.as_deref());

        let input_tokens = record.input_tokens.unwrap_or(0);
        let output_tokens = record.output_tokens.unwrap_or(0);
        let total_tokens = input_tokens + output_tokens;

        // serde_json::json! requires a string literal or pre-computed variable as
        // an object key — extract model string first.
        let model_key = record.model.as_str();

        let payload = json!({
            "calls": [{
                "id": record.id,
                "project_id": self.project_id,
                "op_name": "llm_call",
                "started_at": record.timestamp,
                "ended_at": ended_at,
                "inputs": {
                    "model": record.model,
                    "messages": inputs_messages
                },
                "output": output,
                "summary": {
                    "usage": {
                        model_key: {
                            "prompt_tokens": input_tokens,
                            "completion_tokens": output_tokens,
                            "total_tokens": total_tokens,
                            "requests": 1
                        }
                    }
                },
                "attributes": {
                    "provider": record.provider,
                    "endpoint": record.endpoint,
                    "status_code": record.status_code as i64,
                    "cost_usd": record.cost_usd,
                    "latency_ms": record.latency_ms
                }
            }]
        });

        let body = serde_json::to_string(&payload).unwrap_or_default();
        let _ = self
            .client
            .post(&self.endpoint)
            .header("content-type", "application/json")
            .header("authorization", &self.auth_header)
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
            latency_ms: 300,
            ttft_ms: None,
            input_tokens: Some(50),
            output_tokens: Some(30),
            cost_usd: None,
            request_body: None,
            response_body: None,
            error: None,
            provider_request_id: None,
            trace_id: None,
            parent_id: None,
            prompt_hash: None,
            tags: None,
        }
    }

    #[test]
    fn weave_endpoint_formed_correctly() {
        let s = WeaveSink::new("https://trace.wandb.ai", "key", "entity/proj");
        assert_eq!(s.endpoint, "https://trace.wandb.ai/call/upsert-batch");
        assert_eq!(s.auth_header, "Bearer key");
        assert_eq!(s.project_id, "entity/proj");
    }

    #[test]
    fn add_ms_advances_by_correct_amount() {
        let ts = "2026-02-23T12:00:00Z";
        let result = add_ms(ts, 2000);
        assert!(result.contains("12:00:02"), "should advance by 2 seconds");
    }

    #[test]
    fn token_sums_are_correct() {
        let r = make_record();
        let pt = r.input_tokens.unwrap_or(0);
        let ct = r.output_tokens.unwrap_or(0);
        assert_eq!(pt + ct, 80);
    }
}
