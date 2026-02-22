use bytes::Bytes;
use serde_json::Value;
use std::collections::HashMap;

/// Extract model name and streaming flag in a single JSON parse.
pub fn extract_request_info(body: &str) -> (String, bool) {
    if let Ok(v) = serde_json::from_str::<Value>(body) {
        let model = v["model"]
            .as_str()
            .map(String::from)
            .unwrap_or_else(|| "unknown".to_string());
        let streaming = v["stream"].as_bool().unwrap_or(false);
        (model, streaming)
    } else {
        ("unknown".to_string(), false)
    }
}

/// Extract token usage from OpenAI-compatible response body.
///
/// Handles both chat completions (has `completion_tokens`) and embeddings
/// (only `prompt_tokens` / `total_tokens` — `completion_tokens` absent).
/// When `completion_tokens` is absent, output defaults to 0 so embedding
/// costs are still captured correctly.
pub fn extract_usage(body: &str) -> Option<(i64, i64)> {
    let v: Value = serde_json::from_str(body).ok()?;
    let usage = v.get("usage")?;
    let input = usage["prompt_tokens"].as_i64()
        .or_else(|| usage["input_tokens"].as_i64())?;
    // Embeddings have no completion tokens — default to 0 so we still capture
    // the input token count and can estimate cost.
    let output = usage["completion_tokens"].as_i64()
        .or_else(|| usage["output_tokens"].as_i64())
        .unwrap_or(0);
    Some((input, output))
}

/// Reconstruct usage from SSE chunks (streaming responses).
///
/// Concatenates all chunks before parsing so that SSE events split across
/// network packet boundaries are handled correctly.
///
/// Handles two formats:
/// - OpenAI: single chunk near the end carries `usage` with both token counts.
/// - Anthropic: `input_tokens` appears in the early `message_start` event
///   (nested as `message.usage.input_tokens`); `output_tokens` appears later
///   in `message_delta.usage.output_tokens`.  Both are collected via a single
///   forward pass; later values overwrite earlier ones so the final counts win.
pub fn extract_usage_from_chunks(chunks: &[Bytes]) -> Option<(i64, i64)> {
    // Concatenate to handle events split across network packet boundaries.
    let mut full_text = String::new();
    for chunk in chunks {
        if let Ok(s) = std::str::from_utf8(chunk) {
            full_text.push_str(s);
        }
    }

    let mut input_tokens: Option<i64> = None;
    let mut output_tokens: Option<i64> = None;

    for line in full_text.lines() {
        let data = match line.strip_prefix("data: ") {
            Some(d) if d.trim() != "[DONE]" => d,
            _ => continue,
        };

        if let Ok(v) = serde_json::from_str::<Value>(data) {
            // Top-level `usage` — OpenAI style and Anthropic `message_delta`.
            if let Some(usage) = v.get("usage") {
                let inp = usage["prompt_tokens"].as_i64()
                    .or_else(|| usage["input_tokens"].as_i64());
                let out = usage["completion_tokens"].as_i64()
                    .or_else(|| usage["output_tokens"].as_i64());
                // Always overwrite: later values in the stream are more accurate.
                if inp.is_some() { input_tokens = inp; }
                if out.is_some() { output_tokens = out; }
            }

            // Anthropic `message_start` nests usage under `message.usage`.
            if let Some(usage) = v.get("message").and_then(|m| m.get("usage")) {
                let inp = usage["input_tokens"].as_i64();
                let out = usage["output_tokens"].as_i64();
                if inp.is_some() { input_tokens = inp; }
                if out.is_some() { output_tokens = out; }
            }
        }
    }

    // If we have input tokens, return them even if output tokens are missing.
    // Defaulting output to 0 is consistent with how extract_usage() handles
    // embedding endpoints (which never return completion_tokens).
    match input_tokens {
        Some(i) => Some((i, output_tokens.unwrap_or(0))),
        None => None,
    }
}

/// Extract the concatenated text content from SSE chunks.
///
/// Handles text and tool-call formats:
/// - OpenAI text:      `choices[0].delta.content`
/// - Anthropic text:   `delta.text` (content_block_delta events)
/// - OpenAI tool call: `choices[0].delta.tool_calls[N].function.{name,arguments}`
/// - Anthropic tool:   `content_block_start` with type `tool_use` + `input_json_delta`
///
/// Priority: text content always wins over tool-call summaries.  If only tool
/// calls are present, returns a compact JSON array:
///   `[{"name":"fn","arguments":{...}}]`
/// Arguments are parsed as JSON if valid; otherwise kept as a raw string.
///
/// Returns an empty string if nothing was captured.
pub fn extract_response_text_from_chunks(chunks: &[Bytes]) -> String {
    let mut full_text = String::new();
    for chunk in chunks {
        if let Ok(s) = std::str::from_utf8(chunk) {
            full_text.push_str(s);
        }
    }

    // Text content (highest priority).
    let mut text_result = String::new();

    // OpenAI tool calls: index -> (name, accumulated_arguments).
    let mut oai_tools: HashMap<usize, (String, String)> = HashMap::new();

    // Anthropic tool use: Vec of (name, accumulated_partial_json).
    // Tracks multiple parallel tool use blocks in the order they appear.
    let mut ant_tools: Vec<(String, String)> = Vec::new();

    for line in full_text.lines() {
        let data = match line.strip_prefix("data: ") {
            Some(d) if d.trim() != "[DONE]" => d,
            _ => continue,
        };

        let v = match serde_json::from_str::<Value>(data) {
            Ok(v) => v,
            Err(_) => continue,
        };

        // ---- OpenAI text ------------------------------------------------
        if let Some(content) = v["choices"][0]["delta"]["content"].as_str() {
            text_result.push_str(content);
        }

        // ---- Anthropic text ---------------------------------------------
        if let Some(text) = v["delta"]["text"].as_str() {
            text_result.push_str(text);
        }

        // ---- OpenAI tool calls ------------------------------------------
        if let Some(tool_calls) = v["choices"][0]["delta"]["tool_calls"].as_array() {
            for tc in tool_calls {
                let index = tc["index"].as_u64().unwrap_or(0) as usize;
                let entry = oai_tools.entry(index).or_insert_with(|| (String::new(), String::new()));

                if let Some(name) = tc["function"]["name"].as_str() {
                    if entry.0.is_empty() {
                        entry.0 = name.to_string();
                    }
                }
                if let Some(args_fragment) = tc["function"]["arguments"].as_str() {
                    entry.1.push_str(args_fragment);
                }
            }
        }

        // ---- Anthropic tool use -----------------------------------------
        // content_block_start: {"type":"content_block_start","content_block":{"type":"tool_use","name":"..."}}
        if v["type"].as_str() == Some("content_block_start") {
            if v["content_block"]["type"].as_str() == Some("tool_use") {
                if let Some(name) = v["content_block"]["name"].as_str() {
                    ant_tools.push((name.to_string(), String::new()));
                }
            }
        }

        // content_block_delta: {"type":"content_block_delta","delta":{"type":"input_json_delta","partial_json":"..."}}
        if v["type"].as_str() == Some("content_block_delta") {
            if v["delta"]["type"].as_str() == Some("input_json_delta") {
                if let Some(frag) = v["delta"]["partial_json"].as_str() {
                    // Append to the most recently opened tool use block.
                    if let Some(last) = ant_tools.last_mut() {
                        last.1.push_str(frag);
                    }
                }
            }
        }
    }

    // Text wins over tool-call summaries.
    if !text_result.is_empty() {
        return text_result;
    }

    // Build tool-call summary from OpenAI tool calls.
    if !oai_tools.is_empty() {
        let mut sorted: Vec<(usize, (String, String))> = oai_tools.into_iter().collect();
        sorted.sort_by_key(|(idx, _)| *idx);

        let mut calls = Vec::with_capacity(sorted.len());
        for (_, (name, raw_args)) in sorted {
            let args_value: Value = serde_json::from_str(&raw_args)
                .unwrap_or(Value::String(raw_args));
            calls.push(serde_json::json!({"name": name, "arguments": args_value}));
        }
        if let Ok(s) = serde_json::to_string(&calls) {
            return s;
        }
    }

    // Build tool-call summary from Anthropic tool use (supports multiple tools).
    if !ant_tools.is_empty() {
        let mut calls = Vec::with_capacity(ant_tools.len());
        for (name, raw_args) in ant_tools {
            let args_value: Value = serde_json::from_str(&raw_args)
                .unwrap_or(Value::String(raw_args));
            calls.push(serde_json::json!({"name": name, "arguments": args_value}));
        }
        if let Ok(s) = serde_json::to_string(&calls) {
            return s;
        }
    }

    String::new()
}

/// Detect provider from upstream URL.
pub fn detect_provider(upstream: &str) -> String {
    if upstream.contains("anthropic") {
        "anthropic".to_string()
    } else if upstream.contains("openai") {
        "openai".to_string()
    } else if upstream.contains("together") {
        "together".to_string()
    } else if upstream.contains("groq") {
        "groq".to_string()
    } else if upstream.contains("mistral") {
        "mistral".to_string()
    } else if upstream.contains("cohere") {
        "cohere".to_string()
    } else if upstream.contains("google") || upstream.contains("generativelanguage") {
        "google".to_string()
    } else {
        "unknown".to_string()
    }
}

/// Returns true if the model is in the known pricing table.
///
/// The only correct signal is whether `model_prices` returned the fallback
/// sentinel (1.00, 3.00).  Name-substring heuristics are NOT used here
/// because they would suppress cost warnings for unknown models that happen
/// to share a prefix with a known vendor (e.g. "gpt-new-unknown-v99").
pub fn is_known_model(model: &str) -> bool {
    !matches!(model_prices(model), (1.00, 3.00))
}

/// Estimate cost in USD. Returns 0.0 for invalid inputs.
pub fn estimate_cost(model: &str, input_tokens: i64, output_tokens: i64) -> f64 {
    if input_tokens < 0 || output_tokens < 0 {
        return 0.0;
    }
    let (input_price, output_price) = model_prices(model);
    (input_tokens as f64 * input_price + output_tokens as f64 * output_price) / 1_000_000.0
}

pub fn model_prices(model: &str) -> (f64, f64) {
    // Prices per 1M tokens (input, output) — USD
    // More specific patterns must appear before less specific ones.
    match model {
        // Embeddings — output_tokens is always 0, only input price matters
        m if m.contains("text-embedding-3-large") => (0.13, 0.0),
        m if m.contains("text-embedding-3-small") => (0.02, 0.0),
        m if m.contains("text-embedding-ada-002") => (0.10, 0.0),
        // OpenAI
        m if m.contains("gpt-4o-mini")       => (0.15,  0.60),
        m if m.contains("gpt-4o")            => (2.50,  10.00),
        m if m.contains("gpt-4-turbo")       => (10.00, 30.00),
        m if m.contains("gpt-4")             => (30.00, 60.00),
        m if m.contains("gpt-3.5")           => (0.50,  1.50),
        // NOTE: o1-mini / o3-mini must appear before o1 / o3 — more-specific
        // substrings first, otherwise "o1-mini" would match the "o1" arm.
        m if m.contains("o1-mini")           => (1.10,  4.40),
        m if m.contains("o1-preview")        => (15.00, 60.00),
        m if m.contains("o1")               => (15.00, 60.00),
        m if m.contains("o3-mini")           => (1.10,  4.40),
        m if m.contains("o3")               => (10.00, 40.00),
        // Anthropic Claude 4
        m if m.contains("claude-sonnet-4")   => (3.00,  15.00),
        m if m.contains("claude-opus-4")     => (15.00, 75.00),
        m if m.contains("claude-haiku-4")    => (0.80,  4.00),
        // Anthropic Claude 3.5
        m if m.contains("claude-3-5-sonnet") => (3.00,  15.00),
        m if m.contains("claude-3-5-haiku")  => (0.80,  4.00),
        // Anthropic Claude 3
        m if m.contains("claude-3-opus")     => (15.00, 75.00),
        m if m.contains("claude-3-sonnet")   => (3.00,  15.00),
        m if m.contains("claude-3-haiku")    => (0.25,  1.25),
        // Google Gemini
        m if m.contains("gemini-2.0-flash")  => (0.10,  0.40),
        m if m.contains("gemini-2.0")        => (0.10,  0.40),
        m if m.contains("gemini-1.5-pro")    => (1.25,  5.00),
        m if m.contains("gemini-1.5-flash")  => (0.075, 0.30),
        // Meta Llama
        m if m.contains("llama-3.3-70b")     => (0.59,  0.79),
        m if m.contains("llama-3.1-405b")    => (3.00,  3.00),
        m if m.contains("llama-3.1-70b")     => (0.52,  0.75),
        m if m.contains("llama-3.1-8b")      => (0.18,  0.18),
        // Mistral / Mixtral
        m if m.contains("mistral-large")     => (2.00,  6.00),
        m if m.contains("mistral-small")     => (0.20,  0.60),
        m if m.contains("mixtral")           => (0.24,  0.24),
        // Others
        m if m.contains("deepseek")          => (0.14,  0.28),
        m if m.contains("gemma")             => (0.10,  0.10),
        _ => (1.00, 3.00), // fallback — may be inaccurate for unlisted models
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bytes::Bytes;

    // -------------------------------------------------------------------------
    // extract_request_info
    // -------------------------------------------------------------------------

    #[test]
    fn extract_request_info_valid_json() {
        let body = r#"{"model":"gpt-4o","stream":false,"messages":[]}"#;
        let (model, streaming) = extract_request_info(body);
        assert_eq!(model, "gpt-4o");
        assert!(!streaming);
    }

    #[test]
    fn extract_request_info_stream_true() {
        let body = r#"{"model":"claude-3-5-sonnet-20241022","stream":true}"#;
        let (model, streaming) = extract_request_info(body);
        assert_eq!(model, "claude-3-5-sonnet-20241022");
        assert!(streaming);
    }

    #[test]
    fn extract_request_info_missing_model_field() {
        let body = r#"{"stream":false,"messages":[]}"#;
        let (model, streaming) = extract_request_info(body);
        assert_eq!(model, "unknown");
        assert!(!streaming);
    }

    #[test]
    fn extract_request_info_missing_stream_field() {
        // Absent stream field should default to false, not error.
        let body = r#"{"model":"gpt-4o-mini","messages":[]}"#;
        let (model, streaming) = extract_request_info(body);
        assert_eq!(model, "gpt-4o-mini");
        assert!(!streaming, "missing stream field should default to false");
    }

    #[test]
    fn extract_request_info_invalid_json() {
        let body = "this is not json at all";
        let (model, streaming) = extract_request_info(body);
        assert_eq!(model, "unknown");
        assert!(!streaming);
    }

    #[test]
    fn extract_request_info_empty_string() {
        let (model, streaming) = extract_request_info("");
        assert_eq!(model, "unknown");
        assert!(!streaming);
    }

    #[test]
    fn extract_request_info_model_is_not_string() {
        // model field exists but is a number — should fall back to "unknown".
        let body = r#"{"model":42,"stream":false}"#;
        let (model, _) = extract_request_info(body);
        assert_eq!(model, "unknown");
    }

    #[test]
    fn extract_request_info_stream_is_not_bool() {
        // stream is a string, not a bool — should fall back to false.
        let body = r#"{"model":"gpt-4o","stream":"yes"}"#;
        let (_, streaming) = extract_request_info(body);
        assert!(!streaming);
    }

    // -------------------------------------------------------------------------
    // extract_usage
    // -------------------------------------------------------------------------

    #[test]
    fn extract_usage_openai_format() {
        let body = r#"{
            "usage": {
                "prompt_tokens": 150,
                "completion_tokens": 40,
                "total_tokens": 190
            }
        }"#;
        let result = extract_usage(body);
        assert_eq!(result, Some((150, 40)));
    }

    #[test]
    fn extract_usage_anthropic_format() {
        let body = r#"{
            "usage": {
                "input_tokens": 312,
                "output_tokens": 88
            }
        }"#;
        let result = extract_usage(body);
        assert_eq!(result, Some((312, 88)));
    }

    #[test]
    fn extract_usage_missing_usage_field() {
        let body = r#"{"id":"chatcmpl-abc","choices":[]}"#;
        assert_eq!(extract_usage(body), None);
    }

    #[test]
    fn extract_usage_usage_field_missing_tokens() {
        // usage block exists but has neither recognised key for input — returns None.
        let body = r#"{"usage":{"total_tokens":50}}"#;
        assert_eq!(extract_usage(body), None);
    }

    #[test]
    fn extract_usage_zero_tokens() {
        let body = r#"{"usage":{"prompt_tokens":0,"completion_tokens":0}}"#;
        assert_eq!(extract_usage(body), Some((0, 0)));
    }

    #[test]
    fn extract_usage_negative_tokens() {
        // Negative integers are valid JSON — the function extracts them as-is.
        // The caller (estimate_cost) is responsible for guarding against negatives.
        let body = r#"{"usage":{"prompt_tokens":-5,"completion_tokens":-2}}"#;
        let result = extract_usage(body);
        assert_eq!(result, Some((-5, -2)));
    }

    #[test]
    fn extract_usage_invalid_json() {
        assert_eq!(extract_usage("not json"), None);
    }

    #[test]
    fn extract_usage_empty_string() {
        assert_eq!(extract_usage(""), None);
    }

    #[test]
    fn extract_usage_embedding_no_completion_tokens_defaults_to_zero() {
        // OpenAI embeddings endpoint returns prompt_tokens and total_tokens
        // but no completion_tokens — output should default to 0.
        let body = r#"{"usage":{"prompt_tokens":50,"total_tokens":50}}"#;
        assert_eq!(extract_usage(body), Some((50, 0)));
    }

    #[test]
    fn extract_usage_embedding_input_tokens_style() {
        // Anthropic-style embeddings (input_tokens only).
        let body = r#"{"usage":{"input_tokens":128}}"#;
        assert_eq!(extract_usage(body), Some((128, 0)));
    }

    // -------------------------------------------------------------------------
    // extract_usage_from_chunks
    // -------------------------------------------------------------------------

    /// Build a Bytes chunk from a raw SSE payload string.
    fn sse(payload: &str) -> Bytes {
        Bytes::from(payload.to_string())
    }

    #[test]
    fn extract_usage_from_chunks_openai_realistic_sequence() {
        // Simulate a typical OpenAI streaming response:
        //   - Several delta chunks with no usage
        //   - Final chunk carrying the usage object
        //   - Terminating [DONE] sentinel
        let chunks = vec![
            sse("data: {\"id\":\"x\",\"choices\":[{\"delta\":{\"role\":\"assistant\"}}]}\n\n"),
            sse("data: {\"id\":\"x\",\"choices\":[{\"delta\":{\"content\":\"Hello\"}}]}\n\n"),
            sse("data: {\"id\":\"x\",\"choices\":[{\"delta\":{\"content\":\" world\"}}]}\n\n"),
            sse("data: {\"id\":\"x\",\"choices\":[{\"delta\":{}}],\"usage\":{\"prompt_tokens\":100,\"completion_tokens\":25,\"total_tokens\":125}}\n\n"),
            sse("data: [DONE]\n\n"),
        ];
        let result = extract_usage_from_chunks(&chunks);
        assert_eq!(result, Some((100, 25)));
    }

    #[test]
    fn extract_usage_from_chunks_no_usage_in_any_chunk() {
        let chunks = vec![
            sse("data: {\"id\":\"x\",\"choices\":[{\"delta\":{\"content\":\"Hi\"}}]}\n\n"),
            sse("data: {\"id\":\"x\",\"choices\":[{\"delta\":{\"content\":\"!\"}}]}\n\n"),
            sse("data: [DONE]\n\n"),
        ];
        assert_eq!(extract_usage_from_chunks(&chunks), None);
    }

    #[test]
    fn extract_usage_from_chunks_empty_slice() {
        assert_eq!(extract_usage_from_chunks(&[]), None);
    }

    #[test]
    fn extract_usage_from_chunks_done_only() {
        let chunks = vec![sse("data: [DONE]\n\n")];
        assert_eq!(extract_usage_from_chunks(&chunks), None);
    }

    #[test]
    fn extract_usage_from_chunks_anthropic_style_events() {
        // Anthropic SSE: both tokens happen to appear in message_delta.usage.
        let chunks = vec![
            sse("event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_01\"}}\n\n"),
            sse("event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"delta\":{\"text\":\"Hello\"}}\n\n"),
            sse("event: message_delta\ndata: {\"type\":\"message_delta\",\"usage\":{\"input_tokens\":80,\"output_tokens\":12}}\n\n"),
        ];
        let result = extract_usage_from_chunks(&chunks);
        assert_eq!(result, Some((80, 12)));
    }

    #[test]
    fn extract_usage_from_chunks_anthropic_real_format() {
        // Real Anthropic streaming:
        // - message_start carries input_tokens nested under message.usage
        // - message_delta carries only output_tokens at top-level usage
        let chunks = vec![
            sse("event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_01\",\"type\":\"message\",\"role\":\"assistant\",\"content\":[],\"model\":\"claude-3-5-sonnet-20241022\",\"stop_reason\":null,\"stop_sequence\":null,\"usage\":{\"input_tokens\":100,\"output_tokens\":1}}}\n\n"),
            sse("event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"delta\":{\"type\":\"text_delta\",\"text\":\"Hello world\"}}\n\n"),
            sse("event: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\",\"stop_sequence\":null},\"usage\":{\"output_tokens\":25}}\n\n"),
            sse("event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n"),
        ];
        let result = extract_usage_from_chunks(&chunks);
        // input from message_start, output from message_delta (overwrites preliminary 1)
        assert_eq!(result, Some((100, 25)));
    }

    #[test]
    fn extract_usage_from_chunks_sse_event_split_across_chunks() {
        // Simulate an SSE event split across two network packets.
        // The concatenation in extract_usage_from_chunks must reassemble it.
        let chunks = vec![
            // chunk 1 ends mid-JSON
            Bytes::from("data: {\"usage\":{\"prompt_tokens\":50,\"completio"),
            // chunk 2 continues the JSON
            Bytes::from("n_tokens\":20}}\n\n"),
        ];
        let result = extract_usage_from_chunks(&chunks);
        assert_eq!(result, Some((50, 20)));
    }

    #[test]
    fn extract_usage_from_chunks_usage_in_last_chunk_found_first_by_rev_scan() {
        // Forward scan with overwrite means later values win over earlier ones.
        let chunks = vec![
            sse("data: {\"usage\":{\"prompt_tokens\":999,\"completion_tokens\":999}}\n\n"),
            sse("data: {\"usage\":{\"prompt_tokens\":10,\"completion_tokens\":5}}\n\n"),
        ];
        // Second chunk is processed last and overwrites, so its values win.
        let result = extract_usage_from_chunks(&chunks);
        assert_eq!(result, Some((10, 5)));
    }

    #[test]
    fn extract_usage_from_chunks_chunk_with_only_input_tokens_defaults_output_to_zero() {
        // Embedding endpoints return only prompt_tokens — output_tokens defaults to 0
        // so that cost can still be calculated (not silently dropped).
        let chunks = vec![
            sse("data: {\"usage\":{\"prompt_tokens\":50}}\n\n"),
        ];
        assert_eq!(extract_usage_from_chunks(&chunks), Some((50, 0)));
    }

    #[test]
    fn extract_usage_from_chunks_non_data_lines_ignored() {
        // Lines without "data: " prefix (e.g. SSE event:, id:, comments) must
        // be skipped without panicking.
        let chunks = vec![
            sse(": keep-alive\nevent: ping\ndata: {\"usage\":{\"prompt_tokens\":20,\"completion_tokens\":8}}\n\n"),
        ];
        assert_eq!(extract_usage_from_chunks(&chunks), Some((20, 8)));
    }

    // -------------------------------------------------------------------------
    // extract_response_text_from_chunks
    // -------------------------------------------------------------------------

    #[test]
    fn extract_response_text_openai_delta_content() {
        let chunks = vec![
            sse("data: {\"choices\":[{\"delta\":{\"content\":\"Hello\"}}]}\n\n"),
            sse("data: {\"choices\":[{\"delta\":{\"content\":\" world\"}}]}\n\n"),
            sse("data: [DONE]\n\n"),
        ];
        assert_eq!(extract_response_text_from_chunks(&chunks), "Hello world");
    }

    #[test]
    fn extract_response_text_anthropic_delta_text() {
        let chunks = vec![
            sse("data: {\"type\":\"content_block_delta\",\"delta\":{\"type\":\"text_delta\",\"text\":\"Hi\"}}\n\n"),
            sse("data: {\"type\":\"content_block_delta\",\"delta\":{\"type\":\"text_delta\",\"text\":\"!\"}}\n\n"),
        ];
        assert_eq!(extract_response_text_from_chunks(&chunks), "Hi!");
    }

    #[test]
    fn extract_response_text_empty_stream() {
        let chunks: Vec<Bytes> = vec![];
        assert_eq!(extract_response_text_from_chunks(&chunks), "");
    }

    #[test]
    fn extract_response_text_done_only() {
        let chunks = vec![sse("data: [DONE]\n\n")];
        assert_eq!(extract_response_text_from_chunks(&chunks), "");
    }

    #[test]
    fn extract_response_text_no_content_fields() {
        // Usage-only chunks produce no text.
        let chunks = vec![
            sse("data: {\"usage\":{\"prompt_tokens\":50,\"completion_tokens\":20}}\n\n"),
        ];
        assert_eq!(extract_response_text_from_chunks(&chunks), "");
    }

    #[test]
    fn extract_response_text_openai_tool_call_captured() {
        let chunks = vec![
            sse("data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_1\",\"function\":{\"name\":\"get_weather\",\"arguments\":\"\"}}]}}]}\n\n"),
            sse("data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"arguments\":\"{\\\"location\\\": \\\"NYC\\\"}\"}}]}}]}\n\n"),
            sse("data: [DONE]\n\n"),
        ];
        let result = extract_response_text_from_chunks(&chunks);
        assert!(!result.is_empty(), "tool call should be captured");
        assert!(result.contains("get_weather"), "tool name should appear in output");
    }

    #[test]
    fn extract_response_text_prefers_text_over_tool_call() {
        // When both text and tool calls are present, text content wins.
        let chunks = vec![
            sse("data: {\"choices\":[{\"delta\":{\"content\":\"Hello\",\"tool_calls\":[{\"index\":0,\"function\":{\"name\":\"fn\",\"arguments\":\"{}\"}}]}}]}\n\n"),
        ];
        let result = extract_response_text_from_chunks(&chunks);
        assert_eq!(result, "Hello", "text content should take priority over tool calls");
    }

    #[test]
    fn extract_response_text_anthropic_tool_use_captured() {
        let chunks = vec![
            sse("data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"tool_use\",\"id\":\"toolu_01\",\"name\":\"get_weather\",\"input\":{}}}\n\n"),
            sse("data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"{\\\"location\\\": \"}}\n\n"),
            sse("data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"\\\"NYC\\\"}\"  }}\n\n"),
            sse("data: {\"type\":\"content_block_stop\",\"index\":0}\n\n"),
        ];
        let result = extract_response_text_from_chunks(&chunks);
        assert!(!result.is_empty(), "Anthropic tool use should be captured");
        assert!(result.contains("get_weather"), "tool name should appear in output");
    }

    // -------------------------------------------------------------------------
    // estimate_cost
    // -------------------------------------------------------------------------

    #[test]
    fn estimate_cost_gpt4o_known_prices() {
        // gpt-4o: $2.50/M input, $10.00/M output
        // 1_000 input + 500 output = 0.0025 + 0.005 = $0.0075
        let cost = estimate_cost("gpt-4o", 1_000, 500);
        let expected = (1_000.0 * 2.50 + 500.0 * 10.00) / 1_000_000.0;
        assert!((cost - expected).abs() < 1e-9, "cost={cost} expected={expected}");
    }

    #[test]
    fn estimate_cost_unknown_model_uses_fallback() {
        // Unknown model falls back to $1.00/$3.00 per MTok.
        let cost = estimate_cost("some-new-model-xyz", 1_000_000, 0);
        let expected = 1.00; // 1M input tokens * $1.00/M
        assert!((cost - expected).abs() < 1e-9);
    }

    #[test]
    fn estimate_cost_zero_tokens() {
        assert_eq!(estimate_cost("gpt-4o", 0, 0), 0.0);
    }

    #[test]
    fn estimate_cost_negative_input_tokens_returns_zero() {
        assert_eq!(estimate_cost("gpt-4o", -1, 100), 0.0);
    }

    #[test]
    fn estimate_cost_negative_output_tokens_returns_zero() {
        assert_eq!(estimate_cost("gpt-4o", 100, -1), 0.0);
    }

    #[test]
    fn estimate_cost_both_negative_returns_zero() {
        assert_eq!(estimate_cost("gpt-4o", -10, -10), 0.0);
    }

    #[test]
    fn estimate_cost_claude_opus_4() {
        // claude-opus-4: $15.00/M input, $75.00/M output
        let cost = estimate_cost("claude-opus-4-5", 2_000, 1_000);
        let expected = (2_000.0 * 15.00 + 1_000.0 * 75.00) / 1_000_000.0;
        assert!((cost - expected).abs() < 1e-9);
    }

    #[test]
    fn estimate_cost_gpt4o_mini_cheaper_than_gpt4o() {
        // gpt-4o-mini must match its own price row, not the gpt-4o row.
        let cost_mini = estimate_cost("gpt-4o-mini", 1_000_000, 1_000_000);
        let cost_full = estimate_cost("gpt-4o", 1_000_000, 1_000_000);
        assert!(cost_mini < cost_full, "gpt-4o-mini should be cheaper than gpt-4o");
    }

    #[test]
    fn estimate_cost_embedding_model() {
        // text-embedding-3-large: $0.13/M input, $0.0/M output
        // 1_000 tokens → $0.00013
        let cost = estimate_cost("text-embedding-3-large", 1_000, 0);
        let expected = 1_000.0 * 0.13 / 1_000_000.0;
        assert!((cost - expected).abs() < 1e-12);
    }

    // -------------------------------------------------------------------------
    // is_known_model
    // -------------------------------------------------------------------------

    #[test]
    fn is_known_model_gpt4o_is_known() {
        assert!(is_known_model("gpt-4o"));
    }

    #[test]
    fn is_known_model_gpt4o_mini_is_known() {
        assert!(is_known_model("gpt-4o-mini"));
    }

    #[test]
    fn is_known_model_claude_3_5_sonnet_is_known() {
        assert!(is_known_model("claude-3-5-sonnet-20241022"));
    }

    #[test]
    fn is_known_model_gemini_is_known() {
        assert!(is_known_model("gemini-1.5-pro"));
    }

    #[test]
    fn is_known_model_llama_is_known() {
        assert!(is_known_model("llama-3.1-70b"));
    }

    #[test]
    fn is_known_model_deepseek_is_known() {
        assert!(is_known_model("deepseek-chat"));
    }

    #[test]
    fn is_known_model_embedding_is_known() {
        assert!(is_known_model("text-embedding-3-large"));
        assert!(is_known_model("text-embedding-3-small"));
        assert!(is_known_model("text-embedding-ada-002"));
    }

    #[test]
    fn is_known_model_completely_unknown_model() {
        // A model with no recognised substring — the function inspects both the
        // price table match (falls through to the 1.00/3.00 fallback) and the
        // explicit substring checks.  "xyz-new-model" has none of them.
        assert!(!is_known_model("xyz-new-model-v99"));
    }

    #[test]
    fn is_known_model_empty_string() {
        assert!(!is_known_model(""));
    }

    // -------------------------------------------------------------------------
    // detect_provider
    // -------------------------------------------------------------------------

    #[test]
    fn detect_provider_openai() {
        assert_eq!(detect_provider("https://api.openai.com"), "openai");
    }

    #[test]
    fn detect_provider_anthropic() {
        assert_eq!(detect_provider("https://api.anthropic.com"), "anthropic");
    }

    #[test]
    fn detect_provider_together() {
        assert_eq!(detect_provider("https://api.together.xyz"), "together");
    }

    #[test]
    fn detect_provider_groq() {
        assert_eq!(detect_provider("https://api.groq.com"), "groq");
    }

    #[test]
    fn detect_provider_mistral() {
        assert_eq!(detect_provider("https://api.mistral.ai"), "mistral");
    }

    #[test]
    fn detect_provider_cohere() {
        assert_eq!(detect_provider("https://api.cohere.ai"), "cohere");
    }

    #[test]
    fn detect_provider_google_generativelanguage() {
        assert_eq!(
            detect_provider("https://generativelanguage.googleapis.com"),
            "google"
        );
    }

    #[test]
    fn detect_provider_google_by_hostname() {
        assert_eq!(detect_provider("https://google.com/some/path"), "google");
    }

    #[test]
    fn detect_provider_unknown() {
        assert_eq!(detect_provider("https://some-other-provider.io"), "unknown");
    }

    #[test]
    fn detect_provider_empty_string() {
        assert_eq!(detect_provider(""), "unknown");
    }

    #[test]
    fn detect_provider_anthropic_takes_priority_over_openai_if_both_present() {
        // Anthropic check comes first in the if-else chain.
        assert_eq!(detect_provider("https://anthropic-openai-bridge.example.com"), "anthropic");
    }
}
