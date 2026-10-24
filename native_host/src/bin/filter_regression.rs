//! Regression test: a `WHERE` on one MATCH clause must survive when a later
//! MATCH clause traverses from a variable it bound.
//!
//! The optimizer's `select_scan_node` pass used to treat
//! `Filter(WHERE ...) -> NodeByLabelScan` under a traversal as a disposable
//! planner scan, prune it, and rebuild a bare scan — silently dropping the
//! predicate and matching every node with the label:
//!
//!     MATCH (e:Entity) WHERE e.name = 'x' MATCH (s)-[:R]->(e)   -- all `e`
//!
//! Every query here is checked against an equivalent that routes the bound
//! variable through `WITH`, which the optimizer cannot rewrite across, so the
//! oracle is independent of the bug. Run with and without the property index.

use falkordb_native_host::{Engine, NativeGraph};

struct Case {
    name: &'static str,
    query: &'static str,
    oracle: &'static str,
}

const CASES: &[Case] = &[
    Case {
        name: "single hop, bound node is `to`",
        query: "MATCH (e:E) WHERE e.name = 'e1' MATCH (s:S)-[:R]->(e) RETURN count(DISTINCT e), count(s)",
        oracle: "MATCH (e:E) WHERE e.name = 'e1' WITH e MATCH (s:S)-[:R]->(e) RETURN count(DISTINCT e), count(s)",
    },
    Case {
        name: "single hop, bound node is `from`",
        query: "MATCH (s:S) WHERE s.name = 's1' MATCH (s)-[:R]->(e:E) RETURN count(DISTINCT s), count(e)",
        oracle: "MATCH (s:S) WHERE s.name = 's1' WITH s MATCH (s)-[:R]->(e:E) RETURN count(DISTINCT s), count(e)",
    },
    Case {
        name: "unlabelled far endpoint",
        query: "MATCH (e:E) WHERE e.name = 'e1' MATCH (s)-[:R]->(e) RETURN count(DISTINCT e), count(s)",
        oracle: "MATCH (e:E) WHERE e.name = 'e1' WITH e MATCH (s)-[:R]->(e) RETURN count(DISTINCT e), count(s)",
    },
    Case {
        name: "undirected hop",
        query: "MATCH (e:E) WHERE e.name = 'e1' MATCH (e)-[r]-(x) RETURN count(DISTINCT e), count(r)",
        oracle: "MATCH (e:E) WHERE e.name = 'e1' WITH e MATCH (e)-[r]-(x) RETURN count(DISTINCT e), count(r)",
    },
    Case {
        name: "IN list predicate",
        query: "MATCH (e:E) WHERE e.name IN ['e1', 'e2'] MATCH (s:S)-[:R]->(e) RETURN count(DISTINCT e), count(s)",
        oracle: "MATCH (e:E) WHERE e.name IN ['e1', 'e2'] WITH e MATCH (s:S)-[:R]->(e) RETURN count(DISTINCT e), count(s)",
    },
    Case {
        name: "range predicate",
        query: "MATCH (e:E) WHERE e.v < 2 MATCH (s:S)-[:R]->(e) RETURN count(DISTINCT e), count(s)",
        oracle: "MATCH (e:E) WHERE e.v < 2 WITH e MATCH (s:S)-[:R]->(e) RETURN count(DISTINCT e), count(s)",
    },
    Case {
        name: "non-indexable predicate",
        query: "MATCH (e:E) WHERE toLower(e.name) = 'e1' MATCH (s:S)-[:R]->(e) RETURN count(DISTINCT e), count(s)",
        oracle: "MATCH (e:E) WHERE toLower(e.name) = 'e1' WITH e MATCH (s:S)-[:R]->(e) RETURN count(DISTINCT e), count(s)",
    },
    Case {
        name: "id() predicate",
        query: "MATCH (e:E) WHERE id(e) = 1 MATCH (s:S)-[:R]->(e) RETURN count(DISTINCT e), count(s)",
        oracle: "MATCH (e:E) WHERE id(e) = 1 WITH e MATCH (s:S)-[:R]->(e) RETURN count(DISTINCT e), count(s)",
    },
    Case {
        name: "inline attrs plus WHERE",
        query: "MATCH (e:E {kind: 'k'}) WHERE e.v = 1 MATCH (s:S)-[:R]->(e) RETURN count(DISTINCT e), count(s)",
        oracle: "MATCH (e:E {kind: 'k'}) WHERE e.v = 1 WITH e MATCH (s:S)-[:R]->(e) RETURN count(DISTINCT e), count(s)",
    },
    Case {
        name: "two-hop chain",
        query: "MATCH (e:E) WHERE e.name = 'e1' MATCH (e)<-[:R]-(s:S)-[:R]->(f:E) RETURN count(DISTINCT e), count(*)",
        oracle: "MATCH (e:E) WHERE e.name = 'e1' WITH e MATCH (e)<-[:R]-(s:S)-[:R]->(f:E) RETURN count(DISTINCT e), count(*)",
    },
    Case {
        name: "filter on far endpoint too",
        query: "MATCH (e:E) WHERE e.name = 'e1' MATCH (s:S)-[:R]->(e) WHERE s.name = 's1' RETURN count(DISTINCT e), count(s)",
        oracle: "MATCH (e:E) WHERE e.name = 'e1' WITH e MATCH (s:S)-[:R]->(e) WHERE s.name = 's1' RETURN count(DISTINCT e), count(s)",
    },
    Case {
        name: "variable-length hop",
        query: "MATCH (e:E) WHERE e.name = 'e1' MATCH (e)<-[:R*1..1]-(s) RETURN count(DISTINCT e), count(s)",
        oracle: "MATCH (e:E) WHERE e.name = 'e1' WITH e MATCH (e)<-[:R*1..1]-(s) RETURN count(DISTINCT e), count(s)",
    },
    Case {
        name: "variable-length hop from `from`",
        query: "MATCH (s:S) WHERE s.name = 's1' MATCH (s)-[:R*1..2]->(x) RETURN count(DISTINCT s), count(x)",
        oracle: "MATCH (s:S) WHERE s.name = 's1' WITH s MATCH (s)-[:R*1..2]->(x) RETURN count(DISTINCT s), count(x)",
    },
    Case {
        name: "three clauses",
        query: "MATCH (e:E) WHERE e.name = 'e1' MATCH (s:S)-[:R]->(e) MATCH (s)-[:R]->(f:E) RETURN count(DISTINCT e), count(*)",
        oracle: "MATCH (e:E) WHERE e.name = 'e1' WITH e MATCH (s:S)-[:R]->(e) WITH e, s MATCH (s)-[:R]->(f:E) RETURN count(DISTINCT e), count(*)",
    },
    Case {
        name: "optional match after WHERE",
        query: "MATCH (e:E) WHERE e.name = 'e1' OPTIONAL MATCH (s:S)-[:R]->(e) RETURN count(DISTINCT e), count(s)",
        oracle: "MATCH (e:E) WHERE e.name = 'e1' WITH e OPTIONAL MATCH (s:S)-[:R]->(e) RETURN count(DISTINCT e), count(s)",
    },
];

fn build_graph(graph: &NativeGraph) -> Result<(), String> {
    // 20 E nodes, 30 S nodes; each S points at 3 E nodes, and S nodes chain.
    graph.query(
        "UNWIND range(0, 19) AS i CREATE (:E {name: 'e' + toString(i), v: i % 5, kind: CASE WHEN i % 2 = 0 THEN 'k' ELSE 'j' END})",
    )?;
    graph.query("UNWIND range(0, 29) AS i CREATE (:S {name: 's' + toString(i)})")?;
    graph.query(
        "MATCH (s:S), (e:E) WITH s, e, toInteger(substring(s.name, 1)) AS si, toInteger(substring(e.name, 1)) AS ei \
         WHERE ei = si % 20 OR ei = (si * 7) % 20 OR ei = (si + 3) % 20 CREATE (s)-[:R]->(e)",
    )?;
    graph.query(
        "MATCH (a:S), (b:S) WHERE toInteger(substring(b.name, 1)) = toInteger(substring(a.name, 1)) + 1 CREATE (a)-[:R]->(b)",
    )?;
    Ok(())
}

fn rows(graph: &NativeGraph, q: &str) -> Result<Vec<Vec<String>>, String> {
    Ok(graph.query(q).map_err(|e| format!("{q}: {e}"))?.rows)
}

fn run_cases(graph: &NativeGraph, label: &str) -> Result<usize, String> {
    let mut failures = 0;
    for case in CASES {
        let got = rows(graph, case.query)?;
        let want = rows(graph, case.oracle)?;
        if got == want {
            println!("ok   [{label}] {}: {:?}", case.name, got);
        } else {
            failures += 1;
            println!("FAIL [{label}] {}: got {:?}, want {:?}", case.name, got, want);
        }
    }
    Ok(failures)
}

/// The same planner shape in a write query must touch only the filtered rows.
fn run_write_case(label: &str) -> Result<usize, String> {
    let graph = NativeGraph::new("filter-regression-write");
    build_graph(&graph)?;
    if label == "indexed" {
        graph.query("CREATE INDEX FOR (e:E) ON (e.name)")?;
    }
    let out = graph.query("MATCH (e:E) WHERE e.name = 'e1' MATCH (s:S)-[:R]->(e) SET s.touched = true")?;
    let touched = rows(&graph, "MATCH (s:S) WHERE s.touched RETURN count(s)")?;
    let want = rows(
        &graph,
        "MATCH (e:E) WHERE e.name = 'e1' WITH e MATCH (s:S)-[:R]->(e) RETURN count(DISTINCT s)",
    )?;
    if touched == want {
        println!("ok   [{label}] write touches only filtered rows: {touched:?} ({} props set)", out.stats.properties_set);
        Ok(0)
    } else {
        println!("FAIL [{label}] write touched {touched:?}, want {want:?}");
        Ok(1)
    }
}

/// openCypher row multiplicity on a multigraph, checked against hand-derived
/// counts (both FalkorDB C and earlier builds collapsed parallel edges or
/// merged consecutive MATCH clauses into one uniqueness scope).
///
/// Graph: (a:A)-[:X]->(b:B) twice, (a)-[:Y]->(b) once, and a self-loop
/// (b)-[:L]->(b).
const SEMANTICS: &[(&str, &str)] = &[
    ("MATCH (a:A)-->(b:B) RETURN count(*)", "3"),
    ("MATCH (a:A)-[r]->(b:B) RETURN count(*)", "3"),
    ("MATCH (a:A)-[:X]->(b) RETURN count(*)", "2"),
    ("MATCH (a:A)-[:X|Y]->(b) RETURN count(*)", "3"),
    ("MATCH (a:A)--(b:B) RETURN count(*)", "3"),
    ("MATCH (b:B)--(a:A) RETURN count(*)", "3"),
    ("MATCH (x)--(y) RETURN count(*)", "7"),
    ("MATCH (b:B)-->(b) RETURN count(*)", "1"),
    ("MATCH (b:B)--(b) RETURN count(*)", "1"),
    ("MATCH (a:A)-->(b:B)<--(a) RETURN count(*)", "6"),
    ("MATCH (a:A)-[r]->(b:B)<-[s]-(a) RETURN count(*)", "6"),
    ("MATCH (a:A)-[r]->(b:B) MATCH (a)-[s]->(b) RETURN count(*)", "9"),
    ("MATCH (a:A)-[r]->(b:B) MATCH (a)-[s:Y]->(b) RETURN count(*)", "3"),
    ("MATCH (a:A)-->(b:B) MATCH (a)-->(b) RETURN count(*)", "9"),
    ("MATCH (a:A)-[r]->(b:B), (a)-[s]->(b) RETURN count(*)", "6"),
    ("MATCH (a:A)-->()-->(c) RETURN count(*)", "3"),
    ("MATCH (a:A)-->(b) RETURN count(DISTINCT b)", "1"),
    ("MATCH (n) WHERE NOT (n:B) MATCH (n)-->(m) WHERE m.k = 1 RETURN count(*)", "3"),
    ("OPTIONAL MATCH (z:Missing) WITH * MATCH (a:A) RETURN count(*)", "1"),
    ("MATCH (b:B) MATCH (a:A)-[:Y]->(c) RETURN count(b)", "1"),
    ("OPTIONAL MATCH (z:Missing) WITH z MATCH (z)-[:X {w: 1}]->(c) RETURN count(*)", "0"),
    ("MATCH (n)<-[:X {w: 1}]-(m:A) RETURN count(*)", "1"),
    ("MATCH (a:A)-[r:X]->(b:B) WHERE r.w >= 1 RETURN count(DISTINCT a)", "1"),
    ("MATCH (n)-[:X {w: 2}]->(m) RETURN count(*)", "1"),
    ("MATCH (x) MATCH (n:B)-[:X {w: 2}]-(x) RETURN count(*)", "1"),
    ("MATCH (a:A) OPTIONAL MATCH (z:Missing) WITH * MATCH (c)<-[r:X]-(z) WHERE r.w = 1 RETURN count(*)", "0"),
    ("MATCH (a)<-[r:X]-(b) WHERE r.w = a.k RETURN count(*)", "1"),
    ("MATCH (x:A) MATCH (a)<-[r:X]-(b) WHERE r.w = a.k RETURN count(*)", "1"),
    ("MATCH (n1)<-[:X*0..1]-(n1)-[:Y]->(n4:B)-[:L]-(n6:B) RETURN count(*)", "1"),
    ("MATCH (n1)<-[:X*0..1]-(n1)-[:Y*1..2]->(n4:B)-[]-(n6:B) WHERE n6 = n4 RETURN count(*)", "1"),
    ("MATCH (b:B) WITH b MATCH (a:A)-[:X {w: 1}]->(c) RETURN count(*)", "1"),
    ("MATCH (q:Missing) WITH q MATCH (a:A)-[:X {w: 1}]->(c) RETURN count(*)", "0"),
    ("MATCH (x) MATCH (a:A)-[:X {w: 1}]->(c) RETURN count(*)", "2"),
    ("MATCH (x) WITH x MATCH (a:B)-[:L {w: 4}]->(c) RETURN count(*)", "2"),
    ("MATCH (x) WITH x MATCH (a:A)-[:L {w: 4}]->(c) RETURN count(*)", "0"),
    ("MATCH (x) MATCH (a:A)-[r:X]->(c:B) WHERE r.w = 2 RETURN count(*)", "2"),
    ("MATCH (x) MATCH (n:B) WHERE n.k = 1 RETURN count(*)", "2"),
    ("MATCH (q:Missing) MATCH (n:B) WHERE n.k = 1 RETURN count(*)", "0"),
    ("MATCH (x) WITH x MATCH (n:B) WHERE n.k = 1 RETURN count(*)", "2"),
    ("UNWIND [1, 2] AS x CALL { UNWIND [1, 1, 2] AS y RETURN count(DISTINCT y) AS c } RETURN toInteger(sum(c))", "4"),
    ("MATCH (b:B) UNWIND range(1, 3000) AS i OPTIONAL MATCH (b)-[:Q]->(z), (w:A) RETURN count(DISTINCT i)", "3000"),
    ("MATCH (b:B) UNWIND range(1, 3000) AS i OPTIONAL MATCH (b)-[:L]->(z), (w:A) RETURN count(DISTINCT i % 7)", "7"),
    ("MATCH (a:A)-[r]->(b) WITH DISTINCT a, r WHERE r IS NOT NULL MATCH (b:B) RETURN count(*)", "3"),
    ("MATCH (a:A)-[r]->(b) WITH DISTINCT a, r WHERE type(r) <> 'Q' RETURN count(*)", "3"),
    ("MATCH (b:B) WHERE b.k = 1 MATCH (x:A), (b)-[:L]->(c) RETURN count(*)", "1"),
    ("MATCH (b:B) MATCH (x:A), (b)-[:L]->(c) RETURN count(*)", "1"),
    ("MATCH (b:B) MATCH (b)-[:L]->(c), (x:A) RETURN count(*)", "1"),
    ("MATCH (a:A)-[:X]->(b), (c:A)-[:X]->(d) RETURN count(*)", "2"),
    ("MATCH (a:A)-[r:X]->(b)<-[*1..1]-(a) RETURN count(*)", "4"),
    ("MATCH (a:A)-[r]->(b)<-[*1..1]-(a) RETURN count(*)", "6"),
    ("MATCH (a:A)-[*1..1]->(b)<-[*1..1]-(a) RETURN count(*)", "6"),
    ("MATCH (b:B)-[:L]->(b), (x)-[:L*1..2]->(y) RETURN count(*)", "0"),
    ("MATCH (b:B)-[:L*1..3]->(b) RETURN count(*)", "1"),
    ("MATCH (b:B)-[:L]->(b)-[:L*0..2]->(b) RETURN count(*)", "1"),
    ("MATCH (q:Missing) WITH * MATCH (x:B)<-[:X*1..2]-(y) RETURN count(*)", "0"),
    ("MATCH (q:Missing) MATCH (x:B)<-[:X*1..2]-(y) RETURN count(*)", "0"),
    ("MATCH (q:Missing) WITH * MATCH (x:A)-[:X]->(y:B) RETURN count(*)", "0"),
    ("MATCH (q:Missing) WITH * MATCH (y:B)<-[:X]-(x) RETURN count(*)", "0"),
    ("MATCH (b:B)-[:L]->(b) WITH * MATCH (x:B)<-[:X*1..1]-(y) RETURN count(*)", "2"),
    ("MATCH (b:B)<--(b) MATCH (x:B)<-[:X*1..1]-(y) RETURN count(*)", "2"),
    ("MATCH (x) MATCH (a:A)-[r]->(c) RETURN count(x)", "6"),
    ("MATCH (x) MATCH (y) MATCH (a:A)-[r:X]->(c) RETURN count(DISTINCT x) + count(y)", "10"),
    ("MATCH (a:A) OPTIONAL MATCH (a)-[:Z]->(q) WITH * MATCH (a)-[:Y]->(b) RETURN count(*)", "1"),
    ("MATCH (a:A) OPTIONAL MATCH (a)-[:Z]->(q) WITH * MATCH (a)-[:Y]->(b) WHERE a IS NOT NULL RETURN count(*)", "1"),
    ("OPTIONAL MATCH (z:Missing) WITH z MATCH (z) RETURN count(*)", "0"),
    ("OPTIONAL MATCH (z:Missing) WITH z MATCH (z)-->(y) RETURN count(*)", "0"),
    ("OPTIONAL MATCH (z:Missing) WITH z MATCH (a:A)-[:Y]->(b), (z) RETURN count(*)", "0"),
];

fn run_semantics(indexed: bool) -> Result<usize, String> {
    let graph = NativeGraph::new("filter-regression-semantics");
    graph.query("CREATE (a:A), (b:B {k: 1}), (a)-[:X {w: 1}]->(b), (a)-[:X {w: 2}]->(b), (a)-[:Y {w: 3}]->(b), (b)-[:L {w: 4}]->(b)")?;
    let label = if indexed { "indexed" } else { "semantics" };
    if indexed {
        graph.query("CREATE INDEX FOR (n:B) ON (n.k)")?;
        graph.query("CREATE INDEX FOR ()-[r:X]-() ON (r.w)")?;
        graph.query("CREATE INDEX FOR ()-[r:L]-() ON (r.w)")?;
        // Index population is asynchronous.
        for _ in 0..500 {
            let ready = rows(&graph, "MATCH (n:B) WHERE n.k = 1 RETURN count(n)")? == vec![vec!["Int(1)".to_string()]]
                && rows(&graph, "MATCH ()-[r:X]->() WHERE r.w = 1 RETURN count(r)")? == vec![vec!["Int(1)".to_string()]]
                && rows(&graph, "MATCH ()-[r:L]->() WHERE r.w = 4 RETURN count(r)")? == vec![vec!["Int(1)".to_string()]];
            if ready {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
    }
    let mut failures = 0;
    // A MATCH straight after OPTIONAL MATCH is refused (it used to be folded
    // into the optional pattern and return null-padded rows).
    match graph.query("MATCH (a:A) OPTIONAL MATCH (a)-[:Z]->(q) MATCH (a)-[:Y]->(b) RETURN count(*)") {
        Err(e) if e.contains("WITH clause is required") => println!("ok   [semantics] MATCH after OPTIONAL MATCH is refused"),
        other => {
            failures += 1;
            println!("FAIL [semantics] MATCH after OPTIONAL MATCH: {:?}", other.map(|o| o.rows));
        }
    }
    for (q, want) in SEMANTICS {
        let got = rows(&graph, q)?;
        if got == vec![vec![format!("Int({want})")]] {
            println!("ok   [{label}] {q}: {want}");
        } else {
            failures += 1;
            println!("FAIL [{label}] {q}: got {got:?}, want {want}");
        }
    }
    Ok(failures)
}

fn main() -> Result<(), String> {
    let _engine = Engine::init()?;
    let mut failures = run_semantics(false)?;
    failures += run_semantics(true)?;

    let plain = NativeGraph::new("filter-regression");
    build_graph(&plain)?;
    failures += run_cases(&plain, "no index")?;
    failures += run_write_case("no index")?;

    let indexed = NativeGraph::new("filter-regression-indexed");
    build_graph(&indexed)?;
    indexed.query("CREATE INDEX FOR (e:E) ON (e.name)")?;
    indexed.query("CREATE INDEX FOR (s:S) ON (s.name)")?;
    failures += run_cases(&indexed, "indexed")?;
    failures += run_write_case("indexed")?;

    if failures > 0 {
        return Err(format!("{failures} filter regression case(s) failed"));
    }
    println!("all filter regression cases passed");
    Ok(())
}
