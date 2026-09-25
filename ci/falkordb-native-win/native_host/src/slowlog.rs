use std::{
    collections::hash_map::DefaultHasher,
    hash::{Hash, Hasher},
    time::{SystemTime, UNIX_EPOCH},
};

use parking_lot::Mutex;

const MAX_ENTRIES: usize = 10;
const MIN_LATENCY_MS: f64 = 10.0;
const STR_MAX_LEN: usize = 2048;

#[derive(Debug, Clone)]
pub struct SlowLogEntry {
    pub timestamp: f64,
    pub command: String,
    pub query: String,
    pub latency_ms: f64,
    pub params: Option<String>,
}

pub struct SlowLog {
    inner: Mutex<SlowLogInner>,
}

struct SlowLogInner {
    entries: Vec<(SlowLogEntry, u64)>,
    min_latency: f64,
}

impl Default for SlowLog {
    fn default() -> Self {
        Self::new()
    }
}

impl SlowLog {
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(SlowLogInner {
                entries: Vec::with_capacity(MAX_ENTRIES),
                min_latency: 0.0,
            }),
        }
    }

    pub fn add(&self, command: &str, full_query: &str, latency_ms: f64) {
        if latency_ms < MIN_LATENCY_MS {
            return;
        }

        let (params, query) = split_params(full_query);
        let mut inner = self.inner.lock();

        if inner.entries.len() >= MAX_ENTRIES && latency_ms <= inner.min_latency {
            return;
        }

        let hash = entry_hash(command, query);
        if let Some((entry, _)) = inner.entries.iter_mut().find(|(_, h)| *h == hash) {
            if latency_ms > entry.latency_ms {
                entry.latency_ms = latency_ms;
                entry.timestamp = unix_now();
                entry.params = params.map(truncate);
            }
            return;
        }

        let entry = SlowLogEntry {
            timestamp: unix_now(),
            command: command.to_owned(),
            query: truncate(query),
            latency_ms,
            params: params.map(truncate),
        };

        if inner.entries.len() < MAX_ENTRIES {
            inner.entries.push((entry, hash));
        } else if let Some((idx, _)) = inner
            .entries
            .iter()
            .enumerate()
            .min_by(|a, b| a.1.0.latency_ms.total_cmp(&b.1.0.latency_ms))
        {
            inner.entries[idx] = (entry, hash);
        }

        inner.min_latency = inner
            .entries
            .iter()
            .map(|(e, _)| e.latency_ms)
            .fold(f64::MAX, f64::min);
    }

    pub fn reset(&self) {
        let mut inner = self.inner.lock();
        inner.entries.clear();
        inner.min_latency = 0.0;
    }

    pub fn entries(&self) -> Vec<SlowLogEntry> {
        self.inner
            .lock()
            .entries
            .iter()
            .map(|(entry, _)| entry.clone())
            .collect()
    }
}

fn split_params(query: &str) -> (Option<&str>, &str) {
    if !query.trim_start().to_ascii_uppercase().starts_with("CYPHER ") {
        return (None, query.trim_start());
    }

    // FalkorDB's parser knows the exact offset. The standalone host does not
    // expose it after planning, so keep the whole CYPHER preamble as params
    // only when a top-level query token can be identified.
    for keyword in [
        " MATCH ", " RETURN ", " CREATE ", " MERGE ", " CALL ", " WITH ",
        " UNWIND ", " DELETE ", " SET ", " REMOVE ", " OPTIONAL ",
    ] {
        if let Some(idx) = query.to_ascii_uppercase().find(keyword) {
            return (Some(query[..idx].trim()), query[idx..].trim_start());
        }
    }
    (None, query.trim_start())
}

fn entry_hash(command: &str, query: &str) -> u64 {
    let mut hasher = DefaultHasher::new();
    command.hash(&mut hasher);
    query[..query.len().min(STR_MAX_LEN)].hash(&mut hasher);
    hasher.finish()
}

fn truncate(value: &str) -> String {
    if value.len() <= STR_MAX_LEN {
        return value.to_owned();
    }
    let mut boundary = STR_MAX_LEN;
    while !value.is_char_boundary(boundary) {
        boundary -= 1;
    }
    format!("{}...", &value[..boundary])
}

fn unix_now() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs_f64()
        .floor()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slowlog_keeps_only_slow_queries_and_resets() {
        let log = SlowLog::new();
        log.add("GRAPH.QUERY", "RETURN 1", 1.0);
        assert!(log.entries().is_empty());

        log.add("GRAPH.QUERY", "RETURN 1", 11.0);
        assert_eq!(log.entries().len(), 1);
        log.reset();
        assert!(log.entries().is_empty());
    }
}
