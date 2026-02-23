# trace

**The strace of LLM calls.** A single Rust binary that acts as a local reverse proxy, recording every request and response to a SQLite file on your machine. No cloud. No infra. No data leaving your machine.

```bash
# Install (npm — no Rust required)
npm install -g @opentraceai/trace

# Or build from source
cargo build --release && ./target/release/trace start

OPENAI_BASE_URL=http://localhost:4000/v1 python my_app.py
trace stats --breakdown
trace serve --port 8080    # live web dashboard
```

**Why this exists:**

- **Helicone and LangSmith send your prompts to their servers.** trace writes everything to `~/.trace/trace.db` and stops there.
- **Langfuse self-hosted requires ClickHouse + Postgres + Redis + a background worker.** trace is one binary and one file.
- **You change one env var.** No SDK wrapping, no code changes, no redeployment.

---

## Quickstart

**1. Install:**

```bash
# npm (no Rust required — pre-built binaries for Linux x64/arm64, macOS x64/arm64, Windows x64)
npm install -g @opentraceai/trace

# Homebrew / Cargo (build from source)
git clone <repo> && cd trace
cargo build --release

# Install globally via Cargo
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

# With config file — no flags needed
echo '[start]
port = 4000
upstream = "https://api.openai.com"' > .trace.toml
trace start
```

Output at startup:

```
trace v0.25.0
  listening  http://127.0.0.1:4000
  upstream   https://api.openai.com
  storage    /home/user/.trace/trace.db
  retention  90 days

Set your LLM client:
  OPENAI_BASE_URL=http://localhost:4000/v1

Note: full request/response bodies (including prompts) are stored in /home/user/.trace/trace.db

Listening on 127.0.0.1:4000
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
| `--no-request-bodies` | `TRACE_NO_REQUEST_BODIES` | off | Do not store request bodies (prompts) in the database |
| `--redact-fields` | `TRACE_REDACT_FIELDS` | — | Comma-separated top-level JSON keys to replace with `[REDACTED]` before storing (e.g. `messages,system_prompt`) |
| `--metrics-port` | `TRACE_METRICS_PORT` | off | Expose Prometheus metrics on this port (e.g. `9091`) |
| `--otel-endpoint` | `OTEL_EXPORTER_OTLP_ENDPOINT` | — | Send OTLP HTTP/JSON spans to this endpoint (e.g. `http://localhost:4318`) |
| `--budget-alert-usd` | `TRACE_BUDGET_ALERT_USD` | — | Emit a stderr warning when spend exceeds this USD amount in the period |
| `--budget-period` | `TRACE_BUDGET_PERIOD` | `month` | Budget period: `day`, `week`, or `month` |
| `--route PATH=URL` | — | — | Route a path prefix to a different upstream. May be specified multiple times. More specific paths must come first. |

```bash
trace start --upstream https://api.anthropic.com --port 4001 --verbose
trace start --redact-fields messages,system_prompt
trace start --budget-alert-usd 50.0 --budget-period month --metrics-port 9091

# Multi-provider in one process: Anthropic messages + OpenAI everything else
trace start \
  --route "/v1/messages=https://api.anthropic.com" \
  --upstream https://api.openai.com
```

**Multi-upstream routing**

Point one proxy instance at multiple LLM providers simultaneously. Traffic is fanned to the correct upstream based on the request path prefix; the default upstream catches everything else.

```
trace start --port 4000 \
  --route "/v1/messages=https://api.anthropic.com" \
  --upstream https://api.openai.com

trace v0.25.0
  listening  http://127.0.0.1:4000
  upstream   https://api.openai.com
  routes     1 rule(s):
               /v1/messages -> https://api.anthropic.com
               * -> https://api.openai.com (default)

Set your LLM client:
  OPENAI_BASE_URL=http://localhost:4000/v1     # gpt-* models
  ANTHROPIC_BASE_URL=http://localhost:4000     # claude-* models

Note: full request/response bodies (including prompts) are stored in /home/user/.trace/trace.db

Listening on 127.0.0.1:4000
```

Routes can also be set in `.trace.toml`:

```toml
[[start.routes]]
path = "/v1/messages"
upstream = "https://api.anthropic.com"
```

Routing rules:
- First prefix match wins — list more specific paths before less specific ones
- A prefix match requires a path-segment boundary: `/v1/messages` matches `/v1/messages/stream` but **not** `/v1/messages2`
- A shadowed route (a longer prefix listed after a shorter prefix that covers it) emits a startup warning: `WARNING: route "/v1/messages" is shadowed by earlier route "/v1" — it will never match`
- Routes checked in CLI order first, then config file order
- Route path must start with `/`; route upstream must use `http://` or `https://` — otherwise trace exits at startup with an error
- Routes cannot be set via a single environment variable; use `--route` flags or `[[start.routes]]` in the config file

---

### `trace serve` — local web dashboard

Spin up a browser dashboard that reads from the existing `~/.trace/trace.db`. Does **not** run a proxy — use alongside `trace start` in another terminal.

```bash
trace serve              # http://localhost:8080
trace serve --port 3000  # custom port
```

The dashboard auto-refreshes every 2 seconds and shows:
- Stat cards: total calls, total cost, avg latency, calls last hour
- Live call log with model filter and errors-only checkbox
- Fully offline — zero CDN dependencies

| Flag | Env var | Default | Description |
|---|---|---|---|
| `-p`, `--port` | `TRACE_UI_PORT` | `8080` | Dashboard port |

The `serve.port` field in the config file sets the default port.

---

### `trace report` — cost report for CI/CD

Print a summary of costs for captured calls. Designed for CI pipelines.

```bash
trace report                              # text table, all time
trace report --since 2026-02-01           # from a date
trace report --model gpt-4o              # filter by model
trace report --format json               # JSON output
trace report --format github             # GitHub Actions job summary markdown
trace report --fail-over-usd 5.00        # exit 1 if total cost > $5.00
```

| Flag | Default | Description |
|---|---|---|
| `--since TIMESTAMP` | — | ISO 8601 or `YYYY-MM-DD` |
| `--until TIMESTAMP` | — | ISO 8601 or `YYYY-MM-DD` |
| `-m`, `--model SUBSTR` | — | Filter by model name (substring match) |
| `--format text\|json\|github` | `text` | Output format |
| `--fail-over-usd AMOUNT` | — | Exit with code 1 if total cost exceeds this value |

Text output:

```
OpenTrace Cost Report  2026-02-01 -> now
model                          calls    input tok   output tok         cost
------------------------------------------------------------------------------
gpt-4o                           142      850,200      142,000      $0.8423
gpt-4o-mini                      891    3,201,100      982,000      $0.1204
------------------------------------------------------------------------------
TOTAL                          1,033                               $0.9627
```

`--format github` writes a Markdown table to `$GITHUB_STEP_SUMMARY` when that env var is set (GitHub Actions), otherwise prints to stdout.

**CI usage:**
```yaml
- name: Check LLM spend
  run: trace report --format github --fail-over-usd 10.00
```

---

### `trace config` — manage config file

```bash
trace config path    # print which config file is active (or "no config file found")
trace config show    # print the effective merged config
trace config init    # write a default skeleton to ~/.config/trace/config.toml
```

Config files are searched in order:
1. `.trace.toml` in the current working directory (project-local, takes priority)
2. `~/.config/trace/config.toml` (user-global)

Priority: **CLI flags > env vars > config file > hardcoded defaults**

Example `.trace.toml`:

```toml
[start]
port = 4000
upstream = "https://api.openai.com"
retention_days = 90
# metrics_port = 9091
# otel_endpoint = "http://localhost:4318"
# redact_fields = ["messages"]
# budget_alert_usd = 50.0
# budget_period = "month"

# Route specific path prefixes to different upstreams:
# [[start.routes]]
# path = "/v1/messages"
# upstream = "https://api.anthropic.com"

[serve]
port = 8080
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

When `--budget-alert-usd` was passed to `trace start`, the watch header shows a live budget line:

```
budget  $3.21 / $50.00 this month  (6%)
```

The line turns red when spend reaches 80% of the limit.

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
trace stats                   # overall totals + p50/p95/p99 latency
trace stats --breakdown       # per-model breakdown
trace stats --endpoint        # per-endpoint breakdown
```

| Flag | Description |
|---|---|
| `-b`, `--breakdown` | Cost, tokens, and p99 latency per model |
| `--endpoint` | Cost, tokens, and avg latency per endpoint |

Example output:

```
trace stats

  total calls       1,847
  calls last hour   23
  errors            2
  avg latency       614ms
  latency p50       412ms
  latency p95       1,203ms
  latency p99       2,841ms

  input tokens      4.8M
  output tokens     983.4K
  estimated cost    $28.4712

by model:
  model                           calls       in       out       cost     avg ms   p99 ms
  gpt-4o                            821     2.4M    512.0K   $18.1200      712ms   2900ms
  gpt-4o-mini                       614   891.2K    201.3K    $0.2546      198ms    430ms
  claude-3-5-sonnet-20241022        412     1.5M    270.1K   $10.0953      934ms   3100ms
```

---

### `trace export` — export to file

Export all captured calls to stdout. Pipe to a file.

```bash
trace export > calls.jsonl
trace export --format csv > calls.csv
trace export --since 2026-02-01 --model gpt-4o > filtered.jsonl
```

| Flag | Default | Description |
|---|---|---|
| `--format jsonl\|csv` | `jsonl` | Output format |
| `-m`, `--model SUBSTR` | — | Filter by model name |
| `--since TIMESTAMP` | — | ISO 8601 or `YYYY-MM-DD` |
| `--until TIMESTAMP` | — | ISO 8601 or `YYYY-MM-DD` |

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

## Observability integrations

### Prometheus

```bash
trace start --metrics-port 9091
curl http://localhost:9091/metrics
```

Exposed metrics:

| Metric | Type | Description |
|---|---|---|
| `opentrace_requests_total` | counter | Total requests proxied |
| `opentrace_errors_total` | counter | Requests with status >= 400 or connection failure |
| `opentrace_latency_ms_bucket` | histogram | Request latency in milliseconds |
| `opentrace_input_tokens_total` | counter | Cumulative input tokens |
| `opentrace_output_tokens_total` | counter | Cumulative output tokens |
| `opentrace_cost_usd_total` | counter | Cumulative estimated cost |
| `opentrace_dropped_records_total` | gauge | Records dropped due to DB backpressure |
| `opentrace_budget_spent_usd` | gauge | Spend since start of current budget period (when `--budget-alert-usd` is set) |
| `opentrace_budget_limit_usd` | gauge | Configured budget limit |

### OpenTelemetry

```bash
trace start --otel-endpoint http://localhost:4318
# or
OTEL_EXPORTER_OTLP_ENDPOINT=http://localhost:4318 trace start
```

Each call is exported as an OTLP HTTP/JSON span with attributes: `llm.model`, `llm.provider`, `llm.input_tokens`, `llm.output_tokens`, `llm.cost_usd`, `http.status_code`.

---

## Privacy and compliance

### Field-level request body redaction

Keep call metadata (tokens, cost, latency) without storing sensitive content:

```bash
trace start --redact-fields messages,system_prompt
# or in .trace.toml:
# redact_fields = ["messages", "system_prompt"]
```

Matching **top-level JSON keys** have their values replaced with `"[REDACTED]"` before storing. The body forwarded to the upstream API is **never modified**.

### Suppress all request bodies

```bash
trace start --no-request-bodies
```

Records the call metadata but stores `NULL` in `request_body`.

### Budget alerting

```bash
trace start --budget-alert-usd 50.0 --budget-period month
```

Checks every 60 seconds. Emits to stderr at most once per 10 minutes when spend meets or exceeds the limit:

```
[trace] BUDGET WARNING: $51.23 spent / $50.00 limit this month
```

---

## What gets captured

Every call is one row in SQLite at `~/.trace/trace.db` (or `$XDG_DATA_HOME/trace/trace.db` on Linux when `XDG_DATA_HOME` is explicitly set).

| Field | Type | Description |
|---|---|---|
| `id` | `TEXT` | UUID v4, unique per call |
| `timestamp` | `TEXT` | UTC, ISO 8601, e.g. `2026-02-22T14:01:03.441Z` |
| `provider` | `TEXT` | Detected from upstream URL: `openai`, `anthropic`, `azure-openai`, `google`, `groq`, `mistral`, `cohere`, `together`, `xai`, `perplexity`, `bedrock`, `fireworks`, `nvidia`, `deepseek`, `openrouter`, `ollama`, … |
| `model` | `TEXT` | Extracted from request body `"model"` field |
| `endpoint` | `TEXT` | Request path only, e.g. `/v1/chat/completions` (query string stripped) |
| `status_code` | `INTEGER` | HTTP status from upstream; `0` = connection failure before any response |
| `latency_ms` | `INTEGER` | Total round-trip in milliseconds |
| `ttft_ms` | `INTEGER` | Time to first token in ms (streaming only; equals `latency_ms` for non-streaming) |
| `input_tokens` | `INTEGER` | Prompt tokens from upstream `usage` field |
| `output_tokens` | `INTEGER` | Completion tokens from upstream `usage` field |
| `cost_usd` | `REAL` | Estimated cost in USD, from built-in pricing table |
| `request_body` | `TEXT` | Full JSON request body (up to 16MB); `NULL` when `--no-request-bodies` is set; sensitive fields replaced when `--redact-fields` is set |
| `response_body` | `TEXT` | For non-streaming: full response JSON (up to 10MB). For streaming: extracted text content (up to 256KB). Tool calls are captured as a compact JSON summary. |
| `provider_request_id` | `TEXT` | Value of `x-request-id` response header — useful for provider support tickets |
| `error` | `TEXT` | Upstream connection error message, if any |

**Notes:**
- Query strings are stripped from the stored `endpoint` (they may contain API keys). The full query string is forwarded to the upstream API.
- Response bodies larger than 10MB are forwarded to your app but not stored.
- Streaming responses accumulate up to 4MB of raw SSE data in memory for parsing.

---

## Supported providers

| Provider | Upstream URL | Label |
|---|---|---|
| OpenAI | `https://api.openai.com` (default) | `openai` |
| Anthropic | `https://api.anthropic.com` | `anthropic` |
| Azure OpenAI | `https://*.openai.azure.com` | `azure-openai` |
| Google Gemini | `https://generativelanguage.googleapis.com` | `google` |
| Amazon Bedrock | `https://bedrock-runtime.*.amazonaws.com` | `bedrock` |
| OpenRouter | `https://openrouter.ai` | `openrouter` |
| xAI (Grok) | `https://api.x.ai` | `xai` |
| Perplexity | `https://api.perplexity.ai` | `perplexity` |
| Alibaba Qwen | `https://dashscope.aliyuncs.com` | `qwen` |
| Zhipu AI (GLM) | `https://api.z.ai` | `zhipu` |
| Moonshot (Kimi) | `https://platform.moonshot.ai` | `moonshot` |
| Mistral | `https://api.mistral.ai` | `mistral` |
| Groq | `https://api.groq.com` | `groq` |
| Together AI | `https://api.together.xyz` | `together` |
| Cohere | `https://api.cohere.ai` | `cohere` |
| DeepSeek | `https://api.deepseek.com` | `deepseek` |
| Fireworks AI | `https://api.fireworks.ai` | `fireworks` |
| NVIDIA NIM | `https://integrate.api.nvidia.com` | `nvidia` |
| AI21 Labs | `https://api.ai21.com` | `ai21` |
| Ollama / local | `http://localhost:11434` | `ollama` |

Any OpenAI-compatible endpoint works. The `provider` label in the database is detected from the upstream URL. For local models (LM Studio, vLLM), provider shows as `unknown` — everything else still works.

OpenRouter routes to 200+ models; trace detects the provider as `openrouter` and correctly prices the model from the request body (e.g. `qwen/qwen3-max`, `meta-llama/llama-3.1-70b-instruct`).

Amazon Bedrock model IDs (`anthropic.claude-opus-4-6-v1:0`, `meta.llama3-1-70b-instruct-v1:0`, `amazon.nova-pro-v1:0`) are matched correctly — trace handles both standard and Bedrock naming formats.

The built-in pricing table (80+ entries, updated Feb 2026) covers:

| Family | Models |
|---|---|
| **OpenAI GPT-5** | GPT-5.2, GPT-5.1, GPT-5.1-Codex, Codex Mini |
| **OpenAI GPT-4** | GPT-4.1, GPT-4.1 Mini/Nano, GPT-4o, GPT-4o Mini, GPT-4 Turbo |
| **OpenAI reasoning** | o1, o1-mini, o1-preview, o3, o3-mini |
| **OpenAI embeddings** | text-embedding-3-large/small, ada-002 |
| **Anthropic Claude 4** | Opus 4.6, Opus 4.5, Sonnet 4.6, Sonnet 4.5, Haiku 4 (incl. -thinking variants) |
| **Anthropic Claude 3.x** | Claude 3.7 Sonnet, Claude 3.5 Sonnet/Haiku, Claude 3 Opus/Sonnet/Haiku |
| **Google Gemini** | Gemini 3.1 Pro/Flash, Gemini 2.5 Pro/Flash, Gemini 2.0 Flash/Flash-Lite, Gemini 1.5 Pro/Flash/Flash-8B |
| **xAI Grok** | Grok 3, Grok 2, Grok |
| **Meta Llama 4** | Llama 4 Maverick, Llama 4 Scout |
| **Meta Llama 3.x** | Llama 3.3 70B, Llama 3.2 (1B–90B), Llama 3.1 (8B/70B/405B) |
| **Alibaba Qwen** | Qwen3-Max, Qwen3.5-Plus, Qwen3-Coder-Next/Plus, QwQ-32B, Qwen2.5 (7B/72B), Qwen-Max/Plus/Turbo |
| **Zhipu AI GLM** | GLM-5, GLM-4.7 (X/Flash), GLM-4.5 (X/Flash) |
| **Moonshot Kimi** | Kimi K2.5 |
| **Mistral** | Mistral Large/Medium/Small, Codestral, Pixtral, Mixtral |
| **DeepSeek** | DeepSeek R1, DeepSeek V3 |
| **Amazon Nova** | Nova Premier, Nova Pro, Nova Lite, Nova Micro |
| **AI21 Jamba** | Jamba Large 1.7, Jamba 1.5 Large/Mini |
| **Cohere** | Command R+, Command R |
| **Perplexity** | Sonar Pro, Sonar |
| **Google** | Gemma |

Unknown models fall back to $1.00/$3.00 per million tokens input/output.

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
| Field-level redaction | **yes** | no | no | partial |
| Budget alerting | **yes** | paid | paid | paid |
| Web dashboard | **yes (local)** | cloud | cloud | cloud |
| CI cost gate | **yes** | no | no | no |
| Prometheus / OTel | **yes** | no | partial | partial |
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
- Full request bodies — including your prompts and any PII they contain — are stored in plaintext SQLite. Use `--redact-fields` or `--no-request-bodies` for sensitive deployments.

---

## Contributing

Bug reports and pull requests are welcome.

```bash
cargo test   # 514 tests, no external dependencies required
```

## License

MIT
