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
    CallRecord, DailyStat, ModelStats, ProviderStats, QueryFilter, SearchResult, Stats, Store,
};

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
.cards-grid{display:grid;grid-template-columns:repeat(3,1fr);gap:.75rem;margin-bottom:.5rem}
.card{background:#1a1a1a;border:1px solid #2a2a2a;border-radius:4px;padding:.75rem 1rem}
.card-label{color:#666;font-size:.7rem;text-transform:uppercase;letter-spacing:.05em;margin-bottom:.25rem}
.card-value{color:#4fc3f7;font-size:1.3rem;font-weight:bold}
.section-label{color:#555;font-size:.65rem;text-transform:uppercase;letter-spacing:.08em;margin-bottom:.3rem;margin-top:.25rem}
#heatmap-wrap{margin-bottom:.75rem;overflow-x:auto}
#heatmap{display:block}
.hm-cell{cursor:default}
#heatmap-legend{display:flex;gap:.75rem;align-items:center;font-size:.7rem;color:#666;margin-top:.3rem;flex-wrap:wrap}
details#model-breakdown{margin-bottom:.75rem}
details#model-breakdown summary{color:#555;font-size:.7rem;text-transform:uppercase;letter-spacing:.08em;cursor:pointer;user-select:none;padding:.2rem 0}
details#model-breakdown summary:hover{color:#888}
#breakdown-table{margin-top:.5rem;width:100%;border-collapse:collapse;font-size:.75rem}
#breakdown-table th{background:#151515;color:#666;text-align:right;padding:.3rem .5rem;border-bottom:1px solid #2a2a2a}
#breakdown-table th:first-child{text-align:left}
#breakdown-table td{padding:.25rem .5rem;border-bottom:1px solid #1a1a1a;text-align:right}
#breakdown-table td:first-child{text-align:left}
.filters{display:flex;gap:.5rem;align-items:center;margin-bottom:.75rem;flex-wrap:wrap}
.filters input[type=text]{background:#1a1a1a;border:1px solid #333;color:#e0e0e0;padding:.3rem .5rem;border-radius:3px;font-family:monospace;font-size:.85rem}
#f-search{width:240px}
#f-model{width:140px}
#f-provider{width:130px}
.filters label{color:#888;font-size:.85rem;cursor:pointer;display:flex;align-items:center;gap:.25rem}
.export-wrap{position:relative;margin-left:auto}
.export-btn{background:#1a1a1a;border:1px solid #333;color:#4fc3f7;padding:.3rem .6rem;border-radius:3px;font-family:monospace;font-size:.82rem;cursor:pointer}
.export-btn:hover{border-color:#4fc3f7}
.export-menu{display:none;position:absolute;right:0;top:110%;background:#1e1e1e;border:1px solid #333;border-radius:3px;z-index:200;min-width:130px;box-shadow:0 4px 12px rgba(0,0,0,.5)}
.export-menu a{display:block;padding:.35rem .7rem;color:#ccc;font-size:.8rem;cursor:pointer;text-decoration:none;font-family:monospace}
.export-menu a:hover{background:#2a2a2a;color:#4fc3f7}
.export-wrap:hover .export-menu{display:block}
table{width:100%;border-collapse:collapse;font-size:.78rem}
th{background:#151515;color:#666;text-align:right;padding:.35rem .5rem;border-bottom:1px solid #2a2a2a;position:sticky;top:0;white-space:nowrap}
th:first-child,th:nth-child(3),th:nth-child(4){text-align:left}
td{padding:.3rem .5rem;border-bottom:1px solid #1a1a1a;text-align:right;white-space:nowrap}
td:first-child,td:nth-child(3),td:nth-child(4){text-align:left}
tr:hover td{background:#1c1c1c}
tr.row-err td{color:#ef5350}
tr.row-err:hover td{background:#1c1c1c}
.ok{color:#66bb6a}.err{color:#ef5350}.dim{color:#555}
#status{color:#444;font-size:.7rem;margin-top:.5rem}
#detail-panel{display:none;position:fixed;top:0;right:0;width:420px;height:100vh;background:#111;border-left:1px solid #2a2a2a;overflow-y:auto;padding:1rem;z-index:100;font-size:.78rem}
.detail-header{display:flex;justify-content:space-between;align-items:center;margin-bottom:.75rem}
.detail-title{color:#4fc3f7;font-weight:bold}
.detail-close{background:none;border:none;color:#888;cursor:pointer;font-size:1rem;line-height:1}
.section-hdr{color:#4fc3f7;font-size:.65rem;text-transform:uppercase;letter-spacing:.08em;margin:.75rem 0 .3rem;border-bottom:1px solid #2a2a2a;padding-bottom:.2rem}
.section-hdr:first-child{margin-top:0}
body.light{background:#f5f5f5;color:#222}
body.light .card{background:#fff;border-color:#ddd}
body.light .card-label{color:#999}
body.light .card-value{color:#0077b6}
body.light th{background:#eee;color:#888}
body.light #breakdown-table th{background:#eee;color:#888}
body.light td{border-color:#eee}
body.light #breakdown-table td{border-color:#eee}
body.light tr:hover td{background:#fafafa}
body.light .filters input[type=text]{background:#fff;border-color:#ccc;color:#222}
body.light #dark-toggle{border-color:#ccc;color:#666}
body.light #status{color:#aaa}
body.light #detail-panel{background:#fafafa;border-color:#ddd}
body.light .section-hdr{border-color:#ddd;color:#0077b6}
@media(max-width:600px){
  .cards-grid{grid-template-columns:1fr}
  table{display:block;overflow-x:auto}
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
  <div class="card"><div class="card-label">Total calls</div><div class="card-value" id="s-calls">-</div></div>
  <div class="card"><div class="card-label">Total cost</div><div class="card-value" id="s-cost">-</div></div>
  <div class="card"><div class="card-label">Avg latency</div><div class="card-value" id="s-lat">-</div></div>
</div>
<!-- Stat cards row 2 -->
<div class="cards-grid" style="margin-bottom:.75rem">
  <div class="card"><div class="card-label">Errors</div><div class="card-value" id="s-errors">-</div></div>
  <div class="card"><div class="card-label">p99 latency</div><div class="card-value" id="s-p99">-</div></div>
  <div class="card"><div class="card-label">Calls (1h)</div><div class="card-value" id="s-hour">-</div></div>
</div>

<!-- Heatmap -->
<div id="heatmap-wrap">
  <div class="section-label">DAILY COST &mdash; 90 days</div>
  <svg id="heatmap" height="88"></svg>
  <div id="heatmap-legend"></div>
</div>

<!-- Model breakdown collapsible -->
<details id="model-breakdown">
  <summary>by model &#9658;</summary>
  <table id="breakdown-table">
    <thead><tr>
      <th>model</th><th>calls</th><th>cost</th><th>avg ms</th><th>errors</th>
    </tr></thead>
    <tbody id="breakdown-tbody"></tbody>
  </table>
</details>

<!-- Filters -->
<div class="filters">
  <input type="text" id="f-search" placeholder="Search prompts, responses, models..." oninput="debouncedSearch()">
  <input type="text" id="f-model" placeholder="Filter model..." oninput="saveFilters();refresh()">
  <input type="text" id="f-provider" placeholder="Filter provider..." oninput="saveFilters();refresh()">
  <label><input type="checkbox" id="f-errors" onchange="refresh()"> Errors only</label>
  <div class="export-wrap">
    <button class="export-btn">Export ▾</button>
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
<table>
  <thead><tr>
    <th>id</th><th>timestamp</th><th>model</th><th>provider</th><th>status</th>
    <th>latency</th><th>ttft</th><th>in</th><th>out</th><th>cost</th>
  </tr></thead>
  <tbody id="tbody"></tbody>
</table>
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
function fmtMs(v){return v==null?'-':v+'ms'}
function fmtCost(c){return c==null?'-':'$'+Number(c).toFixed(4)}
function clearEl(el){while(el.firstChild)el.removeChild(el.firstChild);}
function makeCell(text,cls){var td=document.createElement('td');td.textContent=text;if(cls)td.className=cls;return td;}
function makeTr(label,val){
  var tr=document.createElement('tr');
  var td1=document.createElement('td');td1.style.color='#666';td1.style.padding='.2rem .4rem';td1.style.whiteSpace='nowrap';td1.style.verticalAlign='top';td1.textContent=label;
  var td2=document.createElement('td');td2.style.color='#e0e0e0';td2.style.padding='.2rem .4rem';td2.style.wordBreak='break-all';td2.textContent=String(val);
  tr.appendChild(td1);tr.appendChild(td2);return tr;
}

// sticky filters
(function(){
  var m=localStorage.getItem('ot-filter-model');
  var p=localStorage.getItem('ot-filter-provider');
  if(m)document.getElementById('f-model').value=m;
  if(p)document.getElementById('f-provider').value=p;
})();
function saveFilters(){
  localStorage.setItem('ot-filter-model',document.getElementById('f-model').value);
  localStorage.setItem('ot-filter-provider',document.getElementById('f-provider').value);
}

// dark mode
function toggleDark(){
  document.body.classList.toggle('light');
  var light=document.body.classList.contains('light');
  localStorage.setItem('ot-theme',light?'light':'dark');
  document.getElementById('dark-toggle').textContent=light?'\u263e':'\u2600';
}
(function(){
  if(localStorage.getItem('ot-theme')==='light'){
    document.body.classList.add('light');
    document.getElementById('dark-toggle').textContent='\u263e';
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
    ]);

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

    if(r.request_body){
      addSectionHdr('request');
      var pre=document.createElement('pre');
      pre.style.background='#1a1a1a';pre.style.padding='.5rem';pre.style.borderRadius='3px';
      pre.style.fontSize='.72rem';pre.style.overflowX='auto';pre.style.whiteSpace='pre-wrap';
      pre.textContent=r.request_body;
      content.appendChild(pre);
    }
    if(r.response_body){
      addSectionHdr('response');
      var pre2=document.createElement('pre');
      pre2.style.background='#1a1a1a';pre2.style.padding='.5rem';pre2.style.borderRadius='3px';
      pre2.style.fontSize='.72rem';pre2.style.overflowX='auto';pre2.style.whiteSpace='pre-wrap';
      pre2.textContent=r.response_body;
      content.appendChild(pre2);
    }
    if(r.error){
      addSectionHdr('error');
      var errDiv=document.createElement('div');
      errDiv.style.color='#ef5350';errDiv.style.padding='.3rem .4rem';errDiv.style.fontSize='.78rem';
      errDiv.textContent=r.error;
      content.appendChild(errDiv);
    }
  }catch(e){content.textContent='Error: '+e.message;}
}

// Build a single table row for a CallRecord (shared by renderRows and prependRow)
function makeRow(r){
  var ok=r.status_code>=200&&r.status_code<400&&!r.error;
  var tr=document.createElement('tr');
  tr.style.cursor='pointer';
  if(!ok)tr.className='row-err';
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
  tr.appendChild(makeCell((r.timestamp||'').slice(0,19),'dim'));
  tr.appendChild(makeCell(r.model||'-',null));
  tr.appendChild(makeCell(r.provider||'-','dim'));
  tr.appendChild(makeCell(String(r.status_code),ok?'ok':'err'));
  tr.appendChild(makeCell(fmtMs(r.latency_ms),null));
  tr.appendChild(makeCell(fmtMs(r.ttft_ms),'dim'));
  tr.appendChild(makeCell(r.input_tokens!=null?String(r.input_tokens):'-','dim'));
  tr.appendChild(makeCell(r.output_tokens!=null?String(r.output_tokens):'-','dim'));
  tr.appendChild(makeCell(fmtCost(r.cost_usd),null));
  return tr;
}
// row rendering (shared by refresh and search)
function renderRows(calls){
  var tbody=document.getElementById('tbody');
  clearEl(tbody);
  for(var i=0;i<calls.length;i++){tbody.appendChild(makeRow(calls[i]));}
}
// Prepend a single new row from SSE without rebuilding the whole table.
// Caps at 200 rows to prevent unbounded DOM growth.
function prependRow(r){
  var tbody=document.getElementById('tbody');
  tbody.insertBefore(makeRow(r),tbody.firstChild);
  while(tbody.rows.length>200)tbody.deleteRow(tbody.rows.length-1);
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
  var provider=document.getElementById('f-provider').value;
  var errors=document.getElementById('f-errors').checked;
  var url='/api/calls?limit=100';
  if(model)url+='&model='+encodeURIComponent(model);
  if(provider)url+='&provider='+encodeURIComponent(provider);
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
    renderRows(calls);
    // populate model breakdown
    var btbody=document.getElementById('breakdown-tbody');
    clearEl(btbody);
    (data.models||[]).forEach(function(m){
      var tr=document.createElement('tr');
      [m.model,m.calls,'$'+Number(m.total_cost_usd).toFixed(4),Math.round(m.avg_latency_ms)+'ms',m.error_count].forEach(function(v,i){
        var td=document.createElement('td');
        td.textContent=String(v);
        td.style.padding='.25rem .5rem';
        td.style.borderBottom='1px solid #1a1a1a';
        if(i>0)td.style.textAlign='right';
        tr.appendChild(td);
      });
      btbody.appendChild(tr);
    });
    document.getElementById('status').textContent='Updated '+new Date().toLocaleTimeString()+' \u2014 '+calls.length+' calls';
  }catch(e){document.getElementById('status').textContent='Error: '+e.message;}
}

// heatmap
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

// ---- Export helpers ----
function uuidv4(){return([1e7]+-1e3+-4e3+-8e3+-1e11).replace(/[018]/g,c=>(c^crypto.getRandomValues(new Uint8Array(1))[0]&15>>c/4).toString(16));}
function addMs(ts,ms){try{var d=new Date(ts);d.setMilliseconds(d.getMilliseconds()+ms);return d.toISOString();}catch(e){return ts;}}
function toMs(ts){try{return new Date(ts).getTime();}catch(e){return 0;}}

function fmtRecord(r,fmt){
  if(fmt==='csv') return null; // handled separately
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
  // jsonl (default)
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
// SSE: receive new call records in real time.
// Falls back to 2-second polling while the SSE connection is down.
var sseActive=false;
function connectSSE(){
  var es=new EventSource('/stream');
  es.onopen=function(){sseActive=true;};
  es.onmessage=function(evt){
    try{
      var r=JSON.parse(evt.data);
      if(r.type==='ping')return;
      // Honour active filters — only prepend if the record matches
      var mf=document.getElementById('f-model').value.trim().toLowerCase();
      var pf=document.getElementById('f-provider').value.trim().toLowerCase();
      var ef=document.getElementById('f-errors').checked;
      var sf=document.getElementById('f-search').value.trim();
      if(sf)return; // search active — let the search view handle its own refresh
      if(mf&&!(r.model||'').toLowerCase().includes(mf))return;
      if(pf&&!(r.provider||'').toLowerCase().includes(pf))return;
      if(ef&&!(r.error||(r.status_code>=400||r.status_code===0)))return;
      prependRow(r);
    }catch(e){}
    // Refresh stat cards on every new call (single cheap fetch)
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
    setTimeout(connectSSE,3000); // reconnect after 3 s
  };
}
connectSSE();
// Polling fallback — fires only when SSE is reconnecting
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
        let store = Store::open_in_memory().unwrap();
        let store = Arc::new(Mutex::new(store));
        let (event_tx, _rx) = broadcast::channel::<CallRecord>(16);
        let state = ServeState {
            store: Arc::clone(&store),
            event_tx: event_tx.clone(),
        };
        let app = router(state);

        // Send one record into the channel before the request so it is
        // buffered and delivered immediately when the stream opens.
        // _rx keeps the receiver alive so send() does not return SendError.
        event_tx
            .send(make_record("sse-test-1", "gpt-4o", 200))
            .unwrap();
        // Drop the extra sender so the BroadcastStream closes after draining.
        drop(event_tx);

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
        let body = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        let body_str = std::str::from_utf8(&body).unwrap();
        assert!(body_str.contains("data:"), "expected SSE data line");
        assert!(body_str.contains("gpt-4o"), "expected model in SSE payload");
    }
}
