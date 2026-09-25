use std::sync::Arc;

use graph::{
    graph::{
        graph::{Graph, NodeId, RelationshipId},
        id_space::IdSpace,
    },
    identifier_limits::validate_identifier_len,
    runtime::value::Value,
};
use roaring::RoaringTreemap;
use rustc_hash::FxHashMap;

const BI_NULL: u8 = 0;
const BI_BOOL: u8 = 1;
const BI_DOUBLE: u8 = 2;
const BI_STRING: u8 = 3;
const BI_LONG: u8 = 4;
const BI_ARRAY: u8 = 5;
const EDGE_ENDPOINTS_LEN: usize = 16;

pub fn parse_count(value: &str) -> Option<usize> {
    if value.is_empty()
        || !value.bytes().all(|b| b.is_ascii_digit())
        || (value.len() > 1 && value.starts_with('0'))
    {
        return None;
    }
    usize::try_from(value.parse::<i64>().ok()?).ok()
}

pub fn validate_declared_counts(
    tokens: &[Vec<u8>],
    node_count: usize,
    edge_count: usize,
    node_token_count: usize,
    rel_token_count: usize,
) -> Result<(), String> {
    if tokens.len() != node_token_count.saturating_add(rel_token_count) {
        return Err("Bulk insert format error, token count mismatch.".to_string());
    }

    let (node_tokens, rel_tokens) = tokens.split_at(node_token_count);
    let mut node_ceiling = 0usize;
    for token in node_tokens {
        node_ceiling = node_ceiling
            .checked_add(max_records(token, "Label name", 0)?)
            .ok_or_else(|| "node record count overflow".to_string())?;
    }
    if node_count > node_ceiling {
        return Err(format!(
            "Bulk insert format error, declared node count {node_count} exceeds the {node_ceiling} node records the payload describes."
        ));
    }

    let mut edge_ceiling = 0usize;
    for token in rel_tokens {
        edge_ceiling = edge_ceiling
            .checked_add(max_records(token, "Relationship type", EDGE_ENDPOINTS_LEN)?)
            .ok_or_else(|| "edge record count overflow".to_string())?;
    }
    if edge_count > edge_ceiling {
        return Err(format!(
            "Bulk insert format error, declared relation count {edge_count} exceeds the {edge_ceiling} edge records the payload describes."
        ));
    }
    Ok(())
}

pub fn apply(
    graph: &mut Graph,
    tokens: &[Vec<u8>],
    node_count: usize,
    edge_count: usize,
    node_token_count: usize,
    rel_token_count: usize,
) -> Result<(), String> {
    validate_declared_counts(
        tokens,
        node_count,
        edge_count,
        node_token_count,
        rel_token_count,
    )?;

    let mut node_space = graph.open_node_id_space();
    let mut rel_space = graph.open_relationship_id_space();

    let node_ids: Vec<NodeId> = node_space
        .reserve(node_count, graph.deleted_nodes(), &[])?
        .into_iter()
        .map(NodeId::from)
        .collect();
    let rel_ids: Vec<RelationshipId> = rel_space
        .reserve(edge_count, graph.deleted_relationships(), &[])?
        .into_iter()
        .map(RelationshipId::from)
        .collect();

    let mut node_cursor = 0usize;
    let mut rel_cursor = 0usize;
    let mut docs = BulkIndexDocs::default();

    for token in tokens.iter().take(node_token_count) {
        process_node_token(
            graph,
            token,
            &node_ids,
            &mut node_cursor,
            &mut node_space,
            &mut docs,
        )?;
    }

    for token in tokens.iter().skip(node_token_count).take(rel_token_count) {
        process_edge_token(
            graph,
            token,
            &rel_ids,
            &mut rel_cursor,
            &mut rel_space,
            &mut docs,
        )?;
    }

    if node_cursor != node_count {
        return Err(format!(
            "bulk payload created {node_cursor} nodes but declared {node_count}"
        ));
    }
    if rel_cursor != edge_count {
        return Err(format!(
            "bulk payload created {rel_cursor} relationships but declared {edge_count}"
        ));
    }

    docs.publish(graph);
    graph.flush_for_bulk();
    Ok(())
}

#[derive(Default)]
struct BulkIndexDocs {
    nodes: FxHashMap<u64, RoaringTreemap>,
    edges: FxHashMap<u64, RoaringTreemap>,
}

impl BulkIndexDocs {
    fn publish(&mut self, graph: &mut Graph) {
        if !self.nodes.is_empty() {
            graph.commit_index(&mut self.nodes, &mut FxHashMap::default());
        }
        if !self.edges.is_empty() {
            graph.commit_edge_index(&mut self.edges, &mut FxHashMap::default());
        }
    }
}

fn process_node_token(
    graph: &mut Graph,
    data: &[u8],
    node_ids: &[NodeId],
    cursor: &mut usize,
    space: &mut IdSpace,
    docs: &mut BulkIndexDocs,
) -> Result<(), String> {
    let mut idx = 0usize;
    let (labels, prop_names) = parse_header(data, &mut idx, "Label name")?;
    let label_ids: Vec<_> = labels.iter().map(|label| graph.get_label_id_mut(label)).collect();
    let attr_ids: Vec<u16> = prop_names
        .iter()
        .map(|name| graph.get_or_create_node_attr_id(name))
        .collect();

    let mut nodes = RoaringTreemap::new();
    let mut label_rows = Vec::new();
    let mut label_cols = Vec::new();
    let mut attrs: Vec<(u64, Vec<(u16, Value)>)> = Vec::new();

    while idx < data.len() {
        let node_id = *node_ids
            .get(*cursor)
            .ok_or_else(|| "bulk data contains more node records than advertised count".to_string())?;
        *cursor += 1;
        let raw_id = u64::from(node_id);
        nodes.insert(raw_id);

        for &label_id in &label_ids {
            label_rows.push(raw_id);
            label_cols.push(label_id.0 as u64);
        }

        if !attr_ids.is_empty() {
            let mut entries = Vec::with_capacity(attr_ids.len());
            for &attr_id in &attr_ids {
                let value = read_property(data, &mut idx)?;
                if !matches!(value, Value::Null) {
                    entries.push((attr_id, value));
                }
            }
            if !entries.is_empty() {
                attrs.push((raw_id, entries));
            }
        }
    }

    if nodes.is_empty() {
        return Ok(());
    }

    graph.create_nodes(&nodes, space).map_err(|e| e.to_string())?;
    graph.set_nodes_labels_bulk(&label_rows, &label_cols, &mut docs.nodes, true);
    if !attrs.is_empty() {
        graph.import_node_attrs_resolved(&mut attrs, &label_ids, &mut docs.nodes);
    }
    Ok(())
}

fn process_edge_token(
    graph: &mut Graph,
    data: &[u8],
    rel_ids: &[RelationshipId],
    cursor: &mut usize,
    space: &mut IdSpace,
    docs: &mut BulkIndexDocs,
) -> Result<(), String> {
    let mut idx = 0usize;
    let (types, prop_names) = parse_header(data, &mut idx, "Relationship type")?;
    if types.len() != 1 {
        return Err(format!("edges must have exactly one type, got {}", types.len()));
    }

    let type_name = Arc::new(types[0].clone());
    let type_id = graph.get_type_id_mut(&type_name);
    let attr_ids: Vec<u16> = prop_names
        .iter()
        .map(|name| graph.get_or_create_rel_attr_id(name))
        .collect();

    let mut srcs = Vec::new();
    let mut dsts = Vec::new();
    let mut edge_ids = Vec::new();
    let mut attrs: Vec<(u64, Vec<(u16, Value)>)> = Vec::new();

    while idx < data.len() {
        let src = read_u64_ne(data, &mut idx)?;
        let dst = read_u64_ne(data, &mut idx)?;
        let rel_id = *rel_ids
            .get(*cursor)
            .ok_or_else(|| "bulk data contains more edge records than advertised count".to_string())?;
        *cursor += 1;

        srcs.push(src);
        dsts.push(dst);
        edge_ids.push(u64::from(rel_id));

        if !attr_ids.is_empty() {
            let mut entries = Vec::with_capacity(attr_ids.len());
            for &attr_id in &attr_ids {
                let value = read_property(data, &mut idx)?;
                if !matches!(value, Value::Null) {
                    entries.push((attr_id, value));
                }
            }
            if !entries.is_empty() {
                attrs.push((u64::from(rel_id), entries));
            }
        }
    }

    if srcs.is_empty() {
        return Ok(());
    }

    graph
        .create_relationships_bulk(&type_name, &srcs, &dsts, &edge_ids, space)
        .map_err(|e| e.to_string())?;
    if !attrs.is_empty() {
        graph.import_relationship_attrs_resolved(&mut attrs, type_id, &mut docs.edges);
    }
    Ok(())
}

fn max_records(data: &[u8], entity: &str, endpoints_len: usize) -> Result<usize, String> {
    let mut idx = 0usize;
    let (_, props) = parse_header(data, &mut idx, entity)?;
    let per_record = endpoints_len.saturating_add(props.len());
    if per_record == 0 {
        return Ok(0);
    }
    Ok(data.len().saturating_sub(idx) / per_record)
}

fn parse_header(
    data: &[u8],
    idx: &mut usize,
    entity: &str,
) -> Result<(Vec<String>, Vec<Arc<String>>), String> {
    let labels = read_cstring(data, idx)?
        .split(':')
        .map(ToOwned::to_owned)
        .collect::<Vec<_>>();
    for label in &labels {
        validate_identifier_len(label, entity)?;
    }

    let prop_count = read_u32_ne(data, idx)? as usize;
    let cap = prop_count.min(data.len().saturating_sub(*idx));
    let mut props = Vec::with_capacity(cap);
    for _ in 0..prop_count {
        let name = read_cstring(data, idx)?;
        validate_identifier_len(name, "Property name")?;
        props.push(Arc::new(name.to_string()));
    }
    Ok((labels, props))
}

fn read_property(data: &[u8], idx: &mut usize) -> Result<Value, String> {
    let mut stack: Vec<(thin_vec::ThinVec<Value>, usize)> = Vec::new();

    loop {
        let mut value = {
            let type_byte = *data
                .get(*idx)
                .ok_or_else(|| "unexpected end of bulk data reading property type".to_string())?;
            *idx += 1;
            match type_byte {
                BI_NULL => Value::Null,
                BI_BOOL => {
                    let value = *data
                        .get(*idx)
                        .ok_or_else(|| "unexpected end of bulk data reading bool".to_string())?
                        != 0;
                    *idx += 1;
                    Value::Bool(value)
                }
                BI_DOUBLE => Value::Float(read_f64_ne(data, idx)?),
                BI_STRING => Value::String(Arc::new(read_cstring(data, idx)?.to_string())),
                BI_LONG => Value::Int(read_i64_ne(data, idx)?),
                BI_ARRAY => {
                    let len = read_i64_ne(data, idx)?;
                    if len < 0 {
                        return Err(format!("negative array length in bulk data: {len}"));
                    }
                    let len = len as usize;
                    let array = thin_vec::ThinVec::with_capacity(
                        len.min(data.len().saturating_sub(*idx)),
                    );
                    if len == 0 {
                        Value::List(Arc::new(array))
                    } else {
                        stack.push((array, len));
                        continue;
                    }
                }
                other => return Err(format!("unknown bulk property type: {other}")),
            }
        };

        loop {
            let Some((array, remaining)) = stack.last_mut() else {
                return Ok(value);
            };
            array.push(value);
            *remaining -= 1;
            if *remaining != 0 {
                break;
            }
            let (array, _) = stack.pop().unwrap();
            value = Value::List(Arc::new(array));
        }
    }
}

fn read_cstring<'a>(data: &'a [u8], idx: &mut usize) -> Result<&'a str, String> {
    let start = *idx;
    let end = data
        .get(start..)
        .ok_or_else(|| "bulk string offset outside payload".to_string())?
        .iter()
        .position(|&b| b == 0)
        .ok_or_else(|| "unterminated string in bulk data".to_string())?
        + start;
    let value = std::str::from_utf8(&data[start..end])
        .map_err(|e| format!("invalid UTF-8 in bulk data: {e}"))?;
    *idx = end + 1;
    Ok(value)
}

fn read_u32_ne(data: &[u8], idx: &mut usize) -> Result<u32, String> {
    Ok(u32::from_ne_bytes(read_fixed(data, idx)?))
}

fn read_u64_ne(data: &[u8], idx: &mut usize) -> Result<u64, String> {
    Ok(u64::from_ne_bytes(read_fixed(data, idx)?))
}

fn read_i64_ne(data: &[u8], idx: &mut usize) -> Result<i64, String> {
    Ok(i64::from_ne_bytes(read_fixed(data, idx)?))
}

fn read_f64_ne(data: &[u8], idx: &mut usize) -> Result<f64, String> {
    Ok(f64::from_ne_bytes(read_fixed(data, idx)?))
}

fn read_fixed<const N: usize>(data: &[u8], idx: &mut usize) -> Result<[u8; N], String> {
    let end = idx
        .checked_add(N)
        .ok_or_else(|| "bulk payload offset overflow".to_string())?;
    let bytes = data
        .get(*idx..end)
        .ok_or_else(|| format!("unexpected end of bulk data reading {N} bytes"))?;
    *idx = end;
    bytes
        .try_into()
        .map_err(|_| format!("bulk payload expected {N} bytes"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_count_matches_canonical_decimal_contract() {
        assert_eq!(parse_count("0"), Some(0));
        assert_eq!(parse_count("10"), Some(10));
        assert_eq!(parse_count("010"), None);
        assert_eq!(parse_count("-1"), None);
        assert_eq!(parse_count(""), None);
    }
}
