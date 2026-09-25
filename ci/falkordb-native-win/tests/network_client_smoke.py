import argparse
import os
import pathlib
import sys

from falkordb import FalkorDB
from falkordb.node import Node
from falkordb.edge import Edge
from redis.exceptions import AuthenticationError, ConnectionError, ResponseError

HOST = "localhost"
PORT = 6391
PASSWORD = "native-ci-secret"
GRAPH = "network-official-client"

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

    graph.create_node_range_index("Person", "age")
    graph.create_node_fulltext_index("Person", "name")

    graph.query(
        "CREATE (a:Person {name:'Alice',age:40}), "
        "(b:Person {name:'Bob',age:30}), "
        "(a)-[:KNOWS {since:2026}]->(b)"
    )

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

    result = graph.query(
        "MATCH (n:Person) WHERE n.age >= 35 RETURN n.name"
    )
    assert result.result_set == [["Alice"]], result.result_set

    result = graph.query(
        "CALL db.idx.fulltext.queryNodes('Person','Alice') "
        "YIELD node RETURN node.name"
    )
    assert result.result_set == [["Alice"]], result.result_set

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
