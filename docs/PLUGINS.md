# Plug-ins (sandboxed WebAssembly)

The server stays graph- and schema-agnostic. Project logic that has to run next to the data (an ontology engine,
a study scheduler) ships as a WebAssembly plug-in: the server stores the module, runs its functions against one
named graph, and gives it nothing else.

## Sandbox
- A call reaches only the graph it names, through Cypher (`host.query`, `host.transaction`). Writes need the
  caller's write scope; a read-only token gets a read-only call.
- No files, network, environment or other graphs. Memory is capped (4 GiB of linear memory); a call has a
  wall-clock deadline (`timeout_ms`, default 60 s, at most 600 s).
- A trap (deadline, out of memory, panic) fails that call only; the instance is rebuilt on the next call.
- An instance lives between calls for the same (plug-in, graph), so a plug-in may cache state; calls on one
  instance run one at a time.

## API (Bearer token as for queries)
| Route | Scope | Body |
| --- | --- | --- |
| `GET /v1/plugins` | read | |
| `POST /v1/plugins/load` | write | `{"name", "wasm_base64", "replace"}` |
| `POST /v1/plugins/delete` | write | `{"name"}` |
| `POST /v1/plugins/call` | read (writes need write) | `{"plugin", "function", "graph", "input", "timeout_ms"}` |

Modules are stored in `<data_dir>/plugins/` with a hash-checked index (`plugins.json`) and reloaded at start.

## ABI
Plain core WebAssembly (no WASI, no component model):
- exports `memory`, `plugin_alloc(len: i32) -> i32`, `plugin_free(ptr: i32, len: i32)`, and any number of
  `api_<name>(ptr: i32, len: i32) -> i64` (JSON in; returns `(ptr << 32) | len` of a JSON buffer the host frees).
- imports from module `host`: `query(ptr, len, read_only) -> i64`, `transaction(ptr, len) -> i64`
  (`{"queries": [{"cypher", "read_only"}]}`, all or nothing), `now_ms() -> i64`, `log(ptr, len)`.

`ontology/kweb` (on the `ontology` branch) is the reference plug-in. `plugin_smoke KWEB.wasm` checks the contract.
