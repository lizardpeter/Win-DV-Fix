use std::{collections::{BTreeMap, BTreeSet, HashMap}, sync::Arc};

use graph::index::falkordb::data_structures::cow_btree::{CowBTree, RangeIter};

const SIGN_BIT: u64 = 1u64 << 63;

/// Native numeric/range index backed by FalkorDB's in-repo copy-on-write B+ tree.
///
/// FalkorDB's current RediSearch bridge indexes numeric values as f64, so this
/// deliberately follows the same numeric domain for parity during migration.
/// NaN is rejected because it has no useful total numeric ordering.
#[derive(Clone, Default)]
pub struct NativeNumericRangeIndex {
    tree: CowBTree,
    by_doc: HashMap<u64, Vec<u64>>,
}

pub enum NativeRangeIter {
    Tree(RangeIter),
    Empty,
}

impl Iterator for NativeRangeIter {
    type Item = u64;

    fn next(&mut self) -> Option<Self::Item> {
        match self {
            Self::Tree(iter) => iter.next(),
            Self::Empty => None,
        }
    }
}

impl NativeNumericRangeIndex {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Replace every numeric value currently indexed for this document.
    ///
    /// Validation occurs before mutation, so rejected input leaves the existing
    /// document state unchanged.
    pub fn upsert<I>(
        &mut self,
        doc: u64,
        values: I,
    ) -> Result<(), String>
    where
        I: IntoIterator<Item = f64>,
    {
        let mut keys = Vec::new();
        for value in values {
            let key = encode_numeric(value)?;
            keys.push(key);
        }
        keys.sort_unstable();
        keys.dedup();

        self.remove_document(doc);

        if !keys.is_empty() {
            let pairs: Vec<(u64, u64)> = keys.iter().copied().map(|key| (key, doc)).collect();
            self.tree.insert_batch(&pairs);
            self.by_doc.insert(doc, keys);
        }

        Ok(())
    }

    pub fn remove_document(
        &mut self,
        doc: u64,
    ) -> bool {
        let Some(keys) = self.by_doc.remove(&doc) else {
            return false;
        };
        for key in keys {
            self.tree.remove(key, doc);
        }
        true
    }

    #[must_use]
    pub fn contains_document(
        &self,
        doc: u64,
    ) -> bool {
        self.by_doc.contains_key(&doc)
    }

    pub fn equal(
        &self,
        value: f64,
    ) -> Result<NativeRangeIter, String> {
        let key = encode_numeric(value)?;
        Ok(NativeRangeIter::Tree(self.tree.point(key)))
    }

    pub fn range(
        &self,
        min: Option<f64>,
        max: Option<f64>,
        include_min: bool,
        include_max: bool,
    ) -> Result<NativeRangeIter, String> {
        let lo = match min {
            Some(value) => {
                let key = encode_numeric(value)?;
                if include_min {
                    key
                } else if let Some(next) = key.checked_add(1) {
                    next
                } else {
                    return Ok(NativeRangeIter::Empty);
                }
            }
            None => u64::MIN,
        };

        let hi = match max {
            Some(value) => {
                let key = encode_numeric(value)?;
                if include_max {
                    key
                } else if let Some(prev) = key.checked_sub(1) {
                    prev
                } else {
                    return Ok(NativeRangeIter::Empty);
                }
            }
            None => u64::MAX,
        };

        if lo > hi {
            return Ok(NativeRangeIter::Empty);
        }

        Ok(NativeRangeIter::Tree(self.tree.range(lo, hi)))
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.tree.is_empty()
    }
}


/// Native lexicographic string range index.
///
/// The upstream native CowBTree is intentionally specialized to u64 numeric
/// keys. String ordering therefore uses a Rust BTreeMap for correctness first;
/// the public API mirrors the numeric backend so it can later be replaced by a
/// compact page format without changing Indexer/Planner integration.
#[derive(Clone, Default)]
pub struct NativeStringRangeIndex {
    by_value: BTreeMap<Arc<str>, BTreeSet<u64>>,
    by_doc: HashMap<u64, Vec<Arc<str>>>,
}

impl NativeStringRangeIndex {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    pub fn upsert<I, S>(
        &mut self,
        doc: u64,
        values: I,
    )
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        let mut values: Vec<Arc<str>> = values
            .into_iter()
            .map(|value| Arc::<str>::from(value.as_ref()))
            .collect();
        values.sort_unstable();
        values.dedup();

        self.remove_document(doc);

        for value in &values {
            self.by_value.entry(Arc::clone(value)).or_default().insert(doc);
        }
        if !values.is_empty() {
            self.by_doc.insert(doc, values);
        }
    }

    pub fn remove_document(
        &mut self,
        doc: u64,
    ) -> bool {
        let Some(values) = self.by_doc.remove(&doc) else {
            return false;
        };

        for value in values {
            let remove_key = if let Some(docs) = self.by_value.get_mut(value.as_ref()) {
                docs.remove(&doc);
                docs.is_empty()
            } else {
                false
            };
            if remove_key {
                self.by_value.remove(value.as_ref());
            }
        }
        true
    }

    #[must_use]
    pub fn equal(
        &self,
        value: &str,
    ) -> std::vec::IntoIter<u64> {
        self.by_value
            .get(value)
            .map_or_else(Vec::new, |docs| docs.iter().copied().collect())
            .into_iter()
    }

    /// Lexicographic UTF-8/Unicode-scalar ordering. Rust String's byte ordering
    /// is consistent with Unicode scalar ordering for valid UTF-8.
    #[must_use]
    pub fn range(
        &self,
        min: Option<&str>,
        max: Option<&str>,
        include_min: bool,
        include_max: bool,
    ) -> std::vec::IntoIter<u64> {
        use std::ops::Bound::{Excluded, Included, Unbounded};

        let lower = match min {
            Some(v) if include_min => Included(v),
            Some(v) => Excluded(v),
            None => Unbounded,
        };
        let upper = match max {
            Some(v) if include_max => Included(v),
            Some(v) => Excluded(v),
            None => Unbounded,
        };

        let mut out = Vec::new();
        for (_, docs) in self.by_value.range::<str, _>((lower, upper)) {
            out.extend(docs.iter().copied());
        }
        out.into_iter()
    }

    #[must_use]
    pub fn contains_document(
        &self,
        doc: u64,
    ) -> bool {
        self.by_doc.contains_key(&doc)
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.by_value.is_empty()
    }
}

/// Convert an f64 to a u64 whose ordinary unsigned ordering matches numeric
/// ordering. -0.0 is canonicalized to +0.0 so Cypher numeric equality does not
/// distinguish the two IEEE zero encodings.
pub fn encode_numeric(mut value: f64) -> Result<u64, String> {
    if value.is_nan() {
        return Err("NaN cannot be indexed in the native numeric range index".into());
    }
    if value == 0.0 {
        value = 0.0;
    }

    let bits = value.to_bits();
    Ok(if bits & SIGN_BIT != 0 {
        !bits
    } else {
        bits ^ SIGN_BIT
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn collect(iter: NativeRangeIter) -> Vec<u64> {
        iter.collect()
    }

    #[test]
    fn sortable_encoding_tracks_numeric_order() {
        let values = [
            f64::NEG_INFINITY,
            -1000.0,
            -2.5,
            -0.0,
            0.0,
            0.5,
            99.0,
            f64::INFINITY,
        ];
        let keys: Vec<u64> = values
            .iter()
            .map(|&v| encode_numeric(v).unwrap())
            .collect();
        assert!(keys.windows(2).all(|w| w[0] <= w[1]));
        assert_eq!(encode_numeric(-0.0).unwrap(), encode_numeric(0.0).unwrap());
        assert!(encode_numeric(f64::NAN).is_err());
    }

    #[test]
    fn equality_range_upsert_and_remove() {
        let mut idx = NativeNumericRangeIndex::new();
        idx.upsert(10, [5.0]).unwrap();
        idx.upsert(20, [10.0]).unwrap();
        idx.upsert(30, [10.0, 15.0]).unwrap();

        assert_eq!(collect(idx.equal(10.0).unwrap()), vec![20, 30]);
        assert_eq!(
            collect(idx.range(Some(5.0), Some(10.0), true, true).unwrap()),
            vec![10, 20, 30]
        );
        assert_eq!(
            collect(idx.range(Some(5.0), Some(10.0), false, false).unwrap()),
            Vec::<u64>::new()
        );

        idx.upsert(20, [25.0]).unwrap();
        assert_eq!(collect(idx.equal(10.0).unwrap()), vec![30]);
        assert_eq!(collect(idx.equal(25.0).unwrap()), vec![20]);

        assert!(idx.remove_document(30));
        assert!(!idx.contains_document(30));
        assert_eq!(collect(idx.equal(10.0).unwrap()), Vec::<u64>::new());
    }

    #[test]
    fn string_equality_range_upsert_and_remove() {
        let mut idx = NativeStringRangeIndex::new();
        idx.upsert(1, ["alpha"]);
        idx.upsert(2, ["beta", "delta"]);
        idx.upsert(3, ["gamma"]);

        assert_eq!(idx.equal("beta").collect::<Vec<_>>(), vec![2]);
        assert_eq!(
            idx.range(Some("beta"), Some("gamma"), true, false)
                .collect::<Vec<_>>(),
            vec![2, 2]
        );

        idx.upsert(2, ["epsilon"]);
        assert!(idx.equal("beta").next().is_none());
        assert_eq!(idx.equal("epsilon").collect::<Vec<_>>(), vec![2]);

        assert!(idx.remove_document(1));
        assert!(!idx.contains_document(1));
        assert!(idx.equal("alpha").next().is_none());
    }

    #[test]
    fn iterators_hold_snapshot_across_writes() {
        let mut idx = NativeNumericRangeIndex::new();
        idx.upsert(1, [1.0]).unwrap();
        idx.upsert(2, [2.0]).unwrap();

        let old = idx.range(None, None, true, true).unwrap();

        idx.upsert(3, [3.0]).unwrap();
        idx.remove_document(1);

        assert_eq!(collect(old), vec![1, 2]);
        assert_eq!(
            collect(idx.range(None, None, true, true).unwrap()),
            vec![2, 3]
        );
    }
}
