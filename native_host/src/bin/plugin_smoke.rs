//! Plug-in host smoke test: plugin_smoke PATH_TO_KWEB_WASM
//! Loads the module into a fresh data directory and checks the sandbox contract (plugin_host.rs).

use falkordb_native_host::{Engine, plugin_host, server::GraphCatalog};
use serde_json::json;

fn main() {
    let _engine = Engine::init().expect("engine init");
    let wasm = std::env::args().nth(1).expect("usage: plugin_smoke KWEB.wasm");
    let bytes = std::fs::read(&wasm).expect("read wasm");
    let dir = std::env::temp_dir().join(format!("plugin_smoke_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let mut failures = 0;
    let mut check = |name: &str, ok: bool, detail: String| {
        println!("{} {name}{}", if ok { "ok  " } else { "FAIL" }, if ok { String::new() } else { format!(": {detail}") });
        if !ok {
            failures += 1;
        }
    };
    {
        let catalog = GraphCatalog::open(&dir).expect("open catalog");
        let host = plugin_host::get().expect("host started with the catalog");

        // load: validated, stored, listed with its functions
        let info = host.load("kweb", &bytes, false).expect("load");
        check("load lists the api functions", info.functions.contains(&"ping".to_string()), format!("{:?}", info.functions));
        check("a second load without replace is refused", host.load("kweb", &bytes, false).is_err(), String::new());
        check("bad names are refused", host.load("Bad Name", &bytes, false).is_err(), String::new());
        check("garbage is refused", host.load("junk", b"not wasm", false).is_err(), String::new());

        let g = catalog.get_or_create("smoke").expect("graph");
        g.query("CREATE (:A), (:A), (:B)").expect("seed");

        // a read call sees the named graph
        let r = host.call("kweb", "ping", "smoke", Some(g.clone()), false, &json!({"hi": 1}), None).expect("ping");
        check("ping reads the graph", r.output["nodes"] == json!(3), r.output.to_string());
        check("ping echoes its input", r.output["pong"] == json!({"hi": 1}), r.output.to_string());

        // writes need write scope; with it they are atomic and visible
        let ro = host.call("kweb", "write_probe", "smoke", Some(g.clone()), false, &json!({"n": 1}), None).expect("call");
        check("a read-only call cannot write", ro.output.get("error").is_some(), ro.output.to_string());
        let rw = host.call("kweb", "write_probe", "smoke", Some(g.clone()), true, &json!({"n": 2}), None).expect("call");
        check("a write call writes atomically", rw.output["committed"] == json!(true), rw.output.to_string());
        let n = g.query_read_only("MATCH (p:PluginProbe) RETURN count(p)").expect("count").rows[0][0].clone();
        check("the write is in the graph", n == "1" || n == "Int(1)", n);

        // a missing graph is not created by a read call
        let r = host.call("kweb", "ping", "nosuch", None, false, &json!({}), None).expect("call");
        check("a missing graph reads as an error", r.output.get("error").is_some(), r.output.to_string());

        // a runaway call is stopped at its deadline and the instance is rebuilt
        let t = std::time::Instant::now();
        let e = host.call("kweb", "spin", "smoke", Some(g.clone()), false, &json!({}), Some(300));
        check("a runaway call is stopped", e.as_ref().is_err_and(|m| m.contains("deadline")), format!("{:?}", e.as_ref().err()));
        check("it is stopped near its deadline", t.elapsed().as_millis() < 3000, format!("{} ms", t.elapsed().as_millis()));
        let r = host.call("kweb", "ping", "smoke", Some(g.clone()), false, &json!({}), None);
        check("the next call works", r.is_ok(), format!("{:?}", r.err()));

        // a panic is a trap for that call only
        let e = host.call("kweb", "panic", "smoke", Some(g.clone()), false, &json!({}), None);
        check("a panic fails only the call", e.is_err(), String::new());
        let r = host.call("kweb", "ping", "smoke", Some(g.clone()), false, &json!({}), None);
        check("the server and plug-in carry on", r.is_ok(), format!("{:?}", r.err()));
        check("unknown functions are refused", host.call("kweb", "nope", "smoke", Some(g.clone()), false, &json!({}), None).is_err(), String::new());
    }
    // the store survives a restart (same process: the host is process-wide, so check the files)
    let index = std::fs::read_to_string(dir.join("plugins").join("plugins.json")).unwrap_or_default();
    check("the plug-in index is persisted", index.contains("\"kweb\""), index.clone());
    check("the module is persisted", dir.join("plugins").join("kweb.wasm").exists(), String::new());
    let _ = std::fs::remove_dir_all(&dir);
    if failures > 0 {
        println!("{failures} failure(s)");
        std::process::exit(1);
    }
    println!("all plug-in host checks passed");
}
