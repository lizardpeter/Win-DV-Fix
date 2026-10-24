use std::{
    collections::{BTreeSet, HashMap},
    sync::Arc,
};

use graph::{
    index::IndexQuery,
    runtime::value::{Point, Value},
};

use crate::range_index::{NativeNumericRangeIndex, NativeStringRangeIndex};

#[derive(Clone, Default)]
struct NativeFieldIndex {
    numeric: NativeNumericRangeIndex,
    strings: NativeStringRangeIndex,
    numeric_array: NativeNumericRangeIndex,
    string_array: NativeStringRangeIndex,
    points: HashMap<u64, Point>,
}

impl NativeFieldIndex {
    fn remove_document(&mut self, doc: u64) {
        self.numeric.remove_document(doc);
        self.strings.remove_document(doc);
        self.numeric_array.remove_document(doc);
        self.string_array.remove_document(doc);
        self.points.remove(&doc);
    }

    fn upsert(
        &mut self,
        doc: u64,
        value: &Value,
    ) -> Result<(), String> {
        self.remove_document(doc);

        match value {
            Value::Bool(v) => self.numeric.upsert(doc, [f64::from(*v)])?,
            Value::Int(v) => self.numeric.upsert(doc, [*v as f64])?,
            Value::Float(v) => self.numeric.upsert(doc, [*v])?,
            Value::String(v) => self.strings.upsert(doc, [v.as_str()]),
            Value::Point(v) => {
                self.points.insert(doc, v.clone());
            }
            Value::Datetime(v) | Value::Date(v) | Value::Time(v) | Value::Duration(v) => {
                self.numeric.upsert(doc, [*v as f64])?;
            }
            Value::List(items) => {
                let mut numerics = Vec::new();
                let mut strings = Vec::new();
                for item in items.iter() {
                    match item {
                        Value::Bool(v) => numerics.push(f64::from(*v)),
                        Value::Int(v) => numerics.push(*v as f64),
                        Value::Float(v) => numerics.push(*v),
                        Value::String(v) => strings.push(v.as_str()),
                        _ => {}
                    }
                }
                self.numeric_array.upsert(doc, numerics)?;
                self.string_array.upsert(doc, strings);
            }
            Value::Null
            | Value::Map(_)
            | Value::Node(_)
            | Value::Relationship(_)
            | Value::Path(_)
            | Value::VecF32(_) => {}
        }

        Ok(())
    }
}

/// Backend-level native replacement for FalkorDB Range/Tag/Geo queries.
///
/// This consumes FalkorDB's existing IndexQuery<Value> AST directly, so the
/// planner/runtime contract does not change when this is wired into Index.
#[derive(Clone, Default)]
pub struct NativePropertyIndex {
    fields: HashMap<Arc<String>, NativeFieldIndex>,
}

impl NativePropertyIndex {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    pub fn upsert(
        &mut self,
        doc: u64,
        key: Arc<String>,
        value: &Value,
    ) -> Result<(), String> {
        self.fields.entry(key).or_default().upsert(doc, value)
    }

    pub fn remove_document(&mut self, doc: u64) {
        for field in self.fields.values_mut() {
            field.remove_document(doc);
        }
    }

    pub fn query(
        &self,
        query: IndexQuery<Value>,
    ) -> Result<std::vec::IntoIter<u64>, String> {
        Ok(self.query_set(query)?.into_iter().collect::<Vec<_>>().into_iter())
    }

    fn query_set(
        &self,
        query: IndexQuery<Value>,
    ) -> Result<BTreeSet<u64>, String> {
        match query {
            IndexQuery::Equal { key, value } => self.equal_set(&key, &value),
            IndexQuery::Range {
                key,
                min,
                max,
                include_min,
                include_max,
            } => self.range_set(&key, min.as_ref(), max.as_ref(), include_min, include_max),
            IndexQuery::And(children) => {
                let mut iter = children.into_iter();
                let Some(first) = iter.next() else {
                    return Ok(BTreeSet::new());
                };
                let mut out = self.query_set(first)?;
                for child in iter {
                    let rhs = self.query_set(child)?;
                    out.retain(|id| rhs.contains(id));
                    if out.is_empty() {
                        break;
                    }
                }
                Ok(out)
            }
            IndexQuery::Or(children) => {
                let mut out = BTreeSet::new();
                for child in children {
                    out.extend(self.query_set(child)?);
                }
                Ok(out)
            }
            IndexQuery::InList {
                key,
                list: Value::List(items),
            } => {
                let mut out = BTreeSet::new();
                for item in items.iter() {
                    out.extend(self.equal_set(&key, item)?);
                }
                Ok(out)
            }
            IndexQuery::ArrayContains { key, value } => {
                let Some(field) = self.fields.get(&key) else {
                    return Ok(BTreeSet::new());
                };
                match value {
                    Value::Bool(v) => Ok(field
                        .numeric_array
                        .equal(f64::from(v))?
                        .collect()),
                    Value::Int(v) => Ok(field.numeric_array.equal(v as f64)?.collect()),
                    Value::Float(v) => Ok(field.numeric_array.equal(v)?.collect()),
                    Value::String(v) => Ok(field.string_array.equal(v.as_str()).collect()),
                    _ => Ok(BTreeSet::new()),
                }
            }
            IndexQuery::Point {
                key,
                point: Value::Point(query_point),
                radius,
            } => {
                let radius = match radius {
                    Value::Float(v) => v,
                    Value::Int(v) => v as f64,
                    _ => return Ok(BTreeSet::new()),
                };
                if radius < 0.0 || !radius.is_finite() {
                    return Ok(BTreeSet::new());
                }
                let Some(field) = self.fields.get(&key) else {
                    return Ok(BTreeSet::new());
                };
                Ok(field
                    .points
                    .iter()
                    .filter_map(|(&doc, candidate)| {
                        (query_point.distance(candidate) <= radius).then_some(doc)
                    })
                    .collect())
            }
            _ => Ok(BTreeSet::new()),
        }
    }

    fn equal_set(
        &self,
        key: &Arc<String>,
        value: &Value,
    ) -> Result<BTreeSet<u64>, String> {
        let Some(field) = self.fields.get(key) else {
            return Ok(BTreeSet::new());
        };
        match value {
            Value::Bool(v) => Ok(field.numeric.equal(f64::from(*v))?.collect()),
            Value::Int(v) => Ok(field.numeric.equal(*v as f64)?.collect()),
            Value::Float(v) => Ok(field.numeric.equal(*v)?.collect()),
            Value::String(v) => Ok(field.strings.equal(v.as_str()).collect()),
            _ => Ok(BTreeSet::new()),
        }
    }

    fn range_set(
        &self,
        key: &Arc<String>,
        min: Option<&Value>,
        max: Option<&Value>,
        include_min: bool,
        include_max: bool,
    ) -> Result<BTreeSet<u64>, String> {
        let Some(field) = self.fields.get(key) else {
            return Ok(BTreeSet::new());
        };

        let is_string = matches!(min, Some(Value::String(_)))
            || matches!(max, Some(Value::String(_)));

        if is_string {
            let min = match min {
                Some(Value::String(v)) => Some(v.as_str()),
                None => None,
                _ => return Ok(BTreeSet::new()),
            };
            let max = match max {
                Some(Value::String(v)) => Some(v.as_str()),
                None => None,
                _ => return Ok(BTreeSet::new()),
            };
            Ok(field
                .strings
                .range(min, max, include_min, include_max)
                .collect())
        } else {
            let min = match min {
                Some(Value::Bool(v)) => Some(f64::from(*v)),
                Some(Value::Int(v)) => Some(*v as f64),
                Some(Value::Float(v)) => Some(*v),
                None => None,
                _ => return Ok(BTreeSet::new()),
            };
            let max = match max {
                Some(Value::Bool(v)) => Some(f64::from(*v)),
                Some(Value::Int(v)) => Some(*v as f64),
                Some(Value::Float(v)) => Some(*v),
                None => None,
                _ => return Ok(BTreeSet::new()),
            };
            Ok(field
                .numeric
                .range(min, max, include_min, include_max)?
                .collect())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(name: &str) -> Arc<String> {
        Arc::new(name.to_string())
    }

    #[test]
    fn index_query_ast_parity_smoke() {
        let mut idx = NativePropertyIndex::new();
        idx.upsert(1, key("address"), &Value::Int(100)).unwrap();
        idx.upsert(2, key("address"), &Value::Int(200)).unwrap();
        idx.upsert(3, key("address"), &Value::Int(300)).unwrap();

        idx.upsert(1, key("name"), &Value::String(Arc::new("alpha".into())))
            .unwrap();
        idx.upsert(2, key("name"), &Value::String(Arc::new("beta".into())))
            .unwrap();

        let q = IndexQuery::And(vec![
            IndexQuery::Range {
                key: key("address"),
                min: Some(Value::Int(100)),
                max: Some(Value::Int(250)),
                include_min: true,
                include_max: true,
            },
            IndexQuery::InList {
                key: key("name"),
                list: Value::List(Arc::new(thin_vec::thin_vec![
                    Value::String(Arc::new("beta".into()))
                ])),
            },
        ]);
        assert_eq!(idx.query(q).unwrap().collect::<Vec<_>>(), vec![2]);
    }

    #[test]
    fn array_and_geo_queries() {
        let mut idx = NativePropertyIndex::new();
        idx.upsert(
            7,
            key("tags"),
            &Value::List(Arc::new(thin_vec::thin_vec![
                Value::String(Arc::new("shader".into())),
                Value::Int(42)
            ])),
        )
        .unwrap();

        let contains = IndexQuery::ArrayContains {
            key: key("tags"),
            value: Value::String(Arc::new("shader".into())),
        };
        assert_eq!(idx.query(contains).unwrap().collect::<Vec<_>>(), vec![7]);

        let a = Point::new(40.0, -73.0);
        let b = Point::new(40.001, -73.0);
        idx.upsert(9, key("where"), &Value::Point(a.clone())).unwrap();
        idx.upsert(10, key("where"), &Value::Point(Point::new(41.0, -73.0)))
            .unwrap();

        let geo = IndexQuery::Point {
            key: key("where"),
            point: Value::Point(b),
            radius: Value::Float(1000.0),
        };
        assert_eq!(idx.query(geo).unwrap().collect::<Vec<_>>(), vec![9]);
    }
}
