import pathlib
import sys

import trustme

out = pathlib.Path(sys.argv[1]).resolve()
out.mkdir(parents=True, exist_ok=True)

ca = trustme.CA()
server = ca.issue_cert("localhost")
client = ca.issue_cert("falkordb-native-ci-client")

ca.cert_pem.write_to_path(out / "ca.pem")
server.cert_chain_pems[0].write_to_path(out / "server-cert.pem")
server.private_key_pem.write_to_path(out / "server-key.pem")
client.cert_chain_pems[0].write_to_path(out / "client-cert.pem")
client.private_key_pem.write_to_path(out / "client-key.pem")

print(out)
