//! `trace serve` — local web dashboard.
//!
//! Spins up a standalone axum HTTP server that reads from the existing
//! `~/.trace/trace.db` and exposes:
//!
//! - `GET /`              — Static HTML dashboard (inline, zero CDN deps)
//! - `GET /api/calls`     — `Vec<CallRecord>` JSON (supports filters)
//! - `GET /api/stats`     — `{ stats: Stats, models: Vec<ModelStats> }` JSON

use anyhow::Result;
use axum::{
    extract::{Query, State},
    response::Html,
    routing::get,
    Json, Router,
};
use serde::{Deserialize, Serialize};
use std::sync::{Arc, Mutex};

use crate::store::{CallRecord, ModelStats, QueryFilter, Stats, Store};

// ---------------------------------------------------------------------------
// Dashboard HTML (inline — zero external dependencies, works offline)
// ---------------------------------------------------------------------------

const DASHBOARD_HTML: &str = r#"<!DOCTYPE html>
<html lang="en">
<head>
<meta charset="UTF-8">
<meta name="viewport" content="width=device-width,initial-scale=1">
<title>OpenTrace</title>
<style>
*{box-sizing:border-box;margin:0;padding:0}
body{font-family:monospace;background:#0f0f0f;color:#e0e0e0;padding:1rem}
h1{color:#4fc3f7;margin-bottom:1rem;font-size:1.1rem;letter-spacing:.05em}
.cards{display:flex;gap:.75rem;margin-bottom:1rem;flex-wrap:wrap}
.card{background:#1a1a1a;border:1px solid #2a2a2a;border-radius:4px;padding:.75rem 1rem;min-width:150px}
.card-label{color:#666;font-size:.7rem;text-transform:uppercase;letter-spacing:.05em;margin-bottom:.25rem}
.card-value{color:#4fc3f7;font-size:1.3rem;font-weight:bold}
.filters{display:flex;gap:.5rem;align-items:center;margin-bottom:.75rem}
.filters input[type=text]{background:#1a1a1a;border:1px solid #333;color:#e0e0e0;padding:.3rem .5rem;border-radius:3px;font-family:monospace;font-size:.85rem}
.filters label{color:#888;font-size:.85rem;cursor:pointer;display:flex;align-items:center;gap:.25rem}
table{width:100%;border-collapse:collapse;font-size:.78rem}
th{background:#151515;color:#666;text-align:right;padding:.35rem .5rem;border-bottom:1px solid #2a2a2a;position:sticky;top:0;white-space:nowrap}
th:first-child,th:nth-child(3){text-align:left}
td{padding:.3rem .5rem;border-bottom:1px solid #1a1a1a;text-align:right;white-space:nowrap}
td:first-child,td:nth-child(3){text-align:left}
tr:hover td{background:#1c1c1c}
.ok{color:#66bb6a}.err{color:#ef5350}.dim{color:#555}
#status{color:#444;font-size:.7rem;margin-top:.5rem}
</style>
</head>
<body>
<h1>[ OpenTrace ]</h1>
<div class="cards">
  <div class="card"><div class="card-label">Total calls</div><div class="card-value" id="s-calls">-</div></div>
  <div class="card"><div class="card-label">Total cost</div><div class="card-value" id="s-cost">-</div></div>
  <div class="card"><div class="card-label">Avg latency</div><div class="card-value" id="s-lat">-</div></div>
  <div class="card"><div class="card-label">Calls (1h)</div><div class="card-value" id="s-hour">-</div></div>
</div>
<div class="filters">
  <input type="text" id="f-model" placeholder="Filter model..." oninput="refresh()">
  <label><input type="checkbox" id="f-errors" onchange="refresh()"> Errors only</label>
</div>
<table>
  <thead><tr>
    <th>id</th><th>timestamp</th><th>model</th><th>status</th>
    <th>latency</th><th>ttft</th><th>in</th><th>out</th><th>cost</th>
  </tr></thead>
  <tbody id="tbody"></tbody>
</table>
<div id="status">Loading...</div>
<script>
function fmtMs(v){return v==null?'-':String(v)+'ms'}
function fmtCost(c){return c==null?'-':'$'+Number(c).toFixed(4)}
function makeCell(text,cls){
  var td=document.createElement('td');
  td.textContent=text;
  if(cls)td.className=cls;
  return td;
}
async function refresh(){
  var model=document.getElementById('f-model').value;
  var errors=document.getElementById('f-errors').checked;
  var url='/api/calls?limit=100';
  if(model)url+='&model='+encodeURIComponent(model);
  if(errors)url+='&errors=true';
  try{
    var responses=await Promise.all([fetch(url),fetch('/api/stats')]);
    if(!responses[0].ok||!responses[1].ok)throw new Error('API error');
    var calls=await responses[0].json();
    var data=await responses[1].json();
    var stats=data.stats;
    document.getElementById('s-calls').textContent=String(stats.total_calls);
    document.getElementById('s-cost').textContent='$'+Number(stats.total_cost_usd).toFixed(4);
    document.getElementById('s-lat').textContent=Math.round(stats.avg_latency_ms)+'ms';
    document.getElementById('s-hour').textContent=String(stats.calls_last_hour);
    var tbody=document.getElementById('tbody');
    while(tbody.firstChild)tbody.removeChild(tbody.firstChild);
    for(var i=0;i<calls.length;i++){
      var r=calls[i];
      var ok=r.status_code>=200&&r.status_code<400&&!r.error;
      var tr=document.createElement('tr');
      tr.appendChild(makeCell((r.id||'').slice(0,8),'dim'));
      tr.appendChild(makeCell((r.timestamp||'').slice(0,19),'dim'));
      tr.appendChild(makeCell(r.model||'-',null));
      tr.appendChild(makeCell(String(r.status_code),ok?'ok':'err'));
      tr.appendChild(makeCell(fmtMs(r.latency_ms),null));
      tr.appendChild(makeCell(fmtMs(r.ttft_ms),'dim'));
      tr.appendChild(makeCell(r.input_tokens!=null?String(r.input_tokens):'-','dim'));
      tr.appendChild(makeCell(r.output_tokens!=null?String(r.output_tokens):'-','dim'));
      tr.appendChild(makeCell(fmtCost(r.cost_usd),null));
      tbody.appendChild(tr);
    }
    document.getElementById('status').textContent=
      'Updated '+new Date().toLocaleTimeString()+' - '+calls.length+' calls shown';
  }catch(e){
    document.getElementById('status').textContent='Error: '+String(e.message);
  }
}
refresh();
setInterval(refresh,2000);
</script>
</body>
</html>
"#;

// ---------------------------------------------------------------------------
// Shared state
// ---------------------------------------------------------------------------

#[derive(Clone)]
struct ServeState {
    store: Arc<Mutex<Store>>,
}

// ---------------------------------------------------------------------------
// API types
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
struct CallsQuery {
    limit: Option<usize>,
    model: Option<String>,
    errors: Option<bool>,
    since: Option<String>,
    until: Option<String>,
}

#[derive(Serialize)]
struct StatsResponse {
    stats: Stats,
    models: Vec<ModelStats>,
}

// ---------------------------------------------------------------------------
// Routes
// ---------------------------------------------------------------------------

fn router(state: ServeState) -> Router {
    Router::new()
        .route("/", get(dashboard_handler))
        .route("/api/calls", get(api_calls_handler))
        .route("/api/stats", get(api_stats_handler))
        .with_state(state)
}

async fn dashboard_handler() -> Html<&'static str> {
    Html(DASHBOARD_HTML)
}

async fn api_calls_handler(
    State(state): State<ServeState>,
    Query(params): Query<CallsQuery>,
) -> Json<Vec<CallRecord>> {
    let limit = params.limit.unwrap_or(100);
    let filter = QueryFilter {
        errors_only: params.errors.unwrap_or(false),
        model: params.model,
        since: params.since,
        until: params.until,
        ..Default::default()
    };
    let store = state.store.lock().unwrap();
    let records = store.query_filtered(limit, &filter).unwrap_or_default();
    Json(records)
}

async fn api_stats_handler(State(state): State<ServeState>) -> Json<StatsResponse> {
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
    Json(StatsResponse { stats, models })
}

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

pub async fn cmd_serve(port: u16) -> Result<()> {
    let store = Store::open()?;
    let state = ServeState {
        store: Arc::new(Mutex::new(store)),
    };

    let app = router(state);
    let addr = format!("127.0.0.1:{}", port);
    let listener = tokio::net::TcpListener::bind(&addr).await?;

    println!("OpenTrace dashboard running at http://localhost:{}", port);
    println!("Press Ctrl-C to stop.");

    axum::serve(listener, app).await?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::{now_iso, Store};
    use axum::body::to_bytes;
    use tower::ServiceExt; // for oneshot

    fn make_record(id: &str, model: &str, status_code: u16) -> CallRecord {
        CallRecord {
            id: id.to_string(),
            timestamp: now_iso(),
            provider: "openai".to_string(),
            model: model.to_string(),
            endpoint: "/v1/chat/completions".to_string(),
            status_code,
            latency_ms: 100,
            ttft_ms: None,
            input_tokens: Some(100),
            output_tokens: Some(50),
            cost_usd: Some(0.001),
            request_body: None,
            response_body: None,
            error: None,
            provider_request_id: None,
            trace_id: None,
            parent_id: None,
            prompt_hash: None,
        }
    }

    fn make_error_record(id: &str) -> CallRecord {
        let mut r = make_record(id, "gpt-4o", 500);
        r.error = Some("upstream error".to_string());
        r
    }

    fn test_app() -> (Router, Arc<Mutex<Store>>) {
        let store = Store::open_in_memory().unwrap();
        let store = Arc::new(Mutex::new(store));
        let state = ServeState {
            store: Arc::clone(&store),
        };
        (router(state), store)
    }

    #[tokio::test]
    async fn serve_api_calls_handler_returns_records() {
        let (app, store) = test_app();
        for i in 0..3u32 {
            store
                .lock()
                .unwrap()
                .insert(&make_record(&format!("id-{i}"), "gpt-4o", 200))
                .unwrap();
        }

        let resp = app
            .oneshot(
                axum::http::Request::builder()
                    .uri("/api/calls")
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(resp.status(), 200);
        let body = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        let records: Vec<serde_json::Value> = serde_json::from_slice(&body).unwrap();
        assert_eq!(records.len(), 3, "should return all 3 inserted records");
    }

    #[tokio::test]
    async fn serve_api_stats_has_expected_fields() {
        let (app, _store) = test_app();

        let resp = app
            .oneshot(
                axum::http::Request::builder()
                    .uri("/api/stats")
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(resp.status(), 200);
        let body = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert!(json["stats"]["total_calls"].is_number(), "total_calls should be present");
        assert!(json["models"].is_array(), "models should be an array");
    }

    #[tokio::test]
    async fn serve_errors_filter_applied() {
        let (app, store) = test_app();
        store.lock().unwrap().insert(&make_error_record("err-1")).unwrap();
        store
            .lock()
            .unwrap()
            .insert(&make_record("ok-1", "gpt-4o", 200))
            .unwrap();

        let resp = app
            .oneshot(
                axum::http::Request::builder()
                    .uri("/api/calls?errors=true")
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        let body = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        let records: Vec<serde_json::Value> = serde_json::from_slice(&body).unwrap();
        assert_eq!(records.len(), 1, "only the error record should be returned");
    }
}
