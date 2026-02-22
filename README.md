# trace

**The strace of LLM calls.** A single Rust binary that acts as a local reverse proxy, recording every request and response to a SQLite file on your machine. No cloud. No infra. No data leaving your machine.

```bash
cargo build --release && ./target/release/trace start
OPENAI_BASE_URL=http://localhost:4000/v1 python my_app.py
trace stats --breakdown
```

**Why this exists:**

- **Helicone and LangSmith send your prompts to their servers.** trace writes everything to `~/.trace/trace.db` and stops there.
- **Langfuse self-hosted requires ClickHouse + Postgres + Redis + a background worker.** trace is one binary and one file.
- **You change one env var.** No SDK wrapping, no code changes, no redeployment.

---

## Quickstart

**1. Build and install:**

```bash
git clone <repo> && cd trace
cargo build --release

# Optional: install globally
cargo install --path .
```

**2. Start the proxy:**

```bash
# OpenAI (default)
trace start

# Anthropic
trace start --upstream https://api.anthropic.com

# Groq, Together, Mistral, or any OpenAI-compatible endpoint
trace start --upstream https://api.groq.com
```

Output at startup:

```
trace v0.1
  listening  http://127.0.0.1:4000
  upstream   https://api.openai.com
  storage    /home/user/.trace/trace.db
  retention  90 days

Set your LLM client:
  OPENAI_BASE_URL=http://localhost:4000/v1
```

**3. Point your app at it and run:**

```bash
# Python / OpenAI SDK
OPENAI_BASE_URL=http://localhost:4000/v1 python my_app.py

# Anthropic SDK
ANTHROPIC_BASE_URL=http://localhost:4000 python my_app.py

# Node.js / OpenAI SDK
OPENAI_BASE_URL=http://localhost:4000/v1 node my_app.js
```

No code changes. The proxy passes all headers through unchanged, including your `Authorization` header.

---

## How it works

```
your app
    │
    │  OPENAI_BASE_URL=http://localhost:4000/v1
    ▼
trace  (localhost:4000)
    │  ├─ buffers request body, extracts model + streaming flag
    │  ├─ forwards request upstream (headers, body, query string — all unchanged)
    │  ├─ streams response back to your app
    │  ├─ captures TTFT on first non-empty chunk
    │  └─ writes record to ~/.trace/trace.db (background task, non-blocking)
    ▼
upstream LLM API
(api.openai.com, api.anthropic.com, ...)
```

The proxy path adds no observable latency. DB writes happen on a bounded background channel (capacity 20,000 records); under extreme load, records are dropped rather than blocking your requests.

---

## Commands

### `trace start` — run the proxy

| Flag | Env var | Default | Description |
|---|---|---|---|
| `-p`, `--port` | `TRACE_PORT` | `4000` | Port to listen on |
| `-u`, `--upstream` | `TRACE_UPSTREAM` | `https://api.openai.com` | Upstream LLM API base URL |
| `--bind` | `TRACE_BIND` | `127.0.0.1` | Bind address (`0.0.0.0` exposes on LAN, shows a warning) |
| `-v`, `--verbose` | `TRACE_VERBOSE` | off | Print each request/response to stderr |
| `--upstream-timeout` | `TRACE_UPSTREAM_TIMEOUT` | `300` | Upstream timeout in seconds |
| `--retention-days` | `TRACE_RETENTION_DAYS` | `90` | Auto-delete records older than N days (0 = keep forever) |

```bash
trace start --upstream https://api.anthropic.com --port 4001 --verbose
```

---

### `trace query` — inspect captured calls

```bash
trace query                              # last 20 calls
trace query --last 100                   # last 100 calls
trace query --model gpt-4o              # filter by model (substring)
trace query --errors                     # failed calls only (status >= 400 or connection failure)
trace query --since 2026-02-20          # calls from this date onward
trace query --since 2026-02-20T10:00:00Z --until 2026-02-20T11:00:00Z
trace query --json                       # JSON output
trace query --json --bodies              # include request/response bodies
trace query --json --bodies --full       # bodies without truncation
```

| Flag | Default | Description |
|---|---|---|
| `-l`, `--last N` | `20` | Number of recent calls to show |
| `-j`, `--json` | off | Output as JSON |
| `-b`, `--bodies` | off | Include request and response bodies |
| `--full` | off | Print bodies without truncation (use with `--bodies`) |
| `-m`, `--model SUBSTR` | — | Filter by model name (substring match) |
| `-e`, `--errors` | off | Show only failed calls |
| `--since TIMESTAMP` | — | ISO 8601 or `YYYY-MM-DD` |
| `--until TIMESTAMP` | — | ISO 8601 or `YYYY-MM-DD` |

Example table output:

```
id        timestamp            model                   status   latency    ttft    in    out        cost
a1b2c3d4  2026-02-22T14:01:03  gpt-4o                     200    843ms   312ms   512   128     $0.0064
e5f6a7b8  2026-02-22T14:01:11  claude-3-5-sonnet-202…     200   1204ms   489ms   340    95     $0.0052
c9d0e1f2  2026-02-22T14:01:19  gpt-4o-mini                200    231ms    98ms   210    44     $0.0001
f3a4b5c6  2026-02-22T14:01:27  gpt-4o                       0      12ms     -      -     -          -
  error: connection refused
```

---

### `trace watch` — live tail

Stream new calls to the terminal as they arrive. Polls every 250ms. Ctrl-C to stop.

```bash
trace watch
trace watch --model claude
trace watch --errors
```

| Flag | Description |
|---|---|
| `-m`, `--model SUBSTR` | Filter by model name |
| `-e`, `--errors` | Show only failed calls |

---

### `trace show ID` — full detail for one call

Pass any unambiguous prefix from the `id` column (8+ characters is enough).

```bash
trace show a1b2c3d4
trace show a1b2c3d4 --no-bodies    # hide request/response bodies
```

```
trace show
  id           a1b2c3d4-e5f6-7890-abcd-ef1234567890
  timestamp    2026-02-22T14:01:03.441Z
  provider     openai
  model        gpt-4o
  endpoint     /v1/chat/completions
  status       200
  latency      843ms
  ttft         312ms
  in tokens    512
  out tokens   128
  cost         $0.006400
  provider id  chatcmpl-abc123xyz

request:
{
  "model": "gpt-4o",
  "messages": [{"role": "user", "content": "..."}],
  ...
}

response:
{
  "id": "chatcmpl-abc123xyz",
  "choices": [...],
  ...
}
```

---

### `trace stats` — aggregate usage and cost

```bash
trace stats                   # overall totals
trace stats --breakdown       # per-model breakdown
trace stats --endpoint        # per-endpoint breakdown
```

| Flag | Description |
|---|---|
| `-b`, `--breakdown` | Cost, tokens, and latency per model |
| `--endpoint` | Cost, tokens, and latency per endpoint |

Example output:

```
trace stats

  total calls       1,847
  calls last hour   23
  errors            2
  avg latency       614ms

  input tokens      4.8M
  output tokens     983.4K
  estimated cost    $28.4712

by model:
  model                           calls       in       out       cost     avg ms
  gpt-4o                            821     2.4M    512.0K   $18.1200      712ms
  gpt-4o-mini                       614   891.2K    201.3K    $0.2546      198ms
  claude-3-5-sonnet-20241022        412     1.5M    270.1K   $10.0953      934ms
```

---

### `trace info` — database location

```bash
trace info
# DB path  /home/user/.trace/trace.db
# DB size  24.3 MB
```

---

### `trace clear` — delete all records

Prompts for confirmation unless `--yes` is passed. Runs `VACUUM` after deletion to compact the file.

```bash
trace clear
trace clear --yes
```

---

## What gets captured

Every call is one row in SQLite at `~/.trace/trace.db`.

| Field | Type | Description |
|---|---|---|
| `id` | `TEXT` | UUID v4, unique per call |
| `timestamp` | `TEXT` | UTC, ISO 8601, e.g. `2026-02-22T14:01:03.441Z` |
| `provider` | `TEXT` | Detected from upstream URL: `openai`, `anthropic`, `google`, `groq`, `mistral`, `cohere`, `together` |
| `model` | `TEXT` | Extracted from request body `"model"` field |
| `endpoint` | `TEXT` | Request path only, e.g. `/v1/chat/completions` (query string stripped) |
| `status_code` | `INTEGER` | HTTP status from upstream; `0` = connection failure before any response |
| `latency_ms` | `INTEGER` | Total round-trip in milliseconds |
| `ttft_ms` | `INTEGER` | Time to first token in ms (streaming only; equals `latency_ms` for non-streaming) |
| `input_tokens` | `INTEGER` | Prompt tokens from upstream `usage` field |
| `output_tokens` | `INTEGER` | Completion tokens from upstream `usage` field |
| `cost_usd` | `REAL` | Estimated cost in USD, from built-in pricing table |
| `request_body` | `TEXT` | Full JSON request body (up to 16MB) |
| `response_body` | `TEXT` | For non-streaming: full response JSON (up to 10MB). For streaming: extracted text content (up to 256KB). Tool calls are captured as a compact JSON summary. |
| `provider_request_id` | `TEXT` | Value of `x-request-id` response header — useful for provider support tickets |
| `error` | `TEXT` | Upstream connection error message, if any |

**Notes:**
- Query strings are stripped from the stored `endpoint` (they may contain API keys). The full query string is forwarded to the upstream API.
- Response bodies larger than 10MB are forwarded to your app but not stored.
- Streaming responses accumulate up to 4MB of raw SSE data in memory for parsing.

---

## Supported providers

| Provider | Upstream URL |
|---|---|
| OpenAI | `https://api.openai.com` (default) |
| Anthropic | `https://api.anthropic.com` |
| Google Gemini | `https://generativelanguage.googleapis.com` |
| Mistral | `https://api.mistral.ai` |
| Groq | `https://api.groq.com` |
| Together AI | `https://api.together.xyz` |
| Cohere | `https://api.cohere.ai` |
| DeepSeek | `https://api.deepseek.com` |

Any OpenAI-compatible endpoint works. The `provider` label in the database is detected from the upstream URL substring. For local models (Ollama, LM Studio, vLLM), provider will show as `unknown` — everything else still works.

The built-in pricing table covers OpenAI (GPT-4o, GPT-4o mini, o1, o3, embeddings), Anthropic (Claude 3, 3.5, 4 families), Google Gemini (1.5, 2.0), Meta Llama (3.1, 3.3), Mistral, DeepSeek, and Gemma. Unknown models fall back to $1.00/$3.00 per million tokens input/output and emit a warning in verbose mode.

---

## Comparison

| | **trace** | Helicone | LangSmith | Langfuse |
|---|:---:|:---:|:---:|:---:|
| Self-hosted | yes | optional | no | yes |
| Data stays local | **yes** | no | no | optional |
| Zero infrastructure | **yes** | no | no | no |
| Streaming body capture | **yes (256KB cap)** | partial | partial | partial |
| TTFT metric | **yes** | no | no | no |
| Cost tracking | yes | yes | yes | yes |
| Setup time | **~30s** | ~5 min | ~5 min | ~30 min |

Helicone, LangSmith, and Langfuse are mature products. The tradeoff is that your prompts, completions, and call metadata leave your infrastructure. For teams with data residency requirements, SOC 2 audits in progress, or simply a preference for not sharing LLM I/O with a third party, trace is the alternative.

Langfuse self-hosted requires ClickHouse + Postgres + Redis + a background worker to run. trace is a single binary with a single SQLite file. `trace clear` returns you to zero.

---

## Security notes

- The proxy binds to `127.0.0.1` by default. Use `--bind 0.0.0.0` only on trusted networks; a warning is printed at startup.
- TLS certificates are always verified for upstream connections.
- Hop-by-hop headers (`connection`, `transfer-encoding`, `keep-alive`) and network topology headers (`x-forwarded-for`, `x-real-ip`) are stripped before forwarding.
- `proxy-authorization` headers are stripped and never forwarded to the upstream LLM provider.
- On Unix, `~/.trace/trace.db` is created with `0600` permissions (owner read/write only). On Windows, NTFS default permissions apply; avoid using trace on shared Windows machines.
- Full request bodies — including your prompts and any PII they contain — are stored in plaintext SQLite. Keep this in mind before shipping trace to production.

---

## Contributing

Bug reports and pull requests are welcome.

```bash
cargo test   # 229 tests, no external dependencies required
```

## License

MIT
