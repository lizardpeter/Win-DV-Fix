use falkordb_native_host::{Engine, NativeGraph};
use std::path::PathBuf;

fn must_query(graph: &NativeGraph, q: &str) -> Result<falkordb_native_host::QueryOutput, String> {
    graph.query(q).map_err(|e| format!("query failed: {q}\n{e}"))
}

fn joined_rows(out: &falkordb_native_host::QueryOutput) -> String {
    out.rows.iter().flatten().cloned().collect::<Vec<_>>().join(" | ")
}

fn require_contains(
    out: &falkordb_native_host::QueryOutput,
    needle: &str,
    ctx: &str,
) -> Result<(), String> {
    let text = joined_rows(out);
    if !text.contains(needle) {
        return Err(format!("{ctx}: expected {needle:?} in {text:?}"));
    }
    Ok(())
}

fn main() -> Result<(), String> {
    let _engine = Engine::init()?;

    // Create every index before data so inserts exercise FalkorDB's normal
    // commit_index / commit_edge_index path without background-population timing.
    let graph = NativeGraph::new("native-index-integration");
    must_query(&graph, "CREATE INDEX FOR (n:Idx) ON (n.age, n.name)")?;
    must_query(&graph, "CREATE FULLTEXT INDEX FOR (n:Idx) ON (n.text)")?;
    must_query(
        &graph,
        "CREATE VECTOR INDEX FOR (n:Idx) ON (n.emb) OPTIONS {dimension:2, similarityFunction:'euclidean', M:16, efConstruction:200, efRuntime:10}",
    )?;
    must_query(&graph, "CREATE INDEX FOR ()-[r:EIdx]-() ON (r.weight)")?;
    must_query(&graph, "CREATE FULLTEXT INDEX FOR ()-[r:EIdx]-() ON (r.text)")?;
    must_query(
        &graph,
        "CREATE VECTOR INDEX FOR ()-[r:EIdx]-() ON (r.emb) OPTIONS {dimension:2, similarityFunction:'euclidean', M:16, efConstruction:200, efRuntime:10}",
    )?;

    must_query(
        &graph,
        "CREATE (a:Idx {name:'alpha',age:10,text:'hello alpha',emb:vecf32([0.0,0.0])}), (b:Idx {name:'beta',age:20,text:'hello beta',emb:vecf32([10.0,10.0])}), (c:Idx {name:'gamma',age:30,text:'goodbye gamma',emb:vecf32([20.0,20.0])}), (a)-[:EIdx {weight:7,text:'fast link',emb:vecf32([0.0,0.0])}]->(b), (b)-[:EIdx {weight:17,text:'slow link',emb:vecf32([10.0,10.0])}]->(c)",
    )?;

    let out = must_query(
        &graph,
        "MATCH (n:Idx) WHERE n.age >= 15 AND n.age <= 25 RETURN n.name",
    )?;
    if out.rows.len() != 1 {
        return Err(format!("range index expected 1 row, got {:?}", out.rows));
    }
    require_contains(&out, "beta", "range index")?;

    let out = must_query(&graph, "MATCH (n:Idx) WHERE n.name = 'gamma' RETURN n.age")?;
    if out.rows.len() != 1 {
        return Err(format!("string range index expected 1 row, got {:?}", out.rows));
    }
    require_contains(&out, "30", "string equality index")?;

    let out = must_query(
        &graph,
        "CALL db.idx.fulltext.queryNodes('Idx','hello') YIELD node, score RETURN node.name, score ORDER BY node.name",
    )?;
    if out.rows.len() != 2 {
        return Err(format!("fulltext node expected 2 rows, got {:?}", out.rows));
    }
    require_contains(&out, "alpha", "fulltext node")?;
    require_contains(&out, "beta", "fulltext node")?;

    let out = must_query(
        &graph,
        "CALL db.idx.vector.queryNodes('Idx','emb',1,vecf32([0.1,0.1])) YIELD node, score RETURN node.name, score",
    )?;
    if out.rows.len() != 1 {
        return Err(format!("vector node expected 1 row, got {:?}", out.rows));
    }
    require_contains(&out, "alpha", "vector node")?;

    let out = must_query(
        &graph,
        "MATCH ()-[r:EIdx]-() WHERE r.weight >= 10 RETURN r.text",
    )?;
    if out.rows.len() != 1 {
        return Err(format!("edge range expected 1 row, got {:?}", out.rows));
    }
    require_contains(&out, "slow link", "edge range")?;

    let out = must_query(
        &graph,
        "CALL db.idx.fulltext.queryRelationships('EIdx','fast') YIELD relationship, score RETURN relationship.text, score",
    )?;
    if out.rows.len() != 1 {
        return Err(format!("edge fulltext expected 1 row, got {:?}", out.rows));
    }
    require_contains(&out, "fast link", "edge fulltext")?;

    let out = must_query(
        &graph,
        "CALL db.idx.vector.queryRelationships('EIdx','emb',1,vecf32([9.9,9.9])) YIELD relationship, score RETURN relationship.text, score",
    )?;
    if out.rows.len() != 1 {
        return Err(format!("edge vector expected 1 row, got {:?}", out.rows));
    }
    require_contains(&out, "slow link", "edge vector")?;

    // CRUD parity on indexed entities.
    must_query(
        &graph,
        "MATCH (n:Idx {name:'beta'}) SET n.age=25, n.text='updated beta', n.emb=vecf32([1.0,1.0])",
    )?;
    let out = must_query(
        &graph,
        "CALL db.idx.fulltext.queryNodes('Idx','updated') YIELD node RETURN node.name",
    )?;
    require_contains(&out, "beta", "fulltext update")?;
    let out = must_query(
        &graph,
        "CALL db.idx.vector.queryNodes('Idx','emb',1,vecf32([1.1,1.1])) YIELD node RETURN node.name",
    )?;
    require_contains(&out, "beta", "vector update")?;

    must_query(
        &graph,
        "MATCH ()-[r:EIdx {weight:7}]-() SET r.text='updated fast link', r.weight=8",
    )?;
    let out = must_query(
        &graph,
        "CALL db.idx.fulltext.queryRelationships('EIdx','updated') YIELD relationship RETURN relationship.weight",
    )?;
    require_contains(&out, "8", "edge fulltext update")?;

    must_query(&graph, "MATCH (n:Idx {name:'gamma'}) DETACH DELETE n")?;
    let out = must_query(&graph, "MATCH (n:Idx) WHERE n.name='gamma' RETURN n")?;
    if !out.rows.is_empty() {
        return Err(format!("deleted indexed node still returned: {:?}", out.rows));
    }

    println!("NATIVE_CYPHER_INDEX_INTEGRATION_PASS");

    // WAL + index DDL/data replay.
    let wal_path: PathBuf = std::env::temp_dir().join(format!(
        "falkordb-native-index-restart-{}.wal",
        std::process::id()
    ));
    let _ = std::fs::remove_file(&wal_path);
    {
        let persistent = NativeGraph::open_persistent("native-persistent-index", &wal_path)?;
        must_query(
            &persistent,
            "CREATE INDEX FOR (n:PersistIdx) ON (n.address)",
        )?;
        must_query(
            &persistent,
            "CREATE FULLTEXT INDEX FOR (n:PersistIdx) ON (n.text)",
        )?;
        must_query(
            &persistent,
            "CREATE (:PersistIdx {address:4096,text:'durable indexed text'})",
        )?;
    }
    {
        let recovered = NativeGraph::open_persistent("native-persistent-index", &wal_path)?;
        let out = must_query(
            &recovered,
            "MATCH (n:PersistIdx) WHERE n.address=4096 RETURN n.text",
        )?;
        require_contains(&out, "durable indexed text", "WAL range-index recovery")?;
        let out = must_query(
            &recovered,
            "CALL db.idx.fulltext.queryNodes('PersistIdx','durable') YIELD node RETURN node.address",
        )?;
        require_contains(&out, "4096", "WAL fulltext-index recovery")?;
    }
    let _ = std::fs::remove_file(&wal_path);

    // Regression: data may predate index DDL. WAL replay sees the entity
    // records before CREATE INDEX, so recovery must synchronously backfill
    // indexes after the full effects stream has been reconstructed.
    let late_index_wal: PathBuf = std::env::temp_dir().join(format!(
        "falkordb-native-late-index-restart-{}.wal",
        std::process::id()
    ));
    let _ = std::fs::remove_file(&late_index_wal);
    {
        let persistent = NativeGraph::open_persistent("late-index-restart", &late_index_wal)?;
        must_query(
            &persistent,
            "CREATE (:LateIdx {age:40,name:'Alice'}), (:LateIdx {age:30,name:'Bob'})",
        )?;
        must_query(&persistent, "CREATE INDEX FOR (n:LateIdx) ON (n.age)")?;
        must_query(&persistent, "CREATE FULLTEXT INDEX FOR (n:LateIdx) ON (n.name)")?;
    }
    {
        let recovered = NativeGraph::open_persistent("late-index-restart", &late_index_wal)?;
        let out = must_query(
            &recovered,
            "MATCH (n:LateIdx) WHERE n.age >= 35 RETURN n.name",
        )?;
        require_contains(&out, "Alice", "late-created range index recovery")?;
        let out = must_query(
            &recovered,
            "CALL db.idx.fulltext.queryNodes('LateIdx','Alice') YIELD node RETURN node.age",
        )?;
        require_contains(&out, "40", "late-created fulltext index recovery")?;
    }
    let _ = std::fs::remove_file(&late_index_wal);

    println!("NATIVE_LATE_INDEX_RESTART_PASS");
    println!("NATIVE_WAL_INDEX_RESTART_PASS");
    Ok(())
}
