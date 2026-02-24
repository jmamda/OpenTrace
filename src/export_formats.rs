//! Serialization helpers for batch export formats.
//!
//! Used by `trace export --format <name>` to emit call records in the native
//! ingestion format of observability platforms (Langfuse, LangSmith, W&B Weave).
//!
//! Each function returns a single-line JSON string with no trailing newline.
//! Never panics — timestamp parse failures fall back to the raw string.

use serde_json::json;
use uuid::Uuid;

use crate::store::CallRecord;

/// Serialize a `CallRecord` as a Langfuse batch-ingestion JSONL line.
///
/// Each line is a complete `{"batch": [...]}` payload containing one
/// `generation-create` event, ready to POST to `/api/public/ingestion`.
pub fn to_langfuse_line(r: &CallRecord) -> String {
    let end_time = add_ms(&r.timestamp, r.latency_ms);
    let input_tokens = r.input_tokens;
    let output_tokens = r.output_tokens;
    let total_tokens = match (input_tokens, output_tokens) {
        (Some(i), Some(o)) => Some(i + o),
        _ => None,
    };
    let req_body = parse_body(r.request_body.as_deref());
    let resp_body = parse_body(r.response_body.as_deref());
    let level = if r.error.is_some() {
        "ERROR"
    } else {
        "DEFAULT"
    };

    let v = json!({
        "batch": [{
            "id": Uuid::new_v4().to_string(),
            "type": "generation-create",
            "body": {
                "id": r.id,
                "traceId": r.id,
                "name": "llm-call",
                "startTime": r.timestamp,
                "endTime": end_time,
                "model": r.model,
                "input": req_body,
                "output": resp_body,
                "usage": {
                    "input": input_tokens,
                    "output": output_tokens,
                    "total": total_tokens
                },
                "metadata": {
                    "provider": r.provider,
                    "endpoint": r.endpoint,
                    "status_code": r.status_code,
                    "cost_usd": r.cost_usd,
                    "latency_ms": r.latency_ms
                },
                "level": level
            }
        }]
    });
    serde_json::to_string(&v).unwrap_or_default()
}

/// Serialize a `CallRecord` as a LangSmith run JSONL line.
///
/// Each line is a complete run object for `POST /runs`, with timestamps as
/// milliseconds since Unix epoch.
pub fn to_langsmith_line(r: &CallRecord) -> String {
    let start_ms = to_ms(&r.timestamp);
    let end_ms = start_ms + r.latency_ms as i64;
    let inputs = parse_body(r.request_body.as_deref());
    let outputs = parse_body(r.response_body.as_deref());

    let v = json!({
        "id": r.id,
        "name": "llm-call",
        "run_type": "llm",
        "inputs": {"messages": inputs},
        "outputs": {"output": outputs},
        "start_time": start_ms,
        "end_time": end_ms,
        "extra": {
            "model": r.model,
            "provider": r.provider,
            "input_tokens": r.input_tokens,
            "output_tokens": r.output_tokens,
            "cost_usd": r.cost_usd,
            "status_code": r.status_code
        },
        "error": r.error,
        "session_name": "opentrace",
        "tags": ["opentrace", r.provider]
    });
    serde_json::to_string(&v).unwrap_or_default()
}

/// Serialize a `CallRecord` as a W&B Weave upsert-batch JSONL line.
///
/// Each line is `{"calls": [...]}` for one record, ready to POST to
/// `/call/upsert-batch`.
pub fn to_weave_line(r: &CallRecord) -> String {
    let end_time = add_ms(&r.timestamp, r.latency_ms);
    let inputs = parse_body(r.request_body.as_deref());
    let output = parse_body(r.response_body.as_deref());
    let prompt_tokens = r.input_tokens.unwrap_or(0);
    let completion_tokens = r.output_tokens.unwrap_or(0);
    let total_tokens = prompt_tokens + completion_tokens;

    let v = json!({
        "calls": [{
            "id": r.id,
            "op_name": "llm_call",
            "started_at": r.timestamp,
            "ended_at": end_time,
            "inputs": {
                "model": r.model,
                "messages": inputs
            },
            "output": output,
            "summary": {
                "usage": {
                    r.model.as_str(): {
                        "prompt_tokens": prompt_tokens,
                        "completion_tokens": completion_tokens,
                        "total_tokens": total_tokens,
                        "requests": 1
                    }
                }
            },
            "attributes": {
                "provider": r.provider,
                "endpoint": r.endpoint,
                "status_code": r.status_code,
                "cost_usd": r.cost_usd,
                "latency_ms": r.latency_ms
            }
        }]
    });
    serde_json::to_string(&v).unwrap_or_default()
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

/// Parse RFC3339 timestamp to milliseconds since Unix epoch. Returns 0 on error.
fn to_ms(ts: &str) -> i64 {
    chrono::DateTime::parse_from_rfc3339(ts)
        .map(|dt| dt.timestamp_millis())
        .unwrap_or(0)
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

/// Parse a JSON body string (truncated to 10 KB) or return `null`.
fn parse_body(body: Option<&str>) -> serde_json::Value {
    match body {
        Some(s) => serde_json::from_str(truncate_10k(s)).unwrap_or(serde_json::Value::Null),
        None => serde_json::Value::Null,
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::now_iso;
    use uuid::Uuid;

    fn make_record() -> CallRecord {
        CallRecord {
            id: Uuid::new_v4().to_string(),
            timestamp: now_iso(),
            provider: "openai".to_string(),
            model: "gpt-4o".to_string(),
            endpoint: "/v1/chat/completions".to_string(),
            status_code: 200,
            latency_ms: 500,
            ttft_ms: Some(200),
            input_tokens: Some(100),
            output_tokens: Some(50),
            cost_usd: Some(0.002),
            request_body: Some(r#"{"messages":[{"role":"user","content":"hi"}]}"#.to_string()),
            response_body: Some(
                r#"{"choices":[{"message":{"role":"assistant","content":"hello"}}]}"#.to_string(),
            ),
            error: None,
            provider_request_id: None,
            trace_id: None,
            parent_id: None,
            prompt_hash: None,
        }
    }

    #[test]
    fn langfuse_line_is_valid_json() {
        let r = make_record();
        let line = to_langfuse_line(&r);
        let v: serde_json::Value = serde_json::from_str(&line).expect("must be valid JSON");
        assert_eq!(v["batch"][0]["type"].as_str().unwrap(), "generation-create");
        assert_eq!(v["batch"][0]["body"]["model"].as_str().unwrap(), "gpt-4o");
        assert_eq!(v["batch"][0]["body"]["level"].as_str().unwrap(), "DEFAULT");
    }

    #[test]
    fn langfuse_line_error_level() {
        let mut r = make_record();
        r.error = Some("rate limit".to_string());
        let line = to_langfuse_line(&r);
        let v: serde_json::Value = serde_json::from_str(&line).unwrap();
        assert_eq!(v["batch"][0]["body"]["level"].as_str().unwrap(), "ERROR");
    }

    #[test]
    fn langsmith_line_is_valid_json() {
        let r = make_record();
        let line = to_langsmith_line(&r);
        let v: serde_json::Value = serde_json::from_str(&line).expect("must be valid JSON");
        assert_eq!(v["run_type"].as_str().unwrap(), "llm");
        assert_eq!(v["session_name"].as_str().unwrap(), "opentrace");
        assert!(v["start_time"].as_i64().is_some());
        assert!(v["end_time"].as_i64().unwrap() >= v["start_time"].as_i64().unwrap());
    }

    #[test]
    fn weave_line_is_valid_json() {
        let r = make_record();
        let line = to_weave_line(&r);
        let v: serde_json::Value = serde_json::from_str(&line).expect("must be valid JSON");
        assert_eq!(v["calls"][0]["op_name"].as_str().unwrap(), "llm_call");
        assert_eq!(
            v["calls"][0]["summary"]["usage"]["gpt-4o"]["prompt_tokens"]
                .as_i64()
                .unwrap(),
            100
        );
    }

    #[test]
    fn add_ms_increases_timestamp() {
        let ts = "2026-02-23T10:00:00Z";
        let result = add_ms(ts, 1500); // 1.5 seconds
        assert!(
            result.contains("10:00:01") || result.contains("10:00:00"),
            "timestamp should advance"
        );
        assert_ne!(result, ts);
    }

    #[test]
    fn truncate_10k_does_not_panic_on_ascii() {
        let s = "x".repeat(20_000);
        let t = truncate_10k(&s);
        assert_eq!(t.len(), 10_240);
    }
}
