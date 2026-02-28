//! `trace serve` — local web dashboard.
//!
//! Spins up a standalone axum HTTP server that reads from the existing
//! `~/.trace/trace.db` and exposes:
//!
//! - `GET /`              — Static HTML dashboard (inline, zero CDN deps)
//! - `GET /stream`        — Server-Sent Events stream of new CallRecords (real-time)
//! - `GET /api/calls`     — `Vec<CallRecord>` JSON (supports filters)
//! - `GET /api/stats`     — `{ stats: Stats, models: Vec<ModelStats> }` JSON
//! - `GET /api/search`    — FTS5 full-text search over prompts + responses
//! - `GET /api/heatmap`   — Daily cost/call aggregates for the heatmap
//! - `GET /playground`    — Side-by-side model playground page

use anyhow::Result;
use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    response::{
        sse::{Event, KeepAlive, Sse},
        Html,
    },
    routing::get,
    Json, Router,
};
use futures_util::StreamExt;
use serde::{Deserialize, Serialize};
use std::convert::Infallible;
use std::sync::{Arc, Mutex};
use tokio::sync::broadcast;
use tokio_stream::wrappers::BroadcastStream;

use crate::store::{
    AgentStats, CallRecord, DailyStat, ModelStats, ProviderStats, QueryFilter, SearchResult, Stats,
    Store,
};

// ---------------------------------------------------------------------------
// Dashboard HTML
// ---------------------------------------------------------------------------

const DASHBOARD_HTML: &str = r#"<!DOCTYPE html>
<html lang="en" data-theme="dark">
<head>
<meta charset="UTF-8">
<meta name="viewport" content="width=device-width,initial-scale=1">
<title>OpenTrace</title>
<script src="https://cdn.jsdelivr.net/npm/chart.js@4"></script>
<style>
:root{
  --bg-primary:#0f0f0f;--bg-secondary:#1a1a1a;--text-primary:#e0e0e0;--text-muted:#555;
  --accent:#4fc3f7;--border:#2a2a2a;--card-bg:#1a1a1a;--error:#ef5350;--success:#66bb6a;--warning:#ffd54f;
  --th-bg:#151515;--hover-bg:#1c1c1c;--input-bg:#1a1a1a;--input-border:#333;
}
[data-theme="light"]{
  --bg-primary:#f5f5f5;--bg-secondary:#fff;--text-primary:#222;--text-muted:#999;
  --accent:#0077b6;--border:#ddd;--card-bg:#fff;--error:#d32f2f;--success:#388e3c;--warning:#f9a825;
  --th-bg:#eee;--hover-bg:#fafafa;--input-bg:#fff;--input-border:#ccc;
}
*{box-sizing:border-box;margin:0;padding:0}
body{font-family:'JetBrains Mono','Fira Code','SF Mono','Cascadia Code',monospace;background:var(--bg-primary);color:var(--text-primary);padding:1rem}
.header{display:flex;justify-content:space-between;align-items:center;margin-bottom:1rem;flex-wrap:wrap;gap:.5rem}
h1{color:var(--accent);font-size:1.1rem;letter-spacing:.05em}
.nav{display:flex;align-items:center;gap:.75rem}
.nav-link{color:var(--accent);text-decoration:none;font-size:.85rem}
.nav-link:hover{text-decoration:underline}
#dark-toggle{background:none;border:1px solid var(--input-border);color:var(--text-muted);padding:.2rem .5rem;border-radius:3px;cursor:pointer;font-size:.8rem}
.cards-grid{display:grid;grid-template-columns:repeat(3,1fr);gap:.75rem;margin-bottom:.5rem}
.card{background:var(--card-bg);border:1px solid var(--border);border-radius:4px;padding:.75rem 1rem}
.card-label{color:var(--text-muted);font-size:.7rem;text-transform:uppercase;letter-spacing:.05em;margin-bottom:.25rem}
.card-value{color:var(--accent);font-size:1.3rem;font-weight:bold}
.card-spark{height:40px;margin-top:.25rem}
.section-label{color:var(--text-muted);font-size:.65rem;text-transform:uppercase;letter-spacing:.08em;margin-bottom:.3rem;margin-top:.25rem}
#charts-row{display:grid;grid-template-columns:1fr 1fr 1fr;gap:.75rem;margin-bottom:.75rem}
.chart-card{background:var(--card-bg);border:1px solid var(--border);border-radius:4px;padding:.75rem}
.chart-card .chart-title{color:var(--text-muted);font-size:.65rem;text-transform:uppercase;letter-spacing:.08em;margin-bottom:.4rem}
#heatmap-wrap{margin-bottom:.75rem;overflow-x:auto}
#heatmap{display:block}
.hm-cell{cursor:default}
#heatmap-legend{display:flex;gap:.75rem;align-items:center;font-size:.7rem;color:var(--text-muted);margin-top:.3rem;flex-wrap:wrap}
details#model-breakdown{margin-bottom:.75rem}
details#model-breakdown summary{color:var(--text-muted);font-size:.7rem;text-transform:uppercase;letter-spacing:.08em;cursor:pointer;user-select:none;padding:.2rem 0}
details#model-breakdown summary:hover{color:var(--text-primary)}
#breakdown-table{margin-top:.5rem;width:100%;border-collapse:collapse;font-size:.75rem}
#breakdown-table th{background:var(--th-bg);color:var(--text-muted);text-align:right;padding:.3rem .5rem;border-bottom:1px solid var(--border)}
#breakdown-table th:first-child{text-align:left}
#breakdown-table td{padding:.25rem .5rem;border-bottom:1px solid var(--border);text-align:right}
#breakdown-table td:first-child{text-align:left}
.tab-bar{display:flex;gap:0;margin-bottom:.75rem;border-bottom:1px solid var(--border)}
.tab-btn{background:none;border:none;color:var(--text-muted);padding:.4rem .8rem;cursor:pointer;font-family:inherit;font-size:.8rem;border-bottom:2px solid transparent;transition:color .15s,border-color .15s}
.tab-btn:hover{color:var(--text-primary)}
.tab-btn.active{color:var(--accent);border-bottom-color:var(--accent)}
.tab-content{display:none}.tab-content.active{display:block}
.filters{display:flex;gap:.5rem;align-items:center;margin-bottom:.75rem;flex-wrap:wrap}
.filters input[type=text]{background:var(--input-bg);border:1px solid var(--input-border);color:var(--text-primary);padding:.3rem .5rem;border-radius:3px;font-family:inherit;font-size:.85rem}
#f-search{width:240px}
#f-model{width:140px}
#f-provider{width:130px}
.filters label{color:var(--text-muted);font-size:.85rem;cursor:pointer;display:flex;align-items:center;gap:.25rem}
.export-wrap{position:relative;margin-left:auto}
.export-btn{background:var(--input-bg);border:1px solid var(--input-border);color:var(--accent);padding:.3rem .6rem;border-radius:3px;font-family:inherit;font-size:.82rem;cursor:pointer}
.export-btn:hover{border-color:var(--accent)}
.export-menu{display:none;position:absolute;right:0;top:110%;background:var(--bg-secondary);border:1px solid var(--input-border);border-radius:3px;z-index:200;min-width:130px;box-shadow:0 4px 12px rgba(0,0,0,.5)}
.export-menu a{display:block;padding:.35rem .7rem;color:var(--text-primary);font-size:.8rem;cursor:pointer;text-decoration:none;font-family:inherit}
.export-menu a:hover{background:var(--hover-bg);color:var(--accent)}
.export-wrap:hover .export-menu{display:block}
table.call-table{width:100%;border-collapse:collapse;font-size:.78rem}
table.call-table th{background:var(--th-bg);color:var(--text-muted);text-align:right;padding:.35rem .5rem;border-bottom:1px solid var(--border);position:sticky;top:0;white-space:nowrap;cursor:pointer;user-select:none}
table.call-table th:first-child,table.call-table th:nth-child(3),table.call-table th:nth-child(4){text-align:left}
table.call-table th:hover{color:var(--text-primary)}
table.call-table td{padding:.3rem .5rem;border-bottom:1px solid var(--border);text-align:right;white-space:nowrap}
table.call-table td:first-child,table.call-table td:nth-child(3),table.call-table td:nth-child(4){text-align:left}
table.call-table tr:hover td{background:var(--hover-bg)}
table.call-table tr.row-err td{color:var(--error)}
table.call-table tr.row-err:hover td{background:var(--hover-bg)}
table.call-table tr.sse-new{border-left:3px solid var(--accent);transition:border-left-color 2s ease}
table.call-table tr.sse-fade{border-left-color:transparent}
.ok{color:var(--success)}.err{color:var(--error)}.dim{color:var(--text-muted)}
#status{color:var(--text-muted);font-size:.7rem;margin-top:.5rem}
#load-more-wrap{text-align:center;margin-top:.5rem}
#load-more-btn{background:var(--input-bg);border:1px solid var(--input-border);color:var(--accent);padding:.3rem 1rem;border-radius:3px;font-family:inherit;font-size:.8rem;cursor:pointer}
#load-more-btn:hover{border-color:var(--accent)}
#detail-panel{display:none;position:fixed;top:0;right:0;width:420px;height:100vh;background:var(--bg-secondary);border-left:1px solid var(--border);overflow-y:auto;padding:1rem;z-index:100;font-size:.78rem}
.detail-header{display:flex;justify-content:space-between;align-items:center;margin-bottom:.75rem}
.detail-title{color:var(--accent);font-weight:bold}
.detail-close{background:none;border:none;color:var(--text-muted);cursor:pointer;font-size:1rem;line-height:1}
.section-hdr{color:var(--accent);font-size:.65rem;text-transform:uppercase;letter-spacing:.08em;margin:.75rem 0 .3rem;border-bottom:1px solid var(--border);padding-bottom:.2rem}
.section-hdr:first-child{margin-top:0}
.detail-link{color:var(--accent);font-size:.75rem;cursor:pointer;text-decoration:none;display:inline-block;margin-top:.3rem}
.detail-link:hover{text-decoration:underline}
.tag-badge{display:inline-block;background:var(--accent);color:var(--bg-primary);font-size:.65rem;padding:.1rem .35rem;border-radius:3px;margin:.1rem .15rem .1rem 0;font-weight:bold}
.collapsible-body{cursor:pointer;color:var(--accent);font-size:.72rem;padding:.3rem .4rem;background:var(--bg-primary);border-radius:3px;margin-top:.2rem;border:1px solid var(--border)}
.collapsible-body:hover{border-color:var(--accent)}
.collapsible-pre{display:none;background:var(--bg-primary);padding:.5rem;border-radius:3px;font-size:.72rem;overflow-x:auto;white-space:pre-wrap;margin-top:.2rem}
.agents-table,.workflow-timeline{width:100%;border-collapse:collapse;font-size:.78rem}
.agents-table th{background:var(--th-bg);color:var(--text-muted);padding:.35rem .5rem;border-bottom:1px solid var(--border);text-align:right}
.agents-table th:first-child{text-align:left}
.agents-table td{padding:.3rem .5rem;border-bottom:1px solid var(--border);text-align:right}
.agents-table td:first-child{text-align:left;color:var(--accent);cursor:pointer}
.agents-table td:first-child:hover{text-decoration:underline}
.agents-table tr:hover td{background:var(--hover-bg)}
.wf-list{list-style:none;padding:0}
.wf-item{padding:.3rem .5rem;cursor:pointer;font-size:.78rem;border-bottom:1px solid var(--border)}
.wf-item:hover{background:var(--hover-bg);color:var(--accent)}
.wf-bar-wrap{position:relative;margin:.3rem 0;height:22px}
.wf-bar{position:absolute;height:20px;border-radius:3px;top:1px;font-size:.65rem;line-height:20px;padding:0 4px;overflow:hidden;white-space:nowrap;color:#fff;min-width:2px}
.wf-bar-label{font-size:.65rem;color:var(--text-muted);margin-bottom:.15rem}
@media(max-width:600px){
  .cards-grid{grid-template-columns:1fr}
  #charts-row{grid-template-columns:1fr}
  table.call-table{display:block;overflow-x:auto}
  .header{flex-direction:column;align-items:flex-start}
  #f-search{width:100%}
  #detail-panel{width:100%;left:0}
}
</style>
</head>
<body>
<div class="header">
  <h1>[ OpenTrace ]</h1>
  <div class="nav">
    <a href="/playground" class="nav-link">Playground &rarr;</a>
    <button id="dark-toggle" onclick="toggleDark()">&#9728;</button>
  </div>
</div>

<!-- Stat cards row 1 -->
<div class="cards-grid">
  <div class="card"><div class="card-label">Total calls</div><div class="card-value" id="s-calls">-</div><div class="card-spark"><canvas id="spark-calls"></canvas></div></div>
  <div class="card"><div class="card-label">Total cost</div><div class="card-value" id="s-cost">-</div><div class="card-spark"><canvas id="spark-cost"></canvas></div></div>
  <div class="card"><div class="card-label">Avg latency</div><div class="card-value" id="s-lat">-</div><div class="card-spark"><canvas id="spark-lat"></canvas></div></div>
</div>
<!-- Stat cards row 2 -->
<div class="cards-grid" style="margin-bottom:.75rem">
  <div class="card"><div class="card-label">Errors</div><div class="card-value" id="s-errors">-</div></div>
  <div class="card"><div class="card-label">p99 latency</div><div class="card-value" id="s-p99">-</div></div>
  <div class="card"><div class="card-label">Calls (1h)</div><div class="card-value" id="s-hour">-</div></div>
</div>

<!-- Donut charts + latency histogram -->
<div id="charts-row">
  <div class="chart-card"><div class="chart-title">Cost by Provider</div><canvas id="chart-provider"></canvas></div>
  <div class="chart-card"><div class="chart-title">Cost by Model</div><canvas id="chart-model"></canvas></div>
  <div class="chart-card"><div class="chart-title">Latency Distribution</div><canvas id="chart-latency"></canvas></div>
</div>

<!-- Heatmap -->
<div id="heatmap-wrap">
  <div class="section-label">DAILY COST &mdash; 90 days</div>
  <svg id="heatmap" height="88"></svg>
  <div id="heatmap-legend"></div>
</div>

<!-- Model breakdown collapsible (default open) -->
<details id="model-breakdown" open>
  <summary>by model &#9658;</summary>
  <table id="breakdown-table">
    <thead><tr>
      <th>model</th><th>calls</th><th>cost</th><th>avg ms</th><th>errors</th>
    </tr></thead>
    <tbody id="breakdown-tbody"></tbody>
  </table>
</details>

<!-- Tab bar for Calls / Agents / Workflows -->
<div class="tab-bar">
  <button class="tab-btn active" onclick="switchTab('calls')">Calls</button>
  <button class="tab-btn" onclick="switchTab('agents')">Agents</button>
  <button class="tab-btn" onclick="switchTab('workflows')">Workflows</button>
</div>

<!-- Tab: Calls -->
<div id="tab-calls" class="tab-content active">
<!-- Filters -->
<div class="filters">
  <input type="text" id="f-search" placeholder="Search prompts, responses, models..." oninput="debouncedSearch()">
  <input type="text" id="f-model" placeholder="Filter model..." oninput="saveFilters();refresh()">
  <input type="text" id="f-provider" placeholder="Filter provider..." oninput="saveFilters();refresh()">
  <input type="text" id="f-agent" placeholder="Filter agent..." oninput="saveFilters();refresh()">
  <label><input type="checkbox" id="f-errors" onchange="refresh()"> Errors only</label>
  <div class="export-wrap">
    <button class="export-btn">Export &#9662;</button>
    <div class="export-menu">
      <a onclick="downloadExport('jsonl')">JSONL</a>
      <a onclick="downloadExport('csv')">CSV</a>
      <a onclick="downloadExport('langfuse')">Langfuse</a>
      <a onclick="downloadExport('langsmith')">LangSmith</a>
      <a onclick="downloadExport('weave')">W&amp;B Weave</a>
    </div>
  </div>
</div>

<!-- Call log table -->
<table class="call-table">
  <thead><tr id="thead-row">
    <th data-col="id">id</th><th data-col="timestamp">timestamp</th><th data-col="model">model</th><th data-col="provider">provider</th><th data-col="status_code">status</th>
    <th data-col="latency_ms">latency</th><th data-col="ttft_ms">ttft</th><th data-col="input_tokens">in</th><th data-col="output_tokens">out</th><th data-col="cost_usd">cost</th>
  </tr></thead>
  <tbody id="tbody"></tbody>
</table>
<div id="load-more-wrap"><button id="load-more-btn" onclick="loadMore()" style="display:none">Load more</button></div>
</div>

<!-- Tab: Agents -->
<div id="tab-agents" class="tab-content">
  <table class="agents-table">
    <thead><tr><th>agent</th><th>calls</th><th>cost</th><th>avg ms</th><th>errors</th></tr></thead>
    <tbody id="agents-tbody"></tbody>
  </table>
</div>

<!-- Tab: Workflows -->
<div id="tab-workflows" class="tab-content">
  <div id="wf-list-wrap"></div>
  <div id="wf-detail-wrap"></div>
</div>

<div id="status">Loading...</div>

<!-- Detail panel -->
<div id="detail-panel">
  <div class="detail-header">
    <span class="detail-title">Call Detail</span>
    <button class="detail-close" onclick="closeDetail()">&#10005;</button>
  </div>
  <div id="detail-content"></div>
</div>

<script>
/* ---- Utility ---- */
function fmtMs(v){return v==null?'-':v+'ms'}
function fmtCost(c){return c==null?'-':'$'+Number(c).toFixed(4)}
function clearEl(el){while(el.firstChild)el.removeChild(el.firstChild);}
function makeCell(text,cls){var td=document.createElement('td');td.textContent=text;if(cls)td.className=cls;return td;}
function makeTr(label,val){
  var tr=document.createElement('tr');
  var td1=document.createElement('td');td1.style.color='var(--text-muted)';td1.style.padding='.2rem .4rem';td1.style.whiteSpace='nowrap';td1.style.verticalAlign='top';td1.textContent=label;
  var td2=document.createElement('td');td2.style.color='var(--text-primary)';td2.style.padding='.2rem .4rem';td2.style.wordBreak='break-all';td2.textContent=String(val);
  tr.appendChild(td1);tr.appendChild(td2);return tr;
}

/* ---- Relative timestamps ---- */
function relTime(ts){
  try{
    var diff=Date.now()-new Date(ts).getTime();
    if(diff<0)diff=0;
    var s=Math.floor(diff/1000);
    if(s<60)return s+'s ago';
    var m=Math.floor(s/60);if(m<60)return m+'m ago';
    var h=Math.floor(m/60);if(h<24)return h+'h ago';
    var d=Math.floor(h/24);return d+'d ago';
  }catch(e){return (ts||'').slice(0,19);}
}

/* ---- JSON syntax highlighting (safe: all values are entity-escaped) ---- */
function highlightJSON(obj,indent){
  if(indent===undefined)indent=0;
  var pad='  '.repeat(indent);
  if(obj===null)return '<span style="color:#888">null</span>';
  if(typeof obj==='boolean')return '<span style="color:#ce93d8">'+obj+'</span>';
  if(typeof obj==='number')return '<span style="color:#ffd54f">'+obj+'</span>';
  if(typeof obj==='string'){
    var escaped=obj.replace(/&/g,'&amp;').replace(/</g,'&lt;').replace(/>/g,'&gt;').replace(/"/g,'&quot;');
    if(escaped.length>200)escaped=escaped.slice(0,200)+'...';
    return '<span style="color:#81c784">"'+escaped+'"</span>';
  }
  if(Array.isArray(obj)){
    if(obj.length===0)return '[]';
    var items=obj.map(function(v){return pad+'  '+highlightJSON(v,indent+1);});
    return '[\n'+items.join(',\n')+'\n'+pad+']';
  }
  if(typeof obj==='object'){
    var keys=Object.keys(obj);
    if(keys.length===0)return '{}';
    var entries=keys.map(function(k){
      var kEsc=k.replace(/&/g,'&amp;').replace(/</g,'&lt;').replace(/>/g,'&gt;').replace(/"/g,'&quot;');
      return pad+'  <span style="color:#4fc3f7">"'+kEsc+'"</span>: '+highlightJSON(obj[k],indent+1);
    });
    return '{\n'+entries.join(',\n')+'\n'+pad+'}';
  }
  return String(obj);
}

/* ---- sticky filters ---- */
(function(){
  var m=localStorage.getItem('ot-filter-model');
  var p=localStorage.getItem('ot-filter-provider');
  var a=localStorage.getItem('ot-filter-agent');
  if(m)document.getElementById('f-model').value=m;
  if(p)document.getElementById('f-provider').value=p;
  if(a)document.getElementById('f-agent').value=a;
})();
function saveFilters(){
  localStorage.setItem('ot-filter-model',document.getElementById('f-model').value);
  localStorage.setItem('ot-filter-provider',document.getElementById('f-provider').value);
  localStorage.setItem('ot-filter-agent',document.getElementById('f-agent').value);
}

/* ---- Global state ---- */
var showAgentCol=false;
var allCalls=[];
var sortCol=null,sortDir='desc';
var currentCallsPage=100;

/* ---- Tab switching ---- */
function switchTab(name){
  document.querySelectorAll('.tab-btn').forEach(function(b){b.classList.remove('active');});
  document.querySelectorAll('.tab-content').forEach(function(c){c.classList.remove('active');});
  document.querySelector('.tab-btn[onclick*="'+name+'"]').classList.add('active');
  document.getElementById('tab-'+name).classList.add('active');
  if(name==='agents')loadAgents();
  if(name==='workflows')loadWorkflows();
}

/* ---- Theme toggle ---- */
function toggleDark(){
  var html=document.documentElement;
  var isLight=html.getAttribute('data-theme')==='light';
  html.setAttribute('data-theme',isLight?'dark':'light');
  localStorage.setItem('ot-theme',isLight?'dark':'light');
  document.getElementById('dark-toggle').textContent=isLight?'\u2600':'\u263e';
  updateChartColors();
}
(function(){
  if(localStorage.getItem('ot-theme')==='light'){
    document.documentElement.setAttribute('data-theme','light');
    document.getElementById('dark-toggle').textContent='\u263e';
  }
})();

/* ---- Detail panel ---- */
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
    clearEl(content);

    function addSectionHdr(title){
      var hdr=document.createElement('div');
      hdr.className='section-hdr';
      hdr.textContent='[ '+title+' ]';
      content.appendChild(hdr);
    }
    function addTable(rows){
      var tbl=document.createElement('table');
      tbl.style.width='100%';tbl.style.borderCollapse='collapse';
      rows.forEach(function(row){
        if(row[1]==null||row[1]===undefined||String(row[1])==='null')return;
        tbl.appendChild(makeTr(row[0],row[1]));
      });
      content.appendChild(tbl);
    }

    addSectionHdr('metadata');
    addTable([
      ['id',r.id],
      ['timestamp',r.timestamp],
      ['provider',r.provider],
      ['model',r.model],
      ['endpoint',r.endpoint],
      ['status',r.status_code],
      ['provider_request_id',r.provider_request_id||null],
      ['agent',r.agent_name||null],
      ['workflow',r.workflow_id||null],
      ['span',r.span_name||null],
    ]);

    /* Tags as badges */
    if(r.tags){
      var tagsDiv=document.createElement('div');
      tagsDiv.style.marginTop='.3rem';
      r.tags.split(',').forEach(function(t){
        t=t.trim();if(!t)return;
        var badge=document.createElement('span');
        badge.className='tag-badge';
        badge.textContent=t;
        tagsDiv.appendChild(badge);
      });
      content.appendChild(tagsDiv);
    }

    /* Detail panel links */
    if(r.workflow_id){
      var wfLink=document.createElement('a');
      wfLink.className='detail-link';
      wfLink.textContent='View workflow \u2192';
      wfLink.onclick=function(){closeDetail();switchTab('workflows');loadWorkflowDetail(r.workflow_id);};
      content.appendChild(wfLink);
      content.appendChild(document.createElement('br'));
    }
    if(r.agent_name){
      var agLink=document.createElement('a');
      agLink.className='detail-link';
      agLink.textContent='Filter by agent \u2192';
      agLink.onclick=function(){closeDetail();document.getElementById('f-agent').value=r.agent_name;saveFilters();switchTab('calls');refresh();};
      content.appendChild(agLink);
    }

    addSectionHdr('timing');
    addTable([
      ['latency_ms',fmtMs(r.latency_ms)],
      ['ttft_ms',fmtMs(r.ttft_ms)],
    ]);

    addSectionHdr('cost');
    addTable([
      ['input_tokens',r.input_tokens!=null?String(r.input_tokens):null],
      ['output_tokens',r.output_tokens!=null?String(r.output_tokens):null],
      ['cost_usd',fmtCost(r.cost_usd)],
    ]);

    /* Collapsible request body with syntax highlighting */
    if(r.request_body){
      addSectionHdr('request');
      var reqSize=r.request_body.length;
      var reqToggle=document.createElement('div');
      reqToggle.className='collapsible-body';
      reqToggle.textContent='['+reqSize+' bytes] Click to expand';
      var reqPre=document.createElement('pre');
      reqPre.className='collapsible-pre';
      try{var _rp=JSON.parse(r.request_body);reqPre.innerHTML=highlightJSON(_rp,0);}catch(e){reqPre.textContent=r.request_body;}
      reqToggle.onclick=function(){
        var vis=reqPre.style.display==='block';
        reqPre.style.display=vis?'none':'block';
        reqToggle.textContent=vis?'['+reqSize+' bytes] Click to expand':'['+reqSize+' bytes] Click to collapse';
      };
      content.appendChild(reqToggle);
      content.appendChild(reqPre);
    }
    /* Collapsible response body with syntax highlighting */
    if(r.response_body){
      addSectionHdr('response');
      var resSize=r.response_body.length;
      var resToggle=document.createElement('div');
      resToggle.className='collapsible-body';
      resToggle.textContent='['+resSize+' bytes] Click to expand';
      var resPre=document.createElement('pre');
      resPre.className='collapsible-pre';
      try{var _rs=JSON.parse(r.response_body);resPre.innerHTML=highlightJSON(_rs,0);}catch(e){resPre.textContent=r.response_body;}
      resToggle.onclick=function(){
        var vis=resPre.style.display==='block';
        resPre.style.display=vis?'none':'block';
        resToggle.textContent=vis?'['+resSize+' bytes] Click to expand':'['+resSize+' bytes] Click to collapse';
      };
      content.appendChild(resToggle);
      content.appendChild(resPre);
    }
    if(r.error){
      addSectionHdr('error');
      var errDiv=document.createElement('div');
      errDiv.style.color='var(--error)';errDiv.style.padding='.3rem .4rem';errDiv.style.fontSize='.78rem';
      errDiv.textContent=r.error;
      content.appendChild(errDiv);
    }
  }catch(e){content.textContent='Error: '+e.message;}
}

/* ---- Table sorting ---- */
function sortCalls(col){
  if(sortCol===col){sortDir=sortDir==='asc'?'desc':'asc';}
  else{sortCol=col;sortDir='desc';}
  document.querySelectorAll('#thead-row th').forEach(function(th){
    var c=th.getAttribute('data-col');
    var base=th.textContent.replace(/[\u25b2\u25bc\s]+$/,'');
    if(c===sortCol)th.textContent=base+' '+(sortDir==='asc'?'\u25b2':'\u25bc');
    else th.textContent=base;
  });
  allCalls.sort(function(a,b){
    var va=a[col],vb=b[col];
    if(va==null)va='';if(vb==null)vb='';
    if(typeof va==='number'&&typeof vb==='number')return sortDir==='asc'?va-vb:vb-va;
    return sortDir==='asc'?String(va).localeCompare(String(vb)):String(vb).localeCompare(String(va));
  });
  renderRows(allCalls);
}
(function(){
  document.querySelectorAll('#thead-row th').forEach(function(th){
    var col=th.getAttribute('data-col');
    if(col)th.addEventListener('click',function(){sortCalls(col);});
  });
})();

/* ---- Build a single table row ---- */
function makeRow(r,isSSE){
  var ok=r.status_code>=200&&r.status_code<400&&!r.error;
  var tr=document.createElement('tr');
  tr.style.cursor='pointer';
  if(!ok)tr.className='row-err';
  if(isSSE){tr.classList.add('sse-new');setTimeout(function(){tr.classList.add('sse-fade');},50);}
  (function(rid){
    tr.addEventListener('click',function(){showDetail(rid);});
  })(r.id);
  var idCell=makeCell((r.id||'').slice(0,8),'dim');
  idCell.title='Click to copy full ID';
  idCell.style.cursor='copy';
  (function(rid){
    idCell.addEventListener('click',function(e){
      e.stopPropagation();
      navigator.clipboard&&navigator.clipboard.writeText(rid).then(function(){
        var orig=idCell.textContent;
        idCell.textContent='copied!';
        setTimeout(function(){idCell.textContent=orig;},800);
      });
    });
  })(r.id);
  tr.appendChild(idCell);
  var tsCell=makeCell(relTime(r.timestamp),'dim');
  tsCell.title=r.timestamp||'';
  tsCell.setAttribute('data-ts',r.timestamp||'');
  tr.appendChild(tsCell);
  tr.appendChild(makeCell(r.model||'-',null));
  tr.appendChild(makeCell(r.provider||'-','dim'));
  tr.appendChild(makeCell(String(r.status_code),ok?'ok':'err'));
  tr.appendChild(makeCell(fmtMs(r.latency_ms),null));
  tr.appendChild(makeCell(fmtMs(r.ttft_ms),'dim'));
  tr.appendChild(makeCell(r.input_tokens!=null?String(r.input_tokens):'-','dim'));
  tr.appendChild(makeCell(r.output_tokens!=null?String(r.output_tokens):'-','dim'));
  tr.appendChild(makeCell(fmtCost(r.cost_usd),null));
  if(showAgentCol)tr.appendChild(makeCell(r.agent_name||'-','dim'));
  return tr;
}

/* ---- Render rows ---- */
function renderRows(calls){
  showAgentCol=calls.some(function(c){return c.agent_name;});
  var theadRow=document.getElementById('thead-row');
  var existingAgentTh=document.getElementById('th-agent');
  if(showAgentCol&&!existingAgentTh){
    var th=document.createElement('th');th.id='th-agent';th.textContent='agent';th.setAttribute('data-col','agent_name');
    th.addEventListener('click',function(){sortCalls('agent_name');});
    theadRow.appendChild(th);
  } else if(!showAgentCol&&existingAgentTh){existingAgentTh.remove();}
  var tbody=document.getElementById('tbody');
  clearEl(tbody);
  for(var i=0;i<calls.length;i++){tbody.appendChild(makeRow(calls[i],false));}
  document.getElementById('load-more-btn').style.display=calls.length>=currentCallsPage?'inline-block':'none';
}
function prependRow(r){
  var tbody=document.getElementById('tbody');
  tbody.insertBefore(makeRow(r,true),tbody.firstChild);
  allCalls.unshift(r);
  while(tbody.rows.length>200)tbody.deleteRow(tbody.rows.length-1);
}

/* ---- Refresh relative timestamps every 30s ---- */
setInterval(function(){
  document.querySelectorAll('#tbody td[data-ts]').forEach(function(td){
    td.textContent=relTime(td.getAttribute('data-ts'));
  });
},30000);

/* ---- Search (debounced 300ms) ---- */
var searchTimer=null;
function debouncedSearch(){clearTimeout(searchTimer);searchTimer=setTimeout(runSearch,300);}
async function runSearch(){
  var q=document.getElementById('f-search').value.trim();
  if(!q){refresh();return;}
  try{
    var res=await fetch('/api/search?q='+encodeURIComponent(q)+'&limit=100');
    var data=await res.json();
    if(!res.ok){document.getElementById('status').textContent='Search error: '+(data||'');return;}
    allCalls=(data.results||[]).map(function(r){return r.record;});
    renderRows(allCalls);
    document.getElementById('status').textContent='Search: '+data.count+' result(s) for "'+q+'"';
  }catch(e){document.getElementById('status').textContent='Search error: '+e.message;}
}

/* ---- Load more (pagination) ---- */
async function loadMore(){
  currentCallsPage+=100;
  var model=document.getElementById('f-model').value;
  var provider=document.getElementById('f-provider').value;
  var agent=document.getElementById('f-agent').value;
  var errors=document.getElementById('f-errors').checked;
  var url='/api/calls?limit='+currentCallsPage;
  if(model)url+='&model='+encodeURIComponent(model);
  if(provider)url+='&provider='+encodeURIComponent(provider);
  if(agent)url+='&agent='+encodeURIComponent(agent);
  if(errors)url+='&errors=true';
  try{
    var res=await fetch(url);
    allCalls=await res.json();
    renderRows(allCalls);
    document.getElementById('status').textContent='Loaded '+allCalls.length+' calls';
  }catch(e){}
}

/* ---- Chart.js instances ---- */
var chartProvider=null,chartModel=null,chartLatency=null;
var sparkCallsChart=null,sparkCostChart=null,sparkLatChart=null;

function getChartTextColor(){
  return getComputedStyle(document.documentElement).getPropertyValue('--text-muted').trim()||'#555';
}

function makeSparkline(canvasId,data,color){
  var ctx=document.getElementById(canvasId);
  if(!ctx)return null;
  return new Chart(ctx,{
    type:'line',
    data:{labels:data.map(function(_,i){return i;}),datasets:[{data:data,borderColor:color,borderWidth:1.5,fill:false,pointRadius:0,tension:0.3}]},
    options:{responsive:true,maintainAspectRatio:false,plugins:{legend:{display:false},tooltip:{enabled:false}},scales:{x:{display:false},y:{display:false}},animation:false}
  });
}

function makeDoughnut(canvasId,labels,values,colors){
  var ctx=document.getElementById(canvasId);
  if(!ctx)return null;
  return new Chart(ctx,{
    type:'doughnut',
    data:{labels:labels,datasets:[{data:values,backgroundColor:colors,borderWidth:0}]},
    options:{responsive:true,maintainAspectRatio:true,plugins:{legend:{position:'bottom',labels:{color:getChartTextColor(),font:{size:10,family:'inherit'},boxWidth:10,padding:6}},tooltip:{callbacks:{label:function(c){return c.label+': $'+Number(c.raw).toFixed(4);}}}},animation:false}
  });
}

function makeLatencyHistogram(canvasId,buckets){
  var ctx=document.getElementById(canvasId);
  if(!ctx)return null;
  var labels=['<100ms','100-250','250-500','500ms-1s','1-2s','2-5s','5s+'];
  var colors=['#66bb6a','#81c784','#ffd54f','#ffb74d','#ff8a65','#ef5350','#c62828'];
  return new Chart(ctx,{
    type:'bar',
    data:{labels:labels,datasets:[{data:buckets,backgroundColor:colors,borderWidth:0}]},
    options:{indexAxis:'y',responsive:true,maintainAspectRatio:true,plugins:{legend:{display:false},tooltip:{callbacks:{label:function(c){return c.raw+' calls';}}}},scales:{x:{display:false},y:{ticks:{color:getChartTextColor(),font:{size:10,family:'inherit'}},grid:{display:false}}},animation:false}
  });
}

function updateChartColors(){
  var clr=getChartTextColor();
  [chartProvider,chartModel].forEach(function(ch){
    if(!ch)return;
    ch.options.plugins.legend.labels.color=clr;
    ch.update();
  });
  if(chartLatency){
    chartLatency.options.scales.y.ticks.color=clr;
    chartLatency.update();
  }
}

var chartPalette=['#4fc3f7','#81c784','#ffd54f','#ce93d8','#ff8a65','#4db6ac','#a1887f','#90a4ae','#f48fb1','#aed581'];

/* ---- Sparklines from heatmap data ---- */
async function loadSparklines(){
  try{
    var res=await fetch('/api/heatmap?days=7');
    if(!res.ok)return;
    var data=await res.json();
    var days=data.days||[];
    var costArr=[],callsArr=[],latArr=[];
    var map={};days.forEach(function(d){map[d.date]=d;});
    var now=new Date();
    for(var i=6;i>=0;i--){
      var d=new Date(now);d.setDate(now.getDate()-i);
      var key=d.toISOString().slice(0,10);
      var s=map[key];
      costArr.push(s?s.cost_usd:0);
      callsArr.push(s?s.calls:0);
      latArr.push(0);
    }
    if(sparkCallsChart)sparkCallsChart.destroy();
    if(sparkCostChart)sparkCostChart.destroy();
    if(sparkLatChart)sparkLatChart.destroy();
    sparkCallsChart=makeSparkline('spark-calls',callsArr,'#4fc3f7');
    sparkCostChart=makeSparkline('spark-cost',costArr,'#81c784');
    sparkLatChart=makeSparkline('spark-lat',latArr,'#ffd54f');
  }catch(e){}
}

/* ---- Render charts from stats data ---- */
function renderCharts(statsData,calls){
  var providers=statsData.providers||[];
  var pLabels=providers.map(function(p){return p.provider;});
  var pValues=providers.map(function(p){return p.total_cost_usd;});
  if(chartProvider)chartProvider.destroy();
  chartProvider=makeDoughnut('chart-provider',pLabels,pValues,chartPalette.slice(0,pLabels.length));

  var models=statsData.models||[];
  var mLabels=models.map(function(m){return m.model;}).slice(0,10);
  var mValues=models.map(function(m){return m.total_cost_usd;}).slice(0,10);
  if(chartModel)chartModel.destroy();
  chartModel=makeDoughnut('chart-model',mLabels,mValues,chartPalette.slice(0,mLabels.length));

  var buckets=[0,0,0,0,0,0,0];
  (calls||[]).forEach(function(c){
    var ms=c.latency_ms||0;
    if(ms<100)buckets[0]++;
    else if(ms<250)buckets[1]++;
    else if(ms<500)buckets[2]++;
    else if(ms<1000)buckets[3]++;
    else if(ms<2000)buckets[4]++;
    else if(ms<5000)buckets[5]++;
    else buckets[6]++;
  });
  if(chartLatency)chartLatency.destroy();
  chartLatency=makeLatencyHistogram('chart-latency',buckets);
}

/* ---- Main refresh ---- */
async function refresh(){
  if(document.getElementById('f-search').value.trim()){return;}
  var model=document.getElementById('f-model').value;
  var provider=document.getElementById('f-provider').value;
  var agent=document.getElementById('f-agent').value;
  var errors=document.getElementById('f-errors').checked;
  var url='/api/calls?limit='+currentCallsPage;
  if(model)url+='&model='+encodeURIComponent(model);
  if(provider)url+='&provider='+encodeURIComponent(provider);
  if(agent)url+='&agent='+encodeURIComponent(agent);
  if(errors)url+='&errors=true';
  try{
    var [r1,r2]=await Promise.all([fetch(url),fetch('/api/stats')]);
    var calls=await r1.json();
    var data=await r2.json();
    var s=data.stats;
    document.getElementById('s-calls').textContent=String(s.total_calls);
    document.getElementById('s-cost').textContent='$'+Number(s.total_cost_usd).toFixed(4);
    document.getElementById('s-lat').textContent=Math.round(s.avg_latency_ms)+'ms';
    document.getElementById('s-errors').textContent=String(s.error_count);
    document.getElementById('s-p99').textContent=Math.round(data.p99_latency_ms||0)+'ms';
    document.getElementById('s-hour').textContent=String(s.calls_last_hour);
    allCalls=calls;
    renderRows(calls);
    renderCharts(data,calls);
    var btbody=document.getElementById('breakdown-tbody');
    clearEl(btbody);
    (data.models||[]).forEach(function(m){
      var tr=document.createElement('tr');
      [m.model,m.calls,'$'+Number(m.total_cost_usd).toFixed(4),Math.round(m.avg_latency_ms)+'ms',m.error_count].forEach(function(v,i){
        var td=document.createElement('td');
        td.textContent=String(v);
        td.style.padding='.25rem .5rem';
        td.style.borderBottom='1px solid var(--border)';
        if(i>0)td.style.textAlign='right';
        tr.appendChild(td);
      });
      btbody.appendChild(tr);
    });
    document.getElementById('status').textContent='Updated '+new Date().toLocaleTimeString()+' \u2014 '+calls.length+' calls';
  }catch(e){document.getElementById('status').textContent='Error: '+e.message;}
}

/* ---- Heatmap ---- */
async function loadHeatmap(){
  try{
    var res=await fetch('/api/heatmap?days=90');
    if(!res.ok)return;
    renderHeatmap((await res.json()).days);
    renderHeatmapLegend();
  }catch(e){}
}
function renderHeatmap(days){
  var svg=document.getElementById('heatmap');
  clearEl(svg);
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
function renderHeatmapLegend(){
  var wrap=document.getElementById('heatmap-legend');
  clearEl(wrap);
  var items=[
    {label:'empty',fill:'#1e1e1e'},
    {label:'low',fill:'hsl(200,60%,22%)'},
    {label:'mid',fill:'hsl(200,60%,38%)'},
    {label:'high',fill:'hsl(200,60%,55%)'},
  ];
  items.forEach(function(item){
    var span=document.createElement('span');
    span.style.display='flex';span.style.alignItems='center';span.style.gap='.25rem';
    var sq=document.createElement('span');
    sq.style.display='inline-block';sq.style.width='10px';sq.style.height='10px';
    sq.style.background=item.fill;sq.style.borderRadius='2px';sq.style.border='1px solid #333';
    var lbl=document.createElement('span');lbl.textContent=item.label;
    span.appendChild(sq);span.appendChild(lbl);wrap.appendChild(span);
  });
}

/* ---- Agents tab ---- */
async function loadAgents(){
  try{
    var res=await fetch('/api/agents');
    var data=await res.json();
    var tbody=document.getElementById('agents-tbody');
    clearEl(tbody);
    (data.agents||[]).forEach(function(a){
      var tr=document.createElement('tr');
      var tdName=document.createElement('td');
      tdName.textContent=a.agent_name||'-';
      tdName.onclick=function(){
        document.getElementById('f-agent').value=a.agent_name||'';
        saveFilters();switchTab('calls');refresh();
      };
      tr.appendChild(tdName);
      [a.calls,'$'+Number(a.total_cost_usd||0).toFixed(4),Math.round(a.avg_latency_ms||0)+'ms',a.errors||0].forEach(function(v){
        var td=document.createElement('td');td.textContent=String(v);tr.appendChild(td);
      });
      tbody.appendChild(tr);
    });
  }catch(e){}
}

/* ---- Workflows tab ---- */
async function loadWorkflows(){
  try{
    var res=await fetch('/api/calls?limit=1000');
    var calls=await res.json();
    var wfMap={};
    calls.forEach(function(c){
      if(!c.workflow_id)return;
      if(!wfMap[c.workflow_id])wfMap[c.workflow_id]={id:c.workflow_id,count:0,cost:0};
      wfMap[c.workflow_id].count++;
      wfMap[c.workflow_id].cost+=(c.cost_usd||0);
    });
    var wfList=Object.values(wfMap);
    var wrap=document.getElementById('wf-list-wrap');
    clearEl(wrap);
    document.getElementById('wf-detail-wrap').textContent='';
    if(wfList.length===0){wrap.textContent='No workflows found.';return;}
    var ul=document.createElement('ul');ul.className='wf-list';
    wfList.forEach(function(wf){
      var li=document.createElement('li');li.className='wf-item';
      li.textContent=wf.id+' ('+wf.count+' calls, $'+wf.cost.toFixed(4)+')';
      li.onclick=function(){loadWorkflowDetail(wf.id);};
      ul.appendChild(li);
    });
    wrap.appendChild(ul);
  }catch(e){}
}

async function loadWorkflowDetail(wfId){
  try{
    var res=await fetch('/api/workflow/'+encodeURIComponent(wfId));
    if(!res.ok)return;
    var data=await res.json();
    var wrap=document.getElementById('wf-detail-wrap');
    clearEl(wrap);
    var hdr=document.createElement('div');
    hdr.style.marginBottom='.5rem';hdr.style.fontSize='.8rem';hdr.style.color='var(--accent)';
    hdr.textContent='Workflow: '+wfId+' ('+data.calls.length+' calls, $'+Number(data.total_cost_usd).toFixed(4)+', '+data.total_latency_ms+'ms total)';
    wrap.appendChild(hdr);
    if(data.calls.length>0){
      var minTs=Infinity,maxEnd=0;
      data.calls.forEach(function(c){
        var t0=new Date(c.timestamp).getTime();
        if(t0<minTs)minTs=t0;
        var t1=t0+(c.latency_ms||0);
        if(t1>maxEnd)maxEnd=t1;
      });
      var span=maxEnd-minTs||1;
      data.calls.forEach(function(c){
        var t0=new Date(c.timestamp).getTime();
        var left=((t0-minTs)/span)*100;
        var width=Math.max(((c.latency_ms||0)/span)*100,1);
        var label=document.createElement('div');
        label.className='wf-bar-label';
        label.textContent=(c.model||'?')+' '+c.latency_ms+'ms';
        var barWrap=document.createElement('div');
        barWrap.className='wf-bar-wrap';
        var bar=document.createElement('div');
        bar.className='wf-bar';
        bar.style.left=left+'%';bar.style.width=width+'%';
        var sc=c.status_code||0;
        bar.style.background=sc>=500?'var(--error)':sc>=400?'var(--warning)':'var(--success)';
        bar.textContent=(c.id||'').slice(0,8);
        bar.style.cursor='pointer';
        bar.onclick=function(){showDetail(c.id);};
        barWrap.appendChild(bar);
        wrap.appendChild(label);
        wrap.appendChild(barWrap);
      });
    }
  }catch(e){}
}

/* ---- Export helpers ---- */
function uuidv4(){return([1e7]+-1e3+-4e3+-8e3+-1e11).replace(/[018]/g,c=>(c^crypto.getRandomValues(new Uint8Array(1))[0]&15>>c/4).toString(16));}
function addMs(ts,ms){try{var d=new Date(ts);d.setMilliseconds(d.getMilliseconds()+ms);return d.toISOString();}catch(e){return ts;}}
function toMs(ts){try{return new Date(ts).getTime();}catch(e){return 0;}}

function fmtRecord(r,fmt){
  if(fmt==='csv') return null;
  if(fmt==='langfuse'){
    return JSON.stringify({batch:[{id:uuidv4(),type:'generation-create',body:{
      id:r.id,traceId:r.id,name:'llm-call',startTime:r.timestamp,endTime:addMs(r.timestamp,r.latency_ms||0),
      model:r.model,input:{messages:null},output:null,
      usage:{input:r.input_tokens||null,output:r.output_tokens||null,total:(r.input_tokens&&r.output_tokens)?r.input_tokens+r.output_tokens:null},
      metadata:{provider:r.provider,endpoint:r.endpoint,status_code:r.status_code,cost_usd:r.cost_usd||null,latency_ms:r.latency_ms},
      level:r.error?'ERROR':'DEFAULT'
    }}]});
  }
  if(fmt==='langsmith'){
    var sm=toMs(r.timestamp);
    return JSON.stringify({id:r.id,name:'llm-call',run_type:'llm',
      inputs:{messages:null},outputs:{output:null},
      start_time:sm,end_time:sm+(r.latency_ms||0),
      extra:{model:r.model,provider:r.provider,input_tokens:r.input_tokens||null,output_tokens:r.output_tokens||null,cost_usd:r.cost_usd||null,status_code:r.status_code},
      error:r.error||null,session_name:'opentrace',tags:['opentrace',r.provider]
    });
  }
  if(fmt==='weave'){
    var pt=r.input_tokens||0,ct=r.output_tokens||0;
    var usage={};usage[r.model]={prompt_tokens:pt,completion_tokens:ct,total_tokens:pt+ct,requests:1};
    return JSON.stringify({calls:[{id:r.id,op_name:'llm_call',started_at:r.timestamp,ended_at:addMs(r.timestamp,r.latency_ms||0),
      inputs:{model:r.model,messages:null},output:null,
      summary:{usage:usage},
      attributes:{provider:r.provider,endpoint:r.endpoint,status_code:r.status_code,cost_usd:r.cost_usd||null,latency_ms:r.latency_ms}
    }]});
  }
  return JSON.stringify(r);
}

async function downloadExport(fmt){
  var model=document.getElementById('f-model').value.trim();
  var provider=document.getElementById('f-provider').value.trim();
  var errors=document.getElementById('f-errors').checked;
  var url='/api/calls?limit=10000'+(model?'&model='+encodeURIComponent(model):'')+(provider?'&provider='+encodeURIComponent(provider):'')+(errors?'&errors=true':'');
  var data=await fetch(url).then(function(r){return r.json();});
  var rows=Array.isArray(data)?data:(data.calls||[]);
  var lines=[];
  if(fmt==='csv'){
    lines.push('id,timestamp,provider,model,endpoint,status_code,latency_ms,ttft_ms,input_tokens,output_tokens,cost_usd,error');
    rows.forEach(function(r){
      lines.push([r.id,r.timestamp,r.provider,r.model,r.endpoint,r.status_code,r.latency_ms,r.ttft_ms||'',r.input_tokens||'',r.output_tokens||'',r.cost_usd||'',r.error||''].map(function(v){return'"'+String(v).replace(/"/g,'""')+'"';}).join(','));
    });
  } else {
    rows.forEach(function(r){var l=fmtRecord(r,fmt);if(l)lines.push(l);});
  }
  var blob=new Blob([lines.join('\n')],{type:'text/plain'});
  var a=document.createElement('a');
  a.href=URL.createObjectURL(blob);
  a.download='opentrace-export-'+fmt+'-'+new Date().toISOString().slice(0,10)+(fmt==='csv'?'.csv':'.jsonl');
  document.body.appendChild(a);a.click();document.body.removeChild(a);
}

document.addEventListener('keydown',function(e){
  if(e.key==='Escape')closeDetail();
});
refresh();
loadHeatmap();
loadSparklines();
/* ---- SSE ---- */
var sseActive=false;
function connectSSE(){
  var es=new EventSource('/stream');
  es.onopen=function(){sseActive=true;};
  es.onmessage=function(evt){
    try{
      var r=JSON.parse(evt.data);
      if(r.type==='ping')return;
      var mf=document.getElementById('f-model').value.trim().toLowerCase();
      var pf=document.getElementById('f-provider').value.trim().toLowerCase();
      var af=document.getElementById('f-agent').value.trim().toLowerCase();
      var ef=document.getElementById('f-errors').checked;
      var sf=document.getElementById('f-search').value.trim();
      if(sf)return;
      if(mf&&!(r.model||'').toLowerCase().includes(mf))return;
      if(pf&&!(r.provider||'').toLowerCase().includes(pf))return;
      if(af&&!(r.agent_name||'').toLowerCase().includes(af))return;
      if(ef&&!(r.error||(r.status_code>=400||r.status_code===0)))return;
      prependRow(r);
    }catch(e){}
    fetch('/api/stats').then(function(res){return res.json();}).then(function(data){
      var s=data.stats;
      document.getElementById('s-calls').textContent=String(s.total_calls);
      document.getElementById('s-cost').textContent='$'+Number(s.total_cost_usd).toFixed(4);
      document.getElementById('s-lat').textContent=Math.round(s.avg_latency_ms)+'ms';
      document.getElementById('s-errors').textContent=String(s.error_count);
      document.getElementById('s-p99').textContent=Math.round(data.p99_latency_ms||0)+'ms';
      document.getElementById('s-hour').textContent=String(s.calls_last_hour);
    }).catch(function(){});
  };
  es.onerror=function(){
    sseActive=false;
    es.close();
    setTimeout(connectSSE,3000);
  };
}
connectSSE();
setInterval(function(){if(!sseActive)refresh();},2000);
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
    /// Broadcast sender — background poller pushes new CallRecords here;
    /// /stream SSE handlers subscribe to receive them.
    event_tx: broadcast::Sender<CallRecord>,
}

// ---------------------------------------------------------------------------
// API types
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
struct CallsQuery {
    limit: Option<usize>,
    model: Option<String>,
    provider: Option<String>,
    agent: Option<String>,
    workflow: Option<String>,
    errors: Option<bool>,
    since: Option<String>,
    until: Option<String>,
}

#[derive(Serialize)]
struct StatsResponse {
    stats: Stats,
    models: Vec<ModelStats>,
    p99_latency_ms: f64,
    providers: Vec<ProviderStats>,
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
        .route("/stream", get(stream_handler))
        .route("/api/calls", get(api_calls_handler))
        .route("/api/stats", get(api_stats_handler))
        .route("/api/search", get(api_search_handler))
        .route("/api/heatmap", get(api_heatmap_handler))
        .route("/api/detail/:id", get(api_detail_handler))
        .route("/api/workflow/:id", get(api_workflow_handler))
        .route("/api/agents", get(api_agents_handler))
        .route("/playground", get(playground_handler))
        .with_state(state)
}

async fn dashboard_handler() -> Html<&'static str> {
    Html(DASHBOARD_HTML)
}

async fn playground_handler() -> Html<&'static str> {
    Html(PLAYGROUND_HTML)
}

/// `GET /stream` — Server-Sent Events endpoint.
///
/// Each new `CallRecord` written to the database is broadcast to all
/// connected subscribers within ~250 ms.  Slow consumers lag-skip rather
/// than blocking the broadcaster.  A 30-second keepalive ping prevents
/// proxies and browsers from closing idle connections.
async fn stream_handler(
    State(state): State<ServeState>,
) -> Sse<impl futures_util::Stream<Item = Result<Event, Infallible>>> {
    let rx = state.event_tx.subscribe();
    let stream = BroadcastStream::new(rx).filter_map(|result| {
        futures_util::future::ready(match result {
            Ok(record) => serde_json::to_string(&record)
                .ok()
                .map(|json| Ok(Event::default().data(json))),
            // Subscriber lagged — skip missed records, keep stream alive.
            Err(_) => None,
        })
    });
    Sse::new(stream).keep_alive(
        KeepAlive::new()
            .interval(std::time::Duration::from_secs(30))
            .text("ping"),
    )
}

async fn api_calls_handler(
    State(state): State<ServeState>,
    Query(params): Query<CallsQuery>,
) -> Json<Vec<CallRecord>> {
    let limit = params.limit.unwrap_or(100);
    let filter = QueryFilter {
        errors_only: params.errors.unwrap_or(false),
        model: params.model,
        provider: params.provider,
        agent: params.agent,
        workflow: params.workflow,
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
        Ok(None) => Err((
            StatusCode::NOT_FOUND,
            format!("No call found with id prefix: {id}"),
        )),
        Err(e) => Err((StatusCode::INTERNAL_SERVER_ERROR, e.to_string())),
    }
}

// ---------------------------------------------------------------------------
// Workflow & agent API types
// ---------------------------------------------------------------------------

#[derive(Serialize)]
struct WorkflowResponse {
    workflow_id: String,
    calls: Vec<CallRecord>,
    total_cost_usd: f64,
    total_latency_ms: u64,
    agents: Vec<String>,
}

#[derive(Serialize)]
struct AgentsResponse {
    agents: Vec<AgentStats>,
}

async fn api_workflow_handler(
    State(state): State<ServeState>,
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

async fn api_agents_handler(
    State(state): State<ServeState>,
) -> Json<AgentsResponse> {
    let store = state.store.lock().unwrap();
    let agents = store
        .query_agent_stats(&QueryFilter::default())
        .unwrap_or_default();
    Json(AgentsResponse { agents })
}

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

pub async fn cmd_serve(port: u16) -> Result<()> {
    let store = Store::open()?;
    let store = Arc::new(Mutex::new(store));

    // Capacity 256: buffers ~1s of bursts; slow subscribers lag-skip.
    let (event_tx, _) = broadcast::channel::<CallRecord>(256);

    // Background poller: detect new DB rows every 250 ms and broadcast them
    // to all /stream subscribers.  Uses query_after so each poll is cheap
    // (index scan on timestamp).  Skips DB work entirely when no one is
    // connected (receiver_count() == 0 is a single atomic load).
    {
        let store_poll = Arc::clone(&store);
        let tx = event_tx.clone();
        tokio::spawn(async move {
            // Anchor to latest existing record so only arrivals after
            // `trace serve` started are broadcast.
            let mut last_ts = {
                let s = store_poll.lock().unwrap();
                s.query_filtered(1, &crate::store::QueryFilter::default())
                    .unwrap_or_default()
                    .into_iter()
                    .next()
                    .map(|r| r.timestamp)
                    .unwrap_or_else(|| "1970-01-01T00:00:00.000Z".to_string())
            };
            loop {
                tokio::time::sleep(std::time::Duration::from_millis(250)).await;
                if tx.receiver_count() == 0 {
                    continue;
                }
                let records = {
                    let s = store_poll.lock().unwrap();
                    s.query_after(&last_ts, &crate::store::QueryFilter::default(), 50)
                        .unwrap_or_default()
                };
                for record in records {
                    last_ts = record.timestamp.clone();
                    let _ = tx.send(record); // fire-and-forget
                }
            }
        });
    }

    let state = ServeState { store, event_tx };

    let app = router(state);
    let addr = format!("127.0.0.1:{}", port);
    let listener = tokio::net::TcpListener::bind(&addr).await?;

    println!("OpenTrace dashboard  → http://localhost:{}", port);
    println!(
        "Playground           → http://localhost:{}/playground",
        port
    );
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
            tags: None,
            agent_name: None,
            workflow_id: None,
            span_name: None,
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
        let (event_tx, _) = broadcast::channel::<CallRecord>(16);
        let state = ServeState {
            store: Arc::clone(&store),
            event_tx,
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
        assert_eq!(records.len(), 3);
    }

    #[tokio::test]
    async fn serve_api_stats_has_expected_fields() {
        let (app, _) = test_app();
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
        assert!(json["stats"]["total_calls"].is_number());
        assert!(json["models"].is_array());
    }

    #[tokio::test]
    async fn serve_errors_filter_applied() {
        let (app, store) = test_app();
        store
            .lock()
            .unwrap()
            .insert(&make_error_record("err-1"))
            .unwrap();
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
        assert_eq!(records.len(), 1);
    }

    #[tokio::test]
    async fn serve_api_search_returns_results() {
        let (app, store) = test_app();
        store
            .lock()
            .unwrap()
            .insert(&make_record("srch-1", "gpt-4o-serve-search-test", 200))
            .unwrap();
        let resp = app
            .oneshot(
                axum::http::Request::builder()
                    .uri("/api/search?q=gpt-4o-serve-search-test")
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
        let body = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        let data: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert!(data["count"].as_i64().unwrap() >= 1);
    }

    #[tokio::test]
    async fn serve_api_heatmap_returns_array() {
        let (app, store) = test_app();
        store
            .lock()
            .unwrap()
            .insert(&make_record("hm-1", "gpt-4o", 200))
            .unwrap();
        let resp = app
            .oneshot(
                axum::http::Request::builder()
                    .uri("/api/heatmap?days=7")
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
        let body = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        let data: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert!(data["days"].is_array());
    }

    #[tokio::test]
    async fn serve_playground_route_returns_200() {
        let (app, _) = test_app();
        let resp = app
            .oneshot(
                axum::http::Request::builder()
                    .uri("/playground")
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
    }

    #[tokio::test]
    async fn serve_api_detail_returns_record() {
        let (app, store) = test_app();
        store
            .lock()
            .unwrap()
            .insert(&make_record("detail-test-id-1234", "gpt-4o", 200))
            .unwrap();
        let resp = app
            .oneshot(
                axum::http::Request::builder()
                    .uri("/api/detail/detail-test-id-1234")
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
        let body = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        let record: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(record["model"].as_str().unwrap(), "gpt-4o");
    }

    #[tokio::test]
    async fn serve_api_detail_returns_404_for_unknown_id() {
        let (app, _) = test_app();
        let resp = app
            .oneshot(
                axum::http::Request::builder()
                    .uri("/api/detail/nonexistent-id")
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), 404);
    }

    #[tokio::test]
    async fn serve_stream_returns_event_stream_content_type() {
        let (app, _) = test_app();
        let resp = app
            .oneshot(
                axum::http::Request::builder()
                    .uri("/stream")
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
        let ct = resp
            .headers()
            .get("content-type")
            .unwrap()
            .to_str()
            .unwrap();
        assert!(
            ct.contains("text/event-stream"),
            "expected SSE content-type, got: {ct}"
        );
    }

    #[tokio::test]
    async fn serve_stream_delivers_broadcast_record() {
        use http_body_util::BodyExt;

        let store = Store::open_in_memory().unwrap();
        let store = Arc::new(Mutex::new(store));
        let (event_tx, _rx) = broadcast::channel::<CallRecord>(16);
        let state = ServeState {
            store: Arc::clone(&store),
            event_tx: event_tx.clone(),
        };
        let app = router(state);

        // Spawn a task that sends a record after a brief delay so that
        // stream_handler has time to subscribe before the send.
        let tx = event_tx.clone();
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            let _ = tx.send(make_record("sse-test-1", "gpt-4o", 200));
        });

        let resp = app
            .oneshot(
                axum::http::Request::builder()
                    .uri("/stream")
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(resp.status(), 200);

        // Read frames from the infinite SSE stream until we get a data line
        // or the 500ms safety timeout fires.
        let mut body = resp.into_body();
        let text = tokio::time::timeout(std::time::Duration::from_millis(500), async move {
            while let Some(Ok(frame)) = body.frame().await {
                if let Ok(data) = frame.into_data() {
                    let s = String::from_utf8_lossy(&data).into_owned();
                    if s.contains("data:") {
                        return s;
                    }
                }
            }
            String::new()
        })
        .await
        .expect("timed out waiting for SSE data frame");

        assert!(
            text.contains("data:"),
            "expected SSE data line, got: {text}"
        );
        assert!(
            text.contains("gpt-4o"),
            "expected model in SSE payload, got: {text}"
        );
    }
}
