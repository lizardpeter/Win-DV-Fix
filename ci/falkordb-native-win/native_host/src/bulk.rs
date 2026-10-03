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

#[derive(Debug, Clone)]
pub struct BulkRequest {
    pub begin: bool,
    pub node_count: usize,
    pub edge_count: usize,
    pub node_token_count: usize,
    pub rel_token_count: usize,
    pub tokens: Vec<Vec<u8>>,
}

pub fn parse_request(args: &[Vec<u8>]) -> Result<BulkRequest, String> {
    if args.len() < 4 {
        return Err("ERR wrong number of arguments for 'graph.bulk' command".to_string());
    }

    let mut idx = 0usize;
    let begin = args
        .get(idx)
        .is_some_and(|v| v.eq_ignore_ascii_case(b"BEGIN"));
    if begin {
        idx += 1;
    }

    let node_count = parse_count_arg(args.get(idx), "node")?;
    idx += 1;
    let edge_count = parse_count_arg(args.get(idx), "relation")?;
    idx += 1;
    let node_token_count = parse_count_arg(args.get(idx), "node token")?;
    idx += 1;
    let rel_token_count = parse_count_arg(args.get(idx), "relation token")?;
    idx += 1;

    let tokens = args[idx..].to_vec();
    if tokens.len() != node_token_count + rel_token_count {
        return Err("Bulk insert format error, token count mismatch.".to_string());
    }

    let (node_tokens, rel_tokens) = tokens.split_at(node_token_count);

    let mut node_ceiling = 0usize;
    for token in node_tokens {
        node_ceiling = node_ceiling
            .checked_add(max_records(token, "Label name", 0)?)
            .ok_or_else(|| "Bulk insert node record count overflow".to_string())?;
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
            .ok_or_else(|| "Bulk insert relationship record count overflow".to_string())?;
    }
    if edge_count > edge_ceiling {
        return Err(format!(
            "Bulk insert format error, declared relation count {edge_count} exceeds the {edge_ceiling} edge records the payload describes."
        ));
    }

    Ok(BulkRequest {
        begin,
        node_count,
        edge_count,
        node_token_count,
        rel_token_count,
        tokens,
    })
}

fn parse_count_arg(arg: Option<&Vec<u8>>, kind: &str) -> Result<usize, String> {
    let arg = arg.ok_or_else(|| format!("Error parsing {kind} count."))?;
    let s = std::str::from_utf8(arg).map_err(|_| format!("Error parsing {kind} count."))?;
    parse_count(s).ok_or_else(|| format!("Error parsing {kind} count."))
}

fn parse_count(s: &str) -> Option<usize> {
    if s.is_empty() || !s.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    if s.len() > 1 && s.starts_with('0') {
        return None;
    }
    usize::try_from(s.parse::<i64>().ok()?).ok()
}

fn read_cstring<'a>(data: &'a [u8], idx: &mut usize) -> Result<&'a str, String> {
    if *idx >= data.len() {
        return Err("unexpected end of bulk data reading string".to_string());
    }
    let start = *idx;
    let end = data[start..]
        .iter()
        .position(|&b| b == 0)
        .ok_or_else(|| "unterminated string in bulk data".to_string())?
        + start;
    let s = std::str::from_utf8(&data[start..end])
        .map_err(|e| format!("invalid UTF-8 in bulk data: {e}"))?;
    *idx = end + 1;
    Ok(s)
}

fn read_u32_ne(data: &[u8], idx: &mut usize) -> Result<u32, String> {
    let bytes = data
        .get(*idx..*idx + 4)
        .ok_or_else(|| "unexpected end of bulk data reading u32".to_string())?;
    *idx += 4;
    Ok(u32::from_ne_bytes(bytes.try_into().expect("four bytes")))
}

fn read_u64_ne(data: &[u8], idx: &mut usize) -> Result<u64, String> {
    let bytes = data
        .get(*idx..*idx + 8)
        .ok_or_else(|| "unexpected end of bulk data reading u64".to_string())?;
    *idx += 8;
    Ok(u64::from_ne_bytes(bytes.try_into().expect("eight bytes")))
}

fn read_i64_ne(data: &[u8], idx: &mut usize) -> Result<i64, String> {
    let bytes = data
        .get(*idx..*idx + 8)
        .ok_or_else(|| "unexpected end of bulk data reading i64".to_string())?;
    *idx += 8;
    Ok(i64::from_ne_bytes(bytes.try_into().expect("eight bytes")))
}

fn read_f64_ne(data: &[u8], idx: &mut usize) -> Result<f64, String> {
    let bytes = data
        .get(*idx..*idx + 8)
        .ok_or_else(|| "unexpected end of bulk data reading f64".to_string())?;
    *idx += 8;
    Ok(f64::from_ne_bytes(bytes.try_into().expect("eight bytes")))
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
                        .ok_or_else(|| "unexpected end of bulk data reading bool".to_string())?;
                    *idx += 1;
                    Value::Bool(value != 0)
                }
                BI_DOUBLE => Value::Float(read_f64_ne(data, idx)?),
                BI_LONG => Value::Int(read_i64_ne(data, idx)?),
                BI_STRING => Value::String(Arc::new(read_cstring(data, idx)?.to_string())),
                BI_ARRAY => {
                    let len = read_i64_ne(data, idx)?;
                    if len < 0 {
                        return Err(format!("negative array length in bulk data: {len}"));
                    }
                    let len = len as usize;
                    let cap = len.min(data.len().saturating_sub(*idx));
                    let arr = thin_vec::ThinVec::with_capacity(cap);
                    if len == 0 {
                        Value::List(Arc::new(arr))
                    } else {
                        stack.push((arr, len));
                        continue;
                    }
                }
                other => return Err(format!("unknown bulk property type: {other}")),
            }
        };

        loop {
            match stack.last_mut() {
                None => return Ok(value),
                Some((arr, remaining)) => {
                    arr.push(value);
                    *remaining -= 1;
                    if *remaining == 0 {
                        let (arr, _) = stack.pop().expect("array frame exists");
                        value = Value::List(Arc::new(arr));
                    } else {
                        break;
                    }
                }
            }
        }
    }
}

fn parse_header(
    data: &[u8],
    idx: &mut usize,
    entity: &str,
) -> Result<(Vec<String>, Vec<Arc<String>>), String> {
    let labels_str = read_cstring(data, idx)?;
    let labels: Vec<String> = labels_str.split(':').map(ToOwned::to_owned).collect();
    for label in &labels {
        validate_identifier_len(label, entity)?;
    }

    let prop_count = read_u32_ne(data, idx)? as usize;
    let cap = prop_count.min(data.len().saturating_sub(*idx));
    let mut prop_names = Vec::with_capacity(cap);
    for _ in 0..prop_count {
        let name = read_cstring(data, idx)?;
        validate_identifier_len(name, "Property name")?;
        prop_names.push(Arc::new(name.to_string()));
    }
    Ok((labels, prop_names))
}

fn max_records(
    token: &[u8],
    entity: &str,
    endpoints_len: usize,
) -> Result<usize, String> {
    let mut idx = 0;
    let (_, props) = parse_header(token, &mut idx, entity)?;
    let per_record = endpoints_len + props.len();
    if per_record == 0 {
        return Ok(0);
    }
    Ok(token.len().saturating_sub(idx) / per_record)
}

#[derive(Default)]
pub(crate) struct BulkIndexDocs {
    nodes: FxHashMap<u64, RoaringTreemap>,
    edges: FxHashMap<u64, RoaringTreemap>,
}

impl BulkIndexDocs {
    pub(crate) fn publish(&mut self, graph: &mut Graph) {
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
    let mut idx = 0;
    let (labels, prop_names) = parse_header(data, &mut idx, "Label name")?;
    let label_ids: Vec<_> = labels.iter().map(|label| graph.get_label_id_mut(label)).collect();
    let attr_ids: Vec<u16> = prop_names
        .iter()
        .map(|name| graph.get_or_create_node_attr_id(name))
        .collect();

    let mut nodes = RoaringTreemap::new();
    let mut label_rows = Vec::new();
    let mut label_cols = Vec::new();
    let mut attrs = Vec::new();

    while idx < data.len() {
        if *cursor >= node_ids.len() {
            return Err("bulk data contains more node records than advertised count".to_string());
        }
        let node_id = node_ids[*cursor];
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
    let mut idx = 0;
    let (type_names, prop_names) = parse_header(data, &mut idx, "Relationship type")?;
    if type_names.len() != 1 {
        return Err(format!(
            "edges must have exactly one type, got {}",
            type_names.len()
        ));
    }

    let type_name = Arc::new(type_names[0].clone());
    let type_id = graph.get_type_id_mut(&type_name);
    let attr_ids: Vec<u16> = prop_names
        .iter()
        .map(|name| graph.get_or_create_rel_attr_id(name))
        .collect();

    let mut srcs = Vec::new();
    let mut dsts = Vec::new();
    let mut edge_ids = Vec::new();
    let mut attrs = Vec::new();

    while idx < data.len() {
        if *cursor >= rel_ids.len() {
            return Err("bulk data contains more edge records than advertised count".to_string());
        }
        let src = read_u64_ne(data, &mut idx)?;
        let dst = read_u64_ne(data, &mut idx)?;
        let rel_id = rel_ids[*cursor];
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

pub(crate) fn apply(graph: &mut Graph, request: &BulkRequest) -> Result<BulkIndexDocs, String> {
    let mut node_space = graph.open_node_id_space();
    let mut rel_space = graph.open_relationship_id_space();

    let node_ids: Vec<NodeId> = node_space
        .reserve(request.node_count, graph.deleted_nodes(), &[])?
        .into_iter()
        .map(NodeId::from)
        .collect();
    let rel_ids: Vec<RelationshipId> = rel_space
        .reserve(request.edge_count, graph.deleted_relationships(), &[])?
        .into_iter()
        .map(RelationshipId::from)
        .collect();

    let mut node_cursor = 0usize;
    let mut rel_cursor = 0usize;
    let mut docs = BulkIndexDocs::default();

    for token in request.tokens.iter().take(request.node_token_count) {
        process_node_token(
            graph,
            token,
            &node_ids,
            &mut node_cursor,
            &mut node_space,
            &mut docs,
        )?;
    }
    for token in request
        .tokens
        .iter()
        .skip(request.node_token_count)
        .take(request.rel_token_count)
    {
        process_edge_token(
            graph,
            token,
            &rel_ids,
            &mut rel_cursor,
            &mut rel_space,
            &mut docs,
        )?;
    }

    if node_cursor != request.node_count {
        return Err(format!(
            "bulk data described {node_cursor} nodes, expected {}",
            request.node_count
        ));
    }
    if rel_cursor != request.edge_count {
        return Err(format!(
            "bulk data described {rel_cursor} relationships, expected {}",
            request.edge_count
        ));
    }

    graph.flush_for_bulk();
    Ok(docs)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strict_count_parser_matches_redis_string2ll_subset() {
        assert_eq!(parse_count("0"), Some(0));
        assert_eq!(parse_count("10"), Some(10));
        for bad in ["", "00", "010", "+10", "-1", " 1", "1 ", "1.0", "1e2"] {
            assert_eq!(parse_count(bad), None, "{bad:?} should be rejected");
        }
    }
}
