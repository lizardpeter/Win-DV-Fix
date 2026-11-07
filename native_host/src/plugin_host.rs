//! Sandboxed WebAssembly plug-ins.
//!
//! The executable stays graph- and schema-agnostic: it only knows how to store a compiled module, run one of its
//! functions against one named graph, and hand it a few host calls. What a plug-in means (an ontology engine, a
//! study scheduler, anything) is the plug-in's business.
//!
//! Sandbox. A plug-in sees only what the host gives it: Cypher on the one graph the call names (writes only when the
//! caller holds write scope), the clock, and a log line. No files, no network, no other graph. Its memory is capped,
//! a call has a wall-clock deadline, and a trap ends the call (the instance is dropped and rebuilt on the next call),
//! never the server.
//!
//! ABI (plain core WebAssembly, any source language):
//! ABI 1 is for 32-bit modules (pointers and lengths i32, results packed as below); ABI 2 is the same for a module whose
//! memory is 64-bit (memory64): pointers and lengths are i64, and a returned buffer is an i64 pointer to [len: u64 LE][bytes]
//! (freed as (ptr, 8 + len)). The host picks the ABI from the module's exported memory.
//!   exports  memory
//!            plugin_alloc(len: i32) -> i32          a buffer the host may fill
//!            plugin_free(ptr: i32, len: i32)
//!            api_<name>(ptr: i32, len: i32) -> i64  JSON in, JSON out; returns (ptr << 32) | len of a buffer the
//!                                                   plug-in allocated, which the host frees after reading
//!   imports  host.query(ptr, len, read_only: i32) -> i64      Cypher text in; JSON result out (packed as above)
//!            host.transaction(ptr, len) -> i64                {"queries": [{"cypher", "read_only"}]} applied atomically
//!            host.now_ms() -> i64
//!            host.log(ptr, len)
//! An instance lives on between calls for the same (plug-in, graph), so a plug-in may keep state in memory (and
//! check the graph's version itself); calls on one instance run one at a time.

use std::{
    collections::HashMap,
    fs,
    path::{Path, PathBuf},
    sync::{
        Arc, OnceLock,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use parking_lot::Mutex;
use ring::digest;
use serde::{Deserialize, Serialize};
use serde_json::{Value as JsonValue, json};
use wasmtime::{Caller, Config, Engine, Func, Instance, Linker, Memory, Module, Store, StoreLimits, StoreLimitsBuilder, Val};

use crate::NativeGraph;

/// Largest module accepted.
pub const MAX_MODULE_BYTES: usize = 64 * 1024 * 1024;
/// Linear memory a 32-bit plug-in instance may grow to.
pub const MAX_MEMORY_BYTES: usize = 4 * 1024 * 1024 * 1024 - 65536;
/// Linear memory a 64-bit (memory64) plug-in instance may grow to, unless FALKORDB_PLUGIN_MEMORY_GB says otherwise.
pub const DEFAULT_MAX_MEMORY64_BYTES: usize = 16 * 1024 * 1024 * 1024;

fn max_memory64() -> usize {
    std::env::var("FALKORDB_PLUGIN_MEMORY_GB")
        .ok()
        .and_then(|v| v.trim().parse::<usize>().ok())
        .filter(|g| *g >= 1)
        .map(|g| g.saturating_mul(1024 * 1024 * 1024))
        .unwrap_or(DEFAULT_MAX_MEMORY64_BYTES)
}
/// Default and largest wall-clock time for one call.
pub const DEFAULT_CALL_TIMEOUT_MS: u64 = 60_000;
pub const MAX_CALL_TIMEOUT_MS: u64 = 600_000;
/// Largest JSON a host call or a plug-in result may carry.
pub const MAX_MESSAGE_BYTES: usize = 256 * 1024 * 1024;
const LOG_LINES: usize = 200;
const TICK: Duration = Duration::from_millis(10);

#[derive(Clone, Serialize, Deserialize)]
pub struct PluginInfo {
    pub name: String,
    pub sha256: String,
    pub bytes: usize,
    pub loaded_at_ms: u64,
    pub functions: Vec<String>,
}

struct Loaded {
    info: PluginInfo,
    module: Module,
    /// a 64-bit memory: ABI 2
    wide: bool,
}

struct HostState {
    wide: bool,
    graph: Option<Arc<NativeGraph>>,
    graph_name: String,
    may_write: bool,
    limits: StoreLimits,
    log: Vec<String>,
}

struct Live {
    store: Store<HostState>,
    memory: Memory,
    alloc: Func,
    free: Func,
    instance: Instance,
    wide: bool,
}

/// a pointer-sized argument in the module's ABI
fn word(wide: bool, x: u64) -> Val {
    if wide { Val::I64(x as i64) } else { Val::I32(x as u32 as i32) }
}

fn word_of(v: &Val) -> u64 {
    match v {
        Val::I32(x) => *x as u32 as u64,
        Val::I64(x) => *x as u64,
        _ => 0,
    }
}

fn call_alloc(alloc: &Func, mut store: impl wasmtime::AsContextMut, wide: bool, len: u64) -> wasmtime::Result<u64> {
    let mut out = [word(wide, 0)];
    alloc.call(&mut store, &[word(wide, len)], &mut out)?;
    Ok(word_of(&out[0]))
}

fn call_free(free: &Func, mut store: impl wasmtime::AsContextMut, wide: bool, ptr: u64, len: u64) {
    let _ = free.call(&mut store, &[word(wide, ptr), word(wide, len)], &mut []);
}

pub struct PluginHost {
    dir: PathBuf,
    engine: Engine,
    plugins: Mutex<HashMap<String, Arc<Loaded>>>,
    live: Mutex<HashMap<(String, String), Arc<Mutex<Option<Live>>>>>,
    stop: Arc<AtomicBool>,
}

static HOST: OnceLock<PluginHost> = OnceLock::new();

/// The host for a data directory; opened once per process (later calls return the same host).
pub fn open(data_dir: &Path) -> Result<&'static PluginHost, String> {
    if let Some(h) = HOST.get() {
        return Ok(h);
    }
    let h = PluginHost::new(data_dir)?;
    let _ = HOST.set(h);
    Ok(HOST.get().expect("plugin host set"))
}

pub fn get() -> Option<&'static PluginHost> {
    HOST.get()
}

fn valid_name(name: &str) -> bool {
    !name.is_empty() && name.len() <= 64 && name.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_' || b == b'-')
}

/// a module whose exported memory is 64-bit uses ABI 2
fn module_is_wide(m: &Module) -> bool {
    m.exports().find(|e| e.name() == "memory").and_then(|e| e.ty().memory().map(|t| t.is_64())).unwrap_or(false)
}

fn now_ms() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0)
}

fn pack(ptr: i32, len: i32) -> i64 {
    ((ptr as u32 as i64) << 32) | (len as u32 as i64)
}

fn unpack(v: i64) -> (usize, usize) {
    (((v as u64) >> 32) as usize, (v as u64 & 0xffff_ffff) as usize)
}

impl PluginHost {
    fn new(data_dir: &Path) -> Result<Self, String> {
        let mut cfg = Config::new();
        cfg.epoch_interruption(true);
        cfg.wasm_memory64(true);
        let engine = Engine::new(&cfg).map_err(|e| format!("plugin engine: {e}"))?;
        let stop = Arc::new(AtomicBool::new(false));
        {
            // the epoch clock behind call deadlines
            let engine = engine.clone();
            let stop = stop.clone();
            std::thread::Builder::new()
                .name("plugin-epoch".into())
                .spawn(move || {
                    while !stop.load(Ordering::Relaxed) {
                        std::thread::sleep(TICK);
                        engine.increment_epoch();
                    }
                })
                .map_err(|e| format!("plugin epoch thread: {e}"))?;
        }
        let host = PluginHost {
            dir: data_dir.join("plugins"),
            engine,
            plugins: Mutex::new(HashMap::new()),
            live: Mutex::new(HashMap::new()),
            stop,
        };
        host.restore()?;
        Ok(host)
    }

    fn index_path(&self) -> PathBuf {
        self.dir.join("plugins.json")
    }

    fn restore(&self) -> Result<(), String> {
        fs::create_dir_all(&self.dir).map_err(|e| format!("create plugin directory {}: {e}", self.dir.display()))?;
        let path = self.index_path();
        if !path.exists() {
            return Ok(());
        }
        let infos: Vec<PluginInfo> = serde_json::from_slice(&fs::read(&path).map_err(|e| format!("read {}: {e}", path.display()))?)
            .map_err(|e| format!("decode {}: {e}", path.display()))?;
        let mut map = self.plugins.lock();
        for info in infos {
            let bytes = fs::read(self.dir.join(format!("{}.wasm", info.name)))
                .map_err(|e| format!("read plug-in {}: {e}", info.name))?;
            if sha256_hex(&bytes) != info.sha256 {
                return Err(format!("plug-in {}: stored module does not match its recorded hash", info.name));
            }
            let module = self.compile(&bytes)?;
            let wide = module_is_wide(&module);
            map.insert(info.name.clone(), Arc::new(Loaded { info, module, wide }));
        }
        Ok(())
    }

    fn persist(&self, map: &HashMap<String, Arc<Loaded>>) -> Result<(), String> {
        let mut infos: Vec<&PluginInfo> = map.values().map(|l| &l.info).collect();
        infos.sort_by(|a, b| a.name.cmp(&b.name));
        let tmp = self.dir.join("plugins.json.tmp");
        fs::write(&tmp, serde_json::to_vec_pretty(&infos).map_err(|e| e.to_string())?)
            .map_err(|e| format!("write {}: {e}", tmp.display()))?;
        fs::rename(&tmp, self.index_path()).map_err(|e| format!("replace plug-in index: {e}"))
    }

    fn compile(&self, bytes: &[u8]) -> Result<Module, String> {
        let module = Module::new(&self.engine, bytes).map_err(|e| format!("invalid WebAssembly module: {e:#}"))?;
        let wide = module.exports().find(|e| e.name() == "memory").and_then(|e| e.ty().memory().map(|m| m.is_64())).unwrap_or(false);
        // the exports' signatures must match the module's ABI
        let w = if wide { wasmtime::ValType::I64 } else { wasmtime::ValType::I32 };
        for e in module.exports() {
            if let Some(f) = e.ty().func() {
                let (ps, rs): (Vec<_>, Vec<_>) = (f.params().collect(), f.results().collect());
                let want_ok = match e.name() {
                    "plugin_alloc" => ps.len() == 1 && ps[0].matches(&w) && rs.len() == 1 && rs[0].matches(&w),
                    "plugin_free" => ps.len() == 2 && ps.iter().all(|p| p.matches(&w)) && rs.is_empty(),
                    n if n.starts_with("api_") => ps.len() == 2 && ps.iter().all(|p| p.matches(&w)) && rs.len() == 1 && rs[0].matches(&wasmtime::ValType::I64),
                    _ => true,
                };
                if !want_ok {
                    return Err(format!("export {} does not have the {} ABI signature", e.name(), if wide { "64-bit" } else { "32-bit" }));
                }
            }
        }
        for (name, want) in [("memory", "memory"), ("plugin_alloc", "func"), ("plugin_free", "func")] {
            let ok = module.exports().any(|e| {
                e.name() == name
                    && match want {
                        "memory" => e.ty().memory().is_some(),
                        _ => e.ty().func().is_some(),
                    }
            });
            if !ok {
                return Err(format!("a plug-in must export `{name}`"));
            }
        }
        for imp in module.imports() {
            if imp.module() != "host" || !matches!(imp.name(), "query" | "transaction" | "now_ms" | "log") {
                return Err(format!("the plug-in imports {}.{}, which the host does not provide", imp.module(), imp.name()));
            }
        }
        Ok(module)
    }

    pub fn list(&self) -> Vec<PluginInfo> {
        let mut v: Vec<PluginInfo> = self.plugins.lock().values().map(|l| l.info.clone()).collect();
        v.sort_by(|a, b| a.name.cmp(&b.name));
        v
    }

    pub fn load(&self, name: &str, bytes: &[u8], replace: bool) -> Result<PluginInfo, String> {
        if !valid_name(name) {
            return Err("a plug-in name is 1-64 characters of a-z, 0-9, _ and -".into());
        }
        if bytes.len() > MAX_MODULE_BYTES {
            return Err(format!("module is larger than {MAX_MODULE_BYTES} bytes"));
        }
        let module = self.compile(bytes)?;
        let wide = module_is_wide(&module);
        let functions: Vec<String> = module
            .exports()
            .filter(|e| e.name().starts_with("api_") && e.ty().func().is_some())
            .map(|e| e.name()["api_".len()..].to_string())
            .collect();
        let info = PluginInfo { name: name.to_string(), sha256: sha256_hex(bytes), bytes: bytes.len(), loaded_at_ms: now_ms(), functions };
        let mut map = self.plugins.lock();
        if map.contains_key(name) && !replace {
            return Err(format!("plug-in {name} already exists (pass replace to swap it)"));
        }
        let path = self.dir.join(format!("{name}.wasm"));
        let tmp = self.dir.join(format!("{name}.wasm.tmp"));
        fs::write(&tmp, bytes).map_err(|e| format!("write {}: {e}", tmp.display()))?;
        fs::rename(&tmp, &path).map_err(|e| format!("store plug-in: {e}"))?;
        let previous = map.insert(name.to_string(), Arc::new(Loaded { info: info.clone(), module, wide }));
        if let Err(e) = self.persist(&map) {
            match previous {
                Some(p) => {
                    map.insert(name.to_string(), p);
                }
                None => {
                    map.remove(name);
                }
            }
            return Err(e);
        }
        drop(map);
        self.drop_instances(name);
        Ok(info)
    }

    pub fn delete(&self, name: &str) -> Result<bool, String> {
        let mut map = self.plugins.lock();
        let Some(prev) = map.remove(name) else { return Ok(false) };
        if let Err(e) = self.persist(&map) {
            map.insert(name.to_string(), prev);
            return Err(e);
        }
        drop(map);
        let _ = fs::remove_file(self.dir.join(format!("{name}.wasm")));
        self.drop_instances(name);
        Ok(true)
    }

    /// Forget every instance of a plug-in, or of every plug-in on a graph (after the graph is deleted).
    pub fn drop_instances(&self, plugin: &str) {
        self.live.lock().retain(|(p, _), _| p != plugin);
    }

    pub fn drop_graph(&self, graph: &str) {
        self.live.lock().retain(|(_, g), _| g != graph);
    }

    fn instantiate(&self, loaded: &Loaded, graph_name: &str) -> Result<Live, String> {
        let wide = loaded.wide;
        let cap = if wide { max_memory64() } else { MAX_MEMORY_BYTES };
        let limits = StoreLimitsBuilder::new().memory_size(cap).instances(1).tables(16).memories(1).build();
        let mut store = Store::new(
            &self.engine,
            HostState { wide, graph: None, graph_name: graph_name.to_string(), may_write: false, limits, log: Vec::new() },
        );
        store.limiter(|s| &mut s.limits);
        store.set_epoch_deadline(u64::MAX / 2);
        let mut linker: Linker<HostState> = Linker::new(&self.engine);
        linker.func_wrap("host", "now_ms", || -> i64 { now_ms() as i64 }).map_err(|e| format!("plug-in host functions: {e}"))?;
        let r = if wide {
            linker
                .func_wrap("host", "log", |mut c: Caller<'_, HostState>, ptr: i64, len: i64| host_log(&mut c, ptr as u64, len as u64))
                .and_then(|l| {
                    l.func_wrap("host", "query", |mut c: Caller<'_, HostState>, ptr: i64, len: i64, read_only: i32| -> wasmtime::Result<i64> {
                        host_query_call(&mut c, ptr as u64, len as u64, read_only)
                    })
                })
                .and_then(|l| {
                    l.func_wrap("host", "transaction", |mut c: Caller<'_, HostState>, ptr: i64, len: i64| -> wasmtime::Result<i64> {
                        host_transaction_call(&mut c, ptr as u64, len as u64)
                    })
                })
                .map(|_| ())
        } else {
            linker
                .func_wrap("host", "log", |mut c: Caller<'_, HostState>, ptr: i32, len: i32| host_log(&mut c, ptr as u32 as u64, len as u32 as u64))
                .and_then(|l| {
                    l.func_wrap("host", "query", |mut c: Caller<'_, HostState>, ptr: i32, len: i32, read_only: i32| -> wasmtime::Result<i64> {
                        host_query_call(&mut c, ptr as u32 as u64, len as u32 as u64, read_only)
                    })
                })
                .and_then(|l| {
                    l.func_wrap("host", "transaction", |mut c: Caller<'_, HostState>, ptr: i32, len: i32| -> wasmtime::Result<i64> {
                        host_transaction_call(&mut c, ptr as u32 as u64, len as u32 as u64)
                    })
                })
                .map(|_| ())
        };
        r.map_err(|e| format!("plug-in host functions: {e}"))?;
        let instance = linker.instantiate(&mut store, &loaded.module).map_err(|e| format!("instantiate: {e:#}"))?;
        let memory = instance.get_memory(&mut store, "memory").ok_or("no exported memory")?;
        let alloc = instance.get_func(&mut store, "plugin_alloc").ok_or("no plugin_alloc")?;
        let free = instance.get_func(&mut store, "plugin_free").ok_or("no plugin_free")?;
        Ok(Live { store, memory, alloc, free, instance, wide })
    }

    /// Run `api_<function>` of a plug-in against one graph with JSON input. `may_write`: the caller holds write scope.
    pub fn call(
        &self,
        plugin: &str,
        function: &str,
        graph_name: &str,
        graph: Option<Arc<NativeGraph>>,
        may_write: bool,
        input: &JsonValue,
        timeout_ms: Option<u64>,
    ) -> Result<CallResult, String> {
        let loaded = self.plugins.lock().get(plugin).cloned().ok_or_else(|| format!("no plug-in named {plugin}"))?;
        if !loaded.info.functions.iter().any(|f| f == function) {
            return Err(format!("plug-in {plugin} has no function {function} (it has: {})", loaded.info.functions.join(", ")));
        }
        let slot = self
            .live
            .lock()
            .entry((plugin.to_string(), graph_name.to_string()))
            .or_insert_with(|| Arc::new(Mutex::new(None)))
            .clone();
        let mut guard = slot.lock();
        if guard.is_none() {
            *guard = Some(self.instantiate(&loaded, graph_name)?);
        }
        let started = Instant::now();
        let deadline_ticks = (timeout_ms.unwrap_or(DEFAULT_CALL_TIMEOUT_MS).clamp(1, MAX_CALL_TIMEOUT_MS) / TICK.as_millis() as u64).max(1);
        let result = {
            let live = guard.as_mut().expect("instance");
            live.store.data_mut().graph = graph;
            live.store.data_mut().may_write = may_write;
            live.store.data_mut().log.clear();
            live.store.set_epoch_deadline(deadline_ticks);
            let r = run_call(live, function, input);
            live.store.data_mut().graph = None;
            live.store.data_mut().may_write = false;
            live.store.set_epoch_deadline(u64::MAX / 2);
            let log = std::mem::take(&mut live.store.data_mut().log);
            (r, log)
        };
        let (r, log) = result;
        match r {
            Ok(output) => Ok(CallResult { output, log, elapsed_ms: started.elapsed().as_millis() as u64 }),
            Err(e) => {
                // a trap (deadline, out of memory, a bug) leaves the instance in an unknown state: rebuild it next time
                *guard = None;
                Err(if log.is_empty() { e } else { format!("{e}; plug-in log: {}", log.join(" | ")) })
            }
        }
    }
}

impl PluginHost {
    /// a plug-in's page: api_ui({"path"}) -> {"kind": "html"|"js"|"css"|"svg"|"text", "body"}. Called with no graph and
    /// read-only scope: a page shell carries no data.
    pub fn ui(&self, plugin: &str, path: &str) -> Result<(String, Vec<u8>), String> {
        if !self.plugins.lock().get(plugin).is_some_and(|l| l.info.functions.iter().any(|f| f == "ui")) {
            return Err(format!("no plug-in page at /plugins/{plugin}/"));
        }
        let r = self.call(plugin, "ui", "", None, false, &json!({"path": path}), Some(5_000))?;
        let kind = r.output["kind"].as_str().unwrap_or("text").to_string();
        let body = r.output["body"].as_str().ok_or_else(|| r.output["error"].as_str().unwrap_or("no page").to_string())?;
        Ok((kind, body.as_bytes().to_vec()))
    }
}

impl Drop for PluginHost {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

pub struct CallResult {
    pub output: JsonValue,
    pub log: Vec<String>,
    pub elapsed_ms: u64,
}

fn run_call(live: &mut Live, function: &str, input: &JsonValue) -> Result<JsonValue, String> {
    let wide = live.wide;
    let f = live.instance.get_func(&mut live.store, &format!("api_{function}")).ok_or_else(|| format!("no api_{function}"))?;
    let bytes = serde_json::to_vec(input).map_err(|e| e.to_string())?;
    if !wide && i32::try_from(bytes.len()).is_err() {
        return Err("input too large".into());
    }
    let len = bytes.len() as u64;
    let ptr = call_alloc(&live.alloc, &mut live.store, wide, len).map_err(trap)?;
    live.memory
        .write(&mut live.store, ptr as usize, &bytes)
        .map_err(|_| "plugin_alloc returned a buffer outside its memory".to_string())?;
    let mut out_v = [Val::I64(0)];
    f.call(&mut live.store, &[word(wide, ptr), word(wide, len)], &mut out_v).map_err(trap)?;
    call_free(&live.free, &mut live.store, wide, ptr, len);
    let packed = match out_v[0] {
        Val::I64(x) => x,
        _ => return Err(format!("api_{function} must return i64")),
    };
    let (optr, olen, freelen) = if wide {
        let p = packed as u64 as usize;
        let mut lb = [0u8; 8];
        live.memory.read(&live.store, p, &mut lb).map_err(|_| "plug-in returned a result outside its memory".to_string())?;
        let l = u64::from_le_bytes(lb) as usize;
        (p + 8, l, 8 + l)
    } else {
        let (p, l) = unpack(packed);
        (p, l, l)
    };
    if olen > MAX_MESSAGE_BYTES {
        return Err("plug-in result too large".into());
    }
    let mut out = vec![0u8; olen];
    live.memory
        .read(&live.store, optr, &mut out)
        .map_err(|_| "plug-in returned a result outside its memory".to_string())?;
    let base = if wide { optr - 8 } else { optr };
    call_free(&live.free, &mut live.store, wide, base as u64, freelen as u64);
    serde_json::from_slice(&out).map_err(|e| format!("plug-in result is not JSON: {e}"))
}

fn trap(e: wasmtime::Error) -> String {
    if let Some(t) = e.downcast_ref::<wasmtime::Trap>() {
        if *t == wasmtime::Trap::Interrupt {
            return "the plug-in call ran past its deadline and was stopped".into();
        }
    }
    format!("plug-in trapped: {e:#}")
}

fn guest_memory(c: &mut Caller<'_, HostState>) -> wasmtime::Result<Memory> {
    c.get_export("memory").and_then(|e| e.into_memory()).ok_or_else(|| wasmtime::Error::msg("no exported memory"))
}

fn read_guest(c: &mut Caller<'_, HostState>, ptr: u64, len: u64) -> wasmtime::Result<Vec<u8>> {
    let len = len as usize;
    if len > MAX_MESSAGE_BYTES {
        return Err(wasmtime::Error::msg("host call message too large"));
    }
    let mem = guest_memory(c)?;
    let mut buf = vec![0u8; len];
    mem.read(&*c, ptr as usize, &mut buf).map_err(|_| wasmtime::Error::msg("buffer outside the plug-in's memory"))?;
    Ok(buf)
}

/// a reply into the plug-in's memory, returned in its ABI
fn write_guest(c: &mut Caller<'_, HostState>, bytes: &[u8]) -> wasmtime::Result<i64> {
    let wide = c.data().wide;
    let alloc = c.get_export("plugin_alloc").and_then(|e| e.into_func()).ok_or_else(|| wasmtime::Error::msg("no plugin_alloc"))?;
    if !wide && i32::try_from(bytes.len()).is_err() {
        return Err(wasmtime::Error::msg("host reply too large"));
    }
    let total = if wide { 8 + bytes.len() } else { bytes.len() };
    let ptr = call_alloc(&alloc, &mut *c, wide, total as u64)?;
    let mem = guest_memory(c)?;
    if wide {
        mem.write(&mut *c, ptr as usize, &(bytes.len() as u64).to_le_bytes()).map_err(|_| wasmtime::Error::msg("plugin_alloc returned a bad buffer"))?;
        mem.write(&mut *c, ptr as usize + 8, bytes).map_err(|_| wasmtime::Error::msg("plugin_alloc returned a bad buffer"))?;
        Ok(ptr as i64)
    } else {
        mem.write(&mut *c, ptr as usize, bytes).map_err(|_| wasmtime::Error::msg("plugin_alloc returned a bad buffer"))?;
        Ok(pack(ptr as u32 as i32, bytes.len() as i32))
    }
}

fn host_log(c: &mut Caller<'_, HostState>, ptr: u64, len: u64) {
    if let Ok(s) = read_guest(c, ptr, len) {
        let line = String::from_utf8_lossy(&s).chars().take(2000).collect::<String>();
        let log = &mut c.data_mut().log;
        if log.len() < LOG_LINES {
            log.push(line);
        }
    }
}

fn host_query_call(c: &mut Caller<'_, HostState>, ptr: u64, len: u64, read_only: i32) -> wasmtime::Result<i64> {
    let cypher = String::from_utf8(read_guest(c, ptr, len)?).map_err(|_| wasmtime::Error::msg("query text is not UTF-8"))?;
    let out = host_query(c.data(), &cypher, read_only != 0);
    write_guest(c, &serde_json::to_vec(&out)?)
}

fn host_transaction_call(c: &mut Caller<'_, HostState>, ptr: u64, len: u64) -> wasmtime::Result<i64> {
    let body = read_guest(c, ptr, len)?;
    let out = host_transaction(c.data(), &body);
    write_guest(c, &serde_json::to_vec(&out)?)
}

fn host_query(st: &HostState, cypher: &str, read_only: bool) -> JsonValue {
    let Some(graph) = &st.graph else {
        return json!({"error": format!("graph {} does not exist", st.graph_name)});
    };
    if !read_only && !st.may_write {
        return json!({"error": "this call has read-only scope"});
    }
    let r = if read_only { graph.query_read_only(cypher) } else { graph.query(cypher) };
    match r {
        Ok(o) => crate::api::query_output_json(&st.graph_name, o),
        Err(e) => json!({"error": e}),
    }
}

#[derive(Deserialize)]
struct TxQuery {
    cypher: String,
    #[serde(default)]
    read_only: bool,
}

#[derive(Deserialize)]
struct TxBody {
    queries: Vec<TxQuery>,
}

fn host_transaction(st: &HostState, body: &[u8]) -> JsonValue {
    let Some(graph) = &st.graph else {
        return json!({"error": format!("graph {} does not exist", st.graph_name)});
    };
    let tx: TxBody = match serde_json::from_slice(body) {
        Ok(t) => t,
        Err(e) => return json!({"error": format!("transaction body: {e}")}),
    };
    if tx.queries.is_empty() {
        return json!({"results": [], "committed": true});
    }
    let all_read = tx.queries.iter().all(|q| q.read_only);
    if !all_read && !st.may_write {
        return json!({"error": "this call has read-only scope"});
    }
    let cyphers: Vec<String> = tx.queries.iter().map(|q| q.cypher.clone()).collect();
    let r = if all_read {
        graph.snapshot_read(&cyphers)
    } else {
        let flags: Vec<bool> = tx.queries.iter().map(|q| q.read_only).collect();
        graph.transaction_flagged(&cyphers, &flags)
    };
    match r {
        Ok(outs) => json!({
            "results": outs.into_iter().map(|o| crate::api::query_output_json(&st.graph_name, o)).collect::<Vec<_>>(),
            "committed": true
        }),
        Err((at, e)) => json!({"committed": false, "failed_at": at, "error": e}),
    }
}

fn sha256_hex(bytes: &[u8]) -> String {
    digest::digest(&digest::SHA256, bytes).as_ref().iter().map(|b| format!("{b:02x}")).collect()
}
