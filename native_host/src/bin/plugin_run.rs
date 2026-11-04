//! Run a plug-in against a local data directory (a test bench, graph-agnostic):
//!   plugin_run DATA_DIR GRAPH [--cypher STATEMENTS.json] --wasm NAME FILE.wasm --script SCRIPT.json
//! STATEMENTS.json: ["CYPHER", ...] run in order on GRAPH first. SCRIPT.json: [{"fn": name, "input": {...}, "write": bool}]
//! Prints each call's output (one JSON per line) and its time.

use falkordb_native_host::{Engine, plugin_host, server::GraphCatalog};
use serde_json::{Value as J, json};

fn main() {
    let a: Vec<String> = std::env::args().collect();
    let arg = |k: &str| a.iter().position(|x| x == k).map(|i| a[i + 1].clone());
    let _engine = Engine::init().expect("engine");
    let catalog = GraphCatalog::open(&a[1]).expect("catalog");
    let graph = a[2].clone();
    let g = catalog.get_or_create(&graph).expect("graph");
    if let Some(p) = arg("--cypher") {
        let st: Vec<String> = serde_json::from_str(&std::fs::read_to_string(p).unwrap()).unwrap();
        let t = std::time::Instant::now();
        for (i, q) in st.iter().enumerate() {
            if let Err(e) = g.query(q) {
                panic!("statement {i}: {e}");
            }
        }
        eprintln!("{} statements in {:?}", st.len(), t.elapsed());
    }
    let host = plugin_host::get().expect("host");
    let wi = a.iter().position(|x| x == "--wasm").expect("--wasm NAME FILE");
    host.load(&a[wi + 1], &std::fs::read(&a[wi + 2]).unwrap(), true).expect("load");
    let script: Vec<J> = serde_json::from_str(&std::fs::read_to_string(arg("--script").expect("--script")).unwrap()).unwrap();
    for step in script {
        let t = std::time::Instant::now();
        let r = host.call(&a[wi + 1], step["fn"].as_str().unwrap(), &graph, catalog.get(&graph), step["write"].as_bool().unwrap_or(false),
                          &step["input"], Some(600_000));
        let out = match r {
            Ok(r) => json!({"fn": step["fn"], "ms": t.elapsed().as_millis() as u64, "output": r.output, "log": r.log}),
            Err(e) => json!({"fn": step["fn"], "ms": t.elapsed().as_millis() as u64, "error": e}),
        };
        println!("{out}");
    }
}
