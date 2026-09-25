import argparse

from falkordb import FalkorDB
from falkordb.node import Node
from redis.exceptions import AuthenticationError

HOST = "localhost"
PORT = 6392
PASSWORD = "native-tls-ci-secret"
GRAPH = "tls-official-client"


def connect(ca: str, password: str = PASSWORD):
    return FalkorDB(
        host=HOST,
        port=PORT,
        password=password,
        ssl=True,
        ssl_ca_certs=ca,
        ssl_cert_reqs="required",
        ssl_check_hostname=True,
        socket_connect_timeout=5,
        socket_timeout=10,
        protocol=2,
    )


def phase_write(ca: str):
    try:
        bad = connect(ca, "wrong-password")
        bad.list_graphs()
        raise AssertionError("TLS endpoint accepted wrong password")
    except AuthenticationError:
        pass

    db = connect(ca)
    db.flushdb()
    graph = db.select_graph(GRAPH)
    graph.query("CREATE (:Secure {id:1,name:'encrypted'})")
    result = graph.query("MATCH (n:Secure) RETURN n")
    assert len(result.result_set) == 1
    node = result.result_set[0][0]
    assert isinstance(node, Node), type(node)
    assert node.properties == {"id": 1, "name": "encrypted"}, node.properties
    db.close()
    print("OFFICIAL_FALKORDB_TLS_WRITE_PASS")


def phase_read(ca: str):
    db = connect(ca)
    graph = db.select_graph(GRAPH)
    result = graph.query("MATCH (n:Secure {id:1}) RETURN n.name")
    assert result.result_set == [["encrypted"]], result.result_set
    db.close()
    print("OFFICIAL_FALKORDB_TLS_RESTART_PASS")


if __name__ == "__main__":
    parser = argparse.ArgumentParser()
    parser.add_argument("phase", choices=["write", "read"])
    parser.add_argument("ca")
    args = parser.parse_args()
    if args.phase == "write":
        phase_write(args.ca)
    else:
        phase_read(args.ca)
