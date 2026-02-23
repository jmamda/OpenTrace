//! In-memory Prometheus metrics state.
//!
//! `MetricsState` is updated by the DB-writer task on every captured call and
//! exposed via a `/metrics` HTTP endpoint (when `--metrics-port` is set).
//!
//! No external crates are required — the Prometheus text format (version 0.0.4)
//! is simple enough to serialise manually.

use std::collections::HashMap;
use std::sync::Mutex;

use crate::store::CallRecord;

/// Histogram bucket upper bounds (ms). Index 4 is always +Inf.
const LATENCY_BOUNDS_MS: &[u64] = &[100, 500, 1_000, 5_000];
const LE_LABELS: &[&str] = &["100", "500", "1000", "5000", "+Inf"];

/// Inner mutable state — protected by a single Mutex so scrapes are consistent.
struct MetricsInner {
    /// calls_total{model, provider, status_class} → count
    calls: HashMap<(String, String, String), u64>,
    /// tokens_total{model, provider, type="input"} → tokens
    input_tokens: HashMap<(String, String), u64>,
    /// tokens_total{model, provider, type="output"} → tokens
    output_tokens: HashMap<(String, String), u64>,
    /// cost_usd_total{model, provider} → USD
    cost_usd: HashMap<(String, String), f64>,
    /// latency_ms histogram buckets[model] = [le100, le500, le1000, le5000, +Inf]
    /// Each bucket holds the *cumulative* count (calls with latency <= le).
    latency_buckets: HashMap<String, [u64; 5]>,
    latency_sum: HashMap<String, f64>,
    latency_count: HashMap<String, u64>,
    /// ttft_ms histogram (streaming calls only)
    ttft_buckets: HashMap<String, [u64; 5]>,
    ttft_sum: HashMap<String, f64>,
    ttft_count: HashMap<String, u64>,
    /// Dropped records counter (synced from the DB meta table periodically).
    dropped_records: u64,
    /// Budget alerting state (only populated when --budget-alert-usd is set).
    budget_spent_usd: f64,
    budget_limit_usd: f64,
    budget_period: String,
    budget_enabled: bool,
}

/// Shared Prometheus metrics state.
///
/// Clone-able via `Arc` — the `Arc` clone is cheap (ref-count bump).
pub struct MetricsState {
    inner: Mutex<MetricsInner>,
}

impl MetricsState {
    pub fn new() -> Self {
        MetricsState {
            inner: Mutex::new(MetricsInner {
                calls: HashMap::new(),
                input_tokens: HashMap::new(),
                output_tokens: HashMap::new(),
                cost_usd: HashMap::new(),
                latency_buckets: HashMap::new(),
                latency_sum: HashMap::new(),
                latency_count: HashMap::new(),
                ttft_buckets: HashMap::new(),
                ttft_sum: HashMap::new(),
                ttft_count: HashMap::new(),
                dropped_records: 0,
                budget_spent_usd: 0.0,
                budget_limit_usd: 0.0,
                budget_period: String::new(),
                budget_enabled: false,
            }),
        }
    }

    /// Update budget spend gauge (called from the budget-alerting task every 60 s).
    pub fn set_budget_spent(&self, spent: f64, limit: f64, period: &str) {
        let mut inner = self.inner.lock().unwrap();
        inner.budget_spent_usd = spent;
        inner.budget_limit_usd = limit;
        inner.budget_period = period.to_string();
        inner.budget_enabled = true;
    }

    /// Record a captured call.  Called from the DB-writer task on every insert.
    pub fn record(&self, r: &CallRecord) {
        let status_class = match r.status_code {
            0 => "err",
            200..=299 => "2xx",
            400..=499 => "4xx",
            500..=599 => "5xx",
            _ => "other",
        }
        .to_string();

        let mut inner = self.inner.lock().unwrap();

        // calls_total
        *inner
            .calls
            .entry((r.model.clone(), r.provider.clone(), status_class))
            .or_insert(0) += 1;

        // tokens_total
        if let Some(i) = r.input_tokens {
            if i > 0 {
                *inner
                    .input_tokens
                    .entry((r.model.clone(), r.provider.clone()))
                    .or_insert(0) += i as u64;
            }
        }
        if let Some(o) = r.output_tokens {
            if o > 0 {
                *inner
                    .output_tokens
                    .entry((r.model.clone(), r.provider.clone()))
                    .or_insert(0) += o as u64;
            }
        }

        // cost_usd_total
        if let Some(c) = r.cost_usd {
            if c > 0.0 {
                *inner
                    .cost_usd
                    .entry((r.model.clone(), r.provider.clone()))
                    .or_insert(0.0) += c;
            }
        }

        // latency_ms histogram — cumulative buckets
        {
            let buckets = inner
                .latency_buckets
                .entry(r.model.clone())
                .or_insert([0u64; 5]);
            for (i, &bound) in LATENCY_BOUNDS_MS.iter().enumerate() {
                if r.latency_ms <= bound {
                    buckets[i] += 1;
                }
            }
            buckets[4] += 1; // +Inf always
            *inner.latency_sum.entry(r.model.clone()).or_insert(0.0) +=
                r.latency_ms as f64;
            *inner.latency_count.entry(r.model.clone()).or_insert(0) += 1;
        }

        // ttft_ms histogram
        if let Some(ttft) = r.ttft_ms {
            let buckets = inner
                .ttft_buckets
                .entry(r.model.clone())
                .or_insert([0u64; 5]);
            for (i, &bound) in LATENCY_BOUNDS_MS.iter().enumerate() {
                if ttft <= bound {
                    buckets[i] += 1;
                }
            }
            buckets[4] += 1;
            *inner.ttft_sum.entry(r.model.clone()).or_insert(0.0) += ttft as f64;
            *inner.ttft_count.entry(r.model.clone()).or_insert(0) += 1;
        }
    }

    /// Update the dropped-records gauge (called from the periodic flush task).
    pub fn set_dropped_records(&self, count: u64) {
        self.inner.lock().unwrap().dropped_records = count;
    }

    /// Render all metrics in Prometheus text format (version 0.0.4).
    pub fn render_prometheus(&self) -> String {
        let inner = self.inner.lock().unwrap();
        let mut out = String::with_capacity(4096);

        // --- opentrace_calls_total -------------------------------------------
        out.push_str("# HELP opentrace_calls_total Total LLM calls captured\n");
        out.push_str("# TYPE opentrace_calls_total counter\n");
        let mut calls_sorted: Vec<_> = inner.calls.iter().collect();
        calls_sorted.sort_by(|a, b| a.0.cmp(b.0));
        for ((model, provider, status_class), count) in calls_sorted {
            out.push_str(&format!(
                "opentrace_calls_total{{model=\"{}\",provider=\"{}\",status_class=\"{}\"}} {}\n",
                model, provider, status_class, count
            ));
        }

        // --- opentrace_tokens_total ------------------------------------------
        out.push_str("# HELP opentrace_tokens_total Total tokens processed\n");
        out.push_str("# TYPE opentrace_tokens_total counter\n");
        let mut input_sorted: Vec<_> = inner.input_tokens.iter().collect();
        input_sorted.sort_by(|a, b| a.0.cmp(b.0));
        for ((model, provider), tokens) in &input_sorted {
            out.push_str(&format!(
                "opentrace_tokens_total{{model=\"{}\",provider=\"{}\",type=\"input\"}} {}\n",
                model, provider, tokens
            ));
        }
        let mut output_sorted: Vec<_> = inner.output_tokens.iter().collect();
        output_sorted.sort_by(|a, b| a.0.cmp(b.0));
        for ((model, provider), tokens) in &output_sorted {
            out.push_str(&format!(
                "opentrace_tokens_total{{model=\"{}\",provider=\"{}\",type=\"output\"}} {}\n",
                model, provider, tokens
            ));
        }

        // --- opentrace_cost_usd_total ----------------------------------------
        out.push_str("# HELP opentrace_cost_usd_total Total estimated cost in USD\n");
        out.push_str("# TYPE opentrace_cost_usd_total counter\n");
        let mut cost_sorted: Vec<_> = inner.cost_usd.iter().collect();
        cost_sorted.sort_by(|a, b| a.0.cmp(b.0));
        for ((model, provider), cost) in &cost_sorted {
            out.push_str(&format!(
                "opentrace_cost_usd_total{{model=\"{}\",provider=\"{}\"}} {:.8}\n",
                model, provider, cost
            ));
        }

        // --- opentrace_latency_ms histogram ----------------------------------
        out.push_str("# HELP opentrace_latency_ms Request latency in milliseconds\n");
        out.push_str("# TYPE opentrace_latency_ms histogram\n");
        let mut lat_models: Vec<&String> = inner.latency_buckets.keys().collect();
        lat_models.sort();
        for model in &lat_models {
            let buckets = &inner.latency_buckets[*model];
            for (i, le) in LE_LABELS.iter().enumerate() {
                out.push_str(&format!(
                    "opentrace_latency_ms_bucket{{model=\"{}\",le=\"{}\"}} {}\n",
                    model, le, buckets[i]
                ));
            }
            out.push_str(&format!(
                "opentrace_latency_ms_sum{{model=\"{}\"}} {:.0}\n",
                model,
                inner.latency_sum.get(*model).unwrap_or(&0.0)
            ));
            out.push_str(&format!(
                "opentrace_latency_ms_count{{model=\"{}\"}} {}\n",
                model,
                inner.latency_count.get(*model).unwrap_or(&0)
            ));
        }

        // --- opentrace_ttft_ms histogram -------------------------------------
        out.push_str("# HELP opentrace_ttft_ms Time to first token in milliseconds\n");
        out.push_str("# TYPE opentrace_ttft_ms histogram\n");
        let mut ttft_models: Vec<&String> = inner.ttft_buckets.keys().collect();
        ttft_models.sort();
        for model in &ttft_models {
            let buckets = &inner.ttft_buckets[*model];
            for (i, le) in LE_LABELS.iter().enumerate() {
                out.push_str(&format!(
                    "opentrace_ttft_ms_bucket{{model=\"{}\",le=\"{}\"}} {}\n",
                    model, le, buckets[i]
                ));
            }
            out.push_str(&format!(
                "opentrace_ttft_ms_sum{{model=\"{}\"}} {:.0}\n",
                model,
                inner.ttft_sum.get(*model).unwrap_or(&0.0)
            ));
            out.push_str(&format!(
                "opentrace_ttft_ms_count{{model=\"{}\"}} {}\n",
                model,
                inner.ttft_count.get(*model).unwrap_or(&0)
            ));
        }

        // --- opentrace_dropped_records_total ---------------------------------
        out.push_str("# HELP opentrace_dropped_records_total Records dropped due to DB backpressure\n");
        out.push_str("# TYPE opentrace_dropped_records_total gauge\n");
        out.push_str(&format!(
            "opentrace_dropped_records_total {}\n",
            inner.dropped_records
        ));

        // --- opentrace_budget_* gauges (only when budget alerting is enabled) --
        if inner.budget_enabled {
            out.push_str("# HELP opentrace_budget_spent_usd Estimated spend since the start of the current budget period\n");
            out.push_str("# TYPE opentrace_budget_spent_usd gauge\n");
            out.push_str(&format!(
                "opentrace_budget_spent_usd{{period=\"{}\"}} {:.8}\n",
                inner.budget_period, inner.budget_spent_usd
            ));
            out.push_str("# HELP opentrace_budget_limit_usd Budget limit configured via --budget-alert-usd\n");
            out.push_str("# TYPE opentrace_budget_limit_usd gauge\n");
            out.push_str(&format!(
                "opentrace_budget_limit_usd{{period=\"{}\"}} {:.8}\n",
                inner.budget_period, inner.budget_limit_usd
            ));
        }

        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::{now_iso, CallRecord};

    fn make_call(model: &str, provider: &str, status: u16, latency_ms: u64) -> CallRecord {
        CallRecord {
            id: uuid::Uuid::new_v4().to_string(),
            timestamp: now_iso(),
            provider: provider.to_string(),
            model: model.to_string(),
            endpoint: "/v1/chat/completions".to_string(),
            status_code: status,
            latency_ms,
            ttft_ms: Some(latency_ms / 2),
            input_tokens: Some(100),
            output_tokens: Some(50),
            cost_usd: Some(0.001),
            request_body: None,
            response_body: None,
            error: None,
            provider_request_id: None,
            trace_id: None,
            parent_id: None,
        }
    }

    #[test]
    fn metrics_new_renders_empty_prometheus() {
        let m = MetricsState::new();
        let text = m.render_prometheus();
        assert!(text.contains("opentrace_calls_total"));
        assert!(text.contains("opentrace_dropped_records_total 0"));
    }

    #[test]
    fn metrics_record_updates_calls_counter() {
        let m = MetricsState::new();
        m.record(&make_call("gpt-4o", "openai", 200, 300));
        m.record(&make_call("gpt-4o", "openai", 200, 200));
        m.record(&make_call("gpt-4o", "openai", 500, 100));

        let text = m.render_prometheus();
        // 2 successful + 1 error
        assert!(
            text.contains(
                "opentrace_calls_total{model=\"gpt-4o\",provider=\"openai\",status_class=\"2xx\"} 2"
            ),
            "expected 2xx count=2 in:\n{}",
            text
        );
        assert!(
            text.contains(
                "opentrace_calls_total{model=\"gpt-4o\",provider=\"openai\",status_class=\"5xx\"} 1"
            ),
            "expected 5xx count=1 in:\n{}",
            text
        );
    }

    #[test]
    fn metrics_record_updates_latency_histogram() {
        let m = MetricsState::new();
        // 50ms call: falls in le=100, 500, 1000, 5000, +Inf
        m.record(&make_call("gpt-4o", "openai", 200, 50));
        // 300ms call: falls in le=500, 1000, 5000, +Inf (NOT le=100)
        m.record(&make_call("gpt-4o", "openai", 200, 300));

        let text = m.render_prometheus();
        // le="100": only the 50ms call
        assert!(
            text.contains("opentrace_latency_ms_bucket{model=\"gpt-4o\",le=\"100\"} 1"),
            "le=100 bucket should be 1:\n{}",
            text
        );
        // le="500": both calls
        assert!(
            text.contains("opentrace_latency_ms_bucket{model=\"gpt-4o\",le=\"500\"} 2"),
            "le=500 bucket should be 2:\n{}",
            text
        );
        // +Inf: both calls
        assert!(
            text.contains("opentrace_latency_ms_bucket{model=\"gpt-4o\",le=\"+Inf\"} 2"),
            "+Inf bucket should be 2:\n{}",
            text
        );
        // count and sum
        assert!(
            text.contains("opentrace_latency_ms_count{model=\"gpt-4o\"} 2"),
            "count should be 2"
        );
        assert!(
            text.contains("opentrace_latency_ms_sum{model=\"gpt-4o\"} 350"),
            "sum should be 350ms (50+300)"
        );
    }

    #[test]
    fn metrics_dropped_records_reflected() {
        let m = MetricsState::new();
        m.set_dropped_records(42);
        let text = m.render_prometheus();
        assert!(
            text.contains("opentrace_dropped_records_total 42"),
            "dropped records not reflected:\n{}",
            text
        );
    }

    #[test]
    fn budget_gauge_set_in_metrics() {
        let m = MetricsState::new();
        m.set_budget_spent(3.21, 50.0, "month");
        let text = m.render_prometheus();
        assert!(
            text.contains("opentrace_budget_spent_usd{period=\"month\"}"),
            "budget spent gauge not found:\n{}",
            text
        );
        assert!(
            text.contains("opentrace_budget_limit_usd{period=\"month\"}"),
            "budget limit gauge not found:\n{}",
            text
        );
    }

    #[test]
    fn budget_gauge_absent_when_not_enabled() {
        let m = MetricsState::new();
        let text = m.render_prometheus();
        assert!(
            !text.contains("opentrace_budget_spent_usd"),
            "budget gauge should not appear when not enabled"
        );
    }

    #[test]
    fn metrics_counts_calls_correctly() {
        let m = MetricsState::new();
        m.record(&make_call("gpt-4o", "openai", 200, 100));
        m.record(&make_call("gpt-4o", "openai", 200, 200));

        let text = m.render_prometheus();
        assert!(
            text.contains(
                "opentrace_calls_total{model=\"gpt-4o\",provider=\"openai\",status_class=\"2xx\"} 2"
            ),
            "expected 2 calls:\n{}",
            text
        );
    }

    #[test]
    fn metrics_tokens_accumulated() {
        let m = MetricsState::new();
        m.record(&make_call("gpt-4o", "openai", 200, 100));
        m.record(&make_call("gpt-4o", "openai", 200, 100));

        let text = m.render_prometheus();
        // Each call has 100 input tokens; 2 calls → 200 total
        assert!(
            text.contains(
                "opentrace_tokens_total{model=\"gpt-4o\",provider=\"openai\",type=\"input\"} 200"
            ),
            "input tokens not accumulated:\n{}",
            text
        );
    }
}
