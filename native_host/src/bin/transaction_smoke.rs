//! All-or-nothing transactions (`NativeGraph::transaction`, `POST /v1/batch` with `"atomic": true`).
//!
//! Cases:
//! - a committed transaction: later statements see what earlier ones wrote, and every write is visible afterwards;
//! - a failing statement: nothing of the transaction is visible, and the graph accepts the next write;
//! - durability: a committed transaction survives reopening from the WAL, a rolled-back one leaves nothing there;
//! - a failure in the first, a middle and the last statement;
//! - index DDL inside a transaction is refused (and rolls back the rest);
//! - a read statement inside a transaction sees the transaction's own writes;
//! - an empty transaction.
use falkordb_native_host::{Engine, NativeGraph};
use std::fs;

fn count(g: &NativeGraph, cypher: &str) -> Result<String, String> {
    let out = g.query_read_only(cypher)?;
    let v = out.rows.first().and_then(|r| r.first()).cloned().unwrap_or_default();
    // rows are debug-formatted values ("Int(3)"): keep the inner value
    Ok(v.strip_prefix("Int(").and_then(|x| x.strip_suffix(')')).map(str::to_string).unwrap_or(v))
}

fn check(cond: bool, what: &str) -> Result<(), String> {
    if cond { Ok(()) } else { Err(format!("FAILED: {what}")) }
}

fn s(v: &[&str]) -> Vec<String> {
    v.iter().map(|x| (*x).to_string()).collect()
}

fn main() -> Result<(), String> {
    let _engine = Engine::init()?;
    let dir = std::env::temp_dir().join(format!("txn-smoke-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let wal = dir.join("g.wal");

    {
        let g = NativeGraph::open_persistent("g", &wal)?;

        // committed: statements see earlier statements' writes
        let out = g.transaction(&s(&[
            "CREATE (:Claim {cid:'s1'})",
            "MATCH (c:Claim {cid:'s1'}) CREATE (c)-[:HEAD]->(:Ver {vid:'s1.v1'})",
            "MATCH (c:Claim {cid:'s1'})-[:HEAD]->(v:Ver) RETURN v.vid",
        ])).map_err(|(i, e)| format!("committed transaction failed at {i}: {e}"))?;
        check(out.len() == 3, "three outputs")?;
        check(out[2].rows.first().is_some_and(|r| r[0].contains("s1.v1")), "a read inside sees the transaction's writes")?;
        let seen = count(&g, "MATCH (c:Claim)-[:HEAD]->(v:Ver) RETURN count(v)")?;
        let claims = count(&g, "MATCH (c:Claim) RETURN count(c)")?;
        check(seen == "1", &format!("committed writes visible (HEAD count {seen:?}, claims {claims:?})"))?;

        // a failure in the first, a middle and the last statement: nothing of the transaction is visible
        for bad_at in 0..3 {
            let mut stmts = s(&["CREATE (:Claim {cid:'x1'})", "CREATE (:Claim {cid:'x2'})", "CREATE (:Claim {cid:'x3'})"]);
            stmts[bad_at] = "MATCH (n) RETURN n.nope(".to_string();       // a syntax error
            let r = g.transaction(&stmts);
            check(matches!(r, Err((i, _)) if i == bad_at), &format!("failure reported at statement {bad_at}"))?;
            check(count(&g, "MATCH (c:Claim) WHERE c.cid STARTS WITH 'x' RETURN count(c)")? == "0",
                  &format!("nothing visible after a failure at statement {bad_at}"))?;
        }

        // a runtime error (not a parse error) after real writes
        let r = g.transaction(&s(&[
            "CREATE (:Claim {cid:'y1'})",
            "MATCH (c:Claim {cid:'y1'}) SET c.n = 1",
            "UNWIND [1, 0] AS d RETURN 1 / d",                               // division by zero at run time
        ]));
        check(r.is_err(), "a runtime error fails the transaction")?;
        check(count(&g, "MATCH (c:Claim {cid:'y1'}) RETURN count(c)")? == "0", "runtime error: nothing visible")?;

        // index DDL is refused inside a transaction
        let r = g.transaction(&s(&["CREATE (:Claim {cid:'z1'})", "CREATE INDEX FOR (n:Claim) ON (n.cid)"]));
        check(matches!(r, Err((1, ref e)) if e.contains("index DDL")), "index DDL refused")?;
        check(count(&g, "MATCH (c:Claim {cid:'z1'}) RETURN count(c)")? == "0", "DDL refusal rolls back")?;

        // G1 (adv_v21): a rolled-back transaction must leave the index as the committed graph is, whichever statement wrote it
        g.query("CREATE INDEX FOR (n:Idx) ON (n.k)")?;
        g.query("CREATE (:Idx {k:'keep'}), (:Idx {k:'old'}), (:Idx {k:'gone'})")?;
        std::thread::sleep(std::time::Duration::from_millis(300));      // let index population finish
        let r = g.transaction(&s(&[
            "CREATE (:Idx {k:'phantom'})",                               // a create, published by statement 0
            "MATCH (n:Idx {k:'old'}) SET n.k = 'renamed'",               // an update
            "MATCH (n:Idx {k:'gone'}) DELETE n",                         // a delete
            "UNWIND [1, 0] AS d RETURN 1 / d",                           // then a runtime failure
        ]));
        check(r.is_err(), "the indexed transaction fails")?;
        for (k, want) in [("keep", "1"), ("old", "1"), ("gone", "1"), ("phantom", "0"), ("renamed", "0")] {
            let got = count(&g, &format!("MATCH (n:Idx {{k:'{k}'}}) RETURN count(n)"))?;
            check(got == want, &format!("index lookup k={k}: want {want}, got {got} after the rollback"))?;
        }
        g.query("MERGE (:Idx {k:'old'})")?;                               // must find the live node, not create a duplicate
        check(count(&g, "MATCH (n:Idx) WHERE n.k = 'old' RETURN count(n)")? == "1", "MERGE after the rollback makes no duplicate")?;
        g.query("MATCH (n:Idx) DETACH DELETE n")?;
        g.query("DROP INDEX FOR (n:Idx) ON (n.k)")?;

        // snapshot reads: several reads of one committed version; a write inside is refused and changes nothing
        let snap = g.snapshot_read(&s(&["MATCH (c:Claim) RETURN count(c)", "MATCH (c:Claim)-[:HEAD]->(v:Ver) RETURN count(v)"]))
            .map_err(|(i, e)| format!("snapshot read failed at {i}: {e}"))?;
        check(snap.len() == 2 && snap[1].rows.first().is_some_and(|r| r[0].contains('1')), "snapshot read returns both results")?;
        let r = g.snapshot_read(&s(&["MATCH (c:Claim) RETURN count(c)", "CREATE (:Claim {cid:'w1'})"]));
        check(matches!(r, Err((1, ref e)) if e.contains("write")), "a write inside a snapshot read is refused")?;
        check(count(&g, "MATCH (c:Claim {cid:'w1'}) RETURN count(c)")? == "0", "the refused write wrote nothing")?;
        // isolation: a snapshot taken before a write keeps reading the old version while a writer commits on another thread
        {
            let g2 = &g;
            let before = count(g2, "MATCH (c:Claim) RETURN count(c)")?;
            std::thread::scope(|sc| -> Result<(), String> {
                let reader = sc.spawn(|| {
                    let qs: Vec<String> = (0..200).map(|_| "MATCH (n:Tmp) RETURN count(n)".to_string()).collect();
                    g2.snapshot_read(&qs).map(|outs| outs.iter().map(|o| o.rows[0][0].clone()).collect::<std::collections::BTreeSet<_>>())
                });
                for k in 0..20 {
                    g2.query(&format!("CREATE (:Tmp {{k:{k}}})"))?;
                }
                let seen = reader.join().map_err(|_| "reader panicked".to_string())?.map_err(|(i, e)| format!("snapshot {i}: {e}"))?;
                check(seen.len() == 1, &format!("one snapshot gives one answer across 200 reads while writes land: {seen:?} (before {before})"))
            })?;
            g.query("MATCH (n:Tmp) DELETE n")?;
        }

        // K1 (adv_v23): snapshot batches racing writes that escalate (an indexed label) must never deadlock
        {
            g.query("CREATE INDEX FOR (n:Race) ON (n.k)")?;
            let done = std::sync::atomic::AtomicBool::new(false);
            let g2 = &g;
            std::thread::scope(|sc| -> Result<(), String> {
                let watchdog = sc.spawn(|| {
                    let start = std::time::Instant::now();
                    while !done.load(std::sync::atomic::Ordering::SeqCst) {
                        if start.elapsed() > std::time::Duration::from_secs(300) {
                            eprintln!("FAILED: snapshot reads and writes deadlocked");
                            std::process::exit(2);
                        }
                        std::thread::sleep(std::time::Duration::from_millis(200));
                    }
                });
                let readers: Vec<_> = (0..3).map(|_| sc.spawn(|| -> Result<(), String> {
                    for _ in 0..150 {
                        g2.snapshot_read(&s(&["MATCH (n:Race) RETURN count(n)", "MATCH (n:Race {k:1}) RETURN count(n)"]))
                            .map_err(|(i, e)| format!("snapshot {i}: {e}"))?;
                    }
                    Ok(())
                })).collect();
                for k in 0..300 {
                    if k % 3 == 0 {
                        g2.transaction(&s(&[&format!("CREATE (:Race {{k:{k}}})"), &format!("MATCH (n:Race {{k:{k}}}) SET n.k2 = {k}")]))
                            .map_err(|(i, e)| format!("race transaction {i}: {e}"))?;
                    } else {
                        g2.query(&format!("CREATE (:Race {{k:{k}}})"))?;
                    }
                }
                for r in readers {
                    r.join().map_err(|_| "reader panicked".to_string())??;
                }
                done.store(true, std::sync::atomic::Ordering::SeqCst);
                watchdog.join().map_err(|_| "watchdog panicked".to_string())?;
                Ok(())
            })?;
            g.query("MATCH (n:Race) DELETE n")?;
            g.query("DROP INDEX FOR (n:Race) ON (n.k)")?;
        }

        // K2 (adv_v23): a statement flagged read-only inside a transaction can't write
        let r = g.transaction_flagged(&s(&["CREATE (:Claim {cid:'f1'})", "CREATE (:Claim {cid:'f2'})"]), &[false, true]);
        check(matches!(r, Err((1, ref e)) if e.contains("Read only")), "a flagged read-only statement that writes fails the transaction")?;
        check(count(&g, "MATCH (c:Claim) WHERE c.cid IN ['f1','f2'] RETURN count(c)")? == "0", "and nothing of it is written")?;

        // the graph still takes writes, transactional and plain
        g.transaction(&s(&["CREATE (:Claim {cid:'s2'})", "MATCH (c:Claim {cid:'s2'}) SET c.ok = true"]))
            .map_err(|(i, e)| format!("second transaction failed at {i}: {e}"))?;
        g.query("CREATE (:Claim {cid:'s3'})")?;
        check(g.transaction(&[]).is_ok(), "empty transaction")?;
        check(count(&g, "MATCH (c:Claim) RETURN count(c)")? == "3", "s1, s2, s3 after the failures")?;
    }

    // durability: reopen from the WAL
    {
        let g = NativeGraph::open_persistent("g", &wal)?;
        check(count(&g, "MATCH (c:Claim) RETURN count(c)")? == "3", "WAL replay: three claims")?;
        check(count(&g, "MATCH (c:Claim)-[:HEAD]->(v:Ver) RETURN count(v)")? == "1", "WAL replay: the HEAD edge")?;
        check(count(&g, "MATCH (c:Claim {cid:'s2'}) RETURN c.ok")?.contains("true"), "WAL replay: a property set in a later statement")?;
        check(count(&g, "MATCH (c:Claim) WHERE c.cid IN ['x1','x2','x3','y1','z1'] RETURN count(c)")? == "0",
              "WAL replay: rolled-back transactions left nothing")?;
    }
    let _ = fs::remove_dir_all(&dir);
    println!("transaction smoke: ok");
    Ok(())
}
