use std::io::{self, BufRead, Write};

use falkordb_native_host::{Engine, NativeGraph};

fn smoke() -> Result<(), String> {
    let graph = NativeGraph::new("native-smoke");

    let r = graph.query("RETURN 1 AS value")?;
    if r.rows.len() != 1 {
        return Err(format!("RETURN smoke expected one row, got {}", r.rows.len()));
    }
    println!("[PASS] RETURN 1");

    graph.query("CREATE (:Smoke {id: 1, name: 'windows-native'})")?;
    println!("[PASS] CREATE node + MVCC commit");

    let r = graph.query(
        "MATCH (n:Smoke {id: 1, name: 'windows-native'}) RETURN n.id AS id",
    )?;
    if r.rows.len() != 1 {
        return Err(format!("property MATCH expected one row, got {}", r.rows.len()));
    }
    println!("[PASS] MATCH committed node + properties");

    graph.query("CREATE (:Smoke {id: 2})")?;
    graph.query(
        "MATCH (a:Smoke {id: 1}), (b:Smoke {id: 2}) CREATE (a)-[:LINK]->(b)",
    )?;
    let r = graph.query(
        "MATCH (a:Smoke {id: 1})-[:LINK]->(b:Smoke {id: 2}) RETURN a.id, b.id",
    )?;
    if r.rows.len() != 1 {
        return Err(format!("relationship traversal expected one row, got {}", r.rows.len()));
    }
    println!("[PASS] relationship CREATE + traversal");

    graph.query("MATCH (n:Smoke {id: 2}) SET n.updated = true")?;
    let r = graph.query("MATCH (n:Smoke {id: 2, updated: true}) RETURN n.id")?;
    if r.rows.len() != 1 {
        return Err(format!("SET/property readback expected one row, got {}", r.rows.len()));
    }
    println!("[PASS] SET property + readback");

    let r = graph.query("MATCH (n:Smoke) RETURN count(n) AS count")?;
    if r.rows.len() != 1 {
        return Err("aggregate smoke returned no row".to_string());
    }
    println!("[PASS] aggregate read after multiple commits");

    match graph.query("CREATE INDEX FOR (n:Smoke) ON (n.id)") {
        Err(e) if e.contains("RediSearch-backed index layer") => {
            println!("[PASS] index path is explicitly blocked in bring-up mode");
        }
        Err(e) => return Err(format!("unexpected index guard error: {e}")),
        Ok(_) => return Err("index DDL unexpectedly executed with link stubs enabled".to_string()),
    }

    println!("NATIVE_SMOKE_OK");
    Ok(())
}

fn main() -> Result<(), String> {
    let _engine = Engine::init()?;

    if std::env::args().any(|arg| arg == "--smoke") {
        return smoke();
    }

    let graph = NativeGraph::new("native");
    println!("FalkorDB native Windows host v0.3");
    println!("Type Cypher and press Enter. Type :quit to exit.");
    println!("Run with --smoke for the non-indexed native smoke suite.");

    let stdin = io::stdin();
    let mut stdout = io::stdout();

    loop {
        print!("cypher> ");
        stdout.flush().map_err(|e| e.to_string())?;

        let mut line = String::new();
        let n = stdin.lock().read_line(&mut line).map_err(|e| e.to_string())?;
        if n == 0 {
            break;
        }
        let query = line.trim();
        if query.is_empty() {
            continue;
        }
        if query == ":quit" || query == ":q" {
            break;
        }

        match graph.query(query) {
            Ok(out) => {
                if !out.columns.is_empty() {
                    println!("{}", out.columns.join(" | "));
                    for row in out.rows {
                        println!("{}", row.join(" | "));
                    }
                }
                println!("{:?}", out.stats);
            }
            Err(err) => eprintln!("error: {err}"),
        }
    }

    Ok(())
}
