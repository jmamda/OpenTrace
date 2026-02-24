use bytes::Bytes;
use serde_json::Value;
use std::collections::HashMap;
use crate::store::CallRecord;

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

/// Replace the values of top-level JSON object keys listed in `fields` with
/// `"[REDACTED]"`.  Only the **stored** copy is modified — forwarded traffic
/// is never touched.
///
/// Returns the original body unchanged if:
/// - `fields` is empty
/// - the body is not valid JSON
/// - the top-level value is not a JSON object
pub fn redact_json_fields(body: &str, fields: &[String]) -> String {
    if fields.is_empty() {
        return body.to_string();
    }
    let mut value: Value = match serde_json::from_str(body) {
        Ok(v) => v,
        Err(_) => return body.to_string(),
    };
    if let Some(obj) = value.as_object_mut() {
        for field in fields {
            if obj.contains_key(field.as_str()) {
                obj.insert(field.clone(), Value::String("[REDACTED]".to_string()));
            }
        }
    }
    serde_json::to_string(&value).unwrap_or_else(|_| body.to_string())
}

/// Detect provider from upstream URL.
///
/// More-specific patterns (longer hostnames, branded keywords) are checked
/// before shorter ones to avoid false matches (e.g. "azure-openai" before
/// plain "openai").
pub fn detect_provider(upstream: &str) -> String {
    // Amazon Bedrock first — its URLs contain "amazonaws.com" which is
    // unambiguous, and model path segments may include "anthropic" or "openai".
    if upstream.contains("bedrock") && upstream.contains("amazonaws") {
        "bedrock".to_string()
    // Azure OpenAI before plain openai.
    } else if upstream.contains("openai.azure.com") || upstream.contains("azure.com/openai") {
        "azure-openai".to_string()
    // Anthropic before openai — catches anthropic-openai bridges.
    } else if upstream.contains("anthropic") {
        "anthropic".to_string()
    } else if upstream.contains("openai") {
        "openai".to_string()
    } else if upstream.contains("openrouter") {
        "openrouter".to_string()
    } else if upstream.contains("x.ai") {
        "xai".to_string()
    } else if upstream.contains("perplexity") {
        "perplexity".to_string()
    // Zhipu AI — GLM models (api.z.ai / bigmodel.cn)
    } else if upstream.contains("z.ai") || upstream.contains("bigmodel") || upstream.contains("zhipu") {
        "zhipu".to_string()
    // Moonshot AI — Kimi models (platform.moonshot.ai / api.moonshot.cn)
    } else if upstream.contains("moonshot") {
        "moonshot".to_string()
    } else if upstream.contains("fireworks") {
        "fireworks".to_string()
    // NVIDIA NIM API: integrate.api.nvidia.com
    } else if upstream.contains("nvidia") {
        "nvidia".to_string()
    } else if upstream.contains("together") {
        "together".to_string()
    } else if upstream.contains("groq") {
        "groq".to_string()
    } else if upstream.contains("mistral") {
        "mistral".to_string()
    } else if upstream.contains("cohere") {
        "cohere".to_string()
    // Alibaba Cloud DashScope — Qwen models (dashscope.aliyuncs.com or api.dashscope.ai)
    } else if upstream.contains("dashscope") || upstream.contains("aliyun") {
        "qwen".to_string()
    // AI21 Labs — Jamba models (api.ai21.com)
    } else if upstream.contains("ai21") {
        "ai21".to_string()
    } else if upstream.contains("google") || upstream.contains("generativelanguage") {
        "google".to_string()
    } else if upstream.contains("deepseek") {
        "deepseek".to_string()
    } else if upstream.contains("ollama")
        || upstream.contains("localhost")
        || upstream.contains("127.0.0.1")
        || upstream.contains("::1")
    {
        "ollama".to_string()
    } else {
        "unknown".to_string()
    }
}

/// FNV-1a 64-bit hash — zero new dependencies.
fn fnv1a_64(s: &str) -> u64 {
    let mut h: u64 = 14695981039346656037;
    for b in s.bytes() {
        h ^= b as u64;
        h = h.wrapping_mul(1099511628211);
    }
    h
}

/// Extract system prompt from OpenAI (messages[*].role=="system") or
/// Anthropic ("system" field) request body, return its FNV-64 hex fingerprint.
///
/// Returns `None` when no system prompt is present or the body is not valid JSON.
pub fn extract_prompt_hash(req_body: &str) -> Option<String> {
    let v: serde_json::Value = serde_json::from_str(req_body).ok()?;
    let text = v
        .get("system")
        .and_then(|s| s.as_str())
        .or_else(|| {
            v.get("messages")?
                .as_array()?
                .iter()
                .find(|m| m.get("role").and_then(|r| r.as_str()) == Some("system"))?
                .get("content")?
                .as_str()
        })?;
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return None;
    }
    Some(format!("{:016x}", fnv1a_64(trimmed)))
}

/// Default upstream URL for a stored provider name.
/// Used by `trace replay` when no --upstream flag is given.
pub fn default_upstream_for_provider(provider: &str) -> &'static str {
    match provider {
        "anthropic" => "https://api.anthropic.com",
        "google"    => "https://generativelanguage.googleapis.com",
        _           => "https://api.openai.com",
    }
}

/// Classify the error type of a failed call for display purposes.
///
/// Returns a static label such as "rate_limit", "auth", "upstream_5xx",
/// "timeout", or "connection".  Returns `None` for successful calls or
/// failures that don't match any recognised pattern.
///
/// This is display-only — the classification is never stored in the database.
pub fn classify_error(r: &CallRecord) -> Option<&'static str> {
    match r.status_code {
        401 | 403 => Some("auth"),
        429       => Some("rate_limit"),
        s if s >= 500 => Some("upstream_5xx"),
        0 => {
            if let Some(e) = &r.error {
                if e.contains("timeout") || e.contains("timed out") {
                    return Some("timeout");
                }
                if e.contains("connection") {
                    return Some("connection");
                }
            }
            None
        }
        _ => None,
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

/// Estimate cost from a non-streaming response body, adjusting for prompt-cache discounts.
///
/// Anthropic prompt cache pricing:
/// - Cache write (`cache_creation_input_tokens`): 1.25× normal input price
/// - Cache read  (`cache_read_input_tokens`):      0.10× normal input price
///
/// OpenAI prompt cache pricing:
/// - Cached reads (`prompt_tokens_details.cached_tokens`): 0.50× normal input price
///
/// Falls back to `estimate_cost()` when no cache tokens are found.
pub fn estimate_cost_from_body(
    model: &str,
    body: &str,
    input_tokens: i64,
    output_tokens: i64,
) -> f64 {
    if input_tokens < 0 || output_tokens < 0 {
        return 0.0;
    }
    let (input_price, output_price) = model_prices(model);

    if let Ok(v) = serde_json::from_str::<serde_json::Value>(body) {
        if let Some(usage) = v.get("usage") {
            // Anthropic prompt cache
            let write = usage["cache_creation_input_tokens"].as_i64().unwrap_or(0);
            let read  = usage["cache_read_input_tokens"].as_i64().unwrap_or(0);
            if write > 0 || read > 0 {
                let regular = (input_tokens - write - read).max(0);
                return (regular as f64 * input_price
                    + write as f64 * input_price * 1.25
                    + read  as f64 * input_price * 0.10
                    + output_tokens as f64 * output_price)
                    / 1_000_000.0;
            }
            // OpenAI prompt cache
            let cached = usage["prompt_tokens_details"]["cached_tokens"].as_i64().unwrap_or(0);
            if cached > 0 {
                let regular = (input_tokens - cached).max(0);
                return (regular as f64 * input_price
                    + cached as f64 * input_price * 0.50
                    + output_tokens as f64 * output_price)
                    / 1_000_000.0;
            }
        }
    }
    estimate_cost(model, input_tokens, output_tokens)
}

/// Estimate cost from streaming SSE chunks, adjusting for prompt-cache discounts.
///
/// Parses the accumulated chunk stream for Anthropic and OpenAI cache token fields
/// using the same discount rates as `estimate_cost_from_body`.
/// Falls back to `estimate_cost()` when cache tokens are absent.
pub fn estimate_cost_from_chunks(
    model: &str,
    chunks: &[Bytes],
    input_tokens: i64,
    output_tokens: i64,
) -> f64 {
    if input_tokens < 0 || output_tokens < 0 {
        return 0.0;
    }
    let (input_price, output_price) = model_prices(model);

    // Collect cache token counts across all SSE events.
    let mut ant_write: i64 = 0;
    let mut ant_read:  i64 = 0;
    let mut oai_cached: i64 = 0;

    let mut full_text = String::new();
    for chunk in chunks {
        if let Ok(s) = std::str::from_utf8(chunk) {
            full_text.push_str(s);
        }
    }

    for line in full_text.lines() {
        let data = match line.strip_prefix("data: ") {
            Some(d) if d.trim() != "[DONE]" => d,
            _ => continue,
        };
        if let Ok(v) = serde_json::from_str::<serde_json::Value>(data) {
            // Anthropic: usage in message_start (nested under message.usage)
            if let Some(usage) = v.get("message").and_then(|m| m.get("usage")) {
                let w = usage["cache_creation_input_tokens"].as_i64().unwrap_or(0);
                let r = usage["cache_read_input_tokens"].as_i64().unwrap_or(0);
                if w > 0 { ant_write = w; }
                if r > 0 { ant_read  = r; }
            }
            // Anthropic: usage in message_delta; OpenAI: top-level usage
            if let Some(usage) = v.get("usage") {
                let w = usage["cache_creation_input_tokens"].as_i64().unwrap_or(0);
                let r = usage["cache_read_input_tokens"].as_i64().unwrap_or(0);
                if w > 0 { ant_write = w; }
                if r > 0 { ant_read  = r; }
                let c = usage["prompt_tokens_details"]["cached_tokens"].as_i64().unwrap_or(0);
                if c > 0 { oai_cached = c; }
            }
        }
    }

    if ant_write > 0 || ant_read > 0 {
        let regular = (input_tokens - ant_write - ant_read).max(0);
        return (regular as f64 * input_price
            + ant_write as f64 * input_price * 1.25
            + ant_read  as f64 * input_price * 0.10
            + output_tokens as f64 * output_price)
            / 1_000_000.0;
    }
    if oai_cached > 0 {
        let regular = (input_tokens - oai_cached).max(0);
        return (regular as f64 * input_price
            + oai_cached as f64 * input_price * 0.50
            + output_tokens as f64 * output_price)
            / 1_000_000.0;
    }
    estimate_cost(model, input_tokens, output_tokens)
}

pub fn model_prices(model: &str) -> (f64, f64) {
    // Prices per 1M tokens (input, output) — USD.
    // ORDERING RULE: more-specific patterns (longer substrings) MUST appear
    // before less-specific ones, or the less-specific arm fires first.
    // e.g. "gpt-4.1" before "gpt-4", "o1-mini" before "o1", etc.
    match model {
        // ── Embeddings (output_tokens always 0; only input price matters) ──
        m if m.contains("text-embedding-3-large")    => (0.13,   0.0),
        m if m.contains("text-embedding-3-small")    => (0.02,   0.0),
        m if m.contains("text-embedding-ada-002")    => (0.10,   0.0),

        // ── OpenAI o-series reasoning (mini before base) ──────────────────
        m if m.contains("o1-mini")                   => (1.10,   4.40),
        m if m.contains("o1-preview")                => (15.00,  60.00),
        m if m.contains("o1")                        => (15.00,  60.00),
        m if m.contains("o3-mini")                   => (1.10,   4.40),
        m if m.contains("o3")                        => (10.00,  40.00),

        // ── OpenAI GPT-5 series ───────────────────────────────────────────
        // Ordering: specific versions before generic; gpt-5.1-codex before
        // gpt-5.1 (since the former contains the latter as a substring).
        m if m.contains("gpt-5.2")                  => (1.75,   14.00),
        m if m.contains("gpt-5.1-codex")            => (0.25,    2.00),
        m if m.contains("codex-mini")               => (1.50,    6.00),  // codex-mini-latest
        m if m.contains("gpt-5.1")                  => (1.25,   10.00),
        m if m.contains("gpt-5")                    => (1.25,   10.00),  // gpt-5 / gpt-5.0 generic

        // ── OpenAI GPT-4.1 (before gpt-4o and gpt-4) ─────────────────────
        m if m.contains("gpt-4.1-mini")             => (0.40,    1.60),
        m if m.contains("gpt-4.1-nano")             => (0.10,    0.40),
        m if m.contains("gpt-4.1")                  => (2.00,    8.00),

        // ── OpenAI GPT-4o (mini before base) ──────────────────────────────
        m if m.contains("gpt-4o-mini")               => (0.15,   0.60),
        m if m.contains("gpt-4o")                    => (2.50,   10.00),

        // ── OpenAI GPT-4 legacy ────────────────────────────────────────────
        m if m.contains("gpt-4-turbo")               => (10.00,  30.00),
        m if m.contains("gpt-4")                     => (30.00,  60.00),
        m if m.contains("gpt-3.5")                   => (0.50,   1.50),

        // ── Anthropic Claude 4 ────────────────────────────────────────────
        // Opus 4.x is $5/$25 (NOT $15/$75 — that was Claude 3 Opus below).
        // Thinking variants are billed at the same token rate as base.
        // Named versions (4.6, 4.5) listed explicitly then caught by catch-all.
        m if m.contains("claude-opus-4-6")           => (5.00,   25.00),  // opus 4.6 + -thinking
        m if m.contains("claude-opus-4-5")           => (5.00,   25.00),
        m if m.contains("claude-opus-4")             => (5.00,   25.00),  // future opus-4.x

        m if m.contains("claude-sonnet-4-6")         => (3.00,   15.00),
        m if m.contains("claude-sonnet-4-5")         => (3.00,   15.00),
        m if m.contains("claude-sonnet-4")           => (3.00,   15.00),  // future sonnet-4.x

        m if m.contains("claude-haiku-4")            => (1.00,    5.00),

        // ── Anthropic Claude 3.7 ──────────────────────────────────────────
        m if m.contains("claude-3-7-sonnet")         => (3.00,   15.00),

        // ── Anthropic Claude 3.5 ──────────────────────────────────────────
        m if m.contains("claude-3-5-sonnet")         => (3.00,   15.00),
        m if m.contains("claude-3-5-haiku")          => (0.80,   4.00),

        // ── Anthropic Claude 3 ────────────────────────────────────────────
        m if m.contains("claude-3-opus")             => (15.00,  75.00),
        m if m.contains("claude-3-sonnet")           => (3.00,   15.00),
        m if m.contains("claude-3-haiku")            => (0.25,   1.25),

        // ── Google Gemini 3.1 (before 2.5) ───────────────────────────────
        m if m.contains("gemini-3.1-pro")            => (2.00,   12.00),
        m if m.contains("gemini-3.1-flash")          => (0.10,    0.40),
        m if m.contains("gemini-3.1")                => (2.00,   12.00),  // generic 3.1

        // ── Google Gemini 3.0 (before 2.5) ───────────────────────────────
        m if m.contains("gemini-3-pro")              => (2.00,   12.00),
        m if m.contains("gemini-3")                  => (2.00,   12.00),

        // ── Google Gemini 2.5 ─────────────────────────────────────────────
        m if m.contains("gemini-2.5-pro")            => (1.25,   10.00),
        m if m.contains("gemini-2.5-flash")          => (0.075,  0.30),

        // ── Google Gemini 2.0 (flash-lite before flash) ───────────────────
        m if m.contains("gemini-2.0-flash-lite")     => (0.075,  0.30),
        m if m.contains("gemini-2.0-flash")          => (0.10,   0.40),
        m if m.contains("gemini-2.0")                => (0.10,   0.40),

        // ── Google Gemini 1.5 (8b before flash before pro) ────────────────
        m if m.contains("gemini-1.5-flash-8b")       => (0.0375, 0.15),
        m if m.contains("gemini-1.5-pro")            => (1.25,   5.00),
        m if m.contains("gemini-1.5-flash")          => (0.075,  0.30),

        // ── xAI Grok (specific versions before generic) ───────────────────
        m if m.contains("grok-3")                    => (3.00,   15.00),
        m if m.contains("grok-2")                    => (2.00,   10.00),
        m if m.contains("grok")                      => (5.00,   15.00),

        // ── Meta Llama 4 ──────────────────────────────────────────────────
        // Bedrock uses "llama4-maverick" (no hyphen between llama and 4).
        m if m.contains("llama-4-maverick") || m.contains("llama4-maverick") => (0.24, 0.77),
        m if m.contains("llama-4-scout")    || m.contains("llama4-scout")    => (0.20, 0.60),

        // ── Meta Llama 3.3 ────────────────────────────────────────────────
        // Bedrock IDs: meta.llama3-3-70b-instruct-v1:0
        m if m.contains("llama-3.3-70b") || m.contains("llama3-3-70b")      => (0.59, 0.79),

        // ── Meta Llama 3.2 (larger sizes first) ───────────────────────────
        // Bedrock IDs: meta.llama3-2-{90b,11b,3b,1b}-instruct-v1:0
        m if m.contains("llama-3.2-90b") || m.contains("llama3-2-90b")      => (0.90, 0.90),
        m if m.contains("llama-3.2-11b") || m.contains("llama3-2-11b")      => (0.18, 0.18),
        m if m.contains("llama-3.2-3b")  || m.contains("llama3-2-3b")       => (0.06, 0.06),
        m if m.contains("llama-3.2-1b")  || m.contains("llama3-2-1b")       => (0.04, 0.04),

        // ── Meta Llama 3.1 ────────────────────────────────────────────────
        // Bedrock IDs: meta.llama3-1-{405b,70b,8b}-instruct-v1:0
        m if m.contains("llama-3.1-405b") || m.contains("llama3-1-405b")    => (3.00, 3.00),
        m if m.contains("llama-3.1-70b")  || m.contains("llama3-1-70b")     => (0.52, 0.75),
        m if m.contains("llama-3.1-8b")   || m.contains("llama3-1-8b")      => (0.18, 0.18),

        // ── Mistral / Mixtral / Codestral / Pixtral ───────────────────────
        m if m.contains("mistral-large")             => (2.00,   6.00),
        m if m.contains("mistral-medium")            => (2.75,   8.10),
        m if m.contains("mistral-small")             => (0.20,   0.60),
        m if m.contains("codestral")                 => (0.30,   0.90),
        m if m.contains("pixtral")                   => (2.00,   6.00),
        m if m.contains("mixtral")                   => (0.24,   0.24),

        // ── DeepSeek (R1 reasoning before generic deepseek) ───────────────
        m if m.contains("deepseek-r1")               => (0.55,   2.19),
        m if m.contains("deepseek")                  => (0.14,   0.28),

        // ── Cohere Command (plus before base) ─────────────────────────────
        m if m.contains("command-r-plus")            => (2.50,   10.00),
        m if m.contains("command-r")                 => (0.50,   1.50),

        // ── Perplexity Sonar (pro before base) ────────────────────────────
        m if m.contains("sonar-pro")                 => (3.00,   15.00),
        m if m.contains("sonar")                     => (1.00,   5.00),

        // ── Amazon Nova (premier before pro, lite, micro) ─────────────────
        // Bedrock model IDs: amazon.nova-{premier,pro,lite,micro}-v1:0
        m if m.contains("nova-premier")              => (2.50,   12.50),
        m if m.contains("nova-pro")                  => (0.80,    3.20),
        m if m.contains("nova-lite")                 => (0.06,    0.24),
        m if m.contains("nova-micro")                => (0.035,   0.14),

        // ── AI21 Jamba (large before mini) ────────────────────────────────
        // Model IDs use varied formats: "jamba-large-1.7", "jamba-1-5-large",
        // "ai21.jamba-1-5-large-v1:0".  Use compound guards to match both.
        m if m.contains("jamba") && (m.contains("-large") || m.contains("1.7")) => (2.00, 8.00),
        m if m.contains("jamba") && m.contains("-mini")  => (0.20,    0.40),
        m if m.contains("jamba")                          => (2.00,    8.00),  // generic

        // ── Alibaba Qwen (Dashscope / OpenRouter: qwen/...) ───────────────
        // QwQ reasoning models (32b-preview pricier than current qwq-plus)
        m if m.contains("qwq-32b-preview")           => (1.60,    6.40),
        m if m.contains("qwq")                       => (0.20,    0.60),  // qwq-plus, qwq-32b

        // Qwen3 coder series (coder-next before coder; qwen3 is substring of both)
        m if m.contains("qwen3-coder-next")          => (0.07,    0.30),
        m if m.contains("qwen3-coder")               => (1.00,    5.00),  // coder-plus / coder

        // Qwen3.5 before Qwen3 ("qwen3" is a substring of "qwen3.5")
        m if m.contains("qwen3.5")                   => (0.90,    3.60),

        // Qwen3 Max before generic Qwen3 ("qwen3" is a substring of "qwen3-max")
        m if m.contains("qwen3-max")                 => (1.20,    6.00),
        m if m.contains("qwen3")                     => (0.40,    1.20),  // qwen3-plus / turbo

        // Qwen2.5 sized variants (specific sizes before generic)
        m if m.contains("qwen2.5-72b")               => (0.15,    0.40),
        m if m.contains("qwen2.5-7b")                => (0.10,    0.30),
        m if m.contains("qwen2.5")                   => (0.35,    0.75),

        // Legacy Qwen series (max > plus > turbo > generic)
        m if m.contains("qwen-max")                  => (1.60,    6.40),
        m if m.contains("qwen-plus")                 => (0.30,    1.20),
        m if m.contains("qwen-turbo")                => (0.05,    0.20),
        m if m.contains("qwen")                      => (0.40,    1.20),  // generic fallback

        // ── Kimi / Moonshot AI ────────────────────────────────────────────
        m if m.contains("kimi-k2.5")                 => (0.60,    3.00),
        m if m.contains("kimi")                      => (0.60,    3.00),  // generic kimi

        // ── Zhipu AI GLM (api.z.ai) ───────────────────────────────────────
        // Ordering: specific version+variant before version, before generic glm-4.
        // "glm-4.5" is a substring of "glm-4.5-x" and "glm-4.5-flash".
        // "glm-4" is a substring of "glm-4.5", "glm-4.7", etc.
        m if m.contains("glm-5")                     => (1.00,    3.20),
        m if m.contains("glm-4.7-x")                 => (4.50,    8.90),  // GLM-4.7-X premium
        m if m.contains("glm-4.7-flash")             => (0.05,    0.10),
        m if m.contains("glm-4.7")                   => (0.60,    2.20),
        m if m.contains("glm-4.5-x")                 => (4.50,    8.90),  // GLM-4.5-X premium
        m if m.contains("glm-4.5-flash")             => (0.05,    0.10),
        m if m.contains("glm-4.5")                   => (0.55,    2.00),
        m if m.contains("glm-4")                     => (0.55,    2.00),  // GLM-4 generic

        // ── Google Gemma ──────────────────────────────────────────────────
        m if m.contains("gemma")                     => (0.10,   0.10),

        // ── Fallback ──────────────────────────────────────────────────────
        _ => (1.00, 3.00), // unknown model — cost estimate will be inaccurate
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bytes::Bytes;
    use crate::store::{CallRecord, now_iso};

    fn make_test_record(status_code: u16, error: Option<&str>) -> CallRecord {
        CallRecord {
            id: "test-id".to_string(),
            timestamp: now_iso(),
            provider: "openai".to_string(),
            model: "gpt-4o".to_string(),
            endpoint: "/v1/chat/completions".to_string(),
            status_code,
            latency_ms: 100,
            ttft_ms: None,
            input_tokens: Some(100),
            output_tokens: Some(50),
            cost_usd: Some(0.001),
            request_body: None,
            response_body: None,
            error: error.map(String::from),
            provider_request_id: None,
            trace_id: None,
            parent_id: None,
            prompt_hash: None,
        }
    }

    // -------------------------------------------------------------------------
    // classify_error
    // -------------------------------------------------------------------------

    #[test]
    fn classify_429_is_rate_limit() {
        let r = make_test_record(429, Some("rate limit exceeded"));
        assert_eq!(classify_error(&r), Some("rate_limit"));
    }

    #[test]
    fn classify_401_is_auth() {
        let r = make_test_record(401, None);
        assert_eq!(classify_error(&r), Some("auth"));
    }

    #[test]
    fn classify_403_is_auth() {
        let r = make_test_record(403, None);
        assert_eq!(classify_error(&r), Some("auth"));
    }

    #[test]
    fn classify_503_is_upstream_5xx() {
        let r = make_test_record(503, None);
        assert_eq!(classify_error(&r), Some("upstream_5xx"));
    }

    #[test]
    fn classify_timeout_error_string() {
        let r = make_test_record(0, Some("request timed out after 30s"));
        assert_eq!(classify_error(&r), Some("timeout"));
    }

    #[test]
    fn classify_connection_error_string() {
        let r = make_test_record(0, Some("connection refused"));
        assert_eq!(classify_error(&r), Some("connection"));
    }

    #[test]
    fn classify_none_for_200() {
        let r = make_test_record(200, None);
        assert_eq!(classify_error(&r), None);
    }

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
        // Claude Opus 4.x (4.5, 4.6, 4.6-thinking) is $5.00/$25.00 per M tokens.
        // Claude 3 Opus was $15/$75 — that era is matched by "claude-3-opus" arm.
        let cost = estimate_cost("claude-opus-4-5", 2_000, 1_000);
        let expected = (2_000.0 * 5.00 + 1_000.0 * 25.00) / 1_000_000.0;
        assert!((cost - expected).abs() < 1e-9);
    }

    #[test]
    fn estimate_cost_claude_opus_4_6_thinking() {
        // Thinking mode is same price as base opus 4.x: $5/$25.
        let cost46 = estimate_cost("claude-opus-4-6-20260205", 1_000_000, 1_000_000);
        let cost46t = estimate_cost("claude-opus-4-6-thinking-20260205", 1_000_000, 1_000_000);
        assert!((cost46 - cost46t).abs() < 1e-6, "thinking mode should cost same as base");
        // Both should be $5 + $25 = $30 for 1M/1M tokens
        assert!((cost46 - 30.0).abs() < 1e-6);
    }

    #[test]
    fn claude_opus_4_cheaper_than_claude_3_opus() {
        // Claude 3 Opus was $15/$75; Claude 4 Opus is $5/$25.
        let c4 = estimate_cost("claude-opus-4-5", 1_000_000, 1_000_000);
        let c3 = estimate_cost("claude-3-opus-20240229", 1_000_000, 1_000_000);
        assert!(c4 < c3, "claude-opus-4 should be cheaper than claude-3-opus");
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
    // estimate_cost_from_body — cache-aware pricing
    // -------------------------------------------------------------------------

    #[test]
    fn estimate_cost_from_body_anthropic_cache_read_is_cheaper() {
        // claude-3-5-sonnet: $3.00/M input, $15.00/M output.
        // Cache read = 10% of input price = $0.30/M.
        // 1000 input (900 cached read, 100 regular), 200 output.
        let body = r#"{"usage":{"input_tokens":1000,"cache_read_input_tokens":900,"output_tokens":200}}"#;
        let with_cache = estimate_cost_from_body("claude-3-5-sonnet-20241022", body, 1000, 200);
        let without_cache = estimate_cost("claude-3-5-sonnet-20241022", 1000, 200);
        assert!(with_cache < without_cache, "cache read should be cheaper: {with_cache} < {without_cache}");

        // Expected: 100 regular * $3/M + 900 cached * $0.30/M + 200 * $15/M
        let expected = (100.0 * 3.00 + 900.0 * 0.30 + 200.0 * 15.00) / 1_000_000.0;
        assert!((with_cache - expected).abs() < 1e-9, "with_cache={with_cache} expected={expected}");
    }

    #[test]
    fn estimate_cost_from_body_anthropic_cache_write_is_pricier() {
        // Cache write = 1.25x normal input price.
        let body = r#"{"usage":{"input_tokens":1000,"cache_creation_input_tokens":800,"output_tokens":0}}"#;
        let with_write = estimate_cost_from_body("claude-3-5-sonnet-20241022", body, 1000, 0);
        // Expected: 200 regular * $3/M + 800 write * $3.75/M
        let expected = (200.0 * 3.00 + 800.0 * 3.75) / 1_000_000.0;
        assert!((with_write - expected).abs() < 1e-9, "with_write={with_write} expected={expected}");
    }

    #[test]
    fn estimate_cost_from_body_openai_cached_tokens_50pct_discount() {
        // gpt-4o: $2.50/M input. Cached reads are 50% = $1.25/M.
        // 1000 input (800 cached), 0 output.
        let body = r#"{"usage":{"prompt_tokens":1000,"prompt_tokens_details":{"cached_tokens":800},"completion_tokens":0}}"#;
        let with_cache = estimate_cost_from_body("gpt-4o", body, 1000, 0);
        // Expected: 200 regular * $2.50/M + 800 cached * $1.25/M
        let expected = (200.0 * 2.50 + 800.0 * 1.25) / 1_000_000.0;
        assert!((with_cache - expected).abs() < 1e-9, "with_cache={with_cache} expected={expected}");
    }

    #[test]
    fn estimate_cost_from_body_no_cache_falls_back_to_normal() {
        let body = r#"{"usage":{"prompt_tokens":1000,"completion_tokens":200}}"#;
        let from_body = estimate_cost_from_body("gpt-4o", body, 1000, 200);
        let normal = estimate_cost("gpt-4o", 1000, 200);
        assert!((from_body - normal).abs() < 1e-9);
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

    // -------------------------------------------------------------------------
    // redact_json_fields
    // -------------------------------------------------------------------------

    #[test]
    fn redact_removes_target_key() {
        let body = r#"{"model":"gpt-4o","messages":[{"role":"user","content":"Hello"}]}"#;
        let fields = vec!["messages".to_string()];
        let result = redact_json_fields(body, &fields);
        let v: serde_json::Value = serde_json::from_str(&result).unwrap();
        assert_eq!(v["messages"].as_str(), Some("[REDACTED]"));
        assert_eq!(v["model"].as_str(), Some("gpt-4o")); // other fields unchanged
    }

    #[test]
    fn redact_ignores_unknown_key() {
        let body = r#"{"model":"gpt-4o","messages":[]}"#;
        let fields = vec!["system_prompt".to_string()];
        let result = redact_json_fields(body, &fields);
        // Body is returned but field absent — model and messages unchanged
        let v: serde_json::Value = serde_json::from_str(&result).unwrap();
        assert_eq!(v["model"].as_str(), Some("gpt-4o"));
        assert!(v["messages"].is_array());
    }

    #[test]
    fn redact_multiple_fields_all_removed() {
        let body = r#"{"model":"gpt-4o","messages":[],"system_prompt":"You are a helpful assistant"}"#;
        let fields = vec!["messages".to_string(), "system_prompt".to_string()];
        let result = redact_json_fields(body, &fields);
        let v: serde_json::Value = serde_json::from_str(&result).unwrap();
        assert_eq!(v["messages"].as_str(), Some("[REDACTED]"));
        assert_eq!(v["system_prompt"].as_str(), Some("[REDACTED]"));
        assert_eq!(v["model"].as_str(), Some("gpt-4o"));
    }

    #[test]
    fn redact_invalid_json_passthrough() {
        let body = "this is not json at all";
        let fields = vec!["messages".to_string()];
        let result = redact_json_fields(body, &fields);
        assert_eq!(result, body, "invalid JSON should be returned verbatim");
    }

    #[test]
    fn redact_empty_fields_passthrough() {
        let body = r#"{"model":"gpt-4o","messages":[]}"#;
        let result = redact_json_fields(body, &[]);
        assert_eq!(result, body);
    }

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

    #[test]
    fn detect_provider_deepseek() {
        assert_eq!(detect_provider("https://api.deepseek.com"), "deepseek");
        assert_eq!(detect_provider("https://api.deepseek.com/v1"), "deepseek");
    }

    #[test]
    fn detect_provider_azure_openai() {
        assert_eq!(
            detect_provider("https://myresource.openai.azure.com/openai/deployments/gpt-4o"),
            "azure-openai"
        );
        // azure-openai must take priority over plain openai (more specific match first)
        assert_eq!(detect_provider("https://foo.azure.com/openai/models"), "azure-openai");
    }

    #[test]
    fn detect_provider_ollama_by_name() {
        assert_eq!(detect_provider("http://ollama:11434"), "ollama");
        assert_eq!(detect_provider("http://localhost:11434"), "ollama");
        assert_eq!(detect_provider("http://127.0.0.1:11434"), "ollama");
    }

    #[test]
    fn detect_provider_openrouter() {
        assert_eq!(detect_provider("https://openrouter.ai/api/v1"), "openrouter");
    }

    #[test]
    fn detect_provider_xai() {
        assert_eq!(detect_provider("https://api.x.ai/v1"), "xai");
    }

    #[test]
    fn detect_provider_perplexity() {
        assert_eq!(detect_provider("https://api.perplexity.ai"), "perplexity");
        assert_eq!(detect_provider("https://api.perplexity.ai/chat/completions"), "perplexity");
    }

    #[test]
    fn detect_provider_bedrock() {
        assert_eq!(
            detect_provider("https://bedrock-runtime.us-east-1.amazonaws.com"),
            "bedrock"
        );
        assert_eq!(
            detect_provider("https://bedrock-runtime.eu-west-1.amazonaws.com/model/anthropic.claude-3"),
            "bedrock"
        );
    }

    #[test]
    fn detect_provider_fireworks() {
        assert_eq!(detect_provider("https://api.fireworks.ai/inference/v1"), "fireworks");
    }

    #[test]
    fn detect_provider_nvidia() {
        assert_eq!(detect_provider("https://integrate.api.nvidia.com/v1"), "nvidia");
        assert_eq!(detect_provider("https://api.nvidia.com/v1"), "nvidia");
    }

    // ── New model pricing tests ───────────────────────────────────────────────

    #[test]
    fn model_prices_gpt41_cheaper_than_gpt4_legacy() {
        let (inp41, out41) = model_prices("gpt-4.1");
        let (inp4, _) = model_prices("gpt-4");
        assert!(inp41 < inp4, "gpt-4.1 should be cheaper input than legacy gpt-4");
        assert_eq!((inp41, out41), (2.00, 8.00));
    }

    #[test]
    fn model_prices_gpt41_mini_cheaper_than_gpt41() {
        // gpt-4.1-mini has its own arm at $0.40/$1.60 — cheaper than full gpt-4.1.
        let (inp_mini, out_mini) = model_prices("gpt-4.1-mini");
        let (inp_full, _) = model_prices("gpt-4.1");
        assert_eq!((inp_mini, out_mini), (0.40, 1.60));
        assert!(inp_mini < inp_full, "gpt-4.1-mini should be cheaper than gpt-4.1");
    }

    #[test]
    fn model_prices_claude_37_sonnet() {
        assert_eq!(model_prices("claude-3-7-sonnet-20250219"), (3.00, 15.00));
    }

    #[test]
    fn model_prices_gemini_25_pro() {
        assert_eq!(model_prices("gemini-2.5-pro"), (1.25, 10.00));
    }

    #[test]
    fn model_prices_gemini_20_flash_lite() {
        let (inp_lite, _) = model_prices("gemini-2.0-flash-lite");
        let (inp_full, _) = model_prices("gemini-2.0-flash");
        assert!(inp_lite <= inp_full, "flash-lite should be <= flash price");
    }

    #[test]
    fn model_prices_gemini_15_flash_8b() {
        let (inp_8b, _) = model_prices("gemini-1.5-flash-8b");
        let (inp_flash, _) = model_prices("gemini-1.5-flash");
        assert!(inp_8b < inp_flash, "flash-8b should be cheaper than flash");
    }

    #[test]
    fn model_prices_llama_4_scout() {
        assert_eq!(model_prices("llama-4-scout-17b-16e-instruct"), (0.20, 0.60));
    }

    #[test]
    fn model_prices_llama_32_sizes() {
        assert_eq!(model_prices("llama-3.2-90b-vision-instruct"), (0.90, 0.90));
        assert_eq!(model_prices("llama-3.2-11b-vision-instruct"), (0.18, 0.18));
        assert_eq!(model_prices("llama-3.2-3b-instruct"), (0.06, 0.06));
        assert_eq!(model_prices("llama-3.2-1b-instruct"), (0.04, 0.04));
    }

    #[test]
    fn model_prices_deepseek_r1_more_expensive_than_v3() {
        let (inp_r1, out_r1) = model_prices("deepseek-r1");
        let (inp_v3, out_v3) = model_prices("deepseek-v3");
        assert!(inp_r1 > inp_v3, "DeepSeek R1 reasoning should cost more input than V3");
        assert!(out_r1 > out_v3, "DeepSeek R1 reasoning should cost more output than V3");
    }

    #[test]
    fn model_prices_deepseek_r1() {
        assert_eq!(model_prices("deepseek-r1"), (0.55, 2.19));
    }

    #[test]
    fn model_prices_grok_versions() {
        assert_eq!(model_prices("grok-3"), (3.00, 15.00));
        assert_eq!(model_prices("grok-2-1212"), (2.00, 10.00));
    }

    #[test]
    fn model_prices_command_r_plus_more_expensive_than_command_r() {
        let (inp_plus, _) = model_prices("command-r-plus");
        let (inp_base, _) = model_prices("command-r");
        assert!(inp_plus > inp_base, "command-r-plus should cost more than command-r");
    }

    #[test]
    fn model_prices_sonar_pro_more_expensive_than_sonar() {
        let (inp_pro, _) = model_prices("sonar-pro");
        let (inp_base, _) = model_prices("sonar");
        assert!(inp_pro > inp_base, "sonar-pro should cost more than sonar");
    }

    #[test]
    fn model_prices_codestral_and_pixtral() {
        let (inp_code, _) = model_prices("codestral-latest");
        let (inp_pix, _) = model_prices("pixtral-large-2411");
        assert_eq!(inp_code, 0.30);
        assert_eq!(inp_pix, 2.00);
    }

    #[test]
    fn model_prices_mistral_medium() {
        assert_eq!(model_prices("mistral-medium-2312"), (2.75, 8.10));
    }

    #[test]
    fn is_known_model_grok_is_known() {
        assert!(is_known_model("grok-3"));
        assert!(is_known_model("grok-2-1212"));
    }

    #[test]
    fn is_known_model_deepseek_r1_is_known() {
        assert!(is_known_model("deepseek-r1"));
    }

    #[test]
    fn is_known_model_llama_4_scout_is_known() {
        assert!(is_known_model("llama-4-scout-17b-16e-instruct"));
    }

    #[test]
    fn is_known_model_command_r_plus_is_known() {
        assert!(is_known_model("command-r-plus"));
        assert!(is_known_model("command-r"));
    }

    #[test]
    fn is_known_model_gemini_25_is_known() {
        assert!(is_known_model("gemini-2.5-pro"));
    }

    #[test]
    fn is_known_model_claude_37_is_known() {
        assert!(is_known_model("claude-3-7-sonnet-20250219"));
    }

    // ── GPT-5 series ─────────────────────────────────────────────────────

    #[test]
    fn model_prices_gpt52_more_expensive_than_gpt51() {
        let (i52, o52) = model_prices("gpt-5.2");
        let (i51, o51) = model_prices("gpt-5.1");
        assert!(i52 > i51, "gpt-5.2 input should cost more than gpt-5.1");
        assert!(o52 > o51, "gpt-5.2 output should cost more than gpt-5.1");
    }

    #[test]
    fn model_prices_gpt52_correct() {
        assert_eq!(model_prices("gpt-5.2"), (1.75, 14.00));
        assert_eq!(model_prices("gpt-5.2-pro"), (1.75, 14.00));
    }

    #[test]
    fn model_prices_gpt51_correct() {
        assert_eq!(model_prices("gpt-5.1"), (1.25, 10.00));
    }

    #[test]
    fn model_prices_gpt51_codex_cheaper_than_gpt51() {
        let (ci, _) = model_prices("gpt-5.1-codex-mini");
        let (i, _) = model_prices("gpt-5.1");
        assert!(ci < i, "gpt-5.1-codex-mini should be cheaper input than gpt-5.1");
    }

    #[test]
    fn model_prices_codex_mini_latest() {
        assert_eq!(model_prices("codex-mini-latest"), (1.50, 6.00));
    }

    #[test]
    fn model_prices_gpt5_does_not_match_gpt51() {
        // "gpt-5" arm is separate from "gpt-5.1"; both have same price so
        // what matters is that "gpt-5.1" model names are not forced through
        // the fallback (i.e., they are matched by some arm, not the _ default).
        let (i5, _)  = model_prices("gpt-5");
        let (i51, _) = model_prices("gpt-5.1");
        // Both should have known prices, not the $1.00 fallback
        assert!(i5  >= 1.20, "gpt-5 should have a known price > fallback");
        assert!(i51 >= 1.20, "gpt-5.1 should have a known price > fallback");
    }

    #[test]
    fn is_known_model_gpt52_is_known() {
        assert!(is_known_model("gpt-5.2"));
    }

    #[test]
    fn is_known_model_gpt51_is_known() {
        assert!(is_known_model("gpt-5.1"));
    }

    // ── Gemini 3.x ───────────────────────────────────────────────────────

    #[test]
    fn model_prices_gemini_31_pro() {
        assert_eq!(model_prices("gemini-3.1-pro-preview"), (2.00, 12.00));
    }

    #[test]
    fn model_prices_gemini_31_more_expensive_than_25_pro() {
        let (i31, _) = model_prices("gemini-3.1-pro");
        let (i25, _) = model_prices("gemini-2.5-pro");
        assert!(i31 > i25, "gemini-3.1-pro should cost more than gemini-2.5-pro");
    }

    #[test]
    fn is_known_model_gemini_31_is_known() {
        assert!(is_known_model("gemini-3.1-pro-preview"));
    }

    // ── Kimi / Moonshot ──────────────────────────────────────────────────

    #[test]
    fn model_prices_kimi_k25() {
        assert_eq!(model_prices("kimi-k2.5"), (0.60, 3.00));
        assert_eq!(model_prices("kimi-k2.5-instant"), (0.60, 3.00));
    }

    #[test]
    fn model_prices_kimi_k25_cheaper_than_claude_sonnet() {
        let (ki, _) = model_prices("kimi-k2.5");
        let (si, _) = model_prices("claude-sonnet-4-6");
        assert!(ki < si, "kimi-k2.5 should be cheaper than claude-sonnet-4");
    }

    #[test]
    fn detect_provider_moonshot() {
        assert_eq!(detect_provider("https://platform.moonshot.ai/v1"), "moonshot");
        assert_eq!(detect_provider("https://api.moonshot.cn/v1"), "moonshot");
    }

    #[test]
    fn is_known_model_kimi_k25_is_known() {
        assert!(is_known_model("kimi-k2.5-instant"));
    }

    // ── Claude Opus 4 price correction ───────────────────────────────────

    #[test]
    fn model_prices_claude_opus_4_is_5_per_million() {
        assert_eq!(model_prices("claude-opus-4-6"), (5.00, 25.00));
        assert_eq!(model_prices("claude-opus-4-5"), (5.00, 25.00));
        assert_eq!(model_prices("claude-opus-4-6-thinking-20260205"), (5.00, 25.00));
    }

    #[test]
    fn model_prices_claude_3_opus_is_15_per_million() {
        // The $15/$75 price is for Claude 3 Opus (not Claude 4).
        assert_eq!(model_prices("claude-3-opus-20240229"), (15.00, 75.00));
    }

    // ── Explicit Claude 4.5 / 4.6 version arms ───────────────────────────

    #[test]
    fn model_prices_claude_opus_46_explicit() {
        assert_eq!(model_prices("claude-opus-4-6"), (5.00, 25.00));
        assert_eq!(model_prices("claude-opus-4-6-20260205"), (5.00, 25.00));
        assert_eq!(model_prices("claude-opus-4-6-thinking-20260205"), (5.00, 25.00));
    }

    #[test]
    fn model_prices_claude_opus_45_explicit() {
        assert_eq!(model_prices("claude-opus-4-5"), (5.00, 25.00));
        assert_eq!(model_prices("claude-opus-4-5-20251101"), (5.00, 25.00));
    }

    #[test]
    fn model_prices_claude_sonnet_46_explicit() {
        assert_eq!(model_prices("claude-sonnet-4-6"), (3.00, 15.00));
        assert_eq!(model_prices("claude-sonnet-4-6-20260205"), (3.00, 15.00));
    }

    #[test]
    fn model_prices_claude_sonnet_45_explicit() {
        assert_eq!(model_prices("claude-sonnet-4-5"), (3.00, 15.00));
        assert_eq!(model_prices("claude-sonnet-4-5-20250825"), (3.00, 15.00));
    }

    #[test]
    fn is_known_model_claude_opus_46() {
        assert!(is_known_model("claude-opus-4-6"));
        assert!(is_known_model("claude-opus-4-6-thinking-20260205"));
    }

    #[test]
    fn is_known_model_claude_sonnet_46() {
        assert!(is_known_model("claude-sonnet-4-6"));
    }

    // ── Zhipu AI GLM / z.ai ──────────────────────────────────────────────

    #[test]
    fn detect_provider_zhipu() {
        assert_eq!(detect_provider("https://api.z.ai/v1"), "zhipu");
        assert_eq!(detect_provider("https://open.bigmodel.cn/api/paas/v4"), "zhipu");
        assert_eq!(detect_provider("https://api.zhipu.ai/v1"), "zhipu");
    }

    #[test]
    fn model_prices_glm5() {
        assert_eq!(model_prices("glm-5"), (1.00, 3.20));
    }

    #[test]
    fn model_prices_glm47_variants() {
        assert_eq!(model_prices("glm-4.7"), (0.60, 2.20));
        assert_eq!(model_prices("glm-4.7-flash"), (0.05, 0.10));
        assert_eq!(model_prices("glm-4.7-x"), (4.50, 8.90));
    }

    #[test]
    fn model_prices_glm45_variants() {
        assert_eq!(model_prices("glm-4.5"), (0.55, 2.00));
        assert_eq!(model_prices("glm-4.5-flash"), (0.05, 0.10));
        assert_eq!(model_prices("glm-4.5-x"), (4.50, 8.90));
    }

    #[test]
    fn model_prices_glm4_generic() {
        // glm-4 generic catches anything not matched by version-specific arms
        assert_eq!(model_prices("glm-4"), (0.55, 2.00));
    }

    #[test]
    fn model_prices_glm47_not_matched_by_glm4_generic() {
        // glm-4.7 must match its own arm, not the glm-4 catch-all
        let (_i47, o47) = model_prices("glm-4.7");
        let (_i4,  o4)  = model_prices("glm-4-v");  // some glm-4 generic model
        // glm-4.7 has higher output price than base glm-4
        assert!(o47 > o4, "glm-4.7 output price should differ from glm-4 generic");
    }

    #[test]
    fn model_prices_glm5_more_expensive_than_glm45() {
        let (i5, _) = model_prices("glm-5");
        let (i45, _) = model_prices("glm-4.5");
        assert!(i5 > i45, "glm-5 should cost more per input token than glm-4.5");
    }

    #[test]
    fn is_known_model_glm5_is_known() {
        assert!(is_known_model("glm-5"));
    }

    #[test]
    fn is_known_model_glm47_is_known() {
        assert!(is_known_model("glm-4.7"));
    }

    // ── Amazon Nova ───────────────────────────────────────────────────────

    #[test]
    fn model_prices_nova_tiers() {
        assert_eq!(model_prices("amazon.nova-premier-v1:0"), (2.50, 12.50));
        assert_eq!(model_prices("amazon.nova-pro-v1:0"),     (0.80,  3.20));
        assert_eq!(model_prices("amazon.nova-lite-v1:0"),    (0.06,  0.24));
        assert_eq!(model_prices("amazon.nova-micro-v1:0"),   (0.035, 0.14));
    }

    #[test]
    fn model_prices_nova_ordered_cheapest_to_priciest() {
        let (micro, _) = model_prices("nova-micro");
        let (lite, _)  = model_prices("nova-lite");
        let (pro, _)   = model_prices("nova-pro");
        let (prem, _)  = model_prices("nova-premier");
        assert!(micro < lite && lite < pro && pro < prem);
    }

    #[test]
    fn is_known_model_nova_is_known() {
        assert!(is_known_model("amazon.nova-pro-v1:0"));
        assert!(is_known_model("nova-premier"));
    }

    // ── AI21 Jamba ────────────────────────────────────────────────────────

    #[test]
    fn detect_provider_ai21() {
        assert_eq!(detect_provider("https://api.ai21.com/studio/v1"), "ai21");
    }

    #[test]
    fn model_prices_jamba_large_and_mini() {
        assert_eq!(model_prices("jamba-large-1.7"),   (2.00, 8.00));
        assert_eq!(model_prices("jamba-1-5-large"),   (2.00, 8.00));
        assert_eq!(model_prices("jamba-1-5-mini"),    (0.20, 0.40));
        // via Bedrock: ai21.jamba-1-5-large-v1:0
        assert_eq!(model_prices("ai21.jamba-1-5-large-v1:0"), (2.00, 8.00));
    }

    #[test]
    fn is_known_model_jamba_is_known() {
        assert!(is_known_model("jamba-large-1.7"));
        assert!(is_known_model("ai21.jamba-1-5-mini-v1:0"));
    }

    // ── Qwen / Alibaba ────────────────────────────────────────────────────

    #[test]
    fn detect_provider_qwen_dashscope() {
        assert_eq!(detect_provider("https://dashscope.aliyuncs.com/compatible-mode/v1"), "qwen");
        assert_eq!(detect_provider("https://api.dashscope.ai/v1"), "qwen");
    }

    #[test]
    fn model_prices_qwq_series() {
        assert_eq!(model_prices("qwq-plus"),           (0.20, 0.60));
        assert_eq!(model_prices("qwq-32b"),            (0.20, 0.60));
        assert_eq!(model_prices("qwq-32b-preview"),    (1.60, 6.40));
    }

    #[test]
    fn model_prices_qwq_preview_pricier_than_qwq_plus() {
        let (i_preview, _) = model_prices("qwq-32b-preview");
        let (i_plus, _)    = model_prices("qwq-plus");
        assert!(i_preview > i_plus);
    }

    #[test]
    fn model_prices_qwen3_coder_next_cheaper_than_coder() {
        let (i_next, _) = model_prices("qwen3-coder-next");
        let (i_plus, _) = model_prices("qwen3-coder-plus");
        assert!(i_next < i_plus);
    }

    #[test]
    fn model_prices_qwen3_max_correct() {
        assert_eq!(model_prices("qwen3-max"),                    (1.20, 6.00));
        assert_eq!(model_prices("qwen3-max-2025-09-23"),         (1.20, 6.00));
        // OpenRouter format: qwen/qwen3-max
        assert_eq!(model_prices("qwen/qwen3-max"),               (1.20, 6.00));
    }

    #[test]
    fn model_prices_qwen35_plus() {
        assert_eq!(model_prices("qwen3.5-plus"),                 (0.90, 3.60));
        assert_eq!(model_prices("qwen3.5-plus-2026-02-15"),      (0.90, 3.60));
    }

    #[test]
    fn model_prices_qwen25_72b() {
        assert_eq!(model_prices("qwen2.5-72b-instruct"),         (0.15, 0.40));
        assert_eq!(model_prices("qwen/qwen2.5-72b-instruct"),    (0.15, 0.40));
    }

    #[test]
    fn model_prices_qwen_generic_ladder() {
        let (i_max, _)    = model_prices("qwen-max");
        let (i_plus, _)   = model_prices("qwen-plus");
        let (i_turbo, _)  = model_prices("qwen-turbo");
        assert!(i_max > i_plus && i_plus > i_turbo);
    }

    #[test]
    fn is_known_model_qwen3_is_known() {
        assert!(is_known_model("qwen3-max"));
        assert!(is_known_model("qwq-32b"));
        assert!(is_known_model("qwen3.5-plus-2026-02-15"));
    }

    // ── Bedrock Llama ID format (no-hyphen aliases) ───────────────────────

    #[test]
    fn model_prices_bedrock_llama4_ids() {
        // Bedrock: us.meta.llama4-maverick-17b-instruct-v1:0
        assert_eq!(model_prices("us.meta.llama4-maverick-17b-instruct-v1:0"), (0.24, 0.77));
        assert_eq!(model_prices("us.meta.llama4-scout-17b-instruct-v1:0"),    (0.20, 0.60));
    }

    #[test]
    fn model_prices_bedrock_llama31_ids() {
        // Bedrock: meta.llama3-1-70b-instruct-v1:0
        assert_eq!(model_prices("meta.llama3-1-70b-instruct-v1:0"),  (0.52, 0.75));
        assert_eq!(model_prices("meta.llama3-1-8b-instruct-v1:0"),   (0.18, 0.18));
        assert_eq!(model_prices("meta.llama3-1-405b-instruct-v1:0"), (3.00, 3.00));
    }

    #[test]
    fn model_prices_bedrock_llama33_id() {
        assert_eq!(model_prices("meta.llama3-3-70b-instruct-v1:0"), (0.59, 0.79));
    }

    #[test]
    fn model_prices_bedrock_llama32_ids() {
        assert_eq!(model_prices("meta.llama3-2-11b-instruct-v1:0"), (0.18, 0.18));
        assert_eq!(model_prices("meta.llama3-2-1b-instruct-v1:0"),  (0.04, 0.04));
    }

    // -------------------------------------------------------------------------
    // extract_prompt_hash
    // -------------------------------------------------------------------------

    #[test]
    fn prompt_hash_extracted_from_openai_messages() {
        let body = r#"{"model":"gpt-4o","messages":[{"role":"system","content":"You are a helpful assistant"},{"role":"user","content":"Hello"}]}"#;
        let hash = extract_prompt_hash(body);
        assert!(hash.is_some(), "should extract hash from OpenAI messages");
        assert_eq!(hash.as_deref().unwrap().len(), 16, "FNV-64 hex is 16 chars");
        // Idempotent: same prompt always produces same hash.
        assert_eq!(extract_prompt_hash(body), hash);
    }

    #[test]
    fn prompt_hash_extracted_from_anthropic_system() {
        let body = r#"{"model":"claude-3-5-sonnet-20241022","system":"You are a senior software engineer","messages":[]}"#;
        let hash = extract_prompt_hash(body);
        assert!(hash.is_some(), "should extract hash from Anthropic system field");
        assert_eq!(hash.as_deref().unwrap().len(), 16);
    }

    #[test]
    fn prompt_hash_none_for_no_system_prompt() {
        let body = r#"{"model":"gpt-4o","messages":[{"role":"user","content":"Hello"}]}"#;
        assert_eq!(extract_prompt_hash(body), None, "no system prompt → no hash");
    }

    #[test]
    fn prompt_hash_different_prompts_produce_different_hashes() {
        let a = r#"{"model":"gpt-4o","messages":[{"role":"system","content":"Prompt A"}]}"#;
        let b = r#"{"model":"gpt-4o","messages":[{"role":"system","content":"Prompt B"}]}"#;
        assert_ne!(extract_prompt_hash(a), extract_prompt_hash(b));
    }

    // -------------------------------------------------------------------------
    // default_upstream_for_provider
    // -------------------------------------------------------------------------

    #[test]
    fn default_upstream_anthropic() {
        assert_eq!(default_upstream_for_provider("anthropic"), "https://api.anthropic.com");
    }

    #[test]
    fn default_upstream_google() {
        assert_eq!(default_upstream_for_provider("google"), "https://generativelanguage.googleapis.com");
    }

    #[test]
    fn default_upstream_unknown_falls_back_to_openai() {
        assert_eq!(default_upstream_for_provider("openai"), "https://api.openai.com");
        assert_eq!(default_upstream_for_provider("groq"), "https://api.openai.com");
        assert_eq!(default_upstream_for_provider("unknown"), "https://api.openai.com");
    }
}
