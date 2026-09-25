//! Native non-Redis host for FalkorDB's `graph` crate.

use std::{cell::Cell, ffi::c_void, sync::Arc};

use graph::{
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
use parking_lot::RwLock;

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
    pub rows: Vec<Vec<String>>,
    pub stats: OutputStats,
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
    import_folder: String,
    result_set_size: i64,
    timeout_ms: Option<u64>,
}

impl NativeGraph {
    pub fn new(name: &str) -> Self {
        Self {
            inner: RwLock::new(MvccGraph::new(16_384, 16_384, 25, name)),
            import_folder: String::new(),
            result_set_size: -1,
            timeout_ms: None,
        }
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

        #[cfg(feature = "redisearch_link_stubs")]
        if plan_requires_real_index(&first_plan) {
            return Err(
                "native Windows bring-up: this query requires the current RediSearch-backed index layer; native non-indexed Cypher is enabled, but index DDL/scans stay disabled until graph/src/index/falkordb is wired".to_string(),
            );
        }

        if plan_is_write(&first_plan) {
            drop(snapshot);
            self.execute_write(cypher)
        } else {
            self.execute_read(snapshot, first_plan)
        }
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

        #[cfg(feature = "redisearch_link_stubs")]
        if plan.iter().any(|n| {
            matches!(
                n,
                IR::CreateIndex { .. }
                    | IR::DropIndex { .. }
                    | IR::NodeByIndexScan { .. }
                    | IR::EdgeByIndexScan { .. }
                    | IR::NodeByFulltextScan { .. }
                    | IR::EdgeByFulltextScan { .. }
                    | IR::NodeByVectorScan { .. }
                    | IR::EdgeByVectorScan { .. }
            )
        }) {
            return Err(
                "native Windows bring-up: write replan requires the current RediSearch-backed index layer".to_string(),
            );
        }

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
        runtime.build_effects.set(false);

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
        let output = capture_output(&runtime, &result);

        if escalation.crossed() {
            host_guard.commit(Arc::clone(&private));
        } else {
            host_guard.rollback();
        }

        Ok(output)
    }
}

#[cfg(feature = "redisearch_link_stubs")]
fn plan_requires_real_index(plan: &Plan) -> bool {
    plan.plan.iter().any(|n| {
        matches!(
            n,
            IR::CreateIndex { .. }
                | IR::DropIndex { .. }
                | IR::NodeByIndexScan { .. }
                | IR::EdgeByIndexScan { .. }
                | IR::NodeByFulltextScan { .. }
                | IR::EdgeByFulltextScan { .. }
                | IR::NodeByVectorScan { .. }
                | IR::EdgeByVectorScan { .. }
        )
    })
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

    let mut rows = Vec::new();
    for batch in &result.result {
        for row_idx in batch.active_indices() {
            let row = runtime
                .return_names
                .iter()
                .map(|var| {
                    batch
                        .value_at(var.id, row_idx)
                        .map_or_else(|| "Null".to_string(), |v| format!("{v:?}"))
                })
                .collect();
            rows.push(row);
        }
    }

    QueryOutput {
        columns,
        rows,
        stats: OutputStats::from(&result.stats),
    }
}
