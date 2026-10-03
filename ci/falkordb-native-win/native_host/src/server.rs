use std::{
    collections::HashMap,
    env,
    fs::{self, File},
    io::{BufRead, BufReader, Read, Write},
    net::{SocketAddr, TcpListener},
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    thread,
    time::Duration,
};

use rustls::{
    RootCertStore, ServerConfig as RustlsServerConfig, ServerConnection, StreamOwned,
    server::WebPkiClientVerifier,
};

use parking_lot::RwLock;
use subtle::ConstantTimeEq;
use graph::{entity_type::EntityType, graph::constraint::ConstraintType};

use crate::{
    NativeGraph, OutputStats, PreparedFalkorImport, QueryOutput, file_import, native_config, query_scheduler, rdb_file, redis_dump, snapshot, udf_store, wire::WireValue,
};

const MAX_RESP_LINE_BYTES: usize = 64 * 1024;
const MAX_RESP_ARRAY_ITEMS: usize = 100_000;
const MAX_RESP_BULK_BYTES: usize = 512 * 1024 * 1024;
const MAX_RESP_COMMAND_BYTES: usize = 768 * 1024 * 1024;
const MAX_RESP_CONNECTIONS: usize = 256;
const RESP_IO_TIMEOUT_SECS: u64 = 120;
const MAX_AUTH_FAILURES_PER_CONNECTION: u8 = 10;

struct RespConnectionPermit(Arc<AtomicUsize>);

impl Drop for RespConnectionPermit {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::Release);
    }
}

#[derive(Debug, Clone)]
pub struct TlsConfig {
    pub cert_path: PathBuf,
    pub key_path: PathBuf,
    /// When present, require a client certificate chaining to this CA.
    pub client_ca_path: Option<PathBuf>,
}

#[derive(Debug, Clone)]
pub struct ServerConfig {
    pub bind: SocketAddr,
    pub data_dir: PathBuf,
    pub username: String,
    pub password: Option<String>,
    pub viewer_username: String,
    pub viewer_password: Option<String>,
    pub allow_unauthenticated_remote: bool,
    pub allow_plaintext_remote: bool,
    pub tls: Option<TlsConfig>,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            bind: "127.0.0.1:6379".parse().expect("valid default socket"),
            data_dir: PathBuf::from("falkordb-native-data"),
            username: "default".to_string(),
            password: None,
            viewer_username: "viewer".to_string(),
            viewer_password: None,
            allow_unauthenticated_remote: false,
            allow_plaintext_remote: false,
            tls: None,
        }
    }
}

impl ServerConfig {
    pub fn validate(&self) -> Result<(), String> {
        let remote = !self.bind.ip().is_loopback();

        if let Some(viewer_password) = self.viewer_password.as_deref() {
            let Some(admin_password) = self.password.as_deref() else {
                return Err(
                    "viewer credentials require an admin password to be configured"
                        .to_string(),
                );
            };
            if self.viewer_username == self.username {
                return Err(
                    "viewer username must be different from the admin username"
                        .to_string(),
                );
            }
            if viewer_password.as_bytes() == admin_password.as_bytes() {
                return Err(
                    "viewer password must be different from the admin password"
                        .to_string(),
                );
            }
        }

        if remote && self.tls.is_none() && !self.allow_plaintext_remote {
            return Err(
                "refusing plaintext non-loopback bind; configure TLS or explicitly pass                  --allow-plaintext-remote"
                    .to_string(),
            );
        }

        if self.password.is_none() && !self.allow_unauthenticated_remote && remote {
            return Err(
                "refusing unauthenticated non-loopback bind; configure --password or                  --allow-unauthenticated-remote"
                    .to_string(),
            );
        }

        if let Some(tls) = &self.tls {
            if !tls.cert_path.is_file() {
                return Err(format!("TLS certificate not found: {}", tls.cert_path.display()));
            }
            if !tls.key_path.is_file() {
                return Err(format!("TLS private key not found: {}", tls.key_path.display()));
            }
            if let Some(ca) = &tls.client_ca_path
                && !ca.is_file()
            {
                return Err(format!("TLS client CA not found: {}", ca.display()));
            }
        }

        Ok(())
    }
}

pub struct GraphCatalog {
    data_dir: PathBuf,
    graph_dir: PathBuf,
    graphs: RwLock<HashMap<String, Arc<NativeGraph>>>,
}

#[derive(Debug, Clone)]
pub struct RdbFileGraphReport {
    pub graph: String,
    pub fragments: usize,
    pub nodes: u64,
    pub relationships: u64,
    pub checkpoint_file: Option<String>,
}

#[derive(Debug, Clone)]
pub struct GraphDatabaseStats {
    pub graph: String,
    pub nodes: u64,
    pub relationships: u64,
    pub graph_version: u64,
    pub schema_version: u64,
    pub labels: usize,
    pub relationship_types: usize,
    pub property_keys: usize,
    pub index_definitions: usize,
    pub constraints: usize,
    pub wal_bytes: u64,
    pub checkpoint_bytes: u64,
    pub checkpoint_count: usize,
    pub latest_checkpoint_sequence: Option<u64>,
    pub latest_checkpoint_bytes: Option<u64>,
    pub persistent_bytes: u64,
    pub estimated_memory_bytes: Option<u64>,
}

#[derive(Debug, Clone)]
pub struct DatabaseStats {
    pub total_storage_bytes: u64,
    pub graphs_storage_bytes: u64,
    pub attributed_graph_bytes: u64,
    pub unattributed_graph_storage_bytes: u64,
    pub imports_storage_bytes: u64,
    pub imports_inside_data_dir: bool,
    pub other_storage_bytes: u64,
    pub graphs: Vec<GraphDatabaseStats>,
}

#[derive(Debug, Clone)]
pub struct RdbFileImportReport {
    pub import_root: String,
    pub file: String,
    pub redis_rdb_version: u32,
    pub size_bytes: u64,
    pub sha256: String,
    pub dry_run: bool,
    pub udf_count: usize,
    pub ignored_aux_keys: Vec<String>,
    pub graphs: Vec<RdbFileGraphReport>,
}

impl GraphCatalog {
    pub fn open(data_dir: impl AsRef<Path>) -> Result<Self, String> {
        let data_dir = data_dir.as_ref().to_path_buf();
        fs::create_dir_all(&data_dir)
            .map_err(|e| format!("create data directory {}: {e}", data_dir.display()))?;
        udf_store::restore(&data_dir)?;
        let graph_dir = data_dir.join("graphs");
        fs::create_dir_all(&graph_dir)
            .map_err(|e| format!("create graph data directory {}: {e}", graph_dir.display()))?;

        let mut graphs = HashMap::new();
        for entry in fs::read_dir(&graph_dir)
            .map_err(|e| format!("read graph data directory {}: {e}", graph_dir.display()))?
        {
            let entry = entry.map_err(|e| format!("read graph directory entry: {e}"))?;
            let path = entry.path();
            if path.extension().and_then(|s| s.to_str()) != Some("wal") {
                continue;
            }
            let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else {
                continue;
            };
            let name = decode_graph_name(stem)?;
            let graph = NativeGraph::open_persistent(&name, &path)?;
            graphs.insert(name, Arc::new(graph));
        }

        Ok(Self {
            data_dir,
            graph_dir,
            graphs: RwLock::new(graphs),
        })
    }

    fn wal_path(&self, name: &str) -> PathBuf {
        self.graph_dir.join(format!("{}.wal", encode_graph_name(name)))
    }

    pub fn data_dir(&self) -> &Path {
        &self.data_dir
    }

    pub fn get(&self, name: &str) -> Option<Arc<NativeGraph>> {
        self.graphs.read().get(name).cloned()
    }

    pub fn get_or_create(&self, name: &str) -> Result<Arc<NativeGraph>, String> {
        if let Some(graph) = self.get(name) {
            return Ok(graph);
        }

        let mut graphs = self.graphs.write();
        if let Some(graph) = graphs.get(name) {
            return Ok(Arc::clone(graph));
        }

        let graph = Arc::new(NativeGraph::open_persistent(name, self.wal_path(name))?);
        graphs.insert(name.to_string(), Arc::clone(&graph));
        Ok(graph)
    }

    pub fn list(&self) -> Vec<String> {
        let mut names: Vec<String> = self.graphs.read().keys().cloned().collect();
        names.sort();
        names
    }

    /// Return exact persistent storage usage plus live graph cardinality/version
    /// information. Optional memory usage is a sampled in-memory estimate from
    /// the graph engine and is deliberately reported separately from disk bytes.
    pub fn database_stats(
        &self,
        graph_filter: Option<&str>,
        include_memory: bool,
        memory_samples: usize,
    ) -> Result<DatabaseStats, String> {
        if include_memory && memory_samples == 0 {
            return Err("memory_samples must be at least 1".to_string());
        }

        let total_storage_bytes = directory_size_bytes(&self.data_dir)?;
        let graphs_storage_bytes = directory_size_bytes(&self.graph_dir)?;
        let import_root = configured_import_root(&self.data_dir);
        let imports_storage_bytes = directory_size_bytes(&import_root)?;
        let imports_inside_data_dir = path_is_within(&self.data_dir, &import_root);
        let other_storage_bytes = total_storage_bytes
            .saturating_sub(graphs_storage_bytes)
            .saturating_sub(if imports_inside_data_dir {
                imports_storage_bytes
            } else {
                0
            });

        let all_names = self.list();
        let names = if let Some(name) = graph_filter {
            if !all_names.iter().any(|candidate| candidate == name) {
                return Err(format!("graph {name:?} does not exist"));
            }
            vec![name.to_string()]
        } else {
            all_names.clone()
        };

        // Storage attribution always covers every live graph, even when the
        // returned detail rows are filtered to one graph. Otherwise storage
        // belonging to other live graphs would be mislabeled as orphaned.
        let mut attributed_graph_bytes = 0u64;
        for name in &all_names {
            let Some(graph) = self.get(name) else {
                // A concurrent GRAPH.DELETE may remove an entry after list().
                // Unfiltered statistics are an eventually-consistent snapshot,
                // so skip the vanished graph rather than failing the whole report.
                continue;
            };
            let wal_bytes = graph.wal_len()?;
            let (checkpoint_bytes, _, _, _) = self.checkpoint_storage_stats(name)?;
            attributed_graph_bytes = attributed_graph_bytes
                .saturating_add(wal_bytes)
                .saturating_add(checkpoint_bytes);
        }
        let unattributed_graph_storage_bytes =
            graphs_storage_bytes.saturating_sub(attributed_graph_bytes);

        let mut graphs = Vec::with_capacity(names.len());
        for name in names {
            let Some(graph) = self.get(&name) else {
                if graph_filter.is_some() {
                    return Err(format!("graph {name:?} no longer exists"));
                }
                continue;
            };
            let (nodes, relationships, graph_version) = graph.cardinality_and_version();
            let schema = graph.schema_summary();
            let wal_bytes = graph.wal_len()?;
            let (
                checkpoint_bytes,
                checkpoint_count,
                latest_checkpoint_sequence,
                latest_checkpoint_bytes,
            ) = self.checkpoint_storage_stats(&name)?;
            let persistent_bytes = wal_bytes.saturating_add(checkpoint_bytes);

            let estimated_memory_bytes = if include_memory {
                let report = graph.memory_usage_report(memory_samples);
                let node_attributes = report
                    .node_attr_by_label
                    .iter()
                    .map(|(_, bytes)| *bytes)
                    .sum::<usize>();
                let edge_attributes = report
                    .edge_attr_by_type
                    .iter()
                    .map(|(_, bytes)| *bytes)
                    .sum::<usize>();
                let total = report
                    .label_matrices_sz
                    .saturating_add(report.relation_matrices_sz)
                    .saturating_add(report.node_block_storage_sz)
                    .saturating_add(node_attributes)
                    .saturating_add(report.unlabeled_node_attr_sz)
                    .saturating_add(report.edge_block_storage_sz)
                    .saturating_add(edge_attributes)
                    .saturating_add(report.indices_sz);
                Some(total as u64)
            } else {
                None
            };

            graphs.push(GraphDatabaseStats {
                graph: name,
                nodes,
                relationships,
                graph_version,
                schema_version: schema.schema_version,
                labels: schema.labels,
                relationship_types: schema.relationship_types,
                property_keys: schema.property_keys,
                index_definitions: schema.index_definitions,
                constraints: schema.constraints,
                wal_bytes,
                checkpoint_bytes,
                checkpoint_count,
                latest_checkpoint_sequence,
                latest_checkpoint_bytes,
                persistent_bytes,
                estimated_memory_bytes,
            });
        }

        Ok(DatabaseStats {
            total_storage_bytes,
            graphs_storage_bytes,
            attributed_graph_bytes,
            unattributed_graph_storage_bytes,
            imports_storage_bytes,
            imports_inside_data_dir,
            other_storage_bytes,
            graphs,
        })
    }

    fn checkpoint_storage_stats(
        &self,
        name: &str,
    ) -> Result<(u64, usize, Option<u64>, Option<u64>), String> {
        let wal = self.wal_path(name);
        let wal_name = wal
            .file_name()
            .and_then(|value| value.to_str())
            .ok_or_else(|| format!("invalid WAL filename for graph {name:?}"))?;
        let prefix = format!("{wal_name}.snapshot.");

        let mut total = 0u64;
        let mut count = 0usize;
        let mut latest_sequence = None::<u64>;
        let mut latest_bytes = None::<u64>;

        for entry in fs::read_dir(&self.graph_dir)
            .map_err(|e| format!("scan graph storage {}: {e}", self.graph_dir.display()))?
        {
            let entry = entry.map_err(|e| format!("read graph storage entry: {e}"))?;
            let file_type = entry
                .file_type()
                .map_err(|e| format!("read graph storage file type: {e}"))?;
            if !file_type.is_file() {
                continue;
            }
            let filename = entry.file_name();
            let filename = filename.to_string_lossy();
            let Some(rest) = filename.strip_prefix(&prefix) else {
                continue;
            };
            let Some(sequence_text) = rest.strip_suffix(".fgs") else {
                continue;
            };
            let Ok(sequence) = sequence_text.parse::<u64>() else {
                continue;
            };
            let bytes = match entry.metadata() {
                Ok(metadata) => metadata.len(),
                Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                    // Checkpoint rotation/deletion raced this directory scan.
                    continue;
                }
                Err(err) => {
                    return Err(format!(
                        "stat checkpoint {}: {err}",
                        entry.path().display()
                    ));
                }
            };
            total = total.saturating_add(bytes);
            count += 1;
            if latest_sequence.is_none_or(|current| sequence > current) {
                latest_sequence = Some(sequence);
                latest_bytes = Some(bytes);
            }
        }

        Ok((total, count, latest_sequence, latest_bytes))
    }

    pub fn checkpoint_large_wals(
        &self,
        threshold_bytes: u64,
    ) -> Vec<(String, Result<PathBuf, String>)> {
        let graphs: Vec<(String, Arc<NativeGraph>)> = self
            .graphs
            .read()
            .iter()
            .map(|(name, graph)| (name.clone(), Arc::clone(graph)))
            .collect();

        let mut results = Vec::new();
        for (name, graph) in graphs {
            match graph.wal_len() {
                Ok(len) if len >= threshold_bytes => {
                    results.push((name, graph.checkpoint()));
                }
                Ok(_) => {}
                Err(err) => results.push((name, Err(err))),
            }
        }
        results
    }


    pub fn copy(&self, source: &str, destination: &str) -> Result<(), String> {
        if source == destination {
            return Err("destination key already exists".to_string());
        }

        let source_graph = {
            let graphs = self.graphs.read();
            if graphs.contains_key(destination) || self.wal_path(destination).exists() {
                return Err("destination key already exists".to_string());
            }
            graphs
                .get(source)
                .cloned()
                .ok_or_else(|| "Invalid graph operation on empty key".to_string())?
        };

        let destination_graph = Arc::new(
            source_graph.copy_persistent(destination, self.wal_path(destination))?,
        );

        let mut graphs = self.graphs.write();
        if graphs.contains_key(destination) {
            return Err("destination key already exists".to_string());
        }
        graphs.insert(destination.to_string(), destination_graph);
        Ok(())
    }

    /// Import FalkorDB's upstream v19 single-key GRAPH.RESTORE payload and
    /// convert it into the native checkpoint/WAL durability format.
    pub fn restore_payload(
        &self,
        destination: &str,
        payload: &[u8],
    ) -> Result<(), String> {
        let prepared = NativeGraph::prepare_falkordb_v19_payload(destination, payload)?;
        self.restore_prepared_payload(destination, prepared)
    }

    fn restore_prepared_payload(
        &self,
        destination: &str,
        prepared: PreparedFalkorImport,
    ) -> Result<(), String> {
        let mut graphs = self.graphs.write();
        let wal_path = self.wal_path(destination);
        if graphs.contains_key(destination) || wal_path.exists() {
            return Err("restore graph failed, key already exists".to_string());
        }

        let graph = Arc::new(NativeGraph::restore_prepared_falkordb(
            destination,
            &wal_path,
            prepared,
        )?);
        graphs.insert(destination.to_string(), graph);
        Ok(())
    }

    /// Restore one current FalkorDB graph from the ordered module fragments
    /// found in a Redis RDB file. This is intentionally migration-only and
    /// refuses to overwrite an existing graph.
    pub fn restore_rdb_fragments(&self, fragments: &[Vec<u8>]) -> Result<String, String> {
        if fragments.is_empty() {
            return Err("RDB fragment restore requires at least one fragment".to_string());
        }

        let mut decoded = snapshot::load_falkordb_v19_fragments(fragments)?;
        if decoded.len() != 1 {
            return Err(format!(
                "RDB fragment restore expected exactly one graph, decoded {}",
                decoded.len()
            ));
        }

        let (name, graph) = decoded
            .pop()
            .ok_or_else(|| "RDB fragment restore decoded no graph".to_string())?;

        if self.contains(&name)
            || self.wal_path(&name).exists()
            || snapshot::load_latest(&self.wal_path(&name), &name)?.is_some()
        {
            return Err(format!(
                "destination graph {name:?} already exists; raw RDB import will not overwrite it"
            ));
        }

        // Normalize the multi-key RDB representation into the same portable
        // single-key v19 payload used by GRAPH.RESTORE, then let the normal
        // durable import path create checkpoint/WAL storage.
        let payload = snapshot::save_falkordb_v19_payload(&graph, &name);
        self.restore_payload(&name, &payload)?;
        Ok(name)
    }

    /// Parse and optionally restore a complete Redis dump.rdb containing native
    /// FalkorDB v19 graphdata/graphmeta module keys. Files are resolved below
    /// FALKORDB_IMPORT_DIR, or <data-dir>/imports when that variable is unset.
    /// This path never overwrites existing graphs.
    pub fn import_rdb_file(
        &self,
        file: &str,
        expected_graph: Option<&str>,
        expected_sha256: Option<&str>,
        dry_run: bool,
    ) -> Result<RdbFileImportReport, String> {
        let (import_root, path) = rdb_file::resolve_import_file(&self.data_dir, file)?;
        let parsed = rdb_file::parse_rdb_file(&path)?;

        if let Some(expected) = expected_sha256 {
            let expected = expected.trim().to_ascii_lowercase();
            if expected.len() != 64 || !expected.bytes().all(|byte| byte.is_ascii_hexdigit()) {
                return Err("sha256 must be exactly 64 hexadecimal characters".to_string());
            }
            if parsed.sha256_hex != expected {
                return Err(format!(
                    "RDB SHA-256 mismatch for {file:?}: expected {expected}, computed {}",
                    parsed.sha256_hex
                ));
            }
        }

        if !parsed.udfs.is_empty() {
            return Err(format!(
                "RDB contains {} FalkorDB UDF library/libraries; native MCP RDB import refuses to discard them",
                parsed.udfs.len()
            ));
        }

        if let Some(expected) = expected_graph {
            if parsed.graphs.len() != 1 {
                return Err(format!(
                    "expected exactly one graph named {expected:?}, but RDB contains {} graphs",
                    parsed.graphs.len()
                ));
            }
            if parsed.graphs[0].name != expected {
                return Err(format!(
                    "RDB graph name mismatch: expected {expected:?}, found {:?}",
                    parsed.graphs[0].name
                ));
            }
        }

        for graph in &parsed.graphs {
            if self.contains(&graph.name)
                || self.wal_path(&graph.name).exists()
                || snapshot::load_latest(&self.wal_path(&graph.name), &graph.name)?.is_some()
            {
                return Err(format!(
                    "destination graph {:?} already exists; raw RDB import is intentionally non-destructive",
                    graph.name
                ));
            }
        }

        let mut graph_reports = parsed
            .graphs
            .iter()
            .map(|graph| RdbFileGraphReport {
                graph: graph.name.clone(),
                fragments: graph.fragments.len(),
                nodes: graph.node_count,
                relationships: graph.edge_count,
                checkpoint_file: None,
            })
            .collect::<Vec<_>>();

        if dry_run {
            return Ok(RdbFileImportReport {
                import_root: import_root.display().to_string(),
                file: file.to_string(),
                redis_rdb_version: parsed.version,
                size_bytes: parsed.size_bytes,
                sha256: parsed.sha256_hex,
                dry_run: true,
                udf_count: parsed.udfs.len(),
                ignored_aux_keys: parsed.ignored_aux_keys.clone(),
                graphs: graph_reports,
            });
        }

        let mut imported = Vec::<String>::new();
        let import_result = (|| -> Result<(), String> {
            for (source, report) in parsed.graphs.iter().zip(graph_reports.iter_mut()) {
                let restored = self.restore_rdb_fragments(&source.fragments)?;
                if restored != source.name {
                    return Err(format!(
                        "GRAPH.RESTORE.RDB restored {restored:?}, expected {:?}",
                        source.name
                    ));
                }
                imported.push(restored.clone());

                let graph = self
                    .get(&restored)
                    .ok_or_else(|| format!("restored graph {restored:?} disappeared"))?;
                let nodes = graph_query_count(&graph, "MATCH (n) RETURN count(n)")?;
                let relationships =
                    graph_query_count(&graph, "MATCH ()-[r]->() RETURN count(r)")?;
                if nodes != source.node_count || relationships != source.edge_count {
                    return Err(format!(
                        "count verification failed for {:?}: nodes {}/{}, relationships {}/{}",
                        source.name,
                        nodes,
                        source.node_count,
                        relationships,
                        source.edge_count
                    ));
                }

                let checkpoint = graph.checkpoint()?;
                report.nodes = nodes;
                report.relationships = relationships;
                report.checkpoint_file = Some(
                    checkpoint
                        .file_name()
                        .and_then(|value| value.to_str())
                        .unwrap_or("")
                        .to_string(),
                );
            }
            Ok(())
        })();

        if let Err(err) = import_result {
            let mut rollback_errors = Vec::new();
            for name in imported.iter().rev() {
                if let Err(rollback_err) = self.delete(name) {
                    rollback_errors.push(format!("{name:?}: {rollback_err}"));
                }
            }
            if rollback_errors.is_empty() {
                return Err(format!("{err}; imported graphs were rolled back"));
            }
            return Err(format!(
                "{err}; rollback also failed for {}",
                rollback_errors.join(", ")
            ));
        }

        Ok(RdbFileImportReport {
            import_root: import_root.display().to_string(),
            file: file.to_string(),
            redis_rdb_version: parsed.version,
            size_bytes: parsed.size_bytes,
            sha256: parsed.sha256_hex,
            dry_run: false,
            udf_count: parsed.udfs.len(),
            ignored_aux_keys: parsed.ignored_aux_keys,
            graphs: graph_reports,
        })
    }

    /// Import an arbitrary server-local JSONL corpus by substituting each batch
    /// into a caller-supplied Cypher template containing exactly one {{ROWS}}
    /// placeholder. The executable remains schema-agnostic; the template owns
    /// all MATCH/MERGE/CREATE semantics for the target graph.
    #[allow(clippy::too_many_arguments)]
    pub fn bulk_import_jsonl_file(
        &self,
        graph_name: &str,
        file: &str,
        cypher_template: &str,
        expected_sha256: Option<&str>,
        batch_size: Option<usize>,
        max_batch_bytes: Option<usize>,
        start_record: Option<usize>,
        max_records: Option<usize>,
        dry_run: bool,
        create_graph: bool,
        checkpoint: bool,
    ) -> Result<(PathBuf, file_import::JsonlImportReport), String> {
        let (import_root, path) = rdb_file::resolve_import_file(&self.data_dir, file)?;

        let existed = self.contains(graph_name);
        let graph = if let Some(graph) = self.get(graph_name) {
            graph
        } else if dry_run {
            Arc::new(NativeGraph::new(graph_name))
        } else if create_graph {
            self.get_or_create(graph_name)?
        } else {
            return Err(format!(
                "destination graph {graph_name:?} does not exist; set create_graph=true to create it"
            ));
        };

        let result = file_import::import_jsonl(
            &graph,
            &path,
            cypher_template,
            expected_sha256,
            batch_size,
            max_batch_bytes,
            start_record,
            max_records,
            dry_run,
            checkpoint,
        );

        match result {
            Ok(report) => Ok((import_root, report)),
            Err(err) => {
                // A newly-created destination can be rolled back completely.
                // Existing graphs retain already-committed batches so an
                // idempotent template can resume from the reported frontier.
                if !dry_run && create_graph && !existed {
                    match self.delete(graph_name) {
                        Ok(_) => return Err(format!("{err}; newly-created graph was rolled back")),
                        Err(rollback_err) => {
                            return Err(format!(
                                "{err}; rollback of newly-created graph also failed: {rollback_err}"
                            ));
                        }
                    }
                }
                Err(err)
            }
        }
    }

    /// Export one graph in FalkorDB's upstream v19 GRAPH.RESTORE payload
    /// format. This provides a lossless portable migration artifact.
    pub fn dump_payload(&self, name: &str) -> Result<Vec<u8>, String> {
        let graph = self
            .get(name)
            .ok_or_else(|| "Invalid graph operation on empty key".to_string())?;
        Ok(graph.export_falkordb_v19_payload())
    }

    /// Export a standard Redis DUMP value for this graph. A current Redis /
    /// FalkorDB instance can consume the result with RESTORE.
    pub fn redis_dump(&self, name: &str) -> Result<Vec<u8>, String> {
        Ok(redis_dump::create_falkordb_dump(&self.dump_payload(name)?))
    }

    /// Consume a standard Redis DUMP payload produced for a FalkorDB
    /// `graphdata` module key and convert it into native persistence.
    pub fn redis_restore(
        &self,
        destination: &str,
        dump: &[u8],
        replace: bool,
    ) -> Result<(), String> {
        // Validate both the Redis/module envelope and the complete FalkorDB
        // graph before touching an existing key. A payload can have a valid
        // Redis CRC/module wrapper and still contain corrupt graph bytes.
        let payload = redis_dump::extract_falkordb_v19_payload(dump)?;
        let prepared = NativeGraph::prepare_falkordb_v19_payload(destination, &payload)?;

        if self.contains(destination) {
            if !replace {
                return Err("BUSYKEY Target key name already exists.".to_string());
            }
            self.delete(destination)?;
        } else {
            let wal = self.wal_path(destination);
            let has_checkpoint = snapshot::load_latest(&wal, destination)?.is_some();
            if wal.exists() || has_checkpoint {
                if !replace {
                    return Err("BUSYKEY Target key name already exists.".to_string());
                }
                if wal.exists() {
                    fs::remove_file(&wal)
                        .map_err(|e| format!("remove replaced graph WAL {}: {e}", wal.display()))?;
                }
                snapshot::remove_all_checkpoints(&wal)?;
            }
        }

        self.restore_prepared_payload(destination, prepared)
    }


    pub fn contains(&self, name: &str) -> bool {
        self.graphs.read().contains_key(name)
    }

    pub fn delete(&self, name: &str) -> Result<bool, String> {
        let graph = self.graphs.write().remove(name);
        let Some(graph) = graph else {
            return Ok(false);
        };

        // Windows will not unlink a WAL while another handle is alive.
        if Arc::strong_count(&graph) != 1 {
            self.graphs.write().insert(name.to_string(), graph);
            return Err("graph is busy; retry GRAPH.DELETE after active queries complete".to_string());
        }
        drop(graph);

        let wal = self.wal_path(name);
        if wal.exists() {
            fs::remove_file(&wal)
                .map_err(|e| format!("delete graph WAL {}: {e}", wal.display()))?;
        }
        snapshot::remove_all_checkpoints(&wal)?;
        Ok(true)
    }

    pub fn flush(&self) -> Result<usize, String> {
        let names = self.list();
        let mut removed = 0;
        for name in names {
            if self.delete(&name)? {
                removed += 1;
            }
        }
        Ok(removed)
    }
}

fn configured_import_root(data_dir: &Path) -> PathBuf {
    env::var_os("FALKORDB_IMPORT_DIR")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| data_dir.join("imports"))
}

fn path_is_within(root: &Path, candidate: &Path) -> bool {
    match (root.canonicalize(), candidate.canonicalize()) {
        (Ok(root), Ok(candidate)) => candidate.starts_with(root),
        _ => candidate.starts_with(root),
    }
}

fn directory_size_bytes(path: &Path) -> Result<u64, String> {
    if !path.exists() {
        return Ok(0);
    }
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(0),
        Err(err) => {
            return Err(format!("stat storage path {}: {err}", path.display()));
        }
    };
    if metadata.file_type().is_symlink() {
        return Ok(0);
    }
    if metadata.is_file() {
        return Ok(metadata.len());
    }
    if !metadata.is_dir() {
        return Ok(0);
    }

    let mut total = 0u64;
    for entry in fs::read_dir(path)
        .map_err(|e| format!("scan storage directory {}: {e}", path.display()))?
    {
        let entry = entry.map_err(|e| format!("read storage directory entry: {e}"))?;
        total = total.saturating_add(directory_size_bytes(&entry.path())?);
    }
    Ok(total)
}

fn encode_graph_name(name: &str) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(name.len() * 2);
    for b in name.as_bytes() {
        out.push(HEX[(b >> 4) as usize] as char);
        out.push(HEX[(b & 0x0f) as usize] as char);
    }
    out
}

fn decode_graph_name(stem: &str) -> Result<String, String> {
    if stem.len() % 2 != 0 {
        return Err(format!("invalid graph WAL filename: {stem}"));
    }
    let mut bytes = Vec::with_capacity(stem.len() / 2);
    let raw = stem.as_bytes();
    for i in (0..raw.len()).step_by(2) {
        let hi = hex(raw[i]).ok_or_else(|| format!("invalid graph WAL filename: {stem}"))?;
        let lo = hex(raw[i + 1]).ok_or_else(|| format!("invalid graph WAL filename: {stem}"))?;
        bytes.push((hi << 4) | lo);
    }
    String::from_utf8(bytes).map_err(|e| format!("graph WAL name is not UTF-8: {e}"))
}

fn hex(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

pub fn serve(config: ServerConfig) -> Result<(), String> {
    let catalog = Arc::new(GraphCatalog::open(&config.data_dir)?);
    serve_with_catalog(config, catalog)
}

pub fn serve_with_catalog(
    config: ServerConfig,
    catalog: Arc<GraphCatalog>,
) -> Result<(), String> {
    config.validate()?;
    let tls = config
        .tls
        .as_ref()
        .map(load_tls_config)
        .transpose()?
        .map(Arc::new);
    let listener = TcpListener::bind(config.bind)
        .map_err(|e| format!("bind {}: {e}", config.bind))?;

    eprintln!(
        "FalkorDB native RESP{} server listening on {} (data: {})",
        if tls.is_some() { "/TLS" } else { "" },
        config.bind,
        config.data_dir.display()
    );

    let config = Arc::new(config);
    let active_connections = Arc::new(AtomicUsize::new(0));
    for incoming in listener.incoming() {
        match incoming {
            Ok(stream) => {
                let previous = active_connections.fetch_add(1, Ordering::AcqRel);
                if previous >= MAX_RESP_CONNECTIONS {
                    active_connections.fetch_sub(1, Ordering::Release);
                    drop(stream);
                    continue;
                }

                let permit = RespConnectionPermit(Arc::clone(&active_connections));
                let catalog = Arc::clone(&catalog);
                let config = Arc::clone(&config);
                let tls = tls.clone();
                thread::spawn(move || {
                    let _permit = permit;
                    let peer = stream.peer_addr().ok();
                    let timeout = Some(Duration::from_secs(RESP_IO_TIMEOUT_SECS));
                    let setup = stream
                        .set_nodelay(true)
                        .map_err(|e| format!("set TCP_NODELAY: {e}"))
                        .and_then(|_| {
                            stream
                                .set_read_timeout(timeout)
                                .map_err(|e| format!("set RESP read timeout: {e}"))
                        })
                        .and_then(|_| {
                            stream
                                .set_write_timeout(timeout)
                                .map_err(|e| format!("set RESP write timeout: {e}"))
                        });

                    let result = setup.and_then(|_| {
                        if let Some(tls) = tls {
                            let conn = ServerConnection::new(tls)
                                .map_err(|e| format!("create TLS server connection: {e}"))?;
                            handle_connection_io(
                                StreamOwned::new(conn, stream),
                                &catalog,
                                &config,
                                peer,
                            )
                        } else {
                            handle_connection_io(stream, &catalog, &config, peer)
                        }
                    });

                    if let Err(err) = result {
                        eprintln!("client connection ended with error: {err}");
                    }
                });
            }
            Err(err) => eprintln!("accept failed: {err}"),
        }
    }
    Ok(())
}

pub(crate) fn load_tls_config(tls: &TlsConfig) -> Result<RustlsServerConfig, String> {
    // Multiple transitive crates can enable more than one rustls provider.
    // Select ring explicitly so TLS startup is deterministic instead of
    // relying on rustls feature auto-detection.
    let _ = rustls::crypto::ring::default_provider().install_default();
    let mut cert_reader = BufReader::new(
        File::open(&tls.cert_path)
            .map_err(|e| format!("open TLS certificate {}: {e}", tls.cert_path.display()))?,
    );
    let certs = rustls_pemfile::certs(&mut cert_reader)
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| format!("parse TLS certificate {}: {e}", tls.cert_path.display()))?;
    if certs.is_empty() {
        return Err(format!("TLS certificate file contains no certificates: {}", tls.cert_path.display()));
    }

    let mut key_reader = BufReader::new(
        File::open(&tls.key_path)
            .map_err(|e| format!("open TLS private key {}: {e}", tls.key_path.display()))?,
    );
    let key = rustls_pemfile::private_key(&mut key_reader)
        .map_err(|e| format!("parse TLS private key {}: {e}", tls.key_path.display()))?
        .ok_or_else(|| format!("TLS private key file contains no key: {}", tls.key_path.display()))?;

    if let Some(client_ca_path) = &tls.client_ca_path {
        let mut ca_reader = BufReader::new(
            File::open(client_ca_path)
                .map_err(|e| format!("open TLS client CA {}: {e}", client_ca_path.display()))?,
        );
        let mut roots = RootCertStore::empty();
        for cert in rustls_pemfile::certs(&mut ca_reader) {
            roots
                .add(cert.map_err(|e| format!("parse TLS client CA {}: {e}", client_ca_path.display()))?)
                .map_err(|e| format!("add TLS client CA {}: {e}", client_ca_path.display()))?;
        }
        let verifier = WebPkiClientVerifier::builder(Arc::new(roots))
            .build()
            .map_err(|e| format!("build TLS client certificate verifier: {e}"))?;
        RustlsServerConfig::builder()
            .with_client_cert_verifier(verifier)
            .with_single_cert(certs, key)
            .map_err(|e| format!("configure TLS certificate/key: {e}"))
    } else {
        RustlsServerConfig::builder()
            .with_no_client_auth()
            .with_single_cert(certs, key)
            .map_err(|e| format!("configure TLS certificate/key: {e}"))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RespProtocol {
    Resp2,
    Resp3,
}

struct ConnectionState {
    authenticated: bool,
    read_only: bool,
    protocol: RespProtocol,
    client_name: Option<String>,
    auth_failures: u8,
}

fn handle_connection_io<S: Read + Write>(
    io: S,
    catalog: &GraphCatalog,
    config: &ServerConfig,
    peer: Option<SocketAddr>,
) -> Result<(), String> {
    let mut reader = BufReader::new(io);

    let mut state = ConnectionState {
        authenticated: config.password.is_none(),
        read_only: false,
        protocol: RespProtocol::Resp2,
        client_name: None,
        auth_failures: 0,
    };

    loop {
        let command = match read_command(&mut reader)? {
            Some(command) => command,
            None => return Ok(()),
        };

        let should_quit = command
            .first()
            .is_some_and(|arg| ascii_upper(arg) == "QUIT");
        let response = dispatch(command, catalog, config, &mut state);
        write_resp(reader.get_mut(), &response, state.protocol)
            .map_err(|e| format!("write response to {peer:?}: {e}"))?;
        reader
            .get_mut()
            .flush()
            .map_err(|e| format!("flush response: {e}"))?;

        if should_quit || state.auth_failures >= MAX_AUTH_FAILURES_PER_CONNECTION {
            return Ok(());
        }
    }
}

fn dispatch(
    args: Vec<Vec<u8>>,
    catalog: &GraphCatalog,
    config: &ServerConfig,
    state: &mut ConnectionState,
) -> Resp {
    if args.is_empty() {
        return Resp::Error("ERR empty command".to_string());
    }

    let command = ascii_upper(&args[0]);

    if command == "AUTH" {
        return handle_auth(&args, config, state);
    }
    if command == "HELLO" {
        return handle_hello(&args, config, state);
    }

    if !state.authenticated {
        return Resp::Error("NOAUTH Authentication required.".to_string());
    }

    if state.read_only && !read_only_command_allowed(&command, &args) {
        return Resp::Error(format!(
            "NOPERM this user has no permissions to run the '{}' command",
            command.to_ascii_lowercase()
        ));
    }

    match command.as_str() {
        "PING" => {
            if args.len() > 1 {
                Resp::Bulk(args[1].clone())
            } else {
                Resp::Simple("PONG".to_string())
            }
        }
        "ECHO" => require_arity(&args, 2).map_or_else(Resp::Error, |_| Resp::Bulk(args[1].clone())),
        "QUIT" => Resp::Simple("OK".to_string()),
        "SELECT" => {
            if args.get(1).map(|v| v.as_slice()) == Some(b"0") {
                Resp::Simple("OK".to_string())
            } else {
                Resp::Error("ERR only database 0 is supported".to_string())
            }
        }
        "CLIENT" => handle_client(&args, state),
        "INFO" => {
            let body = "# Server\r\nredis_mode:standalone\r\nredis_version:7.2.0\r\nfalkordb_native:1\r\n";
            Resp::Bulk(body.as_bytes().to_vec())
        }
        "COMMAND" => Resp::Array(Vec::new()),
        "MODULE" => handle_module(&args),
        "ACL" => handle_acl(&args, config, state),
        "DUMP" => {
            if args.len() != 2 {
                return Resp::Error("ERR wrong number of arguments for 'dump' command".to_string());
            }
            let name = match utf8(&args[1], "key name") {
                Ok(v) => v,
                Err(err) => return Resp::Error(err),
            };
            match catalog.redis_dump(name) {
                Ok(payload) => Resp::Bulk(payload),
                Err(_) => Resp::Null,
            }
        }
        "RESTORE" | "RESTORE-ASKING" => handle_redis_restore(&args, catalog),
        "GRAPH.LIST" => Resp::Array(catalog.list().into_iter().map(bulk).collect()),
        "GRAPH.BULK" => handle_graph_bulk(&args, catalog),
        "GRAPH.QUERY" => handle_graph_query(&args, catalog, false),
        "GRAPH.RO_QUERY" => handle_graph_query(&args, catalog, true),
        "GRAPH.EXPLAIN" => handle_graph_explain(&args, catalog),
        "GRAPH.PROFILE" => handle_graph_profile(&args, catalog),
        "GRAPH.RESTORE.RDB" => {
            if args.len() < 2 {
                return Resp::Error(
                    "ERR GRAPH.RESTORE.RDB requires one or more ordered graph fragments"
                        .to_string(),
                );
            }
            match catalog.restore_rdb_fragments(&args[1..]) {
                Ok(name) => Resp::Bulk(name.into_bytes()),
                Err(err) => Resp::Error(format!("ERR {err}")),
            }
        }
        "GRAPH.DUMP" => {
            if args.len() != 2 {
                return Resp::Error("ERR wrong number of arguments for 'graph.dump' command".to_string());
            }
            let name = match utf8(&args[1], "graph name") {
                Ok(v) => v,
                Err(err) => return Resp::Error(err),
            };
            match catalog.dump_payload(name) {
                Ok(payload) => Resp::Bulk(payload),
                Err(err) => Resp::Error(format!("ERR {err}")),
            }
        }
        "GRAPH.RESTORE" => {
            if args.len() != 3 {
                return Resp::Error("ERR wrong number of arguments for 'graph.restore' command".to_string());
            }
            let destination = match utf8(&args[1], "destination graph name") {
                Ok(v) => v,
                Err(err) => return Resp::Error(err),
            };
            match catalog.restore_payload(destination, &args[2]) {
                Ok(()) => Resp::Simple("OK".to_string()),
                Err(err) => Resp::Error(format!("ERR {err}")),
            }
        }
        "GRAPH.COPY" => {
            if args.len() != 3 {
                return Resp::Error("ERR wrong number of arguments for 'graph.copy' command".to_string());
            }
            let source = match utf8(&args[1], "source graph name") {
                Ok(v) => v,
                Err(err) => return Resp::Error(err),
            };
            let destination = match utf8(&args[2], "destination graph name") {
                Ok(v) => v,
                Err(err) => return Resp::Error(err),
            };
            match catalog.copy(source, destination) {
                Ok(()) => Resp::Simple("OK".to_string()),
                Err(err) => Resp::Error(format!("ERR {err}")),
            }
        }
        "GRAPH.DELETE" => {
            if let Err(err) = require_arity(&args, 2) {
                return Resp::Error(err);
            }
            let name = match utf8(&args[1], "graph name") {
                Ok(v) => v,
                Err(err) => return Resp::Error(err),
            };
            match catalog.delete(name) {
                Ok(true) => Resp::Simple("Graph removed, internal execution time: 0.000000 milliseconds".to_string()),
                Ok(false) => Resp::Error("ERR Invalid graph operation on empty key".to_string()),
                Err(err) => Resp::Error(format!("ERR {err}")),
            }
        }
        "TTL" | "PTTL" | "EXPIRETIME" => {
            if args.len() != 2 {
                return Resp::Error(format!(
                    "ERR wrong number of arguments for '{}' command",
                    command.to_ascii_lowercase()
                ));
            }
            let exists = std::str::from_utf8(&args[1])
                .ok()
                .is_some_and(|name| catalog.contains(name));
            Resp::Int(if exists { -1 } else { -2 })
        }
        "EXISTS" => {
            let count = args
                .iter()
                .skip(1)
                .filter_map(|v| std::str::from_utf8(v).ok())
                .filter(|name| catalog.contains(name))
                .count() as i64;
            Resp::Int(count)
        }
        "DEL" => {
            let mut count = 0i64;
            for name in args.iter().skip(1).filter_map(|v| std::str::from_utf8(v).ok()) {
                match catalog.delete(name) {
                    Ok(true) => count += 1,
                    Ok(false) => {}
                    Err(err) => return Resp::Error(format!("ERR {err}")),
                }
            }
            Resp::Int(count)
        }
        "FLUSHDB" | "FLUSHALL" => match catalog.flush() {
            Ok(_) => Resp::Simple("OK".to_string()),
            Err(err) => Resp::Error(format!("ERR {err}")),
        },
        "TYPE" => {
            if args.len() != 2 {
                Resp::Error("ERR wrong number of arguments for 'type' command".to_string())
            } else if std::str::from_utf8(&args[1]).ok().is_some_and(|n| catalog.contains(n)) {
                Resp::Simple("graphdata".to_string())
            } else {
                Resp::Simple("none".to_string())
            }
        }
        "GRAPH.SLOWLOG" => handle_graph_slowlog(&args, catalog),
        "GRAPH.INFO" => handle_graph_info(&args),
        "GRAPH.MEMORY" => handle_graph_memory(&args, catalog),
        "GRAPH.CONSTRAINT" => handle_graph_constraint(&args, catalog),
        "GRAPH.CONFIG" => handle_graph_config(&args),
        "GRAPH.UDF" => handle_graph_udf(&args, catalog),
        _ => Resp::Error(format!("ERR unknown command '{}'", String::from_utf8_lossy(&args[0]))),
    }
}

fn read_only_command_allowed(command: &str, args: &[Vec<u8>]) -> bool {
    match command {
        "PING" | "ECHO" | "QUIT" | "SELECT" | "CLIENT" | "INFO" | "COMMAND"
        | "MODULE" | "DUMP" | "EXISTS" | "TYPE" | "TTL" | "PTTL" | "EXPIRETIME"
        | "GRAPH.LIST" | "GRAPH.RO_QUERY" | "GRAPH.EXPLAIN" | "GRAPH.INFO"
        | "GRAPH.MEMORY" => true,
        // Let ACL reach its handler so Browser receives the exact NOPERM it
        // expects while probing whether the authenticated user is an admin.
        "ACL" => args.get(1).is_some_and(|sub| ascii_upper(sub) == "GETUSER"),
        // GRAPH.QUERY is deliberately denied instead of silently downgraded:
        // FalkorDB Browser probes this command to distinguish Read-Only from
        // Read-Write accounts, then switches to GRAPH.RO_QUERY.
        _ => false,
    }
}

fn handle_module(args: &[Vec<u8>]) -> Resp {
    if args.len() != 2 || ascii_upper(&args[1]) != "LIST" {
        return Resp::Error("ERR only MODULE LIST is supported".to_string());
    }

    Resp::Array(vec![Resp::Array(vec![
        bulk("name"),
        bulk("graph"),
        bulk("ver"),
        Resp::Int(1),
        bulk("path"),
        bulk("native"),
        bulk("args"),
        Resp::Array(Vec::new()),
    ])])
}

fn handle_acl(args: &[Vec<u8>], config: &ServerConfig, state: &ConnectionState) -> Resp {
    if args.len() < 2 {
        return Resp::Error("ERR wrong number of arguments for 'acl' command".to_string());
    }
    if state.read_only {
        return Resp::Error(
            "NOPERM this user has no permissions to run the 'acl|getuser' command".to_string(),
        );
    }
    if ascii_upper(&args[1]) != "GETUSER" || args.len() != 3 {
        return Resp::Error("ERR only ACL GETUSER is supported".to_string());
    }

    let Ok(username) = std::str::from_utf8(&args[2]) else {
        return Resp::Null;
    };
    if username != config.username && username != config.viewer_username {
        return Resp::Null;
    }

    let read_only = username == config.viewer_username;
    let commands = if read_only {
        "-@all +graph.explain +graph.list +graph.ro_query +graph.info +graph.memory +module|list +ping +hello +info +dump +exists +ttl +pttl +expiretime"
    } else {
        "+@all"
    };

    Resp::Array(vec![
        bulk("flags"),
        Resp::Array(vec![bulk("on")]),
        bulk("passwords"),
        Resp::Array(Vec::new()),
        bulk("commands"),
        bulk(commands),
        bulk("keys"),
        bulk("~*"),
        bulk("channels"),
        bulk(""),
        bulk("selectors"),
        Resp::Array(Vec::new()),
    ])
}

fn handle_redis_restore(args: &[Vec<u8>], catalog: &GraphCatalog) -> Resp {
    if args.len() < 4 {
        return Resp::Error("ERR wrong number of arguments for 'restore' command".to_string());
    }

    let destination = match utf8(&args[1], "key name") {
        Ok(v) => v,
        Err(err) => return Resp::Error(err),
    };
    let ttl = match utf8(&args[2], "TTL").and_then(|value| {
        value
            .parse::<u64>()
            .map_err(|_| "ERR value is not an integer or out of range".to_string())
    }) {
        Ok(value) => value,
        Err(err) => return Resp::Error(err),
    };

    // Graph database keys are normally persistent. Do not silently discard
    // expiration semantics if somebody RESTOREs an expiring graph.
    if ttl != 0 {
        return Resp::Error(
            "ERR non-zero RESTORE TTL is not supported for native graph keys".to_string(),
        );
    }

    let mut replace = false;
    let mut i = 4usize;
    while i < args.len() {
        let option = ascii_upper(&args[i]);
        match option.as_str() {
            "REPLACE" => {
                replace = true;
                i += 1;
            }
            // MIGRATE does not send these metadata options, but accepting and
            // ignoring them is safe because they affect Redis eviction/LRU
            // metadata rather than graph contents.
            "IDLETIME" | "FREQ" => {
                if i + 1 >= args.len() {
                    return Resp::Error("ERR syntax error".to_string());
                }
                i += 2;
            }
            "ABSTTL" => {
                // With ttl=0 this is semantically identical.
                i += 1;
            }
            _ => return Resp::Error("ERR syntax error".to_string()),
        }
    }

    match catalog.redis_restore(destination, &args[3], replace) {
        Ok(()) => Resp::Simple("OK".to_string()),
        Err(err) if err.starts_with("BUSYKEY") => Resp::Error(err),
        Err(err) => Resp::Error(format!("ERR {err}")),
    }
}

fn handle_auth(args: &[Vec<u8>], config: &ServerConfig, state: &mut ConnectionState) -> Resp {
    let (username, password) = match args.len() {
        2 => (config.username.as_str(), String::from_utf8_lossy(&args[1]).into_owned()),
        3 => (
            match std::str::from_utf8(&args[1]) {
                Ok(v) => v,
                Err(_) => return Resp::Error("WRONGPASS invalid username-password pair".to_string()),
            },
            String::from_utf8_lossy(&args[2]).into_owned(),
        ),
        _ => return Resp::Error("ERR wrong number of arguments for 'auth' command".to_string()),
    };

    let password_matches = |expected: &str| {
        expected.len() == password.len()
            && bool::from(expected.as_bytes().ct_eq(password.as_bytes()))
    };
    let admin_valid = config.password.as_ref().is_none_or(|expected| {
        username == config.username && password_matches(expected)
    });
    let viewer_valid = config.viewer_password.as_ref().is_some_and(|expected| {
        username == config.viewer_username && password_matches(expected)
    });

    if admin_valid {
        state.authenticated = true;
        state.read_only = false;
        state.auth_failures = 0;
        Resp::Simple("OK".to_string())
    } else if viewer_valid {
        state.authenticated = true;
        state.read_only = true;
        state.auth_failures = 0;
        Resp::Simple("OK".to_string())
    } else {
        state.auth_failures = state.auth_failures.saturating_add(1);
        Resp::Error("WRONGPASS invalid username-password pair or user is disabled.".to_string())
    }
}

fn handle_hello(
    args: &[Vec<u8>],
    config: &ServerConfig,
    state: &mut ConnectionState,
) -> Resp {
    let mut requested = 2u8;
    if let Some(proto) = args.get(1).and_then(|v| std::str::from_utf8(v).ok()) {
        match proto {
            "2" => requested = 2,
            "3" => requested = 3,
            _ => return Resp::Error("NOPROTO unsupported protocol version".to_string()),
        }
    }

    let mut i = 2;
    while i < args.len() {
        let option = ascii_upper(&args[i]);
        match option.as_str() {
            "AUTH" if i + 2 < args.len() => {
                let auth_args = vec![b"AUTH".to_vec(), args[i + 1].clone(), args[i + 2].clone()];
                if matches!(handle_auth(&auth_args, config, state), Resp::Error(_)) {
                    return Resp::Error("WRONGPASS invalid username-password pair or user is disabled.".to_string());
                }
                i += 3;
            }
            "SETNAME" if i + 1 < args.len() => {
                state.client_name = Some(String::from_utf8_lossy(&args[i + 1]).into_owned());
                i += 2;
            }
            _ => return Resp::Error("ERR syntax error in HELLO".to_string()),
        }
    }

    if config.password.is_some() && !state.authenticated {
        return Resp::Error("NOAUTH HELLO must be called with the client already authenticated".to_string());
    }

    state.protocol = if requested == 3 {
        RespProtocol::Resp3
    } else {
        RespProtocol::Resp2
    };

    let entries = vec![
        (bulk("server"), bulk("redis")),
        (bulk("version"), bulk("7.2.0")),
        (bulk("proto"), Resp::Int(i64::from(requested))),
        (bulk("id"), Resp::Int(1)),
        (bulk("mode"), bulk("standalone")),
        (bulk("role"), bulk("master")),
        (bulk("modules"), Resp::Array(Vec::new())),
    ];
    if state.protocol == RespProtocol::Resp3 {
        Resp::Map(entries)
    } else {
        Resp::Array(entries.into_iter().flat_map(|(k, v)| [k, v]).collect())
    }
}

fn handle_client(args: &[Vec<u8>], state: &mut ConnectionState) -> Resp {
    let Some(sub) = args.get(1) else {
        return Resp::Error("ERR wrong number of arguments for 'client' command".to_string());
    };
    match ascii_upper(sub).as_str() {
        "SETINFO" => Resp::Simple("OK".to_string()),
        "SETNAME" => {
            if args.len() != 3 {
                return Resp::Error("ERR wrong number of arguments for 'client setname'".to_string());
            }
            state.client_name = Some(String::from_utf8_lossy(&args[2]).into_owned());
            Resp::Simple("OK".to_string())
        }
        "GETNAME" => state
            .client_name
            .as_ref()
            .map_or(Resp::Null, |name| bulk(name)),
        "ID" => Resp::Int(1),
        _ => Resp::Simple("OK".to_string()),
    }
}


fn handle_graph_info(args: &[Vec<u8>]) -> Resp {
    let all = args.len() == 1;
    let mut running = all;
    let mut waiting = all;
    let mut object_pool = all;

    for arg in args.iter().skip(1) {
        match ascii_upper(arg).as_str() {
            "RUNNINGQUERIES" => running = true,
            "WAITINGQUERIES" => waiting = true,
            "OBJECTPOOL" => object_pool = true,
            _ => {}
        }
    }

    if !(running || waiting || object_pool) {
        return bulk("no section found");
    }

    let now = std::time::Instant::now();
    let mut out = Vec::new();

    if running {
        out.push(bulk("# Running queries"));
        out.push(Resp::Array(
            query_scheduler::snapshot_running()
                .into_iter()
                .map(|q| {
                    Resp::Array(vec![
                        bulk("Received at"),
                        Resp::Int(q.received_at),
                        bulk("Graph name"),
                        bulk(q.graph_name),
                        bulk("Query"),
                        bulk(q.query),
                        bulk("Execution duration"),
                        bulk(format!("{:.6}", now.duration_since(q.start).as_secs_f64() * 1000.0)),
                        bulk("Replicated command"),
                        Resp::Int(0),
                    ])
                })
                .collect(),
        ));
    }

    if waiting {
        out.push(bulk("# Waiting queries"));
        out.push(Resp::Array(
            query_scheduler::snapshot_waiting()
                .into_iter()
                .map(|q| {
                    Resp::Array(vec![
                        bulk("Received at"),
                        Resp::Int(q.received_at),
                        bulk("Graph name"),
                        bulk(q.graph_name),
                        bulk("Query"),
                        bulk(q.query),
                        bulk("Wait duration"),
                        bulk(format!("{:.6}", now.duration_since(q.enqueued).as_secs_f64() * 1000.0)),
                    ])
                })
                .collect(),
        ));
    }

    if object_pool {
        let (count, avg) = graph::runtime::string_pool::global().stats();
        let avg = if avg.fract() == 0.0 {
            format!("{}", avg as i64)
        } else {
            format!("{avg}")
        };
        out.push(bulk("Object Pool"));
        out.push(Resp::Array(vec![
            Resp::Array(vec![bulk("Unique Objects in Pool"), Resp::Int(count as i64)]),
            Resp::Array(vec![bulk("Average References per Object"), bulk(avg)]),
        ]));
    }

    Resp::Array(out)
}

fn handle_graph_memory(args: &[Vec<u8>], catalog: &GraphCatalog) -> Resp {
    if args.len() != 3 && args.len() != 5 {
        return Resp::Error("ERR wrong number of arguments for 'graph.memory' command".to_string());
    }
    if ascii_upper(&args[1]) != "USAGE" {
        return Resp::Error(
            "ERR unknown subcommand. Try GRAPH.MEMORY USAGE <key> [SAMPLES <count>]"
                .to_string(),
        );
    }

    let name = match utf8(&args[2], "graph name") {
        Ok(v) => v,
        Err(err) => return Resp::Error(err),
    };
    let graph = match catalog.get(name) {
        Some(graph) => graph,
        None => return Resp::Error("ERR Graph does not exist".to_string()),
    };

    let samples = if args.len() == 5 {
        if ascii_upper(&args[3]) != "SAMPLES" {
            return Resp::Error("ERR expected SAMPLES keyword".to_string());
        }
        match utf8(&args[4], "SAMPLES count").and_then(|v| {
            v.parse::<usize>()
                .map_err(|_| "ERR SAMPLES count must be a positive integer".to_string())
        }) {
            Ok(0) => {
                return Resp::Error("ERR SAMPLES count must be a positive integer".to_string())
            }
            Ok(v) => v,
            Err(err) => return Resp::Error(err),
        }
    } else {
        100
    };

    const MB: usize = 1 << 20;
    let report = graph.memory_usage_report(samples);
    let label_matrices = (report.label_matrices_sz / MB) as i64;
    let relation_matrices = (report.relation_matrices_sz / MB) as i64;
    let node_block = (report.node_block_storage_sz / MB) as i64;
    let unlabeled = (report.unlabeled_node_attr_sz / MB) as i64;
    let edge_block = (report.edge_block_storage_sz / MB) as i64;
    let indices = (report.indices_sz / MB) as i64;

    let mut node_attrs = Vec::new();
    let mut node_attr_sum = 0i64;
    for (name, bytes) in report.node_attr_by_label {
        let mb = (bytes / MB) as i64;
        node_attr_sum += mb;
        node_attrs.push(bulk(name.as_str()));
        node_attrs.push(Resp::Int(mb));
    }

    let mut edge_attrs = Vec::new();
    let mut edge_attr_sum = 0i64;
    for (name, bytes) in report.edge_attr_by_type {
        let mb = (bytes / MB) as i64;
        edge_attr_sum += mb;
        edge_attrs.push(bulk(name.as_str()));
        edge_attrs.push(Resp::Int(mb));
    }

    let total = label_matrices
        + relation_matrices
        + node_block
        + node_attr_sum
        + unlabeled
        + edge_block
        + edge_attr_sum
        + indices;

    Resp::Array(vec![
        bulk("total_graph_sz_mb"),
        Resp::Int(total),
        bulk("label_matrices_sz_mb"),
        Resp::Int(label_matrices),
        bulk("relation_matrices_sz_mb"),
        Resp::Int(relation_matrices),
        bulk("amortized_node_block_sz_mb"),
        Resp::Int(node_block),
        bulk("amortized_node_attributes_by_label_sz_mb"),
        Resp::Array(node_attrs),
        bulk("amortized_unlabeled_nodes_attributes_sz_mb"),
        Resp::Int(unlabeled),
        bulk("amortized_edge_block_sz_mb"),
        Resp::Int(edge_block),
        bulk("amortized_edge_attributes_by_type_sz_mb"),
        Resp::Array(edge_attrs),
        bulk("indices_sz_mb"),
        Resp::Int(indices),
    ])
}

fn handle_graph_constraint(args: &[Vec<u8>], catalog: &GraphCatalog) -> Resp {
    if args.len() < 9 {
        return Resp::Error("ERR wrong number of arguments for 'graph.constraint' command".to_string());
    }

    let create = match ascii_upper(&args[1]).as_str() {
        "CREATE" => true,
        "DROP" => false,
        _ => return Resp::Error("ERR Invalid constraint operation".to_string()),
    };
    let graph_name = match utf8(&args[2], "graph name") {
        Ok(v) => v,
        Err(err) => return Resp::Error(err),
    };
    let constraint_type = match ascii_upper(&args[3]).as_str() {
        "UNIQUE" => ConstraintType::Unique,
        "MANDATORY" => ConstraintType::Mandatory,
        _ => return Resp::Error("ERR Invalid constraint type".to_string()),
    };
    let entity_type = match ascii_upper(&args[4]).as_str() {
        "NODE" => EntityType::Node,
        "RELATIONSHIP" => EntityType::Relationship,
        _ => return Resp::Error("ERR Invalid entity type".to_string()),
    };
    let label = match utf8(&args[5], "constraint label") {
        Ok(v) => v,
        Err(err) => return Resp::Error(err),
    };
    if ascii_upper(&args[6]) != "PROPERTIES" {
        return Resp::Error("ERR Expected PROPERTIES".to_string());
    }
    let count = match utf8(&args[7], "property count").and_then(|v| {
        v.parse::<usize>()
            .map_err(|_| "ERR invalid property count".to_string())
    }) {
        Ok(v) if v > 0 => v,
        Ok(_) => return Resp::Error("ERR constraint must include at least one property".to_string()),
        Err(err) => return Resp::Error(err),
    };
    if args.len() != 8 + count {
        return Resp::Error("ERR property count does not match arguments".to_string());
    }

    let mut properties = Vec::with_capacity(count);
    for value in &args[8..] {
        match utf8(value, "constraint property") {
            Ok(v) => properties.push(v.to_string()),
            Err(err) => return Resp::Error(err),
        }
    }

    let graph = if create {
        match catalog.get_or_create(graph_name) {
            Ok(graph) => graph,
            Err(err) => return Resp::Error(format!("ERR {err}")),
        }
    } else {
        match catalog.get(graph_name) {
            Some(graph) => graph,
            None => return Resp::Error("ERR Invalid graph operation on empty key".to_string()),
        }
    };

    match graph.mutate_constraint(
        create,
        constraint_type,
        entity_type,
        label,
        &properties,
    ) {
        Ok(()) => Resp::Simple("OK".to_string()),
        Err(err) => Resp::Error(format!("ERR {err}")),
    }
}

fn handle_graph_udf(args: &[Vec<u8>], catalog: &GraphCatalog) -> Resp {
    if args.len() < 2 {
        return Resp::Error("ERR wrong number of arguments for 'graph.udf' command".to_string());
    }

    match ascii_upper(&args[1]).as_str() {
        "LOAD" => {
            if !(args.len() == 4 || args.len() == 5) {
                return Resp::Error(
                    "ERR wrong number of arguments for 'GRAPH.UDF LOAD' command".to_string(),
                );
            }

            let (replace, name_idx, script_idx) = if args.len() == 5 {
                if ascii_upper(&args[2]) != "REPLACE" {
                    return Resp::Error(format!(
                        "ERR Unknown option given: '{}'",
                        String::from_utf8_lossy(&args[2])
                    ));
                }
                (true, 3usize, 4usize)
            } else {
                (false, 2usize, 3usize)
            };

            let name = match utf8(&args[name_idx], "UDF library name") {
                Ok(v) => v,
                Err(err) => return Resp::Error(err),
            };
            let script = match utf8(&args[script_idx], "UDF script") {
                Ok(v) => v,
                Err(err) => return Resp::Error(err),
            };

            match udf_store::load(catalog.data_dir(), name, script, replace) {
                Ok(_) => Resp::Simple("OK".to_string()),
                Err(err) => Resp::Error(format!("ERR {err}")),
            }
        }
        "DELETE" => {
            if args.len() != 3 {
                return Resp::Error(
                    "ERR wrong number of arguments for 'GRAPH.UDF DELETE' command".to_string(),
                );
            }
            let name = match utf8(&args[2], "UDF library name") {
                Ok(v) => v,
                Err(err) => return Resp::Error(err),
            };
            match udf_store::delete(catalog.data_dir(), name) {
                Ok(()) => Resp::Simple("OK".to_string()),
                Err(err) => Resp::Error(format!("ERR {err}")),
            }
        }
        "FLUSH" => {
            if args.len() != 2 {
                return Resp::Error(
                    "ERR wrong number of arguments for 'GRAPH.UDF FLUSH' command".to_string(),
                );
            }
            match udf_store::flush(catalog.data_dir()) {
                Ok(()) => Resp::Simple("OK".to_string()),
                Err(err) => Resp::Error(format!("ERR {err}")),
            }
        }
        "LIST" => {
            let mut filter: Option<&str> = None;
            let mut with_code = false;

            for arg in args.iter().skip(2) {
                let value = match utf8(arg, "UDF LIST option") {
                    Ok(v) => v,
                    Err(err) => return Resp::Error(err),
                };
                if value.eq_ignore_ascii_case("WITHCODE") {
                    with_code = true;
                } else if filter.is_none() {
                    filter = Some(value);
                } else {
                    return Resp::Error(format!("ERR Unknown option given: '{value}'"));
                }
            }

            let libraries = graph::udf::get_udf_repo().list(filter, with_code);
            Resp::Array(
                libraries
                    .into_iter()
                    .map(|library| {
                        let mut fields = vec![
                            bulk("library_name"),
                            bulk(library.name),
                            bulk("functions"),
                            Resp::Array(library.function_names.into_iter().map(bulk).collect()),
                        ];
                        if let Some(code) = library.code {
                            fields.push(bulk("library_code"));
                            fields.push(bulk(code));
                        }
                        Resp::Array(fields)
                    })
                    .collect(),
            )
        }
        subcommand => Resp::Error(format!("ERR Unknown UDF subcommand: {subcommand}")),
    }
}

fn handle_graph_config(args: &[Vec<u8>]) -> Resp {
    if args.len() < 2 {
        return Resp::Error("ERR wrong number of arguments for 'graph.config' command".to_string());
    }

    match ascii_upper(&args[1]).as_str() {
        "GET" => {
            if args.len() != 3 {
                return Resp::Error("ERR wrong number of arguments for 'graph.config get'".to_string());
            }
            let name = match utf8(&args[2], "configuration name") {
                Ok(v) => v,
                Err(err) => return Resp::Error(err),
            };

            if name == "*" {
                return Resp::Array(
                    native_config::CONFIG_NAMES
                        .iter()
                        .map(|name| {
                            Resp::Array(vec![
                                bulk(*name),
                                config_value_resp(
                                    native_config::get(name)
                                        .expect("known native config must resolve"),
                                ),
                            ])
                        })
                        .collect(),
                );
            }

            match native_config::get(name) {
                Ok(value) => Resp::Array(vec![bulk(name.to_ascii_uppercase()), config_value_resp(value)]),
                Err(err) => Resp::Error(format!("ERR {err}")),
            }
        }
        "SET" => {
            if args.len() < 4 {
                return Resp::Error("ERR Missing configuration parameter name or value".to_string());
            }
            if (args.len() - 2) % 2 != 0 {
                return Resp::Error("ERR Missing value for configuration parameter".to_string());
            }

            let mut pairs = Vec::with_capacity((args.len() - 2) / 2);
            for chunk in args[2..].chunks_exact(2) {
                let name = match utf8(&chunk[0], "configuration name") {
                    Ok(v) => v.to_string(),
                    Err(err) => return Resp::Error(err),
                };
                let value = match utf8(&chunk[1], "configuration value") {
                    Ok(v) => v.to_string(),
                    Err(err) => return Resp::Error(err),
                };
                pairs.push((name, value));
            }

            match native_config::set_many(&pairs) {
                Ok(()) => Resp::Simple("OK".to_string()),
                Err(err) => Resp::Error(format!("ERR {err}")),
            }
        }
        _ => Resp::Error("ERR Unknown subcommand for GRAPH.CONFIG".to_string()),
    }
}

fn config_value_resp(value: native_config::ConfigValue) -> Resp {
    match value {
        native_config::ConfigValue::Int(value) => Resp::Int(value),
        native_config::ConfigValue::Text(value) => bulk(value),
    }
}


fn handle_graph_explain(args: &[Vec<u8>], catalog: &GraphCatalog) -> Resp {
    if args.len() < 3 {
        return Resp::Error("ERR wrong number of arguments for 'graph.explain' command".to_string());
    }
    let name = match utf8(&args[1], "graph name") {
        Ok(v) => v,
        Err(err) => return Resp::Error(err),
    };
    let query = match utf8(&args[2], "query") {
        Ok(v) => v,
        Err(err) => return Resp::Error(err),
    };
    let Some(graph) = catalog.get(name) else {
        return Resp::Error("ERR Invalid graph operation on empty key".to_string());
    };
    match graph.explain(query) {
        Ok(lines) => Resp::Array(lines.into_iter().map(bulk).collect()),
        Err(err) => Resp::Error(format!("ERR {err}")),
    }
}

fn handle_graph_profile(args: &[Vec<u8>], catalog: &GraphCatalog) -> Resp {
    if args.len() < 3 {
        return Resp::Error("ERR wrong number of arguments for 'graph.profile' command".to_string());
    }
    let name = match utf8(&args[1], "graph name") {
        Ok(v) => v,
        Err(err) => return Resp::Error(err),
    };
    let query = match utf8(&args[2], "query") {
        Ok(v) => v,
        Err(err) => return Resp::Error(err),
    };
    let graph = match catalog.get_or_create(name) {
        Ok(graph) => graph,
        Err(err) => return Resp::Error(format!("ERR {err}")),
    };
    match graph.profile(query) {
        Ok(lines) => Resp::Array(lines.into_iter().map(bulk).collect()),
        Err(err) => Resp::Error(format!("ERR {err}")),
    }
}

fn handle_graph_slowlog(args: &[Vec<u8>], catalog: &GraphCatalog) -> Resp {
    if !(args.len() == 2 || args.len() == 3) {
        return Resp::Error("ERR wrong number of arguments for 'graph.slowlog' command".to_string());
    }
    let name = match utf8(&args[1], "graph name") {
        Ok(v) => v,
        Err(err) => return Resp::Error(err),
    };
    let Some(graph) = catalog.get(name) else {
        return Resp::Error("ERR Invalid graph operation on empty key".to_string());
    };

    if args.len() == 3 {
        if ascii_upper(&args[2]) != "RESET" {
            return Resp::Error("ERR Unknown subcommand".to_string());
        }
        graph.slowlog_reset();
        return Resp::Simple("OK".to_string());
    }

    Resp::Array(
        graph
            .slowlog_entries()
            .into_iter()
            .map(|entry| {
                Resp::Array(vec![
                    bulk(format!("{:.0}", entry.timestamp)),
                    bulk(entry.command),
                    bulk(entry.query),
                    bulk(format!("{:.5}", entry.latency_ms)),
                    entry.params.map_or(Resp::Null, bulk),
                ])
            })
            .collect(),
    )
}

fn handle_graph_bulk(args: &[Vec<u8>], catalog: &GraphCatalog) -> Resp {
    if args.len() < 6 {
        return Resp::Error("ERR wrong number of arguments for 'graph.bulk' command".to_string());
    }

    let name = match utf8(&args[1], "graph name") {
        Ok(v) => v.to_string(),
        Err(err) => return Resp::Error(err),
    };
    let request = match crate::bulk::parse_request(&args[2..]) {
        Ok(request) => request,
        Err(err) => return Resp::Error(format!("ERR {err}")),
    };

    let existed = catalog.contains(&name);
    if request.begin && existed {
        return Resp::Error(format!(
            "ERR Graph with name '{name}' cannot be created, as key '{name}' already exists."
        ));
    }
    if !request.begin && !existed {
        return Resp::Error("ERR Invalid graph operation on empty key".to_string());
    }

    let graph = if existed {
        match catalog.get(&name) {
            Some(graph) => graph,
            None => return Resp::Error("ERR Invalid graph operation on empty key".to_string()),
        }
    } else {
        match catalog.get_or_create(&name) {
            Ok(graph) => graph,
            Err(err) => return Resp::Error(format!("ERR {err}")),
        }
    };

    match graph.bulk_insert(&request) {
        Ok(reply) => Resp::Simple(reply),
        Err(err) => {
            drop(graph);
            if request.begin {
                let _ = catalog.delete(&name);
            }
            Resp::Error(if err.starts_with("ERR ") { err } else { format!("ERR {err}") })
        }
    }
}

fn handle_graph_query(args: &[Vec<u8>], catalog: &GraphCatalog, read_only: bool) -> Resp {
    if args.len() < 3 {
        return Resp::Error("ERR wrong number of arguments for graph query".to_string());
    }
    let name = match utf8(&args[1], "graph name") {
        Ok(v) => v,
        Err(err) => return Resp::Error(err),
    };
    let query = match utf8(&args[2], "query") {
        Ok(v) => v,
        Err(err) => return Resp::Error(err),
    };

    let mut timeout: Option<i64> = None;
    let mut version: Option<u64> = None;
    let mut i = 3usize;
    while i < args.len() {
        let option = ascii_upper(&args[i]);
        match option.as_str() {
            "--COMPACT" | "--TRACK-MEMORY" => {
                i += 1;
            }
            "TIMEOUT" => {
                if i + 1 >= args.len() {
                    return Resp::Error("ERR missing TIMEOUT value".to_string());
                }
                timeout = match utf8(&args[i + 1], "timeout").and_then(|v| {
                    v.parse::<i64>()
                        .map_err(|_| "ERR invalid TIMEOUT value".to_string())
                }) {
                    Ok(v) => Some(v),
                    Err(err) => return Resp::Error(err),
                };
                i += 2;
            }
            "VERSION" => {
                if i + 1 >= args.len() {
                    return Resp::Error("ERR missing VERSION value".to_string());
                }
                version = match utf8(&args[i + 1], "version").and_then(|v| {
                    v.parse::<u64>()
                        .map_err(|_| "ERR invalid VERSION value".to_string())
                }) {
                    Ok(v) => Some(v),
                    Err(err) => return Resp::Error(err),
                };
                i += 2;
            }
            _ => {
                // Upstream ignores unknown trailing query flags rather than
                // rejecting an otherwise valid query.
                i += 1;
            }
        }
    }

    let existing = catalog.get(name);
    if let (Some(graph), Some(expected)) = (&existing, version) {
        let current = graph.schema_version();
        if current != expected {
            return Resp::Array(vec![
                Resp::Error("ERR invalid graph version".to_string()),
                Resp::Int(current as i64),
            ]);
        }
    }

    let graph = if read_only {
        match existing {
            Some(graph) => graph,
            None => return Resp::Error("ERR Invalid graph operation on empty key".to_string()),
        }
    } else if let Some(graph) = existing {
        graph
    } else {
        if let Some(expected) = version
            && expected != 0
        {
            return Resp::Array(vec![
                Resp::Error("ERR invalid graph version".to_string()),
                Resp::Int(0),
            ]);
        }
        match catalog.get_or_create(name) {
            Ok(graph) => graph,
            Err(err) => return Resp::Error(format!("ERR {err}")),
        }
    };

    let result = if read_only {
        graph.query_read_only_with_timeout(query, timeout)
    } else {
        graph.query_with_timeout(query, timeout)
    };

    match result {
        Ok(output) => compact_query_response(&output),
        Err(err) => Resp::Error(format!("ERR {err}")),
    }
}

fn compact_query_response(output: &QueryOutput) -> Resp {
    let stats = stats_response(&output.stats, output.graph_version);
    if output.columns.is_empty() {
        return Resp::Array(vec![stats]);
    }

    let header = Resp::Array(
        output
            .columns
            .iter()
            .map(|column| Resp::Array(vec![Resp::Int(1), bulk(column)]))
            .collect(),
    );
    let rows = Resp::Array(
        output
            .wire_rows
            .iter()
            .map(|row| Resp::Array(row.iter().map(compact_cell).collect()))
            .collect(),
    );
    Resp::Array(vec![header, rows, stats])
}

fn compact_cell(value: &WireValue) -> Resp {
    let (kind, inner) = match value {
        WireValue::Null => (1, Resp::Null),
        WireValue::String(v) => (2, bulk(v)),
        WireValue::Int(v) => (3, Resp::Int(*v)),
        WireValue::Bool(v) => (4, bulk(if *v { "true" } else { "false" })),
        WireValue::Float(v) => (5, bulk(format_float(*v))),
        WireValue::List(values) => (
            6,
            Resp::Array(values.iter().map(compact_cell).collect()),
        ),
        WireValue::Relationship {
            id,
            type_id,
            src,
            dst,
            properties,
        } => (
            7,
            Resp::Array(vec![
                Resp::Int(*id as i64),
                Resp::Int(*type_id as i64),
                Resp::Int(*src as i64),
                Resp::Int(*dst as i64),
                Resp::Array(
                    properties
                        .iter()
                        .map(|(attr, value)| {
                            let cell = compact_cell(value);
                            let Resp::Array(mut pair) = cell else { unreachable!() };
                            let mut prop = vec![Resp::Int(*attr as i64)];
                            prop.append(&mut pair);
                            Resp::Array(prop)
                        })
                        .collect(),
                ),
            ]),
        ),
        WireValue::Node {
            id,
            labels,
            properties,
        } => (
            8,
            Resp::Array(vec![
                Resp::Int(*id as i64),
                Resp::Array(labels.iter().map(|v| Resp::Int(*v as i64)).collect()),
                Resp::Array(
                    properties
                        .iter()
                        .map(|(attr, value)| {
                            let cell = compact_cell(value);
                            let Resp::Array(mut pair) = cell else { unreachable!() };
                            let mut prop = vec![Resp::Int(*attr as i64)];
                            prop.append(&mut pair);
                            Resp::Array(prop)
                        })
                        .collect(),
                ),
            ]),
        ),
        WireValue::Path {
            nodes,
            relationships,
        } => (
            9,
            Resp::Array(vec![
                Resp::Array(vec![
                    Resp::Int(6),
                    Resp::Array(nodes.iter().map(compact_cell).collect()),
                ]),
                Resp::Array(vec![
                    Resp::Int(6),
                    Resp::Array(relationships.iter().map(compact_cell).collect()),
                ]),
            ]),
        ),
        WireValue::Map(values) => {
            let mut entries = Vec::with_capacity(values.len() * 2);
            for (key, value) in values {
                entries.push(bulk(key));
                entries.push(compact_cell(value));
            }
            (10, Resp::Array(entries))
        }
        WireValue::Point { latitude, longitude } => (
            11,
            Resp::Array(vec![bulk(format_float(*latitude)), bulk(format_float(*longitude))]),
        ),
        WireValue::VecF32(values) => (
            12,
            Resp::Array(
                values
                    .iter()
                    .map(|v| bulk(format_float(f64::from(*v))))
                    .collect(),
            ),
        ),
        WireValue::Datetime(v) => (13, Resp::Int(*v)),
        WireValue::Date(v) => (14, Resp::Int(*v)),
        WireValue::Time(v) => (15, Resp::Int(*v)),
        WireValue::Duration(v) => (16, Resp::Int(*v)),
    };
    Resp::Array(vec![Resp::Int(kind), inner])
}

fn stats_response(stats: &OutputStats, version: u64) -> Resp {
    let mut out = Vec::new();
    if stats.labels_added > 0 {
        out.push(bulk(format!("Labels added: {}", stats.labels_added)));
    }
    if stats.labels_removed > 0 {
        out.push(bulk(format!("Labels removed: {}", stats.labels_removed)));
    }
    if stats.nodes_created > 0 {
        out.push(bulk(format!("Nodes created: {}", stats.nodes_created)));
    }
    if stats.properties_set > 0 {
        out.push(bulk(format!("Properties set: {}", stats.properties_set)));
    }
    if stats.properties_removed > 0 {
        out.push(bulk(format!("Properties removed: {}", stats.properties_removed)));
    }
    if stats.relationships_created > 0 {
        out.push(bulk(format!("Relationships created: {}", stats.relationships_created)));
    }
    if stats.nodes_deleted > 0 {
        out.push(bulk(format!("Nodes deleted: {}", stats.nodes_deleted)));
    }
    if stats.relationships_deleted > 0 {
        out.push(bulk(format!("Relationships deleted: {}", stats.relationships_deleted)));
    }
    if stats.indexes_created > 0 {
        out.push(bulk(format!("Indices created: {}", stats.indexes_created)));
    }
    if stats.indexes_dropped > 0 {
        out.push(bulk(format!("Indices deleted: {}", stats.indexes_dropped)));
    }
    out.push(bulk(format!("Cached execution: {}", i32::from(stats.cached))));
    out.push(bulk(format!(
        "Query internal execution time: {:.6} milliseconds",
        stats.execution_time_ms
    )));
    out.push(bulk(format!("Graph version: {version}")));
    Resp::Array(out)
}

fn format_float(value: f64) -> String {
    // FalkorDB uses %.15g on the wire. Rust's shortest round-trippable decimal
    // is accepted by the official clients and preserves the same numeric value.
    format!("{value}")
}

fn require_arity(args: &[Vec<u8>], expected: usize) -> Result<(), String> {
    if args.len() == expected {
        Ok(())
    } else {
        Err("ERR wrong number of arguments".to_string())
    }
}

fn graph_query_count(graph: &NativeGraph, cypher: &str) -> Result<u64, String> {
    let output = graph.query_read_only(cypher)?;
    let value = output
        .wire_rows
        .first()
        .and_then(|row| row.first())
        .ok_or_else(|| format!("count query returned no value: {cypher}"))?;
    match value {
        WireValue::Int(v) if *v >= 0 => Ok(*v as u64),
        WireValue::Int(v) => Err(format!("count query returned negative integer {v}: {cypher}")),
        other => Err(format!("count query returned non-integer {other:?}: {cypher}")),
    }
}


fn utf8<'a>(value: &'a [u8], what: &str) -> Result<&'a str, String> {
    std::str::from_utf8(value).map_err(|_| format!("ERR {what} must be UTF-8"))
}

fn ascii_upper(value: &[u8]) -> String {
    String::from_utf8_lossy(value).to_ascii_uppercase()
}

fn bulk(value: impl AsRef<[u8]>) -> Resp {
    Resp::Bulk(value.as_ref().to_vec())
}

#[derive(Debug)]
enum Resp {
    Simple(String),
    Error(String),
    Int(i64),
    Bulk(Vec<u8>),
    Null,
    Array(Vec<Resp>),
    Map(Vec<(Resp, Resp)>),
}

fn write_resp(writer: &mut impl Write, value: &Resp, protocol: RespProtocol) -> std::io::Result<()> {
    match value {
        Resp::Simple(v) => write!(writer, "+{}\r\n", sanitize_line(v)),
        Resp::Error(v) => write!(writer, "-{}\r\n", sanitize_line(v)),
        Resp::Int(v) => write!(writer, ":{v}\r\n"),
        Resp::Bulk(v) => {
            write!(writer, "${}\r\n", v.len())?;
            writer.write_all(v)?;
            writer.write_all(b"\r\n")
        }
        Resp::Null => {
            if protocol == RespProtocol::Resp3 {
                writer.write_all(b"_\r\n")
            } else {
                writer.write_all(b"$-1\r\n")
            }
        }
        Resp::Array(values) => {
            write!(writer, "*{}\r\n", values.len())?;
            for item in values {
                write_resp(writer, item, protocol)?;
            }
            Ok(())
        }
        Resp::Map(entries) => {
            if protocol == RespProtocol::Resp3 {
                write!(writer, "%{}\r\n", entries.len())?;
                for (key, value) in entries {
                    write_resp(writer, key, protocol)?;
                    write_resp(writer, value, protocol)?;
                }
                Ok(())
            } else {
                write!(writer, "*{}\r\n", entries.len() * 2)?;
                for (key, value) in entries {
                    write_resp(writer, key, protocol)?;
                    write_resp(writer, value, protocol)?;
                }
                Ok(())
            }
        }
    }
}

fn sanitize_line(value: &str) -> String {
    value.replace(['\r', '\n'], " ")
}

fn read_until_limited<R: BufRead>(
    reader: &mut R,
    delimiter: u8,
    limit: usize,
    what: &str,
) -> Result<Vec<u8>, String> {
    let mut out = Vec::with_capacity(limit.min(1024));
    let mut limited = reader.take((limit + 1) as u64);
    let read = limited
        .read_until(delimiter, &mut out)
        .map_err(|e| format!("read {what}: {e}"))?;
    if read > limit {
        return Err(format!("{what} exceeds {limit} bytes"));
    }
    if read > 0 && out.last().copied() != Some(delimiter) {
        return Err(format!("unterminated {what}"));
    }
    Ok(out)
}

fn read_command<R: BufRead>(reader: &mut R) -> Result<Option<Vec<Vec<u8>>>, String> {
    let mut first = read_until_limited(reader, b'\n', MAX_RESP_LINE_BYTES, "RESP command line")?;
    if first.is_empty() {
        return Ok(None);
    }
    trim_crlf(&mut first);

    if first.first() == Some(&b'*') {
        let count = parse_len(&first[1..], "array length")?;
        if count > MAX_RESP_ARRAY_ITEMS {
            return Err(format!(
                "RESP array length {count} exceeds maximum {MAX_RESP_ARRAY_ITEMS}"
            ));
        }
        let mut args = Vec::with_capacity(count);
        let mut total_bytes = first.len();
        for _ in 0..count {
            let arg = read_bulkish(reader)?;
            total_bytes = total_bytes
                .checked_add(arg.len())
                .ok_or_else(|| "RESP command size overflow".to_string())?;
            if total_bytes > MAX_RESP_COMMAND_BYTES {
                return Err(format!(
                    "RESP command exceeds maximum {MAX_RESP_COMMAND_BYTES} bytes"
                ));
            }
            args.push(arg);
        }
        return Ok(Some(args));
    }

    if first.len() > MAX_RESP_LINE_BYTES {
        return Err(format!(
            "inline command exceeds maximum {MAX_RESP_LINE_BYTES} bytes"
        ));
    }

    let line = std::str::from_utf8(&first)
        .map_err(|_| "inline command is not UTF-8".to_string())?;
    Ok(Some(
        line.split_ascii_whitespace()
            .map(|s| s.as_bytes().to_vec())
            .collect(),
    ))
}

fn read_bulkish<R: BufRead>(reader: &mut R) -> Result<Vec<u8>, String> {
    let mut header =
        read_until_limited(reader, b'\n', MAX_RESP_LINE_BYTES, "RESP item header")?;
    if header.is_empty() {
        return Err("unexpected EOF inside command".to_string());
    }
    trim_crlf(&mut header);

    match header.first().copied() {
        Some(b'$') => {
            let len = parse_len(&header[1..], "bulk length")?;
            if len > MAX_RESP_BULK_BYTES {
                return Err(format!(
                    "RESP bulk length {len} exceeds maximum {MAX_RESP_BULK_BYTES}"
                ));
            }
            let mut data = vec![0u8; len];
            reader
                .read_exact(&mut data)
                .map_err(|e| format!("read bulk payload: {e}"))?;
            let mut crlf = [0u8; 2];
            reader
                .read_exact(&mut crlf)
                .map_err(|e| format!("read bulk terminator: {e}"))?;
            if crlf != *b"\r\n" {
                return Err("invalid bulk string terminator".to_string());
            }
            Ok(data)
        }
        Some(b'+') | Some(b':') => {
            let data = header[1..].to_vec();
            if data.len() > MAX_RESP_LINE_BYTES {
                return Err("RESP scalar item is too large".to_string());
            }
            Ok(data)
        }
        _ => Err(format!(
            "unsupported RESP request item: {}",
            String::from_utf8_lossy(&header)
        )),
    }
}

fn parse_len(bytes: &[u8], what: &str) -> Result<usize, String> {
    let s = std::str::from_utf8(bytes).map_err(|_| format!("invalid {what}"))?;
    if s.is_empty() || !s.bytes().all(|b| b.is_ascii_digit()) {
        return Err(format!("invalid {what}: {s}"));
    }
    s.parse::<usize>().map_err(|_| format!("invalid {what}: {s}"))
}

fn trim_crlf(buf: &mut Vec<u8>) {
    if buf.ends_with(b"\n") {
        buf.pop();
    }
    if buf.ends_with(b"\r") {
        buf.pop();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resp_parser_rejects_oversized_array_before_allocation() {
        let raw = format!("*{}\r\n", MAX_RESP_ARRAY_ITEMS + 1);
        let mut reader = BufReader::new(std::io::Cursor::new(raw.as_bytes()));
        let err = match read_command(&mut reader) {
            Ok(_) => panic!("oversized RESP array must fail"),
            Err(err) => err,
        };
        assert!(err.contains("array length"));
        assert!(err.contains("exceeds maximum"));
    }

    #[test]
    fn resp_parser_rejects_oversized_bulk_before_allocation() {
        let raw = format!("*1\r\n${}\r\n", MAX_RESP_BULK_BYTES + 1);
        let mut reader = BufReader::new(std::io::Cursor::new(raw.as_bytes()));
        let err = match read_command(&mut reader) {
            Ok(_) => panic!("oversized RESP bulk must fail"),
            Err(err) => err,
        };
        assert!(err.contains("bulk length"));
        assert!(err.contains("exceeds maximum"));
    }

    #[test]
    fn repeated_bad_auth_is_counted_and_success_resets_counter() {
        let config = ServerConfig {
            password: Some("admin-secret".to_string()),
            ..ServerConfig::default()
        };
        let mut state = ConnectionState {
            authenticated: false,
            read_only: false,
            protocol: RespProtocol::Resp2,
            client_name: None,
            auth_failures: 0,
        };

        for expected in 1..=3 {
            let bad = vec![b"AUTH".to_vec(), b"wrong-secret".to_vec()];
            assert!(matches!(handle_auth(&bad, &config, &mut state), Resp::Error(_)));
            assert_eq!(state.auth_failures, expected);
        }

        let good = vec![b"AUTH".to_vec(), b"admin-secret".to_vec()];
        assert!(matches!(handle_auth(&good, &config, &mut state), Resp::Simple(_)));
        assert_eq!(state.auth_failures, 0);
        assert!(state.authenticated);
    }

    #[test]
    fn graph_name_filename_roundtrip() {
        for name in ["main", "T6 / Nuketown", "Δ-graph"] {
            assert_eq!(decode_graph_name(&encode_graph_name(name)).unwrap(), name);
        }
    }

    #[test]
    fn missing_storage_path_counts_as_zero() {
        let path = std::env::temp_dir().join(format!(
            "falkordb-missing-storage-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&path);
        assert_eq!(directory_size_bytes(&path).unwrap(), 0);
    }

    #[test]
    fn viewer_credentials_require_distinct_admin_credentials() {
        let no_admin = ServerConfig {
            viewer_password: Some("viewer-secret".to_string()),
            ..ServerConfig::default()
        };
        assert!(no_admin.validate().is_err());

        let same_user = ServerConfig {
            password: Some("admin-secret".to_string()),
            viewer_username: "default".to_string(),
            viewer_password: Some("viewer-secret".to_string()),
            ..ServerConfig::default()
        };
        assert!(same_user.validate().is_err());

        let same_secret = ServerConfig {
            password: Some("shared-secret".to_string()),
            viewer_password: Some("shared-secret".to_string()),
            ..ServerConfig::default()
        };
        assert!(same_secret.validate().is_err());

        let valid = ServerConfig {
            password: Some("admin-secret".to_string()),
            viewer_username: "viewer".to_string(),
            viewer_password: Some("viewer-secret".to_string()),
            ..ServerConfig::default()
        };
        assert!(valid.validate().is_ok());
    }

    #[test]
    fn remote_unauthenticated_bind_is_rejected() {
        let config = ServerConfig {
            bind: "0.0.0.0:6379".parse().unwrap(),
            allow_plaintext_remote: true,
            ..ServerConfig::default()
        };
        assert!(config.validate().is_err());
    }

    #[test]
    fn viewer_auth_is_read_only_and_browser_compatible() {
        let config = ServerConfig {
            username: "default".to_string(),
            password: Some("admin-secret".to_string()),
            viewer_username: "viewer".to_string(),
            viewer_password: Some("viewer-secret".to_string()),
            ..ServerConfig::default()
        };
        let mut state = ConnectionState {
            authenticated: false,
            read_only: false,
            protocol: RespProtocol::Resp2,
            client_name: None,
            auth_failures: 0,
        };

        let auth = vec![
            b"AUTH".to_vec(),
            b"viewer".to_vec(),
            b"viewer-secret".to_vec(),
        ];
        assert!(matches!(
            handle_auth(&auth, &config, &mut state),
            Resp::Simple(ref value) if value == "OK"
        ));
        assert!(state.authenticated);
        assert!(state.read_only);

        assert!(read_only_command_allowed("GRAPH.RO_QUERY", &[]));
        assert!(read_only_command_allowed("GRAPH.MEMORY", &[]));
        assert!(read_only_command_allowed("MODULE", &[]));
        assert!(!read_only_command_allowed("GRAPH.QUERY", &[]));
        assert!(!read_only_command_allowed("GRAPH.DELETE", &[]));
        assert!(!read_only_command_allowed("RESTORE", &[]));

        let acl = vec![
            b"ACL".to_vec(),
            b"GETUSER".to_vec(),
            b"viewer".to_vec(),
        ];
        assert!(matches!(
            handle_acl(&acl, &config, &state),
            Resp::Error(ref value) if value.starts_with("NOPERM")
        ));
    }

    #[test]
    fn remote_plaintext_bind_is_rejected() {
        let config = ServerConfig {
            bind: "0.0.0.0:6379".parse().unwrap(),
            password: Some("secret".to_string()),
            ..ServerConfig::default()
        };
        assert!(config.validate().is_err());
    }
}
