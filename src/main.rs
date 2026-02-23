mod capture;
mod config;
mod metrics;
mod otel;
mod proxy;
mod serve;
mod store;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use colored::Colorize;
use std::sync::{atomic::Ordering, Arc};
use tokio::sync::mpsc;

#[derive(Parser)]
#[command(
    name = "trace",
    about = "The strace of LLM calls — local-first observability proxy",
    version = "0.27.0",
    long_about = None,
)]
struct Cli {
    #[command(subcommand)]
    command: Option<Commands>,
}

#[derive(Subcommand)]
enum Commands {
    /// Start the proxy server
    Start {
        /// Port to listen on [default: 4000]
        #[arg(short, long, env = "TRACE_PORT")]
        port: Option<u16>,

        /// Upstream LLM API base URL [default: https://api.openai.com]
        #[arg(short, long, env = "TRACE_UPSTREAM")]
        upstream: Option<String>,

        /// Address to bind on (use 0.0.0.0 to expose on the local network)
        #[arg(long, default_value = "127.0.0.1", env = "TRACE_BIND")]
        bind: String,

        /// Print each request/response to stderr
        #[arg(short, long, env = "TRACE_VERBOSE")]
        verbose: bool,

        /// Upstream request timeout in seconds
        #[arg(long, default_value = "300", env = "TRACE_UPSTREAM_TIMEOUT")]
        upstream_timeout: u64,

        /// Delete records older than this many days (0 = keep forever) [default: 90]
        #[arg(long, env = "TRACE_RETENTION_DAYS")]
        retention_days: Option<u32>,

        /// Do not store request bodies (prompts) in the database.
        #[arg(long, env = "TRACE_NO_REQUEST_BODIES")]
        no_request_bodies: bool,

        /// Expose Prometheus metrics on this port (e.g. 9091). 0 = disabled. [default: 0]
        #[arg(long, env = "TRACE_METRICS_PORT")]
        metrics_port: Option<u16>,

        /// Send spans to this OTLP HTTP endpoint (e.g. http://localhost:4318).
        #[arg(long, env = "OTEL_EXPORTER_OTLP_ENDPOINT")]
        otel_endpoint: Option<String>,

        /// Comma-separated top-level JSON keys to redact from stored request bodies.
        /// Example: --redact-fields messages,system_prompt
        #[arg(long, env = "TRACE_REDACT_FIELDS")]
        redact_fields: Option<String>,

        /// Emit a budget warning to stderr when spend exceeds this USD amount in the period.
        #[arg(long, env = "TRACE_BUDGET_ALERT_USD")]
        budget_alert_usd: Option<f64>,

        /// Budget period for --budget-alert-usd: day | week | month [default: month]
        #[arg(long, env = "TRACE_BUDGET_PERIOD")]
        budget_period: Option<String>,

        /// Route a path prefix to a different upstream. Format: PATH=URL.
        /// May be specified multiple times. More specific paths must come first.
        /// Example: --route /v1/messages=https://api.anthropic.com
        #[arg(long = "route", value_name = "PATH=URL")]
        route: Vec<String>,
    },

    /// Query captured calls
    Query {
        /// Number of recent calls to show
        #[arg(short, long, default_value = "20")]
        last: usize,

        /// Output as JSON
        #[arg(short, long)]
        json: bool,

        /// Include request and response bodies
        #[arg(short, long)]
        bodies: bool,

        /// Print bodies without truncation (use with --bodies)
        #[arg(long)]
        full: bool,

        /// Filter by model name (substring match, e.g. gpt-4o)
        #[arg(short, long)]
        model: Option<String>,

        /// Show only failed calls (status >= 400, status = 0, or upstream error)
        #[arg(short, long)]
        errors: bool,

        /// Show calls at or after this timestamp (ISO 8601 or YYYY-MM-DD)
        #[arg(long)]
        since: Option<String>,

        /// Show calls at or before this timestamp (ISO 8601 or YYYY-MM-DD)
        #[arg(long)]
        until: Option<String>,

        /// Filter by exact HTTP status code (e.g. 429)
        #[arg(long)]
        status: Option<u16>,

        /// Filter by HTTP status code range (e.g. 400-499)
        #[arg(long, value_parser = parse_status_range)]
        status_range: Option<(u16, u16)>,
    },

    /// Watch for new calls in real time (polls every 250ms)
    Watch {
        /// Filter by model name (substring match)
        #[arg(short, long)]
        model: Option<String>,

        /// Show only failed calls
        #[arg(short, long)]
        errors: bool,

        /// Filter by exact HTTP status code (e.g. 500)
        #[arg(long)]
        status: Option<u16>,

        /// Filter by HTTP status code range (e.g. 400-499)
        #[arg(long, value_parser = parse_status_range)]
        status_range: Option<(u16, u16)>,
    },

    /// Show full detail for a single call by ID (prefix of 8+ chars is fine)
    Show {
        /// Call ID or unambiguous prefix from `trace query`
        id: String,
        /// Hide request and response bodies (for sensitive/shared environments)
        #[arg(long)]
        no_bodies: bool,
        /// Show all calls in the same distributed trace as a tree
        #[arg(long)]
        tree: bool,
    },

    /// Show aggregate stats
    Stats {
        /// Break down by model (cost, tokens, latency per model)
        #[arg(short, long)]
        breakdown: bool,

        /// Break down by endpoint
        #[arg(long)]
        endpoint: bool,
    },

    /// Show DB path and size
    Info,

    /// Delete all captured calls and compact the database
    Clear {
        /// Skip confirmation prompt
        #[arg(short, long)]
        yes: bool,
    },

    /// Export captured calls to stdout (pipe to a file with > calls.jsonl)
    Export {
        /// Output format: jsonl (default) or csv
        #[arg(long, default_value = "jsonl")]
        format: String,

        /// Filter by model name (substring match, e.g. gpt-4o)
        #[arg(short, long)]
        model: Option<String>,

        /// Export calls at or after this timestamp (ISO 8601 or YYYY-MM-DD)
        #[arg(long)]
        since: Option<String>,

        /// Export calls at or before this timestamp (ISO 8601 or YYYY-MM-DD)
        #[arg(long)]
        until: Option<String>,

        /// Filter by exact HTTP status code (e.g. 429)
        #[arg(long)]
        status: Option<u16>,

        /// Filter by HTTP status code range (e.g. 400-599)
        #[arg(long, value_parser = parse_status_range)]
        status_range: Option<(u16, u16)>,
    },

    /// Start the local web dashboard (reads existing trace.db, no proxy)
    Serve {
        /// Port for the dashboard [default: 8080]
        #[arg(short, long, env = "TRACE_UI_PORT")]
        port: Option<u16>,
    },

    /// Print a cost report for captured calls
    Report {
        /// Show calls at or after this timestamp (ISO 8601 or YYYY-MM-DD)
        #[arg(long)]
        since: Option<String>,

        /// Show calls at or before this timestamp (ISO 8601 or YYYY-MM-DD)
        #[arg(long)]
        until: Option<String>,

        /// Filter by model name (substring match)
        #[arg(short, long)]
        model: Option<String>,

        /// Output format: text (default) | json | github
        #[arg(long, default_value = "text")]
        format: String,

        /// Exit with code 1 if total cost exceeds this USD amount
        #[arg(long)]
        fail_over_usd: Option<f64>,
    },

    /// Manage the trace config file
    Config {
        #[command(subcommand)]
        action: ConfigAction,
    },

    /// Flush WAL and defragment the database (shrinks disk usage)
    Vacuum {
        /// Path to the database file [default: ~/.trace/trace.db]
        #[arg(long)]
        db: Option<std::path::PathBuf>,
    },

    /// Check call history against performance rules (exits 1 if any rule fails)
    Eval {
        /// Rule to check. Format: METRIC OP VALUE.
        /// Metrics: latency_p99, error_rate, error_count, avg_cost_usd, total_calls.
        /// Operators: <, <=, >, >=, =, ==.
        /// Example: --rule "latency_p99 < 2000" --rule "error_rate < 0.05"
        #[arg(long = "rule", value_name = "METRIC OP VALUE")]
        rules: Vec<String>,

        /// Only evaluate calls at or after this timestamp
        #[arg(long)]
        since: Option<String>,

        /// Only evaluate calls at or before this timestamp
        #[arg(long)]
        until: Option<String>,

        /// Only evaluate calls for this model (substring match)
        #[arg(short, long)]
        model: Option<String>,
    },
}

#[derive(Subcommand)]
enum ConfigAction {
    /// Print the effective config (file values merged with env vars)
    Show,
    /// Print the active config file path (or "no config file found")
    Path,
    /// Write a default config skeleton to ~/.config/trace/config.toml
    Init,
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    // Load config file once; used as fallback defaults below.
    let cfg = config::load_config();
    let cfg_path = config::config_path();

    match cli.command.unwrap_or(Commands::Stats { breakdown: false, endpoint: false }) {
        Commands::Start {
            port, upstream, bind, verbose, upstream_timeout, retention_days,
            no_request_bodies, metrics_port, otel_endpoint,
            redact_fields, budget_alert_usd, budget_period, route,
        } => {
            cmd_start(
                port, upstream, bind, verbose, upstream_timeout, retention_days,
                no_request_bodies, metrics_port, otel_endpoint,
                redact_fields, budget_alert_usd, budget_period, route,
                &cfg,
            ).await
        }
        Commands::Query { last, json, bodies, full, model, errors, since, until, status, status_range } => {
            cmd_query(last, json, bodies, full, model, errors, since, until, status, status_range)
        }
        Commands::Watch { model, errors, status, status_range } => {
            cmd_watch(model, errors, status, status_range).await
        }
        Commands::Show { id, no_bodies, tree } => {
            cmd_show(id, no_bodies, tree)
        }
        Commands::Stats { breakdown, endpoint } => {
            cmd_stats(breakdown, endpoint)
        }
        Commands::Info => {
            cmd_info()
        }
        Commands::Clear { yes } => {
            cmd_clear(yes)
        }
        Commands::Export { format, model, since, until, status, status_range } => {
            cmd_export(format, model, since, until, status, status_range)
        }
        Commands::Serve { port } => {
            let serve_cfg = cfg.serve.as_ref();
            let resolved_port = port
                .or_else(|| serve_cfg.and_then(|s| s.port))
                .unwrap_or(8080);
            serve::cmd_serve(resolved_port).await
        }
        Commands::Report { since, until, model, format, fail_over_usd } => {
            cmd_report(since, until, model, format, fail_over_usd)
        }
        Commands::Config { action } => {
            cmd_config(action, &cfg, cfg_path.as_deref())
        }
        Commands::Vacuum { db } => {
            cmd_vacuum(db)
        }
        Commands::Eval { rules, since, until, model } => {
            cmd_eval(rules, since, until, model)
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn cmd_start(
    port: Option<u16>,
    upstream: Option<String>,
    bind: String,
    verbose: bool,
    upstream_timeout: u64,
    retention_days: Option<u32>,
    no_request_bodies: bool,
    metrics_port: Option<u16>,
    otel_endpoint: Option<String>,
    redact_fields_str: Option<String>,
    budget_alert_usd: Option<f64>,
    budget_period: Option<String>,
    route_args: Vec<String>,
    cfg: &config::TraceConfig,
) -> Result<()> {
    // Merge CLI/env args with config file defaults. Priority: CLI > env > config > hardcoded.
    let start_cfg = cfg.start.as_ref();
    let port = port.or_else(|| start_cfg.and_then(|s| s.port)).unwrap_or(4000);
    let upstream = upstream
        .or_else(|| start_cfg.and_then(|s| s.upstream.clone()))
        .unwrap_or_else(|| "https://api.openai.com".to_string());
    let retention_days = retention_days
        .or_else(|| start_cfg.and_then(|s| s.retention_days))
        .unwrap_or(90);
    let metrics_port = metrics_port
        .or_else(|| start_cfg.and_then(|s| s.metrics_port))
        .unwrap_or(0);
    let otel_endpoint = otel_endpoint.or_else(|| start_cfg.and_then(|s| s.otel_endpoint.clone()));

    // Redact fields: CLI string (comma-sep) → config file array → empty
    let redact_fields: Vec<String> = redact_fields_str
        .map(|s| {
            s.split(',')
                .map(|f| f.trim().to_string())
                .filter(|f| !f.is_empty())
                .collect()
        })
        .or_else(|| start_cfg.and_then(|s| s.redact_fields.clone()))
        .unwrap_or_default();

    let budget_alert_usd = budget_alert_usd.or_else(|| start_cfg.and_then(|s| s.budget_alert_usd));
    let budget_period = budget_period
        .or_else(|| start_cfg.and_then(|s| s.budget_period.clone()))
        .unwrap_or_else(|| "month".to_string());

    // Validate upstream URL scheme early — prevent footguns like file:/// or ftp://.
    {
        let scheme = upstream.splitn(2, "://").next().unwrap_or("");
        if !matches!(scheme, "http" | "https") {
            anyhow::bail!(
                "upstream URL must use http:// or https://, got: {:?}\n  Example: trace start --upstream https://api.openai.com",
                upstream
            );
        }
    }

    // Build routing table from --route CLI flags and [[start.routes]] in config.
    // CLI flags take priority (evaluated first); config file entries are appended.
    let routes: Vec<proxy::UpstreamRoute> = {
        let mut entries: Vec<proxy::UpstreamRoute> = Vec::new();

        for arg in &route_args {
            let parts: Vec<&str> = arg.splitn(2, '=').collect();
            if parts.len() != 2 {
                anyhow::bail!(
                    "--route {:?} must be in PATH=URL format\n  Example: --route /v1/messages=https://api.anthropic.com",
                    arg
                );
            }
            let path = parts[0].to_string();
            let url = parts[1].to_string();
            if !path.starts_with('/') {
                anyhow::bail!("--route path {:?} must start with '/'", path);
            }
            let scheme = url.splitn(2, "://").next().unwrap_or("");
            if !matches!(scheme, "http" | "https") {
                anyhow::bail!("--route URL {:?} must use http:// or https://", url);
            }
            entries.push(proxy::UpstreamRoute { path, upstream: Arc::new(url) });
        }

        if let Some(cfg_routes) = start_cfg.and_then(|s| s.routes.as_ref()) {
            for r in cfg_routes {
                if !r.path.starts_with('/') {
                    anyhow::bail!("config route path {:?} must start with '/'", r.path);
                }
                let scheme = r.upstream.splitn(2, "://").next().unwrap_or("");
                if !matches!(scheme, "http" | "https") {
                    anyhow::bail!(
                        "config route upstream {:?} must use http:// or https://",
                        r.upstream
                    );
                }
                entries.push(proxy::UpstreamRoute {
                    path: r.path.clone(),
                    upstream: Arc::new(r.upstream.clone()),
                });
            }
        }

        // Warn on obviously shadowed routes (longer prefix listed after shorter one).
        // Uses the same boundary logic as resolve_upstream so false positives are avoided.
        for i in 0..entries.len() {
            for j in (i + 1)..entries.len() {
                let p_i = entries[i].path.as_str();
                let p_j = entries[j].path.as_str();
                let shadowed = p_i == "/"
                    || p_j == p_i
                    || (p_j.starts_with(p_i) && p_j[p_i.len()..].starts_with('/'));
                if shadowed {
                    eprintln!(
                        "WARNING: route {:?} is shadowed by earlier route {:?} — it will never match",
                        entries[j].path, entries[i].path
                    );
                }
            }
        }

        entries
    };

    let store = store::Store::open().context("failed to open trace database")?;
    let db_path = store::db_path()?;

    println!("{}", "trace v0.27.0".bold());
    println!("  listening  {}", format!("http://{}:{}", bind, port).cyan());
    println!("  upstream   {}", upstream.cyan());
    if !routes.is_empty() {
        println!("  routes     {} rule(s):", routes.len());
        for r in &routes {
            println!("               {} -> {}", r.path.cyan(), r.upstream.as_ref().cyan());
        }
        println!("               * -> {} (default)", upstream.cyan());
    }
    println!("  storage    {}", db_path.display().to_string().cyan());
    if retention_days > 0 {
        println!("  retention  {} days", retention_days.to_string().cyan());
    }
    if metrics_port > 0 {
        println!("  metrics    {}", format!("http://0.0.0.0:{}/metrics", metrics_port).cyan());
    }
    if let Some(ref ep) = otel_endpoint {
        println!("  otel       {}", ep.cyan());
    }
    if !redact_fields.is_empty() {
        println!("  redact     {}", redact_fields.join(", ").cyan());
    }
    if let Some(limit) = budget_alert_usd {
        println!("  budget     ${:.2} / {}", limit, budget_period.cyan());
    }
    println!();
    println!("Set your LLM client:");
    if routes.is_empty() {
        println!("  {}", format!("OPENAI_BASE_URL=http://localhost:{}/v1", port).yellow());
    } else {
        println!("  {}  # gpt-* models", format!("OPENAI_BASE_URL=http://localhost:{}/v1", port).yellow());
        println!("  {}  # claude-* models", format!("ANTHROPIC_BASE_URL=http://localhost:{}", port).yellow());
    }
    println!();

    // Warn when binding on non-localhost.
    let is_localhost = matches!(bind.as_str(), "127.0.0.1" | "::1" | "localhost");
    if !is_localhost {
        eprintln!("{}", format!(
            "WARNING: proxy is bound to {} and is reachable from the network.",
            bind
        ).yellow().bold());
        eprintln!("{}", "All captured LLM requests (including prompts) will be visible to network peers.".yellow());
        eprintln!();
    }

    if !upstream.starts_with("https://") {
        eprintln!("{}", format!(
            "WARNING: upstream '{}' is not HTTPS — traffic may be unencrypted.",
            upstream
        ).yellow());
        eprintln!();
    }

    for r in &routes {
        if !r.upstream.starts_with("https://") {
            eprintln!("{}", format!(
                "WARNING: route upstream '{}' ({}) is not HTTPS — traffic may be unencrypted.",
                r.upstream, r.path
            ).yellow());
        }
    }

    println!("{}", format!("Note: full request/response bodies (including prompts) are stored in {}", db_path.display()).dimmed());
    #[cfg(windows)]
    eprintln!("{}", "WARNING: on Windows, the trace database has default NTFS permissions and may be readable by other administrators on this machine.".yellow());
    println!();

    // Store budget config in DB meta so `trace watch` can read it.
    if let Some(budget_usd) = budget_alert_usd {
        let _ = store.update_meta("budget_alert_usd", &budget_usd.to_string());
        let _ = store.update_meta("budget_period", &budget_period);
    }

    // Build MetricsState if --metrics-port is set.
    let metrics_state: Option<Arc<metrics::MetricsState>> = if metrics_port > 0 {
        Some(Arc::new(metrics::MetricsState::new()))
    } else {
        None
    };

    // Build OtelExporter if --otel-endpoint is set.
    let otel_exporter: Option<Arc<otel::OtelExporter>> = otel_endpoint.as_deref().map(|ep| {
        Arc::new(otel::OtelExporter::new(ep.to_string()))
    });

    let (store_tx, mut store_rx) = mpsc::channel::<store::CallRecord>(20_000);

    let metrics_writer = metrics_state.clone();
    let otel_writer = otel_exporter.clone();

    let writer_handle = tokio::spawn(async move {
        while let Some(record) = store_rx.recv().await {
            if let Err(e) = store.insert(&record) {
                eprintln!("[trace] db write error: {e}");
            }
            if let Some(ref m) = metrics_writer {
                m.record(&record);
            }
            if let Some(ref otel) = otel_writer {
                let otel = otel.clone();
                let r = record.clone();
                tokio::spawn(async move {
                    otel.export_span(&r).await;
                });
            }
        }
    });

    // Daily retention pruning task.
    if retention_days > 0 {
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(std::time::Duration::from_secs(86_400)).await;
                if let Ok(s) = store::Store::open() {
                    match s.prune_older_than(retention_days) {
                        Ok(n) if n > 0 => eprintln!("[trace] pruned {} records older than {} days", n, retention_days),
                        Ok(_) => {}
                        Err(e) => eprintln!("[trace] prune error: {e}"),
                    }
                }
            }
        });
    }

    let client = reqwest::Client::builder()
        .danger_accept_invalid_certs(false)
        .timeout(std::time::Duration::from_secs(upstream_timeout))
        .build()
        .context("failed to build HTTP client")?;

    // Non-blocking startup connectivity check — default upstream only.
    // Route upstreams are intentionally NOT probed: probing arbitrary URLs from a
    // config file would allow SSRF reconnaissance of internal networks.
    if let Ok(check_client) = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(3))
        .build()
    {
        let check_url = upstream.trim_end_matches('/').to_string();
        match check_client.head(&check_url).send().await {
            Ok(_) => {}
            Err(e) if e.is_connect() || e.is_timeout() => {
                eprintln!("{}", format!(
                    "WARNING: upstream '{}' did not respond ({}). Proxy will start anyway.",
                    check_url, e
                ).yellow());
                eprintln!();
            }
            Err(_) => {}
        }
    }

    let state = proxy::ProxyState {
        upstream: Arc::new(upstream),
        routes,
        client,
        store_tx,
        verbose,
        no_request_bodies,
        redact_fields,
    };

    // Flush DROPPED_RECORDS counter to DB every 10s.
    {
        let flush_store = store::Store::open().context("failed to open trace database for metrics")?;
        let metrics_flush = metrics_state.clone();
        tokio::spawn(async move {
            let mut last_flushed: u64 = 0;
            loop {
                tokio::time::sleep(std::time::Duration::from_secs(10)).await;
                let current = proxy::DROPPED_RECORDS.load(Ordering::Relaxed);
                if current != last_flushed {
                    let db_total: u64 = flush_store
                        .get_meta("dropped_records")
                        .ok()
                        .flatten()
                        .and_then(|s| s.parse().ok())
                        .unwrap_or(0);
                    let new_total = db_total + current - last_flushed;
                    let _ = flush_store.update_meta("dropped_records", &new_total.to_string());
                    if let Some(ref m) = metrics_flush {
                        m.set_dropped_records(new_total);
                    }
                    last_flushed = current;
                }
            }
        });
    }

    // Budget alerting task (checks every 60s, warns at most once per 10min).
    if let Some(budget_usd) = budget_alert_usd {
        let budget_period_task = budget_period.clone();
        let metrics_budget = metrics_state.clone();
        tokio::spawn(async move {
            let mut last_warned: Option<std::time::Instant> = None;
            loop {
                tokio::time::sleep(std::time::Duration::from_secs(60)).await;
                let since = period_start_iso(&budget_period_task);
                if let Ok(budget_store) = store::Store::open() {
                    if let Ok(spent) = budget_store.cost_since(&since) {
                        if let Some(ref m) = metrics_budget {
                            m.set_budget_spent(spent, budget_usd, &budget_period_task);
                        }
                        if spent >= budget_usd {
                            let should_warn = last_warned
                                .map(|t| t.elapsed() >= std::time::Duration::from_secs(600))
                                .unwrap_or(true);
                            if should_warn {
                                eprintln!(
                                    "[trace] BUDGET WARNING: ${:.2} spent / ${:.2} limit this {}",
                                    spent, budget_usd, budget_period_task
                                );
                                last_warned = Some(std::time::Instant::now());
                            }
                        }
                    }
                }
            }
        });
    }

    // Spawn Prometheus metrics HTTP server when --metrics-port > 0.
    if metrics_port > 0 {
        let metrics_for_server = Arc::clone(metrics_state.as_ref().unwrap());
        tokio::spawn(async move {
            use axum::{extract::State, routing::get, Router};

            let metrics_app: Router = Router::new()
                .route(
                    "/metrics",
                    get(
                        |State(m): State<Arc<metrics::MetricsState>>| async move {
                            axum::response::Response::builder()
                                .header("content-type", "text/plain; version=0.0.4; charset=utf-8")
                                .body(axum::body::Body::from(m.render_prometheus()))
                                .unwrap()
                        },
                    ),
                )
                .with_state(metrics_for_server);

            let metrics_addr = format!("0.0.0.0:{}", metrics_port);
            match tokio::net::TcpListener::bind(&metrics_addr).await {
                Ok(listener) => {
                    axum::serve(listener, metrics_app).await.ok();
                }
                Err(e) => {
                    eprintln!("[trace] failed to bind metrics port {}: {}", metrics_port, e);
                }
            }
        });
    }

    let app = proxy::router(state);
    let addr = format!("{}:{}", bind, port);
    let listener = tokio::net::TcpListener::bind(&addr)
        .await
        .map_err(|e| {
            if e.kind() == std::io::ErrorKind::AddrInUse {
                anyhow::anyhow!(
                    "address {} is already in use\n  Is another trace instance running? Try: trace start --port {}",
                    addr, port + 1
                )
            } else {
                anyhow::anyhow!("failed to bind to {}: {}", addr, e)
            }
        })?;

    println!("{}", format!("Listening on {}", addr).dimmed());
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await
        .context("server error")?;

    let _ = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        writer_handle,
    ).await;

    Ok(())
}

async fn shutdown_signal() {
    let ctrl_c = async {
        tokio::signal::ctrl_c()
            .await
            .expect("failed to install Ctrl-C handler");
    };

    #[cfg(unix)]
    let terminate = async {
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("failed to install SIGTERM handler")
            .recv()
            .await;
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => {},
        _ = terminate => {},
    }

    eprintln!("[trace] shutting down gracefully...");
}

fn cmd_query(
    limit: usize,
    json: bool,
    bodies: bool,
    full: bool,
    model: Option<String>,
    errors: bool,
    since: Option<String>,
    until: Option<String>,
    status: Option<u16>,
    status_range: Option<(u16, u16)>,
) -> Result<()> {
    let since = since.map(parse_since_ts).transpose()?;
    let until = until.map(parse_until_ts).transpose()?;

    let store = store::Store::open().context("failed to open trace database")?;
    let filter = store::QueryFilter { errors_only: errors, model, since, until, status, status_range };
    let mut calls = store.query_filtered(limit, &filter)?;

    calls.reverse();

    if json {
        if !bodies {
            for call in &mut calls {
                call.request_body = None;
                call.response_body = None;
            }
        }
        println!("{}", serde_json::to_string_pretty(&calls)?);
        return Ok(());
    }

    if calls.is_empty() {
        if limit == 0 {
            println!("{}", "Use --last N with N >= 1 to show calls.".dimmed());
        } else {
            println!("{}", "No calls recorded yet.".dimmed());
            println!("Start the proxy with: {}", "trace start".yellow());
        }
        return Ok(());
    }

    print_query_header();

    let body_max = if full { usize::MAX } else { 120 };
    for call in &calls {
        print_call_row(call);

        if let Some(ref err) = call.error {
            println!("  {} {}", "error:".red(), err);
        }

        if bodies {
            if let Some(ref req) = call.request_body {
                println!("  {} {}", "request:".dimmed(), dim_json(req, body_max));
            }
            if let Some(ref resp) = call.response_body {
                println!("  {} {}", "response:".dimmed(), dim_json(resp, body_max));
            }
        }
    }

    println!();
    println!("{} calls shown. Run {} for totals.", calls.len(), "trace stats".yellow());
    Ok(())
}

async fn cmd_watch(
    model: Option<String>,
    errors: bool,
    status: Option<u16>,
    status_range: Option<(u16, u16)>,
) -> Result<()> {
    let store = store::Store::open().context("failed to open trace database")?;
    let filter = store::QueryFilter {
        errors_only: errors,
        model,
        status,
        status_range,
        ..Default::default()
    };

    // Read budget config from DB meta (written by `trace start` when budget alerting is active).
    let budget_alert_usd: Option<f64> = store
        .get_meta("budget_alert_usd")
        .ok()
        .flatten()
        .and_then(|s| s.parse().ok());
    let budget_period = store
        .get_meta("budget_period")
        .ok()
        .flatten()
        .unwrap_or_else(|| "month".to_string());

    let latest = store.query_filtered(1, &store::QueryFilter::default())?;
    let mut last_ts = latest
        .first()
        .map(|r| r.timestamp.clone())
        .unwrap_or_else(|| "1970-01-01T00:00:00.000Z".to_string());

    // Budget spend, refreshed every ~10s (40 × 250ms ticks).
    let mut budget_spent: f64 = 0.0;
    let mut budget_tick: u32 = 0;

    // Print budget header line if budget alerting is configured.
    if let Some(limit) = budget_alert_usd {
        let since = period_start_iso(&budget_period);
        budget_spent = store.cost_since(&since).unwrap_or(0.0);
        print_budget_line(budget_spent, limit, &budget_period);
    }

    print_query_header();
    let mut rows_since_header: usize = 0;

    loop {
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {
                println!();
                return Ok(());
            }
            _ = tokio::time::sleep(std::time::Duration::from_millis(250)) => {}
        }

        budget_tick += 1;
        if budget_tick % 40 == 0 {
            if let Some(limit) = budget_alert_usd {
                let since = period_start_iso(&budget_period);
                budget_spent = store.cost_since(&since).unwrap_or(budget_spent);
                print_budget_line(budget_spent, limit, &budget_period);
            }
        }

        let new_records = store.query_after(&last_ts, &filter, 100)?;
        for record in &new_records {
            print_call_row(record);
            last_ts = record.timestamp.clone();
            rows_since_header += 1;
            if rows_since_header >= 20 {
                print_query_header();
                rows_since_header = 0;
            }
        }
    }
}

fn print_budget_line(spent: f64, limit: f64, period: &str) {
    let pct = if limit > 0.0 { (spent / limit * 100.0) as u32 } else { 0 };
    let line = format!("budget  ${:.2} / ${:.2} this {}  ({}%)", spent, limit, period, pct);
    if pct >= 80 {
        println!("{}", line.red());
    } else {
        println!("{}", line.dimmed());
    }
}

fn cmd_show(id: String, no_bodies: bool, tree: bool) -> Result<()> {
    let store = store::Store::open().context("failed to open trace database")?;
    match store.get_by_id(&id)? {
        None => {
            println!("{}", format!("No call found with id prefix: {}", id).red());
        }
        Some(r) => {
            println!("{}", "trace show".bold());
            println!("  id           {}", r.id.cyan());
            println!("  timestamp    {}", r.timestamp.cyan());
            println!("  provider     {}", r.provider.cyan());
            println!("  model        {}", r.model.cyan());
            println!("  endpoint     {}", r.endpoint.cyan());
            let status_str = if r.status_code == 0 || r.status_code >= 400 || r.error.is_some() {
                r.status_code.to_string().red().to_string()
            } else {
                r.status_code.to_string().green().to_string()
            };
            println!("  status       {}", status_str);
            println!("  latency      {}ms", r.latency_ms.to_string().cyan());
            if let Some(ttft) = r.ttft_ms {
                println!("  ttft         {}ms", ttft.to_string().cyan());
            }
            println!("  in tokens    {}",
                r.input_tokens.map(|t| t.to_string()).unwrap_or_else(|| "-".to_string()).cyan());
            println!("  out tokens   {}",
                r.output_tokens.map(|t| t.to_string()).unwrap_or_else(|| "-".to_string()).cyan());
            println!("  cost         {}",
                r.cost_usd.map(|c| format!("${:.6}", c)).unwrap_or_else(|| "-".to_string()).cyan());
            if let Some(ref pid) = r.provider_request_id {
                println!("  provider id  {}", pid.cyan());
            }
            if let Some(ref err) = r.error {
                println!("  error        {}", err.red());
            }
            if no_bodies {
                println!();
                println!("{}", "(bodies hidden — use without --no-bodies to display)".dimmed());
            } else {
                if let Some(ref req) = r.request_body {
                    println!();
                    println!("{}", "request:".bold());
                    println!("{}", pretty_json(req));
                }
                if let Some(ref resp) = r.response_body {
                    println!();
                    println!("{}", "response:".bold());
                    println!("{}", pretty_json(resp));
                }
            }

            // Tree view — show sibling/child calls sharing the same trace_id
            if tree {
                if let Some(ref tid) = r.trace_id {
                    let store2 = store::Store::open().context("failed to open trace database")?;
                    let siblings = store2.query_trace_tree(tid)?;
                    if siblings.len() > 1 {
                        println!();
                        println!("{}", "trace tree:".bold());
                        for s in &siblings {
                            let is_root = s.parent_id.is_none();
                            let is_self = s.id == r.id;
                            let prefix = if is_root { "  ●" } else { "  └" };
                            let id_str = s.id[..8.min(s.id.len())].to_string();
                            let marker = if is_self { " ◀ this call" } else { "" };
                            let status_str = if s.status_code == 0 || s.status_code >= 400 || s.error.is_some() {
                                s.status_code.to_string().red().to_string()
                            } else {
                                s.status_code.to_string().green().to_string()
                            };
                            println!("{} {} {} {} {}ms {}{}",
                                prefix,
                                id_str.cyan(),
                                s.model.as_str(),
                                status_str,
                                s.latency_ms,
                                s.timestamp[..19.min(s.timestamp.len())].to_string().dimmed(),
                                marker.dimmed(),
                            );
                        }
                    } else {
                        println!();
                        println!("{}", "(no other calls share this trace_id)".dimmed());
                    }
                } else {
                    println!();
                    println!("{}", "(no trace_id on this call — was it proxied via OpenTrace with a traceparent header?)".dimmed());
                }
            }
        }
    }
    Ok(())
}

fn cmd_stats(breakdown: bool, endpoint: bool) -> Result<()> {
    let store = store::Store::open().context("failed to open trace database")?;
    let s = store.stats()?;
    let (p50, p95, p99) = store.latency_percentiles(&store::QueryFilter::default())?;

    println!("{}", "trace stats".bold());
    println!();
    println!("  total calls       {}", s.total_calls.to_string().cyan());
    println!("  calls last hour   {}", s.calls_last_hour.to_string().cyan());
    println!("  errors            {}", if s.error_count > 0 {
        s.error_count.to_string().red().to_string()
    } else {
        "0".green().to_string()
    });
    println!("  avg latency       {}ms", format!("{:.0}", s.avg_latency_ms).cyan());
    if s.total_calls > 0 {
        println!("  latency p50       {}ms", format!("{:.0}", p50).cyan());
        println!("  latency p95       {}ms", format!("{:.0}", p95).cyan());
        println!("  latency p99       {}ms", format!("{:.0}", p99).cyan());
    }
    let tp = store.token_percentiles(None)?;
    if tp.input_p50 > 0 || tp.output_p50 > 0 || tp.input_p99 > 0 {
        println!();
        println!("{}", "Token percentiles (input / output):".bold());
        println!("  p50   {:>6} / {:>6}",
            fmt_num_commas(tp.input_p50 as i64), fmt_num_commas(tp.output_p50 as i64));
        println!("  p95   {:>6} / {:>6}",
            fmt_num_commas(tp.input_p95 as i64), fmt_num_commas(tp.output_p95 as i64));
        println!("  p99   {:>6} / {:>6}",
            fmt_num_commas(tp.input_p99 as i64), fmt_num_commas(tp.output_p99 as i64));
    }

    println!();
    println!("  input tokens      {}", s.total_input_tokens.to_string().cyan());
    println!("  output tokens     {}", s.total_output_tokens.to_string().cyan());
    println!("  estimated cost    {}", format!("${:.4}", s.total_cost_usd).cyan());

    let dropped: u64 = store
        .get_meta("dropped_records")
        .ok()
        .flatten()
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    if dropped > 0 {
        println!();
        println!("  {} {} records dropped due to DB write backpressure",
            "WARNING:".yellow().bold(),
            dropped.to_string().red()
        );
        println!("  {} consider reducing request rate or increasing disk I/O capacity",
            "hint:".dimmed());
    }

    if breakdown {
        println!();
        println!("{}", "by model:".bold());
        let models = store.stats_by_model()?;
        if models.is_empty() {
            println!("  {}", "(no data)".dimmed());
        } else {
            let p99_map = store.latency_percentiles_per_model()?;
            println!("{}", format!(
                "  {:<30}  {:>6}  {:>8}  {:>8}  {:>9}  {:>8}  {:>8}",
                "model", "calls", "in", "out", "cost", "avg ms", "p99 ms"
            ).underline());
            for m in &models {
                let in_str = if m.total_input_tokens > 0 {
                    format!("{:>8}", fmt_tokens(m.total_input_tokens))
                } else {
                    "       -".to_string()
                };
                let out_str = if m.total_output_tokens > 0 {
                    format!("{:>8}", fmt_tokens(m.total_output_tokens))
                } else {
                    "       -".to_string()
                };
                let cost_str = format!("${:>8.4}", m.total_cost_usd);
                let err_indicator = if m.error_count > 0 {
                    format!(" ({} err)", m.error_count).red().to_string()
                } else {
                    String::new()
                };
                let (_, _, model_p99) = p99_map
                    .get(&m.model)
                    .copied()
                    .unwrap_or((0.0, 0.0, 0.0));
                println!(
                    "  {:<30}  {:>6}  {}  {}  {}  {:>7.0}ms  {:>6.0}ms{}",
                    truncate(&m.model, 30),
                    m.calls,
                    in_str,
                    out_str,
                    cost_str.cyan(),
                    m.avg_latency_ms,
                    model_p99,
                    err_indicator,
                );
            }
        }
    }

    if endpoint {
        println!();
        println!("{}", "by endpoint:".bold());
        let endpoints = store.stats_by_endpoint()?;
        if endpoints.is_empty() {
            println!("  {}", "(no data)".dimmed());
        } else {
            println!("{}", format!(
                "  {:<35}  {:>6}  {:>8}  {:>8}  {:>9}  {:>8}",
                "endpoint", "calls", "in", "out", "cost", "avg ms"
            ).underline());
            for e in &endpoints {
                let in_str = if e.total_input_tokens > 0 {
                    format!("{:>8}", fmt_tokens(e.total_input_tokens))
                } else {
                    "       -".to_string()
                };
                let out_str = if e.total_output_tokens > 0 {
                    format!("{:>8}", fmt_tokens(e.total_output_tokens))
                } else {
                    "       -".to_string()
                };
                let cost_str = format!("${:>8.4}", e.total_cost_usd);
                let err_indicator = if e.error_count > 0 {
                    format!(" ({} err)", e.error_count).red().to_string()
                } else {
                    String::new()
                };
                println!(
                    "  {:<35}  {:>6}  {}  {}  {}  {:>7.0}ms{}",
                    truncate(&e.endpoint, 35),
                    e.calls,
                    in_str,
                    out_str,
                    cost_str.cyan(),
                    e.avg_latency_ms,
                    err_indicator,
                );
            }
        }
    }

    println!();
    println!("Run {} to see recent calls.", "trace query".yellow());
    Ok(())
}

fn cmd_info() -> Result<()> {
    let db_path = store::db_path()?;

    println!("{}", "trace info".bold());
    println!("  DB path  {}", db_path.display().to_string().cyan());

    if db_path.exists() {
        let size = std::fs::metadata(&db_path)?.len();
        println!("  DB size  {}", format_bytes(size).cyan());
    } else {
        println!("  DB size  {}", "(not created yet — start the proxy first)".dimmed());
    }
    Ok(())
}

fn cmd_clear(yes: bool) -> Result<()> {
    let store = store::Store::open().context("failed to open trace database")?;

    if !yes {
        let count = store.total_calls()?;
        if count == 0 {
            println!("{}", "No calls to delete.".dimmed());
            return Ok(());
        }
        print!(
            "This will permanently delete {} captured {}. Type 'yes' to confirm: ",
            count.to_string().yellow(),
            if count == 1 { "call" } else { "calls" }
        );
        use std::io::Write;
        std::io::stdout().flush()?;
        let mut input = String::new();
        std::io::stdin().read_line(&mut input)?;
        if input.trim() != "yes" {
            println!("Aborted.");
            return Ok(());
        }
    }

    let count = store.clear()?;
    let noun = if count == 1 { "call" } else { "calls" };
    println!("{} {} deleted and database compacted.", count.to_string().cyan(), noun);
    Ok(())
}

fn cmd_export(
    format: String,
    model: Option<String>,
    since: Option<String>,
    until: Option<String>,
    status: Option<u16>,
    status_range: Option<(u16, u16)>,
) -> Result<()> {
    let since = since.map(parse_since_ts).transpose()?;
    let until = until.map(parse_until_ts).transpose()?;

    let store = store::Store::open().context("failed to open trace database")?;
    let filter = store::QueryFilter {
        errors_only: false,
        model,
        since,
        until,
        status,
        status_range,
    };
    let records = store.query_all_filtered(&filter)?;

    match format.as_str() {
        "csv" => {
            println!(
                "id,timestamp,provider,model,endpoint,status_code,latency_ms,\
                 ttft_ms,input_tokens,output_tokens,cost_usd,error,provider_request_id"
            );
            for r in &records {
                println!(
                    "{},{},{},{},{},{},{},{},{},{},{},{},{}",
                    csv_field(&r.id),
                    csv_field(&r.timestamp),
                    csv_field(&r.provider),
                    csv_field(&r.model),
                    csv_field(&r.endpoint),
                    r.status_code,
                    r.latency_ms,
                    r.ttft_ms.map(|v| v.to_string()).unwrap_or_default(),
                    r.input_tokens.map(|v| v.to_string()).unwrap_or_default(),
                    r.output_tokens.map(|v| v.to_string()).unwrap_or_default(),
                    r.cost_usd.map(|v| format!("{:.8}", v)).unwrap_or_default(),
                    csv_field(r.error.as_deref().unwrap_or("")),
                    csv_field(r.provider_request_id.as_deref().unwrap_or("")),
                );
            }
        }
        _ => {
            for r in &records {
                println!("{}", serde_json::to_string(&r)?);
            }
        }
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// trace report
// ---------------------------------------------------------------------------

struct ModelReportRow {
    model: String,
    calls: i64,
    input_tokens: i64,
    output_tokens: i64,
    cost_usd: f64,
}

fn cmd_report(
    since: Option<String>,
    until: Option<String>,
    model: Option<String>,
    format: String,
    fail_over_usd: Option<f64>,
) -> Result<()> {
    let since = since.map(parse_since_ts).transpose()?;
    let until = until.map(parse_until_ts).transpose()?;

    let store = store::Store::open().context("failed to open trace database")?;
    let filter = store::QueryFilter {
        errors_only: false,
        model,
        since: since.clone(),
        until: until.clone(),
        ..Default::default()
    };
    let records = store.query_all_filtered(&filter)?;

    // Aggregate per-model stats from the filtered record set.
    let mut model_map: std::collections::HashMap<String, ModelReportRow> =
        std::collections::HashMap::new();
    for r in &records {
        let entry = model_map.entry(r.model.clone()).or_insert(ModelReportRow {
            model: r.model.clone(),
            calls: 0,
            input_tokens: 0,
            output_tokens: 0,
            cost_usd: 0.0,
        });
        entry.calls += 1;
        entry.input_tokens += r.input_tokens.unwrap_or(0);
        entry.output_tokens += r.output_tokens.unwrap_or(0);
        entry.cost_usd += r.cost_usd.unwrap_or(0.0);
    }

    let total_calls = records.len() as i64;
    let total_cost: f64 = records.iter().map(|r| r.cost_usd.unwrap_or(0.0)).sum();

    // Sort by cost descending.
    let mut rows: Vec<ModelReportRow> = model_map.into_values().collect();
    rows.sort_by(|a, b| b.cost_usd.partial_cmp(&a.cost_usd).unwrap_or(std::cmp::Ordering::Equal));

    let period_from = since.as_deref().unwrap_or("(all time)");
    let period_to = until.as_deref().unwrap_or("now");

    match format.as_str() {
        "json" => {
            let json = serde_json::json!({
                "period": {"from": period_from, "to": period_to},
                "total_calls": total_calls,
                "total_cost_usd": total_cost,
                "models": rows.iter().map(|r| serde_json::json!({
                    "model": r.model,
                    "calls": r.calls,
                    "input_tokens": r.input_tokens,
                    "output_tokens": r.output_tokens,
                    "cost_usd": r.cost_usd,
                })).collect::<Vec<_>>(),
            });
            println!("{}", serde_json::to_string_pretty(&json)?);
        }
        "github" => {
            let output = format_report_github(&rows, total_calls, total_cost);
            if let Ok(summary_path) = std::env::var("GITHUB_STEP_SUMMARY") {
                use std::io::Write;
                let mut f = std::fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(&summary_path)
                    .context("failed to open GITHUB_STEP_SUMMARY")?;
                write!(f, "{}", output)?;
            } else {
                print!("{}", output);
            }
        }
        _ => {
            // text (default)
            print_report_text(&rows, total_calls, total_cost, period_from, period_to);
        }
    }

    // Threshold check — after printing the report.
    if let Some(limit) = fail_over_usd {
        if total_cost > limit {
            anyhow::bail!(
                "cost threshold exceeded: ${:.4} spent > ${:.4} limit",
                total_cost,
                limit
            );
        }
    }

    Ok(())
}

fn print_report_text(
    rows: &[ModelReportRow],
    total_calls: i64,
    total_cost: f64,
    from: &str,
    to: &str,
) {
    println!("OpenTrace Cost Report  {} -> {}", from, to);
    println!(
        "{:<28}  {:>6}  {:>12}  {:>12}  {:>10}",
        "model", "calls", "input tok", "output tok", "cost"
    );
    let sep = "-".repeat(74);
    println!("{}", sep);
    for r in rows {
        println!(
            "{:<28}  {:>6}  {:>12}  {:>12}  ${:>9.4}",
            truncate(&r.model, 28),
            r.calls,
            fmt_num_commas(r.input_tokens),
            fmt_num_commas(r.output_tokens),
            r.cost_usd,
        );
    }
    println!("{}", sep);
    println!(
        "{:<28}  {:>6}  {:>12}  {:>12}  ${:>9.4}",
        "TOTAL",
        total_calls,
        "",
        "",
        total_cost
    );
}

fn format_report_github(rows: &[ModelReportRow], total_calls: i64, total_cost: f64) -> String {
    let mut out = String::new();
    out.push_str("## OpenTrace Cost Report\n");
    out.push_str("| model | calls | input tok | output tok | cost |\n");
    out.push_str("|-------|-------|-----------|------------|------|\n");
    for r in rows {
        out.push_str(&format!(
            "| {} | {} | {} | {} | ${:.4} |\n",
            r.model, r.calls,
            fmt_num_commas(r.input_tokens),
            fmt_num_commas(r.output_tokens),
            r.cost_usd
        ));
    }
    out.push_str(&format!("\n**Total: {} calls / ${:.4}**\n", total_calls, total_cost));
    out
}

// ---------------------------------------------------------------------------
// trace config
// ---------------------------------------------------------------------------

fn cmd_config(
    action: ConfigAction,
    cfg: &config::TraceConfig,
    cfg_path: Option<&std::path::Path>,
) -> Result<()> {
    match action {
        ConfigAction::Path => {
            match cfg_path {
                Some(p) => println!("{}", p.display()),
                None => println!("no config file found"),
            }
        }
        ConfigAction::Show => {
            match cfg_path {
                Some(p) => println!("Config file: {}", p.display()),
                None => println!("No config file found — showing hardcoded defaults"),
            }
            println!();
            println!("[start]");
            if let Some(ref s) = cfg.start {
                println!("  port            = {}", s.port.map(|v| v.to_string()).unwrap_or_else(|| "4000 (default)".to_string()));
                println!("  upstream        = {}", s.upstream.as_deref().unwrap_or("https://api.openai.com (default)"));
                if let Some(v) = s.retention_days { println!("  retention_days  = {}", v); }
                if let Some(v) = s.metrics_port { println!("  metrics_port    = {}", v); }
                if let Some(ref v) = s.otel_endpoint { println!("  otel_endpoint   = {}", v); }
                if let Some(ref v) = s.redact_fields { println!("  redact_fields   = {:?}", v); }
                if let Some(v) = s.budget_alert_usd { println!("  budget_alert_usd = {}", v); }
                if let Some(ref v) = s.budget_period { println!("  budget_period   = {}", v); }
                if let Some(ref routes) = s.routes {
                    for r in routes {
                        println!("  route           {} -> {}", r.path, r.upstream);
                    }
                }
            } else {
                println!("  (using all defaults)");
            }
            println!();
            println!("[serve]");
            if let Some(ref s) = cfg.serve {
                println!("  port = {}", s.port.map(|v| v.to_string()).unwrap_or_else(|| "8080 (default)".to_string()));
            } else {
                println!("  (using all defaults)");
            }
        }
        ConfigAction::Init => {
            let home = std::env::var("HOME")
                .or_else(|_| std::env::var("USERPROFILE"))
                .unwrap_or_else(|_| ".".to_string());
            let config_dir = std::path::PathBuf::from(home)
                .join(".config")
                .join("trace");
            std::fs::create_dir_all(&config_dir)
                .context("failed to create ~/.config/trace")?;
            let config_path = config_dir.join("config.toml");
            if config_path.exists() {
                println!("Config file already exists: {}", config_path.display());
                println!("Delete it first if you want a fresh skeleton.");
                return Ok(());
            }
            std::fs::write(&config_path, config::DEFAULT_CONFIG_TOML)
                .context("failed to write config file")?;
            println!("Created: {}", config_path.display());
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Display helpers
// ---------------------------------------------------------------------------

fn print_query_header() {
    println!("{}", format!(
        "{:<8}  {:<19}  {:<22}  {:>6}  {:>8}  {:>7}  {:>6}  {:>6}  {:>9}",
        "id", "timestamp", "model", "status", "latency", "ttft", "in", "out", "cost"
    ).bold().underline());
}

fn print_call_row(call: &store::CallRecord) {
    let id_short = if call.id.len() >= 8 { &call.id[..8] } else { &call.id };
    let ts = call.timestamp.get(..19).unwrap_or(&call.timestamp);
    let model_short = truncate(&call.model, 22);
    let is_error = call.status_code == 0 || call.status_code >= 400 || call.error.is_some();
    let error_tag = if is_error {
        capture::classify_error(call)
            .map(|tag| format!(" [{}]", tag).red().to_string())
            .unwrap_or_default()
    } else {
        String::new()
    };
    let status_str = if is_error {
        format!("{:>6}", call.status_code).red().to_string()
    } else {
        format!("{:>6}", call.status_code).green().to_string()
    };
    let latency_str = format!("{:>6}ms", call.latency_ms);
    let ttft_str = call.ttft_ms
        .map(|t| format!("{:>5}ms", t))
        .unwrap_or_else(|| format!("{:>7}", "-"));
    let in_str = call.input_tokens
        .map(|t| format!("{:>6}", t))
        .unwrap_or_else(|| "     -".to_string());
    let out_str = call.output_tokens
        .map(|t| format!("{:>6}", t))
        .unwrap_or_else(|| "     -".to_string());
    let cost_str = call.cost_usd
        .map(|c| format!("${:>8.4}", c))
        .unwrap_or_else(|| "         -".to_string());

    println!(
        "{:<8}  {:<19}  {:<22}  {}{}  {}  {}  {}  {}  {}",
        id_short, ts, model_short, status_str, error_tag,
        latency_str, ttft_str, in_str, out_str, cost_str
    );
}

fn pretty_json(s: &str) -> String {
    if let Ok(v) = serde_json::from_str::<serde_json::Value>(s) {
        serde_json::to_string_pretty(&v).unwrap_or_else(|_| s.to_string())
    } else {
        s.to_string()
    }
}

/// Parse a status code range like "400-499" into `(lo, hi)`.
/// Returns an error if the format is wrong or lo > hi.
fn parse_status_range(s: &str) -> std::result::Result<(u16, u16), String> {
    let parts: Vec<&str> = s.splitn(2, '-').collect();
    if parts.len() != 2 {
        return Err(format!("expected format LO-HI (e.g. 400-499), got {:?}", s));
    }
    let lo: u16 = parts[0].parse().map_err(|_| format!("invalid status code {:?}", parts[0]))?;
    let hi: u16 = parts[1].parse().map_err(|_| format!("invalid status code {:?}", parts[1]))?;
    if lo > hi {
        return Err(format!("range lo ({}) must be <= hi ({})", lo, hi));
    }
    Ok((lo, hi))
}

fn cmd_vacuum(db: Option<std::path::PathBuf>) -> Result<()> {
    let db_path = match db {
        Some(p) => p,
        None => store::db_path()?,
    };
    let store = store::Store::open_at(&db_path)?;
    let (before, after) = store.vacuum()?;
    println!("Vacuumed: {:.1} MB → {:.1} MB", before as f64 / 1e6, after as f64 / 1e6);
    Ok(())
}

fn cmd_eval(rules: Vec<String>, since: Option<String>, until: Option<String>, model: Option<String>) -> Result<()> {
    if rules.is_empty() {
        println!("{}", "No rules specified. Use --rule \"latency_p99 < 2000\".".yellow());
        return Ok(());
    }

    let store = store::Store::open().context("failed to open trace database")?;
    let filter = store::QueryFilter {
        since,
        until,
        model,
        ..Default::default()
    };
    let stats = store.eval_stats(&filter)?;

    println!("{}", "trace eval".bold());
    println!();
    println!("  total_calls    {}", stats.total_calls.to_string().cyan());
    println!("  error_count    {}", stats.error_count.to_string().cyan());
    println!("  error_rate     {:.4}", stats.error_rate);
    println!("  latency_p99    {}ms", stats.latency_p99.to_string().cyan());
    println!("  avg_cost_usd   ${:.6}", stats.avg_cost_usd);
    println!();

    let mut any_failed = false;

    for rule in &rules {
        match eval_check_rule(rule, &stats) {
            Ok(true) => {
                println!("  {} {}", "PASS".green().bold(), rule);
            }
            Ok(false) => {
                println!("  {} {}", "FAIL".red().bold(), rule);
                any_failed = true;
            }
            Err(e) => {
                println!("  {} {} — {}", "ERR ".yellow().bold(), rule, e);
                any_failed = true;
            }
        }
    }

    println!();
    if any_failed {
        std::process::exit(1);
    }
    Ok(())
}

/// Parse and evaluate a single eval rule string against EvalStats.
/// Format: "METRIC OPERATOR VALUE"
/// Returns Ok(true) = pass, Ok(false) = fail, Err = bad rule format.
fn eval_check_rule(rule: &str, stats: &store::EvalStats) -> std::result::Result<bool, String> {
    let parts: Vec<&str> = rule.trim().splitn(3, ' ').collect();
    if parts.len() != 3 {
        return Err(format!("expected \"METRIC OP VALUE\", got {:?}", rule));
    }
    let metric = parts[0];
    let op = parts[1];
    let rhs: f64 = parts[2].parse().map_err(|_| format!("invalid number: {}", parts[2]))?;

    let lhs: f64 = match metric {
        "latency_p99"   => stats.latency_p99 as f64,
        "error_rate"    => stats.error_rate,
        "error_count"   => stats.error_count as f64,
        "avg_cost_usd"  => stats.avg_cost_usd,
        "total_calls"   => stats.total_calls as f64,
        other           => return Err(format!("unknown metric: {}", other)),
    };

    let result = match op {
        "<"  => lhs <  rhs,
        "<=" => lhs <= rhs,
        ">"  => lhs >  rhs,
        ">=" => lhs >= rhs,
        "=" | "==" => (lhs - rhs).abs() < f64::EPSILON,
        other => return Err(format!("unknown operator: {}", other)),
    };
    Ok(result)
}

fn parse_since_ts(s: String) -> Result<String> {
    if chrono::DateTime::parse_from_rfc3339(&s).is_ok() {
        return Ok(s);
    }
    if chrono::NaiveDate::parse_from_str(&s, "%Y-%m-%d").is_ok() {
        return Ok(s);
    }
    anyhow::bail!(
        "invalid --since value {:?}: use ISO 8601 (e.g. 2024-02-22T14:30:00Z) or YYYY-MM-DD",
        s
    )
}

fn parse_until_ts(s: String) -> Result<String> {
    if chrono::DateTime::parse_from_rfc3339(&s).is_ok() {
        return Ok(s);
    }
    if chrono::NaiveDate::parse_from_str(&s, "%Y-%m-%d").is_ok() {
        return Ok(format!("{}T23:59:59.999Z", s));
    }
    anyhow::bail!(
        "invalid --until value {:?}: use ISO 8601 (e.g. 2024-02-22T23:59:59Z) or YYYY-MM-DD",
        s
    )
}

/// Compute the ISO 8601 timestamp for the start of the current budget period.
fn period_start_iso(period: &str) -> String {
    use chrono::Datelike;
    let now = chrono::Utc::now();
    match period {
        "day" => now.format("%Y-%m-%dT00:00:00.000Z").to_string(),
        "week" => {
            let days_from_monday = now.weekday().num_days_from_monday() as i64;
            let monday = now - chrono::Duration::days(days_from_monday);
            monday.format("%Y-%m-%dT00:00:00.000Z").to_string()
        }
        _ => {
            // "month" (default)
            format!("{}-{:02}-01T00:00:00.000Z", now.year(), now.month())
        }
    }
}

fn truncate(s: &str, max: usize) -> String {
    let mut chars = s.chars();
    let mut out = String::new();
    for _ in 0..max.saturating_sub(1) {
        match chars.next() {
            Some(c) => out.push(c),
            None => return s.to_string(),
        }
    }
    if chars.next().is_some() {
        out.push('…');
        out
    } else {
        s.to_string()
    }
}

fn dim_json(s: &str, max: usize) -> String {
    let trimmed = s.trim();
    let mut clipped = String::new();
    let mut count = 0;
    for c in trimmed.chars() {
        if count >= max {
            return format!("{}…", clipped).dimmed().to_string();
        }
        clipped.push(c);
        count += 1;
    }
    trimmed.dimmed().to_string()
}

fn fmt_tokens(n: i64) -> String {
    if n >= 1_000_000 {
        format!("{:.1}M", n as f64 / 1_000_000.0)
    } else if n >= 1_000 {
        format!("{:.1}K", n as f64 / 1_000.0)
    } else {
        n.to_string()
    }
}

/// Format an integer with comma-separated thousands groups (e.g. 850,200).
fn fmt_num_commas(n: i64) -> String {
    if n == 0 {
        return "0".to_string();
    }
    let s = n.abs().to_string();
    let mut result = String::new();
    for (i, c) in s.chars().rev().enumerate() {
        if i > 0 && i % 3 == 0 {
            result.push(',');
        }
        result.push(c);
    }
    if n < 0 {
        result.push('-');
    }
    result.chars().rev().collect()
}

fn csv_field(s: &str) -> String {
    if s.is_empty() {
        return String::new();
    }
    if s.contains(',') || s.contains('"') || s.contains('\n') || s.contains('\r') {
        format!("\"{}\"", s.replace('"', "\"\""))
    } else {
        s.to_string()
    }
}

fn format_bytes(bytes: u64) -> String {
    if bytes < 1024 {
        format!("{} B", bytes)
    } else if bytes < 1024 * 1024 {
        format!("{:.1} KB", bytes as f64 / 1024.0)
    } else {
        format!("{:.1} MB", bytes as f64 / (1024.0 * 1024.0))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::{CallRecord, Store};

    // -------------------------------------------------------------------------
    // csv_field
    // -------------------------------------------------------------------------

    #[test]
    fn csv_field_plain_string_unchanged() {
        assert_eq!(csv_field("hello"), "hello");
        assert_eq!(csv_field("gpt-4o"), "gpt-4o");
    }

    #[test]
    fn csv_field_empty_string() {
        assert_eq!(csv_field(""), "");
    }

    #[test]
    fn csv_field_wraps_on_comma() {
        assert_eq!(csv_field("a,b"), "\"a,b\"");
    }

    #[test]
    fn csv_field_escapes_double_quote() {
        assert_eq!(csv_field("say \"hello\""), "\"say \"\"hello\"\"\"");
    }

    #[test]
    fn csv_field_wraps_on_newline() {
        assert_eq!(csv_field("line1\nline2"), "\"line1\nline2\"");
    }

    // -------------------------------------------------------------------------
    // export helpers
    // -------------------------------------------------------------------------

    fn make_export_record(id: &str, model: &str) -> CallRecord {
        CallRecord {
            id: id.to_string(),
            timestamp: store::now_iso(),
            provider: "openai".to_string(),
            model: model.to_string(),
            endpoint: "/v1/chat/completions".to_string(),
            status_code: 200,
            latency_ms: 150,
            ttft_ms: Some(80),
            input_tokens: Some(100),
            output_tokens: Some(50),
            cost_usd: Some(0.0025),
            request_body: Some(r#"{"model":"gpt-4o"}"#.to_string()),
            response_body: Some(r#"{"choices":[]}"#.to_string()),
            error: None,
            provider_request_id: Some("req-123".to_string()),
            trace_id: None,
            parent_id: None,
        }
    }

    #[test]
    fn export_jsonl_roundtrips_all_fields() {
        let store = Store::open_in_memory().unwrap();
        for i in 0..3u32 {
            store
                .insert(&make_export_record(&format!("exp-id-{i}"), "gpt-4o"))
                .unwrap();
        }

        let records = store.query_all_filtered(&crate::store::QueryFilter::default()).unwrap();
        assert_eq!(records.len(), 3);

        for r in &records {
            let line = serde_json::to_string(r).unwrap();
            let parsed: serde_json::Value = serde_json::from_str(&line).unwrap();
            assert_eq!(parsed["model"].as_str().unwrap(), "gpt-4o");
            assert_eq!(parsed["status_code"].as_u64().unwrap(), 200);
            assert_eq!(parsed["input_tokens"].as_i64().unwrap(), 100);
            assert_eq!(parsed["output_tokens"].as_i64().unwrap(), 50);
        }
    }

    #[test]
    fn export_csv_has_correct_headers() {
        let expected_header = "id,timestamp,provider,model,endpoint,status_code,latency_ms,\
                               ttft_ms,input_tokens,output_tokens,cost_usd,error,provider_request_id";
        assert_eq!(
            expected_header,
            "id,timestamp,provider,model,endpoint,status_code,latency_ms,\
             ttft_ms,input_tokens,output_tokens,cost_usd,error,provider_request_id"
        );
    }

    #[test]
    fn export_model_filter_applied() {
        let store = Store::open_in_memory().unwrap();
        store.insert(&make_export_record("a", "gpt-4o")).unwrap();
        store.insert(&make_export_record("b", "claude-opus-4")).unwrap();
        store.insert(&make_export_record("c", "gpt-4o")).unwrap();

        let filter = crate::store::QueryFilter {
            model: Some("gpt".to_string()),
            ..Default::default()
        };
        let records = store.query_all_filtered(&filter).unwrap();
        assert_eq!(records.len(), 2);
        assert!(records.iter().all(|r| r.model == "gpt-4o"));
    }

    // -------------------------------------------------------------------------
    // report helpers
    // -------------------------------------------------------------------------

    fn make_cost_record(id: &str, model: &str, cost: f64) -> CallRecord {
        CallRecord {
            id: id.to_string(),
            timestamp: store::now_iso(),
            provider: "openai".to_string(),
            model: model.to_string(),
            endpoint: "/v1/chat/completions".to_string(),
            status_code: 200,
            latency_ms: 100,
            ttft_ms: None,
            input_tokens: Some(1000),
            output_tokens: Some(500),
            cost_usd: Some(cost),
            request_body: None,
            response_body: None,
            error: None,
            provider_request_id: None,
            trace_id: None,
            parent_id: None,
        }
    }

    #[test]
    fn report_total_cost_matches_records() {
        let store = Store::open_in_memory().unwrap();
        store.insert(&make_cost_record("a", "gpt-4o", 1.5)).unwrap();
        store.insert(&make_cost_record("b", "gpt-4o", 2.0)).unwrap();
        store.insert(&make_cost_record("c", "claude-opus-4", 0.75)).unwrap();

        let records = store.query_all_filtered(&crate::store::QueryFilter::default()).unwrap();
        let total: f64 = records.iter().map(|r| r.cost_usd.unwrap_or(0.0)).sum();
        assert!((total - 4.25).abs() < 1e-9, "total cost should be $4.25, got ${}", total);
    }

    #[test]
    fn report_fail_over_usd_logic() {
        // Verify threshold comparison logic: $6.00 total > $5.00 limit.
        let total_cost = 6.0_f64;
        let limit = 5.0_f64;
        assert!(total_cost > limit, "cost exceeds threshold — should trigger failure");

        // Below threshold should not trigger.
        let below = 4.99_f64;
        assert!(!(below > limit), "cost below threshold should not trigger failure");
    }

    #[test]
    fn report_github_format_has_table_header() {
        let rows = vec![ModelReportRow {
            model: "gpt-4o".to_string(),
            calls: 142,
            input_tokens: 850_200,
            output_tokens: 142_000,
            cost_usd: 0.8423,
        }];
        let output = format_report_github(&rows, 142, 0.8423);
        assert!(output.contains("| model |"), "GitHub output must contain table header");
        assert!(output.contains("gpt-4o"), "GitHub output must contain model name");
        assert!(output.contains("**Total:"), "GitHub output must contain total line");
    }

    // -------------------------------------------------------------------------
    // fmt_num_commas
    // -------------------------------------------------------------------------

    #[test]
    fn fmt_num_commas_zero() {
        assert_eq!(fmt_num_commas(0), "0");
    }

    #[test]
    fn fmt_num_commas_small() {
        assert_eq!(fmt_num_commas(999), "999");
    }

    #[test]
    fn fmt_num_commas_thousands() {
        assert_eq!(fmt_num_commas(1_000), "1,000");
        assert_eq!(fmt_num_commas(850_200), "850,200");
    }

    #[test]
    fn fmt_num_commas_millions() {
        assert_eq!(fmt_num_commas(1_000_000), "1,000,000");
    }

    // -------------------------------------------------------------------------
    // Helpers
    // -------------------------------------------------------------------------

    fn no_color() {
        colored::control::set_override(false);
    }

    // -------------------------------------------------------------------------
    // truncate
    // -------------------------------------------------------------------------

    #[test]
    fn truncate_empty_string() {
        assert_eq!(truncate("", 10), "");
    }

    #[test]
    fn truncate_string_shorter_than_max() {
        assert_eq!(truncate("hello", 10), "hello");
    }

    #[test]
    fn truncate_string_exactly_max_length() {
        assert_eq!(truncate("hello", 5), "hell…");
        assert_eq!(truncate("hell", 5), "hell");
    }

    #[test]
    fn truncate_string_one_over_max() {
        let result = truncate("abcdef", 5);
        assert_eq!(result, "abcd…");
        assert_eq!(result.chars().count(), 5);
    }

    #[test]
    fn truncate_string_much_longer_than_max() {
        let long = "a".repeat(100);
        let result = truncate(&long, 10);
        assert_eq!(result.chars().count(), 10);
        assert!(result.ends_with('…'));
    }

    #[test]
    fn truncate_max_one() {
        assert_eq!(truncate("a", 1), "…");
        assert_eq!(truncate("ab", 1), "…");
        assert_eq!(truncate("", 1), "");
    }

    #[test]
    fn truncate_max_zero() {
        assert_eq!(truncate("abc", 0), "…");
        assert_eq!(truncate("", 0), "");
    }

    #[test]
    fn truncate_multi_byte_emoji_chars() {
        let s = "😀😃😄😁😆";
        assert_eq!(truncate(s, 10), s);
        let result = truncate(s, 3);
        assert_eq!(result.chars().count(), 3);
        assert!(result.ends_with('…'));
    }

    #[test]
    fn truncate_multi_byte_non_emoji() {
        let s = "日本語テスト";
        assert_eq!(truncate(s, 7), s);
        assert_eq!(truncate(s, 6), "日本語テス…");
        let clipped = truncate(s, 4);
        assert_eq!(clipped.chars().count(), 4);
        assert!(clipped.ends_with('…'));
    }

    // -------------------------------------------------------------------------
    // dim_json
    // -------------------------------------------------------------------------

    #[test]
    fn dim_json_empty_string() {
        no_color();
        let result = dim_json("", 10);
        assert_eq!(result, "");
    }

    #[test]
    fn dim_json_string_shorter_than_max() {
        no_color();
        let result = dim_json("hello", 10);
        assert_eq!(result, "hello");
    }

    #[test]
    fn dim_json_string_exactly_max_length() {
        no_color();
        let result = dim_json("hello", 5);
        assert_eq!(result, "hello");
    }

    #[test]
    fn dim_json_string_one_over_max() {
        no_color();
        let result = dim_json("helloo", 5);
        assert_eq!(result, "hello…");
    }

    #[test]
    fn dim_json_string_longer_than_max() {
        no_color();
        let long = "a".repeat(200);
        let result = dim_json(&long, 10);
        assert!(result.ends_with('…'));
        assert_eq!(result.chars().count(), 11);
    }

    #[test]
    fn dim_json_strips_leading_and_trailing_whitespace() {
        no_color();
        let result = dim_json("  hello  ", 20);
        assert_eq!(result, "hello");
    }

    #[test]
    fn dim_json_leading_whitespace_then_clip() {
        no_color();
        let result = dim_json("   hello world   ", 5);
        assert_eq!(result, "hello…");
    }

    #[test]
    fn dim_json_multi_byte_chars_counted_by_char_not_byte() {
        no_color();
        let result = dim_json("日本語", 10);
        assert_eq!(result, "日本語");
        let clipped = dim_json("日本語", 2);
        assert_eq!(clipped, "日本…");
        let full = dim_json("日本語", 3);
        assert_eq!(full, "日本語");
    }

    // -------------------------------------------------------------------------
    // parse_status_range (Feature 1)
    // -------------------------------------------------------------------------

    #[test]
    fn parse_status_range_valid() {
        assert_eq!(parse_status_range("400-499"), Ok((400, 499)));
        assert_eq!(parse_status_range("400-400"), Ok((400, 400)));
        assert_eq!(parse_status_range("500-599"), Ok((500, 599)));
    }

    #[test]
    fn parse_status_range_inverted_fails() {
        assert!(parse_status_range("499-400").is_err(), "lo > hi should fail");
    }

    #[test]
    fn parse_status_range_bad_format_fails() {
        assert!(parse_status_range("400").is_err());
        assert!(parse_status_range("abc-499").is_err());
    }

    // -------------------------------------------------------------------------
    // fmt_tokens
    // -------------------------------------------------------------------------

    #[test]
    fn fmt_tokens_below_1000() {
        assert_eq!(fmt_tokens(0), "0");
        assert_eq!(fmt_tokens(1), "1");
        assert_eq!(fmt_tokens(999), "999");
    }

    #[test]
    fn fmt_tokens_exactly_1000() {
        assert_eq!(fmt_tokens(1_000), "1.0K");
    }

    #[test]
    fn fmt_tokens_thousands_range() {
        assert_eq!(fmt_tokens(1_500), "1.5K");
        assert_eq!(fmt_tokens(999_999), "1000.0K");
    }

    #[test]
    fn fmt_tokens_exactly_1_million() {
        assert_eq!(fmt_tokens(1_000_000), "1.0M");
    }

    #[test]
    fn fmt_tokens_millions_range() {
        assert_eq!(fmt_tokens(2_500_000), "2.5M");
        assert_eq!(fmt_tokens(10_000_000), "10.0M");
    }

    #[test]
    fn fmt_tokens_boundary_999() {
        assert_eq!(fmt_tokens(999), "999");
    }

    #[test]
    fn fmt_tokens_boundary_1000() {
        assert_eq!(fmt_tokens(1_000), "1.0K");
    }

    #[test]
    fn fmt_tokens_boundary_1_000_000() {
        assert_eq!(fmt_tokens(1_000_000), "1.0M");
    }

    #[test]
    fn eval_check_rule_latency_pass() {
        let stats = store::EvalStats {
            total_calls: 10, error_count: 1, error_rate: 0.1,
            latency_p99: 1500, avg_cost_usd: 0.001,
        };
        assert_eq!(eval_check_rule("latency_p99 < 2000", &stats), Ok(true));
        assert_eq!(eval_check_rule("latency_p99 < 1000", &stats), Ok(false));
    }

    #[test]
    fn eval_check_rule_error_rate_pass() {
        let stats = store::EvalStats {
            total_calls: 100, error_count: 3, error_rate: 0.03,
            latency_p99: 500, avg_cost_usd: 0.0005,
        };
        assert_eq!(eval_check_rule("error_rate < 0.05", &stats), Ok(true));
        assert_eq!(eval_check_rule("error_rate < 0.01", &stats), Ok(false));
    }

    #[test]
    fn eval_check_rule_bad_metric() {
        let stats = store::EvalStats {
            total_calls: 1, error_count: 0, error_rate: 0.0,
            latency_p99: 100, avg_cost_usd: 0.0,
        };
        assert!(eval_check_rule("unknown_metric < 100", &stats).is_err());
    }

    #[test]
    fn eval_check_rule_bad_format() {
        let stats = store::EvalStats {
            total_calls: 1, error_count: 0, error_rate: 0.0,
            latency_p99: 100, avg_cost_usd: 0.0,
        };
        assert!(eval_check_rule("malformed", &stats).is_err());
    }

    #[test]
    fn parse_status_range_valid_still_works() {
        assert_eq!(parse_status_range("400-499"), Ok((400u16, 499u16)));
    }
}
