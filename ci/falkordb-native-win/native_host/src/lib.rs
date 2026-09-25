//! Native non-Redis host for FalkorDB's `graph` crate.

pub mod property_index;
pub mod range_index;
pub mod wal;
pub mod wire;

use std::{cell::Cell, ffi::c_void, path::Path, sync::Arc};

use graph::{
    effects::EffectsPayload,
    graph::{
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
use orx_tree::Collection;
use parking_lot::RwLock;
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
        let (wal, records) = Wal::open(wal_path)?;
        let mut mvcc = MvccGraph::new(16_384, 16_384, 25, name);

        if !records.is_empty() {
            let private = mvcc
                .write()
                .ok_or_else(|| "native host: failed to claim MVCC writer during recovery".to_string())?;

            {
                let mut graph = private.borrow_mut();
                for record in &records {
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
        }

        Ok(Self {
            inner: RwLock::new(mvcc),
            name: name.to_string(),
            wal: Some(wal),
            import_folder: String::new(),
            result_set_size: -1,
            timeout_ms: None,
        })
    }

    pub fn query(&self, cypher: &str) -> Result<QueryOutput, String> {
        let (snapshot, first_plan) = {
            let host_guard = self.inner.read();
            let snapshot = host_guard.read();
            let plan = {
                let graph_ref = snapshot.borrow();
                graph_ref.get_plan(cypher)?
            };
            (snapshot, plan)
        };

        if plan_is_write(&first_plan) {
            drop(snapshot);
            self.execute_write(cypher)
        } else {
            self.execute_read(snapshot, first_plan)
        }
    }

    /// Execute a query under the GRAPH.RO_QUERY contract.
    ///
    /// Write plans are rejected before runtime execution, matching FalkorDB's
    /// read-only network command semantics.
    pub fn query_read_only(&self, cypher: &str) -> Result<QueryOutput, String> {
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

        self.execute_read(snapshot, plan)
    }

    pub fn name(&self) -> &str {
        &self.name
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
    ) -> Result<QueryOutput, String> {
        let lock = ReadOnlyEscalation;
        let runtime = Runtime::new(
            snapshot,
            parameters,
            false,
            plan,
            false,
            self.import_folder.clone(),
            self.result_set_size,
            false,
            self.timeout_ms,
            0,
            None,
            &lock,
        );

        let mut result = runtime.query()?;
        result.stats.cached = cached;
        Ok(capture_output(&runtime, &result))
    }

    fn execute_write(&self, cypher: &str) -> Result<QueryOutput, String> {
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
            self.import_folder.clone(),
            self.result_set_size,
            false,
            self.timeout_ms,
            0,
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
