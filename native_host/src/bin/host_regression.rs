//! End-to-end regression tests for host durability and correctness fixes.
//!
//! Each case drives the public host API the way the server does and checks a
//! failure that used to leave a graph unusable or lose data:
//!
//! - a write whose effects buffer is empty no longer wedges the graph
//!   ("another MVCC write is in progress" until restart);
//! - a failed constraint creation no longer wedges the graph;
//! - `x > 5 AND x < 5` on a range index returns nothing instead of panicking;
//! - a crash between publishing a checkpoint and truncating the WAL no longer
//!   stops the graph (and server) from starting;
//! - one damaged graph no longer keeps every other graph offline;
//! - a failed JSONL import into a new graph is rolled back;
//! - RESTORE ... REPLACE keeps the old graph if the new payload fails, and
//!   an interrupted replace is rolled back on the next start.

use std::{fs, path::PathBuf, time::{SystemTime, UNIX_EPOCH}};

use falkordb_native_host::{Engine, NativeGraph, server::GraphCatalog};

fn scratch(label: &str) -> PathBuf {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir = std::env::temp_dir().join(format!(
        "falkordb-host-regression-{label}-{}-{nonce}",
        std::process::id()
    ));
    fs::create_dir_all(&dir).unwrap();
    dir
}

fn count(graph: &NativeGraph, q: &str) -> Result<String, String> {
    let out = graph.query(q)?;
    Ok(out.rows.first().and_then(|r| r.first()).cloned().unwrap_or_default())
}

fn check(name: &str, result: Result<(), String>, failures: &mut usize) {
    match result {
        Ok(()) => println!("ok   {name}"),
        Err(err) => {
            *failures += 1;
            println!("FAIL {name}: {err}");
        }
    }
}

fn empty_effects_write_does_not_wedge() -> Result<(), String> {
    let dir = scratch("wedge");
    let graph = NativeGraph::open_persistent("g", dir.join("g.wal"))?;
    graph.query("CREATE (:Real {v: 1})")?;
    // Registers a label name but changes no data: no effects are emitted.
    graph.query("OPTIONAL MATCH (n:Missing) SET n:Brand")?;
    graph
        .query("CREATE (:Real {v: 2})")
        .map_err(|e| format!("graph rejected the next write: {e}"))?;
    if count(&graph, "MATCH (n:Real) RETURN count(n)")? != "Int(2)" {
        return Err("expected 2 Real nodes".into());
    }
    drop(graph);
    // What was published must match what is durable.
    let reopened = NativeGraph::open_persistent("g", dir.join("g.wal"))?;
    let n = count(&reopened, "MATCH (n:Real) RETURN count(n)")?;
    if n != "Int(2)" {
        return Err(format!("after restart expected 2 Real nodes, got {n}"));
    }
    drop(reopened);
    let _ = fs::remove_dir_all(dir);
    Ok(())
}

fn failed_constraint_does_not_wedge() -> Result<(), String> {
    use graph::{entity_type::EntityType, graph::constraint::ConstraintType};
    let dir = scratch("constraint");
    let graph = NativeGraph::open_persistent("g", dir.join("g.wal"))?;
    graph.query("CREATE (:P {k: 1}), (:P {k: 1})")?;
    // Unique constraint without a supporting index / over duplicate values
    // must fail cleanly; either way the graph must stay writable.
    let _ = graph.mutate_constraint(
        true,
        ConstraintType::Unique,
        EntityType::Node,
        "P",
        &["k".to_string()],
    );
    graph
        .query("CREATE (:P {k: 2})")
        .map_err(|e| format!("graph rejected the next write: {e}"))?;
    drop(graph);
    let _ = fs::remove_dir_all(dir);
    Ok(())
}

fn equal_excluded_range_bounds_do_not_panic() -> Result<(), String> {
    let graph = NativeGraph::new("range");
    graph.query("UNWIND range(1, 10) AS i CREATE (:N {x: i, s: toString(i)})")?;
    graph.query("CREATE INDEX FOR (n:N) ON (n.x)")?;
    graph.query("CREATE INDEX FOR (n:N) ON (n.s)")?;
    for q in [
        "MATCH (n:N) WHERE n.x > 5 AND n.x < 5 RETURN count(n)",
        "MATCH (n:N) WHERE n.x >= 5 AND n.x < 5 RETURN count(n)",
        "MATCH (n:N) WHERE n.s > '5' AND n.s < '5' RETURN count(n)",
    ] {
        let got = count(&graph, q)?;
        if got != "Int(0)" {
            return Err(format!("{q}: expected 0, got {got}"));
        }
    }
    // Writes after a range query in the same graph still work.
    graph.query("MATCH (n:N) WHERE n.x > 5 AND n.x < 5 SET n.y = 1")?;
    graph.query("CREATE (:N {x: 11})")?;
    Ok(())
}

fn checkpoint_published_before_wal_truncation_still_opens() -> Result<(), String> {
    let dir = scratch("overlap");
    let wal_path = dir.join("g.wal");
    let saved_wal;
    {
        let graph = NativeGraph::open_persistent("g", &wal_path)?;
        graph.query("CREATE (:A {i: 1})")?;
        graph.checkpoint()?; // checkpoint 1; WAL compacted
        graph.query("CREATE (:A {i: 2})")?;
        graph.query("CREATE (:A {i: 3})")?;
        // The WAL as it is just before the next checkpoint truncates it.
        saved_wal = fs::read(&wal_path).map_err(|e| e.to_string())?;
        graph.checkpoint()?; // checkpoint 2 published, WAL compacted
    }
    // Recreate the crash window: checkpoint 2 is on disk, but the WAL still
    // holds the frames it already contains (the truncation never happened).
    fs::write(&wal_path, &saved_wal).map_err(|e| e.to_string())?;

    let graph = NativeGraph::open_persistent("g", &wal_path)
        .map_err(|e| format!("graph failed to open after the crash window: {e}"))?;
    let n = count(&graph, "MATCH (n:A) RETURN count(n)")?;
    if n != "Int(3)" {
        return Err(format!("expected 3 nodes (no double replay), got {n}"));
    }
    graph.query("CREATE (:A {i: 4})")?;
    drop(graph);
    let graph = NativeGraph::open_persistent("g", &wal_path)?;
    let n = count(&graph, "MATCH (n:A) RETURN count(n)")?;
    if n != "Int(4)" {
        return Err(format!("after another restart expected 4 nodes, got {n}"));
    }
    drop(graph);
    let _ = fs::remove_dir_all(dir);
    Ok(())
}

fn one_damaged_graph_does_not_block_the_server() -> Result<(), String> {
    let dir = scratch("damaged");
    {
        let catalog = GraphCatalog::open(&dir)?;
        catalog.get_or_create("good")?.query("CREATE (:X)")?;
        catalog.get_or_create("bad")?.query("CREATE (:Y)")?;
    }
    // Corrupt the middle of bad's WAL (not a torn tail: real corruption).
    // Every WAL frame carries its graph's name as the key.
    let bad_wal = fs::read_dir(dir.join("graphs"))
        .unwrap()
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|e| e == "wal"))
        .find(|p| {
            let bytes = fs::read(p).unwrap();
            bytes.windows(3).any(|w| w == b"bad") && !bytes.windows(4).any(|w| w == b"good")
        })
        .ok_or("bad graph WAL not found")?;
    let mut bytes = fs::read(&bad_wal).unwrap();
    let mid = bytes.len() / 2;
    bytes[mid] ^= 0xff;
    bytes.extend_from_slice(b"trailing-garbage-that-is-not-zero");
    fs::write(&bad_wal, &bytes).unwrap();

    let catalog = GraphCatalog::open(&dir)
        .map_err(|e| format!("server failed to start because of one damaged graph: {e}"))?;
    let good = catalog.get("good").ok_or("good graph was not loaded")?;
    if count(&good, "MATCH (n:X) RETURN count(n)")? != "Int(1)" {
        return Err("good graph lost data".into());
    }
    if catalog.failed_graphs().iter().all(|(name, _)| name != "bad") {
        return Err("damaged graph is not reported as offline".into());
    }
    let err = catalog
        .get_or_create("bad")
        .err()
        .ok_or("damaged graph was silently recreated")?;
    if !err.contains("failed to load") {
        return Err(format!("unexpected error for damaged graph: {err}"));
    }
    if fs::read(&bad_wal).unwrap() != bytes {
        return Err("damaged graph's files were modified".into());
    }
    drop(catalog);
    let _ = fs::remove_dir_all(dir);
    Ok(())
}

fn failed_import_into_new_graph_is_rolled_back() -> Result<(), String> {
    let dir = scratch("import");
    let catalog = GraphCatalog::open(&dir)?;
    let imports = dir.join("imports");
    fs::create_dir_all(&imports).unwrap();
    // Second record is not valid JSON, so the import fails after batch 1.
    fs::write(imports.join("rows.jsonl"), "{\"v\": 1}\n{not json}\n").unwrap();
    let result = catalog.bulk_import_jsonl_file(
        "fresh",
        "rows.jsonl",
        "UNWIND {{ROWS}} AS r CREATE (:R {v: r.v})",
        None,
        Some(1),
        None,
        None,
        None,
        false,
        true,
        false,
    );
    let err = match result {
        Ok(_) => return Err("import of invalid JSONL unexpectedly succeeded".into()),
        Err(err) => err,
    };
    if !err.contains("rolled back") {
        return Err(format!("rollback did not happen: {err}"));
    }
    if catalog.contains("fresh") {
        return Err("partially imported graph was left behind".into());
    }
    drop(catalog);
    let _ = fs::remove_dir_all(dir);
    Ok(())
}

fn replace_restore_keeps_old_graph_on_failure() -> Result<(), String> {
    let dir = scratch("replace");
    let catalog = GraphCatalog::open(&dir)?;
    let g = catalog.get_or_create("target")?;
    g.query("CREATE (:Old {v: 1})")?;
    drop(g);
    let source = catalog.get_or_create("source")?;
    source.query("CREATE (:New {v: 2})")?;
    drop(source);
    let good_dump = catalog.redis_dump("source")?;

    // A corrupt dump is rejected before anything is touched.
    let mut bad_dump = good_dump.clone();
    let mid = bad_dump.len() / 2;
    bad_dump[mid] ^= 0xff;
    if catalog.redis_restore("target", &bad_dump, true).is_ok() {
        return Err("corrupt dump was accepted".into());
    }
    let target = catalog.get("target").ok_or("target graph disappeared after a failed replace")?;
    if count(&target, "MATCH (n:Old) RETURN count(n)")? != "Int(1)" {
        return Err("old graph lost data after a failed replace".into());
    }
    drop(target);

    // A good replace swaps the contents, durably.
    catalog.redis_restore("target", &good_dump, true)?;
    let target = catalog.get("target").ok_or("target graph missing after replace")?;
    if count(&target, "MATCH (n:New) RETURN count(n)")? != "Int(1)"
        || count(&target, "MATCH (n:Old) RETURN count(n)")? != "Int(0)"
    {
        return Err("replace did not swap contents".into());
    }
    drop(target);
    drop(catalog);
    let catalog = GraphCatalog::open(&dir)?;
    let target = catalog.get("target").ok_or("replaced graph missing after restart")?;
    if count(&target, "MATCH (n:New) RETURN count(n)")? != "Int(1)" {
        return Err("replaced graph not durable".into());
    }
    drop(target);
    let leftovers: Vec<_> = fs::read_dir(dir.join("graphs"))
        .unwrap()
        .flatten()
        .filter(|e| e.file_name().to_string_lossy().starts_with(".replace-backup-"))
        .collect();
    if !leftovers.is_empty() {
        return Err("replace left a backup directory behind".into());
    }
    drop(catalog);
    let _ = fs::remove_dir_all(dir);
    Ok(())
}

fn interrupted_replace_is_rolled_back_on_start() -> Result<(), String> {
    let dir = scratch("replace-crash");
    let stem;
    {
        let catalog = GraphCatalog::open(&dir)?;
        catalog.get_or_create("target")?.query("CREATE (:Old {v: 1})")?;
        catalog.get("target").unwrap().checkpoint()?;
        stem = fs::read_dir(dir.join("graphs"))
            .unwrap()
            .flatten()
            .map(|e| e.path())
            .find(|p| p.extension().is_some_and(|e| e == "wal"))
            .unwrap()
            .file_stem()
            .unwrap()
            .to_string_lossy()
            .to_string();
    }
    // Recreate the on-disk state of a crash mid-replace: old files moved to
    // the backup directory, a partial (empty) new WAL in place, no marker.
    let graphs = dir.join("graphs");
    let backup = graphs.join(format!(".replace-backup-{stem}"));
    fs::create_dir(&backup).unwrap();
    for entry in fs::read_dir(&graphs).unwrap().flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        if name.starts_with(&stem) {
            fs::rename(entry.path(), backup.join(&name)).unwrap();
        }
    }
    fs::write(backup.join("MOVED"), b"").unwrap();
    fs::write(graphs.join(format!("{stem}.wal")), b"").unwrap();

    let catalog = GraphCatalog::open(&dir)?;
    let target = catalog.get("target").ok_or("graph missing after interrupted replace")?;
    let n = count(&target, "MATCH (n:Old) RETURN count(n)")?;
    if n != "Int(1)" {
        return Err(format!("previous graph was not restored (Old nodes: {n})"));
    }
    if backup.exists() {
        return Err("backup directory was not cleaned up".into());
    }
    drop(target);
    drop(catalog);
    let _ = fs::remove_dir_all(dir);
    Ok(())
}

fn readers_never_see_an_uncommitted_writes_index_entries() -> Result<(), String> {
    use std::sync::{Arc, atomic::{AtomicBool, AtomicUsize, Ordering}};
    let graph = Arc::new(NativeGraph::new("isolation"));
    graph.query("UNWIND range(1, 200) AS i CREATE (:N {x: 1})")?;
    graph.query("CREATE INDEX FOR (n:N) ON (n.x)")?;
    // Let the index population finish before racing.
    for _ in 0..200 {
        if count(&graph, "MATCH (n:N) WHERE n.x = 1 RETURN count(n)")? == "Int(200)" {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }

    let stop = Arc::new(AtomicBool::new(false));
    let leaks = Arc::new(AtomicUsize::new(0));
    let reads = Arc::new(AtomicUsize::new(0));
    let mut readers = Vec::new();
    for _ in 0..3 {
        let (graph, stop, leaks, reads) = (Arc::clone(&graph), Arc::clone(&stop), Arc::clone(&leaks), Arc::clone(&reads));
        readers.push(std::thread::spawn(move || {
            while !stop.load(Ordering::Relaxed) {
                // 999 is only ever written by queries that then fail.
                match graph.query_read_only("MATCH (n:N) WHERE n.x = 999 RETURN count(n)") {
                    Ok(out) if out.rows[0][0] == "Int(0)" => {}
                    Ok(_) | Err(_) => {
                        leaks.fetch_add(1, Ordering::Relaxed);
                    }
                }
                reads.fetch_add(1, Ordering::Relaxed);
            }
        }));
    }
    for _ in 0..150 {
        // CREATE commits its index documents, then the query fails.
        let result = graph.query(
            "UNWIND range(1, 50) AS i CREATE (:N {x: 999}) WITH count(*) AS c RETURN c / 0",
        );
        if result.is_ok() {
            stop.store(true, Ordering::Relaxed);
            return Err("division by zero unexpectedly succeeded".into());
        }
    }
    stop.store(true, Ordering::Relaxed);
    for r in readers {
        r.join().map_err(|_| "reader thread panicked")?;
    }
    let leaks = leaks.load(Ordering::Relaxed);
    if leaks > 0 {
        return Err(format!(
            "{leaks} of {} concurrent reads saw (or broke on) a failed write's index entries",
            reads.load(Ordering::Relaxed)
        ));
    }
    let n = count(&graph, "MATCH (n:N) WHERE n.x = 999 RETURN count(n)")?;
    if n != "Int(0)" {
        return Err(format!("failed writes left {n} index entries behind"));
    }
    graph.query("CREATE (:N {x: 2})")?;
    Ok(())
}

fn graph_stem(dir: &std::path::Path) -> String {
    fs::read_dir(dir.join("graphs"))
        .unwrap()
        .flatten()
        .map(|e| e.path())
        .find(|p| p.extension().is_some_and(|e| e == "wal"))
        .unwrap()
        .file_stem()
        .unwrap()
        .to_string_lossy()
        .to_string()
}

fn seeded(label: &str) -> Result<(PathBuf, String), String> {
    let dir = scratch(label);
    {
        let catalog = GraphCatalog::open(&dir)?;
        let g = catalog.get_or_create("target")?;
        g.query("CREATE (:Old {v: 1})")?;
        g.checkpoint()?;
        g.query("CREATE (:Old {v: 2})")?;
    }
    let stem = graph_stem(&dir);
    Ok((dir, stem))
}

/// A crash while the originals are being moved aside (only some moved, no
/// MOVED marker) must keep every original, not treat the unmoved ones as the
/// partial new graph and delete them.
fn replace_crash_mid_move_keeps_originals() -> Result<(), String> {
    let (dir, stem) = seeded("replace-mid-move")?;
    let graphs = dir.join("graphs");
    let backup = graphs.join(format!(".replace-backup-{stem}"));
    fs::create_dir(&backup).unwrap();
    // Move only the checkpoint; the WAL (holding v=2) stays in place.
    for entry in fs::read_dir(&graphs).unwrap().flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        if name.starts_with(&stem) && name.ends_with(".fgs") {
            fs::rename(entry.path(), backup.join(&name)).unwrap();
        }
    }
    let catalog = GraphCatalog::open(&dir)?;
    let target = catalog.get("target").ok_or("graph lost after crash mid-move")?;
    let n = count(&target, "MATCH (n:Old) RETURN count(n)")?;
    if n != "Int(2)" {
        return Err(format!("originals were not all kept (Old nodes: {n})"));
    }
    drop(target);
    drop(catalog);
    let _ = fs::remove_dir_all(dir);
    Ok(())
}

/// A crash part-way through DELETE finishes the delete on the next start
/// instead of leaving a WAL-less checkpoint or checkpoint-less WAL behind.
fn interrupted_delete_is_finished_on_start() -> Result<(), String> {
    let (dir, stem) = seeded("delete-crash")?;
    let graphs = dir.join("graphs");
    let tomb = graphs.join(format!(".delete-{stem}.wal"));
    fs::create_dir(&tomb).unwrap();
    for entry in fs::read_dir(&graphs).unwrap().flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        if name.starts_with(&stem) && name.ends_with(".fgs") {
            fs::rename(entry.path(), tomb.join(&name)).unwrap();
        }
    }
    let catalog = GraphCatalog::open(&dir)?;
    if catalog.get("target").is_some() {
        return Err("deleted graph came back after an interrupted delete".into());
    }
    let left: Vec<_> = fs::read_dir(&graphs)
        .unwrap()
        .flatten()
        .map(|e| e.file_name().to_string_lossy().to_string())
        .filter(|n| n.contains(&stem))
        .collect();
    if !left.is_empty() {
        return Err(format!("interrupted delete left files behind: {left:?}"));
    }
    // The name is usable again.
    catalog.get_or_create("target")?.query("CREATE (:New)")?;
    drop(catalog);
    let _ = fs::remove_dir_all(dir);
    Ok(())
}

/// A graph whose WAL is missing but whose checkpoint survived is still
/// found and opened (it used to be invisible yet block the name).
fn checkpoint_without_wal_is_discovered() -> Result<(), String> {
    let (dir, stem) = seeded("ckpt-only")?;
    {
        let catalog = GraphCatalog::open(&dir)?;
        catalog.get("target").unwrap().checkpoint()?;
    }
    fs::remove_file(dir.join("graphs").join(format!("{stem}.wal"))).unwrap();
    let catalog = GraphCatalog::open(&dir)?;
    let target = catalog.get("target").ok_or("checkpoint-only graph not discovered")?;
    let n = count(&target, "MATCH (n:Old) RETURN count(n)")?;
    if n != "Int(2)" {
        return Err(format!("checkpoint-only graph has {n} Old nodes, want 2"));
    }
    drop(target);
    drop(catalog);
    let _ = fs::remove_dir_all(dir);
    Ok(())
}

/// Dropping a label's last index and immediately recreating it must keep the
/// new index, both live and after WAL replay.
fn drop_then_create_index_survives_replay() -> Result<(), String> {
    let dir = scratch("index-replay");
    {
        let catalog = GraphCatalog::open(&dir)?;
        let g = catalog.get_or_create("g")?;
        g.query("UNWIND range(1, 50) AS i CREATE (:N {x: i})")?;
        for _ in 0..20 {
            g.query("CREATE INDEX FOR (n:N) ON (n.x)")?;
            g.query("DROP INDEX FOR (n:N) ON (n.x)")?;
        }
        g.query("CREATE INDEX FOR (n:N) ON (n.x)")?;
        g.query("CREATE (:N {x: 1000})")?;
    }
    let catalog = GraphCatalog::open(&dir)?;
    let g = catalog.get("g").ok_or("graph offline after index drop/create replay")?;
    let n = count(&g, "MATCH (n:N) WHERE n.x = 1000 RETURN count(n)")?;
    if n != "Int(1)" {
        return Err(format!("indexed lookup after replay returned {n}"));
    }
    let idx = g.query("CALL db.indexes() YIELD label RETURN count(label)")?;
    if idx.rows.first().and_then(|r| r.first()).map(String::as_str) != Some("Int(1)") {
        return Err(format!("index list after replay: {:?}", idx.rows));
    }
    drop(g);
    drop(catalog);
    let _ = fs::remove_dir_all(dir);
    Ok(())
}

/// DUMP / GRAPH.COPY serialize the committed graph under a read lock; the
/// matrix encoder used to unload the live matrices while doing so, so
/// concurrent readers saw empty matrices and the heap was corrupted.
fn dump_concurrent_with_readers_is_safe() -> Result<(), String> {
    use std::sync::{Arc, atomic::{AtomicBool, AtomicUsize, Ordering}};
    let graph = Arc::new(NativeGraph::new("dump-race"));
    graph.query("UNWIND range(1, 20000) AS i CREATE (:S {i: i})-[:R]->(:T)")?;
    let stop = Arc::new(AtomicBool::new(false));
    let bad = Arc::new(AtomicUsize::new(0));
    let mut readers = Vec::new();
    for _ in 0..3 {
        let (graph, stop, bad) = (Arc::clone(&graph), Arc::clone(&stop), Arc::clone(&bad));
        readers.push(std::thread::spawn(move || {
            while !stop.load(Ordering::Relaxed) {
                match graph.query_read_only("MATCH (s:S)-[:R]->(t:T) RETURN count(*)") {
                    Ok(out) if out.rows[0][0] == "Int(20000)" => {}
                    _ => {
                        bad.fetch_add(1, Ordering::Relaxed);
                    }
                }
            }
        }));
    }
    let mut sizes = std::collections::HashSet::new();
    for _ in 0..40 {
        sizes.insert(graph.export_falkordb_v19_payload().len());
    }
    stop.store(true, Ordering::Relaxed);
    for r in readers {
        r.join().map_err(|_| "reader thread panicked")?;
    }
    let bad = bad.load(Ordering::Relaxed);
    if bad > 0 {
        return Err(format!("{bad} reads returned wrong results while DUMP ran"));
    }
    if sizes.len() != 1 {
        return Err(format!("DUMP of an unchanged graph produced {} different sizes", sizes.len()));
    }
    Ok(())
}

/// A checkpoint that was still being staged when the process died must not
/// become a graph on the next start (it blocked RESTORE with BUSYKEY).
fn unpublished_checkpoint_is_not_a_graph() -> Result<(), String> {
    let dir = scratch("tmp-ckpt");
    let graphs = dir.join("graphs");
    fs::create_dir_all(&graphs).unwrap();
    let stale = graphs.join("6332.wal.snapshot.0.fgs.tmp.12345");
    fs::write(&stale, b"partial").unwrap();
    let catalog = GraphCatalog::open(&dir)?;
    if catalog.contains("c2") || catalog.get("c2").is_some() {
        return Err("a staged checkpoint was opened as graph c2".into());
    }
    if stale.exists() {
        return Err("stale staged checkpoint was left behind".into());
    }
    drop(catalog);
    let _ = fs::remove_dir_all(dir);
    Ok(())
}

/// DUMP must not carry stale process memory. GraphBLAS arrays can be larger
/// than their values and the slack is never initialized; it used to be
/// written verbatim, so dumps held fragments of earlier requests. Fill the
/// heap with a marker, then check that no dump of fresh graphs contains it.
fn dump_contains_no_stale_heap_bytes() -> Result<(), String> {
    const MARK: &[u8] = b"LEAKMRK";
    for round in 0..20 {
        {
            let junk: Vec<Vec<u8>> = (0..40_000)
                .map(|i| MARK.iter().copied().cycle().take(8 + (i % 120)).collect())
                .collect();
            drop(junk);
        }
        let graph = NativeGraph::new(&format!("leak-{round}"));
        graph.query("UNWIND range(1, 10) AS i CREATE (:D {i: i})-[:E {k: i}]->(:D {j: i})")?;
        let payload = graph.export_falkordb_v19_payload();
        if payload.windows(MARK.len()).any(|w| w == MARK) {
            return Err(format!("round {round}: DUMP payload contains stale heap bytes"));
        }
    }
    Ok(())
}

fn main() -> Result<(), String> {
    let _engine = Engine::init()?;
    let mut failures = 0;
    check("empty-effects write does not wedge the graph", empty_effects_write_does_not_wedge(), &mut failures);
    check("failed constraint does not wedge the graph", failed_constraint_does_not_wedge(), &mut failures);
    check("equal excluded range bounds return nothing", equal_excluded_range_bounds_do_not_panic(), &mut failures);
    check("checkpoint published before WAL truncation still opens", checkpoint_published_before_wal_truncation_still_opens(), &mut failures);
    check("one damaged graph does not block the server", one_damaged_graph_does_not_block_the_server(), &mut failures);
    check("failed import into a new graph is rolled back", failed_import_into_new_graph_is_rolled_back(), &mut failures);
    check("replace restore keeps the old graph on failure", replace_restore_keeps_old_graph_on_failure(), &mut failures);
    check("interrupted replace is rolled back on start", interrupted_replace_is_rolled_back_on_start(), &mut failures);
    check("replace crash mid-move keeps every original", replace_crash_mid_move_keeps_originals(), &mut failures);
    check("interrupted delete is finished on start", interrupted_delete_is_finished_on_start(), &mut failures);
    check("checkpoint without WAL is discovered", checkpoint_without_wal_is_discovered(), &mut failures);
    check("drop then create index survives replay", drop_then_create_index_survives_replay(), &mut failures);
    check("DUMP contains no stale heap bytes", dump_contains_no_stale_heap_bytes(), &mut failures);
    check("DUMP concurrent with readers is safe", dump_concurrent_with_readers_is_safe(), &mut failures);
    check("unpublished checkpoint is not a graph", unpublished_checkpoint_is_not_a_graph(), &mut failures);
    check("readers never see an uncommitted write's index entries", readers_never_see_an_uncommitted_writes_index_entries(), &mut failures);
    if failures > 0 {
        return Err(format!("{failures} host regression case(s) failed"));
    }
    println!("all host regression cases passed");
    Ok(())
}
