//! Native non-Redis host for FalkorDB's `graph` crate.

pub mod api;
pub mod bulk;
pub mod browser;
pub mod dashboard;
pub mod diagnostics;
pub mod file_import;
pub mod mcp;
mod oauth_grants;
pub mod property_index;
pub mod query_scheduler;
pub mod range_index;
pub mod rdb_file;
pub mod redis_dump;
pub mod server;
pub mod native_config;
pub mod udf_store;
pub mod snapshot;
pub mod slowlog;
pub mod wal;
pub mod wire;

use std::{ffi::c_void, path::Path, sync::Arc, time::{Duration, Instant}};

use graph::{
    effects::{
        EffectsBuffer, EffectsPayload,
        announce::{AnnouncedConstraint, SchemaBaseline},
    },
    entity_type::EntityType,
    graph::{
        constraint::{ConstraintStatus, ConstraintType},
        graph::{Plan, NODE_CREATION_BUFFER},
        graphblas::matrix,
        mvcc_graph::MvccGraph,
    },
    locks::WriteEscalation,
    planner::IR,
    runtime::{
        functions::{init_functions, init_udf_functions},
        runtime::{QueryStatistics, ResultSummary, Runtime},
    },
};
use orx_tree::{Collection, Dfs, NodeRef};
use parking_lot::RwLock;
use query_scheduler::{QueryMode, QueryPermit};
use slowlog::{SlowLog, SlowLogEntry};
use wal::Wal;
use wire::WireValue;

unsafe extern "C" {
    fn malloc(size: usize) -> *mut c_void;
    fn calloc(count: usize, size: usize) -> *mut c_void;
    fn realloc(ptr: *mut c_void, size: usize) -> *mut c_void;
    fn free(ptr: *mut c_void);
}

pub struct Engine {
    _private: (),
}

impl Engine {
    pub fn init() -> Result<Self, String> {
        matrix::init(Some(malloc), Some(calloc), Some(realloc), Some(free))?;
        init_functions()
            .map_err(|_| "FalkorDB built-in function registry was already initialized".to_string())?;
        init_udf_functions();
        graph::udf::init_udf_repo();
        graph::thread_id::set_main_thread();

        let threads = std::thread::available_parallelism()
            .map_or(4, std::num::NonZeroUsize::get);
        let _ = graph::threadpool::init_thread_pool(threads);

        NODE_CREATION_BUFFER.store(16_384, std::sync::atomic::Ordering::Relaxed);
        Ok(Self { _private: () })
    }
}

/// Initializes the engine once for the whole test binary. Tests that open a
/// `GraphCatalog` or run queries need the function and UDF registries; the
/// engine is process-global and deliberately never torn down here, because
/// tests run concurrently on several threads.
#[cfg(test)]
pub(crate) fn init_engine_for_tests() {
    static ENGINE: std::sync::OnceLock<()> = std::sync::OnceLock::new();
    ENGINE.get_or_init(|| {
        let engine = Engine::init().expect("engine init for tests");
        std::mem::forget(engine);
    });
}

impl Drop for Engine {
    fn drop(&mut self) {
        graph::threadpool::shutdown();
        matrix::shutdown();
    }
}

#[derive(Debug, Clone, Copy)]
pub struct SchemaSummary {
    pub labels: usize,
    pub relationship_types: usize,
    pub property_keys: usize,
    pub index_definitions: usize,
    pub constraints: usize,
    pub schema_version: u64,
}

#[derive(Debug)]
pub struct QueryOutput {
    pub columns: Vec<String>,
    /// Human-readable rows retained for diagnostics and internal smoke tests.
    pub rows: Vec<Vec<String>>,
    /// Typed values used by the RESP/FalkorDB network compatibility layer.
    pub wire_rows: Vec<Vec<WireValue>>,
    pub stats: OutputStats,
    pub graph_version: u64,
}

#[derive(Debug, Default)]
pub struct OutputStats {
    pub labels_added: usize,
    pub labels_removed: usize,
    pub nodes_created: u64,
    pub relationships_created: usize,
    pub nodes_deleted: u64,
    pub relationships_deleted: usize,
    pub properties_set: usize,
    pub properties_removed: usize,
    pub indexes_created: usize,
    pub indexes_dropped: usize,
    pub execution_time_ms: f64,
    pub cached: bool,
}

impl From<&QueryStatistics> for OutputStats {
    fn from(s: &QueryStatistics) -> Self {
        Self {
            labels_added: s.labels_added,
            labels_removed: s.labels_removed,
            nodes_created: s.nodes_created,
            relationships_created: s.relationships_created,
            nodes_deleted: s.nodes_deleted,
            relationships_deleted: s.relationships_deleted,
            properties_set: s.properties_set,
            properties_removed: s.properties_removed,
            indexes_created: s.indexes_created,
            indexes_dropped: s.indexes_dropped,
            execution_time_ms: s.execution_time,
            cached: s.cached,
        }
    }
}

struct ReadOnlyEscalation;

impl WriteEscalation for ReadOnlyEscalation {
    fn upgrade_to_write(&self) -> Result<(), String> {
        Err("native host: a read query attempted to escalate to write".to_string())
    }
}

/// Write-side escalation for a query that already holds the host's
/// exclusive MVCC writer lock.
///
/// The engine's index documents are shared, non-MVCC state: a write publishes
/// them (`commit_deferred_indexes`) before its new graph version is visible,
/// and expects the host to keep readers out from that point on (see
/// `graph::locks`). Readers here run on an MVCC snapshot *without* the host
/// lock, so escalation takes the graph's index-publication lock exclusively:
/// it waits for in-flight readers to finish, blocks new ones, and is held
/// until the write commits or rolls back (this value is dropped). Before
/// escalation — the match phase of a write — reads still run concurrently.
struct PrelockedWriteEscalation {
    publication: Arc<RwLock<()>>,
    held: std::cell::RefCell<Option<parking_lot::ArcRwLockWriteGuard<parking_lot::RawRwLock, ()>>>,
}

impl PrelockedWriteEscalation {
    fn new(publication: &Arc<RwLock<()>>) -> Self {
        Self {
            publication: Arc::clone(publication),
            held: std::cell::RefCell::new(None),
        }
    }

    fn crossed(&self) -> bool {
        self.held.borrow().is_some()
    }
}

impl WriteEscalation for PrelockedWriteEscalation {
    fn upgrade_to_write(&self) -> Result<(), String> {
        let mut held = self.held.borrow_mut();
        if held.is_none() {
            *held = Some(self.publication.write_arc());
        }
        Ok(())
    }
}

pub struct PreparedFalkorImport {
    graph: graph::graph::graph::Graph,
}

pub struct NativeGraph {
    inner: RwLock<MvccGraph>,
    /// Readers hold this shared while executing; an escalated write holds it
    /// exclusively while it publishes index documents and commits. See
    /// [`PrelockedWriteEscalation`].
    index_publication: Arc<RwLock<()>>,
    name: String,
    wal: Option<Wal>,
    import_folder: String,
    result_set_size: i64,
    timeout_ms: Option<u64>,
    slow_log: SlowLog,
}

impl NativeGraph {
    pub fn new(name: &str) -> Self {
        Self {
            inner: RwLock::new(MvccGraph::new(16_384, 16_384, 25, name)),
            index_publication: Arc::new(RwLock::new(())),
            name: name.to_string(),
            wal: None,
            import_folder: String::new(),
            result_set_size: -1,
            timeout_ms: None,
            slow_log: SlowLog::new(),
        }
    }

    /// Open a standalone graph backed by a durable effects WAL.
    ///
    /// Recovery replays FalkorDB's deterministic GRAPH.EFFECT payloads rather
    /// than re-running Cypher text, so transaction-time/random expressions are
    /// not re-evaluated after restart.
    pub fn open_persistent(
        name: &str,
        wal_path: impl AsRef<Path>,
    ) -> Result<Self, String> {
        let wal_path = wal_path.as_ref().to_path_buf();
        let loaded_snapshot = snapshot::load_latest(&wal_path, name)?;
        let has_snapshot = loaded_snapshot.is_some();
        let snapshot_sequence = loaded_snapshot
            .as_ref()
            .map_or(0, |snapshot| snapshot.sequence);

        let (wal, records) = Wal::open_after(&wal_path, snapshot_sequence)?;
        let mut mvcc = if let Some(snapshot) = loaded_snapshot {
            MvccGraph::from_graph(snapshot.graph)
        } else {
            MvccGraph::new(16_384, 16_384, 25, name)
        };

        let replay: Vec<_> = records
            .iter()
            .filter(|record| record.sequence > snapshot_sequence)
            .collect();

        if !replay.is_empty() {
            let private = mvcc
                .write()
                .ok_or_else(|| "native host: failed to claim MVCC writer during recovery".to_string())?;

            {
                let mut graph = private.borrow_mut();
                for record in replay {
                    if record.key.as_slice() != name.as_bytes() {
                        return Err(format!(
                            "WAL sequence {} belongs to graph {:?}, expected {:?}",
                            record.sequence,
                            String::from_utf8_lossy(&record.key),
                            name
                        ));
                    }
                    EffectsPayload::apply(&mut graph, &record.payload).map_err(|e| {
                        format!("WAL recovery failed at sequence {}: {e}", record.sequence)
                    })?;
                }
            }

            mvcc.commit(Arc::clone(&private));
            wait_for_recovered_indexes(&mvcc, Duration::from_secs(60))?;

            // Effects replay uses FalkorDB's normal asynchronous CREATE INDEX
            // path. Multiple index-DDL records for the same label can cause
            // generation-safe population workers to cancel one another; once
            // every worker has released its ticket (pending == 0), perform one
            // deterministic final population from the fully replayed graph.
            //
            // The native Windows index store is id-keyed and ADD_REPLACE
            // semantics make this pass idempotent: documents already populated
            // by a background worker are replaced, while any index whose worker
            // exited without populating is filled here. This keeps WAL recovery
            // deterministic without re-introducing the snapshot progress-counter
            // hang fixed by the pending-only readiness predicate.
            private.borrow_mut().populate_indexes_sync();
        } else if has_snapshot {
            // MvccGraph::from_graph does not perform a commit, so publish the
            // restored graph Arc into its indexers explicitly for subsequent
            // background index work. Sequence 0 is valid for an imported
            // upstream GRAPH.RESTORE payload, so key this on snapshot presence
            // rather than sequence > 0.
            let committed = mvcc.read();
            committed.borrow().set_indexer_graph(Arc::clone(&committed));
        }

        Ok(Self {
            inner: RwLock::new(mvcc),
            index_publication: Arc::new(RwLock::new(())),
            name: name.to_string(),
            wal: Some(wal),
            import_folder: String::new(),
            result_set_size: -1,
            timeout_ms: None,
            slow_log: SlowLog::new(),
        })
    }

    /// Export the current committed graph in FalkorDB's upstream v19
    /// single-key GRAPH.RESTORE payload format.
    #[must_use]
    pub fn export_falkordb_v19_payload(&self) -> Vec<u8> {
        let host_guard = self.inner.read();
        let committed = host_guard.read();
        let graph = committed.borrow();
        snapshot::save_falkordb_v19_payload(&graph, &self.name)
    }

    /// Fully decode and validate an upstream FalkorDB v19 payload without
    /// touching destination persistence. RESTORE REPLACE uses this phase so a
    /// corrupt-but-well-framed Redis DUMP cannot destroy the existing graph.
    pub fn prepare_falkordb_v19_payload(
        name: &str,
        payload: &[u8],
    ) -> Result<PreparedFalkorImport, String> {
        Ok(PreparedFalkorImport {
            graph: snapshot::load_falkordb_v19_payload(payload, name)?,
        })
    }

    /// Persist a graph that has already passed the complete v19 decoder.
    pub fn restore_prepared_falkordb(
        name: &str,
        wal_path: impl AsRef<Path>,
        prepared: PreparedFalkorImport,
    ) -> Result<Self, String> {
        let wal_path = wal_path.as_ref().to_path_buf();

        if wal_path.exists() {
            return Err(format!(
                "destination graph storage already exists: {}",
                wal_path.display()
            ));
        }
        if snapshot::load_latest(&wal_path, name)?.is_some() {
            return Err(format!(
                "destination graph checkpoint already exists for {name:?}"
            ));
        }

        let graph = prepared.graph;
        if let Some(parent) = wal_path.parent()
            && !parent.as_os_str().is_empty()
        {
            std::fs::create_dir_all(parent)
                .map_err(|e| format!("create graph storage directory {}: {e}", parent.display()))?;
        }

        // Sequence 0 means the checkpoint is the imported baseline and the
        // first native mutation starts at WAL sequence 1. The checkpoint is
        // published before the WAL is created, so a crash in between leaves
        // a complete graph (startup finds it by its checkpoint) rather than
        // an empty WAL that makes the name look taken.
        let checkpoint = snapshot::write_checkpoint(&wal_path, name, 0, &graph)?;
        if let Err(err) = std::fs::File::create(&wal_path) {
            let _ = std::fs::remove_file(&checkpoint);
            return Err(format!("create imported graph WAL {}: {e}", wal_path.display(), e = err));
        }

        match Self::open_persistent(name, &wal_path) {
            Ok(graph) => Ok(graph),
            Err(err) => {
                let _ = std::fs::remove_file(&wal_path);
                let _ = std::fs::remove_file(&checkpoint);
                Err(err)
            }
        }
    }

    /// Install an upstream FalkorDB v19 GRAPH.RESTORE payload as a durable
    /// native graph. The import is a one-time conversion: after this returns,
    /// normal native checkpoints/WAL provide durability.
    pub fn restore_falkordb_v19_payload(
        name: &str,
        wal_path: impl AsRef<Path>,
        payload: &[u8],
    ) -> Result<Self, String> {
        let prepared = Self::prepare_falkordb_v19_payload(name, payload)?;
        Self::restore_prepared_falkordb(name, wal_path, prepared)
    }


    /// Create a durable full-graph checkpoint and compact all WAL history that
    /// the checkpoint contains. The graph stays query-consistent throughout:
    /// active readers may finish on their Arc snapshot while new writes wait.
    pub fn checkpoint(&self) -> Result<std::path::PathBuf, String> {
        let wal = self
            .wal
            .as_ref()
            .ok_or_else(|| "native host: checkpoint requires a persistent graph".to_string())?;

        let host_guard = self.inner.write();
        let committed = host_guard.read();
        let sequence = wal.last_sequence();

        let path = {
            let graph = committed.borrow();
            snapshot::write_checkpoint(wal.path(), &self.name, sequence, &graph)?
        };

        // The checkpoint is durable before any WAL bytes are discarded. A
        // crash before this line leaves the old full WAL; a crash after it can
        // recover from the checkpoint plus any subsequent absolute-sequence
        // frames. Recovery accepts a WAL that still overlaps the checkpoint,
        // so a failed truncation only costs disk space and is retried by the
        // next checkpoint — it must not be reported as a failed checkpoint.
        if let Err(err) = wal.reset_after(sequence) {
            eprintln!(
                "FalkorDB native checkpoint {:?}: checkpoint {sequence} is durable but WAL compaction failed (will retry next checkpoint): {err}",
                self.name
            );
        }
        snapshot::cleanup_old_checkpoints(wal.path(), 2)?;

        Ok(path)
    }


    pub fn wal_len(&self) -> Result<u64, String> {
        let wal = self
            .wal
            .as_ref()
            .ok_or_else(|| "native host: WAL length requires a persistent graph".to_string())?;
        std::fs::metadata(wal.path())
            .map(|meta| meta.len())
            .map_err(|e| format!("stat WAL {}: {e}", wal.path().display()))
    }


    /// Constant-time live graph cardinalities and version, read from the
    /// committed graph snapshot without planning or executing Cypher.
    pub fn cardinality_and_version(&self) -> (u64, u64, u64) {
        let host_guard = self.inner.read();
        let committed = host_guard.read();
        let graph = committed.borrow();
        (graph.node_count(), graph.relationship_count(), graph.version)
    }

    /// Lightweight schema cardinalities from the committed graph snapshot.
    /// This avoids planning multiple db.* introspection procedures merely to
    /// populate status/monitoring surfaces.
    pub fn schema_summary(&self) -> SchemaSummary {
        let host_guard = self.inner.read();
        let committed = host_guard.read();
        let graph = committed.borrow();
        SchemaSummary {
            labels: graph.get_labels().len(),
            relationship_types: graph.get_types().len(),
            property_keys: graph.build_global_attrs().len(),
            index_definitions: graph.index_info().len(),
            constraints: graph.constraints().len(),
            schema_version: graph.schema_version,
        }
    }

    pub fn memory_usage_report(
        &self,
        samples: usize,
    ) -> graph::graph::graph::MemoryUsageReport {
        let host_guard = self.inner.read();
        let committed = host_guard.read();
        committed.borrow().memory_usage_report(samples)
    }

    /// Apply an upstream-compatible GRAPH.BULK batch with a durable checkpoint boundary.
    pub fn bulk_insert(&self, request: &bulk::BulkRequest) -> Result<String, String> {
        let _permit = QueryPermit::acquire(&self.name, "GRAPH.BULK", QueryMode::Write)?;
        let mut host_guard = self.inner.write();
        release_claim_on_error(&mut host_guard, |host_guard| {
            self.bulk_insert_locked(host_guard, request)
        })
    }

    fn bulk_insert_locked(
        &self,
        host_guard: &mut MvccGraph,
        request: &bulk::BulkRequest,
    ) -> Result<String, String> {
        let private = host_guard
            .write()
            .ok_or_else(|| "native host: another MVCC write is in progress".to_string())?;

        let mut docs = {
            let mut graph = private.borrow_mut();
            match bulk::apply(&mut graph, request) {
                Ok(docs) => docs,
                Err(err) => {
                    drop(graph);
                    host_guard.rollback();
                    return Err(format!("ERR bulk insert failed: {err}"));
                }
            }
        };

        let mut checkpoint_sequence = None;
        if let Some(wal) = &self.wal {
            let sequence = wal.last_sequence().saturating_add(1);
            let staged = {
                let graph = private.borrow();
                match snapshot::stage_checkpoint(wal.path(), &self.name, sequence, &graph) {
                    Ok(staged) => staged,
                    Err(err) => {
                        drop(graph);
                        host_guard.rollback();
                        return Err(err);
                    }
                }
            };

            let marker_sequence = match wal.append_checkpoint_marker(self.name.as_bytes()) {
                Ok(sequence) => sequence,
                Err(err) => {
                    snapshot::abort_staged_checkpoint(staged);
                    host_guard.rollback();
                    return Err(err);
                }
            };
            debug_assert_eq!(marker_sequence, sequence);

            if let Err(err) = snapshot::publish_staged_checkpoint(staged) {
                host_guard.rollback();
                return Err(err);
            }
            checkpoint_sequence = Some(sequence);
        }

        {
            // Publishing index documents mutates the shared index: exclude
            // snapshot readers until the new version is committed.
            let _publication = self.index_publication.write();
            let mut graph = private.borrow_mut();
            docs.publish(&mut graph);
            drop(graph);
            host_guard.commit(Arc::clone(&private));
        }

        if let (Some(wal), Some(sequence)) = (&self.wal, checkpoint_sequence) {
            if let Err(err) = wal.reset_after(sequence) {
                eprintln!("FalkorDB native GRAPH.BULK: WAL compaction after checkpoint failed: {err}");
            } else if let Err(err) = snapshot::cleanup_old_checkpoints(wal.path(), 2) {
                eprintln!("FalkorDB native GRAPH.BULK: old checkpoint cleanup failed: {err}");
            }
        }

        Ok(format!(
            "{} nodes created, {} relations created",
            request.node_count, request.edge_count
        ))
    }

    pub fn query(&self, cypher: &str) -> Result<QueryOutput, String> {
        self.query_with_timeout(cypher, None)
    }

    pub fn query_with_timeout(
        &self,
        cypher: &str,
        per_query_timeout: Option<i64>,
    ) -> Result<QueryOutput, String> {
        let mut breadcrumb = diagnostics::start_query(&self.name, cypher, "QUERY");
        let wall = Instant::now();
        let is_write = {
            let host_guard = self.inner.read();
            let snapshot = host_guard.read();
            let plan = {
                let graph_ref = snapshot.borrow();
                graph_ref.get_plan(cypher)?
            };
            plan_is_write(&plan)
        };

        let timeout_ms = native_config::effective_timeout(per_query_timeout, is_write)?;
        let result = if is_write {
            let _permit = QueryPermit::acquire(&self.name, cypher, QueryMode::Write)?;
            self.execute_write(cypher, timeout_ms)
        } else {
            let _permit = QueryPermit::acquire(&self.name, cypher, QueryMode::Read)?;
            let (snapshot, plan) = {
                let host_guard = self.inner.read();
                let snapshot = host_guard.read();
                let plan = {
                    let graph_ref = snapshot.borrow();
                    graph_ref.get_plan(cypher)?
                };
                (snapshot, plan)
            };
            self.execute_read(snapshot, plan, timeout_ms)
        };

        if result.is_ok() {
            self.slow_log
                .add("GRAPH.QUERY", cypher, wall.elapsed().as_secs_f64() * 1000.0);
        }
        breadcrumb.complete(result.is_ok());
        result
    }

    /// Execute a query under the GRAPH.RO_QUERY contract.
    ///
    /// Write plans are rejected before runtime execution, matching FalkorDB's
    /// read-only network command semantics.
    pub fn query_read_only(&self, cypher: &str) -> Result<QueryOutput, String> {
        self.query_read_only_with_timeout(cypher, None)
    }

    pub fn query_read_only_with_timeout(
        &self,
        cypher: &str,
        per_query_timeout: Option<i64>,
    ) -> Result<QueryOutput, String> {
        let mut breadcrumb = diagnostics::start_query(&self.name, cypher, "READ");
        let wall = Instant::now();
        {
            let host_guard = self.inner.read();
            let snapshot = host_guard.read();
            let plan = {
                let graph_ref = snapshot.borrow();
                graph_ref.get_plan(cypher)?
            };
            if plan_is_write(&plan) {
                return Err("Read only query cannot perform writes".to_string());
            }
        }

        let _permit = QueryPermit::acquire(&self.name, cypher, QueryMode::Read)?;
        let (snapshot, plan) = {
            let host_guard = self.inner.read();
            let snapshot = host_guard.read();
            let plan = {
                let graph_ref = snapshot.borrow();
                graph_ref.get_plan(cypher)?
            };
            (snapshot, plan)
        };
        debug_assert!(!plan_is_write(&plan));
        let timeout_ms = native_config::effective_timeout(per_query_timeout, false)?;
        let result = self.execute_read(snapshot, plan, timeout_ms);
        if result.is_ok() {
            self.slow_log
                .add("GRAPH.RO_QUERY", cypher, wall.elapsed().as_secs_f64() * 1000.0);
        }
        breadcrumb.complete(result.is_ok());
        result
    }

    pub fn schema_version(&self) -> u64 {
        let host_guard = self.inner.read();
        let committed = host_guard.read();
        committed.borrow().schema_version
    }

    pub fn slowlog_entries(&self) -> Vec<SlowLogEntry> {
        self.slow_log.entries()
    }

    pub fn slowlog_reset(&self) {
        self.slow_log.reset();
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn copy_persistent(
        &self,
        destination_name: &str,
        destination_wal: impl AsRef<Path>,
    ) -> Result<Self, String> {
        let destination_wal = destination_wal.as_ref();
        if destination_wal.exists()
            && std::fs::metadata(destination_wal)
                .map_err(|e| format!("inspect destination WAL {}: {e}", destination_wal.display()))?
                .len()
                != 0
        {
            return Err("destination key already exists".to_string());
        }

        let source_wal = self
            .wal
            .as_ref()
            .ok_or_else(|| "native host: GRAPH.COPY requires a persistent source graph".to_string())?;
        let sequence = source_wal.last_sequence();

        // Copy the current committed graph, not merely the current WAL. The WAL
        // may have been compacted after a checkpoint, so its remaining frames
        // are not necessarily the graph's full history.
        let host_guard = self.inner.read();
        let committed = host_guard.read();
        {
            let graph = committed.borrow();
            snapshot::write_checkpoint(
                destination_wal,
                destination_name,
                sequence,
                &graph,
            )?;
        }
        drop(committed);
        drop(host_guard);

        Self::open_persistent(destination_name, destination_wal)
    }


    pub fn mutate_constraint(
        &self,
        create: bool,
        constraint_type: ConstraintType,
        entity_type: EntityType,
        label: &str,
        properties: &[String],
    ) -> Result<(), String> {
        if properties.is_empty() {
            return Err("constraint must include at least one property".to_string());
        }

        let mut host_guard = self.inner.write();
        release_claim_on_error(
            &mut host_guard,
            |host_guard| {
                self.mutate_constraint_locked(
                    host_guard,
                    create,
                    constraint_type,
                    entity_type,
                    label,
                    properties,
                )
            },
        )
    }

    fn mutate_constraint_locked(
        &self,
        host_guard: &mut MvccGraph,
        create: bool,
        constraint_type: ConstraintType,
        entity_type: EntityType,
        label: &str,
        properties: &[String],
    ) -> Result<(), String> {
        // Constraint changes can build or validate against the shared index;
        // keep snapshot readers out of it until this commits or rolls back.
        let _publication = self.index_publication.write();
        let private = host_guard
            .write()
            .ok_or_else(|| "native host: another MVCC write is in progress".to_string())?;

        let label_arc = Arc::new(label.to_string());
        let property_arcs: Vec<Arc<String>> = properties
            .iter()
            .cloned()
            .map(Arc::new)
            .collect();

        let mut graph = private.borrow_mut();
        let baseline = SchemaBaseline::of(&graph);

        let status = if create {
            graph.create_constraint(
                constraint_type,
                entity_type,
                &label_arc,
                &property_arcs,
            )?;

            // The Redis host settles large constraints asynchronously. The
            // standalone host validates them before publishing so a successful
            // command never leaves an unenforced constraint stranded.
            graph.validate_pending_constraints();

            match entity_type {
                EntityType::Node => {
                    graph.get_label_id_mut(&label_arc);
                }
                EntityType::Relationship => {
                    graph.get_type_id_mut(&label_arc);
                }
            }
            for property in &property_arcs {
                graph.add_node_attribute_name(property);
            }

            graph
                .constraints()
                .iter()
                .find(|constraint| {
                    constraint.matches(
                        &constraint_type,
                        &entity_type,
                        label,
                        &property_arcs,
                    )
                })
                .map(|constraint| constraint.status)
                .ok_or_else(|| "constraint creation did not produce a constraint".to_string())?
        } else {
            graph.drop_constraint(
                &constraint_type,
                &entity_type,
                label,
                &property_arcs,
            )?;
            ConstraintStatus::Operational
        };

        let mut effects = EffectsBuffer::new();
        effects.build_constraint(
            &graph,
            create,
            &AnnouncedConstraint {
                ct: constraint_type,
                entity_type,
                status: create.then_some(status),
                label,
                properties: &property_arcs,
            },
            &baseline,
        )?;
        drop(graph);

        if let Some(wal) = &self.wal {
            if let Err(err) = wal.append_effects(effects, self.name.as_bytes()) {
                host_guard.rollback();
                return Err(err);
            }
        }

        host_guard.commit(Arc::clone(&private));
        Ok(())
    }


    pub fn explain(&self, cypher: &str) -> Result<Vec<String>, String> {
        let host_guard = self.inner.read();
        let snapshot = host_guard.read();
        let plan = {
            let graph_ref = snapshot.borrow();
            graph_ref.get_plan(cypher)?.plan
        };

        Ok(plan
            .root()
            .indices::<Dfs>()
            .map(|idx| {
                let node = plan.node(idx);
                format!("{}{}", " ".repeat(node.depth() * 4), node.data())
            })
            .collect())
    }

    pub fn profile(&self, cypher: &str) -> Result<Vec<String>, String> {
        let wall = Instant::now();
        let is_write = {
            let host_guard = self.inner.read();
            let snapshot = host_guard.read();
            let plan = {
                let graph_ref = snapshot.borrow();
                graph_ref.get_plan(cypher)?
            };
            plan_is_write(&plan)
        };

        let result = if is_write {
            let _permit = QueryPermit::acquire(&self.name, cypher, QueryMode::Write)?;
            self.execute_write_profile(cypher, native_config::effective_timeout(None, true)?)
        } else {
            let _permit = QueryPermit::acquire(&self.name, cypher, QueryMode::Read)?;
            let (snapshot, plan) = {
                let host_guard = self.inner.read();
                let snapshot = host_guard.read();
                let plan = {
                    let graph_ref = snapshot.borrow();
                    graph_ref.get_plan(cypher)?
                };
                (snapshot, plan)
            };
            self.execute_read_profile(
                snapshot,
                plan,
                native_config::effective_timeout(None, false)?,
            )
        };

        if result.is_ok() {
            self.slow_log
                .add("GRAPH.PROFILE", cypher, wall.elapsed().as_secs_f64() * 1000.0);
        }
        result
    }

    fn execute_read_profile(
        &self,
        snapshot: Arc<atomic_refcell::AtomicRefCell<graph::graph::graph::Graph>>,
        Plan {
            plan,
            parameters,
            ..
        }: Plan,
        timeout_ms: Option<u64>,
    ) -> Result<Vec<String>, String> {
        let lock = ReadOnlyEscalation;
        // Keep an escalated writer from publishing index documents while this
        // snapshot reader may scan the shared index.
        let _publication = self.index_publication.read();
        let runtime = Runtime::new(
            snapshot,
            parameters,
            false,
            plan,
            false,
            native_config::runtime_config().import_folder,
            native_config::runtime_config().result_set_size,
            true,
            timeout_ms,
            native_config::runtime_config().query_mem_capacity,
            None,
            &lock,
        );
        let _ = runtime.query()?;
        Ok(format_profile(&runtime))
    }

    fn execute_write_profile(
        &self,
        cypher: &str,
        timeout_ms: Option<u64>,
    ) -> Result<Vec<String>, String> {
        let mut host_guard = self.inner.write();
        release_claim_on_error(&mut host_guard, |host_guard| {
            self.execute_write_profile_locked(host_guard, cypher, timeout_ms)
        })
    }

    fn execute_write_profile_locked(
        &self,
        host_guard: &mut MvccGraph,
        cypher: &str,
        timeout_ms: Option<u64>,
    ) -> Result<Vec<String>, String> {

        let Plan {
            plan,
            parameters,
            ..
        } = {
            let committed = host_guard.read();
            let graph_ref = committed.borrow();
            graph_ref.get_plan(cypher)?
        };

        let private = host_guard
            .write()
            .ok_or_else(|| "native host: another MVCC write is in progress".to_string())?;

        let escalation = PrelockedWriteEscalation::new(&self.index_publication);
        let runtime = Runtime::new(
            Arc::clone(&private),
            parameters,
            true,
            plan,
            false,
            native_config::runtime_config().import_folder,
            native_config::runtime_config().result_set_size,
            true,
            timeout_ms,
            native_config::runtime_config().query_mem_capacity,
            None,
            &escalation,
        );
        runtime.build_effects.set(self.wal.is_some());

        let result = match runtime.query() {
            Ok(result) => result,
            Err(err) => {
                if escalation.crossed() {
                    let committed = host_guard.read();
                    runtime.resync_published_indexes(&committed);
                }
                host_guard.rollback();
                return Err(err);
            }
        };

        let modified = query_modified(&runtime, &result.stats);
        let lines = format_profile(&runtime);
        drop(result);

        if escalation.crossed() {
            if modified
                && let Some(wal) = &self.wal
            {
                // Nothing reached the effects stream (e.g. `OPTIONAL MATCH
                // (n:Missing) SET n:L` only registers a label name). The WAL
                // would hold nothing, so publishing the private version would
                // leave memory ahead of disk; discard it instead. The graph's
                // data is unchanged either way.
                let Some(effects) = runtime.effects_buffer.borrow_mut().take() else {
                    let committed = host_guard.read();
                    runtime.resync_published_indexes(&committed);
                    drop(committed);
                    host_guard.rollback();
                    return Ok(lines);
                };

                if let Err(err) = wal.append_effects(effects, self.name.as_bytes()) {
                    let committed = host_guard.read();
                    runtime.resync_published_indexes(&committed);
                    drop(committed);
                    host_guard.rollback();
                    return Err(err);
                }
            }
            host_guard.commit(Arc::clone(&private));
        } else {
            host_guard.rollback();
        }

        Ok(lines)
    }

    fn execute_read(
        &self,
        snapshot: Arc<atomic_refcell::AtomicRefCell<graph::graph::graph::Graph>>,
        Plan {
            plan,
            cached,
            parameters,
            ..
        }: Plan,
        timeout_ms: Option<u64>,
    ) -> Result<QueryOutput, String> {
        let lock = ReadOnlyEscalation;
        // Keep an escalated writer from publishing index documents while this
        // snapshot reader may scan the shared index.
        let _publication = self.index_publication.read();
        let runtime = Runtime::new(
            snapshot,
            parameters,
            false,
            plan,
            false,
            native_config::runtime_config().import_folder,
            native_config::runtime_config().result_set_size,
            false,
            timeout_ms,
            native_config::runtime_config().query_mem_capacity,
            None,
            &lock,
        );

        diagnostics::mark_stage("runtime_read");
        let mut result = runtime.query()?;
        result.stats.cached = cached;
        diagnostics::mark_stage("capture_read");
        capture_output(&runtime, &result)
    }

    fn execute_write(
        &self,
        cypher: &str,
        timeout_ms: Option<u64>,
    ) -> Result<QueryOutput, String> {
        let mut host_guard = self.inner.write();
        release_claim_on_error(&mut host_guard, |host_guard| {
            self.execute_write_locked(host_guard, cypher, timeout_ms)
        })
    }

    fn execute_write_locked(
        &self,
        host_guard: &mut MvccGraph,
        cypher: &str,
        timeout_ms: Option<u64>,
    ) -> Result<QueryOutput, String> {

        let Plan {
            plan,
            cached,
            parameters,
            ..
        } = {
            let committed = host_guard.read();
            let graph_ref = committed.borrow();
            graph_ref.get_plan(cypher)?
        };

        debug_assert!(plan.iter().any(|n| matches!(
            n,
            IR::Commit | IR::CreateIndex { .. } | IR::DropIndex { .. }
        )));

        let private = host_guard
            .write()
            .ok_or_else(|| "native host: another MVCC write is in progress".to_string())?;

        let escalation = PrelockedWriteEscalation::new(&self.index_publication);
        let runtime = Runtime::new(
            Arc::clone(&private),
            parameters,
            true,
            plan,
            false,
            native_config::runtime_config().import_folder,
            native_config::runtime_config().result_set_size,
            false,
            timeout_ms,
            native_config::runtime_config().query_mem_capacity,
            None,
            &escalation,
        );
        runtime.build_effects.set(self.wal.is_some());

        diagnostics::mark_stage("runtime_write");
        let mut result = match runtime.query() {
            Ok(result) => result,
            Err(err) => {
                if escalation.crossed() {
                    let committed = host_guard.read();
                    runtime.resync_published_indexes(&committed);
                }
                host_guard.rollback();
                return Err(err);
            }
        };

        result.stats.cached = cached;
        let modified = query_modified(&runtime, &result.stats);
        diagnostics::mark_stage("capture_write");
        let output = match capture_output(&runtime, &result) {
            Ok(output) => output,
            Err(err) => {
                if escalation.crossed() {
                    let committed = host_guard.read();
                    runtime.resync_published_indexes(&committed);
                }
                host_guard.rollback();
                return Err(err);
            }
        };
        drop(result);
        diagnostics::mark_stage("commit_write");

        if escalation.crossed() {
            // Durability order is WAL first, MVCC publication second. A crash
            // can therefore lose an unpublished private version, but can never
            // expose a graph mutation that was not durable.
            if modified
                && let Some(wal) = &self.wal
            {
                // Nothing reached the effects stream (e.g. `OPTIONAL MATCH
                // (n:Missing) SET n:L` only registers a label name). The WAL
                // would hold nothing, so publishing the private version would
                // leave memory ahead of disk; discard it instead. The graph's
                // data is unchanged either way.
                let Some(effects) = runtime.effects_buffer.borrow_mut().take() else {
                    let committed = host_guard.read();
                    runtime.resync_published_indexes(&committed);
                    drop(committed);
                    host_guard.rollback();
                    return Ok(output);
                };

                if let Err(err) = wal.append_effects(effects, self.name.as_bytes()) {
                    let committed = host_guard.read();
                    runtime.resync_published_indexes(&committed);
                    drop(committed);
                    host_guard.rollback();
                    return Err(err);
                }
            }

            host_guard.commit(Arc::clone(&private));
        } else {
            host_guard.rollback();
        }

        Ok(output)
    }
}


fn format_profile(runtime: &Runtime<'_>) -> Vec<String> {
    let plan = &runtime.plan;
    let all_ops: Vec<_> = plan.root().indices::<Dfs>().collect();
    let profile_data = runtime.profile_data.borrow();

    all_ops
        .into_iter()
        .filter(|idx| !matches!(plan.node(*idx).data(), IR::Commit))
        .map(|idx| {
            let node = plan.node(idx);
            let mut depth = node.depth();
            let mut cur = idx;
            while let Some(parent) = plan.node(cur).parent() {
                if matches!(parent.data(), IR::Commit) {
                    depth = depth.saturating_sub(1);
                }
                cur = parent.idx();
            }

            let (records, time) = profile_data
                .get(&idx)
                .copied()
                .unwrap_or((0, Duration::ZERO));
            format!(
                "{}{} | Records produced: {records}, Execution time: {:.6} ms",
                "    ".repeat(depth),
                node.data(),
                time.as_secs_f64() * 1000.0
            )
        })
        .collect()
}

fn wait_for_recovered_indexes(
    mvcc: &MvccGraph,
    timeout: Duration,
) -> Result<(), String> {
    let deadline = Instant::now() + timeout;

    loop {
        let snapshot = mvcc.read();
        let infos = snapshot.borrow().index_info();

        // FalkorDB's actual operational predicate is the generation-scoped
        // pending counter. The progress/total pair is informational metadata:
        // synchronous RDB/checkpoint population can finish with pending == 0
        // without advancing that counter, and sparse indexed properties can
        // legitimately produce fewer documents than the label cardinality.
        //
        // Treating progress < total as "not recovered" therefore wedges a
        // correctly rebuilt checkpoint forever (for example pending=0,
        // progress=0/1). Waiting on pending preserves the same readiness
        // contract used by Indexer::is_operational()/enabled().
        let incomplete: Vec<String> = infos
            .iter()
            .filter(|info| info.pending > 0)
            .map(|info| {
                format!(
                    "{}:{} pending={} progress={}/{}",
                    info.entity_type, info.label, info.pending, info.progress, info.total
                )
            })
            .collect();

        if incomplete.is_empty() {
            return Ok(());
        }

        if Instant::now() >= deadline {
            return Err(format!(
                "native host: timed out waiting for recovered index population: {}",
                incomplete.join(", ")
            ));
        }

        // Drop the graph snapshot before sleeping so population workers can
        // borrow and update their index stores without unnecessary retention.
        drop(snapshot);
        std::thread::sleep(Duration::from_millis(2));
    }
}

/// Runs a write body that may claim the MVCC writer, and clears that claim if
/// the body returns an error by any path.
///
/// `MvccGraph::write()` sets a flag that only `commit` or `rollback` clears;
/// an early `?` that skipped both used to leave the graph rejecting every
/// later write ("another MVCC write is in progress") until restart. The caller
/// holds the host's exclusive lock for the whole body, so no other writer can
/// have claimed the flag in between, and `rollback` is idempotent — calling it
/// after a body that already rolled back is a no-op.
///
/// A panic inside the body (an engine bug) is handled the same way and turned
/// into an error: the half-built private version is discarded and the graph
/// stays writable, instead of the connection dying with the claim held.
fn release_claim_on_error<T>(
    host_guard: &mut MvccGraph,
    body: impl FnOnce(&mut MvccGraph) -> Result<T, String>,
) -> Result<T, String> {
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| body(host_guard)))
        .unwrap_or_else(|panic| {
            let detail = panic
                .downcast_ref::<&str>()
                .map(|s| (*s).to_string())
                .or_else(|| panic.downcast_ref::<String>().cloned())
                .unwrap_or_else(|| "unknown panic".to_string());
            Err(format!("internal error during write (changes discarded): {detail}"))
        });
    if result.is_err() {
        host_guard.rollback();
    }
    result
}

fn query_modified(
    runtime: &Runtime<'_>,
    stats: &QueryStatistics,
) -> bool {
    // Same predicate used by FalkorDB's Redis host when deciding whether a
    // write query produced durable/replicable state.
    stats.nodes_created > 0
        || stats.nodes_deleted > 0
        || stats.relationships_created > 0
        || stats.relationships_deleted > 0
        || stats.properties_set > 0
        || stats.properties_removed > 0
        || stats.labels_added > 0
        || stats.labels_removed > 0
        || stats.indexes_created > 0
        || stats.indexes_dropped > 0
        || runtime.effects_count.get() > 0
}

fn plan_is_write(plan: &Plan) -> bool {
    plan.plan.iter().any(|n| {
        matches!(
            n,
            IR::Commit | IR::CreateIndex { .. } | IR::DropIndex { .. }
        )
    })
}

fn capture_output(
    runtime: &Runtime<'_>,
    result: &ResultSummary<'_>,
) -> Result<QueryOutput, String> {
    let columns: Vec<String> = runtime
        .return_names
        .iter()
        .map(ToString::to_string)
        .collect();

    let graph_version = runtime.g.borrow().version;
    let mut rows = Vec::new();
    let mut wire_rows = Vec::new();

    for batch in &result.result {
        for row_idx in batch.active_indices() {
            let mut row = Vec::with_capacity(runtime.return_names.len());
            let mut wire_row = Vec::with_capacity(runtime.return_names.len());

            for var in &runtime.return_names {
                if let Some(value) = batch.value_at(var.id, row_idx) {
                    row.push(format!("{value:?}"));
                    wire_row.push(WireValue::capture(runtime, &value)?);
                } else {
                    row.push("Null".to_string());
                    wire_row.push(WireValue::Null);
                }
            }

            rows.push(row);
            wire_rows.push(wire_row);
        }
    }

    Ok(QueryOutput {
        columns,
        rows,
        wire_rows,
        stats: OutputStats::from(&result.stats),
        graph_version,
    })
}