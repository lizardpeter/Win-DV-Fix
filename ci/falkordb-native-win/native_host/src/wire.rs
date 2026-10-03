use graph::runtime::{runtime::Runtime, value::Value};

#[derive(Debug, Clone, PartialEq)]
pub enum WireValue {
    Null,
    Bool(bool),
    Int(i64),
    Float(f64),
    String(String),
    List(Vec<WireValue>),
    Map(Vec<(String, WireValue)>),
    Node {
        id: u64,
        labels: Vec<u64>,
        properties: Vec<(u64, WireValue)>,
    },
    Relationship {
        id: u64,
        type_id: u64,
        src: u64,
        dst: u64,
        properties: Vec<(u64, WireValue)>,
    },
    Path {
        nodes: Vec<WireValue>,
        relationships: Vec<WireValue>,
    },
    Point { latitude: f64, longitude: f64 },
    VecF32(Vec<f32>),
    Datetime(i64),
    Date(i64),
    Time(i64),
    Duration(i64),
}

impl WireValue {
    pub fn capture(runtime: &Runtime<'_>, value: &Value) -> Result<Self, String> {
        Ok(match value {
            Value::Null => Self::Null,
            Value::Bool(v) => Self::Bool(*v),
            Value::Int(v) => Self::Int(*v),
            Value::Float(v) => Self::Float(*v),
            Value::String(v) => Self::String(v.as_str().to_owned()),
            Value::Datetime(v) => Self::Datetime(*v),
            Value::Date(v) => Self::Date(*v),
            Value::Time(v) => Self::Time(*v),
            Value::Duration(v) => Self::Duration(*v),
            Value::List(values) => Self::List(
                values
                    .iter()
                    .map(|v| Self::capture(runtime, v))
                    .collect::<Result<Vec<_>, _>>()?,
            ),
            Value::Map(map) => Self::Map(
                map.iter()
                    .map(|(k, v)| {
                        Ok((k.as_str().to_owned(), Self::capture(runtime, v)?))
                    })
                    .collect::<Result<Vec<_>, String>>()?,
            ),
            Value::VecF32(values) => Self::VecF32(values.iter().copied().collect()),
            Value::Point(point) => Self::Point {
                latitude: f64::from(point.latitude),
                longitude: f64::from(point.longitude),
            },
            Value::Node(id) => {
                let node_id = u64::from(*id);
                let deleted = runtime.deleted_nodes.borrow();
                if let Some(node) = deleted.get(id) {
                    let graph = runtime.g.borrow();
                    let labels = node
                        .labels
                        .iter()
                        .map(|label| usize::from(*label) as u64)
                        .collect();
                    let properties = node
                        .attrs
                        .iter()
                        .map(|(key, value)| {
                            let attr_id = graph
                                .get_node_attribute_id(key)
                                .ok_or_else(|| {
                                    format!(
                                        "deleted node attribute {key:?} is missing from the graph schema"
                                    )
                                })? as u64;
                            Ok((attr_id, Self::capture(runtime, value)?))
                        })
                        .collect::<Result<Vec<_>, String>>()?;
                    Self::Node {
                        id: node_id,
                        labels,
                        properties,
                    }
                } else {
                    drop(deleted);
                    let graph = runtime.g.borrow();
                    let labels = graph
                        .get_node_label_ids(*id)
                        .map(|label| usize::from(label) as u64)
                        .collect();
                    let properties = graph
                        .get_node_all_attrs_by_id(*id)
                        .map(|(key, value)| {
                            Ok((key as u64, Self::capture(runtime, &value)?))
                        })
                        .collect::<Result<Vec<_>, String>>()?;
                    Self::Node {
                        id: node_id,
                        labels,
                        properties,
                    }
                }
            }
            Value::Relationship(rel) => {
                let (src, dst) = runtime.get_relationship_endpoints(*rel);
                let rel_id = u64::from(*rel);
                let deleted = runtime.deleted_relationships.borrow();
                if let Some(edge) = deleted.get(rel) {
                    let graph = runtime.g.borrow();
                    let type_id = graph
                        .get_type_id(&edge.type_name)
                        .map(usize::from)
                        .ok_or_else(|| {
                            format!(
                                "deleted relationship type {:?} is missing from the graph schema",
                                edge.type_name
                            )
                        })? as u64;
                    let properties = edge
                        .attrs
                        .iter()
                        .map(|(key, value)| {
                            let attr_id = graph
                                .get_global_attribute_id(key)
                                .ok_or_else(|| {
                                    format!(
                                        "deleted relationship attribute {key:?} is missing from the graph schema"
                                    )
                                })? as u64;
                            Ok((attr_id, Self::capture(runtime, value)?))
                        })
                        .collect::<Result<Vec<_>, String>>()?;
                    Self::Relationship {
                        id: rel_id,
                        type_id,
                        src: u64::from(src),
                        dst: u64::from(dst),
                        properties,
                    }
                } else {
                    drop(deleted);
                    let graph = runtime.g.borrow();
                    let type_id = usize::from(graph.get_relationship_type_id(*rel)) as u64;
                    let properties = graph
                        .get_relationship_all_attrs_by_id(*rel)
                        .map(|(key, value)| {
                            Ok((key as u64, Self::capture(runtime, &value)?))
                        })
                        .collect::<Result<Vec<_>, String>>()?;
                    Self::Relationship {
                        id: rel_id,
                        type_id,
                        src: u64::from(src),
                        dst: u64::from(dst),
                        properties,
                    }
                }
            }
            Value::Path(path) => {
                let mut nodes = Vec::new();
                let mut relationships = Vec::new();
                for item in path.iter() {
                    match item {
                        Value::Node(_) => nodes.push(Self::capture(runtime, item)?),
                        Value::Relationship(_) => relationships.push(Self::capture(runtime, item)?),
                        _ => {}
                    }
                }
                Self::Path {
                    nodes,
                    relationships,
                }
            }
        })
    }
}


#[cfg(test)]
mod tests {
    use super::*;

    use graph::{
        graph::{
            graph::{NodeId, Plan, RelationshipId},
            graphblas::{GrB_Mode, GrB_init},
            mvcc_graph::MvccGraph,
        },
        locks::WriteEscalation,
        runtime::{
            functions::init_functions,
            ordermap::OrderMap,
            runtime::Runtime,
            value::{DeletedNode, DeletedRelationship},
        },
    };
    use std::{
        collections::HashSet,
        sync::{Arc, Once},
    };

    static GRAPH_RUNTIME_INIT: Once = Once::new();

    struct AlwaysWritable;

    impl WriteEscalation for AlwaysWritable {
        fn upgrade_to_write(&self) -> Result<(), String> {
            Ok(())
        }
    }

    fn init_graph_runtime() {
        GRAPH_RUNTIME_INIT.call_once(|| {
            unsafe {
                GrB_init(GrB_Mode::GrB_NONBLOCKING as _);
            }
            init_functions().expect("initialize FalkorDB functions for wire tests");
        });
    }

    #[test]
    fn deleted_node_with_missing_schema_attribute_returns_error() {
        init_graph_runtime();

        let graph = MvccGraph::new(16, 16, 25, "wire-missing-node-schema");
        let Plan {
            plan, parameters, ..
        } = graph
            .read()
            .borrow()
            .get_plan("RETURN 1")
            .expect("plan simple wire test query");
        let escalation = AlwaysWritable;
        let runtime = Runtime::new(
            graph.read(),
            parameters,
            false,
            plan,
            false,
            String::new(),
            -1,
            false,
            None,
            0,
            None,
            &escalation,
        );

        let id = NodeId::from(7);
        runtime.deleted_nodes.borrow_mut().insert(
            id,
            DeletedNode::new(
                HashSet::new(),
                OrderMap::from_unique_keys([(
                    Arc::new("missing_property".to_string()),
                    Value::Int(1),
                )]),
            ),
        );

        let err = WireValue::capture(&runtime, &Value::Node(id))
            .expect_err("missing deleted-node schema metadata must be an error");
        assert!(err.contains("missing_property"), "{err}");
        assert!(err.contains("missing from the graph schema"), "{err}");
    }

    #[test]
    fn deleted_relationship_with_missing_schema_type_returns_error() {
        init_graph_runtime();

        let graph = MvccGraph::new(16, 16, 25, "wire-missing-rel-schema");
        let Plan {
            plan, parameters, ..
        } = graph
            .read()
            .borrow()
            .get_plan("RETURN 1")
            .expect("plan simple wire test query");
        let escalation = AlwaysWritable;
        let runtime = Runtime::new(
            graph.read(),
            parameters,
            false,
            plan,
            false,
            String::new(),
            -1,
            false,
            None,
            0,
            None,
            &escalation,
        );

        let id = RelationshipId::from(9);
        runtime.deleted_relationships.borrow_mut().insert(
            id,
            DeletedRelationship::new(
                NodeId::from(1),
                NodeId::from(2),
                Arc::new("MISSING_REL_TYPE".to_string()),
                OrderMap::default(),
            ),
        );

        let err = WireValue::capture(&runtime, &Value::Relationship(id))
            .expect_err("missing deleted-relationship type metadata must be an error");
        assert!(err.contains("MISSING_REL_TYPE"), "{err}");
        assert!(err.contains("missing from the graph schema"), "{err}");
    }
}
