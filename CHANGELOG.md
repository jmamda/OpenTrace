# Changelog

All notable changes to [OpenTrace](https://github.com/jmamda/OpenTrace) are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/).

## [0.3.8] - 2026-02-28

### Added
- **Embedded Query API**: `trace start` now serves read-only query endpoints on the proxy port under `/_trace/`, eliminating the need for direct SQLite access or a separate `trace serve` process
  - `GET /_trace/api/calls` — list calls with filters (model, provider, agent, tag, workflow, errors, since/until)
  - `GET /_trace/api/stats` — aggregate statistics with model and provider breakdown
  - `GET /_trace/api/call/:id` — single call detail by UUID prefix
  - `GET /_trace/api/search?q=` — FTS5 full-text search with BM25 ranking
  - `GET /_trace/api/heatmap` — daily cost/call aggregates for heatmap visualization
  - `GET /_trace/api/agents` — per-agent statistics
  - `GET /_trace/api/workflow/:id` — workflow call timeline with cost totals
  - `GET /_trace/api/workflows` — workflow summaries list
  - `GET /_trace/stream` — Server-Sent Events for real-time call updates
  - `GET /_trace/health` — JSON health response with version, uptime, status
- `trace status` command — check if proxy is running and display health info
- `--no-api` / `TRACE_NO_API` flag to disable embedded API endpoints
- `/health` endpoint now returns JSON `{"status":"ok","version":"..."}` (was plain 200 OK)
- CORS headers on all embedded API responses
- Tag filtering via `?tag=` query parameter on API calls endpoint

## [0.3.7] - 2026-02-28

### Added
- Multi-agent orchestration headers: `X-Trace-Agent`, `X-Trace-Workflow`, `X-Trace-Span` (consumed, not forwarded upstream)
- `trace workflow <id>` command — show all calls in a workflow as a timeline tree with cost/latency summary
- `trace agents` command — per-agent stats summary (calls, cost, avg latency, errors)
- `trace prices` command — list bundled model prices, `--unknown` to find unpriced DB models, `--check` for full audit, `--update` to fetch from LiteLLM
- `--agent` and `--workflow` filters on query/watch/export commands
- Config validation on startup: unknown keys, typos, negative prices, partial sink credentials, body limits > 1GB
- Per-model price overrides via `[start.prices.MODEL_NAME]` in config file (exact + substring match, longest key wins)
- `is_known_model_with_overrides()` suppresses unknown-model warnings for user-configured prices
- Automatic `stream_options.include_usage` injection for streaming requests (improves token capture for OpenAI/Ollama)
- Non-LLM request filtering: health checks and empty bodies skip recording to reduce DB noise
- `agent_name`, `workflow_id`, `span_name` columns in SQLite schema with indexes
- `agent_name` added to FTS5 full-text search index
- Extended query/watch output with agent column when `--agent` filter is active
- Configurable body size limits via config: `max_request_body_bytes`, `max_stored_response_bytes`, `max_stored_stream_bytes`, `max_accumulation_bytes`
- Configurable SQLite tuning via `[start.sqlite]`: `busy_timeout`, `cache_size_kb`, `mmap_size_mb`, `sync_mode`
- Configurable OTel service name and timeout via `[start.otel]`
- Configurable Prometheus latency buckets via `[start.metrics]`
- Configurable sink timeouts for Langfuse, LangSmith, Weave, Phoenix
- Sink credentials fallback: TOML config values used when CLI flags and env vars are absent
- `--db-path` and `--channel-capacity` CLI flags for `trace start`
- `--graceful-shutdown-timeout` CLI flag
- Ollama/local/self-hosted/GGUF model detection at $0 pricing
- `trace config show` now displays all resolved values with source labels (config/default)
- Dashboard: light/dark theme with CSS variables, Chart.js integration, tab navigation, column sorting, SSE live-update highlight, "Load More" pagination
- Dashboard: `/api/agents` endpoint for agent stats

### Changed
- Body size constants (`MAX_REQUEST_BODY_BYTES`, etc.) changed from module-level consts to configurable `ProxyState` fields
- `ProxyState` now carries `price_overrides: Arc<HashMap<String, (f64, f64)>>` for hot-path pricing
- `estimate_cost_from_body` and `estimate_cost_from_chunks` now accept price overrides parameter
- SQLite pragmas are now configurable instead of hardcoded
- `trace_id`/`prompt_hash` indexes moved after ALTER TABLE migrations (fixes fresh-install ordering)
- Unknown model warning now respects price overrides and skips non-LLM requests
- CORS `Access-Control-Allow-Headers` updated to include new orchestration headers
- Dashboard HTML rewritten with CSS custom properties for theming

### Fixed
- FTS5 search `rank`/`snippet` column offsets updated for 3 new schema columns

## [0.3.6] - 2026-02-24

### Added
- `tags TEXT` column populated from `X-Trace-Tag` header (consumed and stripped before forwarding)
- Tags indexed in FTS5 full-text search
- `GET /stream` SSE endpoint in `trace serve` (broadcast channel + 250ms DB poller)
- Dashboard uses EventSource with polling fallback for live updates

### Fixed
- FTS5 search column offsets corrected after tags column addition
- SSE test rewritten to use `frame()` + spawn-delayed send

## [0.3.5] - 2026-02-23

### Added
- Langfuse real-time sink module (`src/sink/langfuse.rs`)
- LangSmith real-time sink module (`src/sink/langsmith.rs`)
- W&B Weave real-time sink module (`src/sink/weave.rs`)
- Arize Phoenix real-time sink module (`src/sink/phoenix.rs`)
- `gen_ai.*` OTel semantic conventions and OpenInference attributes on exported spans
- `trace export --format langfuse|langsmith|weave` for batch export to external platforms
- CLI flags for sink credentials: `--langfuse-*`, `--langsmith-*`, `--weave-*`, `--phoenix-*`

## [0.3.4] - 2026-02-23

### Added
- Dashboard: 6-card summary layout, provider filter dropdown, heatmap legend, model breakdown table
- Error rate percentage in query output
- Query cost total display
- Watch session cumulative cost display

## [0.3.3] - 2026-02-23

### Added
- `--provider` filter on query/watch/export/stats commands
- `trace search` CLI command (wraps FTS5 full-text search)
- Prompt-cache cost adjustment (Anthropic cache_creation/cache_read, OpenAI cached_tokens)
- Dashboard detail panel for individual call inspection

## [0.3.2] - 2026-02-23

### Fixed
- FTS5 query escaping: hyphens in model names (e.g. `gpt-4o`) no longer treated as NOT operators
- `fts5_escape()` double-quotes every token before MATCH

## [0.3.1] - 2026-02-23

### Changed
- npm org renamed from `@opentrace` to `@opentraceai` (original was taken)
- CI npm publish path and case loop fixes

## [0.3.0] - 2026-02-23

### Added
- FTS5 full-text search: `search_calls` SQL + `/api/search?q=QUERY` endpoint + dashboard search box
- 90-day SVG calendar heatmap: `daily_stats` query + `/api/heatmap` endpoint
- Built-in playground at `/playground` route + `trace playground` CLI command (side-by-side streaming model comparison)
- CORS headers on all proxy responses

## [0.2.9] - 2026-02-23

### Fixed
- aarch64 cross-compilation: switched to `rustls-tls` (reqwest `default-features = false`)

## [0.2.8] - 2026-02-22

### Changed
- Audit fixes and re-versioning for semver alignment

## [0.27] - 2026-02-22

### Added
- Agent trace trees: `trace_id` and `parent_id` columns for call lineage
- `trace eval` command for quality rule checks
- `trace replay` command to replay captured calls against a different model
- `trace diff` command to compare model outputs side by side
- `prompt_hash` column for prompt version tracking

## [0.26] - 2026-02-22

### Added
- `--status` and `--status-range` filters on query/watch/export commands
- Error type auto-classification tags: `[rate_limit]`, `[auth]`, `[timeout]`, `[upstream_5xx]`, `[connection]`
- Token p50/p95/p99 percentiles in `trace stats`
- `trace vacuum` command (WAL checkpoint + VACUUM, prints MB before/after)

## [0.25.0] - 2026-02-22

### Added
- Multi-upstream path routing via `--route /v1/messages=https://api.anthropic.com`
- npm distribution: `npm install -g @opentraceai/trace`
- `TRACE_UPSTREAM` env var for `trace serve`

## [0.24.1] - 2026-02-22

### Added
- Explicit Claude 4.x pricing arms
- GLM/Zhipu, Qwen3, Nova, Jamba, Bedrock alias model detection

## [0.24] - 2026-02-22

### Added
- Frontier model pricing: GPT-5 series, Kimi K2.5
- Updated pricing for 50+ models to Feb 2026 actuals

## [0.23] - 2026-02-22

### Added
- 17 providers: added xAI, Perplexity, Bedrock, Fireworks, NVIDIA NIM
- Updated 50+ model prices to Feb 2026 actuals (GPT-4.1, Claude 3.7, Gemini 2.5, Grok 3, Llama 4, DeepSeek R1, Command-R+, Sonar-Pro)
- Linux `XDG_DATA_HOME` support for database path
- Bedrock detection moved before Anthropic in provider chain (Bedrock model paths contain "anthropic")

## [0.21] - 2026-02-22

### Added
- `trace serve [--port 8080]` local web dashboard
- `trace report [--format text|json|github] [--fail-over-usd]` for CI/CD integration
- `trace config show|path|init` for config file management
- Field-level request body redaction via `--redact-fields`
- Budget alerting: `--budget-alert-usd` / `--budget-period` with stderr warnings + Prometheus gauges
- TOML config file support (`.trace.toml` or `~/.config/trace/config.toml`)

## [0.2.0] - 2026-02-22

### Added
- `trace export [--format jsonl|csv]` for data export
- Prometheus `/metrics` endpoint via `--metrics-port`
- OpenTelemetry OTLP span export via `--otel-endpoint`
- p50/p95/p99 latency percentiles in `trace stats`
- DeepSeek, Azure, Ollama, OpenRouter provider auto-detection

## [0.1.0] - 2026-02-22

### Added
- Initial release of OpenTrace
- Local Rust proxy intercepting LLM API calls
- SQLite storage with WAL mode
- `trace start` with configurable port, upstream, bind address, timeout, retention
- `trace query` with `--last`, `--model`, `--errors`, `--json`, `--full` flags
- `trace watch` for real-time call monitoring (250ms polling)
- `trace show <id>` for full call detail (supports 8-char UUID prefix)
- `trace stats` with `--breakdown` and `--endpoint` flags
- `trace clear` to reset database
- Provider auto-detection: OpenAI, Anthropic, Google Gemini, Mistral, Cohere, Together, Groq, Replicate, DeepSeek, Azure, Ollama, OpenRouter
- Model-aware cost estimation for 50+ models
- Streaming SSE passthrough with token/cost extraction
- Non-streaming response capture with usage parsing
- Content-Type detection for streaming vs non-streaming responses
- Upstream reachability check on startup (3s timeout, warn-only)
- `mpsc::channel(20_000)` background writer for zero-latency request path
- Graceful shutdown with 5s writer drain

[0.3.8]: https://github.com/jmamda/OpenTrace/compare/v0.3.7...HEAD
[0.3.7]: https://github.com/jmamda/OpenTrace/compare/v0.3.6...v0.3.7
[0.3.6]: https://github.com/jmamda/OpenTrace/compare/v0.3.5...v0.3.6
[0.3.5]: https://github.com/jmamda/OpenTrace/compare/v0.3.4...v0.3.5
[0.3.4]: https://github.com/jmamda/OpenTrace/compare/v0.3.3...v0.3.4
[0.3.3]: https://github.com/jmamda/OpenTrace/compare/v0.3.2...v0.3.3
[0.3.2]: https://github.com/jmamda/OpenTrace/compare/v0.3.1...v0.3.2
[0.3.1]: https://github.com/jmamda/OpenTrace/compare/v0.3.0...v0.3.1
[0.3.0]: https://github.com/jmamda/OpenTrace/compare/v0.2.9...v0.3.0
[0.2.9]: https://github.com/jmamda/OpenTrace/compare/v0.2.8...v0.2.9
[0.2.8]: https://github.com/jmamda/OpenTrace/compare/v0.27...v0.2.8
[0.27]: https://github.com/jmamda/OpenTrace/compare/v0.26...v0.27
[0.26]: https://github.com/jmamda/OpenTrace/compare/v0.25.0...v0.26
[0.25.0]: https://github.com/jmamda/OpenTrace/compare/v0.241...v0.25.0
[0.24.1]: https://github.com/jmamda/OpenTrace/compare/v0.23...v0.241
[0.24]: https://github.com/jmamda/OpenTrace/compare/v0.23...v0.241
[0.23]: https://github.com/jmamda/OpenTrace/compare/v0.21...v0.23
[0.21]: https://github.com/jmamda/OpenTrace/compare/v0.2.0...v0.21
[0.2.0]: https://github.com/jmamda/OpenTrace/compare/v0.1.0...v0.2.0
[0.1.0]: https://github.com/jmamda/OpenTrace/releases/tag/v0.1.0
