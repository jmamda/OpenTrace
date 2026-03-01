//! Embedded read-only query API mounted on the proxy server under `/_trace/`.
//!
//! Eliminates the need for client apps to access SQLite directly or run
//! `trace serve` separately.

use axum::{
    extract::{Path, Query, State},
    http::{header, StatusCode},
    middleware,
    response::{
        sse::{Event, KeepAlive, Sse},
        Json,
    },
    routing::get,
    Router,
};
use futures_util::StreamExt;
use serde::{Deserialize, Serialize};
use std::convert::Infallible;
use std::sync::{Arc, Mutex};
use std::time::Instant;
use tokio::sync::broadcast;
use tokio_stream::wrappers::BroadcastStream;

use crate::store::{
    AgentStats, CallRecord, DailyStat, ModelStats, ProviderStats, QueryFilter, SearchResult, Stats,
    Store, WorkflowSummary,
};

// ---------------------------------------------------------------------------
// State
// ---------------------------------------------------------------------------

#[derive(Clone)]
pub struct ApiState {
    pub store: Arc<Mutex<Store>>,
    pub event_tx: broadcast::Sender<CallRecord>,
    pub start_time: Instant,
}

// ---------------------------------------------------------------------------
// Query parameter structs
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
struct CallsQuery {
    limit: Option<usize>,
    model: Option<String>,
    provider: Option<String>,
    agent: Option<String>,
    workflow: Option<String>,
    tag: Option<String>,
    errors: Option<bool>,
    since: Option<String>,
    until: Option<String>,
}

#[derive(Deserialize)]
struct SearchQuery {
    q: String,
    limit: Option<usize>,
}

#[derive(Deserialize)]
struct HeatmapQuery {
    days: Option<u32>,
}

#[derive(Deserialize)]
struct WorkflowsQuery {
    limit: Option<usize>,
}

// ---------------------------------------------------------------------------
// Response structs
// ---------------------------------------------------------------------------

#[derive(Serialize)]
struct StatsResponse {
    stats: Stats,
    models: Vec<ModelStats>,
    p99_latency_ms: f64,
    providers: Vec<ProviderStats>,
}

#[derive(Serialize)]
struct SearchResponse {
    results: Vec<SearchResult>,
    count: usize,
}

#[derive(Serialize)]
struct HeatmapResponse {
    days: Vec<DailyStat>,
}

#[derive(Serialize)]
struct AgentsResponse {
    agents: Vec<AgentStats>,
}

#[derive(Serialize)]
struct WorkflowResponse {
    workflow_id: String,
    calls: Vec<CallRecord>,
    total_cost_usd: f64,
    total_latency_ms: u64,
    agents: Vec<String>,
}

#[derive(Serialize)]
struct WorkflowsResponse {
    workflows: Vec<WorkflowSummary>,
}

#[derive(Serialize)]
struct HealthResponse {
    version: String,
    uptime_seconds: u64,
    status: String,
}

// ---------------------------------------------------------------------------
// CORS middleware
// ---------------------------------------------------------------------------

async fn cors_layer(
    req: axum::http::Request<axum::body::Body>,
    next: middleware::Next,
) -> axum::response::Response {
    let mut resp = next.run(req).await;
    let headers = resp.headers_mut();
    headers.insert(header::ACCESS_CONTROL_ALLOW_ORIGIN, "*".parse().unwrap());
    headers.insert(
        header::ACCESS_CONTROL_ALLOW_HEADERS,
        "Authorization, Content-Type, x-api-key".parse().unwrap(),
    );
    headers.insert(
        header::ACCESS_CONTROL_ALLOW_METHODS,
        "GET, OPTIONS".parse().unwrap(),
    );
    resp
}

// ---------------------------------------------------------------------------
// Router
// ---------------------------------------------------------------------------

pub fn api_router(state: ApiState) -> Router {
    Router::new()
        .route("/_trace/api/calls", get(calls_handler))
        .route("/_trace/api/stats", get(stats_handler))
        .route("/_trace/api/call/:id", get(call_detail_handler))
        .route("/_trace/api/search", get(search_handler))
        .route("/_trace/api/heatmap", get(heatmap_handler))
        .route("/_trace/api/agents", get(agents_handler))
        .route("/_trace/api/workflow/:id", get(workflow_handler))
        .route("/_trace/api/workflows", get(workflows_handler))
        .route("/_trace/stream", get(stream_handler))
        .route("/_trace/health", get(health_handler))
        .layer(middleware::from_fn(cors_layer))
        .with_state(state)
}

// ---------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------

async fn calls_handler(
    State(state): State<ApiState>,
    Query(params): Query<CallsQuery>,
) -> Json<Vec<CallRecord>> {
    let filter = QueryFilter {
        errors_only: params.errors.unwrap_or(false),
        model: params.model,
        provider: params.provider,
        agent: params.agent,
        workflow: params.workflow,
        tag: params.tag,
        since: params.since,
        until: params.until,
        ..Default::default()
    };
    let limit = params.limit.unwrap_or(100);
    let store = state.store.lock().unwrap();
    let calls = store.query_filtered(limit, &filter).unwrap_or_default();
    Json(calls)
}

async fn stats_handler(State(state): State<ApiState>) -> Json<StatsResponse> {
    let store = state.store.lock().unwrap();
    let stats = store.stats().unwrap_or(Stats {
        total_calls: 0,
        total_input_tokens: 0,
        total_output_tokens: 0,
        total_cost_usd: 0.0,
        avg_latency_ms: 0.0,
        error_count: 0,
        calls_last_hour: 0,
    });
    let models = store.stats_by_model().unwrap_or_default();
    let (_, _, p99) = store
        .latency_percentiles(&QueryFilter::default())
        .unwrap_or((0.0, 0.0, 0.0));
    let providers = store.stats_by_provider().unwrap_or_default();
    Json(StatsResponse {
        stats,
        models,
        p99_latency_ms: p99,
        providers,
    })
}

async fn call_detail_handler(
    State(state): State<ApiState>,
    Path(id): Path<String>,
) -> Result<Json<CallRecord>, (StatusCode, String)> {
    let store = state.store.lock().unwrap();
    match store.get_by_id(&id) {
        Ok(Some(record)) => Ok(Json(record)),
        Ok(None) => Err((
            StatusCode::NOT_FOUND,
            format!("No call found with id prefix: {id}"),
        )),
        Err(e) => Err((StatusCode::INTERNAL_SERVER_ERROR, e.to_string())),
    }
}

async fn search_handler(
    State(state): State<ApiState>,
    Query(params): Query<SearchQuery>,
) -> Result<Json<SearchResponse>, (StatusCode, String)> {
    let limit = params.limit.unwrap_or(50).min(200);
    let store = state.store.lock().unwrap();
    match store.search_calls(&params.q, limit) {
        Ok(results) => {
            let count = results.len();
            Ok(Json(SearchResponse { results, count }))
        }
        Err(e) => {
            let msg = e.to_string();
            let code = if msg.contains("fts5:") || msg.contains("syntax error") {
                StatusCode::BAD_REQUEST
            } else {
                StatusCode::INTERNAL_SERVER_ERROR
            };
            Err((code, format!("Search error: {msg}")))
        }
    }
}

async fn heatmap_handler(
    State(state): State<ApiState>,
    Query(params): Query<HeatmapQuery>,
) -> Json<HeatmapResponse> {
    let days = params.days.unwrap_or(90).min(365);
    let store = state.store.lock().unwrap();
    let day_stats = store.daily_stats(days).unwrap_or_default();
    Json(HeatmapResponse { days: day_stats })
}

async fn agents_handler(State(state): State<ApiState>) -> Json<AgentsResponse> {
    let store = state.store.lock().unwrap();
    let agents = store
        .query_agent_stats(&QueryFilter::default())
        .unwrap_or_default();
    Json(AgentsResponse { agents })
}

async fn workflow_handler(
    State(state): State<ApiState>,
    Path(id): Path<String>,
) -> Result<Json<WorkflowResponse>, (StatusCode, String)> {
    let store = state.store.lock().unwrap();
    match store.query_workflow(&id) {
        Ok(calls) => {
            if calls.is_empty() {
                return Err((
                    StatusCode::NOT_FOUND,
                    format!("No calls found for workflow: {id}"),
                ));
            }
            let total_cost_usd: f64 = calls.iter().filter_map(|c| c.cost_usd).sum();
            let total_latency_ms: u64 = calls.iter().map(|c| c.latency_ms).sum();
            let mut agents: Vec<String> = calls
                .iter()
                .filter_map(|c| c.agent_name.clone())
                .collect::<std::collections::HashSet<_>>()
                .into_iter()
                .collect();
            agents.sort();
            Ok(Json(WorkflowResponse {
                workflow_id: id,
                calls,
                total_cost_usd,
                total_latency_ms,
                agents,
            }))
        }
        Err(e) => Err((StatusCode::INTERNAL_SERVER_ERROR, e.to_string())),
    }
}

async fn workflows_handler(
    State(state): State<ApiState>,
    Query(params): Query<WorkflowsQuery>,
) -> Json<WorkflowsResponse> {
    let limit = params.limit.unwrap_or(50);
    let store = state.store.lock().unwrap();
    let workflows = store.query_workflow_summaries(limit).unwrap_or_default();
    Json(WorkflowsResponse { workflows })
}

/// `GET /_trace/stream` — Server-Sent Events endpoint.
///
/// Each new `CallRecord` written to the database is broadcast to all
/// connected subscribers.  Slow consumers lag-skip rather than blocking
/// the broadcaster.  A 30-second keepalive ping prevents proxies and
/// browsers from closing idle connections.
async fn stream_handler(
    State(state): State<ApiState>,
) -> Sse<impl futures_util::Stream<Item = Result<Event, Infallible>>> {
    let rx = state.event_tx.subscribe();
    let stream = BroadcastStream::new(rx).filter_map(|result| {
        futures_util::future::ready(match result {
            Ok(record) => serde_json::to_string(&record)
                .ok()
                .map(|json| Ok(Event::default().data(json))),
            Err(_) => None,
        })
    });
    Sse::new(stream).keep_alive(
        KeepAlive::new()
            .interval(std::time::Duration::from_secs(30))
            .text("ping"),
    )
}

async fn health_handler(State(state): State<ApiState>) -> Json<HealthResponse> {
    Json(HealthResponse {
        version: env!("CARGO_PKG_VERSION").to_string(),
        uptime_seconds: state.start_time.elapsed().as_secs(),
        status: "ok".to_string(),
    })
}
