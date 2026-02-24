// Criterion benchmarks for the hot paths in trace.
//
// To enable, add to Cargo.toml:
//
//   [dev-dependencies]
//   criterion = { version = "0.5", features = ["html_reports"] }
//
//   [[bench]]
//   name = "proxy_throughput"
//   harness = false
//
// Then run: cargo bench --bench proxy_throughput
// HTML reports land in target/criterion/.
//
// Note: criterion's transitive dependency num-traits has a build script.
// On machines with Windows Defender Application Control (WDAC), that build
// script may be blocked. If you see "An Application Control policy has
// blocked this file", the benchmarks cannot run on that machine.

use bytes::Bytes;
use criterion::{black_box, criterion_group, criterion_main, BenchmarkId, Criterion};
use trace::capture;
use trace::store::{CallRecord, QueryFilter, Store};

// ---------------------------------------------------------------------------
// Static fixtures — defined once, reused across all benchmark iterations.
// Using `static` means no heap allocation per iteration for fixture setup.
// ---------------------------------------------------------------------------

/// Realistic OpenAI non-streaming chat completion response body.
static OPENAI_RESPONSE_BODY: &str = r#"{
  "id": "chatcmpl-9XwqKLmNpRtVuHjBsAeDfCgYiZoP",
  "object": "chat.completion",
  "created": 1709856000,
  "model": "gpt-4o-2024-05-13",
  "choices": [
    {
      "index": 0,
      "message": {
        "role": "assistant",
        "content": "The Rust programming language prioritizes memory safety, performance, and concurrency without a garbage collector. Its ownership system enforces strict compile-time guarantees that prevent data races and dangling pointers, making it ideal for systems programming."
      },
      "logprobs": null,
      "finish_reason": "stop"
    }
  ],
  "usage": {
    "prompt_tokens": 312,
    "completion_tokens": 48,
    "total_tokens": 360
  },
  "system_fingerprint": "fp_a5d11b2ef2"
}"#;

/// Build the 10-chunk OpenAI streaming SSE fixture once via `lazy_static`-style
/// initialization (a `const fn` cannot construct `Vec<Bytes>`, so we use a
/// free function called inside the benchmark setup closure instead — see below).
fn openai_sse_chunks() -> Vec<Bytes> {
    vec![
        // chunk 0 — role delta (no content yet)
        Bytes::from_static(b"data: {\"id\":\"chatcmpl-abc\",\"object\":\"chat.completion.chunk\",\"created\":1709856000,\"model\":\"gpt-4o-2024-05-13\",\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"content\":\"\"},\"finish_reason\":null}]}\n\n"),
        // chunks 1-7 — content deltas
        Bytes::from_static(b"data: {\"id\":\"chatcmpl-abc\",\"object\":\"chat.completion.chunk\",\"created\":1709856000,\"model\":\"gpt-4o-2024-05-13\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"The \"},\"finish_reason\":null}]}\n\n"),
        Bytes::from_static(b"data: {\"id\":\"chatcmpl-abc\",\"object\":\"chat.completion.chunk\",\"created\":1709856000,\"model\":\"gpt-4o-2024-05-13\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"Rust \"},\"finish_reason\":null}]}\n\n"),
        Bytes::from_static(b"data: {\"id\":\"chatcmpl-abc\",\"object\":\"chat.completion.chunk\",\"created\":1709856000,\"model\":\"gpt-4o-2024-05-13\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"programming \"},\"finish_reason\":null}]}\n\n"),
        Bytes::from_static(b"data: {\"id\":\"chatcmpl-abc\",\"object\":\"chat.completion.chunk\",\"created\":1709856000,\"model\":\"gpt-4o-2024-05-13\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"language \"},\"finish_reason\":null}]}\n\n"),
        Bytes::from_static(b"data: {\"id\":\"chatcmpl-abc\",\"object\":\"chat.completion.chunk\",\"created\":1709856000,\"model\":\"gpt-4o-2024-05-13\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"is \"},\"finish_reason\":null}]}\n\n"),
        Bytes::from_static(b"data: {\"id\":\"chatcmpl-abc\",\"object\":\"chat.completion.chunk\",\"created\":1709856000,\"model\":\"gpt-4o-2024-05-13\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"great.\"},\"finish_reason\":null}]}\n\n"),
        Bytes::from_static(b"data: {\"id\":\"chatcmpl-abc\",\"object\":\"chat.completion.chunk\",\"created\":1709856000,\"model\":\"gpt-4o-2024-05-13\",\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n"),
        // chunk 8 — usage (split: first half)
        // Simulates an SSE event split across a network packet boundary.
        Bytes::from(
            "data: {\"id\":\"chatcmpl-abc\",\"object\":\"chat.completion.chunk\",\"created\":1709856000,\"model\":\"gpt-4o-2024-05-13\",\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}],\"usage\":{\"prompt_tokens\":312,\"completio"
                .to_string(),
        ),
        // chunk 9 — usage (split: second half) + DONE
        Bytes::from("n_tokens\":48,\"total_tokens\":360}}\n\ndata: [DONE]\n\n".to_string()),
    ]
}

/// 20-chunk OpenAI SSE stream with realistic delta content across all chunks.
fn openai_sse_content_chunks() -> Vec<Bytes> {
    // We generate 18 content-carrying chunks plus a usage chunk and a [DONE].
    let words = [
        "The", " Rust", " ownership", " system", " enforces", " strict",
        " compile-time", " guarantees", " that", " prevent", " data", " races",
        " and", " dangling", " pointers,", " making", " it", " ideal",
    ];
    assert_eq!(words.len(), 18, "must be exactly 18 content words for 20 chunks total");

    let mut chunks: Vec<Bytes> = words
        .iter()
        .map(|word| {
            Bytes::from(format!(
                "data: {{\"id\":\"chatcmpl-xyz\",\"object\":\"chat.completion.chunk\",\
                 \"created\":1709856000,\"model\":\"gpt-4o-2024-05-13\",\
                 \"choices\":[{{\"index\":0,\"delta\":{{\"content\":\"{}\"}},\"finish_reason\":null}}]}}\n\n",
                word
            ))
        })
        .collect();

    // chunk 19 — usage + stop
    chunks.push(Bytes::from_static(
        b"data: {\"id\":\"chatcmpl-xyz\",\"object\":\"chat.completion.chunk\",\
          \"created\":1709856000,\"model\":\"gpt-4o-2024-05-13\",\
          \"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}],\
          \"usage\":{\"prompt_tokens\":200,\"completion_tokens\":30,\"total_tokens\":230}}\n\n",
    ));

    // chunk 20 — [DONE]
    chunks.push(Bytes::from_static(b"data: [DONE]\n\n"));

    chunks
}

// ---------------------------------------------------------------------------
// Helper — build a single CallRecord for store benchmarks.
// ---------------------------------------------------------------------------

fn make_record(id: &str) -> CallRecord {
    CallRecord {
        id: id.to_string(),
        timestamp: "2026-02-22T12:00:00.000Z".to_string(),
        provider: "openai".to_string(),
        model: "gpt-4o-2024-05-13".to_string(),
        endpoint: "/v1/chat/completions".to_string(),
        status_code: 200,
        latency_ms: 843,
        ttft_ms: Some(312),
        input_tokens: Some(312),
        output_tokens: Some(48),
        cost_usd: Some(0.001260),
        request_body: Some(
            r#"{"model":"gpt-4o-2024-05-13","messages":[{"role":"user","content":"Explain Rust ownership"}]}"#
                .to_string(),
        ),
        response_body: Some(
            r#"{"choices":[{"message":{"content":"The Rust ownership system..."}}]}"#.to_string(),
        ),
        error: None,
        provider_request_id: Some("req-9XwqKLmNpRtVuHjBsAeDfCgYiZoP".to_string()),
        trace_id: None,
        parent_id: None,
        prompt_hash: None,
    }
}

// ---------------------------------------------------------------------------
// Benchmark 1 — capture::extract_usage (non-streaming response body)
// ---------------------------------------------------------------------------

fn bench_capture_extract_usage(c: &mut Criterion) {
    c.bench_function("capture_extract_usage", |b| {
        b.iter(|| {
            let result = capture::extract_usage(black_box(OPENAI_RESPONSE_BODY));
            black_box(result)
        })
    });
}

// ---------------------------------------------------------------------------
// Benchmark 2 — capture::extract_usage_from_chunks (streaming, 10 chunks)
// ---------------------------------------------------------------------------

fn bench_capture_extract_usage_from_chunks(c: &mut Criterion) {
    // Build fixture once outside the measurement loop.
    let chunks = openai_sse_chunks();

    c.bench_function("capture_extract_usage_from_chunks", |b| {
        b.iter(|| {
            let result = capture::extract_usage_from_chunks(black_box(&chunks));
            black_box(result)
        })
    });
}

// ---------------------------------------------------------------------------
// Benchmark 3 — capture::extract_response_text_from_chunks (20 chunks)
// ---------------------------------------------------------------------------

fn bench_capture_extract_response_text(c: &mut Criterion) {
    let chunks = openai_sse_content_chunks();

    c.bench_function("capture_extract_response_text", |b| {
        b.iter(|| {
            let result = capture::extract_response_text_from_chunks(black_box(&chunks));
            black_box(result)
        })
    });
}

// ---------------------------------------------------------------------------
// Benchmark 4 — store::Store::insert (fresh in-memory store per iteration)
// ---------------------------------------------------------------------------

fn bench_store_insert(c: &mut Criterion) {
    c.bench_function("store_insert", |b| {
        // iter_with_setup: setup runs outside timing, only the closure is timed.
        // Each iteration gets its own freshly initialized in-memory store so
        // there is no primary-key collision across iterations.
        b.iter_with_setup(
            || {
                // Setup (not timed): open a fresh in-memory DB.
                let store = Store::open_in_memory().expect("failed to open in-memory store");
                let record = make_record("bench-id-0001");
                (store, record)
            },
            |(store, record)| {
                // Measured: a single INSERT.
                store.insert(black_box(&record)).expect("insert failed");
                // Return the store to prevent the drop from being elided
                // (in-memory DB destruction is not timed, but we want Drop
                // to happen outside the measurement window — Criterion handles
                // that because iter_with_setup drops after the timing window).
                black_box(store)
            },
        )
    });
}

// ---------------------------------------------------------------------------
// Benchmark 5 — store::Store::query_filtered (1 000 pre-inserted rows)
// ---------------------------------------------------------------------------

fn bench_store_query_filtered(c: &mut Criterion) {
    // Pre-populate the store once; re-use across all iterations.
    // We use iter_batched with SmallInput so Criterion clones the state
    // for each batch — but Store doesn't implement Clone, so instead we
    // build the store in the setup closure and measure many queries against it.
    //
    // Strategy: open + populate once, then drive iter() which only measures
    // query_filtered. The store lives for the entire benchmark run.
    let store = Store::open_in_memory().expect("failed to open in-memory store");

    // Insert 1 000 records with unique IDs and varied models/statuses.
    for i in 0..1_000usize {
        let model = if i % 3 == 0 {
            "gpt-4o-2024-05-13"
        } else if i % 3 == 1 {
            "claude-3-5-sonnet-20241022"
        } else {
            "gpt-4o-mini"
        };
        let status: u16 = if i % 10 == 0 { 429 } else { 200 };
        let mut r = make_record(&format!("bench-record-{:06}", i));
        r.model = model.to_string();
        r.status_code = status;
        // Spread timestamps so ordering is realistic.
        r.timestamp = format!("2026-02-22T{:02}:{:02}:{:02}.000Z", (i / 3600) % 24, (i / 60) % 60, i % 60);
        store.insert(&r).expect("insert failed during setup");
    }

    let filter = QueryFilter {
        errors_only: false,
        model: Some("gpt-4o".to_string()), // substring match — hits ~667 rows
        since: None,
        until: None,
        provider: None,
        status: None,
        status_range: None,
    };

    // Parameterised by result-set limit so we can compare different sizes.
    let mut group = c.benchmark_group("store_query_filtered");
    for limit in [20usize, 100] {
        group.bench_with_input(
            BenchmarkId::from_parameter(format!("limit={}", limit)),
            &limit,
            |b, &lim| {
                b.iter(|| {
                    let results = store
                        .query_filtered(black_box(lim), black_box(&filter))
                        .expect("query failed");
                    black_box(results)
                })
            },
        );
    }
    group.finish();
}

// ---------------------------------------------------------------------------
// Criterion entry points
// ---------------------------------------------------------------------------

criterion_group!(
    benches,
    bench_capture_extract_usage,
    bench_capture_extract_usage_from_chunks,
    bench_capture_extract_response_text,
    bench_store_insert,
    bench_store_query_filtered,
);
criterion_main!(benches);
