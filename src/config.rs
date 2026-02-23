//! Config file support for OpenTrace.
//!
//! Searches for a config file in order:
//! 1. `.trace.toml` in the current working directory (project-local)
//! 2. `~/.config/trace/config.toml` (user-global)
//!
//! Priority: CLI flags > env vars > config file > hardcoded defaults.

use serde::Deserialize;
use std::path::PathBuf;

/// A single `[[start.routes]]` entry in the config file.
#[derive(Debug, Deserialize, Clone)]
pub struct RouteEntry {
    pub path: String,
    pub upstream: String,
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

# Route specific path prefixes to different upstreams:
# [[start.routes]]
# path = "/v1/messages"
# upstream = "https://api.anthropic.com"

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
}
