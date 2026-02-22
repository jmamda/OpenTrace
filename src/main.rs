mod capture;
mod proxy;
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
    version = "0.1.0",
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
        /// Port to listen on
        #[arg(short, long, default_value = "4000", env = "TRACE_PORT")]
        port: u16,

        /// Upstream LLM API base URL
        #[arg(short, long, default_value = "https://api.openai.com", env = "TRACE_UPSTREAM")]
        upstream: String,

        /// Address to bind on (use 0.0.0.0 to expose on the local network)
        #[arg(long, default_value = "127.0.0.1", env = "TRACE_BIND")]
        bind: String,

        /// Print each request/response to stderr
        #[arg(short, long, env = "TRACE_VERBOSE")]
        verbose: bool,

        /// Upstream request timeout in seconds
        #[arg(long, default_value = "300", env = "TRACE_UPSTREAM_TIMEOUT")]
        upstream_timeout: u64,

        /// Delete records older than this many days (0 = keep forever)
        #[arg(long, default_value = "90", env = "TRACE_RETENTION_DAYS")]
        retention_days: u32,

        /// Do not store request bodies (prompts) in the database.
        /// Use in compliance-sensitive environments where prompt data must not
        /// be persisted at rest.
        #[arg(long, env = "TRACE_NO_REQUEST_BODIES")]
        no_request_bodies: bool,
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
    },

    /// Watch for new calls in real time (polls every 250ms)
    Watch {
        /// Filter by model name (substring match)
        #[arg(short, long)]
        model: Option<String>,

        /// Show only failed calls
        #[arg(short, long)]
        errors: bool,
    },

    /// Show full detail for a single call by ID (prefix of 8+ chars is fine)
    Show {
        /// Call ID or unambiguous prefix from `trace query`
        id: String,
        /// Hide request and response bodies (for sensitive/shared environments)
        #[arg(long)]
        no_bodies: bool,
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
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    match cli.command.unwrap_or(Commands::Stats { breakdown: false, endpoint: false }) {
        Commands::Start { port, upstream, bind, verbose, upstream_timeout, retention_days, no_request_bodies } => {
            cmd_start(port, upstream, bind, verbose, upstream_timeout, retention_days, no_request_bodies).await
        }
        Commands::Query { last, json, bodies, full, model, errors, since, until } => {
            cmd_query(last, json, bodies, full, model, errors, since, until)
        }
        Commands::Watch { model, errors } => {
            cmd_watch(model, errors).await
        }
        Commands::Show { id, no_bodies } => {
            cmd_show(id, no_bodies)
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
    }
}

async fn cmd_start(
    port: u16,
    upstream: String,
    bind: String,
    verbose: bool,
    upstream_timeout: u64,
    retention_days: u32,
    no_request_bodies: bool,
) -> Result<()> {
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

    let store = store::Store::open().context("failed to open trace database")?;
    let db_path = store::db_path()?;

    println!("{}", "trace v0.1".bold());
    println!("  listening  {}", format!("http://{}:{}", bind, port).cyan());
    println!("  upstream   {}", upstream.cyan());
    println!("  storage    {}", db_path.display().to_string().cyan());
    if retention_days > 0 {
        println!("  retention  {} days", retention_days.to_string().cyan());
    }
    println!();
    println!("Set your LLM client:");
    println!("  {}", format!("OPENAI_BASE_URL=http://localhost:{}/v1", port).yellow());
    println!();

    // Warn when binding on non-localhost — proxy will be reachable from the network.
    let is_localhost = matches!(bind.as_str(), "127.0.0.1" | "::1" | "localhost");
    if !is_localhost {
        eprintln!("{}", format!(
            "WARNING: proxy is bound to {} and is reachable from the network.",
            bind
        ).yellow().bold());
        eprintln!("{}", "All captured LLM requests (including prompts) will be visible to network peers.".yellow());
        eprintln!();
    }

    // Warn if upstream is plain HTTP — traffic will be unencrypted in transit.
    if !upstream.starts_with("https://") {
        eprintln!("{}", format!(
            "WARNING: upstream '{}' is not HTTPS — traffic may be unencrypted.",
            upstream
        ).yellow());
        eprintln!();
    }

    println!("{}", format!("Note: full request/response bodies (including prompts) are stored in {}", db_path.display()).dimmed());
    // On Windows the database has no restricted file permissions (unlike Unix 0o600).
    // Warn users on shared machines where other admins could read the file.
    #[cfg(windows)]
    eprintln!("{}", "WARNING: on Windows, the trace database has default NTFS permissions and may be readable by other administrators on this machine.".yellow());
    println!();

    // Bounded channel — backpressure prevents OOM at high RPS.
    // try_send in the proxy drops records (and increments DROPPED_RECORDS) rather
    // than blocking requests.
    let (store_tx, mut store_rx) = mpsc::channel::<store::CallRecord>(20_000);
    let writer_handle = tokio::spawn(async move {
        while let Some(record) = store_rx.recv().await {
            if let Err(e) = store.insert(&record) {
                eprintln!("[trace] db write error: {e}");
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
        .danger_accept_invalid_certs(false) // explicit: always verify TLS certs
        .timeout(std::time::Duration::from_secs(upstream_timeout))
        .build()
        .context("failed to build HTTP client")?;

    // Non-blocking startup connectivity check — warns if the upstream host is
    // unreachable.  Any HTTP response (including 4xx/5xx) means the host is up.
    // Only connection errors and DNS failures trigger a warning.
    if let Ok(check_client) = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(3))
        .build()
    {
        let check_url = upstream.trim_end_matches('/').to_string();
        match check_client.head(&check_url).send().await {
            Ok(_) => {} // reachable — any HTTP response is fine
            Err(e) if e.is_connect() || e.is_timeout() => {
                eprintln!("{}", format!(
                    "WARNING: upstream '{}' did not respond ({}). Proxy will start anyway.",
                    check_url, e
                ).yellow());
                eprintln!();
            }
            Err(_) => {} // auth errors, redirects, TLS issues — host is reachable
        }
    }

    let state = proxy::ProxyState {
        upstream: Arc::new(upstream),
        client,
        store_tx,
        verbose,
        no_request_bodies,
    };

    // Flush DROPPED_RECORDS counter to DB every 10s so `trace stats` can see it
    // across process boundaries (the AtomicU64 is in-process only).
    {
        let flush_store = store::Store::open().context("failed to open trace database for metrics")?;
        tokio::spawn(async move {
            let mut last_flushed: u64 = 0;
            loop {
                tokio::time::sleep(std::time::Duration::from_secs(10)).await;
                let current = proxy::DROPPED_RECORDS.load(Ordering::Relaxed);
                if current != last_flushed {
                    let total: u64 = flush_store
                        .get_meta("dropped_records")
                        .ok()
                        .flatten()
                        .and_then(|s| s.parse().ok())
                        .unwrap_or(0);
                    let _ = flush_store.update_meta("dropped_records", &(total + current - last_flushed).to_string());
                    last_flushed = current;
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

    // store_tx was moved into ProxyState which is owned by the router.
    // When axum::serve returns, the router is dropped → ProxyState dropped → store_tx dropped.
    // The writer task sees channel closed and drains. Wait up to 5s.
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
) -> Result<()> {
    // Validate and normalise timestamp arguments before touching the DB.
    let since = since.map(parse_since_ts).transpose()?;
    let until = until.map(parse_until_ts).transpose()?;

    let store = store::Store::open().context("failed to open trace database")?;
    let filter = store::QueryFilter { errors_only: errors, model, since, until };
    let mut calls = store.query_filtered(limit, &filter)?;

    // Show oldest first in table output.
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

async fn cmd_watch(model: Option<String>, errors: bool) -> Result<()> {
    let store = store::Store::open().context("failed to open trace database")?;
    let filter = store::QueryFilter {
        errors_only: errors,
        model,
        ..Default::default()
    };

    // Initialize last_ts to the most recent record's timestamp, or epoch if none.
    let latest = store.query_filtered(1, &store::QueryFilter::default())?;
    let mut last_ts = latest
        .first()
        .map(|r| r.timestamp.clone())
        .unwrap_or_else(|| "1970-01-01T00:00:00.000Z".to_string());

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

fn cmd_show(id: String, no_bodies: bool) -> Result<()> {
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
        }
    }
    Ok(())
}

fn cmd_stats(breakdown: bool, endpoint: bool) -> Result<()> {
    let store = store::Store::open().context("failed to open trace database")?;
    let s = store.stats()?;

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
    println!();
    println!("  input tokens      {}", s.total_input_tokens.to_string().cyan());
    println!("  output tokens     {}", s.total_output_tokens.to_string().cyan());
    println!("  estimated cost    {}", format!("${:.4}", s.total_cost_usd).cyan());

    // Show dropped records warning if any were lost due to channel backpressure.
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
            println!("{}", format!(
                "  {:<30}  {:>6}  {:>8}  {:>8}  {:>9}  {:>8}",
                "model", "calls", "in", "out", "cost", "avg ms"
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
                println!(
                    "  {:<30}  {:>6}  {}  {}  {}  {:>7.0}ms{}",
                    truncate(&m.model, 30),
                    m.calls,
                    in_str,
                    out_str,
                    cost_str.cyan(),
                    m.avg_latency_ms,
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

// ---------------------------------------------------------------------------
// Display helpers
// ---------------------------------------------------------------------------

/// Print the column header for the query / watch table.
fn print_query_header() {
    println!("{}", format!(
        "{:<8}  {:<19}  {:<22}  {:>6}  {:>8}  {:>7}  {:>6}  {:>6}  {:>9}",
        "id", "timestamp", "model", "status", "latency", "ttft", "in", "out", "cost"
    ).bold().underline());
}

/// Print a single call row in the query / watch table format.
fn print_call_row(call: &store::CallRecord) {
    let id_short = if call.id.len() >= 8 { &call.id[..8] } else { &call.id };
    let ts = call.timestamp.get(..19).unwrap_or(&call.timestamp);
    let model_short = truncate(&call.model, 22);
    let status_str = if call.status_code == 0 || call.status_code >= 400 || call.error.is_some() {
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
        "{:<8}  {:<19}  {:<22}  {}  {}  {}  {}  {}  {}",
        id_short, ts, model_short, status_str, latency_str, ttft_str,
        in_str, out_str, cost_str
    );
}

/// Pretty-print JSON if valid, otherwise return as-is.
fn pretty_json(s: &str) -> String {
    if let Ok(v) = serde_json::from_str::<serde_json::Value>(s) {
        serde_json::to_string_pretty(&v).unwrap_or_else(|_| s.to_string())
    } else {
        s.to_string()
    }
}

/// Validate and return a `--since` timestamp for use in a SQL `>=` comparison.
///
/// Accepts full ISO 8601 (`2024-02-22T14:30:00Z`) or date-only (`2024-02-22`).
/// Date-only strings compare correctly with stored timestamps because lexicographic
/// order matches chronological order for ISO 8601: `"2024-02-22T..." >= "2024-02-22"`.
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

/// Validate and return a `--until` timestamp for use in a SQL `<=` comparison.
///
/// Date-only values are normalised to end-of-day (`T23:59:59.999Z`) so that
/// `--until 2024-02-22` includes all calls made on that day rather than
/// stopping at midnight (which the raw string comparison would otherwise do).
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

/// Truncate to `max` Unicode characters (not bytes), appending `…` if clipped.
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

/// Trim and clip a JSON string for inline display.
/// O(max) — stops scanning after `max` characters.
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

    // -------------------------------------------------------------------------
    // Helpers
    // -------------------------------------------------------------------------

    /// Disable ANSI colour codes for the duration of a test so that string
    /// comparisons on the output of `dim_json` work reliably regardless of
    /// whether the test runner has a TTY attached.
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
        // "hello" = 5 chars. Loop runs max-1=9 times but exits early at iter 5
        // (None branch) → returns s.to_string() = "hello".
        assert_eq!(truncate("hello", 10), "hello");
    }

    #[test]
    fn truncate_string_exactly_max_length() {
        // truncate("hello", max=5): loop runs max-1=4 times, consuming 'h','e','l','l'.
        // Then chars.next() returns Some('o') → '…' is appended → "hell…".
        // A string of exactly `max` chars is clipped because the loop reserves
        // one slot for the ellipsis character.
        assert_eq!(truncate("hello", 5), "hell…");
        // A string of exactly max-1 chars fits without clipping.
        assert_eq!(truncate("hell", 5), "hell");
    }

    #[test]
    fn truncate_string_one_over_max() {
        // "abcdef" = 6 chars, max=5: loop consumes 4, chars.next() = 'e' → clips.
        let result = truncate("abcdef", 5);
        assert_eq!(result, "abcd…");
        // The result has 5 Unicode scalar values: 4 ASCII + '…'
        assert_eq!(result.chars().count(), 5);
    }

    #[test]
    fn truncate_string_much_longer_than_max() {
        // max=10: loop runs 9 times (max-1), consumes 9 'a's.
        // chars.next() is Some → "aaaaaaaaa…" = 9 + 1 = 10 Unicode scalars.
        let long = "a".repeat(100);
        let result = truncate(&long, 10);
        assert_eq!(result.chars().count(), 10);
        assert!(result.ends_with('…'));
    }

    #[test]
    fn truncate_max_one() {
        // max=1: loop runs 0 times (max.saturating_sub(1) = 0).
        // chars.next() on "a" returns Some('a') → out.push('…') → "…".
        assert_eq!(truncate("a", 1), "…");
        // Same for a longer string.
        assert_eq!(truncate("ab", 1), "…");
        // Empty string: loop runs 0 times, chars.next() = None → s.to_string() = "".
        assert_eq!(truncate("", 1), "");
    }

    #[test]
    fn truncate_max_zero() {
        // max=0: saturating_sub(1) = 0, loop never runs, checks next char.
        // For a non-empty string that char exists → returns "…".
        assert_eq!(truncate("abc", 0), "…");
        // Empty string: no next char → returns "".
        assert_eq!(truncate("", 0), "");
    }

    #[test]
    fn truncate_multi_byte_emoji_chars() {
        // Each emoji is one Unicode scalar value; truncate works on chars, not bytes.
        let s = "😀😃😄😁😆"; // 5 emoji = 5 chars
        assert_eq!(truncate(s, 10), s); // shorter than max → unchanged
        let result = truncate(s, 3);   // keep 2 emoji + '…'
        assert_eq!(result.chars().count(), 3);
        assert!(result.ends_with('…'));
    }

    #[test]
    fn truncate_multi_byte_non_emoji() {
        // Japanese characters: each is one char but 3 bytes in UTF-8.
        // Verifies truncation counts Unicode scalars, not bytes.
        let s = "日本語テスト"; // 6 chars
        // max=7 → loop runs 6 times consuming all chars, next()=None → unchanged.
        assert_eq!(truncate(s, 7), s);
        // max=6 clips: loop runs 5 times, next()=Some('ト') → "日本語テス…".
        assert_eq!(truncate(s, 6), "日本語テス…");
        // max=4: keep 3 chars + '…' = 4 Unicode scalars total.
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
        // Empty string trims to empty; inner loop never runs → no '…' appended.
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
        // "hello" = 5 chars, max=5.
        // The guard fires when count >= max.  count is incremented AFTER each
        // push, so all 5 chars are pushed (count goes 0→1→2→3→4→5) before the
        // loop ends — the guard never triggers.  Result is the full string.
        let result = dim_json("hello", 5);
        assert_eq!(result, "hello");
    }

    #[test]
    fn dim_json_string_one_over_max() {
        no_color();
        // "helloo" = 6 chars, max=5.
        // After 5 pushes count==5; on char 'o' (6th) guard fires → "hello…".
        let result = dim_json("helloo", 5);
        assert_eq!(result, "hello…");
    }

    #[test]
    fn dim_json_string_longer_than_max() {
        no_color();
        let long = "a".repeat(200);
        let result = dim_json(&long, 10);
        // Should be clipped: exactly 10 'a's + '…' = 11 chars.
        assert!(result.ends_with('…'));
        assert_eq!(result.chars().count(), 11);
    }

    #[test]
    fn dim_json_strips_leading_and_trailing_whitespace() {
        no_color();
        // The function calls s.trim() first — whitespace is removed before counting.
        let result = dim_json("  hello  ", 20);
        assert_eq!(result, "hello");
    }

    #[test]
    fn dim_json_leading_whitespace_then_clip() {
        no_color();
        // After trim, "hello world" is 11 chars; max=5: 5 chars pushed then
        // the 6th (' ') triggers the guard → "hello…".
        let result = dim_json("   hello world   ", 5);
        assert_eq!(result, "hello…");
    }

    #[test]
    fn dim_json_multi_byte_chars_counted_by_char_not_byte() {
        no_color();
        // "日本語" = 3 chars, 9 bytes. max=10 → not clipped.
        let result = dim_json("日本語", 10);
        assert_eq!(result, "日本語");
        // max=2: guard fires when count==2, so 2 chars are pushed then '…'.
        // '日' (count 0→1) + '本' (count 1→2) pushed, then '語' triggers guard.
        let clipped = dim_json("日本語", 2);
        assert_eq!(clipped, "日本…");
        // max=3: all 3 chars fit (count reaches 3 after last push, loop ends) → full string.
        let full = dim_json("日本語", 3);
        assert_eq!(full, "日本語");
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
        // 999 < 1000 → plain number
        assert_eq!(fmt_tokens(999), "999");
    }

    #[test]
    fn fmt_tokens_boundary_1000() {
        // 1000 >= 1000 → K format
        assert_eq!(fmt_tokens(1_000), "1.0K");
    }

    #[test]
    fn fmt_tokens_boundary_1_000_000() {
        // 1_000_000 >= 1_000_000 → M format
        assert_eq!(fmt_tokens(1_000_000), "1.0M");
    }
}
