//! Pure-Rust standalone index backend for the native FalkorDB host.
//!
//! This preserves the public API consumed by indexer.rs/planner/runtime while
//! removing the RediSearch C/Redis-module dependency entirely. The first
//! correctness implementation uses an internally synchronized document store;
//! hot range paths can be moved to FalkorDB's CowBTree without API changes.

pub mod falkordb;
pub mod indexer;
pub mod text_index_options;
pub mod vector_index_options;
pub use text_index_options::TextIndexOptions;
pub use vector_index_options::VectorIndexOptions;

use std::{
    cmp::Ordering as CmpOrdering,
    collections::{BTreeMap, BTreeSet, HashMap, HashSet},
    ffi::CString,
    hash::Hash,
    marker::PhantomData,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
};

use parking_lot::{Mutex, RwLock};

use crate::runtime::{value::Value, vec_distance};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Default)]
pub enum IndexType {
    Range,
    #[default]
    Fulltext,
    Vector,
}

#[derive(Debug, Default)]
pub struct Field {
    pub name: CString,
    pub ty: IndexType,
    options: Option<TextIndexOptions>,
    vector_options: Option<VectorIndexOptions>,
    numeric_arr_name: Option<CString>,
    string_arr_name: Option<CString>,
}

impl Field {
    fn make_arr_names(name: &CString, ty: &IndexType) -> (Option<CString>, Option<CString>) {
        if *ty == IndexType::Range {
            let base = name.to_str().unwrap_or("");
            (
                CString::new(format!("{base}:numeric:arr")).ok(),
                CString::new(format!("{base}:string:arr")).ok(),
            )
        } else {
            (None, None)
        }
    }

    #[must_use]
    pub fn new(name: CString, ty: IndexType, options: Option<TextIndexOptions>) -> Self {
        let (numeric_arr_name, string_arr_name) = Self::make_arr_names(&name, &ty);
        Self {
            name,
            ty,
            options,
            vector_options: None,
            numeric_arr_name,
            string_arr_name,
        }
    }

    #[must_use]
    pub fn new_with_vector_options(
        name: CString,
        ty: IndexType,
        vector_options: VectorIndexOptions,
    ) -> Self {
        let (numeric_arr_name, string_arr_name) = Self::make_arr_names(&name, &ty);
        Self {
            name,
            ty,
            options: None,
            vector_options: Some(vector_options),
            numeric_arr_name,
            string_arr_name,
        }
    }

    #[must_use]
    pub const fn options(&self) -> Option<&TextIndexOptions> {
        self.options.as_ref()
    }

    #[must_use]
    pub const fn vector_options(&self) -> Option<&VectorIndexOptions> {
        self.vector_options.as_ref()
    }

    #[must_use]
    pub const fn numeric_arr_name(&self) -> Option<&CString> {
        self.numeric_arr_name.as_ref()
    }

    #[must_use]
    pub const fn string_arr_name(&self) -> Option<&CString> {
        self.string_arr_name.as_ref()
    }
}

impl PartialEq for Field {
    fn eq(&self, other: &Self) -> bool {
        self.name == other.name && self.ty == other.ty
    }
}
impl Eq for Field {}
impl Hash for Field {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.name.hash(state);
    }
}

pub struct IndexInfo {
    pub label: Arc<String>,
    pub pending: i32,
    pub progress: u64,
    pub total: u64,
    pub fields: HashMap<Arc<String>, Vec<Arc<Field>>>,
    pub field_order: Vec<Arc<String>>,
    pub language: Option<Arc<String>>,
    pub stopwords: Option<Vec<Arc<String>>>,
    pub entity_type: String,
}

#[derive(Debug)]
pub enum IndexQuery<T> {
    Equal { key: Arc<String>, value: T },
    Range {
        key: Arc<String>,
        min: Option<T>,
        max: Option<T>,
        include_min: bool,
        include_max: bool,
    },
    And(Vec<Self>),
    Or(Vec<Self>),
    Point { key: Arc<String>, point: T, radius: T },
    InList { key: Arc<String>, list: T },
    ArrayContains { key: Arc<String>, value: T },
}

pub struct IndexResultsIter<T, F = fn()> {
    inner: std::vec::IntoIter<T>,
    _marker: PhantomData<F>,
}

impl<T, F> IndexResultsIter<T, F> {
    fn from_vec(values: Vec<T>) -> Self {
        Self {
            inner: values.into_iter(),
            _marker: PhantomData,
        }
    }
}
impl<T, F> Iterator for IndexResultsIter<T, F> {
    type Item = T;
    fn next(&mut self) -> Option<Self::Item> {
        self.inner.next()
    }
}

type IdMap = fn();
type ScoreMap = fn();
pub type IdIter = IndexResultsIter<u64, IdMap>;
pub type ScoredIdIter = IndexResultsIter<(u64, f64), ScoreMap>;

impl IndexResultsIter<u64, IdMap> {
    #[must_use]
    pub fn empty() -> Self {
        Self::from_vec(Vec::new())
    }
}
impl IndexResultsIter<(u64, f64), ScoreMap> {
    #[must_use]
    pub fn empty_scored() -> Self {
        Self::from_vec(Vec::new())
    }
}

pub struct EdgeTripleIter {
    inner: std::vec::IntoIter<(u64, u64, u64)>,
}
impl EdgeTripleIter {
    fn from_vec(v: Vec<(u64, u64, u64)>) -> Self {
        Self { inner: v.into_iter() }
    }
    #[must_use]
    pub fn empty() -> Self {
        Self::from_vec(Vec::new())
    }
}
impl Iterator for EdgeTripleIter {
    type Item = (u64, u64, u64);
    fn next(&mut self) -> Option<Self::Item> {
        self.inner.next()
    }
}

pub struct ScoredEdgeTripleIter {
    inner: std::vec::IntoIter<(u64, u64, u64, f64)>,
}
impl ScoredEdgeTripleIter {
    fn from_vec(v: Vec<(u64, u64, u64, f64)>) -> Self {
        Self { inner: v.into_iter() }
    }
    #[must_use]
    pub fn empty() -> Self {
        Self::from_vec(Vec::new())
    }
}
impl Iterator for ScoredEdgeTripleIter {
    type Item = (u64, u64, u64, f64);
    fn next(&mut self) -> Option<Self::Item> {
        self.inner.next()
    }
}

pub struct VectorScoredIdIter {
    inner: ScoredIdIter,
    _vector_owner: Arc<thin_vec::ThinVec<f32>>,
}
impl VectorScoredIdIter {
    #[must_use]
    pub fn empty(vector: Arc<thin_vec::ThinVec<f32>>) -> Self {
        Self {
            inner: IndexResultsIter::empty_scored(),
            _vector_owner: vector,
        }
    }
    fn from_vec(
        values: Vec<(u64, f64)>,
        vector: Arc<thin_vec::ThinVec<f32>>,
    ) -> Self {
        Self {
            inner: IndexResultsIter::from_vec(values),
            _vector_owner: vector,
        }
    }
}
impl Iterator for VectorScoredIdIter {
    type Item = (u64, f64);
    fn next(&mut self) -> Option<Self::Item> {
        self.inner.next()
    }
}

pub struct VectorScoredEdgeTripleIter {
    inner: ScoredEdgeTripleIter,
    _vector_owner: Arc<thin_vec::ThinVec<f32>>,
}
impl VectorScoredEdgeTripleIter {
    #[must_use]
    pub fn empty(vector: Arc<thin_vec::ThinVec<f32>>) -> Self {
        Self {
            inner: ScoredEdgeTripleIter::empty(),
            _vector_owner: vector,
        }
    }
    fn from_vec(
        values: Vec<(u64, u64, u64, f64)>,
        vector: Arc<thin_vec::ThinVec<f32>>,
    ) -> Self {
        Self {
            inner: ScoredEdgeTripleIter::from_vec(values),
            _vector_owner: vector,
        }
    }
}
impl Iterator for VectorScoredEdgeTripleIter {
    type Item = (u64, u64, u64, f64);
    fn next(&mut self) -> Option<Self::Item> {
        self.inner.next()
    }
}

pub struct Document {
    id: u64,
    edge: Option<(u64, u64, u64)>,
    values: HashMap<String, Value>,
}
impl Document {
    #[must_use]
    pub fn new(id: u64) -> Self {
        Self {
            id,
            edge: None,
            values: HashMap::new(),
        }
    }
    #[must_use]
    pub fn new_edge(src: u64, dst: u64, edge_id: u64) -> Self {
        Self {
            id: edge_id,
            edge: Some((src, dst, edge_id)),
            values: HashMap::new(),
        }
    }
    #[must_use]
    pub const fn id(&self) -> u64 {
        self.id
    }
    pub fn set(&mut self, field: &Field, value: &Value) {
        if value_matches_field(field.ty, value) {
            self.values
                .insert(field.name.to_string_lossy().into_owned(), value.clone());
        }
    }
}

#[derive(Clone)]
struct NativeDocument {
    id: u64,
    edge: Option<(u64, u64, u64)>,
    values: HashMap<String, Value>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
struct NumericKey(u64);

impl NumericKey {
    fn from_f64(mut value: f64) -> Option<Self> {
        if value.is_nan() {
            return None;
        }
        if value == 0.0 {
            value = 0.0;
        }
        let bits = value.to_bits();
        Some(Self(if bits & (1u64 << 63) != 0 {
            !bits
        } else {
            bits ^ (1u64 << 63)
        }))
    }

    fn from_value(value: &Value) -> Option<Self> {
        numeric_value(value).and_then(Self::from_f64)
    }
}

type NumericPostings = HashMap<String, BTreeMap<NumericKey, BTreeSet<u64>>>;
type StringPostings = HashMap<String, BTreeMap<String, BTreeSet<u64>>>;

#[derive(Default)]
struct NativeStore {
    docs: HashMap<u64, NativeDocument>,
    scalar_numeric: NumericPostings,
    scalar_string: StringPostings,
    array_numeric: NumericPostings,
    array_string: StringPostings,
    points: HashMap<String, HashMap<u64, crate::runtime::value::Point>>,
    /// Key namespace is r:<token>, s:<stem>, or p:<soundex>.
    /// Values are per-document term scores precomputed at insert/update time.
    fulltext: HashMap<String, HashMap<u64, f64>>,
}

impl NativeStore {
    fn upsert(
        &mut self,
        document: NativeDocument,
        fields: &HashMap<Arc<String>, Vec<Arc<Field>>>,
    ) {
        self.remove(document.id, fields);
        self.index_document(&document, fields);
        self.docs.insert(document.id, document);
    }

    fn remove(
        &mut self,
        id: u64,
        fields: &HashMap<Arc<String>, Vec<Arc<Field>>>,
    ) -> Option<NativeDocument> {
        let document = self.docs.remove(&id)?;
        self.unindex_document(&document, fields);
        Some(document)
    }

    fn rebuild(
        &mut self,
        fields: &HashMap<Arc<String>, Vec<Arc<Field>>>,
    ) {
        let docs: Vec<NativeDocument> = self.docs.values().cloned().collect();
        self.scalar_numeric.clear();
        self.scalar_string.clear();
        self.array_numeric.clear();
        self.array_string.clear();
        self.points.clear();
        self.fulltext.clear();
        for document in &docs {
            self.index_document(document, fields);
        }
    }

    fn index_document(
        &mut self,
        document: &NativeDocument,
        fields: &HashMap<Arc<String>, Vec<Arc<Field>>>,
    ) {
        for field in fields.values().flatten() {
            let name = field.name.to_string_lossy();
            let Some(value) = document.values.get(name.as_ref()) else {
                continue;
            };

            match field.ty {
                IndexType::Range => {
                    self.index_range_value(name.as_ref(), document.id, value);
                }
                IndexType::Fulltext => {
                    if let Value::String(text) = value {
                        self.index_fulltext_value(field, document.id, text);
                    }
                }
                IndexType::Vector => {}
            }
        }
    }

    fn unindex_document(
        &mut self,
        document: &NativeDocument,
        fields: &HashMap<Arc<String>, Vec<Arc<Field>>>,
    ) {
        for field in fields.values().flatten() {
            let name = field.name.to_string_lossy();
            let Some(value) = document.values.get(name.as_ref()) else {
                continue;
            };

            match field.ty {
                IndexType::Range => {
                    self.unindex_range_value(name.as_ref(), document.id, value);
                }
                IndexType::Fulltext => {
                    if let Value::String(text) = value {
                        self.unindex_fulltext_value(field, document.id, text);
                    }
                }
                IndexType::Vector => {}
            }
        }
    }

    fn index_range_value(&mut self, field: &str, id: u64, value: &Value) {
        match value {
            Value::String(value) => insert_string_posting(
                &mut self.scalar_string,
                field,
                value.as_str(),
                id,
            ),
            Value::List(values) => {
                for item in values.iter() {
                    if let Value::String(value) = item {
                        insert_string_posting(
                            &mut self.array_string,
                            field,
                            value.as_str(),
                            id,
                        );
                    } else if let Some(key) = NumericKey::from_value(item) {
                        insert_numeric_posting(
                            &mut self.array_numeric,
                            field,
                            key,
                            id,
                        );
                    }
                }
            }
            Value::Point(point) => {
                self.points
                    .entry(field.to_string())
                    .or_default()
                    .insert(id, point.clone());
            }
            _ => {
                if let Some(key) = NumericKey::from_value(value) {
                    insert_numeric_posting(&mut self.scalar_numeric, field, key, id);
                }
            }
        }
    }

    fn unindex_range_value(&mut self, field: &str, id: u64, value: &Value) {
        match value {
            Value::String(value) => remove_string_posting(
                &mut self.scalar_string,
                field,
                value.as_str(),
                id,
            ),
            Value::List(values) => {
                for item in values.iter() {
                    if let Value::String(value) = item {
                        remove_string_posting(
                            &mut self.array_string,
                            field,
                            value.as_str(),
                            id,
                        );
                    } else if let Some(key) = NumericKey::from_value(item) {
                        remove_numeric_posting(
                            &mut self.array_numeric,
                            field,
                            key,
                            id,
                        );
                    }
                }
            }
            Value::Point(_) => {
                if let Some(points) = self.points.get_mut(field) {
                    points.remove(&id);
                    if points.is_empty() {
                        self.points.remove(field);
                    }
                }
            }
            _ => {
                if let Some(key) = NumericKey::from_value(value) {
                    remove_numeric_posting(&mut self.scalar_numeric, field, key, id);
                }
            }
        }
    }

    fn index_fulltext_value(&mut self, field: &Field, id: u64, text: &str) {
        let opts = field.options();
        let weight = opts.and_then(|o| o.weight).unwrap_or(1.0);
        let nostem = opts.and_then(|o| o.nostem).unwrap_or(false);
        let phonetic = opts
            .and_then(|o| o.phonetic.as_deref())
            .is_some_and(|p| !p.is_empty());

        for token in tokenize(text) {
            let key = if phonetic {
                format!("p:{}", soundex(&token))
            } else if nostem {
                format!("r:{token}")
            } else {
                format!("s:{}", stem(&token))
            };
            *self
                .fulltext
                .entry(key)
                .or_default()
                .entry(id)
                .or_default() += weight;
        }
    }

    fn unindex_fulltext_value(&mut self, field: &Field, id: u64, text: &str) {
        let opts = field.options();
        let nostem = opts.and_then(|o| o.nostem).unwrap_or(false);
        let phonetic = opts
            .and_then(|o| o.phonetic.as_deref())
            .is_some_and(|p| !p.is_empty());

        let mut keys = BTreeSet::new();
        for token in tokenize(text) {
            keys.insert(if phonetic {
                format!("p:{}", soundex(&token))
            } else if nostem {
                format!("r:{token}")
            } else {
                format!("s:{}", stem(&token))
            });
        }
        for key in keys {
            if let Some(posting) = self.fulltext.get_mut(&key) {
                posting.remove(&id);
                if posting.is_empty() {
                    self.fulltext.remove(&key);
                }
            }
        }
    }

    fn fulltext_term_scores(&self, term: &str) -> HashMap<u64, f64> {
        let keys = [
            format!("r:{term}"),
            format!("s:{}", stem(term)),
            format!("p:{}", soundex(term)),
        ];
        let mut scores = HashMap::new();
        for key in keys {
            if let Some(posting) = self.fulltext.get(&key) {
                for (&id, &score) in posting {
                    *scores.entry(id).or_insert(0.0) += score;
                }
            }
        }
        scores
    }
}

fn insert_numeric_posting(
    index: &mut NumericPostings,
    field: &str,
    key: NumericKey,
    id: u64,
) {
    index
        .entry(field.to_string())
        .or_default()
        .entry(key)
        .or_default()
        .insert(id);
}

fn remove_numeric_posting(
    index: &mut NumericPostings,
    field: &str,
    key: NumericKey,
    id: u64,
) {
    let mut remove_field = false;
    if let Some(values) = index.get_mut(field) {
        let mut remove_key = false;
        if let Some(ids) = values.get_mut(&key) {
            ids.remove(&id);
            remove_key = ids.is_empty();
        }
        if remove_key {
            values.remove(&key);
        }
        remove_field = values.is_empty();
    }
    if remove_field {
        index.remove(field);
    }
}

fn insert_string_posting(
    index: &mut StringPostings,
    field: &str,
    value: &str,
    id: u64,
) {
    index
        .entry(field.to_string())
        .or_default()
        .entry(value.to_string())
        .or_default()
        .insert(id);
}

fn remove_string_posting(
    index: &mut StringPostings,
    field: &str,
    value: &str,
    id: u64,
) {
    let mut remove_field = false;
    if let Some(values) = index.get_mut(field) {
        let mut remove_key = false;
        if let Some(ids) = values.get_mut(value) {
            ids.remove(&id);
            remove_key = ids.is_empty();
        }
        if remove_key {
            values.remove(value);
        }
        remove_field = values.is_empty();
    }
    if remove_field {
        index.remove(field);
    }
}

#[derive(Debug, Default)]
struct PendingSlots {
    current_generation: u64,
    current_pending: i32,
    stale_pending: i32,
}

pub struct Index {
    id: u64,
    ready: bool,
    fields: HashMap<Arc<String>, Vec<Arc<Field>>>,
    field_order: Vec<Arc<String>>,
    pending_slots: Arc<Mutex<PendingSlots>>,
    progress: AtomicU64,
    total: AtomicU64,
    language: Option<Arc<String>>,
    stopwords: Option<Vec<Arc<String>>>,
    store: Arc<RwLock<NativeStore>>,
}

impl Default for Index {
    fn default() -> Self {
        static NEXT_INDEX_ID: AtomicU64 = AtomicU64::new(1);
        let id = NEXT_INDEX_ID.fetch_add(1, Ordering::Relaxed);
        Self {
            id,
            ready: false,
            fields: HashMap::new(),
            field_order: Vec::new(),
            pending_slots: Arc::new(Mutex::new(PendingSlots {
                current_generation: id,
                current_pending: 0,
                stale_pending: 0,
            })),
            progress: AtomicU64::new(0),
            total: AtomicU64::new(0),
            language: None,
            stopwords: None,
            store: Arc::new(RwLock::new(NativeStore::default())),
        }
    }
}

impl Index {
    #[must_use]
    pub const fn id(&self) -> u64 { self.id }

    pub fn bump_id(&mut self) {
        static NEXT_INDEX_ID: AtomicU64 = AtomicU64::new(u64::MAX / 2);
        self.id = NEXT_INDEX_ID.fetch_add(1, Ordering::Relaxed);
        let mut slots = self.pending_slots.lock();
        slots.stale_pending += slots.current_pending;
        slots.current_generation = self.id;
        slots.current_pending = 0;
    }

    #[must_use]
    pub fn clone_for_update(&self) -> Self {
        Self {
            id: self.id,
            ready: self.ready,
            fields: self.fields.clone(),
            field_order: self.field_order.clone(),
            pending_slots: self.pending_slots.clone(),
            progress: AtomicU64::new(self.progress.load(Ordering::Relaxed)),
            total: AtomicU64::new(self.total.load(Ordering::Relaxed)),
            language: self.language.clone(),
            stopwords: self.stopwords.clone(),
            store: self.store.clone(),
        }
    }

    #[must_use]
    pub const fn has_rs_index(&self) -> bool { self.ready }

    pub fn create_rs_index(
        &mut self,
        _label: &Arc<String>,
        _stopwords: Option<&Vec<Arc<String>>>,
        _language: Option<&Arc<String>>,
    ) -> Result<(), String> {
        self.ready = true;
        Ok(())
    }

    pub fn register_fields(
        &self,
        _fields: &HashMap<Arc<String>, Vec<Arc<Field>>>,
        _field_options: Option<&TextIndexOptions>,
    ) -> Result<(), String> {
        Ok(())
    }

    pub fn add_document(&self, doc: &mut Document) {
        self.store.write().docs.insert(
            doc.id,
            NativeDocument {
                id: doc.id,
                edge: doc.edge,
                values: std::mem::take(&mut doc.values),
            },
        );
    }

    pub fn delete_document(&self, id: u64) {
        self.store.write().docs.remove(&id);
    }

    pub fn delete_edge_document(&self, _src: u64, _dst: u64, edge_id: u64) {
        self.store.write().docs.remove(&edge_id);
    }

    pub fn query(&self, query: IndexQuery<Value>) -> IdIter {
        let store = self.store.read();
        let mut ids: Vec<u64> = store
            .docs
            .values()
            .filter(|doc| self.matches_query(doc, &query))
            .map(|doc| doc.id)
            .collect();
        ids.sort_unstable();
        ids.dedup();
        IndexResultsIter::from_vec(ids)
    }

    pub fn query_edges(&self, query: IndexQuery<Value>) -> EdgeTripleIter {
        let store = self.store.read();
        let mut out: Vec<(u64, u64, u64)> = store
            .docs
            .values()
            .filter(|doc| self.matches_query(doc, &query))
            .filter_map(|doc| doc.edge)
            .collect();
        out.sort_unstable_by_key(|t| t.2);
        EdgeTripleIter::from_vec(out)
    }

    pub fn fulltext_query(&self, query: &str) -> Result<ScoredIdIter, String> {
        let store = self.store.read();
        let mut out: Vec<(u64, f64)> = store
            .docs
            .values()
            .filter_map(|doc| {
                let score = self.fulltext_score(doc, query);
                (score > 0.0).then_some((doc.id, score))
            })
            .collect();
        out.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(CmpOrdering::Equal)
            .then_with(|| a.0.cmp(&b.0)));
        Ok(IndexResultsIter::from_vec(out))
    }

    pub fn fulltext_query_edges(&self, query: &str) -> Result<ScoredEdgeTripleIter, String> {
        let store = self.store.read();
        let mut out: Vec<(u64, u64, u64, f64)> = store
            .docs
            .values()
            .filter_map(|doc| {
                let edge = doc.edge?;
                let score = self.fulltext_score(doc, query);
                (score > 0.0).then_some((edge.0, edge.1, edge.2, score))
            })
            .collect();
        out.sort_by(|a, b| b.3.partial_cmp(&a.3).unwrap_or(CmpOrdering::Equal)
            .then_with(|| a.2.cmp(&b.2)));
        Ok(ScoredEdgeTripleIter::from_vec(out))
    }

    pub fn vector_query(
        &self,
        field: &str,
        vector: Arc<thin_vec::ThinVec<f32>>,
        k: usize,
    ) -> Result<VectorScoredIdIter, String> {
        let Some(meta) = self.vector_field(field) else {
            return Ok(VectorScoredIdIter::empty(vector));
        };
        if let Some(opts) = meta.vector_options()
            && opts.dimension > 0
            && opts.dimension as usize != vector.len()
        {
            return Err(format!(
                "Vector dimension mismatch, expected {} but got {}",
                opts.dimension,
                vector.len()
            ));
        }
        let metric = meta
            .vector_options()
            .and_then(|o| o.similarity_function.as_deref());
        let field_name = meta.name.to_string_lossy().into_owned();
        let store = self.store.read();
        let mut out: Vec<(u64, f64)> = store
            .docs
            .values()
            .filter_map(|doc| {
                let Value::VecF32(candidate) = doc.values.get(&field_name)? else {
                    return None;
                };
                let d = vec_distance::distance(metric, vector.as_slice(), candidate.as_slice())?;
                Some((doc.id, d))
            })
            .collect();
        out.sort_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(CmpOrdering::Equal)
            .then_with(|| a.0.cmp(&b.0)));
        out.truncate(k);
        Ok(VectorScoredIdIter::from_vec(out, vector))
    }

    pub fn vector_query_edges(
        &self,
        field: &str,
        vector: Arc<thin_vec::ThinVec<f32>>,
        k: usize,
    ) -> Result<VectorScoredEdgeTripleIter, String> {
        let Some(meta) = self.vector_field(field) else {
            return Ok(VectorScoredEdgeTripleIter::empty(vector));
        };
        if let Some(opts) = meta.vector_options()
            && opts.dimension > 0
            && opts.dimension as usize != vector.len()
        {
            return Err(format!(
                "Vector dimension mismatch, expected {} but got {}",
                opts.dimension,
                vector.len()
            ));
        }
        let metric = meta
            .vector_options()
            .and_then(|o| o.similarity_function.as_deref());
        let field_name = meta.name.to_string_lossy().into_owned();
        let store = self.store.read();
        let mut out: Vec<(u64, u64, u64, f64)> = store
            .docs
            .values()
            .filter_map(|doc| {
                let edge = doc.edge?;
                let Value::VecF32(candidate) = doc.values.get(&field_name)? else {
                    return None;
                };
                let d = vec_distance::distance(metric, vector.as_slice(), candidate.as_slice())?;
                Some((edge.0, edge.1, edge.2, d))
            })
            .collect();
        out.sort_by(|a, b| a.3.partial_cmp(&b.3).unwrap_or(CmpOrdering::Equal)
            .then_with(|| a.2.cmp(&b.2)));
        out.truncate(k);
        Ok(VectorScoredEdgeTripleIter::from_vec(out, vector))
    }

    fn vector_field(&self, attr: &str) -> Option<&Field> {
        self.fields
            .get(&Arc::new(attr.to_string()))?
            .iter()
            .find(|f| f.ty == IndexType::Vector)
            .map(Arc::as_ref)
    }

    fn range_value<'a>(&self, doc: &'a NativeDocument, attr: &Arc<String>) -> Option<&'a Value> {
        let field = self.fields.get(attr)?
            .iter()
            .find(|f| f.ty == IndexType::Range)?;
        doc.values.get(field.name.to_str().ok()?)
    }

    fn matches_query(&self, doc: &NativeDocument, query: &IndexQuery<Value>) -> bool {
        match query {
            IndexQuery::Equal { key, value } => self
                .range_value(doc, key)
                .is_some_and(|actual| values_equal(actual, value)),
            IndexQuery::Range {
                key,
                min,
                max,
                include_min,
                include_max,
            } => self.range_value(doc, key).is_some_and(|actual| {
                value_in_range(actual, min.as_ref(), max.as_ref(), *include_min, *include_max)
            }),
            IndexQuery::And(children) => children.iter().all(|q| self.matches_query(doc, q)),
            IndexQuery::Or(children) => children.iter().any(|q| self.matches_query(doc, q)),
            IndexQuery::Point { key, point, radius } => {
                let (Some(Value::Point(actual)), Value::Point(center), Some(r)) = (
                    self.range_value(doc, key),
                    point,
                    numeric_value(radius),
                ) else {
                    return false;
                };
                r >= 0.0 && r.is_finite() && center.distance(actual) <= r
            }
            IndexQuery::InList { key, list: Value::List(items) } => self
                .range_value(doc, key)
                .is_some_and(|actual| items.iter().any(|candidate| values_equal(actual, candidate))),
            IndexQuery::ArrayContains { key, value } => {
                let Some(Value::List(items)) = self.range_value(doc, key) else {
                    return false;
                };
                items.iter().any(|candidate| values_equal(candidate, value))
            }
            _ => false,
        }
    }

    fn fulltext_score(&self, doc: &NativeDocument, query: &str) -> f64 {
        let groups = query_groups(query, self.stopwords.as_ref());
        if groups.is_empty() {
            return 0.0;
        }
        let mut best: f64 = 0.0;
        for group in groups {
            let mut score = 0.0;
            let mut all_matched = true;
            for term in &group {
                let mut term_score: f64 = 0.0;
                for fields in self.fields.values() {
                    for field in fields.iter().filter(|f| f.ty == IndexType::Fulltext) {
                        let Some(Value::String(text)) =
                            doc.values.get(field.name.to_str().unwrap_or(""))
                        else {
                            continue;
                        };
                        let opts = field.options();
                        let weight = opts.and_then(|o| o.weight).unwrap_or(1.0);
                        let nostem = opts.and_then(|o| o.nostem).unwrap_or(false);
                        let phonetic = opts
                            .and_then(|o| o.phonetic.as_deref())
                            .is_some_and(|p| !p.is_empty());
                        let tokens = tokenize(text);
                        for token in tokens {
                            let matched = if phonetic {
                                soundex(&token) == soundex(term)
                            } else if nostem {
                                token == *term
                            } else {
                                stem(&token) == stem(term)
                            };
                            if matched {
                                term_score += weight;
                            }
                        }
                    }
                }
                if term_score == 0.0 {
                    all_matched = false;
                    break;
                }
                score += term_score;
            }
            if all_matched {
                best = best.max(score);
            }
        }
        best
    }

    #[must_use]
    pub fn has_fulltext_field(&self) -> bool {
        self.fields.values().any(|fields| fields.iter().any(|f| f.ty == IndexType::Fulltext))
    }
    #[must_use]
    pub fn contains_field(&self, attr: &Arc<String>) -> bool { self.fields.contains_key(attr) }
    #[must_use]
    pub fn has_field_with_type(&self, attr: &Arc<String>, ty: &IndexType) -> bool {
        self.fields.get(attr).is_some_and(|v| v.iter().any(|f| f.ty == *ty))
    }
    #[must_use]
    pub fn get_fields(&self, attr: &Arc<String>) -> Option<&Vec<Arc<Field>>> { self.fields.get(attr) }
    pub fn add_field_to_existing(&mut self, attr: &Arc<String>, field: Arc<Field>) {
        if let Some(fields) = self.fields.get_mut(attr) { fields.push(field); }
    }
    pub fn insert_field(&mut self, attr: Arc<String>, field: Arc<Field>) {
        if !self.fields.contains_key(&attr) { self.field_order.push(attr.clone()); }
        self.fields.insert(attr, vec![field]);
    }
    pub fn remove_field(&mut self, attr: &Arc<String>) -> bool {
        let removed = self.fields.remove(attr).is_some();
        if removed { self.field_order.retain(|a| a != attr); }
        removed
    }
    pub fn retain_fields(&mut self, attr: &Arc<String>, ty: &IndexType) {
        if let Some(fields) = self.fields.get_mut(attr) {
            fields.retain(|f| f.ty != *ty);
            if fields.is_empty() {
                self.fields.remove(attr);
                self.field_order.retain(|a| a != attr);
            }
        }
    }
    #[must_use]
    pub fn is_empty(&self) -> bool { self.fields.is_empty() }
    #[must_use]
    pub fn field_keys(&self) -> Vec<Arc<String>> { self.fields.keys().cloned().collect() }
    #[must_use]
    pub const fn fields(&self) -> &HashMap<Arc<String>, Vec<Arc<Field>>> { &self.fields }
    #[must_use]
    pub fn field_order(&self) -> &[Arc<String>] { &self.field_order }
    pub fn all_fields(&self) -> impl Iterator<Item=&Arc<Field>> { self.fields.values().flat_map(|f| f.iter()) }

    #[must_use]
    pub fn is_operational(&self) -> bool { self.pending_count() == 0 }
    pub fn set_progress(&self, progress: u64, total: u64) {
        self.progress.store(progress, Ordering::Relaxed);
        self.total.store(total, Ordering::Relaxed);
    }
    #[must_use]
    pub fn progress(&self) -> (u64, u64) {
        (self.progress.load(Ordering::Relaxed), self.total.load(Ordering::Relaxed))
    }
    pub fn increment_pending_for_generation(&self, generation_id: u64) -> i32 {
        let mut slots = self.pending_slots.lock();
        if generation_id == slots.current_generation {
            let prev=slots.current_pending; slots.current_pending += 1; prev
        } else {
            let prev=slots.stale_pending; slots.stale_pending += 1; prev
        }
    }
    pub fn try_decrement_pending_for_generation(&self, generation_id: u64) -> i32 {
        let mut slots = self.pending_slots.lock();
        let value = if generation_id == slots.current_generation {
            &mut slots.current_pending
        } else {
            &mut slots.stale_pending
        };
        let prev=*value;
        if prev>0 { *value-=1; }
        prev
    }
    #[must_use]
    pub fn pending_count_for_generation(&self, generation_id: u64) -> i32 {
        let slots=self.pending_slots.lock();
        if generation_id==slots.current_generation { slots.current_pending } else { slots.stale_pending }
    }
    #[must_use]
    pub fn pending_count(&self) -> i32 { self.pending_slots.lock().current_pending }
    #[must_use]
    pub const fn language(&self) -> Option<&Arc<String>> { self.language.as_ref() }
    pub fn set_language(&mut self, language: Option<Arc<String>>) { self.language=language; }
    #[must_use]
    pub const fn stopwords(&self) -> Option<&Vec<Arc<String>>> { self.stopwords.as_ref() }
    pub fn set_stopwords(&mut self, stopwords: Option<Vec<Arc<String>>>) { self.stopwords=stopwords; }

    #[must_use]
    pub fn memory_usage(&self) -> usize {
        let store=self.store.read();
        store.docs.values().map(|doc| {
            std::mem::size_of::<NativeDocument>()
                + doc.values.iter().map(|(k,v)| k.len()+v.heap_size()).sum::<usize>()
        }).sum()
    }
    #[must_use]
    pub fn index_count(&self) -> usize { self.fields.values().map(Vec::len).sum() }

    pub fn recreate_index(&mut self, _label: &Arc<String>) -> Result<(), String> {
        *self.store.write() = NativeStore::default();
        self.ready=true;
        self.bump_id();
        Ok(())
    }

    #[must_use]
    pub const fn int_loses_f64_precision(i: i64) -> bool {
        i.unsigned_abs() & 0x7FF0_0000_0000_0000 != 0
    }
}

fn value_matches_field(ty: IndexType, value: &Value) -> bool {
    match ty {
        IndexType::Range => matches!(
            value,
            Value::Bool(_) | Value::Int(_) | Value::Float(_) | Value::String(_)
                | Value::List(_) | Value::Point(_) | Value::Datetime(_)
                | Value::Date(_) | Value::Time(_) | Value::Duration(_)
        ),
        IndexType::Fulltext => matches!(value, Value::String(_)),
        IndexType::Vector => matches!(value, Value::VecF32(_)),
    }
}

fn numeric_value(value: &Value) -> Option<f64> {
    match value {
        Value::Bool(v) => Some(f64::from(*v)),
        Value::Int(v) => Some(*v as f64),
        Value::Float(v) => Some(*v),
        Value::Datetime(v) | Value::Date(v) | Value::Time(v) | Value::Duration(v) => Some(*v as f64),
        _ => None,
    }
}

fn values_equal(a: &Value, b: &Value) -> bool {
    if let (Some(a), Some(b)) = (numeric_value(a), numeric_value(b)) {
        return a == b;
    }
    match (a,b) {
        (Value::String(a), Value::String(b)) => a == b,
        (Value::Point(a), Value::Point(b)) => a == b,
        (Value::Bool(a), Value::Bool(b)) => a == b,
        _ => false,
    }
}

fn value_in_range(
    actual: &Value,
    min: Option<&Value>,
    max: Option<&Value>,
    include_min: bool,
    include_max: bool,
) -> bool {
    if matches!(actual, Value::String(_)) {
        let Value::String(actual) = actual else { return false; };
        let min_ok = match min {
            None => true,
            Some(Value::String(v)) => if include_min { actual.as_str() >= v.as_str() } else { actual.as_str() > v.as_str() },
            _ => false,
        };
        let max_ok = match max {
            None => true,
            Some(Value::String(v)) => if include_max { actual.as_str() <= v.as_str() } else { actual.as_str() < v.as_str() },
            _ => false,
        };
        return min_ok && max_ok;
    }
    let Some(actual)=numeric_value(actual) else { return false; };
    let min_ok = match min.and_then(numeric_value) {
        None if min.is_none() => true,
        Some(v) => if include_min { actual >= v } else { actual > v },
        None => false,
    };
    let max_ok = match max.and_then(numeric_value) {
        None if max.is_none() => true,
        Some(v) => if include_max { actual <= v } else { actual < v },
        None => false,
    };
    min_ok && max_ok
}

fn tokenize(text: &str) -> Vec<String> {
    text.to_lowercase()
        .split(|c: char| !c.is_alphanumeric())
        .filter(|s| !s.is_empty())
        .map(ToOwned::to_owned)
        .collect()
}

fn query_groups(query: &str, stopwords: Option<&Vec<Arc<String>>>) -> Vec<Vec<String>> {
    let stop: HashSet<String> = stopwords
        .map(|v| v.iter().map(|s| s.to_lowercase()).collect())
        .unwrap_or_default();
    query
        .split('|')
        .map(tokenize)
        .map(|terms| terms.into_iter().filter(|t| !stop.contains(t)).collect::<Vec<_>>())
        .filter(|terms| !terms.is_empty())
        .collect()
}

fn stem(token: &str) -> String {
    for suffix in ["ingly","edly","ing","ed","es","s"] {
        if token.len() > suffix.len()+2 && token.ends_with(suffix) {
            return token[..token.len()-suffix.len()].to_string();
        }
    }
    token.to_string()
}

fn soundex(word: &str) -> String {
    let mut chars=word.chars().filter(|c| c.is_ascii_alphabetic());
    let Some(first)=chars.next() else { return String::new(); };
    fn code(c: char)->char {
        match c.to_ascii_lowercase() {
            'b'|'f'|'p'|'v'=>'1',
            'c'|'g'|'j'|'k'|'q'|'s'|'x'|'z'=>'2',
            'd'|'t'=>'3',
            'l'=>'4',
            'm'|'n'=>'5',
            'r'=>'6',
            _=>'0',
        }
    }
    let mut out=String::with_capacity(4);
    out.push(first.to_ascii_uppercase());
    let mut prev=code(first);
    for c in chars {
        let v=code(c);
        if v!='0' && v!=prev { out.push(v); }
        prev=v;
        if out.len()==4 { break; }
    }
    while out.len()<4 { out.push('0'); }
    out
}
