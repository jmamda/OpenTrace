//! `trace serve` — local web dashboard.
//!
//! Spins up a standalone axum HTTP server that reads from the existing
//! `~/.trace/trace.db` and exposes:
//!
//! - `GET /`              — Static HTML dashboard (inline, zero CDN deps)
//! - `GET /api/calls`     — `Vec<CallRecord>` JSON (supports filters)
//! - `GET /api/stats`     — `{ stats: Stats, models: Vec<ModelStats> }` JSON
//! - `GET /api/search`    — FTS5 full-text search over prompts + responses
//! - `GET /api/heatmap`   — Daily cost/call aggregates for the heatmap
//! - `GET /playground`    — Side-by-side model playground page

use anyhow::Result;
use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    response::Html,
    routing::get,
    Json, Router,
};
use serde::{Deserialize, Serialize};
use std::sync::{Arc, Mutex};

use crate::store::{CallRecord, DailyStat, ModelStats, QueryFilter, SearchResult, Stats, Store};

// ---------------------------------------------------------------------------
// Dashboard HTML
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
.header{display:flex;justify-content:space-between;align-items:center;margin-bottom:1rem;flex-wrap:wrap;gap:.5rem}
h1{color:#4fc3f7;font-size:1.1rem;letter-spacing:.05em}
.nav{display:flex;align-items:center;gap:.75rem}
.nav-link{color:#4fc3f7;text-decoration:none;font-size:.85rem}
.nav-link:hover{text-decoration:underline}
#dark-toggle{background:none;border:1px solid #333;color:#888;padding:.2rem .5rem;border-radius:3px;cursor:pointer;font-size:.8rem}
.cards{display:flex;gap:.75rem;margin-bottom:.75rem;flex-wrap:wrap}
.card{background:#1a1a1a;border:1px solid #2a2a2a;border-radius:4px;padding:.75rem 1rem;min-width:150px}
.card-label{color:#666;font-size:.7rem;text-transform:uppercase;letter-spacing:.05em;margin-bottom:.25rem}
.card-value{color:#4fc3f7;font-size:1.3rem;font-weight:bold}
.section-label{color:#555;font-size:.65rem;text-transform:uppercase;letter-spacing:.08em;margin-bottom:.3rem;margin-top:.25rem}
#heatmap-wrap{margin-bottom:.75rem;overflow-x:auto}
#heatmap{display:block}
.hm-cell{cursor:default}
.filters{display:flex;gap:.5rem;align-items:center;margin-bottom:.75rem;flex-wrap:wrap}
.filters input[type=text]{background:#1a1a1a;border:1px solid #333;color:#e0e0e0;padding:.3rem .5rem;border-radius:3px;font-family:monospace;font-size:.85rem}
#f-search{width:260px}
#f-model{width:160px}
.filters label{color:#888;font-size:.85rem;cursor:pointer;display:flex;align-items:center;gap:.25rem}
table{width:100%;border-collapse:collapse;font-size:.78rem}
th{background:#151515;color:#666;text-align:right;padding:.35rem .5rem;border-bottom:1px solid #2a2a2a;position:sticky;top:0;white-space:nowrap}
th:first-child,th:nth-child(3){text-align:left}
td{padding:.3rem .5rem;border-bottom:1px solid #1a1a1a;text-align:right;white-space:nowrap}
td:first-child,td:nth-child(3){text-align:left}
tr:hover td{background:#1c1c1c}
.ok{color:#66bb6a}.err{color:#ef5350}.dim{color:#555}
#status{color:#444;font-size:.7rem;margin-top:.5rem}
body.light{background:#f5f5f5;color:#222}
body.light .card{background:#fff;border-color:#ddd}
body.light .card-label{color:#999}
body.light .card-value{color:#0077b6}
body.light th{background:#eee;color:#888}
body.light td{border-color:#eee}
body.light tr:hover td{background:#fafafa}
body.light .filters input[type=text]{background:#fff;border-color:#ccc;color:#222}
body.light #dark-toggle{border-color:#ccc;color:#666}
body.light #status{color:#aaa}
@media(max-width:600px){
  .cards{flex-direction:column}
  table{display:block;overflow-x:auto}
  .header{flex-direction:column;align-items:flex-start}
  #f-search{width:100%}
}
</style>
</head>
<body>
<div class="header">
  <h1>[ OpenTrace ]</h1>
  <div class="nav">
    <a href="/playground" class="nav-link">Playground →</a>
    <button id="dark-toggle" onclick="toggleDark()">☀</button>
  </div>
</div>
<div class="cards">
  <div class="card"><div class="card-label">Total calls</div><div class="card-value" id="s-calls">-</div></div>
  <div class="card"><div class="card-label">Total cost</div><div class="card-value" id="s-cost">-</div></div>
  <div class="card"><div class="card-label">Avg latency</div><div class="card-value" id="s-lat">-</div></div>
  <div class="card"><div class="card-label">Calls (1h)</div><div class="card-value" id="s-hour">-</div></div>
</div>
<div id="heatmap-wrap">
  <div class="section-label">DAILY COST — 90 days</div>
  <svg id="heatmap" height="88"></svg>
</div>
<div class="filters">
  <input type="text" id="f-search" placeholder="Search prompts, responses, models..." oninput="debouncedSearch()">
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
<div id="detail-panel" style="display:none;position:fixed;top:0;right:0;width:420px;height:100vh;background:#111;border-left:1px solid #2a2a2a;overflow-y:auto;padding:1rem;z-index:100;font-size:.78rem">
  <div style="display:flex;justify-content:space-between;align-items:center;margin-bottom:.75rem">
    <span style="color:#4fc3f7;font-weight:bold">Call Detail</span>
    <button onclick="closeDetail()" style="background:none;border:none;color:#888;cursor:pointer;font-size:1rem">X</button>
  </div>
  <div id="detail-content"></div>
</div>
<script>
function fmtMs(v){return v==null?'-':v+'ms'}
function fmtCost(c){return c==null?'-':'$'+Number(c).toFixed(4)}
function makeCell(text,cls){var td=document.createElement('td');td.textContent=text;if(cls)td.className=cls;return td;}
function makeTr(label,val){
  var tr=document.createElement('tr');
  var td1=document.createElement('td');td1.style.color='#666';td1.style.padding='.2rem .4rem';td1.style.whiteSpace='nowrap';td1.style.verticalAlign='top';td1.textContent=label;
  var td2=document.createElement('td');td2.style.color='#e0e0e0';td2.style.padding='.2rem .4rem';td2.style.wordBreak='break-all';td2.textContent=String(val);
  tr.appendChild(td1);tr.appendChild(td2);return tr;
}

// dark mode
function toggleDark(){
  document.body.classList.toggle('light');
  var light=document.body.classList.contains('light');
  localStorage.setItem('ot-theme',light?'light':'dark');
  document.getElementById('dark-toggle').textContent=light?'☾':'☀';
}
(function(){
  if(localStorage.getItem('ot-theme')==='light'){
    document.body.classList.add('light');
    document.getElementById('dark-toggle').textContent='☾';
  }
})();

// detail panel
function closeDetail(){document.getElementById('detail-panel').style.display='none';}
async function showDetail(id){
  var panel=document.getElementById('detail-panel');
  var content=document.getElementById('detail-content');
  content.textContent='Loading...';
  panel.style.display='block';
  try{
    var res=await fetch('/api/detail/'+id);
    if(!res.ok){content.textContent='Error: '+res.status;return;}
    var r=await res.json();
    content.textContent='';
    var tbl=document.createElement('table');tbl.style.width='100%';tbl.style.borderCollapse='collapse';
    var rows=[['id',r.id],['timestamp',r.timestamp],['provider',r.provider],['model',r.model],['endpoint',r.endpoint],['status',r.status_code],['latency',fmtMs(r.latency_ms)],['ttft',fmtMs(r.ttft_ms)],['input tokens',r.input_tokens],['output tokens',r.output_tokens],['cost',fmtCost(r.cost_usd)],['error',r.error||null]];
    rows.forEach(function(row){if(row[1]!=null){tbl.appendChild(makeTr(row[0],row[1]));}});
    content.appendChild(tbl);
    if(r.request_body){var lbl=document.createElement('div');lbl.style.color='#666';lbl.style.marginTop='.75rem';lbl.style.marginBottom='.25rem';lbl.textContent='request';content.appendChild(lbl);var pre=document.createElement('pre');pre.style.background='#1a1a1a';pre.style.padding='.5rem';pre.style.borderRadius='3px';pre.style.fontSize='.72rem';pre.style.overflowX='auto';pre.style.whiteSpace='pre-wrap';pre.textContent=r.request_body;content.appendChild(pre);}
    if(r.response_body){var lbl2=document.createElement('div');lbl2.style.color='#666';lbl2.style.marginTop='.75rem';lbl2.style.marginBottom='.25rem';lbl2.textContent='response';content.appendChild(lbl2);var pre2=document.createElement('pre');pre2.style.background='#1a1a1a';pre2.style.padding='.5rem';pre2.style.borderRadius='3px';pre2.style.fontSize='.72rem';pre2.style.overflowX='auto';pre2.style.whiteSpace='pre-wrap';pre2.textContent=r.response_body;content.appendChild(pre2);}
  }catch(e){content.textContent='Error: '+e.message;}
}

// row rendering (shared by refresh and search)
function renderRows(calls){
  var tbody=document.getElementById('tbody');
  while(tbody.firstChild)tbody.removeChild(tbody.firstChild);
  for(var i=0;i<calls.length;i++){
    var r=calls[i];
    var ok=r.status_code>=200&&r.status_code<400&&!r.error;
    var tr=document.createElement('tr');
    tr.style.cursor='pointer';
    (function(id){tr.addEventListener('click',function(){showDetail(id);});})(r.id);
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
}

// search (debounced 300ms)
var searchTimer=null;
function debouncedSearch(){clearTimeout(searchTimer);searchTimer=setTimeout(runSearch,300);}
async function runSearch(){
  var q=document.getElementById('f-search').value.trim();
  if(!q){refresh();return;}
  try{
    var res=await fetch('/api/search?q='+encodeURIComponent(q)+'&limit=100');
    var data=await res.json();
    if(!res.ok){document.getElementById('status').textContent='Search error: '+(data||'');return;}
    renderRows((data.results||[]).map(function(r){return r.record;}));
    document.getElementById('status').textContent='Search: '+data.count+' result(s) for "'+q+'"';
  }catch(e){document.getElementById('status').textContent='Search error: '+e.message;}
}

// main refresh
async function refresh(){
  if(document.getElementById('f-search').value.trim()){return;}
  var model=document.getElementById('f-model').value;
  var errors=document.getElementById('f-errors').checked;
  var url='/api/calls?limit=100';
  if(model)url+='&model='+encodeURIComponent(model);
  if(errors)url+='&errors=true';
  try{
    var [r1,r2]=await Promise.all([fetch(url),fetch('/api/stats')]);
    var calls=await r1.json();
    var data=await r2.json();
    var s=data.stats;
    document.getElementById('s-calls').textContent=String(s.total_calls);
    document.getElementById('s-cost').textContent='$'+Number(s.total_cost_usd).toFixed(4);
    document.getElementById('s-lat').textContent=Math.round(s.avg_latency_ms)+'ms';
    document.getElementById('s-hour').textContent=String(s.calls_last_hour);
    renderRows(calls);
    document.getElementById('status').textContent='Updated '+new Date().toLocaleTimeString()+' — '+calls.length+' calls';
  }catch(e){document.getElementById('status').textContent='Error: '+e.message;}
}

// heatmap
async function loadHeatmap(){
  try{
    var res=await fetch('/api/heatmap?days=90');
    if(!res.ok)return;
    renderHeatmap((await res.json()).days);
  }catch(e){}
}
function renderHeatmap(days){
  var svg=document.getElementById('heatmap');
  while(svg.firstChild)svg.removeChild(svg.firstChild);
  if(!days||!days.length)return;
  var map={},maxCost=0;
  days.forEach(function(d){map[d.date]=d;if(d.cost_usd>maxCost)maxCost=d.cost_usd;});
  var cells=[],now=new Date();
  for(var i=89;i>=0;i--){
    var d=new Date(now);d.setDate(now.getDate()-i);
    var key=d.toISOString().slice(0,10);
    cells.push({date:key,stat:map[key]||{calls:0,cost_usd:0,error_count:0}});
  }
  var cs=10,gap=2,step=cs+gap;
  var weeks=Math.ceil(cells.length/7);
  svg.setAttribute('width',String(weeks*step));
  svg.setAttribute('height','88');
  cells.forEach(function(cell,idx){
    var col=Math.floor(idx/7),row=idx%7;
    var x=col*step,y=row*step+16;
    var intensity=maxCost>0?cell.stat.cost_usd/maxCost:0;
    var hasErr=cell.stat.error_count>0;
    var fill=intensity<0.001?'#1e1e1e':
      'hsl('+(hasErr?0:200)+',60%,'+Math.round(15+intensity*45)+'%)';
    var g=document.createElementNS('http://www.w3.org/2000/svg','g');
    g.setAttribute('class','hm-cell');
    var rect=document.createElementNS('http://www.w3.org/2000/svg','rect');
    rect.setAttribute('x',x);rect.setAttribute('y',y);
    rect.setAttribute('width',cs);rect.setAttribute('height',cs);
    rect.setAttribute('rx','2');rect.setAttribute('fill',fill);
    var title=document.createElementNS('http://www.w3.org/2000/svg','title');
    title.textContent=cell.date+': '+cell.stat.calls+' calls, $'+Number(cell.stat.cost_usd).toFixed(4)+(cell.stat.error_count>0?' ('+cell.stat.error_count+' errors)':'');
    g.appendChild(rect);g.appendChild(title);svg.appendChild(g);
  });
  // month labels
  var lastMonth='';
  cells.forEach(function(cell,idx){
    var col=Math.floor(idx/7);
    var mo=cell.date.slice(0,7);
    if(mo!==lastMonth){
      lastMonth=mo;
      var txt=document.createElementNS('http://www.w3.org/2000/svg','text');
      txt.setAttribute('x',col*step);txt.setAttribute('y','11');
      txt.setAttribute('fill','#555');txt.setAttribute('font-size','9');
      txt.textContent=cell.date.slice(5,7)+'/'+cell.date.slice(2,4);
      svg.appendChild(txt);
    }
  });
}

refresh();
loadHeatmap();
setInterval(refresh,2000);
</script>
</body>
</html>
"#;

// ---------------------------------------------------------------------------
// Playground HTML
// ---------------------------------------------------------------------------

const PLAYGROUND_HTML: &str = r#"<!DOCTYPE html>
<html lang="en">
<head>
<meta charset="UTF-8">
<meta name="viewport" content="width=device-width,initial-scale=1">
<title>OpenTrace Playground</title>
<style>
*{box-sizing:border-box;margin:0;padding:0}
body{font-family:monospace;background:#0f0f0f;color:#e0e0e0;padding:1rem}
h1{color:#4fc3f7;font-size:1.1rem;margin-bottom:1rem}
.back{color:#4fc3f7;text-decoration:none;font-size:.8rem;display:inline-block;margin-bottom:.75rem}
.cfg{display:flex;gap:.5rem;flex-wrap:wrap;align-items:center;margin-bottom:.75rem}
.cfg label{color:#666;font-size:.75rem}
.cfg input{background:#1a1a1a;border:1px solid #333;color:#e0e0e0;padding:.3rem .5rem;border-radius:3px;font-family:monospace;font-size:.8rem}
.models{display:grid;grid-template-columns:1fr 1fr;gap:.5rem;margin-bottom:.5rem}
.models input{background:#1a1a1a;border:1px solid #333;color:#e0e0e0;padding:.3rem .5rem;border-radius:3px;font-family:monospace;font-size:.85rem;width:100%}
textarea{width:100%;height:100px;background:#1a1a1a;border:1px solid #333;color:#e0e0e0;padding:.5rem;border-radius:3px;font-family:monospace;font-size:.85rem;resize:vertical;margin-bottom:.5rem}
button#run{background:#0077b6;color:#fff;border:none;padding:.4rem 1.2rem;border-radius:3px;cursor:pointer;font-family:monospace;font-size:.85rem}
button#run:disabled{opacity:.5;cursor:not-allowed}
.panels{display:grid;grid-template-columns:1fr 1fr;gap:.75rem;margin-top:.75rem}
.panel{background:#1a1a1a;border:1px solid #2a2a2a;border-radius:4px;padding:.75rem}
.panel-hdr{color:#4fc3f7;font-size:.75rem;margin-bottom:.4rem;display:flex;justify-content:space-between;align-items:baseline}
.panel-meta{color:#555;font-size:.7rem;display:flex;gap:.6rem}
.panel-body{color:#e0e0e0;font-size:.8rem;white-space:pre-wrap;min-height:80px;max-height:400px;overflow-y:auto;margin-top:.5rem;border-top:1px solid #2a2a2a;padding-top:.5rem}
.ok{color:#66bb6a}.err{color:#ef5350}
@media(max-width:600px){.panels{grid-template-columns:1fr}.models{grid-template-columns:1fr}}
</style>
</head>
<body>
<a href="/" class="back">← Dashboard</a>
<h1>[ Playground ]</h1>
<div class="cfg">
  <label>Proxy URL</label>
  <input id="proxy" type="text" value="http://localhost:4000" style="width:200px">
  <label>API Key</label>
  <input id="apikey" type="password" placeholder="sk-..." style="width:180px">
</div>
<div class="models">
  <input id="model-a" type="text" placeholder="Model A (e.g. gpt-4o)" value="gpt-4o">
  <input id="model-b" type="text" placeholder="Model B (e.g. gpt-4o-mini)" value="gpt-4o-mini">
</div>
<textarea id="prompt" placeholder="Enter your prompt here...">Explain the difference between a mutex and a semaphore in two sentences.</textarea>
<button id="run" onclick="runPlayground()">Run ▶</button>
<div class="panels">
  <div class="panel">
    <div class="panel-hdr">
      <span id="label-a">Model A</span>
      <div class="panel-meta">
        <span>TTFT <span id="ttft-a">-</span></span>
        <span>Total <span id="lat-a">-</span></span>
        <span>Chunks~ <span id="tok-a">-</span></span>
      </div>
    </div>
    <div class="panel-body" id="body-a"></div>
  </div>
  <div class="panel">
    <div class="panel-hdr">
      <span id="label-b">Model B</span>
      <div class="panel-meta">
        <span>TTFT <span id="ttft-b">-</span></span>
        <span>Total <span id="lat-b">-</span></span>
        <span>Chunks~ <span id="tok-b">-</span></span>
      </div>
    </div>
    <div class="panel-body" id="body-b"></div>
  </div>
</div>
<script>
async function streamModel(proxy,apiKey,model,prompt,bodyEl,ttftEl,latEl,tokEl,labelEl){
  labelEl.textContent=model;
  bodyEl.textContent='';bodyEl.className='panel-body';
  ttftEl.textContent='-';latEl.textContent='-';tokEl.textContent='-';
  var t0=performance.now(),ttft=null,chunks=0;
  try{
    var res=await fetch(proxy+'/v1/chat/completions',{
      method:'POST',
      headers:{'Content-Type':'application/json','Authorization':'Bearer '+apiKey},
      body:JSON.stringify({model:model,stream:true,messages:[{role:'user',content:prompt}]})
    });
    if(!res.ok){
      bodyEl.className='panel-body err';
      bodyEl.textContent='HTTP '+res.status+': '+(await res.text());
      return;
    }
    var reader=res.body.getReader(),decoder=new TextDecoder(),buf='';
    while(true){
      var chunk=await reader.read();
      if(chunk.done)break;
      buf+=decoder.decode(chunk.value,{stream:true});
      var lines=buf.split('\n');buf=lines.pop();
      for(var i=0;i<lines.length;i++){
        var line=lines[i].trim();
        if(!line||line==='data: [DONE]')continue;
        if(!line.startsWith('data: '))continue;
        try{
          var obj=JSON.parse(line.slice(6));
          var delta=obj.choices&&obj.choices[0]&&obj.choices[0].delta;
          if(delta&&delta.content){
            if(ttft===null){ttft=performance.now()-t0;ttftEl.textContent=Math.round(ttft)+'ms';}
            bodyEl.textContent+=delta.content;
            chunks++;
          }
        }catch(e){}
      }
    }
    latEl.textContent=Math.round(performance.now()-t0)+'ms';
    tokEl.textContent='~'+chunks;
    bodyEl.className='panel-body ok';
  }catch(e){bodyEl.className='panel-body err';bodyEl.textContent='Error: '+e.message;}
}
async function runPlayground(){
  var btn=document.getElementById('run');
  btn.disabled=true;
  await Promise.all([
    streamModel(
      document.getElementById('proxy').value.replace(/\/$/,''),
      document.getElementById('apikey').value,
      document.getElementById('model-a').value,
      document.getElementById('prompt').value,
      document.getElementById('body-a'),
      document.getElementById('ttft-a'),
      document.getElementById('lat-a'),
      document.getElementById('tok-a'),
      document.getElementById('label-a')),
    streamModel(
      document.getElementById('proxy').value.replace(/\/$/,''),
      document.getElementById('apikey').value,
      document.getElementById('model-b').value,
      document.getElementById('prompt').value,
      document.getElementById('body-b'),
      document.getElementById('ttft-b'),
      document.getElementById('lat-b'),
      document.getElementById('tok-b'),
      document.getElementById('label-b')),
  ]);
  btn.disabled=false;
}
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

#[derive(Deserialize)]
struct SearchQuery {
    q: String,
    limit: Option<usize>,
}

#[derive(Deserialize)]
struct HeatmapQuery {
    days: Option<u32>,
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

// ---------------------------------------------------------------------------
// Routes
// ---------------------------------------------------------------------------

fn router(state: ServeState) -> Router {
    Router::new()
        .route("/", get(dashboard_handler))
        .route("/api/calls", get(api_calls_handler))
        .route("/api/stats", get(api_stats_handler))
        .route("/api/search", get(api_search_handler))
        .route("/api/heatmap", get(api_heatmap_handler))
        .route("/api/detail/:id", get(api_detail_handler))
        .route("/playground", get(playground_handler))
        .with_state(state)
}

async fn dashboard_handler() -> Html<&'static str> {
    Html(DASHBOARD_HTML)
}

async fn playground_handler() -> Html<&'static str> {
    Html(PLAYGROUND_HTML)
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

async fn api_search_handler(
    State(state): State<ServeState>,
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

async fn api_heatmap_handler(
    State(state): State<ServeState>,
    Query(params): Query<HeatmapQuery>,
) -> Json<HeatmapResponse> {
    let days = params.days.unwrap_or(90).min(365);
    let store = state.store.lock().unwrap();
    let day_stats = store.daily_stats(days).unwrap_or_default();
    Json(HeatmapResponse { days: day_stats })
}

async fn api_detail_handler(
    State(state): State<ServeState>,
    Path(id): Path<String>,
) -> Result<Json<CallRecord>, (StatusCode, String)> {
    let store = state.store.lock().unwrap();
    match store.get_by_id(&id) {
        Ok(Some(record)) => Ok(Json(record)),
        Ok(None) => Err((StatusCode::NOT_FOUND, format!("No call found with id prefix: {id}"))),
        Err(e) => Err((StatusCode::INTERNAL_SERVER_ERROR, e.to_string())),
    }
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

    println!("OpenTrace dashboard  → http://localhost:{}", port);
    println!("Playground           → http://localhost:{}/playground", port);
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
    use tower::ServiceExt;

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
        let state = ServeState { store: Arc::clone(&store) };
        (router(state), store)
    }

    #[tokio::test]
    async fn serve_api_calls_handler_returns_records() {
        let (app, store) = test_app();
        for i in 0..3u32 {
            store.lock().unwrap().insert(&make_record(&format!("id-{i}"), "gpt-4o", 200)).unwrap();
        }
        let resp = app.oneshot(
            axum::http::Request::builder().uri("/api/calls").body(axum::body::Body::empty()).unwrap(),
        ).await.unwrap();
        assert_eq!(resp.status(), 200);
        let body = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        let records: Vec<serde_json::Value> = serde_json::from_slice(&body).unwrap();
        assert_eq!(records.len(), 3);
    }

    #[tokio::test]
    async fn serve_api_stats_has_expected_fields() {
        let (app, _) = test_app();
        let resp = app.oneshot(
            axum::http::Request::builder().uri("/api/stats").body(axum::body::Body::empty()).unwrap(),
        ).await.unwrap();
        assert_eq!(resp.status(), 200);
        let body = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert!(json["stats"]["total_calls"].is_number());
        assert!(json["models"].is_array());
    }

    #[tokio::test]
    async fn serve_errors_filter_applied() {
        let (app, store) = test_app();
        store.lock().unwrap().insert(&make_error_record("err-1")).unwrap();
        store.lock().unwrap().insert(&make_record("ok-1", "gpt-4o", 200)).unwrap();
        let resp = app.oneshot(
            axum::http::Request::builder().uri("/api/calls?errors=true").body(axum::body::Body::empty()).unwrap(),
        ).await.unwrap();
        let body = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        let records: Vec<serde_json::Value> = serde_json::from_slice(&body).unwrap();
        assert_eq!(records.len(), 1);
    }

    #[tokio::test]
    async fn serve_api_search_returns_results() {
        let (app, store) = test_app();
        store.lock().unwrap().insert(&make_record("srch-1", "gpt-4o-serve-search-test", 200)).unwrap();
        let resp = app.oneshot(
            axum::http::Request::builder()
                .uri("/api/search?q=gpt-4o-serve-search-test")
                .body(axum::body::Body::empty()).unwrap(),
        ).await.unwrap();
        assert_eq!(resp.status(), 200);
        let body = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        let data: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert!(data["count"].as_i64().unwrap() >= 1);
    }

    #[tokio::test]
    async fn serve_api_heatmap_returns_array() {
        let (app, store) = test_app();
        store.lock().unwrap().insert(&make_record("hm-1", "gpt-4o", 200)).unwrap();
        let resp = app.oneshot(
            axum::http::Request::builder().uri("/api/heatmap?days=7").body(axum::body::Body::empty()).unwrap(),
        ).await.unwrap();
        assert_eq!(resp.status(), 200);
        let body = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        let data: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert!(data["days"].is_array());
    }

    #[tokio::test]
    async fn serve_playground_route_returns_200() {
        let (app, _) = test_app();
        let resp = app.oneshot(
            axum::http::Request::builder().uri("/playground").body(axum::body::Body::empty()).unwrap(),
        ).await.unwrap();
        assert_eq!(resp.status(), 200);
    }

    #[tokio::test]
    async fn serve_api_detail_returns_record() {
        let (app, store) = test_app();
        store.lock().unwrap().insert(&make_record("detail-test-id-1234", "gpt-4o", 200)).unwrap();
        let resp = app.oneshot(
            axum::http::Request::builder()
                .uri("/api/detail/detail-test-id-1234")
                .body(axum::body::Body::empty()).unwrap(),
        ).await.unwrap();
        assert_eq!(resp.status(), 200);
        let body = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        let record: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(record["model"].as_str().unwrap(), "gpt-4o");
    }

    #[tokio::test]
    async fn serve_api_detail_returns_404_for_unknown_id() {
        let (app, _) = test_app();
        let resp = app.oneshot(
            axum::http::Request::builder()
                .uri("/api/detail/nonexistent-id")
                .body(axum::body::Body::empty()).unwrap(),
        ).await.unwrap();
        assert_eq!(resp.status(), 404);
    }
}
