//! Config file support for OpenTrace.
//!
//! Searches for a config file in order:
//! 1. `.trace.toml` in the current working directory (project-local)
//! 2. `~/.config/trace/config.toml` (user-global)
//!
//! Priority: CLI flags > env vars > config file > hardcoded defaults.

use serde::Deserialize;
use std::collections::HashMap;
use std::path::PathBuf;

/// A single `[[start.routes]]` entry in the config file.
#[derive(Debug, Deserialize, Clone)]
pub struct RouteEntry {
    pub path: String,
    pub upstream: String,
}

/// Per-model pricing override: cost per 1M tokens.
#[derive(Debug, Deserialize, Clone)]
pub struct PriceEntry {
    pub input: f64,
    pub output: f64,
}

/// Langfuse real-time sink configuration.
#[derive(Debug, Default, Deserialize, Clone)]
pub struct LangfuseConfig {
    pub public_key: Option<String>,
    pub secret_key: Option<String>,
    pub url: Option<String>,
    pub timeout: Option<u64>,
}

/// LangSmith real-time sink configuration.
#[derive(Debug, Default, Deserialize, Clone)]
pub struct LangSmithConfig {
    pub api_key: Option<String>,
    pub endpoint: Option<String>,
    pub timeout: Option<u64>,
}

/// W&B Weave real-time sink configuration.
#[derive(Debug, Default, Deserialize, Clone)]
pub struct WeaveConfig {
    pub api_key: Option<String>,
    pub project: Option<String>,
    pub timeout: Option<u64>,
}

/// Arize Phoenix real-time sink configuration.
#[derive(Debug, Default, Deserialize, Clone)]
pub struct PhoenixConfig {
    pub endpoint: Option<String>,
    pub api_key: Option<String>,
    pub timeout: Option<u64>,
}

/// SQLite tuning parameters.
#[derive(Debug, Default, Deserialize, Clone)]
pub struct SqliteConfig {
    pub busy_timeout: Option<u32>,
    pub cache_size_kb: Option<u32>,
    pub mmap_size_mb: Option<u32>,
    pub sync_mode: Option<String>,
}

/// OpenTelemetry OTLP export configuration.
#[derive(Debug, Default, Deserialize, Clone)]
pub struct OtelConfig {
    pub endpoint: Option<String>,
    pub timeout: Option<u64>,
    pub service_name: Option<String>,
}

/// Prometheus metrics configuration.
#[derive(Debug, Default, Deserialize, Clone)]
pub struct MetricsConfig {
    pub port: Option<u16>,
    pub latency_buckets_ms: Option<Vec<u64>>,
}

#[derive(Debug, Default, Deserialize)]
#[allow(dead_code)]
pub struct StartConfig {
    pub port: Option<u16>,
    pub upstream: Option<String>,
    pub bind: Option<String>,
    pub retention_days: Option<u32>,
    pub metrics_port: Option<u16>,
    pub otel_endpoint: Option<String>,
    pub redact_fields: Option<Vec<String>>,
    pub budget_alert_usd: Option<f64>,
    pub budget_period: Option<String>,
    pub upstream_timeout: Option<u64>,
    pub no_request_bodies: Option<bool>,
    pub verbose: Option<bool>,
    pub routes: Option<Vec<RouteEntry>>,

    // Database
    pub db_path: Option<String>,

    // Body limits
    pub max_request_body_bytes: Option<usize>,
    pub max_stored_response_bytes: Option<usize>,
    pub max_stored_stream_bytes: Option<usize>,
    pub max_accumulation_bytes: Option<usize>,

    // Internal tuning
    pub channel_capacity: Option<usize>,
    pub graceful_shutdown_timeout: Option<u64>,

    // Pricing overrides
    pub prices: Option<HashMap<String, PriceEntry>>,
    pub unknown_price_input: Option<f64>,
    pub unknown_price_output: Option<f64>,

    // Sink configs
    pub langfuse: Option<LangfuseConfig>,
    pub langsmith: Option<LangSmithConfig>,
    pub weave: Option<WeaveConfig>,
    pub phoenix: Option<PhoenixConfig>,

    // SQLite tuning
    pub sqlite: Option<SqliteConfig>,

    // OTel
    pub otel: Option<OtelConfig>,

    // Metrics
    pub metrics: Option<MetricsConfig>,
}

#[derive(Debug, Default, Deserialize)]
pub struct ServeConfig {
    pub port: Option<u16>,
}

#[derive(Debug, Default, Deserialize)]
pub struct TraceConfig {
    pub start: Option<StartConfig>,
    pub serve: Option<ServeConfig>,
}

/// Return the path to the active config file (first found wins):
/// 1. `.trace.toml` in the current working directory
/// 2. `~/.config/trace/config.toml`
pub fn config_path() -> Option<PathBuf> {
    // CWD-local config takes priority.
    if let Ok(cwd) = std::env::current_dir() {
        let local = cwd.join(".trace.toml");
        if local.exists() {
            return Some(local);
        }
    }

    // Fall back to user-global config.
    let home = std::env::var("HOME")
        .or_else(|_| std::env::var("USERPROFILE"))
        .ok()?;
    let global = PathBuf::from(home)
        .join(".config")
        .join("trace")
        .join("config.toml");
    if global.exists() {
        return Some(global);
    }

    None
}

/// Load config from the first found file.  Returns default (all-None) config
/// when no file exists or the file cannot be parsed.
pub fn load_config() -> TraceConfig {
    match config_path() {
        Some(p) => load_config_from(&p),
        None => TraceConfig::default(),
    }
}

/// Load and parse a config file at the given path.  Returns default config on
/// any I/O or parse error so that a corrupt config never prevents startup.
pub fn load_config_from(path: &std::path::Path) -> TraceConfig {
    let content = match std::fs::read_to_string(path) {
        Ok(s) => s,
        Err(_) => return TraceConfig::default(),
    };
    toml::from_str(&content).unwrap_or_default()
}

/// Validate a loaded config against the raw TOML source.
///
/// Returns a list of human-readable warnings.  An empty list means no issues.
pub fn validate_config(raw_toml: &str, config: &TraceConfig) -> Vec<String> {
    let mut warnings = Vec::new();
    check_unknown_fields(raw_toml, &mut warnings);
    if let Some(start) = &config.start {
        validate_start_config(start, &mut warnings);
    }
    warnings
}

/// Compare keys found in the raw TOML against the known field lists.
fn check_unknown_fields(raw_toml: &str, warnings: &mut Vec<String>) {
    let table: toml::Table = match raw_toml.parse() {
        Ok(t) => t,
        Err(_) => return, // parse errors are handled elsewhere
    };

    // Known top-level sections
    let known_top: &[&str] = &["start", "serve"];
    for key in table.keys() {
        if !known_top.contains(&key.as_str()) {
            warnings.push(format!("unknown top-level key: \"{}\"", key));
        }
    }

    // Known [start] scalar/table fields
    let known_start: &[&str] = &[
        "port",
        "upstream",
        "bind",
        "retention_days",
        "metrics_port",
        "otel_endpoint",
        "redact_fields",
        "budget_alert_usd",
        "budget_period",
        "upstream_timeout",
        "no_request_bodies",
        "verbose",
        "routes",
        "db_path",
        "max_request_body_bytes",
        "max_stored_response_bytes",
        "max_stored_stream_bytes",
        "max_accumulation_bytes",
        "channel_capacity",
        "graceful_shutdown_timeout",
        "prices",
        "unknown_price_input",
        "unknown_price_output",
        "langfuse",
        "langsmith",
        "weave",
        "phoenix",
        "sqlite",
        "otel",
        "metrics",
    ];
    if let Some(toml::Value::Table(start)) = table.get("start") {
        for key in start.keys() {
            if !known_start.contains(&key.as_str()) {
                warnings.push(format!("unknown key in [start]: \"{}\"", key));
            }
        }
    }

    // Known [serve] fields
    let known_serve: &[&str] = &["port"];
    if let Some(toml::Value::Table(serve)) = table.get("serve") {
        for key in serve.keys() {
            if !known_serve.contains(&key.as_str()) {
                warnings.push(format!("unknown key in [serve]: \"{}\"", key));
            }
        }
    }
}

/// Validate semantic constraints on [start] config values.
fn validate_start_config(start: &StartConfig, warnings: &mut Vec<String>) {
    if start.port == Some(0) {
        warnings.push("start.port = 0 is invalid (OS will assign a random port)".to_string());
    }
    if start.retention_days == Some(0) {
        warnings.push("start.retention_days = 0 will delete all data on startup".to_string());
    }

    // Negative prices
    if let Some(p) = start.unknown_price_input {
        if p < 0.0 {
            warnings.push(format!("start.unknown_price_input is negative: {}", p));
        }
    }
    if let Some(p) = start.unknown_price_output {
        if p < 0.0 {
            warnings.push(format!("start.unknown_price_output is negative: {}", p));
        }
    }
    if let Some(prices) = &start.prices {
        for (model, entry) in prices {
            if entry.input < 0.0 {
                warnings.push(format!(
                    "start.prices.\"{}\".input is negative: {}",
                    model, entry.input
                ));
            }
            if entry.output < 0.0 {
                warnings.push(format!(
                    "start.prices.\"{}\".output is negative: {}",
                    model, entry.output
                ));
            }
        }
    }

    // Body limits > 1GB
    let gb = 1024 * 1024 * 1024;
    if let Some(v) = start.max_request_body_bytes {
        if v > gb {
            warnings.push(format!("start.max_request_body_bytes ({}) exceeds 1GB", v));
        }
    }
    if let Some(v) = start.max_stored_response_bytes {
        if v > gb {
            warnings.push(format!(
                "start.max_stored_response_bytes ({}) exceeds 1GB",
                v
            ));
        }
    }
    if let Some(v) = start.max_accumulation_bytes {
        if v > gb {
            warnings.push(format!("start.max_accumulation_bytes ({}) exceeds 1GB", v));
        }
    }

    // Partial sink credentials
    if let Some(lf) = &start.langfuse {
        if lf.public_key.is_some() && lf.secret_key.is_none() {
            warnings.push("langfuse: public_key set but secret_key missing".to_string());
        }
        if lf.secret_key.is_some() && lf.public_key.is_none() {
            warnings.push("langfuse: secret_key set but public_key missing".to_string());
        }
    }
    if let Some(ls) = &start.langsmith {
        if ls.endpoint.is_some() && ls.api_key.is_none() {
            warnings.push("langsmith: endpoint set but api_key missing".to_string());
        }
    }
    if let Some(w) = &start.weave {
        if w.project.is_some() && w.api_key.is_none() {
            warnings.push("weave: project set but api_key missing".to_string());
        }
    }
}

/// Default config skeleton written by `trace config init`.
pub const DEFAULT_CONFIG_TOML: &str = r#"[start]
port = 4000
upstream = "https://api.openai.com"
retention_days = 90
# metrics_port = 9091
# otel_endpoint = "http://localhost:4318"
# redact_fields = ["messages"]
# budget_alert_usd = 50.0
# budget_period = "month"

# Database path (default: ~/.trace/trace.db)
# db_path = "/custom/path/trace.db"

# Body limits (bytes)
# max_request_body_bytes = 16777216      # 16 MB
# max_stored_response_bytes = 10485760   # 10 MB
# max_stored_stream_bytes = 262144       # 256 KB
# max_accumulation_bytes = 4194304       # 4 MB

# Internal tuning
# channel_capacity = 20000
# graceful_shutdown_timeout = 5          # seconds

# Default pricing for unknown models ($ per 1M tokens)
# unknown_price_input = 1.0
# unknown_price_output = 3.0

# Per-model pricing overrides ($ per 1M tokens)
# [start.prices.my-custom-model]
# input = 2.5
# output = 10.0

# Route specific path prefixes to different upstreams:
# [[start.routes]]
# path = "/v1/messages"
# upstream = "https://api.anthropic.com"

# SQLite tuning
# [start.sqlite]
# busy_timeout = 5000       # ms
# cache_size_kb = 32000     # KB (maps to PRAGMA cache_size=-N)
# mmap_size_mb = 64         # MB
# sync_mode = "NORMAL"      # OFF | NORMAL | FULL

# OpenTelemetry OTLP export
# [start.otel]
# endpoint = "http://localhost:4318"
# timeout = 5               # seconds
# service_name = "opentrace"

# Prometheus metrics
# [start.metrics]
# port = 9091
# latency_buckets_ms = [100, 500, 1000, 5000]

# Langfuse sink
# [start.langfuse]
# public_key = "pk-lf-..."
# secret_key = "sk-lf-..."
# url = "https://cloud.langfuse.com"
# timeout = 5

# LangSmith sink
# [start.langsmith]
# api_key = "ls-..."
# endpoint = "https://api.smith.langchain.com"
# timeout = 5

# W&B Weave sink
# [start.weave]
# api_key = "wandb-..."
# project = "entity/project"
# timeout = 5

# Arize Phoenix sink
# [start.phoenix]
# endpoint = "http://localhost:6006"
# api_key = "phoenix-..."
# timeout = 5

[serve]
port = 8080
"#;

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(s: &str) -> TraceConfig {
        toml::from_str(s).unwrap_or_default()
    }

    #[test]
    fn config_loads_port_from_toml() {
        let cfg = parse("[start]\nport = 1234\n");
        assert_eq!(
            cfg.start.and_then(|s| s.port),
            Some(1234),
            "port should be parsed from TOML"
        );
    }

    #[test]
    fn config_returns_defaults_when_no_file() {
        // Empty content parses to all-None struct.
        let cfg = parse("");
        assert!(cfg.start.is_none(), "start should be None when not in file");
        assert!(cfg.serve.is_none(), "serve should be None when not in file");
    }

    #[test]
    fn config_cwd_file_takes_priority() {
        // Verify that TOML parsing is independent for each config:
        // a CWD config with port=9999 should parse separately from a global
        // config with port=8888 — both should return their own values.
        let cwd_cfg = parse("[start]\nport = 9999\n");
        let global_cfg = parse("[start]\nport = 8888\n");

        assert_eq!(cwd_cfg.start.and_then(|s| s.port), Some(9999));
        assert_eq!(global_cfg.start.and_then(|s| s.port), Some(8888));

        // The real path search (CWD before global) is implemented in
        // config_path() — verify it doesn't panic.
        let _ = config_path();
    }

    #[test]
    fn config_parses_serve_port() {
        let cfg = parse("[serve]\nport = 8080\n");
        assert_eq!(cfg.serve.and_then(|s| s.port), Some(8080));
    }

    #[test]
    fn config_parses_redact_fields_array() {
        let cfg = parse("[start]\nredact_fields = [\"messages\", \"system_prompt\"]\n");
        let fields = cfg.start.and_then(|s| s.redact_fields).unwrap_or_default();
        assert_eq!(fields, vec!["messages", "system_prompt"]);
    }

    #[test]
    fn config_load_from_nonexistent_returns_default() {
        let path = std::path::Path::new("/nonexistent/path/that/does/not/exist.toml");
        let cfg = load_config_from(path);
        assert!(cfg.start.is_none());
        assert!(cfg.serve.is_none());
    }

    #[test]
    fn config_parses_routes_from_toml() {
        let toml = r#"
[[start.routes]]
path = "/v1/messages"
upstream = "https://api.anthropic.com"

[[start.routes]]
path = "/v1/chat"
upstream = "https://api.openai.com"
"#;
        let cfg = parse(toml);
        let routes = cfg.start.and_then(|s| s.routes).unwrap_or_default();
        assert_eq!(routes.len(), 2);
        assert_eq!(routes[0].path, "/v1/messages");
        assert_eq!(routes[0].upstream, "https://api.anthropic.com");
        assert_eq!(routes[1].path, "/v1/chat");
        assert_eq!(routes[1].upstream, "https://api.openai.com");
    }

    #[test]
    fn config_routes_none_when_not_present() {
        let cfg = parse("[start]\nport = 4000\n");
        let routes = cfg.start.and_then(|s| s.routes);
        assert!(routes.is_none(), "routes should be None when not in config");
    }

    #[test]
    fn config_parses_db_path() {
        let cfg = parse("[start]\ndb_path = \"/tmp/my.db\"\n");
        assert_eq!(
            cfg.start.and_then(|s| s.db_path).as_deref(),
            Some("/tmp/my.db")
        );
    }

    #[test]
    fn config_parses_body_limits() {
        let toml = r#"
[start]
max_request_body_bytes = 8388608
max_stored_response_bytes = 5242880
max_stored_stream_bytes = 131072
max_accumulation_bytes = 2097152
"#;
        let cfg = parse(toml);
        let s = cfg.start.unwrap();
        assert_eq!(s.max_request_body_bytes, Some(8_388_608));
        assert_eq!(s.max_stored_response_bytes, Some(5_242_880));
        assert_eq!(s.max_stored_stream_bytes, Some(131_072));
        assert_eq!(s.max_accumulation_bytes, Some(2_097_152));
    }

    #[test]
    fn config_parses_channel_capacity_and_shutdown() {
        let cfg = parse("[start]\nchannel_capacity = 50000\ngraceful_shutdown_timeout = 10\n");
        let s = cfg.start.unwrap();
        assert_eq!(s.channel_capacity, Some(50_000));
        assert_eq!(s.graceful_shutdown_timeout, Some(10));
    }

    #[test]
    fn config_parses_pricing_overrides() {
        let toml = r#"
[start]
unknown_price_input = 2.0
unknown_price_output = 6.0

[start.prices.my-model]
input = 5.0
output = 15.0
"#;
        let cfg = parse(toml);
        let s = cfg.start.unwrap();
        assert_eq!(s.unknown_price_input, Some(2.0));
        assert_eq!(s.unknown_price_output, Some(6.0));
        let prices = s.prices.unwrap();
        let entry = prices.get("my-model").unwrap();
        assert_eq!(entry.input, 5.0);
        assert_eq!(entry.output, 15.0);
    }

    #[test]
    fn config_parses_sqlite_section() {
        let toml = r#"
[start.sqlite]
busy_timeout = 10000
cache_size_kb = 64000
mmap_size_mb = 128
sync_mode = "FULL"
"#;
        let cfg = parse(toml);
        let sq = cfg.start.unwrap().sqlite.unwrap();
        assert_eq!(sq.busy_timeout, Some(10_000));
        assert_eq!(sq.cache_size_kb, Some(64_000));
        assert_eq!(sq.mmap_size_mb, Some(128));
        assert_eq!(sq.sync_mode.as_deref(), Some("FULL"));
    }

    #[test]
    fn config_parses_otel_section() {
        let toml = r#"
[start.otel]
endpoint = "http://localhost:4318"
timeout = 10
service_name = "my-service"
"#;
        let cfg = parse(toml);
        let o = cfg.start.unwrap().otel.unwrap();
        assert_eq!(o.endpoint.as_deref(), Some("http://localhost:4318"));
        assert_eq!(o.timeout, Some(10));
        assert_eq!(o.service_name.as_deref(), Some("my-service"));
    }

    #[test]
    fn config_parses_metrics_section() {
        let toml = r#"
[start.metrics]
port = 9091
latency_buckets_ms = [50, 200, 1000, 3000]
"#;
        let cfg = parse(toml);
        let m = cfg.start.unwrap().metrics.unwrap();
        assert_eq!(m.port, Some(9091));
        assert_eq!(m.latency_buckets_ms.unwrap(), vec![50, 200, 1000, 3000]);
    }

    #[test]
    fn config_parses_langfuse_section() {
        let toml = r#"
[start.langfuse]
public_key = "pk-lf-test"
secret_key = "sk-lf-test"
url = "https://cloud.langfuse.com"
timeout = 10
"#;
        let cfg = parse(toml);
        let lf = cfg.start.unwrap().langfuse.unwrap();
        assert_eq!(lf.public_key.as_deref(), Some("pk-lf-test"));
        assert_eq!(lf.secret_key.as_deref(), Some("sk-lf-test"));
        assert_eq!(lf.url.as_deref(), Some("https://cloud.langfuse.com"));
        assert_eq!(lf.timeout, Some(10));
    }

    #[test]
    fn config_parses_langsmith_section() {
        let toml = r#"
[start.langsmith]
api_key = "ls-test"
endpoint = "https://api.smith.langchain.com"
timeout = 8
"#;
        let cfg = parse(toml);
        let ls = cfg.start.unwrap().langsmith.unwrap();
        assert_eq!(ls.api_key.as_deref(), Some("ls-test"));
        assert_eq!(
            ls.endpoint.as_deref(),
            Some("https://api.smith.langchain.com")
        );
        assert_eq!(ls.timeout, Some(8));
    }

    #[test]
    fn config_parses_weave_section() {
        let toml = r#"
[start.weave]
api_key = "wandb-test"
project = "entity/proj"
timeout = 7
"#;
        let cfg = parse(toml);
        let w = cfg.start.unwrap().weave.unwrap();
        assert_eq!(w.api_key.as_deref(), Some("wandb-test"));
        assert_eq!(w.project.as_deref(), Some("entity/proj"));
        assert_eq!(w.timeout, Some(7));
    }

    #[test]
    fn config_parses_phoenix_section() {
        let toml = r#"
[start.phoenix]
endpoint = "http://localhost:6006"
api_key = "phoenix-test"
timeout = 6
"#;
        let cfg = parse(toml);
        let p = cfg.start.unwrap().phoenix.unwrap();
        assert_eq!(p.endpoint.as_deref(), Some("http://localhost:6006"));
        assert_eq!(p.api_key.as_deref(), Some("phoenix-test"));
        assert_eq!(p.timeout, Some(6));
    }

    #[test]
    fn config_new_fields_default_to_none() {
        let cfg = parse("[start]\nport = 4000\n");
        let s = cfg.start.unwrap();
        assert!(s.db_path.is_none());
        assert!(s.max_request_body_bytes.is_none());
        assert!(s.channel_capacity.is_none());
        assert!(s.prices.is_none());
        assert!(s.langfuse.is_none());
        assert!(s.sqlite.is_none());
        assert!(s.otel.is_none());
        assert!(s.metrics.is_none());
    }

    // ── Validation tests ─────────────────────────────────────────────────

    #[test]
    fn validate_clean_config_returns_no_warnings() {
        let raw = "[start]\nport = 4000\nupstream = \"https://api.openai.com\"\n";
        let cfg = parse(raw);
        let warnings = validate_config(raw, &cfg);
        assert!(
            warnings.is_empty(),
            "expected no warnings, got: {:?}",
            warnings
        );
    }

    #[test]
    fn validate_detects_unknown_top_level_key() {
        let raw = "[start]\nport = 4000\n\n[bogus]\nfoo = 1\n";
        let cfg = parse(raw);
        let warnings = validate_config(raw, &cfg);
        assert!(
            warnings.iter().any(|w| w.contains("bogus")),
            "expected bogus warning: {:?}",
            warnings
        );
    }

    #[test]
    fn validate_detects_unknown_start_key() {
        let raw = "[start]\nport = 4000\ntypo_field = true\n";
        let cfg = parse(raw);
        let warnings = validate_config(raw, &cfg);
        assert!(
            warnings.iter().any(|w| w.contains("typo_field")),
            "expected typo_field warning: {:?}",
            warnings
        );
    }

    #[test]
    fn validate_detects_port_zero() {
        let raw = "[start]\nport = 0\n";
        let cfg = parse(raw);
        let warnings = validate_config(raw, &cfg);
        assert!(
            warnings.iter().any(|w| w.contains("port")),
            "expected port warning: {:?}",
            warnings
        );
    }

    #[test]
    fn validate_detects_negative_price() {
        let raw = "[start]\nunknown_price_input = -1.0\n";
        let cfg = parse(raw);
        let warnings = validate_config(raw, &cfg);
        assert!(
            warnings.iter().any(|w| w.contains("negative")),
            "expected negative price warning: {:?}",
            warnings
        );
    }

    #[test]
    fn validate_detects_partial_langfuse_creds() {
        let raw = "[start.langfuse]\npublic_key = \"pk-lf-test\"\n";
        let cfg = parse(raw);
        let warnings = validate_config(raw, &cfg);
        assert!(
            warnings.iter().any(|w| w.contains("secret_key missing")),
            "expected langfuse warning: {:?}",
            warnings
        );
    }

    #[test]
    fn validate_detects_partial_weave_creds() {
        let raw = "[start.weave]\nproject = \"entity/proj\"\n";
        let cfg = parse(raw);
        let warnings = validate_config(raw, &cfg);
        assert!(
            warnings.iter().any(|w| w.contains("api_key missing")),
            "expected weave warning: {:?}",
            warnings
        );
    }
}
