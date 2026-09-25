use falkordb_native_host::{Engine, NativeGraph};

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

    println!("NATIVE_SMOKE_OK");
    Ok(())
}
