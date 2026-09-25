use falkordb_native_host::{Engine, NativeGraph, property_index::NativePropertyIndex, range_index::{NativeNumericRangeIndex, NativeStringRangeIndex}};
use graph::{index::IndexQuery, runtime::value::{Point, Value}};
use std::sync::Arc;

fn require_contains(haystack: &str, needle: &str, what: &str) -> Result<(), String> {
    if haystack.contains(needle) {
        Ok(())
    } else {
        Err(format!("{what}: expected {needle:?} in {haystack:?}"))
    }
}

fn main() -> Result<(), String> {
    let _engine = Engine::init()?;
    let graph = NativeGraph::new("native-smoke");

    let ret = graph.query("RETURN 1 AS value")?;
    if ret.rows.len() != 1 || ret.rows[0].len() != 1 {
        return Err(format!("RETURN smoke shape mismatch: {:?}", ret.rows));
    }
    require_contains(&ret.rows[0][0], "1", "RETURN smoke")?;

    let created = graph.query("CREATE (:Test {x: 123, name: 'windows-native'})")?;
    if created.stats.nodes_created != 1 {
        return Err(format!(
            "CREATE smoke expected 1 node, got {:?}",
            created.stats
        ));
    }

    let matched = graph.query("MATCH (n:Test) RETURN n.x AS x, n.name AS name")?;
    if matched.rows.len() != 1 || matched.rows[0].len() != 2 {
        return Err(format!("MATCH smoke shape mismatch: {:?}", matched.rows));
    }
    require_contains(&matched.rows[0][0], "123", "MATCH integer")?;
    require_contains(&matched.rows[0][1], "windows-native", "MATCH string")?;

    let rel = graph.query(
        "CREATE (:A {id: 1})-[:LINKS_TO {weight: 7}]->(:B {id: 2})"
    )?;
    if rel.stats.nodes_created != 2 || rel.stats.relationships_created != 1 {
        return Err(format!("relationship CREATE smoke mismatch: {:?}", rel.stats));
    }

    let traversed = graph.query(
        "MATCH (a:A)-[r:LINKS_TO]->(b:B) RETURN a.id AS a, r.weight AS w, b.id AS b"
    )?;
    if traversed.rows.len() != 1 || traversed.rows[0].len() != 3 {
        return Err(format!("relationship MATCH smoke shape mismatch: {:?}", traversed.rows));
    }
    require_contains(&traversed.rows[0][0], "1", "relationship source")?;
    require_contains(&traversed.rows[0][1], "7", "relationship property")?;
    require_contains(&traversed.rows[0][2], "2", "relationship destination")?;

    let mut native_idx = NativeNumericRangeIndex::new();
    native_idx.upsert(101, [10.0])?;
    native_idx.upsert(102, [20.0, 25.0])?;
    native_idx.upsert(103, [-5.0])?;

    let eq: Vec<u64> = native_idx.equal(20.0)?.collect();
    if eq != vec![102] {
        return Err(format!("native range equality mismatch: {eq:?}"));
    }

    let ranged: Vec<u64> = native_idx
        .range(Some(0.0), Some(25.0), true, true)?
        .collect();
    if ranged != vec![101, 102, 102] {
        return Err(format!("native range scan mismatch: {ranged:?}"));
    }

    let snapshot = native_idx.range(None, None, true, true)?;
    native_idx.upsert(104, [30.0])?;
    native_idx.remove_document(103);
    let snapshot_docs: Vec<u64> = snapshot.collect();
    if snapshot_docs != vec![103, 101, 102, 102] {
        return Err(format!("native range snapshot mismatch: {snapshot_docs:?}"));
    }

    let mut string_idx = NativeStringRangeIndex::new();
    string_idx.upsert(201, ["alpha"]);
    string_idx.upsert(202, ["beta", "delta"]);
    string_idx.upsert(203, ["gamma"]);

    if string_idx.equal("beta").collect::<Vec<_>>() != vec![202] {
        return Err("native string equality mismatch".into());
    }
    let strings = string_idx
        .range(Some("beta"), Some("gamma"), true, false)
        .collect::<Vec<_>>();
    if strings != vec![202, 202] {
        return Err(format!("native string range mismatch: {strings:?}"));
    }

    println!("NATIVE_STRING_RANGE_INDEX_SMOKE_PASS");

    let mut property_idx = NativePropertyIndex::new();
    let k_address = Arc::new("address".to_string());
    let k_name = Arc::new("name".to_string());
    let k_tags = Arc::new("tags".to_string());
    let k_where = Arc::new("where".to_string());

    property_idx.upsert(301, Arc::clone(&k_address), &Value::Int(100))?;
    property_idx.upsert(302, Arc::clone(&k_address), &Value::Int(200))?;
    property_idx.upsert(303, Arc::clone(&k_address), &Value::Int(300))?;
    property_idx.upsert(
        301,
        Arc::clone(&k_name),
        &Value::String(Arc::new("alpha".to_string())),
    )?;
    property_idx.upsert(
        302,
        Arc::clone(&k_name),
        &Value::String(Arc::new("beta".to_string())),
    )?;

    let mut in_items = thin_vec::ThinVec::new();
    in_items.push(Value::String(Arc::new("beta".to_string())));
    let ast = IndexQuery::And(vec![
        IndexQuery::Range {
            key: Arc::clone(&k_address),
            min: Some(Value::Int(100)),
            max: Some(Value::Int(250)),
            include_min: true,
            include_max: true,
        },
        IndexQuery::InList {
            key: Arc::clone(&k_name),
            list: Value::List(Arc::new(in_items)),
        },
    ]);
    let ast_hits = property_idx.query(ast)?.collect::<Vec<_>>();
    if ast_hits != vec![302] {
        return Err(format!("native IndexQuery AST mismatch: {ast_hits:?}"));
    }

    let mut tag_items = thin_vec::ThinVec::new();
    tag_items.push(Value::String(Arc::new("shader".to_string())));
    tag_items.push(Value::Int(42));
    property_idx.upsert(307, Arc::clone(&k_tags), &Value::List(Arc::new(tag_items)))?;
    let contains_hits = property_idx
        .query(IndexQuery::ArrayContains {
            key: Arc::clone(&k_tags),
            value: Value::String(Arc::new("shader".to_string())),
        })?
        .collect::<Vec<_>>();
    if contains_hits != vec![307] {
        return Err(format!("native array contains mismatch: {contains_hits:?}"));
    }

    property_idx.upsert(
        309,
        Arc::clone(&k_where),
        &Value::Point(Point::new(40.0, -73.0)),
    )?;
    property_idx.upsert(
        310,
        Arc::clone(&k_where),
        &Value::Point(Point::new(41.0, -73.0)),
    )?;
    let geo_hits = property_idx
        .query(IndexQuery::Point {
            key: Arc::clone(&k_where),
            point: Value::Point(Point::new(40.001, -73.0)),
            radius: Value::Float(1000.0),
        })?
        .collect::<Vec<_>>();
    if geo_hits != vec![309] {
        return Err(format!("native geo query mismatch: {geo_hits:?}"));
    }

    println!("NATIVE_INDEXQUERY_BACKEND_SMOKE_PASS");


    println!("NATIVE_RANGE_INDEX_SMOKE_PASS");

    println!("NATIVE_SMOKE_OK");
    Ok(())
}
