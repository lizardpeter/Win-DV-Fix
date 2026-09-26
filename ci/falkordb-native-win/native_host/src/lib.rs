//! Native non-Redis host for FalkorDB's `graph` crate.

pub mod api;
pub mod property_index;
pub mod range_index;
pub mod server;
pub mod native_config;
pub mod udf_store;
pub mod snapshot;
pub mod slowlog;
pub mod wal;
pub mod wire;

use std::{cell::Cell, ffi::c_void, path::Path, sync::Arc, time::{Duration, Instant}};

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

impl Drop for Engine {
    fn drop(&mut self) {
        graph::threadpool::shutdown();
        matrix::shutdown();
    }
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

#[derive(Default)]
struct PrelockedWriteEscalation {
    crossed_publication_boundary: Cell<bool>,
}

impl PrelockedWriteEscalation {
    fn crossed(&self) -> bool {
        self.crossed_publication_boundary.get()
    }
}

impl WriteEscalation for PrelockedWriteEscalation {
    fn upgrade_to_write(&self) -> Result<(), String> {
        self.crossed_publication_boundary.set(true);
        Ok(())
    }
}

pub struct NativeGraph {
    inner: RwLock<MvccGraph>,
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

    /// Install an upstream FalkorDB v19 GRAPH.RESTORE payload as a durable
    /// native graph. The import is a one-time conversion: after this returns,
    /// normal native checkpoints/WAL provide durability.
    pub fn restore_falkordb_v19_payload(
        name: &str,
        wal_path: impl AsRef<Path>,
        payload: &[u8],
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

        let graph = snapshot::load_falkordb_v19_payload(payload, name)?;
        if let Some(parent) = wal_path.parent()
            && !parent.as_os_str().is_empty()
        {
            std::fs::create_dir_all(parent)
                .map_err(|e| format!("create graph storage directory {}: {e}", parent.display()))?;
        }

        std::fs::File::create(&wal_path)
            .map_err(|e| format!("create imported graph WAL {}: {e}", wal_path.display()))?;

        // Sequence 0 means the checkpoint is the imported baseline and the
        // first native mutation starts at WAL sequence 1.
        let checkpoint = match snapshot::write_checkpoint(&wal_path, name, 0, &graph) {
            Ok(path) => path,
            Err(err) => {
                let _ = std::fs::remove_file(&wal_path);
                return Err(err);
            }
        };

        match Self::open_persistent(name, &wal_path) {
            Ok(graph) => Ok(graph),
            Err(err) => {
                let _ = std::fs::remove_file(&wal_path);
                let _ = std::fs::remove_file(&checkpoint);
                Err(err)
            }
        }
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
        // frames.
        wal.reset_after(sequence)?;
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


    pub fn query(&self, cypher: &str) -> Result<QueryOutput, String> {
        self.query_with_timeout(cypher, None)
    }

    pub fn query_with_timeout(
        &self,
        cypher: &str,
        per_query_timeout: Option<i64>,
    ) -> Result<QueryOutput, String> {
        let wall = Instant::now();
        let (snapshot, first_plan) = {
            let host_guard = self.inner.read();
            let snapshot = host_guard.read();
            let plan = {
                let graph_ref = snapshot.borrow();
                graph_ref.get_plan(cypher)?
            };
            (snapshot, plan)
        };

        let is_write = plan_is_write(&first_plan);
        let timeout_ms = native_config::effective_timeout(per_query_timeout, is_write)?;
        let result = if is_write {
            drop(snapshot);
            self.execute_write(cypher, timeout_ms)
        } else {
            self.execute_read(snapshot, first_plan, timeout_ms)
        };

        if result.is_ok() {
            self.slow_log
                .add("GRAPH.QUERY", cypher, wall.elapsed().as_secs_f64() * 1000.0);
        }
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
        let wall = Instant::now();
        let (snapshot, plan) = {
            let host_guard = self.inner.read();
            let snapshot = host_guard.read();
            let plan = {
                let graph_ref = snapshot.borrow();
                graph_ref.get_plan(cypher)?
            };
            (snapshot, plan)
        };

        if plan_is_write(&plan) {
            return Err("Read only query cannot perform writes".to_string());
        }

        let timeout_ms = native_config::effective_timeout(per_query_timeout, false)?;
        let result = self.execute_read(snapshot, plan, timeout_ms);
        if result.is_ok() {
            self.slow_log
                .add("GRAPH.RO_QUERY", cypher, wall.elapsed().as_secs_f64() * 1000.0);
        }
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
        let (snapshot, first_plan) = {
            let host_guard = self.inner.read();
            let snapshot = host_guard.read();
            let plan = {
                let graph_ref = snapshot.borrow();
                graph_ref.get_plan(cypher)?
            };
            (snapshot, plan)
        };

        let result = if plan_is_write(&first_plan) {
            drop(snapshot);
            self.execute_write_profile(cypher, native_config::effective_timeout(None, true)?)
        } else {
            self.execute_read_profile(
                snapshot,
                first_plan,
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

        let escalation = PrelockedWriteEscalation::default();
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
                let effects = runtime
                    .effects_buffer
                    .borrow_mut()
                    .take()
                    .ok_or_else(|| "native host: modified profile produced no effects buffer".to_string())?;

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

        let mut result = runtime.query()?;
        result.stats.cached = cached;
        Ok(capture_output(&runtime, &result))
    }

    fn execute_write(
        &self,
        cypher: &str,
        timeout_ms: Option<u64>,
    ) -> Result<QueryOutput, String> {
        let mut host_guard = self.inner.write();

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

        let escalation = PrelockedWriteEscalation::default();
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
        let output = capture_output(&runtime, &result);
        drop(result);

        if escalation.crossed() {
            // Durability order is WAL first, MVCC publication second. A crash
            // can therefore lose an unpublished private version, but can never
            // expose a graph mutation that was not durable.
            if modified
                && let Some(wal) = &self.wal
            {
                let effects = runtime
                    .effects_buffer
                    .borrow_mut()
                    .take()
                    .ok_or_else(|| "native host: modified write produced no effects buffer".to_string())?;

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

fn capture_output(runtime: &Runtime<'_>, result: &ResultSummary<'_>) -> QueryOutput {
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
                    wire_row.push(WireValue::capture(runtime, &value));
                } else {
                    row.push("Null".to_string());
                    wire_row.push(WireValue::Null);
                }
            }

            rows.push(row);
            wire_rows.push(wire_row);
        }
    }

    QueryOutput {
        columns,
        rows,
        wire_rows,
        stats: OutputStats::from(&result.stats),
        graph_version,
    }
}
