use std::{
    fs,
    path::PathBuf,
    sync::{Arc, Barrier},
    thread,
    time::{SystemTime, UNIX_EPOCH},
};

use falkordb_native_host::{Engine, NativeGraph};
use falkordb_native_host::wire::WireValue;

const GRAPH_COUNT: usize = 4;
const WRITERS_PER_GRAPH: usize = 2;
const WRITES_PER_WRITER: usize = 20;
const READERS_PER_GRAPH: usize = 2;
const READS_PER_READER: usize = 40;

fn unique_root() -> PathBuf {
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    std::env::temp_dir().join(format!(
        "falkordb-concurrency-smoke-{}-{stamp}",
        std::process::id()
    ))
}

fn main() -> Result<(), String> {
    let _engine = Engine::init()?;
    let root = unique_root();
    fs::create_dir_all(&root)
        .map_err(|e| format!("create concurrency smoke dir {}: {e}", root.display()))?;

    let mut graphs = Vec::with_capacity(GRAPH_COUNT);
    for i in 0..GRAPH_COUNT {
        let name = format!("concurrency-{i}");
        let wal = root.join(format!("{name}.wal"));
        let graph = Arc::new(NativeGraph::open_persistent(&name, &wal)?);
        graph.query("CREATE (:Counter {id:0,value:0})")?;
        graphs.push((name, wal, graph));
    }

    let participants = GRAPH_COUNT * (WRITERS_PER_GRAPH + READERS_PER_GRAPH);
    let start = Arc::new(Barrier::new(participants + 1));
    let mut handles = Vec::with_capacity(participants);

    for (_, _, graph) in &graphs {
        for _ in 0..WRITERS_PER_GRAPH {
            let graph = Arc::clone(graph);
            let start = Arc::clone(&start);
            handles.push(thread::spawn(move || -> Result<(), String> {
                start.wait();
                for _ in 0..WRITES_PER_WRITER {
                    graph.query(
                        "MATCH (n:Counter {id:0}) SET n.value=n.value+1 RETURN n.value",
                    )?;
                }
                Ok(())
            }));
        }

        for _ in 0..READERS_PER_GRAPH {
            let graph = Arc::clone(graph);
            let start = Arc::clone(&start);
            handles.push(thread::spawn(move || -> Result<(), String> {
                start.wait();
                for _ in 0..READS_PER_READER {
                    let out = graph.query_read_only(
                        "MATCH (n:Counter {id:0}) RETURN n.value",
                    )?;
                    if out.rows.len() != 1 || out.rows[0].len() != 1 {
                        return Err(format!("unexpected concurrent read result: {:?}", out.rows));
                    }
                }
                Ok(())
            }));
        }
    }

    start.wait();

    for handle in handles {
        handle
            .join()
            .map_err(|_| "concurrency worker panicked".to_string())??;
    }

    let expected = (WRITERS_PER_GRAPH * WRITES_PER_WRITER) as i64;
    for (name, _, graph) in &graphs {
        let out = graph.query_read_only(
            "MATCH (n:Counter {id:0}) RETURN n.value",
        )?;
        let got = match out
            .wire_rows
            .first()
            .and_then(|row| row.first())
        {
            Some(WireValue::Int(value)) => *value,
            other => return Err(format!(
                "{name}: final counter was not an integer WireValue: {other:?}"
            )),
        };
        if got != expected {
            return Err(format!("{name}: expected final value {expected}, got {got}"));
        }
    }

    // Drop all live graph handles and prove independent WAL recovery.
    drop(graphs);

    for i in 0..GRAPH_COUNT {
        let name = format!("concurrency-{i}");
        let wal = root.join(format!("{name}.wal"));
        let graph = NativeGraph::open_persistent(&name, &wal)?;
        let out = graph.query_read_only(
            "MATCH (n:Counter {id:0}) RETURN n.value",
        )?;
        let got = match out
            .wire_rows
            .first()
            .and_then(|row| row.first())
        {
            Some(WireValue::Int(value)) => *value,
            other => return Err(format!(
                "{name}: recovered counter was not an integer WireValue: {other:?}"
            )),
        };
        if got != expected {
            return Err(format!(
                "{name}: expected recovered value {expected}, got {got}"
            ));
        }
    }

    fs::remove_dir_all(&root)
        .map_err(|e| format!("remove concurrency smoke dir {}: {e}", root.display()))?;

    println!(
        "CONCURRENCY_SMOKE_PASS graphs={} writers_per_graph={} readers_per_graph={} writes_per_graph={}",
        GRAPH_COUNT,
        WRITERS_PER_GRAPH,
        READERS_PER_GRAPH,
        expected
    );
    Ok(())
}
