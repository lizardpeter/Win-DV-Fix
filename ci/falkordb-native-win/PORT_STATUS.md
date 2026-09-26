# Port Status

## Final feasibility result

Standalone native Windows FalkorDB-derived engine: **WORKING**

Verification is commit-based: the deployable ZIP contains `BUILD_INFO.txt` with
its exact repository commit. The FalkorDB native Windows workflow for that
commit is authoritative and must be green. Required end-to-end markers include
`NATIVE_WINDOWS_FULL_STANDALONE_PASS`,
`NATIVE_WINDOWS_NETWORK_FALKORDB_CLIENT_PASS`,
`NATIVE_WINDOWS_WHOLE_DATABASE_MIGRATION_PASS`, and the migration/import
markers enabled by that commit.

## Completed

- [x] Directly host FalkorDB `graph` crate
- [x] Remove Redis server requirement
- [x] Remove Memurai requirement
- [x] Remove WSL/Docker/VM requirement
- [x] Windows x64 MSVC build
- [x] GraphBLAS/LAGraph Windows native build
- [x] Patch Unix-only FalkorDB host assumptions
- [x] Replace RediSearch implementation with native Rust backend
- [x] Node range indexing
- [x] Relationship range indexing
- [x] String/equality indexing
- [x] Full-text node indexing/query
- [x] Full-text relationship indexing/query
- [x] Vector node indexing/query
- [x] Vector relationship indexing/query
- [x] Indexed property updates
- [x] Indexed entity deletion cleanup
- [x] Native effects WAL
- [x] CRC/sequence/torn-tail handling
- [x] WAL-before-publish durability ordering
- [x] Restart/recovery
- [x] Index DDL replay
- [x] Range-index query after restart
- [x] Full-text query after restart
- [x] Authoritative Windows integration CI
- [x] RESP/TCP network server
- [x] Redis-compatible password authentication
- [x] Multi-graph network catalog
- [x] Official `falkordb-py` client connectivity
- [x] Compact typed Node/Edge wire responses
- [x] Remote range/full-text index usage
- [x] Server-process restart + official-client recovery
- [x] Recovery for indexes created after existing data
- [x] Current FalkorDB Redis DUMP / RESTORE migration compatibility
- [x] Whole-database live migration with graph/UDF verification and rollback
- [x] Portable offline migration bundle with hashes and semantic verification
- [x] Official FalkorDB-generated DUMP import + hard-restart verification
- [x] Deployable Windows ZIP packaging

## Not required in standalone mode

- Redis
- Redis Modules API
- RediSearch
- Memurai
- Docker
- WSL
- Hyper-V / VM

## Remaining engineering, not feasibility blockers

- [x] compact snapshot/checkpoint format to cap WAL replay time
- [x] WAL rotation/compaction
- [ ] crash-injection test matrix
- [ ] long-running concurrency/stress suite
- [ ] index performance tuning
- [ ] vector index acceleration beyond correctness-first backend
- [ ] full-text ranking/tokenization parity tuning if exact upstream behavior is required
- [ ] installer/service registration (portable ZIP packaging is complete)
- [x] direct TLS listener and optional mTLS
- [ ] CAS artifact store
- [ ] universal reversal graph schema/migrations
- [ ] benchmark native backend vs original Redis/RediSearch deployment

## Definition of done for the Windows standalone proof

The proof is considered passed only when all of these execute on a real Windows
runner:

1. bootstrap native dependencies
2. cargo check FalkorDB graph crate
3. cargo check native host
4. run native host unit tests
5. link optimized `smoke.exe`
6. run core Cypher smoke
7. link optimized `indexed_smoke.exe`
8. run node + edge range/full-text/vector integration
9. mutate/delete indexed entities
10. write persistent indexed graph
11. destroy process-local graph
12. reopen from WAL
13. verify indexed range query
14. verify full-text query
15. emit `NATIVE_WINDOWS_FULL_STANDALONE_PASS`

The authoritative workflow additionally proves authenticated TCP/RESP access
with the official FalkorDB Python client before and after hard process restart.
