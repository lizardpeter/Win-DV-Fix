# Native Windows Deployment

This package contains the standalone Windows FalkorDB-derived server. Redis,
Memurai, Docker, WSL, Hyper-V, and a VM are not required at runtime.

## Clean single-EXE portable layout

The normal Windows runtime archive intentionally contains **one file only**:

```text
falkordb-native-windows-x64\
  server.exe
```

Portable mode is now the default behavior of `server.exe`; no BAT file,
PowerShell launcher, installed Python, Redis, Docker, WSL, or service wrapper is
required. On first launch the executable creates one application-state folder
beside itself:

```text
falkordb-native-windows-x64\
  server.exe
  FalkorDB\
    data\
    logs\
    tls\
    tmp\
    imports\
    exports\
    migration\
    cache\
    config\
    profile\
    ...other private runtime state...
```

The graph catalog defaults to `.\FalkorDB\data`. Temporary files, the
embedded OpenAI tunnel runtime, local profile/cache state, OAuth grants, and
other portable runtime files are also confined beneath `.\FalkorDB`.
Relative data/TLS paths cannot escape that state directory.

For advanced service deployments only, `--no-portable` (or
`FALKORDB_PORTABLE=0`) opts out. `--portable` remains accepted explicitly
for compatibility.

The application can therefore be moved as a unit without writing ordinary
runtime state to the user profile, ProgramData, AppData, or the caller's
working directory.

## Local, bounded crash diagnostics

The running `server.exe` writes application-generated crash diagnostics only
beneath the executable's `FalkorDB/logs/` directory, including when explicitly
launched with `--no-portable`. Startup rejects a diagnostics directory that
resolves outside the executable's folder and rejects redirected log files.

```text
falkordb-native-windows-x64/
  server.exe
  FalkorDB/
    logs/
      query-breadcrumbs.bin   # fixed 2 MiB circular journal
      crash-latest.txt        # 4 KiB maximum
      crash-previous.txt      # 4 KiB maximum
```

The journal stores timestamps, query IDs and parent IDs, a non-cryptographic query fingerprint,
length, a short **literal-redacted** query shape, and query start/completion
events. Query result data is never logged. Writes are small, buffered and
best-effort; busy diagnostic locks are skipped instead of blocking the engine.
No background sampling, full-memory dump, or unbounded appending is enabled.
No persistent slowlog is created.

A Rust heap allocation returning null triggers a bounded failure record
containing the requested byte count, active query ID and last thread-local
engine stage (planning, execution, result capture, commit, or serialization).
Rust panics generate a report containing the panic source location but
intentionally omit possibly sensitive panic payloads. On Windows, up to 20 raw
native stack addresses are added to the report on failures without symbolizing
or writing memory dumps. Failed C/native allocator
calls, operating-system hard termination and a few other fatal exceptions may
not enter these Rust hooks; in such cases the last in-progress breadcrumbs
remain useful. These diagnostics do **not** change Windows Error Reporting's
independent system-level collection policy.

Crash reports rotate on the next successful startup, retaining at most two
small reports. The binary journal always overwrites older fixed-size slots.
The application-controlled diagnostic budget is approximately 2.01 MiB.
An existing Windows binary must be replaced with a build containing this
feature; updating the repository does not retrofit diagnostics.

## Where Windows builds come from

This repository is private, and GitHub Actions minutes for private repositories
depend on account billing. Release builds therefore run in the **public**
`lizardpeter/Win-DV-Fix` repository on its `windows-build` branch, where
Actions are free. That branch holds a single-commit snapshot of this tree
(no private history); pushing a new snapshot runs the same two workflows:

```bash
SNAP=$(git commit-tree HEAD^{tree} -p "$(git ls-remote https://github.com/lizardpeter/Win-DV-Fix refs/heads/windows-build | cut -f1)" \
       -m "custom-falkor-db snapshot $(git rev-parse --short HEAD)")
git push https://github.com/lizardpeter/Win-DV-Fix "$SNAP:refs/heads/windows-build"
```

The **FalkorDB Windows Package** run publishes `server.exe` as the
`falkordb-native-windows-x64` artifact and logs its SHA-256. Everything pushed
there is public, so never commit secrets to this repository.

## Build a fresh Windows release without hosted GitHub runners

If the private repository's GitHub Actions jobs fail before they are assigned
a runner, compile directly in a Windows source checkout instead. You need
a Windows MSVC Rust toolchain, the Visual Studio C++ build tools and CMake.
Every native dependency (the FalkorDB engine, GraphBLAS, LAGraph and the
embedded tunnel runtime) is vendored in this repository and compiled or
unpacked by `cargo build`; nothing is cloned or downloaded except Rust crates
pinned by `native_host/Cargo.lock`. The script tests the resulting standalone
executable. It does **not** replace any installed server or touch existing
graph data.

```powershell
git pull --ff-only
powershell -NoProfile -ExecutionPolicy Bypass -File .\scripts\build_local_release.ps1
```

For a fully fresh integration smoke (including graph persistence and restart
tests), use `-FullValidation`. Subsequent builds reuse `build/cargo-target`,
including the compiled GraphBLAS and LAGraph, so only changed code rebuilds.

The same build works with plain cargo:

```powershell
$env:RUSTFLAGS = "-C target-feature=+crt-static"
cargo build --manifest-path native_host\Cargo.toml --bin server --release --locked
```

On success the deployable files are:

- `build/package/falkordb-native-windows-x64/server.exe`
- `build/package/falkordb-native-windows-x64.zip`

The packaging smoke test verifies a fixed 2 MiB query journal and bounded
crash report inside the binary's install-local `FalkorDB/logs/` directory.
Keep your existing `FalkorDB/data/` unchanged when installing the new EXE.
Never overwrite a running executable.

## Start locally

Set a password and run the executable directly:

```powershell
$env:FALKORDB_PASSWORD = "replace-with-a-long-random-secret"
.\server.exe
```

The persistent database is automatically stored in
`.\FalkorDB\data`; no launcher script or external data path is needed.

When upgrading an older portable package, the first normal server launch
automatically moves the legacy top-level runtime folders (including `.\data`)
and `falkordb-secrets.txt` into `.\FalkorDB` when the corresponding new
location does not already exist. This preserves existing graphs, OAuth grants,
and local tunnel credentials while cleaning up the old runtime layout.

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
  --password "replace-with-a-long-random-secret" `
  --tls-cert tls\server-cert.pem `
  --tls-key tls\server-key.pem
```

The server refuses unsafe unauthenticated non-loopback operation unless an
explicit override flag is supplied.

The MCP endpoint (`/mcp`) requires a bearer credential (an API token or an
OAuth access token). When no API token is configured it serves **no** tools,
on any bind address. Tunnels and reverse proxies deliver remote traffic from loopback,
so a loopback bind is not treated as proof of a local caller. To run MCP
without credentials on a trusted machine, pass `--api-allow-unauthenticated-remote`
explicitly.

Failed credential checks are rate limited: per remote address for bearer
tokens and authorization codes, and globally for the OAuth owner secret
(10 failures in 15 minutes locks the consent page for 15 minutes; existing
tokens keep working and a restart prints a fresh pairing code).

## Migrate a current FalkorDB deployment

Migration and recovery utilities are intentionally **not** mixed into the normal
single-EXE runtime. Download the separate
`falkordb-native-windows-x64-tools.zip` artifact only when those utilities are
needed. That optional tools bundle includes its own embedded Python runtime and
pinned FalkorDB/Redis client libraries, so no machine-wide Python or pip
installation is required.

Migrate all graphs plus UDF libraries directly from a current standalone
FalkorDB source into the native Windows destination:

```powershell
.\run-tool.ps1 migrate-live `
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
.\run-tool.ps1 bundle export `
  --source-host SOURCE_HOST `
  --source-port 6379 `
  --source-password "SOURCE_PASSWORD" `
  --output .\FalkorDB\exports\falkordb-migration.zip
```

Move that ZIP to a machine that can reach the native Windows server and import
it:

```powershell
.\run-tool.ps1 bundle import `
  --destination-host DESTINATION_HOST `
  --destination-port 6379 `
  --destination-password "DESTINATION_PASSWORD" `
  --input .\FalkorDB\imports\falkordb-migration.zip
```

The bundle contains the original graph DUMP bytes, SHA-256 hashes, semantic
source signatures, and UDF source code. Import verifies the hashes and compares
the restored graph against the recorded node/relationship counts, labels,
relationship types, property keys, indexes, and constraints. Use
`falkordb_bundle.py inspect --input <bundle.zip>` to inspect its manifest
without connecting to a server.

## Import an existing current FalkorDB dump.rdb directly

For a current standalone FalkorDB RDB file, the original FalkorDB server does
not need to be running. Inspect the file first:

```powershell
.\run-tool.ps1 import-rdb inspect --rdb .\FalkorDB\imports\dump.rdb
```

Then import it into an empty/native destination:

```powershell
.\run-tool.ps1 import-rdb import `
  --rdb .\FalkorDB\imports\dump.rdb `
  --destination-host DESTINATION_HOST `
  --destination-port 6379 `
  --destination-password "DESTINATION_PASSWORD"
```

Add the destination TLS/mTLS options when required. The raw importer supports
current FalkorDB `graphdata`/`graphmeta` v19 RDB persistence, including
multi-key virtual graph fragments and FalkorDB UDF module-AUX source. It
validates the Redis CRC, reassembles every graph, verifies node/relationship
counts, and converts each graph to the native checkpoint/WAL format.

Raw RDB import is intentionally non-destructive and fail-loud. It refuses
existing destination graph names, expiring graph keys, non-zero Redis DBs,
ordinary non-graph Redis keys, Redis FUNCTION libraries, unsupported module
types, or non-v19 FalkorDB graph records rather than silently dropping data.
The one explicit auxiliary exception is FalkorDB's own `telemetry{graph}`
Redis Stream (current stream encodings 26/27): it is parsed and structurally
validated, omitted from native graph semantics, reported in the import audit,
and accepted only when the graph named inside the telemetry key is present in
the same RDB.

### Import an RDB through ChatGPT / MCP

The authenticated MCP surface exposes an admin-only
\`import_falkordb_rdb_file\` tool. It reads a relative file below
\`FALKORDB_IMPORT_DIR\` (or \`<data-dir>/imports\` when unset), so large RDBs do
not transit through the MCP request body. Supply \`expected_graph\` and
\`sha256\` to bind the operation to the intended authoritative snapshot. Use
\`dry_run=true\` for validation without graph creation.

The MCP import refuses overwrite, verifies Redis CRC64 and optional SHA-256,
reassembles the ordered FalkorDB v19 fragments, verifies total node and
relationship counts after restore, checkpoints the new graph immediately, and
rolls back graphs created by the operation if a later verification step fails.
RDBs containing FalkorDB UDF libraries are rejected by this MCP path rather
than discarding UDF source; use the packaged \`import-rdb\` CLI when UDF
restoration is required.

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

On Windows, Ctrl+C, Ctrl+Break, and console-close shutdown events trigger a
best-effort checkpoint of every graph with pending WAL data before the process
exits. If a checkpoint fails, the existing WAL remains available for restart
recovery.

## ChatGPT HTTPS API

The same server can expose the authenticated HTTPS JSON API:

```powershell
.\server.exe `
  --bind 0.0.0.0:6379 `
  --password "RESP_PASSWORD" `
  --tls-cert tls\server-cert.pem `
  --tls-key tls\server-key.pem `
  --api-bind 0.0.0.0:8443 `
  --api-token "READ_WRITE_BEARER_TOKEN" `
  --api-read-token "READ_ONLY_BEARER_TOKEN"
```

The API exposes health, capabilities, graph listing, query/batch execution, and
graph deletion endpoints. Bearer authorization is checked before graph writes.
