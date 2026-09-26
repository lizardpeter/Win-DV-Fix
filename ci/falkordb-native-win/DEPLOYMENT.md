# Native Windows Deployment

This package contains the standalone Windows FalkorDB-derived server. Redis,
Memurai, Docker, WSL, Hyper-V, and a VM are not required at runtime.

## Start locally

Set a password and choose a durable data directory:

```powershell
$env:FALKORDB_PASSWORD = "replace-with-a-long-random-secret"
.\server.exe --bind 127.0.0.1:6379 --data-dir D:\FalkorDBNative\data
```

Connect with the normal FalkorDB Python client:

```python
from falkordb import FalkorDB

db = FalkorDB(
    host="127.0.0.1",
    port=6379,
    password="replace-with-a-long-random-secret",
)
print(db.list_graphs())
```

## Remote deployment

For a non-loopback listener, use authentication and TLS. Supply a normal PEM
server certificate and key. Add `--tls-client-ca` if RESP clients must also
present a certificate signed by your CA.

```powershell
.\server.exe `
  --bind 0.0.0.0:6379 `
  --data-dir D:\FalkorDBNative\data `
  --password "replace-with-a-long-random-secret" `
  --tls-cert D:\FalkorDBNative\tls\server-cert.pem `
  --tls-key D:\FalkorDBNative\tls\server-key.pem
```

The server refuses unsafe unauthenticated non-loopback operation unless an
explicit override flag is supplied.

## Migrate a current FalkorDB deployment

Install the migration client dependencies on the machine from which migration
will be run:

```powershell
py -m pip install -r requirements-migration.txt
```

Then migrate all graphs plus UDF libraries directly from a current standalone
FalkorDB source into the native Windows destination:

```powershell
py .\migrate_current_falkordb.py `
  --source-host SOURCE_HOST `
  --source-port 6379 `
  --source-password "SOURCE_PASSWORD" `
  --destination-host DESTINATION_HOST `
  --destination-port 6379 `
  --destination-password "DESTINATION_PASSWORD"
```

Add the matching `--source-ssl` / `--destination-ssl` options and CA/client
certificate paths for TLS or mTLS endpoints.

By default the migration utility:

- enumerates every graph with `GRAPH.LIST`
- transfers the native Redis/FalkorDB `DUMP` bytes for each graph
- restores under the same graph name
- compares source and destination node/relationship counts
- compares labels, relationship types, property keys, index definitions, and
  constraints
- detects a source graph changing while it is being dumped and retries
- migrates UDF library source code and verifies it
- rolls back a graph or UDF replacement if its migration fails

Use `--replace` only when an existing destination graph/UDF with the same name
should be replaced. Existing destination graph bytes are backed up before a
replacement and restored on failure.

## Offline portable bundle

If source and destination cannot be online simultaneously, create a portable
bundle on a machine that can reach the current FalkorDB source:

```powershell
py .\falkordb_bundle.py export `
  --source-host SOURCE_HOST `
  --source-port 6379 `
  --source-password "SOURCE_PASSWORD" `
  --output .\falkordb-migration.zip
```

Move that ZIP to a machine that can reach the native Windows server and import
it:

```powershell
py .\falkordb_bundle.py import `
  --destination-host DESTINATION_HOST `
  --destination-port 6379 `
  --destination-password "DESTINATION_PASSWORD" `
  --input .\falkordb-migration.zip
```

The bundle contains the original graph DUMP bytes, SHA-256 hashes, semantic
source signatures, and UDF source code. Import verifies the hashes and compares
the restored graph against the recorded node/relationship counts, labels,
relationship types, property keys, indexes, and constraints. Use
`falkordb_bundle.py inspect --input <bundle.zip>` to inspect its manifest
without connecting to a server.

## Import a current FalkorDB dump.rdb directly

If the old FalkorDB server is offline but its Redis RDB file is available, the
package can import the file without starting Redis/FalkorDB first.

Inspect it without changing the destination:

```powershell
py .\import_falkordb_rdb.py inspect --rdb D:\backup\dump.rdb
```

Then import into an empty native destination:

```powershell
py .\import_falkordb_rdb.py import `
  --rdb D:\backup\dump.rdb `
  --destination-host 127.0.0.1 `
  --destination-port 6379 `
  --destination-password "DESTINATION_PASSWORD"
```

The importer understands current FalkorDB v19 `graphdata` and `graphmeta`
module values, including graphs split across multiple virtual RDB keys, and it
restores FalkorDB UDF libraries from the module AUX section. It validates the
Redis checksum and graph fragment counts before writing.

This path intentionally fails rather than silently dropping data when the RDB
contains semantics the native graph host cannot preserve. In particular, raw
import currently requires Redis DB 0 and rejects expiring graph keys, ordinary
Redis keys, Redis FUNCTION libraries, unsupported module types, and non-v19
FalkorDB graph encodings. It is non-destructive: an existing destination graph
with the same name causes an error rather than replacement.

## Import a single ordinary FalkorDB DUMP

The native server implements Redis `RESTORE` for current FalkorDB
`graphdata` v19 values. Standard Redis/FalkorDB clients can therefore send an
ordinary graph `DUMP` directly to the native server. The imported graph is
immediately converted to the native checkpoint/WAL durability format.

The Windows CI also generates a DUMP with the official FalkorDB container,
restores those exact bytes on the Windows-native server, queries data/indexes/
constraints, hard-kills the server, restarts it, and queries the imported graph
again.

## Durability

Each graph has its own native WAL. The server automatically checkpoints graphs
whose WAL reaches the configured threshold (256 MiB by default):

```powershell
.\server.exe --checkpoint-wal-mb 256 ...
```

Set `FALKORDB_CHECKPOINT_WAL_MB` or pass `--checkpoint-wal-mb` to change the
threshold.

## ChatGPT HTTPS API

The same server can expose the authenticated HTTPS JSON API:

```powershell
.\server.exe `
  --bind 0.0.0.0:6379 `
  --data-dir D:\FalkorDBNative\data `
  --password "RESP_PASSWORD" `
  --tls-cert D:\FalkorDBNative\tls\server-cert.pem `
  --tls-key D:\FalkorDBNative\tls\server-key.pem `
  --api-bind 0.0.0.0:8443 `
  --api-token "READ_WRITE_BEARER_TOKEN" `
  --api-read-token "READ_ONLY_BEARER_TOKEN"
```

The API exposes health, capabilities, graph listing, query/batch execution, and
graph deletion endpoints. Bearer authorization is checked before graph writes.
