import argparse
import os
import pathlib
import sys
import struct

from falkordb import FalkorDB
from falkordb.node import Node
from falkordb.edge import Edge
from redis.exceptions import AuthenticationError, ConnectionError, ResponseError

HOST = "localhost"
PORT = 6391
PASSWORD = "native-ci-secret"
GRAPH = "network-official-client"
BULK_GRAPH = "network-bulk"

TLS_DIR = pathlib.Path(
    os.environ.get("FALKORDB_TEST_TLS_DIR", "")
).resolve() if os.environ.get("FALKORDB_TEST_TLS_DIR") else None


def tls_kwargs(with_client_cert=True):
    if TLS_DIR is None:
        return {}
    kwargs = {
        "ssl": True,
        "ssl_ca_certs": str(TLS_DIR / "ca.pem"),
        "ssl_cert_reqs": "required",
        "ssl_check_hostname": True,
    }
    if with_client_cert:
        kwargs["ssl_certfile"] = str(TLS_DIR / "client-cert.pem")
        kwargs["ssl_keyfile"] = str(TLS_DIR / "client-key.pem")
    return kwargs


def connect(password=PASSWORD, with_client_cert=True):
    return FalkorDB(
        host=HOST,
        port=PORT,
        password=password,
        socket_connect_timeout=5,
        socket_timeout=10,
        protocol=2,
        **tls_kwargs(with_client_cert=with_client_cert),
    )


def phase_write():
    # mTLS must actually gate the socket before RESP authentication.
    if TLS_DIR is not None:
        try:
            no_cert = connect(with_client_cert=False)
            no_cert.list_graphs()
            raise AssertionError("mTLS connection without a client certificate unexpectedly succeeded")
        except (ConnectionError, OSError):
            pass

    # Authentication must actually gate commands.
    try:
        bad = connect("definitely-wrong")
        bad.list_graphs()
        raise AssertionError("wrong password unexpectedly authenticated")
    except (AuthenticationError, ResponseError):
        pass

    db = connect()
    db.flushdb()
    graph = db.select_graph(GRAPH)

    result = graph.query("RETURN 1 AS one, 'wire-ok' AS text")
    assert result.result_set == [[1, "wire-ok"]], result.result_set

    # Official FalkorDB client admin/config surface.
    original_resultset_size = db.config_get("RESULTSET_SIZE")
    assert int(original_resultset_size) == -1, original_resultset_size
    db.config_set("RESULTSET_SIZE", 1)
    limited = graph.query("UNWIND [1,2,3] AS x RETURN x")
    assert limited.result_set == [[1]], limited.result_set
    db.config_set("RESULTSET_SIZE", -1)

    db.config_set("TIMEOUT_MAX", 1)
    try:
        graph.query("RETURN 1", timeout=2)
        raise AssertionError("per-query timeout above TIMEOUT_MAX unexpectedly succeeded")
    except ResponseError as exc:
        assert "TIMEOUT_MAX" in str(exc), exc
    db.config_set("TIMEOUT_MAX", 0)

    graph.create_node_range_index("Person", "age")
    graph.create_node_fulltext_index("Person", "name")

    graph.query(
        "CREATE (a:Person {name:'Alice',age:40}), "
        "(b:Person {name:'Bob',age:30}), "
        "(a)-[:KNOWS {since:2026}]->(b)"
    )

    memory = db.execute_command("GRAPH.MEMORY", "USAGE", GRAPH)
    assert len(memory) == 18, memory
    memory_map = dict(zip(memory[0::2], memory[1::2]))
    assert "total_graph_sz_mb" in memory_map, memory_map
    assert "indices_sz_mb" in memory_map, memory_map

    object_pool = db.execute_command("GRAPH.INFO", "ObjectPool")
    assert object_pool[0] == "Object Pool", object_pool
    assert len(object_pool[1]) == 2, object_pool
    assert db.execute_command("GRAPH.INFO", "not-a-section") == "no section found"

    # Explain/profile are parsed into the official client's ExecutionPlan type.
    explain = graph.explain("MATCH (n:Person) RETURN n.name")
    assert explain.plan and len(explain.plan) > 0, explain.plan
    profile = graph.profile("MATCH (n:Person) RETURN n.name")
    assert profile.plan and len(profile.plan) > 0, profile.plan

    # Real per-graph slowlog plus reset.
    graph.query("MATCH (n:Person) RETURN n.name")
    slow = graph.slowlog()
    assert len(slow) > 0, slow
    graph.slowlog_reset()
    assert graph.slowlog() == [], graph.slowlog()

    # Constraint creation/enforcement through official high-level helpers.
    assert graph.create_node_unique_constraint("Person", "name") == "OK"
    memory = db.execute_command("GRAPH.MEMORY", "USAGE", GRAPH, "SAMPLES", 10)
    assert len(memory) == 18, memory
    info = db.execute_command("GRAPH.INFO")
    assert "# Running queries" in info, info
    assert "# Waiting queries" in info, info
    assert "Object Pool" in info, info

    constraints = graph.list_constraints()
    assert any(
        c["type"] == "UNIQUE"
        and c["label"] == "Person"
        and "name" in c["properties"]
        for c in constraints
    ), constraints
    try:
        graph.query("CREATE (:Person {name:'Alice',age:99})")
        raise AssertionError("unique constraint did not reject duplicate Person.name")
    except ResponseError:
        pass

    # GRAPH.COPY must produce an independently queryable durable graph.
    clone = graph.copy("network-official-client-copy")
    clone_result = clone.query("MATCH (n:Person) RETURN count(n)")
    assert clone_result.result_set == [[2]], clone_result.result_set

    # This exercises compact Node/Edge encoding plus schema-id refresh through
    # DB.LABELS, DB.PROPERTYKEYS and DB.RELATIONSHIPTYPES.
    result = graph.query(
        "MATCH (a:Person)-[r:KNOWS]->(b:Person) RETURN a,r,b"
    )
    assert len(result.result_set) == 1
    a, edge, b = result.result_set[0]
    assert isinstance(a, Node), type(a)
    assert isinstance(edge, Edge), type(edge)
    assert isinstance(b, Node), type(b)
    assert a.properties["name"] == "Alice", a.properties
    assert a.properties["age"] == 40, a.properties
    assert b.properties["name"] == "Bob", b.properties
    assert edge.relation == "KNOWS", edge.relation
    assert edge.properties["since"] == 2026, edge.properties

    result = graph.query(
        "MATCH (n:Person) WHERE n.age >= 35 RETURN n.name ORDER BY n.name"
    )
    assert result.result_set == [["Alice"]], result.result_set

    result = graph.query(
        "CALL db.idx.fulltext.queryNodes('Person','Alice') "
        "YIELD node RETURN node.name"
    )
    assert result.result_set == [["Alice"]], result.result_set

    # Read-only command must work for reads and reject writes.
    result = graph.ro_query("MATCH (n:Person) RETURN count(n)")
    assert result.result_set == [[2]], result.result_set
    try:
        graph.ro_query("CREATE (:ShouldFail)")
        raise AssertionError("GRAPH.RO_QUERY accepted a write")
    except ResponseError:
        pass

    # Upstream-compatible binary GRAPH.BULK transport.
    node_token = (
        b"BulkN\x00"
        + struct.pack("=I", 1)
        + b"v\x00"
        + struct.pack("=Bq", 4, 11)
        + struct.pack("=Bq", 4, 22)
    )
    edge_token = (
        b"BulkR\x00"
        + struct.pack("=I", 1)
        + b"w\x00"
        + struct.pack("=QQ", 0, 1)
        + struct.pack("=Bq", 4, 99)
    )
    bulk_reply = db.execute_command(
        "GRAPH.BULK", BULK_GRAPH, "BEGIN", 2, 1, 1, 1, node_token, edge_token
    )
    assert "2 nodes created" in bulk_reply, bulk_reply
    assert "1 relations created" in bulk_reply, bulk_reply

    bulk_graph = db.select_graph(BULK_GRAPH)
    bulk_rows = bulk_graph.query(
        "MATCH (a:BulkN)-[r:BulkR]->(b:BulkN) RETURN a.v,r.w,b.v"
    ).result_set
    assert bulk_rows == [[11, 99, 22]], bulk_rows

    append_token = (
        b"BulkN\x00"
        + struct.pack("=I", 1)
        + b"v\x00"
        + struct.pack("=Bq", 4, 33)
    )
    append_reply = db.execute_command(
        "GRAPH.BULK", BULK_GRAPH, 1, 0, 1, 0, append_token
    )
    assert "1 nodes created" in append_reply, append_reply
    assert bulk_graph.query("MATCH (n:BulkN) RETURN count(n)").result_set == [[3]]

    try:
        db.execute_command("GRAPH.BULK", "bulk-invalid", "BEGIN", "010", 0, 0, 0)
        raise AssertionError("non-canonical GRAPH.BULK count unexpectedly succeeded")
    except ResponseError:
        pass
    assert "bulk-invalid" not in db.list_graphs(), db.list_graphs()
    print("OFFICIAL_FALKORDB_CLIENT_BULK_WRITE_PASS")

    udf_script = """
    function PersistedAdd(x) { return x + 7; }
    falkor.register("PersistedAdd", PersistedAdd);
    """
    assert db.udf_load("native_ci", udf_script, True) == "OK"
    udf_result = graph.query("RETURN native_ci.PersistedAdd(35)")
    assert udf_result.result_set == [[42]], udf_result.result_set

    udf_list = db.udf_list("native_ci", with_code=True)
    assert len(udf_list) == 1, udf_list
    assert udf_list[0][1] == "native_ci", udf_list
    assert "PersistedAdd" in udf_list[0][3], udf_list

    names = db.list_graphs()
    assert GRAPH in names, names
    db.close()
    print("OFFICIAL_FALKORDB_CLIENT_UDF_WRITE_PASS")
    print("OFFICIAL_FALKORDB_CLIENT_WRITE_PASS")
    if TLS_DIR is not None:
        print("OFFICIAL_FALKORDB_CLIENT_MTLS_WRITE_PASS")


def phase_read():
    db = connect()
    graph = db.select_graph(GRAPH)

    # Server process was terminated between phases. This result must therefore
    # come from the standalone WAL recovery path.
    result = graph.query(
        "MATCH (a:Person {name:'Alice'})-[r:KNOWS]->(b:Person {name:'Bob'}) "
        "RETURN a.age, r.since, b.age"
    )
    assert result.result_set == [[40, 2026, 30]], result.result_set

    constraints = graph.list_constraints()
    assert any(
        c["type"] == "UNIQUE"
        and c["label"] == "Person"
        and "name" in c["properties"]
        for c in constraints
    ), constraints

    clone = db.select_graph("network-official-client-copy")
    clone_result = clone.query("MATCH (n:Person) RETURN count(n)")
    assert clone_result.result_set == [[2]], clone_result.result_set

    result = graph.query(
        "MATCH (n:Person) WHERE n.age >= 35 RETURN n.name"
    )
    assert result.result_set == [["Alice"]], result.result_set

    result = graph.query(
        "CALL db.idx.fulltext.queryNodes('Person','Alice') "
        "YIELD node RETURN node.name"
    )
    assert result.result_set == [["Alice"]], result.result_set

    bulk_graph = db.select_graph(BULK_GRAPH)
    bulk_rows = bulk_graph.query(
        "MATCH (n:BulkN) RETURN n.v ORDER BY n.v"
    ).result_set
    assert bulk_rows == [[11], [22], [33]], bulk_rows
    rel_rows = bulk_graph.query(
        "MATCH (:BulkN)-[r:BulkR]->(:BulkN) RETURN r.w"
    ).result_set
    assert rel_rows == [[99]], rel_rows
    print("OFFICIAL_FALKORDB_CLIENT_BULK_RESTART_PASS")

    # GRAPH.UDF is process-global upstream state. The standalone server persists
    # it beside graph WAL/checkpoints and must restore it on a fresh process.
    udf_result = graph.query("RETURN native_ci.PersistedAdd(35)")
    assert udf_result.result_set == [[42]], udf_result.result_set
    udf_list = db.udf_list("native_ci")
    assert len(udf_list) == 1, udf_list

    assert GRAPH in db.list_graphs()
    db.close()
    print("OFFICIAL_FALKORDB_CLIENT_UDF_RESTART_PASS")
    print("OFFICIAL_FALKORDB_CLIENT_RESTART_PASS")
    if TLS_DIR is not None:
        print("OFFICIAL_FALKORDB_CLIENT_MTLS_RESTART_PASS")


if __name__ == "__main__":
    parser = argparse.ArgumentParser()
    parser.add_argument("phase", choices=["write", "read"])
    args = parser.parse_args()
    if args.phase == "write":
        phase_write()
    else:
        phase_read()
