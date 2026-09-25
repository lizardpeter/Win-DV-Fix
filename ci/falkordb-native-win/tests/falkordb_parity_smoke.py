import os
import pathlib

from falkordb import FalkorDB
from redis.exceptions import ResponseError

HOST = "localhost"
PORT = 6391
PASSWORD = "native-ci-secret"
GRAPH = "official-parity"
COPY = "official-parity-copy"

TLS_DIR = pathlib.Path(os.environ["FALKORDB_TEST_TLS_DIR"]).resolve()


def connect():
    return FalkorDB(
        host=HOST,
        port=PORT,
        password=PASSWORD,
        socket_connect_timeout=5,
        socket_timeout=20,
        protocol=2,
        ssl=True,
        ssl_ca_certs=str(TLS_DIR / "ca.pem"),
        ssl_cert_reqs="required",
        ssl_check_hostname=True,
        ssl_certfile=str(TLS_DIR / "client-cert.pem"),
        ssl_keyfile=str(TLS_DIR / "client-key.pem"),
    )


def main():
    db = connect()
    graph = db.select_graph(GRAPH)

    # Clean up from a retried CI phase without flushing graphs used by other
    # network/restart tests.
    for name in (GRAPH, COPY):
        try:
            db.select_graph(name).delete()
        except ResponseError:
            pass

    graph = db.select_graph(GRAPH)

    # Parameter transport and the normal official-client query envelope.
    result = graph.query("RETURN $x AS x, $s AS s", params={"x": 7, "s": "parity"})
    assert result.result_set == [[7, "parity"]], result.result_set

    graph.query(
        "CREATE "
        "(a:Account {id:1,name:'alpha',required:'yes'}),"
        "(b:Account {id:2,name:'beta',required:'yes'}),"
        "(a)-[:LINK {id:10,required:'yes'}]->(b)"
    )

    # Explain/profile must produce plans the unmodified client can parse.
    explain = graph.explain("MATCH (n:Account) WHERE n.id=1 RETURN n.name")
    assert explain.structured_plan is not None
    profile = graph.profile("MATCH (n:Account) RETURN n.name")
    assert profile.structured_plan is not None
    assert any(
        op.profile_stats is not None
        for ops in profile.operations.values()
        for op in ops
    ), profile.plan

    # Slowlog API shape/reset parity. A fast query may legitimately not appear.
    assert isinstance(graph.slowlog(), list)
    graph.slowlog_reset()
    assert graph.slowlog() == []

    # Native range/fulltext/vector paths plus index introspection.
    graph.create_node_range_index("Account", "id")
    graph.create_node_fulltext_index("Account", "name")
    graph.create_node_vector_index("Account", "embedding", dim=2)
    graph.query(
        "MATCH (a:Account {id:1}),(b:Account {id:2}) "
        "SET a.embedding=vecf32([0.0,0.0]), b.embedding=vecf32([10.0,10.0])"
    )
    indexes = graph.list_indices().result_set
    assert len(indexes) >= 3, indexes

    result = graph.query("MATCH (n:Account) WHERE n.id=2 RETURN n.name")
    assert result.result_set == [["beta"]], result.result_set
    result = graph.query(
        "CALL db.idx.fulltext.queryNodes('Account','alpha') "
        "YIELD node RETURN node.id"
    )
    assert result.result_set == [[1]], result.result_set
    result = graph.query(
        "CALL db.idx.vector.queryNodes('Account','embedding',1,vecf32([0.1,0.1])) "
        "YIELD node RETURN node.id"
    )
    assert result.result_set == [[1]], result.result_set

    # Official-client constraint methods must reach the graph crate's real
    # enforcement logic and survive through normal Cypher mutations.
    graph.create_node_unique_constraint("Account", "id")
    constraints = graph.list_constraints()
    assert any(
        c["type"] == "UNIQUE"
        and c["label"] == "Account"
        and "id" in c["properties"]
        for c in constraints
    ), constraints

    try:
        graph.query("CREATE (:Account {id:1,name:'duplicate',required:'yes'})")
        raise AssertionError("UNIQUE constraint accepted duplicate Account.id")
    except ResponseError:
        pass

    graph.create_node_mandatory_constraint("Account", "required")
    try:
        graph.query("CREATE (:Account {id:3,name:'missing-required'})")
        raise AssertionError("MANDATORY constraint accepted missing property")
    except ResponseError:
        pass

    # GRAPH.COPY must include data, index DDL, and constraints because it is
    # reconstructed from the committed effects stream.
    clone = graph.copy(COPY)
    result = clone.query("MATCH (n:Account) WHERE n.id=1 RETURN n.name")
    assert result.result_set == [["alpha"]], result.result_set
    result = clone.query(
        "CALL db.idx.fulltext.queryNodes('Account','beta') "
        "YIELD node RETURN node.id"
    )
    assert result.result_set == [[2]], result.result_set
    copied_constraints = clone.list_constraints()
    assert any(c["type"] == "UNIQUE" for c in copied_constraints), copied_constraints
    try:
        clone.query("CREATE (:Account {id:1,name:'copy-duplicate',required:'yes'})")
        raise AssertionError("copied UNIQUE constraint was not enforced")
    except ResponseError:
        pass

    assert GRAPH in db.list_graphs()
    assert COPY in db.list_graphs()

    # Constraint drops must work through the official client's command path.
    graph.drop_node_mandatory_constraint("Account", "required")
    graph.drop_node_unique_constraint("Account", "id")
    remaining = graph.list_constraints()
    assert not any(c["type"] in ("UNIQUE", "MANDATORY") for c in remaining), remaining

    clone.delete()
    assert COPY not in db.list_graphs()

    # GRAPH.CONFIG API shape should be accepted by the official client even
    # when the setting is not relevant to the standalone deployment.
    assert db.config_get("RESULTSET_SIZE") is not None

    db.close()
    print("OFFICIAL_FALKORDB_PARITY_GATE_PASS")


if __name__ == "__main__":
    main()
