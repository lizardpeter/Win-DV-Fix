use std::{
    collections::HashMap,
    ffi::CString,
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    path::{Path, PathBuf},
    sync::Arc,
};

use crc32fast::Hasher;
use graph::{
    entity_type::EntityType,
    graph::{
        attribute_store::{AttrNameMap, AttributeStore},
        constraint::{Constraint, ConstraintStatus, ConstraintType},
        graph::Graph,
        graphblas::{
            serialization::{
                Decode, Encode, EncodeState, PayloadEntry, Reader, Writer, index_field_type,
            },
            tensor::Tensor,
            versioned_matrix::VersionedMatrix,
        },
    },
    index::{
        Field, IndexInfo, IndexType, TextIndexOptions, VectorIndexOptions,
        indexer::IndexOptions,
    },
};
use roaring::RoaringTreemap;

const FILE_MAGIC: &[u8; 4] = b"FGSN";
const FILE_VERSION: u16 = 1;
const GRAPH_FORMAT_VERSION: u64 = 19;
const FILE_HEADER_LEN: usize = 4 + 2 + 8 + 4 + 8 + 4;

const TYPE_BYTES: u8 = 0;
const TYPE_DOUBLE: u8 = 2;
const TYPE_SIGNED: u8 = 3;
const TYPE_UNSIGNED: u8 = 4;

pub struct LoadedSnapshot {
    pub sequence: u64,
    pub graph: Graph,
    pub path: PathBuf,
}

pub fn write_checkpoint(
    wal_path: &Path,
    graph_name: &str,
    sequence: u64,
    graph: &Graph,
) -> Result<PathBuf, String> {
    let payload = save_graph(graph, graph_name);
    let crc = snapshot_crc(sequence, graph_name.as_bytes(), &payload);
    let final_path = checkpoint_path(wal_path, sequence);
    let temp_path = final_path.with_extension(format!(
        "fgs.tmp.{}",
        std::process::id()
    ));

    let mut header = Vec::with_capacity(FILE_HEADER_LEN);
    header.extend_from_slice(FILE_MAGIC);
    header.extend_from_slice(&FILE_VERSION.to_le_bytes());
    header.extend_from_slice(&sequence.to_le_bytes());
    header.extend_from_slice(&(graph_name.len() as u32).to_le_bytes());
    header.extend_from_slice(&(payload.len() as u64).to_le_bytes());
    header.extend_from_slice(&crc.to_le_bytes());

    {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temp_path)
            .map_err(|e| format!("create checkpoint {}: {e}", temp_path.display()))?;
        file.write_all(&header)
            .and_then(|_| file.write_all(graph_name.as_bytes()))
            .and_then(|_| file.write_all(&payload))
            .and_then(|_| file.flush())
            .map_err(|e| format!("write checkpoint {}: {e}", temp_path.display()))?;
        file.sync_all()
            .map_err(|e| format!("sync checkpoint {}: {e}", temp_path.display()))?;
    }

    fs::rename(&temp_path, &final_path).map_err(|e| {
        let _ = fs::remove_file(&temp_path);
        format!(
            "publish checkpoint {} -> {}: {e}",
            temp_path.display(),
            final_path.display()
        )
    })?;

    Ok(final_path)
}

pub fn load_latest(
    wal_path: &Path,
    expected_graph_name: &str,
) -> Result<Option<LoadedSnapshot>, String> {
    let Some(parent) = wal_path.parent() else {
        return Ok(None);
    };
    if !parent.exists() {
        return Ok(None);
    }

    let prefix = checkpoint_prefix(wal_path);
    let mut candidates = Vec::new();
    for entry in fs::read_dir(parent)
        .map_err(|e| format!("scan checkpoint directory {}: {e}", parent.display()))?
    {
        let entry = entry.map_err(|e| format!("read checkpoint directory entry: {e}"))?;
        let name = entry.file_name();
        let name = name.to_string_lossy();
        let Some(rest) = name.strip_prefix(&prefix) else {
            continue;
        };
        let Some(seq) = rest.strip_suffix(".fgs") else {
            continue;
        };
        let Ok(sequence) = seq.parse::<u64>() else {
            continue;
        };
        candidates.push((sequence, entry.path()));
    }

    candidates.sort_unstable_by(|a, b| b.0.cmp(&a.0));
    let Some((filename_sequence, path)) = candidates.into_iter().next() else {
        return Ok(None);
    };

    let bytes = fs::read(&path)
        .map_err(|e| format!("read checkpoint {}: {e}", path.display()))?;
    if bytes.len() < FILE_HEADER_LEN {
        return Err(format!("checkpoint {} is truncated", path.display()));
    }

    let mut pos = 0usize;
    if &bytes[pos..pos + 4] != FILE_MAGIC {
        return Err(format!("checkpoint {} has bad magic", path.display()));
    }
    pos += 4;

    let version = u16::from_le_bytes(bytes[pos..pos + 2].try_into().unwrap());
    pos += 2;
    if version != FILE_VERSION {
        return Err(format!(
            "checkpoint {} uses unsupported version {version}",
            path.display()
        ));
    }

    let sequence = u64::from_le_bytes(bytes[pos..pos + 8].try_into().unwrap());
    pos += 8;
    if sequence != filename_sequence {
        return Err(format!(
            "checkpoint {} sequence mismatch: filename={filename_sequence}, header={sequence}",
            path.display()
        ));
    }

    let name_len = u32::from_le_bytes(bytes[pos..pos + 4].try_into().unwrap()) as usize;
    pos += 4;
    let payload_len_u64 = u64::from_le_bytes(bytes[pos..pos + 8].try_into().unwrap());
    pos += 8;
    let payload_len = usize::try_from(payload_len_u64)
        .map_err(|_| format!("checkpoint payload too large: {payload_len_u64}"))?;
    let expected_crc = u32::from_le_bytes(bytes[pos..pos + 4].try_into().unwrap());
    pos += 4;

    let end = pos
        .checked_add(name_len)
        .and_then(|v| v.checked_add(payload_len))
        .ok_or_else(|| "checkpoint length overflow".to_string())?;
    if end != bytes.len() {
        return Err(format!(
            "checkpoint {} length mismatch: expected {end} bytes, found {}",
            path.display(),
            bytes.len()
        ));
    }

    let graph_name_bytes = &bytes[pos..pos + name_len];
    pos += name_len;
    let graph_name = std::str::from_utf8(graph_name_bytes)
        .map_err(|_| format!("checkpoint {} graph name is not UTF-8", path.display()))?;
    if graph_name != expected_graph_name {
        return Err(format!(
            "checkpoint {} belongs to graph {graph_name:?}, expected {expected_graph_name:?}",
            path.display()
        ));
    }

    let payload = &bytes[pos..];
    let actual_crc = snapshot_crc(sequence, graph_name_bytes, payload);
    if actual_crc != expected_crc {
        return Err(format!(
            "checkpoint {} CRC mismatch: got {actual_crc:#010x}, expected {expected_crc:#010x}",
            path.display()
        ));
    }

    let graph = load_graph(payload, expected_graph_name)?;
    Ok(Some(LoadedSnapshot {
        sequence,
        graph,
        path,
    }))
}

pub fn remove_all_checkpoints(wal_path: &Path) -> Result<usize, String> {
    let Some(parent) = wal_path.parent() else {
        return Ok(0);
    };
    if !parent.exists() {
        return Ok(0);
    }

    let prefix = checkpoint_prefix(wal_path);
    let mut removed = 0usize;
    for entry in fs::read_dir(parent)
        .map_err(|e| format!("scan checkpoint directory {}: {e}", parent.display()))?
    {
        let entry = entry.map_err(|e| format!("read checkpoint directory entry: {e}"))?;
        let name = entry.file_name();
        let name = name.to_string_lossy();
        let Some(rest) = name.strip_prefix(&prefix) else {
            continue;
        };
        if !rest.ends_with(".fgs") && !rest.contains(".fgs.tmp.") {
            continue;
        }
        fs::remove_file(entry.path())
            .map_err(|e| format!("remove graph checkpoint {}: {e}", entry.path().display()))?;
        removed += 1;
    }
    Ok(removed)
}

pub fn cleanup_old_checkpoints(
    wal_path: &Path,
    keep: usize,
) -> Result<(), String> {
    let Some(parent) = wal_path.parent() else {
        return Ok(());
    };
    let prefix = checkpoint_prefix(wal_path);
    let mut candidates = Vec::new();

    for entry in fs::read_dir(parent)
        .map_err(|e| format!("scan checkpoint directory {}: {e}", parent.display()))?
    {
        let entry = entry.map_err(|e| format!("read checkpoint directory entry: {e}"))?;
        let name = entry.file_name();
        let name = name.to_string_lossy();
        let Some(rest) = name.strip_prefix(&prefix) else {
            continue;
        };
        let Some(seq) = rest.strip_suffix(".fgs") else {
            continue;
        };
        if let Ok(sequence) = seq.parse::<u64>() {
            candidates.push((sequence, entry.path()));
        }
    }

    candidates.sort_unstable_by(|a, b| b.0.cmp(&a.0));
    for (_, path) in candidates.into_iter().skip(keep.max(1)) {
        fs::remove_file(&path)
            .map_err(|e| format!("remove old checkpoint {}: {e}", path.display()))?;
    }
    Ok(())
}

fn checkpoint_prefix(wal_path: &Path) -> String {
    format!(
        "{}.snapshot.",
        wal_path
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
    )
}

fn checkpoint_path(wal_path: &Path, sequence: u64) -> PathBuf {
    let parent = wal_path.parent().unwrap_or_else(|| Path::new("."));
    parent.join(format!("{}{sequence}.fgs", checkpoint_prefix(wal_path)))
}

fn snapshot_crc(sequence: u64, name: &[u8], payload: &[u8]) -> u32 {
    let mut hasher = Hasher::new();
    hasher.update(&sequence.to_le_bytes());
    hasher.update(name);
    hasher.update(payload);
    hasher.finalize()
}

struct VecWriter {
    buf: Vec<u8>,
}

impl VecWriter {
    const fn new() -> Self {
        Self { buf: Vec::new() }
    }

    fn into_vec(self) -> Vec<u8> {
        self.buf
    }
}

impl Writer for VecWriter {
    fn write_unsigned(&mut self, value: u64) {
        self.buf.push(TYPE_UNSIGNED);
        self.buf.extend_from_slice(&value.to_le_bytes());
    }

    fn write_signed(&mut self, value: i64) {
        self.buf.push(TYPE_SIGNED);
        self.buf.extend_from_slice(&value.to_le_bytes());
    }

    fn write_double(&mut self, value: f64) {
        self.buf.push(TYPE_DOUBLE);
        self.buf.extend_from_slice(&value.to_le_bytes());
    }

    fn write_buffer(&mut self, data: &[u8]) {
        self.buf.push(TYPE_BYTES);
        self.buf.extend_from_slice(&(data.len() as u64).to_le_bytes());
        self.buf.extend_from_slice(data);
    }
}

struct VecReader<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> VecReader<'a> {
    const fn new(data: &'a [u8]) -> Self {
        Self { data, pos: 0 }
    }

    fn read_tag(&mut self, expected: u8) -> Result<(), String> {
        let Some(&tag) = self.data.get(self.pos) else {
            return Err("checkpoint graph payload ended before type tag".to_string());
        };
        self.pos += 1;
        if tag != expected {
            return Err(format!(
                "checkpoint graph payload expected type tag {expected}, got {tag}"
            ));
        }
        Ok(())
    }

    fn read_array<const N: usize>(&mut self) -> Result<[u8; N], String> {
        let end = self
            .pos
            .checked_add(N)
            .ok_or_else(|| "checkpoint graph payload length overflow".to_string())?;
        let slice = self
            .data
            .get(self.pos..end)
            .ok_or_else(|| "checkpoint graph payload is truncated".to_string())?;
        self.pos = end;
        slice
            .try_into()
            .map_err(|_| format!("checkpoint graph payload expected {N} bytes"))
    }
}

impl Reader for VecReader<'_> {
    fn read_unsigned(&mut self) -> Result<u64, String> {
        self.read_tag(TYPE_UNSIGNED)?;
        Ok(u64::from_le_bytes(self.read_array()?))
    }

    fn read_signed(&mut self) -> Result<i64, String> {
        self.read_tag(TYPE_SIGNED)?;
        Ok(i64::from_le_bytes(self.read_array()?))
    }

    fn read_double(&mut self) -> Result<f64, String> {
        self.read_tag(TYPE_DOUBLE)?;
        Ok(f64::from_le_bytes(self.read_array()?))
    }

    fn read_buffer(&mut self) -> Result<Vec<u8>, String> {
        self.read_tag(TYPE_BYTES)?;
        let len = self.read_unsigned_raw()? as usize;
        let end = self
            .pos
            .checked_add(len)
            .ok_or_else(|| "checkpoint graph buffer length overflow".to_string())?;
        let value = self
            .data
            .get(self.pos..end)
            .ok_or_else(|| "checkpoint graph buffer is truncated".to_string())?
            .to_vec();
        self.pos = end;
        Ok(value)
    }
}

impl VecReader<'_> {
    fn read_unsigned_raw(&mut self) -> Result<u64, String> {
        Ok(u64::from_le_bytes(self.read_array()?))
    }
}

#[derive(Debug)]
struct Header {
    graph_name: String,
    node_count: u64,
    edge_count: u64,
    deleted_node_count: u64,
    deleted_edge_count: u64,
    label_count: u64,
    relationship_count: u64,
    multi_edge: Vec<bool>,
    key_count: u64,
}

impl Header {
    fn from_graph(graph: &Graph, graph_name: &str) -> Self {
        Self {
            graph_name: graph_name.to_string(),
            node_count: graph.node_count(),
            edge_count: graph.relationship_count(),
            deleted_node_count: graph.deleted_nodes().len(),
            deleted_edge_count: graph.deleted_relationships().len(),
            label_count: graph.label_matrices().len() as u64,
            relationship_count: graph.relationship_tensors().len() as u64,
            multi_edge: graph
                .relationship_tensors()
                .iter()
                .map(Tensor::has_multi_edge)
                .collect(),
            key_count: 1,
        }
    }
}

impl Encode<GRAPH_FORMAT_VERSION> for Header {
    fn encode(&self, writer: &mut dyn Writer) {
        writer.write_buffer(&null_terminated(&self.graph_name));
        writer.write_unsigned(self.node_count);
        writer.write_unsigned(self.edge_count);
        writer.write_unsigned(self.deleted_node_count);
        writer.write_unsigned(self.deleted_edge_count);
        writer.write_unsigned(self.label_count);
        writer.write_unsigned(self.relationship_count);
        for &multi in &self.multi_edge {
            writer.write_unsigned(u64::from(multi));
        }
        writer.write_unsigned(self.key_count);
    }
}

impl Decode<GRAPH_FORMAT_VERSION> for Header {
    fn decode(reader: &mut dyn Reader) -> Result<Self, String> {
        let graph_name = strip_null_terminator(&reader.read_buffer()?);
        let node_count = reader.read_unsigned()?;
        let edge_count = reader.read_unsigned()?;
        let deleted_node_count = reader.read_unsigned()?;
        let deleted_edge_count = reader.read_unsigned()?;
        let label_count = reader.read_unsigned()?;
        let relationship_count = reader.read_unsigned()?;
        let mut multi_edge = Vec::with_capacity(relationship_count as usize);
        for _ in 0..relationship_count {
            multi_edge.push(reader.read_unsigned()? != 0);
        }
        let key_count = reader.read_unsigned()?;

        Ok(Self {
            graph_name,
            node_count,
            edge_count,
            deleted_node_count,
            deleted_edge_count,
            label_count,
            relationship_count,
            multi_edge,
            key_count,
        })
    }
}

struct Schema {
    attribute_names: Vec<Arc<String>>,
    node_labels: Vec<Arc<String>>,
    relationship_types: Vec<Arc<String>>,
    indexes: Vec<IndexInfo>,
    constraints: Vec<Constraint>,
}

impl Schema {
    fn from_graph(graph: &Graph) -> Self {
        Self {
            attribute_names: graph.build_global_attrs(),
            node_labels: graph.get_labels().to_vec(),
            relationship_types: graph.get_types().to_vec(),
            indexes: graph.index_info(),
            constraints: graph.constraints().to_vec(),
        }
    }
}

impl Encode<GRAPH_FORMAT_VERSION> for Schema {
    fn encode(&self, writer: &mut dyn Writer) {
        writer.write_unsigned(self.attribute_names.len() as u64);
        for name in &self.attribute_names {
            writer.write_buffer(&null_terminated(name));
        }

        writer.write_unsigned(self.node_labels.len() as u64);
        for (index, label) in self.node_labels.iter().enumerate() {
            writer.write_unsigned(index as u64);
            writer.write_buffer(&null_terminated(label));
            let infos: Vec<&IndexInfo> = self
                .indexes
                .iter()
                .filter(|info| {
                    info.label.as_str() == label.as_str() && info.entity_type != "RELATIONSHIP"
                })
                .collect();
            encode_index_block(writer, &infos);
            let constraints: Vec<&Constraint> = self
                .constraints
                .iter()
                .filter(|constraint| {
                    constraint.entity_type == EntityType::Node
                        && constraint.label.as_str() == label.as_str()
                })
                .collect();
            encode_constraint_block(writer, &constraints, &self.attribute_names);
        }

        writer.write_unsigned(self.relationship_types.len() as u64);
        for (index, relation) in self.relationship_types.iter().enumerate() {
            writer.write_unsigned(index as u64);
            writer.write_buffer(&null_terminated(relation));
            let infos: Vec<&IndexInfo> = self
                .indexes
                .iter()
                .filter(|info| {
                    info.label.as_str() == relation.as_str()
                        && info.entity_type == "RELATIONSHIP"
                })
                .collect();
            encode_index_block(writer, &infos);
            let constraints: Vec<&Constraint> = self
                .constraints
                .iter()
                .filter(|constraint| {
                    constraint.entity_type == EntityType::Relationship
                        && constraint.label.as_str() == relation.as_str()
                })
                .collect();
            encode_constraint_block(writer, &constraints, &self.attribute_names);
        }
    }
}

impl Decode<GRAPH_FORMAT_VERSION> for Schema {
    fn decode(reader: &mut dyn Reader) -> Result<Self, String> {
        let attr_count = reader.read_unsigned()?;
        let mut attribute_names = Vec::with_capacity(attr_count as usize);
        for _ in 0..attr_count {
            attribute_names.push(Arc::new(strip_null_terminator(&reader.read_buffer()?)));
        }

        let node_count = reader.read_unsigned()?;
        let mut node_labels = Vec::with_capacity(node_count as usize);
        let mut indexes = Vec::new();
        let mut constraints = Vec::new();
        for _ in 0..node_count {
            let (label, info, mut schema_constraints) =
                decode_schema_entry(reader, &attribute_names)?;
            let label = Arc::new(label);
            if let Some(mut info) = info {
                info.label = Arc::clone(&label);
                info.entity_type = "NODE".to_string();
                indexes.push(info);
            }
            for constraint in &mut schema_constraints {
                constraint.entity_type = EntityType::Node;
            }
            constraints.extend(schema_constraints);
            node_labels.push(label);
        }

        let rel_count = reader.read_unsigned()?;
        let mut relationship_types = Vec::with_capacity(rel_count as usize);
        for _ in 0..rel_count {
            let (relation, info, mut schema_constraints) =
                decode_schema_entry(reader, &attribute_names)?;
            let relation = Arc::new(relation);
            if let Some(mut info) = info {
                info.label = Arc::clone(&relation);
                info.entity_type = "RELATIONSHIP".to_string();
                indexes.push(info);
            }
            for constraint in &mut schema_constraints {
                constraint.entity_type = EntityType::Relationship;
            }
            constraints.extend(schema_constraints);
            relationship_types.push(relation);
        }

        Ok(Self {
            attribute_names,
            node_labels,
            relationship_types,
            indexes,
            constraints,
        })
    }
}

fn encode_index_block(writer: &mut dyn Writer, infos: &[&IndexInfo]) {
    writer.write_unsigned(u64::from(!infos.is_empty()));
    if infos.is_empty() {
        return;
    }

    let language = infos
        .first()
        .and_then(|info| info.language.as_ref())
        .map_or("english", |value| value.as_str());
    writer.write_buffer(&null_terminated(language));

    let stopwords: Vec<&str> = infos
        .first()
        .and_then(|info| info.stopwords.as_ref())
        .map(|values| values.iter().map(|value| value.as_str()).collect())
        .unwrap_or_default();
    writer.write_unsigned(stopwords.len() as u64);
    for value in stopwords {
        writer.write_buffer(&null_terminated(value));
    }

    let fields: Vec<_> = infos
        .iter()
        .flat_map(|info| {
            info.field_order.iter().filter_map(move |attr| {
                info.fields
                    .get(attr)
                    .map(|values| values.iter().map(move |field| (attr, field)))
            })
        })
        .flatten()
        .collect();
    writer.write_unsigned(fields.len() as u64);

    for (attr, field) in fields {
        writer.write_buffer(&null_terminated(attr));
        let field_type = match field.ty {
            IndexType::Fulltext => index_field_type::INDEX_FLD_FULLTEXT,
            IndexType::Range => {
                index_field_type::INDEX_FLD_NUMERIC
                    | index_field_type::INDEX_FLD_STR
                    | index_field_type::INDEX_FLD_GEO
            }
            IndexType::Vector => index_field_type::INDEX_FLD_VECTOR,
        };
        writer.write_unsigned(field_type);

        let options = field.options();
        writer.write_double(options.and_then(|v| v.weight).unwrap_or(1.0));
        writer.write_unsigned(u64::from(
            options.and_then(|v| v.nostem).unwrap_or(false),
        ));
        let phonetic = options
            .and_then(|v| v.phonetic.clone())
            .unwrap_or_default();
        writer.write_buffer(&null_terminated(&phonetic));

        if field_type & index_field_type::INDEX_FLD_VECTOR != 0
            && let Some(vector) = field.vector_options()
        {
            writer.write_unsigned(vector.dimension);
            writer.write_unsigned(vector.m.unwrap_or(16) as u64);
            writer.write_unsigned(vector.ef_construction.unwrap_or(200) as u64);
            writer.write_unsigned(vector.ef_runtime.unwrap_or(10) as u64);
            let similarity = match vector.similarity_function.as_deref() {
                Some(value) if value.eq_ignore_ascii_case("ip") => 1,
                Some(value) if value.eq_ignore_ascii_case("cosine") => 2,
                _ => 0,
            };
            writer.write_unsigned(similarity);
        }
    }
}

fn encode_constraint_block(
    writer: &mut dyn Writer,
    constraints: &[&Constraint],
    attribute_names: &[Arc<String>],
) {
    let active: Vec<&Constraint> = constraints
        .iter()
        .copied()
        .filter(|constraint| constraint.status == ConstraintStatus::Operational)
        .collect();
    writer.write_unsigned(active.len() as u64);

    for constraint in active {
        writer.write_unsigned(match constraint.ct {
            ConstraintType::Unique => 0,
            ConstraintType::Mandatory => 1,
        });
        writer.write_unsigned(constraint.properties.len() as u64);
        for property in &constraint.properties {
            let id = attribute_names
                .iter()
                .position(|name| name.as_str() == property.as_str())
                .unwrap_or(0);
            writer.write_unsigned(id as u64);
        }
    }
}

fn decode_schema_entry(
    reader: &mut dyn Reader,
    attribute_names: &[Arc<String>],
) -> Result<(String, Option<IndexInfo>, Vec<Constraint>), String> {
    let _schema_id = reader.read_unsigned()?;
    let schema_name = strip_null_terminator(&reader.read_buffer()?);
    let has_index = reader.read_unsigned()? != 0;

    let info = if has_index {
        let language = strip_null_terminator(&reader.read_buffer()?);
        let stopword_count = reader.read_unsigned()?;
        let mut stopwords = Vec::with_capacity(stopword_count as usize);
        for _ in 0..stopword_count {
            stopwords.push(Arc::new(strip_null_terminator(&reader.read_buffer()?)));
        }

        let field_count = reader.read_unsigned()?;
        let mut fields: HashMap<Arc<String>, Vec<Arc<Field>>> = HashMap::new();
        let mut field_order = Vec::new();
        for _ in 0..field_count {
            let (attr, field) = decode_index_field(reader)?;
            if !fields.contains_key(&attr) {
                field_order.push(Arc::clone(&attr));
            }
            fields.entry(attr).or_default().push(Arc::new(field));
        }

        Some(IndexInfo {
            label: Arc::new(String::new()),
            pending: 0,
            progress: 0,
            total: 0,
            fields,
            field_order,
            language: Some(Arc::new(language)),
            stopwords: (!stopwords.is_empty()).then_some(stopwords),
            entity_type: String::new(),
        })
    } else {
        None
    };

    let constraint_count = reader.read_unsigned()?;
    let mut constraints = Vec::with_capacity(constraint_count as usize);
    for _ in 0..constraint_count {
        let ct = if reader.read_unsigned()? == 0 {
            ConstraintType::Unique
        } else {
            ConstraintType::Mandatory
        };
        let field_count = reader.read_unsigned()?;
        let mut properties = Vec::with_capacity(field_count as usize);
        for _ in 0..field_count {
            let id = reader.read_unsigned()? as usize;
            let property = attribute_names
                .get(id)
                .cloned()
                .unwrap_or_else(|| Arc::new(format!("attr_{id}")));
            properties.push(property);
        }
        let mut constraint = Constraint::new(
            ct,
            EntityType::Node,
            Arc::new(schema_name.clone()),
            properties,
        );
        constraint.status = ConstraintStatus::Operational;
        constraints.push(constraint);
    }

    Ok((schema_name, info, constraints))
}

fn decode_index_field(reader: &mut dyn Reader) -> Result<(Arc<String>, Field), String> {
    let name = strip_null_terminator(&reader.read_buffer()?);
    let field_type = reader.read_unsigned()?;
    let weight = reader.read_double()?;
    let nostem = reader.read_unsigned()? != 0;
    let phonetic = strip_null_terminator(&reader.read_buffer()?);

    let is_vector = field_type & index_field_type::INDEX_FLD_VECTOR != 0;
    let is_fulltext = field_type & index_field_type::INDEX_FLD_FULLTEXT != 0;
    let ty = if is_fulltext {
        IndexType::Fulltext
    } else if is_vector {
        IndexType::Vector
    } else {
        IndexType::Range
    };

    let attr_name = match ty {
        IndexType::Range => name.strip_prefix("range:").unwrap_or(&name).to_string(),
        IndexType::Vector => name.strip_prefix("vector:").unwrap_or(&name).to_string(),
        IndexType::Fulltext => name.clone(),
    };

    let vector_options = if is_vector {
        let dimension = reader.read_unsigned()?;
        let m = reader.read_unsigned()? as usize;
        let ef_construction = reader.read_unsigned()? as usize;
        let ef_runtime = reader.read_unsigned()? as usize;
        let similarity_function = match reader.read_unsigned()? {
            1 => Some("ip".to_string()),
            2 => Some("cosine".to_string()),
            _ => Some("euclidean".to_string()),
        };
        Some(VectorIndexOptions {
            dimension,
            similarity_function,
            m: Some(m),
            ef_construction: Some(ef_construction),
            ef_runtime: Some(ef_runtime),
        })
    } else {
        None
    };

    let text_options = is_fulltext.then_some(TextIndexOptions {
        weight: Some(weight),
        nostem: Some(nostem),
        phonetic: Some(phonetic),
        language: None,
        stopwords: None,
    });

    let field = if let Some(vector) = vector_options {
        Field::new_with_vector_options(
            CString::new(name).map_err(|e| e.to_string())?,
            ty,
            vector,
        )
    } else {
        Field::new(
            CString::new(name).map_err(|e| e.to_string())?,
            ty,
            text_options,
        )
    };

    Ok((Arc::new(attr_name), field))
}

/// Encode a graph using FalkorDB's v19 single-key GRAPH.RESTORE wire format.
///
/// This is intentionally the same type-tagged payload produced by upstream
/// `serializers::encoder::vec_save_graph`: header, schema, payload directory,
/// then graph payloads. It is not the native FGSN checkpoint envelope.
pub fn save_falkordb_v19_payload(graph: &Graph, graph_name: &str) -> Vec<u8> {
    save_graph(graph, graph_name)
}

/// Decode FalkorDB's v19 single-key GRAPH.RESTORE wire format.
///
/// The destination name is supplied by the caller, matching upstream
/// GRAPH.RESTORE semantics: the serialized source graph can be installed
/// under a different destination key without rewriting its payload first.
pub fn load_falkordb_v19_payload(
    data: &[u8],
    destination_name: &str,
) -> Result<Graph, String> {
    load_graph(data, destination_name)
}

fn save_graph(graph: &Graph, graph_name: &str) -> Vec<u8> {
    let payloads = build_payloads(graph);
    let mut writer = VecWriter::new();

    Header::from_graph(graph, graph_name).encode(&mut writer);
    Schema::from_graph(graph).encode(&mut writer);

    writer.write_unsigned(payloads.len() as u64);
    for payload in &payloads {
        writer.write_unsigned(payload.state as u64);
        writer.write_unsigned(payload.count);
    }
    for payload in &payloads {
        graph.encode_payload(&mut writer, payload);
    }

    writer.into_vec()
}

fn load_graph(data: &[u8], destination_name: &str) -> Result<Graph, String> {
    let mut reader = VecReader::new(data);
    let header = Header::decode(&mut reader)?;
    if header.key_count != 1 {
        return Err(format!(
            "checkpoint graph expected one key, found {}",
            header.key_count
        ));
    }
    let schema = Schema::decode(&mut reader)?;

    let payload_count = reader.read_unsigned()?;
    let mut payloads = Vec::with_capacity(payload_count as usize);
    for _ in 0..payload_count {
        let state_raw = reader.read_unsigned()?;
        let state = EncodeState::from_u64(state_raw)
            .ok_or_else(|| format!("unknown graph encode state {state_raw}"))?;
        let count = reader.read_unsigned()?;
        payloads.push((state, count));
    }

    let mut node_attrs = AttributeStore::new();
    let mut relationship_attrs = AttributeStore::new();
    let mut attrs_name = AttrNameMap::default();
    for name in &schema.attribute_names {
        attrs_name.insert(Arc::clone(name));
    }

    let mut deleted_nodes = RoaringTreemap::new();
    let mut deleted_relationships = RoaringTreemap::new();
    let mut label_matrices = Vec::new();
    let mut relationship_tensors = Vec::new();
    let mut adjacency = VersionedMatrix::<bool>::new(0, 0);
    let mut labels_matrix = VersionedMatrix::<bool>::new(0, 0);

    for (state, count) in payloads {
        match state {
            EncodeState::Nodes => {
                node_attrs.decode_with_count(&mut reader, count, attrs_name.len())?;
            }
            EncodeState::DeletedNodes => {
                deleted_nodes.decode_with_count(&mut reader, count, attrs_name.len())?;
            }
            EncodeState::Edges => {
                relationship_attrs.decode_with_count(&mut reader, count, attrs_name.len())?;
            }
            EncodeState::DeletedEdges => {
                deleted_relationships.decode_with_count(&mut reader, count, attrs_name.len())?;
            }
            EncodeState::LabelsMatrices => {
                let actual = reader.read_unsigned()?;
                for _ in 0..actual {
                    let _label_id = reader.read_unsigned()?;
                    label_matrices.push(VersionedMatrix::decode(&mut reader)?);
                }
            }
            EncodeState::RelationMatrices => {
                for _ in 0..header.relationship_count {
                    let _relation_id = reader.read_unsigned()?;
                    relationship_tensors.push(Tensor::decode(&mut reader)?);
                }
            }
            EncodeState::AdjMatrix => {
                adjacency = VersionedMatrix::decode(&mut reader)?;
            }
            EncodeState::LblsMatrix => {
                labels_matrix = VersionedMatrix::decode(&mut reader)?;
            }
            _ => {}
        }
    }

    let mut graph = Graph::restore(
        destination_name,
        25,
        header.node_count,
        header.edge_count,
        deleted_nodes,
        deleted_relationships,
        adjacency,
        labels_matrix,
        VersionedMatrix::<bool>::new(0, 0),
        label_matrices,
        relationship_tensors,
        schema.node_labels,
        schema.relationship_types,
        attrs_name,
        node_attrs,
        relationship_attrs,
    );

    graph.rebuild_derived_matrices();
    rebuild_indexes(&mut graph, &schema.indexes);
    for constraint in schema.constraints {
        graph.add_constraint_raw(constraint);
    }
    graph.populate_indexes_sync();

    Ok(graph)
}

fn rebuild_indexes(graph: &mut Graph, indexes: &[IndexInfo]) {
    for info in indexes {
        let entity_type = if info.entity_type == "RELATIONSHIP" {
            EntityType::Relationship
        } else {
            EntityType::Node
        };

        let mut text_meta_pending = info.language.is_some() || info.stopwords.is_some();
        for attr_name in &info.field_order {
            let Some(fields) = info.fields.get(attr_name) else {
                continue;
            };

            for field in fields {
                let attr = Arc::new(attr_name.to_string());
                let options = field.vector_options().map_or_else(
                    || {
                        let mut text = field.options().cloned();
                        if text_meta_pending && field.ty == IndexType::Fulltext {
                            let text = text.get_or_insert_with(Default::default);
                            text.language = info.language.clone();
                            text.stopwords = info.stopwords.clone();
                            text_meta_pending = false;
                        }
                        text.map(IndexOptions::Text)
                    },
                    |vector| Some(IndexOptions::Vector(vector.clone())),
                );

                if let Err(err) = graph.create_index_sync(
                    &field.ty,
                    &entity_type,
                    &info.label,
                    &vec![attr],
                    options,
                ) {
                    eprintln!(
                        "FalkorDB native checkpoint: failed to rebuild index on {}: {err}",
                        info.label
                    );
                }
            }
        }
    }
}

fn build_payloads(graph: &Graph) -> Vec<PayloadEntry> {
    let mut payloads = Vec::new();

    let node_count = graph.node_count();
    if node_count > 0 {
        payloads.push(PayloadEntry {
            state: EncodeState::Nodes,
            count: node_count,
            offset: 0,
        });
    }

    let deleted_node_count = graph.deleted_nodes_count();
    if deleted_node_count > 0 {
        payloads.push(PayloadEntry {
            state: EncodeState::DeletedNodes,
            count: deleted_node_count,
            offset: 0,
        });
    }

    let edge_count = graph.relationship_count();
    if edge_count > 0 {
        payloads.push(PayloadEntry {
            state: EncodeState::Edges,
            count: edge_count,
            offset: 0,
        });
    }

    let deleted_edge_count = graph.deleted_relationships_count();
    if deleted_edge_count > 0 {
        payloads.push(PayloadEntry {
            state: EncodeState::DeletedEdges,
            count: deleted_edge_count,
            offset: 0,
        });
    }

    let label_count = graph.label_matrices().len();
    if label_count > 0 {
        payloads.push(PayloadEntry {
            state: EncodeState::LabelsMatrices,
            count: label_count as u64,
            offset: 0,
        });
    }

    let relation_count = graph.relationship_tensors().len();
    if relation_count > 0 {
        payloads.push(PayloadEntry {
            state: EncodeState::RelationMatrices,
            count: relation_count as u64,
            offset: 0,
        });
    }

    payloads.push(PayloadEntry {
        state: EncodeState::AdjMatrix,
        count: 1,
        offset: 0,
    });
    payloads.push(PayloadEntry {
        state: EncodeState::LblsMatrix,
        count: 1,
        offset: 0,
    });

    payloads
}

fn null_terminated(value: &str) -> Vec<u8> {
    value
        .as_bytes()
        .iter()
        .copied()
        .chain(std::iter::once(0))
        .collect()
}

fn strip_null_terminator(value: &[u8]) -> String {
    if value.last() == Some(&0) {
        String::from_utf8_lossy(&value[..value.len() - 1]).into_owned()
    } else {
        String::from_utf8_lossy(value).into_owned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn checkpoint_filename_sequence_roundtrip_shape() {
        let wal = PathBuf::from("graphs/abc.wal");
        assert_eq!(
            checkpoint_path(&wal, 42),
            PathBuf::from("graphs/abc.wal.snapshot.42.fgs")
        );
    }
}
