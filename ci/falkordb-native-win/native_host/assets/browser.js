(function(){
"use strict";

var state={
  username:"",password:"",graph:"",
  schema:{labels:[],relationships:[],properties:[]},
  nodes:new Map(),edges:new Map(),positions:new Map(),
  selected:null,panX:0,panY:0,scale:1,drag:null,panning:null
};

var el=function(id){return document.getElementById(id);};
var loginPanel=el("loginPanel"), explorer=el("explorer"), loginError=el("loginError");
var graphSelect=el("graphSelect"), queryInput=el("queryInput"), queryError=el("queryError"), queryStatus=el("queryStatus");
var svg=el("graphSvg"), viewport=el("viewport"), nodesLayer=el("nodesLayer"), edgesLayer=el("edgesLayer"), edgeLabelsLayer=el("edgeLabelsLayer");

function utf8Base64(s){
  var bytes=new TextEncoder().encode(s), bin="";
  for(var i=0;i<bytes.length;i++) bin+=String.fromCharCode(bytes[i]);
  return btoa(bin);
}
function authValue(){return "Basic "+utf8Base64(state.username+":"+state.password);}
async function request(path,options){
  options=options||{};
  var headers=Object.assign({},options.headers||{});
  headers["X-FalkorDB-Dashboard-Authorization"]=authValue();
  if(options.body && !headers["Content-Type"]) headers["Content-Type"]="application/json";
  var response=await fetch(path,Object.assign({},options,{headers:headers,cache:"no-store"}));
  var text=await response.text(), body=null;
  try{body=text?JSON.parse(text):{};}catch(_){body={error:"invalid_json",message:text};}
  if(!response.ok){
    var msg=(body && (body.message||body.error)) || ("HTTP "+response.status);
    throw new Error(msg);
  }
  return body;
}
function setBusy(on,msg){
  el("runButton").disabled=on;
  queryStatus.textContent=msg||"Ready";
}
function showError(target,err){
  target.textContent=String(err && err.message ? err.message : err);
  target.hidden=false;
}
function clearError(target){target.hidden=true;target.textContent="";}

async function connect(){
  clearError(loginError);
  state.username=el("viewerUsername").value.trim();
  state.password=el("viewerPassword").value;
  if(!state.username || !state.password){showError(loginError,"Username and password are required.");return;}
  el("connectButton").disabled=true;
  try{
    var data=await request("/v1/graphs");
    graphSelect.innerHTML="";
    (data.graphs||[]).forEach(function(name){
      var opt=document.createElement("option");opt.value=name;opt.textContent=name;graphSelect.appendChild(opt);
    });
    if(!data.graphs || !data.graphs.length) throw new Error("No graphs are loaded.");
    state.graph=data.graphs[0];
    graphSelect.value=state.graph;
    await loadSchema();
    loginPanel.hidden=true;explorer.hidden=false;
    el("connectionText").textContent=state.username+" @ "+location.host;
    await runQuery();
  }catch(err){
    state.password="";
    showError(loginError,err);
  }finally{el("connectButton").disabled=false;}
}
function disconnect(){
  state.username="";state.password="";state.graph="";state.nodes.clear();state.edges.clear();state.positions.clear();
  explorer.hidden=true;loginPanel.hidden=false;el("viewerPassword").value="";clearGraph();
}
async function query(cypher){
  return request("/v1/query",{method:"POST",body:JSON.stringify({graph:state.graph,cypher:cypher,read_only:true})});
}
async function loadSchema(){
  var qs=[
    "CALL db.labels() YIELD label RETURN label",
    "CALL db.relationshipTypes() YIELD relationshipType RETURN relationshipType",
    "CALL db.propertyKeys() YIELD propertyKey RETURN propertyKey"
  ];
  var all=await Promise.all(qs.map(query));
  state.schema.labels=(all[0].rows||[]).map(function(r){return r[0];});
  state.schema.relationships=(all[1].rows||[]).map(function(r){return r[0];});
  state.schema.properties=(all[2].rows||[]).map(function(r){return r[0];});
}
function propertyMap(item){
  var out={};
  (item.properties||[]).forEach(function(p){
    var key=state.schema.properties[p.attribute_id] || ("property_"+p.attribute_id);
    out[key]=p.value;
  });
  return out;
}
function normalizeNode(v){
  return {
    id:v.id,
    labels:(v.label_ids||[]).map(function(i){return state.schema.labels[i]||("label_"+i);}),
    properties:propertyMap(v)
  };
}
function normalizeEdge(v){
  return {
    id:v.id,source:v.source_id,target:v.destination_id,
    type:state.schema.relationships[v.relationship_type_id]||("relationship_"+v.relationship_type_id),
    properties:propertyMap(v)
  };
}
function collectValue(v,nodes,edges){
  if(v===null || v===undefined) return;
  if(Array.isArray(v)){v.forEach(function(x){collectValue(x,nodes,edges);});return;}
  if(typeof v!=="object") return;
  if(v.type==="node"){
    var n=normalizeNode(v);nodes.set(n.id,n);return;
  }
  if(v.type==="relationship"){
    var e=normalizeEdge(v);edges.set(e.id,e);return;
  }
  if(v.type==="path"){
    collectValue(v.nodes,nodes,edges);collectValue(v.relationships,nodes,edges);return;
  }
  Object.keys(v).forEach(function(k){collectValue(v[k],nodes,edges);});
}
function mergeResult(data,replace){
  var nodes=replace?new Map():new Map(state.nodes), edges=replace?new Map():new Map(state.edges);
  (data.rows||[]).forEach(function(row){row.forEach(function(v){collectValue(v,nodes,edges);});});
  edges.forEach(function(e){
    if(!nodes.has(e.source)) nodes.set(e.source,{id:e.source,labels:["Node"],properties:{}});
    if(!nodes.has(e.target)) nodes.set(e.target,{id:e.target,labels:["Node"],properties:{}});
  });
  state.nodes=nodes;state.edges=edges;
  layoutGraph();
  renderGraph();
}
function nodeCaption(n){
  var p=n.properties||{};
  var value=p.canonical_name||p.name||p.title||p.display_name||p.id;
  if(value===undefined || value===null || value==="") value=(n.labels[0]||"Node")+" #"+n.id;
  value=String(value);
  return value.length>34?value.slice(0,31)+"…":value;
}
function seedPositions(){
  var arr=Array.from(state.nodes.values()), total=Math.max(arr.length,1);
  arr.forEach(function(n,i){
    if(state.positions.has(n.id)) return;
    var a=(i/total)*Math.PI*2;
    state.positions.set(n.id,{x:Math.cos(a)*240+400,y:Math.sin(a)*240+300,vx:0,vy:0});
  });
}
function layoutGraph(){
  seedPositions();
  var nodes=Array.from(state.nodes.values());
  if(nodes.length>500) nodes=nodes.slice(0,500);
  var allowed=new Set(nodes.map(function(n){return n.id;}));
  var edges=Array.from(state.edges.values()).filter(function(e){return allowed.has(e.source)&&allowed.has(e.target);});
  var iters=nodes.length>250?55:110;
  for(var step=0;step<iters;step++){
    for(var i=0;i<nodes.length;i++){
      var a=state.positions.get(nodes[i].id);
      for(var j=i+1;j<nodes.length;j++){
        var b=state.positions.get(nodes[j].id),dx=a.x-b.x,dy=a.y-b.y,d2=dx*dx+dy*dy+0.1;
        var f=Math.min(1800/d2,2.2),d=Math.sqrt(d2);dx/=d;dy/=d;
        a.vx+=dx*f;a.vy+=dy*f;b.vx-=dx*f;b.vy-=dy*f;
      }
    }
    edges.forEach(function(e){
      var a=state.positions.get(e.source),b=state.positions.get(e.target);
      if(!a||!b)return;var dx=b.x-a.x,dy=b.y-a.y,d=Math.sqrt(dx*dx+dy*dy)||1;
      var f=(d-115)*0.0035,ux=dx/d,uy=dy/d;
      a.vx+=ux*f;a.vy+=uy*f;b.vx-=ux*f;b.vy-=uy*f;
    });
    nodes.forEach(function(n){
      var p=state.positions.get(n.id);
      p.vx+=(400-p.x)*0.0009;p.vy+=(300-p.y)*0.0009;
      p.vx*=0.84;p.vy*=0.84;p.x+=p.vx;p.y+=p.vy;
    });
  }
}
function svgEl(name,attrs){
  var e=document.createElementNS("http://www.w3.org/2000/svg",name);
  Object.keys(attrs||{}).forEach(function(k){e.setAttribute(k,String(attrs[k]));});
  return e;
}
function clearGraph(){
  nodesLayer.replaceChildren();edgesLayer.replaceChildren();edgeLabelsLayer.replaceChildren();
  el("emptyCanvas").hidden=false;el("graphSummary").textContent="No result loaded";
}
function renderGraph(){
  nodesLayer.replaceChildren();edgesLayer.replaceChildren();edgeLabelsLayer.replaceChildren();
  var allNodes=Array.from(state.nodes.values()), displayNodes=allNodes.slice(0,500);
  var visible=new Set(displayNodes.map(function(n){return n.id;}));
  var displayEdges=Array.from(state.edges.values()).filter(function(e){return visible.has(e.source)&&visible.has(e.target);}).slice(0,1000);
  displayEdges.forEach(function(e){
    var a=state.positions.get(e.source),b=state.positions.get(e.target);if(!a||!b)return;
    var line=svgEl("line",{x1:a.x,y1:a.y,x2:b.x,y2:b.y,"data-edge-id":e.id,class:"graph-edge"+(state.selected&&state.selected.kind==="edge"&&state.selected.id===e.id?" selected":"")});
    line.addEventListener("click",function(ev){ev.stopPropagation();selectEdge(e.id);});
    edgesLayer.appendChild(line);
    var t=svgEl("text",{x:(a.x+b.x)/2,y:(a.y+b.y)/2-5,class:"edge-label","data-edge-id":e.id});
    t.textContent=e.type;t.addEventListener("click",function(ev){ev.stopPropagation();selectEdge(e.id);});edgeLabelsLayer.appendChild(t);
  });
  displayNodes.forEach(function(n){
    var p=state.positions.get(n.id);if(!p)return;
    var g=svgEl("g",{"data-node-id":n.id,transform:"translate("+p.x+" "+p.y+")"});
    var c=svgEl("circle",{r:20,class:"node-circle"+(state.selected&&state.selected.kind==="node"&&state.selected.id===n.id?" selected":"")});
    var t=svgEl("text",{y:36,class:"node-label"});t.textContent=nodeCaption(n);
    g.appendChild(c);g.appendChild(t);
    g.addEventListener("click",function(ev){ev.stopPropagation();selectNode(n.id);});
    g.addEventListener("dblclick",function(ev){ev.stopPropagation();expandNode(n.id);});
    g.addEventListener("pointerdown",function(ev){ev.stopPropagation();startNodeDrag(ev,n.id);});
    nodesLayer.appendChild(g);
  });
  el("emptyCanvas").hidden=displayNodes.length>0;
  var note="";
  if(allNodes.length>500 || state.edges.size>1000) note=" (canvas capped at 500 nodes / 1000 relationships; table retains the full result)";
  el("graphSummary").textContent=state.nodes.size+" nodes · "+state.edges.size+" relationships"+note;
  applyView();
}
function applyView(){viewport.setAttribute("transform","translate("+state.panX+" "+state.panY+") scale("+state.scale+")");}
function startNodeDrag(ev,id){
  svg.setPointerCapture(ev.pointerId);state.drag={id:id,pointerId:ev.pointerId};moveDraggedNode(ev);
}
function moveDraggedNode(ev){
  if(!state.drag)return;var rect=svg.getBoundingClientRect();
  var p=state.positions.get(state.drag.id);if(!p)return;
  p.x=(ev.clientX-rect.left-state.panX)/state.scale;p.y=(ev.clientY-rect.top-state.panY)/state.scale;p.vx=0;p.vy=0;renderGraph();
}
svg.addEventListener("pointerdown",function(ev){
  if(ev.target===svg){svg.setPointerCapture(ev.pointerId);state.panning={pointerId:ev.pointerId,x:ev.clientX,y:ev.clientY,px:state.panX,py:state.panY};}
});
svg.addEventListener("pointermove",function(ev){
  if(state.drag&&state.drag.pointerId===ev.pointerId){moveDraggedNode(ev);return;}
  if(state.panning&&state.panning.pointerId===ev.pointerId){state.panX=state.panning.px+(ev.clientX-state.panning.x);state.panY=state.panning.py+(ev.clientY-state.panning.y);applyView();}
});
svg.addEventListener("pointerup",function(ev){if(state.drag&&state.drag.pointerId===ev.pointerId)state.drag=null;if(state.panning&&state.panning.pointerId===ev.pointerId)state.panning=null;});
svg.addEventListener("pointercancel",function(){state.drag=null;state.panning=null;});
svg.addEventListener("wheel",function(ev){ev.preventDefault();var old=state.scale;state.scale=Math.max(.2,Math.min(4,state.scale*(ev.deltaY<0?1.12:.89)));var rect=svg.getBoundingClientRect(),mx=ev.clientX-rect.left,my=ev.clientY-rect.top;state.panX=mx-(mx-state.panX)*(state.scale/old);state.panY=my-(my-state.panY)*(state.scale/old);applyView();},{passive:false});
svg.addEventListener("click",function(){state.selected=null;renderInspector();renderGraph();});

function fitView(){
  if(!state.nodes.size){state.panX=0;state.panY=0;state.scale=1;applyView();return;}
  var pts=Array.from(state.nodes.keys()).slice(0,500).map(function(id){return state.positions.get(id);}).filter(Boolean);
  var minX=Math.min.apply(null,pts.map(function(p){return p.x;})),maxX=Math.max.apply(null,pts.map(function(p){return p.x;}));
  var minY=Math.min.apply(null,pts.map(function(p){return p.y;})),maxY=Math.max.apply(null,pts.map(function(p){return p.y;}));
  var rect=svg.getBoundingClientRect(),w=Math.max(100,maxX-minX+120),h=Math.max(100,maxY-minY+120);
  state.scale=Math.max(.2,Math.min(1.5,Math.min(rect.width/w,rect.height/h)));
  state.panX=rect.width/2-((minX+maxX)/2)*state.scale;state.panY=rect.height/2-((minY+maxY)/2)*state.scale;applyView();
}
function selectNode(id){state.selected={kind:"node",id:id};renderInspector();renderGraph();}
function selectEdge(id){state.selected={kind:"edge",id:id};renderInspector();renderGraph();}
function esc(s){return String(s).replace(/[&<>"']/g,function(c){return {"&":"&amp;","<":"&lt;",">":"&gt;","\"":"&quot;","'":"&#39;"}[c];});}
function renderInspector(){
  var box=el("inspectorBody");
  if(!state.selected){box.className="empty-inspector";box.textContent="Nothing selected.";return;}
  box.className="";
  var item=state.selected.kind==="node"?state.nodes.get(state.selected.id):state.edges.get(state.selected.id);
  if(!item){box.textContent="Selection no longer exists.";return;}
  var title=state.selected.kind==="node"?nodeCaption(item):item.type;
  var kind=state.selected.kind==="node"?(item.labels.join(", ")||"Node"):(item.source+" → "+item.target);
  var props=Object.assign({},item.properties||{});
  if(state.selected.kind==="node"){props["_internal_id"]=item.id;props["_labels"]=item.labels.join(", ");}
  else{props["_internal_id"]=item.id;props["_type"]=item.type;props["_source_id"]=item.source;props["_destination_id"]=item.target;}
  var html='<div class="inspect-title">'+esc(title)+'</div><div class="inspect-kind">'+esc(kind)+'</div><dl class="props">';
  Object.keys(props).forEach(function(k){var v=props[k];if(typeof v==="object")v=JSON.stringify(v);html+='<dt>'+esc(k)+'</dt><dd>'+esc(v)+'</dd>';});
  html+="</dl>";
  box.innerHTML=html;
}
function compact(v){
  if(v===null)return "null";
  if(typeof v==="string")return v;
  if(typeof v==="number"||typeof v==="boolean")return String(v);
  if(v.type==="node"){var n=normalizeNode(v);return "["+(n.labels.join(":")||"Node")+"] "+nodeCaption(n);}
  if(v.type==="relationship"){var e=normalizeEdge(v);return "("+e.source+")-["+e.type+"]->("+e.target+")";}
  if(v.type==="path")return "Path("+((v.nodes||[]).length)+" nodes, "+((v.relationships||[]).length)+" relationships)";
  try{return JSON.stringify(v);}catch(_){return String(v);}
}
function renderTable(data){
  var head=el("resultsHead"),body=el("resultsBody");head.replaceChildren();body.replaceChildren();
  var tr=document.createElement("tr");(data.columns||[]).forEach(function(c){var th=document.createElement("th");th.textContent=c;tr.appendChild(th);});head.appendChild(tr);
  (data.rows||[]).slice(0,1000).forEach(function(row){var r=document.createElement("tr");row.forEach(function(v){var td=document.createElement("td");td.textContent=compact(v);r.appendChild(td);});body.appendChild(r);});
  var extra=(data.rows||[]).length>1000?" (table shows first 1000 rows)":"";
  el("tableSummary").textContent=(data.rows||[]).length+" rows · "+(data.stats&&data.stats.execution_time_ms!==undefined?Number(data.stats.execution_time_ms).toFixed(2)+" ms":"")+" · graph version "+data.graph_version+extra;
}
async function runQuery(){
  clearError(queryError);setBusy(true,"Running…");
  try{
    var data=await query(queryInput.value);
    mergeResult(data,true);renderTable(data);fitView();
    queryStatus.textContent="Completed";
  }catch(err){showError(queryError,err);queryStatus.textContent="Failed";}
  finally{el("runButton").disabled=false;}
}
async function expandNode(id){
  clearError(queryError);setBusy(true,"Expanding node "+id+"…");
  try{
    var data=await query("MATCH (n) WHERE id(n) = "+Number(id)+" OPTIONAL MATCH (n)-[r]-(m) RETURN n,r,m LIMIT 100");
    mergeResult(data,false);renderTable(data);queryStatus.textContent="Expanded node "+id;
  }catch(err){showError(queryError,err);queryStatus.textContent="Expand failed";}
  finally{el("runButton").disabled=false;}
}
async function changeGraph(){
  state.graph=graphSelect.value;state.schema={labels:[],relationships:[],properties:[]};state.nodes.clear();state.edges.clear();state.positions.clear();state.selected=null;
  clearGraph();renderInspector();setBusy(true,"Loading schema…");
  try{await loadSchema();await runQuery();}catch(err){showError(queryError,err);setBusy(false,"Failed");}
}
el("connectButton").addEventListener("click",connect);
el("disconnectButton").addEventListener("click",disconnect);
el("runButton").addEventListener("click",runQuery);
el("sampleButton").addEventListener("click",function(){queryInput.value="MATCH (n)-[r]->(m) RETURN n,r,m LIMIT 100";runQuery();});
el("fitButton").addEventListener("click",fitView);
graphSelect.addEventListener("change",changeGraph);
queryInput.addEventListener("keydown",function(ev){if((ev.ctrlKey||ev.metaKey)&&ev.key==="Enter"){ev.preventDefault();runQuery();}});
el("viewerPassword").addEventListener("keydown",function(ev){if(ev.key==="Enter")connect();});
window.addEventListener("resize",applyView);
})();
